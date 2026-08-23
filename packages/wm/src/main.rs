// The `windows` or `console` subsystem (default is `console`) determines
// whether a console window is spawned on launch, if not already ran
// through a console. The following prevents this additional console window
// in release mode.
#![cfg_attr(
  all(not(debug_assertions), target_os = "windows"),
  windows_subsystem = "windows"
)]
#![warn(clippy::all, clippy::pedantic)]
#![feature(iterator_try_collect)]

#[cfg(target_os = "macos")]
use std::io::IsTerminal;
use std::{env, path::PathBuf, process, time::Duration};

use anyhow::{Context, Error};
use tokio::{
  process::Command,
  signal,
  sync::{broadcast, mpsc},
};
use tokio_tungstenite::tungstenite::Message;
use tracing::Level;
use tracing_subscriber::{
  fmt::{self, writer::MakeWriterExt},
  layer::SubscriberExt,
};
use wm_common::{AppCommand, InvokeCommand, Verbosity, WmEvent};
#[cfg(target_os = "macos")]
use wm_platform::DispatcherExtMacOs;
use wm_platform::{
  Dispatcher, DisplayListener, EventLoop, KeybindingEvent,
  KeybindingListener, MouseEvent, MouseEventKind, MouseListener,
  NativeWindow, PlatformEvent, SingleInstance, WindowEvent,
  WindowListener,
};

use crate::{
  ipc_server::IpcServer, sys_tray::SystemTray, user_config::UserConfig,
  wm::WindowManager,
};

mod animation;
mod commands;
mod events;
mod ipc_server;
mod models;
mod pending_sync;
mod sys_tray;
mod traits;
mod user_config;
mod wm;
mod wm_state;

#[cfg(test)]
mod test_utils;

enum NonAnimationInput {
  CtrlC,
  WmExit,
  TrayExit,
  Mouse(MouseEvent),
  Window(WindowEvent),
  DisplaySettingsChanged,
  Keybinding(KeybindingEvent),
  WindowManageRetry(NativeWindow, u8),
  Cleanup,
  IpcMessage(String, mpsc::Sender<Message>, broadcast::Sender<()>),
  TabClick(uuid::Uuid, usize),
  WmEvent(WmEvent),
  ConfigReload,
}

enum MainLoopInput {
  AnimationTick,
  NonAnimation(NonAnimationInput),
}

/// Tracks the one-turn fairness debt created by rendering an animation
/// frame.
///
/// A pending compositor tick normally has deadline priority. Once that
/// frame is rendered, ready non-animation work gets the next turn before
/// another already-pending frame. If no such work arrives before the next
/// compositor tick, rendering continues without an artificial sleep.
#[derive(Default)]
struct AnimationFrameScheduler {
  non_animation_turn_due: bool,
}

impl AnimationFrameScheduler {
  fn should_prioritize_pending_tick(&self) -> bool {
    !self.non_animation_turn_due
  }

  fn on_animation_frame(&mut self) {
    self.non_animation_turn_due = true;
  }

  fn on_non_animation_work(&mut self) {
    self.non_animation_turn_due = false;
  }
}

