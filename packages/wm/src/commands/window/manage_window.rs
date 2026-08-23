#[cfg(target_os = "windows")]
use std::{
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::Duration,
};

use anyhow::Context;
use tracing::info;
use wm_common::{WindowRuleEvent, WindowState, WmEvent};
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;
use wm_platform::{NativeWindow, RectDelta, WindowId};

use crate::{
  commands::{
    container::{attach_container, set_focused_descendant},
    window::run_window_rules,
  },
  models::{
    Container, Monitor, NativeWindowProperties, NonTilingWindow,
    TilingWindow, WindowContainer,
  },
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

const MAX_MANAGE_RETRIES: u8 = 3;
const MANAGE_RETRY_BASE_DELAY_MS: u64 = 50;
#[cfg(target_os = "windows")]
const EARLY_CLOAK_TIMEOUT_MS: u64 = 250;

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EarlyWindowTraits {
  is_child: bool,
  is_tool_or_no_activate: bool,
  is_owned_without_caption: bool,
}

#[cfg(target_os = "windows")]
fn should_early_cloak(traits: EarlyWindowTraits) -> bool {
  !traits.is_child
    && !traits.is_tool_or_no_activate
    && !traits.is_owned_without_caption
}

/// Owns an early cloak until normal platform synchronization takes over.
///
/// Dropping the lease restores visibility. The timeout uses the same
/// idempotent release path, so a stalled classification cannot strand an
/// HWND even if the main event loop is temporarily blocked.
#[cfg(target_os = "windows")]
struct EarlyCloakLease {
  active: Arc<AtomicBool>,
  uncloak: Arc<dyn Fn() + Send + Sync>,
}

#[cfg(target_os = "windows")]
impl EarlyCloakLease {
  fn arm(native_window: &NativeWindow) -> Option<Self> {
    if let Err(err) = native_window.set_cloaked(true) {
      tracing::warn!(
        "Failed to early-cloak window {:?}; continuing visibly: {err:#}",
        native_window.id(),
      );
      return None;
    }

    let window_to_restore = native_window.clone();
    let window_id = native_window.id();
    let uncloak: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
      if window_to_restore.is_valid() {
        if let Err(err) = window_to_restore.set_cloaked(false) {
          tracing::warn!(
            "Failed to restore visibility for early-cloaked window \
             {window_id:?}: {err:#}"
          );
        }
      }
    });

    Some(Self::with_timeout(
      uncloak,
      Duration::from_millis(EARLY_CLOAK_TIMEOUT_MS),
      window_id,
    ))
  }

  fn with_timeout(
    uncloak: Arc<dyn Fn() + Send + Sync>,
    timeout: Duration,
    window_id: WindowId,
  ) -> Self {
    let active = Arc::new(AtomicBool::new(true));
    let timeout_active = Arc::clone(&active);
    let timeout_uncloak = Arc::clone(&uncloak);

    tokio::spawn(async move {
      tokio::time::sleep(timeout).await;
      if Self::release_shared(&timeout_active, &timeout_uncloak) {
        tracing::warn!(
          "Early classification cloak timed out for window {window_id:?}; \
           restored visibility while management continues."
        );
      }
    });

    Self { active, uncloak }
  }

  fn release_shared(
    active: &AtomicBool,
    uncloak: &Arc<dyn Fn() + Send + Sync>,
  ) -> bool {
    if active.swap(false, Ordering::AcqRel) {
      uncloak();
      true
    } else {
      false
    }
  }

  /// Transfers visibility ownership to animation/platform synchronization.
  fn transfer(self) {
    self.active.store(false, Ordering::Release);
  }
}

#[cfg(target_os = "windows")]
impl Drop for EarlyCloakLease {
  fn drop(&mut self) {
    Self::release_shared(&self.active, &self.uncloak);
  }
}

pub fn manage_window(
  native_window: NativeWindow,
  target_parent: Option<Container>,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  manage_window_attempt(native_window, target_parent, state, config, 0)
}

/// Retries a native window whose initial property query failed.
pub fn retry_manage_window(
  native_window: NativeWindow,
  attempt: u8,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  // A stale timer is ignored if another show notification classified or
  // managed the HWND while this retry was sleeping.
  if !state
    .pending_window_manage_retries
    .remove(&native_window.id())
  {
    return Ok(());
  }

  if !native_window.is_valid()
    || state.window_from_native(&native_window).is_some()
    || state.ignored_windows.contains(&native_window)
  {
    return Ok(());
  }

  manage_window_attempt(native_window, None, state, config, attempt)
}

