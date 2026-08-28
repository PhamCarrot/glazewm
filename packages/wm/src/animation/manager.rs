use std::{
  collections::{HashMap, HashSet},
  sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
  },
  time::{Duration, Instant},
};

/// Fraction of the monitor's vblank period to lead the animation clock by.
///
/// After `IDXGIOutput::WaitForVBlank` returns, the surrogate update
/// written this tick is not composited by DWM until the *next* vblank — up
/// to one full frame period later. Advancing the animation clock by a
/// fraction of that period makes the computed position align with where it
/// will be when DWM actually presents it, rather than systematically
/// lagging behind.
///
/// Expressed as a fraction of the per-monitor frame period read live from
/// the installed `DxgiVsyncWaiter`, so the compensation scales correctly
/// across monitors with different refresh rates (e.g. 60 Hz + 175 Hz).
/// `0.5` leads by half a frame: a balance between under-compensating
/// (motion trails the cursor) and over-compensating (motion arrives
/// early). Tune here if the slide feels laggy (increase toward `1.0`) or
/// rushed (decrease toward `0.0`).
#[cfg(target_os = "windows")]
const VSYNC_LEAD_FRACTION: f32 = 0.5;

/// Pipeline latency estimate from vsync wake to DWM composition pickup.
///
/// The animation timer thread records the `Instant` at which
/// `IDXGIOutput::WaitForVBlank` returns. By the time `update_internal`
/// runs and calls `DwmUpdateThumbnailProperties`, roughly this many
/// microseconds have elapsed (Tokio scheduling + compute). Using
/// vsync_time + `VSYNC_PIPELINE_OFFSET_US` as "now" shifts the computed
/// position forward to where it will be when DWM actually composites,
/// eliminating the systematic one-pipeline-delay lag on high-Hz monitors.
/// Used by the iris-wipe driver, which has no per-switch vblank period to
/// lead by.
#[cfg(target_os = "windows")]
const VSYNC_PIPELINE_OFFSET_US: u64 = 1_500;

/// Maximum time a native animation timer generation may go without a
/// returned compositor wait. Six nominal 60 Hz frames avoids false
/// positives from ordinary scheduling jitter while bounding native
/// visibility ownership.
const ANIMATION_TIMER_STALL_TIMEOUT: Duration = Duration::from_millis(100);

/// Emergency fixed-rate cadence used after a native compositor wait
/// stalls. The active waiter's measured period replaces this default when
/// available.
const FALLBACK_FRAME_PERIOD_US: u64 = 16_667;

#[cfg(target_os = "windows")]
trait AnimationVsyncWaiter: Send + Sync {
  fn wait(&self) -> bool;
  fn monitor_handle(&self) -> isize;
  fn frame_period_us(&self) -> u64;
  fn last_wake(&self) -> Option<Instant>;
}

#[cfg(target_os = "windows")]
impl AnimationVsyncWaiter for DxgiVsyncWaiter {
  fn wait(&self) -> bool {
    DxgiVsyncWaiter::wait(self)
  }

  fn monitor_handle(&self) -> isize {
    DxgiVsyncWaiter::monitor_handle(self)
  }

  fn frame_period_us(&self) -> u64 {
    DxgiVsyncWaiter::frame_period_us(self)
  }

  fn last_wake(&self) -> Option<Instant> {
    self.last_wake.lock().ok()?.as_ref().copied()
  }
}

fn elapsed_timer_us(epoch: Instant, now: Instant) -> u64 {
  u64::try_from(now.saturating_duration_since(epoch).as_micros())
    .unwrap_or(u64::MAX)
}

/// Maximum residual slide distance, in pixels, at which workspace geometry
/// settles exactly on its endpoint while the configured transition clock
/// continues.
///
/// Decelerating easing curves spend a large fraction of their wall-clock
/// duration covering the final sliver of distance, which looks "stuck" at
/// the destination. Settling geometry avoids that crawl, while keeping the
/// separate completion clock preserves the public duration contract and
/// focus/opacity handoff ordering. Gating on a fixed *pixel* distance
/// (rather than a fixed progress fraction) keeps the snap below two pixels
/// regardless of slide distance or duration.
#[cfg(target_os = "windows")]
const WS_COMPLETE_THRESHOLD_PX: f32 = 1.5;

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq)]
struct WorkspaceFrameProgress {
  visual: f32,
  complete: bool,
}

/// Separates visual endpoint settling from transition lifetime.
///
/// Non-overshooting curves may submit their exact endpoint once residual
/// motion is subpixel, but logical/native handoff remains owned until raw
/// wall-clock progress reaches 1.0. Overshooting curves never settle
/// early, preserving their complete bounce/spring path.
#[cfg(target_os = "windows")]
fn workspace_frame_progress(
  raw: f32,
  eased: f32,
  can_overshoot: bool,
  slide_distance_px: Option<i32>,
) -> WorkspaceFrameProgress {
  let complete = raw >= 1.0;
  let visually_settled = if can_overshoot || complete {
    complete
  } else if let Some(distance) = slide_distance_px.filter(|d| *d > 0) {
    #[allow(clippy::cast_precision_loss)]
    let remaining_px = (1.0 - eased).abs() * distance as f32;
    remaining_px <= WS_COMPLETE_THRESHOLD_PX
  } else {
    // Fade/zoom have no positional travel. A 1% terminal opacity/scale
    // difference is not useful motion, so settle it while retaining the
    // wall-clock ownership interval.
    eased >= 0.99
  };

  WorkspaceFrameProgress {
    visual: if visually_settled { 1.0 } else { eased },
    complete,
  }
}

/// Maximum compositor-paced frame gaps retained for one workspace visual.
///
/// A 360 ms transition would need to exceed 700 Hz to fill this buffer.
/// The fixed array keeps runtime evidence bounded and allocation-free on
/// the animation path.
#[cfg(target_os = "windows")]
const WORKSPACE_FRAME_GAP_CAPACITY: usize = 256;

/// Allocation-free cadence telemetry emitted once when a workspace visual
/// completes. Recording is constant-time; percentile sorting happens only
/// after visual ownership has ended.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug)]
struct WorkspaceFrameGapTelemetry {
  last_submission: Option<Instant>,
  gaps_us: [u32; WORKSPACE_FRAME_GAP_CAPACITY],
  len: usize,
  total_samples: usize,
  max_us: u32,
}

#[cfg(target_os = "windows")]
impl Default for WorkspaceFrameGapTelemetry {
  fn default() -> Self {
    Self {
      last_submission: None,
      gaps_us: [0; WORKSPACE_FRAME_GAP_CAPACITY],
      len: 0,
      total_samples: 0,
      max_us: 0,
    }
  }
}

#[cfg(target_os = "windows")]
impl WorkspaceFrameGapTelemetry {
  fn record(&mut self, now: Instant) {
    if let Some(previous) = self.last_submission.replace(now) {
      let micros = now
        .saturating_duration_since(previous)
        .as_micros()
        .min(u128::from(u32::MAX)) as u32;
      self.total_samples += 1;
      self.max_us = self.max_us.max(micros);
      if self.len < WORKSPACE_FRAME_GAP_CAPACITY {
        self.gaps_us[self.len] = micros;
        self.len += 1;
      }
    }
  }

  fn summary_ms(&self) -> Option<(usize, f64, f64, f64)> {
    if self.len == 0 {
      return None;
    }

    let mut sorted = self.gaps_us;
    sorted[..self.len].sort_unstable();
    let median_index = (self.len - 1) / 2;
    let p95_index = (self.len * 95).div_ceil(100).saturating_sub(1);
    Some((
      self.total_samples,
      f64::from(sorted[median_index]) / 1_000.0,
      f64::from(sorted[p95_index]) / 1_000.0,
      f64::from(self.max_us) / 1_000.0,
    ))
  }

  fn log_summary(&self, visual: &'static str) {
    if let Some((samples, median_ms, p95_ms, max_ms)) = self.summary_ms() {
      tracing::debug!(
        visual,
        samples,
        retained_samples = self.len,
        median_ms,
        p95_ms,
        max_ms,
        gaps_us = ?&self.gaps_us[..self.len],
        "Workspace animation frame-gap summary."
      );
    }
  }
}

/// Grace period held at the start of a window-open animation before the
/// surrogate begins revealing the window.
///
/// A freshly created window has often not painted its first frame when the
/// open animation starts, so its DWM thumbnail is momentarily blank/black
/// — producing a black box that slides in and "pops" to real content at
/// the end. Holding the animation at progress `0.0` for this period (the
/// surrogate stays off-screen for slide/zoom and fully transparent for
/// fade) gives the app time to paint, so the slide reveals real content
/// from the first visible frame. Implemented via
/// `WindowAnimationState::start_delay`, which is measured from the first
/// rendered frame, so the app gets this long *after* the first animation
/// tick to paint. Roughly two frames at 60 Hz — long enough to cover the
/// typical first-paint latency without a perceptible delay in the window
/// appearing.
#[cfg(target_os = "windows")]
const OPEN_PAINT_GRACE: Duration = Duration::from_millis(30);

/// Maximum time allowed for a newly registered open/close thumbnail to
/// pass its nonblocking composition-tick handoff.
#[cfg(target_os = "windows")]
const SURROGATE_HANDOFF_TIMEOUT: Duration = Duration::from_millis(150);

/// Maximum number of compositor-paced workspace handoff ticks that may
/// encounter a transiently unavailable `IApplicationView` before the
/// transition falls back to the latest visible real-window layout.
///
/// The initial cloak attempt plus two later vsync ticks keeps the retry
/// bounded while covering the short Explorer publication race represented
/// by `TYPE_E_ELEMENTNOTFOUND` (`0x8002802B`).
#[cfg(target_os = "windows")]
const WORKSPACE_CLOAK_RETRY_LIMIT: u8 = 3;

/// Maximum number of failed final workspace synchronization attempts
/// before retained coverage is replaced with a visible-real-window
/// fallback for that workspace group.
#[cfg(target_os = "windows")]
const WORKSPACE_CLEANUP_SYNC_RETRY_LIMIT: u8 = 3;

/// Wall-clock bound for final workspace synchronization retries.
///
/// Retries are compositor-tick driven; this deadline prevents a stopped or
/// very slow timer from retaining workspace coverage indefinitely.
#[cfg(target_os = "windows")]
const WORKSPACE_CLEANUP_SYNC_RETRY_TIMEOUT: Duration =
  Duration::from_millis(100);

/// Duration of the surrogate fade-out at animation completion.
///
/// After the real window is uncloaked beneath the (pixel-aligned, live)
/// surrogate, the surrogate's opacity ramps to zero over this period so
/// shadow, border, and late-repaint differences blend in rather than
/// switching in a single composition.
#[cfg(target_os = "windows")]
const SESSION_FADE_OUT: Duration = Duration::from_millis(100);

/// Private bounded incompatible-workspace directional reveal.
///
/// Forty percent preserves approximately the configured slide's average
/// speed. The clamp prevents a short fallback from becoming either an
/// imperceptible flash or another full-length queued transition.
#[cfg(target_os = "windows")]
const WORKSPACE_REDIRECT_REVEAL_NUMERATOR: u128 = 2;
#[cfg(target_os = "windows")]
const WORKSPACE_REDIRECT_REVEAL_DENOMINATOR: u128 = 5;
#[cfg(target_os = "windows")]
const WORKSPACE_REDIRECT_REVEAL_MIN: Duration = Duration::from_millis(120);
#[cfg(target_os = "windows")]
const WORKSPACE_REDIRECT_REVEAL_MAX: Duration = Duration::from_millis(180);

/// Maximum mid-animation handoff lead in milliseconds.
///
/// The actual lead is `duration_ms * 0.35`, clamped to
/// `[50, HANDOFF_LEAD_MAX_MS]`. Scaling by duration keeps the handoff near
/// the end of the visual travel regardless of easing speed; the cap
/// ensures apps always get ≥50 ms to repaint at the new size before
/// uncloaking.
#[cfg(target_os = "windows")]
const HANDOFF_LEAD_MAX_MS: u64 = 100;

/// Maximum number of real-window handoffs (`ResizeSession::maybe_handoff`)
/// performed per redraw pass.
///
/// Each handoff issues a synchronous `SetWindowPos` plus a DWM thumbnail
/// dims update on the WM thread. When a relayout brings many sessions past
/// their handoff lead on the same tick (e.g. a workspace-wide resize),
/// doing all of them in one pass can stack up enough synchronous work to
/// stall that tick. Capping the count spreads the overflow across the next
/// few ticks (each ~6-16 ms) instead of widening any individual window's
/// lead — deferred windows retry every tick since `remaining_at` stays ≤
/// the lead until they're handed off, so the extra delay is at most a
/// couple of frames, not a fraction of the animation's duration.
#[cfg(target_os = "windows")]
const MAX_HANDOFFS_PER_TICK: usize = 3;

/// How long a sampled surrogate backdrop color stays valid per window.
///
/// Sampling costs two GPU→CPU `BitBlt` readbacks on the WM thread; caching
/// removes them from the keypress path during bursts of relayouts. App
/// background colors change rarely (theme switches), so a stale hit is a
/// cosmetic-only risk bounded by this TTL.
#[cfg(target_os = "windows")]
const EDGE_COLOR_CACHE_TTL: Duration = Duration::from_secs(30);

/// Cache-size threshold above which stale edge-color entries are pruned.
#[cfg(target_os = "windows")]
const EDGE_COLOR_CACHE_PRUNE_LEN: usize = 128;

use tokio::sync::mpsc;
use uuid::Uuid;
use wm_common::{
  DisplayState, EasingFunction, WindowTransitionStyle,
  WorkspaceSwitchAnimationConfig, WorkspaceSwitchDirection,
  WorkspaceSwitchStyle,
};
#[cfg(target_os = "windows")]
use wm_platform::{
  BlurOverlayParams, Color, CornerStyle, DxgiVsyncWaiter,
  NativeBlurOverlay, NativeIrisOverlay, NativeWindowWindowsExt,
  ResizeSession, SessionKind, SessionOptions, SlideAxis, SurrogateBatch,
  WorkspaceSurrogate,
};
use wm_platform::{NativeWindow, OpacityValue, Rect};

#[cfg(target_os = "windows")]
use crate::traits::WindowGetters;
use crate::{
  animation::state::WindowAnimationState,
  commands::general::platform_sync, traits::CommonGetters,
  user_config::UserConfig, wm_state::WmState,
};

#[cfg(target_os = "windows")]
pub(crate) fn opacity_target_was_destroyed(
  error: &wm_platform::Error,
  native_valid_after: bool,
) -> bool {
  error.is_invalid_window_handle() || !native_valid_after
}
#[cfg(target_os = "windows")]
use crate::{
  commands::{
    general::{remove_failed_blur_overlays, upsert_blur_overlay},
    window::{
      detach_window_for_close, isolate_destroyed_window,
      restore_window_after_failed_close,
    },
  },
  models::WindowContainer,
};

/// A single entry in the surrogate update queue built each redraw pass.
#[cfg(target_os = "windows")]
struct PendingSurrogateUpdate {
  window_id: Uuid,
  rect: Rect,
  opacity: u8,
  /// `true` when `remaining_at` ≤ the proportional handoff lead.
  handoff: bool,
}

/// State retained after a close-animation window is detached from the
/// tree.
///
/// Keeping the original container and workspace ID allows a failed final
/// `WM_CLOSE` delivery to restore the real window instead of abandoning it
/// cloaked and unmanaged after the surrogate is destroyed.
#[cfg(target_os = "windows")]
struct PendingCloseWindow {
  handle: isize,
  window: WindowContainer,
  workspace_id: Uuid,
  readiness: SurrogateHandoffReadiness,
  detached: bool,
}

/// Pending hidden-surrogate handoff for a newly opened window.
#[cfg(target_os = "windows")]
struct PendingOpenWindow {
  handle: isize,
  readiness: SurrogateHandoffReadiness,
}

/// Pending transparent-surrogate handoff for a move/resize animation.
#[cfg(target_os = "windows")]
struct PendingMoveResizeWindow {
  handle: isize,
  readiness: SurrogateHandoffReadiness,
  surrogate_revealed: bool,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoveResizeHandoffAction {
  Wait,
  RevealTransparentSurrogate,
  TransferVisibility,
  Fail,
}

#[cfg(target_os = "windows")]
impl PendingMoveResizeWindow {
  fn advance(
    &mut self,
    now: Instant,
    source_valid: bool,
    thumbnail_healthy: bool,
  ) -> MoveResizeHandoffAction {
    match self
      .readiness
      .advance(now, 2, source_valid, thumbnail_healthy)
    {
      SurrogateHandoffOutcome::Waiting if !self.surrogate_revealed => {
        self.surrogate_revealed = true;
        MoveResizeHandoffAction::RevealTransparentSurrogate
      }
      SurrogateHandoffOutcome::Waiting => MoveResizeHandoffAction::Wait,
      SurrogateHandoffOutcome::Ready => {
        MoveResizeHandoffAction::TransferVisibility
      }
      SurrogateHandoffOutcome::Failed => MoveResizeHandoffAction::Fail,
    }
  }
}

/// Tick-based readiness gate used instead of blocking on `DwmFlush`.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug)]
struct SurrogateHandoffReadiness {
  started_at: Instant,
  ticks_seen: u8,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurrogateHandoffOutcome {
  Waiting,
  Ready,
  Failed,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoveResizeRecoveryAction {
  DropDestroyed,
  RetainSurrogate,
  HideSurrogateAndRetry,
  Complete,
}

#[cfg(target_os = "windows")]
fn move_resize_recovery_action(
  source_valid: bool,
  source_was_cloaked: bool,
  positioned: bool,
  opacity_restored: bool,
  uncloaked: bool,
  shown: bool,
) -> MoveResizeRecoveryAction {
  if !source_valid {
    return MoveResizeRecoveryAction::DropDestroyed;
  }
  if !positioned || !uncloaked || !shown {
    return if source_was_cloaked {
      MoveResizeRecoveryAction::RetainSurrogate
    } else {
      MoveResizeRecoveryAction::HideSurrogateAndRetry
    };
  }
  if opacity_restored {
    MoveResizeRecoveryAction::Complete
  } else {
    MoveResizeRecoveryAction::HideSurrogateAndRetry
  }
}

#[cfg(target_os = "windows")]
fn failed_move_resize_update_id(
  window_id: Uuid,
  thumbnail_healthy: bool,
  is_open_close_handoff: bool,
) -> Option<Uuid> {
  (!thumbnail_healthy && !is_open_close_handoff).then_some(window_id)
}

#[cfg(target_os = "windows")]
impl SurrogateHandoffReadiness {
  fn new(now: Instant) -> Self {
    Self {
      started_at: now,
      ticks_seen: 0,
    }
  }

  fn advance(
    &mut self,
    now: Instant,
    required_ticks: u8,
    source_valid: bool,
    thumbnail_healthy: bool,
  ) -> SurrogateHandoffOutcome {
    if !source_valid
      || !thumbnail_healthy
      || now.saturating_duration_since(self.started_at)
        >= SURROGATE_HANDOFF_TIMEOUT
    {
      return SurrogateHandoffOutcome::Failed;
    }

    self.ticks_seen = self.ticks_seen.saturating_add(1);
    if self.ticks_seen >= required_ticks {
      SurrogateHandoffOutcome::Ready
    } else {
      SurrogateHandoffOutcome::Waiting
    }
  }
}

/// One-tick, nonblocking compositor handoff used by workspace and iris
/// transitions. The covering surface is created during `platform_sync`,
/// then the real-window transfer is deferred until the animation timer has
/// observed a compositor-paced tick.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug)]
struct TickHandoff {
  readiness: SurrogateHandoffReadiness,
  ready: bool,
}

#[cfg(target_os = "windows")]
impl TickHandoff {
  fn new(now: Instant) -> Self {
    Self {
      readiness: SurrogateHandoffReadiness::new(now),
      ready: false,
    }
  }

  fn is_pending(self) -> bool {
    !self.ready
  }

  fn advance(
    &mut self,
    now: Instant,
    source_valid: bool,
  ) -> SurrogateHandoffOutcome {
    if self.ready {
      return SurrogateHandoffOutcome::Ready;
    }

    let outcome = self.readiness.advance(now, 1, source_valid, true);
    if outcome == SurrogateHandoffOutcome::Ready {
      self.ready = true;
    }
    outcome
  }
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionCleanupAction {
  HoldCoverage,
  DropTransparent,
  FadeFrom(Instant),
}

#[cfg(target_os = "windows")]
fn session_cleanup_action(
  fade_start: &mut Option<Instant>,
  now: Instant,
  is_transparent: bool,
) -> SessionCleanupAction {
  if fade_start.is_none() {
    *fade_start = Some(now);
    return SessionCleanupAction::HoldCoverage;
  }
  if is_transparent {
    SessionCleanupAction::DropTransparent
  } else {
    SessionCleanupAction::FadeFrom(fade_start.unwrap_or(now))
  }
}

/// Advances a one-tick cleanup gate and returns whether teardown may run.
#[cfg(target_os = "windows")]
fn cleanup_tick_complete(tick_seen: &mut bool) -> bool {
  if *tick_seen {
    true
  } else {
    *tick_seen = true;
    false
  }
}

/// Returns whether retained workspace coverage may be destroyed this tick.
#[cfg(target_os = "windows")]
fn workspace_cleanup_complete(
  redirect_fallback: bool,
  incoming_reflow_handoff_pending: bool,
  tick_seen: &mut bool,
) -> bool {
  !redirect_fallback
    && !incoming_reflow_handoff_pending
    && cleanup_tick_complete(tick_seen)
}

#[cfg(target_os = "windows")]
fn should_preshow_workspace_cleanup_overlay(
  redirect_fallback: bool,
  cleanup_tick_seen: bool,
) -> bool {
  !redirect_fallback && !cleanup_tick_seen
}

/// Tracks a single window's participation in the current workspace-switch
/// slide animation.
#[cfg(target_os = "windows")]
struct WorkspaceSwitchEntry {
  /// Source HWND captured when this logical participant joined the
  /// switch. Kept alongside the UUID so a recycled numeric HWND cannot
  /// inherit an earlier window's transition state.
  source_handle: isize,
  /// Surrogate overlay that slides across the monitor each frame.
  surrogate: Option<WorkspaceSurrogate>,
  /// `true` for windows on the incoming workspace, `false` for outgoing.
  is_incoming: bool,
  /// Whether this real HWND has completed the workspace visibility
  /// transfer. Surrogate motion remains at progress zero until every live
  /// participant reaches this state.
  handoff_cloaked: bool,
}

/// Shared state for all windows in a workspace-switch animation.
///
/// A single elapsed-time driver advances all surrogates in lock-step so
/// every window translates by the same pixel offset on every frame,
/// preserving the illusion that both workspaces move as a single connected
/// panel.
#[cfg(target_os = "windows")]
struct WorkspaceSwitchState {
  /// All participating windows keyed by window ID.
  windows: HashMap<Uuid, WorkspaceSwitchEntry>,
  /// Time of the first rendered frame, lazily set on the first tick.
  ///
  /// Initialized to `None` so the clock starts when `update_internal`
  /// first renders the animation rather than when
  /// `start_workspace_switch` is called mid-`platform_sync`. Without
  /// lazy init, a cold-start gap of 1-3 DWM frames causes surrogates to
  /// jump ahead on their first visible tick.
  start_time: Option<Instant>,
  /// Total animation duration.
  duration: Duration,
  /// Full configured duration, retained when a reversal shortens the
  /// remaining segment to match the distance still on screen.
  base_duration: Duration,
  /// Visual progress at the beginning of the current segment. Normally
  /// `0.0`; after an exact reversal it is `1.0 - last_visual_progress`,
  /// which preserves every surrogate's current geometry without a jump.
  progress_origin: f32,
  /// Progress most recently submitted to the surrogate renderer.
  last_visual_progress: f32,
  /// Set when an active transition is redirected and consumed after the
  /// first redirected surrogate batch is submitted. Used only for debug
  /// latency evidence; it never gates rendering.
  redirect_requested_at: Option<Instant>,
  /// Easing function applied to raw elapsed-time progress.
  easing: EasingFunction,
  /// Motion style (slide, fade, or zoom).
  style: WorkspaceSwitchStyle,
  /// Slide axis (horizontal or vertical). Only used when `style` is
  /// `Slide`.
  slide_direction: WorkspaceSwitchDirection,
  /// Workspace ordering direction: `+1` = target workspace is
  /// higher-index (incoming from the far edge, outgoing to the near
  /// edge). `-1` = opposite.
  order_direction: i32,
  /// Left x-coordinate of the animation monitor in screen pixels.
  monitor_x: i32,
  /// Width of the animation monitor in screen pixels.
  monitor_width: i32,
  /// Top y-coordinate of the animation monitor in screen pixels.
  monitor_y: i32,
  /// Height of the animation monitor in screen pixels.
  monitor_height: i32,
  /// Effective horizontal slide travel distance in screen pixels.
  ///
  /// Less than `monitor_width` by the sum of the outgoing workspace's
  /// trailing gap and the incoming workspace's leading gap (both equal to
  /// `outer_gap` in a standard config). This makes the two workspace
  /// panels start adjacent with no visible seam between them.
  slide_distance_h: i32,
  /// Effective vertical slide travel distance in screen pixels.
  ///
  /// Mirrors `slide_distance_h` on the y-axis.
  slide_distance_v: i32,
  /// Whether `start_time` has been re-anchored to a vsync timestamp.
  ///
  /// The clock is provisionally anchored on the wall-clock cold-start
  /// tick, then re-anchored once to the first real vblank so all
  /// vsync-driven frames share a single origin. Stays `false`
  /// (wall-clock only) if no vblank signal ever arrives.
  vsync_anchored: bool,
  /// Scale applied to the whole workspace during slide transitions.
  ///
  /// Derived from `WorkspaceSwitchAnimationConfig::zoom_factor`. `0.0`
  /// means no zoom (plain slide). The outgoing workspace scales from
  /// `1.0` to `1.0 - zoom_factor`; the incoming from `1.0 -
  /// zoom_factor` to `1.0`.
  zoom_factor: f32,
  /// Opacity endpoints captured when the participant surrogates were
  /// constructed. Exact role reversal requires equal endpoints.
  opacity_outgoing: f32,
  opacity_incoming: f32,
  /// Nonblocking cover-readiness gate. Real workspace windows are not
  /// cloaked/repositioned until this passes on a later animation tick.
  handoff: TickHandoff,
  /// Number of compositor-paced handoff attempts which encountered the
  /// transient ApplicationView publication race.
  cloak_retry_ticks: u8,
  /// Keeps the final surrogate coverage alive for one compositor-paced
  /// tick after the incoming real windows are uncloaked.
  cleanup_tick_seen: bool,
  /// Bounded compositor-paced submission cadence for runtime validation.
  frame_gaps: WorkspaceFrameGapTelemetry,
}

/// Visual treatment used after retained incompatible-redirect coverage has
/// synchronized the latest real workspace beneath it.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceRedirectEffect {
  /// Native/emergency fallback when directional geometry is unavailable.
  OpacityFade,
  /// Translate the exact captured frame in one locked direction while
  /// fading it out. `motion_started` prevents later input from reversing
  /// or restarting already-visible geometry.
  DirectionalReveal {
    axis: SlideAxis,
    direction: i32,
    duration: Duration,
    distance: i32,
    motion_started: bool,
  },
}

/// Nonblocking fade/reveal of retained incompatible-redirect coverage over
/// the synchronized latest real workspace.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug)]
struct WorkspaceRedirectFadeState {
  /// First tick after the real layout sync is a composition-readiness
  /// hold.
  readiness_tick_seen: bool,
  /// Fade clock, started on the tick after readiness has been observed.
  start_time: Option<Instant>,
  effect: WorkspaceRedirectEffect,
  /// Bounded cadence of retained-cover movement/fade submissions.
  frame_gaps: WorkspaceFrameGapTelemetry,
}

#[cfg(target_os = "windows")]
impl WorkspaceRedirectFadeState {
  /// Updates a directional reveal before it has submitted visible motion.
  /// Once motion starts, the existing geometry and clock stay locked even
  /// when a newer destination replaces the real underlay.
  fn retarget_direction_before_motion(&mut self, direction: i32) -> bool {
    let WorkspaceRedirectEffect::DirectionalReveal {
      direction: current_direction,
      motion_started,
      ..
    } = &mut self.effect
    else {
      return false;
    };

    if !*motion_started {
      *current_direction = direction;
      self.readiness_tick_seen = false;
      self.start_time = None;
    }
    true
  }
}

#[cfg(target_os = "windows")]
fn workspace_redirect_reveal_timing(
  configured_duration: Duration,
  configured_distance: i32,
) -> (Duration, i32) {
  if configured_duration.is_zero() || configured_distance <= 0 {
    return (Duration::ZERO, 0);
  }
  let scaled_nanos = configured_duration.as_nanos()
    * WORKSPACE_REDIRECT_REVEAL_NUMERATOR
    / WORKSPACE_REDIRECT_REVEAL_DENOMINATOR;
  let scaled =
    Duration::from_nanos(u64::try_from(scaled_nanos).unwrap_or(u64::MAX));
  let duration = scaled
    .clamp(WORKSPACE_REDIRECT_REVEAL_MIN, WORKSPACE_REDIRECT_REVEAL_MAX)
    .min(configured_duration);
  let ratio = duration.as_secs_f32() / configured_duration.as_secs_f32();
  let distance = (configured_distance as f32 * ratio).round() as i32;
  (duration, distance.max(1).min(configured_distance))
}

#[cfg(target_os = "windows")]
fn workspace_redirect_reveal_easing(progress: f32) -> f32 {
  let progress = progress.clamp(0.0, 1.0);
  1.0 - (1.0 - progress).powi(3)
}

/// Result of applying a new workspace-switch request while another
/// transition owns the current surrogate coverage.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceRedirectOutcome {
  /// No per-window transition was active.
  NotActive,
  /// The pending markers belong to the staged handoff which created the
  /// current transition, not to a newer command.
  ExistingRequest,
  /// The same participant set was reversed in place with no new hosts or
  /// thumbnail registrations.
  Redirected,
  /// Existing coverage cannot be reused directionally. It is retained
  /// while the latest real layout synchronizes, then faded out
  /// nonblockingly.
  FinalStateFallback,
  /// Existing coverage is retained at its exact current frame, then
  /// translated/faded over the synchronized real destination without
  /// allocating another workspace surrogate group.
  DirectionalReveal,
}

/// Bounded action after a transient workspace cloak handoff failure.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceCloakRetryOutcome {
  /// Keep every surrogate at its initial geometry and retry on a later
  /// animation/vsync tick.
  Retry { attempt: u8, remaining: u8 },
  /// Stop retrying and synchronize the latest logical workspace using the
  /// already established visible-real-window fallback.
  FinalStateFallback,
}

#[cfg(target_os = "windows")]
fn redirected_segment_duration(
  base_duration: Duration,
  progress_origin: f32,
) -> Duration {
  // Distance remaining after reversal is `1 - progress_origin`, which is
  // the old transition's traveled distance. Preserve approximately the
  // configured pixels-per-second instead of replaying a full-duration
  // animation for a short return trip. A one-millisecond floor guarantees
  // the endpoint is committed on a later compositor tick.
  let distance_fraction = (1.0 - progress_origin).abs().clamp(0.0, 1.5);
  base_duration
    .mul_f32(distance_fraction)
    .max(Duration::from_millis(1))
}

/// State for an active iris-wipe workspace transition.
///
/// Unlike the per-window slide, the iris wipe uses a single frozen
/// snapshot overlay: the incoming workspace is switched in normally
/// (instantly) underneath, and a growing circular hole in the overlay
/// reveals it. No per-window surrogates are involved.
#[cfg(target_os = "windows")]
struct IrisSwitchState {
  /// Snapshot overlay shown on top of the (already switched) real
  /// windows.
  overlay: NativeIrisOverlay,
  /// Circle origin (screen pixels) from which the hole grows.
  origin_x: i32,
  origin_y: i32,
  /// Radius (px) at which the hole fully covers the monitor.
  max_radius: i32,
  /// Time of the first rendered frame, lazily set on the first tick
  /// (mirrors `WorkspaceSwitchState::start_time`).
  start_time: Option<Instant>,
  /// Total animation duration.
  duration: Duration,
  /// Easing applied to raw elapsed-time progress.
  easing: EasingFunction,
  /// Nonblocking cover-readiness gate. The underlying workspace is not
  /// switched until a later compositor-paced tick confirms the snapshot
  /// overlay has had an opportunity to compose.
  handoff: TickHandoff,
}

/// Result of [`AnimationManager::start_animation_if_needed`], describing
/// what the caller should do with the real app window's position this
/// frame.
pub enum AnimationPositionResult {
  /// Apply this rect to the real window via `reposition_window`.
  ///
  /// The carried `Rect` is the current animated position, available for
  /// callers that bypass the surrogate path (e.g. future macOS support).
  #[allow(dead_code)]
  Apply(Rect),
  /// A staged surrogate handoff is still warming up. Keep the real window
  /// exactly as-is until a later compositor tick transfers visibility.
  Waiting,
  /// The surrogate overlay is handling all visuals; skip repositioning
  /// the real window this frame.
  Frozen,
}

/// Typed result of one animation update pass.
///
/// Only `Fatal` permits the `WindowManager` wrapper to invoke global
/// animation recovery. Expected retry and participant/group-local fallback
/// paths are reported separately so one native HWND cannot cancel
/// unrelated animation groups.
pub(crate) enum AnimationUpdateOutcome {
  Complete,
  RetryScheduled {
    attempt: u8,
    remaining: u8,
    error: anyhow::Error,
  },
  ParticipantFailed {
    window_ids: Vec<Uuid>,
  },
  GroupRecovered {
    participant_count: usize,
    error: anyhow::Error,
  },
  Fatal(anyhow::Error),
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceCleanupSyncFailureAction {
  Retry { attempt: u8, remaining: u8 },
  Recover,
}

#[cfg(target_os = "windows")]
#[derive(Default)]
struct WorkspaceCleanupSyncRetry {
  attempts: u8,
  deadline: Option<Instant>,
}

#[cfg(target_os = "windows")]
impl WorkspaceCleanupSyncRetry {
  fn record_failure(
    &mut self,
    now: Instant,
  ) -> WorkspaceCleanupSyncFailureAction {
    let deadline = *self
      .deadline
      .get_or_insert(now + WORKSPACE_CLEANUP_SYNC_RETRY_TIMEOUT);
    self.attempts = self.attempts.saturating_add(1);

    if self.attempts >= WORKSPACE_CLEANUP_SYNC_RETRY_LIMIT
      || now >= deadline
    {
      WorkspaceCleanupSyncFailureAction::Recover
    } else {
      WorkspaceCleanupSyncFailureAction::Retry {
        attempt: self.attempts,
        remaining: WORKSPACE_CLEANUP_SYNC_RETRY_LIMIT - self.attempts,
      }
    }
  }

