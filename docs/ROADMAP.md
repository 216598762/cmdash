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
- Arbitrary shell-command execution as a built-in widget.
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
- [ ] **Frame `z` gap normalization**: map `z=0` to the default gap and `z<0`
  to a gapless (0ms) frame instead of storing the raw value.
- [x] **GIF auto-animation**: extract animated GIF frames from `f=100`
  payloads instead of treating them as a static image. Animated GIFs are
  decoded into coalesced full-canvas RGBA frames (root + one animation frame
  per extra GIF frame, with per-frame delays and the Netscape loop count
  mapped onto Kitty's `v`), so they play back like a graphical terminal;
  static GIFs stay `f=100` static images.
- [ ] **Error acknowledgements for `I`-addressed commands**: emit failure
  responses when a command is addressed by image *number* (`I`) alone; today
  only `i`-addressed failures produce a response.
- [ ] **Deep negative z-index layering**: draw z-indexes below `INT32_MIN/2`
  under cells with non-default background colors in the compositor.

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
| Async model | Coordinator/UI owner plus per-session I/O tasks | Keeps frame submission serialized while PTYs remain responsive |
| Configuration | TOML with checked-in `config/default.toml` and `docs/CONFIGURATION.md` | Makes the embedded fallback discoverable while keeping schema evolution explicit |
| Default configuration discovery | Explicit CLI path, user config, example/default file, embedded fallback | Preserves safe startup while giving users an editable starting point |
| Default widget palette | Use terminal-native reset/ANSI references, with a deterministic RGB fallback | Makes cmdash blend into the user's terminal without blocking on optional palette-query protocols |
| Widget chrome | Explicit border-style and label policies, including a first-class no-label mode | Keeps layout/content geometry independent from decorative labels and allows themes to control borders consistently |
| Animation model | Optional retained transitions scheduled by the UI coordinator with bounded budgets | Adds motion without compromising PTY responsiveness, deterministic rendering, or plugin isolation |
| Active terminal cursor | Blink only the focused visible terminal pane through the wakeable scheduler, with reduced-motion and static-cursor fallbacks | Provides familiar terminal behavior without waking hidden sessions or reintroducing timer-based PTY polling |
| Initial multiplexer UX | Retained tabs plus interactive horizontal/vertical panes | Validates session isolation and restoration while keeping pane mutation command-driven |
| Terminal key capture | Forward every key to a focused terminal shell except the configured focus-escape bindings (Tab/Shift+Tab), with the same keymap configurable and reload-safe | Prevents quit/help/palette/reload and pane mutations from firing inside a shell while preserving an explicit focus-escape path |

Update this table as product decisions are made; do not let provisional choices silently become public API guarantees.
