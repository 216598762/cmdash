# Changelog

All notable changes to `cmdash` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Overview

cmdash is a Linux PTY-driven terminal multiplexer and dashboard
that renders text bodies with `ratatui` and composes every pane as a
per-instance layer through `dashcompositor` (Kitty graphics protocol
preferred, Sixel fallback).

The workspace has 7 crates:

- `cmdash` — binary entry point: event loop, render pipeline, runtime
  layout mutations, tab management.
- `cmdash-config` — KDL config surface (layout tree, keybinds, presets).
- `cmdash-keybinds` — modifier-aware key router with modes.
- `cmdash-layout` — layout tree engine (Split / Stack / ZStack / Pane /
  Preset) with deterministic PaneId stability.
- `cmdash-pty` — `portable-pty` + `vte` text grid with kitty-graphics
  interception (APC pre-scan state machine).
- `cmdash-widget-sdk` — c-ABI widget trait with `CmdashWidget` trait,
  `WidgetEvent` enum, `cmdash_widget_export!` macro, and runtime
  loading via `libloading`.
- `cmdash-protocol` — line-delimited script-widget frame protocol with
  `HostMsg` serialization, `FrameResponse` parsing, and `ScriptWidget`
  adapter implementing `CmdashWidget`.

### Added

- **PTY-driven terminal front end** (`cmdash-pty`): kitty-graphics
  protocol interception backed by `portable-pty` + `vte`, with a
  five-state APC pre-scan that routes `ESC _ G` payloads to a
  `KittyAccumulator` before the vte parser (which silently drops APC
  strings).
- **Layout engine** (`cmdash-layout`): Split / Stack / ZStack / Pane /
  Preset node kinds with deterministic `PaneId` (pre-order leaf index +
  child-index path). Max tree depth 8. Runtime mutation helpers
  (`replace_leaf_with_split`, `remove_leaf` with sibling absorption,
  `adjacent_pane` for directional focus).
- **Config surface** (`cmdash-config`): KDL parser using `kdl-rs`
  (chosen over `knus` for full spec coverage). Three top-level blocks:
  `layout`, `keybinds`, `presets`, `status_bar`. 28 `KeyAction`
  variants covering pane management, focus navigation, ZStack cycling,
  preset swapping, tab management, mode entry/exit, and app close.
- **Keybind router** (`cmdash-keybinds`): press-only dispatch with
  all four modes routed: `Normal` (global bindings), `PaneResize`
  (arrow keys for split-ratio adjustment), `TabSwitch` (number keys
  1–9), `PresetPick` (number keys for preset selection). Escape exits
  any non-Normal mode.
- **Render pipeline** (`cmdash` binary): per-frame tick loop with
  phase 0–3b architecture. ratatui text body + dashcompositor kitty
  graphics. `GraphicsState` owns the `LayerStack` with per-pane image
  maps and close-channel teardown.
- **Runtime layout mutations**: `AppNewPane` (split focused leaf),
  `PaneClose` (drop + rebalance), `PanePreset` (wholesale swap),
  directional focus (`PaneFocus{Up,Down,Left,Right}`), ZStack focus
  primitives (`PaneStack{Cycle,Down,Up,Left,Right}`).
- **Multi-pane reflow on host resize**: `TickContext::relayout`
  re-resolves the layout tree and per-pane calls `PaneRunner::resize`
  with the full rect (preserving Split-derived origins).
- **Tab management**: `TabStack<TabState>` with `TabNew`, `TabClose`,
  `TabSwitch(n)` actions. Tab bar rendered as ratatui text fallback
  (phase 3a) and dashcompositor pixel overlay (phase 3b).
- **CLI**: `--log=<path>` launch argument for file-based tracing
  (TRACE level forced in file mode; `RUST_LOG` honored in stdout mode).
- **Project docs**: `README.md`, `LICENSE` (MIT), `AGENTS.md`,
  `docs/configuration.md`, `docs/roadmap.md`.
- **Example configs**: `examples/01-minimal.kdl` through
  `examples/04-four-pane-tiled.kdl`.
- **Widget SDK** (`cmdash-widget-sdk`): c-ABI-safe `CmdashWidget`
  trait with `WidgetEvent` enum (Key, Resize, FocusGained, FocusLost),
  `cmdash_widget_export!` macro for C-ABI entry point generation,
  `widget_into_raw`/`widget_from_raw` for FFI, and ABI version pinning.
  Runtime loading via `libloading` from `~/.config/cmdash/widgets/`.
- **Script widget protocol** (`cmdash-protocol`): line-delimited wire
  format with `HostMsg` enum (Frame, Key, Resize, Mouse, Focus) and
  `FrameResponse` parsing. `ScriptWidget` adapter spawns child processes
  with piped stdin/stdout and implements `CmdashWidget`.
- **Status bar** (`cmdash` binary): optional single-row status bar
  configurable via KDL `status_bar { ... }` block. Shows keybind mode,
  focused pane title, and clock. Hot-reloadable.
- **Mouse support**: click-to-focus, Alt+drag split resize, scroll-wheel
  forwarding, SGR extended mouse sequence forwarding to focused pane's
  PTY.
- **Runtime config file loading**: config resolved via priority chain
  (`--config` → `$CMDASH_CONFIG_DIR` → XDG default → bundled fallback)
  with filesystem watcher for hot-reload.
- **Scrollback buffer**: ring-buffer scrollback in `TextGrid` with
  PageUp/PageDown navigation and `ESC[3J` clear.
- **Sixel fallback**: `GraphicsProtocol` enum with `detect()` from
  `TERM`/`TERM_PROGRAM`/`CMDASH_GRAPHICS` env vars and DA1 device-
  attributes query for runtime detection. Verified with unit tests.
- **GPG signing wrapper** for TTY-less hosts:
  `scripts/gpg-cmdash-wrapper.sh` + `just gpg-setup` recipe.

### Known limitations

- **One ignored test** in `cmdash-pty`: the cat-echo round-trip test
  is `#[ignore]`'d due to `portable-pty 0.9` not exposing
  `SlavePty::as_raw_fd()`. Will be resolved when `portable-pty` ships
  a compatible version.
- **Sixel manual testing pending.** Unit tests verify encoding; manual
  testing against real Sixel-capable terminals (xterm, mlterm, foot)
  is recommended.
