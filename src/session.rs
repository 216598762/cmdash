use std::{
    fmt,
    io::{self, Read, Write},
    sync::mpsc::{self, Receiver},
    thread,
};

use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    term::{Config, Term, cell::Flags},
    vte::ansi::{Color as AnsiColor, NamedColor, Processor},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::layout::Rect;

use crate::scene::{CellStyle, Color, Scene};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
}

impl TerminalSize {
    pub const fn new(columns: u16, rows: u16) -> Self {
        Self { columns, rows }
    }

    fn validate(self) -> Result<Self, SessionError> {
        if self.columns < 2 || self.rows == 0 {
            return Err(SessionError::InvalidSize(self));
        }
        Ok(self)
    }
}

impl Dimensions for TerminalSize {
    fn columns(&self) -> usize {
        self.columns as usize
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn total_lines(&self) -> usize {
        self.rows as usize
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionError {
    InvalidSize(TerminalSize),
    Spawn(String),
    Io(String),
    Resize(String),
    Closed,
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSize(size) => {
                write!(
                    formatter,
                    "invalid terminal size {}x{}",
                    size.columns, size.rows
                )
            }
            Self::Spawn(message) => {
                write!(formatter, "could not spawn terminal session: {message}")
            }
            Self::Io(message) => write!(formatter, "terminal session I/O failed: {message}"),
            Self::Resize(message) => write!(formatter, "terminal session resize failed: {message}"),
            Self::Closed => formatter.write_str("terminal session is closed"),
        }
    }
}

impl std::error::Error for SessionError {}

struct SessionOutput {
    receiver: Receiver<io::Result<Vec<u8>>>,
}

pub struct TerminalSession {
    term: Term<VoidListener>,
    processor: Processor,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    output: SessionOutput,
    size: TerminalSize,
    closed: bool,
    failure: Option<String>,
}

impl TerminalSession {
    pub fn spawn(command: Option<&str>, size: TerminalSize) -> Result<Self, SessionError> {
        Self::spawn_with_args(command, &[], size)
    }

    pub fn spawn_with_args(
        command: Option<&str>,
        args: &[&str],
        size: TerminalSize,
    ) -> Result<Self, SessionError> {
        let size = size.validate()?;
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: size.rows,
                cols: size.columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| SessionError::Spawn(error.to_string()))?;

        let mut command_builder = match command {
            Some(command) => CommandBuilder::new(command),
            None => default_command(),
        };
        command_builder.args(args.iter().copied());
        command_builder.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(command_builder)
            .map_err(|error| SessionError::Spawn(error.to_string()))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| SessionError::Io(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| SessionError::Io(error.to_string()))?;
        let output = SessionOutput {
            receiver: spawn_reader(reader),
        };
        let term = Term::new(Config::default(), &size, VoidListener);

        Ok(Self {
            term,
            processor: Processor::new(),
            master: pair.master,
            writer,
            child,
            output,
            size,
            closed: false,
            failure: None,
        })
    }

    pub const fn size(&self) -> TerminalSize {
        self.size
    }

    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    pub fn poll_output(&mut self) -> Result<bool, SessionError> {
        if self.closed {
            return Ok(false);
        }
        let mut changed = false;
        while let Ok(result) = self.output.receiver.try_recv() {
            match result {
                Ok(bytes) => {
                    self.processor.advance(&mut self.term, &bytes);
                    changed = true;
                }
                Err(error) => {
                    let message = error.to_string();
                    self.failure = Some(message.clone());
                    return Err(SessionError::Io(message));
                }
            }
        }
        Ok(changed)
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), SessionError> {
        if self.closed {
            return Err(SessionError::Closed);
        }
        self.writer
            .write_all(bytes)
            .and_then(|_| self.writer.flush())
            .map_err(|error| SessionError::Io(error.to_string()))
    }

    pub fn write_key(&mut self, key: KeyEvent) -> Result<(), SessionError> {
        let bytes = key_bytes(key)
            .ok_or_else(|| SessionError::Io(format!("unsupported key event {:?}", key.code)))?;
        self.write_bytes(&bytes)
    }

