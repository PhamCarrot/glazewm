use windows::Win32::Foundation::{HWND, RECT};

use crate::{
  native_surrogate::to_logical, resize_session::compute_border_inset,
  CornerStyle, NativeSurrogate, Rect, SurrogateBatch,
};

/// Axis along which a workspace-switch slide travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlideAxis {
  Horizontal,
  Vertical,
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn slide_offset(
  progress: f32,
  is_incoming: bool,
  direction: i32,
  distance: i32,
) -> i32 {
  if is_incoming {
    (direction as f32 * distance as f32 * (1.0 - progress)) as i32
  } else {
    (-direction as f32 * distance as f32 * progress) as i32
  }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn zoom_rect(
  rect: &Rect,
  progress: f32,
  is_incoming: bool,
) -> Option<Rect> {
  let scale = if is_incoming {
    progress
  } else {
    1.0 - progress
  };
  let half_w = (rect.width() as f32 / 2.0 * scale).round() as i32;
  let half_h = (rect.height() as f32 / 2.0 * scale).round() as i32;
  if half_w <= 0 || half_h <= 0 {
    return None;
  }

  let cx = rect.x() + rect.width() / 2;
  let cy = rect.y() + rect.height() / 2;
  Some(Rect::from_ltrb(
    cx - half_w,
    cy - half_h,
    cx + half_w,
    cy + half_h,
  ))
}

fn transition_opacity(
  opacity: u8,
  endpoint: f32,
  progress: f32,
  is_incoming: bool,
) -> u8 {
  let (start_frac, end_frac) = if is_incoming {
    (endpoint, 1.0)
  } else {
    (1.0, endpoint)
  };
  let fraction = start_frac + (end_frac - start_frac) * progress;
  (f32::from(opacity) * fraction.clamp(0.0, 1.0)).round() as u8
}

fn redirect_fade_opacity(start: u8, progress: f32) -> u8 {
  (f32::from(start) * (1.0 - progress.clamp(0.0, 1.0))).round() as u8
}

#[derive(Clone, Debug)]
struct WorkspaceSurrogateFrame {
  source: RECT,
  destination: Rect,
}

#[derive(Clone, Debug)]
struct RedirectRevealFrame {
  frame: WorkspaceSurrogateFrame,
  opacity: u8,
}

fn translated_redirect_frame(
  frame: &WorkspaceSurrogateFrame,
  viewport: &Rect,
  axis: SlideAxis,
  offset: i32,
) -> Option<WorkspaceSurrogateFrame> {
  let mut moved = frame.destination.clone();
  match axis {
    SlideAxis::Horizontal => {
      moved.left += offset;
      moved.right += offset;
    }
    SlideAxis::Vertical => {
      moved.top += offset;
      moved.bottom += offset;
    }
  }

  let clipped = Rect::from_ltrb(
    moved.left.max(viewport.left),
    moved.top.max(viewport.top),
    moved.right.min(viewport.right),
    moved.bottom.min(viewport.bottom),
  );
  if clipped.width() <= 0 || clipped.height() <= 0 {
    return None;
  }

  let destination_width = moved.width();
  let destination_height = moved.height();
  let source_width = frame.source.right - frame.source.left;
  let source_height = frame.source.bottom - frame.source.top;
  if destination_width <= 0
    || destination_height <= 0
    || source_width <= 0
    || source_height <= 0
  {
    return None;
  }

  let source_x_per_destination =
    source_width as f32 / destination_width as f32;
  let source_y_per_destination =
    source_height as f32 / destination_height as f32;
  let crop_left = clipped.left - moved.left;
  let crop_top = clipped.top - moved.top;
  let crop_right = moved.right - clipped.right;
  let crop_bottom = moved.bottom - clipped.bottom;
  let source = RECT {
    left: frame.source.left
      + (crop_left as f32 * source_x_per_destination).round() as i32,
    top: frame.source.top
      + (crop_top as f32 * source_y_per_destination).round() as i32,
    right: frame.source.right
      - (crop_right as f32 * source_x_per_destination).round() as i32,
    bottom: frame.source.bottom
      - (crop_bottom as f32 * source_y_per_destination).round() as i32,
  };

  (source.right > source.left && source.bottom > source.top).then_some(
    WorkspaceSurrogateFrame {
      source,
      destination: clipped,
    },
  )
}

/// Positioning strategy for a [`WorkspaceSurrogate`].
enum SurrogateMode {
  /// The surrogate spans the whole monitor viewport, created once and
  /// never repositioned; all motion is expressed via
  /// `rcSource`/`rcDestination` clipping, avoiding any per-frame
  /// `SetWindowPos` call. Used for windows with no live acrylic
  /// backdrop (the common case).
  PinnedViewport,
  /// The surrogate is sized to the window's own (clipped-to-monitor)
  /// visible rect and genuinely moved/resized every frame via a batched
  /// `SetWindowPos`. Used when the surrogate carries a live SWCA acrylic
  /// backdrop from GlazeWM's own `backdrop` config — pinning it to the
  /// full viewport would blur the entire monitor instead of just the
  /// window's current footprint.
  Live,
}

/// Surrogate overlay for a single window participating in a
/// workspace-switch animation.
///
/// Both outgoing and incoming windows move together so the whole workspace
/// slides as a single panel. For windows with no live backdrop
/// ([`SurrogateMode::PinnedViewport`]), the surrogate is created at the
/// full monitor rect (`viewport`) and never repositioned; all per-frame
/// animation is expressed via `rcSource`/`rcDestination` in
/// `DwmUpdateThumbnailProperties`, avoiding any per-frame `SetWindowPos`
/// calls. Windows with a live backdrop ([`SurrogateMode::Live`]) are
/// instead moved/resized every frame to their current visible rect,
/// batched via [`SurrogateBatch`]. The surrogate is hidden (via `SW_HIDE`)
/// when the visible area is empty.
pub struct WorkspaceSurrogate {
  inner: NativeSurrogate,
  /// Final screen rect of the window (target position for incoming,
  /// current screen rect for outgoing).
  pub rect: Rect,
  /// Monitor rect used as the surrogate window's fixed position and size
  /// in [`SurrogateMode::PinnedViewport`] mode.
  ///
  /// In that mode the surrogate is created at `viewport` and stays there
  /// for the entire animation; `rcDestination` coordinates are
  /// expressed relative to `viewport`'s top-left corner in every
  /// per-frame thumbnail update. In [`SurrogateMode::Live`] mode this
  /// is unused for positioning — the surrogate is moved/resized
  /// directly to its absolute screen rect — but per-frame visible-strip
  /// clipping still uses the monitor bounds passed to each `update_*`
  /// call, so a sliding window never spills onto an adjacent monitor.
  viewport: Rect,
  /// DWM thumbnail opacity (0–255) derived from the window-effects
  /// config.
  opacity: u8,
  /// The non-full-opacity end of the opacity animation, as a fraction of
  /// `opacity` (0.0–1.0).
  ///
  /// For outgoing windows this is the final opacity fraction (start =
  /// 1.0, end = `opacity_endpoint`). For incoming windows it is the
  /// initial opacity fraction (start = `opacity_endpoint`, end = 1.0).
  /// At `1.0` (default) the opacity is constant throughout the
  /// animation; at `0.0` the window fully fades out or in.
  opacity_endpoint: f32,
  /// Positioning strategy, chosen once at creation based on whether the
  /// surrogate carries a live acrylic backdrop.
  mode: SurrogateMode,
  /// Invisible border insets of the source window, in physical pixels.
  /// Zero in [`SurrogateMode::PinnedViewport`] mode. Used to offset every
  /// per-frame `rcSource` sample past the invisible border, matching the
  /// deflation already baked into `rect` and the surrogate's created
  /// size.
  border_inset: RECT,
  /// Live on-screen rect for the current animation frame, in
  /// [`SurrogateMode::Live`] mode only -- lets a caller (the acrylic blur
  /// overlay tracker in `AnimationManager::update_internal`) follow the
  /// surrogate's actual footprint instead of only its final `rect`.
  /// `None` in [`SurrogateMode::PinnedViewport`] mode (no single
  /// "screen rect" applies there), and `None` whenever the visible
  /// strip is currently empty (fully off-screen, or zoomed to nothing).
  current_rect: Option<Rect>,
  /// Thumbnail opacity captured when an incompatible rapid workspace
  /// redirect begins fading this retained coverage over the synchronized
  /// latest real workspace.
  redirect_fade_opacity: Option<u8>,
  /// Exact source/destination geometry most recently submitted to DWM.
  /// This is captured once when a directional redirect begins so the
  /// retained composition can continue from its current frame rather than
  /// being recomputed from configured animation endpoints.
  last_frame: Option<WorkspaceSurrogateFrame>,
  /// Captured directional-reveal origin. `None` for a surrogate which was
  /// already hidden when the incompatible redirect was admitted.
  redirect_reveal_frame: Option<RedirectRevealFrame>,
  /// Host visibility as last requested through this owner.
  visible: bool,
}

impl WorkspaceSurrogate {
  /// Returns the raw surrogate host HWND for batch-failure ownership.
  #[must_use]
  pub fn handle(&self) -> isize {
    self.inner.hwnd().0
  }

  /// Creates a hidden surrogate for a workspace-switch animation.
  ///
  /// `viewport` is the monitor rect; `rect` is the source window's screen
  /// rect, used as the thumbnail registration dimensions and the reference
  /// for per-frame coordinate math.
  ///
  /// `opacity_endpoint` controls how far the opacity animates away from
  /// the effect opacity. For outgoing windows pass
  /// `config.opacity_outgoing`; for incoming windows pass
  /// `config.opacity_incoming`. At `1.0` the opacity is held constant;
  /// at `0.0` the window fully fades.
  ///
  /// `corner_style` controls the DWM corner-rounding applied to the
  /// surrogate when it carries a live backdrop
  /// ([`SurrogateMode::Live`]), matching the real window's configured
  /// style. Ignored in [`SurrogateMode::PinnedViewport`] mode, where the
  /// surrogate spans the whole monitor and must not be rounded.
  ///
  /// `acrylic_tint` is GlazeWM's own configured tint
  /// (`BackdropEffectConfig::acrylic_tint`), or `None` when `backdrop`
  /// isn't configured for this window. When `Some`, this switches the
  /// surrogate to [`SurrogateMode::Live`] -- sized/moved to its own
  /// footprint rather than pinned to the viewport -- so a live-tracking
  /// acrylic blur overlay (see [`current_rect`], driven from
  /// `AnimationManager`) has a meaningful per-frame rect to follow. This
  /// no longer applies SWCA to the surrogate itself: SWCA has no
  /// adjustable blur radius, so the actual frosted-glass backdrop
  /// instead comes from the same `Windows.UI.Composition`-based overlay
  /// used in steady state, kept alive and repositioned to this
  /// surrogate's footprint for the whole slide instead of being hidden
  /// -- avoiding both the fixed-intensity mismatch and the handoff flash
  /// a separate SWCA application would cause.
  ///
  /// [`current_rect`]: WorkspaceSurrogate::current_rect
  ///
  /// The surrogate is created hidden. For outgoing windows, call
  /// [`show_initial`] before cloaking the real window to avoid a blank
  /// frame. For incoming windows in stationary styles (fade/zoom), call
  /// [`show_incoming`] before the animation loop to pre-warm the DWM
  /// thumbnail.
  ///
  /// [`show_initial`]: WorkspaceSurrogate::show_initial
  /// [`show_incoming`]: WorkspaceSurrogate::show_incoming
  pub fn new(
    hwnd: HWND,
    rect: &Rect,
    viewport: &Rect,
    opacity: u8,
    opacity_endpoint: f32,
    corner_style: &CornerStyle,
    acrylic_tint: Option<u32>,
  ) -> crate::Result<Self> {
    let carries_live_backdrop = acrylic_tint.is_some();

    let mode = if carries_live_backdrop {
      SurrogateMode::Live
    } else {
      SurrogateMode::PinnedViewport
    };

    // Live mode: surrogate is sized to the window's own rect and
    // moved/resized every frame to track it, so it should match the
    // real window's rounding. PinnedViewport mode: surrogate spans the
    // full viewport and must not be rounded.
    let (source_rect, thumbnail_rect, effective_corner_style): (
      &Rect,
      &Rect,
      &CornerStyle,
    ) = if carries_live_backdrop {
      (rect, rect, corner_style)
    } else {
      (viewport, rect, &CornerStyle::Square)
    };

    // Live mode moves/resizes the surrogate to the window's own logical
    // rect every frame, so — like resize sessions — it must be
    // deflated by the window's invisible resize border to avoid
    // overshooting into the configured gap. PinnedViewport mode sizes
    // `source_rect` to the full monitor viewport, which has no
    // invisible border of its own; deflating it here would incorrectly
    // shrink the pinned surrogate.
    let border_inset = if carries_live_backdrop {
      compute_border_inset(hwnd)
    } else {
      RECT::default()
    };

    let inner = NativeSurrogate::create(
      hwnd,
      source_rect,
      thumbnail_rect,
      None,
      opacity,
      false,
      border_inset,
      effective_corner_style,
      // Workspace surrogates should sit just below the source window;
      // they don't compete with resize surrogates since
      // workspace-switch and resize animations are mutually
      // exclusive.
      hwnd,
    )?;

    // Unlike resize sessions, a workspace surrogate has no useful direct-
    // animation fallback once the workspace switch starts. A surrogate
    // window without a registered DWM thumbnail is transparent, so
    // treating it as a valid replacement would allow the caller to
    // cloak the real window behind nothing. Fail construction and let
    // the WM use its instant-switch path.
    if !inner.has_thumbnail() {
      return Err(crate::Error::Platform(
        "Failed to register workspace surrogate thumbnail.".to_string(),
      ));
    }

    // Store the deflated (logical) rect so all per-frame positioning math
    // below operates in the same coordinate space the surrogate window was
    // actually created/sized in. A no-op when `border_inset` is zero
    // (`PinnedViewport` mode).
    let stored_rect = to_logical(rect, &border_inset);

    Ok(Self {
      inner,
      rect: stored_rect,
      viewport: viewport.clone(),
      opacity,
      opacity_endpoint: opacity_endpoint.clamp(0.0, 1.0),
      mode,
      border_inset,
      current_rect: None,
      redirect_fade_opacity: None,
      last_frame: None,
      redirect_reveal_frame: None,
      visible: false,
    })
  }

  /// Captures the current visual opacity as the start of a redirect fade.
  pub fn begin_redirect_fade(&mut self) {
    self.redirect_fade_opacity = Some(self.inner.window_opacity());
  }

  /// Fades retained rapid-switch coverage toward transparency.
  ///
  /// Returns `false` when the thumbnail update fails. The caller can then
  /// drop this obsolete coverage because the latest real workspace is
  /// already positioned and visible underneath it.
  pub fn update_redirect_fade(&mut self, progress: f32) -> bool {
    let start = self
      .redirect_fade_opacity
      .unwrap_or_else(|| self.inner.window_opacity());
    let opacity = redirect_fade_opacity(start, progress);
    self.inner.set_window_opacity(opacity)
  }

  /// Captures the exact currently submitted thumbnail frame and opacity
  /// for an incompatible directional reveal.
  ///
  /// A hidden participant contributes no pixels and therefore needs no
  /// frame. A visible host without recorded geometry is unsafe to move;
  /// returning `false` lets the caller preserve the opacity-only native
  /// fallback instead.
  pub fn begin_redirect_reveal(&mut self) -> bool {
    if !self.visible {
      self.redirect_reveal_frame = None;
      return true;
    }

    let Some(frame) = self.last_frame.clone() else {
      return false;
    };
    self.redirect_reveal_frame = Some(RedirectRevealFrame {
      frame,
      opacity: self.inner.window_opacity(),
    });
    true
  }

  /// Translates the retained frame from its captured position while
  /// fading it toward transparency.
  ///
  /// Geometry and opacity are updated without unregistering or
  /// re-registering the visible thumbnail. Live/acrylic hosts are queued
  /// into `batch`; pinned hosts remain fixed to the monitor viewport and
  /// move only their DWM destination rectangle.
  pub fn update_redirect_reveal(
    &mut self,
    axis: SlideAxis,
    direction: i32,
    distance: i32,
    progress: f32,
    batch: &mut SurrogateBatch,
  ) -> bool {
    let Some(snapshot) = self.redirect_reveal_frame.clone() else {
      return true;
    };
    let progress = progress.clamp(0.0, 1.0);
    let offset =
      (-(direction.signum()) as f32 * distance.max(0) as f32 * progress)
        .round() as i32;
    let Some(frame) = translated_redirect_frame(
      &snapshot.frame,
      &self.viewport,
      axis,
      offset,
    ) else {
      self.inner.set_visible(false);
      self.visible = false;
      self.current_rect = None;
      self.last_frame = None;
      return true;
    };

    let geometry_ok = self.apply_visible_rect_checked(
      Some(batch),
      frame.source,
      frame.destination.left,
      frame.destination.top,
      frame.destination.right,
      frame.destination.bottom,
    );
    let opacity = redirect_fade_opacity(snapshot.opacity, progress);
    geometry_ok && self.inner.set_window_opacity(opacity)
  }

  /// Live on-screen rect for the current animation frame, when in
  /// [`SurrogateMode::Live`] mode and currently visible.
  #[must_use]
  pub fn current_rect(&self) -> Option<&Rect> {
    self.current_rect.as_ref()
  }

  /// Whether this surrogate carries a live acrylic backdrop
  /// ([`SurrogateMode::Live`]), as opposed to
  /// [`SurrogateMode::PinnedViewport`].
  #[must_use]
  pub fn is_live(&self) -> bool {
    matches!(self.mode, SurrogateMode::Live)
  }

  fn hide_frame(&mut self) {
    self.inner.set_visible(false);
    self.visible = false;
    self.current_rect = None;
    self.last_frame = None;
  }

  /// Hides the DWM thumbnail without destroying it or hiding the surrogate
  /// window.
  ///
  /// Called immediately before the post-animation `DwmFlush` so the flush
  /// frame shows only the uncloaked real windows. Without this, DWM blends
  /// the thumbnail (at configured opacity) on top of the real window
  /// (also at configured opacity), producing a double-blend that appears
  /// fully opaque for one frame.
  pub fn hide_thumbnail(&mut self) {
    self.inner.set_thumbnail_visible(false);
  }

  /// Shows the surrogate at full opacity with the thumbnail at the
  /// window's natural (unscaled) position within the monitor viewport.
  ///
  /// Always uses opacity `255` (fully opaque) so the surrogate completely
  /// covers the real window before it is cloaked, avoiding a double-blend
  /// frame. Call [`apply_effect_opacity`] after the real window is cloaked
  /// to reduce the thumbnail to the configured `opacity`.
  ///
  /// [`apply_effect_opacity`]: WorkspaceSurrogate::apply_effect_opacity
  pub fn show_initial(&mut self) {
    let rc_src = RECT {
      left: self.border_inset.left,
      top: self.border_inset.top,
      right: self.border_inset.left + self.rect.width(),
      bottom: self.border_inset.top + self.rect.height(),
    };
    self.apply_visible_rect(
      None,
      rc_src,
      self.rect.left,
      self.rect.top,
      self.rect.right,
      self.rect.bottom,
    );
    self.inner.set_window_opacity(u8::MAX);
  }

  /// Updates the DWM thumbnail opacity to the configured `opacity` without
  /// changing the surrogate window position or size.
  ///
  /// Call this after the real window has been cloaked so the thumbnail's
  /// effect opacity is applied without causing a double-blend with the
  /// real window underneath.
  pub fn apply_effect_opacity(&mut self) {
    self.inner.set_window_opacity(self.opacity);
  }

  /// Shows the surrogate at the incoming animation start opacity, with the
  /// thumbnail pre-positioned at the window's location within the monitor
  /// viewport.
  ///
  /// Use for incoming windows in stationary transitions (fade, zoom) so
  /// DWM warms the thumbnail before the animation begins. The start
  /// opacity is derived from `opacity_endpoint`: `opacity_endpoint *
  /// effect_opacity`. At `opacity_endpoint = 0.0` the surrogate is
  /// invisible but the thumbnail is registered; at `1.0` it starts fully
  /// opaque. [`update_fade`] or [`update_zoom`] then drives the
  /// per-frame opacity from this initial state.
  ///
  /// [`update_fade`]: WorkspaceSurrogate::update_fade
  /// [`update_zoom`]: WorkspaceSurrogate::update_zoom
  pub fn show_incoming(&mut self) {
    let rc_src = RECT {
      left: self.border_inset.left,
      top: self.border_inset.top,
      right: self.border_inset.left + self.rect.width(),
      bottom: self.border_inset.top + self.rect.height(),
    };
    self.apply_visible_rect(
      None,
      rc_src,
      self.rect.left,
      self.rect.top,
      self.rect.right,
      self.rect.bottom,
    );
    self.inner.set_window_opacity(self.lerp_opacity(0.0, true));
  }

  /// Advances the surrogate opacity for a fade-only transition.
  ///
  /// The surrogate stays at its target rect; only the window opacity is
  /// lerped each frame to produce a crossfade without positional
  /// movement. No repositioning is needed regardless of
  /// [`SurrogateMode`] — [`show_initial`] / [`show_incoming`] already
  /// placed the surrogate at its natural rect.
  ///
  /// [`show_initial`]: WorkspaceSurrogate::show_initial
  /// [`show_incoming`]: WorkspaceSurrogate::show_incoming
  pub fn update_fade(&mut self, eased_progress: f32, is_incoming: bool) {
    self
      .inner
      .set_window_opacity(self.lerp_opacity(eased_progress, is_incoming));
  }

  /// Advances the surrogate along `axis` to `eased_progress` (0.0 → 1.0).
  ///
  /// `monitor_origin` and `monitor_size` are the axis-relevant monitor
  /// coordinates: `monitor_x`/`monitor_width` for `Horizontal`,
  /// `monitor_y`/`monitor_height` for `Vertical`. The visible strip is
  /// clipped to `[monitor_origin, monitor_origin + monitor_size]`. In
  /// [`SurrogateMode::PinnedViewport`] mode this is done via
  /// `rcSource`/`rcDestination` and the surrogate window itself does not
  /// move; in [`SurrogateMode::Live`] mode the surrogate is moved/resized
  /// to the clipped visible rect, batched via `batch`. `slide_distance`
  /// is the effective travel distance (may be less than `monitor_size`
  /// to close the seam gap between the two workspace panels).
  #[allow(clippy::too_many_arguments)]
  pub fn update_slide(
    &mut self,
    axis: SlideAxis,
    batch: &mut SurrogateBatch,
    eased_progress: f32,
    is_incoming: bool,
    direction: i32,
    monitor_origin: i32,
    monitor_size: i32,
    slide_distance: i32,
  ) {
    self.slide_axis(
      batch,
      eased_progress,
      is_incoming,
      direction,
      monitor_origin,
      monitor_size,
      slide_distance,
      axis == SlideAxis::Vertical,
    );
  }

  /// Advances the surrogate along `axis` with a simultaneous
  /// whole-workspace scale to `eased_progress` (0.0 → 1.0).
  ///
  /// Each surrogate is positioned at the scaled screen coordinates of its
  /// window (scaling from the monitor center), so the entire workspace
  /// shrinks/grows as one unit. The outgoing workspace scales from `1.0`
  /// to `1.0 - zoom_factor`; the incoming scales from `1.0 -
  /// zoom_factor` to `1.0`. `slide_distance` controls travel along
  /// `axis`.
  #[allow(clippy::too_many_arguments)]
  pub fn update_slide_zoom(
    &mut self,
    axis: SlideAxis,
    batch: &mut SurrogateBatch,
    eased_progress: f32,
    is_incoming: bool,
    direction: i32,
    monitor_x: i32,
    monitor_width: i32,
    monitor_y: i32,
    monitor_height: i32,
    slide_distance: i32,
    zoom_factor: f32,
  ) {
    self.slide_zoom_axis(
      batch,
      eased_progress,
      is_incoming,
      direction,
      monitor_x,
      monitor_width,
      monitor_y,
      monitor_height,
      slide_distance,
      zoom_factor,
      axis == SlideAxis::Vertical,
    );
  }

  /// Animates a zoom-from-center transition to `eased_progress` (0.0 →
  /// 1.0).
  ///
  /// Each surrogate independently zooms in (incoming) or out (outgoing)
  /// from its own screen-space center. In
  /// [`SurrogateMode::PinnedViewport`] mode the destination rect
  /// grows/shrinks via `rcDestination`; in [`SurrogateMode::Live`] mode
  /// the surrogate window itself is moved/resized to the scaled rect,
  /// batched via `batch`. Opacity is lerped according to
  /// `opacity_endpoint`.
  pub fn update_zoom(
    &mut self,
    batch: &mut SurrogateBatch,
    eased_progress: f32,
    is_incoming: bool,
  ) {
    let Some(destination) =
      zoom_rect(&self.rect, eased_progress, is_incoming)
    else {
      self.hide_frame();
      return;
    };

    let w = self.rect.width();
    let h = self.rect.height();

    let rc_src = RECT {
      left: self.border_inset.left,
      top: self.border_inset.top,
      right: self.border_inset.left + w,
      bottom: self.border_inset.top + h,
    };

    self.apply_visible_rect(
      Some(batch),
      rc_src,
      destination.left,
      destination.top,
      destination.right,
      destination.bottom,
    );
    self
      .inner
      .set_window_opacity(self.lerp_opacity(eased_progress, is_incoming));
  }

  /// Computes the per-frame window opacity for a surrogate at `progress`
  /// (0.0 → 1.0).
  ///
  /// Outgoing: lerps from `opacity` → `opacity_endpoint * opacity`.
  /// Incoming: lerps from `opacity_endpoint * opacity` → `opacity`.
  /// When `opacity_endpoint` is `1.0` (default), the result is constant
  /// `opacity` — no fade.
  fn lerp_opacity(&self, progress: f32, is_incoming: bool) -> u8 {
    transition_opacity(
      self.opacity,
      self.opacity_endpoint,
      progress,
      is_incoming,
    )
  }

  /// Applies a visible-rect update for one animation frame, dispatching on
  /// [`SurrogateMode`].
  ///
  /// `rc_src` is the source-window-local rect to sample from. `vis_left`,
  /// `vis_top`, `vis_right`, `vis_bottom` are the visible destination rect
  /// in absolute screen coordinates.
  ///
  /// In [`SurrogateMode::PinnedViewport`] mode this only updates the DWM
  /// thumbnail's `rcDestination` (expressed relative to `self.viewport`);
  /// the surrogate window itself never moves. In [`SurrogateMode::Live`]
  /// mode the thumbnail fills the surrogate's entire (resized) client
  /// area, and the surrogate window is moved/resized to `[vis_left,
  /// vis_top, vis_right, vis_bottom]` — immediately via `reposition`
  /// when `batch` is `None` (initial placement, before the animation
  /// loop starts), or queued into `batch` for an atomic multi-surrogate
  /// commit otherwise.
  fn apply_visible_rect(
    &mut self,
    batch: Option<&mut SurrogateBatch>,
    rc_src: RECT,
    vis_left: i32,
    vis_top: i32,
    vis_right: i32,
    vis_bottom: i32,
  ) {
    let _ = self.apply_visible_rect_checked(
      batch, rc_src, vis_left, vis_top, vis_right, vis_bottom,
    );
  }

  fn apply_visible_rect_checked(
    &mut self,
    batch: Option<&mut SurrogateBatch>,
    rc_src: RECT,
    vis_left: i32,
    vis_top: i32,
    vis_right: i32,
    vis_bottom: i32,
  ) -> bool {
    let destination =
      Rect::from_ltrb(vis_left, vis_top, vis_right, vis_bottom);
    let thumbnail_ok = match self.mode {
      SurrogateMode::PinnedViewport => {
        let rc_dst = RECT {
          left: vis_left - self.viewport.left,
          top: vis_top - self.viewport.top,
          right: vis_right - self.viewport.left,
          bottom: vis_bottom - self.viewport.top,
        };
        self.inner.set_thumbnail_rects(rc_src, rc_dst)
      }
      SurrogateMode::Live => {
        let w = vis_right - vis_left;
        let h = vis_bottom - vis_top;
        let rc_dst = RECT {
          left: 0,
          top: 0,
          right: w,
          bottom: h,
        };
        let thumbnail_ok = self.inner.set_thumbnail_rects(rc_src, rc_dst);

        match batch {
          Some(b) => self.inner.defer_reposition(b, &destination),
          None => {
            let _ = self.inner.reposition(&destination);
          }
        }
        self.current_rect = Some(destination.clone());
        thumbnail_ok
      }
    };
    self.last_frame = Some(WorkspaceSurrogateFrame {
      source: rc_src,
      destination,
    });
    self.inner.set_visible(true);
    self.visible = true;
    thumbnail_ok
  }

  /// Shared implementation for [`update_slide_zoom`].
  ///
  /// Computes a per-frame scale from the monitor center (both axes)
  /// combined with a slide offset on the primary axis (`is_vertical`
  /// selects which), then routes the resulting visible rect through
  /// [`apply_visible_rect`].
  ///
  /// [`update_slide_zoom`]: WorkspaceSurrogate::update_slide_zoom
  #[allow(clippy::too_many_arguments)]
  fn slide_zoom_axis(
    &mut self,
    batch: &mut SurrogateBatch,
    eased_progress: f32,
    is_incoming: bool,
    direction: i32,
    monitor_x: i32,
    monitor_width: i32,
    monitor_y: i32,
    monitor_height: i32,
    slide_distance: i32,
    zoom_factor: f32,
    is_vertical: bool,
  ) {
    // Outgoing: scale 1.0 → (1 - zoom_factor) as it exits.
    // Incoming: scale (1 - zoom_factor) → 1.0 as it enters.
    let zoom_t = if is_incoming {
      1.0 - eased_progress
    } else {
      eased_progress
    };
    let scale = 1.0 - zoom_factor * zoom_t;

    if scale <= 0.0 {
      self.hide_frame();
      return;
    }

    // Slide offset on the primary axis.
    let slide_offset =
      slide_offset(eased_progress, is_incoming, direction, slide_distance);

    // Zoom all four edges from the monitor center.
    let cx = monitor_x + monitor_width / 2;
    let cy = monitor_y + monitor_height / 2;
    let zoomed_left =
      cx + ((self.rect.left - cx) as f32 * scale).round() as i32;
    let zoomed_top =
      cy + ((self.rect.top - cy) as f32 * scale).round() as i32;
    let zoomed_right =
      cx + ((self.rect.right - cx) as f32 * scale).round() as i32;
    let zoomed_bottom =
      cy + ((self.rect.bottom - cy) as f32 * scale).round() as i32;

    // Apply slide offset on the primary axis only.
    let (final_left, final_top, final_right, final_bottom) = if is_vertical
    {
      (
        zoomed_left,
        zoomed_top + slide_offset,
        zoomed_right,
        zoomed_bottom + slide_offset,
      )
    } else {
      (
        zoomed_left + slide_offset,
        zoomed_top,
        zoomed_right + slide_offset,
        zoomed_bottom,
      )
    };

    // Clip to monitor bounds.
    let monitor_right = monitor_x + monitor_width;
    let monitor_bottom = monitor_y + monitor_height;
    let vis_left = final_left.max(monitor_x);
    let vis_top = final_top.max(monitor_y);
    let vis_right = final_right.min(monitor_right);
    let vis_bottom = final_bottom.min(monitor_bottom);

    if vis_left >= vis_right || vis_top >= vis_bottom {
      self.hide_frame();
      return;
    }

    // Map the visible screen area back to source-window coordinates.
    // screen_x = final_left + src_x * scale  →  src_x = (screen_x -
    // final_left) / scale
    let ww = self.rect.right - self.rect.left;
    let wh = self.rect.bottom - self.rect.top;
    let src_left = (((vis_left - final_left) as f32 / scale).round()
      as i32)
      .clamp(0, ww);
    let src_top =
      (((vis_top - final_top) as f32 / scale).round() as i32).clamp(0, wh);
    let src_right = (((vis_right - final_left) as f32 / scale).round()
      as i32)
      .clamp(0, ww);
    let src_bottom = (((vis_bottom - final_top) as f32 / scale).round()
      as i32)
      .clamp(0, wh);

    let rc_src = RECT {
      left: self.border_inset.left + src_left,
      top: self.border_inset.top + src_top,
      right: self.border_inset.left + src_right,
      bottom: self.border_inset.top + src_bottom,
    };
    self.apply_visible_rect(
      Some(batch),
      rc_src,
      vis_left,
      vis_top,
      vis_right,
      vis_bottom,
    );
    self
      .inner
      .set_window_opacity(self.lerp_opacity(eased_progress, is_incoming));
  }

  /// Shared implementation for [`update_slide`].
  ///
  /// The visible strip of source content, clipped to
  /// `[monitor_origin, monitor_origin + monitor_size]`, is routed through
  /// [`apply_visible_rect`].
  ///
  /// [`update_slide`]: WorkspaceSurrogate::update_slide
  #[allow(clippy::too_many_arguments)]
  fn slide_axis(
    &mut self,
    batch: &mut SurrogateBatch,
    eased_progress: f32,
    is_incoming: bool,
    direction: i32,
    monitor_origin: i32,
    monitor_size: i32,
    slide_distance: i32,
    is_vertical: bool,
  ) {
    // Incoming: start at +direction*slide_distance offset, end at 0.
    // Outgoing: start at 0, end at -direction*slide_distance offset.
    let offset =
      slide_offset(eased_progress, is_incoming, direction, slide_distance);

    // Axis-dependent dimensions.
    let (axis_pos, perp_pos, axis_size, perp_size) = if is_vertical {
      (
        self.rect.y(),
        self.rect.x(),
        self.rect.height(),
        self.rect.width(),
      )
    } else {
      (
        self.rect.x(),
        self.rect.y(),
        self.rect.width(),
        self.rect.height(),
      )
    };

    let current = axis_pos + offset;
    let monitor_end = monitor_origin + monitor_size;

    // Visible strip of this window along the sliding axis.
    let vis_start = current.max(monitor_origin);
    let vis_end = (current + axis_size).min(monitor_end);

    if vis_start >= vis_end {
      self.hide_frame();
      return;
    }

    // Source-window-local start of the visible strip.
    let src_start = vis_start - current;
    let constrained = vis_end - vis_start;

    // `rcSource` is the visible slice of the source window. The visible
    // destination rect is expressed in absolute screen coordinates and
    // routed through `apply_visible_rect`.
    let (mut rc_src, vis_left, vis_top, vis_right, vis_bottom) =
      if is_vertical {
        (
          RECT {
            left: 0,
            top: src_start,
            right: perp_size,
            bottom: src_start + constrained,
          },
          perp_pos,
          vis_start,
          perp_pos + perp_size,
          vis_end,
        )
      } else {
        (
          RECT {
            left: src_start,
            top: 0,
            right: src_start + constrained,
            bottom: perp_size,
          },
          vis_start,
          perp_pos,
          vis_end,
          perp_pos + perp_size,
        )
      };
    // `rc_src` above is 0-based in the source window's logical content
    // space; offset past the invisible border to land in the window's
    // true physical pixel space (matches the offset baked into the
    // initial registration in `NativeSurrogate::create`).
    rc_src.left += self.border_inset.left;
    rc_src.right += self.border_inset.left;
    rc_src.top += self.border_inset.top;
    rc_src.bottom += self.border_inset.top;
    self.apply_visible_rect(
      Some(batch),
      rc_src,
      vis_left,
      vis_top,
      vis_right,
      vis_bottom,
    );
    self
      .inner
      .set_window_opacity(self.lerp_opacity(eased_progress, is_incoming));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn workspace_slide_direction_and_endpoints_are_distinct() {
    for direction in [-1, 1] {
      assert_eq!(
        slide_offset(0.0, true, direction, 1_000),
        direction * 1_000
      );
      assert_eq!(slide_offset(1.0, true, direction, 1_000), 0);
      assert_eq!(slide_offset(0.0, false, direction, 1_000), 0);
      assert_eq!(
        slide_offset(1.0, false, direction, 1_000),
        -direction * 1_000
      );
    }
  }

  #[test]
  fn workspace_zoom_scales_about_each_window_center() {
    let rect = Rect::from_xy(100, 200, 400, 200);
    assert_eq!(zoom_rect(&rect, 0.0, true), None);
    assert_eq!(
      zoom_rect(&rect, 0.5, true),
      Some(Rect::from_xy(200, 250, 200, 100))
    );
    assert_eq!(zoom_rect(&rect, 1.0, true), Some(rect.clone()));

    assert_eq!(zoom_rect(&rect, 0.0, false), Some(rect.clone()));
    assert_eq!(
      zoom_rect(&rect, 0.5, false),
      Some(Rect::from_xy(200, 250, 200, 100))
    );
    assert_eq!(zoom_rect(&rect, 1.0, false), None);
  }

  #[test]
  fn workspace_fade_opacity_has_exact_incoming_and_outgoing_endpoints() {
    assert_eq!(transition_opacity(200, 0.25, 0.0, true), 50);
    assert_eq!(transition_opacity(200, 0.25, 1.0, true), 200);
    assert_eq!(transition_opacity(200, 0.25, 0.0, false), 200);
    assert_eq!(transition_opacity(200, 0.25, 1.0, false), 50);
  }

  #[test]
  fn redirect_fade_opacity_is_bounded_and_reaches_transparency() {
    assert_eq!(redirect_fade_opacity(200, -1.0), 200);
    assert_eq!(redirect_fade_opacity(200, 0.5), 100);
    assert_eq!(redirect_fade_opacity(200, 1.0), 0);
    assert_eq!(redirect_fade_opacity(200, 2.0), 0);
  }

  fn test_redirect_frame(rect: Rect) -> WorkspaceSurrogateFrame {
    WorkspaceSurrogateFrame {
      source: RECT {
        left: 0,
        top: 0,
        right: rect.width(),
        bottom: rect.height(),
      },
      destination: rect,
    }
  }

  #[test]
  fn redirect_reveal_first_frame_is_exactly_the_last_submitted_frame() {
    let viewport = Rect::from_xy(0, 0, 1_000, 800);
    let original = test_redirect_frame(Rect::from_xy(100, 200, 400, 300));
    let translated = translated_redirect_frame(
      &original,
      &viewport,
      SlideAxis::Horizontal,
      0,
    )
    .unwrap();

    assert_eq!(translated.destination, original.destination);
    assert_eq!(translated.source.left, original.source.left);
    assert_eq!(translated.source.top, original.source.top);
    assert_eq!(translated.source.right, original.source.right);
    assert_eq!(translated.source.bottom, original.source.bottom);
  }

  #[test]
  fn redirects_at_50_100_and_200_ms_never_reset_to_configured_endpoints() {
    let viewport = Rect::from_xy(0, 0, 1_920, 1_080);
    let configured_start = 500;
    let configured_end = 100;

    for elapsed_ms in [50_i32, 100, 200] {
      let progress = elapsed_ms as f32 / 360.0;
      let current_left = (configured_start as f32
        + (configured_end - configured_start) as f32 * progress)
        .round() as i32;
      let captured =
        test_redirect_frame(Rect::from_xy(current_left, 100, 400, 600));
      let first_reveal = translated_redirect_frame(
        &captured,
        &viewport,
        SlideAxis::Horizontal,
        0,
      )
      .unwrap();

      assert_eq!(first_reveal.destination, captured.destination);
      assert_ne!(first_reveal.destination.left, configured_start);
      assert_ne!(first_reveal.destination.left, configured_end);
    }
  }

  #[test]
  fn redirect_reveal_applies_one_common_monotonic_translation() {
    let viewport = Rect::from_xy(0, 0, 1_000, 800);
    let first = test_redirect_frame(Rect::from_xy(300, 100, 200, 200));
    let second = test_redirect_frame(Rect::from_xy(550, 100, 200, 200));
    let offsets = [0, -40, -80, -120];
    let mut previous_left = first.destination.left;

    for offset in offsets {
      let moved_first = translated_redirect_frame(
        &first,
        &viewport,
        SlideAxis::Horizontal,
        offset,
      )
      .unwrap();
      let moved_second = translated_redirect_frame(
        &second,
        &viewport,
        SlideAxis::Horizontal,
        offset,
      )
      .unwrap();
      assert_eq!(
        moved_first.destination.left - first.destination.left,
        moved_second.destination.left - second.destination.left
      );
      assert!(moved_first.destination.left <= previous_left);
      previous_left = moved_first.destination.left;
    }
  }

  #[test]
  fn redirect_reveal_clips_from_the_captured_source_without_endpoint_reset(
  ) {
    let viewport = Rect::from_xy(0, 0, 1_000, 800);
    let original = test_redirect_frame(Rect::from_xy(100, 200, 400, 300));
    let translated = translated_redirect_frame(
      &original,
      &viewport,
      SlideAxis::Horizontal,
      -200,
    )
    .unwrap();

    assert_eq!(translated.destination, Rect::from_xy(0, 200, 300, 300));
    assert_eq!(translated.source.left, 100);
    assert_eq!(translated.source.right, 400);
    assert_eq!(translated.source.top, 0);
    assert_eq!(translated.source.bottom, 300);
  }

  #[test]
  fn redirect_reveal_opacity_never_increases() {
    let values: Vec<u8> = [0.0, 0.1, 0.35, 0.7, 1.0]
      .into_iter()
      .map(|progress| redirect_fade_opacity(213, progress))
      .collect();
    assert!(values.windows(2).all(|pair| pair[1] <= pair[0]));
    assert_eq!(values[0], 213);
    assert_eq!(values.last(), Some(&0));
  }
}
