use anyhow::Context;
use tracing::info;
use wm_common::{try_warn, WindowState};
use wm_platform::NativeWindow;

use crate::{
  commands::window::update_window_state,
  models::WindowContainer,
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub fn handle_window_minimize_ended(
  native_window: &NativeWindow,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let found_window = state.window_from_native(native_window);

  // Update the window's state to not be minimized.
  if let Some(window) = found_window {
    let is_minimized = try_warn!(window.native().is_minimized());

    window.update_native_properties(|properties| {
      properties.is_minimized = is_minimized;
    });

    if !is_minimized && window.state() == WindowState::Minimized {
      info!("Window minimize ended: {window}");

      if window.has_pending_placement_update() {
        try_warn!(update_floating_placement(&window));
      }

      let target_state = window
        .prev_state()
        .unwrap_or(WindowState::default_from_config(&config.value));

      update_window_state(window.clone(), target_state, state, config)?;
    }
  }

  Ok(())
}

/// Takes the window's floating placement from its current (restored)
/// frame.
///
/// Used for windows that were managed while minimized, whose placement was
/// otherwise taken from their minimized frame.
fn update_floating_placement(
  window: &WindowContainer,
) -> anyhow::Result<()> {
  let workspace = window.workspace().context("No workspace.")?;
  let max_workspace_rect = workspace.max_workspace_rect()?;

  // Clamp the size the same way as when the window is first managed.
  let placement = window.native().frame()?.clamp_size(
    max_workspace_rect.width() - 10,
    max_workspace_rect.height() - 10,
  );

  window.set_floating_placement(placement);
  window.set_has_pending_placement_update(false);
  Ok(())
}