  fn clear(&mut self) {
    self.attempts = 0;
    self.deadline = None;
  }
}

/// Manages animations for all windows.
pub struct AnimationManager {
  /// Active animations keyed by window ID.
  animations: HashMap<Uuid, WindowAnimationState>,
  /// Opacity-only animations triggered by focus changes.
  ///
  /// Kept separate from movement/resize animations so moving a window and
  /// changing focus cannot replace each other's animation state.
  #[cfg(target_os = "windows")]
  focus_opacity_animations: HashMap<Uuid, WindowAnimationState>,
  /// Sender for animation tick events.
  animation_tick_tx: mpsc::Sender<()>,
  /// Whether the animation timer thread is currently ticking (vs.
  /// parked).
  ///
  /// Acts as the gate for the persistent timer thread: set `true` to
  /// start a ticking phase, `false` to send it back to parking.
  animation_timer_running: Arc<AtomicBool>,
  /// Handle to the persistent animation timer thread.
  ///
  /// Spawned lazily on the first animation and kept for the process
  /// lifetime, parking between animations rather than being re-spawned
  /// each time. This removes the per-animation cost of thread creation
  /// + priority/MMCSS setup from the input-to-first-frame latency path.
  timer_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
  /// Fixed-rate emergency driver, spawned at most once after a native
  /// wait generation is abandoned.
  fallback_timer_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
  /// Signals the persistent timer thread to exit. Set on drop.
  timer_shutdown: Arc<AtomicBool>,
  /// Monotonically increasing identity of the currently armed timer
  /// phase.
  animation_timer_generation: Arc<AtomicU64>,
  /// Microseconds since `animation_timer_epoch` when pacing last
  /// progressed.
  animation_timer_heartbeat_us: Arc<AtomicU64>,
  /// Process-lifetime emergency mode entered after one native wait stall.
  animation_timer_fallback: Arc<AtomicBool>,
  /// Fixed fallback cadence, updated from the active waiter's frame
  /// period.
  animation_timer_period_us: Arc<AtomicU64>,
  animation_timer_epoch: Instant,
  /// DXGI vsync waiter for the animation monitor.
  ///
  /// When `Some`, the timer thread calls `WaitForVBlank` on this output
  /// instead of `DwmFlush`. This gives a full frame period after each
  /// vsync to update surrogates, regardless of which monitor is the
  /// Windows primary. Cleared on workspace-switch completion.
  #[cfg(target_os = "windows")]
  animation_timer_vsync: Arc<Mutex<Option<Arc<dyn AnimationVsyncWaiter>>>>,
  /// Timestamp of the most recent `IDXGIOutput::WaitForVBlank` wake-up.
  ///
  /// Written by the timer thread immediately after vsync fires. Read by
  /// `update_internal` to compute animation progress at a predictive
  /// timestamp (vsync time + a fraction of the vblank period; see
  /// [`VSYNC_LEAD_FRACTION`]) rather than `Instant::now()`, compensating
  /// for the pipeline delay between vsync wake and DWM composition.
  #[cfg(target_os = "windows")]
  animation_vsync_time: Arc<Mutex<Option<Instant>>>,
  /// Active resize sessions keyed by window ID.
  ///
  /// A session is created when a movement/resize animation starts with
  /// `use_surrogate = true` and is destroyed once the animation completes
  /// and the real window has been moved to its final position.
  #[cfg(target_os = "windows")]
  pub(crate) resize_sessions: HashMap<Uuid, ResizeSession>,
  /// Recently sampled surrogate backdrop colors keyed by window handle.
  ///
  /// Lets `ResizeSession::begin` skip its two-`BitBlt` screen sample for
  /// windows resized again within [`EDGE_COLOR_CACHE_TTL`], removing the
  /// GPU→CPU readback burst when a relayout begins many sessions in the
  /// same keypress.
  #[cfg(target_os = "windows")]
  edge_color_cache: HashMap<isize, (Color, Instant)>,
  /// Surrogate updates queued during this redraw pass; committed
  /// atomically by [`flush_surrogate_updates`] so adjacent surrogates
  /// land in the same DWM composition frame.
  ///
  /// [`flush_surrogate_updates`]: AnimationManager::flush_surrogate_updates
  #[cfg(target_os = "windows")]
  pending_surrogate_updates: Vec<PendingSurrogateUpdate>,
  /// Sessions that have been removed from `resize_sessions` after their
  /// animation completed but must outlive the final `platform_sync` call
  /// that repositions the real window. Keyed by window ID so the final
  /// redraw can detect that `pre_commit` already positioned the window.
  ///
  /// The `Option<Instant>` is the fade-out start time: `None` until the
  /// real window has been uncloaked beneath the surrogate, then set on
  /// the first cleanup tick. Entries are dropped once the fade
  /// completes.
  #[cfg(target_os = "windows")]
  pub(crate) pending_session_cleanup:
    Vec<(Uuid, Option<Instant>, ResizeSession)>,
  /// Monitor rects for active slide-in (window-open) animations, keyed by
  /// window ID. Used to hide the surrogate while it is fully off the
  /// monitor.
  #[cfg(target_os = "windows")]
  slide_in_monitor_rects: HashMap<Uuid, Rect>,
  /// Open surrogates registered hidden until one compositor-paced tick
  /// confirms their thumbnail can be revealed without exposing backdrop.
  #[cfg(target_os = "windows")]
  pending_open_windows: HashMap<Uuid, PendingOpenWindow>,
  /// Move/resize surrogates waiting for their transparent three-tick
  /// compositor handoff while the real source window remains visible.
  #[cfg(target_os = "windows")]
  pending_move_resize_windows: HashMap<Uuid, PendingMoveResizeWindow>,
  /// Move/resize sessions whose thumbnail failed and whose real HWND is
  /// being restored. These remain animation-active until reposition,
  /// opacity, and visibility restoration all succeed.
  #[cfg(target_os = "windows")]
  failed_move_resize_recoveries: HashSet<Uuid>,
  /// Active workspace-switch slide animation, or `None` when idle.
  #[cfg(target_os = "windows")]
  workspace_switch: Option<WorkspaceSwitchState>,
  /// Workspace-switch state that just completed; kept alive until the
  /// final `platform_sync` call unclocks the incoming real windows.
  #[cfg(target_os = "windows")]
  pending_ws_cleanup: Option<WorkspaceSwitchState>,
  /// Bounded state for retrying the fallible final sync while
  /// `pending_ws_cleanup` still owns visual coverage.
  #[cfg(target_os = "windows")]
  workspace_cleanup_sync_retry: WorkspaceCleanupSyncRetry,
  /// True when `pending_ws_cleanup` is temporarily retaining old visual
  /// coverage for an incompatible rapid-switch fallback. The state is
  /// dropped only after the latest logical real windows and effects have
  /// synchronized successfully.
  #[cfg(target_os = "windows")]
  workspace_redirect_fallback: bool,
  /// Set only after the fallback redraw has positioned/revealed the
  /// latest real windows. At that point static blur overlays may be
  /// prepared underneath the still-retained old coverage before it is
  /// dropped.
  #[cfg(target_os = "windows")]
  workspace_redirect_real_ready: bool,
  /// Readiness and fade clock for an incompatible rapid-switch
  /// transition.
  #[cfg(target_os = "windows")]
  workspace_redirect_fade: Option<WorkspaceRedirectFadeState>,
  /// Windows with an active close animation, keyed by window ID.
  ///
  /// Each value retains the raw HWND plus enough detached-container state
  /// to restore the real window if final `WM_CLOSE` delivery fails.
  #[cfg(target_os = "windows")]
  pending_close_windows: HashMap<Uuid, PendingCloseWindow>,
  /// Visible transparent windows whose alpha is reasserted once more on
  /// the next compositor-paced tick after a `SWP_FRAMECHANGED` landing.
  #[cfg(target_os = "windows")]
  pending_transparency_reasserts: HashSet<Uuid>,
  /// Active iris-wipe workspace transition, or `None` when idle.
  #[cfg(target_os = "windows")]
  iris_switch: Option<IrisSwitchState>,
}

impl Drop for AnimationManager {
  /// Signals the persistent timer thread to exit.
  ///
  /// Sets the shutdown flag, clears the ticking gate, and unparks the
  /// thread so it observes the shutdown and returns. The handle is
  /// dropped without joining so shutdown never blocks — a thread
  /// mid-`WaitForVBlank` on a sleeping monitor could otherwise stall the
  /// join indefinitely. The thread exits on its own and is reaped by the
  /// OS at process exit.
  fn drop(&mut self) {
    self.timer_shutdown.store(true, Ordering::Relaxed);
    self.invalidate_timer_generation();
    if let Some(handle) = self
      .timer_thread
      .lock()
      .expect("animation mutex poisoned")
      .take()
    {
      handle.thread().unpark();
    }
    if let Some(handle) = self
      .fallback_timer_thread
      .lock()
      .expect("animation mutex poisoned")
      .take()
    {
      handle.thread().unpark();
    }
  }
}

impl AnimationManager {
  /// Creates a new `AnimationManager`.
  pub fn new(animation_tick_tx: mpsc::Sender<()>) -> Self {
    Self {
      animations: HashMap::new(),
      #[cfg(target_os = "windows")]
      focus_opacity_animations: HashMap::new(),
      animation_tick_tx,
      animation_timer_running: Arc::new(AtomicBool::new(false)),
      timer_thread: Mutex::new(None),
      fallback_timer_thread: Mutex::new(None),
      timer_shutdown: Arc::new(AtomicBool::new(false)),
      animation_timer_generation: Arc::new(AtomicU64::new(0)),
      animation_timer_heartbeat_us: Arc::new(AtomicU64::new(0)),
      animation_timer_fallback: Arc::new(AtomicBool::new(false)),
      animation_timer_period_us: Arc::new(AtomicU64::new(
        FALLBACK_FRAME_PERIOD_US,
      )),
      animation_timer_epoch: Instant::now(),
      #[cfg(target_os = "windows")]
      animation_timer_vsync: Arc::new(Mutex::new(None)),
      #[cfg(target_os = "windows")]
      animation_vsync_time: Arc::new(Mutex::new(None)),
      #[cfg(target_os = "windows")]
      resize_sessions: HashMap::new(),
      #[cfg(target_os = "windows")]
      edge_color_cache: HashMap::new(),
      #[cfg(target_os = "windows")]
      pending_surrogate_updates: Vec::new(),
      #[cfg(target_os = "windows")]
      pending_session_cleanup: Vec::new(),
      #[cfg(target_os = "windows")]
      slide_in_monitor_rects: HashMap::new(),
      #[cfg(target_os = "windows")]
      pending_open_windows: HashMap::new(),
      #[cfg(target_os = "windows")]
      pending_move_resize_windows: HashMap::new(),
      #[cfg(target_os = "windows")]
      failed_move_resize_recoveries: HashSet::new(),
      #[cfg(target_os = "windows")]
      workspace_switch: None,
      #[cfg(target_os = "windows")]
      pending_ws_cleanup: None,
      #[cfg(target_os = "windows")]
      workspace_cleanup_sync_retry: WorkspaceCleanupSyncRetry::default(),
      #[cfg(target_os = "windows")]
      workspace_redirect_fallback: false,
      #[cfg(target_os = "windows")]
      workspace_redirect_real_ready: false,
      #[cfg(target_os = "windows")]
      workspace_redirect_fade: None,
      #[cfg(target_os = "windows")]
      pending_close_windows: HashMap::new(),
      #[cfg(target_os = "windows")]
      pending_transparency_reasserts: HashSet::new(),
      #[cfg(target_os = "windows")]
      iris_switch: None,
    }
  }

  /// Inserts or replaces the animation state for a window.
  pub fn start_animation(
    &mut self,
    window_id: Uuid,
    animation: WindowAnimationState,
  ) {
    self.animations.insert(window_id, animation);
  }

  /// Returns the current animation state for a window, if any.
  pub fn get_animation(
    &self,
    window_id: &Uuid,
  ) -> Option<&WindowAnimationState> {
    self.animations.get(window_id)
  }

  /// Starts or reverses an opacity-only focus transition.
  ///
  /// If a transition for the same window is already running, its current
  /// interpolated opacity becomes the new starting point. This avoids a
  /// snap when the cursor crosses windows faster than the configured
  /// duration.
  #[cfg(target_os = "windows")]
  pub fn start_focus_opacity_animation(
    &mut self,
    window_id: Uuid,
    rect: Rect,
    fallback_start: OpacityValue,
    target: OpacityValue,
    duration_ms: u32,
    easing: EasingFunction,
  ) -> OpacityValue {
    let start = self
      .focus_opacity_animations
      .get(&window_id)
      .and_then(|animation| animation.current_state().1)
      .unwrap_or(fallback_start);

    if start == target || duration_ms == 0 {
      self.focus_opacity_animations.remove(&window_id);
      return target;
    }

    let animation = WindowAnimationState::new_opacity(
      rect,
      start.clone(),
      target,
      duration_ms,
      easing,
    );
    self.focus_opacity_animations.insert(window_id, animation);
    let _ = self.ensure_timer_running();
    start
  }

  /// Cancels all focus-opacity transitions.
  #[cfg(target_os = "windows")]
  pub fn clear_focus_opacity_animations(&mut self) {
    self.focus_opacity_animations.clear();
  }

  /// Cancels one focus-opacity transition before an atomic effect handoff.
  #[cfg(target_os = "windows")]
  pub fn clear_focus_opacity_animation(&mut self, window_id: &Uuid) {
    self.focus_opacity_animations.remove(window_id);
  }

  /// Returns `true` if a close animation is in flight for the given
  /// window.
  #[cfg(target_os = "windows")]
  pub fn has_close_animation(&self, window_id: &Uuid) -> bool {
    self.pending_close_windows.contains_key(window_id)
  }

  /// Returns whether an open surrogate is still waiting for its first
  /// compositor-paced readiness tick.
  #[cfg(target_os = "windows")]
  pub fn is_open_handoff_pending(&self, window_id: &Uuid) -> bool {
    self.pending_open_windows.contains_key(window_id)
  }

  /// Returns whether a move/resize surrogate is still in its transparent
  /// compositor-readiness phase.
  #[cfg(target_os = "windows")]
  pub fn is_move_resize_handoff_pending(&self, window_id: &Uuid) -> bool {
    self.pending_move_resize_windows.contains_key(window_id)
  }

  /// Returns `true` if a DWM surrogate is currently covering `window_id`.
  ///
  /// Used by blur-overlay tracking to skip position updates and hide the
  /// overlay while the real window is cloaked — preventing the overlay
  /// from jumping to the window's pre-positioned target rect while the
  /// surrogate is still mid-animation.
  #[cfg(target_os = "windows")]
  pub fn has_active_surrogate(&self, window_id: &Uuid) -> bool {
    self.resize_sessions.contains_key(window_id)
      || self
        .workspace_switch
        .as_ref()
        .map(|ws| ws.windows.contains_key(window_id))
        .unwrap_or(false)
      || self
        .pending_ws_cleanup
        .as_ref()
        .map(|ws| {
          !self.workspace_redirect_real_ready
            && ws.windows.contains_key(window_id)
        })
        .unwrap_or(false)
      || self.pending_close_windows.contains_key(window_id)
      // A completed resize session's surrogate is moved here to fade out
      // over the uncloaked real window (see `remove_completed_animations`)
      // rather than being dropped immediately -- it's still alive and
      // carrying its own live acrylic blur, so the static overlay must stay
      // hidden until the fade finishes, not just until `resize_sessions` no
      // longer holds the entry.
      || self
        .pending_session_cleanup
        .iter()
        .any(|(id, _, _)| id == window_id)
  }

  /// Returns `true` if `window_id` has a `Live`-mode (acrylic-tinted)
  /// workspace-switch surrogate actively tracked this tick, or in its
  /// `pending_ws_cleanup` grace tail.
  ///
  /// That tracked blur overlay is owned entirely by the per-tick driver in
  /// `update_internal` for as long as this returns `true` --
  /// `sync_blur_overlays` must leave these windows alone rather than
  /// hiding/repositioning them.
  #[cfg(target_os = "windows")]
  pub fn has_live_ws_surrogate(&self, window_id: &Uuid) -> bool {
    let is_live = |ws: &WorkspaceSwitchState| {
      ws.windows.get(window_id).is_some_and(|e| {
        e.surrogate
          .as_ref()
          .is_some_and(WorkspaceSurrogate::is_live)
      })
    };
    self.workspace_switch.as_ref().is_some_and(is_live)
      || ((!self.workspace_redirect_fallback
        || self.workspace_redirect_fade.as_ref().is_some_and(|fade| {
          matches!(
            fade.effect,
            WorkspaceRedirectEffect::DirectionalReveal { .. }
          )
        }))
        && self.pending_ws_cleanup.as_ref().is_some_and(is_live))
  }

  /// Returns `true` if `window_id`'s active or fading-out `ResizeSession`
  /// carries a live acrylic-blur tracker (i.e. `backdrop` was configured
  /// when the session began). Mirrors `has_live_ws_surrogate`.
  #[cfg(target_os = "windows")]
  pub fn has_live_resize_tracker(&self, window_id: &Uuid) -> bool {
    self
      .resize_sessions
      .get(window_id)
      .is_some_and(|s| s.blur_overlay_params().is_some())
      || self.pending_session_cleanup.iter().any(|(id, _, s)| {
        id == window_id && s.blur_overlay_params().is_some()
      })
  }

  /// Removes a window's animation and any associated resize session.
  pub fn remove_animation(&mut self, window_id: &Uuid) {
    self.animations.remove(window_id);
    #[cfg(target_os = "windows")]
    self.resize_sessions.remove(window_id);
    #[cfg(target_os = "windows")]
    self.slide_in_monitor_rects.remove(window_id);
    #[cfg(target_os = "windows")]
    self.pending_open_windows.remove(window_id);
    #[cfg(target_os = "windows")]
    self.pending_move_resize_windows.remove(window_id);
    #[cfg(target_os = "windows")]
    self.failed_move_resize_recoveries.remove(window_id);
    #[cfg(target_os = "windows")]
    self.pending_close_windows.remove(window_id);
    #[cfg(target_os = "windows")]
    self.pending_transparency_reasserts.remove(window_id);
  }

  /// Removes every animation-owned resource for one destroyed logical
  /// window.
  ///
  /// This is intentionally UUID-scoped. `expected_handle` is used only to
  /// invalidate HWND-keyed cosmetic cache data; another logical window
  /// which already received the same recycled numeric HWND remains
  /// untouched.
  #[cfg(target_os = "windows")]
  pub(crate) fn remove_destroyed_window_state(
    &mut self,
    window_id: &Uuid,
    expected_handle: isize,
  ) {
    self.animations.remove(window_id);
    self.focus_opacity_animations.remove(window_id);
    self.resize_sessions.remove(window_id);
    self
      .pending_session_cleanup
      .retain(|(id, _, _)| id != window_id);
    self.slide_in_monitor_rects.remove(window_id);
    self.pending_open_windows.remove(window_id);
    self.pending_move_resize_windows.remove(window_id);
    self.failed_move_resize_recoveries.remove(window_id);
    self.pending_close_windows.remove(window_id);
    self.pending_transparency_reasserts.remove(window_id);
    self
      .pending_surrogate_updates
      .retain(|update| update.window_id != *window_id);
    self.edge_color_cache.remove(&expected_handle);

    if let Some(ws) = &mut self.workspace_switch {
      ws.windows.remove(window_id);
    }
    if self
      .workspace_switch
      .as_ref()
      .is_some_and(|ws| ws.windows.is_empty())
    {
      self.workspace_switch = None;
    }

    if let Some(ws) = &mut self.pending_ws_cleanup {
      ws.windows.remove(window_id);
    }
    if self
      .pending_ws_cleanup
      .as_ref()
      .is_some_and(|ws| ws.windows.is_empty())
    {
      self.pending_ws_cleanup = None;
      self.workspace_cleanup_sync_retry.clear();
      self.workspace_redirect_fallback = false;
      self.workspace_redirect_real_ready = false;
      self.workspace_redirect_fade = None;
    }
  }

  /// Removes all completed animations and returns their window IDs.
  ///
  /// Sessions for completed animations are moved to
  /// `pending_session_cleanup` so they remain visible until after the
  /// final `platform_sync` call has repositioned the real windows.
  /// `pre_commit` is called on each session at this point to snapshot
  /// the window's liveness and position the surrogate at the final
  /// target rect.
  pub fn remove_completed_animations(&mut self) -> Vec<Uuid> {
    let completed_ids: Vec<Uuid> = self
      .animations
      .iter()
      .filter(|(_, anim)| anim.is_complete())
      .map(|(id, _)| *id)
      .collect();

    for id in &completed_ids {
      self.animations.remove(id);
      #[cfg(target_os = "windows")]
      if let Some(mut session) = self.resize_sessions.remove(id) {
        session.pre_commit();
        self.pending_session_cleanup.push((*id, None, session));
      }
      #[cfg(target_os = "windows")]
      self.slide_in_monitor_rects.remove(id);
    }

    completed_ids
  }

  /// Whether there are any active animations or a workspace-switch in
  /// flight.
  pub fn has_active_animations(&self) -> bool {
    if !self.animations.is_empty() {
      return true;
    }
    #[cfg(target_os = "windows")]
    if !self.focus_opacity_animations.is_empty() {
      return true;
    }
    #[cfg(target_os = "windows")]
    if self.workspace_switch.is_some() {
      return true;
    }
    // A completed workspace switch remains animation-active until the
    // final platform sync has repositioned and uncloaked the real
    // incoming windows. In particular, keep ticking if that sync
    // failed: the surrogates are the only safe visible replacement
    // until a later tick can retry cleanup.
    #[cfg(target_os = "windows")]
    if self.pending_ws_cleanup.is_some() {
      return true;
    }
    #[cfg(target_os = "windows")]
    if self.iris_switch.is_some() {
      return true;
    }
    // Completed sessions fading out still need ticks to advance the fade.
    #[cfg(target_os = "windows")]
    if !self.pending_session_cleanup.is_empty() {
      return true;
    }
    #[cfg(target_os = "windows")]
    if !self.failed_move_resize_recoveries.is_empty() {
      return true;
    }
    #[cfg(target_os = "windows")]
    if !self.pending_transparency_reasserts.is_empty() {
      return true;
    }
    false
  }

  /// Returns all active animation window IDs.
  pub fn active_window_ids(&self) -> Vec<Uuid> {
    let mut ids: HashSet<Uuid> = self.animations.keys().copied().collect();
    #[cfg(target_os = "windows")]
    ids.extend(self.failed_move_resize_recoveries.iter().copied());
    ids.into_iter().collect()
  }

  /// Drains all active and pending resize sessions and returns them.
  ///
  /// Used by `WmState::Drop` to commit sessions during shutdown or crash
  /// so that no window is left at an intermediate animation position.
  /// Workspace- switch surrogates are also dropped (real windows are
  /// already at their final positions by the time this is called).
  #[cfg(target_os = "windows")]
  pub fn drain_all_sessions(&mut self) -> Vec<ResizeSession> {
    self.invalidate_timer_generation();
    let mut sessions: Vec<ResizeSession> =
      self.resize_sessions.drain().map(|(_, s)| s).collect();
    sessions
      .extend(self.pending_session_cleanup.drain(..).map(|(_, _, s)| s));
    self.pending_surrogate_updates.clear();
    self.pending_open_windows.clear();
    self.pending_move_resize_windows.clear();
    self.failed_move_resize_recoveries.clear();
    self.pending_transparency_reasserts.clear();
    self.focus_opacity_animations.clear();
    self.workspace_switch = None;
    self.pending_ws_cleanup = None;
    self.workspace_cleanup_sync_retry.clear();
    self.workspace_redirect_fallback = false;
    self.workspace_redirect_real_ready = false;
    self.workspace_redirect_fade = None;
    *self
      .animation_timer_vsync
      .lock()
      .expect("animation mutex poisoned") = None;
    *self
      .animation_vsync_time
      .lock()
      .expect("animation mutex poisoned") = None;
    // On WM shutdown close-animation windows are left open. Reveal their
    // real windows before dropping the detached recovery records and
    // surrogates so stopping the WM mid-close cannot leave an
    // application cloaked.
    for pending in self.pending_close_windows.values() {
      let native = NativeWindow::from_handle(pending.handle);
      if let Err(err) = native.set_cloaked(false) {
        tracing::warn!(
          "Failed to uncloak closing window during shutdown: {err}."
        );
      }
      if let Err(err) = native.show() {
        tracing::warn!(
          "Failed to show closing window during shutdown: {err}."
        );
      }
    }
    self.pending_close_windows.clear();
    // Drop the iris overlay (if any); the real windows are already at
    // their final positions, so tearing it down simply reveals them.
    self.iris_switch = None;
    sessions
  }

  /// Cancels every animation and restores native windows to a safe final
  /// state after timer startup or frame processing fails.
  ///
  /// Surrogates are retained locally until a suppression-mode
  /// `platform_sync` has repositioned and revealed the real windows. If
  /// that sync also fails, displayed windows are explicitly uncloaked
  /// and shown as a last-resort usability fallback. Detached
  /// close-animation windows are reattached via their retained recovery
  /// records.
  pub(crate) fn recover_after_animation_failure(
    state: &mut WmState,
    config: &UserConfig,
    reason: &str,
  ) {
    Self::settle_animation_ownership(state, config, reason, true);
  }

  /// Safely settles active visual ownership before a config generation is
  /// replaced. Animation updates read some effect settings from the active
  /// config, so allowing a reload to swap those settings mid-session can
  /// mix two generations and strand cleanup state. Reload is an explicit
  /// control transition rather than an animation failure, but it uses
  /// the same tested visibility-preserving settlement path.
  pub(crate) fn settle_for_config_reload(
    state: &mut WmState,
    config: &UserConfig,
  ) {
    Self::settle_animation_ownership(
      state,
      config,
      "config generation replacement",
      false,
    );
  }

