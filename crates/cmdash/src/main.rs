//! cmdash binary: drives the layout → PTY → ratatui text body and
//! dashcompositor kitty graphics render loop, with crossterm input
//! dispatch via cmdash-keybinds.
//!
//! AGENTS.md §"Rendering pipeline" -- phase 3a draws the cell body
//! through ratatui and phase 3b emits dashcompositor graphics via
//! the passthrough encoder. v1 is single-tab with sync IO via
//! per-pane reader threads; unmatched key presses are forwarded
//! as raw bytes to the focused pane's PTY via
//! `PaneRunner::write_input`.
//!
//! ## Host SIGWINCH (crossterm `Event::Resize`) wiring
//!
//! v2 lifts the v1 hardcoded `DEFAULT_AREA_*(80, 24)` initial
//! cell-grid area to the host terminal's actual size, queried
//! via `crossterm::terminal::size()` at `cmdash::run` entry. A
//! subsequent resize signal is delivered to the binary's tick
//! loop as `Event::Resize(w, h)`; `handle_event` writes the
//! coalesced value into `TickContext::pending_resize`, and the
//! `tick_loop` drains it at the top of each tick to call
//! `TickContext::relayout(w, h)`, which
//!
//! 1. recomputes `ComputedLayout::compute` against `(w, h)`,
//! 2. per-pane calls `PaneRunner::resize(pane.rect)` (v2's
//!    full-rect signature carries the layout-engine `(x, y)`
//!    origin forward into the cached cell-grid rect), and
//! 3. propagates the new dimensions to `GraphicsState::set_cells`
//!    so the dashcompositor framebuffer stays in-sync.
//!
//! ## Pane drop → dashcompositor teardown
//!
//! Each `PaneRunner::Drop` sends its `PaneLayerId` into a
//! shared `mpsc::Sender<PaneLayerId>`. The receiver lives in
//! `cmdash::run` and is drained at the start of each tick so
//! the corresponding `dashcompositor` layers are revoked
//! without forcing `GraphicsState` into an `Arc<Mutex<...>>`
//! (which fails `clippy::arc-with-non-send-sync` because
//! `dashcompositor::LayerStack` is not `Sync`).

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use cmdash::graphics::{GraphicsState, Metrics};
use cmdash::pane::{PaneCloseTx, PaneRunner};
use cmdash::render::{blit_cursor, blit_grid};
use cmdash_config::{parse as parse_config, KeyAction, LayoutNode};
use cmdash_keybinds::Router;
use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
use cmdash_pty::{PaneEvent, ShellSpec};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::Terminal;
use tracing::{debug, info, warn};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    info!("cmdash starting (ratatui text body + dashcompositor kitty graphics)");
    if let Err(e) = run() {
        eprintln!("cmdash: fatal: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cfg_text = include_str!("../config.kdl");
    let cfg = parse_config(cfg_text)?;
    // Move `cfg.layout` out of `cfg` (Option<LayoutNode>) so the
    // layout tree can be moved into `TickContext::new` at the
    // bottom of this function and reused on every host-driven
    // resize. `cfg.keybinds` is still consumed directly further
    // down. AGENTS.md §"PaneId stability" — moving the tree by
    // value does not alter its pre-order leaf numbering, so the
    // layout engine produces the same `cmdash_layout::PaneId`
    // values before and after.
    let layout_root: LayoutNode = cfg
        .layout
        .ok_or_else(|| "config.kdl missing `layout { ... }` block".to_string())?;

    // Source the initial cell-grid area from the live host
    // terminal, NOT a hardcoded default. A real SIGWINCH
    // signal later (crossterm `Event::Resize`) drives the
    // tick-loop's `TickContext::relayout(...)` helper. The
    // fallback below covers only non-TTY CI / window-snap
    // / hide-and-restore zero-area transients.
    let host_size = crossterm::terminal::size();
    let total = match host_size {
        Ok((0, _)) | Ok((_, 0)) => {
            warn!(
                raw = ?host_size,
                "host terminal reports zero-area; defaulting to 80x24"
            );
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            }
        }
        Ok((w, h)) => LayoutRect { x: 0, y: 0, w, h },
        Err(e) => {
            warn!(
                error = %e,
                "crossterm::terminal::size failed; defaulting to 80x24"
            );
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            }
        }
    };
    let layout = ComputedLayout::compute(&layout_root, total)?;
    info!(
        panes = layout.panes.len(),
        cols = total.w,
        rows = total.h,
        "layout resolved"
    );

    let graphics = GraphicsState::new(Metrics::default(), (total.w, total.h));

    // PaneRunner::Drop sends its `PaneLayerId` into this channel;
    // tick_loop drains it at the start of phase 1 to call
    // `GraphicsState::close_pane` for each id. Drop order: the
    // Vec<PaneRunner> drops before `graphics` (reverse
    // declaration order), so Drop-driven sends land on a live
    // receiver owned by `close_rx`.
    let (close_tx, close_rx): (Sender<cmdash_pty::PaneLayerId>, _) = std::sync::mpsc::channel();

    let mut runners: Vec<PaneRunner> = Vec::with_capacity(layout.panes.len());
    for pane in &layout.panes {
        let layer_id = cmdash::derive_layer_id(&pane.id);
        let tx: PaneCloseTx = close_tx.clone();
        match PaneRunner::spawn_with_graphics(
            pane.clone(),
            layer_id,
            ShellSpec::LoginShell,
            Some(tx),
        ) {
            Ok(r) => runners.push(r),
            Err(e) => warn!(error = %e, ?layer_id, "failed to spawn pane"),
        }
    }
    if runners.is_empty() {
        return Err("no panes were spawned; aborting".into());
    }
    // Drop the binary's primary sender so Drop sends a `Send`
    // error if anyone tries to push after `runner.close()`,
    // but keep a clone in each `PaneRunner`.
    drop(close_tx);

    let bindings = Router::new(cfg.keybinds);
    // `focus` and `running` are MOVED into `TickContext::new` below;
    // they are never mutated locally. `guard` and `ctx` stay `mut`
    // because `guard.as_mut()` and `ctx.run()` both take `&mut self`,
    // and `runners` is `mut` because the spawn loop calls
    // `runners.push(r)`.
    let focus: usize = 0;
    let running = true;

    let mut guard = TerminalGuard::enter()?;
    let tick = Duration::from_millis(33);
    let mut ctx = TickContext::new(
        runners,
        bindings,
        focus,
        running,
        close_rx,
        graphics,
        guard.as_mut(),
        tick,
        layout_root,
        None,
    );
    ctx.run()
}

