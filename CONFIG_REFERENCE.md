# GlazeWM Combined configuration reference

This is the complete user-facing reference for the configuration accepted by **this build**. It covers every top-level YAML section, every child key, accepted values, source defaults, hard limits, commands usable from configuration, and the custom DComp animation, multi-layout, and stack features. It was checked against `packages/wm-common/src/parsed_config.rs`, `packages/wm-common/src/app_command.rs`, the primitive parsers in `packages/wm-platform/src/models`, command handlers in `packages/wm/src`, and `resources/assets/sample-config.yaml`.

Last source reconciliation: **2026-08-28**, release **0.0.6**. Unknown keys,
invalid numeric values, malformed quoting, and invalid compiled rule regexes
reject the candidate configuration. Reload is transactional: the current
configuration remains active unless parsing, validation, derived-plan creation,
and native preflight all succeed.

All YAML keys are `snake_case`. Sections and fields omitted from YAML use their source defaults unless a required list item field is noted. Commands in YAML are strings without the leading `glazewm command`; from a terminal use `glazewm.exe command <command>`. Direction values are `left`, `right`, `up`, or `down`.

## Value syntax used throughout

- **Length**: a quoted finite decimal with optional `px` or `%`, such as `"20px"`, `"1.5px"`, `"10%"`, or `"-2.5%"`; no unit means pixels. One leading `+`/`-` and surrounding whitespace are accepted; internal whitespace, exponent notation, unsupported units, and extra text are rejected. Hard ranges are `-100000px..=100000px` and `-1000%..=1000%`. YAML also accepts `{ amount: 20.0, unit: pixel }` or `{ amount: 0.2, unit: percentage }` under the same finite/range checks. Negative lengths parse; whether they are meaningful depends on the field.
- **Rectangle**: `{ top: LENGTH, right: LENGTH, bottom: LENGTH, left: LENGTH }`.
- **Color**: exactly `"#RRGGBB"` or `"#RRGGBBAA"`.
- **Opacity**: a number from `0.0` through `1.0`, a numeric string such as `"0.95"`, or a percent such as `"95%"`. Full configuration validation rejects non-finite or out-of-range values; they are not silently clamped.
- **Duration/threshold**: unsigned 32-bit integer (`0` through `4294967295`) in milliseconds/pixels. This is the hard parser range, not a sensible animation range. For visible animation, approximately `80..700` ms is practical; `0` is immediate.
- **Boolean**: `true` or `false`.

## Complete top-level shape

```yaml
animations: {}
binding_modes: []
gaps: {}
general: {}
keybindings: []
stack: {}
window_behavior: {}
window_effects: {}
window_rules: []
workspaces: []
```

Unknown YAML fields are rejected at the root and within nested/list entries. The error identifies the offending field where Serde has direct struct context. Documented migration aliases remain accepted.

## Hard limits and practical ranges

| Value | Hard accepted range | Practical guidance |
|---|---:|---|
| `duration_ms` | `0..=10000` milliseconds | `0` snaps immediately and skips animation setup; `80..250` feels quick; `250..500` is expressive; above `700` usually feels slow. Values above 10 seconds are rejected rather than clamped. |
| `threshold_px` | `0..=100000` pixels | `0` animates every change; `5..20` suppresses tiny layout noise. Values above the hard maximum are rejected rather than converted to signed pixels. |
| Animation opacity fields | finite `0.0..=1.0` | `0.0` fully transparent; `1.0` fully opaque. |
| Transparency opacity | finite `0.0..=1.0`, or equivalent `0%..100%` | Avoid `0` for normal windows because the window becomes effectively unfocusable. |
| Workspace `zoom_factor` | finite `0.0..=1.0` | `0.03..0.15` gives subtle depth; `1.0` collapses to a point. |
| Bézier `x1`, `x2` | finite `0.0..=1.0` | Required so time remains solvable and monotonic. |
| Bézier `y1`, `y2` | any finite `f32` | Values below `0` or above `1` create undershoot/overshoot. |
| Backdrop `opacity` | finite `0.0..=1.0` | Applies to the acrylic overlay, not the real window. |
| Backdrop `blur_amount` | any finite `f32` | `0..100` is a useful experimental range; source default is `30`. |
| Backdrop `saturation` | any finite `f32` | D2D behavior is defined most usefully around `0.0..=2.0`; source default is `1`. |
| Length pixels | finite `-100000..=100000` | Whole-input decimal syntax; no unit means pixels. |
| Length percentages | finite `-1000%..=1000%` | Internally normalized to `-10.0..=10.0`. |
| Workspace monitor index | `0..=4294967295` (`u32`) | Zero-based; it must identify a monitor that exists to be useful. |
| Command monitor/index values | platform `usize`/declared integer type | Use existing zero-based indices. |

