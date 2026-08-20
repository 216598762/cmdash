use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use clap::Parser;
use cmdash::{
    ApiServer, AppConfig, AppState, Backend, Command, Compositor, CrosstermBackend,
    GraphicsInputDemultiplexer, GraphicsSubmissionStatus, OuterInputEvent, SessionEventBus,
    SessionWakeup, Surface, SurfaceCommand, SurfaceId, TerminalWindowSize, UiEvent, WidgetRegistry,
    WidgetRuntimeContext,
    dashboard::{
        render_static_dashboard_shell_with_theme,
        render_static_dashboard_surface_scenes_with_theme, static_dashboard_surface_areas,
    },
    reload::ConfigReloader,
    ui_event_channel,
};
#[cfg(not(target_os = "linux"))]
use crossterm::event;
use crossterm::event::Event;
use directories::ProjectDirs;

const MAX_EVENTS_PER_BATCH: usize = 32;
#[cfg(not(target_os = "linux"))]
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAINTENANCE_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_CONFIG: &str = include_str!("../config/default.toml");

#[derive(clap::Parser)]
#[command(
    name = "cmdash",
    version,
    about = "A modular terminal dashboard and multiplexer"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short = 'c', long = "config", value_name = "PATH")]
    config: Option<PathBuf>,

    /// Rewrite the configuration file to the latest schema version.
    #[arg(long = "migrate-config")]
    migrate_config: bool,

    /// Enable the local compositor API.
    #[arg(long = "api")]
    api: bool,

    /// Disable the local compositor API.
    #[arg(long = "api-disable")]
    api_disable: bool,

    /// Enable the local compositor API in read-only mode.
    #[arg(long = "api-read-only")]
    api_read_only: bool,

    /// Path for the compositor API Unix socket.
    #[arg(long = "api-socket", value_name = "PATH")]
    api_socket: Option<String>,
}

