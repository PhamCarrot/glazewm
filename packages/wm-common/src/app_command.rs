use std::path::PathBuf;

use clap::{error::KindFormatter, Args, Parser, ValueEnum};
use serde::{Deserialize, Deserializer, Serialize};
use tracing::Level;
use uuid::Uuid;
use wm_platform::{Delta, Direction, LengthValue, OpacityValue};

use crate::{
  parse_windows_command_line, TilingDirection, WorkspaceLayout,
};

/// Authoritative runtime version inherited from the Cargo workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Parser)]
#[clap(name = "glazewm", author, version = VERSION, about, long_about = None)]
pub enum AppCommand {
  /// Starts the window manager.
  Start {
    /// Custom path to user config file.
    ///
    /// The default path is `%userprofile%/.glzr/glazewm/config.yaml`
    #[clap(short = 'c', long = "config", value_hint = clap::ValueHint::FilePath)]
    config_path: Option<PathBuf>,

    #[clap(flatten)]
    verbosity: Verbosity,
  },

  /// Retrieves and outputs a specific part of the window manager's state.
  ///
  /// Requires an already running instance of the window manager.
  #[clap(alias = "q")]
  Query {
    #[clap(subcommand)]
    command: QueryCommand,
  },

  /// Invokes a window manager command.
  ///
  /// Requires an already running instance of the window manager.
  #[clap(alias = "c")]
  Command {
    #[clap(long = "id")]
    subject_container_id: Option<Uuid>,

    #[clap(subcommand)]
    command: InvokeCommand,
  },

  /// Subscribes to one or more WM events (e.g. `window_close`), and
  /// continuously outputs the incoming events.
  ///
  /// Requires an already running instance of the window manager.
  Sub {
    /// WM event(s) to subscribe to.
    #[clap(short = 'e', long, value_enum, num_args = 1..)]
    events: Vec<SubscribableEvent>,
  },

  /// Unsubscribes from a prior event subscription.
  ///
  /// Requires an already running instance of the window manager.
  Unsub {
    /// Subscription ID to unsubscribe from.
    #[clap(long = "id")]
    subscription_id: Uuid,
  },
}

impl AppCommand {
  /// Parses `AppCommand` from command line arguments.
  ///
  /// Defaults to `AppCommand::Start` if no arguments are provided.
  #[must_use]
  pub fn parse_with_default(args: &Vec<String>) -> Self {
    if args.len() == 1 {
      AppCommand::Start {
        config_path: None,
        verbosity: Verbosity {
          verbose: false,
          quiet: false,
        },
      }
    } else {
      AppCommand::parse_from(args)
    }
  }

  /// Returns stable, argument-free metadata suitable for diagnostics.
  ///
  /// This deliberately omits subject IDs, workspace names, executable
  /// paths, shell arguments, subscription IDs, and all other user data.
  #[must_use]
  pub fn diagnostic_metadata(&self) -> (&'static str, &'static str) {
    match self {
      Self::Start { .. } => ("control", "start"),
      Self::Query { command } => ("query", command.diagnostic_name()),
      Self::Command { command, .. } => {
        ("command", command.diagnostic_name())
      }
      Self::Sub { .. } => ("subscription", "subscribe"),
      Self::Unsub { .. } => ("subscription", "unsubscribe"),
    }
  }
}

/// Verbosity flags to be used with `#[command(flatten)]`.
#[derive(Args, Clone, Debug)]
#[clap(about = None, long_about = None)]
pub struct Verbosity {
  /// Enables verbose logging.
  #[clap(short = 'v', long, action)]
  verbose: bool,

  /// Disables logging.
  #[clap(short = 'q', long, action, conflicts_with = "verbose")]
  quiet: bool,
}

impl Verbosity {
  /// Gets the log level based on the verbosity flags.
  #[must_use]
  pub fn level(&self) -> Level {
    match (self.verbose, self.quiet) {
      (true, _) => Level::DEBUG,
      (_, true) => Level::ERROR,
      _ => Level::INFO,
    }
  }
}

#[derive(Clone, Debug, Parser)]
pub enum QueryCommand {
  /// Outputs metadata about the application (e.g. version number).
  AppMetadata,
  /// Outputs the active binding modes.
  BindingModes,
  /// Outputs the focused container (either a window or an empty
  /// workspace).
  Focused,
  /// Outputs the tiling direction of the focused direction container.
  TilingDirection,
  /// Outputs the layout of the focused direction container.
  Layout,
  /// Outputs all monitors.
  Monitors,
  /// Outputs all windows.
  Windows,
  /// Outputs all active workspaces.
  Workspaces,
  /// Outputs whether the window manager is paused.
  Paused,
}

