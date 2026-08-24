# Animation Reference

This document describes the animation names accepted by this build and the
runtime behavior they select. Configuration examples use YAML keys exactly as
the deserializer accepts them.

## Easing names

Every animation `easing` field accepts the names below or a custom cubic
Bézier. Progress `t` is the elapsed fraction in `[0, 1]`. Exact endpoints are
always `f(0) = 0` and `f(1) = 1`.

| Config name | Runtime function |
| --- | --- |
| `linear` | Exact identity `f(t) = t`; represented as `cubic-bezier(0, 0, 1, 1)` |
| `ease_in` | CSS `cubic-bezier(0.42, 0, 1, 1)` |
| `ease_out` | CSS `cubic-bezier(0, 0, 0.58, 1)` |
| `ease_in_out` | CSS `cubic-bezier(0.42, 0, 0.58, 1)` |
| `ease_in_cubic` | Intentional CSS-style profile `cubic-bezier(0.32, 0, 0.67, 0)`; not the exact power `t^3` |
| `ease_out_cubic` | Intentional CSS-style profile `cubic-bezier(0.33, 1, 0.68, 1)`; not the exact reflected power |
| `ease_in_out_cubic` | Intentional CSS-style profile `cubic-bezier(0.65, 0, 0.35, 1)`; not an exact piecewise power |
| `ease_in_quart` | Exact `t^4` |
| `ease_out_quart` | Exact `1 - (1 - t)^4` |
| `ease_in_out_quart` | `((2t)^4)/2` before 0.5; `1 - ((2 - 2t)^4)/2` after 0.5 |
| `ease_in_quint` | Exact `t^5` |
| `ease_out_quint` | Exact `1 - (1 - t)^5` |
| `ease_in_out_quint` | `((2t)^5)/2` before 0.5; `1 - ((2 - 2t)^5)/2` after 0.5 |
| `ease_in_expo` | `2^(10t - 10)`, with an exact zero endpoint |
| `ease_out_expo` | `1 - 2^(-10t)`, with an exact one endpoint |
| `ease_in_out_expo` | `2^(20t - 10)/2` before 0.5; `(2 - 2^(-20t + 10))/2` after 0.5, with exact endpoints |
| `ease_in_back` | `(c1 + 1)t^3 - c1*t^2`, `c1 = 1.70158`; undershoots below 0 |
| `ease_out_back` | `1 + (c1 + 1)(t - 1)^3 + c1(t - 1)^2`; overshoots above 1 |
| `ease_in_out_back` | Standard piecewise Back curve with `c2 = 1.70158 * 1.525`; under- and overshoots |
| `ease_out_spring` | `1 - 2^(-12t) * sin((4t - 4.5) * pi)` between exact endpoints; oscillates around 1 |

`ease_in_overshoot`, `ease_out_overshoot`, and
`ease_in_out_overshoot` are compatibility aliases for the corresponding Back
curves. Serialization uses the canonical `*_back` names.

### Custom cubic Bézier

```yaml
animations:
  window_move:
    easing: "cubic_bezier(0.16, 1.0, 0.3, 1.0)"
```

Exactly four finite floating-point values are required. `x1` and `x2` must be
in `[0, 1]`; `y1` and `y2` may be outside that range to create under- or
overshoot. The evaluator solves the x-axis mapping with a fast Newton step and
a bounded binary-search fallback for flat or degenerate valid curves. It does
not evaluate the curve parameter directly as elapsed progress. Invalid syntax,
missing values, NaN, infinity, or out-of-range x controls reject the complete
config load/reload; there is no silent easing fallback.

Curves whose y controls are in `[0, 1]` remain bounded to `[0, 1]` despite
floating-point roundoff. Curves with out-of-range y controls preserve their
intended overshoot. Geometry interpolation therefore may pass its target and
return; native opacity is always bounded to the legal alpha range.

## Window move, resize, and focus

`animations.window_move` and `animations.window_resize` have no style name.
They interpolate from the current rectangle to the requested rectangle using
their configured duration/easing once the corresponding `threshold_px` is
met. Move/resize uses the sampled-gap surrogate policy needed to cover changing
geometry.

