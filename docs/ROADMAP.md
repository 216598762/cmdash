# cmdash roadmap

This roadmap is intentionally staged so the rendering and session ownership contracts are validated before a large widget ecosystem is built.

## Phase 0 — Product decisions and skeleton

- [x] Prioritize terminal tabs for the first usable multiplexer model; defer pane splitting until the session/rendering contracts are stable.
- [x] Start with a single active workspace; defer multiple saved workspaces until the core runtime contracts are stable.
- [x] Choose the initial Rust stack: `crossterm`, `alacritty_terminal`, `portable-pty`, `ratatui` primitives behind the scene boundary, and `tokio`.
- [x] Require ANSI/VT text, cursor movement, Unicode cells, basic colors, alternate screen, keyboard input, and resize; degrade optional features and omit unsupported graphics without corrupting text/layout.
- [x] Use a versioned native plugin ABI with C-compatible host data, capability negotiation, and no Rust trait objects across the shared-library boundary.
- [x] Create the initial Cargo package with formatting and test commands.
- [x] Add CI and linting workflows for formatting, checks, Clippy, and tests.
- [x] Use TOML as the initial hand-authored configuration format.
- [ ] Define the stable plugin configuration schema and dynamic-plugin terminology.

**Exit criteria:** a documented package/plugin boundary and a small executable that enters/leaves raw mode safely, or a deliberate decision to defer raw mode until Phase 1.

## Phase 1 — Application shell and backend contract

- [x] Implement startup/shutdown with panic-safe terminal restoration.
- [x] Add backend capability detection, resize handling, and input collection.
- [x] Add bounded event batching with a maximum of 32 events per loop iteration.
- [x] Define `AppState`, commands, typed IDs, surfaces, scene primitives, and backend traits.
- [x] Render a static frame containing text, borders, and clipping through the backend contract.
- [x] Define backend-neutral focus state and overlay primitives with focus decoration.
- [x] Route Tab/Shift+Tab keyboard commands through `AppState` and cycle visible surfaces.
- [x] Compose visible surfaces and overlays in z-order with viewport and layer clipping.
- [x] Add initial unit tests for scene composition and backend submission.
- [x] Retain the previous frame and emit changed-cell diffs, including full redraws for the first frame and viewport changes.
- [x] Add explicit invalidation rectangles that force affected cells into the next frame diff.
- [x] Group contiguous changed cells into same-row terminal spans.
- [x] Merge adjacent spans only when their cell styles are compatible.
- [x] Cache the active terminal style across compatible runs to avoid redundant style sequences.
- [x] Track optimized, naive, and saved terminal bytes and report savings in the dashboard footer.
- [x] Handle Unicode display widths, wide-glyph continuation cells, clipping, and single-emission span output.
- [x] Add regression tests for composition, clipping, invalidation, diff suppression, span grouping, style caching, metrics, and Unicode widths.

**Exit criteria (met):** a static dashboard renders and updates without any terminal session feature enabled.

## Phase 2 — Modular widget runtime

- [x] Define the initial widget model, registry, and backend-independent widget runtime.
- [x] Parse and validate version-1 TOML widget-instance configuration.
- [x] Integrate widget registration and configuration-driven instances into the application shell, with the static dashboard retained as a fallback.
- [ ] Add a layout tree with leaf widgets, tab groups, and overlays; defer pane splits until the tab model is validated.
- [ ] Define the versioned plugin host contract and load a minimal external widget.
- [x] Add at least two non-terminal widgets, including text and a clock, exercising the same host-facing contract.
- [x] Start the application shell from a widget-only TOML configuration without enabling terminal sessions.
- [x] Load user-provided widget-only TOML configuration through `--config` / `-c`, with the embedded config as the no-argument fallback.
- [x] Add focus routing, widget health reporting, and widget lifecycle cleanup.

**Exit criteria:** users can run a useful dashboard with no PTY, shell, or terminal-emulation dependency in the active workspace.

The next Phase 2 slice is the layout tree: model leaf widgets, tab groups, and overlays as configuration-driven layout nodes before adding terminal sessions or dynamic plugins.

## Phase 3 — One isolated terminal session

- [ ] Spawn a child process through a PTY and route output into one emulator instance.
- [ ] Route keyboard, paste, resize, and basic mouse input to the focused session.
- [ ] Support normal/alternate screen, colors, cursor state, scrollback, and clean close.
- [ ] Make terminal functionality an optional widget/provider rather than a shell-wide assumption.
- [ ] Add PTY lifecycle and emulator behavior tests.

