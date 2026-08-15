# cmdash

`cmdash` is a Linux terminal application that combines a configurable dashboard with terminal-multiplexer capabilities. It is being designed as a modular compositor: a user can assemble a workspace from terminal sessions, dashboards, and other widgets without requiring terminal sessions at all.

The project is intentionally starting with architecture and behavior contracts before implementation. Read the [configuration reference](docs/CONFIGURATION.md) for the TOML schema and discovery rules, and the [widget guide](docs/WIDGETS.md) for widget lifecycle, rendering, input, plugins, panes, and graphics behavior. The most important rendering requirement is that every terminal session owns an independent terminal-emulation and graphics state. Kitty graphics, sixel content, cursor state, scrollback, and other visual state must remain isolated to the session/tab that produced them and be restored when that session becomes visible again.

## Project documents

- [Architecture](docs/ARCHITECTURE.md) — components, render pipeline, state ownership, and proposed Rust boundaries.
- [Roadmap](docs/ROADMAP.md) — staged implementation plan and acceptance criteria.
- [Configuration reference](docs/CONFIGURATION.md) — TOML discovery, widget/layout options, panes, migrations, and recovery.
- [Widget guide](docs/WIDGETS.md) — widget types, lifecycle, scenes, input, graphics, plugins, panes, and extension guidance.
- [External library candidates](docs/DEPENDENCIES.md) — categorized crate list, evaluation criteria, and selection risks.
- [Default configuration](config/default.toml) — a checked-in widget-only starting point.

## Getting started

The quickest way to try cmdash is from a source checkout. You need a Rust
installation with the toolchain required by the package (`rust-version = 1.96`)
and a terminal that supports the ordinary ANSI/VT controls used by the default
build.

```bash
git clone https://github.com/216598762/cmdash.git
cd cmdash
cargo run
```

With no arguments, cmdash looks for configuration in this order:

1. `$CMDASH_CONFIG`;
2. `$XDG_CONFIG_HOME/cmdash/config.toml` or `~/.config/cmdash/config.toml`;
3. `config/default.toml` in a source checkout;
4. the embedded default configuration.

The default dashboard is widget-only, so it does not start a shell. To create a
personal configuration, copy the example and launch with an explicit path:

```bash
mkdir -p ~/.config/cmdash
cp config/default.toml ~/.config/cmdash/config.toml
$EDITOR ~/.config/cmdash/config.toml
cargo run -- --config ~/.config/cmdash/config.toml
```

The same options work with a built binary, for example
`./cmdash --config ~/.config/cmdash/config.toml`. Configuration is TOML; start
with [CONFIGURATION.md](docs/CONFIGURATION.md) for the schema and
[WIDGETS.md](docs/WIDGETS.md) for widget behavior and examples.

### Daily workflow

- Press `?` for the built-in help overlay or `Ctrl+P` for the command palette.
- Use `Tab` / `Shift+Tab` to cycle focus and `Alt+Arrow` to move between panes.
- Add a terminal pane with `Ctrl+Shift+H` or `Ctrl+Shift+V` when a terminal
  widget is focused. Use `Ctrl+Shift+Left/Right` to adjust its split ratio.
- Close the focused pane with `Ctrl+Shift+W`; merge it from its parent split with
  `Ctrl+Shift+M`. The final visible pane cannot be closed.
- Switch retained tab branches with `Ctrl+PageUp` / `Ctrl+PageDown`.
- Drag in a terminal to select text, then press `Ctrl+Shift+C` to copy through
  OSC 52 when the surrounding terminal supports it.
- Edit a file-backed configuration and press `Ctrl+R` to reload it. Invalid
  changes are rejected without replacing the active workspace. Runtime pane
  changes are retained across reloads but are not automatically written to
  disk, so edit the TOML file if they should survive a restart.
- Press `q` or `Esc` to quit.

For an interactive terminal in the initial layout, add a terminal widget and
layout leaf such as:

```toml
[[workspace.widgets]]
id = 10
type = "terminal"
title = " shell "
command = "sh"

[workspace.layout]
type = "leaf"
widget = 10
```

Optional capabilities are explicit: use `cargo run --features sixel` for the
sixel dashboard-image path or `cargo run --features wasm-plugins` for the
import-free, resource-bounded WASM host foundation. Both remain opt-in and
should be tested in the terminal environment where they will be used.

### Troubleshooting

- **The config is ignored:** check the discovery order above or pass
  `--config /absolute/or/relative/path.toml` explicitly.
- **`Ctrl+R` reports that reload needs a config path:** cmdash started with the
  embedded/default fallback; restart with `--config` or provide a discovered
  file.
- **The workspace does not start:** run the same command with the config path
  and inspect duplicate IDs, missing layout leaves, unsupported versions, and
  invalid widget fields.
- **A terminal is blank or too small:** focus it, resize the outer terminal,
  and check that the configured layout gives it a non-zero area.
- **Copy or graphics do not appear:** the surrounding terminal may not support
  OSC 52, Kitty graphics, or sixel. Text and layout continue without optional
  capabilities.
- **A process exits unexpectedly:** check the in-app diagnostic footer. Set
  `CMDASH_CRASH_DIR` before launching when a bounded crash reproduction report
  is needed.

See the [configuration reference](docs/CONFIGURATION.md), [widget guide](docs/WIDGETS.md),
[architecture](docs/ARCHITECTURE.md), and [roadmap](docs/ROADMAP.md) for deeper
behavior and development details.

## License

cmdash is licensed under the [MIT License](LICENSE).

## Initial principles

1. **Modularity first:** widgets are optional, composable, and not coupled to terminal sessions.
2. **Session isolation:** each terminal tab has its own PTY, emulator, render state, and graphics resource namespace.
3. **Retained rendering:** widgets produce renderable scene data; the backend owns terminal I/O and frame submission.
4. **External crates where practical:** parsing, PTY management, async execution, layout, and terminal backends should use mature Rust libraries rather than bespoke implementations.
5. **Capability-aware behavior:** terminal features are detected and negotiated; unsupported graphics protocols degrade without corrupting layout or text.
6. **Testable core:** terminal state, layout, composition, and protocol handling should be testable without an attached interactive terminal.

## Status

Phase 10 configuration onboarding is complete for the current contract. The project has retained session-scoped graphics, bounded resource diagnostics, validated config reload and migration reporting, terminal selection/copy through OSC 52, a command palette/help surface, stabilized plugin metadata, Wasmtime isolation foundations, interactive pane focus/resize/close commands, fuzz targets and CI smoke runs, crash reproduction artifacts, and multi-target release packaging. `Ctrl+PageUp` / `Ctrl+PageDown` switch tabs, `Alt+Arrow` moves pane focus, `Ctrl+Shift+Arrow` adjusts pane ratios, `Ctrl+Shift+W` closes the focused pane, `Ctrl+P` opens the palette, `?` opens help, `Ctrl+Shift+C` copies a selection, and `--config <path>` / `-c <path>` enables safe reload with `Ctrl+R`; `--migrate-config --config <path>` rewrites legacy version metadata atomically.

Optional sixel support is enabled with `--features sixel`; the default build remains capability-aware. The feature provides a bounded 16-color RGB dashboard-image encoder, while terminal-originated Kitty graphics continue to use the session-owned retained graphics path. Optional isolated WASM plugins are enabled with `--features wasm-plugins`; modules have no imports/WASI access and are subject to size and execution-budget policy.
