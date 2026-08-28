//! `Windows.UI.Composition` based acrylic blur pipeline.
//!
//! Replaces SWCA's fixed-intensity `ACCENT_ENABLE_ACRYLICBLURBEHIND` with
//! a host-backdrop brush fed through a hand-implemented Gaussian-blur
//! effect graph, giving a continuously adjustable blur radius, plus a
//! `CompositionRoundedRectangleGeometry` clip for a continuous corner
//! radius -- neither of which SWCA/Mica can provide. Validated in
//! `packages/composition-blur-spike` before this port; see that crate's
//! module docs for the concrete API findings this implementation relies
//! on.
//!
//! # Threading
//!
//! A `Compositor` must be created on a thread that owns a dispatcher
//! queue, and (confirmed empirically in the spike, not just per docs) that
//! thread must keep pumping messages for async composition work -- e.g.
//! the effect factory's shader-graph compile -- to ever complete.
//! `wm`'s main loop drives everything through `tokio::select!`/
//! `rt.block_on`, which never pumps Win32 messages, so the entire
//! composition pipeline (the `Compositor` itself, and every per-overlay
//! visual-tree build, which touches the same async-sensitive effect
//! factory) is constructed on a dedicated, self-pumping OS thread obtained
//! via `DispatcherQueueController::CreateOnDedicatedThread`.
//!
//! Once created, `Compositor` and every composition object handed back to
//! callers are documented `WinRT` "agile" objects (`windows-rs` applies
//! `unsafe impl Send + Sync` to each of them) -- so the frequent per-tick
//! property updates (`set_rect`, `set_tint`, `set_blur_amount`,
//! `set_corner_radius`, `set_opacity`, `set_saturation`) call directly
//! into them from the caller's thread with no cross-thread marshaling,
//! keeping the hot path exactly as cheap as the SWCA path it replaces.

use std::{
  cell::RefCell,
  sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc, Mutex, MutexGuard, OnceLock,
  },
};

use windows::{
  core::{implement, ComInterface, GUID, HSTRING, PCWSTR},
  Foundation::{Numerics::Vector2, PropertyValue},
  Graphics::Effects::{
    IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectSource_Impl,
    IGraphicsEffect_Impl,
  },
  System::{
    DispatcherQueue, DispatcherQueueController, DispatcherQueueHandler,
  },
  Win32::{
    Foundation::{E_FAIL, E_INVALIDARG, HWND},
    System::WinRT::{
      Composition::ICompositorDesktopInterop,
      Graphics::Direct2D::{
        IGraphicsEffectD2D1Interop, IGraphicsEffectD2D1Interop_Impl,
        GRAPHICS_EFFECT_PROPERTY_MAPPING,
        GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT,
      },
    },
  },
  UI::{
    Color,
    Composition::{
      CompositionBackdropBrush, CompositionColorBrush,
      CompositionEffectBrush, CompositionEffectSourceParameter,
      CompositionRoundedRectangleGeometry, Compositor, ContainerVisual,
      Desktop::DesktopWindowTarget, SpriteVisual,
    },
  },
};

use crate::{BlurOverlayParams, Rect};

static COMPOSITION_READY: tokio::sync::Notify =
  tokio::sync::Notify::const_new();

pub(crate) const MAX_PENDING_COMPOSITION_OVERLAYS: usize = 128;

pub(crate) fn composition_admission_available(
  pending_count: usize,
) -> bool {
  pending_count < MAX_PENDING_COMPOSITION_OVERLAYS
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) async fn wait_for_composition_ready() {
  COMPOSITION_READY.notified().await;
}

/// `CLSID_D2D1GaussianBlur`, the built-in D2D1 Gaussian-blur effect.
const CLSID_D2D1_GAUSSIAN_BLUR: GUID =
  GUID::from_u128(0x1feb_6d69_2fe6_4ac9_8c58_1d7f_93e7_a6a5);

/// `D2D1_GAUSSIANBLUR_OPTIMIZATION_BALANCED`.
const D2D1_GAUSSIANBLUR_OPTIMIZATION_BALANCED: u32 = 1;
/// `D2D1_BORDER_MODE_SOFT`.
const D2D1_BORDER_MODE_SOFT: u32 = 0;

/// `CLSID_D2D1Saturation`, the built-in D2D1 saturation-adjustment effect.
/// Value matches
/// `windows::Win32::Graphics::Direct2D::CLSID_D2D1Saturation` (re-declared
/// as a local `const` so it sits next to `CLSID_D2D1_GAUSSIAN_BLUR` and
/// follows this module's naming convention).
const CLSID_D2D1_SATURATION: GUID =
  GUID::from_u128(0x5cb2_d9cf_327d_459f_a0ce_40c0_b208_6bf7);

