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

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use cmdash::graphics::{GraphicsState, Metrics};
use cmdash::pane::{PaneCloseTx, PaneRunner};
use cmdash::render::{blit_cursor, blit_grid};
use cmdash_config::{
    parse as parse_config, KeyAction, LayoutNode, Pane as CfgPane, PaneKind, Ratio as CfgRatio,
    SplitAxis as CfgSplitAxis,
};
use cmdash_keybinds::Router;
use cmdash_layout::{
    adjacent_pane, remove_leaf, replace_leaf_with_split, ComputedLayout, Direction, PaneId,
    Rect as LayoutRect,
};
use cmdash_pty::PaneLayerId;
use cmdash_pty::{PaneEvent, ShellSpec};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::Terminal;
use tracing::{debug, info, warn};

/// Parsed CLI arguments. The v1 binary's launch surface is
/// deliberately minimal — exactly one override
/// (`--log-level=<level>`) plus `--help` / `-h` — so a
/// hand-rolled [`parse_cli_args`] function keeps the dep
/// graph free of `clap` / `pico-args`. Future surface-area
/// growth (config-override flags, alternate-config paths,
/// …) should still re-evaluate dep-free hand-rolling before
/// re-adding a parser crate; the `Cargo.toml` constraint at
/// v1.0.0 is "minimize the workspace's external-dep churn
/// between minor versions".
struct CliArgs {
    /// `Some(level)` iff `--log-level=<level>` was passed (the
    /// value is already validated against
    /// `error|warn|info|debug|trace`). `None` means the
    /// subscriber falls back to `RUST_LOG` env var, then
    /// `info` default.
    log_level: Option<String>,
    /// `true` iff `--help` or `-h` was passed.
    help: bool,
}

/// Usage text printed on `--help` / `-h` and after a parse
/// error. Held in a `&'static str` constant so `print!` /
/// `eprint!` can take a borrow without re-allocating on each
/// invocation. Multi-line string with `\` line-continuations
/// keeps the embedded indentation readable in source.
const USAGE: &str = "\
Usage: cmdash [--log-level=<level>]

Options:
  --log-level=<level>   tracing-subscriber filter level: one of
                        error, warn, info, debug, trace
                        (case-insensitive). When set, overrides
                        both the $RUST_LOG env var and the
                        fallback 'info' default.
  -h, --help            print this help message and exit.

Notes:
  The flag is strictly pre-tracing_subscriber init: --log-level
  drives the EnvFilter for the binary's INFO/WARN/DEBUG/TRACE
  events from the FIRST tracing macro call forward. Crate-
  targeted filtering (e.g. cmdash_layout=debug) still routes
  through $RUST_LOG in the standard EnvFilter direct-string
  form, and is mutually exclusive with --log-level for a
  single launch.
";

/// Hand-rolled argv parser for the v1 launch surface (see
/// [`CliArgs`]). The full intended surface area is exactly:
///
/// - `--log-level=<level>` (mandatory value)
/// - `--log-level` bare ⇒ parse error (no implicit fallback)
/// - `--help` / `-h` ⇒ terminate-after-print
/// - any `--foo` / `-x` flag ⇒ parse error
/// - any positional arg ⇒ parse error
///
/// The bare `--log-level` case is rejected (rather than
/// silently falling through to "$RUST_LOG or info") so a
/// typo like `cmdash --log-level` does not quietly behave
/// differently from `cmdash` — a `warn!` event late in the
/// tick loop would mask a CLI mistake in the user's
/// mental model.
///
/// Takes an explicit `&[String]` rather than calling
/// `std::env::args()` directly so unit tests can drive it
/// without monkey-patching the process environment.
fn parse_cli_args(args: &[String]) -> Result<CliArgs, String> {
    let mut out = CliArgs {
        log_level: None,
        help: false,
    };
    for arg in args.iter().skip(1) {
        if arg == "--help" || arg == "-h" {
            out.help = true;
            continue;
        }
        if arg == "--log-level" {
            return Err(
                "cmdash: --log-level requires a value: --log-level=<level>".to_string(),
            );
        }
        if let Some(val) = arg.strip_prefix("--log-level=") {
            match val.to_ascii_lowercase().as_str() {
                "error" | "warn" | "info" | "debug" | "trace" => {
                    out.log_level = Some(val.to_string());
                }
                _ => {
                    return Err(format!(
                        "cmdash: invalid --log-level value {val:?} \
                         (expected error|warn|info|debug|trace)"
                    ));
                }
            }
            continue;
        }
        if arg.starts_with('-') {
            return Err(format!("cmdash: unknown flag: {arg:?}"));
        }
        return Err(format!(
            "cmdash: unexpected positional argument: {arg:?}"
        ));
    }
    Ok(out)
}

