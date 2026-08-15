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
- [x] Answer Kitty `a=q` capability queries and route protocol acknowledgements back
  through the child PTY rather than the outer terminal writer.
- [x] Support direct-transfer negotiation, quiet acknowledgements, chunked APC
  uploads, and `a=T` transmit-and-display placement semantics.
- [x] Add installed-`kitten` PTY fixtures covering detection, negotiation, chunked
  uploads, retained resources, and cursor-relative placement without requiring a
  running Kitty window.
- [x] Preserve changed, visible, and removed graphics submissions in frame diffs
  so backend adapters can restore current layers and clear stale placements.
- [x] Add a hybrid outer-terminal graphics policy: direct replay for compatible
  terminals and Kitty Unicode-placeholder replay for pane-safe composition.
- [x] Add capability hints and explicit `CMDASH_KITTY_GRAPHICS`/
  `CMDASH_KITTY_GRAPHICS_MODE` overrides, including controlled graphics disablement.
- [x] Keep placeholder uploads quiet, encode Kitty image IDs with the canonical
  combining-mark table, preserve z-index, and prevent replay from moving the
  parent cursor.

**Exit criteria (met for retained foundation):** a Kitty image is parsed, retained,
  session-isolated, diffed, and emitted through the available direct or
  Unicode-placeholder backend path without stale frame-layer state. Full outer
  terminal rendering and scroll/occlusion conformance remain tracked in the
  graphics compatibility program below.

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

## Phase 11 — Theming and inherited terminal palette

This phase replaces the current widget-specific hard-coded RGB colors with a
semantic theme layer that remains readable across terminal environments while
allowing users to customize the dashboard's appearance.

- [x] Define semantic color roles for surfaces, text, muted text, borders,
  focused borders, success/warning/error states, selections, overlays, and
  terminal defaults; widgets must consume roles rather than embed `Color::rgb`
  constants.
- [x] Add a versioned configuration schema for inherited/fallback themes and
  per-role color overrides, with validation for RGB, ANSI-indexed, and
  terminal-default color values.
- [x] Add a backend palette abstraction that can represent the parent terminal's
  default foreground/background and ANSI 16-color palette without coupling
  widgets to Crossterm escape sequences.
- [x] Make the default theme inherit the parent terminal palette through
  terminal-native reset and ANSI references, with a deterministic RGB fallback
  for fixed-color environments; avoid blocking startup on optional palette-query
  protocols.
- [x] Preserve explicit truecolor and configured theme values when available;
  degrade to inherited/default ANSI colors without corrupting layout, contrast,
  or terminal output.
- [x] Provide a consistent border-style catalog covering rounded, square,
  double-line, heavy, ASCII, and hidden styles, with predictable behavior for
  narrow and zero-area surfaces.
- [x] Separate border geometry, border color, and title/label content so themes
  can control static widget chrome independently without changing content
  geometry.
- [x] Add an explicit label policy such as `auto`, `always`, and `never`, allowing
  a widget to have no label without relying on an empty-string sentinel; define
  how hidden labels affect border geometry, padding, accessibility text, and
  plugin widgets.
- [x] Add extended static appearance options for foreground/background roles,
  border and focus accents, muted/disabled state, bold/dim attributes, padding,
  and per-widget semantic color overrides.
- [x] Define appearance precedence as inherited terminal palette, named theme,
  widget-type defaults, widget-instance overrides, and transient focus/health
  state, with reload-safe validation and diagnostics.
- [x] Add theme reload behavior, documentation, and regression coverage for
  inherited reset/ANSI colors, configured overrides, focused states, border
  variants, hidden labels, overlays, and terminal-session color isolation.

**Exit criteria (met):** widgets share a documented semantic theme API, a fresh
installation follows the parent terminal's palette by default through native
reset/ANSI references, explicit theme configuration overrides inherited values
predictably, widgets support consistent border and label policies including
no-label surfaces, and fixed-color environments receive a stable RGB fallback.

## Phase 12 — Animation, transitions, and dynamic widget options

This phase adds optional motion without making animation a requirement for
correctness, input handling, or terminal-session state. Animations must produce
ordinary retained scenes, remain bounded by the coordinator, and degrade to a
static frame on terminals or configurations that do not support them.