struct InputReader {
    cancellation: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl InputReader {
    fn shutdown(mut self) {
        self.cancellation.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

enum MaintenanceCommand {
    Stop,
    ScheduleCursorBlink {
        delay: Option<Duration>,
        generation: u64,
    },
    ScheduleAnimation {
        delay: Option<Duration>,
    },
}

struct MaintenanceWaker {
    command_sender: Sender<MaintenanceCommand>,
    handle: Option<JoinHandle<()>>,
}

impl MaintenanceWaker {
    fn schedule_cursor_blink(&self, delay: Option<Duration>, generation: u64) {
        let _ = self
            .command_sender
            .send(MaintenanceCommand::ScheduleCursorBlink { delay, generation });
    }

    fn schedule_animation(&self, delay: Option<Duration>) {
        let _ = self
            .command_sender
            .send(MaintenanceCommand::ScheduleAnimation { delay });
    }

    fn shutdown(mut self) {
        let _ = self.command_sender.send(MaintenanceCommand::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn main() -> io::Result<()> {
    let mut backend = CrosstermBackend::new(io::stdout());
    let cli = Cli::parse();
    if cli.migrate_config {
        let path = cli.config.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "--migrate-config requires --config <path>",
            )
        })?;
        let migrations =
            AppConfig::rewrite_file(path).map_err(|error| io::Error::other(format!("{error}")))?;
        for migration in migrations {
            println!("migrated: {}", migration.warning());
        }
        return Ok(());
    }
    let explicit_config_path = cli.config.clone();
    let (config_path, mut config) = load_config(explicit_config_path.as_deref())?;
    apply_api_cli_overrides(&mut config, &cli)?;
    config
        .validate()
        .map_err(|error| io::Error::other(format!("application config rejected: {error}")))?;
    let config_path_for_report = config_path.clone();
    let initial_window_size = backend.window_size()?;
    backend.enter()?;
    probe_outer_terminal(&mut backend)?;
    let (event_sender, event_receiver, pty_wakeup) = ui_event_channel();
    let session_event_bus = SessionEventBus::new();
    let mut api_server = ApiServer::start(&config.api, event_sender.clone())?;
    let registry = WidgetRegistry::builtins_with_context(
        WidgetRuntimeContext::with_session_wakeup(pty_wakeup.clone())
            .with_session_event_bus(session_event_bus)
            .with_initial_terminal_size(initial_window_size.terminal_size())
            .with_kitty_graphics(backend.capabilities().kitty_graphics),
    );
    let mut state = AppState::from_config(backend.capabilities(), &registry, &config)
        .map_err(|error| io::Error::other(format!("application config rejected: {error}")))?;
    let mut compositor = Compositor::new();
    // RAII guard: kept alive for the whole run so the notify watcher keeps
    // emitting `ConfigChanged` events; dropped (which stops the watcher) when
    // `main` returns.
    #[cfg(feature = "watch")]
    let _config_watcher = config_path
        .as_ref()
        .map(|path| {
            let sender = event_sender.clone();
            cmdash::ConfigWatcher::spawn(path.clone(), move |_| {
                let _ = sender.send(UiEvent::ConfigChanged);
            })
        })
        .transpose()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let mut reloader = config_path
        .map(ConfigReloader::new)
        .transpose()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let input_reader = spawn_input_reader(event_sender.clone());
    let maintenance_waker = spawn_maintenance_waker(event_sender);

    let run_result = run(
        &mut backend,
        &mut state,
        &mut compositor,
        RunContext {
            event_receiver: &event_receiver,
            pty_wakeup: &pty_wakeup,
            registry: &registry,
            reloader: reloader.as_mut(),
            maintenance_waker: &maintenance_waker,
            api: api_server.as_mut(),
        },
    );
    input_reader.shutdown();
    maintenance_waker.shutdown();
    if let Some(api) = api_server.as_mut() {
        api.shutdown();
    }
    state.shutdown_widgets();
    let leave_result = backend.leave();

    let result = run_result.and(leave_result);
    if let Err(error) = &result
        && let Some(directory) = env::var_os("CMDASH_CRASH_DIR")
    {
        let _ = cmdash::CrashReport::from_error(error.to_string())
            .with_context(format!("config={:?}", config_path_for_report))
            .write_to(directory);
    }
    result
}

struct RunContext<'a> {
    event_receiver: &'a Receiver<UiEvent>,
    pty_wakeup: &'a SessionWakeup,
    registry: &'a WidgetRegistry,
    reloader: Option<&'a mut ConfigReloader>,
    maintenance_waker: &'a MaintenanceWaker,
    api: Option<&'a mut ApiServer>,
}

/// Applies an already-validated config, recording any migrations and keeping
/// the current runtime when the reload is rejected (never swap a valid
/// runtime for a broken one).
fn apply_loaded_config(
    state: &mut AppState,
    registry: &WidgetRegistry,
    loaded: cmdash::LoadedConfig,
) {
    for migration in &loaded.migrations {
        state.record_diagnostic(format!("config migration: {}", migration.warning()));
    }
    if let Err(error) = state.reload_config(registry, &loaded.config) {
        state.record_diagnostic(format!("config reload rejected: {error}"));
    }
}

/// Re-reads and applies the file-backed config. Shared by the `Ctrl+R`
/// keybinding and the `watch` feature's on-save event so both take the same
/// validate-then-swap path.
fn reload_config_from_disk(
    state: &mut AppState,
    registry: &WidgetRegistry,
    reloader: &mut ConfigReloader,
) {
    match reloader.reload_with_migrations() {
        Ok(loaded) => apply_loaded_config(state, registry, loaded),
        Err(error) => state.record_diagnostic(format!("config reload failed: {error}")),
    }
}

fn run<B>(
    backend: &mut B,
    state: &mut AppState,
    compositor: &mut Compositor,
    mut context: RunContext<'_>,
) -> io::Result<()>
where
    B: Backend<Error = io::Error>,
{
    let mut frame_generation = 0_u64;
    loop {
        // Outer graphics acknowledgements are asynchronous; retry policy is
        // coordinator-owned and shares the existing wakeable loop rather than
        // creating a graphics-specific worker or PTY polling timer.
        backend.poll_graphics_retries(Instant::now())?;
        context.maintenance_waker.schedule_cursor_blink(
            state.cursor_blink_schedule(),
            state.cursor_blink_generation(),
        );
        let animation_schedule = match (
            state.animation_schedule(),
            state.graphics_animation_schedule(),
        ) {
            (Some(ui), Some(graphics)) => Some(ui.min(graphics)),
            (schedule, None) | (None, schedule) => schedule,
        };
        context
            .maintenance_waker
            .schedule_animation(animation_schedule);
        if let Some(reloader) = context.reloader.as_deref_mut() {
            match reloader.poll_with_migrations() {
                Ok(Some(loaded)) => apply_loaded_config(state, context.registry, loaded),
                Ok(None) => {}
                Err(error) => state.record_diagnostic(format!("config reload failed: {error}")),
            }
        }
        let widget_report = state.update_widgets(SystemTime::now());
        if let Some(text) = state.take_clipboard() {
            backend.submit_clipboard(&text)?;
        }
        for invalidation in state.take_surface_invalidations() {
            compositor.invalidate(invalidation);
        }
        let window_size = backend.window_size()?;
        let area = window_size.area();
        sync_dashboard_surfaces(state, window_size)?;
        // Key/mouse-driven viewport navigation (scrollback, selection, cursor
        // presentation) changes a widget scene without necessarily producing
        // a PTY output update. Consume the state redraw request here so the
        // retained compositor does not preserve stale cells from the live
        // viewport while the emulator is rendering history rows. Invalidate
        // only the widget surfaces, not the whole viewport: a full-area
        // invalidate forces every cell (chrome, borders, unrelated widgets) to
        // be re-emitted on each wheel notch or drag, which wastes bandwidth
        // and visibly repaints the dashboard.
        if state.take_redraw_request() {
            for surface in state.workspace().surfaces().values() {
                if surface.visible() {
                    compositor.invalidate(surface.area());
                }
            }
        }
        let widget_health =
            (!state.widget_runtime().is_empty()).then(|| state.widget_runtime().health_summary());
        let base = render_static_dashboard_shell_with_theme(
            area,
            backend.metrics(),
            widget_health.as_deref(),
            state.latest_diagnostic(),
            state.theme(),
        );
        let surface_scenes = if state.widget_runtime().is_empty() {
            render_static_dashboard_surface_scenes_with_theme(area, state.focus(), state.theme())
        } else {
            state.widget_surface_scenes()
        };
        let diff = compositor.compose_and_diff(
            area,
            state,
            &base,
            &surface_scenes,
            widget_report.changed(),
        );
        backend.submit_diff(&diff)?;
        // Workstream 8 adapter swap: the mutation-driven command stream is the
        // source of truth for which placements need upload/place/delete; the
        // render diff still supplies the visible set and placeholder regions.
        let mut graphics_deltas = state.drain_graphics_deltas();
        // A full redraw can erase the outer terminal's visible placements
        // (resize repaints, UI animations). Images whose projection did not
        // change would otherwise never be re-placed and would stay missing
        // until the next scroll — tearing graphics away from text. Re-place
        // every visible submission as a safety net on such frames.
        if diff.full_redraw() {
            let mut known = graphics_deltas
                .changed
                .iter()
                .map(|submission| {
                    (
                        submission.resource().image(),
                        submission.placement().outer_placement_id(),
                    )
                })
                .collect::<std::collections::HashSet<_>>();
            for submission in diff.visible_graphics() {
                let key = (
                    submission.resource().image(),
                    submission.placement().outer_placement_id(),
                );
                if known.insert(key) {
                    graphics_deltas.changed.push(submission.clone());
                }
            }
        }
        let graphics_status = backend.submit_graphics_frame(
            &graphics_deltas.changed,
            diff.visible_graphics(),
            &graphics_deltas.removed,
            diff.visible_placeholders(),
            diff.removed_placeholders(),
        )?;
        if !graphics_status.is_successful()
            && graphics_status.placements() > 0
            && (!graphics_deltas.changed.is_empty() || !graphics_deltas.removed.is_empty())
        {
            let outcome = match &graphics_status {
                GraphicsSubmissionStatus::Suppressed { .. } => "suppressed",
                GraphicsSubmissionStatus::Failed { .. } => "failed",
                GraphicsSubmissionStatus::Degraded { .. } => "degraded",
                GraphicsSubmissionStatus::Rendered { .. } => "rendered",
            };
            state.record_diagnostic(format!(
                "graphics {outcome} for {} placement(s): {}",
                graphics_status.placements(),
                graphics_status.reason().unwrap_or("no additional details")
            ));
        }
        #[cfg(feature = "sixel")]
        backend.submit_sixel(diff.sixel())?;
        compositor.recycle(diff);
        frame_generation = frame_generation.wrapping_add(1);
        if let Some(api) = context.api.as_deref_mut() {
            api.publish_snapshot(cmdash::ApiSnapshot::from_state(
                state,
                compositor.frame(),
                backend.metrics(),
                frame_generation,
                api.expose_graphics(),
            ));
            let registry = context.registry;
            api.process_pending(state, |state| {
                let Some(reloader) = context.reloader.as_deref_mut() else {
                    return Err("API reload requires a file-backed configuration".to_owned());
                };
                let loaded = reloader
                    .reload_with_migrations()
                    .map_err(|error| error.to_string())?;
                state
                    .reload_config(registry, &loaded.config)
                    .map_err(|error| error.to_string())
            });
        }

        if dispatch_available_events(
            backend,
            state,
            context.event_receiver,
            context.pty_wakeup,
            context.registry,
            context.reloader.as_deref_mut(),
        )? {
            break;
        }
    }

    Ok(())
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: usize, timeout: i32) -> i32;
}

fn probe_outer_terminal(backend: &mut CrosstermBackend<io::Stdout>) -> io::Result<()> {
    let requested = env::var("CMDASH_KITTY_GRAPHICS_PROBE")
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes"));
    let kitty_root = env::var_os("KITTY_WINDOW_ID").is_some();
    if !requested && !kitty_root {
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        if !backend.begin_graphics_probe()? {
            return Ok(());
        }
        let mut tty = std::fs::OpenOptions::new().read(true).open("/dev/tty")?;
        let mut poll_fd = PollFd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&tty),
            events: 1,
            revents: 0,
        };
        let ready = unsafe { poll(&mut poll_fd, 1, 300) };
        if ready > 0 {
            let mut bytes = [0_u8; 4096];
            let length = tty.read(&mut bytes)?;
            let _ = backend.feed_outer_input(&bytes[..length]);
        } else if ready == 0 {
            let _ = backend.poll_graphics_probe_timeout();
        }
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = backend;
    }
    Ok(())
}

