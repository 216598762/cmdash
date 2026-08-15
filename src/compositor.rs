use std::collections::BTreeMap;

use ratatui::layout::Rect;

use crate::{
    scene::{Cell, Scene},
    state::{AppState, SurfaceId},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellChange {
    pub x: u16,
    pub y: u16,
    pub cell: Cell,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellSpan {
    pub x: u16,
    pub y: u16,
    cells: Vec<Cell>,
}

impl CellSpan {
    pub const fn x(&self) -> u16 {
        self.x
    }

    pub const fn y(&self) -> u16 {
        self.y
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameDiff {
    viewport: Rect,
    full_redraw: bool,
    invalidated: Vec<Rect>,
    changes: Vec<CellChange>,
    spans: Vec<CellSpan>,
    graphics: Vec<crate::graphics::GraphicsSubmission>,
    visible_graphics: Vec<crate::graphics::GraphicsSubmission>,
    removed_graphics: Vec<crate::graphics::GraphicsSubmission>,
    #[cfg(feature = "sixel")]
    sixel: Vec<crate::sixel::SixelSubmission>,
}

impl FrameDiff {
    pub const fn viewport(&self) -> Rect {
        self.viewport
    }

    pub const fn full_redraw(&self) -> bool {
        self.full_redraw
    }

    pub fn invalidated_regions(&self) -> &[Rect] {
        &self.invalidated
    }

    pub fn changes(&self) -> &[CellChange] {
        &self.changes
    }

    pub fn spans(&self) -> &[CellSpan] {
        &self.spans
    }

    pub fn graphics(&self) -> &[crate::graphics::GraphicsSubmission] {
        &self.graphics
    }

    pub fn visible_graphics(&self) -> &[crate::graphics::GraphicsSubmission] {
        &self.visible_graphics
    }

    pub fn removed_graphics(&self) -> &[crate::graphics::GraphicsSubmission] {
        &self.removed_graphics
    }

    #[cfg(feature = "sixel")]
    pub fn sixel(&self) -> &[crate::sixel::SixelSubmission] {
        &self.sixel
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.graphics.is_empty() && self.removed_graphics.is_empty() && {
            #[cfg(feature = "sixel")]
            {
                self.sixel.is_empty()
            }
            #[cfg(not(feature = "sixel"))]
            {
                true
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Compositor {
    previous: Option<Scene>,
    pending_invalidations: Vec<Rect>,
}

impl Compositor {
    pub const fn new() -> Self {
        Self {
            previous: None,
            pending_invalidations: Vec::new(),
        }
    }

    pub fn compose(
        &self,
        viewport: Rect,
        state: &AppState,
        base: &Scene,
        surface_scenes: &BTreeMap<SurfaceId, Scene>,
    ) -> Scene {
        let mut composed = Scene::new(viewport);
        composed.blit(base, viewport);

        let mut surfaces: Vec<_> = state
            .workspace()
            .surfaces()
            .values()
            .filter(|surface| surface.visible())
            .map(|surface| (surface.z_index(), surface.id()))
            .collect();
        surfaces.sort_unstable();

        for (_, id) in surfaces {
            let Some(surface) = state.workspace().surfaces().get(&id) else {
                continue;
            };
            let Some(surface_scene) = surface_scenes.get(&id) else {
                continue;
            };
            composed.blit(surface_scene, surface.area());
        }

        let mut overlays: Vec<_> = state
            .workspace()
            .overlays()
            .values()
            .filter(|overlay| overlay.visible())
            .map(|overlay| (overlay.z_index(), overlay.id()))
            .collect();
        overlays.sort_unstable();

        for (_, id) in overlays {
            if let Some(overlay) = state.workspace().overlays().get(&id) {
                overlay.render(&mut composed);
            }
        }

        composed
    }

    pub fn invalidate(&mut self, area: Rect) {
        if area.width > 0 && area.height > 0 {
            self.pending_invalidations.push(area);
        }
    }

    pub fn diff(&mut self, current: &Scene) -> FrameDiff {
        let viewport = current.area();
        let full_redraw = self
            .previous
            .as_ref()
            .is_none_or(|previous| previous.area() != viewport);
        let invalidated: Vec<_> = self
            .pending_invalidations
            .drain(..)
            .filter_map(|area| intersect(area, viewport))
            .collect();
        let previous = self.previous.as_ref();
        let graphics_changed = full_redraw
            || previous.is_none_or(|previous| previous.image_layers() != current.image_layers());
        #[cfg(feature = "sixel")]
        let sixel_changed = full_redraw
            || previous.is_none_or(|previous| previous.sixel_layers() != current.sixel_layers());
        #[cfg(feature = "sixel")]
        let sixel = if sixel_changed {
            current.sixel_layers().to_vec()
        } else {
            Vec::new()
        };
        let graphics = if graphics_changed {
            current.image_layers().to_vec()
        } else {
            Vec::new()
        };
        let current_graphics = current
            .image_layers()
            .iter()
            .map(|image| (image.terminal_image_id(), image))
            .collect::<std::collections::BTreeMap<_, _>>();
        let removed_graphics = previous
            .into_iter()
            .flat_map(|previous| previous.image_layers())
            .filter(|image| {
                current_graphics
                    .get(&image.terminal_image_id())
                    .is_none_or(|current| *current != *image)
            })
            .cloned()
            .collect();
        let mut changes = Vec::new();

        for (index, cell) in current.cells().iter().enumerate() {
            let column = index % viewport.width as usize;
            let row = index / viewport.width as usize;
            let x = viewport.x.saturating_add(column as u16);
            let y = viewport.y.saturating_add(row as u16);
            let forced = invalidated.iter().any(|area| contains(*area, x, y));
            let changed = full_redraw
                || forced
                || previous
                    .and_then(|previous| previous.cell_at(x, y))
                    .copied()
                    != Some(*cell);

            if changed {
                changes.push(CellChange { x, y, cell: *cell });
            }
        }

        self.previous = Some(current.clone());
        let spans = group_changes(&changes);
        FrameDiff {
            viewport,
            full_redraw,
            invalidated,
            changes,
            spans,
            graphics,
            visible_graphics: current.image_layers().to_vec(),
            removed_graphics,
            #[cfg(feature = "sixel")]
            sixel,
        }
    }
}

fn group_changes(changes: &[CellChange]) -> Vec<CellSpan> {
    let mut spans = Vec::new();
    for change in changes {
        let extends_previous = spans.last().is_some_and(|span: &CellSpan| {
            span.y == change.y
                && span.x as u32 + span.cells.len() as u32 == change.x as u32
                && span
                    .cells
                    .last()
                    .is_some_and(|cell| cell.style == change.cell.style)
        });
        if extends_previous {
            if let Some(span) = spans.last_mut() {
                span.cells.push(change.cell);
            }
        } else {
            spans.push(CellSpan {
                x: change.x,
                y: change.y,
                cells: vec![change.cell],
            });
        }
    }
    spans
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && y >= area.y
        && x < area.x.saturating_add(area.width)
        && y < area.y.saturating_add(area.height)
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
        BackendCapabilities, CellStyle, Color, Command, Overlay, OverlayCommand, OverlayId,
        OverlayPrimitive, Surface, SurfaceCommand, SurfaceId,
    };

    fn capabilities() -> BackendCapabilities {
        BackendCapabilities {
            truecolor: true,
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: false,
            kitty_unicode_placeholders: false,
            graphics_source: crate::backend::GraphicsCapabilitySource::Unavailable,
            graphics_confidence: crate::backend::GraphicsCapabilityConfidence::Rejected,
            sixel: false,
        }
    }

    fn style() -> CellStyle {
        CellStyle::new(Color::rgb(255, 255, 255), Color::rgb(0, 0, 0))
    }

    #[test]
    fn surfaces_are_composed_in_z_order_and_clipped_to_their_bounds() {
        let viewport = Rect::new(0, 0, 8, 4);
        let mut state = crate::AppState::new(capabilities());
        let lower = Surface::new(SurfaceId::new(1), Rect::new(1, 1, 4, 2)).with_z_index(0);
        let upper = Surface::new(SurfaceId::new(2), Rect::new(2, 1, 4, 2)).with_z_index(1);
        state
            .dispatch(Command::Surface(SurfaceCommand::Add(lower)))
            .unwrap();
        state
            .dispatch(Command::Surface(SurfaceCommand::Add(upper)))
            .unwrap();

        let mut lower_scene = Scene::new(lower.area());
        lower_scene.text(lower.area().x, lower.area().y, "LLLL", style());
        let mut upper_scene = Scene::new(upper.area());
        upper_scene.text(upper.area().x, upper.area().y, "UUUU", style());
        let scenes = BTreeMap::from([(lower.id(), lower_scene), (upper.id(), upper_scene)]);

        let mut base = Scene::new(viewport);
        base.text(0, 0, "base", style());
        let composed = Compositor::new().compose(viewport, &state, &base, &scenes);

        assert_eq!(composed.cell_at(0, 0).unwrap().symbol, 'b');
        assert_eq!(composed.cell_at(1, 1).unwrap().symbol, 'L');
        assert_eq!(composed.cell_at(2, 1).unwrap().symbol, 'U');
        assert_eq!(composed.cell_at(5, 1).unwrap().symbol, 'U');
        assert_eq!(composed.cell_at(6, 1).unwrap().symbol, ' ');
    }

    #[test]
    fn overlays_are_composed_after_surfaces_and_clipped_to_overlay_bounds() {
        let viewport = Rect::new(0, 0, 8, 4);
        let mut state = crate::AppState::new(capabilities());
        let surface = Surface::new(SurfaceId::new(1), Rect::new(0, 0, 8, 4));
        state
            .dispatch(Command::Surface(SurfaceCommand::Add(surface)))
            .unwrap();
        let overlay = Overlay::new(OverlayId::new(5), Rect::new(2, 1, 3, 2))
            .with_z_index(10)
            .with_primitive(OverlayPrimitive::Text {
                x: 1,
                y: 1,
                text: "OVER".to_owned(),
                style: style(),
            });
        state
            .dispatch(Command::Overlay(OverlayCommand::Show(overlay)))
            .unwrap();

        let mut surface_scene = Scene::new(viewport);
        surface_scene.text(0, 1, "surface", style());
        let scenes = BTreeMap::from([(surface.id(), surface_scene)]);
        let base = Scene::new(viewport);
        let composed = Compositor::new().compose(viewport, &state, &base, &scenes);

        assert_eq!(composed.cell_at(0, 1).unwrap().symbol, 's');
        assert_eq!(composed.cell_at(2, 1).unwrap().symbol, 'V');
        assert_eq!(composed.cell_at(3, 1).unwrap().symbol, 'E');
        assert_eq!(composed.cell_at(4, 1).unwrap().symbol, 'R');
        assert_eq!(composed.cell_at(5, 1).unwrap().symbol, 'c');
        assert_eq!(composed.cell_at(2, 3).unwrap().symbol, ' ');
    }

    #[test]
    fn first_frame_is_full_and_unchanged_frames_are_empty() {
        let viewport = Rect::new(0, 0, 3, 2);
        let mut compositor = Compositor::new();
        let mut scene = Scene::new(viewport);
        scene.set(1, 0, 'x', style());

        let first = compositor.diff(&scene);
        assert!(first.full_redraw());
        assert_eq!(first.changes().len(), 6);

        let unchanged = compositor.diff(&scene);
        assert!(!unchanged.full_redraw());
        assert!(unchanged.is_empty());
    }

    #[test]
    fn contiguous_changes_are_grouped_into_row_spans() {
        let viewport = Rect::new(0, 0, 6, 2);
        let mut compositor = Compositor::new();
        let first_scene = Scene::new(viewport);
        compositor.diff(&first_scene);

        let mut second_scene = first_scene.clone();
        second_scene.set(1, 0, 'a', style());
        second_scene.set(2, 0, 'b', style());
        second_scene.set(4, 0, 'c', style());
        second_scene.set(4, 1, 'd', style());
        let diff = compositor.diff(&second_scene);

        assert_eq!(diff.changes().len(), 4);
        assert_eq!(diff.spans().len(), 3);
        assert_eq!(diff.spans()[0].x(), 1);
        assert_eq!(diff.spans()[0].y(), 0);
        assert_eq!(
            diff.spans()[0].cells(),
            &[
                *second_scene.cell_at(1, 0).unwrap(),
                *second_scene.cell_at(2, 0).unwrap(),
            ]
        );
        assert_eq!(diff.spans()[1].cells().len(), 1);
        assert_eq!(diff.spans()[2].y(), 1);
    }

    #[test]
    fn adjacent_changes_with_different_styles_remain_separate_runs() {
        let viewport = Rect::new(0, 0, 6, 1);
        let mut compositor = Compositor::new();
        let first_scene = Scene::new(viewport);
        compositor.diff(&first_scene);

        let mut second_scene = first_scene.clone();
        let first_style = style();
        let second_style = CellStyle::new(Color::rgb(255, 0, 0), Color::rgb(0, 0, 0));
        second_scene.set(1, 0, 'a', first_style);
        second_scene.set(2, 0, 'b', first_style);
        second_scene.set(3, 0, 'c', second_style);
        second_scene.set(4, 0, 'd', second_style);
        let diff = compositor.diff(&second_scene);

        assert_eq!(diff.spans().len(), 2);
        assert_eq!(diff.spans()[0].cells().len(), 2);
        assert_eq!(diff.spans()[1].cells().len(), 2);
        assert_eq!(diff.spans()[0].cells()[0].style, first_style);
        assert_eq!(diff.spans()[1].cells()[0].style, second_style);
    }

    #[test]
    fn changed_frames_emit_only_changed_cells() {
        let viewport = Rect::new(0, 0, 3, 2);
        let mut compositor = Compositor::new();
        let first_scene = Scene::new(viewport);
        compositor.diff(&first_scene);

        let mut second_scene = first_scene.clone();
        second_scene.set(2, 1, 'x', style());
        let diff = compositor.diff(&second_scene);

        assert_eq!(diff.changes().len(), 1);
        assert_eq!(diff.changes()[0].x, 2);
        assert_eq!(diff.changes()[0].y, 1);
        assert_eq!(diff.changes()[0].cell.symbol, 'x');
    }

    #[test]
    fn explicit_invalidation_forces_cells_even_when_the_scene_is_unchanged() {
        let viewport = Rect::new(0, 0, 4, 2);
        let mut compositor = Compositor::new();
        let scene = Scene::new(viewport);
        compositor.diff(&scene);
        compositor.invalidate(Rect::new(1, 0, 2, 1));

        let diff = compositor.diff(&scene);

        assert_eq!(diff.invalidated_regions(), &[Rect::new(1, 0, 2, 1)]);
        assert_eq!(diff.changes().len(), 2);
        assert!(diff.changes().iter().all(|change| change.y == 0));
    }

    #[test]
    fn image_layer_changes_are_part_of_frame_diffs_and_remove_stale_ids() {
        let mut store = crate::SessionGraphicsStore::new(crate::SessionId::new(1));
        store.apply_kitty_command(b"a=T,f=24,i=1", b"AQID").unwrap();
        store.apply_kitty_command(b"a=p,i=1,x=0,y=0", b"").unwrap();
        let mut first_scene = Scene::new(Rect::new(0, 0, 4, 2));
        first_scene.add_image_layer(store.visible_submissions(first_scene.area())[0].clone());
        let mut compositor = Compositor::new();
        let first = compositor.diff(&first_scene);
        assert_eq!(first.graphics().len(), 1);

        let second_scene = Scene::new(Rect::new(0, 0, 4, 2));
        let second = compositor.diff(&second_scene);
        assert!(!second.is_empty());
        assert_eq!(second.graphics().len(), 0);
        assert_eq!(second.removed_graphics().len(), 1);
        assert_eq!(
            second.removed_graphics()[0].terminal_image_id(),
            first.graphics()[0].terminal_image_id()
        );
    }

    #[test]
    fn resizing_the_viewport_forces_a_full_redraw() {
        let mut compositor = Compositor::new();
        compositor.diff(&Scene::new(Rect::new(0, 0, 4, 2)));

        let diff = compositor.diff(&Scene::new(Rect::new(0, 0, 5, 2)));

        assert!(diff.full_redraw());
        assert_eq!(diff.changes().len(), 10);
    }
}
