use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context};
use tracing::{info, warn};
#[cfg(target_os = "windows")]
use wm_common::{HideMethod, ParsedConfig};
use wm_common::{
  InvokeCommand, VecDequeExt, WindowRuleEvent, WmEvent, WorkspaceConfig,
};
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;

use crate::{
  animation::AnimationManager,
  commands::window::run_window_rules,
  models::{Monitor, Workspace},
  traits::{CommonGetters, TilingSizeGetters, WindowGetters},
  user_config::UserConfig,
  wm::WindowManager,
  wm_state::WmState,
};

struct WorkspaceReloadUpdate {
  workspace: Workspace,
  config: WorkspaceConfig,
}

struct MonitorWorkspaceOrder {
  monitor: Monitor,
  workspaces: Vec<Workspace>,
}

/// Fully parsed and preflighted desired-state replacement. No live state
/// is mutated while this plan is built.
struct ReloadPlan {
  candidate: UserConfig,
  config_path: String,
  workspace_updates: Vec<WorkspaceReloadUpdate>,
  monitor_orders: Vec<MonitorWorkspaceOrder>,
  recent_workspace_name: Option<String>,
}

impl ReloadPlan {
  fn prepare(
    state: &WmState,
    config: &UserConfig,
  ) -> anyhow::Result<Self> {
    let candidate = config.load_candidate()?;
    let config_path = candidate
      .path
      .to_str()
      .context("Invalid config path.")?
      .to_string();
    let workspaces = state.workspaces();
    let mut claimed_names = HashSet::with_capacity(workspaces.len());
    let mut assignments = HashMap::with_capacity(workspaces.len());

    // Preserve exact identities first. CFG-005 guarantees candidate names
    // are unique, so every subsequent assignment can claim one
    // remaining config.
    for workspace in &workspaces {
      workspace
        .monitor()
        .context("Cannot reload a detached workspace.")?;
      workspace
        .to_dto()
        .context("Cannot serialize workspace before config reload.")?;

      let current_name = workspace.config().name;
      if let Some(candidate_config) = candidate
        .value
        .workspaces
        .iter()
        .find(|candidate| candidate.name == current_name)
      {
        claimed_names.insert(candidate_config.name.clone());
        assignments.insert(workspace.id(), candidate_config.clone());
      }
    }

    // Reassign removed active names deterministically without mutating the
    // active-workspace set between selections.
    for workspace in &workspaces {
      if assignments.contains_key(&workspace.id()) {
        continue;
      }

      let monitor = workspace
        .monitor()
        .context("Cannot reload a detached workspace.")?;
      let available = candidate
        .value
        .workspaces
        .iter()
        .filter(|candidate| !claimed_names.contains(&candidate.name))
        .collect::<Vec<_>>();
      let selected = available
        .iter()
        .find(|candidate| {
          candidate.bind_to_monitor == Some(monitor.index() as u32)
        })
        .or_else(|| {
          available
            .iter()
            .find(|candidate| candidate.bind_to_monitor.is_none())
        })
        .or_else(|| available.first())
        .copied();

      if let Some(selected) = selected {
        claimed_names.insert(selected.name.clone());
        assignments.insert(workspace.id(), selected.clone());
      }
    }

    let workspace_updates = workspaces
      .iter()
      .filter_map(|workspace| {
        assignments.get(&workspace.id()).and_then(|candidate| {
          (workspace.config() != *candidate).then(|| {
            WorkspaceReloadUpdate {
              workspace: workspace.clone(),
              config: candidate.clone(),
            }
          })
        })
      })
      .collect::<Vec<_>>();

    let assigned_name = |workspace: &Workspace| {
      assignments.get(&workspace.id()).map_or_else(
        || workspace.config().name,
        |config| config.name.clone(),
      )
    };
    let monitor_orders = state
      .monitors()
      .into_iter()
      .map(|monitor| {
        let mut monitor_workspaces = monitor.workspaces();
        monitor_workspaces.sort_by_key(|workspace| {
          candidate.workspace_config_index(&assigned_name(workspace))
        });
        MonitorWorkspaceOrder {
          monitor,
          workspaces: monitor_workspaces,
        }
      })
      .collect();

    let recent_workspace_name = state
      .recent_workspace_name
      .as_ref()
      .and_then(|recent_name| {
        workspaces
          .iter()
          .find(|workspace| workspace.config().name == *recent_name)
          .map(&assigned_name)
          .or_else(|| {
            candidate
              .value
              .workspaces
              .iter()
              .any(|workspace| workspace.name == *recent_name)
              .then(|| recent_name.clone())
          })
      });

    Ok(Self {
      candidate,
      config_path,
      workspace_updates,
      monitor_orders,
      recent_workspace_name,
    })
  }
}