fn load_config(path: Option<&Path>) -> io::Result<(Option<PathBuf>, AppConfig)> {
    if let Some(path) = path {
        let config =
            AppConfig::load_file(path).map_err(|error| io::Error::other(format!("{error}")))?;
        return Ok((Some(path.to_path_buf()), config));
    }

    let candidates = [
        env::var_os("CMDASH_CONFIG").map(PathBuf::from),
        ProjectDirs::from("", "", "cmdash")
            .map(|directories| directories.config_dir().join("config.toml")),
        Some(PathBuf::from("config/default.toml")),
    ];
    for candidate in candidates.into_iter().flatten() {
        if candidate.is_file() {
            let config = AppConfig::load_file(&candidate)
                .map_err(|error| io::Error::other(format!("{error}")))?;
            return Ok((Some(candidate), config));
        }
    }
    let config = AppConfig::parse(DEFAULT_CONFIG)
        .map_err(|error| io::Error::other(format!("embedded default config rejected: {error}")))?;
    Ok((None, config))
}

fn apply_api_cli_overrides(config: &mut AppConfig, cli: &Cli) -> io::Result<()> {
    if cli.api {
        config.api.enabled = true;
    }
    if cli.api_disable {
        config.api.enabled = false;
    }
    if cli.api_read_only {
        config.api.enabled = true;
        config.api.read_only = true;
    }
    if let Some(socket) = &cli.api_socket {
        config.api.socket.clone_from(socket);
        config.api.enabled = true;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn spawn_input_reader(sender: Sender<UiEvent>) -> InputReader {
    let cancellation = Arc::new(AtomicBool::new(false));
    let thread_cancellation = Arc::clone(&cancellation);
    let handle = thread::spawn(move || {
        while !thread_cancellation.load(Ordering::Acquire) {
            match event::poll(INPUT_POLL_INTERVAL) {
                Ok(false) => {}
                Ok(true) if thread_cancellation.load(Ordering::Acquire) => break,
                Ok(true) => match event::read() {
                    Ok(event) => {
                        if sender.send(UiEvent::Input(event)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(UiEvent::InputError(error.to_string()));
                        break;
                    }
                },
                Err(error) => {
                    let _ = sender.send(UiEvent::InputError(error.to_string()));
                    break;
                }
            }
        }
    });
    InputReader {
        cancellation,
        handle: Some(handle),
    }
}

/// Linux input owner that reads `/dev/tty` once, demultiplexes outer graphics
/// replies, and only then decodes terminal input. This avoids crossterm and a
/// graphics probe competing for the same process-wide input stream.
#[cfg(target_os = "linux")]
fn spawn_input_reader(sender: Sender<UiEvent>) -> InputReader {
    let cancellation = Arc::new(AtomicBool::new(false));
    let thread_cancellation = Arc::clone(&cancellation);
    let handle = thread::spawn(move || {
        let mut tty = match std::fs::OpenOptions::new().read(true).open("/dev/tty") {
            Ok(tty) => tty,
            Err(error) => {
                let _ = sender.send(UiEvent::InputError(format!(
                    "could not open /dev/tty: {error}"
                )));
                return;
            }
        };
        let mut demultiplexer = GraphicsInputDemultiplexer::default();
        let mut bytes = [0_u8; 4096];
        while !thread_cancellation.load(Ordering::Acquire) {
            let mut poll_fd = PollFd {
                fd: std::os::fd::AsRawFd::as_raw_fd(&tty),
                events: 1,
                revents: 0,
            };
            let ready = unsafe { poll(&mut poll_fd, 1, 100) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                let _ = sender.send(UiEvent::InputError(io::Error::last_os_error().to_string()));
                break;
            }
            if ready == 0 {
                continue;
            }
            let length = match tty.read(&mut bytes) {
                Ok(length) => length,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = sender.send(UiEvent::InputError(error.to_string()));
                    break;
                }
            };
            if length == 0 {
                break;
            }
            for event in demultiplexer.feed(&bytes[..length]) {
                match event {
                    OuterInputEvent::GraphicsResponse(bytes) => {
                        if sender.send(UiEvent::OuterInput(bytes)).is_err() {
                            return;
                        }
                    }
                    OuterInputEvent::ClipboardResponse(bytes) => {
                        if sender.send(UiEvent::OuterClipboard(bytes)).is_err() {
                            return;
                        }
                    }
                    OuterInputEvent::TerminalInput(bytes) => {
                        for event in decode_terminal_input(&bytes) {
                            if sender.send(UiEvent::Input(event)).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }
    });
    InputReader {
        cancellation,
        handle: Some(handle),
    }
}

#[cfg(target_os = "linux")]
fn decode_terminal_input(bytes: &[u8]) -> Vec<Event> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut events = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            if bytes.get(index + 1) == Some(&b'[')
                && let Some(final_offset) = bytes[index + 2..]
                    .iter()
                    .position(|byte| (0x40..=0x7e).contains(byte))
            {
                let final_index = index + 2 + final_offset;
                let sequence = &bytes[index + 2..=final_index];
                if let Some(mouse) = decode_sgr_mouse(sequence) {
                    events.push(mouse);
                    index = final_index + 1;
                    continue;
                }
                let code = match sequence.last().copied() {
                    Some(b'A') => Some(KeyCode::Up),
                    Some(b'B') => Some(KeyCode::Down),
                    Some(b'C') => Some(KeyCode::Right),
                    Some(b'D') => Some(KeyCode::Left),
                    Some(b'H') => Some(KeyCode::Home),
                    Some(b'F') => Some(KeyCode::End),
                    Some(b'Z') => Some(KeyCode::BackTab),
                    Some(b'~') if sequence.starts_with(b"1;") => Some(KeyCode::Home),
                    Some(b'~') if sequence.starts_with(b"4;") => Some(KeyCode::End),
                    Some(b'~') => Some(KeyCode::Delete),
                    _ => None,
                };
                if let Some(code) = code {
                    events.push(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
                }
                index = final_index + 1;
                continue;
            }
            events.push(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
            index += 1;
            continue;
        }
        let byte = bytes[index];
        if byte == b'\r' || byte == b'\n' {
            events.push(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )));
            index += 1;
            continue;
        }
        if byte == b'\t' {
            events.push(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            index += 1;
            continue;
        }
        if byte < 0x20 {
            let character = (byte.saturating_sub(1) + b'a') as char;
            events.push(Event::Key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::CONTROL,
            )));
            index += 1;
            continue;
        }
        let text = String::from_utf8_lossy(&bytes[index..]);
        let Some(character) = text.chars().next() else {
            break;
        };
        events.push(Event::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::NONE,
        )));
        index += character.len_utf8();
    }
    events
}

