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
- [x] Lock terminal key capture so a focused terminal shell receives every key
  except the explicit focus-escape bindings (Tab/Shift+Tab by default).
  Application commands — quit, help, palette, reload, copy, pane split/resize/
  close/merge, and tab mutations — must not fire from inside a terminal; they
  remain reachable after escaping focus to a non-terminal widget or overlay.
- [x] Add input-routing regressions proving a focused terminal passes `q`, `?`,
  `Esc`, Ctrl+P, Ctrl+R, arrows, and Ctrl+C through to the PTY while Tab/Shift+Tab
  still move focus, and that non-terminal focus retains the full command set.

**Exit criteria (met):** users can create, focus, resize, merge, close, and
restore independent terminal panes without cross-session state leakage, and a
focused terminal shell receives keyboard input without the dashboard
intercepting application commands except the explicit focus-escape bindings.

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
- Kitty anchors every image to the cell grid (start row/column), not to absolute
  pixels. On scroll it shifts each image's start row by the scroll amount
  (`grman_scroll_images`) and frees images once they scroll past the bounded
  history limit, so images live inside the scrollback buffer exactly like text.
  The protocol requires that images "be scrolled along with text" during both
  screen scrolling and history navigation, that only clear-screen (`ED 2`),
  terminal reset, and alternate-screen switching erase images, and that the
  lowercase delete forms release placements while keeping pixel data so a
  scrolled-away image can be re-displayed without retransmission. Kitty also
  reflows (rewraps) text on resize while keeping image placements anchored to
  their cell grid coordinates (`grman_resize` only shifts on-screen images up
  when a vertical-only shrink pushes content lines into history).

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
- [x] Child-terminal protocol state, logical graphics state, composed scene state,
  and outer-terminal serialization are separate interfaces.
- [x] Image resources, placements, virtual placeholders, and backend image IDs
  have distinct identities and lifetimes in the retained store/backend boundary;
  virtual-placeholder ownership and lifecycle integration remain future work.
- [x] Pane-local coordinates must never be confused with outer-terminal absolute
  coordinates; all projections must carry the owning surface and clip rectangle.
- [x] Hidden tabs, overlays, pane movement, resize, scrollback, alternate-screen
  transitions, reload, close, and shutdown have defined graphics behavior and
  regression coverage.
- [x] Unsupported outer-terminal protocols are visible through capability state
  and diagnostics, not inferred from an apparently successful child query.
- [x] Protocol handling remains bounded: payloads, chunk accumulation, resource
  counts, placements, placeholder cells, diagnostic history, animation frames,
  retries, and cancellation state have explicit limits.

### Workstream 1 — Capability and mode contract

- [x] Add a typed capability result with graphics mode, capability source,
  confidence, and placeholder support metadata; terminal-name hints are now
  explicitly distinguishable from active confirmation.
- [x] Define the complete stable mode set: `disabled`, `direct`,
  `unicode_placeholder`, `passthrough`, and `fallback`. The typed runtime and
  capability metadata expose all five modes.
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

- [x] Introduce a `GraphicsProtocolAdapter` that parses Kitty APC, C1 APC where
  applicable, and tmux-style DCS passthrough wrappers without mixing parsing with
  resource storage or backend output.
- [x] Support protocol fields needed for the implemented conformance slice:
  zlib compression, source crops, pixel dimensions, natural PNG/GIF dimensions,
  placement IDs, `C` cursor policy, z-index, delete selectors, frame/animation
  actions, and bounded direct-transfer negotiation. File/shared-memory modes
  remain deliberately rejected and never claim success.
- [x] Preserve exact child-output ordering across text, graphics, DA1, pixel-size,
  and graphics acknowledgements through the session-owned protocol broker and
  captured PTY/conformance fixtures.
- [x] Add a bounded response broker with separate destinations for child PTY
  responses and outer-terminal responses. Never write an outer response into a
  child session or vice versa.
- [x] Add a bounded outer-input demultiplexer that preserves keyboard/CSI input,
  handles split probe responses, and routes only graphics replies to the probe.
- [x] Replace the current crossterm-reader integration point with a process-wide
  raw-input owner so the demultiplexer is fed automatically without competing
  for stdin.
- [x] Define unsupported-transfer behavior so `t=f`/`t=s` negotiation returns a
  bounded `ENOTSUP` response; `kitten icat` can select direct stream mode without
  cmdash claiming that an image was displayed by an unavailable medium.
- [x] Add malformed-sequence recovery and cancellation so one bad graphics command
  cannot fail an otherwise healthy terminal widget. Framing recovery is isolated
  to the session, and coordinator-owned outer transfers can be cancelled.

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
- [x] Preserve natural image geometry when pixel-size ioctl data is unavailable;
  use decoded PNG/GIF dimensions or an explicit pixel/cell fallback rather than
  shrinking a known image to a misleading `1x1` placement.
- [x] Separate session image IDs from outer-terminal IDs and maintain a replay
  generation/acknowledgement state for each outer resource.

### Workstream 4 — Scene and compositor integration

- [x] Make image/placeholder primitives first-class scene data with ownership,
  clipping, occlusion, and z-order semantics; backend emission must not bypass
  overlay and surface composition.
- [x] Represent placeholder graphemes/combining marks as a backend-neutral
  primitive or validated cell cluster rather than writing invisible text directly
  after the frame.
- [x] Diff old/current visible graphics, placeholder regions, and resource uploads
  independently. Clear stale placeholders before text restoration and reapply only
  the visible, non-occluded result.
- [x] Define ordering for overlays, negative/positive image z-index, cell
  backgrounds, text, and multiple overlapping images.
- [x] Ensure zero-area, clipped, hidden, tab-switched, and pane-moved surfaces
  cannot emit graphics outside their assigned scene.

### Workstream 5 — Outer-terminal adapters

- [x] Implement a direct Kitty adapter for a compatible root or explicitly opted-in
  outer terminal, including resource reuse, delete, placement, and acknowledgements.
- [x] Implement a Unicode-placeholder adapter for pane-safe rendering: quiet
  resource upload, virtual placement creation, canonical ID encoding, placeholder
  cell emission, stale-cell clearing, and redraw recovery.
- [x] Implement the bounded tmux-style passthrough serializer with ESC
  doubling/undoubling-compatible wrapping and outer-response routing boundaries.
- [x] Add a bounded textual fallback with an explicit `Degraded` outcome when
  Kitty graphics are unavailable.
- [ ] Add protocol adapters for other supported outer paths only after capability
  and ownership semantics are defined; do not label WezTerm/iTerm2/Sixel support
  as Kitty support without a conformance result.
- [x] Provide deliberate fallbacks such as a textual/placeholder diagnostic or
  configured Sixel path, with no silent success; the optional Sixel stream has a
  bounded capture acceptance test.

### Workstream 6 — Lifecycle, performance, and security

- [x] Define upload/replay behavior for pane creation, movement, resize, tab
  switching, hidden sessions, overlays, reload, close, and application shutdown;
  session resize/shutdown and compositor visibility/occlusion fixtures cover the
  retained graphics lifecycle.
- [x] Add replay generations, unchanged-resource reuse, store cancellation on
  session shutdown/delete-all, and outer-resource cleanup when the backend leaves.
- [x] Add acknowledgement-driven outer-resource garbage collection: retain
  generation state after removal, wait for the upload acknowledgement before
  sending a delete, and retire the resource only after the delete acknowledgement.
- [x] Add bounded retries for missing or failed outer acknowledgements, with
  coordinator-owned deadlines, a fixed retry budget, visible failure metrics, and
  cancellation of unacknowledged work.
- [x] Keep file/shared-memory transfers opt-in and sandboxed; the current host
  rejects `t=f`, `t=t`, and `t=s` with bounded `ENOTSUP` responses and never reads
  arbitrary paths or shared-memory names merely because an inner application
  requested them.
- [x] Add output metrics for graphics uploads, resource reuse, payload bytes, and
  suppressed/degraded placements.
- [x] Add outer acknowledgement, acknowledgement-failure, and garbage-collection
  metrics; parsed-command latency remains future work.
- [x] Bound placeholder output and avoid re-uploading unchanged resources on every
  frame; preserve UI responsiveness during large images and rapid pane switches.

### Workstream 7 — Conformance and regression matrix

- [x] Add protocol golden/conformance tests for the supported action and field
  slice, chunk boundaries, zlib compression, transfer negotiation, response
  ordering, delete selectors, source crops, placement IDs, z-index, cursor
  policy, and animation controls.
- [x] Add deterministic scene/compositor tests for panes, overlays, clipping,
  scrolling, resize, tabs, hidden sessions, multiple placements, and resource
  collisions.
- [x] Add PTY fixtures using installed `kitten icat` for detection, image upload,
  `--place`, `--unicode-placeholder`, passthrough, animation, and failure paths.
- [x] Add an installed-`kitten icat --transfer-mode file` conformance fixture that
  drives the real file-transfer fast path end to end: kitten writes the decoded
  pixels to a `kitty-tty-graphics-protocol-*` temp file, the store reads and
  retains them, the `t=t` marker file is deleted after reading, and the retained
  pixel payload matches the source (a transparent 1x1 RGBA).
- [x] Add a conformance fixture that feeds a real animated GIF as a single
  `f=100` payload through both the store and a PTY session, asserting the store
  auto-extracts its frames into RGBA animation frames (frame count, `Running`
  state, `v` loop mapping, and per-frame decoded pixels) rather than treating it
  as a static image.
- [x] Add a conformance fixture that drives `a=c` composition on non-raw
  (PNG) frames through the headless reference model and a PTY session,
  asserting that a PNG root decodes to RGBA, composes, and converts its wire
  format to `32` identically on both paths.
- [x] Add deterministic captured outer-terminal byte-stream fixtures for direct
  upload, placement-only resource reuse, deletion, Unicode placeholders,
  tmux-style passthrough escaping, and textual fallback.
- [x] Add a bounded headless Kitty stream model that unwraps passthrough, parses
  APC/CSI/SGR output, reassembles chunks, and semantically validates resources,
  placements, placement-ID replacement, z-order, deletion, placeholder
  references, viewport clipping, z-index occlusion, randomized chunk
  boundaries, malformed sequences, bounded input rejection, and delete-
  acknowledgement acceptance. Its optional deterministic RGB framebuffer also
  validates direct pixels, source crops, alpha blending, placeholder pixels,
  clipping, z-order, deletion, and PTY-to-outer rendering.
- [x] Add a bounded headless and capture-based outer-terminal harness that verifies
  direct, Unicode-placeholder, tmux passthrough, textual fallback, acknowledgement,
  retry/cancellation, and optional Sixel streams; add other terminal cases only
  where the advertised capability is verified.
- [x] Assert both sides of every test: child receives the expected response and
  outer adapter reports/render-states the expected resource and placement.
- [x] Add failure tests proving unsupported capability, timeout, malformed payload,
  quota rejection, and outer write failure become visible diagnostics rather than
  empty successful frames.
- [x] Add deterministic performance/pressure tests for large chunked images,
  rapid pane switching with stable placement IDs, placeholder redraws without
  re-upload, bounded retained bytes/resources, and repeated acknowledgement-gated
  outer-resource cleanup.

The completed conformance tranche now includes bounded session diagnostics,
control-only Kitty APC actions, zero-ID animation continuity, lifecycle/resize/
shutdown coverage, and installed-`kitten` PTY fixtures. The remaining unchecked
items are performance/resource-pressure validation and adapters for other
protocols whose capability semantics have not yet been verified.

### Workstream 8 — Virtualized image buffer and mutation-driven emission

**Status:** planned. Today images are an *observation layer* over the emulator
grid: each placement carries a `GraphicsGridAnchor` (column, row, captured
scrollback depth, screen, scroll region, region scroll) and is re-resolved
against current scrollback/view state at render time, then the backend diffs
the visible submissions against previously-emitted ones and emits moves
(same stable `p=` id, which makes Kitty move the placement in place), scoped
deletes, or uploads. This is correct but *render-diff-driven*: the outer
terminal's placement state is reconciled only when a frame is rendered. The
upgrade makes images first-class citizens of a per-session **virtual buffer**
that owns text rows *and* image objects together, and emits an explicit,
ordered, coalesced command stream (move / delete / upload) as the buffer
mutates — the same mutation-driven model a real graphical terminal uses for
its own `grman`.

### Goals and boundaries

- [ ] Treat images as first-class citizens of each session's virtual buffer:
  a per-session `VirtualBuffer` owns an ordered list of `VirtualRow`s, each
  holding text cells (delegating to the `alacritty_terminal` grid) and the set
  of image objects attached to it. This replaces the flat `placements` map +
  per-placement anchor with structural row attachment, so "which rows does
  this image occupy" is O(1) and every buffer mutation is a structural
  operation on both text and images.
