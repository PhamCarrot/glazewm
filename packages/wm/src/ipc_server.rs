#[cfg(target_os = "windows")]
use std::{ffi::c_void, os::windows::io::AsRawSocket};
use std::{
  net::SocketAddr,
  sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, OnceLock,
  },
  time::Duration,
};

use anyhow::{bail, Context};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::{
  net::{TcpListener, TcpStream},
  sync::{broadcast, mpsc, OwnedSemaphorePermit, Semaphore},
  task,
  time::{timeout, Instant},
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
  parse_windows_command_line, AppCommand, AppMetadataData,
  BindingModesData, ClientResponseData, ClientResponseMessage,
  CommandData, EventSubscribeData, EventSubscriptionMessage, FocusedData,
  LayoutData, MonitorsData, QueryCommand, ServerMessage,
  SubscribableEvent, TilingDirectionData, WindowsData, WmEvent,
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
  pub message_rx: mpsc::Receiver<IncomingIpcMessage>,
  _event_rx: broadcast::Receiver<(SubscribableEvent, WmEvent)>,
  event_tx: broadcast::Sender<(SubscribableEvent, WmEvent)>,
  _unsubscribe_rx: broadcast::Receiver<Uuid>,
  unsubscribe_tx: broadcast::Sender<Uuid>,
}

pub(crate) struct IncomingIpcMessage {
  pub correlation_id: u64,
  pub message: String,
  pub response_tx: mpsc::Sender<Message>,
  pub disconnection_tx: broadcast::Sender<()>,
  pub subscription_slots: Arc<Semaphore>,
}

impl IpcServer {
  const DIAGNOSTIC_LOG_INTERVAL: usize = 64;
  const MESSAGE_QUEUE_CAPACITY: usize = 256;
  const RESPONSE_QUEUE_CAPACITY: usize = 64;
  const MAX_MESSAGE_SIZE: usize = 64 * 1024;
  const MAX_CONNECTIONS: usize = 64;
  const MAX_CONCURRENT_PEER_AUTHENTICATIONS: usize = 2;
  const MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 16;
  const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
  const PEER_AUTH_TIMEOUT: Duration = Duration::from_secs(1);
  const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
  const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
  const COMMAND_QUEUE_TIMEOUT: Duration = Duration::from_secs(1);

