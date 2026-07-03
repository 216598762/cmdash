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

/// All leaf panes resolved for one tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputedLayout {
    /// Leaf panes in pre-order.
    pub panes: Vec<ComputedPane>,
    /// The cell-grid area the layout was computed for.
    pub total: Rect,
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

impl ComputedLayout {
    /// Resolve `root` into a flat list of [`ComputedPane`] entries
    /// laid out within the cell-grid `area`.
    ///
    /// `area` must have non-zero width **and** height; a zero-area
    /// root is a configuration error. PaneIds are stable across
    /// repeated calls with the same `root`.
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
        // Seed `path[0] = 0` to represent the implicit outermost
        // `layout { ... }` wrapper that every `cmdash-config::parse()`
        // result has. ``cmdash-config`` strips that wrapper before we
        // ever see the root, so we re-introduce it here as a depth-0
        // ancestor. This keeps leaf paths consistent with the
        // original KDL document's depth (e.g. a leaf inside
        // ``layout { split { ... } }`` ends up at ``path = [0, 0, ...]``
        // rather than ``path = [0, ...]``).
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

/// Split `area` along `axis` at `ratio` percent of the dimension.
/// Child 0 gets `ratio%`; child 1 gets the remainder.
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

#[cfg(test)]
mod internal_sanity_tests {
    use super::*;

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
}
