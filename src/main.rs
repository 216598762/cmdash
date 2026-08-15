use std::{
    collections::{BTreeMap, BTreeSet},
    env, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use cmdash::{
    ApiServer, AppConfig, AppState, Backend, Command, Compositor, CrosstermBackend,
    GraphicsSubmissionStatus, SessionWakeup, Surface, SurfaceCommand, SurfaceId,
    TerminalWindowSize, UiEvent, WidgetRegistry, WidgetRuntimeContext,
    dashboard::{
        render_static_dashboard_shell_with_theme,
        render_static_dashboard_surface_scenes_with_theme, static_dashboard_surface_areas,
    },
    input::command_for_key,
    reload::ConfigReloader,
    ui_event_channel,
};
use crossterm::event::{self, Event};

const MAX_EVENTS_PER_BATCH: usize = 32;
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAINTENANCE_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_CONFIG: &str = include_str!("../config/default.toml");

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
    let args: Vec<_> = env::args().skip(1).collect();
    if args.iter().any(|argument| argument == "--migrate-config") {
        let path = config_path(&args)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "--migrate-config requires --config <path>",
            )
        })?;
        let migrations =
            AppConfig::rewrite_file(&path).map_err(|error| io::Error::other(format!("{error}")))?;
        for migration in migrations {
            println!("migrated: {}", migration.warning());
        }
        return Ok(());
    }
    let explicit_config_path = config_path(&args)?;
    let (config_path, mut config) = load_config(explicit_config_path.as_deref())?;
    apply_api_cli_overrides(&mut config, &args)?;
    config
        .validate()
        .map_err(|error| io::Error::other(format!("application config rejected: {error}")))?;
    let config_path_for_report = config_path.clone();
    let initial_window_size = backend.window_size()?;
    let (event_sender, event_receiver, pty_wakeup) = ui_event_channel();
    let mut api_server = ApiServer::start(&config.api, event_sender.clone())?;
    let registry = WidgetRegistry::builtins_with_context(
        WidgetRuntimeContext::with_session_wakeup(pty_wakeup.clone())
            .with_initial_terminal_size(initial_window_size.terminal_size())
            .with_kitty_graphics(backend.capabilities().kitty_graphics),
    );
    let mut state = AppState::from_config(backend.capabilities(), &registry, &config)
        .map_err(|error| io::Error::other(format!("application config rejected: {error}")))?;
    let mut compositor = Compositor::new();
    let mut reloader = config_path
        .map(ConfigReloader::new)
        .transpose()
        .map_err(|error| io::Error::other(error.to_string()))?;
    backend.enter()?;
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
        context.maintenance_waker.schedule_cursor_blink(
            state.cursor_blink_schedule(),
            state.cursor_blink_generation(),
        );
        context
            .maintenance_waker
            .schedule_animation(state.animation_schedule());
        if let Some(reloader) = context.reloader.as_deref_mut() {
            match reloader.poll_with_migrations() {
                Ok(Some(loaded)) => {
                    for migration in &loaded.migrations {
                        state.record_diagnostic(format!(
                            "config migration: {}",
                            migration.warning()
                        ));
                    }
                    if let Err(error) = state.reload_config(context.registry, &loaded.config) {
                        state.record_diagnostic(format!("config reload rejected: {error}"));
                    }
                }
                Ok(None) => {}
                Err(error) => state.record_diagnostic(format!("config reload failed: {error}")),
            }
        }
        state.update_widgets(SystemTime::now());
        if let Some(text) = state.take_clipboard() {
            backend.submit_clipboard(&text)?;
        }
        for invalidation in state.take_surface_invalidations() {
            compositor.invalidate(invalidation);
        }
        let window_size = backend.window_size()?;
        let area = window_size.area();
        sync_dashboard_surfaces(state, window_size)?;
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
        let scene = compositor.compose(area, state, &base, &surface_scenes);
        let diff = compositor.diff(&scene);
        backend.submit_diff(&diff)?;
        let graphics_status = backend.submit_graphics(
            diff.graphics(),
            diff.visible_graphics(),
            diff.removed_graphics(),
        )?;
        if !graphics_status.is_successful()
            && graphics_status.placements() > 0
            && (!diff.graphics().is_empty() || !diff.removed_graphics().is_empty())
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
        frame_generation = frame_generation.wrapping_add(1);
        if let Some(api) = context.api.as_deref_mut() {
            api.publish_snapshot(cmdash::ApiSnapshot::from_state(
                state,
                &scene,
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

fn load_config(path: Option<&Path>) -> io::Result<(Option<PathBuf>, AppConfig)> {
    if let Some(path) = path {
        let config =
            AppConfig::load_file(path).map_err(|error| io::Error::other(format!("{error}")))?;
        return Ok((Some(path.to_path_buf()), config));
    }

    let candidates = [
        env::var_os("CMDASH_CONFIG").map(PathBuf::from),
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|directory| directory.join("cmdash/config.toml")),
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

fn config_path(args: &[String]) -> io::Result<Option<PathBuf>> {
    let mut path = None;
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--migrate-config" | "--api" | "--api-disable" | "--api-read-only" => {}
            "--api-socket" => {
                let _ = arguments.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--api-socket requires a path")
                })?;
            }
            "--config" | "-c" => {
                let value = arguments.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--config requires a TOML path")
                })?;
                if path.replace(PathBuf::from(value)).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "configuration path was provided more than once",
                    ));
                }
            }
            unknown => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument {unknown:?}; use --config <path>"),
                ));
            }
        }
    }
    Ok(path)
}

