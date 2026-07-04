//! cmdash-layout: layout tree engine resolving a parsed
//! [`cmdash_config::LayoutNode`] tree into a flat list of cell-grid
//! rectangles, one per leaf pane, with deterministic pane IDs.
//!
//! The crate outputs **cell-grid** rectangles (not pixels); the
//! conductor / dashcompositor bridge turns cell rects into pixel
//! rects for downstream layer placement. This follows AGENTS.md
//! ("Bounds = pane rect in cells -> pixels").
//!
//! ## Layout semantics
//!
//! - `LayoutNode::Split { axis, ratio, children }`: divides the
//!   area along the given axis; child 0 gets `ratio` percent of the
//!   dimension; the remainder goes to child 1. Each child is
//!   resolved recursively. v1: exactly two children per split.
//! - `LayoutNode::Stack { panes }`: divides the stack's area into
//!   `N` equal-height vertical strips, top to bottom. Child `i`
//!   gets `i * (h/N)` as its `y` origin; the last child absorbs
//!   any remainder rows. AGENTS.md calls `stack { ... }` a tabbed
//!   viewer; this crate computes the geometry. The visual tab bar
//!   (one row at the top, say) is the renderer's concern.
//! - `LayoutNode::Pane(_)`: emits one `ComputedPane` with the
//!   current `area` rect.
//! - `LayoutNode::Preset { name }`: skipped during resolution
//!   if nested (presets are named saved layouts, not renderable
//!   leaves). If the *root* is a Preset we raise
//!   [`LayoutError::PresetAtRoot`].
//!
//! ## PaneId stability
//!
//! [`PaneId`] is derived from the leaf's position in the static
//! [`cmdash_config::LayoutNode`] tree via its pre-order leaf index
//! plus a compact child-index path. Two `compute()` calls over the
//! same tree produce identical [`PaneId`]s, so consecutive frames
//! keep LayerIds bound across resizes.
//!
//! Live runtime layout mutation (open/close pane at runtime) is
//! deliberately a v2 concern. In v1 each frame starts from the
//! static tree read at startup / config reload, so resize-only
//! reflows preserve every pane's [`PaneId`].
//!
//! ## Errors
//!
//! All layout errors go through [`LayoutError`]. Pure resolution
//! never panics.

use cmdash_config::{LayoutNode, PaneKind, Ratio, SplitAxis};
use thiserror::Error;

/// Maximum supported tree depth. Eight layers covers any reasonable
/// config without pathological buffer usage; deeper trees raise
/// [`LayoutError::TreeTooDeep`].
pub const MAX_TREE_DEPTH: usize = 8;

/// A cell-grid rectangle.
///
/// Coordinates are zero-based with `(x, y)` as top-left and
/// `(w, h)` as cell extent. Right/bottom edges are exclusive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    /// A zero-area Rect.
    pub const ZERO: Rect = Self {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };

    /// Right edge (`x + w`, saturating).
    pub fn right(&self) -> u16 {
        self.x.saturating_add(self.w)
    }

    /// Bottom edge (`y + h`, saturating).
    pub fn bottom(&self) -> u16 {
        self.y.saturating_add(self.h)
    }
}

/// Stable identifier for a leaf pane, derived from the leaf's
/// position in the static [`cmdash_config::LayoutNode`] tree.
///
/// `pre_order` counts every leaf encountered during a DFS
/// pre-order traversal starting at zero. `path` mirrors the child
/// indices from root to the leaf (truncated to [`MAX_TREE_DEPTH`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneId {
    pre_order: u32,
    path: [u16; MAX_TREE_DEPTH],
    path_len: u8,
}

impl PaneId {
    /// Zero-based pre-order leaf index. Stable for the same tree.
    pub fn pre_order(&self) -> u32 {
        self.pre_order
    }

    /// Path of child indices from root to the leaf.
    pub fn path(&self) -> &[u16] {
        &self.path[..self.path_len as usize]
    }

    /// Full underlying array (truncated to `path_len`).
    /// Exposed so layout-mutation helpers can clone [`PaneId`]
    /// without re-deriving from a fresh resolver call.
    pub const fn path_arr(&self) -> &[u16; MAX_TREE_DEPTH] {
        &self.path
    }

    /// Truncated length of `[Self::path]`.
    pub const fn path_len(&self) -> u8 {
        self.path_len
    }
}

/// One leaf pane resolved to a position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputedPane {
    /// Stable id for this leaf - see [`PaneId`].
    pub id: PaneId,
    /// Cell-grid rectangle for this pane. The pane owns this entire
    /// rect. For a [`LayoutNode::Stack`] z-stack, multiple panes
    /// share the same rect.
    pub rect: Rect,
    /// Pane flavor (only [`PaneKind::Shell`] in v1).
    pub kind: PaneKind,
    /// Optional user-facing label.
    pub label: Option<String>,
}

/// Layout-resolution errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    /// The given `Rect` had zero width or zero height.
    #[error("zero-area Rect: {w}x{h} at ({x},{y})")]
    ZeroArea { x: u16, y: u16, w: u16, h: u16 },
    /// A `Split` or `Stack` had no children.
    #[error("layout node `{0}` has no children")]
    EmptyChildren(&'static str),
    /// A `Split` had a number of children other than 2.
    #[error("split has {got} children; v1 supports exactly 2")]
    SplitChildCount { got: usize },
    /// The root LayoutNode was a Preset; presets are saved named
    /// layouts, not directly renderable.
    #[error(
        "root LayoutNode is a Preset; presets are named saved layouts, not directly renderable"
    )]
    PresetAtRoot,
    /// Tree depth exceeded [`MAX_TREE_DEPTH`].
    #[error("layout tree too deep ({0} levels); max 8")]
    TreeTooDeep(usize),
}

