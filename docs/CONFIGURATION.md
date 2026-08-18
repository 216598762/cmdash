# Configuration

Appearance details and color-role examples live in
[APPEARANCE.md](APPEARANCE.md).

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
When cmdash starts from the embedded fallback because no file was found, there
is no file to reload; `Ctrl+R` reports that it requires `--config <path>`.

## First-run and daily workflow

From a source checkout, the most direct workflow is:

```bash
mkdir -p ~/.config/cmdash
cp config/default.toml ~/.config/cmdash/config.toml
$EDITOR ~/.config/cmdash/config.toml
cargo run -- --config ~/.config/cmdash/config.toml
```

The same configuration can be used with an installed or released binary:

```bash
cmdash --config ~/.config/cmdash/config.toml
```

Use an explicit path when experimenting with multiple workspaces. Use
`CMDASH_CONFIG` when a shell profile or launcher should select a workspace
without repeating the argument:

```bash
CMDASH_CONFIG="$HOME/.config/cmdash/config.toml" cmdash
```

A practical edit/reload loop is:

1. Keep the active file in an editor.
2. Make one small TOML change.
3. Save it and wait for the file-backed watcher, or press `Ctrl+R` to request an
   immediate reload.
4. Check the diagnostic footer for migration warnings or rejection details.
5. If validation fails, fix or restore the file; the last valid runtime remains
   active.

Runtime pane creation, closure, split ratios, focus, and tab state are retained
through a safe reload when they remain valid. These runtime changes are held in
memory; cmdash does not currently rewrite the source TOML automatically. Edit
the layout and widget entries yourself when a pane arrangement must survive a
full process restart.

## Top-level options

```toml
version = 1

[workspace]
name = "monitor"

[appearance]
theme = "inherit"

[api]
enabled = false
transport = "unix"
socket = "~/.cache/cmdash/cmdash.sock"

[[plugins]]
name = "example"
manifest = "plugins/example.toml"
enabled = true
```

- `version` is required for new files and must be `1`.
- `workspace.name` labels the active workspace and defaults to `default`.
- `appearance` selects inherited/fallback theme colors and workspace role
  overrides; see [APPEARANCE.md](APPEARANCE.md).
- `animation` enables bounded retained motion when explicitly configured; see
  [ANIMATION.md](ANIMATION.md) for its complete contract.
- `api` enables the local, disabled-by-default compositor API; see [API.md](API.md)
  for endpoints, security, limits, and CLI overrides.
- `plugins` contains named plugin manifest paths. Plugin loading remains
  capability-limited; WASM support is opt-in with `--features wasm-plugins`.
