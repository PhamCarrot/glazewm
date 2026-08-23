use anyhow::Context;

use super::{
  flatten_split_container, flatten_stack_container,
  redistribute_tiling_size,
};
use crate::{
  models::Container,
  traits::{CommonGetters, TilingSizeGetters},
};

/// Removes a container from the tree.
///
/// If the container is a tiling container, the siblings will be resized to
/// fill the freed up space. Will flatten empty parent split containers.
#[allow(clippy::needless_pass_by_value)]
pub fn detach_container(child_to_remove: Container) -> anyhow::Result<()> {
  // Flatten the parent split container if it'll be empty after removing
  // the child.
  if let Some(split_parent) = child_to_remove
    .parent()
    .and_then(|parent| parent.as_split().cloned())
  {
    if split_parent.child_count() == 1 {
      flatten_split_container(split_parent)?;
    }
  }

  // Flatten the parent stack container only when it will be completely
  // empty after removal. A single-child stack is kept so the user's named
  // stack persists and new windows can be moved into it later.
  if let Some(stack_parent) = child_to_remove
    .parent()
    .and_then(|parent| parent.as_stack().cloned())
  {
    if stack_parent.child_count() <= 1 {
      flatten_stack_container(stack_parent)?;
    }
  }

  let parent = child_to_remove.parent().context("No parent.")?;

  parent
    .borrow_children_mut()
    .retain(|c| c.id() != child_to_remove.id());

  parent
    .borrow_child_focus_order_mut()
    .retain(|id| *id != child_to_remove.id());

  *child_to_remove.borrow_parent_mut() = None;

  // Resize the siblings if it is a tiling container.
  if let Ok(child_to_remove) = child_to_remove.as_tiling_container() {
    let tiling_siblings = parent.tiling_children().collect::<Vec<_>>();

    // Give the freed space to siblings in proportion to their space above
    // the minimum. If all remaining siblings are at the minimum, share
    // it equally rather than dividing by zero and corrupting their
    // sizes with NaN.
    redistribute_tiling_size(
      &tiling_siblings,
      child_to_remove.tiling_size(),
    );
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    models::{TilingContainer, TilingWindow, Workspace},
    traits::{TilingSizeGetters, MIN_TILING_SIZE},
  };

  #[test]
  fn detach_into_minimum_sibling_restores_finite_full_size() {
    let removed = TilingWindow::mock().call();
    let survivor = TilingWindow::mock().call();
    let _workspace = Workspace::mock()
      .tiling_containers(vec![
        TilingContainer::from(removed.clone()),
        TilingContainer::from(survivor.clone()),
      ])
      .call();
    removed.set_tiling_size(0.99);
    survivor.set_tiling_size(MIN_TILING_SIZE);

    detach_container(removed.into()).unwrap();

    assert_eq!(survivor.tiling_size(), 1.0);
    assert!(survivor.tiling_size().is_finite());
  }
}
