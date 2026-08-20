# cmdash architecture

## 1. Goals and constraints

`cmdash` is a Linux-first terminal UI and multiplexer with a dashboard model. It must support both of these configurations:

- a workspace containing terminal tabs/panes and dashboard widgets;
- a workspace containing only dashboard widgets, with no terminal sessions running.

The architecture should make the second configuration a normal case rather than a special mode. Terminal support is a widget/session provider, not the foundation of every feature.

### Core decisions

The implementation uses these boundaries:

- **Terminal backend:** `crossterm` owns raw mode, input collection, resize events, and basic terminal controls. The cmdash scene/compositor remains independent of Crossterm's frame lifecycle.
- **Terminal emulator:** use one `alacritty_terminal` instance per session. Kitty APC sequences are intercepted by a cmdash-owned adapter and `SessionGraphicsStore`; retained image layers flow through `Scene` and `Compositor`.
- **Workspace scope:** one active workspace. The state model leaves room for saved workspaces without making them part of the runtime contract.
- **Plugin boundary:** an opt-in Wasmtime host validates import-free modules with fuel accounting. It is a dormant, compile-gated foundation — script widgets are the product's extension model — and it does not pass Rust trait objects, terminal handles, WASI, or filesystem access across the boundary.
- **Terminal capabilities:** ANSI/VT text, cursor movement, Unicode cell output, basic colors, alternate-screen support, keyboard input, and resize handling are required. Truecolor, mouse, bracketed paste, keyboard enhancement, and Kitty graphics are optional capabilities.
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
└── Workspace
    └── Layout tree
        ├── WidgetInstance: widget (a shell script whose stdout renders)
        └── WidgetInstance: terminal surface
            └── Session: PTY + emulator + retained render state
```

The terms below are deliberately separate:

- **Workspace:** a saved arrangement and its runtime state.
- **Layout node:** a horizontal/vertical split, stack, tab group, overlay, or leaf in a workspace layout tree.
- **Widget type:** an implementation registered in the widget registry (`terminal` or `widget`).
- **Widget instance:** one configured and stateful use of a widget type.
- **Surface:** a rectangular region assigned to a widget instance by layout.
- **Session:** a stateful producer of terminal content. A session normally maps to one PTY and one terminal-emulator instance.
- **Selection:** owned by the emulator (`alacritty_terminal`'s `Term::selection`), not a session-side rectangle. The session tracks only the click-count state and the viewport↔grid `Point` translation, then derives the flowed copy/highlight from `Selection::to_range`/`selection_to_string`.
- **Scene:** retained, backend-neutral visual output for a surface for the current frame.
- **Frame:** the complete scene tree/composition result submitted to the terminal backend.
- **Notification:** bounded user-facing status or recovery information rendered in the dashboard UI rather than written to the PTY.

A terminal tab should be modeled as a separate `Session` unless the chosen multiplexer semantics explicitly require a tab group to share one emulator. Separate sessions are the safer default because it guarantees PTY, scrollback, and graphics isolation.

## 3. Component layers

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

`AppState` also owns the coordinator-wide session-event bus
(`src/session_events.rs`). Terminal sessions publish bounded focus/title/line/
exit events, and script widgets subscribe through the shared
`WidgetRuntimeContext`; each subscriber gets a bounded queue (drop-oldest plus a
reported overflow diagnostic) and delivers events to its spawned script on fd 3
as `text` or `json` lines. The same bus carries the read-only session-context
snapshot that `session_env` exposes as `CMDASH_SESSION_*` at spawn. Events are
published from the coordinator thread (focus, line, exit) or routed through a
`UiEvent` (title), never written into a terminal PTY.

Key dispatch is owned by a single coordinator path. `AppState` holds a
validated `Keymap` built from the `[keybindings]` configuration (falling back
byte-for-byte to the legacy defaults when omitted). Input events are translated
into the existing `Command` values through that keymap, so the API and any
future transport share the same validation path. Terminal key capture is
resolved from the same keymap: inside a focused shell, only the configured
`focus_next`/`focus_previous` bindings are intercepted and every other chord is
forwarded to the PTY. The keymap is rebuilt on configuration reload, and the
in-app help/palette text renders the currently active bindings rather than
hardcoded defaults. Chords are backend-neutral (`ctrl`/`alt`/`shift` key
names), never raw escape sequences or crossterm-only codes.

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

State ownership:

```text
AppState
├── workspaces: WorkspaceState
├── focused_surface: SurfaceId
├── widget_registry: WidgetCatalog
├── sessions: SessionRegistry
├── keymap: Keymap
└── backend_capabilities: Capabilities

