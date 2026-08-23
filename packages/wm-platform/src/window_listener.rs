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
struct NativeEventQueueShared {
  events: Mutex<VecDeque<WindowEvent>>,
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
    let mut events = self
      .shared
      .events
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);

    if let Some(key) = WindowListener::coalescible_key(&event) {
      if events
        .back()
        .and_then(WindowListener::coalescible_key)
        .is_some_and(|previous| previous == key)
      {
        *events
          .back_mut()
          .expect("native event queue unexpectedly empty") = event;
        return;
      }

      if events.len() >= limit {
        return;
      }
    } else if events.len() >= limit {
      if let Some(index) = events.iter().position(|queued| {
        WindowListener::coalescible_key(queued).is_some()
      }) {
        events.remove(index);
      }
      // When the queue contains lifecycle events only, allow it to exceed
      // the soft limit rather than losing a show/hide/focus/destroy event.
    }

    events.push_back(event);
    drop(events);
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
        .events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pop_front()
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
    events: Mutex::new(VecDeque::new()),
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
      .events
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(queued.len(), 2);
    assert!(queued
      .iter()
      .all(|event| matches!(event, WindowEvent::MovedOrResized { .. })));
    drop(queued);

    sender.send_with_limit(hidden(3), 2);
    let queued = receiver
      .shared
      .events
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(queued.len(), 2);
    assert!(matches!(queued.back(), Some(WindowEvent::Hidden { .. })));
  }

  #[test]
  fn native_queue_never_drops_lifecycle_only_pressure() {
    let (sender, receiver) = native_window_event_channel();
    sender.send_with_limit(hidden(1), 1);
    sender.send_with_limit(hidden(2), 1);

    let queued = receiver
      .shared
      .events
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(queued.len(), 2);
  }
}
