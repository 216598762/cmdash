//! cmdash-side dashcompositor adapter for kitty graphics coming
//! from nested PTY children.
//!
//! ## Design
//!
//! - One [`GraphicsState`] owns the command's [`dashcompositor::LayerStack`].
//! - Each pane keeps `(pane_layer_id, kitty_image_id) -> LayerId`
//!   in a flat `HashMap` keyed by a stable
//!   [`cmdash_pty::PaneLayerId`] (1:1 with the pane — AGENTS.md
//!   §"Hard rule: one layer per instance").
//! - `GraphicsState::on_kitty` dispatches each
//!   [`KittyGraphicCmd`] variant:
//!   - `Load`: decode the RGBA payload via [`image::load_from_memory`]
//!     and call `Self::push_image` to register the freshly-pushed
//!     [`dashcompositor::ImageLayer`].
//!   - `Place`: re-create the layer at the new pixel position
//!     while preserving the cached RGBA. (dashcompositor's
//!     [`dashcompositor::Layer`] trait has no `set_position`, so
//!     a remove-then-push is the documented v1 path; the pane-side
//!     [`PaneLayerId`] stays stable across this operation.)
//!   - `Delete`: remove the cached entry and the layer.
//!   - `Control`: no-op (matches vte-via-cmdash-pty semantics).
//! - [`GraphicsState::render_and_write`] composites the stack
//!   into a [`dashcompositor::FrameBuffer`] sized from
//!   [`Metrics`] (default `8x16` per cell) and emits through
//!   `dashcompositor::encode_passthrough_to_writer`.
//! - [`GraphicsState::close_pane`] tears down every layer that
//!   came from a given pane (AGENTS.md §"MUST NOT" — bindings
//!   outliving their pane).

use std::collections::HashMap;
use std::io::Write;

use cmdash_pty::{KittyGraphicCmd, PaneLayerId};
use dashcompositor::{
    encode_passthrough_to_writer, Compositor, CpuCompositor, FrameBuffer, ImageLayer, LayerId,
    LayerStack,
};
use thiserror::Error;
use tracing::warn;

/// Cell-pixel metrics used when converting a pane's text rect to
/// the underlying pixel framebuffer size. v1 sticks to the
/// common 8x16 default; per-terminal overrides are a v2 topic.
///
/// Fields are private; construct via [`Self::new`] or
/// [`Default::default`]. The ctor enforces `cell_w > 0 &&
/// cell_h > 0` so [`GraphicsState::render_and_write`] cannot
/// produce a zero-area framebuffer component.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    cell_w: u32,
    cell_h: u32,
}

impl Metrics {
    /// Construct [`Metrics`] with non-zero cell dimensions.
    /// `cell_w > 0 && cell_h > 0` is enforced by `assert!`
    /// (matching the ctor invariant on
    /// [`crate::graphics::GraphicsState`]). The exact panic
    /// phrase `"cell_w and cell_h must be non-zero"` is
    /// consumed by the `metrics_new_panics_on_zero_*`
    /// regression tests in
    ///`internal_sanity_tests` .
    ///
    /// Not `const fn` -- no const-eval consumer exists today
    /// (`Default::default()` is `fn`, not `const fn`;
    /// [`crate::graphics::GraphicsState::new`] takes `Metrics`
    /// by value in a non-const context), and dropping `const`
    /// lets the panic phrase stay stable for debug-time
    /// correlation.
    pub fn new(cell_w: u32, cell_h: u32) -> Self {
        assert!(
            cell_w > 0 && cell_h > 0,
            "Metrics::new: cell_w and cell_h must be non-zero, got {}x{}",
            cell_w,
            cell_h,
        );
        Self { cell_w, cell_h }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new(8, 16)
    }
}

