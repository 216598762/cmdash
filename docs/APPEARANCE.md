# Appearance and theming

Phase 11 appearance support is complete. cmdash now renders widgets through a
semantic theme instead of embedding widget-specific RGB colors throughout the
runtime. The default theme follows the parent terminal's palette using
terminal-native reset and ANSI color references; explicit RGB values remain
available when a workspace needs a fixed appearance.

For the general configuration schema, see
[CONFIGURATION.md](CONFIGURATION.md). For widget lifecycle and scene contracts,
see [WIDGETS.md](WIDGETS.md). The implementation roadmap is in
[ROADMAP.md](ROADMAP.md).

## Quick start

The default configuration inherits the surrounding terminal palette:

```toml
[appearance]
theme = "inherit"
```

A deterministic RGB fallback is also available:

```toml
[appearance]
theme = "fallback"
```

The fallback is useful for screenshots, tests, and environments where a stable
fixed palette matters more than blending into the terminal.

## Theme model

A `Theme` contains semantic roles rather than widget-specific color constants:

| Role | Used for |
| --- | --- |
| `background` | Workspace background and terminal default background. |
| `surface` | Widget panels and dashboard header surfaces. |
| `foreground` | Normal widget and terminal default text. |
| `muted` | Footer, secondary text, and disabled/degraded presentation. |
| `border` | Unfocused borders and normal widget chrome. |
| `focus` | Focused borders, cursor emphasis, and active controls. |
| `accent` | Header and dashboard accent decoration. |
| `success` | Healthy status and positive dashboard information. |
| `warning` | Warning state and attention-required information. |
| `error` | Failure state and error information. |
| `selection_foreground` / `selection_background` | Text selection presentation. |
| `overlay_foreground` / `overlay_background` | Help, palette, and notification overlays. |

Widgets consume these roles through the runtime context. External in-process
widgets can use the same public `Theme` API and should avoid defining a second
palette contract.

## Parent terminal palette inheritance

`theme = "inherit"` does not guess a dark or light RGB palette. Instead, it
uses terminal-native references:

- `Reset` asks the parent terminal for its current default foreground or
  background;
- ANSI references such as `ansi:10` ask the parent terminal to resolve the
  corresponding ANSI palette entry;
- truecolor shell output remains truecolor;
- extended 256-color terminal output preserves ANSI-indexed colors for the first
  16 entries and uses deterministic RGB conversion for the extended cube and
  grayscale range.

This approach is safe for terminals that do not expose palette-query protocols:
the parent terminal already owns the ANSI/default palette, so cmdash does not
need to block startup waiting for an OSC response. It also means changing the
parent terminal's palette affects subsequent cmdash frames naturally.

The `fallback` theme uses explicit RGB values based on the original cmdash
palette. Invalid or unsupported configuration never silently changes to an
arbitrary color; configuration loading reports the role and value that failed.

## Configuring global roles

Global role overrides live under `[appearance.colors]`:

```toml
[appearance]
theme = "inherit"

[appearance.colors]
background = "#101418"
surface = "#1b212c"
foreground = "#e2e8f0"
muted = "ansi:8"
border = "ansi:14"
focus = "#facc15"
accent = "ansi:14"
success = "#86efac"
warning = "#f59e0b"
error = "#f87171"
selection_foreground = "#101418"
selection_background = "ansi:14"
overlay_foreground = "#f5e8ff"
overlay_background = "#261c3a"
```

Supported values are:

- `inherit`, `terminal`, or `default`: the terminal's default foreground or
  background reference;
- `ansi:N`: ANSI palette index `0` through `255`;
- `#RRGGBB`: explicit truecolor.

Role names are validated. Unknown roles and malformed values reject the
configuration before widgets are replaced during a reload.

## Per-widget overrides

Widget-specific appearance overrides remain in the string-valued `settings` map:

```toml
[[workspace.widgets]]
id = 10
type = "text"
title = " deployment "
label = "always"
text = "production: healthy"

[workspace.widgets.settings]
border = "double"
padding = "1"
border_color = "#facc15"
foreground = "#e2e8f0"
background = "#1b212c"
focus = "ansi:11"
muted = "ansi:8"
```

The `border` setting selects geometry and is separate from `border_color`, which
selects the semantic border color. Supported border geometry values are:

