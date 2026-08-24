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
  if x <= 0.0 {
    return 0.0;
  }
  if x >= 1.0 {
    return 1.0;
  }

  // The named `linear` curve must be exactly linear. Besides avoiding a
  // solve, this preserves exact identity even at progress values close to
  // the endpoints where the curve derivative approaches zero.
  if x1 == 0.0 && y1 == 0.0 && x2 == 1.0 && y2 == 1.0 {
    return x;
  }

  let cx = 3.0 * x1;
  let bx = 3.0 * (x2 - x1) - cx;
  let ax = 1.0 - cx - bx;

  let cy = 3.0 * y1;
  let by_ = 3.0 * (y2 - y1) - cy;
  let ay = 1.0 - cy - by_;

  let sample_x = |t: f32| ((ax * t + bx) * t + cx) * t;
  let sample_dx = |t: f32| (3.0 * ax * t + 2.0 * bx) * t + cx;
  let sample_y = |t: f32| ((ay * t + by_) * t + cy) * t;
  let bounded_y = (0.0..=1.0).contains(&y1) && (0.0..=1.0).contains(&y2);
  let output = |t: f32| {
    let y = sample_y(t);
    if bounded_y {
      y.clamp(0.0, 1.0)
    } else {
      y
    }
  };
  const X_TOLERANCE: f32 = 1e-7;

  // Newton-Raphson converges quickly for ordinary easing curves. Valid CSS
  // curves may nevertheless have a flat x derivative (for example
  // x1=x2=0), so fall back to a bounded binary search whenever Newton
  // cannot prove a sufficiently accurate inverse. This keeps the common
  // path fast while making every accepted curve stable and bounded.
  let mut t = x;
  for _ in 0..8 {
    let error = sample_x(t) - x;
    if error.abs() <= X_TOLERANCE {
      return output(t);
    }
    let dx = sample_dx(t);
    if dx.abs() < 1e-6 {
      break;
    }
    t = (t - error / dx).clamp(0.0, 1.0);
  }

  if (sample_x(t) - x).abs() <= X_TOLERANCE {
    return output(t);
  }

  let mut lower = 0.0;
  let mut upper = 1.0;
  for _ in 0..24 {
    t = (lower + upper) * 0.5;
    if sample_x(t) < x {
      lower = t;
    } else {
      upper = t;
    }
  }

  output((lower + upper) * 0.5)
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
  1.0 - 2.0f32.powf(-12.0 * t) * ((t * 4.0 - 4.5) * c4).sin()
}

#[cfg(test)]
mod tests {
  use wm_platform::Rect;

  use super::*;

  const PROGRESS_POINTS: [f32; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];

  fn assert_close(actual: f32, expected: f32) {
    assert!(
      (actual - expected).abs() <= 2e-5,
      "expected {expected:.9}, got {actual:.9}"
    );
  }

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
  fn every_named_easing_matches_canonical_reference_values() {
    let cases = [
      (
        EasingFunction::CubicBezier(0.0, 0.0, 1.0, 1.0),
        [0.0, 0.25, 0.5, 0.75, 1.0],
      ),
      (
        EasingFunction::CubicBezier(0.42, 0.0, 1.0, 1.0),
        [0.0, 0.093_464_65, 0.315_356_82, 0.621_861_9, 1.0],
      ),
      (
        EasingFunction::CubicBezier(0.0, 0.0, 0.58, 1.0),
        [0.0, 0.378_138_12, 0.684_643_2, 0.906_535_3, 1.0],
      ),
      (
        EasingFunction::CubicBezier(0.42, 0.0, 0.58, 1.0),
        [0.0, 0.129_161_94, 0.5, 0.870_838_05, 1.0],
      ),
      (
        EasingFunction::CubicBezier(0.32, 0.0, 0.67, 0.0),
        [0.0, 0.016_616_026, 0.127_798_26, 0.422_658_2, 1.0],
      ),
      (
        EasingFunction::CubicBezier(0.33, 1.0, 0.68, 1.0),
        [0.0, 0.577_341_8, 0.872_201_74, 0.983_384, 1.0],
      ),
      (
        EasingFunction::CubicBezier(0.65, 0.0, 0.35, 1.0),
        [0.0, 0.070_796_706, 0.5, 0.929_203_3, 1.0],
      ),
      (
        EasingFunction::EaseInQuart,
        [0.0, 0.003_906_25, 0.0625, 0.316_406_25, 1.0],
      ),
      (
        EasingFunction::EaseOutQuart,
        [0.0, 0.683_593_75, 0.9375, 0.996_093_75, 1.0],
      ),
      (
        EasingFunction::EaseInOutQuart,
        [0.0, 0.03125, 0.5, 0.96875, 1.0],
      ),
      (
        EasingFunction::EaseInQuint,
        [0.0, 0.000_976_562_5, 0.03125, 0.237_304_69, 1.0],
      ),
      (
        EasingFunction::EaseOutQuint,
        [0.0, 0.762_695_3, 0.96875, 0.999_023_44, 1.0],
      ),
      (
        EasingFunction::EaseInOutQuint,
        [0.0, 0.015625, 0.5, 0.984375, 1.0],
      ),
      (
        EasingFunction::EaseInExpo,
        [0.0, 0.005_524_272, 0.03125, 0.176_776_69, 1.0],
      ),
      (
        EasingFunction::EaseOutExpo,
        [0.0, 0.823_223_3, 0.96875, 0.994_475_7, 1.0],
      ),
      (
        EasingFunction::EaseInOutExpo,
        [0.0, 0.015625, 0.5, 0.984375, 1.0],
      ),
      (
        EasingFunction::EaseInBack,
        [0.0, -0.064_136_565, -0.087_697_5, 0.182_590_3, 1.0],
      ),
      (
        EasingFunction::EaseOutBack,
        [0.0, 0.817_409_7, 1.087_697_5, 1.064_136_6, 1.0],
      ),
      (
        EasingFunction::EaseInOutBack,
        [0.0, -0.099_681_85, 0.5, 1.099_681_9, 1.0],
      ),
      (
        EasingFunction::EaseOutSpring,
        [0.0, 0.875, 1.015625, 0.998_046_9, 1.0],
      ),
    ];

    for (easing, expected) in cases {
      for (progress, expected_value) in
        PROGRESS_POINTS.into_iter().zip(expected)
      {
        assert_close(apply_easing(progress, &easing), expected_value);
      }
    }
  }