fn apply_api_cli_overrides(config: &mut AppConfig, args: &[String]) -> io::Result<()> {
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--api" => config.api.enabled = true,
            "--api-disable" => config.api.enabled = false,
            "--api-read-only" => {
                config.api.enabled = true;
                config.api.read_only = true;
            }
            "--api-socket" => {
                let socket = arguments.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--api-socket requires a path")
                })?;
                config.api.socket = socket.clone();
                config.api.enabled = true;
            }
            _ => {}
        }
    }
    Ok(())
}

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

fn dispatch_available_events(
    state: &mut AppState,
    event_receiver: &Receiver<UiEvent>,
    pty_wakeup: &SessionWakeup,
    registry: &WidgetRegistry,
    reloader: Option<&mut ConfigReloader>,
) -> io::Result<bool> {
    let mut events = Vec::with_capacity(MAX_EVENTS_PER_BATCH);
    collect_ui_event(
        state,
        event_receiver
            .recv()
            .map_err(|_| io::Error::other("input and PTY event channel disconnected"))?,
        &mut events,
        pty_wakeup,
    )?;

    while events.len() < MAX_EVENTS_PER_BATCH {
        match event_receiver.try_recv() {
            Ok(event) => collect_ui_event(state, event, &mut events, pty_wakeup)?,
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(io::Error::other("input and PTY event channel disconnected"));
            }
        }
    }

    dispatch_event_batch(state, registry, reloader, events)
}

fn collect_ui_event(
    state: &mut AppState,
    event: UiEvent,
    events: &mut Vec<Event>,
    pty_wakeup: &SessionWakeup,
) -> io::Result<()> {
    match event {
        UiEvent::Input(event) => events.push(event),
        UiEvent::PtyOutput => pty_wakeup.clear_pending(),
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
        Event::Key(key) => match command_for_key(key) {
            Some(Command::CopySelection) => {
                state.copy_focused_selection();
                Ok(false)
            }
            Some(Command::ReloadConfig) => {
                if let Some(reloader) = reloader {
                    match reloader.reload_with_migrations() {
                        Ok(loaded) => {
                            for migration in &loaded.migrations {
                                state.record_diagnostic(format!(
                                    "config migration: {}",
                                    migration.warning()
                                ));
                            }
                            if let Err(error) = state.reload_config(registry, &loaded.config) {
                                state.record_diagnostic(format!("config reload rejected: {error}"));
                            }
                        }
                        Err(error) => {
                            state.record_diagnostic(format!("config reload failed: {error}"));
                        }
                    }
                } else {
                    state.record_diagnostic("Ctrl+R requires --config <path>");
                }
                Ok(false)
            }
            Some(command) => {
                let effect = state
                    .dispatch(command)
                    .map_err(|error| io::Error::other(format!("command rejected: {error:?}")))?;
                Ok(matches!(effect, cmdash::CommandEffect::Quit))
            }
            None => state
                .handle_focused_key(key)
                .map_err(|error| io::Error::other(format!("widget input rejected: {error}")))
                .map(|_| false),
        },
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
            sixel: false,
        }
    }

    fn tab_event() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
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
    fn config_path_accepts_short_and_long_options() {
        assert_eq!(
            config_path(&["--config".to_owned(), "dashboard.toml".to_owned()]).unwrap(),
            Some(PathBuf::from("dashboard.toml"))
        );
        assert_eq!(
            config_path(&["-c".to_owned(), "dashboard.toml".to_owned()]).unwrap(),
            Some(PathBuf::from("dashboard.toml"))
        );
        assert_eq!(config_path(&[]).unwrap(), None);
    }

    #[test]
    fn config_path_rejects_missing_values_and_unknown_arguments() {
        assert!(config_path(&["--config".to_owned()]).is_err());
        assert!(config_path(&["--api-socket".to_owned()]).is_err());
        assert!(config_path(&["--verbose".to_owned()]).is_err());
    }

    #[test]
    fn api_cli_overrides_have_explicit_precedence() {
        let mut config = AppConfig::parse(
            "version = 1\n[api]\nenabled = true\nread_only = false\nsocket = \"/tmp/from-file.sock\"\n",
        )
        .unwrap();
        apply_api_cli_overrides(
            &mut config,
            &[
                "--api-disable".to_owned(),
                "--api-socket".to_owned(),
                "/tmp/from-cli.sock".to_owned(),
            ],
        )
        .unwrap();
        assert!(config.api.enabled);
        assert_eq!(config.api.socket, "/tmp/from-cli.sock");
        assert!(!config.api.read_only);
    }
}
