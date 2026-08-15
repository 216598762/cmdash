# cmdash architecture

## 1. Goals and constraints

`cmdash` is a Linux-first terminal UI and multiplexer with a dashboard model. It must support both of these configurations:

- a workspace containing terminal tabs/panes and dashboard widgets;
- a workspace containing only dashboard widgets, with no terminal sessions running.

The architecture should make the second configuration a normal case rather than a special mode. Terminal support is a widget/session provider, not the foundation of every feature.

### Initial decisions

The first implementation will use these boundaries:

- **Terminal backend:** `crossterm` owns raw mode, input collection, resize events, and basic terminal controls. The cmdash scene/compositor remains independent of Crossterm's frame lifecycle.
- **Terminal emulator:** use one `alacritty_terminal` instance per session. Kitty APC sequences are intercepted by a cmdash-owned adapter and `SessionGraphicsStore`; retained image layers flow through `Scene` and `Compositor`.
- **Workspace scope:** start with one active workspace. The state model should leave room for saved workspaces later without making them part of the first runtime contract.
- **Plugin boundary:** use a versioned manifest and C-compatible data contract, with untrusted plugin execution isolated behind an opt-in Wasmtime host. Plugin manifests are validated before code loading, and the host must not pass Rust trait objects, terminal handles, WASI, or filesystem access across the boundary.
- **Initial terminal capabilities:** require ANSI/VT text, cursor movement, Unicode cell output, basic colors, alternate-screen support, keyboard input, and resize handling. Treat truecolor, mouse, bracketed paste, keyboard enhancement, and Kitty graphics as optional capabilities.
- **Appearance:** resolve semantic widget roles through a workspace `Theme`; the default uses terminal-native reset/ANSI references so the parent terminal owns inherited palette colors, while explicit RGB overrides and a deterministic fallback remain available.
- **Fallback behavior:** downgrade optional color/input features when unavailable and omit unsupported or over-limit graphics with an in-app degraded diagnostic. Capability mismatches must never emit malformed output or corrupt text/layout.

### Non-negotiable invariants

- A terminal session owns its PTY, terminal-emulator state, scrollback, cursor, selection, and graphics state.
- A tab switch changes which retained session scene is composed; it must not destroy the hidden session's visual state.
- Graphics resources are namespaced by session (and, where useful, by widget instance), so image IDs from one session cannot address another session's images.
- Widgets do not write terminal escape sequences directly to stdout. They emit model updates and renderable scene data through the application pipeline.
- The UI thread is the sole owner of terminal rendering and frame submission. Background work communicates through messages.
- A backend capability mismatch must produce a controlled fallback, not malformed terminal output.

## 2. Conceptual model

```text
Application
└── Workspace(s)
    └── Layout tree
        ├── WidgetInstance: dashboard, clock, monitor, ...
        └── WidgetInstance: terminal surface
            └── Session: PTY + emulator + retained render state
                └── Tab(s) / pane contents, depending on chosen UX model
```

The terms below are deliberately separate:

- **Workspace:** a saved arrangement and its runtime state.
- **Layout node:** a horizontal/vertical split, stack, tab group, overlay, or leaf in a workspace layout tree.
- **Widget type:** an implementation registered in the widget catalog.
- **Widget instance:** one configured and stateful use of a widget type.
- **Surface:** a rectangular region assigned to a widget instance by layout.
- **Session:** a stateful producer of terminal content. A session normally maps to one PTY and one terminal-emulator instance.
- **Scene:** retained, backend-neutral visual output for a surface for the current frame.
- **Frame:** the complete scene tree/composition result submitted to the terminal backend.
- **Notification:** bounded user-facing status or recovery information rendered in the dashboard UI rather than written to the PTY.

A terminal tab should be modeled as a separate `Session` unless the chosen multiplexer semantics explicitly require a tab group to share one emulator. Separate sessions are the safer default because it guarantees PTY, scrollback, and graphics isolation.

## 3. Proposed layers

### 3.1 Application shell

Owns process startup, configuration loading, signal/shutdown handling, logging, and dependency wiring. It should not contain widget-specific rendering logic.

### 3.2 Runtime and event coordinator

Runs the main event loop and routes:

- keyboard, mouse, resize, paste, selection/copy, and terminal capability events;
- PTY output and child-process lifecycle events;
- coordinator-owned maintenance deadlines for wakeable cursor and retained-scene animation;
- timers, filesystem/watch events, and widget messages;
- commands such as focus, split, close, reload, and switch tab.

