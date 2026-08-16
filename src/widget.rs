use std::{
    collections::BTreeMap,
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;

#[cfg(feature = "sixel")]
use crate::sixel::SixelSubmission;
use crate::{
    animation::{AnimationFrame, AnimationSettings},
    appearance::Theme,
    config::{AppConfig, LabelPolicy, WidgetInstanceConfig},
    graphics::{GraphicsPlaceholderLayer, GraphicsSubmission},
    plugin::PluginRegistry,
    scene::{CellStyle, Color, Scene},
    session::{SessionWakeup, TerminalSession, TerminalSize},
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WidgetBorderStyle {
    Rounded,
    Square,
    Double,
    Heavy,
    Ascii,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WidgetAppearance {
    padding: u16,
    border: WidgetBorderStyle,
}

impl Default for WidgetAppearance {
    fn default() -> Self {
        Self {
            padding: 0,
            border: WidgetBorderStyle::Rounded,
        }
    }
}

impl WidgetAppearance {
    pub fn from_settings(settings: &BTreeMap<String, String>) -> Result<Self, WidgetError> {
        let padding = settings
            .get("padding")
            .map(|value| {
                value.parse::<u16>().map_err(|_| {
                    WidgetError::InvalidConfiguration(format!(
                        "widget padding must be a non-negative integer, got {value:?}"
                    ))
                })
            })
            .transpose()?
            .unwrap_or(0);
        let border = settings
            .get("border")
            .or_else(|| settings.get("border_style"))
            .map(|value| match value.to_ascii_lowercase().as_str() {
                "rounded" => Ok(WidgetBorderStyle::Rounded),
                "square" => Ok(WidgetBorderStyle::Square),
                "double" => Ok(WidgetBorderStyle::Double),
                "heavy" => Ok(WidgetBorderStyle::Heavy),
                "ascii" => Ok(WidgetBorderStyle::Ascii),
                "none" => Ok(WidgetBorderStyle::None),
                _ => Err(WidgetError::InvalidConfiguration(format!(
                    "widget border must be rounded, square, double, heavy, ascii, or none, got {value:?}"
                ))),
            })
            .transpose()?
            .unwrap_or(WidgetBorderStyle::Rounded);
        Ok(Self { padding, border })
    }

    pub const fn padding(self) -> u16 {
        self.padding
    }

    pub const fn border(self) -> WidgetBorderStyle {
        self.border
    }

    pub fn content_area(self, area: Rect) -> Rect {
        let border_inset = if self.border == WidgetBorderStyle::None {
            0
        } else {
            1
        };
        let inset = self.padding.saturating_add(border_inset);
        Rect::new(
            area.x.saturating_add(inset),
            area.y.saturating_add(inset),
            area.width.saturating_sub(inset.saturating_mul(2)),
            area.height.saturating_sub(inset.saturating_mul(2)),
        )
    }

    pub fn render_border(self, scene: &mut Scene, area: Rect, title: &str, style: CellStyle) {
        let Some(glyphs) = self.border.glyphs() else {
            return;
        };
        if area.width == 0 || area.height == 0 {
            return;
        }
        let right = area.x.saturating_add(area.width.saturating_sub(1));
        let bottom = area.y.saturating_add(area.height.saturating_sub(1));
        for x in area.x..=right {
            scene.set(x, area.y, glyphs.horizontal, style);
            scene.set(x, bottom, glyphs.horizontal, style);
        }
        for y in area.y..=bottom {
            scene.set(area.x, y, glyphs.vertical, style);
            scene.set(right, y, glyphs.vertical, style);
        }
        if area.width >= 2 && area.height >= 2 {
            scene.set(area.x, area.y, glyphs.top_left, style);
            scene.set(right, area.y, glyphs.top_right, style);
            scene.set(area.x, bottom, glyphs.bottom_left, style);
            scene.set(right, bottom, glyphs.bottom_right, style);
        }
        if area.width > 4 {
            scene.text(area.x.saturating_add(2), area.y, title, style.bold());
        }
    }
}

/// Cursor blinking behavior for an interactive terminal widget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorBlinkSettings {
    enabled: bool,
    interval: Duration,
}

impl Default for CursorBlinkSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: Duration::from_millis(500),
        }
    }
}

impl CursorBlinkSettings {
    pub fn from_settings(settings: &BTreeMap<String, String>) -> Result<Self, WidgetError> {
        let enabled = settings
            .get("cursor_blink")
            .map(|value| {
                value.parse::<bool>().map_err(|_| {
                    WidgetError::InvalidConfiguration(format!(
                        "terminal cursor_blink must be true or false, got {value:?}"
                    ))
                })
            })
            .transpose()?
            .unwrap_or(true);
        let interval_ms = settings
            .get("cursor_blink_interval_ms")
            .map(|value| {
                value.parse::<u64>().map_err(|_| {
                    WidgetError::InvalidConfiguration(format!(
                        "terminal cursor_blink_interval_ms must be an integer, got {value:?}"
                    ))
                })
            })
            .transpose()?
            .unwrap_or(500);
        if !(50..=60_000).contains(&interval_ms) {
            return Err(WidgetError::InvalidConfiguration(format!(
                "terminal cursor_blink_interval_ms must be between 50 and 60000, got {interval_ms}"
            )));
        }
        Ok(Self {
            enabled,
            interval: Duration::from_millis(interval_ms),
        })
    }

    pub const fn enabled(self) -> bool {
        self.enabled
    }

    pub const fn interval(self) -> Duration {
        self.interval
    }
}

#[derive(Clone, Copy)]
struct BorderGlyphs {
    horizontal: char,
    vertical: char,
    top_left: char,
    top_right: char,
    bottom_left: char,
    bottom_right: char,
}

impl WidgetBorderStyle {
    const fn glyphs(self) -> Option<BorderGlyphs> {
        match self {
            Self::Rounded => Some(BorderGlyphs {
                horizontal: '─',
                vertical: '│',
                top_left: '╭',
                top_right: '╮',
                bottom_left: '╰',
                bottom_right: '╯',
            }),
            Self::Square => Some(BorderGlyphs {
                horizontal: '─',
                vertical: '│',
                top_left: '┌',
                top_right: '┐',
                bottom_left: '└',
                bottom_right: '┘',
            }),
            Self::Double => Some(BorderGlyphs {
                horizontal: '═',
                vertical: '║',
                top_left: '╔',
                top_right: '╗',
                bottom_left: '╚',
                bottom_right: '╝',
            }),
            Self::Heavy => Some(BorderGlyphs {
                horizontal: '━',
                vertical: '┃',
                top_left: '┏',
                top_right: '┓',
                bottom_left: '┗',
                bottom_right: '┛',
            }),
            Self::Ascii => Some(BorderGlyphs {
                horizontal: '-',
                vertical: '|',
                top_left: '+',
                top_right: '+',
                bottom_left: '+',
                bottom_right: '+',
            }),
            Self::None => None,
        }
    }
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