impl QueryCommand {
  /// Returns the public command name without any user-provided data.
  #[must_use]
  pub const fn diagnostic_name(&self) -> &'static str {
    match self {
      Self::AppMetadata => "app-metadata",
      Self::BindingModes => "binding-modes",
      Self::Focused => "focused",
      Self::TilingDirection => "tiling-direction",
      Self::Layout => "layout",
      Self::Monitors => "monitors",
      Self::Windows => "windows",
      Self::Workspaces => "workspaces",
      Self::Paused => "paused",
    }
  }
}

#[derive(Clone, Debug, PartialEq, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum SubscribableEvent {
  All,
  ApplicationExiting,
  BindingModesChanged,
  FocusChanged,
  FocusedContainerMoved,
  MonitorAdded,
  MonitorUpdated,
  MonitorRemoved,
  TilingDirectionChanged,
  LayoutChanged,
  UserConfigChanged,
  WindowManaged,
  WindowUnmanaged,
  WorkspaceActivated,
  WorkspaceDeactivated,
  WorkspaceUpdated,
  PauseChanged,
}

#[derive(Clone, Debug, Parser, PartialEq, Serialize)]
pub enum InvokeCommand {
  AdjustBorders(InvokeAdjustBordersCommand),
  Close,
  Focus(InvokeFocusCommand),
  /// Marker command for window rules that forces matching windows to be
  /// managed, bypassing the built-in manageability checks (e.g. the
  /// `WS_EX_TOOLWINDOW` style check on Windows).
  ///
  /// Evaluated *before* a window is managed; running it against an
  /// already-managed window is a no-op.
  ForceManage,
  Ignore,
  MoveCursor(InvokeMoveCursorCommand),
  Move(InvokeMoveCommand),
  MoveWorkspace {
    #[clap(long)]
    direction: Direction,
  },
  Position(InvokePositionCommand),
  Resize(InvokeResizeCommand),
  UpdateWorkspaceConfig {
    #[clap(long, allow_hyphen_values = true)]
    workspace: Option<String>,
    #[clap(flatten)]
    new_config: InvokeUpdateWorkspaceConfig,
  },
  SetFloating {
    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    shown_on_top: Option<bool>,

    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    centered: Option<bool>,

    #[clap(long, allow_hyphen_values = true)]
    x_pos: Option<i32>,

    #[clap(long, allow_hyphen_values = true)]
    y_pos: Option<i32>,

    #[clap(long, allow_hyphen_values = true)]
    width: Option<LengthValue>,

    #[clap(long, allow_hyphen_values = true)]
    height: Option<LengthValue>,
  },
  SetFullscreen {
    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    shown_on_top: Option<bool>,

    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    maximized: Option<bool>,
  },
  SetMinimized,
  SetTiling,
  SetTitleBarVisibility {
    #[clap(required = true, value_enum)]
    visibility: TitleBarVisibility,
  },
  SetTransparency(SetTransparencyCommand),
  ShellExec {
    #[clap(long, action)]
    hide_window: bool,

    #[clap(required = true, trailing_var_arg = true)]
    command: Vec<String>,
  },
  // Reuse `InvokeResizeCommand` struct.
  Size(InvokeResizeCommand),
  ToggleFloating {
    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    shown_on_top: Option<bool>,

    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    centered: Option<bool>,
  },
  ToggleFullscreen {
    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    shown_on_top: Option<bool>,

    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    maximized: Option<bool>,
  },
  ToggleMinimized,
  ToggleTiling,
  /// Toggle the focused tiling window into or out of a stack container.
  ToggleStack,
  /// Cycle focus to the next or previous window within the parent stack.
  CycleStackFocus {
    #[clap(long, default_value_t = false)]
    prev: bool,
  },
  /// Focus the stack child at a specific zero-based index.
  FocusStackIndex {
    #[clap(long)]
    index: usize,
  },
  /// Absorb the adjacent tiling neighbor in the given direction into a
  /// stack with the focused window.
  StackAbsorbNeighbor {
    #[clap(long)]
    direction: Direction,
  },
  /// Move the focused window into a stack with the most-recently-focused
  /// other tiling window on the workspace. If no stack exists at the
  /// target, a new one is created.
  StackInsert,
  /// Move the focused tiling window into the named stack on the current
  /// workspace. Creates a new named stack if none exists yet.
  MoveToStack {
    #[clap(long)]
    name: String,
  },
  ToggleTilingDirection,
  SetTilingDirection {
    #[clap(required = true)]
    tiling_direction: TilingDirection,
  },
  SetWorkspaceLayout {
    #[clap(required = true)]
    layout: WorkspaceLayout,
  },
  WmCycleFocus {
    #[clap(long, default_value_t = false)]
    omit_floating: bool,

    #[clap(long, default_value_t = false)]
    omit_fullscreen: bool,

    #[clap(long, default_value_t = true)]
    omit_minimized: bool,

    #[clap(long, default_value_t = false)]
    omit_tiling: bool,
  },
  WmDisableBindingMode {
    #[clap(long)]
    name: String,
  },
  WmEnableBindingMode {
    #[clap(long)]
    name: String,
  },
  WmExit,
  WmRedraw,
  WmReloadConfig,
  WmTogglePause,
}