/// Hand-implemented D2D1 effect description with exactly one
/// runtime-adjustable scalar property (at index 0), plus any additional
/// fixed (non-adjustable) properties a given effect's D2D1 schema requires
/// but this code never tunes.
///
/// `Compositor::CreateEffectFactory` takes an `IGraphicsEffect` describing
/// an effect graph. `Win2D`'s convenience effect types
/// (`GaussianBlurEffect`, etc.) require the `Win2D` winmd, which
/// `windows-rs`'s metadata-driven binding generator cannot consume -- so
/// this hand-implements the `IGraphicsEffectD2D1Interop` COM shape
/// directly against D2D1's built-in effects, the same approach Microsoft's
/// own `Windows.UI.Composition-Win32-Samples` uses in C++. One instance of
/// this type is used per built-in effect ([`CLSID_D2D1_GAUSSIAN_BLUR`],
/// [`CLSID_D2D1_SATURATION`]) chained in `build_effect_brush`.
#[implement(
  IGraphicsEffect,
  IGraphicsEffectSource,
  IGraphicsEffectD2D1Interop
)]
struct D2d1ScalarEffect {
  effect_id: GUID,
  source: IGraphicsEffectSource,
  /// Name of the single runtime-adjustable scalar property, matched
  /// against the dotted/bare property name in `GetNamedPropertyMapping`
  /// (see its doc comment for why both forms are checked).
  property_name: &'static str,
  /// Initial value baked into the effect graph at factory creation.
  /// Runtime adjustment rebuilds the whole brush rather than mutating
  /// this in place -- see `BlurVisual::set_blur_amount`'s doc comment
  /// for why.
  initial_value: f32,
  /// Additional fixed `u32` properties required by the effect's D2D1
  /// schema, in index order starting at index 1 (index 0 is always
  /// `initial_value`). Empty for saturation; Gaussian blur needs
  /// `[D2D1_GAUSSIANBLUR_OPTIMIZATION_BALANCED, D2D1_BORDER_MODE_SOFT]`
  /// -- `CreateEffectFactory` validates the description against D2D1's
  /// registered schema for the effect and fails with `E_INVALIDARG`
  /// unless all of them are present, even though only the scalar is
  /// runtime-adjustable here.
  extra_properties: &'static [u32],
  name: RefCell<HSTRING>,
}

impl D2d1ScalarEffect {
  fn new(
    effect_id: GUID,
    effect_name: &str,
    source: IGraphicsEffectSource,
    property_name: &'static str,
    initial_value: f32,
    extra_properties: &'static [u32],
  ) -> Self {
    Self {
      effect_id,
      source,
      property_name,
      initial_value,
      extra_properties,
      name: RefCell::new(HSTRING::from(effect_name)),
    }
  }
}

impl IGraphicsEffectSource_Impl for D2d1ScalarEffect {}

impl IGraphicsEffect_Impl for D2d1ScalarEffect {
  fn Name(&self) -> windows::core::Result<HSTRING> {
    Ok(self.name.borrow().clone())
  }

  fn SetName(&self, name: &HSTRING) -> windows::core::Result<()> {
    *self.name.borrow_mut() = name.clone();
    Ok(())
  }
}

impl IGraphicsEffectD2D1Interop_Impl for D2d1ScalarEffect {
  fn GetEffectId(&self) -> windows::core::Result<GUID> {
    Ok(self.effect_id)
  }

  fn GetNamedPropertyMapping(
    &self,
    name: &PCWSTR,
    index: *mut u32,
    mapping: *mut GRAPHICS_EFFECT_PROPERTY_MAPPING,
  ) -> windows::core::Result<()> {
    // SAFETY: `name` is a valid, null-terminated wide string for the
    // duration of this call, per the WinRT effect-description contract.
    let name = unsafe { name.to_string() }.unwrap_or_default();

    // Observed empirically (not documented): the composition engine calls
    // this with the *dotted* `"<Name>.<property>"` path, not the bare
    // property name alone. Match on the segment after the last `.` so this
    // works regardless of which convention is actually in play (both
    // dotted and bare names have been seen recommended in different
    // Microsoft samples).
    let property_name = name.rsplit('.').next().unwrap_or(&name);

    if property_name == self.property_name {
      // SAFETY: `index`/`mapping` are valid out-parameters supplied by the
      // composition engine for this call.
      unsafe {
        *index = 0;
        *mapping = GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT;
      }
      Ok(())
    } else {
      Err(windows::core::Error::from(E_INVALIDARG))
    }
  }

