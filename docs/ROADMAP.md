# cmdash roadmap

This roadmap is intentionally staged so the rendering and session ownership contracts are validated before a large widget ecosystem is built.

## Phase 0 — Product decisions and skeleton

- [x] Prioritize terminal tabs for the first usable multiplexer model; defer pane splitting until the session/rendering contracts are stable.
- [x] Start with a single active workspace; defer multiple saved workspaces until the core runtime contracts are stable.
- [x] Choose the initial Rust stack: `crossterm`, `alacritty_terminal`, `portable-pty`, `ratatui` primitives behind the scene boundary, and `tokio`.
- [x] Require ANSI/VT text, cursor movement, Unicode cells, basic colors, alternate screen, keyboard input, and resize; degrade optional features and omit unsupported graphics without corrupting text/layout.
- [x] Use a versioned plugin data contract with C-compatible host data, capability negotiation, and no Rust trait objects across an isolation boundary.
- [x] Create the initial Cargo package with formatting and test commands.
- [x] Add CI and linting workflows for formatting, checks, Clippy, and tests.
- [x] Use TOML as the initial hand-authored configuration format.
- [x] Define the stable plugin configuration schema and dynamic-plugin terminology.

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
- [x] Add a layout tree with leaf widgets, tab groups, and overlays; defer pane splits until the tab model is validated.
- [x] Define the versioned plugin host contract and exercise it with a minimal external-widget fixture; untrusted execution is now isolated through the opt-in Wasmtime host.
- [x] Add non-terminal widgets including text, a UTC clock, and system information, exercising the same host-facing contract.
- [x] Start the application shell from a widget-only TOML configuration without enabling terminal sessions.
- [x] Load user-provided widget-only TOML configuration through `--config` / `-c`, with the embedded config as the no-argument fallback.
- [x] Add focus routing, widget health reporting, and widget lifecycle cleanup.

**Exit criteria (met):** users can run a useful, layout-driven dashboard with no PTY, shell, or terminal-emulation dependency in the active workspace.

The next milestone is Phase 3: add one isolated terminal session as an optional widget/provider while preserving this layout and plugin boundary.

## Phase 3 — One isolated terminal session

- [x] Spawn a child process through `portable-pty` and route output into one `alacritty_terminal` instance.
- [x] Route focused keyboard, paste, mouse, and resize input to the terminal widget.
- [x] Cover normal/alternate screen, colors, cursor state, scrollback, and clean-close behavior.
- [x] Make terminal functionality an optional widget/provider rather than a shell-wide assumption.
- [x] Add PTY lifecycle, emulator output, input encoding, resize, and shutdown tests.

**Exit criteria (met):** a terminal widget behaves predictably inside any layout surface and can coexist with non-terminal widgets.

## Phase 4 — Tabs and retained session state

- [x] Implement the initial tab model with one retained terminal session per terminal-widget tab and Ctrl+PageUp/PageDown switching.
- [x] Keep inactive sessions alive while excluding them from the visible scene.
- [x] Clear/invalidate the old and new surface regions on tab changes.
- [x] Preserve per-session terminal emulator state, scrollback, modes, cursor, and retained graphics across switches.
- [x] Add regression tests for two sessions with independent output and identical terminal image IDs.

**Exit criteria (met):** switching sessions never leaks text, cursor state, or graphics between tabs.

## Phase 5 — Kitty graphics and full retained scene pipeline

- [x] Verify the selected emulator/parser's Kitty graphics extension point and add a cmdash-owned APC adapter because the emulator does not own a graphics store.
- [x] Implement `SessionGraphicsStore` with session-scoped resource and placement ownership.
- [x] Convert graphics state into retained scene image layers with clipping and surface transforms.
- [x] Submit Kitty graphics through the backend only for visible placements, using session-qualified terminal image IDs.
- [x] Implement tab-switch invalidation and graphics restore/replay behavior.
- [x] Add captured-sequence parser coverage and the A/B image-ID collision test.
- [x] Retain decoded graphics in memory for the lifetime of each live session with bounded resources and degraded health diagnostics.

**Exit criteria (met):** a Kitty image rendered in one tab is hidden, preserved, and restored independently when the user changes tabs.

## Phase 6 — Usable dashboard product

- [x] Add validated file-backed configuration reload with safe replacement and rejection of invalid updates.
- [x] Add a command palette, discoverable keybindings, and status/help UI.
- [x] Add useful built-in widgets and per-widget configuration.
- [x] Improve mouse focus/drag handling and terminal selection/copy through OSC 52.
- [x] Add URL-aware copy notifications and in-app status notifications.
- [x] Add in-app diagnostics that do not pollute the rendered terminal or PTY output.

**Exit criteria (met):** a user can configure a dashboard, launch terminal sessions optionally, understand keybindings, and recover from widget/session failures.

## Phase 7 — Extensibility and hardening

- [x] Define plugin manifest metadata with ABI, capability, widget-type, and version validation.
- [x] Stabilize the widget API, dynamic-plugin contract, and full configuration schema.
- [x] Add feature-gated protocol support such as sixel without changing the default build.
- [x] Add configuration-driven horizontal and vertical pane splitting after tabs and session restoration are stable.
- [x] Add bounded parser stress coverage for escape/protocol input.
- [x] Enforce graphics resource quotas and surface widget/session shutdown failures as diagnostics.
- [x] Add fuzzing targets and scheduled CI smoke runs, upgrade/migration rewrites with warnings, crash reproduction artifacts, and reproducible multi-target release packaging.
- [x] Select Wasmtime as the isolated plugin runtime with an import-free, capability-limited host foundation.
- [x] Add interactive pane focus, adjustable split ratios, and focused-pane lifecycle commands.
- [x] Improve the opt-in sixel path with bounded 16-color quantization.