`NaN`, positive infinity, and negative infinity are rejected for every animation/effect and length float covered by validation.

## Source-default checklist

These are defaults from the Rust structs when a key or section is omitted. The embedded sample config deliberately overrides several of them.

| YAML path | Source default | Accepted type/values |
|---|---|---|
| `animations.window_move.enabled` | `true` | Boolean |
| `animations.window_move.duration_ms` | `150` | `u32` milliseconds |
| `animations.window_move.easing` | `ease_in_out` | Easing string |
| `animations.window_move.threshold_px` | `10` | `u32` pixels |
| `animations.window_resize.enabled` | `true` | Boolean |
| `animations.window_resize.duration_ms` | `150` | `u32` milliseconds |
| `animations.window_resize.easing` | `ease_in_out` | Easing string |
| `animations.window_resize.threshold_px` | `10` | `u32` pixels |
| `animations.focus_change.enabled` | `false` | Boolean |
| `animations.focus_change.duration_ms` | `200` | `u32` milliseconds |
| `animations.focus_change.easing` | `cubic_bezier(0.16, 1, 0.3, 1)` | Easing string |
| `animations.window_open.enabled` | `true` | Boolean |
| `animations.window_open.duration_ms` | `150` | `u32` milliseconds |
| `animations.window_open.easing` | `cubic_bezier(0.16, 1, 0.3, 1)` | Easing string |
| `animations.window_open.style` | `slide_right` | Window-transition style |
| `animations.window_open.opacity_from` | `1.0` | Finite `0.0..=1.0` |
| `animations.window_close.enabled` | `false` | Boolean |
| `animations.window_close.duration_ms` | `150` | `u32` milliseconds |
| `animations.window_close.easing` | `ease_in_cubic` | Easing string |
| `animations.window_close.style` | `none` | Window-transition style |
| `animations.window_close.opacity_to` | `0.0` | Finite `0.0..=1.0` |
| `animations.workspace_switch.enabled` | `true` | Boolean |
| `animations.workspace_switch.duration_ms` | `250` | `u32` milliseconds |
| `animations.workspace_switch.easing` | `cubic_bezier(0.16, 1, 0.3, 1)` | Easing string |
| `animations.workspace_switch.style` | `slide` | `slide`, `fade`, `zoom`, `iris` |
| `animations.workspace_switch.direction` | `horizontal` | `horizontal`, `vertical` |
| `animations.workspace_switch.iris_origin` | `center` | `center`, `cursor`, `focused_window` |
| `animations.workspace_switch.opacity_outgoing` | `1.0` | Finite `0.0..=1.0` |
| `animations.workspace_switch.opacity_incoming` | `1.0` | Finite `0.0..=1.0` |
| `animations.workspace_switch.zoom_factor` | `0.1` | Finite `0.0..=1.0` |
| `general.startup_commands` | `[]` | Command-string list |
| `general.shutdown_commands` | `[]` | Command-string list |
| `general.config_reload_commands` | `[]` | Command-string list |
| `general.focus_follows_cursor` | `false` | Boolean |
| `general.toggle_workspace_on_refocus` | `true` | Boolean |
| `general.cursor_jump.enabled` | `false` | Boolean |
| `general.cursor_jump.trigger` | `monitor_focus` | `monitor_focus`, `window_focus` |
| `general.hide_method` | `cloak` on Windows | `cloak`, `hide`, `place_in_corner` |
| `general.show_all_in_taskbar` | `false` | Boolean |
| `gaps.scale_with_dpi` | `true` | Boolean |
| `gaps.inner_gap` | `0px` | Length |
| `gaps.outer_gap.*` | `0px` each side | Rectangle of lengths |
| `gaps.single_window_outer_gap` | `null` | Rectangle or `null` |
| `window_behavior.initial_state` | `tiling` | `tiling`, `floating` |
| `window_behavior.state_defaults.floating.centered` | `true` | Boolean |
| `window_behavior.state_defaults.floating.shown_on_top` | `false` | Boolean |
| `window_behavior.state_defaults.fullscreen.maximized` | `true` | Boolean |
| `window_behavior.state_defaults.fullscreen.shown_on_top` | `false` | Boolean |
| `window_effects.*.border.enabled` | `false` | Boolean |
| `window_effects.*.border.color` | `#8cbeffff` | Color |
| `window_effects.*.hide_title_bar.enabled` | `false` | Boolean |
| `window_effects.*.corner_style.enabled` | `false` | Boolean |
| `window_effects.*.corner_style.style` | `default` | `default`, `square`, `rounded`, `small_rounded` |
| `window_effects.*.transparency.enabled` | `false` | Boolean |
| `window_effects.*.transparency.opacity` | `1.0` | Opacity |
| `window_effects.*.backdrop.enabled` | `false` | Boolean |
| `window_effects.*.backdrop.style` | `acrylic` | `acrylic`, `mica`, `mica_alt` |
| `window_effects.*.backdrop.tint` | `null` | Color or `null` |
| `window_effects.*.backdrop.blur_amount` | `30.0` | Finite float |
| `window_effects.*.backdrop.opacity` | `1.0` | Finite `0.0..=1.0` |
| `window_effects.*.backdrop.saturation` | `1.0` | Finite float |
| `stack.tab_bar_height` | `0px` | Length; zero disables tab bar |
| `stack.tab_bar_position` | `top` | `top`, `bottom` |
| `stack.tab_bar_background` | `#2d2d2d` | Color |
| `stack.tab_active_background` | `#4a4a6a` | Color |
| `stack.tab_inactive_background` | `#1e1e2e` | Color |
| `stack.tab_text_color` | `#cdd6f4` | Color |
| `stack.tab_active_indicator_color` | `#89b4fa` | Color |
| `stack.tab_active_indicator_height` | `2px` | Length |
| `stack.tab_border_radius` | `4px` | Length |
| `stack.tab_separator_width` | `1px` | Length |
| `stack.tab_separator_color` | `#3d3d3d` | Color |
| `stack.tab_title_overrides` | `[]` | List of `{ regex, replace }` |
| `binding_modes` | `[]` | Binding-mode list |
| `keybindings` | `[]` | Keybinding list |
| `window_rules` | `[]` | Window-rule list |
| `workspaces` | `[]` | Workspace list |