    fn render_with_cursor(&self, area: Rect, focused: bool, _cursor_visible: bool) -> Scene {
        self.render(area, focused)
    }

    fn render_with_animation(
        &self,
        area: Rect,
        focused: bool,
        cursor_visible: bool,
        _animation: AnimationFrame,
    ) -> Scene {
        self.render_with_cursor(area, focused, cursor_visible)
    }

    fn cursor_blink_settings(&self) -> Option<CursorBlinkSettings> {
        None
    }

    fn content_area(&self, area: Rect) -> Rect {
        widget_content_area(area)
    }

    fn graphics(&self, _area: Rect) -> Vec<GraphicsSubmission> {
        Vec::new()
    }

    #[cfg(feature = "sixel")]
    fn sixel(&self, _area: Rect) -> Vec<SixelSubmission> {
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

/// Construction-time services shared by registered widget factories.
///
/// The context is intentionally capability-oriented: factories can use only
/// services exposed here, rather than reaching into application state or
/// global runtime handles.
#[derive(Clone, Default)]
pub struct WidgetRuntimeContext {
    session_wakeup: Option<SessionWakeup>,
    initial_terminal_size: Option<TerminalSize>,
    kitty_graphics: bool,
    theme: Theme,
}

impl WidgetRuntimeContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_session_wakeup(wakeup: SessionWakeup) -> Self {
        Self {
            session_wakeup: Some(wakeup),
            initial_terminal_size: None,
            kitty_graphics: false,
            theme: Theme::default(),
        }
    }

    pub fn with_initial_terminal_size(mut self, size: TerminalSize) -> Self {
        self.initial_terminal_size = Some(size);
        self
    }

    pub fn with_kitty_graphics(mut self, supported: bool) -> Self {
        self.kitty_graphics = supported;
        self
    }

    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    pub fn session_wakeup(&self) -> Option<&SessionWakeup> {
        self.session_wakeup.as_ref()
    }

    pub const fn initial_terminal_size(&self) -> Option<TerminalSize> {
        self.initial_terminal_size
    }

    pub const fn kitty_graphics(&self) -> bool {
        self.kitty_graphics
    }

    pub const fn theme(&self) -> Theme {
        self.theme
    }
}

/// Creates a widget from configuration and the shared runtime context.
pub type WidgetFactory =
    fn(&WidgetInstanceConfig, &WidgetRuntimeContext) -> Result<Box<dyn Widget>, WidgetError>;

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

#[derive(Clone, Default)]
pub struct WidgetRegistry {
    factories: BTreeMap<String, WidgetFactory>,
    context: WidgetRuntimeContext,
}

impl WidgetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn builtins() -> Self {
        Self::build_builtins(WidgetRuntimeContext::new())
    }

    pub fn builtins_with_wakeup(wakeup: SessionWakeup) -> Self {
        Self::builtins_with_context(WidgetRuntimeContext::with_session_wakeup(wakeup))
    }

    pub fn builtins_with_context(context: WidgetRuntimeContext) -> Self {
        Self::build_builtins(context)
    }