/// Direction enum for [`adjacent_pane`]. Lives in
/// `cmdash-layout` because the algorithm is layout-side; the
/// binary callsite wraps `KeyAction::PaneFocus{Up,Down,…}` into
/// this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

    /// Split `area` along `axis` at `ratio` percent of the dimension.
/// Child 0 gets `ratio%`; child 1 gets the remainder.

///
/// # Axis semantics
///
/// Naming is a frequently-stepped trapdoor. This crate's
/// `SplitAxis` enum is named after the **line** the split draws,
/// not the dimension along which the children stack:
///
/// - `SplitAxis::Horizontal` is a **column split**: the split
///   line is horizontal (across the screen), so children stack
///   side-by-side along the x-axis (left<->right). Child 0
///   occupies the left `ratio%` column strip at full height;
///   child 1 occupies the right remainder.
/// - `SplitAxis::Vertical` is a **row split**: the split line is
///   vertical (top-to-bottom across the screen), so children
///   stack top-to-bottom along the y-axis (top<->bottom). Child
///   0 occupies the top `ratio%` row strip at full width; child
///   1 occupies the bottom remainder.
///
/// The rect math is pinned by the unit tests
/// `split_rect_horizontal_60` (column math: same `y`, different
/// `x`) and `split_rect_vertical_30` (row math: same `x`,
/// different `y`); see those for the canonical examples. Phase 4
/// ZStack runtime added these clarifications after a test
/// fixture used `axis=horizontal` while asserting row-stacked
/// neighbours -- which produced silently dead test code masked
/// by a missing `#[test]` attribute. Future contributors should
/// treat those two tests as the load-bearing ground truth.
fn split_rect(area: Rect, axis: SplitAxis, ratio: Ratio) -> (Rect, Rect) {
    match axis {
        SplitAxis::Horizontal => {
            let w_left = ((area.w as u32) * (ratio.0 as u32) / 100) as u16;
            let w_right = area.w.saturating_sub(w_left);
            (
                Rect {
                    x: area.x,
                    y: area.y,
                    w: w_left,
                    h: area.h,
                },
                Rect {
                    x: area.x.saturating_add(w_left),
                    y: area.y,
                    w: w_right,
                    h: area.h,
                },
            )
        }
        SplitAxis::Vertical => {
            let h_top = ((area.h as u32) * (ratio.0 as u32) / 100) as u16;
            let h_bot = area.h.saturating_sub(h_top);
            (
                Rect {
                    x: area.x,
                    y: area.y,
                    w: area.w,
                    h: h_top,
                },
                Rect {
                    x: area.x,
                    y: area.y.saturating_add(h_top),
                    w: area.w,
                    h: h_bot,
                },
            )
        }
    }
}

/// All leaf panes resolved for one tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputedLayout {
    /// Leaf panes in pre-order.
    pub panes: Vec<ComputedPane>,
    /// The cell-grid area the layout was computed for.
    pub total: Rect,
}

impl ComputedLayout {
    pub fn compute(root: &LayoutNode, area: Rect) -> Result<Self, LayoutError> {
        if area.w == 0 || area.h == 0 {
            return Err(LayoutError::ZeroArea {
                x: area.x,
                y: area.y,
                w: area.w,
                h: area.h,
            });
        }
        if matches!(root, LayoutNode::Preset { .. }) {
            return Err(LayoutError::PresetAtRoot);
        }
        let mut next_preorder: u32 = 0;
        let mut panes: Vec<ComputedPane> = Vec::new();
        let mut path: [u16; MAX_TREE_DEPTH] = [0; MAX_TREE_DEPTH];
        path[0] = 0;
        resolve_node(root, area, &mut path, 1, &mut next_preorder, &mut panes)?;
        Ok(ComputedLayout { panes, total: area })
    }
}

