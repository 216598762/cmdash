use std::{collections::BTreeMap, fmt, time::SystemTime};

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;

use crate::{
    backend::BackendCapabilities,
    command::{Command, CommandEffect, FocusCommand, OverlayCommand, SurfaceCommand, TabCommand},
    config::{AppConfig, ConfigError},
    layout::{LayoutError, LayoutTree},
    scene::{CellStyle, Scene},
    widget::{WidgetError, WidgetRegistry, WidgetRuntime, WidgetUpdateReport},
};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

id_type!(WorkspaceId);
id_type!(SurfaceId);
id_type!(WidgetId);
id_type!(SessionId);
id_type!(OverlayId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Surface {
    id: SurfaceId,
    widget: Option<WidgetId>,
    area: Rect,
    visible: bool,
    z_index: i16,
}

impl Surface {
    pub const fn new(id: SurfaceId, area: Rect) -> Self {
        Self {
            id,
            widget: None,
            area,
            visible: true,
            z_index: 0,
        }
    }

    pub const fn id(self) -> SurfaceId {
        self.id
    }

    pub const fn widget(self) -> Option<WidgetId> {
        self.widget
    }

    pub const fn area(self) -> Rect {
        self.area
    }

    pub const fn visible(self) -> bool {
        self.visible
    }

    pub const fn z_index(self) -> i16 {
        self.z_index
    }

    pub const fn with_widget(mut self, widget: WidgetId) -> Self {
        self.widget = Some(widget);
        self
    }

    pub const fn with_z_index(mut self, z_index: i16) -> Self {
        self.z_index = z_index;
        self
    }

    pub const fn with_visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    pub const fn set_area(mut self, area: Rect) -> Self {
        self.area = area;
        self
    }

    pub fn clip_to(self, viewport: Rect) -> Option<Rect> {
        intersect(self.area, viewport)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusTarget {
    Surface(SurfaceId),
    Overlay(OverlayId),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FocusState {
    target: Option<FocusTarget>,
}

impl FocusState {
    pub const fn target(self) -> Option<FocusTarget> {
        self.target
    }

    pub fn is_focused(self, target: FocusTarget) -> bool {
        matches!(self.target, Some(current) if current == target)
    }

    fn set(&mut self, target: FocusTarget) {
        self.target = Some(target);
    }

    fn clear(&mut self) {
        self.target = None;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OverlayPrimitive {
    Fill {
        area: Rect,
        style: CellStyle,
    },
    Border {
        area: Rect,
        title: String,
        style: CellStyle,
    },
    Text {
        x: u16,
        y: u16,
        text: String,
        style: CellStyle,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Overlay {
    id: OverlayId,
    area: Rect,
    z_index: i16,
    visible: bool,
    dismissible: bool,
    primitives: Vec<OverlayPrimitive>,
}

impl Overlay {
    pub fn new(id: OverlayId, area: Rect) -> Self {
        Self {
            id,
            area,
            z_index: 0,
            visible: true,
            dismissible: true,
            primitives: Vec::new(),
        }
    }

    pub const fn id(&self) -> OverlayId {
        self.id
    }

    pub const fn area(&self) -> Rect {
        self.area
    }

    pub const fn z_index(&self) -> i16 {
        self.z_index
    }

    pub const fn visible(&self) -> bool {
        self.visible
    }

    pub const fn dismissible(&self) -> bool {
        self.dismissible
    }

    pub fn primitives(&self) -> &[OverlayPrimitive] {
        &self.primitives
    }

    pub const fn with_z_index(mut self, z_index: i16) -> Self {
        self.z_index = z_index;
        self
    }

    pub const fn with_dismissible(mut self, dismissible: bool) -> Self {
        self.dismissible = dismissible;
        self
    }

    pub const fn with_visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    pub fn with_primitive(mut self, primitive: OverlayPrimitive) -> Self {
        self.primitives.push(primitive);
        self
    }

    pub fn render(&self, scene: &mut Scene) {
        if !self.visible {
            return;
        }

        let mut overlay_scene = Scene::new(self.area);
        for primitive in &self.primitives {
            match primitive {
                OverlayPrimitive::Fill { area, style } => overlay_scene.fill(*area, *style),
                OverlayPrimitive::Border { area, title, style } => {
                    overlay_scene.border(*area, title, *style)
                }
                OverlayPrimitive::Text { x, y, text, style } => {
                    overlay_scene.text(*x, *y, text, *style)
                }
            }
        }
        scene.blit(&overlay_scene, scene.area());
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceState {
    id: WorkspaceId,
    name: String,
    surfaces: BTreeMap<SurfaceId, Surface>,
    overlays: BTreeMap<OverlayId, Overlay>,
}

impl WorkspaceState {
    pub fn new(id: WorkspaceId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            surfaces: BTreeMap::new(),
            overlays: BTreeMap::new(),
        }
    }

    pub const fn id(&self) -> WorkspaceId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn surfaces(&self) -> &BTreeMap<SurfaceId, Surface> {
        &self.surfaces
    }

    pub fn overlays(&self) -> &BTreeMap<OverlayId, Overlay> {
        &self.overlays
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandError {
    DuplicateSurface(SurfaceId),
    DuplicateOverlay(OverlayId),
    SurfaceNotFound(SurfaceId),
    OverlayNotFound(OverlayId),
    SurfaceNotVisible(SurfaceId),
    OverlayNotVisible(OverlayId),
    InvalidSurfaceArea(SurfaceId),
    InvalidOverlayArea(OverlayId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppStateConfigError {
    InvalidConfig(ConfigError),
    Widget(WidgetError),
    Layout(LayoutError),
}

impl fmt::Display for AppStateConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => write!(formatter, "invalid config: {error}"),
            Self::Widget(error) => write!(formatter, "widget setup failed: {error}"),
            Self::Layout(error) => write!(formatter, "layout setup failed: {error}"),
        }
    }
}

impl std::error::Error for AppStateConfigError {}

pub struct AppState {
    workspace: WorkspaceState,
    focus: FocusState,
    backend_capabilities: BackendCapabilities,
    widget_runtime: WidgetRuntime,
    layout: LayoutTree,
    quit_requested: bool,
    redraw_requested: bool,
}

impl AppState {
    pub fn new(backend_capabilities: BackendCapabilities) -> Self {
        Self {
            workspace: WorkspaceState::new(WorkspaceId::new(1), "default"),
            focus: FocusState::default(),
            backend_capabilities,
            widget_runtime: WidgetRuntime::empty(),
            layout: LayoutTree::from_config(None, [], []).expect("empty layout is valid"),
            quit_requested: false,
            redraw_requested: true,
        }
    }

    pub fn from_config(
        backend_capabilities: BackendCapabilities,
        registry: &WidgetRegistry,
        config: &AppConfig,
    ) -> Result<Self, AppStateConfigError> {
        Self::from_config_with_plugins(backend_capabilities, registry, None, config)
    }

    pub fn from_config_with_plugins(
        backend_capabilities: BackendCapabilities,
        registry: &WidgetRegistry,
        plugins: Option<&crate::plugin::PluginRegistry>,
        config: &AppConfig,
    ) -> Result<Self, AppStateConfigError> {
        config
            .validate()
            .map_err(AppStateConfigError::InvalidConfig)?;
        let widget_runtime = WidgetRuntime::from_config_with_plugins(registry, plugins, config)
            .map_err(AppStateConfigError::Widget)?;
        let widget_ids = config
            .workspace
            .widgets
            .iter()
            .map(|widget| WidgetId::new(widget.id));
        let overlay_ids = config
            .workspace
            .overlays
            .iter()
            .map(|overlay| OverlayId::new(overlay.id));
        let layout =
            LayoutTree::from_config(config.workspace.layout.as_ref(), widget_ids, overlay_ids)
                .map_err(AppStateConfigError::Layout)?;
        let mut state = Self {
            workspace: WorkspaceState::new(WorkspaceId::new(1), config.workspace.name.clone()),
            focus: FocusState::default(),
            backend_capabilities,
            widget_runtime,
            layout,
            quit_requested: false,
            redraw_requested: true,
        };

        for widget in &config.workspace.widgets {
            let widget_id = WidgetId::new(widget.id);
            let surface = Surface::new(SurfaceId::new(widget.id), Rect::new(0, 0, 1, 1))
                .with_widget(widget_id)
                .with_visible(state.layout.visible_widget_ids().contains(&widget_id));
            state.workspace.surfaces.insert(surface.id(), surface);
        }
        for overlay_config in &config.workspace.overlays {
            let overlay_id = OverlayId::new(overlay_config.id);
            let overlay_area = Rect::new(
                overlay_config.x,
                overlay_config.y,
                overlay_config.width,
                overlay_config.height,
            );
            let mut overlay = Overlay::new(overlay_id, overlay_area)
                .with_z_index(overlay_config.z_index)
                .with_visible(
                    overlay_config.visible
                        && (config.workspace.layout.is_none()
                            || state.layout.visible_overlay_ids().contains(&overlay_id)),
                );
            if let Some(title) = &overlay_config.title {
                overlay = overlay.with_primitive(OverlayPrimitive::Border {
                    area: overlay_area,
                    title: title.clone(),
                    style: CellStyle::new(
                        crate::scene::Color::rgb(216, 180, 254),
                        crate::scene::Color::rgb(38, 28, 58),
                    ),
                });
            }
            if let Some(text) = &overlay_config.text {
                overlay = overlay.with_primitive(OverlayPrimitive::Text {
                    x: overlay_config.x.saturating_add(1),
                    y: overlay_config.y.saturating_add(1),
                    text: text.clone(),
                    style: CellStyle::new(
                        crate::scene::Color::rgb(245, 232, 255),
                        crate::scene::Color::rgb(38, 28, 58),
                    ),
                });
            }
            state.workspace.overlays.insert(overlay_id, overlay);
        }
        Ok(state)
    }

    pub fn workspace(&self) -> &WorkspaceState {
        &self.workspace
    }

    pub fn widget_runtime(&self) -> &WidgetRuntime {
        &self.widget_runtime
    }

    pub fn layout(&self) -> &LayoutTree {
        &self.layout
    }

    pub fn update_widgets(&mut self, now: SystemTime) -> WidgetUpdateReport {
        let report = self.widget_runtime.update(now);
        if report.requests_redraw() || !report.failed().is_empty() {
            self.redraw_requested = true;
        }
        report
    }

    pub fn handle_focused_key(&mut self, key: KeyEvent) -> Result<bool, String> {
        let Some(FocusTarget::Surface(surface_id)) = self.focus.target() else {
            return Ok(false);
        };
        let Some(widget_id) = self
            .workspace
            .surfaces
            .get(&surface_id)
            .and_then(|surface| surface.widget())
        else {
            return Ok(false);
        };
        if !self.widget_runtime.handles_input(widget_id) {
            return Ok(false);
        }
        let update = self.widget_runtime.handle_key(widget_id, key)?;
        if update == crate::widget::WidgetUpdate::Redraw {
            self.redraw_requested = true;
        }
        Ok(true)
    }

    pub fn handle_focused_paste(&mut self, text: &str) -> Result<bool, String> {
        let Some(FocusTarget::Surface(surface_id)) = self.focus.target() else {
            return Ok(false);
        };
        let Some(widget_id) = self
            .workspace
            .surfaces
            .get(&surface_id)
            .and_then(|surface| surface.widget())
        else {
            return Ok(false);
        };
        if !self.widget_runtime.handles_input(widget_id) {
            return Ok(false);
        }
        self.widget_runtime.handle_paste(widget_id, text)?;
        Ok(true)
    }

    pub fn handle_focused_mouse(&mut self, mouse: MouseEvent) -> Result<bool, String> {
        let Some(FocusTarget::Surface(surface_id)) = self.focus.target() else {
            return Ok(false);
        };
        let Some(surface) = self.workspace.surfaces.get(&surface_id).copied() else {
            return Ok(false);
        };
        let Some(widget_id) = surface.widget() else {
            return Ok(false);
        };
        let area = surface.area();
        if mouse.column < area.x
            || mouse.row < area.y
            || mouse.column >= area.x.saturating_add(area.width)
            || mouse.row >= area.y.saturating_add(area.height)
            || !self.widget_runtime.handles_input(widget_id)
        {
            return Ok(false);
        }
        self.widget_runtime
            .handle_mouse(widget_id, mouse, (area.x, area.y))?;
        Ok(true)
    }

    pub fn resize_widget_surfaces(
        &mut self,
        areas: &BTreeMap<SurfaceId, Rect>,
    ) -> Result<(), String> {
        let resize_requests: Vec<_> = areas
            .iter()
            .filter_map(|(&surface_id, &area)| {
                let surface = self.workspace.surfaces.get(&surface_id)?;
                let widget_id = surface.widget()?;
                (self.widget_runtime.widget_kind(widget_id) == Some("terminal")
                    && area.width >= 2
                    && area.height > 0)
                    .then_some((
                        widget_id,
                        crate::session::TerminalSize::new(area.width, area.height),
                    ))
            })
            .collect();
        for (widget_id, size) in resize_requests {
            if self.widget_runtime.resize(widget_id, size)? == crate::widget::WidgetUpdate::Redraw {
                self.redraw_requested = true;
            }
        }
        Ok(())
    }

    pub fn shutdown_widgets(&mut self) {
        self.widget_runtime.shutdown();
    }

    pub fn widget_surface_scenes(&self) -> BTreeMap<SurfaceId, Scene> {
        let mut areas = BTreeMap::new();
        let mut surface_ids = BTreeMap::new();
        for (&surface_id, surface) in &self.workspace.surfaces {
            if let Some(widget_id) = surface.widget()
                && surface.visible()
            {
                areas.insert(widget_id, surface.area());
                surface_ids.insert(widget_id, surface_id);
            }
        }

        let focused_widget = match self.focus.target() {
            Some(FocusTarget::Surface(surface_id)) => self
                .workspace
                .surfaces
                .get(&surface_id)
                .and_then(|surface| surface.widget()),
            Some(FocusTarget::Overlay(_)) | None => None,
        };
        self.widget_runtime
            .render(&areas, focused_widget)
            .into_iter()
            .filter_map(|(widget_id, scene)| {
                surface_ids
                    .get(&widget_id)
                    .copied()
                    .map(|surface_id| (surface_id, scene))
            })
            .collect()
    }

    pub const fn focus(&self) -> FocusState {
        self.focus
    }

    pub const fn backend_capabilities(&self) -> BackendCapabilities {
        self.backend_capabilities
    }

    pub const fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    pub fn take_redraw_request(&mut self) -> bool {
        let requested = self.redraw_requested;
        self.redraw_requested = false;
        requested
    }

    pub fn dispatch(&mut self, command: Command) -> Result<CommandEffect, CommandError> {
        let effect = match command {
            Command::Quit => {
                self.quit_requested = true;
                CommandEffect::Quit
            }
            Command::RequestRedraw => CommandEffect::Redraw,
            Command::Focus(command) => {
                self.apply_focus(command)?;
                CommandEffect::Redraw
            }
            Command::Tab(command) => {
                self.apply_tab(command);
                CommandEffect::Redraw
            }
            Command::Surface(command) => {
                self.apply_surface(command)?;
                CommandEffect::Redraw
            }
            Command::Overlay(command) => {
                self.apply_overlay(command)?;
                CommandEffect::Redraw
            }
        };

        if matches!(effect, CommandEffect::Redraw | CommandEffect::Quit) {
            self.redraw_requested = true;
        }
        Ok(effect)
    }

    fn apply_tab(&mut self, command: TabCommand) {
        let forward = matches!(command, TabCommand::Next);
        if self.layout.switch_tabs(forward) {
            let visible = self.layout.visible_widget_ids();
            for surface in self.workspace.surfaces.values_mut() {
                if let Some(widget_id) = surface.widget() {
                    *surface = surface.with_visible(visible.contains(&widget_id));
                }
            }
            if let Some(FocusTarget::Surface(surface_id)) = self.focus.target()
                && self
                    .workspace
                    .surfaces
                    .get(&surface_id)
                    .and_then(|surface| surface.widget())
                    .is_some_and(|widget_id| !visible.contains(&widget_id))
            {
                self.focus.clear();
            }
        }
    }

    fn apply_focus(&mut self, command: FocusCommand) -> Result<(), CommandError> {
        match command {
            FocusCommand::Surface(id) => {
                let surface = self
                    .workspace
                    .surfaces
                    .get(&id)
                    .ok_or(CommandError::SurfaceNotFound(id))?;
                if !surface.visible() {
                    return Err(CommandError::SurfaceNotVisible(id));
                }
                self.focus.set(FocusTarget::Surface(id));
            }
            FocusCommand::Overlay(id) => {
                let overlay = self
                    .workspace
                    .overlays
                    .get(&id)
                    .ok_or(CommandError::OverlayNotFound(id))?;
                if !overlay.visible() {
                    return Err(CommandError::OverlayNotVisible(id));
                }
                self.focus.set(FocusTarget::Overlay(id));
            }
            FocusCommand::Next => self.navigate_focus(true),
            FocusCommand::Previous => self.navigate_focus(false),
            FocusCommand::Clear => self.focus.clear(),
        }
        Ok(())
    }

    fn navigate_focus(&mut self, forward: bool) {
        let mut surfaces: Vec<_> = self
            .workspace
            .surfaces
            .values()
            .filter(|surface| surface.visible())
            .collect();
        surfaces.sort_by_key(|surface| (surface.z_index(), surface.id()));

        if surfaces.is_empty() {
            self.focus.clear();
            return;
        }

        let current_index = match self.focus.target() {
            Some(FocusTarget::Surface(id)) => {
                surfaces.iter().position(|surface| surface.id() == id)
            }
            Some(FocusTarget::Overlay(_)) | None => None,
        };
        let next_index = match (current_index, forward) {
            (Some(index), true) => (index + 1) % surfaces.len(),
            (Some(index), false) => (index + surfaces.len() - 1) % surfaces.len(),
            (None, true) => 0,
            (None, false) => surfaces.len() - 1,
        };
        self.focus
            .set(FocusTarget::Surface(surfaces[next_index].id()));
    }

    fn apply_surface(&mut self, command: SurfaceCommand) -> Result<(), CommandError> {
        match command {
            SurfaceCommand::Add(surface) => {
                let id = surface.id();
                if surface.area().width == 0 || surface.area().height == 0 {
                    return Err(CommandError::InvalidSurfaceArea(id));
                }
                if self.workspace.surfaces.insert(id, surface).is_some() {
                    return Err(CommandError::DuplicateSurface(id));
                }
            }
            SurfaceCommand::Remove(id) => {
                if self.workspace.surfaces.remove(&id).is_none() {
                    return Err(CommandError::SurfaceNotFound(id));
                }
                if self.focus.is_focused(FocusTarget::Surface(id)) {
                    self.focus.clear();
                }
            }
            SurfaceCommand::SetArea { id, area } => {
                if area.width == 0 || area.height == 0 {
                    return Err(CommandError::InvalidSurfaceArea(id));
                }
                let surface = self
                    .workspace
                    .surfaces
                    .get_mut(&id)
                    .ok_or(CommandError::SurfaceNotFound(id))?;
                *surface = surface.set_area(area);
            }
            SurfaceCommand::SetVisible { id, visible } => {
                let surface = self
                    .workspace
                    .surfaces
                    .get_mut(&id)
                    .ok_or(CommandError::SurfaceNotFound(id))?;
                *surface = surface.with_visible(visible);
                if !visible && self.focus.is_focused(FocusTarget::Surface(id)) {
                    self.focus.clear();
                }
            }
        }
        Ok(())
    }

    fn apply_overlay(&mut self, command: OverlayCommand) -> Result<(), CommandError> {
        match command {
            OverlayCommand::Show(overlay) => {
                let id = overlay.id();
                if overlay.area().width == 0 || overlay.area().height == 0 {
                    return Err(CommandError::InvalidOverlayArea(id));
                }
                if self.workspace.overlays.insert(id, overlay).is_some() {
                    return Err(CommandError::DuplicateOverlay(id));
                }
            }
            OverlayCommand::Hide(id) => {
                let overlay = self
                    .workspace
                    .overlays
                    .get_mut(&id)
                    .ok_or(CommandError::OverlayNotFound(id))?;
                *overlay = overlay.clone().with_visible(false);
                if self.focus.is_focused(FocusTarget::Overlay(id)) {
                    self.focus.clear();
                }
            }
            OverlayCommand::Remove(id) => {
                if self.workspace.overlays.remove(&id).is_none() {
                    return Err(CommandError::OverlayNotFound(id));
                }
                if self.focus.is_focused(FocusTarget::Overlay(id)) {
                    self.focus.clear();
                }
            }
        }
        Ok(())
    }
}

fn intersect(first: Rect, second: Rect) -> Option<Rect> {
    let left = (first.x as u32).max(second.x as u32);
    let top = (first.y as u32).max(second.y as u32);
    let right = (first.x as u32 + first.width as u32).min(second.x as u32 + second.width as u32);
    let bottom = (first.y as u32 + first.height as u32).min(second.y as u32 + second.height as u32);

    if left >= right || top >= bottom {
        return None;
    }

    Some(Rect::new(
        left as u16,
        top as u16,
        (right - left) as u16,
        (bottom - top) as u16,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Color,
        command::{Command, FocusCommand, OverlayCommand, SurfaceCommand, TabCommand},
    };

    fn capabilities() -> BackendCapabilities {
        BackendCapabilities {
            truecolor: true,
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: false,
        }
    }

    #[test]
    fn surface_clipping_returns_only_the_visible_intersection() {
        let surface = Surface::new(SurfaceId::new(1), Rect::new(2, 3, 8, 6));

        assert_eq!(
            surface.clip_to(Rect::new(5, 5, 4, 4)),
            Some(Rect::new(5, 5, 4, 4))
        );
        assert_eq!(surface.clip_to(Rect::new(20, 20, 2, 2)), None);
    }

    #[test]
    fn focus_commands_reject_missing_or_hidden_targets() {
        let mut state = AppState::new(capabilities());
        let surface = Surface::new(SurfaceId::new(7), Rect::new(0, 0, 10, 4));

        assert_eq!(
            state.dispatch(Command::Focus(FocusCommand::Surface(surface.id()))),
            Err(CommandError::SurfaceNotFound(surface.id()))
        );
        state
            .dispatch(Command::Surface(SurfaceCommand::Add(
                surface.with_visible(false),
            )))
            .unwrap();
        assert_eq!(
            state.dispatch(Command::Focus(FocusCommand::Surface(surface.id()))),
            Err(CommandError::SurfaceNotVisible(surface.id()))
        );
    }

    #[test]
    fn removing_a_focused_surface_clears_focus() {
        let mut state = AppState::new(capabilities());
        let surface = Surface::new(SurfaceId::new(7), Rect::new(0, 0, 10, 4));
        state
            .dispatch(Command::Surface(SurfaceCommand::Add(surface)))
            .unwrap();
        state
            .dispatch(Command::Focus(FocusCommand::Surface(surface.id())))
            .unwrap();
        assert_eq!(
            state.focus().target(),
            Some(FocusTarget::Surface(surface.id()))
        );

        state
            .dispatch(Command::Surface(SurfaceCommand::Remove(surface.id())))
            .unwrap();
        assert_eq!(state.focus().target(), None);
    }

    #[test]
    fn overlay_primitives_render_into_a_scene_and_hidden_overlays_do_not() {
        let style = CellStyle::new(Color::rgb(255, 255, 255), Color::rgb(0, 0, 0));
        let overlay = Overlay::new(OverlayId::new(1), Rect::new(1, 1, 6, 3)).with_primitive(
            OverlayPrimitive::Text {
                x: 2,
                y: 2,
                text: "ok".to_owned(),
                style,
            },
        );
        let mut scene = Scene::new(Rect::new(0, 0, 10, 5));
        overlay.render(&mut scene);
        assert_eq!(scene.cell_at(2, 2).unwrap().symbol, 'o');

        let hidden = overlay.with_visible(false);
        hidden.render(&mut scene);
        assert_eq!(scene.cell_at(2, 2).unwrap().symbol, 'o');
    }

    #[test]
    fn tab_navigation_cycles_visible_surfaces_in_z_order() {
        let mut state = AppState::new(capabilities());
        let first = Surface::new(SurfaceId::new(1), Rect::new(0, 0, 10, 4)).with_z_index(10);
        let second = Surface::new(SurfaceId::new(2), Rect::new(10, 0, 10, 4)).with_z_index(0);
        state
            .dispatch(Command::Surface(SurfaceCommand::Add(first)))
            .unwrap();
        state
            .dispatch(Command::Surface(SurfaceCommand::Add(second)))
            .unwrap();

        state.dispatch(Command::Focus(FocusCommand::Next)).unwrap();
        assert_eq!(
            state.focus().target(),
            Some(FocusTarget::Surface(second.id()))
        );
        state.dispatch(Command::Focus(FocusCommand::Next)).unwrap();
        assert_eq!(
            state.focus().target(),
            Some(FocusTarget::Surface(first.id()))
        );
        state
            .dispatch(Command::Focus(FocusCommand::Previous))
            .unwrap();
        assert_eq!(
            state.focus().target(),
            Some(FocusTarget::Surface(second.id()))
        );
    }

    #[test]
    fn config_creates_widget_surfaces_and_renders_through_app_state() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [workspace]
            name = "configured"
            [[workspace.widgets]]
            id = 7
            type = "text"
            title = " greeting "
            text = "hello"
            "#,
        )
        .unwrap();
        let mut state =
            AppState::from_config(capabilities(), &WidgetRegistry::builtins(), &config).unwrap();
        let surface_id = SurfaceId::new(7);
        state
            .dispatch(Command::Surface(SurfaceCommand::SetArea {
                id: surface_id,
                area: Rect::new(0, 0, 16, 4),
            }))
            .unwrap();

        let scenes = state.widget_surface_scenes();
        assert_eq!(state.workspace().name(), "configured");
        assert_eq!(
            state.workspace().surfaces()[&surface_id].widget(),
            Some(WidgetId::new(7))
        );
        assert_eq!(scenes[&surface_id].cell_at(2, 1).unwrap().symbol, 'h');
    }

    #[test]
    fn configured_layout_controls_visible_tabs_and_overlays() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 1
            type = "text"
            text = "first"
            [[workspace.widgets]]
            id = 2
            type = "text"
            text = "second"
            [[workspace.overlays]]
            id = 4
            text = "notice"
            [workspace.layout]
            type = "stack"
            children = [
              { type = "tabs", active = 1, children = [
                { type = "leaf", widget = 1 },
                { type = "leaf", widget = 2 }
              ] },
              { type = "overlay", overlay = 4 }
            ]
            "#,
        )
        .unwrap();
        let mut state =
            AppState::from_config(capabilities(), &WidgetRegistry::builtins(), &config).unwrap();

        assert!(!state.workspace().surfaces()[&SurfaceId::new(1)].visible());
        assert!(state.workspace().surfaces()[&SurfaceId::new(2)].visible());
        assert!(state.workspace().overlays()[&OverlayId::new(4)].visible());
        assert_eq!(state.layout().visible_widget_ids(), [WidgetId::new(2)]);
        assert_eq!(state.layout().visible_overlay_ids(), [OverlayId::new(4)]);
        state.dispatch(Command::Tab(TabCommand::Next)).unwrap();
        assert_eq!(state.layout().visible_widget_ids(), [WidgetId::new(1)]);
        assert!(!state.workspace().surfaces()[&SurfaceId::new(2)].visible());
    }

    #[test]
    fn overlay_focus_is_cleared_when_the_overlay_is_hidden() {
        let mut state = AppState::new(capabilities());
        let overlay = Overlay::new(OverlayId::new(3), Rect::new(1, 1, 8, 4));
        state
            .dispatch(Command::Overlay(OverlayCommand::Show(overlay.clone())))
            .unwrap();
        state
            .dispatch(Command::Focus(FocusCommand::Overlay(overlay.id())))
            .unwrap();
        state
            .dispatch(Command::Overlay(OverlayCommand::Hide(overlay.id())))
            .unwrap();

        assert_eq!(state.focus().target(), None);
    }
}