    fn build_builtins(context: WidgetRuntimeContext) -> Self {
        let mut registry = Self {
            factories: BTreeMap::new(),
            context,
        };
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
            .register("status", status_widget_factory)
            .expect("built-in widget types are unique");
        registry
            .register("key_value", key_value_widget_factory)
            .expect("built-in widget types are unique");
        registry
            .register("gauge", gauge_widget_factory)
            .expect("built-in widget types are unique");
        registry
            .register("list", list_widget_factory)
            .expect("built-in widget types are unique");
        registry
            .register("log", log_widget_factory)
            .expect("built-in widget types are unique");
        registry
            .register("sparkline", sparkline_widget_factory)
            .expect("built-in widget types are unique");
        registry
            .register("separator", separator_widget_factory)
            .expect("built-in widget types are unique");
        registry
            .register("spacer", spacer_widget_factory)
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

    pub fn context(&self) -> &WidgetRuntimeContext {
        &self.context
    }

    pub fn with_theme(&self, theme: Theme) -> Self {
        let mut registry = self.clone();
        registry.context = registry.context.clone().with_theme(theme);
        registry
    }

    fn instantiate(&self, config: &WidgetInstanceConfig) -> Result<Box<dyn Widget>, WidgetError> {
        AnimationSettings::from_settings(&config.settings)
            .map_err(WidgetError::InvalidConfiguration)?;
        let factory = self
            .factories
            .get(&config.kind)
            .ok_or_else(|| WidgetError::UnknownWidgetType(config.kind.clone()))?;
        factory(config, &self.context)
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
            AnimationSettings::from_settings(&widget_config.settings)
                .map_err(WidgetError::InvalidConfiguration)?;
            let id = WidgetId::new(widget_config.id);
            if instances.contains_key(&id) {
                return Err(WidgetError::DuplicateWidgetId(id));
            }
            let mut widget = if registry.contains(&widget_config.kind) {
                registry.instantiate(widget_config)?
            } else if let Some(plugins) = plugins {
                plugins
                    .instantiate(widget_config, registry.context())
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

    pub fn add_from_config(
        &mut self,
        registry: &WidgetRegistry,
        config: &WidgetInstanceConfig,
    ) -> Result<(), WidgetError> {
        let id = WidgetId::new(config.id);
        if self.instances.contains_key(&id) {
            return Err(WidgetError::DuplicateWidgetId(id));
        }
        let mut widget = registry.instantiate(config)?;
        let kind = widget.kind().to_owned();
        widget
            .initialize()
            .map_err(|reason| WidgetError::InitializationFailed { kind, reason })?;
        let health = widget.health();
        self.instances.insert(id, WidgetEntry { widget, health });
        Ok(())
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

    pub fn content_area(&self, id: WidgetId, area: Rect) -> Rect {
        self.instances
            .get(&id)
            .map_or(area, |entry| entry.widget.content_area(area))
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

    #[cfg(feature = "sixel")]
    pub fn sixel(&self, areas: &BTreeMap<WidgetId, Rect>) -> Vec<SixelSubmission> {
        areas
            .iter()
            .flat_map(|(&id, &area)| {
                self.instances
                    .get(&id)
                    .into_iter()
                    .flat_map(move |entry| entry.widget.sixel(area))
            })
            .collect()
    }

    pub fn render(
        &self,
        areas: &BTreeMap<WidgetId, Rect>,
        focused: Option<WidgetId>,
    ) -> BTreeMap<WidgetId, Scene> {
        self.render_with_cursor(areas, focused, true)
    }

    pub fn render_with_cursor(
        &self,
        areas: &BTreeMap<WidgetId, Rect>,
        focused: Option<WidgetId>,
        cursor_visible: bool,
    ) -> BTreeMap<WidgetId, Scene> {
        self.render_with_animation(areas, focused, cursor_visible, AnimationFrame::complete())
    }

    pub fn render_with_animation(
        &self,
        areas: &BTreeMap<WidgetId, Rect>,
        focused: Option<WidgetId>,
        cursor_visible: bool,
        animation: AnimationFrame,
    ) -> BTreeMap<WidgetId, Scene> {
        self.instances
            .iter()
            .filter_map(|(&id, entry)| {
                let area = *areas.get(&id)?;
                let is_focused = focused == Some(id);
                let mut scene =
                    entry
                        .widget
                        .render_with_animation(area, is_focused, cursor_visible, animation);
                if is_focused && animation.focus_progress < 1000 {
                    scene.apply_motion(animation.focus_progress);
                }
                if animation.transition_progress < 1000 {
                    scene.apply_motion(animation.transition_progress);
                }
                for graphics in entry.widget.graphics(area) {
                    scene.add_placeholder_layer(GraphicsPlaceholderLayer::from_submission(
                        &graphics,
                    ));
                    scene.add_image_layer(graphics);
                }
                #[cfg(feature = "sixel")]
                for sixel in entry.widget.sixel(area) {
                    scene.add_sixel_layer(sixel);
                }
                Some((id, scene))
            })
            .collect()
    }

    pub fn cursor_blink_settings(&self, id: WidgetId) -> Option<CursorBlinkSettings> {
        self.instances
            .get(&id)
            .and_then(|entry| entry.widget.cursor_blink_settings())
    }
}

#[derive(Clone, Copy)]
struct BorderedTextStyle {
    foreground: Color,
    background: Color,
    focused_accent: Color,
    unfocused_accent: Color,
    text: CellStyle,
    appearance: WidgetAppearance,
}

fn render_bordered_text(
    area: Rect,
    title: &str,
    focused: bool,
    text: &str,
    style: BorderedTextStyle,
) -> Scene {
    let accent = if focused {
        style.focused_accent
    } else {
        style.unfocused_accent
    };
    let mut scene = Scene::new(area);
    scene.fill(area, CellStyle::new(style.foreground, style.background));
    style.appearance.render_border(
        &mut scene,
        area,
        title,
        CellStyle::new(accent, style.background),
    );

    let content_area = style.appearance.content_area(area);
    if content_area.width > 0 && content_area.height > 0 {
        let mut content = Scene::new(content_area);
        content.fill(
            content_area,
            CellStyle::new(style.foreground, style.background),
        );
        // Keep the historical one-cell text inset while allowing `padding`
        // to add space around the widget's content area.
        content.text(
            content_area.x.saturating_add(1),
            content_area.y,
            text,
            style.text,
        );
        scene.blit(&content, area);
    }
    scene
}

struct TextWidget {
    title: String,
    label: bool,
    text: String,
    appearance: WidgetAppearance,
    theme: Theme,
}

impl Widget for TextWidget {
    fn kind(&self) -> &str {
        "text"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = self.theme.surface();
        let foreground = self.theme.foreground();
        render_bordered_text(
            area,
            if self.label { &self.title } else { "" },
            focused,
            &self.text,
            BorderedTextStyle {
                foreground,
                background,
                focused_accent: self.theme.focus(),
                unfocused_accent: self.theme.border(),
                text: CellStyle::new(foreground, background),
                appearance: self.appearance,
            },
        )
    }
}

fn text_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    Ok(Box::new(TextWidget {
        title: config.title.clone().unwrap_or_else(|| " text ".to_owned()),
        label: config.label != LabelPolicy::Never,
        text: config.text.clone().unwrap_or_default(),
        appearance: WidgetAppearance::from_settings(&config.settings)?,
        theme: context
            .theme()
            .with_settings(&config.settings)
            .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
    }))
}

struct ClockWidget {
    title: String,
    label: bool,
    format: ClockFormat,
    text: String,
    appearance: WidgetAppearance,
    theme: Theme,
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

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
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
        let background = self.theme.surface();
        let foreground = self.theme.foreground();
        render_bordered_text(
            area,
            if self.label { &self.title } else { "" },
            focused,
            &self.text,
            BorderedTextStyle {
                foreground,
                background,
                focused_accent: self.theme.focus(),
                unfocused_accent: self.theme.success(),
                text: CellStyle::new(foreground, background).bold(),
                appearance: self.appearance,
            },
        )
    }
}

struct SystemWidget {
    title: String,
    label: bool,
    text: String,
    appearance: WidgetAppearance,
    theme: Theme,
}

impl Widget for SystemWidget {
    fn kind(&self) -> &str {
        "system"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = self.theme.surface();
        let foreground = self.theme.foreground();
        render_bordered_text(
            area,
            if self.label { &self.title } else { "" },
            focused,
            &self.text,
            BorderedTextStyle {
                foreground,
                background,
                focused_accent: self.theme.focus(),
                unfocused_accent: self.theme.success(),
                text: CellStyle::new(foreground, background),
                appearance: self.appearance,
            },
        )
    }
}

struct TerminalWidget {
    title: String,
    label: bool,
    session: TerminalSession,
    appearance: WidgetAppearance,
    theme: Theme,
    cursor_blink: CursorBlinkSettings,
}

/// Returns the content rectangle inside a one-cell widget outline.
///
/// Widgets that draw their own border should render content and protocol state
/// into this rectangle so text cannot overwrite the outline.
pub fn widget_content_area(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

impl Widget for TerminalWidget {
    fn kind(&self) -> &str {
        "terminal"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
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
        self.render_with_cursor(area, focused, true)
    }

    fn render_with_cursor(&self, area: Rect, focused: bool, cursor_visible: bool) -> Scene {
        let color = if focused {
            self.theme.focus()
        } else {
            self.theme.border()
        };
        let background = self.theme.background();
        let mut scene = Scene::new(area);
        scene.fill(area, CellStyle::new(self.theme.foreground(), background));
        self.appearance.render_border(
            &mut scene,
            area,
            if self.label { &self.title } else { "" },
            CellStyle::new(color, background),
        );
        let content = self.session.render_with_theme_and_cursor(
            self.appearance.content_area(area),
            focused,
            self.theme,
            cursor_visible,
        );
        scene.blit(&content, area);
        scene
    }

    fn cursor_blink_settings(&self) -> Option<CursorBlinkSettings> {
        (!self.session.is_closed()).then_some(self.cursor_blink)
    }

    fn graphics(&self, area: Rect) -> Vec<GraphicsSubmission> {
        self.session.graphics(self.appearance.content_area(area))
    }

    fn copy_selection(&self, area: Rect) -> Option<String> {
        self.session
            .selected_text(self.appearance.content_area(area))
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

fn terminal_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    let appearance = WidgetAppearance::from_settings(&config.settings)?;
    let cursor_blink = CursorBlinkSettings::from_settings(&config.settings)?;
    let theme = context
        .theme()
        .with_settings(&config.settings)
        .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?;
    let mut session = TerminalSession::spawn_with_session_id_and_wakeup(
        crate::state::SessionId::new(config.id),
        config.command.as_deref(),
        &[],
        context
            .initial_terminal_size()
            .unwrap_or_else(|| TerminalSize::new(80, 24)),
        context.session_wakeup().cloned(),
    )
    .map_err(|error| WidgetError::InitializationFailed {
        kind: "terminal".to_owned(),
        reason: error.to_string(),
    })?;
    session.set_kitty_graphics_support(context.kitty_graphics());
    Ok(Box::new(TerminalWidget {
        title: config
            .title
            .clone()
            .unwrap_or_else(|| " terminal ".to_owned()),
        label: config.label != LabelPolicy::Never,
        session,
        appearance,
        theme,
        cursor_blink,
    }))
}

/// Semantic state used by the `status` widget and shared severity styling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatusLevel {
    Neutral,
    Success,
    Warning,
    Error,
}

impl StatusLevel {
    const fn color(self, theme: Theme) -> Color {
        match self {
            Self::Neutral => theme.muted(),
            Self::Success => theme.success(),
            Self::Warning => theme.warning(),
            Self::Error => theme.error(),
        }
    }
}

fn parse_status_level(value: Option<&String>) -> Result<StatusLevel, WidgetError> {
    match value.map(String::as_str) {
        None => Ok(StatusLevel::Neutral),
        Some("success" | "ok" | "healthy" | "up" | "green" | "passing") => Ok(StatusLevel::Success),
        Some("warning" | "warn" | "degraded" | "yellow") => Ok(StatusLevel::Warning),
        Some("error" | "err" | "failed" | "failure" | "down" | "red" | "critical") => {
            Ok(StatusLevel::Error)
        }
        Some("neutral" | "info" | "idle" | "none") => Ok(StatusLevel::Neutral),
        Some(other) => Err(WidgetError::InvalidConfiguration(format!(
            "status state must be success, warning, error, or neutral, got {other:?}"
        ))),
    }
}

fn parse_gauge_value(settings: &BTreeMap<String, String>) -> Result<u8, WidgetError> {
    let Some(value) = settings.get("value") else {
        return Ok(0);
    };
    let parsed = value.parse::<u8>().map_err(|_| {
        WidgetError::InvalidConfiguration(format!(
            "gauge value must be an integer between 0 and 100, got {value:?}"
        ))
    })?;
    if parsed > 100 {
        return Err(WidgetError::InvalidConfiguration(format!(
            "gauge value must be between 0 and 100, got {parsed}"
        )));
    }
    Ok(parsed)
}

/// Builds a filled, bordered surface and returns its inner content rectangle.
fn bordered_chrome(
    area: Rect,
    title: &str,
    focused: bool,
    theme: Theme,
    appearance: WidgetAppearance,
) -> (Scene, Rect) {
    let accent = if focused {
        theme.focus()
    } else {
        theme.border()
    };
    let background = theme.surface();
    let foreground = theme.foreground();
    let mut scene = Scene::new(area);
    scene.fill(area, CellStyle::new(foreground, background));
    appearance.render_border(&mut scene, area, title, CellStyle::new(accent, background));
    (scene, appearance.content_area(area))
}

/// Renders a `key: value` row clipped to the given area.
fn render_key_value(
    scene: &mut Scene,
    area: Rect,
    key: &str,
    value: &str,
    key_style: CellStyle,
    value_style: CellStyle,
    separator_style: CellStyle,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut column = area.x;
    scene.text(column, area.y, key, key_style);
    column = column.saturating_add(key.chars().count() as u16);
    if column < area.x.saturating_add(area.width) {
        scene.text(column, area.y, ":", separator_style);
        column = column.saturating_add(1);
    }
    if column < area.x.saturating_add(area.width) {
        scene.text(column, area.y, " ", separator_style);
        column = column.saturating_add(1);
    }
    scene.text(column, area.y, value, value_style);
}

/// Renders a bounded progress bar with a textual percentage fallback.
fn render_gauge(
    scene: &mut Scene,
    area: Rect,
    value: u8,
    label: &str,
    fill_style: CellStyle,
    track_style: CellStyle,
    label_style: CellStyle,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let percent = format!("{value}%");
    let label = if label.is_empty() {
        percent
    } else {
        format!("{label} {percent}")
    };
    let label_width = label.chars().count() as u16;
    if area.width <= label_width.saturating_add(1) {
        scene.text(area.x, area.y, &label, label_style);
        return;
    }
    let bar_width = area.width.saturating_sub(label_width).saturating_sub(1);
    let filled = ((u32::from(bar_width) * u32::from(value)) / 100) as u16;
    for column in 0..bar_width {
        let (glyph, style) = if column < filled {
            ('█', fill_style)
        } else {
            ('░', track_style)
        };
        scene.set(area.x.saturating_add(column), area.y, glyph, style);
    }
    scene.text(
        area.x.saturating_add(bar_width).saturating_add(1),
        area.y,
        &label,
        label_style,
    );
}

struct StatusWidget {
    title: String,
    label: bool,
    level: StatusLevel,
    text: String,
    appearance: WidgetAppearance,
    theme: Theme,
}

impl Widget for StatusWidget {
    fn kind(&self) -> &str {
        "status"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = self.theme.surface();
        let status = self.level.color(self.theme);
        render_bordered_text(
            area,
            if self.label { &self.title } else { "" },
            focused,
            &self.text,
            BorderedTextStyle {
                foreground: self.theme.foreground(),
                background,
                focused_accent: self.theme.focus(),
                unfocused_accent: self.theme.border(),
                text: CellStyle::new(status, background),
                appearance: self.appearance,
            },
        )
    }
}

struct KeyValueWidget {
    title: String,
    label: bool,
    key: String,
    value: String,
    appearance: WidgetAppearance,
    theme: Theme,
}

impl Widget for KeyValueWidget {
    fn kind(&self) -> &str {
        "key_value"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = self.theme.surface();
        let foreground = self.theme.foreground();
        let (mut scene, content_area) = bordered_chrome(
            area,
            if self.label { &self.title } else { "" },
            focused,
            self.theme,
            self.appearance,
        );
        if content_area.width > 0 && content_area.height > 0 {
            let mut content = Scene::new(content_area);
            content.fill(content_area, CellStyle::new(foreground, background));
            render_key_value(
                &mut content,
                content_area,
                &self.key,
                &self.value,
                CellStyle::new(self.theme.muted(), background),
                CellStyle::new(self.theme.accent(), background).bold(),
                CellStyle::new(self.theme.muted(), background),
            );
            scene.blit(&content, area);
        }
        scene
    }
}

struct GaugeWidget {
    title: String,
    label: bool,
    value: u8,
    text: String,
    appearance: WidgetAppearance,
    theme: Theme,
}

impl Widget for GaugeWidget {
    fn kind(&self) -> &str {
        "gauge"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = self.theme.surface();
        let foreground = self.theme.foreground();
        let (mut scene, content_area) = bordered_chrome(
            area,
            if self.label { &self.title } else { "" },
            focused,
            self.theme,
            self.appearance,
        );
        if content_area.width > 0 && content_area.height > 0 {
            let mut content = Scene::new(content_area);
            content.fill(content_area, CellStyle::new(foreground, background));
            render_gauge(
                &mut content,
                content_area,
                self.value,
                &self.text,
                CellStyle::new(self.theme.accent(), background),
                CellStyle::new(self.theme.muted(), background),
                CellStyle::new(foreground, background),
            );
            scene.blit(&content, area);
        }
        scene
    }
}

fn status_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    Ok(Box::new(StatusWidget {
        title: config
            .title
            .clone()
            .unwrap_or_else(|| " status ".to_owned()),
        label: config.label != LabelPolicy::Never,
        level: parse_status_level(config.settings.get("state"))?,
        text: config.text.clone().unwrap_or_default(),
        appearance: WidgetAppearance::from_settings(&config.settings)?,
        theme: context
            .theme()
            .with_settings(&config.settings)
            .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
    }))
}

fn key_value_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    let key = config
        .settings
        .get("key")
        .cloned()
        .or_else(|| config.title.clone())
        .unwrap_or_else(|| "value".to_owned());
    Ok(Box::new(KeyValueWidget {
        title: config
            .title
            .clone()
            .unwrap_or_else(|| " key_value ".to_owned()),
        label: config.label != LabelPolicy::Never,
        key,
        value: config.text.clone().unwrap_or_default(),
        appearance: WidgetAppearance::from_settings(&config.settings)?,
        theme: context
            .theme()
            .with_settings(&config.settings)
            .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
    }))
}

fn gauge_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    Ok(Box::new(GaugeWidget {
        title: config.title.clone().unwrap_or_else(|| " gauge ".to_owned()),
        label: config.label != LabelPolicy::Never,
        value: parse_gauge_value(&config.settings)?,
        text: config.text.clone().unwrap_or_default(),
        appearance: WidgetAppearance::from_settings(&config.settings)?,
        theme: context
            .theme()
            .with_settings(&config.settings)
            .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
    }))
}

struct LogLine {
    text: String,
    level: StatusLevel,
}

fn log_level(tag: &str) -> Option<StatusLevel> {
    match tag.to_ascii_lowercase().as_str() {
        "error" | "err" | "critical" => Some(StatusLevel::Error),
        "warning" | "warn" => Some(StatusLevel::Warning),
        "success" | "ok" | "healthy" => Some(StatusLevel::Success),
        "info" | "debug" | "trace" => Some(StatusLevel::Neutral),
        _ => None,
    }
}

fn parse_log_line(line: &str) -> (StatusLevel, String) {
    let trimmed = line.trim_start();
    if trimmed.starts_with('[')
        && let Some(close) = trimmed.find(']')
        && let Some(level) = log_level(&trimmed[1..close])
    {
        (level, trimmed[close + 1..].trim_start().to_owned())
    } else {
        (StatusLevel::Neutral, line.to_owned())
    }
}

fn parse_csv_numbers(raw: &str, max_points: usize) -> Result<Vec<i64>, WidgetError> {
    let mut values = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if values.len() >= max_points {
            return Err(WidgetError::InvalidConfiguration(format!(
                "sparkline values exceed max_points ({max_points})"
            )));
        }
        values.push(part.parse::<i64>().map_err(|_| {
            WidgetError::InvalidConfiguration(format!(
                "sparkline values must be comma-separated integers, got {part:?}"
            ))
        })?);
    }
    Ok(values)
}

