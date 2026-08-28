use std::{
  collections::{HashMap, HashSet},
  time::Instant,
};
#[cfg(target_os = "windows")]
use std::{
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
  },
  time::Duration,
};

use anyhow::Context;
use tokio::sync::mpsc::{self};
use tracing::warn;
use uuid::Uuid;
use wm_common::{
  BindingModeConfig, DisplayState, HideCorner, WindowState, WmEvent,
  WorkspaceLayout,
};
use wm_platform::{
  Direction, Dispatcher, Display, NativeWindow, Point, Rect, WindowId,
};
#[cfg(target_os = "windows")]
use wm_platform::{
  NativeBlurOverlay, NativeStackTabBar, NativeWindowWindowsExt,
  OpacityValue,
};

use crate::{
  animation::AnimationManager,
  commands::{
    container::set_focused_descendant,
    general::platform_sync,
    monitor::{add_monitor, move_bounded_workspaces_to_new_monitor},
    window::{manage_window, unmanage_window},
  },
  models::{
    Container, Monitor, NativeMonitorProperties, RootContainer,
    WindowContainer, Workspace, WorkspaceTarget,
  },
  pending_sync::PendingSync,
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
};

/// One bounded incompatible workspace destination being prepared beneath
/// retained surrogate coverage.
///
/// Logical focus/display state and IPC remain on `public_origin_name`
/// until the destination's real windows have completed a successful native
/// redraw. A later command replaces `destination_name`; relative targets
/// resolve from that absolute name as their virtual base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingWorkspaceRedirect {
  pub public_origin_name: String,
  pub destination_name: String,
  pub order_direction: i32,
}

#[cfg(target_os = "windows")]
const EARLY_CLOAK_TIMEOUT_MS: u64 = 250;

/// Owns one early DWM cloak until platform synchronization has either
/// transferred visibility to an animation/native hide operation or
/// restored the real HWND visibly.
///
/// The lease remains in [`WmState`] after classification so a later
/// fallible platform-sync operation cannot strand the HWND. Its timeout is
/// independent of the WM event loop and uses one idempotent atomic release
/// path.
#[cfg(target_os = "windows")]
pub(crate) struct VisibilityLease {
  expected_window_id: WindowId,
  active: Arc<AtomicBool>,
  restore: Arc<dyn Fn() + Send + Sync>,
  fallback_rect: Arc<Mutex<Option<Rect>>>,
}

#[cfg(target_os = "windows")]
impl VisibilityLease {
  pub(crate) fn arm(native_window: &NativeWindow) -> Option<Self> {
    if let Err(err) = native_window.set_cloaked(true) {
      warn!(
        "Failed to early-cloak window {:?}; continuing visibly: {err:#}",
        native_window.id(),
      );
      return None;
    }

    let fallback_rect = Arc::new(Mutex::new(None));
    let restore_rect = Arc::clone(&fallback_rect);
    let window_to_restore = native_window.clone();
    let expected_window_id = native_window.id();
    let restore: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
      if !window_to_restore.is_valid() {
        return;
      }

      let target_rect = restore_rect
        .lock()
        .ok()
        .and_then(|rect| rect.as_ref().cloned());
      if let Some(target_rect) = target_rect {
        if let Err(err) = window_to_restore.set_frame(&target_rect) {
          warn!(
            "Failed to place early-cloaked window {expected_window_id:?} at its safe fallback rect: {err:#}"
          );
        }
      }
      if let Err(err) = window_to_restore
        .set_transparency(&OpacityValue::from_alpha(u8::MAX))
      {
        warn!(
          "Failed to restore opaque visibility for early-cloaked window {expected_window_id:?}: {err:#}"
        );
      }
      if let Err(err) = window_to_restore.set_cloaked(false) {
        warn!(
          "Failed to uncloak early-cloaked window {expected_window_id:?}: {err:#}"
        );
      }
      if let Err(err) = window_to_restore.show() {
        warn!(
          "Failed to show early-cloaked window {expected_window_id:?}: {err:#}"
        );
      }
    });

    Some(Self::with_timeout(
      expected_window_id,
      restore,
      fallback_rect,
      Duration::from_millis(EARLY_CLOAK_TIMEOUT_MS),
    ))
  }

  fn with_timeout(
    expected_window_id: WindowId,
    restore: Arc<dyn Fn() + Send + Sync>,
    fallback_rect: Arc<Mutex<Option<Rect>>>,
    timeout: Duration,
  ) -> Self {
    let active = Arc::new(AtomicBool::new(true));
    let timeout_active = Arc::clone(&active);
    let timeout_restore = Arc::clone(&restore);

    tokio::spawn(async move {
      tokio::time::sleep(timeout).await;
      if Self::release_shared(&timeout_active, &timeout_restore) {
        warn!(
          "Early visibility handoff timed out for window {expected_window_id:?}; restored the real HWND visibly."
        );
      }
    });

    Self {
      expected_window_id,
      active,
      restore,
      fallback_rect,
    }
  }

  #[cfg(test)]
  pub(crate) fn for_test(
    expected_window_id: WindowId,
    restore: Arc<dyn Fn() + Send + Sync>,
    timeout: Duration,
  ) -> Self {
    Self::with_timeout(
      expected_window_id,
      restore,
      Arc::new(Mutex::new(None)),
      timeout,
    )
  }

  pub(crate) fn release_shared(
    active: &AtomicBool,
    restore: &Arc<dyn Fn() + Send + Sync>,
  ) -> bool {
    if active.swap(false, Ordering::AcqRel) {
      restore();
      true
    } else {
      false
    }
  }

  pub(crate) fn expected_window_id(&self) -> WindowId {
    self.expected_window_id
  }

  fn is_active(&self) -> bool {
    self.active.load(Ordering::Acquire)
  }

  fn set_fallback_rect(&self, rect: Rect) {
    if let Ok(mut fallback_rect) = self.fallback_rect.lock() {
      *fallback_rect = Some(rect);
    }
  }

  fn restore(self) {
    Self::release_shared(&self.active, &self.restore);
  }

  pub(crate) fn transfer(self) {
    self.active.store(false, Ordering::Release);
  }

  fn cancel_destroyed(self) {
    self.active.store(false, Ordering::Release);
  }
}

