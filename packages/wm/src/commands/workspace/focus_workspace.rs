#[cfg(target_os = "windows")]
use std::collections::HashMap;

use anyhow::Context;
use tracing::info;
use wm_common::{WindowState, WmEvent, WorkspaceSwitchIrisOrigin};

use super::activate_workspace;
use crate::{
  commands::{
    container::set_focused_descendant, workspace::deactivate_workspace,
  },
  models::{Container, WorkspaceTarget},
  pending_sync::IrisSwitchRequest,
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::{PendingWorkspaceRedirect, WmState},
};

/// Focuses a workspace by a given target.
///
/// This target can be a workspace name, the most recently focused
/// workspace, the next workspace, the previous workspace, or the workspace
/// in a given direction from the currently focused workspace.
///
/// The workspace will be activated if it isn't already active.
pub fn focus_workspace(
  mut target: WorkspaceTarget,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let focused_workspace = state
    .focused_container()
    .and_then(|focused| focused.workspace())
    .context("No workspace is currently focused.")?;

  #[cfg(target_os = "windows")]
  if matches!(
    &target,
    WorkspaceTarget::Name(name)
      if state.pending_workspace_redirect.as_ref().is_some_and(|pending| {
        pending.destination_name == name.as_str()
      })
  ) {
    return Ok(());
  }

  #[cfg(target_os = "windows")]
  if let Some(pending) = &state.pending_workspace_redirect {
    target = resolve_pending_relative_target(
      target,
      &pending.destination_name,
      config,
    );
  }

  #[cfg(target_os = "windows")]
  let pending_origin =
    state
      .pending_workspace_redirect
      .as_ref()
      .and_then(|pending| {
        state.workspace_by_name(&pending.destination_name)
      });
  #[cfg(target_os = "windows")]
  let resolution_origin = if let Some(origin) = pending_origin {
    origin
  } else {
    if state.pending_workspace_redirect.take().is_some() {
      tracing::warn!(
        "Discarding stale pending workspace redirect before target resolution."
      );
    }
    focused_workspace.clone()
  };
  #[cfg(target_os = "windows")]
  let (target_workspace_name, target_workspace) =
    state.workspace_by_target(&resolution_origin, target, config)?;
  #[cfg(not(target_os = "windows"))]
  let (target_workspace_name, target_workspace) =
    state.workspace_by_target(&focused_workspace, target, config)?;

  #[cfg(target_os = "windows")]
  if state
    .pending_workspace_redirect
    .as_ref()
    .is_some_and(|pending| {
      target_workspace_name.as_deref() == Some(&pending.destination_name)
        || target_workspace.as_ref().is_some_and(|workspace| {
          workspace.config().name == pending.destination_name
        })
    })
  {
    return Ok(());
  }

  // Retrieve or activate the target workspace by its name.
  let target_workspace = match target_workspace {
    Some(_) => anyhow::Ok(target_workspace),
    _ => match target_workspace_name {
      Some(name) => {
        activate_workspace(Some(&name), None, state, config)?;

        Ok(state.workspace_by_name(&name))
      }
      _ => Ok(None),
    },
  }?;

  if let Some(target_workspace) = target_workspace {
    info!("Focusing workspace: {target_workspace}");

    // Get the currently displayed workspace on the same monitor that the
    // workspace to focus is on.
    let displayed_workspace = target_workspace
      .monitor()
      .and_then(|monitor| monitor.displayed_workspace())
      .context("No workspace is currently displayed.")?;

    // Set focus to whichever window last had focus in workspace. If the
    // workspace has no windows, then set focus to the workspace itself.
    let container_to_focus = target_workspace
      .descendant_focus_order()
      .next()
      .unwrap_or_else(|| target_workspace.clone().into());

    #[cfg(target_os = "windows")]
    {
      let ws_anim = &config.value.animations.workspace_switch;
      let pending_base_name = state
        .pending_workspace_redirect
        .as_ref()
        .map(|pending| pending.destination_name.clone())
        .unwrap_or_else(|| focused_workspace.config().name);
      let direction = workspace_switch_direction(
        &target_workspace.config().name,
        &pending_base_name,
        config,
      );
      let requested_roles =
        workspace_switch_roles(&target_workspace, &displayed_workspace);
      let exact_reversal =
        state.animation_manager.can_reverse_workspace_switch(
          &requested_roles,
          direction,
          ws_anim,
        );
      let has_pending_redirect =
        state.pending_workspace_redirect.is_some();
      let incompatible_active_slide = (target_workspace.id()
        != displayed_workspace.id()
        || has_pending_redirect)
        && state.animation_manager.is_workspace_switch_active()
        && ws_anim.should_animate()
        && ws_anim.style == wm_common::WorkspaceSwitchStyle::Slide
        && !state.pending_sync.animations_suppressed()
        && !exact_reversal;

      if incompatible_active_slide {
        let previous_underlay_name = redirect_underlay_name(
          state.pending_workspace_redirect.as_ref(),
          &displayed_workspace.config().name,
        );
        let previous_underlay = state
          .workspace_by_name(&previous_underlay_name)
          .unwrap_or_else(|| displayed_workspace.clone());
        let outcome = state
          .animation_manager
          .begin_incompatible_workspace_redirect(direction, ws_anim);
        stage_workspace_redirect_underlay(
          &target_workspace,
          &previous_underlay,
          &focused_workspace,
          direction,
          state,
        );
        tracing::debug!(
          ?outcome,
          destination = %target_workspace.config().name,
          "Accepted one bounded incompatible workspace underlay without committing logical focus."
        );
        return Ok(());
      }
    }

    #[cfg(target_os = "windows")]
    if state.pending_presented_focus_effect_handoff
      != Some(container_to_focus.id())
    {
      // A different logical target invalidates an unconfirmed handoff from
      // a prior rapid workspace switch. Re-focusing the same pending
      // target keeps its marker until native focus confirms it.
      state.pending_presented_focus_effect_handoff = None;
    }

    set_focused_descendant(&container_to_focus, None);
    state.pending_sync.queue_focus_change();

    // Focusing changes the previous workspace's `has_focus` and, for a
    // same-monitor switch, its `is_displayed` value as well. Publish its
    // post-change DTO before the target so stateful IPC clients cannot
    // retain two focused/displayed workspaces while the visual
    // transition runs.
    if focused_workspace.id() != target_workspace.id() {
      state.emit_event(WmEvent::WorkspaceUpdated {
        updated_workspace: focused_workspace.to_dto()?,
      });
    }

    // Workspace IPC represents the current logical tree. Notify clients as
    // soon as focus order changes; the surrogate animation may continue
    // presenting the outgoing workspace visually until handoff.
    state.emit_event(WmEvent::WorkspaceUpdated {
      updated_workspace: target_workspace.to_dto()?,
    });

    // Only set up the workspace-switch slide animation when the target
    // workspace differs from what is currently displayed on its monitor.
    // For cross-monitor focus switches (e.g. focusing workspace 8 which is
    // already displayed on monitor 2), `target == displayed`, so no
    // workspace layout change occurs on the target monitor and no slide
    // should play.
    if target_workspace.id() != displayed_workspace.id() {
      #[cfg(target_os = "windows")]
      if config.value.general.focus_follows_cursor {
        state.pending_post_switch_cursor_refresh =
          Some(container_to_focus.id());
      }

      let ws_anim = &config.value.animations.workspace_switch;

      if ws_anim.should_animate() {
        if ws_anim.style.is_iris() {
          // Iris wipe: request a snapshot-overlay wipe and let the real
          // windows switch instantly underneath. No per-window
          // incoming/outgoing marking is done — the overlay (not
          // surrogates) drives all the visuals.
          if let Some(monitor) = target_workspace.monitor() {
            let props = monitor.native_properties();
            let bounds = &props.bounds;
            let center = (
              bounds.x() + bounds.width() / 2,
              bounds.y() + bounds.height() / 2,
            );
            let (origin_x, origin_y) = iris_origin_point(
              &ws_anim.iris_origin,
              bounds,
              center,
              &container_to_focus,
            );
            state.pending_sync.request_iris_switch(IrisSwitchRequest {
              monitor_x: bounds.x(),
              monitor_y: bounds.y(),
              monitor_width: bounds.width(),
              monitor_height: bounds.height(),
              monitor_handle: props.handle,
              origin_x,
              origin_y,
            });
          }
        } else {
          prepare_surrogate_workspace_switch(
            &target_workspace,
            &displayed_workspace,
            &focused_workspace,
            state,
            config,
          );
        }
      } else {
        // An instant switch must still cancel any movement surrogate that
        // belongs to an outgoing window; it must not create workspace
        // surrogates or readiness state of its own.
        for window in displayed_workspace
          .descendants()
          .filter_map(|c| c.as_window_container().ok())
        {
          state.animation_manager.remove_animation(&window.id());
        }
      }
    }

    // Display the workspace to switch focus to.
    state
      .pending_sync
      .queue_container_to_redraw(displayed_workspace)
      .queue_container_to_redraw(target_workspace);

    // Get empty workspace to destroy (if one is found). Cannot destroy
    // empty workspaces if they're the only workspace on the monitor.
    let workspace_to_destroy =
      state.workspaces().into_iter().find(|workspace| {
        !workspace.config().keep_alive
          && !workspace.has_children()
          && !workspace.is_displayed()
      });

    if let Some(workspace) = workspace_to_destroy {
      deactivate_workspace(workspace, state)?;
    }

    // Save the currently focused workspace as recent.
    state.recent_workspace_name = Some(focused_workspace.config().name);
    state.pending_sync.queue_cursor_jump();
  }

  Ok(())
}

