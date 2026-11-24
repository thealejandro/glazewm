use anyhow::Context;

use super::flatten_split_container;
use crate::{
  models::Container,
  traits::{CommonGetters, TilingSizeGetters, MIN_TILING_SIZE},
};

/// Removes a container from the tree.
///
/// If the container is a tiling container, the siblings will be resized to
/// fill the freed up space. Will flatten empty parent split containers,
/// and remove any ancestor split containers that are left empty (e.g.
/// `V[H[1]]` when 1 is removed).
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

    // TODO: Share logic with `resize_tiling_container`.
    let available_size =
      tiling_siblings.iter().fold(0.0, |sum, container| {
        sum + container.tiling_size() - MIN_TILING_SIZE
      });

    // Adjust size of the siblings based on the freed up space.
    for sibling in &tiling_siblings {
      let resize_factor =
        (sibling.tiling_size() - MIN_TILING_SIZE) / available_size;

      let size_delta = resize_factor * child_to_remove.tiling_size();
      sibling.set_tiling_size(sibling.tiling_size() + size_delta);
    }
  }

  // An empty split container would otherwise remain in the tree and take
  // up space. This happens when the removed container was the only
  // descendant of a chain of split containers.
  if parent.as_split().is_some_and(|split| split.child_count() == 0) {
    detach_container(parent)?;
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use wm_common::{GapsConfig, TilingDirection, WorkspaceConfig};

  use super::detach_container;
  use crate::{
    commands::container::attach_container,
    models::{Container, SplitContainer, Workspace},
    traits::{CommonGetters, TilingSizeGetters},
  };

  fn workspace() -> Workspace {
    Workspace::new(
      WorkspaceConfig {
        name: "1".to_string(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: false,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    )
  }

  fn split(tiling_direction: TilingDirection) -> SplitContainer {
    SplitContainer::new(tiling_direction, GapsConfig::default())
  }

  fn attach(
    child: &SplitContainer,
    parent: &Container,
  ) -> anyhow::Result<()> {
    attach_container(&child.clone().into(), parent, None)
  }

  #[test]
  fn removes_ancestor_splits_left_empty() -> anyhow::Result<()> {
    // W[V[H[1]] 2] where 1 is removed should result in W[2].
    let workspace = workspace();
    let vertical = split(TilingDirection::Vertical);
    let horizontal = split(TilingDirection::Horizontal);
    let first = split(TilingDirection::Vertical);
    let second = split(TilingDirection::Vertical);

    attach(&vertical, &workspace.clone().into())?;
    attach(&second, &workspace.clone().into())?;
    attach(&horizontal, &vertical.clone().into())?;
    attach(&first, &horizontal.clone().into())?;

    detach_container(first.clone().into())?;

    let child_ids = workspace
      .children()
      .iter()
      .map(CommonGetters::id)
      .collect::<Vec<_>>();

    assert_eq!(child_ids, vec![second.id()]);
    assert!(vertical.is_detached());
    assert!(first.is_detached());
    assert!((second.tiling_size() - 1.0).abs() < f32::EPSILON);
    Ok(())
  }
}