fn main() {
    // Parse args BEFORE tracing_subscriber::init() so the CLI
    // override can drive the EnvFilter before any subscriber
    // state is set globally. We exit 2 (standard Unix
    // usage-error) on bad arg syntax rather than forwarding
    // to `run()` so a typo never sneaks a wrong log level
    // through to the binary's first tracing macro call.
    let cli = match parse_cli_args(&std::env::args().collect::<Vec<_>>()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            eprintln!();
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    };
    if cli.help {
        print!("{USAGE}");
        return;
    }

    // Filter precedence, highest first:
    //   1. `--log-level=<level>` (CLI override)
    //   2. `RUST_LOG` env var (tracing-subscriber default)
    //   3. `info` (v1.0.0 release default)
    let env_filter = if let Some(level) = cli.log_level.as_deref() {
        tracing_subscriber::EnvFilter::new(level)
    } else if let Ok(env_filter) =
        tracing_subscriber::EnvFilter::try_from_default_env()
    {
        env_filter
    } else {
        tracing_subscriber::EnvFilter::new("info")
    };
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();
    if let Some(level) = &cli.log_level {
        info!(
            log_level = %level,
            "cmdash starting (cli log-level override); ratatui text body + dashcompositor kitty graphics"
        );
    } else {
        info!("cmdash starting (ratatui text body + dashcompositor kitty graphics)");
    }
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
    // Do NOT `drop(close_tx)` here — the primary sender is
    // MOVED into [`TickContext::new_full`] below so the
    // runtime mutation paths (`AppNewPane` reconciliation,
    // `PanePreset` rebuild) can spawn fresh `PaneRunner`s
    // against the SAME close-channel. Per-pane clones kept
    // inside each [`PaneRunner`] continue to fire on `Drop`;
    // the primary sender is now long-lived, matching the
    // binary's run-loop lifetime exactly.

    let bindings = Router::new(cfg.keybinds);
    // `focus` and `running` are MOVED into
    // `TickContext::new_full` below; they are never mutated
    // locally. `guard` and `ctx` stay `mut` because
    // `guard.as_mut()` and `ctx.run()` both take `&mut self`,
    // and `runners` is `mut` because the initial-frame spawn
    // loop calls `runners.push(r)`.
    let focus: usize = 0;
    let running = true;

    let mut guard = TerminalGuard::enter()?;
    let tick = Duration::from_millis(33);
    let mut ctx = TickContext::new_full(
        runners,
        bindings,
        focus,
        running,
        close_tx,
        close_rx,
        graphics,
        guard.as_mut(),
        tick,
        layout_root,
        None,
        total,
        cfg.presets,
        BTreeMap::new(),
        ShellSpec::LoginShell,
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
    /// Owned clone of the binary's paired `PaneCloseTx`. Retained
    /// so the runtime mutation paths (`AppNewPane` reconciliation,
    /// `PanePreset` rebuild) can wire fresh `PaneRunner`s into
    /// the SAME close-channel as the initial-frame spawn, preserving
    /// the Drop -> close_tx -> GraphicsState::close_pane round-trip.
    /// AGENTS.md §"Hard rule: one layer per instance" (a LayerId is
    /// bound to a pane instance for the instance's whole lifetime
    /// and is NEVER re-bound to a different pane).
    close_tx: PaneCloseTx,
    /// Last non-zero cell-grid area against which `relayout`
    /// succeeded. Used as the resolution target for runtime
    /// mutations (`AppNewPane`, `PaneClose`, `PanePreset`) when
    /// a SIGWINCH hasn't yet signalled. Defaults to (80, 24) on
    /// a zero-area initial-frame transient.
    last_area: LayoutRect,
    /// Saved layout bodies keyed by their KDL `name`. Populated
    /// from `cmdash_config::Config::presets` at startup. The
    /// `PanePreset(name)` runtime mutation looks up
    /// `self.presets[name]` and wholesale-swaps `self.layout_root`
    /// for the new tree.
    presets: BTreeMap<String, LayoutNode>,
    /// Phase 4 carry-forward: per-ZStack focus tracking. Maps
    /// the focused ZStack member's resolved [`cmdash_layout::PaneId`]
    /// to its index within the parent ZStack. Survives across
    /// `AppNewPane`/`PaneClose` InPlace cycles (label-keyed
    /// reconciliation preserves the member's PaneId when the
    /// sibling stays under the same Split/ZStack parent);
    /// cleared on `Wholesale` swap (`PanePreset`)
    /// reconciliation so a reloaded preset's stale PaneIds
    /// don't linger in the map.
    stack_focus: BTreeMap<PaneId, usize>,
    /// Default shell for runtime-spawned panes. v1 single shell
    /// (`LoginShell`) — `cmdash::run` wires the constant. A future
    /// per-pane shell override slots in here.
    shell: ShellSpec,
}

/// Monotonic LayerId allocator for
/// [`ReconcileMode::Wholesale`] spawns. LayerIds drawn from
/// `cmdash::derive_layer_id(&pane_id)` collide when the new
/// top of the swapped tree also has `pre_order == 0` (both
/// resolve to `LayerId(0)`), so wholesale spawns draw from
/// this counter instead — the swap-produced IDs only flow
/// through their fresh runner + the AGENTS.md §"Hard rule:
/// one layer per instance" exceptions noted in the
/// `ReconcileMode::Wholesale` docstring.
static NEXT_LAYER_ID: AtomicU32 = AtomicU32::new(1);

/// Draw the next monotonic LayerId for a wholesale-swap
/// spawn. Relaxed ordering is sufficient — each spawn only
/// needs "later spawns get later IDs", not strict
/// serialisation across threads (the binary's tick loop is
/// single-threaded).
fn alloc_layer_id() -> PaneLayerId {
    let n = NEXT_LAYER_ID.fetch_add(1, Ordering::Relaxed);
    PaneLayerId(n as u64)
}

/// Reconcile mode for [`TickContext::reconcile_runners`].
/// Selects whether survivors keep their `PaneLayerId`
/// (in-place, for `AppNewPane` / `PaneClose` rebalance) or
/// rotate every `PaneLayerId` (wholesale, for
/// `PanePreset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReconcileMode {
    /// In-place rebalance (`AppNewPane`, `PaneClose`): match
    /// survivors by their `pane.label` (labels are stable
    /// across sibling-absorption rebalance; PaneIds are NOT,
    /// so PaneId-keyed matching would drop the survivor
    /// spuriously). Survivors' `PaneLayerId` is preserved per
    /// the AGENTS.md Hard rule.
    InPlace,
    /// Wholesale swap (`PanePreset`): every old runner is
    /// dropped (its `Drop` revokes the `LayerId` via close_tx)
    /// and every new pane is spawned with a freshly-allocated
    /// `PaneLayerId` (from [`alloc_layer_id`], NOT from
    /// `cmdash::derive_layer_id &pane_id`, because both
    /// would collide on `LayerId(0)` when the new tree's
    /// top pane has `pre_order == 0`). The wholesale swap is
    /// a different topology, not a same-instance mutation,
    /// so the Hard rule's "no rebinding" wording does not
    /// apply.
    Wholesale,
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
    ///
    /// This 10-arg ctor delegates to [`Self::new_full`] with
    /// default `close_tx` (an unused fresh channel whose Sender
    /// is dropped on ctx drop), default `last_area` (80x24),
    /// empty `presets` map, and `ShellSpec::LoginShell`. Tests
    /// that don't exercise the runtime mutation paths
    /// (`AppNewPane` / `PaneFocus{Direction}` / `PanePreset`)
    /// should keep using this; production's `cmdash::run` goes
    /// through [`Self::new_full`] directly so the
    /// runtime-spawn paths share the ctx's `close_tx` Sender.
    // The 10-arg ctor is the most central tenant of the AGENTS.md
    // "minimal API surface" rule -- it mirrors the ten
    // user-provided struct fields one-to-one.
    #[allow(
        dead_code,
        reason = "v1 free-fn fallback for input_tests signature stability; superseded by TickContext::new_full in production use"
    )]
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
        // Default close_tx whose Receiver is never read; v1 tests
        // that build ctx via `new()` don't exercise runtime
        // mutations, so the dead Sender never sends.
        let (close_tx, _close_rx_default): (Sender<cmdash_pty::PaneLayerId>, _) =
            std::sync::mpsc::channel();
        let last_area = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        Self::new_full(
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
            BTreeMap::new(),
            // Phase 4 carry-forward: `new()` (10-arg, used by
            // legacy test fixtures that don't exercise the
            // runtime mutation paths) initializes the
            // per-ZStack focus map empty. Tests that drive
            // ZStack focus actions build via `new_full`
            // directly and can pre-seed `stack_focus` if
            // they want a non-empty starting state.
            BTreeMap::new(),
            ShellSpec::LoginShell,
        )
    }

    /// Construct a [`TickContext`] from all 14 per-frame
    /// building blocks, including the runtime-mutation hooks
    /// (`close_tx: PaneCloseTx`, `last_area: LayoutRect`,
    /// `presets: BTreeMap<String, LayoutNode>`,
    /// `shell: ShellSpec`). Enforces the same `focus <
    /// runners.len()` invariant as [`Self::new`].
    ///
    // Production's `cmdash::run` calls this. Buffered into a
    // sub-struct would just duplicate the schema-history
    // coupling the AGENTS.md "minimal API surface" rule
    // discourages.
    #[allow(clippy::too_many_arguments)]
    pub fn new_full(
        runners: Vec<PaneRunner>,
        bindings: Router,
        focus: usize,
        running: bool,
        close_tx: PaneCloseTx,
        close_rx: Receiver<cmdash_pty::PaneLayerId>,
        graphics: GraphicsState,
        terminal: &'a mut Terminal<B>,
        tick: Duration,
        layout_root: LayoutNode,
        pending_resize: Option<(u16, u16)>,
        last_area: LayoutRect,
        presets: BTreeMap<String, LayoutNode>,
        stack_focus: BTreeMap<PaneId, usize>,
        shell: ShellSpec,
    ) -> Self {
        assert!(
            focus < runners.len(),
            "TickContext::new_full: focus ({focus}) is out of bounds for {} runners",
            runners.len(),
        );
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

    /// Apply a [`KeyAction`] to the full [`TickContext`] —
    /// both the v1 arms (AppClose, PaneFocusNext,
    /// PaneFocusPrev) and the new carry-forward arms
    /// (`AppNewPane`, `PaneFocus{Up,Down,Left,Right}`,
    /// `PaneClose` rebalance, `PanePreset(name)`). The free
    /// [`apply_action`] fn is a parallel impl kept for the
    /// v1 input_tests' signature stability; the binary's tick
    /// loop drives THIS method through [`Self::handle_event_full`].
    pub fn apply_action_full(&mut self, action: KeyAction) {
        match action {
            KeyAction::AppClose => {
                self.running = false;
            }
            KeyAction::PaneFocusNext => {
                if !self.runners.is_empty() {
                    self.focus = (self.focus + 1) % self.runners.len();
                }
            }
            KeyAction::PaneFocusPrev => {
                if !self.runners.is_empty() {
                    self.focus = (self.focus + self.runners.len() - 1) % self.runners.len();
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
        }
    }

    /// Phase 0 of the AGENTS.md rendering pipeline, full
    /// version. Drains crossterm events and routes each one
    /// through [`Self::handle_event_full`]. Non-blocking;
    /// bounded by `event::poll(0)`. The free [`input_phase`]
    /// helper is a parallel impl retained for v1 input_tests
    /// signature compatibility.
    pub fn input_phase_full(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        while event::poll(Duration::from_millis(0))? {
            let evt = event::read()?;
            self.handle_event_full(&evt);
        }
        Ok(())
    }

    pub fn handle_event_full(&mut self, evt: &Event) {
        if let Some(action) = self.bindings.dispatch_crossterm(evt) {
            self.apply_action_full(action);
            return;
        }
        // Host SIGWINCH coalescer — same arm the free
        // [`handle_event`] had under v1. Phase 0.5 drains the
        // slot (`pending_resize.take()`) at the top of the
        // next tick to drive [`Self::relayout`].
        if let Event::Resize(w, h) = evt {
            self.pending_resize = Some((*w, *h));
            return;
        }
        let Event::Key(KeyEvent {
            code,
            kind,
            modifiers: _,
            ..
        }) = evt
        else {
            return;
        };
        if !matches!(kind, KeyEventKind::Press) {
            return;
        }
        let Some(bytes) = event_to_bytes(*code) else {
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

    /// Carry-forward: `PaneFocus{Direction}`. Resolve the
    /// adjacent pane in `dir` from the focused pane via
    /// [`adjacent_pane`]'s rect-proximity algorithm
    /// (max perpendicular overlap → min distance → min
    /// `pre_order`); swap `self.focus` to the matching
    /// runner's Vec index. No-op if no neighbour exists.
    fn focus_by_direction(&mut self, dir: Direction) {
        if self.runners.is_empty() {
            return;
        }
        if self.focus >= self.runners.len() {
            self.focus = self.runners.len() - 1;
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
            self.focus = idx;
        }
    }

    /// Phase 4 carry-forward: locate the focused pane's
    /// parent ZStack + its member index. Returns
    /// `Some((parent_path, member_idx))` if the focused
    /// pane is a direct child of a `LayoutNode::ZStack`,
    /// otherwise `None` -- the caller interprets `None`
    /// as "focused pane is not a ZStack member" and
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
    /// **inside** the ZStack -- and `self.stack_focus` is
    /// **always** updated (even on the wrap-around, since
    /// the post-wrap focus still lives inside the ZStack
    /// and the keyed entry tracks the new member index).
    /// `PaneStackCycle` therefore has no handoff path -- it
    /// is a closed cycle within the ZStack.
    ///
    /// `crosstack_member` looks superficially combinable
    /// (both primitives drive ZStack member indices) but
    /// has the OPPOSITE boundary post-condition: at the
    /// FIRST or LAST member it **escapes** the ZStack via
    /// `focus_by_direction(handoff_direction)` and never
    /// mutates `stack_focus` on the handoff path -- the
    /// new focus lands outside the ZStack, so any keyed
    /// entry would go stale.
    ///
    /// **Trapdoor precedent** -- [`cmdash_layout::split_rect`] in
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
    /// focused pane's parent ZStack + member index, then
    /// advance `self.focus` to the next member in
    /// declaration order, wrapping from the last member
    /// back to the first. No-op if the focused pane is
    /// not a ZStack member.
    fn handle_stack_cycle(&mut self) {
        if self.runners.is_empty() {
            return;
        }
        if self.focus >= self.runners.len() {
            self.focus = self.runners.len() - 1;
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
            self.focus = idx;
            let new_id = self.runners[idx].computed().id;
            self.stack_focus.insert(new_id, next_idx);
        }
    }

    /// Phase 4 + 4.5/5 carry-forward consolidation: directed
    /// ZStack focus primitive. Replaces the 4
    /// near-byte-identical `handle_stack_down`/`up`/`left`/`right`
    /// fns from prior phases; folds their boundary condition
    /// + boundary-handoff shape into a single 2-argument
    ///
    /// Arguments:
    /// - `handoff_direction`: the [`Direction`] the helper
    ///   delegates to when the focused member sits at the
    ///   boundary that needs to escape the ZStack. For advance
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
    ///   via [`cmdash_layout::adjacent_pane`].
    /// - The handoff path does NOT mutate `stack_focus` (the
    ///   new focus is OUTSIDE the ZStack, so the keyed
    ///   stack-focus-map entry would never be queried).
    /// - Algorithmic-shape divergence vs `handle_stack_cycle`.
    ///   These two primitives look combinable (both drive
    ///   ZStack member indices in declaration order) but
    ///   carry fundamentally different post-conditions:
    ///   - `crosstack_member` (this helper) at the FIRST or
    ///     LAST member is a **boundary-hand-off** primitive:
    ///     it **escapes** the ZStack by delegating to
    ///     `focus_by_direction(handoff_direction)`, and it
    ///     **never mutates `stack_focus`** on the handoff
    ///     path -- the new focus lands OUTSIDE the ZStack,
    ///     so any keyed entry for the old focus would be
    ///     stale and we deliberately drop it.
    ///   - `handle_stack_cycle` is a **modulo-wrap**
    ///     primitive: at the LAST member the arithmetic
    ///     `(member_idx + 1) % panes.len()` wraps the focus
    ///     BACK to the FIRST member (it stays **inside**
    ///     the ZStack), and it **always mutates
    ///     `stack_focus`** -- even on the wrap-around, the
    ///     keyed member-index entry tracks the post-wrap
    ///     focus.
    ///
    ///   Folding these into one fn would tangle two
    ///   different post-conditions behind a single
    ///   conditional branch -- an anti-pattern. They are
    ///   intentionally separate.
    ///
    /// - **Trapdoor precedent** -- the [`cmdash_layout::split_rect`]
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
            self.focus = self.runners.len() - 1;
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
                self.focus = idx;
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
                self.focus = idx;
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
            self.focus = self.runners.len() - 1;
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
                    }),
                ],
            };
            self.reconcile_runners(ReconcileMode::InPlace);
            return;
        }
        let new_leaf = LayoutNode::Pane(CfgPane {
            kind: PaneKind::Shell,
            label: None,
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
            self.focus = self.runners.len() - 1;
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
            self.focus = self.runners.len() - 1;
        }
        self.reconcile_runners(ReconcileMode::InPlace);
    }

    /// Carry-forward: `PanePreset(name)`. Look up
    /// `self.presets[name]`. If present, drop ALL runners
    /// (their `Drop`s revoke every `LayerId` via close_tx
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
    ///      on close_tx; next phase 1 revokes the
    ///      dashcompositor layer).
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
        let mut survivors: std::collections::HashMap<String, PaneRunner> =
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
                            survivors.insert(label, r);
                            continue;
                        }
                    }
                    to_drop.push(r);
                }
            }
        }
        // Drop dead runners: their `Drop` fires `close_tx`
        // -> next phase 1 revokes the dashcompositor layers
        // (Hard rule: no orphan LayerIds).
        drop(to_drop);
        // Build the new runner Vec: rebind for survivors (so
        // the survivor's LayerId stays stable per Hard rule),
        // spawn fresh LayerIds for genuinely new panes / for
        // Wholesale slots.
        let mut new_runners: Vec<PaneRunner> = Vec::with_capacity(post_layout.panes.len());
        for pane in &post_layout.panes {
            let survivor_opt: Option<PaneRunner> = if matches!(mode, ReconcileMode::InPlace) {
                pane.label
                    .as_deref()
                    .map(str::to_string)
                    .and_then(|l| survivors.remove(&l))
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
                    cmdash::derive_layer_id(&pane.id)
                };
                let tx: PaneCloseTx = self.close_tx.clone();
                match PaneRunner::spawn_with_graphics(
                    pane.clone(),
                    layer_id,
                    self.shell.clone(),
                    Some(tx),
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
        // Drop any survivors that didn't get consumed (e.g.,
        // a label vanished across the layout swap).
        drop(survivors);
        self.runners = new_runners;
        self.graphics
            .set_cells((self.last_area.w, self.last_area.h));
    }

    /// Drive the AGENTS.md rendering pipeline until `running`
    /// flips `false` or every pane exits. The loop body is the
    /// same logic that lived in the prior free `tick_loop`
    /// function; bundling it on this struct lets `cmdash::run`
    /// invoke it as a one-shot `ctx.run()`. Phase 0 input is
    /// routed through [`Self::input_phase_full`] so the
    /// carry-forward arms reach the live binary.
    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        loop {
            // Phase 0: drain input events via the FULL action
            // handler (v1 arms + AppNewPane + PaneFocus{Direction}
            // + PanePreset). Non-blocking; bound by
            // `event::poll(Duration::from_millis(0))`. Each Press
            // event is routed through [`Self::handle_event_full`]
            // which dispatches via [`Self::apply_action_full`],
            // OR forwarded as bytes to the focused pane. The
            // carry-forward UX reaches the user through this path.
            self.input_phase_full()?;
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
#[allow(
    dead_code,
    reason = "v1 free-fn fallback for input_tests signature stability; superseded by TickContext::input_phase_full in production use"
)]
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

