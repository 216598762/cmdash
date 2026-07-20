# cmdash — Development Roadmap

This document outlines the planned work to bring cmdash from its current
state to a mature terminal multiplexer + dashboard. Items are grouped by
priority tier. Within each tier, items are roughly independent and can be
tackled in parallel.

## Tier 1: Core usability gaps

These are the things that prevent cmdash from being usable as a daily
driver terminal multiplexer.

### 1.1 Runtime config file loading

**Status:** ✅ Working.

**Current state:** Config is loaded at startup via a priority chain:
`--config` CLI flag → `$CMDASH_CONFIG_DIR` env → `~/.config/cmdash/config.kdl`
XDG default → bundled `include_str!("../config.kdl")` fallback. A
filesystem watcher hot-reloads the config when the file changes at
runtime (re-parses and signals the main loop via channel).

**Implementation:**
- `resolve_config_path()` resolves the path from the priority chain.
- `read_config_text()` reads from the resolved path, falling back to the
  bundled default with appropriate logging.
- `--config=<path>` CLI flag for explicit override.
- `CMDASH_CONFIG_DIR` environment variable override.
- Filesystem watcher for runtime hot-reload.

**Remaining:**
- `figment` was originally planned for layered config (file → env → CLI)
  but was not needed — direct `std::fs` + `std::env` calls suffice.
- Pretty-print errors with file:line context (deferred to §3.6).

### 1.2 Tab bar rendering

**Status:** ✅ Working.

**Current state:** `TabStack<TabState>` is implemented and tab actions
(`TabNew`, `TabClose`, `TabSwitch(n)`) are wired through `TickContext`.
The tab bar is rendered in two layers:

- **Phase 3a (text):** `render_tab_bar()` renders a ratatui text tab bar
  at row 0 with active tab highlighted (Blue bg, White bold) and
  inactive tabs styled DarkGray/Gray. Called unconditionally in
  `TickContext::run` after pane grid rendering.
- **Phase 3b (pixel):** `graphics.update_tab_bar()` pushes termcompositor
  `RectLayer` + `TextLayer` overlays that overwrite the text tab bar on
  Kitty/Sixel-capable hosts.
- `TAB_BAR_HEIGHT` (1 row) reserves space at the top of the terminal;
  the layout area is reduced by that amount.

**Remaining:**
- Click-to-focus (future, requires mouse support §3.2).
- Configurable tab bar position (top/bottom) via KDL.
- Tab bar hidden when only 1 tab exists (configurable).

### 1.3 Per-pane shell specification

**Status:** ✅ Working.

**Current state:** `cmdash_config::Pane` has an optional `command: Option<String>`
field parsed from KDL (`pane kind=shell command="htop"`). `shell_spec_from_command()`
in `main.rs` converts the string into a `ShellSpec::Command { argv }` by
splitting on whitespace, falling back to `ShellSpec::LoginShell` when `None`.
The command is wired through `TickContext` during pane spawning.

**Implementation:**
- `Pane.command` field in `cmdash-config` (line 132).
- `read_pane()` parser handles `command="..."` entries.
- `shell_spec_from_command()` utility in `main.rs` (line 81).
- Example config: `examples/05-per-pane-commands.kdl`.

**Remaining:**
- `AppNewPane` spawns with `ShellSpec::LoginShell`, not the focused
  pane's command — could inherit for consistency.
- `split_whitespace()` doesn't handle shell metacharacters (quotes,
  redirects) — documented caveat.

### 1.4 Scrollback / alternate screen

**Status:** ✅ Working.

**Current state:** `TextGrid` has a ring-buffer scrollback
(`VecDeque<Vec<Cell>>`, default capacity 1000 rows). `scroll_up_one`
captures the top row into scrollback before shifting. `PageUp` enters
scrollback mode; `PageDown` scrolls toward live view (only intercepted
when already in scrollback, otherwise forwarded to the PTY for pagers).
Any non-PageUp/PageDown key resets to live view. `ESC [3J` clears the
scrollback buffer; `ESC [2J` only clears the visible screen (xterm
semantics). `blit_grid` renders scrollback rows above the live grid
when the viewport offset is > 0.

`cmdash-pty` parses `CSI ? 47 h`/`l`, `CSI ? 1047 h`/`l`, and
`CSI ? 1049 h`/`l` and surfaces `PaneEvent::AlternateScreen { enabled }`;
while the alternate screen is active, scrollback capture is suppressed so
full-screen TUIs do not pollute history. The binary tracks per-pane
alternate-screen state in `TickContext::pane_alternate_screen` and
disables PageUp/PageDown scrollback navigation while the focused pane is
in the alternate screen.

Per-pane scrollback capacity can be configured via KDL with
`scrollback-capacity=<n>` on any `pane` node; the value is threaded
through `PaneRunner::spawn_with_graphics_and_env` to `TextGrid::set_scrollback_capacity`.

**Remaining:**
- No remaining work.

### 1.5 Sixel fallback verification

**Status:** ✅ Unit tests verified. Manual terminal testing scripted.

**Current state:** `GraphicsProtocol` enum (Kitty/Sixel/TextOnly) with
`detect()` from `TERM`/`TERM_PROGRAM`/`CMDASH_GRAPHICS` env vars.
`render_and_write` dispatches to the kitty or sixel encoder based on
protocol, with TextOnly early-out. Startup logs the chosen protocol.
11 unit tests verify detection, encoding dispatch, and tab bar behavior.

**Manual verification:**
- `scripts/verify-sixel.sh` launches cmdash inside `xterm`, `mlterm`,
  and/or `foot` (whichever are installed) with `CMDASH_GRAPHICS=sixel`
  and a pane that emits a kitty graphics load command. It captures the
  host terminal output and checks for valid Sixel DCS sequences.
- `scripts/verify-kitty.sh` does the same for Kitty-capable terminals
  (`kitty`, `foot`, `wezterm`) with `CMDASH_GRAPHICS=kitty` and checks
  for valid Kitty APC-G sequences.
- `examples/11-sixel-test.kdl` and `examples/12-kitty-test.kdl` are
  standalone configs that run `examples/graphics-test-emitter.sh`
  with a `sixel` or `kitty` argument respectively.

**Device Attributes (DA1) query:** `query_device_attributes()` sends
`ESC[c` to the terminal and parses the response for Sixel attribute
4. Uses `poll(2)` via `extern "C"` (no background thread, no stray
bytes consumed on timeout). Gated behind `is_terminal()` so it's
skipped in CI/non-TTY environments. Only runs when env-var detection
yields `TextOnly` (avoids startup delay for configured users).

**Remaining:**
- Run `scripts/verify-sixel.sh` on a machine with `xterm`, `mlterm`,
  or `foot` installed and confirm all requested terminals pass.

## Tier 2: Extensibility

### 2.1 Native Rust widget SDK

**Status:** ✅ Working.

