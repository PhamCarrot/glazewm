#[cfg(target_os = "windows")]
use std::{
  collections::HashSet,
  sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::Context;
use tracing::{debug, error, warn};
#[cfg(target_os = "windows")]
use wm_common::TabBarPosition;
use wm_common::{
  CursorJumpTrigger, DisplayState, HideCorner, HideMethod, UniqueExt,
  WindowState, WmEvent,
};
#[cfg(target_os = "windows")]
use wm_common::{WindowEffectConfig, WorkspaceSwitchStyle};
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;
#[cfg(target_os = "windows")]
use wm_platform::{
  BackdropStyle, BlurOverlayParams, CornerStyle, NativeBlurOverlay,
  NativeIrisOverlay, NativeStackTabBar, OpacityValue, SurrogateBatch,
  SurrogateBatchResult, TabBarColors, TabInfo, WorkspaceSurrogate,
};
use wm_platform::{Rect, WindowZOrder};

#[cfg(target_os = "windows")]
use crate::animation::manager::{
  opacity_target_was_destroyed, WorkspaceCloakRetryOutcome,
  WorkspaceRedirectOutcome,
};
#[cfg(target_os = "windows")]
use crate::animation::AnimationManager;
#[cfg(target_os = "windows")]
use crate::commands::window::isolate_destroyed_window;
#[cfg(target_os = "windows")]
use crate::commands::workspace::commit_pending_workspace_redirect;
#[cfg(target_os = "windows")]
use crate::pending_sync::IrisSwitchRequest;
use crate::{
  animation::AnimationPositionResult,
  models::{Container, WindowContainer},
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepositionWindowOutcome {
  Applied,
  NativeMinimizePending,
}

#[cfg(target_os = "windows")]
static WINDOW_POSITION_FAILURES: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "windows")]
fn rate_limited_native_failure(counter: &AtomicUsize) -> Option<usize> {
  let previous = counter
    .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
      value.checked_add(1)
    })
    .ok()?;
  let occurrence = previous + 1;
  (occurrence == 1 || occurrence.is_multiple_of(64)).then_some(occurrence)
}

/// Returns the smallest iris radius that fully covers the monitor from the
/// origin — the distance to the farthest monitor corner.
#[cfg(target_os = "windows")]
fn iris_max_radius(req: &IrisSwitchRequest) -> i32 {
  let corners = [
    (req.monitor_x, req.monitor_y),
    (req.monitor_x + req.monitor_width, req.monitor_y),
    (req.monitor_x, req.monitor_y + req.monitor_height),
    (
      req.monitor_x + req.monitor_width,
      req.monitor_y + req.monitor_height,
    ),
  ];
  corners
    .iter()
    .map(|&(x, y)| {
      let dx = f64::from(x - req.origin_x);
      let dy = f64::from(y - req.origin_y);
      dx.hypot(dy)
    })
    .fold(0.0_f64, f64::max)
    .ceil() as i32
}

/// Whether a per-window workspace transition has a safe visible
/// replacement for every participant and enough direction information to
/// animate.
#[cfg(target_os = "windows")]
fn should_start_workspace_switch(
  participant_count: usize,
  ready_surrogate_count: usize,
  direction: i32,
  is_no_slide: bool,
) -> bool {
  participant_count > 0
    && ready_surrogate_count == participant_count
    && (direction != 0 || is_no_slide)
}

/// Whether a visible reposition needs a second, tick-deferred alpha
/// reassertion.
///
/// `SWP_FRAMECHANGED` can briefly reset an existing layered-window alpha,
/// but opaque windows have no alpha state to race with DWM and must not
/// pay for a blocking composition flush.
#[cfg(target_os = "windows")]
fn needs_deferred_transparency_reassert(
  is_visible: bool,
  effect_opacity: u8,
) -> bool {
  is_visible && effect_opacity < u8::MAX
}

#[cfg(target_os = "windows")]
fn should_defer_native_focus(
  is_workspace_switch_incoming: bool,
  has_resize_session: bool,
  is_workspace_redirect_fallback: bool,
) -> bool {
  is_workspace_switch_incoming
    || has_resize_session
    || is_workspace_redirect_fallback
}

#[cfg(target_os = "windows")]
fn should_defer_cursor_jump(is_workspace_redirect_fallback: bool) -> bool {
  is_workspace_redirect_fallback
}

#[cfg(target_os = "windows")]
fn anyhow_target_was_destroyed(
  error: &anyhow::Error,
  native_valid_after: bool,
) -> bool {
  error.chain().any(|cause| {
    cause
      .downcast_ref::<wm_platform::Error>()
      .is_some_and(|error| error.is_invalid_window_handle())
  }) || !native_valid_after
}

