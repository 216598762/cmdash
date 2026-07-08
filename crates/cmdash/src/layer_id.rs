//! Layer-identity derivation: maps a [`cmdash_layout::PaneId`] to a
//! [`cmdash_pty::PaneLayerId`].
//!
//! AGENTS.md §"Hard rule: one layer per instance" states that the
//! layer identity is bound to the pane identity 1:1 for the pane's
//! whole lifetime. In cmdash v1 the layout tree is static, so a
//! pane's `pre_order` index alone is a unique, stable identifier
//! across the whole tree across resizes. We pack a tab id in the
//! high half so a v2 with multiple tabs can extend this without
//! re-deriving any pane-layer id (collision-free per tab).
//!
//! Cycle-22 atom-1 introduces multi-tab support: the
//! [`derive_layer_id_for_tab`] helper takes the tab id alongside
//! the [`PaneId`] so the high half carries the tab identifier at
//! per-tab granularity. The legacy [`derive_layer_id`] alias is
//! preserved for callers that hardcode the v1 single-tab path
//! (it returns `derive_layer_id_for_tab(pane, SINGLE_TAB)`).

use cmdash_layout::PaneId;
use cmdash_pty::PaneLayerId;

/// Single active tab for v1. Future tab support will produce
/// additional tab IDs from the conductor's tab stack.
pub const SINGLE_TAB: u32 = 0;

/// Derive a pane-layer ID from a [`PaneId`].
///
/// Layout is static in v1, so the pre-order leaf index alone is
/// unique across the tree. The packed form
/// `((tab_id as u64) << 32) | (pre_order as u64)` keeps each tab's
/// IDs collision-free in v2 when multiple tabs land.
///
/// Cycle-22 atom-1: prefer [`derive_layer_id_for_tab`] when the
/// caller has a `tab_id` available so multi-tab runners get
/// collision-free layer ids. This alias pins the v1 shape at
/// [`SINGLE_TAB`] for tests that hardcode the single-tab path.
pub fn derive_layer_id(pane: &PaneId) -> PaneLayerId {
    derive_layer_id_for_tab(pane, SINGLE_TAB)
}

/// Derive a pane-layer ID for a specific tab.
///
/// `tab_id` packs into the high 32 bits of the resulting
/// `PaneLayerId`'s `u64`; `pane.pre_order()` packs into the low
/// 32 bits. The shape `((tab_id as u64) << 32) | (pre_order as u64)`
/// is collision-free across tabs because the high half separates
/// each tab's id space — a v1 `pre_order=5` pane on `tab_id=0` is
/// `LayerId(0x0_0000_0005)`; the same pane geometry on `tab_id=1`
/// is `LayerId(0x1_0000_0005)`. The two ids never collide in
/// `dashcompositor`'s `LayerStack` even when the resolver produces
/// the same `pre_order` index across two separate tabs.
///
/// Pin: invalid `tab_id` values (anything past `u32::MAX / 2`)
/// would collide with the high-bit sign of `PaneLayerId` only if
/// the upstream type is signed; today it's `u64`, so the
/// collision-free guarantee holds for the entire `u32` range.
/// Cycle-22 atom-1.
pub fn derive_layer_id_for_tab(pane: &PaneId, tab_id: u32) -> PaneLayerId {
    // Runtime cap added by cycle-22 atom-1 ship-green review
    // (reviewer item B): tab_id must stay below the high-bit
    // sign boundary of the u64 packing, otherwise two panes on
    // the same tab could alias via the high-bit sign flip. A
    // future cycle (atom-2+) that accidentally passes
    // tab_id=0x8000_0000 now panics in debug builds instead of
    // silently aliasing LayerIds.
    debug_assert!(
        tab_id <= u32::MAX >> 1,
        "tab_id overflows the high-32-bits cap ({tab_id} > u32::MAX/2)"
    );
    let raw = ((tab_id as u64) << 32) | (pane.pre_order() as u64);
    PaneLayerId(raw)
}

#[cfg(test)]
mod tests {
    //! Collision-free guarantee: two tabs with the same
    //! `pre_order` index produce distinct `PaneLayerId` values;
    //! and within a single tab two distinct panes produce
    //! distinct `PaneLayerId` values (so dashcompositor's
    //! `LayerStack::render` cannot alias two panes on the
    //! same tab to a single `LayerId`).
    use super::*;
    use cmdash_config::{LayoutNode, PaneKind, Ratio, SplitAxis};
    use cmdash_layout::{ComputedLayout, Rect};

