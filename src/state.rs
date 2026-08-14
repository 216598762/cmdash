use std::collections::BTreeMap;

use ratatui::layout::Rect;

use crate::{
    backend::BackendCapabilities,
    command::{Command, CommandEffect, FocusCommand, OverlayCommand, SurfaceCommand},
    scene::{CellStyle, Scene},
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
pub struct AppState {
    workspace: WorkspaceState,
    focus: FocusState,
    backend_capabilities: BackendCapabilities,
    quit_requested: bool,
    redraw_requested: bool,
}

impl AppState {
    pub fn new(backend_capabilities: BackendCapabilities) -> Self {
        Self {
            workspace: WorkspaceState::new(WorkspaceId::new(1), "default"),
            focus: FocusState::default(),
            backend_capabilities,
            quit_requested: false,
            redraw_requested: true,
        }
    }

    pub fn workspace(&self) -> &WorkspaceState {
        &self.workspace
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
        command::{Command, FocusCommand, OverlayCommand, SurfaceCommand},
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