#[allow(clippy::too_many_arguments)]
async fn next_non_animation_input(
  exit_rx: &mut mpsc::UnboundedReceiver<()>,
  tray_exit_rx: &mut mpsc::UnboundedReceiver<()>,
  mouse_listener: &mut MouseListener,
  window_listener: &mut WindowListener,
  display_listener: &mut DisplayListener,
  keybinding_listener: &mut KeybindingListener,
  window_manage_retry_rx: &mut mpsc::UnboundedReceiver<(NativeWindow, u8)>,
  cleanup_interval: &mut tokio::time::Interval,
  ipc_message_rx: &mut mpsc::Receiver<(
    String,
    mpsc::Sender<Message>,
    broadcast::Sender<()>,
  )>,
  tab_click_rx: &mut mpsc::UnboundedReceiver<(uuid::Uuid, usize)>,
  event_rx: &mut mpsc::UnboundedReceiver<WmEvent>,
  config_reload_rx: &mut mpsc::UnboundedReceiver<()>,
) -> NonAnimationInput {
  tokio::select! {
    _ = signal::ctrl_c() => NonAnimationInput::CtrlC,
    Some(()) = exit_rx.recv() => NonAnimationInput::WmExit,
    Some(()) = tray_exit_rx.recv() => NonAnimationInput::TrayExit,
    Some(event) = mouse_listener.next_event() => {
      NonAnimationInput::Mouse(event)
    },
    Some(event) = window_listener.next_event() => {
      NonAnimationInput::Window(event)
    },
    Some(()) = display_listener.next_event() => {
      NonAnimationInput::DisplaySettingsChanged
    },
    Some(event) = keybinding_listener.next_event() => {
      NonAnimationInput::Keybinding(event)
    },
    Some((native_window, attempt)) = window_manage_retry_rx.recv() => {
      NonAnimationInput::WindowManageRetry(native_window, attempt)
    },
    _ = cleanup_interval.tick() => NonAnimationInput::Cleanup,
    Some((message, response_tx, disconnection_tx)) = ipc_message_rx.recv() => {
      NonAnimationInput::IpcMessage(
        message,
        response_tx,
        disconnection_tx,
      )
    },
    Some((stack_id, tab_index)) = tab_click_rx.recv() => {
      NonAnimationInput::TabClick(stack_id, tab_index)
    },
    Some(wm_event) = event_rx.recv() => {
      NonAnimationInput::WmEvent(wm_event)
    },
    Some(()) = config_reload_rx.recv() => NonAnimationInput::ConfigReload,
  }
}

#[cfg(test)]
mod scheduler_tests {
  use super::AnimationFrameScheduler;

  #[test]
  fn animation_frame_creates_one_non_animation_turn() {
    let mut scheduler = AnimationFrameScheduler::default();
    assert!(scheduler.should_prioritize_pending_tick());

    scheduler.on_animation_frame();
    assert!(!scheduler.should_prioritize_pending_tick());

    scheduler.on_non_animation_work();
    assert!(scheduler.should_prioritize_pending_tick());
  }

  #[test]
  fn consecutive_idle_frames_keep_non_animation_priority_available() {
    let mut scheduler = AnimationFrameScheduler::default();
    scheduler.on_animation_frame();
    scheduler.on_animation_frame();

    assert!(!scheduler.should_prioritize_pending_tick());
    scheduler.on_non_animation_work();
    assert!(scheduler.should_prioritize_pending_tick());
  }
}

/// Main entry point for the application.
///
/// Conditionally starts the WM or runs a CLI command based on the given
/// subcommand.
fn main() -> anyhow::Result<()> {
  let args = std::env::args().collect::<Vec<_>>();
  let app_command = AppCommand::parse_with_default(&args);

  if let AppCommand::Start {
    config_path,
    verbosity,
  } = app_command
  {
    let rt = tokio::runtime::Runtime::new()?;
    let (event_loop, dispatcher) = EventLoop::new()?;

    let task_handle = std::thread::spawn(move || {
      rt.block_on(async {
        let start_res =
          start_wm(config_path, verbosity, &dispatcher).await;

        if let Err(err) = &start_res {
          // If unable to start the WM, the error is fatal and a message
          // dialog is shown.
          tracing::error!("{:?}", err);
          dispatcher.show_error_dialog("Fatal error", &err.to_string());
        }

        if let Err(err) = dispatcher.stop_event_loop() {
          // Forcefully exit the process to ensure the event loop is
          // stopped.
          tracing::error!("Failed to stop event loop gracefully: {}", err);
          process::exit(1);
        }

        start_res
      })
    });

    // Run event loop (blocks until shutdown). This must be on the main
    // thread for macOS compatibility.
    event_loop.run()?;

    // Wait for clean exit of the WM.
    task_handle.join().unwrap()
  } else {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(wm_cli::start(args))
  }
}