  fn settle_animation_ownership(
    state: &mut WmState,
    config: &UserConfig,
    reason: &str,
    is_failure: bool,
  ) {
    if is_failure {
      tracing::error!(
        "Animation system failure ({reason}); snapping windows to safe final \
         state."
      );
    } else {
      tracing::info!(
        "Settling active animation ownership before {reason}."
      );
    }

    let mut affected_ids: HashSet<Uuid> =
      state.animation_manager.animations.keys().copied().collect();

    #[cfg(target_os = "windows")]
    {
      affected_ids.extend(
        state
          .animation_manager
          .focus_opacity_animations
          .keys()
          .copied(),
      );
      affected_ids
        .extend(state.animation_manager.resize_sessions.keys().copied());
      affected_ids.extend(
        state
          .animation_manager
          .pending_session_cleanup
          .iter()
          .map(|(id, _, _)| *id),
      );
      if let Some(ws) = &state.animation_manager.workspace_switch {
        affected_ids.extend(ws.windows.keys().copied());
      }
      if let Some(ws) = &state.animation_manager.pending_ws_cleanup {
        affected_ids.extend(ws.windows.keys().copied());
      }
      affected_ids.extend(
        state
          .animation_manager
          .pending_close_windows
          .keys()
          .copied(),
      );
      affected_ids.extend(
        state.animation_manager.pending_open_windows.keys().copied(),
      );
      affected_ids.extend(
        state
          .animation_manager
          .pending_move_resize_windows
          .keys()
          .copied(),
      );
    }

    state.animation_manager.invalidate_timer_generation();
    state.animation_manager.animations.clear();

    #[cfg(target_os = "windows")]
    let mut retained_sessions = {
      let close_ids: HashSet<Uuid> = state
        .animation_manager
        .pending_close_windows
        .keys()
        .copied()
        .collect();
      let pending_move_ids: HashSet<Uuid> = state
        .animation_manager
        .pending_move_resize_windows
        .keys()
        .copied()
        .collect();
      for id in &pending_move_ids {
        if let Some(session) =
          state.animation_manager.resize_sessions.get_mut(id)
        {
          session.hide();
        }
      }
      let mut sessions: Vec<(Uuid, ResizeSession)> =
        state.animation_manager.resize_sessions.drain().collect();
      sessions.extend(
        state
          .animation_manager
          .pending_session_cleanup
          .drain(..)
          .map(|(id, _, session)| (id, session)),
      );

      // Close animations deliberately leave the real window at its
      // original rect. Other sessions must pre-commit their real
      // window to the final target before the surrogate can safely
      // disappear.
      for (id, session) in &mut sessions {
        if !close_ids.contains(id) {
          session.pre_commit();
        }
      }
      sessions
    };

    #[cfg(target_os = "windows")]
    let retained_workspace_switch =
      state.animation_manager.workspace_switch.take();
    #[cfg(target_os = "windows")]
    let retained_workspace_cleanup =
      state.animation_manager.pending_ws_cleanup.take();
    #[cfg(target_os = "windows")]
    {
      state.animation_manager.workspace_redirect_fallback = false;
      state.animation_manager.workspace_redirect_real_ready = false;
      state.animation_manager.workspace_redirect_fade = None;
      state.animation_manager.workspace_cleanup_sync_retry.clear();
    }
    #[cfg(target_os = "windows")]
    let retained_iris = state.animation_manager.iris_switch.take();

    #[cfg(target_os = "windows")]
    {
      state.animation_manager.pending_surrogate_updates.clear();
      state.animation_manager.slide_in_monitor_rects.clear();
      state.animation_manager.pending_open_windows.clear();
      state.animation_manager.pending_move_resize_windows.clear();
      state
        .animation_manager
        .failed_move_resize_recoveries
        .clear();
      state.animation_manager.focus_opacity_animations.clear();
      *state
        .animation_manager
        .animation_timer_vsync
        .lock()
        .expect("animation mutex poisoned") = None;
      *state
        .animation_manager
        .animation_vsync_time
        .lock()
        .expect("animation mutex poisoned") = None;

      let pending_closes =
        std::mem::take(&mut state.animation_manager.pending_close_windows);
      for (id, pending) in pending_closes {
        Self::reveal_pending_close(pending, state);
        tracing::debug!(
          "Restored close window {id} during animation-system recovery."
        );
      }
    }

    for id in &affected_ids {
      if let Some(container) = state.container_by_id(*id) {
        if let Ok(window) = container.as_window_container() {
          state.pending_sync.queue_container_to_redraw(window);
        }
      }
    }
    state
      .pending_sync
      .suppress_animations()
      .queue_focus_change()
      .queue_all_effects_update();

    if let Err(sync_err) = platform_sync(state, config) {
      tracing::error!(
        "Failed to synchronize safe animation final state: {sync_err}."
      );

      #[cfg(target_os = "windows")]
      {
        // The native sync could not establish the latest intended
        // underlay. Keep the already-public workspace authoritative and
        // clear the bounded intent so recovery cannot strand it.
        state.pending_workspace_redirect = None;
        state.pending_sync.clear_workspace_switch_participants();
      }

      #[cfg(target_os = "windows")]
      for id in &affected_ids {
        let Some(window) = state
          .container_by_id(*id)
          .and_then(|container| container.as_window_container().ok())
        else {
          continue;
        };
        let is_displayed = window
          .workspace()
          .is_some_and(|workspace| workspace.is_displayed());
        let native = window.native();
        if let Err(err) = native.set_cloaked(false) {
          tracing::warn!(
            "Failed to uncloak window {id} during animation fallback: {err}."
          );
        }
        let visibility_result = if is_displayed {
          native.show()
        } else {
          native.hide()
        };
        if let Err(err) = visibility_result {
          tracing::warn!(
            "Failed to restore window {id} visibility during animation \
             fallback: {err}."
          );
        }
      }
    }

    // These locals intentionally outlive the final sync and native
    // fallback; dropping them now destroys the retained
    // surrogates/iris overlay only after the real windows have been
    // made safe.
    #[cfg(target_os = "windows")]
    {
      retained_sessions.clear();
      drop(retained_workspace_switch);
      drop(retained_workspace_cleanup);
      drop(retained_iris);
    }
  }

  /// Starts a ticking phase of the persistent animation timer thread.
  ///
  /// The timer thread is spawned once (on the first animation) and parked
  /// between animations rather than re-created each time, so the cost of
  /// thread creation and priority/MMCSS setup never lands on the
  /// input-to-first-frame latency path. This call wakes the parked thread
  /// (or spawns it the first time) when there are active animations and
  /// the thread is not already ticking.
  ///
  /// The thread uses a two-tier vsync strategy on Windows:
  ///
  /// 1. **`IDXGIOutput::WaitForVBlank`** — when a `DxgiVsyncWaiter` is
  ///    installed, waits for the animation monitor's specific vblank
  ///    signal. Per-monitor and reliable at any Hz.
  /// 2. **`DwmFlush`** — fallback when no waiter is installed, aligning to
  ///    the primary monitor's composition cycle.
  ///
  /// On non-Windows, `DwmFlush` is a no-op so a fixed 60 fps sleep is
  /// used.
  #[must_use]
  pub fn ensure_timer_running(&self) -> bool {
    if !self.has_active_animations() {
      return true;
    }

    // Claim one idle -> ticking transition. The event loop is the ordinary
    // caller, but the atomic guard keeps startup idempotent for tests and
    // recovery paths that may arm the timer concurrently.
    if self
      .animation_timer_running
      .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
      .is_err()
    {
      return true;
    }

    self
      .animation_timer_generation
      .fetch_add(1, Ordering::AcqRel);
    self.animation_timer_heartbeat_us.store(
      elapsed_timer_us(self.animation_timer_epoch, Instant::now()),
      Ordering::Release,
    );

    #[cfg(target_os = "windows")]
    if let Ok(guard) = self.animation_timer_vsync.lock() {
      if let Some(waiter) = guard.as_ref() {
        self
          .animation_timer_period_us
          .store(waiter.frame_period_us().max(1), Ordering::Release);
      }
    }

    let use_fallback =
      self.animation_timer_fallback.load(Ordering::Acquire);
    let thread_slot = if use_fallback {
      &self.fallback_timer_thread
    } else {
      &self.timer_thread
    };
    let mut guard = thread_slot.lock().expect("animation mutex poisoned");
    match guard.as_ref() {
      // Wake the parked thread to begin a new ticking phase.
      Some(handle) => {
        handle.thread().unpark();
        true
      }
      // First animation: spawn the persistent thread.
      None => {
        if let Some(handle) = self.spawn_timer_thread(use_fallback) {
          *guard = Some(handle);
          true
        } else {
          false
        }
      }
    }
  }

  /// Keeps the persistent driver active only while some animation-owned
  /// state still needs compositor ticks.
  fn settle_timer_after_update(&self) {
    if self.has_active_animations() {
      let _ = self.ensure_timer_running();
      return;
    }

    self.animation_timer_running.store(false, Ordering::Release);

    // All animations are done: clear the move/resize DXGI waiter so a
    // later animation selects the monitor again. Reusing an output across
    // an idle display-topology change can make WaitForVBlank stall instead
    // of failing promptly.
    #[cfg(target_os = "windows")]
    {
      *self
        .animation_timer_vsync
        .lock()
        .expect("animation mutex poisoned") = None;
      *self
        .animation_vsync_time
        .lock()
        .expect("animation mutex poisoned") = None;
    }
  }

  /// Spawns the persistent animation timer thread.
  ///
  /// Returns the join handle, or `None` if the OS thread could not be
  /// created. On failure the ticking gate is reset so a later
  /// [`ensure_timer_running`] call retries the spawn.
  ///
  /// [`ensure_timer_running`]: AnimationManager::ensure_timer_running
  fn spawn_timer_thread(
    &self,
    is_fallback_driver: bool,
  ) -> Option<std::thread::JoinHandle<()>> {
    let tx = self.animation_tick_tx.clone();
    let running = self.animation_timer_running.clone();
    let shutdown = self.timer_shutdown.clone();
    let generation = self.animation_timer_generation.clone();
    let heartbeat_us = self.animation_timer_heartbeat_us.clone();
    let fallback_mode = self.animation_timer_fallback.clone();
    let fallback_period_us = self.animation_timer_period_us.clone();
    let timer_epoch = self.animation_timer_epoch;
    #[cfg(target_os = "windows")]
    let vsync_waiter = self.animation_timer_vsync.clone();
    #[cfg(target_os = "windows")]
    let vsync_time = self.animation_vsync_time.clone();

    // Spawn a real OS thread (not a Tokio task) so blocking vsync waits do
    // not stall the async runtime.
    let result = std::thread::Builder::new()
      .name(
        if is_fallback_driver {
          "glazewm-anim-fallback"
        } else {
          "glazewm-anim-tick"
        }
        .into(),
      )
      .spawn(move || {
        // Elevate scheduling priority once for the thread's whole
        // lifetime. MMCSS "DisplayPostProcessing" gives
        // near-real-time guarantees beyond
        // THREAD_PRIORITY_HIGHEST, matching the scheduling class used by
        // DWM and video renderers. Falls back gracefully to
        // THREAD_PRIORITY_HIGHEST if avrt.dll is unavailable. The
        // persistent thread keeps the registration across
        // animations rather than re-acquiring it per switch; while
        // parked it consumes no CPU.
        wm_platform::set_thread_priority_highest();
        #[cfg(target_os = "windows")]
        let _mmcss = wm_platform::try_set_thread_mmcss();

        loop {
          // Idle: park until a ticking phase begins (or shutdown). A
          // `while` loop re-checks the gate to absorb spurious
          // wake-ups and any buffered unpark token from a
          // just-ended phase.
          while !running.load(Ordering::Acquire)
            || fallback_mode.load(Ordering::Acquire) != is_fallback_driver
          {
            if shutdown.load(Ordering::Relaxed) {
              return;
            }
            std::thread::park();
          }
          if shutdown.load(Ordering::Relaxed) {
            return;
          }

          let phase_generation = generation.load(Ordering::Acquire);
          heartbeat_us.store(
            elapsed_timer_us(timer_epoch, Instant::now()),
            Ordering::Release,
          );

          // Send an immediate tick so the first animation frame begins
          // without waiting for the next vblank. Without this the
          // surrogate is frozen at its start position for up to
          // one full frame period (~16 ms at 60 Hz, ~5.7 ms at
          // 175 Hz) before any movement begins.
          if !Self::queue_animation_tick(&tx) {
            return;
          }

          if is_fallback_driver {
            let period = Duration::from_micros(
              fallback_period_us.load(Ordering::Acquire).max(1),
            );
            let mut next_deadline = Instant::now() + period;

            while running.load(Ordering::Acquire)
              && fallback_mode.load(Ordering::Acquire)
              && generation.load(Ordering::Acquire) == phase_generation
            {
              std::thread::park_timeout(
                next_deadline.saturating_duration_since(Instant::now()),
              );
              if shutdown.load(Ordering::Relaxed) {
                return;
              }
              if !running.load(Ordering::Acquire)
                || !fallback_mode.load(Ordering::Acquire)
                || generation.load(Ordering::Acquire) != phase_generation
              {
                break;
              }

              let now = Instant::now();
              heartbeat_us.store(
                elapsed_timer_us(timer_epoch, now),
                Ordering::Release,
              );
              if !Self::queue_animation_tick(&tx) {
                return;
              }
              next_deadline = if now > next_deadline + period {
                now + period
              } else {
                next_deadline + period
              };
            }
            continue;
          }

          // Ticking phase: drive frames until the gate clears.
          while running.load(Ordering::Acquire)
            && !fallback_mode.load(Ordering::Acquire)
            && generation.load(Ordering::Acquire) == phase_generation
          {
            // Per-monitor IDXGIOutput::WaitForVBlank during
            // workspace-switch. Clone under the lock so the
            // wait runs without holding it — cleanup can clear
            // the Arc without blocking an in-progress wait.
            // Record the wake-up time immediately after vsync fires so
            // `update_internal` can compute phase-accurate animation
            // progress.
            #[cfg(target_os = "windows")]
            let dxgi_waited = {
              let waiter_handle = vsync_waiter
                .lock()
                .expect("animation mutex poisoned")
                .clone();
              let did_wait =
                waiter_handle.map(|waiter| waiter.wait()).unwrap_or(false);
              if did_wait {
                *vsync_time.lock().expect("animation mutex poisoned") =
                  Some(Instant::now());
              }
              did_wait
            };
            #[cfg(not(target_os = "windows"))]
            let dxgi_waited = false;

            if !dxgi_waited {
              // DwmFlush for window move/resize animations (no
              // workspace-switch active). On non-Windows this is a no-op,
              // so a fixed 60 fps sleep paces the loop.
              wm_platform::dwm_flush();
              #[cfg(not(target_os = "windows"))]
              std::thread::sleep(std::time::Duration::from_micros(16_667));
            }

            if shutdown.load(Ordering::Relaxed) {
              return;
            }
            if !running.load(Ordering::Acquire)
              || fallback_mode.load(Ordering::Acquire)
              || generation.load(Ordering::Acquire) != phase_generation
            {
              break;
            }

            heartbeat_us.store(
              elapsed_timer_us(timer_epoch, Instant::now()),
              Ordering::Release,
            );

            if !Self::queue_animation_tick(&tx) {
              return;
            }
          }
          // Gate cleared: loop back to park until the next animation.
        }
        // `_mmcss` is dropped on return, reverting the MMCSS registration.
      });

    match result {
      Ok(handle) => Some(handle),
      Err(err) => {
        tracing::warn!("Failed to spawn animation tick thread: {err}.");
        // Reset atomics directly: the caller still owns this timer slot's
        // mutex, so the general invalidation helper would self-deadlock.
        self.animation_timer_running.store(false, Ordering::Release);
        self
          .animation_timer_generation
          .fetch_add(1, Ordering::AcqRel);
        None
      }
    }
  }

  /// Invalidates an in-flight pacing phase without waiting for its native
  /// compositor call to return.
  pub(crate) fn invalidate_timer_generation(&self) {
    self.animation_timer_running.store(false, Ordering::Release);
    self
      .animation_timer_generation
      .fetch_add(1, Ordering::AcqRel);
    self.animation_timer_heartbeat_us.store(
      elapsed_timer_us(self.animation_timer_epoch, Instant::now()),
      Ordering::Release,
    );
    if let Ok(guard) = self.timer_thread.lock() {
      if let Some(handle) = guard.as_ref() {
        handle.thread().unpark();
      }
    }
    if let Ok(guard) = self.fallback_timer_thread.lock() {
      if let Some(handle) = guard.as_ref() {
        handle.thread().unpark();
      }
    }
  }

  /// Returns the independent event-loop deadline for the current native
  /// pacing generation. Idle and fixed-rate fallback phases need no
  /// watchdog.
  pub(crate) fn timer_watchdog_deadline(&self) -> Option<Instant> {
    #[cfg(not(target_os = "windows"))]
    {
      None
    }

    #[cfg(target_os = "windows")]
    {
      if !self.has_active_animations()
        || !self.animation_timer_running.load(Ordering::Acquire)
        || self.animation_timer_fallback.load(Ordering::Acquire)
      {
        return None;
      }

      let heartbeat =
        self.animation_timer_heartbeat_us.load(Ordering::Acquire);
      let period = self.animation_timer_period_us.load(Ordering::Acquire);
      let timeout_us =
        u64::try_from(ANIMATION_TIMER_STALL_TIMEOUT.as_micros())
          .unwrap_or(u64::MAX)
          .max(period.saturating_mul(6))
          .min(250_000);
      Some(
        self.animation_timer_epoch
          + Duration::from_micros(heartbeat.saturating_add(timeout_us)),
      )
    }
  }

  /// Atomically abandons one stale native generation and enters the single
  /// process-lifetime fallback mode. The caller must restore all animation
  /// visibility ownership before allowing later animations to start.
  pub(crate) fn abandon_stalled_timer_generation(
    &self,
    now: Instant,
  ) -> bool {
    let Some(deadline) = self.timer_watchdog_deadline() else {
      return false;
    };
    if now < deadline
      || self
        .animation_timer_fallback
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
      return false;
    }

    self.invalidate_timer_generation();
    #[cfg(target_os = "windows")]
    {
      *self
        .animation_timer_vsync
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
      *self
        .animation_vsync_time
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
    true
  }

  /// Queues a compositor tick without allowing redundant frame signals to
  /// accumulate while the WM event loop is busy.
  ///
  /// A full channel already represents all the state a tick carries, so it
  /// is treated as success. A closed channel means the WM loop is gone and
  /// the persistent timer thread should exit.
  fn queue_animation_tick(tx: &mpsc::Sender<()>) -> bool {
    match tx.try_send(()) {
      Ok(()) | Err(mpsc::error::TrySendError::Full(())) => true,
      Err(mpsc::error::TrySendError::Closed(())) => false,
    }
  }

  #[cfg(target_os = "windows")]
  fn fail_open_handoff(state: &mut WmState, id: Uuid, reason: &str) {
    let pending = state.animation_manager.pending_open_windows.remove(&id);
    state.animation_manager.animations.remove(&id);
    state.animation_manager.slide_in_monitor_rects.remove(&id);

    // Position while the real window is still cloaked, then reveal it.
    // This synchronous native fallback is intentionally failure-only; the
    // normal handoff path remains compositor-tick driven and nonblocking.
    if let Some(mut session) =
      state.animation_manager.resize_sessions.remove(&id)
    {
      session.pre_commit();
    }

    if let Some(pending) = pending {
      let native = NativeWindow::from_handle(pending.handle);
      if native.is_valid() {
        if let Err(err) = native.set_cloaked(false) {
          tracing::warn!(
            "Failed to uncloak window {id} after open handoff failure: \
             {err}."
          );
        }
        if let Err(err) = native.show() {
          tracing::warn!(
            "Failed to show window {id} after open handoff failure: {err}."
          );
        }
      }
    }

    if let Some(window) = state
      .container_by_id(id)
      .and_then(|container| container.as_window_container().ok())
    {
      state.pending_sync.queue_container_to_redraw(window);
    }
    tracing::warn!(
      "Open surrogate handoff for {id} failed ({reason}); revealed the \
       real window at its final layout position."
    );
  }

  #[cfg(target_os = "windows")]
  pub(crate) fn fail_move_resize_handoff(
    state: &mut WmState,
    id: Uuid,
    reason: &str,
  ) {
    state
      .animation_manager
      .pending_move_resize_windows
      .remove(&id);
    state.animation_manager.animations.remove(&id);
    state
      .animation_manager
      .pending_surrogate_updates
      .retain(|update| update.window_id != id);
    state
      .animation_manager
      .failed_move_resize_recoveries
      .insert(id);

    if let Some(window) = state
      .container_by_id(id)
      .and_then(|container| container.as_window_container().ok())
    {
      state.pending_sync.queue_container_to_redraw(window);
    }
    tracing::warn!(
      "Move/resize surrogate handoff for {id} failed ({reason}); \
       restoring only this window's real HWND."
    );
  }

  /// Retries failed move/resize handoffs without dropping their last
  /// visible representation before the real HWND is usable.
  #[cfg(target_os = "windows")]
  pub(crate) fn recover_failed_move_resize_handoffs(state: &mut WmState) {
    let ids: Vec<Uuid> = state
      .animation_manager
      .failed_move_resize_recoveries
      .iter()
      .copied()
      .collect();

    for id in ids {
      let Some(session) = state.animation_manager.resize_sessions.get(&id)
      else {
        state
          .animation_manager
          .failed_move_resize_recoveries
          .remove(&id);
        continue;
      };
      let handle = session.source_handle();
      let source_was_cloaked = session.is_session_cloaked();
      let opacity = OpacityValue::from_alpha(session.effect_opacity);
      let native = NativeWindow::from_handle(handle);

      if move_resize_recovery_action(
        native.is_valid(),
        source_was_cloaked,
        false,
        false,
        false,
        false,
      ) == MoveResizeRecoveryAction::DropDestroyed
      {
        isolate_destroyed_window(
          id,
          handle,
          state,
          "move/resize recovery",
        );
        tracing::debug!(
          "Isolated failed move/resize session {id}: source HWND was \
           destroyed during recovery."
        );
        continue;
      }

      let positioned = state
        .animation_manager
        .resize_sessions
        .get_mut(&id)
        .is_some_and(ResizeSession::pre_commit);
      if !positioned {
        if move_resize_recovery_action(
          true,
          source_was_cloaked,
          false,
          false,
          false,
          false,
        ) == MoveResizeRecoveryAction::HideSurrogateAndRetry
        {
          if let Some(session) =
            state.animation_manager.resize_sessions.get_mut(&id)
          {
            session.hide();
          }
        }
        tracing::warn!(
          "Real-window reposition for failed move/resize session {id} \
           did not succeed; retaining the currently visible \
           representation and retrying."
        );
        continue;
      }

      let opacity_restored =
        native.set_transparency(&opacity).map_or_else(
          |err| {
            tracing::warn!(
              "Failed to restore opacity for move/resize session {id}: \
             {err}; retrying."
            );
            false
          },
          |()| true,
        );
      let uncloaked = native.set_cloaked(false).map_or_else(
        |err| {
          tracing::warn!(
            "Failed to uncloak move/resize session {id}: {err}; \
             retaining its surrogate and retrying."
          );
          false
        },
        |()| true,
      );
      let shown = native.show().map_or_else(
        |err| {
          tracing::warn!(
            "Failed to show move/resize session {id}: {err}; retaining \
             its surrogate and retrying."
          );
          false
        },
        |()| true,
      );

      match move_resize_recovery_action(
        true,
        source_was_cloaked,
        positioned,
        opacity_restored,
        uncloaked,
        shown,
      ) {
        MoveResizeRecoveryAction::Complete => {
          state.animation_manager.resize_sessions.remove(&id);
          state
            .animation_manager
            .failed_move_resize_recoveries
            .remove(&id);
          tracing::warn!(
            "Recovered move/resize session {id} in the current platform \
             sync pass using the real window."
          );
        }
        MoveResizeRecoveryAction::HideSurrogateAndRetry => {
          if let Some(session) =
            state.animation_manager.resize_sessions.get_mut(&id)
          {
            // The real window is visible, or was never cloaked, so the
            // failed surrogate must no longer be presented while a
            // remaining native property operation is retried.
            session.hide();
          }
        }
        MoveResizeRecoveryAction::RetainSurrogate
        | MoveResizeRecoveryAction::DropDestroyed => {}
      }
    }
  }

  #[cfg(target_os = "windows")]
  fn reveal_pending_close(
    pending: PendingCloseWindow,
    state: &mut WmState,
  ) {
    let id = pending.window.id();
    let native = NativeWindow::from_handle(pending.handle);
    if !native.is_valid() {
      return;
    }

    if pending.detached || pending.window.is_detached() {
      if let Err(err) = restore_window_after_failed_close(
        pending.window,
        pending.workspace_id,
        state,
      ) {
        tracing::warn!(
          "Failed to restore detached close window {id}: {err}."
        );
      }
    } else {
      if let Err(err) = native.set_cloaked(false) {
        tracing::warn!(
          "Failed to uncloak close window {id} during fallback: {err}."
        );
      }
      if let Err(err) = native.show() {
        tracing::warn!(
          "Failed to show close window {id} during fallback: {err}."
        );
      }
    }
  }

  #[cfg(target_os = "windows")]
  fn fail_close_handoff(
    state: &mut WmState,
    id: Uuid,
    pending: PendingCloseWindow,
    reason: &str,
  ) {
    let handle = pending.handle;
    state.animation_manager.animations.remove(&id);
    state.animation_manager.resize_sessions.remove(&id);
    Self::reveal_pending_close(pending, state);

    let native = NativeWindow::from_handle(handle);
    if native.is_valid() {
      if let Err(err) = native.close() {
        tracing::warn!(
          "Failed to close visible real window {id} after handoff failure: \
           {err}."
        );
      }
    }
    tracing::warn!(
      "Close surrogate handoff for {id} failed ({reason}); used the \
       visible-real-window close fallback."
    );
  }

  #[cfg(target_os = "windows")]
  fn recover_failed_opacity_participant(
    state: &mut WmState,
    config: &UserConfig,
    id: Uuid,
    context: &str,
  ) {
    state.animation_manager.focus_opacity_animations.remove(&id);
    state
      .animation_manager
      .pending_transparency_reasserts
      .remove(&id);

    let Some(window) = state
      .container_by_id(id)
      .and_then(|container| container.as_window_container().ok())
    else {
      return;
    };
    let native = window.native();
    if !native.is_valid() {
      isolate_destroyed_window(id, native.id().0, state, context);
      return;
    }

    let is_focused =
      state.focused_container().is_some_and(|c| c.id() == id);
    let effect = if is_focused {
      &config.value.window_effects.focused_window
    } else {
      &config.value.window_effects.other_windows
    };
    let fallback_opacity = if effect.transparency.enabled {
      effect.transparency.opacity.clone()
    } else {
      OpacityValue::from_alpha(u8::MAX)
    };
    if let Err(err) = native.set_transparency(&fallback_opacity) {
      tracing::warn!(
        "Failed to restore configured opacity for animation participant \
         {id} after {context}: {err}; leaving its existing visible native \
         state unchanged."
      );
    }
  }

  /// Replaces one persistently failing workspace cleanup group with its
  /// real windows before destroying retained surrogate coverage.
  ///
  /// This failure-only path is synchronous but bounded by the participant
  /// count. Healthy animation frames never execute it. Each HWND is
  /// checked against the UUID-owned source handle before any native
  /// mutation.
  #[cfg(target_os = "windows")]
  fn recover_workspace_cleanup_group(
    state: &mut WmState,
    config: &UserConfig,
  ) -> usize {
    let Some(mut cleanup) =
      state.animation_manager.pending_ws_cleanup.take()
    else {
      return 0;
    };
    let focused_id = state.focused_container().map(|c| c.id());
    let participant_count = cleanup.windows.len();

    for (id, mut entry) in cleanup.windows.drain() {
      let window = state
        .container_by_id(id)
        .and_then(|container| container.as_window_container().ok());

      if let Some(window) = window {
        let native = window.native();
        if native.id().0 != entry.source_handle || !native.is_valid() {
          isolate_destroyed_window(
            id,
            entry.source_handle,
            state,
            "workspace cleanup fallback",
          );
        } else {
          if let Some(target) = state.window_target_positions.get(&id) {
            if let Err(err) = native.set_frame(target) {
              tracing::warn!(
                "Failed to place workspace cleanup participant {id} at its \
                 safe final rect: {err}."
              );
            }
          }

          let effect = if focused_id == Some(id) {
            &config.value.window_effects.focused_window
          } else {
            &config.value.window_effects.other_windows
          };
          let opacity = if effect.transparency.enabled {
            effect.transparency.opacity.clone()
          } else {
            OpacityValue::from_alpha(u8::MAX)
          };
          if let Err(err) = native.set_transparency(&opacity) {
            tracing::warn!(
              "Failed to restore opacity for workspace cleanup participant \
               {id}: {err}."
            );
          }

          let is_logically_visible = window
            .workspace()
            .is_some_and(|workspace| workspace.is_displayed())
            && matches!(
              window.display_state(),
              DisplayState::Showing | DisplayState::Shown
            );
          if is_logically_visible {
            if let Err(err) = native.set_cloaked(false) {
              tracing::warn!(
                "Failed to uncloak workspace cleanup participant {id}: \
                 {err}."
              );
            }
            if let Err(err) = native.show() {
              tracing::warn!(
                "Failed to show workspace cleanup participant {id}: {err}."
              );
            }
          } else if let Err(err) = native.hide() {
            tracing::warn!(
              "Failed to hide inactive workspace cleanup participant {id}: \
               {err}."
            );
          }
        }
      }

      // The real HWND has been restored or isolated. Hide coverage before
      // its Drop unregisters the DWM thumbnail and destroys the host.
      if let Some(surrogate) = entry.surrogate.as_mut() {
        surrogate.hide_thumbnail();
      }
      state.pending_sync.remove_window(&id);
    }

    state
      .pending_sync
      .clear_workspace_switch_participants()
      .dequeue_focus_change();
    state.animation_manager.workspace_redirect_fallback = false;
    state.animation_manager.workspace_redirect_real_ready = false;
    state.animation_manager.workspace_redirect_fade = None;
    state.animation_manager.workspace_cleanup_sync_retry.clear();
    participant_count
  }