const SPARK_GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn normalize_sparkline(values: &[i64]) -> Vec<u8> {
    let Some(&min) = values.iter().min() else {
        return Vec::new();
    };
    let max = values.iter().max().copied().unwrap_or(min);
    if max == min {
        return values.iter().map(|_| 3).collect();
    }
    values
        .iter()
        .map(|value| {
            let numerator = (*value as i128 - min as i128) * 7;
            let denominator = max as i128 - min as i128;
            (numerator / denominator) as u8
        })
        .collect()
}

fn render_sparkline_glyphs(scene: &mut Scene, area: Rect, levels: &[u8], style: CellStyle) {
    for (index, level) in levels.iter().enumerate() {
        let x = area.x.saturating_add(index as u16);
        if x >= area.x.saturating_add(area.width) {
            break;
        }
        scene.set(x, area.y, SPARK_GLYPHS[usize::from(*level)], style);
    }
}

fn render_separator(
    scene: &mut Scene,
    area: Rect,
    label: &str,
    line_style: CellStyle,
    label_style: CellStyle,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let label_width = label.chars().count() as u16;
    if label.is_empty() || label_width.saturating_add(2) > area.width {
        for column in 0..area.width {
            scene.set(area.x.saturating_add(column), area.y, '─', line_style);
        }
        return;
    }
    let dash_span = (area.width - label_width - 2) / 2;
    for column in 0..dash_span {
        scene.set(area.x.saturating_add(column), area.y, '─', line_style);
    }
    scene.text(
        area.x.saturating_add(dash_span),
        area.y,
        &format!(" {label} "),
        label_style,
    );
    let right = area
        .x
        .saturating_add(dash_span)
        .saturating_add(label_width)
        .saturating_add(2);
    for column in right..area.x.saturating_add(area.width) {
        scene.set(column, area.y, '─', line_style);
    }
}

