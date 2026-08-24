#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]

use std::{future::Future, time::Duration};

use anyhow::Context;
use wm_common::ClientResponseData;
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

  let message = args[1..].join(" ");
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

  use super::with_ipc_timeout;

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
}
