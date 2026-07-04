# cmdash

Layer-based terminal multiplexer and dashboard.

A Rust workspace that combines a layout engine, PTY layer,
ratatui text rendering, and a dashcompositor-based kitty
graphics output path. The `cmdash` binary wires these crates
together: layout to PTY to ratatui text body, with crossterm
input dispatch through the keybinds router.

- **Version**: 0.1.0
- **License**: MIT (see [`LICENSE`](LICENSE))
- **Repository**: https://github.com/216598762/cmdash
- **Rust version**: 1.73+ (the `Cargo.toml` rust-version floor)

## Workspace layout

The workspace (`Cargo.toml`) has 7 members:

| Crate | Role |
| :--- | :--- |
| `cmdash` | binary + crate glue (event loop wiring) |
| `cmdash-config` | config parsing (`parse`, `KeyAction`, `LayoutNode`, `Pane`, `PaneKind`, ...) |
| `cmdash-keybinds` | keybind dispatch (`Router`) |
| `cmdash-layout` | layout engine (`ComputedLayout`, `Direction`, `PaneId`, `Rect`) |
| `cmdash-pty` | PTY layer (`PaneLayerId`, `PaneEvent`, `ShellSpec`) |
| `cmdash-widget-sdk` | widget SDK |
| `cmdash-protocol` | protocol primitives |

The `cmdash` binary in `crates/cmdash/src/main.rs` drives the
rendering pipeline: phase 3a draws the cell body through ratatui
and phase 3b emits dashcompositor kitty graphics via the
passthrough encoder. v1 is single-tab with sync IO via per-pane
reader threads; unmatched key presses are forwarded as raw bytes
to the focused pane's PTY.

## Installation

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
- A recent stable Rust toolchain (1.73 floor).
- A C compiler and linker for the `dashcompositor` git dep
  (`dashcompositor = { git = "...", branch = "main" }` in
  `Cargo.toml`).
- A PTY-allocation host (Linux `openpty` / macOS BSD pty).
  See `cmdash-pty`'s `SlavePty::as_raw_fd` story in the
  audit-protocol ledger for the current PTY-alloc status.

## Local CI surface

This repo's `.github/workflows/ci.yml` was removed in the
dispatch-broken cleanup atom (`7b8eee0`) and is intentionally
absent on the post-cleanup chain. The canonical local CI
recipe set lives in the [`justfile`](justfile); run
`just --list` to enumerate all recipes. Key entry points:

```bash
# Strict-pin clippy residual count (re-baselined to EXPECTED=0).
just clippy-baseline-0

# 300-run SOAK on the 3 newly-un-#[ignore]'d kitty tests,
# with a GPT-4.1-mini LLM-judge layer classifying each run as
# clean | messy | troll. Requires OPENAI_API_KEY on the host.
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

## Documentation

- [`docs/1.0-checklist.md`](docs/1.0-checklist.md) -- the
  release-gating ledger. Captures each open 1.0 line item
  (CI-surface atoms + test-baseline atoms + release-hygiene
  atoms) with current status and the forward-fixup atoms
  that resolved each item.
- [`docs/ci-evidence.md`](docs/ci-evidence.md) -- the
  audit-protocol ledger. Captures divergent commit-body
  claims vs measured ground truth, written-forward-only as
  forward-fixup atoms (no amend, no rebase, no force-push).

## License

MIT -- see [`LICENSE`](LICENSE).

```
Copyright (c) 2026 The cmdash authors
```

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
