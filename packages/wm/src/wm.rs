use std::time::Instant;

use anyhow::{bail, Context};
use tokio::sync::mpsc::{self};
use tracing::warn;
use uuid::Uuid;
#[cfg(target_os = "windows")]
use wm_common::TitleBarVisibility;
use wm_common::{
  FloatingStateConfig, FullscreenStateConfig, InvokeCommand, WindowState,
  WmEvent,
};
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;
use wm_platform::{
  Dispatcher, LengthValue, NativeWindow, PlatformEvent, RectDelta,
  WindowEvent,
};

#[cfg(target_os = "windows")]
use crate::{
  animation::{manager::AnimationUpdateOutcome, AnimationManager},
  commands::{
    container::{
      focus_container_by_id, focus_in_direction, set_tiling_direction,
      toggle_tiling_direction,
    },
    general::{
      cycle_focus, disable_binding_mode, enable_binding_mode,
      move_cursor_to_active_window, platform_sync, reload_config,
      shell_exec, toggle_pause,
    },
    monitor::focus_monitor,
    window::{
      cycle_stack_focus, focus_stack_index, ignore_window,
      isolate_destroyed_window, move_to_stack, move_window_in_direction,
      move_window_to_workspace, resize_window, retry_manage_window,
      set_window_position, set_window_size, stack_absorb_neighbor,
      stack_insert, toggle_stack, update_window_state,
      WindowPositionTarget,
    },
    workspace::{
      focus_workspace, move_workspace_in_direction, set_workspace_layout,
      update_workspace_config,
    },
  },
  events::{
    handle_display_settings_changed, handle_mouse_move,
    handle_window_destroyed, handle_window_focused, handle_window_hidden,
    handle_window_minimize_ended, handle_window_minimized,
    handle_window_moved_or_resized, handle_window_reconcile_requested,
    handle_window_shown, handle_window_title_changed,
  },
  ipc_server::IpcServer,
  models::{Container, WindowContainer, WorkspaceTarget},
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub struct WindowManager {
  pub event_rx: mpsc::UnboundedReceiver<WmEvent>,
  pub exit_rx: mpsc::UnboundedReceiver<()>,
  pub animation_tick_rx: mpsc::Receiver<()>,
  pub window_manage_retry_rx: mpsc::UnboundedReceiver<(NativeWindow, u8)>,
  pub state: WmState,
  /// Receiver for tab click events routed from the event-loop thread.
  ///
  /// Always present (not cfg-gated) so that the `tokio::select!` arm in
  /// `main.rs` compiles on all platforms. On non-Windows the sender is
  /// never used, so this receiver never yields an item.
  pub tab_click_rx: mpsc::UnboundedReceiver<(uuid::Uuid, usize)>,
}

#[cfg(target_os = "windows")]
fn native_command_target_was_destroyed(
  error: &wm_platform::Error,
  native_valid_after: bool,
) -> bool {
  error.is_invalid_window_handle() || !native_valid_after
}

#[cfg(target_os = "windows")]
fn finish_native_window_command(
  window: &WindowContainer,
  state: &mut WmState,
  operation: &str,
  result: wm_platform::Result<()>,
) -> anyhow::Result<()> {
  match result {
    Ok(()) => Ok(()),
    Err(error) => {
      let window_id = window.id();
      let (expected_handle, native_valid_after) = {
        let native = window.native();
        (native.id().0, native.is_valid())
      };
      if native_command_target_was_destroyed(&error, native_valid_after) {
        isolate_destroyed_window(
          window_id,
          expected_handle,
          state,
          operation,
        );
      }

      Err(anyhow::Error::new(error)).with_context(|| {
        format!("Failed to {operation} for window {window_id}")
      })
    }
  }
}

#[cfg(target_os = "windows")]
fn apply_transparency_command(
  args: &wm_common::SetTransparencyCommand,
  original_opacity: &wm_platform::OpacityValue,
  mut set_opacity: impl FnMut(
    &wm_platform::OpacityValue,
  ) -> wm_platform::Result<()>,
  mut adjust_opacity: impl FnMut(
    &wm_platform::Delta<wm_platform::OpacityValue>,
  ) -> wm_platform::Result<()>,
) -> Result<(), (&'static str, wm_platform::Error)> {
  if let Some(opacity) = &args.opacity {
    if let Err(error) = set_opacity(opacity) {
      let _ = set_opacity(original_opacity);
      return Err(("set transparency", error));
    }
  }

  if let Some(opacity_delta) = &args.opacity_delta {
    if let Err(error) = adjust_opacity(opacity_delta) {
      let _ = set_opacity(original_opacity);
      return Err(("adjust transparency", error));
    }
  }

  Ok(())
}

impl WindowManager {
  pub fn new(
    config: &mut UserConfig,
    dispatcher: Dispatcher,
  ) -> anyhow::Result<Self> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (exit_tx, exit_rx) = mpsc::unbounded_channel();
    // Animation ticks are level-triggered: one pending signal is enough to
    // redraw all current animation state. Bounding the channel prevents a
    // busy WM loop from accumulating stale frame work.
    let (animation_tick_tx, animation_tick_rx) = mpsc::channel(1);
    let (window_manage_retry_tx, window_manage_retry_rx) =
      mpsc::unbounded_channel();
    let (tab_click_tx, tab_click_rx) = mpsc::unbounded_channel();

    let mut state = WmState::new(
      dispatcher,
      event_tx,
      exit_tx,
      animation_tick_tx,
      window_manage_retry_tx,
      tab_click_tx,
    );
    state.populate(config)?;

    // Start animation timer if `populate` created any animations. This
    // mirrors the `ensure_timer_running` call at the end of
    // `process_event` for the initial population path.
    if !state.animation_manager.ensure_timer_running() {
      AnimationManager::recover_after_animation_failure(
        &mut state,
        config,
        "animation timer startup failed during WM initialization",
      );
    }

    Ok(Self {
      event_rx,
      exit_rx,
      animation_tick_rx,
      window_manage_retry_rx,
      state,
      tab_click_rx,
    })
  }

  pub fn process_window_manage_retry(
    &mut self,
    native_window: NativeWindow,
    attempt: u8,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    retry_manage_window(native_window, attempt, &mut self.state, config)?;

    if !self.state.is_paused && self.state.pending_sync.has_changes() {
      platform_sync(&mut self.state, config)?;
    }

    if !self.state.animation_manager.ensure_timer_running() {
      AnimationManager::recover_after_animation_failure(
        &mut self.state,
        config,
        "animation timer startup failed after window manage retry",
      );
    }

    Ok(())
  }

  pub fn process_event(
    &mut self,
    event: PlatformEvent,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    let state = &mut self.state;

    match event {
      PlatformEvent::DisplaySettingsChanged => {
        handle_display_settings_changed(state, config)
      }
      PlatformEvent::Keybinding(keybinding_event) => {
        // Find the keybinding config that matches this keybinding.
        let commands = config
          .active_keybinding_configs(
            &self.state.binding_modes,
            self.state.is_paused,
          )
          .find(|kb_config| {
            kb_config.bindings.contains(&keybinding_event.0)
          })
          .map(|kb_config| kb_config.commands.clone());

        if let Some(commands) = commands {
          self.process_commands(&commands, None, config)?;
        }

        // Return early since we don't want to redraw twice.
        return Ok(());
      }
      PlatformEvent::Mouse(event) => {
        handle_mouse_move(&event, state, config)
      }
      PlatformEvent::Window(window_event) => match window_event {
        WindowEvent::ReconcileRequested { .. } => {
          handle_window_reconcile_requested(state, config)
        }
        WindowEvent::Focused { window, .. } => {
          handle_window_focused(&window, state, config)
        }
        WindowEvent::Shown { window, .. } => {
          handle_window_shown(window, state, config)
        }
        WindowEvent::Hidden { window, .. } => {
          handle_window_hidden(&window, state, config)
        }
        WindowEvent::MovedOrResized {
          window,
          is_interactive_start,
          is_interactive_end,
          ..
        } => handle_window_moved_or_resized(
          &window,
          is_interactive_start,
          is_interactive_end,
          state,
          config,
        ),
        WindowEvent::Minimized { window, .. } => {
          handle_window_minimized(&window, state, config)
        }
        WindowEvent::MinimizeEnded { window, .. } => {
          handle_window_minimize_ended(&window, state, config)
        }
        WindowEvent::TitleChanged { window, .. } => {
          handle_window_title_changed(&window, state, config)
        }
        WindowEvent::Destroyed { window_id, .. } => {
          handle_window_destroyed(window_id, state)
        }
      },
    }?;

    if !state.is_paused && state.pending_sync.has_changes() {
      platform_sync(state, config)?;
    }

    if !self.state.animation_manager.ensure_timer_running() {
      AnimationManager::recover_after_animation_failure(
        &mut self.state,
        config,
        "animation timer startup failed after platform event",
      );
    }

    Ok(())
  }

  /// Updates all active animations and redraws windows that are animating.
  pub fn update_animations(
    &mut self,
    config: &UserConfig,
  ) -> anyhow::Result<()> {
    // Access animation_manager through state to avoid double borrow.
    match AnimationManager::update_internal(&mut self.state, config) {
      AnimationUpdateOutcome::Complete => Ok(()),
      AnimationUpdateOutcome::RetryScheduled {
        attempt,
        remaining,
        error,
      } => {
        tracing::warn!(
          "Animation final synchronization failed on bounded attempt \
           {attempt}; retained safe coverage and scheduled another tick \
           ({remaining} attempt(s) remain): {error}."
        );
        Ok(())
      }
      AnimationUpdateOutcome::ParticipantFailed { window_ids } => {
        tracing::warn!(
          "Isolated {} failed animation participant(s) without cancelling \
           unrelated animation groups: {window_ids:?}.",
          window_ids.len()
        );
        Ok(())
      }
      AnimationUpdateOutcome::GroupRecovered {
        participant_count,
        error,
      } => {
        tracing::warn!(
          "Workspace animation cleanup exhausted its bounded retry; \
           restored {participant_count} real participant(s) before dropping \
           that group's coverage and kept unrelated animations active: \
           {error}."
        );
        Ok(())
      }
      AnimationUpdateOutcome::Fatal(err) => {
        AnimationManager::recover_after_animation_failure(
          &mut self.state,
          config,
          &format!("fatal animation update failure: {err}"),
        );
        Err(err)
      }
    }
  }

  /// Returns the independent deadline for detecting a native compositor
  /// timer generation that has stopped making progress.
  pub fn animation_timer_watchdog_deadline(&self) -> Option<Instant> {
    self.state.animation_manager.timer_watchdog_deadline()
  }

  /// Abandons one stale native timer generation, restores every animation-
  /// owned real window visibly, and leaves future animations on the
  /// bounded fixed-rate fallback driver.
  pub fn process_animation_timer_watchdog(&mut self, config: &UserConfig) {
    if self
      .state
      .animation_manager
      .abandon_stalled_timer_generation(Instant::now())
    {
      AnimationManager::recover_after_animation_failure(
        &mut self.state,
        config,
        "native compositor timer watchdog expired",
      );
    }
  }

  pub fn process_commands(
    &mut self,
    commands: &Vec<InvokeCommand>,
    subject_container_id: Option<Uuid>,
    config: &mut UserConfig,
  ) -> anyhow::Result<Uuid> {
    let state = &mut self.state;

    // Get the container to run WM commands with.
    let subject_container = match subject_container_id {
      Some(id) => state.container_by_id(id).with_context(|| {
        format!("No container found with the given ID '{id}'.")
      })?,
      None => state
        .focused_container()
        .context("No subject container for command.")?,
    };

    let new_subject_container_id = WindowManager::run_commands(
      commands,
      subject_container,
      state,
      config,
    )?;

    if state.pending_sync.has_changes() {
      platform_sync(state, config)?;
    }

    // Start animation timer if animations were created by a command (e.g.
    // startup commands or IPC commands). Without this, surrogate
    // animations started outside of the platform event loop would
    // never tick, leaving windows permanently cloaked.
    if !self.state.animation_manager.ensure_timer_running() {
      AnimationManager::recover_after_animation_failure(
        &mut self.state,
        config,
        "animation timer startup failed after command",
      );
    }

    Ok(new_subject_container_id)
  }

  pub fn run_commands(
    commands: &Vec<InvokeCommand>,
    subject_container: Container,
    state: &mut WmState,
    config: &mut UserConfig,
  ) -> anyhow::Result<Uuid> {
    let mut current_subject_container = subject_container;

    for command in commands {
      WindowManager::run_command(
        command,
        current_subject_container.clone(),
        state,
        config,
      )?;

      // Update the subject container in case the container type changes.
      // For example, when going from a tiling to a floating window.
      current_subject_container =
        if current_subject_container.is_detached() {
          match state.container_by_id(current_subject_container.id()) {
            Some(container) => container,
            None => break,
          }
        } else {
          current_subject_container
        }
    }

    Ok(current_subject_container.id())
  }

  #[allow(clippy::too_many_lines)]
  pub fn run_command(
    command: &InvokeCommand,
    subject_container: Container,
    state: &mut WmState,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    // No-op if WM is currently paused.
    if state.is_paused && *command != InvokeCommand::WmTogglePause {
      return Ok(());
    }

    if subject_container.is_detached() {
      bail!("Cannot run command because subject container is detached.");
    }

    match &command {
      InvokeCommand::AdjustBorders(args) => {
        match subject_container.as_window_container() {
          Ok(window) => {
            let args = args.clone();
            let border_delta = RectDelta::new(
              args.left.unwrap_or(LengthValue::from_px(0)),
              args.top.unwrap_or(LengthValue::from_px(0)),
              args.right.unwrap_or(LengthValue::from_px(0)),
              args.bottom.unwrap_or(LengthValue::from_px(0)),
            );

            window.set_border_delta(border_delta);
            state.pending_sync.queue_container_to_redraw(window);

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::Close => {
        match subject_container.as_window_container() {
          Ok(window) => {
            #[cfg(target_os = "windows")]
            if config.value.animations.window_close.should_animate() {
              let effect_cfg = if window.id()
                == state
                  .focused_container()
                  .map(|c| c.id())
                  .unwrap_or_default()
              {
                &config.value.window_effects.focused_window
              } else {
                &config.value.window_effects.other_windows
              };
              let effect_opacity = if effect_cfg.transparency.enabled {
                effect_cfg.transparency.opacity.to_alpha()
              } else {
                u8::MAX
              };
              let corner_style = if effect_cfg.corner_style.enabled {
                effect_cfg.corner_style.style.clone()
              } else {
                wm_platform::CornerStyle::Default
              };
              // Snapshotted onto the `ResizeSession` (rather than re-read
              // live from config later) since the close animation's
              // direct-drive loop runs after the window is detached from
              // the container tree, where `effect_cfg` can no longer be
              // recomputed.
              let corner_radius = if effect_cfg.corner_style.enabled {
                effect_cfg.corner_style.style.approx_radius_px()
              } else {
                wm_platform::CornerStyle::Default.approx_radius_px()
              };
              let blur_overlay =
                effect_cfg.backdrop.acrylic_tint().map(|tint| {
                  effect_cfg
                    .backdrop
                    .to_overlay_params(tint, corner_radius)
                });

              let recovery_workspace_id =
                window.workspace().map(|workspace| workspace.id());
              if let (Ok(rect), Some(recovery_workspace_id)) = (
                window.to_rect().and_then(|r| {
                  window
                    .total_border_delta()
                    .map(|d| r.apply_delta(&d, None))
                }),
                recovery_workspace_id,
              ) {
                let window_id = window.id();

                // Register the transparent surrogate over the
                // still-visible window first. The animation manager
                // stages the visibility transfer on later ticks.
                {
                  let native_ref = window.native();
                  state.animation_manager.start_close_animation(
                    window_id,
                    window.clone(),
                    recovery_workspace_id,
                    rect,
                    effect_opacity,
                    corner_style,
                    blur_overlay,
                    config,
                    &*native_ref,
                  );
                }

                // The animation manager keeps the real window visible
                // through the surrogate's readiness ticks, then cloaks
                // and detaches it without blocking this command path.
                if state.animation_manager.has_close_animation(&window_id)
                {
                  return Ok(());
                }
              }
            }

            // Fallback: animations disabled, rect unavailable, or
            // surrogate creation failed — close immediately.
            if let Err(err) = window.native().close() {
              warn!("Failed to close window: {:?}", err);
            }

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::Focus(args) => {
        if let Some(direction) = &args.direction {
          focus_in_direction(&subject_container, direction, state)?;
        }

        if let Some(direction) = &args.workspace_in_direction {
          focus_workspace(
            WorkspaceTarget::Direction(direction.clone()),
            state,
            config,
          )?;
        }

        if let Some(container_id) = &args.container_id {
          focus_container_by_id(container_id, state)?;
        }

        if let Some(name) = &args.workspace {
          focus_workspace(
            WorkspaceTarget::Name(name.clone()),
            state,
            config,
          )?;
        }

        if let Some(monitor_index) = &args.monitor {
          focus_monitor(*monitor_index, state, config)?;
        }

        if args.next_active_workspace {
          focus_workspace(WorkspaceTarget::NextActive, state, config)?;
        }

        if args.prev_active_workspace {
          focus_workspace(WorkspaceTarget::PreviousActive, state, config)?;
        }

        if args.next_workspace {
          focus_workspace(WorkspaceTarget::Next, state, config)?;
        }

        if args.prev_workspace {
          focus_workspace(WorkspaceTarget::Previous, state, config)?;
        }

        if args.recent_workspace {
          focus_workspace(WorkspaceTarget::Recent, state, config)?;
        }

        if args.next_active_workspace_on_monitor {
          focus_workspace(
            WorkspaceTarget::NextActiveInMonitor,
            state,
            config,
          )?;
        }

        if args.prev_active_workspace_on_monitor {
          focus_workspace(
            WorkspaceTarget::PreviousActiveInMonitor,
            state,
            config,
          )?;
        }

        Ok(())
      }
      // No-op at runtime. `force-manage` is a marker that is evaluated
      // before a window is managed (see `check_is_manageable`).
      InvokeCommand::ForceManage => Ok(()),
      InvokeCommand::Ignore => {
        match subject_container.as_window_container() {
          Ok(window) => ignore_window(window, state),
          _ => Ok(()),
        }
      }
      InvokeCommand::MoveCursor(args) => {
        if args.direction.is_some() {
          move_cursor_to_active_window(state)?;
        }
        Ok(())
      }
      InvokeCommand::Move(args) => {
        match subject_container.as_window_container() {
          Ok(window) => {
            if let Some(direction) = &args.direction {
              move_window_in_direction(
                window.clone(),
                direction,
                state,
                config,
              )?;
            }

            if let Some(direction) = &args.workspace_in_direction {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Direction(direction.clone()),
                state,
                config,
              )?;
            }

            if let Some(name) = &args.workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Name(name.clone()),
                state,
                config,
              )?;
            }

            if args.next_active_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::NextActive,
                state,
                config,
              )?;
            }

            if args.prev_active_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::PreviousActive,
                state,
                config,
              )?;
            }

            if args.next_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Next,
                state,
                config,
              )?;
            }

            if args.prev_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Previous,
                state,
                config,
              )?;
            }

            if args.recent_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Recent,
                state,
                config,
              )?;
            }

            if args.next_active_workspace_on_monitor {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::NextActiveInMonitor,
                state,
                config,
              )?;
            }

            if args.prev_active_workspace_on_monitor {
              move_window_to_workspace(
                window,
                WorkspaceTarget::PreviousActiveInMonitor,
                state,
                config,
              )?;
            }
            Ok(())
          }

          _ => Ok(()),
        }
      }
      InvokeCommand::MoveWorkspace { direction } => {
        let workspace =
          subject_container.workspace().context("No workspace.")?;

        move_workspace_in_direction(&workspace, direction, state, config)
      }
      InvokeCommand::Position(args) => {
        match subject_container.as_window_container() {
          Ok(window) => {
            if args.centered {
              set_window_position(
                window,
                &WindowPositionTarget::Centered,
                state,
              )
            } else {
              set_window_position(
                window,
                &WindowPositionTarget::Coordinates(args.x_pos, args.y_pos),
                state,
              )
            }
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::UpdateWorkspaceConfig {
        workspace,
        new_config,
      } => {
        let workspace = if let Some(workspace_name) = workspace {
          state
            .workspace_by_name(workspace_name)
            .context("Workspace doesn't exist.")?
        } else {
          subject_container.workspace().context("No workspace.")?
        };
        update_workspace_config(&workspace, state, config, new_config)
      }
      InvokeCommand::Resize(args) => {
        match subject_container.as_window_container() {
          Ok(window) => resize_window(
            &window,
            args.width.clone(),
            args.height.clone(),
            state,
          ),
          _ => Ok(()),
        }
      }
      InvokeCommand::SetFloating {
        centered,
        shown_on_top,
        x_pos,
        y_pos,
        width,
        height,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          let floating_defaults =
            &config.value.window_behavior.state_defaults.floating;
          let centered = centered.unwrap_or(floating_defaults.centered);

          let window = update_window_state(
            window.clone(),
            WindowState::Floating(FloatingStateConfig {
              centered,
              shown_on_top: shown_on_top
                .unwrap_or(floating_defaults.shown_on_top),
            }),
            state,
            config,
          )?;

          // Allow size and position to be set if window has not previously
          // been manually placed.
          if !window.has_custom_floating_placement() {
            if width.is_some() || height.is_some() {
              set_window_size(
                window.clone(),
                width.clone(),
                height.clone(),
                state,
              )?;
            }

            if centered {
              set_window_position(
                window,
                &WindowPositionTarget::Centered,
                state,
              )?;
            } else if x_pos.is_some() || y_pos.is_some() {
              set_window_position(
                window,
                &WindowPositionTarget::Coordinates(*x_pos, *y_pos),
                state,
              )?;
            }
          }

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::SetFullscreen {
        maximized,
        shown_on_top,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          let fullscreen_defaults =
            &config.value.window_behavior.state_defaults.fullscreen;

          update_window_state(
            window.clone(),
            WindowState::Fullscreen(FullscreenStateConfig {
              maximized: maximized
                .unwrap_or(fullscreen_defaults.maximized),
              shown_on_top: shown_on_top
                .unwrap_or(fullscreen_defaults.shown_on_top),
            }),
            state,
            config,
          )?;

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::SetMinimized => {
        match subject_container.as_window_container() {
          Ok(window) => {
            update_window_state(
              window.clone(),
              WindowState::Minimized,
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::SetTiling => {
        match subject_container.as_window_container() {
          Ok(window) => {
            update_window_state(
              window,
              WindowState::Tiling,
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::SetTitleBarVisibility {
        // LINT: `visibility` is only used on Windows.
        #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
        visibility,
      } => match subject_container.as_window_container() {
        #[cfg(target_os = "windows")]
        Ok(window) => {
          let native = window.native();
          let result = native.set_title_bar_visibility(
            *visibility == TitleBarVisibility::Shown,
          );
          drop(native);
          finish_native_window_command(
            &window,
            state,
            "set title bar visibility",
            result,
          )
        }
        _ => Ok(()),
      },
      // LINT: `args` is only used on Windows.
      #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
      InvokeCommand::SetTransparency(args) => {
        match subject_container.as_window_container() {
          #[cfg(target_os = "windows")]
          Ok(window) => {
            let native = window.native();
            let original_opacity = match native.transparency() {
              Ok(opacity) => opacity,
              Err(error) => {
                drop(native);
                return finish_native_window_command(
                  &window,
                  state,
                  "read transparency before update",
                  Err(error),
                );
              }
            };

            let result = apply_transparency_command(
              args,
              &original_opacity,
              |opacity| native.set_transparency(opacity),
              |delta| native.adjust_transparency(delta),
            );
            drop(native);
            match result {
              Ok(()) => Ok(()),
              Err((operation, error)) => finish_native_window_command(
                &window,
                state,
                operation,
                Err(error),
              ),
            }
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::ShellExec {
        hide_window,
        command,
      } => shell_exec(command, *hide_window, state),
      InvokeCommand::Size(args) => {
        match subject_container.as_window_container() {
          Ok(window) => set_window_size(
            window,
            args.width.clone(),
            args.height.clone(),
            state,
          ),
          _ => Ok(()),
        }
      }
      InvokeCommand::ToggleFloating {
        centered,
        shown_on_top,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          let floating_defaults =
            &config.value.window_behavior.state_defaults.floating;

          let centered = centered.unwrap_or(floating_defaults.centered);
          let target_state = WindowState::Floating(FloatingStateConfig {
            centered,
            shown_on_top: shown_on_top
              .unwrap_or(floating_defaults.shown_on_top),
          });

          let window = update_window_state(
            window.clone(),
            window.toggled_state(target_state, config),
            state,
            config,
          )?;

          if !window.has_custom_floating_placement() && centered {
            set_window_position(
              window,
              &WindowPositionTarget::Centered,
              state,
            )?;
          }

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::ToggleFullscreen {
        maximized,
        shown_on_top,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          let fullscreen_defaults =
            &config.value.window_behavior.state_defaults.fullscreen;

          let target_state =
            WindowState::Fullscreen(FullscreenStateConfig {
              maximized: maximized
                .unwrap_or(fullscreen_defaults.maximized),
              shown_on_top: shown_on_top
                .unwrap_or(fullscreen_defaults.shown_on_top),
            });

          update_window_state(
            window.clone(),
            window.toggled_state(target_state, config),
            state,
            config,
          )?;

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::ToggleMinimized => {
        match subject_container.as_window_container() {
          Ok(window) => {
            update_window_state(
              window.clone(),
              window.toggled_state(WindowState::Minimized, config),
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::ToggleTiling => {
        match subject_container.as_window_container() {
          Ok(window) => {
            update_window_state(
              window.clone(),
              window.toggled_state(WindowState::Tiling, config),
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::ToggleStack => {
        if let Ok(window) = subject_container.as_window_container() {
          let tiling = match window {
            WindowContainer::TilingWindow(ref w) => w.clone(),
            WindowContainer::NonTilingWindow(_) => {
              // Convert floating → tiling. If the window's saved
              // insertion target puts it back into an existing stack,
              // it is already in a good state — don't toggle it back out.
              let converted = update_window_state(
                window,
                WindowState::Tiling,
                state,
                config,
              )?;
              let WindowContainer::TilingWindow(w) = converted else {
                return Ok(());
              };
              if w.parent().is_some_and(|p| p.as_stack().is_some()) {
                state.pending_sync.queue_focus_change();
                return Ok(());
              }
              w
            }
          };
          toggle_stack(&tiling, state, config)?;
          state.pending_sync.queue_focus_change();
        }
        Ok(())
      }
      InvokeCommand::CycleStackFocus { prev } => {
        cycle_stack_focus(&subject_container, *prev, state)?;
        state.pending_sync.queue_focus_change();
        Ok(())
      }
      InvokeCommand::FocusStackIndex { index } => {
        focus_stack_index(&subject_container, *index, state)?;
        state.pending_sync.queue_focus_change();
        Ok(())
      }
      InvokeCommand::StackAbsorbNeighbor { direction } => {
        if let Some(window) = subject_container.as_tiling_window() {
          stack_absorb_neighbor(&window, direction, state, config)?;
          state.pending_sync.queue_focus_change();
        }
        Ok(())
      }
      InvokeCommand::StackInsert => {
        if let Ok(window) = subject_container.as_window_container() {
          let tiling = ensure_tiling(window, state, config)?;
          stack_insert(&tiling, state, config)?;
          state.pending_sync.queue_focus_change();
        }
        Ok(())
      }
      InvokeCommand::MoveToStack { name } => {
        if let Ok(window) = subject_container.as_window_container() {
          let tiling = ensure_tiling(window, state, config)?;
          move_to_stack(&tiling, name, state, config)?;
          state.pending_sync.queue_focus_change();
        }
        Ok(())
      }
      InvokeCommand::ToggleTilingDirection => {
        toggle_tiling_direction(subject_container, state, config)
      }
      InvokeCommand::SetTilingDirection { tiling_direction } => {
        set_tiling_direction(
          subject_container,
          state,
          config,
          tiling_direction,
        )
      }
      InvokeCommand::SetWorkspaceLayout { layout } => {
        set_workspace_layout(subject_container, layout.clone(), state)
      }
      InvokeCommand::WmCycleFocus {
        omit_floating,
        omit_fullscreen,
        omit_minimized,
        omit_tiling,
      } => cycle_focus(
        *omit_floating,
        *omit_fullscreen,
        *omit_minimized,
        *omit_tiling,
        state,
        config,
      ),
      InvokeCommand::WmDisableBindingMode { name } => {
        disable_binding_mode(name, state);
        Ok(())
      }
      InvokeCommand::WmEnableBindingMode { name } => {
        enable_binding_mode(name, state, config)
      }
      InvokeCommand::WmExit => state.emit_exit(),
      InvokeCommand::WmRedraw => {
        state
          .pending_sync
          .queue_container_to_redraw(state.root_container.clone());

        Ok(())
      }
      InvokeCommand::WmReloadConfig => reload_config(state, config),
      InvokeCommand::WmTogglePause => {
        toggle_pause(state);
        Ok(())
      }
    }
  }

  /// Runs cleanup tasks when the WM is exiting.
  pub(crate) fn cleanup(
    &mut self,
    config: &mut UserConfig,
    ipc_server: &mut IpcServer,
  ) {
    self.state.emit_event(WmEvent::ApplicationExiting);

    // Ensure that the WM is unpaused, otherwise, shutdown commands won't
    // get executed.
    self.state.is_paused = false;

    // Run user's shutdown commands.
    if let Err(err) = self.process_commands(
      &config.value.general.shutdown_commands.clone(),
      None,
      config,
    ) {
      tracing::warn!("Failed to run shutdown commands: {:?}", err);
    }

    // Emit remaining WM events before exiting.
    while let Ok(wm_event) = self.event_rx.try_recv() {
      tracing::info!(
        "Emitting WM event before shutting down: {:?}",
        wm_event
      );

      if let Err(err) = ipc_server.process_event(wm_event) {
        tracing::warn!("{:?}", err);
      }
    }
  }
}

/// Ensures `window` is a `TilingWindow`, converting it from any non-tiling
/// state (floating, fullscreen, minimized) if needed.
fn ensure_tiling(
  window: WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<crate::models::TilingWindow> {
  match window {
    WindowContainer::TilingWindow(w) => Ok(w),
    WindowContainer::NonTilingWindow(_) => {
      let converted =
        update_window_state(window, WindowState::Tiling, state, config)?;
      match converted {
        WindowContainer::TilingWindow(w) => Ok(w),
        _ => anyhow::bail!("Window could not be converted to tiling."),
      }
    }
  }
}

#[cfg(all(test, target_os = "windows"))]
mod native_command_tests {
  use std::cell::RefCell;

  use tokio::sync::mpsc;
  use wm_common::{
    ParsedConfig, SetTransparencyCommand, TitleBarVisibility,
  };
  use wm_platform::{Delta, Dispatcher, NativeWindow, OpacityValue};

  use super::*;
  use crate::{
    commands::container::attach_container,
    models::{Monitor, TilingWindow, Workspace},
    traits::CommonGetters,
  };

  fn state_with_invalid_window() -> (WmState, WindowContainer) {
    let (event_tx, _) = mpsc::unbounded_channel();
    let (exit_tx, _) = mpsc::unbounded_channel();
    let (animation_tick_tx, _) = mpsc::channel(1);
    let (window_manage_retry_tx, _) = mpsc::unbounded_channel();
    let (tab_click_tx, _) = mpsc::unbounded_channel();
    let state = WmState::new(
      Dispatcher::mock(),
      event_tx,
      exit_tx,
      animation_tick_tx,
      window_manage_retry_tx,
      tab_click_tx,
    );
    let window = TilingWindow::mock()
      .native(NativeWindow::from_handle(-1))
      .call();
    let workspace = Workspace::mock()
      .tiling_containers(vec![window.clone().into()])
      .call();
    let monitor = Monitor::mock().workspaces(vec![workspace]).call();
    attach_container(
      &monitor.into(),
      &state.root_container.clone().into(),
      None,
    )
    .expect("test monitor should attach");

    (state, window.into())
  }

  #[test]
  fn destroyed_titlebar_target_returns_error_and_isolates_only_that_window(
  ) {
    let (mut state, window) = state_with_invalid_window();
    let mut config = UserConfig::for_test(ParsedConfig::default());
    let window_id = window.id();

    WindowManager::run_command(
      &InvokeCommand::SetTitleBarVisibility {
        visibility: TitleBarVisibility::Shown,
      },
      window.into(),
      &mut state,
      &mut config,
    )
    .expect_err("destroyed titlebar target must not report success");

    assert!(state.container_by_id(window_id).is_none());
    assert!(state.windows().is_empty());
  }

  #[test]
  fn destroyed_transparency_target_returns_error_and_isolates_only_that_window(
  ) {
    let (mut state, window) = state_with_invalid_window();
    let mut config = UserConfig::for_test(ParsedConfig::default());
    let window_id = window.id();

    WindowManager::run_command(
      &InvokeCommand::SetTransparency(SetTransparencyCommand {
        opacity: Some(OpacityValue::try_new(0.5).unwrap()),
        opacity_delta: None,
      }),
      window.into(),
      &mut state,
      &mut config,
    )
    .expect_err("destroyed opacity target must not report success");

    assert!(state.container_by_id(window_id).is_none());
    assert!(state.windows().is_empty());
  }

  #[test]
  fn only_destroyed_native_command_failures_request_local_isolation() {
    assert!(native_command_target_was_destroyed(
      &wm_platform::Error::WindowNotFound,
      true
    ));
    assert!(native_command_target_was_destroyed(
      &wm_platform::Error::Platform("raced destruction".to_string()),
      false
    ));
    assert!(!native_command_target_was_destroyed(
      &wm_platform::Error::Platform("live access failure".to_string()),
      true
    ));
  }

  #[test]
  fn failed_transparency_sequence_restores_the_original_alpha() {
    let original = OpacityValue::from_alpha(200);
    let calls = RefCell::new(Vec::new());
    let args = SetTransparencyCommand {
      opacity: Some(OpacityValue::try_new(0.5).unwrap()),
      opacity_delta: Some(Delta {
        inner: OpacityValue::try_new(0.1).unwrap(),
        is_negative: true,
      }),
    };

    let result = apply_transparency_command(
      &args,
      &original,
      |opacity| {
        calls.borrow_mut().push(opacity.to_alpha());
        Ok(())
      },
      |_| {
        Err(wm_platform::Error::Platform(
          "injected delta failure".into(),
        ))
      },
    );

    assert!(matches!(result, Err(("adjust transparency", _))));
    assert_eq!(
      calls.into_inner(),
      [args.opacity.unwrap().to_alpha(), original.to_alpha()]
    );
  }
}
