# cmdash Session Persistence Architecture

This document describes the planned client/server split that will enable
detach/attach semantics for cmdash. It is the implementation plan for
roadmap item §3.5.

## Status

**Design phase.** No code changes have been made yet. The migration will
proceed in three milestones (see [Migration Path](#migration-path)) so
the existing single-process `TickContext` is not rewritten all at once.

## Goals

- Survive terminal emulator close and SSH disconnect without killing
  running pane children.
- Allow multiple frontends to attach to the same session (one at a time
  for v1; concurrent read-only attach is a future goal).
- Keep PTY children, layout tree, tab stack, and scrollback history in a
  long-lived server process.
- Keep rendering, input handling, and host-terminal feature negotiation
  in a short-lived frontend process.

## Non-goals

- Persist session state across server reboot or OOM kill (v1 is
  memory-bound only).
- Support concurrent interactive attach from two frontends (v1 is
  single-writer attach).
- Reimplement graphics composition or VT parsing — termcompositor and
  `vte` remain the answer.

## Process model

```text
┌─────────────────────────────────────────────────────────────────────┐
│  Frontend (`cmdash`)                                                │
│  - Renders with ratatui + termcompositor                              │
│  - Reads crossterm input                                            │
│  - Sends keystrokes / resize / actions to server                    │
└───────────────────────┬───────────────────────────────────────────────┘
                        │ Unix domain socket / Windows named pipe
┌───────────────────────┴───────────────────────────────────────────────┐
│  Server (`cmdash server`)                                               │
│  - Owns PaneRunner / PanePty children                                   │
│  - Runs vte parser into TextGrid                                        │
│  - Owns LayoutNode, TabStack, Config                                    │
│  - Streams dirty rows and graphics commands to frontend                 │
└─────────────────────────────────────────────────────────────────────────┘
```

### CLI modes

| Command | Behaviour |
|---------|-----------|
| `cmdash` | Attach to the default session (`default`). If the server is not running, fork it first. |
| `cmdash attach <name>` | Attach to a named session. Fork a server if needed. |
| `cmdash detach` | Tell the currently attached frontend to detach (graceful exit). |
| `cmdash list-sessions` | List active session sockets in the runtime directory. |
| `cmdash kill-server [name]` | Terminate the server and all its pane children. |
| `cmdash server --session <name>` | Internal entry point used by the auto-fork machinery. |

### Server daemonization

When `cmdash` starts and no server exists for the target session:

1. Resolve the socket path (see [Session naming and storage](#session-naming-and-storage)).
2. `fork()` / `setsid()` using a crate such as `daemonize`.
3. In the daemon, bind the Unix domain socket / Windows named pipe.
4. The parent waits for the socket to appear (with a timeout), then connects.

The server keeps running as long as at least one pane child is alive. When
the last pane exits, the server exits and removes its socket.

## IPC transport

- **Unix**: `tokio::net::UnixListener` / `UnixStream`.
- **Windows**: `tokio::net::windows::named_pipe` (`NamedPipeServer` /
  `NamedPipeClient`).
- **Serialization**: `bincode` over `serde`.
  - Chosen for compact binary payloads and zero-copy-friendly layout,
    which matters when streaming text-cell updates at ~30 Hz.
  - Trade-off: no forward compatibility. Client and server must be built
    from the same commit. The handshake enforces this.
- **Authentication**: filesystem permissions only. Sockets are created
  with mode `0600` (owner read/write only). No TCP listener is ever
  opened.

## Wire protocol

After connection, the frontend and server perform a handshake, then
stream messages in both directions.

### Handshake

```text
Frontend -> Server: Handshake {
    version: String,      // env!("CARGO_PKG_VERSION")
    git_hash: String,     // built from build.rs / VERGEN
    terminal_size: (u16, u16),
}

Server -> Frontend: HandshakeAck {
    version: String,
    git_hash: String,
    session_name: String,
}
```

If `version` or `git_hash` do not match exactly, the server closes the
connection with a clear error. This prevents `bincode` schema mismatches.

### Frontend → Server messages

| Message | Purpose |
|---------|---------|
| `Input(Event)` | Raw crossterm input event (key press, mouse, paste). |
| `Action(KeyAction)` | Parsed host keybind action (e.g. `PaneFocusNext`, `TabNew`). |
| `Resize(u16, u16)` | Host terminal resized; server re-runs layout. |
| `CopySelection { pane, rect }` | Request selected text from the server-side grid. |
| `Detach` | Gracefully disconnect this frontend. |

### Server → Frontend messages

| Message | Purpose |
|---------|---------|
| `SyncFull { layout, grids, graphics, tabs, mode_flags }` | Full state snapshot sent on attach. |
| `FrameIncremental { dirty_rows, graphics, cursors, mode_flags }` | Per-tick delta. |
| `SessionEvent { kind }` | Server-side events such as `PaneClosed`, `ConfigReloaded`. |
| `Error(String)` | Non-fatal error string for the frontend to log. |

#### `FrameIncremental` details

- `dirty_rows: HashMap<PaneLayerId, Vec<(u16, Vec<Cell>)>>` — only rows
  that changed since the last successfully delivered frame.
- `graphics: Vec<(PaneLayerId, KittyGraphicCmd)>` — new image loads,
  placements, and deletes.
- `cursors: HashMap<PaneLayerId, (u16, u16)>` — cursor positions per pane.
- `mode_flags: HostModeFlags` — merged kitty keyboard, bracketed paste,
  focus reporting, and alternate-screen state.

The server clears its `dirty_rows` queue only after the message is
successfully encoded, so a transient frontend disconnect does not lose
updates.

## State ownership

### Server owns

- `Vec<PaneRunner>` and the underlying `PanePty` children.
- `TextGrid` per pane (including scrollback ring buffers).
- `LayoutNode` / `ComputedLayout`.
- `TabStack<TabState>`.
- Parsed `Config` and the filesystem watcher for hot reload.
- Per-pane mode state: `keyboard_flags`, `bracketed_paste_enabled`,
  `focus_reporting_enabled`, `alternate_screen`.
- A registry of active kitty image IDs per pane (for `SyncFull`
  reconstruction).

### Frontend owns

- `ratatui::Terminal` and the active crossterm backend.
- `GraphicsState` / `termcompositor::LayerStack` and decoded image
  buffers.
- A local replica of the currently visible tab's grids.
- `cmdash_keybinds::Router` and copy-mode UI state.
- Host terminal capability detection (`TermCapabilities`).

### Why this split?

PTY children must outlive the terminal emulator, so they live in the
server. The `termcompositor::LayerStack` is tied to the host terminal's
graphics protocol and must be recreated on every attach, so it lives in
the frontend. Text grids are deterministic state that can be replicated
to a new frontend on attach.

## Rendering split

### Text

The server runs the `vte::Parser` and produces `TextGrid` cells. It sends
explicit `Cell` arrays to the frontend. The frontend blits them into a
`ratatui::Frame` at the pane's computed rect, exactly as the current
`blit_grid` path does today.

### Graphics

The server intercepts kitty APC sequences and parses them into
`KittyGraphicCmd`. It forwards the command metadata to the frontend. The
frontend decodes image payloads with `image::load_from_memory` and
pushes them into its local `GraphicsState` / `termcompositor::LayerStack`.

On attach, the server re-sends `KittyGraphicCmd::Load` for every active
image so the new frontend can rebuild the layer stack from scratch.

## Lifecycle and attach/detach semantics

### Detach

- User runs `cmdash detach` or closes the terminal emulator.
- Frontend sends `Detach` and exits.
- Server notices the socket EOF, drops the client handler, and stops
  encoding `FrameIncremental` messages.
- PTY children keep running; scrollback continues to accumulate.

### Attach

- New frontend connects and sends `Handshake`.
- Server replies with `HandshakeAck`.
- Server sends `SyncFull` containing the current layout, all pane grids,
  active image loads, tab stack, and mode flags.
- Server resumes streaming `FrameIncremental` messages.

### Persistence boundary

Session state survives:

- Terminal emulator close.
- SSH session disconnect.
- Frontend crash.

Session state does **not** survive:

- Server process crash.
- Server kill (`cmdash kill-server`).
- System reboot or OOM kill.

## Session naming and storage

- Socket directory:
  - `$XDG_RUNTIME_DIR/cmdash/` if `XDG_RUNTIME_DIR` is set.
  - Otherwise `~/.local/share/cmdash/sockets/`.
- Socket name: `session-<name>.sock`.
- Default session name: `default`.
- Session metadata (name, creation time, pane count) can be read from the
  socket directory listing; no SQLite or JSON metadata file is required for
  v1.

## Migration path

To avoid a risky big-bang rewrite, the split will be implemented in
three milestones.

### Milestone 1: in-process channel split

Keep a single binary. Refactor `TickContext` into two internal tasks:

- `ServerTask` — owns runners, grids, layout, tabs, config.
- `FrontendTask` — owns ratatui terminal, graphics state, input handling.

Connect them with an `tokio::sync::mpsc` channel pair. Define the
`SyncFull` and `FrameIncremental` message types. Prove the frontend can
render correctly from replicated grids and explicit kitty command
envelopes.

### Milestone 2: serialization validation

Replace the in-memory `mpsc` channel with an internal Unix domain socket
pair. Force every payload through `bincode`. Measure bandwidth and latency
at 30 Hz to validate the protocol before exposing it to real attach/detach.

### Milestone 3: forking and CLI

Implement daemonization and the new CLI modes (`cmdash server`,
`cmdash attach`, `cmdash detach`, `cmdash list-sessions`,
`cmdash kill-server`). Add stale-socket cleanup and version-mismatch
handling.

## Error handling and recovery

| Scenario | Handling |
|----------|----------|
| Stale socket from crashed server | `connect()` returns `ECONNREFUSED`. Frontend unlinks the dead socket and forks a new server. |
| Version / git hash mismatch | Server closes the connection immediately with a clear error message. |
| Frontend crash | Server times out socket writes, drops the client handler, and keeps PTYs alive. |
| Server crash | All pane children receive `SIGHUP` from the kernel when the server exits. This is acceptable for v1. |
| Permission denied | Frontend prints a clear error and exits. |

## Security

- Sockets are created with `0600` permissions and owned by the launching
  user.
- No TCP listener is ever opened.
- Environment variables are captured at server spawn time. The frontend
  can pass updated variables (e.g. `$SSH_AUTH_SOCK`, `$DISPLAY`) in the
  `Handshake`, and the server applies them only to newly spawned panes.
- Existing pane children are not re-parented or re-environed.

## Open questions and risks

1. **Bandwidth at 30 Hz**: full-screen `Cell` deltas may be larger than
   raw PTY output for busy terminals. We should measure and consider
   run-length encoding or streaming raw output for high-traffic panes.
2. **Image payload size**: re-sending all active kitty image payloads on
   attach could be slow. We may need a lazy "load on first Place"
   strategy.
3. **Copy mode**: selection currently reads from the frontend's local
   grid replica. If the replica is incomplete, selection may behave
   differently than today. We may need the frontend to request full
   scrollback rows on demand.
4. **Windows named pipes**: the `tokio::net::windows::named_pipe` API is
   different enough from Unix sockets that the IPC layer needs a thin
   abstraction.
5. **Server idle shutdown**: should the server exit when the last pane
   closes, or keep running for re-attach? v1 will exit to keep the design
   simple.