#[cfg(target_os = "windows")]
fn workspace_switch_roles(
  target_workspace: &crate::models::Workspace,
  displayed_workspace: &crate::models::Workspace,
) -> HashMap<uuid::Uuid, bool> {
  target_workspace
    .descendants()
    .filter_map(|container| container.as_window_container().ok())
    .filter(|window| window.state() != WindowState::Minimized)
    .map(|window| (window.id(), true))
    .chain(
      displayed_workspace
        .descendants()
        .filter_map(|container| container.as_window_container().ok())
        .filter(|window| window.state() != WindowState::Minimized)
        .map(|window| (window.id(), false)),
    )
    .collect()
}

#[cfg(target_os = "windows")]
fn stage_workspace_redirect_underlay(
  target_workspace: &crate::models::Workspace,
  previous_underlay: &crate::models::Workspace,
  focused_workspace: &crate::models::Workspace,
  direction: i32,
  state: &mut WmState,
) {
  let public_origin_name = state
    .pending_workspace_redirect
    .as_ref()
    .map(|pending| pending.public_origin_name.clone())
    .unwrap_or_else(|| focused_workspace.config().name);

  state
    .pending_sync
    .clear_workspace_switch_participants()
    .set_workspace_switch_direction(direction);
  for window in target_workspace
    .descendants()
    .filter_map(|container| container.as_window_container().ok())
    .filter(|window| window.state() != WindowState::Minimized)
  {
    state
      .pending_sync
      .setup_workspace_switch_incoming(window.id());
  }
  for window in previous_underlay
    .descendants()
    .filter_map(|container| container.as_window_container().ok())
    .filter(|window| window.state() != WindowState::Minimized)
  {
    state.animation_manager.remove_animation(&window.id());
    state
      .pending_sync
      .setup_workspace_switch_outgoing(window.id());
  }

  state.pending_workspace_redirect = Some(replace_pending_redirect(
    state.pending_workspace_redirect.as_ref(),
    public_origin_name,
    target_workspace.config().name,
    direction,
  ));
  state.pending_presented_focus_effect_handoff = None;
  state
    .pending_sync
    .queue_container_to_redraw(previous_underlay.clone())
    .queue_container_to_redraw(target_workspace.clone());
}