#[cfg(target_os = "windows")]
impl Drop for VisibilityLease {
  fn drop(&mut self) {
    Self::release_shared(&self.active, &self.restore);
  }
}

pub struct WmState {
  /// Root node of the container tree. Monitors are the children of the
  /// root node, followed by workspaces, then split containers/windows.
  pub root_container: RootContainer,

  pub dispatcher: Dispatcher,

  pub pending_sync: PendingSync,

  /// Manager for window animations.
  pub animation_manager: AnimationManager,

  /// Tracks the target position for each window to prevent animation
  /// restart loops.
  pub window_target_positions: HashMap<Uuid, Rect>,

  /// Name of the most recently focused workspace.
  ///
  /// Used for the `general.toggle_workspace_on_refocus` option on
  /// workspace focus.
  pub recent_workspace_name: Option<String>,

  /// Latest absolute real-workspace underlay requested while incompatible
  /// workspace coverage is still retained. Bounded to exactly one entry.
  pub pending_workspace_redirect: Option<PendingWorkspaceRedirect>,

  /// Last selected layout for workspaces that may be deactivated and
  /// later recreated during this WM session.
  pub workspace_layouts: HashMap<String, WorkspaceLayout>,

  /// The previously focused window that had focus effects applied.
  ///
  /// Used to efficiently update window effects by only removing focus
  /// effects from the previous window rather than all windows when focus
  /// changes.
  pub prev_effects_window: Option<WindowContainer>,

  /// Time since a previously focused window was unmanaged or minimized.
  ///
  /// Used to decide whether to override incoming focus events.
  pub unmanaged_or_minimized_timestamp: Option<Instant>,

  /// Configs of currently enabled binding modes.
  pub binding_modes: Vec<BindingModeConfig>,

  /// Windows that the WM should ignore. Windows can be added via the
  /// `ignore` command.
  pub ignored_windows: Vec<NativeWindow>,

  /// Native windows with one delayed manageability retry already queued.
  ///
  /// Coalesces duplicate show notifications and prevents a failing HWND
  /// from spawning multiple retry chains.
  pub pending_window_manage_retries: HashSet<WindowId>,

  /// Early-cloaked managed windows whose visibility handoff has not yet
  /// completed. Bounded by the number of concurrently classifying windows
  /// and drained after the corresponding platform-sync pass.
  #[cfg(target_os = "windows")]
  pending_visibility_leases: HashMap<Uuid, VisibilityLease>,

  /// Sender for delayed native-window manageability retries.
  pub window_manage_retry_tx: mpsc::UnboundedSender<(NativeWindow, u8)>,

  /// Whether the WM is paused.
  pub is_paused: bool,

  /// Guards the transactional core of a config reload from recursive
  /// reload commands in window rules or post-commit hooks.
  pub is_reloading_config: bool,

  /// Whether the OS focused window is the same as the WM focused window.
  pub is_focus_synced: bool,

  /// Logical focus target awaiting one post-workspace-switch cursor
  /// refresh.
  ///
  /// The refresh is consumed only after a matching native foreground
  /// event confirms focus, so focus-follows-cursor cannot race a
  /// covered incoming window during the surrogate handoff.
  #[cfg(target_os = "windows")]
  pub pending_post_switch_cursor_refresh: Option<Uuid>,

  /// Incoming focused window whose workspace-switch surrogate already
  /// presented the focused effects. The matching native focus
  /// confirmation commits those effects without restarting from the
  /// unfocused opacity.
  #[cfg(target_os = "windows")]
  pub pending_presented_focus_effect_handoff: Option<Uuid>,

  /// Acrylic blur overlay windows keyed by managed-window UUID.
  ///
  /// Each overlay is a `WS_POPUP` window with
  /// `ACCENT_ENABLE_ACRYLICBLURBEHIND` applied via
  /// `SetWindowCompositionAttribute`, positioned at `HWND_BOTTOM` flush
  /// with the managed window's DWM frame rect. When the managed window
  /// is semi-transparent (`transparency` effect), the overlay's
  /// blurred-desktop content shows through, producing a frosted-glass
  /// look.
  #[cfg(target_os = "windows")]
  pub blur_overlays: HashMap<Uuid, NativeBlurOverlay>,

  /// Whether the initial state has been populated.
  has_initialized: bool,

  /// Sender for emitting WM-related events.
  event_tx: mpsc::UnboundedSender<WmEvent>,

  /// Sender for gracefully shutting down the WM.
  exit_tx: mpsc::UnboundedSender<()>,

  /// Sender half of the tab-click channel.
  ///
  /// Always present so that the channel stays open (preventing
  /// `tab_click_rx.recv()` from immediately returning `None`). Only
  /// actually used to send events on Windows when a tab is clicked.
  pub tab_click_tx: mpsc::UnboundedSender<(Uuid, usize)>,

  /// Live tab bar windows keyed by their `StackContainer` ID.
  #[cfg(target_os = "windows")]
  pub tab_bars: HashMap<Uuid, NativeStackTabBar>,
}

impl WmState {
  #[cfg(test)]
  pub(crate) fn mark_initialized_for_test(&mut self) {
    self.has_initialized = true;
  }