  fn GetPropertyCount(&self) -> windows::core::Result<u32> {
    #[allow(clippy::cast_possible_truncation)]
    Ok(1 + self.extra_properties.len() as u32)
  }

  fn GetProperty(
    &self,
    index: u32,
  ) -> windows::core::Result<windows::Foundation::IPropertyValue> {
    if index == 0 {
      return PropertyValue::CreateSingle(self.initial_value)?.cast();
    }

    match self.extra_properties.get((index - 1) as usize) {
      Some(&value) => PropertyValue::CreateUInt32(value)?.cast(),
      None => Err(windows::core::Error::from(E_INVALIDARG)),
    }
  }

  fn GetSource(
    &self,
    index: u32,
  ) -> windows::core::Result<IGraphicsEffectSource> {
    if index == 0 {
      Ok(self.source.clone())
    } else {
      Err(windows::core::Error::from(E_INVALIDARG))
    }
  }

  fn GetSourceCount(&self) -> windows::core::Result<u32> {
    Ok(1)
  }
}

/// The dedicated, self-pumping composition thread and its `Compositor`.
struct CompositionThread {
  /// Kept alive for the process's lifetime: dropping this tears down the
  /// dedicated thread and its dispatcher queue, which would stall every
  /// composition object's async completions (see the module docs).
  _controller: DispatcherQueueController,
  queue: DispatcherQueue,
}

static COMPOSITION_THREAD: OnceLock<Option<CompositionThread>> =
  OnceLock::new();
static COMPOSITION_WARMUP_QUEUED: AtomicBool = AtomicBool::new(false);

enum ActorCompositorState {
  Uninitialized,
  Ready(Compositor),
  Failed,
}

thread_local! {
  static ACTOR_COMPOSITOR: RefCell<ActorCompositorState> =
    const { RefCell::new(ActorCompositorState::Uninitialized) };
}

fn actor_compositor() -> windows::core::Result<Compositor> {
  ACTOR_COMPOSITOR.with(|state| {
    let mut state = state.borrow_mut();
    match &*state {
      ActorCompositorState::Ready(compositor) => Ok(compositor.clone()),
      ActorCompositorState::Failed => Err(windows::core::Error::new(
        E_FAIL,
        "Composition actor initialization previously failed.".into(),
      )),
      ActorCompositorState::Uninitialized => match Compositor::new() {
        Ok(compositor) => {
          *state = ActorCompositorState::Ready(compositor.clone());
          Ok(compositor)
        }
        Err(error) => {
          *state = ActorCompositorState::Failed;
          Err(error)
        }
      },
    }
  })
}

fn warm_composition_actor() -> windows::core::Result<()> {
  let compositor = actor_compositor()?;
  let backdrop = compositor.CreateHostBackdropBrush()?;
  let _ = build_effect_brush(&compositor, &backdrop, 30.0, 1.0)?;
  Ok(())
}

/// Lazily initializes the composition thread on first use, caching failure
/// too (as `None`) so later overlay creations don't retry an unavailable
/// pipeline on every call. Falls back to the SWCA path on failure.
fn composition_thread() -> Option<&'static CompositionThread> {
  COMPOSITION_THREAD
    .get_or_init(|| match init_composition_thread() {
      Ok(thread) => Some(thread),
      Err(err) => {
        tracing::warn!(
          "Composition-based acrylic blur unavailable, falling back to \
           SWCA: {err}"
        );
        None
      }
    })
    .as_ref()
}

/// Starts the composition actor and queues an optional warmup before the
/// WM accepts native or IPC events. It never waits for `Compositor::new`.
pub(crate) fn initialize_composition_actor() -> bool {
  let Some(thread) = composition_thread() else {
    return false;
  };
  if COMPOSITION_WARMUP_QUEUED
    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
    .is_ok()
  {
    let handler = DispatcherQueueHandler::new(move || {
      if let Err(error) = warm_composition_actor() {
        tracing::warn!(
          "Composition actor warmup failed; retaining SWCA acrylic: {error}."
        );
      }
      Ok(())
    });
    match thread.queue.TryEnqueue(&handler) {
      Ok(true) => {}
      Ok(false) => {
        COMPOSITION_WARMUP_QUEUED.store(false, Ordering::Release);
        tracing::warn!(
          "Composition actor rejected warmup; retaining SWCA acrylic."
        );
      }
      Err(error) => {
        COMPOSITION_WARMUP_QUEUED.store(false, Ordering::Release);
        tracing::warn!(
          "Composition actor warmup queue failed; retaining SWCA acrylic: \
           {error}."
        );
      }
    }
  }
  true
}

