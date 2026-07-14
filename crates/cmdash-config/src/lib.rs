//! cmdash-config: KDL parsing for cmdash user configuration.
//!
//! ## Parser choice
//!
//! We use [`kdl`](https://crates.io/crates/kdl) (kdl-rs, the canonical
//! Rust KDL parser) over `knus` and `facet-kdl`.
//!
//! ## Justification
//!
//! - **Full spec coverage.** kdl-rs parses both KDL v1 and v2 (the
//!   production spec), including nested property-only nodes, positional
//!   arguments, type annotations, and `{ ... }` blocks. `knus 3.4.0`'s
//!   grammar has gaps - property-only nodes (e.g. `pane kind=shell`)
//!   are rejected regardless of `;` or `{}`, empirically confirmed.
//!   This gap makes `knus` unusable for cmdash-config's `LayoutNode`
//!   shape, which is precisely the shape cmdash needs.
//! - **Maturity.** kdl-rs has been the canonical Rust KDL parser for
//!   several years. `knus` is younger; `facet-kdl` is pre-1.0 and part
//!   of a wider reflection family.
//! - **Stable-Rust only.** kdl-rs works on stable. No nightly.
//! - **Community docs / examples.** kdl-rs has exposition-grade README
//!   and example corpus.
//! - **Explicit walker code.** We do NOT use `kdl_derive`; the walker
//!   is plain Rust so the same `Config` struct stays consumable by
//!   other workspace crates without forcing them onto a derive macro.
//!
//! Pin: `kdl = "6.3.4"` exact. Bumped intentionally after a green
//! regression test in this crate.
//!
//! ## Schema
//!
//! A cmdash configuration looks like the file in
//! `tests/fixtures/config.kdl`. The schema is:
//!
//! ```text
//! layout {
//!   split axis=horizontal|vertical ratio=<n> { ... }
//!   stack { pane* }
//!   pane kind=shell [label="..."] [command="..."]
//!   pane kind=widget ref-name="<name>" [label="..."]
//!   pane kind=script command="<cmd>" [label="..."]
//!   preset "name" | preset name="..."
//! }
//! keybinds {
//!   bind "<chord>" action="<action>"
//! }
//! ```

use std::collections::BTreeMap;

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};

pub mod theme;
pub use theme::{parse_color, Theme};

use thiserror::Error;

/// Top-level cmdash configuration document.
///
/// `layout` is the active layout tree (resolved each frame into a
/// [`cmdash_layout::ComputedLayout`]). `presets` is a name-keyed
/// map of saved layout bodies that the `KeyAction::PanePreset(name)`
/// runtime mutation swaps the active `layout` against; both fields
/// are owned so the binary passes them by value into
/// [`TickContext`](https://docs.rs/cmdash) and never has to
/// re-parse on a preset swap.
///
/// The pair of fields is intentionally flat — `presets` is NOT
/// nested under `layout` — so a runtime swap doesn't have to
/// walk into a possibly-mutated tree to look up a named body.
/// Presets are populated from a top-level
/// `presets { preset "name" { body } }` block in the KDL source;
/// see [`parse`] for the walker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub layout: Option<LayoutNode>,
    pub keybinds: Vec<Keybind>,
    /// Saved layout bodies keyed by their `name`. Populated by
    /// [`parse`] from a top-level `presets { … }` block; an empty
    /// map means no presets are defined.
    pub presets: BTreeMap<String, LayoutNode>,
    /// Optional status bar configuration. When `None`, no status
    /// bar is rendered (the default). When `Some(Bar)`, a single
    /// row is reserved at the configured position and the bar is
    /// rendered in phase 3a after pane blits.
    pub status_bar: Option<Bar>,
    /// Optional theme configuration. When `None`, the hardcoded
    /// default colors are used.
    pub theme: Option<Theme>,
}

/// Configuration for the optional status bar.
///
/// Parsed from a top-level `status_bar { ... }` block in the
/// KDL config. When present and `enabled = true`, a single row
/// is reserved at the configured position (top or bottom) and
/// the status bar is rendered in phase 3a.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bar {
    /// Whether the status bar is enabled. Defaults to `false`
    /// when the block is present but `enabled` is omitted.
    pub enabled: bool,
    /// Position of the status bar. `Top` renders above panes
    /// (below the tab bar); `Bottom` renders below panes.
    pub position: BarPosition,
    /// Show the current time (HH:MM format).
    pub show_clock: bool,
    /// Show the focused pane's label (if set).
    pub show_pane_title: bool,
    /// Show the active keybind mode (`Normal`, `PaneResize`, etc.).
    pub show_mode: bool,
}

impl Default for Bar {
    fn default() -> Self {
        Self {
            enabled: false,
            position: BarPosition::Bottom,
            show_clock: true,
            show_pane_title: true,
            show_mode: true,
        }
    }
}

/// Position of the status bar on screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BarPosition {
    /// Render the status bar at the top of the screen, below
    /// the tab bar.
    Top,
    /// Render the status bar at the bottom of the screen.
    #[default]
    Bottom,
}

/// A node in the layout tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutNode {
    /// `split axis=horizontal|vertical ratio=<n> { ... }`
    Split {
        axis: SplitAxis,
        ratio: Ratio,
        children: Vec<LayoutNode>,
    },
    /// `stack { pane* }` — equal-height vertical strips (tabbed
    /// UI); each member owns its row.
    Stack { panes: Vec<LayoutNode> },
    /// `zstack { pane* }` — Kinetic-style overlay; every
    /// member shares the parent's rect verbatim. Distinct
    /// `PaneId`s per member (Hard rule: one `LayerId` per
    /// pane instance; z-order = resolver pre-order).
    ZStack { panes: Vec<LayoutNode> },
    /// `pane kind=shell [label="..."]`
    Pane(Pane),
    /// `preset "name"` or `preset name="..."`
    Preset { name: String },
}

/// Axis along which a `split` divides its children.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SplitAxis {
    #[default]
    Horizontal,
    Vertical,
}

/// Integer percentage (0..=100) for a split. Floats in KDL like
/// `ratio=0.6` are rounded to nearest percent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratio(pub u8);

impl Default for Ratio {
    fn default() -> Self {
        Self(50)
    }
}

/// A single PTY pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub kind: PaneKind,
    pub label: Option<String>,
    /// Per-pane shell command override. When `Some(argv)`, the
    /// pane spawns `argv[0]` with `argv[1..]` as arguments instead
    /// of the default `$SHELL` / `/bin/sh`. Parsed from KDL as
    /// `command="htop"` or `command="htop --delay=5"`; the
    /// string is split by whitespace into argv at spawn time.
    /// `None` falls back to `ShellSpec::LoginShell`.
    pub command: Option<String>,
}

/// Flavor of pane. `Widget` carries a `ref_name` that maps to
/// a dynamically-loaded cdylib in `~/.config/cmdash/widgets/<ref_name>/`.
/// `Script` carries a command string that is spawned as a child
/// process speaking the [`cmdash_protocol`] wire format.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PaneKind {
    #[default]
    Shell,
    /// `kind=widget ref-name="<name>"` — loaded via `libloading`
    /// at startup from `~/.config/cmdash/widgets/<ref_name>/`.
    Widget { ref_name: String },
    /// `kind=script command="<cmd>"` — spawned as a child process
    /// with piped stdin/stdout speaking the line-delimited frame
    /// protocol. The command is stored in [`Pane::command`].
    Script,
}

/// A keybind line: `bind "<chord>" action="<action>"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keybind {
    pub mods: Modifiers,
    pub key: KeyToken,
    pub action: KeyAction,
}

/// Modifier mask on a keybind chord.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_: bool,
}

/// A key token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyToken {
    Char(char),
    Named(KeyName),
    F(u8),
}

/// Names of non-character keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyName {
    Enter,
    Escape,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

/// What a keybind triggers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    PaneClose,
    AppNewPane,
    AppClose,
    PaneFocusNext,
    PaneFocusPrev,
    PaneFocusUp,
    PaneFocusDown,
    PaneFocusLeft,
    PaneFocusRight,
    /// `pane.stack.cycle` - cycle focus through members of the
    /// currently-focused `ZStack` (wrap-around from last → first).
    /// No-op if the focused pane is not a member of a `ZStack`.
    /// Phase 4 carry-forward.
    PaneStackCycle,
    /// `pane.stack.down` - directional within-`ZStack` Down:
    /// focus the next member of the focused `ZStack` in
    /// declaration order; if the focused pane is the last (top)
    /// member, hand focus off to the topmost pane geometrically
    /// below the `ZStack` via [`adjacent_pane`]. No-op if the
    /// focused pane is not a `ZStack` member, or if the focused
    /// `ZStack` member has no geometrically-below neighbour.
    /// Phase 4 carry-forward.
    PaneStackDown,
    /// `pane.stack.up` - mirror of `pane.stack.down`: focus
    /// the **previous** member of the focused `ZStack` in
    /// declaration order; if the focused pane is the first
    /// (bottom) member, hand focus off to the topmost pane
    /// geometrically above the `ZStack` via [`adjacent_pane`].
    /// No-op if the focused pane is not a `ZStack` member, or
    /// if the focused `ZStack` member has no geometrically-above
    /// neighbour. Phase 4 carry-forward.
    PaneStackUp,
    /// Phase 4.5/5 carry-forward: `PaneStackLeft`. Cycle
    /// through `ZStack` members in declaration order with
    /// retreat semantics at the FIRST member, then hand off
    /// geometrically to the pane outside the `ZStack` via
    /// `Direction::Left` (column-split trapdoor: a `ZStack`
    /// in the right half of a `split axis=horizontal`
    /// lands on the sibling `Split` member to its LEFT).
    PaneStackLeft,
    /// Phase 4.5/5 carry-forward: `PaneStackRight`. Cycle
    /// through `ZStack` members in declaration order with
    /// advance semantics at the LAST member, then hand off
    /// geometrically to the pane outside the `ZStack` via
    /// `Direction::Right` -- the horizontal-axis mirror of
    /// `PaneStackDown`/`PaneStackUp` on the geometric axis.
    PaneStackRight,
    /// `pane.preset.<name>` - focus a named preset.
    PanePreset(String),
    /// `tab.new` -- create a new empty tab and switch focus to
    /// it. The new tab holds a single `pane kind=shell` leaf at
    /// the active cell-grid area. (M-t default keybind).
    TabNew,
    /// `tab.close` -- close the active tab. All its
    /// `PaneRunner`s are dropped (revoking every `dashcompositor`
    /// `LayerId` per `AGENTS.md` Hard rule); `active_tab` is
    /// clamped to `tabs.len() - 1`. Closing the last tab
    /// quits the binary (matches the `PaneClose` last-pane
    /// semantics). A non-active tab's close is a future
    /// extension. (M-w default keybind).
    TabClose,
    /// `tab.switch.<n>` (n in 1..=9) -- switch focus to the
    /// nth tab; the M-1..M-9 default keybinds.
    TabSwitch(usize),
    /// Enter `PaneResize` mode. Arrow keys will resize the focused
    /// pane's parent split until Escape is pressed.
    EnterPaneResize,
    /// Enter `TabSwitch` mode. Number keys 1-9 switch tabs.
    EnterTabSwitch,
    /// Enter `PresetPick` mode. Number keys select presets.
    EnterPresetPick,
    /// Exit the current mode back to Normal.
    ModeExit,
    /// Resize the focused pane's split in a direction.
    /// Used while in `PaneResize` mode.
    PaneResizeUp,
    PaneResizeDown,
    PaneResizeLeft,
    PaneResizeRight,
    /// Enter copy mode. In copy mode the user can select text
    /// from the focused pane and copy it to the system clipboard.
    EnterCopyMode,
    /// Move the copy-mode cursor up.
    CopyModeMoveUp,
    /// Move the copy-mode cursor down.
    CopyModeMoveDown,
    /// Move the copy-mode cursor left.
    CopyModeMoveLeft,
    /// Move the copy-mode cursor right.
    CopyModeMoveRight,
    /// Begin or extend the selection from the current copy-mode
    /// cursor position.
    CopyModeStartSelection,
    /// Copy the selected text to the system clipboard and exit
    /// copy mode.
    CopyModeCopy,
}

