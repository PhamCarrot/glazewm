use anyhow::Context;
use wm_common::{DisplayState, WindowState, WmEvent};
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;

use crate::{
  commands::container::{
    attach_container, detach_container, flatten_child_split_containers,
    set_focused_descendant,
  },
  models::WindowContainer,
  traits::{CommonGetters, WindowGetters},
  wm_state::WmState,
};

/// Restores a still-live window after final close-message delivery fails.
///
/// Native visibility is recovered before attempting tree attachment. If
/// the layout recovery fails, the application therefore remains visible
/// and usable as an unmanaged window rather than being stranded cloaked.
#[cfg(target_os = "windows")]
pub fn restore_window_after_failed_close(
  window: WindowContainer,
  preferred_workspace_id: uuid::Uuid,
  state: &mut WmState,
) -> anyhow::Result<()> {
  {
    let native = window.native();
    if let Err(err) = native.set_cloaked(false) {
      tracing::warn!(
        "Failed to uncloak window {} after close delivery failure: {err}.",
        window.id()
      );
    }
    if let Err(err) = native.show() {
      tracing::warn!(
        "Failed to show window {} after close delivery failure: {err}.",
        window.id()
      );
    }
  }
  window.set_display_state(DisplayState::Shown);

  let target_workspace = state
    .container_by_id(preferred_workspace_id)
    .filter(|container| container.as_workspace().is_some())
    .or_else(|| {
      state
        .focused_container()
        .and_then(|container| container.workspace())
        .map(Into::into)
    })
    .context("No workspace available to restore failed close.")?;

  attach_container(&window.clone().into(), &target_workspace, None)?;
  set_focused_descendant(&window.clone().into(), None);
  state.window_target_positions.remove(&window.id());
  state.pending_sync.queue_focus_change();
  state
    .pending_sync
    .queue_container_to_redraw(target_workspace);
  state.emit_event(WmEvent::WindowManaged {
    managed_window: window.to_dto()?,
  });

  Ok(())
}

/// Detaches a closing window from the container tree and reflows its
/// siblings without removing the active close animation.
///
/// Called at the start of a close animation so sibling windows begin their
/// reflow animations immediately, in parallel with the close surrogate.
/// The animation state is intentionally preserved here because
/// `AnimationManager::update_internal` continues driving the surrogate and
/// sends `WM_CLOSE` once the animation completes.
#[cfg(target_os = "windows")]
pub fn detach_window_for_close(
  window: WindowContainer,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let ancestors = window.ancestors().take(3).collect::<Vec<_>>();
  let focus_target = state.focus_target_after_removal(&window.clone());

  detach_container(window.clone().into())?;

  state.window_target_positions.remove(&window.id());
  // NOTE: `state.animation_manager.remove_animation` is intentionally
  // skipped — the close surrogate must keep running until
  // `AnimationManager::update_internal` sends `WM_CLOSE` on completion.

  for ancestor in ancestors.iter().rev() {
    flatten_child_split_containers(ancestor)?;
  }

  state.emit_event(WmEvent::WindowUnmanaged {
    unmanaged_id: window.id(),
    #[allow(clippy::cast_possible_wrap, clippy::unnecessary_cast)]
    unmanaged_handle: window.native().id().0 as isize,
  });

  if let Some(focus_target) = focus_target {
    set_focused_descendant(&focus_target, None);
    state.pending_sync.queue_focus_change();
    state.unmanaged_or_minimized_timestamp =
      Some(std::time::Instant::now());
  }

  if window.state() == WindowState::Tiling {
    let ancestor_to_redraw = ancestors
      .into_iter()
      .find(|ancestor| !ancestor.is_detached())
      .context("No ancestor to redraw.")?;

    state
      .pending_sync
      .queue_containers_to_redraw(ancestor_to_redraw.tiling_children());
  }

  Ok(())
}

#[allow(clippy::needless_pass_by_value)]
pub fn unmanage_window(
  window: WindowContainer,
  state: &mut WmState,
) -> anyhow::Result<()> {
  // Create iterator of parent, grandparent, and great-grandparent.
  let ancestors = window.ancestors().take(3).collect::<Vec<_>>();

  // Get container to switch focus to after the window has been removed.
  let focus_target = state.focus_target_after_removal(&window.clone());

  detach_container(window.clone().into())?;

  // Clean up animation tracking data.
  state.window_target_positions.remove(&window.id());
  state.animation_manager.remove_animation(&window.id());

  // After detaching the container, flatten any redundant split containers.
  // For example, in the layout V[1 H[2]] where container 1 is detached to
  // become V[H[2]], this will then need to be flattened to V[2].
  for ancestor in ancestors.iter().rev() {
    flatten_child_split_containers(ancestor)?;
  }

  state.emit_event(WmEvent::WindowUnmanaged {
    unmanaged_id: window.id(),
    #[allow(clippy::cast_possible_wrap, clippy::unnecessary_cast)]
    unmanaged_handle: window.native().id().0 as isize,
  });

  // Reassign focus to suitable target.
  if let Some(focus_target) = focus_target {
    set_focused_descendant(&focus_target, None);
    state.pending_sync.queue_focus_change();
    state.unmanaged_or_minimized_timestamp =
      Some(std::time::Instant::now());
  }

  // Sibling containers need to be redrawn if the window was tiling.
  if window.state() == WindowState::Tiling {
    let ancestor_to_redraw = ancestors
      .into_iter()
      .find(|ancestor| !ancestor.is_detached())
      .context("No ancestor to redraw.")?;

    state
      .pending_sync
      .queue_containers_to_redraw(ancestor_to_redraw.tiling_children());
  }

  Ok(())
}
