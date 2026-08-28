use std::{collections::HashMap, env, fs, path::PathBuf};

use anyhow::{Context, Result};
use regex::Regex;
use wm_common::{
  InvokeCommand, KeybindingConfig, MatchType, ParsedConfig,
  WindowMatchConfig, WindowRuleConfig, WindowRuleEvent, WorkspaceConfig,
};

use crate::{
  models::{Monitor, NativeWindowProperties, WindowContainer, Workspace},
  traits::{CommonGetters, WindowGetters},
};

/// Resource string for the sample config file.
const SAMPLE_CONFIG: &str =
  include_str!("../../../resources/assets/sample-config.yaml");

#[derive(Debug)]
pub struct UserConfig {
  /// Path to the user config file.
  pub path: PathBuf,

  /// Parsed user config value.
  pub value: ParsedConfig,

  /// Unparsed user config string.
  pub value_str: String,

  /// Hashmap of window rule event types (e.g. `WindowRuleEvent::Manage`)
  /// and the corresponding window rules of that type.
  window_rules_by_event: HashMap<WindowRuleEvent, Vec<CompiledWindowRule>>,

  /// Pre-compiled regexes from `stack.tab_title_overrides`, paired with
  /// their replacement strings. Built once on load and on config reload
  /// so `sync_tab_bars` does not recompile on every platform-sync tick.
  pub compiled_tab_title_overrides: Vec<(Regex, String)>,
}

#[derive(Clone, Debug)]
struct CompiledWindowRule {
  config: WindowRuleConfig,
  match_window: Vec<CompiledWindowMatchConfig>,
}

#[derive(Clone, Debug)]
struct CompiledWindowMatchConfig {
  process: Option<CompiledMatchType>,
  class: Option<CompiledMatchType>,
  title: Option<CompiledMatchType>,
}

#[derive(Clone, Debug)]
enum CompiledMatchType {
  Equals(String),
  Includes(String),
  Regex(Regex),
  NotEquals(String),
  NotRegex(Regex),
}

impl CompiledMatchType {
  fn compile(match_type: &MatchType, path: &str) -> Result<Self> {
    match match_type {
      MatchType::Equals { equals } => Ok(Self::Equals(equals.clone())),
      MatchType::Includes { includes } => {
        Ok(Self::Includes(includes.clone()))
      }
      MatchType::Regex { regex } => Regex::new(regex)
        .map(Self::Regex)
        .with_context(|| format!("Invalid regex at {path}: {regex:?}.")),
      MatchType::NotEquals { not_equals } => {
        Ok(Self::NotEquals(not_equals.clone()))
      }
      MatchType::NotRegex { not_regex } => {
        Regex::new(not_regex).map(Self::NotRegex).with_context(|| {
          format!("Invalid negative regex at {path}: {not_regex:?}.")
        })
      }
    }
  }

  fn is_match(&self, value: &str) -> bool {
    match self {
      Self::Equals(expected) => value == expected,
      Self::Includes(expected) => value.contains(expected),
      Self::Regex(regex) => regex.is_match(value),
      Self::NotEquals(expected) => value != expected,
      Self::NotRegex(regex) => !regex.is_match(value),
    }
  }
}

impl CompiledWindowMatchConfig {
  fn compile(config: &WindowMatchConfig, path: &str) -> Result<Self> {
    let compile = |match_type: &Option<MatchType>, field: &str| {
      match_type
        .as_ref()
        .map(|value| {
          CompiledMatchType::compile(value, &format!("{path}.{field}"))
        })
        .transpose()
    };

    Ok(Self {
      process: compile(&config.window_process, "window_process")?,
      class: compile(&config.window_class, "window_class")?,
      title: compile(&config.window_title, "window_title")?,
    })
  }
}

impl CompiledWindowRule {
  fn compile(config: &WindowRuleConfig, path: &str) -> Result<Self> {
    let match_window = config
      .match_window
      .iter()
      .enumerate()
      .map(|(index, match_config)| {
        CompiledWindowMatchConfig::compile(
          match_config,
          &format!("{path}.match[{index}]"),
        )
      })
      .collect::<Result<Vec<_>>>()?;

    Ok(Self {
      config: config.clone(),
      match_window,
    })
  }
}

impl UserConfig {
  #[cfg(test)]
  pub(crate) fn for_test(value: ParsedConfig) -> Self {
    let window_rules_by_event = Self::window_rules_by_event(&value)
      .expect("test window-rule regexes should compile");
    let compiled_tab_title_overrides =
      Self::compile_tab_title_overrides(&value);

    Self {
      path: PathBuf::from("test-config.yaml"),
      value,
      value_str: String::new(),
      window_rules_by_event,
      compiled_tab_title_overrides,
    }
  }

  /// Creates an instance of `UserConfig`. Reads and validates the user
  /// config from the given path.
  ///
  /// Creates a new config file from sample if it doesn't exist.
  pub fn new(config_path: Option<PathBuf>) -> anyhow::Result<Self> {
    let default_config_path = home::home_dir()
      .context("Unable to get home directory.")?
      .join(".glzr/glazewm/config.yaml");

    let config_path = config_path
      .or_else(|| env::var("GLAZEWM_CONFIG_PATH").ok().map(PathBuf::from))
      .unwrap_or(default_config_path);

    let (config_value, config_str) = Self::read(&config_path)?;

    let window_rules_by_event =
      Self::window_rules_by_event(&config_value)?;
    let compiled_tab_title_overrides =
      Self::compile_tab_title_overrides(&config_value);

    Ok(Self {
      path: config_path,
      value: config_value,
      value_str: config_str,
      window_rules_by_event,
      compiled_tab_title_overrides,
    })
  }