For `window_effects.*`, replace `*` with either `focused_window` or `other_windows`; both children have the same complete schema and defaults.

## Animations

All six children are optional. If an animation or native surrogate/capture setup fails, the implementation is designed to fall back to the final visible window state; `iris` capture failure becomes an instant workspace switch.

```yaml
animations:
  window_move:
    enabled: true
    duration_ms: 150
    easing: "ease_in_out"
    threshold_px: 10
  window_resize:
    enabled: true
    duration_ms: 150
    easing: "ease_in_out"
    threshold_px: 10
  focus_change:
    enabled: false
    duration_ms: 200
    easing: "cubic_bezier(0.16, 1, 0.3, 1)"
  window_open:
    enabled: true
    duration_ms: 150
    easing: "cubic_bezier(0.16, 1, 0.3, 1)"
    style: "slide_right"
    opacity_from: 1.0
  window_close:
    enabled: false
    duration_ms: 150
    easing: "ease_in_cubic"
    style: "none"
    opacity_to: 0.0
  workspace_switch:
    enabled: true
    duration_ms: 250
    easing: "cubic_bezier(0.16, 1, 0.3, 1)"
    style: "slide"
    direction: "horizontal"
    iris_origin: "center"
    opacity_outgoing: 1.0
    opacity_incoming: 1.0
    zoom_factor: 0.1
```

### Shared animation fields

- `enabled`: switches that animation path on/off.
- `duration_ms`: total duration in the enforced range `0..=10000`. `0` is valid and effectively immediate; invalid values reject the complete configuration.
- `easing`: one of the CSS/cubic names `linear`, `ease_in`, `ease_out`, `ease_in_out`, `ease_in_cubic`, `ease_out_cubic`, `ease_in_out_cubic`; the exact power families `ease_in_quart`, `ease_out_quart`, `ease_in_out_quart`, `ease_in_quint`, `ease_out_quint`, `ease_in_out_quint`; the exact exponential family `ease_in_expo`, `ease_out_expo`, `ease_in_out_expo`; the true overshooting Back family `ease_in_back`, `ease_out_back`, `ease_in_out_back`; `ease_out_spring`; or `cubic_bezier(x1, y1, x2, y2)`. `ease_in_overshoot`, `ease_out_overshoot`, and `ease_in_out_overshoot` are aliases for the corresponding Back curves. Quart and quint are evaluated as fourth- and fifth-order power functions, Expo uses base-2 exponential formulas, and Back uses the standard `1.70158` overshoot constant. A custom curve requires exactly four floats; `x1` and `x2` must be in `[0,1]`, while the y values may exceed it for overshoot.
- `threshold_px`: only on move/resize; enforced range `0..=100000`. Changes smaller than this pixel distance do not animate.

