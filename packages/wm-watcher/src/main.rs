// The `windows` or `console` subsystem (default is `console`) determines
// whether a console window is spawned on launch, if not already ran
// through a console. The following prevents this additional console window
// in release mode.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![warn(clippy::all, clippy::pedantic)]

use std::{sync::mpsc, thread, time::Duration};

use anyhow::Context;
use wm_common::{ClientResponseData, ContainerDto, WindowDto, WmEvent};
use wm_ipc_client::IpcClient;
use wm_platform::{NativeWindow, NativeWindowWindowsExt, OpacityValue};

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  tracing_subscriber::fmt().init();

  let mut client = IpcClient::connect().await?;

  // Get handles to windows that are already open on watcher launch.
  let mut managed_handles = query_initial_windows(&mut client)
    .await?
    .into_iter()
    .map(|window| window.handle)
    .collect::<Vec<_>>();

  // Update window handles on window manage/unmanage events.
  let subscribe_res =
    watch_managed_handles(&mut client, &mut managed_handles).await;

  if subscribe_res.is_ok() {
    client.close().await;
  }

  match subscribe_res {
    Ok(()) => {
      tracing::info!("WM exited successfully. Skipping watcher cleanup.");
    }
    Err(err) => {
      tracing::info!(
        "Running watcher cleanup. WM exited unexpectedly: {}",
        err
      );

      restore_managed_windows(managed_handles, CLEANUP_TIMEOUT);
    }
  }

  Ok(())
}

fn restore_managed_windows(handles: Vec<isize>, timeout: Duration) {
  let worker_count = handles.len();
  let (completion_tx, completion_rx) = mpsc::channel();
  let mut started_workers = 0usize;

  for handle in handles {
    let completion_tx = completion_tx.clone();
    let spawn_result = thread::Builder::new()
      .name(format!("glazewm-watcher-cleanup-{handle}"))
      .spawn(move || {
        restore_managed_window(handle);
        let _ = completion_tx.send(handle);
      });

    match spawn_result {
      Ok(_) => started_workers += 1,
      Err(err) => tracing::warn!(
        "Failed to start cleanup worker for window {handle}: {err}"
      ),
    }
  }
  drop(completion_tx);

  let completed_workers =
    wait_for_cleanup_workers(&completion_rx, started_workers, timeout);
  if completed_workers < worker_count {
    tracing::warn!(
      "Watcher cleanup timed out or could not start for {} of {} windows; \
       exiting so Windows can release watcher process resources.",
      worker_count - completed_workers,
      worker_count
    );
  }
}

fn wait_for_cleanup_workers(
  completion_rx: &mpsc::Receiver<isize>,
  worker_count: usize,
  timeout: Duration,
) -> usize {
  let deadline = std::time::Instant::now() + timeout;
  let mut completed_workers = 0usize;

  while completed_workers < worker_count {
    let remaining =
      deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
      break;
    }

    match completion_rx.recv_timeout(remaining) {
      Ok(_) => completed_workers += 1,
      Err(
        mpsc::RecvTimeoutError::Timeout
        | mpsc::RecvTimeoutError::Disconnected,
      ) => break,
    }
  }

  completed_workers
}

fn restore_managed_window(handle: isize) {
  let window = NativeWindow::from_handle(handle);

  // Remove DWM cloak first so the window becomes compositable before
  // `ShowWindow` makes it visible. Each window runs in a disposable
  // worker: undocumented shell COM can block forever for a destroyed or
  // hung HWND, and crash recovery must never keep the watcher process
  // alive forever.
  if let Err(err) = window.set_cloaked(false) {
    tracing::warn!(
      "Failed to uncloak window {handle} during cleanup: {err:?}"
    );
  }

  if let Err(err) = window.show() {
    tracing::warn!("Failed to show window {handle}: {err:?}");
  }

  if let Err(err) = window.set_taskbar_visibility(true) {
    tracing::warn!(
      "Failed to restore taskbar visibility for window {handle}: {err:?}"
    );
  }
  if let Err(err) = window.set_border_color(None) {
    tracing::warn!("Failed to reset border for window {handle}: {err:?}");
  }
  if let Err(err) =
    window.set_transparency(&OpacityValue::from_alpha(u8::MAX))
  {
    tracing::warn!(
      "Failed to restore opacity for window {handle}: {err:?}"
    );
  }
}

async fn query_initial_windows(
  client: &mut IpcClient,
) -> anyhow::Result<Vec<WindowDto>> {
  let query_message = "query windows";

  client
    .send(query_message)
    .await
    .context("Failed to send window query command.")?;

  client
    .client_response(query_message)
    .await
    .and_then(|response| match response.data {
      Some(ClientResponseData::Windows(data)) => Some(data),
      _ => None,
    })
    .map(|data| {
      data
        .windows
        .into_iter()
        .filter_map(|container| match container {
          ContainerDto::Window(window) => Some(window),
          _ => None,
        })
        .collect::<Vec<_>>()
    })
    .context("Invalid data in windows query response.")
}

async fn watch_managed_handles(
  client: &mut IpcClient,
  handles: &mut Vec<isize>,
) -> anyhow::Result<()> {
  let subscription_message =
    "sub -e window_managed window_unmanaged application_exiting";

  client
    .send(subscription_message)
    .await
    .context("Failed to send subscribe command to IPC server.")?;

  let subscription_id = client
    .client_response(subscription_message)
    .await
    .and_then(|response| match response.data {
      Some(ClientResponseData::EventSubscribe(data)) => {
        Some(data.subscription_id)
      }
      _ => None,
    })
    .context("No subscription ID in watcher event subscription.")?;

  loop {
    let event_data = client
      .event_subscription(&subscription_id)
      .await
      .and_then(|event| event.data);

    match event_data {
      Some(WmEvent::WindowManaged { managed_window }) => {
        if let ContainerDto::Window(window) = managed_window {
          tracing::info!("Watcher added handle: {}.", window.handle);
          handles.push(window.handle);
        }
      }
      Some(WmEvent::WindowUnmanaged {
        unmanaged_handle, ..
      }) => {
        tracing::info!("Watcher removed handle: {}.", unmanaged_handle);
        handles.retain(|&handle| handle != unmanaged_handle);
      }
      Some(WmEvent::ApplicationExiting) => {
        return Ok(());
      }
      Some(_) => unreachable!(),
      None => {
        anyhow::bail!("IPC connection closed unexpectedly.")
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use std::{sync::mpsc, thread, time::Duration};

  use super::wait_for_cleanup_workers;

  #[test]
  fn cleanup_wait_has_a_hard_deadline() {
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
      thread::sleep(Duration::from_millis(100));
      let _ = tx.send(1);
    });

    let completed =
      wait_for_cleanup_workers(&rx, 1, Duration::from_millis(10));
    assert_eq!(completed, 0);

    worker.join().expect("test worker should finish");
  }
}