SessionState
├── pty_handle / child lifecycle
├── terminal_emulator
├── scrollback and viewport state
├── selection and input mode
├── graphics_store: SessionGraphicsStore
└── render_cache / dirty regions
```

### 3.6 Widget API and extension boundary

Widgets use a small lifecycle-oriented interface:

- receive application/runtime messages;
- update their own state;
- handle input when focused;
- measure or accept a `Surface` rectangle;
- produce a backend-neutral `Scene`;
- optionally request timers, redraws, or external capabilities.

There are two concrete implementations: `terminal` (a live PTY session) and
`widget` (a spawned shell script). Custom in-process widgets implement the
public `Widget` trait and register a factory in the `WidgetRegistry`; see
[CREATING_WIDGETS.md](CREATING_WIDGETS.md).

The opt-in Wasmtime host (behind `wasm-plugins`) is a dormant foundation rather
than the product's extension model. It rejects all module imports, does not
link WASI, configures fuel accounting, and bounds module size; future host
functions must be added explicitly to the manifest capability set. It must not
expose `stdout`, raw terminal escape sequences, or a concrete terminal backend.

Widget categories can include:

- pure display widgets (clock, CPU, logs, metrics);
- interactive widgets (file browser, command palette);
- session widgets (terminal emulator surfaces);
- container/layout widgets (split, tab group, overlay).

Container widgets compose child widget instances rather than reimplementing
their behavior.

### 3.7 Widget runtime helpers and scheduling

The widget runtime keeps rendering, data, and scheduling concerns separate:

- **Shared helpers** in `src/widget.rs` implement bounded severity styling,
  bordered surfaces, `key: value` rows, progress bars, sparkline normalization,
  and horizontal rules. Widgets reuse them instead of duplicating
  widget-specific rendering, and each helper clips to its area and defines
  minimum-size behavior.
- **Provider/render separation** keeps data sources distinct from `Scene`
  output. A script widget recomputes its stdout in `update(now)` and returns
  `WidgetUpdate::Redraw` only when it changed; terminal widgets consume session
  output through their owned `TerminalSession`. A data provider must be
  testable without an interactive terminal and must not outlive its owning
  instance.
- **Scheduling** is coordinator-owned. `WidgetRuntime::update` runs on the UI
  maintenance tick; widgets do not poll or spawn timers. Only instances with an
  assigned, visible area are rendered, so hidden or inactive widgets do not
  produce scene work.
- **Lifecycle ownership** is per instance: factories construct state, the
  runtime runs `initialize`, `update`, input, `resize`, and `shutdown`, and a
  widget error degrades or fails only that instance's health rather than the
  dashboard, backend, or another widget.

The authoring contract is documented in [CREATING_WIDGETS.md](CREATING_WIDGETS.md).

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

The emulator also owns the Kitty keyboard protocol negotiation: its
`kitty_keyboard` config is enabled so `CSI >`/`=`/`<` push/set/pop requests
update a per-screen mode stack and `CSI ? u` queries are answered through the
same PTY-response path as DA1. `key_bytes` consults `term.mode()` when
forwarding input, so a child that opts in receives disambiguated
`CSI number ; modifier u` encodings for modified and ambiguous keys while
legacy programs keep the C0/ESC/`CSI ~` encodings. Text presentation
attributes are first-class scene data too: `CellStyle` carries italic,
underline style/color, strikeout, reverse, and hidden alongside bold/dim, the
session render path maps them from the emulator's cell flags, and the backend
serializes them as SGR (`3`, `4:x`, `58`, `9`, `7`, `8`), leaving degradation
to the outer terminal's own SGR handling rather than discarding the attribute
from the scene model.

Input and output protocol surface is honored the way a real terminal honors
it. `mouse_bytes` gates on the emulator's negotiated mouse mode: nothing is
emitted without `?1000`/`?1002`/`?1003`, presses alone are reported under
`?1000`, drags and releases additionally under `?1002`, button-less motion
under `?1003`, and the encoding follows `?1006` (SGR), `?1005` (UTF-8), or
legacy X10, with releases encoded as button 3. When the child owns the mouse
the terminal's own selection is suppressed so events pass through verbatim,
and focus reporting (`?1004`) forwards `CSI I`/`CSI O` on focus transitions
through the widget runtime. OSC 8 hyperlinks are retained per cell by the
emulator and exposed as `hyperlink_at`/`selected_hyperlink`, so a copied
selection over a link surfaces the target URL rather than its display text.
Synchronized output relies on the vte parser's built-in BSU/ESU buffering
(`CSI ? 2026 h/l`), which already flushes a burst atomically on the ESU; the
session additionally enforces the parser's 150 ms sync timeout so a burst
whose ESU never arrives cannot strand output in the grid or its scroll
observer.

Capability advertisement, the clipboard, bells, and shell notifications round
out the session surface. `TERM` is configurable per terminal (`settings.term`,
default `xterm-256color`; `xterm-kitty` opts programs into the negotiated
protocols) and the emulator answers DA1/DA2, while the session intercepts
XTVERSION (`CSI > q`) and replies with a `DCS > | cmdash <version> ST`
identity. The emulator's OSC 52 handling is enabled (`Osc52::CopyPaste`): a
child store and the terminal's own selection both populate a session-shared,
byte-bounded clipboard cache (and the backend submission queue), and a child
load defers to an outer-terminal system-clipboard query. The backend emits
`ESC ] 52 ; c ; ? ST`, the raw-input owner demultiplexes the host's base64
answer apart from keyboard input, and the decoded text is delivered back to
any session with a pending read; if the host does not answer within the read
timeout (or no frontend is attached), the session falls back to the cache. `BEL` becomes a bounded, deduplicated frontend diagnostic, and
OSC 9/777 notifications are parsed from the plain output stream into truncated
frontend diagnostics (OSC 133/1337 markers are recognized but ignored). These
session-to-frontend events ride the existing `UiEvent` channel, so sessions
spawned without a frontend (tests) simply drop them.

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

### 5.3 Virtualized image buffer (Workstream 8)

`src/virtual_buffer.rs` models images as first-class citizens of a per-session
**virtual buffer** that owns text rows and image objects together. `VirtualBuffer`
holds ordered `VirtualRow`s (each with the set of attached image objects),
`ImageObject` (a resource plus its placements), and an `ImageIdentityRegistry`
that owns the child's `i=`/`I=`/`P`/`Q` identities. Buffer mutations — create,
delete, scroll, insert-lines, and limit eviction — produce a coalesced
`GraphicsCommand` stream (`Upload`/`Place`/`Delete`) that backend adapters
serialize, so the outer terminal's placement state is *mutation-driven* rather
than render-diff-driven. The buffer is wired into `SessionGraphicsStore`, which
mirrors mutations into the command stream while keeping its anchor + placement
projection as the render authority (Workstream 8 is complete).

**ratatui-image decision:** `ratatui-image` is *not* adopted for the
re-emission path. It is a client-side renderer — it queries the terminal,
transforms image data into protocol payloads, and manages stateful Kitty
placement/caching for images the *app itself* draws to its own terminal. It
cannot parse a child process's APC stream or act as a middleman re-emitting a
child's images to an outer terminal; the data direction is inverted for a
multiplexer. Its stateful patterns (upload-once/re-place, stable placement ids,
delete-on-remove, Unicode-placeholder cells) are already implemented in
`SessionGraphicsStore` + the backend adapters. Its client-side patterns are the
reference for the dashboard-owned image path, which is now implemented: a
script widget's `@@CMDASH_IMAGE` directive decodes to an RGBA dashboard image
and re-emits it as `f=32` Kitty graphics (or sixel), diffed and submitted
through the same retained-scene pipeline.

A retained placement also emulates a real graphics terminal's cursor movement:
after an image is placed the child emulator's cursor advances right by the
placement's `c` cells and down by its `r` cells, unless the client requested
`C=1`. Because the Kitty APC never reaches `alacritty_terminal`, the session
feeds the equivalent `CUF`/`CUD` movement back into both the emulator and its
scroll-region observer, so trailing text and consecutive images follow the
image instead of stacking on its top-left cell. A lowercase `a=t` transmits the
image data without displaying it, and the image appears only once a later
`a=p`/`a=T` placement arrives. When only one of `c`/`r` is given, the missing
extent is derived from the source image's aspect ratio. Sub-cell `X`/`Y` pixel
offsets are retained on the placement and re-emitted to the outer terminal in
direct mode so images land pixel-exactly; Unicode-placeholder mode stays
cell-granular and cannot express sub-cell offsets. Each placement also records
the cell pixel size and the on-screen pixel dimensions it is drawn at, so an
occlusion clip derives its source crop in pixel space rather than as a
whole-cell fraction of the image: a placement that starts partway into its
anchor cell is cropped from the correct source pixel, and the clipped placement
re-anchors with the sub-cell remainder of the original `X`/`Y` offset.

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

Every placement also carries a **stable outer-terminal placement id** (`p=`),
assigned by the store from a per-image monotonic sequence and keyed by the
placement's map key for its whole lifetime. Because Kitty matches placements by
`(i, p)` and its `grman_put` reuses an existing ref with that pair, a placement
that moves under scrollback navigation or reflow is re-placed with the *same*
`p=` id and the outer terminal relocates it instead of stacking a duplicate.
Deletes are emitted unconditionally (uploads are quiet `q=2`, so an
acknowledgement-gated delete could never fire and removed placements lingered as
ghosts at their old cells): a departed placement whose image still has other
visible placements is removed with a placement-scoped `d=i,i=X,p=P`, and the
last placement gets an image-level `d=i,i=X`. Verified against a real Kitty
(`tests/kitty_verify.py` drives the compiled emulator offscreen and inspects its
real `grman.update_layers`), lowercase `d=i` releases the placement but retains
the image data at the outer terminal, so the cached resource is kept and a
reappearance re-places with a bare `a=p` instead of re-uploading; only the
uppercase `d=I` frees the data. In Unicode-placeholder mode the virtual image
is kept alive for its remaining placeholder cells and only the departed cells
are cleared by the diff. The harness additionally replays the placeholder byte
streams against real Kitty's cell-image scan: the 24-bit-color lower id, the
0-based row/col combining marks, and the 1-based high-8-bits mark (Kitty's
`diacritic_to_num` returns index+1 and the decode subtracts one) reproduce the
correct image/source-rect binding, and tmux-passthrough framing is verified
lossless against the direct-mode bytes. Animation is cross-checked through
Kitty's coalesced-frame readback (`image_for_client_id`): `a=f` deltas, `a=c`
full/partial-rect overwrites, and alpha blending produce byte-identical frames
(alpha blending may differ by 1 in a low byte: Kitty truncates float math,
cmdash uses integer math), and the animated-GIF stream cmdash serves (coalesced
RGBA root + `a=f` frames with gaps) matches Kitty's frame data, gap, and total
animation duration. The text renderer maps grid lines to viewport rows
with alacritty's own `line + display_offset` translation, so history text stays
glued to the images anchored above it during scroll.

A bounded `GraphicsProtocolBroker` keeps child-PTY responses in a separate queue
from outer-terminal probe traffic. `GraphicsCapabilityProbe` emits a Kitty/DA1/
pixel-size probe, correlates only the outer Kitty acknowledgement, and reports
confirmed, rejected, or timed-out capability state. The session now delegates
APC framing to `GraphicsProtocolAdapter`, which incrementally handles 7-bit APC,
C1 APC, C1 ST, tmux passthrough unwrapping, malformed-sequence recovery, and
input/payload bounds before the retained store interprets parameters. Capability
metadata records whether support was inferred from the environment, explicitly
overridden, or actively probed, together with confidence. On Linux, one raw
`/dev/tty` input owner feeds the outer demultiplexer before decoding keyboard
input, so graphics acknowledgements cannot be consumed by a competing crossterm
reader. Direct replay reuses uploaded resources by generation, passthrough wraps
and ESC-doubles Kitty APCs for tmux-style hosts, and text fallback emits a
bounded degraded marker. Primary/alternate screen anchors, DECSTBM region
tracking, opaque scene occlusion, and cleanup generations are retained. Outer
resources now keep generation/acknowledgement state: removed resources wait for
the upload acknowledgement before deletion and are retired only after the delete
acknowledgement. Missing or failed outer acknowledgements are retried by the UI
coordinator with a fixed two-retry budget and a 250 ms deadline; cancellation
abandons unacknowledged work while accepted resources still follow delete
acknowledgement cleanup. Image and placeholder regions are first-class retained
scene layers; the compositor clips, orders, diffs, and occludes both before any
terminal-specific adapter emits bytes. A session-owned VT observer mirrors the
emulator's private margins and scroll displacement so partial-region linefeeds,
explicit scrolls, reverse index, origin mode, and resize resets move matching
graphics anchors without confusing them with primary-screen scrollback. A
column resize makes the emulator reflow (rewrap) text, which moves its
scrollback depth without scrolling content uniformly; the store re-anchors
full-screen placements by re-capturing each one's current grid row against the
new depth, so an image keeps its grid cell through the reflow the way Kitty
preserves a placement's `start_row`. Row-only resizes stay on the scrollback
model, and partial-region/relative placements keep their existing resolution.
The same observer also forwards screen-scoped erases to the store as
`GraphicsErase`: `ED 2` removes visible placements while preserving history
rows, `ED 0`/`ED 1` remove whole image rows from the cursor to the bottom/top
of the screen (row-granular, matching Kitty's `grman_remove_cell_images`),
`ED 3` removes scrollback-only placements, `RIS` clears everything, and screen
switches erase the alternate buffer. Erase scopes resolve against the
scrollback depth captured before the emulator consumes each chunk, so a clear
cannot re-anchor visible images into history nor resurrect one that `ED 3`
just removed; pixel data is retained on all of these except `RIS`, mirroring
Kitty's re-display cache.

The protocol store accepts zlib-compressed direct payloads and the local
`kitten icat` fast path — `t=f` file, `t=t` temporary-file, and `t=s` POSIX
shared-memory transfers resolve a base64-encoded path (bounded to 2048 bytes)
through the `S` size / `O` offset keys, read the range into memory, apply the
`o=z` zlib step, and enforce the decoded-storage budget. `t=s` unlinks the
shared-memory name after reading, and `t=t` deletes the file only when its name
carries the `tty-graphics-protocol` marker (Kitty's own temp-file convention),
so a program cannot use it to delete an arbitrary path; reads are same-user
local opens, so no remote/SSH permission hook is modeled. It normalizes
retained
payloads for safe replay, preserves source crops and explicit cursor policy,
tracks bounded animation frames/control state — including `a=c` frame
composition (source rectangle `X`/`Y`, destination rectangle `x`/`y`, shared
`w`/`h` size, and the `C` alpha-blend/overwrite mode), the `a=f` frame
composition keys (`c` base frame, `r` frame to write/edit, `X`
blend/replace, `Y` background canvas, and the `x`/`y`/`s`/`v` partial
rectangle), and the `v` loop-count control key. A new `a=f` frame is stored
as a delta (rectangle plus base/canvas metadata) and coalesced on demand;
editing an existing frame coalesces it, composes the new rectangle on top, and
stores the result as a full keyframe, mirroring Kitty's
`get_coalesced_frame_data` chain resolution. `a=c` coalesces both source and
destination frames before composing so a delta frame contributes its rendered
pixels. A non-raw (`f=100`) PNG/GIF frame is decoded to RGBA8 on coalesce via
the `png`/`gif` crates so `a=c` composes it on pixels instead of rejecting it,
and composing onto a PNG/GIF root converts the resource to raw RGBA (`f=32`)
since its stored bytes are now decoded pixels. An `f=100` animated GIF is
decoded on transmit into coalesced
full-canvas RGBA frames (the root plus one animation frame per extra GIF frame,
with per-frame delays and the Netscape loop count mapped onto `v`), so a single
GIF upload animates like a graphical terminal; static GIFs remain static
`f=100` images.

Animations actually play on screen. The store advances each animatable image on
the wall clock (`advance_animations`), skipping gapless frames, wrapping the
root→extra-frame sequence, and honoring Kitty's state/loop semantics — a
`Loading` animation plays through once and stops at the wrap, while `Running`
loops per the `v` key (`v`/`v-1`/infinite). The render path serves the
coalesced current frame's base64 payload with a generation that bumps on every
frame change, so the outer terminal re-uploads the new pixels rather than
re-placing the stale root frame. A maintenance-wakeup schedule (derived from
each widget's `advance_graphics_animation`, threaded through the widget runtime
and `AppState`) wakes the render loop at the next frame deadline. It implements
the full
image/placement delete matrix (`a/A`, `i/I`, `n/N`, `c/C`, `p/P`, `q/Q`, `r/R`,
`x/X`, `y/Y`, `z/Z`, `f/F`), honoring Kitty's lowercase/uppercase retention rule
(lowercase releases placements but keeps decoded pixel data for re-display;
uppercase also frees data once no placement — including scrollback — still
references it). Position/cell/z-index selectors resolve each placement's
current cell against the scrollback view and are scoped to the active screen,
and `d=f` deletes a single frame via `r=<frame>` (0/absent = the root frame),
renumbering the extras, rebalancing the animation gap schedule, and adjusting
the current-frame index — deleting the root promotes the first extra frame,
while deleting the whole frame set frees the image data and resets playback.
Relative placements (`P`/`Q` parent, `H`/`V` signed cell
offset) are stored as a parent reference and re-resolved against scroll/region
state at render time, so they follow their parent through scrolling and are
removed with it; parent chains are cycle-checked (`ECYCLE`) and depth-bounded
(`ETOODEEP`, at least 8). Virtual placements (`U=1`) are stored as invisible
prototypes: they never render, never move the cursor, never scroll or
re-anchor on resize, and can be a relative placement's parent but cannot
themselves be relative (`EINVAL`). Their delete-selector scope matches Kitty's
`is_virtual_ref` — only `i/I`, `n/N`, and `r/R` remove them, while the
position/z-index/visible selectors and the screen-erase scopes skip them
because they have no physical location. A relative placement anchored to a
virtual parent resolves its origin from the min column / min row of the parent's
U+10EEEE placeholder cells rather than the creating cursor: the session scans
the child's text grid for placeholder glyphs (decoding the image id from the
foreground RGB plus the third combining mark) and feeds those cells to the
store, and a relative child of a virtual parent with no placeholder cells yet
is invisible, matching Kitty's `resolve_cell_ref`. Image numbers (`I` key) allocate a fresh internal id
on transmit and resolve to the newest surviving image with that number, so a
client can pipeline `I`-addressed commands before learning the assigned `i`;
`i` and `I` are mutually exclusive (`EINVAL`). The `N=1` transient usage hint is
stored per frame (the root frame on the resource, each extra frame on its
animation frame): `a=f` deltas inherit their base chain's transient status,
frame edits OR the coalesced chain with the transmitted hint, and `a=c` marks
the destination transient when either source frame is transient, while eviction
still keys off the root frame's hint like Kitty. When an upload would exceed
the decoded-byte quota the store evicts in Kitty's order — unreferenced images
first, then transient before retained, then oldest by generation — recording a
diagnostic before falling back to rejection. File, temporary-file, and shared-memory
transfers (`t=f` file, `t=t` temp file, `t=s` shared memory) are
capability-negotiated and supported: the payload decodes to a bounded path
(2048 bytes), the file is read with `S`/`O` size/offset bounds against the
decoded-storage budget, `t=s` unlinks the shared-memory name after reading, and
`t=t` deletes the file only when its name carries the `tty-graphics-protocol`
marker, mirroring Kitty's conventions for the `kitten icat` fast path. Query
commands (`a=q`) load and validate their payload exactly like a transmit
(`handle_add_command` with `is_query=true`): they require an `i=` image id
(else a diagnostic and no response), resolve the transfer medium, check the
format, enforce Kitty's raw `bpp * s * v` data-size match and the 10000
dimension cap, and require a parseable GIF/PNG header for `f=100` — replying
`OK` only when the image would load and retaining nothing afterwards. The
backend's direct and placeholder adapters
serialize only direct payloads, while tmux passthrough wraps the same bytes with
ESC doubling. Capture fixtures feed those complete streams into a bounded
headless terminal model and assert both acceptance state and protocol responses.
Deterministic pressure fixtures also replay large chunked RGB payloads through
an actual one-pixel-per-cell framebuffer, alternate stable placement IDs across
rapid pane-like switches, redraw unchanged Unicode placeholders without
re-uploading resources, and repeat acknowledgement-gated cleanup while asserting
retained byte/resource counts remain within configured limits. The incremental
adapter also accepts payload-less control APCs used by Kitty animation/deletion
commands, and the session carries zero-ID continuity to the most recently
allocated image for subsequent frame/control actions. A bounded
raw PTY capture is available for conformance diagnostics without becoming a
second rendering source; installed-`kitten` fixtures use it to verify real
placement, placeholder, passthrough, animation, and failure streams. Session
resize, alternate-screen/scroll-region tracking, hidden-surface composition,
overlay occlusion, reload/close ownership, and shutdown cleanup are validated
before an outer adapter reports a rendered result.

This is why a global image map or a single terminal emulator shared by tabs is explicitly out of scope.

### 5.4 Composited grid with per-cell image references (Workstream 9)

`src/scene.rs` treats the *displayed* grid — the retained composed scene, not
the child emulator's grid — as the place where images are anchored. A parallel
`image_refs` array (one `u32` per cell, parallel to `cells`) records which
image/placeholder layer covers each cell, with `0` meaning none and nonzero
values resolving into the frame's `image_layers`/`placeholder_layers` via a
kind bit. `Scene::annotate_image_cells` re-stamps the covered cells after every
composition (full-redraw and partial paths), applying layers in z order so the
*topmost* covering layer wins per cell; the layer lists still carry every
placement for z-stacking at the outer terminal. Keeping the references out of
`Cell` means the text-span diff, span grouping, and cell equality are
untouched — image coverage is a property of the grid, mirroring how Kitty
anchors every image to the cell grid (`grman_scroll_images`) and how Termux
stores a bitmap reference inside each covered cell's style.

The grid owns the scroll/erase vocabulary: `scroll_region`/`scroll_rows`
(positive = content moves up, matching `record_scroll`), `insert_lines`/
`delete_lines`, `erase_rows`/`erase_region`, and `clear` move cells *and*
their references in lockstep as row-slice memmoves. `build_diff` computes a
`GridGraphicsDiff` (`appeared`/`removed`/`moved`, keyed by stable
resource + placement identity rather than frame-local handles) whenever the
frame's layers change, carried on `FrameDiff` and scratch-pooled. The layer
lists remain the emission authority; the grid diff is the verification layer
that proves the emission path agrees with what the cells actually display.
The remaining phases (grid-driven scroll/reflow as real mutations, selection
through the reflow map, and deleting the projection layer) are tracked in
Workstream 9 on the roadmap.

**Why not Termux's cell-split model?** Termux's graphics support (the long-open
Sixel/iTerm2 PR and the Kitty follow-up built on it) renders images by
*rasterizing them into the text grid*: each covered cell's packed style
encodes `(bitmap number, slice x, slice y)`, the full decoded bitmap is cached
per image, and the renderer draws the cell-sized slice with
`Canvas.drawBitmap` instead of text. Because images live inside the same rows
as text, scrolling, erasing, and resize reflow are *atomically* correct — the
same reason Kitty's `INDEX_GRAPHICS` shifts placements with the grid — and a
placeholder-style path in cmdash (Unicode placeholder glyphs written into the
text grid) inherits that atomicity for free. That model is unreachable for
cmdash's direct mode, which drives an *external* terminal: there is no canvas
to draw onto, so images must be commanded (`a=p`/`a=d`) rather than baked into
cells, and the scroll accounting must be mirrored (Workstream 8) instead of
being a side effect of moving rows. Termux's approach also trades expressiveness
for that atomicity: cell-granular only (no sub-cell `X`/`Y` offsets, no z-index,
no placement lifetime), it retains one full decoded bitmap per image with a
known OOM failure mode on large payloads, and its interrupted-sequence handling
can wedge the emulator. cmdash keeps the grid anchoring (Workstream 9) while
retaining the placement model's pixel-exact offsets, z-order, animations, and
bounded resource policy — accepting the sub-frame two-flush lag that is
inherent to driving an external terminal.

### 5.5 Other graphics protocols

The scene model is Kitty-first, behind a capability-aware adapter boundary. Kitty graphics are re-emitted protocol-faithfully; sixel is an opt-in dashboard path. Text and layout remain correct when graphics are unavailable, and protocol handling belongs behind the capability-aware adapter rather than in dashboard widgets.

The graphics-state adapter sits next to the mature `alacritty_terminal` parser/emulator and has conformance tests based on captured escape sequences. The opt-in sixel path uses the same retained scene boundary: dashboard submissions are clipped, diffed, and emitted only after backend capability negotiation.

### 5.6 Resource policy

Graphics are retained in memory while a session is alive, whether it is visible
or hidden, subject to per-session limits:

- maximum decoded bytes: 4 MiB;
- maximum resources: 256;
- maximum placements: 1,024;
- bounded diagnostics for rejected or oversized payloads;
- eviction (unreferenced first, then transient before retained, then oldest by
  generation) when an upload would exceed the decoded-byte quota;
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

Widgets must not query the terminal or write palette escape sequences directly. The backend translates the scene's color representation into
Crossterm color commands, preserving the serialized frame boundary.


Use three representations:

1. **Widget/session model:** mutable state and protocol semantics.
2. **Scene:** immutable frame-local primitives such as cells, spans, borders, rectangles, image placements, placeholder regions, and overlays.
3. **Backend submission:** terminal-specific cursor movement, color encoding, clear operations, and graphics escape sequences.

Image and placeholder layers are diffed as part of `FrameDiff`; stale physical image IDs are explicitly deleted before visible current layers are replayed. A frame diff carries changed, currently visible, and removed image submissions plus independently clipped placeholder regions, so a placeholder adapter can clear old cell regions before text output and restore only the composed, non-occluded result afterward.

The scene should carry clipping and ownership metadata. Every image placement should include its owning `SessionId` or a derived resource namespace so the compositor can reject cross-session references during development.

The backend targets a single local terminal but keeps these concerns separate. The interaction model prioritizes retained terminal tabs. Configuration-driven horizontal and vertical pane splits are supported, and the command layer creates new terminal sessions, provides directional pane focus, ratio adjustment, merge/close lifecycle operations, and persists mutable layout state through safe reload while preserving retained session ownership:

- terminal input/output and raw mode;
- layout and cell rendering;
- graphics protocol submission;
- capability detection.

The adopted crates are cataloged in [Dependencies](DEPENDENCIES.md). In summary:

| Concern | Direction |
| --- | --- |
| Terminal I/O and raw mode | `crossterm` |
| Layout primitives | `ratatui` `Rect` behind the retained scene boundary |
| Async model | standard-library threads + channels (no `tokio`) |
| PTY management | `portable-pty`, with narrow `libc` adapters |
| Escape parsing / emulation | `alacritty_terminal`, one instance per session |
| Kitty/image output | cmdash-owned session adapter and retained `Scene` image layers; zlib decoding is isolated to bounded direct payloads, while the optional sixel encoder/submission path remains dependency-free |
| Plugins | opt-in Wasmtime host with no imports (dormant; script widgets are the extension model) |
| Errors | `thiserror` |
| Config/serialization | `serde` + `toml` |

Avoid adding a crate solely to bypass a small, well-tested adapter boundary,
and do not let a graphics or plugin helper become a global state owner.

## 7. Concurrency and lifecycle

The UI/coordinator task owns `AppState` and all frame composition. Per-session I/O tasks read from PTYs and send bounded messages containing output bytes, resize acknowledgements, and process events. Widget workers may send state updates but cannot mutate UI state directly.

Important lifecycle behavior:

- PTY output is backpressured or batched to avoid starving input and rendering.
- API requests are bounded, batched, and rejected on queue/size limits rather than blocking frame submission.
- Resize events update the session emulator and PTY dimensions in a defined order.
- Closing a session cancels its I/O task, waits for child cleanup, then releases graphics resources.
- Shutdown restores terminal modes even after a panic/error path where the backend supports it.

## 8. Modularity strategy

Optional host functionality is gated behind compile-time cargo features:

- `sixel`: the bounded 16-color dashboard RGB encoder;
- `image`: JPEG/BMP decode for script-widget images;
- `watch`: event-driven config reload-on-save;
- `wasm-plugins`: the dormant Wasmtime isolation host.

The default build includes none of them and remains capability-aware. The
extension model is the script `widget`: a workspace config chooses which
instances are present, and each script's lifecycle (spawn, bounded output,
restart backoff, shutdown) is owned by the widget runtime. In-process widgets
register factories in the `WidgetRegistry`. The dormant Wasmtime host validates
import-free manifests and modules, rejects unsupported ABI/API versions and
capabilities, and cannot write directly to the terminal backend.

The configuration contract is `cmdash.workspace` v1, with a string-valued
widget settings map; legacy/missing config versions can be rewritten and are
reported through explicit migration warnings. Runtime failures can be written
as bounded reproduction artifacts when `CMDASH_CRASH_DIR` is configured.

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
- a bounded headless Kitty stream model that semantically parses APC/CSI/SGR,
  tmux unwrapping, chunk reassembly, resources, placements, placement-ID
  replacement, z-order, placeholder references, viewport clipping, z-index
  occlusion, animation frames and `a=c` composition (including decoding
  non-raw PNG/GIF roots to RGBA), virtual-parent origin from Unicode
  placeholder cells, randomized chunk boundaries, malformed
  sequences, bounded input rejection, and delete-acknowledgement acceptance;
- an optional one-pixel-per-cell headless RGB framebuffer that decodes bounded
  RGB/RGBA fixtures, applies crops, alpha blending, clipping, z-order, deletion,
  and placeholder pixels, including a PTY-to-outer-stream acceptance fixture;
- acknowledgement-routing tests for upload success/failure, deferred deletion,
  delete acknowledgement, resource retirement, bounded retries/cancellation, and
  graphics metrics;
- fuzz targets and retained seed corpora for TOML migration, plugin manifests, Kitty APC chunking, and sixel encoding;
- pane lifecycle tests for independent PTYs, nested layout persistence, and safe reload;
- release archive, checksum, feature-variant, and startup checks on tagged builds;
- API wire, authorization, queue, snapshot-generation, Unix-socket, and subscription tests.

A key regression test: write a Kitty image with ID `1` in tab A, write a different image with ID `1` in tab B, switch A → B → A, and verify that each tab restores its own image without cross-contamination.