### Window and focus animation fields

- `focus_change`: interpolates between the focused/non-focused transparency targets under `window_effects`; Windows only.
- `window_open.style` and `window_close.style`: `slide_right`, `slide_left`, `slide_top`, `slide_bottom`, `none`, or `zoom`. Aliases `right`, `left`, `top`, `bottom`, and `fade` are accepted. On `window_open` only, the legacy key name `direction:` is accepted as an alias for `style:`.
- `opacity_from` / `opacity_to`: finite float in the enforced range `0.0..=1.0`. Invalid values reject the complete configuration. `none` plus opacity creates a pure fade.

### Workspace transition fields

- `style`: `slide`, `fade`, `zoom`, or Windows-only `iris`.
- Workspace `zoom` scales each window independently around its own screen-space
  center. It does not collapse the complete workspace toward the monitor
  center. The separate `zoom_factor` option applies monitor-centered scaling
  only while `style: slide` is active.
- `direction`: `horizontal` or `vertical`; used by `slide` only.
- `iris_origin`: `center`, `cursor`, or `focused_window`; used by `iris` only. `focused_window` falls back to monitor center when none is available.
- `opacity_outgoing` / `opacity_incoming`: finite float in the enforced range `0.0..=1.0`; applies to all styles. Invalid values reject the complete configuration.
- `zoom_factor`: finite float in the enforced range `0.0..=1.0`, recommended `0.05..=0.15`; affects `slide` only. Invalid values reject the complete configuration.

Rapid slide requests do not create an unbounded animation queue. An exact
two-workspace reversal reuses the currently visible surrogate set. An
incompatible third destination replaces one bounded absolute destination,
stages that real workspace underneath the retained frame, then performs a
120–180 ms directional reveal (40% of the configured duration). Logical focus,
focus effects, cursor targets, and workspace IPC updates commit only when that
underlay is ready. Native failure uses the visible-real-window emergency
fallback. This scheduling policy is internal and has no YAML key.

## General/core

```yaml
general:
  startup_commands: []
  shutdown_commands: []
  config_reload_commands: []
  focus_follows_cursor: false
  toggle_workspace_on_refocus: true
  cursor_jump:
    enabled: false
    trigger: "monitor_focus"
  hide_method: "cloak"
  show_all_in_taskbar: false
```

- `startup_commands`: command strings run after startup.
- `shutdown_commands`: command strings run immediately before clean shutdown.
- `config_reload_commands`: command strings run after a successful reload.
- `focus_follows_cursor`: focuses the window under real pointer movement. This is independent of `cursor_jump`.
- `toggle_workspace_on_refocus`: focusing the already-focused workspace switches to the previous workspace.
- `cursor_jump.enabled`: moves the pointer automatically after the selected focus event.
- `cursor_jump.trigger`: `monitor_focus` or `window_focus`.
- `hide_method`: `cloak` (recommended Windows DWM cloak), `hide` (legacy Win32 hide), or `place_in_corner`. The last is forced on macOS.
- `show_all_in_taskbar`: if true, windows on hidden workspaces remain represented in the Windows taskbar; ineffective with `hide`.

### Gaps

```yaml
gaps:
  scale_with_dpi: true
  inner_gap: "20px"
  outer_gap: { top: "60px", right: "20px", bottom: "20px", left: "20px" }
  single_window_outer_gap: null
```

- `scale_with_dpi`: scales configured gaps for monitor DPI.
- `inner_gap`: spacing between adjacent tiling windows.
- `outer_gap`: edge spacing for normal workspace layouts.
- `single_window_outer_gap`: optional replacement rectangle when exactly one tiling window exists; use `null` to disable.

### Initial state

```yaml
window_behavior:
  initial_state: "tiling"
  state_defaults:
    floating:
      centered: true
      shown_on_top: false
    fullscreen:
      maximized: true
      shown_on_top: false
```

`initial_state` is `tiling` or `floating`. The child booleans become defaults for new windows and for state-changing commands when the corresponding flags are omitted.

