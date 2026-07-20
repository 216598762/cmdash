//! cmdash binary: drives the layout → PTY → ratatui text body and
//! termcompositor kitty graphics render loop, with crossterm input
//! dispatch via cmdash-keybinds.
//!
//! AGENTS.md §"Rendering pipeline" -- phase 3a draws the cell body
//! through ratatui and phase 3b emits termcompositor graphics via
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
//!    so the termcompositor framebuffer stays in-sync.
//!
//! ## Pane drop → termcompositor teardown
//!
//! Each `PaneRunner::Drop` sends its `PaneLayerId` into a
//! shared `mpsc::Sender<PaneLayerId>`. The receiver lives in
//! `cmdash::run` and is drained at the start of each tick so
//! the corresponding `termcompositor` layers are revoked
//! without forcing `GraphicsState` into an `Arc<Mutex<...>>`
//! (which fails `clippy::arc-with-non-send-sync` because
//! `termcompositor::LayerStack` is not `Sync`).

use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use cmdash::graphics::{GraphicsState, Metrics};
use cmdash::pane::{PaneCloseTx, PaneRunner};
use cmdash::protocol::{ConfigReload, LoadedWidget, ServerConfig, WidgetFactories};
// `TabStack` (and `Tab`) are re-exported from the lib crate's
// `tabs` module via `cmdash::TabStack`. main.rs is the binary
// entrypoint; `crate::TabStack` would resolve to the binary
// crate's flat namespace (which does not define `TabStack`),
// not the lib. Use the lib-crate path so the `tabs: TabStack<TabState>`
// field on [`TickContext`] resolves.
use cmdash::frontend_task::FrontendTask;
use cmdash::server_task::ServerTask;
use cmdash::tick_context::{shell_spec_from_command, STATUS_BAR_HEIGHT, TAB_BAR_HEIGHT};
use cmdash_config::{format_errors_with_context, parse_collect, LayoutNode, PaneKind};
use cmdash_keybinds::Router;
use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
use cmdash_pty::ShellSpec;

use notify::Watcher as _;
use ratatui::Terminal;
use tracing::{debug, info, warn};

/// Command-line arguments parsed at binary entry. v1 ships
/// `--log=<path>`, `--config=<path>`, and `--help`.
///
/// `Debug` is derived so `Result<CliArgs, _>` can be used as an
/// assertion expectation type, matching the binary's
/// `cli_args_tests` test infrastructure (the parser is one of
/// the few pieces of v1 surface area with a hand-rolled
/// unit-test target); `Debug` also lets `expect_err` assertions
/// print the parse-error string via `{err:?}`. No other
/// derives are required: `cli_args_tests` compares
/// `cli.log.as_deref()` (`PathBuf`'s `PartialEq`, not `CliArgs`'s)
/// and uses `String::contains(...)` for error-message
/// assertions, not `PartialEq` on `Result<CliArgs, _>`.
#[derive(Debug)]
pub(crate) struct CliArgs {
    /// `--log=<path>` opens a tracing-subscriber file writer at
    /// `path` and silences stdout. v1 dumps at TRACE level in
    /// pretty-printed multi-line indented format (file-only).
    /// Append-mode; the parent directory must exist; a missing or
    /// unreadable path is a startup error so the user notices
    /// immediately rather than chasing phantom debug logs from a
    /// different working directory later.
    pub(crate) log: Option<std::path::PathBuf>,
    /// `--config=<path>` overrides the config file path. When
    /// absent, the resolution chain is:
    /// 1. `$CMDASH_CONFIG_DIR/config.kdl` (env override)
    /// 2. `~/.config/cmdash/config.kdl` (XDG default)
    /// 3. bundled `config.kdl` (`include_str!` fallback)
    pub(crate) config: Option<std::path::PathBuf>,
}

impl CliArgs {
    /// Hand-rolled argv parser; v1 knows `--log=<path>`,
    /// `--config=<path>`, and `--help`. Scan is one-pass over
    /// argv (skipping `argv[0]` = program name); each recognized
    /// flag wins its own slot; the first occurrence of each flag
    /// wins and subsequent ones are noted-but-ignored so launch
    /// scripts that duplicate flags don't break silently.
    ///
    /// Errors fall into 3 buckets (same as v1):
    /// 1. **Bare `--log` / `--config`** (no `=<path>`) is rejected:
    ///    ambiguous between "no value" and "missing value".
    /// 2. **`--log=` / `--config=`** (empty value) is rejected.
    /// 3. **Unrecognized flag** is silently accepted as a
    ///    forward-compat hedge (warned to stderr).
    pub fn parse(argv: &[String]) -> Result<Self, String> {
        let mut log: Option<std::path::PathBuf> = None;
        let mut config: Option<std::path::PathBuf> = None;
        let mut help = false;
        for token in argv.iter().skip(1) {
            if let Some(value) = token.strip_prefix("--log=") {
                if log.is_some() {
                    eprintln!("cmdash: --log=<path> specified more than once; keeping first");
                    continue;
                }
                if value.is_empty() {
                    return Err("--log=<path> requires a non-empty <path> argument".into());
                }
                log = Some(std::path::PathBuf::from(value));
                continue;
            }
            if token == "--log" {
                return Err(
                    "--log=<path> requires an =<path> argument; bare `--log` not accepted".into(),
                );
            }
            if let Some(value) = token.strip_prefix("--config=") {
                if config.is_some() {
                    eprintln!("cmdash: --config=<path> specified more than once; keeping first");
                    continue;
                }
                if value.is_empty() {
                    return Err("--config=<path> requires a non-empty <path> argument".into());
                }
                config = Some(std::path::PathBuf::from(value));
                continue;
            }
            if token == "--config" {
                return Err(
                    "--config=<path> requires an =<path> argument; bare `--config` not accepted"
                        .into(),
                );
            }
            if token == "--help" || token == "-h" {
                help = true;
                continue;
            }
            // Forward-compat hedge: future flags leak through v1's
            // parser without aborting. Error-only-not-warn would
            // force every script to be re-paged against the latest
            // flag catalog; warn is a softer contract.
            if token.starts_with("--") {
                eprintln!("cmdash: warning: ignoring unrecognized flag `{token}` (forward-compat)");
            }
        }
        if help {
            return Err("HELP".into());
        }
        Ok(Self { log, config })
    }
}

/// Initialize the tracing subscriber. Two-mode setup:
///
/// - **`--log=<path>`** ⇒ file-only, TRACE level, pretty-indented
///   multi-line events with target + file + line + thread info.
///   `RUST_LOG` is intentionally IGNORED in this mode because
///   TRACE is what makes a `--log` launch match the user's
///   "all information useful for debugging" target (any filter
///   narrower than TRACE would silently drop event categories the
///   user is asking to see).
/// - **no `--log`** ⇒ stdout, INFO `default` (`RUST_LOG` env overrides),
///   single-line compact. Preserves the prior launch behavior.
///
/// The dual-mode setup keeps the default launch quiet (stdout stays
/// on the existing info-only contract) while letting a debugging
/// session opt into `cmdash --log=/tmp/cmdash-debug.log` without
/// spamming the host terminal (which is busy with kitty graphics
/// output).
///
/// File-mode error policy: a missing or unreadable `<path>` is a
/// STARTUP ERROR (exit code 3) so the user notices immediately
/// rather than capturing zero logs and chasing a phantom failure.
/// The parent directory is NOT auto-created (verifies the user
/// actually wanted the log at that location).
fn init_tracing(log_path: Option<&std::path::Path>) {
    use tracing_subscriber::{fmt, EnvFilter};
    match log_path {
        Some(path) => {
            let file = match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                Ok(f) => f,
                Err(e) => {
                    eprintln!(
                        "cmdash: --log=<{}> could not be opened: {}",
                        path.display(),
                        e,
                    );
                    std::process::exit(3);
                }
            };
            fmt()
                .with_env_filter(EnvFilter::new("trace"))
                .with_writer(file)
                .pretty()
                .with_target(true)
                .with_file(true)
                .with_line_number(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .init();
        }
        None => {
            fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
                )
                .with_target(false)
                .init();
        }
    }
}

/// Print the `--help` banner and exit. Kept as a standalone fn
/// so the `HELP` sentinel from `CliArgs::parse` has a single
/// call site.
fn print_help() {
    eprintln!(concat!(
        "cmdash \u{2014} terminal multiplexer + dashboard",
        "\n",
        "\nUSAGE:",
        "\n  cmdash [OPTIONS]",
        "\n",
        "\nOPTIONS:",
        "\n  --config=<path>   Path to a KDL config file (default: ~/.config/cmdash/config.kdl)",
        "\n  --log=<path>      Write trace-level diagnostics to <path> (stdout is silent)",
        "\n  --help, -h        Print this help message",
        "\n",
        "\nCONFIG RESOLUTION:",
        "\n  1. --config=<path> (explicit CLI override)",
        "\n  2. $CMDASH_CONFIG_DIR/config.kdl (env override)",
        "\n  3. ~/.config/cmdash/config.kdl (XDG default)",
        "\n  4. bundled default (compiled-in fallback)",
    ));
}

/// Resolve the config file path using the priority chain:
/// 1. Explicit `--config=<path>` (already resolved by caller)
/// 2. `$CMDASH_CONFIG_DIR/config.kdl` env override
/// 3. `~/.config/cmdash/config.kdl` XDG default
/// 4. `None` → use bundled fallback
///
/// Returns `(path, source_label)` where `source_label` is a
/// human-readable description for tracing.
fn resolve_config_path(
    cli_config: Option<&std::path::Path>,
) -> (Option<std::path::PathBuf>, &'static str) {
    // Priority 1: explicit CLI override.
    if let Some(path) = cli_config {
        return (Some(path.to_path_buf()), "--config=<path>");
    }
    // Priority 2: $CMDASH_CONFIG_DIR env override.
    if let Ok(dir) = std::env::var("CMDASH_CONFIG_DIR") {
        if !dir.is_empty() {
            let path = std::path::PathBuf::from(dir).join("config.kdl");
            return (Some(path), "$CMDASH_CONFIG_DIR");
        }
    }
    // Priority 3: XDG default (~/.config/cmdash/config.kdl).
    if let Some(home) = std::env::var_os("HOME") {
        let path = std::path::PathBuf::from(home)
            .join(".config")
            .join("cmdash")
            .join("config.kdl");
        return (Some(path), "~/.config/cmdash/config.kdl");
    }
    // Priority 4: bundled fallback.
    (None, "bundled default")
}

/// Read the config text from the resolved path, falling back to
/// the bundled default if the file is missing or unreadable.
/// Returns a `Cow<'static, str>` so the borrowed-bundled path
/// avoids a heap allocation.
fn read_config_text(
    path: Option<&std::path::Path>,
    source_label: &str,
) -> std::borrow::Cow<'static, str> {
    match path {
        Some(p) => match std::fs::read_to_string(p) {
            Ok(text) => {
                info!(
                    path = %p.display(),
                    source = source_label,
                    bytes = text.len(),
                    "config file loaded"
                );
                std::borrow::Cow::Owned(text)
            }
            Err(e) => {
                warn!(
                    path = %p.display(),
                    error = %e,
                    source = source_label,
                    "config file not readable; falling back to bundled default"
                );
                std::borrow::Cow::Borrowed(include_str!("../config.kdl"))
            }
        },
        None => {
            info!(source = source_label, "using bundled default config");
            std::borrow::Cow::Borrowed(include_str!("../config.kdl"))
        }
    }
}

/// Filesystem watcher that monitors the config file's parent
/// directory for changes and sends re-parsed configs to the
/// main tick loop via an mpsc channel.
struct ConfigWatcher {
    _watcher: notify::RecommendedWatcher,
}

impl ConfigWatcher {
    fn spawn(
        config_path: Option<&std::path::Path>,
    ) -> (Option<Self>, Option<UnboundedReceiver<ConfigReload>>) {
        let Some(path) = config_path else {
            return (None, None);
        };
        let path = path.to_path_buf();
        let (tx, rx) = unbounded_channel();
        let watcher = match Self::start_watcher(path, tx) {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e,
                      "config watcher: failed to start; hot-reload disabled");
                return (None, None);
            }
        };
        (Some(watcher), Some(rx))
    }

    fn start_watcher(
        path: std::path::PathBuf,
        tx: UnboundedSender<ConfigReload>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Debounce window: collapse rapid successive Modify events
        // into a single ConfigReload. 500ms is long enough to absorb
        // editor save-temp-rename cycles but short enough to feel
        // instant to the user.
        //
        // Edge case — inotify coalescing on Linux: the kernel can
        // merge two back-to-back writes into a single inotify event,
        // so only one Modify reaches the watcher. This is harmless
        // here (we still get one reload) but tests that assert
        // "exactly one reload from two writes" must insert a small
        // sleep (~50ms) between writes so the kernel delivers them
        // as separate events. See the
        // `config_hot_reload_debounces_rapid_writes` test.
        let debounce_ms: u64 = 500;
        let mut last_reload = std::time::Instant::now() - Duration::from_millis(debounce_ms);
        let watcher_path = path.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else { return };
                if !matches!(event.kind, notify::EventKind::Modify(_)) {
                    return;
                }
                let now = std::time::Instant::now();
                if now.duration_since(last_reload).as_millis() < debounce_ms as u128 {
                    return;
                }
                last_reload = now;
                let text = match std::fs::read_to_string(&watcher_path) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(error = %e,
                              path = %watcher_path.display(),
                              "config watcher: read failed");
                        return;
                    }
                };
                let (cfg, parse_errors) = parse_collect(&text);
                if !parse_errors.is_empty() {
                    let file_label = watcher_path.display().to_string();
                    let formatted =
                        format_errors_with_context(&parse_errors, &text, Some(&file_label));
                    for line in formatted.lines() {
                        warn!(%line, "config watcher: parse error");
                    }
                    return;
                }
                info!(path = %watcher_path.display(),
                      "config file changed; applying hot-reload");
                let _ = tx.send(ConfigReload {
                    keybinds: cfg.keybinds,
                    presets: cfg.presets,
                    layout_root: cfg.layout,
                    status_bar: cfg.status_bar,
                    theme: cfg.theme,
                });
            })?;
        if let Some(parent) = path.parent() {
            watcher
                .watch(parent, notify::RecursiveMode::NonRecursive)
                .map_err(|e| {
                    let b: Box<dyn std::error::Error + Send + Sync> = Box::new(e);
                    b
                })?;
            info!(path = %parent.display(),
                  "config watcher: watching directory");
        } else {
            watcher
                .watch(&path, notify::RecursiveMode::NonRecursive)
                .map_err(|e| {
                    let b: Box<dyn std::error::Error + Send + Sync> = Box::new(e);
                    b
                })?;
        }
        Ok(Self { _watcher: watcher })
    }
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let cli = match CliArgs::parse(&argv) {
        Ok(c) => c,
        Err(e) => {
            if e == "HELP" {
                print_help();
                std::process::exit(0);
            }
            eprintln!("cmdash: {e}");
            std::process::exit(2);
        }
    };
    init_tracing(cli.log.as_deref());
    // Surface a one-line banner to stderr so a backgrounded
    // `cmdash --log=/tmp/x.log &` invocation has evidence the
    // binary is alive BEFORE the file-only subscriber swallows
    // stdout. Per the user's "File-only (silence stdout)" choice,
    // the regular `info!("cmdash starting ...")` line below lands
    // on the file, so without this stderr banner the launch is
    // silent for as long as the file only fires non-startup events.
    if let Some(ref p) = cli.log {
        eprintln!(
            "cmdash: --log=<{}>, file-only subscriber at TRACE level; \
             see that file for diagnostics (stdout is silent by design)",
            p.display(),
        );
    }
    if let Err(e) = run(&cli).await {
        eprintln!("cmdash: fatal: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: &CliArgs) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut caps = cmdash::graphics::TermCapabilities::detect();
    info!(
        graphics = caps.graphics.name(),
        "cmdash starting (ratatui text body + termcompositor graphics)"
    );
    let (config_path, source_label) = resolve_config_path(cli.config.as_deref());
    let cfg_text = read_config_text(config_path.as_deref(), source_label);
    let (cfg, parse_errors) = parse_collect(&cfg_text);
    if !parse_errors.is_empty() {
        let file_label = config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<config>".into());
        let formatted = format_errors_with_context(&parse_errors, &cfg_text, Some(&file_label));
        return Err(formatted.into());
    }
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
    // Compute the total rows reserved for chrome (tab bar +
    // optional status bar). When the status bar is enabled at
    // the top it sits directly below the tab bar; when at the
    // bottom it sits at the last row. Either way, the layout
    // area is reduced by the combined chrome height.
    let status_bar_enabled = cfg.status_bar.as_ref().is_some_and(|sb| sb.enabled);
    let chrome_height = TAB_BAR_HEIGHT
        + if status_bar_enabled {
            STATUS_BAR_HEIGHT
        } else {
            0
        };
    let layout_area = LayoutRect {
        x: 0,
        y: 0,
        w: total.w,
        h: total.h.saturating_sub(chrome_height),
    };
    let layout = ComputedLayout::compute(&layout_root, layout_area)?;
    info!(
        panes = layout.panes.len(),
        cols = layout_area.w,
        rows = layout_area.h,
        tab_bar = TAB_BAR_HEIGHT,
        status_bar = status_bar_enabled,
        "layout resolved"
    );

    // PaneRunner::Drop sends its `PaneLayerId` into this channel;
    // tick_loop drains it at the start of phase 1 to call
    // `GraphicsState::close_pane` for each id. Drop order: the
    // Vec<PaneRunner> drops before `graphics` (reverse
    // declaration order), so Drop-driven sends land on a live
    // receiver owned by `close_rx`.
    let (close_tx, close_rx) = unbounded_channel::<cmdash_pty::PaneLayerId>();

    let mut runners: Vec<PaneRunner> = Vec::with_capacity(layout.panes.len());
    // Load widget factories before spawning panes so widget panes
    // can be instantiated immediately.
    let widget_factories = load_widgets();
    // Derive the environment variables that advertise host
    // terminal capabilities to child PTYs once, then reuse
    // them for every shell pane spawned in the initial layout.
    let pane_env_vars = caps.to_env_vars();
    for pane in &layout.panes {
        let layer_id = cmdash::derive_layer_id(&pane.id);
        let tx: PaneCloseTx = close_tx.clone();
        match &pane.kind {
            PaneKind::Widget { ref_name } => {
                if let Some(factory) = widget_factories.get(ref_name) {
                    let raw =
                        unsafe { (factory.create)(cmdash_widget_sdk::CMDASH_WIDGET_ABI_VERSION) };
                    if raw.is_null() {
                        warn!(name = %ref_name, "widget create returned null (ABI mismatch?)");
                    } else {
                        let widget = unsafe { cmdash_widget_sdk::widget_from_raw(raw) };
                        runners.push(PaneRunner::spawn_widget(
                            pane.clone(),
                            layer_id,
                            widget,
                            Some(tx),
                        ));
                    }
                } else {
                    warn!(name = %ref_name, "widget not found in ~/.config/cmdash/widgets/");
                }
            }
            PaneKind::Script => {
                let cmd = pane.command.as_deref().unwrap_or("");
                match cmdash::script_widget::ScriptWidget::spawn(cmd, pane.label.as_deref()) {
                    Ok(mut widget) => {
                        widget.set_theme(cfg.theme.clone().unwrap_or_default());
                        runners.push(PaneRunner::spawn_widget(
                            pane.clone(),
                            layer_id,
                            Box::new(widget),
                            Some(tx),
                        ));
                    }
                    Err(e) => {
                        warn!(error = %e, command = %cmd, "failed to spawn script widget");
                    }
                }
            }
            PaneKind::Shell => {
                let shell = shell_spec_from_command(&pane.command, &ShellSpec::LoginShell);
                match PaneRunner::spawn_with_graphics_and_env(
                    pane.clone(),
                    layer_id,
                    shell,
                    Some(tx),
                    pane_env_vars.clone(),
                    pane.scrollback_capacity
                        .unwrap_or(cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY),
                ) {
                    Ok(r) => runners.push(r),
                    Err(e) => warn!(error = %e, ?layer_id, "failed to spawn pane"),
                }
            }
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
    // Start the config file watcher for hot-reload.
    let (_config_watcher, config_reload_rx) = ConfigWatcher::spawn(config_path.as_deref());

    // `focus` and `running` are MOVED into
    // `TickContext::new_full` below; they are never mutated
    // locally. `guard` and `ctx` stay `mut` because
    // `guard.as_mut()` and `ctx.run()` both take `&mut self`,
    // and `runners` is `mut` because the initial-frame spawn
    // loop calls `runners.push(r)`.
    let focus: usize = 0;
    let _running = true;

    let mut guard = TerminalGuard::enter()?;

    // DA1 capability probe: if env-var detection yielded
    // TextOnly, send a DEC VT220 Primary Device Attributes
    // query (ESC[c) to detect Sixel support at runtime.
    // Only runs after raw mode is active so the response is
    // byte-oriented (not line-buffered). Skipped when
    // CMDASH_GRAPHICS or TERM already identified a protocol.
    if caps.graphics == cmdash::GraphicsProtocol::TextOnly {
        if let Some(detected) =
            cmdash::GraphicsProtocol::query_device_attributes(Duration::from_millis(100))
        {
            info!(
                protocol = detected.name(),
                "DA1 query detected graphics protocol"
            );
            caps.graphics = detected;
        }
    }

    let graphics = GraphicsState::new_with_caps(Metrics::default(), (total.w, total.h), caps);

    // --- Milestone 1: split into ServerTask + FrontendTask ---
    let (client_tx, client_rx) = unbounded_channel();
    let (server_tx, server_rx) = unbounded_channel();

    let server_config = ServerConfig {
        layout_root,
        presets: cfg.presets,
        shell: ShellSpec::LoginShell,
        status_bar: cfg.status_bar,
        theme: cfg.theme.unwrap_or_default(),
        widget_factories,
    };

    let mut server = ServerTask::new(
        server_config,
        runners,
        focus,
        layout_area,
        cmdash::server_task::ServerChannels {
            close_tx,
            close_rx,
            config_reload_rx,
            client_rx,
            server_tx,
        },
    );

    let mut frontend = FrontendTask::new(guard.as_mut(), graphics, bindings, client_tx, server_rx);

    // Spawn the server in a background tokio task.
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            warn!(error = %e, "server task error");
        }
    });

    // Run the frontend in the current task (borrows terminal).
    let frontend_result = frontend.run().await;

    // Wait for the server to finish.
    if let Err(e) = server_handle.await {
        warn!(error = %e, "server task panicked");
    }

    frontend_result
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

/// Scan `~/.config/cmdash/widgets/<name>/` for shared libraries
/// and load each one via `libloading`. Returns a map of widget name
/// to loaded library + create function. Failures are logged and
/// skipped so a missing widget doesn't prevent cmdash from starting.
fn load_widgets() -> WidgetFactories {
    use std::ffi::c_void;
    let mut factories = WidgetFactories::new();
    let widget_dir = match std::env::var_os("HOME") {
        Some(home) => std::path::PathBuf::from(home)
            .join(".config")
            .join("cmdash")
            .join("widgets"),
        None => return factories,
    };
    if !widget_dir.is_dir() {
        debug!(path = %widget_dir.display(), "widget directory not found; no widgets loaded");
        return factories;
    }
    let entries = match std::fs::read_dir(&widget_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, path = %widget_dir.display(), "cannot read widget directory");
            return factories;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        // Find the first .so / .dll / .dylib in the subdirectory.
        let lib_ext = if cfg!(target_os = "macos") {
            "dylib"
        } else if cfg!(target_os = "windows") {
            "dll"
        } else {
            "so"
        };
        let lib_path = match std::fs::read_dir(&path) {
            Ok(rd) => rd
                .flatten()
                .find(|e| e.path().extension().is_some_and(|ext| ext == lib_ext))
                .map(|e| e.path()),
            Err(_) => None,
        };
        let Some(lib_path) = lib_path else {
            debug!(name, path = %path.display(), "no shared library found in widget directory");
            continue;
        };
        let lib = match unsafe { libloading::Library::new(&lib_path) } {
            Ok(l) => l,
            Err(e) => {
                warn!(error = %e, name, path = %lib_path.display(), "failed to load widget library");
                continue;
            }
        };
        let create_fn: unsafe extern "C" fn(u32) -> *mut c_void =
            match unsafe { lib.get(b"cmdash_widget_create") } {
                Ok(sym) => *sym,
                Err(e) => {
                    warn!(error = %e, name, "widget missing cmdash_widget_create symbol");
                    continue;
                }
            };
        info!(name, path = %lib_path.display(), "widget loaded");
        factories.insert(
            name,
            LoadedWidget {
                _library: lib,
                create: create_fn,
            },
        );
    }
    factories
}

