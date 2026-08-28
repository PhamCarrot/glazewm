use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::{
  models::{Container, Workspace},
  traits::CommonGetters,
};

/// A pending iris-wipe workspace switch requested by `focus_workspace`.
///
/// Carries the monitor geometry and the circle origin (all in screen
/// pixels) so `platform_sync` can snapshot the monitor and start the
/// overlay before the real windows are switched underneath. Stored as
/// primitives to avoid coupling `PendingSync` to platform types.
#[derive(Debug, Clone, Copy)]
pub struct IrisSwitchRequest {
  pub monitor_x: i32,
  pub monitor_y: i32,
  pub monitor_width: i32,
  pub monitor_height: i32,
  pub monitor_handle: isize,
  pub origin_x: i32,
  pub origin_y: i32,
}

#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct PendingSync {
  /// Containers (and their descendants) that have a pending redraw.
  containers_to_redraw: HashMap<Uuid, Container>,

  /// Workspaces where z-order should be updated. Windows that match the
  /// focused window's state should be brought to the front.
  workspaces_to_reorder: Vec<Workspace>,

  /// Whether native focus should be reassigned to the WM's focused
  /// container.
  needs_focus_update: bool,

  /// Whether window effect for the focused window should be updated.
  needs_focused_effect_update: bool,

  /// Whether window effects for all windows should be updated.
  needs_all_effects_update: bool,

  /// Whether to jump the cursor to the focused container (if enabled in
  /// user config).
  needs_cursor_jump: bool,

  /// Window IDs on the incoming workspace that should slide in.
  workspace_switch_incoming: HashSet<Uuid>,

  /// Window IDs on the outgoing workspace that should slide out.
  workspace_switch_outgoing: HashSet<Uuid>,

  /// Slide direction for the current workspace switch.
  ///
  /// `+1` means the target workspace has a higher config index (incoming
  /// slides in from the right, outgoing slides out to the left). `-1`
  /// means the opposite. `0` means no directional preference (fade in
  /// place).
  workspace_switch_direction: i32,

  /// Window IDs that just underwent a tiling/floating state change this
  /// sync cycle. Used by `platform_sync` to allow `window_move`
  /// animations across the state boundary (e.g. tiling → floating or
  /// floating → tiling).
  window_state_changes: HashSet<Uuid>,

  /// Pending iris-wipe workspace switch, consumed by `platform_sync` to
  /// create the snapshot overlay before the real windows are switched.
  iris_switch: Option<IrisSwitchRequest>,

  /// Whether animations should be skipped for this sync cycle.
  ///
  /// Set on display setting changes: animating a relayout caused by a
  /// resolution or working-area change is visually meaningless, and the
  /// cloak/surrogate mechanism would kick exclusive-fullscreen games out
  /// of fullscreen (reverting the resolution and re-triggering the
  /// event in a loop). In-flight animations of redrawn windows are
  /// cancelled and their windows snap to their target rect.
  animations_suppressed: bool,
}

impl PendingSync {
  pub fn has_changes(&self) -> bool {
    !self.containers_to_redraw.is_empty()
      || !self.workspaces_to_reorder.is_empty()
      || self.needs_focus_update
      || self.needs_focused_effect_update
      || self.needs_all_effects_update
      || self.needs_cursor_jump
      || self.iris_switch.is_some()
  }

  pub fn clear(&mut self) -> &mut Self {
    self.containers_to_redraw.clear();
    self.workspaces_to_reorder.clear();
    self.needs_focus_update = false;
    self.needs_focused_effect_update = false;
    self.needs_all_effects_update = false;
    self.needs_cursor_jump = false;
    self.workspace_switch_incoming.clear();
    self.workspace_switch_outgoing.clear();
    self.workspace_switch_direction = 0;
    self.window_state_changes.clear();
    self.iris_switch = None;
    self.animations_suppressed = false;
    self
  }

  pub fn queue_container_to_redraw<T>(&mut self, container: T) -> &mut Self
  where
    T: Into<Container>,
  {
    let container: Container = container.into();
    self.containers_to_redraw.insert(container.id(), container);
    self
  }

