# Configuration

cmdash uses versioned TOML configuration with the `cmdash.workspace` schema. The
current configuration version is `1`.

## Discovery order

Configuration is selected in this order:

1. An explicit `--config <path>` or `-c <path>` argument.
2. `$CMDASH_CONFIG`, when set.
3. `$XDG_CONFIG_HOME/cmdash/config.toml` (or `~/.config/cmdash/config.toml`).
4. `config/default.toml` when running from a source checkout.
5. The embedded default configuration.

An explicitly selected or discovered file is watched and can be safely reloaded
with `Ctrl+R`. Invalid edits are rejected without replacing the active state.

## Top-level options

```toml
version = 1

[workspace]
name = "monitor"

[[plugins]]
name = "example"
manifest = "plugins/example.toml"
enabled = true
```

- `version` is required for new files and must be `1`.
- `workspace.name` labels the active workspace and defaults to `default`.
- `plugins` contains named plugin manifest paths. Plugin loading remains
  capability-limited; WASM support is opt-in with `--features wasm-plugins`.

Files without a version, or with legacy version `0`, are accepted with a
migration warning. `AppConfig::migrate_source` provides the safe rewrite
primitive used by tooling and migration tests. Unsupported future versions are
rejected.

## Widgets

```toml
[[workspace.widgets]]
id = 10
type = "terminal"
title = " shell "
command = "sh"

[workspace.widgets.settings]
scrollback = "4096"
```

Every widget needs a unique numeric `id` and a non-empty `type`. Built-in types
are `text`, `clock`, `system`, and `terminal`.

- `title`, `text`, `format`, and `command` are optional type-specific fields.
- `settings` is a stable string-to-string map reserved for widget options.
- `clock.format` accepts `HH:MM` or `HH:MM:SS`.
- A terminal widget owns its PTY, emulator, selection, graphics resources, and
  shutdown lifecycle.

## Layouts and panes

A layout is a tree. A leaf references a widget; `columns` divide horizontally,
`split` divides according to an explicit direction and optional percentage-like
`ratios`, `tabs` retain inactive branches, `stack` overlays children, and an
`overlay` references a workspace overlay.

```toml
[workspace.layout]
type = "split"
direction = "horizontal"
ratios = [60, 40]
children = [
  { type = "leaf", widget = 10 },
  { type = "leaf", widget = 11 }
]
```

Pane controls operate on the focused terminal:

- `Ctrl+Shift+H` / `Ctrl+Shift+V`: split horizontally/vertically.
- `Alt+Arrow`: move focus directionally.
- `Ctrl+Shift+Left` / `Ctrl+Shift+Right`: adjust split ratios.
- `Ctrl+Shift+W`: close the focused pane.
- `Ctrl+Shift+M`: merge the focused pane into its parent split.
- `Ctrl+PageUp` / `Ctrl+PageDown`: switch retained tabs.

New panes inherit the focused terminal's command and widget settings. Pane
creation assigns a fresh widget/session identity. Runtime pane trees, ratios,
tab selection, and focus are retained across safe reloads when they remain
valid against the edited configuration.

## Overlays

```toml
[[workspace.overlays]]
id = 20
x = 2
y = 2
width = 40
height = 6
z_index = 10
visible = true
title = " notice "
text = "Hello"
```

Overlay IDs must be unique and areas must have non-zero width and height.

## Graphics and optional features

Kitty graphics are retained per terminal session and bounded by the session
store limits. Unsupported or oversized resources become degraded diagnostics
instead of corrupting text output. The optional `sixel` feature provides a
bounded 16-color dashboard RGB encoder; the default build remains dependency-
free and capability-aware. Optional WASM plugins run without imports or WASI
access and use per-instance execution budgets.

## Reload, migration, and diagnostics

`Ctrl+R` reloads the selected file. `?` opens help and `Ctrl+P` opens the command
palette. `CMDASH_CRASH_DIR` enables bounded crash reproduction reports when the
application exits with an error. Diagnostics are shown in the dashboard footer
and are kept separate from PTY output.

Common recovery actions:

- Fix duplicate IDs, missing layout references, or unsupported versions shown in
  the diagnostic.
- Restore the last valid file; the active configuration remains in memory while
  a replacement fails validation.
- Use the embedded/default file as a minimal known-good starting point.
- Promote a migration or parser failure to a regression test before upgrading
  the schema.
