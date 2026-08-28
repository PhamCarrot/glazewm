use std::collections::HashMap;

use anyhow::Context;
use tracing::info;
use wm_common::{WmEvent, WorkspaceConfig, WorkspaceLayout};

use super::sort_workspaces;
use crate::{
  commands::container::attach_container,
  models::{Monitor, Workspace},
  traits::{CommonGetters, PositionGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Activates a workspace on the target monitor.
///
/// If no workspace name is provided, the first suitable workspace defined
/// in the user's config will be used.
///
/// If no target monitor is provided, the workspace is activated on
/// whichever monitor it is bound to, or the currently focused monitor.
pub fn activate_workspace(
  workspace_name: Option<&str>,
  target_monitor: Option<Monitor>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let workspace_config = workspace_config(
    workspace_name,
    target_monitor.clone(),
    state,
    config,
  )?;

  let target_monitor = target_monitor
    .or_else(|| {
      workspace_config
        .bind_to_monitor
        .and_then(|index| {
          state
            .monitors()
            .into_iter()
            .find(|monitor| monitor.index() == index as usize)
        })
        .or_else(|| {
          state
            .focused_container()
            .and_then(|focused| focused.monitor())
        })
    })
    .context("Failed to get a target monitor for the workspace.")?;

  let monitor_rect = target_monitor.to_rect()?;

  let layout = workspace_layout_for_activation(
    &workspace_config.name,
    workspace_config.default_layout.as_ref(),
    &monitor_rect,
    &state.workspace_layouts,
  );

  let workspace = Workspace::new(
    workspace_config.clone(),
    config.value.gaps.clone(),
    layout,
  );

  // Attach the created workspace to the specified monitor.
  attach_container(
    &workspace.clone().into(),
    &target_monitor.clone().into(),
    None,
  )?;

  sort_workspaces(&target_monitor, config)?;

  info!("Activating workspace: {workspace}");

  state.emit_event(WmEvent::WorkspaceActivated {
    activated_workspace: workspace.to_dto()?,
  });

  Ok(())
}

fn workspace_layout_for_activation(
  workspace_name: &str,
  configured_default: Option<&WorkspaceLayout>,
  monitor_rect: &wm_platform::Rect,
  remembered_layouts: &HashMap<String, WorkspaceLayout>,
) -> WorkspaceLayout {
  remembered_layouts
    .get(workspace_name)
    .cloned()
    .or_else(|| configured_default.cloned())
    .unwrap_or_else(|| {
      if monitor_rect.height() > monitor_rect.width() {
        WorkspaceLayout::SplitVertical
      } else {
        WorkspaceLayout::Bsp
      }
    })
}

/// Gets config for the workspace to activate.
fn workspace_config(
  workspace_name: Option<&str>,
  target_monitor: Option<Monitor>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WorkspaceConfig> {
  let found_config = match workspace_name {
    Some(workspace_name) => config
      .inactive_workspace_configs(&state.workspaces())
      .into_iter()
      .find(|config| config.name == workspace_name)
      .with_context(|| {
        format!(
          "Workspace with name '{workspace_name}' doesn't exist or is already active."
        )
      }),
    None => target_monitor
      .and_then(|target_monitor| {
        config.workspace_config_for_monitor(
          &target_monitor,
          &state.workspaces(),
        )
      })
      .or_else(|| {
        config.next_inactive_workspace_config(&state.workspaces())
      })
      .context("No workspace config available to activate workspace."),
  };

  found_config.cloned()
}

#[cfg(test)]
mod tests {
  use wm_platform::Rect;

  use super::*;

  #[test]
  fn activation_restores_remembered_workspace_layout() {
    let mut remembered = HashMap::new();
    remembered.insert("dev".to_string(), WorkspaceLayout::Grid);

    let layout = workspace_layout_for_activation(
      "dev",
      Some(&WorkspaceLayout::Columns),
      &Rect::from_xy(0, 0, 1920, 1080),
      &remembered,
    );

    assert_eq!(layout, WorkspaceLayout::Grid);
  }

  #[test]
  fn activation_uses_configured_default_without_remembered_layout() {
    let remembered = HashMap::new();

    let layout = workspace_layout_for_activation(
      "dev",
      Some(&WorkspaceLayout::Grid),
      &Rect::from_xy(0, 0, 1920, 1080),
      &remembered,
    );

    assert_eq!(layout, WorkspaceLayout::Grid);
  }

  #[test]
  fn activation_uses_orientation_default_without_remembered_layout() {
    let remembered = HashMap::new();

    assert_eq!(
      workspace_layout_for_activation(
        "landscape",
        None,
        &Rect::from_xy(0, 0, 1920, 1080),
        &remembered,
      ),
      WorkspaceLayout::Bsp,
    );
    assert_eq!(
      workspace_layout_for_activation(
        "portrait",
        None,
        &Rect::from_xy(0, 0, 1080, 1920),
        &remembered,
      ),
      WorkspaceLayout::SplitVertical,
    );
  }
}
