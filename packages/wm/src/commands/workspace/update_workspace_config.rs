use anyhow::Context;
use wm_common::{
  validate_workspace_name, InvokeUpdateWorkspaceConfig, WmEvent,
  WorkspaceConfig,
};

use super::sort_workspaces;
use crate::{
  models::Workspace, traits::CommonGetters, user_config::UserConfig,
  wm_state::WmState,
};

pub fn update_workspace_config(
  workspace: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
  new_config: &InvokeUpdateWorkspaceConfig,
) -> anyhow::Result<()> {
  let current_config = workspace.config();

  // Validate the workspace name change.
  if let Some(new_name) = &new_config.name {
    validate_workspace_name(new_name).map_err(anyhow::Error::msg)?;

    if new_name != &current_config.name {
      if state.pending_workspace_redirect.is_some() {
        anyhow::bail!(
          "Cannot rename a workspace while a workspace transition is pending"
        );
      }

      if state
        .workspace_by_name(new_name)
        .is_some_and(|other_workspace| {
          other_workspace.id() != workspace.id()
        })
      {
        anyhow::bail!("The workspace \"{}\" already exists", new_name);
      }
    }
  }

  // Update the config with the incoming values.
  let updated_config = WorkspaceConfig {
    name: new_config
      .name
      .clone()
      .unwrap_or(current_config.name.clone()),
    display_name: new_config
      .display_name
      .clone()
      .or(current_config.display_name.clone()),
    bind_to_monitor: new_config
      .bind_to_monitor
      .or(current_config.bind_to_monitor),
    keep_alive: new_config.keep_alive.unwrap_or(current_config.keep_alive),
    default_layout: current_config.default_layout.clone(),
  };

  let monitor = workspace.monitor().context("No displayed workspace.")?;
  let old_recent_workspace_name = state.recent_workspace_name.clone();

  workspace.set_config(updated_config.clone());
  if state.recent_workspace_name.as_deref()
    == Some(current_config.name.as_str())
  {
    state.recent_workspace_name = Some(updated_config.name.clone());
  }
  let update_result =
    sort_workspaces(&monitor, config).and_then(|()| workspace.to_dto());
  let updated_workspace = match update_result {
    Ok(updated_workspace) => updated_workspace,
    Err(error) => {
      workspace.set_config(current_config.clone());
      state.recent_workspace_name = old_recent_workspace_name;
      let _ = sort_workspaces(&monitor, config);
      return Err(error);
    }
  };

  // TODO: Re-assign bound workspaces to their respective monitors.

  state.emit_event(WmEvent::WorkspaceUpdated { updated_workspace });

  Ok(())
}

#[cfg(test)]
mod tests {
  use tokio::sync::mpsc;
  use wm_common::{
    InvokeUpdateWorkspaceConfig, ParsedConfig, WorkspaceConfig,
    WorkspaceLayout,
  };
  use wm_platform::Dispatcher;