#[derive(Debug, Error)]
pub enum GraphicsError {
    #[error("image decode failed: {0}")]
    ImageDecode(#[from] image::ImageError),
    #[error("graphics dispatch failed: {0}")]
    Dispatch(String),
}

/// Per-(pane, kitty-image-id) bookkeeping: one dashcompositor
/// layer slot plus the cached RGBA so that `Place` commands can
/// rebuild an [`ImageLayer`] without re-decoding the payload.
#[derive(Debug, Clone)]
struct ImageEntry {
    layer_id: LayerId,
    rgba: image::RgbaImage,
}

/// Per-pane graphics state. Holds a shared
/// [`dashcompositor::LayerStack`], per-pane image maps, and the
/// cell-pixel metrics used for framebuffer sizing.
pub struct GraphicsState {
    /// dashcompositor layer stack -- private; mutating is exposed
    /// through `push_image` / `close_pane` / `render_and_write`.
    stack: LayerStack,
    /// Cell-pixel metrics for framebuffer sizing -- private;
    /// passed in via `Self::new` and read inside `render_and_write`.
    metrics: Metrics,
    /// Total terminal size in cells; private. Set once in
    /// `Self::new`, which enforces `cells.0 > 0 && cells.1 > 0`
    /// via `assert!` so a downstream `render_and_write` cannot
    /// produce a zero-size framebuffer. v1 has a single tab with
    /// one root layout rect, so resizing isn't a `set_cells`
    /// path -- constructing a fresh `GraphicsState` is the v1
    /// contract; v2 may add `pub fn set_cells` with the same
    /// assert guard.
    cells: (u16, u16),
    /// Bookkeeping for per-(pane, `kitty_image_id`) layers.
    /// Invariant: for every `pane`, every `kitty_id` recorded in
    /// `pane_images[pane]` is also the second component of a key
    /// in `images`. Maintained by `push_image`, the `on_kitty`
    /// `Delete` path, and `close_pane`. The cross-key invariant
    /// is locked by `pane_images_subset_of_images_keys_after_*`
    /// below -- a future regression that mutated one map without
    /// the other would not survive that check.
    images: HashMap<(PaneLayerId, u32), ImageEntry>,
    pane_images: HashMap<PaneLayerId, Vec<u32>>,
}

impl GraphicsState {
    /// Construct a [`GraphicsState`] with cell-pixel metrics and
    /// a non-zero cell dimension. `cells.0 > 0 && cells.1 > 0`
    /// is enforced by `assert!` so a downstream
    /// [`Self::render_and_write`] cannot produce a zero-size
    /// framebuffer. The exactly-string `"cells must be non-zero"`
    /// in the panic message is consumed by the
    /// `graphics_state_new_panics_on_zero_*` regression tests.
    pub fn new(metrics: Metrics, cells: (u16, u16)) -> Self {
        assert!(
            cells.0 > 0 && cells.1 > 0,
            "GraphicsState::new: cells must be non-zero (cols > 0 and rows > 0), got {}x{}",
            cells.0,
            cells.1,
        );
        Self {
            stack: LayerStack::default(),
            metrics,
            cells,
            images: HashMap::new(),
            pane_images: HashMap::new(),
        }
    }

    /// Replace the cell-grid size [`Self::render_and_write`]
    /// composes against. v1 had a single tab with one root
    /// layout rect, so resizing wasn't a path; v2 wires host
    /// SIGWINCH (crossterm `Event::Resize`) into the binary's
    /// tick loop, which must call [`Self::set_cells`] so the
    /// dashcompositor framebuffer pixel dimensions stay
    /// in-sync with the layout engine's cell-grid rect.
    /// Asserts the same `non-zero` invariant as [`Self::new`]
    /// -- window-snap / hide-and-restore can briefly emit
    /// `Event::Resize(0, 0)` and we must reject before a
    /// zero-pixel composition would crash dashcompositor.
    pub fn set_cells(&mut self, cells: (u16, u16)) {
        assert!(
            cells.0 > 0 && cells.1 > 0,
            "GraphicsState::set_cells: cells must be non-zero (cols > 0 and rows > 0), got {}x{}",
            cells.0,
            cells.1,
        );
        self.cells = cells;
    }

    /// Read-only accessor for the cell-grid size
    /// [`Self::render_and_write`] composes against. Mirrors
    /// [`Self::set_cells`]; non-zero-by-construction guarantee
    /// is inherited from [`Self::new`] or any prior
    /// [`Self::set_cells`] call. Used by tests to assert a
    /// host resize made it through the binary's tick loop.
    pub fn cells(&self) -> (u16, u16) {
        self.cells
    }

