#[cfg(target_os = "windows")]
use std::collections::HashMap;
use std::collections::VecDeque;
#[cfg(target_os = "windows")]
use std::sync::{Arc, Mutex};

#[cfg(not(target_os = "windows"))]
use tokio::sync::mpsc;
#[cfg(target_os = "windows")]
use tokio::sync::Notify;

use crate::{platform_impl, Dispatcher, WindowEvent, WindowId};

/// Maximum number of already-queued native events compacted in one pass.
///
/// Keeping the drain bounded prevents a continuously noisy native producer
/// from holding `next_event` inside a synchronous `try_recv` loop forever.
#[cfg(not(target_os = "windows"))]
const MAX_COMPACTION_BATCH: usize = 256;

/// Maximum number of native Windows events retained before noisy
/// location/title notifications yield to lifecycle events.
#[cfg(target_os = "windows")]
const MAX_NATIVE_EVENT_QUEUE: usize = 4_096;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CoalescibleEvent {
  MovedOrResized(WindowId),
  TitleChanged(WindowId),
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum LatestEventLane {
  Visibility,
  Minimize,
  Focus,
  Motion,
  Title,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct LatestEventKey {
  window_id: WindowId,
  generation: u64,
  lane: LatestEventLane,
}

#[cfg(target_os = "windows")]
struct LatestEventRecord {
  sequence: u64,
  event: WindowEvent,
}

#[cfg(target_os = "windows")]
enum NativeEventToken {
  Latest { key: LatestEventKey, sequence: u64 },
  Barrier(WindowEvent),
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, Default)]
struct WindowGeneration {
  generation: u64,
  destroy_pending: bool,
}

/// Fixed-capacity native event history plus bounded latest-state records.
///
/// Replaceable state updates retain only the newest value for an HWND and
/// generation. Destroy is an explicit generation barrier. If pressure
/// cannot be represented without losing a barrier, history is cleared and
/// one out-of-band reconciliation request is delivered before newer
/// events.
#[cfg(target_os = "windows")]
struct NativeEventQueue {
  order: VecDeque<NativeEventToken>,
  latest: HashMap<LatestEventKey, LatestEventRecord>,
  generations: HashMap<WindowId, WindowGeneration>,
  next_sequence: u64,
  reconcile_pending: bool,
}

#[cfg(target_os = "windows")]
impl NativeEventQueue {
  fn new() -> Self {
    Self {
      order: VecDeque::with_capacity(MAX_NATIVE_EVENT_QUEUE),
      latest: HashMap::new(),
      generations: HashMap::new(),
      next_sequence: 1,
      reconcile_pending: false,
    }
  }

  fn len(&self) -> usize {
    self.order.len()
  }

  fn push(&mut self, event: WindowEvent, limit: usize) {
    let limit = limit.max(1);

    match event {
      WindowEvent::Destroyed { window_id, .. } => {
        if self
          .generations
          .get(&window_id)
          .is_some_and(|generation| generation.destroy_pending)
        {
          return;
        }

        self.latest.retain(|key, _| key.window_id != window_id);
        self.compact_stale_tokens();
        if self.order.len() >= limit {
          self.begin_overflow_reconciliation();
        }
        let generation = self.generations.entry(window_id).or_default();
        generation.generation = generation.generation.wrapping_add(1);
        generation.destroy_pending = true;
        self.order.push_back(NativeEventToken::Barrier(event));
      }
      WindowEvent::MovedOrResized {
        is_interactive_start: true,
        ..
      }
      | WindowEvent::MovedOrResized {
        is_interactive_end: true,
        ..
      }
      | WindowEvent::ReconcileRequested { .. } => {
        self.push_barrier(event, limit);
      }
      event => self.push_latest(event, limit),
    }
  }

  fn push_latest(&mut self, event: WindowEvent, limit: usize) {
    let Some((window_id, lane)) = latest_event_identity(&event) else {
      self.push_barrier(event, limit);
      return;
    };

    if lane == LatestEventLane::Focus {
      self
        .latest
        .retain(|key, _| key.lane != LatestEventLane::Focus);
    }
    if matches!(event, WindowEvent::Shown { .. }) {
      self
        .generations
        .entry(window_id)
        .or_default()
        .destroy_pending = false;
    }
    let generation = self
      .generations
      .get(&window_id)
      .copied()
      .unwrap_or_default()
      .generation;
    let key = LatestEventKey {
      window_id,
      generation,
      lane,
    };

    self.compact_stale_tokens();
    if self.order.len() >= limit {
      if let Some(record) = self.latest.get_mut(&key) {
        // The existing token will deliver this newest state. Under
        // capacity pressure preserving bounded latest state is
        // preferable to adding another ordering token.
        record.event = event;
        return;
      }
      if matches!(lane, LatestEventLane::Motion | LatestEventLane::Title) {
        return;
      }
      self.begin_overflow_reconciliation();
    }

    let sequence = self.take_sequence();
    self
      .latest
      .insert(key, LatestEventRecord { sequence, event });
    self
      .order
      .push_back(NativeEventToken::Latest { key, sequence });
  }

  fn push_barrier(&mut self, event: WindowEvent, limit: usize) {
    self.compact_stale_tokens();
    if self.order.len() >= limit {
      self.begin_overflow_reconciliation();
    }
    self.order.push_back(NativeEventToken::Barrier(event));
  }

  fn begin_overflow_reconciliation(&mut self) {
    self.order.clear();
    self.latest.clear();
    self.generations.clear();
    self.reconcile_pending = true;
  }

  fn take_sequence(&mut self) -> u64 {
    let sequence = self.next_sequence;
    self.next_sequence = self.next_sequence.wrapping_add(1);
    if self.next_sequence == 0 {
      self.next_sequence = 1;
      self.reconcile_pending = true;
    }
    sequence
  }

  fn compact_stale_tokens(&mut self) {
    self.order.retain(|token| match token {
      NativeEventToken::Latest { key, sequence } => self
        .latest
        .get(key)
        .is_some_and(|record| record.sequence == *sequence),
      NativeEventToken::Barrier(_) => true,
    });
  }

  fn pop(&mut self) -> Option<WindowEvent> {
    if self.reconcile_pending {
      self.reconcile_pending = false;
      return Some(WindowEvent::ReconcileRequested {
        notification: crate::WindowEventNotification(None),
      });
    }

    while let Some(token) = self.order.pop_front() {
      let event = match token {
        NativeEventToken::Latest { key, sequence } => {
          let is_current = self
            .latest
            .get(&key)
            .is_some_and(|record| record.sequence == sequence);
          if !is_current {
            continue;
          }
          self.latest.remove(&key).map(|record| record.event)
        }
        NativeEventToken::Barrier(event) => Some(event),
      };
      if self.order.is_empty() && self.latest.is_empty() {
        self.generations.clear();
      }
      if event.is_some() {
        return event;
      }
    }

    self.latest.clear();
    self.generations.clear();
    None
  }
}

#[cfg(target_os = "windows")]
fn latest_event_identity(
  event: &WindowEvent,
) -> Option<(WindowId, LatestEventLane)> {
  match event {
    WindowEvent::Focused { window, .. } => {
      Some((window.id(), LatestEventLane::Focus))
    }
    WindowEvent::Hidden { window, .. }
    | WindowEvent::Shown { window, .. } => {
      Some((window.id(), LatestEventLane::Visibility))
    }
    WindowEvent::MovedOrResized { window, .. } => {
      Some((window.id(), LatestEventLane::Motion))
    }
    WindowEvent::Minimized { window, .. }
    | WindowEvent::MinimizeEnded { window, .. } => {
      Some((window.id(), LatestEventLane::Minimize))
    }
    WindowEvent::TitleChanged { window, .. } => {
      Some((window.id(), LatestEventLane::Title))
    }
    WindowEvent::Destroyed { .. }
    | WindowEvent::ReconcileRequested { .. } => None,
  }
}

#[cfg(target_os = "windows")]
struct NativeEventQueueShared {
  queue: Mutex<NativeEventQueue>,
  notify: Notify,
}

/// Nonblocking producer used by the `WinEvent` callback.
///
/// Lifecycle and interactive-boundary events are never dropped. Once the
/// queue reaches its noisy-event limit, a lifecycle event evicts the
/// oldest coalescible notification; additional noisy notifications are
/// discarded because their handlers query the latest native state when an
/// earlier notification is processed.
#[cfg(target_os = "windows")]
#[derive(Clone)]
pub(crate) struct NativeWindowEventSender {
  shared: Arc<NativeEventQueueShared>,
}

#[cfg(target_os = "windows")]
impl NativeWindowEventSender {
  pub(crate) fn send(&self, event: WindowEvent) {
    self.send_with_limit(event, MAX_NATIVE_EVENT_QUEUE);
  }

  fn send_with_limit(&self, event: WindowEvent, limit: usize) {
    let mut queue = self
      .shared
      .queue
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);
    queue.push(event, limit);
    debug_assert!(queue.len() <= limit.max(1));
    drop(queue);
    self.shared.notify.notify_one();
  }
}

