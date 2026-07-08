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
- `cmdash-widget-sdk` — c-ABI widget trait (stub).
- `cmdash-protocol` — script-widget frame protocol (stub).

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
  `layout`, `keybinds`, `presets`. 15 `KeyAction` variants.
- **Keybind router** (`cmdash-keybinds`): press-only dispatch,
  `Normal` mode routed (other modes are stubs).
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
  `TabSwitch(n)` actions. Tab bar rendering is not yet implemented.
- **CLI**: `--log=<path>` launch argument for file-based tracing
  (TRACE level forced in file mode; `RUST_LOG` honored in stdout mode).
- **Project docs**: `README.md`, `LICENSE` (MIT), `AGENTS.md`,
  `docs/configuration.md`, `docs/roadmap.md`.
- **Example configs**: `examples/01-minimal.kdl` through
  `examples/04-four-pane-tiled.kdl`.
- **GPG signing wrapper** for TTY-less hosts:
  `scripts/gpg-cmdash-wrapper.sh` + `just gpg-setup` recipe.

### Known limitations

- **Config is compile-time embedded** (`include_str!`). Runtime config
  file loading (`~/.config/cmdash/config.kdl`) is not yet implemented.
- **One ignored test** in `cmdash-pty`: the cat-echo round-trip test
  is `#[ignore]`'d due to `portable-pty 0.9` not exposing
  `SlavePty::as_raw_fd()`. Will be resolved when `portable-pty` ships
  a compatible version.
- **Widget SDK and script protocol are stubs.** No native widget
  loading or script widget spawning is implemented.
- **Tab bar is not rendered.** Tab actions work but there's no visual
  indicator.
- **Only `Normal` mode is routed** in the keybind router. Other modes
  (`PaneResize`, `TabSwitch`, `PresetPick`) are enum stubs.
- **Sixel fallback is untested.** The code path exists but has not
  been verified against a real Sixel-capable terminal.
- **No scrollback buffer.** `TextGrid` is fixed-size; content that
  scrolls past the bottom is lost.
