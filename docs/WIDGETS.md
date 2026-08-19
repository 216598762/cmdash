# Widgets

Widgets are cmdash's composable units of behavior and rendering. A workspace is
made from widget instances placed into a layout tree. Every dashboard item is
exactly one of two types:

- a **`terminal`** — a live PTY session with its own emulator, scrollback,
  selection, and graphics state;
- a **`widget`** — a shell script whose stdout renders into the surface.

A `widget` is a script run directly by the dashboard, not a compiled plugin. The
configured `command` is spawned through `/bin/sh -c`, its stdout feeds a bounded
output ring rendered into the surface, its stderr becomes a bounded diagnostic,
and its lifecycle (spawn, read, restart, reap, kill) is owned by the widget.
Script output wakes the same coalescing `SessionWakeup` as terminal PTY readers,
so widgets coexist with active sessions on one frame loop. Scripts may opt into
read-only session context (`CMDASH_SESSION_*` at spawn) and a bounded
session-event bus: terminal sessions publish focus/title/line/exit events, and
subscribing widgets receive them as newline-delimited `text` or `json` lines on
the script's fd 3.

The former compiled data widgets (`text`, `clock`, `system`, `status`,
`key_value`, `gauge`, `list`, `log`, `sparkline`, `separator`, `spacer`) have
been removed; existing configurations migrate them to equivalent `widget`
scripts on load.

This page documents the widget contract and runtime behavior. For the complete
TOML schema and configuration discovery rules, see
[CONFIGURATION.md](CONFIGURATION.md). For the ownership and frame-composition
design, see [ARCHITECTURE.md](ARCHITECTURE.md). To implement, register, test, or
distribute a widget, see [CREATING_WIDGETS.md](CREATING_WIDGETS.md).

## Widget terminology

cmdash keeps these concepts separate:

- **Widget type:** an implementation identified by a string (`terminal` or
  `widget`).
- **Widget instance:** one configured use of a widget type, identified by a
  numeric `id`.
- **Surface:** the rectangular area assigned to an instance by the layout tree.
- **Scene:** retained, backend-neutral cells and image layers produced for a
  surface during rendering.
- **Session:** state owned by a stateful widget. A terminal widget owns one PTY,
  emulator, selection, and graphics store.
- **Layout node:** a leaf, split, tab group, stack, or overlay reference that
  determines visibility and geometry.

A widget ID identifies the configured instance, not merely the implementation.
IDs must be unique across `workspace.widgets`, and layout leaves refer to these
IDs.

## Quick start

The checked-in [default configuration](../config/default.toml) demonstrates a
clock script, a system-info script, a terminal, an overlay, tabs, and columns. A
minimal custom workspace is:

```toml
version = 1

[workspace]
name = "overview"

[[workspace.widgets]]
id = 1
type = "widget"
title = " clock "
command = "date +%H:%M:%S"

[workspace.widgets.settings]
mode = "interval"
interval_ms = "1000"

[[workspace.widgets]]
id = 2
type = "terminal"
title = " shell "
command = "/bin/sh"

[workspace.layout]
type = "columns"
children = [
  { type = "leaf", widget = 1 },
  { type = "leaf", widget = 2 },
]
```

A widget that is configured but not reachable from the layout is still created
and validated, but it has no visible surface and does not contribute a scene.
Every layout leaf must refer to an existing widget ID.

## Using widgets day to day

Most users only need two files:

- edit `config/default.toml` or a copied user configuration to choose widget
  instances and their layout;
- use this page when a widget's runtime behavior, input, graphics, or lifecycle
  matters.

A practical cycle is:

1. Start cmdash with `--config <path>` so `Ctrl+R` can reload the file.
2. Focus widgets with `Tab` / `Shift+Tab`, or move between panes with `Alt+Arrow`.
3. Use `?` and `Ctrl+P` to discover commands without memorizing the keymap.
4. Edit the TOML file, save it, and reload with `Ctrl+R`.
5. Keep the diagnostic footer visible while testing a new widget or layout.
6. If a change is rejected, fix the file and reload again; the previous valid
   runtime remains active.

The runtime does not automatically save pane splits, widget IDs, or ratio
changes back to TOML. Treat the file as the source of truth for the next process
start, and copy desired runtime layout changes into it manually.

