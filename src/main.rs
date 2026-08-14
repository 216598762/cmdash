use std::{
    collections::{BTreeMap, BTreeSet},
    env, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use cmdash::{
    AppConfig, AppState, Backend, Command, Compositor, CrosstermBackend, Surface, SurfaceCommand,
    SurfaceId, WidgetRegistry,
    dashboard::{
        render_static_dashboard_shell_with_metrics_health_and_diagnostic,
        render_static_dashboard_surface_scenes, static_dashboard_surface_areas,
    },
    input::command_for_key,
    reload::ConfigReloader,
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

[[workspace.widgets]]
id = 3
type = "system"
title = " system "

[[workspace.overlays]]
id = 9
x = 2
y = 4
width = 30
height = 4
z_index = 10
title = " notice "
text = "Phase 2 widget host"

[workspace.layout]
type = "stack"
children = [
  { type = "tabs", active = 0, children = [
    { type = "columns", children = [
      { type = "leaf", widget = 1 },
      { type = "leaf", widget = 2 },
      { type = "leaf", widget = 3 }
    ] }
  ] },
  { type = "overlay", overlay = 9 }
]
"#;

fn main() -> io::Result<()> {
    let mut backend = CrosstermBackend::new(io::stdout());
    let args: Vec<_> = env::args().skip(1).collect();
    let config_path = config_path(&args)?;
    let config = load_config(config_path.as_deref())?;
    let registry = WidgetRegistry::builtins();
    let mut state = AppState::from_config(backend.capabilities(), &registry, &config)
        .map_err(|error| io::Error::other(format!("application config rejected: {error}")))?;
    let mut compositor = Compositor::new();
    let mut reloader = config_path
        .map(ConfigReloader::new)
        .transpose()
        .map_err(|error| io::Error::other(error.to_string()))?;
    backend.enter()?;

    let run_result = run(
        &mut backend,
        &mut state,
        &mut compositor,
        &registry,
        reloader.as_mut(),
    );
    state.shutdown_widgets();
    let leave_result = backend.leave();

    run_result.and(leave_result)
}

fn run<B>(
    backend: &mut B,
    state: &mut AppState,
    compositor: &mut Compositor,
    registry: &WidgetRegistry,
    mut reloader: Option<&mut ConfigReloader>,
) -> io::Result<()>
where
    B: Backend<Error = io::Error>,
{
    loop {
        if let Some(reloader) = reloader.as_deref_mut() {
            match reloader.poll() {
                Ok(Some(config)) => {
                    if let Err(error) = state.reload_config(registry, &config) {
                        state.record_diagnostic(format!("config reload rejected: {error}"));
                    }
                }
                Ok(None) => {}
                Err(error) => state.record_diagnostic(format!("config reload failed: {error}")),
            }
        }
        state.update_widgets(SystemTime::now());
        for invalidation in state.take_surface_invalidations() {
            compositor.invalidate(invalidation);
        }
        let area = backend.size()?;
        sync_dashboard_surfaces(state, area)?;
        let widget_health =
            (!state.widget_runtime().is_empty()).then(|| state.widget_runtime().health_summary());
        let base = render_static_dashboard_shell_with_metrics_health_and_diagnostic(
            area,
            backend.metrics(),
            widget_health.as_deref(),
            state.latest_diagnostic(),
        );
        let surface_scenes = if state.widget_runtime().is_empty() {
            render_static_dashboard_surface_scenes(area, state.focus())
        } else {
            state.widget_surface_scenes()
        };
        let scene = compositor.compose(area, state, &base, &surface_scenes);
        let diff = compositor.diff(&scene);
        backend.submit_diff(&diff)?;
        backend.submit_graphics(diff.graphics(), diff.removed_graphics())?;

        if dispatch_available_events(state, registry, reloader.as_deref_mut())? {
            break;
        }
    }

    Ok(())
}

fn load_config(path: Option<&Path>) -> io::Result<AppConfig> {
    match path {
        Some(path) => {
            AppConfig::load_file(path).map_err(|error| io::Error::other(format!("{error}")))
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

fn dispatch_available_events(
    state: &mut AppState,
    registry: &WidgetRegistry,
    reloader: Option<&mut ConfigReloader>,
) -> io::Result<bool> {
    if !event::poll(Duration::from_millis(250))? {
        return Ok(false);
    }

    let mut events = Vec::with_capacity(MAX_EVENTS_PER_BATCH);
    events.push(event::read()?);
    while events.len() < MAX_EVENTS_PER_BATCH && event::poll(Duration::ZERO)? {
        events.push(event::read()?);
    }

    dispatch_event_batch(state, registry, reloader, events)
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
            Some(Command::ReloadConfig) => {
                if let Some(reloader) = reloader {
                    match reloader.reload() {
                        Ok(config) => {
                            if let Err(error) = state.reload_config(registry, &config) {
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

fn sync_dashboard_surfaces(state: &mut AppState, area: Rect) -> io::Result<()> {
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
        .resize_widget_surfaces(&surface_areas)
        .map_err(|error| io::Error::other(format!("widget resize rejected: {error}")))?;
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
        assert!(config_path(&["--verbose".to_owned()]).is_err());
    }
}