  /// Reads and validates the user config from the given path.
  ///
  /// Creates a new config file from sample if it doesn't exist.
  fn read(
    config_path: &PathBuf,
  ) -> anyhow::Result<(ParsedConfig, String)> {
    if !config_path.exists() {
      Self::create_sample(config_path)?;
    }

    let config_str = fs::read_to_string(config_path)
      .context("Unable to read config file.")?;

    // TODO: Improve error formatting of serde_yaml errors. Something
    // similar to https://github.com/AlexanderThaller/format_serde_error
    let config_value: ParsedConfig = serde_yaml::from_str(&config_str)?;
    config_value
      .validate()
      .map_err(anyhow::Error::msg)
      .context("Invalid configuration.")?;

    Ok((config_value, config_str))
  }

  /// Initializes a new config file from the sample config resource.
  fn create_sample(config_path: &PathBuf) -> Result<()> {
    let parent_dir =
      config_path.parent().context("Invalid config path.")?;

    fs::create_dir_all(parent_dir).with_context(|| {
      format!("Unable to create directory {}.", config_path.display())
    })?;

    fs::write(config_path, SAMPLE_CONFIG).with_context(|| {
      format!("Unable to write to {}.", config_path.display())
    })?;

    Ok(())
  }

  #[cfg(test)]
  pub fn reload(&mut self) -> anyhow::Result<()> {
    let candidate = self.load_candidate()?;
    *self = candidate;

    Ok(())
  }

  /// Loads and derives a complete replacement config without mutating the
  /// currently active generation.
  pub fn load_candidate(&self) -> anyhow::Result<Self> {
    let (config_value, config_str) = Self::read(&self.path)?;

    let window_rules_by_event =
      Self::window_rules_by_event(&config_value)?;
    let compiled_tab_title_overrides =
      Self::compile_tab_title_overrides(&config_value);

    Ok(Self {
      path: self.path.clone(),
      value: config_value,
      value_str: config_str,
      window_rules_by_event,
      compiled_tab_title_overrides,
    })
  }

  fn default_window_rules(
    config_value: &ParsedConfig,
  ) -> Vec<WindowRuleConfig> {
    let mut window_rules = Vec::new();

    let floating_defaults =
      &config_value.window_behavior.state_defaults.floating;

    // Default float rules.
    window_rules.push(WindowRuleConfig {
      commands: vec![InvokeCommand::SetFloating {
        centered: Some(floating_defaults.centered),
        shown_on_top: Some(floating_defaults.shown_on_top),
        x_pos: None,
        y_pos: None,
        width: None,
        height: None,
      }],
      match_window: vec![
        WindowMatchConfig {
          window_class: Some(MatchType::Equals { equals:
          // W10/W11 system dialog shown when moving and deleting files.
          "OperationStatusWindow".to_string(),
        }),
          ..WindowMatchConfig::default()
        },
        WindowMatchConfig {
          window_class: Some(MatchType::Equals { equals:
          // W10/W11 system dialogs (e.g. File Explorer save/open dialog).
          "#32770".to_string(),
        }),
          ..WindowMatchConfig::default()
        },
      ],
      on: vec![WindowRuleEvent::Manage],
      run_once: true,
    });

    // Default ignore rules.
    window_rules.push(WindowRuleConfig {
      commands: vec![InvokeCommand::Ignore],
      match_window: vec![
        WindowMatchConfig {
          window_process: Some(MatchType::Equals {
            equals: "SearchApp".to_string(),
          }),
          ..WindowMatchConfig::default()
        },
        WindowMatchConfig {
          window_process: Some(MatchType::Equals {
            equals: "SearchHost".to_string(),
          }),
          ..WindowMatchConfig::default()
        },
        WindowMatchConfig {
          window_process: Some(MatchType::Equals {
            equals: "ShellExperienceHost".to_string(),
          }),
          ..WindowMatchConfig::default()
        },
        WindowMatchConfig {
          window_process: Some(MatchType::Equals {
            // W10/11 start menu.
            equals: "StartMenuExperienceHost".to_string(),
          }),
          ..WindowMatchConfig::default()
        },
        WindowMatchConfig {
          window_process: Some(MatchType::Equals {
            // W10/11 screen snipping tool.
            equals: "ScreenClippingHost".to_string(),
          }),
          ..WindowMatchConfig::default()
        },
        WindowMatchConfig {
          window_process: Some(MatchType::Equals {
            // W11 lock screen.
            equals: "LockApp".to_string(),
          }),
          ..WindowMatchConfig::default()
        },
      ],
      on: vec![WindowRuleEvent::Manage],
      run_once: true,
    });

    window_rules
  }

  /// Compiles `stack.tab_title_overrides` regexes once so `sync_tab_bars`
  /// can apply them without recompiling on every platform-sync tick.
  ///
  /// Invalid regex patterns are logged and skipped rather than propagated
  /// as errors, so a single bad pattern does not break the entire tab bar.
  fn compile_tab_title_overrides(
    config_value: &ParsedConfig,
  ) -> Vec<(Regex, String)> {
    config_value
      .stack
      .tab_title_overrides
      .iter()
      .filter_map(|o| match Regex::new(&o.regex) {
        Ok(re) => Some((re, o.replace.clone())),
        Err(err) => {
          tracing::warn!(
            "Invalid tab_title_overrides regex {:?}: {err}",
            o.regex
          );
          None
        }
      })
      .collect()
  }