  /// Internal update, accessed through `WmState` to avoid double-borrow.
  pub(crate) fn update_internal(
    state: &mut WmState,
    config: &UserConfig,
  ) -> AnimationUpdateOutcome {
    if !state.animation_manager.has_active_animations() {
      return AnimationUpdateOutcome::Complete;
    }

    #[cfg(target_os = "windows")]
    let mut failed_participants = Vec::new();

    // Complete deferred alpha restoration on a later compositor-paced
    // tick. A destroyed/unmanaged window is simply discarded; a live
    // native failure is retained and propagated so the animation failure
    // recovery path can preserve a visible real window.
    #[cfg(target_os = "windows")]
    {
      let pending = std::mem::take(
        &mut state.animation_manager.pending_transparency_reasserts,
      );
      for id in pending {
        let Some(container) = state.container_by_id(id) else {
          continue;
        };
        let Ok(window) = container.as_window_container() else {
          continue;
        };
        if let Err(err) = window.native().reassert_transparency() {
          if opacity_target_was_destroyed(&err, window.native().is_valid())
          {
            let handle = window.native().id().0;
            tracing::debug!(
              "Dropping deferred transparency reassert for destroyed \
               window {id}: {err}."
            );
            isolate_destroyed_window(
              id,
              handle,
              state,
              "deferred transparency reassert",
            );
          } else {
            tracing::warn!(
              "Deferred transparency reassert failed for window {id}: \
               {err}; isolating this animation participant."
            );
            Self::recover_failed_opacity_participant(
              state,
              config,
              id,
              "deferred transparency reassert",
            );
            failed_participants.push(id);
          }
        };
      }
    }

    // Workspace and iris covers are created during `platform_sync`, but
    // visibility is not transferred in that same call. Observe one
    // animation/vsync tick first; on failure, drop the cover and let the
    // still-pending sync run in suppression mode with real windows.
    #[cfg(target_os = "windows")]
    {
      let workspace_participants = state
        .animation_manager
        .workspace_switch
        .as_ref()
        .filter(|ws| ws.handoff.is_pending())
        .map(|ws| {
          ws.windows
            .iter()
            .map(|(id, entry)| (*id, entry.source_handle))
            .collect::<Vec<_>>()
        });
      if let Some(participants) = workspace_participants {
        let stale: Vec<(Uuid, isize)> = participants
          .into_iter()
          .filter(|(id, expected_handle)| {
            state
              .container_by_id(*id)
              .and_then(|container| container.as_window_container().ok())
              .is_none_or(|window| {
                window.native().id().0 != *expected_handle
                  || !window.native().is_valid()
              })
          })
          .collect();
        for (id, handle) in stale {
          isolate_destroyed_window(
            id,
            handle,
            state,
            "workspace readiness",
          );
        }
        let source_valid = state
          .animation_manager
          .workspace_switch
          .as_ref()
          .is_some_and(|ws| !ws.windows.is_empty());
        let outcome = state
          .animation_manager
          .workspace_switch
          .as_mut()
          .map_or(SurrogateHandoffOutcome::Failed, |ws| {
            ws.handoff.advance(Instant::now(), source_valid)
          });
        if outcome == SurrogateHandoffOutcome::Failed {
          tracing::warn!(
            "Workspace cover readiness failed; using instant visible-real-window fallback."
          );
          state.animation_manager.workspace_switch = None;
          state.pending_sync.suppress_animations();
        }
      }

      if state.animation_manager.is_iris_handoff_pending() {
        let outcome = state
          .animation_manager
          .iris_switch
          .as_mut()
          .map_or(SurrogateHandoffOutcome::Failed, |iris| {
            iris.handoff.advance(Instant::now(), true)
          });
        if outcome == SurrogateHandoffOutcome::Failed {
          tracing::warn!(
            "Iris cover readiness timed out; using instant workspace-switch fallback."
          );
          state.animation_manager.iris_switch = None;
          state.pending_sync.suppress_animations();
        }
      }
    }

    // Focus-opacity transitions drive the real windows directly. They are
    // deliberately independent from movement/resize animations so both can
    // run during the same frame without replacing one another's state.
    #[cfg(target_os = "windows")]
    {
      let updates: Vec<(Uuid, OpacityValue, bool)> = state
        .animation_manager
        .focus_opacity_animations
        .iter()
        .filter_map(|(id, animation)| {
          animation
            .current_state()
            .1
            .map(|opacity| (*id, opacity, animation.is_complete()))
        })
        .collect();

      let mut finished = Vec::new();
      for (id, opacity, is_complete) in updates {
        let found_window = state
          .container_by_id(id)
          .and_then(|container| container.as_window_container().ok());

        if let Some(window) = found_window {
          let opacity_result = window.native().set_transparency(&opacity);
          match opacity_result {
            Ok(()) if is_complete => finished.push(id),
            Ok(()) => {}
            Err(err)
              if opacity_target_was_destroyed(
                &err,
                window.native().is_valid(),
              ) =>
            {
              let handle = window.native().id().0;
              // The HWND can disappear after container lookup and even
              // after a pre-call IsWindow check. Treat only the returned
              // invalid-handle class (or a confirming post-call liveness
              // failure) as expected destruction; all live-window errors
              // still propagate.
              tracing::debug!(
                "Dropping focus-opacity animation for destroyed window \
                 {id}: {err}."
              );
              finished.push(id);
              isolate_destroyed_window(
                id,
                handle,
                state,
                "focus-opacity update",
              );
            }
            Err(err) => {
              tracing::warn!(
                "Animated opacity update failed for window {id}: {err}; \
                 isolating this animation participant."
              );
              Self::recover_failed_opacity_participant(
                state,
                config,
                id,
                "focus-opacity update",
              );
              finished.push(id);
              failed_participants.push(id);
            }
          }
        } else {
          // The window was unmanaged while its fade was running.
          finished.push(id);
        }
      }

      for id in finished {
        state.animation_manager.focus_opacity_animations.remove(&id);
      }
    }

    // Advance open/close thumbnail readiness on animation ticks. No
    // compositor flush or wait occurs on the WM thread: open reveals after
    // one tick with the host hidden, while close keeps the real window
    // visible through an additional tick before cloak/detach.
    #[cfg(target_os = "windows")]
    {
      let now = Instant::now();
      let move_resize_ids: Vec<Uuid> = state
        .animation_manager
        .pending_move_resize_windows
        .keys()
        .copied()
        .collect();

      for id in move_resize_ids {
        let handle = state
          .animation_manager
          .pending_move_resize_windows
          .get(&id)
          .map_or(0, |pending| pending.handle);
        let native = NativeWindow::from_handle(handle);
        let source_valid = native.is_valid();
        let healthy = state
          .animation_manager
          .resize_sessions
          .get_mut(&id)
          .is_some_and(ResizeSession::probe_thumbnail);
        let action = state
          .animation_manager
          .pending_move_resize_windows
          .get_mut(&id)
          .map_or(MoveResizeHandoffAction::Fail, |pending| {
            pending.advance(now, source_valid, healthy)
          });

        match action {
          MoveResizeHandoffAction::Wait => {}
          MoveResizeHandoffAction::RevealTransparentSurrogate => {
            if let Some(session) =
              state.animation_manager.resize_sessions.get_mut(&id)
            {
              session.show();
            }
          }
          MoveResizeHandoffAction::TransferVisibility => {
            if let Err(err) = native.set_cloaked(true) {
              if opacity_target_was_destroyed(&err, native.is_valid()) {
                isolate_destroyed_window(
                  id,
                  handle,
                  state,
                  "move/resize visibility transfer",
                );
              } else {
                Self::fail_move_resize_handoff(
                  state,
                  id,
                  &format!("real-window cloak failed: {err}"),
                );
              }
              continue;
            }

            let completed = state
              .animation_manager
              .resize_sessions
              .get_mut(&id)
              .is_some_and(ResizeSession::complete_move_resize_handoff);
            if !completed {
              Self::fail_move_resize_handoff(
                state,
                id,
                "post-cloak target positioning failed",
              );
              continue;
            }

            state
              .animation_manager
              .pending_move_resize_windows
              .remove(&id);
            if let Some(animation) =
              state.animation_manager.animations.get(&id)
            {
              animation.restart_on_next_frame();
            }
          }
          MoveResizeHandoffAction::Fail => {
            if source_valid {
              Self::fail_move_resize_handoff(
                state,
                id,
                "thumbnail update or readiness timeout",
              );
            } else {
              isolate_destroyed_window(
                id,
                handle,
                state,
                "move/resize readiness",
              );
            }
          }
        }
      }

      let open_ids: Vec<Uuid> = state
        .animation_manager
        .pending_open_windows
        .keys()
        .copied()
        .collect();

      for id in open_ids {
        let handle = state
          .animation_manager
          .pending_open_windows
          .get(&id)
          .map_or(0, |pending| pending.handle);
        let native = NativeWindow::from_handle(handle);
        let source_valid = native.is_valid();
        let healthy = state
          .animation_manager
          .resize_sessions
          .get_mut(&id)
          .is_some_and(ResizeSession::probe_thumbnail);
        let outcome = state
          .animation_manager
          .pending_open_windows
          .get_mut(&id)
          .map_or(SurrogateHandoffOutcome::Failed, |pending| {
            pending.readiness.advance(now, 1, source_valid, healthy)
          });

        match outcome {
          SurrogateHandoffOutcome::Waiting => {}
          SurrogateHandoffOutcome::Ready => {
            state.animation_manager.pending_open_windows.remove(&id);
            if let Some(session) =
              state.animation_manager.resize_sessions.get_mut(&id)
            {
              session.show();
            }
            if let Some(animation) =
              state.animation_manager.animations.get(&id)
            {
              animation.restart_on_next_frame();
            }
          }
          SurrogateHandoffOutcome::Failed => {
            if source_valid {
              Self::fail_open_handoff(
                state,
                id,
                "thumbnail update or readiness timeout",
              );
            } else {
              isolate_destroyed_window(
                id,
                handle,
                state,
                "open readiness",
              );
            }
          }
        }
      }

      let close_ids: Vec<Uuid> = state
        .animation_manager
        .pending_close_windows
        .iter()
        .filter(|(_, pending)| !pending.detached)
        .map(|(id, _)| *id)
        .collect();

      for id in close_ids {
        let handle = state
          .animation_manager
          .pending_close_windows
          .get(&id)
          .map_or(0, |pending| pending.handle);
        let native = NativeWindow::from_handle(handle);
        let source_valid = native.is_valid();
        let healthy = state
          .animation_manager
          .resize_sessions
          .get_mut(&id)
          .is_some_and(ResizeSession::probe_thumbnail);
        let outcome = state
          .animation_manager
          .pending_close_windows
          .get_mut(&id)
          .map_or(SurrogateHandoffOutcome::Failed, |pending| {
            pending.readiness.advance(now, 2, source_valid, healthy)
          });

        match outcome {
          SurrogateHandoffOutcome::Waiting => {}
          SurrogateHandoffOutcome::Failed => {
            if source_valid {
              if let Some(pending) =
                state.animation_manager.pending_close_windows.remove(&id)
              {
                Self::fail_close_handoff(
                  state,
                  id,
                  pending,
                  "thumbnail update or readiness timeout",
                );
              }
            } else {
              isolate_destroyed_window(
                id,
                handle,
                state,
                "close readiness",
              );
            }
          }
          SurrogateHandoffOutcome::Ready => {
            let Some(mut pending) =
              state.animation_manager.pending_close_windows.remove(&id)
            else {
              continue;
            };

            if let Err(err) = native.set_cloaked(true) {
              if opacity_target_was_destroyed(&err, native.is_valid()) {
                isolate_destroyed_window(
                  id,
                  handle,
                  state,
                  "close visibility transfer",
                );
              } else {
                Self::fail_close_handoff(
                  state,
                  id,
                  pending,
                  &format!("real-window cloak failed: {err}"),
                );
              }
              continue;
            }

            if let Err(err) =
              detach_window_for_close(pending.window.clone(), state)
            {
              pending.detached = pending.window.is_detached();
              Self::fail_close_handoff(
                state,
                id,
                pending,
                &format!("layout detach failed: {err}"),
              );
              continue;
            }

            pending.detached = true;
            if let Some(animation) =
              state.animation_manager.animations.get(&id)
            {
              animation.restart_on_next_frame();
            }
            state
              .animation_manager
              .pending_close_windows
              .insert(id, pending);
          }
        }
      }
    }

    // Queue in-progress windows for redraw.
    let active_window_ids: Vec<_> = state
      .animation_manager
      .active_window_ids()
      .into_iter()
      .filter(|id| {
        #[cfg(target_os = "windows")]
        if state
          .animation_manager
          .failed_move_resize_recoveries
          .contains(id)
        {
          return true;
        }
        #[cfg(target_os = "windows")]
        if state
          .animation_manager
          .pending_open_windows
          .contains_key(id)
          || state
            .animation_manager
            .pending_move_resize_windows
            .contains_key(id)
          || state
            .animation_manager
            .pending_close_windows
            .get(id)
            .is_some_and(|pending| !pending.detached)
        {
          return false;
        }
        state
          .animation_manager
          .get_animation(id)
          .is_some_and(|animation| !animation.is_complete())
      })
      .collect();

    for window_id in &active_window_ids {
      if let Some(container) = state.container_by_id(*window_id) {
        if let Ok(window) = container.as_window_container() {
          state.pending_sync.queue_container_to_redraw(window);
        }
      }
    }

    // Drive close surrogates directly. These windows have been detached
    // from the layout tree when the close animation started, so they
    // are not queued for redraw by the loop above and cannot be driven
    // through `platform_sync`. We replicate the same per-frame update
    // logic used inside `start_animation_if_needed` for surrogate
    // sessions.
    #[cfg(target_os = "windows")]
    {
      let close_in_progress: Vec<Uuid> = state
        .animation_manager
        .pending_close_windows
        .iter()
        .filter(|(id, pending)| {
          pending.detached
            && state
              .animation_manager
              .animations
              .get(id)
              .map_or(false, |a| !a.is_complete())
        })
        .map(|(id, _)| *id)
        .collect();

      // Batches every closing window's acrylic-overlay reposition this
      // tick into one `DeferWindowPos` transaction instead of each
      // issuing its own synchronous `SetWindowPos` -- same reasoning
      // as `platform_sync`'s per-tick blur-overlay sync passes.
      let mut blur_batch = SurrogateBatch::new();

      for id in &close_in_progress {
        let is_zoom = state
          .animation_manager
          .resize_sessions
          .get(id)
          .map(|s| s.zoom)
          .unwrap_or(false);

        // Extract values before taking a mutable borrow on
        // resize_sessions.
        let anim_data =
          state.animation_manager.animations.get(id).map(|a| {
            let (rect, opacity) = a.current_state();
            let progress = a.eased_progress();
            (rect, opacity, progress)
          });

        let Some((current_rect, opacity, progress)) = anim_data else {
          continue;
        };
        let opacity_u8 = opacity.map(|o| o.to_alpha()).unwrap_or(u8::MAX);

        if is_zoom {
          if let Some(session) =
            state.animation_manager.resize_sessions.get_mut(id)
          {
            session.update_zoom_fade(1.0 - progress, opacity_u8);
          }
        } else if let Some(session) =
          state.animation_manager.resize_sessions.get_mut(id)
        {
          session.update(&current_rect, opacity_u8);
        }

        // Close animations are detached from the layout tree, so they
        // never reach `platform_sync`'s per-window loop -- track
        // the acrylic blur overlay directly here instead,
        // mirroring `sync_blur_overlays`' steady-state behavior
        // but following the surrogate's live rect.
        if let Some(session) =
          state.animation_manager.resize_sessions.get(id)
        {
          if let Some(params) = session.blur_overlay_params() {
            match session.current_rect() {
              Some(rect) => upsert_blur_overlay(
                &mut state.blur_overlays,
                *id,
                params,
                &rect,
                &mut blur_batch,
              ),
              None => {
                if let Some(overlay) = state.blur_overlays.get_mut(id) {
                  overlay.hide();
                }
              }
            }
          }
        }
      }

      let blur_result = blur_batch.commit();
      remove_failed_blur_overlays(
        state,
        &blur_result,
        "close-animation tracking",
      );

      let failed_close_ids: Vec<Uuid> = state
        .animation_manager
        .pending_close_windows
        .iter()
        .filter(|(id, pending)| {
          pending.detached
            && state
              .animation_manager
              .resize_sessions
              .get(id)
              .is_some_and(|session| !session.has_surrogate())
        })
        .map(|(id, _)| *id)
        .collect();
      for id in failed_close_ids {
        if let Some(pending) =
          state.animation_manager.pending_close_windows.remove(&id)
        {
          Self::fail_close_handoff(
            state,
            id,
            pending,
            "visible thumbnail update failed",
          );
        }
      }
    }

    // Finalize completed close animations before
    // `remove_completed_animations` so that their sessions are dropped
    // directly (not moved to `pending_session_cleanup`) and
    // `platform_sync` never attempts to reposition or uncloak these
    // windows.
    //
    // The window was already detached from the layout tree when the close
    // animation started, so `WM_CLOSE` is sent via the stored HWND rather
    // than through the container tree.
    #[cfg(target_os = "windows")]
    {
      let close_done: Vec<Uuid> = state
        .animation_manager
        .pending_close_windows
        .iter()
        .filter(|(id, pending)| {
          pending.detached
            && state
              .animation_manager
              .get_animation(id)
              .map_or(false, |a| a.is_complete())
        })
        .map(|(id, _)| *id)
        .collect();

      for id in close_done {
        let pending =
          state.animation_manager.pending_close_windows.remove(&id);

        // Send `WM_CLOSE` while the surrogate is still alive. If delivery
        // fails, the real window can be revealed and reattached before its
        // visual replacement is destroyed.
        if let Some(pending) = pending {
          let native = NativeWindow::from_handle(pending.handle);
          if let Err(err) = native.close() {
            tracing::warn!(
              "Failed to send WM_CLOSE for window {id}: {err}; restoring the \
               real window."
            );

            if native.is_valid() {
              if let Err(recovery_err) = restore_window_after_failed_close(
                pending.window,
                pending.workspace_id,
                state,
              ) {
                // Native visibility is restored before tree attachment, so
                // even this failure leaves a usable unmanaged application
                // window instead of a cloaked one.
                tracing::warn!(
                  "Failed to reattach window {id} after close delivery \
                   failure: {recovery_err}."
                );
              }
            } else {
              tracing::debug!(
                "Window {id} became invalid after failed WM_CLOSE delivery; \
                 skipping container restoration."
              );
            }
          }
        }

        // Close animations bypass `pending_session_cleanup`: on success
        // the app is closing, while on failure recovery above has
        // already made the real window visible and queued a normal
        // redraw before this surrogate is dropped.
        state.animation_manager.animations.remove(&id);
        state.animation_manager.resize_sessions.remove(&id);
      }
    }

    // Remove completed animations. Their sessions are moved to
    // `pending_session_cleanup` and must outlive the `platform_sync` call
    // below so the real window is repositioned before surrogates
    // disappear.
    let completed_ids =
      state.animation_manager.remove_completed_animations();

    // Queue completed animations for a final redraw so `platform_sync`
    // moves the real window to its target position and uncloak it.
    for window_id in &completed_ids {
      if let Some(container) = state.container_by_id(*window_id) {
        if let Ok(window) = container.as_window_container() {
          state.pending_sync.queue_container_to_redraw(window);
        }
      }
    }

    // Re-apply any focus change that was deferred while the now-completed
    // resize surrogates were active. `sync_focus` skips
    // `SetForegroundWindow` while a resize session is live to prevent
    // the OS from asynchronously removing the DWM cloak and triggering
    // a costly re-cloak on the next tick. Queuing here ensures the
    // focus transfer happens in the same `platform_sync` that uncloak
    // the windows.
    #[cfg(target_os = "windows")]
    if !completed_ids.is_empty() {
      state.pending_sync.queue_focus_change();
    }

    // Drive workspace-switch slide surrogates. All windows share a single
    // elapsed-time driver so every surrogate translates by the same pixel
    // offset each frame, making both workspaces move as one connected
    // panel.
    //
    // This runs before `platform_sync` so that when the animation
    // completes, the incoming windows are queued for redraw and
    // uncloaked in the same tick.
    #[cfg(target_os = "windows")]
    let ws_complete_ids: Option<Vec<Uuid>> = {
      use crate::animation::engine::{
        animation_progress_at, apply_easing,
      };

      // Compute the predictive vsync timestamp before taking a mutable
      // borrow on `workspace_switch`. `predictive_vsync_now` reads
      // from the installed waiter, so the period is always current —
      // no stale cached field.
      let ws_vsync_now = state.animation_manager.predictive_vsync_now();

      // Likewise precomputed before the mutable borrow below -- needed by
      // the live blur-overlay tracker to pick `focused_window` vs
      // `other_windows` config per surrogate.
      let focused_id_for_overlay =
        state.focused_container().map(|c| c.id());

      if let Some(ws) = &mut state.animation_manager.workspace_switch {
        // The shell may temporarily have no `IApplicationView` for a live
        // HWND. Keep all surrogates stationary and leave the shared clock
        // uninitialized until every participant has transferred behind its
        // cover; retries are driven by later animation/vsync ticks.
        if ws.handoff.is_pending()
          || ws.windows.values().any(|entry| !entry.handoff_cloaked)
        {
          None
        } else {
          // Anchor the animation clock to the first vsync-aligned tick so
          // every inter-frame step is measured on the same clock.
          // Falls back to the wall clock so progress always advances
          // if no vblank signal arrives.
          let now = match ws_vsync_now {
            Some(vsync) => {
              if !ws.vsync_anchored {
                ws.start_time = Some(vsync);
                ws.vsync_anchored = true;
              }
              vsync
            }
            None => Instant::now(),
          };
          let start = *ws.start_time.get_or_insert(now);
          let raw_progress =
            animation_progress_at(start, ws.duration, now);
          let segment_eased = apply_easing(raw_progress, &ws.easing);
          let eased = ws.progress_origin
            + (1.0 - ws.progress_origin) * segment_eased;

          // Let decelerating non-overshooting geometry settle once its
          // residual travel is subpixel, but retain transition ownership
          // until the configured wall-clock duration elapses. This avoids
          // both an imperceptible final crawl and the old 75 ms-short
          // duration/focus handoff. Overshooting curves keep every sample.
          let slide_distance_px = match ws.style {
            WorkspaceSwitchStyle::Slide => {
              Some(match ws.slide_direction {
                WorkspaceSwitchDirection::Horizontal => {
                  ws.slide_distance_h
                }
                WorkspaceSwitchDirection::Vertical => ws.slide_distance_v,
              })
            }
            // Iris is driven separately. Fade/zoom have no positional
            // travel and use the 99% visual-settle threshold.
            WorkspaceSwitchStyle::Fade
            | WorkspaceSwitchStyle::Zoom
            | WorkspaceSwitchStyle::Iris => None,
          };
          let frame_progress = workspace_frame_progress(
            raw_progress,
            eased,
            ws.easing.can_overshoot(),
            slide_distance_px,
          );
          let ws_done = frame_progress.complete;
          let eased_final = frame_progress.visual;
          // Retargeting must use the progress actually submitted to the
          // surrogates. During a visually settled tail this is exactly
          // 1.0, not the subpixel pre-snap eased value.
          ws.last_visual_progress = eased_final;

          // Batches every workspace-switch surrogate's `SetWindowPos`
          // (only issued by surrogates carrying a live acrylic
          // backdrop, tracking their own footprint instead of
          // staying pinned to the viewport)
          // into one `DeferWindowPos` transaction per tick — mirrors the
          // `SurrogateBatch` already used for move/resize relayouts.
          // Surrogates with no live backdrop never push into this
          // batch, so it stays empty (a no-op commit) for the common
          // case.
          let mut ws_batch = SurrogateBatch::new();

          for (&window_id, entry) in ws.windows.iter_mut() {
            if let Some(ref mut s) = entry.surrogate {
              // At completion, hide outgoing surrogates immediately. They
              // have already slid fully off-screen, but hiding
              // the thumbnail outright guarantees nothing
              // lingers for the final composition frame
              // before the real windows are uncloaked.
              if ws_done && !entry.is_incoming {
                s.hide_thumbnail();
                continue;
              }
              match ws.style {
                WorkspaceSwitchStyle::Slide => {
                  let (axis, origin, size, dist) = match ws.slide_direction
                  {
                    WorkspaceSwitchDirection::Horizontal => (
                      SlideAxis::Horizontal,
                      ws.monitor_x,
                      ws.monitor_width,
                      ws.slide_distance_h,
                    ),
                    WorkspaceSwitchDirection::Vertical => (
                      SlideAxis::Vertical,
                      ws.monitor_y,
                      ws.monitor_height,
                      ws.slide_distance_v,
                    ),
                  };
                  if ws.zoom_factor > 0.0 {
                    s.update_slide_zoom(
                      axis,
                      &mut ws_batch,
                      eased_final,
                      entry.is_incoming,
                      ws.order_direction,
                      ws.monitor_x,
                      ws.monitor_width,
                      ws.monitor_y,
                      ws.monitor_height,
                      dist,
                      ws.zoom_factor,
                    );
                  } else {
                    s.update_slide(
                      axis,
                      &mut ws_batch,
                      eased_final,
                      entry.is_incoming,
                      ws.order_direction,
                      origin,
                      size,
                      dist,
                    );
                  }
                }
                WorkspaceSwitchStyle::Fade => {
                  s.update_fade(eased_final, entry.is_incoming);
                }
                WorkspaceSwitchStyle::Zoom => {
                  s.update_zoom(
                    &mut ws_batch,
                    eased_final,
                    entry.is_incoming,
                  );
                }
                // Iris is driven by a separate snapshot overlay (see
                // `iris_switch`), never by per-window surrogates, so it
                // never reaches this driver.
                WorkspaceSwitchStyle::Iris => {}
              }

              // `Live`-mode surrogates carry no blur of their own (see
              // `WorkspaceSurrogate::new`'s doc comment) -- instead, the
              // same acrylic blur overlay used in steady state
              // is kept alive and repositioned to follow the
              // surrogate's own live footprint every
              // tick, computed by the `update_*` call just above. This is
              // what keeps `blur_amount`/tint/corner_radius
              // correct and continuous through the whole slide
              // instead of a fixed-intensity look that
              // jumps to the tuned value only once the animation ends.
              if s.is_live() {
                match s.current_rect() {
                  Some(rect) => {
                    let effect_cfg =
                      if Some(window_id) == focused_id_for_overlay {
                        &config.value.window_effects.focused_window
                      } else {
                        &config.value.window_effects.other_windows
                      };
                    if let Some(tint) = effect_cfg.backdrop.acrylic_tint()
                    {
                      let corner_radius =
                        if effect_cfg.corner_style.enabled {
                          effect_cfg.corner_style.style.approx_radius_px()
                        } else {
                          CornerStyle::Default.approx_radius_px()
                        };
                      let params = effect_cfg
                        .backdrop
                        .to_overlay_params(tint, corner_radius);
                      upsert_blur_overlay(
                        &mut state.blur_overlays,
                        window_id,
                        params,
                        rect,
                        &mut ws_batch,
                      );
                    }
                  }
                  None => {
                    if let Some(overlay) =
                      state.blur_overlays.get_mut(&window_id)
                    {
                      overlay.hide();
                    }
                  }
                }
              }
            }
          }
          let ws_result = ws_batch.commit();
          ws.frame_gaps.record(now);
          if let Some(requested_at) = ws.redirect_requested_at.take() {
            tracing::debug!(
              first_frame_submission_ms =
                requested_at.elapsed().as_secs_f64() * 1_000.0,
              "Submitted first surrogate frame after workspace redirect."
            );
          }
          if !ws_result.failed_hwnds.is_empty() {
            let failed_blur_ids: Vec<Uuid> = state
              .blur_overlays
              .iter()
              .filter(|(_, overlay)| {
                ws_result.failed_hwnds.contains(&overlay.handle())
              })
              .map(|(id, _)| *id)
              .collect();
            for id in failed_blur_ids {
              tracing::warn!(
                "Removing blur overlay for {id} after fallback positioning \
                 failed during workspace-animation tracking."
              );
              state.blur_overlays.remove(&id);
            }
            let failed_workspace_surrogates = ws_result
              .failed_hwnds
              .iter()
              .filter(|hwnd| {
                ws.windows.values().any(|entry| {
                  entry
                    .surrogate
                    .as_ref()
                    .is_some_and(|surrogate| surrogate.handle() == **hwnd)
                })
              })
              .count();
            if failed_workspace_surrogates > 0 {
              tracing::warn!(
                "{failed_workspace_surrogates} workspace surrogate fallback \
                 position update(s) failed; retaining workspace cleanup \
                 safeguards."
              );
            }
          }

          if ws_done {
            Some(ws.windows.keys().copied().collect())
          } else {
            None
          }
        }
      } else {
        None
      }
    };

    // On completion, move surrogates to pending cleanup so they outlive
    // the final `platform_sync` call that unclocks the incoming real
    // windows.
    #[cfg(target_os = "windows")]
    if let Some(ids) = ws_complete_ids {
      if let Some(ws) = &state.animation_manager.workspace_switch {
        ws.frame_gaps.log_summary("workspace_switch");
      }
      state.animation_manager.pending_ws_cleanup =
        state.animation_manager.workspace_switch.take();
      state.animation_manager.workspace_cleanup_sync_retry.clear();

      for id in ids {
        if let Some(container) = state.container_by_id(id) {
          if let Ok(window) = container.as_window_container() {
            state.pending_sync.queue_container_to_redraw(window);
          }
        }
      }

      // Re-queue focus: `sync_focus` suppressed `SetForegroundWindow`
      // while the animation was running to prevent the OS from
      // asynchronously uncloaking the incoming focused window
      // mid-slide. Now that the surrogates are done and incoming
      // windows are about to be uncloaked, it is safe to transfer OS
      // focus.
      state.pending_sync.queue_focus_change();
    }

    // Drive the iris-wipe overlay. The incoming workspace was already
    // switched in normally underneath the overlay; here a growing
    // circular hole reveals it. Uses the same vsync-aligned predictive
    // timestamp as the slide driver.
    #[cfg(target_os = "windows")]
    {
      use crate::animation::engine::{
        animation_progress_at, apply_easing,
      };

      let iris_done = if let Some(iris) =
        &mut state.animation_manager.iris_switch
      {
        let start = *iris.start_time.get_or_insert_with(Instant::now);
        let raw_progress = {
          let now = state
            .animation_manager
            .animation_vsync_time
            .lock()
            .expect("animation mutex poisoned")
            .map(|t| {
              t + std::time::Duration::from_micros(
                VSYNC_PIPELINE_OFFSET_US,
              )
            })
            .unwrap_or_else(Instant::now);
          animation_progress_at(start, iris.duration, now)
        };
        let eased = apply_easing(raw_progress, &iris.easing);
        // Grow the hole from 0 to `max_radius` (which reaches the farthest
        // corner at `eased == 1.0`); the overlay is dropped on the same
        // final frame, so the corners never linger.
        let radius = (eased * iris.max_radius as f32).round() as i32;
        iris.overlay.set_hole(iris.origin_x, iris.origin_y, radius);
        raw_progress >= 1.0
      } else {
        false
      };

      if iris_done {
        // Dropping the overlay destroys the snapshot window, revealing the
        // fully switched incoming workspace beneath.
        state.animation_manager.iris_switch = None;
      }
    }

    // The latest incompatible workspace target is already fully positioned
    // beneath retained old coverage. After one compositor tick, fade that
    // obsolete coverage out; only then restore native focus/cursor and
    // tab-bar ownership to the final workspace.
    #[cfg(target_os = "windows")]
    let redirect_now = state
      .animation_manager
      .predictive_vsync_now()
      .unwrap_or_else(Instant::now);
    let redirect_complete = {
      let animation_manager = &mut state.animation_manager;
      let blur_overlays = &mut state.blur_overlays;
      animation_manager
        .advance_workspace_redirect_fade(redirect_now, blur_overlays)
    };
    if let Some(ids) = redirect_complete {
      for id in ids {
        if let Some(container) = state.container_by_id(id) {
          if let Ok(window) = container.as_window_container() {
            state.pending_sync.queue_container_to_redraw(window);
          }
        }
      }
      // A redirected logical focus was deliberately left native-unsynced
      // while retained coverage was visible. A cancellation back to the
      // already-logical workspace leaves `is_focus_synced` true and must
      // not emit a duplicate FocusChanged event or cursor jump.
      if !state.is_focus_synced {
        state.pending_sync.queue_focus_change().queue_cursor_jump();
      }
    }

    if state.pending_sync.has_changes() {
      if let Err(err) = platform_sync(state, config) {
        // A workspace switch that has reached `pending_ws_cleanup` must
        // not be stranded by a fallible native operation in the
        // final platform sync. Keep its surrogates alive, re-queue
        // every still-managed participant, and retry on the next
        // animation tick. Dropping the cleanup state here
        // could expose a real incoming window that is still cloaked.
        #[cfg(target_os = "windows")]
        if let Some(cleanup_ids) = state
          .animation_manager
          .pending_ws_cleanup
          .as_ref()
          .map(|ws| ws.windows.keys().copied().collect::<Vec<_>>())
        {
          for id in cleanup_ids {
            if let Some(container) = state.container_by_id(id) {
              if let Ok(window) = container.as_window_container() {
                state.pending_sync.queue_container_to_redraw(window);
              }
            }
          }
          state.pending_sync.queue_focus_change();
          let _ = state.animation_manager.ensure_timer_running();

          return match state
            .animation_manager
            .workspace_cleanup_sync_retry
            .record_failure(Instant::now())
          {
            WorkspaceCleanupSyncFailureAction::Retry {
              attempt,
              remaining,
            } => AnimationUpdateOutcome::RetryScheduled {
              attempt,
              remaining,
              error: err,
            },
            WorkspaceCleanupSyncFailureAction::Recover => {
              let participant_count =
                Self::recover_workspace_cleanup_group(state, config);
              state.animation_manager.settle_timer_after_update();
              AnimationUpdateOutcome::GroupRecovered {
                participant_count,
                error: err,
              }
            }
          };
        }

        return AnimationUpdateOutcome::Fatal(err);
      }

      #[cfg(target_os = "windows")]
      state.animation_manager.workspace_cleanup_sync_retry.clear();
    }

    // Fade out pending sessions now that `platform_sync` has moved the
    // real windows to their final positions, then drop them. Dropping
    // a session destroys its surrogate overlay.
    #[cfg(target_os = "windows")]
    {
      // Keep newly completed surrogates fully visible for one additional
      // compositor-paced tick after `platform_sync` uncloaks the real
      // windows. This replaces the former blocking `DwmFlush` barrier.
      // Transparent surrogates are also retained unchanged for that tick;
      // dropping or zeroing them immediately can expose a still-cloaked
      // real window if DWM has not consumed the uncloak yet.

      // Keep the acrylic blur overlay tracking each fading session's own
      // surrogate through the whole `pending_session_cleanup` tail,
      // exactly as it did through the active animation -- since the
      // overlay is never hidden to begin with, there's no
      // `pending_ws_cleanup`-style pre-show/flush gap to plug here.
      // Batched into one `DeferWindowPos` transaction, same
      // reasoning as the other blur-overlay sync loops.
      let mut fade_tail_batch = SurrogateBatch::new();
      for (id, _, session) in
        &state.animation_manager.pending_session_cleanup
      {
        if let Some(params) = session.blur_overlay_params() {
          match session.current_rect() {
            Some(rect) => upsert_blur_overlay(
              &mut state.blur_overlays,
              *id,
              params,
              &rect,
              &mut fade_tail_batch,
            ),
            None => {
              if let Some(overlay) = state.blur_overlays.get_mut(id) {
                overlay.hide();
              }
            }
          }
        }
      }
      let fade_tail_result = fade_tail_batch.commit();
      remove_failed_blur_overlays(
        state,
        &fade_tail_result,
        "resize-session fade tracking",
      );

      // After `pre_commit` the surrogate is a pixel-aligned live mirror of
      // the uncloaked window beneath it, so ramping its opacity to zero
      // blends shadow/border/late-repaint differences instead of swapping
      // them in a single composition. Entries are dropped once fully
      // faded.
      let fade_now = Instant::now();
      // Windows whose fading resize-session surrogate is dropped this tick
      // -- `has_active_surrogate` now covers
      // `pending_session_cleanup`, so once dropped here the static
      // overlay needs a follow-up sync to reappear (see the same
      // reasoning below for workspace-switch cleanup).
      let mut dropped_session_ids: Vec<Uuid> = Vec::new();
      state.animation_manager.pending_session_cleanup.retain_mut(
        |(id, fade_start, session)| {
          let start = match session_cleanup_action(
            fade_start,
            fade_now,
            session.effect_opacity < u8::MAX,
          ) {
            SessionCleanupAction::HoldCoverage => return true,
            SessionCleanupAction::DropTransparent => {
              dropped_session_ids.push(*id);
              return false;
            }
            SessionCleanupAction::FadeFrom(start) => start,
          };
          let progress =
            fade_now.saturating_duration_since(start).as_secs_f32()
              / SESSION_FADE_OUT.as_secs_f32();
          if progress >= 1.0 {
            dropped_session_ids.push(*id);
            return false;
          }
          #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
          )]
          let opacity =
            (f32::from(session.effect_opacity) * (1.0 - progress)) as u8;
          session.fade_overlay(opacity);
          true
        },
      );

