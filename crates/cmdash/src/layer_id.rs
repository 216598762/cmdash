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
pub fn derive_layer_id(pane: &PaneId) -> PaneLayerId {
    let raw = ((SINGLE_TAB as u64) << 32) | (pane.pre_order() as u64);
    PaneLayerId(raw)
}