/// Concrete backend alias used by [`TerminalGuard`] and the
/// production [`Terminal`]. Tests can swap to a `TestBackend`
/// locally without going through the guard.
type CmdashBackend = ratatui::backend::CrosstermBackend<std::io::Stdout>;

/// Owns a `Terminal<CmdashBackend>` whose setup (raw mode +
/// alternate screen + mouse capture) is reverted by [`Drop`] on
/// error or normal return. Without this guard, an early `?` in
/// the setup between `enable_raw_mode()` and the `run()` loop
/// would strand the user in the alternate screen.
///
/// Pinned to [`CmdashBackend`] (rather than generic over
/// `Backend`) so the [`Drop`] impl never has to coordinate
/// coherence bounds across `Write`/`Backend`/`Execute` -- the
/// guard is used in exactly one configuration.
struct TerminalGuard {
    terminal: Option<Terminal<CmdashBackend>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        use crossterm::event::EnableMouseCapture;
        use crossterm::execute;
        use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
        let mut stdout = std::io::stdout();
        enable_raw_mode()?;
        // Construct the guard BEFORE entering alt screen + mouse
        // capture + creating the terminal so Drop runs cleanup
        // even if `Terminal::new` fails between those steps.
        let mut guard = Self { terminal: None };
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CmdashBackend::new(stdout);
        guard.terminal = Some(Terminal::new(backend)?);
        Ok(guard)
    }

    fn as_mut(&mut self) -> &mut Terminal<CmdashBackend> {
        self.terminal.as_mut().expect("terminal owned by guard")
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        use crossterm::event::DisableMouseCapture;
        use crossterm::execute;
        use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
        if let Some(mut t) = self.terminal.take() {
            // Success path: revert via the terminal's own backend.
            let _ = execute!(t.backend_mut(), LeaveAlternateScreen, DisableMouseCapture);
            let _ = t.show_cursor();
        } else {
            // `Terminal::new` failed AFTER alt screen + mouse capture
            // were entered. The original `stdout` was consumed by
            // the dropped backend; we open a fresh handle to the
            // same fd for a best-effort alt-screen revert. Raw
            // mode is restored by the kernel on process exit.
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        }
        let _ = disable_raw_mode();
    }
}

/// Pivot struct for one tick of the AGENTS.md rendering pipeline.
///
/// Bundles the ten per-frame arguments of the prior free
/// function `tick_loop` into one struct so `cmdash::run` calls
/// `TickContext::run` as a single-shot pipeline call instead of
/// threading individual references through a 9-argument
/// function (which tripped `clippy::too_many_arguments`).
///
/// All fields are **owned** except `terminal`, which is borrowed
/// from a surrounding [`TerminalGuard`] whose `Drop` reverts the
/// alt-screen and mouse-capture on exit. The other nine are
/// owned because `cmdash::run` builds the struct once and
/// runs the loop to completion — there is no caller that needs
/// post-loop access to the runners, graphics, or bindings.
///
/// AGENTS.md §"Rendering pipeline (one frame)" enumerates the
/// six tick phases (input, drain, snapshot, event route,
/// ratatui draw, dashcompositor emit, sleep). The field names
/// mirror those phases: `runners` + `bindings` + `focus` +
/// `running` are phase 0/1/2 inputs; `close_rx` + `graphics` +
/// `tick` are phase 1/2/3b/4 resources; `terminal` is phase 3a;
/// `layout_root` + `pending_resize` drive phase 0.5 (host
/// SIGWINCH relayout).
struct TickContext<'a, B: ratatui::backend::Backend> {
    /// All live panes (phase 0 input + phase 3a layout source).
    runners: Vec<PaneRunner>,
    /// Crossterm keybind router (phase 0 input).
    bindings: Router,
    /// Focused-pane index (phase 0/2 focus tracking).
    focus: usize,
    /// Set to `false` by an action handler to quit the loop.
    running: bool,
    /// MPSC receiver of `PaneRunner::Drop` close notifications;
    /// drained at the start of phase 1.
    close_rx: Receiver<cmdash_pty::PaneLayerId>,
    /// dashcompositor layer book-keeping (phase 1 revoke +
    /// phase 2/3b update).
    graphics: GraphicsState,
    /// ratatui terminal borrowed from a [`TerminalGuard`]; the
    /// guard's `Drop` reverts alt-screen + mouse-capture on
    /// exit, so the borrow lifetime is tied to the guard.
    terminal: &'a mut Terminal<B>,
    /// Per-tick pacing knob (phase 4 sleep duration).
    tick: Duration,
    /// Owned copy of the KDL layout tree, consumed by
    /// [`ComputedLayout::compute`] on every host-driven resize.
    /// Held by value so [`Self::relayout`] does not need to
    /// borrow from `cmdash::run`'s stack after construction.
    /// AGENTS.md §"PaneId stability" — moving the tree by value
    /// does not shift pre-order leaf numbering, so the layout
    /// engine produces the same `cmdash_layout::PaneId`
    /// values before and after a relayout.
    layout_root: LayoutNode,
    /// Coalesced (cols, rows) of the most recent crossterm
    /// `Event::Resize`. Empty during normal ticks; consumed
    /// (via `take()`) at the start of phase 0.5 so successive
    /// resize signals naturally coalesce — only the LATEST
    /// (cols, rows) reaches [`Self::relayout`] this tick.
    pending_resize: Option<(u16, u16)>,
}