**Exit criteria (met for the current contract):** documented extension points, repeatable builds/tests, and controlled behavior under malformed input and resource pressure.

## Phase 8 — Interactive pane evolution

This phase extends the current retained-tab and pane foundation into a user-mutable multiplexer layout without weakening session ownership.

- [x] Create a new pane from the focused terminal with an explicit horizontal or vertical split command.
- [x] Define pane creation policy for shell command, terminal size, session identity, and inherited widget settings.
- [x] Merge panes and close pane groups while shutting down only the sessions that are no longer referenced.
- [x] Persist the pane tree, split ratios, focus target, and tab membership across safe configuration reloads.
- [x] Add directional focus tests for nested splits, tabs, overlays, and zero-area edge cases.
- [x] Add lifecycle regressions proving pane creation, close, merge, tab switching, and application shutdown do not leak PTYs or graphics resources.

**Exit criteria (met):** users can create, focus, resize, merge, close, and restore independent terminal panes without cross-session state leakage.

## Phase 9 — Fuzzing, release, and graphics validation

This phase turns the current hardening foundations into repeatable validation and publishable feature variants.

- [x] Retain minimized fuzzing corpora for config migration, plugin manifests, Kitty APC input, and sixel encoding.
- [x] Increase scheduled fuzz budgets, triage crash artifacts, and publish reproducible parser regressions as unit tests.
- [x] Add upgrade-path tests for each configuration version and make migration warnings actionable with a safe rewrite command.
- [x] Validate release archives, checksums, binary sizes, and startup behavior on Linux x86_64, macOS ARM64, and Windows x86_64.
- [x] Publish separately tested default, `sixel`, and `wasm-plugins` release variants with capability/permission notes.
- [x] Integrate dashboard sixel images through retained `Scene` image layers and backend capability negotiation.

**Exit criteria (met):** malformed input, migration, release packaging, and optional graphics/runtime variants are continuously and reproducibly validated.

## Phase 10 — Configuration onboarding and reference documentation

This phase makes the configuration file a first-class user-facing product surface rather than only an embedded fallback.

- [x] Add a checked-in `config/default.toml` containing the smallest useful widget-only dashboard and documented comments for each option.
- [x] Define the default-config discovery order: explicit `--config` / `-c`, user config path, checked-in/example config, then embedded fallback.
- [x] Create `docs/CONFIGURATION.md` with the `cmdash.workspace` v1 schema, top-level options, widget fields, plugin manifests, layout nodes, split ratios, overlays, graphics limits, and feature-gated options.
- [x] Provide complete TOML examples for a dashboard-only workspace, terminal tabs, nested panes, overlays, plugin metadata, and safe reload.
- [x] Document validation errors, migration warnings, unsupported versions, environment variables such as `CMDASH_CRASH_DIR`, and default keyboard bindings.
- [x] Add schema/configuration tests that parse the checked-in default file, verify every documented option, and exercise invalid/recovery examples.
- [x] Add a command or help entry that points users from the runtime palette to the configuration reference.

**Exit criteria (met):** a new user can locate a working default configuration, understand every supported option, safely customize it, and recover from invalid edits.

## Decision log starters

| Topic | Provisional direction | Why it matters |
| --- | --- | --- |
| Workspace scope | One active workspace initially; saved workspaces later | Keeps persistence and lifecycle complexity out of the first runtime contract |
| Terminal backend | `crossterm` behind the cmdash backend boundary | Supplies raw mode, input, resize, and basic controls without owning retained composition |
| Terminal emulator | One `alacritty_terminal` instance plus a cmdash-owned graphics adapter per session | Preserves emulator and image state isolation while leaving a Kitty verification gate |
| Terminal capabilities | ANSI/VT text and core interaction required; graphics and enhanced input optional | Ensures unsupported terminal features degrade deliberately rather than corrupting output |
| Terminal session ownership | One emulator and graphics store for each terminal tab/session | Prevents state and image-ID cross-contamination |
| Rendering | Retained, backend-neutral scene composed into complete frames | Makes widgets modular and tab restoration deterministic |
| Widget extensibility | Versioned manifest plus opt-in Wasmtime host | Keeps untrusted widget execution isolated and avoids exposing Rust's unstable ABI or terminal handles |
| Graphics | Kitty first, optional dependency-free sixel adapter, capability-aware fallback | Matches the initial requirement while keeping restoration faithful and making sixel opt-in |
| Async model | Coordinator/UI owner plus per-session I/O tasks | Keeps frame submission serialized while PTYs remain responsive |
| Configuration | TOML with checked-in `config/default.toml` and `docs/CONFIGURATION.md` | Makes the embedded fallback discoverable while keeping schema evolution explicit |
| Default configuration discovery | Explicit CLI path, user config, example/default file, embedded fallback | Preserves safe startup while giving users an editable starting point |
| Initial multiplexer UX | Retained tabs plus interactive horizontal/vertical panes | Validates session isolation and restoration while keeping pane mutation command-driven |

Update this table as product decisions are made; do not let provisional choices silently become public API guarantees.