impl InvokeCommand {
  /// Returns the public command name without formatting command arguments.
  #[must_use]
  pub const fn diagnostic_name(&self) -> &'static str {
    match self {
      Self::AdjustBorders(_) => "adjust-borders",
      Self::Close => "close",
      Self::Focus(_) => "focus",
      Self::ForceManage => "force-manage",
      Self::Ignore => "ignore",
      Self::MoveCursor(_) => "move-cursor",
      Self::Move(_) => "move",
      Self::MoveWorkspace { .. } => "move-workspace",
      Self::Position(_) => "position",
      Self::Resize(_) => "resize",
      Self::UpdateWorkspaceConfig { .. } => "update-workspace-config",
      Self::SetFloating { .. } => "set-floating",
      Self::SetFullscreen { .. } => "set-fullscreen",
      Self::SetMinimized => "set-minimized",
      Self::SetTiling => "set-tiling",
      Self::SetTitleBarVisibility { .. } => "set-title-bar-visibility",
      Self::SetTransparency(_) => "set-transparency",
      Self::ShellExec { .. } => "shell-exec",
      Self::Size(_) => "size",
      Self::ToggleFloating { .. } => "toggle-floating",
      Self::ToggleFullscreen { .. } => "toggle-fullscreen",
      Self::ToggleMinimized => "toggle-minimized",
      Self::ToggleTiling => "toggle-tiling",
      Self::ToggleStack => "toggle-stack",
      Self::CycleStackFocus { .. } => "cycle-stack-focus",
      Self::FocusStackIndex { .. } => "focus-stack-index",
      Self::StackAbsorbNeighbor { .. } => "stack-absorb-neighbor",
      Self::StackInsert => "stack-insert",
      Self::MoveToStack { .. } => "move-to-stack",
      Self::ToggleTilingDirection => "toggle-tiling-direction",
      Self::SetTilingDirection { .. } => "set-tiling-direction",
      Self::SetWorkspaceLayout { .. } => "set-workspace-layout",
      Self::WmCycleFocus { .. } => "wm-cycle-focus",
      Self::WmDisableBindingMode { .. } => "wm-disable-binding-mode",
      Self::WmEnableBindingMode { .. } => "wm-enable-binding-mode",
      Self::WmExit => "wm-exit",
      Self::WmRedraw => "wm-redraw",
      Self::WmReloadConfig => "wm-reload-config",
      Self::WmTogglePause => "wm-toggle-pause",
    }
  }
}

impl<'de> Deserialize<'de> for InvokeCommand {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    // Clap expects an array of string slices where the first argument is
    // the binary name/path. When deserializing commands from the user
    // config, we therefore have to prepend an additional empty argument.
    let unparsed = String::deserialize(deserializer)?;
    let mut arguments = parse_windows_command_line(&unparsed)
      .map_err(serde::de::Error::custom)?;
    arguments.insert(0, String::new());

    InvokeCommand::try_parse_from(arguments).map_err(|err| {
      // Format the error message and remove the "error: " prefix.
      let err_msg = err.apply::<KindFormatter>().to_string();
      serde::de::Error::custom(err_msg.trim_start_matches("error: "))
    })
  }
}

#[derive(Clone, Debug, PartialEq, Serialize, ValueEnum)]
#[clap(rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum TitleBarVisibility {
  Shown,
  Hidden,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = true)]
pub struct InvokeAdjustBordersCommand {
  #[clap(long, allow_hyphen_values = true)]
  pub top: Option<LengthValue>,

  #[clap(long, allow_hyphen_values = true)]
  pub right: Option<LengthValue>,

  #[clap(long, allow_hyphen_values = true)]
  pub bottom: Option<LengthValue>,

