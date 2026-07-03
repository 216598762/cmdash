//! ratatui render-side conversion: [`cmdash_pty::TextGrid`] → a
//! ratatui [`Buffer`]. Keeping the mapping logic in this crate
//! lets the binary compose multiple panes without leaking
//! per-pane details into the main loop.
//!
//! AGENTS.md §"Rendering pipeline" step 2 says the cell body is
//! drawn into a ratatui `Frame`; that's what [`blit_grid`] + the
//! [`blit_cursor`] helper drive.

use cmdash_pty::{CellAttrs, Color as PtyColor, TextGrid};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatRect;
use ratatui::style::{Color as RatColor, Modifier, Style};

/// Map the cmdash-pty [`PtyColor`] to ratatui's [`RatColor`].
pub fn pty_color_to_ratatui(c: PtyColor) -> RatColor {
    match c {
        PtyColor::Default => RatColor::Reset,
        PtyColor::Indexed(u) => RatColor::Indexed(u),
        PtyColor::Rgb(r, g, b) => RatColor::Rgb(r, g, b),
    }
}

/// Map the cmdash-pty [`CellAttrs`] to a [`Modifier`] bitmask.
pub fn pty_attrs_to_modifier(a: CellAttrs) -> Modifier {
    let mut m = Modifier::empty();
    if a.bold {
        m |= Modifier::BOLD;
    }
    if a.italic {
        m |= Modifier::ITALIC;
    }
    if a.underline {
        m |= Modifier::UNDERLINED;
    }
    if a.reverse {
        m |= Modifier::REVERSED;
    }
    m
}

/// Render a `TextGrid` into a `Buffer` at `area`.
///
/// Cells outside the grid bounds are not touched. Cells that are
/// still the canonical blank (space + default fg/bg + no attrs)
/// are skipped, so a smaller grid does not erase pre-existing
/// content stamped by an earlier pane.
pub fn blit_grid(grid: &TextGrid, buf: &mut Buffer, area: RatRect) {
    let cols = grid.cols();
    let rows = grid.rows();
    for y in 0..rows {
        if y >= area.height {
            break;
        }
        for x in 0..cols {
            if x >= area.width {
                break;
            }
            let cell = grid.cell(x, y);
            let bx = area.x + x;
            let by = area.y + y;
            if bx >= buf.area.width || by >= buf.area.height {
                continue;
            }
            if cell.ch == ' '
                && matches!(cell.fg, PtyColor::Default)
                && matches!(cell.bg, PtyColor::Default)
                && cell.attrs == CellAttrs::default()
            {
                continue;
            }
            let dest = buf.get_mut(bx, by);
            dest.set_symbol(&cell.ch.to_string());
            dest.set_style(
                Style::default()
                    .fg(pty_color_to_ratatui(cell.fg))
                    .bg(pty_color_to_ratatui(cell.bg))
                    .add_modifier(pty_attrs_to_modifier(cell.attrs)),
            );
        }
    }
}

/// Render the cursor as a reverse-video cell, if the cursor sits
/// inside the pane rect.
pub fn blit_cursor(grid: &TextGrid, buf: &mut Buffer, area: RatRect) {
    let (cx, cy) = grid.cursor();
    if cx >= area.width || cy >= area.height {
        return;
    }
    let bx = area.x + cx;
    let by = area.y + cy;
    if bx >= buf.area.width || by >= buf.area.height {
        return;
    }
    let dest = buf.get_mut(bx, by);
    dest.set_style(dest.style().add_modifier(Modifier::REVERSED));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_color_default_maps_to_reset() {
        assert_eq!(pty_color_to_ratatui(PtyColor::Default), RatColor::Reset);
    }

    #[test]
    fn pty_color_indexed_maps_to_index() {
        assert_eq!(
            pty_color_to_ratatui(PtyColor::Indexed(7)),
            RatColor::Indexed(7)
        );
    }

    #[test]
    fn pty_color_rgb_packs_three_u8_into_three_u32() {
        assert_eq!(
            pty_color_to_ratatui(PtyColor::Rgb(10, 20, 30)),
            RatColor::Rgb(10, 20, 30)
        );
    }

    #[test]
    fn pty_attrs_empty() {
        assert_eq!(
            pty_attrs_to_modifier(CellAttrs::default()),
            Modifier::empty()
        );
    }

    // Real `blit_grid` end-to-end coverage lives in the integration
    // test (`crates/cmdash/tests/wiring_smoke.rs`), which spawns a
    // real PTY, feeds bytes through vte, blits the resulting grid
    // into a ratatui `TestBackend`, and asserts the rendered
    // buffer. Unit tests here only cover the static color/attrs
    // mappings because `cmdash_pty::TextGrid::put` is private.
}
