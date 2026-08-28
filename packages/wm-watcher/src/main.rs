// The `windows` or `console` subsystem (default is `console`) determines
// whether a console window is spawned on launch, if not already ran
// through a console. The following prevents this additional console window
// in release mode.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![warn(clippy::all, clippy::pedantic)]

use std::{
  collections::HashMap,
  io,
  sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
  },
  thread,
  time::{Duration, Instant},
};

use anyhow::Context;
use wm_common::{ClientResponseData, ContainerDto, WindowDto, WmEvent};
use wm_ipc_client::IpcClient;
use wm_platform::{
  NativeWindow, NativeWindowRecoveryIdentity, NativeWindowWindowsExt,
  OpacityValue,
};

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_WORKER_LIMIT: usize = 4;
const RECOVERY_MARKER_PREFIX: &str = "GlazeWM.Watcher.Recovery";

type CleanupTask = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, Eq, PartialEq)]
struct CleanupSummary {
  total_windows: usize,
  started_workers: usize,
  completed_windows: usize,
  skipped_windows: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryOutcome {
  Restored,
  SkippedIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManagedWindowRecord {
  id: String,
  handle: isize,
  identity: NativeWindowRecoveryIdentity,
  marker: String,
}

#[derive(Default)]
struct ManagedWindows {
  records: HashMap<isize, ManagedWindowRecord>,
}

impl ManagedWindows {
  fn from_windows<B: WindowRecoveryBackend>(
    windows: Vec<WindowDto>,
    backend: &B,
  ) -> Self {
    let mut managed = Self::default();
    for window in windows {
      managed.track_window(window, backend);
    }
    managed
  }

  fn track_window<B: WindowRecoveryBackend>(
    &mut self,
    window: WindowDto,
    backend: &B,
  ) -> bool {
    if self
      .records
      .get(&window.handle)
      .is_some_and(|record| record.id == window.id.to_string())
    {
      return false;
    }

    let handle = window.handle;
    let id = window.id.to_string();
    let record = match ManagedWindowRecord::capture(window, backend) {
      Ok(record) => record,
      Err(err) => {
        tracing::warn!(
          "Watcher skipped recovery tracking for logical window {id} / HWND \
           {handle}: {err}"
        );
        return false;
      }
    };

    self.insert_record(record, backend)
  }

  fn insert_record<B: WindowRecoveryBackend>(
    &mut self,
    record: ManagedWindowRecord,
    backend: &B,
  ) -> bool {
    if self
      .records
      .get(&record.handle)
      .is_some_and(|current| current.id == record.id)
    {
      return false;
    }
    if let Some(previous) = self.records.insert(record.handle, record) {
      previous.release_marker(backend);
    }
    true
  }

  fn remove<B: WindowRecoveryBackend>(
    &mut self,
    id: &str,
    handle: isize,
    backend: &B,
  ) -> bool {
    if !self
      .records
      .get(&handle)
      .is_some_and(|record| record.id == id)
    {
      return false;
    }
    if let Some(record) = self.records.remove(&handle) {
      record.release_marker(backend);
    }
    true
  }

  fn release_all_markers<B: WindowRecoveryBackend>(&self, backend: &B) {
    for record in self.records.values() {
      record.release_marker(backend);
    }
  }

  fn into_sorted_records(self) -> Vec<ManagedWindowRecord> {
    let mut records = self.records.into_values().collect::<Vec<_>>();
    records.sort_unstable_by_key(|record| record.handle);
    records
  }
}

impl ManagedWindowRecord {
  fn capture<B: WindowRecoveryBackend>(
    window: WindowDto,
    backend: &B,
  ) -> Result<Self, String> {
    let identity = backend.capture_identity(window.handle)?;
    if identity.class_name != window.class_name {
      return Err(format!(
        "native class changed from '{}' to '{}' before tracking",
        window.class_name, identity.class_name
      ));
    }

    let marker = format!("{RECOVERY_MARKER_PREFIX}.{}", window.id);
    backend.set_marker(window.handle, &marker)?;
    let confirmed_identity = backend.capture_identity(window.handle);
    let marker_present = backend.has_marker(window.handle, &marker);
    if confirmed_identity.as_ref() != Ok(&identity)
      || marker_present != Ok(true)
    {
      let _ = backend.remove_marker(window.handle, &marker);
      return Err(
        "native identity changed while the recovery marker was installed"
          .to_string(),
      );
    }

    Ok(Self {
      id: window.id.to_string(),
      handle: window.handle,
      identity,
      marker,
    })
  }

  fn is_current<B: WindowRecoveryBackend>(
    &self,
    backend: &B,
  ) -> Result<bool, String> {
    Ok(
      backend.capture_identity(self.handle)? == self.identity
        && backend.has_marker(self.handle, &self.marker)?,
    )
  }

  fn release_marker<B: WindowRecoveryBackend>(&self, backend: &B) {
    match self.is_current(backend) {
      Ok(true) => {
        if let Err(err) = backend.remove_marker(self.handle, &self.marker) {
          tracing::warn!(
            "Failed to remove watcher recovery marker for logical window {} \
             / HWND {}: {err}",
            self.id,
            self.handle
          );
        }
      }
      Ok(false) => {}
      Err(err) => tracing::debug!(
        "Could not revalidate watcher marker cleanup for logical window {} / \
         HWND {}: {err}",
        self.id,
        self.handle
      ),
    }
  }
}

trait WindowRecoveryBackend: Send + Sync + 'static {
  fn capture_identity(
    &self,
    handle: isize,
  ) -> Result<NativeWindowRecoveryIdentity, String>;
  fn set_marker(&self, handle: isize, marker: &str) -> Result<(), String>;
  fn has_marker(
    &self,
    handle: isize,
    marker: &str,
  ) -> Result<bool, String>;
  fn remove_marker(
    &self,
    handle: isize,
    marker: &str,
  ) -> Result<bool, String>;
  fn uncloak(&self, handle: isize) -> Result<(), String>;
  fn show(&self, handle: isize) -> Result<(), String>;
  fn show_in_taskbar(&self, handle: isize) -> Result<(), String>;
  fn clear_border(&self, handle: isize) -> Result<(), String>;
  fn make_opaque(&self, handle: isize) -> Result<(), String>;
}

#[derive(Clone, Copy)]
struct NativeRecoveryBackend;

impl WindowRecoveryBackend for NativeRecoveryBackend {
  fn capture_identity(
    &self,
    handle: isize,
  ) -> Result<NativeWindowRecoveryIdentity, String> {
    NativeWindow::from_handle(handle)
      .recovery_identity()
      .map_err(|error| error.to_string())
  }

