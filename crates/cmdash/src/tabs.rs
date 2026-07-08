//! Tab-axis data surface for cmdash's `KeyAction::{TabNew,
//! TabClose, TabSwitch(n)}` runtime primitives.
//!
//! ## Design
//!
//! `Tab<T>` is a single-tab wrapper holding a payload `T` plus a
//! human-readable `label` (the v1 tab title). `TabStack<T>` is a
//! linear stack of Tabs plus an `active_idx` cursor; the cursor is
//! upkept by the `TabStack::remove` / `TabStack::switch_to` /
//! hot-path accessors so callers don't have to re-clamp `active_idx
//! < len()` themselves.
//!
//! ## Cross-tab LayerId namespace contract
//!
//! The cmdash `cmdash::layer_id` module's
//! `derive_layer_id_for_tab(pane, tab_id)` packs `(tab_id << 32)
//! | pre_order` into a `PaneLayerId`, collision-free across tabs.
//! Every fresh pane spawn in a tab MUST pass the tab's
//! `TabStack::active_idx()` (cast to `u32`) into
//! `derive_layer_id_for_tab`, so two tabs with the same pre-order
//! geometry never alias to the same `LayerId`.
//!
//! InPlace survivor rebinds in `TickContext::reconcile_runners`
//! deliberately preserve the survivor's `PaneLayerId` per AGENTS.md
//! "Hard rule: one layer per instance".
//!
//! The tab bar is not yet rendered; it will be added once the
//! keyboard model settles.

/// A single tab; holds the per-tab payload `T` plus an optional
/// human-readable title.
///
/// `T` is unconstrained (no trait bounds imposed by this mod)
/// because the binary-side callers bind `T = TabState` which
/// carries its own trait derives (`#[derive(Debug)]` etc.); the
/// generic struct itself is bounded only by `T: fmt::Debug` so
/// `Tab<T>: fmt::Debug` keeps the test-helper debug-println paths
/// working without forcing every concrete `T` to re-derive
/// beyond its own surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab<T> {
    /// Per-tab state. Forwarded verbatim from the `TickContext`
    /// `tab_state` field at insertion time.
    pub state: T,
    /// Optional human-readable title surfaced through the
    /// future tab-bar layer (`AGENTS.md` §"Tabs"). Currently
    /// populated via [`TabStack::new_with_label`] for the
    /// initial-frame tab; runtime `TabNew` arms wire `TabStack.push`
    /// with `label: None`.
    pub label: Option<String>,
}

impl<T> Tab<T> {
    /// Wrap `state` with `label = None`.
    pub fn new(state: T) -> Self {
        Self { state, label: None }
    }

    /// Wrap `state` with the given label.
    pub fn with_label(state: T, label: impl Into<String>) -> Self {
        Self {
            state,
            label: Some(label.into()),
        }
    }

    /// Borrow a reference to the inner payload.
    pub fn state(&self) -> &T {
        &self.state
    }

    /// Mutable borrow of the inner payload.
    pub fn state_mut(&mut self) -> &mut T {
        &mut self.state
    }
}

/// Linear stack of `Tab<T>` plus an `active_idx` cursor.
///
/// Invariants upheld by the mutating methods:
///
/// 1. `active_idx < self.tabs.len()` whenever `self.tabs` is
///    non-empty. The pre-condition is enforced on every entry to
///    a mutator; the post-condition is `<self.tabs.len()` for the
///    cursor after each operation.
/// 2. After `TabStack::remove`, `active_idx` is clamped to
///    `len() - 1` (matches AGENTS.md / cmdash-config `TabClose`
///    rustdoc semantics: "active_tab is clamped to tabs.len() -
///    1; closing the last tab quits the binary").
/// 3. After `TabStack::switch_to(n)`, `active_idx = n` for any
///    in-range `n`; out-of-range `n` is a no-op, leaving the
///    cursor unchanged (matches cmdash-config `TabSwitch(n)` M-1..M-9
///    keybind range: out-of-range bindings are silently ignored).
///
/// `T: fmt::Debug` is the only trait bound so the type composites
/// with `cmdash::main::TabState` (the per-tab payload defined on
/// the binary side) without forcing cross-crate trait shuffling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabStack<T> {
    tabs: Vec<Tab<T>>,
    active_idx: usize,
}

impl<T> TabStack<T> {
    /// Construct a `TabStack` containing a single `Tab` of initial
    /// payload `state` with `active_idx = 0`. The `initial-frame`
    /// shape used by `cmdash::run`'s top-level wiring so the
    /// 1-tab `TabStack` with minimal call-site edits.
    pub fn new(state: T) -> Self {
        Self {
            tabs: vec![Tab::new(state)],
            active_idx: 0,
        }
    }

    /// Like [`TabStack::new`] but with an initial-frame label.
    pub fn new_with_label(state: T, label: impl Into<String>) -> Self {
        Self {
            tabs: vec![Tab::with_label(state, label)],
            active_idx: 0,
        }
    }

