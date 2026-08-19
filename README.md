# cmdash

**A modular terminal dashboard and multiplexer for Linux.**

cmdash turns your terminal into a workspace you compose from two kinds of items:

- **`terminal`** — a live shell session (its own PTY, emulator, scrollback, text selection, and graphics state).
- **`widget`** — a shell script you write; its stdout renders into a pane.

Arrange them in a layout tree of splits, tabs, columns, stacks, and overlays, all
driven by a small TOML file. Every terminal session is fully isolated, and the
whole workspace renders through a retained, byte-diffed compositor, so switching
tabs or panes never leaks text or images between sessions.

> cmdash is a terminal *application*. It runs inside your existing terminal
> emulator — it does not replace your terminal.

---

## Why cmdash

- **One config, one process.** A single `config.toml` describes every pane, its
  layout, and its settings. No daemon, no server, no separate window manager.
- **Real terminal emulation.** Each `terminal` runs `alacritty_terminal` with its
  own PTY — alternate screen, cursor, scrollback, selection, hyperlinks, mouse
  reporting, and shell sessions that behave like a real terminal.
- **First-class graphics.** Kitty graphics are re-emitted to your outer terminal
  protocol-faithfully, including scrollback, animation frames, and per-session
  resource isolation. Optional sixel output is available for dashboard images.
- **Widgets are scripts.** A `widget` is just a command run through your shell.
  Pipe text (or an image) to stdout and it appears in the pane. No plugin SDK,
  no recompilation.
- **Sessions stay isolated.** Tabs keep their emulator, scrollback, selection,
  and image resources alive while hidden and restore them exactly when shown.
- **Degrades gracefully.** If your terminal lacks a capability (Kitty graphics,
  sixel, OSC 52 clipboard), text and layout keep working.

---

## Getting started