**Current state:** `cmdash-widget-sdk` defines a c-ABI-safe
`CmdashWidget` trait, a `WidgetEvent` enum (Key, Resize, FocusGained,
FocusLost), a pinned `CMDASH_WIDGET_ABI_VERSION`, and a
`cmdash_widget_export!` macro that generates the required
`cmdash_widget_create` C-ABI entry point. The host binary loads widgets
at startup from `~/.config/cmdash/widgets/<name>/` via `libloading` and
calls `widget.render(area, frame)` once per frame for each
`pane kind=widget ref_name="<name>"` leaf. An example widget is provided
at `examples/widget-clock/`.

**Implementation:**
- `CmdashWidget` trait in `crates/cmdash-widget-sdk/src/lib.rs`.
- `widget_into_raw` / `widget_from_raw` double-box FFI helpers.
- `cmdash_widget_export!` macro for `.so` authors.
- `load_widgets()` in `crates/cmdash/src/main.rs` scans the widget
  directory and loads matching `.so`/`.dll`/`.dylib` files.
- `PaneKind::Widget { ref_name: String }` parsed from KDL.
- Integration tests in `crates/cmdash/tests/widget_sdk_integration.rs`
  cover cdylib loading, FFI round-trip, zero-area handling, offset
  rendering, and object safety.

**Remaining:**
- Hot-reload of widgets at runtime (out of scope for v1).

### 2.2 Script widget protocol

**Status:** ✅ Working.

**Current state:** `cmdash-protocol` implements the line-delimited
frame protocol. The host spawns a script process with piped stdin/stdout
and sends `FRAME`, `KEY`, `RESIZE`, and `FOCUS` messages. The script
replies with a `FRAME width=... height=...` header followed by ANSI text
lines. `ScriptWidget` in `crates/cmdash/src/script_widget.rs` implements
`CmdashWidget` so script widgets plug into the same render path as
native widgets.

**Implementation:**
- `HostMsg` enum and `FrameResponse` parsing in
  `crates/cmdash-protocol/src/lib.rs`.
- `ScriptWidget::spawn` and `CmdashWidget` impl in
  `crates/cmdash/src/script_widget.rs`.
- `PaneKind::Script` parsed from KDL (`pane kind=script command="..."`).
- Integration tests in `crates/cmdash/tests/script_widget_integration.rs`
  cover spawn/lifecycle, frame round-trip, event forwarding, repeated
  renders, and immediate-exit handling.

**Remaining:**
- Pixel-bitmap frame mode (future goal, v1 is line+ANSI only).
- Mouse event forwarding to scripts (message type exists but is not yet
  wired in `ScriptWidget`).

### 2.3 Optional status bar

**Status:** ✅ Working.

**Current state:** An optional status bar is configurable via the
`status_bar { ... }` KDL block. It is disabled by default; when enabled,
one row is reserved and the layout area is reduced accordingly. The status
bar renders in phase 3a after pane blits and shows the current keybind
mode, the focused pane's label, and the current time. It is
hot-reloadable via the config file watcher.

**Example:**
```kdl
status_bar {
    enabled     #true
    position    "bottom"    // or "top"
    show-clock  #true
    show-pane-title #true
    show-mode   #true
}
```

**Implementation:**
- `Bar` struct and `read_status_bar()` parser in `cmdash-config`.
- `render_status_bar()` in `crates/cmdash/src/status_bar.rs`.
- Layout area reduction and rendering wired in `TickContext::run`.
- Unit tests in `crates/cmdash/src/status_bar.rs`.

**Remaining:**
- Configurable format strings / additional fields (CPU, memory, etc.).

### 2.4 Additional keybind modes

**Status:** ✅ Working.

**Current state:** All four modes are routed: `Normal`, `PaneResize`,
`TabSwitch`, and `PresetPick`. `Normal` handles global bindings;
`PaneResize` routes arrow keys to adjust the focused pane's parent split
ratio; `TabSwitch` routes number keys 1–9 for tab switching; `PresetPick`
routes number keys for preset selection. Escape exits any non-Normal
mode.

**Implementation:**
- `cmdash_keybinds::Mode` enum and `Router::set_mode()`.
- `KeyAction::{EnterPaneResize, EnterTabSwitch, EnterPresetPick,
  ModeExit}` plus `PaneResize{Up,Down,Left,Right}`.
- Mode transitions tested in `crates/cmdash/tests/mode_transitions.rs`.

## Tier 3: Polish and robustness

### 3.1 Async I/O migration

**Status:** ✅ Complete.

**Current state:** The main loop is now async (`#[tokio::main]`).
Crossterm input is read from a `tokio::task::spawn_blocking` task and
forwarded over an unbounded channel, and the tick loop uses
`tokio::select!` to await input, pane close notifications, config reload,
and the periodic tick interval. `std::sync::mpsc` has been replaced with
`tokio::sync::mpsc::unbounded_channel` for both the pane close channel
and the config reload channel. Tests use `#[tokio::test]` and the
`tokio::time::timeout` helper where needed.

### 3.2 Mouse support

**Status:** ✅ Working.

**Current state:** Mouse capture is enabled (`EnableMouseCapture`) and
crossterm mouse events are handled in `TickContext::handle_mouse_event`.
Left-click focuses the pane under the cursor; Alt+left-click-drag adjusts
the nearest parent Split's ratio; scroll-wheel and other mouse events
are forwarded to the focused pane's PTY as SGR extended mouse sequences.

**Implementation:**
- `focus_by_click()` maps a mouse click to the pane under the cursor.
- `start_drag_resize()` / `update_drag_resize()` implement Alt+drag
  split resizing.
- `forward_mouse_to_pty()` encodes mouse events as SGR sequences and
  writes them to the focused pane's PTY.
- Mouse events are consumed by the host before reaching the child PTY,
  except for forwarded SGR sequences.

**Remaining:**
- Scroll-wheel scrollback navigation (currently scroll-wheel events are
  forwarded to the focused pane's PTY rather than driving the scrollback
  viewport).

### 3.3 Theme / color customization

**Status:** ✅ Working.

**Current state:** `cmdash_config::Theme` provides 15 configurable color
keys (default fg/bg, cursor style, tab bar, status bar, widget/border
colors) parsed from an optional `theme { ... }` KDL block. All keys are
optional — omitted keys keep their built-in defaults. Color formats
include named colors, hex RGB (`"#ff8800"`), RGB tuples
(`"rgb(255,136,0)"`), indexed 256 (`"i196"`), and `"reset"`. Cursor
style supports `"block"`, `"underline"`, and `"bar"` (with aliases).

**Implementation:**
- `Theme` struct with 15 `Option<Color>` fields in `cmdash-config/src/theme.rs`.
- `parse_color()` for named/hex/rgb/indexed/reset color formats.
- `read_theme()` KDL walker in `cmdash-config/src/lib.rs`.
- Hot-reload support via the existing config file watcher.
- `ScriptWidget::set_theme()` wires theme colors to script widget borders.
- `render_tab_bar()` and `render_status_bar()` accept `&Theme`.
- `docs/configuration.md` §3.5 documents all recognized keys.
- `examples/09-theme.kdl` showcases dark/light theme examples.
- Commented-out `theme { ... }` block in bundled `config.kdl`.

