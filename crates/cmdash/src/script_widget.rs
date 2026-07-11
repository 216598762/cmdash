//! Script-widget adapter: wraps an external executable that speaks the
//! [`cmdash_protocol`] line-delimited frame protocol.
//!
//! The script is spawned with piped `stdin`/`stdout`. A reader thread
//! reads frames from stdout; the render loop sends `FRAME` requests
//! and picks up the latest response via a non-blocking channel.
//!
//! ## Lifecycle
//!
//! 1. [`ScriptWidget::spawn`] spawns the child process and a reader
//!    thread.
//! 2. Each [`CmdashWidget::render`] call sends a `FRAME` request to
//!    the child's stdin and renders the latest frame received from the
//!    reader thread.
//! 3. [`CmdashWidget::on_event`] forwards key/resize events as
//!    `KEY`/`RESIZE` messages to stdin.
//! 4. [`Drop`] kills the child and joins the reader thread.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use cmdash_protocol::{FrameResponse, HostMsg};
use cmdash_widget_sdk::{CmdashWidget, KeyCode, WidgetEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use tracing::{debug, warn};

/// Re-export the theme type for convenience.
type Theme = cmdash_config::Theme;

/// A [`CmdashWidget`] backed by an external script process that speaks
/// the [`cmdash_protocol`] wire format.
pub struct ScriptWidget {
    /// The child process (kept alive for the widget's lifetime).
    child: Child,
    /// Buffered writer to the child's stdin.
    stdin: std::io::BufWriter<std::process::ChildStdin>,
    /// Receiver of frames from the reader thread. Non-blocking via
    /// [`mpsc::Receiver::try_recv`].
    frame_rx: Receiver<FrameResponse>,
    /// Handle to the reader thread; joined on [`Drop`].
    reader_handle: Option<JoinHandle<()>>,
    /// Monotonically increasing frame generation counter.
    gen: u64,
    /// Last successfully received frame. Rendered when no fresh frame
    /// is available yet.
    last_frame: FrameResponse,
    /// Human-readable name for logging and the border title.
    name: String,
    /// Last area sent in a FRAME request. Used to skip redundant
    /// requests when the area hasn't changed.
    last_area: (u16, u16),
    /// Theme for border and error colors. Defaults to
    /// `Theme::default()` if not set via [`Self::set_theme`].
    theme: Theme,
}

impl ScriptWidget {
    /// Spawn a script-widget process from a shell-style command string.
    ///
    /// The command is split on whitespace into `argv`; the first element
    /// is the program to exec. Returns an error if the process cannot
    /// be spawned or its stdio cannot be piped.
    pub fn spawn(
        command: &str,
        label: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let parts: Vec<&str> = command.split_whitespace().collect();
        let program = parts.first().ok_or("script command is empty")?;
        let args = &parts[1..];

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn script `{command}`: {e}"))?;

        let stdin = child.stdin.take().ok_or("script has no stdin")?;
        let stdout = child.stdout.take().ok_or("script has no stdout")?;

        let (frame_tx, frame_rx) = mpsc::channel::<FrameResponse>();

        let reader_handle = thread::Builder::new()
            .name("cmdash-script-reader".into())
            .spawn(move || reader_loop(BufReader::new(stdout), frame_tx))
            .expect("spawn script reader thread");

        let name = label.unwrap_or("script").to_string();
        debug!(command, name, "script widget spawned");

        Ok(Self {
            child,
            stdin: std::io::BufWriter::new(stdin),
            frame_rx,
            reader_handle: Some(reader_handle),
            gen: 0,
            last_frame: FrameResponse::default(),
            name,
            last_area: (0, 0),
            theme: Theme::default(),
        })
    }

    /// Write a [`HostMsg`] to the child's stdin. Returns an I/O error
    /// if the write fails (pipe broken — child likely exited).
    fn send_msg(&mut self, msg: &HostMsg) -> std::io::Result<()> {
        writeln!(self.stdin, "{msg}")?;
        self.stdin.flush()
    }

    /// Drain the frame channel and return the latest response, if any.
    fn try_recv_frame(&mut self) -> Option<FrameResponse> {
        let mut latest = None;
        while let Ok(frame) = self.frame_rx.try_recv() {
            latest = Some(frame);
        }
        latest
    }

    /// Update the theme used for border and error colors.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }
}

/// Background thread: reads lines from the script's stdout and sends
/// complete [`FrameResponse`]s to the main thread.
///
/// Protocol: the script outputs `FRAME width=W height=H` followed by
/// ANSI text lines. The reader detects FRAME headers to delimit
/// frames. A `pending` buffer avoids losing a FRAME header that
/// was consumed by `read_line` in the inner loop.
fn reader_loop(mut stdout: BufReader<std::process::ChildStdout>, frame_tx: Sender<FrameResponse>) {
    let mut line = String::new();
    // When the inner loop breaks because it saw a FRAME header,
    // that header has already been consumed by `read_line` into
    // `line`. We save it here so the outer loop can re-process
    // it on the next iteration instead of reading past it.
    let mut pending: Option<String> = None;
    loop {
        if pending.is_none() {
            line.clear();
            match stdout.read_line(&mut line) {
                Ok(0) => {
                    debug!("script reader: EOF");
                    break;
                }
                Err(e) => {
                    warn!(error = %e, "script reader: read error");
                    break;
                }
                _ => {}
            }
        } else {
            line = pending.take().unwrap();
        }
        if !FrameResponse::is_frame_header(&line) {
            continue;
        }
        let mut response = match FrameResponse::parse_header(&line) {
            Some(r) => r,
            None => continue,
        };
        // Read body lines until the next FRAME header or EOF.
        loop {
            line.clear();
            match stdout.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if FrameResponse::is_frame_header(&line) {
                        // Save the consumed header for the outer loop.
                        pending = Some(std::mem::take(&mut line));
                        break;
                    }
                    response.lines.push(line.trim_end_matches('\n').to_string());
                }
                Err(_) => break,
            }
        }
        if frame_tx.send(response).is_err() {
            break; // Receiver dropped — host is exiting.
        }
    }
}