  pub fn new(
    dispatcher: Dispatcher,
    event_tx: mpsc::UnboundedSender<WmEvent>,
    exit_tx: mpsc::UnboundedSender<()>,
    animation_tick_tx: mpsc::Sender<()>,
    window_manage_retry_tx: mpsc::UnboundedSender<(NativeWindow, u8)>,
    tab_click_tx: mpsc::UnboundedSender<(Uuid, usize)>,
  ) -> Self {
    Self {
      root_container: RootContainer::new(),
      dispatcher,
      pending_sync: PendingSync::default(),
      animation_manager: AnimationManager::new(animation_tick_tx),
      window_target_positions: HashMap::new(),
      workspace_layouts: HashMap::new(),
      #[cfg(target_os = "windows")]
      blur_overlays: HashMap::new(),
      prev_effects_window: None,
      recent_workspace_name: None,
      pending_workspace_redirect: None,
      unmanaged_or_minimized_timestamp: None,
      binding_modes: Vec::new(),
      ignored_windows: Vec::new(),
      pending_window_manage_retries: HashSet::new(),
      #[cfg(target_os = "windows")]
      pending_visibility_leases: HashMap::new(),
      window_manage_retry_tx,
      is_paused: false,
      is_reloading_config: false,
      is_focus_synced: false,
      #[cfg(target_os = "windows")]
      pending_post_switch_cursor_refresh: None,
      #[cfg(target_os = "windows")]
      pending_presented_focus_effect_handoff: None,
      has_initialized: false,
      event_tx,
      exit_tx,
      tab_click_tx,
      #[cfg(target_os = "windows")]
      tab_bars: HashMap::new(),
    }
  }

  /// Keeps an early cloak owned by WM state until the complete native
  /// synchronization pass establishes a replacement visibility owner.
  #[cfg(target_os = "windows")]
  pub(crate) fn register_visibility_lease(
    &mut self,
    window_id: Uuid,
    lease: VisibilityLease,
  ) {
    let current_window = self
      .container_by_id(window_id)
      .and_then(|container| container.as_window_container().ok());
    let Some(current_window) = current_window else {
      lease.restore();
      return;
    };

    if current_window.native().id() != lease.expected_window_id() {
      warn!(
        "Refused stale early-cloak ownership for window {window_id}; the managed HWND no longer matches {:?}.",
        lease.expected_window_id()
      );
      lease.cancel_destroyed();
      return;
    }

    if let Some(target_rect) = self
      .window_target_positions
      .get(&window_id)
      .cloned()
      .or_else(|| current_window.to_rect().ok())
    {
      lease.set_fallback_rect(target_rect);
    }

    if let Some(replaced) =
      self.pending_visibility_leases.insert(window_id, lease)
    {
      warn!(
        "Replaced duplicate early visibility lease for window {window_id}; restored the older owner visibly."
      );
      replaced.restore();
    }
  }

  /// Settles every early visibility lease after a platform-sync attempt.
  ///
  /// An incomplete deferred sync retains active leases. A failed sync
  /// restores every live matching HWND. A complete successful sync
  /// transfers ownership only when the native/animation path
  /// demonstrably owns the window's current visibility; otherwise it
  /// takes the visible fallback.
  #[cfg(target_os = "windows")]
  pub(crate) fn settle_visibility_leases(
    &mut self,
    sync_succeeded: bool,
    sync_complete: bool,
  ) {
    let leases = std::mem::take(&mut self.pending_visibility_leases);

    for (window_id, lease) in leases {
      if !lease.is_active() {
        continue;
      }

      let current_window = self
        .container_by_id(window_id)
        .and_then(|container| container.as_window_container().ok());
      let Some(current_window) = current_window else {
        lease.cancel_destroyed();
        continue;
      };
      if current_window.native().id() != lease.expected_window_id() {
        warn!(
          "Canceled stale early visibility lease for window {window_id}; its HWND identity changed before handoff."
        );
        lease.cancel_destroyed();
        continue;
      }

      if !sync_succeeded {
        warn!(
          "Restoring early-cloaked window {window_id} after platform synchronization failed."
        );
        lease.restore();
        continue;
      }
      if !sync_complete {
        self.pending_visibility_leases.insert(window_id, lease);
        continue;
      }

      let is_logically_visible = matches!(
        current_window.display_state(),
        DisplayState::Showing | DisplayState::Shown
      );
      let surrogate_owns_visibility =
        self.animation_manager.has_active_surrogate(&window_id);
      let real_window_is_uncloaked = if is_logically_visible
        && !surrogate_owns_visibility
      {
        match current_window.native().is_cloaked() {
          Ok(is_cloaked) => !is_cloaked,
          Err(err) => {
            warn!(
              "Failed to verify early visibility handoff for window {window_id}: {err:#}; using visible fallback."
            );
            false
          }
        }
      } else {
        false
      };

      if !is_logically_visible
        || surrogate_owns_visibility
        || real_window_is_uncloaked
      {
        lease.transfer();
      } else {
        warn!(
          "Platform synchronization completed without a visibility owner for early-cloaked window {window_id}; using visible fallback."
        );
        lease.restore();
      }
    }
  }

  /// Cancels an early visibility owner for a destroyed logical window
  /// without touching a potentially recycled numeric HWND.
  #[cfg(target_os = "windows")]
  pub(crate) fn cancel_visibility_lease(
    &mut self,
    window_id: Uuid,
    expected_handle: isize,
  ) -> bool {
    let matches_expected =
      self.pending_visibility_leases.get(&window_id).is_some_and(
        |lease| lease.expected_window_id().0 == expected_handle,
      );
    if !matches_expected {
      return false;
    }

    if let Some(lease) = self.pending_visibility_leases.remove(&window_id)
    {
      lease.cancel_destroyed();
      return true;
    }
    false
  }

