use anyhow::Context;
use wm_common::{
  try_warn, ActiveDrag, FullscreenStateConfig, InitialWindowState,
  TilingDirection, WindowState,
};
use wm_platform::{LengthValue, Point, Rect};

use crate::{
  commands::{
    container::{move_container_within_tree, wrap_in_split_container},
    window::{is_floating_monitor, set_window_size, update_window_state},
  },
  events::update_floating_window_position,
  models::{
    DirectionContainer, Monitor, NonTilingWindow, SplitContainer, TilingContainer,
    WindowContainer,
  },
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters, WindowGetters,
  },
  user_config::UserConfig,
  wm_state::WmState,
};

/// Handles the event for when a window is finished being moved or resized
/// by the user (e.g. via the window's drag handles).
///
/// This resizes the window if it's a tiling window and attach a dragged
/// floating window.
///
/// TODO: Move this to a better location - maybe a new `active_drag_ext`
/// mod.
pub fn handle_window_moved_or_resized_end(
  window: &WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let Some(active_drag) = window.active_drag() else {
    return Ok(());
  };

  match &window {
    WindowContainer::NonTilingWindow(window) => {
      let is_maximized = try_warn!(window.native().is_maximized());

      window.update_native_properties(|properties| {
        properties.is_maximized = is_maximized;
      });

      let nearest_monitor = state
        .nearest_monitor(&window.native())
        .context("Failed to get workspace of nearest monitor.")?;

      let should_fullscreen = window.should_fullscreen(
        &nearest_monitor
          .displayed_workspace()
          .context("No workspace.")?,
      )?;

      if is_maximized || should_fullscreen {
        let fullscreen_state = if let WindowState::Fullscreen(
          fullscreen_state,
        ) = window.state()
        {
          fullscreen_state
        } else {
          config
            .value
            .window_behavior
            .state_defaults
            .fullscreen
            .clone()
        };

        let window = update_window_state(
          window.clone().into(),
          WindowState::Fullscreen(FullscreenStateConfig {
            maximized: is_maximized,
            ..fullscreen_state
          }),
          state,
          config,
        )?;

        window.set_active_drag(None);

        if is_maximized {
          // Dequeue the window from redraw if it's maximized, since the
          // window is already in the correct state.
          state
            .pending_sync
            .dequeue_container_from_redraw(window.clone());
        } else {
          // Force a redraw to snap the window to the monitor edges.
          // TODO: Skip redraw if it's already matches fullscreen frame.
          state.pending_sync.queue_container_to_redraw(window.clone());
        }

        return Ok(());
      }

      let tiling_by_default = matches!(
        config.value.window_behavior.initial_state,
        InitialWindowState::Tiling
      );

      if should_drop_as_tiling(
        active_drag.is_from_floating,
        monitor_change(&active_drag, &nearest_monitor, state, config),
        tiling_by_default,
      ) {
        let window = drop_as_tiling_window(window, state, config)?;
        window.set_active_drag(None);
      } else {
        update_floating_window_position(
          window,
          window.native_properties().frame,
          &nearest_monitor,
          state,
        )?;
        window.set_active_drag(None);
      }
    }
    WindowContainer::TilingWindow(window) => {
      tracing::info!(
        "Tiling window move/resize ended: {}",
        window.as_window_container()?
      );

      let frame = window.native_properties().frame;

      // Update the window's size based on the new frame position. This
      // means we use the actual window dimensions as the source of truth.
      set_window_size(
        window.clone().into(),
        Some(LengthValue::from_px(frame.width())),
        Some(LengthValue::from_px(frame.height())),
        state,
      )?;

      window.set_active_drag(None);

      // Force a redraw of the window to snap it back to its original
      // position. This is necessary when:
      // - The window is the only tiling window in the workspace.
      // - The window is not past the movement threshold for transitioning
      //   to floating while being dragged.
      // - Resizing in a direction that doesn't change the window's tiling
      //   size.
      state.pending_sync.queue_container_to_redraw(window.clone());
    }
  }

  Ok(())
}

/// Change of monitor of a dragged window, by whether each monitor is in
/// `floating_monitors`.
#[derive(Clone, Copy, Debug)]
struct MonitorChange {
  from_floating_monitor: bool,
  to_floating_monitor: bool,
}

/// Gets the change of monitor between the drag start and `nearest_monitor`.
///
/// Returns `None` if the window stayed on the same monitor.
fn monitor_change(
  active_drag: &ActiveDrag,
  nearest_monitor: &Monitor,
  state: &WmState,
  config: &UserConfig,
) -> Option<MonitorChange> {
  let start_monitor = state
    .monitor_at_point(&active_drag.initial_position.center_point())?;

  (start_monitor.id() != nearest_monitor.id()).then(|| MonitorChange {
    from_floating_monitor: is_floating_monitor(&start_monitor, config),
    to_floating_monitor: is_floating_monitor(nearest_monitor, config),
  })
}

/// Gets whether a dragged non-tiling window is dropped as tiling.
///
/// A window that was tiling before the drag returns to tiling, unless it's
/// dropped on another monitor that is in `floating_monitors`. A floating
/// window stays floating, unless it's moved from a floating monitor onto
/// one that isn't, and new windows are tiling by default.
fn should_drop_as_tiling(
  is_from_floating: bool,
  monitor_change: Option<MonitorChange>,
  tiling_by_default: bool,
) -> bool {
  match monitor_change {
    None => !is_from_floating,
    Some(change) if is_from_floating => {
      change.from_floating_monitor
        && !change.to_floating_monitor
        && tiling_by_default
    }
    Some(change) => !change.to_floating_monitor,
  }
}