  use super::*;
  use crate::{
    commands::container::attach_container,
    models::Monitor,
    traits::{CommonGetters, TilingDirectionGetters},
    wm_state::PendingWorkspaceRedirect,
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

  fn test_config(names: &[&str]) -> UserConfig {
    let mut parsed = ParsedConfig::default();
    parsed.workspaces =
      names.iter().map(|name| workspace_config(name)).collect();
    UserConfig::for_test(parsed)
  }

  fn test_state(
    names: &[&str],
  ) -> (WmState, Vec<Workspace>, mpsc::UnboundedReceiver<WmEvent>) {
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
    state.mark_initialized_for_test();
    let workspaces = names
      .iter()
      .map(|name| Workspace::mock().name((*name).to_string()).call())
      .collect::<Vec<_>>();
    let monitor = Monitor::mock().workspaces(workspaces.clone()).call();
    attach_container(
      &monitor.into(),
      &state.root_container.clone().into(),
      None,
    )
    .expect("test monitor should attach");

    (state, workspaces, event_rx)
  }

  fn rename(name: &str) -> InvokeUpdateWorkspaceConfig {
    InvokeUpdateWorkspaceConfig {
      name: Some(name.to_string()),
      display_name: None,
      bind_to_monitor: None,
      keep_alive: None,
    }
  }

  #[test]
  fn runtime_rename_rejects_invalid_or_exact_duplicate_identity() {
    let config = test_config(&["One", "Dev"]);
    let (mut state, workspaces, mut event_rx) =
      test_state(&["One", "Dev"]);
    let subject = &workspaces[0];

    for rejected in ["", "   ", " One", "One ", "Dev"] {
      update_workspace_config(
        subject,
        &mut state,
        &config,
        &rename(rejected),
      )
      .expect_err("invalid runtime workspace name must fail");
      assert_eq!(subject.config().name, "One");
      assert!(event_rx.try_recv().is_err());
    }

    update_workspace_config(subject, &mut state, &config, &rename("dev"))
      .expect("case-distinct workspace name should remain valid");
    assert_eq!(subject.config().name, "dev");
    assert!(matches!(
      event_rx.try_recv(),
      Ok(WmEvent::WorkspaceUpdated { .. })
    ));
  }

  #[test]
  fn runtime_rename_updates_recent_identity() {
    let config = test_config(&["One", "Two"]);
    let (mut state, workspaces, _) = test_state(&["One", "Two"]);
    state.recent_workspace_name = Some("One".to_string());

    update_workspace_config(
      &workspaces[0],
      &mut state,
      &config,
      &rename("Renamed"),
    )
    .expect("valid runtime workspace rename should succeed");

    assert_eq!(state.recent_workspace_name.as_deref(), Some("Renamed"));
    assert_eq!(
      state
        .workspace_by_name("Renamed")
        .expect("renamed workspace should resolve")
        .id(),
      workspaces[0].id()
    );
    assert!(state.workspace_by_name("One").is_none());
  }

  #[test]
  fn pending_redirect_blocks_only_identity_change() {
    let config = test_config(&["One", "Two"]);
    let (mut state, workspaces, _) = test_state(&["One", "Two"]);
    state.pending_workspace_redirect = Some(PendingWorkspaceRedirect {
      public_origin_name: "One".to_string(),
      destination_name: "Two".to_string(),
      order_direction: 1,
    });

    update_workspace_config(
      &workspaces[0],
      &mut state,
      &config,
      &InvokeUpdateWorkspaceConfig {
        name: None,
        display_name: Some("Visible label".to_string()),
        bind_to_monitor: None,
        keep_alive: None,
      },
    )
    .expect("non-identity workspace update should remain available");
    assert_eq!(
      workspaces[0].config().display_name.as_deref(),
      Some("Visible label")
    );

    update_workspace_config(
      &workspaces[0],
      &mut state,
      &config,
      &rename("Renamed"),
    )
    .expect_err("pending transition must reject identity change");
    assert_eq!(workspaces[0].config().name, "One");
    assert_eq!(
      state
        .pending_workspace_redirect
        .as_ref()
        .map(|pending| pending.destination_name.as_str()),
      Some("Two")
    );
  }

  #[test]
  fn runtime_rename_preserves_workspace_layout_and_unrelated_fields() {
    let config = test_config(&["One"]);
    let (mut state, workspaces, _) = test_state(&["One"]);
    let workspace = &workspaces[0];
    let original_layout = workspace.layout();

    update_workspace_config(
      workspace,
      &mut state,
      &config,
      &InvokeUpdateWorkspaceConfig {
        name: Some("Renamed".to_string()),
        display_name: Some("Label".to_string()),
        bind_to_monitor: Some(2),
        keep_alive: Some(true),
      },
    )
    .expect("complete workspace config update should succeed");

    let updated = workspace.config();
    assert_eq!(updated.name, "Renamed");
    assert_eq!(updated.display_name.as_deref(), Some("Label"));
    assert_eq!(updated.bind_to_monitor, Some(2));
    assert!(updated.keep_alive);
    assert_eq!(workspace.layout(), original_layout);
    assert_eq!(updated.default_layout, None::<WorkspaceLayout>);
  }
}
