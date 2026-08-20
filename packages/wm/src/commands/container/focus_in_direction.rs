use anyhow::Context;
use wm_common::{TilingDirection, WindowState};
use wm_platform::{Direction, Rect};

use super::set_focused_descendant;
use crate::{
  models::{Container, DirectionContainer, TilingContainer},
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters, WindowGetters,
  },
  wm_state::WmState,
};

pub fn focus_in_direction(
  origin_container: &Container,
  direction: &Direction,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let focus_target = match origin_container {
    Container::TilingWindow(_) => {
      // If a suitable focus target isn't found in the current workspace,
      // attempt to find a workspace in the given direction.
      tiling_focus_target(origin_container, direction)?.map_or_else(
        || workspace_focus_target(origin_container, direction, state),
        |container| Ok(Some(container)),
      )?
    }
    Container::NonTilingWindow(ref non_tiling_window) => {
      match non_tiling_window.state() {
        WindowState::Floating(_) => {
          floating_focus_target(origin_container, direction)
        }
        WindowState::Fullscreen(_) => {
          workspace_focus_target(origin_container, direction, state)?
        }
        _ => None,
      }
    }
    Container::Workspace(_) => {
      workspace_focus_target(origin_container, direction, state)?
    }
    _ => None,
  };

  // Set focus to the target container.
  if let Some(focus_target) = focus_target {
    set_focused_descendant(&focus_target, None);
    state.pending_sync.queue_focus_change().queue_cursor_jump();
  }

  Ok(())
}

fn floating_focus_target(
  origin_container: &Container,
  direction: &Direction,
) -> Option<Container> {
  let is_floating = |sibling: &Container| {
    sibling.as_non_tiling_window().is_some_and(|window| {
      matches!(window.state(), WindowState::Floating(_))
    })
  };

  let mut floating_siblings =
    origin_container.siblings().filter(is_floating);

  // Wrap if next/previous floating window is not found.
  match direction {
    Direction::Left => origin_container
      .next_siblings()
      .find(is_floating)
      .or_else(|| floating_siblings.last()),
    Direction::Right => origin_container
      .prev_siblings()
      .find(is_floating)
      .or_else(|| floating_siblings.next()),
    // Cannot focus vertically from a floating window.
    _ => None,
  }
}

/// Gets a focus target within the current workspace. Traverse upwards from
/// the origin container to find an adjacent container that can be focused.
fn tiling_focus_target(
  origin_container: &Container,
  direction: &Direction,
) -> anyhow::Result<Option<Container>> {
  let tiling_direction = TilingDirection::from_direction(direction);
  let mut origin_or_ancestor = origin_container.clone();

  // Traverse upwards from the focused container. Stop searching when a
  // workspace is encountered.
  while !origin_or_ancestor.is_workspace() {
    let parent_container = origin_or_ancestor
      .parent()
      .context("No parent container.")?;

    // Non-direction containers (e.g. `StackContainer`) are transparent to
    // directional navigation — skip past them to their own parent.
    let parent = match parent_container.as_direction_container() {
      Ok(p) => p,
      Err(_) => {
        origin_or_ancestor = parent_container;
        continue;
      }
    };

    let focus_target = if parent.layout().is_split() {
      // Preserve the original tree-order behavior for split layouts.
      if parent.tiling_direction() != tiling_direction {
        origin_or_ancestor = parent.into();
        continue;
      }

      match direction {
        Direction::Up | Direction::Left => origin_or_ancestor
          .prev_siblings()
          .find_map(|c| c.as_tiling_container().ok()),
        _ => origin_or_ancestor
          .next_siblings()
          .find_map(|c| c.as_tiling_container().ok()),
      }
    } else {
      // Grid, rows, columns, and master-stack layouts are two-dimensional;
      // choose by rendered position instead of child order.
      spatial_tiling_sibling(&origin_or_ancestor, &parent, direction)?
    };

    match focus_target {
      Some(target) => {
        // Return once a suitable focus target is found.
        return Ok(match target {
          TilingContainer::TilingWindow(_) => Some(target.into()),
          TilingContainer::Split(split) => split
            .descendant_in_direction(&direction.inverse())
            .map(Into::into),
          TilingContainer::Stack(stack) => stack
            .child_focus_order()
            .find_map(|c| c.into_tiling_window().ok())
            .map(Into::into),
        });
      }
      None => origin_or_ancestor = parent.into(),
    }
  }

  Ok(None)
}

/// Finds the visually nearest direct tiling child in `direction`.
fn spatial_tiling_sibling(
  origin: &Container,
  parent: &DirectionContainer,
  direction: &Direction,
) -> anyhow::Result<Option<TilingContainer>> {
  let origin_rect = origin.to_rect()?;

  Ok(parent
    .tiling_children()
    .filter(|candidate| candidate.id() != origin.id())
    .filter_map(|candidate| {
      let candidate_rect = candidate.to_rect().ok()?;
      directional_score(&origin_rect, &candidate_rect, direction)
        .map(|score| (score, candidate))
    })
    .min_by_key(|(score, _)| *score)
    .map(|(_, candidate)| candidate))
}

/// Produces an ordering key for a candidate rectangle in `direction`.
fn directional_score(
  origin: &Rect,
  candidate: &Rect,
  direction: &Direction,
) -> Option<(i32, i32, i32)> {
  let origin_center = origin.center_point();
  let candidate_center = candidate.center_point();

  let (primary_distance, perpendicular_distance, overlap) = match direction {
    Direction::Left => (
      origin_center.x - candidate_center.x,
      (origin_center.y - candidate_center.y).abs(),
      origin.y_overlap(candidate),
    ),
    Direction::Right => (
      candidate_center.x - origin_center.x,
      (origin_center.y - candidate_center.y).abs(),
      origin.y_overlap(candidate),
    ),
    Direction::Up => (
      origin_center.y - candidate_center.y,
      (origin_center.x - candidate_center.x).abs(),
      origin.x_overlap(candidate),
    ),
    Direction::Down => (
      candidate_center.y - origin_center.y,
      (origin_center.x - candidate_center.x).abs(),
      origin.x_overlap(candidate),
    ),
  };

  if primary_distance <= 0 {
    return None;
  }

  let overlap_penalty = if overlap > 0 { 0 } else { 1 };
  Some((
    overlap_penalty,
    primary_distance,
    perpendicular_distance,
  ))
}

/// Gets a focus target outside of the current workspace in the given
/// direction.
///
/// This will descend into the workspace in the given direction, and will
/// always return a tiling container. This makes it different from the
/// `focus_workspace` command with `FocusWorkspaceTarget::Direction`.
fn workspace_focus_target(
  origin_container: &Container,
  direction: &Direction,
  state: &WmState,
) -> anyhow::Result<Option<Container>> {
  let monitor = origin_container.monitor().context("No monitor.")?;

  let target_workspace = state
    .monitor_in_direction(&monitor, direction)?
    .and_then(|monitor| monitor.displayed_workspace());

  let focused_fullscreen = target_workspace
    .as_ref()
    .and_then(|workspace| workspace.descendant_focus_order().next())
    .filter(|focused| match focused {
      Container::NonTilingWindow(window) => {
        matches!(window.state(), WindowState::Fullscreen(_))
      }
      _ => false,
    });

  let focus_target = focused_fullscreen
    .or_else(|| {
      target_workspace.as_ref().and_then(|workspace| {
        workspace
          .descendant_in_direction(&direction.inverse())
          .map(Into::into)
      })
    })
    .or(target_workspace.map(Into::into));

  Ok(focus_target)
}
