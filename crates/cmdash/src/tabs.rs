//! Tab-axis data surface for cmdash's `KeyAction::{TabNew,
//! TabClose, TabSwitch(n)}` runtime primitives.
//!
//! ## Design
//!
//! `Tab<T>` is a single-tab wrapper holding a payload `T` plus a
//! human-readable `label` (the v1 tab title). `TabStack<T>` is a
//! linear stack of Tabs plus an `active_idx` cursor; the cursor is
//! upkept by the `TabStack::remove` / `TabStack::switch_to` /
//! hot-path accessors so callers don't have to re-clamp `active_idx`
//! < `len()` themselves.
//!
//! ## Cross-tab `LayerId` namespace contract
//!
//! The cmdash `cmdash::layer_id` module's
//! `derive_layer_id_for_tab(pane, tab_id)` packs `(tab_id << 32)
//! | pre_order` into a `PaneLayerId`, collision-free across tabs.
//! Every fresh pane spawn in a tab MUST pass the tab's
//! `TabStack::active_idx()` (cast to `u32`) into
//! `derive_layer_id_for_tab`, so two tabs with the same pre-order
//! geometry never alias to the same `LayerId`.
//!
//! `InPlace` survivor rebinds in `TickContext::reconcile_runners`
//! deliberately preserve the survivor's `PaneLayerId` per `AGENTS.md`
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
///    `len() - 1` (matches `AGENTS.md` / cmdash-config `TabClose`
///    rustdoc semantics: "`active_tab` is clamped to `tabs.len()` -
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
    use crate::layer_id::derive_layer_id_for_tab;
    use cmdash_config::{LayoutNode, PaneKind, Ratio as CfgRatio, SplitAxis as CfgSplitAxis};
    use cmdash_layout::{ComputedLayout, PaneId, Rect};
    use cmdash_pty::PaneLayerId;
    // ----------------------------------------------------------------
    // Tab<T> tests.
    // ----------------------------------------------------------------

    /// `Tab::new` wraps `state` with `label = None`.
    #[test]
    fn tab_new_sets_state_and_none_label() {
        let tab = Tab::new(42u32);
        assert_eq!(tab.state, 42);
        assert!(tab.label.is_none(), "Tab::new must set label to None");
    }

    /// `Tab::with_label` wraps `state` with the given label.
    #[test]
    fn tab_with_label_sets_state_and_label() {
        let tab = Tab::with_label(99u32, "hello");
        assert_eq!(tab.state, 99);
        assert_eq!(tab.label.as_deref(), Some("hello"));
    }

    /// `Tab::with_label` accepts `String` as well as `&str`.
    #[test]
    fn tab_with_label_accepts_string() {
        let s = String::from("owned");
        let tab = Tab::with_label(1u32, s);
        assert_eq!(tab.label.as_deref(), Some("owned"));
    }

    /// `Tab::with_label` with an empty string produces
    /// `Some("")`, not `None`. The `render_tab_bar` function
    /// filters these to `None` at render time, but the `Tab`
    /// itself stores the empty string faithfully.
    #[test]
    fn tab_with_empty_string_label_stores_some_empty() {
        let tab = Tab::with_label(1u32, "");
        assert_eq!(
            tab.label.as_deref(),
            Some(""),
            "empty-string label must be Some(\"\"), not None"
        );
    }

    /// `Tab::state` returns a shared reference to the payload.
    #[test]
    fn tab_state_returns_shared_ref() {
        let tab = Tab::new(77u32);
        assert_eq!(tab.state(), &77);
    }

    /// `Tab::state_mut` returns a mutable reference to the
    /// payload; mutations through it are visible via `state()`.
    #[test]
    fn tab_state_mut_allows_mutation() {
        let mut tab = Tab::new(10u32);
        *tab.state_mut() = 99;
        assert_eq!(
            tab.state(),
            &99,
            "mutation via state_mut must be visible via state"
        );
    }

    /// `Tab` derives `Clone`: cloning preserves both `state`
    /// and `label`.
    #[test]
    fn tab_clone_preserves_state_and_label() {
        let tab = Tab::with_label(5u32, "original");
        let cloned = tab.clone();
        assert_eq!(cloned.state, 5);
        assert_eq!(cloned.label.as_deref(), Some("original"));
    }

    /// `Tab` derives `PartialEq` + `Eq`: two tabs with the
    /// same state and label are equal.
    #[test]
    fn tab_equality_compares_state_and_label() {
        let a = Tab::with_label(1u32, "same");
        let b = Tab::with_label(1u32, "same");
        assert_eq!(a, b, "identical tabs must be equal");
    }

    /// Two tabs with different labels are NOT equal, even if
    /// `state` matches.
    #[test]
    fn tab_inequality_on_different_label() {
        let a = Tab::with_label(1u32, "alpha");
        let b = Tab::with_label(1u32, "beta");
        assert_ne!(a, b, "same state but different label must be unequal");
    }

    /// Two tabs with different states are NOT equal, even if
    /// `label` matches.
    #[test]
    fn tab_inequality_on_different_state() {
        let a = Tab::with_label(1u32, "same");
        let b = Tab::with_label(2u32, "same");
        assert_ne!(a, b, "same label but different state must be unequal");
    }

    /// `Tab` with `None` label vs `Some(...)` are NOT equal,
    /// even if `state` matches.
    #[test]
    fn tab_none_label_vs_some_label_are_unequal() {
        let a = Tab::new(1u32);
        let b = Tab::with_label(1u32, "label");
        assert_ne!(
            a, b,
            "None label vs Some(label) must be unequal even with same state"
        );
    }

    /// `Tab` derives `Debug`. Pins that Debug formatting doesn't
    /// panic (useful for test-helper debug-println paths).
    #[test]
    fn tab_debug_does_not_panic() {
        let tab = Tab::with_label(42u32, "debug");
        let dbg = format!("{tab:?}");
        assert!(
            dbg.contains("42") && dbg.contains("debug"),
            "Debug output must contain state and label; got: {dbg}"
        );
    }

    /// `state_mut` on a label-carrying tab mutates state
    /// without affecting the label.
    #[test]
    fn state_mut_preserves_label() {
        let mut tab = Tab::with_label(1u32, "keep");
        *tab.state_mut() = 2;
        assert_eq!(tab.state(), &2);
        assert_eq!(
            tab.label.as_deref(),
            Some("keep"),
            "state mutation must not affect label"
        );
    }

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
    /// (matches `AGENTS.md` "`active_idx` clamped to len-1" wiring).
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
    // ----------------------------------------------------------------
    // Empty-stack edge cases.
    // ----------------------------------------------------------------

    /// `remove_active` on an already-empty stack returns `None`
    /// and leaves the stack empty. Pins the early-return guard
    /// in `remove_active` so a double-close (e.g. `TabClose`
    /// dispatched twice) is a no-op rather than a panic.
    #[test]
    fn remove_active_on_empty_stack_returns_none() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.remove_active();
        assert!(ts.is_empty());
        // Second remove on empty stack.
        assert!(
            ts.remove_active().is_none(),
            "double-remove must return None"
        );
        assert!(ts.is_empty(), "stack stays empty after double-remove");
    }

    /// `switch_to` on an empty stack returns `false`. Pins the
    /// `n < self.tabs.len()` guard against zero-length Vec.
    #[test]
    fn switch_to_on_empty_stack_returns_false() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.remove_active();
        assert!(ts.is_empty());
        assert!(
            !ts.switch_to(0),
            "switch_to(0) on empty stack must return false"
        );
        assert!(
            !ts.switch_to(1),
            "switch_to(1) on empty stack must return false"
        );
    }

    /// `active()` on an empty stack returns `None`. The
    /// `self.tabs.get(self.active_idx)` call uses Vec's
    /// bounds-checked get, which returns None for any index
    /// on an empty Vec.
    #[test]
    fn active_on_empty_stack_returns_none() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.remove_active();
        assert!(
            ts.active().is_none(),
            "active() on empty stack must be None"
        );
    }

    /// `active_mut()` on an empty stack returns `None`.
    /// Symmetric to `active_on_empty_stack_returns_none`.
    #[test]
    fn active_mut_on_empty_stack_returns_none() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.remove_active();
        assert!(
            ts.active_mut().is_none(),
            "active_mut() on empty stack must be None"
        );
    }

    /// `iter()` on an empty stack yields zero items. Pins the
    /// empty-Vec iteration contract.
    #[test]
    fn iter_on_empty_stack_yields_zero_items() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.remove_active();
        assert_eq!(ts.iter().count(), 0, "empty stack iter must yield 0 items");
    }

    /// `get_mut` on an empty stack returns `None`. Mirrors
    /// `get_out_of_range_returns_none` for the mutable path.
    #[test]
    fn get_mut_on_empty_stack_returns_none() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.remove_active();
        assert!(
            ts.get_mut(0).is_none(),
            "get_mut(0) on empty stack must be None"
        );
    }

    // ----------------------------------------------------------------
    // remove_active edge cases.
    // ----------------------------------------------------------------

    /// Remove the FIRST tab (`active_idx=0`) from a 3-tab stack.
    /// After removal, `active_idx` stays at 0 (the old tab 1
    /// slides into position 0). Pins the non-clamping branch
    /// in `remove_active` where `active_idx < tabs.len()`
    /// post-removal.
    #[test]
    fn remove_active_first_tab_slides_remaining_down() {
        let mut ts: TabStack<u32> = TabStack::new(10);
        ts.push(20);
        ts.push(30);
        // active_idx=2 (last pushed). Switch to 0.
        ts.switch_to(0);
        assert_eq!(ts.active_idx(), 0);
        let removed = ts.remove_active();
        assert_eq!(
            removed.unwrap().state,
            10,
            "removed tab must be the first one"
        );
        assert_eq!(ts.len(), 2);
        assert_eq!(
            ts.active_idx(),
            0,
            "active_idx stays 0 after removing the first tab"
        );
        assert_eq!(
            ts.active().unwrap().state,
            20,
            "old tab 1 (state=20) slides into position 0"
        );
    }

    /// Remove when `active_idx=1` from a 3-tab stack (not the
    /// tail). After removal, `active_idx` stays at 1 (the old
    /// tab 2 slides into position 1). Pins the
    /// `active_idx >= tabs.len()` false branch where clamping
    /// is NOT triggered.
    #[test]
    fn remove_active_middle_tab_preserves_idx_when_not_tail() {
        let mut ts: TabStack<u32> = TabStack::new(10);
        ts.push(20);
        ts.push(30);
        ts.switch_to(1);
        assert_eq!(ts.active_idx(), 1);
        let removed = ts.remove_active();
        assert_eq!(
            removed.unwrap().state,
            20,
            "removed tab must be the middle one"
        );
        assert_eq!(ts.len(), 2);
        assert_eq!(
            ts.active_idx(),
            1,
            "active_idx stays 1 when not at the tail"
        );
        assert_eq!(
            ts.active().unwrap().state,
            30,
            "old tab 2 (state=30) slides into position 1"
        );
    }

    /// Chained removes: remove tabs one by one from a 5-tab
    /// stack, always removing the active tab (which is the
    /// last pushed). After each remove, `active_idx` is clamped
    /// to len-1. Pins the clamping invariant across a sequence
    /// of removes.
    #[test]
    fn chained_removes_clamp_active_each_time() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push(1);
        ts.push(2);
        ts.push(3);
        ts.push(4);
        // active_idx=4, len=5
        assert_eq!(ts.active_idx(), 4);

        ts.remove_active(); // remove 4, clamp to 3
        assert_eq!(ts.len(), 4);
        assert_eq!(ts.active_idx(), 3);
        assert_eq!(ts.active().unwrap().state, 3);

        ts.remove_active(); // remove 3, clamp to 2
        assert_eq!(ts.len(), 3);
        assert_eq!(ts.active_idx(), 2);
        assert_eq!(ts.active().unwrap().state, 2);

        ts.remove_active(); // remove 2, clamp to 1
        assert_eq!(ts.len(), 2);
        assert_eq!(ts.active_idx(), 1);
        assert_eq!(ts.active().unwrap().state, 1);

        ts.remove_active(); // remove 1, clamp to 0
        assert_eq!(ts.len(), 1);
        assert_eq!(ts.active_idx(), 0);
        assert_eq!(ts.active().unwrap().state, 0);

        ts.remove_active(); // remove last, empty
        assert!(ts.is_empty());
        assert_eq!(ts.active_idx(), 0);
        assert!(ts.active().is_none());
    }

    // ----------------------------------------------------------------
    // switch_to edge cases.
    // ----------------------------------------------------------------

    /// `switch_to(0)` when currently at a different tab returns
    /// `true` and moves the cursor. Pins the happy-path return
    /// value contract.
    #[test]
    fn switch_to_returns_true_on_success() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push(1);
        ts.push(2);
        assert_eq!(ts.active_idx(), 2);
        assert!(ts.switch_to(0), "switch_to(0) must return true");
        assert_eq!(ts.active_idx(), 0);
        assert!(ts.switch_to(1), "switch_to(1) must return true");
        assert_eq!(ts.active_idx(), 1);
    }

    /// `switch_to` to the CURRENT active tab returns `true` and
    /// is a no-op (cursor stays). Pins that self-swap is
    /// idempotent.
    #[test]
    fn switch_to_current_tab_is_idempotent() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push(1);
        ts.switch_to(0);
        assert_eq!(ts.active_idx(), 0);
        assert!(ts.switch_to(0), "switch_to(current) must return true");
        assert_eq!(ts.active_idx(), 0, "cursor stays on self-swap");
    }

    /// `switch_to` returns `false` for out-of-range n. Pins the
    /// return-value contract for the no-op branch.
    #[test]
    fn switch_to_returns_false_for_out_of_range() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        assert!(
            !ts.switch_to(1),
            "switch_to(1) on 1-tab stack must return false"
        );
        assert!(
            !ts.switch_to(usize::MAX),
            "switch_to(MAX) must return false"
        );
    }

    /// `switch_to` followed by `remove_active` then `switch_to`
    /// back. Exercises the full cycle: switch, remove (which
    /// may shift indices), switch again. Pins that the
    /// index-space is coherent after mutation.
    #[test]
    fn switch_remove_switch_cycle() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push(1);
        ts.push(2);
        ts.push(3);
        // Switch to tab 1, remove it, then switch to tab 0.
        ts.switch_to(1);
        assert_eq!(ts.active().unwrap().state, 1);
        ts.remove_active(); // removes state=1; remaining: [0, 2, 3]
        assert_eq!(ts.len(), 3);
        assert_eq!(ts.active_idx(), 1); // clamped: was 1, len=3, 1 < 3
        assert_eq!(ts.active().unwrap().state, 2); // old tab 2 slides to idx 1
        ts.switch_to(0);
        assert_eq!(ts.active().unwrap().state, 0);
    }

    // ----------------------------------------------------------------
    // Label preservation edge cases.
    // ----------------------------------------------------------------

    /// Labels survive `remove_active`: removing a tab with a
    /// label does not corrupt the labels of remaining tabs.
    #[test]
    fn labels_preserved_through_remove_active() {
        let mut ts: TabStack<u32> = TabStack::new_with_label(0, "alpha");
        ts.push_with_label(1, "beta");
        ts.push_with_label(2, "gamma");
        // Remove the active tab (gamma, idx=2).
        ts.remove_active();
        assert_eq!(ts.len(), 2);
        // Remaining tabs: alpha (idx 0), beta (idx 1).
        assert_eq!(ts.get(0).unwrap().label.as_deref(), Some("alpha"));
        assert_eq!(ts.get(1).unwrap().label.as_deref(), Some("beta"));
    }

    /// Removing the first tab preserves the labels of the
    /// remaining tabs (they slide down).
    #[test]
    fn remove_first_tab_preserves_sibling_labels() {
        let mut ts: TabStack<u32> = TabStack::new_with_label(0, "first");
        ts.push_with_label(1, "second");
        ts.push_with_label(2, "third");
        ts.switch_to(0);
        ts.remove_active(); // removes "first"
        assert_eq!(ts.get(0).unwrap().label.as_deref(), Some("second"));
        assert_eq!(ts.get(1).unwrap().label.as_deref(), Some("third"));
    }

    // ----------------------------------------------------------------
    // iter_mut and get_mut edge cases.
    // ----------------------------------------------------------------

    /// `iter_mut` allows mutation of all tabs. Pins the mutable
    /// iteration contract.
    #[test]
    fn iter_mut_allows_mutation_of_all_tabs() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push(1);
        ts.push(2);
        for tab in ts.iter_mut() {
            tab.state += 10;
        }
        let states: Vec<u32> = ts.iter().map(|t| t.state).collect();
        assert_eq!(states, vec![10, 11, 12], "iter_mut must mutate all tabs");
    }

    /// `get_mut` on a valid index provides mutable access.
    #[test]
    fn get_mut_on_valid_index_provides_mutable_access() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push(1);
        ts.get_mut(0).unwrap().state = 99;
        assert_eq!(ts.get(0).unwrap().state, 99);
    }

    /// `get_mut` on an out-of-range index returns `None`.
    #[test]
    fn get_mut_out_of_range_returns_none() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        assert!(ts.get_mut(1).is_none());
        assert!(ts.get_mut(usize::MAX).is_none());
    }
    // ----------------------------------------------------------------
    // Property-based invariant tests.
    // ----------------------------------------------------------------

    /// Invariant checker: after every mutation, the following
    /// must hold:
    /// 1. If non-empty: `active_idx < len`
    /// 2. `is_empty() == (len() == 0)`
    /// 3. `active().is_some() == !is_empty()`
    ///
    /// (`active_mut` invariant is tested separately via
    /// `active_mut_on_empty_stack_returns_none` — cannot
    /// check from an immutable `&TabStack` ref.)
    fn assert_invariants<T: std::fmt::Debug>(ts: &TabStack<T>) {
        if ts.is_empty() {
            assert_eq!(ts.len(), 0, "is_empty implies len==0");
            assert!(ts.active().is_none(), "empty -> active() None");
        } else {
            assert!(
                ts.active_idx() < ts.len(),
                "active_idx ({}) must be < len ({})",
                ts.active_idx(),
                ts.len()
            );
            assert!(ts.active().is_some(), "non-empty -> active() Some");
        }
    }

    /// Seeded PRNG (xorshift32) for deterministic randomized
    /// testing. Fixed seed ensures reproducibility across runs.
    struct Rng {
        state: u32,
    }

    impl Rng {
        fn new(seed: u32) -> Self {
            Self { state: seed.max(1) }
        }

        fn next_u32(&mut self) -> u32 {
            debug_assert!(self.state != 0, "Rng degenerated to all-zeros");
            self.state ^= self.state << 13;
            self.state ^= self.state >> 17;
            self.state ^= self.state << 5;
            self.state
        }

        /// Uniform in [0, bound).
        fn range(&mut self, bound: u32) -> u32 {
            if bound == 0 {
                0
            } else {
                self.next_u32() % bound
            }
        }
    }

    /// Randomized invariant test: perform 2000 random operations
    /// (`push`, `remove_active`, `switch_to`) on a `TabStack` and assert
    /// invariants after every mutation. The seed is fixed for
    /// reproducibility; changing the seed explores a different
    /// operation sequence.
    #[test]
    fn randomized_invariant_check_2000_operations() {
        let mut rng = Rng::new(0xDEAD_BEEF);
        let mut ts: TabStack<u32> = TabStack::new(0);
        assert_invariants(&ts);

        let mut next_state: u32 = 1;
        for _ in 0..2000 {
            let op = rng.range(3);
            match op {
                0 => {
                    // push
                    let idx = ts.push(next_state);
                    assert_eq!(idx, ts.len() - 1, "push must return new tab index");
                    next_state += 1;
                }
                1 => {
                    // remove_active
                    let was_empty = ts.is_empty();
                    let old_len = ts.len();
                    let removed = ts.remove_active();
                    if was_empty {
                        assert!(removed.is_none(), "remove on empty returns None");
                    } else {
                        assert!(removed.is_some(), "remove on non-empty returns Some");
                        assert_eq!(ts.len(), old_len - 1);
                    }
                }
                2 => {
                    // switch_to with random target
                    let target = if ts.is_empty() {
                        rng.range(10) as usize
                    } else {
                        // Mix of in-range and out-of-range targets.
                        let max_target = ts.len() + 3;
                        rng.range(max_target as u32) as usize
                    };
                    let _ = ts.switch_to(target);
                }
                _ => unreachable!(),
            }
            assert_invariants(&ts);
        }
    }

    /// Targeted invariant test: exercise the push-then-switch
    /// cycle. After every push, `switch_to` every valid index and
    /// assert invariants. This exercises the interaction between
    /// `push` (which sets active to the new tail) and `switch_to`
    /// (which moves it elsewhere).
    #[test]
    fn push_then_switch_to_every_index_invariant() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        assert_invariants(&ts);

        for i in 1..=20u32 {
            ts.push(i);
            assert_invariants(&ts);
            // switch_to every valid index.
            for j in 0..ts.len() {
                ts.switch_to(j);
                assert_eq!(ts.active_idx(), j, "switch_to({j}) must land on {j}");
                assert_invariants(&ts);
            }
        }
    }

    /// Exhaustive remove test: push N tabs, then remove all
    /// one by one, asserting invariants after each removal.
    /// After the last removal, the stack must be empty.
    #[test]
    fn push_n_then_remove_all_invariant() {
        for n in 1..=50u32 {
            let mut ts: TabStack<u32> = TabStack::new(0);
            for i in 1..n {
                ts.push(i);
            }
            assert_eq!(ts.len(), n as usize);
            assert_invariants(&ts);

            // Remove all tabs.
            for _ in 0..n {
                ts.remove_active();
                assert_invariants(&ts);
            }
            assert!(
                ts.is_empty(),
                "after removing all {n} tabs, stack must be empty"
            );
            assert_eq!(ts.len(), 0);
            assert!(ts.active().is_none());
        }
    }

    /// Alternating push/remove stress test: alternate between
    /// pushing and removing tabs. The stack size oscillates;
    /// invariants must hold at every step.
    #[test]
    fn alternating_push_remove_invariant() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        let mut next_state: u32 = 1;

        for _ in 0..500 {
            // Push 1-3 tabs.
            let push_count = (next_state % 3) + 1;
            for _ in 0..push_count {
                ts.push(next_state);
                next_state += 1;
                assert_invariants(&ts);
            }
            // Remove 1-2 tabs.
            let remove_count = (next_state % 2) + 1;
            for _ in 0..remove_count {
                ts.remove_active();
                assert_invariants(&ts);
            }
        }
    }

    /// Randomized mutation + `switch_to` invariant: 1000 random
    /// operations (`push`, `remove`, `switch_to`). When `switch_to` is
    /// chosen (op==2), verifies the target matches `active_idx`.
    /// Invariants are checked after every operation regardless
    /// of op type.
    #[test]
    fn randomized_mutation_with_switch_to_invariant() {
        let mut rng = Rng::new(0xCAFE_BABE);
        let mut ts: TabStack<u32> = TabStack::new(0);
        let mut next_state: u32 = 1;

        for _ in 0..1000 {
            let op = rng.range(3);
            match op {
                0 => {
                    ts.push(next_state);
                    next_state += 1;
                }
                1 => {
                    ts.remove_active();
                }
                2 => {
                    // After any mutation, switch to every valid
                    // index and verify active_idx matches.
                    if !ts.is_empty() {
                        let target = rng.range(ts.len() as u32) as usize;
                        ts.switch_to(target);
                        assert_eq!(
                            ts.active_idx(),
                            target,
                            "switch_to({target}) must set active_idx"
                        );
                    }
                }
                _ => unreachable!(),
            }
            assert_invariants(&ts);
        }
    }

    // ----------------------------------------------------------------
    // Cross-tab LayerId namespace contract tests.
    //
    // Per AGENTS.md §"Tabs" and tabs.rs module docs: every fresh
    // pane spawn in a tab MUST pass the tab's
    // `TabStack::active_idx()` (cast to `u32`) into
    // `derive_layer_id_for_tab`, so two tabs with the same
    // pre-order geometry never alias to the same `LayerId`.
    //
    // These tests exercise the contract through the `TabStack` API
    // to pin that `active_idx` → `tab_id` casting produces
    // collision-free `LayerId` values across tabs.
    // ----------------------------------------------------------------

    /// Same `PaneId` (`pre_order`=0) on two different tab indices
    /// (via `TabStack::active_idx`) produces distinct `LayerId`s.
    /// This is the fundamental cross-tab namespace invariant and
    /// exercises the production contract: every fresh pane spawn
    /// in a tab passes `active_idx() as u32` into
    /// `derive_layer_id_for_tab`.
    #[test]
    fn cross_tab_same_pane_different_tab_indices_distinct_layer_ids() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push(1);
        // Stack: [tab0, tab1], active = 1.

        let pane = PaneId::default(); // pre_order = 0

        // Derive LayerIds via the production contract: active_idx() as u32.
        ts.switch_to(0);
        let id_tab0 = derive_layer_id_for_tab(&pane, ts.active_idx() as u32);
        ts.switch_to(1);
        let id_tab1 = derive_layer_id_for_tab(&pane, ts.active_idx() as u32);

        assert_ne!(
            id_tab0, id_tab1,
            "same PaneId on different tabs (via active_idx) must produce distinct LayerIds"
        );
    }

    /// Collision sweep: for 100 tab indices, the same `PaneId`
    /// must produce 100 distinct `LayerId` values. This catches
    /// packing bugs where the high-32-bit `tab_id` field bleeds
    /// into the low-32-bit `pre_order` field (e.g. missing shift,
    /// wrong mask).
    #[test]
    fn cross_tab_collision_sweep_100_tabs_same_pane() {
        let pane = PaneId::default(); // pre_order = 0
        let mut seen = std::collections::HashSet::new();
        for tab_id in 0u32..100 {
            let layer_id = derive_layer_id_for_tab(&pane, tab_id);
            assert!(
                seen.insert(layer_id),
                "LayerId collision at tab_id={tab_id}: {layer_id:?} already in set"
            );
        }
        assert_eq!(
            seen.len(),
            100,
            "100 distinct tab_ids must yield 100 distinct LayerIds"
        );
    }

    /// Full matrix: M `tab_id`s × N panes must all produce distinct
    /// `LayerId` values. The packing `(tab_id << 32) | pre_order`
    /// means each (`tab_id`, `pre_order`) pair maps to a unique u64;
    /// this test verifies no two cells in the matrix alias.
    #[test]
    fn cross_tab_full_matrix_no_collision() {
        // Build a 3-pane layout to get 3 distinct pre_order values.
        let layout_root = LayoutNode::Split {
            axis: CfgSplitAxis::Horizontal,
            ratio: CfgRatio(50),
            children: vec![
                LayoutNode::Split {
                    axis: CfgSplitAxis::Vertical,
                    ratio: CfgRatio(50),
                    children: vec![
                        LayoutNode::Pane(cmdash_config::Pane {
                            kind: PaneKind::Shell,
                            label: Some("a".into()),
                            command: None,
                        }),
                        LayoutNode::Pane(cmdash_config::Pane {
                            kind: PaneKind::Shell,
                            label: Some("b".into()),
                            command: None,
                        }),
                    ],
                },
                LayoutNode::Pane(cmdash_config::Pane {
                    kind: PaneKind::Shell,
                    label: Some("c".into()),
                    command: None,
                }),
            ],
        };
        let area = Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let layout = ComputedLayout::compute(&layout_root, area).expect("compute 3-pane layout");
        assert_eq!(layout.panes.len(), 3, "fixture must produce 3 leaf panes");

        let pre_orders: Vec<u32> = layout.panes.iter().map(|p| p.id.pre_order()).collect();
        // Verify the fixture gives us 3 distinct pre_order values.
        let distinct_pre_orders: std::collections::HashSet<u32> =
            pre_orders.iter().copied().collect();
        assert_eq!(
            distinct_pre_orders.len(),
            3,
            "3 panes must have 3 distinct pre_orders"
        );

        // 10 tab_ids × 3 panes = 30 unique (tab_id, pre_order) pairs.
        let num_tabs = 10u32;
        let mut seen = std::collections::HashSet::new();
        for tab_id in 0..num_tabs {
            for pane in &layout.panes {
                let layer_id = derive_layer_id_for_tab(&pane.id, tab_id);
                assert!(
                    seen.insert(layer_id),
                    "collision at (tab_id={tab_id}, pre_order={}) -> {layer_id:?}",
                    pane.id.pre_order()
                );
            }
        }
        assert_eq!(
            seen.len(),
            (num_tabs as usize) * 3,
            "full matrix must produce (num_tabs × num_panes) distinct LayerIds"
        );
    }

    /// Packing math verification: `derive_layer_id_for_tab` must
    /// pack `(tab_id << 32) | pre_order` exactly. This catches
    /// off-by-one shifts, endianness bugs, or sign-extension
    /// issues in the u64 packing.
    #[test]
    fn cross_tab_packing_math_exact() {
        let pane = PaneId::default(); // pre_order = 0
                                      // (tab_id=0, pre_order=0) → raw = 0x0000_0000_0000_0000
        let id0 = derive_layer_id_for_tab(&pane, 0);
        assert_eq!(id0, PaneLayerId(0), "(0, 0) must pack to 0");

        // (tab_id=1, pre_order=0) → raw = 0x0000_0001_0000_0000
        let id1 = derive_layer_id_for_tab(&pane, 1);
        assert_eq!(id1, PaneLayerId(1u64 << 32), "(1, 0) must pack to 1 << 32");

        // (tab_id=0, pre_order=42) — need a pane with pre_order=42.
        // We can't easily get pre_order=42 without a 42-leaf tree,
        // but we CAN verify the high-bit boundary: tab_id=0x7FFF_FFFF
        // (u32::MAX >> 1) is the largest safe tab_id.
        let id_max_tab = derive_layer_id_for_tab(&pane, u32::MAX >> 1);
        let expected_max = ((u32::MAX >> 1) as u64) << 32;
        assert_eq!(
            id_max_tab,
            PaneLayerId(expected_max),
            "largest safe tab_id must pack correctly into high 32 bits"
        );
    }

    /// `TabStack` lifecycle + `LayerId` contract: push/remove/switch
    /// operations must not corrupt the `tab_id` → `LayerId` mapping.
    /// After a push-remove-switch cycle, the surviving tab's
    /// `LayerId` derivation must still be collision-free.
    #[test]
    fn cross_tab_lifecycle_preserves_layer_id_contract() {
        let mut ts: TabStack<u32> = TabStack::new(0);
        ts.push(1);
        ts.push(2);
        // Stack: [0, 1, 2], active = 2.

        let pane = PaneId::default();

        // Switch to tab 0 first, then derive its LayerId.
        ts.switch_to(0);
        let id_tab0 = derive_layer_id_for_tab(&pane, ts.active_idx() as u32);
        // Switch away and back — LayerId must not change.
        ts.switch_to(2);
        ts.switch_to(0);
        let id_tab0_recheck = derive_layer_id_for_tab(&pane, ts.active_idx() as u32);
        assert_eq!(
            id_tab0, id_tab0_recheck,
            "switch away and back must not change LayerId for same (pane, tab)"
        );

        // Remove tab 1 (middle). Stack: [0, 2].
        ts.switch_to(1);
        ts.remove_active();
        // Tab 2 is now at index 1.
        assert_eq!(ts.len(), 2);
        assert_eq!(ts.active_idx(), 1);

        // Tab 0's LayerId derivation must be unaffected.
        ts.switch_to(0);
        let id_tab0_post = derive_layer_id_for_tab(&pane, ts.active_idx() as u32);
        assert_eq!(
            id_tab0, id_tab0_post,
            "removing a sibling tab must not change tab0's LayerId"
        );

        // Tab 2 (now at index 1) must still be distinct from tab 0.
        ts.switch_to(1);
        let id_tab2 = derive_layer_id_for_tab(&pane, ts.active_idx() as u32);
        assert_ne!(
            id_tab0, id_tab2,
            "tab0 and tab2 LayerIds must remain distinct after lifecycle mutations"
        );
    }
}