## Layouts and stacks

The custom workspace layouts are runtime state selected by commands, not fields inside a `workspaces:` entry.

| Layout value | Actual behavior |
|---|---|
| `bsp` | Recursively splits the insertion target; split axis follows the target tiling direction. |
| `split_horizontal` | Equal-width top-level columns. |
| `split_vertical` | Equal-height top-level rows. |
| `columns` | Column distribution layout. |
| `rows` | Row distribution layout. |
| `grid` | Near-square row/column grid. |
| `vertical_stack` | Main pane plus windows stacked vertically in the secondary area. |
| `horizontal_stack` | Main pane plus windows stacked horizontally in the secondary area. |

Only `split_horizontal`, `split_vertical`, `vertical_stack`, and `horizontal_stack` consume per-window tiling-size adjustments. Layout choice is remembered for an existing workspace during the process lifetime; there is no per-workspace YAML `layout` key.

```yaml
keybindings:
  - commands: ["set-workspace-layout grid"]
    bindings: ["alt+g"]
  - commands: ["set-tiling-direction horizontal"]
    bindings: ["alt+h"]
  - commands: ["toggle-tiling-direction"]
    bindings: ["alt+v"]
```

`set-tiling-direction` accepts `horizontal` or `vertical` and controls insertion direction in direction containers/BSP. `toggle-tiling-direction` flips it.

### Stack tab bar

```yaml
stack:
  tab_bar_height: "30px"
  tab_bar_position: "top"
  tab_bar_background: "#2d2d2d"
  tab_active_background: "#4a4a6a"
  tab_inactive_background: "#1e1e2e"
  tab_text_color: "#cdd6f4"
  tab_active_indicator_color: "#89b4fa"
  tab_active_indicator_height: "2px"
  tab_border_radius: "4px"
  tab_separator_width: "1px"
  tab_separator_color: "#3d3d3d"
  tab_title_overrides:
    - regex: "^(.*) - Mozilla Firefox$"
      replace: "$1"
```

- `tab_bar_height`: `0px` disables rendering; Windows only.
- `tab_bar_position`: `top` or `bottom`.
- Color, indicator, radius, and separator fields control exactly the named tab visual; zero height/width disables that element.
- `tab_title_overrides`: ordered Rust-regex replacements. Capture references such as `$1` work; omitted `replace` means empty text. Invalid regex rules are logged and skipped.

Stack commands are listed under IPC/commands below. Named stacks created by `move-to-stack` are runtime containers; there is no YAML list of stack names.

## Workspaces

```yaml
workspaces:
  - name: "1"
    display_name: "Main"
    bind_to_monitor: 0
    keep_alive: true
    default_layout: "grid"
```

- `name` (required string): identifier used by focus/move commands. Names must
  be nonempty, have no leading or trailing Unicode whitespace, and be unique
  by exact case-sensitive comparison. Internal whitespace is allowed, and
  names such as `Dev` and `dev` are distinct; names are not otherwise
  normalized.
- `display_name` (optional string): presentation name for integrations/UI.
- `bind_to_monitor` (optional unsigned integer): zero-based monitor index.
- `keep_alive` (default `false`): retain the workspace when empty.
- `default_layout` (optional): initial layout when the workspace has no
  remembered runtime selection. Accepted values are `bsp`,
  `split_horizontal`, `split_vertical`, `columns`, `rows`, `grid`,
  `vertical_stack`, and `horizontal_stack`. A layout selected at runtime takes
  precedence until GlazeWM exits; it is not persisted across restarts. If this
  field is omitted, landscape monitors use `bsp` and portrait monitors use
  `split_vertical`, preserving the existing behavior. Invalid names reject the
  configuration instead of silently falling back.

At runtime, `update-workspace-config [--workspace NAME] --name NEW --display-name TEXT --bind-to-monitor N --keep-alive BOOL` updates one or more of these values. The same name rules apply. A rename is rejected while a deferred workspace transition is pending so recent-workspace and transition targets cannot become stale. Without `--workspace`, it targets the subject/focused workspace. This is runtime state, not a config-file edit.

## Keybindings and binding modes

```yaml
keybindings:
  - commands:
      - "focus --direction left"
      - "shell-exec cmd"
    bindings: ["alt+h", "alt+left"]

binding_modes:
  - name: "resize"
    display_name: "Resize mode"
    keybindings:
      - commands: ["resize --width -2%"]
        bindings: ["h", "left"]
```

