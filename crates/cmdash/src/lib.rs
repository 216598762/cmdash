//! cmdash binary crate: event loop + crate glue.
//!
//! Text bodies render through ratatui; kitty graphics events
//! from nested PTY children route through [`crate::graphics`]
//! into a [`dashcompositor::LayerStack`], then are emitted after
//! the ratatui draw via dashcompositor's kitty passthrough
//! encoder.
//!
//! See AGENTS.md §"Hard rule: one layer per instance" for the
//! layer-per-pane invariant this crate upholds.

pub mod graphics;
pub mod layer_id;
pub mod pane;
pub mod render;
pub mod tabs;

pub use graphics::{GraphicsError, GraphicsProtocol, GraphicsState, Metrics, TabBarData};
pub use layer_id::{derive_layer_id, derive_layer_id_for_tab, SINGLE_TAB};
pub use pane::{PaneCloseTx, PaneRunner, RunnerError};
pub use render::{blit_cursor, blit_grid, pty_attrs_to_modifier, pty_color_to_ratatui};
pub use tabs::{Tab, TabStack};

#[doc(hidden)]
pub fn _cmdash_dep_smoke() {
    // Surface dashcompositor feature-flag typos at `cargo check`.
    let _ = core::mem::size_of::<dashcompositor::FrameBuffer>();
}