  pub fn queue_containers_to_redraw<I, T>(
    &mut self,
    containers: I,
  ) -> &mut Self
  where
    I: IntoIterator<Item = T>,
    T: Into<Container>,
  {
    for container in containers {
      let container: Container = container.into();
      self.containers_to_redraw.insert(container.id(), container);
    }

    self
  }

  pub fn dequeue_container_from_redraw<T>(
    &mut self,
    container: T,
  ) -> &mut Self
  where
    T: Into<Container>,
  {
    self.containers_to_redraw.remove(&container.into().id());
    self
  }

  /// Removes every pending operation owned specifically by `window_id`.
  ///
  /// Global focus/effect flags are intentionally retained: after a
  /// destroyed focused window is detached, those operations must run
  /// against the newly selected safe focus target. Workspace-wide
  /// redraw/reorder work is also preserved for surviving windows.
  pub fn remove_window(&mut self, window_id: &Uuid) -> &mut Self {
    self.containers_to_redraw.remove(window_id);
    self.workspace_switch_incoming.remove(window_id);
    self.workspace_switch_outgoing.remove(window_id);
    self.window_state_changes.remove(window_id);
    self
  }

  pub fn queue_workspace_to_reorder(
    &mut self,
    workspace: Workspace,
  ) -> &mut Self {
    self.workspaces_to_reorder.push(workspace);
    self
  }

  pub fn queue_focus_change(&mut self) -> &mut Self {
    self.needs_focus_update = true;
    self
  }

  /// Cancels a focus operation owned by a failed animation handoff without
  /// disturbing unrelated redraw, effect, cursor, or workspace work.
  pub fn dequeue_focus_change(&mut self) -> &mut Self {
    self.needs_focus_update = false;
    self
  }

  pub fn queue_focused_effect_update(&mut self) -> &mut Self {
    self.needs_focused_effect_update = true;
    self
  }

  pub fn queue_all_effects_update(&mut self) -> &mut Self {
    self.needs_all_effects_update = true;
    self
  }

  pub fn queue_cursor_jump(&mut self) -> &mut Self {
    self.needs_cursor_jump = true;
    self
  }

  pub fn needs_focus_update(&self) -> bool {
    self.needs_focus_update
  }

  pub fn needs_focused_effect_update(&self) -> bool {
    self.needs_focused_effect_update
  }

  pub fn needs_all_effects_update(&self) -> bool {
    self.needs_all_effects_update
  }

  pub fn needs_cursor_jump(&self) -> bool {
    self.needs_cursor_jump
  }

  pub fn containers_to_redraw(&self) -> &HashMap<Uuid, Container> {
    &self.containers_to_redraw
  }

  pub fn workspaces_to_reorder(&self) -> &Vec<Workspace> {
    &self.workspaces_to_reorder
  }

  /// Marks a window as having just changed tiling/floating state this
  /// cycle.
  pub fn mark_window_state_change(&mut self, id: Uuid) -> &mut Self {
    self.window_state_changes.insert(id);
    self
  }

  /// Returns `true` if the window changed tiling/floating state this
  /// cycle.
  pub fn is_window_state_change(&self, id: &Uuid) -> bool {
    self.window_state_changes.contains(id)
  }

  /// Registers a window as an incoming workspace-switch target.
  pub fn setup_workspace_switch_incoming(
    &mut self,
    window_id: Uuid,
  ) -> &mut Self {
    self.workspace_switch_incoming.insert(window_id);
    self
  }

  /// Registers a window as an outgoing workspace-switch window.
  pub fn setup_workspace_switch_outgoing(
    &mut self,
    window_id: Uuid,
  ) -> &mut Self {
    self.workspace_switch_outgoing.insert(window_id);
    self
  }

  /// Replaces the one-cycle workspace participant markers without
  /// disturbing unrelated redraw, focus, effect, or cursor work.
  ///
  /// Directional redirect input uses this before installing the latest
  /// absolute underlay so a command burst never accumulates participants.
  pub fn clear_workspace_switch_participants(&mut self) -> &mut Self {
    self.workspace_switch_incoming.clear();
    self.workspace_switch_outgoing.clear();
    self.workspace_switch_direction = 0;
    self
  }

