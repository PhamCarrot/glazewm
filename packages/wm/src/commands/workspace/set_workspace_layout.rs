use anyhow::Context;
use wm_common::{WmEvent, WorkspaceLayout};

use crate::{
  models::{Container, Workspace},
  traits::{CommonGetters, TilingDirectionGetters},
  wm_state::WmState,
};

/// Sets the layout for the workspace that owns the given container.
///
/// Nested split-container layouts are intentionally left to the separate
/// tiling-direction commands.
pub fn set_workspace_layout(
  container: Container,
  layout: WorkspaceLayout,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let previous_tiling_direction = container
    .workspace()
    .context("No workspace.")?
    .tiling_direction();
  let workspace =
    set_layout_on_owning_workspace(&container, layout.clone())?;

  // Redraw all tiling windows in the affected workspace.
  for window in workspace
    .descendants()
    .filter_map(|c| c.as_tiling_window().cloned())
  {
    state.pending_sync.queue_container_to_redraw(window);
  }

  state.emit_event(WmEvent::LayoutChanged {
    direction_container: workspace.to_dto()?,
    new_layout: layout,
  });

  let new_tiling_direction = workspace.tiling_direction();
  if previous_tiling_direction != new_tiling_direction {
    state.emit_event(WmEvent::TilingDirectionChanged {
      direction_container: workspace.to_dto()?,
      new_tiling_direction,
    });
  }

  Ok(())
}

fn set_layout_on_owning_workspace(
  container: &Container,
  layout: WorkspaceLayout,
) -> anyhow::Result<Workspace> {
  let workspace = container.workspace().context("No workspace.")?;
  workspace.set_layout(layout);
  Ok(workspace)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::models::{SplitContainer, TilingContainer, TilingWindow};

  #[test]
  fn workspace_layout_command_does_not_mutate_nested_split_layout() {
    let window = TilingWindow::mock().call();
    let split = SplitContainer::mock()
      .layout(WorkspaceLayout::SplitVertical)
      .tiling_containers(vec![TilingContainer::from(window.clone())])
      .call();
    let workspace = Workspace::mock()
      .layout(WorkspaceLayout::Bsp)
      .tiling_containers(vec![TilingContainer::from(split.clone())])
      .call();

    let changed_workspace = set_layout_on_owning_workspace(
      &window.into(),
      WorkspaceLayout::Grid,
    )
    .unwrap();

    assert_eq!(changed_workspace.id(), workspace.id());
    assert_eq!(workspace.layout(), WorkspaceLayout::Grid);
    assert_eq!(split.layout(), WorkspaceLayout::SplitVertical);
  }
}
