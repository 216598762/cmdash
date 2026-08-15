# Local compositor API

Phase 13 provides an optional, local-only API for automation, companion tools,
tests, and dashboards. The API is disabled by default. When enabled, it listens
on a Unix-domain socket and communicates with newline-delimited JSON messages.
It never exposes raw PTY streams, terminal escape injection, backend handles, or
live Rust structs.

## Safety and ownership

- The default is disabled and read-only.
- Enabled sockets are created with mode `0600` and are intended for the owning
  user. The socket path must be absolute or use `~/`; parent-directory and path
  traversal errors are rejected.
- Every application mutation is executed by the UI coordinator through the
  existing `AppState::dispatch(Command)` validation path.
- The API listener and client threads only parse bounded requests and carry
  response channels. They never access `AppState`, widgets, sessions, or the
  compositor.
- Arbitrary shell execution, raw PTY writes, raw escape sequences, clipboard
  contents, image uploads, and public TCP access are not API operations.
- A listener failure, disconnected client, full queue, or oversized message is
  reported to that client and cannot terminate or stall the dashboard.

The API is a local automation interface, not a network service. Do not expose
its Unix socket through a proxy or replace it with a network mount.

## Transport and wire format

The initial transport is a Unix socket. One client request is one JSON object
terminated by a newline, followed by one JSON response terminated by a newline.
Clients should use a fresh connection per request and close their write side
after sending the line.

Request envelope:

```json
{
  "version": 1,
  "request_id": "example-1",
  "method": "GET",
  "path": "/v1/health"
}
```

Mutation request with a body:

```json
{
  "version": 1,
  "request_id": "focus-1",
  "method": "POST",
  "path": "/v1/commands",
  "body": { "type": "focus_next" }
}
```

Successful response:

```json
{
  "version": 1,
  "request_id": "example-1",
  "ok": true,
  "result": { "status": "ok", "generation": 12, "api_version": 1 }
}
```

Error response:

```json
{
  "version": 1,
  "request_id": "example-1",
  "ok": false,
  "error": {
    "code": "read_only",
    "message": "mutating API operations are disabled"
  }
}
```

Supported request methods are `GET`, `POST`, and `DELETE`. The API version is
currently `1`; unknown versions are rejected rather than guessed. Request IDs
are 1–64 bytes and paths are limited to 256 bytes. Requests and responses are
bounded by configuration limits.

## Endpoints

All endpoints use the `/v1` prefix.

### Read-only endpoints

| Endpoint | Result |
| --- | --- |
| `GET /v1/health` | Process status, API version, and current frame generation. |
| `GET /v1/capabilities` | Transport, read-only state, graphics exposure, and allowed operations. |
| `GET /v1/workspace` | Workspace ID/name and current focus target. |
| `GET /v1/surfaces` | Surface/widget IDs, visibility, z-order, geometry, and focus. |
| `GET /v1/widgets` | Widget IDs, kinds, and bounded health summaries. |
| `GET /v1/compositor/frame` | Current viewport, bounded cells/styles, and optional graphics metadata. |
| `GET /v1/compositor/diff?from=N` | Empty changes for the current generation, or a bounded snapshot-required result/failure when history is unavailable. |
| `GET /v1/metrics` | Output frame and byte metrics. |
| `GET /v1/diagnostics` | Bounded application/widget diagnostics. |

A frame response is generated at a coordinator frame boundary. Its `generation`
identifies the state and scene snapshot together. Cells contain a symbol, width
classification, foreground/background color representation, and bold/dim flags.
Graphics responses contain only session-qualified resource metadata; encoded
image payloads are never returned unless a future explicitly bounded capability
adds them.

### Safe mutation endpoints

Mutations require `[api].read_only = false` or an equivalent explicit runtime
choice. `POST /v1/commands` accepts only this allowlist:

```json
{ "type": "request_redraw" }
{ "type": "focus_next" }
{ "type": "focus_previous" }
{ "type": "focus_surface", "id": 10 }
{ "type": "focus_clear" }
{ "type": "tab_next" }
{ "type": "tab_previous" }
{ "type": "pane_grow" }
{ "type": "pane_shrink" }
{ "type": "pane_close" }
{ "type": "pane_merge" }
{ "type": "pane_split", "direction": "horizontal" }
```