  /// Restores a live matching window before it is intentionally unmanaged.
  #[cfg(target_os = "windows")]
  pub(crate) fn restore_visibility_lease(
    &mut self,
    window_id: Uuid,
    expected_handle: isize,
  ) -> bool {
    let matches_expected =
      self.pending_visibility_leases.get(&window_id).is_some_and(
        |lease| lease.expected_window_id().0 == expected_handle,
      );
    if !matches_expected {
      return false;
    }

    if let Some(lease) = self.pending_visibility_leases.remove(&window_id)
    {
      lease.restore();
      return true;
    }
    false
  }

  /// Cancels any pre-management lease matching a destroyed native HWND.
  #[cfg(target_os = "windows")]
  pub(crate) fn cancel_visibility_lease_for_native(
    &mut self,
    native_window_id: WindowId,
  ) -> bool {
    let matching_id =
      self
        .pending_visibility_leases
        .iter()
        .find_map(|(id, lease)| {
          (lease.expected_window_id() == native_window_id).then_some(*id)
        });
    matching_id.is_some_and(|id| {
      self.cancel_visibility_lease(id, native_window_id.0)
    })
  }

  #[cfg(all(test, target_os = "windows"))]
  pub(crate) fn pending_visibility_lease_count(&self) -> usize {
    self.pending_visibility_leases.len()
  }

  /// Populates the initial WM state by creating containers for all
  /// existing windows and monitors.
  pub fn populate(
    &mut self,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    // Get the originally focused window when the WM was started.
    let focused_window = self.dispatcher.focused_window().ok();

    // Create a monitor, and consequently a workspace, for each detected
    // native monitor.
    for native_display in self.dispatcher.sorted_displays()? {
      if let Ok(native_properties) =
        NativeMonitorProperties::try_from(&native_display)
      {
        let monitor =
          add_monitor(native_display, native_properties, self)?;
        move_bounded_workspaces_to_new_monitor(&monitor, self, config)?;
      }
    }

    // Manage windows in reverse z-order (bottom to top). This helps to
    // preserve the original stacking order.
    for native_window in
      self.dispatcher.visible_windows()?.into_iter().rev()
    {
      let nearest_workspace = self
        .nearest_monitor(&native_window)
        .and_then(|m| m.displayed_workspace());

      if let Some(workspace) = nearest_workspace {
        manage_window(
          native_window,
          Some(workspace.into()),
          self,
          config,
        )?;
      }
    }

    let container_to_focus = focused_window
      .and_then(|focused_window| {
        self.window_from_native(&focused_window).map(Into::into)
      })
      .or_else(|| self.windows().pop().map(Into::into))
      .or_else(|| self.workspaces().pop().map(Into::into))
      .context("Failed to get container to focus.")?;

    set_focused_descendant(&container_to_focus, None);
    self.is_focus_synced = true;

    self
      .pending_sync
      .queue_focus_change()
      .queue_all_effects_update();

    for workspace in self.workspaces() {
      self.pending_sync.queue_workspace_to_reorder(workspace);
    }

    platform_sync(self, config)?;
    self.has_initialized = true;

    Ok(())
  }

  pub fn monitors(&self) -> Vec<Monitor> {
    self.root_container.monitors()
  }

  pub fn workspaces(&self) -> Vec<Workspace> {
    self
      .monitors()
      .iter()
      .flat_map(Monitor::workspaces)
      .collect()
  }

  /// Gets workspaces sorted by their position in the user config.
  pub fn sorted_workspaces(&self, config: &UserConfig) -> Vec<Workspace> {
    let mut workspaces = self.workspaces();
    config.sort_workspaces(&mut workspaces);
    workspaces
  }

  pub fn windows(&self) -> Vec<WindowContainer> {
    self
      .root_container
      .descendants()
      .filter_map(|container| container.try_into().ok())
      .collect()
  }

  /// Gets the monitor that encompasses the largest portion of a given
  /// window.
  ///
  /// Defaults to the first monitor if the nearest monitor is invalid.
  pub fn nearest_monitor(
    &self,
    native_window: &NativeWindow,
  ) -> Option<Monitor> {
    self
      .monitor_from_native(
        &self.dispatcher.nearest_display(native_window).ok()?,
      )
      .or(self.monitors().first().cloned())
  }

  /// Gets monitor that corresponds to the given `Display`.
  pub fn monitor_from_native(
    &self,
    native_display: &Display,
  ) -> Option<Monitor> {
    self
      .monitors()
      .into_iter()
      .find(|monitor| monitor.native() == *native_display)
  }

  /// Gets the closest monitor in a given direction.
  ///
  /// Uses i3wm's algorithm for finding best guess.
  pub fn monitor_in_direction(
    &self,
    origin_monitor: &Monitor,
    direction: &Direction,
  ) -> anyhow::Result<Option<Monitor>> {
    let origin_rect = origin_monitor.native_properties().bounds;

    // Create a tuple of monitors and their rect.
    let monitors_with_rect = self
      .monitors()
      .into_iter()
      .map(|monitor| {
        let rect = monitor.native_properties().bounds;
        anyhow::Ok((monitor, rect))
      })
      .try_collect::<Vec<_>>()?;

    let closest_monitor = monitors_with_rect
      .into_iter()
      .filter(|(_, rect)| match direction {
        Direction::Right => {
          rect.x() > origin_rect.x() && rect.y_overlap(&origin_rect) > 0
        }
        Direction::Left => {
          rect.x() < origin_rect.x() && rect.y_overlap(&origin_rect) > 0
        }
        Direction::Down => {
          rect.y() > origin_rect.y() && rect.x_overlap(&origin_rect) > 0
        }
        Direction::Up => {
          rect.y() < origin_rect.y() && rect.x_overlap(&origin_rect) > 0
        }
      })
      .min_by(|(_, rect_a), (_, rect_b)| match direction {
        Direction::Right => rect_a.x().cmp(&rect_b.x()),
        Direction::Left => rect_b.x().cmp(&rect_a.x()),
        Direction::Down => rect_a.y().cmp(&rect_b.y()),
        Direction::Up => rect_b.y().cmp(&rect_a.y()),
      })
      .map(|(monitor, _)| monitor);

    Ok(closest_monitor)
  }