#[allow(
    dead_code,
    reason = "v1 free-fn fallback for input_tests signature stability; superseded by TickContext::handle_event_full in production use"
)]
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

#[allow(
    dead_code,
    reason = "v1 free-fn fallback for input_tests signature stability; superseded by TickContext::apply_action_full in production use"
)]
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
mod cli_args_tests {
    //! Unit tests for [`parse_cli_args`]. The function is
    //! intentionally hand-rolled to keep the v1 binary's
    //! dep graph free of `clap` / `pico-args`, so the test
    //! surface here pins the language-level behavior
    //! (validation, error messages, set/clear of the override
    //! field) rather than deferring to a third-party parser
    //! crate's table-driven self-tests. The
    //! `log_level_verbose_shorthand_is_still_rejected` test
    //! in particular is a deliberate "design choice" lock so
    //! a future contributor adding `-v` shorthand has to
    //! address the ask_user choice behind the v1 launch
    //! surface first.
    use super::{parse_cli_args, CliArgs};

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn log_level_all_six_valid_levels_parse_ok() {
        for level in ["error", "warn", "info", "debug", "trace"] {
            let raw = format!("--log-level={level}");
            let parsed = parse_cli_args(&argv(&["cmdash", &raw]))
                .unwrap_or_else(|e| panic!("{level} must parse: {e}"));
            assert_eq!(
                parsed.log_level.as_deref(),
                Some(level),
                "validator must echo the canonical lowercase form"
            );
            assert!(!parsed.help);
        }
    }