#[cfg(test)]
use cmdash::render::extract_selected_text;
#[cfg(test)]
use cmdash::tick_context::{
    encode_kitty_key_event, event_to_bytes, kitty_event_type, kitty_key_code, kitty_modifiers,
    redacted_event_debug, render_tab_bar, TabState, TickContext,
};
#[cfg(test)]
use cmdash::TabStack;
#[cfg(test)]
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
#[cfg(test)]
use ratatui::style::{Color, Modifier};
/// Per-tab payload carried by every [`Tab<T>`] in the
/// `cmdash::main::TickContext::tabs: TabStack<TabState>` stack.
///
/// ## Tab-state design
///
/// The fields here MIRROR the v1 singular fields on
/// [`TickContext`] (`runners`, `focus`, `layout_root`,
/// `stack_focus`). The v1 fields stay authoritative for v1 code
/// paths (the 100+ call sites that read them directly
/// continue to work); the per-tab payload here is
/// authoritative ONLY for the tab-axis actions
/// (`KeyAction::TabNew` / `TabClose` / `TabSwitch(n)`), which
/// mutate `self.tabs` and then call
/// [`TickContext::sync_v1_from_active_tab`] +
/// [`TickContext::reconcile_runners`] to bring the v1 fields
/// in line with the new active tab.
///
/// ## Why the v1 + tabs duplication
///
/// The additive design avoids changing existing call sites:
/// v1 fields stay (no test fixture changes), the new `tabs`
/// field is added alongside, and the tab actions go through
/// a new code path that syncs the v1 fields from the active
/// tab after every tab mutation.
///
/// ## `Clone` bound for `Tab<T>: Clone`
///
/// The `Tab<T>` derive in `crate::tabs` requires `T: Clone`;
/// in turn, the manual `Clone` impl on [`cmdash::pane::PaneRunner`]
/// returns a "shell" with `pty: None` (the source keeps its
/// pty; the clone has no runtime backend). The
/// `TabState.runners` Vec is therefore a Vec of shells —
/// decorative only; the authoritative runners for v1 code
/// paths are the v1 field's `Vec<PaneRunner>` (real pty +
/// reader thread), reconciled via
/// [`TickContext::reconcile_runners`] after every tab mutation.
// Imports used only by test modules (via `use super::*`).
#[cfg(test)]
use std::collections::BTreeMap;