fn manage_window_attempt(
  native_window: NativeWindow,
  target_parent: Option<Container>,
  state: &mut WmState,
  config: &mut UserConfig,
  failed_attempts: u8,
) -> anyhow::Result<()> {
  #[cfg(target_os = "windows")]
  let mut early_cloak = match early_cloak_candidate(&native_window) {
    Ok(None) => {
      state
        .pending_window_manage_retries
        .remove(&native_window.id());
      return Ok(());
    }
    Ok(Some(true)) => EarlyCloakLease::arm(&native_window),
    Ok(Some(false)) => None,
    Err(err) => {
      schedule_manage_retry(native_window, failed_attempts, state, &err);
      return Ok(());
    }
  };

  let native_properties = match check_is_manageable(
    &native_window,
    config,
    cfg!(target_os = "windows"),
  ) {
    Ok(Some(properties)) => {
      state
        .pending_window_manage_retries
        .remove(&native_window.id());
      properties
    }
    Ok(None) => {
      // This window was successfully classified as intentionally
      // unmanaged. Cancel any older delayed retry for the same HWND.
      state
        .pending_window_manage_retries
        .remove(&native_window.id());
      return Ok(());
    }
    Err(err) => {
      schedule_manage_retry(native_window, failed_attempts, state, &err);
      return Ok(());
    }
  };

  // Create the window instance. This may fail if the window handle has
  // already been destroyed, or if the WM state doesn't have a monitor/
  // workspace to place the window on yet.
  let window = match create_window(
    native_window,
    native_properties,
    target_parent,
    state,
    config,
  ) {
    Ok(window) => window,
    Err(err) => {
      tracing::warn!("Operation failed: {:?}", err);
      return Ok(());
    }
  };

  // Set the newly added window as focus descendant. This means the window
  // rules will be run as if the window is focused.
  set_focused_descendant(&window.clone().into(), None);

  // Window might be detached if `ignore` command has been invoked.
  let updated_window = run_window_rules(
    window.clone(),
    &WindowRuleEvent::Manage,
    state,
    config,
  )?;

  if let Some(window) = updated_window {
    info!("New window managed: {window}");

    state.emit_event(WmEvent::WindowManaged {
      managed_window: window.to_dto()?,
    });

    // OS focus should be set to the newly added window in case it's not
    // already focused.
    state.pending_sync.queue_focus_change();

    // Normally, a `PlatformEvent::WindowFocused` event is what triggers
    // focus effects and workspace reordering to be applied. However, when
    // a window is first launched, this event can come before the
    // window is managed, and so we need to force an update here.
    state.pending_sync.queue_focused_effect_update();
    state.pending_sync.queue_workspace_to_reorder(
      window.workspace().context("No workspace.")?,
    );

    // Sibling containers need to be redrawn if the window is tiling.
    state.pending_sync.queue_container_to_redraw(
      if window.state() == WindowState::Tiling {
        window.parent().context("No parent.")?
      } else {
        window.into()
      },
    );

    // All fallible management work is complete. The existing
    // open-animation and platform-sync paths now own when the real
    // HWND becomes visible.
    #[cfg(target_os = "windows")]
    if let Some(lease) = early_cloak.take() {
      lease.transfer();
    }
  } else {
    // Window was detached by an `ignore` rule. The early-cloak lease drops
    // here and restores normal visibility.
  }

  Ok(())
}

#[cfg(target_os = "windows")]
fn early_cloak_candidate(
  native_window: &NativeWindow,
) -> anyhow::Result<Option<bool>> {
  use wm_platform::{
    WS_CAPTION, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
  };

  if !native_window.is_visible()? {
    return Ok(None);
  }

  Ok(Some(should_early_cloak(EarlyWindowTraits {
    is_child: native_window.has_window_style(WS_CHILD),
    is_tool_or_no_activate: native_window
      .has_window_style_ex(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW),
    is_owned_without_caption: native_window.has_owner_window()
      && !native_window.has_window_style(WS_CAPTION),
  })))
}

pub fn cancel_pending_manage_retry(
  native_window_id: WindowId,
  state: &mut WmState,
) -> bool {
  remove_pending_manage_retry(
    &mut state.pending_window_manage_retries,
    native_window_id,
  )
}

fn remove_pending_manage_retry(
  pending: &mut std::collections::HashSet<WindowId>,
  native_window_id: WindowId,
) -> bool {
  pending.remove(&native_window_id)
}

fn next_manage_retry_attempt(failed_attempts: u8) -> Option<u8> {
  (failed_attempts < MAX_MANAGE_RETRIES).then_some(failed_attempts + 1)
}

