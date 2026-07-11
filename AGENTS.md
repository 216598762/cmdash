# AGENTS.md — cmdash

A terminal multiplexer and dashboard, written in Rust. Read this
before touching code.

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
together by a layer architecture that physically owns every pane on its
own composited layer rather than treating panes as rectangles in a shared
text grid.

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
| `cmdash-layout`        | layout tree: Split / Stack / ZStack / Pane / Preset      |
| `cmdash-keybinds`      | modifier-aware key router, modes, actions                |
| `cmdash-pty`           | `portable-pty` + `vte` → text grid, kitty-graphics split |
| `cmdash-widget-sdk`    | c-ABI trait for dynamic widgets (`CmdashWidget`)         |
| `cmdash-protocol`      | line-delimited script-widget frame protocol spec          |

### Render loop (one frame)

The `TickContext::run` loop iterates `self.runners` (a `Vec<PaneRunner>`)
once per frame (~30 fps, 33 ms tick):

1. **Phase 0** — drain crossterm input events. Unmatched key presses are
   forwarded as raw bytes to the focused pane's PTY. Matched keybinds
   dispatch to `apply_action_full`. `Event::Resize(w, h)` arms
   `pending_resize`.
2. **Phase 0.5** — coalesce `pending_resize` and run `relayout(w, h)`,
   which re-resolves the KDL layout tree against the new cell-grid area
   and per-pane calls `PaneRunner::resize(pane.rect)`. Propagates new
   dimensions to `GraphicsState::set_cells`.
3. **Phase 1** — drain the close-channel (`PaneRunner::Drop` messages),
   poll exits, snapshot each pane's text grid.
4. **Phase 2** — route kitty graphics events from nested PTY children
   into `GraphicsState` per-pane image maps.
5. **Phase 3a** — render each pane's text grid into a ratatui `Frame`
   at the pane's computed rect (cell-grid `(x, y, w, h)`).
6. **Phase 3b** — emit dashcompositor kitty graphics through the
   passthrough encoder (`encode_passthrough_to_writer`). Sixel is the
   fallback. Degraded text-mode (ratatui only) when neither protocol is
   detected.

### Runtime layout mutations

The following `KeyAction` variants mutate the live layout tree at runtime:

- **`AppNewPane`** — replaces the focused leaf with a
  `Split { Horizontal, 50, [original, new_shell] }`. Survivors keep
  their `PaneLayerId` (label-keyed reconciliation).
- **`PaneClose`** — drops the focused runner (its `Drop` revokes the
  `LayerId` via close-channel), then rebalances the tree via
  `remove_leaf` (sibling absorption collapses a 2-child Split to its
  survivor). Closing the last pane quits.
- **`PanePreset(name)`** — wholesale-swaps the layout tree for a named
  preset body. All old runners are dropped; fresh `LayerId`s are
  allocated from a monotonic counter.
- **`PaneFocus{Next,Prev,Up,Down,Left,Right}`** — focus navigation
  via declaration-order or rect-proximity (`adjacent_pane`).
- **`PaneStack{Cycle,Down,Up,Left,Right}`** — ZStack member focus
  primitives (within-overlay navigation with geometric handoff at
  boundaries).
- **`TabNew` / `TabClose` / `TabSwitch(n)`** — multi-tab operations.
  `TabStack<T>` carries per-tab state; tab mutations sync v1 fields
  and reconcile runners. The tab bar is rendered as both a ratatui
  text fallback (phase 3a) and a dashcompositor pixel overlay (phase 3b).

### Nested-terminal graphics handling

When a child PTY emits kitty graphics commands, `cmdash-pty` intercepts
them via a pre-scan state machine before the `vte` parser (vte silently
drops APC strings). Image uploads are buffered and turned into
`dashcompositor::ImageLayer`s bound to the originating pane's `LayerId`.
Placement commands update the layer's position/size. On pane resize,
placements are reapplied against the new pixel rect.

cmdash does **not** blindly forward graphics escape sequences through its
own stdout — placement is fragile and pane-local. Capture, extract, route.

## Key dependencies (do not reinvent these)

- `dashcompositor` — git dep pinned to `branch = "main"` with
  `default-features = false` and features `kitty-encoder`,
  `sixel-encoder`, `image-decoder`, `font-rasterizer`. Switch to a
  crates.io version pin once upstream publishes.
- `ratatui` — text rendering, widget toolkit, frame.
- `portable-pty` — every child PTY, no roll-our-own.
- `vte` — VT/ANSI parser into a text grid; never hand-roll.
- `kdl` (kdl-rs) — KDL config parser. Chosen over `knus` for full spec
  coverage (property-only nodes, positional args, type annotations).
