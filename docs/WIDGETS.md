# Widgets

Widgets are cmdash's composable units of behavior and rendering. A workspace is
made from widget instances placed into a layout tree. A widget may be a passive
dashboard display, an interactive control, or a terminal session; terminal
sessions are not required for a dashboard to run.

This page documents the widget contract and runtime behavior. For the complete
TOML schema and configuration discovery rules, see
[CONFIGURATION.md](CONFIGURATION.md). For the ownership and frame-composition
design, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Widget terminology

cmdash keeps these concepts separate:

- **Widget type:** an implementation identified by a string such as `clock` or
  `terminal`.
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
text widget, clock, system widget, overlay, tabs, and columns. A minimal custom
workspace is:

```toml
version = 1

[workspace]
name = "overview"

[[workspace.widgets]]
id = 1
type = "text"
title = " welcome "
text = "cmdash is ready"

[[workspace.widgets]]
id = 2
type = "clock"
format = "HH:MM"

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
2. Focus widgets with `Tab` / `Shift+Tab`, or move between terminal panes with
   `Alt+Arrow`.
3. Use `?` and `Ctrl+P` to discover commands without memorizing the keymap.
4. Edit the TOML file, save it, and reload with `Ctrl+R`.
5. Keep the diagnostic footer visible while testing a new widget or layout.
6. If a change is rejected, fix the file and reload again; the previous valid
   runtime remains active.

The runtime does not automatically save pane splits, widget IDs, or ratio
changes back to TOML. Treat the file as the source of truth for the next
process start, and copy desired runtime layout changes into it manually.

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
text = "optional type-specific text"
format = "optional type-specific format"
command = "sh"

[workspace.widgets.settings]
scrollback = "4096"
profile = "default"
padding = "1"
border = "rounded"
```

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `id` | integer | yes | Unique instance and surface identity. |
| `type` | string | yes | Registered widget type. |
| `title` | string | no | Border/title text when the widget renders one. |
| `label` | string | no | `auto`, `always`, or `never`; controls whether the title is drawn. |
| `text` | string | no | Type-specific display text. |
| `format` | string | no | Type-specific display format. The clock uses this field. |
| `command` | string | no | Command used by a terminal instance. |
| `settings` | string map | no | Stable extension settings passed to the widget. Built-ins and the reference plugin support `padding` and `border`. |

Titles provide the border label and omitted titles use each widget's built-in
default. The `label` policy is explicit: `auto` (default) follows normal title
behavior, `always` renders the title, and `never` renders no label while
preserving content geometry. An empty title is not required to suppress labels.

Unknown fields are not a substitute for `settings`: keep widget-specific
options in the string-valued settings map so the configuration contract remains
forward-compatible. The current built-in factories use the common fields;
future widget types can define documented settings without changing the
top-level schema.

The configuration validator rejects duplicate IDs, empty types, invalid layout
references, empty layout groups, and unsupported configuration versions. A
widget factory may apply additional validation, such as the clock format check.

### Content padding and borders

Widgets that draw an outline support these optional string settings:

- `padding`: a non-negative number of additional cells between the border and
  the content area. The default is `0`.
- `border`: `rounded` (default), `square`, `double`, `heavy`, `ascii`, or
  `none`. `border_style` is accepted as a compatibility alias.
- `border_color` and semantic role names such as `foreground`, `background`,
  `focus`, and `muted`: `inherit`, `ansi:N`, or `#RRGGBB`.
- Terminal cursor blinking uses `cursor_blink` and
  `cursor_blink_interval_ms`; see the terminal widget section below.

Custom glyph sets, per-side visibility, and animated border/title styling remain
future extensions in Phase 12.

The configured appearance controls the widget's content rectangle. Terminal PTY
size, terminal graphics, selection, mouse routing, and resize handling all use
that rectangle, so increasing padding cannot cause terminal output to overlap
the border. `border = "none"` removes the outline while retaining configured
padding. Built-in text-like widgets retain their historical one-cell text
inset inside the content rectangle.