impl CmdashWidget for ScriptWidget {
    fn name(&self) -> &str {
        &self.name
    }

    fn render(&mut self, area: Rect, frame: &mut Frame) {
        // 1. Send a FRAME request only when dimensions change
        //    to avoid flooding the script with identical requests.
        let new_area = (area.width, area.height);
        if new_area != self.last_area {
            self.last_area = new_area;
            self.gen += 1;
            let msg = HostMsg::Frame {
                width: area.width,
                height: area.height,
                gen: self.gen,
            };
            if let Err(e) = self.send_msg(&msg) {
                warn!(error = %e, name = %self.name, "script: failed to send FRAME");
                render_error(area, frame, &self.name, "script process error", &self.theme);
                return;
            }
        }

        // 2. Pick up the latest frame from the reader thread.
        if let Some(response) = self.try_recv_frame() {
            self.last_frame = response;
        }

        // 2.5 Check if the script process has exited.
        if let Ok(Some(exit_status)) = self.child.try_wait() {
            // If the script exited successfully and we have content,
            // show the last frame with an 'exited' indicator.
            if exit_status.success() && !self.last_frame.lines.is_empty() {
                let border_style = Style::default().fg(self.theme.border_color());
                let block = Block::default()
                    .title(format!(" {} [exited] ", self.name))
                    .borders(Borders::ALL)
                    .border_style(border_style);
                let inner = block.inner(area);
                frame.render_widget(block, area);
                if inner.width > 0 && inner.height > 0 {
                    let lines: Vec<Line> = self
                        .last_frame
                        .lines
                        .iter()
                        .take(inner.height as usize)
                        .map(|l| Line::from(Span::raw(l.clone())))
                        .collect();
                    frame.render_widget(Paragraph::new(lines), inner);
                }
            } else {
                render_error(
                    area,
                    frame,
                    &self.name,
                    &format!("script exited: {exit_status}"),
                    &self.theme,
                );
            }
            return;
        }

        // 3. Render the ANSI text lines into the ratatui frame.
        let border_style = Style::default().fg(self.theme.border_color());
        let block = Block::default()
            .title(format!(" {} ", self.name))
            .borders(Borders::ALL)
            .border_style(border_style);

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if self.last_frame.lines.is_empty() {
            // Script hasn't responded yet — show a placeholder.
            let waiting = Paragraph::new(Span::styled(
                "waiting for script...",
                Style::default().fg(self.theme.border_color()),
            ));
            frame.render_widget(waiting, inner);
            return;
        }

        let lines: Vec<Line> = self
            .last_frame
            .lines
            .iter()
            .take(inner.height as usize)
            .map(|l| Line::from(Span::raw(l.clone())))
            .collect();
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    fn on_event(&mut self, event: &WidgetEvent) {
        match event {
            WidgetEvent::Key { code, modifiers } => {
                let key_str = match code {
                    KeyCode::Char(c) => c.to_string(),
                    KeyCode::Enter => "enter".into(),
                    KeyCode::Esc => "esc".into(),
                    KeyCode::Backspace => "backspace".into(),
                    KeyCode::Tab => "tab".into(),
                    KeyCode::Up => "up".into(),
                    KeyCode::Down => "down".into(),
                    KeyCode::Left => "left".into(),
                    KeyCode::Right => "right".into(),
                    KeyCode::Home => "home".into(),
                    KeyCode::End => "end".into(),
                    KeyCode::PageUp => "pageup".into(),
                    KeyCode::PageDown => "pagedown".into(),
                    KeyCode::F(n) => format!("f{n}"),
                };
                let mut mods = Vec::new();
                if modifiers.ctrl {
                    mods.push("ctrl");
                }
                if modifiers.shift {
                    mods.push("shift");
                }
                if modifiers.alt {
                    mods.push("alt");
                }
                if modifiers.super_ {
                    mods.push("super");
                }
                let msg = HostMsg::Key {
                    key: key_str,
                    modifiers: mods.join("+"),
                };
                if let Err(e) = self.send_msg(&msg) {
                    debug!(error = %e, "script: failed to send KEY");
                }
            }
            WidgetEvent::Resize { width, height } => {
                let msg = HostMsg::Resize {
                    width: *width,
                    height: *height,
                };
                if let Err(e) = self.send_msg(&msg) {
                    debug!(error = %e, "script: failed to send RESIZE");
                }
            }
            WidgetEvent::FocusGained => {
                if let Err(e) = self.send_msg(&HostMsg::Focus { gained: true }) {
                    debug!(error = %e, "script: failed to send FOCUS gained");
                }
            }
            WidgetEvent::FocusLost => {
                if let Err(e) = self.send_msg(&HostMsg::Focus { gained: false }) {
                    debug!(error = %e, "script: failed to send FOCUS lost");
                }
            }
        }
    }
}

impl Drop for ScriptWidget {
    fn drop(&mut self) {
        // Best-effort kill before joining the reader so the reader
        // sees EOF promptly.
        let _ = self.child.kill();
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Render an error message inside a bordered block.
/// Uses the theme's `error_color` for the border and text.
fn render_error(
    area: Rect,
    frame: &mut Frame,
    title: &str,
    message: &str,
    theme: &Theme,
) {
    let err_color = theme.error_color();
    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(err_color));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width > 0 && inner.height > 0 {
        let msg = Paragraph::new(Span::styled(
            message,
            Style::default().fg(err_color).add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(msg, inner);
    }
}