#[cfg(target_os = "windows")]
struct NativeWindowEventReceiver {
  shared: Arc<NativeEventQueueShared>,
}

#[cfg(target_os = "windows")]
impl NativeWindowEventReceiver {
  async fn recv(&self) -> Option<WindowEvent> {
    loop {
      let notified = self.shared.notify.notified();
      if let Some(event) = self
        .shared
        .queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pop()
      {
        return Some(event);
      }
      notified.await;
    }
  }
}

#[cfg(target_os = "windows")]
fn native_window_event_channel(
) -> (NativeWindowEventSender, NativeWindowEventReceiver) {
  let shared = Arc::new(NativeEventQueueShared {
    queue: Mutex::new(NativeEventQueue::new()),
    notify: Notify::new(),
  });
  (
    NativeWindowEventSender {
      shared: shared.clone(),
    },
    NativeWindowEventReceiver { shared },
  )
}

/// A listener for system-wide window events.
pub struct WindowListener {
  #[cfg(not(target_os = "windows"))]
  event_rx: mpsc::UnboundedReceiver<WindowEvent>,
  #[cfg(target_os = "windows")]
  event_rx: NativeWindowEventReceiver,
  pending_events: VecDeque<WindowEvent>,

  /// Inner platform-specific window listener.
  inner: platform_impl::WindowListener,
}