#[allow(clippy::too_many_lines)]
async fn start_wm(
  config_path: Option<PathBuf>,
  verbosity: Verbosity,
  dispatcher: &Dispatcher,
) -> anyhow::Result<()> {
  setup_logging(&verbosity)?;

  // Capture panics into the error log so silent crashes are diagnosable.
  std::panic::set_hook(Box::new(|info| {
    let location = info
      .location()
      .map(|l| format!("{}:{}", l.file(), l.line()))
      .unwrap_or_else(|| "unknown location".to_string());
    let message = info
      .payload()
      .downcast_ref::<&str>()
      .copied()
      .or_else(|| {
        info.payload().downcast_ref::<String>().map(String::as_str)
      })
      .unwrap_or("unknown panic");
    tracing::error!("PANIC at {}: {}", location, message);
  }));

  // Ensure that only one instance of the WM is running.
  let _single_instance = SingleInstance::new()?;

  #[cfg(target_os = "macos")]
  {
    if !dispatcher.has_ax_permission(true) {
      anyhow::bail!(
        "Accessibility permissions are not granted. In System Preferences, \
         go to Privacy & Security > Accessibility and enable GlazeWM."
      );
    }
  }

  // Parse and validate user config.
  let mut config = UserConfig::new(config_path)?;

  // Add application icon to system tray.
  let mut tray = SystemTray::new(&config.path, dispatcher.clone())?;

  let mut wm = WindowManager::new(&mut config, dispatcher.clone())?;

  let mut ipc_server = IpcServer::start().await?;

  // On Windows, start watcher process for restoring hidden windows on
  // crash. macOS' hidden windows are always accessible.
  #[cfg(target_os = "windows")]
  if let Err(err) = start_watcher_process() {
    tracing::warn!(
      "Failed to start watcher process: {err}{}",
      cfg!(debug_assertions)
        .then_some(".\n Run `cargo build -p wm-watcher` to build it.")
        .unwrap_or_default()
    );
  }

  // On macOS, update the current process' PATH variable so that
  // `shell-exec` can resolve programs defined in the shell's PATH. Skip if
  // running via a terminal.
  #[cfg(target_os = "macos")]
  if !std::io::stdin().is_terminal() {
    update_path_env();
  }

  // Start listening for platform events after populating initial state.
  let mut window_listener = WindowListener::new(dispatcher)?;
  let mut display_listener = DisplayListener::new(dispatcher)?;
  let mut mouse_listener = MouseListener::new(
    if config.value.general.focus_follows_cursor {
      &[MouseEventKind::Move, MouseEventKind::LeftButtonUp]
    } else {
      &[MouseEventKind::LeftButtonUp]
    },
    dispatcher,
  )?;
  let mut keybinding_listener = KeybindingListener::new(
    &config
      .active_keybinding_configs(&[], false)
      .flat_map(|kb| kb.bindings)
      .collect::<Vec<_>>(),
    dispatcher,
  )?;

  // Run user's startup commands.
  if let Err(err) = wm.process_commands(
    &config.value.general.startup_commands.clone(),
    None,
    &mut config,
  ) {
    tracing::error!("{:?}", err);
  }

  // Create an interval for periodically cleaning up invalid windows.
  let mut cleanup_interval = tokio::time::interval(Duration::from_secs(5));
  cleanup_interval
    .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

  let mut frame_scheduler = AnimationFrameScheduler::default();

  loop {
    // A queued tick means the compositor deadline has arrived. Service it
    // before entering the general fair selection unless the preceding
    // action was itself an animation frame and therefore owes other
    // ready work a turn.
    if frame_scheduler.should_prioritize_pending_tick()
      && wm.animation_tick_rx.try_recv().is_ok()
    {
      while wm.animation_tick_rx.try_recv().is_ok() {}
      let res = wm.update_animations(&config);
      frame_scheduler.on_animation_frame();
      if let Err(err) = res {
        tracing::error!("{:?}", err);
      }
      continue;
    }

    let input = {
      let next_non_animation = next_non_animation_input(
        &mut wm.exit_rx,
        &mut tray.exit_rx,
        &mut mouse_listener,
        &mut window_listener,
        &mut display_listener,
        &mut keybinding_listener,
        &mut wm.window_manage_retry_rx,
        &mut cleanup_interval,
        &mut ipc_server.message_rx,
        &mut wm.tab_click_rx,
        &mut wm.event_rx,
        &mut tray.config_reload_rx,
      );
      tokio::pin!(next_non_animation);

      if frame_scheduler.non_animation_turn_due {
        // If both are ready, complete one fairly-selected non-animation
        // item before rendering another frame. The animation
        // branch remains as the no-work fallback, so an idle
        // desktop never loses a refresh interval.
        tokio::select! {
          biased;
          event = &mut next_non_animation => {
            MainLoopInput::NonAnimation(event)
          },
          Some(()) = wm.animation_tick_rx.recv() => {
            MainLoopInput::AnimationTick
          },
        }
      } else {
        // General case: preserve fair selection between a future
        // compositor deadline and the aggregate of all
        // fairly-selected non-animation sources.
        tokio::select! {
          event = &mut next_non_animation => {
            MainLoopInput::NonAnimation(event)
          },
          Some(()) = wm.animation_tick_rx.recv() => {
            MainLoopInput::AnimationTick
          },
        }
      }
    };

    let res = match input {
      MainLoopInput::AnimationTick => {
        while wm.animation_tick_rx.try_recv().is_ok() {}
        frame_scheduler.on_animation_frame();
        wm.update_animations(&config)
      }
      MainLoopInput::NonAnimation(NonAnimationInput::CtrlC) => {
        tracing::info!("Received SIGINT signal.");
        break;
      }
      MainLoopInput::NonAnimation(NonAnimationInput::WmExit) => {
        tracing::info!("Exiting through WM command.");
        break;
      }
      MainLoopInput::NonAnimation(NonAnimationInput::TrayExit) => {
        tracing::info!("Exiting through system tray.");
        break;
      }
      MainLoopInput::NonAnimation(NonAnimationInput::Mouse(event)) => {
        frame_scheduler.on_non_animation_work();
        tracing::debug!("Received mouse event: {:?}", event);
        wm.process_event(PlatformEvent::Mouse(event), &mut config)
      }
      MainLoopInput::NonAnimation(NonAnimationInput::Window(event)) => {
        frame_scheduler.on_non_animation_work();
        tracing::debug!("Received window event: {:?}", event);
        wm.process_event(PlatformEvent::Window(event), &mut config)
      }
      MainLoopInput::NonAnimation(
        NonAnimationInput::DisplaySettingsChanged,
      ) => {
        frame_scheduler.on_non_animation_work();
        tracing::debug!("Received display settings changed event.");
        wm.process_event(
          PlatformEvent::DisplaySettingsChanged,
          &mut config,
        )
      }
      MainLoopInput::NonAnimation(NonAnimationInput::Keybinding(
        event,
      )) => {
        frame_scheduler.on_non_animation_work();
        tracing::debug!("Received keyboard event: {:?}", event);
        wm.process_event(PlatformEvent::Keybinding(event), &mut config)
      }
      MainLoopInput::NonAnimation(
        NonAnimationInput::WindowManageRetry(native_window, attempt),
      ) => {
        frame_scheduler.on_non_animation_work();
        wm.process_window_manage_retry(native_window, attempt, &mut config)
      }
      MainLoopInput::NonAnimation(NonAnimationInput::Cleanup) => {
        frame_scheduler.on_non_animation_work();
        if wm.state.is_paused {
          Ok(())
        } else {
          wm.state.cleanup_invalid_windows()
        }
      }
      MainLoopInput::NonAnimation(NonAnimationInput::IpcMessage(
        message,
        response_tx,
        disconnection_tx,
      )) => {
        frame_scheduler.on_non_animation_work();
        tracing::info!("Received IPC message: {:?}", message);

        if let Err(err) = ipc_server.process_message(
          message,
          &response_tx,
          &disconnection_tx,
          &mut wm,
          &mut config,
        ) {
          tracing::error!("{:?}", err);
        }

        Ok(())
      }
      MainLoopInput::NonAnimation(NonAnimationInput::TabClick(
        stack_id,
        tab_index,
      )) => {
        frame_scheduler.on_non_animation_work();
        // Synthesize a `FocusStackIndex` command targeted at the stack.
        wm.process_commands(
          &vec![wm_common::InvokeCommand::FocusStackIndex {
            index: tab_index,
          }],
          Some(stack_id),
          &mut config,
        )
        .map(|_| ())
      }
      MainLoopInput::NonAnimation(NonAnimationInput::WmEvent(
        wm_event,
      )) => {
        frame_scheduler.on_non_animation_work();
        tracing::debug!("Received WM event: {:?}", wm_event);

        // Disable mouse listener when the WM is paused.
        if let WmEvent::PauseChanged { is_paused } = wm_event {
          let _ = mouse_listener.enable(!is_paused);
        }

        // Update keybinding and mouse listeners on config changes.
        if matches!(
          wm_event,
          WmEvent::UserConfigChanged { .. }
            | WmEvent::BindingModesChanged { .. }
            | WmEvent::PauseChanged { .. }
        ) {
          keybinding_listener.update(
            &config
              .active_keybinding_configs(&wm.state.binding_modes, false)
              .flat_map(|kb| kb.bindings)
              .collect::<Vec<_>>(),
          );

          mouse_listener.set_enabled_events(
            if config.value.general.focus_follows_cursor {
              &[MouseEventKind::Move, MouseEventKind::LeftButtonUp]
            } else {
              &[MouseEventKind::LeftButtonUp]
            },
          )?;
        }

        if let Err(err) = ipc_server.process_event(wm_event) {
          tracing::error!("{:?}", err);
        }

        Ok(())
      }
      MainLoopInput::NonAnimation(NonAnimationInput::ConfigReload) => {
        frame_scheduler.on_non_animation_work();
        wm.process_commands(
          &vec![InvokeCommand::WmReloadConfig],
          None,
          &mut config,
        )
        .map(|_| ())
      }
    };

    if let Err(err) = res {
      tracing::error!("{:?}", err);
    }
  }

  tracing::info!("Window manager shutting down.");
  wm.cleanup(&mut config, &mut ipc_server);

  Ok(())
}