#[cfg(target_os = "windows")]
fn native_window_identity(window: &WindowContainer) -> (isize, bool) {
  // Destroyed-window isolation can detach this same container. Keep the
  // native `RefCell` guard wholly inside this helper so it is always
  // dropped before cleanup begins.
  let native = window.native();
  (native.id().0, native.is_valid())
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceCloakFailureKind {
  Destroyed,
  TransientApplicationView,
  AccessDenied,
  UnexpectedLiveWindow,
}

#[cfg(target_os = "windows")]
fn classify_workspace_cloak_failure(
  error: &wm_platform::Error,
  native_valid_after: bool,
) -> WorkspaceCloakFailureKind {
  classify_workspace_cloak_failure_flags(
    opacity_target_was_destroyed(error, native_valid_after),
    error.is_application_view_unavailable(),
    error.is_access_denied(),
  )
}

#[cfg(target_os = "windows")]
fn classify_workspace_cloak_failure_flags(
  target_destroyed: bool,
  application_view_unavailable: bool,
  access_denied: bool,
) -> WorkspaceCloakFailureKind {
  if target_destroyed {
    WorkspaceCloakFailureKind::Destroyed
  } else if application_view_unavailable {
    WorkspaceCloakFailureKind::TransientApplicationView
  } else if access_denied {
    WorkspaceCloakFailureKind::AccessDenied
  } else {
    WorkspaceCloakFailureKind::UnexpectedLiveWindow
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusSyncOutcome {
  Complete,
  #[cfg(target_os = "windows")]
  TargetDestroyed {
    window_id: uuid::Uuid,
    expected_handle: isize,
  },
}

/// Returns whether a managed window may use the surrogate animation path
/// for this redraw.
///
/// A workspace-switch surrogate that already owns the visibility handoff
/// must be allowed to finish. Every other fullscreen window or
/// display-change-suppressed redraw bypasses animation so GlazeWM cannot
/// cloak or cover an exclusive/borderless application while Windows is
/// changing its presentation mode.
fn should_use_window_animation_path(
  is_outgoing_switch: bool,
  is_frozen_by_workspace_switch: bool,
  is_fullscreen: bool,
  animations_suppressed: bool,
  is_floating: bool,
  animation_enabled: bool,
  is_state_change: bool,
  has_slide_in: bool,
) -> bool {
  !is_outgoing_switch
    && (is_frozen_by_workspace_switch
      || (!is_fullscreen
        && !animations_suppressed
        && ((!is_floating && animation_enabled)
          || (is_state_change && animation_enabled)
          || has_slide_in)))
}

/// Whether the latest logical rectangle should replace the last presented
/// target immediately.
///
/// An incoming window covered by an active workspace surrogate has not yet
/// presented a later relayout. Retaining its previous target lets the
/// final covered sync create one ordinary move/resize handoff from that
/// visible geometry to the latest layout. Hidden/minimized/outgoing
/// windows and ordinary stationary-workspace redraws keep the existing
/// immediate commit.
fn should_commit_presented_target(
  is_frozen_by_workspace_switch: bool,
  is_visible: bool,
  previous_target: Option<&Rect>,
  target_rect: &Rect,
) -> bool {
  !(is_frozen_by_workspace_switch
    && is_visible
    && previous_target.is_some_and(|previous| previous != target_rect))
}

#[cfg(target_os = "windows")]
fn should_preserve_native_minimize_at_handoff(
  is_visible: bool,
  has_surrogate: bool,
  logical_is_minimized: bool,
  native_is_minimized: bool,
) -> bool {
  is_visible
    && has_surrogate
    && !logical_is_minimized
    && native_is_minimized
}

/// Whether retained surrogate coverage owns the real window's visibility.
///
/// Ordinary move/resize and completed workspace handoffs return `true`. A
/// workspace participant whose cloak failed returns `false`, keeping its
/// visible real HWND on the non-fatal positioning path during local
/// fallback.
#[cfg(target_os = "windows")]
fn surrogate_owns_visibility(
  has_active_surrogate: bool,
  workspace_handoff_cloaked: Option<bool>,
) -> bool {
  has_active_surrogate && workspace_handoff_cloaked.unwrap_or(true)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RedrawOutcome {
  Complete,
  CompleteAfterWorkspaceFallback,
  Deferred,
}

pub fn platform_sync(
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let result = platform_sync_inner(state, config);

  #[cfg(target_os = "windows")]
  {
    let sync_complete =
      result.is_ok() && !state.pending_sync.has_changes();
    state.settle_visibility_leases(result.is_ok(), sync_complete);
  }

  result
}

fn platform_sync_inner(
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  #[cfg(target_os = "windows")]
  let invalid_redirect_recovery = state
    .pending_workspace_redirect
    .as_ref()
    .is_some_and(|pending| {
      state.workspace_by_name(&pending.destination_name).is_none()
    });
  #[cfg(target_os = "windows")]
  if invalid_redirect_recovery {
    if let Some(pending) = state.pending_workspace_redirect.take() {
      warn!(
        destination = %pending.destination_name,
        "Discarding unavailable workspace redirect destination and restoring the currently displayed workspace."
      );
    }
    state
      .pending_sync
      .clear_workspace_switch_participants()
      .queue_container_to_redraw(state.root_container.clone());
    state.animation_manager.begin_workspace_redirect_fallback();
  }

  let mut focused_container =
    state.focused_container().context("No focused container.")?;

  let mut finish_workspace_fallback = false;
  if !state.pending_sync.containers_to_redraw().is_empty()
    || !state.pending_sync.workspaces_to_reorder().is_empty()
  {
    match redraw_containers(&focused_container, state, config)? {
      RedrawOutcome::Deferred => {
        // A covering workspace/iris surface was created in this pass.
        // Keep the complete pending sync intact; the animation timer will
        // retry it after one compositor-paced readiness tick.
        return Ok(());
      }
      RedrawOutcome::CompleteAfterWorkspaceFallback => {
        finish_workspace_fallback = true;
      }
      RedrawOutcome::Complete => {}
    }
    #[cfg(target_os = "windows")]
    if finish_workspace_fallback
      && state.pending_workspace_redirect.is_some()
    {
      commit_pending_workspace_redirect(state, config)?;
    }
    focused_container = state
      .focused_container()
      .context("No focused container after redraw.")?;
  }

  // Focus is synced after `redraw_containers` so that the workspace-switch
  // animation is already set up when `sync_focus` runs. This lets the
  // deferral check in `sync_focus` correctly suppress
  // `SetForegroundWindow` during the slide (the animation manager
  // re-queues focus after it completes), preventing the OS from
  // asynchronously uncloaking the incoming focused window mid-animation.
  if state.pending_sync.needs_focus_update() {
    #[cfg(target_os = "windows")]
    if let FocusSyncOutcome::TargetDestroyed {
      window_id,
      expected_handle,
    } = sync_focus(&focused_container, state)?
    {
      isolate_destroyed_window(
        window_id,
        expected_handle,
        state,
        "native focus",
      );
      focused_container = state
        .focused_container()
        .context("No safe focus target after destroyed HWND.")?;

      // Make one immediate attempt against the newly selected target. This
      // is not a retry loop: a second stale target is isolated and
      // native focus is reset to the desktop, leaving later normal
      // events to select again.
      if let FocusSyncOutcome::TargetDestroyed {
        window_id,
        expected_handle,
      } = sync_focus(&focused_container, state)?
      {
        isolate_destroyed_window(
          window_id,
          expected_handle,
          state,
          "replacement native focus",
        );
        focused_container = state
          .focused_container()
          .context("No focus target after second destroyed HWND.")?;
        state.dispatcher.reset_focus()?;
        state.is_focus_synced = false;
      }
    }
    #[cfg(not(target_os = "windows"))]
    sync_focus(&focused_container, state)?;
  }

  #[cfg(target_os = "windows")]
  let defer_cursor_jump = should_defer_cursor_jump(
    state
      .animation_manager
      .is_workspace_redirect_fallback_active(),
  );
  #[cfg(not(target_os = "windows"))]
  let defer_cursor_jump = false;

  if state.pending_sync.needs_cursor_jump()
    && config.value.general.cursor_jump.enabled
    && !defer_cursor_jump
  {
    jump_cursor(focused_container.clone(), state, config)?;
  }

  if state.pending_sync.needs_focused_effect_update()
    || state.pending_sync.needs_all_effects_update()
  {
    // Keep reference to the previous window that had focus effects
    // applied.
    let prev_effects_window = state.prev_effects_window.clone();

    let mut focused_window = focused_container.as_window_container().ok();
    let focus_changed =
      prev_effects_window.as_ref().map(|window| window.id())
        != focused_window.as_ref().map(|window| window.id());
    let animate_focus_change =
      config.value.animations.focus_change.should_animate()
        && state.pending_sync.needs_focused_effect_update()
        && !state.pending_sync.needs_all_effects_update()
        && focus_changed;

    // A full effect refresh (for example after config reload) should apply
    // the new values immediately. It must not leave an old transition
    // running with targets from the previous configuration.
    #[cfg(target_os = "windows")]
    if !config.value.animations.focus_change.should_animate()
      || state.pending_sync.needs_all_effects_update()
    {
      state.animation_manager.clear_focus_opacity_animations();
    }

    if let Some(window) = focused_window.clone() {
      if apply_window_effects_isolating(
        &window,
        true,
        state,
        config,
        "focused effect application",
      )? {
        #[cfg(target_os = "windows")]
        if animate_focus_change {
          start_focus_transparency_animation(
            &window, true, state, config,
          )?;
        }
        state.prev_effects_window = Some(window);
      } else {
        focused_container = state
          .focused_container()
          .context("No focus target after destroyed effect window.")?;
        focused_window = focused_container.as_window_container().ok();
        if let Some(replacement) = focused_window.clone() {
          if apply_window_effects_isolating(
            &replacement,
            true,
            state,
            config,
            "replacement focused effect application",
          )? {
            #[cfg(target_os = "windows")]
            if animate_focus_change {
              start_focus_transparency_animation(
                &replacement,
                true,
                state,
                config,
              )?;
            }
            state.prev_effects_window = Some(replacement);
          }
        }
      }
    } else {
      state.prev_effects_window = None;
    }

    // Get windows that should have the unfocused border applied to them.
    // For the sake of performance, we only update the border of the
    // previously focused window. If the `reset_window_effects` flag is
    // passed, the unfocused border is applied to all unfocused windows.
    let unfocused_windows =
      if state.pending_sync.needs_all_effects_update() {
        state.windows()
      } else {
        prev_effects_window.clone().into_iter().collect()
      }
      .into_iter()
      .filter(|window| state.container_by_id(window.id()).is_some())
      .filter(|window| window.id() != focused_container.id())
      .collect::<Vec<_>>();

    for window in unfocused_windows {
      if !apply_window_effects_isolating(
        &window,
        false,
        state,
        config,
        "unfocused effect application",
      )? {
        continue;
      }
      #[cfg(target_os = "windows")]
      if animate_focus_change {
        start_focus_transparency_animation(&window, false, state, config)?;
      }
    }

    // Re-apply animation-driven opacity for the focused window if an
    // opacity focus animation is running. `apply_window_effects` above
    // may have reset the transparency to the config value; overriding it
    // here ensures the animated opacity is visible on the first frame.
    #[cfg(target_os = "windows")]
    if let Ok(window) = focused_container.as_window_container() {
      if let Some(anim) =
        state.animation_manager.get_animation(&window.id())
      {
        if let (_, Some(opacity)) = anim.current_state() {
          debug!(
            "Overriding transparency for {} with in-progress animation \
             opacity: alpha={}.",
            window.id(),
            opacity.to_alpha()
          );
          let transparency_result = {
            let native = window.native();
            native.set_transparency(&opacity)
          };
          if let Err(err) = transparency_result {
            let (expected_handle, native_is_valid) =
              native_window_identity(&window);
            if opacity_target_was_destroyed(&err, native_is_valid) {
              debug!(
                "Canceled opacity animation for destroyed window {}: \
                 {err}.",
                window.id()
              );
              isolate_destroyed_window(
                window.id(),
                expected_handle,
                state,
                "in-progress opacity restoration",
              );
            } else {
              return Err(err).with_context(|| {
                format!(
                  "Failed to restore in-progress opacity for window {}",
                  window.id()
                )
              });
            }
          }
        }
      }
    }
  }

  // The latest real layout is now positioned, visible, focused, and has
  // its ordinary effects applied. Let static blur preparation see that
  // layout while old rapid-switch coverage still exists above it.
  #[cfg(target_os = "windows")]
  if finish_workspace_fallback {
    state
      .animation_manager
      .prepare_workspace_redirect_fallback_effects();
  }

  // Sync acrylic blur overlays every tick so they track window position
  // through moves, resizes, and workspace changes.
  #[cfg(target_os = "windows")]
  sync_blur_overlays(state, config, &focused_container);

  // Incompatible rapid-switch coverage deliberately outlives final real-
  // window positioning, effects, and static blur preparation. Start its
  // nonblocking readiness-held crossfade only now; native focus remains
  // deferred until the animation manager drops that obsolete coverage.
  #[cfg(target_os = "windows")]
  if finish_workspace_fallback {
    state.animation_manager.finish_workspace_redirect_fallback();
    sync_tab_bars(state, config);
  }

  #[cfg(target_os = "windows")]
  if invalid_redirect_recovery {
    state
      .animation_manager
      .prepare_workspace_redirect_fallback_effects();
    state.animation_manager.finish_workspace_redirect_fallback();
  }

  state.pending_sync.clear();

  Ok(())
}

fn sync_focus(
  focused_container: &Container,
  state: &mut WmState,
) -> anyhow::Result<FocusSyncOutcome> {
  let native_window = focused_container.as_window_container().ok();

  // Defer `SetForegroundWindow` while the focused window is covered by an
  // active surrogate (workspace-switch or resize). The OS may
  // asynchronously remove the DWM cloak when a window becomes the
  // foreground window, causing the slow `IApplicationView::SetCloak`
  // path to fire on the next animation tick and blocking the frame loop.
  // `AnimationManager::update_internal` re-queues the focus change once
  // the animation completes and the window is uncloaked.
  #[cfg(target_os = "windows")]
  if let Some(window) = &native_window {
    // Incompatible rapid redirects retain old coverage for one readiness
    // tick plus a short fade over the synchronized latest workspace.
    // Native focus (and its cursor/effect confirmation) transfers only
    // after that final visual handoff completes.
    let is_workspace_redirect_fallback = state
      .animation_manager
      .is_workspace_redirect_fallback_active();
    let is_ws_incoming =
      state.animation_manager.is_workspace_switch_active()
        && state
          .animation_manager
          .is_workspace_switch_incoming(&window.id());
    let has_resize_session = state
      .animation_manager
      .resize_sessions
      .contains_key(&window.id());
    if should_defer_native_focus(
      is_ws_incoming,
      has_resize_session,
      is_workspace_redirect_fallback,
    ) {
      // Logical focus already points at the covered target, but native
      // focus still points elsewhere. Keep focus-follows-cursor
      // gated until cleanup requeues this operation and the matching
      // foreground event confirms it.
      state.is_focus_synced = false;
      return Ok(FocusSyncOutcome::Complete);
    }
  }

  // Sets focus to the appropriate target:
  // - If the container is a window, focuses that window.
  // - If the container is a workspace, "resets" focus by focusing the
  //   desktop window.
  //
  // In either case, a `PlatformEvent::WindowFocused` event is subsequently
  // triggered.
  let result = if let Some(window) = &native_window {
    tracing::info!("Setting focus to window: {window}");
    window.native().focus()
  } else {
    tracing::info!("Setting focus to the desktop window.");
    state.dispatcher.reset_focus()
  };

  if let Err(err) = result {
    #[cfg(target_os = "windows")]
    if let Some(window) = native_window {
      if opacity_target_was_destroyed(&err, window.native().is_valid()) {
        state.is_focus_synced = false;
        return Ok(FocusSyncOutcome::TargetDestroyed {
          window_id: window.id(),
          expected_handle: window.native().id().0,
        });
      }
    }
    // Keep cursor-driven focus gated until a later native focus event
    // proves that the OS and logical targets match. Returning the
    // error also keeps `pending_sync` uncleared, so the next safe
    // synchronization opportunity can retry instead of reporting a
    // focus change which never happened.
    state.is_focus_synced = false;
    return Err(anyhow::anyhow!("Failed to set native focus: {err}"));
  }

  state.emit_event(WmEvent::FocusChanged {
    focused_container: focused_container.to_dto()?,
  });

  Ok(FocusSyncOutcome::Complete)
}

/// Finds windows that should be brought to the top of their workspace's
/// z-order.
///
/// Windows are brought to front if they match the focused window's state
/// (floating/tiling) and any of these conditions are met:
///  * Focus has changed to a different window.
///  * Focused window's state has changed (e.g. tiling -> floating).
///  * Focused window has moved to a different workspace.
fn windows_to_bring_to_front(
  focused_container: &Container,
  state: &WmState,
) -> anyhow::Result<Vec<WindowContainer>> {
  let focused_workspace =
    focused_container.workspace().context("No workspace.")?;

  // Add focused workspace if there's been a focus change.
  let workspaces_to_reorder = state
    .pending_sync
    .workspaces_to_reorder()
    .iter()
    .chain(
      state
        .pending_sync
        .needs_focus_update()
        .then_some(&focused_workspace),
    )
    .unique_by(|workspace| workspace.id());

  // Bring forward windows that match the focused state. Only do this for
  // tiling/floating windows.
  let windows_to_bring_to_front = workspaces_to_reorder
    .flat_map(|workspace| {
      let focused_descendant = workspace
        .descendant_focus_order()
        .next()
        .and_then(|container| container.as_window_container().ok());

      match focused_descendant {
        Some(focused_descendant) => workspace
          .descendants()
          .filter_map(|descendant| descendant.as_window_container().ok())
          .filter(|window| {
            let is_floating_or_tiling = matches!(
              window.state(),
              WindowState::Floating(_) | WindowState::Tiling
            );

            is_floating_or_tiling
              && window.state().is_same_state(&focused_descendant.state())
          })
          .collect(),
        None => vec![],
      }
    })
    .collect::<Vec<_>>();

  Ok(windows_to_bring_to_front)
}

#[allow(clippy::too_many_lines)]
fn redraw_containers(
  focused_container: &Container,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<RedrawOutcome> {
  let windows_to_redraw = state.windows_to_redraw();
  let windows_to_bring_to_front =
    windows_to_bring_to_front(focused_container, state)?;

  let windows_to_update = {
    let mut windows = windows_to_redraw
      .iter()
      .chain(&windows_to_bring_to_front)
      .unique_by(|window| window.id())
      .collect::<Vec<_>>();

    // Sorting a single window is a no-op, and building the focus-order
    // index below is an O(total windows) tree walk that runs on every
    // redraw pass while any animation is active -- skip it unless there
    // is actually more than one window to order relative to each other.
    if windows.len() > 1 {
      // Indexed once per pass instead of linearly scanned per window
      // below: that scan made sorting O(windows_to_update x total
      // windows), which showed up as input lag during relayouts that
      // moved several windows on systems with many windows open.
      let focus_order_index: std::collections::HashMap<uuid::Uuid, usize> =
        state
          .root_container
          .descendant_focus_order()
          .enumerate()
          .map(|(i, container)| (container.id(), i))
          .collect();

      // Sort so that windows to be shown are processed before windows to
      // be hidden (prevents a blank-frame flash when uncloaking replaces
      // cloaking, e.g. during stack-focus cycling). Within each group,
      // least-recently-focused is processed first (after the rev() below).
      windows.sort_by_key(|window| {
        let is_inactive_stack_child = window
          .parent()
          .and_then(|p| p.as_stack().cloned())
          .map(|s| {
            s.borrow_child_focus_order().front().copied()
              != Some(window.id())
          })
          .unwrap_or(false);
        let ws_displayed = if state.pending_workspace_redirect.is_some()
          && state
            .pending_sync
            .is_workspace_switch_incoming(&window.id())
        {
          true
        } else if state.pending_workspace_redirect.is_some()
          && state
            .pending_sync
            .is_workspace_switch_outgoing(&window.id())
        {
          false
        } else {
          window.workspace().is_some_and(|w| w.is_displayed())
        };
        let will_show = !is_inactive_stack_child && ws_displayed;
        let focus_pos = focus_order_index.get(&window.id()).copied();
        // Windows to show sort higher → after rev() they are processed
        // first.
        (will_show, focus_pos)
      });
    }

    windows
  };

  // Whether animations are skipped for this sync cycle (e.g. display
  // setting changes). In-flight animations of redrawn windows are
  // cancelled below so their windows snap to their target rect.
  let suppress_animations = state.pending_sync.animations_suppressed();

  #[cfg(target_os = "windows")]
  let mut workspace_redirected = false;
  #[cfg(target_os = "windows")]
  let mut workspace_fallback = state.pending_workspace_redirect.is_some();
  #[cfg(target_os = "windows")]
  let mut workspace_cloak_fallback = false;

  // Workspace-switch pre-pass: create slide surrogates for all
  // incoming/outgoing windows before any real window is repositioned.
  // Outgoing surrogates are shown immediately (before the real window is
  // cloaked) to eliminate the blank-frame flicker.
  #[cfg(target_os = "windows")]
  {
    let has_ws_windows = windows_to_update.iter().any(|window| {
      let id = window.id();
      state.pending_sync.is_workspace_switch_incoming(&id)
        || state.pending_sync.is_workspace_switch_outgoing(&id)
    });

    if (!has_ws_windows
      && state.animation_manager.is_workspace_handoff_pending())
      || state.animation_manager.is_iris_handoff_pending()
    {
      return Ok(RedrawOutcome::Deferred);
    }

    let mut created_staged_handoff = false;
    let ws_config = &config.value.animations.workspace_switch;
    if has_ws_windows
      && state.animation_manager.is_workspace_switch_active()
      && (!ws_config.should_animate()
        || suppress_animations
        || ws_config.style == WorkspaceSwitchStyle::Iris)
    {
      workspace_fallback =
        state.animation_manager.begin_workspace_redirect_fallback();
      state.pending_presented_focus_effect_handoff = None;
    }
    if ws_config.should_animate() && !suppress_animations {
      // Iris-wipe pre-pass: snapshot the monitor (still showing the
      // outgoing workspace) and show the overlay before the real
      // windows are switched in the redraw loop below. The hole is
      // then driven by the animation manager.
      if let Some(req) = state.pending_sync.take_iris_switch() {
        let monitor = Rect::from_xy(
          req.monitor_x,
          req.monitor_y,
          req.monitor_width,
          req.monitor_height,
        );
        // Drop any in-flight overlay first so the new snapshot captures
        // the real current workspace, not the previous overlay
        // mid-wipe. This makes rapid switches play as clean
        // successive wipes rather than nested ones.
        state.animation_manager.clear_iris_switch();
        match NativeIrisOverlay::create(&monitor) {
          Ok(overlay) => {
            state.animation_manager.start_iris_switch(
              overlay,
              req.origin_x,
              req.origin_y,
              iris_max_radius(&req),
              req.monitor_handle,
              ws_config.duration_ms,
              ws_config.easing.clone(),
            );
            // Do not switch the real windows in this sync. The existing
            // animation/vsync timer will retry after one readiness tick,
            // giving DWM an opportunity to compose the covering snapshot
            // without blocking the WM thread.
            created_staged_handoff = true;
          }
          Err(err) => {
            tracing::warn!(
              "Iris overlay failed; instant workspace switch: {err}."
            );
          }
        }
      }

      let direction = state.pending_sync.workspace_switch_direction();
      // Only start a new workspace-switch animation when there are
      // actually incoming/outgoing windows in this sync (i.e., this
      // is the initial platform_sync for the switch, not a follow-up
      // focus event).
      if has_ws_windows
        && ws_config.style != WorkspaceSwitchStyle::Iris
        && state.animation_manager.is_workspace_switch_active()
      {
        let requested_roles = windows_to_update
          .iter()
          .filter_map(|window| {
            let id = window.id();
            if state.pending_sync.is_workspace_switch_incoming(&id) {
              Some((id, true))
            } else if state.pending_sync.is_workspace_switch_outgoing(&id)
            {
              Some((id, false))
            } else {
              None
            }
          })
          .collect::<std::collections::HashMap<_, _>>();

        match state.animation_manager.redirect_workspace_switch(
          &requested_roles,
          direction,
          ws_config,
        ) {
          WorkspaceRedirectOutcome::Redirected => {
            workspace_redirected = true;
            state.pending_presented_focus_effect_handoff =
              focused_container
                .as_window_container()
                .ok()
                .filter(|window| {
                  state
                    .pending_sync
                    .is_workspace_switch_incoming(&window.id())
                })
                .map(|window| window.id());
          }
          WorkspaceRedirectOutcome::FinalStateFallback => {
            workspace_fallback = true;
            state.pending_presented_focus_effect_handoff = None;
          }
          WorkspaceRedirectOutcome::DirectionalReveal => {
            workspace_fallback = true;
            state.pending_presented_focus_effect_handoff = None;
          }
          WorkspaceRedirectOutcome::NotActive
          | WorkspaceRedirectOutcome::ExistingRequest => {}
        }
      }

      if has_ws_windows
        && ws_config.style != WorkspaceSwitchStyle::Iris
        && !state.animation_manager.is_workspace_switch_active()
        && state.pending_workspace_redirect.is_none()
      {
        let is_no_slide = ws_config.style.is_no_slide();
        let mut ws_windows: Vec<(
          uuid::Uuid,
          isize,
          Option<WorkspaceSurrogate>,
          bool,
        )> = Vec::new();
        let mut monitor_x = 0i32;
        let mut monitor_width = 0i32;
        let mut monitor_y = 0i32;
        let mut monitor_height = 0i32;
        let mut monitor_handle = 0isize;

        for window in windows_to_update.iter() {
          let id = window.id();
          let is_incoming =
            state.pending_sync.is_workspace_switch_incoming(&id);
          let is_outgoing =
            state.pending_sync.is_workspace_switch_outgoing(&id);

          if !is_incoming && !is_outgoing {
            continue;
          }

          if monitor_width == 0 {
            if let Some(m) = window.monitor() {
              let props = m.native_properties();
              let b = &props.bounds;
              monitor_x = b.x();
              monitor_width = b.width();
              monitor_y = b.y();
              monitor_height = b.height();
              monitor_handle = props.handle;
            }
          }

          let hwnd = window.native().hwnd();
          let source_handle = window.native().id().0;

          let effect_cfg = if window.id() == focused_container.id() {
            &config.value.window_effects.focused_window
          } else {
            &config.value.window_effects.other_windows
          };
          let opacity = if effect_cfg.transparency.enabled {
            effect_cfg.transparency.opacity.to_alpha()
          } else {
            u8::MAX
          };
          // Carry the acrylic blur onto the surrogate so the frosted-glass
          // effect stays visible while the workspace slides. The static
          // blur overlay is hidden for the duration of the animation.
          let acrylic_tint = effect_cfg.backdrop.acrylic_tint();
          let corner_style = if effect_cfg.corner_style.enabled {
            effect_cfg.corner_style.style.clone()
          } else {
            CornerStyle::Default
          };

          if is_incoming {
            let surrogate = match window.to_rect().and_then(|r| {
              window.total_border_delta().map(|d| r.apply_delta(&d, None))
            }) {
              Ok(rect) => {
                let viewport = Rect::from_xy(
                  monitor_x,
                  monitor_y,
                  monitor_width,
                  monitor_height,
                );
                match WorkspaceSurrogate::new(
                  hwnd,
                  &rect,
                  &viewport,
                  opacity,
                  ws_config.opacity_incoming,
                  &corner_style,
                  acrylic_tint,
                ) {
                  Ok(surrogate) => Some(surrogate),
                  Err(err) => {
                    tracing::warn!(
                      "Failed to create incoming workspace surrogate for \
                       {id}: {err}."
                    );
                    None
                  }
                }
              }
              Err(err) => {
                tracing::warn!(
                  "Failed to resolve incoming workspace window rect for \
                   {id}: {err}."
                );
                None
              }
            };
            ws_windows.push((id, source_handle, surrogate, true));
          } else {
            let current = if let Some(rect) =
              state.window_target_positions.get(&id).cloned()
            {
              Some(rect)
            } else {
              match window.native().frame() {
                Ok(rect) => Some(rect),
                Err(err) => {
                  tracing::warn!(
                    "Failed to resolve outgoing workspace window rect for \
                     {id}: {err}."
                  );
                  None
                }
              }
            };
            let surrogate = current.and_then(|current| {
              let viewport = Rect::from_xy(
                monitor_x,
                monitor_y,
                monitor_width,
                monitor_height,
              );
              match WorkspaceSurrogate::new(
                hwnd,
                &current,
                &viewport,
                opacity,
                ws_config.opacity_outgoing,
                &corner_style,
                acrylic_tint,
              ) {
                Ok(surrogate) => Some(surrogate),
                Err(err) => {
                  tracing::warn!(
                    "Failed to create outgoing workspace surrogate for \
                     {id}: {err}."
                  );
                  None
                }
              }
            });
            ws_windows.push((id, source_handle, surrogate, false));
          }
        }

        let participant_count = ws_windows.len();
        let ready_surrogate_count = ws_windows
          .iter()
          .filter(|(_, _, surrogate, _)| surrogate.is_some())
          .count();

        // For slide styles, skip when direction == 0: workspace names were
        // not found in the config so the slide offset would be 0,
        // placing surrogates at their target and causing an
        // instant flash. Non-slide styles (fade/zoom) have no
        // slide offset so direction == 0 is fine. Also require a
        // valid visible surrogate for every participant. A partial
        // transition must not freeze or cloak a real window that has
        // no replacement; dropping this still-hidden vector falls back to
        // the normal instant workspace switch below.
        if should_start_workspace_switch(
          participant_count,
          ready_surrogate_count,
          direction,
          is_no_slide,
        ) {
          // Show outgoing surrogates while real windows are still active
          // so their DWM thumbnails can warm during the staged tick. For
          // stationary styles, also show incoming surrogates at their
          // initial opacity.
          for (_, _, ref mut surrogate, is_incoming) in &mut ws_windows {
            if let Some(s) = surrogate {
              if !*is_incoming {
                s.show_initial();
              } else if ws_config.style != WorkspaceSwitchStyle::Slide {
                s.show_incoming();
              }
            }
          }

          let presented_focus_effect_handoff = focused_container
            .as_window_container()
            .ok()
            .filter(|window| {
              state
                .pending_sync
                .is_workspace_switch_incoming(&window.id())
            })
            .map(|window| window.id());

          state.animation_manager.start_workspace_switch(
            ws_windows,
            direction, // order_direction: +1/-1
            monitor_x,
            monitor_width,
            monitor_y,
            monitor_height,
            monitor_handle,
            config,
          );
          state.pending_presented_focus_effect_handoff =
            presented_focus_effect_handoff;
          // Preserve the full pending sync and transfer visibility only
          // after the animation timer observes the first compositor tick.
          created_staged_handoff = true;
        } else if ready_surrogate_count != participant_count {
          tracing::warn!(
            "Workspace-switch animation skipped: only \
             {ready_surrogate_count}/{participant_count} participant \
             surrogate(s) were ready; using instant switch."
          );
        }
        // If the incoming workspace is empty, or direction == 0 (workspace
        // not in config), skip the animation.
      }
    }

    if created_staged_handoff {
      return Ok(RedrawOutcome::Deferred);
    }

    // The complete pending workspace sync is intentionally retained while
    // the cover warms. A native event can request another `platform_sync`
    // before the animation timer observes its first compositor tick; do
    // not let that incidental call bypass readiness and cloak the real
    // windows early. A genuinely newer workspace command was already
    // handled by the redirect/final-state logic above and removes the
    // pending handoff.
    if state.animation_manager.is_workspace_handoff_pending() {
      return Ok(RedrawOutcome::Deferred);
    }

    // Stage two of the workspace handoff: cloak each live real HWND while
    // every surrogate remains stationary at progress zero. The
    // undocumented shell ApplicationView can briefly be unpublished
    // for an otherwise valid HWND; that exact failure is retried only
    // on later animation ticks. Other failures stay local to this
    // workspace transition and use the visible-real-window final-state
    // fallback.
    let cloak_participants = state
      .animation_manager
      .pending_workspace_cloak_participants();
    for (window_id, expected_handle) in cloak_participants {
      let Some(window) = windows_to_update.iter().find(|window| {
        window.id() == window_id
          && window.native().id().0 == expected_handle
      }) else {
        isolate_destroyed_window(
          window_id,
          expected_handle,
          state,
          "workspace cloak handoff lookup",
        );
        return Ok(RedrawOutcome::Deferred);
      };

      let (process_name, title) = {
        let properties = window.native_properties();
        (properties.process_name.clone(), properties.title.clone())
      };

      let cloak_result = match window.native().is_cloaked() {
        Ok(true) => Ok(()),
        Ok(false) => window.native().set_cloaked(true),
        Err(err) => Err(err),
      };

      if let Err(err) = cloak_result {
        // A failing undocumented shell call has no formal atomicity
        // guarantee. Re-query DWM before classifying it: if the requested
        // state actually landed, record the transfer and avoid both an
        // unnecessary retry and an incorrect visible-window fallback.
        if window.native().is_cloaked().unwrap_or(false) {
          state.animation_manager.mark_workspace_participant_cloaked(
            &window_id,
            expected_handle,
          );
          continue;
        }

        let (_, native_is_valid) = native_window_identity(window);
        match classify_workspace_cloak_failure(&err, native_is_valid) {
          WorkspaceCloakFailureKind::Destroyed => {
            debug!(
              %window_id,
              hwnd = format_args!("{expected_handle:#x}"),
              %process_name,
              %title,
              error = ?err,
              "Removed destroyed participant during workspace cloak handoff."
            );
            isolate_destroyed_window(
              window_id,
              expected_handle,
              state,
              "workspace cloak handoff",
            );
            return Ok(RedrawOutcome::Deferred);
          }
          WorkspaceCloakFailureKind::TransientApplicationView => {
            match state.animation_manager.record_workspace_cloak_retry() {
              WorkspaceCloakRetryOutcome::Retry { attempt, remaining } => {
                warn!(
                  %window_id,
                  hwnd = format_args!("{expected_handle:#x}"),
                  %process_name,
                  %title,
                  hresult = "0x8002802B",
                  attempt,
                  remaining,
                  error = ?err,
                  "Workspace cloak handoff deferred after transient ApplicationView lookup failure."
                );
                return Ok(RedrawOutcome::Deferred);
              }
              WorkspaceCloakRetryOutcome::FinalStateFallback => {
                warn!(
                  %window_id,
                  hwnd = format_args!("{expected_handle:#x}"),
                  %process_name,
                  %title,
                  hresult = "0x8002802B",
                  error = ?err,
                  "Workspace cloak handoff exhausted its bounded retries; using latest visible real-window state."
                );
              }
            }
          }
          WorkspaceCloakFailureKind::AccessDenied => {
            warn!(
              %window_id,
              hwnd = format_args!("{expected_handle:#x}"),
              %process_name,
              %title,
              error = ?err,
              "Workspace cloak handoff crossed an integrity boundary; leaving the window visible and using the latest real-window state."
            );
          }
          WorkspaceCloakFailureKind::UnexpectedLiveWindow => {
            error!(
              %window_id,
              hwnd = format_args!("{expected_handle:#x}"),
              %process_name,
              %title,
              error = ?err,
              "Unexpected live-HWND workspace cloak handoff failure; containing it to this transition."
            );
          }
        }

        workspace_fallback =
          state.animation_manager.begin_workspace_redirect_fallback();
        workspace_cloak_fallback = workspace_fallback;
        state.pending_presented_focus_effect_handoff = None;
        break;
      }

      state
        .animation_manager
        .mark_workspace_participant_cloaked(&window_id, expected_handle);
    }
  }

  // Get monitors by their optimal hide corner.
  let monitors_by_hide_corner = state.monitors_by_hide_corner();

  // Whether any window in this redraw cycle changes size. Pure
  // translations in the same cycle then share the `window_resize` timing
  // so all edges stay in lock-step during the relayout (see
  // `AnimationManager::start_animation_if_needed`).
  let cycle_has_resize = windows_to_update.iter().any(|window| {
    let target_rect = window.to_rect().and_then(|rect| {
      window
        .total_border_delta()
        .map(|delta| rect.apply_delta(&delta, None))
    });

    match (target_rect, state.window_target_positions.get(&window.id())) {
      (Ok(target_rect), Some(prev)) => {
        prev.width() != target_rect.width()
          || prev.height() != target_rect.height()
      }
      _ => false,
    }
  });

  for window in windows_to_update.iter().rev() {
    #[cfg(target_os = "windows")]
    let expected_handle = window.native().id().0;
    let should_bring_to_front = windows_to_bring_to_front.contains(window);

    let workspace =
      window.workspace().context("Window has no workspace.")?;

    let monitor = window.monitor().context("No monitor.")?;
    let hide_corner = monitors_by_hide_corner
      .iter()
      .find(|(m, _)| m.id() == monitor.id())
      .map(|(_, hide_corner)| hide_corner)
      .context("Monitor not found in hide corner map.")?;

    // Whether the window should be shown above all other windows.
    let z_order = match window.state() {
      WindowState::Floating(config) if config.shown_on_top => {
        WindowZOrder::TopMost
      }
      WindowState::Fullscreen(config) if config.shown_on_top => {
        WindowZOrder::TopMost
      }
      _ if should_bring_to_front => {
        let focused_descendant = workspace
          .descendant_focus_order()
          .next()
          .and_then(|container| container.as_window_container().ok());

        if let Some(focused_descendant) = focused_descendant {
          if window.id() == focused_descendant.id() {
            WindowZOrder::Normal
          } else {
            WindowZOrder::AfterWindow(focused_descendant.native().id())
          }
        } else {
          WindowZOrder::Normal
        }
      }
      _ => WindowZOrder::Normal,
    };

    // Set the z-order of the window.
    //
    // NOTE: macOS doesn't have a robust public API for setting the z-order
    // of a window. See `NativeWindow::raise` for more details.
    #[cfg(target_os = "windows")]
    if should_bring_to_front && !windows_to_redraw.contains(window) {
      tracing::info!("Updating window z-order: {window}");
      if let Err(err) = window.native().set_z_order(&z_order) {
        tracing::warn!("Failed to set window z-order: {}", err);
      }
    }

    // Skip updating the window's position if it only required a z-order
    // change.
    if !windows_to_redraw.contains(window) {
      continue;
    }

    // Capture display state before transition to detect opening windows.
    let previous_display_state = window.display_state();

    // Inactive stack children are always hidden; the active child (first
    // in child_focus_order) is the only visible window in the stack.
    let is_inactive_stack_child = window
      .parent()
      .and_then(|p| p.as_stack().cloned())
      .map(|stack| {
        stack.borrow_child_focus_order().front().copied()
          != Some(window.id())
      })
      .unwrap_or(false);

    // Transition display state depending on whether window will be
    // shown or hidden.
    let new_display_state = if is_inactive_stack_child {
      DisplayState::Hidden
    } else {
      let workspace_should_be_displayed =
        if state.pending_workspace_redirect.is_some()
          && state
            .pending_sync
            .is_workspace_switch_incoming(&window.id())
        {
          true
        } else if state.pending_workspace_redirect.is_some()
          && state
            .pending_sync
            .is_workspace_switch_outgoing(&window.id())
        {
          false
        } else {
          workspace.is_displayed()
        };
      match (
        previous_display_state.clone(),
        workspace_should_be_displayed,
      ) {
        (DisplayState::Hidden | DisplayState::Hiding, true) => {
          DisplayState::Showing
        }
        (DisplayState::Shown | DisplayState::Showing, false) => {
          DisplayState::Hiding
        }
        _ => previous_display_state.clone(),
      }
    };
    window.set_display_state(new_display_state);

    let target_rect = window
      .to_rect()?
      .apply_delta(&window.total_border_delta()?, None);

    let is_visible = matches!(
      window.display_state(),
      DisplayState::Showing | DisplayState::Shown
    );

    // True while this visible incoming window is covered by an active
    // workspace-switch surrogate. Unlike the one-cycle PendingSync marker,
    // this remains true until the final covered handoff.
    #[cfg(target_os = "windows")]
    let is_frozen_by_ws_animation = state
      .animation_manager
      .is_workspace_switch_incoming(&window.id());
    #[cfg(not(target_os = "windows"))]
    let is_frozen_by_ws_animation = false;

    // Get the previous target position before updating.
    let previous_target =
      state.window_target_positions.get(&window.id()).cloned();

    // Do not claim that a relayout was presented while an incoming
    // workspace surrogate still shows the previous geometry. The final
    // covered sync will see this retained target, stage a normal
    // resize surrogate, and only then allow workspace coverage to
    // retire. The layout tree itself already holds the latest target,
    // so repeated changes coalesce without a queue.
    if should_commit_presented_target(
      is_frozen_by_ws_animation,
      is_visible,
      previous_target.as_ref(),
      &target_rect,
    ) {
      state
        .window_target_positions
        .insert(window.id(), target_rect.clone());
    }

    // Floating windows are not animated in general, but we allow a single
    // `window_move` animation when the window just crossed the
    // tiling/floating boundary so the transition is smooth rather than
    // a teleport.
    let is_floating = matches!(window.state(), WindowState::Floating(_));

    // Fullscreen windows are never animated: cloaking the real window (or
    // covering it with a surrogate) kicks exclusive-fullscreen games out
    // of fullscreen, reverting their resolution mode-set and
    // re-triggering a display-settings-changed relayout in a loop.
    let is_fullscreen =
      matches!(window.state(), WindowState::Fullscreen(_));
    let is_state_change =
      state.pending_sync.is_window_state_change(&window.id());

    let is_outgoing_switch = state
      .pending_sync
      .is_workspace_switch_outgoing(&window.id());
    let is_incoming_switch = state
      .pending_sync
      .is_workspace_switch_incoming(&window.id());
    #[cfg(target_os = "windows")]
    let workspace_handoff_fallback_for_window = (workspace_cloak_fallback
      && (is_incoming_switch || is_outgoing_switch))
      || (state.pending_workspace_redirect.is_some()
        && (is_incoming_switch || is_outgoing_switch));
    #[cfg(not(target_os = "windows"))]
    let workspace_handoff_fallback_for_window = false;

    // A window is resizing when its dimensions change (vs. a pure
    // translation).
    let is_resize = previous_target
      .as_ref()
      .map(|prev| {
        prev.width() != target_rect.width()
          || prev.height() != target_rect.height()
      })
      .unwrap_or(false);

    let anim_enabled = if is_resize {
      config.value.animations.window_resize.should_animate()
    } else {
      config.value.animations.window_move.should_animate()
    };

    // Compute effect opacity and corner style unconditionally — needed for
    // both the movement surrogate path and the fade-in path.
    #[cfg(target_os = "windows")]
    let (effect_opacity, corner_style, blur_overlay) = {
      let effect_cfg = if window.id() == focused_container.id() {
        &config.value.window_effects.focused_window
      } else {
        &config.value.window_effects.other_windows
      };
      let opacity = if effect_cfg.transparency.enabled {
        effect_cfg.transparency.opacity.to_alpha()
      } else {
        u8::MAX
      };
      let style = if effect_cfg.corner_style.enabled {
        effect_cfg.corner_style.style.clone()
      } else {
        CornerStyle::Default
      };
      // Snapshotted onto the `ResizeSession` (rather than re-read live
      // from config at tracking time) since the close animation's
      // direct-drive loop runs after the window is detached from the
      // container tree, where `effect_cfg` can no longer be
      // recomputed.
      let corner_radius = if effect_cfg.corner_style.enabled {
        effect_cfg.corner_style.style.approx_radius_px()
      } else {
        CornerStyle::Default.approx_radius_px()
      };
      let blur_overlay = effect_cfg.backdrop.acrylic_tint().map(|tint| {
        effect_cfg.backdrop.to_overlay_params(tint, corner_radius)
      });
      (opacity, style, blur_overlay)
    };

    // Start a slide-in animation for newly appearing tiling windows.
    // `previous_target.is_none()` is true only on the first
    // `platform_sync` call for this window, so the slide-in starts
    // exactly once.
    #[cfg(target_os = "windows")]
    if previous_target.is_none()
      && is_visible
      && !is_floating
      && !is_fullscreen
      && !is_outgoing_switch
      && !is_frozen_by_ws_animation
      && !suppress_animations
      && !workspace_handoff_fallback_for_window
      && config.value.animations.window_open.should_animate()
    {
      let monitor_rect = monitor.to_rect()?;
      // `window.native()` returns a `Ref` into the window's `RefCell`.
      // Bound it to this inner scope so the normal positioning path below
      // can safely borrow the window mutably when open-surrogate creation
      // declines or fails during a rapid show/destroy race.
      {
        let native_ref = window.native();
        state.animation_manager.start_open_animation(
          window.id(),
          target_rect.clone(),
          monitor_rect,
          effect_opacity,
          corner_style,
          blur_overlay,
          config,
          &*native_ref,
        );
      }
    }

    // A slide-in animation creates a `ResizeSession` and animation entry,
    // making the window eligible for the `Frozen`/`Apply` animation paths
    // even when `window_move` animations are disabled.
    #[cfg(target_os = "windows")]
    let has_slide_in = state
      .animation_manager
      .resize_sessions
      .contains_key(&window.id())
      && (state
        .animation_manager
        .is_open_handoff_pending(&window.id())
        || state
          .animation_manager
          .get_animation(&window.id())
          .map_or(false, |a| !a.is_complete()));
    #[cfg(not(target_os = "windows"))]
    let has_slide_in = false;

    // Windows frozen by an in-flight workspace-switch animation stay on
    // the animation path regardless of suppression — the switch's
    // surrogate teardown uncloaks them, so dropping them here would
    // break its invariants. Fullscreen windows and suppressed cycles
    // otherwise always take the non-animated path, which also cancels
    // any in-flight animation (and its surrogate) via
    // `remove_animation` below.
    let should_use_animations = should_use_window_animation_path(
      is_outgoing_switch,
      is_frozen_by_ws_animation,
      is_fullscreen,
      suppress_animations || workspace_handoff_fallback_for_window,
      is_floating,
      anim_enabled,
      is_state_change,
      has_slide_in,
    );

    // Determine the rect to use for this frame.
    //
    // On Windows, `start_animation_if_needed` takes `&NativeWindow`, so a
    // `Ref<NativeWindow>` is obtained via `window.native()` and kept alive
    // only for the duration of that call. It must NOT outlive this block:
    // `reposition_window` (called below) may invoke
    // `set_has_pending_dpi_adjustment`, which calls `borrow_mut` on the
    // same `RefCell` — holding the `Ref` across that call would panic.
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
    let (position_result, anim_opacity) = if should_use_animations {
      // Incoming workspace-switch windows: the surrogate handles all
      // visuals for the full animation duration — freeze the real
      // window.
      #[cfg(target_os = "windows")]
      let native_ref = window.native();
      #[cfg(target_os = "windows")]
      if is_frozen_by_ws_animation {
        (AnimationPositionResult::Frozen, None)
      } else {
        state.animation_manager.start_animation_if_needed(
          window.id(),
          is_resize,
          cycle_has_resize,
          target_rect.clone(),
          previous_target,
          &*native_ref,
          effect_opacity,
          corner_style,
          blur_overlay,
          config,
        )
      }
      #[cfg(not(target_os = "windows"))]
      state.animation_manager.start_animation_if_needed(
        window.id(),
        is_resize,
        cycle_has_resize,
        target_rect.clone(),
        previous_target,
        u8::MAX,
        config,
      )
    } else {
      // Animations are skipped for this window. Cancel any in-progress
      // animation and its surrogate so subsequent ticks don't re-cloak it.
      state.animation_manager.remove_animation(&window.id());
      (AnimationPositionResult::Apply(target_rect.clone()), None)
    };

    debug!("Updating window position: {window}");

    match position_result {
      AnimationPositionResult::Waiting => {
        // A hidden/transparent surrogate is warming up across compositor
        // ticks. Keep the real window visible and untouched until the
        // animation manager completes or fails the handoff.
      }
      AnimationPositionResult::Frozen => {
        // A surrogate overlay is covering this window. On the first frame,
        // cloak the real window (so only the surrogate is visible) and
        // synchronously pre-position it at its target rect. Both
        // operations are skipped on subsequent frames: they are
        // idempotent, and repeating a blocking `SetWindowPos`
        // cross-process every tick stalls the animation loop on
        // slow apps and delays keybinding processing.
        //
        // `handle_window_hidden` is guarded against unmanaging cloaked
        // windows so cloaking is safe. If something unclocks the
        // window mid-animation the next tick will re-cloak and
        // re-position it.
        //
        // For `ResizeSession`-backed animations, `pre_commit` also calls
        // `SetWindowPos` synchronously just before the surrogate drops,
        // guaranteeing the window is at `target_rect` when uncloaked.
        // Skip the per-tick `DwmGetWindowAttribute(DWMWA_CLOAKED)`
        // round-trip for resize-session windows whose cloak state
        // is already known — the check only fires on the first
        // `Frozen` frame and after session
        // teardown. Workspace-switch frozen windows (no resize session)
        // retain the full per-tick guard as a safety net.
        #[cfg(target_os = "windows")]
        let already_cloaked_by_session = state
          .animation_manager
          .resize_sessions
          .get(&window.id())
          .map_or(false, |s| s.is_session_cloaked());
        #[cfg(target_os = "windows")]
        let is_cloaked = if already_cloaked_by_session {
          true
        } else {
          let cloak_result = {
            let native = window.native();
            native.is_cloaked()
          };
          match cloak_result {
            Ok(is_cloaked) => is_cloaked,
            Err(err) => {
              let (_, native_is_valid) = native_window_identity(window);
              if opacity_target_was_destroyed(&err, native_is_valid) {
                isolate_destroyed_window(
                  window.id(),
                  expected_handle,
                  state,
                  "workspace/animation cloak query",
                );
                continue;
              }
              return Err(err).with_context(|| {
                format!(
                  "Failed to query cloak state for window {}",
                  window.id()
                )
              });
            }
          }
        };
        #[cfg(target_os = "windows")]
        if !is_cloaked {
          let is_resize_session = state
            .animation_manager
            .resize_sessions
            .contains_key(&window.id());
          // Every path that can reach `Frozen` now passes a tick-staged
          // readiness gate (workspace or move/resize), so no synchronous
          // compositor barrier is needed before cloaking.
          let cloak_result = {
            let native = window.native();
            native.set_cloaked(true)
          };
          if let Err(err) = cloak_result {
            let (_, native_is_valid) = native_window_identity(window);
            if opacity_target_was_destroyed(&err, native_is_valid) {
              isolate_destroyed_window(
                window.id(),
                expected_handle,
                state,
                "animation cloak handoff",
              );
              continue;
            }
            return Err(err).with_context(|| {
              format!("Failed to cloak animated window {}", window.id())
            });
          }

          // Pre-position the cloaked window at its target rect so it
          // appears there when uncloaked at animation end. Posted
          // asynchronously — the animation duration (~300 ms) is far
          // longer than any app's message-queue processing time.
          if !is_resize_session {
            use wm_platform::{
              SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE,
              SWP_NOSENDCHANGING, SWP_NOZORDER,
            };
            let _ = window.native().set_window_pos(
              &z_order,
              &target_rect,
              SWP_NOZORDER
                | SWP_FRAMECHANGED
                | SWP_NOACTIVATE
                | SWP_NOSENDCHANGING
                | SWP_ASYNCWINDOWPOS,
            );
          } else {
            // Growing resize sessions (both dimensions grow): pre-position
            // the cloaked window at target asynchronously so
            // DWM captures the correctly-sized content during
            // the curtain-reveal. Mixed and shrinking sessions
            // use the clip/wipe approach (thumbnail at
            // source), and stretch sessions sample source-sized content
            // for the whole animation — both leave the window
            // at source until `pre_commit`.
            let session_flags = state
              .animation_manager
              .resize_sessions
              .get(&window.id())
              .map(|s| (s.needs_preposition(), s.is_move_only()));

            if let Some((true, is_move_only)) = session_flags {
              // Post asynchronously: the thumbnail stays registered at
              // source dims until `sync_registration` confirms the resize
              // landed, so a slow-to-respond app costs at most a few
              // frames of backdrop fill in the newly
              // revealed area — never a mis-sized capture.
              // `pre_commit` issues a final synchronous move
              // at animation end as a correctness guarantee.
              use wm_platform::{
                SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE,
                SWP_NOSENDCHANGING, SWP_NOZORDER,
              };
              let mut swp_flags = SWP_NOZORDER
                | SWP_NOACTIVATE
                | SWP_NOSENDCHANGING
                | SWP_ASYNCWINDOWPOS;
              // `SWP_FRAMECHANGED` forces `WM_NCCALCSIZE` plus a full
              // repaint in the target app; a pure move needs neither, so
              // multi-window relayouts skip that per-window repaint burst
              // for windows that only change position.
              if !is_move_only {
                swp_flags |= SWP_FRAMECHANGED;
              }
              let _ = window.native().set_window_pos(
                &z_order,
                &target_rect,
                swp_flags,
              );
            }
          }

          // Mark the session cloaked so subsequent Frozen ticks skip the
          // per-tick `DwmGetWindowAttribute` query.
          if let Some(session) = state
            .animation_manager
            .resize_sessions
            .get_mut(&window.id())
          {
            session.mark_session_cloaked();
          }
        }
      }
      AnimationPositionResult::Apply(ref apply_rect) => {
        // Only omit `SWP_ASYNCWINDOWPOS` when a surrogate is active for
        // this window — adjacent windows must stay in lock-step
        // with the overlay. For pure moves (no surrogate) async is
        // correct and avoids blocking on the target process's
        // message queue each frame. Also treat incoming ws-switch
        // windows as having a surrogate when being uncloaked at
        // animation completion. Without this, the window
        // would be repositioned with `SWP_ASYNCWINDOWPOS` and immediately
        // uncloaked — if its message queue is slow, it appears at its old
        // position for one frame. A workspace participant whose handoff
        // cloak failed is the exception: its real window still owns
        // visibility, so treat retained fallback coverage as decorative
        // and keep later positioning/uncloak failures locally
        // non-fatal.
        #[cfg(target_os = "windows")]
        let has_surrogate = surrogate_owns_visibility(
          state.animation_manager.has_active_surrogate(&window.id()),
          state
            .animation_manager
            .workspace_participant_handoff_cloaked(&window.id()),
        );
        #[cfg(not(target_os = "windows"))]
        let has_surrogate = false;

        // Skip the `SetWindowPos` on the animation-completion redraw when
        // `pre_commit` already positioned the window at exactly this rect.
        // Repositioning again is not just redundant — `SWP_FRAMECHANGED`
        // forces a frame recalculation and full repaint that lands right
        // as the window is uncloaked below, flashing at the end of
        // every move/resize animation. The uncloak and effects
        // below still run.
        #[cfg(target_os = "windows")]
        let already_positioned = is_visible
          && state
            .animation_manager
            .was_pre_committed_at(&window.id(), apply_rect);
        #[cfg(not(target_os = "windows"))]
        let already_positioned = false;
        let mut native_minimize_pending = false;

        if !already_positioned {
          match reposition_window(
            window,
            apply_rect,
            *hide_corner,
            &z_order,
            is_visible,
            has_surrogate,
            config,
          ) {
            Ok(RepositionWindowOutcome::Applied) => {}
            Ok(RepositionWindowOutcome::NativeMinimizePending) => {
              native_minimize_pending = true;
            }
            Err(err) => {
              if has_surrogate {
                #[cfg(target_os = "windows")]
                let (_, native_is_valid) = native_window_identity(window);
                #[cfg(target_os = "windows")]
                if anyhow_target_was_destroyed(&err, native_is_valid) {
                  isolate_destroyed_window(
                    window.id(),
                    expected_handle,
                    state,
                    "surrogate-covered reposition",
                  );
                  continue;
                }
                return Err(err).with_context(|| {
                  format!(
                    "Failed to position surrogate-covered window {}",
                    window.id()
                  )
                });
              }
              #[cfg(target_os = "windows")]
              if let Some(failure_count) =
                rate_limited_native_failure(&WINDOW_POSITION_FAILURES)
              {
                tracing::warn!(
                  failure_count,
                  error = %err,
                  "Failed to set a real-window position; repeated native \
                   failures are rate-limited."
                );
              }
              #[cfg(not(target_os = "windows"))]
              tracing::warn!("Failed to set window position: {}", err);
            }
          }

          #[cfg(target_os = "windows")]
          if !native_minimize_pending
            && needs_deferred_transparency_reassert(
              is_visible,
              effect_opacity,
            )
          {
            state
              .animation_manager
              .schedule_transparency_reassert(window.id());
          }
        }

        // Uncloak after repositioning so the window is revealed at the
        // correct position. This undoes `set_cloaked(true)` from
        // the `Frozen` branch for non-`HideMethod::Cloak`
        // configurations (that method already calls `set_cloaked`
        // internally inside `reposition_window`).
        #[cfg(target_os = "windows")]
        if is_visible && has_surrogate {
          let uncloak_result = {
            let native = window.native();
            native.set_cloaked(false)
          };
          if let Err(err) = uncloak_result {
            let (_, native_is_valid) = native_window_identity(window);
            if opacity_target_was_destroyed(&err, native_is_valid) {
              isolate_destroyed_window(
                window.id(),
                expected_handle,
                state,
                "animation uncloak handoff",
              );
              continue;
            }
            return Err(err).with_context(|| {
              format!("Failed to uncloak animated window {}", window.id())
            });
          }

          // Workspace-switch cleanup deliberately keeps the surrogate
          // visible for one more compositor-paced tick before teardown.
        }

        // Apply animated opacity for opacity-style focus animations. The
        // real window is not cloaked in this path, so `set_transparency`
        // updates it directly each frame.
        #[cfg(target_os = "windows")]
        if let Some(ref opacity) = anim_opacity {
          if let Err(err) = window.native().set_transparency(opacity) {
            if opacity_target_was_destroyed(
              &err,
              window.native().is_valid(),
            ) {
              state.animation_manager.remove_animation(&window.id());
              debug!(
                "Canceled animated opacity for destroyed window {}: \
                 {err}.",
                window.id()
              );
            } else {
              return Err(err).with_context(|| {
                format!(
                  "Failed to apply animated opacity to window {}",
                  window.id()
                )
              });
            }
          }
        }

        if native_minimize_pending {
          // `MINIMIZESTART` is a lossless native lifecycle event. Leave
          // logical state untouched here: applying it inside this sync
          // would queue survivor redraws that the final
          // `pending_sync.clear` below would erase. The
          // scheduler guarantees a non-animation
          // turn immediately after a due frame, so the queued event
          // creates a fresh sync cycle without letting this
          // placement restore the HWND first.
          continue;
        }
      }
    }

    // Mark fullscreen windows as fullscreen on every redraw (including
    // during animations) to ensure browser fullscreen APIs work
    // correctly.
    let is_transitioning_fullscreen =
      match (window.prev_state(), window.state()) {
        (Some(_), WindowState::Fullscreen(s)) if !s.maximized => true,
        (Some(WindowState::Fullscreen(_)), _) => true,
        _ => false,
      };

    let is_currently_fullscreen =
      matches!(window.state(), WindowState::Fullscreen(_));

    if is_currently_fullscreen {
      if let Err(err) = window.native().mark_fullscreen(true) {
        warn!("Failed to mark window as fullscreen: {}", err);
      }
    } else if is_transitioning_fullscreen {
      if let Err(err) = window.native().mark_fullscreen(false) {
        warn!("Failed to unmark window as fullscreen: {}", err);
      }
    }

    // Skip setting taskbar visibility if the window is hidden (has no
    // effect). Since cloaked windows are normally always visible in the
    // taskbar, we only need to set visibility if `show_all_in_taskbar` is
    // `false`.
    #[cfg(target_os = "windows")]
    if config.value.general.hide_method == HideMethod::Cloak
      && !config.value.general.show_all_in_taskbar
      && matches!(
        window.display_state(),
        DisplayState::Showing | DisplayState::Hiding
      )
    {
      if let Err(err) = window.native().set_taskbar_visibility(is_visible)
      {
        tracing::warn!("Failed to set taskbar visibility: {}", err);
      }
    }
  }

  // Commit all surrogate repositions queued during this pass in a single
  // `DeferWindowPos` transaction so adjacent windows' edges land in the
  // same DWM composition frame.
  #[cfg(target_os = "windows")]
  {
    let failed_ids = state.animation_manager.flush_surrogate_updates();
    for id in failed_ids {
      AnimationManager::fail_move_resize_handoff(
        state,
        id,
        "thumbnail property update failed during the current redraw",
      );
    }
    AnimationManager::recover_failed_move_resize_handoffs(state);
  }

  // Keep the acrylic blur overlay tracking each move/resize/open session's
  // surrogate at its just-flushed live rect, instead of leaving it hidden
  // for the whole animation. Close sessions are tracked separately, inside
  // `AnimationManager::update_internal`'s direct-drive loop -- that path
  // never reaches `platform_sync` (closing windows are detached from the
  // layout tree), so skip them here to avoid a duplicate upsert this tick.
  //
  // Repositions are batched into one `DeferWindowPos` transaction
  // (separate from the surrogate batch `flush_surrogate_updates` just
  // committed above) rather than each overlay issuing its own
  // synchronous `SetWindowPos` -- that per-window cost scales with tick
  // rate, which is most visible on high-refresh-rate displays where the
  // animation manager ticks in lockstep with vsync.
  #[cfg(target_os = "windows")]
  {
    let mut blur_batch = SurrogateBatch::new();
    for (id, session) in &state.animation_manager.resize_sessions {
      if state.animation_manager.has_close_animation(id)
        || state.animation_manager.is_move_resize_handoff_pending(id)
      {
        continue;
      }
      let Some(params) = session.blur_overlay_params() else {
        continue;
      };
      match session.current_rect() {
        Some(rect) => upsert_blur_overlay(
          &mut state.blur_overlays,
          *id,
          params,
          &rect,
          &mut blur_batch,
        ),
        None => {
          if let Some(overlay) = state.blur_overlays.get_mut(id) {
            overlay.hide();
          }
        }
      }
    }
    let blur_result = blur_batch.commit();
    remove_failed_blur_overlays(
      state,
      &blur_result,
      "active resize-session tracking",
    );
  }

  // Apply effect opacity to outgoing surrogates now that the real windows
  // have been cloaked. This removes the double-blend that would occur if
  // the surrogate's configured opacity were set before cloaking.
  #[cfg(target_os = "windows")]
  if !workspace_redirected {
    state.animation_manager.apply_outgoing_surrogate_opacities();
  }

  #[cfg(target_os = "windows")]
  sync_tab_bars(state, config);

  #[cfg(target_os = "windows")]
  if workspace_fallback {
    return Ok(RedrawOutcome::CompleteAfterWorkspaceFallback);
  }

  Ok(RedrawOutcome::Complete)
}

fn reposition_window(
  window: &WindowContainer,
  rect: &Rect,
  hide_corner: HideCorner,
  // LINT: `z_order` is only used on Windows.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  z_order: &WindowZOrder,
  is_visible: bool,
  // When true, `SWP_ASYNCWINDOWPOS` is omitted so that adjacent windows
  // move synchronously with the surrogate overlay (both hit DWM in the
  // same frame), preventing a one-frame gap between the overlay and its
  // neighbours.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  has_surrogate: bool,
  config: &UserConfig,
) -> anyhow::Result<RepositionWindowOutcome> {
  // For `HideMethod::PlaceInCorner`, we need to reposition hidden windows
  // to the corner of the monitor.
  if config.value.general.hide_method == HideMethod::PlaceInCorner
    && !is_visible
  {
    const VISIBLE_SLIVER: i32 = 1;

    let monitor_rect = window
      .monitor()
      .context("No monitor.")?
      .native_properties()
      .working_area;

    let frame = window.native_properties().frame;

    let position_y = monitor_rect.bottom - VISIBLE_SLIVER;
    let position_x = match hide_corner {
      HideCorner::BottomLeft => {
        monitor_rect.left + VISIBLE_SLIVER - frame.width()
      }
      HideCorner::BottomRight => monitor_rect.right - VISIBLE_SLIVER,
    };

    // Even though the window size is unchanged, `NativeWindow::set_frame`
    // is used instead of `NativeWindow::reposition` because the latter
    // resulted in occasional incorrect positionings on macOS.
    window.native().set_frame(&Rect::from_xy(
      position_x,
      position_y,
      frame.width(),
      frame.height(),
    ))?;

    return Ok(RepositionWindowOutcome::Applied);
  }

  #[cfg(target_os = "windows")]
  let native_is_minimized = window.native().is_minimized()?;
  #[cfg(target_os = "windows")]
  if should_preserve_native_minimize_at_handoff(
    is_visible,
    has_surrogate,
    window.state() == WindowState::Minimized,
    native_is_minimized,
  ) {
    return Ok(RepositionWindowOutcome::NativeMinimizePending);
  }

  if window.active_drag().is_some() {
    window.native().resize(rect.width(), rect.height())?;
  } else {
    #[cfg(target_os = "macos")]
    window.native().set_frame(rect)?;

    #[cfg(target_os = "windows")]
    {
      use wm_platform::{
        SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE,
        SWP_NOSENDCHANGING, WS_MAXIMIZEBOX,
      };

      // Restore window if it's minimized/maximized and shouldn't be. This
      // is needed to be able to move and resize it.
      let should_restore = match &window.state() {
        // Need to restore window if transitioning from maximized
        // fullscreen to non-maximized fullscreen.
        WindowState::Fullscreen(fullscreen) => {
          !fullscreen.maximized && window.native().is_maximized()?
        }
        // No need to restore window if it'll be minimized. Transitioning
        // from maximized to minimized works without having to
        // restore.
        WindowState::Minimized => false,
        _ => native_is_minimized || window.native().is_maximized()?,
      };

      if should_restore {
        // Restoring to position has the same effect as `ShowWindow` with
        // `SW_RESTORE`, but doesn't cause a flicker.
        window.native().restore(Some(rect))?;
      }

      // During animation frames, omit `SWP_ASYNCWINDOWPOS` so that
      // adjacent windows are repositioned synchronously. This keeps
      // their on-screen position in lock-step with surrogate
      // overlays (which update DWM directly via
      // `UpdateLayeredWindow`), closing the blank gap that
      // appears when async repositioning lags one frame behind the
      // surrogate.
      let mut swp_flags = SWP_NOACTIVATE
        | SWP_NOSENDCHANGING
        | if has_surrogate {
          Default::default()
        } else {
          SWP_ASYNCWINDOWPOS
        };

      match &window.state() {
        WindowState::Minimized => {
          if !native_is_minimized {
            window.native().minimize()?;
          }
        }
        WindowState::Fullscreen(fullscreen)
          if fullscreen.maximized
            && window.native().has_window_style(WS_MAXIMIZEBOX) =>
        {
          if !window.native().is_maximized()? {
            window.native().maximize()?;
          }

          window.native().set_window_pos(z_order, rect, swp_flags)?;
        }
        _ => {
          swp_flags |= SWP_FRAMECHANGED;

          window.native().set_window_pos(z_order, rect, swp_flags)?;

          // `SWP_FRAMECHANGED` forces a non-client-area recalculation that
          // can make DWM briefly composite the window at full opacity
          // before its existing `LWA_ALPHA` is reasserted -- a one-frame
          // flash to solid on every resize/move landing for windows using
          // the `transparency` effect. Reassert it immediately so DWM
          // recomposites with the correct alpha right away.
          _ = window.native().reassert_transparency();

          // When there's a mismatch between the DPI of the monitor and the
          // window, the window might be sized incorrectly after the first
          // move. Setting the position twice resolves inconsistencies from
          // the first call. The flag is cleared after so this only runs
          // once per DPI-change event, not on every subsequent animation
          // frame.
          if window.has_pending_dpi_adjustment() {
            window.native().set_window_pos(z_order, rect, swp_flags)?;
            _ = window.native().reassert_transparency();
            window.set_has_pending_dpi_adjustment(false);
          }
        }
      }

      // Set visibility based on the hide method.
      if config.value.general.hide_method == HideMethod::Cloak {
        window.native().set_cloaked(!is_visible)?;
      } else if is_visible {
        // A newly managed window may still carry an early classification
        // cloak even when the configured hide method is `hide`. Clear that
        // independent DWM state before showing the HWND.
        window.native().set_cloaked(false)?;
        window.native().show()?;
      } else {
        window.native().hide()?;
      }
    }
  }

  Ok(RepositionWindowOutcome::Applied)
}

fn jump_cursor(
  focused_container: Container,
  state: &WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let cursor_jump = &config.value.general.cursor_jump;

  let jump_target = match cursor_jump.trigger {
    CursorJumpTrigger::WindowFocus => Some(focused_container),
    CursorJumpTrigger::MonitorFocus => {
      let target_monitor =
        focused_container.monitor().context("No monitor.")?;

      let cursor_monitor = state
        .dispatcher
        .cursor_position()
        .ok()
        .and_then(|pos| state.monitor_at_point(&pos));

      // Jump to the target monitor if the cursor is not already on it.
      cursor_monitor
        .filter(|monitor| monitor.id() != target_monitor.id())
        .map(|_| target_monitor.into())
    }
  };

  if let Some(jump_target) = jump_target {
    let center = jump_target.to_rect()?.center_point();

    if let Err(err) = state.dispatcher.set_cursor_position(&center) {
      tracing::warn!("Failed to set cursor position: {}", err);
    }
  }

  Ok(())
}

fn apply_window_effects(
  // LINT: `window` is only used on Windows.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  window: &WindowContainer,
  is_focused: bool,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let window_effects = &config.value.window_effects;

  // LINT: `effect_config` is only used on Windows.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  let effect_config = if is_focused {
    &window_effects.focused_window
  } else {
    &window_effects.other_windows
  };

  // Skip if both focused + non-focused window effects are disabled.
  #[cfg(target_os = "windows")]
  if window_effects.focused_window.border.enabled
    || window_effects.other_windows.border.enabled
    || window_effects.focused_window.backdrop.enabled
    || window_effects.other_windows.backdrop.enabled
  {
    apply_border_effect(window, effect_config);
  }

  #[cfg(target_os = "windows")]
  if window_effects.focused_window.hide_title_bar.enabled
    || window_effects.other_windows.hide_title_bar.enabled
  {
    apply_hide_title_bar_effect(window, effect_config);
  }

  #[cfg(target_os = "windows")]
  if window_effects.focused_window.corner_style.enabled
    || window_effects.other_windows.corner_style.enabled
  {
    apply_corner_effect(window, effect_config);
  }

  #[cfg(target_os = "windows")]
  if window_effects.focused_window.transparency.enabled
    || window_effects.other_windows.transparency.enabled
  {
    apply_transparency_effect(window, effect_config)?;
  }

  #[cfg(target_os = "windows")]
  if window_effects.focused_window.backdrop.enabled
    || window_effects.other_windows.backdrop.enabled
  {
    apply_backdrop_effect(window, effect_config);
  }

  Ok(())
}

#[cfg(target_os = "windows")]
fn apply_window_effects_isolating(
  window: &WindowContainer,
  is_focused: bool,
  state: &mut WmState,
  config: &UserConfig,
  operation: &str,
) -> anyhow::Result<bool> {
  match apply_window_effects(window, is_focused, config) {
    Ok(()) => Ok(true),
    Err(err) => {
      let (expected_handle, native_is_valid) =
        native_window_identity(window);
      if anyhow_target_was_destroyed(&err, native_is_valid) {
        isolate_destroyed_window(
          window.id(),
          expected_handle,
          state,
          operation,
        );
        Ok(false)
      } else {
        Err(err)
      }
    }
  }
}

#[cfg(not(target_os = "windows"))]
fn apply_window_effects_isolating(
  window: &WindowContainer,
  is_focused: bool,
  _state: &mut WmState,
  config: &UserConfig,
  _operation: &str,
) -> anyhow::Result<bool> {
  apply_window_effects(window, is_focused, config)?;
  Ok(true)
}

/// Commits the effect state already presented by an incoming workspace
/// surrogate without starting a second focus-opacity transition.
///
/// The previously focused window is committed to its unfocused effect
/// first, matching the normal focused-effect update path. Updating
/// `prev_effects_window` before the post-switch cursor refresh ensures a
/// genuine cursor-selected focus change can animate normally afterward.
#[cfg(target_os = "windows")]
pub fn commit_presented_focus_effect_handoff(
  window: &WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<bool> {
  let previous = state.prev_effects_window.clone();

  state
    .animation_manager
    .clear_focus_opacity_animation(&window.id());
  if !apply_window_effects_isolating(
    window,
    true,
    state,
    config,
    "presented focus-effect handoff",
  )? {
    return Ok(false);
  }

  if let Some(previous) = previous.filter(|w| w.id() != window.id()) {
    state
      .animation_manager
      .clear_focus_opacity_animation(&previous.id());
    let _ = apply_window_effects_isolating(
      &previous,
      false,
      state,
      config,
      "previous focus-effect handoff",
    )?;
  }

  state.prev_effects_window = Some(window.clone());
  Ok(true)
}

#[cfg(target_os = "windows")]
fn apply_border_effect(
  window: &WindowContainer,
  effect_config: &WindowEffectConfig,
) {
  // Suppress the border when a blur-behind material is active so the
  // colored frame doesn't clash with the acrylic/mica backdrop.
  let border_color =
    if effect_config.border.enabled && !effect_config.backdrop.enabled {
      Some(&effect_config.border.color)
    } else {
      None
    };

  _ = window.native().set_border_color(border_color);

  let native = window.native().clone();
  let border_color = border_color.cloned();

  // Re-apply border color after a short delay to better handle
  // windows that change it themselves.
  tokio::task::spawn(async move {
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    _ = native.set_border_color(border_color.as_ref());
  });
}

#[cfg(target_os = "windows")]
fn apply_hide_title_bar_effect(
  window: &WindowContainer,
  effect_config: &WindowEffectConfig,
) {
  _ = window
    .native()
    .set_title_bar_visibility(!effect_config.hide_title_bar.enabled);
}

#[cfg(target_os = "windows")]
fn apply_corner_effect(
  window: &WindowContainer,
  effect_config: &WindowEffectConfig,
) {
  let corner_style = if effect_config.corner_style.enabled {
    &effect_config.corner_style.style
  } else {
    &CornerStyle::Default
  };

  _ = window.native().set_corner_style(corner_style);
}

#[cfg(target_os = "windows")]
fn apply_transparency_effect(
  window: &WindowContainer,
  effect_config: &WindowEffectConfig,
) -> anyhow::Result<()> {
  let transparency = configured_transparency(effect_config);

  debug!(
    "Applying transparency to {}: alpha={}.",
    window.id(),
    transparency.to_alpha()
  );

  window
    .native()
    .set_transparency(&transparency)
    .with_context(|| {
      format!("Failed to set transparency for {}", window.id())
    })
}

#[cfg(target_os = "windows")]
fn configured_transparency(
  effect_config: &WindowEffectConfig,
) -> OpacityValue {
  if effect_config.transparency.enabled {
    effect_config.transparency.opacity.clone()
  } else {
    // Disabled transparency means fully opaque.
    OpacityValue::from_alpha(u8::MAX)
  }
}

#[cfg(target_os = "windows")]
fn start_focus_transparency_animation(
  window: &WindowContainer,
  is_focused: bool,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let effects = &config.value.window_effects;
  let (fallback_effect, target_effect) = if is_focused {
    (&effects.other_windows, &effects.focused_window)
  } else {
    (&effects.focused_window, &effects.other_windows)
  };

  let fallback_start = configured_transparency(fallback_effect);
  let target = configured_transparency(target_effect);
  let Ok(rect) = window.to_rect() else {
    return Ok(());
  };
  let animation = &config.value.animations.focus_change;
  let start = state.animation_manager.start_focus_opacity_animation(
    window.id(),
    rect,
    fallback_start,
    target,
    animation.duration_ms,
    animation.easing.clone(),
  );

  // `apply_window_effects` set the target opacity just above. Restore the
  // transition's first value before DWM presents this frame.
  let transparency_result = {
    let native = window.native();
    native.set_transparency(&start)
  };
  if let Err(err) = transparency_result {
    let (expected_handle, native_is_valid) =
      native_window_identity(window);
    if opacity_target_was_destroyed(&err, native_is_valid) {
      debug!(
        "Canceled initial focus-opacity handoff for destroyed window {}: \
         {err}.",
        window.id()
      );
      isolate_destroyed_window(
        window.id(),
        expected_handle,
        state,
        "initial focus-opacity handoff",
      );
      return Ok(());
    }
    return Err(err).with_context(|| {
      format!(
        "Failed to set initial focus opacity for window {}",
        window.id()
      )
    });
  }

  Ok(())
}

/// Creates, updates, or destroys tab bar overlay windows for all
/// `StackContainer`s that have a non-zero `tab_bar_height`.
///
/// Stale entries (stacks that have been removed) are dropped from
/// `state.tab_bars`. New entries are created via
/// `NativeStackTabBar::create`. Existing entries are repositioned and
/// repainted via `NativeStackTabBar::update`.
#[cfg(target_os = "windows")]
fn sync_tab_bars(state: &mut WmState, config: &UserConfig) {
  use uuid::Uuid;

  let ws_switch_active =
    state.animation_manager.is_workspace_switch_active();

  struct StackTabInfo {
    id: Uuid,
    tab_bar_rect: Rect,
    tabs: Vec<TabInfo>,
    active_index: usize,
    /// Whether the stack's workspace is currently displayed.
    visible: bool,
  }

  // Collect ALL stacks with a configured tab bar (across all workspaces).
  // The `visible` flag drives whether the bar is shown or hidden.
  let stacks_info: Vec<StackTabInfo> = state
    .root_container
    .descendants()
    .filter_map(|c| c.as_stack().cloned())
    .filter(|s| s.tab_bar_height_px() > 0)
    .filter_map(|s| {
      let stack_rect = s.to_rect().ok()?;
      let tab_h = s.tab_bar_height_px();
      let tab_bar_rect = match s.tab_bar_position() {
        TabBarPosition::Top => Rect::from_ltrb(
          stack_rect.left,
          stack_rect.top,
          stack_rect.right,
          stack_rect.top + tab_h,
        ),
        TabBarPosition::Bottom => Rect::from_ltrb(
          stack_rect.left,
          stack_rect.bottom - tab_h,
          stack_rect.right,
          stack_rect.bottom,
        ),
      };

      let focus_order = s.borrow_child_focus_order();
      let active_id = focus_order.front().copied();
      drop(focus_order);

      let children = s.children();
      let active_index = active_id
        .and_then(|id| children.iter().position(|c| c.id() == id))
        .unwrap_or(0);

      let tabs: Vec<TabInfo> = children
        .iter()
        .filter_map(|c| c.as_tiling_window())
        .map(|w| {
          let raw_title = w.native_properties().title;
          let title = config
            .compiled_tab_title_overrides
            .iter()
            .fold(raw_title, |t, (re, rep)| {
              re.replace_all(&t, rep.as_str()).into_owned()
            });
          TabInfo {
            title,
            hwnd: w.native().id().0,
          }
        })
        .collect();

      if tabs.is_empty() {
        return None;
      }

      let visible = !ws_switch_active
        && s.workspace().map_or(false, |ws| ws.is_displayed());

      Some(StackTabInfo {
        id: s.id(),
        tab_bar_rect,
        tabs,
        active_index,
        visible,
      })
    })
    .collect();

  // Retain bars for all existing stacks (avoids destroy/recreate on
  // workspace switch). Bars for stacks that have been removed are dropped.
  let current_ids: HashSet<Uuid> =
    stacks_info.iter().map(|s| s.id).collect();
  state.tab_bars.retain(|id, _| current_ids.contains(id));

  let stack_cfg = &config.value.stack;
  let colors = TabBarColors {
    background: stack_cfg.tab_bar_background.clone(),
    active: stack_cfg.tab_active_background.clone(),
    inactive: stack_cfg.tab_inactive_background.clone(),
    text: stack_cfg.tab_text_color.clone(),
    indicator: stack_cfg.tab_active_indicator_color.clone(),
    indicator_height: stack_cfg.tab_active_indicator_height.to_px(0, None),
    border_radius: stack_cfg.tab_border_radius.to_px(0, None),
    separator_width: stack_cfg.tab_separator_width.to_px(0, None),
    separator: stack_cfg.tab_separator_color.clone(),
  };

  let tab_click_tx = state.tab_click_tx.clone();
  let dispatcher = state.dispatcher.clone();

  for info in stacks_info {
    if let Some(bar) = state.tab_bars.get_mut(&info.id) {
      if info.visible {
        bar.update(
          &info.tab_bar_rect,
          info.tabs,
          info.active_index,
          colors.clone(),
        );
      } else {
        bar.hide();
      }
    } else if info.visible {
      let tx = tab_click_tx.clone();
      let stack_id = info.id;
      let on_click = Box::new(move |idx: usize| {
        let _ = tx.send((stack_id, idx));
      });

      match NativeStackTabBar::create(
        &dispatcher,
        &info.tab_bar_rect,
        info.tabs,
        info.active_index,
        colors.clone(),
        on_click,
      ) {
        Ok(bar) => {
          state.tab_bars.insert(info.id, bar);
        }
        Err(err) => {
          tracing::warn!("Failed to create tab bar: {err}");
        }
      }
    }
  }
}

#[cfg(target_os = "windows")]
fn apply_backdrop_effect(
  window: &WindowContainer,
  effect_config: &WindowEffectConfig,
) {
  // `Acrylic` is handled via a persistent `NativeBlurOverlay` placed at
  // `HWND_BOTTOM` behind the managed window — SWCA is not applied to the
  // managed window itself to avoid the `WS_EX_LAYERED`/SWCA compositing
  // conflict. `Mica`/`MicaAlt` use `DWMWA_SYSTEMBACKDROP_TYPE` directly
  // on the managed window (Win11 22H2+).
  let style = match (
    effect_config.backdrop.enabled,
    &effect_config.backdrop.style,
  ) {
    (true, BackdropStyle::Mica | BackdropStyle::MicaAlt) => {
      Some(&effect_config.backdrop.style)
    }
    _ => None,
  };

  if let Err(e) = window.native().set_blur_behind(style) {
    warn!("Failed to set blur-behind on window: {e}.");
  }
}

/// Creates or updates a tracked acrylic blur overlay for `window_id`,
/// applying `tint`/`blur_amount`/`corner_radius` and moving it to `rect`.
///
/// Used by the workspace-switch live-tracking driver in
/// `AnimationManager::update_internal`, which always has a definite
/// up-to-date rect from the surrogate it's following each tick and so
/// needs no conditional re-query -- unlike `sync_blur_overlays` below,
/// which only re-queries `frame()` (a cross-process call) when the window
/// is actually being redrawn this tick.
///
/// Takes the overlay map directly (rather than `&mut WmState`) so callers
/// already holding an unrelated borrow into other `WmState` fields -- e.g.
/// the workspace-switch driver, which calls this while still borrowing
/// `state.animation_manager.workspace_switch` for `rect` itself -- can
/// pass `&mut state.blur_overlays` without a borrow-checker conflict.
#[cfg(target_os = "windows")]
pub(crate) fn upsert_blur_overlay(
  overlays: &mut std::collections::HashMap<uuid::Uuid, NativeBlurOverlay>,
  window_id: uuid::Uuid,
  params: BlurOverlayParams,
  rect: &Rect,
  batch: &mut SurrogateBatch,
) {
  match overlays.entry(window_id) {
    std::collections::hash_map::Entry::Occupied(e) => {
      let overlay = e.into_mut();
      overlay.apply(params);
      overlay.defer_rect(batch, rect);
    }
    std::collections::hash_map::Entry::Vacant(e) => {
      match NativeBlurOverlay::create(rect, params) {
        Ok(overlay) => {
          debug!("Blur overlay created for {window_id}.");
          e.insert(overlay);
        }
        Err(err) => {
          debug!("Blur overlay creation failed for {window_id}: {err}.");
        }
      }
    }
  }
}

/// Resolves `window`'s acrylic overlay params from its
/// focused/other-window backdrop config, or `None` when backdrop isn't
/// configured/enabled or isn't set to `BackdropStyle::Acrylic` for it.
///
/// Shared by [`sync_blur_overlays`] (the per-tick static path) and the
/// interactive-drag tracker in `handle_window_moved_or_resized`, which
/// needs the same params to keep the overlay live while the OS drags the
/// window outside of `GlazeWM`'s own redraw pipeline.
#[cfg(target_os = "windows")]
pub(crate) fn blur_overlay_params_for(
  is_focused: bool,
  config: &UserConfig,
) -> Option<BlurOverlayParams> {
  let effect_cfg = if is_focused {
    &config.value.window_effects.focused_window
  } else {
    &config.value.window_effects.other_windows
  };

  let tint = effect_cfg.backdrop.acrylic_tint()?;

  // Mirrors `corner_style` (falling back to `CornerStyle::Default` when
  // disabled, same as `apply_corner_effect`) so the overlay's rounded clip
  // lines up with the real managed window's own DWM-rendered corners
  // sitting on top of it, rather than being an independently configured
  // radius that can mismatch what's actually on screen.
  let corner_radius = if effect_cfg.corner_style.enabled {
    effect_cfg.corner_style.style.approx_radius_px()
  } else {
    CornerStyle::Default.approx_radius_px()
  };

  Some(effect_cfg.backdrop.to_overlay_params(tint, corner_radius))
}

#[cfg(target_os = "windows")]
pub(crate) fn remove_failed_blur_overlays(
  state: &mut WmState,
  result: &SurrogateBatchResult,
  context: &str,
) {
  if result.failed_hwnds.is_empty() {
    return;
  }
  let failed: Vec<_> = state
    .blur_overlays
    .iter()
    .filter(|(_, overlay)| result.failed_hwnds.contains(&overlay.handle()))
    .map(|(id, _)| *id)
    .collect();
  for id in failed {
    tracing::warn!(
      "Removing blur overlay for {id} after fallback positioning failed \
       during {context}."
    );
    state.blur_overlays.remove(&id);
  }
}

/// Creates, repositions, and removes acrylic blur overlay windows so that
/// every managed window with `backdrop: acrylic` has a matching overlay
/// positioned at `HWND_BOTTOM` flush with the window's DWM frame rect.
///
/// Called on every `platform_sync` tick so overlays track window position
/// through moves, resizes, and focus changes. The overlay is hidden (but
/// kept alive) for windows on inactive workspaces so it can be re-shown
/// instantly when the workspace becomes active again.
///
/// During animations, surrogates carry the acrylic effect directly via
/// SWCA so blur is visible throughout the animation. The static overlay is
/// hidden while a surrogate is active and restored once the animation
/// completes.
///
/// The tint is re-evaluated for every window on every tick (cheap: it
/// no-ops internally unless the resolved value changed, e.g. on focus
/// change). The window's on-screen rect, however, is only re-queried via
/// `frame()` -- a cross-process `DwmGetWindowAttribute` call -- for
/// windows actually queued for redraw this tick (or on first creation).
/// GlazeWM owns all positioning of managed windows through that same
/// redraw queue, so a window that isn't in it this tick cannot have moved;
/// skipping the query avoids a `DwmGetWindowAttribute` round-trip per
/// acrylic window per tick for windows unrelated to whatever triggered
/// this sync (e.g. other monitors/workspaces during a workspace-switch
/// animation elsewhere).
#[cfg(target_os = "windows")]
fn sync_blur_overlays(
  state: &mut WmState,
  config: &UserConfig,
  focused_container: &Container,
) {
  let all_windows = state.windows();
  let mut wanted_ids = std::collections::HashSet::new();
  let mut batch = SurrogateBatch::new();

  // `containers_to_redraw()` may hold an ancestor (e.g. a whole workspace
  // on a workspace switch) rather than each window individually --
  // mirror the same descendant expansion `redraw_containers` uses via
  // `windows_to_redraw` so a window whose ancestor was queued still
  // counts as redrawing.
  let redrawing_ids: std::collections::HashSet<_> = state
    .windows_to_redraw()
    .iter()
    .map(CommonGetters::id)
    .collect();

  for window in &all_windows {
    let is_focused = window.id() == focused_container.id();

    let Some(params) = blur_overlay_params_for(is_focused, config) else {
      continue;
    };

    wanted_ids.insert(window.id());

    // Windows with a `Live`-mode (acrylic-tinted) workspace-switch
    // surrogate are owned entirely by the per-tick tracker in
    // `AnimationManager::update_internal` for the whole slide plus the
    // `pending_ws_cleanup` grace tick -- it updates tint/blur_amount/
    // corner_radius/rect every tick using the surrogate's own live
    // position, which this function cannot do since it isn't invoked
    // on most mid-slide ticks (only when `platform_sync` itself runs).
    // Leave these overlays untouched here entirely: falling through to
    // the hide logic below would hide them out from under the tracker,
    // since the outgoing workspace's `!is_displayed()` flips true the
    // instant the switch command runs, well before the animation
    // completes. Windows with an active or fading `ResizeSession`
    // (move/resize/open/ close) are likewise owned entirely by the
    // trackers in `platform_sync`'s post-flush loop,
    // `AnimationManager::update_internal`'s close direct- drive loop,
    // and its `pending_session_cleanup` fade-tail loop -- same
    // reasoning as the workspace-switch case above.
    if state.animation_manager.has_live_ws_surrogate(&window.id())
      || state
        .animation_manager
        .has_live_resize_tracker(&window.id())
    {
      continue;
    }

    // Hide the static overlay while some other kind of surrogate/session
    // is active for this window without a live blur tracker of its own
    // (e.g. blur-behind not configured for it). When such a surrogate
    // is running, the real window is cloaked at its target rect;
    // reading frame() would return the target position while the
    // surrogate is still mid-animation, causing the overlay to jump
    // ahead.
    //
    // Also hide for windows on inactive workspaces; keep the entry alive
    // so it can be re-shown immediately when the workspace returns.
    let should_hide =
      state.animation_manager.has_active_surrogate(&window.id())
        || !window.workspace().is_some_and(|ws| ws.is_displayed());

    if should_hide {
      if let Some(overlay) = state.blur_overlays.get_mut(&window.id()) {
        overlay.hide();
      }
      continue;
    }

    let is_redrawing = redrawing_ids.contains(&window.id());
    let had_overlay = state.blur_overlays.contains_key(&window.id());

    match state.blur_overlays.entry(window.id()) {
      std::collections::hash_map::Entry::Occupied(e) => {
        let overlay = e.into_mut();
        overlay.apply(params);

        // Always re-query and re-show when the overlay isn't currently
        // visible, even if this window isn't part of this tick's redraw --
        // otherwise an overlay hidden on some earlier tick (e.g. while a
        // surrogate was active) stays hidden indefinitely once
        // `should_hide` clears, unless this exact window happens
        // to be redrawn again for an unrelated reason (see
        // `is_visible` doc comment).
        if is_redrawing || !overlay.is_visible() {
          // Use the DWM extended frame bounds so the overlay sits flush
          // with the window's visible edge, clipping the invisible resize
          // border.
          match window.native().frame() {
            Ok(rect) => overlay.defer_rect(&mut batch, &rect),
            Err(err) => debug!(
              "Blur overlay frame() query failed for {}: {err}.",
              window.id()
            ),
          }
        }
      }
      std::collections::hash_map::Entry::Vacant(e) => {
        debug_assert!(!had_overlay);

        let Ok(rect) = window.native().frame() else {
          continue;
        };

        match NativeBlurOverlay::create(&rect, params) {
          Ok(overlay) => {
            debug!("Blur overlay created for {}.", window.id());
            e.insert(overlay);
          }
          Err(err) => {
            debug!(
              "Blur overlay creation failed for {}: {err}.",
              window.id()
            );
          }
        }
      }
    }
  }

  // Destroy overlays for windows that no longer need them.
  state.blur_overlays.retain(|id, _| wanted_ids.contains(id));

  let result = batch.commit();
  remove_failed_blur_overlays(state, &result, "static overlay sync");
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use anyhow::Context;
  use wm_platform::Rect;

  use super::{
    anyhow_target_was_destroyed, classify_workspace_cloak_failure_flags,
    needs_deferred_transparency_reassert, rate_limited_native_failure,
    should_commit_presented_target, should_defer_cursor_jump,
    should_defer_native_focus, should_preserve_native_minimize_at_handoff,
    should_start_workspace_switch, should_use_window_animation_path,
    surrogate_owns_visibility, WorkspaceCloakFailureKind,
  };

  #[test]
  fn native_failure_diagnostics_are_strictly_rate_limited() {
    let counter = std::sync::atomic::AtomicUsize::new(0);
    let emitted: Vec<_> = (1..=130)
      .filter_map(|_| rate_limited_native_failure(&counter))
      .collect();
    assert_eq!(emitted, vec![1, 64, 128]);
  }

  #[test]
  fn only_destroyed_targets_are_isolated_from_native_failures() {
    let invalid = Err::<(), _>(wm_platform::Error::WindowNotFound)
      .context("wrapped native operation")
      .unwrap_err();
    assert!(anyhow_target_was_destroyed(&invalid, true));

    let live_failure = anyhow::Error::new(wm_platform::Error::Platform(
      "live failure".into(),
    ));
    assert!(!anyhow_target_was_destroyed(&live_failure, true));
    assert!(anyhow_target_was_destroyed(&live_failure, false));
  }

  #[test]
  fn workspace_cloak_failures_have_disjoint_local_actions() {
    assert_eq!(
      classify_workspace_cloak_failure_flags(false, true, false),
      WorkspaceCloakFailureKind::TransientApplicationView
    );
    assert_eq!(
      classify_workspace_cloak_failure_flags(false, false, true),
      WorkspaceCloakFailureKind::AccessDenied
    );
    assert_eq!(
      classify_workspace_cloak_failure_flags(false, false, false),
      WorkspaceCloakFailureKind::UnexpectedLiveWindow
    );
    assert_eq!(
      classify_workspace_cloak_failure_flags(true, true, true),
      WorkspaceCloakFailureKind::Destroyed
    );
  }

  #[test]
  fn only_successfully_cloaked_workspace_participants_transfer_visibility()
  {
    assert!(surrogate_owns_visibility(true, Some(true)));
    assert!(!surrogate_owns_visibility(true, Some(false)));
    assert!(surrogate_owns_visibility(true, None));
    assert!(!surrogate_owns_visibility(false, Some(true)));
  }

  #[test]
  fn workspace_switch_requires_every_surrogate() {
    assert!(!should_start_workspace_switch(2, 1, 1, false));
    assert!(!should_start_workspace_switch(1, 0, 1, false));
  }

  #[test]
  fn workspace_switch_starts_when_every_surrogate_is_ready() {
    assert!(should_start_workspace_switch(2, 2, 1, false));
    assert!(should_start_workspace_switch(2, 2, 0, true));
  }

  #[test]
  fn slide_workspace_switch_still_requires_a_direction() {
    assert!(!should_start_workspace_switch(2, 2, 0, false));
    assert!(!should_start_workspace_switch(0, 0, 1, false));
  }

  #[test]
  fn only_visible_transparent_repositions_need_deferred_reassertion() {
    assert!(!needs_deferred_transparency_reassert(true, u8::MAX));
    assert!(!needs_deferred_transparency_reassert(false, 128));
    assert!(needs_deferred_transparency_reassert(true, 254));
  }

  #[test]
  fn surrogate_covered_focus_is_deferred_until_handoff() {
    assert!(should_defer_native_focus(true, false, false));
    assert!(should_defer_native_focus(false, true, false));
    assert!(should_defer_native_focus(false, false, true));
    assert!(!should_defer_native_focus(false, false, false));
  }

  #[test]
  fn redirected_workspace_cursor_jump_is_deferred_until_handoff() {
    assert!(should_defer_cursor_jump(true));
    assert!(!should_defer_cursor_jump(false));
  }

  #[test]
  fn fullscreen_windows_bypass_surrogate_animation() {
    assert!(!should_use_window_animation_path(
      false, false, true, false, false, true, true, true,
    ));
  }

  #[test]
  fn display_change_suppression_cancels_ordinary_animation_path() {
    assert!(!should_use_window_animation_path(
      false, false, false, true, false, true, true, true,
    ));
  }

  #[test]
  fn active_workspace_handoff_finishes_even_when_suppressed() {
    assert!(should_use_window_animation_path(
      false, true, false, true, false, true, false, false,
    ));
  }

  #[test]
  fn outgoing_workspace_windows_are_owned_by_switch_cleanup() {
    assert!(!should_use_window_animation_path(
      true, false, false, false, false, true, true, true,
    ));
  }

  #[test]
  fn close_minimize_and_destroy_reflows_retain_last_presented_target() {
    let presented = Rect::from_xy(0, 0, 800, 600);
    let latest = Rect::from_xy(0, 0, 1200, 600);

    // Native minimize, native destroy, and GlazeWM close all redraw the
    // same surviving tiling siblings. The presentation rule is therefore
    // intentionally trigger-independent.
    for trigger in ["native minimize", "native destroy", "GlazeWM close"] {
      assert!(
        !should_commit_presented_target(
          true,
          true,
          Some(&presented),
          &latest,
        ),
        "{trigger} must retain the survivor's presented geometry"
      );
    }
    assert!(should_commit_presented_target(
      true,
      true,
      Some(&presented),
      &presented,
    ));
  }

  #[test]
  fn repeated_covered_reflows_coalesce_to_one_latest_target() {
    let presented = Rect::from_xy(0, 0, 600, 600);
    let intermediate = Rect::from_xy(0, 0, 900, 600);
    let latest = Rect::from_xy(0, 0, 1200, 600);
    let mut recorded = presented.clone();

    for logical_target in [&intermediate, &latest] {
      if should_commit_presented_target(
        true,
        true,
        Some(&recorded),
        logical_target,
      ) {
        recorded = logical_target.clone();
      }
    }

    assert_eq!(recorded, presented);
    assert!(should_commit_presented_target(
      false,
      true,
      Some(&recorded),
      &latest,
    ));
    assert_ne!(recorded, latest);
  }

  #[test]
  fn stationary_workspace_survivor_reflow_uses_resize_animation_path() {
    assert!(should_use_window_animation_path(
      false, false, false, false, false, true, false, false,
    ));
  }

  #[test]
  fn covered_native_minimize_wins_over_final_window_placement() {
    assert!(should_preserve_native_minimize_at_handoff(
      true, true, false, true,
    ));
    assert!(!should_preserve_native_minimize_at_handoff(
      true, false, false, true,
    ));
    assert!(!should_preserve_native_minimize_at_handoff(
      false, true, false, true,
    ));
    assert!(!should_preserve_native_minimize_at_handoff(
      true, true, true, true,
    ));
    assert!(!should_preserve_native_minimize_at_handoff(
      true, true, false, false,
    ));
  }

  #[test]
  fn nonpresented_windows_and_stationary_relayouts_commit_immediately() {
    let presented = Rect::from_xy(0, 0, 800, 600);
    let latest = Rect::from_xy(0, 0, 1200, 600);

    // Hidden/minimized windows are not visible; outgoing and ordinary
    // stationary-workspace windows are not incoming-frozen.
    assert!(should_commit_presented_target(
      true,
      false,
      Some(&presented),
      &latest,
    ));
    assert!(should_commit_presented_target(
      false,
      true,
      Some(&presented),
      &latest,
    ));
    assert!(should_commit_presented_target(true, true, None, &latest,));
  }
}