impl<'a, B: ratatui::backend::Backend> TickContext<'a, B> {
    /// Construct a [`TickContext`] from the ten per-frame
    /// building blocks (runners + bindings + focus-and-running +
    /// close_rx + graphics + borrowed terminal + tick +
    /// layout_root + pending_resize). Enforces `focus <
    /// runners.len()` so the `runners.get_mut(*focus)`
    /// write-input path inside [`Self::run`] cannot index out
    /// of bounds; the `apply_action::PaneClose` arm restores
    /// this invariant after a tail-remove by clamping focus to
    /// `len() - 1`.
    // The 10-arg ctor is the most central tenant of the AGENTS.md
    // "minimal API surface" rule -- it mirrors the eight struct
    // fields one-to-one, so introducing a shadow `TickContextInit`
    // sub-struct would just create a second type that has to be
    // kept in lock-step with these fields forever. The ctor is
    // currently called exactly once, from `cmdash::run`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runners: Vec<PaneRunner>,
        bindings: Router,
        focus: usize,
        running: bool,
        close_rx: Receiver<cmdash_pty::PaneLayerId>,
        graphics: GraphicsState,
        terminal: &'a mut Terminal<B>,
        tick: Duration,
        layout_root: LayoutNode,
        pending_resize: Option<(u16, u16)>,
    ) -> Self {
        assert!(
            focus < runners.len(),
            "TickContext::new: focus ({focus}) is out of bounds for {} runners",
            runners.len(),
        );
        Self {
            runners,
            bindings,
            focus,
            running,
            close_rx,
            graphics,
            terminal,
            tick,
            layout_root,
            pending_resize,
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
    /// §"PaneId stability"). The defensive `assert_eq!` in
    /// the per-pair loop surfaces a future regression that
    /// breaks the index alignment (e.g. someone introduces a
    /// v2 hot-swap that mutates runner order without
    /// compensating in layout).
    ///
    /// **Failure tolerance.** A single pane's `runner.resize`
    /// error is logged via `tracing::warn!` and the loop
    /// continues for siblings — a misbehaved PTY child must
    /// not bring the multiplexer down. An infrequent
    /// LayoutError or a runner-count mismatch also logs
    /// without escalating — the next tick's resize signal will
    /// have a fresh chance to succeed.
    pub fn relayout(&mut self, w: u16, h: u16) {
        // Zero-area safeguard before any side effect: a live
        // SIGWINCH that round-trips through `(0, 0)` (a window
        // snap or hide-and-restore on GNOME / KDE / macOS
        // minimize-restore) would otherwise panic through
        // `GraphicsState::set_cells`'s `assert!(cells.0 > 0 &&
        // cells.1 > 0)`. `cmdash::run`'s startup `host_size`
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
        let total = LayoutRect { x: 0, y: 0, w, h };
        let layout = match ComputedLayout::compute(&self.layout_root, total) {
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
        self.graphics.set_cells((w, h));
    }

    /// Drive the AGENTS.md rendering pipeline until `running`
    /// flips `false` or every pane exits. The loop body is the
    /// same logic that lived in the prior free `tick_loop`
    /// function; bundling it on this struct lets `cmdash::run`
    /// invoke it as a one-shot `ctx.run()`.
    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        loop {
            // Phase 0: drain input events. Non-blocking; bound by
            // `event::poll(Duration::from_millis(0))`. Each Press
            // event is either routed through
            // `Router::dispatch_crossterm` or forwarded as bytes
            // to the focused pane. `Event::Resize(w, h)` arms
            // the `pending_resize` slot for phase 0.5 below.
            input_phase(
                &mut self.runners,
                &self.bindings,
                &mut self.focus,
                &mut self.running,
                &mut self.pending_resize,
            )?;
            if !self.running {
                return Ok(());
            }

            // Phase 0.5: host SIGWINCH coalescer. Drains the
            // resize slot queued during phase 0 and runs
            // `relayout(...)` BEFORE phase 1's drain, so a
            // resize signal that arrived mid-tick produces a
            // fresh per-pane rect in `self.runners[i].computed()` by
            // the time phase 3a reads it.
            if let Some((w, h)) = self.pending_resize.take() {
                self.relayout(w, h);
            }

            // Phase 1: drain the close-channel (Drop messages)
            // FIRST so their revisions are visible before phase 2/3
            // in the same tick. Then poll exits + tick + snapshot.
            while let Ok(id) = self.close_rx.try_recv() {
                self.graphics.close_pane(id);
            }
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
                snapshots.push(Some(runner.tick()?));
            }

            // Phase 2: route events -> graphics. Kitty graphics
            // emitted by a nested PTY are pushed onto the per-pane
            // image map; everything else is logged. Failures
            // log + continue (a busted image must not bring the
            // multiplexer down).
            for (runner, snap) in self.runners.iter().zip(snapshots.iter()) {
                if let Some(snap) = snap {
                    for ev in &snap.pending_events {
                        if let PaneEvent::KittyGraphic { cmd } = ev {
                            self.graphics.apply_kitty_event(runner.layer_id(), cmd);
                        }
                    }
                }
            }

            // Phase 3a: render the cell body through ratatui.
            // Capture the focused index BEFORE drawing so the
            // debug! can fire from inside the draw closure
            // body without forcing a borrow conflict with the
            // mutable `&mut self.terminal` reborrow taken by
            // `terminal.draw`. The accessor `pub const fn focus`
            // is therefore called on the hot path and no longer
            // needs `#[allow(dead_code)]`.
            let focus_idx_dbg = self.focus();
            self.terminal.draw(|frame| {
                debug!(focus_idx = focus_idx_dbg, "rendering frame");
                let buf = frame.buffer_mut();
                for (runner, snap) in self.runners.iter().zip(snapshots.iter()) {
                    let area = ratatui::layout::Rect::new(
                        runner.computed().rect.x,
                        runner.computed().rect.y,
                        runner.computed().rect.w,
                        runner.computed().rect.h,
                    );
                    if let Some(snap) = snap {
                        blit_grid(&snap.grid, buf, area);
                        blit_cursor(&snap.grid, buf, area);
                    }
                }
            })?;

            // Phase 3b: emit dashcompositor kitty graphics through
            // a fresh stdout handle. The terminal's own backend
            // already finished writing row-bearing text; kitty
            // escapes overlay on kitty-capable hosts and degrade
            // gracefully elsewhere. AGENTS.md §"Rendering
            // pipeline" step 6 prescribes this exact path.
            let mut stdout = std::io::stdout();
            if let Err(e) = self.graphics.render_and_write(&mut stdout) {
                warn!(error = %e, "graphics emit failed");
            }

            if all_exited {
                return Ok(());
            }
            std::thread::sleep(self.tick);
        }
    }
}

