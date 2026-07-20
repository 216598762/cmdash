//! Tick context for the cmdash session state and event handling.
//!
//! This module holds the session-state management, layout mutation,
//! event dispatch, and host-terminal synchronization logic extracted
//! from `main.rs` as part of the Milestone 1 session-persistence
//! split. The monolithic tick loop (`run`/`run_loop`/`tick_and_render`)
//! has been replaced by [`crate::ServerTask`] + [`crate::FrontendTask`];
//! the methods here are retained for testability and for the helper
//! functions that both `TickContext` tests and `ServerTask` share.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::Terminal;
use tracing::{debug, info, warn};

use crate::clipboard::copy_text_to_clipboard;
use crate::graphics::GraphicsState;
use crate::pane::{PaneCloseTx, PaneRunner};
use crate::protocol::{ConfigReload, CopyModeState, WidgetFactories};
use crate::render::extract_selected_text;
use crate::tabs::TabStack;
use cmdash_config::{
    KeyAction, LayoutNode, Pane as CfgPane, PaneKind, Ratio as CfgRatio, SplitAxis as CfgSplitAxis,
};
use cmdash_keybinds::Router;
use cmdash_layout::{
    adjacent_pane, remove_leaf, replace_leaf_with_split, ComputedLayout, Direction, PaneId,
    Rect as LayoutRect,
};
use cmdash_pty::PaneLayerId;
use cmdash_pty::ShellSpec;

/// Convert a per-pane `command` override into a [`ShellSpec`].
/// `None` falls back to `default` (typically [`ShellSpec::LoginShell`]
/// for initial spawns, or `self.shell` for runtime-spawned panes).
/// `Some(cmd)` splits by whitespace into argv and spawns `argv[0]`
/// with `argv[1..]` as arguments. Called from `run()` and
/// `reconcile_runners` when spawning a fresh pane.
///
/// **Limitation:** The command is split on whitespace via
/// `str::split_whitespace()`. This means shell metacharacters
/// (pipes, redirects, quoted arguments with spaces) are NOT
/// supported. For example, `command="echo 'hello world'"`
/// produces `["echo", "'hello", "world'"]` (broken quoting).
/// This is an acceptable v1 limitation — users should avoid
/// shell metacharacters in the `command` field.
///
/// Moved here from the binary crate (`main.rs`) as part of the
/// Milestone 1 session-persistence extraction so the tick loop
/// can call it when reconciling runners after layout mutations.
pub fn shell_spec_from_command(command: &Option<String>, default: &ShellSpec) -> ShellSpec {
    match command {
        Some(cmd) => {
            let argv: Vec<String> = cmd.split_whitespace().map(String::from).collect();
            if argv.is_empty() {
                default.clone()
            } else {
                ShellSpec::Command { argv }
            }
        }
        None => default.clone(),
    }
}

/// Number of terminal rows reserved for the tab bar at the top of
/// the screen. The layout area's height is reduced by this amount
/// so panes don't overlap the tab bar. The tab bar is rendered in
/// phase 3a after pane blits into row 0 of the ratatui buffer.
/// When only 1 row of terminal height is available, panes are
/// skipped entirely (the tab bar alone fills the screen).
pub const TAB_BAR_HEIGHT: u16 = 1;

/// Number of terminal rows reserved for the status bar. The status
/// bar is optional — when disabled (the default), this constant
/// contributes 0 rows to the layout offset. When enabled, the
/// layout area's height is reduced by this amount and the status
/// bar is rendered in phase 3a after pane blits.
pub const STATUS_BAR_HEIGHT: u16 = 1;

pub struct TabState {
    /// Per-tab pane Vec. Clone-shells (pty-less) per the
    /// [`crate::pane::PaneRunner`] manual `Clone` impl; the
    /// authoritative real runners are the v1 field's
    /// `Vec<PaneRunner>` (see the doc above).
    pub runners: Vec<PaneRunner>,
    /// Per-tab focused-pane index.
    pub focus: usize,
    /// Per-tab KDL layout tree.
    pub layout_root: LayoutNode,
    /// Per-tab `ZStack` focus map (per the v1 `stack_focus`
    /// semantics).
    pub stack_focus: BTreeMap<cmdash_layout::PaneId, usize>,
}

/// Active drag-to-resize state for Alt+drag on split panes.
///
/// Tracks the initial mouse position, the Split node being
/// resized, and the ratio at the start of the drag so each
/// Drag event can compute the delta and update the tree.
pub struct DragState {
    /// Tree path (child indices from root) to the Split node
    /// whose ratio is being adjusted. Fixed-size array backed
    /// by `MAX_TREE_DEPTH` (8).
    pub split_path: [u16; 8],
    /// Number of valid elements in `split_path`.
    pub split_path_len: u8,
    /// Initial mouse column (for Horizontal splits) or row
    /// (for Vertical splits) at drag start.
    pub start_pos: u16,
    /// Ratio of the Split node at drag start.
    pub initial_ratio: u8,
    /// The Split's axis — determines which mouse coord maps
    /// to the ratio.
    pub axis: cmdash_config::SplitAxis,
    /// Total cells along the Split axis (parent rect width for
    /// Horizontal, height for Vertical). Used to convert pixel
    /// deltas to percentage changes.
    pub total_cells: u16,
}

/// Pivot struct for one tick of the AGENTS.md rendering pipeline.
///
/// Bundles the ten per-frame arguments of the prior free
/// function `tick_loop` into one struct so `crate::run` calls
/// `TickContext::run` as a single-shot pipeline call instead of
/// threading individual references through a 9-argument
/// function (which tripped `clippy::too_many_arguments`).
///
/// All fields are **owned** except `terminal`, which is borrowed
/// from a surrounding [`TerminalGuard`] whose `Drop` reverts the
/// alt-screen and mouse-capture on exit. The other nine are
/// owned because `crate::run` builds the struct once and
/// runs the loop to completion — there is no caller that needs
/// post-loop access to the runners, graphics, or bindings.
///
/// AGENTS.md §"Rendering pipeline (one frame)" enumerates the
/// six tick phases (input, drain, snapshot, event route,
/// ratatui draw, termcompositor emit, sleep). The field names
/// mirror those phases: `runners` + `bindings` + `focus` +
/// `running` are phase 0/1/2 inputs; `close_rx` + `graphics` +
/// `tick` are phase 1/2/3b/4 resources; `terminal` is phase 3a;
/// `layout_root` + `pending_resize` drive phase 0.5 (host
/// SIGWINCH relayout).
pub struct TickContext<'a, B: ratatui::backend::Backend> {
    /// All live panes (phase 0 input + phase 3a layout source).
    pub runners: Vec<PaneRunner>,
    /// Crossterm keybind router (phase 0 input).
    pub bindings: Router,
    /// Focused-pane index (phase 0/2 focus tracking).
    pub focus: usize,
    /// Set to `false` by an action handler to quit the loop.
    pub running: bool,
    /// Unbounded MPSC receiver of `PaneRunner::Drop` close notifications;
    /// drained at the start of phase 1.
    // TODO: remove — only read by `drain_close_channel` which is now
    // test-only after run()/run_loop() extraction.
    #[allow(dead_code)]
    pub close_rx: UnboundedReceiver<cmdash_pty::PaneLayerId>,
    /// termcompositor layer book-keeping (phase 1 revoke +
    /// phase 2/3b update).
    pub graphics: GraphicsState,
    /// ratatui terminal borrowed from a [`TerminalGuard`]; the
    /// guard's `Drop` reverts alt-screen + mouse-capture on
    /// exit, so the borrow lifetime is tied to the guard.
    // TODO: remove — no longer used after run()/run_loop() extraction
    // to ServerTask + FrontendTask. Retained only so `new_full` and
    // existing tests compile without churn.
    #[allow(dead_code)]
    pub terminal: &'a mut Terminal<B>,
    /// Per-tick pacing knob (phase 4 sleep duration).
    // TODO: remove — was only used by run_loop's tick interval.
    // Retained only so `new_full` and existing tests compile.
    #[allow(dead_code)]
    pub tick: Duration,
    /// Owned copy of the KDL layout tree, consumed by
    /// [`ComputedLayout::compute`] on every host-driven resize.
    /// Held by value so [`Self::relayout`] does not need to
    /// borrow from `crate::run`'s stack after construction.
    /// `AGENTS.md` `§` "`PaneId` stability" — moving the tree by value
    /// does not shift pre-order leaf numbering, so the layout
    /// engine produces the same `cmdash_layout::PaneId`
    /// values before and after a relayout.
    pub layout_root: LayoutNode,
    /// Coalesced (cols, rows) of the most recent crossterm
    /// `Event::Resize`. Empty during normal ticks; consumed
    /// (via `take()`) at the start of phase 0.5 so successive
    /// resize signals naturally coalesce — only the LATEST
    /// (cols, rows) reaches [`Self::relayout`] this tick.
    pub pending_resize: Option<(u16, u16)>,
    /// Owned clone of the binary's paired close sender. Retained
    /// so the runtime mutation paths (`AppNewPane` reconciliation,
    /// `PanePreset` rebuild) can wire fresh `PaneRunner`s into
    /// the SAME close-channel as the initial-frame spawn, preserving
    /// the Drop -> `close_tx` -> `GraphicsState`::`close_pane` round-trip.
    /// AGENTS.md §"Hard rule: one layer per instance" (`a` `LayerId` is
    /// bound to a pane instance for the instance's whole lifetime
    /// and is NEVER re-bound to a different pane).
    pub close_tx: UnboundedSender<cmdash_pty::PaneLayerId>,
    /// Last non-zero cell-grid area against which `relayout`
    /// succeeded. Used as the resolution target for runtime
    /// mutations (`AppNewPane`, `PaneClose`, `PanePreset`) when
    /// a SIGWINCH hasn't yet signalled. Defaults to (80, 24) on
    /// a zero-area initial-frame transient.
    pub last_area: LayoutRect,
    /// Saved layout bodies keyed by their KDL `name`. Populated
    /// from `cmdash_config::Config::presets` at startup. The
    /// `PanePreset(name)` runtime mutation looks up
    /// `self.presets[name]` and wholesale-swaps `self.layout_root`
    /// for the new tree.
    pub presets: BTreeMap<String, LayoutNode>,
    /// Phase 4 carry-forward: per-ZStack focus tracking. Maps
    /// the focused `ZStack` member's resolved [`cmdash_layout::PaneId`]
    /// to its index within the parent `ZStack`. Survives across
    /// `AppNewPane`/`PaneClose` `InPlace` cycles (label-keyed
    /// reconciliation preserves the member's `PaneId` when the
    /// sibling stays under the same Split/ZStack parent);
    /// cleared on `Wholesale` swap (`PanePreset`)
    /// reconciliation so a reloaded preset's stale `PaneIds`
    /// don't linger in the map.
    pub stack_focus: BTreeMap<PaneId, usize>,
    /// Default shell for runtime-spawned panes. v1 single shell
    /// (`LoginShell`) — `crate::run` wires the constant. A future
    /// per-pane shell override slots in here.
    pub shell: ShellSpec,
    /// Active drag-to-resize state (Alt+drag on any pane).
    pub drag_state: Option<DragState>,
    /// Latest snapshot of the focused pane captured during the
    /// most recent tick. Kept so copy-mode can read the focused
    /// pane's text grid without re-advancing the PTY state
    /// machine. Only the focused pane is retained to avoid
    /// cloning every pane's grid every tick.
    pub last_focused_snapshot: Option<cmdash_pty::PaneTerminalState>,
    /// Active copy-mode state. When `Some`, the user is
    /// selecting text in the focused pane to copy to the
    /// system clipboard.
    pub copy_mode: Option<CopyModeState>,
    /// Per-tab payload stack. The v1 singular `runners` /
    /// `focus` / `layout_root` / `stack_focus` fields above
    /// are unaffected by the tab-axis actions; the call sites
    /// that read them directly continue to work. The `tabs`
    /// field is authoritative ONLY for the tab-axis actions
    /// (`KeyAction::TabNew` / `TabClose` / `TabSwitch(n)`),
    /// which mutate `self.tabs` and then call
    /// [`Self::sync_v1_from_active_tab`] + [`Self::reconcile_runners`]
    /// to bring the v1 fields in line with the new active
    /// tab. Initial-frame state is a 1-tab stack with the
    /// initial `runners` (cloned shells) / `focus` /
    /// `layout_root` / `stack_focus` so the v1 + tabs
    /// surfaces are coherent at construction.
    pub tabs: TabStack<TabState>,
    /// Config hot-reload channel receiver.
    // TODO: remove — was only used by run_loop's tokio::select! branch.
    #[allow(dead_code)]
    pub config_reload_rx: Option<UnboundedReceiver<ConfigReload>>,
    /// Optional status bar configuration. When `None`, no status
    /// bar is rendered. When `Some(Bar)`, a single row is reserved
    /// and the status bar is rendered in phase 3a.
    pub status_bar: Option<cmdash_config::Bar>,
    /// Optional theme configuration. When `None`, hardcoded default
    /// colors are used for the tab bar, status bar, and widget borders.
    pub theme: cmdash_config::Theme,
    /// Loaded widget libraries keyed by widget `ref_name`. Populated
    /// by [`load_widgets`] at startup; used by [`Self::reconcile_runners`]
    /// when spawning `PaneKind::Widget` panes.
    pub widget_factories: WidgetFactories,
    /// Current Kitty keyboard protocol progressive-enhancement
    /// flags advertised to the host terminal. The union of all
    /// live pane flags; recomputed every tick after pane events
    /// are drained.
    // TODO: only read by sync_host_keyboard_flags (test-only after extraction).
    #[allow(dead_code)]
    pub host_keyboard_flags: u8,
    /// Per-pane keyboard enhancement flags, keyed by layer id.
    /// Updated from `PaneEvent::KeyboardEnhancement` events and
    /// pruned when a pane closes.
    #[allow(dead_code)]
    pub pane_keyboard_flags: HashMap<PaneLayerId, u8>,
    /// Whether we have pushed a keyboard enhancement entry onto
    /// the host terminal's stack. Used to pop exactly once on
    /// exit so the host returns to its prior state.
    #[allow(dead_code)]
    pub host_keyboard_pushed: bool,
    /// Per-pane bracketed-paste mode state, keyed by layer id.
    /// Updated from `PaneEvent::BracketedPaste` events and pruned
    /// when a pane closes.
    pub pane_bracketed_paste: HashMap<PaneLayerId, bool>,
    /// Whether any pane currently has bracketed paste enabled.
    /// Mirrors the union of `pane_bracketed_paste` and drives
    /// host `CSI ? 2004 h` / `CSI ? 2004 l` synchronization.
    #[allow(dead_code)]
    pub host_bracketed_paste: bool,
    /// Whether we have enabled bracketed paste on the host
    /// terminal. Used to disable exactly once on exit.
    #[allow(dead_code)]
    pub host_bracketed_paste_pushed: bool,
    /// Per-pane focus-reporting mode state, keyed by layer id.
    /// Updated from `PaneEvent::FocusReporting` events and pruned
    /// when a pane closes.
    // TODO: only read by drain_close_channel (test-only after extraction).
    #[allow(dead_code)]
    pub pane_focus_reporting: HashMap<PaneLayerId, bool>,
    /// Whether any pane currently has focus reporting enabled.
    /// Mirrors the union of `pane_focus_reporting` and drives
    /// host focus-change event synchronization.
    #[allow(dead_code)]
    pub host_focus_reporting: bool,
    /// Whether we have enabled focus-change reporting on the host
    /// terminal. Used to disable exactly once on exit.
    #[allow(dead_code)]
    pub host_focus_reporting_pushed: bool,
    /// Per-pane alternate-screen state, keyed by layer id.
    /// Updated from `PaneEvent::AlternateScreen` events and pruned
    /// when a pane closes.
    // TODO: only read by update_alternate_screen_from_snapshots (removed).
    #[allow(dead_code)]
    pub pane_alternate_screen: HashMap<PaneLayerId, bool>,
    /// Whether the host terminal currently has focus. Drives
    /// forwarding of `CSI I` / `CSI O` to the focused pane.
    pub host_focused: bool,
}

