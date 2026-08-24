#![allow(clippy::missing_errors_doc)]

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
  connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream,
};
use uuid::Uuid;
use wm_common::{
  ClientResponseMessage, EventSubscriptionMessage, ServerMessage,
  DEFAULT_IPC_PORT,
};

pub struct IpcClient {
  stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl IpcClient {
  pub async fn connect() -> anyhow::Result<Self> {
    let server_addr = format!("ws://127.0.0.1:{DEFAULT_IPC_PORT}");

    Self::connect_to(server_addr).await
  }

  async fn connect_to(server_addr: String) -> anyhow::Result<Self> {
    let (stream, _) = connect_async(server_addr)
      .await
      .context("Failed to connect to IPC server.")?;

    Ok(Self { stream })
  }

  /// Sends a message to the IPC server.
  pub async fn send(&mut self, message: &str) -> anyhow::Result<()> {
    self
      .stream
      .send(Message::Text(message.into()))
      .await
      .context("Failed to send command.")?;

    Ok(())
  }

  /// Sends and flushes a WebSocket close frame.
  ///
  /// Closing is best-effort because a successful command such as `wm-exit`
  /// can shut down the server before the client finishes its handshake. A
  /// late close failure must not turn an already-successful command into
  /// an error.
  pub async fn close(&mut self) {
    let _ = SinkExt::close(&mut self.stream).await;
  }

  /// Waits and returns the next reply from the IPC server.
  pub async fn next_message(&mut self) -> anyhow::Result<ServerMessage> {
    let response = self
      .stream
      .next()
      .await
      .context("Failed to receive response.")?
      .context("Invalid response message.")?;

    let json_response =
      serde_json::from_str::<ServerMessage>(response.to_text()?)?;

    Ok(json_response)
  }

  pub async fn client_response(
    &mut self,
    client_message: &str,
  ) -> Option<ClientResponseMessage> {
    while let Ok(response) = self.next_message().await {
      if let ServerMessage::ClientResponse(client_response) = response {
        if client_response.client_message == client_message {
          return Some(client_response);
        }
      }
    }

    None
  }

  pub async fn event_subscription(
    &mut self,
    subscription_id: &Uuid,
  ) -> Option<EventSubscriptionMessage> {
    while let Ok(response) = self.next_message().await {
      if let ServerMessage::EventSubscription(event_sub) = response {
        if &event_sub.subscription_id == subscription_id {
          return Some(event_sub);
        }
      }
    }

    None
  }
}

#[cfg(test)]
mod tests {
  use futures_util::StreamExt;
  use tokio::net::TcpListener;
  use tokio_tungstenite::accept_async;

  use super::IpcClient;

  #[tokio::test]
  async fn graceful_close_sends_a_websocket_close_frame() {
    let listener = TcpListener::bind("127.0.0.1:0")
      .await
      .expect("test listener should bind");
    let address = listener
      .local_addr()
      .expect("test listener should have an address");

    let server = tokio::spawn(async move {
      let (stream, _) = listener
        .accept()
        .await
        .expect("test server should accept the client");
      let mut websocket = accept_async(stream)
        .await
        .expect("test WebSocket handshake should succeed");

      websocket
        .next()
        .await
        .expect("client should send a frame")
        .expect("close frame should be valid")
        .is_close()
    });

    let mut client = IpcClient::connect_to(format!("ws://{address}"))
      .await
      .expect("test client should connect");
    client.close().await;

    assert!(
      server.await.expect("test server task should finish"),
      "client must close with a WebSocket close frame"
    );
  }
}