#[cfg(target_os = "linux")]
fn decode_sgr_mouse(sequence: &[u8]) -> Option<Event> {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    if !sequence.starts_with(b"<") || !matches!(sequence.last(), Some(b'M' | b'm')) {
        return None;
    }
    let body = std::str::from_utf8(&sequence[1..sequence.len().saturating_sub(1)]).ok()?;
    let mut values = body.split(';');
    let button = values.next()?.parse::<u16>().ok()?;
    let column = values.next()?.parse::<u16>().ok()?.saturating_sub(1);
    let row = values.next()?.parse::<u16>().ok()?.saturating_sub(1);
    let modifiers = KeyModifiers::from_bits_truncate(
        (u8::from(button & 4 != 0) * KeyModifiers::SHIFT.bits())
            | (u8::from(button & 8 != 0) * KeyModifiers::ALT.bits())
            | (u8::from(button & 16 != 0) * KeyModifiers::CONTROL.bits()),
    );
    let base = button & 3;
    let kind = if button & 64 != 0 {
        match base {
            0 => MouseEventKind::ScrollUp,
            1 => MouseEventKind::ScrollDown,
            2 => MouseEventKind::ScrollLeft,
            _ => MouseEventKind::ScrollRight,
        }
    } else if button & 32 != 0 {
        MouseEventKind::Drag(match base {
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            _ => MouseButton::Left,
        })
    } else if sequence.ends_with(b"m") {
        MouseEventKind::Up(match base {
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            _ => MouseButton::Left,
        })
    } else {
        MouseEventKind::Down(match base {
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            _ => MouseButton::Left,
        })
    };
    Some(Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    }))
}