/// Queues one per-overlay visual-tree build without waiting on the caller.
/// The actor owns the expensive effect-factory work; the returned receiver
/// is polled by the WM on a normal readiness wakeup.
pub(crate) fn queue_blur_visual(
  hwnd: HWND,
  rect: Rect,
  params: BlurOverlayParams,
) -> crate::Result<mpsc::Receiver<windows::core::Result<BlurVisual>>> {
  let thread = COMPOSITION_THREAD
    .get()
    .and_then(Option::as_ref)
    .ok_or_else(|| {
      crate::Error::Platform(
        "Composition actor was not initialized before event admission."
          .to_string(),
      )
    })?;
  let queue = thread.queue.clone();
  let actor_queue = queue.clone();
  let hwnd_raw = hwnd.0;
  let (sender, receiver) = mpsc::sync_channel(1);
  let mut work = Some(move || {
    let result = actor_compositor().and_then(|compositor| {
      build_visual_tree(
        &compositor,
        &actor_queue,
        HWND(hwnd_raw),
        &rect,
        params,
      )
    });
    let _ = sender.send(result);
    COMPOSITION_READY.notify_one();
  });
  let handler = DispatcherQueueHandler::new(move || {
    if let Some(work) = work.take() {
      work();
    }
    Ok(())
  });
  ensure_composition_work_queued(thread.queue.TryEnqueue(&handler)?)?;
  Ok(receiver)
}

fn init_composition_thread() -> crate::Result<CompositionThread> {
  let controller = DispatcherQueueController::CreateOnDedicatedThread()?;
  let queue = controller.DispatcherQueue()?;
  Ok(CompositionThread {
    _controller: controller,
    queue,
  })
}

fn ensure_composition_work_queued(queued: bool) -> crate::Result<()> {
  if queued {
    Ok(())
  } else {
    Err(crate::Error::Platform(
      "Composition dispatcher rejected queued work.".to_string(),
    ))
  }
}

/// Unpacks an ABGR-packed tint (see `BackdropEffectConfig::acrylic_tint`)
/// into a `windows::UI::Color`.
#[allow(clippy::cast_possible_truncation)]
fn unpack_abgr_tint(tint: u32) -> Color {
  Color {
    A: (tint >> 24) as u8,
    B: (tint >> 16) as u8,
    G: (tint >> 8) as u8,
    R: tint as u8,
  }
}

/// A live `Windows.UI.Composition` visual tree providing an acrylic blur
/// overlay's rendering: a live host-backdrop brush, blurred through a
/// Gaussian-blur effect graph, with a tint layer composited on top, both
/// clipped to a continuous rounded rectangle.
pub(crate) struct BlurVisual {
  /// Binds the visual tree to the overlay's `HWND`. Kept alive but never
  /// touched again -- dropping it would unbind composition from the
  /// window.
  _target: DesktopWindowTarget,

  /// Retained (rather than just used during `create`) so
  /// `set_blur_amount`/`set_saturation` can rebuild the effect brush --
  /// see `set_blur_amount`'s doc comment for why a rebuild, not an
  /// in-place property update, is used.
  compositor: Compositor,
  queue: DispatcherQueue,
  host_backdrop: CompositionBackdropBrush,
  root: ContainerVisual,
  blur_sprite: SpriteVisual,
  tint_sprite: SpriteVisual,

  tint_brush: CompositionColorBrush,
  rounded_geometry: CompositionRoundedRectangleGeometry,

  /// Current blur amount, kept alongside `saturation` so either setter
  /// can rebuild the full effect graph using the other's current value.
  blur_amount: f32,
  /// Current saturation. See `blur_amount`.
  saturation: f32,
  effect_updates: Arc<EffectUpdateSlot>,
}

#[derive(Clone, Copy)]
struct EffectUpdateRequest {
  generation: u64,
  blur_amount: f32,
  saturation: f32,
}

struct EffectUpdateSlot {
  latest: Mutex<EffectUpdateRequest>,
  next_generation: AtomicU64,
  queued: AtomicBool,
  alive: AtomicBool,
  failure_count: AtomicU64,
}

impl EffectUpdateSlot {
  fn new(blur_amount: f32, saturation: f32) -> Self {
    Self {
      latest: Mutex::new(EffectUpdateRequest {
        generation: 0,
        blur_amount,
        saturation,
      }),
      next_generation: AtomicU64::new(1),
      queued: AtomicBool::new(false),
      alive: AtomicBool::new(true),
      failure_count: AtomicU64::new(0),
    }
  }