Animation state is owned by `AppState` and advanced only by the coordinator clock;
widgets receive bounded frame progress while the compositor and backend retain
normal scene/output ownership. See [ANIMATION.md](ANIMATION.md) for the motion
contract and configuration.

The coordinator updates application state and schedules a frame. It does not render partial output from event handlers. API wakeups enter the same event path; listener failures and client backpressure remain local to the API boundary.

### 3.3 Application state and commands

Contains workspace layout, focus, keymaps, widget configuration, session registry, and command handling.

### 3.4 Local API boundary

The optional local API is a transport adapter around the coordinator, not a
second application owner:

```text
Unix socket clients
        │ bounded JSON requests
        ▼
API listener/client workers
        │ bounded queue + wakeup
        ▼
UI coordinator ──► AppState::dispatch / reload / frame snapshot
        │
        └──────────► bounded response channels
```

The listener never receives `AppState`, `Compositor`, session handles, or widget
references. A snapshot is published after composition at a frame generation;
read requests observe that generation and mutation requests execute on the UI
thread. See [API.md](API.md) for the wire contract and limits.

### 3.5 Application state details

Suggested state ownership:

```text
AppState
├── workspaces: WorkspaceState
├── focused_surface: SurfaceId
├── widget_registry: WidgetCatalog
├── sessions: SessionRegistry
└── backend_capabilities: Capabilities

SessionState
├── pty_handle / child lifecycle
├── terminal_emulator
├── scrollback and viewport state
├── selection and input mode
├── graphics_store: SessionGraphicsStore
└── render_cache / dirty regions
```

### 3.6 Widget API and plugin boundary

Widgets should have a small lifecycle-oriented interface, conceptually similar to:

- receive application/runtime messages;
- update their own state;
- handle input when focused;
- measure or accept a `Surface` rectangle;
- produce a backend-neutral `Scene`;
- optionally request timers, redraws, or external capabilities.

Dynamic plugins are an early product requirement, so this boundary must be designed before widget implementations spread across the codebase. The host exposes the versioned, capability-based contract through an opt-in Wasmtime runtime rather than passing Rust trait objects directly across a shared-library boundary. The initial host rejects all module imports, does not link WASI, configures fuel accounting, and bounds module size; future host functions must be added explicitly to the manifest capability set. The plugin manager must document plugin lifecycle, permissions, threading, and failure isolation, and reject unsupported ABI/API versions before loading widget code.

The plugin API should avoid exposing `stdout`, raw terminal escape sequences, or a concrete terminal backend. A plugin communicates through messages and backend-neutral scene data. Built-in widgets should use the same host-facing contract wherever practical so they exercise the plugin boundary continuously.

Widget categories can include:

- pure display widgets (clock, CPU, logs, metrics);
- interactive widgets (file browser, command palette);
- session widgets (terminal emulator surfaces);
- container/layout widgets (split, tab group, overlay).

Container widgets should compose child widget instances rather than reimplement their behavior.

## 4. Full render pipeline

The render path is retained and frame-oriented:

```text
OS / PTY / input
       │
       ▼
Event collector ──► Command router ──► AppState / SessionState
                                      │
                                      ▼
                              Widget update + dirty tracking
                                      │
                                      ▼
                         Layout engine assigns surfaces/clip rects
                                      │
                                      ▼
                      Theme + layout resolve each visible widget's appearance
                                      │
                                      ▼
                    Each visible widget builds a backend-neutral Scene
                                      │
                                      ▼
                    Compositor orders, clips, and merges visible scenes
                                      │
                                      ▼
                    Terminal backend diffs/submits the complete Frame
```

A frame should follow these rules:

1. Drain or batch available events so a burst of PTY output does not submit one frame per byte.
2. Apply commands and state updates before layout.
3. Recompute layout only when dimensions or layout-affecting state changes.
4. Render every visible surface from its retained state into a clipped scene.
5. Composite by z-order, applying focus decorations and overlays at the end.
6. Compare with the previous frame where safe, clear invalidated regions, diff retained image layers, and submit the backend-specific output.
7. Keep hidden sessions alive but do not include their scenes or graphics placements in the submitted frame.
8. Advance active retained animations only at coordinator wakeups; a static frame
   remains valid when motion is disabled or unsupported.