fn spawn_maintenance_waker(sender: Sender<UiEvent>) -> MaintenanceWaker {
    let (command_sender, command_receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut maintenance_deadline = Instant::now() + MAINTENANCE_INTERVAL;
        let mut cursor_deadline: Option<(Instant, u64)> = None;
        let mut animation_deadline: Option<Instant> = None;
        loop {
            let now = Instant::now();
            let next_deadline = cursor_deadline
                .map(|(cursor, _)| cursor)
                .into_iter()
                .chain(animation_deadline)
                .chain(std::iter::once(maintenance_deadline))
                .min()
                .expect("maintenance always has a deadline");
            match command_receiver.recv_timeout(next_deadline.saturating_duration_since(now)) {
                Ok(MaintenanceCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(MaintenanceCommand::ScheduleCursorBlink { delay, generation }) => {
                    cursor_deadline = delay.map(|delay| (Instant::now() + delay, generation));
                }
                Ok(MaintenanceCommand::ScheduleAnimation { delay }) => {
                    animation_deadline = delay.map(|delay| Instant::now() + delay);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    if now >= maintenance_deadline {
                        if sender.send(UiEvent::Tick).is_err() {
                            break;
                        }
                        maintenance_deadline = now + MAINTENANCE_INTERVAL;
                    }
                    if cursor_deadline.is_some_and(|(deadline, _)| now >= deadline) {
                        let (_, generation) = cursor_deadline.expect("cursor deadline was checked");
                        if sender.send(UiEvent::CursorBlink(generation)).is_err() {
                            break;
                        }
                        cursor_deadline = None;
                    }
                    if animation_deadline.is_some_and(|deadline| now >= deadline) {
                        if sender.send(UiEvent::AnimationFrame).is_err() {
                            break;
                        }
                        animation_deadline = None;
                    }
                }
            }
        }
    });
    MaintenanceWaker {
        command_sender,
        handle: Some(handle),
    }
}