  fn next_correlation_id() -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
  }

  pub async fn start() -> anyhow::Result<Self> {
    let (message_tx, message_rx) =
      mpsc::channel(Self::MESSAGE_QUEUE_CAPACITY);
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (unsubscribe_tx, _unsubscribe_rx) = broadcast::channel(16);

    let server_addr = format!("127.0.0.1:{DEFAULT_IPC_PORT}");
    let server = TcpListener::bind(server_addr.clone()).await?;
    Self::prevent_listener_inheritance(&server)?;
    info!("IPC server started on: '{}'.", server_addr);

    let connection_slots = Arc::new(Semaphore::new(Self::MAX_CONNECTIONS));
    let rejected_connections = Arc::new(AtomicUsize::new(0));
    let connection_failures = Arc::new(AtomicUsize::new(0));
    let task = task::spawn(async move {
      while let Ok((stream, addr)) = server.accept().await {
        let Some(connection_permit) =
          Self::try_acquire_connection_slot(&connection_slots)
        else {
          let rejected =
            rejected_connections.fetch_add(1, Ordering::Relaxed);
          if rejected == 0 || (rejected + 1).is_multiple_of(64) {
            warn!(
              "IPC connection limit reached; rejecting excess local clients ({} rejected in this saturation epoch).",
              rejected + 1
            );
          }
          drop(stream);
          continue;
        };
        let message_tx = message_tx.clone();
        let connection_failures = Arc::clone(&connection_failures);

        task::spawn(async move {
          let _connection_permit = connection_permit;
          if let Err(err) =
            Self::handle_connection(stream, addr, message_tx).await
          {
            if let Some(occurrence) =
              Self::rate_limited_occurrence(&connection_failures)
            {
              warn!(
                failure_count = occurrence,
                category = Self::connection_error_category(&err),
                "IPC connection failed; repeated failures are rate-limited."
              );
            }
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

  fn try_acquire_connection_slot(
    slots: &Arc<Semaphore>,
  ) -> Option<OwnedSemaphorePermit> {
    Arc::clone(slots).try_acquire_owned().ok()
  }

  fn rate_limited_occurrence(counter: &AtomicUsize) -> Option<usize> {
    let previous = counter
      .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        value.checked_add(1)
      })
      .ok()?;
    let occurrence = previous + 1;
    (occurrence == 1
      || occurrence.is_multiple_of(Self::DIAGNOSTIC_LOG_INTERVAL))
    .then_some(occurrence)
  }

  fn connection_error_category(error: &anyhow::Error) -> &'static str {
    if let Some(error) = error.downcast_ref::<std::io::Error>() {
      return match error.kind() {
        std::io::ErrorKind::TimedOut => "io-timeout",
        std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::UnexpectedEof => "io-disconnect",
        std::io::ErrorKind::PermissionDenied => "io-permission",
        _ => "io",
      };
    }
    if error
      .downcast_ref::<tokio::time::error::Elapsed>()
      .is_some()
    {
      return "timeout";
    }
    if error
      .downcast_ref::<tokio_tungstenite::tungstenite::Error>()
      .is_some()
    {
      return "websocket";
    }
    "ipc"
  }

  fn try_acquire_subscription_slot(
    slots: &Arc<Semaphore>,
  ) -> anyhow::Result<OwnedSemaphorePermit> {
    Arc::clone(slots)
      .try_acquire_owned()
      .context("Maximum subscriptions reached for this IPC connection.")
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
    message_tx: mpsc::Sender<IncomingIpcMessage>,
  ) -> anyhow::Result<()> {
    Self::handle_connection_with_timeouts(
      stream,
      addr,
      message_tx,
      Self::HANDSHAKE_TIMEOUT,
      Self::CONNECTION_IDLE_TIMEOUT,
      Self::SOCKET_WRITE_TIMEOUT,
    )
    .await
  }

  async fn handle_connection_with_timeouts(
    stream: TcpStream,
    addr: SocketAddr,
    message_tx: mpsc::Sender<IncomingIpcMessage>,
    handshake_timeout: Duration,
    idle_timeout: Duration,
    write_timeout: Duration,
  ) -> anyhow::Result<()> {
    info!("Incoming IPC connection from: {}.", addr);

    Self::authorize_peer(&stream, addr).await?;

    let websocket_config = WebSocketConfig::default()
      .max_message_size(Some(Self::MAX_MESSAGE_SIZE))
      .max_frame_size(Some(Self::MAX_MESSAGE_SIZE));
    let ws_stream = timeout(
      handshake_timeout,
      accept_hdr_async_with_config(
        stream,
        Self::validate_handshake,
        Some(websocket_config),
      ),
    )
    .await
    .context("Timed out during websocket handshake.")?
    .context("Error during websocket handshake.")?;

    let (mut outgoing, mut incoming) = ws_stream.split();
    let (response_tx, mut response_rx) =
      mpsc::channel(Self::RESPONSE_QUEUE_CAPACITY);
    let (disconnection_tx, _) = broadcast::channel(16);
    let subscription_slots =
      Arc::new(Semaphore::new(Self::MAX_SUBSCRIPTIONS_PER_CONNECTION));
    let idle_deadline = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle_deadline);

    let res = async {
      loop {
        tokio::select! {
          () = &mut idle_deadline => {
            bail!("IPC connection exceeded its activity idle deadline.");
          }
          Some(response) = response_rx.recv() => {
            timeout(write_timeout, outgoing.send(response))
              .await
              .context("Timed out writing IPC response.")??;
            idle_deadline.as_mut().reset(Instant::now() + idle_timeout);
          }
          message = incoming.next() => {
            match message {
              Some(Ok(message)) => {
                idle_deadline.as_mut().reset(Instant::now() + idle_timeout);
                if message.is_text() || message.is_binary() {
                  timeout(Self::COMMAND_QUEUE_TIMEOUT, message_tx.send(
                    IncomingIpcMessage {
                      correlation_id: Self::next_correlation_id(),
                      message: message.to_text()?.to_string(),
                      response_tx: response_tx.clone(),
                      disconnection_tx: disconnection_tx.clone(),
                      subscription_slots: Arc::clone(&subscription_slots),
                    }))
                  .await
                  .context("Timed out queueing IPC command.")??;
                } else if message.is_close() {
                  // Complete the WebSocket close handshake before dropping
                  // the TCP stream. Short-lived CLI clients use this path
                  // after receiving their response.
                  timeout(write_timeout, outgoing.close())
                    .await
                    .context("Timed out closing IPC websocket.")??;
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

  async fn authorize_peer(
    stream: &TcpStream,
    peer_addr: SocketAddr,
  ) -> anyhow::Result<()> {
    static AUTH_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

    let local_addr = stream
      .local_addr()
      .context("Failed to read the IPC server endpoint.")?;
    let auth_slots = AUTH_SLOTS.get_or_init(|| {
      Arc::new(Semaphore::new(Self::MAX_CONCURRENT_PEER_AUTHENTICATIONS))
    });
    let auth_permit = timeout(
      Self::PEER_AUTH_TIMEOUT,
      Arc::clone(auth_slots).acquire_owned(),
    )
    .await
    .context("Timed out waiting for IPC peer authentication capacity.")?
    .context("IPC peer authentication capacity was closed.")?;
    let owned_by_current_user = timeout(
      Self::PEER_AUTH_TIMEOUT,
      task::spawn_blocking(move || {
        let _auth_permit = auth_permit;
        wm_platform::is_tcp_peer_owned_by_current_user(
          local_addr, peer_addr,
        )
      }),
    )
    .await
    .context("Timed out authenticating IPC peer ownership.")?
    .context("IPC peer authentication worker failed.")?
    .context("Failed to authenticate IPC peer ownership.")?;

    if !owned_by_current_user {
      bail!("IPC peer is not owned by the current Windows user.");
    }
    Ok(())
  }

  pub fn process_message(
    &self,
    incoming: IncomingIpcMessage,
    wm: &mut WindowManager,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    let IncomingIpcMessage {
      correlation_id,
      message,
      response_tx,
      disconnection_tx,
      subscription_slots,
    } = incoming;
    if response_tx.capacity() == 0 {
      bail!("IPC response queue is full; command was not processed.");
    }

    let app_command = Self::parse_app_command(&message);
    if let Ok(command) = &app_command {
      static REQUEST_DIAGNOSTICS: AtomicUsize = AtomicUsize::new(0);
      if let Some(request_count) =
        Self::rate_limited_occurrence(&REQUEST_DIAGNOSTICS)
      {
        let (request_kind, command_name) = command.diagnostic_metadata();
        info!(
          correlation_id,
          request_count,
          request_kind,
          command_name,
          "Received IPC request; repeated successful requests are sampled."
        );
      }
    } else {
      static REJECTION_DIAGNOSTICS: AtomicUsize = AtomicUsize::new(0);
      if let Some(rejection_count) =
        Self::rate_limited_occurrence(&REJECTION_DIAGNOSTICS)
      {
        warn!(
          correlation_id,
          rejection_count,
          "Rejected malformed IPC request; payload omitted and repeated \
           rejections are sampled."
        );
      }
    }

    let response_data =
      app_command
        .map_err(anyhow::Error::msg)
        .and_then(|app_command| {
          self.handle_app_command(
            app_command,
            &response_tx,
            &disconnection_tx,
            &subscription_slots,
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

  fn parse_app_command(message: &str) -> Result<AppCommand, String> {
    let mut arguments = parse_windows_command_line(message)
      .map_err(|error| error.to_string())?;
    arguments.insert(0, String::new());
    AppCommand::try_parse_from(arguments)
      .map_err(|error| error.to_string())
  }

  #[allow(clippy::too_many_lines)]
  fn handle_app_command(
    &self,
    app_command: AppCommand,
    response_tx: &mpsc::Sender<Message>,
    disconnection_tx: &broadcast::Sender<()>,
    subscription_slots: &Arc<Semaphore>,
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
            version: wm_common::VERSION.to_string(),
          })
        }
        QueryCommand::TilingDirection => {
          let direction_container = wm
            .state
            .focused_container()
            .and_then(|focused| focused.direction_container())
            .context("No direction container.")?;

          ClientResponseData::TilingDirection(TilingDirectionData {
            direction_container: direction_container.to_dto()?,
            tiling_direction: direction_container.tiling_direction(),
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
        let subscription_permit =
          Self::try_acquire_subscription_slot(subscription_slots)?;
        let subscription_id = Uuid::new_v4();
        info!("New event subscription {}: {:?}", subscription_id, events);

        let response_tx = response_tx.clone();
        let mut event_rx = self.event_tx.subscribe();
        let mut unsubscribe_rx = self.unsubscribe_tx.subscribe();
        let mut disconnection_rx = disconnection_tx.subscribe();

        task::spawn(async move {
          let _subscription_permit = subscription_permit;
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
    let event_type = Self::event_type(&event);

    self
      .event_tx
      .send((event_type, event))
      .map_err(|err| anyhow::anyhow!("Failed to send event: {}", err))?;

    Ok(())
  }

  fn event_type(event: &WmEvent) -> SubscribableEvent {
    match event {
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
      WmEvent::TilingDirectionChanged { .. } => {
        SubscribableEvent::TilingDirectionChanged
      }
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
    }
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
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  use wm_common::{
    ContainerDto, TilingDirection, WorkspaceDto, WorkspaceLayout,
  };

  use super::*;

  #[test]
  fn repeated_diagnostics_emit_first_and_every_sixty_fourth_only() {
    let counter = AtomicUsize::new(0);
    let emitted: Vec<_> = (1..=130)
      .filter_map(|_| IpcServer::rate_limited_occurrence(&counter))
      .collect();
    assert_eq!(emitted, vec![1, 64, 128]);
    assert_eq!(counter.load(Ordering::Relaxed), 130);
  }

  #[test]
  fn connection_error_categories_do_not_include_error_payloads() {
    let secret = "credential=do-not-log";
    let error = anyhow::anyhow!(secret);
    let category = IpcServer::connection_error_category(&error);
    assert_eq!(category, "ipc");
    assert!(!category.contains(secret));
  }

  fn workspace_dto() -> ContainerDto {
    ContainerDto::Workspace(WorkspaceDto {
      id: Uuid::nil(),
      name: "1".to_string(),
      display_name: None,
      parent_id: None,
      children: vec![],
      child_focus_order: vec![],
      has_focus: true,
      is_displayed: true,
      width: 1920,
      height: 1080,
      x: 0,
      y: 0,
      layout: WorkspaceLayout::SplitHorizontal,
    })
  }

  #[test]
  fn ipc_parser_preserves_quoted_shell_argument_boundaries() {
    let command = IpcServer::parse_app_command(
      r#"command shell-exec pwsh.exe -Command "Write-Output \"hello world\"" "" "C:\path with space\\" & |"#,
    )
    .expect("quoted IPC command should parse");

    let AppCommand::Command {
      command:
        wm_common::InvokeCommand::ShellExec {
          hide_window,
          command,
        },
      ..
    } = command
    else {
      panic!("expected shell-exec IPC command");
    };
    assert!(!hide_window);
    assert_eq!(
      command,
      vec![
        "pwsh.exe".to_string(),
        "-Command".to_string(),
        r#"Write-Output "hello world""#.to_string(),
        String::new(),
        "C:\\path with space\\".to_string(),
        "&".to_string(),
        "|".to_string(),
      ]
    );
  }

  #[test]
  fn ipc_parser_rejects_unterminated_quote_before_dispatch() {
    let error = IpcServer::parse_app_command(
      r#"command shell-exec pwsh.exe "unterminated"#,
    )
    .expect_err("unterminated IPC command should be rejected");

    assert!(error.contains("unterminated quote"));
  }

  #[test]
  fn upstream_and_custom_layout_events_route_separately() {
    let tiling_direction = WmEvent::TilingDirectionChanged {
      direction_container: workspace_dto(),
      new_tiling_direction: TilingDirection::Vertical,
    };
    let layout = WmEvent::LayoutChanged {
      direction_container: workspace_dto(),
      new_layout: WorkspaceLayout::Grid,
    };

    assert_eq!(
      IpcServer::event_type(&tiling_direction),
      SubscribableEvent::TilingDirectionChanged
    );
    assert_eq!(
      IpcServer::event_type(&layout),
      SubscribableEvent::LayoutChanged
    );

    let tiling_json = serde_json::to_value(&tiling_direction)
      .expect("Tiling direction event should serialize.");
    let layout_json = serde_json::to_value(&layout)
      .expect("Layout event should serialize.");
    assert_eq!(
      tiling_json["eventType"],
      serde_json::json!("tiling_direction_changed")
    );
    assert_eq!(
      tiling_json["newTilingDirection"],
      serde_json::json!("vertical")
    );
    assert_eq!(
      layout_json["eventType"],
      serde_json::json!("layout_changed")
    );
  }

  #[test]
  fn upstream_tiling_direction_response_keeps_original_json_shape() {
    let value = serde_json::to_value(ClientResponseData::TilingDirection(
      TilingDirectionData {
        tiling_direction: TilingDirection::Horizontal,
        direction_container: workspace_dto(),
      },
    ))
    .expect("Tiling direction response should serialize.");

    assert_eq!(value["tilingDirection"], serde_json::json!("horizontal"));
    assert_eq!(
      value["directionContainer"]["type"],
      serde_json::json!("workspace")
    );
  }

  #[test]
  fn custom_layout_response_remains_available() {
    let value =
      serde_json::to_value(ClientResponseData::Layout(LayoutData {
        layout: WorkspaceLayout::Grid,
        direction_container: workspace_dto(),
      }))
      .expect("Layout response should serialize.");

    assert_eq!(value["layout"], serde_json::json!("grid"));
    assert_eq!(
      value["directionContainer"]["type"],
      serde_json::json!("workspace")
    );
  }

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

  #[test]
  fn connection_admission_is_hard_bounded_and_reusable() {
    let slots = Arc::new(Semaphore::new(2));
    let first = IpcServer::try_acquire_connection_slot(&slots)
      .expect("first connection should be admitted");
    let second = IpcServer::try_acquire_connection_slot(&slots)
      .expect("second connection should be admitted");
    assert!(IpcServer::try_acquire_connection_slot(&slots).is_none());

    drop(first);
    assert!(IpcServer::try_acquire_connection_slot(&slots).is_some());
    drop(second);
  }

  #[test]
  fn subscription_admission_is_per_connection_and_reusable() {
    let slots = Arc::new(Semaphore::new(2));
    let first = IpcServer::try_acquire_subscription_slot(&slots)
      .expect("first subscription should be admitted");
    let second = IpcServer::try_acquire_subscription_slot(&slots)
      .expect("second subscription should be admitted");
    assert!(IpcServer::try_acquire_subscription_slot(&slots).is_err());

    drop(first);
    assert!(IpcServer::try_acquire_subscription_slot(&slots).is_ok());
    drop(second);
  }

  #[tokio::test]
  async fn half_open_handshake_expires_at_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0")
      .await
      .expect("test listener should bind");
    let addr =
      listener.local_addr().expect("listener should have address");
    let client = tokio::spawn(async move {
      let _stream = TcpStream::connect(addr)
        .await
        .expect("half-open client should connect");
      tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let (stream, peer) = listener
      .accept()
      .await
      .expect("server should accept half-open client");
    let (message_tx, _) = mpsc::channel(1);

    let result = IpcServer::handle_connection_with_timeouts(
      stream,
      peer,
      message_tx,
      Duration::from_millis(20),
      Duration::from_secs(1),
      Duration::from_secs(1),
    )
    .await;

    assert!(result
      .expect_err("half-open handshake should time out")
      .to_string()
      .contains("Timed out during websocket handshake"));
    client.await.expect("client task should finish");
  }

  #[tokio::test]
  async fn established_idle_connection_expires_at_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0")
      .await
      .expect("test listener should bind");
    let addr =
      listener.local_addr().expect("listener should have address");
    let client = tokio::spawn(async move {
      let mut stream = TcpStream::connect(addr)
        .await
        .expect("websocket client should connect");
      let request = format!(
        "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
      );
      stream
        .write_all(request.as_bytes())
        .await
        .expect("client should send handshake");
      let mut response = [0u8; 1024];
      let read = stream
        .read(&mut response)
        .await
        .expect("client should read handshake response");
      assert!(String::from_utf8_lossy(&response[..read])
        .starts_with("HTTP/1.1 101"));
      tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let (stream, peer) = listener
      .accept()
      .await
      .expect("server should accept websocket client");
    let (message_tx, _) = mpsc::channel(1);

    let result = IpcServer::handle_connection_with_timeouts(
      stream,
      peer,
      message_tx,
      Duration::from_secs(1),
      Duration::from_millis(20),
      Duration::from_secs(1),
    )
    .await;

    assert!(result
      .expect_err("idle websocket should time out")
      .to_string()
      .contains("activity idle deadline"));
    client.await.expect("client task should finish");
  }

  #[cfg(target_os = "windows")]
  #[tokio::test]
  async fn missing_peer_owner_is_rejected_before_command_parsing() {
    let listener = TcpListener::bind("127.0.0.1:0")
      .await
      .expect("test listener should bind");
    let addr =
      listener.local_addr().expect("listener should have address");
    let client = tokio::spawn(async move {
      let mut stream = TcpStream::connect(addr)
        .await
        .expect("websocket client should connect");
      let request = format!(
        "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\ncommand shell-exec echo should-not-run"
      );
      stream
        .write_all(request.as_bytes())
        .await
        .expect("client should send a command-bearing handshake");
    });
    let (stream, peer) = listener
      .accept()
      .await
      .expect("server should accept websocket client");
    let nonexistent_peer = SocketAddr::new(peer.ip(), 1);
    let (message_tx, mut message_rx) = mpsc::channel(1);

    let result = IpcServer::handle_connection_with_timeouts(
      stream,
      nonexistent_peer,
      message_tx,
      Duration::from_secs(1),
      Duration::from_secs(1),
      Duration::from_secs(1),
    )
    .await;

    assert!(result
      .expect_err("unowned endpoint should be rejected")
      .to_string()
      .contains("authenticate IPC peer ownership"));
    assert!(message_rx.try_recv().is_err());
    client.await.expect("client task should finish");
  }

  #[cfg(target_os = "windows")]
  #[tokio::test]
  async fn current_user_peer_authorization_survives_reconnect_storm() {
    let listener = TcpListener::bind("127.0.0.1:0")
      .await
      .expect("test listener should bind");
    let addr =
      listener.local_addr().expect("listener should have address");

    for _ in 0..128 {
      let connect = TcpStream::connect(addr);
      let (client, accepted) = tokio::join!(connect, listener.accept());
      let client = client.expect("current-user client should connect");
      let (server, peer) = accepted.expect("server should accept client");

      IpcServer::authorize_peer(&server, peer)
        .await
        .expect("current-user reconnect should authenticate");
      drop(server);
      drop(client);
    }
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
