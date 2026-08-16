# Creating widgets

This guide explains how to implement, register, test, and distribute a cmdash
widget. It is the development companion to the user-facing catalog and runtime
reference in [WIDGETS.md](WIDGETS.md). Read that document first for the widget
model, the built-in types, and the layout/plugin boundaries; this guide covers
the code behind those boundaries.

## Choosing where a widget lives

Before writing code, decide which extension path matches the job:

| Need | Path |
| --- | --- |
| A general-purpose widget that ships with cmdash | Register a built-in factory in `WidgetRegistry::build_builtins`. |
| A project-local widget that does not need process isolation | Register an in-process factory against the public `Widget` trait (this guide). |
| Untrusted, capability-limited, or separately distributed code | Use the versioned plugin manifest and the opt-in Wasmtime host; see [WIDGETS.md](WIDGETS.md) for the manifest and WASM isolation contract. |

Every path produces the same backend-neutral `Scene` output and uses the same
factory shape, so a widget can start in-process and move behind the plugin
boundary later without changing its rendering model.

## The contract

A widget is any `Send` type that implements the [`Widget`](WIDGETS.md) trait:

```rust
pub trait Widget: Send {
    fn kind(&self) -> &str;
    fn initialize(&mut self) -> Result<(), String> { Ok(()) }
    fn update(&mut self, now: SystemTime) -> Result<WidgetUpdate, String>;
    fn health(&self) -> WidgetHealth;
    fn render(&self, area: Rect, focused: bool) -> Scene;
    fn content_area(&self, area: Rect) -> Rect;
    fn graphics(&self, area: Rect) -> Vec<GraphicsSubmission>;
    fn handles_input(&self) -> bool;
    fn handle_key(&mut self, key: KeyEvent) -> Result<WidgetUpdate, String>;
    fn resize(&mut self, size: TerminalSize) -> Result<WidgetUpdate, String>;
    fn shutdown(&mut self) -> Result<(), String>;
    // ... plus paste, mouse, selection, cursor-blink, and animation hooks.
}
```

Only `kind` and `render` are strictly required; every other method (including
`update`) has a safe default. Implement only the hooks your widget needs.

### Lifecycle

1. **Construct** — the factory builds the widget from configuration and the
   runtime context. Do not spawn workers, open files, or mutate shared state in
   the constructor.
2. **Initialize** — `initialize()` runs after construction and may perform
   fallible startup (the terminal widget polls its PTY here). Return an `Err` to
   fail the instance with a diagnostic rather than a partially working widget.
3. **Update** — the coordinator calls `update(now)` on a maintenance tick.
   Return `WidgetUpdate::Redraw` only when the visible output changed, and
   `WidgetUpdate::Unchanged` otherwise. This is the redraw-coalescing contract.
4. **Render** — `render(area, focused)` produces a fresh, clipped `Scene` for the
   assigned surface. It must be a pure function of retained state.
5. **Shutdown** — `shutdown()` releases owned resources (PTYs, tasks, files).
   The runtime calls it on pane close, reload, and application exit.

### Health and failure isolation

`health()` returns `Healthy`, `Degraded(reason)`, or `Failed(reason)`. The
runtime records failures from `update`, `handle_key`, and `resize` on the
affected entry and reports them through `statuses()` and diagnostics. A widget
error must never take down the dashboard, the backend, or another widget.

## The factory

Factories are plain functions with one shared shape:

```rust
type WidgetFactory =
    fn(&WidgetInstanceConfig, &WidgetRuntimeContext) -> Result<Box<dyn Widget>, WidgetError>;

fn greeting_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    // ...
}
```

### `WidgetInstanceConfig`

```rust
pub struct WidgetInstanceConfig {
    pub id: u64,
    pub kind: String,           // `type` in TOML
    pub title: Option<String>,
    pub label: LabelPolicy,     // Auto | Always | Never
    pub text: Option<String>,
    pub format: Option<String>,
    pub command: Option<String>,
    pub settings: BTreeMap<String, String>,
}
```

- `id` is the unique numeric widget ID from TOML.
- `title`, `text`, `format`, and `command` are the optional type-specific fields.
- `label` controls whether the title is drawn in the widget border; `Never`
  removes the label without an empty-string sentinel.
- `settings` is the stable string-to-string map for widget-specific options.
  Parse and validate every setting you read, and return
  `WidgetError::InvalidConfiguration` for bad values. Never `unwrap` settings.

### `WidgetRuntimeContext`

```rust
let context = WidgetRuntimeContext::new()
    .with_session_wakeup(wakeup)          // optional: terminal sessions only
    .with_initial_terminal_size(size)     // optional: terminal sessions only
    .with_kitty_graphics(supported)       // optional: terminal sessions only
    .with_theme(theme);                   // resolved semantic theme
```