fn schedule_manage_retry(
  native_window: NativeWindow,
  failed_attempts: u8,
  state: &mut WmState,
  error: &anyhow::Error,
) {
  let Some(attempt) = next_manage_retry_attempt(failed_attempts) else {
    tracing::warn!(
      "Native property queries failed after {MAX_MANAGE_RETRIES} retries for \
       window {:?}; leaving it visible and unmanaged: {error:#}",
      native_window.id(),
    );
    return;
  };

  if !state
    .pending_window_manage_retries
    .insert(native_window.id())
  {
    tracing::debug!(
      "Manageability retry already pending for window {:?}.",
      native_window.id()
    );
    return;
  }

  tracing::warn!(
    "Failed to query native properties for window {:?}; scheduling \
     manageability retry {attempt}/{MAX_MANAGE_RETRIES}: {error:#}",
    native_window.id(),
  );

  let retry_tx = state.window_manage_retry_tx.clone();
  tokio::spawn(async move {
    tokio::time::sleep(std::time::Duration::from_millis(
      MANAGE_RETRY_BASE_DELAY_MS * u64::from(attempt),
    ))
    .await;
    let _ = retry_tx.send((native_window, attempt));
  });
}

/// Checks if a window is manageable and retrieves its native properties.
///
/// Windows matched by a `force-manage` window rule skip the built-in
/// manageability checks (visibility is still required).
///
/// Returns `Ok(Some(properties))` if the window is manageable and its
/// properties were retrieved successfully.
fn check_is_manageable(
  native_window: &NativeWindow,
  config: &UserConfig,
  visibility_already_confirmed: bool,
) -> anyhow::Result<Option<NativeWindowProperties>> {
  if !visibility_already_confirmed && !native_window.is_visible()? {
    return Ok(None);
  }

  // Ensure window has a valid process name, title, etc.
  let native_properties = NativeWindowProperties::try_from(native_window)?;

  // Bypass the checks below for windows matched by a `force-manage`
  // window rule.
  if config.is_force_managed(&native_properties) {
    return Ok(Some(native_properties));
  }

  #[cfg(target_os = "macos")]
  {
    use wm_platform::NativeWindowExtMacOs;

    let is_standard_window = native_window.role()? == "AXWindow"
      && native_window.subrole()? == "AXStandardWindow";

    if !is_standard_window {
      tracing::debug!(
        "Skipping non-standard window: {native_properties:?}"
      );
      return Ok(None);
    }
  }

  #[cfg(target_os = "windows")]
  {
    use wm_platform::{
      NativeWindowWindowsExt, WS_CAPTION, WS_CHILD, WS_EX_NOACTIVATE,
      WS_EX_TOOLWINDOW,
    };

    // Ensure window is top-level (i.e. not a child window). Ignore
    // windows that cannot be focused or if they're unavailable in
    // task switcher (alt+tab menu).
    if native_window.has_window_style(WS_CHILD)
      || native_window
        .has_window_style_ex(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW)
    {
      tracing::debug!(
        "Skipping window with unmanageable styles (candidate for a `force-manage` rule): {native_properties:?}"
      );
      return Ok(None);
    }

    // Some applications spawn top-level windows for menus that
    // should be ignored. This includes the autocomplete popup in
    // Notepad++ and title bar menu in Keepass. Although not
    // foolproof, these can typically be identified by having an
    // owner window and no title bar.
    if native_window.has_owner_window()
      && !native_window.has_window_style(WS_CAPTION)
    {
      tracing::debug!(
        "Skipping owned window without caption (candidate for a `force-manage` rule): {native_properties:?}"
      );
      return Ok(None);
    }
  }

  Ok(Some(native_properties))
}