impl WindowListener {
  /// Creates a new window listener.
  pub fn new(dispatcher: &Dispatcher) -> crate::Result<Self> {
    #[cfg(not(target_os = "windows"))]
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    #[cfg(target_os = "windows")]
    let (event_tx, event_rx) = native_window_event_channel();
    let inner = platform_impl::WindowListener::new(event_tx, dispatcher)?;

    Ok(Self {
      event_rx,
      pending_events: VecDeque::new(),
      inner,
    })
  }

  /// Returns the next window event from the listener.
  ///
  /// This will block until a window event is available.
  pub async fn next_event(&mut self) -> Option<WindowEvent> {
    if self.pending_events.is_empty() {
      let first_event = self.event_rx.recv().await?;
      Self::push_compacted(&mut self.pending_events, first_event);

      #[cfg(not(target_os = "windows"))]
      // Compact a bounded snapshot of events which accumulated while the
      // WM loop was processing other work. Lifecycle events and
      // interactive move/resize boundaries are never coalesced and
      // remain ordering barriers between noisy notifications.
      for _ in 1..MAX_COMPACTION_BATCH {
        let Ok(event) = self.event_rx.try_recv() else {
          break;
        };
        Self::push_compacted(&mut self.pending_events, event);
      }
    }

    self.pending_events.pop_front()
  }