**Remaining:**
- Apply theme colors to remaining hardcoded palette entries (e.g.
  native widget borders beyond script widgets).

### 3.4 Clipboard integration

**Status:** ✅ Working.

**Current state:** `Event::Paste` is forwarded to the focused pane's
PTY, wrapped in bracketed-paste delimiters when the pane has requested
bracketed-paste mode. A copy-mode (`Mode::Copy`) lets the user select
text in the focused pane and copy it to the system clipboard via the
`arboard` crate.

**Implementation:**
- `Event::Paste` is handled by `TickContext::handle_paste`, which wraps
  the pasted text in `ESC [ 200 ~` / `ESC [ 201 ~` when the focused
  pane has requested bracketed paste.
- Copy-mode actions are parsed from KDL (`copy.enter`,
  `copy.move.up/down/left/right`, `copy.select`, `copy.copy`).
- `Mode::Copy` is routed through `cmdash_keybinds::Router` with default
  keybinds: arrow keys move the cursor, `v` starts/extends the
  selection, `y` or Enter copies and exits.
- `CopyModeState` tracks the cursor and selection anchor in pane-local
  visual coordinates.
- `blit_selection` renders the selected region with reversed video.
- `extract_selected_text` reads the selected cells from the focused
  pane's latest snapshot and copies them to the system clipboard.

**Remaining:**
- Selection currently reads from the live grid only; scrollback-aware
  selection is a future enhancement.
- OSC 52 clipboard integration (child PTY → system clipboard) is
  tracked separately in §4.5.

### 3.5 Session persistence (detach/attach)

**Status:** Architecture complete; implementation not started.

**Current state:** cmdash runs as a foreground process. When the
terminal closes or the SSH session drops, all panes are killed.

**Goal:** Support detach/attach like tmux, so a cmdash session survives
terminal disconnect and can be reattached later.

**Architecture:** See `docs/session-persistence-architecture.md` for the
full design. In summary:
- A long-lived **server** process owns the PTY children (`PaneRunner`),
  `TextGrid` state, layout tree, tab stack, and config.
- A short-lived **frontend** process handles crossterm input, ratatui
  text rendering, and the local `GraphicsState` / termcompositor layer stack.
- Frontend and server communicate over a Unix domain socket (named pipe on
  Windows) using a `bincode`-serialized protocol.
- On attach, the server sends a `SyncFull` snapshot; afterwards it streams
  `FrameIncremental` deltas at the tick rate.
- Kitty/Sixel graphics commands are forwarded to the frontend, which owns
  the decoded image layers.

**CLI surface:**
- `cmdash` — attach to the default session, forking a server if needed.
- `cmdash attach <session>` — attach to a named session.
- `cmdash detach` — gracefully disconnect the current frontend.
- `cmdash list-sessions` — list active sessions.
- `cmdash kill-server [session]` — terminate the server and its panes.

**Migration path (3 milestones):**
1. **In-process channel split:** refactor `TickContext` into internal
   `ServerTask` and `FrontendTask` connected by `tokio::sync::mpsc`.
2. **Serialization validation:** replace the in-memory channel with an
   internal Unix socket pair and force all payloads through `bincode`.
3. **Forking and CLI:** implement daemonization, stale-socket cleanup,
   version-mismatch handling, and the new CLI modes.

**Remaining:**
- Implement milestones 1–3.
- Measure bandwidth at 30 Hz and optimize `FrameIncremental` deltas if
  needed (run-length encoding, raw-output fallback for high-traffic panes).
- Decide whether the server should idle-exit when the last pane closes
  (v1: yes, to keep the design simple).

### 3.6 Configuration validation and error reporting

**Status:** ✅ Working.

**Current state:** `parse_with_validation()` and `validate()` perform
semantic validation after parsing. Duplicate chord detection warns
with last-wins semantics. Preset reference validation catches
`pane.preset.<name>` keybinds referencing undefined presets. Layout
tree depth is validated against `MAX_TREE_DEPTH` (8) — hard error
in `parse_with_validation()`, warning in `validate()`. The 2-child
`split` limit is enforced at parse time by `read_split()`. The
`format_error_with_context()` helper pretty-prints errors with
file labels and best-effort source line display.

**Implementation:**
- `ConfigWarning` enum: `DuplicateChord`, `MissingPresetRef`,
  `TreeTooDeep`.
- `ConfigError::TreeTooDeep(depth, max)` for hard depth violations.
- `parse_with_validation()` returns `(Result<Config, ConfigError>,
  Vec<ConfigWarning>)`.
- `validate()` standalone post-parse check.
- `format_error_with_context(err, source, file_label)` for
  pretty-printed error display.
- Duplicate chord detection via `HashSet` with `Hash` derives on
  `Modifiers`/`KeyToken`/`KeyName`.
- Binary wiring: both startup parse and config watcher use
  `parse_with_validation`; warnings logged via `tracing::warn!`.

**Remaining:**
- Source-line-aware error display (caret pointer) — currently
  best-effort substring match; full column tracking needs KDL span
  integration.

## Tier 4: Modern terminal emulator extensions

Goal: make cmdash a first-class modern terminal host by passing through,
emulating, or explicitly negotiating the terminal extensions that
contemporary TUI applications expect. These features are grouped by
function: input, output, rendering, synchronization, queries/reports,
image protocols, and Unicode/text layout.

### 4.1 Kitty keyboard protocol

**Status:** ✅ Working.

**Current state:** PTY-side VTE parsing intercepts `CSI =/>/< u`
sequences (set/push/pop) and emits `PaneEvent::KeyboardEnhancement`.
`PanePty` tracks `keyboard_flags` per-pane; `PanePtyOps` trait exposes
`keyboard_flags()`. Host-side, `TickContext` pushes/pops keyboard
enhancement on the host terminal and maintains per-pane flag tracking
with merge semantics (flags accumulate across ticks; stale entries
pruned on pane close). Enhanced key events are encoded via
`encode_kitty_key_event` when the focused pane requests them; legacy
encoding is used otherwise.

**Implementation:**
- `PaneEvent::KeyboardEnhancement { flags }` in `cmdash-pty`.
- `PanePty::keyboard_flags` field updated in `advance()` from events.
- `PanePtyOps::keyboard_flags()` trait method.
- `collect_keyboard_enhancement_flags()` helper in `cmdash::pane`
  (pub, takes `&mut HashMap<PaneLayerId, u8>`, returns `bool`).
- `TickContext` state: `host_keyboard_flags`, `pane_keyboard_flags`,
  `host_keyboard_pushed`.
