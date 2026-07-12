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

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use cmdash::graphics::{GraphicsState, Metrics};
use cmdash::pane::{PaneCloseTx, PaneRunner};
use cmdash::render::{blit_cursor, blit_grid};
// `TabStack` (and `Tab`) are re-exported from the lib crate's
// `tabs` module via `cmdash::TabStack`. main.rs is the binary
// entrypoint; `crate::TabStack` would resolve to the binary
// crate's flat namespace (which does not define `TabStack`),
// not the lib. Use the lib-crate path so the `tabs: TabStack<TabState>`
// field on [`TickContext`] resolves.
use cmdash::TabStack;
use cmdash_config::{
    format_errors_with_context, parse_collect, KeyAction, LayoutNode, Pane as CfgPane, PaneKind,
    Ratio as CfgRatio, SplitAxis as CfgSplitAxis,
};
use cmdash_keybinds::Router;
use cmdash_layout::{
    adjacent_pane, remove_leaf, replace_leaf_with_split, ComputedLayout, Direction, PaneId,
    Rect as LayoutRect,
};
use cmdash_pty::PaneLayerId;
use cmdash_pty::{PaneEvent, ShellSpec};

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
fn shell_spec_from_command(command: &Option<String>, default: &ShellSpec) -> ShellSpec {
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
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use notify::Watcher as _;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Terminal;
use tracing::{debug, info, warn};

/// Loaded widget library and its C-ABI create function.
/// The `Library` is kept alive so the function pointer remains valid.
struct LoadedWidget {
    _library: libloading::Library,
    create: unsafe extern "C" fn(u32) -> *mut std::ffi::c_void,
}

/// Map of widget `ref_name` to loaded library + create function.
type WidgetFactories = std::collections::HashMap<String, LoadedWidget>;

/// Number of terminal rows reserved for the tab bar at the top of
/// the screen. The layout area's height is reduced by this amount
/// so panes don't overlap the tab bar. The tab bar is rendered in
/// phase 3a after pane blits into row 0 of the ratatui buffer.
/// When only 1 row of terminal height is available, panes are
/// skipped entirely (the tab bar alone fills the screen).
const TAB_BAR_HEIGHT: u16 = 1;

/// Number of terminal rows reserved for the status bar. The status
/// bar is optional — when disabled (the default), this constant
/// contributes 0 rows to the layout offset. When enabled, the
/// layout area's height is reduced by this amount and the status
/// bar is rendered in phase 3a after pane blits.
const STATUS_BAR_HEIGHT: u16 = 1;

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

/// Parsed config payload sent from the filesystem watcher
/// thread to the main tick loop via an mpsc channel.
#[derive(Debug)]
struct ConfigReload {
    keybinds: Vec<cmdash_config::Keybind>,
    presets: BTreeMap<String, LayoutNode>,
    layout_root: Option<LayoutNode>,
    status_bar: Option<cmdash_config::Bar>,
    theme: Option<cmdash_config::Theme>,
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
    let mut gfx_protocol = cmdash::GraphicsProtocol::detect();
    info!(
        graphics = gfx_protocol.name(),
        "cmdash starting (ratatui text body + dashcompositor graphics)"
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
                match PaneRunner::spawn_with_graphics(pane.clone(), layer_id, shell, Some(tx)) {
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
    let running = true;

    let mut guard = TerminalGuard::enter()?;

    // DA1 capability probe: if env-var detection yielded
    // TextOnly, send a DEC VT220 Primary Device Attributes
    // query (ESC[c) to detect Sixel support at runtime.
    // Only runs after raw mode is active so the response is
    // byte-oriented (not line-buffered). Skipped when
    // CMDASH_GRAPHICS or TERM already identified a protocol.
    if gfx_protocol == cmdash::GraphicsProtocol::TextOnly {
        if let Some(detected) =
            cmdash::GraphicsProtocol::query_device_attributes(Duration::from_millis(100))
        {
            info!(
                protocol = detected.name(),
                "DA1 query detected graphics protocol"
            );
            gfx_protocol = detected;
        }
    }

    let graphics =
        GraphicsState::new_with_protocol(Metrics::default(), (total.w, total.h), gfx_protocol);

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
        layout_area,
        cfg.presets,
        BTreeMap::new(),
        ShellSpec::LoginShell,
        config_reload_rx,
    );
    ctx.widget_factories = widget_factories;
    ctx.status_bar = cfg.status_bar;
    ctx.theme = cfg.theme.unwrap_or_default();
    ctx.run().await
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabState {
    /// Per-tab pane Vec. Clone-shells (pty-less) per the
    /// [`cmdash::pane::PaneRunner`] manual `Clone` impl; the
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
struct DragState {
    /// Tree path (child indices from root) to the Split node
    /// whose ratio is being adjusted. Fixed-size array backed
    /// by MAX_TREE_DEPTH (8).
    split_path: [u16; 8],
    /// Number of valid elements in `split_path`.
    split_path_len: u8,
    /// Initial mouse column (for Horizontal splits) or row
    /// (for Vertical splits) at drag start.
    start_pos: u16,
    /// Ratio of the Split node at drag start.
    initial_ratio: u8,
    /// The Split's axis — determines which mouse coord maps
    /// to the ratio.
    axis: cmdash_config::SplitAxis,
    /// Total cells along the Split axis (parent rect width for
    /// Horizontal, height for Vertical). Used to convert pixel
    /// deltas to percentage changes.
    total_cells: u16,
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
    /// Unbounded MPSC receiver of `PaneRunner::Drop` close notifications;
    /// drained at the start of phase 1.
    close_rx: UnboundedReceiver<cmdash_pty::PaneLayerId>,
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
    /// `AGENTS.md` `§` "`PaneId` stability" — moving the tree by value
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
    /// Owned clone of the binary's paired close sender. Retained
    /// so the runtime mutation paths (`AppNewPane` reconciliation,
    /// `PanePreset` rebuild) can wire fresh `PaneRunner`s into
    /// the SAME close-channel as the initial-frame spawn, preserving
    /// the Drop -> `close_tx` -> `GraphicsState`::`close_pane` round-trip.
    /// AGENTS.md §"Hard rule: one layer per instance" (`a` `LayerId` is
    /// bound to a pane instance for the instance's whole lifetime
    /// and is NEVER re-bound to a different pane).
    close_tx: UnboundedSender<cmdash_pty::PaneLayerId>,
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
    /// the focused `ZStack` member's resolved [`cmdash_layout::PaneId`]
    /// to its index within the parent `ZStack`. Survives across
    /// `AppNewPane`/`PaneClose` `InPlace` cycles (label-keyed
    /// reconciliation preserves the member's `PaneId` when the
    /// sibling stays under the same Split/ZStack parent);
    /// cleared on `Wholesale` swap (`PanePreset`)
    /// reconciliation so a reloaded preset's stale `PaneIds`
    /// don't linger in the map.
    stack_focus: BTreeMap<PaneId, usize>,
    /// Default shell for runtime-spawned panes. v1 single shell
    /// (`LoginShell`) — `cmdash::run` wires the constant. A future
    /// per-pane shell override slots in here.
    shell: ShellSpec,
    /// Active drag-to-resize state (Alt+drag on any pane).
    drag_state: Option<DragState>,
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
    tabs: TabStack<TabState>,
    /// Config hot-reload channel receiver.
    config_reload_rx: Option<UnboundedReceiver<ConfigReload>>,
    /// Optional status bar configuration. When `None`, no status
    /// bar is rendered. When `Some(Bar)`, a single row is reserved
    /// and the status bar is rendered in phase 3a.
    status_bar: Option<cmdash_config::Bar>,
    /// Optional theme configuration. When `None`, hardcoded default
    /// colors are used for the tab bar, status bar, and widget borders.
    theme: cmdash_config::Theme,
    /// Loaded widget libraries keyed by widget `ref_name`. Populated
    /// by [`load_widgets`] at startup; used by [`Self::reconcile_runners`]
    /// when spawning `PaneKind::Widget` panes.
    widget_factories: WidgetFactories,
    /// Current Kitty keyboard protocol progressive-enhancement
    /// flags advertised to the host terminal. The union of all
    /// live pane flags; recomputed every tick after pane events
    /// are drained.
    host_keyboard_flags: u8,
    /// Per-pane keyboard enhancement flags, keyed by layer id.
    /// Updated from `PaneEvent::KeyboardEnhancement` events and
    /// pruned when a pane closes.
    pane_keyboard_flags: HashMap<PaneLayerId, u8>,
    /// Whether we have pushed a keyboard enhancement entry onto
    /// the host terminal's stack. Used to pop exactly once on
    /// exit so the host returns to its prior state.
    host_keyboard_pushed: bool,
}

/// Monotonic `LayerId` allocator for
/// [`ReconcileMode::Wholesale`] spawns. `LayerIds` drawn from
/// `cmdash::derive_layer_id(&pane_id)` collide when the new
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
    /// across sibling-absorption rebalance; `PaneIds` are NOT,
    /// so PaneId-keyed matching would drop the survivor
    /// spuriously). Survivors' `PaneLayerId` is preserved per
    /// the AGENTS.md Hard rule.
    InPlace,
    /// Wholesale swap (`PanePreset`): every old runner is
    /// dropped (its `Drop` revokes the `LayerId` via `close_tx`)
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

/// Snapshot collection + exit-status result returned by
/// [`TickContext::tick_runners`]. Bundled into a struct to keep
/// the return type readable and clippy-friendly.
struct RunnerTickResult {
    snapshots: Vec<Option<cmdash_pty::PaneTerminalState>>,
    all_exited: bool,
}

impl<'a, B: ratatui::backend::Backend> TickContext<'a, B> {
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
            tabs: TabStack::new(initial_tab),
            config_reload_rx,
            status_bar: None,
            theme: cmdash_config::Theme::default(),
            widget_factories: std::collections::HashMap::new(),
            host_keyboard_flags: 0,
            pane_keyboard_flags: HashMap::new(),
            host_keyboard_pushed: false,
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
            KeyAction::ModeExit => {
                self.bindings.set_mode(cmdash_keybinds::Mode::Normal);
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
        let page_size = self
            .runners
            .get(self.focus)
            .map(|r| r.computed().rect.h as usize)
            .unwrap_or(24);
        match code {
            KeyCode::PageUp => {
                if let Some(runner) = self.runners.get_mut(self.focus) {
                    runner.scrollback_up(page_size);
                }
                return;
            }
            KeyCode::PageDown => {
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

        let bytes = if focused_flags != 0 {
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
                self.focus = idx;
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
        self.focus = pane_idx;
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
                        command: None,
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
                    cmdash::derive_layer_id_for_tab(&pane.id, self.active_tab_idx_u32())
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
                        match cmdash::script_widget::ScriptWidget::spawn(cmd, pane.label.as_deref())
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
                        match PaneRunner::spawn_with_graphics(
                            pane.clone(),
                            layer_id,
                            shell,
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
    /// `cmdash::derive_layer_id_for_tab`. The
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
            }),
            stack_focus: BTreeMap::new(),
        };
        self.tabs.push(new_state);
        self.sync_v1_from_active_tab();
        self.reconcile_runners(ReconcileMode::Wholesale);
    }

    /// Close the active tab. Empty stack quits the binary;
    /// otherwise sync the v1 fields and reconcile (Wholesale)
    /// so the dashcompositor layer book-keeping tracks the new
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

    /// Drive the AGENTS.md rendering pipeline until `running`
    /// flips `false` or every pane exits. The loop body is the
    /// same logic that lived in the prior free `tick_loop`
    /// function; bundling it on this struct lets `cmdash::run`
    /// invoke it as a one-shot `ctx.run().await`.
    ///
    /// v2 uses a `tokio::select!` loop. Crossterm's blocking
    /// `event::read` is isolated in a `tokio::task::spawn_blocking`
    /// task that forwards events over an unbounded channel. The
    /// main loop awaits that channel, the pane close channel, the
    /// config reload channel, and a `tokio::time::Interval` tick.
    /// Rendering and runner ticking remain synchronous inside the
    /// tick branch, preserving the single-threaded pipeline
    /// invariant required by `GraphicsState` and the borrowed
    /// `Terminal`.
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Spawn the crossterm input reader off the async runtime.
        // `event::read` blocks, so it runs in spawn_blocking and
        // forwards events over an unbounded channel. The task exits
        // when the main loop drops `input_rx` (on return).
        let (input_tx, input_rx) = unbounded_channel::<Event>();
        tokio::task::spawn_blocking(move || loop {
            match event::read() {
                Ok(evt) => {
                    if input_tx.send(evt).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "crossterm input reader error");
                    break;
                }
            }
        });

        let result = self.run_loop(input_rx).await;
        // Ensure the host terminal's keyboard enhancement stack
        // is popped before returning, even if the loop exited
        // because of an error. This restores the host to its
        // prior keyboard reporting mode.
        self.pop_host_keyboard_flags();
        result
    }

    /// Core event loop used by both production and tests.
    /// Tests pass a custom `input_rx` to exercise channel-close
    /// branches without a real terminal.
    pub(crate) async fn run_loop(
        &mut self,
        mut input_rx: UnboundedReceiver<Event>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut tick_interval = tokio::time::interval(self.tick);

        while self.running {
            tokio::select! {
                // Phase 0: crossterm input event.
                evt = input_rx.recv() => {
                    match evt {
                        Some(evt) => {
                            self.handle_event_full(&evt);
                            if let Event::Resize(w, h) = evt {
                                self.pending_resize = Some((w, h));
                            }
                        }
                        None => {
                            warn!("crossterm input channel closed; exiting event loop");
                            break;
                        }
                    }
                }

                // Phase 1: pane close notifications from Drop.
                id = self.close_rx.recv() => {
                    match id {
                        Some(id) => self.graphics.close_pane(id),
                        None => {
                            // All senders dropped; no more close
                            // notifications can arrive. Keep running
                            // so the remaining panes stay rendered.
                        }
                    }
                }

                // Phase 0.6: config hot-reload.
                msg = async {
                    match self.config_reload_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match msg {
                        Some(msg) => self.apply_config_reload(msg),
                        None => {
                            warn!("config reload channel closed; disabling hot-reload");
                            self.config_reload_rx = None;
                        }
                    }
                }

                // Phase 2/3: periodic tick + render.
                _ = tick_interval.tick() => {
                    self.tick_and_render()?;
                }
            }
        }

        Ok(())
    }

    /// Single iteration of the synchronous tick/render pipeline.
    /// Kept as a separate method so the async `run` loop stays
    /// readable and so the render logic remains testable.
    /// Phase 0.5: host SIGWINCH coalescer. Drains the resize slot
    /// queued during phase 0 and runs `relayout(...)` BEFORE the
    /// close-channel drain, so a resize signal that arrived mid-tick
    /// produces a fresh per-pane rect by the time rendering reads it.
    fn process_pending_resize(&mut self) {
        if let Some((w, h)) = self.pending_resize.take() {
            self.relayout(w, h);
        }
    }

    /// Phase 1 (part 1): drain the close-channel (Drop messages)
    /// FIRST so their revisions are visible before phase 2/3 in the
    /// same tick.
    fn drain_close_channel(&mut self) {
        let mut needs_sync = false;
        while let Ok(id) = self.close_rx.try_recv() {
            self.graphics.close_pane(id);
            // Only trigger a host sync when the closed pane's flags
            // were non-zero. A removed entry of 0 doesn't change the
            // union, so the sync would be a no-op.
            if self.pane_keyboard_flags.remove(&id).is_some_and(|f| f != 0) {
                needs_sync = true;
            }
        }
        if needs_sync {
            self.sync_host_keyboard_flags();
        }
    }

    /// Phase 1 (part 2): poll exits and tick runners.
    /// Returns the collected snapshots and a flag indicating whether
    /// all panes have exited.
    /// Recompute the union of all live pane keyboard enhancement
    /// flags and push/pop the host terminal's enhancement stack
    /// so that it matches. Called after pane snapshots are
    /// collected and after pane closures are drained.
    fn sync_host_keyboard_flags(&mut self) {
        let union = self
            .pane_keyboard_flags
            .values()
            .fold(0u8, |acc, &f| acc | f);
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
    fn pop_host_keyboard_flags(&mut self) {
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

    /// Drain `PaneEvent::KeyboardEnhancement` events from the
    /// freshly-collected pane snapshots and update
    /// `self.pane_keyboard_flags`. After updating, recompute the
    /// host terminal's enhancement state.
    fn update_keyboard_flags_from_snapshots(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) {
        let changed = cmdash::pane::collect_keyboard_enhancement_flags(
            &self.runners,
            snapshots,
            &mut self.pane_keyboard_flags,
        );
        if changed {
            self.sync_host_keyboard_flags();
        }
    }

    fn tick_runners(
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

    /// Phase 2: route pending events -> graphics. Kitty graphics
    /// emitted by a nested PTY are pushed onto the per-pane image map;
    /// everything else is logged. Failures log + continue (a busted
    /// image must not bring the multiplexer down).
    fn route_graphics_events(&mut self, snapshots: &[Option<cmdash_pty::PaneTerminalState>]) {
        for (runner, snap) in self.runners.iter().zip(snapshots.iter()) {
            if let Some(snap) = snap {
                for ev in &snap.pending_events {
                    if let PaneEvent::KittyGraphic { cmd } = ev {
                        self.graphics.apply_kitty_event(runner.layer_id(), cmd);
                    }
                }
            }
        }
    }

    /// Phase 3a: draw the cell body (PTY grids, tab bar,
    /// status bar, and widgets) into the ratatui terminal
    /// buffer. Kept separate from [`Self::emit_graphics`]
    /// so tests can inspect the text buffer without
    /// exercising the dashcompositor graphics path.
    fn render_cell_body(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
            // Pass 1: shell panes — blit PTY grids into the
            // ratatui buffer. Widget panes are skipped here
            // (their snapshots are None) and rendered in
            // pass 2 via widget.render().
            {
                let buf = frame.buffer_mut();
                for (runner, snap) in self.runners.iter().zip(snapshots.iter()) {
                    let computed_rect = runner.computed().rect;
                    debug!(
                        layer_id = ?runner.layer_id(),
                        rect.w = computed_rect.w,
                        rect.h = computed_rect.h,
                        "blitting pane"
                    );
                    let area = ratatui::layout::Rect::new(
                        computed_rect.x,
                        computed_rect.y + TAB_BAR_HEIGHT,
                        computed_rect.w,
                        computed_rect.h,
                    );
                    if let Some(snap) = snap {
                        blit_grid(&snap.grid, buf, area);
                        blit_cursor(&snap.grid, buf, area);
                    }
                }
                render_tab_bar(buf, &self.tabs, &self.theme);
                // Status bar rendering (Phase 3a).
                if let Some(ref sb) = self.status_bar {
                    if sb.enabled {
                        let total_h = self.last_area.h + TAB_BAR_HEIGHT;
                        let sb_y = match sb.position {
                            cmdash_config::BarPosition::Top => TAB_BAR_HEIGHT,
                            cmdash_config::BarPosition::Bottom => {
                                total_h.saturating_sub(STATUS_BAR_HEIGHT)
                            }
                        };
                        let sb_area = ratatui::layout::Rect::new(
                            0,
                            sb_y,
                            self.last_area.w,
                            STATUS_BAR_HEIGHT,
                        );
                        let mode_name = match self.bindings.mode() {
                            cmdash_keybinds::Mode::Normal => "Normal",
                            cmdash_keybinds::Mode::PaneResize => "Resize",
                            cmdash_keybinds::Mode::TabSwitch => "TabSwitch",
                            cmdash_keybinds::Mode::PresetPick => "PresetPick",
                        };
                        let pane_title = self
                            .runners
                            .get(self.focus)
                            .and_then(|r| r.computed().label.as_deref());
                        cmdash::status_bar::render_status_bar(
                            buf,
                            sb_area,
                            mode_name,
                            pane_title,
                            sb.show_clock,
                            sb.show_pane_title,
                            sb.show_mode,
                            &self.theme,
                        );
                    }
                }
            }
            // Pass 2: widget panes — call widget.render()
            // with the full Frame. The buf borrow from pass
            // 1 has been dropped (inner block ended), so
            // frame is available for direct widget rendering.
            for runner in self.runners.iter_mut() {
                let computed_rect = runner.computed().rect;
                if let Some(widget) = runner.widget.as_mut() {
                    let area = ratatui::layout::Rect::new(
                        computed_rect.x,
                        computed_rect.y + TAB_BAR_HEIGHT,
                        computed_rect.w,
                        computed_rect.h,
                    );
                    widget.render(area, frame);
                }
            }
        })?;
        Ok(())
    }

    /// Phase 3b: emit dashcompositor kitty graphics through
    /// a fresh stdout handle. The terminal's own backend
    /// already finished writing row-bearing text; kitty
    /// escapes overlay on kitty-capable hosts and degrade
    /// gracefully elsewhere. AGENTS.md §"Rendering
    /// pipeline" step 6 prescribes this exact path.
    ///
    /// Before compositing, rebuild the tab bar as
    /// dashcompositor layers (RectLayer background +
    /// TextLayer per tab via fontdue). The ratatui
    /// text tab bar from phase 3a is preserved as a
    /// degraded-mode fallback; the pixel overlay
    /// overwrites it on kitty-capable hosts.
    fn emit_graphics(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let tab_bar_data = cmdash::graphics::TabBarData {
            labels: self.tabs.iter().map(|t| t.label.as_deref()).collect(),
            active_idx: self.tabs.active_idx(),
            bar_width_cells: self.graphics.cells().0,
        };
        self.graphics.update_tab_bar(&tab_bar_data);
        let mut stdout = std::io::stdout();
        if let Err(e) = self.graphics.render_and_write(&mut stdout) {
            warn!(error = %e, "graphics emit failed");
        }
        Ok(())
    }

    /// Orchestrator: draw the cell body, then emit the
    /// dashcompositor graphics overlay. Kept thin so tests
    /// can call [`Self::render_cell_body`] and
    /// [`Self::emit_graphics`] independently.
    fn render_frame(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.render_cell_body(snapshots)?;
        self.emit_graphics()?;
        Ok(())
    }

    pub(crate) fn tick_and_render(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.process_pending_resize();
        self.drain_close_channel();
        let RunnerTickResult {
            snapshots,
            all_exited,
        } = self.tick_runners()?;
        self.update_keyboard_flags_from_snapshots(&snapshots);
        self.route_graphics_events(&snapshots);
        self.render_frame(&snapshots)?;

        if all_exited {
            self.running = false;
        }

        Ok(())
    }
}

fn render_tab_bar(
    buf: &mut ratatui::buffer::Buffer,
    tabs: &TabStack<TabState>,
    theme: &cmdash_config::Theme,
) {
    let bar_width = buf.area.width as usize;
    // Clear the tab bar row.
    for x in 0..bar_width {
        let cell = buf.get_mut(x as u16, 0);
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
            let cell = buf.get_mut(col as u16, 0);
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
fn redacted_event_debug(evt: &Event) -> String {
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
fn kitty_key_code(code: &KeyCode) -> Option<u32> {
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
fn kitty_modifiers(modifiers: KeyModifiers) -> u8 {
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
fn kitty_event_type(kind: KeyEventKind) -> u8 {
    match kind {
        KeyEventKind::Press => 1,
        KeyEventKind::Repeat => 2,
        KeyEventKind::Release => 3,
    }
}

/// Encode a single key event using the Kitty keyboard protocol CSI `u` form.
/// Returns `None` when the key has no Kitty protocol representation.
fn encode_kitty_key_event(
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
        let dir = std::env::temp_dir().join("cmdash_config_test");
        let _ = std::fs::create_dir_all(&dir);
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

        // Cleanup.
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
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
        let dir = std::env::temp_dir().join("cmdash_cli_config_e2e_test");
        let _ = std::fs::create_dir_all(&dir);
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

        // Cleanup.
        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&dir);
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
    /// tests that don't exercise the `layout`::`compute` path.
    /// Keeping it tiny avoids hitting `MAX_TREE_DEPTH` on
    /// out-of-band nesting during negative-test setup.
    fn dummy_layout_root() -> LayoutNode {
        LayoutNode::Pane(CfgPane {
            kind: PaneKind::Shell,
            label: None,
            command: None,
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
    fn setup_fixture_ctx<'a>(
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
        let cfg = cmdash_config::parse(kdl).expect("setup_fixture_ctx: parse KDL");
        let layout_root = cfg.layout.expect("setup_fixture_ctx: layout block");
        let layout =
            ComputedLayout::compute(&layout_root, last_area).expect("setup_fixture_ctx: compute");
        let (close_tx, close_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut runners: Vec<PaneRunner> = Vec::with_capacity(layout.panes.len());
        for pane in &layout.panes {
            let tx_clone = close_tx.clone();
            let layer_id = cmdash::derive_layer_id(&pane.id);
            let r = PaneRunner::spawn_with_graphics(
                pane.clone(),
                layer_id,
                shell.clone(),
                Some(tx_clone),
            )
            .expect("setup_fixture_ctx: spawn pane");
            runners.push(r);
        }
        let graphics = GraphicsState::new(
            cmdash::graphics::Metrics::default(),
            (last_area.w, last_area.h),
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
            shell,
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
        //    revokes the dashcompositor image registration.
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
        let r0 =
            PaneRunner::spawn_with_graphics(pane_a, id_a, shell.clone(), Some(close_tx.clone()))
                .expect("spawn runner A");
        let r1 = PaneRunner::spawn_with_graphics(pane_b, id_b, shell, Some(close_tx.clone()))
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
        let r0 =
            PaneRunner::spawn_with_graphics(pane_a, id_a, shell.clone(), Some(close_tx.clone()))
                .expect("spawn r0");
        let r1 =
            PaneRunner::spawn_with_graphics(pane_b, id_b, shell.clone(), Some(close_tx.clone()))
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
        let runner = PaneRunner::spawn_with_graphics(pane, layer, shell, Some(close_tx.clone()))
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
    /// dashcompositor image registration for that id).
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
        let runner =
            PaneRunner::spawn_with_graphics(pane, original_layer, shell, Some(close_tx.clone()))
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
        let graphics = GraphicsState::new(
            cmdash::graphics::Metrics::default(),
            (last_area.w, last_area.h),
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

    /// When the crossterm input channel closes, the main loop should
    /// break cleanly rather than spin forever.
    #[tokio::test]
    async fn input_rx_none_breaks_loop() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let mut ctx = setup_run_loop_ctx(&mut terminal);
        let (_input_tx, input_rx) = unbounded_channel::<Event>();
        // Dropping the sender makes `input_rx.recv()` return `None`.
        drop(_input_tx);

        let result = tokio::time::timeout(Duration::from_secs(2), ctx.run_loop(input_rx)).await;

        assert!(result.is_ok(), "run_loop should exit on input_rx None");
        assert!(ctx.running, "loop should break without processing AppClose");
    }

    /// When the pane close channel has no senders left,
    /// `close_rx.recv()` returns `None`. The loop should continue
    /// processing other events (in this case, the AppClose keypress).
    #[tokio::test]
    async fn close_rx_none_continues_loop() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let mut ctx = setup_run_loop_ctx(&mut terminal);

        // Replace the live close receiver with one whose sender has
        // already been dropped, so the first `recv()` returns `None`.
        let (close_tx, close_rx) = unbounded_channel::<PaneLayerId>();
        drop(close_tx);
        ctx.close_rx = close_rx;

        let (input_tx, input_rx) = unbounded_channel::<Event>();
        input_tx
            .send(key_event(KeyCode::Char('q'), KeyModifiers::empty()))
            .expect("send AppClose key");
        drop(input_tx);

        let result = tokio::time::timeout(Duration::from_secs(2), ctx.run_loop(input_rx)).await;

        assert!(result.is_ok(), "run_loop should not hang on close_rx None");
        assert!(
            !ctx.running,
            "AppClose should have been processed after close_rx None"
        );
    }

    /// When the config reload channel closes, the loop should disable
    /// hot-reload. We keep the input channel alive but empty so the
    /// only ready branch is the config reload receiver returning `None`;
    /// the loop would otherwise run forever, so we stop it with a short
    /// timeout and assert the field was cleared.
    #[tokio::test]
    async fn config_reload_rx_none_disables_hot_reload() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let mut ctx = setup_run_loop_ctx(&mut terminal);

        // Set up a config reload receiver whose sender has been dropped.
        let (cfg_tx, cfg_rx) = unbounded_channel::<ConfigReload>();
        drop(cfg_tx);
        ctx.config_reload_rx = Some(cfg_rx);

        // Keep the input sender alive so `input_rx.recv()` stays pending.
        // Disable the periodic tick so the config reload branch is the
        // only ready one and is selected immediately.
        ctx.tick = Duration::from_secs(3600);
        let (_input_tx, input_rx) = unbounded_channel::<Event>();
        let _ = tokio::time::timeout(Duration::from_millis(200), ctx.run_loop(input_rx)).await;

        assert!(
            ctx.config_reload_rx.is_none(),
            "config hot-reload should be disabled after channel closes"
        );
    }

    /// When no input events are pending, the tick branch should fire
    /// and the loop should keep running. We let a few ticks elapse
    /// under a short timeout and assert the loop is still alive.
    #[tokio::test]
    async fn tick_branch_fires_and_keeps_loop_alive() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
        let mut ctx = setup_run_loop_ctx(&mut terminal);
        // Keep the input sender alive but empty so the only ready
        // branch is the periodic tick.
        let (_input_tx, input_rx) = unbounded_channel::<Event>();
        let _ = tokio::time::timeout(Duration::from_millis(100), ctx.run_loop(input_rx)).await;
        assert!(ctx.running, "tick branch should keep the loop running");
    }

    /// process_pending_resize should consume the pending_resize slot
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

    /// drain_close_channel should remove all pending close messages
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

    /// tick_runners should return one snapshot per runner and report
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
                s.push_str(buf.get(x as u16, 0).symbol());
            }
            s
        }

        /// Helper: extract the `Style` of a single cell at `(x, 0)`.
        fn cell_style(buf: &Buffer, x: u16) -> ratatui::style::Style {
            buf.get(x, 0).style()
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
                buf.get(11, 0).symbol(),
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
                buf.get_mut(x, 0).set_symbol("X");
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
            assert_eq!(buf.get(0, 0).symbol(), " ");
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
            let dir = std::env::temp_dir().join("cmdash_hot_reload_test");
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
            // Cleanup BEFORE assertions so temp files don't leak
            // if an assertion panics.
            let _ = std::fs::remove_file(&config_path);
            let _ = std::fs::remove_dir(&dir);

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

            let dir = std::env::temp_dir().join("cmdash_hot_reload_layout_test");
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
            let dir = std::env::temp_dir().join("cmdash_hot_reload_debounce_test");
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

            // Cleanup BEFORE assertions.
            let _ = std::fs::remove_file(&config_path);
            let _ = std::fs::remove_dir(&dir);

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
            let dir = std::env::temp_dir().join("cmdash_hot_reload_invalid_test");
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

            // Cleanup BEFORE assertions.
            let _ = std::fs::remove_file(&config_path);
            let _ = std::fs::remove_dir(&dir);

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
    mod render_cell_body_tests {
        use super::*;
        use cmdash_widget_sdk::{CmdashWidget, WidgetEvent};
        use ratatui::layout::Rect;
        use ratatui::style::{Color, Style};
        use ratatui::widgets::{Paragraph, Widget};

        /// Build a minimal context with a custom widget runner (no real PTY)
        /// so `render_cell_body` can be exercised against a `TestBackend`
        /// without spawning shell children.
        fn setup_widget_ctx<'a>(
            terminal: &'a mut ratatui::Terminal<ratatui::backend::TestBackend>,
            widget: Box<dyn CmdashWidget>,
        ) -> TickContext<'a, ratatui::backend::TestBackend> {
            let kdl = r#"layout { pane kind=widget ref-name="marker" }"#;
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
            let runner = PaneRunner::spawn_widget(pane, layer_id, widget, Some(close_tx.clone()));
            let runners = vec![runner];
            let graphics = GraphicsState::new(
                cmdash::graphics::Metrics::default(),
                (last_area.w, last_area.h),
            );
            let bindings = Router::new(vec![]);
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

        /// A widget that renders a visible marker so tests can assert the
        /// widget render path was exercised by `render_cell_body`.
        struct MarkerWidget {
            marker: String,
        }

        impl CmdashWidget for MarkerWidget {
            fn name(&self) -> &str {
                "marker"
            }

            fn render(&mut self, area: Rect, frame: &mut ratatui::Frame) {
                let text = ratatui::text::Text::from(self.marker.clone());
                Paragraph::new(text)
                    .style(Style::default().fg(Color::Yellow))
                    .render(area, frame.buffer_mut());
            }

            fn on_event(&mut self, _event: &WidgetEvent) {}
        }

        /// `render_cell_body` must draw the tab bar into the ratatui buffer
        /// even when no shell panes are present. The tab bar occupies row 0
        /// and writes non-space content (the tab index / label), so the
        /// buffer should differ from a blank screen.
        #[tokio::test]
        async fn render_cell_body_draws_tab_bar() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);

            let snapshots: Vec<Option<cmdash_pty::PaneTerminalState>> = vec![None];
            ctx.render_cell_body(&snapshots)
                .expect("render_cell_body should succeed");

            let buf = terminal.backend().buffer().clone();
            let mut non_space_count = 0;
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    if buf.get(x, y).symbol() != " " {
                        non_space_count += 1;
                    }
                }
            }
            assert!(
                non_space_count > 0,
                "render_cell_body must draw non-space content (tab bar) to the TestBackend buffer"
            );
        }

        /// `render_cell_body` must call `widget.render()` for widget panes.
        /// The marker widget writes a known string into its area; the test
        /// asserts that string survives into the ratatui buffer.
        #[tokio::test]
        async fn render_cell_body_renders_widget_pane() {
            // 25 rows: row 0 is reserved for the tab bar, rows 1-24 for the pane.
            let backend = ratatui::backend::TestBackend::new(80, 25);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let widget = Box::new(MarkerWidget {
                marker: "WIDGET_MARKER".to_string(),
            });
            let mut ctx = setup_widget_ctx(&mut terminal, widget);

            let snapshots: Vec<Option<cmdash_pty::PaneTerminalState>> = vec![None];
            ctx.render_cell_body(&snapshots)
                .expect("render_cell_body should succeed");

            let buf = terminal.backend().buffer().clone();
            let mut found = false;
            'outer: for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    if buf.get(x, y).symbol() == "W" {
                        let mut ok = true;
                        for (i, ch) in "WIDGET_MARKER".chars().enumerate() {
                            let cx = x + i as u16;
                            if cx >= buf.area.width || buf.get(cx, y).symbol() != ch.to_string() {
                                ok = false;
                                break;
                            }
                        }
                        if ok {
                            found = true;
                            break 'outer;
                        }
                    }
                }
            }
            assert!(
                found,
                "render_cell_body must invoke widget.render() and place 'WIDGET_MARKER' in the buffer"
            );
        }

        /// `render_cell_body` must be independent of the dashcompositor
        /// graphics path. Calling it should not touch `GraphicsState` at all;
        /// this test simply verifies it returns Ok and leaves the buffer in
        /// a deterministic state (tab bar drawn, no panic).
        #[tokio::test]
        async fn render_cell_body_does_not_exercise_graphics_path() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);

            let snapshots: Vec<Option<cmdash_pty::PaneTerminalState>> = vec![None];
            let result = ctx.render_cell_body(&snapshots);
            assert!(
                result.is_ok(),
                "render_cell_body must succeed without touching GraphicsState"
            );

            // The buffer should contain the tab bar but no image-layer data.
            let buf = terminal.backend().buffer().clone();
            let has_non_space = (0..buf.area.height)
                .any(|y| (0..buf.area.width).any(|x| buf.get(x, y).symbol() != " "));
            assert!(
                has_non_space,
                "tab bar should be present in the text buffer"
            );
        }

        /// `emit_graphics` is the dashcompositor counter-part. It should
        /// run without panic even when the only layer is a widget pane and
        /// no images have been pushed.
        #[tokio::test]
        async fn emit_graphics_does_not_panic_with_empty_layers() {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend->Terminal");
            let mut ctx = setup_run_loop_ctx(&mut terminal);

            let result = ctx.emit_graphics();
            assert!(
                result.is_ok(),
                "emit_graphics must succeed with empty layers"
            );
        }
    }
}