fn resolve_node(
    n: &LayoutNode,
    area: Rect,
    path: &mut [u16; MAX_TREE_DEPTH],
    path_len: u8,
    next_preorder: &mut u32,
    out: &mut Vec<ComputedPane>,
) -> Result<(), LayoutError> {
    match n {
        LayoutNode::Split {
            axis,
            ratio,
            children,
        } => {
            if children.is_empty() {
                return Err(LayoutError::EmptyChildren("split"));
            }
            if children.len() != 2 {
                return Err(LayoutError::SplitChildCount {
                    got: children.len(),
                });
            }
            let (left, right) = split_rect(area, *axis, *ratio);
            for (i, child) in children.iter().enumerate() {
                if path_len as usize >= MAX_TREE_DEPTH {
                    return Err(LayoutError::TreeTooDeep(path_len as usize + 1));
                }
                let child_area = if i == 0 { left } else { right };
                path[path_len as usize] = i as u16;
                resolve_node(child, child_area, path, path_len + 1, next_preorder, out)?;
            }
        }
        LayoutNode::Stack { panes } => {
            if panes.is_empty() {
                return Err(LayoutError::EmptyChildren("stack"));
            }
            // Equal-height vertical strips: child `i` gets a slice
            // of the parent's height. `base_h = area.h / N`; any
            // remainder rows are absorbed by the last child so the
            // slices tile the area exactly.
            let n = panes.len() as u16;
            let base_h = area.h / n;
            let rem = area.h.saturating_sub(base_h.saturating_mul(n));
            for (i, child) in panes.iter().enumerate() {
                if path_len as usize >= MAX_TREE_DEPTH {
                    return Err(LayoutError::TreeTooDeep(path_len as usize + 1));
                }
                path[path_len as usize] = i as u16;
                let child_y = area.y.saturating_add(base_h.saturating_mul(i as u16));
                let child_h = if (i as u16).saturating_add(1) == n {
                    base_h.saturating_add(rem)
                } else {
                    base_h
                };
                let child_area = Rect {
                    x: area.x,
                    y: child_y,
                    w: area.w,
                    h: child_h,
                };
                resolve_node(child, child_area, path, path_len + 1, next_preorder, out)?;
            }
        }
        LayoutNode::ZStack { panes } => {
            // Z-stack overlay: every member shares the parent's
            // `area` verbatim. The Hard rule (one `LayerId` per
            // pane instance) is preserved — each member still
            // emits its own `ComputedPane` with its own `PaneId`,
            // so reconcile_runners and the binary happy path
            // continue to treat them as distinct panes. Z-order
            // is determined by resolver pre_order (later members
            // render on top of earlier ones).
            if panes.is_empty() {
                return Err(LayoutError::EmptyChildren("zstack"));
            }
            for (i, child) in panes.iter().enumerate() {
                if path_len as usize >= MAX_TREE_DEPTH {
                    return Err(LayoutError::TreeTooDeep(path_len as usize + 1));
                }
                path[path_len as usize] = i as u16;
                // Same area for every peer — no slicing.
                resolve_node(child, area, path, path_len + 1, next_preorder, out)?;
            }
        }
        LayoutNode::Pane(p) => {
            let id = PaneId {
                pre_order: *next_preorder,
                path: *path,
                path_len,
            };
            *next_preorder += 1;
            out.push(ComputedPane {
                id,
                rect: area,
                kind: p.kind,
                label: p.label.clone(),
            });
        }
        LayoutNode::Preset { .. } => {
            // Nested Preset: skip silently. The cmdash binary's
            // conductor iterates presets for the picker UI via
            // cmdash-config's parsed map of named layout bodies.
        }
    }
    Ok(())
}

/// Replace the leaf at `path[..]` in `root` with a [`LayoutNode::Split`]
/// whose children are `[original_leaf_clone, new_pane]`. The original
/// leaf becomes child 0; the new pane is child 1.
///
/// Pre-order invariance: the original leaf's [`PaneId`] (its
/// `pre_order` index) is unchanged because the layout resolver's
/// DFS pre-order traversal still enumerates child 0 first. The
/// new pane takes the next available pre-order slot (which may
/// shift downstream leaves' pre-order, but they are post-mutation
/// anyway). This invariant unlocks the v2 "Hard rule: one layer
/// per instance" guarantee — the original leaf's [`LayerId`]
/// (derived from `pre_order`) is stable across the mutation so the
/// AGENTS.md "no LayerId rebinding" rule holds without a fresh
/// PTY spawn for the survivor.
///
/// Returns the original leaf's [`LayoutNode`] so callers may inspect
/// the dropped content (label, kind) without re-resolving the
/// tree.
pub fn replace_leaf_with_split(
    root: &mut LayoutNode,
    path: &[u16],
    new_pane: LayoutNode,
    axis: SplitAxis,
    ratio: Ratio,
) -> Result<LayoutNode, LayoutError> {
    if path.is_empty() {
        return Err(LayoutError::TreeTooDeep(0));
    }
    if path.len() > MAX_TREE_DEPTH {
        return Err(LayoutError::TreeTooDeep(path.len()));
    }
    let leaf_idx = path[path.len() - 1] as usize;
    let parent_path = &path[..path.len() - 1];
    let original = clone_child(root, parent_path, leaf_idx)?;
    let replacement = LayoutNode::Split {
        axis,
        ratio,
        children: vec![original.clone(), new_pane],
    };
    replace_child(root, parent_path, leaf_idx, replacement)?;
    Ok(original)
}