    /// Push a fresh [`ImageLayer`] onto the stack from a
    /// pre-decoded `RGBA`, register it under `(pane, kitty_id)`,
    /// and add the `kitty_id` to the `pane`'s image list. Used by
    /// both `Self::on_kitty` (production) and unit/integration
    /// tests (no real PNG decode required).
    pub fn push_image(
        &mut self,
        pane: PaneLayerId,
        kitty_id: u32,
        rgba: image::RgbaImage,
    ) -> LayerId {
        let layer = ImageLayer::from_dynamic(image::DynamicImage::ImageRgba8(rgba.clone()), 0, 0);
        let lid = self.stack.push(layer);
        self.images.insert(
            (pane, kitty_id),
            ImageEntry {
                layer_id: lid,
                rgba,
            },
        );
        self.pane_images.entry(pane).or_default().push(kitty_id);
        lid
    }

    /// Apply one kitty event from the supplied pane's PTY. Errors
    /// are surfaced, never swallowed silently: callers decide
    /// whether to log+continue (`apply_kitty_event` is a thin
    /// wrapper that logs via `tracing::warn!` and returns `()`).
    /// Private -- only `Self::apply_kitty_event` (the public
    /// surface) and the internal sanity tests in this module
    /// call this; the pub surface is exactly `apply_kitty_event`.
    fn on_kitty(&mut self, pane: PaneLayerId, cmd: &KittyGraphicCmd) -> Result<(), GraphicsError> {
        match cmd {
            KittyGraphicCmd::Load {
                id,
                placement_id: _,
                format: _,
                width: _,
                height: _,
                data,
            } => {
                let dyn_img = image::load_from_memory(data)?;
                self.push_image(pane, *id, dyn_img.to_rgba8());
            }
            KittyGraphicCmd::Place {
                id,
                placement_id: _,
                x,
                y,
                cols_cells: _,
                rows_cells: _,
                z,
            } => {
                if let Some(mut entry) = self.images.remove(&(pane, *id)) {
                    self.stack.remove(entry.layer_id);
                    let dyn_img = image::DynamicImage::ImageRgba8(entry.rgba.clone());
                    let layer =
                        ImageLayer::from_dynamic(dyn_img, *x as u32, *y as u32).with_z(*z as u32);
                    let new_lid = self.stack.push(layer);
                    entry.layer_id = new_lid;
                    self.images.insert((pane, *id), entry);
                }
            }
            KittyGraphicCmd::Delete { id } => {
                if let Some(entry) = self.images.remove(&(pane, *id)) {
                    self.stack.remove(entry.layer_id);
                    if let Some(v) = self.pane_images.get_mut(&pane) {
                        v.retain(|x| x != id);
                    }
                }
            }
            KittyGraphicCmd::Control { .. } => {}
        }
        Ok(())
    }

    /// Best-effort wrapper around `Self::on_kitty` that logs
    /// failures via `tracing::warn!` instead of propagating. v1
    /// treats kitty errors as non-fatal because the child's own
    /// shell session must keep running; a failed image must not
    /// crash the multiplexer.
    pub fn apply_kitty_event(&mut self, pane: PaneLayerId, cmd: &KittyGraphicCmd) {
        if let Err(e) = self.on_kitty(pane, cmd) {
            warn!(error = %e, ?pane, "kitty graphics decode/route failed");
        }
    }

    /// Compose the layer stack into a framebuffer sized from
    /// `cells.0 * cell_w` by `cells.1 * cell_h` pixels, then enqueue
    /// it through dashcompositor's kitty passthrough encoder.
    /// Uses `CpuCompositor.compose` rather than
    /// `LayerStack::render_to_current_terminal` so frame size is
    /// driven by the binary's grid (not dashcompositor's
    /// `TerminalSize::current()` heuristic, which can drift on
    /// non-TTY CI).
    pub fn render_and_write<W: Write>(&self, writer: &mut W) -> Result<(), GraphicsError> {
        // Early-out when no images are loaded: composing an
        // empty LayerStack still produces a full-frame APC-G
        // block (~1 MiB at 80×24 cells) that overwrites the
        // text body rendered by ratatui in phase 3a. Skipping
        // the compose+encode avoids both the stdout corruption
        // and the per-frame CPU cost.
        if self.images.is_empty() {
            return Ok(());
        }
        let w_px = self.cells.0 as u32 * self.metrics.cell_w;
        let h_px = self.cells.1 as u32 * self.metrics.cell_h;
        let mut fb = FrameBuffer::new(w_px, h_px);
        CpuCompositor.compose(&self.stack, &mut fb);
        encode_passthrough_to_writer(&fb, writer)
            .map_err(|e| GraphicsError::Dispatch(e.to_string()))?;
        Ok(())
    }