The context is the construction-time capability boundary. Read the resolved
theme with `context.theme()` and apply per-widget role overrides with
`theme.with_settings(&config.settings)`. Do not reach into `AppState`, the
compositor, the backend, or global state; the context is all a factory may use.

## Rendering a scene

Widgets never write terminal escape sequences. They build a backend-neutral
`Scene`; the compositor and backend own serialization.

### Geometry, borders, and labels

Use the shared appearance helpers so your widget matches built-in chrome:

```rust
let appearance = WidgetAppearance::from_settings(&config.settings)?;
let content = appearance.content_area(area);       // inner rectangle
appearance.render_border(&mut scene, area, title, border_style);
```

`WidgetAppearance` parses `settings.padding` and `settings.border`
(`rounded`, `square`, `double`, `heavy`, `ascii`, `none`). Always render content
into `content_area(area)`, never the raw surface, so text cannot overwrite the
outline.

### Cells, text, and clipping

```rust
use cmdash::scene::{CellStyle, Color, Scene};

let mut scene = Scene::new(area);
scene.fill(area, CellStyle::new(theme.foreground(), theme.surface()));
scene.text(area.x, area.y, "hello", CellStyle::new(theme.accent(), theme.surface()).bold());
```

`Scene::text` and `Scene::set` clip to the scene's area and respect Unicode
display widths (wide glyphs consume two cells with a continuation cell). Fill
and blit within `content_area(area)` and the widget cannot draw outside its
assigned surface.

Use semantic theme roles (`surface`, `foreground`, `muted`, `border`, `focus`,
`accent`, `success`, `warning`, `error`) instead of hardcoded RGB so inherited
palettes and theme overrides keep working. See
[APPEARANCE.md](APPEARANCE.md).

### Reusing shared helpers

The built-in catalog shares bounded rendering helpers — severity styling,
`key: value` rows, progress bars, sparkline normalization, and horizontal rules.
When you need one of these, prefer the shared helper or add a new bounded helper
to `src/widget.rs` rather than duplicating the logic. Helpers must clip to the
given area and define their minimum-size behavior explicitly.

## Focus, input, and resize

- Return `true` from `handles_input()` to receive keyboard and mouse input.
- `handle_key` returns `WidgetUpdate`; `Redraw` requests a frame, `Unchanged`
  does not. Encode terminal input through the session, never through stdout.
- `resize(size)` is called with the session/window size; return `Redraw` when
  geometry changed.
- Implement `copy_selection`, `handle_paste`, and `handle_mouse` only for
  widgets that need them.

Terminal widgets are the canonical interactive implementation: they own a
`TerminalSession`, forward keys/paste/mouse to it, and render its emulator grid.

## Background work and wakeups

- **Synchronous refresh** is the simplest model: `update(now)` recomputes a
  value (the `clock` widget recomputes UTC) and returns `Redraw` on change. The
  coordinator already invokes `update` on its maintenance tick, so no timer
  polling is needed.
- **Session-driven work** (terminal output) uses `WidgetRuntimeContext`'s
  optional `SessionWakeup` and is consumed through the session, not a widget
  worker.
- **Hidden widgets** are excluded from `render` because the runtime only renders
  instances with an assigned, visible area. Keep `update` cheap and side-effect
  free; do not perform work solely because an instance is hidden.

If a widget genuinely needs its own wakeups or a data provider, keep the
provider separate from rendering, send bounded messages into the widget, and
request redraws through `WidgetUpdate`. Providers must be testable without an
interactive terminal and must not outlive their owning instance.

## Registration

```rust
let mut registry = WidgetRegistry::builtins();
registry
    .register("greeting", greeting_factory)
    .expect("widget types must be unique");
```

`register` rejects duplicate type names. Instantiation goes through
`WidgetRuntime::from_config(&registry, &config)`, which validates settings,
calls the factory, runs `initialize()`, and captures initial health. Unknown
types, duplicate IDs, and initialization failures become `WidgetError`s.

## A minimal example