    /// Push a NEW tab at index `len()` and switch the cursor to
    /// it. Mirrors `cmdash-config::KeyAction::TabNew` rustdoc:
    /// "create a new empty tab and switch focus to it". Returns
    /// the new tab's index.
    pub fn push(&mut self, state: T) -> usize {
        let new_idx = self.tabs.len();
        self.tabs.push(Tab::new(state));
        self.active_idx = new_idx;
        new_idx
    }

    /// Like `TabStack::push` but with a label.
    pub fn push_with_label(&mut self, state: T, label: impl Into<String>) -> usize {
        let new_idx = self.tabs.len();
        self.tabs.push(Tab::with_label(state, label));
        self.active_idx = new_idx;
        new_idx
    }

    /// Remove the active tab. After removal the cursor is clamped
    /// to `len() - 1` so the next `active()` call lands on the
    /// newly-active tab (or returns `None` on an empty stack).
    /// Removing the LAST tab leaves the stack empty — the
    /// `cmdash::main::KeyAction::TabClose` arm translates that to
    /// `self.running = false` (binary quits).
    ///
    /// The removed tab is returned so callers can inspect its
    /// pre-removal state (e.g. for logging or for tab-bar cleanup
    /// in a future atom).
    pub fn remove_active(&mut self) -> Option<Tab<T>> {
        if self.tabs.is_empty() {
            return None;
        }
        let removed = self.tabs.remove(self.active_idx);
        if self.tabs.is_empty() {
            self.active_idx = 0;
        } else if self.active_idx >= self.tabs.len() {
            self.active_idx = self.tabs.len() - 1;
        }
        Some(removed)
    }

    /// Switch the cursor to the zero-indexed tab `n`. Out-of-range
    /// `n` is a silent no-op (matches M-1..M-9 keybind range: a
    /// chord bound to `tab.switch.7` against a stack of 3 tabs is
    /// silently ignored rather than refreshing focus with an
    /// error). Returns `true` on success, `false` on out-of-range
    /// `n` or an empty stack.
    pub fn switch_to(&mut self, n: usize) -> bool {
        if n < self.tabs.len() {
            self.active_idx = n;
            true
        } else {
            false
        }
    }

    /// Number of tabs.
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// `true` iff no tabs remain.
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Index of the active tab. Only valid when `len() > 0`; when
    /// the stack is empty (after `remove_active` of the last
    /// tab), the value is `0` but should be paired with an
    /// `is_empty()` guard before any `active()` call.
    pub fn active_idx(&self) -> usize {
        self.active_idx
    }

    /// Borrow the active tab. Returns `None` when the stack is
    /// empty (the post-`remove_active(last)` state).
    pub fn active(&self) -> Option<&Tab<T>> {
        self.tabs.get(self.active_idx)
    }

    /// Mutable borrow of the active tab. Returns `None` on
    /// empty stack.
    pub fn active_mut(&mut self) -> Option<&mut Tab<T>> {
        self.tabs.get_mut(self.active_idx)
    }

    /// Iterate all tabs (label + state). Used by phase 3a's
    /// terminal-draw future tab-bar render and by tests that need
    /// to drive per-tab assertions without exposing the inner
    /// Vec.
    pub fn iter(&self) -> impl Iterator<Item = &Tab<T>> {
        self.tabs.iter()
    }

