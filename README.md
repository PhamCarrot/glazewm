# GlazeWM Combined

A personal fork of [florensm/glazewm](https://github.com/florensm/glazewm) combining two branches:

- `personal/dcomp`: smooth window movement, resizing, open/close, and workspace animations.
- `feat/layout-architecture`: Split Horizontal, Split Vertical, Columns, Rows, Grid, Vertical Stack, and Horizontal Stack layouts.

The integration also adds spatial focus and window movement for non-split layouts, preventing layouts such as Grid from unexpectedly changing back to a basic split.

The included packaging script can rebuild the Windows executables, portable ZIP, and unsigned x64 MSI.

## Personal Windows package

The portable archive includes a self-contained default configuration. From
the extracted package directory, start it with:

```powershell
.\glazewm.exe start --config .\config.yaml
```

The command-line client is available as `.\cli\glazewm.exe`. The MSI is an
unsigned per-machine installer, so Windows displays an Unknown publisher
warning and requests administrator approval during installation. The binaries
are intentionally built without UIAccess because unsigned UIAccess executables
are not supported by Windows.