For schema fields, discovery order, migration, and complete layout examples,
continue to [CONFIGURATION.md](CONFIGURATION.md). For architecture and scene
ownership, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Widget configuration

Each widget instance uses the common shape below:

```toml
[[workspace.widgets]]
id = 10
type = "terminal"
title = " shell "
label = "auto"
command = "/bin/sh"

[workspace.widgets.settings]
scrollback = "4096"
padding = "1"
border = "rounded"
```

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `id` | integer | yes | Unique instance and surface identity. |
| `type` | string | yes | `terminal` or `widget`. |
| `title` | string | no | Border/title text when the widget renders one. |
| `label` | string | no | `auto`, `always`, or `never`; controls whether the title is drawn. |
| `command` | string | widget | The shell command a `widget` runs (required for `widget`; optional for `terminal`, which falls back to the login shell). |
| `settings` | string map | no | Stable extension settings passed to the widget. |

Titles provide the border label and omitted titles use each type's built-in
default. The `label` policy is explicit: `auto` (default) follows normal title
behavior, `always` renders the title, and `never` renders no label while
preserving content geometry. An empty title is not required to suppress labels.

Keep widget-specific options in the string-valued `settings` map so the
configuration contract remains forward-compatible.

The configuration validator rejects duplicate IDs, empty types, invalid layout
references, empty layout groups, and unsupported configuration versions. A
widget factory may apply additional validation, such as a terminal's
`settings.term` value or a script widget's `mode`.

### Content padding and borders

Widgets that draw an outline support these optional string settings:

- `padding`: a non-negative number of additional cells between the border and
  the content area. The default is `0`.
- `border`: `rounded` (default), `square`, `double`, `heavy`, `ascii`, or
  `none`. `border_style` is accepted as a compatibility alias.
- `border_color` and semantic role names such as `foreground`, `background`,
  `focus`, and `muted`: `inherit`, `ansi:N`, or `#RRGGBB`.

Motion and terminal cursor settings are documented in
[ANIMATION.md](ANIMATION.md); they do not change the widget content geometry.

The configured appearance controls the widget's content rectangle. Terminal PTY
size, terminal graphics, selection, mouse routing, and resize handling all use
that rectangle, so increasing padding cannot cause terminal output to overlap
the border. `border = "none"` removes the outline while retaining configured
padding.

For example:

```toml
[[workspace.widgets]]
id = 12
type = "widget"
title = " deploy "
command = "echo 'production: healthy'"

[workspace.widgets.settings]
padding = "2"
border = "double"
```

## The two item types

### `terminal`

`terminal` is the stateful session widget. Each instance owns an independent
`TerminalSession`, including:

- PTY and child-process lifecycle;
- terminal parser and emulator grid;
- alternate-screen and cursor state;
- scrollback, selection, and paste handling;
- terminal size and input modes;
- Kitty graphics resources and placements;
- render cache and graphics diagnostics.

```toml
[[workspace.widgets]]
id = 4
type = "terminal"
title = " shell "
command = "/bin/sh"
```

If `command` is omitted, the session uses the platform's configured shell
fallback. The widget handles keyboard, mouse, paste, resize, selection copy,
and shutdown. It is the only widget type that owns a PTY.

The cursor is rendered by cmdash inside the terminal scene. Its optional
presentation blink and motion settings are documented in
[ANIMATION.md](ANIMATION.md); terminal emulator cursor modes remain authoritative.

Scrollback is navigated with the mouse wheel and `Shift+PageUp`/`Shift+PageDown`.
`Shift+Up`/`Shift+Down` and `Shift+Home`/`Shift+End` scroll history while no
selection is active and extend an existing selection once one exists (see
"Terminal selection and copy"). While history exists the terminal draws a
right-edge scrollbar (muted track, focus-colored thumb) and, when scrolled away
from the live screen, a percentage indicator in the title bar. Both are
theme-aware and can be disabled per terminal with `settings.scrollbar` and
`settings.scroll_indicator` (default `true`). History is bounded by
`settings.scrollback` (default `10000` lines); graphics that scroll past that
limit are evicted and their decoded bytes released.

A terminal widget is not a global terminal pane. Splitting it creates another
terminal widget and another session ID; the new pane inherits the source
configuration while retaining independent process and emulator state.