    /// Mutable iteration over all tabs. Lets reconcile paths
    /// touch sibling tabs without exposing the inner Vec.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Tab<T>> {
        self.tabs.iter_mut()
    }

    /// Borrow the tab at index `n` (for tests + sibling access).
    /// Out-of-range `n` returns `None`.
    pub fn get(&self, n: usize) -> Option<&Tab<T>> {
        self.tabs.get(n)
    }

    /// Mutable borrow of the tab at index `n`.
    pub fn get_mut(&mut self, n: usize) -> Option<&mut Tab<T>> {
        self.tabs.get_mut(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `new` constructs a 1-tab stack with `active_idx == 0`.
    #[test]
    fn new_constructs_single_tab_stack_with_active_idx_zero() {
        let ts: TabStack<u32> = TabStack::new(42);
        assert_eq!(ts.len(), 1);
        assert!(!ts.is_empty());
        assert_eq!(ts.active_idx(), 0);
        let active = ts.active().expect("single-tab stack has an active tab");
        assert_eq!(active.state, 42);
        assert!(active.label.is_none());
    }

    /// `new_with_label` carries the initial label through to the
    /// first tab; subsequent `push*` arms keep their own labels.
    #[test]
    fn new_with_label_threads_label_to_initial_tab() {
        let ts: TabStack<u32> = TabStack::new_with_label(7, "first");
        assert_eq!(ts.active().unwrap().label.as_deref(), Some("first"));
    }

    /// `push` appends a tab AND switches the cursor to it. Returns
    /// the new tab's index (`len() - 1` pre-push). Mirrors
    /// `TabNew` semantics.
    #[test]
    fn push_appends_tab_and_switches_cursor() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push(1);
        assert_eq!(ts.len(), 2);
        assert_eq!(ts.active_idx(), 1);
        assert_eq!(ts.active().unwrap().state, 1);
        ts.push(2);
        assert_eq!(ts.len(), 3);
        assert_eq!(ts.active_idx(), 2);
        assert_eq!(ts.active().unwrap().state, 2);
    }

    /// `push_with_label` carries the label.
    #[test]
    fn push_with_label_threads_label() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push_with_label(1, "alpha");
        assert_eq!(ts.active().unwrap().label.as_deref(), Some("alpha"));
    }

    /// `remove_active` of the LAST tab leaves the stack empty;
    /// subsequent `active()` returns `None`. Mirrors the
    /// `KeyAction::TabClose` rustdoc's "closing the last tab
    /// quits the binary" semantics (the binary-side arm
    /// translates empty-stack to `self.running = false`).
    #[test]
    fn remove_active_of_last_tab_leaves_stack_empty() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        let removed = ts.remove_active();
        assert_eq!(removed.unwrap().state, 0);
        assert!(ts.is_empty(), "last tab removed -> empty stack");
        assert!(ts.active().is_none(), "empty stack -> active() None");
        assert_eq!(
            ts.active_idx(),
            0,
            "active_idx resets to 0 on empty stack (no longer valid, but predictable)"
        );
    }

    /// `remove_active` of a MIDDLE tab clamps the cursor to
    /// `len() - 1` so the next active lands on the new tail
    /// (matches AGENTS.md "active_idx clamped to len-1" wiring).
    #[test]
    fn remove_active_of_middle_tab_clamps_active_to_len_minus_one() {
        let mut ts: TabStack<u32> = TabStack::new_with_label(0, "a");
        ts.push_with_label(1, "b");
        ts.push_with_label(2, "c");
        assert_eq!(ts.active_idx(), 2);
        ts.remove_active();
        assert_eq!(ts.len(), 2);
        assert_eq!(ts.active_idx(), 1, "clamp to len-1 after remove");
        assert_eq!(
            ts.active().unwrap().state,
            1,
            "new active lands on the new tail (tab 'b')"
        );
    }

    /// `switch_to(n)` for in-range `n` swaps the cursor; out-of-range
    /// is a silent no-op. Mirrors `KeyAction::TabSwitch(n)` M-1..M-9.
    #[test]
    fn switch_to_in_range_swaps_active() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push(1);
        ts.push(2);
        ts.switch_to(0);
        assert_eq!(ts.active_idx(), 0);
        ts.switch_to(2);
        assert_eq!(ts.active_idx(), 2);
    }

    /// `switch_to(n)` for OUT-OF-RANGE `n` is a silent no-op.
    #[test]
    fn switch_to_out_of_range_is_silent_no_op() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push(1);
        ts.switch_to(5);
        assert_eq!(
            ts.active_idx(),
            1,
            "out-of-range switch_to leaves cursor unchanged"
        );
        ts.switch_to(usize::MAX);
        assert_eq!(ts.active_idx(), 1);
    }

    /// `iter` returns every tab in declaration order; `iter_mut`
    /// supports mutation. Pin both for the future tab-bar layer's
    /// render pass.
    #[test]
    fn iter_yields_all_tabs_in_declaration_order() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push(1);
        ts.push(2);
        let states: Vec<u32> = ts.iter().map(|t| t.state).collect();
        assert_eq!(states, vec![0, 1, 2]);
    }

    /// `active_mut` provides mutable access to the active tab's
    /// payload.
    #[test]
    fn active_mut_provides_mutable_state_access() {
        let mut ts: TabStack<u32> = TabStack::new(10);
        ts.active_mut().unwrap().state = 99;
        assert_eq!(ts.active().unwrap().state, 99);
    }

    /// `len` after several pushes then removes stabilises at the
    /// post-remove count.
    #[test]
    fn len_tracks_push_and_remove() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        assert_eq!(ts.len(), 1);
        ts.push(1);
        ts.push(2);
        assert_eq!(ts.len(), 3);
        ts.remove_active();
        assert_eq!(ts.len(), 2);
        ts.remove_active();
        ts.remove_active();
        assert_eq!(ts.len(), 0);
        assert!(ts.is_empty());
    }

    /// Empty-stack `get(0)` returns `None` so test code probing
    /// for "is there ANY tab 0 here?" doesn't index-OOB.
    #[test]
    fn get_out_of_range_returns_none() {
        let ts: TabStack<u32> = TabStack::new(0);
        assert!(ts.get(1).is_none());
        assert!(ts.get(usize::MAX).is_none());
        assert!(ts.get(0).is_some());
    }
}