/// Remove the leaf at `path[..]` in `root` and rebalance: if the
/// leaf's parent is a [`LayoutNode::Split`] (or [`LayoutNode::Stack`])
/// whose surviving sibling is itself a leaf, the survivor is
/// SPLICED into the grandparent at the parent's slot, "absorbing"
/// the now-single-leaf parent upward. This mirrors tmux/zellij's
/// tree-on-close behavior — a 2-child Split collapses to its
/// survivor when one leaf closes.
///
/// If the leaf's parent has no surviving sibling (i.e. the leaf
/// was the only child), the slot is left empty (callers downstream
/// should treat the empty tree as "binary quits"). If the leaf
/// is the root itself, [`LayoutError::SplitChildCount`] is returned
/// so the action handler can short-circuit to `*running = false`.
///
/// Depth bound: bounded by [`MAX_TREE_DEPTH`] (8) — a v1 flat
/// Split of Split-of-Split can absorb at most one layer up per
/// close. A future v2 with deeper nesting may need a recursive
/// rebalance loop.
pub fn remove_leaf(root: &mut LayoutNode, path: &[u16]) -> Result<(), LayoutError> {
    if path.is_empty() {
        return Err(LayoutError::TreeTooDeep(0));
    }
    if path.len() > MAX_TREE_DEPTH {
        return Err(LayoutError::TreeTooDeep(path.len()));
    }
    let leaf_idx = path[path.len() - 1] as usize;
    let parent_path = &path[..path.len() - 1];
    // Step 1: capture the surviving sibling (clone before mutating).
    let surviving = match if parent_path.is_empty() {
        Some((&mut *root) as &mut LayoutNode)
    } else {
        None
    } {
        Some(n) => match n {
            LayoutNode::Split { children, .. } => surviving_sibling_of(leaf_idx, children),
            LayoutNode::Stack { panes } | LayoutNode::ZStack { panes } => {
                surviving_sibling_of(leaf_idx, panes)
            }
            LayoutNode::Pane(_) | LayoutNode::Preset { .. } => {
                return Err(LayoutError::SplitChildCount { got: 0 })
            }
        },
        None => {
            let parent_node = walk_mut(root, parent_path)?;
            match parent_node {
                LayoutNode::Split { children, .. } => {
                    surviving_sibling_of(leaf_idx, children)
                }
                LayoutNode::Stack { panes } | LayoutNode::ZStack { panes } => {
                    surviving_sibling_of(leaf_idx, panes)
                }
                LayoutNode::Pane(_) | LayoutNode::Preset { .. } => {
                    return Err(LayoutError::SplitChildCount { got: 0 })
                }
            }
        }
    };
    // Step 2: physically remove the leaf from the parent's children.
    {
        let parent_node: &mut LayoutNode =
            if parent_path.is_empty() { root } else { walk_mut(root, parent_path)? };
        match parent_node {
            LayoutNode::Split { children, .. } => {
                if leaf_idx >= children.len() {
                    return Err(LayoutError::SplitChildCount { got: children.len() });
                }
                children.remove(leaf_idx);
            }
            LayoutNode::Stack { panes } | LayoutNode::ZStack { panes } => {
                if leaf_idx >= panes.len() {
                    return Err(LayoutError::SplitChildCount { got: panes.len() });
                }
                panes.remove(leaf_idx);
            }
            LayoutNode::Pane(_) | LayoutNode::Preset { .. } => {
                return Err(LayoutError::SplitChildCount { got: 0 })
            }
        }
    }
    // Step 3: absorb the surviving sibling upward into the
    // grandparent's slot (or replacing root if the parent IS root).
    if let Some(survivor) = surviving {
        if parent_path.is_empty() {
            *root = survivor;
        } else {
            let grandparent_path = &parent_path[..parent_path.len() - 1];
            let parent_slot = parent_path[parent_path.len() - 1] as usize;
            replace_child(root, grandparent_path, parent_slot, survivor)?;
        }
    }
    Ok(())
}

/// Walk an immutable `&` chain of children at `path` into the
/// tree, returning the node at `path`. Returns
/// [`LayoutError::SplitChildCount`] on a path that requests a
/// child index some non-Split/Stack node has, or out-of-range.
///
/// # Path semantics
///
/// Naming is a frequently-stepped trapdoor. `path` is the
/// **child-index-only** form -- not the raw `PaneId.path()`,
/// which carries a seed `[0]` internally. Live callers in the
/// `cmdash` binary pre-strip that seed before invoking this
/// helper, so `path` here is the array of child indices from the
/// root down (e.g., `[0, 1]` for the second child of the first
/// child). Specifically:
///
/// - `path` is empty (`&[]`): returns `Ok(root)` verbatim.
/// - Out-of-range child index: `Err(SplitChildCount { got: idx })`.
/// - Walking past a leaf (`LayoutNode::Pane` /
///   `LayoutNode::Preset`): `Err(SplitChildCount { got: 1 })`.
///
/// Callers that want a soft-fail `Option` apply `.ok()` (or
/// `.ok()?` if they already short-circuit on `None`); the
/// `cmdash` binary uses this pattern at its 4 inline callsites
/// (`focused_zstack_context` / `handle_stack_cycle` /
/// `crosstack_member(Direction::Down|Up|Left|Right, advance)`).
///
/// The path-traversal contract is pinned by the unit tests
/// `replace_leaf_with_split_preserves_original_pre_order` (asserts
/// pre-order invariance under a walk-plus-Split) and
/// `remove_leaf_collapses_top_level_split` (asserts a 3-level
/// nested walk through Split-of-Split). Treat those as the
/// load-bearing ground truth for callers doing layout mutation
/// on top of [`LayoutNode`].
pub fn walk_imut<'a>(
    root: &'a LayoutNode,
    path: &[u16],
) -> Result<&'a LayoutNode, LayoutError> {
    let mut node = root;
    for &idx in path {
        let next = match node {
            LayoutNode::Split { children, .. } => children.get(idx as usize),
            LayoutNode::Stack { panes } | LayoutNode::ZStack { panes } => {
                panes.get(idx as usize)
            }
            LayoutNode::Pane(_) | LayoutNode::Preset { .. } => {
                return Err(LayoutError::SplitChildCount { got: 1 })
            }
        }
        .ok_or(LayoutError::SplitChildCount { got: idx as usize })?;
        node = next;
    }
    Ok(node)
}

