use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

use crate::graphics::{GraphicsPlaceholderLayer, GraphicsSubmission};
#[cfg(feature = "sixel")]
use crate::sixel::SixelSubmission;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum Underline {
    #[default]
    None,
    Plain,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// The interned payload behind a `CellStyle` handle. Each distinct foreground/
/// background/attribute combination is stored once in the process-wide style
/// table, and `CellStyle` carries only the compact table index.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct StyleData {
    pub foreground: Color,
    pub background: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: Underline,
    pub underline_color: Option<Color>,
    pub strikeout: bool,
    pub reverse: bool,
    pub hidden: bool,
}

impl StyleData {
    const fn new(foreground: Color, background: Color) -> Self {
        Self {
            foreground,
            background,
            bold: false,
            dim: false,
            italic: false,
            underline: Underline::None,
            underline_color: None,
            strikeout: false,
            reverse: false,
            hidden: false,
        }
    }
}

/// Process-wide style interner. `CellStyle` resolves through this table so a
/// cell stores a 4-byte handle instead of the expanded 9-field struct. The
/// table only grows (styles are never evicted), but it is bounded in practice
/// by the color/attribute space a session actually renders.
#[derive(Default)]
struct StyleTable {
    styles: Vec<StyleData>,
    ids: HashMap<StyleData, u32>,
}

impl StyleTable {
    fn intern(&mut self, data: StyleData) -> u32 {
        if let Some(&id) = self.ids.get(&data) {
            return id;
        }
        let id = self.styles.len() as u32;
        self.styles.push(data);
        self.ids.insert(data, id);
        id
    }
}

fn style_table() -> &'static Mutex<StyleTable> {
    static TABLE: OnceLock<Mutex<StyleTable>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(StyleTable::default()))
}

/// A compact, interned terminal cell style. The public constructor and builder
/// API is unchanged, but the value is now a handle into the process-wide style
/// table, so `Cell` shrinks by the size of the expanded style struct and styles
/// compare as integers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CellStyle(u32);

impl CellStyle {
    pub fn new(foreground: Color, background: Color) -> Self {
        Self::from_data(StyleData::new(foreground, background))
    }

    pub fn bold(self) -> Self {
        self.map(|data| data.bold = true)
    }

    pub fn dim(self) -> Self {
        self.map(|data| data.dim = true)
    }

    pub fn italic(self) -> Self {
        self.map(|data| data.italic = true)
    }

    pub fn underline(self) -> Self {
        self.map(|data| data.underline = Underline::Plain)
    }

    pub fn underline_style(self, underline: Underline) -> Self {
        self.map(|data| data.underline = underline)
    }

    pub fn underline_color(self, color: Color) -> Self {
        self.map(|data| data.underline_color = Some(color))
    }

    pub fn strikeout(self) -> Self {
        self.map(|data| data.strikeout = true)
    }

    pub fn reverse(self) -> Self {
        self.map(|data| data.reverse = true)
    }

    pub fn hidden(self) -> Self {
        self.map(|data| data.hidden = true)
    }

    /// Returns a copy with the dim attribute set to `dim` (used by the
    /// animation layer's motion transition, which mutates every cell).
    pub fn with_dim(self, dim: bool) -> Self {
        self.map(|data| data.dim = dim)
    }

    /// Resolves this handle to its interned style data.
    pub(crate) fn resolve(self) -> StyleData {
        style_table()
            .lock()
            .expect("style table mutex poisoned")
            .styles[self.0 as usize]
    }

    fn from_data(data: StyleData) -> Self {
        Self(
            style_table()
                .lock()
                .expect("style table mutex poisoned")
                .intern(data),
        )
    }

    fn map(self, f: impl FnOnce(&mut StyleData)) -> Self {
        let mut table = style_table().lock().expect("style table mutex poisoned");
        let mut data = table.styles[self.0 as usize];
        f(&mut data);
        Self(table.intern(data))
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
    /// One reference per cell, parallel to `cells`: `IMAGE_REF_NONE` (0) when
    /// the cell displays no image, otherwise a handle into this scene's
    /// `image_layers` or `placeholder_layers` (see `annotate_image_cells`).
    /// Kept separate from `Cell` so the text-span diff, span grouping, and
    /// cell equality are untouched — the reference is a property of the
    /// *grid*, exactly like Kitty anchoring every image to the cell grid and
    /// Termux storing a bitmap reference inside each covered cell's style.
    image_refs: Vec<u32>,
    /// One logical-line tag per row, parallel to the grid's rows (index
    /// `y - area.y`). A tag is the absolute logical line (oldest-history-
    /// relative) the row displays — the same space as a placement's
    /// `canonical_line` — so the grid can answer "which logical line is this
    /// row" in O(1) and verify that placements sit on the line they are
    /// anchored to (Workstream 10). The tag moves with its row through every
    /// grid mutation; a blank/revealed row carries [`LINE_TAG_NONE`] until the
    /// next render stamps it.
    line_tags: Vec<i64>,
    cursor: Option<SceneCursor>,
    image_layers: Vec<GraphicsSubmission>,
    placeholder_layers: Vec<GraphicsPlaceholderLayer>,
    #[cfg(feature = "sixel")]
    sixel_layers: Vec<SixelSubmission>,
}

/// No image covers this cell.
pub const IMAGE_REF_NONE: u32 = 0;
/// A row that currently displays no logical line (blank, revealed by a
/// scroll/insert/delete, or never stamped). `i64::MIN` can never collide with
/// a real tag: absolute logical lines start at 0 (the oldest history line).
pub const LINE_TAG_NONE: i64 = i64::MIN;
/// Kind bit for handles that resolve into `placeholder_layers` rather than
/// `image_layers`; the low bits (minus 1) are the layer index.
const PLACEHOLDER_REF_KIND: u32 = 0x8000_0000;

impl Scene {
    pub fn new(area: Rect) -> Self {
        let cell_count = area.width as usize * area.height as usize;
        let style = CellStyle::new(Color::reset(), Color::reset());
        Self {
            area,
            cells: vec![Cell::blank(style); cell_count],
            image_refs: vec![IMAGE_REF_NONE; cell_count],
            line_tags: vec![LINE_TAG_NONE; area.height as usize],
            cursor: None,
            image_layers: Vec::new(),
            placeholder_layers: Vec::new(),
            #[cfg(feature = "sixel")]
            sixel_layers: Vec::new(),
        }
    }

