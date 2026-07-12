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
- **Phase 3b (pixel):** `graphics.update_tab_bar()` pushes dashcompositor
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

**Current state:** ✅ Implemented. `TextGrid` has a ring-buffer scrollback
(`VecDeque<Vec<Cell>>`, default capacity 1000 rows). `scroll_up_one`
captures the top row into scrollback before shifting. `PageUp` enters
scrollback mode; `PageDown` scrolls toward live view (only intercepted
when already in scrollback, otherwise forwarded to the PTY for pagers).
Any non-PageUp/PageDown key resets to live view. `ESC [3J` clears the
scrollback buffer; `ESC [2J` only clears the visible screen (xterm
semantics). `blit_grid` renders scrollback rows above the live grid
when the viewport offset is > 0.

**Remaining:**
- Alternate screen detection/toggle (DECSET/DECRST 1047/1049).
- Configurable scrollback capacity via KDL.

### 1.5 Sixel fallback verification

**Status:** ✅ Unit tests verified. Manual terminal testing pending.

**Current state:** `GraphicsProtocol` enum (Kitty/Sixel/TextOnly) with
`detect()` from `TERM`/`TERM_PROGRAM`/`CMDASH_GRAPHICS` env vars.
`render_and_write` dispatches to the kitty or sixel encoder based on
protocol, with TextOnly early-out. Startup logs the chosen protocol.
11 unit tests verify detection, encoding dispatch, and tab bar behavior.

**Remaining:**
- Manual testing against `xterm` with Sixel support and `mlterm`.

**Device Attributes (DA1) query:** `query_device_attributes()` sends
`ESC[c` to the terminal and parses the response for Sixel attribute
4. Uses `poll(2)` via `extern "C"` (no background thread, no stray
bytes consumed on timeout). Gated behind `is_terminal()` so it's
skipped in CI/non-TTY environments. Only runs when env-var detection
yields `TextOnly` (avoids startup delay for configured users).

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

**Current state:** No clipboard support. `Event::Paste` is redacted in
the event logger but not forwarded.

**Goal:** Support paste from system clipboard.

**Steps:**
- Forward `Event::Paste` content to the focused pane's PTY.
- Add a copy-mode (select text in a pane, copy to clipboard).
- Use `crossterm` or a clipboard crate for system integration.

### 3.5 Session persistence (detach/attach)

**Current state:** cmdash runs as a foreground process. When the
terminal closes or the SSH session drops, all panes are killed.

**Goal:** Support detach/attach like tmux, so a cmdash session survives
terminal disconnect and can be reattached later.

**Steps:**
- Implement a server mode: cmdash forks a background process that
  owns the PTY children and layout state.
- The foreground process connects to the server via a Unix domain
  socket (or named pipe on Windows).
- `cmdash attach <session>` reconnects to a running session.
- `cmdash detach` (or loss of the controlling terminal) disconnects
  the frontend without killing the server.
- This is architecturally significant — the `TickContext` render loop
  would need to split into a backend (PTY + state) and frontend
  (render + input) pair. Defer until the sync I/O architecture (3.1)
  is settled.

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
  PTY panes use Kitty encoding when `focused_flags != 0`.
- `drain_close_channel` prunes `pane_keyboard_flags` on close.
- `pop_host_keyboard_flags` called on `run()` exit.

**Known tech debt:**
- `collect_keyboard_enhancement_flags` is `pub` (not `pub(crate)`)
  because Rust treats the lib and binary as separate crates. Add a
  `# Crate-internal` doc note to signal this is not public API.
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
- Gate on host capability detection; fall back to legacy key encoding
  when the host does not support Kitty keyboard protocol.

### 4.2 Bracketed paste

**Status:** Not started.

**Goal:** Support bracketed paste (`CSI ? 2004 h`/`l`) passthrough.

**Steps:**
- Track per-pane bracketed-paste state from child PTY mode requests.
- Wrap pasted content in `ESC [ 200 ~` / `ESC [ 201 ~`.
- Forward the wrapped bytes to the focused pane's PTY.

### 4.3 Focus reporting

**Status:** Not started.

**Goal:** Report focus-in/focus-out events to child PTYs.

**Steps:**
- Track focus-reporting mode per pane (`CSI ? 1004 h`/`l`).
- Emit `CSI I` / `CSI O` on focus changes.
- Forward host focus events when cmdash itself gains/loses focus.

### 4.4 Hyperlinks (OSC 8)

**Status:** Not started.

**Goal:** Pass through OSC 8 hyperlink sequences.

**Steps:**
- Preserve OSC 8 escape sequences in `cmdash-pty` output.
- Route hyperlink metadata to dashcompositor text layers or passthrough.
- Maintain per-pane hyperlink ID namespaces.

### 4.5 OSC 52 clipboard integration

**Status:** Not started.

**Goal:** Allow child PTYs to read/write the system clipboard via OSC 52.

**Steps:**
- Intercept OSC 52 set/query sequences from child PTYs.
- Integrate with a clipboard crate or crossterm clipboard APIs.
- Implement a security policy (allow/deny read vs. write; per-pane opt-in).

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
- Map attributes to dashcompositor text styling.

### 4.8 True color / 24-bit color guarantees

**Status:** Working (via `ratatui`/`dashcompositor`).

**Goal:** Ensure 24-bit color is preserved end-to-end.

**Steps:**
- Audit color handling in the `vte` → dashcompositor path.
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
- Pass ligature hints to the dashcompositor font rasterizer.
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
- Route decoded images to dashcompositor image layers.
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
- Route them to the dashcompositor font rasterizer.
- Manage per-pane font glyph caches.

### 4.19 Overline / double underline

**Status:** Not started.

**Goal:** Support SGR 53/55/21 and `SGR 4:2`/`4:3` underline styles.

**Steps:**
- Extend the cell attribute model.
- Map styles to dashcompositor text rendering.
- Add tests for underline style round-trip.

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