- [ ] Formalize the image-identity layer ("parse the kitty image IDs"): a
  dedicated registry owns the child's client `i=` ids, `I=` numbers (newest-
  surviving resolution), relative `P`/`Q` parents, and the mapping to outer-
  terminal resource ids and replay generations. This consolidates the
  identity handling already spread through `SessionGraphicsStore` into one
  first-class module of the virtual buffer.
- [ ] Emit explicit host-terminal commands on buffer state change instead of
  only at render time: every buffer mutation produces a `GraphicsCommand`
  stream (`Place`, `Move`, `Delete`, `Upload`) that the backend adapters
  serialize immediately (or batch per frame), so the outer terminal is
  commanded to move or delete the exact image IDs as the buffer changes.
- [ ] Keep the command stream bounded, coalesced, and idempotent: a burst of
  mutations (e.g. a 20-line scroll) emits at most one command per affected
  image object per frame, ordered by row, deduplicated, and safe to reapply.
- [ ] Preserve every existing guarantee: session isolation, ack-gated outer-
  resource GC, generation-based reuse (no re-upload), scrollback-limit
  eviction, view-offset history navigation as pure view math, and the
  direct / Unicode-placeholder / passthrough adapter contract.
- [ ] Evaluate a specialized serialization library (ratatui-image) honestly
  and document the decision; do not adopt a dependency that does not fit the
  re-emission direction.

### Virtual buffer object model

- [ ] Define `VirtualRow` as the union of a text line (borrowed from the
  emulator grid) and attached image objects; define `ImageObject` as a
  resource (decoded payload, format, generation) plus its placements (each
  with a stable outer `p=` id, source crop, z-index, cell offsets, and
  relative/virtual parent links).
- [ ] Own the mapping from child image id / number / parent to
  `ImageObject` in the identity registry, and the mapping from object to
  outer-terminal resource id + generation in the adapter boundary, so a
  single identity is unambiguous across child, virtual buffer, and outer
  terminal.
- [ ] Attach every placement to an owning `VirtualRow`; a placement that
  spans multiple rows attaches to its start row and records its cell size,
  exactly like Kitty's `start_row`-anchored `grman` placements.

### Mutation-to-command mapping

Define an explicit table from each buffer mutation to its command stream:

- [ ] **Scroll / linefeed (N rows):** move each attached image object up N
  rows (emit `a=p` with the same `p=` id and the new row); objects whose
  resolved row passes the configured history limit are deleted (`d=i,i=X,
  p=P`, then `d=i,i=X` for the last placement) and their decoded bytes
  freed.
- [ ] **Insert / delete lines (DECSTBM region):** move objects inside the
  region by the delta; objects shifted out of the region or off-screen are
  deleted. Reverse index and origin-mode cursor movement map to the same
  region-scoped move.
- [ ] **Erase scopes (ED 0/1/2/3, EL, RIS, soft reset, alternate-screen
  switch):** emit delete commands scoped to the erased rows/placements,
  preserving history rows for `ED 2`, clearing scrollback-only placements for
  `ED 3`, and clearing the alternate screen on switch — the scopes already
  modeled in the store, now produced as a single mutation with an explicit
  command stream.
- [ ] **Reflow on resize:** rewrap rows and re-attach objects to their new
  rows, emitting moves only for objects whose row actually changed, so a
  reflow never produces spurious deltas (matching how Kitty preserves a
  placement's `start_row` through a rewrap).
- [ ] **View navigation (scrollback offset):** pure view math — no commands,
  because the outer terminal already holds the placements and only the
  rendered window changes (the existing view-offset resolution remains).

### Command coalescing and adapter integration

- [ ] Add a per-session command queue that coalesces mutation bursts into one
  frame's command set: each affected `ImageObject` emits at most one
  move/delete/upload, ordered by row, deduplicated, and idempotent, and the
  queue is drained by the backend in submission order.
- [ ] Replace the render-time `submit_graphics(changed, visible, removed)`
  diff with the mutation-produced command queue as the source of truth for
  the outer terminal, while keeping the frame diff for *visibility* (which
  placements are in the rendered window after view navigation) and the
  ack-gated resource GC.
- [ ] Feed the command stream through the existing direct / Unicode-
  placeholder / passthrough adapters unchanged (they already serialize
  `a=p` moves, `d=i,i=X,p=P` scoped deletes, and placeholder-cell clears);
  the virtual buffer supplies the commands, the adapters keep the bytes.

### ratatui-image evaluation (documented decision)

- [ ] Record in the roadmap/architecture why `ratatui-image` is **not**
  adopted for the core re-emission path: it is a *client-side* renderer for
  ratatui apps — it queries the terminal for protocol support and font size,
  transforms image data into protocol payloads (Sixel/Kitty/iTerm2), and
  manages stateful Kitty placement/caching for images the *app itself* draws
  to its own terminal. It cannot parse a child process's APC stream or act
  as a middleman re-emitting a child's images to an outer terminal; the data
  direction is inverted for a multiplexer, so adopting it would mean
  re-architecting around a role it does not play.
- [ ] Note that the stateful patterns ratatui-image encapsulates — upload-
  once/re-place with a cache, stable placement ids, delete-on-remove,
  Unicode-placeholder cells — are already implemented in cmdash's
  `SessionGraphicsStore` + backend adapters (generations, `outer_placement_ids`,
  ack-gated GC, placeholder adapter), so the crate adds no missing capability
  for child-derived images.
- [ ] Keep ratatui-image (or its patterns) as a candidate only for a future
  *dashboard-owned* image path (cmdash rendering its own images to the outer
  terminal, e.g. a script-widget image output), where the client-side
  direction is correct; even there the existing adapters already cover the
  serialization, so no new dependency is expected in this workstream.

### Testing and validation

- [ ] Add virtual-buffer unit tests: row attachment, scroll moves, insert/
  delete-line moves, erase deletes, reflow re-attach, and past-limit
  eviction, asserting the object list matches the emulator grid after each
  mutation.
- [ ] Add command-stream golden tests for each mutation (scroll N, insert 3
  lines, `ED 2`, `ED 3`, RIS, alternate-screen switch, reflow), asserting
  exactly-one move per affected object, correct ordering, idempotency, and
  no ghost deletes.
- [ ] Add coalescing tests: a burst of mutations collapses to one frame's
  command set with no duplicate or conflicting commands.
- [ ] Extend `tests/kitty_verify.py` to replay the mutation-produced command
  stream against real Kitty and assert placement positions/deletion through
  `grman.update_layers` after scroll, insert/delete-line, erase, and reflow
  sequences (extending the current 38 checks).
- [ ] Add coexistence tests with Phase 16 view navigation and Phase 17
  script widgets: a terminal streaming while a widget runs, images moving
  through history, and the outer placement state staying in sync.

**Exit criteria:** images are first-class virtual-buffer citizens attached to
rows; every buffer mutation emits an explicit, coalesced, idempotent
move/delete/upload command stream that keeps the outer terminal's placement
state provably in sync (real-Kitty verified) through scroll, erase, reflow,
and limit eviction; the child's image IDs are parsed and owned by a dedicated
identity registry; and the ratatui-image decision is documented in the
roadmap/architecture with the client-side-direction rationale.

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

- [x] Establish a coherent catalog of useful built-in dashboard widgets.
- [x] Keep built-ins optional, composable, capability-aware, and usable without
  terminal sessions.
- [x] Reuse common rendering, appearance, layout, and bounded-data helpers
  instead of duplicating widget-specific behavior.
- [x] Define stable widget type names, defaults, settings, failure behavior, and
  compatibility expectations.
- [x] Provide a complete guide for creating, testing, registering, and
  distributing custom widgets.
- [x] Keep custom widgets behind the existing `Widget`/factory/context boundary.
- [x] Ensure widget authors never need direct access to terminal output, PTYs,
  compositor internals, or global mutable state.

### Built-in widget catalog

Start with dependency-light widgets that exercise the existing contracts:

- [x] Add a `status` widget for semantic success, warning, error, and neutral
  states.
- [x] Add a `key_value` widget for bounded labeled values and diagnostics.
- [x] Add a `gauge` widget for bounded progress or utilization displays with a
  textual fallback.
- [x] Add a clipped `list` widget for bounded static or scrollable rows.
- [x] Add a bounded recent-message `log` widget with severity styling.
- [x] Add a `sparkline` widget for compact historical values with a scene-safe
  glyph and textual fallback.
- [x] Add a `separator` or `spacer` widget for intentional layout grouping
  without requiring a fake text widget.
- [ ] Extend `system` only where the data source and refresh behavior are
  portable and well-defined (deferred pending a portable metrics provider).

Each widget must define stable TOML type and field names, defaults, minimum useful
geometry, update/redraw behavior, theme-role usage, focus/input behavior, bounded
data policy, degraded/failed health states, and lifecycle behavior.

Host metrics, network data, process inspection, filesystem watching, and arbitrary
command execution must not be added implicitly. Each requires a separate provider
and explicit security/dependency decisions.

### Shared widget infrastructure

- [x] Add reusable helpers for bounded text and row rendering, status/severity
  styling, clipping, and minimum-size handling, with deterministic test data.
- [x] Define how data-backed widgets request wakeups or periodic updates, ensuring
  hidden or inactive widgets do not create unnecessary work.
- [x] Keep data providers separate from rendering so providers can be tested
  without an interactive terminal.
- [x] Preserve the existing semantic theme, animation, graphics, and scene
  contracts for all new widgets.

### Widget authoring documentation

- [x] Create `docs/CREATING_WIDGETS.md` as the focused development guide, while
  keeping `docs/WIDGETS.md` primarily as the user-facing catalog and runtime
  reference.
- [x] Document how to choose between a built-in, in-process custom widget, and
  plugin widget.
- [x] Document the `Widget` lifecycle, factory contract, runtime context, and
  `WidgetInstanceConfig`/`settings` behavior.
- [x] Explain scene rendering, clipping, geometry, Unicode widths, borders,
  labels, theme roles, focus, input, resize, graphics, and animation.
- [x] Document `Unchanged` versus `Redraw`, health reporting, diagnostics,
  failure isolation, background work, wakeups, cancellation, and shutdown.
- [x] Provide a complete minimal custom-widget example and a data-backed example.
- [x] Document factory registration, configuration examples, testing strategy,
  plugin/WASM restrictions, and troubleshooting for invisible or invalid widgets.

### Documentation updates

- [x] Expand `docs/WIDGETS.md` with the built-in catalog and link to the authoring
  guide, moving implementation tutorials into `CREATING_WIDGETS.md`.
- [x] Update `docs/CONFIGURATION.md` with every new built-in type, setting,
  default, and settings namespace.
- [x] Update `docs/ARCHITECTURE.md` with shared widget helpers, provider/render
  separation, scheduling, and lifecycle ownership.
- [x] Update `README.md` with links to the widget catalog and authoring guide.
- [x] Update `docs/DEPENDENCIES.md` if a new metrics or data-provider dependency
  is selected, including its capability and portability rationale (no new
  dependency was selected).

### Testing and validation

- [x] Add configuration tests for every built-in type, default, invalid setting,
  and minimum-size case.
- [x] Add rendering/golden tests for normal, focused, empty, clipped, narrow, and
  zero-area surfaces, proving no widget draws outside its assigned scene.
- [x] Add deterministic update tests, redraw-coalescing tests, and hidden-widget
  scheduling tests.
- [x] Add health and failure-isolation tests for malformed or unavailable data.
- [x] Add reload, removal, pane-closure, and shutdown lifecycle tests.
- [x] Add theme, border, label, animation, reduced-motion, and optional-feature
  compatibility tests.
- [x] Add an example custom-widget test that follows `CREATING_WIDGETS.md`.
- [x] Add plugin capability and configuration tests wherever the authoring guide
  references the plugin path.
- [ ] Validate documentation links and configuration examples in CI where
  practical.

**Exit criteria (met):** cmdash ships with a documented, stable set of useful
built-in widgets; every widget has bounded rendering, update, health, and
lifecycle behavior; a new author can create and test a custom widget by
following `docs/CREATING_WIDGETS.md`; user-facing widget documentation is
separated from implementation guidance; and no widget bypasses the scene,
coordinator, theme, or plugin boundaries. The `system` metrics extension remains
deferred pending a portable metrics provider, and CI documentation-link
validation remains an optional follow-up.

### Non-goals

- A third-party widget marketplace.
- Arbitrary shell-command execution as a built-in widget. *(Reversed by
  Phase 17: script-driven widgets are now the dashboard item contract, with
  bounded execution, output, and restart policy.)*
- Unbounded network or filesystem polling.
- Completing the full WASM host-function ABI.
- Promising cross-platform system metrics before provider behavior is defined.
- Moving application commands or layout containers into ordinary dashboard widgets.

## Phase 15 — Configurable keybindings

This phase moves the hardcoded `command_for_key` mapping into a validated,
reload-safe configuration layer and makes terminal key capture honor the same
bindings, without regressing session input, focus, or pane behavior.

### Goals and boundaries

- [x] Define a stable, versioned keybinding schema with a single source of truth
  for application commands, focus navigation, and terminal passthrough.
- [x] Keep the current defaults byte-for-byte compatible so existing users keep
  the same quit, help, palette, reload, copy, tab, pane, and focus keys.
- [x] Route all key dispatch through the configuration layer; no widget, plugin,
  or command may hardcode its own competing key capture.
- [x] Resolve ambiguous or conflicting bindings deterministically, with validation
  errors instead of silent first-match behavior.
- [x] Preserve the Phase 8 terminal key-capture contract: inside a terminal shell
  only the configured focus-escape/navigation bindings are intercepted, and every
  other key is forwarded to the PTY.
- [x] Keep keybindings reload-safe and backend-neutral: no raw escape sequences or
  crossterm-only key codes in the public configuration.

### Configuration schema

Add a `[keybindings]` section with a stable action-to-key map:

```toml
[keybindings]
quit = "q"
quit_alt = "esc"
help = "?"
palette = "ctrl+p"
reload = "ctrl+r"
copy_selection = "ctrl+shift+c"
focus_next = "tab"
focus_previous = "shift+tab"
focus_left = "alt+left"
focus_right = "alt+right"
focus_up = "alt+up"
focus_down = "alt+down"
tab_next = "ctrl+pagedown"
tab_previous = "ctrl+pageup"
pane_split_horizontal = "ctrl+shift+h"
pane_split_vertical = "ctrl+shift+v"
pane_grow = "ctrl+shift+right"
pane_shrink = "ctrl+shift+left"
pane_close = "ctrl+shift+w"
pane_merge = "ctrl+shift+m"
```

Define the allowed key-token grammar (key names plus `ctrl`, `alt`, and `shift`
modifiers, `esc`, function keys, arrows, `pagedown`/`pageup`, and `backtab`),
reject unknown tokens and duplicate actions, and document precedence when one
key is bound to multiple actions. The terminal escape set defaults to the
focus-navigation actions (`focus_next`/`focus_previous`); remapping those also
remaps how a user escapes terminal capture.

### Implementation boundaries

- [x] Replace the hardcoded `command_for_key` dispatch with a keymap produced from
  the validated configuration plus an immutable default fallback.
- [x] Keep the coordinator as the sole dispatcher: keybindings translate into the
  existing `Command` values, so API-submitted commands and future transports share
  the same validation path.
- [x] Derive terminal escape handling from the same keymap rather than a separate
  hardcoded Tab check, so capture and navigation stay consistent.
- [x] Expose the resolved keymap through capability/help output and keep the
  discoverable-bindings UI in sync after reload.

### Documentation updates

- [x] Document the `[keybindings]` schema, key grammar, defaults, precedence, and
  reload behavior in `docs/CONFIGURATION.md`.
- [x] Update `docs/ARCHITECTURE.md` with the keymap ownership boundary between
  configuration, coordinator dispatch, and terminal passthrough.
- [x] Update the in-app help/palette text to list the currently active bindings
  rather than hardcoded defaults.

### Testing and validation

- [x] Add configuration tests for every action, the full default map, unknown
  keys/modifiers, duplicate actions, and empty or partial maps falling back to
  defaults.
- [x] Add dispatch tests proving remapped keys produce the expected `Command` and
  unmapped keys fall through to widget input.
- [x] Add terminal key-capture tests proving the configured escape binding is the
  only intercepted key inside a terminal shell, and that changing it reloads
  correctly.
- [x] Add reload, conflict-resolution, and precedence regressions, plus fuzz
  coverage for malformed key strings.

**Exit criteria (met):** every application keybinding is configured, validated,
and reload-safe; defaults are unchanged; the coordinator is the single dispatch
authority; and terminal key capture uses the same keymap so a focused shell
receives all keys except the configured focus-escape bindings.

### Non-goals

- Arbitrary multi-chord or leader-key sequences beyond the declared grammar.
- Per-widget keybinding overrides.
- Raw escape-sequence passthrough as a user-facing configuration format.
- Mouse, paste, or resize rebinding.

## Phase 16 — Scrollback buffer, history navigation, and terminal feature parity

This phase closes the gap between cmdash's live-viewport terminal and a
full-featured graphical terminal emulator. Text and graphics must live in a
single, bounded scrollback buffer, the user must be able to navigate that
history the way Kitty and Ghostty allow, and the remaining protocol surface
those terminals support must be planned and prioritized rather than left
implicit.

### Goals and boundaries

- [x] Give every terminal session one scrollback buffer that text and graphics
  move through together, matching how Kitty/Ghostty anchor images to the cell
  grid instead of to absolute pixels.
- [x] Let the user navigate history (mouse wheel, Shift+PageUp/PageDown,
  touchpad) without disturbing the live child process or leaking state between
  panes/tabs.
- [x] Bound history by a configurable line count and evict image data past that
  limit, so a long-running session cannot grow the retained store without bound.
- [x] Model the protocol's erase/reset semantics for graphics (clear-screen,
  reset, alternate-screen switch) instead of only tracking scroll displacement.
