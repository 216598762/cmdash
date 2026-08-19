use std::collections::{BTreeMap, HashMap, HashSet};

use ratatui::layout::Rect;

use crate::{
    scene::{Cell, CellStyle, Scene, SceneCursor},
    state::{AppState, FocusTarget, Overlay, OverlayId, Surface, SurfaceId, WidgetId},
};

/// Compact, per-frame handle for an interned `CellStyle`. Span grouping keys off
/// this id rather than the expanded 9-field `CellStyle` struct.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct StyleId(u32);

/// Per-frame style interner: stores each distinct `CellStyle` once and returns
/// a `StyleId` handle, so repeated styles collapse to a single entry and the
/// span-grouping hot path compares small integers.
#[derive(Clone, Debug, Default)]
struct StyleInterner {
    /// `StyleId` → `CellStyle` (the id is the table index).
    styles: Vec<CellStyle>,
    /// `CellStyle` → `StyleId` for O(1) dedup.
    ids: HashMap<CellStyle, StyleId>,
}

impl StyleInterner {
    fn clear(&mut self) {
        self.styles.clear();
        self.ids.clear();
    }

    fn intern(&mut self, style: CellStyle) -> StyleId {
        if let Some(&id) = self.ids.get(&style) {
            return id;
        }
        let id = StyleId(self.styles.len() as u32);
        self.styles.push(style);
        self.ids.insert(style, id);
        id
    }

    fn distinct_styles(&self) -> usize {
        self.styles.len()
    }
}

/// Reusable scratch buffers for the per-frame diff work (cell changes, row
/// spans, and graphics/placeholder/sixel layer vectors). The compositor takes
/// them out each frame, fills them, and the main loop recycles them back via
/// `Compositor::recycle`, so steady-state rendering performs no scratch
/// allocation. Bounded by the frame's own cell/layer counts (a buffer only
/// ever retains the capacity of the largest frame it has seen).
#[derive(Clone, Debug, Default)]
struct FrameBufferPool {
    changes: Vec<CellChange>,
    spans: Vec<CellSpan>,
    graphics: Vec<crate::graphics::GraphicsSubmission>,
    visible_graphics: Vec<crate::graphics::GraphicsSubmission>,
    removed_graphics: Vec<crate::graphics::GraphicsSubmission>,
    placeholders: Vec<crate::graphics::GraphicsPlaceholderLayer>,
    visible_placeholders: Vec<crate::graphics::GraphicsPlaceholderLayer>,
    removed_placeholders: Vec<crate::graphics::GraphicsPlaceholderLayer>,
    #[cfg(feature = "sixel")]
    sixel: Vec<crate::sixel::SixelSubmission>,
    styles: StyleInterner,
    last_frame_styles: usize,
    scratch_reallocations: u64,
}

