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
///
/// ## Scrollback rendering
///
/// When the grid's `scrollback_offset` is > 0 (the user is
/// viewing scrollback history), the top rows of the area are
/// filled from the scrollback ring buffer and the bottom rows
/// from the live grid. The mapping is:
///
/// - Visible row `y < offset` → scrollback row
///   `scrollback.len() - offset + y` (newest scrollback rows
///   first, filling downward).
/// - Visible row `y >= offset` → live grid row `y - offset`.
pub fn blit_grid(grid: &TextGrid, buf: &mut Buffer, area: RatRect) {
    let cols = grid.cols();
    let rows = grid.rows();
    let offset = grid.scrollback_offset();
    let sb_len = grid.scrollback_len();
    let visible_rows = area.height as usize;
    for vy in 0..visible_rows {
        let x_limit = cols.min(area.width);
        for x in 0..x_limit {
            // Resolve the source cell: scrollback row or live
            // grid row, depending on the viewport offset.
            let cell_ref: Option<&cmdash_pty::Cell> = if vy < offset && offset <= sb_len {
                let sb_idx = sb_len - offset + vy;
                grid.scrollback_row(sb_idx)
                    .and_then(|row| row.get(x as usize))
            } else {
                let gy = if vy >= offset { vy - offset } else { vy };
                if gy < rows as usize {
                    Some(grid.cell(x, gy as u16))
                } else {
                    None
                }
            };
            let Some(cell) = cell_ref else {
                continue;
            };
            let bx = area.x + x;
            let by = area.y + vy as u16;
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

/// Render a rectangular selection overlay on top of a pane.
///
/// `sel_start` and `sel_end` are pane-local (x, y) cell
/// coordinates. The inclusive bounding box is highlighted with a
/// reversed-video style. Coordinates outside the pane area are
/// clamped.
pub fn blit_selection(buf: &mut Buffer, area: RatRect, sel_start: (u16, u16), sel_end: (u16, u16)) {
    let min_x = sel_start.0.min(sel_end.0);
    let max_x = sel_start.0.max(sel_end.0);
    let min_y = sel_start.1.min(sel_end.1);
    let max_y = sel_start.1.max(sel_end.1);
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let bx = area.x + x;
            let by = area.y + y;
            if bx >= buf.area.width || by >= buf.area.height {
                continue;
            }
            let dest = buf.get_mut(bx, by);
            dest.set_style(dest.style().add_modifier(Modifier::REVERSED));
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

/// Extract the text covered by the current copy-mode selection.
/// If no selection anchor is set, only the character under the cursor
/// is returned. Coordinates are clamped to the grid bounds.
///
/// `cursor_x`/`cursor_y` are the current cursor coordinates.
/// `selection_start` is the optional anchor coordinate.
pub fn extract_selected_text(
    grid: &TextGrid,
    cursor_x: u16,
    cursor_y: u16,
    selection_start: Option<(u16, u16)>,
) -> String {
    let cols = grid.cols();
    let rows = grid.rows();
    let (start_x, start_y, end_x, end_y) = if let Some(anchor) = selection_start {
        let min_x = anchor.0.min(cursor_x);
        let max_x = anchor.0.max(cursor_x);
        let min_y = anchor.1.min(cursor_y);
        let max_y = anchor.1.max(cursor_y);
        (min_x, min_y, max_x, max_y)
    } else {
        // No selection: copy only the character under the cursor.
        let cx = cursor_x.min(cols.saturating_sub(1));
        let cy = cursor_y.min(rows.saturating_sub(1));
        return grid.cell(cx, cy).ch.to_string();
    };
    let mut lines: Vec<String> = Vec::new();
    for y in start_y..=end_y {
        if y >= rows {
            break;
        }
        let mut line = String::new();
        for x in start_x..=end_x {
            if x >= cols {
                break;
            }
            let cell = grid.cell(x, y);
            line.push(cell.ch);
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

/// Copy the given text to the system clipboard.
///
/// This is a thin wrapper around [`arboard::Clipboard`] so the
/// copy-mode path can be tested without mocking the clipboard.
pub fn copy_text_to_clipboard(
    text: impl Into<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text.into())?;
    Ok(())
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

    /// `blit_selection` highlights the inclusive bounding box
    /// between two pane-local coordinates with the REVERSED
    /// modifier.
    #[test]
    fn blit_selection_highlights_inclusive_bounding_box() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, 24);
                blit_selection(frame.buffer_mut(), area, (1, 1), (3, 2));
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        // Cells inside the selection should be reversed.
        for y in 1..=2 {
            for x in 1..=3 {
                assert!(
                    buf.get(x, y)
                        .style()
                        .add_modifier
                        .contains(ratatui::style::Modifier::REVERSED),
                    "cell ({x},{y}) must be reversed"
                );
            }
        }
        // Cells outside the selection should NOT be reversed.
        assert!(
            !buf.get(0, 0)
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "cell (0,0) must not be reversed"
        );
        assert!(
            !buf.get(4, 2)
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "cell (4,2) must not be reversed"
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
            buf.get(0, 0)
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "cursor at (0,0) must have REVERSED modifier"
        );
        // Cell (1, 0) must NOT have REVERSED.
        assert!(
            !buf.get(1, 0)
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED),
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
            buf.get(40, 10)
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "cursor maps to (40,10) and must have REVERSED modifier"
        );
    }
}