  fn set_marker(&self, handle: isize, marker: &str) -> Result<(), String> {
    NativeWindow::from_handle(handle)
      .set_recovery_marker(marker)
      .map_err(|error| error.to_string())
  }

  fn has_marker(
    &self,
    handle: isize,
    marker: &str,
  ) -> Result<bool, String> {
    NativeWindow::from_handle(handle)
      .has_recovery_marker(marker)
      .map_err(|error| error.to_string())
  }

  fn remove_marker(
    &self,
    handle: isize,
    marker: &str,
  ) -> Result<bool, String> {
    NativeWindow::from_handle(handle)
      .remove_recovery_marker(marker)
      .map_err(|error| error.to_string())
  }

  fn uncloak(&self, handle: isize) -> Result<(), String> {
    NativeWindow::from_handle(handle)
      .set_cloaked(false)
      .map_err(|error| error.to_string())
  }

  fn show(&self, handle: isize) -> Result<(), String> {
    NativeWindow::from_handle(handle)
      .show()
      .map_err(|error| error.to_string())
  }

  fn show_in_taskbar(&self, handle: isize) -> Result<(), String> {
    NativeWindow::from_handle(handle)
      .set_taskbar_visibility(true)
      .map_err(|error| error.to_string())
  }

  fn clear_border(&self, handle: isize) -> Result<(), String> {
    NativeWindow::from_handle(handle)
      .set_border_color(None)
      .map_err(|error| error.to_string())
  }