  /// Determines the preferred hide corner for each monitor. Used for
  /// [`HideMethod::PlaceInCorner`].
  ///
  /// The corner is chosen by simulating a 400x400 window frame in the
  /// bottom-left and bottom-right of the monitor's working area, then
  /// picking the side that overlaps the least with other monitors'
  /// working areas (ties favor bottom-right).
  pub fn monitors_by_hide_corner(&self) -> Vec<(Monitor, HideCorner)> {
    const TEST_FRAME_SIZE: i32 = 400;
    const VISIBLE_SLIVER: i32 = 1;

    let monitors = self.monitors();
    let working_areas = monitors
      .iter()
      .map(|monitor| monitor.native_properties().working_area)
      .collect::<Vec<_>>();

    monitors
      .into_iter()
      .enumerate()
      .map(|(idx, monitor)| {
        let monitor_rect = &working_areas[idx];
        let test_frame_y = monitor_rect.bottom - TEST_FRAME_SIZE;

        let left_test_frame = Rect::from_xy(
          monitor_rect.left - TEST_FRAME_SIZE + VISIBLE_SLIVER,
          test_frame_y,
          TEST_FRAME_SIZE,
          TEST_FRAME_SIZE,
        );

        let right_test_frame = Rect::from_xy(
          monitor_rect.right - VISIBLE_SLIVER,
          test_frame_y,
          TEST_FRAME_SIZE,
          TEST_FRAME_SIZE,
        );

        let overlap_area = |test_frame: &Rect| -> i32 {
          working_areas
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != idx)
            .map(|(_, rect)| test_frame.intersection_area(rect))
            .sum()
        };

        let left_overlap = overlap_area(&left_test_frame);
        let right_overlap = overlap_area(&right_test_frame);

        let corner = if left_overlap < right_overlap {
          HideCorner::BottomLeft
        } else {
          HideCorner::BottomRight
        };

        (monitor, corner)
      })
      .collect()
  }

  /// Gets window that corresponds to the given `NativeWindow`.
  pub fn window_from_native(
    &self,
    native_window: &NativeWindow,
  ) -> Option<WindowContainer> {
    self
      .windows()
      .into_iter()
      .find(|window| &*window.native() == native_window)
  }

  pub fn workspace_by_name(
    &self,
    workspace_name: &str,
  ) -> Option<Workspace> {
    self
      .workspaces()
      .into_iter()
      .find(|workspace| workspace.config().name == workspace_name)
  }

  /// Gets a workspace and its name by the given target.
  ///
  /// Returns a tuple of the workspace name and the `Workspace` instance
  /// if active.
  #[allow(clippy::too_many_lines)]
  pub fn workspace_by_target(
    &self,
    origin_workspace: &Workspace,
    target: WorkspaceTarget,
    config: &UserConfig,
  ) -> anyhow::Result<(Option<String>, Option<Workspace>)> {
    let (name, workspace) = match target {
      WorkspaceTarget::Name(name) => {
        #[allow(clippy::match_bool)]
        match origin_workspace.config().name == name {
          false => (Some(name.clone()), self.workspace_by_name(&name)),
          // Toggle the workspace if it's already focused.
          true if config.value.general.toggle_workspace_on_refocus => (
            self.recent_workspace_name.clone(),
            self
              .recent_workspace_name
              .as_ref()
              .and_then(|name| self.workspace_by_name(name)),
          ),
          true => (None, None),
        }
      }
      WorkspaceTarget::Recent => (
        self.recent_workspace_name.clone(),
        self
          .recent_workspace_name
          .as_ref()
          .and_then(|name| self.workspace_by_name(name)),
      ),
      WorkspaceTarget::NextActive => {
        let active_workspaces = self.sorted_workspaces(config);
        let origin_index = active_workspaces
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of given workspace.")?;

        let next_active_workspace = active_workspaces
          .get(origin_index + 1)
          .or_else(|| active_workspaces.first());

        (
          next_active_workspace.map(|workspace| workspace.config().name),
          next_active_workspace.cloned(),
        )
      }
      WorkspaceTarget::PreviousActive => {
        let active_workspaces = self.sorted_workspaces(config);
        let origin_index = active_workspaces
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of given workspace.")?;

        let prev_active_workspace = active_workspaces.get(
          origin_index
            .checked_sub(1)
            .unwrap_or(active_workspaces.len() - 1),
        );

        (
          prev_active_workspace.map(|workspace| workspace.config().name),
          prev_active_workspace.cloned(),
        )
      }
      WorkspaceTarget::NextActiveInMonitor => {
        let monitor = origin_workspace
          .monitor()
          .context("No monitor in workspace")?;

        let mut workspace_in_monitor = monitor.workspaces();
        config.sort_workspaces(&mut workspace_in_monitor);

        let origin_index = workspace_in_monitor
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of give workspace")?;

        let next_active_workspace_in_monitor = workspace_in_monitor
          .get(origin_index + 1)
          .or_else(|| workspace_in_monitor.first());

        (
          next_active_workspace_in_monitor
            .map(|workspace| workspace.config().name),
          next_active_workspace_in_monitor.cloned(),
        )
      }
      WorkspaceTarget::PreviousActiveInMonitor => {
        let monitor = origin_workspace
          .monitor()
          .context("No monitor in workspace")?;

        let mut workspace_in_monitor = monitor.workspaces();
        config.sort_workspaces(&mut workspace_in_monitor);

        let origin_index = workspace_in_monitor
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of give workspace")?;

        let prev_active_workspace_in_monitor = workspace_in_monitor.get(
          origin_index
            .checked_sub(1)
            .unwrap_or(workspace_in_monitor.len() - 1),
        );

        (
          prev_active_workspace_in_monitor
            .map(|workspace| workspace.config().name),
          prev_active_workspace_in_monitor.cloned(),
        )
      }
      WorkspaceTarget::Next => {
        let workspaces = &config.value.workspaces;
        let origin_name = origin_workspace.config().name.clone();
        let origin_index = workspaces
          .iter()
          .position(|workspace| workspace.name == origin_name)
          .context("Failed to get index of given workspace.")?;

        let next_workspace_config = workspaces
          .get(origin_index + 1)
          .or_else(|| workspaces.first());

        let next_workspace_name =
          next_workspace_config.map(|config| config.name.clone());

        let next_workspace = next_workspace_name
          .as_ref()
          .and_then(|name| self.workspace_by_name(name));

        (next_workspace_name, next_workspace)
      }
      WorkspaceTarget::Previous => {
        let workspaces = &config.value.workspaces;
        let origin_name = origin_workspace.config().name.clone();
        let origin_index = workspaces
          .iter()
          .position(|workspace| workspace.name == origin_name)
          .context("Failed to get index of given workspace.")?;

        let previous_workspace_config = workspaces.get(
          origin_index.checked_sub(1).unwrap_or(workspaces.len() - 1),
        );

        let previous_workspace_name =
          previous_workspace_config.map(|config| config.name.clone());

        let previous_workspace = previous_workspace_name
          .as_ref()
          .and_then(|name| self.workspace_by_name(name));

        (previous_workspace_name, previous_workspace)
      }

      WorkspaceTarget::Direction(direction) => {
        let origin_monitor =
          origin_workspace.monitor().context("No focused monitor.")?;

        let target_workspace = self
          .monitor_in_direction(&origin_monitor, &direction)?
          .and_then(|monitor| monitor.displayed_workspace());

        (
          target_workspace
            .as_ref()
            .map(|workspace| workspace.config().name),
          target_workspace,
        )
      }
    };

    Ok((name, workspace))
  }

  /// Gets windows that should be redrawn.
  ///
  /// When redrawing after a command that changes a window's type (e.g.
  /// tiling -> floating), the original detached window might still be
  /// queued for a redraw and should be filtered out.
  pub fn windows_to_redraw(&self) -> Vec<WindowContainer> {
    self
      .pending_sync
      .containers_to_redraw()
      .values()
      .flat_map(CommonGetters::self_and_descendants)
      .filter(|container| !container.is_detached())
      .filter_map(|container| container.try_into().ok())
      .collect()
  }

  /// Gets the currently focused container. This can either be a window or
  /// a workspace without any descendant windows.
  pub fn focused_container(&self) -> Option<Container> {
    self.root_container.descendant_focus_order().next()
  }

  /// Emits a WM event through an MSPC channel.
  ///
  /// Does not emit events while the WM is paused or populating initial
  /// state. This is to prevent events (e.g. workspace activation events)
  /// from being emitted via IPC server before the initial state is
  /// prepared.
  pub fn emit_event(&self, event: WmEvent) {
    if self.has_initialized
      && (!self.is_paused || matches!(event, WmEvent::PauseChanged { .. }))
    {
      if let Err(err) = self.event_tx.send(event) {
        warn!("Failed to send event: {}", err);
      }
    }
  }

  /// Starts graceful shutdown via an MSPC channel.
  pub fn emit_exit(&self) -> anyhow::Result<()> {
    self.exit_tx.send(())?;
    Ok(())
  }

  pub fn container_by_id(&self, id: Uuid) -> Option<Container> {
    self
      .root_container
      .self_and_descendants()
      .find(|container| container.id() == id)
  }

  /// Gets container to focus after the given window is unmanaged,
  /// minimized, or moved to another workspace.
  pub fn focus_target_after_removal(
    &self,
    removed_window: &WindowContainer,
  ) -> Option<Container> {
    // If the removed window is not focused, no need to change focus.
    if self.focused_container() != Some(removed_window.clone().into()) {
      return None;
    }

    // Get descendant focus order excluding the removed container.
    let workspace = removed_window.workspace()?;
    let descendant_focus_order = workspace
      .descendant_focus_order()
      .filter(|descendant| descendant.id() != removed_window.id())
      .collect::<Vec<_>>();

    // Get focus target that matches the removed window type. This applies
    // for windows that aren't in a minimized state.
    let focus_target_of_type = descendant_focus_order
      .iter()
      .filter_map(|descendant| descendant.as_window_container().ok())
      .find(|descendant| {
        matches!(
          (descendant.state(), removed_window.state()),
          (WindowState::Tiling, WindowState::Tiling)
            | (WindowState::Floating(_), WindowState::Floating(_))
            | (WindowState::Fullscreen(_), WindowState::Fullscreen(_))
        )
      })
      .map(Into::into);

    if focus_target_of_type.is_some() {
      return focus_target_of_type;
    }

    let non_minimized_focus_target = descendant_focus_order
      .iter()
      .filter_map(|descendant| descendant.as_window_container().ok())
      .find(|descendant| descendant.state() != WindowState::Minimized)
      .map(Into::into);

    non_minimized_focus_target
      .or(descendant_focus_order.first().cloned())
      .or(Some(workspace.into()))
  }

  /// Returns all containers that contain the given point.
  #[allow(clippy::unused_self)]
  pub fn containers_at_point(
    &self,
    origin_container: &Container,
    point: &Point,
  ) -> Vec<Container> {
    origin_container
      .descendants()
      .filter(|descendant| {
        descendant
          .to_rect()
          .is_ok_and(|rect| rect.contains_point(point))
      })
      .collect()
  }

  /// Returns the monitor that contains the given point.
  pub fn monitor_at_point(&self, point: &Point) -> Option<Monitor> {
    self
      .monitors()
      .iter()
      .find(|monitor| {
        monitor
          .to_rect()
          .is_ok_and(|rect| rect.contains_point(point))
      })
      .cloned()
  }

  /// Cleans up windows that are no longer alive.
  ///
  /// This addresses the "ghost window" issue where applications may
  /// terminate without sending window destroy events, leaving invalid
  /// windows in WM state.
  ///
  /// See: <https://github.com/glzr-io/glazewm/issues/1219>
  pub fn cleanup_invalid_windows(&mut self) -> anyhow::Result<()> {
    let invalid_windows = self
      .windows()
      .into_iter()
      .filter(|window| !window.native().is_valid());

    for window in invalid_windows {
      tracing::info!("Removing invalid window: {}", window);
      unmanage_window(window, self)?;
    }

    // Prune ignored windows that are no longer valid.
    self.ignored_windows.retain(NativeWindow::is_valid);

    Ok(())
  }
}