Focus, tab, pane, and surface validation remains in `AppState`; an invalid
surface, hidden target, missing pane, or unsupported split returns
`command_rejected` without changing state.

`POST /v1/reload` invokes the existing file-backed configuration validation and
replacement path. It fails when cmdash has no file-backed configuration, and a
rejected configuration leaves the active state unchanged.

### Polling subscriptions

`POST /v1/subscriptions` creates a bounded generation subscription:

```json
{
  "version": 1,
  "request_id": "sub-1",
  "method": "POST",
  "path": "/v1/subscriptions"
}
```

The response contains an ID and polling path. `GET /v1/subscriptions/{id}`
returns queued frame-generation events and drains them. Events contain metadata,
not full frames; clients fetch `/v1/compositor/frame` for the referenced
snapshot. `DELETE /v1/subscriptions/{id}` removes the subscription. Subscription
queues share the configured event-depth limit.

## Configuration

Add this optional section to a version-1 TOML file:

```toml
[api]
enabled = false
transport = "unix"
socket = "~/.cache/cmdash/cmdash.sock"
read_only = true
max_clients = 4
max_request_bytes = 65536
max_response_bytes = 1048576
event_queue_depth = 64
frame_history_depth = 4
expose_graphics = false
```

Defaults keep the listener off and mutating operations disabled. Limits are
validated before startup or reload:

- clients: 1–64;
- requests: 1 KiB–1 MiB;
- responses: 4 KiB–8 MiB;
- event/subscription queue: 1–1024;
- frame history: 0–64 snapshots;
- Unix socket path: at most 100 bytes, absolute or `~/`, with no `..` component.

The API listener is started from the validated configuration. Changes to
listener transport/path require restarting cmdash; ordinary workspace reloads
remain safe and atomic.

### CLI overrides

The dependency-free CLI supports:

```text
cmdash --api
cmdash --api-disable
cmdash --api-read-only
cmdash --api-socket <path>
cmdash --api --config <path>
```

CLI overrides apply after the selected TOML file and before validation. `--api`
and `--api-socket` enable the listener; `--api-read-only` enables it while
forcing read-only mode. `--api-disable` wins over a file's `enabled = true`.
The existing `--config` and `--migrate-config` behavior is unchanged.

## Errors and recovery

Common error codes include:

- `unsupported_version`, `invalid_request_id`, `invalid_path`,
  `unsupported_method`;
- `malformed_request`, `request_too_large`, `response_too_large`, `queue_full`,
  `client_limit`, `timeout`;
- `not_ready`, `not_found`, `read_only`, `invalid_command`,
  `command_rejected`, `reload_rejected`;
- `snapshot_required`, `subscription_limit`, and `subscription_not_found`.

If the socket cannot be created, startup fails with a normal configuration or
I/O error rather than starting an unadvertised network listener. If a socket
path already exists and is not a Unix socket, cmdash refuses to remove it.
Remove stale sockets only after verifying their ownership and process status.

## Compatibility and future transports

The `/v1` wire DTOs are a public boundary. Internal `Scene`, `FrameDiff`,
`Compositor`, session, and widget structs are deliberately not serialized
 directly. Additive fields may be introduced with defaults; changing field
meaning or command semantics requires a new API version. Unknown versions and
unknown commands fail closed.

The transport boundary leaves room for Windows named pipes or explicitly
configured loopback TCP in a future phase. Any future network transport must
require explicit binding, authentication, and a security review; it must not
inherit the Unix-socket trust model automatically.

## Testing contract

The API implementation tests request validation, JSON command allowlisting,
read-only authorization, state validation, snapshot generation, diff fallback,
configuration defaults, and bounded queues. Integration tests should exercise a
real temporary Unix socket, restrictive permissions, disconnected clients,
oversized requests/responses, reload precedence, and subscription event
coalescing before a network transport is considered.