  fn make_opaque(&self, handle: isize) -> Result<(), String> {
    NativeWindow::from_handle(handle)
      .set_transparency(&OpacityValue::from_alpha(u8::MAX))
      .map_err(|error| error.to_string())
  }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  tracing_subscriber::fmt().init();

  let mut client = IpcClient::connect().await?;
  let recovery_backend = NativeRecoveryBackend;

  // Get handles to windows that are already open on watcher launch.
  let mut managed_windows = ManagedWindows::from_windows(
    query_initial_windows(&mut client).await?,
    &recovery_backend,
  );

  // Update window handles on window manage/unmanage events.
  let subscribe_res = watch_managed_handles(
    &mut client,
    &mut managed_windows,
    &recovery_backend,
  )
  .await;

  if subscribe_res.is_ok() {
    client.close().await;
  }

  match subscribe_res {
    Ok(()) => {
      managed_windows.release_all_markers(&recovery_backend);
      tracing::info!("WM exited successfully. Skipping watcher cleanup.");
    }
    Err(err) => {
      tracing::info!(
        "Running watcher cleanup. WM exited unexpectedly: {}",
        err
      );

      restore_managed_windows(
        managed_windows.into_sorted_records(),
        CLEANUP_TIMEOUT,
      );
    }
  }

  Ok(())
}

fn restore_managed_windows(
  records: Vec<ManagedWindowRecord>,
  timeout: Duration,
) {
  let backend = Arc::new(NativeRecoveryBackend);
  let summary = run_cleanup_pool_with(
    records,
    timeout,
    Arc::new(move |record: &ManagedWindowRecord| {
      restore_managed_window(record, &*backend)
    }),
    |name, task| thread::Builder::new().name(name).spawn(task).map(drop),
  );

  if summary.completed_windows < summary.total_windows {
    tracing::warn!(
      "Watcher cleanup timed out or could not start for {} of {} windows \
       ({} of {} fixed workers started); exiting so Windows can release \
       watcher process resources.",
      summary.total_windows - summary.completed_windows,
      summary.total_windows,
      summary.started_workers,
      CLEANUP_WORKER_LIMIT.min(summary.total_windows),
    );
  }
  if summary.skipped_windows != 0 {
    tracing::warn!(
      "Watcher intentionally skipped {} window(s) whose HWND recovery \
       identity was stale or ambiguous.",
      summary.skipped_windows
    );
  }
}

fn run_cleanup_pool_with<T, R, S>(
  items: Vec<T>,
  timeout: Duration,
  restore: Arc<R>,
  mut spawn_worker: S,
) -> CleanupSummary
where
  T: Send + Sync + 'static,
  R: Fn(&T) -> RecoveryOutcome + Send + Sync + 'static,
  S: FnMut(String, CleanupTask) -> io::Result<()>,
{
  let total_windows = items.len();
  if total_windows == 0 {
    return CleanupSummary {
      total_windows,
      started_workers: 0,
      completed_windows: 0,
      skipped_windows: 0,
    };
  }

  let deadline = Instant::now() + timeout;
  let items = Arc::new(items);
  let next_index = Arc::new(AtomicUsize::new(0));
  let (completion_tx, completion_rx) = mpsc::channel();
  let mut started_workers = 0usize;

  for worker_index in 0..CLEANUP_WORKER_LIMIT.min(total_windows) {
    let items = Arc::clone(&items);
    let next_index = Arc::clone(&next_index);
    let restore = Arc::clone(&restore);
    let completion_tx = completion_tx.clone();
    let task = Box::new(move || loop {
      let index = next_index.fetch_add(1, Ordering::Relaxed);
      let Some(item) = items.get(index) else {
        break;
      };
      let outcome = restore(item);
      let _ = completion_tx.send(outcome);
    });
    let spawn_result = spawn_worker(
      format!("glazewm-watcher-cleanup-{worker_index}"),
      task,
    );

    match spawn_result {
      Ok(()) => started_workers += 1,
      Err(err) => tracing::warn!(
        "Failed to start fixed cleanup worker {worker_index}: {err}"
      ),
    }
  }
  drop(restore);
  drop(completion_tx);

  // If every spawn failed, do not run potentially blocking native cleanup
  // on the watcher main thread. Exiting is the deterministic bounded
  // fallback.
  let (completed_windows, skipped_windows) = if started_workers == 0 {
    (0, 0)
  } else {
    wait_for_cleanup_workers(&completion_rx, total_windows, deadline)
  };

  CleanupSummary {
    total_windows,
    started_workers,
    completed_windows,
    skipped_windows,
  }
}

fn wait_for_cleanup_workers(
  completion_rx: &mpsc::Receiver<RecoveryOutcome>,
  window_count: usize,
  deadline: Instant,
) -> (usize, usize) {
  let mut completed_windows = 0usize;
  let mut skipped_windows = 0usize;

  while completed_windows < window_count {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      break;
    }

    match completion_rx.recv_timeout(remaining) {
      Ok(outcome) => {
        completed_windows += 1;
        skipped_windows +=
          usize::from(outcome == RecoveryOutcome::SkippedIdentity);
      }
      Err(
        mpsc::RecvTimeoutError::Timeout
        | mpsc::RecvTimeoutError::Disconnected,
      ) => break,
    }
  }