- `sync_host_keyboard_flags` / `push_host_keyboard_flags` /
  `pop_host_keyboard_flags` lifecycle methods.
- `handle_event_full` routes widget press events separately;
  PTY panes use Kitty encoding when `focused_flags != 0` and the host
  terminal advertises Kitty keyboard support via
  `TermCapabilities::supports_kitty_keyboard()`; legacy encoding is
  used otherwise.
- `drain_close_channel` prunes `pane_keyboard_flags` on close.
- `pop_host_keyboard_flags` called on `run()` exit.

**Known tech debt:**
- Dual flag tracking: `PanePty::keyboard_flags()` returns a cached
  value updated in `advance()`, while `collect_keyboard_enhancement_flags`
  reads from snapshot events. Both derive from the same source so they
  stay consistent, but a cross-linking comment would help maintainers.
- `drain_close_channel` calls `sync_host_keyboard_flags` unconditionally
  when any pane closes; short-circuit when the closed pane's flags were
  already 0 to avoid a redundant host escape sequence write.

**Remaining:**
- Negotiate with the host terminal via `CSI > 1 u` / `CSI < u`.
  Currently, enhancement is pushed when any pane requests it, but
  the initial negotiation sequence is not sent on startup.

### 4.2 Bracketed paste

**Status:** ✅ Working.

**Current state:** PTY-side VTE parsing intercepts `CSI ? 2004 h`/`l`
sequences and emits `PaneEvent::BracketedPaste { enabled }`.
`PanePty` tracks bracketed-paste state per-pane; `PanePtyOps` exposes
`bracketed_paste_enabled()`. Host-side, `TickContext` enables/disables
bracketed paste on the host terminal and maintains the host state as the
*union* of all live pane requests, so focus changes never disable the
mode while any pane still needs it. Pasted content is wrapped in
`ESC [ 200 ~` / `ESC [ 201 ~` and forwarded to the focused pane's PTY.

**Implementation:**
- `PaneEvent::BracketedPaste { enabled }` in `cmdash-pty`.
- `PanePty::bracketed_paste_enabled` field updated in `advance()` from events.
- `PanePtyOps::bracketed_paste_enabled()` trait method.
- `collect_bracketed_paste_flags()` helper in `cmdash::pane` merges
  per-pane state and detects changes.
- `TickContext` state: `host_bracketed_paste_enabled`,
  `pane_bracketed_paste_flags`.
- `sync_host_bracketed_paste` enables/disables host bracketed paste
  when the union changes.
- `prepare_paste_bytes()` wraps pasted text only when the focused pane
  has requested bracketed paste and the host terminal advertises
  bracketed-paste support via
  `TermCapabilities::supports_bracketed_paste()`; raw paste is used
  otherwise.
- Integration test `host_bracketed_paste_union_across_focus_changes`
  verifies the union semantics across focus changes.

**Remaining:**
- No remaining work.

### 4.3 Focus reporting

**Status:** ✅ Working.

**Current state:** PTY-side VTE parsing intercepts `CSI ? 1004 h`/`l`
sequences and emits `PaneEvent::FocusReporting { enabled }`.
`PanePty` tracks `focus_reporting_enabled` per-pane; `PanePtyOps` exposes
`focus_reporting_enabled()`. Host-side, `TickContext` enables/disables
focus-change reporting on the host terminal and maintains the host state as
the *union* of all live pane requests. When the host gains or loses focus,
`CSI I` / `CSI O` is forwarded to the focused pane. When a pane newly
enables focus reporting, the current host focus state is immediately
reported to that pane.

**Implementation:**
- `PaneEvent::FocusReporting { enabled }` in `cmdash-pty`.
- `PanePty::focus_reporting_enabled` field updated in `advance()` from events.
- `PanePtyOps::focus_reporting_enabled()` trait method.
- `collect_focus_reporting_flags()` helper in `cmdash::pane` merges
  per-pane state and detects changes.
- `TickContext` state: `host_focus_reporting`, `pane_focus_reporting`,
  `host_focus_reporting_pushed`, `host_focused`.
- `sync_host_focus_reporting` enables/disables host focus-change
  reporting when the union changes.
- `forward_focus_event_to_focused_pane` forwards `CSI I`/`CSI O` to
  the focused pane on host focus changes.
- `update_focus_reporting_from_snapshots` sends the initial host focus
  state to any pane that just enabled focus reporting, gated by host
  support via `TermCapabilities::supports_focus_events()`.

**Remaining:**
- No remaining work.

### 4.4 Hyperlinks (OSC 8)

**Status:** Partial.

**Current state:** `cmdash-pty` parses OSC 8 hyperlink sequences in
`osc_dispatch` and surfaces `PaneEvent::Hyperlink { uri }`. The URI is
interned in `TextGrid::urls` and attached to newly printed cells via the
`link_id` field. This prepares the ground for per-cell hyperlink
rendering and host-terminal forwarding, but the binary does not yet
consume `PaneEvent::Hyperlink` in the tick loop or emit the corresponding
OSC 8 sequence to the host terminal.

**Remaining:**
- Forward open/close hyperlink events from pane snapshots to the host
  terminal as raw OSC 8 sequences (with URI sanitization).
- Per-cell hyperlink rendering in the ratatui/termcompositor text layer
  (future goal; v1 will forward to the host terminal only).

### 4.5 OSC 52 clipboard integration

**Status:** ✅ Working.

**Current state:** PTY-side VTE parsing intercepts OSC 52 set/query
sequences and emits `PaneEvent::ClipboardOsc52 { clipboard, action }`.
`PanePty` tracks pending clipboard events per-pane. Host-side,
`TickContext` routes the events to the system clipboard according to the
`clipboard { osc52 ... }` config policy.

**Implementation:**
- `PaneEvent::ClipboardOsc52 { clipboard, action }` and
  `Osc52Action::{Set, Query}` in `cmdash-pty`.
- `collect_osc52_events()` helper in `cmdash::pane` gathers pending
  clipboard events from pane snapshots.
- `TickContext::update_osc52_from_snapshots()` applies the configured
  `Osc52Policy`:
  - `Disabled` — ignores all OSC 52 requests.
  - `WriteOnly` — writes decoded `Set` text to the system clipboard via
    `arboard`.
  - `ReadWrite` — writes `Set` text and responds to `Query` requests
    with the current system clipboard contents encoded as an OSC 52
    sequence.
- `TickContext::encode_osc52_response()` formats OSC 52 response
  sequences with base64-encoded text.
- `cmdash_config::ClipboardConfig` and `Osc52Policy` parsed from the
  top-level `clipboard { osc52 "..." }` KDL block.
- Unit tests in `crates/cmdash/src/main.rs` cover set/query routing and
  response encoding.
- Documentation in `docs/configuration.md` §4.6 and §6.8.

**Remaining:**
- No remaining work.

### 4.6 Synchronized output (BSU/ESU)

**Status:** Not started.