fn dispatch_available_events<B: Backend<Error = io::Error>>(
    backend: &mut B,
    state: &mut AppState,
    event_receiver: &Receiver<UiEvent>,
    pty_wakeup: &SessionWakeup,
    registry: &WidgetRegistry,
    mut reloader: Option<&mut ConfigReloader>,
) -> io::Result<bool> {
    let mut events = Vec::with_capacity(MAX_EVENTS_PER_BATCH);
    collect_ui_event(
        backend,
        state,
        event_receiver
            .recv()
            .map_err(|_| io::Error::other("input and PTY event channel disconnected"))?,
        &mut events,
        pty_wakeup,
        registry,
        reloader.as_deref_mut(),
    )?;

    while events.len() < MAX_EVENTS_PER_BATCH {
        match event_receiver.try_recv() {
            Ok(event) => collect_ui_event(
                backend,
                state,
                event,
                &mut events,
                pty_wakeup,
                registry,
                reloader.as_deref_mut(),
            )?,
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(io::Error::other("input and PTY event channel disconnected"));
            }
        }
    }

    dispatch_event_batch(state, registry, reloader, events)
}

fn collect_ui_event<B: Backend<Error = io::Error>>(
    backend: &mut B,
    state: &mut AppState,
    event: UiEvent,
    events: &mut Vec<Event>,
    pty_wakeup: &SessionWakeup,
    registry: &WidgetRegistry,
    reloader: Option<&mut ConfigReloader>,
) -> io::Result<()> {
    match event {
        UiEvent::Input(event) => events.push(event),
        UiEvent::ConfigChanged => {
            if let Some(reloader) = reloader {
                reload_config_from_disk(state, registry, reloader);
            }
        }
        UiEvent::OuterInput(bytes) => {
            let batch = backend.feed_outer_input(&bytes);
            if let Some(error) = batch.graphics_error {
                state.record_diagnostic(format!("outer graphics input rejected: {error}"));
            }
        }
        UiEvent::OuterClipboard(bytes) => {
            let batch = backend.feed_outer_input(&bytes);
            if let Some(error) = batch.graphics_error {
                state.record_diagnostic(format!("outer graphics input rejected: {error}"));
            }
            if let Some(text) = batch.clipboard_text {
                state.deliver_clipboard(text);
            }
        }
        UiEvent::PtyOutput => pty_wakeup.clear_pending(),
        UiEvent::ClipboardStore(text) => state.record_clipboard(text),
        UiEvent::ClipboardRead(_) => {
            backend.request_clipboard()?;
        }
        UiEvent::Bell(_) => state.record_bell(),
        UiEvent::Notification(_, message) => state.record_diagnostic(message),
        UiEvent::SessionTitle(id, title) => state.publish_session_title(id, title),
        UiEvent::Tick => {}
        UiEvent::ApiWakeup => {}
        UiEvent::AnimationFrame => {
            state.advance_animations(SystemTime::now());
        }
        UiEvent::CursorBlink(generation) => {
            state.advance_cursor_blink(generation);
        }
        UiEvent::InputError(message) => return Err(io::Error::other(message)),
    }
    Ok(())
}

