# cmdash — Configuration & Usage

This document explains **how to configure** cmdash (the KDL schema + the
bundled `config.kdl`) and **how to use it** day-to-day (running, the
rendering lifecycle, runtime mutations, keybindings). It is the
user-facing companion to the architectural rules in
[`AGENTS.md`](../AGENTS.md) and the high-level overview in
[`README.md`](../README.md).

> **TL;DR.** cmdash v1.0.0 is a single Rust binary. Configuration is
> **embedded at compile time** (`include_str!`); editing it requires
> a recompile. The KDL schema is small — three top-level blocks
> (`layout` / `keybinds` / `presets`) — and the runtime mutation
> toolbox is built around a five-variant layout-tree grammar
> (`split` / `stack` / `zstack` / `pane` / `preset`).

---

## 1. Overview & running cmdash

### 1.1. What cmdash is

cmdash is a single Rust binary (`crates/cmdash`) that emulates a
tree of terminal panes, renders them through `ratatui` for the cell
body, and pushes a per-instance layer out via the Kitty graphics
protocol (or Sixel as fallback) through
[dashcompositor](https://github.com/216598762/dashcompositor). It
is a multiplexer (tmux/zellij-family) glued together with a widget
dashboard by a layer architecture that physically owns every pane on
its own composited layer. See
[`AGENTS.md` §"What cmdash is"](../AGENTS.md) for the canonical
project brief.

### 1.2. Running it

```sh
# from the workspace root, after `cargo build --release`:
cargo run --release -p cmdash

# or, if installed with `--path crates/cmdash`:
cargo install --path crates/cmdash
cmdash
```

When you launch it:

1. The binary emits its bundled `config.kdl` is parsed in-process
   (see §2).
2. `crossterm::terminal::size()` reports the host-window cell-grid
   area. If the host reports zero-area (a transient during window
   snap/minimize/restore) or the call fails, cmdash falls back to
   **80 × 24** and logs a `warn!` line.
3. The layout tree resolves against that area into a flat list of
   leaf panes, each owning its own `dashcompositor::LayerId` per the
   **Hard rule: one layer per instance** (see
   [`AGENTS.md`](../AGENTS.md)).
4. Each pane spawns its own login shell (`PaneKind::Shell` is the v1
   only kind — see §4.4).
5. A **tick loop** runs at 33 ms cadence — roughly 30 fps. Each tick
   drains crossterm input, drains `PaneRunner::Drop`-driven
   close-events, re-layouts on a coalesced host SIGWINCH, draws the
   cell body through `ratatui`, and finally emits dashcompositor
   kitty-graphics overlays through stdout.

### 1.3. Exiting

For most cases, use the bundled default **`Alt-Q`** (binds to
`app.close`), which sets `running = false` so the tick loop returns
and the `TerminalGuard` restores the host terminal (raw-mode +
alternate-screen + mouse-capture). Closing the **last** pane via
`pane.close` (default **`Ctrl-W`**) also quits.

---

## 2. The v1.0.0 config model — embedded at compile time

> **Pitfall #1 — Recompile to edit the config.**
> `cmdash::run` reads its config from a **compile-time** `include_str!`
> on `crates/cmdash/config.kdl` (see `crates/cmdash/src/main.rs`). v1
> does not consult `~/.config/cmdash/config.kdl` or any
> `CMDASH_CONFIG` environment variable. To change your config you
> edit `crates/cmdash/config.kdl` and **rebuild**.

```text
crates/cmdash/config.kdl   ← compile-time-embedded source of truth
             ↓
  cmdash::run reads it via
             ↓
include_str!("../config.kdl")
             ↓
  cmdash_config::parse(&src)   (the KDL walker)
             ↓
  Config { layout, keybinds, presets }
```

This is a deliberately small surface: there is no second source of
truth, no plugin discovery path, and no schema-version negotiation.
If you want a different default config (different splits, different
shell, different keybinds), fork `crates/cmdash/config.kdl`, edit it,
and recompile. The schema and the walker both live in the
[`cmdash-config`](../crates/cmdash-config/src/lib.rs) crate; all
parsing errors return strongly-typed `ConfigError` variants with
human-readable messages.

---

## 3. Top-level schema

A cmdash config has **exactly three** valid top-level KDL nodes —
anything else is an `UnknownTopLevel` parse error:

| Top-level | Required? | Purpose |
|-----------|-----------|---------|
| `layout { ... }` | recommended | The active layout tree the binary resolves each frame. |
| `keybinds { ... }` | optional | A list of `bind "<chord>" action="<action>"` lines that drive `cmdash-keybinds::Router`. |
| `presets { ... }` | optional | A name-keyed map of saved `LayoutNode` bodies callable at runtime via `pane.preset.<name>`. |

A minimal config:

```kdl
layout {
    pane kind=shell label="default"
}
```

…has one shell pane, no keybindings, and no presets. All key presses
that do not match an unbound chord go straight into the focused
pane's PTY as raw bytes (`event_to_bytes` in `main.rs`).

### 3.1. Inside `layout { ... }`

`layout` is a single-node container — it must hold **exactly one**
`LayoutNode` (any of `split` / `stack` / `zstack` / `pane` /
`preset`). A `layout { pane kind=shell ... }` and a
`layout { split axis=horizontal ... }` are both valid; a
`layout { pane kind=shell; pane kind=shell }` (two siblings at the
top level) is **not** valid — you must put them under a `split`,
`stack`, or `zstack` first.

### 3.2. Inside `keybinds { ... }`

Each child must be a `bind` line — anything else is
`UnexpectedKindbindChild`:

```kdl
keybinds {
    bind "ctrl-w" action="pane.close"
    bind "alt-q" action="app.close"
}
```

Chord grammar and action grammar are detailed in §5.

### 3.3. Inside `presets { ... }`

Each child must be a `preset "<name>" { <body> }` block. Duplicate
preset names are rejected (`DuplicatePreset`).

```kdl
presets {
    preset "code" {
        split axis=horizontal ratio=0.5 {
            pane kind=shell label="edit"
            pane kind=shell label="run"
        }
    }
}
```

A `preset "<name>"` reference **inside** `layout { ... }` (rather
than under `presets { ... }`) is a runtime-swap handle: pressing
the matching `pane.preset.<name>` action wholesale-swaps the active
tree for that preset's body (§5.3).

---

## 4. Layout tree semantics & primitives

cmdash-config recognises five `LayoutNode` variants:

### 4.1. `pane` — a leaf PTY

```kdl
pane kind=shell [label="<text>"]
```

`kind=shell` is the **only** valid value in v1. The optional `label`
is a human-readable string surfaced in the layout resolver (and used
by the runtime-mutation reconciliation keying in §6).

### 4.2. `split` — a binary tree split (TWO children exactly)

```kdl
split axis=horizontal|vertical ratio=<float> {
    <child-a>
    <child-b>
}
```

- **`axis`**: either `horizontal` or `vertical` (see trapdoor below).
- **`ratio`**: a `0..=1` float. The walker rounds to nearest percent
  and clamps to `0..=100`, giving `Ratio(pub u8)` internally. Default
  is `0.5`.
- **Children**: **exactly two** in v1. One child or three+ errors out
  as `SplitChildCount { got: N }`.

> **Pitfall #2 — `axis=horizontal` is a column split, not a row split.**
> The token name is opposite of what it sounds like:
>
> - **`split axis=horizontal`** is a **column** split — the split
>   line is horizontal (across the screen), so children stack
>   **side-by-side along x** (left ↔ right). Child 0 occupies the
>   left `ratio%`; child 1 takes the remainder on the right.
> - **`split axis=vertical`** is a **row** split — the split
>   line is vertical (top-to-bottom across the screen), so
>   children stack **top-to-bottom along y** (top ↓ bottom). Child
>   0 occupies the top `ratio%`; child 1 takes the remainder below.
>
> The rustdoc on `cmdash_layout::split_rect` documents this as a
> frequently-stepped trapdoor; treat any tutorial that uses
> `axis=horizontal` to mean "stack top/bottom" as wrong.

### 4.3. `stack` — equal-height vertical strips

```kdl
stack {
    pane kind=shell label="a"
    pane kind=shell label="b"
    pane kind=shell label="c"
}
```

Divides the stack's area into `N` equal-height vertical strips,
top-to-bottom; the last child absorbs any remainder rows. Distinct
`PaneId`s per member (still one `LayerId` per pane instance). An
empty `stack { }` is `EmptyChildren("stack")` at resolution time.

### 4.4. `zstack` — overlay z-stack (same rect, different IDs)

```kdl
zstack {
    pane kind=shell label="bottom"
    pane kind=shell label="middle"
    pane kind=shell label="top"
}
```

Every member **shares the parent's rect verbatim** — i.e. they
*overlay* rather than tile. z-order comes from resolver pre-order
(declaration order); the **last** member draws on top. Each member
still gets its own `PaneId` so the AGENTS.md "one layer per
instance" invariant holds. A 3-member `zstack` therefore produces 3
distinct `dashcompositor::LayerId`s all in the same rect.

A 3-member `zstack` is also the typical handle for the phase-4
`pane.stack.*` focus primitives (§5.2) — within a `zstack`, the
focus can cycle (wrap-around), step up/down (with geometric
handoff at the boundary), and step left/right (with horizontal-axis
geometric handoff).

### 4.5. `preset` — a name reference inside the active layout

```kdl
layout {
    preset "code"
}
```

The root of `layout { ... }` **cannot** be a `preset` reference
(that's `LayoutError::PresetAtRoot`); it is only valid as a nested
child, which is rare in v1 since `pane.preset.<name>` runtime
swaps the *whole* tree atomically.

### 4.6. Tree depth

The layout resolver caps nesting at **`MAX_TREE_DEPTH = 8`** (set in
`crates/cmdash-layout/src/lib.rs`). A 4-deep `split axis=... ratio=...`
under all-leaves is well within budget; a 9-deep pathological config
returns `LayoutError::TreeTooDeep(N)` at startup.

---

## 5. Keybinds & runtime mutations

### 5.1. Chord grammar

A chord is

```
<modifier>-<modifier>-...-<key>
```

where each `<modifier>` is one of (case-sensitive):

| Modifier token | Effect |
|----------------|--------|
| `ctrl`   / `control` / `ctl` | Sets `Modifiers::ctrl` |
| `shift`                     | Sets `Modifiers::shift` |
| `alt`    / `meta`           | Sets `Modifiers::alt` |
| `super`  / `cmd` / `win`    | Sets `Modifiers::super_` |

> **Note — `alt` covers both Alt and Option keys in v1.**
> `cmdash-config`'s `parse_chord` collapses `alt` and `meta` to a
> single `Modifiers::alt` bit (no L/R distinction in v1). The chord
> `alt-q` therefore matches **both** Left-Alt+Q and Right-Alt+Q on
> Linux/Windows keyboards AND Opt+Q (macOS Option+Q) on macOS hosts
> whose Option key is captured by the same modifier bit. v1 cannot
> differentiate Left-Alt from Right-Alt; a v2 hook would consume
> crossterm's `KeyModifiers::LEFT_ALT` / `RIGHT_ALT` flag
> distinction. **Recall Pitfall #1**: because the default config
> (`crates/cmdash/config.kdl`) is `include_str!`-embedded at compile
> time, switching your host's quit keybind to anything other than
> the bundled `alt-q` requires editing that file + rebuilding the
> `cmdash` binary.

Exactly one non-modifier token must close the chord. Valid
**`<key>`** tokens:

- A single character (`a`, `A`, `,`, `/`, `?`, etc.).
- A **named key**: `enter` / `return`, `esc` / `escape`, `tab`,
  `backspace` / `bs`, `up`, `down`, `left`, `right`, `home`, `end`,
  `pageup` / `pgup`, `pagedown` / `pgdn`.
- An **F-key** `f1` … `f24`. **F25 and above are rejected** as
  `InvalidChord`.

> **Pitfall #3 — Press-only key events.** cmdash-keybinds'
> `Router::dispatch_crossterm` only matches **Press** events
> (`crossterm::event::KeyEventKind::Press`). Repeat and Release
> events fall through to the focused pane's PTY. For most workloads
> this is correct (PTYs auto-repeat typed characters internally);
> use a `bind` if you want a Repeat-disabled behaviour.

### 5.2. Action grammar — the 17 wire-form action strings

Every `bind "<chord>" action="<verb>"` line must map to one of the
17 wire-form action strings below. **Counting detail:** the
`KeyAction` enum in `crates/cmdash-config/src/lib.rs` has 15
variants; the table enumerates 17 wire-form strings because
`app.new-pane` / `app.new_pane` are aliases for the SAME variant
and `pane.preset.<name>` accepts any prefix-match against the
`pane.preset.` literal. So the **parser surface** is 17 strings
mapping onto **15 enum variants** — keep this distinction in
mind when reading the `parse_action` match arms. The full
mapping:

| Action string | Behaviour |
|---------------|-----------|
| `app.close` | Quit the binary. |
| `app.new-pane` / `app.new_pane` | Split the focused leaf (Horizontal-50 with a fresh shell pane as child 1). The original leaf's `pre_order` and `LayerId` are preserved (Hard rule). |
| `pane.close` | Drop the focused pane; sibling-absorption collapses a 2-child parent `split`/`stack` upward. Closing the last pane quits the binary. |
| `pane.focus.next` / `pane.focus.prev` | Cycle focus through all runners in declaration order (modulo-wrap). |
| `pane.focus.up` / `pane.focus.down` / `pane.focus.left` / `pane.focus.right` | Geographic focus by rect proximity (`adjacent_pane` algorithm: max perpendicular overlap → min distance → min `pre_order`). No-op when no neighbour exists in that direction. |
| `pane.stack.cycle` | Within the focused member's parent `zstack`, advance to the **next** member with wrap-around. No-op outside a `zstack`. |
| `pane.stack.down` / `pane.stack.up` | Within the focused `zstack` member, advance/retreat in declaration order; **at the boundary** (last for `down`, first for `up`), hand off to the geometrically nearest pane outside the `zstack` via the rect-proximity algorithm. |
| `pane.stack.left` / `pane.stack.right` | Horizontal-axis mirror of `down`/`up`: cycle through `zstack` members in declaration order; at the boundary, hand off geometrically to the sibling cell of the enclosing `split axis=horizontal`. |
| `pane.preset` | Wholesale-swap to the **empty-name** preset — i.e. `KeyAction::PanePreset(String::new())`. In practice this never resolves to a real preset body (preset names are non-empty by construction); in v1 you should always use the `pane.preset.<name>` form below. |
| `pane.preset.<name>` | Wholesale-swap the entire layout tree for the named preset body. Every pane gets a **fresh** `LayerId` (the swap is a different topology, not a rebalance). |

> **Pitfall #4 — Unknown action strings are silently rejected.**
> `cmdash_config::parse` returns `InvalidAction(<string>)` if the
> action does not round-trip through `parse_action` in
> `crates/cmdash-config/src/lib.rs`. v1 does **not** autodiscover
> action names — adding a new verb requires adding it to the
> `KeyAction` enum and the `parse_action` match arms in lock-step.

### 5.3. Runtime mutations — what `pane.preset.<name>` and `app.new-pane` actually do

Three of the actions above mutate the active tree at runtime, not
just the focus:

- **`app.new-pane`** — Replaces the focused leaf with a fresh
  `split axis=horizontal ratio=0.5 [original_clone, new_leaf]`. The
  reconcile engine matches survivors by `pane.label` to preserve
  `PaneLayerId` for the focused pane (per the AGENTS.md Hard rule).
- **`pane.close`** — Drops the focused runner first (its `Drop`
  enqueues the `PaneLayerId` onto the close-channel, so the next
  tick's phase 1 revokes the dashcompositor layer). Then the layout
  tree rebalances with `remove_leaf` — a 2-child `split` collapses
  to its surviving sibling; nested rebalances absorb one level up.
  The action handler applies **label-keyed reconciliation** so the
  survivor keeps its `PaneLayerId`.
- **`pane.preset.<name>`** — Drops every old runner (each `Drop`
  revokes its `LayerId`), swaps `self.layout_root` to the named
  body, and re-spawns fresh runners with **fresh** `PaneLayerId`s
  from a monotonic counter (NOT from `cmdash::derive_layer_id`,
  because both would collide on `LayerId(0)` when the swap's top
  pane happens to land at `pre_order == 0`).

---

## 6. Worked examples

The progression below walks from minimal authoring up to a real
multi-preset layout. Each example past the first is a fork-and-edit
of the prior — replace `crates/cmdash/config.kdl` and rebuild.

> **Reference copies on disk.** Standing copies of these four
> canonical configs (extracted verbatim from this doc) live at the
> project root in [`examples/`](../examples/) as separate `.kdl`
> files: [`01-minimal.kdl`](../examples/01-minimal.kdl)
> (§6.1), [`02-two-pane-split.kdl`](../examples/02-two-pane-split.kdl)
> (§6.2), [`03-stack-and-zstack.kdl`](../examples/03-stack-and-zstack.kdl)
> (§6.3), and [`04-four-pane-tiled.kdl`](../examples/04-four-pane-tiled.kdl)
> (§6.5). To try one: copy it on top of `crates/cmdash/config.kdl`
> and rebuild — see §2 (Pitfall #1). Each header below links to
> the matching on-disk file.

### 6.1. Minimal — single shell pane

This is what ships by default:

```kdl
layout {
    pane kind=shell label="default"
}

keybinds {
    bind "ctrl-w" action="pane.close"
    bind "alt-q" action="app.close"
}
```

Useful for sanity-checking that the binary parsed and rendered one
pane.

### 6.2. Two-pane horizontal split + directional focus

*(Reference copy: [`examples/02-two-pane-split.kdl`](../examples/02-two-pane-split.kdl))*


A 60/40 left/right split with arrows bound to rect-proximity
focus:

```kdl
layout {
    split axis=horizontal ratio=0.6 {
        pane kind=shell label="edit"
        pane kind=shell label="log"
    }
}

keybinds {
    bind "ctrl-w"  action="pane.close"
    bind "alt-q"  action="app.close"
    bind "ctrl-n"  action="app.new-pane"
    bind "ctrl-h"  action="pane.focus.left"
    bind "ctrl-j"  action="pane.focus.down"
    bind "ctrl-k"  action="pane.focus.up"
    bind "ctrl-l"  action="pane.focus.right"
    bind "tab"     action="pane.focus.next"
}
```

`ctrl-h/j/k/l` mnemonically map onto the **vim direction keys**
(`h`=left, `j`=down, `k`=up, `l`=right), even though the runtime
translation goes through rect-proximity arithmetic rather than
literal vim semantics.

### 6.3. Tabbed stack + ZStack overlay

*(Reference copy: [`examples/03-stack-and-zstack.kdl`](../examples/03-stack-and-zstack.kdl))*


A `stack` of three shell panes (= 3 equal-height vertical strips)
plus a separate `zstack { ... }`, with the phase-4 ZStack focus
primitives bound:

```kdl
layout {
    split axis=vertical ratio=0.7 {
        stack {
            pane kind=shell label="tab-a"
            pane kind=shell label="tab-b"
            pane kind=shell label="tab-c"
        }
        zstack {
            pane kind=shell label="overlay-bottom"
            pane kind=shell label="overlay-top"
        }
    }
}

keybinds {
    bind "ctrl-w"                               action="pane.close"
    bind "alt-q"                               action="app.close"
    bind "ctrl-c"                               action="pane.stack.cycle"
    bind "ctrl-alt-down"                        action="pane.stack.down"
    bind "ctrl-alt-up"                          action="pane.stack.up"
    bind "ctrl-alt-left"                        action="pane.stack.left"
    bind "ctrl-alt-right"                       action="pane.stack.right"
    bind "alt-1"                                action="pane.focus.next"
    bind "alt-2"                                action="pane.focus.prev"
}
```

Behaviour sketch:

- Pressing `ctrl-c` while a member of the `zstack` is focused
  cycles focus to the **next** `zstack` member (with wrap-around).
- `ctrl-alt-down` advances through the `zstack` in declaration
  order; pressing it on the **last** member hands focus off to the
  geometrically nearest pane outside the `zstack` (the bottom row
  of the `split` below, from the top half of the layout).
- Cycling between the `stack`'s 3 members uses
  `pane.focus.next` / `pane.focus.prev` (declaration-order
  modulo-wrap).

### 6.4. Presets — define once, swap wholesale at runtime

```kdl
layout {
    split axis=horizontal ratio=0.5 {
        pane kind=shell label="home-a"
        pane kind=shell label="home-b"
    }
}

presets {
    preset "code" {
        split axis=horizontal ratio=0.5 {
            pane kind=shell label="edit"
            pane kind=shell label="run"
        }
    }
    preset "logs" {
        stack {
            pane kind=shell label="tailed"
            pane kind=shell label="metrics"
            pane kind=shell label="audit"
        }
    }
}

keybinds {
    bind "ctrl-w"  action="pane.close"
    bind "alt-q"  action="app.close"
    bind "ctrl-1"  action="pane.preset.code"
    bind "ctrl-2"  action="pane.preset.logs"
    bind "alt-1"   action="pane.preset.code"
}
```

Pressing `ctrl-1` (or `alt-1`) wholesale-swaps the active tree for
the `code` preset's body. Every existing pane is torn down (its
`drop_pane` event revokes the `LayerId` per the AGENTS.md Hard
rule) and the new panes spawn with fresh `LayerId`s.

### 6.5. Advanced — 4-pane Split-of-Split (2×2 grid)

*(Reference copy: [`examples/04-four-pane-tiled.kdl`](../examples/04-four-pane-tiled.kdl))*


A `split axis=vertical { split axis=horizontal …, split axis=horizontal … }`
gives four cells:

```kdl
layout {
    split axis=vertical ratio=0.5 {
        split axis=horizontal ratio=0.5 {
            pane kind=shell label="top-left"
            pane kind=shell label="top-right"
        }
        split axis=horizontal ratio=0.5 {
            pane kind=shell label="bot-left"
            pane kind=shell label="bot-right"
        }
    }
}

keybinds {
    bind "ctrl-w"  action="pane.close"
    bind "alt-q"  action="app.close"
    bind "ctrl-n"  action="app.new-pane"
    bind "ctrl-h"  action="pane.focus.left"
    bind "ctrl-j"  action="pane.focus.down"
    bind "ctrl-k"  action="pane.focus.up"
    bind "ctrl-l"  action="pane.focus.right"
}
```

This exercises the deepest tree the layout engine reasonably needs
to handle in v1 (depth = 2; well below `MAX_TREE_DEPTH = 8`).
`ctrl-h` from `top-left` is a no-op (no neighbour left of the
leftmost column); `ctrl-l` from `bot-right` is a no-op symmetric
case.

### 6.6. Z-stack nested inside a split (Phase 3 derivation)

A `zstack` that overlays **just one cell** of a `split`, not the
whole screen, demonstrates the resolver's "scope-by-parent-area"
invariant. The overlay members share the cell's rect (not the
root's):

```kdl
layout {
    split axis=vertical ratio=0.6 {
        zstack {
            pane kind=shell label="overlay-a"
            pane kind=shell label="overlay-b"
        }
        pane kind=shell label="tail"
    }
}
```

All phase-4 ZStack focus primitives (`pane.stack.cycle`,
`pane.stack.{up,down,left,right}`) work the same way here; they
are scoped to whichever `zstack` the focused member lives in,
regardless of where in the tree that `zstack` sits.

---

## 7. Cross-references

- [`README.md`](../README.md) — top-level overview, architecture,
  installation, workspace table.
- [`AGENTS.md`](../AGENTS.md) — canonical project brief; the
  rules behind the layer-per-instance, intra-doc-link-hygiene,
  and per-frame rendering pipeline.
- [`CHANGELOG.md`](../CHANGELOG.md) — release history; the v1.0.0
  entry records the workspaces, the baked-in config, and the
  initial keybind surface.
- [`docs/1.0-checklist.md`](./1.0-checklist.md) — internal
  release-progress ledger for the v1.0.0 cutoff.
- [`docs/ci-evidence.md`](./ci-evidence.md) — local-CI recipe
  guide (clippy-baseline-0, flake-soak, the `--no-gpgsign=false`
  per-commit + `--no-sign` per-tag signature workaround on a host
  with a TTY-less GPG agent).
- [`examples/`](../examples/) — the four canonical configs from §6
  as standalone `.kdl` files (`01-minimal.kdl`,
  `02-two-pane-split.kdl`, `03-stack-and-zstack.kdl`,
  `04-four-pane-tiled.kdl`). Reference-able from §6.2, §6.3,
  §6.5, and §6.1.
- [`LICENSE`](../LICENSE) — MIT.

### Code items referenced by this doc (and how to refer to them)

This doc references types like
`cmdash_config::Config` / `cmdash_layout::ComputedLayout` /
`cmdash_keybinds::Router` / `cmdash_pty::PaneRunner` directly.
For the **rustdoc** surface (`cargo doc -p cmdash --lib`) prefer
**bare backticks** (`cmdash_config::Config`) because the strict
gate `-D rustdoc::broken-intra-doc-links` (per AGENTS.md) rejects
`[…]`-links to bin entry-points, items inside `#[cfg(test)] mod`s,
or private methods on public structs. AGENTS.md documents the
specific anti-patterns in detail.