/// Walk a mutable `&mut` chain of children at `path` into the
/// tree, returning the node at `path`. Subject to the same
/// error conditions as [`walk_imut`].
fn walk_mut<'a>(
    root: &'a mut LayoutNode,
    path: &[u16],
) -> Result<&'a mut LayoutNode, LayoutError> {
    let mut node = root;
    for &idx in path {
        let next = match node {
            LayoutNode::Split { children, .. } => children.get_mut(idx as usize),
            LayoutNode::Stack { panes } | LayoutNode::ZStack { panes } => {
                panes.get_mut(idx as usize)
            }
            LayoutNode::Pane(_) | LayoutNode::Preset { .. } => {
                return Err(LayoutError::SplitChildCount { got: 1 })
            }
        }
        .ok_or(LayoutError::SplitChildCount { got: idx as usize })?;
        node = next;
    }
    Ok(node)
}

/// Clone the child at `parent_path -> idx` so callers can read
/// the pre-mutation contents without consuming them.
fn clone_child(
    root: &LayoutNode,
    parent_path: &[u16],
    idx: usize,
) -> Result<LayoutNode, LayoutError> {
    let parent = if parent_path.is_empty() {
        root
    } else {
        walk_imut(root, parent_path)?
    };
    let slot = match parent {
        LayoutNode::Split { children, .. } => children.get(idx),
        LayoutNode::Stack { panes } | LayoutNode::ZStack { panes } => panes.get(idx),
        LayoutNode::Pane(_) | LayoutNode::Preset { .. } => {
            return Err(LayoutError::SplitChildCount { got: 1 })
        }
    }
    .ok_or(LayoutError::SplitChildCount { got: idx })?;
    Ok(slot.clone())
}

/// Replace `parent_path -> idx` with `new_child` in-place.
fn replace_child(
    root: &mut LayoutNode,
    parent_path: &[u16],
    idx: usize,
    new_child: LayoutNode,
) -> Result<(), LayoutError> {
    let parent = if parent_path.is_empty() {
        root
    } else {
        walk_mut(root, parent_path)?
    };
    match parent {
        LayoutNode::Split { children, .. } => {
            if idx >= children.len() {
                return Err(LayoutError::SplitChildCount { got: children.len() });
            }
            children[idx] = new_child;
            Ok(())
        }
        LayoutNode::Stack { panes } | LayoutNode::ZStack { panes } => {
            if idx >= panes.len() {
                return Err(LayoutError::SplitChildCount { got: panes.len() });
            }
            panes[idx] = new_child;
            Ok(())
        }
        LayoutNode::Pane(_) | LayoutNode::Preset { .. } => {
            Err(LayoutError::SplitChildCount { got: 1 })
        }
    }
}

/// Clone the first child whose index is NOT `idx`. v1 expectations:
/// a Split has exactly 2 children so the survivor is uniquely
/// determined; a Stack has `N` children and a v2 close-mode
/// extension may need a directional choice. The current shape
/// returns the FIRST non-idx neighbour, which is consistent
/// with the Split use-case.
fn surviving_sibling_of(idx: usize, children: &[LayoutNode]) -> Option<LayoutNode> {
    for (i, c) in children.iter().enumerate() {
        if i != idx {
            return Some(c.clone());
        }
    }
    None
}

/// Compute the four-direction adjacent pane for a focused pane,
/// using the cell-grid `layout`. This is the algorithm named in
/// AGENTS.md Phase 2 carry-forward ("look up the adjacent pane
/// via `ComputedLayout`'s side-of-rect resolution"). Returns
/// `None` when no candidate satisfies the directional
/// constraints.
///
/// Algorithm (per direction):
/// 1. **Direction side** — `Right`: candidate's `rect.x >=
///    focused.right()` so the candidate is strictly to the
///    right (overlap-inclusive; a candidate whose rect starts at
///    `focused.right()` is still "to the right" but barely);
///    `Left`: candidate's `rect.right() <= focused.x`; `Down`:
///    candidate's `rect.y >= focused.bottom()`; `Up`:
///    candidate's `rect.bottom() <= focused.y`.
/// 2. **Perpendicular overlap** — `Right`/`Left`: maximize
///    `vertical_overlap(candidate, focused)` where
///    `vertical_overlap = max(0, min(c.bottom, f.bottom) -
///    max(c.y, f.y))`; tie-break by `horizontal_distance`
///    (smaller wins), final tie-break by `pre_order` (smaller wins).
/// 3. **Symmetric** for `Up`/`Down` with horizontal overlap /
///    vertical distance.
///
/// Ties on ALL three criteria resolve to the lexical-minimum
/// `pre_order` leaf, so a 2x2 grid resolves deterministically
/// (top-left wins in a 4-way tie).
pub fn adjacent_pane(
    layout: &ComputedLayout,
    focused: PaneId,
    direction: Direction,
) -> Option<PaneId> {
    let focused_pane = layout
        .panes
        .iter()
        .find(|p| p.id == focused)?;
    let f = focused_pane.rect;
    let mut best: Option<(u32 /* overlap */, u32 /* distance */, u32 /* pre_order */, PaneId)> =
        None;
    for pane in &layout.panes {
        if pane.id == focused {
            continue;
        }
        let c = pane.rect;
        let is_candidate = match direction {
            Direction::Right => c.x >= f.right(),
            Direction::Left => c.right() <= f.x,
            Direction::Down => c.y >= f.bottom(),
            Direction::Up => c.bottom() <= f.y,
        };
        if !is_candidate {
            continue;
        }
        // Distance along the direction axis.
        let dist = match direction {
            Direction::Right => c.x.saturating_sub(f.right()) as u32,
            Direction::Left => f.x.saturating_sub(c.right()) as u32,
            Direction::Down => c.y.saturating_sub(f.bottom()) as u32,
            Direction::Up => f.y.saturating_sub(c.bottom()) as u32,
        };
        // Perpendicular overlap (signed-clamped to zero).
        let overlap = match direction {
            Direction::Right | Direction::Left => {
                let lo = c.y.max(f.y) as u32;
                let hi = c.bottom().min(f.bottom()) as u32;
                hi.saturating_sub(lo)
            }
            Direction::Down | Direction::Up => {
                let lo = c.x.max(f.x) as u32;
                let hi = c.right().min(f.right()) as u32;
                hi.saturating_sub(lo)
            }
        };
        let pre_order = pane.id.pre_order();
        let replace = match best {
            None => true,
            Some((ov, di, po, _)) => {
                overlap > ov
                    || (overlap == ov && dist < di)
                    || (overlap == ov && dist == di && pre_order < po)
            }
        };
        if replace {
            // The `PaneId` is `Copy` (it's a u32 + a fixed-size `path`
            // array + path_len), so this is a trivial copy.
            best = Some((
                overlap,
                dist,
                pre_order,
                PaneId {
                    pre_order: pane.id.pre_order(),
                    path: *pane.id.path_arr(),
                    path_len: pane.id.path_len(),
                },
            ));
        }
    }
    best.map(|(_, _, _, id)| id)
}