impl Drop for WmState {
  fn drop(&mut self) {
    // Restore any early-cloaked HWND before tearing down animation/native
    // state. The lease release is idempotent with its independent timeout.
    #[cfg(target_os = "windows")]
    for (_, lease) in self.pending_visibility_leases.drain() {
      lease.restore();
    }

    // Commit all active resize sessions before cleaning up windows so that
    // surrogate overlays are destroyed and windows are moved to their
    // target positions. This prevents invisible or mispositioned
    // windows after a crash or forced exit.
    #[cfg(target_os = "windows")]
    for session in self.animation_manager.drain_all_sessions() {
      if let Err(err) = session.commit() {
        warn!("Failed to commit resize session on shutdown: {:?}", err);
      }
    }

    let managed_windows = self.windows();

    for window in &managed_windows {
      // Redraw windows to their intended positions. On macOS, this will
      // unhide windows that are on other workspaces.
      if let Ok(rect) = window.to_rect() {
        if let Err(err) = window.native().set_frame(&rect) {
          warn!("Failed to redraw window on cleanup: {:?}", err);
        }
      }

      // Reset any effects on Windows.
      #[cfg(target_os = "windows")]
      {
        // Uncloak before showing — a surrogate animation may have cloaked
        // this window. Without this, the window stays invisible after
        // exit.
        if let Err(err) = window.native().set_cloaked(false) {
          warn!("Failed to uncloak window on cleanup: {err:?}");
        }

        if let Err(err) = window.native().show() {
          warn!("Failed to show window: {:?}", err);
        }

        if let Err(err) = window.native().set_taskbar_visibility(true) {
          warn!(
            "Failed to restore taskbar visibility on cleanup: {err:?}"
          );
        }
        if let Err(err) = window.native().set_border_color(None) {
          warn!("Failed to reset border color on cleanup: {err:?}");
        }
        if let Err(err) = window
          .native()
          .set_transparency(&OpacityValue::from_alpha(u8::MAX))
        {
          warn!("Failed to restore opacity on cleanup: {err:?}");
        }
      }
    }
  }
}

