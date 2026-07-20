use super::*;

// ------------------------------------------------------------------
// Scrollback buffer tests
// ------------------------------------------------------------------

/// `scroll_up_one` captures the top row into the scrollback
/// ring buffer before shifting cells up. After one scroll,
/// `scrollback_len()` is 1 and the captured row contains
/// the character that was on row 0.
#[test]
fn scroll_up_one_captures_top_row_into_scrollback() {
    let mut g = TextGrid::new(3, 3);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'X',
    );
    g.scroll_up_one();
    assert_eq!(g.scrollback_len(), 1);
    let row = g.scrollback_row(0).expect("row 0");
    assert_eq!(row[0].ch, 'X');
}

/// Multiple scrolls accumulate rows in the scrollback buffer
/// in FIFO order. After 3 scrolls, `scrollback_len()` is 3
/// and the rows are in chronological order (oldest first).
#[test]
fn scrollback_accumulates_rows_in_fifo_order() {
    let mut g = TextGrid::new(2, 2);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'B',
    );
    g.scroll_up_one();
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'C',
    );
    g.scroll_up_one();
    assert_eq!(g.scrollback_len(), 3);
    assert_eq!(g.scrollback_row(0).unwrap()[0].ch, 'A');
    assert_eq!(g.scrollback_row(1).unwrap()[0].ch, 'B');
    assert_eq!(g.scrollback_row(2).unwrap()[0].ch, 'C');
}

/// Scrollback ring buffer respects capacity. When the buffer
/// is full, the oldest row is discarded on each new scroll.
#[test]
fn scrollback_ring_buffer_drops_oldest_at_capacity() {
    let mut g = TextGrid::new(2, 2);
    // Set capacity to 2 rows.
    g.scrollback_capacity = 2;
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'B',
    );
    g.scroll_up_one();
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'C',
    );
    g.scroll_up_one();
    // Oldest row 'A' was dropped; buffer has 'B' and 'C'.
    assert_eq!(g.scrollback_len(), 2);
    assert_eq!(g.scrollback_row(0).unwrap()[0].ch, 'B');
    assert_eq!(g.scrollback_row(1).unwrap()[0].ch, 'C');
}

/// `scrollback_up(n)` moves the viewport offset into
/// scrollback history. `scrollback_down(n)` returns to
/// live view. `in_scrollback()` tracks the state.
#[test]
fn scrollback_up_down_and_in_scrollback() {
    let mut g = TextGrid::new(2, 2);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'B',
    );
    g.scroll_up_one();
    // 2 rows in scrollback. Start in live view.
    assert!(!g.in_scrollback());
    assert_eq!(g.scrollback_offset(), 0);
    // Scroll up by 1.
    g.scrollback_up(1);
    assert!(g.in_scrollback());
    assert_eq!(g.scrollback_offset(), 1);
    // Scroll up by 1 more — now at full depth.
    g.scrollback_up(1);
    assert_eq!(g.scrollback_offset(), 2);
    // Scroll up beyond capacity — clamped.
    g.scrollback_up(100);
    assert_eq!(g.scrollback_offset(), 2);
    // Scroll back down.
    g.scrollback_down(1);
    assert_eq!(g.scrollback_offset(), 1);
    g.scrollback_down(1);
    assert_eq!(g.scrollback_offset(), 0);
    assert!(!g.in_scrollback());
    // Down beyond 0 — clamped.
    g.scrollback_down(100);
    assert_eq!(g.scrollback_offset(), 0);
}

/// `scrollback_reset()` returns the viewport to live view
/// from any offset.
#[test]
fn scrollback_reset_returns_to_live_view() {
    let mut g = TextGrid::new(2, 2);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'B',
    );
    g.scroll_up_one();
    g.scrollback_up(2);
    assert!(g.in_scrollback());
    g.scrollback_reset();
    assert!(!g.in_scrollback());
    assert_eq!(g.scrollback_offset(), 0);
}
/// `clear_scrollback()` (ESC [3J) clears the scrollback
/// buffer and resets the offset. `clear_all()` (ESC [2J)
/// only clears the visible screen and does NOT touch
/// scrollback — matching xterm / VTE semantics.
#[test]
fn clear_scrollback_resets_buffer_and_offset() {
    let mut g = TextGrid::new(2, 2);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();
    g.scrollback_up(1);
    assert!(g.in_scrollback());
    assert_eq!(g.scrollback_len(), 1);
    g.clear_scrollback();
    assert_eq!(g.scrollback_len(), 0);
    assert_eq!(g.scrollback_offset(), 0);
    assert!(!g.in_scrollback());
}

