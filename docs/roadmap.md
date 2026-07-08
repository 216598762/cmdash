# cmdash — Development Roadmap

This document outlines the planned work to bring cmdash from its current
state to a mature terminal multiplexer + dashboard. Items are grouped by
priority tier. Within each tier, items are roughly independent and can be
tackled in parallel.

## Tier 1: Core usability gaps

These are the things that prevent cmdash from being usable as a daily
driver terminal multiplexer.

### 1.1 Runtime config file loading

**Current state:** Config is embedded at compile time via
`include_str!("../config.kdl")`. Changing keybinds or layout requires
recompiling.

**Goal:** Load `~/.config/cmdash/config.kdl` at startup. Fall back to the
bundled default if the file is missing or unreadable. Support
`CMDASH_CONFIG_DIR` env var override.

**Steps:**
- Add `figment` as a dependency for layered config (file → env → CLI).
- Wire `cmdash::run` to read from the config dir path at startup.
- Add `--config=<path>` CLI flag for explicit override.
- Add `ReloadConfig` key action to hot-reload at runtime.

### 1.2 Tab bar rendering

**Current state:** `TabStack<TabState>` is implemented and tab actions
(`TabNew`, `TabClose`, `TabSwitch(n)`) are wired through
`TickContext`. But no tab bar is rendered — the user has no visual
indication of how many tabs exist or which is active.

**Goal:** Render a tab bar as its own layer at the top (or bottom) of the
screen. Show tab titles, highlight the active tab, support click-to-focus
(future).

**Steps:**
- Add a tab-bar render pass in `TickContext::run` (phase 3a, after pane
  rendering).
- Reserve 1 row at the top of the terminal for the tab bar; reduce the
  layout area by that amount.
- Render tab labels from `TabStack::iter()`, highlighting
  `active_idx()`.
- The tab bar is its own layer per the one-layer-per-instance rule.

### 1.3 Per-pane shell specification

**Current state:** All panes spawn `ShellSpec::LoginShell`. The
`ShellSpec::Command { argv }` variant exists but is only used in tests.

**Goal:** Allow the KDL config to specify per-pane shell commands.

**Steps:**
- Extend `cmdash_config::Pane` with an optional `command` field.
- Parse `pane kind=shell command="htop"` in `read_pane`.
- Wire `TickContext` to use the per-pane shell spec when spawning.
- Handle `AppNewPane` — new panes should inherit the focused pane's
  shell spec (or default to login shell).

### 1.4 Scrollback / alternate screen

**Current state:** `TextGrid` is a fixed-size grid with no scrollback
buffer. When content scrolls past the bottom, it's lost. This is a
daily-driver blocker — most terminal users expect scrollback as a
basic feature.

**Goal:** Add a scrollback buffer per pane with keyboard navigation.

**Steps:**
- Add a `scrollback: Vec<Vec<Cell>>` to `TextGrid` (or a ring buffer).
- On `scroll_up_one`, push the scrolled row into scrollback.
- Add `PageUp`/`PageDown` handling in the binary to view scrollback
  (with a visual indicator).
- Limit scrollback size (configurable, default 1000 lines).

### 1.5 Sixel fallback verification

**Current state:** The code path for Sixel encoding exists in
`GraphicsState::render_and_write` via `dashcompositor`, but it has never
been tested against a real Sixel-capable terminal.

**Goal:** Verify the Sixel fallback works when Kitty graphics is not
available.

**Steps:**
- Add terminal capability detection (query `TERM` + device attributes).
- Test against `xterm` with Sixel support and `mlterm`.
- Log the chosen protocol at startup.
- Ensure graceful degradation to text-only mode when neither is available.

## Tier 2: Extensibility

### 2.1 Native Rust widget SDK

**Current state:** `cmdash-widget-sdk` is a stub with only a module-level
doc comment.

**Goal:** Implement the `CmdashWidget` trait, the c-ABI export mechanism,
and runtime loading via `libloading`.

**Steps:**
- Define `CmdashWidget` trait in `cmdash-widget-sdk/src/lib.rs`.
- Define `WidgetEvent` enum (key, mouse, resize, focus).
- Define the C ABI: `#[no_mangle] pub extern "C" fn cmdash_widget_create()
  -> *mut dyn CmdashWidget`.
- Add a version constant for ABI compatibility checking.
- Implement the loader in `cmdash` binary: scan
  `~/.config/cmdash/widgets/<name>/` for `.so`/`.dll`, load via
  `libloading::Library::new`.
- Add `PaneKind::Widget { ref: String }` to `cmdash-config`.
- Wire widget panes into the render loop (phase 3a calls
  `widget.render(area, frame)`).
- Create an example widget (`examples/widget-clock/`) as a `cdylib`.

### 2.2 Script widget protocol

**Current state:** `cmdash-protocol` is a stub. AGENTS.md describes the
protocol shape but there is no implementation.

**Goal:** Implement the line-delimited frame protocol so any executable
can act as a widget.