    /// Clears this scene in place to a blank frame of `area`, reallocating cell
    /// storage only when the area changes so a retained frame buffer can be
    /// reused across frames without a per-frame allocation.
    pub fn reset(&mut self, area: Rect) {
        self.area = area;
        let cell_count = area.width as usize * area.height as usize;
        let blank = Cell::blank(CellStyle::new(Color::reset(), Color::reset()));
        if self.cells.len() == cell_count {
            self.cells.fill(blank);
            self.image_refs.fill(IMAGE_REF_NONE);
        } else {
            self.cells.clear();
            self.cells.resize(cell_count, blank);
            self.image_refs.clear();
            self.image_refs.resize(cell_count, IMAGE_REF_NONE);
        }
        if self.line_tags.len() == area.height as usize {
            self.line_tags.fill(LINE_TAG_NONE);
        } else {
            self.line_tags.clear();
            self.line_tags.resize(area.height as usize, LINE_TAG_NONE);
        }
        self.cursor = None;
        self.image_layers.clear();
        self.placeholder_layers.clear();
        #[cfg(feature = "sixel")]
        self.sixel_layers.clear();
    }

    /// Replaces this scene's contents with `other`'s in place, reusing the
    /// existing cell-buffer allocation when the sizes match (a memcpy, not an
    /// allocation). Used to retain the previous frame in the compositor.
    pub fn replace_with(&mut self, other: &Scene) {
        self.area = other.area;
        if self.cells.len() == other.cells.len() {
            self.cells.copy_from_slice(&other.cells);
            self.image_refs.copy_from_slice(&other.image_refs);
        } else {
            self.cells.clear();
            self.cells.extend_from_slice(&other.cells);
            self.image_refs.clear();
            self.image_refs.extend_from_slice(&other.image_refs);
        }
        if self.line_tags.len() == other.line_tags.len() {
            self.line_tags.copy_from_slice(&other.line_tags);
        } else {
            self.line_tags.clear();
            self.line_tags.extend_from_slice(&other.line_tags);
        }
        self.cursor = other.cursor;
        self.image_layers.clear();
        self.image_layers.extend(other.image_layers.iter().cloned());
        self.placeholder_layers.clear();
        self.placeholder_layers
            .extend_from_slice(&other.placeholder_layers);
        #[cfg(feature = "sixel")]
        {
            self.sixel_layers.clear();
            self.sixel_layers.extend(other.sixel_layers.iter().cloned());
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

    /// The image reference stamped on the cell at `(x, y)`, or `IMAGE_REF_NONE`.
    pub fn image_ref_at(&self, x: u16, y: u16) -> u32 {
        match self.index(x, y) {
            Some(index) => self.image_refs[index],
            None => IMAGE_REF_NONE,
        }
    }

    /// The logical-line tag of the row at `y` (absolute history-relative
    /// line, or [`LINE_TAG_NONE`] when the row displays no line yet). O(1).
    pub fn line_tag_at(&self, y: u16) -> i64 {
        self.line_tags
            .get((y - self.area.y) as usize)
            .copied()
            .unwrap_or(LINE_TAG_NONE)
    }

    /// Stamps the logical-line tag of the row at `y`. The render path stamps
    /// every displayed row each frame from the emulator's absolute line;
    /// grid mutations move existing tags with their rows.
    pub fn set_line_tag(&mut self, y: u16, tag: i64) {
        let index = (y - self.area.y) as usize;
        if index < self.line_tags.len() {
            self.line_tags[index] = tag;
        }
    }

    /// The per-cell image references, parallel to `cells()` (row-major).
    pub fn image_refs(&self) -> &[u32] {
        &self.image_refs
    }

    /// Whether `handle` resolves into `image_layers` (kind bit clear) rather
    /// than `placeholder_layers` or none.
    pub const fn is_image_ref(handle: u32) -> bool {
        handle != IMAGE_REF_NONE && handle & PLACEHOLDER_REF_KIND == 0
    }

    /// Resolves an image-layer handle (kind bit clear) to its submission.
    pub fn image_layer_for_ref(&self, handle: u32) -> Option<&GraphicsSubmission> {
        if handle == IMAGE_REF_NONE || handle & PLACEHOLDER_REF_KIND != 0 {
            return None;
        }
        self.image_layers.get((handle as usize) - 1)
    }

    /// Resolves a placeholder-layer handle (kind bit set) to its layer.
    pub fn placeholder_layer_for_ref(&self, handle: u32) -> Option<&GraphicsPlaceholderLayer> {
        if handle == IMAGE_REF_NONE || handle & PLACEHOLDER_REF_KIND == 0 {
            return None;
        }
        self.placeholder_layers
            .get((handle & !PLACEHOLDER_REF_KIND) as usize - 1)
    }

    /// Recomputes the per-cell image references from the current layer lists:
    /// every cell covered by an image/placeholder layer is stamped with that
    /// layer's handle, and layers are applied in z order so the *topmost*
    /// covering layer wins per cell (the last write). Idempotent; the
    /// compositor runs it once after each composition so the retained frame
    /// always carries fresh annotations, and cells outside every layer are
    /// reset to `IMAGE_REF_NONE`.
    pub fn annotate_image_cells(&mut self) {
        self.image_refs.fill(IMAGE_REF_NONE);
        // Collect (area, handle) pairs first so the layer borrows end before
        // the cell stamps mutate `self`.
        let mut stamps =
            Vec::with_capacity(self.image_layers.len() + self.placeholder_layers.len());
        for (index, layer) in self.image_layers.iter().enumerate() {
            stamps.push((layer.placement().area(), (index as u32) + 1));
        }
        for (index, layer) in self.placeholder_layers.iter().enumerate() {
            stamps.push((layer.area(), PLACEHOLDER_REF_KIND | ((index as u32) + 1)));
        }
        for (area, handle) in stamps {
            self.stamp_image_ref(area, handle);
        }
    }

    fn stamp_image_ref(&mut self, area: Rect, handle: u32) {
        let x_start = area.x.max(self.area.x);
        let y_start = area.y.max(self.area.y);
        let x_end = (area.x as u32 + area.width as u32)
            .min(self.area.x as u32 + self.area.width as u32) as u16;
        let y_end = (area.y as u32 + area.height as u32)
            .min(self.area.y as u32 + self.area.height as u32) as u16;
        for y in y_start..y_end {
            for x in x_start..x_end {
                if let Some(index) = self.index(x, y) {
                    self.image_refs[index] = handle;
                }
            }
        }
    }

    /// Clears all image/placeholder/sixel layers in place, keeping the cell
    /// buffer and cursor intact. The compositor uses this to rebuild layer
    /// state in one pass before re-compositing only the dirty cell regions.
    pub fn clear_layers(&mut self) {
        self.image_layers.clear();
        self.placeholder_layers.clear();
        #[cfg(feature = "sixel")]
        self.sixel_layers.clear();
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
            cell.style = cell.style.with_dim(dim);
        }
    }

    pub fn add_image_layer(&mut self, submission: GraphicsSubmission) {
        if let Some(submission) = submission.clipped_to(self.area) {
            self.image_layers.push(submission);
            self.image_layers
                .sort_by_key(|layer| (layer.placement().z_index(), layer.resource()));
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
        self.accumulate_layers(source, clip);
        self.blit_cells(source, clip);
    }

    /// Accumulates `source`'s image/placeholder/sixel layers into this scene,
    /// clipping and occluding them against `clip`, without copying any cells.
    /// Kept separate from `blit_cells` so the compositor can rebuild layer
    /// state in one pass while only re-compositing dirty cell regions.
    pub fn accumulate_layers(&mut self, source: &Scene, clip: Rect) {
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
            .sort_by_key(|layer| (layer.placement().z_index(), layer.resource()));
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
    }

    /// Copies `source`'s cell content (and cursor) into this scene, clipped to
    /// `clip`, without touching image/placeholder/sixel layers.
    pub fn blit_cells(&mut self, source: &Scene, clip: Rect) {
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
        // Line tags travel with the blitted rows: the destination rows now
        // display the source's content, so they inherit its logical lines.
        // Tags are per-row, so a partial-width clip still transfers the whole
        // row's tag — in the compose flow a row comes from one source scene.
        for y in y_start..y_end {
            self.set_line_tag(y, source.line_tag_at(y));
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
        visible.sort_by_key(|layer| (layer.placement().z_index(), layer.resource()));
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

    /// Clips a vertical `[top, bottom)` region to this scene and requires it
    /// to be non-empty; `None` when there is nothing to operate on.
    fn clamp_region(&self, top: u16, bottom: u16) -> Option<(u16, u16)> {
        let top = top.max(self.area.y);
        let bottom = bottom.min(self.area.y.saturating_add(self.area.height));
        (top < bottom).then_some((top, bottom))
    }

    fn blank_rows(&mut self, y: u16, count: u16) {
        let width = self.area.width as usize;
        let start = self.index(self.area.x, y).unwrap_or(self.cells.len());
        let end = start
            .saturating_add(count as usize * width)
            .min(self.cells.len());
        let blank = Cell::blank(CellStyle::new(Color::reset(), Color::reset()));
        for index in start..end {
            self.cells[index] = blank;
            self.image_refs[index] = IMAGE_REF_NONE;
        }
        let tag_start = (y - self.area.y) as usize;
        let tag_end = (tag_start + count as usize).min(self.line_tags.len());
        self.line_tags[tag_start..tag_end].fill(LINE_TAG_NONE);
    }

    /// Scrolls the rows in `[top, bottom)` by `delta` rows, moving cells and
    /// their image references in lockstep. A positive delta moves content up
    /// (blanking the bottom `delta` rows), matching `record_scroll`'s
    /// convention; a negative delta moves content down. Wide/continuation
    /// cells stay intact because a wide glyph never spans rows.
    pub fn scroll_region(&mut self, top: u16, bottom: u16, delta: i16) {
        let Some((top, bottom)) = self.clamp_region(top, bottom) else {
            return;
        };
        let width = self.area.width as usize;
        let height = (bottom - top) as usize;
        if delta.unsigned_abs() as usize >= height {
            self.blank_rows(top, bottom - top);
            return;
        }
        if delta > 0 {
            let shift = delta as usize;
            let src = (top as usize + shift) * width..(bottom as usize) * width;
            self.cells.copy_within(src.clone(), (top as usize) * width);
            self.image_refs.copy_within(src, (top as usize) * width);
            let tag_src = (top as usize + shift)..(bottom as usize);
            self.line_tags.copy_within(tag_src, top as usize);
            self.blank_rows(bottom - shift as u16, shift as u16);
        } else {
            let shift = delta.unsigned_abs() as usize;
            let src = (top as usize) * width..(bottom as usize - shift) * width;
            self.cells
                .copy_within(src.clone(), (top as usize + shift) * width);
            self.image_refs
                .copy_within(src, (top as usize + shift) * width);
            let tag_src = (top as usize)..(bottom as usize - shift);
            self.line_tags.copy_within(tag_src, top as usize + shift);
            self.blank_rows(top, shift as u16);
        }
    }

    /// Scrolls the whole scene by `delta` rows (positive = content moves up).
    pub fn scroll_rows(&mut self, delta: i16) {
        self.scroll_region(
            self.area.y,
            self.area.y.saturating_add(self.area.height),
            delta,
        );
    }

    /// Inserts `count` blank rows at `top`, pushing rows in `[top, bottom)`
    /// down and dropping the rows that fall off the bottom of the scene. The
    /// blanked rows carry no image references.
    pub fn insert_lines(&mut self, top: u16, count: u16) {
        if count == 0 {
            return;
        }
        let bottom = self.area.y.saturating_add(self.area.height);
        let Some((top, bottom)) = self.clamp_region(top, bottom) else {
            return;
        };
        let width = self.area.width as usize;
        let shift = (count as usize).min((bottom - top) as usize);
        let src = (top as usize) * width..(bottom as usize - shift) * width;
        self.cells
            .copy_within(src.clone(), (top as usize + shift) * width);
        self.image_refs
            .copy_within(src, (top as usize + shift) * width);
        self.line_tags.copy_within(
            (top as usize)..(bottom as usize - shift),
            top as usize + shift,
        );
        self.blank_rows(top, shift as u16);
    }

    /// Deletes `count` rows starting at `top`, pulling the rows below them up
    /// and blanking the rows revealed at the bottom of the scene. Image
    /// references move with their rows; the blanked rows carry none.
    pub fn delete_lines(&mut self, top: u16, count: u16) {
        if count == 0 {
            return;
        }
        let bottom = self.area.y.saturating_add(self.area.height);
        let Some((top, bottom)) = self.clamp_region(top, bottom) else {
            return;
        };
        let width = self.area.width as usize;
        let shift = (count as usize).min((bottom - top) as usize);
        let src = (top as usize + shift) * width..(bottom as usize) * width;
        self.cells.copy_within(src.clone(), (top as usize) * width);
        self.image_refs.copy_within(src, (top as usize) * width);
        self.line_tags
            .copy_within((top as usize + shift)..(bottom as usize), top as usize);
        self.blank_rows(bottom - shift as u16, shift as u16);
    }

    /// Blanks `count` rows starting at `y`, dropping their image references.
    pub fn erase_rows(&mut self, y: u16, count: u16) {
        let Some((top, _)) = self.clamp_region(y, y.saturating_add(count)) else {
            return;
        };
        self.blank_rows(top, count);
    }

    /// Blanks the cells inside `rect`, dropping their image references.
    pub fn erase_region(&mut self, rect: Rect) {
        let x_start = rect.x.max(self.area.x);
        let y_start = rect.y.max(self.area.y);
        let x_end = (rect.x as u32 + rect.width as u32)
            .min(self.area.x as u32 + self.area.width as u32) as u16;
        let y_end = (rect.y as u32 + rect.height as u32)
            .min(self.area.y as u32 + self.area.height as u32) as u16;
        let blank = Cell::blank(CellStyle::new(Color::reset(), Color::reset()));
        for y in y_start..y_end {
            for x in x_start..x_end {
                if let Some(index) = self.index(x, y) {
                    self.cells[index] = blank;
                    self.image_refs[index] = IMAGE_REF_NONE;
                }
            }
        }
        for y in y_start..y_end {
            let row = usize::from(y.saturating_sub(self.area.y));
            if row < self.line_tags.len() {
                self.line_tags[row] = LINE_TAG_NONE;
            }
        }
    }

    /// Re-lays out the grid at a new column width (Termux-style display
    /// reflow), wrapping rows that overflow and carrying cells *and* their
    /// image references together. A placement whose references cross a wrap
    /// boundary is re-sliced onto the wrapped rows — its handle is unchanged,
    /// so the grid still resolves it to the same covering submission. Wide
    /// glyphs are never split: a lead and its continuation wrap as one unit.
    ///
    /// The layer lists are cleared because placement rectangles are not
    /// reflow-aware (a split placement cannot be one rect); the caller
    /// re-accumulates layers from the source scenes — which the compositor
    /// does every frame — before the next `annotate_image_cells`. The
    /// reference array is the grid's own truth and survives the reflow.
    pub fn reflow(&mut self, columns: u16) {
        if columns == 0 || columns == self.area.width {
            return;
        }
        let old_width = usize::from(self.area.width);
        let new_width = usize::from(columns);
        let old_cells = std::mem::take(&mut self.cells);
        let old_refs = std::mem::take(&mut self.image_refs);
        let mut new_cells: Vec<Cell> = Vec::with_capacity(old_cells.len());
        let mut new_refs: Vec<u32> = Vec::with_capacity(old_refs.len());
        let mut new_tags: Vec<i64> = Vec::with_capacity(self.line_tags.len());

        let blank = Cell::blank(CellStyle::new(Color::reset(), Color::reset()));
        for row_start in (0..old_cells.len()).step_by(old_width) {
            let row_end = (row_start + old_width).min(old_cells.len());
            // Every row this old row re-wraps into keeps its logical-line tag:
            // a wrapped paragraph is one logical line, so the split segments
            // (and their recombination on a later grow) all share the parent
            // tag, exactly like Zellij's line-merge/split anchoring.
            let tag = self
                .line_tags
                .get(row_start / old_width)
                .copied()
                .unwrap_or(LINE_TAG_NONE);
            let before = new_cells.len();
            // Tokenize the row, merging each wide lead with its continuation
            // so a glyph never splits across a wrap boundary.
            let mut tokens: Vec<Vec<(Cell, u32)>> = Vec::new();
            let mut i = row_start;
            while i < row_end {
                let cell = old_cells[i];
                let reference = old_refs[i];
                if cell.width == CellWidth::Wide && i + 1 < row_end {
                    tokens.push(vec![(cell, reference), (old_cells[i + 1], old_refs[i + 1])]);
                    i += 2;
                } else {
                    tokens.push(vec![(cell, reference)]);
                    i += 1;
                }
            }
            // Wrap the token stream into rows of at most `columns` cells. A
            // token wider than the row starts a new row even if it overflows
            // (a degenerate 1-column scene), rather than being dropped. Each
            // old row re-wraps independently (the composed grid has no
            // line-wrap metadata, so every row is its own paragraph), and the
            // final partial segment is padded to the new width so the buffer
            // stays strictly row-major.
            let mut segment: Vec<(Cell, u32)> = Vec::with_capacity(new_width);
            for token in tokens {
                if !segment.is_empty() && segment.len() + token.len() > new_width {
                    for (cell, reference) in segment.drain(..) {
                        new_cells.push(cell);
                        new_refs.push(reference);
                    }
                }
                segment.extend(token);
            }
            while segment.len() < new_width {
                segment.push((blank, IMAGE_REF_NONE));
            }
            for (cell, reference) in segment {
                new_cells.push(cell);
                new_refs.push(reference);
            }
            let rows_produced = (new_cells.len() - before) / new_width;
            new_tags.extend(std::iter::repeat_n(tag, rows_produced));
        }

        self.cells = new_cells;
        self.image_refs = new_refs;
        self.line_tags = new_tags;
        self.area.width = columns;
        self.area.height = u16::try_from(self.cells.len() / new_width.max(1)).unwrap_or(u16::MAX);
        self.image_layers.clear();
        self.placeholder_layers.clear();
        #[cfg(feature = "sixel")]
        self.sixel_layers.clear();
    }

    /// Blanks the entire scene, dropping every image reference. Keeps the
    /// area and cursor; layers are cleared so a later `annotate_image_cells`
    /// has nothing to restore.
    pub fn clear(&mut self) {
        let blank = Cell::blank(CellStyle::new(Color::reset(), Color::reset()));
        self.cells.fill(blank);
        self.image_refs.fill(IMAGE_REF_NONE);
        self.line_tags.fill(LINE_TAG_NONE);
        self.image_layers.clear();
        self.placeholder_layers.clear();
        #[cfg(feature = "sixel")]
        self.sixel_layers.clear();
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

    fn style() -> CellStyle {
        CellStyle::new(Color::rgb(1, 2, 3), Color::rgb(4, 5, 6))
    }

    #[test]
    fn cell_style_is_a_compact_handle_with_global_dedup() {
        // The handle is a single 4-byte table index, not the expanded style
        // struct, so the cell buffer shrinks accordingly.
        assert_eq!(std::mem::size_of::<CellStyle>(), std::mem::size_of::<u32>());

        let first = CellStyle::new(Color::rgb(1, 2, 3), Color::rgb(4, 5, 6));
        let again = CellStyle::new(Color::rgb(1, 2, 3), Color::rgb(4, 5, 6));
        let other = CellStyle::new(Color::rgb(9, 9, 9), Color::rgb(4, 5, 6));
        assert_eq!(first, again, "identical styles must resolve to one handle");
        assert_ne!(first, other, "distinct styles must get distinct handles");

        let data = first.resolve();
        assert_eq!(data.foreground, Color::rgb(1, 2, 3));
        assert_eq!(data.background, Color::rgb(4, 5, 6));
        assert!(!data.bold);
        assert!(first.bold().resolve().bold);
    }

    #[test]
    fn text_is_clipped_to_the_scene() {
        let mut scene = Scene::new(Rect::new(0, 0, 4, 1));
        scene.text(2, 0, "abcd", style());

        assert_eq!(scene.cell_at(0, 0).unwrap().symbol, ' ');
        assert_eq!(scene.cell_at(1, 0).unwrap().symbol, ' ');
        assert_eq!(scene.cell_at(2, 0).unwrap().symbol, 'a');
        assert_eq!(scene.cell_at(3, 0).unwrap().symbol, 'b');
    }

    #[test]
    fn wide_text_tracks_its_continuation_cell() {
        let mut scene = Scene::new(Rect::new(0, 0, 5, 1));
        scene.text(0, 0, "界a", style());

        assert_eq!(scene.cell_at(0, 0).unwrap().symbol, '界');
        assert_eq!(scene.cell_at(0, 0).unwrap().width, CellWidth::Wide);
        assert_eq!(scene.cell_at(1, 0).unwrap().width, CellWidth::Continuation);
        assert_eq!(scene.cell_at(2, 0).unwrap().symbol, 'a');
        assert_eq!(scene.cell_at(2, 0).unwrap().width, CellWidth::Narrow);
    }

    #[test]
    fn wide_text_is_not_started_when_it_would_be_clipped() {
        let mut scene = Scene::new(Rect::new(0, 0, 2, 1));
        scene.text(1, 0, "界", style());

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
    fn image_layers_tie_break_equal_z_by_image_id() {
        let mut store = crate::SessionGraphicsStore::new(crate::SessionId::new(5));
        store
            .apply_kitty_command(b"a=T,f=24,i=6,z=0", b"AQID")
            .unwrap();
        store
            .apply_kitty_command(b"a=T,f=24,i=5,z=0", b"BAUG")
            .unwrap();
        let submissions = store.visible_submissions(Rect::new(0, 0, 8, 2));
        // Add the layers in reverse order to prove the scene re-sorts by
        // (z, image id) rather than preserving insertion order.
        let mut scene = Scene::new(Rect::new(0, 0, 8, 2));
        for submission in submissions.iter().rev() {
            scene.add_image_layer(submission.clone());
        }
        let ids = scene
            .image_layers()
            .iter()
            .map(|layer| layer.resource().image())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![5, 6]);
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
        scene.set(0, 0, 'x', style());
        scene.set(2, 3, 'o', style());

        assert!(scene.cell_at(0, 0).is_none());
        assert_eq!(scene.cell_at(2, 3).unwrap().symbol, 'o');
    }

    fn dashboard_submission(x: u16, y: u16, w: u16, h: u16, key: u64) -> GraphicsSubmission {
        let resource = crate::graphics::GraphicsResourceId::new(
            crate::graphics::DASHBOARD_SESSION,
            key as u32,
        );
        let placement =
            crate::graphics::GraphicsPlacement::dashboard(resource, x, y, w, h, 0, key, key as u32);
        GraphicsSubmission::from_rgba(resource, &[255, 0, 0, 255], 1, 1, 1, placement)
    }

    #[test]
    fn image_refs_stamp_covered_cells_and_leave_the_rest_empty() {
        let mut scene = Scene::new(Rect::new(0, 0, 6, 4));
        scene.add_image_layer(dashboard_submission(1, 1, 2, 2, 7));
        scene.annotate_image_cells();

        let handle = scene.image_ref_at(1, 1);
        assert_ne!(handle, IMAGE_REF_NONE);
        assert_eq!(handle, scene.image_ref_at(2, 1));
        assert_eq!(handle, scene.image_ref_at(1, 2));
        assert_eq!(handle, scene.image_ref_at(2, 2));
        assert_eq!(scene.image_ref_at(0, 0), IMAGE_REF_NONE);
        assert_eq!(scene.image_ref_at(3, 1), IMAGE_REF_NONE);
        assert_eq!(scene.image_ref_at(1, 3), IMAGE_REF_NONE);

        // The handle resolves back to the covering submission.
        let resolved = scene.image_layer_for_ref(handle).expect("handle resolves");
        assert_eq!(resolved.placement().key(), 7);
        assert!(scene.placeholder_layer_for_ref(handle).is_none());
    }

    #[test]
    fn placeholder_layers_stamp_with_the_kind_bit() {
        let resource =
            crate::graphics::GraphicsResourceId::new(crate::graphics::DASHBOARD_SESSION, 1);
        let mut scene = Scene::new(Rect::new(0, 0, 4, 2));
        scene.add_placeholder_layer(crate::graphics::GraphicsPlaceholderLayer::new(
            resource,
            Rect::new(2, 0, 1, 1),
            0,
        ));
        scene.annotate_image_cells();

        assert_eq!(scene.image_ref_at(1, 0), IMAGE_REF_NONE);
        let handle = scene.image_ref_at(2, 0);
        assert_ne!(handle, IMAGE_REF_NONE);
        assert!(
            !Scene::is_image_ref(handle),
            "placeholder handles are not image handles"
        );
        assert!(scene.placeholder_layer_for_ref(handle).is_some());
        assert!(scene.image_layer_for_ref(handle).is_none());
    }

    #[test]
    fn annotate_image_cells_is_idempotent() {
        let mut scene = Scene::new(Rect::new(0, 0, 4, 2));
        scene.add_image_layer(dashboard_submission(0, 0, 2, 2, 7));
        scene.annotate_image_cells();
        let first = scene.image_refs().to_vec();
        scene.annotate_image_cells();
        assert_eq!(first, scene.image_refs());
    }

    #[test]
    fn scroll_region_moves_image_refs_with_cells() {
        let mut scene = Scene::new(Rect::new(0, 0, 4, 6));
        scene.add_image_layer(dashboard_submission(1, 3, 1, 1, 7));
        scene.annotate_image_cells();
        let handle = scene.image_ref_at(1, 3);
        assert_ne!(handle, IMAGE_REF_NONE);

        scene.scroll_region(0, 6, 2);
        assert_eq!(scene.image_ref_at(1, 3), IMAGE_REF_NONE);
        assert_eq!(
            scene.image_ref_at(1, 1),
            handle,
            "content moved up two rows"
        );
        for y in 4..6 {
            assert_eq!(
                scene.image_ref_at(1, y),
                IMAGE_REF_NONE,
                "vacated rows are blank"
            );
        }

        scene.scroll_region(0, 6, -1);
        assert_eq!(
            scene.image_ref_at(1, 2),
            handle,
            "content moved back down one row"
        );
    }

    #[test]
    fn insert_and_delete_lines_shift_image_refs() {
        let mut scene = Scene::new(Rect::new(0, 0, 4, 6));
        scene.add_image_layer(dashboard_submission(1, 3, 1, 1, 7));
        scene.annotate_image_cells();
        let handle = scene.image_ref_at(1, 3);

        scene.insert_lines(2, 2);
        assert_eq!(
            scene.image_ref_at(1, 5),
            handle,
            "insert pushes content down"
        );
        assert_eq!(scene.image_ref_at(1, 2), IMAGE_REF_NONE);
        assert_eq!(scene.image_ref_at(1, 3), IMAGE_REF_NONE);

        scene.delete_lines(2, 2);
        assert_eq!(
            scene.image_ref_at(1, 3),
            handle,
            "delete pulls content back up"
        );
    }

    #[test]
    fn erase_ops_drop_image_refs_and_clear_drops_everything() {
        let mut scene = Scene::new(Rect::new(0, 0, 6, 4));
        scene.add_image_layer(dashboard_submission(1, 1, 3, 2, 7));
        scene.annotate_image_cells();
        assert_ne!(scene.image_ref_at(2, 1), IMAGE_REF_NONE);

        scene.erase_region(Rect::new(0, 0, 2, 4));
        assert_eq!(scene.image_ref_at(1, 1), IMAGE_REF_NONE);
        assert_ne!(
            scene.image_ref_at(2, 1),
            IMAGE_REF_NONE,
            "outside the erased region"
        );

        scene.erase_rows(2, 1);
        assert_eq!(scene.image_ref_at(2, 2), IMAGE_REF_NONE);
        assert_ne!(scene.image_ref_at(2, 1), IMAGE_REF_NONE);

        scene.clear();
        assert!(scene.image_refs().iter().all(|r| *r == IMAGE_REF_NONE));
        assert!(scene.image_layers().is_empty());
    }

    #[test]
    fn reflow_wraps_rows_and_carries_image_refs_across_the_boundary() {
        // A 6-column scene: text in row 0, an image anchored on the wrap
        // boundary at columns 4-5 of row 2.
        let mut scene = Scene::new(Rect::new(0, 0, 6, 3));
        for (x, symbol) in ['a', 'b', 'c', 'd', 'e', 'f'].into_iter().enumerate() {
            scene.set(x as u16, 0, symbol, style());
        }
        scene.add_image_layer(dashboard_submission(4, 2, 2, 1, 7));
        scene.annotate_image_cells();
        let handle = scene.image_ref_at(4, 2);
        assert_ne!(handle, IMAGE_REF_NONE);

        scene.reflow(4);

        assert_eq!(scene.area().width, 4);
        assert_eq!(scene.area().height, 6, "each old row re-wraps to two rows");
        // Row 0's text re-wrapped: 'a'..'d' on the first row, 'e','f' padded
        // with blanks on the second.
        assert_eq!(scene.cell_at(0, 0).unwrap().symbol, 'a');
        assert_eq!(scene.cell_at(3, 0).unwrap().symbol, 'd');
        assert_eq!(scene.cell_at(0, 1).unwrap().symbol, 'e');
        assert_eq!(scene.cell_at(1, 1).unwrap().symbol, 'f');
        // The placement's references re-sliced onto the wrapped rows: old row
        // 2 becomes rows 4-5, and the image's columns 4-5 land on row 5.
        assert_eq!(scene.image_ref_at(0, 4), IMAGE_REF_NONE);
        assert_eq!(scene.image_ref_at(0, 5), handle);
        assert_eq!(scene.image_ref_at(1, 5), handle);
        assert_eq!(scene.image_ref_at(2, 5), IMAGE_REF_NONE);
        assert!(
            scene.image_layers().is_empty(),
            "layer rects are not reflow-aware"
        );
    }

    #[test]
    fn reflow_never_splits_a_wide_glyph_across_a_wrap_boundary() {
        let mut scene = Scene::new(Rect::new(0, 0, 5, 1));
        scene.set(0, 0, 'a', style());
        scene.set(1, 0, '界', style()); // wide: lead at 1, continuation at 2
        scene.set(3, 0, 'b', style());
        scene.set(4, 0, 'c', style());

        scene.reflow(3);

        // The wide pair must stay together; the wrap happens before it.
        assert_eq!(scene.cell_at(0, 0).unwrap().symbol, 'a');
        assert_eq!(scene.cell_at(1, 0).unwrap().symbol, '界');
        assert_eq!(scene.cell_at(2, 0).unwrap().width, CellWidth::Continuation);
        assert_eq!(scene.cell_at(0, 1).unwrap().symbol, 'b');
        assert_eq!(scene.cell_at(1, 1).unwrap().symbol, 'c');
        // No continuation cell dangles at a row start.
        assert_ne!(scene.cell_at(0, 1).unwrap().width, CellWidth::Continuation);
    }

    #[test]
    fn line_tags_are_stamped_and_move_with_scroll_region() {
        let mut scene = Scene::new(Rect::new(0, 0, 4, 6));
        assert_eq!(scene.line_tag_at(0), LINE_TAG_NONE, "rows start untagged");
        for y in 0..6 {
            scene.set_line_tag(y, 100 + i64::from(y));
        }

        scene.scroll_region(0, 6, 2);
        // Content moved up two rows: the tag that was on row 2 is on row 0.
        assert_eq!(scene.line_tag_at(0), 102);
        assert_eq!(scene.line_tag_at(3), 105);
        assert_eq!(
            scene.line_tag_at(4),
            LINE_TAG_NONE,
            "vacated rows are untagged"
        );
        assert_eq!(scene.line_tag_at(5), LINE_TAG_NONE);

        scene.scroll_region(0, 6, -1);
        // Content moved back down one row.
        assert_eq!(scene.line_tag_at(1), 102);
        assert_eq!(scene.line_tag_at(2), 103);
    }

    #[test]
    fn line_tags_move_with_insert_and_delete_lines() {
        let mut scene = Scene::new(Rect::new(0, 0, 4, 6));
        for y in 0..6 {
            scene.set_line_tag(y, 50 + i64::from(y));
        }

        scene.insert_lines(2, 2);
        assert_eq!(scene.line_tag_at(2), LINE_TAG_NONE);
        assert_eq!(scene.line_tag_at(3), LINE_TAG_NONE);
        assert_eq!(scene.line_tag_at(4), 52, "content pushed down");
        assert_eq!(scene.line_tag_at(5), 53);

        scene.delete_lines(2, 2);
        assert_eq!(scene.line_tag_at(2), 52, "content pulled back up");
        assert_eq!(scene.line_tag_at(3), 53);
        assert_eq!(scene.line_tag_at(5), LINE_TAG_NONE);
    }

    #[test]
    fn erase_and_clear_drop_line_tags() {
        let mut scene = Scene::new(Rect::new(0, 0, 6, 4));
        for y in 0..4 {
            scene.set_line_tag(y, 200 + i64::from(y));
        }

        scene.erase_region(Rect::new(0, 0, 2, 4));
        assert_eq!(scene.line_tag_at(0), LINE_TAG_NONE);
        assert_eq!(scene.line_tag_at(1), LINE_TAG_NONE);
        assert_eq!(scene.line_tag_at(3), LINE_TAG_NONE);

        scene.set_line_tag(2, 202);
        scene.erase_rows(2, 1);
        assert_eq!(scene.line_tag_at(2), LINE_TAG_NONE);

        scene.set_line_tag(3, 203);
        scene.clear();
        assert!(scene.line_tags.iter().all(|tag| *tag == LINE_TAG_NONE));
    }

    #[test]
    fn reflow_split_rows_keep_the_parent_line_tag() {
        let mut scene = Scene::new(Rect::new(0, 0, 6, 2));
        scene.set_line_tag(0, 10);
        scene.set_line_tag(1, 11);

        scene.reflow(4);

        // Each old row re-wraps into two rows sharing the parent tag: a
        // wrapped paragraph is one logical line.
        assert_eq!(scene.line_tag_at(0), 10);
        assert_eq!(scene.line_tag_at(1), 10);
        assert_eq!(scene.line_tag_at(2), 11);
        assert_eq!(scene.line_tag_at(3), 11);
    }

    #[test]
    fn blit_copies_line_tags_with_cells() {
        let mut source = Scene::new(Rect::new(0, 0, 4, 3));
        for y in 0..3 {
            source.set_line_tag(y, 300 + i64::from(y));
        }
        let mut scene = Scene::new(Rect::new(0, 0, 6, 4));

        scene.blit_cells(&source, Rect::new(0, 0, 4, 3));

        assert_eq!(scene.line_tag_at(0), 300);
        assert_eq!(scene.line_tag_at(1), 301);
        assert_eq!(scene.line_tag_at(2), 302);
        assert_eq!(
            scene.line_tag_at(3),
            LINE_TAG_NONE,
            "rows outside the blit keep their state"
        );
    }
}
