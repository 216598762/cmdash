use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Color {
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
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
}

impl Scene {
    pub fn new(area: Rect) -> Self {
        let cell_count = area.width as usize * area.height as usize;
        let style = CellStyle::new(Color::rgb(220, 224, 230), Color::rgb(18, 22, 30));
        Self {
            area,
            cells: vec![Cell::blank(style); cell_count],
        }
    }

    pub const fn area(&self) -> Rect {
        self.area
    }

    pub fn cell_at(&self, x: u16, y: u16) -> Option<&Cell> {
        self.index(x, y).map(|index| &self.cells[index])
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

        for y in y_start..y_end {
            for x in x_start..x_end {
                self.clear_cell_occupancy(x, y);
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
    fn drawing_outside_the_scene_is_ignored() {
        let mut scene = Scene::new(Rect::new(2, 3, 4, 2));
        scene.set(0, 0, 'x', STYLE);
        scene.set(2, 3, 'o', STYLE);

        assert!(scene.cell_at(0, 0).is_none());
        assert_eq!(scene.cell_at(2, 3).unwrap().symbol, 'o');
    }
}
