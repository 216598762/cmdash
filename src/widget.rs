use std::{
    collections::BTreeMap,
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;

use crate::{
    config::{AppConfig, WidgetInstanceConfig},
    graphics::GraphicsSubmission,
    plugin::PluginRegistry,
    scene::{CellStyle, Color, Scene},
    session::{TerminalSession, TerminalSize},
    state::WidgetId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WidgetUpdate {
    Unchanged,
    Redraw,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WidgetHealth {
    Healthy,
    Degraded(String),
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WidgetStatus {
    id: WidgetId,
    kind: String,
    health: WidgetHealth,
}

impl WidgetStatus {
    pub const fn id(&self) -> WidgetId {
        self.id
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn health(&self) -> &WidgetHealth {
        &self.health
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WidgetUpdateReport {
    changed: Vec<WidgetId>,
    failed: Vec<WidgetId>,
}

impl WidgetUpdateReport {
    pub fn changed(&self) -> &[WidgetId] {
        &self.changed
    }

    pub fn failed(&self) -> &[WidgetId] {
        &self.failed
    }

    pub const fn requests_redraw(&self) -> bool {
        !self.changed.is_empty()
    }
}

pub trait Widget: Send {
    fn kind(&self) -> &str;

    fn initialize(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn update(&mut self, _now: SystemTime) -> Result<WidgetUpdate, String> {
        Ok(WidgetUpdate::Unchanged)
    }

    fn health(&self) -> WidgetHealth {
        WidgetHealth::Healthy
    }

    fn render(&self, area: Rect, focused: bool) -> Scene;

    fn graphics(&self, _area: Rect) -> Vec<GraphicsSubmission> {
        Vec::new()
    }

    fn handles_input(&self) -> bool {
        false
    }

    fn handle_key(&mut self, _key: KeyEvent) -> Result<WidgetUpdate, String> {
        Ok(WidgetUpdate::Unchanged)
    }

    fn resize(&mut self, _size: TerminalSize) -> Result<WidgetUpdate, String> {
        Ok(WidgetUpdate::Unchanged)
    }

    fn handle_paste(&mut self, _text: &str) -> Result<WidgetUpdate, String> {
        Ok(WidgetUpdate::Unchanged)
    }

    fn copy_selection(&self, _area: Rect) -> Option<String> {
        None
    }

    fn handle_mouse(
        &mut self,
        _mouse: MouseEvent,
        _origin: (u16, u16),
    ) -> Result<WidgetUpdate, String> {
        Ok(WidgetUpdate::Unchanged)
    }

    fn shutdown(&mut self) -> Result<(), String> {
        Ok(())
    }
}

pub type WidgetFactory = fn(&WidgetInstanceConfig) -> Result<Box<dyn Widget>, WidgetError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WidgetError {
    DuplicateWidgetType(String),
    UnknownWidgetType(String),
    DuplicateWidgetId(WidgetId),
    InvalidConfiguration(String),
    InitializationFailed { kind: String, reason: String },
    Plugin(String),
}

impl fmt::Display for WidgetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateWidgetType(kind) => write!(formatter, "duplicate widget type {kind:?}"),
            Self::UnknownWidgetType(kind) => write!(formatter, "unknown widget type {kind:?}"),
            Self::DuplicateWidgetId(id) => write!(formatter, "duplicate widget id {}", id.get()),
            Self::InvalidConfiguration(message) => formatter.write_str(message),
            Self::InitializationFailed { kind, reason } => {
                write!(formatter, "failed to initialize {kind:?} widget: {reason}")
            }
            Self::Plugin(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for WidgetError {}

#[derive(Default)]
pub struct WidgetRegistry {
    factories: BTreeMap<String, WidgetFactory>,
}

impl WidgetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn builtins() -> Self {
        let mut registry = Self::new();
        registry
            .register("text", text_widget_factory)
            .expect("built-in widget types are unique");
        registry
            .register("clock", clock_widget_factory)
            .expect("built-in widget types are unique");
        registry
            .register("system", system_widget_factory)
            .expect("built-in widget types are unique");
        registry
            .register("terminal", terminal_widget_factory)
            .expect("built-in widget types are unique");
        registry
    }

    pub fn register(
        &mut self,
        kind: impl Into<String>,
        factory: WidgetFactory,
    ) -> Result<(), WidgetError> {
        let kind = kind.into();
        if self.factories.contains_key(&kind) {
            return Err(WidgetError::DuplicateWidgetType(kind));
        }
        self.factories.insert(kind, factory);
        Ok(())
    }

    pub fn contains(&self, kind: &str) -> bool {
        self.factories.contains_key(kind)
    }

    fn instantiate(&self, config: &WidgetInstanceConfig) -> Result<Box<dyn Widget>, WidgetError> {
        let factory = self
            .factories
            .get(&config.kind)
            .ok_or_else(|| WidgetError::UnknownWidgetType(config.kind.clone()))?;
        factory(config)
    }
}

struct WidgetEntry {
    widget: Box<dyn Widget>,
    health: WidgetHealth,
}

pub struct WidgetRuntime {
    instances: BTreeMap<WidgetId, WidgetEntry>,
}

impl WidgetRuntime {
    pub fn empty() -> Self {
        Self {
            instances: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    pub fn from_config(registry: &WidgetRegistry, config: &AppConfig) -> Result<Self, WidgetError> {
        Self::from_config_with_plugins(registry, None, config)
    }

    pub fn from_config_with_plugins(
        registry: &WidgetRegistry,
        plugins: Option<&PluginRegistry>,
        config: &AppConfig,
    ) -> Result<Self, WidgetError> {
        let mut instances = BTreeMap::new();
        for widget_config in &config.workspace.widgets {
            let id = WidgetId::new(widget_config.id);
            if instances.contains_key(&id) {
                return Err(WidgetError::DuplicateWidgetId(id));
            }
            let mut widget = if registry.contains(&widget_config.kind) {
                registry.instantiate(widget_config)?
            } else if let Some(plugins) = plugins {
                plugins
                    .instantiate(widget_config)
                    .map_err(|error| WidgetError::Plugin(error.to_string()))?
            } else {
                return Err(WidgetError::UnknownWidgetType(widget_config.kind.clone()));
            };
            let kind = widget.kind().to_owned();
            widget
                .initialize()
                .map_err(|reason| WidgetError::InitializationFailed { kind, reason })?;
            let health = widget.health();
            instances.insert(id, WidgetEntry { widget, health });
        }
        Ok(Self { instances })
    }

    pub fn widget_ids(&self) -> impl Iterator<Item = WidgetId> + '_ {
        self.instances.keys().copied()
    }

    pub fn widget_kind(&self, id: WidgetId) -> Option<&str> {
        self.instances.get(&id).map(|entry| entry.widget.kind())
    }

    pub fn statuses(&self) -> impl Iterator<Item = WidgetStatus> + '_ {
        self.instances.iter().map(|(&id, entry)| WidgetStatus {
            id,
            kind: entry.widget.kind().to_owned(),
            health: entry.health.clone(),
        })
    }

    pub fn health(&self, id: WidgetId) -> Option<&WidgetHealth> {
        self.instances.get(&id).map(|entry| &entry.health)
    }

    pub fn health_summary(&self) -> String {
        let mut healthy = 0;
        let mut degraded = 0;
        let mut failed = 0;
        for entry in self.instances.values() {
            match entry.health {
                WidgetHealth::Healthy => healthy += 1,
                WidgetHealth::Degraded(_) => degraded += 1,
                WidgetHealth::Failed(_) => failed += 1,
            }
        }

        if healthy + degraded + failed == 0 {
            return "no widgets".to_owned();
        }
        let mut parts = Vec::new();
        if healthy > 0 {
            parts.push(format!("{healthy} healthy"));
        }
        if degraded > 0 {
            parts.push(format!("{degraded} degraded"));
        }
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        parts.join(", ")
    }

    pub fn update(&mut self, now: SystemTime) -> WidgetUpdateReport {
        let mut report = WidgetUpdateReport::default();
        for (&id, entry) in &mut self.instances {
            match entry.widget.update(now) {
                Ok(WidgetUpdate::Unchanged) => {
                    entry.health = entry.widget.health();
                }
                Ok(WidgetUpdate::Redraw) => {
                    entry.health = entry.widget.health();
                    report.changed.push(id);
                }
                Err(reason) => {
                    entry.health = WidgetHealth::Failed(reason);
                    report.failed.push(id);
                }
            }
        }
        report
    }

    pub fn handle_key(&mut self, id: WidgetId, key: KeyEvent) -> Result<WidgetUpdate, String> {
        let entry = self
            .instances
            .get_mut(&id)
            .ok_or_else(|| format!("widget {} is not registered", id.get()))?;
        match entry.widget.handle_key(key) {
            Ok(update) => {
                entry.health = entry.widget.health();
                Ok(update)
            }
            Err(error) => {
                entry.health = WidgetHealth::Failed(error.clone());
                Err(error)
            }
        }
    }

    pub fn handles_input(&self, id: WidgetId) -> bool {
        self.instances
            .get(&id)
            .is_some_and(|entry| entry.widget.handles_input())
    }

    pub fn handle_paste(&mut self, id: WidgetId, text: &str) -> Result<WidgetUpdate, String> {
        let entry = self
            .instances
            .get_mut(&id)
            .ok_or_else(|| format!("widget {} is not registered", id.get()))?;
        entry.widget.handle_paste(text)
    }

    pub fn copy_selection(&self, id: WidgetId, area: Rect) -> Option<String> {
        self.instances
            .get(&id)
            .and_then(|entry| entry.widget.copy_selection(area))
    }

    pub fn handle_mouse(
        &mut self,
        id: WidgetId,
        mouse: MouseEvent,
        origin: (u16, u16),
    ) -> Result<WidgetUpdate, String> {
        let entry = self
            .instances
            .get_mut(&id)
            .ok_or_else(|| format!("widget {} is not registered", id.get()))?;
        entry.widget.handle_mouse(mouse, origin)
    }

    pub fn resize(&mut self, id: WidgetId, size: TerminalSize) -> Result<WidgetUpdate, String> {
        let entry = self
            .instances
            .get_mut(&id)
            .ok_or_else(|| format!("widget {} is not registered", id.get()))?;
        match entry.widget.resize(size) {
            Ok(update) => {
                entry.health = entry.widget.health();
                Ok(update)
            }
            Err(error) => {
                entry.health = WidgetHealth::Failed(error.clone());
                Err(error)
            }
        }
    }

    pub fn shutdown_widget(&mut self, id: WidgetId) -> Result<(), String> {
        let Some(mut entry) = self.instances.remove(&id) else {
            return Err(format!("widget {} is not registered", id.get()));
        };
        entry.widget.shutdown()
    }

    pub fn shutdown(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        for entry in self.instances.values_mut() {
            if let Err(error) = entry.widget.shutdown() {
                entry.health = WidgetHealth::Failed(error.clone());
                failures.push(error);
            }
        }
        failures
    }

    pub fn graphics(&self, areas: &BTreeMap<WidgetId, Rect>) -> Vec<GraphicsSubmission> {
        areas
            .iter()
            .flat_map(|(&id, &area)| {
                self.instances
                    .get(&id)
                    .into_iter()
                    .flat_map(move |entry| entry.widget.graphics(area))
            })
            .collect()
    }

    pub fn render(
        &self,
        areas: &BTreeMap<WidgetId, Rect>,
        focused: Option<WidgetId>,
    ) -> BTreeMap<WidgetId, Scene> {
        self.instances
            .iter()
            .filter_map(|(&id, entry)| {
                let area = *areas.get(&id)?;
                let mut scene = entry.widget.render(area, focused == Some(id));
                for graphics in entry.widget.graphics(area) {
                    scene.add_image_layer(graphics);
                }
                Some((id, scene))
            })
            .collect()
    }
}

struct TextWidget {
    title: String,
    text: String,
}

impl Widget for TextWidget {
    fn kind(&self) -> &str {
        "text"
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = Color::rgb(27, 33, 44);
        let foreground = Color::rgb(226, 232, 240);
        let accent = if focused {
            Color::rgb(250, 204, 21)
        } else {
            Color::rgb(125, 211, 252)
        };
        let mut scene = Scene::new(area);
        scene.fill(area, CellStyle::new(foreground, background));
        scene.border(area, &self.title, CellStyle::new(accent, background));
        if area.height > 2 {
            scene.text(
                area.x.saturating_add(2),
                area.y.saturating_add(1),
                &self.text,
                CellStyle::new(foreground, background),
            );
        }
        scene
    }
}

fn text_widget_factory(config: &WidgetInstanceConfig) -> Result<Box<dyn Widget>, WidgetError> {
    Ok(Box::new(TextWidget {
        title: config.title.clone().unwrap_or_else(|| " text ".to_owned()),
        text: config.text.clone().unwrap_or_default(),
    }))
}

struct ClockWidget {
    title: String,
    format: ClockFormat,
    text: String,
}

#[derive(Clone, Copy)]
enum ClockFormat {
    HoursMinutes,
    HoursMinutesSeconds,
}

impl ClockWidget {
    fn display(&self, now: SystemTime) -> String {
        let seconds = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs()
            % 86_400;
        let hours = seconds / 3_600;
        let minutes = (seconds % 3_600) / 60;
        let seconds = seconds % 60;
        match self.format {
            ClockFormat::HoursMinutes => format!("{hours:02}:{minutes:02}"),
            ClockFormat::HoursMinutesSeconds => format!("{hours:02}:{minutes:02}:{seconds:02}"),
        }
    }
}

impl Widget for ClockWidget {
    fn kind(&self) -> &str {
        "clock"
    }

    fn update(&mut self, now: SystemTime) -> Result<WidgetUpdate, String> {
        let text = self.display(now);
        if text == self.text {
            Ok(WidgetUpdate::Unchanged)
        } else {
            self.text = text;
            Ok(WidgetUpdate::Redraw)
        }
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = Color::rgb(27, 33, 44);
        let foreground = Color::rgb(226, 232, 240);
        let accent = if focused {
            Color::rgb(250, 204, 21)
        } else {
            Color::rgb(134, 239, 172)
        };
        let mut scene = Scene::new(area);
        scene.fill(area, CellStyle::new(foreground, background));
        scene.border(area, &self.title, CellStyle::new(accent, background));
        if area.height > 2 {
            scene.text(
                area.x.saturating_add(2),
                area.y.saturating_add(1),
                &self.text,
                CellStyle::new(foreground, background).bold(),
            );
        }
        scene
    }
}

struct SystemWidget {
    title: String,
    text: String,
}

impl Widget for SystemWidget {
    fn kind(&self) -> &str {
        "system"
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = Color::rgb(27, 45, 44);
        let foreground = Color::rgb(220, 252, 231);
        let accent = if focused {
            Color::rgb(250, 204, 21)
        } else {
            Color::rgb(110, 231, 183)
        };
        let mut scene = Scene::new(area);
        scene.fill(area, CellStyle::new(foreground, background));
        scene.border(area, &self.title, CellStyle::new(accent, background));
        if area.height > 2 {
            scene.text(
                area.x.saturating_add(2),
                area.y.saturating_add(1),
                &self.text,
                CellStyle::new(foreground, background),
            );
        }
        scene
    }
}

struct TerminalWidget {
    title: String,
    session: TerminalSession,
}

impl Widget for TerminalWidget {
    fn kind(&self) -> &str {
        "terminal"
    }

    fn initialize(&mut self) -> Result<(), String> {
        self.session
            .poll_output()
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn update(&mut self, _now: SystemTime) -> Result<WidgetUpdate, String> {
        self.session
            .poll_output()
            .map(|changed| {
                if changed {
                    WidgetUpdate::Redraw
                } else {
                    WidgetUpdate::Unchanged
                }
            })
            .map_err(|error| error.to_string())
    }

    fn health(&self) -> WidgetHealth {
        if let Some(error) = self.session.failure() {
            WidgetHealth::Failed(error.to_owned())
        } else if let Some(diagnostic) = self.session.graphics_diagnostics().last() {
            WidgetHealth::Degraded(diagnostic.message().to_owned())
        } else {
            WidgetHealth::Healthy
        }
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let mut scene = self.session.render(area, focused);
        let color = if focused {
            Color::rgb(250, 204, 21)
        } else {
            Color::rgb(216, 180, 254)
        };
        scene.border(
            area,
            &self.title,
            CellStyle::new(color, Color::rgb(18, 22, 30)),
        );
        scene
    }

    fn graphics(&self, area: Rect) -> Vec<GraphicsSubmission> {
        self.session.graphics(area)
    }

    fn copy_selection(&self, area: Rect) -> Option<String> {
        self.session.selected_text(area)
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<WidgetUpdate, String> {
        self.session
            .write_key(key)
            .map(|_| WidgetUpdate::Unchanged)
            .map_err(|error| error.to_string())
    }

    fn resize(&mut self, size: TerminalSize) -> Result<WidgetUpdate, String> {
        if self.session.size() == size {
            return Ok(WidgetUpdate::Unchanged);
        }
        self.session
            .resize(size)
            .map(|_| WidgetUpdate::Redraw)
            .map_err(|error| error.to_string())
    }

    fn handle_paste(&mut self, text: &str) -> Result<WidgetUpdate, String> {
        self.session
            .write_paste(text)
            .map(|_| WidgetUpdate::Unchanged)
            .map_err(|error| error.to_string())
    }

    fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        origin: (u16, u16),
    ) -> Result<WidgetUpdate, String> {
        let position = (
            mouse.column.saturating_sub(origin.0),
            mouse.row.saturating_sub(origin.1),
        );
        match mouse.kind {
            crossterm::event::MouseEventKind::Down(_) => self.session.begin_selection(position),
            crossterm::event::MouseEventKind::Drag(_) => self.session.update_selection(position),
            _ => {}
        }
        self.session
            .write_mouse(mouse, origin)
            .map(|_| WidgetUpdate::Unchanged)
            .map_err(|error| error.to_string())
    }

    fn shutdown(&mut self) -> Result<(), String> {
        self.session.shutdown().map_err(|error| error.to_string())
    }
}

fn terminal_widget_factory(config: &WidgetInstanceConfig) -> Result<Box<dyn Widget>, WidgetError> {
    let session = TerminalSession::spawn_with_session_id(
        crate::state::SessionId::new(config.id),
        config.command.as_deref(),
        &[],
        TerminalSize::new(80, 24),
    )
    .map_err(|error| WidgetError::InitializationFailed {
        kind: "terminal".to_owned(),
        reason: error.to_string(),
    })?;
    Ok(Box::new(TerminalWidget {
        title: config
            .title
            .clone()
            .unwrap_or_else(|| " terminal ".to_owned()),
        session,
    }))
}

fn system_widget_factory(config: &WidgetInstanceConfig) -> Result<Box<dyn Widget>, WidgetError> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    Ok(Box::new(SystemWidget {
        title: config
            .title
            .clone()
            .unwrap_or_else(|| " system ".to_owned()),
        text: format!("{os} / {arch}"),
    }))
}

fn clock_widget_factory(config: &WidgetInstanceConfig) -> Result<Box<dyn Widget>, WidgetError> {
    let format = match config.format.as_deref().unwrap_or("HH:MM:SS") {
        "HH:MM" => ClockFormat::HoursMinutes,
        "HH:MM:SS" => ClockFormat::HoursMinutesSeconds,
        format => {
            return Err(WidgetError::InvalidConfiguration(format!(
                "clock format must be HH:MM or HH:MM:SS, got {format:?}"
            )));
        }
    };
    let mut widget = ClockWidget {
        title: config.title.clone().unwrap_or_else(|| " clock ".to_owned()),
        format,
        text: String::new(),
    };
    let _ = widget.update(SystemTime::now());
    Ok(Box::new(widget))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWidget;

    impl Widget for FailingWidget {
        fn kind(&self) -> &str {
            "failing"
        }

        fn update(&mut self, _now: SystemTime) -> Result<WidgetUpdate, String> {
            Err("update failed".to_owned())
        }

        fn render(&self, area: Rect, _focused: bool) -> Scene {
            Scene::new(area)
        }
    }

    fn failing_widget_factory(
        _config: &WidgetInstanceConfig,
    ) -> Result<Box<dyn Widget>, WidgetError> {
        Ok(Box::new(FailingWidget))
    }

    #[test]
    fn builtins_register_and_render_configured_text_widgets() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 4
            type = "text"
            title = " greeting "
            text = "hello"
            "#,
        )
        .unwrap();
        let registry = WidgetRegistry::builtins();
        let runtime = WidgetRuntime::from_config(&registry, &config).unwrap();
        let areas = BTreeMap::from([(WidgetId::new(4), Rect::new(0, 0, 16, 4))]);

        assert_eq!(runtime.widget_ids().collect::<Vec<_>>(), [WidgetId::new(4)]);
        assert_eq!(runtime.widget_kind(WidgetId::new(4)), Some("text"));
        let scenes = runtime.render(&areas, Some(WidgetId::new(4)));
        assert_eq!(scenes[&WidgetId::new(4)].cell_at(2, 1).unwrap().symbol, 'h');
    }

    #[test]
    fn clock_updates_and_renders_a_deterministic_utc_value() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 4
            type = "clock"
            format = "HH:MM:SS"
            "#,
        )
        .unwrap();
        let mut runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let report = runtime.update(UNIX_EPOCH + Duration::from_secs(3_723));
        let areas = BTreeMap::from([(WidgetId::new(4), Rect::new(0, 0, 16, 4))]);
        let scenes = runtime.render(&areas, None);

        assert_eq!(report.changed(), &[WidgetId::new(4)]);
        assert_eq!(scenes[&WidgetId::new(4)].cell_at(2, 1).unwrap().symbol, '0');
        assert_eq!(scenes[&WidgetId::new(4)].cell_at(9, 1).unwrap().symbol, '3');
    }

    #[test]
    fn system_widget_renders_platform_information() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 5
            type = "system"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let areas = BTreeMap::from([(WidgetId::new(5), Rect::new(0, 0, 24, 4))]);
        let scenes = runtime.render(&areas, None);

        assert_eq!(
            scenes[&WidgetId::new(5)].cell_at(2, 1).unwrap().symbol,
            std::env::consts::OS.chars().next().unwrap()
        );
    }

    #[test]
    fn terminal_widget_owns_a_session_and_accepts_input() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 6
            type = "terminal"
            command = "sh"
            "#,
        )
        .unwrap();
        let mut runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let id = WidgetId::new(6);

        assert!(runtime.handles_input(id));
        assert_eq!(
            runtime
                .handle_key(
                    id,
                    KeyEvent::new(
                        crossterm::event::KeyCode::Char('x'),
                        crossterm::event::KeyModifiers::NONE
                    )
                )
                .unwrap(),
            WidgetUpdate::Unchanged
        );
        let _ = runtime.shutdown();
    }

    #[test]
    fn unknown_widget_types_are_rejected_by_the_registry() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 1
            type = "missing"
            "#,
        )
        .unwrap();

        assert_eq!(
            WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).err(),
            Some(WidgetError::UnknownWidgetType("missing".to_owned()))
        );
    }

    #[test]
    fn failed_updates_are_recorded_in_health_status() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 8
            type = "failing"
            "#,
        )
        .unwrap();
        let mut registry = WidgetRegistry::builtins();
        registry
            .register("failing", failing_widget_factory)
            .unwrap();
        let mut runtime = WidgetRuntime::from_config(&registry, &config).unwrap();

        let report = runtime.update(SystemTime::now());

        assert_eq!(report.failed(), &[WidgetId::new(8)]);
        assert_eq!(
            runtime.health(WidgetId::new(8)),
            Some(&WidgetHealth::Failed("update failed".to_owned()))
        );
        assert_eq!(runtime.health_summary(), "1 failed");
    }

    #[test]
    fn invalid_clock_formats_are_rejected() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 1
            type = "clock"
            format = "invalid"
            "#,
        )
        .unwrap();

        assert_eq!(
            WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).err(),
            Some(WidgetError::InvalidConfiguration(
                "clock format must be HH:MM or HH:MM:SS, got \"invalid\"".to_owned()
            ))
        );
    }

    #[test]
    fn rendering_omits_instances_without_a_layout_area() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 1
            type = "text"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();

        assert!(runtime.render(&BTreeMap::new(), None).is_empty());
    }
}