You need a Rust toolchain of at least **1.96** (the project's `rust-version`)
and an ordinary ANSI/VT-capable terminal.

```bash
git clone https://github.com/216598762/cmdash.git
cd cmdash
cargo run
```

With no arguments, cmdash looks for configuration in this order:

1. `--config <path>` / `-c <path>` (explicit);
2. `$CMDASH_CONFIG`;
3. `$XDG_CONFIG_HOME/cmdash/config.toml` (or `~/.config/cmdash/config.toml`);
4. `config/default.toml` in a source checkout;
5. a built-in embedded fallback.

To make it your own, copy the checked-in example and launch with an explicit
path:

```bash
mkdir -p ~/.config/cmdash
cp config/default.toml ~/.config/cmdash/config.toml
$EDITOR ~/.config/cmdash/config.toml
cargo run -- --config ~/.config/cmdash/config.toml
```

The same options work with a compiled binary:
`./cmdash --config ~/.config/cmdash/config.toml`.

### A minimal workspace

```toml
version = 1

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

The first pane re-runs `date` every second; the second is an interactive shell.
See the [configuration reference](docs/CONFIGURATION.md) for every option.

---

## Daily use

| Action | Binding |
| --- | --- |
| Cycle focus | `Tab` / `Shift+Tab` |
| Move focus directionally | `Alt+Arrow` |
| Split focused terminal | `Ctrl+Shift+H` (horizontal) / `Ctrl+Shift+V` (vertical) |
| Adjust split ratio | `Ctrl+Shift+Left` / `Ctrl+Shift+Right` |
| Close / merge focused pane | `Ctrl+Shift+W` / `Ctrl+Shift+M` |
| Switch tabs | `Ctrl+PageUp` / `Ctrl+PageDown` |
| Select text | drag (double-click = word, triple-click = line); `Shift+Arrows` keyboard selection |
| Copy selection | `Ctrl+Shift+C` (via OSC 52 when supported) |
| Scrollback | wheel, `Shift+PageUp`/`Shift+PageDown` |
| Command palette / help | `Ctrl+P` / `?` |
| Reload configuration | `Ctrl+R` |
| Quit | `q` / `Esc` |

All of these bindings are configurable through the `[keybindings]` section (see
[CONFIGURATION.md](docs/CONFIGURATION.md)).

---

## Configuration

cmdash uses **versioned TOML** (currently version `1`). The top-level areas are:

- `[workspace]` — name plus the `widgets`, `overlays`, and `layout` tree;
- `[appearance]` — `inherit` (follow your terminal's palette) or `fallback`, plus
  semantic color-role overrides;
- `[animation]` — opt-in retained motion (disabled by default);
- `[api]` — opt-in local Unix-socket automation (disabled and read-only by default);
- `[keybindings]` — remap any action.

Changes to a file-backed configuration are applied by pressing `Ctrl+R`; invalid
edits are rejected without touching the running workspace. Use
`--migrate-config --config <path>` to atomically rewrite an older file's version
metadata.

- [Configuration reference](docs/CONFIGURATION.md) — schema, discovery, layouts, panes, overlays, migration.
- [Appearance guide](docs/APPEARANCE.md) — themes, palette inheritance, borders, labels.
- [Animation guide](docs/ANIMATION.md) — motion, cursor presentation, accessibility.
- [API guide](docs/API.md) — the local automation socket.

---

## Widgets

A `widget` is a shell script invoked as `/bin/sh -c "<command>"`. Everything the
script prints to **stdout** becomes pane content; **stderr** becomes a bounded
diagnostic. Useful options:

- `mode = "interval"` re-runs the script on a cadence; `stream` (default) reads
  continuously.
- `parse_tags = "true"` colors lines by a `[error]`/`[warning]`/`[success]`/
  `[info]` prefix.
- `session_env = "true"` (default) exposes `CMDASH_SESSION_*` context at spawn.
- `session_events = "text" | "json"` subscribes the script to terminal
  focus/title/line/exit events delivered on its **fd 3**.

Widgets can also display images: print `@@CMDASH_IMAGE <base64>` (a JPEG or BMP)
on stdout and, with the `image` feature, the decoded image is shown in the pane
(see [WIDGETS.md](docs/WIDGETS.md)).

The checked-in `config/widgets/` directory has ready-to-copy examples (clock,
uptime, git status, log tail).

---

## Optional features

Everything below is a cargo feature; the default build stays lean and
capability-aware.

| Feature | What it adds |
| --- | --- |
| `sixel` | A bounded 16-color sixel encoder for dashboard-provided images. |
| `image` | JPEG/BMP decoding for the script-widget `@@CMDASH_IMAGE` directive. |
| `watch` | Event-driven config reload-on-save (a `notify` watcher). |
| `wasm-plugins` | The import-free Wasmtime isolation host (a dormant foundation; see below). |

```bash
cargo run --features image,sixel
```

The `wasm-plugins` feature exists as a compile-gated, import-free foundation for
future native plugin isolation; it is not the product's extension model (script
widgets are) and is not required for normal use.

---

## Documentation

| Document | Covers |
| --- | --- |
| [Configuration](docs/CONFIGURATION.md) | TOML schema, discovery, keybindings, layouts, panes, overlays, migration. |
| [Widgets](docs/WIDGETS.md) | The widget model, lifecycle, input, graphics, and the script contract. |
| [Creating widgets](docs/CREATING_WIDGETS.md) | The authoring guide with examples and the factory/context contract. |
| [Appearance](docs/APPEARANCE.md) | Themes, palette inheritance, borders, labels, overrides. |
| [Animation](docs/ANIMATION.md) | Retained motion, cursor presentation, accessibility. |
| [API](docs/API.md) | The local automation socket and its safety model. |
| [Architecture](docs/ARCHITECTURE.md) | Components, render pipeline, session/graphics isolation. |
| [Dependencies](docs/DEPENDENCIES.md) | Dependency decisions and the keep-bespoke rationale. |
| [Roadmap](docs/ROADMAP.md) | The staged implementation plan and its completion record. |

---

## Troubleshooting

- **Config ignored?** Check the discovery order above or pass `--config` explicitly.
- **`Ctrl+R` says it needs a config path?** You started from the embedded fallback; restart with `--config`.
- **Workspace won't start?** Check duplicate IDs, missing layout leaves, unsupported versions, and widget fields.
- **A terminal is blank or too small?** Focus it, resize the outer terminal, and check its layout area.
- **Copy or graphics missing?** The outer terminal may not support OSC 52, Kitty graphics, or sixel. Text and layout continue regardless.
- **A process exits unexpectedly?** Watch the in-app diagnostic footer; set `CMDASH_CRASH_DIR` for a bounded crash report.

---

## License

[MIT](LICENSE)
