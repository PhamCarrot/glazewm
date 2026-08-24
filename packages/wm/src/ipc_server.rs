#[cfg(target_os = "windows")]
use std::{ffi::c_void, os::windows::io::AsRawSocket};
use std::{iter, net::SocketAddr};

use anyhow::{bail, Context};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::{
  net::{TcpListener, TcpStream},
  sync::{broadcast, mpsc},
  task,
};
use tokio_tungstenite::{
  accept_hdr_async_with_config,
  tungstenite::{
    handshake::server::{ErrorResponse, Request, Response},
    http::StatusCode,
    protocol::WebSocketConfig,
    Message,
  },
};
use tracing::{info, warn};
use uuid::Uuid;
use wm_common::{
  AppCommand, AppMetadataData, BindingModesData, ClientResponseData,
  ClientResponseMessage, CommandData, EventSubscribeData,
  EventSubscriptionMessage, FocusedData, LayoutData, MonitorsData,
  QueryCommand, ServerMessage, SubscribableEvent, WindowsData, WmEvent,
  WorkspacesData, DEFAULT_IPC_PORT,
};

use crate::{
  traits::{CommonGetters, TilingDirectionGetters},
  user_config::UserConfig,
  wm::WindowManager,
};

#[cfg(target_os = "windows")]
const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
unsafe extern "system" {
  #[cfg(test)]
  fn GetHandleInformation(handle: *mut c_void, flags: *mut u32) -> i32;
  fn SetHandleInformation(
    handle: *mut c_void,
    mask: u32,
    flags: u32,
  ) -> i32;
}

pub struct IpcServer {
  abort_handle: task::AbortHandle,
  pub message_rx:
    mpsc::Receiver<(String, mpsc::Sender<Message>, broadcast::Sender<()>)>,
  _event_rx: broadcast::Receiver<(SubscribableEvent, WmEvent)>,
  event_tx: broadcast::Sender<(SubscribableEvent, WmEvent)>,
  _unsubscribe_rx: broadcast::Receiver<Uuid>,
  unsubscribe_tx: broadcast::Sender<Uuid>,
}

impl IpcServer {
  const MESSAGE_QUEUE_CAPACITY: usize = 256;
  const RESPONSE_QUEUE_CAPACITY: usize = 64;
  const MAX_MESSAGE_SIZE: usize = 64 * 1024;

  pub async fn start() -> anyhow::Result<Self> {
    let (message_tx, message_rx) =
      mpsc::channel(Self::MESSAGE_QUEUE_CAPACITY);
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (unsubscribe_tx, _unsubscribe_rx) = broadcast::channel(16);

    let server_addr = format!("127.0.0.1:{DEFAULT_IPC_PORT}");
    let server = TcpListener::bind(server_addr.clone()).await?;
    Self::prevent_listener_inheritance(&server)?;
    info!("IPC server started on: '{}'.", server_addr);

    let task = task::spawn(async move {
      while let Ok((stream, addr)) = server.accept().await {
        let message_tx = message_tx.clone();

        task::spawn(async move {
          if let Err(err) =
            Self::handle_connection(stream, addr, message_tx).await
          {
            warn!("Error handling connection: {}", err);
          }
        });
      }
    });

    Ok(Self {
      abort_handle: task.abort_handle(),
      #[allow(clippy::used_underscore_binding)]
      _event_rx,
      event_tx,
      message_rx,
      unsubscribe_tx,
      #[allow(clippy::used_underscore_binding)]
      _unsubscribe_rx,
    })
  }

  #[cfg(target_os = "windows")]
  fn prevent_listener_inheritance(
    server: &TcpListener,
  ) -> anyhow::Result<()> {
    let handle = usize::try_from(server.as_raw_socket())
      .context("IPC listener handle does not fit in a pointer.")?
      as *mut c_void;
    let result =
      unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };

    if result == 0 {
      return Err(std::io::Error::last_os_error())
        .context("Failed to make the IPC listener non-inheritable.");
    }