/// Monotonic `LayerId` allocator for
/// [`ReconcileMode::Wholesale`] spawns. `LayerIds` drawn from
/// `crate::derive_layer_id(&pane_id)` collide when the new
/// top of the swapped tree also has `pre_order == 0` (both
/// resolve to `LayerId(0)`), so wholesale spawns draw from
/// this counter instead — the swap-produced IDs only flow
/// through their fresh runner + the AGENTS.md §"Hard rule:
/// one layer per instance" exceptions noted in the
/// `ReconcileMode::Wholesale` docstring.
static NEXT_LAYER_ID: AtomicU32 = AtomicU32::new(1);

/// Draw the next monotonic `LayerId` for a wholesale-swap
/// spawn. Relaxed ordering is sufficient — each spawn only
/// needs "later spawns get later IDs", not strict
/// serialisation across threads (the binary's tick loop is
/// single-threaded).
pub fn alloc_layer_id() -> PaneLayerId {
    let n = NEXT_LAYER_ID.fetch_add(1, Ordering::Relaxed);
    PaneLayerId(n as u64)
}

/// Reconcile mode for [`TickContext::reconcile_runners`].
/// Selects whether survivors keep their `PaneLayerId`
/// (in-place, for `AppNewPane` / `PaneClose` rebalance) or
/// rotate every `PaneLayerId` (wholesale, for
/// `PanePreset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileMode {
    /// In-place rebalance (`AppNewPane`, `PaneClose`): match
    /// survivors by their `pane.label` (labels are stable
    /// across sibling-absorption rebalance; `PaneIds` are NOT,
    /// so PaneId-keyed matching would drop the survivor
    /// spuriously). Survivors' `PaneLayerId` is preserved per
    /// the AGENTS.md Hard rule.
    InPlace,
    /// Wholesale swap (`PanePreset`): every old runner is
    /// dropped (its `Drop` revokes the `LayerId` via `close_tx`)
    /// and every new pane is spawned with a freshly-allocated
    /// `PaneLayerId` (from [`alloc_layer_id`], NOT from
    /// `crate::derive_layer_id &pane_id`, because both
    /// would collide on `LayerId(0)` when the new tree's
    /// top pane has `pre_order == 0`). The wholesale swap is
    /// a different topology, not a same-instance mutation,
    /// so the Hard rule's "no rebinding" wording does not
    /// apply.
    Wholesale,
}