/// A cmdash config error.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cmdash config: KDL syntax error:\n{0}")]
    Kdl(String),
    #[error("unknown top-level KDL node `{0}`")]
    UnknownTopLevel(String),
    #[error("unknown layout KDL node `{0}`")]
    UnknownLayoutNode(String),
    #[error("`keybinds` may only contain `bind`, got `{0}`")]
    UnexpectedKindbindChild(String),
    #[error("`presets` block is empty")]
    EmptyChildren(&'static str),
    #[error("duplicate `layout` block")]
    DuplicateLayout,
    #[error("duplicate `presets` block")]
    DuplicatePresets,
    #[error("duplicate `status_bar` block")]
    DuplicateStatusBar,
    #[error("`presets` may only contain `preset`, got `{0}`")]
    UnexpectedPresetsChild(String),
    #[error("duplicate preset name `{0}`")]
    DuplicatePreset(String),
    #[error("invalid `axis` value `{0}`")]
    InvalidAxis(String),
    #[error("invalid `ratio` value `{0}`")]
    InvalidRatio(String),
    #[error("invalid `kind` value `{0}`")]
    InvalidPaneKind(String),
    #[error("invalid chord: {0}")]
    InvalidChord(String),
    #[error("invalid action: {0}")]
    InvalidAction(String),
    #[error("invalid status_bar: {0}")]
    InvalidStatusBar(String),
    #[error("invalid theme: {0}")]
    InvalidTheme(String),
    #[error("`split` must have exactly 2 children; got {0}")]
    SplitChildCount(usize),
    #[error("duplicate `theme` block")]
    DuplicateTheme,
}

/// A non-fatal syntax hint surfaced by the pre-scan validator.
/// These are advisory — the KDL parser may still succeed — but
/// flagging them early gives users actionable feedback before
/// the opaque "KDL syntax error" message from kdl-rs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigWarning {
    /// Possible unclosed brace, missing value after `=`,
    /// empty block, or trailing comma.
    SyntaxHint { line: usize, message: String },
}

impl std::fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigWarning::SyntaxHint { line, message } => {
                write!(f, "line {line}: {message}")
            }
        }
    }
}

/// Parse a cmdash configuration from raw KDL source.
pub fn parse(source: &str) -> Result<Config, ConfigError> {
    let mut errors = Vec::new();
    let cfg = parse_into(source, &mut errors);
    match errors.into_iter().next() {
        Some(e) => Err(e),
        None => Ok(cfg),
    }
}

/// Parse a cmdash configuration, collecting errors instead of
/// short-circuiting on the first one. Returns the partially-parsed
/// config (populated fields for successful top-level nodes) alongside
/// the list of errors encountered. Used by the config hot-reload
/// watcher so a single bad node doesn't prevent other nodes from
/// being loaded.
pub fn parse_collect(source: &str) -> (Config, Vec<ConfigError>) {
    let mut errors = Vec::new();
    let cfg = parse_into(source, &mut errors);
    (cfg, errors)
}

/// Shared walker: populates a [`Config`] from KDL source,
/// pushing any errors into `errors` instead of returning them.
/// Both [`parse`] and [`parse_collect`] delegate here so the
/// top-level node dispatch stays in one place.
fn parse_into(source: &str, errors: &mut Vec<ConfigError>) -> Config {
    let doc: KdlDocument = match source.parse() {
        Ok(d) => d,
        Err(e) => {
            errors.push(ConfigError::Kdl(e.to_string()));
            return Config::default();
        }
    };
    let mut cfg = Config::default();
    for n in doc.nodes() {
        let name = n.name().value();
        match name {
            "layout" => {
                if cfg.layout.is_some() {
                    errors.push(ConfigError::DuplicateLayout);
                    continue;
                }
                let children = match n.children() {
                    Some(c) => c,
                    None => {
                        errors.push(ConfigError::UnknownLayoutNode(
                            "layout block must contain a LayoutNode".into(),
                        ));
                        continue;
                    }
                };
                let kids = children.nodes();
                let first = match kids.first() {
                    Some(f) => f,
                    None => {
                        errors.push(ConfigError::UnknownLayoutNode(
                            "layout block must contain a LayoutNode".into(),
                        ));
                        continue;
                    }
                };
                if kids.len() > 1 {
                    errors.push(ConfigError::UnknownLayoutNode(
                        "layout block may contain exactly one LayoutNode".into(),
                    ));
                    continue;
                }
                match read_layout(first) {
                    Ok(node) => cfg.layout = Some(node),
                    Err(e) => errors.push(e),
                }
            }
            "keybinds" => {
                if let Some(c) = n.children() {
                    for k in c.nodes() {
                        if k.name().value() != "bind" {
                            errors.push(ConfigError::UnexpectedKindbindChild(
                                k.name().value().to_string(),
                            ));
                            continue;
                        }
                        match read_keybind(k) {
                            Ok(kb) => cfg.keybinds.push(kb),
                            Err(e) => errors.push(e),
                        }
                    }
                }
            }
            "presets" => {
                if !cfg.presets.is_empty() {
                    errors.push(ConfigError::DuplicatePresets);
                    continue;
                }
                let c = match n.children() {
                    Some(c) => c,
                    None => {
                        errors.push(ConfigError::EmptyChildren("presets"));
                        continue;
                    }
                };
                for k in c.nodes() {
                    if k.name().value() != "preset" {
                        errors.push(ConfigError::UnexpectedPresetsChild(
                            k.name().value().to_string(),
                        ));
                        continue;
                    }
                    match read_named_preset(k) {
                        Ok((name, body)) => {
                            use std::collections::btree_map::Entry;
                            match cfg.presets.entry(name) {
                                Entry::Vacant(e) => {
                                    e.insert(body);
                                }
                                Entry::Occupied(e) => {
                                    errors.push(ConfigError::DuplicatePreset(e.key().clone()));
                                }
                            }
                        }
                        Err(e) => errors.push(e),
                    }
                }
            }
            "status_bar" => {
                if cfg.status_bar.is_some() {
                    errors.push(ConfigError::DuplicateStatusBar);
                    continue;
                }
                match read_status_bar(n) {
                    Ok(bar) => cfg.status_bar = Some(bar),
                    Err(e) => errors.push(e),
                }
            }
            "theme" => {
                if cfg.theme.is_some() {
                    errors.push(ConfigError::DuplicateTheme);
                    continue;
                }
                match read_theme(n) {
                    Ok(t) => cfg.theme = Some(t),
                    Err(e) => errors.push(e),
                }
            }
            other => {
                errors.push(ConfigError::UnknownTopLevel(other.into()));
            }
        }
    }
    cfg
}

fn read_layout(n: &KdlNode) -> Result<LayoutNode, ConfigError> {
    match n.name().value() {
        "split" => read_split(n),
        "stack" => read_stack(n),
        "zstack" => read_zstack(n),
        "pane" => read_pane(n),
        "preset" => read_preset(n),
        other => Err(ConfigError::UnknownLayoutNode(other.into())),
    }
}

/// Parse `zstack { pane_split_pane }` blocks. Mirrors `read_stack`
/// but disambiguates intent: members share the parent's rect
/// (overlay z-stacked panes), not the strip-split geometry of
/// `stack`. Empty `zstack { }` returns `LayoutNode::ZStack { panes:
/// vec![] }`; the resolver surfaces
/// `LayoutError::EmptyChildren("zstack")` if encountered.
fn read_zstack(n: &KdlNode) -> Result<LayoutNode, ConfigError> {
    let mut panes = Vec::new();
    if let Some(c) = n.children() {
        for child in c.nodes() {
            panes.push(read_layout(child)?);
        }
    }
    Ok(LayoutNode::ZStack { panes })
}

fn read_split(n: &KdlNode) -> Result<LayoutNode, ConfigError> {
    let mut axis = SplitAxis::default();
    let mut ratio: f64 = 0.5;
    for entry in n.entries() {
        let key = entry.name().map(|id| id.value());
        let raw = entry_to_string(entry);
        match (key, raw.as_str()) {
            (Some("axis"), "vertical") => axis = SplitAxis::Vertical,
            (Some("axis"), "horizontal") => {}
            (Some("axis"), _) if !raw.is_empty() => {
                return Err(ConfigError::InvalidAxis(raw));
            }
            (Some("ratio"), _) => {
                ratio = raw
                    .parse::<f64>()
                    .map_err(|_| ConfigError::InvalidRatio(raw.clone()))?;
            }
            _ => {}
        }
    }
    let mut kids = Vec::new();
    if let Some(c) = n.children() {
        for child in c.nodes() {
            kids.push(read_layout(child)?);
        }
    }
    if kids.len() != 2 {
        return Err(ConfigError::SplitChildCount(kids.len()));
    }
    let pct = (ratio * 100.0).round().clamp(0.0, 100.0) as u8;
    Ok(LayoutNode::Split {
        axis,
        ratio: Ratio(pct),
        children: kids,
    })
}

fn read_stack(n: &KdlNode) -> Result<LayoutNode, ConfigError> {
    let mut panes = Vec::new();
    if let Some(c) = n.children() {
        for child in c.nodes() {
            panes.push(read_layout(child)?);
        }
    }
    Ok(LayoutNode::Stack { panes })
}

fn read_pane(n: &KdlNode) -> Result<LayoutNode, ConfigError> {
    let mut kind_is_widget = false;
    let mut kind_is_script = false;
    let mut ref_name: Option<String> = None;
    let mut label: Option<String> = None;
    let mut command: Option<String> = None;
    for entry in n.entries() {
        let key = entry.name().map(|id| id.value());
        let raw = entry_to_string(entry);
        match (key, raw.as_str()) {
            (Some("kind"), "shell") => {}
            (Some("kind"), "widget") => kind_is_widget = true,
            (Some("kind"), "script") => kind_is_script = true,
            (Some("kind"), _) if !raw.is_empty() => {
                return Err(ConfigError::InvalidPaneKind(raw));
            }
            (Some("ref-name"), _) => ref_name = Some(raw),
            (Some("label"), _) => label = Some(raw),
            (Some("command"), _) => command = Some(raw),
            _ => {}
        }
    }
    let kind = if kind_is_widget {
        let rn = ref_name.ok_or_else(|| {
            ConfigError::InvalidPaneKind("widget kind requires `ref-name` attribute".into())
        })?;
        PaneKind::Widget { ref_name: rn }
    } else if kind_is_script {
        if command.is_none() {
            return Err(ConfigError::InvalidPaneKind(
                "script kind requires `command` attribute".into(),
            ));
        }
        PaneKind::Script
    } else {
        PaneKind::Shell
    };
    Ok(LayoutNode::Pane(Pane {
        kind,
        label,
        command,
    }))
}

fn read_preset(n: &KdlNode) -> Result<LayoutNode, ConfigError> {
    let mut name = String::new();
    for entry in n.entries() {
        if entry.name().map(|id| id.value()) == Some("name") {
            name = entry_to_string(entry);
            break;
        }
    }
    if name.is_empty() {
        for entry in n.entries() {
            if entry.name().is_none() {
                name = entry_to_string(entry);
                break;
            }
        }
    }
    if name.is_empty() {
        return Err(ConfigError::InvalidAction(
            "preset node requires a name argument".into(),
        ));
    }
    Ok(LayoutNode::Preset { name })
}

/// Read a `preset "name" { body }` block under the top-level
/// `presets { ... }` namespace. The body's children are parsed as
/// a [`LayoutNode`] tree (Split / Stack / Pane / nested Preset).
///
/// The first child of the preset block wins; subsequent children
/// are ignored so the future "multi-body preset" extension
/// doesn't have to break this schema. The intent is the inverse
/// of [`read_layout`] — a name-bearing layout WRAPPER that owns
/// its body inline.
fn read_named_preset(n: &KdlNode) -> Result<(String, LayoutNode), ConfigError> {
    let mut name = String::new();
    for entry in n.entries() {
        if entry.name().map(|id| id.value()) == Some("name") {
            name = entry_to_string(entry);
            break;
        }
    }
    if name.is_empty() {
        for entry in n.entries() {
            if entry.name().is_none() {
                name = entry_to_string(entry);
                break;
            }
        }
    }
    if name.is_empty() {
        return Err(ConfigError::InvalidAction(
            "preset block requires a name argument".into(),
        ));
    }
    let kids = n
        .children()
        .ok_or_else(|| {
            ConfigError::UnknownLayoutNode("preset block must contain a LayoutNode body".into())
        })?
        .nodes();
    let first = kids.first().ok_or_else(|| {
        ConfigError::UnknownLayoutNode("preset block must contain a LayoutNode body".into())
    })?;
    if kids.len() > 1 {
        return Err(ConfigError::UnknownLayoutNode(
            "preset block may contain exactly one LayoutNode body".into(),
        ));
    }
    let body = read_layout(first)?;
    Ok((name, body))
}