- `rounded` (default): `╭─╮ │ ╰─╯`;
- `square`: `┌─┐ │ └─┘`;
- `double`: `╔═╗ ║ ╚═╝`;
- `heavy`: `┏━┓ ┃ ┗━┛`;
- `ascii`: `+-+ | +-+`;
- `none`: no outline, while padding still applies.

`border_style` is accepted as an alias for `border` for compatibility.

## Labels

The `label` field controls whether the widget title is rendered in the border:

```toml
label = "never"
title = "this title is retained as metadata but not drawn"
```

Policies are:

- `auto` (default): render the configured title or the built-in widget default;
- `always`: render the title, including an empty configured title;
- `never`: do not draw a label.

A hidden label does not change the widget surface or content rectangle. This
keeps terminal PTY sizing, mouse coordinates, graphics, selection, and plugin
content stable when visual chrome changes.

## Precedence and reload

Appearance is resolved in this order, from lowest to highest precedence:

1. inherited terminal-native theme;
2. `appearance.theme` (`inherit` or `fallback`);
3. `[appearance.colors]` role overrides;
4. widget-type defaults;
5. widget-instance `settings` role overrides;
6. transient focus, selection, and health presentation.

The complete appearance is rebuilt when a file-backed configuration reloads.
Invalid appearance values reject the replacement and leave the active widgets
and theme unchanged. Runtime-created panes inherit the focused widget's command,
settings, label policy, and resulting appearance configuration.

## Plugin contract

The `WidgetRuntimeContext` exposes the resolved `Theme` to in-process widget
factories and plugin modules. Plugins should use `Theme` roles and
`WidgetAppearance` geometry helpers rather than hard-coded colors. The host
continues to clip plugin scenes to their assigned surface; theme resolution does
not grant a plugin direct terminal output access.

WASM plugins will need an explicit future appearance capability before they can
request dynamic theme changes. They must not emit raw color escape sequences or
assume a specific terminal palette.

## Cursor blinking and accessibility

Focused visible terminal panes blink their scene cursor by default. Configure
`cursor_blink = "false"` for a static cursor or set
`cursor_blink_interval_ms` between `50` and `60000` milliseconds. Input, PTY
output, cursor movement, and focus changes restore the cursor before the next
blink. Hidden tabs and unfocused terminal panes remain static, and the scheduler
wakes only while a pane is active. The cursor's emulator visibility mode still
wins, so applications can hide it with terminal control sequences.

Cursor settings are per terminal widget because each pane owns its own PTY and
emulator. They belong in the widget's string-valued `settings` map rather than
under `[appearance]`:

### Default blinking

The defaults are equivalent to:

```toml
[[workspace.widgets]]
id = 10
type = "terminal"
command = "sh"

[workspace.widgets.settings]
cursor_blink = "true"
cursor_blink_interval_ms = "500"
```

### Static cursor

Disable blinking when a persistent cursor is preferred:

```toml
[[workspace.widgets]]
id = 11
type = "terminal"
command = "sh"

[workspace.widgets.settings]
cursor_blink = "false"
```

### Custom blink interval

Use a slower or faster interval within the supported 50–60000 millisecond
range:

```toml
[[workspace.widgets]]
id = 12
type = "terminal"
command = "sh"

[workspace.widgets.settings]
cursor_blink_interval_ms = "750"
```

Runtime-created panes inherit these terminal widget settings. Configuration
reloads validate the values before replacing the active runtime, so an invalid
interval leaves the previous terminal and cursor behavior unchanged.

### Reduced-motion equivalent today

A global reduced-motion setting is not implemented yet. To avoid cursor motion
for a specific terminal pane today, use the static-cursor fallback:

```toml
[[workspace.widgets]]
id = 13
type = "terminal"
command = "sh"

[workspace.widgets.settings]
# Current reduced-motion equivalent for this pane.
cursor_blink = "false"
```

The planned Phase 12 global form is documented here for design reference only
and is not accepted by the current configuration parser:

```toml
# Planned; not currently supported.
[appearance.motion]
reduced_motion = true
```

Animation is intentionally not part of Phase 11. The next roadmap phase covers
opt-in transitions, reduced-motion preferences, timing budgets, and animated
border/label changes. Phase 11 provides the static appearance foundation those
features will consume.

The terminal remains the authority for inherited reset and ANSI colors. A
terminal that changes its palette while cmdash is running will affect native
references on subsequent output, but explicit RGB overrides remain unchanged.
