//! Status bar rendering for the optional bottom (or top) status bar.
//!
//! The status bar is a single row that displays the current keybind
//! mode, the focused pane's label, and the current time. It is
//! rendered in phase 3a after pane blits, matching the tab bar
//! pattern.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Render the status bar into `buf` at the given `area`.
///
/// The area should be exactly 1 row tall and span the full terminal
/// width. Content is right-aligned for the clock, left-aligned for
/// mode and pane title, with a separator between them.
///
/// # Arguments
///
/// * `buf` — the ratatui buffer to render into.
/// * `area` — the 1-row rect for the status bar.
/// * `mode` — the current keybind mode name (e.g. "Normal", "PaneResize").
/// * `pane_title` — the focused pane's label, if set.
/// * `show_clock` — whether to display the current time.
/// * `show_pane_title` — whether to display the pane title.
/// * `show_mode` — whether to display the current mode.
pub fn render_status_bar(
    buf: &mut Buffer,
    area: Rect,
    mode: &str,
    pane_title: Option<&str>,
    show_clock: bool,
    show_pane_title: bool,
    show_mode: bool,
) {
    let width = area.width as usize;

    // Build left side: mode + pane title.
    let mut left_spans: Vec<Span> = Vec::new();
    if show_mode {
        left_spans.push(Span::styled(
            format!(" {mode}"),
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ));
    }
    if show_pane_title {
        if let Some(title) = pane_title {
            if !title.is_empty() {
                left_spans.push(Span::styled(
                    format!(" {title}"),
                    Style::default().fg(Color::Gray),
                ));
            }
        }
    }

    // Build right side: clock using std time (no chrono dependency).
    let right_text = if show_clock {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Simple HH:MM from epoch seconds (UTC).
        let total_minutes = (now / 60) as u32;
        let hours = (total_minutes / 60) % 24;
        let minutes = total_minutes % 60;
        format!("{hours:02}:{minutes:02}")
    } else {
        String::new()
    };

    // Compose the line with padding between left and right.
    let left_width: usize = left_spans.iter().map(|s| s.width()).sum();
    let right_width = right_text.len();
    let separator_count = width.saturating_sub(left_width + right_width);
    let separator = " ".repeat(separator_count);

    let mut spans = left_spans;
    spans.push(Span::styled(separator, Style::default()));
    if !right_text.is_empty() {
        spans.push(Span::styled(
            format!("{right_text} "),
            Style::default().fg(Color::Gray),
        ));
    }

    let line = Line::from(spans);
    buf.set_line(area.x, area.y, &line, area.width);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_status_bar_does_not_panic_on_empty_area() {
        let backend = ratatui::backend::TestBackend::new(80, 5);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                let area = Rect::new(0, 4, 80, 1);
                render_status_bar(buf, area, "Normal", None, false, false, true);
            })
            .unwrap();
    }

    #[test]
    fn render_status_bar_mode_visible_when_enabled() {
        let backend = ratatui::backend::TestBackend::new(80, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                let area = Rect::new(0, 0, 80, 1);
                render_status_bar(buf, area, "Normal", None, false, false, true);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let cell_text: String = (0..80)
            .map(|x| buf.get(x, 0).symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            cell_text.contains("Normal"),
            "mode 'Normal' must appear in status bar; got: {cell_text}"
        );
    }

    #[test]
    fn render_status_bar_mode_hidden_when_disabled() {
        let backend = ratatui::backend::TestBackend::new(80, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                let area = Rect::new(0, 0, 80, 1);
                render_status_bar(buf, area, "Normal", None, false, false, false);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let cell_text: String = (0..80)
            .map(|x| buf.get(x, 0).symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            !cell_text.contains("Normal"),
            "mode must not appear when show_mode=false; got: {cell_text}"
        );
    }

    #[test]
    fn render_status_bar_pane_title_visible() {
        let backend = ratatui::backend::TestBackend::new(80, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                let area = Rect::new(0, 0, 80, 1);
                render_status_bar(buf, area, "Normal", Some("editor"), false, true, true);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let cell_text: String = (0..80)
            .map(|x| buf.get(x, 0).symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            cell_text.contains("editor"),
            "pane title 'editor' must appear; got: {cell_text}"
        );
    }

    #[test]
    fn render_status_bar_pane_title_hidden() {
        let backend = ratatui::backend::TestBackend::new(80, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                let area = Rect::new(0, 0, 80, 1);
                render_status_bar(buf, area, "Normal", Some("editor"), false, false, true);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let cell_text: String = (0..80)
            .map(|x| buf.get(x, 0).symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            !cell_text.contains("editor"),
            "pane title must not appear when show_pane_title=false; got: {cell_text}"
        );
    }

    #[test]
    fn render_status_bar_clock_visible_when_enabled() {
        let backend = ratatui::backend::TestBackend::new(80, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                let area = Rect::new(0, 0, 80, 1);
                render_status_bar(buf, area, "Normal", None, true, false, true);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let cell_text: String = (0..80)
            .map(|x| buf.get(x, 0).symbol().chars().next().unwrap_or(' '))
            .collect();
        // Clock should contain a colon (HH:MM format).
        assert!(
            cell_text.contains(':'),
            "clock must appear with HH:MM format; got: {cell_text}"
        );
    }
}