/// Parse a top-level `status_bar { ... }` block.
///
/// Recognized keys:
/// - `enabled` (bool, default `false`)
/// - `position` (string: `"top"` or `"bottom"`, default `"bottom"`)
/// - `show-clock` (bool, default `true`)
/// - `show-pane-title` (bool, default `true`)
/// - `show-mode` (bool, default `true`)
fn read_status_bar(n: &KdlNode) -> Result<Bar, ConfigError> {
    let mut bar = Bar::default();
    if let Some(c) = n.children() {
        for child in c.nodes() {
            match child.name().value() {
                "enabled" => {
                    bar.enabled = child
                        .entries()
                        .first()
                        .and_then(|e| e.value().as_bool())
                        .unwrap_or(true);
                }
                "position" => {
                    let val = child
                        .entries()
                        .first()
                        .map(entry_to_string)
                        .unwrap_or_default();
                    match val.as_str() {
                        "top" => bar.position = BarPosition::Top,
                        "bottom" => bar.position = BarPosition::Bottom,
                        other => {
                            return Err(ConfigError::InvalidStatusBar(format!(
                                "unknown position `{other}`; expected \"top\" or \"bottom\""
                            )));
                        }
                    }
                }
                "show-clock" => {
                    bar.show_clock = child
                        .entries()
                        .first()
                        .and_then(|e| e.value().as_bool())
                        .unwrap_or(true);
                }
                "show-pane-title" => {
                    bar.show_pane_title = child
                        .entries()
                        .first()
                        .and_then(|e| e.value().as_bool())
                        .unwrap_or(true);
                }
                "show-mode" => {
                    bar.show_mode = child
                        .entries()
                        .first()
                        .and_then(|e| e.value().as_bool())
                        .unwrap_or(true);
                }
                other => {
                    return Err(ConfigError::InvalidStatusBar(format!(
                        "unknown key `{other}` in status_bar block"
                    )));
                }
            }
        }
    }
    Ok(bar)
}

/// Parse a top-level `theme { ... }` block.
///
/// Recognized keys:
/// - Terminal defaults: `default-fg`, `default-bg`, `cursor-style`
/// - Tab bar: `tab-bar-bg`, `tab-active-bg`, `tab-active-fg`,
///   `tab-inactive-bg`, `tab-inactive-fg`
/// - Status bar: `status-bar-bg`, `status-mode-fg`, `status-mode-bg`,
///   `status-clock-fg`, `status-pane-title-fg`
/// - Widget/border: `border-color`, `error-color`
///
/// Color values are parsed by [`theme::parse_color`].
/// `cursor-style` accepts `"block"`, `"underline"`, or `"bar"`.
fn read_theme(n: &KdlNode) -> Result<Theme, ConfigError> {
    let mut theme = Theme::default();
    if let Some(c) = n.children() {
        for child in c.nodes() {
            let val = child
                .entries()
                .first()
                .and_then(|e| e.value().as_string())
                .unwrap_or("");
            match child.name().value() {
                "cursor-style" => {
                    let cursor = theme::CursorStyle::parse(val).ok_or_else(|| {
                        ConfigError::InvalidTheme(format!(
                            "invalid cursor style `{val}`; expected block, underline, or bar"
                        ))
                    })?;
                    theme.cursor_style = Some(cursor);
                }
                key @ ("default-fg"
                | "default-bg"
                | "tab-bar-bg"
                | "tab-active-bg"
                | "tab-active-fg"
                | "tab-inactive-bg"
                | "tab-inactive-fg"
                | "status-bar-bg"
                | "status-mode-fg"
                | "status-mode-bg"
                | "status-clock-fg"
                | "status-pane-title-fg"
                | "border-color"
                | "error-color") => {
                    let color = theme::parse_color(val).ok_or_else(|| {
                        ConfigError::InvalidTheme(format!(
                            "invalid color value `{val}` for key `{key}`"
                        ))
                    })?;
                    match key {
                        "default-fg" => theme.default_fg = Some(color),
                        "default-bg" => theme.default_bg = Some(color),
                        "tab-bar-bg" => theme.tab_bar_bg = Some(color),
                        "tab-active-bg" => theme.tab_active_bg = Some(color),
                        "tab-active-fg" => theme.tab_active_fg = Some(color),
                        "tab-inactive-bg" => theme.tab_inactive_bg = Some(color),
                        "tab-inactive-fg" => theme.tab_inactive_fg = Some(color),
                        "status-bar-bg" => theme.status_bar_bg = Some(color),
                        "status-mode-fg" => theme.status_mode_fg = Some(color),
                        "status-mode-bg" => theme.status_mode_bg = Some(color),
                        "status-clock-fg" => theme.status_clock_fg = Some(color),
                        "status-pane-title-fg" => theme.status_pane_title_fg = Some(color),
                        "border-color" => theme.border_color = Some(color),
                        "error-color" => theme.error_color = Some(color),
                        _ => unreachable!(),
                    }
                }
                other => {
                    return Err(ConfigError::InvalidTheme(format!(
                        "unknown key `{other}` in theme block"
                    )));
                }
            }
        }
    }
    Ok(theme)
}

fn read_keybind(n: &KdlNode) -> Result<Keybind, ConfigError> {
    let mut chord_str: Option<String> = None;
    for entry in n.entries() {
        if entry.name().is_none() {
            chord_str = Some(entry_to_string(entry));
            break;
        }
    }
    let mut action_str: Option<String> = None;
    for entry in n.entries() {
        let key = entry.name().map(|id| id.value());
        match key {
            Some("action") => action_str = Some(entry_to_string(entry)),
            Some(other) => {
                return Err(ConfigError::InvalidChord(format!(
                    "unknown argument `{other}`"
                )))
            }
            None => {}
        }
    }
    let chord_str = chord_str.ok_or_else(|| ConfigError::InvalidChord("missing chord".into()))?;
    let action_str =
        action_str.ok_or_else(|| ConfigError::InvalidAction("missing action".into()))?;
    let (mods, key) =
        parse_chord(&chord_str).ok_or_else(|| ConfigError::InvalidChord(chord_str.clone()))?;
    // Hint-augmented reject path: when parse_action declines a
    // tab.switch.<n> input the user sees the valid range (1..=9)
    // instead of just the echo of their own string.
    let action = match parse_action(&action_str) {
        Some(a) => a,
        None => {
            let hint = action_tab_switch_hint(&action_str);
            return Err(ConfigError::InvalidAction(if hint.is_empty() {
                action_str.clone()
            } else {
                format!("{action_str}{hint}")
            }));
        }
    };
    Ok(Keybind { mods, key, action })
}

fn parse_chord(s: &str) -> Option<(Modifiers, KeyToken)> {
    let mut mods = Modifiers::default();
    let mut key_part: Option<&str> = None;
    for part in s.split('-') {
        match part {
            "ctrl" | "control" | "ctl" => mods.ctrl = true,
            "shift" => mods.shift = true,
            "alt" | "meta" | "m" | "M" => mods.alt = true,
            "super" | "cmd" | "win" => mods.super_ = true,
            other => {
                if key_part.is_some() {
                    return None;
                }
                key_part = Some(other);
            }
        }
    }
    Some((mods, parse_key_token(key_part?)?))
}

fn parse_key_token(s: &str) -> Option<KeyToken> {
    if let Some(rest) = s.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u8>() {
            if (1..=24).contains(&n) {
                return Some(KeyToken::F(n));
            }
        }
    }
    match s {
        "enter" | "return" => Some(KeyToken::Named(KeyName::Enter)),
        "esc" | "escape" => Some(KeyToken::Named(KeyName::Escape)),
        "tab" => Some(KeyToken::Named(KeyName::Tab)),
        "backspace" | "bs" => Some(KeyToken::Named(KeyName::Backspace)),
        "up" => Some(KeyToken::Named(KeyName::Up)),
        "down" => Some(KeyToken::Named(KeyName::Down)),
        "left" => Some(KeyToken::Named(KeyName::Left)),
        "right" => Some(KeyToken::Named(KeyName::Right)),
        "home" => Some(KeyToken::Named(KeyName::Home)),
        "end" => Some(KeyToken::Named(KeyName::End)),
        "pageup" | "pgup" => Some(KeyToken::Named(KeyName::PageUp)),
        "pagedown" | "pgdn" => Some(KeyToken::Named(KeyName::PageDown)),
        _ => {
            let mut chars = s.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Some(KeyToken::Char(c))
        }
    }
}

fn parse_action(s: &str) -> Option<KeyAction> {
    match s {
        "pane.close" => Some(KeyAction::PaneClose),
        "app.new-pane" | "app.new_pane" => Some(KeyAction::AppNewPane),
        "app.close" => Some(KeyAction::AppClose),
        "pane.focus.next" => Some(KeyAction::PaneFocusNext),
        "pane.focus.prev" => Some(KeyAction::PaneFocusPrev),
        "pane.focus.up" => Some(KeyAction::PaneFocusUp),
        "pane.focus.down" => Some(KeyAction::PaneFocusDown),
        "pane.focus.left" => Some(KeyAction::PaneFocusLeft),
        "pane.focus.right" => Some(KeyAction::PaneFocusRight),
        // Phase 4 carry-forward: per-ZStack focus primitives.
        // `pane.stack.cycle` cycles the focused ZStack's focus
        // forward with wrap-around; `pane.stack.down` is the
        // directional within-ZStack Down with a bottom-edge
        // geometric handoff to the topmost pane below the
        // ZStack; `pane.stack.up` mirrors Down with a top-edge
        // geometric handoff to the topmost pane above the
        // ZStack.
        "pane.stack.cycle" => Some(KeyAction::PaneStackCycle),
        "pane.stack.down" => Some(KeyAction::PaneStackDown),
        "pane.stack.up" => Some(KeyAction::PaneStackUp),
        "pane.stack.left" => Some(KeyAction::PaneStackLeft),
        "pane.stack.right" => Some(KeyAction::PaneStackRight),
        // Mode entry actions.
        "pane.resize.enter" => Some(KeyAction::EnterPaneResize),
        "tab.switch.enter" => Some(KeyAction::EnterTabSwitch),
        "preset.pick.enter" => Some(KeyAction::EnterPresetPick),
        "mode.exit" => Some(KeyAction::ModeExit),
        // Pane resize actions (used inside PaneResize mode).
        "pane.resize.up" => Some(KeyAction::PaneResizeUp),
        "pane.resize.down" => Some(KeyAction::PaneResizeDown),
        "pane.resize.left" => Some(KeyAction::PaneResizeLeft),
        "pane.resize.right" => Some(KeyAction::PaneResizeRight),
        // Copy-mode actions.
        "copy.enter" => Some(KeyAction::EnterCopyMode),
        "copy.move.up" => Some(KeyAction::CopyModeMoveUp),
        "copy.move.down" => Some(KeyAction::CopyModeMoveDown),
        "copy.move.left" => Some(KeyAction::CopyModeMoveLeft),
        "copy.move.right" => Some(KeyAction::CopyModeMoveRight),
        "copy.select" => Some(KeyAction::CopyModeStartSelection),
        "copy.copy" => Some(KeyAction::CopyModeCopy),
        // `pane.preset` (bare, no name suffix) is rejected so a
        // missing name surfaces as `InvalidAction` at config-parse
        // time rather than as a runtime no-op — mirrors the
        // `tab.switch` (no n suffix) convention.
        // Tab-axis actions. `tab.new`, `tab.close`,
        // `tab.switch.<n>` (n in 1..=9) wire the tab keybinds
        // (M-t / M-w / M-1..M-9). The `tab.switch` PLANE (no
        // `<n>` suffix) is rejected so a future typo or partial
        // binding surfaces as `InvalidAction` at config-parse
        // time rather than as a runtime no-op.
        "tab.new" => Some(KeyAction::TabNew),
        "tab.close" => Some(KeyAction::TabClose),
        other => {
            // `tab.switch.<n>` parses before the `pane.preset.<name>`
            // strip so an ambiguous `tab.switch.1` (which starts
            // with `tab.` not `pane.`) doesn't fall through to
            // the preset strip arm.
            if let Some(n_str) = other.strip_prefix("tab.switch.") {
                n_str
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=9).contains(n))
                    .map(KeyAction::TabSwitch)
            } else {
                other
                    .strip_prefix("pane.preset.")
                    .map(|rest| KeyAction::PanePreset(rest.to_string()))
            }
        }
    }
}