**Goal:** Support synchronized output DCS sequences (`CSI ? 2026 h`/`l`).

**Steps:**
- Buffer output between Begin Synchronized Update (BSU) and End
  Synchronized Update (ESU).
- Flush atomically on ESU to avoid tearing.
- Coalesce with the existing tick loop frame boundaries.

### 4.7 Extended SGR attributes

**Status:** Partial (cursor styles done).

**Goal:** Support undercurl, colored underlines, strikethrough, italic,
and bold.

**Steps:**
- Ensure the `vte` parser preserves extended SGR attributes.
- Extend the internal cell attribute model.
- Map attributes to termcompositor text styling.

### 4.8 True color / 24-bit color guarantees

**Status:** Working (via `ratatui`/`termcompositor`).

**Goal:** Ensure 24-bit color is preserved end-to-end.

**Steps:**
- Audit color handling in the `vte` → termcompositor path.
- Add tests for 24-bit color round-trip.
- Document host terminal true-color requirements.

### 4.9 Bi-directional text and complex scripts

**Status:** Not started.

**Goal:** Correctly render Arabic, Hebrew, and mixed-direction text.

**Steps:**
- Evaluate `unicode-bidi` / `harfbuzz` integration.
- Handle bidi reordering in the text grid.
- Preserve logical-to-visual cursor mapping.

### 4.10 Font ligatures

**Status:** Not started.

**Goal:** Render font ligatures when the host font supports them.

**Steps:**
- Pass ligature hints to the termcompositor font rasterizer.
- Detect ligature-friendly fonts.
- Provide a config toggle for ligature rendering.

### 4.11 Emoji and grapheme clusters

**Status:** Partial.

**Goal:** Correctly handle emoji ZWJ sequences and wide characters.

**Steps:**
- Use `unicode-width` / `unicode-segmentation` for width calculations.
- Ensure cursor movement accounts for wide characters.
- Update grapheme cluster handling as Unicode versions evolve.

### 4.12 Color palette queries (OSC 4/10/11)

**Status:** Not started.

**Goal:** Respond to color queries from child PTYs.

**Steps:**
- Maintain palette state synchronized with the active theme.
- Reply to OSC 4 (indexed), OSC 10 (foreground), and OSC 11 (background)
  queries.
- Handle palette updates from child PTYs.

### 4.13 Window title reports

**Status:** Not started.

**Goal:** Set the host terminal title from child PTY OSC 2 sequences.

**Steps:**
- Intercept `OSC 2` / `OSC 0` title sequences from child PTYs.
- Emit the title to the host terminal.
- Optionally display the title in the status bar.

### 4.14 Desktop notifications

**Status:** Not started.

**Goal:** Support OSC 777 and OSC 99 notifications.

**Steps:**
- Intercept notification escape sequences from child PTYs.
- Integrate with OS notification APIs.
- Provide a config toggle and per-pane allowlist.

### 4.15 Additional image protocols

**Status:** Partial (Kitty/Sixel done).

**Goal:** Support iTerm inline images, Contour, and WezTerm image
extensions.

**Steps:**
- Extend the `cmdash-pty` graphics parser.
- Route decoded images to termcompositor image layers.
- Maintain per-pane image ID namespaces.

### 4.16 Unicode version support

**Status:** Not started.

**Goal:** Track the Unicode version used for width and segmentation.

**Steps:**
- Pin Unicode version in dependency manifests.
- Document the supported Unicode version.
- Update as new Unicode versions are released.

### 4.17 DECRPM / mode reports

**Status:** Not started.

**Goal:** Respond to `DECRQM` / `DECRPM` queries.

**Steps:**
- Implement a mode report state machine.
- Reply with accurate mode values for supported modes.
- Track private and standard modes per pane.

### 4.18 Soft fonts (DRCS)

**Status:** Not started.

**Goal:** Support `DECDLD` soft font loading.

**Steps:**
- Capture soft font definitions from child PTYs.
- Route them to the termcompositor font rasterizer.
- Manage per-pane font glyph caches.

### 4.19 Overline / double underline

**Status:** Not started.

**Goal:** Support SGR 53/55/21 and `SGR 4:2`/`4:3` underline styles.

**Steps:**
- Extend the cell attribute model.
- Map styles to termcompositor text rendering.
- Add tests for underline style round-trip.

### 4.20 Capability advertisement to child PTYs

**Status:** ✅ Working.

**Goal:** Make cmdash a transparent modern-terminal proxy by advertising
the host terminal's supported capabilities to each child PTY, and by
responding to capability queries from child applications. This is the
cross-cutting foundation that lets features like Kitty keyboard,
bracketed paste, focus reporting, and true color be negotiated rather
than assumed. It also unblocks the host-capability fallback work
remaining in §4.1–§4.3.

**Current state:**
- `TermCapabilities` registry captures host support for graphics
  (Kitty/Sixel), input (Kitty keyboard, focus events, bracketed paste),
  color (true color, 256 color), and queries (DA1/DA2).
- Host capabilities are detected at startup from `TERM`,
  `TERM_PROGRAM`, `COLORTERM`, and the `CMDASH_GRAPHICS` override.
- Capabilities are advertised to child PTYs via `TERM`, `COLORTERM`,
  and custom `CMDASH_*` environment variables (`CMDASH_GRAPHICS`,
  `CMDASH_KITTY_KEYBOARD`, `CMDASH_FOCUS_EVENTS`,
  `CMDASH_BRACKETED_PASTE`, `CMDASH_QUERIES`).
- DA1/DA2 query responses are generated from `TermCapabilities` and
  written back to the requesting child PTY.
- Documentation is complete: see `docs/configuration.md` §3 and
  `README.md` "Environment variables".

**Remaining:**
- Reconcile per-pane feature requests against the host capability set;
  fall back to legacy behavior when the host does not support a
  requested feature.

## Tier 5: Adopt termcompositor v2.0.0 capabilities

