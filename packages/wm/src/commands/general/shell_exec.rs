use std::path::Path;

#[cfg(target_os = "windows")]
use anyhow::Context;
#[cfg(target_os = "macos")]
use shell_util::{CommandOptions, Shell};
use uuid::Uuid;
use wm_common::join_windows_command_line;
#[cfg(target_os = "windows")]
use wm_platform::DispatcherExtWindows;

use crate::wm_state::WmState;

pub fn shell_exec(
  command: &[String],
  // LINT: `hide_window` is only used on Windows.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  hide_window: bool,
  state: &WmState,
) -> anyhow::Result<()> {
  let expanded_command = expand_command(command, state)?;
  let (program, arguments) =
    resolve_program_and_arguments(&expanded_command, |path| {
      Path::new(path).is_file()
    })?;
  let request_id = Uuid::new_v4();
  tracing::info!(
    %request_id,
    argument_count = arguments.len(),
    "Queued shell exec request; executable and arguments omitted."
  );

  // NOTE: The standard library's `Command::new` is not used because it
  // launches the program as a subprocess. This prevents cleanup of handles
  // held by our process (e.g. the IPC server port) until the subprocess
  // exits.
  let result = {
    #[cfg(target_os = "macos")]
    {
      Shell::spawn(
        &program,
        arguments.iter().map(String::as_str),
        &CommandOptions::default(),
      )
    }
    #[cfg(target_os = "windows")]
    {
      let home_dir =
        home::home_dir().context("Unable to get home directory.")?;
      let serialized_arguments = join_windows_command_line(&arguments);

      // `ShellExecuteExW` is retained for App Paths registry resolution
      // (`chrome` without it being in PATH), but the platform method only
      // performs a bounded enqueue. Native Shell work and any DDE/App
      // Execution Alias delay run on the dedicated shell worker.
      state.dispatcher.queue_shell_execute_ex(
        &program,
        &serialized_arguments,
        &home_dir,
        hide_window,
      )
    }
  };

  result.map_err(|err| {
    anyhow::anyhow!(
      "Shell exec request {request_id} failed. The command must be valid and \
      the bounded shell-launch worker must be available. Error category: \
      {err}",
    )
  })?;

  Ok(())
}

fn expand_command(
  command: &[String],
  // LINT: `state` is only used on Windows.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  state: &WmState,
) -> anyhow::Result<Vec<String>> {
  #[cfg(target_os = "windows")]
  {
    command
      .iter()
      .map(|argument| state.dispatcher.expand_env_strings(argument))
      .collect::<wm_platform::Result<Vec<_>>>()
      .map_err(anyhow::Error::from)
  }
  #[cfg(target_os = "macos")]
  {
    // TODO: Expand environment variables on macOS.
    Ok(command.to_vec())
  }
}

/// Resolves the executable without changing the remaining argument vector.
/// A split executable path is combined only for backwards compatibility
/// with legacy unquoted config such as `C:\Program Files\App\app.exe
/// --flag`.
fn resolve_program_and_arguments<F>(
  command: &[String],
  mut is_file: F,
) -> anyhow::Result<(String, Vec<String>)>
where
  F: FnMut(&str) -> bool,
{
  let first = command
    .first()
    .filter(|program| !program.is_empty())
    .ok_or_else(|| anyhow::anyhow!("Shell exec command is empty."))?;

  if !first.contains(&['/', '\\'][..]) || is_file(first) {
    return Ok((first.clone(), command[1..].to_vec()));
  }

  let mut cumulative_path = first.clone();
  for part_index in 1..command.len() {
    cumulative_path.push(' ');
    cumulative_path.push_str(&command[part_index]);
    if is_file(&cumulative_path) {
      return Ok((cumulative_path, command[part_index + 1..].to_vec()));
    }
  }

  anyhow::bail!("Shell exec program path is not valid.")
}

#[cfg(test)]
mod tests {
  use super::*;

  fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
  }

  #[test]
  fn preserves_exact_arguments_and_shell_metacharacters() {
    let command = strings(&[
      "pwsh.exe",
      "-Command",
      "Write-Output \"hello world\"",
      "",
      "&",
      "|",
      "C:\\trailing\\",
    ]);
    let (program, arguments) =
      resolve_program_and_arguments(&command, |_| false).expect(
        "App Paths executable should resolve without path probing",
      );

    assert_eq!(program, "pwsh.exe");
    assert_eq!(arguments, command[1..]);
    let serialized = join_windows_command_line(&arguments);
    assert_eq!(
      wm_common::parse_windows_command_line(&serialized)
        .expect("ShellExecute arguments should round trip"),
      arguments
    );
  }

  #[test]
  fn resolves_quoted_and_legacy_unquoted_executable_paths() {
    let quoted =
      strings(&[r"C:\Program Files\App\app.exe", "two words", ""]);
    let resolved = resolve_program_and_arguments(&quoted, |path| {
      path == r"C:\Program Files\App\app.exe"
    })
    .expect("single quoted path token should resolve");
    assert_eq!(resolved, (quoted[0].clone(), quoted[1..].to_vec()));

    let legacy =
      strings(&[r"C:\Program", r"Files\App\app.exe", "two words", ""]);
    let resolved = resolve_program_and_arguments(&legacy, |path| {
      path == r"C:\Program Files\App\app.exe"
    })
    .expect("legacy split executable path should resolve");
    assert_eq!(
      resolved,
      (
        r"C:\Program Files\App\app.exe".to_string(),
        legacy[2..].to_vec(),
      )
    );
  }

  #[test]
  fn invalid_or_empty_program_fails_before_launch_planning_completes() {
    let mut probes = 0;
    let result = resolve_program_and_arguments(
      &strings(&[r"C:\missing\app.exe", "argument"]),
      |_| {
        probes += 1;
        false
      },
    );
    assert!(result.is_err());
    assert_eq!(probes, 2);

    assert!(resolve_program_and_arguments(&[], |_| true).is_err());
    assert!(
      resolve_program_and_arguments(&[String::new()], |_| true).is_err()
    );
  }
}