pub fn reload_config(
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  if state.is_reloading_config {
    bail!("A config reload is already in progress.");
  }

  // Parse, validate, derive, and preflight without mutating the active
  // config or container tree. Every returned reload error occurs before
  // commit.
  let plan = ReloadPlan::prepare(state, config)?;

  // Animation updates consult live effect/config values. Settle the old
  // generation's native ownership before replacing those values so one
  // session never mixes generations. This path is nonblocking and uses the
  // existing visibility-safe recovery transaction.
  if state.animation_manager.has_active_animations() {
    AnimationManager::settle_for_config_reload(state, config);
  }

  state.is_reloading_config = true;

  // Keep reference to old config for comparison.
  #[cfg(target_os = "windows")]
  let old_config = config.value.clone();

  let ReloadPlan {
    candidate,
    config_path,
    workspace_updates,
    monitor_orders,
    recent_workspace_name,
  } = plan;

  // Commit the new desired state once. All operations in this section are
  // in-memory and were preflighted above.
  *config = candidate;
  state.recent_workspace_name = recent_workspace_name;

  for update in workspace_updates {
    update.workspace.set_config(update.config);
    match update.workspace.to_dto() {
      Ok(updated_workspace) => {
        state.emit_event(WmEvent::WorkspaceUpdated { updated_workspace });
      }
      Err(error) => warn!(
        "Failed to serialize preflighted workspace after reload commit: \
         {error:?}"
      ),
    }
  }

  for order in monitor_orders {
    for (target_index, workspace) in
      order.workspaces.into_iter().enumerate()
    {
      order
        .monitor
        .borrow_children_mut()
        .shift_to_index(target_index, workspace.into());
    }
  }

  state.workspace_layouts.retain(|workspace_name, _| {
    config.workspace_config_index(workspace_name).is_some()
  });

  update_container_gaps(state, config);

  #[cfg(target_os = "windows")]
  update_window_effects(&old_config, state, config);

  // Ensure all windows are shown when hide method is changed.
  #[cfg(target_os = "windows")]
  if old_config.general.hide_method != config.value.general.hide_method
    && config.value.general.hide_method == HideMethod::Cloak
  {
    for window in state.windows() {
      if let Err(err) = window.native().show() {
        warn!("Failed to show window after hide-method reload: {err:?}");
      }
    }
  }

  // Ensure all windows are shown in taskbar when `show_all_in_taskbar` is
  // changed.
  #[cfg(target_os = "windows")]
  if old_config.general.show_all_in_taskbar
    != config.value.general.show_all_in_taskbar
    && config.value.general.show_all_in_taskbar
  {
    for window in state.windows() {
      if let Err(err) = window.native().set_taskbar_visibility(true) {
        warn!(
          "Failed to restore taskbar visibility after config reload: {err:?}"
        );
      }
    }
  }

  state.binding_modes.clear();
  state
    .pending_sync
    .queue_container_to_redraw(state.root_container.clone());
  state.animation_manager.invalidate_timer_generation();

  state.emit_event(WmEvent::UserConfigChanged {
    config_path,
    config_string: config.value_str.clone(),
    parsed_config: config.value.clone(),
  });

  info!("Config reload committed.");

  // Window rules and reload commands can contain irreversible user actions
  // such as close or shell-exec. They are deliberately post-commit
  // reconciliation/hooks, not members of the atomic desired-state commit.
  // A failure is isolated and logged instead of reporting that the
  // already- committed config rolled back.
  for window in state.windows() {
    window.set_done_window_rules(Vec::new());
    if let Err(error) =
      run_window_rules(window, &WindowRuleEvent::Manage, state, config)
    {
      warn!("Window-rule reconciliation failed after reload: {error:?}");
    }
  }

  run_reload_hooks_best_effort(
    &config.value.general.config_reload_commands.clone(),
    state,
    config,
  );

  state.is_reloading_config = false;

  Ok(())
}

