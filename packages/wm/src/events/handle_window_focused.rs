use anyhow::Context;
use tracing::info;
use wm_common::{DisplayState, WindowRuleEvent, WmEvent};
use wm_platform::NativeWindow;
#[cfg(target_os = "windows")]
use wm_platform::{MouseEvent, PressedButtons};

#[cfg(target_os = "windows")]
use crate::commands::general::commit_presented_focus_effect_handoff;
use crate::{
  commands::{
    container::set_focused_descendant, window::run_window_rules,
    workspace::focus_workspace,
  },
  models::WorkspaceTarget,
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub fn handle_window_focused(
  native_window: &NativeWindow,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  let found_window = state.window_from_native(native_window);
  let focused_container =
    state.focused_container().context("No focused container.")?;

  // Update the focus sync state. If the OS focused window is not same as
  // the WM's focused container, then the focus is not synced.
  state.is_focus_synced = match focused_container.as_window_container() {
    Ok(window) => *window.native() == *native_window,
    _ => native_window.is_desktop_window().unwrap_or(false),
  };

  // Handle overriding focus on close/minimize. After a window is closed
  // or minimized, the OS or the closed application might automatically
  // switch focus to a different window. To force focus to go to the WM's
  // target focus container, we reassign any focus events 100ms after
  // close/minimize. This will cause focus to briefly flicker to the OS
  // focus target and then to the WM's focus target.
  if should_override_focus(state) {
    state.pending_sync.queue_focus_change();
    return Ok(());
  }

  // Do not adopt focus from a window which the WM is in the process of
  // hiding. If native focus actually diverged from the logical target,
  // queue restoration instead of leaving `is_focus_synced` false
  // indefinitely (which would also suppress focus-follows-cursor).
  // `sync_focus` safely defers the native call while the incoming target
  // has a live surrogate, and animation cleanup queues it again at the
  // handoff.
  if let Some(window) = &found_window {
    if window.display_state() == DisplayState::Hiding {
      if should_restore_focus_for_hiding_window(
        &window.display_state(),
        state.is_focus_synced,
      ) {
        state.pending_sync.queue_focus_change();
      }
      return Ok(());
    }
  }

  if let Some(window) = found_window {
    let workspace = window.workspace().context("No workspace")?;

    // Native focus has been synced to the WM's focused container.
    if focused_container == window.clone().into() {
      state.is_focus_synced = true;
      state.pending_sync.queue_workspace_to_reorder(workspace);
      #[cfg(target_os = "windows")]
      if matches_presented_focus_effect_handoff(
        &state.pending_presented_focus_effect_handoff,
        window.id(),
      ) {
        commit_presented_focus_effect_handoff(&window, state, config)?;
        state.pending_presented_focus_effect_handoff = None;
      } else {
        state.pending_sync.queue_focused_effect_update();
      }
      #[cfg(not(target_os = "windows"))]
      state.pending_sync.queue_focused_effect_update();
      #[cfg(target_os = "windows")]
      refresh_cursor_after_confirmed_switch_focus(
        window.id(),
        state,
        config,
      )?;
      return Ok(());
    }

    info!("Window manually focused: {window}");
    state.pending_sync.queue_focused_effect_update();

    // Handle focus events from windows on hidden workspaces. For example,
    // if Discord is forcefully shown by the OS when it's on a hidden
    // workspace, switch focus to Discord's workspace.
    if window.display_state() == DisplayState::Hidden
      && !workspace.is_displayed()
    {
      info!("Focusing off-screen window: {window}");

      focus_workspace(
        WorkspaceTarget::Name(workspace.config().name),
        state,
        config,
      )?;
    }

    // Update the WM's focus state.
    set_focused_descendant(&window.clone().into(), None);

    // If the focused window is inside a stack, queue all children for
    // redraw so the newly-active child is uncloaked and siblings are
    // cloaked. This handles OS-driven focus (e.g. taskbar click on a
    // cloaked stack window after a lock/unlock cycle).
    if let Some(parent) = window.parent() {
      if let Some(stack) = parent.as_stack() {
        for child in stack.tiling_children() {
          state.pending_sync.queue_container_to_redraw(child);
        }
      }
    }

    // Run window rules for focus events.
    run_window_rules(
      window.clone(),
      &WindowRuleEvent::Focus,
      state,
      config,
    )?;

    state.is_focus_synced = true;
    state.pending_sync.queue_workspace_to_reorder(workspace);

    // Broadcast the focus change event.
    state.emit_event(WmEvent::FocusChanged {
      focused_container: window.to_dto()?,
    });

    #[cfg(target_os = "windows")]
    refresh_cursor_after_confirmed_switch_focus(
      window.id(),
      state,
      config,
    )?;
  } else {
    // Ignored windows and the desktop still update the focused effect
    // state.
    state.pending_sync.queue_focused_effect_update();

    if state.is_focus_synced {
      // Empty workspaces synchronize native focus to the desktop window.
      #[cfg(target_os = "windows")]
      refresh_cursor_after_confirmed_switch_focus(
        focused_container.id(),
        state,
        config,
      )?;
    }
  }

  Ok(())
}

/// Returns true if focus should be reassigned to the WM's focus container.
fn should_override_focus(state: &WmState) -> bool {
  let has_recent_unmanage = state
    .unmanaged_or_minimized_timestamp
    .is_some_and(|time| time.elapsed().as_millis() < 100);

  has_recent_unmanage && !state.is_focus_synced
}

fn should_restore_focus_for_hiding_window(
  display_state: &DisplayState,
  is_focus_synced: bool,
) -> bool {
  *display_state == DisplayState::Hiding && !is_focus_synced
}

#[cfg(target_os = "windows")]
fn take_matching_cursor_refresh(
  pending_target: &mut Option<uuid::Uuid>,
  confirmed_target: uuid::Uuid,
  focus_follows_cursor: bool,
) -> bool {
  pending_target.take().is_some_and(|target| {
    focus_follows_cursor && target == confirmed_target
  })
}

#[cfg(target_os = "windows")]
fn matches_presented_focus_effect_handoff(
  pending_target: &Option<uuid::Uuid>,
  confirmed_target: uuid::Uuid,
) -> bool {
  *pending_target == Some(confirmed_target)
}

/// Re-evaluates focus-follows-cursor once after native focus confirms a
/// workspace switch target.
///
/// Failure to query the cursor is cosmetic: the pending marker is consumed
/// and the next real mouse move follows the normal path.
#[cfg(target_os = "windows")]
fn refresh_cursor_after_confirmed_switch_focus(
  confirmed_target: uuid::Uuid,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  if !take_matching_cursor_refresh(
    &mut state.pending_post_switch_cursor_refresh,
    confirmed_target,
    config.value.general.focus_follows_cursor,
  ) {
    return Ok(());
  }

  let position = match state.dispatcher.cursor_position() {
    Ok(position) => position,
    Err(err) => {
      tracing::warn!(
        "Failed to refresh focus-follows-cursor after workspace switch: {err}."
      );
      return Ok(());
    }
  };

  super::handle_mouse_move(
    &MouseEvent::Move {
      position,
      pressed_buttons: PressedButtons::default(),
      window_below_cursor: None,
    },
    state,
    config,
  )
}

#[cfg(test)]
mod tests {
  #[cfg(target_os = "windows")]
  use uuid::Uuid;
  use wm_common::DisplayState;

  #[cfg(target_os = "windows")]
  use super::matches_presented_focus_effect_handoff;
  use super::should_restore_focus_for_hiding_window;
  #[cfg(target_os = "windows")]
  use super::take_matching_cursor_refresh;

  #[test]
  fn mismatched_focus_on_hiding_window_is_retried() {
    assert!(should_restore_focus_for_hiding_window(
      &DisplayState::Hiding,
      false,
    ));
    assert!(!should_restore_focus_for_hiding_window(
      &DisplayState::Hiding,
      true,
    ));
    assert!(!should_restore_focus_for_hiding_window(
      &DisplayState::Hidden,
      false,
    ));
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn cursor_refresh_requires_matching_confirmed_native_focus() {
    let expected = Uuid::new_v4();
    let other = Uuid::new_v4();

    let mut pending = Some(expected);
    assert!(!take_matching_cursor_refresh(&mut pending, other, true));
    assert_eq!(pending, None);

    let mut pending = Some(expected);
    assert!(!take_matching_cursor_refresh(&mut pending, expected, false));
    assert_eq!(pending, None);

    let mut pending = Some(expected);
    assert!(take_matching_cursor_refresh(&mut pending, expected, true));
    assert_eq!(pending, None);
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn presented_focus_effect_handoff_waits_for_matching_native_focus() {
    let expected = Uuid::new_v4();
    let other = Uuid::new_v4();

    let mut pending = Some(expected);
    assert!(!matches_presented_focus_effect_handoff(&pending, other,));
    assert_eq!(pending, Some(expected));

    assert!(matches_presented_focus_effect_handoff(&pending, expected,));
    pending.take();
    assert_eq!(pending, None);
  }
}