  fn rate_limited_failure(&self) -> Option<u64> {
    let previous = self
      .failure_count
      .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        value.checked_add(1)
      })
      .ok()?;
    let occurrence = previous + 1;
    (occurrence == 1 || occurrence.is_multiple_of(64))
      .then_some(occurrence)
  }

  fn publish(&self, blur_amount: f32, saturation: f32) -> bool {
    let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
    *lock_unpoisoned(&self.latest) = EffectUpdateRequest {
      generation,
      blur_amount,
      saturation,
    };
    self
      .queued
      .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
  }
}

fn enqueue_effect_rebuild(
  queue: &DispatcherQueue,
  compositor: &Compositor,
  host_backdrop: &CompositionBackdropBrush,
  blur_sprite: &SpriteVisual,
  updates: &Arc<EffectUpdateSlot>,
) -> crate::Result<()> {
  let queued_updates = Arc::clone(&updates);
  let queued_queue = queue.clone();
  let queued_compositor = compositor.clone();
  let queued_backdrop = host_backdrop.clone();
  let queued_sprite = blur_sprite.clone();
  let handler = DispatcherQueueHandler::new(move || {
    let request = *lock_unpoisoned(&queued_updates.latest);

    if queued_updates.alive.load(Ordering::Acquire) {
      match build_effect_brush(
        &queued_compositor,
        &queued_backdrop,
        request.blur_amount,
        request.saturation,
      ) {
        Ok(brush) => {
          let still_latest = lock_unpoisoned(&queued_updates.latest)
            .generation
            == request.generation;
          if still_latest && queued_updates.alive.load(Ordering::Acquire) {
            if let Err(error) = queued_sprite.SetBrush(&brush) {
              if let Some(failure_count) =
                queued_updates.rate_limited_failure()
              {
                tracing::warn!(
                  failure_count,
                  error = %error,
                  "Asynchronous composition effect handoff failed; repeated \
                   failures are rate-limited."
                );
              }
            }
          }
        }
        Err(error) => {
          if let Some(failure_count) =
            queued_updates.rate_limited_failure()
          {
            tracing::warn!(
              failure_count,
              error = %error,
              "Asynchronous composition effect rebuild failed; repeated \
               failures are rate-limited."
            );
          }
        }
      }
    }

    queued_updates.queued.store(false, Ordering::Release);
    let newer_request = lock_unpoisoned(&queued_updates.latest).generation
      != request.generation;
    if newer_request
      && queued_updates.alive.load(Ordering::Acquire)
      && queued_updates
        .queued
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
      if let Err(error) = enqueue_effect_rebuild(
        &queued_queue,
        &queued_compositor,
        &queued_backdrop,
        &queued_sprite,
        &queued_updates,
      ) {
        queued_updates.queued.store(false, Ordering::Release);
        if let Some(failure_count) = queued_updates.rate_limited_failure()
        {
          tracing::warn!(
            failure_count,
            error = %error,
            "Composition actor rejected the latest effect update; repeated \
             failures are rate-limited."
          );
        }
      }
    }
    Ok(())
  });
  ensure_composition_work_queued(queue.TryEnqueue(&handler)?)
}

impl BlurVisual {
  /// Resizes the visual tree's clip and both child visuals to match
  /// `rect`. Does not reposition the `HWND` itself -- callers still
  /// issue their own `SetWindowPos`, exactly as with the SWCA path.
  ///
  /// Must resize `root`/`blur_sprite`/`tint_sprite` in addition to the
  /// clip geometry -- they're independently-sized visuals set once in
  /// `build_visual_tree` and never otherwise touched, so leaving them out
  /// here left them pinned at their creation-time size while only the clip
  /// grew, showing blur/tint over just the original area and nothing over
  /// the rest whenever the overlay's `HWND` was resized after creation.
  pub(crate) fn set_rect(&self, rect: &Rect) -> crate::Result<()> {
    let size = Vector2 {
      X: pixels_to_dips(rect.width()),
      Y: pixels_to_dips(rect.height()),
    };
    self.root.SetSize(size)?;
    self.blur_sprite.SetSize(size)?;
    self.tint_sprite.SetSize(size)?;
    self.rounded_geometry.SetSize(size)?;
    Ok(())
  }

  /// Updates the tint layer's color; no-op unless the ABGR value changed.
  pub(crate) fn set_tint(&self, tint: u32) -> crate::Result<()> {
    self.tint_brush.SetColor(unpack_abgr_tint(tint))?;
    Ok(())
  }