/// Render a [`KdlEntry`] to a flat String regardless of literal kind.
fn entry_to_string(entry: &KdlEntry) -> String {
    let v = entry.value();
    if let Some(s) = v.as_string() {
        return s.to_string();
    }
    if let Some(n) = v.as_integer() {
        if let Ok(n64) = i64::try_from(n) {
            return n64.to_string();
        }
        return n.to_string();
    }
    if let Some(f) = v.as_float() {
        return f.to_string();
    }
    if let Some(b) = v.as_bool() {
        return b.to_string();
    }
    if matches!(v, KdlValue::Null) {
        return "null".into();
    }
    String::new()
}

/// Build a hint suffix for `parse_action` rejection messages
/// rooted in the `tab.switch.<n>` family. Returns an empty
/// string for any input that isn't a `tab.switch.*` shape so
/// the caller emits the original action string verbatim.
fn action_tab_switch_hint(s: &str) -> String {
    if s == "tab.switch" {
        return "; missing `.<n>` suffix (use `tab.switch.1`..`tab.switch.9`)".into();
    }
    if let Some(n_str) = s.strip_prefix("tab.switch.") {
        if n_str.is_empty() {
            return "; expected `tab.switch.<n>` with n in 1..=9".into();
        }
        match n_str.parse::<usize>() {
            Ok(n) if (1..=9).contains(&n) => {
                // In-range input should never reach this code
                // path; parse_action would have returned Some(_).
                // Defensive no-op.
                String::new()
            }
            Ok(n) => format!("; valid range for `tab.switch.<n>` is n=1..=9 (got n={n})"),
            Err(_) => "; expected `tab.switch.<n>` where <n> is a decimal integer 1..=9".into(),
        }
    } else {
        String::new()
    }
}

/// Format config errors with source-context caret lines.
///
/// Each error is rendered with the error message followed by
/// the offending source line and a caret pointing to the
/// approximate column. Line numbers are extracted from the
/// error message text where possible (kdl-rs embeds
/// `line:col-line:col` spans in its syntax-error messages).
/// For non-KDL errors (which lack positional info) the
/// function falls back to showing no context line.
pub fn format_errors_with_context(
    errors: &[ConfigError],
    source: &str,
    file_label: Option<&str>,
) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = String::new();
    for (i, err) in errors.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if let Some(label) = file_label {
            out.push_str(&format!("{label}: "));
        }
        let msg = err.to_string();
        out.push_str(&msg);
        // Try to extract a line number from the error message.
        // kdl-rs syntax errors contain spans like "3:5-3:10" or
        // "3:5" in the message body.
        if let Some((line_num, col)) = extract_line_col_from_msg(&msg) {
            let line_idx = line_num.saturating_sub(1); // 1-based → 0-based
            if line_idx < lines.len() {
                out.push('\n');
                out.push_str(&format!("  {}:\n", line_num));
                out.push_str(&format!("  {}\n", lines[line_idx]));
                // Caret underline.
                let col_start = col;
                out.push_str(&format!("  {}^", " ".repeat(col_start)));
            }
        }
        out.push('\n');
    }
    out
}

/// Attempt to extract a `(line, col)` pair from a KDL error
/// message string. kdl-rs embeds spans in the form
/// `N:M-N:M` or `N:M` where `N` is the 1-based line number
/// and `M` is the 1-based column. Returns `None` if no
/// span is found.
fn extract_line_col_from_msg(msg: &str) -> Option<(usize, usize)> {
    // Look for a pattern like "3:5" or "3:5-3:10" in the message.
    for token in msg.split_whitespace() {
        // Try "N:M-N:M" first.
        if let Some(rest) = token.strip_prefix(char::is_numeric) {
            // rest starts after first digit(s) of line number.
            if rest.starts_with(':') {
                // Find the col after ':'
                if let Some(col_part) = rest.strip_prefix(':') {
                    // col_part should start with digits.
                    let col_str: String = col_part
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    if !col_str.is_empty() {
                        let line_str: String = token.chars().take_while(|c| *c != ':').collect();
                        if let (Ok(line), Ok(col)) =
                            (line_str.parse::<usize>(), col_str.parse::<usize>())
                        {
                            return Some((line, col));
                        }
                    }
                }
            }
        }
    }
    // Fallback: scan character-by-character for "digit+:digit+".
    let chars: Vec<char> = msg.chars().collect();
    for w in 0..chars.len() {
        if chars[w].is_ascii_digit() {
            // Collect line digits.
            let mut line_digits = String::new();
            let mut j = w;
            while j < chars.len() && chars[j].is_ascii_digit() {
                line_digits.push(chars[j]);
                j += 1;
            }
            // Expect ':'
            if j < chars.len() && chars[j] == ':' {
                j += 1;
                // Collect col digits.
                let mut col_digits = String::new();
                while j < chars.len() && chars[j].is_ascii_digit() {
                    col_digits.push(chars[j]);
                    j += 1;
                }
                if !line_digits.is_empty() && !col_digits.is_empty() {
                    if let (Ok(line), Ok(col)) =
                        (line_digits.parse::<usize>(), col_digits.parse::<usize>())
                    {
                        return Some((line, col));
                    }
                }
            }
        }
    }
    None
}