  #[test]
  fn custom_bezier_inversion_handles_flat_derivatives() {
    let degenerate = EasingFunction::CubicBezier(0.0, 0.0, 0.0, 1.0);
    assert_close(apply_easing(0.000_001, &degenerate), 0.000_298);
    assert_close(apply_easing(0.5, &degenerate), 0.889_881_55);

    let linear = EasingFunction::CubicBezier(0.0, 0.0, 1.0, 1.0);
    for progress in [0.0, 0.000_001, 0.25, 0.5, 0.999_999, 1.0] {
      assert_eq!(apply_easing(progress, &linear), progress);
    }
  }

  #[test]
  fn ordinary_named_curves_are_bounded_monotonic_and_mirrored() {
    let monotonic = [
      EasingFunction::CubicBezier(0.0, 0.0, 1.0, 1.0),
      EasingFunction::CubicBezier(0.42, 0.0, 1.0, 1.0),
      EasingFunction::CubicBezier(0.0, 0.0, 0.58, 1.0),
      EasingFunction::CubicBezier(0.42, 0.0, 0.58, 1.0),
      EasingFunction::CubicBezier(0.32, 0.0, 0.67, 0.0),
      EasingFunction::CubicBezier(0.33, 1.0, 0.68, 1.0),
      EasingFunction::CubicBezier(0.65, 0.0, 0.35, 1.0),
      EasingFunction::EaseInQuart,
      EasingFunction::EaseOutQuart,
      EasingFunction::EaseInOutQuart,
      EasingFunction::EaseInQuint,
      EasingFunction::EaseOutQuint,
      EasingFunction::EaseInOutQuint,
      EasingFunction::EaseInExpo,
      EasingFunction::EaseOutExpo,
      EasingFunction::EaseInOutExpo,
    ];
    for easing in monotonic {
      let mut previous = apply_easing(0.0, &easing);
      for step in 1_u16..=1_000 {
        let value = apply_easing(f32::from(step) / 1_000.0, &easing);
        assert!((0.0..=1.0).contains(&value), "{easing:?}: {value}");
        assert!(value + 1e-6 >= previous, "{easing:?} is not monotonic");
        previous = value;
      }
    }

    for (ease_in, ease_out) in [
      (EasingFunction::EaseInQuart, EasingFunction::EaseOutQuart),
      (EasingFunction::EaseInQuint, EasingFunction::EaseOutQuint),
      (EasingFunction::EaseInExpo, EasingFunction::EaseOutExpo),
      (EasingFunction::EaseInBack, EasingFunction::EaseOutBack),
    ] {
      for step in 0_u16..=100 {
        let t = f32::from(step) / 100.0;
        assert_close(
          apply_easing(t, &ease_in),
          1.0 - apply_easing(1.0 - t, &ease_out),
        );
      }
    }
  }

  #[test]
  fn expo_endpoint_neighborhood_and_back_extrema_are_canonical() {
    assert_close(
      apply_easing(0.001, &EasingFunction::EaseInExpo),
      0.000_983_355,
    );
    assert_close(
      apply_easing(0.999, &EasingFunction::EaseOutExpo),
      0.999_016_64,
    );
    assert_close(
      apply_easing(0.001, &EasingFunction::EaseInOutExpo),
      0.000_495_097_4,
    );
    assert_close(
      apply_easing(0.999, &EasingFunction::EaseInOutExpo),
      0.999_504_9,
    );

    let in_extreme = 0.419_897_5;
    let out_extreme = 1.0 - in_extreme;
    assert_close(
      apply_easing(in_extreme, &EasingFunction::EaseInBack),
      -0.100_004_07,
    );
    assert_close(
      apply_easing(out_extreme, &EasingFunction::EaseOutBack),
      1.100_004_1,
    );
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