- [x] Define a retained animation model with timelines, keyframes, triggers,
  start/end values, cancellation, completion, and interruption semantics; keep
  animation state separate from PTY/emulator state.
- [x] Add a versioned animation configuration contract with per-widget and
  per-theme options for enabled state, duration, delay, easing, repeat count,
  direction, fill mode, and trigger events.
- [x] Support a deliberate initial set of effects: widget enter/exit, focus
  changes, border and label transitions, color/attribute interpolation, value
  changes, progress updates, loading indicators, spinners, pulses, overlays,
  tab switches, and pane creation/closure.
- [x] Extend border and label options for animated visibility, style changes,
  title placement, label reveal/hide behavior, and transition-specific colors;
  define how `label = never` or a hidden border interacts with animation and
  content geometry.
- [x] Add a wakeable animation scheduler that requests frames only while an
  animation is active, coalesces simultaneous updates, and preserves the
  existing event-driven PTY/input path without reintroducing fixed-rate output
  polling.
- [x] Add terminal cursor blinking for the focused, visible terminal pane: derive
  cursor visibility and shape from the session emulator, reset the blink phase
  on keyboard input, PTY output, focus changes, and cursor movement, and pause
  blinking for unfocused panes, hidden tabs, inactive sessions, and shutdown.
  Make the interval, enabled state, reduced-motion behavior, and static-cursor
  fallback configurable while ensuring the scheduler wakes only the active
  pane and never uses cursor blinking to drive PTY polling.
- [x] Define terminal-safe interpolation and fallback rules for ANSI palettes,
  truecolor, bold/dim attributes, glyph changes, and unsupported effects;
  alpha/transparency must never leak malformed escape sequences or corrupt
  neighboring surfaces.
- [x] Add global and per-widget motion controls, including pause/resume, a
  reduced-motion preference, an animation budget, maximum concurrent effects,
  and a static fallback for slow or overloaded terminals.
- [x] Expose animation capabilities to plugins through explicit manifest/API
  bits, bounded frame/keyframe counts, execution and memory quotas, and host
  ownership of scheduling; plugins must not spawn unbounded animation workers.
- [x] Define lifecycle behavior for hidden tabs, closed panes, failed widgets,
  reloads, and shutdown so animations cannot retain sessions, graphics, or
  worker threads after their owner disappears.
- [x] Add deterministic clock injection, golden scene tests, timing-independent
  transition tests, cancellation/restart coverage, performance benchmarks, and
  fuzz/config validation for malformed animation options.
- [x] Document animation presets and extended theme options with examples for
  dashboard status changes, terminal focus, overlays, pane transitions, and
  plugin widgets.

**Exit criteria (met):** animations are opt-in, bounded, interruptible, and
accessibility-aware; active motion wakes the UI without timer-based PTY polling;
all animated output remains clipped and session-isolated; and disabling motion
produces a visually coherent static dashboard. See [ANIMATION.md](ANIMATION.md)
for the implementation and user-facing contract.

## Phase 13 — Compositor API endpoints, options, and documentation

This phase adds a versioned, capability-aware local API for inspecting compositor
state, requesting rendered snapshots, and submitting safe application commands
without exposing raw terminal ownership or bypassing the UI coordinator.

### Goals and boundaries

- [x] Define a stable machine-readable API for local automation, companion tools,
  tests, and future dashboard clients.
- [x] Keep the API disabled by default and local-only unless explicitly enabled.
- [x] Route every mutation through the existing `AppState::dispatch(Command)` and
  UI/coordinator path; API workers must never mutate `AppState`, widgets,
  sessions, or `Compositor` directly.
- [x] Expose backend-neutral state and frame data rather than terminal escape
  sequences, raw PTY streams, or backend handles.
- [x] Define bounded, versioned wire types before making internal Rust structs
  part of a public contract.
- [x] Keep remote TCP access, arbitrary shell execution, raw PTY injection, and
  public-network control out of the initial phase.

### Endpoint contract

Define logical versioned endpoints independently of the initial transport.

Read-only endpoints should include:

- `GET /v1/health` — process health, uptime, API version, and coordinator status.
- `GET /v1/capabilities` — backend capabilities, optional features, and allowed
  API operations.
- `GET /v1/workspace` — workspace identity, active tab, focus target, and layout
  metadata.
- `GET /v1/surfaces` — surface/widget IDs, visibility, z-order, geometry, and
  focus state.
