# cmdash — Configuration & Usage

This document explains **how to configure** cmdash (the KDL schema + the
bundled `config.kdl`) and **how to use it** day-to-day (running, the
rendering lifecycle, runtime mutations, keybindings). It is the
user-facing companion to the architectural rules in
[`AGENTS.md`](../AGENTS.md) and the high-level overview in
[`README.md`](../README.md).

> **TL;DR.** cmdash is a single Rust binary. Configuration is
> loaded at runtime from a KDL file, falling back to a bundled
> default. The KDL schema is small — five top-level blocks
> (`layout` / `keybinds` / `presets` / `status_bar` / `theme`) — and the
> runtime mutation toolbox is built around a five-variant layout-tree
> grammar (`split` / `stack` / `zstack` / `pane` / `preset`).

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
   - **Status bar** — `status_bar` config is updated and
     `relayout()` is called so the layout area recalculates
     immediately (chrome height changes take effect).
   - **Layout** — if the layout tree changed, all panes are torn
     down and re-spawned (Wholesale reconcile). If the layout
     is unchanged, only keybinds, presets, and status bar are
     refreshed.

> **Note:** The watcher is **not** started for the bundled fallback
> (priority 4), since there is no file on disk to watch. Also,
> invalid config edits are logged as warnings and ignored — the
> previous valid config remains active until a valid edit arrives.

This means you can edit your config file in one pane and see
keybind/layout changes take effect in cmdash without restarting.

---

## 3. Environment variables

cmdash reads a small set of environment variables at startup and also
advertises a matching set to every child PTY it spawns.

### 3.1. Host-side variables (read by cmdash)

| Variable | Purpose |
|----------|---------|
| `CMDASH_CONFIG_DIR` | Directory to load `config.kdl` from (see §2). Overrides the XDG default. |
| `CMDASH_GRAPHICS` | Force the graphics protocol: `kitty`, `sixel`, `none`, `text`, or `off`. Takes priority over `TERM`/`TERM_PROGRAM` detection. |
| `TERM` | Used to detect the host terminal type and graphics capabilities. |
| `TERM_PROGRAM` | Used together with `TERM` to identify the host terminal emulator. |
| `COLORTERM` | Used to detect true-color/256-color support (`truecolor`, `24bit`, `256color`). |
| `RUST_LOG` | Standard `tracing`/`EnvFilter` log level when `--log` is not used (see §1.5). |

### 3.2. Child-PTY variables (advertised to nested shells)

cmdash derives a capability profile from the host terminal and injects
the following variables into every spawned pane so that nested
applications can discover what the multiplexer supports:

| Variable | Values | Meaning |
|----------|--------|---------|
| `TERM` | `xterm-kitty`, `xterm-256color` | Capability-appropriate terminal type. `xterm-kitty` is used when Kitty graphics are active; otherwise `xterm-256color`. |
| `COLORTERM` | `truecolor`, `256color`, `no` | Color depth advertised by the host. |
| `CMDASH_GRAPHICS` | `kitty`, `sixel`, `none` | Graphics protocol cmdash is using for the host terminal. Applications should consult this (not just `TERM`) when deciding whether to emit Kitty graphics or Sixel. |
| `CMDASH_KITTY_KEYBOARD` | `1` / `0` | Host supports the Kitty keyboard protocol progressive enhancement (`CSI > 1 u`). |
| `CMDASH_FOCUS_EVENTS` | `1` / `0` | Host supports focus-change reporting (`CSI ? 1004 h`). |
| `CMDASH_BRACKETED_PASTE` | `1` / `0` | Host supports bracketed paste (`CSI ? 2004 h`). |
| `CMDASH_QUERIES` | `1` / `0` | Host responds to capability queries such as DA1/DA2. |

These variables are set automatically; you do not need to declare them
in your KDL config. They can be read inside a pane just like any other
environment variable:

```sh
# Inside a cmdash pane
echo "Graphics protocol: $CMDASH_GRAPHICS"
echo "Kitty keyboard:    $CMDASH_KITTY_KEYBOARD"
```

> **Note:** `CMDASH_GRAPHICS` is read by cmdash itself **and**
> advertised to children. Setting it in cmdash's own environment
> forces the host protocol; the same value is then inherited by
> children unless cmdash overrides it with its detected profile.

---

## 4. Top-level schema

A cmdash config has **five** valid top-level KDL nodes:

| Top-level | Required? | Purpose |
|-----------|-----------|---------|
| `layout { ... }` | recommended | The active layout tree. |
| `keybinds { ... }` | optional | `bind "<chord>" action="<action>"` lines. |
| `presets { ... }` | optional | Named layout bodies for `pane.preset.<name>`. |
| `status_bar { ... }` | optional | Enable/configure the status bar. |
| `theme { ... }` | optional | Customize colors and cursor style. |

### 4.1. Inside `layout { ... }`

Must hold **exactly one** `LayoutNode` (`split` / `stack` / `zstack` /
`pane` / `preset`).

### 4.2. Inside `keybinds { ... }`

Each child must be a `bind` line:

```kdl
keybinds {
    bind "alt-w"  action="pane.close"
    bind "alt-q"  action="app.close"
}
```

### 4.3. Inside `presets { ... }`

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

### 4.4. Inside `status_bar { ... }`

Enables an optional single-row status bar rendered below the tab bar
(or above panes when `position = "top"`). When present and
`enabled` is `#true`, one extra terminal row is reserved for the status
bar and the layout area height is reduced accordingly.

```kdl
status_bar {
    enabled     #true       // required to show the bar
    position    "bottom"    // "top" or "bottom" (default: "bottom")
    show-clock  #true       // show HH:MM in the right corner
    show-pane-title #true   // show the focused pane's label
    show-mode   #true       // show the current keybind mode
}
```

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Must be `#true` for the status bar to render. |
| `position` | string | `"bottom"` | `"top"` renders below the tab bar; `"bottom"` renders at the last row. |
| `show-clock` | bool | `true` | Display the current time (HH:MM, UTC). |
| `show-pane-title` | bool | `true` | Display the focused pane's `label` (if set). |
| `show-mode` | bool | `true` | Display the current keybind mode name. |

> **Note:** Boolean values in KDL use `#true`/`#false` syntax (KDL v2).
> Bare `true`/`false` are not valid.

When `status_bar` is omitted entirely, no status bar is rendered and
the layout uses the full terminal height (minus the tab bar).

The status bar is **hot-reloadable**: editing the `status_bar` block
in your config file at runtime will enable/disable the bar and
recalculate the layout area immediately (no restart required).

### 4.5. Inside `theme { ... }`

Customizes the color scheme for the tab bar, status bar, widget
borders, and error messages. All fields are optional — when omitted,
the built-in default color is used. This allows partial themes: only
specify the colors you want to override.

```kdl
theme {
    // Terminal defaults
    default-fg       "white"       // foreground for terminal body
    default-bg       "black"       // background for terminal body
    cursor-style     "block"       // "block" | "underline" | "bar"

    // Tab bar
    tab-bar-bg       "dark-gray"   // background of the tab bar strip
    tab-active-bg    "blue"        // background of the active tab
    tab-active-fg    "white"       // text color of the active tab
    tab-inactive-bg  "dark-gray"   // background of inactive tabs
    tab-inactive-fg  "gray"        // text color of inactive tabs

    // Status bar (requires status_bar.enabled = #true)
    status-bar-bg    "dark-gray"   // background of the status bar
    status-mode-fg   "white"       // foreground of the mode indicator
    status-mode-bg   "dark-gray"   // background of the mode indicator
    status-clock-fg  "gray"        // foreground of the clock
    status-pane-title-fg "gray"    // foreground of the pane title

    // Widget / border colors
    border-color     "dark-gray"   // default border for widgets
    error-color      "red"         // color for error messages
}
```