struct ListWidget {
    title: String,
    label: bool,
    rows: Vec<String>,
    appearance: WidgetAppearance,
    theme: Theme,
}

impl Widget for ListWidget {
    fn kind(&self) -> &str {
        "list"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = self.theme.surface();
        let foreground = self.theme.foreground();
        let (mut scene, content_area) = bordered_chrome(
            area,
            if self.label { &self.title } else { "" },
            focused,
            self.theme,
            self.appearance,
        );
        if content_area.width > 0 && content_area.height > 0 {
            let mut content = Scene::new(content_area);
            content.fill(content_area, CellStyle::new(foreground, background));
            let style = CellStyle::new(foreground, background);
            for (row, item) in self
                .rows
                .iter()
                .take(content_area.height as usize)
                .enumerate()
            {
                content.text(
                    content_area.x,
                    content_area.y.saturating_add(row as u16),
                    item,
                    style,
                );
            }
            scene.blit(&content, area);
        }
        scene
    }
}

struct LogWidget {
    title: String,
    label: bool,
    lines: Vec<LogLine>,
    appearance: WidgetAppearance,
    theme: Theme,
}

impl Widget for LogWidget {
    fn kind(&self) -> &str {
        "log"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = self.theme.surface();
        let foreground = self.theme.foreground();
        let (mut scene, content_area) = bordered_chrome(
            area,
            if self.label { &self.title } else { "" },
            focused,
            self.theme,
            self.appearance,
        );
        if content_area.width > 0 && content_area.height > 0 {
            let mut content = Scene::new(content_area);
            content.fill(content_area, CellStyle::new(foreground, background));
            let start = self
                .lines
                .len()
                .saturating_sub(content_area.height as usize);
            for (row, line) in self.lines[start..].iter().enumerate() {
                let style = CellStyle::new(line.level.color(self.theme), background);
                content.text(
                    content_area.x,
                    content_area.y.saturating_add(row as u16),
                    &line.text,
                    style,
                );
            }
            scene.blit(&content, area);
        }
        scene
    }
}

