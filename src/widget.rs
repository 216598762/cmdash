use std::{
    collections::BTreeMap,
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ratatui::layout::Rect;

use crate::{
    config::{AppConfig, WidgetInstanceConfig},
    scene::{CellStyle, Color, Scene},
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

    fn shutdown(&mut self) {}
}

pub type WidgetFactory = fn(&WidgetInstanceConfig) -> Result<Box<dyn Widget>, WidgetError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WidgetError {
    DuplicateWidgetType(String),
    UnknownWidgetType(String),
    DuplicateWidgetId(WidgetId),
    InvalidConfiguration(String),
    InitializationFailed { kind: String, reason: String },
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
        let mut instances = BTreeMap::new();
        for widget_config in &config.workspace.widgets {
            let id = WidgetId::new(widget_config.id);
            if instances.contains_key(&id) {
                return Err(WidgetError::DuplicateWidgetId(id));
            }
            let mut widget = registry.instantiate(widget_config)?;
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

    pub fn shutdown(&mut self) {
        for entry in self.instances.values_mut() {
            entry.widget.shutdown();
        }
    }

    pub fn render(
        &self,
        areas: &BTreeMap<WidgetId, Rect>,
        focused: Option<WidgetId>,
    ) -> BTreeMap<WidgetId, Scene> {
        self.instances
            .iter()
            .filter_map(|(&id, entry)| {
                areas
                    .get(&id)
                    .map(|&area| (id, entry.widget.render(area, focused == Some(id))))
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