#[cfg(test)]
mod internal_sanity_tests {
    use super::*;
    use cmdash_config::Pane;

    #[test]
    fn split_rect_horizontal_60() {
        let (l, r) = split_rect(
            Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 10,
            },
            SplitAxis::Horizontal,
            Ratio(60),
        );
        assert_eq!(
            l,
            Rect {
                x: 0,
                y: 0,
                w: 60,
                h: 10
            }
        );
        assert_eq!(
            r,
            Rect {
                x: 60,
                y: 0,
                w: 40,
                h: 10
            }
        );
    }

    #[test]
    fn split_rect_vertical_30() {
        let (t, b) = split_rect(
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 10,
            },
            SplitAxis::Vertical,
            Ratio(30),
        );
        assert_eq!(
            t,
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 3
            }
        );
        assert_eq!(
            b,
            Rect {
                x: 0,
                y: 3,
                w: 80,
                h: 7
            }
        );
    }

    fn p(label: Option<&str>) -> LayoutNode {
        LayoutNode::Pane(Pane {
            kind: PaneKind::Shell,
            label: label.map(str::to_string),
        })
    }

    fn split_h(a: LayoutNode, b: LayoutNode) -> LayoutNode {
        LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: Ratio(50),
            children: vec![a, b],
        }
    }

    fn split_v(a: LayoutNode, b: LayoutNode) -> LayoutNode {
        LayoutNode::Split {
            axis: SplitAxis::Vertical,
            ratio: Ratio(50),
            children: vec![a, b],
        }
    }

    /// `replace_leaf_with_split` MUST preserve the original leaf's
    /// `pre_order` index because the layout's resolver enumerates
    /// child 0 first. Pins the Phase 2 carry-forward invariant.
    #[test]
    fn replace_leaf_with_split_preserves_original_pre_order() {
        let mut root = split_h(p(Some("a")), p(Some("b")));
        let pre = ComputedLayout::compute(
            &root,
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute pre");
        let original_id = pre.panes[0].id; // leaf a
        assert_eq!(original_id.pre_order(), 0);
        let original_path: Vec<u16> = original_id.path().to_vec();

        // Replace the leftmost leaf with a vertical-split that
        // contains the original leaf (now child 0) and a new pane
        // (child 1, label "c"). Full tree-indices path: leaf "a"
        // lives at root.children[0]; the wrapper seed at
        // PaneId.path[0] has already been stripped (live callers
        // in `cmdash/src/main.rs::split_focused_for_new_pane`
        // strip it before invoking this helper).
        let dropped = replace_leaf_with_split(
            &mut root,
            &[0],
            p(Some("c")),
            SplitAxis::Vertical,
            Ratio(40),
        )
        .expect("replace");
        assert_eq!(
            dropped,
            LayoutNode::Pane(Pane {
                kind: PaneKind::Shell,
                label: Some("a".to_string()),
            })
        );

        // The original leaf's pre_order is still 0; the new pane
        // has pre_order 1 (one leaf "c" was added before "b").
        let post = ComputedLayout::compute(
            &root,
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute post");
        assert_eq!(post.panes.len(), 3);
        // The stable invariant is `pre_order`: the resolver
        // wraps an extra level deep post-Split, so the original
        // leaf's `path` grows by one slot (the wrapper index
        // 0 inserts in front). `pre_order` itself is what the
        // AGENTS.md Hard rule pins.
        assert_eq!(
            post.panes[0].id.pre_order(),
            original_id.pre_order(),
            "original leaf's pre_order unchanged across the replacement"
        );
        assert_eq!(
            post.panes[1].id.pre_order(),
            1,
            "new pane takes the next available pre_order"
        );
        assert_eq!(post.panes[1].label, Some("c".to_string()));
        assert_eq!(post.panes[2].label, Some("b".to_string()));
        let _ = original_path;
    }

    /// Closing the right child of a 2-leaf Split collapses the
    /// Split into the surviving left child (sibling absorption).
    /// Pins AGENTS.md's "split-on-close collapses cleanly" UX.
    /// Computed against the SAME recursed area.
    #[test]
    fn remove_leaf_collapses_top_level_split() {
        let mut root = split_h(p(Some("a")), p(Some("b")));
        // Full tree-indices path: leaf "b" lives at
        // root.children[1] of the implicit-wrapper-stripped
        // PaneId path (live callers strip the seed before
        // calling this helper).
        remove_leaf(&mut root, &[1]).expect("remove b");
        assert_eq!(
            root,
            p(Some("a")),
            "closing b must collapse the Split to leaf a"
        );
    }

    /// Closing one leaf of a NESTED Split (inner Split is a child
    /// of the outer Split) absorbs the survivor of the inner Split
    /// into the outer Split's slot. Pins the recursive rebalance
    /// path that keeps tree height bounded.
    #[test]
    fn remove_leaf_absorbs_nested_split_one_level() {
        let mut root = split_h(
            // outer Split child 0 = inner Split
            split_h(p(Some("a1")), p(Some("a2"))),
            p(Some("b")),
        );
        // Close "a2" at path [0, 0, 1]:
        //   [0]         -> outer Split's child 0 (inner Split)
        //   [0, 0, 1]   -> inner-Split child 1 (a2). path[2] = 1.
        remove_leaf(&mut root, &[0, 1]).expect("remove a2");
        // After close: inner Split collapses to "a1", which is
        // spliced into outer Split's child 0 slot. Tree becomes
        // Split { H, [a1, b] }.
        let expected = split_h(p(Some("a1")), p(Some("b")));
        assert_eq!(
            root, expected,
            "closing a2 must absorb the inner Split one level up"
        );
    }

    /// Closing the only child of a Split has no surviving sibling;
    /// after removing the leaf the parent has zero children, which
    /// the layout resolver surfaces as an [`EmptyChildren`]
    /// error. The action handler downstream should trigger
    /// `*running = false` BEFORE calling `remove_leaf` against
    /// an only-child parent, but if it doesn't, the resolver's
    /// next `compute` call will produce the error.
    #[test]
    fn resolve_after_remove_only_child_surfaces_empty_children() {
        // Build a Split whose single child we then remove.
        let mut root = split_h(p(Some("a")), p(Some("b")));
        remove_leaf(&mut root, &[1]).expect("remove b");
        // After remove b, root is a single "a" Pane.  Compute
        // succeeds; the user-visible tree is just one pane.
        let post = ComputedLayout::compute(
            &root,
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute single leaf");
        assert_eq!(post.panes.len(), 1);
        assert_eq!(post.panes[0].label, Some("a".to_string()));
    }

    /// Phase 3: `zstack { a, b }` resolves every member with
    /// the parent's `area` verbatim. Members share the cell-
    /// grid surface (z-stack overlay); each still gets its own
    /// `PaneId` and `pre_order` index. Pins Phase 3's "shared
    /// rect, distinct ids" invariant.
    #[test]
    fn resolve_zstack_shares_parent_rect() {
        let root = LayoutNode::ZStack {
            panes: vec![
                LayoutNode::Pane(Pane {
                    kind: PaneKind::Shell,
                    label: Some("a".to_string()),
                }),
                LayoutNode::Pane(Pane {
                    kind: PaneKind::Shell,
                    label: Some("b".to_string()),
                }),
            ],
        };
        let layout = ComputedLayout::compute(
            &root,
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute zstack");
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(
            layout.panes[0].rect, layout.panes[1].rect,
            "ZStack members share the same rect"
        );
        assert_eq!(
            layout.panes[0].rect,
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24
            }
        );
        assert_ne!(
            layout.panes[0].id, layout.panes[1].id,
            "ZStack members have distinct PaneIds (Hard rule: one layer per instance)"
        );
        assert_eq!(layout.panes[0].id.pre_order(), 0);
        assert_eq!(layout.panes[1].id.pre_order(), 1);
    }

    /// Rect-proximity adjacency: a 2-pane horizontal-split
    /// layout; focusing left and pressing Right yields the
    /// right pane; focusing right and pressing Left yields the
    /// left pane. This is the AGENTS.md Phase 2 carry-forward
    /// algorithm priority path: max perpendicular overlap,
    /// then min distance, then min pre_order.
    #[test]
    fn adjacent_pane_right_left_simple_split() {
        let root = split_h(p(Some("left")), p(Some("right")));
        let layout = ComputedLayout::compute(
            &root,
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        assert_eq!(layout.panes.len(), 2);
        let left_id = layout.panes[0].id;
        let right_id = layout.panes[1].id;
        assert_eq!(
            adjacent_pane(&layout, left_id, Direction::Right),
            Some(right_id)
        );
        assert_eq!(
            adjacent_pane(&layout, right_id, Direction::Left),
            Some(left_id)
        );
        // Symmetric "no neighbour in opposite direction" returns
        // `None` (no pane is LEFT of the leftmost pane).
        assert_eq!(
            adjacent_pane(&layout, left_id, Direction::Left),
            None
        );
    }

    /// 2x2 grid (outer V over inner H pairs): focused pane Right
    /// yields the pane directly to its right (max vertical
    /// overlap with the focused row). Pin for the perpendicular-
    /// overlap priority.
    #[test]
    fn adjacent_pane_2x2_grid_right_resolves_to_overlapping_neighbor() {
        // Build: split V { split H{ top-left, top-right }, split H{
        // bot-left, bot-right } }. Outer V splits rows; each row's
        // inner H splits columns. Label positions match the
        // resolver outcome so the assertions can read "tl <-> tr"
        // / "tr <-> br" / etc. directly.
        let root = split_v(
            split_h(p(Some("tl")), p(Some("tr"))),
            split_h(p(Some("bl")), p(Some("br"))),
        );
        let layout = ComputedLayout::compute(
            &root,
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute 2x2");
        assert_eq!(layout.panes.len(), 4);
        let tl = layout.panes[0].id;
        let tr = layout.panes[1].id;
        let bl = layout.panes[2].id;
        let br = layout.panes[3].id;
        // tl -> right should pick tr (overlapping neighbours, top
        // half) rather than br (also to the right, but no vertical
        // overlap with tl's top row).
        assert_eq!(adjacent_pane(&layout, tl, Direction::Right), Some(tr));
        assert_eq!(adjacent_pane(&layout, bl, Direction::Right), Some(br));
        assert_eq!(adjacent_pane(&layout, tl, Direction::Down), Some(bl));
        assert_eq!(adjacent_pane(&layout, tr, Direction::Down), Some(br));
        assert_eq!(adjacent_pane(&layout, bl, Direction::Up), Some(tl));
        assert_eq!(adjacent_pane(&layout, br, Direction::Up), Some(tr));
        assert_eq!(adjacent_pane(&layout, tl, Direction::Left), None);
        assert_eq!(adjacent_pane(&layout, br, Direction::Right), None);
    }

    /// Phase 3: a ZStack nested inside a Split has its members
    /// overlay the Split's child rect — NOT the root rect. The
    /// resolver passes the path's accumulating area verbatim to
    /// non-stripping children, so overlay rects are scoped to the
    /// nearest ancestor that performed a slicing split. Pins
    /// Phase 3's "scope-by-parent-area" invariant.
    #[test]
    fn resolve_zstack_within_split_uses_split_child_rect() {
        // Outer V split at 50% gives the top half (y=0..12) to
        // the ZStack and the bottom half (y=12..24) to "tail".
        // (split_v in this codebase slices rows top/bottom; split_h
        // slices columns left/right.)
        let root = split_v(
            LayoutNode::ZStack {
                panes: vec![
                    LayoutNode::Pane(Pane {
                        kind: PaneKind::Shell,
                        label: Some("ovl_a".to_string()),
                    }),
                    LayoutNode::Pane(Pane {
                        kind: PaneKind::Shell,
                        label: Some("ovl_b".to_string()),
                    }),
                ],
            },
            p(Some("tail")),
        );
        let layout = ComputedLayout::compute(
            &root,
            Rect { x: 0, y: 0, w: 80, h: 24 },
        )
        .expect("compute ZStack-within-Split");
        // 3 panes total: 2 overlay + 1 tail.
        assert_eq!(layout.panes.len(), 3);
        let ovl_a = &layout.panes[0];
        let ovl_b = &layout.panes[1];
        let tail = &layout.panes[2];
        assert_eq!(ovl_a.label, Some("ovl_a".to_string()));
        assert_eq!(ovl_b.label, Some("ovl_b".to_string()));
        assert_eq!(tail.label, Some("tail".to_string()));
        // Both overlay members share the TOP half (y=0..12), not
        // the root area (y=0..24).
        assert_eq!(ovl_a.rect, ovl_b.rect);
        assert_eq!(
            ovl_a.rect,
            Rect { x: 0, y: 0, w: 80, h: 12 }
        );
        // Tail got the bottom half.
        assert_eq!(
            tail.rect,
            Rect { x: 0, y: 12, w: 80, h: 12 }
        );
        // Overlay peers still have distinct PaneIds.
        assert_ne!(ovl_a.id, ovl_b.id);
    }

    /// Phase 3: a 3-member ZStack emits three ComputedPanes each
    /// sharing the same rect but with strictly increasing
    /// pre_orders. The dashcompositor's LayerStack draws pre_order
    /// 2 LAST, so the latest member is the topmost visible pane.
    /// Pins Phase 3's "shared rect, ordered z-stack" invariant.
    #[test]
    fn resolve_zstack_three_members_ordered_pre_orders() {
        let root = LayoutNode::ZStack {
            panes: vec![
                p(Some("bottom")),
                p(Some("middle")),
                p(Some("top")),
            ],
        };
        let layout = ComputedLayout::compute(
            &root,
            Rect { x: 0, y: 0, w: 80, h: 24 },
        )
        .expect("compute 3-member zstack");
        assert_eq!(layout.panes.len(), 3);
        // All share the rect.
        assert_eq!(layout.panes[0].rect, layout.panes[1].rect);
        assert_eq!(layout.panes[1].rect, layout.panes[2].rect);
        // Pre-orders strictly increase, in declaration order.
        assert!(layout.panes[0].id.pre_order() < layout.panes[1].id.pre_order());
        assert!(layout.panes[1].id.pre_order() < layout.panes[2].id.pre_order());
        // Labels preserved in declaration order.
        assert_eq!(layout.panes[0].label, Some("bottom".to_string()));
        assert_eq!(layout.panes[1].label, Some("middle".to_string()));
        assert_eq!(layout.panes[2].label, Some("top".to_string()));
        // Distinct ids.
        for (i, a) in layout.panes.iter().enumerate() {
            for (j, b) in layout.panes.iter().enumerate() {
                if i != j {
                    assert_ne!(a.id, b.id);
                }
            }
        }
    }
}