    /// Returns `true` if a record exists for `(pane, kitty_id)`,
    /// i.e. an image layer was loaded into the pane and has not
    /// since been deleted. Useful for tests; cheap because the
    /// inner map has at most one entry per `(pane, kitty_id)`.
    pub fn has_image(&self, pane: PaneLayerId, kitty_id: u32) -> bool {
        self.images.contains_key(&(pane, kitty_id))
    }

    /// Tear down every layer that originated from `pane`. Called
    /// from the binary when a pane's child exits — the per-pane
    /// [`PaneLayerId`] is dropped from the maps and the
    /// dashcompositor `LayerStack` is asked to forget each
    /// associated `LayerId`.
    pub fn close_pane(&mut self, pane: PaneLayerId) {
        if let Some(ids) = self.pane_images.remove(&pane) {
            for id in ids {
                if let Some(entry) = self.images.remove(&(pane, id)) {
                    self.stack.remove(entry.layer_id);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internal sanity tests. Image layers are pushed via [`GraphicsState::push_image`]
// so we do not depend on a (notoriously fiddly) embedded PNG byte sequence.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod internal_sanity_tests {
    use super::*;

    fn rgba1x1() -> image::RgbaImage {
        image::RgbaImage::new(1, 1)
    }

    fn place_cmd(id: u32, x: i32, y: i32, z: i32) -> KittyGraphicCmd {
        KittyGraphicCmd::Place {
            id,
            placement_id: 0,
            x,
            y,
            cols_cells: None,
            rows_cells: None,
            z,
        }
    }

    #[test]
    fn load_inserts_layer_and_records_mapping() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.push_image(PaneLayerId(1), 7, rgba1x1());
        assert!(g.images.contains_key(&(PaneLayerId(1), 7)));
        let n = g
            .pane_images
            .get(&PaneLayerId(1))
            .map_or(0, std::vec::Vec::len);
        assert_eq!(n, 1);
    }

    #[test]
    fn place_updates_position_and_keeps_rgba() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        let pane = PaneLayerId(2);
        g.push_image(pane, 7, rgba1x1());
        g.on_kitty(pane, &place_cmd(7, 10, 20, 0)).expect("place");
        assert!(g.images.contains_key(&(pane, 7)));
    }

    #[test]
    fn delete_removes_layer_and_clears_per_pane_listing() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        let pane = PaneLayerId(3);
        g.push_image(pane, 7, rgba1x1());
        g.on_kitty(pane, &KittyGraphicCmd::Delete { id: 7 })
            .expect("delete");
        assert!(!g.images.contains_key(&(pane, 7)));
        let v = g.pane_images.get(&pane).expect("pane_images entry");
        assert!(
            v.is_empty(),
            "deleted image should leave an empty per-pane vec"
        );
    }

    #[test]
    fn unknown_place_is_silent_no_op() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.on_kitty(PaneLayerId(4), &place_cmd(99, 1, 2, 0))
            .expect("unknown place is a no-op");
        assert!(g.images.is_empty());
    }

    #[test]
    fn render_and_write_emits_escapes() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.push_image(PaneLayerId(5), 7, rgba1x1());
        let mut out = Vec::new();
        g.render_and_write(&mut out).expect("render");
        assert!(
            out.windows(3).any(|w| w == b"\x1b_G"),
            "encoded stream should contain the kitty APC-G escape"
        );
    }