`animations.focus_change` interpolates only between the opacity targets under
`window_effects.focused_window.transparency` and
`window_effects.other_windows.transparency`. It does not move or resize a
window and does not change focus-follows-cursor semantics.

## Window open and close styles

Set the style under `animations.window_open.style` or
`animations.window_close.style`:

```yaml
animations:
  window_open:
    enabled: true
    duration_ms: 180
    easing: "ease_out_quint"
    style: "zoom"
    opacity_from: 1.0
  window_close:
    enabled: true
    duration_ms: 140
    easing: "ease_in_cubic"
    style: "none"
    opacity_to: 0.0
```

| Style | Open behavior | Close behavior |
| --- | --- | --- |
| `slide_right` | Starts one window width to the right and moves to the final tile | Moves one window width to the right |
| `slide_left` | Starts one window width to the left | Moves one window width to the left |
| `slide_top` | Starts one window height above | Moves one window height above |
| `slide_bottom` | Starts one window height below | Moves one window height below |
| `none` | No geometry change; `opacity_from < 1` produces a fade-in | No geometry change; `opacity_to < 1` produces a fade-out |
| `zoom` | Thumbnail grows from zero around the window center | Thumbnail shrinks to zero around the window center |

`right`, `left`, `top`, and `bottom` are aliases for the corresponding slide
styles. `fade` is an alias for `none`; fading is controlled by
`opacity_from`/`opacity_to`. On `window_open`, the legacy key `direction:` is
also accepted as an alias for the `style:` key. An unknown style rejects the
config. `none` with an opacity endpoint of `1.0` has no visible animation and
is skipped.

Open/close use transparent, tick-staged DWM handoff surrogates. A failed or
timed-out handoff reveals/preserves the real window at its safe geometry rather
than leaving it cloaked. Move/resize retains its separate sampled-gap policy.

## Workspace switch styles

```yaml
animations:
  workspace_switch:
    enabled: true
    duration_ms: 250
    easing: "ease_out_quint"
    style: "slide"
    direction: "horizontal"
    iris_origin: "center"
    opacity_outgoing: 1.0
    opacity_incoming: 1.0
    zoom_factor: 0.1
```

| Style | Runtime behavior |
| --- | --- |
| `slide` | Incoming windows start one monitor dimension in workspace-order direction while outgoing windows travel the opposite way. `direction` selects horizontal or vertical motion. Visible regions are clipped to the monitor. |
| `fade` | Incoming and outgoing surrogates remain at their window rectangles and crossfade using the configured opacity endpoints. |
| `zoom` | Each window independently expands from or shrinks toward its own screen-space center, with the configured opacity endpoints. It does not converge the whole workspace on the monitor center. |
| `iris` | A frozen outgoing-monitor snapshot is cut away by a growing circular aperture, revealing the live incoming workspace. Capture/overlay failure falls back to an instant visible switch. |

For `slide`, `zoom_factor` optionally scales the complete workspace around the
monitor center while it translates: outgoing scale is `1 -> 1-zoom_factor`,
incoming scale is `1-zoom_factor -> 1`. It is ignored by standalone `fade`,
`zoom`, and `iris` styles.

`direction` accepts `horizontal` or `vertical`. Workspace order determines the
sign: a higher configured workspace index enters from the right/bottom; a lower
index enters from the left/top. Missing/equal order uses zero directional
offset.

`iris_origin` accepts `center`, `cursor`, or `focused_window`. A cursor outside
the target monitor and a missing/unpositioned focused window both fall back to
the monitor center. Unknown style, direction, or origin names reject the
config.

## Timing and failure behavior

All durations are unsigned integer milliseconds. `0` completes immediately.
Animations are advanced by compositor-paced ticks when the per-monitor DXGI
waiter is available and fall back to the normal timer path if waiter creation
fails. Non-overshooting movement may snap only after less than one pixel of
travel remains; overshooting Back, spring, and overshooting custom Bézier
curves run for their full wall-clock duration so the return motion is retained.

Native animation, thumbnail, cloak, timer, or teardown failure always prefers
a visible usable real window. No public config option changes that safety
fallback.