    #[test]
    fn log_level_upper_case_is_accepted_with_canonical_lowercase_casing() {
        let parsed = parse_cli_args(&argv(&["cmdash", "--log-level=DEBUG"]))
            .expect("--log-level=DEBUG must parse (case-insensitive)");
        assert_eq!(
            parsed.log_level.as_deref(),
            Some("DEBUG"),
            "parser preserves original casing in the stored value; \
             EnvFilter normalizes it downstream"
        );
    }

    #[test]
    fn log_level_bare_flag_is_rejected_with_usage_error_message() {
        let err = parse_cli_args(&argv(&["cmdash", "--log-level"]))
            .expect_err("--log-level with no =value must be a parse error");
        assert!(
            err.contains("--log-level") && err.contains("requires a value"),
            "error message must name the flag and the missing-value requirement: {err}"
        );
    }

    #[test]
    fn log_level_unknown_value_is_rejected_with_the_offending_value_quoted() {
        let err = parse_cli_args(&argv(&["cmdash", "--log-level=BOGUS"]))
            .expect_err("--log-level=BOGUS must be a parse error");
        assert!(
            err.contains("BOGUS") && err.contains("error"),
            "error message must quote the offending value and list valid levels: {err}"
        );
    }

    #[test]
    fn log_level_verbose_shorthand_is_still_rejected_in_v1() {
        // Deliberate: the user explicitly chose --log-level=<level>
        // over -v / --verbose via ask_user. Lock that decision
        // so a future contributor adding `-v` shorthand has to
        // address the ask_user precedent before merging.
        assert!(
            parse_cli_args(&argv(&["cmdash", "-v"])).is_err(),
            "-v must remain rejected; --log-level=<level> is the v1 binary's only verbosity dial"
        );
        assert!(
            parse_cli_args(&argv(&["cmdash", "--verbose"])).is_err(),
            "--verbose must remain rejected; --log-level=<level> is the v1 binary's only verbosity dial"
        );
    }

    #[test]
    fn help_flag_short_and_long_set_help_true_and_leave_level_none() {
        for flag in ["--help", "-h"] {
            let parsed = parse_cli_args(&argv(&["cmdash", flag]))
                .unwrap_or_else(|e| panic!("{flag} must parse, not error: {e}"));
            assert!(parsed.help, "{flag} must turn help=true");
            assert!(
                parsed.log_level.is_none(),
                "{flag} must not affect log_level"
            );
        }
    }

    #[test]
    fn unknown_long_flag_is_rejected() {
        let err = parse_cli_args(&argv(&["cmdash", "--unknown-flag"]))
            .expect_err("--unknown-flag must be a parse error");
        assert!(
            err.contains("unknown") && err.contains("--unknown-flag"),
            "error message must name the unknown flag: {err}"
        );
    }

    #[test]
    fn positional_argument_is_rejected() {
        let err = parse_cli_args(&argv(&["cmdash", "extra"]))
            .expect_err("positional arg must be a parse error");
        assert!(
            err.contains("positional") && err.contains("\"extra\""),
            "error message must name the offending token: {err}"
        );
    }

    #[test]
    fn no_args_returns_empty_with_help_false_and_level_none() {
        let parsed = parse_cli_args(&argv(&["cmdash"]))
            .expect("argv = [\"cmdash\"] is the no-args case and must parse");
        assert_eq!(parsed.log_level, None);
        assert!(!parsed.help);
    }

    #[test]
    fn cli_args_struct_can_be_constructed_with_default_field_set() {
        // A future revision that adds a new field to CliArgs
        // will break this test site as a "remember to update the
        // tests" reminder rather than silently absorbing the new
        // field.
        let _ = CliArgs {
            log_level: None,
            help: false,
        };
    }
}

