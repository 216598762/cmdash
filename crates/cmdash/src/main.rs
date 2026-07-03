//! cmdash binary: drives the layout → PTY → ratatui render loop
//! with crossterm input dispatch via cmdash-keybinds.
//!
//! AGENTS.md §"Rendering pipeline" requires the cell body to be
//! drawn into a ratatui `Frame` (this module's `terminal.draw`).
//! v1 single-tab, sync IO via per-pane reader threads, degraded
//! text-mode (no dashcompositor wiring). Key bindings from the
//! config are routed through cmdash-keybinds; unmatched presses
//! are forwarded as raw bytes to the focused pane's PTY via
//! `PaneRunner::write_input`.

use std::time::Duration;

use cmdash::pane::PaneRunner;
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
    info!("cmdash starting (degraded text-mode; ratatui-only)");
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

    let mut runners: Vec<PaneRunner> = Vec::with_capacity(layout.panes.len());
    for pane in &layout.panes {
        let layer_id = cmdash::derive_layer_id(&pane.id);
        match PaneRunner::spawn(pane.clone(), layer_id, ShellSpec::LoginShell) {
            Ok(r) => runners.push(r),
            Err(e) => warn!(error = %e, ?layer_id, "failed to spawn pane"),
        }
    }
    if runners.is_empty() {
        return Err("no panes were spawned; aborting".into());
    }

    let bindings = Router::new(cfg.keybinds);
    let mut focus: usize = 0;
    let mut running = true;

    let mut guard = TerminalGuard::enter()?;
    let tick = Duration::from_millis(33);
    let result = tick_loop(
        &mut runners,
        &bindings,
        &mut focus,
        &mut running,
        guard.as_mut(),
        tick,
    );
    drop(guard);
    result
}

/// Concrete backend alias used by [`TerminalGuard`] and the
/// production [`Terminal`]. Tests can swap to a `TestBackend`
/// locally without going through the guard.
type CmdashBackend = ratatui::backend::CrosstermBackend<std::io::Stdout>;

/// Owns a `Terminal<CmdashBackend>` whose setup (raw mode +
/// alternate screen + mouse capture) is reverted by [`Drop`] on
/// error or normal return. Without this guard, an early `?` in
/// the setup between `enable_raw_mode()` and `tick_loop()`
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

fn tick_loop<B: ratatui::backend::Backend>(
    runners: &mut [PaneRunner],
    bindings: &Router,
    focus: &mut usize,
    running: &mut bool,
    terminal: &mut Terminal<B>,
    tick: Duration,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        // Phase 0: drain input events. Non-blocking; bound by
        // `event::poll(Duration::from_millis(0))`. Each Press event
        // is either routed through `Router::dispatch_crossterm` or
        // forwarded as bytes to the focused pane.
        input_phase(runners, bindings, focus, running)?;
        if !*running {
            return Ok(());
        }

        // Phase 1: poll exits + drain bytes + snapshot.
        let mut snapshots: Vec<Option<cmdash_pty::PaneTerminalState>> =
            Vec::with_capacity(runners.len());
        let mut all_exited = true;
        for runner in runners.iter_mut() {
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

        // Phase 2: react to events (kitty placeholder).
        for snap in snapshots.iter().flatten() {
            for ev in &snap.pending_events {
                if let PaneEvent::KittyGraphic { cmd } = ev {
                    debug!(?cmd, "kitty event placeholder (no graphics-path yet)");
                }
            }
        }

        // Phase 3: render.
        terminal.draw(|frame| {
            let buf = frame.buffer_mut();
            for (runner, snap) in runners.iter().zip(snapshots.iter()) {
                let area = ratatui::layout::Rect::new(
                    runner.computed.rect.x,
                    runner.computed.rect.y,
                    runner.computed.rect.w,
                    runner.computed.rect.h,
                );
                if let Some(snap) = snap {
                    blit_grid(&snap.grid, buf, area);
                    blit_cursor(&snap.grid, buf, area);
                }
            }
        })?;

        if all_exited {
            return Ok(());
        }
        std::thread::sleep(tick);
    }
}

/// Phase 0: drain any pending crossterm events and dispatch them
/// — either to a keybind action or straight into the focused
/// pane's PTY. Non-blocking; bounded by the caller's tick.
/// Returns `Err` on crossterm I/O failures so the binary can stop
/// cleanly.
fn input_phase(
    runners: &mut [PaneRunner],
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
    runners: &mut [PaneRunner],
    running: &mut bool,
) {
    if let Some(action) = bindings.dispatch_crossterm(evt) {
        apply_action(action, focus, runners.len(), running);
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

fn apply_action(action: KeyAction, focus: &mut usize, runners_len: usize, running: &mut bool) {
    match action {
        KeyAction::AppClose => {
            *running = false;
        }
        KeyAction::PaneFocusNext => {
            if runners_len > 0 {
                *focus = (*focus + 1) % runners_len;
            }
        }
        KeyAction::PaneFocusPrev => {
            if runners_len > 0 {
                *focus = (*focus + runners_len - 1) % runners_len;
            }
        }
        // v1: PaneClose, AppNewPane, PanePreset, PaneFocus{Up,
        // Down, Left, Right} are no-ops. v2 will hook them into
        // the layout engine.
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