  /// Updates the live blur radius by rebuilding the effect brush.
  ///
  /// The plan's original design mutated the existing brush in place via
  /// `effect_brush.Properties().InsertScalar("Blur.BlurAmount", value)`
  /// (the effect graph's `Name`-prefixed named-property system), matching
  /// the pattern shown for a *static* setup in Microsoft's own samples.
  /// Confirmed by actually running the real integration (not just the
  /// spike's automated checks) that this path reliably fails with
  /// `E_INVALIDARG` here, regardless of whether `GetNamedPropertyMapping`
  /// is queried with `"BlurAmount"` or `"Blur.BlurAmount"` -- consistent
  /// with `InsertScalar` needing the property to have been registered via
  /// `Compositor::CreateEffectFactoryWithProperties`'s
  /// `animatableProperties` list (which requires an
  /// `IIterable<HSTRING>`, not constructible from a `Vec` in this
  /// `windows-rs` version without hand-implementing the `WinRT` iterator
  /// interfaces) rather than the plain `CreateEffectFactory`
  /// this code uses. Rebuilding the brush instead reuses only the
  /// `GetProperty`-based initial-value path, which is confirmed working
  /// (overlays visibly render blur from their initial `blur_amount`).
  pub(crate) fn set_blur_amount(
    &mut self,
    value: f32,
  ) -> crate::Result<()> {
    #[allow(clippy::float_cmp)]
    if self.blur_amount == value {
      return Ok(());
    }
    self.blur_amount = value;
    self.queue_effect_rebuild()
  }

  /// Updates the live saturation by rebuilding the effect brush. Same
  /// rebuild-not-mutate approach as `set_blur_amount`, for the same
  /// reason (`InsertScalar` on a named effect property is not confirmed
  /// working in this pipeline) -- both knobs share the one effect graph,
  /// so either setter rebuilds the whole thing using the other's current
  /// stored value.
  pub(crate) fn set_saturation(
    &mut self,
    value: f32,
  ) -> crate::Result<()> {
    #[allow(clippy::float_cmp)]
    if self.saturation == value {
      return Ok(());
    }
    self.saturation = value;
    self.queue_effect_rebuild()
  }

  /// Updates the clip's corner radius.
  pub(crate) fn set_corner_radius(&self, value: f32) -> crate::Result<()> {
    self
      .rounded_geometry
      .SetCornerRadius(Vector2 { X: value, Y: value })?;
    Ok(())
  }

  /// Updates the overlay's own opacity. `root` sits above both
  /// `blur_sprite` and `tint_sprite`, so this fades the whole composited
  /// overlay (blur + tint together) as one unit -- a plain `Visual`
  /// property, not an effect-graph one, so unlike `set_blur_amount` this
  /// never needs a brush rebuild.
  pub(crate) fn set_opacity(&self, value: f32) -> crate::Result<()> {
    self.root.SetOpacity(value)?;
    Ok(())
  }

  fn queue_effect_rebuild(&self) -> crate::Result<()> {
    if !self
      .effect_updates
      .publish(self.blur_amount, self.saturation)
    {
      return Ok(());
    }
    if let Err(error) = enqueue_effect_rebuild(
      &self.queue,
      &self.compositor,
      &self.host_backdrop,
      &self.blur_sprite,
      &self.effect_updates,
    ) {
      self.effect_updates.queued.store(false, Ordering::Release);
      return Err(error);
    }
    Ok(())
  }
}

impl Drop for BlurVisual {
  fn drop(&mut self) {
    self.effect_updates.alive.store(false, Ordering::Release);
  }
}

/// `DesktopWindowTarget` sizes composition visuals 1:1 against the HWND's
/// actual client pixel size (no DPI virtualization layer here, unlike
/// XAML/UWP) -- so this is a passthrough today. Named/kept separate from a
/// bare cast so a future DPI-aware sizing adjustment has a single call
/// site.
#[allow(clippy::cast_precision_loss, clippy::unnecessary_wraps)]
fn pixels_to_dips(pixels: i32) -> f32 {
  pixels as f32
}