- [x] Keep emulator state (`alacritty_terminal`) the source of truth for text,
  and keep graphics state an observation/retention layer, as today.
- [x] Land the remaining Kitty graphics surface (relative placements, image
  numbers, usage hints, the full delete-selector set, storage-quota eviction)
  only with conformance coverage; never claim support without a verified result.
  Three residuals stay open below: frame `z` gap normalization, `I`-addressed
  error acknowledgements, and deep negative z-index layering.
- [x] Plan terminal feature parity (hyperlinks, synchronized output, the Kitty
  keyboard protocol, mouse/focus reports, OSC 52 clipboard, underline styles,
  bell, and notifications) as explicit, capability-aware workstreams.

### Scrollback and history navigation

The current session renders only the live viewport through
`grid().display_iter()`, while `ScrollRegionTracker` and the placement anchor's
captured `scrollback` depth keep images aligned with text as new lines scroll
in. What is missing is the *view* half of scrollback: the user cannot scroll up
into history, so graphics that have scrolled out of the viewport are simply
clipped and cannot be revisited.

Implementation approach (mirrors Kitty's cell-anchored model):

- [x] Add a per-session **view offset** (`display_offset`) that the focused
  terminal advances on wheel/Shift+PageUp and retreats on Shift+PageDown,
  clamped to `[0, history_size]`. `alacritty_terminal` already exposes the grid
  history and `display_offset`; `display_iter()` honors it, so text rendering
  shows the window `[display_offset, display_offset + rows)`. The live cursor
  is hidden while scrolled, the view stays pinned when new output arrives
  (matching Kitty/Ghostty rather than auto-jumping to the bottom), and any
  forwarded key/paste returns the view to the live screen. Mouse wheel events
  scroll the terminal's own history unless the app has enabled mouse reporting
  or the alternate screen is active, in which case they reach the child PTY.
- [x] Resolve image placements against the same offset. The existing
  `GraphicsGridAnchor::resolve_row_with_state` already maps a placement to a
  row relative to the *current* scrollback depth; `visible_submissions_with_scroll_state`
  now adds the view offset for full-screen primary placements so an image
  re-anchors to the history window exactly like the text above/below it, and
  is clipped (with its source crop) when partially outside the window.
- [x] Re-emit a scrolled-out placement when the view returns to it, reusing the
  retained resource by generation (no re-upload) rather than re-decoding the
  payload.
- [x] Keep alternate-screen and DECSTBM partial-region placements isolated from
  primary-screen history navigation, as today; only full-screen primary
  placements participate in the shared scrollback window.
- [x] Add a scroll indicator (percentage/line offset) in the terminal chrome and
  a bounded scrollbar, both optional and theme-aware, matching Kitty/Ghostty's
  visual affordances. The scrollbar draws a muted track with a focus-colored
  thumb on the right edge whenever history exists, and the percentage indicator
  appears right-aligned in the title bar while scrolled away from the live
  screen. Both are toggled per terminal via `scrollbar` and `scroll_indicator`
  settings (default enabled).

### Graphics scrollback semantics

- [x] Make the session `ScrollRegionTracker` (or a sibling observer) also
  observe `ED 2` (clear screen), `RIS`/soft reset, and 1049/1047/47
  alternate-screen transitions, and forward matching
  clear-visible/clear-all/clear-alternate operations to the graphics store so
  images are erased in the same scope a real terminal erases text. `ED 2`
  erases visible placements while preserving history rows; `RIS` clears all
  placements and resources; and screen switches erase the alternate screen.
  `ED 0` erases from the cursor row to the bottom of the screen, `ED 1` from
  the top down to the cursor row (both at row granularity, matching Kitty's
  `grman_remove_cell_images`), and `ED 3` clears scrollback-only history
  placements. Erase scopes are resolved against the scrollback depth captured
  *before* the emulator consumes each chunk, so `ED 2` cannot slide visible
  images into history nor `ED 3` resurrect a scrolled-out image.
- [x] On primary-screen scroll, keep shifting placement anchors by the scroll
  amount; when a placement's resolved row falls past the configured history
  limit, drop the placement and, once an image has no remaining placements,
  release its decoded bytes (Kitty's `grman_scroll_images` free-past-limit
  behavior). The per-terminal `settings.scrollback` count (default 10000, the
  emulator's default) bounds both `alacritty_terminal`'s history and
  `SessionGraphicsStore::evict_beyond_scrollback_limit`, which drops full-screen
  primary placements whose monotonic scroll displacement exceeds
  `row + limit` and frees images with no remaining placements. A pager-history
  allowance is not yet modeled.
- [x] Implement the lowercase/uppercase delete distinction precisely:
  lowercase `d=` variants release placements but retain pixel data so a
  scrolled-away image can be re-displayed without retransmission; uppercase
  variants free data too, unless the image is still referenced by a placement
  in the scrollback buffer. `d=a`/`d=p` erase placements but keep the decoded
  resource (and the generation-last image id) so the client can re-place it;
  `d=A`/`d=P` additionally free every image with no remaining placement, and
  `d=I` frees the targeted image only once its last placement is gone.
- [x] Reflow (rewrap) text on resize (handled by `alacritty_terminal`'s primary
  grid) and re-anchor image placements across the reflow. A column change
  rewraps text and moves its scrollback depth without scrolling content
  uniformly, so the store re-captures each full-screen placement's grid row
  against the new scrollback depth — the placement keeps its grid cell instead
  of being spuriously shifted by the rewrap, matching how Kitty preserves a
  placement's `start_row` through a reflow. Row-only resizes stay on the
  scrollback model, and partial-region/relative placements keep their existing
  resolution.
- [x] Evict transient images (protocol `N=1` usage hint) before retained ones
  under storage pressure, and bound the decoded-byte quota the way Kitty's
  320MB-per-buffer quota works, with an explicit degraded diagnostic on
  overflow. On quota pressure the store now evicts in Kitty's order —
  unreferenced images first, then transient before retained, then oldest
  first — records an eviction diagnostic, and only rejects the upload if the
  budget still cannot be met (e.g. a single oversized payload).
- [x] **Keep the outer stream in step with scroll**: scrolling with images on
  screen no longer tears text from graphics or leaves ghost placements at the
  old cells. Two compounding defects were fixed in the outer-rendering path.
  First, the text renderer mapped alacritty's absolute grid lines (negative
  when scrolled into history) straight into `u16` scene rows, wrapping history
  rows to ~65535 and silently dropping them; it now translates via
  `line + display_offset` (alacritty's own `point_to_viewport`) exactly like
  the graphics path. Second, the backend never deleted removed placements: every
  upload is sent quiet (`q=2`), so the acknowledgement-gated delete could never
  fire, and moved placements were re-emitted without a stable `p=` id, so the
  outer terminal stacked a new placement on top of the old one. The store now
  assigns each placement a stable outer-terminal id (`p=`, per-image unique,
  keyed by the placement's map key) so a scrolled/reflowed placement is
  re-placed with the same id and Kitty's `grman_put` moves it in place; deletes
  are emitted unconditionally, scoped to the placement (`d=i,i=X,p=P`) while
  the image still has other visible placements and image-level (`d=i,i=X`) for
  the last one. Unicode-placeholder mode keeps still-visible virtual images
  alive and clears only the departed cells.
- [x] **Verify the outer stream against a real Kitty**: `tests/kitty_verify.py`
  drives the actual compiled Kitty emulator offscreen (no display needed) via
  the same `test_create_write_buffer`/`test_parse_written_data` hooks Kitty's
  own test suite uses, replays the exact bytes cmdash emits for a scroll-move,
  and asserts through Kitty's real `grman.update_layers` that a same-`p=`
  re-place moves the placement (no ghost), that `d=i,i=X,p=P` removes exactly
  one placement, and that text scrolling pushes a placement into history where
  it appears exactly once at any view depth. It also replays the Unicode-
  placeholder byte streams (`write_placeholder_upload`/`write_placeholder_cells`/
  `clear_placeholder_cells`), confirming against real Kitty's cell-image scan
  that the 24-bit-color lower id, 0-based row/col combining marks, and 1-based
  high-8-bits mark (Kitty's `diacritic_to_num` is 1-based and subtracts one)
  all decode to the right image and source rect, and that placeholder moves
  leave no ghost; plus the tmux-passthrough framing, verified lossless
  byte-for-byte and driving real Kitty to the same end state as direct mode.
  Animation coverage reads back Kitty's coalesced frame pixels via
  `image_for_client_id` and compares them against cmdash's
  `coalesce_frame`/`compose_animation_frame`: `a=f` deltas onto a blank
  canvas, `a=c` full overwrites, partial source-crop rects, and alpha
  blending all reproduce cmdash's results (alpha blending may differ by 1 in
  a low byte because Kitty blends in float and truncates while cmdash blends
  in integer math), and an animated-GIF playback stream (coalesced RGBA root
  + `a=f` frame with gap) matches the store's extraction and duration. This
  surfaced one correction:
  lowercase `d=i` **retains the image data** at the outer terminal (the
  protocol's re-display-without-retransmission contract), so the backend keeps
  its cached resource and re-places with a bare `a=p` on reappearance instead
  of re-uploading; only uppercase `d=I` frees the data. The headless reference
  model now mirrors that distinction.

### Remaining Kitty graphics protocol surface

These are implemented by Kitty/Ghostty and currently missing or partial in
cmdash. Each item ships with a conformance test and an `ENOTSUP`/`EINVAL`
acknowledgement path rather than silent success.

- [x] **Relative placements** (`P`, `Q`, `H`, `V`): anchor a placement to
  another placement (`P` = parent image id, `Q` = parent placement id) with a
  signed cell offset (`H`/`V`); track parent lifetime (a child is removed with
  its parent), reject cycles (`ECYCLE`) and missing parents (`ENOPARENT`) and
  over-deep chains (`ETOODEEP`, allowing the required depth of 8), never move
  the cursor, and reject a virtual placement (`U=1`) made relative (`EINVAL`).
  Relative origins are re-resolved against scrollback/region state at render
  time so a child follows its parent through scrolling and history navigation.
- [x] **Virtual placements** (`U=1`): model Kitty's invisible prototype
  placements. A virtual placement never renders, never moves the cursor, never
  scrolls, and never re-anchors on resize; it can be a relative placement's
  parent but cannot itself be relative (`EINVAL`). Delete selectors mirror
  Kitty's `is_virtual_ref` distinction: only `d=i/I`, `d=n/N`, and `d=r/R`
  remove virtual placements, while `d=a/A`, `d=c/C`, `d=p/P`, `d=q/Q`,
  `d=x/X`, `d=y/Y`, and `d=z/Z` (and the screen-erase scopes) leave them alone
  because they have no physical location. The position a relative child
  derives from a virtual parent comes from the parent's Unicode-placeholder
  cells (see the virtual-parent origin item below).
- [x] **Image numbers** (`I` key): allocate a fresh internal id on every
  numbered transmit and reply `i=<id>,I=<number>;OK`; resolve `I` references on
  place/frame/animate/compose commands to the newest surviving image with that
  number (falling back to the previous one once the newest is freed); reject
  commands that specify both `i` and `I` (`EINVAL`). The `d=n/N` delete
  selector that also keys off `I` remains in the full delete-selector item
  below.
- [x] **Usage hints** (`N=1` transient): prefer transient images for eviction
  (parsed from the `N` bitmask on transmit and stored per resource; there is
  no durable disk cache to skip).
- [x] **Full delete-selector set** (`d=n/N,c/C,q/Q,r/R,x/X,y/Y,z/Z`): complete
  the delete matrix beyond the original `a/A,p/P,f/F,i/I` slice. `d=c/C`
  targets the cursor cell, `d=p/P` and `d=q/Q` a 1-based `x`/`y` cell (with a
  `z`-index filter for `q/Q`), `d=x/X`/`d=y/Y` a column/row, `d=z/Z` a
  z-index, `d=n/N` the newest image with the `I` number (optionally narrowed
  by `p`), and `d=r/R` the inclusive image-id range `[x, y]`. Position-based
  selectors resolve each placement's current cell against scrollback and are
  scoped to the active screen; every lowercase variant retains pixel data and
  every uppercase variant frees it once unreferenced.
- [x] **Animation completion**: `a=c` now composes a pixel rectangle from one
  frame onto another (source frame `r`, destination frame `c`, source offset
  `X`/`Y`, destination offset `x`/`y`, shared size `w`/`h`, and the `C`
  alpha-blend/overwrite mode) for raw RGB/RGBA frames, rejecting missing
  frames (`ENOENT`), out-of-bounds rectangles (`EINVAL`), and overlapping
  same-frame rectangles (`EINVAL`); non-raw PNG/GIF frames are rejected with a
  diagnostic. The remaining `a=a` animation-control key `v` (loop count) is
  also stored. Frame *playback* (scheduling the stored frames to the outer
  terminal) remains a deliberate boundary.
- [x] **Storage quota and LRU eviction**: evict unreferenced/transient/oldest
  images on quota pressure (unreferenced first, then transient before
  retained, then oldest by generation) and surface the eviction as a
  diagnostic before falling back to rejection on overflow.
- [x] **Quiet response suppression** (`q` key): `q=1` suppresses success (`OK`)
  responses while `q>=2` suppresses every response, matching Kitty's
  `finish_command_response`; a failure is still recorded as a diagnostic
  regardless of `q`.

### Kitty graphics protocol — next workstream

The following surface is still missing or partial relative to Kitty/Ghostty.
Each item should ship with a conformance fixture and an explicit
`ENOTSUP`/`EINVAL` path rather than silent success.

- [x] **Local transfer mediums** (`t=f` file, `t=t` temp file, `t=s` shared
  memory): the store now reads payloads from the filesystem/shared memory using
  the `S`/`O` size/offset keys (whole-file read when `S` is absent), bounds the
  read against the decoded-storage budget, unlinks `t=s` shared-memory names,
  and deletes a `t=t` file only when it carries the `tty-graphics-protocol`
  marker (matching Kitty's temp-file convention). Same-user local reads, so no
  remote/SSH boss permission hook is modeled.
- [x] **`a=f` frame composition**: compose transmitted frame data onto a
  background canvas using `c=<frame>` (previous frame), `r=<frame>` (edit an
  existing frame), `X=1` (replace vs. the default alpha blend), `Y=<RGBA>`
  background color, and the partial rectangle `x`/`y`/`s`/`v`. New frames are
  stored as deltas and coalesced on demand; editing an existing frame coalesces
  it, composes the new rectangle, and stores a full keyframe. `a=c` also
  coalesces delta sources/destinations before composing.
- [x] **Terminal-driven animation playback**: the store advances frames on the
  wall clock at each frame `z` gap (skipping gapless frames), honoring the
  loop count and animation state (`Loading` plays once; `Running` loops per
  `v`). The render path serves the coalesced current frame with a bumped
  generation so the outer terminal re-uploads it, and the scheduler wakes the
  render loop at each frame deadline.
- [x] **`d=f` frame deletion**: delete a specific frame via `r=<frame>` with
  renumbering, gap rebalance, and current-frame index adjustment (deleting the
  root promotes the first extra frame), plus the `F` free-the-image
  distinction — `d=f` alone no longer clears every frame unconditionally.
- [x] **`a=c` composition for non-raw frames**: decode PNG/GIF frames so they
  compose on pixels rather than being rejected with a diagnostic. A `f=100`
  PNG/GIF frame is decoded to RGBA8 on coalesce (`png`/`gif` crates), and
  composing onto a PNG/GIF root converts the resource to raw RGBA (`f=32`).
  `f=100` PNG payloads now also read their natural dimensions.
- [x] **Z-order tie-break by image id**: order equal-z overlaps by lower image
  id first, matching Kitty, instead of insertion order. The store, scene, and
  headless reference model all sort equal-z placements by ascending image id;
  the composited scene ties equal-z layers across sessions by the full
  resource id (session, then image) so a multiplexed scene stays total even
  when two sessions reuse the same client image id.
- [x] **Virtual-parent origin**: derive a relative placement's position from
  its virtual (`U=1`) parent's Unicode-placeholder cells (min x / min y)
  rather than the creating cursor. The session scans the child's text grid
  for U+10EEEE placeholder glyphs (decoding the image id from the foreground
  RGB plus the third combining mark) and feeds the cells to the store, which
  resolves a virtual parent's origin from the min column / min row of its
  placeholder cells; a relative child of a virtual parent with no placeholder
  cells yet is invisible, matching Kitty's `resolve_cell_ref`.
- [x] **Transient propagation on compose**: mark a composited frame transient
  when any source frame carries the `N=1` hint (per-frame, not per-image).
  The `N=1` bit is now stored per frame (the root frame on the resource, each
  extra frame on its `GraphicsAnimationFrame`); `a=f` deltas inherit their
  base chain's transient status, frame edits OR the coalesced chain with the
  transmitted hint, and `a=c` marks the destination transient when either
  source frame is transient, matching Kitty's `CoalescedFrameData.transient`
  propagation. Eviction still keys off the root frame's hint, like Kitty.
- [x] **`a=q` query loading**: a query now loads and validates its payload
  like a transmit — it requires an `i=` image id (else logs and emits no
  response), resolves the transfer medium (base64/zlib or file/shared-memory
  read), checks the format, enforces Kitty's raw `bpp * s * v` data-size
  match and the 10000-dimension cap, and requires a parseable GIF/PNG header
  for `f=100`. It replies `OK` only when the image would load and never
  retains the image.
- [x] **Frame `z` gap normalization**: map `z=0` to the default gap and `z<0`
  to a gapless (0ms) frame instead of storing the raw value. The store keeps
  the raw `z` on the frame and normalizes at consumption time
  (`normalized_gap_ms`: `z=0` -> the default gap, `z<0` -> 0ms), and playback
  skips gapless frames immediately like Kitty's `while (!gap)` loop.
- [x] **GIF auto-animation**: extract animated GIF frames from `f=100`
  payloads instead of treating them as a static image. Animated GIFs are
  decoded into coalesced full-canvas RGBA frames (root + one animation frame
  per extra GIF frame, with per-frame delays and the Netscape loop count
  mapped onto Kitty's `v`), so they play back like a graphical terminal;
  static GIFs stay `f=100` static images.
- [x] **Error acknowledgements for `I`-addressed commands**: emit failure
  responses when a command is addressed by image *number* (`I`) alone. The
  response builder parses `I` and echoes `i=<resolved id>,I=<number>;ENOENT`
  (plus `p=` when a placement id was given), and a query with an unresolved
  number logs a diagnostic instead of replying OK, matching Kitty's
  `image_spec_by_id` fallback semantics.
- [x] **Full-range 32-bit z-index**: parse the placement `z` key as the full
  i32 range instead of i16. `GraphicsPlacement`/`GraphicsPlaceholderLayer`
  carry `i32` z-indexes, the scene sorts by the full value, and the value is
  passed through to the outer terminal verbatim; deep-negative values below
  `INT32_MIN/2` (Kitty draws these under cells with non-default backgrounds)
  are the outer renderer's concern in a passthrough architecture, so they are
  preserved rather than clamped.

### Terminal feature parity (Kitty/Ghostty baseline)

Plan and prioritize these as separate, capability-gated workstreams; do not
imply support from the presence of `alacritty_terminal` alone. Each requires a
captured-sequence or PTY conformance fixture.

- [x] **OSC 8 hyperlinks** — OSC 8 links are parsed by the emulator and
  retained per cell; `hyperlink_at`/`selected_hyperlink` expose the target URI
  so a copied selection over a link notifies the URL even when its display
  text differs, and the copy path prefers the link target. Link-count bounding
  and click-to-open remain out of scope.
- [x] **Synchronized output** (DEC 2026/2027 BSU/ESU) — the emulator's parser
  buffers a `?2026h`…`?2026l` burst and flushes it atomically on the ESU; the
  session now also enforces the 150 ms sync timeout, flushing a burst whose
  ESU never arrives so a misbehaving child cannot strand output.
- [x] **Kitty keyboard protocol** — extended key encoding (`CSI number ;
  modifier u`) for disambiguation, negotiated through the kitty
  progressive-enhancement stack (`CSI >`/`=`/`<` push/set/pop and `CSI ? u`
  query, answered from the emulator's per-screen mode stack). A child that
  opts in receives disambiguated modified/ambiguous keys while legacy programs
  keep the C0/ESC/`CSI ~` encodings. The child's `TERM` must still advertise
  the protocol before programs will request it (see capability advertisement
  below).
- [x] **Mouse protocols** — SGR mouse (1006), UTF-8 mouse (1005), and the
  legacy X10 fallback are emitted only after the app negotiates a reporting
  mode: `?1000` reports presses, `?1002` adds drags/releases, and `?1003` adds
  button-less motion, with every release encoded as button 3. Mouse reporting
  also suppresses the terminal's own selection so events reach the child
  verbatim. Focus reporting (`?1004`) sends `CSI I`/`CSI O` as focus moves
  between panes.
- [x] **OSC 52 clipboard** — the emulator's OSC 52 handling is enabled and
  both directions are wired: a child's store (copy) and the terminal's own
  selection both land in a session-shared, byte-bounded cache and are
  submitted to the backend, while a child's load (paste) queries the outer
  terminal's system clipboard (`ESC ] 52 ; c ; ? ST`) and delivers the
  decoded answer back to the session, falling back to the cache when the host
  does not respond within the read timeout (and when no frontend is attached).
- [x] **Text presentation attributes** — italic, reverse, strikeout, and hidden
  (SGR 3/7/9/8) plus underline now flow from the emulator through
  `CellStyle`/`Scene`/backend serialization instead of being dropped; blink
  (SGR 5/6) is not stored per cell by the emulator and remains a no-op.
- [x] **Underline styles** — undercurl/dashed/dotted/double/colored underline
  from SGR 4:x and DECSET 58 are retained on the cell and emitted as
  `4:x`/`58` SGR; an outer terminal that lacks the style degrades to its own
  underline rendering.
- [x] **Bell and visual bell** — `BEL` is routed from the emulator to the
  frontend and surfaced as a bounded visual diagnostic; consecutive bells
  collapse so a bell flood cannot spam the status area. No audio is available
  from a TUI, and bells from hidden sessions do not wake them.
- [x] **Notifications and shell integration** — OSC 9 (message/progress) and
  OSC 777 (notify) sequences are parsed from the output stream into bounded,
  truncated frontend diagnostics rather than being treated as arbitrary
  output. OSC 133/1337 prompt markers are recognized but intentionally
  ignored, since cmdash renders the live grid directly and does not need
  shell-integration grouping.
- [x] **Capability advertisement** — the terminal's `TERM` is now configurable
  (`settings.term`, default `xterm-256color`; set `xterm-kitty` to opt
  programs into the negotiated keyboard/graphics protocols), DA1/DA2 are
  already answered by the emulator, and XTVERSION (`CSI > q`) is now answered
  with a `DCS > | cmdash <version> ST` identity.

### Testing and validation

- [x] Add PTY fixtures that scroll text+images off the top, navigate the view
  back, and assert both text and images reappear at the correct rows with no
  re-upload.
- [x] Add conformance tests for clear-screen/reset/alternate-screen erasure
  scope, delete lowercase-vs-uppercase retention, and scroll-past-limit
  eviction under a configured quota.
- [x] Add reflow tests proving a resized pane re-anchors both text and
  straddling images without detaching or duplicating them.
- [x] Add conformance for relative placements (cycles, missing parents, offsets,
  cursor policy), image numbers, transient hints, and the full delete-selector
  matrix.
- [ ] Add captured-sequence fixtures for hyperlinks, synchronized output,
  OSC 52, bell, and notifications; assert the outer stream and the child
  acknowledgement sides. Session-level PTY and emulator fixtures now cover
  keyboard protocol, mouse/focus reports, text presentation attributes,
  underline styles, OSC 52 store/load, bells, shell notifications, and
  XTVERSION.
- [x] Add bounded-pressure tests (long-running sessions, rapid scroll, many
  images) proving memory stays within the configured history/byte quotas.

**Exit criteria:** a terminal session scrolls text and graphics through one
bounded history buffer; the user can navigate history and return to live output;
images are erased/evicted/reflowed exactly like a real graphical terminal; the
remaining protocol surface is either implemented with conformance or explicitly
rejected with a diagnostic; and every advertised Kitty/Ghostty-parity feature
has a captured or interactive verification path.

### Non-goals

- Re-implementing a full text emulator: `alacritty_terminal` remains the grid
  owner; scrollback view is a rendering/resolution concern, not a new grid.
- In-scrollback search, pager piping (`kitty +kitten scrollback`), or exporting
  history to a file in this phase.
- Guaranteeing images survive a *hard* terminal reset or a session whose
  history was cleared by the child.
- Claiming WezTerm/iTerm2/Zellij image protocol support without a conformance
  result; those remain gated behind verified capability semantics.

## Phase 17 — Script-driven dashboard items (`terminal` | `widget`)

This phase replaces the internal data-widget catalog with a two-type dashboard
item model. Every dashboard item is either a `terminal` (a live PTY session, as
today) or a `widget` (a shell script that is spawned directly and whose output
renders into the surface). The built-in Rust data widgets — `text`, `clock`,
`system`, `status`, `key_value`, `gauge`, `list`, `log`, `sparkline`,
`separator`, and `spacer` — are removed; the plugin/WASM widget path is
superseded by script widgets. All dashboard items must function alongside an
active terminal session: widget output wakes the same coordinator loop as PTY
output, hidden widgets behave like hidden terminals, and scripts may opt into
bounded session context and events.

### Implementation status (complete)

The two-type model, the script process runtime, the wakeup integration, the
configuration migration, and the data-widget removal are implemented and
tested (`src/script.rs`, `src/config.rs`, `src/widget.rs`, `src/state.rs`).
The coordinator-owned session-event bus is also implemented
(`src/session_events.rs`): terminal sessions publish bounded focus/title/line/
exit events, script widgets subscribe with a bounded queue and deliver them to
their spawned process on fd 3 (`text` or `json`), and `session_env` exposes the
read-only `CMDASH_SESSION_*` context snapshot at spawn.

### Goals and boundaries

- [x] Collapse the dashboard item model to exactly two types: `terminal` and
  `widget`. `terminal` keeps its current session/PTY/emulator/graphics
  contract unchanged.
- [x] Make the `widget` type a first-class script runner: the configured
  `command` is spawned as a child process, stdout renders into the surface,
  stderr feeds bounded diagnostics, and the process lifecycle (spawn, read,
  restart, reap, kill) is owned by the widget.
- [x] Keep the widget runtime on the shared event/wakeup path: script output
  notifies the same coalescing `SessionWakeup` used by terminal PTY readers,
  so widgets never require their own polling timers and never block or starve
  the frame loop while terminals stream.
- [x] Give scripts bounded, opt-in session integration: read-only session
  context as environment variables at spawn, and an event pipe (fd 3)
  delivering bounded terminal-session events (line output, focus changes,
  title changes, exit).
- [x] Remove the internal data-widget implementations and migrate existing
  configurations: every removed type rewrites to `type = "widget"` with an
  equivalent shell command, preserving titles, labels, and appearance
  settings where possible.
- [x] Ship the widget catalog as example scripts (`config/widgets/*.sh` and
  `examples/widgets/`) instead of compiled code, so the old catalog remains
  reachable as editable, documented scripts.
- [x] Supersede the plugin/WASM widget path: dashboard items are scripts or
  terminals; the Wasmtime host stays compile-gated and dormant, reserved for
  future host-function ABI work and no longer advertised as a widget path.
- [x] Keep every execution bounded: output ring size, line count, event queue
  depth, restart count/backoff, and process lifetime all have explicit limits
  with visible diagnostics on violation.

### Widget configuration

The `widget` type reuses the existing common fields. `command` is required:

```toml
[[workspace.widgets]]
id = 1
type = "widget"
title = " load "
command = "/usr/bin/env bash config/widgets/load.sh"

[workspace.widgets.settings]
mode = "interval"
interval_ms = "2000"
```

Define and validate these `settings` (all string-valued, as today):

- `mode`: `stream` (default) runs the script once and keeps reading stdout as
  it arrives; `interval` runs the script to EOF and re-runs it every
  `interval_ms`.
- `interval_ms`: re-run cadence for `interval` mode (default `1000`, bounded
  `100..=60000`).
- `render`: `text` (default) renders lines as plain rows, tail-kept and
  clipped to the surface; `parse_tags` (boolean) additionally recognizes the
  existing bracketed severity tags (`[error]`, `[warning]`, `[success]`,
  `[info]` and aliases) and styles the remainder of each line with the theme
  role, reusing the proven `log` helper.
- `max_lines` and `max_bytes`: the bounded output ring (defaults `1024` lines
  and `64 KiB`); overflow drops the oldest lines and records a diagnostic.
- `restart`: whether a crashed/exited script is restarted (default `true`)
  with a bounded exponential backoff (e.g. `250 ms` doubling to `8 s`); the
  last rendered output is retained while restarting so the surface does not
  flash empty, and the widget reports `Degraded` with the stderr tail.
- `handles_input`: whether focused keys are forwarded to the script's stdin
  (default `false`). When enabled, the focused widget receives keys under the
  same focus-routing contract as terminals, with application commands still
  taking precedence via the configured keymap.
- `session_env`: expose read-only session context at spawn (default `true`).
- `session_events`: `off` (default), `text`, or `json` — subscribe to bounded
  terminal-session events delivered as newline-delimited lines on fd 3.

Script environment at spawn (when `session_env` is enabled):

- `CMDASH_WIDGET_ID`, `CMDASH_WIDGET_TITLE` — instance identity;
- `CMDASH_SURFACE_COLUMNS`, `CMDASH_SURFACE_ROWS` — current surface size
  (refreshed on resize for `stream` mode, re-evaluated at each `interval`
  spawn);
- `CMDASH_SESSION_COUNT`, `CMDASH_FOCUSED_TITLE`, `CMDASH_FOCUSED_SESSION`
  — read-only session context snapshot taken at spawn.

Event lines (plain `text` format) are bounded per event and in total:

```text
session <id> focus <title>
session <id> title <new-title>
session <id> line <text>
session <id> exit <code>
```

Events are queued per subscribing widget with a bounded depth; overflow drops
oldest events and records a diagnostic. Hidden widgets receive no events and
pause `interval` re-runs (stream processes stay alive and keep a bounded ring,
matching hidden-terminal behavior), resuming on visibility.

### Process runtime contract

- [x] Spawn via the user's shell (`/bin/sh -c "<command>"`) with a piped
  stdout/stderr and an optional stdin/fd-3 pair, mirroring how terminal
  sessions spawn children today but without a PTY (scripts are not
  interactive terminals by default).
- [x] Route stdout through a per-widget bounded ring that the widget's
  `update` drains non-blockingly; a reader thread notifies the shared
  `SessionWakeup` so output wakes the coordinator exactly like PTY output.
- [x] Treat stderr as bounded diagnostics: the tail (e.g. last 4 KiB) is
  reported through health (`Degraded`/`Failed`) and the in-app diagnostics
  footer, never mixed into the rendered surface.
- [x] Define lifecycle behavior for pane close, tab hide, configuration
  reload, and application shutdown: SIGTERM, then SIGKILL after a grace
  period, with prompt reaping (no zombies) and bounded restart backoff.
  Shutdown failures become diagnostics exactly like terminal sessions.
- [x] Define resize behavior: `stream` scripts get updated
  `CMDASH_SURFACE_*` on resize and may receive SIGWINCH; `interval` scripts
  re-read the environment at each spawn. Output is always clipped to the
  surface by the scene, so an oversized line can never corrupt neighbors.
- [x] Restart a crashed script only within the bounded backoff budget; an
  exit is not a dashboard failure while `restart = true`, but repeated
  immediate exits escalate to `Failed` health with the stderr tail.

### Session coexistence and event bus

- [x] Add a coordinator-owned session-event bus that terminal sessions publish
  to (bounded line/focus/title/exit events) and widgets subscribe to by id,
  with per-widget queue bounds and drop-plus-diagnostic overflow behavior.
- [x] Deliver events to scripts over fd 3 as plain text or JSON,
  never mixing event lines with stdout content, and never writing an event
  into a terminal PTY.
- [x] Prove coexistence under load: a streaming terminal and a chatty widget
  script both wake the same loop, frames stay coalesced, and neither side
  starves the other (bounded batch processing per tick, as today).
- [x] Keep hidden-widget semantics explicit: no redraw, no events, no
  `interval` re-runs; state and ring retained for immediate restore.

### Migration and the shipped catalog

- [x] Bump the workspace schema version and add migration rules for every
  removed type, emitting an actionable warning plus a rewritten entry:
  - `text` → `widget` with `command = "printf '<text>'"`;
  - `clock` → `date +%H:%M` / `date +%H:%M:%S` by `format`, `mode =
    "interval"`, `interval_ms = "1000"`;
  - `system` → `uname -sm`, `mode = "interval"`;
  - `status` → `printf '<text>'` with the configured state mapped to a
    `[ok]`/`[warn]`/`[err]` tag and `parse_tags = "true"`;
  - `key_value` → `printf '<key>: <text>'`;
  - `gauge` → `printf '<value>%%'` with a documented visual approximation;
  - `list` → `printf '%s\n' ...`;
  - `log` → same, retaining the severity-tag convention with `parse_tags`;
  - `sparkline` → migrated to the shipped `sparkline.sh` example (values
    from a `CMDASH_WIDGET_VALUES`-style setting or inline) or plain value
    text with a documented approximation;
  - `separator` → `printf '─%.0s' $(seq 1 $CMDASH_SURFACE_COLUMNS)` style
    script (or empty output with a themed rule rendered by the `separator`
    render mode if kept as a script-driven render option);
  - `spacer` → `printf ''`.
- [x] Update `config/default.toml` to the two-type model with script widgets
  and at least one `terminal`, and add `config/widgets/*.sh` plus
  `examples/widgets/` covering clock, load/uptime, git status, weather-free
  system info, log tail, and a streaming example.
- [x] Reject plugin widget types in configuration with a migration diagnostic
  pointing at script widgets, and mark the Wasmtime host dormant in the
  feature documentation (compile-gated, no longer a documented widget path).

### Documentation updates

- [x] Rewrite `docs/WIDGETS.md` around the two-type model: the `widget` script
  contract (spawn, env vars, fd 3 events, stdout/stderr split, render modes,
  tags), the shipped example scripts, and how widgets and terminals share one
  layout, focus, and wakeup path.
- [x] Convert `docs/CREATING_WIDGETS.md` into a script-authoring guide (the
  shell contract, testing a script against the harness, and when a widget
  should be a `terminal` instead), keeping the Rust `Widget` trait docs for
  the now-internal implementations.
- [x] Update `docs/CONFIGURATION.md` with the `widget` settings, migration
  behavior, and the two-type validation rules (e.g. `widget` without
  `command` is rejected).
- [x] Update `docs/ARCHITECTURE.md` with the script process runtime, the
  session-event bus, wakeup integration, and the dormant plugin boundary.
- [x] Update `docs/DEPENDENCIES.md` only if a new process-management
  dependency is selected (prefer stdlib `std::process` plus the existing
  portable-pty machinery; no new dependency expected).
- [x] Update `README.md` and the decision log table with the two-type item
  model and the script-widget contract.

### Testing and validation

- [x] Add process-runtime unit tests: spawn, bounded ring overflow (oldest
  dropped + diagnostic), EOF, SIGTERM/SIGKILL shutdown, zombie reaping,
  bounded restart backoff, and restart escalation to `Failed`.
- [x] Add wakeup integration tests proving script output and PTY output share
  the coalescing wakeup and that a chatty script cannot starve terminal
  rendering (bounded per-tick processing).
- [x] Add session-event bus tests: publish/subscribe by widget, per-widget
  queue bounds, drop-plus-diagnostic overflow, focus/title/line/exit event
  shapes, and fd 3 delivery without PTY contamination.
- [x] Add hidden-widget tests: no redraw, no events, paused `interval`
  re-runs, retained ring, and clean resume.
- [x] Add migration tests for every removed type (round-trip parse, warning
  emission, rewritten `widget` entry, and preserved titles/appearance), plus
  invalid-case tests (`widget` without `command`, bad `interval_ms`, unknown
  `mode`/`render` values).
- [x] Add script-fixture tests that run real shell scripts against the
  harness: a streaming script (bounded tail behavior), an interval script
  with an injected clock (re-run cadence), a tag-emitting script (`parse_tags`
  styling), an exiting script (restart + health), and an input-forwarding
  script (focused keys reach stdin).
- [x] Add coexistence PTY fixtures: one active `terminal` streaming while a
  `widget` script emits on a cadence, asserting both update the same frame
  loop and neither loses output.
- [x] Update the fuzz corpus with the new `settings` grammar and keep
  configuration fuzz targets green.

**Exit criteria:** every dashboard item is a `terminal` or a `widget`;
`widget` items are scripts spawned and rendered directly with bounded
processes, output, and restarts; existing configurations migrate cleanly with
warnings; the shipped example scripts cover the former built-in catalog;
widgets and terminals coexist on one wakeup path with active sessions; opt-in
session env and event subscriptions work with bounded queues; and the plugin
widget path is dormant and no longer advertised.

### Non-goals

- Interactive widgets beyond optional key-to-stdin forwarding (mouse click
  actions are a later extension).
- Scripts reading other widgets' state or mutating the compositor, backend,
  layout, or terminal sessions; the event bus is read-only and bounded.
- Unbounded processes, implicit network access, or arbitrary binary plugins;
  scripts only, with the same explicit security posture as `terminal`
  `command` today.
- Both `text` and `json` event formats ship (text lines and newline-delimited
  JSON objects).

## Phase 18 — Internal text selection (mouse-driven, grid-anchored)

This phase replaces the hand-rolled rectangular selection with the emulator's
native selection machinery, so a focused terminal selects text the way a real
terminal does: flowed (not boxed) ranges, double-click word selection,
triple-click line selection, grid-anchored points that survive scrollback, and
copy text with correct wrap/newline semantics.

### Implementation status (core + keyboard + settings landed)

The delegation core, keyboard selection, and the configurable settings are
implemented and tested: the hand-rolled viewport tuple is gone and the
emulator-owned `Term::selection` is the single source of truth (`SelectionType`
modes, `Side` endpoints, grid `Point`s). Mouse tracking with a bounded
click-count window drives `Simple`/`Semantic`/`Lines`, `Shift`+click extends,
drag direction sets each endpoint's `Side`, copy uses `selection_to_string()`,
and the highlight follows the flowed `SelectionRange` with theme selection
colors. `Shift`+arrows (and `Shift`+Home/End) extend or begin a keyboard
selection anchored at the grid cursor, and the five settings
(`double_click_timeout_ms`, `semantic_escape_chars`, `selection_auto_scroll`,
`copy_on_select`, `copy_on_release`) are validated per terminal and wired into
the session (`copy_on_select`/`copy_on_release` auto-copy on mouse release).
Remaining increments: drag auto-scroll (gated by the now-parsed
`selection_auto_scroll`), deterministic clear-on-bare-click/focus-loss, and the
mouse-reporting handoff tests.

### Goals and boundaries

- [x] Replace the current `Selection { anchor, active }` viewport tuple (a
  rectangular bounding box) with `alacritty_terminal`'s own `Selection`
  machinery (`SelectionType::Simple`/`Block`/`Semantic`/`Lines`, `Side`
  endpoints, grid `Point`s). The emulator already owns this model, so the
  upgrade is wiring and translation, not a reimplementation of selection
  semantics.
- [x] Track `MouseDown` and `MouseDrag` properly, including click count, to
  drive selection mode and endpoint updates: single-click+drag is `Simple`,
  double-click+drag is `Semantic` (word), triple-click+drag is `Lines`,
  `Shift`+click extends an existing selection instead of starting a new one,
  and the `Side` of each endpoint follows the drag direction for precise
  edge handling.
- [x] Anchor selection to grid `Point`s (absolute `Line`/`Column`), not
  viewport cells, so the selection survives scrollback navigation and a view
  that moves during the drag — exactly like `hyperlink_at` already translates
  viewport cells via `point_to_viewport`/`viewport_to_point`.
- [x] Produce flowed copy text via `Term::selection_to_string()`: wrapped
  lines copy without a spurious `\n`, hard line breaks copy with `\n`, and
  wide/zero-width cells are handled by the emulator's own logic, replacing
  the scene-extraction `selected_text`.
- [x] Render the highlight over the flowed `SelectionRange` (via
  `Selection::to_range(&term)`) instead of a rectangle, so the highlight
  follows the text across wrapped lines and never over-paints continuation
  cells.
- [x] Preserve the mouse-reporting handoff: when the child application has
  enabled mouse reporting (or the alternate screen is active), the event
  reaches the child and no local selection is made.
- [x] Keep the OSC 8 `selected_hyperlink` behavior (anchor cell's link) and
  the OSC 52 copy/submit path unchanged.

### Mouse and click-count contract

- [x] Track click count in the session with a bounded double-click window
  (e.g. `500 ms`) and a movement threshold: a press within the window and
  near the previous press increments the count (1 → 2 → 3), otherwise the
  count resets to 1. Map the count to `Simple`/`Semantic`/`Lines` at
  selection start, matching Kitty/Ghostty/alacritty.
- [x] On `MouseDown` with no subsequent drag (a bare click), clear the
  selection and place the cursor at the cell; on `MouseDrag`, update the
  selection tail and compute the tail `Side` from the pointer's position
  relative to the anchor column.
- [x] On `Shift`+`MouseDown`, extend the current selection to the new point
  (update the tail, preserve the anchor and mode) instead of beginning a new
  selection.
- [ ] Support drag auto-scroll: while the pointer is held beyond the top or
  bottom of the content area, advance the session's `display_offset` (bounded
  by history) so a selection can extend into scrollback, and stop when the
  pointer returns inside.
- [ ] Clear the selection deterministically: a bare click, focus leaving the
  pane, and (configurable) on copy; keep the selection across child output,
  matching alacritty's behavior (Kitty clears on copy, which becomes the
  `copy_on_select`/`copy_on_release` setting).

### Coordinate translation and rendering

- [x] Add a session helper mapping viewport (column, row) cells to grid
  `Point`s with the current `display_offset` (and the inverse for rendering),
  shared by selection, `hyperlink_at`, and the render path so selection and
  content never drift by one row in history.
- [x] Derive the visible `SelectionRange` at render time from
  `selection.to_range(&term)` and translate its points back through
  `point_to_viewport`, drawing the theme selection colors only for cells
  inside the range and skipping continuation cells.
- [x] Handle the scrolled-back case: a selection made in history renders only
  while the matching rows are in view, and the live cursor stays hidden while
  scrolled (existing behavior) without being mistaken for the selection
  anchor.

### Keyboard selection and configuration

- [x] Add optional keyboard selection: `Shift`+arrows extend the selection tail
  by one cell/line (and `Shift`+Home/End to line ends), so selection is
  reachable without a mouse. `Shift`+Left/Right always select; `Shift`+Up/Down/
  Home/End extend an existing selection and otherwise keep scrolling history
  (`Shift`+PageUp/PageDown always scroll). Vi-mode selection
  (`toggle_vi_mode`/`vi_motion`) is explicitly out of scope for this phase.
- [x] Add validated settings: `semantic_escape_chars` (word-break characters
  passed to the emulator's semantic search), `double_click_timeout_ms`,
  `selection_auto_scroll` (default `true`, parsed now and gating the deferred
  drag auto-scroll), and `copy_on_select`/`copy_on_release` (default off,
  auto-copying the finalized selection on mouse release), with theme-aware
  selection colors using the existing selection role.
- [x] Keep selection config reload-safe and per-terminal, applying the same
  `settings` namespace as `scrollbar`/`scroll_indicator` today.

### Documentation updates

- [x] Update `docs/WIDGETS.md` (selection interaction, modes, copy, scrollback
  selection, mouse-reporting handoff) and `docs/CONFIGURATION.md` (new
  settings and defaults).
- [x] Update `docs/ARCHITECTURE.md`: selection ownership moves from the
  hand-rolled session tuple to the emulator-owned `Term::selection`, with the
  viewport↔grid translation as the only session-side logic.
- [x] Document the new `Shift`+arrow selection bindings and copy-on-release
  behavior in `docs/WIDGETS.md`; the bindings are terminal-widget-local (like
  scrollback navigation) and are not part of the `[keybindings]` map.

### Testing and validation

- [x] Add selection-mode tests: single/double/triple-click maps to
  `Simple`/`Semantic`/`Lines`, click count resets on timeout/movement, and
  `Shift`+click extends rather than replaces.
- [x] Add flowed-copy tests: a wrapped line copies without `\n`, a hard line
  break copies with `\n`, wide/zero-width cells are skipped correctly, and
  `Block` mode copies a rectangle.
- [ ] Add scrollback tests: select rows in history, navigate the view, and
  copy the same grid points; drag auto-scroll is bounded by history and stops
  on release.
- [x] Add render tests: the highlight follows the flowed `SelectionRange` and
  never over-paints continuation cells; the scrolled-back selection renders
  only while in view.
- [ ] Add mouse-reporting tests: a child with mouse reporting (or an active
  alternate screen) receives the events and no local selection is made.
- [x] Update the existing `selection_tracks_dragged_cells_and_copies_visible_text`
  and `selected_hyperlink` regressions to the emulator-owned model, and add
  click-count/keyboard-selection fixtures.
- [x] Add configuration tests for `semantic_escape_chars`,
  `double_click_timeout_ms`, `selection_auto_scroll`, `copy_on_select`, and
  `copy_on_release`, including defaults, valid values, and invalid-value
  rejection (`selection_settings_parse_and_validate`).

**Exit criteria:** a focused terminal selects text with flowed semantics,
word/line modes via double/triple-click, and Shift-click extension; selection
is grid-anchored and survives scrollback navigation and drag auto-scroll; copy
text has correct wrap/newline handling through `selection_to_string`; the
highlight follows the flowed range; keyboard selection works; and the
mouse-reporting handoff and OSC 52/OSC 8 paths are unchanged.

### Non-goals

- Vi-mode selection and search (separate future work over the emulator's vi
  helpers).
- Selection persistence across a full terminal reset or a history clear
  (`RIS`).
- Multi-cursor or multiple simultaneous selections.
- Copy-on-select enabled by default (opt-in only).

## Phase 19 — Compositor buffer aggregation (retained frame buffer refactor)

This phase restructures how the compositor aggregates per-surface scenes into a
single retained frame buffer, without changing the public `Scene` drawing
contract, the `FrameDiff`/backend contract, or the Phase 13 compositor API.
It is a performance-and-ownership refactor, not a user-facing feature.

### Implementation status (retained buffer + damage + pooling landed)

The retained frame buffer, damage-driven aggregation, scratch-vector pooling,
and `CellStyle` interning are implemented and tested. `Compositor` owns two
reused buffers (`composed` and `previous`); the main loop calls
`compose_and_diff(.., changed_widgets)`, which computes per-surface damage from
widget redraws, surface/overlay geometry-visibility-z changes, focus moves,
base-shell diffs, and explicit invalidations (plus a full redraw on first
frame/resize/active animation). Only the dirty regions are re-composited
(`Scene::blit_cells`) and re-diffed, so a steady frame touches no unchanged
cells; image/placeholder/sixel layers are rebuilt once per frame via
`Scene::accumulate_layers`. The composed buffer is exposed via
`Compositor::frame()` for the API snapshot. The per-frame change/span/layer
scratch vectors are pooled (`FrameBufferPool`) and recycled after the backend
consumes each diff (`Compositor::recycle`), and span grouping keys off a
per-frame `StyleInterner` handle rather than the expanded `CellStyle` struct;
`retained_buffer_reallocations`, `scratch_reallocations`, and
`last_frame_distinct_styles` prove the savings. The z-ordered surface/overlay
lists are cached and recomputed only when the set/visibility/z-order changes
(`z_order_recomputations` proves the reuse), and removed-graphics/placeholder
detection is a keyed-set diff (images by resource + placement key, placeholders
by their full identity) skipped when the layers are unchanged. `CellStyle` is
now a 4-byte handle into a process-wide `StyleTable` (the constructor/builder
API is unchanged), so the cell buffer stores compact styles; the per-frame
`StyleInterner` is now redundant with that global table. Remaining increment:
deferring image/placeholder/sixel layer sorting to one pass per frame.

### Goals and boundaries

- [x] Replace the per-frame full-buffer clone with a single retained,
  reusable frame buffer owned by the compositor. Today `Compositor::diff` runs
  `self.previous = Some(current.clone())`, cloning the entire composed cell
  buffer every frame (a ~`width × height × sizeof(Cell)` allocation per tick,
  plus a fresh composed `Scene` from `compose`). The refactor eliminates the
  clone and the per-frame composed-scene allocation.
- [x] Pool the per-frame allocations behind the frame buffer: cell vectors
  and the change/span/layer scratch vectors are recycled across frames, so
  steady-state rendering performs no cell-buffer or scratch allocation (first
  frame and resize still allocate, then reuse).
- [x] Aggregate surfaces directly into the retained buffer in z-order with
  last-write-wins, instead of the two-pass `compose` (blit-merge into a fresh
  scene) then `diff` (full-viewport linear scan). Composition and change
  detection now run one pass over dirty regions.
- [x] Keep the `FrameDiff` struct and the backend `submit_diff`/`submit_graphics`
  contract byte-compatible, so direct / Unicode-placeholder / passthrough
  adapters and the API snapshot path are untouched.
- [x] Preserve every correctness guarantee: clipping, occlusion (image and
  placeholder splitting), z-order, wide/continuation-cell handling, cursor
  ownership, invalidation, and full-redraw-on-resize.

### Current model and its costs (for the plan)

- `Scene` owns a flat `cells: Vec<Cell>` (one `Cell` ≈ `char` + 4-byte
  interned `CellStyle` handle + width) plus
  `image_layers`/`placeholder_layers`/`sixel_layers`.
- `Compositor::compose` allocates a fresh composed `Scene` and blits the base,
  each visible surface, and each overlay into it, sorting image/placeholder
  layers on every `add_*`/`blit`.
- `Compositor::diff` scans the full viewport cell-by-cell every frame, builds
  `CellChange`s, then `group_changes` into same-style row spans; it also
  recomputes removed graphics/placeholders with `BTreeMap`/`contains` filters
  and clones the frame into `previous`.

### Buffer and pooling design

- [x] Introduce a retained composed buffer plus a retained previous-generation
  buffer. The compositor resets the composed buffer and replaces the previous
  buffer in place; it never clones the full frame.
- [x] Add a `FrameBufferPool` that reuses the change/span/layer scratch
  vectors: `build_diff` takes them out each frame and the main loop returns
  them via `Compositor::recycle`, so steady-state frames perform no scratch
  allocation (a vector only reallocates when its capacity grows while filling;
  `Compositor::scratch_reallocations` exposes the count).
- [x] Intern `CellStyle` into a compact style handle (id into a per-frame style
  table) so identical styles are stored once and span grouping keys off the
  handle rather than the expanded 9-field struct (`CellSpan` carries the
  `StyleId`; `Compositor::last_frame_distinct_styles` proves dedup).
- [x] Migrate `CellStyle` itself to a handle-backed interned type: a
  process-wide `StyleTable` stores each distinct `StyleData` once and
  `CellStyle` carries a 4-byte index, shrinking the cell buffer and making
  styles compare as integers. The public constructor/builder API (`new`, `bold`,
  `dim`, …) is unchanged; field reads go through `CellStyle::resolve()`. (The
  now-redundant per-frame compositor `StyleInterner` can be removed in a
  follow-up.)

### Damage tracking and single-pass aggregation

- [x] Track per-surface dirty regions: a surface contributes a dirty rect when
  its widget returns `Redraw`, when it is resized/moved/revealed, when focus
  changes its chrome, or when the compositor is explicitly invalidated. The
  base shell is diffed against a cached copy, and the first frame/resize/active
  animation dirty the whole frame.
- [x] Aggregate only dirty regions each frame: surfaces blit their dirty region
  into the retained buffer in z-order (last-write-wins), and each written cell
  that differs from the previous buffer is recorded as a change in the same
  pass — no separate full-viewport cell scan and no compose-then-diff double
  traversal (the base-shell equality check is the only full-width comparison).
- [x] Cache the z-ordered visible surface and overlay lists and recompute them
  only when the set, visibility, or z-order changes (flagged by the snapshot
  diff): a steady frame reuses the cached lists without re-sorting or
  re-fetching, and `Compositor::z_order_recomputations` proves the reuse.
- [ ] (deferred) Defer image/placeholder/sixel layer sorting to one pass per
  frame after all dirty surfaces are aggregated, rather than sorting on every
  `add_*`/`blit`.
- [x] Replace the removed-graphics/placeholder recomputation with keyed-set
  diffs: images are keyed by (resource, placement key) and placeholders by
  their full (resource, area, z-index) identity, so removal detection is
  O(visible) and is skipped entirely when the layers are unchanged.

### Compatibility and validation

- [x] Keep `FrameDiff` fields and semantics identical (viewport, full_redraw,
  invalidated, changes, spans, graphics, visible/removed graphics and
  placeholders, cursor, sixel) and keep the metrics counters (optimized/
  naive/saved bytes) accurate after the refactor.
- [x] Add allocation/retention counters to prove steady-state frames reuse the
  buffers (no per-frame cell-buffer clone or composed-scene allocation),
  exposed as `Compositor::retained_buffer_reallocations`.
- [x] Update the compositor's existing golden tests (z-order, occlusion,
  spans, invalidation, cursor, resize, image/placeholder removal) to run
  against the new buffer and add retention regressions.

### Testing and validation

- [x] Add unit tests for the retained buffers: reuse across frames, resize
  reallocation, and no unbounded growth under churn.
- [x] Add damage-tracking tests: a single surface redraw dirties only its
  region and produces changes limited to that region; focus-chrome and surface
  moves each dirty both the old and new rects, and an incremental frame is
  byte-identical to a fresh full recompose.
- [x] Add single-pass aggregation tests proving the result is byte-identical
  to the old two-pass `compose`+`diff` for every existing golden fixture
  (z-order, clipping, occlusion, wide cells, cursor, overlays).
- [x] Add style-interning tests: repeated styles produce one handle, the
  per-frame table stores only distinct styles, and span grouping is unchanged
  when keyed off the interned handle.
- [x] Add set-diff tests for graphics/placeholder removal: a keyed-graphics
  test removes only the absent placement of a shared image id. (Equal-z
  tie-breaks and cross-session resource-collision cases remain deferred.)
- [x] Add allocation-counter tests asserting steady-state frames do not
  allocate the frame buffer (via `retained_buffer_reallocations`).
- [x] Re-run the full conformance suite (`cargo test`, `kitty_verify.py`, the
  headless reference model, and clippy) with no behavioral change.

**Exit criteria:** the compositor owns a single retained frame buffer; steady-
state frames aggregate dirty regions in one pass with no full-frame clone,
full-viewport scan, or per-frame scratch allocation; `CellStyle` interning and
keyed layer diffs remove the dominant per-frame work; the `FrameDiff`/backend
and API contracts are byte-compatible; and every existing golden fixture plus
new damage/pool/allocator tests pass with metrics showing the savings.

### Non-goals

- No user-facing feature or `Scene`/`FrameDiff` API change.
- No GPU, multi-threaded, or SIMD rendering in this phase.
- No change to the widget scene contract (widgets still build `Scene`s; the
  refactor is strictly below that boundary).
- No change to backend serialization, capability negotiation, or the Phase 13
  compositor API schema.

## Phase 20 — Dependency consolidation and reinvention review

This phase reviews the ~30k-line codebase for places where cmdash re-implements
what a small, well-maintained crate already does, adopts the clear wins, and
documents the deliberate keep-bespoke decisions so they are not re-litigated.
The goal is fewer hand-rolled edges and boilerplate, not a dependency for every
module: the retained scene/session model is intentionally novel and stays in-
house.

### Findings

| Area | Hand-rolled today | Candidate | Verdict |
| --- | --- | --- | --- |
| Base64 | `encode_base64_payload`/`decode_base64` (~40 lines + edge cases) | [`base64`](https://crates.io/crates/base64) | **Adopt** — tiny, ubiquitous, removes a hand-rolled encoder/decoder on the hot graphics path |
| CLI parsing | `env::args().skip(1)` + `--config`/`-c` match | [`clap`](https://crates.io/crates/clap) (derive) | **Adopt** — declarative flags, free `--help`/`--version`, covers the future `--api-*` overrides |
| Config/cache path discovery | Hand-rolled `XDG_CONFIG_HOME`/`HOME`/`.config` | [`directories`](https://crates.io/crates/directories) (or `dirs`/`etcetera`) | **Adopt** — cross-platform XDG roots for config, cache, crash, plugin dirs |
| Error types | 26 hand-rolled `impl fmt::Display` + `std::error::Error` | [`thiserror`](https://crates.io/crates/thiserror) (+ optional [`anyhow`](https://crates.io/crates/anyhow) at the `main` boundary) | **Adopt** — removes error boilerplate, makes `?`/source-chain ergonomic |
| Config reload | Metadata-polled reload (`Ctrl+R`) | [`notify`](https://crates.io/crates/notify) | **Adopt later** — event-driven reload-on-save; gate behind a `watch` setting |
| Image decoding | `png` + `gif` crates (protocol slice only) | [`image`](https://crates.io/crates/image) | **Adopt when needed** — unify decode and add JPEG/WebP/BMP for future dashboard/script-widget images; kitty's in-band `f=100` is PNG-only, so no protocol gain today |
| Sixel encoding | 220-line bounded 16-color encoder (`src/sixel.rs`) | `sixel-rs`/`tty-sixel`/`libsixel` | **Keep for now** — deliberately bounded and dependency-free; adopt only if truecolor sixel fidelity is required |
| Kitty protocol | ~9k-line store + adapters (parse/serialize/move/delete) | `little-kitty`, `kitty-graphics-protocol`, `ratatui-image` | **Keep bespoke** (see rationale below) |
| Scene/compositor/frame-diff | ~1.4k lines retained scene + diff | ratatui `Buffer`/`Frame` | **Keep bespoke** — ratatui is immediate-mode; it fights retained diff + session graphics |
| Widget runtime/layout/coordinator | ~6k lines | any TUI framework | **Keep bespoke** — product-specific session/graphics ownership |
| Animation scheduler | ~570 lines | none fit | **Keep bespoke** — coordinator-owned, no crate models it |
| Keymap grammar | ~600 lines key-token parser | none standardized | **Keep bespoke** — bounded, crossterm-typed |
| Async model | std threads + channels (no `tokio` despite the doc) | `tokio` | **Keep std** — PTY I/O is blocking-on-pty + reader threads; no async needed |

### Why the graphics path stays bespoke (do not re-litigate)

- `little-kitty`, `kitty-graphics-protocol`, and `ratatui-image` are *client-
  side* encoders: an app drawing its own images to its own terminal. They
  cannot parse a child process's APC stream, model a per-session retained
  store with stable `p=` re-placement, ack-gated resource GC, relative/virtual
  placements, or the Unicode-placeholder/tmux-passthrough re-emission modes
  cmdash needs as a multiplexer. (Workstream 8 records this for `ratatui-image`
  specifically.)
- `alacritty_terminal` deliberately exposes no session-owned Kitty graphics
  store, and no emulator-side crate exists; tmux does not re-emit Kitty at
  all. The store + VT scroll observer are therefore the novel core, not a
  reinvention of something available elsewhere.
- The client-side crates *are* the right reference for the direct-mode byte
  emitter, but adopting one would only replace a few hundred lines of
  serialization while fighting the stable-`p` move/delete semantics; revisit
  only if one grows an emulator/replay mode.

### Why the scene/compositor/widget layers stay bespoke

- ratatui is immediate-mode: its `Buffer`/`Frame` are rebuilt per draw and do
  not carry retained diffs, session-qualified graphics, occlusion, or cursor
  ownership. cmdash's retained `Scene`/`Compositor` model is what makes tab
  restoration and protocol-faithful image lifetime work; ratatui remains a
  layout-rect primitive only.
- The widget runtime, layout tree, and coordinator own session/graphics
  isolation and persistence that no framework models; Phase 19 optimizes the
  compositor in-house rather than adopting a model that would undo it.

### Adoption plan

- [x] Add `base64`, `clap` (derive), `directories`, and `thiserror` as direct
  dependencies; replace the hand-rolled base64, CLI arg match, XDG path
  discovery, and the 26 error-`Display` impls with the crates, keeping every
  error message string byte-identical so tests and docs stay valid.
- [ ] (deferred — not required to close this phase) Gate `notify` behind an
  opt-in `watch` setting and wire it to the existing validation/replacement
  reload path (never replace a valid runtime with a broken one mid-save).
- [ ] (deferred — not required to close this phase) Add `image` only when a
  non-PNG/GIF decode is needed (script-widget image output or dashboard
  thumbnails); keep `png`+`gif` for the protocol slice until then.
- [x] Reconcile `docs/DEPENDENCIES.md` with the actual `Cargo.toml`: record
  that the async model is std threads (not `tokio`), mark the adopted crates,
  and move `tracing`/`proptest`/`insta`/`criterion` to an explicit "future,
  profile-gated" list instead of "selected direction".
- [x] Add an update to the decision-log table capturing the four adopted
  crates and the keep-bespoke graphics/scene rationale.

### Testing and validation

- [x] Prove base64 parity: the crate's encoder/decoder reproduces the current
  hand-rolled output for empty, 1-byte, 2-byte, 3-byte, multi-chunk, and
  non-ASCII payloads (golden fixtures), so no retained payload changes.
  (`base64_round_trips_and_matches_the_independent_encoder` +
  `base64_decode_skips_whitespace_stops_at_padding_and_rejects_garbage`.)
- [x] Prove CLI parity: `--config`/`-c`, `--migrate-config`, and the unknown-
  argument error behave identically; add `--help`/`--version` smoke tests.
  (`cli_accepts_short_and_long_config_options`,
  `cli_rejects_missing_values_and_unknown_arguments`,
  `api_cli_overrides_have_explicit_precedence`.)
- [x] Prove path parity: the crate resolves the same config/cache/crash roots
  as the hand-rolled logic on Linux (and CI) for `XDG_CONFIG_HOME` and
  `HOME`-fallback cases.
  (`config_discovery_uses_the_directories_crate_roots`.)
- [x] Prove error parity: every public error `Display` string is unchanged
  after the `thiserror` migration (add a snapshot/expectation test over the
  error catalogue).
  (`error_display_strings_are_byte_identical_after_thiserror_migration` pins
  the `GraphicsError` catalogue; the remaining types share the same
  format-string migration.)
- [x] Re-run the full suite (`cargo test`, `kitty_verify.py`, clippy) with no
  behavioral change, and verify no new dependency is pulled into the default
  or `sixel` builds beyond the four adopted crates.

**Exit criteria (met):** the four adopted crates replace their hand-rolled
counterparts with byte-identical behavior and passing parity tests;
`DEPENDENCIES.md` matches `Cargo.toml` (std-thread async recorded, future
crates marked profile-gated); the graphics/scene/widget bespoke decisions are
documented with their rationale; and the dependency tree is unchanged outside
base64/clap/directories/thiserror. The only intentionally open items are the
`notify`/`image` adoptions, which are explicitly deferred until a `watch`
setting or a non-PNG/GIF decode need arises.

### Non-goals

- Adopting a terminal-emulator, graphics, scene, widget, or animation
  framework — the retained session/scene model is deliberate and stays in-house.
- Pulling in `tokio`, `tracing`, `proptest`, `insta`, or `criterion` unless a
  concrete profile or test need justifies them.
- Replacing the optional Wasmtime host or the sixel encoder in this phase.

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
| Graphics | Session-owned Kitty adapter with a bounded protocol framer, direct replay, Unicode-placeholder mode, typed active probing, a child/outer response broker, a process-wide raw-input owner, and scroll-aware grid anchors | Keeps child protocol handling isolated, makes outer capability evidence explicit, lets placements follow primary-screen content, and prevents outer acknowledgements from competing with keyboard input |
| Scrollback model | Session-owned bounded history plus cell-anchored image placements resolved against the current view offset, with placement/data eviction past the history limit | Lets text and graphics move through history together, keeps long-lived sessions bounded, and makes history navigation a pure view/resolution concern over the existing grid |
| Async model | Coordinator/UI owner plus per-session I/O tasks; standard-library threads and channels (not `tokio`) for PTY reads | Keeps frame submission serialized while PTYs remain responsive, without an async runtime that blocking-on-pty readers do not need |
| Configuration | TOML with checked-in `config/default.toml` and `docs/CONFIGURATION.md` | Makes the embedded fallback discoverable while keeping schema evolution explicit |
| Default configuration discovery | Explicit CLI path, user config, example/default file, embedded fallback | Preserves safe startup while giving users an editable starting point |
| Default widget palette | Use terminal-native reset/ANSI references, with a deterministic RGB fallback | Makes cmdash blend into the user's terminal without blocking on optional palette-query protocols |
| Widget chrome | Explicit border-style and label policies, including a first-class no-label mode | Keeps layout/content geometry independent from decorative labels and allows themes to control borders consistently |
| Animation model | Optional retained transitions scheduled by the UI coordinator with bounded budgets | Adds motion without compromising PTY responsiveness, deterministic rendering, or plugin isolation |
| Active terminal cursor | Blink only the focused visible terminal pane through the wakeable scheduler, with reduced-motion and static-cursor fallbacks | Provides familiar terminal behavior without waking hidden sessions or reintroducing timer-based PTY polling |
| Initial multiplexer UX | Retained tabs plus interactive horizontal/vertical panes | Validates session isolation and restoration while keeping pane mutation command-driven |
| Terminal key capture | Forward every key to a focused terminal shell except the configured focus-escape bindings (Tab/Shift+Tab), with the same keymap configurable and reload-safe | Prevents quit/help/palette/reload and pane mutations from firing inside a shell while preserving an explicit focus-escape path |
| Dashboard item model | Exactly two item types: `terminal` (live PTY session) and `widget` (a spawned shell script whose stdout renders into the surface) | Removes the compiled data-widget catalog, makes the data source user-owned and editable, and keeps one stable item contract |
| Widget data source | The configured `command` is spawned via the user's shell with bounded output ring, stderr diagnostics, restart backoff, and shutdown reaping | Widgets are shell scripts called directly rather than internally handled, with every execution path bounded and observable |
| Session coexistence | Widget output wakes the same coalescing `SessionWakeup` as PTY readers; hidden widgets pause interval re-runs and events while retaining state | Keeps all dashboard items working alongside active terminal sessions without polling timers or frame-loop starvation |
| Widget session integration | Opt-in read-only session env at spawn plus a bounded fd-3 event pipe (line/focus/title/exit), never written into a PTY | Gives scripts the context to react to terminal activity while keeping the bus read-only, bounded, and isolated |
| Plugin/WASM widget path | Superseded by script widgets; the Wasmtime host stays compile-gated and dormant, reserved for future host-function ABI work | Scripts cover the dashboard-item extension need with far less surface area; avoids advertising a widget path that is not the product model |
| Image buffer model | Per-session virtual buffer where text rows and image objects are first-class citizens; placements attach to rows and buffer mutations emit explicit move/delete/upload command streams | Makes the outer terminal's placement state mutation-driven instead of render-diff-driven, matching how a real graphical terminal owns its `grman` |
| Image identity | A dedicated registry owns child `i=`/`I=`/`P`/`Q` identities and maps them to outer-terminal resource ids and replay generations | Keeps one unambiguous identity across child, virtual buffer, and outer terminal, with session isolation preserved |
| Graphics serialization library | `ratatui-image` is not adopted for the re-emission path (client-side direction: app draws to its own terminal; cannot parse a child APC stream); its stateful patterns are already implemented in the store/adapters | Avoids a dependency that inverts the data flow for a multiplexer while documenting the reusable patterns it does confirm |
| Text selection model | Delegate to `alacritty_terminal`'s `Selection` (`Simple`/`Block`/`Semantic`/`Lines` with `Side` endpoints), anchor to grid points, and copy via `selection_to_string` | Replaces the hand-rolled viewport rectangle with flowed, scrollback-safe semantics the emulator already owns; keeps selection and content from drifting |
| Selection interaction | Track click count and drag to map single/double/triple-click to selection modes, `Shift`+click to extension, and drag auto-scroll over the bounded history | Matches Kitty/Ghostty/alacritty mouse behavior without reimplementing selection state |
| Frame buffer ownership | A single retained, reusable frame buffer with a bounded arena, per-surface dirty regions, and single-pass aggregation; no per-frame full-frame clone or scan | Removes the steady-state allocation and O(viewport) rescan while keeping the `FrameDiff`/backend contract byte-compatible |
| Style representation | `CellStyle` is a 4-byte handle into a process-wide `StyleTable` (constructor/builder API unchanged, field access via `resolve()`) | Shrinks the cell buffer and makes styles compare as integers in the diff path |
| Dependency policy | Adopt small standard crates that replace clear reinventions (`base64`, `clap`, `directories`, `thiserror`; later `notify`, `image`), keep the session-graphics/scene/widget layers bespoke | Removes hand-rolled edges where a mature crate is a drop-in, without replacing the deliberately novel retained session/graphics model |

Update this table as product decisions are made; do not let provisional choices silently become public API guarantees.
