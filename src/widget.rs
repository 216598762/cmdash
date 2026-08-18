use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use alacritty_terminal::grid::Scroll;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
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
    session_events::SessionEventBus,
    state::{SessionId, WidgetId},
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

/// Optional scrollback affordances drawn in a terminal widget's chrome.
///
/// Both are theme-aware and only appear while scrollback exists; the
/// percentage indicator additionally requires the view to be scrolled away
/// from the live screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrollbackChrome {
    scrollbar: bool,
    indicator: bool,
}

impl Default for ScrollbackChrome {
    fn default() -> Self {
        Self {
            scrollbar: true,
            indicator: true,
        }
    }
}

impl ScrollbackChrome {
    pub fn from_settings(settings: &BTreeMap<String, String>) -> Result<Self, WidgetError> {
        let scrollbar = settings
            .get("scrollbar")
            .map(|value| {
                value.parse::<bool>().map_err(|_| {
                    WidgetError::InvalidConfiguration(format!(
                        "terminal scrollbar must be true or false, got {value:?}"
                    ))
                })
            })
            .transpose()?
            .unwrap_or(true);
        let indicator = settings
            .get("scroll_indicator")
            .map(|value| {
                value.parse::<bool>().map_err(|_| {
                    WidgetError::InvalidConfiguration(format!(
                        "terminal scroll_indicator must be true or false, got {value:?}"
                    ))
                })
            })
            .transpose()?
            .unwrap_or(true);
        Ok(Self {
            scrollbar,
            indicator,
        })
    }

    pub const fn scrollbar(self) -> bool {
        self.scrollbar
    }

