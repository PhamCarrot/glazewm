use tracing::info;
use wm_common::WindowState;
use wm_platform::NativeWindow;

use crate::{
  commands::{
    container::set_focused_descendant, window::update_window_state,
  },
  models::WindowContainer,
  traits::WindowGetters,
  user_config::UserConfig,
  wm_state::WmState,
};

fn should_apply_minimize_start(current_state: &WindowState) -> bool {
  current_state != &WindowState::Minimized
}

pub fn handle_window_minimized(
  native_window: &NativeWindow,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let found_window = state.window_from_native(native_window);

  // Update the window's state to be minimized.
  if let Some(window) = found_window {
    apply_window_minimized(window, state, config)?;
  }

  Ok(())
}

/// Applies an authoritative native minimize transition to a managed
/// window. Shared with the final workspace handoff, which can observe an
/// iconic HWND before its queued `MINIMIZESTART` event reaches this
/// handler.
fn apply_window_minimized(
  window: WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  window.update_native_properties(|properties| {
    properties.is_minimized = true;
  });

  // `WindowMinimized` is emitted from EVENT_SYSTEM_MINIMIZESTART and is
  // an ordered lifecycle barrier in the native queue. Treat the event
  // itself as authoritative: during a workspace handoff, a queued
  // platform placement can restore the covered HWND before this handler
  // runs, making a fresh `IsIconic` query false and losing the user's
  // minimize. A later `WindowMinimizeEnded` event remains the ordered
  // correction if the app immediately restores itself.
  if should_apply_minimize_start(&window.state()) {
    info!("Window minimized: {window}");

    let window = update_window_state(
      window.clone(),
      WindowState::Minimized,
      state,
      config,
    )?;

    // Clear the drag state, as a window can be minimized while
    // being dragged (e.g. via `toggle-minimized`).
    // TODO: Investigate other code paths where the drag state should be
    // cleared (e.g. most commands that call `update_window_state`).
    window.set_active_drag(None);

    // Focus should be reassigned after a window has been minimized.
    if let Some(focus_target) = state.focus_target_after_removal(&window) {
      set_focused_descendant(&focus_target, None);
      state.pending_sync.queue_focus_change().queue_cursor_jump();
      state.unmanaged_or_minimized_timestamp =
        Some(std::time::Instant::now());
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use wm_common::WindowState;

  use super::should_apply_minimize_start;

  #[test]
  fn minimize_start_remains_authoritative_during_deferred_handoff() {
    assert!(should_apply_minimize_start(&WindowState::Tiling));
    assert!(!should_apply_minimize_start(&WindowState::Minimized));
  }
}
