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

    /// `blit_grid` against a blank `TextGrid` (the initial state
    /// after `TextGrid::new`) must NOT touch the buffer. The
    /// skip-blank optimization in `blit_grid` intentionally skips
    /// cells that are `space + default fg/bg + no attrs` so a
    /// smaller grid does not erase pre-existing content. This test
    /// verifies the baseline: a fresh grid produces zero buffer
    /// mutations against a clean buffer.
    ///
    /// Catches: a regression that removes the skip-blank guard
    /// would cause `blit_grid` to overwrite every cell with a
    /// space, which is the exact "blank screen" symptom.
    #[test]
    fn blit_grid_blank_grid_does_not_touch_buffer() {
        let grid = cmdash_pty::TextGrid::new(80, 24);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        // Write a sentinel AND blit the blank grid in a SINGLE
        // draw call. `Terminal::draw` resets the buffer before
        // each closure, so combining them in one call is required
        // to verify that `blit_grid` skips the sentinel cell.
        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                // Stamp sentinel BEFORE blit.
                buf.get_mut(0, 0).set_symbol("X");
                buf.get_mut(5, 3).set_symbol("Y");
                // Now blit a blank grid on top.
                let area = ratatui::layout::Rect::new(0, 0, 80, 24);
                blit_grid(&grid, buf, area);
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            buf.get(0, 0).symbol(),
            "X",
            "blit_grid with blank grid must not overwrite pre-existing buffer content; \
             the skip-blank guard should leave cell (0,0) at its sentinel value"
        );
        assert_eq!(
            buf.get(5, 3).symbol(),
            "Y",
            "blit_grid with blank grid must not overwrite pre-existing buffer content; \
             the skip-blank guard should leave cell (5,3) at its sentinel value"
        );
    }

    /// `blit_cursor` with cursor at (0, 0) must add the REVERSED
    /// modifier to that cell. The cursor cell is the only cell
    /// that gets the reverse-video treatment; all other cells
    /// must remain un-reversed.
    #[test]
    fn blit_cursor_at_origin_adds_reversed() {
        let grid = cmdash_pty::TextGrid::new(80, 24);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, 24);
                blit_cursor(&grid, frame.buffer_mut(), area);
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        assert!(
            buf.get(0, 0).style().add_modifier.contains(ratatui::style::Modifier::REVERSED),
            "cursor at (0,0) must have REVERSED modifier"
        );
        // Cell (1, 0) must NOT have REVERSED.
        assert!(
            !buf.get(1, 0).style().add_modifier.contains(ratatui::style::Modifier::REVERSED),
            "non-cursor cell (1,0) must NOT have REVERSED modifier"
        );
    }

    /// `blit_cursor` with cursor outside the area must be a
    /// no-op (no panic, no buffer mutation). The cursor sits at
    /// (0, 0) by default; when the area starts at (10, 10), the
    /// cursor is outside the area's local coordinate space.
    /// `blit_cursor` when the cursor position lies outside the
    /// area's cell range must be a no-op. The cursor is at (0,0)
    /// by default in a fresh `TextGrid`; an area starting at
    /// (40, 10) maps the cursor to buffer cell (40, 10) — which
    /// IS inside the area (0 < area.width). To truly be outside,
    /// we need a zero-sized area (width=0 or height=0), but
    /// `blit_cursor` guards against that via
    /// `cx >= area.width || cy >= area.height`. With a 1x1 area
    /// at (40, 10), cursor (0,0) maps to (40,10) which IS inside;
    /// the cursor IS rendered. Instead, test that cells FAR from
    /// the area are unaffected by the cursor render.
    #[test]
    fn blit_cursor_does_not_affect_cells_outside_area() {
        let grid = cmdash_pty::TextGrid::new(80, 24);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                // Stamp sentinels at cells far from the area.
                buf.get_mut(0, 0).set_symbol("A");
                buf.get_mut(1, 0).set_symbol("B");
                // blit_cursor with area at (40, 10). Cursor (0,0)
                // maps to buffer cell (40, 10) — cells (0,0) and
                // (1,0) must be unaffected.
                let area = ratatui::layout::Rect::new(40, 10, 20, 5);
                blit_cursor(&grid, buf, area);
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            buf.get(0, 0).symbol(),
            "A",
            "cell (0,0) must retain sentinel 'A' after blit_cursor with area at (40,10)"
        );
        assert_eq!(
            buf.get(1, 0).symbol(),
            "B",
            "cell (1,0) must retain sentinel 'B' after blit_cursor with area at (40,10)"
        );
        // The cursor-reversed cell at (40, 10) must have REVERSED.
        assert!(
            buf.get(40, 10).style().add_modifier.contains(ratatui::style::Modifier::REVERSED),
            "cursor maps to (40,10) and must have REVERSED modifier"
        );
    }
}
