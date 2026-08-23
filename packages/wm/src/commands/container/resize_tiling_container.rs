use crate::{
  models::TilingContainer,
  traits::{CommonGetters, TilingSizeGetters, MIN_TILING_SIZE},
};

/// Applies a collective size delta to sibling containers while preserving
/// the minimum tiling size.
///
/// Normally the delta is weighted by each sibling's space above the
/// minimum. If every sibling is already at the minimum, there is no
/// proportional weight to use, so the delta is shared equally instead.
pub(super) fn redistribute_tiling_size(
  siblings: &[TilingContainer],
  total_delta: f32,
) {
  if siblings.is_empty() || total_delta == 0.0 {
    return;
  }

  let available_size = siblings.iter().fold(0.0, |sum, container| {
    sum + (container.tiling_size() - MIN_TILING_SIZE).max(0.0)
  });

  if available_size > f32::EPSILON && available_size.is_finite() {
    for sibling in siblings {
      let resize_factor = (sibling.tiling_size() - MIN_TILING_SIZE)
        .max(0.0)
        / available_size;
      sibling.set_tiling_size(
        sibling.tiling_size() + resize_factor * total_delta,
      );
    }
  } else {
    #[allow(clippy::cast_precision_loss)]
    let equal_delta = total_delta / siblings.len() as f32;

    for sibling in siblings {
      sibling.set_tiling_size(sibling.tiling_size() + equal_delta);
    }
  }
}

pub fn resize_tiling_container(
  container_to_resize: &TilingContainer,
  target_size: f32,
) {
  let tiling_siblings =
    container_to_resize.tiling_siblings().collect::<Vec<_>>();

  // Ignore cases where the container is the only child.
  if tiling_siblings.is_empty() {
    container_to_resize.set_tiling_size(1.);
    return;
  }

  // Prevent the container from being smaller than the minimum size, and
  // larger than the space available from sibling containers.
  #[allow(clippy::cast_precision_loss)]
  let clamped_target_size = target_size.clamp(
    MIN_TILING_SIZE,
    1. - (tiling_siblings.len() as f32 * MIN_TILING_SIZE),
  );

  let size_delta = clamped_target_size - container_to_resize.tiling_size();
  container_to_resize.set_tiling_size(clamped_target_size);

  // Larger siblings normally absorb more of the adjustment. When all
  // siblings are already at the minimum, the equal-share fallback avoids a
  // zero denominator and lets them grow as the target shrinks.
  redistribute_tiling_size(&tiling_siblings, -size_delta);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    models::{TilingContainer, TilingWindow, Workspace},
    traits::TilingSizeGetters,
  };

  fn workspace_with_two_windows() -> (TilingWindow, TilingWindow) {
    let first = TilingWindow::mock().call();
    let second = TilingWindow::mock().call();
    let _workspace = Workspace::mock()
      .tiling_containers(vec![
        TilingContainer::from(first.clone()),
        TilingContainer::from(second.clone()),
      ])
      .call();

    (first, second)
  }

  #[test]
  fn resize_from_minimum_uses_finite_equal_fallback() {
    let (first, second) = workspace_with_two_windows();
    first.set_tiling_size(0.99);
    second.set_tiling_size(MIN_TILING_SIZE);

    resize_tiling_container(&first.clone().into(), 0.5);

    assert_eq!(first.tiling_size(), 0.5);
    assert_eq!(second.tiling_size(), 0.5);
    assert!(second.tiling_size().is_finite());
  }

  #[test]
  fn unchanged_resize_at_minimum_does_not_corrupt_sibling() {
    let (first, second) = workspace_with_two_windows();
    first.set_tiling_size(0.99);
    second.set_tiling_size(MIN_TILING_SIZE);

    resize_tiling_container(&first.clone().into(), 0.99);

    assert_eq!(first.tiling_size(), 0.99);
    assert_eq!(second.tiling_size(), MIN_TILING_SIZE);
    assert!(second.tiling_size().is_finite());
  }
}