  (completed_windows, skipped_windows)
}

fn restore_managed_window<B: WindowRecoveryBackend>(
  record: &ManagedWindowRecord,
  backend: &B,
) -> RecoveryOutcome {
  if !revalidate_before_restore(record, backend, "initial recovery") {
    return RecoveryOutcome::SkippedIdentity;
  }

  // Remove DWM cloak first so the window becomes compositable before
  // `ShowWindow` makes it visible. Native restores run in the fixed
  // disposable worker pool: undocumented shell COM can block forever for
  // a destroyed or hung HWND, and crash recovery must never keep the
  // watcher process alive forever.
  if let Err(err) = backend.uncloak(record.handle) {
    tracing::warn!(
      "Failed to uncloak logical window {} / HWND {} during cleanup: {err}",
      record.id,
      record.handle
    );
  }

  if !revalidate_before_restore(record, backend, "post-uncloak recovery") {
    return RecoveryOutcome::SkippedIdentity;
  }
  if let Err(err) = backend.show(record.handle) {
    tracing::warn!(
      "Failed to show logical window {} / HWND {}: {err}",
      record.id,
      record.handle
    );
  }

  if !revalidate_before_restore(record, backend, "post-show recovery") {
    return RecoveryOutcome::SkippedIdentity;
  }
  if let Err(err) = backend.show_in_taskbar(record.handle) {
    tracing::warn!(
      "Failed to restore taskbar visibility for logical window {} / HWND {}: \
       {err}",
      record.id,
      record.handle
    );
  }

  if !revalidate_before_restore(record, backend, "post-taskbar recovery") {
    return RecoveryOutcome::SkippedIdentity;
  }
  if let Err(err) = backend.clear_border(record.handle) {
    tracing::warn!(
      "Failed to reset border for logical window {} / HWND {}: {err}",
      record.id,
      record.handle
    );
  }

  if !revalidate_before_restore(record, backend, "post-border recovery") {
    return RecoveryOutcome::SkippedIdentity;
  }
  if let Err(err) = backend.make_opaque(record.handle) {
    tracing::warn!(
      "Failed to restore opacity for logical window {} / HWND {}: {err}",
      record.id,
      record.handle
    );
  }

  if revalidate_before_restore(record, backend, "marker teardown") {
    if let Err(err) = backend.remove_marker(record.handle, &record.marker)
    {
      tracing::warn!(
        "Failed to remove recovery marker for logical window {} / HWND {}: \
         {err}",
        record.id,
        record.handle
      );
    }
  }

  RecoveryOutcome::Restored
}

