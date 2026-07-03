//! cmdash binary crate: event loop + crate glue.
//!
//! v1 renders cell-only via ratatui (degraded text-mode per
//! AGENTS.md §"Rendering pipeline" step 7). Kitty-graphics events
//! from nested terminals surface as `cmdash_pty::PaneEvent::KittyGraphic`;
//! cmdash logs them via `tracing` and holds them as a placeholder
//! for the dashcompositor wiring planned for v2.
//!
//! See AGENTS.md §"Hard rule: one layer per instance" for the
//! layer-per-pane invariant this crate upholds.

pub mod layer_id;
pub mod pane;
pub mod render;

pub use layer_id::{derive_layer_id, SINGLE_TAB};
pub use pane::{PaneRunner, RunnerError};
pub use render::{blit_cursor, blit_grid, pty_attrs_to_modifier, pty_color_to_ratatui};

#[doc(hidden)]
pub fn _cmdash_dep_smoke() {
    // Surface dashcompositor feature-flag typos at `cargo check`.
    let _ = core::mem::size_of::<dashcompositor::FrameBuffer>();
}