Each keybinding's `commands` and `bindings` are lists. A binding is `+`-separated and case-insensitive. Available names are:

- Letters `a`–`z`; numbers `0`–`9` (also `d0`–`d9`); `f1`–`f24`.
- Modifiers: `cmd`, `ctrl`/`control`, `alt`/`menu`, `shift`, `win`, and left/right variants `lcmd`, `rcmd`, `lctrl`, `rctrl`, `lalt`/`lmenu`, `ralt`/`rmenu`, `lshift`, `rshift`, `lwin`, `rwin`.
- Special/navigation: `space`, `tab`, `enter`/`return`, `delete`, `escape`, `backspace`, `left`, `right`, `up`, `down`, `home`, `end`, `page_up`, `page_down`, `insert`, `num_lock`, `scroll_lock`, `caps_lock`.
- Numpad: `numpad0`–`numpad9`, `numpad_add`/`add`, `numpad_subtract`/`subtract`, `numpad_multiply`/`multiply`, `numpad_divide`/`divide`, `numpad_decimal`/`decimal`.
- Media/language: `volume_up`, `volume_down`, `volume_mute`, `media_next_track`, `media_prev_track`, `media_stop`, `media_play_pause`, `print_screen`, `muhenkan`, `henkan`.
- OEM: `oem_semicolon`, `oem_question`, `oem_tilde`, `oem_open_brackets`, `oem_pipe`, `oem_close_brackets`, `oem_quotes`, `oem_8`, `oem_102`, `oem_plus`, `oem_comma`/`comma`, `oem_minus`, `oem_period`/`period`. On Windows, a single literal key available without Shift/Alt in the active keyboard layout is also accepted.

A binding mode needs `name`; `display_name` and `keybindings` are optional. Enable/disable it with `wm-enable-binding-mode --name NAME` and `wm-disable-binding-mode --name NAME`. The active mode's bindings replace the normal set. While paused, only bindings containing `wm-toggle-pause` remain active.

## Window effects

The same complete child structure is valid under both `focused_window` and `other_windows`:

```yaml
window_effects:
  focused_window:
    border: { enabled: false, color: "#8cbeff" }
    hide_title_bar: { enabled: false }
    corner_style: { enabled: false, style: "default" }
    transparency: { enabled: false, opacity: "100%" }
    backdrop:
      enabled: false
      style: "acrylic"
      tint: null
      blur_amount: 30.0
      opacity: 1.0
      saturation: 1.0
  other_windows:
    border: { enabled: false, color: "#8cbeff" }
    hide_title_bar: { enabled: false }
    corner_style: { enabled: false, style: "default" }
    transparency: { enabled: false, opacity: "100%" }
    backdrop: { enabled: false, style: "acrylic", tint: null, blur_amount: 30.0, opacity: 1.0, saturation: 1.0 }
```

- `border`: Windows 11 border highlight.
- `hide_title_bar`: removes the native title bar; some applications may render incorrectly.
- `corner_style.style`: `default`, `square`, `rounded`, or `small_rounded`; Windows 11.
- `transparency.opacity`: window opacity using the opacity syntax above. `0` makes the real window transparent and effectively unfocusable.
- `backdrop` (legacy section alias: `blur_behind`): DWM backdrop. `style` is `acrylic`, `mica`, or `mica_alt`; `tint` is null or a color. Acrylic is supported from Windows 10 1803; Mica variants require Windows 11 22H2. `blur_amount` and `saturation` accept any finite float; `opacity` is strictly `0.0..=1.0`. A practical saturation range is `0.0..=2.0`, but the parser does not enforce it. Those three affect the custom acrylic overlay only and are ignored for Mica; if Windows.UI.Composition is unavailable, acrylic falls back to fixed OS intensity. Backdrop corner radius follows `corner_style` and has no independent key.

## Window rules

```yaml
window_rules:
  - commands: ["set-floating --centered", "set-transparency --opacity 95%"]
    match:
      - window_process: { equals: "notepad" }
        window_class: { regex: "Notepad.*" }
        window_title: { not_regex: "Private" }
      - window_title: { includes: "Picture in Picture" }
    on: ["manage", "title_change"]
    run_once: true
```