    #[test]
    fn close_pane_drops_all_layers() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        let pane = PaneLayerId(6);
        g.push_image(pane, 7, rgba1x1());
        g.push_image(pane, 8, rgba1x1());
        g.close_pane(pane);
        assert!(!g.pane_images.contains_key(&pane));
        assert!(g.images.is_empty());
    }

    /// Cross-key invariant pin: for every `pane`, every `kitty_id`
    /// recorded in `pane_images[pane]` MUST also appear as the
    /// second component of a key in `images`. Exercised against
    /// the three mutating paths (`push_image`, `on_kitty::Place`,
    /// `on_kitty::Delete`) so a future regression that mutates one
    /// map without the other is caught at unit-test time.
    #[test]
    fn pane_images_subset_of_images_keys_after_push_place_delete() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        let pane = PaneLayerId(42);
        // Three pushes.
        g.push_image(pane, 1, rgba1x1());
        g.push_image(pane, 2, rgba1x1());
        g.push_image(pane, 3, rgba1x1());
        // Place-and-replace on kitty_id=2 (keeps both class membership
        // and the entry in pane_images).
        g.on_kitty(pane, &place_cmd(2, 5, 6, 0)).expect("place");
        // Delete on kitty_id=1 (removes from BOTH maps).
        g.on_kitty(pane, &KittyGraphicCmd::Delete { id: 1 })
            .expect("delete");
        // After all three ops the surviving pane_images[pane] is [2, 3]
        // (insert order; delete removed 1, place on 2 didn't change
        // its membership). Every entry must back a real `images` key.
        let recorded = g
            .pane_images
            .get(&pane)
            .expect("pane_images should still hold an entry for this pane")
            .clone();
        assert_eq!(recorded, vec![2, 3]);
        for kitty_id in &recorded {
            assert!(
                g.images.contains_key(&(pane, *kitty_id)),
                "pane_images[pane] = {:?} contains kitty_id {} but \
                 images lacks key ({:?}, {}) -- cross-key invariant violated",
                recorded,
                kitty_id,
                pane,
                kitty_id,
            );
        }
    }

    /// Ctor invariant pin: zero cols must panic with the exact phrase
    /// `"cells must be non-zero"` so external debuggers and test
    /// matchers can correlate the failure to the `Self::new` assert
    /// rather than chasing an opaque zero-framebuffer downstream.
    #[test]
    #[should_panic(expected = "cells must be non-zero")]
    fn graphics_state_new_panics_on_zero_cols() {
        let _ = GraphicsState::new(Metrics::default(), (0, 24));
    }

    /// Ctor invariant pin: zero rows must panic with the same
    /// phrase, symmetric to the cols case above.
    #[test]
    #[should_panic(expected = "cells must be non-zero")]
    fn graphics_state_new_panics_on_zero_rows() {
        let _ = GraphicsState::new(Metrics::default(), (80, 0));
    }

    /// Ctor invariant pin: zero `cell_w` must panic with the exact
    /// phrase `"cell_w and cell_h must be non-zero"` so debug-time
    /// failures (and tests) can correlate directly to the
    /// [`Metrics::new`] assert rather than chasing an opaque
    /// panic. Mirrors `graphics_state_new_panics_on_zero_cols`
    /// in shape and structure.
    #[test]
    #[should_panic(expected = "cell_w and cell_h must be non-zero")]
    fn metrics_new_panics_on_zero_cell_w() {
        let _ = Metrics::new(0, 16);
    }

    /// Ctor invariant pin: zero `cell_h` must panic with the same
    /// exact phrase, symmetric to the `cell_w` case above and to
    /// `graphics_state_new_panics_on_zero_rows`.
    #[test]
    #[should_panic(expected = "cell_w and cell_h must be non-zero")]
    fn metrics_new_panics_on_zero_cell_h() {
        let _ = Metrics::new(8, 0);
    }

    /// Regression test for `PaneRunner::Drop` -> `GraphicsState::close_pane`
    /// coupling through the close-channel. Spawns a real `PaneRunner` with
    /// the channel sender, drops the runner, drains the receiver
    /// (simulating `tick_loop`'s phase 1), and finally calls
    /// `close_pane` with the received id to assert the bookkeeping
    /// revokes the pane's image.
    ///
    /// This is the integration check that proves: (1) `Drop` enqueues
    /// the pane's `PaneLayerId` onto the close channel, and (2) the
    /// message contains the same id the binary will resolve through
    /// its tick loop's drain.
    #[test]
    fn drop_pane_runner_sends_close_to_channel() {
        use crate::pane::{PaneCloseTx, PaneRunner};
        use cmdash_config::parse as parse_config;
        use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
        use cmdash_pty::ShellSpec;
        use std::sync::mpsc;

        let mut graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let (close_tx, close_rx): (PaneCloseTx, _) = mpsc::channel();
        let pane_id = PaneLayerId(99);

        // Pre-populate one image layer for the pane.
        graphics.push_image(pane_id, 1, image::RgbaImage::new(1, 1));
        assert!(graphics.has_image(pane_id, 1), "image registered pre-drop");

        let cfg_text = "layout { pane kind=shell label=\"drop_test\" }";
        let cfg = parse_config(cfg_text).expect("parse KDL");
        let cfg_root = cfg.layout.expect("layout block");
        let layout = ComputedLayout::compute(
            &cfg_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute layout");
        let computed = layout.panes[0].clone();
        let runner = PaneRunner::spawn_with_graphics(
            computed,
            pane_id,
            ShellSpec::Command {
                argv: vec!["true".to_string()],
            },
            Some(close_tx),
        )
        .expect("spawn_with_graphics");

        // Drop enqueues the pane's layer id onto the close channel.
        drop(runner);

        // Simulate `tick_loop` phase 1: drain the close message and
        // call `close_pane` to revoke the dashcompositor layers.
        let received = close_rx
            .try_recv()
            .expect("PaneRunner::Drop must send a close message to the channel");
        assert_eq!(received, pane_id);
        graphics.close_pane(received);
        assert!(
            !graphics.has_image(pane_id, 1),
            "image layer should be revoked once the close-channel message is applied"
        );
    }

    /// `set_cells` ctor invariant pin: zero cols must panic
    /// with the same `"cells must be non-zero"` phrase the
    /// [`Self::new`] ctor uses, so callers -- debuggers and
    /// test matchers alike -- can correlate the panic to the
    /// `set_cells` assert rather than chasing an opaque
    /// zero-framebuffer downstream.
    #[test]
    #[should_panic(expected = "cells must be non-zero")]
    fn set_cells_panics_on_zero_cols() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.set_cells((0, 24));
    }

    /// Symmetric to `set_cells_panics_on_zero_cols`: zero
    /// rows must trip the same assert with the same panic
    /// phrase.
    #[test]
    #[should_panic(expected = "cells must be non-zero")]
    fn set_cells_panics_on_zero_rows() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.set_cells((80, 0));
    }

    /// Happy-path regression: a non-zero resize must round-trip
    /// through the read-only `cells()` accessor. Exercises the
    /// binding from the binary's host-resize-driven
    /// `GraphicsState::set_cells(...)` call to the
    /// `render_and_write` pixel composition surface.
    #[test]
    fn set_cells_updates_internal_state() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.set_cells((132, 50));
        assert_eq!(g.cells(), (132, 50));
    }

    /// Render-and-write with an empty `LayerStack` (no images
    /// pushed) must succeed and produce ZERO output. Without
    /// this early-out, `render_and_write` would compose a
    /// full-frame APC-G block (~1 MiB at 80×24 cells) into
    /// stdout on EVERY tick, overwriting the text body from
    /// ratatui's phase 3a `terminal.draw()`. This is the
    /// root-cause fix for the blank-screen bug: the encoder
    /// was emitting a full-screen empty kitty frame that
    /// occluded all text content.
    #[test]
    fn render_and_write_empty_stack_succeeds() {
        let g = GraphicsState::new(Metrics::default(), (80, 24));
        let mut out = Vec::new();
        g.render_and_write(&mut out)
            .expect("render_and_write with empty stack must not error");
        assert!(
            out.is_empty(),
            "empty-stack render must produce ZERO output (early-out); got {} bytes",
            out.len()
        );
    }

    /// Non-empty-stack output must be bounded: the encoder
    /// should not dump excessive framebuffer data. A 640x384
    /// pixel framebuffer (80×24 cells at 8×16 px/cell) with
    /// one 1×1 image should produce a compressed passthrough
    /// frame well under 4 MiB.
    #[test]
    fn render_and_write_nonempty_stack_output_is_bounded() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.push_image(PaneLayerId(1), 1, rgba1x1());
        let mut out = Vec::new();
        g.render_and_write(&mut out).expect("render");
        assert!(
            out.len() < 4 * 1024 * 1024,
            "non-empty-stack render output must be under 4 MiB; got {} bytes",
            out.len()
        );
    }
}