```rust
use std::collections::BTreeMap;
use std::time::SystemTime;

use cmdash::{
    CellStyle, Scene, Theme, Widget, WidgetAppearance, WidgetError, WidgetInstanceConfig,
    WidgetRegistry, WidgetRuntimeContext, WidgetUpdate,
};
use ratatui::layout::Rect;

struct GreetingWidget {
    title: String,
    text: String,
    appearance: WidgetAppearance,
    theme: Theme,
}

impl Widget for GreetingWidget {
    fn kind(&self) -> &str {
        "greeting"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = self.theme.surface();
        let foreground = self.theme.foreground();
        let accent = if focused {
            self.theme.focus()
        } else {
            self.theme.border()
        };
        let mut scene = Scene::new(area);
        scene.fill(area, CellStyle::new(foreground, background));
        self.appearance.render_border(
            &mut scene,
            area,
            &self.title,
            CellStyle::new(accent, background),
        );
        let content = self.appearance.content_area(area);
        if content.width > 0 && content.height > 0 {
            scene.text(content.x, content.y, &self.text, CellStyle::new(foreground, background));
        }
        scene
    }
}

fn greeting_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    Ok(Box::new(GreetingWidget {
        title: config.title.clone().unwrap_or_else(|| " greeting ".to_owned()),
        text: config.text.clone().unwrap_or_default(),
        appearance: WidgetAppearance::from_settings(&config.settings)?,
        theme: context
            .theme()
            .with_settings(&config.settings)
            .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
    }))
}
```

A data-backed widget is the same shape plus a mutable value and an `update`
override. The `clock` widget is the reference: it stores the rendered string,
recomputes it in `update(now)`, and returns `Redraw` only when it changed.

```rust
use std::time::{SystemTime, UNIX_EPOCH};

struct SecondsWidget {
    text: String,
    appearance: WidgetAppearance,
    theme: Theme,
}

impl Widget for SecondsWidget {
    fn kind(&self) -> &str {
        "seconds"
    }

    fn update(&mut self, now: SystemTime) -> Result<WidgetUpdate, String> {
        let seconds = now.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let text = seconds.to_string();
        if text == self.text {
            return Ok(WidgetUpdate::Unchanged);
        }
        self.text = text;
        Ok(WidgetUpdate::Redraw)
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        // Render `self.text` into a bordered scene, as in the greeting example.
        let _ = focused;
        let mut scene = Scene::new(area);
        scene.fill(
            area,
            CellStyle::new(self.theme.foreground(), self.theme.surface()),
        );
        let content = self.appearance.content_area(area);
        scene.text(
            content.x,
            content.y,
            &self.text,
            CellStyle::new(self.theme.accent(), self.theme.surface()),
        );
        scene
    }
}
```

`update` returns `Unchanged` until the rendered value actually changes, which is
what lets the compositor coalesce frames instead of redrawing on every tick.

## Testing a widget

Test configuration parsing, rendering, and lifecycle without a terminal:

1. Parse a TOML snippet with `AppConfig::parse` and instantiate via
   `WidgetRuntime::from_config(&registry, &config)`.
2. Render with a known `Rect` and assert cell symbols and styles — the same
   pattern used by the built-in widget tests.
3. Cover invalid settings, narrow/zero-area surfaces, and `update` coalescing.

```rust
let config = AppConfig::parse(
    r#"version = 1
       [[workspace.widgets]]
       id = 1
       type = "greeting"
       text = "hello"
    "#,
)
.unwrap();
let runtime = WidgetRuntime::from_config(&registry, &config).unwrap();
let area = Rect::new(0, 0, 12, 3);
let scene = runtime.render(&BTreeMap::from([(WidgetId::new(1), area)]), None);
assert_eq!(scene[&WidgetId::new(1)].cell_at(1, 1).unwrap().symbol, 'h');
```

The checked-in widget test suite follows this pattern and includes a custom
factory registered against the public API, so the guide and the tests stay in
sync.

## Plugin and WASM restrictions

External plugins use the versioned manifest contract and the opt-in Wasmtime
host described in [WIDGETS.md](WIDGETS.md). Regardless of path, a widget must
never:

- write to `stdout` or emit raw terminal escape sequences;
- access PTY handles, the compositor, the backend, or global mutable state;
- read the filesystem, network, or shared memory implicitly;
- spawn unbounded workers or retain resources after shutdown.

When adding a host function for plugins, bound its inputs, outputs, and fuel,
serialize scene data instead of exposing backend handles, define failure and
shutdown behavior, and add manifest, host, malformed-input, and resource-limit
tests.

## Troubleshooting

- **The widget never renders:** it must be present in the layout and receive a
  non-zero area; `render` only runs for visible instances with an assigned area.
- **Text overwrites the border:** render into `appearance.content_area(area)`,
  not the full surface.
- **`update` returns `Redraw` every tick:** the widget is recomputing output
  that has not changed, which defeats redraw coalescing and wastes frames.
- **A setting is ignored:** parse it in the factory and validate it; unrecognized
  settings are ignored by the common appearance/theme parsers rather than
  failing the widget.
- **Initialization fails:** the error surfaces as `WidgetError`/health and a
  diagnostic; keep fallible startup inside `initialize()`, not the constructor.