**Steps:**
- Write the protocol spec in `crates/cmdash-protocol/README.md`.
- Implement frame parsing/serialization in `cmdash-protocol/src/lib.rs`.
- Define message types: `Frame`, `Key`, `Resize`, `Mouse` (cmdash →
  script) and `FrameReply` (script → cmdash).
- Add `PaneKind::Script { ref: String }` to `cmdash-config`.
- Implement the spawn path: `std::process::Command` with piped
  stdin/stdout.
- Wire into the render loop: send `FRAME` request, read reply, blit
  ANSI text into the pane's rect.
- Create an example script widget (`examples/script-hello/`).

### 2.3 Optional status bar

**Current state:** No status bar is rendered. The tab bar (item 1.2)
shows tab state but there's no general-purpose status line.

**Goal:** Add an optional status bar configurable via KDL. When
disabled (the default), no rows are reserved. When enabled, the status
bar renders at the bottom of the screen as its own layer.

**Steps:**
- Add a `status_bar { ... }` block to the KDL config schema:
```kdl
status_bar enabled=true position="bottom" show_clock=true show_pane_title=true show_mode=true
```

Or as a block for readability:

```kdl
status_bar {
    enabled=true
    position="bottom"  // or "top"
    show_clock=true
    show_pane_title=true
    show_mode=true
}
```
- Parse `status_bar` in `cmdash_config::parse` into a `StatusBar` struct.
- When enabled, reserve 1 row (top or bottom) and reduce the layout
  area accordingly.
- Render status bar content in `TickContext::run` (phase 3a, after
  pane + tab bar rendering). The status bar is its own layer per the
  one-layer-per-instance rule.
- Fields: active mode, focused pane title, clock, keybind hint.
- Default: disabled (zero config surface for users who don't want it).

### 2.4 Additional keybind modes

**Current state:** `Mode::Normal` is the only routed mode. `PaneResize`,
`TabSwitch`, `PresetPick` are enum stubs.

**Goal:** Implement the remaining modes.

**Steps:**
- `PaneResize`: enter on keybind, arrow keys resize the focused pane's
  split ratio, escape exits.
- `PresetPick`: enter on `M-p`, show a picker overlay, arrow keys
  navigate, enter selects.
- `TabSwitch`: enter on modifier+tab, cycle through tabs.
- Each mode has its own binding set in the `Router`.

## Tier 3: Polish and robustness

### 3.1 Async I/O migration

**Current state:** Each pane has a dedicated reader thread using
blocking `std::sync::mpsc`. The main tick loop is synchronous.

**Goal:** Migrate to `tokio` for non-blocking I/O and better scalability
with many panes.

**Steps:**
- Add `tokio` as a dependency.
- Replace reader threads with `tokio::task::spawn_blocking` or async
  reads.
- Replace `mpsc` with `tokio::sync::mpsc`.
- Keep the tick loop structure but make it async-friendly.

### 3.2 Mouse support

**Current state:** Mouse capture is enabled (`EnableMouseCapture`) but
mouse events are not routed to panes or used for focus/resize.

**Goal:** Support mouse-based pane focus, resize, and scroll.

**Steps:**
- Route `Event::Mouse` to the pane under the cursor for focus.
- Add drag-to-resize on split borders.
- Forward mouse events to the focused pane's PTY (for TUI apps that
  support mouse).
- Add scroll-wheel scrollback navigation.

### 3.3 Theme / color customization

**Current state:** No theming support. Colors come from the PTY child's
SGR sequences.

**Goal:** Allow users to define a color theme in KDL config.

**Steps:**
- Add a `theme { ... }` block to the KDL config schema.
- Define theme properties: default fg/bg, cursor style, border color.
- Apply theme colors in `blit_grid` as defaults.

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

**Current state:** Config parse errors are returned as `ConfigError`
variants with messages, but there's no validation of semantic
correctness (e.g., referencing a preset that doesn't exist, binding the
same chord twice).

**Goal:** Validate config at load time with clear error messages.

**Steps:**
- Check for duplicate chord bindings (warn, last-wins).
- Validate preset references in `pane.preset.<name>` against the
  `presets` map.
- Enforce binary `split` node's 2-child limit at parse time (the parser
  currently collects all children into a `Vec` without rejecting a
  3+ child split).
- Validate layout tree depth against `MAX_TREE_DEPTH`.
- Pretty-print errors with file:line context (when reading from file).

## Testing priorities

- **Integration tests for tab operations** — TabNew/TabClose/TabSwitch
  through the full `TickContext` with real PTY children.
- **Widget loading test** — load a test cdylib, verify render output.
- **Script protocol round-trip** — spawn a script, send frame, verify
  reply.
- **Config file loading test** — load from `~/.config/cmdash/`, verify
  overrides.
- **Sixel encoding smoke test** — verify the fallback path produces
  valid Sixel escapes.
- **Scrollback round-trip test** — verify scrolled content is preserved
  and navigable via PageUp/PageDown.
- **Stress test** — many panes (50+), verify no panics or layer leaks.