struct SparklineWidget {
    title: String,
    label: bool,
    values: Vec<i64>,
    levels: Vec<u8>,
    appearance: WidgetAppearance,
    theme: Theme,
}

impl Widget for SparklineWidget {
    fn kind(&self) -> &str {
        "sparkline"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = self.theme.surface();
        let foreground = self.theme.foreground();
        let (mut scene, content_area) = bordered_chrome(
            area,
            if self.label { &self.title } else { "" },
            focused,
            self.theme,
            self.appearance,
        );
        if content_area.width > 0 && content_area.height > 0 {
            let mut content = Scene::new(content_area);
            content.fill(content_area, CellStyle::new(foreground, background));
            if self.values.is_empty() {
                // nothing to render
            } else if content_area.width < 2 {
                let min = self.values.iter().min().copied().unwrap_or(0);
                let max = self.values.iter().max().copied().unwrap_or(0);
                content.text(
                    content_area.x,
                    content_area.y,
                    &format!("{min}-{max}"),
                    CellStyle::new(foreground, background),
                );
            } else {
                render_sparkline_glyphs(
                    &mut content,
                    content_area,
                    &self.levels,
                    CellStyle::new(self.theme.accent(), background),
                );
            }
            scene.blit(&content, area);
        }
        scene
    }
}

struct SeparatorWidget {
    title: String,
    label: bool,
    text: String,
    appearance: WidgetAppearance,
    theme: Theme,
}

impl Widget for SeparatorWidget {
    fn kind(&self) -> &str {
        "separator"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = self.theme.surface();
        let foreground = self.theme.foreground();
        let (mut scene, content_area) = bordered_chrome(
            area,
            if self.label { &self.title } else { "" },
            focused,
            self.theme,
            self.appearance,
        );
        if content_area.width > 0 && content_area.height > 0 {
            let mut content = Scene::new(content_area);
            content.fill(content_area, CellStyle::new(foreground, background));
            render_separator(
                &mut content,
                content_area,
                &self.text,
                CellStyle::new(self.theme.muted(), background),
                CellStyle::new(foreground, background).bold(),
            );
            scene.blit(&content, area);
        }
        scene
    }
}

struct SpacerWidget {
    appearance: WidgetAppearance,
    theme: Theme,
}

impl Widget for SpacerWidget {
    fn kind(&self) -> &str {
        "spacer"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn render(&self, area: Rect, _focused: bool) -> Scene {
        let background = self.theme.surface();
        let mut scene = Scene::new(area);
        scene.fill(area, CellStyle::new(self.theme.foreground(), background));
        self.appearance.render_border(
            &mut scene,
            area,
            "",
            CellStyle::new(self.theme.border(), background),
        );
        scene
    }
}

fn list_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    Ok(Box::new(ListWidget {
        title: config.title.clone().unwrap_or_else(|| " list ".to_owned()),
        label: config.label != LabelPolicy::Never,
        rows: config
            .text
            .clone()
            .unwrap_or_default()
            .split('\n')
            .map(str::to_owned)
            .collect(),
        appearance: WidgetAppearance::from_settings(&config.settings)?,
        theme: context
            .theme()
            .with_settings(&config.settings)
            .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
    }))
}

fn log_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    Ok(Box::new(LogWidget {
        title: config.title.clone().unwrap_or_else(|| " log ".to_owned()),
        label: config.label != LabelPolicy::Never,
        lines: config
            .text
            .as_deref()
            .unwrap_or("")
            .lines()
            .map(parse_log_line)
            .map(|(level, text)| LogLine { text, level })
            .collect(),
        appearance: WidgetAppearance::from_settings(&config.settings)?,
        theme: context
            .theme()
            .with_settings(&config.settings)
            .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
    }))
}

fn sparkline_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    let max_points = config
        .settings
        .get("max_points")
        .map(|value| {
            value.parse::<usize>().map_err(|_| {
                WidgetError::InvalidConfiguration(format!(
                    "sparkline max_points must be an integer, got {value:?}"
                ))
            })
        })
        .transpose()?
        .unwrap_or(64);
    let raw = config
        .settings
        .get("values")
        .or(config.text.as_ref())
        .map(String::as_str)
        .unwrap_or("");
    let values = parse_csv_numbers(raw, max_points)?;
    let levels = normalize_sparkline(&values);
    Ok(Box::new(SparklineWidget {
        title: config
            .title
            .clone()
            .unwrap_or_else(|| " sparkline ".to_owned()),
        label: config.label != LabelPolicy::Never,
        values,
        levels,
        appearance: WidgetAppearance::from_settings(&config.settings)?,
        theme: context
            .theme()
            .with_settings(&config.settings)
            .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
    }))
}

fn separator_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    Ok(Box::new(SeparatorWidget {
        title: config
            .title
            .clone()
            .unwrap_or_else(|| " separator ".to_owned()),
        label: config.label != LabelPolicy::Never,
        text: config.text.clone().unwrap_or_default(),
        appearance: WidgetAppearance::from_settings(&config.settings)?,
        theme: context
            .theme()
            .with_settings(&config.settings)
            .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
    }))
}

fn spacer_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    Ok(Box::new(SpacerWidget {
        appearance: WidgetAppearance::from_settings(&config.settings)?,
        theme: context
            .theme()
            .with_settings(&config.settings)
            .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
    }))
}

