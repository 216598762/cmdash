# cmdash

**Layer-based terminal multiplexer and dashboard.** A single Rust
binary that hosts **real pseudoterminals** in recursive splits,
renders their text bodies with **`ratatui`**, and composes every
pane, widget, script process, and overlay as a **per-instance
layer** through the [dashcompositor](https://github.com/216598762/dashcompositor)
graphics layer stack. Output streams to the host terminal via the
**Kitty graphics protocol** (preferred) or **Sixel** (fallback).

cmdash owns **no** graphics path of its own — every pixel-level
render goes through dashcompositor first. The hard rule across
the entire codebase is **one layer per instance**: two visually
adjacent panes are always two different layers, even when the
underlying widget code is the same. That invariant is what makes
nested terminals with Kitty graphics safe, because graphics
emitted from one pane's child never leak into its neighbor.

## At a glance

| | |
| :--- | :--- |
| Latest release | **v1.0.0** (annotated tag at commit `4a403dd`, `--no-sign`) |
| Cargo workspace version | `0.1.0` (see [`Cargo.toml`](Cargo.toml) `[workspace.package]`) |
| License | **MIT** — see [`LICENSE`](LICENSE) |
| Repository | <https://github.com/216598762/cmdash> |
| Rust floor | `1.73` (per `Cargo.toml` `rust-version`) |
| Edition | `2021` |
| Workspace members | 7 crates — see [Workspace layout](#workspace-layout) |
| Local CI | `just clippy-baseline-0`, `just flake-soak` (see [`justfile`](justfile)) |

> **Status at v1.0.0**: all four release-hygiene line items on
> [`docs/1.0-checklist.md`](docs/1.0-checklist.md) (= `DONE-v1.0.0`,
> C2 = `DONE`, C3 = `DONE`, C4 = `DONE-MIT`). Two OPEN items carry
> forward: **B2** (one `#[ignore]`'d test in `cmdash-pty` waiting
> on upstream `portable-pty` upgrade) and **A2** (LLM-judge
> `clean:messy:troll` signal ratio not yet captured — gated on
> `OPENAI_API_KEY`).

## What cmdash is

A Rust workspace that combines:

- a **layout engine** — KDL-configured layout tree of
  `Split / Stack / Pane / Preset` nodes (`cmdash-layout`),
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
- a **plugin model** — c-ABI-safe dynamic widgets (`cmdash-widget-sdk`)
  AND any executable speaking a line-delimited frame protocol
  as a "script widget" (`cmdash-protocol`).

The `cmdash` binary in `crates/cmdash/src/main.rs` wires these
crates together. v1 is single-tab; every frame is one
`TickContext::run` pass through the workspace; unmatched key
presses are forwarded as raw bytes to the focused pane's PTY.

## Workspace layout

The workspace (`Cargo.toml` `[workspace]`) has **7** members:

| Crate | Role |
| :--- | :--- |
| `cmdash` | binary + crate glue — event loop wiring, per-frame orchestration |
| `cmdash-config` | KDL config surface (`parse`, `KeyAction`, `LayoutNode`, `Pane`, `PaneKind`) |
| `cmdash-keybinds` | modifier-aware key router (`Router`), modes (`Normal` / `PaneResize` / `TabSwitch` / `PresetPick`), actions |
| `cmdash-layout` | layout tree engine (`ComputedLayout`, `Direction`, `PaneId`, `Rect`) — Split / Stack / Pane / Preset |
| `cmdash-pty` | `portable-pty` + `vte` → text grid; kitty-graphics split; per-pane reader threads |
| `cmdash-widget-sdk` | c-ABI `CmdashWidget` trait for dynamic Rust widget `cdylib`s |
| `cmdash-protocol` | line-delimited script-widget frame protocol spec (`FRAME` / `KEY` / `RESIZE` / `MOUSE`) |

Integration tests for the binary live inside the `cmdash` crate
itself (no separate `cmdash-tests` workspace member).

## Installation & Build

Clone and build the workspace:

```bash
git clone https://github.com/216598762/cmdash
cd cmdash
cargo build --workspace --release
```

Or install just the `cmdash` binary into your cargo bin:

```bash
cargo install --path crates/cmdash
```

Build requirements:

- A recent stable Rust toolchain (1.73 floor, 1.73+ supported).
- A C compiler and linker for the `dashcompositor` git dep
  (pinned via `Cargo.toml` to `branch = "main"` until upstream
  `v0.4` lands on crates.io).
- A PTY-allocation host (Linux `openpty` / macOS BSD pty).
  See the audit-protocol ledger entry for the current PTY-alloc
  status.

## Running cmdash

Build per [Installation & Build](#installation--build), then:

```bash
# Default launch — no flags. Tracing filter is $RUST_LOG
# (if set), else 'info'.
./target/release/cmdash

# Verbose: every tracing event the binary emits lands.
./target/release/cmdash --log-level=trace

# Quiet: only warnings + errors above.
./target/release/cmdash --log-level=warn

# Help / unknown value → exit 0 / exit 2 with a usage message.
./target/release/cmdash --help
```

The `--log-level=<level>` flag (one of `error` / `warn` /
`info` / `debug` / `trace`, case-insensitive) overrides the
`RUST_LOG` env var and the `info` fallback when set. See
[`docs/configuration.md` §1.4](./docs/configuration.md) for
the full precedence rules and the two Pitfall notes (silent
launch at `--log-level=error`; crate-targeted filtering
required to go through `$RUST_LOG`).

## Architecture (one frame)

The `cmdash::main::TickContext::run` loop iterates `self.runners`
(a `Vec<PaneRunner>`) once per frame and dispatches per pane:

- **Phase 0** drains crossterm events; `Event::Resize(w, h)` arms
  `pending_resize`.
- **Phase 0.5** coalesces `pending_resize.take()` and runs
  `TickContext::relayout(w, h)`, re-resolving the KDL tree against
  the new cell-grid area and per-pane calling
  `PaneRunner::resize(pane.rect)`. v2 lifts `(x, y)` so a Split's
  second child stays at `x = layout_w * ratio`.
- **Phase 1** (per pane) — PTY: `vte::Parser` → text grid;
  native widget: `widget.render(area, frame)`; script widget:
  consume the most recent frame from the exec pipe.
- **Phase 2** — Look up (or create) the pane's `LayerId` in the
  dashcompositor `LayerStack`. Bounds = pane rect (cells →
  pixels). Push overlay layers (tab bar, focus ring, keybind
  help) on top.
- **Phase 3** — `LayerStack::render_to_current_terminal()` →
  `FrameBuffer`, then stream-encode to stdout:
  preferred `dashcompositor::encoder::kitty::encode_passthrough_to_writer`
  (O(1) per write), fallback `dashcompositor::encoder::sixel::encode_to_writer`.
  Degraded text-mode when neither protocol is detected: render
  through ratatui only, log the degraded mode at startup.

`GraphicsState::set_cells((w, h))` propagates the new dimensions
to dashcompositor's framebuffer so the cell-grid → pixel
composition stays in lock-step across host resizes.

## Plugin model

Two distinct extension points — both **physically separate
layers**:

### Native Rust widgets (via `cmdash-widget-sdk`)

c-ABI-safe trait; cmdash loads each widget's `.so` / `.dll`
through `libloading`. User flow:

1. `cargo new --lib my-widget`, set `crate-type = ["cdylib"]`.
2. Add `cmdash-widget-sdk = "<version>"` as a dependency.
3. Export a C symbol `cmdash_widget_create` returning a boxed
   trait object pinned to a published ABI version.
4. Drop the compiled `.so` / `.dll` into
   `~/.config/cmdash/widgets/<name>/`.
5. Reference from layout: `pane { kind "widget" ref "my-widget" }`.

Two instances of the same widget code are two layers, two
`PaneId`s, two loaded copies. Hot-reload is out of scope for v1.

### Script widgets (via `cmdash-protocol`)

Any executable — no compile step, no Rust required. cmdash
spawns the child with piped `stdin` / `stdout` and speaks the
line-delimited frame protocol defined under
`crates/cmdash-protocol/`:

```
# cmdash → script (per frame request)
FRAME width=80 height=24 gen=42
KEY key=h mod=alt
RESIZE w=80 h=24
MOUSE x=10 y=5 kind=press btn=left

# script → cmdash (frame reply)
FRAME width=80 height=24
<ANSI text — interpreted like a tiny terminal>
```

Pixel-bitmap frame mode is a planned v2. v1 = line + ANSI only.
A script process is one layer, one `PaneId`, same as any other pane.

## Local CI surface

`.github/workflows/ci.yml` was intentionally removed in the
dispatch-broken cleanup atom (`7b8eee0`); the canonical local CI
recipe set lives in the [`justfile`](justfile). Run
`just --list` to enumerate all recipes. The two entry points
that gate v1.0.0:

```bash
# Strict-pin clippy residual count (re-baselined to EXPECTED=0).
just clippy-baseline-0

# 300-run SOAK (100 iter × 3 tests) on the un-#[ignore]'d kitty
# tests with a GPT-4.1-mini LLM-judge layer classifying each run
# as clean | messy | troll. Requires OPENAI_API_KEY on the host.
just flake-soak
```

Quick smoke checks (pre-`just`):

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo doc --workspace --no-deps
```

> **`cargo doc` gate.** The doc-build gate is
> `cargo doc -p cmdash --lib --no-deps -D rustdoc::broken-intra-doc-links`
> (lib-crate only — see AGENTS.md for the full hygiene rules and
> the bare-backticks-for-non-resolving-links convention).

## Documentation map

- [`AGENTS.md`](AGENTS.md) — the **project brief**. Read this
  before touching code. Captures the canonical rules
  (one-layer-per-instance, intra-doc-link-hygiene), the
  non-goals, the architecture narrative, the workspace-crate
  table, the keybinding system, the rendering pipeline spec,
  the plugin-model spec, the script-widget protocol shape, and
  the development workflow.
- [`CHANGELOG.md`](CHANGELOG.md) — the release history. Keep-a-
  Changelog format; v1.0.0 is the first stable entry.
- [`docs/1.0-checklist.md`](docs/1.0-checklist.md) — the
  **release-gating ledger**. Captures each 1.0 line item
  (A1 / A2 / B1 / B2 / C1 / C2 / C3 / C4) with status and the
  forward-fixup atom that resolved each item.
- [`docs/ci-evidence.md`](docs/ci-evidence.md) — the
  **audit-protocol ledger**. Captures divergent commit-body
  claims vs measured ground truth across audit cycles 0 → 10.
  Written forward-only as forward-fixup atoms (no amend, no
  rebase, no force-push; per-commit `--no-gpgsign=false`
  host-signature workaround; per-tag `--no-sign` workaround).
- [`docs/configuration.md`](docs/configuration.md) — the
  **user-facing configuration and usage guide**. The canonical
  reference for the KDL config schema (the three top-level
  blocks `layout` / `keybinds` / `presets` and the five-variant
  `LayoutNode` grammar), the chord + action grammar (all 17
  wire-form `KeyAction` strings on 15 enum variants), the
  `axis=horizontal` column-split TRAPDOOR, and six progressively
  complex worked examples. Read this to author or audit a
  `crates/cmdash/config.kdl`. **Pitfall:** the config is
  **embedded at compile time** via `include_str!` — to change
  your config, edit the file and rebuild (no
  `~/.config/cmdash/config.kdl` runtime override in v1).

## Contributing

Forward-only-no-rewind discipline: every commit is a forward-fixup
atom atop the prior chain tip. No amends, no rebases, no
force-pushes. When a commit body references a future atom,
backpatch the SHA into the prior atom's body *before* the future
atom lands (or leave a clear placeholder that a followup atom will
resolve — see the v1.0.0 chain's `109375e` + `c45d6e2` cleanup
atoms for the canonical resolved-placeholder pattern).

Before push, run:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc -p cmdash --lib --no-deps -D rustdoc::broken-intra-doc-links
```

Conventional-commit prefixes used on this chain: `feat:`,
`fix:`, `refactor:`, `docs:`, `test:`, `style:`,
`chore:` (build/deps). New prefixes need a precedent atom first.

## License

MIT — see [`LICENSE`](LICENSE).

```
Copyright (c) 2026 The cmdash authors

Permission is hereby granted, free of charge, to any person
obtaining a copy of this software and associated documentation
files (the "Software"), to deal in the Software without
restriction, including without limitation the rights to use,
copy, modify, merge, publish, distribute, sublicense, and/or
sell copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following
conditions: the above copyright notice and this permission
notice shall be included in all copies or substantial portions
of the Software.
```