fn run_reload_hooks_best_effort(
  commands: &[InvokeCommand],
  state: &mut WmState,
  config: &mut UserConfig,
) {
  let mut subject = state.focused_container();
  for (index, command) in commands.iter().enumerate() {
    let Some(current_subject) = subject.clone() else {
      warn!("Skipped config reload hook {index}: no focused container.");
      break;
    };

    if let Err(error) = WindowManager::run_command(
      command,
      current_subject.clone(),
      state,
      config,
    ) {
      warn!("Config reload hook {index} failed: {error:?}");
    }

    subject = if current_subject.is_detached() {
      state
        .container_by_id(current_subject.id())
        .or_else(|| state.focused_container())
    } else {
      Some(current_subject)
    };
  }
}

/// Updates outer gap of workspaces and inner gaps of tiling containers.
fn update_container_gaps(state: &mut WmState, config: &UserConfig) {
  let tiling_containers = state
    .root_container
    .self_and_descendants()
    .filter_map(|container| container.as_tiling_container().ok());

  for container in tiling_containers {
    container.set_gaps_config(config.value.gaps.clone());
  }

  for workspace in state.workspaces() {
    workspace.set_gaps_config(config.value.gaps.clone());
  }
}

#[cfg(target_os = "windows")]
fn update_window_effects(
  old_config: &ParsedConfig,
  state: &mut WmState,
  config: &UserConfig,
) {
  let focused_container = state.focused_container();
  let focused_id = focused_container.as_ref().map(CommonGetters::id);

  let window_effects = &config.value.window_effects;
  let old_window_effects = &old_config.window_effects;

  // Window border effects are left at system defaults if disabled in the
  // config. However, when transitioning from colored borders to having
  // them disabled, it's best to reset to the system defaults.
  if !window_effects.focused_window.border.enabled
    && old_window_effects.focused_window.border.enabled
  {
    if let Some(window) = focused_container
      .as_ref()
      .and_then(|container| container.as_window_container().ok())
    {
      if let Err(error) = window.native().set_border_color(None) {
        warn!("Failed to reset focused border during reload: {error:?}");
      }
    }
  }

  if !window_effects.other_windows.border.enabled
    && old_window_effects.other_windows.border.enabled
  {
    let unfocused_windows = state
      .windows()
      .into_iter()
      .filter(|window| Some(window.id()) != focused_id);

    for window in unfocused_windows {
      if let Err(error) = window.native().set_border_color(None) {
        warn!("Failed to reset unfocused border during reload: {error:?}");
      }
    }
  }

  state.pending_sync.queue_all_effects_update();
}

#[cfg(test)]
mod tests {
  use std::{fs, path::PathBuf};

  use tokio::sync::mpsc;
  use uuid::Uuid;
  use wm_common::{EasingFunction, InvokeUpdateWorkspaceConfig, WmEvent};
  use wm_platform::{Dispatcher, Rect};