#[cfg(target_os = "windows")]
fn replace_pending_redirect(
  existing: Option<&PendingWorkspaceRedirect>,
  public_origin_name: String,
  destination_name: String,
  order_direction: i32,
) -> PendingWorkspaceRedirect {
  PendingWorkspaceRedirect {
    public_origin_name: existing
      .map(|pending| pending.public_origin_name.clone())
      .unwrap_or(public_origin_name),
    destination_name,
    order_direction,
  }
}

#[cfg(target_os = "windows")]
fn redirect_underlay_name(
  pending: Option<&PendingWorkspaceRedirect>,
  displayed_name: &str,
) -> String {
  pending
    .map(|pending| pending.destination_name.clone())
    .unwrap_or_else(|| displayed_name.to_string())
}

/// Resolves config-ordered relative commands from the latest intended
/// destination before ordinary workspace lookup/activation. This is the
/// bounded virtual-base rule used by held next/previous input.
#[cfg(target_os = "windows")]
fn resolve_pending_relative_target(
  target: WorkspaceTarget,
  origin_name: &str,
  config: &UserConfig,
) -> WorkspaceTarget {
  let names: Vec<&str> = config
    .value
    .workspaces
    .iter()
    .map(|workspace| workspace.name.as_str())
    .collect();
  resolve_pending_relative_target_from_names(target, origin_name, &names)
}