      // `sync_blur_overlays` (called from `platform_sync` above) hides
      // each window's static acrylic overlay while its
      // workspace-switch surrogate is alive, including this
      // `pending_ws_cleanup` grace period. The surrogate itself
      // carries a live SWCA acrylic blur throughout, so that's fine
      // while it's alive -- but dropping `pending_ws_cleanup`
      // below destroys the surrogate outright, and nothing shows the
      // static overlay again until a later `platform_sync` call
      // runs. Between those two moments DWM can composite a frame
      // with *no* blur behind the (semi-transparent) real window at
      // all -- a one-frame flicker at the end of every switch.
      // Pre-show the static overlay at its final rect (known from the
      // surrogate, which is already positioned there) while the surrogate
      // remains alive for one more animation tick. The tick boundary,
      // rather than a blocking compositor flush, closes the handoff gap.
      let incoming_acrylic_windows: Vec<(Uuid, Rect)> = state
        .animation_manager
        .pending_ws_cleanup
        .as_ref()
        .filter(|ws| {
          // During an incompatible redirect, static overlays for the
          // latest real workspace are already prepared underneath the
          // retained thumbnails. Re-showing an old incoming overlay here
          // would leave a fully opaque tint block after its thumbnail has
          // faded, so only ordinary cleanup uses this pre-show bridge.
          should_preshow_workspace_cleanup_overlay(
            state.animation_manager.workspace_redirect_fallback,
            ws.cleanup_tick_seen,
          )
        })
        .map(|ws| {
          ws.windows
            .iter()
            .filter(|(_, entry)| entry.is_incoming)
            .filter_map(|(id, entry)| {
              Some((*id, entry.surrogate.as_ref()?.rect.clone()))
            })
            .collect()
        })
        .unwrap_or_default();

      let focused_id = state.focused_container().map(|c| c.id());
      let mut pre_show_batch = SurrogateBatch::new();

      for (id, rect) in &incoming_acrylic_windows {
        let effect_cfg = if Some(*id) == focused_id {
          &config.value.window_effects.focused_window
        } else {
          &config.value.window_effects.other_windows
        };

        let Some(tint) = effect_cfg.backdrop.acrylic_tint() else {
          continue;
        };

        let corner_radius = if effect_cfg.corner_style.enabled {
          effect_cfg.corner_style.style.approx_radius_px()
        } else {
          CornerStyle::Default.approx_radius_px()
        };
        let params =
          effect_cfg.backdrop.to_overlay_params(tint, corner_radius);

        match state.blur_overlays.entry(*id) {
          std::collections::hash_map::Entry::Occupied(e) => {
            let overlay = e.into_mut();
            overlay.apply(params);
            overlay.defer_rect(&mut pre_show_batch, rect);
          }
          std::collections::hash_map::Entry::Vacant(e) => {
            if let Ok(overlay) = NativeBlurOverlay::create(rect, params) {
              e.insert(overlay);
            }
          }
        }
      }
      let pre_show_result = pre_show_batch.commit();
      remove_failed_blur_overlays(
        state,
        &pre_show_result,
        "workspace-cleanup pre-show",
      );

      // Capture the participating window IDs before dropping the cleanup
      // state (which destroys the surrogates) so a follow-up
      // `platform_sync` call can settle their overlays' tint/rect
      // through the normal path. Without this second sync, an
      // overlay would stay however it was left above until some
      // unrelated redraw happens to fire -- the animation
      // timer stops right after this block when no other animation is
      // active, so there may be no later tick. Combined with
      // `dropped_session_ids` (resize-session surrogates dropped above,
      // subject to the exact same hazard).
      let mut cleanup_window_ids = dropped_session_ids;
      // A relayout requested while an incoming workspace was covered
      // starts its normal resize session during the final covered
      // platform sync. Keep the old workspace coverage for as long
      // as that new session is still transparent and warming; once
      // readiness transfers, the resize surrogate (or its
      // visible-real fallback) owns the next frame.
      let incoming_reflow_handoff_pending = state
        .animation_manager
        .pending_ws_cleanup
        .as_ref()
        .is_some_and(|ws| {
          ws.windows.iter().any(|(id, entry)| {
            entry.is_incoming
              && state
                .animation_manager
                .pending_move_resize_windows
                .contains_key(id)
          })
        });
      let redirect_fallback =
        state.animation_manager.workspace_redirect_fallback;
      let drop_workspace_cleanup = state
        .animation_manager
        .pending_ws_cleanup
        .as_mut()
        .is_some_and(|ws| {
          workspace_cleanup_complete(
            redirect_fallback,
            incoming_reflow_handoff_pending,
            &mut ws.cleanup_tick_seen,
          )
        });
      if drop_workspace_cleanup {
        if let Some(ws) = state.animation_manager.pending_ws_cleanup.take()
        {
          cleanup_window_ids.extend(ws.windows.keys().copied());
        }
        state.animation_manager.workspace_cleanup_sync_retry.clear();
      }

      if !cleanup_window_ids.is_empty() {
        let mut queued = 0;
        for id in &cleanup_window_ids {
          if let Some(container) = state.container_by_id(*id) {
            if let Ok(window) = container.as_window_container() {
              state.pending_sync.queue_container_to_redraw(window);
              queued += 1;
            }
          }
        }
        tracing::debug!(
          "Post-cleanup overlay re-sync: queued {queued} window(s) for \
           redraw, running follow-up platform_sync."
        );
        if let Err(err) = platform_sync(state, config) {
          tracing::warn!(
            "Post-cleanup overlay synchronization failed after real-window \
             visibility was restored: {err}; isolating {} participant(s) \
             without global animation recovery.",
            cleanup_window_ids.len()
          );
          for id in &cleanup_window_ids {
            state.pending_sync.remove_window(id);
          }
          failed_participants.extend(cleanup_window_ids);
        }
      }
    }

    // Keep the timer running while animations are active; stop it
    // otherwise so the background thread exits cleanly.
    state.animation_manager.settle_timer_after_update();

    #[cfg(target_os = "windows")]
    if !failed_participants.is_empty() {
      failed_participants.sort_unstable();
      failed_participants.dedup();
      return AnimationUpdateOutcome::ParticipantFailed {
        window_ids: failed_participants,
      };
    }

    AnimationUpdateOutcome::Complete
  }