/// `clear_all()` (ESC [2J) clears the visible screen but
/// does NOT clear the scrollback buffer. This is the
/// correct xterm/VTE behavior: ESC [2J only affects the
/// display, not the history.
#[test]
fn clear_all_preserves_scrollback() {
    let mut g = TextGrid::new(2, 2);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();
    assert_eq!(g.scrollback_len(), 1);
    g.clear_all();
    // Scrollback is preserved — only ESC [3J clears it.
    assert_eq!(g.scrollback_len(), 1);
}

/// Capacity=0 disables scrollback capture entirely (the
/// ring buffer stays empty even after many scrolls).
#[test]
fn scrollback_capacity_zero_disables_capture() {
    let mut g = TextGrid::new(2, 2);
    g.scrollback_capacity = 0;
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'B',
    );
    g.scroll_up_one();
    assert_eq!(g.scrollback_len(), 0);
}

// ------------------------------------------------------------------
// Scrollback ring buffer tests: push, capacity, up/down/reset,
// in_scrollback, alternate_screen, set_scrollback_capacity
// ------------------------------------------------------------------

#[test]
fn scrollback_push_captures_top_row() {
    let mut g = TextGrid::new(4, 3);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'H',
    );
    g.put(
        1,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'i',
    );
    g.scroll_up_one();

    assert_eq!(g.scrollback_len(), 1);
    let row = g.scrollback_row(0).expect("row 0");
    assert_eq!(row[0].ch, 'H');
    assert_eq!(row[1].ch, 'i');
}

#[test]
fn scrollback_multiple_scrolls_accumulate() {
    let mut g = TextGrid::new(3, 3);
    // Write 'A' at row 0, scroll, write 'B' at row 0, scroll.
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'B',
    );
    g.scroll_up_one();

    assert_eq!(g.scrollback_len(), 2);
    // Index 0 = oldest = 'A' row.
    assert_eq!(g.scrollback_row(0).unwrap()[0].ch, 'A');
    // Index 1 = newest = 'B' row.
    assert_eq!(g.scrollback_row(1).unwrap()[0].ch, 'B');
}

#[test]
fn scrollback_capacity_evicts_oldest() {
    let mut g = TextGrid::new(3, 3);
    g.set_scrollback_capacity(2);

    // Push 3 rows into scrollback; capacity is 2, so oldest is evicted.
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        '1',
    );
    g.scroll_up_one();
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        '2',
    );
    g.scroll_up_one();
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        '3',
    );
    g.scroll_up_one();

    assert_eq!(g.scrollback_len(), 2);
    // Oldest ('1') was evicted; '2' is now oldest.
    assert_eq!(g.scrollback_row(0).unwrap()[0].ch, '2');
    assert_eq!(g.scrollback_row(1).unwrap()[0].ch, '3');
}

#[test]
fn set_scrollback_capacity_zero_disables_capture() {
    let mut g = TextGrid::new(3, 3);
    g.set_scrollback_capacity(0);

    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'X',
    );
    g.scroll_up_one();

    assert_eq!(g.scrollback_len(), 0);
}

#[test]
fn set_scrollback_capacity_truncates_buffer() {
    let mut g = TextGrid::new(3, 3);
    g.set_scrollback_capacity(10);

    // Push 5 rows.
    for i in 0..5 {
        g.put(
            0,
            0,
            Color::Default,
            Color::Default,
            CellAttrs::default(),
            (b'A' + i) as char,
        );
        g.scroll_up_one();
    }
    assert_eq!(g.scrollback_len(), 5);

    // Lower capacity to 2; should truncate oldest 3 rows.
    g.set_scrollback_capacity(2);
    assert_eq!(g.scrollback_len(), 2);
    // 'D' and 'E' remain.
    assert_eq!(g.scrollback_row(0).unwrap()[0].ch, 'D');
    assert_eq!(g.scrollback_row(1).unwrap()[0].ch, 'E');
}