    pub fn resize(&mut self, size: TerminalSize) -> Result<(), SessionError> {
        let size = size.validate()?;
        if self.closed {
            return Err(SessionError::Closed);
        }
        self.master
            .resize(PtySize {
                rows: size.rows,
                cols: size.columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| SessionError::Resize(error.to_string()))?;
        self.term.resize(size);
        self.size = size;
        Ok(())
    }

    pub fn render(&self, area: Rect, focused: bool) -> Scene {
        let mut scene = Scene::new(area);
        let default_style = CellStyle::new(Color::rgb(220, 224, 230), Color::rgb(18, 22, 30));
        scene.fill(area, default_style);
        for indexed in self.term.grid().display_iter() {
            let point = indexed.point;
            let cell = indexed.cell;
            let x = area.x.saturating_add(point.column.0 as u16);
            let y = area.y.saturating_add(point.line.0 as u16);
            if x >= area.x.saturating_add(area.width) || y >= area.y.saturating_add(area.height) {
                continue;
            }
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let mut style = CellStyle::new(
                color_to_scene(cell.fg, Color::rgb(220, 224, 230)),
                color_to_scene(cell.bg, Color::rgb(18, 22, 30)),
            );
            if cell.flags.contains(Flags::BOLD) {
                style = style.bold();
            }
            if cell.flags.contains(Flags::DIM) {
                style = style.dim();
            }
            scene.set(x, y, cell.c, style);
        }
        if focused {
            let cursor = self.term.grid().cursor.point;
            let x = area.x.saturating_add(cursor.column.0 as u16);
            let y = area.y.saturating_add(cursor.line.0 as u16);
            if let Some(cell) = scene.cell_at(x, y).copied() {
                let cursor_style = CellStyle::new(cell.style.background, cell.style.foreground);
                scene.set(x, y, cell.symbol, cursor_style);
            }
        }
        scene
    }

    pub fn shutdown(&mut self) -> Result<(), SessionError> {
        if self.closed {
            return Ok(());
        }
        let kill_result = self.child.kill();
        let wait_result = self.child.wait();
        self.closed = true;
        if let Err(error) = kill_result {
            return Err(SessionError::Io(error.to_string()));
        }
        wait_result
            .map(|_| ())
            .map_err(|error| SessionError::Io(error.to_string()))
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn default_command() -> CommandBuilder {
    if let Some(shell) = std::env::var_os("SHELL") {
        CommandBuilder::new(shell)
    } else {
        CommandBuilder::new("sh")
    }
}

fn spawn_reader(mut reader: Box<dyn Read + Send>) -> Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        loop {
            let mut buffer = vec![0; 4096];
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(length) => {
                    buffer.truncate(length);
                    if sender.send(Ok(buffer)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
        }
    });
    receiver
}

fn key_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(character) if control && character.is_ascii_alphabetic() => {
            Some(vec![character.to_ascii_lowercase() as u8 - b'a' + 1])
        }
        KeyCode::Char(character) => Some(character.to_string().into_bytes()),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(number) if (1..=12).contains(&number) => {
            Some(format!("\x1b[{}~", 10 + number).into_bytes())
        }
        _ => None,
    }
}

fn color_to_scene(color: AnsiColor, fallback: Color) -> Color {
    match color {
        AnsiColor::Spec(rgb) => Color::rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(index) => indexed_color(index),
        AnsiColor::Named(named) => named_color(named).unwrap_or(fallback),
    }
}

fn named_color(color: NamedColor) -> Option<Color> {
    let color = match color {
        NamedColor::Black => (0, 0, 0),
        NamedColor::Red => (205, 49, 49),
        NamedColor::Green => (13, 188, 121),
        NamedColor::Yellow => (229, 229, 16),
        NamedColor::Blue => (36, 114, 200),
        NamedColor::Magenta => (188, 63, 188),
        NamedColor::Cyan => (17, 168, 205),
        NamedColor::White => (229, 229, 229),
        NamedColor::BrightBlack => (102, 102, 102),
        NamedColor::BrightRed => (241, 76, 76),
        NamedColor::BrightGreen => (35, 209, 139),
        NamedColor::BrightYellow => (245, 245, 67),
        NamedColor::BrightBlue => (59, 142, 234),
        NamedColor::BrightMagenta => (214, 112, 214),
        NamedColor::BrightCyan => (41, 184, 219),
        NamedColor::BrightWhite => (255, 255, 255),
        _ => return None,
    };
    Some(Color::rgb(color.0, color.1, color.2))
}

fn indexed_color(index: u8) -> Color {
    const BASIC: [Color; 16] = [
        Color::rgb(0, 0, 0),
        Color::rgb(205, 49, 49),
        Color::rgb(13, 188, 121),
        Color::rgb(229, 229, 16),
        Color::rgb(36, 114, 200),
        Color::rgb(188, 63, 188),
        Color::rgb(17, 168, 205),
        Color::rgb(229, 229, 229),
        Color::rgb(102, 102, 102),
        Color::rgb(241, 76, 76),
        Color::rgb(35, 209, 139),
        Color::rgb(245, 245, 67),
        Color::rgb(59, 142, 234),
        Color::rgb(214, 112, 214),
        Color::rgb(41, 184, 219),
        Color::rgb(255, 255, 255),
    ];
    match index {
        index @ 0..=15 => BASIC[index as usize],
        16..=231 => {
            let index = index as u16 - 16;
            let red = index / 36;
            let green = (index % 36) / 6;
            let blue = index % 6;
            let channel = |value: u16| if value == 0 { 0 } else { 55 + 40 * value };
            Color::rgb(
                channel(red) as u8,
                channel(green) as u8,
                channel(blue) as u8,
            )
        }
        232..=255 => {
            let value = 8 + (index as u16 - 232) * 10;
            Color::rgb(value as u8, value as u8, value as u8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for_output(session: &mut TerminalSession) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if session.poll_output().unwrap() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("terminal output did not arrive");
    }

    #[test]
    fn terminal_session_parses_pty_output_into_the_emulator() {
        let mut session = TerminalSession::spawn_with_args(
            Some("sh"),
            &["-c", "printf 'hello\\n'; sleep 5"],
            TerminalSize::new(40, 8),
        )
        .unwrap();
        wait_for_output(&mut session);
        let scene = session.render(Rect::new(0, 0, 40, 8), false);

        let rendered: String = (0..5)
            .map(|column| scene.cell_at(column, 0).unwrap().symbol)
            .collect();
        assert_eq!(rendered, "hello");
        session.shutdown().unwrap();
    }

    #[test]
    fn terminal_session_resizes_and_rejects_invalid_sizes() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(40, 8)).unwrap();

        session.resize(TerminalSize::new(60, 12)).unwrap();
        assert_eq!(session.size(), TerminalSize::new(60, 12));
        assert_eq!(
            session.resize(TerminalSize::new(1, 0)),
            Err(SessionError::InvalidSize(TerminalSize::new(1, 0)))
        );
        session.shutdown().unwrap();
    }

    #[test]
    fn key_encoding_covers_text_control_and_navigation_input() {
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![3])
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(vec![b'\r'])
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
    }
}
