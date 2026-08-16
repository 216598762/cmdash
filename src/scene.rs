use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

use crate::graphics::{GraphicsPlaceholderLayer, GraphicsSubmission};
#[cfg(feature = "sixel")]
use crate::sixel::SixelSubmission;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Color {
    Rgb { red: u8, green: u8, blue: u8 },
    Ansi(u8),
    Reset,
}

impl Color {
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self::Rgb { red, green, blue }
    }

    pub const fn ansi(index: u8) -> Self {
        Self::Ansi(index)
    }

    pub const fn reset() -> Self {
        Self::Reset
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellStyle {
    pub foreground: Color,
    pub background: Color,
    pub bold: bool,
    pub dim: bool,
}

impl CellStyle {
    pub const fn new(foreground: Color, background: Color) -> Self {
        Self {
            foreground,
            background,
            bold: false,
            dim: false,
        }
    }

    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CellWidth {
    Narrow,
    Wide,
    Continuation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    pub symbol: char,
    pub style: CellStyle,
    pub width: CellWidth,
}

/// Backend-neutral hardware-cursor state for a composed frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneCursor {
    x: u16,
    y: u16,
    visible: bool,
}

impl SceneCursor {
    pub const fn new(x: u16, y: u16, visible: bool) -> Self {
        Self { x, y, visible }
    }

    pub const fn x(self) -> u16 {
        self.x
    }

    pub const fn y(self) -> u16 {
        self.y
    }

    pub const fn visible(self) -> bool {
        self.visible
    }
}

impl Cell {
    const fn blank(style: CellStyle) -> Self {
        Self {
            symbol: ' ',
            style,
            width: CellWidth::Narrow,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scene {
    area: Rect,
    cells: Vec<Cell>,
    cursor: Option<SceneCursor>,
    image_layers: Vec<GraphicsSubmission>,
    placeholder_layers: Vec<GraphicsPlaceholderLayer>,
    #[cfg(feature = "sixel")]
    sixel_layers: Vec<SixelSubmission>,
}

impl Scene {
    pub fn new(area: Rect) -> Self {
        let cell_count = area.width as usize * area.height as usize;
        let style = CellStyle::new(Color::reset(), Color::reset());
        Self {
            area,
            cells: vec![Cell::blank(style); cell_count],
            cursor: None,
            image_layers: Vec::new(),
            placeholder_layers: Vec::new(),
            #[cfg(feature = "sixel")]
            sixel_layers: Vec::new(),
        }
    }

    pub const fn area(&self) -> Rect {
        self.area
    }

    pub fn cell_at(&self, x: u16, y: u16) -> Option<&Cell> {
        self.index(x, y).map(|index| &self.cells[index])
    }

    pub const fn cursor(&self) -> Option<SceneCursor> {
        self.cursor
    }

    pub fn set_cursor(&mut self, x: u16, y: u16, visible: bool) {
        if self.index(x, y).is_some() {
            self.cursor = Some(SceneCursor::new(x, y, visible));
        }
    }

    pub fn clear_cursor(&mut self) {
        self.cursor = None;
    }

    pub fn image_layers(&self) -> &[GraphicsSubmission] {
        &self.image_layers
    }

    pub fn placeholder_layers(&self) -> &[GraphicsPlaceholderLayer] {
        &self.placeholder_layers
    }

    /// Applies the bounded transition appearance used by the animation layer.
    ///
    /// Terminal scenes remain ordinary retained cells; animation never emits
    /// terminal escape sequences or alters graphics ownership.
    pub fn apply_motion(&mut self, progress: u16) {
        if progress >= 1000 {
            return;
        }
        let dim = progress < 500;
        for cell in &mut self.cells {
            cell.style.dim = dim;
        }
    }

    pub fn add_image_layer(&mut self, submission: GraphicsSubmission) {
        if let Some(submission) = submission.clipped_to(self.area) {
            self.image_layers.push(submission);
            self.image_layers
                .sort_by_key(|layer| layer.placement().z_index());
        }
    }

    pub fn add_placeholder_layer(&mut self, layer: GraphicsPlaceholderLayer) {
        if let Some(layer) = layer.clipped_to(self.area) {
            self.placeholder_layers.push(layer);
            self.placeholder_layers.sort_by_key(|layer| layer.z_index());
        }
    }

    #[cfg(feature = "sixel")]
    pub fn sixel_layers(&self) -> &[SixelSubmission] {
        &self.sixel_layers
    }

    #[cfg(feature = "sixel")]
    pub fn add_sixel_layer(&mut self, submission: SixelSubmission) {
        if let Some(submission) = submission.clipped_to(self.area) {
            self.sixel_layers.push(submission);
        }
    }

    pub fn set(&mut self, x: u16, y: u16, symbol: char, style: CellStyle) {
        match symbol.width().unwrap_or(0) {
            0 => {}
            1 => self.set_narrow(x, y, symbol, style),
            2 => self.set_wide(x, y, symbol, style),
            _ => unreachable!("unicode-width returns widths in the range 0..=2"),
        }
    }

    pub fn fill(&mut self, rect: Rect, style: CellStyle) {
        let x_start = rect.x.max(self.area.x);
        let y_start = rect.y.max(self.area.y);
        let x_end = (rect.x as u32 + rect.width as u32)
            .min(self.area.x as u32 + self.area.width as u32) as u16;
        let y_end = (rect.y as u32 + rect.height as u32)
            .min(self.area.y as u32 + self.area.height as u32) as u16;

        for y in y_start..y_end {
            for x in x_start..x_end {
                self.set(x, y, ' ', style);
            }
        }
    }

    pub fn text(&mut self, x: u16, y: u16, text: &str, style: CellStyle) {
        let mut column = x as u32;
        let right = self.area.x as u32 + self.area.width as u32;
        for symbol in text.chars() {
            let width = symbol.width().unwrap_or(0) as u32;
            if width == 0 {
                continue;
            }
            if column + width > right {
                break;
            }
            self.set(column as u16, y, symbol, style);
            column += width;
        }
    }

    pub fn blit(&mut self, source: &Scene, clip: Rect) {
        let x_start = source.area.x.max(self.area.x).max(clip.x);
        let y_start = source.area.y.max(self.area.y).max(clip.y);
        let x_end = (source.area.x as u32 + source.area.width as u32)
            .min(self.area.x as u32 + self.area.width as u32)
            .min(clip.x as u32 + clip.width as u32) as u16;
        let y_end = (source.area.y as u32 + source.area.height as u32)
            .min(self.area.y as u32 + self.area.height as u32)
            .min(clip.y as u32 + clip.height as u32) as u16;

        let cursor_in_clip = self
            .cursor
            .is_some_and(|cursor| contains(clip, cursor.x, cursor.y));
        if cursor_in_clip && source.cursor.is_none() {
            self.cursor = None;
        }
        if let Some(cursor) = source.cursor
            && contains(clip, cursor.x, cursor.y)
            && self.index(cursor.x, cursor.y).is_some()
        {
            self.cursor = Some(cursor);
        }

        for y in y_start..y_end {
            for x in x_start..x_end {
                self.clear_cell_occupancy(x, y);
            }
        }
        self.occlude_images(clip);
        self.occlude_placeholder_layers(clip);
        for image in &source.image_layers {
            if let Some(image) = image
                .clipped_to(clip)
                .and_then(|image| image.clipped_to(self.area))
            {
                self.image_layers.push(image);
            }
        }
        self.image_layers
            .sort_by_key(|layer| layer.placement().z_index());
        for placeholder in &source.placeholder_layers {
            if let Some(placeholder) = placeholder
                .clipped_to(clip)
                .and_then(|placeholder| placeholder.clipped_to(self.area))
            {
                self.placeholder_layers.push(placeholder);
            }
        }
        self.placeholder_layers.sort_by_key(|layer| layer.z_index());
        #[cfg(feature = "sixel")]
        for image in &source.sixel_layers {
            if let Some(image) = image
                .clipped_to(clip)
                .and_then(|image| image.clipped_to(self.area))
            {
                self.sixel_layers.push(image);
            }
        }
        for y in y_start..y_end {
            for x in x_start..x_end {
                if let Some(cell) = source.cell_at(x, y).copied() {
                    if cell.width == CellWidth::Continuation
                        && (x == source.area.x
                            || source
                                .cell_at(x.saturating_sub(1), y)
                                .is_none_or(|lead| lead.width != CellWidth::Wide))
                    {
                        continue;
                    }
                    if let Some(index) = self.index(x, y) {
                        self.cells[index] = cell;
                    }
                }
            }
        }
    }

    /// Removes image fragments covered by an opaque composed surface or
    /// overlay. Image layers are split around the occluder so visible portions
    /// remain renderable and no backend escape sequence is emitted underneath
    /// a higher z-order surface.
    pub fn occlude_images(&mut self, occluder: Rect) {
        let mut visible = Vec::new();
        for image in std::mem::take(&mut self.image_layers) {
            let image_area = image.placement().area();
            let Some(intersection) = intersect(image_area, occluder) else {
                visible.push(image);
                continue;
            };
            let candidates = [
                Rect::new(
                    image_area.x,
                    image_area.y,
                    image_area.width,
                    intersection.y.saturating_sub(image_area.y),
                ),
                Rect::new(
                    image_area.x,
                    intersection.y.saturating_add(intersection.height),
                    image_area.width,
                    image_area
                        .y
                        .saturating_add(image_area.height)
                        .saturating_sub(intersection.y.saturating_add(intersection.height)),
                ),
                Rect::new(
                    image_area.x,
                    intersection.y,
                    intersection.x.saturating_sub(image_area.x),
                    intersection.height,
                ),
                Rect::new(
                    intersection.x.saturating_add(intersection.width),
                    intersection.y,
                    image_area
                        .x
                        .saturating_add(image_area.width)
                        .saturating_sub(intersection.x.saturating_add(intersection.width)),
                    intersection.height,
                ),
            ];
            for candidate in candidates {
                if candidate.width > 0
                    && candidate.height > 0
                    && let Some(fragment) = image.clipped_to(candidate)
                {
                    visible.push(fragment);
                }
            }
        }
        visible.sort_by_key(|layer| layer.placement().z_index());
        self.image_layers = visible;
    }

    /// Applies the same opaque-surface occlusion policy to backend-neutral
    /// placeholder regions. Keeping this in `Scene` prevents adapters from
    /// reintroducing cells underneath overlays after composition.
    pub fn occlude_placeholder_layers(&mut self, occluder: Rect) {
        let mut visible = Vec::new();
        for layer in std::mem::take(&mut self.placeholder_layers) {
            let area = layer.area();
            let Some(intersection) = intersect(area, occluder) else {
                visible.push(layer);
                continue;
            };
            let candidates = [
                Rect::new(
                    area.x,
                    area.y,
                    area.width,
                    intersection.y.saturating_sub(area.y),
                ),
                Rect::new(
                    area.x,
                    intersection.y.saturating_add(intersection.height),
                    area.width,
                    area.y
                        .saturating_add(area.height)
                        .saturating_sub(intersection.y.saturating_add(intersection.height)),
                ),
                Rect::new(
                    area.x,
                    intersection.y,
                    intersection.x.saturating_sub(area.x),
                    intersection.height,
                ),
                Rect::new(
                    intersection.x.saturating_add(intersection.width),
                    intersection.y,
                    area.x
                        .saturating_add(area.width)
                        .saturating_sub(intersection.x.saturating_add(intersection.width)),
                    intersection.height,
                ),
            ];
            for candidate in candidates {
                if candidate.width > 0
                    && candidate.height > 0
                    && let Some(fragment) = layer.clipped_to(candidate)
                {
                    visible.push(fragment);
                }
            }
        }
        visible.sort_by_key(|layer| layer.z_index());
        self.placeholder_layers = visible;
    }

    pub fn border(&mut self, rect: Rect, title: &str, style: CellStyle) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }

        let right = rect.x.saturating_add(rect.width.saturating_sub(1));
        let bottom = rect.y.saturating_add(rect.height.saturating_sub(1));
        for x in rect.x..=right {
            self.set(x, rect.y, '─', style);
            self.set(x, bottom, '─', style);
        }
        for y in rect.y..=bottom {
            self.set(rect.x, y, '│', style);
            self.set(right, y, '│', style);
        }

        if rect.width >= 2 && rect.height >= 2 {
            self.set(rect.x, rect.y, '╭', style);
            self.set(right, rect.y, '╮', style);
            self.set(rect.x, bottom, '╰', style);
            self.set(right, bottom, '╯', style);
        }

        if rect.width > 4 {
            let title_x = rect.x.saturating_add(2);
            self.text(title_x, rect.y, title, style.bold());
        }
    }

    pub(crate) fn cells(&self) -> &[Cell] {
        &self.cells
    }

    fn set_narrow(&mut self, x: u16, y: u16, symbol: char, style: CellStyle) {
        self.clear_cell_occupancy(x, y);
        if let Some(index) = self.index(x, y) {
            self.cells[index] = Cell {
                symbol,
                style,
                width: CellWidth::Narrow,
            };
        }
    }

    fn set_wide(&mut self, x: u16, y: u16, symbol: char, style: CellStyle) {
        let Some(next_x) = x.checked_add(1) else {
            return;
        };
        if self.index(x, y).is_none() || self.index(next_x, y).is_none() {
            return;
        }

        self.clear_cell_occupancy(x, y);
        self.clear_cell_occupancy(next_x, y);
        let lead = self.index(x, y).expect("wide lead was validated");
        let continuation = self
            .index(next_x, y)
            .expect("wide continuation was validated");
        self.cells[lead] = Cell {
            symbol,
            style,
            width: CellWidth::Wide,
        };
        self.cells[continuation] = Cell {
            symbol: ' ',
            style,
            width: CellWidth::Continuation,
        };
    }

    fn clear_cell_occupancy(&mut self, x: u16, y: u16) {
        let Some(index) = self.index(x, y) else {
            return;
        };
        let cell = self.cells[index];
        match cell.width {
            CellWidth::Wide => {
                if let Some(next) = x.checked_add(1).and_then(|next| self.index(next, y)) {
                    self.cells[next] = Cell::blank(cell.style);
                }
            }
            CellWidth::Continuation => {
                if let Some(previous) = x
                    .checked_sub(1)
                    .and_then(|previous| self.index(previous, y))
                {
                    self.cells[previous] = Cell::blank(cell.style);
                }
            }
            CellWidth::Narrow => {}
        }
    }

    fn index(&self, x: u16, y: u16) -> Option<usize> {
        if x < self.area.x
            || y < self.area.y
            || x >= self.area.x.saturating_add(self.area.width)
            || y >= self.area.y.saturating_add(self.area.height)
        {
            return None;
        }

        let column = (x - self.area.x) as usize;
        let row = (y - self.area.y) as usize;
        Some(row * self.area.width as usize + column)
    }
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && y >= area.y
        && x < area.x.saturating_add(area.width)
        && y < area.y.saturating_add(area.height)
}

fn intersect(first: Rect, second: Rect) -> Option<Rect> {
    let left = first.x.max(second.x);
    let top = first.y.max(second.y);
    let right = first
        .x
        .saturating_add(first.width)
        .min(second.x.saturating_add(second.width));
    let bottom = first
        .y
        .saturating_add(first.height)
        .min(second.y.saturating_add(second.height));
    (left < right && top < bottom).then(|| Rect::new(left, top, right - left, bottom - top))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLE: CellStyle = CellStyle::new(Color::rgb(1, 2, 3), Color::rgb(4, 5, 6));

    #[test]
    fn text_is_clipped_to_the_scene() {
        let mut scene = Scene::new(Rect::new(0, 0, 4, 1));
        scene.text(2, 0, "abcd", STYLE);

        assert_eq!(scene.cell_at(0, 0).unwrap().symbol, ' ');
        assert_eq!(scene.cell_at(1, 0).unwrap().symbol, ' ');
        assert_eq!(scene.cell_at(2, 0).unwrap().symbol, 'a');
        assert_eq!(scene.cell_at(3, 0).unwrap().symbol, 'b');
    }

    #[test]
    fn wide_text_tracks_its_continuation_cell() {
        let mut scene = Scene::new(Rect::new(0, 0, 5, 1));
        scene.text(0, 0, "界a", STYLE);

        assert_eq!(scene.cell_at(0, 0).unwrap().symbol, '界');
        assert_eq!(scene.cell_at(0, 0).unwrap().width, CellWidth::Wide);
        assert_eq!(scene.cell_at(1, 0).unwrap().width, CellWidth::Continuation);
        assert_eq!(scene.cell_at(2, 0).unwrap().symbol, 'a');
        assert_eq!(scene.cell_at(2, 0).unwrap().width, CellWidth::Narrow);
    }

    #[test]
    fn wide_text_is_not_started_when_it_would_be_clipped() {
        let mut scene = Scene::new(Rect::new(0, 0, 2, 1));
        scene.text(1, 0, "界", STYLE);

        assert_eq!(scene.cell_at(0, 0).unwrap().symbol, ' ');
        assert_eq!(scene.cell_at(1, 0).unwrap().symbol, ' ');
    }

    #[test]
    fn blit_respects_the_destination_clip() {
        let style = CellStyle::new(Color::rgb(1, 2, 3), Color::rgb(4, 5, 6));
        let mut source = Scene::new(Rect::new(0, 0, 4, 2));
        source.text(0, 0, "abcd", style);
        let mut destination = Scene::new(Rect::new(0, 0, 4, 2));
        destination.blit(&source, Rect::new(1, 0, 2, 1));

        assert_eq!(destination.cell_at(0, 0).unwrap().symbol, ' ');
        assert_eq!(destination.cell_at(1, 0).unwrap().symbol, 'b');
        assert_eq!(destination.cell_at(2, 0).unwrap().symbol, 'c');
        assert_eq!(destination.cell_at(3, 0).unwrap().symbol, ' ');
    }

    #[test]
    fn placeholder_layers_are_clipped_and_occluded_with_images() {
        let resource = crate::GraphicsResourceId::new(crate::SessionId::new(9), 3);
        let mut scene = Scene::new(Rect::new(0, 0, 6, 3));
        scene.add_placeholder_layer(GraphicsPlaceholderLayer::new(
            resource,
            Rect::new(1, 1, 4, 1),
            -2,
        ));
        assert_eq!(scene.placeholder_layers().len(), 1);
        assert_eq!(scene.placeholder_layers()[0].area(), Rect::new(1, 1, 4, 1));

        let occluder = Scene::new(Rect::new(2, 1, 2, 1));
        scene.blit(&occluder, occluder.area());
        assert_eq!(scene.placeholder_layers().len(), 2);
        assert!(
            scene
                .placeholder_layers()
                .iter()
                .all(|layer| intersect(layer.area(), occluder.area()).is_none())
        );
    }

    #[test]
    fn image_layers_are_clipped_and_blitted_with_the_scene() {
        let mut source = Scene::new(Rect::new(0, 0, 8, 4));
        let mut store = crate::SessionGraphicsStore::new(crate::SessionId::new(1));
        store.apply_kitty_command(b"a=T,f=24,i=1", b"AQID").unwrap();
        store
            .apply_kitty_command_with_context(b"a=p,i=1,c=5,r=2", b"", (2, 1), (10, 20))
            .unwrap();
        source.add_image_layer(store.visible_submissions(source.area())[1].clone());
        let mut destination = Scene::new(Rect::new(0, 0, 8, 4));
        destination.blit(&source, Rect::new(3, 1, 2, 1));

        assert_eq!(destination.image_layers().len(), 1);
        assert_eq!(
            destination.image_layers()[0].placement().area(),
            Rect::new(3, 1, 2, 1)
        );
    }

    #[test]
    fn opaque_blits_occlude_only_the_covered_image_region() {
        let mut store = crate::SessionGraphicsStore::new(crate::SessionId::new(2));
        store.apply_kitty_command(b"a=T,f=24,i=2", b"AQID").unwrap();
        store
            .apply_kitty_command_with_context(b"a=p,i=2,c=6,r=2", b"", (0, 1), (10, 20))
            .unwrap();
        let image = store.visible_submissions(Rect::new(0, 0, 8, 4))[1].clone();
        let mut destination = Scene::new(Rect::new(0, 0, 8, 4));
        destination.add_image_layer(image);
        let overlay = Scene::new(Rect::new(2, 1, 2, 2));
        destination.blit(&overlay, overlay.area());

        assert_eq!(destination.image_layers().len(), 2);
        assert!(
            destination.image_layers().iter().all(|layer| intersect(
                layer.placement().area(),
                Rect::new(2, 1, 2, 2)
            )
            .is_none())
        );
    }

    #[test]
    fn cursor_coordinates_survive_widget_to_viewport_blitting() {
        let mut source = Scene::new(Rect::new(5, 4, 4, 2));
        source.set_cursor(6, 5, true);
        let mut destination = Scene::new(Rect::new(0, 0, 12, 8));

        destination.blit(&source, destination.area());

        assert_eq!(destination.cursor(), Some(SceneCursor::new(6, 5, true)));
    }

    #[test]
    fn an_opaque_overlay_clears_a_cursor_inside_its_bounds() {
        let mut scene = Scene::new(Rect::new(0, 0, 8, 4));
        scene.set_cursor(3, 2, true);
        let overlay = Scene::new(Rect::new(2, 1, 3, 2));

        scene.blit(&overlay, overlay.area());

        assert_eq!(scene.cursor(), None);
    }

    #[cfg(feature = "sixel")]
    #[test]
    fn sixel_layers_are_retained_and_conservatively_clipped() {
        let image = crate::sixel::SixelSubmission::new(
            1,
            1,
            crate::sixel::SixelImage {
                width: 2,
                height: 1,
                rgb: &[255, 255, 255, 0, 0, 0],
            },
        )
        .unwrap();
        let mut scene = Scene::new(Rect::new(0, 0, 8, 4));
        scene.add_sixel_layer(image);
        assert_eq!(scene.sixel_layers().len(), 1);

        let mut clipped = Scene::new(Rect::new(0, 0, 8, 4));
        clipped.blit(&scene, Rect::new(1, 1, 2, 1));
        assert_eq!(clipped.sixel_layers().len(), 1);
        assert!(clipped.sixel_layers()[0].encoded().starts_with(b"\x1bPq"));
    }

    #[test]
    fn drawing_outside_the_scene_is_ignored() {
        let mut scene = Scene::new(Rect::new(2, 3, 4, 2));
        scene.set(0, 0, 'x', STYLE);
        scene.set(2, 3, 'o', STYLE);

        assert!(scene.cell_at(0, 0).is_none());
        assert_eq!(scene.cell_at(2, 3).unwrap().symbol, 'o');
    }
}