- `GET /v1/widgets` — widget kinds, health, and bounded status information.
- `GET /v1/compositor/frame` — a consistent frame snapshot containing viewport,
  generation, cells/styles, visible ownership, and bounded graphics metadata.
- `GET /v1/compositor/diff` — a bounded diff from a requested frame generation,
  or an explicit snapshot-required response when history is unavailable.
- `GET /v1/metrics` — output and compositor metrics.
- `GET /v1/diagnostics` — bounded application and widget diagnostics.

Mutation endpoints should initially include:

- `POST /v1/commands` — a versioned allowlist-backed subset of focus, tab, pane,
  surface, overlay, redraw, and reload commands.
- `POST /v1/reload` — request configuration reload through the existing
  validation/replacement path.
- `POST /v1/subscriptions` and `DELETE /v1/subscriptions/{id}` — manage bounded
  frame, state, diagnostic, and lifecycle notifications.

Use dedicated wire DTOs for these endpoints. Do not serialize internal
`Compositor`, `Scene`, `FrameDiff`, or live session structs directly; this keeps
private fields and implementation-specific enum layouts out of the public API.

### Transport and coordinator bridge

- [x] Choose and document a transport, with a Unix-domain socket as the preferred
  Linux-first implementation.
- [x] Define a bounded JSON request/response envelope, likely using `serde_json`,
  with API version, request ID, typed result, and typed error fields.
- [x] Add a transport abstraction that can later support Windows named pipes or
  an explicitly enabled loopback TCP adapter without changing endpoint semantics.
- [x] Add a bounded API request queue and response/event bridge owned by the UI
  coordinator.
- [x] Generate API snapshots at a defined point in the frame loop so related
  state and frame responses share a generation.
- [x] Add bounded frame history or an explicit snapshot-required fallback for
  diff requests.
- [x] Ensure client disconnects, full queues, and API listener failures cannot
  stall or terminate the dashboard.

The intended ownership flow is:

```text
API listener
    │
    ▼
bounded request queue
    │
    ▼
UI/coordinator event loop
    │
    ├── AppState::dispatch(...)
    ├── configuration reload
    ├── compositor snapshot generation
    └── response/event publication
```

### Configuration and CLI options