/// Takes a pooled scratch vector and returns it alongside the capacity it
/// carried in, so the caller can detect whether filling it caused a fresh
/// allocation this frame.
fn take_scratch<T>(slot: &mut Vec<T>) -> (Vec<T>, usize) {
    let taken = std::mem::take(slot);
    let capacity = taken.capacity();
    (taken, capacity)
}

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
    style_id: StyleId,
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
    placeholders: Vec<crate::graphics::GraphicsPlaceholderLayer>,
    visible_placeholders: Vec<crate::graphics::GraphicsPlaceholderLayer>,
    removed_placeholders: Vec<crate::graphics::GraphicsPlaceholderLayer>,
    cursor: Option<SceneCursor>,
    cursor_changed: bool,
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

    pub fn placeholders(&self) -> &[crate::graphics::GraphicsPlaceholderLayer] {
        &self.placeholders
    }

    pub fn visible_placeholders(&self) -> &[crate::graphics::GraphicsPlaceholderLayer] {
        &self.visible_placeholders
    }

    pub fn removed_placeholders(&self) -> &[crate::graphics::GraphicsPlaceholderLayer] {
        &self.removed_placeholders
    }

    pub const fn cursor(&self) -> Option<SceneCursor> {
        self.cursor
    }

    pub const fn cursor_changed(&self) -> bool {
        self.cursor_changed
    }

    #[cfg(feature = "sixel")]
    pub fn sixel(&self) -> &[crate::sixel::SixelSubmission] {
        &self.sixel
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
            && self.graphics.is_empty()
            && self.removed_graphics.is_empty()
            && self.placeholders.is_empty()
            && self.removed_placeholders.is_empty()
            && !self.cursor_changed
            && {
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
    /// Retained composed frame buffer, reused across frames by the main loop.
    composed: Option<Scene>,
    /// Retained previous generation, updated in place (never full-frame cloned).
    previous: Option<Scene>,
    pending_invalidations: Vec<Rect>,
    composed_reallocations: u64,
    previous_reallocations: u64,
    /// Per-frame snapshots used to detect structural (geometry/visibility/
    /// z-order/focus/overlay) changes that must dirty a surface region.
    surface_snapshot: BTreeMap<SurfaceId, Surface>,
    overlay_snapshot: BTreeMap<OverlayId, Overlay>,
    focus_snapshot: Option<FocusTarget>,
    base_snapshot: Option<Scene>,
    pool: FrameBufferPool,
    /// Cached z-ordered visible surface/overlay lists, recomputed only when the
    /// surface/overlay set, visibility, or z-index changes (so a steady frame
    /// does not re-sort and re-fetch them).
    surface_order: Vec<(i16, SurfaceId)>,
    overlay_order: Vec<(i16, OverlayId)>,
    surface_order_dirty: bool,
    overlay_order_dirty: bool,
    surface_order_computed: bool,
    overlay_order_computed: bool,
    z_order_recomputations: u64,
}

/// The regions the current frame must re-composite and re-diff, plus the
/// explicit invalidation rects that force a region into the diff even when the
/// recomposed cells are unchanged.
struct Damage {
    full_redraw: bool,
    dirty: Vec<Rect>,
    invalidated: Vec<Rect>,
}

impl Compositor {
    pub fn new() -> Self {
        Self {
            composed: None,
            previous: None,
            pending_invalidations: Vec::new(),
            composed_reallocations: 0,
            previous_reallocations: 0,
            surface_snapshot: BTreeMap::new(),
            overlay_snapshot: BTreeMap::new(),
            focus_snapshot: None,
            base_snapshot: None,
            pool: FrameBufferPool::default(),
            surface_order: Vec::new(),
            overlay_order: Vec::new(),
            surface_order_dirty: false,
            overlay_order_dirty: false,
            surface_order_computed: false,
            overlay_order_computed: false,
            z_order_recomputations: 0,
        }
    }

    /// Composes a fresh, owned `Scene` for a single frame. Retained for callers
    /// that need an owned scene (tests); the main loop uses `compose_and_diff`
    /// so the composed buffer is reused in place.
    pub fn compose(
        &self,
        viewport: Rect,
        state: &AppState,
        base: &Scene,
        surface_scenes: &BTreeMap<SurfaceId, Scene>,
    ) -> Scene {
        let mut composed = Scene::new(viewport);
        blit_frame(&mut composed, viewport, state, base, surface_scenes);
        composed
    }

    /// Composes the frame into the retained buffer and diffs it against the
    /// previous generation. The composed buffer is available via `frame()` for
    /// snapshot consumers.
    ///
    /// `changed_widgets` is the set of widget ids whose update reported a
    /// redraw this frame; only those surfaces (plus structural/base damage)
    /// are re-composited and re-diffed, so a steady frame touches no
    /// unchanged cells. On the first frame, a resize, or while a UI animation
    /// is active, the whole frame is redrawn.
    pub fn compose_and_diff(
        &mut self,
        viewport: Rect,
        state: &AppState,
        base: &Scene,
        surface_scenes: &BTreeMap<SurfaceId, Scene>,
        changed_widgets: &[WidgetId],
    ) -> FrameDiff {
        let damage = self.compute_damage(viewport, state, base, changed_widgets);
        self.compose_into(viewport, state, base, surface_scenes, &damage);
        let current = self
            .composed
            .as_ref()
            .expect("composed frame is initialized");
        diff_regions(
            current,
            &mut self.previous,
            &mut self.previous_reallocations,
            &mut self.pool,
            &damage,
        )
    }

    /// The retained composed buffer from the last `compose_and_diff`.
    pub fn frame(&self) -> &Scene {
        self.composed
            .as_ref()
            .expect("no frame has been composed yet")
    }

    /// Number of times the retained buffers (re)allocated their cell storage.
    /// Steady-state frames reuse the buffers, so this advances only on the
    /// first frame and viewport resizes.
    pub const fn retained_buffer_reallocations(&self) -> u64 {
        self.composed_reallocations + self.previous_reallocations
    }

    /// Number of times the pooled scratch vectors allocated fresh storage.
    /// Steady-state frames that recycle their diff keep this flat after the
    /// first frame.
    pub const fn scratch_reallocations(&self) -> u64 {
        self.pool.scratch_reallocations
    }

    /// Number of distinct `CellStyle` values interned during the last frame,
    /// proving repeated styles collapse to a single handle.
    pub const fn last_frame_distinct_styles(&self) -> usize {
        self.pool.last_frame_styles
    }

    /// Number of times the cached z-ordered surface/overlay lists were
    /// recomputed. Steady-state frames reuse the cache, so this advances only
    /// when the surface/overlay set, visibility, or z-order changes.
    pub const fn z_order_recomputations(&self) -> u64 {
        self.z_order_recomputations
    }

    /// Returns the scratch vectors owned by `diff` to the pool so the next
    /// frame reuses their allocation instead of reallocating. Call this after
    /// the backend has fully consumed the diff.
    pub fn recycle(&mut self, diff: FrameDiff) {
        let FrameDiff {
            changes,
            spans,
            graphics,
            visible_graphics,
            removed_graphics,
            placeholders,
            visible_placeholders,
            removed_placeholders,
            #[cfg(feature = "sixel")]
            sixel,
            ..
        } = diff;
        let pool = &mut self.pool;
        pool.changes = changes;
        pool.spans = spans;
        pool.graphics = graphics;
        pool.visible_graphics = visible_graphics;
        pool.removed_graphics = removed_graphics;
        pool.placeholders = placeholders;
        pool.visible_placeholders = visible_placeholders;
        pool.removed_placeholders = removed_placeholders;
        #[cfg(feature = "sixel")]
        {
            pool.sixel = sixel;
        }
    }

    /// Computes which regions the current frame must re-composite: the whole
    /// viewport on a full redraw, or the union of base changes, changed
    /// widgets, structural surface/overlay/focus changes, and explicit
    /// invalidations on a partial frame.
    fn compute_damage(
        &mut self,
        viewport: Rect,
        state: &AppState,
        base: &Scene,
        changed_widgets: &[WidgetId],
    ) -> Damage {
        let full_redraw = self
            .previous
            .as_ref()
            .is_none_or(|previous| previous.area() != viewport)
            || self
                .composed
                .as_ref()
                .is_none_or(|composed| composed.area() != viewport)
            || state.animation_schedule().is_some();

        let invalidated: Vec<Rect> = self
            .pending_invalidations
            .drain(..)
            .filter_map(|area| intersect(area, viewport))
            .collect();
        let mut dirty: Vec<Rect> = invalidated.clone();

        if full_redraw {
            dirty.push(viewport);
        } else {
            // Base shell damage: the static chrome (header/footer) changes
            // independently of the surfaces.
            match &self.base_snapshot {
                None => dirty.push(viewport),
                Some(previous) if previous.cells() != base.cells() => {
                    dirty.push(base_diff_rect(previous, base, viewport));
                }
                _ => {}
            }

            // Widget-driven damage: a widget that reported a redraw dirties
            // its surface area.
            for &widget in changed_widgets {
                if let Some((&surface_id, surface)) = state
                    .workspace()
                    .surfaces()
                    .iter()
                    .find(|(_, surface)| surface.widget() == Some(widget))
                    && surface.visible()
                {
                    dirty.push(state.workspace().surfaces()[&surface_id].area());
                }
            }

            self.diff_surface_snapshots(state, &mut dirty);
            self.diff_overlay_snapshots(state, &mut dirty);
            self.diff_focus(state, &mut dirty);
        }

        self.update_snapshots(state, base);
        Damage {
            full_redraw,
            dirty: coalesce_rects(dirty),
            invalidated,
        }
    }

    /// Dirties surfaces whose geometry, visibility, widget binding, or z-index
    /// changed since the last frame (moves reveal the base/underlying layers,
    /// so both the old and new areas are dirtied), and marks the cached
    /// z-ordered surface list dirty so it is recomputed before composition.
    fn diff_surface_snapshots(&mut self, state: &AppState, dirty: &mut Vec<Rect>) {
        let surfaces = state.workspace().surfaces();
        let mut changed = false;
        for (&id, surface) in surfaces {
            match self.surface_snapshot.get(&id) {
                Some(previous) if *previous == *surface => {}
                _ => {
                    changed = true;
                    if let Some(previous) = self.surface_snapshot.get(&id) {
                        dirty.push(previous.area());
                    }
                    if surface.visible() {
                        dirty.push(surface.area());
                    }
                }
            }
        }
        for (&id, previous) in &self.surface_snapshot {
            if !surfaces.contains_key(&id) {
                changed = true;
                dirty.push(previous.area());
            }
        }
        self.surface_order_dirty |= changed;
    }

    /// Dirties overlays that were shown, hidden, moved, or re-rendered, and
    /// marks the cached z-ordered overlay list dirty for recomputation.
    fn diff_overlay_snapshots(&mut self, state: &AppState, dirty: &mut Vec<Rect>) {
        let overlays = state.workspace().overlays();
        let mut changed = false;
        for (&id, overlay) in overlays {
            match self.overlay_snapshot.get(&id) {
                Some(previous) if *previous == *overlay => {}
                _ => {
                    changed = true;
                    if let Some(previous) = self.overlay_snapshot.get(&id) {
                        dirty.push(previous.area());
                    }
                    if overlay.visible() {
                        dirty.push(overlay.area());
                    }
                }
            }
        }
        for (&id, previous) in &self.overlay_snapshot {
            if !overlays.contains_key(&id) {
                changed = true;
                dirty.push(previous.area());
            }
        }
        self.overlay_order_dirty |= changed;
    }

    /// Dirties both the previously- and newly-focused surface/overlay when
    /// focus moves, since focus changes the affected chrome (borders).
    fn diff_focus(&self, state: &AppState, dirty: &mut Vec<Rect>) {
        let current = state.focus().target();
        if current == self.focus_snapshot {
            return;
        }
        if let Some(target) = self.focus_snapshot {
            dirty.push(focus_area(state, target));
        }
        if let Some(target) = current {
            dirty.push(focus_area(state, target));
        }
    }

    /// Refreshes the structural snapshots for the next frame's damage diff.
    fn update_snapshots(&mut self, state: &AppState, base: &Scene) {
        self.surface_snapshot = state.workspace().surfaces().clone();
        self.overlay_snapshot = state.workspace().overlays().clone();
        self.focus_snapshot = state.focus().target();
        match &mut self.base_snapshot {
            Some(previous) => previous.replace_with(base),
            None => self.base_snapshot = Some(base.clone()),
        }
    }

    fn compose_into(
        &mut self,
        viewport: Rect,
        state: &AppState,
        base: &Scene,
        surface_scenes: &BTreeMap<SurfaceId, Scene>,
        damage: &Damage,
    ) {
        if self
            .composed
            .as_ref()
            .is_none_or(|composed| composed.area() != viewport)
        {
            self.composed = Some(Scene::new(viewport));
            self.composed_reallocations = self.composed_reallocations.saturating_add(1);
        }
        // Refresh the cached z-ordered surface/overlay lists once per frame.
        // They change only when the surface/overlay set, visibility, or z-index
        // changes (flagged by the snapshot diff in `compute_damage`), so a
        // steady frame reuses them without re-sorting or re-fetching.
        if !self.surface_order_computed || self.surface_order_dirty {
            self.surface_order = z_ordered_surfaces(state);
            self.surface_order_computed = true;
            self.surface_order_dirty = false;
            self.z_order_recomputations = self.z_order_recomputations.saturating_add(1);
        }
        if !self.overlay_order_computed || self.overlay_order_dirty {
            self.overlay_order = z_ordered_overlays(state);
            self.overlay_order_computed = true;
            self.overlay_order_dirty = false;
            self.z_order_recomputations = self.z_order_recomputations.saturating_add(1);
        }

        let composed = self
            .composed
            .as_mut()
            .expect("composed frame is initialized");
        if damage.full_redraw {
            composed.reset(viewport);
            blit_frame(composed, viewport, state, base, surface_scenes);
            return;
        }

        // Layers are rebuilt in one pass so graphics/placeholders stay
        // correct regardless of which cells were damaged; cell content is then
        // re-composited only in the dirty regions (the composed buffer retains
        // every other cell from the previous frame).
        composed.clear_layers();
        composed.accumulate_layers(base, viewport);
        for &(_, id) in &self.surface_order {
            let Some(surface) = state.workspace().surfaces().get(&id) else {
                continue;
            };
            let Some(scene) = surface_scenes.get(&id) else {
                continue;
            };
            composed.accumulate_layers(scene, surface.area());
        }
        for &(_, id) in &self.overlay_order {
            let Some(overlay) = state.workspace().overlays().get(&id) else {
                continue;
            };
            let scene = overlay.scene();
            composed.accumulate_layers(&scene, scene.area());
        }

        for region in &damage.dirty {
            // `blit_cells` clips to each source's own area, so a region that
            // does not overlap a surface/overlay is a natural no-op.
            composed.blit_cells(base, *region);
            for &(_, id) in &self.surface_order {
                let Some(scene) = surface_scenes.get(&id) else {
                    continue;
                };
                composed.blit_cells(scene, *region);
            }
            for &(_, id) in &self.overlay_order {
                let Some(overlay) = state.workspace().overlays().get(&id) else {
                    continue;
                };
                let scene = overlay.scene();
                composed.blit_cells(&scene, *region);
            }
        }
    }

    pub fn invalidate(&mut self, area: Rect) {
        if area.width > 0 && area.height > 0 {
            self.pending_invalidations.push(area);
        }
    }

    /// Diffs an externally supplied scene against the retained previous buffer,
    /// updating the previous buffer in place. Convenience path for callers and
    /// tests that build a scene outside the retained compose path.
    pub fn diff(&mut self, current: &Scene) -> FrameDiff {
        diff_against_previous(
            current,
            &mut self.previous,
            &mut self.pending_invalidations,
            &mut self.previous_reallocations,
            &mut self.pool,
        )
    }
}

/// Blits the base, z-ordered visible surfaces, and z-ordered visible overlays
/// into `composed`, sharing the composition logic between the owned `compose`
/// and the retained `compose_into`.
fn blit_frame(
    composed: &mut Scene,
    viewport: Rect,
    state: &AppState,
    base: &Scene,
    surface_scenes: &BTreeMap<SurfaceId, Scene>,
) {
    composed.blit(base, viewport);

    for (_, id) in z_ordered_surfaces(state) {
        let Some(surface) = state.workspace().surfaces().get(&id) else {
            continue;
        };
        let Some(surface_scene) = surface_scenes.get(&id) else {
            continue;
        };
        composed.blit(surface_scene, surface.area());
    }

    for (_, id) in z_ordered_overlays(state) {
        if let Some(overlay) = state.workspace().overlays().get(&id) {
            overlay.render(composed);
        }
    }
}

/// Visible surfaces sorted by z-index (ties broken by id for determinism).
fn z_ordered_surfaces(state: &AppState) -> Vec<(i16, SurfaceId)> {
    let mut surfaces: Vec<_> = state
        .workspace()
        .surfaces()
        .values()
        .filter(|surface| surface.visible())
        .map(|surface| (surface.z_index(), surface.id()))
        .collect();
    surfaces.sort_unstable();
    surfaces
}

/// Visible overlays sorted by z-index (ties broken by id for determinism).
fn z_ordered_overlays(state: &AppState) -> Vec<(i16, OverlayId)> {
    let mut overlays: Vec<_> = state
        .workspace()
        .overlays()
        .values()
        .filter(|overlay| overlay.visible())
        .map(|overlay| (overlay.z_index(), overlay.id()))
        .collect();
    overlays.sort_unstable();
    overlays
}

/// The area a focus target occupies, used to dirty the chrome when focus moves.
fn focus_area(state: &AppState, target: FocusTarget) -> Rect {
    match target {
        FocusTarget::Surface(id) => state
            .workspace()
            .surfaces()
            .get(&id)
            .map_or(Rect::new(0, 0, 0, 0), |surface| surface.area()),
        FocusTarget::Overlay(id) => state
            .workspace()
            .overlays()
            .get(&id)
            .map_or(Rect::new(0, 0, 0, 0), |overlay| overlay.area()),
    }
}

/// Bounding rectangle of the cells that differ between the cached base shell
/// and the freshly rendered one.
fn base_diff_rect(previous: &Scene, current: &Scene, viewport: Rect) -> Rect {
    let mut min_x = u16::MAX;
    let mut min_y = u16::MAX;
    let mut max_x = 0_u16;
    let mut max_y = 0_u16;
    let mut any = false;
    for (index, cell) in current.cells().iter().enumerate() {
        if previous.cells().get(index).copied() == Some(*cell) {
            continue;
        }
        let x = viewport
            .x
            .saturating_add((index % viewport.width as usize) as u16);
        let y = viewport
            .y
            .saturating_add((index / viewport.width as usize) as u16);
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
        any = true;
    }
    if !any {
        return Rect::new(0, 0, 0, 0);
    }
    Rect::new(
        min_x,
        min_y,
        max_x.saturating_sub(min_x).saturating_add(1),
        max_y.saturating_sub(min_y).saturating_add(1),
    )
}

/// Merges overlapping rectangles into a minimal disjoint set, sorted by
/// (y, x), so the dirty-region scan is duplicate-free and roughly row-major.
fn coalesce_rects(rects: Vec<Rect>) -> Vec<Rect> {
    let mut rects: Vec<Rect> = rects
        .into_iter()
        .filter(|rect| rect.width > 0 && rect.height > 0)
        .collect();
    rects.sort_by_key(|rect| (rect.y, rect.x));
    let mut merged: Vec<Rect> = Vec::new();
    for rect in rects {
        if let Some(last) = merged.last_mut()
            && rects_overlap(*last, rect)
        {
            *last = union_rects(*last, rect);
        } else {
            merged.push(rect);
        }
    }
    merged
}

fn rects_overlap(first: Rect, second: Rect) -> bool {
    intersect(first, second).is_some()
}

fn union_rects(first: Rect, second: Rect) -> Rect {
    let x = first.x.min(second.x);
    let y = first.y.min(second.y);
    let right = first
        .x
        .saturating_add(first.width)
        .max(second.x.saturating_add(second.width));
    let bottom = first
        .y
        .saturating_add(first.height)
        .max(second.y.saturating_add(second.height));
    Rect::new(x, y, right.saturating_sub(x), bottom.saturating_sub(y))
}

/// Computes a frame diff between `current` and the retained `previous` buffer,
/// then updates `previous` in place (reusing its cell allocation when the
/// viewport is unchanged). Convenience path for callers and tests that build a
/// scene outside the retained compose path: it scans the whole viewport.
fn diff_against_previous(
    current: &Scene,
    previous: &mut Option<Scene>,
    invalidations: &mut Vec<Rect>,
    reallocations: &mut u64,
    pool: &mut FrameBufferPool,
) -> FrameDiff {
    let viewport = current.area();
    let full_redraw = previous
        .as_ref()
        .is_none_or(|previous| previous.area() != viewport);
    let invalidated: Vec<Rect> = invalidations
        .drain(..)
        .filter_map(|area| intersect(area, viewport))
        .collect();
    build_diff(
        current,
        previous,
        reallocations,
        pool,
        full_redraw,
        &invalidated,
        &[viewport],
    )
}

/// The retained-path diff: scans only `damage.dirty` (the whole viewport when
/// `damage.full_redraw`) instead of every cell, and reports `damage.invalidated`
/// as the forced regions.
fn diff_regions(
    current: &Scene,
    previous: &mut Option<Scene>,
    reallocations: &mut u64,
    pool: &mut FrameBufferPool,
    damage: &Damage,
) -> FrameDiff {
    let scan: Vec<Rect> = if damage.full_redraw {
        vec![current.area()]
    } else {
        damage.dirty.clone()
    };
    build_diff(
        current,
        previous,
        reallocations,
        pool,
        damage.full_redraw,
        &damage.invalidated,
        &scan,
    )
}

/// Shared diff core: layer/cursor comparison plus a cell scan limited to
/// `scan`, then an in-place update of the retained previous buffer. The
/// change/span/layer scratch vectors are taken from `pool` and returned inside
/// the `FrameDiff`; `Compositor::recycle` puts them back for the next frame.
fn build_diff(
    current: &Scene,
    previous: &mut Option<Scene>,
    reallocations: &mut u64,
    pool: &mut FrameBufferPool,
    full_redraw: bool,
    invalidated: &[Rect],
    scan: &[Rect],
) -> FrameDiff {
    let viewport = current.area();
    let previous_scene = previous.as_ref();
    let cursor_changed =
        full_redraw || previous_scene.is_none_or(|previous| previous.cursor() != current.cursor());
    let graphics_changed = full_redraw
        || previous_scene.is_none_or(|previous| {
            previous.image_layers() != current.image_layers()
                || previous.placeholder_layers() != current.placeholder_layers()
        });
    #[cfg(feature = "sixel")]
    let sixel_changed = full_redraw
        || previous_scene.is_none_or(|previous| previous.sixel_layers() != current.sixel_layers());

    let (mut changes, changes_cap) = take_scratch(&mut pool.changes);
    changes.clear();
    let (mut graphics, graphics_cap) = take_scratch(&mut pool.graphics);
    graphics.clear();
    let (mut visible_graphics, visible_graphics_cap) = take_scratch(&mut pool.visible_graphics);
    visible_graphics.clear();
    let (mut removed_graphics, removed_graphics_cap) = take_scratch(&mut pool.removed_graphics);
    removed_graphics.clear();
    let (mut placeholders, placeholders_cap) = take_scratch(&mut pool.placeholders);
    placeholders.clear();
    let (mut visible_placeholders, visible_placeholders_cap) =
        take_scratch(&mut pool.visible_placeholders);
    visible_placeholders.clear();
    let (mut removed_placeholders, removed_placeholders_cap) =
        take_scratch(&mut pool.removed_placeholders);
    removed_placeholders.clear();
    #[cfg(feature = "sixel")]
    let (mut sixel, sixel_cap) = take_scratch(&mut pool.sixel);
    #[cfg(feature = "sixel")]
    sixel.clear();

    if graphics_changed {
        graphics.extend_from_slice(current.image_layers());
    }
    visible_graphics.extend_from_slice(current.image_layers());
    // Keyed-set diffs replace the previous linear/quadratic removal scans: map
    // the current layer set by a stable key (images by resource + placement
    // key, placeholders by their full identity) so removal detection is
    // O(visible) and skipped entirely when the layers are unchanged.
    if graphics_changed {
        let current_graphics = current
            .image_layers()
            .iter()
            .map(|image| ((image.resource(), image.placement().key()), image))
            .collect::<BTreeMap<_, _>>();
        removed_graphics.extend(
            previous_scene
                .into_iter()
                .flat_map(|previous| previous.image_layers())
                .filter(|image| {
                    current_graphics
                        .get(&(image.resource(), image.placement().key()))
                        .is_none_or(|current| *current != *image)
                })
                .cloned(),
        );
        let current_placeholders: HashSet<_> = current
            .placeholder_layers()
            .iter()
            .map(|placeholder| {
                (
                    placeholder.resource(),
                    placeholder.area(),
                    placeholder.z_index(),
                )
            })
            .collect();
        removed_placeholders.extend(
            previous_scene
                .into_iter()
                .flat_map(|previous| previous.placeholder_layers())
                .filter(|placeholder| {
                    !current_placeholders.contains(&(
                        placeholder.resource(),
                        placeholder.area(),
                        placeholder.z_index(),
                    ))
                })
                .copied(),
        );
    }
    if graphics_changed {
        placeholders.extend_from_slice(current.placeholder_layers());
    }
    visible_placeholders.extend_from_slice(current.placeholder_layers());
    #[cfg(feature = "sixel")]
    if sixel_changed {
        sixel.extend_from_slice(current.sixel_layers());
    }

    for region in scan {
        for y in region.y..region.y.saturating_add(region.height) {
            for x in region.x..region.x.saturating_add(region.width) {
                let Some(cell) = current.cell_at(x, y).copied() else {
                    continue;
                };
                let forced = invalidated.iter().any(|area| contains(*area, x, y));
                let changed = full_redraw
                    || forced
                    || previous_scene
                        .and_then(|previous| previous.cell_at(x, y))
                        .copied()
                        != Some(cell);
                if changed {
                    changes.push(CellChange { x, y, cell });
                }
            }
        }
    }

    // Retain the previous generation in place: reuse the existing cell buffer
    // when the viewport is unchanged (a memcpy, not an allocation); allocate
    // only on the first frame or a resize.
    match previous {
        Some(previous) => {
            if previous.cells().len() != current.cells().len() {
                *reallocations = reallocations.saturating_add(1);
            }
            previous.replace_with(current);
        }
        None => {
            *previous = Some(current.clone());
            *reallocations = reallocations.saturating_add(1);
        }
    }

    pool.styles.clear();
    let (mut spans, spans_cap) = take_scratch(&mut pool.spans);
    group_changes(&changes, &mut pool.styles, &mut spans);
    pool.last_frame_styles = pool.styles.distinct_styles();

    // Count scratch reallocations: a vector reallocated this frame only when
    // its capacity grew while being filled, so steady-state recycling keeps
    // the counter flat.
    pool.scratch_reallocations = pool
        .scratch_reallocations
        .saturating_add(u64::from(changes.capacity() > changes_cap))
        .saturating_add(u64::from(spans.capacity() > spans_cap))
        .saturating_add(u64::from(graphics.capacity() > graphics_cap))
        .saturating_add(u64::from(
            visible_graphics.capacity() > visible_graphics_cap,
        ))
        .saturating_add(u64::from(
            removed_graphics.capacity() > removed_graphics_cap,
        ))
        .saturating_add(u64::from(placeholders.capacity() > placeholders_cap))
        .saturating_add(u64::from(
            visible_placeholders.capacity() > visible_placeholders_cap,
        ))
        .saturating_add(u64::from(
            removed_placeholders.capacity() > removed_placeholders_cap,
        ));
    #[cfg(feature = "sixel")]
    {
        pool.scratch_reallocations = pool
            .scratch_reallocations
            .saturating_add(u64::from(sixel.capacity() > sixel_cap));
    }

    FrameDiff {
        viewport,
        full_redraw,
        invalidated: invalidated.to_vec(),
        changes,
        spans,
        graphics,
        visible_graphics,
        removed_graphics,
        placeholders,
        visible_placeholders,
        removed_placeholders,
        cursor: current.cursor(),
        cursor_changed,
        #[cfg(feature = "sixel")]
        sixel,
    }
}

/// Groups adjacent same-style changes into row spans, keying the style
/// comparison off the interned `StyleId` handle rather than the expanded
/// 9-field `CellStyle` struct. `spans` is a pooled scratch vector (cleared in
/// place and returned full).
fn group_changes(changes: &[CellChange], interner: &mut StyleInterner, spans: &mut Vec<CellSpan>) {
    spans.clear();
    for change in changes {
        let style_id = interner.intern(change.cell.style);
        let extends_previous = spans.last().is_some_and(|span: &CellSpan| {
            span.y == change.y
                && span.x as u32 + span.cells.len() as u32 == change.x as u32
                && span.style_id == style_id
        });
        if extends_previous {
            if let Some(span) = spans.last_mut() {
                span.cells.push(change.cell);
            }
        } else {
            spans.push(CellSpan {
                x: change.x,
                y: change.y,
                style_id,
                cells: vec![change.cell],
            });
        }
    }
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
        BackendCapabilities, CellStyle, Color, Command, FocusCommand, Overlay, OverlayCommand,
        OverlayId, OverlayPrimitive, Surface, SurfaceCommand, SurfaceId,
    };

    fn terminal_style() -> CellStyle {
        CellStyle::new(Color::rgb(200, 200, 200), Color::rgb(10, 10, 10))
    }

    fn two_surface_state() -> (crate::AppState, Rect) {
        let viewport = Rect::new(0, 0, 8, 2);
        let mut state = crate::AppState::new(capabilities());
        let left = Surface::new(SurfaceId::new(1), Rect::new(0, 0, 4, 1))
            .with_z_index(0)
            .with_widget(WidgetId::new(1));
        let right = Surface::new(SurfaceId::new(2), Rect::new(4, 0, 4, 1))
            .with_z_index(0)
            .with_widget(WidgetId::new(2));
        state
            .dispatch(Command::Surface(SurfaceCommand::Add(left)))
            .unwrap();
        state
            .dispatch(Command::Surface(SurfaceCommand::Add(right)))
            .unwrap();
        (state, viewport)
    }

    fn capabilities() -> BackendCapabilities {
        BackendCapabilities {
            truecolor: true,
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: false,
            kitty_unicode_placeholders: false,
            graphics_source: crate::backend::GraphicsCapabilitySource::Unavailable,
            graphics_confidence: crate::backend::GraphicsCapabilityConfidence::Rejected,
            kitty_passthrough: false,
            kitty_text_fallback: false,
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
    fn cursor_only_changes_produce_a_non_empty_diff_and_are_not_repeated() {
        let viewport = Rect::new(0, 0, 4, 2);
        let mut compositor = Compositor::new();
        let mut scene = Scene::new(viewport);
        scene.set_cursor(1, 1, true);

        let first = compositor.diff(&scene);
        assert!(first.full_redraw());

        let mut moved = scene.clone();
        moved.set_cursor(2, 1, true);
        let diff = compositor.diff(&moved);
        assert!(!diff.is_empty());
        assert!(diff.cursor_changed());
        assert_eq!(diff.cursor(), Some(SceneCursor::new(2, 1, true)));
        assert!(diff.changes().is_empty());

        let unchanged = compositor.diff(&moved);
        assert!(unchanged.is_empty());
        assert!(!unchanged.cursor_changed());
    }

    #[test]
    fn cursor_visibility_toggles_produce_a_cursor_only_diff() {
        let viewport = Rect::new(0, 0, 4, 2);
        let mut compositor = Compositor::new();
        let mut scene = Scene::new(viewport);
        scene.set_cursor(1, 1, true);
        compositor.diff(&scene);

        let mut hidden = scene.clone();
        hidden.set_cursor(1, 1, false);
        let diff = compositor.diff(&hidden);
        assert!(!diff.is_empty());
        assert!(diff.cursor_changed());
        assert_eq!(diff.cursor(), Some(SceneCursor::new(1, 1, false)));
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
    fn placeholder_layers_are_owned_by_frame_diffs_and_removed_with_the_scene() {
        let resource = crate::GraphicsResourceId::new(crate::SessionId::new(4), 7);
        let mut first_scene = Scene::new(Rect::new(0, 0, 4, 2));
        first_scene.add_placeholder_layer(crate::GraphicsPlaceholderLayer::new(
            resource,
            Rect::new(1, 0, 2, 1),
            3,
        ));
        let mut compositor = Compositor::new();
        let first = compositor.diff(&first_scene);
        assert_eq!(first.placeholders().len(), 1);
        assert_eq!(first.visible_placeholders().len(), 1);
        assert!(first.removed_placeholders().is_empty());

        let second_scene = Scene::new(first_scene.area());
        let second = compositor.diff(&second_scene);
        assert!(second.placeholders().is_empty());
        assert_eq!(second.removed_placeholders(), first.visible_placeholders());
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

    #[test]
    fn compose_and_diff_matches_the_owned_compose_path() {
        let viewport = Rect::new(0, 0, 8, 4);
        let mut state = crate::AppState::new(capabilities());
        let surface = Surface::new(SurfaceId::new(1), Rect::new(1, 1, 4, 2)).with_z_index(0);
        state
            .dispatch(Command::Surface(SurfaceCommand::Add(surface)))
            .unwrap();
        let mut surface_scene = Scene::new(surface.area());
        surface_scene.text(surface.area().x, surface.area().y, "LLLL", style());
        let scenes = BTreeMap::from([(surface.id(), surface_scene)]);
        let mut base = Scene::new(viewport);
        base.text(0, 0, "base", style());

        let owned = Compositor::new().compose(viewport, &state, &base, &scenes);
        let mut retained = Compositor::new();
        retained.compose_and_diff(viewport, &state, &base, &scenes, &[]);
        let frame = retained.frame();
        assert_eq!(owned.cells(), frame.cells());
        assert_eq!(owned.cursor(), frame.cursor());
    }

    #[test]
    fn retained_buffers_are_reused_across_steady_state_frames() {
        let viewport = Rect::new(0, 0, 4, 2);
        let state = crate::AppState::new(capabilities());
        let base = Scene::new(viewport);
        let scenes = BTreeMap::new();
        let mut compositor = Compositor::new();

        compositor.compose_and_diff(viewport, &state, &base, &scenes, &[]);
        assert_eq!(compositor.retained_buffer_reallocations(), 2);

        for _ in 0..5 {
            compositor.compose_and_diff(viewport, &state, &base, &scenes, &[]);
        }
        assert_eq!(
            compositor.retained_buffer_reallocations(),
            2,
            "steady-state frames must reuse the retained buffers"
        );
    }

    #[test]
    fn resizing_reallocates_each_retained_buffer_once() {
        let state = crate::AppState::new(capabilities());
        let base = Scene::new(Rect::new(0, 0, 4, 2));
        let scenes = BTreeMap::new();
        let mut compositor = Compositor::new();

        compositor.compose_and_diff(Rect::new(0, 0, 4, 2), &state, &base, &scenes, &[]);
        assert_eq!(compositor.retained_buffer_reallocations(), 2);

        compositor.compose_and_diff(Rect::new(0, 0, 5, 2), &state, &base, &scenes, &[]);
        assert_eq!(
            compositor.retained_buffer_reallocations(),
            4,
            "a resize reallocates the composed and previous buffers once each"
        );
    }

    #[test]
    fn single_surface_redraw_dirties_only_its_region() {
        let (state, viewport) = two_surface_state();
        let base = Scene::new(viewport);
        let mut compositor = Compositor::new();

        let mut left = Scene::new(Rect::new(0, 0, 4, 1));
        left.text(0, 0, "AAAA", terminal_style());
        let mut right = Scene::new(Rect::new(4, 0, 4, 1));
        right.text(4, 0, "BBBB", terminal_style());
        let scenes = BTreeMap::from([
            (SurfaceId::new(1), left),
            (SurfaceId::new(2), right.clone()),
        ]);
        compositor.compose_and_diff(viewport, &state, &base, &scenes, &[]);

        let mut left_changed = Scene::new(Rect::new(0, 0, 4, 1));
        left_changed.text(0, 0, "XXXX", terminal_style());
        let scenes = BTreeMap::from([
            (SurfaceId::new(1), left_changed),
            (SurfaceId::new(2), right),
        ]);
        let diff =
            compositor.compose_and_diff(viewport, &state, &base, &scenes, &[WidgetId::new(1)]);

        assert!(!diff.full_redraw());
        assert_eq!(diff.changes().len(), 4);
        assert!(
            diff.changes().iter().all(|change| change.x < 4),
            "changes must be confined to the redrawn surface: {changes:?}",
            changes = diff.changes()
        );
        // The composed frame keeps the untouched right surface intact.
        assert_eq!(compositor.frame().cell_at(5, 0).unwrap().symbol, 'B');
    }

    #[test]
    fn focus_change_dirties_both_the_old_and_new_surface() {
        let (mut state, viewport) = two_surface_state();
        let base = Scene::new(viewport);
        let mut compositor = Compositor::new();

        state
            .dispatch(Command::Focus(FocusCommand::Surface(SurfaceId::new(1))))
            .unwrap();
        let scenes = |left: &str, right: &str| {
            let mut left_scene = Scene::new(Rect::new(0, 0, 4, 1));
            left_scene.text(0, 0, left, terminal_style());
            let mut right_scene = Scene::new(Rect::new(4, 0, 4, 1));
            right_scene.text(4, 0, right, terminal_style());
            BTreeMap::from([
                (SurfaceId::new(1), left_scene),
                (SurfaceId::new(2), right_scene),
            ])
        };
        compositor.compose_and_diff(viewport, &state, &base, &scenes("AAAA", "BBBB"), &[]);

        state
            .dispatch(Command::Focus(FocusCommand::Surface(SurfaceId::new(2))))
            .unwrap();
        // Both scenes re-render with their new chrome (here, different text).
        let diff =
            compositor.compose_and_diff(viewport, &state, &base, &scenes("aaaa", "bbbb"), &[]);

        assert!(!diff.full_redraw());
        assert!(
            diff.changes().iter().any(|change| change.x < 4),
            "the previously-focused surface must be redrawn"
        );
        assert!(
            diff.changes().iter().any(|change| change.x >= 4),
            "the newly-focused surface must be redrawn"
        );
    }

    #[test]
    fn moved_surface_dirties_both_its_old_and_new_area() {
        let (mut state, viewport) = two_surface_state();
        let base = Scene::new(viewport);
        let mut compositor = Compositor::new();

        let mut left = Scene::new(Rect::new(0, 0, 4, 1));
        left.text(0, 0, "AAAA", terminal_style());
        let mut right = Scene::new(Rect::new(4, 0, 4, 1));
        right.text(4, 0, "BBBB", terminal_style());
        let scenes = BTreeMap::from([
            (SurfaceId::new(1), left.clone()),
            (SurfaceId::new(2), right.clone()),
        ]);
        compositor.compose_and_diff(viewport, &state, &base, &scenes, &[]);

        // Move the left surface to the second row; its old cells must revert
        // to the base and the new cells must be drawn.
        state
            .dispatch(Command::Surface(SurfaceCommand::SetArea {
                id: SurfaceId::new(1),
                area: Rect::new(0, 1, 2, 1),
            }))
            .unwrap();
        let mut moved = Scene::new(Rect::new(0, 1, 2, 1));
        moved.text(0, 1, "AA", terminal_style());
        let scenes = BTreeMap::from([(SurfaceId::new(1), moved), (SurfaceId::new(2), right)]);
        let diff = compositor.compose_and_diff(viewport, &state, &base, &scenes, &[]);

        assert!(!diff.full_redraw());
        // The old area reverted to the blank base, and the new area now shows
        // the moved surface.
        assert_eq!(compositor.frame().cell_at(0, 0).unwrap().symbol, ' ');
        assert_eq!(compositor.frame().cell_at(0, 1).unwrap().symbol, 'A');
        assert!(
            diff.changes().iter().any(|change| change.y == 0),
            "the vacated area must be re-emitted"
        );
        assert!(
            diff.changes().iter().any(|change| change.y == 1),
            "the moved-into area must be re-emitted"
        );
    }

    #[test]
    fn incremental_frames_match_the_full_recompose_for_cells() {
        let (state, viewport) = two_surface_state();
        let base = Scene::new(viewport);
        let mut compositor = Compositor::new();

        let mut left = Scene::new(Rect::new(0, 0, 4, 1));
        left.text(0, 0, "AAAA", terminal_style());
        let mut right = Scene::new(Rect::new(4, 0, 4, 1));
        right.text(4, 0, "BBBB", terminal_style());
        let scenes = BTreeMap::from([
            (SurfaceId::new(1), left),
            (SurfaceId::new(2), right.clone()),
        ]);
        compositor.compose_and_diff(viewport, &state, &base, &scenes, &[]);

        let mut left_changed = Scene::new(Rect::new(0, 0, 4, 1));
        left_changed.text(0, 0, "WIDE", terminal_style());
        let scenes = BTreeMap::from([
            (SurfaceId::new(1), left_changed.clone()),
            (SurfaceId::new(2), right),
        ]);
        compositor.compose_and_diff(viewport, &state, &base, &scenes, &[WidgetId::new(1)]);

        // The retained frame must byte-match a fresh full compose of the same
        // scenes.
        let full = Compositor::new().compose(viewport, &state, &base, &scenes);
        assert_eq!(compositor.frame().cells(), full.cells());
    }

    #[test]
    fn style_interner_deduplicates_repeated_styles() {
        let mut interner = StyleInterner::default();
        let style = terminal_style();
        let bold = style.bold();

        let first = interner.intern(style);
        let again = interner.intern(style);
        let second = interner.intern(bold);

        assert_eq!(first, again, "repeated styles must reuse the same handle");
        assert_ne!(first, second, "distinct styles must get distinct handles");
        assert_eq!(interner.distinct_styles(), 2);
    }

    #[test]
    fn compose_and_diff_reports_only_distinct_styles_for_the_frame() {
        let (state, viewport) = two_surface_state();
        let base = Scene::new(viewport);
        let mut compositor = Compositor::new();

        let style = terminal_style();
        let mut left = Scene::new(Rect::new(0, 0, 4, 1));
        left.text(0, 0, "AAAA", style);
        let mut right = Scene::new(Rect::new(4, 0, 4, 1));
        right.text(4, 0, "BBBB", style);
        let scenes = BTreeMap::from([(SurfaceId::new(1), left), (SurfaceId::new(2), right)]);
        compositor.compose_and_diff(viewport, &state, &base, &scenes, &[]);

        // Two distinct styles across the whole frame: the shared surface style
        // (interred once for all eight surface cells) and the base's default
        // blank style. Interning stores each exactly once.
        assert_eq!(compositor.last_frame_distinct_styles(), 2);
        assert!(
            compositor.last_frame_distinct_styles()
                < viewport.width as usize * viewport.height as usize
        );
    }

    #[test]
    fn steady_state_frames_with_recycle_do_not_reallocate_scratch() {
        let (state, viewport) = two_surface_state();
        let base = Scene::new(viewport);
        let mut compositor = Compositor::new();

        let scenes = |left: &str, right: &str| {
            let mut left_scene = Scene::new(Rect::new(0, 0, 4, 1));
            left_scene.text(0, 0, left, terminal_style());
            let mut right_scene = Scene::new(Rect::new(4, 0, 4, 1));
            right_scene.text(4, 0, right, terminal_style());
            BTreeMap::from([
                (SurfaceId::new(1), left_scene),
                (SurfaceId::new(2), right_scene),
            ])
        };

        let diff =
            compositor.compose_and_diff(viewport, &state, &base, &scenes("AAAA", "BBBB"), &[]);
        compositor.recycle(diff);
        let after_first = compositor.scratch_reallocations();
        assert!(
            after_first > 0,
            "the first frame must allocate its scratch vectors"
        );

        for _ in 0..5 {
            let diff =
                compositor.compose_and_diff(viewport, &state, &base, &scenes("AAAA", "BBBB"), &[]);
            compositor.recycle(diff);
        }
        assert_eq!(
            compositor.scratch_reallocations(),
            after_first,
            "steady-state frames must reuse the pooled scratch vectors"
        );
    }

    #[test]
    fn span_grouping_is_unchanged_when_keyed_off_interned_handles() {
        let viewport = Rect::new(0, 0, 6, 1);
        let mut compositor = Compositor::new();
        let first_scene = Scene::new(viewport);
        compositor.diff(&first_scene);

        let first_style = style();
        let second_style = CellStyle::new(Color::rgb(255, 0, 0), Color::rgb(0, 0, 0));
        let mut second_scene = first_scene.clone();
        second_scene.set(1, 0, 'a', first_style);
        second_scene.set(2, 0, 'b', first_style);
        second_scene.set(3, 0, 'c', second_style);
        second_scene.set(4, 0, 'd', second_style);
        let diff = compositor.diff(&second_scene);

        // Same grouping as the full-struct comparison: two spans, two cells each.
        assert_eq!(diff.spans().len(), 2);
        assert_eq!(diff.spans()[0].cells().len(), 2);
        assert_eq!(diff.spans()[1].cells().len(), 2);
        assert_eq!(diff.spans()[0].cells()[0].style, first_style);
        assert_eq!(diff.spans()[1].cells()[0].style, second_style);
    }

    #[test]
    fn z_ordered_lists_are_recomputed_only_when_structure_changes() {
        let (mut state, viewport) = two_surface_state();
        let base = Scene::new(viewport);
        let mut compositor = Compositor::new();

        let scenes = |left: &str, right: &str| {
            let mut left_scene = Scene::new(Rect::new(0, 0, 4, 1));
            left_scene.text(0, 0, left, terminal_style());
            let mut right_scene = Scene::new(Rect::new(4, 0, 4, 1));
            right_scene.text(4, 0, right, terminal_style());
            BTreeMap::from([
                (SurfaceId::new(1), left_scene),
                (SurfaceId::new(2), right_scene),
            ])
        };

        compositor.compose_and_diff(viewport, &state, &base, &scenes("AAAA", "BBBB"), &[]);
        let after_first = compositor.z_order_recomputations();
        assert!(
            after_first > 0,
            "the first frame must populate the cached z-ordered lists"
        );

        for _ in 0..5 {
            compositor.compose_and_diff(viewport, &state, &base, &scenes("AAAA", "BBBB"), &[]);
        }
        assert_eq!(
            compositor.z_order_recomputations(),
            after_first,
            "steady-state frames must reuse the cached z-ordered lists"
        );

        // Moving a surface changes the layout and forces a recompute.
        state
            .dispatch(Command::Surface(SurfaceCommand::SetArea {
                id: SurfaceId::new(1),
                area: Rect::new(0, 1, 2, 1),
            }))
            .unwrap();
        let mut moved = Scene::new(Rect::new(0, 1, 2, 1));
        moved.text(0, 1, "AA", terminal_style());
        let mut right = Scene::new(Rect::new(4, 0, 4, 1));
        right.text(4, 0, "BBBB", terminal_style());
        let scenes = BTreeMap::from([(SurfaceId::new(1), moved), (SurfaceId::new(2), right)]);
        compositor.compose_and_diff(viewport, &state, &base, &scenes, &[]);
        assert!(
            compositor.z_order_recomputations() > after_first,
            "a surface geometry change must recompute the cached list"
        );
    }

    #[test]
    fn keyed_graphics_diff_removes_only_the_absent_placement() {
        let mut store = crate::SessionGraphicsStore::new(crate::SessionId::new(1));
        store.apply_kitty_command(b"a=T,f=24,i=1", b"AQID").unwrap();
        store.apply_kitty_command(b"a=p,i=1,x=0,y=0", b"").unwrap();
        store.apply_kitty_command(b"a=p,i=1,x=2,y=0", b"").unwrap();

        let area = Rect::new(0, 0, 4, 2);
        let submissions = store.visible_submissions(area);
        assert!(
            submissions.len() >= 2,
            "expected multiple placements of the same image, got {}",
            submissions.len()
        );

        let mut first_scene = Scene::new(area);
        for submission in &submissions {
            first_scene.add_image_layer(submission.clone());
        }
        let mut compositor = Compositor::new();
        compositor.diff(&first_scene);

        // Keep every placement except the last; the absent one (and only it)
        // must be reported as removed. The old image-id-only key would collapse
        // the shared image id and falsely remove the other kept placements.
        let removed_key = submissions
            .last()
            .expect("submissions are non-empty")
            .placement()
            .key();
        let mut second_scene = Scene::new(area);
        for submission in &submissions[..submissions.len() - 1] {
            second_scene.add_image_layer(submission.clone());
        }
        let diff = compositor.diff(&second_scene);

        let removed_keys: Vec<_> = diff
            .removed_graphics()
            .iter()
            .map(|submission| submission.placement().key())
            .collect();
        assert_eq!(
            removed_keys,
            vec![removed_key],
            "exactly the absent placement is removed"
        );
    }
}