  fn window_rules_by_event(
    config_value: &ParsedConfig,
  ) -> Result<HashMap<WindowRuleEvent, Vec<CompiledWindowRule>>> {
    let mut window_rules_by_event = HashMap::new();

    // Compile user-defined and built-in rules once per accepted config
    // generation. Event buckets clone the compiled regex handles rather
    // than rebuilding regex automata while windows are being matched.
    let default_window_rules = Self::default_window_rules(config_value);
    let all_window_rules = config_value
      .window_rules
      .iter()
      .enumerate()
      .map(|(index, rule)| (format!("window_rules[{index}]"), rule))
      .chain(default_window_rules.iter().enumerate().map(
        |(index, rule)| (format!("built-in window rule {index}"), rule),
      ));

    for (path, window_rule) in all_window_rules {
      let compiled_rule = CompiledWindowRule::compile(window_rule, &path)?;
      for event_type in &compiled_rule.config.on {
        window_rules_by_event
          .entry(event_type.clone())
          .or_insert_with(Vec::new)
          .push(compiled_rule.clone());
      }
    }

    Ok(window_rules_by_event)
  }

  /// Window rules that should be applied to the window when the given
  /// event occurs.
  pub fn pending_window_rules(
    &self,
    window: &WindowContainer,
    event: &WindowRuleEvent,
  ) -> Vec<WindowRuleConfig> {
    let native_properties = window.native_properties();

    let Some(rules) = self.window_rules_by_event.get(event) else {
      return Vec::new();
    };

    rules
      .iter()
      .filter(|rule| {
        // Skip if window has already ran the rule.
        !window.done_window_rules().contains(&rule.config)
          && Self::rule_matches(rule, &native_properties)
      })
      .map(|rule| rule.config.clone())
      .collect()
  }

  /// Whether a window with the given native properties is matched by a
  /// `force-manage` window rule on the `manage` event.
  ///
  /// Used to bypass the built-in manageability checks before the window
  /// enters the window rule pipeline.
  pub fn is_force_managed(
    &self,
    properties: &NativeWindowProperties,
  ) -> bool {
    self
      .window_rules_by_event
      .get(&WindowRuleEvent::Manage)
      .is_some_and(|rules| {
        rules.iter().any(|rule| {
          rule.config.commands.contains(&InvokeCommand::ForceManage)
            && Self::rule_matches(rule, properties)
        })
      })
  }

  /// Whether a window with the given native properties matches any of
  /// the window rule's match configs.
  fn rule_matches(
    rule: &CompiledWindowRule,
    properties: &NativeWindowProperties,
  ) -> bool {
    rule.match_window.iter().any(|match_config| {
      let is_process_match =
        match_config.process.as_ref().is_none_or(|match_type| {
          // TODO: Temp fix for matching Zebar on both platforms with
          // the same process name. Consider using lowercase for every
          // `equals` match type.
          if properties.process_name == "Zebar" {
            match_type.is_match("Zebar") || match_type.is_match("zebar")
          } else {
            match_type.is_match(&properties.process_name)
          }
        });

      let is_class_match = {
        #[cfg(target_os = "windows")]
        {
          match_config.class.as_ref().is_none_or(|match_type| {
            match_type.is_match(&properties.class_name)
          })
        }
        #[cfg(not(target_os = "windows"))]
        {
          match_config.class.is_none()
        }
      };

      let is_title_match = match_config
        .title
        .as_ref()
        .is_none_or(|match_type| match_type.is_match(&properties.title));

      is_process_match && is_class_match && is_title_match
    })
  }

  pub fn inactive_workspace_configs(
    &self,
    active_workspaces: &[Workspace],
  ) -> Vec<&WorkspaceConfig> {
    self
      .value
      .workspaces
      .iter()
      .filter(|config| {
        !active_workspaces
          .iter()
          .any(|workspace| workspace.config().name == config.name)
      })
      .collect()
  }

  pub fn workspace_config_for_monitor(
    &self,
    monitor: &Monitor,
    active_workspaces: &[Workspace],
  ) -> Option<&WorkspaceConfig> {
    let inactive_configs =
      self.inactive_workspace_configs(active_workspaces);

    inactive_configs.into_iter().find(|&config| {
      config
        .bind_to_monitor
        .as_ref()
        .is_some_and(|monitor_index| {
          monitor.index() == *monitor_index as usize
        })
    })
  }

  /// Gets the first inactive workspace config, prioritizing configs that
  /// don't have a monitor binding.
  pub fn next_inactive_workspace_config(
    &self,
    active_workspaces: &[Workspace],
  ) -> Option<&WorkspaceConfig> {
    let inactive_configs =
      self.inactive_workspace_configs(active_workspaces);

    inactive_configs
      .iter()
      .find(|config| config.bind_to_monitor.is_none())
      .or(inactive_configs.first())
      .copied()
  }

  pub fn workspace_config_index(
    &self,
    workspace_name: &str,
  ) -> Option<usize> {
    self
      .value
      .workspaces
      .iter()
      .position(|config| config.name == workspace_name)
  }

  pub fn sort_workspaces(&self, workspaces: &mut [Workspace]) {
    workspaces.sort_by_key(|workspace| {
      self.workspace_config_index(&workspace.config().name)
    });
  }

  /// Keybinding configs that should be active for the current binding mode
  /// and pause state.
  ///
  /// When paused, only the configs with `InvokeCommand::WmTogglePause` are
  /// returned so that unpausing remains possible.
  pub fn active_keybinding_configs(
    &self,
    binding_modes: &[wm_common::BindingModeConfig],
    is_paused: bool,
  ) -> impl Iterator<Item = KeybindingConfig> {
    let source_configs = if let Some(first_mode) = binding_modes.first() {
      &first_mode.keybindings
    } else {
      &self.value.keybindings
    }
    .clone();

    source_configs.into_iter().filter(move |kb| {
      if is_paused {
        kb.commands
          .contains(&wm_common::InvokeCommand::WmTogglePause)
      } else {
        true
      }
    })
  }
}