fn dispatch_event_batch<I>(
    state: &mut AppState,
    registry: &WidgetRegistry,
    mut reloader: Option<&mut ConfigReloader>,
    events: I,
) -> io::Result<bool>
where
    I: IntoIterator<Item = Event>,
{
    for event in events.into_iter().take(MAX_EVENTS_PER_BATCH) {
        if dispatch_event(state, registry, reloader.as_deref_mut(), event)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn dispatch_event(
    state: &mut AppState,
    registry: &WidgetRegistry,
    reloader: Option<&mut ConfigReloader>,
    event: Event,
) -> io::Result<bool> {
    match event {
        Event::Key(key) => {
            let command = if state.focused_terminal_captures_keys() {
                state.keymap().terminal_capture_for_key(key)
            } else {
                state.keymap().command_for_key(key)
            };
            match command {
                Some(Command::CopySelection) => {
                    state.copy_focused_selection();
                    Ok(false)
                }
                Some(Command::ReloadConfig) => {
                    if let Some(reloader) = reloader {
                        reload_config_from_disk(state, registry, reloader);
                    } else {
                        state.record_diagnostic("Ctrl+R requires --config <path>");
                    }
                    Ok(false)
                }
                Some(command) => {
                    let effect = state.dispatch(command).map_err(|error| {
                        io::Error::other(format!("command rejected: {error:?}"))
                    })?;
                    Ok(matches!(effect, cmdash::CommandEffect::Quit))
                }
                None => state
                    .handle_focused_key(key)
                    .map_err(|error| io::Error::other(format!("widget input rejected: {error}")))
                    .map(|_| false),
            }
        }
        Event::Paste(text) => state
            .handle_focused_paste(&text)
            .map_err(|error| io::Error::other(format!("widget paste rejected: {error}")))
            .map(|_| false),
        Event::Mouse(mouse) => state
            .handle_mouse(mouse)
            .map_err(|error| io::Error::other(format!("widget mouse input rejected: {error}")))
            .map(|_| false),
        _ => Ok(false),
    }
}

fn sync_dashboard_surfaces(
    state: &mut AppState,
    window_size: TerminalWindowSize,
) -> io::Result<()> {
    let area = window_size.area();
    let surface_areas: BTreeMap<_, _> = if state.widget_runtime().is_empty() {
        static_dashboard_surface_areas(area).into_iter().collect()
    } else {
        state
            .layout()
            .widget_areas(area)
            .into_iter()
            .map(|(widget_id, widget_area)| (SurfaceId::new(widget_id.get()), widget_area))
            .collect()
    };
    let visible_surface_ids: BTreeSet<_> = surface_areas.keys().copied().collect();
    let hidden_surface_ids: Vec<_> = state
        .workspace()
        .surfaces()
        .keys()
        .copied()
        .filter(|id| !visible_surface_ids.contains(id))
        .collect();
    for id in hidden_surface_ids {
        if state
            .workspace()
            .surfaces()
            .get(&id)
            .is_some_and(|surface| surface.visible())
        {
            state
                .dispatch(Command::Surface(SurfaceCommand::SetVisible {
                    id,
                    visible: false,
                }))
                .map_err(|error| io::Error::other(format!("surface sync rejected: {error:?}")))?;
        }
    }

    for (&id, &surface_area) in &surface_areas {
        let existing = state.workspace().surfaces().contains_key(&id);
        let should_show = existing
            && surface_area.width > 0
            && surface_area.height > 0
            && state
                .workspace()
                .surfaces()
                .get(&id)
                .is_some_and(|surface| !surface.visible());
        let command = if surface_area.width == 0 || surface_area.height == 0 {
            if existing {
                Some(Command::Surface(SurfaceCommand::SetVisible {
                    id,
                    visible: false,
                }))
            } else {
                None
            }
        } else if existing {
            Some(Command::Surface(SurfaceCommand::SetArea {
                id,
                area: surface_area,
            }))
        } else {
            Some(Command::Surface(SurfaceCommand::Add(Surface::new(
                id,
                surface_area,
            ))))
        };

        if let Some(command) = command {
            state
                .dispatch(command)
                .map_err(|error| io::Error::other(format!("surface sync rejected: {error:?}")))?;
        }
        if should_show {
            state
                .dispatch(Command::Surface(SurfaceCommand::SetVisible {
                    id,
                    visible: true,
                }))
                .map_err(|error| io::Error::other(format!("surface sync rejected: {error:?}")))?;
        }
    }

    state
        .resize_widget_surfaces(&surface_areas, window_size)
        .map_err(|error| io::Error::other(format!("widget resize rejected: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use cmdash::{BackendCapabilities, FocusTarget, SurfaceId};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;

    fn capabilities() -> BackendCapabilities {
        BackendCapabilities {
            truecolor: true,
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: false,
            kitty_unicode_placeholders: false,
            graphics_source: cmdash::GraphicsCapabilitySource::Unavailable,
            graphics_confidence: cmdash::GraphicsCapabilityConfidence::Rejected,
            kitty_passthrough: false,
            kitty_text_fallback: false,
            sixel: false,
        }
    }

    fn tab_event() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn raw_input_decoder_keeps_graphics_out_of_keyboard_events() {
        let mut demux = GraphicsInputDemultiplexer::default();
        let events = demux.feed(b"q\x1b_Gi=7;OK\x1b\\\x1b[A");
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], OuterInputEvent::TerminalInput(_)));
        assert!(matches!(events[1], OuterInputEvent::GraphicsResponse(_)));
        assert!(matches!(events[2], OuterInputEvent::TerminalInput(_)));

        let decoded = decode_terminal_input(b"q\x1b[A\r");
        assert_eq!(decoded.len(), 3);
        assert!(matches!(decoded[0], Event::Key(_)));
        assert!(matches!(decoded[1], Event::Key(_)));
        assert!(matches!(decoded[2], Event::Key(_)));
    }

    #[test]
    fn event_batches_are_bounded() {
        let mut state = AppState::new(capabilities());
        for id in [SurfaceId::new(1), SurfaceId::new(2)] {
            state
                .dispatch(Command::Surface(SurfaceCommand::Add(Surface::new(
                    id,
                    Rect::new(0, 0, 10, 4),
                ))))
                .unwrap();
        }

        let events = (0..MAX_EVENTS_PER_BATCH + 1).map(|_| tab_event());
        assert!(
            !dispatch_event_batch(&mut state, &WidgetRegistry::builtins(), None, events,).unwrap()
        );
        assert_eq!(
            state.focus().target(),
            Some(FocusTarget::Surface(SurfaceId::new(2)))
        );
    }

    #[test]
    fn quit_stops_dispatching_the_current_batch() {
        let mut state = AppState::new(capabilities());
        let events = [
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            tab_event(),
        ];

        assert!(
            dispatch_event_batch(&mut state, &WidgetRegistry::builtins(), None, events,).unwrap()
        );
        assert!(state.quit_requested());
        assert_eq!(state.focus().target(), None);
    }

    #[test]
    fn terminal_focus_captures_keys_except_the_escape_binding() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 1
            type = "terminal"
            command = "sh"
            [[workspace.widgets]]
            id = 2
            type = "text"
            text = "plain"
            "#,
        )
        .unwrap();
        let registry = WidgetRegistry::builtins();
        let mut state = AppState::from_config(capabilities(), &registry, &config).unwrap();
        state
            .dispatch(Command::Focus(cmdash::FocusCommand::Surface(
                SurfaceId::new(1),
            )))
            .unwrap();
        assert!(state.focused_terminal_captures_keys());

        let quit = dispatch_event(
            &mut state,
            &registry,
            None,
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        )
        .unwrap();
        assert!(!quit);
        assert!(!state.quit_requested());
        assert_eq!(
            state.focus().target(),
            Some(FocusTarget::Surface(SurfaceId::new(1)))
        );

        dispatch_event(&mut state, &registry, None, tab_event()).unwrap();
        assert_eq!(
            state.focus().target(),
            Some(FocusTarget::Surface(SurfaceId::new(2)))
        );

        state.shutdown_widgets();
    }

    #[test]
    fn remapped_keys_drive_the_same_command_path() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [keybindings]
            quit = "ctrl+q"
            "#,
        )
        .unwrap();
        let registry = WidgetRegistry::builtins();
        let mut state = AppState::from_config(capabilities(), &registry, &config).unwrap();

        // The default quit binding no longer applies.
        let quit_on_q = dispatch_event(
            &mut state,
            &registry,
            None,
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        )
        .unwrap();
        assert!(!quit_on_q);
        assert!(!state.quit_requested());

        // The remapped binding quits.
        let quit_on_ctrl_q = dispatch_event(
            &mut state,
            &registry,
            None,
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        )
        .unwrap();
        assert!(quit_on_ctrl_q);
        assert!(state.quit_requested());

        state.shutdown_widgets();
    }

    #[test]
    fn terminal_capture_uses_the_remapped_escape_binding() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 1
            type = "terminal"
            command = "sh"
            [[workspace.widgets]]
            id = 2
            type = "text"
            text = "plain"
            [keybindings]
            focus_next = "ctrl+j"
            focus_previous = "ctrl+k"
            "#,
        )
        .unwrap();
        let registry = WidgetRegistry::builtins();
        let mut state = AppState::from_config(capabilities(), &registry, &config).unwrap();
        state
            .dispatch(Command::Focus(cmdash::FocusCommand::Surface(
                SurfaceId::new(1),
            )))
            .unwrap();
        assert!(state.focused_terminal_captures_keys());

        // The old escape binding (Tab) is now forwarded to the PTY.
        dispatch_event(&mut state, &registry, None, tab_event()).unwrap();
        assert_eq!(
            state.focus().target(),
            Some(FocusTarget::Surface(SurfaceId::new(1)))
        );

        // The remapped escape binding still moves focus.
        dispatch_event(
            &mut state,
            &registry,
            None,
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
        )
        .unwrap();
        assert_eq!(
            state.focus().target(),
            Some(FocusTarget::Surface(SurfaceId::new(2)))
        );

        state.shutdown_widgets();
    }

    #[test]
    fn cli_accepts_short_and_long_config_options() {
        assert_eq!(
            Cli::try_parse_from(["cmdash", "--config", "dashboard.toml"])
                .unwrap()
                .config,
            Some(PathBuf::from("dashboard.toml"))
        );
        assert_eq!(
            Cli::try_parse_from(["cmdash", "-c", "dashboard.toml"])
                .unwrap()
                .config,
            Some(PathBuf::from("dashboard.toml"))
        );
        assert_eq!(Cli::try_parse_from(["cmdash"]).unwrap().config, None);
    }

    #[test]
    fn cli_rejects_missing_values_and_unknown_arguments() {
        assert!(Cli::try_parse_from(["cmdash", "--config"]).is_err());
        assert!(Cli::try_parse_from(["cmdash", "--api-socket"]).is_err());
        assert!(Cli::try_parse_from(["cmdash", "--verbose"]).is_err());
    }

    #[test]
    fn api_cli_overrides_have_explicit_precedence() {
        let mut config = AppConfig::parse(
            "version = 1\n[api]\nenabled = true\nread_only = false\nsocket = \"/tmp/from-file.sock\"\n",
        )
        .unwrap();
        let cli = Cli::try_parse_from([
            "cmdash",
            "--api-disable",
            "--api-socket",
            "/tmp/from-cli.sock",
        ])
        .unwrap();
        apply_api_cli_overrides(&mut config, &cli).unwrap();
        assert!(config.api.enabled);
        assert_eq!(config.api.socket, "/tmp/from-cli.sock");
        assert!(!config.api.read_only);
    }

    #[test]
    fn config_discovery_uses_the_directories_crate_roots() {
        // Phase 20 parity contract: `ProjectDirs::from("", "", "cmdash")` is the
        // replacement for the hand-rolled XDG discovery and must resolve to the
        // `<config_dir>/cmdash` root on every supported platform.
        let dirs = ProjectDirs::from("", "", "cmdash")
            .expect("config discovery must resolve a project root");
        let config_dir = dirs.config_dir();
        assert!(
            config_dir.ends_with("cmdash"),
            "config root must end with the application name, got {config_dir:?}"
        );
        assert!(config_dir.is_absolute());
    }
}
