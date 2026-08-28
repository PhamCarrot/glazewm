# IPC compatibility, security, and migration reference

Last source reconciliation: **2026-08-28**, release **0.0.6**.

This build preserves the GlazeWM text-command WebSocket protocol at
`ws://127.0.0.1:6123` and adds custom layout/stack commands. The hardening work
did not introduce a protocol version negotiation or change successful response
JSON. Existing native same-user clients should continue to work; clients that
depend on browser WebSocket access, ambiguous whitespace splitting, unlimited
connections, or unbounded subscriptions must migrate as described below.

## Connection policy

- The listener binds only to IPv4 loopback `127.0.0.1:6123`; the port is not
  configurable.
- Before command parsing, Windows TCP ownership is checked. Both endpoints must
  resolve to a process owned by the same Windows user as GlazeWM. A peer whose
  owner cannot be established is rejected fail-closed.
- Any WebSocket handshake containing an `Origin` header is rejected with HTTP
  403. This intentionally blocks browser-origin command execution. Native
  clients should omit `Origin`, as the included CLI/client does.
- The listener socket is explicitly non-inheritable, so child processes cannot
  accidentally keep port 6123 alive after WM shutdown.

These controls defend the local command surface from another user session and
from ordinary browser pages. They are not a sandbox against malware already
running as the same Windows user; such a process can generally control that
user's desktop by other Windows APIs as well.

## Hard resource and time bounds

| Resource | Current hard bound | Saturation/failure behavior |
|---|---:|---|
| Simultaneous connections | 64 | Excess TCP clients are closed immediately. |
| Concurrent peer-owner checks | 2 | Admission waits at most 1 second, then rejects. |
| WebSocket handshake | 3 seconds | Half-open handshake is closed. |
| Frame/message payload | 64 KiB | Tungstenite rejects the oversized frame/message. |
| WM command queue | 256 messages | Queue admission waits at most 1 second, then closes/fails the connection. |
| Per-connection response queue | 64 messages | A full queue rejects the command before processing or ends a slow subscription. |
| Subscriptions per connection | 16 | Further subscriptions return an ordinary failed client response. |
| Inactive established connection | 5 minutes | Connection closes at its activity deadline. |
| Socket response/close write | 5 seconds | Slow/broken client is disconnected. |

The bounds are compile-time implementation policy, not configuration keys.
Clients should reconnect with bounded backoff rather than opening parallel
unbounded connections. A subscription consumer must continuously drain event
messages; a slow consumer is disconnected instead of accumulating memory.

## Command parsing migration

Raw WebSocket messages and YAML command strings now use one Windows
command-line parser. This preserves argument boundaries instead of splitting on
every whitespace character.

```text
command shell-exec pwsh.exe -Command "Write-Output \"hello world\"" ""
```

The example passes `Write-Output "hello world"` as one argument and then an
empty argument. Backslashes immediately before quotes follow Windows quoting
rules. Unicode and shell metacharacters are passed as literal argument text;
GlazeWM does not invoke a shell unless the requested executable is itself a
shell. Unterminated quotes reject the request before command dispatch.

Migration guidance:

- Preserve quotes around arguments containing spaces.
- Do not pre-split and then join arguments without Windows escaping; use the
  included CLI/client or an equivalent `CommandLineToArgvW`-compatible encoder.
- Do not rely on the old behavior that treated quote characters as ordinary
  bytes.

## Response contract

Server messages remain tagged by `messageType`:

```json
{
  "messageType": "client_response",
  "clientMessage": "query app-metadata",
  "data": { "version": "0.0.6" },
  "error": null,
  "success": true
}
```

Failed commands keep the same envelope with `data: null`, a private error
string, and `success: false`. `clientMessage` intentionally echoes the original
request for compatibility. Server diagnostics no longer log full command
arguments, but a client that stores its responses can still store that echoed
text; avoid putting credentials directly in command arguments.

The standard queries are `app-metadata`, `binding-modes`, `focused`,
`tiling-direction`, `monitors`, `windows`, `workspaces`, and `paused`. This fork
adds `layout`. Upstream `tiling-direction` and `tiling_direction_changed` retain
their original field names/JSON shape. Custom `layout` and `layout_changed` are
separate additive messages; a nested tiling-direction change does not emit a
misleading layout event.

## Event and workspace-transition ordering

Subscriptions use `messageType: event_subscription`, a UUID
`subscriptionId`, and a `data.eventType` payload. `all` includes both upstream
and custom events. The complete public event-name list is in
`CONFIG_REFERENCE.md`.

For an ordinary workspace switch, logical tree events describe the committed
logical state while native animation may still be presenting retained visual
coverage. For an incompatible rapid slide request, version 0.0.6 first stages
one bounded latest real-workspace underlay without mutating public focus. The
request may return success once safely staged; `WorkspaceUpdated`, focus state,
recent-workspace state, focus effects, and cursor targets change only when the
underlay is ready and commits. Newer rapid requests replace that one pending
destination. Integrations should react to emitted state/events or query state,
not assume command acknowledgement means compositor handoff has completed.

## Shell launch acknowledgement

`command shell-exec ...` returns success after the request is accepted by a
bounded 16-entry Windows Shell worker. Parse errors, missing worker, stopped WM,
and queue saturation remain synchronous failures. A later native Shell launch
failure is logged asynchronously and cannot be returned through the already
completed IPC request. This keeps Shell/UWP/App Paths resolution off the WM and
animation thread.

## Diagnostic privacy and compatibility

Verbose server logs sample successful/malformed requests and repeated native or
connection failures at the first occurrence and every 64th occurrence. Logs
contain a process-local numeric correlation ID, command/event category and safe
failure category—not command arguments, config contents, process paths, titles,
or DTO payloads. This affects diagnostics only; response payloads and command
execution are unchanged.

## Client acceptance checklist

1. Connect from a native same-user process without `Origin`.
2. Send a quoted command containing one spaced argument and one empty argument.
3. Verify `query app-metadata` returns `0.0.6`.
4. Verify both `query tiling-direction` and `query layout` parse their distinct
   response shapes.
5. Subscribe to both corresponding events and drain responses continuously.
6. Exercise reconnect with bounded backoff after an idle timeout or saturation
   rejection.
7. Verify the client does not persist secret-bearing `clientMessage` echoes.
