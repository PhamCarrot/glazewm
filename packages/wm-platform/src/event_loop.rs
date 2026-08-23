use crate::{platform_impl, Dispatcher};

/// A cross-platform event loop that allows for remote dispatching via
/// [`Dispatcher`].
///
/// Does not start pumping events until [`EventLoop::run`] is called.
///
/// This type is `!Send` to ensure it stays on the thread where it was
/// created, as the `Drop` implementation relies on thread-specific
/// cleanup.
///
/// # Platform-specific
///
/// - **macOS**: Can be created on any thread. Runs `CFRunLoopRun()`.
/// - **Windows**: Can be created on any thread. Runs a Win32 message loop.
pub struct EventLoop {
  inner: platform_impl::EventLoop,
  /// Marker to ensure not `Send`.
  _marker: std::marker::PhantomData<*const ()>,
}

impl EventLoop {
  /// Creates a new event loop and dispatcher.
  pub fn new() -> crate::Result<(Self, Dispatcher)> {
    let (event_loop, dispatcher) = platform_impl::EventLoop::new()?;
    Ok((
      Self {
        inner: event_loop,
        _marker: std::marker::PhantomData,
      },
      dispatcher,
    ))
  }

  /// Runs the event loop, blocking the current thread until shutdown.
  ///
  /// # Platform-specific
  ///
  /// - **macOS**: Must be called from the main thread. Runs
  ///   `CFRunLoopRun()`.
  /// - **Windows**: Can be called from any thread. Runs Win32 message
  ///   loop.
  pub fn run(self) -> crate::Result<()> {
    self.inner.run()
  }
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::*;

  #[test]
  fn event_loop_start_stop() {
    let (event_loop, dispatcher) =
      EventLoop::new().expect("Failed to create event loop.");

    // Stop the event loop after a short delay.
    let handle = std::thread::spawn(move || {
      std::thread::sleep(Duration::from_millis(10));
      dispatcher.stop_event_loop()
    });

    event_loop.run().expect("Failed to run event loop.");

    // Ensure the event loop is stopped.
    assert!(handle.join().unwrap().is_ok());
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn dropped_stopped_event_loop_does_not_stop_next_event_loop() {
    let (first_event_loop, first_dispatcher) =
      EventLoop::new().expect("Failed to create first event loop.");
    first_dispatcher
      .stop_event_loop()
      .expect("Failed to stop first event loop.");
    drop(first_event_loop);

    let (second_event_loop, second_dispatcher) =
      EventLoop::new().expect("Failed to create second event loop.");
    let handle = std::thread::spawn(move || {
      std::thread::sleep(Duration::from_millis(10));
      second_dispatcher.stop_event_loop()
    });

    second_event_loop
      .run()
      .expect("Failed to run second event loop.");
    assert!(handle.join().unwrap().is_ok());
  }
}