9. For placeholder graphics, clear removed layers before the text diff, upload changed
   resources, and re-emit current visible placeholder cells after composition;
   direct replay may submit only changed resource/placement layers.
10. Submit only current visible image layers and delete stale session-qualified
    image IDs.

The backend may optimize the final submission, but the logical frame must represent the complete visible dashboard. This prevents stale graphics or text from leaking across tab switches. Motion changes retained scene presentation only; it never gives widgets direct terminal output ownership. See [ANIMATION.md](ANIMATION.md) for scheduler, accessibility, and lifecycle details.

## 5. Terminal sessions and graphics isolation

### 5.1 Session boundary

A terminal widget owns a `SessionId`. The session contains its own:

- PTY and child process;
- terminal parser and emulator grid;
- alternate screen, modes, cursor, scrollback, and selection;
- title/current-directory metadata;
- protocol-specific graphics state;
- scene/render cache and dirty tracking.

Switching tabs changes focus/visibility in the layout. It does not transfer emulator state or graphics resources between sessions.

### 5.2 Kitty graphics model

Kitty graphics are treated as terminal-session state, not as a global backend cache. `alacritty_terminal` remains the text/parser owner, while cmdash intercepts Kitty APC sequences and retains resources and placements in a session-owned adapter. The current flow is:

```text
PTY bytes
  │
  ▼
Escape parser / terminal emulator
  │  cmdash intercepts Kitty APC graphics; text/grid bytes continue to the emulator
  ▼
SessionGraphicsStore(session_id)
  ├── image data/resources, keyed by session-scoped IDs
  ├── placements and z-order
  ├── cell/ pixel anchors and clipping metadata
  └── visibility/invalidations
  │
  ▼
SessionScene(session_id, surface)
  ├── text/cell layers
  └── image placement layers
  │
  ▼
Compositor includes it only if the session is visible
```

When a user switches away from a tab:

- the old tab's scene is removed from the composed frame and its rectangle is invalidated/cleared;
- its emulator and `SessionGraphicsStore` remain in memory (subject to an explicit future resource policy);
- no image data is reinterpreted as belonging to the newly selected tab.

When the user returns:

- the retained emulator state and placements are rendered into a fresh scene for the current surface size;
- the backend receives the visible frame and replayable Kitty resource commands only for that session's visible resources;
- session-qualified terminal image IDs prevent identical source IDs in different tabs from colliding;
- if the outer terminal supports Unicode placeholders, the backend creates a quiet virtual Kitty placement and emits placeholder cells after the normal text frame, so pane composition controls the visible location;
- direct `a=T` placement replay remains available when placeholder mode is unavailable or explicitly selected for a compatible outer terminal;
- if the terminal backend cannot safely retain/reuse an image, the session can replay/re-upload from its store without changing logical state.

Placeholder replay clears stale virtual placements before the next text diff and
re-emits current visible placeholders after each frame. This keeps image output
from being silently lost when a pane is moved, resized, hidden, or redrawn. The
mode can be selected with `CMDASH_KITTY_GRAPHICS_MODE=placeholder` (the default
when placeholder support is detected), `direct`, or `off`; `CMDASH_KITTY_GRAPHICS=1`
and `0` remain explicit capability overrides. Placements now retain a logical
emulator-grid anchor, active screen, DECSTBM region, region-scroll displacement, and the
scrollback depth at creation, then resolve against current terminal state before
surface clipping. Full-screen primary placements follow scrollback; partial-region
placements follow only matching region displacement, while alternate-screen
placements remain isolated.

Graphics submission is an explicit outer-rendering contract rather than a
successful no-op: the backend reports `Rendered`, `Degraded`, `Suppressed`, or
`Failed` with a placement count and bounded reason. The selected
`disabled`/`direct`/`unicode_placeholder`/`passthrough`/`text_fallback` mode is
included in backend capability metadata and API snapshots. Placeholder geometry is validated before emission,
so an invalid placement cannot leave a partial escape stream behind. Child-side
malformed graphics commands are isolated to the session: cmdash records a
bounded diagnostic and returns a Kitty error acknowledgement when an image or
placement ID is available instead of terminating the terminal widget.