### `widget` (script widgets)

A `widget` runs its `command` through the user's shell and renders stdout into
the surface. This is the primary way to build dashboard content:

```toml
[[workspace.widgets]]
id = 5
type = "widget"
title = " git "
command = "git -C . status --short | head -n 12"

[workspace.widgets.settings]
mode = "interval"
interval_ms = "5000"
```

Script behavior is controlled entirely through `settings`:

- `mode`: `stream` (default) runs once and keeps reading stdout as it arrives;
  `interval` runs to EOF and re-runs every `interval_ms`.
- `interval_ms`: the re-run cadence for `interval` mode (default `1000`, bounded
  `100..=60000`).
- `render`: `text` (default). With `parse_tags = "true"`, each line is styled by
  its `[error]`/`[warning]`/`[success]`/`[info]` prefix, and the tag is
  stripped.
- `max_lines` (default `1024`) and `max_bytes` (default `65536`) bound the
  output ring; overflow drops the oldest lines and records a diagnostic.
- `restart` (`true` default) restarts an exited script with bounded exponential
  backoff; repeated immediate exits escalate to `Failed` health.
- `handles_input` (`false` default) forwards focused keys to the script's stdin.
- `session_env` (`true` default) exposes `CMDASH_WIDGET_ID`,
  `CMDASH_WIDGET_TITLE`, `CMDASH_SURFACE_COLUMNS`, `CMDASH_SURFACE_ROWS`,
  `CMDASH_SESSION_COUNT`, `CMDASH_FOCUSED_SESSION`, and `CMDASH_FOCUSED_TITLE`
  at spawn (a read-only snapshot taken at spawn time).
- `session_events`: `off` (default), `text`, or `json` — subscribes the widget
  to bounded terminal-session events (focus, title, line output, exit)
  delivered as newline-delimited lines on the script's fd 3.