- `crossterm` — terminal I/O (events, raw mode, alternate screen).
- `image` — PNG decode for kitty graphics payloads.
- `tracing` + `tracing-subscriber` — structured logging.
- `libloading` — hot-load widget `.so` / `.dll` files at runtime.
- `serde` / `serde_json` — internal messages (planned).
- `notify` — filesystem watcher for config hot-reload.
- `anyhow` / `thiserror` at crate seams.

## Feature status

| Feature | Status |
|---------|--------|
| Nested terminals (recursive splits) | ✅ Working |
| Kitty graphics protocol (full support) | ✅ Working (intercept + re-route) |
| Multi-pane reflow on host resize | ✅ Working |
| Runtime layout mutations (new pane, close, preset swap) | ✅ Working |
| Directional focus navigation | ✅ Working |
| ZStack overlay + focus primitives | ✅ Working |
| Tabs (TabStack, tab actions + tab bar rendering) | ✅ Working |
| Configurable layouts (KDL) | ✅ Working |
| Layout presets | ✅ Working |
| Modifier-based keybinds | ✅ Working (all 4 modes routed: Normal, PaneResize, TabSwitch, PresetPick) |
| Status bar | ✅ Working (optional, configurable, hot-reloadable) |
| Mouse support | ✅ Working (click-to-focus, Alt+drag resize, scroll, PTY forwarding) |
| Theme / color customization | ✅ Working (15 color keys, cursor-style, hot-reloadable, wired to script widgets) |
| Native Rust widgets (cmdash-widget-sdk) | ✅ Working (c-ABI trait, libloading, export macro) |
| Script widgets (cmdash-protocol) | ✅ Working (line-delimited frame protocol, spawn/render) |
| Runtime config file loading (~/.config/cmdash/) | ✅ Working (priority chain: `--config`, `$CMDASH_CONFIG_DIR`, XDG default, bundled fallback) |
| Scrollback buffer | ✅ Working (ring buffer, PageUp/PageDown, ESC [3J clear) |
| Sixel fallback | ✅ Code path exists, verified with unit tests |

## Keybinding system

- One global **modifier** (`alt` by default, config-overridable to
  `ctrl`, `super`, or `shift`).
- **Modes:** `Normal`, `PaneResize`, `TabSwitch`, `PresetPick` —
  all four modes are routed. `Normal` handles global bindings;
  `PaneResize` routes arrow keys for split-ratio adjustment;
  `TabSwitch` routes number keys 1–9 for tab switching;
  `PresetPick` routes number keys for preset selection. Escape
  exits any non-Normal mode.
- **Actions:** enum-driven (`KeyAction`), 28 variants covering pane
  management, focus navigation, ZStack cycling, preset swapping, tab
  management, mode entry/exit, and app close.

KDL binding example:

```kdl
keybinds {
    bind "alt-w"  action="pane.close"
    bind "alt-q"  action="app.close"
    bind "ctrl-a" action="app.new-pane"
}
```

## Layout engine (KDL)

Layout node kinds:

- `split { axis "h"|"v", ratio 0.6, a {...}, b {...} }` — binary split
  (exactly 2 children). `axis=horizontal` is a **column** split (left/
  right); `axis=vertical` is a **row** split (top/bottom). The naming is
  a known trapdoor — see `split_rect` rustdoc.
- `stack { pane* }` — equal-height vertical strips (not tabbed viewer).
- `zstack { pane* }` — overlay z-stack; every member shares the parent's
  rect. Distinct `PaneId`s per member. Z-order = resolver pre-order.
- `pane { kind "shell" [label "..."] }` — leaf PTY.
- `preset name "coding" { ...layout body... }` — named saved layout.

Resolved each frame into a tree of `(PaneId, Rect)`. `PaneId` is derived
from pre-order leaf index + child-index path; deterministic so layer IDs
stay stable across resizes of the same tree. Max tree depth: 8.

## Status bar

Optional single-row status bar rendered below the tab bar (or above
panes when `position = "top"`). Configurable via KDL:

```kdl
status_bar {
    enabled     #true
    position    "bottom"
    show-clock  #true
    show-pane-title #true
    show-mode   #true
}
```

When enabled, one extra terminal row is reserved and the layout area
is reduced by that amount. The status bar is hot-reloadable — editing
the config at runtime enables/disables the bar immediately.

## Mouse support

- **Click-to-focus**: clicking a pane focuses it.
- **Alt+drag resize**: Alt+click on a pane and drag to resize its
  parent split's ratio.
- **Scroll-wheel forwarding**: scroll events are forwarded to the
  focused pane's PTY.
- **SGR extended mouse**: mouse events are encoded as SGR sequences
  and forwarded to the focused pane's PTY for TUI apps that support
  mouse input.

## Config model

- **Runtime loading:** config is resolved at startup via a priority chain:
  1. `--config=<path>` (CLI override)
  2. `$CMDASH_CONFIG_DIR/config.kdl` (env override)
  3. `~/.config/cmdash/config.kdl` (XDG default)
  4. bundled `config.kdl` (`include_str!` fallback)

  If a file path is resolved but the file is missing or unreadable,
  cmdash logs a `warn` and falls back to the bundled default.
- **Bundled fallback:** the compile-time `include_str!` from
  `crates/cmdash/config.kdl` serves as the zero-config default when
  no user config file exists.
- **Theme / color customization:** an optional `theme { ... }` top-level
  KDL block lets users customize 15 color keys (terminal fg/bg,
  cursor style, tab bar, status bar, widget borders, error color)
  and a cursor-style enum (`block`, `underline`, `bar`). Colors
  accept named values, hex `#RRGGBB`, `rgb(R,G,B)`, indexed
  `iN`/`indexed(N)`, or `reset`. The theme is hot-reloadable: editing
  the config at runtime applies new colors immediately. Script widget
  borders receive the theme at spawn time via `ScriptWidget::set_theme`.
  See `docs/configuration.md` §3.5 for the full key reference.

## Plugin model — native Rust widgets

`cmdash-widget-sdk` exposes a c-ABI-safe trait:

```rust
pub trait CmdashWidget: Send {
    fn name(&self) -> &str;
    fn render(&mut self, area: Rect, frame: &mut ratatui::Frame);
    fn on_event(&mut self, event: &WidgetEvent) {}
}
```

The `cmdash_widget_export!` macro generates the `cmdash_widget_create`
C-ABI entry point. ABI version is pinned via `CMDASH_WIDGET_ABI_VERSION`.

User flow: `cargo new --lib my-widget` → set `crate-type = ["cdylib"]` →
add `cmdash-widget-sdk` dep → `cmdash_widget_export!(MyWidget)` →
drop `.so` into `~/.config/cmdash/widgets/<name>/` → reference from
layout via `pane kind=widget ref_name="<name>"`.

Two instances of the same widget are two layers, two `PaneId`s, two
loaded copies. Hot-reload is out of scope for v1.

## Script-as-widget protocol

Spawn a child with piped `stdin`/`stdout`. Line-delimited, versioned:

cmdash → script: `FRAME width=80 height=24 gen=42` / `KEY key=h mod=alt` /
`RESIZE width=80 height=24` / `MOUSE x=10 y=5 kind=press btn=left` /
`FOCUS gained|lost`

script → cmdash: `FRAME width=80 height=24` + ANSI text body.

Implemented in `crates/cmdash/src/script_widget.rs`. The `ScriptWidget`
struct wraps a child process, spawns a reader thread, and implements
`CmdashWidget` so it plugs into the same render path as native widgets.

v1 = line+ANSI only. Pixel-bitmap frame mode is a future goal.

## Rendering pipeline details

`GraphicsState` owns the `dashcompositor::LayerStack` and per-pane image
maps (`HashMap<(PaneLayerId, kitty_image_id), ImageEntry>`). Each entry
caches the decoded RGBA so `Place` commands can rebuild the `ImageLayer`
without re-decoding.

`PaneRunner::Drop` sends its `PaneLayerId` into an mpsc close-channel;
the tick loop drains it at the start of each tick and calls
`GraphicsState::close_pane` for each id. This avoids wrapping
`GraphicsState` in `Arc<Mutex<>>` (which fails clippy because
`LayerStack` is not `Sync`).

## Doc-link hygiene

`RUSTDOCFLAGS='-D rustdoc::broken-intra-doc-links' cargo doc -p cmdash --lib --no-deps`
is the project doc-build gate (lib crate only). Use **bare backticks**
for any item that won't resolve in the lib rustdoc surface: bin
entrypoints, `#[cfg(test)]` mod items, private methods on public
structs. `[`crate::main::X`]` always fails; `[`cmdash_layout::split_rect`]`
works cross-crate.

## Development workflow

- Use conventional commit prefixes: `feat:`, `fix:`, `refactor:`, `docs:`,
  `test:`, `style:`, `chore:`.
- Run `cargo fmt --all` before committing.
- Before push: `cargo clippy --workspace --all-targets -- -D warnings`
  and `cargo test --workspace` — both must pass.
- Reuse over reinvention: search awesome-rust / awesome-ratatui first.
  If a crate does the job, pull it in.
- GPG signing on TTY-less hosts: use `scripts/gpg-cmdash-wrapper.sh`
  (committed, no secrets). Run `just gpg-setup` once per host. The
  passphrase lives in `~/.config/cmdash/gpg-passphrase` (chmod 600,
  host-local, gitignored).

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