termcompositor was renamed from `dashcompositor` and shipped v2.0.0
with a substantial set of new capabilities that cmdash's current
graphics adapter (`crates/cmdash/src/graphics.rs`) does not yet use.
The rename itself is complete — all `dashcompositor` mentions are now
`termcompositor`, the workspace dependency points at the v2.0.0 git
source, and the workspace MSRV was raised from 1.73 to 1.85 (the
`kitty-encoder`, `sixel-encoder`, and `image-decoder` features require
Rust ≥ 1.85 per upstream's feature-flag note). This tier collects every
refactor that would let cmdash benefit from the new surface; items are
ordered roughly by impact, with the highest-leverage perf win
(diff-based rendering) first.

The v1 graphics adapter still works unchanged against v2.0.0 because
the rename preserved the existing API (`encode_passthrough_to_writer`,
`LayerStack`, `ImageLayer::from_dynamic`, `CpuCompositor`, `RectLayer`,
`TextLayer`, `encoder::encode_to_writer`). Nothing below is a blocker;
each item is independently shippable.

### 5.1 Diff-based rendering via `DirtyRegion` / `render_diff`

**Status:** Not started. **Highest leverage.**

**Current state:** `GraphicsState::render_and_write` composites the
entire `LayerStack` into a fresh `FrameBuffer` every tick via
`CpuCompositor.compose(&self.stack, &mut fb)`, then encodes the whole
framebuffer through the kitty or sixel encoder. At 80×24 cells × 8×16
px/cell that is a 640×384 RGBA buffer re-composited and re-encoded 30
times per second even when nothing on screen changed.

**Opportunity:** termcompositor v2.0.0 adds `DirtyRegion` and
`LayerStack::render_diff(target, dirty)`:
- `DirtyRegion::mark_rect(DirtyRect::new(x, y, w, h))` declares a
  sub-rectangle dirty.
- `DirtyRegion::mark_full()` forces a full re-render.
- `LayerStack::render_diff(target, dirty)` composites only layers whose
  bounding boxes intersect the dirty regions and copies just those
  regions into `target`; the rest of the framebuffer is preserved from
  the previous frame.

**Refactor:**
- Add a `dirty: DirtyRegion` field to `GraphicsState` and a persistent
  `FrameBuffer` (re-allocated only on `set_cells` resize) instead of a
  fresh one per tick.
- Mark dirty on every layer push/remove and on every `Place`/`Delete`
  kitty event (using the old + new bounding box).
- In `update_tab_bar`, mark the tab bar row dirty only when the active
  tab, label set, or bar width actually changes (the existing
  `TODO(v2): add a dirty flag` comment in `graphics.rs` already calls
  this out).
- Replace the `CpuCompositor.compose` call in `render_and_write` with
  `self.stack.render_diff(&mut self.fb, &mut self.dirty)`.
- Keep the existing `images.is_empty() && tab_bar_layers.is_empty()`
  early-out as the "nothing ever pushed" fast path; the dirty path is
  the "something pushed but unchanged this tick" fast path.

**Expected win:** at idle (no pane output, no tab switch), the
compositor does zero per-pixel work and the encoder emits nothing; the
host terminal's CPU stays flat. Under typing, only the focused pane's
changed rows are re-composited.

### 5.2 Animation loop (`animation::run` / `AnimConfig`)

**Status:** Not started.

**Current state:** cmdash owns its own 33 ms tick interval via
`tokio::time::interval` in `TickContext::run` / `ServerTask::run`,
coalescing input, close-channel drains, config reload, and the periodic
tick into one `tokio::select!`. Delta-time is implicit (always ~33 ms).

**Opportunity:** termcompositor v2.0.0 ships `animation::{run,
run_with_config, run_with_stack, AnimConfig, AnimContext}` — a built-in
frame loop with delta-time tracking, terminal-resize handling, and
opt-in rendering. CLI flags `--animate` and `--fps <N>` are exposed by
the upstream binary.

**Refactor (evaluate before committing):**
- Audit whether cmdash's mixed input/PTY/config tick can be driven by
  `AnimContext`'s per-frame callback, or whether the input + PTY sides
  should stay on the existing `tokio::select!` and only the *render*
  phase should delegate to `animation::run_with_stack`.
- Most likely split: keep `tokio::select!` for input/PTY/close/reload,
  but replace the manual `CpuCompositor.compose` + `encode_*_to_writer`
  pair in phase 3b with `animation::run_with_stack` so the render side
  gets delta-time, automatic resize, and the dirty-region integration
  for free.
- Use `AnimConfig`'s fps knob to make the tick rate configurable
  (currently hardcoded to 33 ms / ~30 fps).
- Add `--fps <N>` and `--animate` CLI flags mirroring upstream.

**Risk:** the animation loop is built around a single `LayerStack`;
cmdash's session-persistence split (§3.5) puts the stack on the
frontend and the PTY driving on the server. The animation loop fits the
frontend side cleanly; the server side should keep its own tick.

### 5.3 Layer transforms (`Transform`)

**Status:** Not started.

**Opportunity:** v2.0.0 adds per-layer `Transform` with rotation,
scaling, anchor points, and bilinear interpolation, set via
`LayerEntry::set_transform(Some(t))` and applied by `CpuCompositor`
(dedicated `apply_transform_to_target` path with inverse mapping +
bilinear sampling).

**Refactor candidates:**
- **Focus transition animation:** when focus moves between panes, animate
  the newly-focused pane's image layers with a brief scale-up (e.g.
  1.0 → 1.02 → 1.0 over 150 ms) using `Transform::new().with_scale(s)
  .with_anchor(cx, cy)`. Requires the §5.2 animation loop for the
  per-frame interpolation.
- **Tab switch transition:** slide the outgoing tab's layers left/right
  via `Transform::with_translation` while the incoming tab's layers
  slide in from the opposite side.
- **Pane close animation:** fade + shrink the closed pane's image layers
  out before `close_pane` revokes the `LayerId`.
- **Widget zoom:** Alt+click on a widget could scale it up to fill the
  screen via a `Transform` on the widget's layer.

**Note:** `Transform` applies to a single `LayerEntry`; to animate a
whole pane (which may own multiple `ImageLayer`s from multiple kitty
image ids), wrap the pane's layers in a `SceneGraph` (§5.4) so one
transform cascades to all children.

### 5.4 New layer types: `SolidColor`, `GradientLayer`, `BorderLayer`, `CanvasLayer`, `DropShadow`, `SceneGraph`, `ClipLayer`

**Status:** Not started.

**Current state:** `GraphicsState` uses only `RectLayer`, `TextLayer`,
and `ImageLayer`. Pane borders, focus highlights, and the status bar
are rendered as ratatui text in phase 3a; the pixel overlay in phase 3b
only covers the tab bar.

**Refactor candidates, per layer type:**

- **`SolidColor`** — replace the tab bar background `RectLayer(0, 0,
  bar_w_px, ch, TAB_BAR_BG)` with `SolidColor::new(TAB_BAR_BG)`. A
  `SolidColor` fills the whole framebuffer, so this is only correct when
  the bar spans the full width (which it does today). Saves the
  per-pixel rect bounds check.

- **`GradientLayer` / `GradientLayerBuilder`** — use `new_linear()` /
  `new_radial()` builders for:
  - Tab bar background: a subtle horizontal gradient (active tab
    lighter, inactive tabs darker) instead of the flat `TAB_BAR_BG`.
  - Status bar background: a vertical gradient matching the theme.
  - Focus highlight: a radial gradient glow behind the focused pane's
    border.
  - The builder API (`new_linear().at(...).size(...).colors(...)`)
    replaces the deprecated `GradientLayer::linear()` / `radial()`
    constructors.

