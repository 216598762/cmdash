# AGENTS.md — cmdash

A terminal multiplexer and dashboard, written in Rust. This file is the project
brief: read it before touching code. Future agents (and humans) are expected to
honor the constraints below.

## What cmdash is

A single Rust binary that:

- embeds real pseudoterminals (recursive splits),
- renders their text bodies with `ratatui`,
- composes **per-instance layers** (each terminal pane, widget, script
  process, and overlay is its own layer) through
  [dashcompositor](https://github.com/216598762/dashcompositor),
- writes the result to the host terminal through the Kitty graphics
  protocol (preferred) or Sixel (fallback).

It is a multiplexer (tmux/zellij-family) plus a widget dashboard, glued
together by a layer architecture that physically owns every pane on its own
composited layer rather than treating panes as rectangles in a shared text
grid.

cmdash owns **no** graphics path of its own. Every pixel-level render and
every graphics escape sequence cmdash writes goes through dashcompositor
first. If a feature seems to need a parallel graphics pipeline, the design
is wrong.

## Hard rule: one layer per instance

Every pane, widget, and overlay instance gets its own entry in the
`dashcompositor::LayerStack`. When a pane is opened it allocates a fresh
`LayerId`; when it closes the layer is torn down. Bounds, opacity, and
z-order on that layer are driven by the layout engine, but the layer
identity is bound to the pane identity 1:1 for the pane's whole lifetime.

Concretely:

- Two visually adjacent panes are two different layers. Always.
- Two instances of the same widget — same library, same name, same code —
  are still two layers. Always.
- Nested panes (a terminal opened inside a widget, a script spawned from a
  terminal, etc.) each add their own layer.
- A layer is never re-bound to a different pane. Once allocated, its
  `LayerId` is read-only for that pane; on close it is destroyed.

This invariant is what makes nested terminals with the Kitty graphics
protocol safe: graphics emitted by one pane are routed into that pane's
layer, never leaking into its neighbors.

## Hard rule: intra-doc-link hygiene

`cargo doc -p cmdash --lib --no-deps -D rustdoc::broken-intra-doc-links`
is the project doc-build gate. It runs against the **lib crate only**,
so any `[..]` intra-doc-link that points outside the lib crate's
rendered surface — bin entrypoints, `#[cfg(test)] mod` items,
private methods on public structs — is rejected as broken at pre-push
time. Bare backticks are the safe fallback for anything that won't
resolve.

Concretely:

- **`[`crate::main::X`]` ALWAYS fails** in lib-crate rustdoc (bin
  entrypoint, not a lib module). Commit `5a8f4a2`.
- **`[..]` NEVER resolves to items inside `#[cfg(test)] mod
  internal_sanity_tests`** of the same crate (test mods excluded
  from public-doc surface). Commit `bbc28c1` (pane.rs:76) + commit
  `4ded9e9` (graphics.rs 5 links).
- **`[`crate::xyz::X`]` MAY resolve** in cross-crate rustdoc if
  `xyz` is the EXTERNAL crate name; there is no `main.rs` in
  `cmdash_layout`, so the `crate::main` rule does NOT apply.
  Cross-crate example: `[`cmdash_layout::split_rect`]` works from
  cmdash's lib rustdoc.
- **Bare backticks are the safe fallback** for anything that won't
  resolve: bin-local items (`TickContext::run`), test-mod stubs
  (`StubPty`, `Self::push_image`), private methods on public
  structs (`GraphicsState::on_kitty`). The last case is subtle:
  the lint runs against the public-doc surface, so it rejects
  private methods the source AST would otherwise link.

This invariant is what makes the cargo-doc gate enforceable. Without
the disciplined bare-backtick fallback, every docs-only commit risks
a gate regression.

## Non-goals

- Do **not** reimplement graphics composition — dashcompositor does it.
- Do **not** reimplement font rasterization — opt into dashcompositor's
  `font-rasterizer` feature (fontdue under the hood).
- Do **not** embed a scripting VM for user widgets — script widgets are
  arbitrary executables that speak the cmdash frame protocol.
- Do **not** implement shell/job control — cmdash hosts shells, it is not a
  shell.

## Architecture

```
┌─────────────────────────── cmdash ──────────────────────────┐
│ ┌───────────┐ ┌───────────┐ ┌───────────┐         ...       │
│ │ pane A    │ │ pane B    │ │ pane C    │                  │
│ │ (PTY)     │ │ (widget)  │ │ (script)  │                  │
│ └─────┬─────┘ └─────┬─────┘ └─────┬─────┘                  │
│       ▼ text grid  ▼ draw()      ▼ frame proto             │
│ ┌────────────────────────────────────────────┐             │
│ │ per-instance LayerStack                    │             │
│ │ (one LayerId per pane, even when adjacent) │             │
│ └────────────────┬───────────────────────────┘             │
│                  ▼                                          │
│         dashcompositor::LayerStack::render                  │
│                  ▼                                          │
│  encoder: kitty | sixel   (streaming, O(1) per write)       │
└──────────────────────────────────────────────────────────────┘
```

### Workspace crates

| crate                  | role                                                     |
| ---------------------- | -------------------------------------------------------- |
| `cmdash`               | binary: event loop, glue                                 |
| `cmdash-config`        | KDL types + parser                                       |
| `cmdash-layout`        | layout tree: Split / Stack / Pane / Preset               |
| `cmdash-keybinds`      | modifier-aware key router, modes, actions                |
| `cmdash-pty`           | `portable-pty` + `vte` → text grid, kitty-graphics split |
| `cmdash-widget-sdk`    | c-ABI trait for dynamic widgets (`CmdashWidget`)         |
| `cmdash-protocol`      | line-delimited script-widget frame protocol spec         |

### v2 multi-pane per-tick iteration

The `cmdash::main::TickContext::run` loop iterates `self.runners`
(a `Vec<PaneRunner>`) once per frame and dispatches per pane:

- Phase 0 drains crossterm events; `Event::Resize(w, h)` arms
  `pending_resize`.
- Phase 0.5 coalesces `pending_resize.take()` and runs
  `TickContext::relayout(w, h)`, re-resolving the KDL tree against
  the new cell-grid area and per-pane calling
  `PaneRunner::resize(pane.rect)`. v2 lifts `(x, y)` so a Split's
  second child stays at `x = layout_w * ratio`.
- Phase 3a draws each pane to
  `ratatui::layout::Rect::new(runner.computed().rect.x,
  runner.computed().rect.y, runner.computed().rect.w,
  runner.computed().rect.h)` — no v1-style `(0, 0)` clobber.

`GraphicsState::set_cells((w, h))` propagates the new dims to
dashcompositor's framebuffer so the cell-grid → pixel composition
stays in lock-step.

## Key dependencies (do not reinvent these)

Pulled from [awesome-rust](https://github.com/rust-unofficial/awesome-rust)
and [awesome-ratatui](https://github.com/ratatui/awesome-ratatui).

- `dashcompositor` from `https://github.com/216598762/dashcompositor`,
  pinned to `branch = "main"` with `default-features = false` and
  features `kitty-encoder`, `sixel-encoder`, `image-decoder`,
  `font-rasterizer` (the last is renderer-only). Upstream `v0.4` is
  not yet on crates.io and is not tagged upstream (`HEAD` is
  `v0.11.0`); once `v0.4` lands on crates.io, switch the dep to
  `version = "0.4"` and drop the git fields.
- `ratatui` — text rendering, widget toolkit, frame.
- `portable-pty` — every child PTY, no roll-our-own.
- `vte` — VT/ANSI parser into a text grid; never hand-roll.
- `libloading` — hot-load widget `.so` / `.dll` files at runtime.
- `knus` or `facet-kdl` — KDL parser.
- `figment` — layered config: file → env → CLI.
- `tracing` + `tracing-subscriber` — structured logging.
- `tokio` — async runtime for IO and event dispatch.
- `serde` / `serde_json` — internal messages.
- `anyhow` / `thiserror` at crate seams.

## Feature checklist

1. **Nested terminals.** Recursive splits; every split level adds panes
   without a hard depth limit (only OS resource limits). Each nested
   terminal is its own layer.
2. **Kitty graphics protocol — full support.** Concretely this means:
   - cmdash emits graphics escape sequences via
     `dashcompositor::encoder::kitty::encode_passthrough_to_writer`
     (the O(1) streaming entry point);
   - graphics escape sequences emitted by a nested PTY child are
     intercepted by `cmdash-pty` and re-routed into the originating
     pane's layer (so `img2sixel`-style apps work inside nested
     terminals);
   - placement / control commands (`a=p`, `a=d`, `d`, `c`, …) are
     honored against the pane's pixel rect and reapplied on resize.
3. **Tabs.** Tab bar is its own layer; each tab holds one layout tree.
   Keybinds `M-1`..`M-9` switch, `M-t` new, `M-w` close.
4. **Drop-in ratatui widgets.** Rust users author against
   `cmdash-widget-sdk` and produce a `cdylib`. cmdash loads the lib via
   `libloading`. Each loaded widget instance is a separate layer.
   Hot-reload is out of scope for v1.
5. **Drop-in scripts as widgets.** Any executable. cmdash opens
   `stdin`/`stdout` pipes and speaks the line-delimited frame protocol
   defined in `crates/cmdash-protocol/README.md`.
6. **Modifier-based keybinds.** One global modifier. Default is
   `MOD_LEFTALT`; config-overridable. See *Keybinding system* below.
7. **Custom keybinds.** User-defined in KDL.
8. **Configurable layouts.** KDL layout tree.
9. **Layout presets.** Named presets, callable by keybind
   (`M-p` enters preset picker, or directly via `RunPreset("name")`).
10. **v2 multi-pane reflow across host resizes.** Each
    `Event::Resize` from the host terminal cascades through
    `TickContext::relayout`; every pane's `computed().rect` is
    updated against the new `cmdash_layout::Rect` so a Split's
    children stay at their `(ratio * w, 0)` offsets. The v2
    `PaneRunner::resize(rect: LayoutRect)` signature is what makes
    this coordinate-safe; v1's `(cols, rows)` API clobbered the
    origin.

## Keybinding system

- One global **modifier** (`MOD_LEFTALT` by default). Overridable in
  config to `ctrl`, `super`, or `shift` (shift is discouraged because it
  conflicts with text selection).
- **Modes:** `Normal`, `PaneResize`, `TabSwitch`, `PresetPick` — each
  with its own binding set.
- **Actions:** enum-driven, e.g. `FocusNext`, `FocusPrev`, `SplitV`,
  `SplitH`, `NewTab`, `CloseTab`, `SwitchTab(n)`, `RunPreset(name)`,
  `ReloadConfig`, `SetModifier(mod)`.

KDL binding example:

```kdl
keybind {
    mod "alt"
    key "c"
    action "run_preset"
    arg "coding"
}
```

## Layout engine (KDL)

Layout node kinds:

- `split { axis "h"|"v", ratio 0.6, a {...}, b {...} }`
- `stack { a {...}, b {...} }` — internal tabbed viewer
- `pane { kind "pty"|"widget"|"script" ref "<name>" title "..." }`
- `preset name "coding" { ...layout body... }`

Resolved each frame into a tree of `(PaneId, Rect)`. Resolution is
deterministic so that layer ids stay stable across relayouts of the same
tab. **Idempotent:** two `ComputedLayout::compute` calls against the
same tree produce the same `PaneId`s, so the tick loop's per-pane
pairing (`runners[i].computed().id == layout.panes[i].id`) holds
across host resizes. The v2 `PaneRunner::resize(rect: LayoutRect)`
signature (Phase 1, commit `de7ccae`) is the API that lets the blit
path at phase 3a honor the Split-derived `x`/`y` instead of v1's
hardcoded `(0, 0)` clobber.

## Rendering pipeline (one frame)

1. Resolve the active tab's layout tree into `(PaneId, Rect)[]`.
   **v2:** resolve runs at `cmdash::run` entry against the host's
   `crossterm::terminal::size()`, and re-runs once per host
   `Event::Resize(w, h)` via `TickContext::relayout(w, h)`. The
   `Tree → (PaneId, Rect)` cache means phase 3a always reads the
   current `(PaneId, Rect)` without a tree walk.
2. For each pane:
   - **PTY** → advance `vte`, get a `TextGrid`, draw into a ratatui
     `Frame` for the text body.
   - **Native widget** → call `widget.render(area, frame)`.
   - **Script widget** → consume the most recent frame from the exec
     pipe, blit into the region.
3. Look up (or create) the pane's `LayerId` in the dashcompositor
   `LayerStack`. Bounds = pane rect in cells → pixels.
4. Push overlay layers (tab bar, focus ring, keybind help) on top.
5. `LayerStack::render_to_current_terminal()` → `FrameBuffer`.
6. Stream-encode to stdout:
   - preferred: `dashcompositor::encoder::kitty::encode_passthrough_to_writer`
   - fallback: `dashcompositor::encoder::sixel::encode_to_writer`
7. **Degraded text-mode** when neither protocol is detected: render
   through ratatui only, log the degraded mode at startup.

## Plugin model — native Rust widgets

`cmdash-widget-sdk` exposes a c-ABI-safe trait:

```rust
pub trait CmdashWidget: Send + Sync {
    fn new() -> Self where Self: Sized;
    fn on_event(&mut self, evt: WidgetEvent);
    fn render(&mut self, area: Rect, frame: &mut ratatui::Frame);
}
```

User flow:

1. `cargo new --lib my-widget`, set `crate-type = ["cdylib"]`.
2. Add `cmdash-widget-sdk = "<version>"` as a dep.
3. Export a C symbol `cmdash_widget_create` returning a boxed trait object
   pinned to a published ABI version.
4. Drop the compiled `.so` / `.dll` into
   `~/.config/cmdash/widgets/<name>/`.
5. Reference from layout: `pane { kind "widget" ref "my-widget" }`.

Two instances of the same widget are two layers, two `PaneId`s, two
loaded copies.

## Script-as-widget protocol

Spawn a child with piped `stdin`/`stdout`. Line-delimited, versioned
prefix:

cmdash → script (per frame request):

```
FRAME width=80 height=24 gen=42
KEY key=h mod=alt
RESIZE w=80 h=24
MOUSE x=10 y=5 kind=press btn=left
```

script → cmdash (frame reply):

```
FRAME width=80 height=24
<ANSI text — interpreted like a tiny terminal>
```

Pixel-bitmap frame mode is a planned v2. v1 = line+ANSI only. Full spec
lives at `crates/cmdash-protocol/README.md` (not yet written — write it
before implementing script widgets).

A script process is one layer, one `PaneId`, same as any other pane.

## Nested-terminal graphics handling

When a child PTY emits kitty graphics commands, `cmdash-pty` intercepts
them in the `vte` stream:

- image uploads are buffered and turned into
  `dashcompositor::ImageLayer`s bound to the originating pane's `LayerId`;
- placement commands (`a=p`, `a=d`, `d=a`, etc.) update the layer's
  position/size;
- on pane resize, placements are reapplied against the new pixel rect.

cmdash does **not** blindly forward graphics escape sequences through its
own stdout — placement is fragile and pane-local. Capture, extract, route.

## Config & on-disk layout

- `~/.config/cmdash/config.kdl` — global: modifier, theme, keybinds,
  default tab.
- `~/.config/cmdash/layouts/*.kdl` — named layout presets.
- `~/.config/cmdash/widgets/<name>/` — installed dynamic libs and/or
  script executables.

Discovery: `figment` over `knus`-parsed KDL files, with env-var overrides
(`CMDASH_MODIFIER`, `CMDASH_CONFIG_DIR`).

## Repository layout (planned)

```
cmdash/
├── Cargo.toml              # workspace
├── crates/
│   ├── cmdash/             # binary
│   ├── cmdash-config/
│   ├── cmdash-layout/
│   ├── cmdash-keybinds/
│   ├── cmdash-pty/
│   ├── cmdash-widget-sdk/
│   └── cmdash-protocol/
├── examples/
│   ├── widget-clock/       # cdylib sample
│   ├── script-hello/       # exec sample
│   └── layouts/            # example KDL layouts
└── docs/
    ├── script-widget-protocol.md
    └── kitty-in-nested-pty.md
```

## Phase 2 of v2 split-pane nesting

Phase 1 — *v2 `PaneRunner::resize` contract lift* (commit `de7ccae`)
— changed the signature from `(cols: u16, rows: u16)` to
`(rect: cmdash_layout::Rect)`. The body is
`self.pty.resize(rect.w, rect.h)?; self.computed.rect = rect;`,
so the layout-engine's `(x, y)` carry forward instead of v1's
hardcoded `(0, 0)`. The cached `ComputedPane::rect` now matches
the layout engine across the pane's whole lifetime. The pairing
invariant (`runners[i].computed().id == layout.panes[i].id`) is
locked at the `computed()` accessor and verified by every
regression test in `cmdash::src::main.rs::input_tests` and
`wiring_smoke.rs`.

Phase 2 — *host SIGWINCH multi-pane wiring* (commit `31c47b7`) —
sources the initial cell-grid area from
`crossterm::terminal::size()` with a zero-area / `Err` fallback
to `(80, 24)` and drops the hardcoded `DEFAULT_AREA_*` constants.
`cmdash::main::TickContext` gains two fields (`layout_root:
LayoutNode`, `pending_resize: Option<(u16, u16)>`), a top-of-tick
phase 0.5 that coalesces `Event::Resize` signals, and a
`TickContext::relayout(w, h)` helper that re-runs
`ComputedLayout::compute` and per-pane calls `runner.resize(pane.rect)`.
`GraphicsState::set_cells` propagates the new dims to
dashcompositor's framebuffer. Six new tests pin the behavior: the
`assert_eq!(runner.computed().id, pane.id)` pairing,
coalesce-on-overwrite, and the Split-derived rect round-trip
against real PTY children.

Phase 2 carry-forward — *focus navigation + runtime mutations*
(next) — wires the AGENTS.md `KeyAction` arms that are currently
no-ops in `cmdash::main::apply_action`:

- `AppNewPane`: insert a leaf into the live KDL tree under
  `TickContext::layout_root`; reflow on the new leaf count via
  `relayout`.
- `PaneFocus{Up,Down,Left,Right}`: look up the adjacent pane via
  `ComputedLayout`'s side-of-rect resolution; swap
  `TickContext.focus`. v1's existing `PaneFocus{Next,Prev}`
  wrappers stay as lexical-order fallbacks.
- `PanePreset(name)`: swap the active layout tree from
  `cmdash_config::Config.presets`, drive a `relayout` on the new
  tree.

Each branch needs a regression test in
`cmdash::src::main.rs::input_tests` against a multi-pane fixture
and a focused `wiring_smoke.rs` test that drives the same path
through real `PaneRunner::spawn_with_graphics` children.

## Development workflow

- Commit often. Multi-line commit messages, conventional prefix:
  `feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `style:` (see
  commit `14ad9a0`), `chore:` (build/deps); each new prefix needs
  a precedent atom first.
- Push major changes. (No remote configured yet — add one when the user
  provides a URL, then push.)
- Feature branches: `feat/<short-name>`. Squash into `main`.
- Before push: `cargo clippy --workspace --all-targets -- -D warnings`
  and `cargo test --workspace` — both must pass.
- Reuse over reinvention: search awesome-rust / awesome-ratatui first.
  If a crate does the job, pull it in.
- Always run `cargo fmt --all` before committing.
- **GPG signing (TTY-less hosts).** If the host's `gpg-agent` cannot satisfy
  passphrase requests through its standard cache path (e.g. `ERR 67108933
  Not implemented` on the `preset_passphrase` assuan command, or
  `gpg-preset-passphrase` binary missing on the host's PATH), use the
  reproducible `scripts/gpg-cmdash-wrapper.sh`. The wrapper is committed
  to the repo and contains NO secrets; the user's GPG key passphrase
  lives in `~/.config/cmdash/gpg-passphrase` (chmod 600, host-local,
  NOT committed; `.gitignore` excludes `*gpg-passphrase*` patterns).
  Run `just gpg-setup` once per host to wire git's `gpg.program` +
  re-enable `commit.gpgsign=true`. See
  `scripts/gpg-cmdash-wrapper.README.md` + `docs/ci-evidence.md` audit
  cycle 12 for the full diagnostic.
- **doc-link-hygiene workflow.** When swapping a `[..]` intra-doc-link
  for a bare-backtick form to clear the rustdoc gate, commit TWO
  atoms atomically: the FIX itself uses a `fix:` prefix; the
  NARRATIVE atom documenting the fix-design decision uses a `docs:`
  prefix via `git commit --allow-empty`. See commit `15e4362` for the
  canonical pattern.
- In the NARRATIVE atom's body, NAME the audit-requested form verbatim
  (e.g. `[`crate::pane::StubPty`]`, not "the more-qualified form") and
  cite the file:line of any `#[cfg(test)]`-private item.
- Cite the eventual resolution for out-of-scope items in the same
  chain (e.g. `bbc28c1` → `4ded9e9` for graphics.rs's 5 residuals).
- When genuinely stuck on a design choice, ask the user with concrete
  options rather than picking silently.

## MUST (for agents and contributors)

- Allocate one `dashcompositor::LayerId` per `PaneId` and never share
  one layer across two panes.
- Use dashcompositor's streaming encoders for the final write
  (`encode_passthrough_to_writer`, `sixel::encode_to_writer`).
- Use `portable-pty` for every child PTY; never roll a POSIX-only path.
- Use the KDL parser for config; never hand-roll.
- Place every widget — native or script — in its own layer, even when
  adjacent.
- Run `cargo clippy` and `cargo test` before pushing.

## MUST NOT

- Reimplement graphics composition.
- Reimplement VT parsing (`vte` is the answer).
- Embed a scripting VM for user scripts.
- Blind-forward kitty graphics escape sequences through cmdash's own
  stdout.
- Mutate a `LayerId` once it's bound to a pane; tear it down at pane
  close.
- Reach for a second graphics pipeline outside dashcompositor.
