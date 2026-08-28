use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
  dtos::ContainerDto,
  parsed_config::{BindingModeConfig, ParsedConfig},
  TilingDirection, WorkspaceLayout,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
  tag = "eventType",
  rename_all = "snake_case",
  rename_all_fields = "camelCase"
)]
pub enum WmEvent {
  ApplicationExiting,
  BindingModesChanged {
    new_binding_modes: Vec<BindingModeConfig>,
  },
  FocusChanged {
    focused_container: ContainerDto,
  },
  FocusedContainerMoved {
    focused_container: ContainerDto,
  },
  MonitorAdded {
    added_monitor: ContainerDto,
  },
  MonitorRemoved {
    removed_id: Uuid,
    removed_device_name: String,
  },
  MonitorUpdated {
    updated_monitor: ContainerDto,
  },
  TilingDirectionChanged {
    direction_container: ContainerDto,
    new_tiling_direction: TilingDirection,
  },
  LayoutChanged {
    direction_container: ContainerDto,
    new_layout: WorkspaceLayout,
  },
  UserConfigChanged {
    config_path: String,
    config_string: String,
    parsed_config: ParsedConfig,
  },
  WindowManaged {
    managed_window: ContainerDto,
  },
  WindowUnmanaged {
    unmanaged_id: Uuid,
    unmanaged_handle: isize,
  },
  WorkspaceActivated {
    activated_workspace: ContainerDto,
  },
  WorkspaceDeactivated {
    deactivated_id: Uuid,
    deactivated_name: String,
  },
  WorkspaceUpdated {
    updated_workspace: ContainerDto,
  },
  PauseChanged {
    is_paused: bool,
  },
}

impl WmEvent {
  /// Returns an argument-free event name suitable for local diagnostics.
  ///
  /// Event DTOs can contain window titles, process names, paths, and the
  /// complete user configuration, so diagnostics must not format `self`.
  #[must_use]
  pub const fn diagnostic_name(&self) -> &'static str {
    match self {
      Self::ApplicationExiting => "application-exiting",
      Self::BindingModesChanged { .. } => "binding-modes-changed",
      Self::FocusChanged { .. } => "focus-changed",
      Self::FocusedContainerMoved { .. } => "focused-container-moved",
      Self::MonitorAdded { .. } => "monitor-added",
      Self::MonitorRemoved { .. } => "monitor-removed",
      Self::MonitorUpdated { .. } => "monitor-updated",
      Self::TilingDirectionChanged { .. } => "tiling-direction-changed",
      Self::LayoutChanged { .. } => "layout-changed",
      Self::UserConfigChanged { .. } => "user-config-changed",
      Self::WindowManaged { .. } => "window-managed",
      Self::WindowUnmanaged { .. } => "window-unmanaged",
      Self::WorkspaceActivated { .. } => "workspace-activated",
      Self::WorkspaceDeactivated { .. } => "workspace-deactivated",
      Self::WorkspaceUpdated { .. } => "workspace-updated",
      Self::PauseChanged { .. } => "pause-changed",
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ParsedConfig;

  #[test]
  fn config_event_diagnostic_omits_config_contents_and_paths() {
    let secret = "token=do-not-log";
    let event = WmEvent::UserConfigChanged {
      config_path: format!(r"C:\private\{secret}\config.yaml"),
      config_string: format!("startup_commands: [{secret}]"),
      parsed_config: ParsedConfig::default(),
    };

    let diagnostic = event.diagnostic_name();
    assert_eq!(diagnostic, "user-config-changed");
    assert!(!diagnostic.contains(secret));
    assert!(!diagnostic.contains("private"));
  }
}