  use super::*;
  use crate::{
    animation::state::WindowAnimationState,
    commands::container::{attach_container, set_focused_descendant},
    models::Workspace,
  };

  struct ReloadFixture {
    state: WmState,
    config: UserConfig,
    config_path: PathBuf,
    old_workspace: Workspace,
    stable_workspace: Workspace,
    event_rx: mpsc::UnboundedReceiver<WmEvent>,
    test_root: PathBuf,
  }

  impl Drop for ReloadFixture {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.test_root);
    }
  }

  fn old_yaml() -> &'static str {
    "gaps:\n  inner_gap: '5px'\nworkspaces:\n  - name: 'Old'\n  - name: 'Stable'\n"
  }

  fn new_yaml() -> &'static str {
    "gaps:\n  inner_gap: '12px'\nworkspaces:\n  - name: 'New'\n  - name: 'Stable'\n"
  }

  fn fixture() -> ReloadFixture {
    let test_root = std::env::current_dir()
      .expect("test working directory should exist")
      .join("target")
      .join(format!("reload001-{}", Uuid::new_v4()));
    fs::create_dir_all(&test_root)
      .expect("source-local reload test directory should be created");
    let config_path = test_root.join("config.yaml");
    fs::write(&config_path, old_yaml())
      .expect("initial reload config should be written");
    let config = UserConfig::new(Some(config_path.clone()))
      .expect("initial reload config should load");

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (exit_tx, _) = mpsc::unbounded_channel();
    let (animation_tick_tx, _) = mpsc::channel(1);
    let (window_manage_retry_tx, _) = mpsc::unbounded_channel();
    let (tab_click_tx, _) = mpsc::unbounded_channel();
    let mut state = WmState::new(
      Dispatcher::mock(),
      event_tx,
      exit_tx,
      animation_tick_tx,
      window_manage_retry_tx,
      tab_click_tx,
    );
    let old_workspace = Workspace::mock().name("Old".to_string()).call();
    let stable_workspace =
      Workspace::mock().name("Stable".to_string()).call();
    let monitor = Monitor::mock()
      .workspaces(vec![old_workspace.clone(), stable_workspace.clone()])
      .call();
    attach_container(
      &monitor.into(),
      &state.root_container.clone().into(),
      None,
    )
    .expect("test monitor should attach");
    set_focused_descendant(&old_workspace.clone().into(), None);
    state.recent_workspace_name = Some("Old".to_string());
    state.mark_initialized_for_test();

    ReloadFixture {
      state,
      config,
      config_path,
      old_workspace,
      stable_workspace,
      event_rx,
      test_root,
    }
  }

  #[test]
  fn invalid_candidate_fails_before_any_live_state_mutation() {
    let mut fixture = fixture();
    let previous_config = fixture.config.value_str.clone();
    let previous_recent = fixture.state.recent_workspace_name.clone();
    fs::write(
      &fixture.config_path,
      "workspaces:\n  - name: 'Duplicate'\n  - name: 'Duplicate'\n",
    )
    .expect("invalid candidate should be written");

    reload_config(&mut fixture.state, &mut fixture.config)
      .expect_err("invalid candidate must fail before commit");

    assert_eq!(fixture.config.value_str, previous_config);
    assert_eq!(fixture.old_workspace.config().name, "Old");
    assert_eq!(fixture.stable_workspace.config().name, "Stable");
    assert_eq!(fixture.state.recent_workspace_name, previous_recent);
    assert!(!fixture.state.is_reloading_config);
    assert!(fixture.event_rx.try_recv().is_err());
  }

  #[test]
  fn valid_reload_commits_config_workspace_order_and_recent_once() {
    let mut fixture = fixture();
    fs::write(&fixture.config_path, new_yaml())
      .expect("replacement config should be written");

    reload_config(&mut fixture.state, &mut fixture.config)
      .expect("preflighted reload should commit");

    assert_eq!(fixture.config.value.gaps.inner_gap.amount, 12.0);
    assert_eq!(fixture.old_workspace.config().name, "New");
    assert_eq!(fixture.stable_workspace.config().name, "Stable");
    assert_eq!(
      fixture.state.recent_workspace_name.as_deref(),
      Some("New")
    );
    assert!(!fixture.state.is_reloading_config);
    let ordered_names = fixture.state.monitors()[0]
      .workspaces()
      .into_iter()
      .map(|workspace| workspace.config().name)
      .collect::<Vec<_>>();
    assert_eq!(ordered_names, ["New", "Stable"]);

    let events = std::iter::from_fn(|| fixture.event_rx.try_recv().ok())
      .collect::<Vec<_>>();
    assert_eq!(
      events
        .iter()
        .filter(|event| matches!(event, WmEvent::WorkspaceUpdated { .. }))
        .count(),
      1
    );
    assert_eq!(
      events
        .iter()
        .filter(|event| matches!(event, WmEvent::UserConfigChanged { .. }))
        .count(),
      1
    );

    reload_config(&mut fixture.state, &mut fixture.config)
      .expect("same-config reload should be idempotent");
    let repeated_events =
      std::iter::from_fn(|| fixture.event_rx.try_recv().ok())
        .collect::<Vec<_>>();
    assert!(!repeated_events
      .iter()
      .any(|event| matches!(event, WmEvent::WorkspaceUpdated { .. })));
    assert_eq!(fixture.old_workspace.config().name, "New");
  }

  #[test]
  fn reload_settles_old_animation_generation_before_commit() {
    let mut fixture = fixture();
    fixture.state.animation_manager.start_animation(
      Uuid::new_v4(),
      WindowAnimationState::new_movement(
        Rect::from_xy(0, 0, 100, 100),
        Rect::from_xy(100, 0, 100, 100),
        360,
        EasingFunction::CubicBezier(0.0, 0.0, 1.0, 1.0),
      ),
    );
    assert!(fixture.state.animation_manager.has_active_animations());
    fs::write(&fixture.config_path, new_yaml())
      .expect("replacement config should be written");

    reload_config(&mut fixture.state, &mut fixture.config)
      .expect("reload should safely settle active animation state");

    assert!(!fixture.state.animation_manager.has_active_animations());
    assert_eq!(fixture.config.value.gaps.inner_gap.amount, 12.0);
    assert!(!fixture.state.is_reloading_config);
  }

  #[test]
  fn post_commit_hook_failures_are_isolated_and_recursion_is_bounded() {
    let mut fixture = fixture();
    fixture.state.pending_sync.clear();
    fixture.state.is_reloading_config = true;
    let hooks = vec![
      InvokeCommand::UpdateWorkspaceConfig {
        workspace: Some("Missing".to_string()),
        new_config: InvokeUpdateWorkspaceConfig {
          name: None,
          display_name: None,
          bind_to_monitor: None,
          keep_alive: Some(true),
        },
      },
      InvokeCommand::WmRedraw,
      InvokeCommand::WmReloadConfig,
    ];

    run_reload_hooks_best_effort(
      &hooks,
      &mut fixture.state,
      &mut fixture.config,
    );

    assert!(fixture.state.pending_sync.has_changes());
    assert!(fixture.state.is_reloading_config);
    assert_eq!(fixture.config.value_str, old_yaml());
  }

  #[test]
  fn paused_reload_core_remains_deterministic_and_clears_guard() {
    let mut fixture = fixture();
    fixture.state.is_paused = true;
    fs::write(&fixture.config_path, new_yaml())
      .expect("replacement config should be written");

    reload_config(&mut fixture.state, &mut fixture.config)
      .expect("direct paused reload should commit core desired state");

    assert!(fixture.state.is_paused);
    assert!(!fixture.state.is_reloading_config);
    assert_eq!(fixture.old_workspace.config().name, "New");
  }
}