#[cfg(test)]
mod tests {
  use wm_common::{
    EasingFunction, ParsedConfig, WindowTransitionStyle, WorkspaceLayout,
    WorkspaceSwitchDirection, WorkspaceSwitchIrisOrigin,
    WorkspaceSwitchStyle,
  };
  use wm_platform::{BackdropStyle, Rect};

  use super::*;

  /// The bundled sample config (which uses the `type` key for animation
  /// transition types) must always parse.
  #[test]
  fn sample_config_parses() {
    let config: ParsedConfig = serde_yaml::from_str(SAMPLE_CONFIG)
      .expect("sample config should parse");
    config.validate().expect("sample config should validate");

    assert_eq!(
      config.animations.window_open.style,
      WindowTransitionStyle::SlideRight
    );
    assert_eq!(
      config.animations.workspace_switch.style,
      WorkspaceSwitchStyle::Slide
    );
  }

  fn assert_unknown_config_key_is_rejected(yaml: &str, unknown_key: &str) {
    let error = serde_yaml::from_str::<ParsedConfig>(yaml)
      .expect_err("unknown config key must be rejected")
      .to_string();
    assert!(
      error.contains("unknown field"),
      "missing actionable unknown-field diagnostic for {unknown_key}: {error}"
    );
    assert!(
      error.contains(unknown_key),
      "diagnostic omitted unknown key {unknown_key}: {error}"
    );
  }

  #[test]
  fn unknown_root_and_nested_config_keys_are_rejected() {
    assert_unknown_config_key_is_rejected("animtions: {}\n", "animtions");
    assert_unknown_config_key_is_rejected(
      "animations:\n  workspace_switch:\n    duration_mss: 360\n",
      "duration_mss",
    );
  }

  #[test]
  fn unknown_keys_in_config_list_entries_are_rejected() {
    assert_unknown_config_key_is_rejected(
      "workspaces:\n  - name: '1'\n    naem: typo\n",
      "naem",
    );
    assert_unknown_config_key_is_rejected(
      "window_rules:\n  - commands: ['ignore']\n    match:\n      - window_titel:\n          equals: 'Terminal'\n",
      "window_titel",
    );
  }

  #[test]
  fn unknown_keys_in_structured_config_values_are_rejected() {
    let cases = [
      "gaps:\n  outer_gap:\n    left: '0px'\n    top: '0px'\n    right: '0px'\n    bottom: '0px'\n    botom: '10px'\n",
      "gaps:\n  inner_gap:\n    amount: 10\n    unit: pixel\n    amuont: 20\n",
      "window_effects:\n  focused_window:\n    border:\n      color:\n        r: 1\n        g: 2\n        b: 3\n        a: 255\n        alpha: 1\n",
    ];

    for yaml in cases {
      assert!(
        serde_yaml::from_str::<ParsedConfig>(yaml).is_err(),
        "accepted unknown key in structured config value:\n{yaml}"
      );
    }
  }