fn create_window(
  native_window: NativeWindow,
  native_properties: NativeWindowProperties,
  target_parent: Option<Container>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  let nearest_monitor = state
    .nearest_monitor(&native_window)
    .context("No nearest monitor.")?;

  let nearest_workspace = nearest_monitor
    .displayed_workspace()
    .context("No nearest workspace.")?;

  let gaps_config = config.value.gaps.clone();
  let window_state =
    window_state_to_create(&native_properties, &nearest_monitor, config)?;

  // Attach the new window as the first child of the target parent (if
  // provided), otherwise, add as a sibling of the focused container.
  let (target_parent, target_index) = match target_parent {
    Some(parent) => (parent, 0),
    None => insertion_target(&window_state, state)?,
  };

  let target_workspace =
    target_parent.workspace().context("No target workspace.")?;

  let prefers_centered = config
    .value
    .window_behavior
    .state_defaults
    .floating
    .centered;

  // Calculate where window should be placed when floating is enabled. Use
  // the original width/height of the window and optionally position it in
  // the center of the workspace.
  let is_same_workspace = nearest_workspace.id() == target_workspace.id();
  let floating_placement = {
    let placement = if !is_same_workspace || prefers_centered {
      native_properties
        .frame
        .translate_to_center(&target_workspace.to_rect()?)
    } else {
      native_properties.frame.clone()
    };

    // Clamp the window size to be within the workspace's outer gaps. 10px
    // is arbitrary - helps differentiate from tiling windows.
    let max_workspace_rect = target_workspace.max_workspace_rect()?;
    placement.clamp_size(
      max_workspace_rect.width() - 10,
      max_workspace_rect.height() - 10,
    )
  };

  // Window has no border delta unless it's later changed via the
  // `adjust_borders` command.
  let border_delta = RectDelta::zero();

  let window_container: WindowContainer = match window_state {
    WindowState::Tiling => TilingWindow::new(
      None,
      native_window,
      native_properties,
      None,
      border_delta,
      floating_placement,
      false,
      gaps_config,
      Vec::new(),
      None,
    )
    .into(),
    _ => NonTilingWindow::new(
      None,
      native_window,
      native_properties,
      window_state,
      None,
      border_delta,
      None,
      floating_placement,
      !prefers_centered,
      Vec::new(),
      None,
    )
    .into(),
  };

  attach_container(
    &window_container.clone().into(),
    &target_parent,
    Some(target_index),
  )?;

  // The OS might spawn the window on a different monitor to the target
  // parent, so adjustments might need to be made because of DPI.
  if nearest_monitor
    .has_dpi_difference(&window_container.clone().into())?
  {
    window_container.set_has_pending_dpi_adjustment(true);
  }

  Ok(window_container)
}

/// Gets the initial state for a window based on its native state.
///
/// Note that maximized windows are initialized as tiling.
fn window_state_to_create(
  native_properties: &NativeWindowProperties,
  nearest_monitor: &Monitor,
  config: &UserConfig,
) -> anyhow::Result<WindowState> {
  if native_properties.is_minimized {
    return Ok(WindowState::Minimized);
  }

  let nearest_workspace = nearest_monitor
    .displayed_workspace()
    .context("No workspace.")?;

  // Only initialize as fullscreen if the window *exceeds* the workspace
  // bounds (due to the 1px inset).
  //
  // For example, with 0px outer gaps and a window that covers the entire
  // workspace, it would still not be initialized as fullscreen. The window
  // needs to be within the workspace's outer gaps by at least 1px on each
  // side.
  if !native_properties.is_maximized
    && native_properties
      .frame
      .inset(1)
      .contains_rect(&nearest_workspace.max_workspace_rect()?)
  {
    return Ok(WindowState::Fullscreen(
      config
        .value
        .window_behavior
        .state_defaults
        .fullscreen
        .clone(),
    ));
  }

  // Initialize windows that can't be resized as floating.
  if !native_properties.is_resizable {
    return Ok(WindowState::Floating(
      config.value.window_behavior.state_defaults.floating.clone(),
    ));
  }

  Ok(WindowState::default_from_config(&config.value))
}

/// Gets where to insert a new window in the container tree.
///
/// Rules:
/// - For non-tiling windows: Always append to the workspace.
/// - For tiling windows:
///   1. Try to insert after the focused tiling window (or its parent
///      stack) if one exists.
///   2. If a non-tiling window is focused, try to insert after the first
///      tiling window found.
///   3. If no tiling windows exist, append to the workspace.
///
/// New windows are never inserted into an existing `StackContainer`
/// automatically. Stacking is always an explicit user action via
/// `stack-insert` or `stack-absorb-neighbor`.
///
/// Returns tuple of (parent container, insertion index).
fn insertion_target(
  window_state: &WindowState,
  state: &WmState,
) -> anyhow::Result<(Container, usize)> {
  let focused_container =
    state.focused_container().context("No focused container.")?;

  let focused_workspace =
    focused_container.workspace().context("No workspace.")?;

  // For tiling windows, try to find a suitable tiling window to insert
  // next to.
  if *window_state == WindowState::Tiling {
    let sibling = match focused_container {
      Container::TilingWindow(_) => Some(focused_container),
      _ => focused_workspace
        .descendant_focus_order()
        .find(Container::is_tiling_window),
    };

    if let Some(sibling) = sibling {
      let parent = sibling.parent().context("No parent.")?;

      // If the sibling lives inside a stack, insert after the stack itself
      // rather than into it — automatic stacking is always explicit.
      if parent.as_stack().is_some() {
        let stack_parent = parent.parent().context("No stack parent.")?;
        return Ok((stack_parent, parent.index() + 1));
      }

      return Ok((parent, sibling.index() + 1));
    }
  }

  // Default to appending to workspace.
  Ok((
    focused_workspace.clone().into(),
    focused_workspace.child_count(),
  ))
}

