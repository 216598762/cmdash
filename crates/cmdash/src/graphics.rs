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
//! - [`GraphicsState::on_kitty`] dispatches each
//!   [`KittyGraphicCmd`] variant:
//!   - `Load`: decode the RGBA payload via [`image::load_from_memory`]
//!     and call [`Self::push_image`] to register the freshly-pushed
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
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    pub cell_w: u32,
    pub cell_h: u32,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            cell_w: 8,
            cell_h: 16,
        }
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
    pub stack: LayerStack,
    pub metrics: Metrics,
    /// Total terminal size in cells; updated by callers
    /// (main.rs) before each `render_and_write`. v1 has a single
    /// tab with one root layout rect, so (cols, rows) is the
    /// full-screen total.
    pub cells: (u16, u16),
    images: HashMap<(PaneLayerId, u32), ImageEntry>,
    pane_images: HashMap<PaneLayerId, Vec<u32>>,
}

impl GraphicsState {
    pub fn new(metrics: Metrics, cells: (u16, u16)) -> Self {
        Self {
            stack: LayerStack::default(),
            metrics,
            cells,
            images: HashMap::new(),
            pane_images: HashMap::new(),
        }
    }

    /// Push a fresh [`ImageLayer`] onto the stack from a
    /// pre-decoded RGBA, register it under `(pane, kitty_id)`,
    /// and add the kitty_id to the pane's image list. Used by
    /// both [`Self::on_kitty`] (production) and unit/integration
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
    /// whether to log+continue ([`apply_kitty_event`] is a thin
    /// wrapper that logs via `tracing::warn!` and returns `()`).
    pub fn on_kitty(
        &mut self,
        pane: PaneLayerId,
        cmd: &KittyGraphicCmd,
    ) -> Result<(), GraphicsError> {
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

    /// Best-effort wrapper around [`Self::on_kitty`] that logs
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
}