/// Lightweight pre-scan of KDL source for common syntax mistakes.
///
/// Runs a character-by-character scan BEFORE the full KDL parse to
/// surface advisory [`ConfigWarning::SyntaxHint`]s for issues the kdl-rs parser
/// either silently accepts or produces opaque error messages for.
///
/// Checks performed:
/// - Unmatched `{` or `}` (brace depth)
/// - Missing value after `=` (e.g. `split axis=` with no value)
/// - Empty blocks `{ }` with no children
/// - Trailing commas at end of a line
pub fn pre_scan_kdl(source: &str) -> Vec<ConfigWarning> {
    let mut hints = Vec::new();
    let mut brace_depth: i32 = 0;
    let mut in_string = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut prev_nonws: char = '\0';
    let mut line_number: usize = 1;

    let chars: Vec<char> = source.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let c = chars[i];

        // Track line numbers.
        if c == '\n' {
            line_number += 1;
            if !in_block_comment {
                in_line_comment = false;
            }
            prev_nonws = '\0';
            i += 1;
            continue;
        }

        // Inside a line comment — skip to end of line.
        if in_line_comment {
            i += 1;
            continue;
        }

        // Inside a block comment — scan for `*/`.
        if in_block_comment {
            if c == '*' && i + 1 < len && chars[i + 1] == '/' {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        // Inside a quoted string — skip to closing quote.
        if in_string {
            if c == '\\' && i + 1 < len {
                i += 2; // skip escaped char
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        // Start of a line comment.
        if c == '/' && i + 1 < len && chars[i + 1] == '/' {
            in_line_comment = true;
            i += 2;
            continue;
        }

        // Start of a block comment.
        if c == '/' && i + 1 < len && chars[i + 1] == '*' {
            in_block_comment = true;
            i += 2;
            continue;
        }

        // Start of a string.
        if c == '"' {
            in_string = true;
            i += 1;
            continue;
        }

        // Brace tracking.
        if c == '{' {
            brace_depth += 1;
            // Empty block check: peek ahead for `}` (ignoring
            // whitespace, `//` line comments, and `/* */` block
            // comments). A block containing only comments is NOT
            // flagged as empty because the comment is meaningful
            // content.
            let mut j = i + 1;
            let mut saw_comment = false;
            loop {
                // Skip whitespace.
                while j < len && chars[j].is_ascii_whitespace() {
                    j += 1;
                }
                // Skip `//` line comments (to end of line).
                if j + 1 < len && chars[j] == '/' && chars[j + 1] == '/' {
                    saw_comment = true;
                    j += 2;
                    while j < len && chars[j] != '\n' {
                        j += 1;
                    }
                    continue;
                }
                // Skip `/* */` block comments.
                if j + 1 < len && chars[j] == '/' && chars[j + 1] == '*' {
                    saw_comment = true;
                    j += 2;
                    while j + 1 < len {
                        if chars[j] == '*' && chars[j + 1] == '/' {
                            j += 2;
                            break;
                        }
                        j += 1;
                    }
                    continue;
                }
                break;
            }
            if !saw_comment && j < len && chars[j] == '}' {
                hints.push(ConfigWarning::SyntaxHint {
                    line: line_number,
                    message: "empty block `{ }` has no children".into(),
                });
            }
        } else if c == '}' {
            brace_depth -= 1;
            if brace_depth < 0 {
                hints.push(ConfigWarning::SyntaxHint {
                    line: line_number,
                    message: "extra closing brace `}` without matching `{`".into(),
                });
            }
        }

        // Missing value after `=`: if `=` is followed by whitespace
        // then `{` or end-of-thing, flag it.
        if c == '=' {
            let mut j = i + 1;
            while j < len && chars[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < len && (chars[j] == '{' || chars[j] == '}') {
                hints.push(ConfigWarning::SyntaxHint {
                    line: line_number,
                    message: "missing value after `=`".into(),
                });
            }
        }

        // Trailing comma: if this line ends with `,` after
        // non-whitespace content, flag it.
        if c == ',' && prev_nonws != '\0' {
            // Check if this is the last non-whitespace on the line.
            let mut j = i + 1;
            let mut trailing = true;
            while j < len {
                if chars[j] == '\n' {
                    break;
                }
                if !chars[j].is_ascii_whitespace() {
                    trailing = false;
                    break;
                }
                j += 1;
            }
            if trailing {
                hints.push(ConfigWarning::SyntaxHint {
                    line: line_number,
                    message: "trailing comma".into(),
                });
            }
        }

        if !c.is_ascii_whitespace() {
            prev_nonws = c;
        }
        i += 1;
    }

    // Unclosed brace: depth > 0 at end of input.
    if brace_depth > 0 {
        hints.push(ConfigWarning::SyntaxHint {
            line: line_number,
            message: format!("unclosed block: {brace_depth} unclosed brace(s) at end of input"),
        });
    }

    hints
}

// ===========================================================================
// Round-trip tests for the KDL parser.
//
// These tests pin the dispatch wiring of `read_layout`. Any
// future variant addition MUST be covered by a round-trip test
// here: if a `match n.name().value()` arm is added to
// `read_layout` without a corresponding `parse_*_layout_round_trip`
// test below, the dispatch silently falls through to
// `UnknownLayoutNode` and reaches the user as a parse error.
// Phase 3 carry-forward regression surface.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: extract the label from a `LayoutNode::Pane(Pane)`
    /// variant. Returns `None` for any non-Pane variant. Used by
    /// the round-trip tests below to keep the `assert_eq!` lines
    /// readable instead of inlining a match in every assertion.
    fn pane_label(node: &LayoutNode) -> Option<String> {
        match node {
            LayoutNode::Pane(p) => p.label.clone(),
            _ => None,
        }
    }

    /// `layout { zstack { ... } }` reaches `read_zstack` and
    /// yields `LayoutNode::ZStack`. Phase 3 carry-forward.
    #[test]
    fn parse_zstack_layout_round_trip() {
        let src = r#"
            layout {
                zstack {
                    pane kind=shell label="top"
                    pane kind=shell label="bottom"
                }
            }
        "#;
        let cfg = parse(src).expect("layout with zstack parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::ZStack { panes } => {
                assert_eq!(panes.len(), 2, "zstack has two panes");
                assert_eq!(pane_label(&panes[0]), Some("top".to_string()));
                assert_eq!(pane_label(&panes[1]), Some("bottom".to_string()));
            }
            other => panic!(
                "expected LayoutNode::ZStack, got a different variant: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// Parity baseline: `layout { stack { ... } }` reaches
    /// `read_stack` and yields `LayoutNode::Stack`. Without this
    /// test, the `stack` arm could regress to `UnknownLayoutNode`
    /// while the `zstack` arm keeps passing.
    #[test]
    fn parse_stack_layout_round_trip() {
        let src = r#"
            layout {
                stack {
                    pane kind=shell label="a"
                    pane kind=shell label="b"
                }
            }
        "#;
        let cfg = parse(src).expect("layout with stack parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::Stack { panes } => {
                assert_eq!(panes.len(), 2);
                assert_eq!(pane_label(&panes[0]), Some("a".to_string()));
                assert_eq!(pane_label(&panes[1]), Some("b".to_string()));
            }
            other => panic!(
                "expected LayoutNode::Stack, got a different variant: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// Sibling baseline: `layout { split { ... } }` still parses
    /// after the Phase 3 additions. Guards against an accidental
    /// regression in `read_split`.
    #[test]
    fn parse_split_layout_round_trip() {
        let src = r#"
            layout {
                split axis=horizontal ratio=0.6 {
                    pane kind=shell label="left"
                    pane kind=shell label="right"
                }
            }
        "#;
        let cfg = parse(src).expect("layout with split parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::Split {
                axis,
                ratio,
                children,
            } => {
                assert_eq!(axis, SplitAxis::default());
                assert_eq!(ratio, Ratio(60));
                assert_eq!(children.len(), 2);
            }
            other => panic!(
                "expected LayoutNode::Split, got a different variant: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// Negative baseline: an unknown inner `LayoutNode` name
    /// (`foo`) must surface as `UnknownLayoutNode`. Catches an
    /// accidental `let _ = ...` fallback arm in `read_layout`.
    #[test]
    fn parse_unknown_inner_layout_node_returns_err() {
        let src = r#"
            layout {
                foo { }
            }
        "#;
        let err = parse(src).expect_err("foo arm is not in the dispatch");
        assert!(
            matches!(err, ConfigError::UnknownLayoutNode(_)),
            "expected UnknownLayoutNode, got: {:?}",
            err
        );
    }

    // ============================================================
    // Tab-action parsing tests.
    //
    // Each new KeyAction variant MUST have a parse_action round-trip test
    // here. Without these, a typo in parse_action silently regresses to
    // `None` and surfaces as `InvalidAction` at config parse time.
    // ============================================================

    /// `tab.new` round-trips into `KeyAction::TabNew`.
    #[test]
    fn parse_tab_new_round_trip() {
        let act = parse_action("tab.new").expect("tab.new parses");
        assert_eq!(act, KeyAction::TabNew);
    }

    /// `tab.close` round-trips into `KeyAction::TabClose`.
    #[test]
    fn parse_tab_close_round_trip() {
        let act = parse_action("tab.close").expect("tab.close parses");
        assert_eq!(act, KeyAction::TabClose);
    }

    /// `tab.switch.<n>` for `n` in 1..=9 round-trips into
    /// `KeyAction::TabSwitch(n)`. Parametric over the full
    /// M-1..M-9 default keybind range from `AGENTS.md` feature #3.
    #[test]
    fn parse_tab_switch_n_round_trip() {
        for n in 1..=9usize {
            let s = format!("tab.switch.{n}");
            let act = parse_action(&s).unwrap_or_else(|| panic!("{s} must parse"));
            assert_eq!(act, KeyAction::TabSwitch(n), "{s}");
        }
    }

    /// Copy-mode action strings round-trip into the expected
    /// `KeyAction` variants.
    #[test]
    fn parse_copy_mode_actions_round_trip() {
        assert_eq!(parse_action("copy.enter"), Some(KeyAction::EnterCopyMode));
        assert_eq!(
            parse_action("copy.move.up"),
            Some(KeyAction::CopyModeMoveUp)
        );
        assert_eq!(
            parse_action("copy.move.down"),
            Some(KeyAction::CopyModeMoveDown)
        );
        assert_eq!(
            parse_action("copy.move.left"),
            Some(KeyAction::CopyModeMoveLeft)
        );
        assert_eq!(
            parse_action("copy.move.right"),
            Some(KeyAction::CopyModeMoveRight)
        );
        assert_eq!(
            parse_action("copy.select"),
            Some(KeyAction::CopyModeStartSelection)
        );
        assert_eq!(parse_action("copy.copy"), Some(KeyAction::CopyModeCopy));
    }

    /// `tab.switch.0` and `tab.switch.10` are OUT OF RANGE
    /// (the `AGENTS.md` range is M-1..M-9, ON-BOUNDS EXCLUSIVE of
    /// 0 and 10). Both `parse_action` inputs return None as a
    /// candidate signal: at config-parse time,
    /// a `Keybind` chord bound to `tab.switch.10` would silently
    /// no-op at dispatch time without this rejection.
    #[test]
    fn parse_tab_switch_out_of_range_returns_none() {
        assert!(
            parse_action("tab.switch.0").is_none(),
            "n=0 is out of range"
        );
        assert!(
            parse_action("tab.switch.10").is_none(),
            "n=10 is out of range"
        );
        assert!(
            parse_action("tab.switch.99").is_none(),
            "n=99 is out of range"
        );
    }

    /// `pane.preset` (NO name suffix) is rejected: a config
    /// that forgets the name should fail loudly at parse time,
    /// not silently no-op at dispatch time. Mirrors the
    /// `tab.switch` (no n suffix) convention.
    #[test]
    fn parse_pane_preset_no_name_returns_none() {
        assert!(
            parse_action("pane.preset").is_none(),
            "pane.preset without name is invalid"
        );
    }

    /// `tab.switch` (NO digit suffix) is rejected: a config
    /// that forgets the n-suffix should fail loudly at parse
    /// time, not silently no-op at dispatch time. Mirrors the
    /// `pane.preset` (no name) convention.
    #[test]
    fn parse_tab_switch_no_n_returns_none() {
        assert!(
            parse_action("tab.switch").is_none(),
            "tab.switch without n is invalid"
        );
        assert!(
            parse_action("tab.switch.").is_none(),
            "tab.switch. (empty n) is invalid"
        );
    }

    /// `tab.foo` (UNRELATED tab-prefix) is rejected: typos
    /// like `tab.newpane` or `tab.close-tab` should NOT fall
    /// through to `pane.preset.foo` (which would wire to
    /// `PanePreset("foo")` — a subtle but real bug shape).
    /// This pins the `parse_action`'s strip-prefix ordering.
    #[test]
    fn parse_tab_unrelated_prefix_returns_none() {
        assert!(
            parse_action("tab.foo").is_none(),
            "tab.foo must not fall through to pane.preset"
        );
        assert!(parse_action("tab.newpane").is_none());
        assert!(parse_action("tab.close-tab").is_none());
    }

    /// `split` with 3 children is rejected at parse time.
    #[test]
    fn parse_split_three_children_returns_err() {
        let src = r#"
            layout {
                split axis=horizontal ratio=0.5 {
                    pane kind=shell label="a"
                    pane kind=shell label="b"
                    pane kind=shell label="c"
                }
            }
        "#;
        let err = parse(src).expect_err("3-child split must error");
        assert!(
            matches!(err, ConfigError::SplitChildCount(3)),
            "expected SplitChildCount(3), got: {err:?}"
        );
    }

    /// `split` with 1 child is rejected at parse time.
    #[test]
    fn parse_split_one_child_returns_err() {
        let src = r#"
            layout {
                split axis=horizontal ratio=0.5 {
                    pane kind=shell label="a"
                }
            }
        "#;
        let err = parse(src).expect_err("1-child split must error");
        assert!(
            matches!(err, ConfigError::SplitChildCount(1)),
            "expected SplitChildCount(1), got: {err:?}"
        );
    }

    /// `split` with 0 children is rejected at parse time.
    #[test]
    fn parse_split_zero_children_returns_err() {
        let src = r#"
            layout {
                split axis=horizontal ratio=0.5 {
                }
            }
        "#;
        let err = parse(src).expect_err("0-child split must error");
        assert!(
            matches!(err, ConfigError::SplitChildCount(0)),
            "expected SplitChildCount(0), got: {err:?}"
        );
    }

    // ===========================================================
    // parse_chord aliases for `m`/`M`.
    //
    // Pin that `M-` and `m-` chords route to Modifiers.alt so
    // a future contributor who accidentally drops the lowercase
    // `m` arm cannot silently regress the `cmdash/config.kdl`'s
    // `M-1`..`M-9` + `M-t` + `M-w` keybinds.
    // ===========================================================

    /// `bind "M-t"` parses as (mods.alt=true, key=t) — pins the
    /// uppercase-M arm.
    #[test]
    fn parse_chord_uppercase_m_alias_routes_to_alt() {
        let (mods, _key) = parse_chord("M-t").expect("M-t must parse");
        assert!(mods.alt);
        assert!(!mods.ctrl);
        assert!(!mods.shift);
        assert!(!mods.super_);
    }

    /// `bind "m-t"` parses as (mods.alt=true, key=t) — pins the
    /// lowercase-m arm. Conventional short form for alt/meta in
    /// zellij / older tmux configs.
    #[test]
    fn parse_chord_lowercase_m_alias_routes_to_alt() {
        let (mods, _key) = parse_chord("m-t").expect("m-t must parse");
        assert!(mods.alt);
        assert!(!mods.ctrl);
        assert!(!mods.shift);
        assert!(!mods.super_);
    }

    /// `parse_chord("M")` (single token, no dash) returns `None`.
    /// Pins the negative case: a single-token modifier alias without
    /// a `key_part` must NOT slip through to `Some((Mods::alt, KeyToken::Char('M')))`.
    /// Without this pin, a future refactor that defaults the
    /// missing `key_part` to the modifier string would regress.
    #[test]
    fn parse_chord_bare_m_returns_none() {
        assert!(parse_chord("M").is_none());
        assert!(parse_chord("m").is_none());
    }

    /// A `zstack` nested inside a `split` round-trips: the
    /// visitor walks the children of split's body, finds the
    /// inner `zstack`, and dispatches to `read_zstack`. Pins
    /// the resolver-aware scope-by-parent-area invariant.
    #[test]
    fn parse_zstack_within_split_round_trip() {
        let src = r#"
            layout {
                split axis=horizontal ratio=0.5 {
                    zstack {
                        pane kind=shell label="overlay"
                        pane kind=shell label="overlay_below"
                    }
                    pane kind=shell label="tail"
                }
            }
        "#;
        let cfg = parse(src).expect("split-with-zstack parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::Split { children, .. } => {
                assert_eq!(children.len(), 2);
                match &children[0] {
                    LayoutNode::ZStack { panes } => {
                        assert_eq!(panes.len(), 2);
                        assert_eq!(pane_label(&panes[0]), Some("overlay".to_string()));
                        assert_eq!(pane_label(&panes[1]), Some("overlay_below".to_string()));
                    }
                    other => panic!(
                        "expected ZStack child[0], got: {:?}",
                        std::mem::discriminant(other)
                    ),
                }
                match &children[1] {
                    LayoutNode::Pane(_) => {}
                    other => panic!(
                        "expected Pane child[1], got: {:?}",
                        std::mem::discriminant(other)
                    ),
                }
            }
            other => panic!(
                "expected LayoutNode::Split, got a different variant: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    // ============================================================
    // parse_chord audit tests.
    //
    // `parse_chord` must:
    //   1. parse all default chords from config.kdl via the
    //      public `parse()` API without `ConfigError::InvalidChord`.
    //   2. parse the canonical modifier prefixes: `ctrl`,
    //      `control`, `ctl`, `shift`, `alt`, `meta`, `m`, `M`,
    //      `super`, `cmd`, `win`.
    //   3. ANY OTHER modifier prefix MUST surface as
    //      `ConfigError::InvalidChord` rather than panic or
    //      silently mis-parse the prefix as the key token.
    //
    // When adding a new modifier arm to `parse_chord`, update
    // these tests in the same commit: move the prefix from the
    // `unknown_prefixes` list to `known_chords`.
    // ============================================================

    /// (1) Verbatim copy of the 14 default keybinds from
    /// `crates/cmdash/config.kdl`. Drives each through the
    /// public `parse()` API in a single round-trip and
    /// asserts EVERY chord survives without surfacing
    /// `ConfigError::InvalidChord`. Pins the wire-level
    /// contract that `cmdash::run`'s config-parse step
    /// (which converts KDL into `Router`) succeeds for
    /// the canonical default surface.
    #[test]
    fn audit_canonical_config_kdl_14_chords_round_trip() {
        // Verbatim chord strings from `crates/cmdash/config.kdl`.
        let canonical_chords = [
            "alt-w", "alt-q", "ctrl-a", "M-t", "M-w", "M-1", "M-2", "M-3", "M-4", "M-5", "M-6",
            "M-7", "M-8", "M-9",
        ];
        assert_eq!(
            canonical_chords.len(),
            14,
            "fixture invariant: cmdash/config.kdl ships exactly 14 default keybinds",
        );
        let mut kdl = String::from("keybinds {\n");
        for chord in canonical_chords.iter() {
            kdl.push_str(&format!("    bind \"{chord}\" action=\"app.new-pane\"\n"));
        }
        kdl.push_str("}\n");
        let cfg = parse(&kdl).expect(
            "all 14 cmdash/config.kdl default keybinds must parse without surfacing \
             ConfigError::InvalidChord",
        );
        assert_eq!(
            cfg.keybinds.len(),
            14,
            "all 14 chord strings must round-trip into 14 Keybind entries",
        );
        // Shape pin: per-chord expectations on (mods.mask).
        // The expected list mirrors `canonical_chords` index-
        // for-index so a reader can diff the two side by side.
        // Order: (ctrl, shift, alt, super).
        let expected_mods: &[(bool, bool, bool, bool)] = &[
            (false, false, true, false), // alt-w
            (false, false, true, false), // alt-q
            (true, false, false, false), // ctrl-a
            (false, false, true, false), // M-t
            (false, false, true, false), // M-w
            (false, false, true, false), // M-1
            (false, false, true, false), // M-2
            (false, false, true, false), // M-3
            (false, false, true, false), // M-4
            (false, false, true, false), // M-5
            (false, false, true, false), // M-6
            (false, false, true, false), // M-7
            (false, false, true, false), // M-8
            (false, false, true, false), // M-9
        ];
        assert_eq!(
            canonical_chords.len(),
            expected_mods.len(),
            "fixture invariant: canonical_chords.len() == expected_mods.len()",
        );
        for (idx, chord) in canonical_chords.iter().enumerate() {
            let got = &cfg.keybinds[idx];
            let got_mods = (got.mods.ctrl, got.mods.shift, got.mods.alt, got.mods.super_);
            assert_eq!(
                got_mods, expected_mods[idx],
                "modifier mask mismatch for canonical {chord}",
            );
        }
    }

    /// (2) Augmentation surface: future one-off keybind
    /// prefixes layering `super` / `shift` / multi-modifier
    /// combos on the canonical v1 surface. Without this pin a
    /// future contributor who accidentally drops the `super` /
    /// `shift` arms from `parse_chord`'s match silently
    /// regresses; this test asserts the modifier-arm coverage
    /// for the v1 modifiers so any drop fails immediately at
    /// unit-test time, NOT at binary startup.
    // The unmodified modifier-mask tuple shape
    // `(&str, (bool, bool, bool, bool))` is the test's
    // natural data form. The clippy `type_complexity`
    // lint's threshold is conservatively configured for
    // production code; the test surface is hand-curated
    // and the tuple shape reads more clearly than a
    // factored `type` alias -- `clippy::type_complexity`
    // is allowed here.
    #[test]
    #[allow(clippy::type_complexity)]
    fn audit_ctrl_shift_super_prefixed_chords_parse() {
        let augmented: &[(&str, (bool, bool, bool, bool))] = &[
            // (chord, (ctrl, shift, alt, super))
            ("ctrl-x", (true, false, false, false)),
            ("ctrl-tab", (true, false, false, false)),
            ("ctrl-shift-a", (true, true, false, false)),
            ("super-r", (false, false, false, true)),
            ("super-l", (false, false, false, true)),
            ("shift-tab", (false, true, false, false)),
            ("ctrl-alt-a", (true, false, true, false)),
            ("ctrl-ctl-x", (true, false, false, false)),
            ("shift-shift-a", (false, true, false, false)),
            ("alt-meta-a", (false, false, true, false)), // NOTE: parse_chord treats `cmd` and `win` as
            // `super` aliases (see `parse_chord`'s match arms).
            // Chords that combine `alt` with `cmd`/`win`
            // therefore produce BOTH alt+super in the
            // returned `Mods`, not just alt -- this is the
            // documented v1 alias shape. The test's expected
            // mask mirrors that.
            ("alt-cmd-a", (false, false, true, true)),
            ("alt-win-a", (false, false, true, true)),
            ("control-a", (true, false, false, false)),
        ];
        for (chord, exp_mods) in augmented.iter() {
            // Drive through the public `parse` API for the
            // wire-level round-trip.
            let kdl = format!("keybinds {{\n    bind \"{chord}\" action=\"app.new-pane\"\n}}\n");
            let cfg = parse(&kdl).unwrap_or_else(|e| {
                panic!("{chord} must parse through the public parse() API: {e:?}")
            });
            let kb = cfg
                .keybinds
                .first()
                .unwrap_or_else(|| panic!("{chord} must yield at least one Keybind"));
            let got_mods = (kb.mods.ctrl, kb.mods.shift, kb.mods.alt, kb.mods.super_);
            assert_eq!(
                got_mods, *exp_mods,
                "modifier mask mismatch for augmented {chord}",
            );
            // Confirm SOME key shape was parsed (we don't pin
            // the exact KeyToken variant because augmented
            // chords may legally use Named or F variants).
            match &kb.key {
                KeyToken::Char(_) | KeyToken::Named(_) | KeyToken::F(_) => {}
            }
        }
    }

    /// (3) Negative side of the audit contract: future
    /// un-handled modifier prefixes (e.g. `hyper-<key>`,
    /// `leader-<key>`, `mod-<key>`, `altgr-<key>`,
    /// `fn-<key>`) MUST surface as `ConfigError::InvalidChord`
    /// rather than panic AND rather than silently mis-parse
    /// the prefix as the key token. Pins the panic-safety +
    /// fail-loud-at-parse-time contract.
    ///
    /// Input list is exhaustively partitioned into three
    /// categories:
    ///
    ///   (a) Real-world un-handled modifier aliases (`hyper`,
    ///       `leader`, `mod`, `altgr`, `fn`). The category
    ///       that motivated the audit.
    ///   (b) Empty / degenerate inputs that must NOT panic.
    ///       `""`, `"-"`, `"-a"`, `"--a"`, `"ctrl-"`.
    ///       Defensive: panic-safety holds even on malformed
    ///       KDL input.
    ///   (c) Multi-char unknown tokens that look like keys.
    ///       `"abc"`, `"f99"`, `"f0"`, `"f-1"`. These would
    ///       silently succeed in a buggy impl that falls
    ///       back to "treat the unknown-prefix as key" inside
    ///       `parse_chord`'s `other` arm. The
    ///       `parse_key_token` rejection must surface as
    ///       `InvalidChord`.
    #[test]
    fn audit_unknown_modifier_prefix_returns_invalid_chord_not_panic() {
        let unknown_prefix_chords = [
            // (a) Real-world un-handled modifier aliases.
            "hyper-a", "leader-a", "mod-a", "altgr-a", "fn-a",
            // (b) Empty / degenerate inputs.
            "", "-", "-a", "--a", "ctrl-", // (c) Multi-char unknown tokens.
            "abc", "f99", "f0", "f-1",
        ];
        for chord in unknown_prefix_chords.iter() {
            let kdl = format!("keybinds {{\n    bind \"{chord}\" action=\"app.new-pane\"\n}}\n");
            let result = parse(&kdl);
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!(
                    "{chord:?} must NOT parse cleanly (parse_chord silently \
                     treats the unknown prefix as a key token)"
                ),
            };
            assert!(
                matches!(err, ConfigError::InvalidChord(_)),
                "{chord:?} must surface as ConfigError::InvalidChord; got {err:?}",
            );
        }
    }

    /// (4) Direct-call panic-safety pin. `parse_chord` is
    /// `fn` (not `pub fn`), but the descending-mod rule lets
    /// cfg(test) read it. Calls `parse_chord` on every
    /// unknown-modifier-prefix input from the audit surface
    /// AND on every canonical/augmented chord (positive
    /// controls) and asserts that NONE of them panics. A
    /// future contributor who introduces a panic path in
    /// `parse_chord`'s body would surface as a test-process
    /// abort (the panic propagates out of the loop), exactly
    /// the symptom the audit asks to prevent. We
    /// intentionally do NOT `assert!` on the return value --
    /// only on the absence of panic -- because tests (1)/(2)
    /// and (3) cover the wire-level shape.
    /// (4) Direct-call panic-safety + return-value-shape pin.
    /// Strengthened vs the prior `let _ = parse_chord(chord)`
    /// shape: the test now asserts BOTH panic-safety AND
    /// return-value shape. A future contributor who
    /// accidentally swaps the `Some(_)/None` return-value sign
    /// in `parse_chord`'s body without introducing a panic
    /// previously slipped through the audit since the loop
    /// body's only assertion was "did not panic". Splitting
    /// `known_chords` from `unknown_prefixes` and asserting
    /// the expected shape on each closes that hole at
    /// unit-test time. `parse_chord` is `fn` (not `pub fn`),
    /// but the descending-mod rule lets cfg(test) read it.
    #[test]
    fn audit_parse_chord_direct_call_never_panics() {
        // The 14 canonical cmdash/config.kdl chords MUST
        // return `Some((mods, key))`. The 4 augmented chords
        // (ctrl/alt/shift/super positive controls) likewise.
        let known_chords = [
            // Canonical 14 (from cmdash/config.kdl).
            "alt-w",
            "alt-q",
            "ctrl-a",
            "M-t",
            "M-w",
            "M-1",
            "M-2",
            "M-3",
            "M-4",
            "M-5",
            "M-6",
            "M-7",
            "M-8",
            "M-9",
            // Augmented positive control.
            "ctrl-x",
            "super-r",
            "shift-tab",
            "ctrl-alt-a",
        ];
        // Real-world un-handled modifiers + degenerate
        // inputs + multi-char unknown tokens. None of them
        // map to BOTH a recognised `parse_chord` arm AND a
        // recognised `KeyToken`, so the function MUST return
        // `None` for each.
        let unknown_prefixes = [
            // Real-world un-handled modifiers.
            "hyper-a", "leader-a", "mod-a", "altgr-a", "fn-a", // Empty / degenerate.
            "", "-", "-a", "--a", "ctrl-", // Multi-char unknown.
            "abc", "f99", "f0", "f-1",
        ];
        // Panic-safety: any panic propagates out of the test
        // process and surfaces as a `cargo test` failure for
        // the entire test binary. There is no `catch_unwind`
        // surrounding these calls so the test is
        // intentionally strict -- a panic = a CI fail = a
        // wire-level alarm that `parse_chord` regressed.
        // The combined shape checks panic-safety +
        // return-value at the same time.
        for chord in known_chords.iter() {
            assert!(
                parse_chord(chord).is_some(),
                "{chord} must return Some((mods, key))"
            );
        }
        for chord in unknown_prefixes.iter() {
            assert!(
                parse_chord(chord).is_none(),
                "{chord:?} must return None (no recognised \
                 prefix-plus-key shape)"
            );
        }
    }

    /// `ratio=abc` (non-parseable float) is rejected at parse
    /// time with `ConfigError::InvalidRatio`. Pins the fix that
    /// wired the previously-dead `InvalidRatio` variant into
    /// `read_split`; before the fix, `unwrap_or(0.5)` silently
    /// produced a 50/50 split with no warning.
    #[test]
    fn parse_invalid_ratio_returns_err() {
        let src = r#"
            layout {
                split axis=horizontal ratio=abc {
                    pane kind=shell label="a"
                    pane kind=shell label="b"
                }
            }
        "#;
        let err = parse(src).expect_err("invalid ratio must error");
        assert!(
            matches!(err, ConfigError::InvalidRatio(ref s) if s == "abc"),
            "expected InvalidRatio(\"abc\"), got: {err:?}"
        );
    }

    /// `preset` with no name argument in the layout tree is
    /// rejected at parse time. Mirrors the `read_named_preset`
    /// check for the top-level `presets` block. Without this,
    /// a bare `preset` node would be silently skipped by the
    /// resolver (nested presets are no-ops), hiding a user
    /// config error.
    #[test]
    fn parse_preset_no_name_returns_err() {
        let src = r#"
            layout {
                preset
            }
        "#;
        let err = parse(src).expect_err("preset with no name must error");
        assert!(
            matches!(err, ConfigError::InvalidAction(_)),
            "expected InvalidAction, got: {err:?}"
        );
    }

    // ============================================================
    // Per-pane shell command parsing tests.
    //
    // Pin the `command` field on `Pane` so a future contributor
    // who accidentally drops the `command` arm from `read_pane`
    // cannot silently regress to `command: None` for all panes.
    // ============================================================

    /// `pane kind=shell command="htop"` round-trips into
    /// `Pane.command = Some("htop")`. Pins the per-pane shell
    /// command parsing from roadmap item 1.3.
    #[test]
    fn parse_pane_command_round_trip() {
        let src = r#"
            layout {
                pane kind=shell label="monitor" command="htop"
            }
        "#;
        let cfg = parse(src).expect("pane with command parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::Pane(p) => {
                assert_eq!(p.label, Some("monitor".to_string()));
                assert_eq!(p.command, Some("htop".to_string()));
            }
            other => panic!("expected Pane, got: {:?}", other),
        }
    }

    /// `pane kind=shell command="htop --delay=5 --color"`
    /// preserves the full command string (whitespace included)
    /// for the binary to split into argv at spawn time.
    #[test]
    fn parse_pane_command_with_args_round_trip() {
        let src = r#"
            layout {
                pane kind=shell label="monitor" command="htop --delay=5 --color"
            }
        "#;
        let cfg = parse(src).expect("pane with command+args parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::Pane(p) => {
                assert_eq!(p.command, Some("htop --delay=5 --color".to_string()));
            }
            other => panic!("expected Pane, got: {:?}", other),
        }
    }

    /// `pane kind=shell` (no `command`) yields `command: None`.
    /// Pins the default fallback path so a future refactor
    /// that accidentally defaults `command` to `Some("")`
    /// instead of `None` is caught immediately.
    #[test]
    fn parse_pane_no_command_yields_none() {
        let src = r#"
            layout {
                pane kind=shell label="default"
            }
        "#;
        let cfg = parse(src).expect("pane without command parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::Pane(p) => {
                assert_eq!(
                    p.command, None,
                    "pane without command= must yield command: None"
                );
            }
            other => panic!("expected Pane, got: {:?}", other),
        }
    }

    /// `pane kind=shell command=""` (empty string) yields
    /// `command: Some("")`. The binary's
    /// `shell_spec_from_command` treats `Some("")` the same
    /// as `None` (falls back to default shell) because
    /// `split_whitespace()` on an empty string yields zero
    /// argv elements. This test pins the parse-time behavior
    /// so the binary can rely on `Some("")` being reachable.
    #[test]
    fn parse_pane_empty_command_yields_some_empty() {
        let src = r#"
            layout {
                pane kind=shell label="empty" command=""
            }
        "#;
        let cfg = parse(src).expect("pane with empty command parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::Pane(p) => {
                assert_eq!(
                    p.command,
                    Some(String::new()),
                    "pane with command=\"\" must yield Some(\"\")"
                );
            }
            other => panic!("expected Pane, got: {:?}", other),
        }
    }

    /// Multiple panes with different commands in a split
    /// layout. Pins that each pane independently carries its
    /// own command override.
    #[test]
    fn parse_split_with_per_pane_commands() {
        let src = r#"
            layout {
                split axis=horizontal ratio=0.5 {
                    pane kind=shell label="editor" command="nvim"
                    pane kind=shell label="monitor" command="htop"
                }
            }
        "#;
        let cfg = parse(src).expect("split with per-pane commands parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::Split { children, .. } => {
                assert_eq!(children.len(), 2);
                match &children[0] {
                    LayoutNode::Pane(p) => {
                        assert_eq!(p.command, Some("nvim".to_string()));
                    }
                    other => panic!("expected Pane, got: {:?}", other),
                }
                match &children[1] {
                    LayoutNode::Pane(p) => {
                        assert_eq!(p.command, Some("htop".to_string()));
                    }
                    other => panic!("expected Pane, got: {:?}", other),
                }
            }
            other => panic!("expected Split, got: {:?}", other),
        }
    }

    // ============================================================
    // §3.8 Pre-scan validator tests.
    // ============================================================

    /// Unclosed brace detected by pre-scan.
    #[test]
    fn pre_scan_unclosed_brace() {
        let hints = pre_scan_kdl("layout { split {");
        assert!(
            hints.iter().any(|h| matches!(h,
                ConfigWarning::SyntaxHint { ref message, .. }
                if message.contains("unclosed")
            )),
            "must detect unclosed brace; got: {hints:?}"
        );
    }

    /// Extra close brace detected by pre-scan.
    #[test]
    fn pre_scan_extra_close_brace() {
        let hints = pre_scan_kdl("layout { pane kind=shell } }");
        assert!(
            hints.iter().any(|h| matches!(h,
                ConfigWarning::SyntaxHint { ref message, .. }
                if message.contains("extra closing brace")
            )),
            "must detect extra close brace; got: {hints:?}"
        );
    }

    /// Missing value after `=` detected by pre-scan.
    #[test]
    fn pre_scan_missing_value_after_equals() {
        let hints = pre_scan_kdl("split axis= {");
        assert!(
            hints.iter().any(|h| matches!(h,
                ConfigWarning::SyntaxHint { ref message, .. }
                if message.contains("missing value after")
            )),
            "must detect missing value after =; got: {hints:?}"
        );
    }

    /// Empty block detected by pre-scan.
    #[test]
    fn pre_scan_empty_block() {
        let hints = pre_scan_kdl("layout { } ");
        assert!(
            hints.iter().any(|h| matches!(h,
                ConfigWarning::SyntaxHint { ref message, .. }
                if message.contains("empty block")
            )),
            "must detect empty block; got: {hints:?}"
        );
    }

    /// Trailing comma detected by pre-scan.
    #[test]
    fn pre_scan_trailing_comma() {
        let src = "bind \"alt-w\" action=\"pane.close\",\n";
        let hints = pre_scan_kdl(src);
        assert!(
            hints.iter().any(|h| matches!(h,
                ConfigWarning::SyntaxHint { ref message, .. }
                if message.contains("trailing comma")
            )),
            "must detect trailing comma; got: {hints:?}"
        );
    }

    /// Valid KDL produces no hints.
    #[test]
    fn pre_scan_valid_kdl_no_hints() {
        let src = r#"
            layout {
                split axis=horizontal ratio=0.5 {
                    pane kind=shell label="a"
                    pane kind=shell label="b"
                }
            }
        "#;
        let hints = pre_scan_kdl(src);
        assert!(
            hints.is_empty(),
            "valid KDL must produce no hints; got: {hints:?}"
        );
    }

    /// Empty source produces no hints.
    #[test]
    fn pre_scan_empty_source_no_hints() {
        let hints = pre_scan_kdl("");
        assert!(hints.is_empty());
    }

    /// Line numbers are correct in hints.
    #[test]
    fn pre_scan_hint_reports_correct_line() {
        let src2 = "line1\nkey= {\n";
        let hints = pre_scan_kdl(src2);
        assert!(
            hints
                .iter()
                .any(|h| matches!(h, ConfigWarning::SyntaxHint { line: 2, .. })),
            "hints should report line 2 for issues on line 2; got: {hints:?}"
        );
    }

    /// Strings containing braces do not false-positive.
    #[test]
    fn pre_scan_braces_in_strings_ignored() {
        let src = r#"layout { pane kind=shell label="{foo}" }"#;
        let hints = pre_scan_kdl(src);
        let brace_hints: Vec<_> = hints
            .iter()
            .filter(|h| {
                matches!(h,
                    ConfigWarning::SyntaxHint { ref message, .. }
                    if message.contains("brace") || message.contains("unclosed")
                )
            })
            .collect();
        assert!(
            brace_hints.is_empty(),
            "braces inside strings must not trigger hints; got: {brace_hints:?}"
        );
    }

    /// `ConfigWarning::SyntaxHint` Display impl includes line number.
    #[test]
    fn syntax_hint_display_includes_line() {
        let hint = ConfigWarning::SyntaxHint {
            line: 42,
            message: "test message".into(),
        };
        let s = format!("{hint}");
        assert!(
            s.contains("42"),
            "Display must include line number; got: {s}"
        );
        assert!(
            s.contains("test message"),
            "Display must include message; got: {s}"
        );
    }

    /// A block containing only a `//` line comment is NOT
    /// flagged as empty. Pins the fix for the false-positive
    /// where `node { // placeholder }` was incorrectly
    /// detected as an empty block.
    #[test]
    fn pre_scan_commented_block_not_flagged_as_empty() {
        let src = "node { // placeholder
}
";
        let hints = pre_scan_kdl(src);
        let empty_hints: Vec<_> = hints
            .iter()
            .filter(|h| {
                matches!(h,
                    ConfigWarning::SyntaxHint { ref message, .. }
                    if message.contains("empty block")
                )
            })
            .collect();
        assert!(
            empty_hints.is_empty(),
            "commented block must not be flagged empty; got: {empty_hints:?}"
        );
    }

    /// A block containing only a `/* */` block comment is NOT
    /// flagged as empty. Pins the fix that added block-comment
    /// peek support to the empty block check.
    #[test]
    fn pre_scan_block_comment_not_flagged_as_empty() {
        let src = "node { /* placeholder */ }
";
        let hints = pre_scan_kdl(src);
        let empty_hints: Vec<_> = hints
            .iter()
            .filter(|h| {
                matches!(h,
                    ConfigWarning::SyntaxHint { ref message, .. }
                    if message.contains("empty block")
                )
            })
            .collect();
        assert!(
            empty_hints.is_empty(),
            "block-comment block must not be flagged empty; got: {empty_hints:?}"
        );
    }

    /// A block containing only a `/* */` block comment
    /// spanning multiple lines is NOT flagged as empty.
    #[test]
    fn pre_scan_multiline_block_comment_not_flagged_as_empty() {
        let src = "node { /* line one
   line two */
}
";
        let hints = pre_scan_kdl(src);
        let empty_hints: Vec<_> = hints
            .iter()
            .filter(|h| {
                matches!(h,
                    ConfigWarning::SyntaxHint { ref message, .. }
                    if message.contains("empty block")
                )
            })
            .collect();
        assert!(
            empty_hints.is_empty(),
            "multiline block-comment block must not be flagged empty; got: {empty_hints:?}"
        );
    }

    /// Block comment containing `{` does not inflate brace
    /// depth. Without `in_block_comment` tracking in the main
    /// scan body, the unbalanced `{` inside `/* ... */` would
    /// increment `brace_depth` and produce a false "unclosed
    /// brace" hint.
    #[test]
    fn pre_scan_block_comment_brace_not_tracked() {
        // Unbalanced `{` inside block comment: without tracking,
        // brace_depth would reach 2 and never return to 0.
        let src = "layout { /* { */ pane kind=shell }\n";
        let hints = pre_scan_kdl(src);
        let brace_hints: Vec<_> = hints
            .iter()
            .filter(|h| {
                matches!(h,
                    ConfigWarning::SyntaxHint { ref message, .. }
                    if message.contains("brace") || message.contains("unclosed")
                )
            })
            .collect();
        assert!(
            brace_hints.is_empty(),
            "braces inside block comments must not trigger hints; got: {brace_hints:?}"
        );
    }

    /// Block comment containing `}` does not deflate brace
    /// depth. The symmetric case to the `{` test above.
    #[test]
    fn pre_scan_block_comment_close_brace_not_tracked() {
        let src = "layout { pane kind=shell /* } */ }\n";
        let hints = pre_scan_kdl(src);
        let brace_hints: Vec<_> = hints
            .iter()
            .filter(|h| {
                matches!(h,
                    ConfigWarning::SyntaxHint { ref message, .. }
                    if message.contains("brace") || message.contains("unclosed")
                )
            })
            .collect();
        assert!(
            brace_hints.is_empty(),
            "close brace inside block comment must not trigger hints; got: {brace_hints:?}"
        );
    }
    /// Block comment containing `=` does not trigger the
    /// "missing value after `=`" check. Without block comment
    /// tracking, `=` inside `/* ... */` followed by `}` would
    /// produce a false hint because the `=` check looks at
    /// the next non-whitespace char after `=` (which would be
    /// the `}` from `*/` if block comment chars were processed).
    #[test]
    fn pre_scan_block_comment_equals_not_tracked() {
        // `=` INSIDE block comment: without tracking, the `=`
        // check would see `}` as the next non-whitespace char
        // (from `*/}`) and flag "missing value after `=`".
        let src = "split /* axis=} */horizontal {\n    pane kind=shell\n    pane kind=shell\n}\n";
        let hints = pre_scan_kdl(src);
        let eq_hints: Vec<_> = hints
            .iter()
            .filter(|h| {
                matches!(h,
                    ConfigWarning::SyntaxHint { ref message, .. }
                    if message.contains("missing value after")
                )
            })
            .collect();
        assert!(
            eq_hints.is_empty(),
            "= inside block comment must not trigger hint; got: {eq_hints:?}"
        );
    }

    /// Unclosed `/*` at EOF does not produce spurious hints
    /// (the block comment consumes the rest of the input
    /// without triggering brace or `=` false positives).
    #[test]
    fn pre_scan_unclosed_block_comment_at_eof_no_spurious_hints() {
        let src = "layout { pane kind=shell /* unclosed\n";
        let hints = pre_scan_kdl(src);
        // Should only get the unclosed brace hint (depth=1),
        // NOT a false "missing value" or "extra brace".
        let false_positives: Vec<_> = hints
            .iter()
            .filter(|h| {
                matches!(h,
                    ConfigWarning::SyntaxHint { ref message, .. }
                    if message.contains("missing value") || message.contains("extra closing")
                )
            })
            .collect();
        assert!(
            false_positives.is_empty(),
            "unclosed block comment must not produce false-positive hints; got: {false_positives:?}"
        );
    }

    // ============================================================
    // §3.9 extract_line_col_from_msg and format_errors_with_context tests.
    // ============================================================

    /// KDL syntax error with span "3:5-3:10" extracts (3, 5).
    #[test]
    fn extract_line_col_from_kdl_span() {
        let msg = "cmdash config: KDL syntax error:
3:5-3:10 expected value, got `}`";
        let result = extract_line_col_from_msg(msg);
        assert_eq!(result, Some((3, 5)));
    }

    /// Single-position span "2:0" extracts (2, 0).
    #[test]
    fn extract_line_col_from_single_pos() {
        let msg = "error at 2:0";
        let result = extract_line_col_from_msg(msg);
        assert_eq!(result, Some((2, 0)));
    }

    /// No span info returns None.
    #[test]
    fn extract_line_col_no_span_returns_none() {
        let msg = "duplicate `layout` block";
        let result = extract_line_col_from_msg(msg);
        assert_eq!(result, None);
    }

    /// Empty string returns None.
    #[test]
    fn extract_line_col_empty_returns_none() {
        assert_eq!(extract_line_col_from_msg(""), None);
    }

    /// `format_errors_with_context` shows correct source line
    /// for a KDL error with positional info.
    #[test]
    fn format_errors_shows_correct_source_line() {
        let src = "line one
line two
line three
";
        let err = ConfigError::Kdl("1:5-1:5 expected value".into());
        let formatted = format_errors_with_context(&[err], src, None);
        // Should contain the error message, line number, and source line.
        assert!(
            formatted.contains("line one"),
            "must show the source line; got: {formatted}"
        );
        assert!(
            formatted.contains("1:"),
            "must show the line number; got: {formatted}"
        );
        assert!(
            formatted.contains("^"),
            "must include a caret; got: {formatted}"
        );
    }

    /// `format_errors_with_context` skips context for non-KDL
    /// errors that have no positional info.
    #[test]
    fn format_errors_no_context_for_semantic_error() {
        let src = "line one
line two
";
        let err = ConfigError::DuplicateLayout;
        let formatted = format_errors_with_context(&[err], src, None);
        assert!(
            formatted.contains("duplicate `layout` block"),
            "must show the error message; got: {formatted}"
        );
        // Should NOT show a source line or caret.
        assert!(
            !formatted.contains("line one"),
            "must not show source line for semantic error; got: {formatted}"
        );
    }

    /// `format_errors_with_context` prepends `file_label`.
    #[test]
    fn format_errors_prepends_file_label() {
        let src = "bad
";
        let err = ConfigError::Kdl("1:0-1:0 error".into());
        let formatted = format_errors_with_context(&[err], src, Some("config.kdl"));
        assert!(
            formatted.starts_with("config.kdl: "),
            "must prepend file label; got: {formatted}"
        );
    }

    /// Multiple errors are separated by blank lines and each
    /// gets its own source context when available.
    #[test]
    fn format_errors_multiple_errors_each_get_context() {
        let src = "line one
line two
line three
";
        let errors = vec![
            ConfigError::Kdl("1:0-1:0 syntax error".into()),
            ConfigError::Kdl("3:2-3:2 another error".into()),
        ];
        let formatted = format_errors_with_context(&errors, src, None);
        // Both source lines must appear.
        assert!(
            formatted.contains("line one"),
            "must show first source line; got: {formatted}"
        );
        assert!(
            formatted.contains("line three"),
            "must show third source line; got: {formatted}"
        );
        // Errors separated by blank line.
        let parts: Vec<&str> = formatted
            .split(
                "

",
            )
            .collect();
        assert!(
            parts.len() >= 2,
            "must have at least 2 error blocks separated by blank line; got: {formatted}"
        );
    }

    /// Errors on different lines show their respective source
    /// lines, not always line 1.
    #[test]
    fn format_errors_different_lines_show_correct_source() {
        let src = "aaa
bbb
ccc
";
        let errors = vec![
            ConfigError::Kdl("2:1-2:1 middle error".into()),
            ConfigError::Kdl("3:0-3:0 last error".into()),
        ];
        let formatted = format_errors_with_context(&errors, src, None);
        assert!(
            formatted.contains("bbb"),
            "first error should show line 2 source; got: {formatted}"
        );
        assert!(
            formatted.contains("ccc"),
            "second error should show line 3 source; got: {formatted}"
        );
        // Must NOT show line 1.
        assert!(
            !formatted.contains("aaa"),
            "must not show unrelated line 1; got: {formatted}"
        );
    }

    /// Mix of KDL errors (with spans) and semantic errors
    /// (without spans) — KDL ones get context, semantic ones don't.
    #[test]
    fn format_errors_mixed_kdl_and_semantic() {
        let src = "line one
line two
";
        let errors = vec![
            ConfigError::Kdl("1:3-1:3 bad token".into()),
            ConfigError::DuplicateLayout,
            ConfigError::Kdl("2:0-2:0 another".into()),
        ];
        let formatted = format_errors_with_context(&errors, src, None);
        // KDL errors get source lines.
        assert!(
            formatted.contains("line one"),
            "first KDL error must show its source line; got: {formatted}"
        );
        assert!(
            formatted.contains("line two"),
            "third error must show its source line; got: {formatted}"
        );
        // Semantic error in between does not show source.
        let dup_idx = formatted.find("duplicate `layout` block").unwrap();
        let near = &formatted[dup_idx..dup_idx + 80];
        assert!(
            !near.contains("line one") && !near.contains("line two"),
            "semantic error must not show source line; got: {near}"
        );
    }

    /// An empty error list produces an empty string with no
    /// trailing newline or blank lines.
    #[test]
    fn format_errors_empty_list_returns_empty() {
        let src = "valid config\n";
        let formatted = format_errors_with_context(&[], src, None);
        assert!(
            formatted.is_empty(),
            "empty error list must produce empty string; got: {formatted:?}"
        );
    }

    // ============================================================
    // § Widget pane kind parsing tests.
    //
    // Pin the `kind=widget ref-name="..."` round-trip so a future
    // contributor who drops the widget arm from `read_pane` cannot
    // silently regress to `PaneKind::Shell` for widget panes.
    // ============================================================

    /// `pane kind=widget ref-name="widget-clock"` round-trips into
    /// `PaneKind::Widget { ref_name: "widget-clock" }`.
    #[test]
    fn parse_pane_widget_kind_round_trip() {
        let src = r#"
            layout {
                pane kind=widget ref-name="widget-clock" label="clock"
            }
        "#;
        let cfg = parse(src).expect("widget pane parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::Pane(p) => {
                assert_eq!(p.label, Some("clock".to_string()));
                assert_eq!(p.command, None);
                match &p.kind {
                    PaneKind::Widget { ref_name } => {
                        assert_eq!(ref_name, "widget-clock");
                    }
                    other => panic!("expected Widget kind, got: {:?}", other),
                }
            }
            other => panic!("expected Pane, got: {:?}", other),
        }
    }

    /// `pane kind=widget` without `ref-name` is rejected.
    #[test]
    fn parse_pane_widget_kind_missing_ref_name_returns_err() {
        let src = r#"
            layout {
                pane kind=widget label="broken"
            }
        "#;
        let err = parse(src).expect_err("widget without ref-name must error");
        assert!(
            matches!(err, ConfigError::InvalidPaneKind(ref s) if s.contains("ref-name")),
            "expected InvalidPaneKind mentioning ref-name; got: {err:?}"
        );
    }

    /// Widget pane in a split layout round-trips correctly.
    #[test]
    fn parse_split_with_widget_and_shell_panes() {
        let src = r#"
            layout {
                split axis=horizontal ratio=0.3 {
                    pane kind=widget ref-name="widget-clock" label="clock"
                    pane kind=shell label="shell"
                }
            }
        "#;
        let cfg = parse(src).expect("split with widget parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::Split { children, .. } => {
                assert_eq!(children.len(), 2);
                match &children[0] {
                    LayoutNode::Pane(p) => match &p.kind {
                        PaneKind::Widget { ref_name } => {
                            assert_eq!(ref_name, "widget-clock");
                        }
                        other => panic!("expected Widget kind, got: {:?}", other),
                    },
                    other => panic!("expected Pane, got: {:?}", other),
                }
                match &children[1] {
                    LayoutNode::Pane(p) => {
                        assert!(matches!(p.kind, PaneKind::Shell));
                    }
                    other => panic!("expected Pane, got: {:?}", other),
                }
            }
            other => panic!("expected Split, got: {:?}", other),
        }
    }

    // ============================================================
    // § Script pane kind parsing tests.
    //
    // Pin the `kind=script command="..."` round-trip so a future
    // contributor who drops the script arm from `read_pane` cannot
    // silently regress to `PaneKind::Shell` for script panes.
    // ============================================================

    /// `pane kind=script command="python3 foo.py"` round-trips into
    /// `PaneKind::Script` with `Pane.command = Some("python3 foo.py")`.
    #[test]
    fn parse_pane_script_kind_round_trip() {
        let src = r#"
            layout {
                pane kind=script command="python3 foo.py" label="my-script"
            }
        "#;
        let cfg = parse(src).expect("script pane parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::Pane(p) => {
                assert_eq!(p.label, Some("my-script".to_string()));
                assert_eq!(p.command, Some("python3 foo.py".to_string()));
                assert!(matches!(p.kind, PaneKind::Script));
            }
            other => panic!("expected Pane, got: {:?}", other),
        }
    }

    /// `pane kind=script` without `command` is rejected.
    #[test]
    fn parse_pane_script_kind_missing_command_returns_err() {
        let src = r#"
            layout {
                pane kind=script label="broken"
            }
        "#;
        let err = parse(src).expect_err("script without command must error");
        assert!(
            matches!(err, ConfigError::InvalidPaneKind(ref s) if s.contains("command")),
            "expected InvalidPaneKind mentioning command; got: {err:?}"
        );
    }

    /// Script pane in a split layout with shell pane round-trips.
    #[test]
    fn parse_split_with_script_and_shell_panes() {
        let src = r#"
            layout {
                split axis=horizontal ratio=0.3 {
                    pane kind=script command="python3 widget.py" label="widget"
                    pane kind=shell label="shell"
                }
            }
        "#;
        let cfg = parse(src).expect("split with script parses");
        let layout = cfg.layout.expect("Config.layout populated");
        match layout {
            LayoutNode::Split { children, .. } => {
                assert_eq!(children.len(), 2);
                match &children[0] {
                    LayoutNode::Pane(p) => {
                        assert!(matches!(p.kind, PaneKind::Script));
                        assert_eq!(p.command, Some("python3 widget.py".to_string()));
                    }
                    other => panic!("expected Pane, got: {:?}", other),
                }
                match &children[1] {
                    LayoutNode::Pane(p) => {
                        assert!(matches!(p.kind, PaneKind::Shell));
                    }
                    other => panic!("expected Pane, got: {:?}", other),
                }
            }
            other => panic!("expected Split, got: {:?}", other),
        }
    }
}