- **`BorderLayer`** — draw per-pane borders in the pixel layer
  (phase 3b) instead of relying on ratatui block borders in phase 3a.
  `BorderLayer::new(x, y, w, h, color).with_border_width(px)` gives
  pixel-precise border widths that survive the kitty/sixel round-trip;
  ratatui's cell-grid borders snap to cell boundaries and look jagged
  on fractional-DPI hosts. One `BorderLayer` per pane, z-order above
  the pane's `ImageLayer`s but below the tab bar.

- **`CanvasLayer`** — freeform `draw_pixel` / `draw_line` /
  `draw_circle` / `fill_rect` / `clear` methods. Candidates:
  - Custom widget decorations that don't fit the rect/text/image model.
  - Scrollbar indicators on the focused pane (a thin `draw_line` on the
    right edge showing scrollback position).
  - A minimap overlay showing the full scrollback buffer as a 1px-per-row
    column.

- **`DropShadow`** — `DropShadow::new(inner).with_offset(x, y)
  .with_blur(r).with_spread(p).with_glow(color, blur)`. Candidates:
  - Focused pane: a `DropShadow` wrapper around the pane's `BorderLayer`
    with a theme-colored glow so the focused pane reads as "lifted".
  - Floating widgets / ZStack overlays: a drop shadow behind the
    top-most ZStack member so it reads as floating above its siblings.
  - Modal dialogs (future): shadow behind a centered overlay.

- **`SceneGraph`** — parent-child tree with grouped transforms,
  cascading visibility/opacity/offset, and traversal methods
  (`parent()`, `children()`, `ancestors()`, `depth()`, `descendants()`,
  `move_to()` with cycle detection). Candidates:
  - Group all of a pane's `ImageLayer`s under one `SceneGraph` node so a
    single `set_opacity` / `set_transform` / `set_visible` cascades to
    every image the pane owns. Today `close_pane` walks
    `pane_images[pane]` and removes each layer individually; a
    `SceneGraph` would let one `stack.remove(pane_scene_id)` tear down
    the whole pane.
  - Group the tab bar's background + per-tab highlights + per-tab text
    under one `SceneGraph` so the whole tab bar can be toggled with one
    `set_visible` call.
  - Use `move_to()` for re-parenting a pane's scene when a ZStack
    member cycles to the top (§5.3 tab-switch transition).

- **`ClipLayer`** — `ClipLayer::new(inner)` clips inner layer rendering
  to a rectangular region. Candidates:
  - Clip each pane's `SceneGraph` to the pane's rect so a transformed
    (scaled/rotated) pane's pixels cannot bleed into a neighbour during
    a §5.3 focus animation. Today the only thing preventing bleed is
    that no transforms are applied; once transforms land, `ClipLayer` is
    the correctness guard.
  - Clip the status bar's layers to the status bar row so a wide
    gradient cannot overflow into the pane area.

### 5.5 Layer lookup by name (`find_by_name` / `find_by_name_mut`)

**Status:** Not started.

**Current state:** `GraphicsState` tracks the tab bar's `LayerId`s in a
`tab_bar_layers: Vec<LayerId>` field and rebuilds the whole vec every
frame in `update_tab_bar` (draining the old ids, pushing fresh layers).
The per-tab highlight and text layers get names like
`"tab_bar_tab_{idx}_bg"` and `"tab_bar_tab_{idx}_text"` but those names
are never read back.

**Opportunity:** v2.0.0 adds `LayerStack::find_by_name(name)` and
`find_by_name_mut(name)`, which return the first entry whose name
matches.

**Refactor:**
- Instead of draining + re-pushing every frame, push the tab bar layers
  once and mutate them in place via `find_by_name_mut`:
  - On a tab switch, `find_by_name_mut("tab_bar_tab_{old}_bg")` →
    `set_color`-equivalent (or remove + re-push just the two affected
    tabs' layers) and `find_by_name_mut("tab_bar_tab_{new}_bg")` →
    active color.
  - On a label change, `find_by_name_mut("tab_bar_tab_{idx}_text")` →
    update the `TextLayer`'s text.
- Pair with the `DirtyRegion` work in §5.1 so the in-place mutation
  marks only the affected tab's rect dirty, not the whole bar.
- This directly addresses the `TODO(v2): add a dirty flag` comment in
  `update_tab_bar`.

**Caveat:** `find_by_name` is O(n) over the stack; with ~5–7 tab bar
layers + N pane image layers this is fine. If pane counts grow large,
keep the `Vec<LayerId>` cache for the hot path and use `find_by_name`
only for the cold tab-bar mutation path.

### 5.6 Rounded corners (`RectLayer::with_border_radius`)

**Status:** Not started.

**Opportunity:** `RectLayer::with_border_radius(r)` clips the four
corners to circular arcs (radius clamped to `min(w, h) / 2`).

**Refactor candidates:**
- Tab bar tab highlights: round the active tab's highlight rect with a
  small radius (e.g. 4 px) so the active tab reads as a pill rather than
  a sharp rectangle.
- Widget panes: round the widget's background rect for a card-like
  appearance.
- Floating ZStack overlays: round the overlay's border + background for
  a dialog/window look.
- Status bar: optionally round the status bar's ends when it doesn't
  span the full width.

### 5.7 Accessibility metadata (`AccessibilityMetadata` / `SemanticRole`)

**Status:** Not started.

**Opportunity:** v2.0.0 lets any layer carry `AccessibilityMetadata`
with `alt_text` and a `SemanticRole` (`Text`, `Button`, `Image`,
`Container`, `Separator`, `Status`, `Navigation`, `Custom`).

**Refactor:**
- Tag the tab bar background as `SemanticRole::Navigation` with alt
  text "Tab bar: <n> tabs, tab <i> active".
- Tag each tab highlight as `SemanticRole::Button` with alt text
  "Tab <n>: <label>".
- Tag each pane's `SceneGraph` root (§5.4) as `SemanticRole::Container`
  with alt text "Pane <label>: <kind>".
- Tag the status bar as `SemanticRole::Status` with alt text echoing
  the rendered mode/label/clock.
- Tag image layers as `SemanticRole::Image` with alt text derived from
  the kitty graphics command's id.

**Payoff:** headless terminals and accessibility tools can convey the
screen layout without rendering the visual output. Low effort, high
accessibility value.

### 5.8 Tmux passthrough helpers (`wrap_for_tmux` / `wrap_for_tmux_to_writer` / `PassthroughWriter`)

**Status:** Not started.

**Current state:** cmdash emits kitty graphics via
`encode_passthrough_to_writer(&fb, writer)`, which wraps the APC-G
payload in the kitty passthrough envelope (`ESC P ... ESC \\`). When
cmdash runs *inside tmux*, tmux requires an additional
`ESC P tmux; <passthrough> ESC \\` wrapper around any passthrough
sequence it should forward to the outer terminal. cmdash does not
detect or apply this outer wrapper today; running cmdash inside tmux
produces garbled kitty graphics.

**Opportunity:** v2.0.0 exposes `wrap_for_tmux`,
`wrap_for_tmux_to_writer`, and a `PassthroughWriter` adapter that apply
the tmux passthrough envelope when `TMUX` is set in the environment.

**Refactor:**
- Detect `$TMUX` at startup (alongside the existing `TERM` /
  `TERM_PROGRAM` / `CMDASH_GRAPHICS` detection in
  `GraphicsProtocol::detect`).
- When tmux is detected, wrap the writer passed to
  `encode_passthrough_to_writer` in a `PassthroughWriter` (or call
  `wrap_for_tmux_to_writer` directly) so the kitty APC-G frames are
  double-wrapped for tmux forwarding.
- Add a `GraphicsProtocol::KittyInTmux` (or a separate `tmux_passthrough:
  bool` flag on `GraphicsState`) so the Sixel path can also be wrapped
  if needed (Sixel inside tmux has the same passthrough requirement).
- Add an integration test that sets `TMUX=1` and verifies the encoded
  output starts with the tmux passthrough prefix.

### 5.9 Unified dispatch (`dispatch_to_writer` / `detect` / `detect_with_probe`)

**Status:** Not started.

**Current state:** `GraphicsState::render_and_write` manually matches on
`self.caps.graphics` (`GraphicsProtocol::Kitty` →
`encode_passthrough_to_writer`, `Sixel` → `encode_sixel_to_writer`,
`TextOnly` → early-out). The protocol detection lives in cmdash's own
`GraphicsProtocol::detect_from_env` + `query_device_attributes`,
duplicating logic that termcompositor now ships.

**Opportunity:** v2.0.0 exposes `detect()` (auto-pick protocol from
`TERM`/`TERM_PROGRAM`) and `dispatch_to_writer(protocol, &fb, writer)`
(a single entry point that routes to the kitty or sixel encoder based on
the protocol), plus `detect_with_probe()` for a runtime DA1-style probe.

**Refactor (evaluate for partial adoption):**
- Replace the manual `match self.caps.graphics` in `render_and_write`
  with `dispatch_to_writer(self.caps.graphics.into(), &fb, writer)`. The
  `GraphicsProtocol → termcompositor::Protocol` conversion is a 3-arm
  `From` impl.
- Audit whether cmdash's `GraphicsProtocol::detect_from_env` +
  `query_device_attributes` can delegate to termcompositor's `detect()`
  / `detect_with_probe()`. cmdash's detection carries cmdash-specific
  overrides (`CMDASH_GRAPHICS` env var, `CMDASH_*` capability env vars)
  that upstream doesn't know about, so a full delegation is likely not
  possible — but the DA1 probe path could call `detect_with_probe()` to
  share the response-parsing code.
- Keep cmdash's `TermCapabilities` struct (it tracks more than just the
  graphics protocol: kitty keyboard, focus events, bracketed paste,
  true color, etc.) but have its `graphics` field derive from
  termcompositor's `detect()` where the cmdash-specific overrides don't
  apply.