  #[test]
  fn unknown_key_reload_preserves_previous_valid_config() {
    let test_root = std::env::current_dir()
      .expect("test working directory should exist")
      .join("target")
      .join(format!("cfg001-reload-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&test_root)
      .expect("source-local reload test directory should be created");
    let config_path = test_root.join("config.yaml");
    let valid_yaml = "general:\n  focus_follows_cursor: true\n";
    fs::write(&config_path, valid_yaml)
      .expect("valid test config should be written");

    let mut config = UserConfig::new(Some(config_path.clone()))
      .expect("valid test config should load");
    let previous_value_str = config.value_str.clone();
    assert!(config.value.general.focus_follows_cursor);

    fs::write(
      &config_path,
      "general:\n  focus_follows_cursor: false\n  focus_follow_cursor: true\n",
    )
    .expect("invalid reload config should be written");
    let error =
      config.reload().expect_err("unknown key must reject reload");

    assert!(error.to_string().contains("unknown field"));
    assert!(config.value.general.focus_follows_cursor);
    assert_eq!(config.value_str, previous_value_str);

    fs::remove_dir_all(&test_root)
      .expect("source-local reload test directory should be removed");
  }

  #[test]
  fn out_of_range_animation_reload_preserves_previous_valid_config() {
    let test_root = std::env::current_dir()
      .expect("test working directory should exist")
      .join("target")
      .join(format!("cfg002-reload-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&test_root)
      .expect("source-local reload test directory should be created");
    let config_path = test_root.join("config.yaml");
    let valid_yaml =
      "animations:\n  workspace_switch:\n    duration_ms: 360\n";
    fs::write(&config_path, valid_yaml)
      .expect("valid test config should be written");

    let mut config = UserConfig::new(Some(config_path.clone()))
      .expect("valid test config should load");
    let previous_value_str = config.value_str.clone();
    assert_eq!(config.value.animations.workspace_switch.duration_ms, 360);

    fs::write(
      &config_path,
      "animations:\n  workspace_switch:\n    duration_ms: 10001\n",
    )
    .expect("out-of-range reload config should be written");
    let error = config
      .reload()
      .expect_err("out-of-range animation duration must reject reload");

    assert!(error.to_string().contains("Invalid configuration"));
    assert_eq!(config.value.animations.workspace_switch.duration_ms, 360);
    assert_eq!(config.value_str, previous_value_str);

    fs::remove_dir_all(&test_root)
      .expect("source-local reload test directory should be removed");
  }

  #[test]
  fn invalid_string_and_structured_lengths_are_rejected() {
    for yaml in [
      "gaps:\n  inner_gap: 'junk100pxmore'\n",
      "gaps:\n  inner_gap: '100001px'\n",
      "gaps:\n  inner_gap: '1001%'\n",
      "gaps:\n  inner_gap:\n    amount: .nan\n    unit: pixel\n",
      "gaps:\n  inner_gap:\n    amount: .inf\n    unit: percentage\n",
      "gaps:\n  inner_gap:\n    amount: 100001\n    unit: pixel\n",
      "gaps:\n  inner_gap:\n    amount: 10.01\n    unit: percentage\n",
    ] {
      assert!(
        serde_yaml::from_str::<ParsedConfig>(yaml).is_err(),
        "accepted invalid length config:\n{yaml}"
      );
    }
  }

  #[test]
  fn decimal_length_config_preserves_units() {
    let config: ParsedConfig = serde_yaml::from_str(
      "gaps:\n  inner_gap: '1.5px'\n  outer_gap:\n    top: '-2.5%'\n    right: '.5px'\n    bottom: '1.px'\n    left: '+2%'\n",
    )
    .expect("defined decimal length forms should parse");

    assert_eq!(config.gaps.inner_gap.amount, 1.5);
    assert_eq!(config.gaps.outer_gap.top.amount, -0.025);
    assert_eq!(config.gaps.outer_gap.right.amount, 0.5);
    assert_eq!(config.gaps.outer_gap.bottom.amount, 1.0);
    assert_eq!(config.gaps.outer_gap.left.amount, 0.02);
  }

  #[test]
  fn invalid_length_reload_preserves_previous_geometry_config() {
    let test_root = std::env::current_dir()
      .expect("test working directory should exist")
      .join("target")
      .join(format!("cfg003-reload-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&test_root)
      .expect("source-local reload test directory should be created");
    let config_path = test_root.join("config.yaml");
    let valid_yaml = "gaps:\n  inner_gap: '20px'\n";
    fs::write(&config_path, valid_yaml)
      .expect("valid test config should be written");

    let mut config = UserConfig::new(Some(config_path.clone()))
      .expect("valid test config should load");
    let previous_value_str = config.value_str.clone();
    assert_eq!(config.value.gaps.inner_gap.amount, 20.0);

    fs::write(&config_path, "gaps:\n  inner_gap: 'junk100pxmore'\n")
      .expect("invalid reload config should be written");
    config
      .reload()
      .expect_err("invalid length must reject reload");

    assert_eq!(config.value.gaps.inner_gap.amount, 20.0);
    assert_eq!(config.value_str, previous_value_str);

    fs::remove_dir_all(&test_root)
      .expect("source-local reload test directory should be removed");
  }

  #[test]
  fn invalid_workspace_identity_reload_preserves_previous_config() {
    let test_root = std::env::current_dir()
      .expect("test working directory should exist")
      .join("target")
      .join(format!("cfg005-reload-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&test_root)
      .expect("source-local reload test directory should be created");
    let config_path = test_root.join("config.yaml");
    let valid_yaml = "workspaces:\n  - name: 'Dev'\n  - name: 'Web'\n";
    fs::write(&config_path, valid_yaml)
      .expect("valid workspace config should be written");

    let mut config = UserConfig::new(Some(config_path.clone()))
      .expect("valid workspace config should load");
    let previous_value_str = config.value_str.clone();
    let previous_names = config
      .value
      .workspaces
      .iter()
      .map(|workspace| workspace.name.clone())
      .collect::<Vec<_>>();

    for invalid_yaml in [
      "workspaces:\n  - name: 'Dev'\n  - name: 'Dev'\n",
      "workspaces:\n  - name: '   '\n",
      "workspaces:\n  - name: ' Dev'\n",
    ] {
      fs::write(&config_path, invalid_yaml)
        .expect("invalid workspace config should be written");
      let error = config
        .reload()
        .expect_err("invalid workspace identity must reject reload");

      assert!(error.to_string().contains("Invalid configuration"));
      assert_eq!(
        config
          .value
          .workspaces
          .iter()
          .map(|workspace| workspace.name.clone())
          .collect::<Vec<_>>(),
        previous_names
      );
      assert_eq!(config.value_str, previous_value_str);
    }

    fs::remove_dir_all(&test_root)
      .expect("source-local reload test directory should be removed");
  }

  #[test]
  fn workspace_default_layout_is_optional_and_accepts_every_layout() {
    let legacy: ParsedConfig = serde_yaml::from_str(
      "workspaces:\n  - name: legacy\n",
    )
    .expect("workspace configs without default_layout must remain valid");
    assert_eq!(legacy.workspaces[0].default_layout, None);

    let yaml = r"
workspaces:
  - name: bsp
    default_layout: bsp
  - name: split-horizontal
    default_layout: split_horizontal
  - name: split-vertical
    default_layout: split_vertical
  - name: columns
    default_layout: columns
  - name: rows
    default_layout: rows
  - name: grid
    default_layout: grid
  - name: vertical-stack
    default_layout: vertical_stack
  - name: horizontal-stack
    default_layout: horizontal_stack
";
    let config: ParsedConfig = serde_yaml::from_str(yaml)
      .expect("every public workspace layout name must parse");

    assert_eq!(
      config
        .workspaces
        .iter()
        .map(|workspace| workspace.default_layout.clone().unwrap())
        .collect::<Vec<_>>(),
      vec![
        WorkspaceLayout::Bsp,
        WorkspaceLayout::SplitHorizontal,
        WorkspaceLayout::SplitVertical,
        WorkspaceLayout::Columns,
        WorkspaceLayout::Rows,
        WorkspaceLayout::Grid,
        WorkspaceLayout::VerticalStack,
        WorkspaceLayout::HorizontalStack,
      ]
    );
  }

  #[test]
  fn invalid_workspace_default_layout_reports_the_value_and_choices() {
    let error = serde_yaml::from_str::<ParsedConfig>(
      "workspaces:\n  - name: invalid\n    default_layout: diagonal\n",
    )
    .expect_err(
      "unknown workspace layouts must fail configuration parsing",
    )
    .to_string();

    assert!(error.contains("diagonal"));
    assert!(error.contains("bsp"));
    assert!(error.contains("horizontal_stack"));
  }

  #[test]
  fn non_finite_animation_values_are_rejected() {
    let mut config = ParsedConfig::default();
    config.animations.workspace_switch.opacity_incoming = f32::NAN;

    let error = config
      .validate()
      .expect_err("NaN animation opacity must be rejected");
    assert!(error.contains("opacity_incoming"));
  }

  #[test]
  fn normalized_effect_values_must_stay_in_range() {
    for value in
      [".nan", ".inf", "-.inf", "-0.1", "1.1", "'-1%'", "'101%'"]
    {
      let yaml = format!(
        "window_effects:\n  focused_window:\n    transparency:\n      opacity: {value}\n"
      );
      assert!(
        serde_yaml::from_str::<ParsedConfig>(&yaml).is_err(),
        "accepted config opacity {value}"
      );
    }
  }

  #[test]
  fn non_finite_cubic_bezier_control_point_is_rejected() {
    let yaml = r"
animations:
  window_move:
    easing: 'cubic_bezier(0.2, NaN, 0.8, 1.0)'
";

    let error = serde_yaml::from_str::<ParsedConfig>(yaml)
      .expect_err("non-finite easing must fail to parse");
    assert!(error.to_string().contains("must be finite"));
  }

  #[test]
  fn exact_named_easings_parse_and_round_trip() {
    let cases = [
      ("linear", EasingFunction::CubicBezier(0.0, 0.0, 1.0, 1.0)),
      ("ease_in", EasingFunction::CubicBezier(0.42, 0.0, 1.0, 1.0)),
      ("ease_out", EasingFunction::CubicBezier(0.0, 0.0, 0.58, 1.0)),
      (
        "ease_in_out",
        EasingFunction::CubicBezier(0.42, 0.0, 0.58, 1.0),
      ),
      (
        "ease_in_cubic",
        EasingFunction::CubicBezier(0.32, 0.0, 0.67, 0.0),
      ),
      (
        "ease_out_cubic",
        EasingFunction::CubicBezier(0.33, 1.0, 0.68, 1.0),
      ),
      (
        "ease_in_out_cubic",
        EasingFunction::CubicBezier(0.65, 0.0, 0.35, 1.0),
      ),
      ("ease_in_quart", EasingFunction::EaseInQuart),
      ("ease_out_quart", EasingFunction::EaseOutQuart),
      ("ease_in_out_quart", EasingFunction::EaseInOutQuart),
      ("ease_in_quint", EasingFunction::EaseInQuint),
      ("ease_out_quint", EasingFunction::EaseOutQuint),
      ("ease_in_out_quint", EasingFunction::EaseInOutQuint),
      ("ease_in_expo", EasingFunction::EaseInExpo),
      ("ease_out_expo", EasingFunction::EaseOutExpo),
      ("ease_in_out_expo", EasingFunction::EaseInOutExpo),
      ("ease_in_back", EasingFunction::EaseInBack),
      ("ease_out_back", EasingFunction::EaseOutBack),
      ("ease_in_out_back", EasingFunction::EaseInOutBack),
      ("ease_out_spring", EasingFunction::EaseOutSpring),
    ];

    for (name, expected) in cases {
      let parsed: EasingFunction = serde_yaml::from_str(name)
        .unwrap_or_else(|_| panic!("{name} should parse"));
      assert_eq!(parsed, expected);

      let serialized = serde_yaml::to_string(&parsed)
        .expect("named easing should serialize");
      let reparsed: EasingFunction = serde_yaml::from_str(&serialized)
        .expect("serialized named easing should parse");
      assert_eq!(reparsed, expected);
    }

    for (alias, canonical, expected) in [
      (
        "ease_in_overshoot",
        "ease_in_back",
        EasingFunction::EaseInBack,
      ),
      (
        "ease_out_overshoot",
        "ease_out_back",
        EasingFunction::EaseOutBack,
      ),
      (
        "ease_in_out_overshoot",
        "ease_in_out_back",
        EasingFunction::EaseInOutBack,
      ),
    ] {
      let parsed: EasingFunction =
        serde_yaml::from_str(alias).expect("overshoot alias should parse");
      assert_eq!(parsed, expected);
      assert_eq!(
        serde_yaml::to_string(&parsed)
          .expect("Back easing should serialize")
          .trim(),
        canonical
      );
    }
  }

  #[test]
  fn custom_bezier_parser_enforces_its_public_contract() {
    let overshoot: EasingFunction =
      serde_yaml::from_str("'cubic_bezier(0.2, -0.5, 0.8, 1.5)'")
        .expect("finite y control points may overshoot");
    assert_eq!(
      overshoot,
      EasingFunction::CubicBezier(0.2, -0.5, 0.8, 1.5)
    );

    for invalid in [
      "'cubic_bezier()'",
      "'cubic_bezier(0.2, 0.3, 0.8)'",
      "'cubic_bezier(0.2, nope, 0.8, 1.0)'",
      "'cubic_bezier(-0.1, 0.0, 0.8, 1.0)'",
      "'cubic_bezier(0.2, 0.0, 1.1, 1.0)'",
      "'cubic_bezier(0.2, inf, 0.8, 1.0)'",
      "'cubic_bezier(0.2, -inf, 0.8, 1.0)'",
      "'cubic_bezier(0.2, NaN, 0.8, 1.0)'",
      "'cubic_bezier(0.2, 0.0, 0.8, 1.0) trailing'",
      "'not_an_easing'",
    ] {
      assert!(
        serde_yaml::from_str::<EasingFunction>(invalid).is_err(),
        "invalid easing unexpectedly parsed: {invalid}"
      );
    }

    for valid in [
      "'cubic_bezier(0, 0, 0, 1)'",
      "'cubic_bezier(1, -10, 1, 10)'",
      "'cubic_bezier(0.5, 0.5, 0.5, 0.5)'",
    ] {
      assert!(
        serde_yaml::from_str::<EasingFunction>(valid).is_ok(),
        "valid easing failed to parse: {valid}"
      );
    }
  }

  /// Configs written before the `style` -> `type` key rename must keep
  /// parsing via the legacy aliases (including the older `direction` alias
  /// on `window_open`).
  #[test]
  fn legacy_style_keys_parse() {
    let yaml = r"
animations:
  window_open:
    style: 'zoom'
  window_close:
    style: 'slide_left'
  workspace_switch:
    style: 'fade'
";
    let config: ParsedConfig =
      serde_yaml::from_str(yaml).expect("legacy config should parse");

    assert_eq!(
      config.animations.window_open.style,
      WindowTransitionStyle::Zoom
    );
    assert_eq!(
      config.animations.window_close.style,
      WindowTransitionStyle::SlideLeft
    );
    assert_eq!(
      config.animations.workspace_switch.style,
      WorkspaceSwitchStyle::Fade
    );

    let yaml_direction = r"
animations:
  window_open:
    direction: 'slide_top'
";
    let config: ParsedConfig = serde_yaml::from_str(yaml_direction)
      .expect("direction alias should parse");
    assert_eq!(
      config.animations.window_open.style,
      WindowTransitionStyle::SlideTop
    );
  }

  #[test]
  fn every_animation_style_name_and_alias_parses_distinctly() {
    for (name, expected) in [
      ("slide_right", WindowTransitionStyle::SlideRight),
      ("right", WindowTransitionStyle::SlideRight),
      ("slide_left", WindowTransitionStyle::SlideLeft),
      ("left", WindowTransitionStyle::SlideLeft),
      ("slide_top", WindowTransitionStyle::SlideTop),
      ("top", WindowTransitionStyle::SlideTop),
      ("slide_bottom", WindowTransitionStyle::SlideBottom),
      ("bottom", WindowTransitionStyle::SlideBottom),
      ("none", WindowTransitionStyle::None),
      ("fade", WindowTransitionStyle::None),
      ("zoom", WindowTransitionStyle::Zoom),
    ] {
      let parsed: WindowTransitionStyle = serde_yaml::from_str(name)
        .unwrap_or_else(|_| panic!("window style should parse: {name}"));
      assert_eq!(parsed, expected);
    }
    assert!(
      serde_yaml::from_str::<WindowTransitionStyle>("slide").is_err()
    );

    for (name, expected) in [
      ("slide", WorkspaceSwitchStyle::Slide),
      ("fade", WorkspaceSwitchStyle::Fade),
      ("zoom", WorkspaceSwitchStyle::Zoom),
      ("iris", WorkspaceSwitchStyle::Iris),
    ] {
      let parsed: WorkspaceSwitchStyle = serde_yaml::from_str(name)
        .unwrap_or_else(|_| {
          panic!("workspace style should parse: {name}")
        });
      assert_eq!(parsed, expected);
    }
    assert!(serde_yaml::from_str::<WorkspaceSwitchStyle>("none").is_err());

    for (name, expected) in [
      ("horizontal", WorkspaceSwitchDirection::Horizontal),
      ("vertical", WorkspaceSwitchDirection::Vertical),
    ] {
      assert_eq!(
        serde_yaml::from_str::<WorkspaceSwitchDirection>(name)
          .expect("workspace direction should parse"),
        expected
      );
    }
    assert!(serde_yaml::from_str::<WorkspaceSwitchDirection>("diagonal")
      .is_err());

    for (name, expected) in [
      ("center", WorkspaceSwitchIrisOrigin::Center),
      ("cursor", WorkspaceSwitchIrisOrigin::Cursor),
      ("focused_window", WorkspaceSwitchIrisOrigin::FocusedWindow),
    ] {
      assert_eq!(
        serde_yaml::from_str::<WorkspaceSwitchIrisOrigin>(name)
          .expect("iris origin should parse"),
        expected
      );
    }
    assert!(
      serde_yaml::from_str::<WorkspaceSwitchIrisOrigin>("window").is_err()
    );
  }

  /// Creates `NativeWindowProperties` with the given process name and
  /// title for testing.
  fn test_properties(
    process_name: &str,
    title: &str,
  ) -> NativeWindowProperties {
    NativeWindowProperties {
      title: title.to_string(),
      #[cfg(target_os = "windows")]
      class_name: "TestClass".to_string(),
      process_name: process_name.to_string(),
      frame: Rect::from_ltrb(0, 0, 100, 100),
      is_minimized: false,
      is_maximized: false,
      is_resizable: true,
      #[cfg(target_os = "windows")]
      shadow_borders: wm_platform::RectDelta::zero(),
    }
  }

  /// Creates a `UserConfig` with the given window rules for testing.
  fn test_config(window_rules: Vec<WindowRuleConfig>) -> UserConfig {
    let config_value = ParsedConfig {
      window_rules,
      ..ParsedConfig::default()
    };

    UserConfig {
      path: PathBuf::new(),
      window_rules_by_event: UserConfig::window_rules_by_event(
        &config_value,
      )
      .expect("test window-rule regexes should compile"),
      compiled_tab_title_overrides: Vec::new(),
      value: config_value,
      value_str: String::new(),
    }
  }

  /// Creates a window rule with the given command matching the given
  /// process name.
  fn test_rule(
    command: InvokeCommand,
    process_name: &str,
  ) -> WindowRuleConfig {
    WindowRuleConfig {
      commands: vec![command],
      match_window: vec![WindowMatchConfig {
        window_process: Some(MatchType::Equals {
          equals: process_name.to_string(),
        }),
        ..WindowMatchConfig::default()
      }],
      on: vec![WindowRuleEvent::Manage],
      run_once: true,
    }
  }

  #[test]
  fn force_manage_rule_matches_window() {
    let config = test_config(vec![test_rule(
      InvokeCommand::ForceManage,
      "my-launcher",
    )]);

    assert!(
      config.is_force_managed(&test_properties("my-launcher", "Launcher"))
    );
  }

  #[test]
  fn non_matching_window_is_not_force_managed() {
    let config = test_config(vec![test_rule(
      InvokeCommand::ForceManage,
      "my-launcher",
    )]);

    assert!(
      !config.is_force_managed(&test_properties("other-app", "Other"))
    );
  }

  #[test]
  fn matching_rule_without_force_manage_command_is_skipped() {
    let config =
      test_config(vec![test_rule(InvokeCommand::Ignore, "my-launcher")]);

    assert!(!config
      .is_force_managed(&test_properties("my-launcher", "Launcher")));
  }

  #[test]
  fn no_windows_are_force_managed_by_default() {
    let config = test_config(Vec::new());

    assert!(!config.is_force_managed(&test_properties(
      "Flow.Launcher",
      "Flow.Launcher"
    )));
  }

  fn force_manage_match_config(match_type: MatchType) -> UserConfig {
    test_config(vec![WindowRuleConfig {
      commands: vec![InvokeCommand::ForceManage],
      match_window: vec![WindowMatchConfig {
        window_process: Some(match_type),
        ..WindowMatchConfig::default()
      }],
      on: vec![WindowRuleEvent::Manage],
      run_once: true,
    }])
  }

  #[test]
  fn compiled_window_rule_matchers_preserve_all_match_semantics() {
    for (match_type, matching, non_matching) in [
      (
        MatchType::Equals {
          equals: "terminal".to_string(),
        },
        "terminal",
        "terminal-preview",
      ),
      (
        MatchType::Includes {
          includes: "term".to_string(),
        },
        "terminal",
        "browser",
      ),
      (
        MatchType::Regex {
          regex: "^term(inal)?$".to_string(),
        },
        "terminal",
        "terminal-preview",
      ),
      (
        MatchType::NotEquals {
          not_equals: "browser".to_string(),
        },
        "terminal",
        "browser",
      ),
      (
        MatchType::NotRegex {
          not_regex: "^browser".to_string(),
        },
        "terminal",
        "browser-preview",
      ),
    ] {
      let config = force_manage_match_config(match_type);
      assert!(
        config.is_force_managed(&test_properties(matching, "Window"))
      );
      assert!(
        !config.is_force_managed(&test_properties(non_matching, "Window"))
      );
    }
  }

  #[test]
  fn window_rule_regex_is_stored_compiled_and_reused() {
    let config = force_manage_match_config(MatchType::Regex {
      regex: "^terminal-[0-9]+$".to_string(),
    });
    let rule = &config.window_rules_by_event[&WindowRuleEvent::Manage][0];
    let Some(CompiledMatchType::Regex(regex)) =
      &rule.match_window[0].process
    else {
      panic!("runtime rule must own the compiled regex");
    };
    assert_eq!(regex.as_str(), "^terminal-[0-9]+$");

    for index in 0..10_000 {
      assert!(config.is_force_managed(&test_properties(
        &format!("terminal-{index}"),
        "Window"
      )));
    }
  }

  #[test]
  fn invalid_window_rule_regex_reload_preserves_active_generation() {
    let test_root = std::env::current_dir()
      .expect("test working directory should exist")
      .join("target")
      .join(format!("perf003-reload-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&test_root)
      .expect("source-local reload test directory should be created");
    let config_path = test_root.join("config.yaml");
    let valid_yaml = r"
window_rules:
  - commands: ['force-manage']
    match:
      - window_process:
          regex: '^terminal$'
";
    fs::write(&config_path, valid_yaml)
      .expect("valid regex config should be written");

    let mut config = UserConfig::new(Some(config_path.clone()))
      .expect("valid regex config should load");
    let previous_value_str = config.value_str.clone();
    assert!(
      config.is_force_managed(&test_properties("terminal", "Terminal"))
    );

    let invalid_yaml = valid_yaml.replace("^terminal$", "(");
    fs::write(&config_path, invalid_yaml)
      .expect("invalid regex config should be written");
    let error = config
      .reload()
      .expect_err("invalid window-rule regex must reject reload");

    let diagnostic = format!("{error:#}");
    assert!(diagnostic.contains("window_rules[0].match[0].window_process"));
    assert!(diagnostic.contains("unclosed group"));
    assert_eq!(config.value_str, previous_value_str);
    assert!(
      config.is_force_managed(&test_properties("terminal", "Terminal"))
    );

    fs::remove_dir_all(&test_root)
      .expect("source-local reload test directory should be removed");
  }

  /// Configs written before the `blur_behind` -> `backdrop` key rename
  /// must keep parsing via the `#[serde(alias = "blur_behind")]` on
  /// `WindowEffectConfig::backdrop`.
  #[test]
  fn backdrop_key_alias_parses() {
    let yaml = r"
window_effects:
  focused_window:
    blur_behind:
      enabled: true
      style: 'mica'
";
    let config: ParsedConfig =
      serde_yaml::from_str(yaml).expect("legacy config should parse");

    assert!(config.window_effects.focused_window.backdrop.enabled);
    assert_eq!(
      config.window_effects.focused_window.backdrop.style,
      BackdropStyle::Mica
    );
  }
}