**Exit criteria:** a terminal widget behaves predictably inside any layout surface and can coexist with non-terminal widgets.

## Phase 4 — Tabs and retained session state

- [ ] Implement the initial tab model with one `Session` per terminal tab.
- [ ] Keep inactive sessions alive while excluding them from the visible scene.
- [ ] Clear/invalidate the old surface on focus or tab changes.
- [ ] Preserve per-session scrollback, modes, cursor, selection, and render cache across switches.
- [ ] Add regression tests for two sessions with identical terminal image IDs and independent output.

**Exit criteria:** switching sessions never leaks text, cursor state, or graphics between tabs.

## Phase 5 — Kitty graphics and full retained scene pipeline

- [ ] Verify the selected emulator/parser's Kitty graphics support and extension points.
- [ ] Implement `SessionGraphicsStore` with session-scoped resource and placement ownership.
- [ ] Convert graphics state into scene image layers with clipping and surface transforms.
- [ ] Submit Kitty graphics through the backend only for visible placements.
- [ ] Implement tab-switch invalidation and restore/replay behavior.
- [ ] Add captured-sequence conformance tests and the A/B image-ID collision test.
- [ ] Retain decoded graphics in memory for the lifetime of each live session; define optional limits and diagnostics for oversized or unsupported graphics.

**Exit criteria:** a Kitty image rendered in one tab is hidden, preserved, and restored independently when the user changes tabs.

## Phase 6 — Usable dashboard product

- [ ] Add configuration reload or a safe restart workflow.
- [ ] Add command palette, discoverable keybindings, and status/help UI.
- [ ] Add useful built-in widgets and per-widget configuration.
- [ ] Improve mouse support, selection/copy, URLs, and notifications as prioritized.
- [ ] Add logging/diagnostics that do not pollute the rendered terminal.

**Exit criteria:** a user can configure a dashboard, launch terminal sessions optionally, understand keybindings, and recover from widget/session failures.

## Phase 7 — Extensibility and hardening

- [ ] Stabilize the widget API, dynamic-plugin contract, and configuration schema.
- [ ] Add feature-gated protocol support such as sixel if demand warrants it.
- [ ] Add pane splitting only after tabs and session restoration are stable.
- [ ] Add fuzzing for escape/protocol parsing and stress tests for high-output sessions.
- [ ] Add resource quotas, crash diagnostics, upgrade/migration handling, and release packaging.

**Exit criteria:** documented extension points, repeatable builds/tests, and controlled behavior under malformed input and resource pressure.

## Decision log starters

| Topic | Provisional direction | Why it matters |
| --- | --- | --- |
| Workspace scope | One active workspace initially; saved workspaces later | Keeps persistence and lifecycle complexity out of the first runtime contract |
| Terminal backend | `crossterm` behind the cmdash backend boundary | Supplies raw mode, input, resize, and basic controls without owning retained composition |
| Terminal emulator | One `alacritty_terminal` instance plus a cmdash-owned graphics adapter per session | Preserves emulator and image state isolation while leaving a Kitty verification gate |
| Terminal capabilities | ANSI/VT text and core interaction required; graphics and enhanced input optional | Ensures unsupported terminal features degrade deliberately rather than corrupting output |
| Terminal session ownership | One emulator and graphics store for each terminal tab/session | Prevents state and image-ID cross-contamination |
| Rendering | Retained, backend-neutral scene composed into complete frames | Makes widgets modular and tab restoration deterministic |
| Widget extensibility | Versioned native plugin ABI with C-compatible host data | Makes external widgets a first-class design constraint without exposing Rust's unstable ABI |
| Graphics | Kitty first, capability-aware fallback; retain live-session graphics in memory initially | Matches the initial requirement while keeping restoration faithful and predictable |
| Async model | Coordinator/UI owner plus per-session I/O tasks | Keeps frame submission serialized while PTYs remain responsive |
| Configuration | TOML for the initial user-facing format | Readable for hand-authored layouts and widget settings |
| Initial multiplexer UX | Tabs first; panes later | Validates session isolation and restoration before expanding layout complexity |

Update this table as product decisions are made; do not let provisional choices silently become public API guarantees.
