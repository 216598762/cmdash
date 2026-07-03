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
use cmdash_config::{parse as parse_config, KeyAction};
use cmdash_keybinds::Router;
use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
use cmdash_pty::{PaneEvent, ShellSpec};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::Terminal;
use tracing::{debug, info, warn};

const DEFAULT_AREA_COLS: u16 = 80;
const DEFAULT_AREA_ROWS: u16 = 24;

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
    let layout_root = cfg
        .layout
        .as_ref()
        .ok_or("config.kdl missing `layout { ... }` block")?;
    let total = LayoutRect {
        x: 0,
        y: 0,
        w: DEFAULT_AREA_COLS,
        h: DEFAULT_AREA_ROWS,
    };
    let layout = ComputedLayout::compute(layout_root, total)?;
    info!(panes = layout.panes.len(), "layout resolved");

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
/// Bundles the eight per-frame arguments of the prior free
/// function `tick_loop` into one struct so `cmdash::run` calls
/// `TickContext::run` as a single-shot pipeline call instead of
/// threading individual references through a 7-argument
/// function (which tripped `clippy::too_many_arguments`).
///
/// All fields are **owned** except `terminal`, which is borrowed
/// from a surrounding [`TerminalGuard`] whose `Drop` reverts the
/// alt-screen and mouse-capture on exit. The other seven are
/// owned because `cmdash::run` builds the struct once and
/// runs the loop to completion — there is no caller that needs
/// post-loop access to the runners, graphics, or bindings.
///
/// AGENTS.md §"Rendering pipeline (one frame)" enumerates the
/// six tick phases (input, drain, snapshot, event route,
/// ratatui draw, dashcompositor emit, sleep). The field names
/// mirror those phases: `runners` + `bindings` + `focus` +
/// `running` are phase 0/1/2 inputs; `close_rx` + `graphics` +
/// `tick` are phase 1/2/3b/4 resources; `terminal` is phase 3a.
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
}

impl<'a, B: ratatui::backend::Backend> TickContext<'a, B> {
    /// Construct a [`TickContext`] from the eight per-frame
    /// building blocks (runners + bindings + focus-and-running +
    /// close_rx + graphics + borrowed terminal + tick).
    /// Enforces `focus < runners.len()` so the
    /// `runners.get_mut(*focus)` write-input path inside
    /// [`Self::run`] cannot index out of bounds; the
    /// `apply_action::PaneClose` arm restores this invariant
    /// after a tail-remove by clamping focus to `len() - 1`.
    // The 8-arg ctor is the most central tenant of the AGENTS.md
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
            // to the focused pane.
            input_phase(
                &mut self.runners,
                &self.bindings,
                &mut self.focus,
                &mut self.running,
            )?;
            if !self.running {
                return Ok(());
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
/// pane's PTY. Non-blocking; bounded by the caller's tick.
/// Returns `Err` on crossterm I/O failures so the binary can stop
/// cleanly.
fn input_phase(
    runners: &mut Vec<PaneRunner>,
    bindings: &Router,
    focus: &mut usize,
    running: &mut bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    while event::poll(Duration::from_millis(0))? {
        let evt = event::read()?;
        handle_event(&evt, bindings, focus, runners, running);
    }
    Ok(())
}

fn handle_event(
    evt: &Event,
    bindings: &Router,
    focus: &mut usize,
    runners: &mut Vec<PaneRunner>,
    running: &mut bool,
) {
    if let Some(action) = bindings.dispatch_crossterm(evt) {
        apply_action(action, focus, runners, running);
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
    //! Regression tests for the `KeyAction::PaneClose` wiring on
    //! the focused `PaneRunner`. Drives `handle_event` with a
    //! synthetic crossterm event so the keybind -> action ->
    //! `Vec::remove` -> `Drop` -> close-channel path is exercised
    //! without spinning up a real terminal.
    //!
    //! The full path mirrors the live binary: a Ctrl-W keypress
    //! is dispatched by `Router::dispatch_crossterm` to a
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
    use cmdash_config::{KeyAction, KeyToken, Keybind, Modifiers as CfgModifiers};
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
        );
    }
}
