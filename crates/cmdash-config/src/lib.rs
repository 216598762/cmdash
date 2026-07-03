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
//!   pane kind=shell [label="..."]
//!   preset "name" | preset name="..."
//! }
//! keybinds {
//!   bind "<chord>" action="<action>"
//! }
//! ```

use std::collections::BTreeMap;

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};

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
}

/// First-release flavor of pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaneKind {
    #[default]
    Shell,
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
    /// currently-focused ZStack (wrap-around from last → first).
    /// No-op if the focused pane is not a member of a ZStack.
    /// Phase 4 carry-forward.
    PaneStackCycle,
    /// `pane.stack.down` - directional within-ZStack Down:
    /// focus the next member of the focused ZStack in
    /// declaration order; if the focused pane is the last (top)
    /// member, hand focus off to the topmost pane geometrically
    /// below the ZStack via [`adjacent_pane`]. No-op if the
    /// focused pane is not a ZStack member, or if the focused
    /// ZStack member has no geometrically-below neighbour.
    /// Phase 4 carry-forward.
    PaneStackDown,
    /// `pane.stack.up` - mirror of `pane.stack.down`: focus
    /// the **previous** member of the focused ZStack in
    /// declaration order; if the focused pane is the first
    /// (bottom) member, hand focus off to the topmost pane
    /// geometrically above the ZStack via [`adjacent_pane`].
    /// No-op if the focused pane is not a ZStack member, or
    /// if the focused ZStack member has no geometrically-above
    /// neighbour. Phase 4 carry-forward.
    PaneStackUp,
    /// `pane.preset.<name>` - focus a named preset.
    PanePreset(String),
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
}

/// Parse a cmdash configuration from raw KDL source.
pub fn parse(source: &str) -> Result<Config, ConfigError> {
    let doc: KdlDocument = source
        .parse()
        .map_err(|e: kdl::KdlError| ConfigError::Kdl(e.to_string()))?;        let mut cfg = Config::default();
        for n in doc.nodes() {
            let name = n.name().value();
            match name {
                "layout" => {
                    if cfg.layout.is_some() {
                        return Err(ConfigError::DuplicateLayout);
                    }
                    // The outer `layout { ... }` is a single-node container.
                    // Descend into its first (and only) child, which is the
                    // actual root LayoutNode.
                    let children = n.children().ok_or_else(|| {
                        ConfigError::UnknownLayoutNode("layout block must contain a LayoutNode".into())
                    })?;
                    let kids = children.nodes();
                    let first = kids.first().ok_or_else(|| {
                        ConfigError::UnknownLayoutNode("layout block must contain a LayoutNode".into())
                    })?;
                    if kids.len() > 1 {
                        return Err(ConfigError::UnknownLayoutNode(
                            "layout block may contain exactly one LayoutNode".into(),
                        ));
                    }
                    cfg.layout = Some(read_layout(first)?);
                }
                "keybinds" => {
                    if let Some(c) = n.children() {
                        for k in c.nodes() {
                            if k.name().value() != "bind" {
                                return Err(ConfigError::UnexpectedKindbindChild(
                                    k.name().value().to_string(),
                                ));
                            }
                            cfg.keybinds.push(read_keybind(k)?);
                        }
                    }
                }
                "presets" => {
                    if !cfg.presets.is_empty() {
                        return Err(ConfigError::DuplicatePresets);
                    }
                    let c = n.children().ok_or_else(|| {
                        ConfigError::EmptyChildren("presets")
                    })?;
                    for k in c.nodes() {
                        if k.name().value() != "preset" {
                            return Err(ConfigError::UnexpectedPresetsChild(
                                k.name().value().to_string(),
                            ));
                        }
                        let (name, body) = read_named_preset(k)?;
                        if cfg.presets.contains_key(&name) {
                            return Err(ConfigError::DuplicatePreset(name));
                        }
                        cfg.presets.insert(name, body);
                    }
                }
                other => return Err(ConfigError::UnknownTopLevel(other.into())),
            }
        }
        Ok(cfg)
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
                ratio = raw.parse().unwrap_or(0.5);
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
    let mut kind = PaneKind::default();
    let mut label: Option<String> = None;
    for entry in n.entries() {
        let key = entry.name().map(|id| id.value());
        let raw = entry_to_string(entry);
        match (key, raw.as_str()) {
            (Some("kind"), "shell") => kind = PaneKind::Shell,
            (Some("kind"), _) if !raw.is_empty() => {
                return Err(ConfigError::InvalidPaneKind(raw));
            }
            (Some("label"), _) => label = Some(raw),
            _ => {}
        }
    }
    Ok(LayoutNode::Pane(Pane { kind, label }))
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
        .ok_or_else(|| ConfigError::UnknownLayoutNode(
            "preset block must contain a LayoutNode body".into(),
        ))?
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
    let action =
        parse_action(&action_str).ok_or_else(|| ConfigError::InvalidAction(action_str.clone()))?;
    Ok(Keybind { mods, key, action })
}

fn parse_chord(s: &str) -> Option<(Modifiers, KeyToken)> {
    let mut mods = Modifiers::default();
    let mut key_part: Option<&str> = None;
    for part in s.split('-') {
        match part {
            "ctrl" | "control" | "ctl" => mods.ctrl = true,
            "shift" => mods.shift = true,
            "alt" | "meta" => mods.alt = true,
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
        "pane.preset" => Some(KeyAction::PanePreset(String::new())),
        other => other
            .strip_prefix("pane.preset.")
            .map(|rest| KeyAction::PanePreset(rest.to_string())),
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
            LayoutNode::Split { axis, ratio, children } => {
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

    /// Negative baseline: an unknown inner LayoutNode name
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
                        assert_eq!(
                            pane_label(&panes[0]),
                            Some("overlay".to_string())
                        );
                        assert_eq!(
                            pane_label(&panes[1]),
                            Some("overlay_below".to_string())
                        );
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
}