For example:

```toml
[[workspace.widgets]]
id = 12
type = "text"
title = " deploy "
text = "production: healthy"

[workspace.widgets.settings]
padding = "2"
border = "double"
```

## Built-in widget types

### `text`

`text` is a static display widget. It renders a filled, bordered surface and
places the configured `text` inside it. It is useful for labels, status notes,
static dashboard content, and testing a layout without starting a process.

```toml
[[workspace.widgets]]
id = 1
type = "text"
title = " deployment "
text = "production: healthy"
```

The title defaults to ` text ` when omitted. The text defaults to an empty
string. `text` does not handle keyboard or mouse input and does not create a
session.

### `clock`

`clock` displays the current UTC time derived from the system clock. Its
`format` may be either `HH:MM` or `HH:MM:SS`; the default is `HH:MM:SS`.
The widget requests a redraw when the displayed value changes.

```toml
[[workspace.widgets]]
id = 2
type = "clock"
title = " UTC "
format = "HH:MM:SS"
```

An unsupported format fails widget initialization rather than silently
rendering a different value. The title defaults to ` clock `.

### `system`

`system` displays the current operating-system and architecture identifiers,
for example `linux / x86_64`. It is a small diagnostic widget and does not
start a worker or session.

```toml
[[workspace.widgets]]
id = 3
type = "system"
title = " host "
```

The title defaults to ` system `. More detailed metrics are a future widget
extension; this built-in should not be treated as a complete monitoring
interface.

### Choosing a widget type

Use the smallest type that matches the job:

| Need | Type | Starts a process? | Accepts input? |
| --- | --- | ---: | ---: |
| Static text, labels, or notes | `text` | no | no |
| A UTC time display | `clock` | no | no |
| Basic host identity information | `system` | no | no |
| A shell or interactive terminal program | `terminal` | yes | yes |