#### Recognized keys

| Key | Category | Default | Description |
|-----|----------|---------|-------------|
| `default-fg` | terminal | `white` | Default foreground color for the terminal body. |
| `default-bg` | terminal | `black` | Default background color for the terminal body. |
| `cursor-style` | terminal | `block` | Cursor shape: `block`, `underline`, or `bar`. |
| `tab-bar-bg` | tab bar | `dark-gray` | Background of the tab bar strip. |
| `tab-active-bg` | tab bar | `blue` | Background of the active tab. |
| `tab-active-fg` | tab bar | `white` | Text color of the active tab. |
| `tab-inactive-bg` | tab bar | `dark-gray` | Background of inactive tabs. |
| `tab-inactive-fg` | tab bar | `gray` | Text color of inactive tabs. |
| `status-bar-bg` | status bar | `dark-gray` | Background of the status bar row. |
| `status-mode-fg` | status bar | `white` | Foreground of the mode indicator text. |
| `status-mode-bg` | status bar | `dark-gray` | Background of the mode indicator. |
| `status-clock-fg` | status bar | `gray` | Foreground of the clock display. |
| `status-pane-title-fg` | status bar | `gray` | Foreground of the pane title display. |
| `border-color` | widget | `dark-gray` | Default border color for widgets and bordered blocks. |
| `error-color` | widget | `red` | Color for error messages and error borders. |

#### Valid color formats

Color values accept any of the following formats (case-insensitive):

| Format | Example | Description |
|--------|---------|-------------|
| **Named** | `"red"`, `"dark-gray"`, `"white"` | 12 standard names (see below). |
| **Hex RGB** | `"#FF8040"`, `"#f0f"` | 6-digit or 3-digit hex (`#RGB` expands each nibble × 17). |
| **rgb()** | `"rgb(255, 128, 64)"` | Comma-separated R, G, B (0–255). |
| **Indexed** | `"i5"`, `"indexed(5)"` | ANSI-256 palette index (0–255). |
| **reset** | `"reset"` | Passthrough to terminal defaults (background becomes black in RGBA helpers). |

**Named colors:**
`black`, `dark-gray` (or `darkgray`/`dark_gray`), `gray` (or `grey`),
`white`, `red`, `green`, `blue`, `yellow`, `cyan`, `magenta`, `reset`.

**`cursor-style` values:**
`block` (solid block), `underline` (or `under`/`u`), `bar` (or `pipe`/`|`).

> **Note:** The `theme` block is **hot-reloadable**. Editing it in your
> config file at runtime applies the new colors immediately — no restart
> required. If a color value is invalid, cmdash logs a warning and the field
> falls back to its built-in default.

---

## 5. Layout tree semantics & primitives

### 5.1. `pane` — a leaf PTY

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

### 5.2. `split` — a binary tree split (TWO children exactly)

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

### 5.3. `stack` — equal-height vertical strips

```kdl
stack {
    pane kind=shell label="a"
    pane kind=shell label="b"
}
```

Divides the area into `N` equal-height vertical strips, top-to-bottom;
the last child absorbs any remainder rows.

### 5.4. `zstack` — overlay z-stack (same rect, different IDs)

```kdl
zstack {
    pane kind=shell label="bottom"
    pane kind=shell label="top"
}
```

Every member **shares the parent's rect verbatim** — they overlay
rather than tile. Z-order = declaration order (last member on top).
Each member still gets its own `PaneId`.

### 5.5. `preset` — a name reference inside the active layout

The root of `layout { ... }` **cannot** be a `preset` reference
(`LayoutError::PresetAtRoot`).

### 5.6. Tree depth

Max nesting: **8** (`MAX_TREE_DEPTH`). Deeper trees return
`LayoutError::TreeTooDeep(N)`.

---

## 6. Keybinds & runtime mutations

### 6.1. Chord grammar

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