  fn push_compacted(
    pending_events: &mut VecDeque<WindowEvent>,
    event: WindowEvent,
  ) {
    if let Some(key) = Self::coalescible_key(&event) {
      if pending_events
        .back()
        .and_then(Self::coalescible_key)
        .is_some_and(|previous_key| previous_key == key)
      {
        *pending_events
          .back_mut()
          .expect("compaction queue unexpectedly empty") = event;
        return;
      }
    }

    pending_events.push_back(event);
  }

  fn coalescible_key(event: &WindowEvent) -> Option<CoalescibleEvent> {
    match event {
      WindowEvent::MovedOrResized {
        window,
        is_interactive_start: false,
        is_interactive_end: false,
        ..
      } => Some(CoalescibleEvent::MovedOrResized(window.id())),
      WindowEvent::TitleChanged { window, .. } => {
        Some(CoalescibleEvent::TitleChanged(window.id()))
      }
      _ => None,
    }
  }

  /// Terminates the window listener.
  pub fn terminate(&mut self) {
    self.inner.terminate();
  }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use super::*;
  use crate::WindowEventNotification;

  fn window(handle: isize) -> crate::NativeWindow {
    platform_impl::NativeWindow::new(handle).into()
  }

  fn moved(
    handle: isize,
    is_interactive_start: bool,
    is_interactive_end: bool,
  ) -> WindowEvent {
    WindowEvent::MovedOrResized {
      window: window(handle),
      is_interactive_start,
      is_interactive_end,
      notification: WindowEventNotification(None),
    }
  }

  fn title(handle: isize) -> WindowEvent {
    WindowEvent::TitleChanged {
      window: window(handle),
      notification: WindowEventNotification(None),
    }
  }

  fn hidden(handle: isize) -> WindowEvent {
    WindowEvent::Hidden {
      window: window(handle),
      notification: WindowEventNotification(None),
    }
  }

  fn shown(handle: isize) -> WindowEvent {
    WindowEvent::Shown {
      window: window(handle),
      notification: WindowEventNotification(None),
    }
  }

  fn destroyed(handle: isize) -> WindowEvent {
    WindowEvent::Destroyed {
      window_id: WindowId(handle),
      notification: WindowEventNotification(None),
    }
  }

  #[test]
  fn coalesces_only_adjacent_noisy_events_for_the_same_window() {
    let mut pending = VecDeque::new();
    WindowListener::push_compacted(&mut pending, moved(1, false, false));
    WindowListener::push_compacted(&mut pending, moved(1, false, false));
    WindowListener::push_compacted(&mut pending, moved(2, false, false));
    WindowListener::push_compacted(&mut pending, title(2));
    WindowListener::push_compacted(&mut pending, title(2));

    assert_eq!(pending.len(), 3);
    assert!(matches!(
      pending.pop_front(),
      Some(WindowEvent::MovedOrResized { ref window, .. })
        if window.id() == WindowId(1)
    ));
    assert!(matches!(
      pending.pop_front(),
      Some(WindowEvent::MovedOrResized { ref window, .. })
        if window.id() == WindowId(2)
    ));
    assert!(matches!(
      pending.pop_front(),
      Some(WindowEvent::TitleChanged { ref window, .. })
        if window.id() == WindowId(2)
    ));
  }