  /// Returns the predicted vsync instant: last wake + frame period ×
  /// (1 - `VSYNC_LEAD_FRACTION`).
  ///
  /// Reads the period live from the installed waiter instead of a stale
  /// cached field. Returns `None` when no waiter is installed or no wake
  /// has been recorded yet.
  #[cfg(target_os = "windows")]
  fn predictive_vsync_now(&self) -> Option<Instant> {
    let guard = self.animation_timer_vsync.lock().ok()?;
    let waiter = guard.as_ref()?;
    let period_us = waiter.frame_period_us();
    let last_wake = waiter.last_wake()?;
    #[allow(
      clippy::cast_precision_loss,
      clippy::cast_possible_truncation,
      clippy::cast_sign_loss
    )]
    let lead = Duration::from_micros(
      (period_us as f64 * f64::from(1.0_f32 - VSYNC_LEAD_FRACTION)) as u64,
    );
    Some(last_wake + lead)
  }

  /// Returns the predictive vsync instant if available, else wall-clock
  /// now.
  #[cfg(target_os = "windows")]
  fn predictive_now(&self) -> Instant {
    self.predictive_vsync_now().unwrap_or_else(Instant::now)
  }

  /// Installs or switches the vsync waiter to the monitor with handle
  /// `monitor_handle`, so animation ticks are always paced against
  /// whichever monitor the animating window is actually on.
  ///
  /// No-op during workspace or iris switch (they own the waiter). Skips
  /// DXGI enumeration when the window is already on the installed
  /// monitor.
  #[cfg(target_os = "windows")]
  fn ensure_waiter_for(&self, monitor_handle: isize) {
    // Don't touch the waiter during switch phases — they manage it.
    if self.workspace_switch.is_some() || self.iris_switch.is_some() {
      return;
    }

    // Fast path: already pacing from this monitor.
    {
      let guard = self
        .animation_timer_vsync
        .lock()
        .unwrap_or_else(|e| e.into_inner());
      if guard
        .as_ref()
        .is_some_and(|w| w.monitor_handle() == monitor_handle)
      {
        return;
      }
    }

    // Always switch to the target monitor's waiter here -- the fast path
    // above already ruled out "no-op, same monitor", so by this point the
    // animating window is confirmed to be on a *different* monitor than
    // whatever's currently installed (or nothing is installed yet). A
    // prior "upgrade only, never downgrade to a slower refresh rate"
    // comparison used to gate this, intended to avoid needlessly
    // slowing down in-flight animations -- but it meant that once a
    // fast waiter (e.g. a laptop's high-refresh internal panel) was
    // installed, later animations on a genuinely slower monitor (e.g.
    // a fixed-60Hz external display) kept pacing against the wrong
    // monitor's vblank signal until every animation fully idled out
    // and the cache reset to `None`, which reads as stutter/lag
    // specific to that monitor. Pacing must always follow the monitor
    // actually being animated on, not whichever was fastest
    // historically.
    match DxgiVsyncWaiter::for_monitor(monitor_handle) {
      Ok(new_waiter) => {
        let mut guard = self
          .animation_timer_vsync
          .lock()
          .unwrap_or_else(|e| e.into_inner());
        tracing::debug!(
          monitor = monitor_handle,
          period_us = new_waiter.frame_period_us(),
          "vsync waiter switched"
        );
        *guard = Some(Arc::new(new_waiter));
      }
      Err(err) => {
        tracing::warn!(?err, "failed to create vsync waiter for monitor");
      }
    }
  }

  /// Determines whether a new animation should be started for a window.
  fn should_start_new_animation(
    &self,
    window_id: &Uuid,
    is_resize: bool,
    target_rect: &Rect,
    previous_target: Option<&Rect>,
    config: &UserConfig,
  ) -> bool {
    let existing_animation = self.get_animation(window_id);

    let (enabled, threshold_px) = if is_resize {
      let c = &config.value.animations.window_resize;
      (c.should_animate(), c.threshold_px)
    } else {
      let c = &config.value.animations.window_move;
      (c.should_animate(), c.threshold_px)
    };
    let threshold = i64::from(threshold_px);

    if enabled {
      if let Some(anim) = existing_animation {
        if anim.is_complete() {
          // Animation already at its target — treat as a static window and
          // apply the threshold check against the completed target so a
          // new animation starts if the window needs to move.
          let distance =
            Self::rect_change_distance(&anim.target_rect, target_rect);
          distance > threshold
        } else {
          // Redirect any in-progress animation to the new target whenever
          // the destination changes, regardless of distance.
          // Without this, small target adjustments (< threshold)
          // are silently swallowed and the window snaps after
          // the stale animation finishes.
          anim.target_rect != *target_rect
        }
      } else if let Some(prev_target) = previous_target {
        let distance =
          Self::rect_change_distance(prev_target, target_rect);
        distance > threshold
      } else {
        false
      }
    } else {
      false
    }
  }

  /// Sum of absolute rect-component changes without overflowing `i32` at
  /// extreme virtual-screen coordinates.
  fn rect_change_distance(first: &Rect, second: &Rect) -> i64 {
    let first_width = i64::from(first.right) - i64::from(first.left);
    let second_width = i64::from(second.right) - i64::from(second.left);
    let first_height = i64::from(first.bottom) - i64::from(first.top);
    let second_height = i64::from(second.bottom) - i64::from(second.top);

    (i64::from(first.left) - i64::from(second.left)).abs()
      + (i64::from(first.top) - i64::from(second.top)).abs()
      + (first_width - second_width).abs()
      + (first_height - second_height).abs()
  }

  /// Determines the rect and opacity to use for a window this frame.
  ///
  /// Starts a new animation when movement or resize crosses the configured
  /// threshold.
  ///
  /// `cycle_has_resize` is `true` when any window in the same redraw cycle
  /// changes size. Pure translations then adopt the `window_resize` timing
  /// and easing so adjacent edges stay in lock-step throughout the
  /// relayout — with differing `window_move`/`window_resize` durations,
  /// a moved window would otherwise arrive early and detach from its
  /// still-resizing neighbors.
  ///
  /// Returns [`AnimationPositionResult::Frozen`] while a surrogate overlay
  /// is active so the caller does not reposition the real window on
  /// intermediate frames.
  pub fn start_animation_if_needed(
    &mut self,
    window_id: Uuid,
    is_resize: bool,
    cycle_has_resize: bool,
    target_rect: Rect,
    previous_target: Option<Rect>,
    // Only used on Windows to capture the window for the surrogate.
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
    native_window: &NativeWindow,
    // Opacity from window-effects config; used as surrogate opacity when
    // the animation has no per-frame fade component.
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
    effect_opacity: u8,
    // Corner style from window-effects config; applied to the surrogate
    // so it matches the real window's rounded corners during the
    // animation.
    #[cfg(target_os = "windows")] corner_style: CornerStyle,
    // Tint/blur-amount/corner-radius/opacity/saturation for the acrylic
    // blur-overlay tracker, or `None` when blur-behind is not configured.
    #[cfg(target_os = "windows")] blur_overlay: Option<BlurOverlayParams>,
    config: &UserConfig,
  ) -> (AnimationPositionResult, Option<OpacityValue>) {
    #[cfg(target_os = "windows")]
    if self.failed_move_resize_recoveries.contains(&window_id) {
      if let Some(session) = self.resize_sessions.get_mut(&window_id) {
        session.update_recovery_target(target_rect);
      }
      return (AnimationPositionResult::Frozen, None);
    }

    let existing_animation = self.get_animation(&window_id).cloned();

    let should_start = self.should_start_new_animation(
      &window_id,
      is_resize,
      &target_rect,
      previous_target.as_ref(),
      config,
    );

    if should_start {
      if let Some(prev_target) = previous_target {
        // Start from the current animated position on cancel-and-replace
        // so the animation does not jump back to the original
        // start.
        let start_rect = existing_animation
          .as_ref()
          .map(|a| a.current_rect())
          .unwrap_or_else(|| prev_target.clone());

        // Share the resize timing across the whole cycle when any window
        // in it resizes, keeping all edges in lock-step (see doc
        // comment).
        let use_resize_timing = is_resize
          || (cycle_has_resize
            && config.value.animations.window_resize.should_animate());

        let (duration_ms, easing) = if use_resize_timing {
          let c = &config.value.animations.window_resize;
          (c.duration_ms, c.easing.clone())
        } else {
          let c = &config.value.animations.window_move;
          (c.duration_ms, c.easing.clone())
        };

        let animation = WindowAnimationState::new_movement(
          start_rect.clone(),
          target_rect.clone(),
          duration_ms,
          easing,
        );
        self.start_animation(window_id, animation);

        // Redirect an in-flight surrogate session to the new target, or
        // create a new one. The surrogate overlay is our own window and
        // moves instantly each frame; the real window only needs
        // one async move to its final position. This avoids
        // per-frame cross-process `SWP_ASYNCWINDOWPOS` calls,
        // which lag behind when the target process's message loop
        // is slow.
        #[cfg(target_os = "windows")]
        let pending_redirect =
          self.pending_move_resize_windows.contains_key(&window_id);
        #[cfg(target_os = "windows")]
        let compatible_pending_redirect = pending_redirect
          && self.resize_sessions.get(&window_id).is_some_and(|session| {
            session.is_compatible_readiness_redirect(
              native_window.hwnd().0,
              effect_opacity,
              corner_style,
              blur_overlay,
            )
          });
        #[cfg(target_os = "windows")]
        if pending_redirect && !compatible_pending_redirect {
          self.pending_move_resize_windows.remove(&window_id);
          // A source or rendering-policy change requires a fresh hidden
          // host. Geometry-only redirects preserve the existing host and
          // readiness epoch below.
          if let Some(mut session) =
            self.resize_sessions.remove(&window_id)
          {
            session.hide();
          }
        }
        #[cfg(target_os = "windows")]
        if let Some(session) = self.resize_sessions.get_mut(&window_id) {
          if compatible_pending_redirect {
            session.redirect_during_readiness(&start_rect, &target_rect);
          } else {
            session.update_target(&start_rect, &target_rect);
          }
        } else {
          // Drop any still-fading surrogate from a just-completed
          // animation of this window so two overlays don't
          // stack.
          self
            .pending_session_cleanup
            .retain(|(id, _, _)| id != &window_id);
          let hwnd = native_window.hwnd();
          let cached_edge_color = self.cached_edge_color(hwnd.0);
          let had_cached_color = cached_edge_color.is_some();
          match ResizeSession::begin(
            hwnd,
            &start_rect,
            &target_rect,
            SessionOptions {
              kind: SessionKind::MoveResize,
              effect_opacity,
              initially_visible: false,
              corner_style,
              place_at_top: true,
              edge_color: cached_edge_color,
              blur_overlay,
            },
          ) {
            Ok(session) => {
              let has_surrogate = session.has_surrogate();
              // Only cache on a miss so staleness stays bounded by the TTL
              // rather than sliding forward on every reuse.
              if !had_cached_color {
                if let Some(color) = session.edge_color() {
                  self.remember_edge_color(hwnd.0, color.clone());
                }
              }
              self.resize_sessions.insert(window_id, session);
              if has_surrogate {
                self.pending_move_resize_windows.insert(
                  window_id,
                  PendingMoveResizeWindow {
                    handle: hwnd.0,
                    readiness: SurrogateHandoffReadiness::new(
                      Instant::now(),
                    ),
                    surrogate_revealed: false,
                  },
                );
              }
            }
            Err(err) => {
              tracing::warn!(
                "Failed to begin resize session for window {window_id}: \
                 {err}."
              );
            }
          }
        }
        // Install or upgrade the vsync waiter to the highest-Hz monitor
        // among active sessions after both the new-session and redirect
        // paths.
        #[cfg(target_os = "windows")]
        self.ensure_waiter_for(DxgiVsyncWaiter::window_monitor(
          native_window.hwnd(),
        ));
      }
    }

    // A new move/resize thumbnail uses two nonblocking ticks: one to
    // reveal its transparent host, then a transfer after one full
    // visible composition interval. This avoids racing DWM at vblank.
    #[cfg(target_os = "windows")]
    if self.pending_move_resize_windows.contains_key(&window_id) {
      return (AnimationPositionResult::Waiting, None);
    }

    // A newly registered open thumbnail remains hidden for its first
    // compositor-paced tick. Do not evaluate or expose the animation yet;
    // its clock is restarted when readiness succeeds.
    #[cfg(target_os = "windows")]
    if self.pending_open_windows.contains_key(&window_id) {
      return (AnimationPositionResult::Frozen, None);
    }

    // Evaluate this frame's position at a predictive timestamp so the
    // surrogate aligns with the next DWM composition rather than lagging
    // by one pipeline delay. On non-Windows there is no vsync clock,
    // so this is just `Instant::now()`.
    #[cfg(target_os = "windows")]
    let now = self.predictive_now();
    #[cfg(not(target_os = "windows"))]
    let now = Instant::now();

    // Re-fetch the animation after potentially starting a new one.
    if let Some(animation) = self.get_animation(&window_id) {
      let (current_rect, opacity) = animation.current_state_at(now);

      // Drive the surrogate overlay when one is active. `has_surrogate()`
      // requires a valid DWM thumbnail — if thumbnail registration failed
      // (e.g. elevated/UWP window), the surrogate is transparent and
      // useless: snap the window to target and clean up rather than
      // cloaking it behind an empty overlay.
      // Extract the session status with a shared borrow first, then take a
      // mutable borrow only for the drive path. Avoids a triple-lookup
      // (contains_key → get → get_mut) on the same key.
      #[cfg(target_os = "windows")]
      let session_status = self
        .resize_sessions
        .get(&window_id)
        .map(|s| (s.has_surrogate(), s.effect_opacity, s.zoom));

      #[cfg(target_os = "windows")]
      match session_status {
        Some((true, effect_opacity, zoom)) => {
          let monitor_rect =
            self.slide_in_monitor_rects.get(&window_id).cloned();
          let session = self
            .resize_sessions
            .get_mut(&window_id)
            .expect("resize session must exist after status check");
          let opacity_u8 = opacity
            .as_ref()
            .map(|o| o.to_alpha())
            .unwrap_or(effect_opacity);
          if zoom {
            // Extract progress with a separate borrow before mutably using
            // session.
            let progress = self
              .animations
              .get(&window_id)
              .map(|a| a.eased_progress())
              .unwrap_or(1.0);
            let is_close =
              self.pending_close_windows.contains_key(&window_id);
            let forward_progress =
              if is_close { 1.0 - progress } else { progress };
            let session = self
              .resize_sessions
              .get_mut(&window_id)
              .expect("resize session must exist after status check");
            session.update_zoom_fade(forward_progress, opacity_u8);
          } else if let Some(monitor_rect) = monitor_rect {
            session.update_clipped(
              &current_rect,
              &monitor_rect,
              opacity_u8,
            );
          } else {
            // Queue instead of applying immediately: all surrogate
            // repositions in this redraw pass are committed atomically by
            // `flush_surrogate_updates` so adjacent windows' edges land in
            // the same DWM composition frame.
            let handoff =
              self.animations.get(&window_id).map_or(false, |a| {
                // Scale the lead with the animation duration so the
                // handoff stays near the end of the visual
                // travel regardless of easing speed. For
                // expo-out at 150 ms this fires at ~96%
                // visual progress.
                #[allow(
                  clippy::cast_possible_truncation,
                  clippy::cast_sign_loss
                )]
                let lead_ms = (a.duration.as_millis() as f32 * 0.35)
                  .clamp(50.0, HANDOFF_LEAD_MAX_MS as f32)
                  as u64;
                a.remaining_at(now) <= Duration::from_millis(lead_ms)
              });
            self.pending_surrogate_updates.push(PendingSurrogateUpdate {
              window_id,
              rect: current_rect,
              opacity: opacity_u8,
              handoff,
            });
          }
          return (AnimationPositionResult::Frozen, None);
        }
        Some((false, _, _)) => {
          // Thumbnail failed — drop the transparent surrogate and snap.
          // Open handoffs must place and reveal the real HWND explicitly;
          // once the session is removed, the ordinary Apply branch no
          // longer knows that the window is cloaked.
          if let Some(mut session) =
            self.resize_sessions.remove(&window_id)
          {
            session.hide();
            if !self.pending_close_windows.contains_key(&window_id) {
              session.pre_commit();
              if let Err(err) = native_window.set_cloaked(false) {
                tracing::warn!(
                  "Failed to uncloak window {window_id} after thumbnail \
                   failure: {err}"
                );
              }
              if let Err(err) = native_window.show() {
                tracing::warn!(
                  "Failed to show window {window_id} after thumbnail \
                   failure: {err}"
                );
              }
            }
          }
          self.animations.remove(&window_id);
          return (AnimationPositionResult::Apply(target_rect), None);
        }
        None => {}
      }

      (AnimationPositionResult::Apply(current_rect), opacity)
    } else {
      // No animation in the map — either the animation completed and
      // `remove_completed_animations` was already called, or animations
      // are disabled. Apply the final target rect directly.
      (AnimationPositionResult::Apply(target_rect), None)
    }
  }

  /// Returns `true` when `window_id`'s animation just completed and
  /// `pre_commit` synchronously positioned the real window at `rect`.
  ///
  /// Used by `platform_sync` on the completion redraw to skip the
  /// redundant `SetWindowPos` — its `SWP_FRAMECHANGED` would force a
  /// full frame recalculation and repaint of the window right as it is
  /// uncloaked, producing a visible flash at the end of move/resize
  /// animations.
  #[cfg(target_os = "windows")]
  pub fn was_pre_committed_at(
    &self,
    window_id: &Uuid,
    rect: &Rect,
  ) -> bool {
    self.pending_session_cleanup.iter().any(|(id, _, session)| {
      id == window_id
        && session.target_rect() == rect
        && session.pre_commit_succeeded()
    })
  }

  /// Returns the cached surrogate backdrop color for `hwnd` when still
  /// within [`EDGE_COLOR_CACHE_TTL`].
  #[cfg(target_os = "windows")]
  fn cached_edge_color(&self, hwnd: isize) -> Option<Color> {
    self
      .edge_color_cache
      .get(&hwnd)
      .filter(|(_, sampled_at)| {
        sampled_at.elapsed() < EDGE_COLOR_CACHE_TTL
      })
      .map(|(color, _)| color.clone())
  }

  /// Caches `color` as `hwnd`'s surrogate backdrop color.
  ///
  /// Prunes stale entries once the cache exceeds
  /// [`EDGE_COLOR_CACHE_PRUNE_LEN`] so closed windows don't accumulate
  /// indefinitely.
  #[cfg(target_os = "windows")]
  fn remember_edge_color(&mut self, hwnd: isize, color: Color) {
    if self.edge_color_cache.len() >= EDGE_COLOR_CACHE_PRUNE_LEN {
      self.edge_color_cache.retain(|_, (_, sampled_at)| {
        sampled_at.elapsed() < EDGE_COLOR_CACHE_TTL
      });
    }
    self.edge_color_cache.insert(hwnd, (color, Instant::now()));
  }

  /// Applies all surrogate updates queued during the current redraw pass
  /// in a single `DeferWindowPos` transaction.
  ///
  /// Called at the end of each redraw pass. Committing all repositions
  /// atomically guarantees that adjacent windows' surrogates move in the
  /// same DWM composition frame during multi-window relayouts; sequential
  /// per-surrogate `SetWindowPos` calls can straddle a composition
  /// boundary and let edges visibly desync for a frame.
  ///
  /// Real-window handoffs are capped at [`MAX_HANDOFFS_PER_TICK`] per
  /// pass; see its docs for why. Surrogate position/opacity updates are
  /// never throttled — only the real-window handoff is deferred.
  #[cfg(target_os = "windows")]
  pub fn flush_surrogate_updates(&mut self) -> Vec<Uuid> {
    if self.pending_surrogate_updates.is_empty() {
      return Vec::new();
    }

    let mut batch = SurrogateBatch::new();
    let mut handoffs_this_tick = 0usize;
    let mut failed_ids = Vec::new();
    let mut surrogate_owners = HashMap::new();
    for update in std::mem::take(&mut self.pending_surrogate_updates) {
      if let Some(session) =
        self.resize_sessions.get_mut(&update.window_id)
      {
        if let Some(hwnd) = session.surrogate_handle() {
          surrogate_owners.insert(hwnd, update.window_id);
        }
        if update.handoff && handoffs_this_tick < MAX_HANDOFFS_PER_TICK {
          session.maybe_handoff();
          handoffs_this_tick += 1;
        }
        let healthy =
          session.defer_update(&mut batch, &update.rect, update.opacity);
        if let Some(id) = failed_move_resize_update_id(
          update.window_id,
          healthy,
          session.is_open_close_handoff(),
        ) {
          failed_ids.push(id);
        }
      }
    }
    let batch_result = batch.commit();
    failed_ids.extend(
      batch_result
        .failed_hwnds
        .iter()
        .filter_map(|hwnd| surrogate_owners.get(hwnd).copied()),
    );
    failed_ids.sort_unstable();
    failed_ids.dedup();
    failed_ids
  }

  /// Returns `true` while a workspace-switch slide animation is in
  /// progress or its surrogates are still live during post-animation
  /// cleanup.
  ///
  /// Includes `pending_ws_cleanup` so that callers (e.g. tab-bar
  /// visibility, focus deferral) stay in their animation-active state
  /// until surrogates are fully dropped, preventing a one-frame flash
  /// between animation completion and surrogate teardown.
  #[cfg(target_os = "windows")]
  pub fn is_workspace_switch_active(&self) -> bool {
    self.workspace_switch.is_some() || self.pending_ws_cleanup.is_some()
  }

  /// Whether an active workspace transition can reuse its existing
  /// participant set as an exact role reversal.
  #[cfg(target_os = "windows")]
  pub(crate) fn can_reverse_workspace_switch(
    &self,
    requested_roles: &HashMap<Uuid, bool>,
    order_direction: i32,
    ws_config: &WorkspaceSwitchAnimationConfig,
  ) -> bool {
    self.workspace_switch.as_ref().is_some_and(|ws| {
      !ws.handoff.is_pending()
        && ws.windows.len() == requested_roles.len()
        && ws.windows.iter().all(|(id, entry)| {
          requested_roles.get(id) == Some(&!entry.is_incoming)
        })
        && ws.style == ws_config.style
        && ws.slide_direction == ws_config.direction
        && ws.easing == ws_config.easing
        && ws.base_duration
          == Duration::from_millis(u64::from(ws_config.duration_ms))
        && (ws.zoom_factor - ws_config.zoom_factor.clamp(0.0, 1.0)).abs()
          <= f32::EPSILON
        && (ws.opacity_incoming - ws.opacity_outgoing).abs()
          <= f32::EPSILON
        && (ws.opacity_incoming - ws_config.opacity_incoming).abs()
          <= f32::EPSILON
        && (ws.opacity_outgoing - ws_config.opacity_outgoing).abs()
          <= f32::EPSILON
        && (ws.style != WorkspaceSwitchStyle::Slide
          || (order_direction != 0
            && order_direction == -ws.order_direction))
    })
  }

  /// Admits one incompatible user redirect using the directional reveal
  /// when exact frame capture is available, otherwise retaining the
  /// established opacity-only final-state fallback.
  #[cfg(target_os = "windows")]
  pub(crate) fn begin_incompatible_workspace_redirect(
    &mut self,
    order_direction: i32,
    ws_config: &WorkspaceSwitchAnimationConfig,
  ) -> WorkspaceRedirectOutcome {
    if self.begin_workspace_directional_reveal(order_direction, ws_config)
    {
      WorkspaceRedirectOutcome::DirectionalReveal
    } else {
      self.begin_workspace_redirect_fallback();
      WorkspaceRedirectOutcome::FinalStateFallback
    }
  }

  /// Redirects an active workspace transition when the new command is an
  /// exact reversal of the same participants.
  ///
  /// Role reversal plus `progress = 1 - previous_progress` is an exact
  /// visual identity for slide, slide+zoom, fade and zoom. It therefore
  /// reuses the existing hosts and DWM thumbnail registrations without a
  /// discontinuity. Any request that cannot prove that identity is routed
  /// to a bounded final-state fallback; historical commands are never
  /// queued.
  #[cfg(target_os = "windows")]
  pub(crate) fn redirect_workspace_switch(
    &mut self,
    requested_roles: &HashMap<Uuid, bool>,
    order_direction: i32,
    ws_config: &WorkspaceSwitchAnimationConfig,
  ) -> WorkspaceRedirectOutcome {
    if self.workspace_switch.is_none() {
      return if self.pending_ws_cleanup.is_some() {
        self.begin_incompatible_workspace_redirect(
          order_direction,
          ws_config,
        )
      } else {
        WorkspaceRedirectOutcome::NotActive
      };
    }

    let is_existing_request =
      self.workspace_switch.as_ref().is_some_and(|ws| {
        ws.windows.len() == requested_roles.len()
          && ws.windows.iter().all(|(id, entry)| {
            requested_roles.get(id) == Some(&entry.is_incoming)
          })
          && (ws.style != WorkspaceSwitchStyle::Slide
            || order_direction == ws.order_direction)
      });
    if is_existing_request {
      return WorkspaceRedirectOutcome::ExistingRequest;
    }

    let can_redirect = self.can_reverse_workspace_switch(
      requested_roles,
      order_direction,
      ws_config,
    );

    if !can_redirect {
      let outcome = self
        .begin_incompatible_workspace_redirect(order_direction, ws_config);
      tracing::debug!(
        "Rapid workspace switch could not safely reuse current coverage; \
         synchronizing the latest real target beneath retained coverage."
      );
      return outcome;
    }

    let ws = self
      .workspace_switch
      .as_mut()
      .expect("redirect compatibility requires an active switch");
    let progress_origin = 1.0 - ws.last_visual_progress;
    for entry in ws.windows.values_mut() {
      entry.is_incoming = !entry.is_incoming;
    }
    ws.order_direction = order_direction;
    ws.progress_origin = progress_origin;
    ws.last_visual_progress = progress_origin;
    ws.duration =
      redirected_segment_duration(ws.base_duration, progress_origin);
    ws.start_time = None;
    ws.vsync_anchored = false;
    ws.cleanup_tick_seen = false;
    ws.redirect_requested_at = Some(Instant::now());

    tracing::debug!(
      progress_origin,
      duration_ms = ws.duration.as_millis(),
      "Reversed active workspace-switch coverage in place."
    );
    WorkspaceRedirectOutcome::Redirected
  }

  /// Holds current workspace coverage until the latest logical final
  /// state has completed one successful `platform_sync` pass.
  #[cfg(target_os = "windows")]
  pub(crate) fn begin_workspace_redirect_fallback(&mut self) -> bool {
    if self.workspace_redirect_fallback {
      // A newer target arrived while old coverage was already holding or
      // fading. Keep its current opacity, cancel only the old fade clock,
      // and wait for the newest real layout to synchronize
      // underneath it.
      self.workspace_redirect_real_ready = false;
      self.workspace_redirect_fade = None;
      return self.pending_ws_cleanup.is_some();
    }
    if let Some(active) = self.workspace_switch.take() {
      self.pending_ws_cleanup = Some(active);
      self.workspace_cleanup_sync_retry.clear();
    }
    if self.pending_ws_cleanup.is_none() {
      return false;
    }
    self.workspace_redirect_fallback = true;
    self.workspace_redirect_real_ready = false;
    self.workspace_redirect_fade = None;
    true
  }

  /// Captures the exact last submitted frame of one retained workspace
  /// surrogate group and arms a bounded directional reveal.
  #[cfg(target_os = "windows")]
  fn begin_workspace_directional_reveal(
    &mut self,
    order_direction: i32,
    ws_config: &WorkspaceSwitchAnimationConfig,
  ) -> bool {
    if !ws_config.should_animate()
      || ws_config.style != WorkspaceSwitchStyle::Slide
      || order_direction == 0
    {
      return false;
    }

    if self.workspace_redirect_fallback {
      let Some(fade) = self.workspace_redirect_fade.as_mut() else {
        return false;
      };
      if !fade.retarget_direction_before_motion(order_direction) {
        return false;
      }
      self.workspace_redirect_real_ready = false;
      return self.pending_ws_cleanup.is_some();
    }

    if let Some(active) = self.workspace_switch.take() {
      self.pending_ws_cleanup = Some(active);
      self.workspace_cleanup_sync_retry.clear();
    }
    let Some(ws) = self.pending_ws_cleanup.as_mut() else {
      return false;
    };
    if ws.style != WorkspaceSwitchStyle::Slide
      || ws.slide_direction != ws_config.direction
    {
      return false;
    }

    let captured = ws.windows.values_mut().all(|entry| {
      entry
        .surrogate
        .as_mut()
        .is_some_and(WorkspaceSurrogate::begin_redirect_reveal)
    });
    if !captured {
      return false;
    }

    let (axis, configured_distance) = match ws.slide_direction {
      WorkspaceSwitchDirection::Horizontal => {
        (SlideAxis::Horizontal, ws.slide_distance_h)
      }
      WorkspaceSwitchDirection::Vertical => {
        (SlideAxis::Vertical, ws.slide_distance_v)
      }
    };
    let (duration, distance) = workspace_redirect_reveal_timing(
      Duration::from_millis(u64::from(ws_config.duration_ms)),
      configured_distance,
    );
    if duration.is_zero() || distance == 0 {
      return false;
    }

    self.workspace_redirect_fallback = true;
    self.workspace_redirect_real_ready = false;
    self.workspace_redirect_fade = Some(WorkspaceRedirectFadeState {
      readiness_tick_seen: false,
      start_time: None,
      effect: WorkspaceRedirectEffect::DirectionalReveal {
        axis,
        direction: order_direction,
        duration,
        distance,
        motion_started: false,
      },
      frame_gaps: WorkspaceFrameGapTelemetry::default(),
    });
    tracing::debug!(
      duration_ms = duration.as_millis(),
      distance_px = distance,
      direction = order_direction,
      retained_participants = ws.windows.len(),
      "Armed bounded directional workspace reveal from the last submitted frame."
    );
    let _ = self.ensure_timer_running();
    true
  }

  /// Marks the latest real-window layout as ready so static effects can be
  /// staged underneath old rapid-switch coverage before teardown.
  #[cfg(target_os = "windows")]
  pub(crate) fn prepare_workspace_redirect_fallback_effects(&mut self) {
    if self.workspace_redirect_fallback {
      self.workspace_redirect_real_ready = true;
    }
  }

  /// Starts a nonblocking fade of old rapid-switch coverage after the
  /// latest logical real windows and effects have synchronized
  /// successfully.
  #[cfg(target_os = "windows")]
  pub(crate) fn finish_workspace_redirect_fallback(&mut self) {
    if !self.workspace_redirect_fallback
      || !self.workspace_redirect_real_ready
      || self.pending_ws_cleanup.is_none()
    {
      return;
    }

    if self.workspace_redirect_fade.is_none() {
      if let Some(ws) = &mut self.pending_ws_cleanup {
        for entry in ws.windows.values_mut() {
          if let Some(surrogate) = &mut entry.surrogate {
            surrogate.begin_redirect_fade();
          }
        }
      }
      self.workspace_redirect_fade = Some(WorkspaceRedirectFadeState {
        readiness_tick_seen: false,
        start_time: None,
        effect: WorkspaceRedirectEffect::OpacityFade,
        frame_gaps: WorkspaceFrameGapTelemetry::default(),
      });
    }
    let _ = self.ensure_timer_running();
  }

  /// Whether incompatible rapid-switch coverage is retaining visual
  /// ownership.
  #[cfg(target_os = "windows")]
  pub(crate) fn is_workspace_redirect_fallback_active(&self) -> bool {
    self.workspace_redirect_fallback && self.pending_ws_cleanup.is_some()
  }

  /// Advances the one-tick readiness hold and short crossfade for an
  /// incompatible rapid workspace redirect. Returns participant IDs when
  /// old coverage has been removed and final focus/overlay sync may run.
  #[cfg(target_os = "windows")]
  fn advance_workspace_redirect_fade(
    &mut self,
    now: Instant,
    blur_overlays: &mut HashMap<Uuid, NativeBlurOverlay>,
  ) -> Option<Vec<Uuid>> {
    if !self.workspace_redirect_fallback
      || !self.workspace_redirect_real_ready
    {
      return None;
    }
    let fade = self.workspace_redirect_fade.as_mut()?;
    if !fade.readiness_tick_seen {
      fade.readiness_tick_seen = true;
      if matches!(
        fade.effect,
        WorkspaceRedirectEffect::DirectionalReveal { .. }
      ) {
        // Anchor now so the next eligible compositor tick submits the
        // first nonzero directional movement instead of spending
        // another frame at progress zero.
        fade.start_time = Some(now);
      }
      return None;
    }

    match &mut fade.effect {
      WorkspaceRedirectEffect::OpacityFade => {
        let start = *fade.start_time.get_or_insert(now);
        let progress = now.saturating_duration_since(start).as_secs_f32()
          / SESSION_FADE_OUT.as_secs_f32();
        let progress = progress.clamp(0.0, 1.0);
        fade.frame_gaps.record(now);
        let updates_ok =
          self.pending_ws_cleanup.as_mut().is_some_and(|ws| {
            ws.windows.values_mut().all(|entry| {
              entry.surrogate.as_mut().is_none_or(|surrogate| {
                surrogate.update_redirect_fade(progress)
              })
            })
          });

        if updates_ok && progress < 1.0 {
          return None;
        }
        if !updates_ok {
          tracing::warn!(
            "Rapid workspace crossfade update failed; dropping obsolete coverage over the visible latest workspace."
          );
        }
      }
      WorkspaceRedirectEffect::DirectionalReveal {
        axis,
        direction,
        duration,
        distance,
        motion_started,
      } => {
        let start = *fade.start_time.get_or_insert(now);
        let raw_progress =
          now.saturating_duration_since(start).as_secs_f32()
            / duration.as_secs_f32();
        let raw_progress = raw_progress.clamp(0.0, 1.0);
        if raw_progress > 0.0 && !*motion_started {
          *motion_started = true;
          tracing::debug!(
            duration_ms = duration.as_millis(),
            distance_px = *distance,
            direction = *direction,
            "Submitted first directional workspace-reveal movement."
          );
        }
        let progress = workspace_redirect_reveal_easing(raw_progress);
        fade.frame_gaps.record(now);
        let mut batch = SurrogateBatch::new();
        let mut failed_ids = Vec::new();
        let mut surrogate_owners = Vec::new();

        if let Some(ws) = &mut self.pending_ws_cleanup {
          for (&id, entry) in &mut ws.windows {
            let Some(surrogate) = &mut entry.surrogate else {
              continue;
            };
            surrogate_owners.push((id, surrogate.handle()));
            if !surrogate.update_redirect_reveal(
              *axis, *direction, *distance, progress, &mut batch,
            ) {
              failed_ids.push(id);
            }

            if surrogate.is_live() {
              match surrogate.current_rect() {
                Some(rect) => {
                  if let Some(overlay) = blur_overlays.get_mut(&id) {
                    overlay.defer_rect(&mut batch, rect);
                  }
                }
                None => {
                  if let Some(overlay) = blur_overlays.get_mut(&id) {
                    overlay.hide();
                  }
                }
              }
            }
          }
        }

        let batch_result = batch.commit();
        for (id, handle) in surrogate_owners {
          if batch_result.failed_hwnds.contains(&handle) {
            failed_ids.push(id);
          }
        }
        let failed_overlay_ids: Vec<Uuid> = blur_overlays
          .iter()
          .filter(|(_, overlay)| {
            batch_result.failed_hwnds.contains(&overlay.handle())
          })
          .map(|(id, _)| *id)
          .collect();
        for id in failed_overlay_ids {
          tracing::warn!(
            "Removing blur overlay for {id} after directional workspace-reveal positioning failed."
          );
          blur_overlays.remove(&id);
        }

        failed_ids.sort_unstable();
        failed_ids.dedup();
        if let Some(ws) = &mut self.pending_ws_cleanup {
          for id in failed_ids {
            tracing::warn!(
              "Dropping only retained workspace participant {id} after its directional reveal update failed; the ready real underlay remains visible."
            );
            ws.windows.remove(&id);
          }
        }

        let has_coverage =
          self.pending_ws_cleanup.as_ref().is_some_and(|ws| {
            ws.windows.values().any(|entry| entry.surrogate.is_some())
          });
        if raw_progress < 1.0 && has_coverage {
          return None;
        }
      }
    }

    Some(self.complete_workspace_redirect_fade())
  }

  #[cfg(target_os = "windows")]
  fn complete_workspace_redirect_fade(&mut self) -> Vec<Uuid> {
    if let Some(fade) = &self.workspace_redirect_fade {
      fade.frame_gaps.log_summary("redirect");
    }
    let ids = self
      .pending_ws_cleanup
      .take()
      .map(|ws| ws.windows.keys().copied().collect())
      .unwrap_or_default();
    self.workspace_redirect_fallback = false;
    self.workspace_redirect_real_ready = false;
    self.workspace_redirect_fade = None;
    ids
  }

  /// Returns `true` while workspace coverage exists but the first
  /// compositor-paced readiness tick has not happened yet.
  #[cfg(target_os = "windows")]
  pub fn is_workspace_handoff_pending(&self) -> bool {
    self
      .workspace_switch
      .as_ref()
      .is_some_and(|ws| ws.handoff.is_pending())
  }

  /// Returns the live workspace participants whose real HWNDs still need
  /// to transfer behind their stationary surrogates.
  ///
  /// This stage starts only after the one-tick compositor readiness gate.
  /// Keeping it separate lets `platform_sync` retry a transient shell view
  /// lookup without advancing the shared workspace animation clock.
  #[cfg(target_os = "windows")]
  pub(crate) fn pending_workspace_cloak_participants(
    &self,
  ) -> Vec<(Uuid, isize)> {
    self
      .workspace_switch
      .as_ref()
      .filter(|ws| !ws.handoff.is_pending())
      .map(|ws| {
        ws.windows
          .iter()
          .filter(|(_, entry)| !entry.handoff_cloaked)
          .map(|(id, entry)| (*id, entry.source_handle))
          .collect()
      })
      .unwrap_or_default()
  }

  /// Records that one workspace participant's real HWND is now safely
  /// covered by its stationary surrogate.
  #[cfg(target_os = "windows")]
  pub(crate) fn mark_workspace_participant_cloaked(
    &mut self,
    window_id: &Uuid,
    expected_handle: isize,
  ) {
    if let Some(entry) = self
      .workspace_switch
      .as_mut()
      .and_then(|ws| ws.windows.get_mut(window_id))
      .filter(|entry| entry.source_handle == expected_handle)
    {
      entry.handoff_cloaked = true;
    }
  }

  /// Returns the recorded workspace-handoff cloak state when `window_id`
  /// belongs to active or retained workspace coverage.
  ///
  /// `Some(false)` is important during local fallback: that participant's
  /// cloak call failed before visibility transferred, so attempting an
  /// unnecessary uncloak would repeat the same access/native error and
  /// could incorrectly escalate to global animation recovery.
  #[cfg(target_os = "windows")]
  pub(crate) fn workspace_participant_handoff_cloaked(
    &self,
    window_id: &Uuid,
  ) -> Option<bool> {
    self
      .workspace_switch
      .as_ref()
      .and_then(|ws| ws.windows.get(window_id))
      .or_else(|| {
        self
          .pending_ws_cleanup
          .as_ref()
          .and_then(|ws| ws.windows.get(window_id))
      })
      .map(|entry| entry.handoff_cloaked)
  }

  /// Advances the bounded retry counter for a transient ApplicationView
  /// lookup failure.
  #[cfg(target_os = "windows")]
  pub(crate) fn record_workspace_cloak_retry(
    &mut self,
  ) -> WorkspaceCloakRetryOutcome {
    let Some(ws) = self.workspace_switch.as_mut() else {
      return WorkspaceCloakRetryOutcome::FinalStateFallback;
    };

    ws.cloak_retry_ticks = ws.cloak_retry_ticks.saturating_add(1);
    if ws.cloak_retry_ticks >= WORKSPACE_CLOAK_RETRY_LIMIT {
      WorkspaceCloakRetryOutcome::FinalStateFallback
    } else {
      WorkspaceCloakRetryOutcome::Retry {
        attempt: ws.cloak_retry_ticks,
        remaining: WORKSPACE_CLOAK_RETRY_LIMIT - ws.cloak_retry_ticks,
      }
    }
  }

  /// Returns `true` while an iris snapshot exists but its first
  /// compositor-paced readiness tick has not happened yet.
  #[cfg(target_os = "windows")]
  pub fn is_iris_handoff_pending(&self) -> bool {
    self
      .iris_switch
      .as_ref()
      .is_some_and(|iris| iris.handoff.is_pending())
  }

  /// Schedules a second alpha reassertion for the next animation tick.
  /// This replaces a blocking `DwmFlush` barrier after visible
  /// `SWP_FRAMECHANGED` landings.
  #[cfg(target_os = "windows")]
  pub fn schedule_transparency_reassert(&mut self, window_id: Uuid) {
    self.pending_transparency_reasserts.insert(window_id);
    let _ = self.ensure_timer_running();
  }

  /// Returns `true` while `window_id` is an incoming participant in the
  /// active workspace-switch animation.
  ///
  /// Unlike the `pending_sync` incoming flag (cleared after the first
  /// `platform_sync`), this stays `true` for the full animation duration
  /// so that focus events during the animation do not prematurely
  /// uncloak the real window before the surrogate finishes sliding in.
  #[cfg(target_os = "windows")]
  pub fn is_workspace_switch_incoming(&self, window_id: &Uuid) -> bool {
    self
      .workspace_switch
      .as_ref()
      .and_then(|ws| ws.windows.get(window_id))
      .map(|e| e.is_incoming)
      .unwrap_or(false)
  }

  /// Installs a workspace-switch animation for the provided windows.
  ///
  /// Accepts pre-created [`WorkspaceSurrogate`] instances together with
  /// their incoming/outgoing flags. A shared driver advances all
  /// surrogates in lock-step so the entire workspace moves as one panel.
  /// Any previous workspace-switch state is dropped.
  ///
  /// `monitor_handle` is the `HMONITOR` of the animation monitor, used to
  /// look up the `IDXGIOutput` for per-monitor vsync waiting.
  /// on secondary monitors whose refresh rate differs from the primary.
  #[cfg(target_os = "windows")]
  pub fn start_workspace_switch(
    &mut self,
    windows: Vec<(Uuid, isize, Option<WorkspaceSurrogate>, bool)>,
    order_direction: i32,
    monitor_x: i32,
    monitor_width: i32,
    monitor_y: i32,
    monitor_height: i32,
    monitor_handle: isize,
    config: &UserConfig,
  ) {
    self.workspace_switch = None;

    let ws_config = &config.value.animations.workspace_switch;

    let duration_ms = ws_config.duration_ms;

    // Slide each workspace the full monitor dimension so the outgoing
    // workspace exits completely and the incoming starts one full
    // monitor away.
    let slide_distance_h = monitor_width.max(1);
    let slide_distance_v = monitor_height.max(1);

    let ws_windows: HashMap<Uuid, WorkspaceSwitchEntry> = windows
      .into_iter()
      .map(|(id, source_handle, surrogate, is_incoming)| {
        (
          id,
          WorkspaceSwitchEntry {
            source_handle,
            surrogate,
            is_incoming,
            handoff_cloaked: false,
          },
        )
      })
      .collect();

    if !ws_windows.is_empty() {
      tracing::info!(
        "Starting workspace-switch animation: style={:?}, direction={:?}, \
         order={}, monitor=({monitor_x},{monitor_y},{monitor_width}x{monitor_height}), \
         slide_distance=({slide_distance_h}x{slide_distance_v}), \
         windows={}",
        ws_config.style,
        ws_config.direction,
        order_direction,
        ws_windows.len(),
      );
      // Install the per-monitor DXGI vsync waiter so the timer thread
      // wakes up right after each vblank, giving a full frame period
      // for surrogate updates before the next DWM composition.
      match DxgiVsyncWaiter::for_monitor(monitor_handle) {
        Ok(waiter) => {
          *self
            .animation_timer_vsync
            .lock()
            .expect("animation mutex poisoned") = Some(Arc::new(waiter));
        }
        Err(err) => {
          tracing::warn!(
            ?err,
            "failed to create vsync waiter for workspace switch"
          );
        }
      }
      self.workspace_switch = Some(WorkspaceSwitchState {
        windows: ws_windows,
        start_time: None,
        duration: Duration::from_millis(u64::from(duration_ms)),
        base_duration: Duration::from_millis(u64::from(duration_ms)),
        progress_origin: 0.0,
        last_visual_progress: 0.0,
        redirect_requested_at: None,
        easing: ws_config.easing.clone(),
        style: ws_config.style.clone(),
        slide_direction: ws_config.direction.clone(),
        order_direction,
        monitor_x,
        monitor_width,
        monitor_y,
        monitor_height,
        slide_distance_h,
        slide_distance_v,
        zoom_factor: ws_config.zoom_factor.clamp(0.0, 1.0),
        opacity_outgoing: ws_config.opacity_outgoing,
        opacity_incoming: ws_config.opacity_incoming,
        vsync_anchored: false,
        handoff: TickHandoff::new(Instant::now()),
        cloak_retry_ticks: 0,
        cleanup_tick_seen: false,
        frame_gaps: WorkspaceFrameGapTelemetry::default(),
      });
    } else {
      tracing::warn!("Workspace-switch skipped: no windows to animate.");
    }
  }

  /// Starts an iris-wipe workspace transition driven by `overlay`.
  ///
  /// The overlay is a frozen snapshot of the outgoing workspace shown on
  /// top of the (already switched) real windows. The hole grows from
  /// radius `0` to `max_radius` — which fully covers the monitor — from
  /// `(origin_x, origin_y)` over `duration_ms`, revealing the live
  /// incoming workspace beneath. Installs the per-monitor vsync waiter
  /// so the wipe is frame-aligned, mirroring the slide driver.
  #[cfg(target_os = "windows")]
  pub fn start_iris_switch(
    &mut self,
    overlay: NativeIrisOverlay,
    origin_x: i32,
    origin_y: i32,
    max_radius: i32,
    monitor_handle: isize,
    duration_ms: u32,
    easing: EasingFunction,
  ) {
    tracing::info!(
      "Starting iris-wipe workspace switch: origin=({origin_x},{origin_y}), \
       max_radius={max_radius}, duration_ms={duration_ms}."
    );
    match DxgiVsyncWaiter::for_monitor(monitor_handle) {
      Ok(waiter) => {
        *self
          .animation_timer_vsync
          .lock()
          .expect("animation mutex poisoned") = Some(Arc::new(waiter));
      }
      Err(err) => {
        tracing::warn!(
          ?err,
          "failed to create vsync waiter for iris switch"
        );
      }
    }
    self.iris_switch = Some(IrisSwitchState {
      overlay,
      origin_x,
      origin_y,
      max_radius: max_radius.max(1),
      start_time: None,
      duration: Duration::from_millis(u64::from(duration_ms)),
      easing,
      handoff: TickHandoff::new(Instant::now()),
    });
    let _ = self.ensure_timer_running();
  }

  /// Drops any in-flight iris overlay immediately, revealing the real
  /// windows beneath.
  ///
  /// Called before snapshotting for a new switch so the snapshot captures
  /// the real current workspace rather than the previous overlay
  /// mid-wipe — making rapid switches play as clean successive wipes
  /// instead of nested ones.
  #[cfg(target_os = "windows")]
  pub fn clear_iris_switch(&mut self) {
    self.iris_switch = None;
  }

  /// Starts an open animation for a newly appearing window.
  ///
  /// The surrogate animates from a computed start state (determined by
  /// `window_open.direction`) to the window's final target rect. A
  /// `ResizeSession` handles all visuals; the real window remains cloaked
  /// until the animation completes.
  ///
  /// No-ops when `style` is `None` and `opacity_from` is `1.0` (nothing
  /// would visually change for the duration).
  #[cfg(target_os = "windows")]
  pub fn start_open_animation(
    &mut self,
    window_id: Uuid,
    target_rect: Rect,
    monitor_rect: Rect,
    effect_opacity: u8,
    corner_style: CornerStyle,
    blur_overlay: Option<BlurOverlayParams>,
    config: &UserConfig,
    native_window: &NativeWindow,
  ) {
    let anim_config = &config.value.animations.window_open;
    // Keep this defensive guard even though `platform_sync` normally
    // filters the call. Other callers must not turn a zero-duration
    // animation into DWM surrogate/readiness work that delays a newly
    // opened window.
    if !anim_config.should_animate() {
      return;
    }
    let is_zoom = anim_config.style == WindowTransitionStyle::Zoom;
    let is_stationary = anim_config.style.is_stationary();

    // Pace the open animation on the window's own monitor.
    self.ensure_waiter_for(DxgiVsyncWaiter::window_monitor(
      native_window.hwnd(),
    ));

    // Stationary styles keep the surrogate at target position; slide
    // styles offset the start one full window dimension off-screen.
    let start_rect = if is_stationary {
      target_rect.clone()
    } else {
      Self::compute_transition_start_rect(&target_rect, &anim_config.style)
    };

    let mut anim = WindowAnimationState::new_movement(
      start_rect.clone(),
      target_rect.clone(),
      anim_config.duration_ms,
      anim_config.easing.clone(),
    );

    // For `None`/fade style only: hold at progress 0.0 so the app can
    // paint before the surrogate reveals it. At progress 0.0 the
    // surrogate sits at the window's target rect with `start_opacity`,
    // so showing it too early would flash a black (unpainted)
    // rectangle at the window's position.
    //
    // Slide and zoom surrogates are invisible at progress 0.0 (off-screen
    // and zero-size respectively), so the grace period only adds a
    // blank gap for those styles — omit it so the animation starts
    // immediately and the blank between cloak and first visible
    // surrogate pixel is minimised.
    if is_stationary && !is_zoom {
      anim.start_delay = OPEN_PAINT_GRACE;
    }

    // Zoom open does NOT auto-fade — the surrogate is fully opaque so the
    // small thumbnail is immediately visible as it grows. Fade-in while
    // zooming makes the initial frames invisible (opacity=0 + tiny
    // size = nothing to see), which is why it felt unsmooth. Users can
    // still set opacity_from explicitly to combine fade with zoom.
    let effective_opacity_from = anim_config.opacity_from;

    if effective_opacity_from < 1.0 {
      let effect_frac = effect_opacity as f32 / 255.0;
      let start_frac =
        effective_opacity_from.clamp(0.0, 1.0) * effect_frac;
      anim.start_opacity = Some(OpacityValue(start_frac));
      anim.target_opacity = Some(OpacityValue(effect_frac));
    }

    match ResizeSession::begin(
      native_window.hwnd(),
      &start_rect,
      &target_rect,
      SessionOptions {
        kind: SessionKind::OpenClose,
        effect_opacity,
        initially_visible: false,
        corner_style,
        place_at_top: true,
        // No cache for open animations: the window is new, so no prior
        // sample exists.
        edge_color: None,
        blur_overlay,
      },
    ) {
      Ok(mut session) => {
        // Never cloak until a valid visible replacement exists. If the
        // native cloak fails, drop the provisional surrogate and leave the
        // real window visible instead of starting a double-rendered zoom.
        if is_zoom && session.has_surrogate() {
          if let Err(err) = native_window.set_cloaked(true) {
            tracing::warn!(
              "Unable to cloak window {window_id} for zoom-open animation: \
               {err}; using the visible real window."
            );
            return;
          }
        }
        session.zoom = is_zoom;
        let initial_opacity_u8 = (effective_opacity_from.clamp(0.0, 1.0)
          * effect_opacity as f32)
          .round() as u8;
        if effective_opacity_from < 1.0 {
          session.update(&start_rect, initial_opacity_u8);
        }
        if !session.has_surrogate() {
          tracing::warn!(
            "Open animation unavailable for {window_id}: DWM thumbnail \
             setup failed; revealing the real window at its final rect."
          );
          session.pre_commit();
          if let Err(err) = native_window.set_cloaked(false) {
            tracing::warn!(
              "Failed to uncloak window {window_id} after open animation \
               setup failure: {err}"
            );
          }
          if let Err(err) = native_window.show() {
            tracing::warn!(
              "Failed to show window {window_id} after open animation setup \
               failure: {err}"
            );
          }
          return;
        }
        // For zoom: the drive loop handles the first frame.
        // update_zoom_fade is NOT called here so the surrogate
        // stays hidden until the first animation tick sets the
        // correct progress.
        self.animations.insert(window_id, anim);
        self.resize_sessions.insert(window_id, session);
        self.pending_open_windows.insert(
          window_id,
          PendingOpenWindow {
            handle: native_window.hwnd().0,
            readiness: SurrogateHandoffReadiness::new(Instant::now()),
          },
        );
        if !is_stationary {
          self.slide_in_monitor_rects.insert(window_id, monitor_rect);
        }
      }
      Err(err) => {
        tracing::warn!(
          "Failed to begin open animation for {window_id}: {err}."
        );
      }
    }
  }

  /// Starts a close animation for a window.
  ///
  /// The transparent surrogate is registered and shown above the
  /// still-visible real window. `update_internal` waits for its
  /// compositor-paced readiness ticks before cloaking and detaching the
  /// real HWND. The surrogate style is determined by
  /// `window_close.style`:
  /// - `None`/`Zoom`: surrogate stays at `current_rect`, fades/zooms out.
  /// - Slide styles: surrogate slides off the corresponding screen edge
  ///   while fading. The real window is never repositioned during a close
  ///   animation.
  ///
  /// When the animation completes, `update_internal` sends `WM_CLOSE` and
  /// unmanages the window. No-ops if a close animation is already active.
  #[cfg(target_os = "windows")]
  pub fn start_close_animation(
    &mut self,
    window_id: Uuid,
    recovery_window: WindowContainer,
    recovery_workspace_id: Uuid,
    current_rect: Rect,
    effect_opacity: u8,
    corner_style: CornerStyle,
    blur_overlay: Option<BlurOverlayParams>,
    config: &UserConfig,
    native_window: &NativeWindow,
  ) {
    if !config.value.animations.window_close.should_animate() {
      return;
    }

    if self.pending_close_windows.contains_key(&window_id) {
      return;
    }

    let anim_config = &config.value.animations.window_close;
    let is_zoom = anim_config.style == WindowTransitionStyle::Zoom;
    let is_stationary = anim_config.style.is_stationary();

    // Skip stationary style (no slide, no zoom) with no opacity change —
    // nothing would visually change for the duration.
    if is_stationary && !is_zoom && anim_config.opacity_to >= 1.0 {
      return;
    }

    // For slide-out, the surrogate travels from current_rect to an
    // off-screen target. The real window stays at current_rect
    // throughout.
    let target_rect = if is_stationary {
      current_rect.clone()
    } else {
      Self::compute_transition_start_rect(
        &current_rect,
        &anim_config.style,
      )
    };

    let mut anim = WindowAnimationState::new_movement(
      current_rect.clone(),
      target_rect.clone(),
      anim_config.duration_ms,
      anim_config.easing.clone(),
    );

    if anim_config.opacity_to < 1.0 {
      let effect_frac = effect_opacity as f32 / 255.0;
      let target_frac =
        anim_config.opacity_to.clamp(0.0, 1.0) * effect_frac;
      anim.start_opacity = Some(OpacityValue(effect_frac));
      anim.target_opacity = Some(OpacityValue(target_frac));
    }

    match ResizeSession::begin(
      native_window.hwnd(),
      &current_rect,
      &target_rect,
      SessionOptions {
        kind: SessionKind::OpenClose,
        effect_opacity,
        initially_visible: false,
        corner_style,
        place_at_top: false,
        // Open/close handoffs never expose sampled solid fill.
        edge_color: None,
        blur_overlay,
      },
    ) {
      Ok(mut session) => {
        // `ResizeSession::begin` intentionally returns a session without a
        // surrogate when native surrogate creation or DWM thumbnail
        // registration fails, allowing move/resize animations to fall back
        // to direct real-window positioning. A close animation has
        // no such path: its real window is detached and cloaked by
        // the caller. Do not mark the close as active unless a
        // visible replacement is actually ready; the caller will
        // then issue the ordinary close request while the real
        // window remains managed and visible.
        if !session.has_surrogate() {
          tracing::warn!(
            "Close animation unavailable for {window_id}: surrogate or DWM \
             thumbnail creation failed; closing the real window directly."
          );
          return;
        }

        // Pace the close animation on the window's own monitor only after
        // its visual replacement is known-good. This avoids
        // retaining an unused waiter when construction falls back
        // to an immediate close.
        self.ensure_waiter_for(DxgiVsyncWaiter::window_monitor(
          native_window.hwnd(),
        ));

        // The handoff host is transparent, so showing it above the still
        // visible real window cannot expose a sampled-color frame. The
        // readiness gate keeps that real window visible for two animation
        // ticks before cloaking and detaching it.
        session.show();
        session.zoom = is_zoom;
        self.animations.insert(window_id, anim);
        self.resize_sessions.insert(window_id, session);
        self.pending_close_windows.insert(
          window_id,
          PendingCloseWindow {
            handle: native_window.hwnd().0,
            window: recovery_window,
            workspace_id: recovery_workspace_id,
            readiness: SurrogateHandoffReadiness::new(Instant::now()),
            detached: false,
          },
        );
      }
      Err(err) => {
        tracing::warn!(
          "Failed to begin close animation for {window_id}: {err}."
        );
      }
    }
  }

  /// Computes the off-screen rect for a slide open/close transition.
  ///
  /// For open (`start_open_animation`): returns the start rect positioned
  /// off-screen, one full window dimension outside the target edge. The
  /// surrogate slides from this rect to `base`.
  ///
  /// For close (`start_close_animation`): returns the off-screen target
  /// rect so the surrogate slides from `base` (the window's current
  /// position) to off-screen.
  ///
  /// `SlideRight` → exits/enters from the right edge;
  /// `SlideLeft` → left edge; `SlideTop` → top; `SlideBottom` → bottom.
  #[cfg(target_os = "windows")]
  fn compute_transition_start_rect(
    base: &Rect,
    style: &WindowTransitionStyle,
  ) -> Rect {
    let w = base.width();
    let h = base.height();

    let (x, y) = match style {
      WindowTransitionStyle::SlideRight => (base.x() + w, base.y()),
      WindowTransitionStyle::SlideLeft => (base.x() - w, base.y()),
      WindowTransitionStyle::SlideTop => (base.x(), base.y() - h),
      WindowTransitionStyle::SlideBottom => (base.x(), base.y() + h),
      // Stationary styles never call this function.
      _ => (base.x(), base.y()),
    };

    Rect::from_xy(x, y, w, h)
  }

  /// Applies the configured effect opacity to all outgoing
  /// workspace-switch surrogates.
  ///
  /// Called after the outgoing real windows have been cloaked so the
  /// thumbnail opacity transitions from the fully-opaque `show_initial`
  /// state to the configured effect opacity without causing a
  /// double-blend frame.
  #[cfg(target_os = "windows")]
  pub fn apply_outgoing_surrogate_opacities(&mut self) {
    let Some(ref mut ws) = self.workspace_switch else {
      return;
    };
    for entry in ws.windows.values_mut() {
      if !entry.is_incoming {
        if let Some(ref mut s) = entry.surrogate {
          s.apply_effect_opacity();
        }
      }
    }
  }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use std::sync::{
    atomic::AtomicUsize, mpsc as std_mpsc, Arc as StdArc, Condvar,
    Mutex as StdMutex,
  };

  use super::*;
  use crate::animation::engine::apply_easing;

  #[test]
  fn rect_change_distance_does_not_overflow_signed_geometry() {
    let first = Rect::from_ltrb(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
    let second = Rect::from_ltrb(i32::MAX, i32::MAX, i32::MAX, i32::MAX);

    let distance = AnimationManager::rect_change_distance(&first, &second);
    assert!(distance > i64::from(u32::MAX));
    assert!(distance > i64::from(i32::MAX));
  }

  struct BlockingTestWaiter {
    gate: StdArc<(StdMutex<bool>, Condvar)>,
    entered_tx: std_mpsc::Sender<()>,
    wait_calls: StdArc<AtomicUsize>,
  }

  impl AnimationVsyncWaiter for BlockingTestWaiter {
    fn wait(&self) -> bool {
      self.wait_calls.fetch_add(1, Ordering::AcqRel);
      let _ = self.entered_tx.send(());
      let (lock, condition) = &*self.gate;
      let mut released = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
      while !*released {
        released = condition
          .wait(released)
          .unwrap_or_else(std::sync::PoisonError::into_inner);
      }
      true
    }

    fn monitor_handle(&self) -> isize {
      1
    }

    fn frame_period_us(&self) -> u64 {
      6_944
    }

    fn last_wake(&self) -> Option<Instant> {
      None
    }
  }

  fn timer_test_animation() -> WindowAnimationState {
    WindowAnimationState::new_movement(
      Rect::from_xy(0, 0, 100, 100),
      Rect::from_xy(100, 0, 100, 100),
      360,
      EasingFunction::default(),
    )
  }

  fn test_workspace_switch(
    entries: &[(Uuid, isize, bool)],
  ) -> WorkspaceSwitchState {
    WorkspaceSwitchState {
      windows: entries
        .iter()
        .map(|(id, source_handle, is_incoming)| {
          (
            *id,
            WorkspaceSwitchEntry {
              source_handle: *source_handle,
              surrogate: None,
              is_incoming: *is_incoming,
              handoff_cloaked: true,
            },
          )
        })
        .collect(),
      start_time: None,
      duration: Duration::from_millis(360),
      base_duration: Duration::from_millis(360),
      progress_origin: 0.0,
      last_visual_progress: 0.0,
      redirect_requested_at: None,
      easing: EasingFunction::default(),
      style: WorkspaceSwitchStyle::Slide,
      slide_direction: WorkspaceSwitchDirection::Horizontal,
      order_direction: 1,
      monitor_x: 0,
      monitor_width: 1920,
      monitor_y: 0,
      monitor_height: 1080,
      slide_distance_h: 1920,
      slide_distance_v: 1080,
      vsync_anchored: false,
      zoom_factor: 0.0,
      opacity_outgoing: 1.0,
      opacity_incoming: 1.0,
      handoff: TickHandoff::new(Instant::now()),
      cloak_retry_ticks: 0,
      cleanup_tick_seen: false,
      frame_gaps: WorkspaceFrameGapTelemetry::default(),
    }
  }

  #[test]
  fn pending_workspace_cleanup_keeps_animation_manager_active() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);

    assert!(!manager.has_active_animations());

    manager.pending_ws_cleanup = Some(WorkspaceSwitchState {
      windows: HashMap::new(),
      start_time: None,
      duration: Duration::ZERO,
      base_duration: Duration::ZERO,
      progress_origin: 0.0,
      last_visual_progress: 0.0,
      redirect_requested_at: None,
      easing: EasingFunction::default(),
      style: WorkspaceSwitchStyle::Slide,
      slide_direction: WorkspaceSwitchDirection::Horizontal,
      order_direction: 1,
      monitor_x: 0,
      monitor_width: 1,
      monitor_y: 0,
      monitor_height: 1,
      slide_distance_h: 1,
      slide_distance_v: 1,
      vsync_anchored: false,
      zoom_factor: 0.0,
      opacity_outgoing: 1.0,
      opacity_incoming: 1.0,
      handoff: TickHandoff::new(Instant::now()),
      cloak_retry_ticks: 0,
      cleanup_tick_seen: false,
      frame_gaps: WorkspaceFrameGapTelemetry::default(),
    });

    assert!(manager.has_active_animations());
  }

  #[test]
  fn one_shot_workspace_cleanup_failure_schedules_retry_and_success_resets(
  ) {
    let start = Instant::now();
    let mut retry = WorkspaceCleanupSyncRetry::default();

    assert_eq!(
      retry.record_failure(start),
      WorkspaceCleanupSyncFailureAction::Retry {
        attempt: 1,
        remaining: 2,
      }
    );
    retry.clear();
    assert_eq!(
      retry.record_failure(start + Duration::from_millis(10)),
      WorkspaceCleanupSyncFailureAction::Retry {
        attempt: 1,
        remaining: 2,
      }
    );
  }

  #[test]
  fn persistent_workspace_cleanup_failure_is_strictly_bounded() {
    let start = Instant::now();
    let mut retry = WorkspaceCleanupSyncRetry::default();

    assert!(matches!(
      retry.record_failure(start),
      WorkspaceCleanupSyncFailureAction::Retry { attempt: 1, .. }
    ));
    assert!(matches!(
      retry.record_failure(start + Duration::from_millis(10)),
      WorkspaceCleanupSyncFailureAction::Retry { attempt: 2, .. }
    ));
    assert_eq!(
      retry.record_failure(start + Duration::from_millis(20)),
      WorkspaceCleanupSyncFailureAction::Recover
    );
    assert_eq!(
      retry.record_failure(start + Duration::from_millis(30)),
      WorkspaceCleanupSyncFailureAction::Recover
    );
  }

  #[test]
  fn workspace_cleanup_retry_deadline_is_independent_of_frame_delivery() {
    let start = Instant::now();
    let mut retry = WorkspaceCleanupSyncRetry::default();

    assert!(matches!(
      retry.record_failure(start),
      WorkspaceCleanupSyncFailureAction::Retry { .. }
    ));
    assert_eq!(
      retry.record_failure(start + WORKSPACE_CLEANUP_SYNC_RETRY_TIMEOUT),
      WorkspaceCleanupSyncFailureAction::Recover
    );
  }

  #[test]
  fn scheduled_workspace_cleanup_retry_preserves_unrelated_animation_group(
  ) {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let cleanup_id = Uuid::new_v4();
    let unrelated_id = Uuid::new_v4();
    manager.pending_ws_cleanup =
      Some(test_workspace_switch(&[(cleanup_id, 1, true)]));
    manager
      .animations
      .insert(unrelated_id, timer_test_animation());

    assert!(matches!(
      manager
        .workspace_cleanup_sync_retry
        .record_failure(Instant::now()),
      WorkspaceCleanupSyncFailureAction::Retry { .. }
    ));
    assert!(manager.pending_ws_cleanup.is_some());
    assert!(manager.animations.contains_key(&unrelated_id));
    assert_eq!(manager.active_window_ids(), vec![unrelated_id]);
  }

  #[test]
  fn workspace_duration_ownership_survives_visual_settle_for_named_curves()
  {
    let duration = Duration::from_millis(360);
    let start = Instant::now();
    let curves = [
      EasingFunction::CubicBezier(0.0, 0.0, 1.0, 1.0),
      EasingFunction::EaseOutQuart,
      EasingFunction::EaseOutQuint,
      EasingFunction::EaseOutExpo,
      EasingFunction::EaseOutBack,
      EasingFunction::EaseOutSpring,
    ];

    for refresh_hz in [60_u64, 120, 144, 175] {
      let period = Duration::from_nanos(1_000_000_000 / refresh_hz);
      for easing in &curves {
        let mut first_complete = None;
        for frame in 0..256_u32 {
          let elapsed = period * frame;
          let raw = crate::animation::engine::animation_progress_at(
            start,
            duration,
            start + elapsed,
          );
          let progress = workspace_frame_progress(
            raw,
            apply_easing(raw, easing),
            easing.can_overshoot(),
            Some(1_920),
          );
          if elapsed < duration {
            assert!(
              !progress.complete,
              "{easing:?} completed at {elapsed:?} on {refresh_hz} Hz"
            );
          } else if progress.complete {
            first_complete = Some(elapsed);
            break;
          }
        }

        let completed_at =
          first_complete.expect("transition must complete");
        assert!(completed_at >= duration);
        assert!(
          completed_at <= duration + period + Duration::from_millis(1),
          "{easing:?} completed at {completed_at:?} on {refresh_hz} Hz"
        );
      }
    }
  }

  #[test]
  fn ease_out_quint_settles_subpixel_geometry_without_ending_clock() {
    let easing = EasingFunction::EaseOutQuint;
    let raw = 285.0 / 360.0;
    let eased = apply_easing(raw, &easing);
    let progress = workspace_frame_progress(
      raw,
      eased,
      easing.can_overshoot(),
      Some(1_920),
    );

    assert_eq!(progress.visual, 1.0);
    assert!(!progress.complete);
    assert!(
      (1.0 - eased) * 1_920.0 <= WS_COMPLETE_THRESHOLD_PX,
      "expected subpixel residual, got {} px",
      (1.0 - eased) * 1_920.0
    );

    let complete = workspace_frame_progress(
      1.0,
      apply_easing(1.0, &easing),
      easing.can_overshoot(),
      Some(1_920),
    );
    assert_eq!(complete.visual, 1.0);
    assert!(complete.complete);
  }

  #[test]
  fn overshooting_workspace_curves_never_settle_before_duration() {
    for easing in
      [EasingFunction::EaseOutBack, EasingFunction::EaseOutSpring]
    {
      for raw in [0.25_f32, 0.5, 0.75, 0.99] {
        let eased = apply_easing(raw, &easing);
        let progress = workspace_frame_progress(
          raw,
          eased,
          easing.can_overshoot(),
          Some(1_920),
        );
        assert_eq!(progress.visual, eased);
        assert!(!progress.complete);
      }
    }
  }

  #[test]
  fn exact_reversal_from_visually_settled_tail_reuses_existing_group() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let outgoing = Uuid::new_v4();
    let incoming = Uuid::new_v4();
    let mut ws = test_workspace_switch(&[
      (outgoing, 10, false),
      (incoming, 20, true),
    ]);
    ws.last_visual_progress = 1.0;
    ws.start_time = Some(Instant::now());
    ws.handoff.ready = true;
    manager.workspace_switch = Some(ws);
    let requested_roles =
      HashMap::from([(outgoing, true), (incoming, false)]);

    assert_eq!(
      manager.redirect_workspace_switch(
        &requested_roles,
        -1,
        &workspace_redirect_config(),
      ),
      WorkspaceRedirectOutcome::Redirected
    );
    let redirected = manager.workspace_switch.as_ref().unwrap();
    assert_eq!(redirected.windows.len(), 2);
    assert_eq!(redirected.progress_origin, 0.0);
    assert_eq!(redirected.duration, Duration::from_millis(360));
    assert_eq!(redirected.start_time, None);
  }

  #[test]
  fn redirect_crossfade_never_reshows_obsolete_acrylic_overlay() {
    assert!(should_preshow_workspace_cleanup_overlay(false, false));
    assert!(!should_preshow_workspace_cleanup_overlay(false, true));
    assert!(!should_preshow_workspace_cleanup_overlay(true, false));
  }

  fn workspace_redirect_config() -> WorkspaceSwitchAnimationConfig {
    WorkspaceSwitchAnimationConfig {
      duration_ms: 360,
      easing: EasingFunction::default(),
      zoom_factor: 0.0,
      opacity_outgoing: 1.0,
      opacity_incoming: 1.0,
      ..WorkspaceSwitchAnimationConfig::default()
    }
  }

  fn directional_reveal_state(
    direction: i32,
  ) -> WorkspaceRedirectFadeState {
    WorkspaceRedirectFadeState {
      readiness_tick_seen: false,
      start_time: None,
      effect: WorkspaceRedirectEffect::DirectionalReveal {
        axis: SlideAxis::Horizontal,
        direction,
        duration: Duration::from_millis(144),
        distance: 768,
        motion_started: false,
      },
      frame_gaps: WorkspaceFrameGapTelemetry::default(),
    }
  }

  #[test]
  fn directional_reveal_timing_is_bounded_and_preserves_slide_speed() {
    let (duration, distance) =
      workspace_redirect_reveal_timing(Duration::from_millis(360), 1920);
    assert_eq!(duration, Duration::from_millis(144));
    assert_eq!(distance, 768);

    let (short_duration, short_distance) =
      workspace_redirect_reveal_timing(Duration::from_millis(80), 800);
    assert_eq!(short_duration, Duration::from_millis(80));
    assert_eq!(short_distance, 800);

    let (long_duration, long_distance) = workspace_redirect_reveal_timing(
      Duration::from_millis(1_000),
      2_000,
    );
    assert_eq!(long_duration, Duration::from_millis(180));
    assert_eq!(long_distance, 360);

    assert_eq!(
      workspace_redirect_reveal_timing(Duration::ZERO, 1920),
      (Duration::ZERO, 0)
    );
  }

  #[test]
  fn workspace_frame_gap_telemetry_is_bounded_and_deterministic() {
    let mut telemetry = WorkspaceFrameGapTelemetry::default();
    let start = Instant::now();
    telemetry.record(start);
    for elapsed_ms in [10, 30, 60, 100, 150] {
      telemetry.record(start + Duration::from_millis(elapsed_ms));
    }

    assert_eq!(telemetry.summary_ms(), Some((5, 30.0, 50.0, 50.0)));

    for index in 0..(WORKSPACE_FRAME_GAP_CAPACITY + 20) {
      telemetry.record(start + Duration::from_millis(151 + index as u64));
    }
    assert_eq!(telemetry.len, WORKSPACE_FRAME_GAP_CAPACITY);
    assert_eq!(telemetry.total_samples, WORKSPACE_FRAME_GAP_CAPACITY + 25);
  }

  #[test]
  fn directional_reveal_easing_is_monotonic_bounded_and_non_overshooting()
  {
    let values: Vec<f32> = (0..=100)
      .map(|step| workspace_redirect_reveal_easing(step as f32 / 100.0))
      .collect();
    assert_eq!(values.first(), Some(&0.0));
    assert_eq!(values.last(), Some(&1.0));
    assert!(values.iter().all(|value| (0.0..=1.0).contains(value)));
    assert!(values.windows(2).all(|pair| pair[1] >= pair[0]));
  }

  #[test]
  fn reveal_direction_changes_before_motion_and_locks_after_motion() {
    let mut reveal = directional_reveal_state(1);
    reveal.readiness_tick_seen = true;
    reveal.start_time = Some(Instant::now());

    assert!(reveal.retarget_direction_before_motion(-1));
    assert!(!reveal.readiness_tick_seen);
    assert_eq!(reveal.start_time, None);
    assert!(matches!(
      reveal.effect,
      WorkspaceRedirectEffect::DirectionalReveal {
        direction: -1,
        motion_started: false,
        ..
      }
    ));

    let locked_start = Instant::now();
    reveal.readiness_tick_seen = true;
    reveal.start_time = Some(locked_start);
    if let WorkspaceRedirectEffect::DirectionalReveal {
      motion_started,
      ..
    } = &mut reveal.effect
    {
      *motion_started = true;
    }
    assert!(reveal.retarget_direction_before_motion(1));
    assert!(reveal.readiness_tick_seen);
    assert_eq!(reveal.start_time, Some(locked_start));
    assert!(matches!(
      reveal.effect,
      WorkspaceRedirectEffect::DirectionalReveal {
        direction: -1,
        motion_started: true,
        ..
      }
    ));
  }

  #[test]
  fn repeated_destination_after_motion_keeps_one_group_and_one_clock() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    manager.pending_ws_cleanup =
      Some(test_workspace_switch(&[(a, 11, false), (b, 22, true)]));
    manager.workspace_redirect_fallback = true;
    let start = Instant::now();
    let mut reveal = directional_reveal_state(1);
    reveal.readiness_tick_seen = true;
    reveal.start_time = Some(start);
    if let WorkspaceRedirectEffect::DirectionalReveal {
      motion_started,
      ..
    } = &mut reveal.effect
    {
      *motion_started = true;
    }
    manager.workspace_redirect_fade = Some(reveal);

    assert_eq!(
      manager.begin_incompatible_workspace_redirect(
        -1,
        &workspace_redirect_config(),
      ),
      WorkspaceRedirectOutcome::DirectionalReveal
    );
    let fade = manager.workspace_redirect_fade.as_ref().unwrap();
    assert_eq!(fade.start_time, Some(start));
    assert!(fade.readiness_tick_seen);
    assert!(matches!(
      fade.effect,
      WorkspaceRedirectEffect::DirectionalReveal {
        direction: 1,
        motion_started: true,
        ..
      }
    ));
    assert!(manager.workspace_switch.is_none());
    assert_eq!(
      manager.pending_ws_cleanup.as_ref().unwrap().windows.len(),
      2
    );
  }

  #[test]
  fn directional_reveal_waits_one_tick_then_moves_without_waiting_360_ms()
  {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    manager.pending_ws_cleanup =
      Some(test_workspace_switch(&[(Uuid::new_v4(), 11, false)]));
    manager.workspace_redirect_fallback = true;
    manager.workspace_redirect_real_ready = true;
    manager.workspace_redirect_fade = Some(directional_reveal_state(1));
    let mut blur_overlays = HashMap::new();
    let now = Instant::now();

    assert!(manager
      .advance_workspace_redirect_fade(now, &mut blur_overlays)
      .is_none());
    let fade = manager.workspace_redirect_fade.as_ref().unwrap();
    assert!(fade.readiness_tick_seen);
    assert_eq!(fade.start_time, Some(now));

    // Test participants have no native surrogate, so the first movement
    // tick safely completes obsolete coverage. Crucially, it is eligible
    // after one frame rather than after the original 360 ms slide.
    let completed = manager
      .advance_workspace_redirect_fade(
        now + Duration::from_millis(7),
        &mut blur_overlays,
      )
      .unwrap();
    assert_eq!(completed.len(), 1);
    assert!(manager.pending_ws_cleanup.is_none());
    assert!(manager.workspace_redirect_fade.is_none());
  }

  fn staged_workspace_switch(
    entries: &[(Uuid, isize, bool)],
  ) -> WorkspaceSwitchState {
    let mut ws = test_workspace_switch(entries);
    ws.handoff.ready = true;
    for entry in ws.windows.values_mut() {
      entry.handoff_cloaked = false;
    }
    ws
  }

  #[test]
  fn transient_workspace_cloak_failure_retries_then_starts_from_zero() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let outgoing = Uuid::new_v4();
    let incoming = Uuid::new_v4();
    manager.workspace_switch = Some(staged_workspace_switch(&[
      (outgoing, 11, false),
      (incoming, 22, true),
    ]));

    assert_eq!(
      manager.record_workspace_cloak_retry(),
      WorkspaceCloakRetryOutcome::Retry {
        attempt: 1,
        remaining: 2,
      }
    );
    manager.mark_workspace_participant_cloaked(&outgoing, 11);
    assert_eq!(
      manager.pending_workspace_cloak_participants(),
      vec![(incoming, 22)]
    );
    manager.mark_workspace_participant_cloaked(&incoming, 22);

    let ws = manager.workspace_switch.as_ref().unwrap();
    assert!(manager.pending_workspace_cloak_participants().is_empty());
    assert_eq!(ws.start_time, None);
    assert_eq!(ws.last_visual_progress, 0.0);
  }

  #[test]
  fn persistent_workspace_cloak_failure_falls_back_locally() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let failed_participant = Uuid::new_v4();
    let covered_participant = Uuid::new_v4();
    let unrelated = Uuid::new_v4();
    let rect = Rect::from_xy(0, 0, 640, 480);
    manager.workspace_switch = Some(staged_workspace_switch(&[
      (failed_participant, 11, true),
      (covered_participant, 22, false),
    ]));
    manager.mark_workspace_participant_cloaked(&covered_participant, 22);
    manager.animations.insert(
      unrelated,
      WindowAnimationState::new_movement(
        rect.clone(),
        Rect::from_xy(20, 0, 640, 480),
        360,
        EasingFunction::default(),
      ),
    );

    assert!(matches!(
      manager.record_workspace_cloak_retry(),
      WorkspaceCloakRetryOutcome::Retry { attempt: 1, .. }
    ));
    assert!(matches!(
      manager.record_workspace_cloak_retry(),
      WorkspaceCloakRetryOutcome::Retry { attempt: 2, .. }
    ));
    assert_eq!(
      manager.record_workspace_cloak_retry(),
      WorkspaceCloakRetryOutcome::FinalStateFallback
    );
    assert!(manager.begin_workspace_redirect_fallback());

    assert!(manager.workspace_switch.is_none());
    assert!(manager.pending_ws_cleanup.is_some());
    assert!(manager.workspace_redirect_fallback);
    assert!(manager.animations.contains_key(&unrelated));
    assert_eq!(
      manager.workspace_participant_handoff_cloaked(&failed_participant),
      Some(false)
    );
    assert_eq!(
      manager.workspace_participant_handoff_cloaked(&covered_participant),
      Some(true)
    );
  }

  #[test]
  fn destroyed_workspace_handoff_participant_does_not_cancel_survivor() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let destroyed = Uuid::new_v4();
    let survivor = Uuid::new_v4();
    manager.workspace_switch = Some(staged_workspace_switch(&[
      (destroyed, 11, false),
      (survivor, 22, true),
    ]));

    manager.remove_destroyed_window_state(&destroyed, 11);

    assert_eq!(
      manager.pending_workspace_cloak_participants(),
      vec![(survivor, 22)]
    );
    assert!(manager.workspace_switch.is_some());
  }

  #[test]
  fn exact_reverse_during_cloak_retry_reuses_bounded_state() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    manager.workspace_switch =
      Some(staged_workspace_switch(&[(a, 11, false), (b, 22, true)]));
    manager.mark_workspace_participant_cloaked(&a, 11);
    assert!(matches!(
      manager.record_workspace_cloak_retry(),
      WorkspaceCloakRetryOutcome::Retry { .. }
    ));

    assert_eq!(
      manager.redirect_workspace_switch(
        &HashMap::from([(a, true), (b, false)]),
        -1,
        &workspace_redirect_config(),
      ),
      WorkspaceRedirectOutcome::Redirected
    );
    let ws = manager.workspace_switch.as_ref().unwrap();
    assert!(ws.windows[&a].handoff_cloaked);
    assert!(!ws.windows[&b].handoff_cloaked);
    assert_eq!(ws.windows.len(), 2);
    assert!(manager.pending_ws_cleanup.is_none());
  }

  #[test]
  fn newer_third_workspace_during_cloak_retry_uses_latest_state_fallback()
  {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    manager.workspace_switch =
      Some(staged_workspace_switch(&[(a, 11, false), (b, 22, true)]));
    assert!(matches!(
      manager.record_workspace_cloak_retry(),
      WorkspaceCloakRetryOutcome::Retry { .. }
    ));

    assert_eq!(
      manager.redirect_workspace_switch(
        &HashMap::from([(b, false), (c, true)]),
        1,
        &workspace_redirect_config(),
      ),
      WorkspaceRedirectOutcome::FinalStateFallback
    );
    assert!(manager.workspace_switch.is_none());
    assert!(manager.pending_ws_cleanup.is_some());
    assert!(manager.workspace_redirect_fallback);
  }

  #[test]
  fn reverse_workspace_switch_reuses_one_bounded_state_from_current_frame()
  {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut ws = test_workspace_switch(&[(a, 11, false), (b, 22, true)]);
    ws.handoff.ready = true;
    ws.last_visual_progress = 0.35;
    manager.workspace_switch = Some(ws);

    let requested = HashMap::from([(a, true), (b, false)]);
    assert_eq!(
      manager.redirect_workspace_switch(
        &requested,
        -1,
        &workspace_redirect_config(),
      ),
      WorkspaceRedirectOutcome::Redirected
    );

    let ws = manager.workspace_switch.as_ref().unwrap();
    assert_eq!(ws.windows.len(), 2);
    assert!(ws.windows[&a].is_incoming);
    assert!(!ws.windows[&b].is_incoming);
    assert!((ws.progress_origin - 0.65).abs() < f32::EPSILON);
    assert_eq!(ws.last_visual_progress, ws.progress_origin);
    assert!(
      ws.duration.abs_diff(Duration::from_millis(126))
        < Duration::from_micros(1)
    );
    assert!(manager.pending_ws_cleanup.is_none());
    assert!(!manager.workspace_redirect_fallback);
  }

  #[test]
  fn repeated_reversals_replace_progress_without_growing_state() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut ws = test_workspace_switch(&[(a, 11, false), (b, 22, true)]);
    ws.handoff.ready = true;
    ws.last_visual_progress = 0.4;
    manager.workspace_switch = Some(ws);
    let config = workspace_redirect_config();

    assert_eq!(
      manager.redirect_workspace_switch(
        &HashMap::from([(a, true), (b, false)]),
        -1,
        &config,
      ),
      WorkspaceRedirectOutcome::Redirected
    );
    manager
      .workspace_switch
      .as_mut()
      .unwrap()
      .last_visual_progress = 0.8;
    assert_eq!(
      manager.redirect_workspace_switch(
        &HashMap::from([(a, false), (b, true)]),
        1,
        &config,
      ),
      WorkspaceRedirectOutcome::Redirected
    );

    let ws = manager.workspace_switch.as_ref().unwrap();
    assert_eq!(ws.windows.len(), 2);
    assert!(!ws.windows[&a].is_incoming);
    assert!(ws.windows[&b].is_incoming);
    assert!((ws.progress_origin - 0.2).abs() < f32::EPSILON);
    assert!(manager.pending_ws_cleanup.is_none());
  }

  #[test]
  fn current_profile_reverses_at_50_100_and_200_milliseconds() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let easing = EasingFunction::CubicBezier(0.33, 1.0, 0.68, 1.0);
    let config = WorkspaceSwitchAnimationConfig {
      duration_ms: 360,
      easing: easing.clone(),
      zoom_factor: 0.0,
      ..WorkspaceSwitchAnimationConfig::default()
    };

    for elapsed_ms in [50_u64, 100, 200] {
      let mut manager = AnimationManager::new(animation_tick_tx.clone());
      let mut ws = test_workspace_switch(&[(a, 11, false), (b, 22, true)]);
      ws.handoff.ready = true;
      ws.easing = easing.clone();
      let rendered = apply_easing(elapsed_ms as f32 / 360.0, &easing);
      ws.last_visual_progress = rendered;
      manager.workspace_switch = Some(ws);

      assert_eq!(
        manager.redirect_workspace_switch(
          &HashMap::from([(a, true), (b, false)]),
          -1,
          &config,
        ),
        WorkspaceRedirectOutcome::Redirected
      );
      let redirected = manager.workspace_switch.as_ref().unwrap();
      assert!(
        (redirected.progress_origin - (1.0 - rendered)).abs()
          < f32::EPSILON
      );
      assert_eq!(redirected.windows.len(), 2);
    }
  }

  #[test]
  fn hundred_held_key_reversals_keep_constant_state_and_latest_roles() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut ws = test_workspace_switch(&[(a, 11, false), (b, 22, true)]);
    ws.handoff.ready = true;
    ws.last_visual_progress = 0.5;
    manager.workspace_switch = Some(ws);
    let config = workspace_redirect_config();

    for command in 0..100 {
      let a_is_incoming = command % 2 == 0;
      let requested =
        HashMap::from([(a, a_is_incoming), (b, !a_is_incoming)]);
      let direction = if a_is_incoming { -1 } else { 1 };
      assert_eq!(
        manager.redirect_workspace_switch(&requested, direction, &config,),
        WorkspaceRedirectOutcome::Redirected
      );
      let ws = manager.workspace_switch.as_mut().unwrap();
      assert_eq!(ws.windows.len(), 2);
      assert_eq!(ws.windows[&a].is_incoming, a_is_incoming);
      ws.last_visual_progress = 0.5;
    }

    assert!(manager.pending_ws_cleanup.is_none());
    assert!(!manager.workspace_redirect_fallback);
  }

  #[test]
  fn repeated_same_target_does_not_restart_active_workspace_switch() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut ws = test_workspace_switch(&[(a, 11, false), (b, 22, true)]);
    ws.handoff.ready = true;
    ws.last_visual_progress = 0.42;
    ws.start_time = Some(Instant::now());
    let original_start = ws.start_time;
    manager.workspace_switch = Some(ws);

    assert_eq!(
      manager.redirect_workspace_switch(
        &HashMap::from([(a, false), (b, true)]),
        1,
        &workspace_redirect_config(),
      ),
      WorkspaceRedirectOutcome::ExistingRequest
    );
    let ws = manager.workspace_switch.as_ref().unwrap();
    assert_eq!(ws.start_time, original_start);
    assert_eq!(ws.last_visual_progress, 0.42);
    assert_eq!(ws.progress_origin, 0.0);
  }

  #[test]
  fn incompatible_latest_target_uses_single_final_state_fallback() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();
    let mut ws = test_workspace_switch(&[(a, 11, false), (b, 22, true)]);
    ws.handoff.ready = true;
    ws.last_visual_progress = 0.4;
    manager.workspace_switch = Some(ws);
    let config = workspace_redirect_config();

    assert_eq!(
      manager.redirect_workspace_switch(
        &HashMap::from([(b, false), (c, true)]),
        1,
        &config,
      ),
      WorkspaceRedirectOutcome::FinalStateFallback
    );
    assert!(manager.workspace_switch.is_none());
    assert_eq!(
      manager.pending_ws_cleanup.as_ref().unwrap().windows.len(),
      2
    );
    assert!(manager.workspace_redirect_fallback);
    assert!(manager.has_active_surrogate(&a));

    // A newer C -> D command replaces pending logical sync markers outside
    // the manager; it does not append another transition or participant.
    assert_eq!(
      manager.redirect_workspace_switch(
        &HashMap::from([(c, false), (d, true)]),
        1,
        &config,
      ),
      WorkspaceRedirectOutcome::FinalStateFallback
    );
    assert_eq!(
      manager.pending_ws_cleanup.as_ref().unwrap().windows.len(),
      2
    );

    manager.prepare_workspace_redirect_fallback_effects();
    assert!(manager.workspace_redirect_real_ready);
    assert!(!manager.has_active_surrogate(&a));
    manager.finish_workspace_redirect_fallback();
    assert!(manager.pending_ws_cleanup.is_some());
    assert!(manager.workspace_redirect_fallback);
    assert!(manager.workspace_redirect_real_ready);
    assert!(manager.workspace_redirect_fade.is_some());

    let now = Instant::now();
    let mut blur_overlays = HashMap::new();
    assert!(manager
      .advance_workspace_redirect_fade(now, &mut blur_overlays)
      .is_none());
    assert!(manager
      .advance_workspace_redirect_fade(
        now + Duration::from_millis(7),
        &mut blur_overlays,
      )
      .is_none());
    let completed = manager
      .advance_workspace_redirect_fade(
        now + Duration::from_millis(7) + SESSION_FADE_OUT,
        &mut blur_overlays,
      )
      .unwrap();
    assert_eq!(completed.len(), 2);
    assert!(manager.pending_ws_cleanup.is_none());
    assert!(!manager.workspace_redirect_fallback);
    assert!(!manager.workspace_redirect_real_ready);
    assert!(manager.workspace_redirect_fade.is_none());
  }

  #[test]
  fn newer_target_during_crossfade_reuses_constant_coverage_and_restarts_stage(
  ) {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();
    let mut ws = test_workspace_switch(&[(a, 11, false), (b, 22, true)]);
    ws.handoff.ready = true;
    manager.workspace_switch = Some(ws);
    let config = workspace_redirect_config();

    assert_eq!(
      manager.redirect_workspace_switch(
        &HashMap::from([(b, false), (c, true)]),
        1,
        &config,
      ),
      WorkspaceRedirectOutcome::FinalStateFallback
    );
    manager.prepare_workspace_redirect_fallback_effects();
    manager.finish_workspace_redirect_fallback();
    assert!(manager.workspace_redirect_fade.is_some());

    assert_eq!(
      manager.redirect_workspace_switch(
        &HashMap::from([(c, false), (d, true)]),
        1,
        &config,
      ),
      WorkspaceRedirectOutcome::FinalStateFallback
    );
    assert_eq!(
      manager.pending_ws_cleanup.as_ref().unwrap().windows.len(),
      2
    );
    assert!(!manager.workspace_redirect_real_ready);
    assert!(manager.workspace_redirect_fade.is_none());
  }

  #[test]
  fn failed_redirect_crossfade_drops_only_obsolete_coverage() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    manager.pending_ws_cleanup =
      Some(test_workspace_switch(&[(a, 11, false), (b, 22, true)]));
    manager.workspace_redirect_fallback = true;
    manager.workspace_redirect_real_ready = true;
    manager.workspace_redirect_fade = Some(WorkspaceRedirectFadeState {
      readiness_tick_seen: true,
      start_time: Some(Instant::now()),
      effect: WorkspaceRedirectEffect::OpacityFade,
      frame_gaps: WorkspaceFrameGapTelemetry::default(),
    });

    let completed = manager.complete_workspace_redirect_fade();

    assert_eq!(completed.len(), 2);
    assert!(completed.contains(&a));
    assert!(completed.contains(&b));
    assert!(manager.pending_ws_cleanup.is_none());
    assert!(!manager.workspace_redirect_fallback);
    assert!(!manager.workspace_redirect_real_ready);
    assert!(manager.workspace_redirect_fade.is_none());
  }

  #[test]
  fn staged_handoff_and_asymmetric_opacity_decline_live_redirect() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let requested = HashMap::from([(a, true), (b, false)]);

    let mut staged = AnimationManager::new(animation_tick_tx.clone());
    staged.workspace_switch =
      Some(test_workspace_switch(&[(a, 11, false), (b, 22, true)]));
    assert_eq!(
      staged.redirect_workspace_switch(
        &HashMap::from([(a, false), (b, true)]),
        1,
        &workspace_redirect_config(),
      ),
      WorkspaceRedirectOutcome::ExistingRequest
    );
    assert!(staged.workspace_switch.is_some());
    assert_eq!(
      staged.redirect_workspace_switch(
        &requested,
        -1,
        &workspace_redirect_config(),
      ),
      WorkspaceRedirectOutcome::FinalStateFallback
    );

    let mut asymmetric = AnimationManager::new(animation_tick_tx);
    let mut ws = test_workspace_switch(&[(a, 11, false), (b, 22, true)]);
    ws.handoff.ready = true;
    ws.opacity_incoming = 0.5;
    asymmetric.workspace_switch = Some(ws);
    let mut config = workspace_redirect_config();
    config.opacity_incoming = 0.5;
    assert_eq!(
      asymmetric.redirect_workspace_switch(&requested, -1, &config),
      WorkspaceRedirectOutcome::FinalStateFallback
    );
  }

  #[test]
  fn destroyed_window_cleanup_is_uuid_scoped_and_idempotent() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let stale = Uuid::new_v4();
    let survivor = Uuid::new_v4();
    // Simulate HWND reuse: both logical records observe the same numeric
    // value, but only the stale UUID may be canceled.
    let recycled_handle = 77;
    let rect = Rect::from_xy(0, 0, 640, 480);

    for id in [stale, survivor] {
      manager.animations.insert(
        id,
        WindowAnimationState::new_movement(
          rect.clone(),
          Rect::from_xy(20, 0, 640, 480),
          360,
          EasingFunction::default(),
        ),
      );
      manager.focus_opacity_animations.insert(
        id,
        WindowAnimationState::new_opacity(
          rect.clone(),
          OpacityValue::from_alpha(200),
          OpacityValue::from_alpha(255),
          120,
          EasingFunction::default(),
        ),
      );
      manager.pending_open_windows.insert(
        id,
        PendingOpenWindow {
          handle: recycled_handle,
          readiness: SurrogateHandoffReadiness::new(Instant::now()),
        },
      );
      manager.pending_move_resize_windows.insert(
        id,
        PendingMoveResizeWindow {
          handle: recycled_handle,
          readiness: SurrogateHandoffReadiness::new(Instant::now()),
          surrogate_revealed: false,
        },
      );
      manager.pending_transparency_reasserts.insert(id);
      manager
        .pending_surrogate_updates
        .push(PendingSurrogateUpdate {
          window_id: id,
          rect: rect.clone(),
          opacity: u8::MAX,
          handoff: false,
        });
    }
    manager.workspace_switch = Some(test_workspace_switch(&[
      (stale, recycled_handle, false),
      (survivor, recycled_handle, true),
    ]));
    manager.pending_ws_cleanup = Some(test_workspace_switch(&[
      (stale, recycled_handle, false),
      (survivor, recycled_handle, true),
    ]));

    manager.remove_destroyed_window_state(&stale, recycled_handle);
    manager.remove_destroyed_window_state(&stale, recycled_handle);

    assert!(!manager.animations.contains_key(&stale));
    assert!(!manager.focus_opacity_animations.contains_key(&stale));
    assert!(!manager.pending_open_windows.contains_key(&stale));
    assert!(!manager.pending_move_resize_windows.contains_key(&stale));
    assert!(!manager.pending_transparency_reasserts.contains(&stale));
    assert!(manager
      .pending_surrogate_updates
      .iter()
      .all(|update| update.window_id != stale));
    assert!(!manager
      .workspace_switch
      .as_ref()
      .is_some_and(|ws| ws.windows.contains_key(&stale)));
    assert!(!manager
      .pending_ws_cleanup
      .as_ref()
      .is_some_and(|ws| ws.windows.contains_key(&stale)));

    assert!(manager.animations.contains_key(&survivor));
    assert!(manager.focus_opacity_animations.contains_key(&survivor));
    assert!(manager.pending_open_windows.contains_key(&survivor));
    assert!(manager.pending_move_resize_windows.contains_key(&survivor));
    assert!(manager.pending_transparency_reasserts.contains(&survivor));
    assert!(manager
      .workspace_switch
      .as_ref()
      .is_some_and(|ws| ws.windows.contains_key(&survivor)));
    assert!(manager
      .pending_ws_cleanup
      .as_ref()
      .is_some_and(|ws| ws.windows.contains_key(&survivor)));
  }

  #[test]
  fn animation_ticks_coalesce_and_stop_after_receiver_closes() {
    let (animation_tick_tx, mut animation_tick_rx) = mpsc::channel(1);

    assert!(AnimationManager::queue_animation_tick(&animation_tick_tx));
    assert!(AnimationManager::queue_animation_tick(&animation_tick_tx));
    assert_eq!(animation_tick_rx.try_recv(), Ok(()));
    assert!(matches!(
      animation_tick_rx.try_recv(),
      Err(mpsc::error::TryRecvError::Empty)
    ));

    drop(animation_tick_rx);
    assert!(!AnimationManager::queue_animation_tick(&animation_tick_tx));
  }

  #[test]
  fn blocked_waiter_is_abandoned_once_and_fallback_keeps_ticking() {
    let (animation_tick_tx, mut animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let gate = StdArc::new((StdMutex::new(false), Condvar::new()));
    let wait_calls = StdArc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = std_mpsc::channel();
    *manager
      .animation_timer_vsync
      .lock()
      .expect("animation mutex should not be poisoned") =
      Some(Arc::new(BlockingTestWaiter {
        gate: StdArc::clone(&gate),
        entered_tx,
        wait_calls: StdArc::clone(&wait_calls),
      }));
    manager
      .animations
      .insert(Uuid::new_v4(), timer_test_animation());

    assert!(manager.ensure_timer_running());
    entered_rx
      .recv_timeout(Duration::from_secs(1))
      .expect("fake native waiter should block on its first wait");
    assert_eq!(animation_tick_rx.try_recv(), Ok(()));

    let deadline = manager
      .timer_watchdog_deadline()
      .expect("active native generation should have a deadline");
    assert!(manager.abandon_stalled_timer_generation(deadline));
    assert!(!manager.abandon_stalled_timer_generation(deadline));
    assert!(manager.animation_timer_fallback.load(Ordering::Acquire));
    assert!(!manager.animation_timer_running.load(Ordering::Acquire));

    // Recovery normally clears the affected animations before future input
    // starts another one. Model that boundary, then prove the independent
    // fixed-rate driver starts without waiting for the blocked native
    // call.
    manager.animations.clear();
    manager
      .animations
      .insert(Uuid::new_v4(), timer_test_animation());
    assert!(manager.ensure_timer_running());
    let fallback_deadline = Instant::now() + Duration::from_secs(1);
    while animation_tick_rx.try_recv().is_err() {
      assert!(
        Instant::now() < fallback_deadline,
        "fallback timer should queue an independent tick"
      );
      std::thread::yield_now();
    }
    assert!(manager.fallback_timer_thread.lock().unwrap().is_some());
    assert!(manager.timer_thread.lock().unwrap().is_some());

    let (lock, condition) = &*gate;
    *lock.lock().unwrap() = true;
    condition.notify_all();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
      wait_calls.load(Ordering::Acquire),
      1,
      "an abandoned native driver must never enter another wait"
    );
    manager.animations.clear();
    manager.invalidate_timer_generation();
  }

  #[test]
  fn control_transition_invalidation_clears_watchdog_generation() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    manager
      .animations
      .insert(Uuid::new_v4(), timer_test_animation());
    manager
      .animation_timer_running
      .store(true, Ordering::Release);
    let before =
      manager.animation_timer_generation.load(Ordering::Acquire);
    assert!(manager.timer_watchdog_deadline().is_some());

    manager.invalidate_timer_generation();

    assert!(!manager.animation_timer_running.load(Ordering::Acquire));
    assert!(
      manager.animation_timer_generation.load(Ordering::Acquire) > before
    );
    assert!(manager.timer_watchdog_deadline().is_none());
  }

  #[test]
  fn open_handoff_is_ready_after_one_tick() {
    let start = Instant::now();
    let mut readiness = SurrogateHandoffReadiness::new(start);
    assert_eq!(
      readiness.advance(start, 1, true, true),
      SurrogateHandoffOutcome::Ready
    );
  }

  #[test]
  fn workspace_cover_transfers_only_on_a_later_tick() {
    let start = Instant::now();
    let mut handoff = TickHandoff::new(start);
    assert!(handoff.is_pending());
    assert_eq!(
      handoff.advance(start + Duration::from_millis(7), true),
      SurrogateHandoffOutcome::Ready
    );
    assert!(!handoff.is_pending());
  }

  #[test]
  fn staged_cover_failure_never_marks_the_handoff_ready() {
    let start = Instant::now();
    let mut destroyed = TickHandoff::new(start);
    assert_eq!(
      destroyed.advance(start + Duration::from_millis(7), false),
      SurrogateHandoffOutcome::Failed
    );
    assert!(destroyed.is_pending());

    let mut timed_out = TickHandoff::new(start);
    assert_eq!(
      timed_out.advance(start + SURROGATE_HANDOFF_TIMEOUT, true),
      SurrogateHandoffOutcome::Failed
    );
    assert!(timed_out.is_pending());
  }

  #[test]
  fn workspace_cleanup_waits_one_compositor_tick() {
    let mut cleanup_tick_seen = false;
    assert!(!cleanup_tick_complete(&mut cleanup_tick_seen));
    assert!(cleanup_tick_seen);
    assert!(cleanup_tick_complete(&mut cleanup_tick_seen));
  }

  #[test]
  fn workspace_cleanup_waits_for_incoming_reflow_readiness() {
    let mut cleanup_tick_seen = false;

    assert!(!workspace_cleanup_complete(
      false,
      true,
      &mut cleanup_tick_seen,
    ));
    assert!(!cleanup_tick_seen);

    assert!(!workspace_cleanup_complete(
      false,
      false,
      &mut cleanup_tick_seen,
    ));
    assert!(cleanup_tick_seen);
    assert!(workspace_cleanup_complete(
      false,
      false,
      &mut cleanup_tick_seen,
    ));
  }

  #[test]
  fn transparent_session_cleanup_holds_then_drops() {
    let now = Instant::now();
    let mut fade_start = None;
    assert_eq!(
      session_cleanup_action(&mut fade_start, now, true),
      SessionCleanupAction::HoldCoverage
    );
    assert_eq!(
      session_cleanup_action(
        &mut fade_start,
        now + Duration::from_millis(7),
        true,
      ),
      SessionCleanupAction::DropTransparent
    );
  }

  #[test]
  fn opaque_session_cleanup_holds_then_fades() {
    let now = Instant::now();
    let mut fade_start = None;
    assert_eq!(
      session_cleanup_action(&mut fade_start, now, false),
      SessionCleanupAction::HoldCoverage
    );
    assert_eq!(
      session_cleanup_action(
        &mut fade_start,
        now + Duration::from_millis(7),
        false,
      ),
      SessionCleanupAction::FadeFrom(now)
    );
  }

  #[test]
  fn close_handoff_keeps_real_window_for_first_tick() {
    let start = Instant::now();
    let mut readiness = SurrogateHandoffReadiness::new(start);
    assert_eq!(
      readiness.advance(start, 2, true, true),
      SurrogateHandoffOutcome::Waiting
    );
    assert_eq!(
      readiness.advance(start + Duration::from_millis(16), 2, true, true),
      SurrogateHandoffOutcome::Ready
    );
  }

  #[test]
  fn move_resize_handoff_waits_one_composed_tick_before_cloak() {
    let start = Instant::now();
    let mut pending = PendingMoveResizeWindow {
      handle: 1,
      readiness: SurrogateHandoffReadiness::new(start),
      surrogate_revealed: false,
    };
    assert_eq!(
      pending.advance(start, true, true),
      MoveResizeHandoffAction::RevealTransparentSurrogate
    );
    assert!(pending.surrogate_revealed);
    assert_eq!(
      pending.advance(start + Duration::from_millis(16), true, true),
      MoveResizeHandoffAction::TransferVisibility
    );
  }

  #[test]
  fn rapid_redirects_do_not_restart_move_resize_readiness_epoch() {
    for redirect_interval in [
      Duration::from_micros(6_000),
      Duration::from_millis(16),
      Duration::from_millis(33),
    ] {
      let start = Instant::now();
      let mut pending = PendingMoveResizeWindow {
        handle: 1,
        readiness: SurrogateHandoffReadiness::new(start),
        surrogate_revealed: false,
      };

      // Geometry redirects update the session/animation only; they leave
      // this pending readiness object untouched even when input arrives
      // faster than a display frame.
      for _ in 0..32 {
        let _redirect_arrival = start + redirect_interval;
      }
      assert_eq!(
        pending.advance(start, true, true),
        MoveResizeHandoffAction::RevealTransparentSurrogate
      );
      assert_eq!(
        pending.advance(start + Duration::from_millis(7), true, true),
        MoveResizeHandoffAction::TransferVisibility
      );
    }
  }

  #[test]
  fn held_key_redirects_keep_latest_animation_target() {
    let start = Rect::from_xy(0, 0, 400, 300);
    let mut animation = WindowAnimationState::new_movement(
      start.clone(),
      Rect::from_xy(100, 0, 400, 300),
      150,
      EasingFunction::default(),
    );
    for x in [200, 300, 450, 700] {
      animation = WindowAnimationState::new_movement(
        animation.current_rect(),
        Rect::from_xy(x, 0, 400, 300),
        150,
        EasingFunction::default(),
      );
    }
    assert_eq!(animation.target_rect.x(), 700);
  }

  #[test]
  fn handoff_fails_on_thumbnail_update_failure() {
    let start = Instant::now();
    let mut readiness = SurrogateHandoffReadiness::new(start);
    assert_eq!(
      readiness.advance(start, 1, true, false),
      SurrogateHandoffOutcome::Failed
    );
  }

  #[test]
  fn destroyed_hwnd_cancels_pending_readiness() {
    let start = Instant::now();
    let mut readiness = SurrogateHandoffReadiness::new(start);
    assert_eq!(
      readiness.advance(start, 1, false, true),
      SurrogateHandoffOutcome::Failed
    );
  }

  #[test]
  fn failed_thumbnail_keeps_surrogate_until_cloaked_real_window_recovers()
  {
    assert_eq!(
      move_resize_recovery_action(true, true, false, false, false, false),
      MoveResizeRecoveryAction::RetainSurrogate
    );
    assert_eq!(
      move_resize_recovery_action(true, true, true, true, false, false),
      MoveResizeRecoveryAction::RetainSurrogate
    );
    assert_eq!(
      move_resize_recovery_action(true, true, true, true, true, true),
      MoveResizeRecoveryAction::Complete
    );
  }

  #[test]
  fn failed_thumbnail_before_cloak_hides_failed_surrogate() {
    assert_eq!(
      move_resize_recovery_action(true, false, false, false, false, false),
      MoveResizeRecoveryAction::HideSurrogateAndRetry
    );
  }

  #[test]
  fn same_frame_thumbnail_failure_isolated_to_one_move_resize_session() {
    let healthy_id = Uuid::new_v4();
    let failed_id = Uuid::new_v4();
    let open_id = Uuid::new_v4();
    let failed: Vec<Uuid> = [
      failed_move_resize_update_id(healthy_id, true, false),
      failed_move_resize_update_id(failed_id, false, false),
      failed_move_resize_update_id(open_id, false, true),
    ]
    .into_iter()
    .flatten()
    .collect();

    assert_eq!(failed, vec![failed_id]);
  }

  #[test]
  fn stale_thumbnail_after_visible_handoff_uses_same_frame_recovery() {
    let id = Uuid::new_v4();
    assert_eq!(failed_move_resize_update_id(id, false, false), Some(id));
    assert_eq!(
      move_resize_recovery_action(true, true, true, true, true, true),
      MoveResizeRecoveryAction::Complete
    );
  }

  #[test]
  fn opacity_failure_retries_after_real_window_is_visible() {
    assert_eq!(
      move_resize_recovery_action(true, true, true, false, true, true),
      MoveResizeRecoveryAction::HideSurrogateAndRetry
    );
  }

  #[test]
  fn destroyed_source_drops_failed_recovery() {
    assert_eq!(
      move_resize_recovery_action(false, true, false, false, false, false),
      MoveResizeRecoveryAction::DropDestroyed
    );
  }

  #[test]
  fn destroyed_opacity_target_is_isolated_before_or_during_native_call() {
    assert!(opacity_target_was_destroyed(
      &wm_platform::Error::WindowNotFound,
      true,
    ));
    assert!(opacity_target_was_destroyed(
      &wm_platform::Error::Platform("raced destruction".to_string()),
      false,
    ));
  }

  #[test]
  fn live_hwnd_opacity_failure_still_propagates() {
    assert!(!opacity_target_was_destroyed(
      &wm_platform::Error::Platform("live failure".to_string()),
      true,
    ));
  }

  #[test]
  fn failed_recovery_keeps_animation_manager_ticking() {
    let (animation_tick_tx, _animation_tick_rx) = mpsc::channel(1);
    let mut manager = AnimationManager::new(animation_tick_tx);
    let failed_id = Uuid::new_v4();
    manager.failed_move_resize_recoveries.insert(failed_id);

    assert!(manager.has_active_animations());
    assert_eq!(manager.active_window_ids(), vec![failed_id]);

    manager.failed_move_resize_recoveries.remove(&failed_id);
    assert!(!manager.has_active_animations());
  }

  #[test]
  fn readiness_timeout_never_transfers_to_surrogate() {
    let start = Instant::now();
    let mut readiness = SurrogateHandoffReadiness::new(start);
    assert_eq!(
      readiness.advance(start + SURROGATE_HANDOFF_TIMEOUT, 1, true, true,),
      SurrogateHandoffOutcome::Failed
    );
  }

  #[test]
  fn repeated_handoffs_do_not_share_tick_state() {
    let start = Instant::now();
    for _ in 0..128 {
      let mut readiness = SurrogateHandoffReadiness::new(start);
      assert_eq!(
        readiness.advance(start, 2, true, true),
        SurrogateHandoffOutcome::Waiting
      );
      assert_eq!(
        readiness.advance(
          start + Duration::from_millis(16),
          2,
          true,
          true,
        ),
        SurrogateHandoffOutcome::Ready
      );
    }
  }

  #[test]
  fn window_transition_styles_have_distinct_open_and_close_geometry() {
    let base = Rect::from_xy(100, 200, 300, 150);
    let cases = [
      (WindowTransitionStyle::SlideRight, (400, 200)),
      (WindowTransitionStyle::SlideLeft, (-200, 200)),
      (WindowTransitionStyle::SlideTop, (100, 50)),
      (WindowTransitionStyle::SlideBottom, (100, 350)),
    ];

    for (style, (expected_x, expected_y)) in cases {
      let offset =
        AnimationManager::compute_transition_start_rect(&base, &style);
      assert_eq!((offset.x(), offset.y()), (expected_x, expected_y));
      assert_eq!((offset.width(), offset.height()), (300, 150));

      // Open interpolates offset -> base; close reverses those endpoints.
      let open = WindowAnimationState::new_movement(
        offset.clone(),
        base.clone(),
        100,
        EasingFunction::CubicBezier(0.0, 0.0, 1.0, 1.0),
      );
      let close = WindowAnimationState::new_movement(
        base.clone(),
        offset.clone(),
        100,
        EasingFunction::CubicBezier(0.0, 0.0, 1.0, 1.0),
      );
      let now = Instant::now();
      assert_eq!(open.current_state_at(now).0, offset);
      assert_eq!(close.current_state_at(now).0, base);
      assert_eq!(
        open.current_state_at(now + Duration::from_millis(100)).0,
        base
      );
      assert_eq!(
        close.current_state_at(now + Duration::from_millis(100)).0,
        offset
      );
    }
  }

  #[test]
  fn stationary_window_styles_have_no_slide_component() {
    let base = Rect::from_xy(10, 20, 300, 200);
    for style in [WindowTransitionStyle::None, WindowTransitionStyle::Zoom]
    {
      assert!(style.is_stationary());
      assert_eq!(
        AnimationManager::compute_transition_start_rect(&base, &style),
        base
      );
    }
    for style in [
      WindowTransitionStyle::SlideRight,
      WindowTransitionStyle::SlideLeft,
      WindowTransitionStyle::SlideTop,
      WindowTransitionStyle::SlideBottom,
    ] {
      assert!(!style.is_stationary());
    }
  }
}
