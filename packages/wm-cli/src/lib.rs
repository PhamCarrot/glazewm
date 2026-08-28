#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]

use std::{future::Future, time::Duration};

use anyhow::Context;
use wm_common::{join_windows_command_line, ClientResponseData};
use wm_ipc_client::IpcClient;

const IPC_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);

async fn with_ipc_timeout<F>(
  operation: &'static str,
  timeout: Duration,
  future: F,
) -> anyhow::Result<F::Output>
where
  F: Future,
{
  tokio::time::timeout(timeout, future)
    .await
    .with_context(|| format!("Timed out {operation}."))
}

pub async fn start(args: Vec<String>) -> anyhow::Result<()> {
  let mut client = with_ipc_timeout(
    "connecting to the IPC server",
    IPC_OPERATION_TIMEOUT,
    IpcClient::connect(),
  )
  .await??;

  let message = join_windows_command_line(&args[1..]);
  with_ipc_timeout(
    "sending a command to the IPC server",
    IPC_OPERATION_TIMEOUT,
    client.send(&message),
  )
  .await?
  .context("Failed to send command to IPC server.")?;

  let client_response = with_ipc_timeout(
    "waiting for an IPC response",
    IPC_OPERATION_TIMEOUT,
    client.client_response(&message),
  )
  .await?
  .context("Failed to receive response from IPC server.")?;

  match client_response.data {
    // For event subscriptions, omit the initial response message and
    // continuously output subsequent event messages.
    Some(ClientResponseData::EventSubscribe(data)) => loop {
      let event_subscription = client
        .event_subscription(&data.subscription_id)
        .await
        .context("Failed to receive response from IPC server.")?;

      println!("{}", serde_json::to_string(&event_subscription)?);
    },
    // For all other messages, output and exit when the first response
    // message is received.
    _ => {
      println!("{}", serde_json::to_string(&client_response)?);
      client.close().await;
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use std::{future, time::Duration};

  use super::{join_windows_command_line, with_ipc_timeout};

  #[tokio::test]
  async fn ipc_operation_timeout_is_bounded() {
    let result = with_ipc_timeout(
      "waiting for a test response",
      Duration::from_millis(10),
      future::pending::<()>(),
    )
    .await;

    let error = result.expect_err("pending IPC work should time out");
    assert!(error
      .to_string()
      .contains("Timed out waiting for a test response."));
  }

  #[test]
  fn ipc_message_preserves_os_argument_boundaries() {
    let arguments = vec![
      "command".to_string(),
      "shell-exec".to_string(),
      "pwsh.exe".to_string(),
      "two words".to_string(),
      String::new(),
      r#"literal"quote"#.to_string(),
      "C:\\path with space\\".to_string(),
    ];

    let message = join_windows_command_line(&arguments);
    assert_eq!(
      wm_common::parse_windows_command_line(&message)
        .expect("CLI message should round trip"),
      arguments
    );
  }
}