  #[test]
  fn lifecycle_and_interactive_events_are_lossless_ordering_barriers() {
    let mut pending = VecDeque::new();
    WindowListener::push_compacted(&mut pending, moved(1, false, false));
    WindowListener::push_compacted(&mut pending, hidden(1));
    WindowListener::push_compacted(&mut pending, hidden(1));
    WindowListener::push_compacted(&mut pending, moved(1, false, false));
    WindowListener::push_compacted(&mut pending, moved(1, true, false));
    WindowListener::push_compacted(&mut pending, moved(1, false, true));

    assert_eq!(pending.len(), 6);
    assert!(matches!(pending[0], WindowEvent::MovedOrResized { .. }));
    assert!(matches!(pending[1], WindowEvent::Hidden { .. }));
    assert!(matches!(pending[2], WindowEvent::Hidden { .. }));
    assert!(matches!(pending[3], WindowEvent::MovedOrResized { .. }));
    assert!(matches!(
      pending[4],
      WindowEvent::MovedOrResized {
        is_interactive_start: true,
        is_interactive_end: false,
        ..
      }
    ));
    assert!(matches!(
      pending[5],
      WindowEvent::MovedOrResized {
        is_interactive_start: false,
        is_interactive_end: true,
        ..
      }
    ));
  }

  #[test]
  fn native_queue_bounds_noise_and_preserves_lifecycle_events() {
    let (sender, receiver) = native_window_event_channel();
    sender.send_with_limit(moved(1, false, false), 2);
    sender.send_with_limit(moved(2, false, false), 2);
    sender.send_with_limit(moved(3, false, false), 2);

    let queued = receiver
      .shared
      .queue
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(queued.len(), 2);
    drop(queued);

    sender.send_with_limit(hidden(3), 2);
    let mut queued = receiver
      .shared
      .queue
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(queued.len(), 1);
    assert!(matches!(
      queued.pop(),
      Some(WindowEvent::ReconcileRequested { .. })
    ));
    assert!(matches!(queued.pop(), Some(WindowEvent::Hidden { .. })));
  }

  #[test]
  fn native_queue_hard_bounds_lifecycle_only_pressure() {
    let (sender, receiver) = native_window_event_channel();
    sender.send_with_limit(hidden(1), 1);
    sender.send_with_limit(hidden(2), 1);

    let mut queued = receiver
      .shared
      .queue
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(queued.len(), 1);
    assert!(matches!(
      queued.pop(),
      Some(WindowEvent::ReconcileRequested { .. })
    ));
    assert!(matches!(queued.pop(), Some(WindowEvent::Hidden { .. })));
    assert_eq!(queued.len(), 0);
  }

  #[test]
  fn destroy_is_a_generation_barrier_for_reused_numeric_hwnd() {
    let (sender, receiver) = native_window_event_channel();
    sender.send_with_limit(shown(42), 8);
    sender.send_with_limit(destroyed(42), 8);
    sender.send_with_limit(shown(42), 8);

    let mut queued = receiver
      .shared
      .queue
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(matches!(
      queued.pop(),
      Some(WindowEvent::Destroyed {
        window_id: WindowId(42),
        ..
      })
    ));
    assert!(matches!(
      queued.pop(),
      Some(WindowEvent::Shown { ref window, .. }) if window.id() == WindowId(42)
    ));
    assert_eq!(queued.len(), 0);
  }

  #[test]
  fn hundred_thousand_mixed_events_stay_within_fixed_capacity() {
    const LIMIT: usize = 64;
    let (sender, receiver) = native_window_event_channel();

    for index in 0..100_000_isize {
      let handle = (index % 257) + 1;
      let event = match index % 5 {
        0 => moved(handle, false, false),
        1 => title(handle),
        2 => hidden(handle),
        3 => shown(handle),
        _ => destroyed(handle),
      };
      sender.send_with_limit(event, LIMIT);
      let queued = receiver
        .shared
        .queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
      assert!(queued.len() <= LIMIT);
      assert!(queued.latest.len() <= LIMIT);
      assert!(queued.generations.len() <= LIMIT);
    }

    let mut queued = receiver
      .shared
      .queue
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(matches!(
      queued.pop(),
      Some(WindowEvent::ReconcileRequested { .. })
    ));
    while queued.pop().is_some() {
      assert!(queued.len() <= LIMIT);
    }
    assert_eq!(queued.len(), 0);
    assert!(queued.latest.is_empty());
    assert!(queued.generations.is_empty());
  }
}