### 6.2. Action grammar

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
| `pane.resize.enter` | Enter PaneResize mode. See §6.4. |
| `tab.switch.enter` | Enter TabSwitch mode. See §6.4. |
| `preset.pick.enter` | Enter PresetPick mode. See §6.4. |
| `mode.exit` | Return to Normal mode from any non-Normal mode. |
| `pane.resize.up` / `.down` / `.left` / `.right` | Resize focused pane's split ±2% (PaneResize mode only). See §6.4. |

> **Pitfall #4 — Unknown action strings are rejected** as
> `InvalidAction(<string>)` at config parse time.

### 6.3. Runtime mutations

- **`app.new-pane`** — replaces focused leaf with
  `split axis=horizontal ratio=0.5 [original, new_shell]`.
- **`pane.close`** — drops focused runner, rebalances tree
  (`remove_leaf`), survivor keeps its `LayerId`.
- **`pane.preset.<name>`** — drops all runners, swaps layout tree,
  spawns fresh runners with fresh `LayerId`s.

### 6.4. Keybind modes

cmdash has a mode-based keybind router with four modes:

| Mode | Description |
|------|-------------|
| **Normal** | Default mode. All `keybinds { ... }` bindings are active. |
| **PaneResize** | Arrow keys resize the focused pane's parent split (±2% per press). |
| **TabSwitch** | Number keys 1–9 switch tabs. |
| **PresetPick** | Number keys select layout presets. |
| **Copy** | Arrow keys move the copy-mode cursor; `v` starts/extends the selection; `y` or Enter copies to the system clipboard. |

**Entering a mode:** Press the configured keybind
(e.g. `M-r` for `pane.resize.enter`, `M-p` for `preset.pick.enter`).

**Exiting a mode:** Press **Escape** (hardcoded in the router — works
in all non-Normal modes).

**Displaying the current mode:** When `status_bar` is enabled with
`show-mode` is `#true`, the status bar shows the active mode name on the
left side. In Normal mode it displays `Normal`; when you enter
PaneResize it switches to `PaneResize`, and so on. This gives
immediate visual feedback that a mode is active.

**Mode-specific actions (active only while the mode is active):**

| Action string | Mode | Behaviour |
|---------------|------|-----------|
| `pane.resize.enter` | Normal → PaneResize | Enter PaneResize mode. |
| `pane.resize.up` / `.down` / `.left` / `.right` | PaneResize | Resize the focused pane's parent split ±2%. |
| `tab.switch.enter` | Normal → TabSwitch | Enter TabSwitch mode. |
| `preset.pick.enter` | Normal → PresetPick | Enter PresetPick mode. |
| `copy.enter` | Normal → Copy | Enter Copy mode. |
| `copy.move.up` / `.down` / `.left` / `.right` | Copy | Move the copy-mode cursor. |
| `copy.select` | Copy | Start/extend the text selection. |
| `copy.copy` | Copy | Copy the selected text to the system clipboard and exit Copy mode. |
| `mode.exit` | any non-Normal | Return to Normal mode (also triggered by Escape). |

**Default mode-entry keybinds (in the bundled `config.kdl`):**

```kdl
keybinds {
    bind "M-r"  action="pane.resize.enter"   // Alt-R → PaneResize
    bind "M-p"  action="preset.pick.enter"   // Alt-P → PresetPick
}
```

**Unmatched keys in non-Normal modes** fall through to the focused
pane's PTY, so you can still type normally while in PaneResize or
PresetPick mode — only the explicitly bound keys are intercepted.

### 6.5. Bracketed paste support

cmdash supports the terminal bracketed-paste protocol so child
applications can distinguish pasted text from typed keystrokes. A pane
requests the mode by emitting the standard private-mode sequences:

```text
# Enable bracketed paste
ESC [ ? 2 0 4 h

# Disable bracketed paste
ESC [ ? 2 0 4 l
```

When a pane has requested bracketed paste, cmdash wraps pasted content
in the standard delimiters before forwarding it to that pane:

```text
ESC [ 2 0 0 ~ <pasted text> ESC [ 2 0 1 ~
```

Pasted text is forwarded raw for panes that have not requested the mode.

**Host synchronization.** cmdash tracks bracketed-paste state per pane
and enables it on the host terminal whenever **any** live pane has it
requested. This means:

- Focus changes do not disable bracketed paste while another pane still
  wants it.
- The host terminal is disabled only when no pane still has the mode
  enabled.

**Example — a shell that enables bracketed paste on startup:**

```kdl
layout {
    // \\033 is the octal escape for the ESC byte, so the shell emits
    // the literal sequence ESC[?2004h before replacing itself with bash.
    pane kind=shell label="editor" command="sh -c 'printf \"\\033[?2004h\"; exec bash'"
}

keybinds {
    bind "alt-w"  action="pane.close"
    bind "alt-q"  action="app.close"
}
```

In this example the shell prints `ESC[?2004h` on startup. cmdash
intercepts the sequence, records that the pane wants bracketed paste,
enables it on the host terminal, and from then on wraps any pasted
text in `ESC[200~` / `ESC[201~` for that pane.

### 6.6. Focus reporting support

cmdash supports the standard terminal focus-reporting protocol so child
applications can react to the host terminal gaining or losing focus. A pane
requests the mode by emitting the standard private-mode sequences:

```text
# Enable focus reporting
ESC [ ? 1 0 0 4 h

# Disable focus reporting
ESC [ ? 1 0 0 4 l
```

When a pane has requested focus reporting and the host terminal reports a focus
change, cmdash forwards the appropriate sequence to the focused pane:

```text
# Host gained focus
ESC [ I

# Host lost focus
ESC [ O
```

**Host synchronization.** cmdash tracks focus-reporting state per pane and
enables focus-change events on the host terminal whenever **any** live pane has
it requested. This means:

- Focus changes are forwarded to the currently focused pane only when that pane
  has requested focus reporting.
- Focus changes are not forwarded to panes that have not requested the mode.
- The host terminal continues to emit focus-change events while any pane still
  wants them.

**Example — a shell that enables focus reporting on startup:**

```kdl
layout {
    // \\033 is the octal escape for the ESC byte, so the shell emits
    // the literal sequence ESC[?1004h before replacing itself with bash.
    pane kind=shell label="editor" command="sh -c 'printf \"\\033[?1004h\"; exec bash'"
}

keybinds {
    bind "alt-w"  action="pane.close"
    bind "alt-q"  action="app.close"
}
```

In this example the shell prints `ESC[?1004h` on startup. cmdash intercepts the
sequence, records that the pane wants focus reporting, enables focus-change
events on the host terminal, and from then on forwards `ESC[I` / `ESC[O` to the
focused pane whenever the host terminal gains or loses focus.

---

## 7. Worked examples

Standing copies live in [`examples/`](../examples/). To try one: copy
it on top of `crates/cmdash/config.kdl` and rebuild.

### 7.1. Minimal — single shell pane

```kdl
layout {
    pane kind=shell label="default"
}

keybinds {
    bind "alt-w"  action="pane.close"
    bind "alt-q"  action="app.close"
}
```

### 7.2. Two-pane horizontal split + directional focus

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

### 7.3. Tabbed stack + ZStack overlay

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

### 7.4. Presets — define once, swap wholesale at runtime

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

### 7.5. Per-pane commands — override the default shell

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

### 7.6. Advanced — 4-pane 2×2 grid

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

## 8. Cross-references

- [`README.md`](../README.md) — top-level overview, installation.
- [`AGENTS.md`](../AGENTS.md) — architecture rules, non-goals,
  render loop, plugin model.
- [`docs/roadmap.md`](./roadmap.md) — development roadmap.
- [`CHANGELOG.md`](../CHANGELOG.md) — release history.
- [`examples/`](../examples/) — standalone `.kdl` config files.
- [`LICENSE`](../LICENSE) — MIT.
