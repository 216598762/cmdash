# cmdash

**Layer-based terminal multiplexer and dashboard.** A single Rust
binary that hosts **real pseudoterminals** in recursive splits,
renders their text bodies with **`ratatui`**, and composes every
pane, widget, script process, and overlay as a **per-instance
layer** through the [dashcompositor](https://github.com/216598762/dashcompositor)
graphics layer stack. Output streams to the host terminal via the
**Kitty graphics protocol** (preferred) or **Sixel** (fallback).

cmdash owns **no** graphics path of its own — every pixel-level
render goes through dashcompositor first. The hard rule across the
entire codebase is **one layer per instance**: two visually adjacent
panes are always two different layers, even when the underlying
widget code is the same. That invariant is what makes nested
terminals with Kitty graphics safe, because graphics emitted from
one pane's child never leak into its neighbor.

## At a glance

| | |
| :--- | :--- |
| Version | `0.1.0` (workspace) |
| License | **MIT** — see [`LICENSE`](LICENSE) |
| Repository | <https://github.com/216598762/cmdash> |
| Rust floor | `1.73` (per `Cargo.toml` `rust-version`) |
| Edition | `2021` |
| Workspace members | 7 crates — see [Workspace layout](#workspace-layout) |
| Local CI | `just clippy-baseline-0`, `just lint-doc` (see [`justfile`](justfile)) |

## What cmdash is

A Rust workspace that combines:

- a **layout engine** — KDL-configured layout tree of
  `Split / Stack / ZStack / Pane / Preset` nodes (`cmdash-layout`),
- a **PTY layer** — `portable-pty` + `vte` per pane producing
  text grids (`cmdash-pty`),
- a **config surface** — KDL types + parser for keybinds,
  modifiers, layouts (`cmdash-config` + `cmdash-keybinds`),
- a **ratatui text renderer** — per-frame cell-grid blit
  (`cmdash` binary, the integrator crate),
- a **dashcompositor layer-stack renderer** — per-instance
  `LayerId` ownership, passthrough Kitty-graphics encoder,
  Sixel fallback (`cmdash` + [`dashcompositor`](https://github.com/216598762/dashcompositor)
  git dep),
- a **plugin model** — c-ABI-safe dynamic widgets (`cmdash-widget-sdk`,
  planned) AND any executable speaking a line-delimited frame protocol
  as a "script widget" (`cmdash-protocol`, planned).

The `cmdash` binary in `crates/cmdash/src/main.rs` wires these
crates together. v1 is single-tab; every frame is one
`TickContext::run` pass through the workspace; unmatched key
presses are forwarded as raw bytes to the focused pane's PTY.

## Workspace layout

| Crate | Role |
| :--- | :--- |
| `cmdash` | binary + crate glue — event loop wiring, per-frame orchestration |
| `cmdash-config` | KDL config surface (`parse`, `KeyAction`, `LayoutNode`, `Pane`, `PaneKind`) |
| `cmdash-keybinds` | modifier-aware key router (`Router`), modes (`Normal` / `PaneResize` / `TabSwitch` / `PresetPick`), actions |
| `cmdash-layout` | layout tree engine (`ComputedLayout`, `Direction`, `PaneId`, `Rect`) — Split / Stack / ZStack / Pane / Preset |
| `cmdash-pty` | `portable-pty` + `vte` → text grid; kitty-graphics split; per-pane reader threads |
| `cmdash-widget-sdk` | c-ABI `CmdashWidget` trait for dynamic Rust widget `cdylib`s (stub) |
| `cmdash-protocol` | line-delimited script-widget frame protocol spec (stub) |

## Installation & Build

```bash
git clone https://github.com/216598762/cmdash
cd cmdash
cargo build --workspace --release
```

Or install just the `cmdash` binary:

```bash
cargo install --path crates/cmdash
```

Build requirements:

- A recent stable Rust toolchain (1.73 floor).
- A C compiler and linker for the `dashcompositor` git dep.
- A PTY-allocation host (Linux `openpty` / macOS BSD pty).

## Running cmdash

```bash
# Default launch — no flags. Tracing writes to stdout at info level.
./target/release/cmdash

# Capture every tracing event to a file. Stdout stays silent;
# a stderr banner announces the launch. Trace is forced in file mode.
./target/release/cmdash --log=/tmp/cmdash-debug.log

# Quiet on stdout — warnings + errors only.
RUST_LOG=warn ./target/release/cmdash
```

The `--log=<path>` flag controls **where** tracing events land;
`RUST_LOG` controls **what** filter applies. They're orthogonal:
`--log=<path>` (file mode) ignores `RUST_LOG`; without it (stdout
mode), `RUST_LOG` is honored. See
[`docs/configuration.md` §1.4](./docs/configuration.md) for details.

## Architecture (one frame)

The `TickContext::run` loop iterates `self.runners` once per frame:

- **Phase 0** — drain crossterm events; route keybinds or forward
  raw bytes to focused pane's PTY.
- **Phase 0.5** — coalesce host resize, re-resolve layout tree,
  resize each pane.
- **Phase 1** — drain close-channel, poll exits, snapshot grids.
- **Phase 2** — route kitty graphics events into `GraphicsState`.
- **Phase 3a** — render text grids through ratatui.
- **Phase 3b** — emit dashcompositor kitty/sixel graphics.

`GraphicsState::set_cells((w, h))` keeps the framebuffer in sync
with the cell grid across host resizes.

## Local CI surface

Run `just --list` to enumerate all recipes:

```bash
# Strict-pin clippy residual count at 0.
just clippy-baseline-0

# Targeted doc-lint check (fast, single-lint).
just lint-doc
```

Quick smoke checks (pre-push):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS='-D rustdoc::broken-intra-doc-links' cargo doc -p cmdash --lib --no-deps
```

## Documentation map

- [`AGENTS.md`](AGENTS.md) — **project brief**. Architecture rules,
  non-goals, workspace layout, render loop, keybinding system,
  plugin model, development workflow.
- [`docs/roadmap.md`](docs/roadmap.md) — **development roadmap**.
  Prioritized plan for future work (config loading, tab bar,
  widget SDK, script protocol, and more).
- [`docs/configuration.md`](docs/configuration.md) — **user-facing
  configuration and usage guide**. KDL schema, chord/action grammar,
  worked examples.
- [`CHANGELOG.md`](CHANGELOG.md) — release history.
- [`examples/`](examples/) — standalone `.kdl` config files.

## Contributing

Before push, run:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS='-D rustdoc::broken-intra-doc-links' cargo doc -p cmdash --lib --no-deps
```

Conventional-commit prefixes: `feat:`, `fix:`, `refactor:`, `docs:`,
`test:`, `style:`, `chore:`.

## License

MIT — see [`LICENSE`](LICENSE).
