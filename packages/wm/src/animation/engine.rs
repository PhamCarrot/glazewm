use std::time::{Duration, Instant};

use wm_common::EasingFunction;

/// Calculates the current progress of an animation (0.0 to 1.0).
#[cfg(test)]
pub fn animation_progress(start_time: Instant, duration: Duration) -> f32 {
  animation_progress_at(start_time, duration, Instant::now())
}

/// Calculates the animation progress at an explicit `now` instant (0.0 to
/// 1.0).
///
/// Allows callers to supply a predictive timestamp (e.g. vsync wake-up
/// time plus an estimated pipeline offset) so the computed position aligns
/// with the DWM composition event rather than the moment `update_internal`
/// runs. Uses `saturating_duration_since` so a `now` that precedes
/// `start_time` (possible on the first frame when `pipeline_offset` >
/// elapsed) returns `0.0` instead of panicking.
pub fn animation_progress_at(
  start_time: Instant,
  duration: Duration,
  now: Instant,
) -> f32 {
  let elapsed = now.saturating_duration_since(start_time);

  if elapsed >= duration {
    return 1.0;
  }

  #[allow(clippy::cast_precision_loss)]
  let progress = elapsed.as_millis() as f32 / duration.as_millis() as f32;
  progress.clamp(0.0, 1.0)
}

/// Applies an easing function to a linear progress value (0.0 to 1.0).
pub fn apply_easing(progress: f32, easing: &EasingFunction) -> f32 {
  match easing {
    EasingFunction::EaseOutSpring => ease_out_spring(progress),
    EasingFunction::EaseInQuart => progress.powi(4),
    EasingFunction::EaseOutQuart => 1.0 - (1.0 - progress).powi(4),
    EasingFunction::EaseInOutQuart => ease_in_out_power(progress, 4),
    EasingFunction::EaseInQuint => progress.powi(5),
    EasingFunction::EaseOutQuint => 1.0 - (1.0 - progress).powi(5),
    EasingFunction::EaseInOutQuint => ease_in_out_power(progress, 5),
    EasingFunction::EaseInExpo => ease_in_expo(progress),
    EasingFunction::EaseOutExpo => ease_out_expo(progress),
    EasingFunction::EaseInOutExpo => ease_in_out_expo(progress),
    EasingFunction::EaseInBack => ease_in_back(progress),
    EasingFunction::EaseOutBack => ease_out_back(progress),
    EasingFunction::EaseInOutBack => ease_in_out_back(progress),
    EasingFunction::CubicBezier(x1, y1, x2, y2) => {
      cubic_bezier(*x1, *y1, *x2, *y2, progress)
    }
  }
}

fn ease_in_out_power(t: f32, exponent: i32) -> f32 {
  if t < 0.5 {
    (2.0 * t).powi(exponent) / 2.0
  } else {
    1.0 - (-2.0 * t + 2.0).powi(exponent) / 2.0
  }
}

fn ease_in_expo(t: f32) -> f32 {
  if t <= 0.0 {
    0.0
  } else {
    2.0f32.powf(10.0 * t - 10.0)
  }
}

fn ease_out_expo(t: f32) -> f32 {
  if t >= 1.0 {
    1.0
  } else {
    1.0 - 2.0f32.powf(-10.0 * t)
  }
}

fn ease_in_out_expo(t: f32) -> f32 {
  if t <= 0.0 {
    0.0
  } else if t >= 1.0 {
    1.0
  } else if t < 0.5 {
    2.0f32.powf(20.0 * t - 10.0) / 2.0
  } else {
    (2.0 - 2.0f32.powf(-20.0 * t + 10.0)) / 2.0
  }
}

const BACK_OVERSHOOT: f32 = 1.70158;

fn ease_in_back(t: f32) -> f32 {
  let c3 = BACK_OVERSHOOT + 1.0;
  c3 * t.powi(3) - BACK_OVERSHOOT * t.powi(2)
}

fn ease_out_back(t: f32) -> f32 {
  let c3 = BACK_OVERSHOOT + 1.0;
  1.0 + c3 * (t - 1.0).powi(3) + BACK_OVERSHOOT * (t - 1.0).powi(2)
}

fn ease_in_out_back(t: f32) -> f32 {
  let c2 = BACK_OVERSHOOT * 1.525;
  if t < 0.5 {
    (2.0 * t).powi(2) * ((c2 + 1.0) * 2.0 * t - c2) / 2.0
  } else {
    ((2.0 * t - 2.0).powi(2) * ((c2 + 1.0) * (t * 2.0 - 2.0) + c2) + 2.0)
      / 2.0
  }
}