    /// Helper: build a 2-leaf horizontal-split `LayoutNode`
    /// with the given labels so a unit-test can derive two
    /// DISTINCT `PaneId`s via `ComputedLayout::compute` without
    /// needing a public `PaneId` constructor (which would be
    /// an API surface addition). Keeps this test module
    /// self-contained against the existing `cmdash-config` +
    /// `cmdash-layout` public surface.
    fn two_pane_split(a: &str, b: &str) -> LayoutNode {
        LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: Ratio(50),
            children: vec![
                LayoutNode::Pane(cmdash_config::Pane {
                    kind: PaneKind::Shell,
                    label: Some(a.into()),
                }),
                LayoutNode::Pane(cmdash_config::Pane {
                    kind: PaneKind::Shell,
                    label: Some(b.into()),
                }),
            ],
        }
    }

    /// Same `PaneId`, two different tabs, distinct ids.
    /// Pins the v2 multi-tab extension path that
    /// `cmdash::main::TickContext::spawn_with_graphics` flows
    /// through.
    #[test]
    fn derive_layer_id_for_tab_distinguishes_tabs() {
        let pane = PaneId::default();
        let id0 = derive_layer_id_for_tab(&pane, 0);
        let id1 = derive_layer_id_for_tab(&pane, 1);
        assert_ne!(id0, id1, "tab-0 vs tab-1 must produce distinct ids");
    }

    /// [`derive_layer_id`] is a thin [`SINGLE_TAB`] wrapper for
    /// `derive_layer_id_for_tab`. Pins the legacy alias.
    #[test]
    fn derive_layer_id_aliases_single_tab() {
        let pane = PaneId::default();
        let a = derive_layer_id(&pane);
        let b = derive_layer_id_for_tab(&pane, SINGLE_TAB);
        assert_eq!(
            a, b,
            "derive_layer_id must equal derive_layer_id_for_tab(_, SINGLE_TAB)"
        );
    }

    /// Same `tab_id=7`, two DIFFERENT `PaneId`s, distinct ids.
    /// Pins the low-32-bits unmasking path: two panes on the
    /// same tab must each own a fresh `PaneLayerId`. Without
    /// this, a v2 multi-pane tab would alias every geometry
    /// cell to `LayerId(0x7_0000_0000)` once `pre_order=0`
    /// is reached, and dashcompositor's `LayerStack::render`
    /// would clobber every pane on top of the first. Cycle-22
    /// atom-1.
    #[test]
    fn derive_layer_id_for_tab_distinguishes_panes_in_same_tab() {
        // Build a 2-leaf horizontal-split layout and resolve
        // it through the public `ComputedLayout::compute` API.
        // The resolver hands out pre-orders 0 and 1, so the
        // two resulting PaneIds are distinct even though both
        // share the same tab_id underneath.
        let root = two_pane_split("left", "right");
        let layout = ComputedLayout::compute(
            &root,
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute 2-pane split");
        assert_eq!(layout.panes.len(), 2);
        let pane_left = layout.panes[0].id;
        let pane_right = layout.panes[1].id;
        assert_ne!(
            pane_left, pane_right,
            "the 2-pane Split must produce distinct PaneIds (resolver invariant)"
        );
        assert_ne!(
            pane_left.pre_order(),
            pane_right.pre_order(),
            "pre_order indices must differ (this is what makes the packing unambiguous)"
        );

        // Same tab_id, distinct panes → distinct LayerIds.
        const TAB_ID: u32 = 7;
        let id_left = derive_layer_id_for_tab(&pane_left, TAB_ID);
        let id_right = derive_layer_id_for_tab(&pane_right, TAB_ID);
        assert_ne!(
            id_left, id_right,
            "same-tab same-tab-id but distinct pre_orders must produce distinct LayerIds"
        );
        // Sanity: the cross-tab variant still differs (so the
        // test is not symmetric trivial).
        let id_left_tab0 = derive_layer_id_for_tab(&pane_left, 0);
        assert_ne!(
            id_left, id_left_tab0,
            "different tab ids must still produce distinct LayerIds"
        );
    }
}
