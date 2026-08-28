# GlazeWM Combined

A personal Windows fork of [GlazeWM](https://github.com/glzr-io/glazewm) focused on smooth animations, flexible layouts, and reliable window handling.

The authoritative source release is **0.0.6**. Cargo package metadata, CLI and
IPC version text, Windows executable resources, CI, and packaging all derive
from the single workspace version in `Cargo.toml`.

Built from and credited to [florensm's DComp branch](https://github.com/florensm/glazewm/tree/personal/dcomp) and [layout architecture branch](https://github.com/florensm/glazewm/tree/feat/layout-architecture).

## Features

- **Native Windows animation system.** DWM thumbnails and DirectComposition
  drive move, resize, open, close, focus, and workspace transitions without
  turning GlazeWM into a replacement system compositor.
- **Independent animation control.** Every transition family has its own
  duration, style, opacity, zoom, and easing settings. Exact quartic, quintic,
  exponential, Back/overshoot, spring, and custom cubic Bézier curves are
  supported.
- **Responsive workspace transitions.** Exact reversals reuse the current
  visual state without snapping, while incompatible rapid requests retain only
  the latest destination and use a bounded directional handoff.
- **Eight workspace layouts.** BSP, horizontal and vertical split, columns,
  rows, grid, vertical stack, and horizontal stack layouts are available per
  workspace, with runtime commands and IPC events for status-bar integration.
- **Fail-visible window handling.** Animation setup, handoff, timer, thumbnail,
  and cleanup failures restore surviving real windows instead of leaving them
  cloaked, transparent, hidden, or trapped beneath a surrogate.
- **Low-latency bounded scheduling.** Animation ticks remain deadline-aware
  while native events and IPC retain service guarantees. Event, connection,
  subscription, composition, watcher, and recovery work all have explicit
  capacity or deadline bounds.
- **Transactional configuration.** Unknown or unsafe values are rejected, and
  a failed reload preserves the active valid configuration. Window-rule regexes
  are compiled once per accepted configuration generation.
- **Hardened local IPC and crash recovery.** Existing WebSocket/JSON clients
  remain compatible, connections are authenticated to the current Windows
  user, and the watcher verifies HWND identity before restoring windows after
  an unexpected WM exit.

## What differs from the original animation fork

- **A complete multi-layout workspace system.** Each workspace can use BSP,
  horizontal or vertical split, columns, rows, grid, or either stack layout.
  Layout commands, queries, and change events remain separate from GlazeWM's
  nested tiling direction, so status bars such as YASB can track both correctly.
- **More expressive animation profiles.** Open, close, focus, move, resize, and
  workspace transitions have independent timing. Exact quartic, quintic,
  exponential, Back/overshoot, and spring easings are available alongside
  custom cubic Bézier curves and the original spatial styles.
- **Responsive rapid workspace switching.** Exact reversal reuses the active
  visual state without snapping. An incompatible third destination keeps only
  the latest requested workspace and performs a bounded directional reveal,
  avoiding an unbounded animation/surrogate queue or a second laggy full slide.
- **Fail-visible Windows lifecycle handling.** Transparent open/close handoff,
  generation-bound visibility leases, timer watchdog recovery, local participant
  failure isolation, and identity-checked watcher cleanup are designed so a
  surviving real window is restored instead of remaining cloaked, transparent,
  or trapped beneath a surrogate.
- **Hardened configuration and local IPC.** Reload is preflighted and
  transactional; unknown or unsafe values are rejected without replacing the
  active config. IPC retains the existing WebSocket/JSON contract while adding
  same-Windows-user authorization, connection/message/subscription limits,
  timeouts, Windows-correct argument quoting, and redacted rate-limited logs.
- **Reproducible release identity.** All workspace packages, CLI/IPC version
  text, Windows resources, CI, and packaging derive version `0.0.6` from the
  Cargo workspace, with a dated Rust nightly and fail-closed identity checks.

See [CONFIG_REFERENCE.md](CONFIG_REFERENCE.md) for every config option,
[ANIMATION_REFERENCE.md](ANIMATION_REFERENCE.md) for easing and transition
behavior, and [IPC_MIGRATION.md](IPC_MIGRATION.md) for the local IPC contract,
limits, security policy, quoting, and compatibility notes. A complete starting
config is included at `resources/assets/sample-config.yaml`.