    pub const fn indicator(self) -> bool {
        self.indicator
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

    /// Advances any terminal-driven Kitty animation this widget owns to `now`
    /// and returns the delay until its next frame deadline (`None` when the
    /// widget plays no animation).
    fn advance_graphics_animation(&mut self, _now: Instant) -> Option<Duration> {
        None
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

    /// Delivers the host terminal's decoded clipboard content to a widget that
    /// answered an OSC 52 clipboard-read request. Non-terminal widgets ignore
    /// this; a terminal answers its pending read with the supplied text.
    fn handle_clipboard(&mut self, _text: &str) -> Result<WidgetUpdate, String> {
        Ok(WidgetUpdate::Unchanged)
    }

    fn copy_selection(&self, _area: Rect) -> Option<String> {
        None
    }

    /// Returns the URI of the hyperlink under the widget's current selection,
    /// if any, so the copy path can surface a link even when its display text
    /// differs from the target URL (OSC 8).
    fn selected_hyperlink(&self, _area: Rect) -> Option<String> {
        None
    }

    /// The terminal-session id backing this widget, if it is a terminal.
    fn session_id(&self) -> Option<SessionId> {
        None
    }

    /// The title of this widget's terminal session, if it is a terminal.
    fn session_title(&self) -> Option<String> {
        None
    }

    fn handle_mouse(
        &mut self,
        _mouse: MouseEvent,
        _origin: (u16, u16),
    ) -> Result<WidgetUpdate, String> {
        Ok(WidgetUpdate::Unchanged)
    }

    /// Forwards a focus-in/out transition so a terminal can report it to the
    /// child application when `?1004` is enabled.
    fn handle_focus(&mut self, _focused: bool) -> Result<(), String> {
        Ok(())
    }

    /// Notifies a widget that it became visible or hidden, so it can pause
    /// background work (e.g. interval re-runs) while not on screen.
    fn set_visible(&mut self, _visible: bool) {}

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
    session_event_bus: Option<SessionEventBus>,
    initial_terminal_size: Option<TerminalSize>,
    kitty_graphics: bool,
    theme: Theme,
    clipboard: Arc<Mutex<Option<String>>>,
}

impl WidgetRuntimeContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_session_wakeup(wakeup: SessionWakeup) -> Self {
        Self {
            session_wakeup: Some(wakeup),
            session_event_bus: None,
            initial_terminal_size: None,
            kitty_graphics: false,
            theme: Theme::default(),
            clipboard: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_session_event_bus(mut self, bus: SessionEventBus) -> Self {
        self.session_event_bus = Some(bus);
        self
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

    pub fn session_event_bus(&self) -> Option<&SessionEventBus> {
        self.session_event_bus.as_ref()
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

    /// The session-shared clipboard cache (bounded to the last copied text),
    /// shared between the frontend's selection copy and each terminal's OSC 52
    /// store/load path.
    pub fn clipboard(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.clipboard)
    }
}

/// Creates a widget from configuration and the shared runtime context.
pub type WidgetFactory =
    fn(&WidgetInstanceConfig, &WidgetRuntimeContext) -> Result<Box<dyn Widget>, WidgetError>;

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WidgetError {
    #[error("duplicate widget type {0:?}")]
    DuplicateWidgetType(String),
    #[error("unknown widget type {0:?}")]
    UnknownWidgetType(String),
    #[error("duplicate widget id {}", .0.get())]
    DuplicateWidgetId(WidgetId),
    #[error("{0}")]
    InvalidConfiguration(String),
    #[error("failed to initialize {kind:?} widget: {reason}")]
    InitializationFailed { kind: String, reason: String },
    #[error("{0}")]
    Plugin(String),
}

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
            .register("terminal", terminal_widget_factory)
            .expect("built-in widget types are unique");
        registry
            .register("widget", crate::script::script_widget_factory)
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

    pub fn session_id(&self, id: WidgetId) -> Option<SessionId> {
        self.instances.get(&id).and_then(|entry| entry.widget.session_id())
    }

    pub fn session_title(&self, id: WidgetId) -> Option<String> {
        self.instances.get(&id).and_then(|entry| entry.widget.session_title())
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

    /// Advances every widget's terminal-driven Kitty animation to `now` and
    /// returns the earliest delay until a frame deadline (`None` when no widget
    /// plays an animation).
    pub fn advance_graphics_animations(&mut self, now: Instant) -> Option<Duration> {
        let mut next: Option<Duration> = None;
        for entry in self.instances.values_mut() {
            if let Some(delay) = entry.widget.advance_graphics_animation(now) {
                next = Some(match next {
                    Some(earliest) => earliest.min(delay),
                    None => delay,
                });
            }
        }
        next
    }

    /// Marks each widget visible or hidden so widgets can pause background
    /// work (e.g. script interval re-runs) while not on screen.
    pub fn set_visibility(&mut self, visible: &BTreeSet<WidgetId>) {
        for (&id, entry) in &mut self.instances {
            entry.widget.set_visible(visible.contains(&id));
        }
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

    /// Delivers the host terminal's decoded clipboard content to every widget.
    /// Only the terminal that requested the read answers it; the rest ignore
    /// the value, so a single response can service any number of pending reads.
    pub fn broadcast_clipboard(&mut self, text: &str) {
        for entry in self.instances.values_mut() {
            let _ = entry.widget.handle_clipboard(text);
        }
    }

    pub fn copy_selection(&self, id: WidgetId, area: Rect) -> Option<String> {
        self.instances
            .get(&id)
            .and_then(|entry| entry.widget.copy_selection(area))
    }

    pub fn selected_hyperlink(&self, id: WidgetId, area: Rect) -> Option<String> {
        self.instances
            .get(&id)
            .and_then(|entry| entry.widget.selected_hyperlink(area))
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

    pub fn handle_focus(&mut self, id: WidgetId, focused: bool) -> Result<(), String> {
        let entry = self
            .instances
            .get_mut(&id)
            .ok_or_else(|| format!("widget {} is not registered", id.get()))?;
        entry.widget.handle_focus(focused)
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

struct TerminalWidget {
    title: String,
    label: bool,
    session: TerminalSession,
    appearance: WidgetAppearance,
    theme: Theme,
    cursor_blink: CursorBlinkSettings,
    scrollback_chrome: ScrollbackChrome,
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

fn scrollback_scroll(key: KeyEvent) -> Option<Scroll> {
    if !key.modifiers.contains(KeyModifiers::SHIFT) {
        return None;
    }
    match key.code {
        KeyCode::PageUp => Some(Scroll::PageUp),
        KeyCode::PageDown => Some(Scroll::PageDown),
        KeyCode::Up => Some(Scroll::Delta(1)),
        KeyCode::Down => Some(Scroll::Delta(-1)),
        KeyCode::Home => Some(Scroll::Top),
        KeyCode::End => Some(Scroll::Bottom),
        _ => None,
    }
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

    fn advance_graphics_animation(&mut self, now: Instant) -> Option<Duration> {
        self.session.advance_graphics_animations(now)
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
        let content_area = self.appearance.content_area(area);
        let content = self.session.render_with_theme_and_cursor(
            content_area,
            focused,
            self.theme,
            cursor_visible,
        );
        scene.blit(&content, area);
        self.render_scrollback_chrome(&mut scene, area, content_area, focused);
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

    fn selected_hyperlink(&self, _area: Rect) -> Option<String> {
        self.session.selected_hyperlink()
    }

    fn session_id(&self) -> Option<SessionId> {
        Some(self.session.session_id())
    }

    fn session_title(&self) -> Option<String> {
        Some(self.title.clone())
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<WidgetUpdate, String> {
        if let Some(scroll) = scrollback_scroll(key) {
            let changed = self.session.scroll_display(scroll);
            return Ok(if changed {
                WidgetUpdate::Redraw
            } else {
                WidgetUpdate::Unchanged
            });
        }
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

    fn handle_clipboard(&mut self, text: &str) -> Result<WidgetUpdate, String> {
        self.session
            .answer_clipboard_load(text)
            .map(|_| WidgetUpdate::Unchanged)
            .map_err(|error| error.to_string())
    }

    fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        origin: (u16, u16),
    ) -> Result<WidgetUpdate, String> {
        // When the child application has not captured the wheel, a scroll
        // event navigates the terminal's own scrollback instead of reaching
        // the PTY.
        if !self.session.captures_mouse_scroll() {
            let scroll = match mouse.kind {
                crossterm::event::MouseEventKind::ScrollUp => Some(Scroll::Delta(3)),
                crossterm::event::MouseEventKind::ScrollDown => Some(Scroll::Delta(-3)),
                _ => None,
            };
            if let Some(scroll) = scroll {
                let changed = self.session.scroll_display(scroll);
                return Ok(if changed {
                    WidgetUpdate::Redraw
                } else {
                    WidgetUpdate::Unchanged
                });
            }
        }
        let position = (
            mouse.column.saturating_sub(origin.0),
            mouse.row.saturating_sub(origin.1),
        );
        // When the child has captured mouse reporting the terminal forwards
        // the event verbatim and must not run its own selection.
        if !self.session.reports_mouse() {
            match mouse.kind {
                crossterm::event::MouseEventKind::Down(_) => {
                    self.session
                        .begin_selection(position, mouse.modifiers.contains(KeyModifiers::SHIFT));
                }
                crossterm::event::MouseEventKind::Drag(_) => {
                    self.session.update_selection(position);
                }
                _ => {}
            }
        }
        self.session
            .write_mouse(mouse, origin)
            .map(|_| WidgetUpdate::Unchanged)
            .map_err(|error| error.to_string())
    }

    fn handle_focus(&mut self, focused: bool) -> Result<(), String> {
        self.session
            .write_focus(focused)
            .map_err(|error| error.to_string())
    }

    fn shutdown(&mut self) -> Result<(), String> {
        self.session.shutdown().map_err(|error| error.to_string())
    }
}

impl TerminalWidget {
    /// Draws the optional scrollbar and percentage indicator for a terminal
    /// that has scrolled-back history.
    fn render_scrollback_chrome(
        &self,
        scene: &mut Scene,
        area: Rect,
        content_area: Rect,
        focused: bool,
    ) {
        let history = self.session.scrollback_lines();
        if history == 0 || content_area.width == 0 || content_area.height == 0 {
            return;
        }
        let offset = self.session.scrollback_offset();
        let track_height = usize::from(content_area.height);
        let background = self.theme.background();

        if self.scrollback_chrome.scrollbar() {
            let x = content_area
                .x
                .saturating_add(content_area.width.saturating_sub(1));
            let total = history.saturating_add(track_height);
            let thumb_len = track_height
                .saturating_mul(track_height)
                .checked_div(total)
                .unwrap_or(track_height)
                .clamp(1, track_height);
            let max_thumb_top = track_height - thumb_len;
            let thumb_top =
                (history.saturating_sub(offset)).saturating_mul(max_thumb_top) / history;
            let thumb_color = if focused {
                self.theme.focus()
            } else {
                self.theme.border()
            };
            let track_color = self.theme.muted();
            for row in 0..track_height {
                let y = content_area.y.saturating_add(row as u16);
                let (glyph, color) = if row >= thumb_top && row < thumb_top + thumb_len {
                    ('█', thumb_color)
                } else {
                    ('│', track_color)
                };
                scene.set(x, y, glyph, CellStyle::new(color, background));
            }
        }

        if self.scrollback_chrome.indicator() && offset > 0 {
            let percent = offset.saturating_mul(100) / history;
            let text = format!("{}%", percent.min(100));
            let text_len = text.chars().count() as u16;
            let indicator_color = if focused {
                self.theme.focus()
            } else {
                self.theme.muted()
            };
            let style = CellStyle::new(indicator_color, background);
            let has_border = self.appearance.border() != WidgetBorderStyle::None;
            if has_border && area.width > text_len.saturating_add(2) {
                let right = area.x.saturating_add(area.width.saturating_sub(1));
                scene.text(
                    right.saturating_sub(text_len.saturating_add(1)),
                    area.y,
                    &text,
                    style,
                );
            } else if content_area.width >= text_len {
                let start = content_area
                    .x
                    .saturating_add(content_area.width.saturating_sub(text_len));
                scene.text(start, content_area.y, &text, style);
            }
        }
    }
}

fn terminal_widget_factory(
    config: &WidgetInstanceConfig,
    context: &WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    let appearance = WidgetAppearance::from_settings(&config.settings)?;
    let cursor_blink = CursorBlinkSettings::from_settings(&config.settings)?;
    let scrollback_chrome = ScrollbackChrome::from_settings(&config.settings)?;
    let scrollback_limit = config
        .settings
        .get("scrollback")
        .map(|value| {
            value.parse::<usize>().map_err(|_| {
                WidgetError::InvalidConfiguration(format!(
                    "terminal scrollback must be a non-negative integer, got {value:?}"
                ))
            })
        })
        .transpose()?;
    let theme = context
        .theme()
        .with_settings(&config.settings)
        .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?;
    // `TERM` advertises the implemented feature set to the child; the default
    // `xterm-256color` is universally available, while `xterm-kitty` (or
    // `xterm-ghostty`) opts programs into the negotiated protocols.
    let term_env = config
        .settings
        .get("term")
        .map(|value| {
            let value = value.trim();
            if value.is_empty() || value.len() > 64 || value.contains('\0') {
                return Err(WidgetError::InvalidConfiguration(format!(
                    "terminal term must be a non-empty TERM name, got {value:?}"
                )));
            }
            Ok(value.to_owned())
        })
        .transpose()?
        .unwrap_or_else(|| "xterm-256color".to_owned());
    let mut session = TerminalSession::spawn_with_session_id_and_wakeup(
        crate::state::SessionId::new(config.id),
        config.command.as_deref(),
        &[],
        context
            .initial_terminal_size()
            .unwrap_or_else(|| TerminalSize::new(80, 24)),
        context.session_wakeup().cloned(),
        &term_env,
        context.clipboard(),
    )
    .map_err(|error| WidgetError::InitializationFailed {
        kind: "terminal".to_owned(),
        reason: error.to_string(),
    })?;
    session.set_kitty_graphics_support(context.kitty_graphics());
    if let Some(bus) = context.session_event_bus() {
        session.set_session_event_bus(bus.clone());
    }
    if let Some(limit) = scrollback_limit {
        session.set_scrollback_limit(limit);
    }
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
        scrollback_chrome,
    }))
}

/// Semantic state used by the `status` widget and shared severity styling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StatusLevel {
    Neutral,
    Success,
    Warning,
    Error,
}

impl StatusLevel {
    pub(crate) const fn color(self, theme: Theme) -> Color {
        match self {
            Self::Neutral => theme.muted(),
            Self::Success => theme.success(),
            Self::Warning => theme.warning(),
            Self::Error => theme.error(),
        }
    }
}

pub(crate) fn bordered_chrome(
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
fn log_level(tag: &str) -> Option<StatusLevel> {
    match tag.to_ascii_lowercase().as_str() {
        "error" | "err" | "critical" => Some(StatusLevel::Error),
        "warning" | "warn" => Some(StatusLevel::Warning),
        "success" | "ok" | "healthy" => Some(StatusLevel::Success),
        "info" | "debug" | "trace" => Some(StatusLevel::Neutral),
        _ => None,
    }
}

pub(crate) fn parse_log_line(line: &str) -> (StatusLevel, String) {
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

    fn scrollback_widget() -> TerminalWidget {
        TerminalWidget {
            title: " shell ".to_owned(),
            label: true,
            session: TerminalSession::spawn_with_args(
                Some("sh"),
                &["-c", "yes x | head -n 20; sleep 5"],
                TerminalSize::new(20, 4),
            )
            .unwrap(),
            appearance: WidgetAppearance::default(),
            theme: Theme::fallback(),
            cursor_blink: CursorBlinkSettings::default(),
            scrollback_chrome: ScrollbackChrome::default(),
        }
    }

    fn wait_for_scrollback(widget: &mut TerminalWidget) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            widget.session.poll_output().unwrap();
            if widget.session.scrollback_lines() > 0 {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("terminal scrollback did not arrive");
    }

    #[test]
    fn terminal_widget_scrolls_scrollback_with_shifted_navigation_keys() {
        let mut widget = scrollback_widget();
        wait_for_scrollback(&mut widget);
        assert_eq!(widget.session.scrollback_offset(), 0);

        let update = widget
            .handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(update, WidgetUpdate::Redraw);
        assert!(widget.session.scrollback_offset() > 0);

        // A forwarded key returns the viewport to the live screen.
        widget
            .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(widget.session.scrollback_offset(), 0);

        widget.session.shutdown().unwrap();
    }

    #[test]
    fn terminal_widget_scrolls_scrollback_with_the_mouse_wheel() {
        let mut widget = scrollback_widget();
        wait_for_scrollback(&mut widget);
        assert_eq!(widget.session.scrollback_offset(), 0);

        widget
            .handle_mouse(
                MouseEvent {
                    kind: crossterm::event::MouseEventKind::ScrollUp,
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                },
                (0, 0),
            )
            .unwrap();
        assert!(widget.session.scrollback_offset() > 0);

        widget
            .handle_mouse(
                MouseEvent {
                    kind: crossterm::event::MouseEventKind::ScrollDown,
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                },
                (0, 0),
            )
            .unwrap();
        assert_eq!(widget.session.scrollback_offset(), 0);

        widget.session.shutdown().unwrap();
    }

    #[test]
    fn terminal_scrollbar_settings_parse_and_validate() {
        let chrome = ScrollbackChrome::from_settings(&BTreeMap::new()).unwrap();
        assert!(chrome.scrollbar());
        assert!(chrome.indicator());

        let disabled = ScrollbackChrome::from_settings(&BTreeMap::from([
            ("scrollbar".to_owned(), "false".to_owned()),
            ("scroll_indicator".to_owned(), "false".to_owned()),
        ]))
        .unwrap();
        assert!(!disabled.scrollbar());
        assert!(!disabled.indicator());

        let invalid =
            ScrollbackChrome::from_settings(&BTreeMap::from([("scrollbar".to_owned(), "yes".to_owned())]));
        assert!(invalid.is_err());
    }

    #[test]
    fn terminal_rejects_invalid_scrollback_settings() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 4
            type = "terminal"
            command = "sh"

            [workspace.widgets.settings]
            scrollback = "lots"
            "#,
        )
        .unwrap();
        assert_eq!(
            WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).err(),
            Some(WidgetError::InvalidConfiguration(
                "terminal scrollback must be a non-negative integer, got \"lots\"".to_owned()
            ))
        );
    }

    #[test]
    fn terminal_scrollback_chrome_renders_thumb_track_and_indicator() {
        let mut widget = scrollback_widget();
        wait_for_scrollback(&mut widget);
        let area = Rect::new(0, 0, 22, 6);

        // At the live screen the thumb sits at the bottom of the track and no
        // percentage is shown.
        let live = widget.render(area, true);
        assert_eq!(live.cell_at(20, 4).unwrap().symbol, '█');
        assert_eq!(live.cell_at(20, 1).unwrap().symbol, '│');
        let top: String = (0..22).map(|x| live.cell_at(x, 0).unwrap().symbol).collect();
        assert!(!top.contains('%'));

        // Scrolling to the top moves the thumb up and reveals the indicator.
        widget.session.scroll_display(Scroll::Top);
        let scrolled = widget.render(area, true);
        assert_eq!(scrolled.cell_at(20, 1).unwrap().symbol, '█');
        assert_eq!(scrolled.cell_at(20, 4).unwrap().symbol, '│');
        assert_eq!(scrolled.cell_at(19, 0).unwrap().symbol, '%');

        widget.session.shutdown().unwrap();
    }

    #[test]
    fn terminal_scrollback_chrome_can_be_disabled() {
        let mut widget = scrollback_widget();
        wait_for_scrollback(&mut widget);
        widget.session.scroll_display(Scroll::Top);
        widget.scrollback_chrome = ScrollbackChrome {
            scrollbar: false,
            indicator: false,
        };

        let scene = widget.render(Rect::new(0, 0, 22, 6), true);
        // The rightmost content column keeps its (blank) terminal cell and no
        // percentage appears in the border.
        assert_eq!(scene.cell_at(20, 1).unwrap().symbol, ' ');
        let top: String = (0..22).map(|x| scene.cell_at(x, 0).unwrap().symbol).collect();
        assert!(!top.contains('%'));

        widget.session.shutdown().unwrap();
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
            scrollback_chrome: ScrollbackChrome::default(),
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