    Ok(())
  }

  #[cfg(not(target_os = "windows"))]
  fn prevent_listener_inheritance(
    _server: &TcpListener,
  ) -> anyhow::Result<()> {
    Ok(())
  }

  async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    message_tx: mpsc::Sender<(
      String,
      mpsc::Sender<Message>,
      broadcast::Sender<()>,
    )>,
  ) -> anyhow::Result<()> {
    info!("Incoming IPC connection from: {}.", addr);

    let websocket_config = WebSocketConfig::default()
      .max_message_size(Some(Self::MAX_MESSAGE_SIZE))
      .max_frame_size(Some(Self::MAX_MESSAGE_SIZE));
    let ws_stream = accept_hdr_async_with_config(
      stream,
      Self::validate_handshake,
      Some(websocket_config),
    )
    .await
    .context("Error during websocket handshake.")?;

    let (mut outgoing, mut incoming) = ws_stream.split();
    let (response_tx, mut response_rx) =
      mpsc::channel(Self::RESPONSE_QUEUE_CAPACITY);
    let (disconnection_tx, _) = broadcast::channel(16);

    let res = async {
      loop {
        tokio::select! {
          Some(response) = response_rx.recv() => {
            outgoing.send(response).await?;
          }
          message = incoming.next() => {
            match message {
              Some(Ok(message)) => {
                if message.is_text() || message.is_binary() {
                  message_tx.send((
                    message.to_text()?.to_string(),
                    response_tx.clone(),
                    disconnection_tx.clone(),
                  )).await?;
                } else if message.is_close() {
                  // Complete the WebSocket close handshake before dropping
                  // the TCP stream. Short-lived CLI clients use this path
                  // after receiving their response.
                  outgoing.close().await?;
                  break Ok(());
                }
              }
              Some(Err(err)) => bail!("WebSocket error: {}", err),
              None => {
                // WebSocket connection closed.
                break Ok(());
              },
            }
          }
        }
      }
    }
    .await;

    info!("IPC disconnection from: {}.", addr);

    // One-shot query/command clients have no disconnection subscribers.
    // Sending to zero receivers is therefore an expected no-op.
    let _ = disconnection_tx.send(());

    res
  }

  pub fn process_message(
    &self,
    message: String,
    response_tx: &mpsc::Sender<Message>,
    disconnection_tx: &broadcast::Sender<()>,
    wm: &mut WindowManager,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    if response_tx.capacity() == 0 {
      bail!("IPC response queue is full; command was not processed.");
    }

    let app_command = AppCommand::try_parse_from(
      iter::once("").chain(message.split_whitespace()),
    );

    let response_data =
      app_command
        .map_err(anyhow::Error::msg)
        .and_then(|app_command| {
          self.handle_app_command(
            app_command,
            response_tx,
            disconnection_tx,
            wm,
            config,
          )
        });

    // Respond to the client with the result of the command.
    response_tx
      .try_send(Self::to_client_response_msg(message, response_data)?)
      .map_err(|err| {
        anyhow::anyhow!("Failed to send response: {}", err)
      })?;

    Ok(())
  }

  #[allow(clippy::too_many_lines)]
  fn handle_app_command(
    &self,
    app_command: AppCommand,
    response_tx: &mpsc::Sender<Message>,
    disconnection_tx: &broadcast::Sender<()>,
    wm: &mut WindowManager,
    config: &mut UserConfig,
  ) -> anyhow::Result<ClientResponseData> {
    let response_data = match app_command {
      AppCommand::Query { command } => match command {
        QueryCommand::Windows => {
          ClientResponseData::Windows(WindowsData {
            windows: wm
              .state
              .windows()
              .into_iter()
              .map(|window| window.to_dto())
              .try_collect()?,
          })
        }
        QueryCommand::Workspaces => {
          ClientResponseData::Workspaces(WorkspacesData {
            workspaces: wm
              .state
              .workspaces()
              .into_iter()
              .map(|workspace| workspace.to_dto())
              .try_collect()?,
          })
        }
        QueryCommand::Monitors => {
          ClientResponseData::Monitors(MonitorsData {
            monitors: wm
              .state
              .monitors()
              .into_iter()
              .map(|monitor| monitor.to_dto())
              .try_collect()?,
          })
        }
        QueryCommand::BindingModes => {
          ClientResponseData::BindingModes(BindingModesData {
            binding_modes: wm.state.binding_modes.clone(),
          })
        }
        QueryCommand::Focused => {
          let focused_container = wm
            .state
            .focused_container()
            .context("No focused container.")?;

          ClientResponseData::Focused(FocusedData {
            focused: focused_container.to_dto()?,
          })
        }
        QueryCommand::AppMetadata => {
          ClientResponseData::AppMetadata(AppMetadataData {
            version: env!("VERSION_NUMBER").to_string(),
          })
        }
        QueryCommand::Layout => {
          let direction_container = wm
            .state
            .focused_container()
            .and_then(|focused| focused.direction_container())
            .context("No direction container.")?;

          ClientResponseData::Layout(LayoutData {
            direction_container: direction_container.to_dto()?,
            layout: direction_container.layout(),
          })
        }
        QueryCommand::Paused => {
          ClientResponseData::Paused(wm.state.is_paused)
        }
      },
      AppCommand::Command {
        subject_container_id,
        command,
      } => {
        let subject_container_id = wm.process_commands(
          &vec![command],
          subject_container_id,
          config,
        )?;

        ClientResponseData::Command(CommandData {
          subject_container_id,
        })
      }
      AppCommand::Sub { events } => {
        let subscription_id = Uuid::new_v4();
        info!("New event subscription {}: {:?}", subscription_id, events);

        let response_tx = response_tx.clone();
        let mut event_rx = self.event_tx.subscribe();
        let mut unsubscribe_rx = self.unsubscribe_tx.subscribe();
        let mut disconnection_rx = disconnection_tx.subscribe();

        task::spawn(async move {
          loop {
            tokio::select! {
              Ok(()) = disconnection_rx.recv() => {
                break;
              }
              Ok(id) = unsubscribe_rx.recv() => {
                if id == subscription_id {
                  break;
                }
              }
              Ok((event_type, event)) = event_rx.recv() => {
                // Check whether the event is one of the subscribed events.
                if events.contains(&event_type)
                  || events.contains(&SubscribableEvent::All)
                {
                  let send_result = Self::to_event_subscription_msg(
                    subscription_id,
                    event,
                  )
                  .and_then(|event_msg| {
                    response_tx
                      .try_send(event_msg)
                      .map_err(anyhow::Error::from)
                  });

                  if let Err(err) = send_result {
                    warn!("Error emitting WM event: {}", err);
                    break;
                  }
                }
              }
            }
          }
        });

        ClientResponseData::EventSubscribe(EventSubscribeData {
          subscription_id,
        })
      }
      AppCommand::Unsub { subscription_id } => {
        self
          .unsubscribe_tx
          .send(subscription_id)
          .context("Failed to unsubscribe from event.")?;

        ClientResponseData::EventUnsubscribe
      }
      AppCommand::Start { .. } => bail!("Unsupported IPC command."),
    };

    Ok(response_data)
  }

  #[allow(clippy::result_large_err)]
  fn validate_handshake(
    request: &Request,
    response: Response,
  ) -> Result<Response, ErrorResponse> {
    if request.headers().contains_key("origin") {
      let mut rejection = ErrorResponse::new(Some(
        "Browser-origin IPC connections are not allowed.".to_string(),
      ));
      *rejection.status_mut() = StatusCode::FORBIDDEN;
      return Err(rejection);
    }

    Ok(response)
  }

  fn to_client_response_msg(
    client_message: String,
    response_data: anyhow::Result<ClientResponseData>,
  ) -> anyhow::Result<Message> {
    let error = response_data.as_ref().err().map(ToString::to_string);
    let success = response_data.as_ref().is_ok();

    let message = ServerMessage::ClientResponse(ClientResponseMessage {
      client_message,
      data: response_data.ok(),
      error,
      success,
    });

    let message_json = serde_json::to_string(&message)?;
    Ok(Message::Text(message_json.into()))
  }

  fn to_event_subscription_msg(
    subscription_id: Uuid,
    event: WmEvent,
  ) -> anyhow::Result<Message> {
    let message =
      ServerMessage::EventSubscription(EventSubscriptionMessage {
        data: Some(event),
        error: None,
        subscription_id,
        success: true,
      });

    let message_json = serde_json::to_string(&message)?;
    Ok(Message::Text(message_json.into()))
  }

  pub fn process_event(&mut self, event: WmEvent) -> anyhow::Result<()> {
    let event_type = match event {
      WmEvent::ApplicationExiting => SubscribableEvent::ApplicationExiting,
      WmEvent::BindingModesChanged { .. } => {
        SubscribableEvent::BindingModesChanged
      }
      WmEvent::FocusChanged { .. } => SubscribableEvent::FocusChanged,
      WmEvent::FocusedContainerMoved { .. } => {
        SubscribableEvent::FocusedContainerMoved
      }
      WmEvent::MonitorAdded { .. } => SubscribableEvent::MonitorAdded,
      WmEvent::MonitorUpdated { .. } => SubscribableEvent::MonitorUpdated,
      WmEvent::MonitorRemoved { .. } => SubscribableEvent::MonitorRemoved,
      WmEvent::LayoutChanged { .. } => SubscribableEvent::LayoutChanged,
      WmEvent::UserConfigChanged { .. } => {
        SubscribableEvent::UserConfigChanged
      }
      WmEvent::WindowManaged { .. } => SubscribableEvent::WindowManaged,
      WmEvent::WindowUnmanaged { .. } => {
        SubscribableEvent::WindowUnmanaged
      }
      WmEvent::WorkspaceActivated { .. } => {
        SubscribableEvent::WorkspaceActivated
      }
      WmEvent::WorkspaceDeactivated { .. } => {
        SubscribableEvent::WorkspaceDeactivated
      }
      WmEvent::WorkspaceUpdated { .. } => {
        SubscribableEvent::WorkspaceUpdated
      }
      WmEvent::PauseChanged { .. } => SubscribableEvent::PauseChanged,
    };

    self
      .event_tx
      .send((event_type, event))
      .map_err(|err| anyhow::anyhow!("Failed to send event: {}", err))?;

    Ok(())
  }

  pub fn stop(&self) {
    info!("Shutting down IPC server.");
    self.abort_handle.abort();
  }
}