#[cfg(test)]
mod redacted_event_debug_tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    /// `Char(c)` keystroke: the printable character is
    /// redacted to `Char(<redacted char>)` while modifiers,
    /// kind, and state are preserved. Pins the privacy
    /// redaction so a future crossterm upgrade that changes
    /// the `KeyEvent` field order doesn't silently leak
    /// printable text into `--log=<path>`.
    #[tokio::test]
    async fn char_key_event_redacts_printable_char() {
        let evt = Event::Key(KeyEvent {
            code: KeyCode::Char('Z'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        let s = redacted_event_debug(&evt);
        assert!(
            s.contains("Char(<redacted char>)"),
            "Char must be redacted; got: {s}"
        );
        assert!(
            !s.contains("Char(Z)"),
            "the actual character must not appear as Char(Z) in the redacted output; got: {s}"
        );
        assert!(
            s.contains("CONTROL"),
            "modifiers must be preserved; got: {s}"
        );
        assert!(s.contains("Press"), "kind must be preserved; got: {s}");
    }

    /// `Event::Paste` event: the pasted string content is redacted
    /// to `Paste(<redacted>)`. Pins the reviewer-feedback catch
    /// that clipboard paste events carry typed passwords / API
    /// keys verbatim.
    #[tokio::test]
    async fn paste_event_redacts_content() {
        let evt = Event::Paste("secret-password".into());
        let s = redacted_event_debug(&evt);
        assert!(
            s.contains("Paste(<redacted>)"),
            "Paste content must be redacted; got: {s}"
        );
        assert!(
            !s.contains("secret"),
            "the actual paste content must not appear; got: {s}"
        );
    }

    /// Non-`Char` `KeyCode` variants (arrows, F-keys,
    /// Backspace, etc.) carry no printable text, so the full
    /// `{:?}` escape is forwarded without redaction. Pins the
    /// "KEPT (full Debug escape)" contract for non-printable
    /// key codes.
    #[tokio::test]
    async fn non_char_key_event_passes_through_unredacted() {
        let evt = Event::Key(KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        let s = redacted_event_debug(&evt);
        assert!(
            s.contains("Up"),
            "non-Char KeyCode must pass through unredacted; got: {s}"
        );
        assert!(
            !s.contains("redacted"),
            "non-Char KeyCode must not contain 'redacted'; got: {s}"
        );
    }

    /// `Event::Resize` event carries only geometry — no printable
    /// text — so it passes through unredacted.
    #[tokio::test]
    async fn resize_event_passes_through_unredacted() {
        let evt = Event::Resize(132, 50);
        let s = redacted_event_debug(&evt);
        assert!(
            s.contains("132") && s.contains("50"),
            "Resize must pass through unredacted; got: {s}"
        );
    }
}

#[cfg(test)]
mod cli_args_tests {
    use super::*;
    use cmdash::test_utils::make_isolated_test_dir;

    /// Helper to lift a `&str` literal into a `String` for the
    /// `&[String]` parser signature.
    fn arg(s: &str) -> String {
        s.to_string()
    }

    /// No `--log` token at all: the field stays `None`. The
    /// production launch shape (default stdout subscriber at
    /// INFO level).
    #[tokio::test]
    async fn parse_log_absent_returns_none() {
        let argv = vec![arg("cmdash")];
        let cli = CliArgs::parse(&argv).expect("parse");
        assert!(cli.log.is_none(), "--log absence must yield None");
    }

    /// `--log=/tmp/x.log` is the basic happy path with an
    /// absolute path; resolves relative vs. absolute via
    /// standard `PathBuf` semantics (no rewrite).
    #[tokio::test]
    async fn parse_log_equals_path_is_some() {
        let argv = vec![arg("cmdash"), arg("--log=/tmp/x.log")];
        let cli = CliArgs::parse(&argv).expect("parse");
        assert_eq!(
            cli.log.as_deref(),
            Some(std::path::Path::new("/tmp/x.log")),
            "--log=/tmp/x.log must yield Some(\"/tmp/x.log\")"
        );
    }

    /// `--log=<relative>` preserves the relative segment
    /// verbatim; CWD resolution happens at `OpenOptions::open`
    /// time, not at parse time.
    #[tokio::test]
    async fn parse_log_relative_path_is_some() {
        let argv = vec![arg("cmdash"), arg("--log=debug.log")];
        let cli = CliArgs::parse(&argv).expect("parse");
        assert_eq!(
            cli.log.as_deref(),
            Some(std::path::Path::new("debug.log")),
            "--log=debug.log must yield Some(\"debug.log\")"
        );
    }

    /// `--log=` (empty value) is REJECTED: an empty `PathBuf`
    /// silently trips Rust's "no such file" error downstream
    /// instead of surfacing a clear upfront message.
    #[tokio::test]
    async fn parse_log_empty_value_errors() {
        let argv = vec![arg("cmdash"), arg("--log=")];
        let err = CliArgs::parse(&argv).expect_err("--log= with empty value must error");
        assert!(
            err.contains("--log"),
            "error message must reference --log: {err:?}"
        );
    }

    /// Bare `--log` (no `=<path>`) is REJECTED: ambiguous
    /// between "no log" and "missing value".
    #[tokio::test]
    async fn parse_log_bare_no_equals_errors() {
        let argv = vec![arg("cmdash"), arg("--log")];
        let err = CliArgs::parse(&argv).expect_err("bare --log must error");
        assert!(
            err.contains("=path") || err.contains("=<path>"),
            "error message must point at the =<path> syntax: {err:?}"
        );
    }

    /// First `--log=<path>` wins; subsequent ones warn-and-ignore.
    /// Pin the "first wins" semantic so launch scripts that
    /// accidentally pass two `--log=X --log=Y` don't quietly
    /// retarget the file path mid-run.
    #[tokio::test]
    async fn parse_log_first_wins() {
        let argv = vec![
            arg("cmdash"),
            arg("--log=/tmp/a.log"),
            arg("--log=/tmp/b.log"),
        ];
        let cli = CliArgs::parse(&argv).expect("parse");
        assert_eq!(
            cli.log.as_deref(),
            Some(std::path::Path::new("/tmp/a.log")),
            "first --log=X must win over subsequent --log=Y"
        );
    }

    /// Unknown `--flag` after `--log=...` is silently accepted
    /// (forward-compat hedge with a warn to stderr) so future
    /// flag additions don't break existing launch scripts.
    /// The PARSE SUCCEEDS and the already-set `--log` is
    /// preserved through the unrecognized token.
    #[tokio::test]
    async fn parse_unknown_flag_after_log_is_ignored() {
        let argv = vec![
            arg("cmdash"),
            arg("--log=/tmp/x"),
            arg("--future-flag"),
            arg("--value=42"),
        ];
        let cli = CliArgs::parse(&argv).expect("parse must succeed");
        assert_eq!(
            cli.log.as_deref(),
            Some(std::path::Path::new("/tmp/x")),
            "unknown --future-flag in argv must not invalidate the prior --log=<path>"
        );
    }

    /// Unknown `--flag` alone (no --log) still parses
    /// successfully. Forward-compat hedge: a launcher that adds
    /// a flag in cmdash v2 must NOT break v1 launch scripts.
    #[tokio::test]
    async fn parse_unknown_flag_alone_is_ignored() {
        let argv = vec![arg("cmdash"), arg("--future-flag")];
        let cli = CliArgs::parse(&argv).expect("parse must succeed");
        assert!(cli.log.is_none());
    }

    /// Lone `--log` (no `=<path>`) errors BEFORE scanning
    /// subsequent flags — pin: parser reads left-to-right and
    /// aborts at the first failing token.
    #[tokio::test]
    async fn parse_log_bare_aborts_before_subsequent_flags() {
        let argv = vec![arg("cmdash"), arg("--log"), arg("--future-flag")];
        let err = CliArgs::parse(&argv).expect_err("lone --log must abort");
        assert!(
            err.contains("--log"),
            "error message must mention --log: {err:?}"
        );
    }

    /// Empty argv (`vec![]`) — the parser scans with
    /// `argv.iter().skip(1)` which yields nothing; the result is
    /// `Ok(Self { log: None })`. Pin this shape in case a future
    /// refactor accidentally panics on `skip(1)` of an empty slice.
    #[tokio::test]
    async fn parse_empty_argv_returns_none() {
        let argv: Vec<String> = vec![];
        let cli = CliArgs::parse(&argv).expect("parse");
        assert!(
            cli.log.is_none(),
            "empty argv must yield None (skip(1) is a no-op on empty)"
        );
    }

    /// `--log=/foo --log=` (valid first, empty second) — the
    /// first-wins semantic reaches all the way through invalid
    /// second tokens; the empty check is short-circuited by the
    /// "log already set" warn-and-continue. Pin: second invalid
    /// --log does NOT abort when a valid first exists; only when
    /// the FIRST --log is itself empty/bare does the parse error
    /// out.
    #[tokio::test]
    async fn parse_log_valid_then_empty_keeps_first() {
        let argv = vec![arg("cmdash"), arg("--log=/tmp/foo.log"), arg("--log=")];
        let cli = CliArgs::parse(&argv).expect(
            "valid --log=/foo then invalid --log= must keep the first \
             (warn-and-continue, NOT abort)",
        );
        assert_eq!(
            cli.log.as_deref(),
            Some(std::path::Path::new("/tmp/foo.log")),
            "first valid --log=X must win; --log= after must warn-and-continue"
        );
    }

    // ==========================================================
    // --config=<path> parsing tests.
    // ==========================================================

    /// `--config=/tmp/custom.kdl` is the basic happy path.
    #[tokio::test]
    async fn parse_config_equals_path_is_some() {
        let argv = vec![arg("cmdash"), arg("--config=/tmp/custom.kdl")];
        let cli = CliArgs::parse(&argv).expect("parse");
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/custom.kdl")),
            "--config=/tmp/custom.kdl must yield Some(\"/tmp/custom.kdl\")"
        );
    }

    /// `--config=` (empty value) is REJECTED.
    #[tokio::test]
    async fn parse_config_empty_value_errors() {
        let argv = vec![arg("cmdash"), arg("--config=")];
        let err = CliArgs::parse(&argv).expect_err("--config= with empty value must error");
        assert!(
            err.contains("--config"),
            "error message must reference --config: {err:?}"
        );
    }

    /// Bare `--config` (no `=<path>`) is REJECTED.
    #[tokio::test]
    async fn parse_config_bare_no_equals_errors() {
        let argv = vec![arg("cmdash"), arg("--config")];
        let err = CliArgs::parse(&argv).expect_err("bare --config must error");
        assert!(
            err.contains("=path") || err.contains("=<path>"),
            "error message must point at the =<path> syntax: {err:?}"
        );
    }

    /// First `--config=<path>` wins; subsequent ones warn-and-ignore.
    #[tokio::test]
    async fn parse_config_first_wins() {
        let argv = vec![
            arg("cmdash"),
            arg("--config=/tmp/a.kdl"),
            arg("--config=/tmp/b.kdl"),
        ];
        let cli = CliArgs::parse(&argv).expect("parse");
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/a.kdl")),
            "first --config=X must win over subsequent --config=Y"
        );
    }

    /// `--config` absent: the field stays `None`.
    #[tokio::test]
    async fn parse_config_absent_returns_none() {
        let argv = vec![arg("cmdash")];
        let cli = CliArgs::parse(&argv).expect("parse");
        assert!(cli.config.is_none(), "--config absence must yield None");
    }

    /// Both `--log` and `--config` can be set independently.
    #[tokio::test]
    async fn parse_log_and_config_both_set() {
        let argv = vec![
            arg("cmdash"),
            arg("--log=/tmp/debug.log"),
            arg("--config=/tmp/custom.kdl"),
        ];
        let cli = CliArgs::parse(&argv).expect("parse");
        assert_eq!(
            cli.log.as_deref(),
            Some(std::path::Path::new("/tmp/debug.log")),
        );
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/custom.kdl")),
        );
    }

    // ==========================================================
    // --help / -h parsing tests.
    // ==========================================================

    /// `--help` returns the HELP sentinel.
    #[tokio::test]
    async fn parse_help_returns_help_sentinel() {
        let argv = vec![arg("cmdash"), arg("--help")];
        let err = CliArgs::parse(&argv).expect_err("--help must return HELP sentinel");
        assert_eq!(err, "HELP", "--help must return the HELP sentinel");
    }

    /// `-h` returns the HELP sentinel.
    #[tokio::test]
    async fn parse_h_returns_help_sentinel() {
        let argv = vec![arg("cmdash"), arg("-h")];
        let err = CliArgs::parse(&argv).expect_err("-h must return HELP sentinel");
        assert_eq!(err, "HELP", "-h must return the HELP sentinel");
    }

    // ==========================================================
    // resolve_config_path tests.
    //
    // These test the resolution priority chain. Environment
    // variable tests use a unique prefix to avoid collisions
    // with other tests running in parallel.
    // ==========================================================

    /// Priority 1: explicit CLI override wins over everything.
    #[tokio::test]
    async fn resolve_config_path_cli_override_wins() {
        let explicit = Some(std::path::Path::new("/tmp/explicit.kdl"));
        let (path, label) = resolve_config_path(explicit);
        assert_eq!(
            path.as_deref(),
            Some(std::path::Path::new("/tmp/explicit.kdl"))
        );
        assert_eq!(label, "--config=<path>");
    }

    /// Priority 3: XDG default (~/.config/cmdash/config.kdl)
    /// is returned when no CLI override and no env var.
    /// We can't easily clear `CMDASH_CONFIG_DIR` in a test
    /// (parallel tests share the process env), so we test
    /// the CLI-override path which is the priority-1 winner.
    #[tokio::test]
    async fn resolve_config_path_xdg_default_shape() {
        // With no CLI override, the result should be
        // Some(<path>) with the XDG default shape, unless
        // CMDASH_CONFIG_DIR is set (which may happen in CI).
        // We only verify the return shape, not the exact path.
        let (path, label) = resolve_config_path(None);
        // label is either "$CMDASH_CONFIG_DIR" or "~/.config/cmdash/config.kdl"
        // depending on env. Both are valid.
        assert!(
            label == "$CMDASH_CONFIG_DIR"
                || label == "~/.config/cmdash/config.kdl"
                || label == "bundled default",
            "unexpected label: {label}"
        );
        // If not bundled fallback, path must be Some.
        if label != "bundled default" {
            assert!(path.is_some(), "non-bundled label must have a path");
        }
    }

    /// End-to-end integration test: read a config file from disk
    /// via `read_config_text` + `resolve_config_path` and verify
    /// the parsed config produces the expected layout. Uses a
    /// temp file with a two-pane split (distinct from the bundled
    /// single-pane default) so we can confirm the file-sourced
    /// text is actually being parsed.
    #[tokio::test]
    async fn config_file_loading_end_to_end() {
        use std::io::Write;
        let dir = make_isolated_test_dir("cmdash_config_test");
        let path = dir.join("config.kdl");
        let kdl_src = r#"layout {
            split axis=horizontal ratio=0.7 {
                pane kind=shell label="editor"
                pane kind=shell label="terminal"
            }
        }
        keybinds {
            bind "alt-w" action="pane.close"
            bind "alt-q" action="app.close"
        }
        "#;
        let mut f = std::fs::File::create(&path).expect("create temp config");
        f.write_all(kdl_src.as_bytes()).expect("write config");
        drop(f);

        // Simulate the resolution chain with explicit --config path.
        let (resolved_path, label) = resolve_config_path(Some(&path));
        assert_eq!(label, "--config=<path>");
        assert_eq!(resolved_path.as_deref(), Some(path.as_path()));

        let cfg_text = read_config_text(resolved_path.as_deref(), label);
        let cfg = cmdash_config::parse(&cfg_text).expect("parse config from file");
        let layout_root = cfg.layout.expect("layout block present");
        match layout_root {
            cmdash_config::LayoutNode::Split {
                axis,
                ratio,
                children,
            } => {
                assert_eq!(axis, cmdash_config::SplitAxis::Horizontal);
                assert_eq!(ratio, cmdash_config::Ratio(70));
                assert_eq!(children.len(), 2, "split must have 2 children");
            }
            other => panic!(
                "expected Split layout from file, got: {:?}",
                std::mem::discriminant(&other)
            ),
        }
        assert_eq!(cfg.keybinds.len(), 2, "2 keybinds in file");
    }

    /// End-to-end integration test: missing config file falls
    /// back to the bundled default (single-pane layout). Confirms
    /// that `read_config_text` returns valid KDL when the resolved
    /// path doesn't exist on disk.
    #[tokio::test]
    async fn config_file_missing_falls_back_to_bundled() {
        let nonexistent = std::path::PathBuf::from("/tmp/cmdash_nonexistent_config.kdl");
        let _ = std::fs::remove_file(&nonexistent); // ensure it doesn't exist

        let (path, label) = resolve_config_path(Some(&nonexistent));
        assert_eq!(path.as_deref(), Some(nonexistent.as_path()));

        let cfg_text = read_config_text(path.as_deref(), label);
        let cfg = cmdash_config::parse(&cfg_text).expect("bundled fallback must parse");
        let layout_root = cfg.layout.expect("layout block present");
        // Bundled default is a single pane.
        match layout_root {
            cmdash_config::LayoutNode::Pane(p) => {
                assert_eq!(
                    p.label.as_deref(),
                    Some("default"),
                    "bundled default has label 'default'"
                );
            }
            other => panic!(
                "expected Pane (bundled default), got: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// End-to-end: `--config=<path>` CLI override wires through
    /// `CliArgs::parse` -> `resolve_config_path` ->
    /// `read_config_text` -> `cmdash_config::parse` ->
    /// `ComputedLayout::compute`. Writes a custom 2-pane split
    /// config to a temp file, invokes the full resolution chain
    /// as `cmdash::run` would, and asserts the resolved layout
    /// has exactly 2 panes with the expected labels and split
    /// geometry.
    #[tokio::test]
    async fn cli_config_override_end_to_end() {
        let dir = make_isolated_test_dir("cmdash_cli_config_e2e_test");
        let config_path = dir.join("config.kdl");
        let custom_config = r#"
            layout {
                split axis=horizontal ratio=0.6 {
                    pane kind=shell label="editor"
                    pane kind=shell label="terminal"
                }
            }
            keybinds {
                bind "alt-w"  action="pane.close"
                bind "alt-q"  action="app.close"
            }
        "#;
        std::fs::write(&config_path, custom_config).expect("write temp config");

        // Step 1: CliArgs::parse recognizes --config=<path>.
        let argv = vec![
            "cmdash".to_string(),
            format!("--config={}", config_path.display()),
        ];
        let cli = CliArgs::parse(&argv).expect("parse --config flag");
        assert_eq!(
            cli.config.as_deref(),
            Some(config_path.as_path()),
            "CliArgs must capture --config=<path>"
        );

        // Step 2: resolve_config_path returns the CLI override.
        let (resolved_path, label) = resolve_config_path(cli.config.as_deref());
        assert_eq!(
            resolved_path.as_deref(),
            Some(config_path.as_path()),
            "resolve_config_path must return the CLI override path"
        );
        assert_eq!(
            label, "--config=<path>",
            "source label must be --config=<path>"
        );

        // Step 3: read_config_text reads the file contents.
        let cfg_text = read_config_text(resolved_path.as_deref(), label);
        assert!(
            cfg_text.contains("editor"),
            "cfg_text must contain the custom config content"
        );

        // Step 4: cmdash_config::parse parses the KDL.
        let cfg = cmdash_config::parse(&cfg_text).expect("parse custom config");
        let layout_root = cfg.layout.expect("custom config must have layout");

        // Step 5: Verify keybinds round-trip.
        assert_eq!(cfg.keybinds.len(), 2, "custom config must have 2 keybinds");

        // Step 6: ComputedLayout::compute resolves the layout.
        let area = cmdash_layout::Rect {
            x: 0,
            y: 0,
            w: 120,
            h: 40,
        };
        let layout = cmdash_layout::ComputedLayout::compute(&layout_root, area)
            .expect("compute custom layout");
        assert_eq!(
            layout.panes.len(),
            2,
            "custom 2-pane split must resolve to 2 panes"
        );
        assert_eq!(
            layout.panes[0].label.as_deref(),
            Some("editor"),
            "pane 0 must have label 'editor'"
        );
        assert_eq!(
            layout.panes[1].label.as_deref(),
            Some("terminal"),
            "pane 1 must have label 'terminal'"
        );
        // Ratio 0.6 over 120 cols: left = 72, right = 48.
        assert_eq!(
            layout.panes[0].rect.w, 72,
            "editor pane width must be 72 (60% of 120)"
        );
        assert_eq!(
            layout.panes[1].rect.w, 48,
            "terminal pane width must be 48 (40% of 120)"
        );
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
    use cmdash::graphics::{GraphicsProtocol, TermCapabilities};
    use cmdash_layout::ComputedPane;
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

    /// Spawn a single `PaneRunner` wired to a close-channel and
    /// using `/bin/true` (fast-exit child) so `Drop::drop`
    /// rejoins the reader thread promptly in tests.
    ///
    /// `#[allow(dead_code)]` because the `should_panic` tests
    /// use `make_runner_with_id` (the sibling helper below)
    /// and a future test author may want a single-pane
    /// `make_runner` shortcut.
    #[allow(dead_code)]
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
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
    /// tests that don't exercise the `layout`::`compute` path.
    /// Keeping it tiny avoids hitting `MAX_TREE_DEPTH` on
    /// out-of-band nesting during negative-test setup.
    fn dummy_layout_root() -> LayoutNode {
        LayoutNode::Pane(CfgPane {
            kind: PaneKind::Shell,
            label: None,
            command: None,
            scrollback_capacity: None,
        })
    }

    /// Test-side fixture builder for the
    /// 32+ `apply_action_full`-driven tests in this mod.
    /// Replaces ~25 lines of repeated boilerplate
    /// (KDL parse + `ComputedLayout::compute` +
    /// `for pane in layout.panes { spawn_with_graphics }` +
    /// `TestBackend` + `Terminal` + `GraphicsState` +
    /// 14-arg `TickContext::new_full`) with a single 6-arg
    /// call. Returns `(ctx, layout_root, last_area)` so tests
    /// can drive post-dispatch layout assertions (e.g.
    /// `relayout` test re-computes `pre_layout` against the
    /// pre-dispatch `layout_root` + `last_area`).
    ///
    /// The 2 `should_panic` tests cannot use this helper:
    /// they need `runners: Vec::new()` to trigger the
    /// `focus < runners.len()` assert, and this helper
    /// always spawns at least one runner from the parsed
    /// KDL. Those tests keep their manual
    /// `TickContext::new_full` construction.
    ///
    /// `pending_resize` is hardcoded to `None`; the relayout
    /// test, which needs `Some((132, 50))`, mutates
    /// `ctx.pending_resize` after the call (the field is
    /// private to `fn main`'s module but accessible to
    /// `input_tests` via the descendant-mod rule).
    ///
    /// Pre-dispatch `PaneLayerId` capture: tests that need
    /// `dropped_layer_id` / `survivor_layer_id` BEFORE the
    /// dispatch read `ctx.runners[i].layer_id()` directly
    /// after the helper returns (the field is private but
    /// accessible to the descendant `input_tests` mod).
    /// AGENTS.md "minimal API surface" rule says no
    /// fixture-side `Opts` struct; the 6-arg flat signature
    /// is the agreed helper shape.
    pub(crate) fn setup_fixture_ctx<'a>(
        kdl: &str,
        focus: usize,
        bindings: Router,
        shell: ShellSpec,
        last_area: LayoutRect,
        terminal: &'a mut ratatui::Terminal<ratatui::backend::TestBackend>,
    ) -> (
        TickContext<'a, ratatui::backend::TestBackend>,
        LayoutNode,
        LayoutRect,
    ) {
        setup_fixture_ctx_with_runner(
            kdl,
            focus,
            bindings,
            |pane, layer_id, close_tx| {
                PaneRunner::spawn_with_graphics(
                    pane,
                    layer_id,
                    shell.clone(),
                    Some(close_tx),
                    cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
                )
                .expect("setup_fixture_ctx: spawn pane")
            },
            last_area,
            terminal,
        )
    }

    /// Variant of [`setup_fixture_ctx`] that accepts a custom runner
    /// factory. Used by tests that need stub PTY runners instead of
    /// spawning real child processes.
    pub(crate) fn setup_fixture_ctx_with_runner<'a>(
        kdl: &str,
        focus: usize,
        bindings: Router,
        mut make_runner: impl FnMut(ComputedPane, PaneLayerId, PaneCloseTx) -> PaneRunner,
        last_area: LayoutRect,
        terminal: &'a mut ratatui::Terminal<ratatui::backend::TestBackend>,
    ) -> (
        TickContext<'a, ratatui::backend::TestBackend>,
        LayoutNode,
        LayoutRect,
    ) {
        let cfg = cmdash_config::parse(kdl).expect("setup_fixture_ctx_with_runner: parse KDL");
        let layout_root = cfg
            .layout
            .expect("setup_fixture_ctx_with_runner: layout block");
        let layout = ComputedLayout::compute(&layout_root, last_area)
            .expect("setup_fixture_ctx_with_runner: compute");
        let (close_tx, close_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut runners: Vec<PaneRunner> = Vec::with_capacity(layout.panes.len());
        for pane in &layout.panes {
            let tx_clone = close_tx.clone();
            let layer_id = cmdash::derive_layer_id(&pane.id);
            let runner = make_runner(pane.clone(), layer_id, tx_clone);
            runners.push(runner);
        }
        let graphics = GraphicsState::new_with_caps(
            cmdash::graphics::Metrics::default(),
            (last_area.w, last_area.h),
            TermCapabilities {
                graphics: GraphicsProtocol::Kitty,
                kitty_keyboard: true,
                focus_events: true,
                bracketed_paste: true,
                true_color: true,
                color_256: true,
                queries: true,
            },
        );
        let ctx = TickContext::new_full(
            runners,
            bindings,
            focus,
            true,
            close_tx,
            close_rx,
            graphics,
            terminal,
            std::time::Duration::from_millis(33),
            layout_root.clone(),
            None,
            last_area,
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
            ShellSpec::LoginShell,
            None,
        );
        (ctx, layout_root, last_area)
    }

    /// Ctrl-W on a 2-pane Vec, routed through the production
    /// `TickContext::handle_event_full` -> `apply_action_full`
    /// pipeline. Vec shrinks by one, the survivor is unmoved,
    /// the close-channel receives the dropped pane's
    /// `PaneLayerId`, and `graphics.close_pane` drains the
    /// matching image registration.
    ///
    /// The free-fn `handle_event` + `apply_action` pair was
    /// removed in this atom; test and production now share the
    /// same dispatch + reconcile surface end-to-end (AGENTS.md
    /// Phase 2 dual-location contract).
    #[tokio::test]
    async fn ctrl_w_pane_close_pops_focused_runner_and_routes_close_message() {
        // Split layout_root with 2 leaves so the focused
        // pane's resolver path_len >= 2 (closing a direct
        // child of `layout_root` triggers the v2
        // `close_focused_and_rebalance` "binary quits"
        // short-circuit). The runners are spawned FROM this
        // layout_root so their `pane.id`s align with the ctx
        // `layout_root`'s panes for reconcile-by-label.
        let source = r#"layout {
            split axis=horizontal ratio=0.5 {
                pane kind=shell label="a"
                pane kind=shell label="b"
            }
        }"#;
        let last_area = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers {
                ctrl: true,
                ..CfgModifiers::default()
            },
            key: KeyToken::Char('w'),
            action: KeyAction::PaneClose,
        }]);

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");

        let (mut ctx, _layout_root, _last_area) = setup_fixture_ctx(
            source,
            0, // focus on the left leaf
            bindings,
            ShellSpec::Command {
                argv: vec!["true".to_string()],
            },
            last_area,
            &mut terminal,
        );
        assert_eq!(ctx.runners.len(), 2, "split must produce 2 panes");

        // Left leaf (focus 0); its `LayerId` will be the one
        // routed into close_rx on dispatch.
        let dropped_layer_id = ctx.runners[0].layer_id();
        let survivor_layer_id = ctx.runners[1].layer_id();

        // Pre-register one image for the focused pane so we
        // can prove `close_pane` revokes it on drain, matching
        // the production LayerStack revoking flow.
        ctx.graphics
            .push_image(dropped_layer_id, 1, image::RgbaImage::new(1, 1));
        assert!(ctx.graphics.has_image(dropped_layer_id, 1));

        // Dispatch Ctrl-W through the production
        // `TickContext::handle_event_full` path: Router
        // dispatch -> KeyAction::PaneClose -> apply_action_full
        // -> close_focused_and_rebalance -> reconcile_runners.
        ctx.handle_event_full(&key_event(
            crossterm::event::KeyCode::Char('w'),
            crossterm::event::KeyModifiers::CONTROL,
        ));

        // 1) Vec shrank by one, the survivor is the original
        //    r1 (its PaneLayerId matches), and one open pane
        //    does not quit the binary.
        assert_eq!(ctx.runners.len(), 1);
        assert!(ctx.running, "closing one pane must not stop the binary");
        assert_eq!(ctx.runners[0].layer_id(), survivor_layer_id);

        // 2) Focus stays valid (still 0 since 0 < 1).
        assert_eq!(ctx.focus(), 0);

        // 3) `Drop::drop` enqueued the closing pane's id onto
        //    the close-channel the binary's main loop drains
        //    (now exposed via ctx.close_rx).
        let received = ctx
            .close_rx
            .try_recv()
            .expect("PaneRunner::Drop must enqueue the closing pane's layer id");
        assert_eq!(received, dropped_layer_id);

        // 4) Simulating phase 1 -- drain + close_pane --
        //    revokes the termcompositor image registration.
        ctx.graphics.close_pane(received);
        assert!(!ctx.graphics.has_image(dropped_layer_id, 1));
    }

    /// Closing the last surviving pane routed through the
    /// production `TickContext::handle_event_full` path flips
    /// `running` to `false` and quits the binary. Single-leaf
    /// dummy layout: the focused leaf IS the root (resolver
    /// path len == 1), so `close_focused_and_rebalance`
    /// follows the "binary quits" branch (calls
    /// `self.runners.clear()` and `self.running = false`). The
    /// free-fn `handle_event` form is gone in v2; the only
    /// live dispatch site is `TickContext::handle_event_full`.
    #[tokio::test]
    async fn pane_close_last_pane_quits_binary() {
        let source = r#"layout { pane kind=shell label="only" }"#;
        let last_area = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('w'),
            action: KeyAction::PaneClose,
        }]);

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");

        let (mut ctx, _layout_root, _last_area) = setup_fixture_ctx(
            source,
            0, // focus on the only leaf
            bindings,
            ShellSpec::Command {
                argv: vec!["true".to_string()],
            },
            last_area,
            &mut terminal,
        );
        let dropped_layer_id = ctx.runners[0].layer_id();

        ctx.handle_event_full(&key_event(
            crossterm::event::KeyCode::Char('w'),
            crossterm::event::KeyModifiers::NONE,
        ));

        assert!(ctx.runners.is_empty());
        assert!(!ctx.running, "closing the final pane must quit the binary");
        let received = ctx
            .close_rx
            .try_recv()
            .expect("closing the only pane must enqueue the close message");
        assert_eq!(received, dropped_layer_id);
    }

    /// Removing the focused pane when it is the TAIL of a
    /// Vec drives `close_focused_and_rebalance` ->
    /// `reconcile_runners(InPlace)`; the post-rebalance focus
    /// index must clamp to the new last index so the
    /// `runners.get_mut(*focus)` PTY-write path cannot index
    /// out of bounds in subsequent ticks. Routed through the
    /// production `TickContext::handle_event_full` path
    /// with a 3-pane Split layout (a on top, b+c on the
    /// bottom nested split), focusing the tail pane (c).
    #[tokio::test]
    async fn pane_close_clamps_focus_when_tail_removed() {
        let source = r#"layout {
            split axis=vertical ratio=0.5 {
                pane kind=shell label="a"
                split axis=vertical ratio=0.5 {
                    pane kind=shell label="b"
                    pane kind=shell label="c"
                }
            }
        }"#;
        let last_area = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('w'),
            action: KeyAction::PaneClose,
        }]);

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");

        let (mut ctx, _layout_root, _last_area) = setup_fixture_ctx(
            source,
            2, // focus on the tail pane (c)
            bindings,
            ShellSpec::Command {
                argv: vec!["true".to_string()],
            },
            last_area,
            &mut terminal,
        );
        assert_eq!(ctx.runners.len(), 3, "3-pane split must produce 3 panes");
        // Resolver pre-order: pane_a -> pane_b -> pane_c;
        // focus on the tail = pane_c (idx 2).
        let survivor_a = ctx.runners[0].layer_id();
        let survivor_b = ctx.runners[1].layer_id();
        let dropped_layer_id = ctx.runners[2].layer_id();
        assert_ne!(ctx.runners[0].layer_id(), ctx.runners[1].layer_id());
        assert_ne!(ctx.runners[1].layer_id(), ctx.runners[2].layer_id());

        // Drive through the production dispatch path:
        // handle_event_full -> Router::dispatch -> PaneClose
        // -> apply_action_full -> close_focused_and_rebalance
        // -> reconcile_runners(InPlace).
        ctx.handle_event_full(&key_event(
            crossterm::event::KeyCode::Char('w'),
            crossterm::event::KeyModifiers::NONE,
        ));

        assert_eq!(ctx.runners.len(), 2);
        assert!(ctx.running);
        // c was removed at idx 2 → focus clamps from 2 -> 1.
        assert_eq!(ctx.focus(), 1, "removing the tail must clamp focus");
        // Survivors must stay at positions 0 and 1 by
        // `PaneLayerId` (not just by Vec index); the dropped
        // runner was at idx 2.
        assert_eq!(ctx.runners[0].layer_id(), survivor_a);
        assert_eq!(ctx.runners[1].layer_id(), survivor_b);
        assert_ne!(ctx.runners[1].layer_id(), dropped_layer_id);
    }

    /// Building a [`TickContext`] with `focus >= runners.len()`
    /// must panic with a `focus` keyword in the message, so a
    /// caller passing a stale `focus` after a panic-driven
    /// re-construction cannot silently index past the runner
    /// Vec. Locks the AGENTS.md "every invariant needs a
    /// regression test" rule for the focus invariant.
    /// Uses a `ratatui::backend::TestBackend` to construct a
    /// real `Terminal` without writing to stdout.
    /// Migrated to `TickContext::new_full` (was `new`);
    /// the focus-bound panic invariant is enforced at the
    /// 14-arg ctor.
    #[tokio::test]
    #[should_panic(expected = "focus")]
    async fn tick_context_new_full_panics_when_focus_out_of_bounds() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
        let bindings = Router::new(vec![]);
        let graphics =
            cmdash::graphics::GraphicsState::new(cmdash::graphics::Metrics::default(), (80, 24));
        // Empty runners + focus=0 -> 0 < 0 is false -> assert! fires.
        let _ctx = TickContext::new_full(
            Vec::<PaneRunner>::new(),
            bindings,
            0,
            true,
            close_tx,
            close_rx,
            graphics,
            &mut terminal,
            std::time::Duration::from_millis(33),
            dummy_layout_root(),
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
            None,
        );
    }

    /// Companion to the empty-Vec test above: locks the
    /// strict-less-than semantics across the non-zero boundary.
    /// focus=2 + 2 panes -> 2 < 2 is false -> assert! fires.
    /// Catches a future regression that swaps `<` for `<=`
    /// (would accept focus == len and silently index past the
    /// Vec on the next `runners.get_mut(*focus)` call). Uses
    /// `make_runner_with_id` so each pane has a distinct
    /// `PaneLayerId` independent of layout-pre-order numbering.
    /// Migrated to `TickContext::new_full` for the same reason
    /// as the empty-Vec companion.
    #[tokio::test]
    #[should_panic(expected = "focus")]
    async fn tick_context_new_full_panics_when_focus_equals_non_zero_len() {
        let (close_tx, close_rx) = tokio::sync::mpsc::unbounded_channel();
        let r0 = make_runner_with_id("a", PaneLayerId(1), close_tx.clone());
        let r1 = make_runner_with_id("b", PaneLayerId(2), close_tx.clone());
        let bindings = Router::new(vec![]);
        let graphics =
            cmdash::graphics::GraphicsState::new(cmdash::graphics::Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let _ctx = TickContext::new_full(
            vec![r0, r1],
            bindings,
            2, // focus == runners.len()
            true,
            close_tx,
            close_rx,
            graphics,
            &mut terminal,
            std::time::Duration::from_millis(33),
            dummy_layout_root(),
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
            None,
        );
    }

    /// Phase 2 v2 wiring regression: a crossterm
    /// `Event::Resize(w, h)` synthesised at the
    /// `TickContext::handle_event_full` boundary must land
    /// in `pending_resize` so the top of the next tick drives
    /// `relayout(w, h)`. Splits the assertion into the two
    /// smallest claims the bug surface allows: (1) the
    /// option transitions from `None` -> `Some((w, h))`, (2)
    /// subsequent resize signals coalesce (overwrite, NOT
    /// push) so rapid SIGWINCH bursts collapse to the LATEST
    /// dims. Migrated from the prior free-fn form so the
    /// test exercises the same dispatch that production
    /// drives.
    #[tokio::test]
    async fn handle_event_resize_event_arms_pending_resize() {
        let source = r#"layout { pane kind=shell label="resize-anchor" }"#;
        let last_area = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let bindings = Router::new(vec![]);

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");

        let (mut ctx, _layout_root, _last_area) = setup_fixture_ctx(
            source,
            0, // focus on the only leaf
            bindings,
            ShellSpec::Command {
                argv: vec!["true".to_string()],
            },
            last_area,
            &mut terminal,
        );

        // Phase 0.5 starts at `pending_resize == None`. After
        // dispatch the field must transition to Some with the
        // dispatched dims.
        ctx.handle_event_full(&Event::Resize(132, 50));
        assert_eq!(
            ctx.pending_resize,
            Some((132, 50)),
            "Event::Resize must arm pending_resize for phase 0.5 relayout"
        );

        // Coalesce-on-overwrite: a second resize arrives
        // BEFORE phase 0.5 has taken the first queued tuple,
        // so the value should simply be replaced, not stacked.
        ctx.handle_event_full(&Event::Resize(200, 60));
        assert_eq!(
            ctx.pending_resize,
            Some((200, 60)),
            "second Event::Resize must coalesce onto (NOT push past) the first"
        );
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
    /// `for_id` `and` `for_cells` invariants without depending on a
    /// real PTY round-trip.
    #[tokio::test]
    async fn relayout_emits_resize_per_pane_when_host_signals_resize() {
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
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
        let r0 = PaneRunner::spawn_with_graphics(
            pane_a,
            id_a,
            shell.clone(),
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn runner A");
        let r1 = PaneRunner::spawn_with_graphics(
            pane_b,
            id_b,
            shell,
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn runner B");
        let runners = vec![r0, r1];
        let bindings = Router::new(vec![]);
        let graphics =
            cmdash::graphics::GraphicsState::new(cmdash::graphics::Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(132, 50);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");

        let mut ctx = TickContext::new_full(
            runners,
            bindings,
            0,
            true,
            close_tx,
            close_rx,
            graphics,
            &mut terminal,
            std::time::Duration::from_millis(33),
            layout_root.clone(),
            Some((132, 50)),
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
            ShellSpec::LoginShell,
            None,
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
                h: 49
            },
            "child A post-relayout rect must match 132x50 Horizontal-60 split"
        );
        assert_eq!(
            ctx.runners[1].computed().rect,
            LayoutRect {
                x: 79,
                y: 0,
                w: 53,
                h: 49
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
                h: 49,
            },
        )
        .expect("compute post-layout");
        assert_eq!(ctx.runners[0].computed().id, post_layout.panes[0].id);
        assert_eq!(ctx.runners[1].computed().id, post_layout.panes[1].id);

        // GraphicsState cells propagated to the new dims --
        // termcompositor framebuffer pixel composition must
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

    /// `AppNewPane` against a single-leaf `TickContext`: the
    /// focused leaf becomes child 0 of a fresh Horizontal
    /// Split (ratio 50), a new leaf spawn at child 1, and
    /// `reconcile_runners` brings `Vec<PaneRunner>` to length
    /// 2. The original focused pane's `LayerId` is stable per
    /// AGENTS.md Hard rule (a `LayerId` is bound to a pane
    /// instance for its whole lifetime and is NOT re-bound).
    #[tokio::test]
    async fn app_new_pane_splits_focused_leaf_and_spawns_runner() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
        let runner = PaneRunner::spawn_with_graphics(
            pane,
            original_layer,
            shell,
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
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
            close_rx,
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
            None,
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
    #[tokio::test]
    async fn pane_focus_right_resolves_to_adjacent_pane_via_rect_proximity() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
        let r0 = PaneRunner::spawn_with_graphics(
            pane_a,
            id_a,
            shell.clone(),
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn r0");
        let r1 = PaneRunner::spawn_with_graphics(
            pane_b,
            id_b,
            shell,
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
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
            close_rx,
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
            None,
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

    /// `PaneClose` rebalance: with focus on child 0 of a 2-leaf
    /// Split, the Split's sibling-absorption rebalance
    /// collapses the Split into child 1; `reconcile_runners`
    /// rebuilds `Vec<PaneRunner>` against the post-rebalance
    /// tree with the survivor's `LayerId` intact.
    #[tokio::test]
    async fn pane_close_rebalance_collapses_split_to_one_leaf() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
        let r0 = PaneRunner::spawn_with_graphics(
            pane_a,
            id_a,
            shell.clone(),
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn r0");
        let r1 = PaneRunner::spawn_with_graphics(
            pane_b,
            id_b,
            shell,
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
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
            close_rx,
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
            None,
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

    /// Duplicate-label survivor preservation: a config with two
    /// panes sharing the same label ("dup") undergoes
    /// `AppNewPane` on the first pane. The `InPlace` reconcile
    /// path must preserve BOTH survivors — without the
    /// `HashMap<String, Vec<PaneRunner>>` fix, the second
    /// `insert` would overwrite the first survivor, causing its
    /// `Drop` to fire spuriously and the second same-labeled
    /// pane to get a fresh spawn instead of inheriting the
    /// survivor. Pins the duplicate-label collision fix.
    #[tokio::test]
    async fn reconcile_inplace_preserves_both_survivors_with_duplicate_labels() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout {
            split axis=horizontal ratio=0.5 {
                pane kind=shell label="dup"
                pane kind=shell label="dup"
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
        assert_eq!(initial_layout.panes.len(), 2);
        let pane_a = initial_layout.panes[0].clone();
        let pane_b = initial_layout.panes[1].clone();
        let id_a = cmdash::derive_layer_id(&pane_a.id);
        let id_b = cmdash::derive_layer_id(&pane_b.id);
        let r0 = PaneRunner::spawn_with_graphics(
            pane_a,
            id_a,
            shell.clone(),
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn r0");
        let r1 = PaneRunner::spawn_with_graphics(
            pane_b,
            id_b,
            shell.clone(),
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn r1");
        let layer_a = r0.layer_id();
        let layer_b = r1.layer_id();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers {
                ctrl: true,
                ..CfgModifiers::default()
            },
            key: KeyToken::Char('a'),
            action: KeyAction::AppNewPane,
        }]);
        let last_area = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let mut ctx = TickContext::new_full(
            vec![r0, r1],
            bindings,
            0,
            true,
            close_tx,
            close_rx,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            last_area,
            BTreeMap::new(),
            BTreeMap::new(),
            shell,
            None,
        );
        ctx.apply_action_full(KeyAction::AppNewPane);
        // After AppNewPane on focus=0 of a 2-leaf Split with
        // duplicate labels: the tree becomes a 3-leaf Split
        // (original child 0 is now itself a Split of [dup, new]).
        // The InPlace reconcile must preserve BOTH "dup"-labeled
        // survivors (r0 and r1) by their LayerIds. The new pane
        // (label=None) gets a fresh spawn.
        assert_eq!(
            ctx.runners.len(),
            3,
            "AppNewPane on a 2-pane split yields 3 panes"
        );
        // Collect all surviving LayerIds.
        let survivor_layer_ids: Vec<PaneLayerId> =
            ctx.runners.iter().map(|r| r.layer_id()).collect();
        // Both original LayerIds must be present (the duplicate-label
        // fix preserves both; without the fix, one would be dropped
        // and replaced by a fresh spawn).
        assert!(
            survivor_layer_ids.contains(&layer_a),
            "survivor A's LayerId must be preserved after AppNewPane with duplicate labels"
        );
        assert!(
            survivor_layer_ids.contains(&layer_b),
            "survivor B's LayerId must be preserved after AppNewPane with duplicate labels"
        );
    }

    /// PanePreset(name): wholesale `layout_root` swap; the
    /// original pane's `LayerId` is revoked (Hard rule); the new
    /// tree has fresh `LayerIds` per pane. Pin: distinct fresh
    /// `LayerIds` per pane, AND the `original` `LayerId` does NOT
    /// appear in the post-state Vec.
    #[tokio::test]
    async fn pane_preset_swaps_layout_root_and_reconciles_runners() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            close_rx,
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
            None,
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
    /// focus through a focused `ZStack`'s members with
    /// wrap-around (last member -> first member). The
    /// focused pane is the LAST member (top by z-order /
    /// `pre_order`) of a 3-`member` `ZStack`; cycling once
    /// must wrap it to the FIRST member. Pins the
    /// "within-ZStack rotatation" half of the Phase 4
    /// contract.
    #[tokio::test]
    async fn pane_stack_cycle_wraps_around_zstack_focus() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
                    cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            close_rx,
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
            None,
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
    /// focused member is NOT the last of the `ZStack`
    /// advances to the next member in declaration order
    /// (no wrap; no geometric handoff). The handoff case
    /// is covered separately by
    #[tokio::test]
    /// `pane_stack_down_at_top_hands_off_to_pane_below`.
    async fn pane_stack_down_within_stack_advances_to_next_member() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
                    cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            close_rx,
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
            None,
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
    /// `ZStack`'s last (top by z-order / `pre_order`) member
    /// hands focus off to the topmost pane geometrically
    /// below the `ZStack` via [`adjacent_pane`]. The
    /// fixture's outer horizontal split places one
    /// default-configured pane ("below") under the
    /// `ZStack` so the geometry is unambiguous; `the` `ZStack`
    /// occupies the top half (y=0..12), the below-pane
    /// occupies the bottom half (y=12..24). Focus the
    /// LAST member of the `ZStack` ("top") and press
    #[tokio::test]
    /// `PaneStackDown`; focus must hand off to "below"
    /// (path [0, 1]).
    async fn pane_stack_down_at_top_hands_off_to_pane_below() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
                    cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            close_rx,
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
            None,
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

    #[tokio::test]
    async fn pane_stack_up_within_stack_advances_to_previous_member() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
                    cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            close_rx,
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
            None,
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

    #[tokio::test]
    async fn pane_stack_up_at_bottom_hands_off_to_pane_above() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
                    cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            close_rx,
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
            None,
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
    /// the **previous** member of the focused `ZStack` in
    /// declaration order. Stop before the first member (no
    /// handoff in this test).
    #[tokio::test]
    async fn pane_stack_left_within_stack_retreats_to_previous_member() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
                    cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            close_rx,
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
            None,
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
    /// to the **next** member of the focused `ZStack` in
    /// declaration order. Stop before the last member (no
    /// handoff in this test).
    #[tokio::test]
    async fn pane_stack_right_within_stack_advances_to_next_member() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
                    cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            close_rx,
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
            None,
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
    /// `ZStack`'s last (rightmost-by-declaration) member hands
    /// focus off to the topmost pane geometrically to the
    /// RIGHT of the `ZStack` via [`adjacent_pane`]. The
    /// fixture's outer horizontal split places the `ZStack` in
    /// the left column (x=0..40) and a default-configured
    /// pane ("`right_outside`") in the right column (x=40..80)
    /// so the geometry is unambiguous. Focus the LAST
    /// member ("`right_inside`") and `press` `PaneStackRight`;
    /// focus must hand off to "`right_outside`" (path [0, 1]).
    /// Pinned by `split_rect_horizontal_60` in the
    /// cmdash-layout crate's ground-truth unit tests.
    #[tokio::test]
    async fn pane_stack_right_at_last_member_hands_off_to_pane_right() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
                    cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            close_rx,
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
            None,
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
    /// `ZStack`'s first (leftmost-by-declaration) member hands
    /// focus off to the topmost pane geometrically to the
    /// LEFT of the `ZStack` via [`adjacent_pane`]. The
    /// fixture's outer horizontal split places a default-
    /// configured pane ("`left_outside`") in the left column
    /// (x=0..40) and the `ZStack` in the right column
    /// (x=40..80) so the geometry is unambiguous. Focus the
    /// FIRST member ("`left_inside`") and `press` `PaneStackLeft`;
    /// focus must hand off to "`left_outside`" (path [0, 0]).
    #[tokio::test]
    async fn pane_stack_left_at_first_member_hands_off_to_pane_left() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
                    cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            close_rx,
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
            None,
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

    /// Phase 5.0 carry-forward: `PaneStackRight` on a `ZStack`
    /// with exactly ONE member must immediately hand off to
    /// `Direction::Right` rather than advancing the focus.
    /// The boundary check `member_idx + 1 == panes.len()`
    /// inside `crosstack_member` triggers regardless of the
    /// advance/retreat branch (single-member `ZStacks` hit
    /// BOTH boundary conditions by definition: `member_idx
    /// == 0` AND `member_idx + 1 == panes.len()`). This pins
    /// the edge case at the consolidated dispatch site so
    /// future additions of directional variants can't
    /// regress it silently. Use `axis=horizontal` (column
    /// split -- same y, different x) so the side pane sits
    /// in the geometric right of the 1-member `ZStack`.
    #[tokio::test]
    async fn pane_stack_right_on_one_member_zstack_immediately_hands_off_to_right() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
                    cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            close_rx,
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
            None,
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

    /// Phase 5.0 carry-forward: `PaneStackLeft` on a `ZStack`
    /// with exactly ONE member must immediately hand off to
    /// `Direction::Left` rather than retreating the focus.
    /// Horizontal-axis mirror of
    /// `pane_stack_right_on_one_member_zstack_immediately_hands_off_to_right`;
    /// same dual boundary-condition rationale (single-member
    /// `ZStack` hits BOTH `member_idx == 0` and
    /// `member_idx + 1 == panes.len()` from inside `crosstack_member`).
    #[tokio::test]
    async fn pane_stack_left_on_one_member_zstack_immediately_hands_off_to_left() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
                    cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
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
            close_rx,
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
            None,
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

    /// Phase 6 carry-forward: `PaneStackCycle` on a `ZStack`
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
    /// stack `ZStack` at root -- NO `split axis=horizontal` or
    /// `split axis=vertical` Split pane -- so the axis-trapdoor
    /// (which only matters for the boundary-handoff path in
    /// `crosstack_member`) cannot confound the assertion.
    /// Cycle's algorithm is closed (no handoff), so any axis
    /// trapdoor in the fixture would only add noise.
    #[tokio::test]
    async fn pane_stack_cycle_on_one_member_zstack_wraps_to_same_member() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
        let r0 = PaneRunner::spawn_with_graphics(
            pane_only,
            id_only,
            shell,
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
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
            close_rx,
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
            None,
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
    /// `ZStack` at the LAST member wraps modulo-style:
    /// `(2 + 1) % 3 == 0` -- the FIRST member. Pin: focus
    /// idx shifts from 2 to 0 (full wrap), `stack_focus`
    /// records (`id_a`, 0) for the post-wrap focus, AND
    /// `post_focus_id`.`path`()[1] == 0 pins the declaration-
    /// order index of the FIRST member (path[0] is the
    /// resolver seed, always 0; path[1] is the meaningful
    /// ZStack-member index per the resolver convention).
    ///
    /// **Trapdoor avoidance**: deliberately a pure within-
    /// stack `ZStack` at root -- NO `split axis=horizontal`
    /// ANYWHERE -- because cycle's algorithm has no handoff
    /// path. The axis-trapdoor (column vs row split) only
    /// affects `crosstack_member`'s boundary-handoff path;
    /// using axis-trapdoor fixtures for a cycle test would
    /// be a semantic-noose (the fixture's trapdoor would
    /// be irrelevant to cycle's behavior and would invite a
    /// future reader to misinterpret the assertion).
    #[tokio::test]
    async fn pane_stack_cycle_on_three_member_zstack_wraps_last_to_first() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
        let r0 = PaneRunner::spawn_with_graphics(
            pane_a,
            id_a,
            shell.clone(),
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn r0");
        let r1 = PaneRunner::spawn_with_graphics(
            pane_b,
            id_b,
            shell.clone(),
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn r1");
        let r2 = PaneRunner::spawn_with_graphics(
            pane_c,
            id_c,
            shell,
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
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
            close_rx,
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
            None,
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

    // ============================================================
    // Phase 2 carry-forward: EDGE-CASE tests for the runtime-
    // mutation arms (`AppNewPane`, `PaneFocus{Direction}`,
    // `PaneClose`, `PanePreset`) driven through
    // `TickContext::apply_action_full`. The four primary tests
    // above pin the happy-path; this block pins the boundary /
    // no-op surfaces that the structural-deliverable row's
    // deferred lib-crate half covers.
    //
    // AGENTS.md Hard rule + structural-finding pins covered:
    // - close_rx round-trip pin (Hard rule: one layer per
    //   instance; the Drop -> close_tx -> close_rx channel
    //   must echo the dropped pane's PaneLayerId back out for
    //   the binary's tick-loop phase 1 to drain).
    // - survivor `PaneId.path_len` reconcile-after-`AppNewPane`
    //   (the lib-crate harness verifies the survivor's
    //   `path_len` ticks from 1 to 2 post-`AppNewPane` because
    //   the layout engine now wraps the focused leaf in a
    //   Split; the full-PaneId reconcile is TickContext-owned).
    // - sibling-absorbed `PaneClose` (closing the only pane
    //   quits the binary via `running = false`, the v1 PaneClose
    //   path with a TickContext ctor shape).
    // - `PaneFocusUp` / `PaneFocusDown` no-op on a 1-row
    //   Horizontal Split (the adjacent-pane algorithm returns
    //   `None` when no neighbour exists on the axis).
    // - `PanePreset("missing")` no-op (unknown preset names
    //   don't mutate `self.layout_root`).
    // ============================================================

    /// Edge case: `PaneFocusUp` / `PaneFocusDown` against a
    /// 1-row Horizontal Split must NO-OP (no neighbour on the
    /// vertical axis). Pins the
    /// `cmdash_layout::adjacent_pane` fallback-on-`None` arm
    /// for the up/down directions; left/right adjacency is
    /// exercised separately by
    /// `pane_focus_right_resolves_to_adjacent_pane_via_rect_proximity`.
    #[tokio::test]
    async fn apply_action_full_pane_focus_up_down_noop_on_single_row() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
        let r0 = PaneRunner::spawn_with_graphics(
            pane_a,
            id_a,
            shell.clone(),
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn r0");
        let r1 = PaneRunner::spawn_with_graphics(
            pane_b,
            id_b,
            shell,
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
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
            close_rx,
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
            None,
        );
        assert_eq!(ctx.focus, 0);
        ctx.apply_action_full(KeyAction::PaneFocusUp);
        assert_eq!(
            ctx.focus, 0,
            "PaneFocusUp on a 1-row H-Split must NO-OP (no neighbour on vertical axis)"
        );
        // Right at the boundary is exercised by the primary
        // PaneFocusRight test -- here we only assert Up/Down
        // stay no-op from the focus=0 starting point.
        ctx.apply_action_full(KeyAction::PaneFocusDown);
        assert_eq!(
            ctx.focus, 0,
            "PaneFocusDown on a 1-row H-Split must NO-OP (no neighbour on vertical axis)"
        );
        // Move to the right pane and re-pane Up/Down no-op:
        // same surface, different focus.
        ctx.apply_action_full(KeyAction::PaneFocusRight);
        assert_eq!(ctx.focus, 1);
        ctx.apply_action_full(KeyAction::PaneFocusUp);
        assert_eq!(
            ctx.focus, 1,
            "PaneFocusUp from focus=1 also NO-OPs on a 1-row H-Split"
        );
        ctx.apply_action_full(KeyAction::PaneFocusDown);
        assert_eq!(
            ctx.focus, 1,
            "PaneFocusDown from focus=1 also NO-OPs on a 1-row H-Split"
        );
    }

    /// Edge case: `PaneClose` on a single-leaf `TickContext`
    /// flips `running = false` (the binary quits) and the
    /// `Vec<PaneRunner>` drains to empty. Pins the
    /// empty-`runners`-post-close -> quit-the-binary arm
    /// distinguishable from the multi-leaf rebalance case
    /// (`pane_close_rebalance_collapses_split_to_one_leaf`
    /// keeps the survivor and doesn't quit).
    #[tokio::test]
    async fn apply_action_full_pane_close_final_pane_quits() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
        let shell = ShellSpec::Command {
            argv: vec!["true".into()],
        };
        let cfg_text = r#"layout { pane kind=shell label="only" }"#;
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
        let layer = cmdash::derive_layer_id(&pane.id);
        let runner = PaneRunner::spawn_with_graphics(
            pane,
            layer,
            shell,
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn");
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
            close_rx,
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
            None,
        );
        assert!(ctx.running, "pre-close: binary must be running");
        assert_eq!(ctx.runners.len(), 1);
        ctx.apply_action_full(KeyAction::PaneClose);
        assert!(
            ctx.runners.is_empty(),
            "PaneClose on the only pane must drain the runner Vec"
        );
        assert!(
            !ctx.running,
            "PaneClose on the only pane must flip running -> false (binary quits)"
        );
    }

    /// Edge case: `PanePreset("missing")` on a `TickContext`
    /// whose `presets` map lacks that name must NO-OP. The
    /// swap-to-preset handler logs `unknown name; no-op` and
    /// returns without mutating `self.layout_root` /
    /// `self.runners`. Pins the unrelated-preset-name
    /// rejection surface so a future regression that
    /// accidentally treats any string as a `KeyAction::PanePreset`
    /// target (or panics on `BTreeMap::get` returning `None`)
    /// fails this check.
    #[tokio::test]
    async fn apply_action_full_pane_preset_unknown_name_noop() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
            pane.clone(),
            original_layer,
            shell.clone(),
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn");
        // Seed the presets map with a DIFFERENT-named entry
        // (the BTreeMap presence is non-None for the
        // `is_some()` branch, but `name != self.preset_name`
        // is the predicate the swap handler uses; both
        // surfaces are pinned by this single test).
        let beta_cfg_text = r#"layout {
            split axis=horizontal ratio=0.6 {
                pane kind=shell label="beta-left"
                pane kind=shell label="beta-right"
            }
        }"#;
        let beta_cfg = cmdash_config::parse(beta_cfg_text).expect("parse beta");
        let beta_layout_root = beta_cfg.layout.expect("beta layout block");
        let mut presets = BTreeMap::new();
        presets.insert("beta".to_string(), beta_layout_root);
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
            close_rx,
            graphics,
            &mut terminal,
            Duration::from_millis(33),
            layout_root.clone(),
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
            None,
        );
        ctx.apply_action_full(KeyAction::PanePreset("missing".to_string()));
        // After a no-op preset swap: runners.len unchanged,
        // focused LayerId unchanged, layout_root unchanged.
        assert_eq!(
            ctx.runners.len(),
            1,
            "PanePreset(\"missing\") must leave runners.len unchanged"
        );
        assert_eq!(
            ctx.runners[0].layer_id(),
            original_layer,
            "PanePreset(\"missing\") must NOT revoke the original LayerId"
        );
        // layout_root is `Clone`-only (it has `#[derive(Debug,
        // Clone)]` per cmdash-config); match by structural
        // equality against the pre-snapshot we cloned in.
        // Compute the post-state canvas from the layout_root
        // to confirm it's STILL the alpha single-pane tree.
        let post_layout = ComputedLayout::compute(&ctx.layout_root, ctx.last_area)
            .expect("post-preset-noop compute");
        assert_eq!(
            post_layout.panes.len(),
            1,
            "PanePreset(\"missing\") must not change the layout's leaf count"
        );
    }

    /// Hard-rule pin (AGENTS.md \u00a7\"Hard rule: one layer per
    /// instance\"): after `PaneClose` drops the focused
    /// runner, the `PaneRunner::Drop` impl enqueues the
    /// runner's `PaneLayerId` onto the close-channel; the
    /// binary's tick-loop phase 1 drains the channel and
    /// routes each enqueued id through
    /// `GraphicsState::close_pane` (which revokes the
    /// termcompositor image registration for that id).
    /// Verify the `close_rx` round-trip directly: after
    /// `apply_action_full(KeyAction::PaneClose)` on focus=0
    /// of a 2-pane H-Split, `close_rx`.`try_recv`() must yield
    /// the LEFT pane's dropped `PaneLayerId`, AND the
    /// survivor's `PaneLayerId` is unchanged.
    #[tokio::test]
    async fn apply_action_full_pane_close_drops_runner_routes_close_message() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
        let r0 = PaneRunner::spawn_with_graphics(
            pane_a,
            id_a,
            shell.clone(),
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn r0");
        let r1 = PaneRunner::spawn_with_graphics(
            pane_b,
            id_b,
            shell,
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
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
            close_rx,
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
            None,
        );
        ctx.apply_action_full(KeyAction::PaneClose);
        // Primary pin: close_rx must echo back the dropped
        // PaneRunner's PaneLayerId.
        let dropped = ctx
            .close_rx
            .try_recv()
            .expect("PaneRunner::Drop must enqueue the closing pane's layer id on close_tx");
        assert_eq!(
            dropped, id_a,
            "close_rx must yield the LEFT pane's (focus=0) PaneLayerId"
        );
        assert_eq!(
            ctx.runners.len(),
            1,
            "PaneClose rebalance collapses the Split from 2 to 1 runners"
        );
        assert_eq!(
            ctx.runners[0].layer_id(),
            survivor_layer,
            "PaneClose rebalance: survivor pane keeps its LayerId per Hard rule"
        );
    }

    /// Structural-finding pin (AGENTS.md Phase 2 carry-forward
    /// structural-deliverable row item 2: `AppNewPane`
    /// survivor's `PaneId` reconcile-gated-later): after
    /// `apply_action_full(KeyAction::AppNewPane)` on a
    /// single-leaf root, the post-state `Vec<PaneRunner>`
    /// has length 2; the ORIGINAL focused pane's
    /// `PaneLayerId` is preserved (Hard rule per
    /// `app_new_pane_splits_focused_leaf_and_spawns_runner`
    /// above); AND the layout-root -> Split tree's
    /// `ComputedLayout::compute` output reports
    /// `panes[0].id.path_len == 2` because the focused
    /// leaf now lives at `path == [0, 0]` (parent-Split +
    /// first-child). The full `PaneId` reconcile is
    /// TickContext-owned (the lib-crate harness pins only
    /// the `path_len` invariant; the same algorithm reaches
    /// through to the vector `PaneId` once `TickContext`
    /// `relayout`s on the next SIGWINCH or zero-area
    /// pin-event).
    #[tokio::test]
    async fn apply_action_full_app_new_pane_survivor_path_len_reconciles_to_two() {
        let (close_tx, close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
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
        assert_eq!(
            initial_layout.panes[0].id.path_len(),
            1,
            "pre-AppNewPane: single-leaf layout yields path_len == 1"
        );
        let runner = PaneRunner::spawn_with_graphics(
            pane,
            original_layer,
            shell,
            Some(close_tx.clone()),
            cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
        )
        .expect("spawn");
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
            close_rx,
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
            None,
        );
        ctx.apply_action_full(KeyAction::AppNewPane);
        // Post-AppNewPane pin: layout_root became a Split, the
        // survivor (the original alpha pane) is now at
        // path [0, 0] with path_len == 2 (Split + leaf).
        let post_layout = ComputedLayout::compute(&ctx.layout_root, ctx.last_area)
            .expect("post-AppNewPane compute");
        assert_eq!(post_layout.panes.len(), 2);
        let survivor_paneid = &post_layout.panes[0].id;
        assert_eq!(
            survivor_paneid.path_len(),
            2,
            "survivor's PaneId.path_len must tick from 1 to 2 after AppNewPane wraps the focused leaf in a Split"
        );
        assert_eq!(
            survivor_paneid.path(),
            &[0, 0][..],
            "survivor's PaneId path is [0, 0] (parent-Split + first-child)"
        );
        // Hard rule pin (already covered by the primary
        // `app_new_pane_splits_focused_leaf_and_spawns_runner`,
        // but re-pinned here for the path_len invariant's
        // audit-trail witness).
        assert_eq!(
            ctx.runners[0].layer_id(),
            original_layer,
            "AppNewPane: original focused pane's LayerId is preserved (Hard rule)"
        );
    }

    /// Build a minimal context with a single pane and a runner
    /// produced by `make_runner`. The shared layout/compute/setup
    /// logic is reused by `setup_run_loop_ctx` (widget runner) and
    /// `setup_shell_ctx` (stub PTY runner).
    fn setup_ctx_with_runner<'a>(
        terminal: &'a mut ratatui::Terminal<ratatui::backend::TestBackend>,
        make_runner: impl FnOnce(ComputedPane, PaneLayerId, PaneCloseTx) -> PaneRunner,
        bindings: Router,
    ) -> TickContext<'a, ratatui::backend::TestBackend> {
        let caps = TermCapabilities {
            graphics: GraphicsProtocol::Kitty,
            kitty_keyboard: true,
            focus_events: true,
            bracketed_paste: true,
            true_color: true,
            color_256: true,
            queries: true,
        };
        setup_ctx_with_runner_and_caps(terminal, make_runner, bindings, caps)
    }

    fn setup_ctx_with_runner_and_caps<'a>(
        terminal: &'a mut ratatui::Terminal<ratatui::backend::TestBackend>,
        make_runner: impl FnOnce(ComputedPane, PaneLayerId, PaneCloseTx) -> PaneRunner,
        bindings: Router,
        caps: TermCapabilities,
    ) -> TickContext<'a, ratatui::backend::TestBackend> {
        let kdl = r#"layout { pane kind=shell }"#;
        let cfg = cmdash_config::parse(kdl).expect("parse KDL");
        let layout_root = cfg.layout.expect("layout block");
        let last_area = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let layout = ComputedLayout::compute(&layout_root, last_area).expect("compute layout");
        let pane = layout.panes[0].clone();
        let layer_id = cmdash::derive_layer_id(&pane.id);
        let (close_tx, close_rx) = unbounded_channel::<PaneLayerId>();
        let runner = make_runner(pane, layer_id, close_tx.clone());
        let runners = vec![runner];
        let graphics = GraphicsState::new_with_caps(
            cmdash::graphics::Metrics::default(),
            (last_area.w, last_area.h),
            caps,
        );
        TickContext::new_full(
            runners,
            bindings,
            0,
            true,
            close_tx,
            close_rx,
            graphics,
            terminal,
            Duration::from_millis(33),
            layout_root,
            None,
            last_area,
            BTreeMap::new(),
            BTreeMap::new(),
            ShellSpec::LoginShell,
            None,
        )
    }

    /// Build a minimal context with one widget runner (no real PTY)
    /// so `tick_and_render` is cheap and cannot leave a child process
    /// behind if a test panics or times out. The keybinding `q` is
    /// wired to `AppClose` so tests can drive a clean exit.
    fn setup_run_loop_ctx<'a>(
        terminal: &'a mut ratatui::Terminal<ratatui::backend::TestBackend>,
    ) -> TickContext<'a, ratatui::backend::TestBackend> {
        use cmdash_widget_sdk::{CmdashWidget, WidgetEvent};

        struct DummyWidget;
        impl CmdashWidget for DummyWidget {
            fn name(&self) -> &str {
                "dummy"
            }
            fn render(&mut self, _area: ratatui::layout::Rect, _frame: &mut ratatui::Frame) {}
            fn on_event(&mut self, _event: &WidgetEvent) {}
        }

        setup_ctx_with_runner(
            terminal,
            |pane, layer_id, close_tx| {
                PaneRunner::spawn_widget(pane, layer_id, Box::new(DummyWidget), Some(close_tx))
            },
            Router::new(vec![Keybind {
                mods: CfgModifiers::default(),
                key: KeyToken::Char('q'),
                action: KeyAction::AppClose,
            }]),
        )
    }

    /// `process_pending_resize` should consume the `pending_resize` slot
    /// and run relayout against the requested dimensions.
    #[tokio::test]
    async fn process_pending_resize_consumes_slot_and_relayouts() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let mut ctx = setup_run_loop_ctx(&mut terminal);
        ctx.pending_resize = Some((132, 50));
        ctx.process_pending_resize();
        assert!(
            ctx.pending_resize.is_none(),
            "pending_resize should be consumed"
        );
        assert_eq!(ctx.last_area.w, 132, "layout width should be updated");
        assert_eq!(
            ctx.last_area.h,
            50 - TAB_BAR_HEIGHT,
            "layout height should account for tab bar"
        );
    }

    /// `drain_close_channel` should remove all pending close messages
    /// from the close receiver.
    #[tokio::test]
    async fn drain_close_channel_drains_messages() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let mut ctx = setup_run_loop_ctx(&mut terminal);
        // Send a close message through the context's own close_tx.
        let id = PaneLayerId(999);
        ctx.close_tx.send(id).expect("send close message");
        ctx.drain_close_channel();
        assert!(
            ctx.close_rx.try_recv().is_err(),
            "close channel should be empty"
        );
    }

    /// `tick_runners` should return one snapshot per runner and report
    /// that not all panes have exited (the dummy widget never exits).
    #[tokio::test]
    async fn tick_runners_returns_snapshots_and_exit_status() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let mut ctx = setup_run_loop_ctx(&mut terminal);
        let result = ctx.tick_runners().expect("tick_runners should succeed");
        assert_eq!(
            result.snapshots.len(),
            ctx.runners.len(),
            "one snapshot per runner"
        );
        assert!(
            !result.all_exited,
            "widget runner should not be considered exited"
        );
    }

    #[cfg(test)]
    mod shell_spec_from_command_tests {
        use super::*;

        /// `None` command returns the default shell.
        #[tokio::test]
        async fn none_returns_default() {
            let default = ShellSpec::LoginShell;
            let result = shell_spec_from_command(&None, &default);
            assert_eq!(result, ShellSpec::LoginShell);
        }

        /// `Some("")` (empty string) returns the default shell
        /// because `split_whitespace()` yields an empty iterator.
        #[tokio::test]
        async fn empty_string_returns_default() {
            let default = ShellSpec::LoginShell;
            let result = shell_spec_from_command(&Some(String::new()), &default);
            assert_eq!(result, ShellSpec::LoginShell);
        }

        /// `Some("  ")` (whitespace-only) returns the default shell
        /// because `split_whitespace()` yields an empty iterator.
        #[tokio::test]
        async fn whitespace_only_returns_default() {
            let default = ShellSpec::LoginShell;
            let result = shell_spec_from_command(&Some("  ".to_string()), &default);
            assert_eq!(result, ShellSpec::LoginShell);
        }

        /// `Some("htop")` produces `Command { argv: ["htop"] }`.
        #[tokio::test]
        async fn simple_command() {
            let default = ShellSpec::LoginShell;
            let result = shell_spec_from_command(&Some("htop".to_string()), &default);
            assert_eq!(
                result,
                ShellSpec::Command {
                    argv: vec!["htop".to_string()]
                }
            );
        }

        /// `Some("htop --arg1 --arg2")` produces
        /// `Command { argv: ["htop", "--arg1", "--arg2"] }`.
        #[tokio::test]
        async fn command_with_args() {
            let default = ShellSpec::LoginShell;
            let result = shell_spec_from_command(&Some("htop --arg1 --arg2".to_string()), &default);
            assert_eq!(
                result,
                ShellSpec::Command {
                    argv: vec![
                        "htop".to_string(),
                        "--arg1".to_string(),
                        "--arg2".to_string()
                    ]
                }
            );
        }

        /// `Some("echo hello world")` produces
        /// `Command { argv: ["echo", "hello", "world"] }`.
        #[tokio::test]
        async fn command_with_multiple_args() {
            let default = ShellSpec::LoginShell;
            let result = shell_spec_from_command(&Some("echo hello world".to_string()), &default);
            assert_eq!(
                result,
                ShellSpec::Command {
                    argv: vec!["echo".to_string(), "hello".to_string(), "world".to_string()]
                }
            );
        }

        /// Different default shell is preserved when command is `None`.
        #[tokio::test]
        async fn none_preserves_custom_default() {
            let default = ShellSpec::Command {
                argv: vec!["/bin/bash".to_string()],
            };
            let result = shell_spec_from_command(&None, &default);
            assert_eq!(
                result,
                ShellSpec::Command {
                    argv: vec!["/bin/bash".to_string()]
                }
            );
        }

        /// Explicit command overrides the default shell.
        #[tokio::test]
        async fn some_overrides_default() {
            let default = ShellSpec::Command {
                argv: vec!["/bin/bash".to_string()],
            };
            let result = shell_spec_from_command(&Some("/bin/zsh".to_string()), &default);
            assert_eq!(
                result,
                ShellSpec::Command {
                    argv: vec!["/bin/zsh".to_string()]
                }
            );
        }
    }

    #[cfg(test)]
    mod tab_bar_render_tests {
        use super::*;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        /// Helper: build a minimal `TabStack<TabState>` with `n` tabs,
        /// the first tab active, no labels. Each `TabState` is
        /// constructed with an empty runner Vec and a single-pane
        /// layout root so `TabStack` invariants hold.
        fn make_tabs(n: usize) -> TabStack<TabState> {
            let dummy_state = TabState {
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
            let mut tabs = TabStack::new(dummy_state);
            for _ in 1..n {
                tabs.push(TabState {
                    runners: Vec::new(),
                    focus: 0,
                    layout_root: LayoutNode::Pane(CfgPane {
                        kind: PaneKind::Shell,
                        label: None,
                        command: None,
                        scrollback_capacity: None,
                    }),
                    stack_focus: BTreeMap::new(),
                });
            }
            // `push` sets the new tab as active; switch back to tab 0
            // so the first tab is active by default.
            tabs.switch_to(0);
            tabs
        }

        /// Helper: build a `TabStack` with labels on specific tabs.
        /// `labels[i]` is the label for tab `i`; `None` means no label.
        fn make_tabs_with_labels(labels: &[Option<&str>]) -> TabStack<TabState> {
            assert!(!labels.is_empty(), "need at least one tab");
            let dummy_state = TabState {
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
            let mut tabs = if let Some(l) = labels[0] {
                TabStack::new_with_label(dummy_state, l)
            } else {
                TabStack::new(dummy_state)
            };
            for l in &labels[1..] {
                let st = TabState {
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
                match l {
                    Some(label) => {
                        tabs.push_with_label(st, *label);
                    }
                    None => {
                        tabs.push(st);
                    }
                }
            }
            // `push` / `push_with_label` sets the new tab as active;
            // switch back to tab 0 so the first tab is active by default.
            tabs.switch_to(0);
            tabs
        }

        /// Helper: extract the text content of row 0 from a buffer
        /// as a `String` (symbol per cell, spaces included).
        fn row_text(buf: &Buffer) -> String {
            let w = buf.area.width as usize;
            let mut s = String::with_capacity(w);
            for x in 0..w {
                s.push_str(buf[(x as u16, 0)].symbol());
            }
            s
        }

        /// Helper: extract the `Style` of a single cell at `(x, 0)`.
        fn cell_style(buf: &Buffer, x: u16) -> ratatui::style::Style {
            buf[(x, 0)].style()
        }

        /// Single tab with no label: row 0 shows " 1 " starting at
        /// column 0 with blue bg + white bold style (active tab).
        #[tokio::test]
        async fn single_tab_no_label() {
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, 1));
            let tabs = make_tabs(1);
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            assert!(
                text.starts_with(" 1 "),
                "single unlabeled tab must render as ' 1 '; got: {:?}",
                &text[..5.min(text.len())]
            );
            // Active tab: blue bg, white fg, bold.
            let s = cell_style(&buf, 1);
            assert_eq!(s.bg, Some(Color::Blue), "active tab bg must be Blue");
            assert_eq!(s.fg, Some(Color::White), "active tab fg must be White");
            assert!(
                s.add_modifier.contains(Modifier::BOLD),
                "active tab must be bold"
            );
        }

        /// Single tab with a label: row 0 shows " 1:main ".
        #[tokio::test]
        async fn single_tab_with_label() {
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, 1));
            let tabs = make_tabs_with_labels(&[Some("main")]);
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            assert!(
                text.starts_with(" 1:main "),
                "single labeled tab must render as ' 1:main '; got: {:?}",
                &text[..10.min(text.len())]
            );
        }

        /// Empty-string label is treated as `None` (no dangling colon).
        #[tokio::test]
        async fn empty_label_filtered_to_no_colon() {
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, 1));
            let tabs = make_tabs_with_labels(&[Some("")]);
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            assert!(
                text.starts_with(" 1 "),
                "empty label must render as unlabeled ' 1 '; got: {:?}",
                &text[..5.min(text.len())]
            );
            assert!(
                !text.starts_with(" 1:"),
                "empty label must NOT produce a colon after the number"
            );
        }

        /// Multi-tab layout: 3 tabs, first active. Verify ordering,
        /// separator spaces, and that tab 2+3 use inactive style.
        #[tokio::test]
        async fn multi_tab_ordering_and_separator() {
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, 1));
            let tabs = make_tabs_with_labels(&[Some("a"), Some("b"), Some("c")]);
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            // " 1:a " + " " + " 2:b " + " " + " 3:c "
            assert!(
                text.starts_with(" 1:a   2:b   3:c "),
                "3-tab layout must show ' 1:a   2:b   3:c '; got: {:?}",
                &text[..20.min(text.len())]
            );
        }

        /// Active tab highlight: tab 1 (idx 0) is active (blue bg);
        /// tab 2 (idx 1) is inactive (dark gray bg).
        #[tokio::test]
        async fn active_highlight_vs_inactive() {
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, 1));
            let tabs = make_tabs_with_labels(&[Some("active"), Some("inactive")]);
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            // Active tab cell (column 1, inside ' 1:active ').
            let active_s = cell_style(&buf, 1);
            assert_eq!(active_s.bg, Some(Color::Blue));
            assert_eq!(active_s.fg, Some(Color::White));
            // Find the start of tab 2 text. " 1:active " = 10 chars,
            // Tab 0 " 1:active " = 10 chars (cols 0-9). Separator
            // bumps col to 11 but does NOT write a cell, so col 10
            // retains the initial clear style (DarkGray bg). Tab 1
            // starts writing at col 11 with inactive style.
            let inactive_s = cell_style(&buf, 11);
            assert_eq!(
                inactive_s.bg,
                Some(Color::DarkGray),
                "inactive tab bg must be DarkGray"
            );
            assert_eq!(
                inactive_s.fg,
                Some(Color::Gray),
                "inactive tab fg must be Gray"
            );
        }

        /// Second tab is active: verify the highlight moves correctly.
        #[tokio::test]
        async fn second_tab_active_highlight() {
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, 1));
            let mut tabs = make_tabs_with_labels(&[Some("first"), Some("second")]);
            // Switch active to tab 1 (second tab, 0-indexed).
            tabs.switch_to(1);
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            // Tab 1 (idx 0) should be inactive.
            let tab1_style = cell_style(&buf, 1);
            assert_eq!(
                tab1_style.bg,
                Some(Color::DarkGray),
                "first tab must be inactive when second is active"
            );
            // Tab 2 (idx 1) starts at col 10 (" 1:first " = 9 chars + separator).
            let tab2_style = cell_style(&buf, 10);
            assert_eq!(
                tab2_style.bg,
                Some(Color::Blue),
                "second tab must be active (Blue bg)"
            );
        }

        /// Truncation: 3 tabs in a 20-column buffer. Only tabs that
        /// fit are rendered; the rest are silently dropped.
        #[tokio::test]
        async fn three_tabs_fit_in_wide_buffer() {
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
            let tabs = make_tabs_with_labels(&[Some("a"), Some("b"), Some("c")]);
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            // " 1:a " = 5, sep = 1, " 2:b " = 5, sep = 1, " 3:c " = 5 => 17 chars.
            // All 3 tabs fit in 20 columns.
            assert!(
                text.contains(" 3:c "),
                "all 3 tabs must fit in 20 cols; got: {:?}",
                text
            );
        }

        /// Truncation: 3 tabs in a 12-column buffer. Tab 3 is cut off.
        #[tokio::test]
        async fn truncation_cuts_off_later_tabs() {
            let mut buf = Buffer::empty(Rect::new(0, 0, 12, 1));
            let tabs = make_tabs_with_labels(&[Some("a"), Some("b"), Some("c")]);
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            // " 1:a " = 5, sep = 1, " 2:b " = 5 => 11 chars. Tab 3
            // would start at col 11, but " 3:c " = 5 chars needs
            // col 11..15, and only col 11 is available (width=12).
            // One char of tab 3's text fits.
            assert!(
                text.contains(" 2:b "),
                "tab 2 must fit in 12 cols; got: {:?}",
                text
            );
            // Verify the boundary column has the trailing space of tab 2,
            // not the leading space of tab 3
            assert_eq!(
                buf[(11, 0)].symbol(),
                " ",
                "col 11 must be separator/trailing space, not tab 3 content"
            );
            assert!(
                !text.contains(" 3:c "),
                "tab 3's full text must NOT fit in 12 cols; got: {:?}",
                text
            );
        }

        /// Background fill: cells beyond the last tab text are
        /// cleared with the dark-gray background (not left as
        /// stale content from a previous frame).
        #[tokio::test]
        async fn background_fill_after_last_tab() {
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, 1));
            // Pre-fill row 0 with 'X' to simulate stale content.
            for x in 0..80 {
                buf[(x, 0)].set_symbol("X");
            }
            let tabs = make_tabs(1);
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            // After " 1 " (3 chars), the rest should be spaces.
            assert!(
                !text.contains('X'),
                "stale content must be cleared by background fill; got: {:?}",
                &text[..10.min(text.len())]
            );
            // Verify trailing cells have dark-gray background.
            let trail_s = cell_style(&buf, 10);
            assert_eq!(
                trail_s.bg,
                Some(Color::DarkGray),
                "trailing cells must have DarkGray background"
            );
        }

        /// Zero-width buffer: `render_tab_bar` must not panic when
        /// the buffer has width 0. The clear loop and tab-text loop
        /// both guard on `col < bar_width` / `x < bar_width`, so
        /// the body is a no-op. Pins the no-panic contract for a
        /// degenerate (zero-column) terminal.
        #[tokio::test]
        async fn zero_width_buffer_does_not_panic() {
            let mut buf = Buffer::empty(Rect::new(0, 0, 0, 1));
            let tabs = make_tabs(1);
            // Must not panic — the buffer is zero-width, all loops
            // are no-ops.
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            // Buffer is still empty (zero cells).
            assert_eq!(buf.area.width, 0);
        }

        /// Single-char buffer width: only 1 column is available.
        /// The leading space of the first tab's text (" 1 ") fits
        /// at col 0; the digit and trailing space are truncated.
        /// The single cell must have the active tab's Blue bg.
        #[tokio::test]
        async fn single_char_buffer_shows_leading_space() {
            let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
            let tabs = make_tabs(1);
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            // Only col 0 is available — the leading space of " 1 ".
            assert_eq!(buf[(0, 0)].symbol(), " ");
            let s = cell_style(&buf, 0);
            assert_eq!(
                s.bg,
                Some(Color::Blue),
                "single-char active tab must have Blue bg"
            );
            assert_eq!(
                s.fg,
                Some(Color::White),
                "single-char active tab must have White fg"
            );
        }

        /// Long label truncated at buffer edge: a label wider than
        /// the buffer is silently cut off mid-character. The cells
        /// that DO fit must carry the active style; trailing cells
        /// beyond the tab text must have the `DarkGray` background
        /// fill.
        #[tokio::test]
        async fn long_label_truncated_at_buffer_edge() {
            // Buffer is 12 cols wide. Tab text " 1:longlabel "
            // is 14 chars — 2 chars overflow.
            let mut buf = Buffer::empty(Rect::new(0, 0, 12, 1));
            let tabs = make_tabs_with_labels(&[Some("longlabel")]);
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            // First 12 chars of " 1:longlabel " are " 1:longlabe".
            let text = row_text(&buf);
            assert!(
                text.starts_with(" 1:longlabe"),
                "first 12 cols must be the truncated label; got: {:?}",
                &text[..text.len().min(14)]
            );
            // Col 11 (last col) is the char 'e' from "longlabe",
            // which is part of the active tab text.
            let s = cell_style(&buf, 11);
            assert_eq!(
                s.bg,
                Some(Color::Blue),
                "truncated label chars must still have active-tab Blue bg"
            );
            // The trailing space (col 13) and the "l" (col 12)
            // don't fit, so they are absent from the buffer.
            assert_eq!(text.len(), 12, "buffer width limits the output to 12 chars");
        }

        /// Multiple tabs with long labels: the second tab's label
        /// is truncated by the buffer edge. The separator and
        /// second tab's leading space must still render correctly
        /// for the portion that fits.
        #[tokio::test]
        async fn multi_tab_long_labels_truncated() {
            // 15 cols. Tab 1: " 1:ab " = 6 chars (col 0-5).
            // Separator: col 6. Tab 2: " 2:longname " starts at
            // col 7. 15 - 7 = 8 cols for tab 2, but " 2:longname "
            // is 12 chars, so only " 2:longn" (8 chars) fits.
            let mut buf = Buffer::empty(Rect::new(0, 0, 15, 1));
            let tabs = make_tabs_with_labels(&[Some("ab"), Some("longname")]);
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            assert!(
                text.starts_with(" 1:a"),
                "tab 1 must appear at start; got: {:?}",
                &text[..text.len().min(8)]
            );
            // Tab 2's text starts at col 7.
            let tab2_start = cell_style(&buf, 7);
            assert_eq!(
                tab2_start.bg,
                Some(Color::DarkGray),
                "tab 2 (inactive) must have DarkGray bg"
            );
            // Col 14 (last col) is inside tab 2's truncated text.
            let tab2_end = cell_style(&buf, 14);
            assert_eq!(
                tab2_end.bg,
                Some(Color::DarkGray),
                "truncated inactive tab chars must keep DarkGray bg"
            );
        }
        // ----------------------------------------------------------------
        // Multi-byte UTF-8 label tests.
        // ----------------------------------------------------------------

        /// Emoji label: earth globe emoji is 4 bytes but one
        /// Unicode scalar value. Tab text becomes ` 1:<emoji> `.
        /// Pins that emoji labels don't panic and the symbol is
        /// stored correctly in the buffer cell.
        #[tokio::test]
        async fn emoji_label_renders_without_panic() {
            let tabs = make_tabs_with_labels(&[Some("\u{1f30d}")]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            assert!(
                text.contains('\u{1f30d}'),
                "emoji must appear in rendered text; got: {text:?}"
            );
            // Active style on the emoji cell (col 3).
            let emoji_style = cell_style(&buf, 3);
            assert_eq!(
                emoji_style.bg,
                Some(Color::Blue),
                "emoji cell must have active tab Blue bg"
            );
        }

        /// CJK label: Japanese kanji is 9 bytes (3 per char) but
        /// 3 Unicode scalar values. Pins that CJK labels render
        /// without panicking and each character occupies one cell
        /// in the buffer.
        #[tokio::test]
        async fn cjk_label_renders_without_panic() {
            let tabs = make_tabs_with_labels(&[Some("\u{65e5}\u{672c}\u{8a9e}")]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            assert!(
                text.contains('\u{65e5}'),
                "first CJK char must appear in rendered text; got: {text:?}"
            );
            assert!(
                text.contains('\u{8a9e}'),
                "third CJK char must appear in rendered text; got: {text:?}"
            );
            // Style check on first CJK char (col 3).
            let cjk_style = cell_style(&buf, 3);
            assert_eq!(
                cjk_style.bg,
                Some(Color::Blue),
                "CJK char cell must have active tab Blue bg"
            );
        }

        /// Mixed ASCII + accented: e-acute (U+00E9) is 2 bytes
        /// but 1 column width and 1 Unicode scalar value. Pins
        /// that precomposed accented characters (common in
        /// European languages) render correctly.
        #[tokio::test]
        async fn mixed_ascii_multibyte_label_renders() {
            let tabs = make_tabs_with_labels(&[Some("caf\u{e9}")]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            assert!(
                text.starts_with(" 1:caf\u{e9} "),
                "mixed ASCII+accented label must render; got: {text:?}"
            );
            // Style on the accented char cell (col 5).
            let accent_style = cell_style(&buf, 5);
            assert_eq!(
                accent_style.bg,
                Some(Color::Blue),
                "accented char cell must have active tab Blue bg"
            );
        }

        /// Multi-tab mixed UTF-8 styles: emoji active tab + CJK
        /// inactive tab. Pins that styles are applied correctly
        /// to multi-byte character cells across active/inactive
        /// tabs.
        ///
        /// Layout: tab 0 " 1:<rocket> " (5 chars, cols 0-4),
        /// separator at col 5, tab 1 " 2:<CJK> " (6 chars,
        /// cols 6-11).
        #[tokio::test]
        async fn multi_tab_mixed_utf8_styles() {
            let tabs = make_tabs_with_labels(&[Some("\u{1f680}"), Some("\u{65e5}\u{672c}")]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 30, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            assert!(
                text.contains('\u{1f680}'),
                "emoji must appear in rendered text; got: {text:?}"
            );
            assert!(
                text.contains('\u{65e5}'),
                "CJK must appear in rendered text; got: {text:?}"
            );
            // Active tab style on emoji cell (col 3).
            let emoji_style = cell_style(&buf, 3);
            assert_eq!(
                emoji_style.bg,
                Some(Color::Blue),
                "active tab emoji must have Blue bg"
            );
            // Inactive tab style on first CJK char (col 9).
            let cjk_style = cell_style(&buf, 9);
            assert_eq!(
                cjk_style.bg,
                Some(Color::DarkGray),
                "inactive tab CJK char must have DarkGray bg"
            );
        }

        /// Truncation with multi-byte chars: a long emoji label
        /// that exceeds the buffer width must be truncated at the
        /// char boundary (not byte boundary). Pins that `.chars()`
        /// iteration and the col >= `bar_width` truncation guard
        /// work for non-ASCII text without panicking.
        ///
        /// Tab text is 9 chars (" 1:" + 5 emoji + " "). Buffer
        /// width 6: chars 0-5 fit, chars 6+ truncated.
        #[tokio::test]
        async fn truncation_with_emoji_label() {
            let tabs =
                make_tabs_with_labels(&[Some("\u{1f30d}\u{1f680}\u{1f389}\u{1f38a}\u{2728}")]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 6, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            assert!(
                text.starts_with(" 1:"),
                "must start with \" 1:\"; got: {text:?}"
            );
            // First 3 emoji fit (cols 3, 4, 5).
            assert!(
                text.contains('\u{1f30d}')
                    && text.contains('\u{1f680}')
                    && text.contains('\u{1f389}'),
                "first 3 emoji must appear; got: {text:?}"
            );
            // 4th and 5th emoji are truncated.
            assert!(
                !text.contains('\u{1f38a}') && !text.contains('\u{2728}'),
                "truncated emoji must not appear; got: {text:?}"
            );
        }

        /// Inactive tab with CJK label: verifies both bg and fg
        /// styles (`DarkGray` bg + `Gray` fg) are applied to
        /// multi-byte character cells, matching the ASCII
        /// inactive tab contract.
        #[tokio::test]
        async fn inactive_tab_with_cjk_label_has_dark_gray_style() {
            let tabs = make_tabs_with_labels(&[Some("a"), Some("\u{65e5}\u{672c}\u{8a9e}")]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 30, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            // Tab 1 " 1:a " = 5 chars + 1 separator = 6 cols.
            // Tab 2 starts at col 6: ' ' 6, '2' 7, ':' 8, CJK 9.
            let cjk_style = cell_style(&buf, 9);
            assert_eq!(
                cjk_style.bg,
                Some(Color::DarkGray),
                "inactive tab CJK char must have DarkGray bg"
            );
            assert_eq!(
                cjk_style.fg,
                Some(Color::Gray),
                "inactive tab CJK char must have Gray fg"
            );
        }

        // ----------------------------------------------------------------
        // Combining and zero-width Unicode character tests.
        // ----------------------------------------------------------------

        /// Combining character: "e" + U+0301 (COMBINING ACUTE ACCENT).
        /// In `.chars()` iteration these are two separate scalar
        /// values: the base letter and the combining mark. Each gets
        /// its own cell (col += 1 per char). Pins that combining
        /// sequences don't panic and the base letter + mark occupy
        /// two adjacent cells.
        ///
        /// Tab text: " 1:e<unk> " where <unk> is U+0301 — 8 chars
        /// (space, 1, colon, e, combining-acute, space).
        #[tokio::test]
        async fn combining_accent_does_not_panic() {
            // "e" + combining acute accent (U+0301)
            let label = "e\u{0301}";
            let tabs = make_tabs_with_labels(&[Some(label)]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            // The base 'e' is at col 3, combining mark at col 4.
            // Both cells must have the active tab style.
            let base_style = cell_style(&buf, 3);
            assert_eq!(
                base_style.bg,
                Some(Color::Blue),
                "base letter cell must have active tab Blue bg"
            );
            let combining_style = cell_style(&buf, 4);
            assert_eq!(
                combining_style.bg,
                Some(Color::Blue),
                "combining mark cell must have active tab Blue bg"
            );
            // The text must contain the base letter.
            assert!(
                text.contains('e'),
                "rendered text must contain base letter 'e'; got: {text:?}"
            );
        }

        /// Zero-width space (U+200B): a format character that has
        /// no visual width but is a valid Unicode scalar value.
        /// `.chars()` yields it as a separate char. In the buffer,
        /// it occupies one cell (col += 1). Pins that zero-width
        /// characters don't panic and the cell is styled.
        #[tokio::test]
        async fn zero_width_space_does_not_panic() {
            // Label: "a" + zero-width space + "b"
            let label = "a\u{200b}b";
            let tabs = make_tabs_with_labels(&[Some(label)]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            // Tab text: " 1:a<unk>b " = 9 chars.
            // 'a' at col 3, ZWS at col 4, 'b' at col 5.
            let zws_style = cell_style(&buf, 4);
            assert_eq!(
                zws_style.bg,
                Some(Color::Blue),
                "zero-width space cell must have active tab Blue bg"
            );
            // 'b' after the ZWS must also be styled.
            let b_style = cell_style(&buf, 5);
            assert_eq!(
                b_style.bg,
                Some(Color::Blue),
                "char after ZWS must have active tab Blue bg"
            );
        }

        /// Zero-width joiner (U+200D): used in emoji sequences
        /// (e.g. family emoji). As a standalone char in a label,
        /// it's a zero-width scalar that occupies one cell. Pins
        /// that ZWJ doesn't panic.
        #[tokio::test]
        async fn zero_width_joiner_in_label_does_not_panic() {
            let label = "a\u{200d}b";
            let tabs = make_tabs_with_labels(&[Some(label)]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            assert!(
                text.contains('a') && text.contains('b'),
                "base chars must appear around ZWJ; got: {text:?}"
            );
            // ZWJ cell at col 4 must have active tab style.
            let zwj_style = cell_style(&buf, 4);
            assert_eq!(
                zwj_style.bg,
                Some(Color::Blue),
                "ZWJ cell must have active tab Blue bg"
            );
        }

        /// Zero-width non-joiner (U+200C): another format character.
        /// Pins that ZWNJ doesn't panic and occupies its own cell.
        #[tokio::test]
        async fn zero_width_non_joiner_in_label_does_not_panic() {
            let label = "x\u{200c}y";
            let tabs = make_tabs_with_labels(&[Some(label)]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            assert!(
                text.contains('x') && text.contains('y'),
                "base chars must appear around ZWNJ; got: {text:?}"
            );
            // ZWNJ cell at col 4 must have active tab style.
            let zwnj_style = cell_style(&buf, 4);
            assert_eq!(
                zwnj_style.bg,
                Some(Color::Blue),
                "ZWNJ cell must have active tab Blue bg"
            );
        }

        /// Multiple combining marks on one base: "a" + U+0300
        /// (grave) + U+0301 (acute) = 3 chars in `.chars()`. Each
        /// gets its own cell. Pins that stacked combining marks
        /// don't panic and all 3 cells are styled.
        #[tokio::test]
        async fn stacked_combining_marks_do_not_panic() {
            // "a" + combining grave + combining acute
            let label = "a\u{0300}\u{0301}";
            let tabs = make_tabs_with_labels(&[Some(label)]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            // Tab text: " 1:a<unk><unk> " = 8 chars.
            // 'a' at col 3, first combining at col 4, second at col 5.
            for col in [3u16, 4, 5] {
                let style = cell_style(&buf, col);
                assert_eq!(
                    style.bg,
                    Some(Color::Blue),
                    "cell at col {col} must have active tab Blue bg"
                );
            }
        }

        /// Mixed combining + zero-width: label with base letter,
        /// combining mark, zero-width space, and ASCII. Pins that
        /// a complex Unicode mix doesn't panic and all cells get
        /// the correct style.
        #[tokio::test]
        async fn mixed_combining_and_zero_width_does_not_panic() {
            // "e" + combining acute + ZWS + "f"
            let label = "e\u{0301}\u{200b}f";
            let tabs = make_tabs_with_labels(&[Some(label)]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            // Tab text: " 1:e<unk><unk>f " = 10 chars.
            // 'e' at col 3, combining at col 4, ZWS at col 5, 'f' at col 6.
            for col in [3u16, 4, 5, 6] {
                let style = cell_style(&buf, col);
                assert_eq!(
                    style.bg,
                    Some(Color::Blue),
                    "cell at col {col} in mixed Unicode label must have Blue bg"
                );
            }
        }

        /// Combining mark on inactive tab: verifies the inactive
        /// style (`DarkGray` bg) is applied to combining character
        /// cells, not just the base letter.
        #[tokio::test]
        async fn inactive_tab_combining_mark_has_dark_gray_bg() {
            // Tab 1 (active): "a", Tab 2 (inactive): "e" + combining acute
            let tabs = make_tabs_with_labels(&[Some("a"), Some("e\u{0301}")]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            // Tab 1 " 1:a " = 5 chars + 1 separator = 6 cols.
            // Tab 2 starts at col 6: ' ' 6, '2' 7, ':' 8, 'e' 9, combining 10.
            let combining_style = cell_style(&buf, 10);
            assert_eq!(
                combining_style.bg,
                Some(Color::DarkGray),
                "inactive tab combining mark must have DarkGray bg"
            );
        }

        /// Truncation at combining sequence boundary: a label with
        /// many combining marks that exceeds the buffer width. The
        /// `.chars()` iteration truncates at the char boundary,
        /// which may split a base+combining pair. Pins that this
        /// doesn't panic.
        #[tokio::test]
        async fn truncation_splits_combining_sequence_without_panic() {
            // Label: 5 base+combining pairs = 10 chars.
            // Tab text: " 1:" + 10 chars + " " = 14 chars.
            // Buffer width 8: chars 0-7 fit.
            let pairs: String = "e\u{0301}".repeat(5);
            let tabs = make_tabs_with_labels(&[Some(pairs.as_str())]);
            let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
            render_tab_bar(&mut buf, &tabs, &cmdash_config::Theme::default());
            let text = row_text(&buf);
            assert!(
                text.starts_with(" 1:"),
                "must start with ' 1:'; got: {text:?}"
            );
        }
    }

    #[cfg(test)]
    mod config_hot_reload_tests {

        use cmdash::test_utils::make_isolated_test_dir;
        use std::time::Duration;

        /// Config hot-reload: write a config file, spawn a
        /// `ConfigWatcher` for it, modify the file with different
        /// keybinds, and assert the watcher's channel delivers a
        /// `ConfigReload` with the new keybinds.
        ///
        /// This test exercises the full hot-reload pipeline:
        /// file write -> notify event -> re-parse -> mpsc send ->
        /// channel receive -> keybind assertion.
        #[tokio::test]
        async fn config_hot_reload_detects_keybind_changes() {
            let _dir = make_isolated_test_dir("cmdash_hot_reload_test");
            let dir = std::env::temp_dir().join(format!(
                "cmdash_hot_reload_test_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let _ = std::fs::create_dir_all(&dir);
            let config_path = dir.join("config.kdl");

            // Step 1: write initial config with alt-q -> app.close.
            let initial = "\n            layout {\n                pane kind=shell label=\"a\"\n            }\n            keybinds {\n                bind \"alt-q\" action=\"app.close\"\n            }\n        ";
            std::fs::write(&config_path, initial).expect("write initial config");

            // Step 2: spawn the ConfigWatcher.
            let (_watcher, rx_opt) = super::ConfigWatcher::spawn(Some(config_path.as_ref()));
            let mut rx = rx_opt.expect("watcher must produce a receiver when path is Some");

            // Give the watcher time to initialize (it watches the
            // parent directory; the notify backend may need a tick).
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Step 3: overwrite the config with different keybinds
            // (alt-w -> pane.close instead of alt-q -> app.close).
            let updated = "\n            layout {\n                pane kind=shell label=\"a\"\n            }\n            keybinds {\n                bind \"alt-w\" action=\"pane.close\"\n            }\n        ";
            std::fs::write(&config_path, updated).expect("write updated config");

            // Step 4: wait for the watcher's debounce (500ms) plus
            // margin. The channel should deliver the re-parsed
            // config with the new keybinds.
            let result = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
            let reload = result
                .expect("watcher must deliver a ConfigReload within 5s")
                .expect("channel closed");

            // Step 5: assert the new keybinds round-tripped.
            // Exactly 1 keybind (the old alt-q -> app.close is gone).
            assert_eq!(
                reload.keybinds.len(),
                1,
                "reloaded config must have exactly 1 keybind; got: {}",
                reload.keybinds.len()
            );
            let kb = &reload.keybinds[0];
            // The new keybind should be alt-w -> pane.close.
            assert_eq!(
                kb.action,
                cmdash_config::KeyAction::PaneClose,
                "reloaded action must be PaneClose; got: {:?}",
                kb.action
            );
            // Verify the modifier is Alt and the key is 'w'.
            assert!(
                kb.mods.alt,
                "reloaded keybind modifier must include Alt; got: {:?}",
                kb.mods
            );
            assert!(
                matches!(kb.key, cmdash_config::KeyToken::Char('w')),
                "reloaded keybind key must be Char('w'); got: {:?}",
                kb.key
            );
        }

        /// Config hot-reload: modify the layout tree (single pane ->
        /// two-pane split) and assert the watcher delivers a
        /// `ConfigReload` whose `layout_root` resolves to 2 panes
        /// with correct labels. This pins the layout-change payload
        /// that `check_config_reload` compares against the active
        /// tree to decide whether to trigger a Wholesale reconcile.
        #[tokio::test]
        async fn config_hot_reload_detects_layout_changes() {
            use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
            let dir = make_isolated_test_dir("cmdash_hot_reload_layout_test");
            let _ = std::fs::create_dir_all(&dir);
            let config_path = dir.join("config.kdl");

            // Step 1: write initial config with a SINGLE pane.
            let initial = "\n            layout {\n                pane kind=shell label=\"solo\"\n            }\n        ";
            std::fs::write(&config_path, initial).expect("write initial config");

            // Step 2: spawn the ConfigWatcher.
            let (_watcher, rx_opt) = super::ConfigWatcher::spawn(Some(config_path.as_ref()));
            let mut rx = rx_opt.expect("watcher must produce a receiver");

            // Give the watcher time to initialize.
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Step 3: overwrite with a TWO-PANE split layout.
            let updated = "\n            layout {\n                split axis=horizontal ratio=0.5 {\n                    pane kind=shell label=\"left\"\n                    pane kind=shell label=\"right\"\n                }\n            }\n        ";
            std::fs::write(&config_path, updated).expect("write updated config");

            // Step 4: wait for the watcher's debounce + margin.
            let result = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;

            // Cleanup BEFORE assertions.
            let _ = std::fs::remove_file(&config_path);
            let _ = std::fs::remove_dir(&dir);

            let reload = result
                .expect("watcher must deliver a ConfigReload within 5s")
                .expect("channel closed");

            // Step 5: assert the new layout tree resolves to 2 panes.
            let new_root = reload
                .layout_root
                .expect("ConfigReload must carry a layout_root");
            let computed = ComputedLayout::compute(
                &new_root,
                LayoutRect {
                    x: 0,
                    y: 0,
                    w: 120,
                    h: 40,
                },
            )
            .expect("new layout must compute successfully");
            assert_eq!(
                computed.panes.len(),
                2,
                "new layout must resolve to 2 panes; got: {}",
                computed.panes.len()
            );
            assert_eq!(
                computed.panes[0].label.as_deref(),
                Some("left"),
                "pane 0 label must be 'left'"
            );
            assert_eq!(
                computed.panes[1].label.as_deref(),
                Some("right"),
                "pane 1 label must be 'right'"
            );
        }

        /// Debounce behavior: two rapid successive file writes
        /// (well within the 500ms debounce window) must collapse
        /// to a single `ConfigReload`. The watcher's debounce
        /// coalesces rapid edits so the tick loop isn't spammed
        /// with redundant reconciles. This test writes two
        /// configs with DIFFERENT keybinds in quick succession
        /// and asserts only ONE `ConfigReload` arrives, carrying
        /// the SECOND write's keybinds.
        #[tokio::test]
        async fn config_hot_reload_debounces_rapid_writes() {
            let dir = make_isolated_test_dir("cmdash_hot_reload_debounce_test");
            let _ = std::fs::create_dir_all(&dir);
            let config_path = dir.join("config.kdl");

            // Step 1: write initial config.
            let initial = "\n            layout {\n                pane kind=shell label=\"a\"\n            }\n            keybinds {\n                bind \"alt-q\" action=\"app.close\"\n            }\n        ";
            std::fs::write(&config_path, initial).expect("write initial config");

            // Step 2: spawn the ConfigWatcher.
            let (_watcher, rx_opt) = super::ConfigWatcher::spawn(Some(config_path.as_ref()));
            let mut rx = rx_opt.expect("watcher must produce a receiver");

            // Give the watcher time to initialize.
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Step 3: two rapid successive writes WITHIN the 500ms
            // debounce window. Both write the same keybind
            // (alt-e -> pane.focus.next) so the content assertion
            // is correct regardless of inotify coalescing behavior.
            // The "only one message" assertion pins the collapse.
            let first_update = "\n            layout {\n                pane kind=shell label=\"a\"\n            }\n            keybinds {\n                bind \"alt-e\" action=\"pane.focus.next\"\n            }\n        ";
            std::fs::write(&config_path, first_update).expect("write first update");
            // Sleep briefly so inotify delivers this as a separate
            // filesystem event. Without the gap, the kernel can
            // coalesce the two rapid writes into a single event,
            // yielding 0 reloads instead of 1. 50ms is well within
            // the 500ms debounce window.
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Second write (<< 500ms from first, so debounce
            // collapses both into one ConfigReload).
            let second_update = "\n            layout {\n                pane kind=shell label=\"a\"\n            }\n            keybinds {\n                bind \"alt-e\" action=\"pane.focus.next\"\n            }\n        ";
            std::fs::write(&config_path, second_update).expect("write second update");

            // Step 4: wait for the debounce window to expire and
            // the watcher to deliver.
            let result = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;

            // Step 5: assert no second ConfigReload arrives within
            // a short grace period (debounce = 500ms, so 300ms
            // after the first should be safe).
            tokio::time::sleep(Duration::from_millis(800)).await;
            let second_result = rx.try_recv();

            let reload = result
                .expect("watcher must deliver a ConfigReload within 5s")
                .expect("channel closed");
            assert!(
                second_result.is_err(),
                "debounce must collapse two rapid writes into one ConfigReload; \
             got a second: {:?}",
                second_result
            );

            // Step 6: the single ConfigReload must carry the
            // expected keybinds (alt-e -> pane.focus.next).
            // Both writes used the same keybind, so this is
            // correct regardless of which event was processed.
            assert_eq!(reload.keybinds.len(), 1);
            let kb = &reload.keybinds[0];
            assert_eq!(
                kb.action,
                cmdash_config::KeyAction::PaneFocusNext,
                "debounced reload must carry the SECOND write's action; got: {:?}",
                kb.action
            );
            assert!(
                matches!(kb.key, cmdash_config::KeyToken::Char('e')),
                "debounced reload must carry the SECOND write's key; got: {:?}",
                kb.key
            );
        }

        /// Invalid-config edge case: overwrite the config file with
        /// unparseable KDL and assert the watcher does NOT deliver
        /// a `ConfigReload` (the parse failure is logged and the
        /// previous config stays active). Then write a valid config
        /// and assert a `ConfigReload` DOES arrive, proving the
        /// watcher recovered.
        #[tokio::test]
        async fn config_hot_reload_ignores_invalid_config() {
            let dir = make_isolated_test_dir("cmdash_hot_reload_invalid_test");
            let _ = std::fs::create_dir_all(&dir);
            let config_path = dir.join("config.kdl");

            // Step 1: write a VALID initial config.
            let initial = "\n            layout {\n                pane kind=shell label=\"a\"\n            }\n            keybinds {\n                bind \"alt-q\" action=\"app.close\"\n            }\n        ";
            std::fs::write(&config_path, initial).expect("write initial config");

            // Step 2: spawn the ConfigWatcher.
            let (_watcher, rx_opt) = super::ConfigWatcher::spawn(Some(config_path.as_ref()));
            let mut rx = rx_opt.expect("watcher must produce a receiver");

            // Give the watcher time to initialize.
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Step 3: overwrite with INVALID KDL (bare garbage).
            std::fs::write(&config_path, "this is not valid KDL {{{")
                .expect("write invalid config");

            // Wait for the debounce window to expire. The watcher
            // should attempt to parse, fail, log a warning, and
            // NOT send a ConfigReload.
            tokio::time::sleep(Duration::from_secs(1)).await;

            // Step 4: assert NO ConfigReload arrived for the invalid edit.
            let invalid_result = rx.try_recv();
            assert!(
                invalid_result.is_err(),
                "invalid config must not produce a ConfigReload; got: {:?}",
                invalid_result
            );

            // Step 5: now write a VALID config with different keybinds.
            let valid_update = "\n            layout {\n                pane kind=shell label=\"a\"\n            }\n            keybinds {\n                bind \"alt-e\" action=\"pane.focus.next\"\n            }\n        ";
            std::fs::write(&config_path, valid_update).expect("write valid config");

            // Step 6: the watcher should deliver a ConfigReload for
            // the valid edit, proving it recovered from the invalid one.
            let result = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;

            let reload = result
                .expect("valid config after invalid must produce a ConfigReload")
                .expect("channel closed");
            assert_eq!(reload.keybinds.len(), 1);
            let kb = &reload.keybinds[0];
            assert_eq!(
                kb.action,
                cmdash_config::KeyAction::PaneFocusNext,
                "recovered reload must carry the valid config's action; got: {:?}",
                kb.action
            );
        }
    }

    #[cfg(test)]
    mod keyboard_protocol_tests {
        use super::*;

        // ------------------------------------------------------------------
        // Free-function helpers
        // ------------------------------------------------------------------

        #[test]
        fn kitty_key_code_maps_character_keys() {
            assert_eq!(kitty_key_code(&KeyCode::Char('a')), Some('a' as u32));
            assert_eq!(kitty_key_code(&KeyCode::Char('Z')), Some('Z' as u32));
            assert_eq!(kitty_key_code(&KeyCode::Char(' ')), Some(' ' as u32));
        }

        #[test]
        fn kitty_key_code_maps_special_keys() {
            assert_eq!(kitty_key_code(&KeyCode::Enter), Some(57351));
            assert_eq!(kitty_key_code(&KeyCode::Tab), Some(57352));
            assert_eq!(kitty_key_code(&KeyCode::Backspace), Some(57353));
            assert_eq!(kitty_key_code(&KeyCode::Esc), Some(57350));
            assert_eq!(kitty_key_code(&KeyCode::Delete), Some(57355));
            assert_eq!(kitty_key_code(&KeyCode::Left), Some(57356));
            assert_eq!(kitty_key_code(&KeyCode::Right), Some(57357));
            assert_eq!(kitty_key_code(&KeyCode::Up), Some(57358));
            assert_eq!(kitty_key_code(&KeyCode::Down), Some(57359));
            assert_eq!(kitty_key_code(&KeyCode::PageUp), Some(57360));
            assert_eq!(kitty_key_code(&KeyCode::PageDown), Some(57361));
            assert_eq!(kitty_key_code(&KeyCode::Home), Some(57362));
            assert_eq!(kitty_key_code(&KeyCode::End), Some(57363));
        }

        #[test]
        fn kitty_key_code_maps_function_keys() {
            assert_eq!(kitty_key_code(&KeyCode::F(1)), Some(57370));
            assert_eq!(kitty_key_code(&KeyCode::F(4)), Some(57373));
            assert_eq!(kitty_key_code(&KeyCode::F(12)), Some(57381));
        }

        #[test]
        fn kitty_key_code_returns_none_for_unsupported() {
            // Null has no Kitty protocol representation in this mapping.
            assert!(kitty_key_code(&KeyCode::Null).is_none());
        }

        #[test]
        fn kitty_modifiers_maps_individual_bits() {
            assert_eq!(kitty_modifiers(KeyModifiers::SHIFT), 1);
            assert_eq!(kitty_modifiers(KeyModifiers::ALT), 2);
            assert_eq!(kitty_modifiers(KeyModifiers::CONTROL), 4);
            assert_eq!(kitty_modifiers(KeyModifiers::SUPER), 8);
        }

        #[test]
        fn kitty_modifiers_combines_bits() {
            let mods = KeyModifiers::SHIFT | KeyModifiers::CONTROL;
            assert_eq!(kitty_modifiers(mods), 5);

            let all = KeyModifiers::SHIFT
                | KeyModifiers::ALT
                | KeyModifiers::CONTROL
                | KeyModifiers::SUPER;
            assert_eq!(kitty_modifiers(all), 15);
        }

        #[test]
        fn kitty_modifiers_empty_is_zero() {
            assert_eq!(kitty_modifiers(KeyModifiers::empty()), 0);
        }

        #[test]
        fn kitty_event_type_maps_kinds() {
            assert_eq!(kitty_event_type(KeyEventKind::Press), 1);
            assert_eq!(kitty_event_type(KeyEventKind::Repeat), 2);
            assert_eq!(kitty_event_type(KeyEventKind::Release), 3);
        }

        #[test]
        fn encode_kitty_key_event_basic_press() {
            let bytes = encode_kitty_key_event(
                &KeyCode::Char('a'),
                KeyModifiers::empty(),
                KeyEventKind::Press,
            )
            .expect("encodeable key");
            assert_eq!(bytes, b"\x1b[97u");
        }

        #[test]
        fn encode_kitty_key_event_with_modifiers() {
            let bytes = encode_kitty_key_event(
                &KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
            )
            .expect("encodeable key");
            assert_eq!(bytes, b"\x1b[97;4u");

            let bytes = encode_kitty_key_event(
                &KeyCode::Char('a'),
                KeyModifiers::SHIFT | KeyModifiers::ALT,
                KeyEventKind::Press,
            )
            .expect("encodeable key");
            assert_eq!(bytes, b"\x1b[97;3u");
        }

        #[test]
        fn encode_kitty_key_event_release_requires_event_type() {
            let bytes = encode_kitty_key_event(
                &KeyCode::Char('a'),
                KeyModifiers::empty(),
                KeyEventKind::Release,
            )
            .expect("encodeable key");
            assert_eq!(bytes, b"\x1b[97;0:3u");

            let bytes = encode_kitty_key_event(
                &KeyCode::Char('a'),
                KeyModifiers::SHIFT,
                KeyEventKind::Release,
            )
            .expect("encodeable key");
            assert_eq!(bytes, b"\x1b[97;1:3u");
        }

        #[test]
        fn encode_kitty_key_event_repeat() {
            let bytes = encode_kitty_key_event(
                &KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                KeyEventKind::Repeat,
            )
            .expect("encodeable key");
            assert_eq!(bytes, b"\x1b[97;4:2u");
        }

        #[test]
        fn encode_kitty_key_event_returns_none_for_unsupported() {
            assert!(encode_kitty_key_event(
                &KeyCode::Null,
                KeyModifiers::empty(),
                KeyEventKind::Press
            )
            .is_none());
        }

        #[test]
        fn event_to_bytes_maps_common_keys() {
            assert_eq!(event_to_bytes(KeyCode::Enter), Some(b"\r".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::Backspace), Some(b"\x7f".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::Tab), Some(b"\t".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::Esc), Some(b"\x1b".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::Up), Some(b"\x1b[A".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::Down), Some(b"\x1b[B".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::Right), Some(b"\x1b[C".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::Left), Some(b"\x1b[D".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::Home), Some(b"\x1b[H".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::End), Some(b"\x1b[F".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::PageUp), Some(b"\x1b[5~".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::PageDown), Some(b"\x1b[6~".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::Delete), Some(b"\x1b[3~".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::F(1)), Some(b"\x1b[OP".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::F(2)), Some(b"\x1b[OQ".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::F(3)), Some(b"\x1b[OR".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::F(4)), Some(b"\x1b[OS".to_vec()));
        }

        #[test]
        fn event_to_bytes_maps_characters() {
            assert_eq!(event_to_bytes(KeyCode::Char('a')), Some(b"a".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::Char('A')), Some(b"A".to_vec()));
            assert_eq!(event_to_bytes(KeyCode::Char(' ')), Some(b" ".to_vec()));
        }

        #[test]
        fn event_to_bytes_returns_none_for_unsupported() {
            assert!(event_to_bytes(KeyCode::F(5)).is_none());
            assert!(event_to_bytes(KeyCode::Null).is_none());
        }

        // ------------------------------------------------------------------
        // TickContext state management
        // ------------------------------------------------------------------

        #[tokio::test]
        async fn sync_host_keyboard_flags_computes_union() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);

            ctx.pane_keyboard_flags.insert(PaneLayerId(1), 0b0000_0001);
            ctx.pane_keyboard_flags.insert(PaneLayerId(2), 0b0000_0010);
            ctx.sync_host_keyboard_flags();

            assert_eq!(ctx.host_keyboard_flags, 0b0000_0011);
            assert!(ctx.host_keyboard_pushed);
        }

        #[tokio::test]
        async fn sync_host_keyboard_flags_pops_when_union_drops_to_zero() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);

            // First push some flags.
            ctx.pane_keyboard_flags.insert(PaneLayerId(1), 0b0000_0001);
            ctx.sync_host_keyboard_flags();
            assert!(ctx.host_keyboard_pushed);

            // Remove all pane flags and re-sync.
            ctx.pane_keyboard_flags.clear();
            ctx.sync_host_keyboard_flags();

            assert_eq!(ctx.host_keyboard_flags, 0);
            assert!(!ctx.host_keyboard_pushed);
        }

        #[tokio::test]
        async fn sync_host_keyboard_flags_noop_when_union_unchanged() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);

            ctx.pane_keyboard_flags.insert(PaneLayerId(1), 0b0000_0001);
            ctx.sync_host_keyboard_flags();
            let pushed_once = ctx.host_keyboard_pushed;

            // Calling sync again with the same union should not change state.
            ctx.sync_host_keyboard_flags();
            assert_eq!(ctx.host_keyboard_flags, 0b0000_0001);
            assert_eq!(ctx.host_keyboard_pushed, pushed_once);
        }

        #[tokio::test]
        async fn sync_host_keyboard_flags_noop_when_host_unsupported() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);
            ctx.graphics = GraphicsState::new_with_caps(
                cmdash::graphics::Metrics::default(),
                (80, 24),
                TermCapabilities {
                    graphics: GraphicsProtocol::TextOnly,
                    kitty_keyboard: false,
                    focus_events: false,
                    bracketed_paste: false,
                    true_color: false,
                    color_256: false,
                    queries: false,
                },
            );

            ctx.pane_keyboard_flags.insert(PaneLayerId(1), 0b0000_0001);
            ctx.sync_host_keyboard_flags();

            assert_eq!(ctx.host_keyboard_flags, 0);
            assert!(!ctx.host_keyboard_pushed);
        }

        #[tokio::test]
        async fn sync_host_bracketed_paste_noop_when_host_unsupported() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);
            ctx.graphics = GraphicsState::new_with_caps(
                cmdash::graphics::Metrics::default(),
                (80, 24),
                TermCapabilities {
                    graphics: GraphicsProtocol::TextOnly,
                    kitty_keyboard: false,
                    focus_events: false,
                    bracketed_paste: false,
                    true_color: false,
                    color_256: false,
                    queries: false,
                },
            );

            ctx.pane_bracketed_paste.insert(PaneLayerId(1), true);
            ctx.sync_host_bracketed_paste();

            assert!(!ctx.host_bracketed_paste);
            assert!(!ctx.host_bracketed_paste_pushed);
        }

        #[tokio::test]
        async fn sync_host_focus_reporting_noop_when_host_unsupported() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);
            ctx.graphics = GraphicsState::new_with_caps(
                cmdash::graphics::Metrics::default(),
                (80, 24),
                TermCapabilities {
                    graphics: GraphicsProtocol::TextOnly,
                    kitty_keyboard: false,
                    focus_events: false,
                    bracketed_paste: false,
                    true_color: false,
                    color_256: false,
                    queries: false,
                },
            );

            ctx.pane_focus_reporting.insert(PaneLayerId(1), true);
            ctx.sync_host_focus_reporting();

            assert!(!ctx.host_focus_reporting);
            assert!(!ctx.host_focus_reporting_pushed);
        }

        #[tokio::test]
        async fn prepare_paste_bytes_forwards_raw_when_host_bracketed_paste_unsupported() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);
            ctx.graphics = GraphicsState::new_with_caps(
                cmdash::graphics::Metrics::default(),
                (80, 24),
                TermCapabilities {
                    graphics: GraphicsProtocol::TextOnly,
                    kitty_keyboard: false,
                    focus_events: false,
                    bracketed_paste: false,
                    true_color: false,
                    color_256: false,
                    queries: false,
                },
            );

            // Pane requests bracketed paste, but host does not support it.
            ctx.pane_bracketed_paste
                .insert(ctx.runners[0].layer_id(), true);
            let bytes = ctx.prepare_paste_bytes("hello");
            assert_eq!(bytes, b"hello");
        }

        #[tokio::test]
        async fn pop_host_keyboard_flags_is_idempotent() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);

            ctx.host_keyboard_pushed = true;
            ctx.pop_host_keyboard_flags();
            assert!(!ctx.host_keyboard_pushed);

            // Second pop should be a no-op and not panic.
            ctx.pop_host_keyboard_flags();
            assert!(!ctx.host_keyboard_pushed);
        }

        #[tokio::test]
        async fn pop_host_keyboard_flags_noop_when_not_pushed() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);

            ctx.host_keyboard_pushed = false;
            ctx.pop_host_keyboard_flags();
            assert!(!ctx.host_keyboard_pushed);
        }

        #[tokio::test]
        async fn update_keyboard_flags_from_snapshots_ignores_widget_runners() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);

            // The single runner in setup_run_loop_ctx is a widget runner.
            assert!(ctx.runners[0].is_widget());

            let snapshot = cmdash_pty::PaneTerminalState {
                grid: cmdash_pty::TextGrid::new(80, 24),
                cols: 80,
                rows: 24,
                pending_events: vec![cmdash_pty::PaneEvent::KeyboardEnhancement {
                    flags: 0b0000_0111,
                }],
            };

            ctx.update_keyboard_flags_from_snapshots(&[Some(snapshot)]);

            // Widget runners should not contribute keyboard flags.
            assert!(ctx.pane_keyboard_flags.is_empty());
            assert_eq!(ctx.host_keyboard_flags, 0);
        }
    }

    #[cfg(test)]
    mod test_helpers {
        use cmdash_pty::{PaneLayerId, PanePtyOps, PaneTerminalState, PtyError, TextGrid};

        /// Stub PTY backend used by integration tests to avoid spawning real
        /// child processes. The stub returns valid empty snapshots and can
        /// optionally record bytes written via `PanePtyOps::write`.
        pub struct StubPty {
            layer_id: PaneLayerId,
            focus_reporting: bool,
            written: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        }

        impl StubPty {
            pub fn new(layer_id: PaneLayerId) -> Self {
                Self {
                    layer_id,
                    focus_reporting: false,
                    written: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                }
            }

            /// Create a stub that records every byte payload written through
            /// `PanePtyOps::write`. The returned `Arc<Mutex<...>>` can be
            /// inspected after the stub has been moved into a `PaneRunner`.
            pub fn new_recording(
                layer_id: PaneLayerId,
            ) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
                let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let stub = Self {
                    layer_id,
                    focus_reporting: false,
                    written: std::sync::Arc::clone(&written),
                };
                (stub, written)
            }

            /// Create a stub with focus reporting enabled that also records
            /// every byte payload written through `PanePtyOps::write`.
            pub fn with_focus_reporting_recording(
                layer_id: PaneLayerId,
            ) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
                let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let stub = Self {
                    layer_id,
                    focus_reporting: true,
                    written: std::sync::Arc::clone(&written),
                };
                (stub, written)
            }
        }

        impl PanePtyOps for StubPty {
            fn layer_id(&self) -> PaneLayerId {
                self.layer_id
            }
            fn resize(&mut self, _cols: u16, _rows: u16) -> Result<(), PtyError> {
                Ok(())
            }
            fn write(&mut self, bytes: &[u8]) -> Result<usize, PtyError> {
                self.written.lock().unwrap().push(bytes.to_vec());
                Ok(bytes.len())
            }
            fn advance(&mut self, _bytes: &[u8]) -> Result<(), PtyError> {
                Ok(())
            }
            fn snapshot(&mut self) -> PaneTerminalState {
                PaneTerminalState {
                    grid: TextGrid::new(80, 24),
                    cols: 80,
                    rows: 24,
                    pending_events: Vec::new(),
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
                self.focus_reporting
            }
            fn scrollback_up(&mut self, _n: usize) {}
            fn scrollback_down(&mut self, _n: usize) {}
            fn scrollback_reset(&mut self) {}
            fn in_scrollback(&self) -> bool {
                false
            }
            fn in_alternate_screen(&self) -> bool {
                false
            }
        }
    }

    #[cfg(test)]
    mod bracketed_paste_tests {
        use super::test_helpers::StubPty;
        use super::*;

        #[tokio::test]
        async fn prepare_paste_bytes_wraps_when_bracketed_paste_enabled() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);
            let layer_id = ctx.runners[0].layer_id();
            ctx.pane_bracketed_paste.insert(layer_id, true);

            let bytes = ctx.prepare_paste_bytes("hello");

            assert_eq!(
                bytes,
                b"\x1b[200~hello\x1b[201~".to_vec(),
                "pasted text should be wrapped in bracketed-paste delimiters"
            );
        }

        #[tokio::test]
        async fn prepare_paste_bytes_forwards_raw_when_bracketed_paste_disabled() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);
            let layer_id = ctx.runners[0].layer_id();
            ctx.pane_bracketed_paste.insert(layer_id, false);

            let bytes = ctx.prepare_paste_bytes("hello");

            assert_eq!(
                bytes,
                b"hello".to_vec(),
                "pasted text should be forwarded raw when bracketed paste is disabled"
            );
        }

        #[tokio::test]
        async fn handle_paste_skips_widget_runners() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);
            // setup_run_loop_ctx creates a widget runner.
            assert!(ctx.runners[0].is_widget());

            // Should not panic and should not write anything.
            ctx.handle_paste("hello");
        }

        /// Integration test: the host terminal's bracketed-paste state is
        /// the *union* of all live pane requests, not a property of the
        /// focused pane. Focus changes must not disable bracketed paste
        /// while any pane still has it enabled.
        #[tokio::test]
        async fn host_bracketed_paste_union_across_focus_changes() {
            use cmdash_pty::{PaneEvent, PaneTerminalState, TextGrid};

            let kdl = r#"
                layout {
                    split axis=horizontal ratio=0.5 {
                        pane kind=shell label="left"
                        pane kind=shell label="right"
                    }
                }
            "#;
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let (mut ctx, _layout_root, _last_area) =
                super::input_tests::setup_fixture_ctx_with_runner(
                    kdl,
                    0,
                    Router::new(vec![]),
                    |pane, layer_id, close_tx| {
                        PaneRunner::with_pty_for_test(
                            pane,
                            layer_id,
                            Box::new(StubPty::new(layer_id)),
                            Some(close_tx),
                        )
                    },
                    LayoutRect {
                        x: 0,
                        y: 0,
                        w: 80,
                        h: 24,
                    },
                    &mut terminal,
                );

            assert_eq!(ctx.runners.len(), 2, "fixture must provide two panes");
            let layer_a = ctx.runners[0].layer_id();
            let layer_b = ctx.runners[1].layer_id();

            // Pane A enables bracketed paste; pane B does not.
            let snapshots = vec![
                Some(PaneTerminalState {
                    grid: TextGrid::new(80, 24),
                    cols: 80,
                    rows: 24,
                    pending_events: vec![PaneEvent::BracketedPaste { enabled: true }],
                }),
                Some(PaneTerminalState {
                    grid: TextGrid::new(80, 24),
                    cols: 80,
                    rows: 24,
                    pending_events: vec![],
                }),
            ];
            ctx.update_bracketed_paste_from_snapshots(&snapshots);
            ctx.sync_host_bracketed_paste();

            assert_eq!(
                ctx.pane_bracketed_paste.get(&layer_a),
                Some(&true),
                "pane A should report bracketed paste enabled"
            );
            assert_eq!(
                ctx.pane_bracketed_paste.get(&layer_b),
                None,
                "pane B should have no bracketed-paste state yet"
            );
            assert!(
                ctx.host_bracketed_paste,
                "host bracketed paste should be enabled when any pane requests it"
            );

            // Focus moves to pane B, which has not requested bracketed paste.
            // Host state must remain enabled because it is a union across all panes.
            ctx.apply_action_full(KeyAction::PaneFocusNext);
            assert_eq!(ctx.focus, 1, "focus should move to pane B");
            assert!(
                ctx.host_bracketed_paste,
                "host bracketed paste must stay enabled across focus changes when any pane is active"
            );

            // Now pane A disables bracketed paste. With no pane requesting it,
            // the host should disable.
            let snapshots = vec![
                Some(PaneTerminalState {
                    grid: TextGrid::new(80, 24),
                    cols: 80,
                    rows: 24,
                    pending_events: vec![PaneEvent::BracketedPaste { enabled: false }],
                }),
                Some(PaneTerminalState {
                    grid: TextGrid::new(80, 24),
                    cols: 80,
                    rows: 24,
                    pending_events: vec![],
                }),
            ];
            ctx.update_bracketed_paste_from_snapshots(&snapshots);
            ctx.sync_host_bracketed_paste();

            assert_eq!(
                ctx.pane_bracketed_paste.get(&layer_a),
                Some(&false),
                "pane A should report bracketed paste disabled"
            );
            assert!(
                !ctx.host_bracketed_paste,
                "host bracketed paste should disable when no pane requests it"
            );
        }
    }

    #[cfg(test)]
    mod focus_reporting_tests {
        use super::test_helpers::StubPty;
        use super::*;
        use cmdash_pty::{PaneEvent, PaneTerminalState, TextGrid};

        /// Recorder shared between `StubPty` and the test assertions.
        type Recorder = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

        /// Build a single-pane `PaneTerminalState` snapshot that reports the
        /// requested focus-reporting state.
        fn focus_reporting_enabled_snapshot(enabled: bool) -> Option<PaneTerminalState> {
            Some(PaneTerminalState {
                grid: TextGrid::new(80, 24),
                cols: 80,
                rows: 24,
                pending_events: vec![PaneEvent::FocusReporting { enabled }],
            })
        }

        #[tokio::test]
        async fn update_focus_reporting_from_snapshots_collects_state() {
            let kdl = r#"
                layout {
                    pane kind=shell label="focus-test"
                }
            "#;
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let (mut ctx, _layout_root, _last_area) =
                super::input_tests::setup_fixture_ctx_with_runner(
                    kdl,
                    0,
                    Router::new(vec![]),
                    |pane, layer_id, close_tx| {
                        PaneRunner::with_pty_for_test(
                            pane,
                            layer_id,
                            Box::new(StubPty::new(layer_id)),
                            Some(close_tx),
                        )
                    },
                    LayoutRect {
                        x: 0,
                        y: 0,
                        w: 80,
                        h: 24,
                    },
                    &mut terminal,
                );
            let layer_id = ctx.runners[0].layer_id();

            let snapshots = vec![Some(PaneTerminalState {
                grid: TextGrid::new(80, 24),
                cols: 80,
                rows: 24,
                pending_events: vec![PaneEvent::FocusReporting { enabled: true }],
            })];
            ctx.update_focus_reporting_from_snapshots(&snapshots);

            assert_eq!(
                ctx.pane_focus_reporting.get(&layer_id),
                Some(&true),
                "focus reporting should be enabled for the pane"
            );
            assert!(
                ctx.host_focus_reporting,
                "host focus reporting should be enabled when any pane requests it"
            );
        }

        #[tokio::test]
        async fn host_focus_event_forwards_to_focused_pane() {
            let kdl = r#"
                layout {
                    pane kind=shell label="focus-test"
                }
            "#;
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let (mut ctx, _layout_root, _last_area) =
                super::input_tests::setup_fixture_ctx_with_runner(
                    kdl,
                    0,
                    Router::new(vec![]),
                    |pane, layer_id, close_tx| {
                        PaneRunner::with_pty_for_test(
                            pane,
                            layer_id,
                            Box::new(StubPty::new(layer_id)),
                            Some(close_tx),
                        )
                    },
                    LayoutRect {
                        x: 0,
                        y: 0,
                        w: 80,
                        h: 24,
                    },
                    &mut terminal,
                );
            let layer_id = ctx.runners[0].layer_id();
            ctx.pane_focus_reporting.insert(layer_id, true);
            ctx.host_focus_reporting = true;

            // The runner is a widget by default in setup_run_loop_ctx, so we
            // cannot easily observe the written bytes. Instead, verify the
            // method does not panic and that the host focus state is tracked.
            ctx.handle_event_full(&crossterm::event::Event::FocusGained);
            assert!(ctx.host_focused, "host should be marked as focused");

            ctx.handle_event_full(&crossterm::event::Event::FocusLost);
            assert!(!ctx.host_focused, "host should be marked as unfocused");
        }

        #[tokio::test]
        async fn host_focus_event_skips_pane_without_focus_reporting() {
            let kdl = r#"
                layout {
                    pane kind=shell label="focus-test"
                }
            "#;
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let (mut ctx, _layout_root, _last_area) =
                super::input_tests::setup_fixture_ctx_with_runner(
                    kdl,
                    0,
                    Router::new(vec![]),
                    |pane, layer_id, close_tx| {
                        PaneRunner::with_pty_for_test(
                            pane,
                            layer_id,
                            Box::new(StubPty::new(layer_id)),
                            Some(close_tx),
                        )
                    },
                    LayoutRect {
                        x: 0,
                        y: 0,
                        w: 80,
                        h: 24,
                    },
                    &mut terminal,
                );
            // Pane has not requested focus reporting.
            ctx.handle_event_full(&crossterm::event::Event::FocusGained);
            assert!(ctx.host_focused);
            ctx.handle_event_full(&crossterm::event::Event::FocusLost);
            assert!(!ctx.host_focused);
        }

        /// Build a `TickContext` with a single pane backed by a recording
        /// `StubPty`. `pane_focus_reporting` initializes both the stub's
        /// `focus_reporting_enabled` state and the context's
        /// `pane_focus_reporting` / `host_focus_reporting` flags. Returns the
        /// context, the recorder, and the pane's layer id.
        fn setup_focus_recording_test<'a>(
            terminal: &'a mut ratatui::Terminal<ratatui::backend::TestBackend>,
            label: &str,
            host_focused: bool,
            pane_focus_reporting: bool,
        ) -> (
            TickContext<'a, ratatui::backend::TestBackend>,
            Recorder,
            PaneLayerId,
        ) {
            let kdl = format!(
                r#"
                layout {{
                    pane kind=shell label="{}"
                }}
            "#,
                label
            );
            let recorder: std::cell::RefCell<Option<Recorder>> = std::cell::RefCell::new(None);
            let (mut ctx, _layout_root, _last_area) =
                super::input_tests::setup_fixture_ctx_with_runner(
                    &kdl,
                    0,
                    Router::new(vec![]),
                    |pane, layer_id, close_tx| {
                        let (stub, rec) = if pane_focus_reporting {
                            StubPty::with_focus_reporting_recording(layer_id)
                        } else {
                            StubPty::new_recording(layer_id)
                        };
                        *recorder.borrow_mut() = Some(rec);
                        PaneRunner::with_pty_for_test(
                            pane,
                            layer_id,
                            Box::new(stub),
                            Some(close_tx),
                        )
                    },
                    LayoutRect {
                        x: 0,
                        y: 0,
                        w: 80,
                        h: 24,
                    },
                    terminal,
                );
            ctx.host_focused = host_focused;
            let layer_id = ctx.runners[0].layer_id();
            ctx.pane_focus_reporting
                .insert(layer_id, pane_focus_reporting);
            ctx.host_focus_reporting = pane_focus_reporting;
            let recorder = recorder.into_inner().expect("recorder was set");
            (ctx, recorder, layer_id)
        }

        /// End-to-end: when the focused pane has requested focus reporting,
        /// a host `FocusGained` event is forwarded as `CSI I` (ESC [ I).
        #[tokio::test]
        async fn focus_gained_forwards_csi_i_to_focused_pane() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let (mut ctx, recorder, _layer_id) =
                setup_focus_recording_test(&mut terminal, "focus-gained-test", true, true);

            ctx.handle_event_full(&crossterm::event::Event::FocusGained);

            let written = recorder.lock().unwrap();
            assert_eq!(written.len(), 1, "exactly one write should be recorded");
            assert_eq!(
                written[0], b"[I",
                "FocusGained should forward CSI I to the focused pane"
            );
        }

        /// End-to-end: when the focused pane has requested focus reporting,
        /// a host `FocusLost` event is forwarded as `CSI O` (ESC [ O).
        #[tokio::test]
        async fn focus_lost_forwards_csi_o_to_focused_pane() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let (mut ctx, recorder, _layer_id) =
                setup_focus_recording_test(&mut terminal, "focus-lost-test", true, true);

            ctx.handle_event_full(&crossterm::event::Event::FocusLost);

            let written = recorder.lock().unwrap();
            assert_eq!(written.len(), 1, "exactly one write should be recorded");
            assert_eq!(
                written[0], b"[O",
                "FocusLost should forward CSI O to the focused pane"
            );
        }

        /// End-to-end: focus events must NOT be forwarded to a pane that has
        /// not requested focus reporting, even when the host terminal reports
        /// a focus change.
        #[tokio::test]
        async fn focus_event_not_forwarded_when_focus_reporting_disabled() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let (mut ctx, recorder, _layer_id) =
                setup_focus_recording_test(&mut terminal, "focus-disabled-test", true, false);

            ctx.handle_event_full(&crossterm::event::Event::FocusGained);
            ctx.handle_event_full(&crossterm::event::Event::FocusLost);

            let written = recorder.lock().unwrap();
            assert!(
                written.is_empty(),
                "no focus events should be forwarded when focus reporting is disabled"
            );
        }

        /// When a pane enables focus reporting, cmdash must immediately
        /// report the current host focus state to that pane. If the host
        /// is focused, the pane receives `CSI I` (`ESC [ I`).
        #[tokio::test]
        async fn initial_focus_state_sent_when_pane_enables_focus_reporting() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let (mut ctx, recorder, layer_id) =
                setup_focus_recording_test(&mut terminal, "focus-init-test", true, false);

            ctx.update_focus_reporting_from_snapshots(&[focus_reporting_enabled_snapshot(true)]);

            let written = recorder.lock().unwrap();
            assert_eq!(
                written.as_slice(),
                vec![b"[I".to_vec()].as_slice(),
                "newly enabled pane should immediately receive CSI I when host is focused"
            );
            assert_eq!(
                ctx.pane_focus_reporting.get(&layer_id),
                Some(&true),
                "pane focus reporting state should be recorded"
            );
        }

        /// When a pane enables focus reporting while the host is not
        /// focused, the pane immediately receives `CSI O` (`ESC [ O`).
        #[tokio::test]
        async fn initial_focus_state_sends_focus_out_when_host_unfocused() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let (mut ctx, recorder, _layer_id) =
                setup_focus_recording_test(&mut terminal, "focus-init-out-test", false, false);

            ctx.update_focus_reporting_from_snapshots(&[focus_reporting_enabled_snapshot(true)]);

            let written = recorder.lock().unwrap();
            assert_eq!(
                written.as_slice(),
                vec![b"[O".to_vec()].as_slice(),
                "newly enabled pane should immediately receive CSI O when host is unfocused"
            );
        }

        /// Re-enabling focus reporting (e.g. the child emitted the enable
        /// sequence again) must not send a duplicate initial focus event.
        #[tokio::test]
        async fn re_enabling_focus_reporting_does_not_resend_initial_state() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let (mut ctx, recorder, _layer_id) =
                setup_focus_recording_test(&mut terminal, "focus-reenable-test", true, false);

            ctx.update_focus_reporting_from_snapshots(&[focus_reporting_enabled_snapshot(true)]);

            {
                let mut written = recorder.lock().unwrap();
                written.clear();
            }

            ctx.update_focus_reporting_from_snapshots(&[focus_reporting_enabled_snapshot(true)]);

            let written = recorder.lock().unwrap();
            assert!(
                written.is_empty(),
                "re-enabling focus reporting should not send a duplicate initial focus event"
            );
        }
    }

    #[cfg(test)]
    mod render_and_text_tests {
        use super::*;

        /// Build a minimal context with a custom widget runner (no real PTY)
        /// so `render_cell_body` can be exercised against a `TestBackend`
        /// without spawning shell children.
        /// `extract_selected_text` returns the character under the
        /// cursor when no selection anchor is set.
        #[test]
        fn extract_selected_text_returns_cursor_cell_without_selection() {
            let mut grid = cmdash_pty::TextGrid::new(10, 5);
            // Put a known character at the origin.
            grid.put_char(0, 0, 'X');
            let text = extract_selected_text(&grid, 0, 0, None);
            assert_eq!(text, "X");
        }

        /// `extract_selected_text` returns the rectangular selection
        /// between the anchor and the cursor.
        #[test]
        fn extract_selected_text_returns_rectangular_selection() {
            let mut grid = cmdash_pty::TextGrid::new(10, 5);
            // Row 1: " hello" (leading space).
            grid.put_char(1, 1, ' ');
            for (i, c) in "hello".chars().enumerate() {
                grid.put_char(2 + i as u16, 1, c);
            }
            let text = extract_selected_text(&grid, 6, 1, Some((1, 1)));
            assert_eq!(text, " hello");
        }
    }

    #[cfg(test)]
    mod copy_mode_snapshot_tests {
        use super::*;
        use cmdash_pty::{PaneLayerId, PanePtyOps, PaneTerminalState, PtyError, TextGrid};

        /// Stub PTY that returns a snapshot with a unique marker in the grid.
        struct MarkedStubPty {
            layer_id: PaneLayerId,
            marker: char,
        }

        impl MarkedStubPty {
            fn new(layer_id: PaneLayerId, marker: char) -> Self {
                Self { layer_id, marker }
            }
        }

        impl PanePtyOps for MarkedStubPty {
            fn layer_id(&self) -> PaneLayerId {
                self.layer_id
            }
            fn resize(&mut self, _cols: u16, _rows: u16) -> Result<(), PtyError> {
                Ok(())
            }
            fn write(&mut self, _bytes: &[u8]) -> Result<usize, PtyError> {
                Ok(0)
            }
            fn advance(&mut self, _bytes: &[u8]) -> Result<(), PtyError> {
                Ok(())
            }
            fn snapshot(&mut self) -> PaneTerminalState {
                let mut grid = TextGrid::new(10, 5);
                grid.put_char(0, 0, self.marker);
                PaneTerminalState {
                    grid,
                    cols: 10,
                    rows: 5,
                    pending_events: Vec::new(),
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
            fn scrollback_up(&mut self, _n: usize) {}
            fn scrollback_down(&mut self, _n: usize) {}
            fn scrollback_reset(&mut self) {}
            fn in_scrollback(&self) -> bool {
                false
            }
            fn in_alternate_screen(&self) -> bool {
                false
            }
        }

        /// `last_focused_snapshot` retains only the focused pane's
        /// snapshot while in copy mode, not every pane's snapshot.
        #[test]
        fn copy_mode_retains_only_focused_pane_snapshot() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let kdl = r#"layout {
                split axis=horizontal {
                    pane kind=shell label="left"
                    pane kind=shell label="right"
                }
            }"#;
            let (mut ctx, _root, _area) = setup_fixture_ctx_with_runner(
                kdl,
                0,
                Router::new(Vec::new()),
                |pane, layer_id, close_tx| {
                    let marker = if pane.label.as_deref() == Some("left") {
                        'L'
                    } else {
                        'R'
                    };
                    PaneRunner::with_pty_for_test(
                        pane,
                        layer_id,
                        Box::new(MarkedStubPty::new(layer_id, marker)),
                        Some(close_tx),
                    )
                },
                LayoutRect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 24,
                },
                &mut terminal,
            );

            // Before entering copy mode, no snapshot is retained.
            assert!(ctx.last_focused_snapshot.is_none());

            // Enter copy mode and tick once.
            ctx.enter_copy_mode();
            let result = ctx.tick_runners().expect("tick runners");
            ctx.update_last_focused_snapshot(&result.snapshots);

            // Only the focused (left) pane's snapshot should be retained.
            let snapshot = ctx
                .last_focused_snapshot
                .as_ref()
                .expect("snapshot should be retained in copy mode");
            assert_eq!(snapshot.grid.cell(0, 0).ch, 'L');

            // Move focus to the second pane and tick again.
            ctx.set_focus(1);
            ctx.enter_copy_mode();
            let result = ctx.tick_runners().expect("tick runners");
            ctx.update_last_focused_snapshot(&result.snapshots);

            let snapshot = ctx
                .last_focused_snapshot
                .as_ref()
                .expect("snapshot should be retained in copy mode");
            assert_eq!(snapshot.grid.cell(0, 0).ch, 'R');

            // Exiting copy mode should clear the retained snapshot.
            ctx.apply_action_full(KeyAction::ModeExit);
            assert!(ctx.last_focused_snapshot.is_none());

            // When copy mode is inactive, further ticks should not retain
            // any snapshot.
            let result = ctx.tick_runners().expect("tick runners");
            ctx.update_last_focused_snapshot(&result.snapshots);
            assert!(ctx.last_focused_snapshot.is_none());
        }
    }
}