- `commands` (required list): any invoke commands below.
- `match` (required list): entries are OR alternatives; populated fields within an entry are AND conditions. Omitted process/class/title fields are wildcards.
- Each match object must be exactly one of `{ equals: STRING }`, `{ includes: STRING }`, `{ regex: STRING }`, `{ not_equals: STRING }`, `{ not_regex: STRING }`. Comparisons are case-sensitive. Invalid regex never matches (and `not_regex` with invalid regex also returns false).
- `on`: any of `manage`, `title_change`, `focus`; default is `[manage, title_change]`.
- `run_once`: default `true`; prevents the same rule from rerunning for that managed window.
- `ignore` unmanages/skips a matching window. `force-manage` is a pre-management marker that bypasses built-in manageability checks and is otherwise a no-op.

## Scratchpad status

This build has **no scratchpad configuration or command**. A source-wide search finds no `scratchpad`, `scratch_pad`, or `scratch-pad` schema, parser, command, or handler. Do not add a `scratchpad:` YAML section: it would be ignored as an unknown top-level key. Stacks, minimized windows, and hidden workspaces are separate features and are not scratchpads.

## IPC and commands

### Where command strings work

Use the same command grammar in `keybindings[].commands`, `binding_modes[].keybindings[].commands`, `window_rules[].commands`, and the three general lifecycle command lists. YAML and raw IPC command strings use Windows command-line quoting: quoted spaces and empty arguments are preserved, backslashes before quotes follow Windows rules, and an unterminated quote rejects the command before dispatch. `shell-exec` consumes all remaining parsed arguments as the program command.

From a terminal:

```powershell
glazewm.exe command focus --direction left
glazewm.exe command --id 01234567-89ab-cdef-0123-456789abcdef set-floating --centered
```

`--id UUID` changes the subject container; otherwise the focused container is used.

### Complete invoke-command list

Square brackets below mean optional syntax; alternatives separated by `|` are mutually exclusive where stated.

- `adjust-borders --top LENGTH [--right LENGTH --bottom LENGTH --left LENGTH]`: adjust one or more tiling borders; at least one side required.
- `close`: request the subject window to close.
- `focus` plus exactly one of `--direction DIR`, `--container-id UUID`, `--workspace-in-direction DIR`, `--workspace NAME`, `--monitor INDEX`, `--next-active-workspace`, `--prev-active-workspace`, `--next-workspace`, `--prev-workspace`, `--next-active-workspace-on-monitor`, `--prev-active-workspace-on-monitor`, `--recent-workspace`.
- `force-manage`: window-rule pre-management marker described above.
- `ignore`: unmanage/ignore the subject window.
- `move-cursor --direction DIR`: move the cursor to the center of the currently active window. A direction argument is required by the parser, but the current handler only checks that it exists and otherwise ignores its value.
- `move` plus exactly one of `--direction DIR`, `--workspace-in-direction DIR`, `--workspace NAME`, `--next-active-workspace`, `--prev-active-workspace`, `--next-workspace`, `--prev-workspace`, `--next-active-workspace-on-monitor`, `--prev-active-workspace-on-monitor`, `--recent-workspace`.
- `move-workspace --direction DIR`: move the current workspace to the adjacent monitor.
- `position --centered [--x-pos INTEGER --y-pos INTEGER]`: position a floating window; at least one option required.
- `resize --width LENGTH [--height LENGTH]`: adjust tiling size or floating dimensions; at least one required. `size` is the same command shape.
- `update-workspace-config [--workspace NAME] [--name STRING] [--display-name STRING] [--bind-to-monitor UINT] [--keep-alive BOOL]`: update at least one workspace setting.
- `set-floating [--shown-on-top[=BOOL]] [--centered[=BOOL]] [--x-pos INTEGER] [--y-pos INTEGER] [--width LENGTH] [--height LENGTH]`: enter floating state. Bare boolean switches mean true; false requires `=false`.
- `set-fullscreen [--shown-on-top[=BOOL]] [--maximized[=BOOL]]`: enter fullscreen state, using configured defaults for omitted values.
- `set-minimized`, `set-tiling`: enter the named state.
- `set-title-bar-visibility shown|hidden`: change native title-bar visibility.
- `set-transparency --opacity OPACITY [--opacity-delta SIGNED_OPACITY]`: set and/or adjust opacity; at least one required. Examples: `--opacity 95%`, `--opacity-delta -5%`.
- `shell-exec [--hide-window] PROGRAM [ARG ...]`: enqueue a process launch on
  the bounded Windows Shell worker; `--hide-window` requests a hidden initial
  window. Parsing, unavailable-worker, stopped-WM, and queue-full errors are
  returned immediately. A later native launch failure is written to the GlazeWM
  log because the command has already been acknowledged after enqueue. The queue
  holds at most 16 pending launches so a blocked Windows Shell call cannot stall
  the WM thread or cause unbounded growth.