/// Builds a fresh effect brush (`host_backdrop` -> Gaussian blur ->
/// saturation -> brush) at the given knob values. Split out from
/// `build_visual_tree` so `BlurVisual::set_blur_amount`/`set_saturation`
/// can call it again on demand -- see `set_blur_amount`'s doc comment for
/// why a rebuild, not an in-place property update, is used.
///
/// Both stages are described in one `IGraphicsEffect` graph passed to a
/// single `CreateEffectFactory` call, producing one brush -- not two
/// independently-chained brushes. `SetSourceParameter("Source", ..)` binds
/// `host_backdrop` to the *inner* (blur) node's named parameter; the outer
/// (saturation) node's own source is the blur node's
/// `IGraphicsEffectSource` directly (an internal graph edge via
/// `GetSource`, not a named parameter), and the composition engine
/// resolves the "Source" name lookup through to it regardless of nesting
/// depth.
///
/// `exposure`, `vignette`, and `grain` were tried and dropped.
/// `Windows.UI.Composition`'s `CreateEffectFactory` accepts a curated
/// subset of D2D1 built-in effects at *construction* time, but `Exposure`
/// (`CLSID_D2D1Exposure`) was a no-op at render time despite constructing
/// without error and being byte-identical in its wrapper to `Saturation`,
/// which renders correctly -- observed with no visible change even at
/// extreme values well outside the documented `-2..2` range, so this
/// isn't a subtlety-of-effect issue, just an unsupported effect that
/// silently degrades to pass-through instead of failing loudly.
/// `CLSID_D2D1Vignette` plus the `Turbulence`/`Composite` combination
/// grain needed both failed outright with `E_INVALIDARG` ("Unsupported
/// effect type") at construction, confirmed via bisection. `HueRotation`
/// was also tried and dropped -- `CreateEffectFactory` accepted it fine,
/// but with no persistent tint the live desktop content behind the
/// overlay had too little color for a rotation to visibly do anything.
fn build_effect_brush(
  compositor: &Compositor,
  host_backdrop: &CompositionBackdropBrush,
  blur_amount: f32,
  saturation: f32,
) -> windows::core::Result<CompositionEffectBrush> {
  let source_param =
    CompositionEffectSourceParameter::Create(&HSTRING::from("Source"))?;
  let blur_effect: IGraphicsEffectSource = D2d1ScalarEffect::new(
    CLSID_D2D1_GAUSSIAN_BLUR,
    "Blur",
    source_param.cast()?,
    "BlurAmount",
    blur_amount,
    &[
      D2D1_GAUSSIANBLUR_OPTIMIZATION_BALANCED,
      D2D1_BORDER_MODE_SOFT,
    ],
  )
  .into();
  let saturation_effect: IGraphicsEffect = D2d1ScalarEffect::new(
    CLSID_D2D1_SATURATION,
    "Saturation",
    blur_effect,
    "Saturation",
    saturation,
    &[],
  )
  .into();
  let effect_factory =
    compositor.CreateEffectFactory(&saturation_effect)?;
  let effect_brush = effect_factory.CreateBrush()?;
  effect_brush
    .SetSourceParameter(&HSTRING::from("Source"), host_backdrop)?;
  Ok(effect_brush)
}

/// Builds the full visual tree: a `ContainerVisual` rooting a blur sprite
/// (host-backdrop brush through the Gaussian-blur/saturation effect graph)
/// and a tint sprite (flat color) stacked above it, both clipped by a
/// shared rounded rectangle geometry.
fn build_visual_tree(
  compositor: &Compositor,
  queue: &DispatcherQueue,
  hwnd: HWND,
  rect: &Rect,
  params: BlurOverlayParams,
) -> windows::core::Result<BlurVisual> {
  // SAFETY: `hwnd` is a valid, already-created top-level window.
  let target = unsafe {
    compositor
      .cast::<ICompositorDesktopInterop>()?
      .CreateDesktopWindowTarget(hwnd, false)?
  };

  let width = pixels_to_dips(rect.width());
  let height = pixels_to_dips(rect.height());
  let size = Vector2 {
    X: width,
    Y: height,
  };

  let rounded_geometry = compositor.CreateRoundedRectangleGeometry()?;
  rounded_geometry.SetSize(size)?;
  rounded_geometry.SetCornerRadius(Vector2 {
    X: params.corner_radius,
    Y: params.corner_radius,
  })?;
  let clip =
    compositor.CreateGeometricClipWithGeometry(&rounded_geometry)?;

  let host_backdrop = compositor.CreateHostBackdropBrush()?;
  let effect_brush = build_effect_brush(
    compositor,
    &host_backdrop,
    params.blur_amount,
    params.saturation,
  )?;

  let blur_sprite = compositor.CreateSpriteVisual()?;
  blur_sprite.SetBrush(&effect_brush)?;
  blur_sprite.SetSize(size)?;

  let tint_brush =
    compositor.CreateColorBrushWithColor(unpack_abgr_tint(params.tint))?;
  let tint_sprite = compositor.CreateSpriteVisual()?;
  tint_sprite.SetBrush(&tint_brush)?;
  tint_sprite.SetSize(size)?;

  let root = compositor.CreateContainerVisual()?;
  root.SetSize(size)?;
  root.SetClip(&clip)?;
  root.SetOpacity(params.opacity)?;
  root.Children()?.InsertAtTop(&blur_sprite)?;
  root.Children()?.InsertAtTop(&tint_sprite)?;

  target.SetRoot(&root)?;

  Ok(BlurVisual {
    _target: target,
    compositor: compositor.clone(),
    queue: queue.clone(),
    host_backdrop,
    root,
    blur_sprite,
    tint_sprite,
    tint_brush,
    rounded_geometry,
    blur_amount: params.blur_amount,
    saturation: params.saturation,
    effect_updates: Arc::new(EffectUpdateSlot::new(
      params.blur_amount,
      params.saturation,
    )),
  })
}