- `keybindings` maps stable action names to key chords; see
  [Keybindings](#keybindings) below.

Files without a version, or with legacy version `0`, are accepted with a
migration warning. `AppConfig::migrate_source` provides the safe rewrite
primitive used by tooling and migration tests. Unsupported future versions are
rejected.

## Keybindings

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

The `[keybindings]` section maps stable action names to key chords. Keys are
written as a single key name plus optional `ctrl`, `alt`, and `shift` modifiers
joined with `+`. Supported key names are printable characters, `space`, `esc`,
`enter`, `tab`, `backtab` (also `shift+tab`), `backspace`, the four arrows,
`home`, `end`, `pageup`/`pgup`, `pagedown`/`pgdn`, `delete`/`del`,
`insert`/`ins`, and `f1` through `f12`.

- Every action has a default binding; omitting `[keybindings]` entirely keeps
  the defaults unchanged.
- Rebinding an action removes its previous binding, so the map never contains
  two chords for one action.
- Binding one chord to two different actions is rejected as a conflict; unknown
  action names and unparsable chords fail configuration validation.
- Inside a focused terminal shell only `focus_next` and `focus_previous` are
  intercepted; remapping them also remaps how the user escapes terminal capture.
- Keybindings are reload-safe: `Ctrl+R` revalidates and swaps the whole keymap
  along with the rest of the configuration, and the in-app help and command
  palette list the currently active bindings.

## Widgets

```toml
[[workspace.widgets]]
id = 10
type = "terminal"
title = " shell "
command = "sh"

[workspace.widgets.settings]
scrollback = "4096"
padding = "1"
border = "rounded"
```

Every widget needs a unique numeric `id` and a non-empty `type`. There are
exactly two built-in types: `terminal` and `widget`.

- A `terminal` owns a live PTY session (shell, emulator, selection, graphics).
- A `widget` is a shell script spawned directly; its stdout renders into the
  surface. `command` is required, and stderr becomes a bounded diagnostic.
- The former data-widget types (`text`, `clock`, `system`, `status`,
  `key_value`, `gauge`, `list`, `log`, `sparkline`, `separator`, `spacer`) are
  removed; on load they migrate to `type = "widget"` with an equivalent
  command and an actionable warning.

- `title` and `command` are optional type-specific fields (`command` is
  required for `widget`).
- `label` accepts `auto`, `always`, or `never`; it controls whether the title is
  drawn in the widget border.
- `settings` is a stable string-to-string map reserved for widget options.
- `settings.padding` is a non-negative number of additional content cells.
- `settings.border` accepts `rounded`, `square`, `double`, `heavy`, `ascii`, or
  `none`; `border_style` is an alias.
- `settings.border_color` and semantic role names such as `foreground`,
  `background`, `focus`, and `muted` accept `inherit`, `ansi:N`, or `#RRGGBB`.
- Widget `settings` (all string-valued):
  - `mode`: `stream` (default) runs once and keeps reading stdout; `interval`
    runs to EOF and re-runs every `interval_ms`.
  - `interval_ms`: re-run cadence for `interval` mode (default `1000`, bounded
    `100..=60000`).
  - `render`: `text` (default); `parse_tags` (`true`/`false`) styles each line
    by its `[error]`/`[warning]`/`[success]`/`[info]` prefix.
  - `max_lines` (default `1024`) and `max_bytes` (default `65536`) bound the
    output ring; overflow drops the oldest lines and records a diagnostic.
  - `restart` (`true` default) restarts an exited script with bounded
    exponential backoff; repeated immediate exits escalate to `Failed` health.
  - `handles_input` (`false` default) forwards focused keys to the script's
    stdin.
  - `session_env` (`true` default) exposes `CMDASH_WIDGET_ID`,
    `CMDASH_WIDGET_TITLE`, `CMDASH_SURFACE_COLUMNS`, `CMDASH_SURFACE_ROWS`,
    `CMDASH_SESSION_COUNT`, `CMDASH_FOCUSED_SESSION`, and
    `CMDASH_FOCUSED_TITLE` at spawn (the session context is a read-only
    snapshot taken at spawn).
  - `session_events`: `off` (default), `text`, or `json` — subscribes the
    widget to bounded terminal-session events (focus, title, line output, and
    exit) delivered as newline-delimited lines on the script's fd 3.
- A terminal widget owns its PTY, emulator, selection, graphics resources, and
  shutdown lifecycle.
- `settings.scrollbar` and `settings.scroll_indicator` accept `true` or `false`
  (default `true`) and toggle the terminal's right-edge scrollbar and its
  title-bar scroll percentage, respectively. Both only appear while scrollback
  exists and are themed with the `focus`/`muted` roles.
- `settings.scrollback` is a non-negative line count (default `10000`) bounding
  the terminal's history. Images that scroll above that limit are evicted and
  their decoded bytes released.
- `settings.term` sets the child's `TERM` environment variable (default
  `xterm-256color`, the universally available value). Set `xterm-kitty` (or
  another capability-rich entry whose terminfo is installed) to make programs
  opt in to the negotiated Kitty keyboard protocol and graphics; the value must
  be a non-empty name of at most 64 bytes.

Appearance is configured through `[appearance]`; see
[APPEARANCE.md](APPEARANCE.md) for semantic roles, parent-terminal palette
inheritance, border styles, label policies, override precedence, and examples.
The default `theme = "inherit"` uses terminal-native reset and ANSI references,
while `theme = "fallback"` selects deterministic RGB colors.

Animation and terminal cursor options are documented in
[ANIMATION.md](ANIMATION.md). The workspace-level `[animation]` section is
optional and disabled by default; invalid motion values reject a reload.

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
creation assigns a fresh widget/session identity. The last visible pane cannot
be closed. Runtime pane trees, ratios, tab selection, and focus are retained
across safe reloads when they remain valid against the edited configuration;
see [WIDGETS.md](WIDGETS.md) for the session and lifecycle implications.

Pane changes are not persisted to disk automatically. To make a runtime layout
permanent, copy its intended widget IDs and layout tree into the file before
restarting cmdash.

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

The command-line interface accepts these configuration options:

```text
cmdash [--config <path> | -c <path>]
cmdash --migrate-config --config <path>
cmdash --api [--api-read-only]
cmdash --api-socket <path>
cmdash --api-disable
```

API flags apply after TOML parsing. See [API.md](API.md) for the local socket
contract and safe read-only/mutating behavior.

`--migrate-config` validates the file and atomically adds or updates the schema
version metadata. It prints each applied migration and does not start the
interactive dashboard. Unsupported future versions are rejected rather than
rewritten.

Common recovery actions:

- Fix duplicate IDs, missing layout references, or unsupported versions shown in
  the diagnostic.
- Restore the last valid file; the active configuration remains in memory while
  a replacement fails validation.
- Use the embedded/default file as a minimal known-good starting point.
- Promote a migration or parser failure to a regression test before upgrading
  the schema.