/// Initialize logging with the specified verbosity level.
///
/// Error logs are saved to `~/.glzr/glazewm/errors.log`.
fn setup_logging(verbosity: &Verbosity) -> anyhow::Result<()> {
  let error_log_dir = home::home_dir()
    .context("Unable to get home directory.")?
    .join(".glzr/glazewm/");

  let error_writer =
    tracing_appender::rolling::never(error_log_dir, "errors.log");

  let subscriber = tracing_subscriber::registry()
    .with(
      // Output to stdout with specified verbosity level.
      fmt::Layer::new()
        .with_writer(std::io::stdout.with_max_level(verbosity.level())),
    )
    .with(
      // Output to error log file.
      fmt::Layer::new()
        .with_writer(error_writer.with_max_level(Level::ERROR)),
    );

  tracing::subscriber::set_global_default(subscriber)?;

  tracing::info!(
    "Starting WM with log level {:?}.",
    verbosity.level().to_string()
  );

  Ok(())
}

/// Launches watcher binary (Windows-only). This is a separate process that
/// is responsible for restoring hidden windows in case the main WM process
/// crashes.
///
/// This assumes the watcher binary exists in the same directory as the
/// WM binary.
#[allow(unused)]
fn start_watcher_process() -> anyhow::Result<tokio::process::Child, Error>
{
  let watcher_path = env::current_exe()?
    .parent()
    .context("Failed to resolve path to the watcher process.")?
    .join("glazewm-watcher");

  Command::new(&watcher_path)
    .spawn()
    .context("Failed to start watcher process.")
}

/// Updates the current process' PATH by querying the login shell.
///
/// Apps launched outside a terminal (Spotlight, Finder, login items)
/// inherit a PATH that only contains `/usr/bin:/bin:/usr/sbin:/sbin`. This
/// causes `shell-exec` to fail for binaries that aren't in the system
/// PATH.
#[cfg(target_os = "macos")]
fn update_path_env() {
  let shell =
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());

  // Use `-l` and `-i` (login + interactive) so that both profile and rc
  // files are sourced.
  let path_var = match std::process::Command::new(&shell)
    .args(["-lic", "printf '%s' \"$PATH\""])
    .output()
  {
    Ok(output) if output.status.success() => {
      String::from_utf8(output.stdout)
        .ok()
        .filter(|path| !path.is_empty())
    }
    _ => None,
  };

  if let Some(path) = path_var {
    std::env::set_var("PATH", path);
  } else {
    tracing::warn!(
      "Failed to query login shell for PATH. Keeping existing PATH."
    );
  }
}