A dashboard can contain only passive widgets. Add `terminal` instances only for
workflows that need a PTY; this keeps startup fast and makes failure isolation
clear.

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
command = "sh"
```

If `command` is omitted, the session uses the platform's configured shell
fallback. The widget handles keyboard, mouse, paste, resize, selection copy,
and shutdown. It is the only built-in widget that currently owns a PTY.

The cursor is rendered by cmdash inside the terminal scene. When the terminal
pane is focused and visible, it blinks at the configured interval; keyboard
input, PTY output, cursor movement, and focus changes reset it to visible.
Unfocused panes and hidden tabs do not blink. Set `cursor_blink = "false"` for a
static cursor, or use `cursor_blink_interval_ms` to tune the interval. The
scheduler is wakeable and does not use blinking to poll PTY output.

A terminal widget is not a global terminal pane. Splitting it creates another
terminal widget and another session ID; the new pane inherits the source
configuration while retaining independent process and emulator state.

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
installed. For example, a terminal startup failure or invalid clock format is
reported as a widget setup error.

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
  was limited, such as an omitted graphics payload.
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

Built-in widget borders occupy the outer edge of the assigned surface. Terminal
content is rendered into the configured content rectangle, inset by the border
and padding; with defaults this is one cell on every side. The terminal's PTY
size, graphics placements, selection coordinates, and mouse origin use that
inner rectangle as well. This keeps terminal text and cursor
output from overwriting the outline.

The layout system currently sizes widgets to their assigned surface; it does
not provide a separate general-purpose alignment option for centering a widget
inside a larger parent area. Splits, columns, and explicit layout geometry are
the supported positioning controls.

### Frame composition

The compositor renders visible widget scenes, orders surfaces and overlays,
clips them to the viewport, and diffs the result against the previous frame.
Only visible layout branches contribute output. Hidden tab sessions remain
alive and retain state, but their scenes and graphics are not submitted.

### Colors and theming

Static theming is implemented. The default semantic theme uses terminal-native
reset and ANSI references so widget colors follow the parent terminal palette.
Use `[appearance.colors]` for workspace-wide role overrides and widget
`settings` for per-instance overrides. The complete role list, color syntax,
precedence rules, border styles, and label policy are documented in
[APPEARANCE.md](APPEARANCE.md).

Animation is not implemented yet and remains an opt-in retained-scene feature
planned for Phase 12. Planned options include transitions for focus, borders,
labels, values, overlays, tabs, and panes, plus duration, easing, repetition,
reduced-motion, and per-widget motion budgets.

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

### Terminal keys and copy

For a focused terminal widget, key events are encoded for the PTY and paste is
sent through the session's bracketed-paste-aware path. `Ctrl+Shift+C` copies the
current selection through the backend's OSC 52 clipboard submission path. The
copy notification is kept in cmdash diagnostics/status state rather than sent
to the shell.

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
uses a session-qualified resource identity, so image ID `1` in session A is
not the same resource as image ID `1` in session B. The store retains decoded
resources, encoded payloads, placements, z-order, and diagnostics.

A widget's `graphics(area)` output is clipped to its surface and passed through
the retained scene and compositor pipeline. The backend negotiates Kitty
support and submits only visible current layers. When a tab is hidden, its
resources remain associated with the hidden session but are not submitted;
when it becomes visible again, its scene can be rebuilt from retained state.

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
for dashboard-provided images. A widget can return sixel submissions through
its feature-gated `sixel(area)` method. These layers are retained in `Scene`,
clipped, diffed, and submitted only when backend capability detection reports
sixel support.

Enable the feature with:

```text
cargo run --features sixel
```

Terminal-originated Kitty graphics and dashboard-provided sixel images are
separate paths. Enabling sixel does not change ownership of terminal graphics
or make sixel available to terminal PTYs.

## Plugins and extension points

Built-in widgets are registered in a `WidgetRegistry`. A factory receives a
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
context also exposes the resolved `Theme` used by built-in and external widgets.
Applications that run terminal sessions can construct a context with
`WidgetRuntimeContext::with_session_wakeup(wakeup)` and pass it to
`WidgetRegistry::builtins_with_context`. The session wakeup is optional so
widget-only dashboards and custom passive widgets remain usable without a PTY.
The built-in terminal factory consumes this capability; other built-ins ignore
it. Future runtime services should be added as explicit context capabilities
with documented ownership and failure behavior.

External in-process widgets use the same `Widget` scene contract. The public
`WidgetRuntimeContext::theme()` method provides the resolved semantic `Theme`.
`WidgetAppearance::from_settings` parses the common `padding` and `border`
settings, `WidgetAppearance::content_area(area)` gives the matching inner
rectangle, and `WidgetAppearance::render_border(...)` draws the selected
outline. Plugins that use these helpers get the same geometry and colors for
rendering, input, and terminal-like content. The older `widget_content_area(area)` helper
remains available for the fixed one-cell legacy contract. The host clips the
resulting scene to the assigned surface but does not guess arbitrary plugin
border geometry. The checked-in `ExternalTextPlugin` is the reference
implementation of this contract.

`WidgetRegistry::builtins()` remains the no-service convenience constructor.
External implementations use the versioned plugin contract rather than writing
directly to the terminal backend.

### Manifest contract

A plugin manifest currently contains:

```toml
manifest_version = 1
name = "example"
version = "1.0.0"
abi_version = 1
runtime = "wasm"
capabilities = 1