#[cfg(test)]
mod tests {
  use std::collections::HashSet;
  #[cfg(target_os = "windows")]
  use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
  };

  #[cfg(target_os = "windows")]
  use wm_platform::WindowId;

  use super::{next_manage_retry_attempt, remove_pending_manage_retry};
  #[cfg(target_os = "windows")]
  use super::{should_early_cloak, EarlyCloakLease, EarlyWindowTraits};

  #[test]
  fn native_property_retries_are_bounded() {
    assert_eq!(next_manage_retry_attempt(0), Some(1));
    assert_eq!(next_manage_retry_attempt(1), Some(2));
    assert_eq!(next_manage_retry_attempt(2), Some(3));
    assert_eq!(next_manage_retry_attempt(3), None);
  }

  #[test]
  fn destroyed_hwnd_cancels_pending_manage_retry() {
    let destroyed = wm_platform::WindowId(42);
    let retained = wm_platform::WindowId(84);
    let mut pending = HashSet::from([destroyed, retained]);

    assert!(remove_pending_manage_retry(&mut pending, destroyed));
    assert!(!remove_pending_manage_retry(&mut pending, destroyed));
    assert_eq!(pending, HashSet::from([retained]));
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn cheap_rejects_are_never_early_cloaked() {
    assert!(!should_early_cloak(EarlyWindowTraits {
      is_child: true,
      ..Default::default()
    }));
    assert!(!should_early_cloak(EarlyWindowTraits {
      is_tool_or_no_activate: true,
      ..Default::default()
    }));
    assert!(!should_early_cloak(EarlyWindowTraits {
      is_owned_without_caption: true,
      ..Default::default()
    }));
    assert!(should_early_cloak(EarlyWindowTraits::default()));
  }

  #[cfg(target_os = "windows")]
  fn test_uncloak_action(
    cloaked: &Arc<AtomicBool>,
    calls: &Arc<AtomicUsize>,
  ) -> Arc<dyn Fn() + Send + Sync> {
    let cloaked = Arc::clone(cloaked);
    let calls = Arc::clone(calls);
    Arc::new(move || {
      cloaked.store(false, Ordering::Release);
      calls.fetch_add(1, Ordering::AcqRel);
    })
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn cloak_lease_restores_visibility_once_on_failure_or_repetition() {
    let cloaked = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let action = test_uncloak_action(&cloaked, &calls);
    let active = AtomicBool::new(true);

    assert!(EarlyCloakLease::release_shared(&active, &action));
    assert!(!EarlyCloakLease::release_shared(&active, &action));
    assert!(!cloaked.load(Ordering::Acquire));
    assert_eq!(calls.load(Ordering::Acquire), 1);
  }

  #[cfg(target_os = "windows")]
  #[tokio::test]
  async fn cloak_lease_timeout_restores_visibility() {
    let cloaked = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let lease = EarlyCloakLease::with_timeout(
      test_uncloak_action(&cloaked, &calls),
      std::time::Duration::from_millis(1),
      WindowId(42),
    );

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(!cloaked.load(Ordering::Acquire));
    assert_eq!(calls.load(Ordering::Acquire), 1);
    drop(lease);
    assert_eq!(calls.load(Ordering::Acquire), 1);
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn successful_transfer_leaves_visibility_to_platform_sync() {
    let cloaked = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let lease = EarlyCloakLease {
      active: Arc::new(AtomicBool::new(true)),
      uncloak: test_uncloak_action(&cloaked, &calls),
    };

    lease.transfer();
    assert!(cloaked.load(Ordering::Acquire));
    assert_eq!(calls.load(Ordering::Acquire), 0);
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn repeated_failed_creations_never_leave_a_cloak_owned() {
    for _ in 0..128 {
      let cloaked = Arc::new(AtomicBool::new(true));
      let calls = Arc::new(AtomicUsize::new(0));
      let lease = EarlyCloakLease {
        active: Arc::new(AtomicBool::new(true)),
        uncloak: test_uncloak_action(&cloaked, &calls),
      };

      drop(lease);
      assert!(!cloaked.load(Ordering::Acquire));
      assert_eq!(calls.load(Ordering::Acquire), 1);
    }
  }
}