fn system_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    Ok(Box::new(SystemWidget {
        title: config
            .title
            .clone()
            .unwrap_or_else(|| " system ".to_owned()),
        label: config.label != LabelPolicy::Never,
        text: format!("{os} / {arch}"),
        appearance: WidgetAppearance::from_settings(&config.settings)?,
        theme: context
            .theme()
            .with_settings(&config.settings)
            .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
    }))
}

fn clock_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
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
        label: config.label != LabelPolicy::Never,
        format,
        text: String::new(),
        appearance: WidgetAppearance::from_settings(&config.settings)?,
        theme: context
            .theme()
            .with_settings(&config.settings)
            .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
    };
    let _ = widget.update(SystemTime::now());
    Ok(Box::new(widget))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        thread,
        time::{Duration, Instant},
    };

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
        _context: &WidgetRuntimeContext,
    ) -> Result<Box<dyn Widget>, WidgetError> {
        Ok(Box::new(FailingWidget))
    }

    #[test]
    fn cursor_blink_settings_parse_and_validate() {
        let settings = BTreeMap::from([
            ("cursor_blink".to_owned(), "false".to_owned()),
            ("cursor_blink_interval_ms".to_owned(), "750".to_owned()),
        ]);
        let parsed = CursorBlinkSettings::from_settings(&settings).unwrap();
        assert!(!parsed.enabled());
        assert_eq!(parsed.interval(), Duration::from_millis(750));

        let invalid = BTreeMap::from([("cursor_blink_interval_ms".to_owned(), "0".to_owned())]);
        assert!(CursorBlinkSettings::from_settings(&invalid).is_err());
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
    fn bordered_widget_text_stays_inside_the_outline() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 4
            type = "text"
            text = "01234567890123456789"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let area = Rect::new(0, 0, 10, 4);
        let scene = runtime.render(&BTreeMap::from([(WidgetId::new(4), area)]), None);

        assert_eq!(scene[&WidgetId::new(4)].cell_at(9, 1).unwrap().symbol, '│');
    }

    #[test]
    fn appearance_settings_control_padding_and_border_style() {
        let config = AppConfig::parse(
            r##"
            version = 1
            [[workspace.widgets]]
            id = 4
            type = "text"
            text = "hello"

            [workspace.widgets.settings]
            padding = "2"
            border = "double"
            foreground = "#010203"
            "##,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let id = WidgetId::new(4);
        let area = Rect::new(0, 0, 12, 10);
        assert_eq!(runtime.content_area(id, area), Rect::new(3, 3, 6, 4));
        let scene = runtime.render(&BTreeMap::from([(id, area)]), None);
        assert_eq!(scene[&id].cell_at(0, 0).unwrap().symbol, '╔');
        assert_eq!(scene[&id].cell_at(11, 1).unwrap().symbol, '║');
        assert_eq!(
            scene[&id].cell_at(4, 3).unwrap().style.foreground,
            Color::rgb(1, 2, 3)
        );
    }

    #[test]
    fn never_label_suppresses_title_without_changing_content_geometry() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 4
            type = "text"
            title = " visible title "
            label = "never"
            text = "hello"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let id = WidgetId::new(4);
        let area = Rect::new(0, 0, 16, 5);
        let scene = runtime.render(&BTreeMap::from([(id, area)]), None);

        assert_eq!(runtime.content_area(id, area), Rect::new(1, 1, 14, 3));
        assert_eq!(scene[&id].cell_at(2, 0).unwrap().symbol, '─');
    }

    #[test]
    fn invalid_appearance_settings_are_rejected() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 4
            type = "text"

            [workspace.widgets.settings]
            padding = "wide"
            "#,
        )
        .unwrap();

        assert!(matches!(
            WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config),
            Err(WidgetError::InvalidConfiguration(message))
                if message.contains("padding")
        ));
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
    fn status_widget_renders_semantic_state_colors() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 7
            type = "status"
            text = "all good"
            [workspace.widgets.settings]
            state = "success"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let id = WidgetId::new(7);
        let area = Rect::new(0, 0, 14, 3);
        let scene = runtime.render(&BTreeMap::from([(id, area)]), None);
        assert_eq!(scene[&id].cell_at(2, 1).unwrap().symbol, 'a');
        assert_eq!(
            scene[&id].cell_at(2, 1).unwrap().style.foreground,
            Color::ansi(10)
        );
    }

    #[test]
    fn status_widget_rejects_unknown_state_values() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 7
            type = "status"
            [workspace.widgets.settings]
            state = "bogus"
            "#,
        )
        .unwrap();

        assert!(matches!(
            WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config),
            Err(WidgetError::InvalidConfiguration(message)) if message.contains("state")
        ));
    }

    #[test]
    fn key_value_widget_renders_key_and_value_with_distinct_roles() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 8
            type = "key_value"
            text = "42%"
            [workspace.widgets.settings]
            key = "CPU"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let id = WidgetId::new(8);
        let area = Rect::new(0, 0, 20, 3);
        let scene = runtime.render(&BTreeMap::from([(id, area)]), None);

        assert_eq!(scene[&id].cell_at(1, 1).unwrap().symbol, 'C');
        assert_eq!(
            scene[&id].cell_at(1, 1).unwrap().style.foreground,
            Color::ansi(8)
        );
        assert_eq!(scene[&id].cell_at(6, 1).unwrap().symbol, '4');
        assert_eq!(
            scene[&id].cell_at(6, 1).unwrap().style.foreground,
            Color::ansi(14)
        );
    }

    #[test]
    fn gauge_widget_renders_a_bounded_bar_and_percentage() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 9
            type = "gauge"
            [workspace.widgets.settings]
            value = "50"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let id = WidgetId::new(9);
        let area = Rect::new(0, 0, 16, 3);
        let scene = runtime.render(&BTreeMap::from([(id, area)]), None);

        assert_eq!(scene[&id].cell_at(1, 1).unwrap().symbol, '█');
        assert_eq!(scene[&id].cell_at(5, 1).unwrap().symbol, '█');
        assert_eq!(scene[&id].cell_at(6, 1).unwrap().symbol, '░');
        assert_eq!(scene[&id].cell_at(12, 1).unwrap().symbol, '5');
        assert_eq!(scene[&id].cell_at(14, 1).unwrap().symbol, '%');
    }

    #[test]
    fn gauge_widget_rejects_out_of_range_values() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 9
            type = "gauge"
            [workspace.widgets.settings]
            value = "101"
            "#,
        )
        .unwrap();

        assert!(matches!(
            WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config),
            Err(WidgetError::InvalidConfiguration(message)) if message.contains("between 0 and 100")
        ));
    }

    #[test]
    fn narrow_gauge_falls_back_to_text_without_drawing_outside_the_scene() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 9
            type = "gauge"
            [workspace.widgets.settings]
            value = "73"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let id = WidgetId::new(9);
        let area = Rect::new(0, 0, 6, 3);
        let scene = runtime.render(&BTreeMap::from([(id, area)]), None);

        assert_eq!(scene[&id].cell_at(1, 1).unwrap().symbol, '7');
        assert_eq!(scene[&id].cell_at(5, 1).unwrap().symbol, '│');
    }

    #[test]
    fn list_widget_renders_rows_clipped_to_height() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 10
            type = "list"
            text = "alpha\nbeta\ngamma"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let id = WidgetId::new(10);
        let area = Rect::new(0, 0, 12, 4);
        let scene = runtime.render(&BTreeMap::from([(id, area)]), None);

        assert_eq!(scene[&id].cell_at(1, 1).unwrap().symbol, 'a');
        assert_eq!(scene[&id].cell_at(1, 2).unwrap().symbol, 'b');
        assert_eq!(scene[&id].cell_at(1, 3).unwrap().symbol, '─');
    }

    #[test]
    fn log_widget_styles_lines_by_severity_prefix() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 11
            type = "log"
            text = "[error] boom\n[ok] fine"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let id = WidgetId::new(11);
        let area = Rect::new(0, 0, 12, 4);
        let scene = runtime.render(&BTreeMap::from([(id, area)]), None);

        assert_eq!(scene[&id].cell_at(1, 1).unwrap().symbol, 'b');
        assert_eq!(
            scene[&id].cell_at(1, 1).unwrap().style.foreground,
            Color::ansi(9)
        );
        assert_eq!(scene[&id].cell_at(1, 2).unwrap().symbol, 'f');
        assert_eq!(
            scene[&id].cell_at(1, 2).unwrap().style.foreground,
            Color::ansi(10)
        );
    }

    #[test]
    fn sparkline_widget_renders_normalized_glyphs() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 12
            type = "sparkline"
            [workspace.widgets.settings]
            values = "0,4,8"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let id = WidgetId::new(12);
        let area = Rect::new(0, 0, 8, 3);
        let scene = runtime.render(&BTreeMap::from([(id, area)]), None);

        assert_eq!(scene[&id].cell_at(1, 1).unwrap().symbol, '▁');
        assert_eq!(scene[&id].cell_at(2, 1).unwrap().symbol, '▄');
        assert_eq!(scene[&id].cell_at(3, 1).unwrap().symbol, '█');
    }

    #[test]
    fn sparkline_widget_rejects_invalid_values() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 12
            type = "sparkline"
            [workspace.widgets.settings]
            values = "1,2,x"
            "#,
        )
        .unwrap();

        assert!(matches!(
            WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config),
            Err(WidgetError::InvalidConfiguration(message)) if message.contains("comma-separated integers")
        ));
    }

    #[test]
    fn separator_widget_renders_a_rule_with_a_centered_label() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 13
            type = "separator"
            text = "CPU"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let id = WidgetId::new(13);
        let area = Rect::new(0, 0, 12, 3);
        let scene = runtime.render(&BTreeMap::from([(id, area)]), None);

        assert_eq!(scene[&id].cell_at(1, 1).unwrap().symbol, '─');
        assert_eq!(scene[&id].cell_at(4, 1).unwrap().symbol, 'C');
        assert_eq!(scene[&id].cell_at(8, 1).unwrap().symbol, '─');
    }

    #[test]
    fn spacer_widget_renders_blank_content_within_its_border() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 14
            type = "spacer"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();
        let id = WidgetId::new(14);
        let area = Rect::new(0, 0, 6, 3);
        let scene = runtime.render(&BTreeMap::from([(id, area)]), None);

        assert_eq!(scene[&id].cell_at(0, 0).unwrap().symbol, '╭');
        assert_eq!(scene[&id].cell_at(1, 1).unwrap().symbol, ' ');
    }

    #[test]
    fn custom_widget_follows_the_authoring_guide() {
        struct GreetingWidget {
            text: String,
            theme: Theme,
        }

        impl Widget for GreetingWidget {
            fn kind(&self) -> &str {
                "greeting"
            }

            fn render(&self, area: Rect, _focused: bool) -> Scene {
                let mut scene = Scene::new(area);
                scene.fill(
                    area,
                    CellStyle::new(self.theme.foreground(), self.theme.surface()),
                );
                scene.text(
                    area.x,
                    area.y,
                    &self.text,
                    CellStyle::new(self.theme.accent(), self.theme.surface()),
                );
                scene
            }
        }

        fn greeting_factory(
            config: &WidgetInstanceConfig,
            context: &WidgetRuntimeContext,
        ) -> Result<Box<dyn Widget>, WidgetError> {
            Ok(Box::new(GreetingWidget {
                text: config.text.clone().unwrap_or_default(),
                theme: context
                    .theme()
                    .with_settings(&config.settings)
                    .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?,
            }))
        }

        let mut registry = WidgetRegistry::builtins();
        registry.register("greeting", greeting_factory).unwrap();

        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 15
            type = "greeting"
            text = "hi"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&registry, &config).unwrap();
        let id = WidgetId::new(15);
        let scene = runtime.render(&BTreeMap::from([(id, Rect::new(0, 0, 6, 2))]), None);

        assert_eq!(runtime.widget_kind(id), Some("greeting"));
        assert_eq!(scene[&id].cell_at(0, 0).unwrap().symbol, 'h');
        assert_eq!(
            scene[&id].cell_at(0, 0).unwrap().style.foreground,
            Theme::inherited().accent()
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
    fn terminal_content_is_inset_from_its_widget_border() {
        let mut widget = TerminalWidget {
            title: " shell ".to_owned(),
            label: true,
            session: TerminalSession::spawn_with_args(
                Some("sh"),
                &["-c", "printf x; sleep 5"],
                TerminalSize::new(6, 3),
            )
            .unwrap(),
            appearance: WidgetAppearance::default(),
            theme: Theme::fallback(),
            cursor_blink: CursorBlinkSettings::default(),
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            widget.session.poll_output().unwrap();
            if widget.session.cursor_position() == (1, 0) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let area = Rect::new(2, 3, 8, 5);
        let scene = widget.render(area, false);
        assert_eq!(scene.cell_at(2, 3).unwrap().symbol, '╭');
        assert_eq!(scene.cell_at(2, 4).unwrap().symbol, '│');
        assert_eq!(scene.cell_at(3, 4).unwrap().symbol, 'x');
        assert_eq!(scene.cell_at(9, 4).unwrap().symbol, '│');
        assert_eq!(scene.cell_at(2, 7).unwrap().symbol, '╰');
        widget.session.shutdown().unwrap();
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