fn revalidate_before_restore<B: WindowRecoveryBackend>(
  record: &ManagedWindowRecord,
  backend: &B,
  stage: &str,
) -> bool {
  match record.is_current(backend) {
    Ok(true) => true,
    Ok(false) => {
      tracing::warn!(
        "Skipped {stage} for logical window {}: HWND {} now belongs to a \
         different native identity or lacks its recovery marker.",
        record.id,
        record.handle
      );
      false
    }
    Err(err) => {
      tracing::warn!(
        "Skipped {stage} for logical window {} / HWND {} because identity \
         could not be verified: {err}",
        record.id,
        record.handle
      );
      false
    }
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
  windows: &mut ManagedWindows,
  backend: &NativeRecoveryBackend,
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
          let handle = window.handle;
          let id = window.id.to_string();
          if windows.track_window(window, backend) {
            tracing::info!(
              "Watcher added recovery identity for logical window {id} / \
               HWND {handle}."
            );
          }
        }
      }
      Some(WmEvent::WindowUnmanaged {
        unmanaged_id,
        unmanaged_handle,
      }) => {
        if windows.remove(
          &unmanaged_id.to_string(),
          unmanaged_handle,
          backend,
        ) {
          tracing::info!(
            "Watcher removed recovery identity for logical window \
             {unmanaged_id} / HWND {unmanaged_handle}."
          );
        } else {
          tracing::debug!(
            "Watcher ignored stale/unmatched unmanage for logical window \
             {unmanaged_id} / HWND {unmanaged_handle}."
          );
        }
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
  use std::{
    collections::{HashSet, VecDeque},
    sync::{mpsc, Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
  };

  use super::*;

  #[derive(Default)]
  struct MockRecoveryState {
    identities: HashMap<
      isize,
      VecDeque<Result<NativeWindowRecoveryIdentity, String>>,
    >,
    markers: HashSet<(isize, String)>,
    operations: Vec<&'static str>,
  }

  #[derive(Default)]
  struct MockRecoveryBackend {
    state: Mutex<MockRecoveryState>,
  }

  impl MockRecoveryBackend {
    fn set_identity_sequence(
      &self,
      handle: isize,
      identities: Vec<Result<NativeWindowRecoveryIdentity, String>>,
    ) {
      self
        .state
        .lock()
        .expect("mock recovery state should not be poisoned")
        .identities
        .insert(handle, identities.into());
    }

    fn install_marker(&self, handle: isize, marker: &str) {
      self
        .state
        .lock()
        .expect("mock recovery state should not be poisoned")
        .markers
        .insert((handle, marker.to_string()));
    }

    fn operations(&self) -> Vec<&'static str> {
      self
        .state
        .lock()
        .expect("mock recovery state should not be poisoned")
        .operations
        .clone()
    }

    fn record_operation(&self, operation: &'static str) {
      self
        .state
        .lock()
        .expect("mock recovery state should not be poisoned")
        .operations
        .push(operation);
    }
  }

  impl WindowRecoveryBackend for MockRecoveryBackend {
    fn capture_identity(
      &self,
      handle: isize,
    ) -> Result<NativeWindowRecoveryIdentity, String> {
      let mut state = self
        .state
        .lock()
        .expect("mock recovery state should not be poisoned");
      let identities = state
        .identities
        .get_mut(&handle)
        .ok_or_else(|| "destroyed HWND".to_string())?;
      if identities.len() > 1 {
        identities.pop_front().expect("nonempty identity sequence")
      } else {
        identities
          .front()
          .cloned()
          .unwrap_or_else(|| Err("destroyed HWND".to_string()))
      }
    }

    fn set_marker(
      &self,
      handle: isize,
      marker: &str,
    ) -> Result<(), String> {
      self.install_marker(handle, marker);
      Ok(())
    }

    fn has_marker(
      &self,
      handle: isize,
      marker: &str,
    ) -> Result<bool, String> {
      Ok(
        self
          .state
          .lock()
          .expect("mock recovery state should not be poisoned")
          .markers
          .contains(&(handle, marker.to_string())),
      )
    }

    fn remove_marker(
      &self,
      handle: isize,
      marker: &str,
    ) -> Result<bool, String> {
      Ok(
        self
          .state
          .lock()
          .expect("mock recovery state should not be poisoned")
          .markers
          .remove(&(handle, marker.to_string())),
      )
    }

    fn uncloak(&self, _handle: isize) -> Result<(), String> {
      self.record_operation("uncloak");
      Ok(())
    }

    fn show(&self, _handle: isize) -> Result<(), String> {
      self.record_operation("show");
      Ok(())
    }

    fn show_in_taskbar(&self, _handle: isize) -> Result<(), String> {
      self.record_operation("taskbar");
      Ok(())
    }

    fn clear_border(&self, _handle: isize) -> Result<(), String> {
      self.record_operation("border");
      Ok(())
    }

    fn make_opaque(&self, _handle: isize) -> Result<(), String> {
      self.record_operation("opacity");
      Ok(())
    }
  }

  fn identity(
    process_id: u32,
    thread_id: u32,
    process_creation_time: u64,
    class_name: &str,
  ) -> NativeWindowRecoveryIdentity {
    NativeWindowRecoveryIdentity {
      process_id,
      thread_id,
      process_creation_time,
      class_name: class_name.to_string(),
    }
  }

  fn record(
    id: &str,
    handle: isize,
    identity: NativeWindowRecoveryIdentity,
  ) -> ManagedWindowRecord {
    ManagedWindowRecord {
      id: id.to_string(),
      handle,
      identity,
      marker: format!("{RECOVERY_MARKER_PREFIX}.{id}"),
    }
  }

  fn spawn_test_worker(name: String, task: CleanupTask) -> io::Result<()> {
    thread::Builder::new().name(name).spawn(task).map(drop)
  }

  #[test]
  fn managed_window_tracking_deduplicates_and_ignores_stale_unmanage() {
    let backend = MockRecoveryBackend::default();
    let native_identity = identity(10, 20, 30, "TestWindow");
    backend.set_identity_sequence(11, vec![Ok(native_identity.clone())]);
    let first = record("first", 11, native_identity.clone());
    let replacement = record("replacement", 11, native_identity);
    backend.install_marker(11, &first.marker);
    backend.install_marker(11, &replacement.marker);

    let mut windows = ManagedWindows::default();
    assert!(windows.insert_record(first.clone(), &backend));
    assert!(!windows.insert_record(first, &backend));
    assert!(windows.insert_record(replacement.clone(), &backend));
    assert!(!windows.remove("first", 11, &backend));
    assert_eq!(windows.records.get(&11), Some(&replacement));
    assert!(windows.remove("replacement", 11, &backend));
    assert!(windows.records.is_empty());
  }

  #[test]
  fn matching_identity_and_marker_restore_once_then_release_marker() {
    let backend = MockRecoveryBackend::default();
    let expected = identity(10, 20, 30, "TestWindow");
    let record = record("window", 11, expected.clone());
    backend.set_identity_sequence(11, vec![Ok(expected)]);
    backend.install_marker(11, &record.marker);

    assert_eq!(
      restore_managed_window(&record, &backend),
      RecoveryOutcome::Restored
    );
    assert_eq!(
      backend.operations(),
      vec!["uncloak", "show", "taskbar", "border", "opacity"]
    );
    assert_eq!(backend.has_marker(11, &record.marker), Ok(false));
  }

  #[test]
  fn every_native_identity_component_rejects_reused_hwnd() {
    let expected = identity(10, 20, 30, "TestWindow");
    let mismatches = [
      identity(11, 20, 30, "TestWindow"),
      identity(10, 21, 30, "TestWindow"),
      identity(10, 20, 31, "TestWindow"),
      identity(10, 20, 30, "ReplacementWindow"),
    ];

    for mismatch in mismatches {
      let backend = MockRecoveryBackend::default();
      let record = record("window", 11, expected.clone());
      backend.set_identity_sequence(11, vec![Ok(mismatch)]);
      backend.install_marker(11, &record.marker);

      assert_eq!(
        restore_managed_window(&record, &backend),
        RecoveryOutcome::SkippedIdentity
      );
      assert!(backend.operations().is_empty());
      assert_eq!(backend.has_marker(11, &record.marker), Ok(true));
    }
  }

  #[test]
  fn missing_marker_or_destroyed_hwnd_never_runs_native_restore() {
    let expected = identity(10, 20, 30, "TestWindow");

    let missing_marker_backend = MockRecoveryBackend::default();
    let missing_marker_record = record("window", 11, expected.clone());
    missing_marker_backend
      .set_identity_sequence(11, vec![Ok(expected.clone())]);
    assert_eq!(
      restore_managed_window(
        &missing_marker_record,
        &missing_marker_backend
      ),
      RecoveryOutcome::SkippedIdentity
    );
    assert!(missing_marker_backend.operations().is_empty());

    let destroyed_backend = MockRecoveryBackend::default();
    let destroyed_record = record("window", 11, expected);
    destroyed_backend
      .set_identity_sequence(11, vec![Err("destroyed HWND".to_string())]);
    destroyed_backend.install_marker(11, &destroyed_record.marker);
    assert_eq!(
      restore_managed_window(&destroyed_record, &destroyed_backend),
      RecoveryOutcome::SkippedIdentity
    );
    assert!(destroyed_backend.operations().is_empty());
  }

  #[test]
  fn hwnd_reuse_mid_restore_stops_before_mutating_replacement() {
    let backend = MockRecoveryBackend::default();
    let expected = identity(10, 20, 30, "TestWindow");
    let replacement = identity(11, 21, 31, "ReplacementWindow");
    let record = record("window", 11, expected.clone());
    backend.set_identity_sequence(11, vec![Ok(expected), Ok(replacement)]);
    backend.install_marker(11, &record.marker);

    assert_eq!(
      restore_managed_window(&record, &backend),
      RecoveryOutcome::SkippedIdentity
    );
    assert_eq!(backend.operations(), vec!["uncloak"]);
  }

  #[test]
  fn cleanup_summary_counts_identity_skips_without_growing_work() {
    let summary = run_cleanup_pool_with(
      (0..100).collect(),
      Duration::from_secs(1),
      Arc::new(|item: &i32| {
        if item % 2 == 0 {
          RecoveryOutcome::Restored
        } else {
          RecoveryOutcome::SkippedIdentity
        }
      }),
      spawn_test_worker,
    );

    assert_eq!(summary.total_windows, 100);
    assert_eq!(summary.started_workers, CLEANUP_WORKER_LIMIT);
    assert_eq!(summary.completed_windows, 100);
    assert_eq!(summary.skipped_windows, 50);
  }

  #[test]
  fn ten_thousand_handles_use_at_most_the_fixed_worker_limit() {
    let restored = Arc::new(Mutex::new(Vec::new()));
    let restored_for_worker = Arc::clone(&restored);
    let summary = run_cleanup_pool_with(
      (0..10_000).collect(),
      Duration::from_secs(2),
      Arc::new(move |handle: &i32| {
        restored_for_worker
          .lock()
          .expect("restore list should not be poisoned")
          .push(*handle);
        RecoveryOutcome::Restored
      }),
      spawn_test_worker,
    );

    assert_eq!(
      summary,
      CleanupSummary {
        total_windows: 10_000,
        started_workers: CLEANUP_WORKER_LIMIT,
        completed_windows: 10_000,
        skipped_windows: 0,
      }
    );
    let mut restored = Arc::try_unwrap(restored)
      .expect("all test workers should have exited")
      .into_inner()
      .expect("restore list should not be poisoned");
    restored.sort_unstable();
    assert_eq!(restored, (0..10_000).collect::<Vec<_>>());
  }

  #[test]
  fn partial_worker_start_still_drains_the_shared_queue() {
    let mut spawn_attempt = 0;
    let summary = run_cleanup_pool_with(
      (0..100).collect(),
      Duration::from_secs(1),
      Arc::new(|_: &i32| RecoveryOutcome::Restored),
      |name, task| {
        spawn_attempt += 1;
        if spawn_attempt == 1 {
          Err(io::Error::other("injected spawn failure"))
        } else {
          spawn_test_worker(name, task)
        }
      },
    );

    assert_eq!(summary.total_windows, 100);
    assert_eq!(summary.started_workers, CLEANUP_WORKER_LIMIT - 1);
    assert_eq!(summary.completed_windows, 100);
  }

  #[test]
  fn total_worker_start_failure_returns_immediately_without_native_work() {
    let restore_calls = Arc::new(AtomicUsize::new(0));
    let restore_calls_for_worker = Arc::clone(&restore_calls);
    let start = Instant::now();
    let summary = run_cleanup_pool_with(
      (0..100).collect(),
      Duration::from_secs(5),
      Arc::new(move |_: &i32| {
        restore_calls_for_worker.fetch_add(1, Ordering::Relaxed);
        RecoveryOutcome::Restored
      }),
      |_, _| Err(io::Error::other("injected spawn failure")),
    );

    assert_eq!(summary.started_workers, 0);
    assert_eq!(summary.completed_windows, 0);
    assert_eq!(restore_calls.load(Ordering::Relaxed), 0);
    assert!(start.elapsed() < Duration::from_millis(100));
  }

  #[test]
  fn cleanup_pool_observes_one_absolute_deadline() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let gate_for_worker = Arc::clone(&gate);
    let restored = Arc::new(AtomicUsize::new(0));
    let restored_for_worker = Arc::clone(&restored);
    let start = Instant::now();
    let summary = run_cleanup_pool_with(
      (0..8).collect(),
      Duration::from_millis(10),
      Arc::new(move |_: &i32| {
        let (lock, condition) = &*gate_for_worker;
        let mut released =
          lock.lock().expect("gate should not be poisoned");
        while !*released {
          released = condition
            .wait(released)
            .expect("gate should not be poisoned");
        }
        restored_for_worker.fetch_add(1, Ordering::Relaxed);
        RecoveryOutcome::Restored
      }),
      spawn_test_worker,
    );

    assert_eq!(summary.started_workers, CLEANUP_WORKER_LIMIT);
    assert_eq!(summary.completed_windows, 0);
    assert!(start.elapsed() < Duration::from_millis(100));

    let (lock, condition) = &*gate;
    *lock.lock().expect("gate should not be poisoned") = true;
    condition.notify_all();
    let settle_deadline = Instant::now() + Duration::from_secs(1);
    while restored.load(Ordering::Relaxed) != 8
      && Instant::now() < settle_deadline
    {
      thread::yield_now();
    }
    assert_eq!(restored.load(Ordering::Relaxed), 8);
  }

  #[test]
  fn cleanup_wait_has_a_hard_deadline() {
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
      thread::sleep(Duration::from_millis(100));
      let _ = tx.send(RecoveryOutcome::Restored);
    });

    let completed = wait_for_cleanup_workers(
      &rx,
      1,
      Instant::now() + Duration::from_millis(10),
    );
    assert_eq!(completed, (0, 0));

    worker.join().expect("test worker should finish");
  }
}
