use ratatui::layout::Rect;

use crate::state::{Overlay, OverlayId, Surface, SurfaceId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Quit,
    RequestRedraw,
    ReloadConfig,
    CopySelection,
    ToggleHelp,
    TogglePalette,
    Focus(FocusCommand),
    Tab(TabCommand),
    Surface(SurfaceCommand),
    Overlay(OverlayCommand),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabCommand {
    Next,
    Previous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusCommand {
    Surface(SurfaceId),
    Overlay(OverlayId),
    Next,
    Previous,
    Clear,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceCommand {
    Add(Surface),
    Remove(SurfaceId),
    SetArea { id: SurfaceId, area: Rect },
    SetVisible { id: SurfaceId, visible: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OverlayCommand {
    Show(Overlay),
    Hide(OverlayId),
    Remove(OverlayId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandEffect {
    Noop,
    Redraw,
    Quit,
}