  /// Returns `true` if the window is an incoming workspace-switch window.
  pub fn is_workspace_switch_incoming(&self, window_id: &Uuid) -> bool {
    self.workspace_switch_incoming.contains(window_id)
  }

  /// Returns `true` if the window is an outgoing workspace-switch window.
  pub fn is_workspace_switch_outgoing(&self, window_id: &Uuid) -> bool {
    self.workspace_switch_outgoing.contains(window_id)
  }

  /// Sets the slide direction for the current workspace switch.
  pub fn set_workspace_switch_direction(
    &mut self,
    direction: i32,
  ) -> &mut Self {
    self.workspace_switch_direction = direction;
    self
  }

  /// Returns the slide direction for the current workspace switch.
  pub fn workspace_switch_direction(&self) -> i32 {
    self.workspace_switch_direction
  }

  /// Requests an iris-wipe workspace switch for this sync cycle.
  pub fn request_iris_switch(
    &mut self,
    request: IrisSwitchRequest,
  ) -> &mut Self {
    self.iris_switch = Some(request);
    self
  }

  /// Consumes and returns the pending iris-wipe request, if any.
  pub fn take_iris_switch(&mut self) -> Option<IrisSwitchRequest> {
    self.iris_switch.take()
  }

  /// Suppresses animations for this sync cycle (see
  /// `animations_suppressed` field docs).
  pub fn suppress_animations(&mut self) -> &mut Self {
    self.animations_suppressed = true;
    self
  }

  /// Returns `true` if animations should be skipped for this sync cycle.
  pub fn animations_suppressed(&self) -> bool {
    self.animations_suppressed
  }
}

#[cfg(test)]
mod tests {
  use uuid::Uuid;

  use super::PendingSync;

  #[test]
  fn removing_destroyed_window_preserves_other_pending_participants() {
    let stale = Uuid::new_v4();
    let survivor = Uuid::new_v4();
    let mut pending = PendingSync::default();
    pending
      .setup_workspace_switch_incoming(stale)
      .setup_workspace_switch_outgoing(stale)
      .mark_window_state_change(stale)
      .setup_workspace_switch_incoming(survivor)
      .queue_focus_change()
      .queue_focused_effect_update();

    pending.remove_window(&stale);

    assert!(!pending.is_workspace_switch_incoming(&stale));
    assert!(!pending.is_workspace_switch_outgoing(&stale));
    assert!(!pending.is_window_state_change(&stale));
    assert!(pending.is_workspace_switch_incoming(&survivor));
    assert!(pending.needs_focus_update());
    assert!(pending.needs_focused_effect_update());
  }

  #[test]
  fn replacing_workspace_participants_preserves_unrelated_pending_work() {
    let old_incoming = Uuid::new_v4();
    let old_outgoing = Uuid::new_v4();
    let latest = Uuid::new_v4();
    let mut pending = PendingSync::default();
    pending
      .setup_workspace_switch_incoming(old_incoming)
      .setup_workspace_switch_outgoing(old_outgoing)
      .set_workspace_switch_direction(-1)
      .queue_focus_change()
      .queue_focused_effect_update()
      .queue_cursor_jump();

    pending
      .clear_workspace_switch_participants()
      .setup_workspace_switch_incoming(latest)
      .set_workspace_switch_direction(1);

    assert!(!pending.is_workspace_switch_incoming(&old_incoming));
    assert!(!pending.is_workspace_switch_outgoing(&old_outgoing));
    assert!(pending.is_workspace_switch_incoming(&latest));
    assert_eq!(pending.workspace_switch_direction(), 1);
    assert!(pending.needs_focus_update());
    assert!(pending.needs_focused_effect_update());
    assert!(pending.needs_cursor_jump());
  }

  #[test]
  fn canceling_animation_owned_focus_preserves_unrelated_pending_work() {
    let window = Uuid::new_v4();
    let mut pending = PendingSync::default();
    pending
      .queue_focus_change()
      .queue_focused_effect_update()
      .queue_cursor_jump()
      .mark_window_state_change(window);

    pending.dequeue_focus_change();

    assert!(!pending.needs_focus_update());
    assert!(pending.needs_focused_effect_update());
    assert!(pending.needs_cursor_jump());
    assert!(pending.is_window_state_change(&window));
  }
}