#[cfg(test)]
mod workspace_identity_tests {
  use tokio::sync::mpsc;
  use wm_common::{ParsedConfig, WorkspaceConfig};
  use wm_platform::{Direction, Dispatcher, Rect};

  use super::WmState;
  use crate::{
    commands::container::attach_container,
    models::{Monitor, Workspace, WorkspaceTarget},
    traits::CommonGetters,
    user_config::UserConfig,
  };

  fn workspace_config(name: &str) -> WorkspaceConfig {
    WorkspaceConfig {
      name: name.to_string(),
      display_name: None,
      bind_to_monitor: None,
      keep_alive: false,
      default_layout: None,
    }
  }

  fn test_state() -> (WmState, UserConfig, Workspace, Workspace, Workspace)
  {
    let (event_tx, _) = mpsc::unbounded_channel();
    let (exit_tx, _) = mpsc::unbounded_channel();
    let (animation_tick_tx, _) = mpsc::channel(1);
    let (window_manage_retry_tx, _) = mpsc::unbounded_channel();
    let (tab_click_tx, _) = mpsc::unbounded_channel();
    let state = WmState::new(
      Dispatcher::mock(),
      event_tx,
      exit_tx,
      animation_tick_tx,
      window_manage_retry_tx,
      tab_click_tx,
    );

    let dev = Workspace::mock().name("Dev".to_string()).call();
    let lower_dev = Workspace::mock().name("dev".to_string()).call();
    let third = Workspace::mock().name("Third".to_string()).call();
    let left_monitor = Monitor::mock()
      .bounds(Rect::from_xy(0, 0, 1_600, 900))
      .workspaces(vec![dev.clone(), lower_dev.clone()])
      .call();
    let right_monitor = Monitor::mock()
      .bounds(Rect::from_xy(1_600, 0, 1_600, 900))
      .workspaces(vec![third.clone()])
      .call();

    for monitor in [left_monitor, right_monitor] {
      attach_container(
        &monitor.into(),
        &state.root_container.clone().into(),
        None,
      )
      .expect("test monitor should attach");
    }

    let mut parsed = ParsedConfig::default();
    parsed.workspaces = ["Dev", "dev", "Third"]
      .into_iter()
      .map(workspace_config)
      .collect();
    let config = UserConfig::for_test(parsed);

    (state, config, dev, lower_dev, third)
  }