/// Evaluates a CSS cubic bezier at the given `x` progress (0.0 to 1.0).
///
/// Control points `(x1, y1)` and `(x2, y2)` define the curve between the
/// implicit anchors `(0, 0)` and `(1, 1)`. Uses Newton-Raphson iteration
/// to find the curve parameter `t` such that `Bx(t) = x`, then returns
/// `By(t)`.
fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
  let cx = 3.0 * x1;
  let bx = 3.0 * (x2 - x1) - cx;
  let ax = 1.0 - cx - bx;

  let cy = 3.0 * y1;
  let by_ = 3.0 * (y2 - y1) - cy;
  let ay = 1.0 - cy - by_;

  let sample_x = |t: f32| ((ax * t + bx) * t + cx) * t;
  let sample_dx = |t: f32| (3.0 * ax * t + 2.0 * bx) * t + cx;
  let sample_y = |t: f32| ((ay * t + by_) * t + cy) * t;

  let mut t = x;
  for _ in 0..8 {
    let dx = sample_dx(t);
    if dx.abs() < 1e-6 {
      break;
    }
    t = (t - (sample_x(t) - x) / dx).clamp(0.0, 1.0);
  }

  sample_y(t)
}

/// Exponentially-decaying spring easing function.
///
/// Produces an underdamped spring effect: the value overshoots past 1.0,
/// oscillates, and settles. Runs to full wall-clock duration (not cut off
/// at 99%) to preserve the bounce.
fn ease_out_spring(t: f32) -> f32 {
  if t <= 0.0 {
    return 0.0;
  }
  if t >= 1.0 {
    return 1.0;
  }
  let c4 = (2.0 * std::f32::consts::PI) / 2.0;
  2.0f32.powf(-12.0 * t) * ((t * 4.0 - 4.5) * c4).sin() * -1.0 + 1.0
}

#[cfg(test)]
mod tests {
  use wm_platform::Rect;

  use super::*;

  #[test]
  fn test_animation_progress() {
    let start = Instant::now() - Duration::from_millis(50);
    let duration = Duration::from_millis(100);

    let progress = animation_progress(start, duration);
    assert!((progress - 0.5).abs() < 0.1);
  }

  #[test]
  fn test_interpolate_rect() {
    let start = Rect::from_xy(0, 0, 100, 100);
    let end = Rect::from_xy(100, 100, 200, 200);

    let mid = start.interpolate(&end, 0.5);
    assert_eq!(mid.x(), 50);
    assert_eq!(mid.y(), 50);
    assert_eq!(mid.width(), 150);
    assert_eq!(mid.height(), 150);
  }

  #[test]
  fn exact_named_easings_have_stable_endpoints_and_midpoints() {
    let cases = [
      (EasingFunction::EaseInQuart, 0.0625),
      (EasingFunction::EaseOutQuart, 0.9375),
      (EasingFunction::EaseInOutQuart, 0.5),
      (EasingFunction::EaseInQuint, 0.03125),
      (EasingFunction::EaseOutQuint, 0.96875),
      (EasingFunction::EaseInOutQuint, 0.5),
      (EasingFunction::EaseInOutExpo, 0.5),
    ];

    for (easing, expected_midpoint) in cases {
      assert_eq!(apply_easing(0.0, &easing), 0.0);
      assert_eq!(apply_easing(1.0, &easing), 1.0);
      assert!(
        (apply_easing(0.5, &easing) - expected_midpoint).abs() < 1e-6
      );
    }

    assert_eq!(apply_easing(0.0, &EasingFunction::EaseInExpo), 0.0);
    assert_eq!(apply_easing(1.0, &EasingFunction::EaseInExpo), 1.0);
    assert_eq!(apply_easing(0.0, &EasingFunction::EaseOutExpo), 0.0);
    assert_eq!(apply_easing(1.0, &EasingFunction::EaseOutExpo), 1.0);
  }

  #[test]
  fn back_curves_produce_true_overshoot_and_finish_exactly() {
    assert!(apply_easing(0.2, &EasingFunction::EaseInBack) < 0.0);
    assert!(apply_easing(0.8, &EasingFunction::EaseOutBack) > 1.0);
    assert!(apply_easing(0.2, &EasingFunction::EaseInOutBack) < 0.0);
    assert!(apply_easing(0.8, &EasingFunction::EaseInOutBack) > 1.0);

    for easing in [
      EasingFunction::EaseInBack,
      EasingFunction::EaseOutBack,
      EasingFunction::EaseInOutBack,
    ] {
      assert_eq!(apply_easing(0.0, &easing), 0.0);
      assert_eq!(apply_easing(1.0, &easing), 1.0);
      assert!(easing.can_overshoot());
    }
  }
}