/// Phase 0: drain any pending crossterm events and dispatch them
/// — either to a keybind action or straight into the focused
/// pane's PTY, OR schedule a host SIGWINCH relayout. Non-blocking;
/// bounded by the caller's tick. Returns `Err` on crossterm I/O
/// failures so the binary can stop cleanly.
fn input_phase(
    runners: &mut Vec<PaneRunner>,
    bindings: &Router,
    focus: &mut usize,
    running: &mut bool,
    pending_resize: &mut Option<(u16, u16)>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    while event::poll(Duration::from_millis(0))? {
        let evt = event::read()?;
        handle_event(&evt, bindings, focus, runners, running, pending_resize);
    }
    Ok(())
}

fn handle_event(
    evt: &Event,
    bindings: &Router,
    focus: &mut usize,
    runners: &mut Vec<PaneRunner>,
    running: &mut bool,
    pending_resize: &mut Option<(u16, u16)>,
) {
    if let Some(action) = bindings.dispatch_crossterm(evt) {
        apply_action(action, focus, runners, running);
        return;
    }
    // Host SIGWINCH (crossterm `Event::Resize`) — coalesce-on-
    // overwrite so a rapid resize burst collapses to the LATEST
    // (cols, rows) by the time the next tick reaches phase 0.5.
    // This arm deliberately does NOT mutate `runners`; relayout
    // happens at the top of the tick after this input drain so
    // the cross-key close-channel invariant
    // (`Drop::drop enqueues onto a live receiver`) is preserved
    // for any pane drops that share the same tick.
    if let Event::Resize(w, h) = evt {
        *pending_resize = Some((*w, *h));
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
    if !matches!(kind, KeyEventKind::Press) {
        return;
    }
    // The `modifiers` field is intentionally ignored for v1
    // forwarding: a shift modifier in the focus pane is just
    // whatever the PTY (<input>) further decodes from the bytes.
    let _ = modifiers;
    let Some(bytes) = event_to_bytes(*code) else {
        return;
    };
    if let Some(runner) = runners.get_mut(*focus) {
        if let Err(e) = runner.write_input(&bytes) {
            debug!(error = ?e, layer_id = ?runner.layer_id(), "write_input failed");
        }
    }
}

fn apply_action(
    action: KeyAction,
    focus: &mut usize,
    runners: &mut Vec<PaneRunner>,
    running: &mut bool,
) {
    match action {
        KeyAction::AppClose => {
            *running = false;
        }
        KeyAction::PaneFocusNext => {
            if !runners.is_empty() {
                *focus = (*focus + 1) % runners.len();
            }
        }
        KeyAction::PaneFocusPrev => {
            if !runners.is_empty() {
                *focus = (*focus + runners.len() - 1) % runners.len();
            }
        }
        // v1 implements PaneClose here: remove the focused
        // runner from the Vec, which fires `PaneRunner::Drop`
        // and routes the pane's `PaneLayerId` into the close
        // channel. TickContext::run drains that channel in
        // phase 1 and calls `GraphicsState::close_pane`,
        // satisfying AGENTS.md §"Hard rule: one layer per
        // instance". Closing the last pane quits the binary
        // (`*running = false`); the focus index is clamped to
        // the new last entry when the tail was removed so the
        // next PTY-write-input path can't index out of bounds.
        KeyAction::PaneClose => {
            if runners.is_empty() {
                return;
            }
            // Sanitize focus before indexing (defensive — should
            // already be in-bounds from new-tab spawn and the
            // focus clamp below; cheap).
            if *focus >= runners.len() {
                *focus = runners.len() - 1;
            }
            let closing_layer_id = runners[*focus].layer_id();
            debug!(
                layer_id = ?closing_layer_id,
                focus = *focus,
                "pane-close: drop focused runner (Drop -> close_tx -> graphics.close_pane)"
            );
            runners.remove(*focus);
            if runners.is_empty() {
                *running = false;
                return;
            }
            if *focus >= runners.len() {
                *focus = runners.len() - 1;
            }
        }
        // v1: AppNewPane, PanePreset, PaneFocus{Up,Down,Left,
        // Right} are no-ops. v2 will hook them into the layout
        // engine.
        _ => {
            debug!(?action, "key action not yet implemented in v1");
        }
    }
}

/// Encode an unmatched key press as PTY-friendly bytes for the
/// focused pane. Returns `None` for variants that should NOT
/// leak to the PTY (Insert, F-keys above 4, media keys,
/// modifier-only events).
fn event_to_bytes(code: KeyCode) -> Option<Vec<u8>> {
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
mod input_tests {
    //! Regression tests for the binary's `cmdash::run` tick-loop
    //! surface. Drives `handle_event` with synthetic crossterm
    //! events so the keybind -> action -> `Vec::remove` -> `Drop`
    //! -> close-channel path is exercised without spinning up a
    //! real terminal. The full live-binary tick path falls
    //! outside this module's scope because it requires a real
    //! `TerminalGuard`; the integration tests in
    //! `crates/cmdash/tests/wiring_smoke.rs` exercise the
    //! resize/relayout wiring end-to-end via real PTY
    //! children.
    //!
    //! The keybind -> action -> `Vec::remove` -> `Drop` path
    //! mirrors the live binary: a Ctrl-W keypress is dispatched
    //! by `Router::dispatch_crossterm` to a
    //! `KeyAction::PaneClose`, `apply_action` removes the
    //! focused `PaneRunner` from the Vec, the dropped runner's
    //! `Drop::drop` enqueues its `PaneLayerId` on the
    //! `PaneCloseTx`, and the next tick's phase 1 drains the
    //! channel and calls `GraphicsState::close_pane`.
    use super::*;
    // `PaneCloseTx`, `PaneRunner`, `GraphicsState`, and `Metrics`
    // are all in scope via `super::*` -> the parent
    // (`fn main`'s module) re-exports them through
    // `use cmdash::graphics::{GraphicsState, Metrics};` and
    // `use cmdash::pane::{PaneCloseTx, PaneRunner};`. main.rs is
    // the binary's entrypoint; the library's `pane` module is
    // reached as `cmdash::pane`, never as `crate::pane` (which
    // would resolve to the binary's flat namespace).
    use cmdash_config::{
        KeyAction, KeyToken, Keybind, LayoutNode, Modifiers as CfgModifiers, Pane as CfgPane,
        PaneKind,
    };
    use cmdash_keybinds::Router;
    use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
    use cmdash_pty::{PaneLayerId, ShellSpec};
    use std::sync::mpsc;

    /// Spawn a single `PaneRunner` wired to a close-channel and
    /// using `/bin/true` (fast-exit child) so `Drop::drop`
    /// rejoins the reader thread promptly in tests.
    fn make_runner(label: &str, close_tx: PaneCloseTx) -> PaneRunner {
        let cfg = cmdash_config::parse(&format!("layout {{ pane kind=shell label=\"{label}\" }}"))
            .expect("parse KDL");
        let root = cfg.layout.expect("layout block");
        let layout = ComputedLayout::compute(
            &root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute layout");
        let computed = layout.panes[0].clone();
        let layer_id = cmdash::derive_layer_id(&computed.id);
        PaneRunner::spawn_with_graphics(
            computed,
            layer_id,
            ShellSpec::Command {
                argv: vec!["true".to_string()],
            },
            Some(close_tx),
        )
        .expect("spawn_with_graphics")
    }

    /// Spawn a single `PaneRunner` like `make_runner` does,
    /// but with an EXPLICIT `PaneLayerId`. Use this in tests
    /// that need distinct ids without depending on the layout
    /// tree's pre-order numbering.
    fn make_runner_with_id(
        label: &str,
        layer_id: PaneLayerId,
        close_tx: PaneCloseTx,
    ) -> PaneRunner {
        let cfg = cmdash_config::parse(&format!("layout {{ pane kind=shell label=\"{label}\" }}"))
            .expect("parse KDL");
        let root = cfg.layout.expect("layout block");
        let layout = ComputedLayout::compute(
            &root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute layout");
        let computed = layout.panes[0].clone();
        PaneRunner::spawn_with_graphics(
            computed,
            layer_id,
            ShellSpec::Command {
                argv: vec!["true".to_string()],
            },
            Some(close_tx),
        )
        .expect("spawn_with_graphics")
    }

    /// Build a synthetic crossterm key event for the given
    /// chord. Used to drive `handle_event` directly without
    /// calling `event::poll`.
    fn key_event(code: crossterm::event::KeyCode, mods: crossterm::event::KeyModifiers) -> Event {
        Event::Key(crossterm::event::KeyEvent {
            code,
            modifiers: mods,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    }

    /// Build a single-leaf `LayoutNode` fixture for ctor-arg
    /// tests that don't exercise the layout::compute path.
    /// Keeping it tiny avoids hitting MAX_TREE_DEPTH on
    /// out-of-band nesting during negative-test setup.
    fn dummy_layout_root() -> LayoutNode {
        LayoutNode::Pane(CfgPane {
            kind: PaneKind::Shell,
            label: None,
        })
    }

    /// Ctrl-W on a 2-pane Vec: Vec shrinks by one, the
    /// survivor is unmoved, the close-channel receives the
    /// dropped pane's `PaneLayerId`, and `graphics.close_pane`
    /// drains the matching image registration. Exercises the
    /// live binary's full Ctrl-W -> close-channel ->
    /// dashcompositor revoke pipeline.
    #[test]
    fn ctrl_w_pane_close_pops_focused_runner_and_routes_close_message() {
        let (close_tx, close_rx): (PaneCloseTx, _) = mpsc::channel();
        let r0 = make_runner("a", close_tx.clone());
        let r1 = make_runner("b", close_tx.clone());
        let dropped_layer_id = r0.layer_id();
        let survivor_layer_id = r1.layer_id();
        let mut runners: Vec<PaneRunner> = vec![r0, r1];

        // Pre-register one image for the focused pane so we can
        // prove `close_pane` revokes it on drain, matching the
        // production LayerStack revoking flow.
        let mut graphics = GraphicsState::new(cmdash::graphics::Metrics::default(), (80, 24));
        graphics.push_image(dropped_layer_id, 1, image::RgbaImage::new(1, 1));
        assert!(graphics.has_image(dropped_layer_id, 1));

        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers {
                ctrl: true,
                ..CfgModifiers::default()
            },
            key: KeyToken::Char('w'),
            action: KeyAction::PaneClose,
        }]);

        let mut focus: usize = 0;
        let mut running = true;

        handle_event(
            &key_event(
                crossterm::event::KeyCode::Char('w'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
            &bindings,
            &mut focus,
            &mut runners,
            &mut running,
            &mut None,
        );

        // 1) Vec shrank by one, the survivor is the original
        //    `r1` (its `PaneLayerId` matches), and one open
        //    pane does not quit the binary.
        assert_eq!(runners.len(), 1);
        assert!(running, "closing one pane must not stop the binary");
        assert_eq!(runners[0].layer_id(), survivor_layer_id);

        // 2) Focus stays valid (still 0 since 0 < 1).
        assert_eq!(focus, 0);

        // 3) `Drop::drop` enqueued the closing pane's id onto
        //    the close-channel the binary's main loop drains.
        let received = close_rx
            .try_recv()
            .expect("PaneRunner::Drop must enqueue the closing pane's layer id");
        assert_eq!(received, dropped_layer_id);

        // 4) Simulating phase 1 -- drain + close_pane
        //    -- revokes the dashcompositor image registration.
        graphics.close_pane(received);
        assert!(!graphics.has_image(dropped_layer_id, 1));

        drop(close_tx);
        drop(runners);
        drop(graphics);
    }

    /// Closing the last surviving pane flips `running` to
    /// false and quits the binary. Verifies the empty-Vec edge
    /// case is handled without panicking.
    #[test]
    fn pane_close_last_pane_quits_binary() {
        let (close_tx, close_rx): (PaneCloseTx, _) = mpsc::channel();
        let r0 = make_runner("only", close_tx.clone());
        let dropped_layer_id = r0.layer_id();
        let mut runners: Vec<PaneRunner> = vec![r0];

        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('w'),
            action: KeyAction::PaneClose,
        }]);

        let mut focus: usize = 0;
        let mut running = true;

        handle_event(
            &key_event(
                crossterm::event::KeyCode::Char('w'),
                crossterm::event::KeyModifiers::NONE,
            ),
            &bindings,
            &mut focus,
            &mut runners,
            &mut running,
            &mut None,
        );

        assert!(runners.is_empty());
        assert!(!running, "closing the final pane must quit the binary");
        let received = close_rx
            .try_recv()
            .expect("closing the only pane must enqueue the close message");
        assert_eq!(received, dropped_layer_id);

        drop(close_tx);
        drop(runners);
    }

    /// Removing the focused pane when it is the TAIL of the
    /// Vec must clamp `focus` to the new last index so the
    /// `runners.get_mut(*focus)` PTY-write path cannot index
    /// out of bounds in subsequent ticks.
    #[test]
    fn pane_close_clamps_focus_when_tail_removed() {
        let (close_tx, _close_rx): (PaneCloseTx, _) = mpsc::channel();
        // Three distinct `PaneLayerId`s by construction -- we
        // pass them explicitly. v1 stack layouts collapse to a
        // single tabbed pane so a shared layout can't give us
        // three distinct ids; independent single-pane layouts
        // all derive `PaneLayerId(0)` and would collide.
        let r0 = make_runner_with_id("a", PaneLayerId(1), close_tx.clone());
        let r1 = make_runner_with_id("b", PaneLayerId(2), close_tx.clone());
        let r2 = make_runner_with_id("c", PaneLayerId(3), close_tx.clone());
        let survivor_a = r0.layer_id();
        let survivor_b = r1.layer_id();
        let dropped_layer_id = r2.layer_id();
        let mut runners = vec![r0, r1, r2];
        assert_ne!(runners[0].layer_id(), runners[1].layer_id());
        assert_ne!(runners[1].layer_id(), runners[2].layer_id());

        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('w'),
            action: KeyAction::PaneClose,
        }]);

        // Focus the LAST pane (index 2). Removing it should
        // clamp focus to len-1 (1) so the next tick gets a
        // valid index.
        let mut focus: usize = 2;
        let mut running = true;

        handle_event(
            &key_event(
                crossterm::event::KeyCode::Char('w'),
                crossterm::event::KeyModifiers::NONE,
            ),
            &bindings,
            &mut focus,
            &mut runners,
            &mut running,
            &mut None,
        );

        assert_eq!(runners.len(), 2);
        assert!(running);
        assert_eq!(focus, 1, "removing the tail must clamp focus");
        // Survivors must stay at positions 0 and 1 by `PaneLayerId`
        // (not just by Vec index); the dropped runner was at idx 2.
        assert_eq!(runners[0].layer_id(), survivor_a);
        assert_eq!(runners[1].layer_id(), survivor_b);
        assert_ne!(runners[1].layer_id(), dropped_layer_id);

        drop(close_tx);
        drop(runners);
    }

    /// Building a [`TickContext`] with `focus >= runners.len()`
    /// must panic with a `focus` keyword in the message, so a
    /// caller passing a stale `focus` after a panic-driven
    /// re-construction cannot silently index past the runner
    /// Vec. Locks the AGENTS.md "every invariant needs a
    /// regression test" rule for the focus invariant.
    /// Uses a `ratatui::backend::TestBackend` to construct a
    /// real `Terminal` without writing to stdout.
    #[test]
    #[should_panic(expected = "focus")]
    fn tick_context_new_panics_when_focus_out_of_bounds() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let (_close_tx, close_rx): (PaneCloseTx, _) = mpsc::channel::<cmdash_pty::PaneLayerId>();
        let bindings = Router::new(vec![]);
        let graphics =
            cmdash::graphics::GraphicsState::new(cmdash::graphics::Metrics::default(), (80, 24));
        // Empty runners + focus=0 -> 0 < 0 is false -> assert! fires.
        let _ctx = TickContext::new(
            Vec::<PaneRunner>::new(),
            bindings,
            0,
            true,
            close_rx,
            graphics,
            &mut terminal,
            std::time::Duration::from_millis(33),
            dummy_layout_root(),
            None,
        );
        drop(_close_tx);
    }

    /// Companion to the empty-Vec test above: locks the
    /// strict-less-than semantics across the non-zero boundary.
    /// focus=2 + 2 panes -> 2 < 2 is false -> assert! fires.
    /// Catches a future regression that swaps `<` for `<=`
    /// (would accept focus == len and silently index past the
    /// Vec on the next `runners.get_mut(*focus)` call). Uses
    /// `make_runner_with_id` so each pane has a distinct
    /// `PaneLayerId` independent of layout-pre-order numbering.
    #[test]
    #[should_panic(expected = "focus")]
    fn tick_context_new_panics_when_focus_equals_non_zero_len() {
        let (close_tx, _close_rx) = mpsc::channel::<cmdash_pty::PaneLayerId>();
        let r0 = make_runner_with_id("a", PaneLayerId(1), close_tx.clone());
        let r1 = make_runner_with_id("b", PaneLayerId(2), close_tx.clone());
        let bindings = Router::new(vec![]);
        let graphics =
            cmdash::graphics::GraphicsState::new(cmdash::graphics::Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let _ctx = TickContext::new(
            vec![r0, r1],
            bindings,
            2, // focus == runners.len()
            true,
            _close_rx,
            graphics,
            &mut terminal,
            std::time::Duration::from_millis(33),
            dummy_layout_root(),
            None,
        );
    }

    /// Phase 2 v2 wiring regression: a crossterm
    /// `Event::Resize(w, h)` synthesised at the `handle_event`
    /// boundary must land in `pending_resize` so the top of
    /// the next tick drives `relayout(w, h)`. Splits the
    /// assertion into the two smallest claims the bug
    /// surface allows: (1) the option transitions from
    /// `None` -> `Some((w, h))`, (2) subsequent resize
    /// signals coalesce (overwrite, NOT push) so rapid
    /// SIGWINCH bursts collapse to the LATEST dims.
    #[test]
    fn handle_event_resize_event_arms_pending_resize() {
        let (close_tx, _close_rx): (PaneCloseTx, _) = mpsc::channel();
        let mut runners: Vec<PaneRunner> = vec![];
        let bindings = Router::new(vec![]);
        let mut focus: usize = 0;
        let mut running = true;
        let mut pending_resize: Option<(u16, u16)> = None;

        handle_event(
            &Event::Resize(132, 50),
            &bindings,
            &mut focus,
            &mut runners,
            &mut running,
            &mut pending_resize,
        );
        assert_eq!(
            pending_resize,
            Some((132, 50)),
            "Event::Resize must arm pending_resize for phase 0.5 relayout"
        );

        // Coalesce-on-overwrite: a second resize arrives
        // BEFORE phase 0.5 has taken the first queued tuple,
        // so the value should simply be replaced, not stacked.
        handle_event(
            &Event::Resize(200, 60),
            &bindings,
            &mut focus,
            &mut runners,
            &mut running,
            &mut pending_resize,
        );
        assert_eq!(
            pending_resize,
            Some((200, 60)),
            "second Event::Resize must coalesce onto (NOT push past) the first"
        );

        drop(close_tx);
    }

    /// Phase 2 v2 wiring regression end-to-end at the tick
    /// surface: build a real `TickContext` over a Split KDL
    /// config so two `PaneRunner`s spawn side-by-side, drive
    /// `relayout(132, 50)`, and assert both runners' cached
    /// rects match the layout engine's output AND the
    /// `cmdash_layout::PaneId` pairing invariant holds
    /// (runners[i].id == layout.panes[i].id) AND
    /// `GraphicsState::cells` propagated to the new dims.
    ///
    /// The counterpart integration test in
    /// `crates/cmdash/tests/wiring_smoke.rs::relayout_drives_per_pane_resize_via_real_pty`
    /// exercises the same wiring end-to-end through real
    /// PTY children; this lib unit-test pins the deterministic
    /// for_id and for_cells invariants without depending on a
    /// real PTY round-trip.
    #[test]
    fn relayout_emits_resize_per_pane_when_host_signals_resize() {
        let source = r#"layout {
            split axis=horizontal ratio=0.6 {
                pane kind=shell label="split-a"
                pane kind=shell label="split-b"
            }
        }"#;
        let cfg = cmdash_config::parse(source).expect("parse split config");
        let layout_root = cfg
            .layout
            .clone()
            .expect("layout block must contain a Split");

        // Initial-frame spawn: both panes derive from a SHARED
        // `ComputedLayout::compute(&layout_root, ...)` invocation
        // so `pane_a.id.path_len == pane_b.id.path_len == 2`
        // (matching the Split config's leaf path-depth) -- the
        // SAME pairing requirement enforced inside
        // `TickContext::relayout`'s per-pair
        // `assert_eq!(runner.computed().id, pane.id)`. Earlier
        // rounds derived each pane via independent
        // `make_runner("split-a")` parses, which yield
        // `path_len: 1` PaneIds (single-pane KDL) and break the
        // assertion even though `pre_order` matches. This
        // bug was invisible to `cargo test -p cmdash --lib`
        // because `input_tests` lives in the binary crate;
        // only `cargo test --workspace` exercises it.
        let (close_tx, close_rx): (PaneCloseTx, _) = mpsc::channel();
        let initial_layout = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute initial 80x24 split layout");
        assert_eq!(
            initial_layout.panes.len(),
            2,
            "expected 2 leaf panes from Split config"
        );
        let pane_a = initial_layout.panes[0].clone();
        let pane_b = initial_layout.panes[1].clone();
        let id_a = cmdash::derive_layer_id(&pane_a.id);
        let id_b = cmdash::derive_layer_id(&pane_b.id);
        // `/bin/true` is the fast-exit child used by the rest of
        // `input_tests`; the test exercises the layout -> runner
        // resize pairing path, NOT the live PTY resize path (that
        // is `wiring_smoke.rs::relayout_drives_per_pane_resize_via_real_pty`).
        let shell = ShellSpec::Command {
            argv: vec!["true".to_string()],
        };
        let r0 =
            PaneRunner::spawn_with_graphics(pane_a, id_a, shell.clone(), Some(close_tx.clone()))
                .expect("spawn runner A");
        let r1 = PaneRunner::spawn_with_graphics(pane_b, id_b, shell, Some(close_tx))
            .expect("spawn runner B");
        let runners = vec![r0, r1];
        let bindings = Router::new(vec![]);
        let graphics =
            cmdash::graphics::GraphicsState::new(cmdash::graphics::Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(132, 50);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");

        let mut ctx = TickContext::new(
            runners,
            bindings,
            0,
            true,
            close_rx,
            graphics,
            &mut terminal,
            std::time::Duration::from_millis(33),
            layout_root.clone(),
            Some((132, 50)),
        );

        // Pairing pin BEFORE relayout: each runner's id must
        // already match the layout engine's pre-order leaf
        // numbering, otherwise `relayout`'s per-pair assert_eq!
        // would fire. This separately verifies the spawn loop's
        // index-alignment with the layout tree.
        let pre_layout = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute pre-layout");
        assert_eq!(pre_layout.panes.len(), ctx.runners.len());
        assert_eq!(ctx.runners[0].computed().id, pre_layout.panes[0].id);
        assert_eq!(ctx.runners[1].computed().id, pre_layout.panes[1].id);

        // Drive relayout: 80x24 -> 132x50. The pre-queued
        // `pending_resize = Some((132, 50))` lets us call
        // relayout directly bypassing phase 0; equivalent to
        // having phase 0.5 drain the slot at the top of the
        // loop.
        ctx.relayout(132, 50);

        // Post-relayout: every pane rect must match the
        // layout engine's 132x50 Split output. Ratio 0.6 over
        // width 132 -> child A at (0, 0, 79, 50) and child B
        // at (79, 0, 53, 50). Same `cmdash_layout::split_rect`
        // math as the v2-contract pin in `wiring_smoke.rs`.
        assert_eq!(
            ctx.runners[0].computed().rect,
            LayoutRect {
                x: 0,
                y: 0,
                w: 79,
                h: 50
            },
            "child A post-relayout rect must match 132x50 Horizontal-60 split"
        );
        assert_eq!(
            ctx.runners[1].computed().rect,
            LayoutRect {
                x: 79,
                y: 0,
                w: 53,
                h: 50
            },
            "child B post-relayout rect must match 132x50 Horizontal-60 split"
        );

        // Pairing pin AFTER relayout: each runner's id must
        // still match the layout engine's pre-order (no
        // identity shift across resize; AGENTS.md §"PaneId
        // stability" + §"Hard rule: one layer per instance").
        let post_layout = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 132,
                h: 50,
            },
        )
        .expect("compute post-layout");
        assert_eq!(ctx.runners[0].computed().id, post_layout.panes[0].id);
        assert_eq!(ctx.runners[1].computed().id, post_layout.panes[1].id);

        // GraphicsState cells propagated to the new dims --
        // dashcompositor framebuffer pixel composition must
        // catch up to the layout engine's cell-grid surface.
        assert_eq!(
            ctx.graphics.cells(),
            (132, 50),
            "GraphicsState cells must follow the relayout dimension"
        );
    }
}