#[cfg(test)]
mod tests {
  use std::{
    sync::{atomic::Ordering, mpsc},
    time::{Duration, Instant},
  };

  use windows::{System::DispatcherQueueHandler, Win32::Foundation::HWND};

  use super::{
    composition_admission_available, composition_thread,
    ensure_composition_work_queued, initialize_composition_actor,
    queue_blur_visual, EffectUpdateSlot, MAX_PENDING_COMPOSITION_OVERLAYS,
  };
  use crate::{BlurOverlayParams, Rect};

  #[test]
  fn repeated_effect_failures_emit_bounded_diagnostics() {
    let slot = EffectUpdateSlot::new(30.0, 1.0);
    let emitted: Vec<_> = (1..=130)
      .filter_map(|_| slot.rate_limited_failure())
      .collect();
    assert_eq!(emitted, vec![1, 64, 128]);
  }

  #[test]
  fn composition_admission_is_strictly_bounded() {
    assert!(composition_admission_available(0));
    assert!(composition_admission_available(
      MAX_PENDING_COMPOSITION_OVERLAYS - 1
    ));
    assert!(!composition_admission_available(
      MAX_PENDING_COMPOSITION_OVERLAYS
    ));
    assert!(!composition_admission_available(usize::MAX));
  }

  #[test]
  fn rejected_composition_work_fails_without_waiting() {
    assert!(ensure_composition_work_queued(false).is_err());
    assert!(ensure_composition_work_queued(true).is_ok());
  }

  #[test]
  fn effect_updates_keep_one_queued_wake_and_latest_generation() {
    let updates = EffectUpdateSlot::new(1.0, 1.0);
    let mut queue_wakes = 0;
    for index in 0_u16..10_000 {
      queue_wakes += usize::from(updates.publish(f32::from(index), 2.0));
    }

    let latest = *super::lock_unpoisoned(&updates.latest);
    assert_eq!(queue_wakes, 1);
    assert_eq!(latest.generation, 10_000);
    assert_eq!(latest.blur_amount, 9_999.0);
    assert_eq!(latest.saturation, 2.0);
    assert!(updates.queued.load(Ordering::Acquire));
  }

  #[test]
  fn completed_generation_allows_exactly_one_new_actor_wake() {
    let updates = EffectUpdateSlot::new(1.0, 1.0);
    assert!(updates.publish(2.0, 1.0));
    assert!(!updates.publish(3.0, 1.0));
    updates.queued.store(false, Ordering::Release);
    assert!(updates.publish(4.0, 1.0));
    assert!(!updates.publish(5.0, 1.0));
  }

  #[test]
  fn overlay_setup_queue_does_not_wait_for_a_busy_actor() {
    let thread = composition_thread()
      .expect("composition actor should initialize on supported Windows");
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let mut work = Some(move || {
      let _ = entered_tx.send(());
      let _ = release_rx.recv_timeout(Duration::from_secs(2));
    });
    let blocker = DispatcherQueueHandler::new(move || {
      if let Some(work) = work.take() {
        work();
      }
      Ok(())
    });
    assert!(thread
      .queue
      .TryEnqueue(&blocker)
      .expect("composition actor should accept the test blocker"));
    entered_rx
      .recv_timeout(Duration::from_secs(2))
      .expect("composition actor should begin the test blocker");

    let initialization_started = Instant::now();
    assert!(initialize_composition_actor());
    let initialization_elapsed = initialization_started.elapsed();
    let started = Instant::now();
    let receiver = queue_blur_visual(
      HWND::default(),
      Rect::from_xy(0, 0, 100, 100),
      BlurOverlayParams {
        tint: 0,
        blur_amount: 20.0,
        corner_radius: 8.0,
        opacity: 1.0,
        saturation: 1.0,
      },
    )
    .expect("busy actor should still accept bounded overlay work");
    let queue_elapsed = started.elapsed();
    let _ = release_tx.send(());

    assert!(
      initialization_elapsed < Duration::from_millis(50),
      "actor warmup blocked its caller for {initialization_elapsed:?}"
    );
    assert!(
      queue_elapsed < Duration::from_millis(50),
      "overlay setup blocked on the actor for {queue_elapsed:?}"
    );
    assert!(
      receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("queued invalid-HWND build should finish")
        .is_err(),
      "an invalid HWND must fail instead of creating a visual"
    );
  }
}
