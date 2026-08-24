# GlazeWM Combined

A personal Windows fork of [GlazeWM](https://github.com/glzr-io/glazewm) focused on smooth animations, flexible layouts, and reliable window handling.

Built from and credited to [florensm's DComp branch](https://github.com/florensm/glazewm/tree/personal/dcomp) and [layout architecture branch](https://github.com/florensm/glazewm/tree/feat/layout-architecture).

## Features

- Smooth move, resize, open, close, focus, and workspace animations.
- Named easing curves, custom cubic Bézier curves, and multiple animation styles.
- BSP, split, columns, rows, grid, vertical stack, and horizontal stack layouts.
- Spatial focus and window movement that preserve the selected workspace layout.
- Safer animation fallbacks, synchronized focus, bounded IPC, and watcher recovery.
- Optional signed UIAccess packaging for managing elevated applications.

See [CONFIG_REFERENCE.md](CONFIG_REFERENCE.md) for every config option and [ANIMATION_REFERENCE.md](ANIMATION_REFERENCE.md) for easing and animation details. A complete starting config is included at `resources/assets/sample-config.yaml`.

## Personal Windows package

The packaging script builds `glazewm.exe`, `glazewm-watcher.exe`, the CLI, a portable ZIP, and an x64 MSI. From an extracted portable package, run:

```powershell
.\glazewm.exe start --config .\config.yaml
```

The CLI is available at `.\cli\glazewm.exe`. UIAccess builds must be signed with a trusted certificate and installed in a secure Windows location.