/// Handles transition from temporary floating window to tiling window on
/// drag end.
#[allow(clippy::too_many_lines)]
fn drop_as_tiling_window(
  moved_window: &NonTilingWindow,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  tracing::info!(
    "Tiling window drag ended: {}",
    moved_window.as_window_container()?
  );

  let mouse_pos = state.dispatcher.cursor_position()?;
  let mouse_workspace = state
    .monitor_at_point(&mouse_pos)
    .and_then(|monitor| monitor.displayed_workspace())
    .or_else(|| moved_window.workspace())
    .context("Couldn't find workspace for window drop.")?;

  // Get the workspace, split containers, and other windows under the
  // dragged window.
  let containers_at_pos = state
    .containers_at_point(&mouse_workspace.clone().into(), &mouse_pos)
    .into_iter()
    .filter(|container| container.id() != moved_window.id());

  // Get the deepest direction container under the dragged window.
  let target_parent: DirectionContainer = containers_at_pos
    .filter_map(|container| container.as_direction_container().ok())
    .fold(mouse_workspace.into(), |acc, container| {
      if container.ancestors().count() > acc.ancestors().count() {
        container
      } else {
        acc
      }
    });

  // If the target parent has no children (i.e. an empty workspace), then
  // add the window directly.
  if target_parent.tiling_children().count() == 0 {
    move_container_within_tree(
      &moved_window.clone().into(),
      &target_parent.clone().into(),
      0,
      state,
    )?;

    moved_window.set_insertion_target(None);

    return update_window_state(
      moved_window.as_window_container()?,
      WindowState::Tiling,
      state,
      config,
    );
  }

  let nearest_container = target_parent
    .children()
    .into_iter()
    .filter_map(|container| container.as_tiling_container().ok())
    .try_fold(None, |acc: Option<TilingContainer>, container| match acc {
      Some(acc) => {
        let is_nearer = acc.to_rect()?.distance_to_point(&mouse_pos)
          < container.to_rect()?.distance_to_point(&mouse_pos);

        anyhow::Ok(Some(if is_nearer { acc } else { container }))
      }
      None => Ok(Some(container)),
    })?
    .context("No nearest container.")?;

  let tiling_direction = target_parent.tiling_direction();
  let drop_position =
    drop_position(&mouse_pos, &nearest_container.to_rect()?);

  let moved_window = update_window_state(
    moved_window.clone().into(),
    WindowState::Tiling,
    state,
    config,
  )?;

  let should_split = nearest_container.is_tiling_window()
    && match tiling_direction {
      TilingDirection::Horizontal => {
        drop_position == DropPosition::Top
          || drop_position == DropPosition::Bottom
      }
      TilingDirection::Vertical => {
        drop_position == DropPosition::Left
          || drop_position == DropPosition::Right
      }
    };

  if should_split {
    let split_container = SplitContainer::new(
      tiling_direction.inverse(),
      config.value.gaps.clone(),
    );

    wrap_in_split_container(
      &split_container,
      &target_parent.clone().into(),
      &[nearest_container],
    )?;

    let target_index = match drop_position {
      DropPosition::Top | DropPosition::Left => 0,
      _ => 1,
    };

    move_container_within_tree(
      &moved_window.clone().into(),
      &split_container.into(),
      target_index,
      state,
    )?;
  } else {
    let target_index = match drop_position {
      DropPosition::Top | DropPosition::Left => nearest_container.index(),
      _ => nearest_container.index() + 1,
    };

    move_container_within_tree(
      &moved_window.clone().into(),
      &target_parent.clone().into(),
      target_index,
      state,
    )?;
  }

  state.pending_sync.queue_container_to_redraw(target_parent);

  Ok(moved_window)
}

/// Represents where the window was dropped over another.
#[derive(Debug, Clone, PartialEq)]
enum DropPosition {
  Top,
  Bottom,
  Left,
  Right,
}

/// Gets the drop position for a window based on the mouse position.
///
/// This approach divides the window rect into an "X", creating four
/// triangular quadrants, to determine which side the cursor is closest to.
fn drop_position(mouse_pos: &Point, rect: &Rect) -> DropPosition {
  let delta_x = mouse_pos.x - rect.center_point().x;
  let delta_y = mouse_pos.y - rect.center_point().y;

  if delta_x.abs() > delta_y.abs() {
    // Window is in the left or right triangle.
    if delta_x > 0 {
      DropPosition::Right
    } else {
      DropPosition::Left
    }
  } else {
    // Window is in the top or bottom triangle.
    if delta_y > 0 {
      DropPosition::Bottom
    } else {
      DropPosition::Top
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn floating_window_tiles_when_moved_off_a_floating_monitor() {
    let drop = |from_floating_monitor, to_floating_monitor, tiling| {
      should_drop_as_tiling(
        true,
        Some(MonitorChange {
          from_floating_monitor,
          to_floating_monitor,
        }),
        tiling,
      )
    };

    assert!(drop(true, false, true));
    assert!(!drop(true, false, false), "floating by default");
    assert!(!drop(false, false, true), "floated by the user");
    assert!(!drop(true, true, true), "onto another floating monitor");
    assert!(!should_drop_as_tiling(true, None, true), "same monitor");
  }

  #[test]
  fn tiling_window_floats_when_moved_onto_a_floating_monitor() {
    let change = |to_floating_monitor| {
      Some(MonitorChange {
        from_floating_monitor: false,
        to_floating_monitor,
      })
    };

    assert!(!should_drop_as_tiling(false, change(true), true));
    assert!(should_drop_as_tiling(false, change(false), true));
    assert!(should_drop_as_tiling(false, None, true), "same monitor");
  }
}
