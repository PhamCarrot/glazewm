use tracing::{info, warn};
use wm_common::WindowState;

use crate::{
  events::{
    handle_window_focused, handle_window_hidden,
    handle_window_minimize_ended, handle_window_minimized,
    handle_window_shown,
  },
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Reconciles current native state after the fixed-capacity WinEvent queue
/// had to discard history under extreme lifecycle pressure.
///
/// Every operation is isolated. One inaccessible or destroyed HWND cannot
/// prevent the remaining windows from being checked, and the final root
/// redraw re-establishes layout/visibility ownership through the normal
/// sync path.
pub fn handle_window_reconcile_requested(
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  warn!(
    "Native window event capacity was exceeded; reconciling current HWND state."
  );

  if let Err(err) = state.cleanup_invalid_windows() {
    warn!("Failed to remove one or more invalid windows during event reconciliation: {err:#}");
  }

  let dispatcher = state.dispatcher.clone();
  match dispatcher.visible_windows() {
    Ok(native_windows) => {
      for native_window in native_windows {
        if let Err(err) =
          handle_window_shown(native_window, state, config)
        {
          warn!(
            "Failed to reconcile one visible native window after queue overflow: {err:#}"
          );
        }
      }
    }
    Err(err) => warn!(
      "Failed to enumerate visible windows during event reconciliation: {err:#}"
    ),
  }

  // Reconcile lifecycle state for already-managed windows. Clone the list
  // so an isolated unmanage/state transition can safely mutate the tree.
  for window in state.windows() {
    if state.container_by_id(window.id()).is_none() {
      continue;
    }

    let native_window = window.native().clone();
    match native_window.is_minimized() {
      Ok(true) if window.state() != WindowState::Minimized => {
        if let Err(err) =
          handle_window_minimized(&native_window, state, config)
        {
          warn!(
            "Failed to reconcile minimized window {}: {err:#}",
            window.id()
          );
        }
      }
      Ok(false) if window.state() == WindowState::Minimized => {
        if let Err(err) =
          handle_window_minimize_ended(&native_window, state, config)
        {
          warn!(
            "Failed to reconcile restored window {}: {err:#}",
            window.id()
          );
        }
      }
      Ok(_) => {}
      Err(err) => warn!(
        "Failed to query minimized state for window {} during reconciliation: {err:#}",
        window.id()
      ),
    }

    let belongs_to_displayed_workspace = window
      .workspace()
      .is_some_and(|workspace| workspace.is_displayed());
    if belongs_to_displayed_workspace
      && native_window.is_visible().is_ok_and(|visible| !visible)
    {
      if let Err(err) = handle_window_hidden(&native_window, state, config)
      {
        warn!(
          "Failed to reconcile hidden window {}: {err:#}",
          window.id()
        );
      }
    }
  }

  if let Ok(focused_window) = dispatcher.focused_window() {
    if let Err(err) = handle_window_focused(&focused_window, state, config)
    {
      warn!(
        "Failed to reconcile native focus after queue overflow: {err:#}"
      );
    }
  }

  state
    .pending_sync
    .queue_container_to_redraw(state.root_container.clone())
    .queue_focus_change()
    .queue_all_effects_update();
  info!(
    "Queued one complete native-state redraw after event reconciliation."
  );

  Ok(())
}
