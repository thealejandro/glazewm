//! New tiling windows kept transparent until they reach their place.
//!
//! The OS reports a new window only once it is already visible, so it
//! would otherwise be seen at its original position before jumping into
//! the layout.

use std::{
  collections::HashMap,
  sync::{Arc, Mutex, MutexGuard, PoisonError},
  time::{Duration, Instant},
};

use tracing::warn;
use wm_platform::{
  NativeWindow, NativeWindowWindowsExt, OpacityValue, Rect, WS_EX_LAYERED,
};

/// How long a hidden window may wait for its first redraw before it's
/// shown anyway.
const REDRAW_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to wait for the OS to apply the new position.
const MOVE_TIMEOUT: Duration = Duration::from_millis(250);

/// Interval between checks of the window's position.
const MOVE_POLL_INTERVAL: Duration = Duration::from_millis(4);

/// Interval between fade-in updates (about one frame at 60 Hz).
const FADE_STEP: Duration = Duration::from_millis(16);

/// Progress of a hidden window towards being shown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
  /// Waiting for its first redraw in the layout.
  AwaitingRedraw,
  /// Moving to its place or fading in.
  Revealing,
}

/// Registry of hidden windows by handle, shared with the reveal tasks.
///
/// A window stays in the registry until it's fully shown again, so that
/// `reveal_all` can show it if the WM exits in the meantime.
#[derive(Clone, Debug, Default)]
pub struct HiddenWindows(Arc<Mutex<HashMap<isize, Phase>>>);

impl HiddenWindows {
  /// Makes the window fully transparent until `reveal_when_placed` shows
  /// it, or `REDRAW_TIMEOUT` passes.
  ///
  /// Skipped for windows that are already layered, since their own
  /// transparency couldn't be restored afterwards.
  pub fn hide(&self, native: &NativeWindow) {
    if native.has_window_style_ex(WS_EX_LAYERED) {
      return;
    }

    if let Err(err) =
      native.set_transparency(&OpacityValue::from_alpha(0))
    {
      warn!("Failed to hide window until placed: {}", err);
      return;
    }

    self.mark_hidden(native.id().0);

    let hidden = self.clone();
    let native = native.clone();

    tokio::task::spawn(async move {
      tokio::time::sleep(REDRAW_TIMEOUT).await;
      hidden.reveal_now(&native);
    });
  }

  /// Shows the window once the OS has moved it to `target`, fading it in
  /// over `fade`.
  ///
  /// No-op unless the window is hidden and waiting for its first redraw.
  pub fn reveal_when_placed(
    &self,
    native: &NativeWindow,
    target: Rect,
    fade: Duration,
  ) {
    if !self.start_revealing(native.id().0) {
      return;
    }

    let hidden = self.clone();
    let native = native.clone();

    tokio::task::spawn(async move {
      // Repositioning is asynchronous; wait until it's applied. An error
      // means the window is gone, so there's nothing to wait for.
      let start = Instant::now();
      while start.elapsed() < MOVE_TIMEOUT
        && native.frame_with_shadows().is_ok_and(|frame| frame != target)
      {
        tokio::time::sleep(MOVE_POLL_INTERVAL).await;
      }

      // Alpha follows elapsed time, since sleeps overshoot the timer
      // resolution.
      let fade_start = Instant::now();
      while fade_start.elapsed() < fade {
        let alpha = fade_alpha(fade_start.elapsed(), fade);
        if !hidden.set_alpha_if_hidden(&native, alpha) {
          return;
        }

        tokio::time::sleep(FADE_STEP).await;
      }

      hidden.reveal_now(&native);
    });
  }

  /// Fully shows every hidden window. Used when the WM exits.
  pub fn reveal_all(&self) {
    let mut windows = self.lock();

    for (handle, _) in windows.drain() {
      show(&NativeWindow::from_handle(handle));
    }
  }

  /// Fully shows the window if it's still hidden.
  fn reveal_now(&self, native: &NativeWindow) {
    let mut windows = self.lock();

    if windows.remove(&native.id().0).is_some() {
      show(native);
    }
  }

  /// Sets an intermediate alpha, unless the window was already shown.
  ///
  /// Returns whether the window is still hidden.
  fn set_alpha_if_hidden(&self, native: &NativeWindow, alpha: u8) -> bool {
    // Held while setting the alpha so `reveal_now` can't run in between.
    let windows = self.lock();

    if !windows.contains_key(&native.id().0) {
      return false;
    }

    native
      .set_transparency(&OpacityValue::from_alpha(alpha))
      .is_ok()
  }

  fn lock(&self) -> MutexGuard<'_, HashMap<isize, Phase>> {
    self.0.lock().unwrap_or_else(PoisonError::into_inner)
  }

  /// Registers a window that has just been hidden.
  fn mark_hidden(&self, handle: isize) {
    self.lock().insert(handle, Phase::AwaitingRedraw);
  }

  /// Moves the window to `Phase::Revealing`.
  ///
  /// Returns `false` if it isn't hidden or is already being revealed.
  fn start_revealing(&self, handle: isize) -> bool {
    match self.lock().get_mut(&handle) {
      Some(phase @ Phase::AwaitingRedraw) => {
        *phase = Phase::Revealing;
        true
      }
      _ => false,
    }
  }
}

/// Makes the window opaque and drops the layered style added by
/// `HiddenWindows::hide`, which would otherwise slow down its drawing.
fn show(native: &NativeWindow) {
  if let Err(err) =
    native.set_transparency(&OpacityValue::from_alpha(u8::MAX))
  {
    warn!("Failed to show window after placing it: {}", err);
  }

  native.remove_window_style_ex(WS_EX_LAYERED);
}

/// Alpha after `elapsed` of a fade-in lasting `duration`.
///
/// Returns at least 1 while fading, and fully opaque once `duration` has
/// passed.
fn fade_alpha(elapsed: Duration, duration: Duration) -> u8 {
  if elapsed >= duration {
    return u8::MAX;
  }

  let alpha =
    elapsed.as_micros() * u128::from(u8::MAX) / duration.as_micros();

  u8::try_from(alpha).map_or(u8::MAX, |alpha| alpha.max(1))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn reveals_only_once_after_being_hidden() {
    let hidden = HiddenWindows::default();
    assert!(!hidden.start_revealing(1), "never hidden");

    hidden.mark_hidden(1);
    assert!(hidden.start_revealing(1));
    assert!(!hidden.start_revealing(1), "already revealing");
  }

  #[test]
  fn fade_alpha_follows_elapsed_time() {
    let ms = Duration::from_millis;
    assert_eq!(fade_alpha(ms(0), ms(120)), 1);
    assert_eq!(fade_alpha(ms(60), ms(120)), 127);
    assert_eq!(fade_alpha(ms(119), ms(120)), 252);
    assert_eq!(fade_alpha(ms(120), ms(120)), u8::MAX);
    assert_eq!(fade_alpha(ms(500), ms(120)), u8::MAX);
    assert_eq!(fade_alpha(ms(0), Duration::ZERO), u8::MAX);
  }
}