### 5.10 Custom `Compositor` for focus effects

**Status:** Not started.

**Opportunity:** v2.0.0's `Compositor` trait (`fn compose(&self, stack,
target)`) lets cmdash plug in a custom compositor alongside the default
`CpuCompositor`. The default sorts visible entries by effective z-order
and calls each layer's `render` with its opacity.

**Refactor candidates:**
- **Focus dimming compositor:** render the focused pane's layers at
  full opacity and all other panes' layers at a reduced opacity (e.g.
  0.7) so the focused pane reads as brighter. This requires the
  `SceneGraph` grouping from §5.4 so the compositor can identify which
  scene each entry belongs to.
- **Double-buffer compositor:** a compositor that renders into two
  framebuffers and flips, so the encoder always reads a stable frame
  while the next frame is being composited. Pairs with the §5.1
  dirty-region work.
- **Concurrent compositor:** for very large framebuffers, a compositor
  that composites non-overlapping dirty regions in parallel via
  rayon/threads. The `Compositor` trait is `&self`, so a concurrent
  impl is feasible; the upstream `CpuCompositor` is single-threaded by
  design.

### 5.11 `render_to_current_terminal` for auto-sized rendering

**Status:** Not started.

**Current state:** `GraphicsState::render_and_write` sizes the
framebuffer from `self.cells` (set via `Self::new` / `set_cells`),
which the binary keeps in sync with crossterm `Event::Resize`. The
sizing logic lives in cmdash.

**Opportunity:** `LayerStack::render_to_current_terminal()` auto-detects
the terminal size via `TerminalSize::current()` and renders into a
framebuffer of that size, returning `(FrameBuffer, TerminalSize)`.

**Refactor (low priority):** cmdash's explicit `set_cells` path is
preferred because it stays in lock-step with the layout engine's
`relayout` (the `assert!(cells.0 > 0 && cells.1 > 0)` guard catches the
zero-area SIGWINCH transient before it reaches the compositor).
`render_to_current_terminal` would bypass that guard. Keep this as a
reference / fallback path only; do not replace the `set_cells` flow.

### 5.12 SVG layer (`SVGLayer`, `svg-renderer` feature)

**Status:** Not started.

**Opportunity:** v2.0.0 ships an opt-in `svg-renderer` feature (pulls in
`resvg`) that enables `SVGLayer` for rendering vector SVG content into
the framebuffer.

**Refactor candidates:**
- Native widget authors who want vector graphics (icons, charts,
  gauges) without bundling a raster image pipeline could emit SVG and
  let the compositor rasterize it.
- Script widgets (§2.2) could send SVG in their frame response instead
  of ANSI text, unlocking vector graphics for the script-widget
  protocol. This would be a v2 protocol extension (the v1 protocol is
  line+ANSI only per §2.2).
- Enable the `svg-renderer` feature in the workspace `Cargo.toml`
  `termcompositor` dep only when a config flag opts in (the `resvg`
  transitive dep is non-trivial).

## Testing priorities

- **Integration tests for tab operations** — ✅ Complete.
  `crates/cmdash/tests/tab_operations.rs` exercises TabNew/TabClose/
  TabSwitch through the full `TickContext` with real PTY children and
  verifies cross-tab `LayerId` contracts.
- **Widget loading test** — ✅ Complete. `widget_sdk_integration.rs`
  tests cdylib loading via libloading, CmdashWidget trait with MockWidget,
  FFI round-trip, zero-area handling, offset rendering, and object safety.
- **Script protocol round-trip** — ✅ Complete. `script_widget_integration.rs`
  tests spawn/lifecycle, frame protocol round-trip, event forwarding
  (KEY/RESIZE/FOCUS), repeated renders, and immediate-exit handling.
- **Config file loading test** — load from `~/.config/cmdash/`, verify
  overrides.
- **Sixel encoding smoke test** — verify the fallback path produces
  valid Sixel escapes.
- **Scrollback round-trip test** — verify scrolled content is preserved
  and navigable via PageUp/PageDown.
- **Stress test** — many panes (50+), verify no panics or layer leaks.