[[widgets]]
type = "example-status"
capabilities = 1
```

The host validates manifest version, identity, ABI version, widget type names,
uniqueness, and requested capabilities before loading widget code. The widget
type name must be non-empty and no longer than 32 bytes.

The host-facing capability bits currently describe:

- `RENDER_SCENE`: produce backend-neutral scene output;
- `UPDATE`: receive update opportunities;
- `INPUT`: receive input routed by the host;
- `OVERLAYS`: request or contribute overlay behavior.

A plugin may request only capabilities available from the selected host. The
host must not expose stdout, raw terminal escape sequences, PTY handles,
filesystem access, or Rust trait objects across the boundary.

### WASM isolation status

The optional `wasm-plugins` feature uses Wasmtime for import-free validation
and per-instance isolation. It bounds module size and fuel, creates a separate
store for each instance, and does not link WASI or terminal imports:

```text
cargo run --features wasm-plugins
```

The current runtime foundation intentionally rejects modules with imports. The
actual host-function ABI for lifecycle calls, input messages, bounded scene
output, and manifest-to-runtime loading is still a future extension. A plugin
configuration should therefore be treated as a contract/validation example,
not as a promise that arbitrary WASM widget binaries are currently executable.

When implementing a new host function, keep the following rules:

1. Add it to the versioned ABI and capability declaration.
2. Bound input sizes, output sizes, and execution time/fuel.
3. Serialize scene data rather than exposing backend handles.
4. Define failure and shutdown behavior for a single affected instance.
5. Add a manifest, host, malformed-input, and resource-limit test.

## Panes, sessions, and persistence

Pane commands operate on focused terminal widget instances. A split:

1. verifies that the focused instance is a terminal;
2. allocates a fresh widget ID and session identity;
3. clones the source terminal configuration for the new widget;
4. starts an independent terminal session;
5. inserts a split node with initial 50/50 ratios;
6. focuses the new surface and invalidates the affected area.

Closing or merging a pane shuts down only the removed widget session, removes its
surface and configuration entry, and normalizes a parent with one remaining
child. The last visible pane cannot be closed. Shutdown failures are recorded
as bounded diagnostics.

Runtime changes mark the layout dirty and serialize the current layout tree,
split ratios, and active tab state into the in-memory configuration. During a
safe reload, valid runtime pane entries and the mutable layout are retained;
invalid replacement configuration is rejected without replacing the active
state. Focus is restored only when its surface still exists and is visible.

This gives every terminal pane independent PTY and graphics ownership while
allowing the surrounding workspace arrangement to persist across reloads.

## Troubleshooting a widget

- **The widget is not visible:** confirm its `id` appears in a reachable
  `leaf`, `columns`, `split`, or active `tabs` branch. A valid but unreachable
  widget is still initialized without receiving a visible surface.
- **A widget fails during startup:** check the diagnostic footer for its type and
  initialization error. For terminals, verify the command exists and that the
  layout gives the pane a non-zero area.
- **A key is ignored:** focus the widget, then check whether it is interactive.
  `text`, `clock`, and `system` intentionally do not handle input.
- **A terminal copy does not reach the clipboard:** selection and OSC 52 depend
  on the surrounding terminal emulator's clipboard policy.
- **Images are missing:** Kitty and sixel are optional capability paths. Check
  terminal support, feature flags, clipping, and graphics quota diagnostics.
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

A built-in widget implementation should:

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
- plugin manifests reject unsupported versions, capabilities, and duplicate
  widget types;
- WASM modules with imports or excessive size are rejected;
- update, input, and shutdown failures stay local and become diagnostics.

The project also maintains fuzz targets and seed corpora for configuration
migration, plugin manifests, Kitty APC streams, and sixel RGB encoding. Widget
parsers and host-function decoders should add bounded fuzz inputs as their
contracts grow.

## Related documents

- [Configuration reference](CONFIGURATION.md) — discovery, TOML fields,
  layouts, panes, overlays, migrations, and recovery.
- [Appearance guide](APPEARANCE.md) — semantic themes, inherited terminal
  palette colors, borders, labels, and per-widget overrides.
- [Architecture](ARCHITECTURE.md) — state ownership, scenes, compositor, and
  backend boundaries.
- [Dependencies](DEPENDENCIES.md) — selected crate roles and optional feature
  boundaries.
- [Roadmap](ROADMAP.md) — planned host ABI, pane, fuzzing, graphics, and
  configuration work.