#[cfg(target_os = "windows")]
fn resolve_pending_relative_target_from_names(
  target: WorkspaceTarget,
  origin_name: &str,
  names: &[&str],
) -> WorkspaceTarget {
  let step = match target {
    WorkspaceTarget::Next => 1_isize,
    WorkspaceTarget::Previous => -1_isize,
    other => return other,
  };
  let Some(origin_index) =
    names.iter().position(|name| *name == origin_name)
  else {
    return if step > 0 {
      WorkspaceTarget::Next
    } else {
      WorkspaceTarget::Previous
    };
  };
  let destination_index = if step > 0 {
    (origin_index + 1) % names.len()
  } else {
    origin_index.checked_sub(1).unwrap_or(names.len() - 1)
  };
  WorkspaceTarget::Name(names[destination_index].to_string())
}

/// Commits a ready directional-redirect underlay through the same logical
/// focus, recent-workspace, cursor, effect, and IPC order as an ordinary
/// workspace focus. Native focus remains deferred until retained coverage
/// has finished revealing the real destination.
#[cfg(target_os = "windows")]
pub(crate) fn commit_pending_workspace_redirect(
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<bool> {
  let Some(pending) = state.pending_workspace_redirect.clone() else {
    return Ok(false);
  };
  let target_workspace = state
    .workspace_by_name(&pending.destination_name)
    .with_context(|| {
      format!(
        "Pending workspace redirect target '{}' is no longer active.",
        pending.destination_name
      )
    })?;
  let public_origin = state
    .workspace_by_name(&pending.public_origin_name)
    .or_else(|| {
      state
        .focused_container()
        .and_then(|container| container.workspace())
    })
    .context(
      "Pending workspace redirect has no public origin workspace.",
    )?;
  let container_to_focus = target_workspace
    .descendant_focus_order()
    .next()
    .unwrap_or_else(|| target_workspace.clone().into());

  if public_origin.id() == target_workspace.id() {
    state.pending_workspace_redirect = None;
    let workspace_to_destroy =
      state.workspaces().into_iter().find(|workspace| {
        !workspace.config().keep_alive
          && !workspace.has_children()
          && !workspace.is_displayed()
      });
    if let Some(workspace) = workspace_to_destroy {
      deactivate_workspace(workspace, state)?;
    }
    tracing::debug!(
      destination = %target_workspace.config().name,
      "Committed ready workspace redirect cancellation without changing logical focus or IPC."
    );
    return Ok(true);
  }

  state.pending_presented_focus_effect_handoff = container_to_focus
    .as_window_container()
    .ok()
    .map(|window| window.id());
  set_focused_descendant(&container_to_focus, None);
  state.pending_sync.queue_focus_change();
  if public_origin.id() != target_workspace.id() {
    state.emit_event(WmEvent::WorkspaceUpdated {
      updated_workspace: public_origin.to_dto()?,
    });
  }
  state.emit_event(WmEvent::WorkspaceUpdated {
    updated_workspace: target_workspace.to_dto()?,
  });
  if config.value.general.focus_follows_cursor {
    state.pending_post_switch_cursor_refresh =
      Some(container_to_focus.id());
  }
  state.recent_workspace_name = Some(public_origin.config().name);
  state.pending_sync.queue_cursor_jump();
  state.pending_workspace_redirect = None;

  let workspace_to_destroy =
    state.workspaces().into_iter().find(|workspace| {
      !workspace.config().keep_alive
        && !workspace.has_children()
        && !workspace.is_displayed()
    });
  if let Some(workspace) = workspace_to_destroy {
    deactivate_workspace(workspace, state)?;
  }
  tracing::debug!(
    destination = %target_workspace.config().name,
    "Committed ready workspace redirect underlay to logical focus and IPC."
  );
  Ok(true)
}

fn prepare_surrogate_workspace_switch(
  target_workspace: &crate::models::Workspace,
  displayed_workspace: &crate::models::Workspace,
  focused_workspace: &crate::models::Workspace,
  state: &mut WmState,
  config: &UserConfig,
) {
  // Derive slide direction before queuing redraws so `platform_sync` can
  // build per-window surrogates with the correct direction.
  let direction = workspace_switch_direction(
    &target_workspace.config().name,
    &focused_workspace.config().name,
    config,
  );
  state.pending_sync.set_workspace_switch_direction(direction);

  // Minimized windows have no visible content to animate and including
  // them causes flicker when the animation system tries to snapshot
  // them.
  for window in target_workspace
    .descendants()
    .filter_map(|c| c.as_window_container().ok())
    .filter(|w| w.state() != WindowState::Minimized)
  {
    state
      .pending_sync
      .setup_workspace_switch_incoming(window.id());
  }

  for window in displayed_workspace
    .descendants()
    .filter_map(|c| c.as_window_container().ok())
    .filter(|w| w.state() != WindowState::Minimized)
  {
    state.animation_manager.remove_animation(&window.id());
    state
      .pending_sync
      .setup_workspace_switch_outgoing(window.id());
  }
}

/// Returns `+1` if `target_name` has a higher config index than
/// `focused_name`, `-1` if lower, or `0` if undetermined.
///
/// Used to derive the slide direction for the workspace-switch animation.
fn workspace_switch_direction(
  target_name: &str,
  focused_name: &str,
  config: &UserConfig,
) -> i32 {
  let names: Vec<&str> = config
    .value
    .workspaces
    .iter()
    .map(|w| w.name.as_str())
    .collect();

  workspace_switch_direction_in_order(target_name, focused_name, &names)
}

fn workspace_switch_direction_in_order(
  target_name: &str,
  focused_name: &str,
  names: &[&str],
) -> i32 {
  let target_idx = names.iter().position(|n| *n == target_name);
  let focused_idx = names.iter().position(|n| *n == focused_name);

  match (focused_idx, target_idx) {
    (Some(fi), Some(ti)) if ti > fi => 1,
    (Some(fi), Some(ti)) if ti < fi => -1,
    _ => 0,
  }
}

/// Computes the iris-wipe circle origin (screen pixels) for the configured
/// mode.
///
/// Falls back to `center` (the monitor center) when the cursor is on
/// another monitor or the focused container is not a positioned window.
fn iris_origin_point(
  origin: &WorkspaceSwitchIrisOrigin,
  monitor_bounds: &wm_platform::Rect,
  center: (i32, i32),
  focused: &Container,
) -> (i32, i32) {
  let cursor = if matches!(origin, WorkspaceSwitchIrisOrigin::Cursor) {
    wm_platform::cursor_position()
  } else {
    None
  };
  let focused_rect =
    if matches!(origin, WorkspaceSwitchIrisOrigin::FocusedWindow) {
      focused
        .as_window_container()
        .ok()
        .and_then(|w| w.to_rect().ok())
    } else {
      None
    };

  iris_origin_from_inputs(
    origin,
    monitor_bounds,
    center,
    cursor,
    focused_rect.as_ref(),
  )
}

fn iris_origin_from_inputs(
  origin: &WorkspaceSwitchIrisOrigin,
  monitor_bounds: &wm_platform::Rect,
  center: (i32, i32),
  cursor: Option<(i32, i32)>,
  focused_rect: Option<&wm_platform::Rect>,
) -> (i32, i32) {
  match origin {
    WorkspaceSwitchIrisOrigin::Center => center,
    WorkspaceSwitchIrisOrigin::Cursor => cursor
      .filter(|&(x, y)| {
        x >= monitor_bounds.x()
          && x < monitor_bounds.x() + monitor_bounds.width()
          && y >= monitor_bounds.y()
          && y < monitor_bounds.y() + monitor_bounds.height()
      })
      .unwrap_or(center),
    WorkspaceSwitchIrisOrigin::FocusedWindow => focused_rect
      .map_or(center, |r| (r.x() + r.width() / 2, r.y() + r.height() / 2)),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn workspace_order_controls_slide_direction() {
    let names = ["1", "2", "3"];
    assert_eq!(workspace_switch_direction_in_order("3", "1", &names), 1);
    assert_eq!(workspace_switch_direction_in_order("1", "3", &names), -1);
    assert_eq!(workspace_switch_direction_in_order("2", "2", &names), 0);
    assert_eq!(
      workspace_switch_direction_in_order("missing", "1", &names),
      0
    );
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn latest_underlay_replaces_one_bounded_destination_and_keeps_origin() {
    let first =
      replace_pending_redirect(None, "3".to_string(), "2".to_string(), -1);
    let latest = replace_pending_redirect(
      Some(&first),
      "2".to_string(),
      "4".to_string(),
      1,
    );

    assert_eq!(latest.public_origin_name, "3");
    assert_eq!(latest.destination_name, "4");
    assert_eq!(latest.order_direction, 1);
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn repeated_destination_hides_previous_underlay_not_logical_workspace() {
    assert_eq!(redirect_underlay_name(None, "3"), "3");
    let pending = PendingWorkspaceRedirect {
      public_origin_name: "3".to_string(),
      destination_name: "2".to_string(),
      order_direction: -1,
    };
    assert_eq!(redirect_underlay_name(Some(&pending), "3"), "2");
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn held_next_and_previous_advance_from_latest_intended_destination() {
    let names = ["1", "2", "3", "4"];
    let mut intended = "1".to_string();
    for _ in 0..3 {
      let resolved = resolve_pending_relative_target_from_names(
        WorkspaceTarget::Next,
        &intended,
        &names,
      );
      let WorkspaceTarget::Name(name) = resolved else {
        panic!("next should resolve to one absolute destination");
      };
      intended = name;
    }
    assert_eq!(intended, "4");

    for _ in 0..2 {
      let resolved = resolve_pending_relative_target_from_names(
        WorkspaceTarget::Previous,
        &intended,
        &names,
      );
      let WorkspaceTarget::Name(name) = resolved else {
        panic!("previous should resolve to one absolute destination");
      };
      intended = name;
    }
    assert_eq!(intended, "2");
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn relative_virtual_base_wraps_and_missing_base_stays_unresolved() {
    let names = ["1", "2", "3", "4"];
    assert!(matches!(
      resolve_pending_relative_target_from_names(
        WorkspaceTarget::Next,
        "4",
        &names,
      ),
      WorkspaceTarget::Name(name) if name == "1"
    ));
    assert!(matches!(
      resolve_pending_relative_target_from_names(
        WorkspaceTarget::Previous,
        "1",
        &names,
      ),
      WorkspaceTarget::Name(name) if name == "4"
    ));
    assert!(matches!(
      resolve_pending_relative_target_from_names(
        WorkspaceTarget::Next,
        "removed",
        &names,
      ),
      WorkspaceTarget::Next
    ));
  }

  #[test]
  fn iris_origins_use_requested_point_and_safe_fallbacks() {
    let bounds = wm_platform::Rect::from_xy(100, 200, 1_000, 800);
    let center = (600, 600);
    let focused = wm_platform::Rect::from_xy(250, 300, 400, 200);

    assert_eq!(
      iris_origin_from_inputs(
        &WorkspaceSwitchIrisOrigin::Center,
        &bounds,
        center,
        Some((300, 400)),
        Some(&focused),
      ),
      center
    );
    assert_eq!(
      iris_origin_from_inputs(
        &WorkspaceSwitchIrisOrigin::Cursor,
        &bounds,
        center,
        Some((300, 400)),
        None,
      ),
      (300, 400)
    );
    assert_eq!(
      iris_origin_from_inputs(
        &WorkspaceSwitchIrisOrigin::Cursor,
        &bounds,
        center,
        Some((99, 400)),
        None,
      ),
      center
    );
    assert_eq!(
      iris_origin_from_inputs(
        &WorkspaceSwitchIrisOrigin::FocusedWindow,
        &bounds,
        center,
        None,
        Some(&focused),
      ),
      (450, 400)
    );
    assert_eq!(
      iris_origin_from_inputs(
        &WorkspaceSwitchIrisOrigin::FocusedWindow,
        &bounds,
        center,
        None,
        None,
      ),
      center
    );
  }
}