  #[clap(long, allow_hyphen_values = true)]
  pub left: Option<LengthValue>,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = false)]
#[allow(clippy::struct_excessive_bools)]
pub struct InvokeFocusCommand {
  #[clap(long)]
  pub direction: Option<Direction>,

  #[clap(long)]
  pub container_id: Option<Uuid>,

  #[clap(long)]
  pub workspace_in_direction: Option<Direction>,

  #[clap(long)]
  pub workspace: Option<String>,

  #[clap(long)]
  pub monitor: Option<usize>,

  #[clap(long)]
  pub next_active_workspace: bool,

  #[clap(long)]
  pub prev_active_workspace: bool,

  #[clap(long)]
  pub next_workspace: bool,

  #[clap(long)]
  pub prev_workspace: bool,

  #[clap(long)]
  pub next_active_workspace_on_monitor: bool,

  #[clap(long)]
  pub prev_active_workspace_on_monitor: bool,

  #[clap(long)]
  pub recent_workspace: bool,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = false)]
pub struct InvokeMoveCursorCommand {
  /// Move the cursor to the center of the currently focused window.
  #[clap(long)]
  pub direction: Option<Direction>,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = false)]
#[allow(clippy::struct_excessive_bools)]
pub struct InvokeMoveCommand {
  /// Direction to move the window.
  #[clap(long)]
  pub direction: Option<Direction>,

  /// Move window to workspace in specified direction.
  #[clap(long)]
  pub workspace_in_direction: Option<Direction>,

  /// Name of workspace to move the window.
  #[clap(long)]
  pub workspace: Option<String>,

  #[clap(long)]
  pub next_active_workspace: bool,

  #[clap(long)]
  pub prev_active_workspace: bool,

  #[clap(long)]
  pub next_workspace: bool,

  #[clap(long)]
  pub prev_workspace: bool,

  #[clap(long)]
  pub next_active_workspace_on_monitor: bool,

  #[clap(long)]
  pub prev_active_workspace_on_monitor: bool,

  #[clap(long)]
  pub recent_workspace: bool,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = true)]
pub struct InvokeResizeCommand {
  #[clap(long, allow_hyphen_values = true)]
  pub width: Option<LengthValue>,

  #[clap(long, allow_hyphen_values = true)]
  pub height: Option<LengthValue>,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = true)]
pub struct SetTransparencyCommand {
  #[clap(long)]
  pub opacity: Option<OpacityValue>,

  #[clap(long, allow_hyphen_values = true)]
  pub opacity_delta: Option<Delta<OpacityValue>>,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = true)]
pub struct InvokePositionCommand {
  #[clap(long, action)]
  pub centered: bool,

  #[clap(long, allow_hyphen_values = true)]
  pub x_pos: Option<i32>,

  #[clap(long, allow_hyphen_values = true)]
  pub y_pos: Option<i32>,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = true)]
pub struct InvokeUpdateWorkspaceConfig {
  #[clap(long, allow_hyphen_values = true)]
  pub name: Option<String>,

  #[clap(long, allow_hyphen_values = true)]
  pub display_name: Option<String>,

  #[clap(long)]
  pub bind_to_monitor: Option<u32>,

  #[clap(long)]
  pub keep_alive: Option<bool>,
}

#[cfg(test)]
mod tests {
  use clap::CommandFactory;

  use super::*;

  #[test]
  fn command_line_version_matches_cargo_package_version() {
    assert_eq!(AppCommand::command().get_version(), Some(VERSION));
    assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
  }

  #[test]
  fn parses_upstream_and_custom_layout_queries() {
    let tiling_direction =
      AppCommand::try_parse_from(["glazewm", "query", "tiling-direction"])
        .expect("Failed to parse upstream tiling-direction query.");
    assert!(matches!(
      tiling_direction,
      AppCommand::Query {
        command: QueryCommand::TilingDirection
      }
    ));

    let layout =
      AppCommand::try_parse_from(["glazewm", "query", "layout"])
        .expect("Failed to parse custom layout query.");
    assert!(matches!(
      layout,
      AppCommand::Query {
        command: QueryCommand::Layout
      }
    ));
  }

  #[test]
  fn parses_upstream_and_custom_layout_subscriptions() {
    let command = AppCommand::try_parse_from([
      "glazewm",
      "sub",
      "--events",
      "tiling_direction_changed",
      "layout_changed",
    ])
    .expect("Failed to parse layout-related subscriptions.");

    let AppCommand::Sub { events } = command else {
      panic!("Expected subscription command.");
    };
    assert_eq!(
      events,
      vec![
        SubscribableEvent::TilingDirectionChanged,
        SubscribableEvent::LayoutChanged,
      ]
    );
  }