- `toggle-floating [--shown-on-top[=BOOL]] [--centered[=BOOL]]`, `toggle-fullscreen [--shown-on-top[=BOOL]] [--maximized[=BOOL]]`, `toggle-minimized`, `toggle-tiling`: toggle states.
- `toggle-stack`: move the focused tiling window into/out of a stack.
- `cycle-stack-focus [--prev]`: cycle stack children; default is next.
- `focus-stack-index --index UINT`: focus a zero-based child index.
- `stack-absorb-neighbor --direction DIR`: combine the directional tiling neighbor with the focused window in a stack.
- `stack-insert`: stack with the most recently focused other tiling window, creating a stack when needed.
- `move-to-stack --name STRING`: move into/create a named stack on the current workspace.
- `toggle-tiling-direction`; `set-tiling-direction horizontal|vertical`.
- `set-workspace-layout bsp|split_horizontal|split_vertical|columns|rows|grid|vertical_stack|horizontal_stack`.
- `wm-cycle-focus [--omit-floating BOOL] [--omit-fullscreen BOOL] [--omit-minimized BOOL] [--omit-tiling BOOL]`: cycle state groups. Defaults are false, false, true, false respectively.
- `wm-enable-binding-mode --name STRING`; `wm-disable-binding-mode --name STRING`.
- `wm-exit`: cleanly stop the WM; `wm-redraw`: redraw all managed windows; `wm-reload-config`: reload YAML; `wm-toggle-pause`: pause/resume management.

### Queries, subscriptions, and raw IPC

```powershell
glazewm.exe query app-metadata
glazewm.exe query binding-modes
glazewm.exe query focused
glazewm.exe query tiling-direction
glazewm.exe query layout
glazewm.exe query monitors
glazewm.exe query windows
glazewm.exe query workspaces
glazewm.exe query paused

glazewm.exe sub --events focus_changed workspace_updated
glazewm.exe unsub --id SUBSCRIPTION_UUID
```

`query` also has alias `q`. Subscription event values are `all`, `application_exiting`, `binding_modes_changed`, `focus_changed`, `focused_container_moved`, `monitor_added`, `monitor_updated`, `monitor_removed`, `tiling_direction_changed`, `layout_changed`, `user_config_changed`, `window_managed`, `window_unmanaged`, `workspace_activated`, `workspace_deactivated`, `workspace_updated`, and `pause_changed`.

The local WebSocket IPC endpoint is fixed at `ws://127.0.0.1:6123`; there is no config key for the port. Responses are tagged with JSON `messageType` (`client_response` or `event_subscription`); event payloads use `eventType` with the values above and camelCase fields. The listener accepts only a locally connected process owned by the same Windows user, rejects browser-origin handshakes, and uses hard bounds for connections, message/frame size, command/response queues, subscriptions, handshake/authentication, idle time, and writes. Existing response JSON and command-success semantics are retained. See `IPC_MIGRATION.md` for exact limits and integration guidance.

### Process startup/config selection

```powershell
glazewm.exe start --config C:\path\to\config.yaml
glazewm.exe start --verbose
glazewm.exe start --quiet
```

`start` accepts `-c/--config`, `-v/--verbose`, or `-q/--quiet` (`verbose` and `quiet` conflict). With no subcommand, the executable defaults to `start`. Config precedence is command-line path, then `GLAZEWM_CONFIG_PATH`, then `%USERPROFILE%\.glzr\glazewm\config.yaml`; when the default file is absent, the embedded repository sample is created there.

## Defaults and portability cautions

- Every top-level section defaults if omitted, but list-item required fields (`workspace.name`, window-rule `commands`/`match`, binding-mode `name`) must be present when that item exists.
- The embedded sample intentionally overrides some source defaults (for example `toggle_workspace_on_refocus`, cursor jump, close animation, fullscreen maximization, borders). Values labeled as defaults in this document are source defaults, not necessarily sample choices.
- Windows-native effects and DComp animations are best-effort. Unsupported APIs should leave the real window usable; Mica has no acrylic fallback, while acrylic has a fixed-intensity fallback.
- Runtime commands such as workspace layout/config changes do not rewrite YAML and generally do not survive a process restart unless represented by startup commands/keybindings.