/// Snapshot collection + exit-status result returned by
/// [`TickContext::tick_runners`]. Bundled into a struct to keep
/// the return type readable and clippy-friendly.
// Only constructed by `tick_runners` which is now test-only.
#[allow(dead_code)]
pub struct RunnerTickResult {
    pub snapshots: Vec<Option<cmdash_pty::PaneTerminalState>>,
    pub all_exited: bool,
}
impl<'a, B: ratatui::backend::Backend> TickContext<'a, B>
where
    B::Error: Send + Sync + 'static,
{
    /// Construct a [`TickContext`] from all 14 per-frame
    /// building blocks, including the runtime-mutation hooks
    /// (`close_tx: PaneCloseTx`, `last_area: LayoutRect`,
    /// `presets: BTreeMap<String, LayoutNode>`,
    /// `shell: ShellSpec`). Enforces `focus < runners.len()`
    /// so the `runners.get_mut(*focus)` write-input path
    /// inside [`Self::run`] cannot index out of bounds;
    /// `Self::apply_action_full::PaneClose` restores this
    /// invariant after a tail-remove by clamping focus to
    /// `len() - 1`.
    ///
    // Production's `crate::run` calls this. Buffered into a
    // sub-struct would just duplicate the schema-history
    // coupling the AGENTS.md "minimal API surface" rule
    // discourages.
    #[allow(clippy::too_many_arguments)]
    pub fn new_full(
        runners: Vec<PaneRunner>,
        bindings: Router,
        focus: usize,
        running: bool,
        close_tx: UnboundedSender<cmdash_pty::PaneLayerId>,
        close_rx: UnboundedReceiver<cmdash_pty::PaneLayerId>,
        graphics: GraphicsState,
        terminal: &'a mut Terminal<B>,
        tick: Duration,
        layout_root: LayoutNode,
        pending_resize: Option<(u16, u16)>,
        last_area: LayoutRect,
        presets: BTreeMap<String, LayoutNode>,
        stack_focus: BTreeMap<PaneId, usize>,
        shell: ShellSpec,
        config_reload_rx: Option<UnboundedReceiver<ConfigReload>>,
    ) -> Self {
        assert!(
            focus < runners.len(),
            "TickContext::new_full: focus ({focus}) is out of bounds for {} runners",
            runners.len(),
        );
        // Seed the per-tab stack with the initial 1-tab
        // payload. The TabState's
        // `runners` is a CLONE of the input Vec (shells:
        // `PaneRunner::clone` returns pty-less shells per the
        // manual `Clone` impl in `pane.rs`); the v1 field's
        // `runners` keeps the real PaneRunners. The
        // `TabState.runners` is decorative — `reconcile_runners`
        // always spawns fresh real runners on tab mutations.
        let initial_tab = TabState {
            runners: runners.clone(),
            focus,
            layout_root: layout_root.clone(),
            stack_focus: stack_focus.clone(),
        };
        Self {
            runners,
            bindings,
            focus,
            running,
            close_tx,
            close_rx,
            graphics,
            terminal,
            tick,
            layout_root,
            pending_resize,
            last_area,
            presets,
            stack_focus,
            shell,
            drag_state: None,
            copy_mode: None,
            last_focused_snapshot: None,
            tabs: TabStack::new(initial_tab),
            config_reload_rx,
            status_bar: None,
            theme: cmdash_config::Theme::default(),
            widget_factories: std::collections::HashMap::new(),
            host_keyboard_flags: 0,
            pane_keyboard_flags: HashMap::new(),
            host_keyboard_pushed: false,
            pane_bracketed_paste: HashMap::new(),
            host_bracketed_paste: false,
            host_bracketed_paste_pushed: false,
            pane_focus_reporting: HashMap::new(),
            host_focus_reporting: false,
            host_focus_reporting_pushed: false,
            pane_alternate_screen: HashMap::new(),
            host_focused: true,
        }
    }

    /// Read-only accessor for the focused-pane index. The
    /// invariant `focus < runners.len()` is upheld by
    /// [`Self::new`] and restored by `apply_action::PaneClose`
    /// after tail removal, so the returned index can be used
    /// to index `runners` without an extra bounds check.
    /// Called from phase 3a's `terminal.draw` closure for
    /// structured tracing; also exposed for external scripts
    /// and terminal UI indicators.
    pub const fn focus(&self) -> usize {
        self.focus
    }

    /// Recompute the layout against `(w, h)` and resize every
    /// live [`PaneRunner`] to its new cell-grid rect. Driven
    /// from the top of `tick_loop` whenever
    /// [`Self::pending_resize`] is non-empty. Idempotent for
    /// repeated calls with the same `(w, h)`.
    ///
    /// **Pairing invariant.** `runners[i]` and `layout.panes[i]`
    /// share the same `cmdash_layout::PaneId` because
    /// [`ComputedLayout::compute`] against the same KDL tree
    /// yields the same pre-order leaf numbering (AGENTS.md
    /// §"`PaneId` stability"). The defensive `assert_eq!` in
    /// the per-pair loop surfaces a future regression that
    /// breaks the index alignment (e.g. someone introduces a
    /// v2 hot-swap that mutates runner order without
    /// compensating in layout).
    ///
    /// **Failure tolerance.** A single pane's `runner.resize`
    /// error is logged via `tracing::warn!` and the loop
    /// continues for siblings — a misbehaved PTY child must
    /// not bring the multiplexer down. An infrequent
    /// `LayoutError` or a runner-count mismatch also logs
    /// without escalating — the next tick's resize signal will
    /// have a fresh chance to succeed.
    pub fn relayout(&mut self, w: u16, h: u16) {
        // Zero-area safeguard before any side effect: a live
        // SIGWINCH that round-trips through `(0, 0)` (a window
        // snap or hide-and-restore on GNOME / KDE / macOS
        // minimize-restore) would otherwise panic through
        // `GraphicsState::set_cells`'s `assert!(cells.0 > 0 &&
        // cells.1 > 0)`. `crate::run`'s startup `host_size`
        // match already defaults to (80, 24) on the equivalent
        // initial-frame transient; this branch extends the
        // same protection to the dynamic per-tick path so the
        // live binary can't crash on a defensive transient the
        // host itself filters to (0, 0) only briefly.
        //
        // Early-return is preferred over "skip set_cells
        // only" because `PanePty::resize(0, 0)` against a
        // running child is itself a likely-Err path; keeping
        // the panes at their last-known rects and letting the
        // next non-zero SIGWINCH re-run the full path
        // preserves the cell-grid -> browser-cache -> PTY
        // triplet in lock-step rather than tearing it apart.
        if w == 0 || h == 0 {
            warn!(
                w,
                h, "relayout: zero-area resize signal; skipping layout refresh + set_cells"
            );
            return;
        }
        let chrome_height = TAB_BAR_HEIGHT
            + if self.status_bar.as_ref().is_some_and(|sb| sb.enabled) {
                STATUS_BAR_HEIGHT
            } else {
                0
            };
        let layout_area = LayoutRect {
            x: 0,
            y: 0,
            w,
            h: h.saturating_sub(chrome_height),
        };
        let layout = match ComputedLayout::compute(&self.layout_root, layout_area) {
            Ok(l) => l,
            Err(e) => {
                warn!(error = ?e, w, h, "relayout: ComputedLayout::compute failed; skipping");
                return;
            }
        };
        if layout.panes.len() != self.runners.len() {
            warn!(
                live_runners = self.runners.len(),
                computed_panes = layout.panes.len(),
                "relayout: live runner count and computed pane count diverged; \
                 skipping per-pane resize"
            );
            return;
        }
        for (runner, pane) in self.runners.iter_mut().zip(layout.panes.iter()) {
            assert_eq!(
                runner.computed().id,
                pane.id,
                "relayout: runners[i]/layout.panes[i] index pairing violated"
            );
            if let Err(e) = runner.resize(pane.rect) {
                warn!(
                    error = ?e,
                    layer_id = ?runner.layer_id(),
                    "relayout: pane resize failed; continuing for siblings"
                );
            }
        }
        self.last_area = layout_area;
        self.graphics.set_cells((w, h));
    }

    /// Apply a [`KeyAction`] to the full [`TickContext`] —
    /// both the v1 arms (`AppClose`, `PaneFocusNext`,
    /// `PaneFocusPrev`, `PaneClose` rebalance) and the
    /// carry-forward arms (`AppNewPane`,
    /// `PaneFocus{Up,Down,Left,Right}`, `PanePreset(name)`).
    /// The binary's tick loop drives this method through
    /// [`Self::handle_event_full`]; the prior v1 free-fn
    /// `apply_action` was removed so test + production share
    /// the same reconcile path end-to-end.
    pub fn apply_action_full(&mut self, action: KeyAction) {
        match action {
            KeyAction::AppClose => {
                self.running = false;
            }
            KeyAction::PaneFocusNext => {
                if !self.runners.is_empty() {
                    self.set_focus((self.focus + 1) % self.runners.len());
                }
            }
            KeyAction::PaneFocusPrev => {
                if !self.runners.is_empty() {
                    self.set_focus((self.focus + self.runners.len() - 1) % self.runners.len());
                }
            }
            KeyAction::PaneFocusUp => self.focus_by_direction(Direction::Up),
            KeyAction::PaneFocusDown => self.focus_by_direction(Direction::Down),
            KeyAction::PaneFocusLeft => self.focus_by_direction(Direction::Left),
            KeyAction::PaneFocusRight => self.focus_by_direction(Direction::Right),
            // Phase 4 + 4.5/5 ZStack focus primitives. The
            // four directional primitives
            // (`PaneStackDown`/`Up`/`Left`/`Right`) are
            // folded into the single
            // `crosstack_member(direction, advance)` helper
            // per the Phase 5.0 duplication-sweep: each
            // takes the geometric handoff direction as its
            // first arg, and a boolean advance flag (true =
            // forward to next member, handoff at last; false
            // = backward to previous member, handoff at
            // first). `PaneStackCycle` is intentionally NOT
            // folded into the helper because its
            // modulo-wrap arithmetic is a fundamentally
            // different algorithm from the boundary-handoff
            // shape.
            KeyAction::PaneStackCycle => self.handle_stack_cycle(),
            KeyAction::PaneStackDown => self.crosstack_member(Direction::Down, true),
            KeyAction::PaneStackUp => self.crosstack_member(Direction::Up, false),
            KeyAction::PaneStackLeft => self.crosstack_member(Direction::Left, false),
            KeyAction::PaneStackRight => self.crosstack_member(Direction::Right, true),
            KeyAction::AppNewPane => self.split_focused_for_new_pane(),
            KeyAction::PaneClose => self.close_focused_and_rebalance(),
            KeyAction::PanePreset(name) => self.swap_to_preset(&name),
            // Tab-axis actions: dispatched through per-tab-state
            // methods below. The cmdash-config-side parse_action
            // arms accept the 3 keybind tokens at KDL load time.
            KeyAction::TabNew => self.create_new_tab(),
            KeyAction::TabClose => self.close_active_tab(),
            KeyAction::TabSwitch(n) => self.switch_to_tab(n),
            // Mode entry/exit actions.
            KeyAction::EnterPaneResize => {
                self.bindings.set_mode(cmdash_keybinds::Mode::PaneResize);
            }
            KeyAction::EnterTabSwitch => {
                self.bindings.set_mode(cmdash_keybinds::Mode::TabSwitch);
            }
            KeyAction::EnterPresetPick => {
                self.bindings.set_mode(cmdash_keybinds::Mode::PresetPick);
            }
            KeyAction::EnterCopyMode => {
                self.enter_copy_mode();
            }
            KeyAction::ModeExit => {
                self.bindings.set_mode(cmdash_keybinds::Mode::Normal);
                self.copy_mode = None;
                self.last_focused_snapshot = None;
            }
            // Copy-mode movement and selection actions.
            KeyAction::CopyModeMoveUp => self.copy_mode_move(0, -1),
            KeyAction::CopyModeMoveDown => self.copy_mode_move(0, 1),
            KeyAction::CopyModeMoveLeft => self.copy_mode_move(-1, 0),
            KeyAction::CopyModeMoveRight => self.copy_mode_move(1, 0),
            KeyAction::CopyModeStartSelection => self.copy_mode_start_selection(),
            KeyAction::CopyModeCopy => {
                if let Err(e) = self.copy_mode_copy() {
                    warn!(error = ?e, "copy-mode copy failed");
                }
            }
            // Pane resize actions (active in PaneResize mode).
            KeyAction::PaneResizeUp => self.pane_resize_by_direction(Direction::Up),
            KeyAction::PaneResizeDown => self.pane_resize_by_direction(Direction::Down),
            KeyAction::PaneResizeLeft => self.pane_resize_by_direction(Direction::Left),
            KeyAction::PaneResizeRight => self.pane_resize_by_direction(Direction::Right),
        }
    }

    pub fn handle_event_full(&mut self, evt: &Event) {
        // Observability hook: log every crossterm event that
        // reaches the dispatch surface so a future run of the
        // live-binary AppNewPane integration test can directly
        // inspect what key tuple (if any) the router saw for
        // the `Ctrl-a` byte. The expected observation for the
        // AppNewPane happy-path test is a `Key` event with
        // `code: Char('a')`, `modifiers: CONTROL`, `kind:
        // Press`. Any other shape (e.g. `code: Null`,
        // `modifiers: NONE`, or no event reaching this line at
        // all) is diagnostic of a PTY-routing regression.
        // Privacy-redaction: full rationale + what's-redacted
        // list lives on `redacted_event_debug`'s rustdoc.
        debug!("crossterm event = {}", redacted_event_debug(evt));
        if let Some(action) = self.bindings.dispatch_crossterm(evt) {
            self.apply_action_full(action);
            return;
        }
        // Host SIGWINCH coalescer — Phase 0.5 drains the slot
        // (`pending_resize.take()`) at the top of the next
        // tick to drive [`Self::relayout`]. Coalesce-on-
        // overwrite so a rapid resize burst collapses to the
        // LATEST (cols, rows) by the time the next tick
        // reaches phase 0.5. This arm deliberately does NOT
        // mutate `runners`; relayout happens at the top of the
        // tick after this input drain so the cross-key
        // close-channel invariant
        // (`Drop::drop enqueues onto a live receiver`) is
        // preserved for any pane drops that share the same
        // tick.
        if let Event::Resize(w, h) = evt {
            self.pending_resize = Some((*w, *h));
            return;
        }
        // Mouse events: click-to-focus, Alt+drag resize, PTY forwarding.
        if let Event::Mouse(mouse) = evt {
            self.handle_mouse_event(mouse);
            return;
        }
        // Paste events: forward to the focused pane. If the pane has
        // requested bracketed paste, wrap the pasted content in the
        // standard bracketed-paste delimiters so the child application
        // can distinguish pasted input from typed input.
        if let Event::Paste(text) = evt {
            self.handle_paste(text);
            return;
        }
        // Host focus-change events: forward `CSI I` / `CSI O` to the
        // focused pane when it (or any pane) has requested focus
        // reporting. The host only emits these events when focus-change
        // reporting is enabled via `EnableFocusChange`.
        if let Event::FocusGained = evt {
            self.host_focused = true;
            self.forward_focus_event_to_focused_pane(true);
            return;
        }
        if let Event::FocusLost = evt {
            self.host_focused = false;
            self.forward_focus_event_to_focused_pane(false);
            return;
        }
        let Event::Key(KeyEvent {
            code,
            kind,
            modifiers,
            ..
        }) = evt
        else {
            return;
        };
        // Intercept PageUp/PageDown for scrollback navigation.
        // These keys control the scrollback viewport instead of
        // being forwarded to the child PTY. Any other key
        // resets scrollback to live view before forwarding.
        // Scrollback navigation is disabled while the focused pane
        // is in the alternate screen buffer so full-screen TUIs
        // (vim, htop, less) receive PageUp/PageDown directly.
        let focused_in_alt = self
            .runners
            .get(self.focus)
            .map(|r| r.in_alternate_screen())
            .unwrap_or(false);
        let page_size = self
            .runners
            .get(self.focus)
            .map(|r| r.computed().rect.h as usize)
            .unwrap_or(24);
        match code {
            KeyCode::PageUp if !focused_in_alt => {
                if let Some(runner) = self.runners.get_mut(self.focus) {
                    runner.scrollback_up(page_size);
                }
                return;
            }
            KeyCode::PageDown if !focused_in_alt => {
                // Only intercept PageDown when already in
                // scrollback mode. Otherwise forward to the
                // PTY so pagers (less, man) receive it.
                let in_sb = self
                    .runners
                    .get(self.focus)
                    .map(|r| r.in_scrollback())
                    .unwrap_or(false);
                if in_sb {
                    if let Some(runner) = self.runners.get_mut(self.focus) {
                        runner.scrollback_down(page_size);
                    }
                    return;
                }
            }
            _ => {
                // Reset scrollback on any other key press.
                if let Some(runner) = self.runners.get_mut(self.focus) {
                    if runner.in_scrollback() {
                        runner.scrollback_reset();
                    }
                }
            }
        }
        // Determine the focused pane's capabilities before deciding
        // how to encode this key event. Widget panes keep the
        // existing widget event path; PTY panes use the Kitty
        // protocol when the child has requested enhancement.
        let focused_flags = self
            .runners
            .get(self.focus)
            .map(|r| r.keyboard_flags())
            .unwrap_or(0);
        let is_widget = self
            .runners
            .get(self.focus)
            .map(|r| r.is_widget())
            .unwrap_or(false);

        if is_widget {
            // Widgets only receive press events.
            if !matches!(kind, KeyEventKind::Press) {
                return;
            }
            if let Some(runner) = self.runners.get_mut(self.focus) {
                if let Some(widget) = runner.widget.as_mut() {
                    let widget_code = match code {
                        KeyCode::Char(c) => cmdash_widget_sdk::KeyCode::Char(*c),
                        KeyCode::Enter => cmdash_widget_sdk::KeyCode::Enter,
                        KeyCode::Esc => cmdash_widget_sdk::KeyCode::Esc,
                        KeyCode::Backspace => cmdash_widget_sdk::KeyCode::Backspace,
                        KeyCode::Tab => cmdash_widget_sdk::KeyCode::Tab,
                        KeyCode::Up => cmdash_widget_sdk::KeyCode::Up,
                        KeyCode::Down => cmdash_widget_sdk::KeyCode::Down,
                        KeyCode::Left => cmdash_widget_sdk::KeyCode::Left,
                        KeyCode::Right => cmdash_widget_sdk::KeyCode::Right,
                        KeyCode::Home => cmdash_widget_sdk::KeyCode::Home,
                        KeyCode::End => cmdash_widget_sdk::KeyCode::End,
                        KeyCode::PageUp => cmdash_widget_sdk::KeyCode::PageUp,
                        KeyCode::PageDown => cmdash_widget_sdk::KeyCode::PageDown,
                        KeyCode::F(n) => cmdash_widget_sdk::KeyCode::F(*n),
                        _ => return,
                    };
                    let widget_evt = cmdash_widget_sdk::WidgetEvent::Key {
                        code: widget_code,
                        modifiers: cmdash_widget_sdk::KeyModifiers {
                            ctrl: modifiers.contains(crossterm::event::KeyModifiers::CONTROL),
                            shift: modifiers.contains(crossterm::event::KeyModifiers::SHIFT),
                            alt: modifiers.contains(crossterm::event::KeyModifiers::ALT),
                            super_: false,
                        },
                    };
                    widget.on_event(&widget_evt);
                }
            }
            return;
        }

        let bytes = if focused_flags != 0 && self.graphics.caps().supports_kitty_keyboard() {
            encode_kitty_key_event(code, *modifiers, *kind)
        } else if matches!(kind, KeyEventKind::Press) {
            event_to_bytes(*code)
        } else {
            None
        };
        let Some(bytes) = bytes else {
            return;
        };
        if let Some(runner) = self.runners.get_mut(self.focus) {
            if let Err(e) = runner.write_input(&bytes) {
                debug!(
                    error = ?e,
                    layer_id = ?runner.layer_id(),
                    "write_input failed"
                );
            }
        }
    }

    /// Set the focused pane index, clearing copy-mode state when
    /// the focus moves to a different pane.
    pub fn set_focus(&mut self, idx: usize) {
        if idx != self.focus {
            self.copy_mode = None;
        }
        self.focus = idx;
    }

    /// Carry-forward: `PaneFocus{Direction}`. Resolve the
    /// adjacent pane in `dir` from the focused pane via
    /// [`adjacent_pane`]'s rect-proximity algorithm
    /// (max perpendicular overlap → min distance → min
    /// `pre_order`); swap `self.focus` to the matching
    /// runner's Vec index. No-op if no neighbour exists.
    /// Dispatch a crossterm mouse event. Handles click-to-focus,
    /// Alt+drag split resize, scroll, and mouse forwarding to the
    /// focused pane's PTY.
    fn handle_mouse_event(&mut self, mouse: &MouseEvent) {
        let tab_bar_offset = TAB_BAR_HEIGHT;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if mouse.modifiers.contains(KeyModifiers::ALT) {
                    self.start_drag_resize(mouse, tab_bar_offset);
                } else {
                    self.focus_by_click(mouse.column, mouse.row, tab_bar_offset);
                    // Forward press to the newly-focused pane's PTY.
                    self.forward_mouse_to_pty(mouse, tab_bar_offset);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.drag_state.is_some() {
                    self.update_drag_resize(mouse, tab_bar_offset);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag_state = None;
                self.forward_mouse_to_pty(mouse, tab_bar_offset);
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.forward_mouse_to_pty(mouse, tab_bar_offset);
            }
            _ => {
                self.forward_mouse_to_pty(mouse, tab_bar_offset);
            }
        }
    }

    /// Click-to-focus: find the pane whose rect contains the click
    /// position and swap `self.focus` to that index.
    fn focus_by_click(&mut self, col: u16, row: u16, tab_bar_offset: u16) {
        if self.runners.is_empty() {
            return;
        }
        let layout_area = LayoutRect {
            x: 0,
            y: 0,
            w: self.last_area.w,
            h: self.last_area.h,
        };
        let layout = match ComputedLayout::compute(&self.layout_root, layout_area) {
            Ok(l) => l,
            Err(_) => return,
        };
        // Adjust for tab bar: the layout is rendered starting at
        // row `tab_bar_offset`, so a click at terminal row `row`
        // maps to layout row `row - tab_bar_offset`.
        let layout_row = row.saturating_sub(tab_bar_offset);
        for (idx, pane) in layout.panes.iter().enumerate() {
            if pane.rect.x <= col
                && col < pane.rect.x.saturating_add(pane.rect.w)
                && pane.rect.y <= layout_row
                && layout_row < pane.rect.y.saturating_add(pane.rect.h)
            {
                self.set_focus(idx);
                return;
            }
        }
    }

    /// Begin an Alt+drag resize. Find the closest enclosing Split
    /// for the clicked pane and record the initial drag state.
    fn start_drag_resize(&mut self, mouse: &MouseEvent, tab_bar_offset: u16) {
        if self.runners.is_empty() {
            return;
        }
        let layout_area = LayoutRect {
            x: 0,
            y: 0,
            w: self.last_area.w,
            h: self.last_area.h,
        };
        let layout = match ComputedLayout::compute(&self.layout_root, layout_area) {
            Ok(l) => l,
            Err(_) => return,
        };
        let layout_row = mouse.row.saturating_sub(tab_bar_offset);
        // Find which pane was clicked.
        let clicked_pane_idx = layout.panes.iter().position(|p| {
            p.rect.x <= mouse.column
                && mouse.column < p.rect.x.saturating_add(p.rect.w)
                && p.rect.y <= layout_row
                && layout_row < p.rect.y.saturating_add(p.rect.h)
        });
        let Some(pane_idx) = clicked_pane_idx else {
            return;
        };
        self.set_focus(pane_idx);
        // Walk the layout tree to find the nearest enclosing Split.
        // The pane's resolver path gives us the tree location; the
        // parent of the leaf is the Split we want to resize.
        let pane_id = layout.panes[pane_idx].id;
        let tree_path = pane_id.path();
        // path[0] is the resolver seed (always 0); tree path is
        // path[1..]. The Split is the parent of the leaf, so we
        // take tree_path[..tree_path.len()-1] to get the Split's
        // path.
        if tree_path.len() <= 1 {
            return; // leaf is the root — no parent Split.
        }
        // Build the path to the parent Split (skip resolver seed,
        // drop the leaf index). Use fixed-size array.
        let raw_path = &tree_path[1..tree_path.len() - 1];
        // Navigate to the Split node to read its axis and ratio.
        // Borrow and pattern-match directly — no clone needed.
        let mut node = &self.layout_root;
        for &idx in raw_path {
            match node {
                LayoutNode::Split { children, .. } => {
                    node = children.get(idx as usize).unwrap();
                }
                LayoutNode::Stack { panes } | LayoutNode::ZStack { panes } => {
                    node = panes.get(idx as usize).unwrap();
                }
                _ => return,
            }
        }
        if let LayoutNode::Split { axis, ratio, .. } = node {
            let total_cells = match axis {
                cmdash_config::SplitAxis::Horizontal => self.last_area.w,
                cmdash_config::SplitAxis::Vertical => self.last_area.h,
            };
            let start_pos = match axis {
                cmdash_config::SplitAxis::Horizontal => mouse.column,
                cmdash_config::SplitAxis::Vertical => layout_row,
            };
            let mut path_arr = [0u16; 8];
            let path_len = raw_path.len().min(8);
            path_arr[..path_len].copy_from_slice(&raw_path[..path_len]);
            self.drag_state = Some(DragState {
                split_path: path_arr,
                split_path_len: path_len as u8,
                start_pos,
                initial_ratio: ratio.0,
                axis: *axis,
                total_cells,
            });
        }
    }

    /// Continue an Alt+drag resize: compute the delta from the
    /// initial position and update the Split's ratio.
    fn update_drag_resize(&mut self, mouse: &MouseEvent, tab_bar_offset: u16) {
        // Extract data from drag_state before any &mut self borrows
        // to avoid borrow-checker conflicts with relayout().
        let (split_path, split_path_len, start_pos, initial_ratio, axis, total_cells) = {
            let Some(ref drag) = self.drag_state else {
                return;
            };
            (
                drag.split_path,
                drag.split_path_len,
                drag.start_pos,
                drag.initial_ratio,
                drag.axis,
                drag.total_cells,
            )
        };
        let layout_row = mouse.row.saturating_sub(tab_bar_offset);
        let current_pos = match axis {
            cmdash_config::SplitAxis::Horizontal => mouse.column,
            cmdash_config::SplitAxis::Vertical => layout_row,
        };
        let delta = current_pos as i32 - start_pos as i32;
        // Convert cell delta to percentage change.
        let pct_delta = if total_cells > 0 {
            (delta * 100 / total_cells as i32) as i16
        } else {
            0
        };
        let new_ratio = (initial_ratio as i16 + pct_delta).clamp(1, 99) as u8;
        let path_slice = &split_path[..split_path_len as usize];
        if let Err(e) = cmdash_layout::update_split_ratio(
            &mut self.layout_root,
            path_slice,
            cmdash_config::Ratio(new_ratio),
        ) {
            warn!(error = ?e, "drag resize: update_split_ratio failed");
        }
        // Trigger relayout with the current area.
        let w = self.last_area.w;
        let h = self.last_area.h;
        self.relayout(w, h);
    }

    /// Forward a mouse event to the focused pane's PTY as an SGR
    /// extended mouse sequence. The child must have enabled mouse
    /// tracking (e.g. `\x1b[?1006h`) for the bytes to be useful;
    /// apps that haven't opted in simply ignore the bytes.
    fn forward_mouse_to_pty(&mut self, mouse: &MouseEvent, tab_bar_offset: u16) {
        if self.focus >= self.runners.len() {
            return;
        }
        let layout_row = mouse.row.saturating_sub(tab_bar_offset);
        // Encode button: SGR format.
        let button: u8 = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => 0,
            MouseEventKind::Down(MouseButton::Middle) => 1,
            MouseEventKind::Down(MouseButton::Right) => 2,
            MouseEventKind::Up(MouseButton::Left) => 0,
            MouseEventKind::Up(MouseButton::Middle) => 1,
            MouseEventKind::Up(MouseButton::Right) => 2,
            MouseEventKind::Drag(MouseButton::Left) => 32,
            MouseEventKind::Drag(MouseButton::Middle) => 33,
            MouseEventKind::Drag(MouseButton::Right) => 34,
            MouseEventKind::ScrollUp => 64,
            MouseEventKind::ScrollDown => 65,
            _ => return,
        };
        let mut modifiers: u8 = 0;
        if mouse.modifiers.contains(KeyModifiers::SHIFT) {
            modifiers |= 4;
        }
        if mouse.modifiers.contains(KeyModifiers::ALT) {
            modifiers |= 8;
        }
        if mouse.modifiers.contains(KeyModifiers::CONTROL) {
            modifiers |= 16;
        }
        let btn = button | modifiers;
        let suffix = if matches!(mouse.kind, MouseEventKind::Up(..)) {
            'm'
        } else {
            'M'
        };
        let seq = format!(
            "\x1b[<{};{};{}{suffix}",
            btn,
            mouse.column + 1,
            layout_row + 1
        );
        if let Some(runner) = self.runners.get_mut(self.focus) {
            let _ = runner.write_input(seq.as_bytes());
        }
    }

    // ------------------------------------------------------------------
    // Copy-mode helpers
    // ------------------------------------------------------------------

    /// Enter copy mode for the focused pane. The cursor starts at
    /// the top-left of the pane's visible area. Widget panes have
    /// no PTY text grid, so copy mode is disabled for them.
    pub fn enter_copy_mode(&mut self) {
        if self.runners.is_empty() {
            return;
        }
        if self.runners[self.focus].is_widget() {
            return;
        }
        self.copy_mode = Some(CopyModeState {
            cursor_x: 0,
            cursor_y: 0,
            selection_start: None,
        });
        self.bindings.set_mode(cmdash_keybinds::Mode::Copy);
    }

    /// Move the copy-mode cursor by `(dx, dy)` cells, clamping to
    /// the focused pane's rect.
    fn copy_mode_move(&mut self, dx: i16, dy: i16) {
        let Some(state) = self.copy_mode.as_mut() else {
            return;
        };
        let rect = self.runners.get(self.focus).map(|r| r.computed().rect);
        let Some(rect) = rect else { return };
        let new_x =
            (state.cursor_x as i32 + dx as i32).clamp(0, rect.w.saturating_sub(1) as i32) as u16;
        let new_y =
            (state.cursor_y as i32 + dy as i32).clamp(0, rect.h.saturating_sub(1) as i32) as u16;
        state.cursor_x = new_x;
        state.cursor_y = new_y;
    }

    /// Toggle the selection anchor at the current cursor position.
    /// If no selection exists, start one; if one exists, clear it.
    fn copy_mode_start_selection(&mut self) {
        let Some(state) = self.copy_mode.as_mut() else {
            return;
        };
        if state.selection_start.is_some() {
            state.selection_start = None;
        } else {
            state.selection_start = Some((state.cursor_x, state.cursor_y));
        }
    }

    /// Update `last_focused_snapshot` from the latest runner
    /// snapshots. Only the focused pane's snapshot is retained,
    /// and only while copy mode is active.
    // TODO: only called from tests after run()/run_loop() extraction.
    #[allow(dead_code)]
    pub fn update_last_focused_snapshot(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) {
        if self.copy_mode.is_some() {
            self.last_focused_snapshot = snapshots.get(self.focus).cloned().flatten();
        } else {
            self.last_focused_snapshot = None;
        }
    }
    /// Copy the selected text from the focused pane to the system
    /// clipboard and exit copy mode. Returns an error if the
    /// clipboard cannot be accessed.
    fn copy_mode_copy(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let state = self.copy_mode.as_ref().copied();
        let Some(state) = state else { return Ok(()) };
        let Some(snapshot) = self.last_focused_snapshot.as_ref() else {
            return Ok(());
        };
        let text = extract_selected_text(
            &snapshot.grid,
            state.cursor_x,
            state.cursor_y,
            state.selection_start,
        );
        let mut clipboard = crate::ArboardClipboard;
        copy_text_to_clipboard(&mut clipboard, &text)?;
        self.copy_mode = None;
        self.bindings.set_mode(cmdash_keybinds::Mode::Normal);
        Ok(())
    }

    /// Find the parent Split of the focused pane and return
    /// `(split_path, axis, current_ratio, child_index)`. Returns
    /// `None` if the focused pane has no parent Split.
    fn parent_split_of_focused(&self) -> Option<(Vec<u16>, cmdash_config::SplitAxis, u8, usize)> {
        if self.runners.is_empty() {
            return None;
        }
        let focused_id = self.runners[self.focus].computed().id;
        let tree_path = focused_id.path();
        if tree_path.len() <= 1 {
            return None; // leaf is the root.
        }
        let parent_path: Vec<u16> = tree_path[1..tree_path.len() - 1].to_vec();
        let child_idx = *tree_path.last().unwrap_or(&0) as usize;
        let mut node = &self.layout_root;
        for &idx in &parent_path {
            match node {
                LayoutNode::Split { children, .. } => {
                    node = children.get(idx as usize)?;
                }
                LayoutNode::Stack { panes } | LayoutNode::ZStack { panes } => {
                    node = panes.get(idx as usize)?;
                }
                _ => return None,
            }
        }
        match node {
            LayoutNode::Split { axis, ratio, .. } => Some((parent_path, *axis, ratio.0, child_idx)),
            _ => None,
        }
    }

    /// Resize the focused pane's parent split in the given direction.
    /// Finds the enclosing Split via the focused pane's resolver path,
    /// computes the new ratio (±2% per arrow press), and triggers
    /// relayout. No-op if the focused pane has no parent Split or if
    /// the direction doesn't match the Split's axis.
    fn pane_resize_by_direction(&mut self, dir: Direction) {
        let Some((parent_path, axis, current_ratio, child_idx)) = self.parent_split_of_focused()
        else {
            return;
        };
        // Only resize if the direction matches the Split's axis.
        if !matches!(
            (axis, dir),
            (
                cmdash_config::SplitAxis::Horizontal,
                Direction::Left | Direction::Right
            ) | (
                cmdash_config::SplitAxis::Vertical,
                Direction::Up | Direction::Down
            )
        ) {
            return;
        }
        // Compute new ratio: ±2% per arrow press.
        let delta: i16 = match (axis, dir) {
            (cmdash_config::SplitAxis::Horizontal, Direction::Right) => 2,
            (cmdash_config::SplitAxis::Horizontal, Direction::Left) => -2,
            (cmdash_config::SplitAxis::Vertical, Direction::Down) => 2,
            (cmdash_config::SplitAxis::Vertical, Direction::Up) => -2,
            _ => unreachable!(),
        };
        // If focused pane is child 1, flip the direction.
        let adjusted_delta = if child_idx == 1 { -delta } else { delta };
        let new_ratio = (current_ratio as i16 + adjusted_delta).clamp(1, 99) as u8;
        let _ = cmdash_layout::update_split_ratio(
            &mut self.layout_root,
            &parent_path,
            cmdash_config::Ratio(new_ratio),
        );
        let w = self.last_area.w;
        let h = self.last_area.h;
        self.relayout(w, h);
    }

    fn focus_by_direction(&mut self, dir: Direction) {
        if self.runners.is_empty() {
            return;
        }
        if self.focus >= self.runners.len() {
            self.set_focus(self.runners.len() - 1);
        }
        let focused_id = self.runners[self.focus].computed().id;
        let layout = match ComputedLayout::compute(&self.layout_root, self.last_area) {
            Ok(l) => l,
            Err(e) => {
                warn!(error = ?e, "focus_by_direction: compute failed");
                return;
            }
        };
        let Some(target_id) = adjacent_pane(&layout, focused_id, dir) else {
            return;
        };
        if let Some(idx) = self
            .runners
            .iter()
            .position(|r| r.computed().id == target_id)
        {
            self.set_focus(idx);
        }
    }

    /// Phase 4 carry-forward: locate the focused pane's
    /// parent `ZStack` + its member index. Returns
    /// `Some((parent_path, member_idx))` if the focused
    /// pane is a direct child of a `LayoutNode::ZStack`,
    /// otherwise `None` -- the caller interprets `None`
    /// as "focused pane is not a `ZStack` member" and
    /// no-ops. `focused_path` is a tree-indexed path
    /// WITHOUT the resolver `path[0]` seed.
    fn focused_zstack_context(
        layout_root: &LayoutNode,
        focused_path: &[u16],
    ) -> Option<(Vec<u16>, usize)> {
        if focused_path.is_empty() {
            return None;
        }
        let last_idx = *focused_path.last()? as usize;
        let parent_path = focused_path.split_last()?.1.to_vec();
        let parent_node = cmdash_layout::walk_imut(layout_root, &parent_path).ok()?;
        match parent_node {
            LayoutNode::ZStack { panes } => {
                if last_idx < panes.len() {
                    Some((parent_path, last_idx))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// ## Algorithmic-shape divergence vs `crosstack_member`
    ///
    /// This is a **modulo-wrap** primitive. At the LAST
    /// member, the focus wraps BACK to the FIRST via
    /// `(member_idx + 1) % panes.len()` -- it stays
    /// **inside** the `ZStack` -- and `self.stack_focus` is
    /// **always** updated (even on the wrap-around, since
    /// the post-wrap focus still lives inside the `ZStack`
    /// and the keyed entry tracks the new member index).
    /// `PaneStackCycle` therefore has no handoff path -- it
    /// is a closed cycle within the `ZStack`.
    ///
    /// `crosstack_member` looks superficially combinable
    /// (both primitives drive `ZStack` member indices) but
    /// has the OPPOSITE boundary post-condition: at the
    /// FIRST or LAST member it **escapes** the `ZStack` via
    /// `focus_by_direction(handoff_direction)` and never
    /// mutates `stack_focus` on the handoff path -- the
    /// new focus lands outside the `ZStack`, so any keyed
    /// entry would go stale.
    ///
    /// **Trapdoor precedent** -- `cmdash_layout::split_rect` in
    /// `cmdash_layout` documents that the cfg
    /// `split axis=horizontal` keyword is a *column* split
    /// (same y range, different x columns), the OPPOSITE of
    /// the axis-token's prose name. Two fn-names that sound
    /// combinable (`handle_stack_cycle` and
    /// `crosstack_member`) can quietly carry two different
    /// post-conditions. **Do NOT refold these two
    /// primitives in a future refactor.**
    ///
    /// Phase 4 carry-forward: `PaneStackCycle`. Find the
    /// focused pane's parent `ZStack` + member index, then
    /// advance `self.focus` to the next member in
    /// declaration order, wrapping from the last member
    /// back to the first. No-op if the focused pane is
    /// not a `ZStack` member.
    fn handle_stack_cycle(&mut self) {
        if self.runners.is_empty() {
            return;
        }
        if self.focus >= self.runners.len() {
            self.set_focus(self.runners.len() - 1);
        }
        let focused_id = self.runners[self.focus].computed().id;
        let seed_path = focused_id.path();
        let tree_path: &[u16] = if !seed_path.is_empty() {
            &seed_path[1..]
        } else {
            &[]
        };
        let Some((parent_path, member_idx)) =
            Self::focused_zstack_context(&self.layout_root, tree_path)
        else {
            return;
        };
        let Some(LayoutNode::ZStack { panes }) =
            cmdash_layout::walk_imut(&self.layout_root, &parent_path).ok()
        else {
            return;
        };
        let next_idx = (member_idx + 1) % panes.len();
        let mut next_path = parent_path.clone();
        next_path.push(next_idx as u16);
        if let Some(idx) = self.runners.iter().position(|r| {
            let rp = r.computed().id.path();
            let tp: &[u16] = if !rp.is_empty() { &rp[1..] } else { &[] };
            tp == next_path.as_slice()
        }) {
            self.set_focus(idx);
            let new_id = self.runners[idx].computed().id;
            self.stack_focus.insert(new_id, next_idx);
        }
    }

    /// Phase 4 + 4.5/5 carry-forward consolidation: directed
    /// `ZStack` focus primitive. Replaces the 4
    /// near-byte-identical `handle_stack_down`/`up`/`left`/`right`
    /// fns from prior phases; folds their boundary condition
    /// + boundary-handoff shape into a single 2-argument
    ///
    /// Arguments:
    /// - `handoff_direction`: the [`Direction`] the helper
    ///   delegates to when the focused member sits at the
    ///   boundary that needs to escape the `ZStack`. For advance
    ///   (`advance == true`) this is invoked at the LAST
    ///   member; for retreat (`advance == false`) this is
    ///   invoked at the FIRST member. The four directional
    ///   primitives map via:
    ///   - `PaneStackDown`  -> handoff `Direction::Down`   at LAST
    ///   - `PaneStackUp`    -> handoff `Direction::Up`     at FIRST
    ///   - `PaneStackLeft`  -> handoff `Direction::Left`   at FIRST
    ///   - `PaneStackRight` -> handoff `Direction::Right`  at LAST
    /// - `advance`: `true` advances to the next member in
    ///   declaration order (used by `Down`/`Right`); `false`
    ///   retreats to the previous member (used by `Up`/`Left`).
    ///
    /// Behaviour:
    /// - No-op when no runner is focused, when the focused
    ///   runner's path doesn't fit a ZStack-member slot, or
    ///   when the post-boundary handoff finds no neighbour
    ///   via `cmdash_layout::adjacent_pane`.
    /// - The handoff path does NOT mutate `stack_focus` (the
    ///   new focus is OUTSIDE the `ZStack`, so the keyed
    ///   stack-focus-map entry would never be queried).
    /// - Algorithmic-shape divergence vs `handle_stack_cycle`.
    ///   These two primitives look combinable (both drive
    ///   `ZStack` member indices in declaration order) but
    ///   carry fundamentally different post-conditions:
    ///   - `crosstack_member` (this helper) at the FIRST or
    ///     LAST member is a **boundary-hand-off** primitive:
    ///     it **escapes** the `ZStack` by delegating to
    ///     `focus_by_direction(handoff_direction)`, and it
    ///     **never mutates `stack_focus`** on the handoff
    ///     path -- the new focus lands OUTSIDE the `ZStack`,
    ///     so any keyed entry for the old focus would be
    ///     stale and we deliberately drop it.
    ///   - `handle_stack_cycle` is a **modulo-wrap**
    ///     primitive: at the LAST member the arithmetic
    ///     `(member_idx + 1) % panes.len()` wraps the focus
    ///     BACK to the FIRST member (it stays **inside**
    ///     the `ZStack`), and it **always mutates
    ///     `stack_focus`** -- even on the wrap-around, the
    ///     keyed member-index entry tracks the post-wrap
    ///     focus.
    ///
    ///   Folding these into one fn would tangle two
    ///   different post-conditions behind a single
    ///   conditional branch -- an anti-pattern. They are
    ///   intentionally separate.
    ///
    /// - **Trapdoor precedent** -- the `cmdash_layout::split_rect`
    ///   rustdoc on `cmdash_layout` warns that the cfg
    ///   `axis=horizontal` is a *column* split (same y
    ///   range, different x columns), the OPPOSITE of the
    ///   axis-token's prose name. Names that suggest one
    ///   direction can quietly denote the opposite
    ///   direction; in the same vein, two fn-names that
    ///   sound combinable (`crosstack_member` and
    ///   `handle_stack_cycle`) can quietly carry two
    ///   different post-conditions. **Do NOT refold these
    ///   two primitives in a future refactor.**
    fn crosstack_member(&mut self, handoff_direction: Direction, advance: bool) {
        if self.runners.is_empty() {
            return;
        }
        if self.focus >= self.runners.len() {
            self.set_focus(self.runners.len() - 1);
        }
        let focused_id = self.runners[self.focus].computed().id;
        let seed_path = focused_id.path();
        let tree_path: &[u16] = if !seed_path.is_empty() {
            &seed_path[1..]
        } else {
            &[]
        };
        let Some((parent_path, member_idx)) =
            Self::focused_zstack_context(&self.layout_root, tree_path)
        else {
            return;
        };
        let Some(LayoutNode::ZStack { panes }) =
            cmdash_layout::walk_imut(&self.layout_root, &parent_path).ok()
        else {
            return;
        };
        if advance {
            // Advance mode: cycle forward through declaration
            // order. At the LAST member, hand off to the
            // geometric neighbour in `handoff_direction`
            // (Down for `PaneStackDown`; Right for
            // `PaneStackRight`). `adjacent_pane` skips panes
            // that share the ZStack's rect (zero
            // perpendicular gap distance=0), so the
            // resolution lands on a sibling Split member
            // outside the ZStack.
            if member_idx + 1 == panes.len() {
                self.focus_by_direction(handoff_direction);
                return;
            }
            let next_idx = member_idx + 1;
            let mut next_path = parent_path.clone();
            next_path.push(next_idx as u16);
            if let Some(idx) = self.runners.iter().position(|r| {
                let rp = r.computed().id.path();
                let tp: &[u16] = if !rp.is_empty() { &rp[1..] } else { &[] };
                tp == next_path.as_slice()
            }) {
                self.set_focus(idx);
                let new_id = self.runners[idx].computed().id;
                self.stack_focus.insert(new_id, next_idx);
            }
        } else {
            // Retreat mode: cycle backward through declaration
            // order. At the FIRST member, hand off to the
            // geometric neighbour in `handoff_direction`
            // (Up for `PaneStackUp`; Left for `PaneStackLeft`).
            // Same adjacent_pane self-skip semantics as
            // above.
            if member_idx == 0 {
                self.focus_by_direction(handoff_direction);
                return;
            }
            let prev_idx = member_idx - 1;
            let mut next_path = parent_path.clone();
            next_path.push(prev_idx as u16);
            if let Some(idx) = self.runners.iter().position(|r| {
                let rp = r.computed().id.path();
                let tp: &[u16] = if !rp.is_empty() { &rp[1..] } else { &[] };
                tp == next_path.as_slice()
            }) {
                self.set_focus(idx);
                let new_id = self.runners[idx].computed().id;
                self.stack_focus.insert(new_id, prev_idx);
            }
        }
    }

    /// Carry-forward: `AppNewPane`. Locate the focused leaf
    /// in `self.layout_root` and replace it with a
    /// `Split { Horizontal, Ratio(50), [original_clone, new_leaf] }`,
    /// then [`Self::reconcile_runners`] so the new pane has a
    /// freshly-spawned runner AND survivors' cached
    /// `PaneId`s align with the post-split tree resolution.
    /// The original focused pane's `pre_order` index is
    /// preserved (it becomes child 0 of the new Split) — its
    /// `LayerId` stays stable per AGENTS.md Hard rule (no
    /// rebinding).
    fn split_focused_for_new_pane(&mut self) {
        if self.runners.is_empty() {
            return;
        }
        if self.focus >= self.runners.len() {
            self.set_focus(self.runners.len() - 1);
        }
        let focused_id = self.runners[self.focus].computed().id;
        // The resolver seeds `path[0] = 0` to represent an
        // implicit outermost `layout { ... }` wrapper;
        // [`replace_leaf_with_split`] walks the actual
        // `LayoutNode` tree, so we strip the seed before
        // passing.
        let seed_path = focused_id.path();
        let tree_path: &[u16] = if !seed_path.is_empty() {
            &seed_path[1..]
        } else {
            &[]
        };
        // When the focused leaf IS the root (resolver path
        // length 1, all-seed), there is no enclosing Split to
        // [`replace_leaf_with_split`]. Replace the root itself
        // with `Split { Horizontal, 50, [original_clone, new_leaf] }`.
        if tree_path.is_empty() {
            let original_root = match &self.layout_root {
                LayoutNode::Pane(p) => LayoutNode::Pane(p.clone()),
                _ => {
                    warn!(
                        "AppNewPane: focused leaf IS the root but root is {:?}; no-op",
                        self.layout_root
                    );
                    return;
                }
            };
            self.layout_root = LayoutNode::Split {
                axis: CfgSplitAxis::Horizontal,
                ratio: CfgRatio(50),
                children: vec![
                    original_root,
                    LayoutNode::Pane(CfgPane {
                        kind: PaneKind::Shell,
                        label: None,
                        command: None,
                        scrollback_capacity: None,
                    }),
                ],
            };
            self.reconcile_runners(ReconcileMode::InPlace);
            return;
        }
        let new_leaf = LayoutNode::Pane(CfgPane {
            kind: PaneKind::Shell,
            label: None,
            command: None,
            scrollback_capacity: None,
        });
        match replace_leaf_with_split(
            &mut self.layout_root,
            tree_path,
            new_leaf,
            CfgSplitAxis::Horizontal,
            CfgRatio(50),
        ) {
            Ok(_) => self.reconcile_runners(ReconcileMode::InPlace),
            Err(e) => {
                warn!(error = ?e, "AppNewPane: replace_leaf_with_split failed")
            }
        }
    }

    /// Carry-forward: `PaneClose`. Drop the focused runner
    /// FIRST (its `Drop` fires `close_tx` -> next phase 1
    /// revokes the `LayerId` per AGENTS.md Hard rule);
    /// rebalance `self.layout_root` via [`remove_leaf`]
    /// (sibling absorption collapses a 2-child `Split` to
    /// its survivor); then [`Self::reconcile_runners`]
    /// rebuilds `self.runners` against the post-rebalance
    /// resolution. Closing the final pane quits the binary.
    fn close_focused_and_rebalance(&mut self) {
        if self.runners.is_empty() {
            return;
        }
        if self.focus >= self.runners.len() {
            self.set_focus(self.runners.len() - 1);
        }
        let focused_id = self.runners[self.focus].computed().id;
        // When the focused leaf IS the root (resolver path
        // length 1, all-seed), there's no enclosing Split to
        // rebalance — closing it means the binary quits.
        if focused_id.path().len() <= 1 {
            warn!("PaneClose: focused leaf IS the root; binary quits");
            self.runners.clear();
            self.running = false;
            return;
        }
        // Strip the resolver's `path[0] = 0` implicit-wrapper
        // seed so [`remove_leaf`] walks the actual tree.
        let seed_path = focused_id.path();
        let tree_path: &[u16] = &seed_path[1..];
        // Drop the focused runner FIRST so its Drop-driven
        // close_tx emit lands BEFORE the tree mutates the
        // survivor's PaneId (next phase 1's `try_recv` then
        // sees the right LayerId for `close_pane`).
        self.runners.remove(self.focus);
        if let Err(e) = remove_leaf(&mut self.layout_root, tree_path) {
            warn!(
                error = ?e,
                "PaneClose: remove_leaf failed; treating as quit"
            );
            self.running = false;
            return;
        }
        if self.runners.is_empty() {
            self.running = false;
            return;
        }
        if self.focus >= self.runners.len() {
            self.set_focus(self.runners.len() - 1);
        }
        self.reconcile_runners(ReconcileMode::InPlace);
    }

    /// Carry-forward: `PanePreset(name)`. Look up
    /// `self.presets[name]`. If present, drop ALL runners
    /// (their `Drop`s revoke every `LayerId` via `close_tx`
    /// for the AGENTS.md Hard rule), swap
    /// `self.layout_root` to the named body, reset
    /// `self.focus = 0`, and [`Self::reconcile_runners`]
    /// spawns fresh runners against the new tree. Unknown
    /// `name` is a no-op (logged).
    fn swap_to_preset(&mut self, name: &str) {
        let Some(new_root) = self.presets.get(name).cloned() else {
            warn!(name, "PanePreset: unknown name; no-op");
            return;
        };
        self.runners.clear();
        self.layout_root = new_root;
        self.focus = 0;
        self.reconcile_runners(ReconcileMode::Wholesale);
        if self.runners.is_empty() {
            warn!(name, "PanePreset: new tree has zero leaves; quitting");
            self.running = false;
        }
    }

    /// Reconcile `self.runners` with the post-mutation
    /// `ComputedLayout::panes` for `self.layout_root` against
    /// `self.last_area`. The run loop's hot-path
    /// [`Self::relayout`] assumes a length-stable pairing
    /// (panes == runners); runtime mutations (`AppNewPane`,
    /// `PaneClose`, `PanePreset`) change one or both, so this
    /// method re-establishes the pairing invariant before the
    /// next tick.
    ///
    /// `mode` selects whether the reconciliation preserves
    /// surviving runners' `PaneLayerId` (in-place, for
    /// `AppNewPane` / `PaneClose`) or rotates all
    /// `PaneLayerId`s wholesale (for `PanePreset`, which is a
    /// topologically-different tree).
    ///
    /// Algorithm:
    /// 1. Compute `post_layout` against `self.last_area`.
    /// 2. Take ownership of `self.runners`.
    /// 3. Partition by [`ReconcileMode`]:
    ///    - `InPlace`: if a runner's `pane.label` is
    ///      preserved in `post_layout`, keep the runner
    ///      (its `PaneLayerId` stays per AGENTS.md Hard rule);
    ///      otherwise drop it (`Drop` enqueues `PaneLayerId`
    ///      on `close_tx`; next phase 1 revokes the
    ///      termcompositor layer).
    ///    - `Wholesale`: drop ALL old runners; every pane in
    ///      `post_layout` gets a freshly spawned runner with
    ///      a NEW `PaneLayerId` (the wholesale swap is a
    ///      different topology, not a same-instance mutation).
    /// 4. For each `post_layout.panes[i]`:
    ///    - If an `InPlace` survivor matches by label: rebind
    ///      the runner's `PaneId` (its `pre_order` may have
    ///      shifted across rebalance) + resize PTY to the new
    ///      rect; `PaneLayerId` unchanged.
    ///    - Else (genuinely new pane, or `Wholesale` slot):
    ///      spawn a fresh `PaneRunner` with a new
    ///      `PaneLayerId`.
    /// 5. Repaint `GraphicsState` cells to match
    ///    `self.last_area`.
    fn reconcile_runners(&mut self, mode: ReconcileMode) {
        let post_layout = match ComputedLayout::compute(&self.layout_root, self.last_area) {
            Ok(l) => l,
            Err(e) => {
                warn!(error = ?e, "reconcile: compute failed");
                return;
            }
        };
        let old_runners = std::mem::take(&mut self.runners);
        // InPlace: survivors keyed by pane label (panes keep
        // their labels across rebalance; PaneIds shift
        // pre_order, so PaneId-keyed matching would drop the
        // survivor). Wholesale: every old runner goes to
        // `to_drop` for `LayerId` revocation.
        // Survivors keyed by pane label, using a `Vec<PaneRunner>`
        // per label so configs with DUPLICATE labels don't silently
        // drop a survivor. Without the Vec, a second `insert(label, r)`
        // would overwrite the first runner, causing its `Drop` to fire
        // spuriously and the second pane with the same label to get a
        // fresh spawn instead of inheriting the survivor. The Vec
        // preserves insertion order so survivors are consumed in the
        // same order they were collected (pre-order runner order).
        let mut survivors: std::collections::HashMap<String, Vec<PaneRunner>> =
            std::collections::HashMap::new();
        let mut to_drop: Vec<PaneRunner> = Vec::with_capacity(old_runners.len());
        match mode {
            ReconcileMode::Wholesale => {
                to_drop = old_runners;
                // Phase 4 carry-forward: a preset reload
                // wholesale-swaps the layout tree so every
                // resolved PaneId rotates. The per-ZStack
                // focus map's old entries reference pane
                // instances that no longer exist; drop them
                // so the next `PaneStackCycle` /
                // `PaneStackDown` rebuilds the map against
                // the fresh tree.
                self.stack_focus.clear();
            }
            ReconcileMode::InPlace => {
                for r in old_runners {
                    if let Some(label) = r.computed().label.clone() {
                        let preserved = post_layout
                            .panes
                            .iter()
                            .any(|p| p.label.as_deref() == Some(label.as_str()));
                        if preserved {
                            survivors.entry(label).or_default().push(r);
                            continue;
                        }
                    }
                    to_drop.push(r);
                }
            }
        }
        // Drop dead runners: their `Drop` fires `close_tx`
        // -> next phase 1 revokes the termcompositor layers
        // (Hard rule: no orphan LayerIds).
        drop(to_drop);
        // Build the new runner Vec: rebind for survivors (so
        // the survivor's LayerId stays stable per Hard rule),
        // spawn fresh LayerIds for genuinely new panes / for
        // Wholesale slots.
        let mut new_runners: Vec<PaneRunner> = Vec::with_capacity(post_layout.panes.len());
        let env_vars = self.graphics.caps().to_env_vars();
        for pane in &post_layout.panes {
            let survivor_opt: Option<PaneRunner> = if matches!(mode, ReconcileMode::InPlace) {
                pane.label
                    .as_deref()
                    .map(str::to_string)
                    .and_then(|l| survivors.get_mut(&l).and_then(|v| v.pop()))
            } else {
                None
            };
            if let Some(mut r) = survivor_opt {
                // InPlace survivor: rebind `PaneId` (its
                // `pre_order` may have shifted) + resize PTY.
                // `PaneLayerId` is unchanged (Hard rule).
                let mut updated = pane.clone();
                if r.resize(pane.rect).is_err() {
                    warn!(
                        rect = ?pane.rect,
                        "reconcile: resize failed; keeping previous rect"
                    );
                    updated.rect = r.computed().rect;
                }
                r.rebind_pane(updated);
                new_runners.push(r);
            } else {
                // New pane (or Wholesale slot). Wholesale
                // swaps draw from [`alloc_layer_id`] so the
                // fresh LayerId never collides with a
                // `derive_layer_id(&pane.id) == LayerId(0)`
                // result the InPlace path would use for a
                // post-swap top pane that happens to land at
                // `pre_order == 0`.
                let layer_id = if matches!(mode, ReconcileMode::Wholesale) {
                    alloc_layer_id()
                } else {
                    // Tab-aware LayerId so a multi-tab reconcile
                    // produces collision-free ids across tabs.
                    // For a single-tab binary this matches the
                    // v1 `derive_layer_id` call sites
                    // byte-for-byte.
                    crate::derive_layer_id_for_tab(&pane.id, self.active_tab_idx_u32())
                };
                let tx: PaneCloseTx = self.close_tx.clone();
                match &pane.kind {
                    PaneKind::Widget { ref_name } => {
                        if let Some(factory) = self.widget_factories.get(ref_name) {
                            let raw = unsafe {
                                (factory.create)(cmdash_widget_sdk::CMDASH_WIDGET_ABI_VERSION)
                            };
                            if raw.is_null() {
                                warn!(name = %ref_name, "reconcile: widget create returned null");
                            } else {
                                let widget = unsafe { cmdash_widget_sdk::widget_from_raw(raw) };
                                new_runners.push(PaneRunner::spawn_widget(
                                    pane.clone(),
                                    layer_id,
                                    widget,
                                    Some(tx),
                                ));
                            }
                        } else {
                            warn!(name = %ref_name, "reconcile: widget not found");
                        }
                    }
                    PaneKind::Script => {
                        let cmd = pane.command.as_deref().unwrap_or("");
                        match crate::script_widget::ScriptWidget::spawn(cmd, pane.label.as_deref())
                        {
                            Ok(mut widget) => {
                                widget.set_theme(self.theme.clone());
                                new_runners.push(PaneRunner::spawn_widget(
                                    pane.clone(),
                                    layer_id,
                                    Box::new(widget),
                                    Some(tx),
                                ));
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    command = %cmd,
                                    ?layer_id,
                                    "reconcile: script spawn failed"
                                );
                            }
                        }
                    }
                    PaneKind::Shell => {
                        let shell = shell_spec_from_command(&pane.command, &self.shell);
                        match PaneRunner::spawn_with_graphics_and_env(
                            pane.clone(),
                            layer_id,
                            shell,
                            Some(tx),
                            env_vars.clone(),
                            pane.scrollback_capacity
                                .unwrap_or(cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY),
                        ) {
                            Ok(r) => new_runners.push(r),
                            Err(e) => {
                                warn!(
                                    error = ?e,
                                    ?layer_id,
                                    "reconcile: spawn failed"
                                );
                            }
                        }
                    }
                }
            }
        }
        // Drop any survivors that didn't get consumed (e.g.,
        // a label vanished across the layout swap).
        drop(survivors);
        self.runners = new_runners;
        self.graphics
            .set_cells((self.last_area.w, self.last_area.h));
    }

    /// The active tab's index as `u32` for
    /// `crate::derive_layer_id_for_tab`. The
    /// `reconcile_runners` `InPlace` path passes this as the
    /// second arg so multi-tab `LayerIds` are collision-free.
    /// For a single-tab binary this matches the v1
    /// `derive_layer_id` call sites byte-for-byte, so existing
    /// tests are unaffected. Private (no `pub`) because the
    /// only call site is `reconcile_runners` in this same
    /// `impl` block.
    fn active_tab_idx_u32(&self) -> u32 {
        self.tabs.active_idx() as u32
    }

    /// Copy the active tab's `focus` / `layout_root` /
    /// `stack_focus` into the v1 fields so v1 code paths see
    /// the post-tab-mutation state. The v1 `runners` field is
    /// NOT synced here (the active tab's `runners` is a Vec of
    /// clone-shells; the authoritative real runners are written
    /// by the subsequent [`Self::reconcile_runners`] call). The
    /// tick loop's `if !self.running { return Ok(()) }` check
    /// fires BEFORE any access after a `close_active_tab` of
    /// the last tab empties the stack, so this helper's no-op
    /// on empty stack is safe.
    fn sync_v1_from_active_tab(&mut self) {
        if let Some(active) = self.tabs.active() {
            self.focus = active.state.focus;
            self.layout_root = active.state.layout_root.clone();
            self.stack_focus = active.state.stack_focus.clone();
        }
    }

    /// Create a new tab with a single default-shell pane and
    /// switch focus to it. The active tab's `layout_root` is a
    /// 1-leaf `LayoutNode::Pane` so the subsequent
    /// `reconcile_runners(Wholesale)` spawns one fresh runner
    /// for the new tab.
    fn create_new_tab(&mut self) {
        let new_state = TabState {
            runners: Vec::new(),
            focus: 0,
            layout_root: LayoutNode::Pane(CfgPane {
                kind: PaneKind::Shell,
                label: None,
                command: None,
                scrollback_capacity: None,
            }),
            stack_focus: BTreeMap::new(),
        };
        self.tabs.push(new_state);
        self.sync_v1_from_active_tab();
        self.reconcile_runners(ReconcileMode::Wholesale);
    }

    /// Close the active tab. Empty stack quits the binary;
    /// otherwise sync the v1 fields and reconcile (Wholesale)
    /// so the termcompositor layer book-keeping tracks the new
    /// active tab's pane geometry.
    fn close_active_tab(&mut self) {
        let _removed = self.tabs.remove_active();
        if self.tabs.is_empty() {
            self.running = false;
        } else {
            self.sync_v1_from_active_tab();
            self.reconcile_runners(ReconcileMode::Wholesale);
        }
    }

    /// Switch to tab `n` (out-of-range is silent no-op per
    /// M-1..M-9 keybind semantics; mirrors `TabStack::switch_to`).
    fn switch_to_tab(&mut self, n: usize) {
        if self.tabs.switch_to(n) {
            self.sync_v1_from_active_tab();
            self.reconcile_runners(ReconcileMode::Wholesale);
        }
    }

    /// Drain the config-reload channel and apply the latest
    /// payload. Keybinds and presets swap immediately; layout
    /// changes trigger a Wholesale reconcile.
    // TODO: only called from tests after run()/run_loop() extraction.
    #[allow(dead_code)]
    fn apply_config_reload(&mut self, msg: ConfigReload) {
        self.bindings = Router::new(msg.keybinds);
        self.presets = msg.presets;
        self.status_bar = msg.status_bar;
        self.theme = msg.theme.unwrap_or_default();
        // Trigger relayout when status bar enable/disable changes chrome height.
        let w = self.last_area.w;
        let h = self.last_area.h;
        self.relayout(w, h);
        if let Some(new_layout) = msg.layout_root {
            if new_layout != self.layout_root {
                info!("config hot-reload: layout changed; rebuilding panes");
                self.layout_root = new_layout;
                self.reconcile_runners(ReconcileMode::Wholesale);
            } else {
                debug!("config hot-reload: keybinds+presets updated");
            }
        } else {
            debug!("config hot-reload: keybinds+presets updated");
        }
    }

    /// Phase 0.5: host SIGWINCH coalescer. Drains the resize slot
    /// queued during phase 0 and runs `relayout(...)` BEFORE the
    /// close-channel drain, so a resize signal that arrived mid-tick
    /// produces a fresh per-pane rect by the time rendering reads it.
    // TODO: only called from tests after run()/run_loop() extraction.
    #[allow(dead_code)]
    pub fn process_pending_resize(&mut self) {
        if let Some((w, h)) = self.pending_resize.take() {
            self.relayout(w, h);
        }
    }

    /// Phase 1 (part 1): drain the close-channel (Drop messages)
    /// FIRST so their revisions are visible before phase 2/3 in the
    /// same tick.
    // TODO: only called from tests after run()/run_loop() extraction.
    #[allow(dead_code)]
    pub fn drain_close_channel(&mut self) {
        let mut needs_keyboard_sync = false;
        let mut needs_bracketed_paste_sync = false;
        let mut needs_focus_reporting_sync = false;
        while let Ok(id) = self.close_rx.try_recv() {
            self.graphics.close_pane(id);
            // Only trigger a host sync when the closed pane's flags
            // were non-zero. A removed entry of 0 doesn't change the
            // union, so the sync would be a no-op.
            if self.pane_keyboard_flags.remove(&id).is_some_and(|f| f != 0) {
                needs_keyboard_sync = true;
            }
            if self
                .pane_bracketed_paste
                .remove(&id)
                .is_some_and(|enabled| enabled)
            {
                needs_bracketed_paste_sync = true;
            }
            if self
                .pane_focus_reporting
                .remove(&id)
                .is_some_and(|enabled| enabled)
            {
                needs_focus_reporting_sync = true;
            }
        }
        if needs_keyboard_sync {
            self.sync_host_keyboard_flags();
        }
        if needs_bracketed_paste_sync {
            self.sync_host_bracketed_paste();
        }
        if needs_focus_reporting_sync {
            self.sync_host_focus_reporting();
        }
    }

    /// Phase 1 (part 2): poll exits and tick runners.
    /// Returns the collected snapshots and a flag indicating whether
    /// all panes have exited.
    /// Recompute the union of all live pane keyboard enhancement
    /// flags and push/pop the host terminal's enhancement stack
    /// so that it matches. Called after pane snapshots are
    /// collected and after pane closures are drained.
    // TODO: only called from drain_close_channel (test-only).
    #[allow(dead_code)]
    pub fn sync_host_keyboard_flags(&mut self) {
        let union = if self.graphics.caps().supports_kitty_keyboard() {
            self.pane_keyboard_flags
                .values()
                .fold(0u8, |acc, &f| acc | f)
        } else {
            0
        };
        if union == self.host_keyboard_flags {
            return;
        }
        if union == 0 {
            self.pop_host_keyboard_flags();
        } else {
            self.push_host_keyboard_flags(union);
        }
        self.host_keyboard_flags = union;
    }

    /// Push a keyboard enhancement flag set onto the host terminal.
    /// Uses crossterm's `PushKeyboardEnhancementFlags` command.
    // TODO: only called from sync_host_keyboard_flags (test-only).
    #[allow(dead_code)]
    fn push_host_keyboard_flags(&mut self, flags: u8) {
        use crossterm::execute;
        let flags = KeyboardEnhancementFlags::from_bits_truncate(flags);
        if let Err(e) = execute!(std::io::stdout(), PushKeyboardEnhancementFlags(flags)) {
            warn!(error = ?e, "failed to push keyboard enhancement flags");
            return;
        }
        self.host_keyboard_pushed = true;
    }

    /// Pop the previously-pushed keyboard enhancement flags from
    /// the host terminal. Uses crossterm's `PopKeyboardEnhancementFlags`
    /// command. Safe to call multiple times: the second pop is a no-op.
    // TODO: only called from sync_host_keyboard_flags (test-only).
    #[allow(dead_code)]
    pub fn pop_host_keyboard_flags(&mut self) {
        use crossterm::execute;
        if !self.host_keyboard_pushed {
            return;
        }
        if let Err(e) = execute!(std::io::stdout(), PopKeyboardEnhancementFlags) {
            warn!(error = ?e, "failed to pop keyboard enhancement flags");
            return;
        }
        self.host_keyboard_pushed = false;
    }

    /// Synchronize the host terminal's bracketed-paste mode with
    /// the union of all live pane requests. If any pane has
    /// requested bracketed paste, enable it on the host so
    /// crossterm emits `Event::Paste`; disable it when no pane
    /// still wants it.
    // TODO: only called from drain_close_channel (test-only).
    #[allow(dead_code)]
    pub fn sync_host_bracketed_paste(&mut self) {
        let any = self.graphics.caps().supports_bracketed_paste()
            && self.pane_bracketed_paste.values().any(|&v| v);
        if any == self.host_bracketed_paste {
            return;
        }
        if any {
            self.push_host_bracketed_paste();
        } else {
            self.pop_host_bracketed_paste();
        }
        self.host_bracketed_paste = any;
    }

    /// Enable bracketed paste on the host terminal.
    // TODO: only called from sync_host_bracketed_paste (test-only).
    #[allow(dead_code)]
    fn push_host_bracketed_paste(&mut self) {
        use crossterm::execute;
        if self.host_bracketed_paste_pushed {
            return;
        }
        if let Err(e) = execute!(std::io::stdout(), EnableBracketedPaste) {
            warn!(error = ?e, "failed to enable bracketed paste");
            return;
        }
        self.host_bracketed_paste_pushed = true;
    }

    /// Disable bracketed paste on the host terminal. Safe to call
    /// multiple times: the second disable is a no-op.
    // TODO: only called from sync_host_bracketed_paste (test-only).
    #[allow(dead_code)]
    fn pop_host_bracketed_paste(&mut self) {
        use crossterm::execute;
        if !self.host_bracketed_paste_pushed {
            return;
        }
        if let Err(e) = execute!(std::io::stdout(), DisableBracketedPaste) {
            warn!(error = ?e, "failed to disable bracketed paste");
            return;
        }
        self.host_bracketed_paste_pushed = false;
    }

    /// Synchronize the host terminal's focus-change reporting mode
    /// with the union of all live pane requests. If any pane has
    /// requested focus reporting, enable it on the host so crossterm
    /// emits `Event::FocusGained` / `Event::FocusLost`.
    // TODO: only called from drain_close_channel (test-only).
    #[allow(dead_code)]
    pub fn sync_host_focus_reporting(&mut self) {
        let any = self.graphics.caps().supports_focus_events()
            && self.pane_focus_reporting.values().any(|&v| v);
        if any == self.host_focus_reporting {
            return;
        }
        if any {
            self.push_host_focus_reporting();
        } else {
            self.pop_host_focus_reporting();
        }
        self.host_focus_reporting = any;
    }

    /// Enable focus-change reporting on the host terminal.
    // TODO: only called from sync_host_focus_reporting (test-only).
    #[allow(dead_code)]
    fn push_host_focus_reporting(&mut self) {
        use crossterm::event::EnableFocusChange;
        use crossterm::execute;
        if self.host_focus_reporting_pushed {
            return;
        }
        if let Err(e) = execute!(std::io::stdout(), EnableFocusChange) {
            warn!(error = ?e, "failed to enable focus-change reporting");
            return;
        }
        self.host_focus_reporting_pushed = true;
    }

    /// Disable focus-change reporting on the host terminal. Safe to
    /// call multiple times: the second disable is a no-op.
    // TODO: only called from sync_host_focus_reporting (test-only).
    #[allow(dead_code)]
    fn pop_host_focus_reporting(&mut self) {
        use crossterm::event::DisableFocusChange;
        use crossterm::execute;
        if !self.host_focus_reporting_pushed {
            return;
        }
        if let Err(e) = execute!(std::io::stdout(), DisableFocusChange) {
            warn!(error = ?e, "failed to disable focus-change reporting");
            return;
        }
        self.host_focus_reporting_pushed = false;
    }

    /// Forward a focus-in/focus-out event to the focused pane when
    /// that pane has requested focus reporting. Widget panes and
    /// panes that have not enabled focus reporting are skipped.
    fn forward_focus_event_to_focused_pane(&mut self, focused: bool) {
        if self.runners.is_empty() {
            return;
        }
        if !self.graphics.caps().supports_focus_events() {
            return;
        }
        let Some(runner) = self.runners.get_mut(self.focus) else {
            return;
        };
        if runner.is_widget() {
            return;
        }
        if !runner.focus_reporting_enabled() {
            return;
        }
        Self::write_focus_event_to_runner(runner, focused);
    }

    /// Write a focus-in/focus-out event (`CSI I` / `CSI O`) to a
    /// specific runner. The caller is responsible for ensuring the
    /// runner is a PTY pane that has requested focus reporting.
    pub fn write_focus_event_to_runner(runner: &mut PaneRunner, focused: bool) {
        let bytes = if focused {
            b"\x1b[I".to_vec()
        } else {
            b"\x1b[O".to_vec()
        };
        if let Err(e) = runner.write_input(&bytes) {
            debug!(error = ?e, layer_id = ?runner.layer_id(), "write_input failed for focus event");
        }
    }

    /// Drain `PaneEvent::KeyboardEnhancement` events from the
    /// freshly-collected pane snapshots and update
    /// `self.pane_keyboard_flags`. After updating, recompute the
    /// host terminal's enhancement state.
    // TODO: only called from tests after run()/run_loop() extraction.
    #[allow(dead_code)]
    pub fn update_keyboard_flags_from_snapshots(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) {
        let changed = crate::pane::collect_keyboard_enhancement_flags(
            &self.runners,
            snapshots,
            &mut self.pane_keyboard_flags,
        );
        if changed {
            self.sync_host_keyboard_flags();
        }
    }

    /// Drain `PaneEvent::BracketedPaste` events from the freshly-
    /// collected pane snapshots and update `self.pane_bracketed_paste`.
    /// After updating, recompute the host terminal's bracketed-paste
    /// state.
    // TODO: only called from tests after run()/run_loop() extraction.
    #[allow(dead_code)]
    pub fn update_bracketed_paste_from_snapshots(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) {
        let changed = crate::pane::collect_bracketed_paste_flags(
            &self.runners,
            snapshots,
            &mut self.pane_bracketed_paste,
        );
        if changed {
            self.sync_host_bracketed_paste();
        }
    }

    /// Drain `PaneEvent::FocusReporting` events from the freshly-
    /// collected pane snapshots and update `self.pane_focus_reporting`.
    /// After updating, recompute the host terminal's focus-change
    /// reporting state and send the current host focus state to any
    /// pane that just enabled focus reporting.
    // TODO: only called from tests after run()/run_loop() extraction.
    #[allow(dead_code)]
    pub fn update_focus_reporting_from_snapshots(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) {
        // Capture the prior tick's state so we can detect panes that
        // just enabled focus reporting and immediately report the
        // current host focus state to them.
        let prev_focus_reporting = self.pane_focus_reporting.clone();
        let changed = crate::pane::collect_focus_reporting_flags(
            &self.runners,
            snapshots,
            &mut self.pane_focus_reporting,
        );
        if changed {
            self.sync_host_focus_reporting();
        }
        // Send the initial focus state to any pane that just enabled
        // focus reporting. Standard terminal behavior: on `CSI ? 1004 h`,
        // the terminal immediately reports whether it is focused.
        for runner in self.runners.iter_mut() {
            let layer_id = runner.layer_id();
            let was_enabled = prev_focus_reporting
                .get(&layer_id)
                .copied()
                .unwrap_or(false);
            let is_enabled = self
                .pane_focus_reporting
                .get(&layer_id)
                .copied()
                .unwrap_or(false);
            if is_enabled && !was_enabled && self.graphics.caps().supports_focus_events() {
                Self::write_focus_event_to_runner(runner, self.host_focused);
            }
        }
    }

    // TODO: only called from tests after run()/run_loop() extraction.
    #[allow(dead_code)]
    pub fn tick_runners(
        &mut self,
    ) -> Result<RunnerTickResult, Box<dyn std::error::Error + Send + Sync>> {
        let mut snapshots: Vec<Option<cmdash_pty::PaneTerminalState>> =
            Vec::with_capacity(self.runners.len());
        let mut all_exited = true;
        for runner in self.runners.iter_mut() {
            match runner.try_wait_exit()? {
                Some(_code) => {
                    debug!(layer_id = ?runner.layer_id(), "pane exited");
                }
                None => {
                    all_exited = false;
                }
            }
            // Widget panes have no PTY to poll — skip tick()
            // and push None so the render loop handles them via
            // widget.render() instead of blit_grid.
            if runner.is_widget() {
                snapshots.push(None);
            } else {
                snapshots.push(Some(runner.tick()?));
            }
        }
        Ok(RunnerTickResult {
            snapshots,
            all_exited,
        })
    }

    /// Render a crossterm `Event` into the file-log payload we
    /// emit at `handle_event_full`'s entry point with the printable
    /// payload content REDACTED before persistence under
    /// `--log=<path>`.
    ///
    /// **Privacy story.** An unredacted
    /// `debug!("crossterm event = {:?}", evt)` trace would emit
    /// printable text byte-for-byte into the `--log=<path>`
    /// subscriber. Over a long `--log=foo.log` session that means
    /// any printable text reaching the focused pane (passwords,
    /// API keys, essays, clipboard paste contents, etc.) gets
    /// persisted to the log file verbatim -- a privacy leak.
    /// Rather than gate the whole trace on
    /// `cfg!(debug_assertions)` (which would strip the trace from
    /// release binaries -- exactly the builds where the trace is
    /// most useful for field debugging), this helper redacts
    /// printable payloads while keeping everything else
    /// observable.
    ///
    /// **What's kept vs redacted.**
    /// - REDACTED: `KeyCode::Char(_)` printable character
    ///   (`Char(<redacted char>)` sentinel).
    /// - REDACTED: `Event::Paste(_)` pasted string content
    ///   (`Paste(<redacted>)` sentinel). Reviewer-feedback
    ///   catch here -- clipboard paste events carry
    ///   typed passwords / API keys / etc. verbatim, same
    ///   severity as the `Char(c)` keystroke leak.
    /// - KEPT (full Debug escape): `modifiers`, `kind`, `state`,
    ///   every non-`Char` `KeyCode` variant (`F(n)`, arrows,
    ///   `Backspace`, `Tab`, etc. carry no printable text),
    ///   `Resize`, `Mouse` (carry geometry + button state;
    ///   no printable text), `FocusGained`, `FocusLost`.
    ///
    /// **Open-fallback trade-off.** The match's
    /// `_ => format!("{:?}", evt)` arm forwards all UNKNOWN
    /// future crossterm `Event` variants verbatim. If crossterm
    /// ever adds a variant carrying printable text (a hypothetical
    /// `SpeechInput(String)` or `SnippetInsert(String)` payload),
    /// it auto-leaks through this fall-through -- the same root
    /// cause as the `Event::Paste(String)` leak this helper closes.
    /// Re-audit this arm every time `crossterm` is upgraded;
    /// the round-2 paste leak is the precedent.
    ///
    /// **Hot-path cost.** One `String` allocation per event the
    /// logger captures (controlled by the file-only subscriber's
    /// level filter; off the hot path under the default launch
    /// where `--log=<path>` is absent). Allocations are bounded
    /// by the real event rate (human-keystroke-rate for `Key`
    /// events, paste-burst-rate for `Paste`, zero for `Resize`
    /// outside a host drag-resize burst).
    /// Return whether the focused pane has requested bracketed paste
    /// mode. Widget panes are treated as not supporting bracketed paste.
    fn focused_bracketed_paste_enabled(&self) -> bool {
        let Some(layer_id) = self.runners.get(self.focus).map(|r| r.layer_id()) else {
            return false;
        };
        self.pane_bracketed_paste
            .get(&layer_id)
            .copied()
            .unwrap_or(false)
    }

    /// Forward a paste event to the focused pane. When the pane has
    /// requested bracketed paste, wrap the content in `ESC [ 200 ~`
    /// and `ESC [ 201 ~` so the child can treat it as a paste. When
    /// bracketed paste is not enabled, forward the raw bytes as-is.
    /// Build the byte payload for a paste event directed at the
    /// focused pane. When the pane has requested bracketed paste,
    /// wrap the content in `ESC [ 200 ~` and `ESC [ 201 ~` so the
    /// child can treat it as a paste. Otherwise forward the raw
    /// bytes as-is.
    pub fn prepare_paste_bytes(&self, text: &str) -> Vec<u8> {
        let bracketed = self.focused_bracketed_paste_enabled()
            && self.graphics.caps().supports_bracketed_paste();
        if bracketed {
            let mut out = Vec::with_capacity(text.len() + 12);
            out.extend_from_slice(b"\x1b[200~");
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\x1b[201~");
            out
        } else {
            text.as_bytes().to_vec()
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.runners.is_empty() {
            return;
        }
        // Widget panes do not have a PTY and cannot receive raw input;
        // skip paste events for them.
        if self
            .runners
            .get(self.focus)
            .map(|r| r.is_widget())
            .unwrap_or(false)
        {
            return;
        }
        let bytes = self.prepare_paste_bytes(text);
        if let Some(runner) = self.runners.get_mut(self.focus) {
            if let Err(e) = runner.write_input(&bytes) {
                debug!(error = ?e, layer_id = ?runner.layer_id(), "write_input failed for paste");
            }
        }
    }
}

pub fn render_tab_bar(
    buf: &mut ratatui::buffer::Buffer,
    tabs: &TabStack<TabState>,
    theme: &cmdash_config::Theme,
) {
    let bar_width = buf.area.width as usize;
    // Clear the tab bar row.
    for x in 0..bar_width {
        let cell = &mut buf[(x as u16, 0)];
        cell.set_symbol(" ");
        cell.set_style(
            Style::default()
                .bg(theme.tab_bar_bg())
                .fg(theme.tab_inactive_fg()),
        );
    }
    let mut col: usize = 0;
    for (idx, tab) in tabs.iter().enumerate() {
        if col >= bar_width {
            break;
        }
        let is_active = idx == tabs.active_idx();
        let label = tab.label.as_deref().filter(|l| !l.is_empty());
        let tab_text = if let Some(l) = label {
            format!(" {}:{} ", idx + 1, l)
        } else {
            format!(" {} ", idx + 1)
        };
        let style = if is_active {
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .bg(theme.tab_inactive_bg())
                .fg(theme.tab_inactive_fg())
        };
        for ch in tab_text.chars() {
            if col >= bar_width {
                break;
            }
            let cell = &mut buf[(col as u16, 0)];
            cell.set_symbol(&ch.to_string());
            cell.set_style(style);
            col += 1;
        }
        // Separator space between tabs.
        if col < bar_width && idx + 1 < tabs.len() {
            col += 1;
        }
    }
}

pub fn redacted_event_debug(evt: &Event) -> String {
    match evt {
        Event::Key(KeyEvent {
            code: KeyCode::Char(_),
            modifiers,
            kind,
            state,
            ..
        }) => format!(
            "Key(KeyEvent {{ code: Char(<redacted char>), \
             modifiers: {:?}, kind: {:?}, state: {:?} }})",
            modifiers, kind, state,
        ),
        Event::Paste(_) => "Paste(<redacted>)".to_string(),
        _ => format!("{:?}", evt),
    }
}

/// Encode an unmatched key press as PTY-friendly bytes for the
/// focused pane. Returns `None` for variants that should NOT
/// leak to the PTY (Insert, F-keys above 4, media keys,
/// modifier-only events).
/// Map a crossterm [`KeyCode`] to a Kitty keyboard protocol key code.
/// Returns `None` for keys that have no Kitty protocol representation.
pub fn kitty_key_code(code: &KeyCode) -> Option<u32> {
    match code {
        KeyCode::Char(c) => Some(*c as u32),
        KeyCode::Enter => Some(57351),
        KeyCode::Tab => Some(57352),
        KeyCode::Backspace => Some(57353),
        KeyCode::Esc => Some(57350),
        KeyCode::Delete => Some(57355),
        KeyCode::Insert => Some(57354),
        KeyCode::Left => Some(57356),
        KeyCode::Right => Some(57357),
        KeyCode::Up => Some(57358),
        KeyCode::Down => Some(57359),
        KeyCode::PageUp => Some(57360),
        KeyCode::PageDown => Some(57361),
        KeyCode::Home => Some(57362),
        KeyCode::End => Some(57363),
        KeyCode::F(n) => Some(57369u32.saturating_add(*n as u32)),
        KeyCode::CapsLock => Some(57364),
        KeyCode::ScrollLock => Some(57365),
        KeyCode::NumLock => Some(57366),
        KeyCode::PrintScreen => Some(57367),
        KeyCode::Pause => Some(57368),
        KeyCode::Menu => Some(57369),
        _ => None,
    }
}

/// Map crossterm [`KeyModifiers`] to a Kitty keyboard protocol modifier bitmask.
pub fn kitty_modifiers(modifiers: KeyModifiers) -> u8 {
    let mut mods = 0u8;
    if modifiers.contains(KeyModifiers::SHIFT) {
        mods |= 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        mods |= 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        mods |= 4;
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        mods |= 8;
    }
    mods
}

/// Map a crossterm [`KeyEventKind`] to a Kitty keyboard protocol event type.
pub fn kitty_event_type(kind: KeyEventKind) -> u8 {
    match kind {
        KeyEventKind::Press => 1,
        KeyEventKind::Repeat => 2,
        KeyEventKind::Release => 3,
    }
}

/// Encode a single key event using the Kitty keyboard protocol CSI `u` form.
/// Returns `None` when the key has no Kitty protocol representation.
pub fn encode_kitty_key_event(
    code: &KeyCode,
    modifiers: KeyModifiers,
    kind: KeyEventKind,
) -> Option<Vec<u8>> {
    let key = kitty_key_code(code)?;
    let mods = kitty_modifiers(modifiers);
    let event_type = kitty_event_type(kind);

    let mut seq = format!("\x1b[{}u", key);
    if mods != 0 {
        seq = format!("\x1b[{};{}u", key, mods);
    }
    if event_type != 1 {
        // Repeat (2) or release (3): include the event type. Modifiers are
        // required when event type is present, even if zero.
        seq = format!("\x1b[{};{}:{}u", key, mods, event_type);
    }
    Some(seq.into_bytes())
}

pub fn event_to_bytes(code: KeyCode) -> Option<Vec<u8>> {
    let bytes: &[u8] = match code {
        KeyCode::Enter => b"\r",
        KeyCode::Backspace => b"\x7f",
        KeyCode::Tab => b"\t",
        KeyCode::Esc => b"\x1b",
        KeyCode::Up => b"\x1b[A",
        KeyCode::Down => b"\x1b[B",
        KeyCode::Right => b"\x1b[C",
        KeyCode::Left => b"\x1b[D",
        KeyCode::Home => b"\x1b[H",
        KeyCode::End => b"\x1b[F",
        KeyCode::PageUp => b"\x1b[5~",
        KeyCode::PageDown => b"\x1b[6~",
        KeyCode::Delete => b"\x1b[3~",
        KeyCode::F(1) => b"\x1b[OP",
        KeyCode::F(2) => b"\x1b[OQ",
        KeyCode::F(3) => b"\x1b[OR",
        KeyCode::F(4) => b"\x1b[OS",
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            return Some(s.as_bytes().to_vec());
        }
        _ => return None,
    };
    Some(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::{GraphicsState, Metrics};
    use crate::pane::PaneRunner;
    use cmdash_config::{LayoutNode, Pane as CfgPane, PaneKind, Ratio, SplitAxis};
    use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
    use cmdash_pty::{PaneLayerId, PanePtyOps, PaneTerminalState, PtyError, TextGrid};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::BTreeMap;
    use tokio::sync::mpsc::unbounded_channel;

    struct StubPty {
        layer_id: PaneLayerId,
    }

    impl PanePtyOps for StubPty {
        fn layer_id(&self) -> PaneLayerId {
            self.layer_id
        }
        fn resize(&mut self, _: u16, _: u16) -> Result<(), PtyError> {
            Ok(())
        }
        fn write(&mut self, _: &[u8]) -> Result<usize, PtyError> {
            Ok(0)
        }
        fn advance(&mut self, _: &[u8]) -> Result<(), PtyError> {
            Ok(())
        }
        fn snapshot(&mut self) -> PaneTerminalState {
            PaneTerminalState {
                grid: TextGrid::new(80, 24),
                cols: 80,
                rows: 24,
                pending_events: vec![],
            }
        }
        fn try_wait(&mut self) -> Result<Option<i32>, PtyError> {
            Ok(None)
        }
        fn kill(&mut self) -> Result<(), PtyError> {
            Ok(())
        }
        fn keyboard_flags(&self) -> u8 {
            0
        }
        fn focus_reporting_enabled(&self) -> bool {
            false
        }
        fn scrollback_up(&mut self, _: usize) {}
        fn scrollback_down(&mut self, _: usize) {}
        fn scrollback_reset(&mut self) {}
        fn in_scrollback(&self) -> bool {
            false
        }
        fn in_alternate_screen(&self) -> bool {
            false
        }
    }

    fn single() -> LayoutNode {
        LayoutNode::Pane(CfgPane {
            kind: PaneKind::Shell,
            label: None,
            command: None,
            scrollback_capacity: None,
        })
    }

    fn split_h() -> LayoutNode {
        LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: Ratio(50),
            children: vec![single(), single()],
        }
    }

    fn make_ctx<'a>(
        terminal: &'a mut Terminal<TestBackend>,
        layout_root: LayoutNode,
        runners: Vec<PaneRunner>,
    ) -> TickContext<'a, TestBackend> {
        let (close_tx, close_rx) = unbounded_channel();
        let area = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        TickContext::new_full(
            runners,
            cmdash_keybinds::Router::new(vec![]),
            0,
            true,
            close_tx,
            close_rx,
            GraphicsState::new(Metrics::default(), (80, 24)),
            terminal,
            Duration::from_millis(16),
            layout_root,
            None,
            area,
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
            None,
        )
    }

    fn make_runners(layout_root: &LayoutNode) -> Vec<PaneRunner> {
        let area = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let layout = ComputedLayout::compute(layout_root, area).unwrap();
        layout
            .panes
            .iter()
            .enumerate()
            .map(|(i, pane)| {
                let lid = PaneLayerId(i as u64 + 1);
                let pty: Box<dyn PanePtyOps + Send> = Box::new(StubPty { layer_id: lid });
                PaneRunner::with_pty_for_test(pane.clone(), lid, pty, None)
            })
            .collect()
    }

    #[test]
    fn relayout_zero_area_is_noop() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let runners = make_runners(&single());
        let mut ctx = make_ctx(&mut terminal, single(), runners);
        let old_area = ctx.last_area;

        ctx.relayout(0, 24);
        assert_eq!(ctx.last_area, old_area);

        ctx.relayout(80, 0);
        assert_eq!(ctx.last_area, old_area);
    }

    #[test]
    fn relayout_updates_last_area_and_graphics_cells() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let runners = make_runners(&single());
        let mut ctx = make_ctx(&mut terminal, single(), runners);

        ctx.relayout(100, 50);

        assert_eq!(ctx.last_area.w, 100);
        assert_eq!(ctx.last_area.h, 50 - TAB_BAR_HEIGHT);
        assert_eq!(ctx.graphics.cells(), (100, 50));
    }

    #[test]
    fn relayout_propagates_resize_to_all_runners() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let runners = make_runners(&split_h());
        let mut ctx = make_ctx(&mut terminal, split_h(), runners);

        ctx.relayout(80, 24);

        // Each runner should have been resized to its pane's rect.
        for runner in &ctx.runners {
            let rect = runner.computed().rect;
            assert_eq!(rect.w, 40);
        }
    }

    #[test]
    fn relayout_with_status_bar_reduces_layout_height() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let runners = make_runners(&single());
        let mut ctx = make_ctx(&mut terminal, single(), runners);
        ctx.status_bar = Some(cmdash_config::Bar {
            enabled: true,
            ..Default::default()
        });

        ctx.relayout(80, 24);

        assert_eq!(ctx.last_area.h, 24 - TAB_BAR_HEIGHT - STATUS_BAR_HEIGHT);
    }

    #[test]
    fn relayout_mismatched_pane_count_skips_runner_resize() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        // Layout has 2 panes but only 1 runner to trigger mismatch.
        let layout_root = split_h();
        let mut runners = make_runners(&split_h());
        runners.truncate(1);
        let original_rect = runners[0].computed().rect;
        let mut ctx = make_ctx(&mut terminal, layout_root, runners);
        let old_area = ctx.last_area;

        ctx.relayout(80, 24);

        // last_area is NOT updated when runner/pane counts mismatch,
        // because the mismatch check returns early before updating state.
        assert_eq!(ctx.last_area, old_area);
        // The single runner should retain its original rect because
        // per-pane resize is skipped when counts diverge.
        assert_eq!(ctx.runners[0].computed().rect, original_rect);
    }
}