The retained session store supports multiple placements per image and replaces
only the matching `(image_id, placement_id)` pair. Resource IDs remain
session-qualified, while the backend derives a separate outer-terminal image ID.
A bounded `GraphicsProtocolBroker` keeps child-PTY responses in a separate queue
from outer-terminal probe traffic. `GraphicsCapabilityProbe` emits a Kitty/DA1/
pixel-size probe, correlates only the outer Kitty acknowledgement, and reports
confirmed, rejected, or timed-out capability state; callers must provide the raw
outer input and must not feed child PTY bytes into it. Capability metadata records whether support was inferred from the environment,
explicitly overridden, or actively probed, together with confidence. The
`GraphicsInputDemultiplexer` now separates Kitty/CSI probe replies from ordinary
keyboard bytes across read boundaries. Direct replay reuses uploaded resources
by generation, passthrough wraps and ESC-doubles Kitty APCs for tmux-style hosts,
and text fallback emits a bounded degraded marker. Primary/alternate screen
anchors, DECSTBM region tracking, opaque scene occlusion, and cleanup generations
are retained. Outer resources now keep generation/acknowledgement state: removed
resources wait for the upload acknowledgement before deletion and are retired
only after the delete acknowledgement. A session-owned VT observer mirrors the emulator's private margins
and scroll displacement so partial-region linefeeds, explicit scrolls, reverse
index, origin mode, and resize resets move matching graphics anchors without
confusing them with primary-screen scrollback. Automatic ownership of the
process-wide crossterm reader remains follow-up integration work.

This is why a global image map or a single terminal emulator shared by tabs is explicitly out of scope.

### 5.3 Other graphics protocols

The scene model should allow Kitty first, then add protocol adapters such as sixel or iTerm-style images if their support is justified. Text and layout must remain correct when graphics are unavailable. Protocol handling belongs behind a capability-aware adapter, not in dashboard widgets.

A practical initial implementation can use a mature terminal parser/emulator crate and add a narrowly scoped graphics-state adapter if the selected emulator does not expose the required protocol. The adapter must have conformance tests based on captured escape sequences. The opt-in sixel path now uses the same retained scene boundary: dashboard submissions are clipped, diffed, and emitted only after backend capability negotiation.

### 5.4 Resource policy

Initial behavior should retain graphics in memory while a session is alive, whether it is visible or hidden. Later, add configurable limits:

- maximum decoded bytes per session;
- maximum number of image resources and placements;
- LRU eviction only when a session can replay or re-request data safely;
- explicit cleanup when a session closes.

Eviction must never silently change the terminal's logical state. If an image cannot be restored, the scene should show a deliberate placeholder and report a diagnostic rather than corrupting adjacent text.

## 6. Rendering and backend boundaries

### 6.1 Appearance and palette boundary

Appearance resolution is application state, not widget-local terminal I/O. The
runtime combines the inherited `Theme`, workspace role overrides, widget
settings, and transient focus/health state before a widget emits its `Scene`.
Inherited colors use `Color::Reset` and `Color::Ansi(index)`, allowing the parent
terminal to resolve its own default and ANSI palette. Explicit `Color::Rgb`
values are retained for configured truecolor roles and protocol-originated
truecolor cells. See [APPEARANCE.md](APPEARANCE.md) for the public contract.

Widgets and plugins must not query the terminal or write palette escape
sequences directly. The backend translates the scene's color representation into
Crossterm color commands, preserving the serialized frame boundary.


Use three representations:

1. **Widget/session model:** mutable state and protocol semantics.
2. **Scene:** immutable frame-local primitives such as cells, spans, borders, rectangles, image placements, and overlays.
3. **Backend submission:** terminal-specific cursor movement, color encoding, clear operations, and graphics escape sequences.

Image layers are diffed as part of `FrameDiff`; stale physical image IDs are explicitly deleted before visible current layers are replayed. A frame diff carries changed, currently visible, and removed image submissions so placeholder backends can clear old cell regions before text output and restore current placeholders afterward.

The scene should carry clipping and ownership metadata. Every image placement should include its owning `SessionId` or a derived resource namespace so the compositor can reject cross-session references during development.

The first backend can target a single local terminal, but the interface should keep these concerns separate. The first interaction model prioritizes retained terminal tabs. Configuration-driven horizontal and vertical pane splits are supported, and the command layer now creates new terminal sessions, provides directional pane focus, ratio adjustment, merge/close lifecycle operations, and persists mutable layout state through safe reload while preserving retained session ownership:

- terminal input/output and raw mode;
- layout and cell rendering;
- graphics protocol submission;
- capability detection.

Candidate crates are cataloged in [External library candidates](DEPENDENCIES.md). The current shortlist is:

| Concern | Candidate direction |
| --- | --- |
| Terminal I/O and raw mode | `crossterm` |
| Layout and cell-oriented widgets | `ratatui` primitives behind the retained scene boundary |
| Async runtime | `tokio` |
| PTY management | `portable-pty`, with narrow `nix` adapters if needed |
| Escape parsing | Parser APIs exposed by `alacritty_terminal`, with `vte` only if a narrow adapter is required |
| Terminal emulation | `alacritty_terminal`, one instance per session |
| Kitty/image output | Cmdash-owned session adapter and retained `Scene` image layers; optional dependency-free sixel encoder/submission path for dashboard RGB images |
| Dynamic plugins | Versioned manifest plus opt-in Wasmtime host with no imports | Isolates plugin faults and keeps terminal/filesystem capabilities explicit |
| Errors/logging | `thiserror`, `anyhow`, `tracing`, `tracing-subscriber` |
| Config/serialization | `serde` + `toml` |

The selected crates still require version pinning and a focused API/protocol check when the Cargo package is created. Avoid adding a crate solely to bypass a small, well-tested adapter boundary, and do not let a graphics or plugin helper become a global state owner.

## 7. Concurrency and lifecycle

The UI/coordinator task owns `AppState` and all frame composition. Per-session I/O tasks read from PTYs and send bounded messages containing output bytes, resize acknowledgements, and process events. Widget workers may send state updates but cannot mutate UI state directly.

Important lifecycle behavior:

- PTY output is backpressured or batched to avoid starving input and rendering.
- API requests are bounded, batched, and rejected on queue/size limits rather than blocking frame submission.
- Resize events update the session emulator and PTY dimensions in a defined order.
- Closing a session cancels its I/O task, waits for child cleanup, then releases graphics resources.
- Shutdown restores terminal modes even after a panic/error path where the backend supports it.

## 8. Modularity strategy

Dynamic plugins are an early requirement, with compile-time feature flags still used for optional host functionality:

- `terminal` enables PTY/session functionality;
- graphics protocols can be independently enabled where dependencies justify it;
- built-in and external widgets register versioned capabilities and TOML configuration schemas;
- a workspace config chooses which widget types and instances are present;
- plugin loading, health, permissions, and shutdown are managed by a host-side plugin manager.

The plugin manager must validate manifests, reject unsupported ABI/API versions and capabilities, isolate failures to the affected widget where possible, and ensure a plugin cannot write directly to the terminal backend. The current runtime choice is Wasmtime behind the `wasm-plugins` feature; the default build remains free of that runtime. The configuration contract is `cmdash.workspace` v1, with named plugin manifests and a string-valued widget settings map; legacy/missing config versions can be rewritten and are reported through explicit migration warnings. Runtime failures can be written as bounded reproduction artifacts when `CMDASH_CRASH_DIR` is configured.

## 9. Testing strategy

The core should be testable without a real terminal:

- layout tests for horizontal/vertical split, tab, overlay geometry, and clipping;
- scene/compositor tests proving hidden sessions contribute no primitives;
- session isolation tests using two emulators/stores with colliding Kitty image IDs;
- parser conformance tests for text, alternate screen, resize, and graphics sequences;
- frame golden tests for cell output and invalidation;
- PTY integration tests for shell startup, input, output, resize, and clean shutdown;
- capability and outcome tests for terminals with and without Kitty, direct versus
  Unicode-placeholder mode, active probe acknowledgement/timeout, explicit
  suppression, recoverable protocol errors, and opt-in sixel support;
- captured outer-terminal byte-stream fixtures for direct upload/reuse/delete,
  Unicode-placeholder cells, tmux passthrough escaping, and textual fallback;
- acknowledgement-routing tests for upload success/failure, deferred deletion,
  delete acknowledgement, resource retirement, and bounded graphics metrics;
- fuzz targets and retained seed corpora for TOML migration, plugin manifests, Kitty APC chunking, and sixel encoding;
- pane lifecycle tests for independent PTYs, nested layout persistence, and safe reload;
- release archive, checksum, feature-variant, and startup checks on tagged builds;
- API wire, authorization, queue, snapshot-generation, Unix-socket, and subscription tests.

A key regression test: write a Kitty image with ID `1` in tab A, write a different image with ID `1` in tab B, switch A → B → A, and verify that each tab restores its own image without cross-contamination.
