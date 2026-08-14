use std::{
    collections::BTreeMap,
    env, io,
    path::PathBuf,
    time::{Duration, SystemTime},
};

use cmdash::{
    AppConfig, AppState, Backend, Command, Compositor, CrosstermBackend, Surface, SurfaceCommand,
    WidgetRegistry,
    dashboard::{
        configured_widget_surface_areas, render_static_dashboard_shell_with_metrics_and_health,
        render_static_dashboard_surface_scenes, static_dashboard_surface_areas,
    },
    input::command_for_key,
};
use crossterm::event::{self, Event};
use ratatui::layout::Rect;

const MAX_EVENTS_PER_BATCH: usize = 32;
const DEFAULT_CONFIG: &str = r#"
version = 1

[workspace]
name = "default"

[[workspace.widgets]]
id = 1
type = "text"
title = " workspace "
text = "Dashboard widgets are configuration-driven."

[[workspace.widgets]]
id = 2
type = "clock"
title = " clock "
format = "HH:MM:SS"
"#;

fn main() -> io::Result<()> {
    let mut backend = CrosstermBackend::new(io::stdout());
    let args: Vec<_> = env::args().skip(1).collect();
    let config = load_config(&args)?;
    let registry = WidgetRegistry::builtins();
    let mut state = AppState::from_config(backend.capabilities(), &registry, &config)
        .map_err(|error| io::Error::other(format!("application config rejected: {error}")))?;
    let mut compositor = Compositor::new();
    backend.enter()?;

    let run_result = run(&mut backend, &mut state, &mut compositor);
    state.shutdown_widgets();
    let leave_result = backend.leave();

    run_result.and(leave_result)
}

fn run<B>(backend: &mut B, state: &mut AppState, compositor: &mut Compositor) -> io::Result<()>
where
    B: Backend<Error = io::Error>,
{
    loop {
        state.update_widgets(SystemTime::now());
        let area = backend.size()?;
        sync_dashboard_surfaces(state, area)?;
        let widget_health =
            (!state.widget_runtime().is_empty()).then(|| state.widget_runtime().health_summary());
        let base = render_static_dashboard_shell_with_metrics_and_health(
            area,
            backend.metrics(),
            widget_health.as_deref(),
        );
        let surface_scenes = if state.widget_runtime().is_empty() {
            render_static_dashboard_surface_scenes(area, state.focus())
        } else {
            state.widget_surface_scenes()
        };
        let scene = compositor.compose(area, state, &base, &surface_scenes);
        let diff = compositor.diff(&scene);
        backend.submit_diff(&diff)?;

        if dispatch_available_events(state)? {
            break;
        }
    }

    Ok(())
}

fn load_config(args: &[String]) -> io::Result<AppConfig> {
    match config_path(args)? {
        Some(path) => {
            AppConfig::load_file(&path).map_err(|error| io::Error::other(format!("{error}")))
        }
        None => AppConfig::parse(DEFAULT_CONFIG).map_err(|error| {
            io::Error::other(format!("embedded default config rejected: {error}"))
        }),
    }
}

fn config_path(args: &[String]) -> io::Result<Option<PathBuf>> {
    let mut path = None;
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
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

fn dispatch_available_events(state: &mut AppState) -> io::Result<bool> {
    if !event::poll(Duration::from_millis(250))? {
        return Ok(false);
    }

    let mut events = Vec::with_capacity(MAX_EVENTS_PER_BATCH);
    events.push(event::read()?);
    while events.len() < MAX_EVENTS_PER_BATCH && event::poll(Duration::ZERO)? {
        events.push(event::read()?);
    }

    dispatch_event_batch(state, events)
}

fn dispatch_event_batch<I>(state: &mut AppState, events: I) -> io::Result<bool>
where
    I: IntoIterator<Item = Event>,
{
    for event in events.into_iter().take(MAX_EVENTS_PER_BATCH) {
        if dispatch_event(state, event)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn dispatch_event(state: &mut AppState, event: Event) -> io::Result<bool> {
    match event {
        Event::Key(key) => match command_for_key(key) {
            Some(command) => {
                let effect = state
                    .dispatch(command)
                    .map_err(|error| io::Error::other(format!("command rejected: {error:?}")))?;
                Ok(matches!(effect, cmdash::CommandEffect::Quit))
            }
            None => Ok(false),
        },
        _ => Ok(false),
    }
}

fn sync_dashboard_surfaces(state: &mut AppState, area: Rect) -> io::Result<()> {
    let surface_areas: BTreeMap<_, _> = if state.widget_runtime().is_empty() {
        static_dashboard_surface_areas(area).into_iter().collect()
    } else {
        let surface_ids: Vec<_> = state.workspace().surfaces().keys().copied().collect();
        configured_widget_surface_areas(area, &surface_ids)
    };

    for (id, surface_area) in surface_areas {
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cmdash::{BackendCapabilities, FocusTarget, SurfaceId};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn capabilities() -> BackendCapabilities {
        BackendCapabilities {
            truecolor: true,
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: false,
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
        assert!(!dispatch_event_batch(&mut state, events).unwrap());
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

        assert!(dispatch_event_batch(&mut state, events).unwrap());
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
        assert!(config_path(&["--verbose".to_owned()]).is_err());
    }
}