Example scripts ship in `config/widgets/` (clock, uptime, git status, log
tail). Any program that prints lines on stdout works; the dashboard only
interprets the optional image directive described under
[Script-widget images](#script-widget-images-optional-image-feature).

Because a script is an ordinary process, its output is inherently bounded by the
ring above and its failure is isolated to that widget — it cannot take down the
dashboard, the backend, or another widget.

## Lifecycle contract

The built-in runtime models widgets through a small lifecycle-oriented trait.
The relevant operations are:

```text
initialize() -> Result
update(now) -> Unchanged | Redraw
health() -> Healthy | Degraded(message) | Failed(message)
render(area, focused) -> Scene
graphics(area) -> image submissions
handle_key(key) -> Unchanged | Redraw
resize(size) -> Unchanged | Redraw
handle_paste(text) -> Unchanged | Redraw
copy_selection(area) -> optional text
handle_mouse(mouse, origin) -> Unchanged | Redraw
shutdown() -> Result
```

Not every widget needs every operation. Default implementations are no-ops:
passive widgets can render without accepting input, graphics, or shutdown
work. A widget should return `Redraw` only when its visible output needs to be
recomposed.

### Initialization

The runtime creates each configured instance through a registered factory and
calls `initialize` before the instance becomes active. Initialization errors
are associated with the widget type and prevent an invalid runtime from being
installed. For example, a terminal startup failure or an invalid script setting
is reported as a widget setup error.

### Updates

The application coordinator calls `update` with the current `SystemTime`.
Widgets update their own state and return whether rendering changed. The runtime
collects changed and failed IDs in a `WidgetUpdateReport`; failures move the
widget to `Failed` health and are surfaced through diagnostics rather than
written into a terminal PTY.

### Health

Widget health is deliberately separate from normal output:

- `Healthy` means the widget is operating normally.
- `Degraded(message)` means it is usable but an optional resource or capability
  was limited, such as an omitted graphics payload or a dropped ring line.
- `Failed(message)` means an operation failed and the widget may no longer
  produce reliable output.

The runtime keeps a bounded status set and summary. A failed widget should not
prevent unrelated widgets from rendering or receiving updates.

### Shutdown

The runtime shuts down widget instances when the application exits, a pane is
closed, or a configuration reload replaces the runtime. Terminal shutdown
cancels the session's I/O lifecycle and restores terminal state where possible.
Shutdown failures become diagnostics and do not get mixed into PTY output.

## Rendering model

A widget never writes terminal escape sequences directly. It returns a
backend-neutral `Scene` for the rectangle assigned by layout:

```text
Widget state
    │
    ├── render(area, focused) ──► cell scene
    ├── graphics(area) ─────────► Kitty image layers
    └── sixel(area) [feature] ───► sixel layers
                                      │
                                      ▼
                              compositor and backend
```

A `Scene` contains styled cells and optional retained image layers. Its drawing
operations are clipped to the scene area. Text uses Unicode display widths:
wide characters occupy a lead and continuation cell, zero-width characters do
not advance the cursor, and a wide character is not started if it would be
clipped at the right edge.

The `focused` argument lets a widget draw focus decoration without owning focus
policy. The application state chooses the focused surface; the widget only
renders the visual distinction.

### Surface geometry

The layout engine assigns a `Rect` to each visible widget. Supported layout
nodes include:

- `leaf`: one widget ID;
- `columns`: equal horizontal children;
- `split`: horizontal or vertical children with optional ratios;
- `tabs`: one active child while retaining inactive branches;
- `stack`: children composed in order;
- `overlay`: a reference to a configured overlay.

For example:

```toml
[workspace.layout]
type = "split"
direction = "vertical"
ratios = [70, 30]
children = [
  { type = "leaf", widget = 4 },
  { type = "leaf", widget = 1 },
]
```

A widget should render to the supplied area and should not assume the whole
terminal viewport. The scene and compositor enforce clipping so a child cannot
draw into a neighboring pane. The public `widget_content_area(area)` helper
returns the one-cell-inset rectangle for widgets that draw an outline.

### Widget outlines and terminal content

Widget borders occupy the outer edge of the assigned surface. Terminal content
is rendered into the configured content rectangle, inset by the border and
padding; with defaults this is one cell on every side. The terminal's PTY size,
graphics placements, selection coordinates, and mouse origin use that inner
rectangle as well. This keeps terminal text and cursor output from overwriting
the outline.

The layout system sizes widgets to their assigned surface; it does not provide a
separate general-purpose alignment option for centering a widget inside a larger
parent area. Splits, columns, and explicit layout geometry are the supported
positioning controls.

### Frame composition

The compositor renders visible widget scenes, orders surfaces and overlays,
clips them to the viewport, and diffs the result against the previous frame.
Only visible layout branches contribute output. Hidden tab sessions remain
alive and retain state, but their scenes and graphics are not submitted.

### Colors and theming

The default semantic theme uses terminal-native reset and ANSI references so
widget colors follow the parent terminal palette. Use `[appearance.colors]` for
workspace-wide role overrides and widget `settings` for per-instance overrides.
The complete role list, color syntax, precedence rules, border styles, and label
policy are documented in [APPEARANCE.md](APPEARANCE.md).

Optional retained-scene motion, transition triggers, and the coordinator-owned
scheduler are documented in [ANIMATION.md](ANIMATION.md).

This separation means widget code is independent of terminal cursor movement,
style caching, changed-cell grouping, output metrics, and backend capability
negotiation. Those concerns belong to the compositor and backend.

## Input and interaction

Input is routed through commands and application focus before it reaches a
widget. A widget receives input only when its surface is focused and the widget
reports that it handles input.

The current interaction path is:

```text
keyboard / mouse / paste
          │
          ▼
     command router
          │
          ▼
      AppState focus
          │
          ▼
   focused widget instance
```

### Focus

Focus is tracked by surface or overlay, not by widget type. `Tab` and
`Shift+Tab` cycle visible surfaces; `Alt+Arrow` performs directional pane
navigation. A widget can show a focus border, but it must not change focus by
itself.

Mouse button-down events first select the surface under the pointer. Subsequent
mouse events are passed to the focused input-capable widget when the pointer is
inside its area. Terminal widgets preserve terminal mouse reporting while also
updating cmdash selection state for drag selection.

### Terminal selection and copy

Selection is owned by the emulator grid (`alacritty_terminal`'s `Selection`)
rather than a hand-rolled viewport rectangle. Single-click+drag selects a
flowed range, double-click selects a semantic word, and triple-click selects a
whole line; the click count is bounded by a double-click window and a movement
threshold, and `Shift`+click extends the current selection. Selection points are
anchored to absolute grid lines, so a selection survives scrollback navigation,
and copy uses the emulator's own `selection_to_string` for correct wrap/newline
semantics. The highlight follows the flowed selection range with the theme
selection colors. When the child has enabled mouse reporting (or the alternate
screen is active), the events reach the child and no local selection is made.

Keyboard selection is available without a mouse: `Shift`+Left/Right extend the
tail one cell and `Shift`+Up/Down one line, while `Shift`+Home/End jump to the
line start/end, anchoring at the grid cursor when no selection exists. To avoid
breaking scrollback, `Shift`+Left/Right always select, `Shift`+Up/Down/Home/End
select while a selection is active and otherwise scroll history, and
`Shift`+PageUp/PageDown always scroll. The double-click window, semantic
word-break characters, and auto-scroll/copy behavior are configurable per
terminal via `settings` (see [CONFIGURATION.md](CONFIGURATION.md)).

For a focused terminal widget, key events are encoded for the PTY and paste is
sent through the session's bracketed-paste-aware path. `Ctrl+Shift+C` copies the
current selection through the backend's OSC 52 clipboard submission path; a
terminal with `copy_on_select` or `copy_on_release` enabled auto-copies its
finalized selection when the mouse button lifts instead. The copy notification
is kept in cmdash diagnostics/status state rather than sent to the shell.

Pane and application commands are handled before widget input when their key
bindings match:

| Binding | Action |
| --- | --- |
| `Ctrl+Shift+H` | Split the focused terminal horizontally. |
| `Ctrl+Shift+V` | Split the focused terminal vertically. |
| `Alt+Arrow` | Move focus directionally. |
| `Ctrl+Shift+Left/Right` | Grow or shrink the focused split ratio. |
| `Ctrl+Shift+W` | Close the focused pane. |
| `Ctrl+Shift+M` | Merge/remove the focused pane from its parent split. |
| `Ctrl+PageUp/PageDown` | Switch retained tabs. |
| `Ctrl+P` | Open the command palette. |
| `?` | Toggle help. |
| `Ctrl+R` | Reload the selected configuration. |

The palette and help overlay are application surfaces, not widget instances.

When a key appears to do nothing, check which surface is focused and whether
that widget accepts input. Application commands are handled before terminal
input; ordinary unbound keys are passed to a focused input-capable terminal.

## Graphics

### Kitty graphics

Kitty graphics are terminal-session state. A terminal widget's graphics store
uses a session-qualified resource identity, so image ID `1` in session A is not
the same resource as image ID `1` in session B. The store retains decoded
resources, encoded payloads, placements, z-order, and diagnostics.

A widget's `graphics(area)` output is clipped to its surface and passed through
the retained scene and compositor pipeline. The backend negotiates Kitty support
and submits only visible current layers. When a tab is hidden, its resources
remain associated with the hidden session but are not submitted; when it becomes
visible again, its scene can be rebuilt from retained state.

The store has bounded defaults:

- maximum decoded bytes: 4 MiB per session;
- maximum resources: 256;
- maximum placements: 1,024;
- bounded diagnostics for rejected or oversized payloads.

Unsupported formats, invalid payloads, and quota violations must produce a
controlled error or degraded diagnostic. They must not overwrite neighboring
cells or another session's image.

### Optional sixel

The `sixel` Cargo feature adds a dependency-free, bounded 16-color RGB encoder
for dashboard-provided images. A widget can return sixel submissions through its
feature-gated `sixel(area)` method. These layers are retained in `Scene`,
clipped, diffed, and submitted only when backend capability detection reports
sixel support.

Enable the feature with:

```text
cargo run --features sixel
```

Terminal-originated Kitty graphics and dashboard-provided sixel images are
separate paths. Enabling sixel does not change ownership of terminal graphics
or make sixel available to terminal PTYs.

### Script-widget images (optional `image` feature)

A `widget` script can emit an image on its stdout with a single directive line:

```text
@@CMDASH_IMAGE <base64>
```

where `<base64>` is standard base64 of a **JPEG or BMP** file. The `image` cargo
feature decodes the payload into RGBA and the widget surfaces it through the
same retained `Scene` image-layer pipeline as terminal graphics: when the outer
terminal advertises Kitty graphics the image is re-uploaded as raw RGBA (`f=32`);
otherwise it falls back to sixel (when the `sixel` feature is also compiled in).
A malformed directive is reported as a degraded widget diagnostic and the last
good image is kept. Enable both features with:

```text
cargo run --features image,sixel
```

The directive is consumed (not rendered as text), uses a stable dashboard
resource identity so re-emitted images replace in place rather than stacking,
and the image is deleted when the widget is hidden or closed. PNG/GIF payloads
are not accepted here: they belong to the terminal-originated Kitty protocol
slice (`f=100`), which keeps its own narrower `png`/`gif` decoders.

## Extension points

Widgets are registered in a `WidgetRegistry`. A factory receives a
`WidgetInstanceConfig` and a shared `&WidgetRuntimeContext`, then returns a
boxed widget. The context is the construction-time capability boundary for
services that a widget may need; factories should not reach into global state or
application internals.

The factory contract is:

```rust
fn factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError>
```

`WidgetRuntimeContext::new()` creates a context without optional services. The
context exposes the resolved `Theme` and the optional session wakeup, initial
terminal size, session-event bus, and Kitty-graphics capability. Applications
that run terminal sessions construct a context with
`WidgetRuntimeContext::with_session_wakeup(wakeup)` (and the other `with_*`
builders) and pass it to `WidgetRegistry::builtins_with_context`. The session
wakeup is optional so widget-only dashboards and custom passive widgets remain
usable without a PTY.

External in-process widgets use the same `Widget` scene contract. The public
`WidgetRuntimeContext::theme()` method provides the resolved semantic `Theme`;
`WidgetAppearance::from_settings` parses the common `padding` and `border`
settings; `WidgetAppearance::content_area(area)` gives the matching inner
rectangle; and `WidgetAppearance::render_border(...)` draws the selected
outline. The older `widget_content_area(area)` helper remains available for the
fixed one-cell legacy contract. The host clips the resulting scene to the
assigned surface. The in-process authoring guide is
[CREATING_WIDGETS.md](CREATING_WIDGETS.md).

### WASM isolation (dormant)

The optional `wasm-plugins` feature uses Wasmtime for import-free validation and
per-instance isolation: it bounds module size and fuel, creates a separate store
for each instance, and does not link WASI or terminal imports. This is a
compile-gated foundation, not the product's extension model — script widgets
are. The host-function ABI for lifecycle, input, and scene output remains a
future extension, so a plugin manifest should be treated as a validation
example rather than a promise that arbitrary WASM widget binaries are currently
executable.

## Panes, sessions, and persistence

Pane commands operate on focused terminal widget instances. A split:

1. verifies that the focused instance is a terminal;
2. allocates a fresh widget ID and session identity;
3. clones the source terminal configuration for the new widget;
4. starts an independent terminal session;
5. inserts a split node with initial 50/50 ratios;
6. focuses the new surface and invalidates the affected area.

Closing or merging a pane shuts down only the removed widget session, removes
its surface and configuration entry, and normalizes a parent with one remaining
child. The last visible pane cannot be closed. Shutdown failures are recorded as
bounded diagnostics.

Runtime changes mark the layout dirty and serialize the current layout tree,
split ratios, and active tab state into the in-memory configuration. During a
safe reload, valid runtime pane entries and the mutable layout are retained;
invalid replacement configuration is rejected without replacing the active
state. Focus is restored only when its surface still exists and is visible.

This gives every terminal pane independent PTY and graphics ownership while
allowing the surrounding workspace arrangement to persist across reloads.

## Troubleshooting a widget

- **The widget is not visible:** confirm its `id` appears in a reachable `leaf`,
  `columns`, `split`, or active `tabs` branch. A valid but unreachable widget is
  still initialized without receiving a visible surface.
- **A widget fails during startup:** check the diagnostic footer for its type and
  initialization error. For terminals, verify the command exists and that the
  layout gives the pane a non-zero area.
- **A key is ignored:** focus the widget, then check whether it is interactive.
  Script widgets accept keys only with `handles_input = "true"`.
- **A terminal copy does not reach the clipboard:** selection and OSC 52 depend
  on the surrounding terminal emulator's clipboard policy.
- **Images are missing:** Kitty and sixel are optional capability paths. Check
  terminal support, feature flags, clipping, and graphics quota diagnostics. For
  pane-safe Kitty rendering, use the default Unicode-placeholder mode or set
  `CMDASH_KITTY_GRAPHICS_MODE=placeholder`; use `direct` only when the outer
  terminal is known to support root-terminal placement semantics.
- **Reload loses a change:** configuration reload is validation-based and keeps
  the last valid runtime. Check TOML syntax, duplicate IDs, layout references,
  widget type names, and schema version.

## Failure isolation and diagnostics

Widget failures are local whenever possible:

- parse and validation failures reject a candidate configuration before it is
  installed;
- initialization failures identify the affected widget type;
- update/input/resize failures move the instance to failed health and request
  diagnostics;
- graphics quota failures degrade the graphics path without corrupting text;
- shutdown failures are reported while other widgets continue shutting down;
- application crash reproduction reports can be written to the directory in
  `CMDASH_CRASH_DIR`.

Diagnostics are deliberately separate from PTY output. This prevents an
internal error message from becoming shell input or changing terminal
scrollback.

## Developing a widget

The focused, step-by-step authoring guide — with complete examples, the factory
and runtime-context contract, scene rendering, lifecycle, registration, and
testing — is [CREATING_WIDGETS.md](CREATING_WIDGETS.md). The summary below is a
quick checklist rather than the full tutorial.

A custom widget implementation should:

1. Define a state struct containing only the data it owns.
2. Implement `kind` with a stable configuration type name.
3. Validate type-specific configuration in its factory or initialization.
4. Implement `update` and return `Redraw` only for visible changes.
5. Render only inside the supplied `Rect` and use `Scene` primitives.
6. Return `Healthy`, `Degraded`, or `Failed` deliberately.
7. Handle input only when `handles_input` returns `true`.
8. Release workers, sessions, and resources in `shutdown`.
9. Avoid direct terminal writes, global mutable state, and cross-session IDs.
10. Add tests for configuration, rendering, clipping, lifecycle, and failures.

A minimal passive widget has the following shape:

```rust
struct StatusWidget {
    title: String,
    text: String,
}

impl Widget for StatusWidget {
    fn kind(&self) -> &str {
        "status"
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let mut scene = Scene::new(area);
        // Fill, border, and text using scene primitives.
        let _ = focused;
        scene
    }
}
```

Register its factory in the widget registry, document the TOML fields, and add
at least one end-to-end configuration test through `WidgetRuntime`. A widget
that needs input, graphics, or external work should add tests for capability
fallback and failure isolation as well.

## Testing checklist

Useful widget and integration tests include:

- duplicate and missing widget IDs are rejected;
- unknown widget types fail before runtime installation;
- invalid type-specific fields produce actionable errors;
- passive widgets render within their assigned rectangle;
- Unicode-wide text does not escape or leave stale continuation cells;
- hidden tab branches produce no scene or graphics submission;
- focus changes affect decoration but not widget ownership;
- terminal panes have independent PTYs, emulator state, selection, and image
  namespaces;
- pane split, ratio adjustment, close, merge, and reload preserve valid state;
- graphics are clipped, quota-limited, and capability-aware;
- sixel output is tested separately under `--features sixel`;
- update, input, and shutdown failures stay local and become diagnostics.

The project also maintains fuzz targets and seed corpora for configuration
migration, plugin manifests, Kitty APC streams, and sixel RGB encoding.

## Related documents

- [Configuration reference](CONFIGURATION.md) — discovery, TOML fields, layouts,
  panes, overlays, migrations, and recovery.
- [Appearance guide](APPEARANCE.md) — semantic themes, inherited terminal
  palette colors, borders, labels, and per-widget overrides.
- [Animation guide](ANIMATION.md) — retained motion, cursor presentation,
  scheduling, accessibility, and lifecycle limits.
- [Creating widgets](CREATING_WIDGETS.md) — the focused authoring guide with
  examples, the factory/context contract, rendering, lifecycle, registration,
  and testing.
- [Architecture](ARCHITECTURE.md) — state ownership, scenes, compositor, and
  backend boundaries.
- [Dependencies](DEPENDENCIES.md) — dependency decisions and optional feature
  boundaries.
- [Roadmap](ROADMAP.md) — the staged implementation plan and completion record.
