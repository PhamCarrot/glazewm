use anyhow::Context;
use wm_common::{TilingDirection, WmEvent, WorkspaceLayout};

use super::{flatten_split_container, wrap_in_split_container};
use crate::{
  models::{Container, DirectionContainer, SplitContainer, TilingWindow},
  traits::{CommonGetters, TilingDirectionGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub fn toggle_tiling_direction(
  container: Container,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let direction_container = match container {
    Container::TilingWindow(tiling_window) => {
      toggle_window_direction(tiling_window, config)
    }
    Container::Workspace(workspace) => {
      workspace.set_layout(workspace.layout().inverse());
      Ok(workspace.into())
    }
    // Can only toggle tiling direction from a tiling window or workspace.
    _ => return Ok(()),
  }?;

  let direction_container_dto = direction_container.to_dto()?;
  state.emit_event(WmEvent::TilingDirectionChanged {
    direction_container: direction_container_dto.clone(),
    new_tiling_direction: direction_container.tiling_direction(),
  });

  // `layout_changed` is the custom workspace-layout signal consumed by the
  // active-layout integration. A nested split direction is not a workspace
  // layout change and must not publish a misleading custom event.
  if is_workspace_layout_change(&direction_container) {
    state.emit_event(WmEvent::LayoutChanged {
      direction_container: direction_container_dto,
      new_layout: direction_container.layout(),
    });
  }

  Ok(())
}

fn is_workspace_layout_change(
  direction_container: &DirectionContainer,
) -> bool {
  matches!(direction_container, DirectionContainer::Workspace(_))
}

fn toggle_window_direction(
  tiling_window: TilingWindow,
  config: &UserConfig,
) -> anyhow::Result<DirectionContainer> {
  let parent = tiling_window
    .direction_container()
    .context("No direction container.")?;

  // If the window is an only child, then either change the layout of its
  // parent workspace or flatten its parent split container.
  if tiling_window.tiling_siblings().count() == 0 {
    return match parent {
      DirectionContainer::Workspace(workspace) => {
        workspace.set_layout(workspace.layout().inverse());
        Ok(workspace.into())
      }
      DirectionContainer::Split(split_container) => {
        flatten_split_container(split_container.clone())?;

        tiling_window
          .direction_container()
          .context("No direction container.")
      }
    };
  }

  // Create a new split container to wrap the window, using the inverse of
  // the current split direction. For non-split layouts, default to
  // vertical.
  let new_split_layout = match parent.layout() {
    WorkspaceLayout::SplitHorizontal => WorkspaceLayout::SplitVertical,
    _ => WorkspaceLayout::SplitVertical,
  };

  let split_container =
    SplitContainer::new(new_split_layout, config.value.gaps.clone());

  wrap_in_split_container(
    &split_container,
    &parent.into(),
    &[tiling_window.into()],
  )?;

  Ok(split_container.into())
}

pub fn set_tiling_direction(
  container: Container,
  state: &mut WmState,
  config: &UserConfig,
  tiling_direction: &TilingDirection,
) -> anyhow::Result<()> {
  let direction_container = container
    .direction_container()
    .context("No direction container.")?;

  if direction_container.tiling_direction() == *tiling_direction {
    Ok(())
  } else {
    toggle_tiling_direction(container, state, config)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::models::Workspace;

  #[test]
  fn only_workspace_direction_changes_are_custom_layout_changes() {
    let workspace = Workspace::mock().call();
    let split = SplitContainer::mock().call();

    assert!(is_workspace_layout_change(&workspace.into()));
    assert!(!is_workspace_layout_change(&split.into()));
  }
}