#[test]
fn alternate_screen_disables_scrollback_capture() {
    let mut g = TextGrid::new(3, 3);
    g.set_alternate_screen(true);

    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'Z',
    );
    g.scroll_up_one();

    // Alternate screen prevents scrollback capture.
    assert_eq!(g.scrollback_len(), 0);
}

#[test]
fn scrollback_up_enters_scrollback_mode() {
    let mut g = TextGrid::new(3, 3);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();

    assert!(!g.in_scrollback());
    assert_eq!(g.scrollback_offset(), 0);

    g.scrollback_up(1);

    assert!(g.in_scrollback());
    assert_eq!(g.scrollback_offset(), 1);
}

#[test]
fn scrollback_up_clamps_to_buffer_length() {
    let mut g = TextGrid::new(3, 3);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();

    // Request scrolling up by 100, but buffer only has 1 row.
    g.scrollback_up(100);

    assert_eq!(g.scrollback_offset(), 1);
    assert_eq!(g.scrollback_offset(), g.scrollback_len());
}

#[test]
fn scrollback_down_returns_toward_live() {
    let mut g = TextGrid::new(3, 3);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();

    g.scrollback_up(1);
    assert!(g.in_scrollback());

    g.scrollback_down(1);
    assert!(!g.in_scrollback());
    assert_eq!(g.scrollback_offset(), 0);
}

#[test]
fn scrollback_down_saturates_at_zero() {
    let mut g = TextGrid::new(3, 3);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();

    g.scrollback_up(1);
    g.scrollback_down(100); // Overshoot.

    assert_eq!(g.scrollback_offset(), 0);
    assert!(!g.in_scrollback());
}

#[test]
fn scrollback_reset_returns_to_live() {
    let mut g = TextGrid::new(3, 3);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();

    g.scrollback_up(1);
    assert!(g.in_scrollback());

    g.scrollback_reset();
    assert!(!g.in_scrollback());
    assert_eq!(g.scrollback_offset(), 0);
}

#[test]
fn in_scrollback_false_by_default() {
    let g = TextGrid::new(3, 3);
    assert!(!g.in_scrollback());
    assert_eq!(g.scrollback_offset(), 0);
    assert_eq!(g.scrollback_len(), 0);
}

#[test]
fn scrollback_row_none_for_out_of_bounds() {
    let mut g = TextGrid::new(3, 3);
    g.put(
        0,
        0,
        Color::Default,
        Color::Default,
        CellAttrs::default(),
        'A',
    );
    g.scroll_up_one();

    assert!(g.scrollback_row(0).is_some());
    assert!(g.scrollback_row(1).is_none());
    assert!(g.scrollback_row(999).is_none());
}

#[test]
fn scrollback_preserves_cell_colors_and_attrs() {
    let mut g = TextGrid::new(3, 3);
    g.put(
        0,
        0,
        Color::Indexed(1),
        Color::Indexed(2),
        CellAttrs::default(),
        'C',
    );
    g.scroll_up_one();

    let row = g.scrollback_row(0).unwrap();
    assert_eq!(row[0].ch, 'C');
    assert_eq!(row[0].fg, Color::Indexed(1));
    assert_eq!(row[0].bg, Color::Indexed(2));
}

#[test]
fn scrollback_up_down_multiple_rows() {
    let mut g = TextGrid::new(3, 3);
    // Push 5 rows.
    for i in 0..5 {
        g.put(
            0,
            0,
            Color::Default,
            Color::Default,
            CellAttrs::default(),
            (b'A' + i) as char,
        );
        g.scroll_up_one();
    }
    assert_eq!(g.scrollback_len(), 5);

    // Scroll up3.
    g.scrollback_up(3);
    assert_eq!(g.scrollback_offset(), 3);
    assert!(g.in_scrollback());

    // Scroll down 2.
    g.scrollback_down(2);
    assert_eq!(g.scrollback_offset(), 1);
    assert!(g.in_scrollback());

    // Scroll down 1 more → live view.
    g.scrollback_down(1);
    assert_eq!(g.scrollback_offset(), 0);
    assert!(!g.in_scrollback());
}