// Existing `input_tests` module: tick-loop regression tests that
// drive `cmdash::run` end-to-end via the test backend. Adding
// `cli_args_tests` ABOVE rather than BELOW this module keeps
// related test modules grouped together and the new tests visible
// immediately when a future reader opens the bottom of the file.
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

    // ============================================================
    // Phase 2 carry-forward regression tests for the runtime-
    // mutation arms wired through `TickContext::apply_action_full`.
    // These pin the AGENTS.md Phase 2 carry-forward invariants:
    //   - AppNewPane: original focused pane's `LayerId` is
    //     preserved per the Hard rule (`runners[0].layer_id()`
    //     matches the pre-action LayerId).
    //   - PaneFocus{Direction}: rect-proximity selects the
    //     adjacent pane via [`cmdash_layout::adjacent_pane`].
    //   - PaneClose rebalance: with a 2-leaf Split, closing the
    //     focused leaf collapses the tree to the survivor.
    //   - PanePreset: wholesale swap replaces `self.layout_root`
    //     AND revokes the original pane's `LayerId` (Hard rule).
    // Each test drives the FULL action handler end-to-end via
    // `apply_action_full`, NOT the legacy free `apply_action`
    // v1 input_tests use.
    // ============================================================

    /// AppNewPane against a single-leaf `TickContext`: the
    /// focused leaf becomes child 0 of a fresh Horizontal
    /// Split (ratio 50), a new leaf spawn at child 1, and
    /// `reconcile_runners` brings `Vec<PaneRunner>` to length
    /// 2. The original focused pane's `LayerId` is stable per
    /// AGENTS.md Hard rule (a LayerId is bound to a pane
    /// instance for its whole lifetime and is NOT re-bound).
    #[test]
    fn app_new_pane_splits_focused_leaf_and_spawns_runner() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout { pane kind=shell label="alpha" }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse single-pane config");
        let layout_root = cfg.layout.expect("layout block");
        let initial_layout = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute initial-layout");
        let pane = initial_layout.panes[0].clone();
        let original_layer = cmdash::derive_layer_id(&pane.id);
        let runner =
            PaneRunner::spawn_with_graphics(pane, original_layer, shell, Some(close_tx.clone()))
                .expect("spawn single-pane runner");
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            vec![runner],
            bindings,
            0,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        ctx.apply_action_full(KeyAction::AppNewPane);
        // After AppNewPane on a single-leaf root: layout_root
        // became a Split with two leaves; reconcile_runners
        // spawned one fresh PaneRunner. Original focused
        // pane's LayerId is preserved.
        assert_eq!(
            ctx.runners.len(),
            2,
            "AppNewPane on a single-leaf root must yield 2 PaneRunners"
        );
        let post_layout =
            ComputedLayout::compute(&ctx.layout_root, ctx.last_area).expect("post-Split compute");
        assert_eq!(post_layout.panes.len(), 2);
        assert_eq!(
            ctx.runners[0].layer_id(),
            original_layer,
            "AppNewPane preserves the original focused pane's LayerId per AGENTS.md Hard rule"
        );
    }

    /// PaneFocus{Direction} via rect-proximity
    /// ([`cmdash_layout::adjacent_pane`]) on a 2-leaf horizontal
    /// Split: leftmost pane's Right selects the right pane;
    /// pressing Right again on the rightmost is a no-op
    /// (no neighbour); Left from the rightmost returns to the
    /// leftmost.
    #[test]
    fn pane_focus_right_resolves_to_adjacent_pane_via_rect_proximity() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            split axis=horizontal ratio=0.6 {
                pane kind=shell label="left"
                pane kind=shell label="right"
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial_layout = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        let pane_a = initial_layout.panes[0].clone();
        let pane_b = initial_layout.panes[1].clone();
        let id_a = cmdash::derive_layer_id(&pane_a.id);
        let id_b = cmdash::derive_layer_id(&pane_b.id);
        let r0 =
            PaneRunner::spawn_with_graphics(pane_a, id_a, shell.clone(), Some(close_tx.clone()))
                .expect("spawn r0");
        let r1 = PaneRunner::spawn_with_graphics(pane_b, id_b, shell, Some(close_tx.clone()))
            .expect("spawn r1");
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            vec![r0, r1],
            bindings,
            0,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        assert_eq!(ctx.focus, 0);
        ctx.apply_action_full(KeyAction::PaneFocusRight);
        assert_eq!(
            ctx.focus, 1,
            "PaneFocusRight on the leftmost pane of a 2-leaf horizontal split selects the right pane"
        );
        ctx.apply_action_full(KeyAction::PaneFocusRight);
        assert_eq!(
            ctx.focus, 1,
            "PaneFocusRight against the rightmost pane is a no-op (no neighbour)"
        );
        ctx.apply_action_full(KeyAction::PaneFocusLeft);
        assert_eq!(
            ctx.focus, 0,
            "PaneFocusLeft from the rightmost pane returns to the left pane"
        );
    }

    /// PaneClose rebalance: with focus on child 0 of a 2-leaf
    /// Split, the Split's sibling-absorption rebalance
    /// collapses the Split into child 1; `reconcile_runners`
    /// rebuilds `Vec<PaneRunner>` against the post-rebalance
    /// tree with the survivor's `LayerId` intact.
    #[test]
    fn pane_close_rebalance_collapses_split_to_one_leaf() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            split axis=horizontal ratio=0.6 {
                pane kind=shell label="left"
                pane kind=shell label="right"
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial_layout = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        let pane_a = initial_layout.panes[0].clone();
        let pane_b = initial_layout.panes[1].clone();
        let id_a = cmdash::derive_layer_id(&pane_a.id);
        let id_b = cmdash::derive_layer_id(&pane_b.id);
        let r0 =
            PaneRunner::spawn_with_graphics(pane_a, id_a, shell.clone(), Some(close_tx.clone()))
                .expect("spawn r0");
        let r1 = PaneRunner::spawn_with_graphics(pane_b, id_b, shell, Some(close_tx.clone()))
            .expect("spawn r1");
        let survivor_layer = r1.layer_id();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            vec![r0, r1],
            bindings,
            0,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        ctx.apply_action_full(KeyAction::PaneClose);
        // After PaneClose on focus=0 of a 2-leaf Split: the
        // Split collapses to its right survivor; survivor's
        // LayerId is intact per AGENTS.md Hard rule; binary
        // does NOT quit (one pane left).
        assert_eq!(ctx.runners.len(), 1);
        assert!(ctx.running, "one pane left => binary does NOT quit");
        assert_eq!(
            ctx.runners[0].layer_id(),
            survivor_layer,
            "PaneClose rebalance: survivor pane keeps its LayerId per AGENTS.md Hard rule"
        );
        let post_layout = ComputedLayout::compute(&ctx.layout_root, ctx.last_area)
            .expect("post-rebalance compute");
        assert_eq!(post_layout.panes.len(), 1);
    }

    /// PanePreset(name): wholesale layout_root swap; the
    /// original pane's LayerId is revoked (Hard rule); the new
    /// tree has fresh LayerIds per pane. Pin: distinct fresh
    /// LayerIds per pane, AND the original LayerId does NOT
    /// appear in the post-state Vec.
    #[test]
    fn pane_preset_swaps_layout_root_and_reconciles_runners() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout { pane kind=shell label="alpha" }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial_layout = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        let pane = initial_layout.panes[0].clone();
        let original_layer = cmdash::derive_layer_id(&pane.id);
        let runner = PaneRunner::spawn_with_graphics(
            pane,
            original_layer,
            shell.clone(),
            Some(close_tx.clone()),
        )
        .expect("spawn alpha runner");
        // Synthesize the preset body locally so the test's
        // `presets` map gets the parsed `LayoutNode` without
        // depending on a top-level `presets { ... }` KDL
        // block in this particular synthetic cfg.
        let beta_cfg_text = r#"layout {
            split axis=horizontal ratio=0.6 {
                pane kind=shell label="beta-left"
                pane kind=shell label="beta-right"
            }
        }"#;
        let beta_cfg = cmdash_config::parse(beta_cfg_text).expect("parse beta");
        let beta_layout_root = beta_cfg.layout.expect("beta layout block");
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut presets = BTreeMap::new();
        presets.insert("beta".to_string(), beta_layout_root);
        let mut ctx = TickContext::new_full(
            vec![runner],
            bindings,
            0,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            presets,
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        ctx.apply_action_full(KeyAction::PanePreset("beta".to_string()));
        // After wholesale swap: 2 panes (the new preset body),
        // 2 fresh LayerIds. The original alpha runner is
        // dropped (its `Drop` enqueues its LayerId on close_tx
        // for the AGENTS.md Hard rule; the binary's tick-loop
        // would drain it on the next phase 1, no echo here).
        assert_eq!(ctx.runners.len(), 2);
        let post_layout =
            ComputedLayout::compute(&ctx.layout_root, ctx.last_area).expect("post-swap compute");
        assert_eq!(post_layout.panes.len(), 2);
        assert_ne!(
            ctx.runners[0].layer_id(),
            original_layer,
            "PanePreset: original LayerId MUST be revoked (Hard rule: no rebinding)"
        );
        assert_ne!(
            ctx.runners[0].layer_id(),
            ctx.runners[1].layer_id(),
            "PanePreset: distinct fresh LayerIds per pane"
        );
    }

    /// Phase 4 carry-forward: `PaneStackCycle` rotates
    /// focus through a focused ZStack's members with
    /// wrap-around (last member -> first member). The
    /// focused pane is the LAST member (top by z-order /
    /// pre_order) of a 3-member ZStack; cycling once
    /// must wrap it to the FIRST member. Pins the
    /// "within-ZStack rotatation" half of the Phase 4
    /// contract.
    #[test]
    fn pane_stack_cycle_wraps_around_zstack_focus() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            zstack {
                pane kind=shell label="a"
                pane kind=shell label="b"
                pane kind=shell label="c"
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        assert_eq!(initial.panes.len(), 3);
        let runners: Vec<PaneRunner> = initial
            .panes
            .iter()
            .map(|p| {
                PaneRunner::spawn_with_graphics(
                    p.clone(),
                    cmdash::derive_layer_id(&p.id),
                    shell.clone(),
                    Some(close_tx.clone()),
                )
                .expect("spawn")
            })
            .collect();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            runners,
            bindings,
            // Start focused on the LAST member ("c") so
            // cycling once must wrap to the first ("a").
            2,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        ctx.apply_action_full(KeyAction::PaneStackCycle);
        // After cycling from last -> first: focus moves to
        // the runner for "a" (path [0, 0]).
        let focused_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(focused_id.path(), &[0, 0][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("a".to_string()),
            "PaneStackCycle: last -> first wraps to the first member"
        );
        assert_eq!(ctx.stack_focus.get(&focused_id).copied(), Some(0));
    }

    /// Phase 4 carry-forward: `PaneStackDown` when the
    /// focused member is NOT the last of the ZStack
    /// advances to the next member in declaration order
    /// (no wrap; no geometric handoff). The handoff case
    /// is covered separately by
    #[test]
    /// `pane_stack_down_at_top_hands_off_to_pane_below`.
    fn pane_stack_down_within_stack_advances_to_next_member() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            zstack {
                pane kind=shell label="a"
                pane kind=shell label="b"
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        assert_eq!(initial.panes.len(), 2);
        let runners: Vec<PaneRunner> = initial
            .panes
            .iter()
            .map(|p| {
                PaneRunner::spawn_with_graphics(
                    p.clone(),
                    cmdash::derive_layer_id(&p.id),
                    shell.clone(),
                    Some(close_tx.clone()),
                )
                .expect("spawn")
            })
            .collect();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            runners,
            bindings,
            // Start focused on the FIRST member ("a"). Down
            // should advance to "b" (the next-in-declaration-
            // order member, NOT a wrap to "a" itself; that
            // is PaneStackCycle's job).
            0,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        ctx.apply_action_full(KeyAction::PaneStackDown);
        let focused_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(focused_id.path(), &[0, 1][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("b".to_string()),
            "PaneStackDown within-ZStack: advance to next member in declaration order"
        );
        assert_eq!(ctx.stack_focus.get(&focused_id).copied(), Some(1));
    }

    /// Phase 4 carry-forward: `PaneStackDown` at the
    /// ZStack's last (top by z-order / pre_order) member
    /// hands focus off to the topmost pane geometrically
    /// below the ZStack via [`adjacent_pane`]. The
    /// fixture's outer horizontal split places one
    /// default-configured pane ("below") under the
    /// ZStack so the geometry is unambiguous; the ZStack
    /// occupies the top half (y=0..12), the below-pane
    /// occupies the bottom half (y=12..24). Focus the
    /// LAST member of the ZStack ("top") and press
    #[test]
    /// PaneStackDown; focus must hand off to "below"
    /// (path [0, 1]).
    fn pane_stack_down_at_top_hands_off_to_pane_below() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            split axis=vertical ratio=0.5 {
                zstack {
                    pane kind=shell label="bottom"
                    pane kind=shell label="top"
                }
                pane kind=shell label="below"
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        // 3 panes total: zstack[bottom], zstack[top],
        // below. Order of resolution: pre_order 0 = bottom,
        // 1 = top (the LAST ZStack member = top in z-order
        // + last in declaration order). pre_order 2 =
        // below.
        assert_eq!(initial.panes.len(), 3);
        let runners: Vec<PaneRunner> = initial
            .panes
            .iter()
            .map(|p| {
                PaneRunner::spawn_with_graphics(
                    p.clone(),
                    cmdash::derive_layer_id(&p.id),
                    shell.clone(),
                    Some(close_tx.clone()),
                )
                .expect("spawn")
            })
            .collect();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            runners,
            bindings,
            // Start focused on the LAST ZStack member
            // ("top") -- pre_order 1, path [0, 0, 1] inside
            // the tree.
            1,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        // Sanity: focused runner is the LAST ZStack member.
        let pre_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(pre_focus_id.path(), &[0, 0, 1][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("top".to_string())
        );
        ctx.apply_action_full(KeyAction::PaneStackDown);
        // After the handoff: focus moved to the geometric
        // neighbour below the ZStack -- which is the
        // "below" pane at path [0, 1].
        assert_ne!(
            ctx.focus, 1,
            "PaneStackDown at last member must hand focus off (NOT stay at the same index)"
        );
        let post_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(post_focus_id.path(), &[0, 1][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("below".to_string()),
            "PaneStackDown at last ZStack member must hand focus off to the pane below"
        );
        // The handoff path also doesn't add to stack_focus:
        // the new focus is OUTSIDE the ZStack, so the
        // focused_pane's `focused_zstack_context` lookup
        // returns None, the helper is a no-op for the
        // stack_focus map. The map should be empty after
        // the handoff (no entries from this test's
        // run-through).
        assert!(
            ctx.stack_focus.is_empty(),
            "PaneStackDown handoff target is outside the ZStack; stack_focus should stay empty"
        );
    }

    #[test]
    fn pane_stack_up_within_stack_advances_to_previous_member() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            zstack {
                pane kind=shell label="a"
                pane kind=shell label="b"
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        assert_eq!(initial.panes.len(), 2);
        let runners: Vec<PaneRunner> = initial
            .panes
            .iter()
            .map(|p| {
                PaneRunner::spawn_with_graphics(
                    p.clone(),
                    cmdash::derive_layer_id(&p.id),
                    shell.clone(),
                    Some(close_tx.clone()),
                )
                .expect("spawn")
            })
            .collect();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            runners,
            bindings,
            // Start focused on the SECOND member ("b"). Up
            // should retreat to "a" (the previous-in-
            // declaration-order member; not a wrap to "b"
            // itself -- that's PaneStackCycle's job).
            1,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        ctx.apply_action_full(KeyAction::PaneStackUp);
        let focused_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(focused_id.path(), &[0, 0][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("a".to_string()),
            "PaneStackUp within-ZStack: retreat to previous member in declaration order"
        );
        assert_eq!(ctx.stack_focus.get(&focused_id).copied(), Some(0));
    }

    #[test]
    fn pane_stack_up_at_bottom_hands_off_to_pane_above() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            split axis=vertical ratio=0.5 {
                pane kind=shell label="above"
                zstack {
                    pane kind=shell label="bottom"
                    pane kind=shell label="top"
                }
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        // 3 panes total: above, zstack[bottom], zstack[top].
        // pre_order 0 = above (top half y=0..12),
        // pre_order 1 = bottom (FIRST ZStack member =
        // bottom of z-order; cell y=12..24 shared with
        // pre_order 2), pre_order 2 = top (LAST ZStack
        // member; same y range).
        assert_eq!(initial.panes.len(), 3);
        let runners: Vec<PaneRunner> = initial
            .panes
            .iter()
            .map(|p| {
                PaneRunner::spawn_with_graphics(
                    p.clone(),
                    cmdash::derive_layer_id(&p.id),
                    shell.clone(),
                    Some(close_tx.clone()),
                )
                .expect("spawn")
            })
            .collect();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            runners,
            bindings,
            // Start focused on the FIRST ZStack member
            // ("bottom") -- pre_order 1, path [0, 1, 0]
            // inside the tree.
            1,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        // Sanity: focused runner is the FIRST ZStack member.
        let pre_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(pre_focus_id.path(), &[0, 1, 0][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("bottom".to_string())
        );
        ctx.apply_action_full(KeyAction::PaneStackUp);
        // After the handoff: focus moved to the geometric
        // neighbour above the ZStack -- which is the
        // "above" pane at path [0, 0].
        assert_ne!(
            ctx.focus, 1,
            "PaneStackUp at first member must hand focus off (NOT stay at the same index)"
        );
        let post_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(post_focus_id.path(), &[0, 0][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("above".to_string()),
            "PaneStackUp at first ZStack member must hand focus off to the pane above"
        );
        // The handoff path also doesn't add to stack_focus:
        // the new focus is OUTSIDE the ZStack, so the
        // focused_pane's `focused_zstack_context` lookup
        // returns None, the helper is a no-op for the
        // stack_focus map.
        assert!(
            ctx.stack_focus.is_empty(),
            "PaneStackUp handoff target is outside the ZStack; stack_focus should stay empty"
        );
    }

    /// Phase 4.5/5 carry-forward: `PaneStackLeft` retreats to
    /// the **previous** member of the focused ZStack in
    /// declaration order. Stop before the first member (no
    /// handoff in this test).
    #[test]
    fn pane_stack_left_within_stack_retreats_to_previous_member() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            zstack {
                pane kind=shell label="a"
                pane kind=shell label="b"
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        assert_eq!(initial.panes.len(), 2);
        let runners: Vec<PaneRunner> = initial
            .panes
            .iter()
            .map(|p| {
                PaneRunner::spawn_with_graphics(
                    p.clone(),
                    cmdash::derive_layer_id(&p.id),
                    shell.clone(),
                    Some(close_tx.clone()),
                )
                .expect("spawn")
            })
            .collect();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            runners,
            bindings,
            // Start focused on pane "b" (member_idx=1, the
            // LAST entry in declaration order).
            1,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        // Sanity: focused runner is pane "b".
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("b".to_string())
        );
        ctx.apply_action_full(KeyAction::PaneStackLeft);
        // After within-stack Left: focus moved to pane "a"
        // (member_idx=0).
        assert_eq!(
            ctx.focus, 0,
            "PaneStackLeft within-ZStack must retreat to the previous member"
        );
        let post_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("a".to_string()),
            "PaneStackLeft within-ZStack: retreat to previous member in declaration order"
        );
        assert_eq!(ctx.stack_focus.get(&post_focus_id).copied(), Some(0));
    }

    /// Phase 4.5/5 carry-forward: `PaneStackRight` advances
    /// to the **next** member of the focused ZStack in
    /// declaration order. Stop before the last member (no
    /// handoff in this test).
    #[test]
    fn pane_stack_right_within_stack_advances_to_next_member() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            zstack {
                pane kind=shell label="a"
                pane kind=shell label="b"
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        assert_eq!(initial.panes.len(), 2);
        let runners: Vec<PaneRunner> = initial
            .panes
            .iter()
            .map(|p| {
                PaneRunner::spawn_with_graphics(
                    p.clone(),
                    cmdash::derive_layer_id(&p.id),
                    shell.clone(),
                    Some(close_tx.clone()),
                )
                .expect("spawn")
            })
            .collect();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            runners,
            bindings,
            // Start focused on pane "a" (member_idx=0, the
            // FIRST entry in declaration order).
            0,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        // Sanity: focused runner is pane "a".
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("a".to_string())
        );
        ctx.apply_action_full(KeyAction::PaneStackRight);
        // After within-stack Right: focus moved to pane "b"
        // (member_idx=1).
        assert_eq!(
            ctx.focus, 1,
            "PaneStackRight within-ZStack must advance to the next member"
        );
        let post_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("b".to_string()),
            "PaneStackRight within-ZStack: advance to next member in declaration order"
        );
        assert_eq!(ctx.stack_focus.get(&post_focus_id).copied(), Some(1));
    }

    /// Phase 4.5/5 carry-forward: `PaneStackRight` at the
    /// ZStack's last (rightmost-by-declaration) member hands
    /// focus off to the topmost pane geometrically to the
    /// RIGHT of the ZStack via [`adjacent_pane`]. The
    /// fixture's outer horizontal split places the ZStack in
    /// the left column (x=0..40) and a default-configured
    /// pane ("right_outside") in the right column (x=40..80)
    /// so the geometry is unambiguous. Focus the LAST
    /// member ("right_inside") and press PaneStackRight;
    /// focus must hand off to "right_outside" (path [0, 1]).
    /// Pinned by `split_rect_horizontal_60` in the
    /// cmdash-layout crate's ground-truth unit tests.
    #[test]
    fn pane_stack_right_at_last_member_hands_off_to_pane_right() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            split axis=horizontal ratio=0.5 {
                zstack {
                    pane kind=shell label="left_inside"
                    pane kind=shell label="right_inside"
                }
                pane kind=shell label="right_outside"
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        // 3 panes total: zstack[left_inside] at pre_order 0
        // (member_idx=0; x=0..40, y=0..24), zstack[right_inside]
        // at pre_order 1 (member_idx=1 LAST; same rect x=0..40,
        // y=0..24 as the ZStack overlay), right_outside at
        // pre_order 2 (x=40..80, y=0..24).
        assert_eq!(initial.panes.len(), 3);
        let runners: Vec<PaneRunner> = initial
            .panes
            .iter()
            .map(|p| {
                PaneRunner::spawn_with_graphics(
                    p.clone(),
                    cmdash::derive_layer_id(&p.id),
                    shell.clone(),
                    Some(close_tx.clone()),
                )
                .expect("spawn")
            })
            .collect();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            runners,
            bindings,
            // Start focused on the LAST ZStack member
            // ("right_inside") -- pre_order 1, path
            // [0, 0, 1] inside the tree.
            1,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        // Sanity: focused runner is the LAST ZStack member.
        let pre_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(pre_focus_id.path(), &[0, 0, 1][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("right_inside".to_string())
        );
        ctx.apply_action_full(KeyAction::PaneStackRight);
        // After the handoff: focus moved to the geometric
        // neighbour to the right of the ZStack -- which is
        // the "right_outside" pane at path [0, 1].
        assert_ne!(
            ctx.focus, 1,
            "PaneStackRight at last member must hand focus off (NOT stay at the same index)"
        );
        let post_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(post_focus_id.path(), &[0, 1][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("right_outside".to_string()),
            "PaneStackRight at last ZStack member must hand focus off to the pane to the right"
        );
        // Handoff target is OUTSIDE the ZStack, so stack_focus
        // should stay empty.
        assert!(
            ctx.stack_focus.is_empty(),
            "PaneStackRight handoff target is outside the ZStack; stack_focus should stay empty"
        );
    }

    /// Phase 4.5/5 carry-forward: `PaneStackLeft` at the
    /// ZStack's first (leftmost-by-declaration) member hands
    /// focus off to the topmost pane geometrically to the
    /// LEFT of the ZStack via [`adjacent_pane`]. The
    /// fixture's outer horizontal split places a default-
    /// configured pane ("left_outside") in the left column
    /// (x=0..40) and the ZStack in the right column
    /// (x=40..80) so the geometry is unambiguous. Focus the
    /// FIRST member ("left_inside") and press PaneStackLeft;
    /// focus must hand off to "left_outside" (path [0, 0]).
    #[test]
    fn pane_stack_left_at_first_member_hands_off_to_pane_left() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            split axis=horizontal ratio=0.5 {
                pane kind=shell label="left_outside"
                zstack {
                    pane kind=shell label="left_inside"
                    pane kind=shell label="right_inside"
                }
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        // 3 panes total: left_outside at pre_order 0
        // (x=0..40, y=0..24); zstack[left_inside] at
        // pre_order 1 (member_idx=0 FIRST; x=40..80,
        // y=0..24 -- ZStack overlay shares rect);
        // zstack[right_inside] at pre_order 2 (member_idx=1;
        // same rect).
        assert_eq!(initial.panes.len(), 3);
        let runners: Vec<PaneRunner> = initial
            .panes
            .iter()
            .map(|p| {
                PaneRunner::spawn_with_graphics(
                    p.clone(),
                    cmdash::derive_layer_id(&p.id),
                    shell.clone(),
                    Some(close_tx.clone()),
                )
                .expect("spawn")
            })
            .collect();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            runners,
            bindings,
            // Start focused on the FIRST ZStack member
            // ("left_inside") -- pre_order 1, path
            // [0, 1, 0] inside the tree.
            1,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        // Sanity: focused runner is the FIRST ZStack member.
        let pre_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(pre_focus_id.path(), &[0, 1, 0][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("left_inside".to_string())
        );
        ctx.apply_action_full(KeyAction::PaneStackLeft);
        // After the handoff: focus moved to the geometric
        // neighbour to the left of the ZStack -- which is the
        // "left_outside" pane at path [0, 0].
        assert_ne!(
            ctx.focus, 1,
            "PaneStackLeft at first member must hand focus off (NOT stay at the same index)"
        );
        let post_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(post_focus_id.path(), &[0, 0][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("left_outside".to_string()),
            "PaneStackLeft at first ZStack member must hand focus off to the pane to the left"
        );
        // Handoff target is OUTSIDE the ZStack, so stack_focus
        // should stay empty.
        assert!(
            ctx.stack_focus.is_empty(),
            "PaneStackLeft handoff target is outside the ZStack; stack_focus should stay empty"
        );
    }

    /// Phase 5.0 carry-forward: `PaneStackRight` on a ZStack
    /// with exactly ONE member must immediately hand off to
    /// `Direction::Right` rather than advancing the focus.
    /// The boundary check `member_idx + 1 == panes.len()`
    /// inside `crosstack_member` triggers regardless of the
    /// advance/retreat branch (single-member ZStacks hit
    /// BOTH boundary conditions by definition: `member_idx
    /// == 0` AND `member_idx + 1 == panes.len()`). This pins
    /// the edge case at the consolidated dispatch site so
    /// future additions of directional variants can't
    /// regress it silently. Use `axis=horizontal` (column
    /// split -- same y, different x) so the side pane sits
    /// in the geometric right of the 1-member ZStack.
    #[test]
    fn pane_stack_right_on_one_member_zstack_immediately_hands_off_to_right() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            split axis=horizontal ratio=0.5 {
                zstack {
                    pane kind=shell label="only_inside"
                }
                pane kind=shell label="right_outside"
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        // 2 panes total: zstack[only_inside] at pre_order 0
        // (member_idx=0, the ONLY member; x=0..40, y=0..24),
        // right_outside at pre_order 1 (x=40..80, y=0..24).
        assert_eq!(initial.panes.len(), 2);
        let runners: Vec<PaneRunner> = initial
            .panes
            .iter()
            .map(|p| {
                PaneRunner::spawn_with_graphics(
                    p.clone(),
                    cmdash::derive_layer_id(&p.id),
                    shell.clone(),
                    Some(close_tx.clone()),
                )
                .expect("spawn")
            })
            .collect();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            runners,
            bindings,
            // Start focused on the ONLY ZStack member
            // ("only_inside") -- pre_order 0, path
            // [0, 0, 0] inside the tree.
            0,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        // Sanity: focused runner is the ONLY ZStack member.
        let pre_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(pre_focus_id.path(), &[0, 0, 0][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("only_inside".to_string())
        );
        ctx.apply_action_full(KeyAction::PaneStackRight);
        // After the boundary handoff: focus moved to the
        // geometric neighbour to the right of the ZStack --
        // which is the "right_outside" pane at path [0, 1].
        assert_ne!(
            ctx.focus, 0,
            "PaneStackRight on a 1-member ZStack must immediately hand focus off (NOT stay at the same index)"
        );
        let post_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(post_focus_id.path(), &[0, 1][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("right_outside".to_string()),
            "PaneStackRight on a 1-member ZStack must hand focus off to the right"
        );
        // Handoff target is OUTSIDE the ZStack, so stack_focus
        // should stay empty.
        assert!(
            ctx.stack_focus.is_empty(),
            "PaneStackRight handoff target is outside the ZStack; stack_focus should stay empty"
        );
    }

    /// Phase 5.0 carry-forward: `PaneStackLeft` on a ZStack
    /// with exactly ONE member must immediately hand off to
    /// `Direction::Left` rather than retreating the focus.
    /// Horizontal-axis mirror of
    /// `pane_stack_right_on_one_member_zstack_immediately_hands_off_to_right`;
    /// same dual boundary-condition rationale (single-member
    /// ZStack hits BOTH `member_idx == 0` and
    /// `member_idx + 1 == panes.len()` from inside crosstack_member).
    #[test]
    fn pane_stack_left_on_one_member_zstack_immediately_hands_off_to_left() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            split axis=horizontal ratio=0.5 {
                pane kind=shell label="left_outside"
                zstack {
                    pane kind=shell label="only_inside"
                }
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        // 2 panes total: left_outside at pre_order 0
        // (x=0..40, y=0..24), zstack[only_inside] at
        // pre_order 1 (member_idx=0, the ONLY member;
        // x=40..80, y=0..24).
        assert_eq!(initial.panes.len(), 2);
        let runners: Vec<PaneRunner> = initial
            .panes
            .iter()
            .map(|p| {
                PaneRunner::spawn_with_graphics(
                    p.clone(),
                    cmdash::derive_layer_id(&p.id),
                    shell.clone(),
                    Some(close_tx.clone()),
                )
                .expect("spawn")
            })
            .collect();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            runners,
            bindings,
            // Start focused on the ONLY ZStack member
            // ("only_inside") -- pre_order 1, path
            // [0, 1, 0] inside the tree.
            1,
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        // Sanity: focused runner is the ONLY ZStack member.
        let pre_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(pre_focus_id.path(), &[0, 1, 0][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("only_inside".to_string())
        );
        ctx.apply_action_full(KeyAction::PaneStackLeft);
        // After the boundary handoff: focus moved to the
        // geometric neighbour to the left of the ZStack --
        // which is the "left_outside" pane at path [0, 0].
        assert_ne!(
            ctx.focus, 1,
            "PaneStackLeft on a 1-member ZStack must immediately hand focus off (NOT stay at the same index)"
        );
        let post_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(post_focus_id.path(), &[0, 0][..]);
        assert_eq!(
            ctx.runners[ctx.focus].computed().label,
            Some("left_outside".to_string()),
            "PaneStackLeft on a 1-member ZStack must hand focus off to the left"
        );
        // Handoff target is OUTSIDE the ZStack, so stack_focus
        // should stay empty.
        assert!(
            ctx.stack_focus.is_empty(),
            "PaneStackLeft handoff target is outside the ZStack; stack_focus should stay empty"
        );
    }

    // ============================================================
    // Phase 6 carry-forward regression tests for the
    // `PaneStackCycle` modulo-wrap primitive. These pin the
    // boundary-condition corners of the cycle algorithm --
    // specifically the 1-member wrap-to-self corner and the
    // 3-member last-to-first wrap corner -- WITHOUT using
    // the `split axis=horizontal/vertical` trapdoor fixture
    // (cycle never handoffs via `focus_by_direction`, so the
    // axis-trapdoor split semantics are irrelevant to its
    // algorithm -- they would only confound the assertions).
    // ============================================================

    /// Phase 6 carry-forward: `PaneStackCycle` on a ZStack
    /// with exactly ONE member wraps modulo-style:
    /// `(0 + 1) % 1 == 0` -- the SAME member. Pin: focus
    /// idx stays at 0 (no escape, no handoff to a sibling
    /// Split member), AND `stack_focus` records the post-
    /// wrap focus idx (0) -- the keyed member-index entry
    /// tracks the cycle result even when the focus identity
    /// doesn't change. This distinguishes `handle_stack_cycle`
    /// from `crosstack_member(handoff_direction, advance)`:
    /// `cycle` always mutates `stack_focus`; `crosstack` at
    /// boundary NEVER mutates `stack_focus` (the handoff
    /// early-exits BEFORE the mutating block).
    ///
    /// **Trapdoor avoidance**: the fixture is a pure within-
    /// stack ZStack at root -- NO `split axis=horizontal` or
    /// `split axis=vertical` Split pane -- so the axis-trapdoor
    /// (which only matters for the boundary-handoff path in
    /// `crosstack_member`) cannot confound the assertion.
    /// Cycle's algorithm is closed (no handoff), so any axis
    /// trapdoor in the fixture would only add noise.
    #[test]
    fn pane_stack_cycle_on_one_member_zstack_wraps_to_same_member() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            zstack {
                pane kind=shell label="only"
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        assert_eq!(initial.panes.len(), 1);
        let pane_only = initial.panes[0].clone();
        let id_only = cmdash::derive_layer_id(&pane_only.id);
        let r0 = PaneRunner::spawn_with_graphics(pane_only, id_only, shell, Some(close_tx.clone()))
            .expect("spawn r0");
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            vec![r0],
            bindings,
            0, // focus on "only" (SOLE member)
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        let pre_focus_id = ctx.runners[ctx.focus].computed().id;
        ctx.apply_action_full(KeyAction::PaneStackCycle);
        // Phase 6 cycle boundary pin: (0+1) % 1 == 0 -- focus
        // identity UNCHANGED, but stack_focus is updated.
        assert_eq!(
            ctx.focus, 0,
            "PaneStackCycle on a 1-member ZStack wraps ((0+1)%1==0) -- focus STAYS at 0"
        );
        let post_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(
            post_focus_id, pre_focus_id,
            "PaneStackCycle on a 1-member ZStack must NOT change focus identity"
        );
        assert!(
            ctx.stack_focus.contains_key(&post_focus_id),
            "PaneStackCycle on a 1-member ZStack must update stack_focus (records post-wrap idx 0) -- this distinguishes cycle from crosstack_member which NEVER mutates stack_focus on the handoff path"
        );
        assert_eq!(
            ctx.stack_focus.get(&post_focus_id).copied(),
            Some(0),
            "stack_focus must record post-wrap idx 0"
        );
    }

    /// Phase 6 carry-forward: `PaneStackCycle` on a 3-member
    /// ZStack at the LAST member wraps modulo-style:
    /// `(2 + 1) % 3 == 0` -- the FIRST member. Pin: focus
    /// idx shifts from 2 to 0 (full wrap), `stack_focus`
    /// records (id_a, 0) for the post-wrap focus, AND
    /// post_focus_id.path()[1] == 0 pins the declaration-
    /// order index of the FIRST member (path[0] is the
    /// resolver seed, always 0; path[1] is the meaningful
    /// ZStack-member index per the resolver convention).
    ///
    /// **Trapdoor avoidance**: deliberately a pure within-
    /// stack ZStack at root -- NO `split axis=horizontal`
    /// ANYWHERE -- because cycle's algorithm has no handoff
    /// path. The axis-trapdoor (column vs row split) only
    /// affects `crosstack_member`'s boundary-handoff path;
    /// using axis-trapdoor fixtures for a cycle test would
    /// be a semantic-noose (the fixture's trapdoor would
    /// be irrelevant to cycle's behavior and would invite a
    /// future reader to misinterpret the assertion).
    #[test]
    fn pane_stack_cycle_on_three_member_zstack_wraps_last_to_first() {
        let (close_tx, _close_rx_unused): (PaneCloseTx, _) = mpsc::channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            zstack {
                pane kind=shell label="a"
                pane kind=shell label="b"
                pane kind=shell label="c"
            }
        }"#;
        let cfg = cmdash_config::parse(cfg_text).expect("parse");
        let layout_root = cfg.layout.expect("layout block");
        let initial = ComputedLayout::compute(
            &layout_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute");
        assert_eq!(initial.panes.len(), 3);
        let pane_a = initial.panes[0].clone();
        let pane_b = initial.panes[1].clone();
        let pane_c = initial.panes[2].clone();
        let id_a = cmdash::derive_layer_id(&pane_a.id);
        let id_b = cmdash::derive_layer_id(&pane_b.id);
        let id_c = cmdash::derive_layer_id(&pane_c.id);
        let r0 =
            PaneRunner::spawn_with_graphics(pane_a, id_a, shell.clone(), Some(close_tx.clone()))
                .expect("spawn r0");
        let r1 =
            PaneRunner::spawn_with_graphics(pane_b, id_b, shell.clone(), Some(close_tx.clone()))
                .expect("spawn r1");
        let r2 = PaneRunner::spawn_with_graphics(pane_c, id_c, shell, Some(close_tx.clone()))
            .expect("spawn r2");
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![]);
        let mut ctx = TickContext::new_full(
            vec![r0, r1, r2],
            bindings,
            2, // focus on "c" (LAST member)
            true,
            close_tx,
            _close_rx_unused,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
            ShellSpec::LoginShell,
        );
        let pre_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_ne!(
            pre_focus_id, ctx.runners[0].computed().id,
            "pre-focus sanity: must NOT already be at the FIRST member (otherwise the wrap assertion proves nothing)"
        );
        ctx.apply_action_full(KeyAction::PaneStackCycle);
        // Phase 6 cycle boundary pin: (2+1) % 3 == 0 -- the
        // full wrap from LAST to FIRST member.
        assert_ne!(
            ctx.focus, 2,
            "PaneStackCycle on the LAST member must NOT no-op -- it wraps modulo"
        );
        assert_eq!(
            ctx.focus, 0,
            "PaneStackCycle wraps last (idx 2) -> first (idx 0) via (2+1) % 3"
        );
        let post_focus_id = ctx.runners[ctx.focus].computed().id;
        assert_eq!(
            post_focus_id,
            ctx.runners[0].computed().id,
            "post-focus PaneId must match pane 'a' at pre_order=0 (FIRST member)"
        );
        // Path[1] pin: declaration-order ZStack member index.
        // Path[0] is the resolver seed, always 0 -- NOT a
        // meaningful per-this-test signal.
        assert_eq!(
            post_focus_id.path()[1],
            0,
            "post-focus path[1] must read 0 (declaration-order idx of FIRST member)"
        );
        assert!(
            ctx.stack_focus.contains_key(&post_focus_id),
            "PaneStackCycle wrap must update stack_focus"
        );
        assert_eq!(
            ctx.stack_focus.get(&post_focus_id).copied(),
            Some(0),
            "stack_focus must record post-wrap idx 0 (FIRST member)"
        );
    }
}