  #[test]
  fn direct_recent_relative_and_directional_targets_use_exact_identity() {
    let (mut state, config, dev, lower_dev, third) = test_state();

    let (name, direct) = state
      .workspace_by_target(
        &dev,
        WorkspaceTarget::Name("dev".to_string()),
        &config,
      )
      .expect("direct target should resolve");
    assert_eq!(name.as_deref(), Some("dev"));
    assert_eq!(
      direct.expect("lowercase target should exist").id(),
      lower_dev.id()
    );

    state.recent_workspace_name = Some("dev".to_string());
    let (name, recent) = state
      .workspace_by_target(&dev, WorkspaceTarget::Recent, &config)
      .expect("recent target should resolve");
    assert_eq!(name.as_deref(), Some("dev"));
    assert_eq!(
      recent.expect("recent target should exist").id(),
      lower_dev.id()
    );

    let (name, next) = state
      .workspace_by_target(&lower_dev, WorkspaceTarget::Next, &config)
      .expect("next target should resolve from exact config identity");
    assert_eq!(name.as_deref(), Some("Third"));
    assert_eq!(
      next.expect("next target should be active").id(),
      third.id()
    );

    let (name, previous) = state
      .workspace_by_target(&dev, WorkspaceTarget::Previous, &config)
      .expect("previous target should wrap from exact config identity");
    assert_eq!(name.as_deref(), Some("Third"));
    assert_eq!(
      previous.expect("previous target should be active").id(),
      third.id()
    );

    let (name, directional) = state
      .workspace_by_target(
        &dev,
        WorkspaceTarget::Direction(Direction::Right),
        &config,
      )
      .expect("directional target should resolve");
    assert_eq!(name.as_deref(), Some("Third"));
    assert_eq!(
      directional
        .expect("right monitor should have a workspace")
        .id(),
      third.id()
    );
  }
}

#[cfg(all(test, target_os = "windows"))]
mod visibility_lease_tests {
  use std::{
    sync::{
      atomic::{AtomicUsize, Ordering},
      Arc,
    },
    time::Duration,
  };

  use tokio::sync::mpsc;
  use wm_common::DisplayState;
  use wm_platform::{
    Dispatcher, NativeWindow, NativeWindowWindowsExt, WindowId,
  };

  use super::{VisibilityLease, WmState};
  use crate::{
    commands::container::attach_container,
    models::{Monitor, TilingWindow, Workspace},
    traits::{CommonGetters, WindowGetters},
  };

  fn test_state_with_window(handle: isize) -> (WmState, TilingWindow) {
    let (event_tx, _) = mpsc::unbounded_channel();
    let (exit_tx, _) = mpsc::unbounded_channel();
    let (animation_tick_tx, _) = mpsc::channel(1);
    let (window_manage_retry_tx, _) = mpsc::unbounded_channel();
    let (tab_click_tx, _) = mpsc::unbounded_channel();
    let state = WmState::new(
      Dispatcher::mock(),
      event_tx,
      exit_tx,
      animation_tick_tx,
      window_manage_retry_tx,
      tab_click_tx,
    );
    let window = TilingWindow::mock()
      .native(NativeWindow::from_handle(handle))
      .call();
    let workspace = Workspace::mock()
      .tiling_containers(vec![window.clone().into()])
      .call();
    let monitor = Monitor::mock().workspaces(vec![workspace]).call();
    attach_container(
      &monitor.into(),
      &state.root_container.clone().into(),
      None,
    )
    .unwrap();
    (state, window)
  }

  fn test_lease(
    handle: isize,
    calls: &Arc<AtomicUsize>,
  ) -> VisibilityLease {
    let calls = Arc::clone(calls);
    VisibilityLease::for_test(
      WindowId(handle),
      Arc::new(move || {
        calls.fetch_add(1, Ordering::AcqRel);
      }),
      Duration::from_secs(60),
    )
  }

  #[tokio::test]
  async fn failed_platform_sync_restores_and_drains_visibility_owner() {
    let (mut state, window) = test_state_with_window(42);
    let calls = Arc::new(AtomicUsize::new(0));
    state.register_visibility_lease(window.id(), test_lease(42, &calls));

    state.settle_visibility_leases(false, false);

    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(state.pending_visibility_lease_count(), 0);
  }

  #[tokio::test]
  async fn deferred_platform_sync_retains_one_bounded_visibility_owner() {
    let (mut state, window) = test_state_with_window(43);
    let calls = Arc::new(AtomicUsize::new(0));
    state.register_visibility_lease(window.id(), test_lease(43, &calls));

    for _ in 0..128 {
      state.settle_visibility_leases(true, false);
      assert_eq!(state.pending_visibility_lease_count(), 1);
    }

    state.settle_visibility_leases(false, false);
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(state.pending_visibility_lease_count(), 0);
  }

  #[tokio::test]
  async fn hidden_native_handoff_transfers_without_visible_restore() {
    let (mut state, window) = test_state_with_window(44);
    window.set_display_state(DisplayState::Hidden);
    let calls = Arc::new(AtomicUsize::new(0));
    state.register_visibility_lease(window.id(), test_lease(44, &calls));

    state.settle_visibility_leases(true, true);

    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert_eq!(state.pending_visibility_lease_count(), 0);
  }

  #[tokio::test]
  async fn destroyed_hwnd_cancels_only_its_matching_visibility_owner() {
    let (mut state, window) = test_state_with_window(45);
    let calls = Arc::new(AtomicUsize::new(0));
    state.register_visibility_lease(window.id(), test_lease(45, &calls));

    assert!(!state.cancel_visibility_lease(window.id(), 999));
    assert_eq!(state.pending_visibility_lease_count(), 1);
    assert!(state.cancel_visibility_lease_for_native(WindowId(45)));

    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert_eq!(state.pending_visibility_lease_count(), 0);
  }

  #[tokio::test]
  async fn repeated_failed_handoffs_never_accumulate_visibility_owners() {
    let (mut state, window) = test_state_with_window(46);
    let calls = Arc::new(AtomicUsize::new(0));

    for _ in 0..128 {
      state.register_visibility_lease(window.id(), test_lease(46, &calls));
      state.settle_visibility_leases(false, false);
      assert_eq!(state.pending_visibility_lease_count(), 0);
    }

    assert_eq!(calls.load(Ordering::Acquire), 128);
  }
}
