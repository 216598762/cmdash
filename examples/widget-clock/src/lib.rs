//! widget-clock — Example cmdash cdylib widget.
//!
//! Displays the current wall-clock time in a bordered ratatui panel.
//! This widget validates the full cmdash widget loading pipeline:
//!
//! 1. Host loads `libwidget_clock.so` via `libloading`
//! 2. Host calls `cmdash_widget_create(ABI_VERSION)` → `Box<dyn CmdashWidget>`
//! 3. Host calls `widget.render(area, frame)` once per frame (~30 fps)
//! 4. Host forwards `WidgetEvent` on focus/key events
//!
//! Build:  `cargo build -p widget-clock --release`
//! Install: `cp target/release/libwidget_clock.so ~/.config/cmdash/widgets/widget-clock/`
//! Config:  `pane kind=widget ref-name="widget-clock"`

use cmdash_widget_sdk::{cmdash_widget_export, CmdashWidget, WidgetEvent};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// Real-time clock widget. Tracks focus state for a visual border
/// highlight and renders the current HH:MM:SS on every frame.
///
/// Caches the second to avoid redundant `SystemTime::now()` syscalls
/// across consecutive frames within the same second.
#[derive(Default)]
pub struct ClockWidget {
    /// Whether this pane currently holds input focus.
    focused: bool,
    /// Last rendered second — avoids redundant syscalls.
    last_second: u64,
}

impl CmdashWidget for ClockWidget {
    fn name(&self) -> &str {
        "widget-clock"
    }

    fn render(&mut self, area: Rect, frame: &mut Frame) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();

        let total_secs = now.as_secs();
        // Skip redundant syscalls when the second hasn't changed.
        // Tradeoff: a terminal resize won't redraw until the next
        // second ticks — acceptable for an example widget.
        if total_secs == self.last_second {
            return;
        }
        self.last_second = total_secs;

        let hours = (total_secs / 3600) % 24;
        let minutes = (total_secs / 60) % 60;
        let seconds = total_secs % 60;

        let time_str = format!("{:02}:{:02}:{:02}", hours, minutes, seconds);

        let border_style = if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let block = Block::default()
            .title(" 🕐 Clock ")
            .borders(Borders::ALL)
            .border_style(border_style);

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width > 0 && inner.height > 0 {
            let clock_style = Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);

            let line = Line::from(Span::styled(time_str, clock_style));
            let paragraph = Paragraph::new(line).alignment(Alignment::Center);
            frame.render_widget(paragraph, inner);
        }
    }

    fn on_event(&mut self, event: &WidgetEvent) {
        match event {
            WidgetEvent::FocusGained => self.focused = true,
            WidgetEvent::FocusLost => self.focused = false,
            _ => {}
        }
    }
}

cmdash_widget_export!(ClockWidget);