Add an optional `[api]` configuration section while keeping the existing
workspace schema compatible:

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
```

Plan and validate options for:

- enabled/disabled state;
- transport and socket or named-pipe path;
- explicit loopback bind address for future TCP support;
- read-only mode and allowed operation set;
- authentication mode/reference;
- maximum clients, request/response sizes, request timeout, and event queue
  depth;
- frame snapshot/history depth;
- whether graphics metadata is exposed.

Secrets must not be stored directly in TOML. Prefer protected token files,
environment-provided secrets, or OS-level socket permissions. Add documented
CLI overrides such as `--api`, `--api-disable`, `--api-read-only`, and
`--api-socket <path>` only where one-shot automation needs them; CLI precedence
must be explicit.

### Security and capability policy

- [x] Keep the listener disabled by default and create local sockets with
  restrictive permissions.
- [x] Validate socket paths and reject unsafe configurations where practical.
- [x] Make read-only operation the default and use explicit mutation allowlists.
- [x] Reject arbitrary shell execution, raw PTY input, raw terminal escape output,
  clipboard contents, and unbounded graphics payloads by default.
- [x] Enforce request, response, client, queue, timeout, subscription, and frame
  history limits.
- [x] Advertise supported and enabled operations through `/v1/capabilities`.
- [x] Require explicit binding and authentication before any future TCP support.

### Documentation deliverables

- [x] Add `docs/API.md` with the API versioning policy, transport setup,
  endpoint reference, request/response schemas, error envelopes, permissions,
  limits, examples, subscriptions, and compatibility rules.
- [x] Update `docs/ARCHITECTURE.md` with the API-to-coordinator ownership
  boundary and frame-generation model.
- [x] Update `docs/CONFIGURATION.md` with `[api]` options, CLI overrides,
  security defaults, and recovery behavior.
- [x] Update `docs/DEPENDENCIES.md` with the selected serialization and transport
  dependencies and their boundary rationale.
- [x] Update `README.md` with automation setup, local-socket troubleshooting,
  and read-only/mutating deployment guidance.

### Testing and validation

- [x] Add wire serialization round-trip tests, unknown-version tests, malformed
  request tests, and invalid-command tests.
- [x] Test read-only mode, authorization failures, capability negotiation, and
  unsupported endpoint behavior.
- [x] Test request/response limits, full queues, timeouts, disconnected clients,
  and listener shutdown.
- [x] Prove API mutations execute only through the coordinator and existing
  command/state validation.
- [x] Verify frame snapshots contain only visible surfaces and retain
  session-qualified graphics ownership.
- [x] Test state/frame generation consistency and diff fallback behavior.
- [x] Test configuration reload, CLI precedence, socket permissions, and unsafe
  path rejection.
- [x] Add fuzz coverage for API envelopes, command payloads, oversized messages,
  and subscription requests.

**Exit criteria (met):** the API is disabled by default and safely enabled through
documented options; local clients can query health, capabilities, workspace state,
surfaces, widgets, diagnostics, metrics, and compositor frames; safe focus/tab/
pane/reload commands can be submitted; all mutations remain coordinator-owned;
responses are versioned, bounded, and generation-consistent; API failures cannot
crash or stall the dashboard; and endpoint schemas, security behavior, examples,
and compatibility rules are documented and tested in [API.md](API.md).

## Graphics compatibility program — protocol-faithful multiplexer architecture

**Status:** foundation complete; the larger architecture remains in progress.

The initial retained graphics implementation proved that cmdash can parse Kitty
APCs, answer `kitten icat` negotiation, retain image data, and submit a backend
command. It did not prove that the outer terminal rendered the image. A child-side
`ESC_G...OK` response only confirms that cmdash answered the inner PTY; it says
nothing about outer-terminal capability, pane coordinate translation, clipping,
scroll anchoring, z-order, or placeholder lifetime. A backend that silently drops
unsupported graphics also makes this failure look like a successful no-op.

### Findings from comparable implementations

- Kitty's own `icat` documentation warns that graphics may not work inside a
  multiplexer, explains that `icat` communicates directly with the TTY, and
  recommends Unicode placeholders or explicit passthrough when integrating with
  a complex host.
- tmux's passthrough model wraps application output in a DCS sequence and requires
  an explicit `allow-passthrough` policy. tmux primarily forwards the protocol;
  it does not become the image renderer or infer arbitrary pane placement.
- Kitty's Unicode-placeholder model separates resource upload from visible cell
  placement. The outer terminal owns the image while ordinary cell composition
  determines where placeholders move.
- Ghostty advertises Kitty graphics and supports the Unicode-placeholder path used
  to make graphics work through multiplexers. Current ecosystem compatibility
  matrices report WezTerm and iTerm2 primarily through inline-image protocols and
  Zellij primarily through Sixel, so capability names must not be treated as a
  universal protocol guarantee.

Reference material:
[Kitty `icat` documentation](https://sw.kovidgoyal.net/kitty/kittens/icat/),
[Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/),
[tmux passthrough option](https://man7.org/linux/man-pages/man1/tmux.1.html),
[Ghostty features](https://ghostty.org/docs/features), and
[Yazi's terminal/multiplexer compatibility matrix](https://yazi-rs.github.io/docs/image-preview/).

### Completed foundation

- [x] Keep Kitty parsing and child-PTY responses inside the session boundary.
- [x] Answer `a=q` and DA1 negotiation in the correct PTY direction.
- [x] Support bounded direct-transfer and chunked APC upload handling.
- [x] Retain session-qualified resources and image payloads through `Scene`,
  `Compositor`, and visible-frame submission.
- [x] Carry changed, currently visible, and removed image submissions through
  `FrameDiff`.
- [x] Add direct replay and Unicode-placeholder backend paths with capability
  hints, explicit mode overrides, quiet uploads, z-index, and cursor-static output.
- [x] Add no-Kitty and installed-`kitten` PTY fixtures plus backend/compositor
  regression tests.
- [x] Add typed direct/placeholder/disabled mode selection and expose the
  selected mode through backend capability metadata.
- [x] Distinguish rendered, degraded, suppressed, and failed graphics outcomes;
  record bounded diagnostics instead of treating an outer no-op as success.
- [x] Support multiple placements and placement-ID replacement in the retained
  session graphics store.
- [x] Recover from malformed or unsupported child graphics commands by retaining
  a diagnostic and returning a bounded Kitty error acknowledgement when IDs are
  available.

### Architecture goals and invariants

- [x] A graphics command must end in one explicit state: rendered, intentionally
  suppressed with a reason, degraded to a fallback, or failed with a bounded
  diagnostic. `Ok(())` must never mean an image was silently discarded.
- [ ] Child-terminal protocol state, logical graphics state, composed scene state,
  and outer-terminal serialization must be separate interfaces.
- [x] Image resources, placements, virtual placeholders, and backend image IDs
  have distinct identities and lifetimes in the retained store/backend boundary;
  virtual-placeholder ownership and lifecycle integration remain future work.
- [ ] Pane-local coordinates must never be confused with outer-terminal absolute
  coordinates; all projections must carry the owning surface and clip rectangle.
- [ ] Hidden tabs, overlays, pane movement, resize, scrollback, alternate-screen
  transitions, reload, close, and shutdown must have defined graphics behavior.
- [x] Unsupported outer-terminal protocols are visible through capability state
  and diagnostics, not inferred from an apparently successful child query.
- [x] Protocol handling remains bounded: payloads, chunk accumulation, resource
  counts, placements, placeholder cells, and diagnostic history have explicit
  limits; retry policy remains future work.

### Workstream 1 — Capability and mode contract

- [x] Add a typed capability result with graphics mode, capability source,
  confidence, and placeholder support metadata; terminal-name hints are now
  explicitly distinguishable from active confirmation.
- [ ] Define the complete stable mode set: `disabled`, `direct`,
  `unicode_placeholder`, `passthrough`, and `fallback`. The current runtime
  implements the first three and documents the remaining adapter boundary.
- [x] Add a bounded active outer-terminal probe containing Kitty graphics, DA1,
  and pixel-size queries, with response correlation and timeout/rejection
  outcomes. Automatic outer-input demultiplexing is still required to wire this
  into every interactive input path.
- [x] Make `CMDASH_KITTY_GRAPHICS` and
  `CMDASH_KITTY_GRAPHICS_MODE=placeholder|direct|passthrough|off` overrides
  explicit and precedence-ordered.
- [x] Expose selected mode, capability source, and confidence through backend/API
  capability metadata; last outer diagnostic publication remains follow-up work.

### Workstream 2 — Protocol adapter and response broker

- [ ] Introduce a `GraphicsProtocolAdapter` that parses Kitty APC, C1 APC where
  applicable, and tmux-style DCS passthrough wrappers without mixing parsing with
  resource storage or backend output.
- [ ] Support protocol fields needed for conformance: compression, source crops,
  pixel dimensions, natural PNG dimensions, placement IDs, `C` cursor policy,
  z-index, delete selectors, frame/animation actions, and all bounded transfer
  modes.
- [ ] Preserve exact child-output ordering across text, graphics, DA1, pixel-size,
  and graphics acknowledgements.
- [x] Add a bounded response broker with separate destinations for child PTY
  responses and outer-terminal responses. Never write an outer response into a
  child session or vice versa.
- [x] Add a bounded outer-input demultiplexer that preserves keyboard/CSI input,
  handles split probe responses, and routes only graphics replies to the probe.
- [ ] Replace the current crossterm-reader integration point with a process-wide
  raw-input owner so the demultiplexer is fed automatically without competing
  for stdin.
- [ ] Define unsupported-transfer behavior so `t=f`/`t=s` negotiation reliably
  falls back to direct stream mode without claiming that an image was displayed.
- [ ] Add malformed-sequence recovery and cancellation so one bad graphics command
  cannot fail an otherwise healthy terminal widget.

### Workstream 3 — Logical graphics state and geometry

- [x] Replace the one-placement-per-image map with separate resource and placement
  registries supporting multiple placements, placement IDs, replacement, delete,
  and resource lifetime rules from the Kitty protocol.
- [x] Store logical emulator-grid anchors, captured scrollback depth, cursor
  position, pixel dimensions, cell dimensions, z-index, and owning session rather
  than only absolute `u16` screen coordinates.
- [x] Track primary/alternate screen ownership, scrollback-relative movement,
  resize clipping, and replay resource generations; placements no longer leak
  between alternate and primary screens.
- [x] Track DECSTBM margins and cursor movement through a session-owned VT
  observer; partial-region linefeeds, explicit scrolls, reverse index, origin
  mode, alternate-screen state, and resize resets now move matching image
  anchors without using primary-screen scrollback for non-default regions.
- [ ] Preserve natural image geometry when pixel-size ioctl data is unavailable;
  use CSI 14t/16t or a documented fallback rather than shrinking an image to a
  misleading `1x1` placement.
- [ ] Separate session image IDs from outer-terminal IDs and maintain a replay
  generation/acknowledgement state for each outer resource.

### Workstream 4 — Scene and compositor integration

- [ ] Make image/placeholder primitives first-class scene data with ownership,
  clipping, occlusion, and z-order semantics; backend emission must not bypass
  overlay and surface composition.
- [ ] Represent placeholder graphemes/combining marks as a backend-neutral
  primitive or validated cell cluster rather than writing invisible text directly
  after the frame.
- [ ] Diff old/current visible graphics, placeholder regions, and resource uploads
  independently. Clear stale placeholders before text restoration and reapply only
  the visible, non-occluded result.
- [ ] Define ordering for overlays, negative/positive image z-index, cell
  backgrounds, text, and multiple overlapping images.
- [ ] Ensure zero-area, clipped, hidden, tab-switched, and pane-moved surfaces
  cannot emit graphics outside their assigned scene.

### Workstream 5 — Outer-terminal adapters

- [ ] Implement a direct Kitty adapter for a compatible root or explicitly opted-in
  outer terminal, including resource reuse, delete, placement, and acknowledgements.
- [ ] Implement a Unicode-placeholder adapter for pane-safe rendering: quiet
  resource upload, virtual placement creation, canonical ID encoding, placeholder
  cell emission, stale-cell clearing, and redraw recovery.
- [x] Implement the bounded tmux-style passthrough serializer with ESC
  doubling/undoubling-compatible wrapping and outer-response routing boundaries.
- [x] Add a bounded textual fallback with an explicit `Degraded` outcome when
  Kitty graphics are unavailable.
- [ ] Add protocol adapters for other supported outer paths only after capability
  and ownership semantics are defined; do not label WezTerm/iTerm2/Sixel support
  as Kitty support without a conformance result.
- [ ] Provide deliberate fallbacks such as a textual/placeholder diagnostic or
  configured Sixel/inline-image path, with no silent success.

### Workstream 6 — Lifecycle, performance, and security

- [ ] Define upload/replay behavior for pane creation, movement, resize, tab
  switching, hidden sessions, overlays, reload, close, and application shutdown.
- [x] Add replay generations, unchanged-resource reuse, store cancellation on
  session shutdown/delete-all, and outer-resource cleanup when the backend leaves.
- [x] Add acknowledgement-driven outer-resource garbage collection: retain
  generation state after removal, wait for the upload acknowledgement before
  sending a delete, and retire the resource only after the delete acknowledgement.
- [ ] Add bounded retries for missing or failed outer acknowledgements.
- [ ] Keep file/shared-memory transfers opt-in and sandboxed; never read arbitrary
  paths or shared-memory names merely because an inner application requested them.
- [x] Add output metrics for graphics uploads, resource reuse, payload bytes, and
  suppressed/degraded placements.
- [x] Add outer acknowledgement, acknowledgement-failure, and garbage-collection
  metrics; parsed-command latency remains future work.
- [ ] Bound placeholder output and avoid re-uploading unchanged resources on every
  frame; preserve UI responsiveness during large images and rapid pane switches.

### Workstream 7 — Conformance and regression matrix

- [ ] Add protocol golden tests for every supported action/field, chunk boundary,
  compression mode, transfer negotiation, response ordering, delete operation,
  source crop, placement ID, z-index, and cursor policy.
- [ ] Add deterministic scene/compositor tests for panes, overlays, clipping,
  scrolling, resize, tabs, hidden sessions, multiple placements, and resource
  collisions.
- [ ] Add PTY fixtures using installed `kitten icat` for detection, image upload,
  `--place`, `--unicode-placeholder`, passthrough, animation, and failure paths.
- [x] Add deterministic captured outer-terminal byte-stream fixtures for direct
  upload, placement-only resource reuse, deletion, Unicode placeholders,
  tmux-style passthrough escaping, and textual fallback.
- [x] Add a bounded headless Kitty stream model that unwraps passthrough, parses
  APC/CSI/SGR output, reassembles chunks, and semantically validates resources,
  placements, placement-ID replacement, z-order, deletion, placeholder
  references, malformed sequences, and bounded input rejection.
- [ ] Add a headless or capture-based outer-terminal harness that verifies the
  emitted stream is accepted by Kitty; add Ghostty/WezTerm/Sixel/inline-image
  cases only where the advertised capability is verified.
- [ ] Assert both sides of every test: child receives the expected response and
  outer adapter reports/render-states the expected resource and placement.
- [ ] Add failure tests proving unsupported capability, timeout, malformed payload,
  quota rejection, and outer write failure become visible diagnostics rather than
  empty successful frames.
- [ ] Add performance tests for large/chunked images, rapid pane switching,
  placeholder redraws, and bounded memory/resource cleanup.

**Exit criteria:** graphics support is protocol-faithful and capability-explicit;
`kitten icat` either produces a verified outer-terminal image or a visible,
explainable fallback; pane-local images remain correct through scroll, resize,
movement, overlays, tabs, and lifecycle changes; unsupported modes never report
silent success; and every advertised outer-terminal mode has capture-based or
interactive conformance coverage.

## Phase 14 — Built-in widget catalog and widget authoring guide

This phase expands the default dashboard widget catalog and makes widget creation
approachable without weakening the scene, lifecycle, configuration, or plugin
boundaries.

### Goals and boundaries

- [ ] Establish a coherent catalog of useful built-in dashboard widgets.
- [ ] Keep built-ins optional, composable, capability-aware, and usable without
  terminal sessions.
- [ ] Reuse common rendering, appearance, layout, and bounded-data helpers
  instead of duplicating widget-specific behavior.
- [ ] Define stable widget type names, defaults, settings, failure behavior, and
  compatibility expectations.
- [ ] Provide a complete guide for creating, testing, registering, and
  distributing custom widgets.
- [ ] Keep custom widgets behind the existing `Widget`/factory/context boundary.
- [ ] Ensure widget authors never need direct access to terminal output, PTYs,
  compositor internals, or global mutable state.

### Built-in widget catalog

Start with dependency-light widgets that exercise the existing contracts:

- [ ] Add a `status` widget for semantic success, warning, error, and neutral
  states.
- [ ] Add a `key_value` widget for bounded labeled values and diagnostics.
- [ ] Add a `gauge` widget for bounded progress or utilization displays with a
  textual fallback.
- [ ] Add a clipped `list` widget for bounded static or scrollable rows.
- [ ] Add a bounded recent-message `log` widget with severity styling.
- [ ] Add a `sparkline` widget for compact historical values with a scene-safe
  glyph and textual fallback.
- [ ] Add a `separator` or `spacer` widget for intentional layout grouping
  without requiring a fake text widget.
- [ ] Extend `system` only where the data source and refresh behavior are
  portable and well-defined.

Each widget must define stable TOML type and field names, defaults, minimum useful
geometry, update/redraw behavior, theme-role usage, focus/input behavior, bounded
data policy, degraded/failed health states, and lifecycle behavior.

Host metrics, network data, process inspection, filesystem watching, and arbitrary
command execution must not be added implicitly. Each requires a separate provider
and explicit security/dependency decisions.

### Shared widget infrastructure

- [ ] Add reusable helpers for bounded text and row rendering, status/severity
  styling, clipping, minimum-size handling, and deterministic test data.
- [ ] Define how data-backed widgets request wakeups or periodic updates, ensuring
  hidden or inactive widgets do not create unnecessary work.
- [ ] Keep data providers separate from rendering so providers can be tested
  without an interactive terminal.
- [ ] Preserve the existing semantic theme, animation, graphics, and scene
  contracts for all new widgets.

### Widget authoring documentation

- [ ] Create `docs/CREATING_WIDGETS.md` as the focused development guide, while
  keeping `docs/WIDGETS.md` primarily as the user-facing catalog and runtime
  reference.
- [ ] Document how to choose between a built-in, in-process custom widget, and
  plugin widget.
- [ ] Document the `Widget` lifecycle, factory contract, runtime context, and
  `WidgetInstanceConfig`/`settings` behavior.
- [ ] Explain scene rendering, clipping, geometry, Unicode widths, borders,
  labels, theme roles, focus, input, resize, graphics, and animation.
- [ ] Document `Unchanged` versus `Redraw`, health reporting, diagnostics,
  failure isolation, background work, wakeups, cancellation, and shutdown.
- [ ] Provide a complete minimal custom-widget example and a data-backed example.
- [ ] Document factory registration, configuration examples, testing strategy,
  plugin/WASM restrictions, and troubleshooting for invisible or invalid widgets.

### Documentation updates

- [ ] Expand `docs/WIDGETS.md` with the built-in catalog and link to the authoring
  guide, moving implementation tutorials into `CREATING_WIDGETS.md`.
- [ ] Update `docs/CONFIGURATION.md` with every new built-in type, setting,
  default, and settings namespace.
- [ ] Update `docs/ARCHITECTURE.md` with shared widget helpers, provider/render
  separation, scheduling, and lifecycle ownership.
- [ ] Update `README.md` with links to the widget catalog and authoring guide.
- [ ] Update `docs/DEPENDENCIES.md` if a new metrics or data-provider dependency
  is selected, including its capability and portability rationale.

### Testing and validation

- [ ] Add configuration tests for every built-in type, default, invalid setting,
  and minimum-size case.
- [ ] Add rendering/golden tests for normal, focused, empty, clipped, narrow, and
  zero-area surfaces, proving no widget draws outside its assigned scene.
- [ ] Add deterministic update tests, redraw-coalescing tests, and hidden-widget
  scheduling tests.
- [ ] Add health and failure-isolation tests for malformed or unavailable data.
- [ ] Add reload, removal, pane-closure, and shutdown lifecycle tests.
- [ ] Add theme, border, label, animation, reduced-motion, and optional-feature
  compatibility tests.
- [ ] Add an example custom-widget test that follows `CREATING_WIDGETS.md`.
- [ ] Add plugin capability and configuration tests wherever the authoring guide
  references the plugin path.
- [ ] Validate documentation links and configuration examples in CI where
  practical.

**Exit criteria:** cmdash ships with a documented, stable set of useful built-in
widgets; every widget has bounded rendering, update, health, and lifecycle
behavior; a new author can create and test a custom widget by following
`docs/CREATING_WIDGETS.md`; user-facing widget documentation is separated from
implementation guidance; and no widget bypasses the scene, coordinator, theme,
or plugin boundaries.

### Non-goals

- A third-party widget marketplace.
- Arbitrary shell-command execution as a built-in widget.
- Unbounded network or filesystem polling.
- Completing the full WASM host-function ABI.
- Promising cross-platform system metrics before provider behavior is defined.
- Moving application commands or layout containers into ordinary dashboard widgets.

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
| Graphics | Session-owned Kitty adapter with direct replay, Unicode-placeholder mode, typed active probing, a child/outer response broker, and scroll-aware grid anchors | Keeps child protocol handling isolated, makes outer capability evidence explicit, and lets placements follow primary-screen content; scroll-region and automatic outer-input routing remain follow-up work |
| Async model | Coordinator/UI owner plus per-session I/O tasks | Keeps frame submission serialized while PTYs remain responsive |
| Configuration | TOML with checked-in `config/default.toml` and `docs/CONFIGURATION.md` | Makes the embedded fallback discoverable while keeping schema evolution explicit |
| Default configuration discovery | Explicit CLI path, user config, example/default file, embedded fallback | Preserves safe startup while giving users an editable starting point |
| Default widget palette | Use terminal-native reset/ANSI references, with a deterministic RGB fallback | Makes cmdash blend into the user's terminal without blocking on optional palette-query protocols |
| Widget chrome | Explicit border-style and label policies, including a first-class no-label mode | Keeps layout/content geometry independent from decorative labels and allows themes to control borders consistently |
| Animation model | Optional retained transitions scheduled by the UI coordinator with bounded budgets | Adds motion without compromising PTY responsiveness, deterministic rendering, or plugin isolation |
| Active terminal cursor | Blink only the focused visible terminal pane through the wakeable scheduler, with reduced-motion and static-cursor fallbacks | Provides familiar terminal behavior without waking hidden sessions or reintroducing timer-based PTY polling |
| Initial multiplexer UX | Retained tabs plus interactive horizontal/vertical panes | Validates session isolation and restoration while keeping pane mutation command-driven |

Update this table as product decisions are made; do not let provisional choices silently become public API guarantees.
