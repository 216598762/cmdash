# cmdash — Configuration & Usage

This document explains **how to configure** cmdash (the KDL schema + the
bundled `config.kdl`) and **how to use it** day-to-day (running, the
rendering lifecycle, runtime mutations, keybindings). It is the
user-facing companion to the architectural rules in
[`AGENTS.md`](../AGENTS.md) and the high-level overview in
[`README.md`](../README.md).

> **TL;DR.** cmdash is a single Rust binary. Configuration is
> loaded at runtime from a KDL file, falling back to a bundled
> default. The KDL schema is small — three top-level blocks
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
[dashcompositor](https://github.com/216598762/dashcompositor). See
[`AGENTS.md`](../AGENTS.md) for the canonical project brief.

### 1.2. Running it

```sh
cargo run --release -p cmdash
# or, if installed:
cmdash
```

When you launch it:

1. Config is loaded from the first available source (see §2).
2. `crossterm::terminal::size()` reports the host-window cell-grid
   area. If the host reports zero-area or the call fails, cmdash
   falls back to **80 × 24** and logs a `warn!` line.
3. The layout tree resolves against that area into a flat list of
   leaf panes, each owning its own `dashcompositor::LayerId`.
4. Each pane spawns its own login shell.
5. A **tick loop** runs at 33 ms cadence (~30 fps).

### 1.3. CLI flags

```
cmdash [OPTIONS]

OPTIONS:
  --config=<path>   Path to a KDL config file (default: ~/.config/cmdash/config.kdl)
  --log=<path>      Write trace-level diagnostics to <path> (stdout is silent)
  --help, -h        Print help message
```

### 1.4. Exiting

Use **`Alt-Q`** (`app.close`), or close the last pane via **`Alt-W`**
(`pane.close`).

---

### 1.5. Logging

cmdash uses `tracing` + `tracing-subscriber`. Two orthogonal knobs:

- `--log=<path>` (CLI) — redirects output to a file at `<path>`.
  TRACE level is forced; `RUST_LOG` is ignored. Append mode; parent
  directory must exist. Missing/unreadable path = startup error
  (exit 3).
- `RUST_LOG` (env var) — standard `EnvFilter` format. Honored only
  when `--log=<path>` is NOT passed (stdout mode). Default: `info`.

```bash
# Full trace to file (stdout silent, stderr banner on launch).
cmdash --log=/tmp/cmdash-debug.log

# Quiet stdout.
RUST_LOG=warn cmdash

# Crate-targeted filter (stdout mode only).
RUST_LOG=cmdash_layout=debug,info cmdash
```

Parser error classes:
- Bare `--log` (no `=<path>`) → exit 2.
- `--log=` (empty value) → exit 2.
- Unknown `--flag` → warn to stderr, parse continues (forward-compat).

---

## 2. Config file loading — runtime resolution

> **Pitfall #1 (resolved) — Config is now loaded at runtime.**
> cmdash resolves its config file using the following priority chain:
>
> 1. `--config=<path>` (explicit CLI override)
> 2. `$CMDASH_CONFIG_DIR/config.kdl` (environment variable override)
> 3. `~/.config/cmdash/config.kdl` (XDG default)
> 4. Bundled `config.kdl` (compiled-in fallback)
>
> If a file path is resolved but the file is missing or unreadable,
> cmdash logs a `warn` and falls back to the bundled default.
> To customize, create `~/.config/cmdash/config.kdl` and restart.

### 2.1. Config hot-reload

When cmdash resolves a config file path (priorities 1–3 above), it
spawns a filesystem watcher on the file's parent directory using the
[`notify`](https://crates.io/crates/notify) crate. When the config
file is modified on disk:

1. The watcher detects the change (500 ms debounce to coalesce
   rapid edits).
2. The file is re-parsed into a fresh `Config` payload.
3. The new config is sent to the tick loop via an mpsc channel.
4. On the next tick (Phase 0.6), the tick loop applies the changes:
   - **Keybinds** swap immediately via a fresh `Router`.
   - **Presets** replace the stored preset map.
   - **Layout** — if the layout tree changed, all panes are torn
     down and re-spawned (Wholesale reconcile). If the layout
     is unchanged, only keybinds and presets are refreshed.

> **Note:** The watcher is **not** started for the bundled fallback
> (priority 4), since there is no file on disk to watch. Also,
> invalid config edits are logged as warnings and ignored — the
> previous valid config remains active until a valid edit arrives.

This means you can edit your config file in one pane and see
keybind/layout changes take effect in cmdash without restarting.

---

## 3. Top-level schema

A cmdash config has **exactly three** valid top-level KDL nodes:

| Top-level | Required? | Purpose |
|-----------|-----------|---------|
| `layout { ... }` | recommended | The active layout tree. |
| `keybinds { ... }` | optional | `bind "<chord>" action="<action>"` lines. |
| `presets { ... }` | optional | Named layout bodies for `pane.preset.<name>`. |

### 3.1. Inside `layout { ... }`

Must hold **exactly one** `LayoutNode` (`split` / `stack` / `zstack` /
`pane` / `preset`).

### 3.2. Inside `keybinds { ... }`

Each child must be a `bind` line:

```kdl
keybinds {
    bind "alt-w"  action="pane.close"
    bind "alt-q"  action="app.close"
}
```

### 3.3. Inside `presets { ... }`

Each child must be a `preset "<name>" { <body> }` block. Duplicate
names are rejected.

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

---

## 4. Layout tree semantics & primitives

### 4.1. `pane` — a leaf PTY

```kdl
pane kind=shell [label="<text>"] [command="<cmd>"]
```

`kind=shell` is the **only** valid value in v1.

**Optional fields:**

| Field | Default | Description |
|-------|---------|-------------|
| `label` | `None` | Display label shown in the tab bar and used for survivor matching across runtime mutations. |
| `command` | `None` | Per-pane shell command override. When set, the pane spawns this command instead of the default login shell (`$SHELL` / `/bin/sh`). |

**`command` field details:**

- The command string is **split by whitespace** into argv at spawn time. `command="htop --delay=5 --color"` produces `["htop", "--delay=5", "--color"]`.
- The command is executed directly (no shell) — pipes (`|`), redirects (`>`), variable expansion (`$VAR`), and other shell features are **not** available. For complex commands, wrap in a shell: `command="sh -c 'cargo build && echo DONE'"`.
- If the command binary is not found on `$PATH`, the PTY child fails to spawn and cmdash logs a warning. The pane will appear blank.
- An empty `command=""` falls back to the default login shell.
- Each pane in a layout can have its own independent `command`.

### 4.2. `split` — a binary tree split (TWO children exactly)

```kdl
split axis=horizontal|vertical ratio=<float> {
    <child-a>
    <child-b>
}
```

> **Pitfall #2 — `axis=horizontal` is a column split, not a row split.**
>
> - **`split axis=horizontal`** = **column** split — children stack
>   **side-by-side along x** (left ↔ right). Child 0 = left
>   `ratio%`; child 1 = right remainder.
> - **`split axis=vertical`** = **row** split — children stack
>   **top-to-bottom along y** (top ↓ bottom). Child 0 = top
>   `ratio%`; child 1 = bottom remainder.

### 4.3. `stack` — equal-height vertical strips

```kdl
stack {
    pane kind=shell label="a"
    pane kind=shell label="b"
}
```

Divides the area into `N` equal-height vertical strips, top-to-bottom;
the last child absorbs any remainder rows.

### 4.4. `zstack` — overlay z-stack (same rect, different IDs)

```kdl
zstack {
    pane kind=shell label="bottom"
    pane kind=shell label="top"
}
```

Every member **shares the parent's rect verbatim** — they overlay
rather than tile. Z-order = declaration order (last member on top).
Each member still gets its own `PaneId`.

### 4.5. `preset` — a name reference inside the active layout

The root of `layout { ... }` **cannot** be a `preset` reference
(`LayoutError::PresetAtRoot`).

### 4.6. Tree depth

Max nesting: **8** (`MAX_TREE_DEPTH`). Deeper trees return
`LayoutError::TreeTooDeep(N)`.

---

## 5. Keybinds & runtime mutations

### 5.1. Chord grammar

```
<modifier>-<modifier>-...-<key>
```

| Modifier token | Effect |
|----------------|--------|
| `ctrl` / `control` / `ctl` | Sets `Modifiers::ctrl` |
| `shift` | Sets `Modifiers::shift` |
| `alt` / `meta` / `m` / `M` | Sets `Modifiers::alt` |
| `super` / `cmd` / `win` | Sets `Modifiers::super_` |

Valid **`<key>`** tokens: single character, named key (`enter`,
`esc`, `tab`, `backspace`, `up`, `down`, `left`, `right`, `home`,
`end`, `pageup`, `pagedown`), or F-key (`f1`…`f24`).

> **Pitfall #3 — Press-only key events.** The router only matches
> **Press** events. Repeat and Release fall through to the PTY.

### 5.2. Action grammar

| Action string | Behaviour |
|---------------|-----------|
| `app.close` | Quit the binary. |
| `app.new-pane` / `app.new_pane` | Split the focused leaf (Horizontal-50). Original leaf's `LayerId` preserved. |
| `pane.close` | Drop the focused pane; sibling-absorption collapses the parent. Closing last pane quits. |
| `pane.focus.next` / `pane.focus.prev` | Cycle focus in declaration order. |
| `pane.focus.up` / `.down` / `.left` / `.right` | Geographic focus by rect proximity. |
| `pane.stack.cycle` | Cycle through focused `zstack` members (wrap-around). |
| `pane.stack.down` / `.up` / `.left` / `.right` | Directional within-`zstack` with geometric handoff at boundary. |
| `pane.preset.<name>` | Wholesale-swap layout tree for named preset. Fresh `LayerId`s. |
| `tab.new` | Create a new empty tab and switch to it. |
| `tab.close` | Close the active tab. Closing last tab quits. |
| `tab.switch.<n>` (n=1..=9) | Switch to the nth tab. |

> **Pitfall #4 — Unknown action strings are rejected** as
> `InvalidAction(<string>)` at config parse time.

### 5.3. Runtime mutations

- **`app.new-pane`** — replaces focused leaf with
  `split axis=horizontal ratio=0.5 [original, new_shell]`.
- **`pane.close`** — drops focused runner, rebalances tree
  (`remove_leaf`), survivor keeps its `LayerId`.
- **`pane.preset.<name>`** — drops all runners, swaps layout tree,
  spawns fresh runners with fresh `LayerId`s.

---

## 6. Worked examples

Standing copies live in [`examples/`](../examples/). To try one: copy
it on top of `crates/cmdash/config.kdl` and rebuild.

### 6.1. Minimal — single shell pane

```kdl
layout {
    pane kind=shell label="default"
}

keybinds {
    bind "alt-w"  action="pane.close"
    bind "alt-q"  action="app.close"
}
```

### 6.2. Two-pane horizontal split + directional focus

*(See [`examples/02-two-pane-split.kdl`](../examples/02-two-pane-split.kdl))*

```kdl
layout {
    split axis=horizontal ratio=0.6 {
        pane kind=shell label="edit"
        pane kind=shell label="log"
    }
}

keybinds {
    bind "alt-w"   action="pane.close"
    bind "alt-q"   action="app.close"
    bind "ctrl-n"  action="app.new-pane"
    bind "ctrl-h"  action="pane.focus.left"
    bind "ctrl-j"  action="pane.focus.down"
    bind "ctrl-k"  action="pane.focus.up"
    bind "ctrl-l"  action="pane.focus.right"
    bind "tab"     action="pane.focus.next"
}
```

### 6.3. Tabbed stack + ZStack overlay

*(See [`examples/03-stack-and-zstack.kdl`](../examples/03-stack-and-zstack.kdl))*

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
    bind "alt-w"            action="pane.close"
    bind "alt-q"            action="app.close"
    bind "ctrl-c"           action="pane.stack.cycle"
    bind "ctrl-alt-down"    action="pane.stack.down"
    bind "ctrl-alt-up"      action="pane.stack.up"
    bind "ctrl-alt-left"    action="pane.stack.left"
    bind "ctrl-alt-right"   action="pane.stack.right"
}
```

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
}

keybinds {
    bind "alt-w"   action="pane.close"
    bind "alt-q"   action="app.close"
    bind "ctrl-1"  action="pane.preset.code"
}
```

### 6.5. Per-pane commands — override the default shell

*(See [`examples/05-per-pane-commands.kdl`](../examples/05-per-pane-commands.kdl))*

Each pane can specify a `command` to run instead of the default
login shell. The command string is split by whitespace into argv;
shell metacharacters are not supported.

```kdl
layout {
    split axis=horizontal ratio=0.6 {
        pane kind=shell label="editor" command="nvim"
        pane kind=shell label="monitor" command="htop --delay=5"
    }
}

keybinds {
    bind "alt-w"   action="pane.close"
    bind "alt-q"   action="app.close"
}
```

This opens `nvim` in the left pane and `htop --delay=5` in the
right pane, instead of spawning login shells.

**Shell wrapper for complex commands:**

```kdl
pane kind=shell label="build" command="sh -c 'cargo build && echo DONE'"
```

### 6.6. Advanced — 4-pane 2×2 grid

*(See [`examples/04-four-pane-tiled.kdl`](../examples/04-four-pane-tiled.kdl))*

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
```

---

## 7. Cross-references

- [`README.md`](../README.md) — top-level overview, installation.
- [`AGENTS.md`](../AGENTS.md) — architecture rules, non-goals,
  render loop, plugin model.
- [`docs/roadmap.md`](./roadmap.md) — development roadmap.
- [`CHANGELOG.md`](../CHANGELOG.md) — release history.
- [`examples/`](../examples/) — standalone `.kdl` config files.
- [`LICENSE`](../LICENSE) — MIT.