  #[test]
  fn parses_force_manage_command() {
    let command = InvokeCommand::try_parse_from(["", "force-manage"])
      .expect("Failed to parse `force-manage` command.");

    assert_eq!(command, InvokeCommand::ForceManage);
  }

  #[test]
  fn diagnostic_metadata_never_formats_sensitive_command_arguments() {
    let secret = "token=production-secret".to_string();
    let command = AppCommand::Command {
      subject_container_id: Some(Uuid::nil()),
      command: InvokeCommand::ShellExec {
        hide_window: true,
        command: vec!["private-tool.exe".to_string(), secret.clone()],
      },
    };

    let metadata = command.diagnostic_metadata();
    assert_eq!(metadata, ("command", "shell-exec"));
    let rendered = format!("{}:{}", metadata.0, metadata.1);
    assert!(!rendered.contains(&secret));
    assert!(!rendered.contains("private-tool.exe"));
    assert!(!rendered.contains(&Uuid::nil().to_string()));
  }

  #[test]
  fn config_deserialization_preserves_shell_argument_boundaries() {
    use serde::de::value::{Error, StrDeserializer};

    let command = InvokeCommand::deserialize(StrDeserializer::<Error>::new(
      r#"shell-exec pwsh.exe -Command "Write-Output \"hello world\"" "" "C:\path with space\\""#,
    ))
    .expect("quoted shell command should deserialize");

    assert_eq!(
      command,
      InvokeCommand::ShellExec {
        hide_window: false,
        command: vec![
          "pwsh.exe".to_string(),
          "-Command".to_string(),
          r#"Write-Output "hello world""#.to_string(),
          String::new(),
          "C:\\path with space\\".to_string(),
        ],
      }
    );
  }

  #[test]
  fn config_deserialization_rejects_unterminated_quotes() {
    use serde::de::value::{Error, StrDeserializer};

    let error = InvokeCommand::deserialize(StrDeserializer::<Error>::new(
      r#"shell-exec "unterminated"#,
    ))
    .expect_err("unterminated command should be rejected");

    assert!(error.to_string().contains("unterminated quote"));
  }

  #[test]
  fn transparency_commands_reject_invalid_opacity_before_dispatch() {
    for value in ["NaN", "inf", "-inf", "-0.1", "1.1", "-1%", "101%"] {
      let result = AppCommand::try_parse_from([
        "glazewm",
        "command",
        "set-transparency",
        "--opacity",
        value,
      ]);
      assert!(result.is_err(), "accepted absolute opacity {value}");
    }

    for value in ["+NaN", "+inf", "-inf", "+1.1", "-1.1", "+101%", "-101%"]
    {
      let result = AppCommand::try_parse_from([
        "glazewm",
        "command",
        "set-transparency",
        "--opacity-delta",
        value,
      ]);
      assert!(result.is_err(), "accepted opacity delta {value}");
    }
  }

  #[test]
  fn transparency_commands_accept_normalized_boundaries() {
    for value in ["0", "1", "0%", "100%"] {
      AppCommand::try_parse_from([
        "glazewm",
        "command",
        "set-transparency",
        "--opacity",
        value,
      ])
      .unwrap_or_else(|error| {
        panic!("rejected absolute opacity {value}: {error}")
      });
    }

    for value in ["+0", "+1", "-0", "-1", "+0%", "+100%", "-100%"] {
      AppCommand::try_parse_from([
        "glazewm",
        "command",
        "set-transparency",
        "--opacity-delta",
        value,
      ])
      .unwrap_or_else(|error| {
        panic!("rejected opacity delta {value}: {error}")
      });
    }
  }

  #[test]
  fn length_commands_reject_invalid_input_before_dispatch() {
    for value in [
      "junk100pxmore",
      "NaNpx",
      "infpx",
      "100001px",
      "1001%",
      "1e3px",
    ] {
      let result = AppCommand::try_parse_from([
        "glazewm", "command", "resize", "--width", value,
      ]);
      assert!(result.is_err(), "accepted resize length {value}");
    }
  }

  #[test]
  fn length_commands_accept_defined_sign_decimal_and_unit_syntax() {
    for value in ["0", "100000px", "-100000px", "+2.5%", "-.5%"] {
      AppCommand::try_parse_from([
        "glazewm", "command", "resize", "--width", value,
      ])
      .unwrap_or_else(|error| {
        panic!("rejected resize length {value}: {error}")
      });
    }
  }
}