impl Drop for IpcServer {
  fn drop(&mut self) {
    self.stop();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn native_ipc_handshake_without_origin_is_allowed() {
    let request = Request::new(());
    let response = Response::new(());

    assert!(IpcServer::validate_handshake(&request, response).is_ok());
  }

  #[test]
  fn browser_origin_ipc_handshake_is_rejected() {
    let request = Request::builder()
      .header("origin", "https://example.test")
      .body(())
      .expect("test request should be valid");
    let response = Response::new(());

    let rejection = IpcServer::validate_handshake(&request, response)
      .expect_err("browser-origin handshake must be rejected");
    assert_eq!(rejection.status(), StatusCode::FORBIDDEN);
  }

  #[cfg(target_os = "windows")]
  #[tokio::test]
  async fn ipc_listener_handle_is_explicitly_non_inheritable() {
    let listener = TcpListener::bind("127.0.0.1:0")
      .await
      .expect("test listener should bind");
    let handle = usize::try_from(listener.as_raw_socket())
      .expect("test listener handle should fit in a pointer")
      as *mut c_void;

    let set_result = unsafe {
      SetHandleInformation(
        handle,
        HANDLE_FLAG_INHERIT,
        HANDLE_FLAG_INHERIT,
      )
    };
    assert_ne!(set_result, 0, "test should set inheritance first");

    IpcServer::prevent_listener_inheritance(&listener)
      .expect("listener inheritance should be cleared");

    let mut flags = 0u32;
    let get_result =
      unsafe { GetHandleInformation(handle, &raw mut flags) };
    assert_ne!(get_result, 0, "test should read listener flags");
    assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
  }
}
