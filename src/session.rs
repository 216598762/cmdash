use std::{
    fmt,
    io::{self, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

use alacritty_terminal::{
    event::{Event, EventListener, WindowSize as EmulatorWindowSize},
    grid::Dimensions,
    term::{Config, Term, TermMode, cell::Flags},
    vte::ansi::{Color as AnsiColor, NamedColor, Processor},
};

use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::layout::Rect;

use crate::{
    appearance::Theme,
    graphics::{
        GraphicsProtocolBroker, GraphicsSubmission, SessionGraphicsStore, kitty_error_response,
    },
    scene::{CellStyle, Color, Scene},
    state::SessionId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl TerminalSize {
    pub const fn new(columns: u16, rows: u16) -> Self {
        Self::with_pixels(columns, rows, 0, 0)
    }

    pub const fn with_pixels(columns: u16, rows: u16, pixel_width: u16, pixel_height: u16) -> Self {
        Self {
            columns,
            rows,
            pixel_width,
            pixel_height,
        }
    }

    pub fn cell_width(self) -> u16 {
        self.pixel_width.checked_div(self.columns).unwrap_or(0)
    }

    pub fn cell_height(self) -> u16 {
        self.pixel_height.checked_div(self.rows).unwrap_or(0)
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
    Graphics(String),
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
            Self::Graphics(message) => {
                write!(formatter, "Kitty graphics parsing failed: {message}")
            }
        }
    }
}

impl std::error::Error for SessionError {}

#[derive(Debug)]
pub enum UiEvent {
    Input(CrosstermEvent),
    PtyOutput,
    Tick,
    AnimationFrame,
    ApiWakeup,
    CursorBlink(u64),
    InputError(String),
}

/// A coalescing wakeup shared by terminal PTY readers and the UI coordinator.
#[derive(Clone)]
pub struct SessionWakeup {
    sender: Sender<UiEvent>,
    pending: Arc<AtomicBool>,
}

impl SessionWakeup {
    fn notify(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.sender.send(UiEvent::PtyOutput);
        }
    }

    pub fn clear_pending(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

pub fn ui_event_channel() -> (Sender<UiEvent>, Receiver<UiEvent>, SessionWakeup) {
    let (sender, receiver) = mpsc::channel();
    let wakeup = SessionWakeup {
        sender: sender.clone(),
        pending: Arc::new(AtomicBool::new(false)),
    };
    (sender, receiver, wakeup)
}

struct SessionOutput {
    receiver: Receiver<io::Result<Vec<u8>>>,
}

/// Receives terminal-emulator requests that must be sent back to the child PTY.
///
/// Shells use these requests for capability negotiation. In particular, fish
/// sends a Primary Device Attribute query during startup and waits for the
/// emulator's response before continuing.
struct SessionEventListener {
    pty_writer: Sender<String>,
    size: Arc<Mutex<TerminalSize>>,
}

impl SessionEventListener {
    fn send_to_pty(&self, text: String) {
        // The session drains this session-owned channel after feeding
        // output to the emulator. A disconnected receiver only
        // means the session is shutting down, so there is nothing useful to
        // do with the response.
        let _ = self.pty_writer.send(text);
    }
}

impl EventListener for SessionEventListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self.send_to_pty(normalize_emulator_response(text)),
            Event::TextAreaSizeRequest(format) => {
                let size = *self.size.lock().expect("terminal size mutex poisoned");
                self.send_to_pty(format(EmulatorWindowSize {
                    num_lines: size.rows,
                    num_cols: size.columns,
                    cell_width: size.cell_width(),
                    cell_height: size.cell_height(),
                }));
            }
            _ => {}
        }
    }
}

fn normalize_emulator_response(text: String) -> String {
    // alacritty_terminal's default DA1 response is the short `CSI ? 6 c`.
    // kitty's icat detector requires a parameterized DA1 response, so expose
    // the equivalent standard `CSI ? 1 ; 2 c` identity to PTY applications.
    if text == "\x1b[?6c" {
        "\x1b[?1;2c".to_owned()
    } else {
        text
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Selection {
    anchor: (u16, u16),
    active: (u16, u16),
}

pub struct TerminalSession {
    term: Term<SessionEventListener>,
    processor: Processor,
    emulator_responses: Receiver<String>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    output: SessionOutput,
    size: TerminalSize,
    reported_size: Arc<Mutex<TerminalSize>>,
    closed: bool,
    failure: Option<String>,
    graphics: SessionGraphicsStore,
    graphics_broker: GraphicsProtocolBroker,
    graphics_input: Vec<u8>,
    selection: Option<Selection>,
}

impl TerminalSession {
    pub fn spawn(command: Option<&str>, size: TerminalSize) -> Result<Self, SessionError> {
        Self::spawn_with_session_id(allocate_session_id(), command, &[], size)
    }

    pub fn spawn_with_args(
        command: Option<&str>,
        args: &[&str],
        size: TerminalSize,
    ) -> Result<Self, SessionError> {
        Self::spawn_with_session_id(allocate_session_id(), command, args, size)
    }

    pub fn spawn_with_session_id(
        session_id: SessionId,
        command: Option<&str>,
        args: &[&str],
        size: TerminalSize,
    ) -> Result<Self, SessionError> {
        Self::spawn_with_session_id_and_wakeup(session_id, command, args, size, None)
    }

    pub fn spawn_with_session_id_and_wakeup(
        session_id: SessionId,
        command: Option<&str>,
        args: &[&str],
        size: TerminalSize,
        wakeup: Option<SessionWakeup>,
    ) -> Result<Self, SessionError> {
        let size = size.validate()?;
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: size.rows,
                cols: size.columns,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
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
            receiver: spawn_reader(reader, wakeup),
        };
        let (response_sender, response_receiver) = mpsc::channel();
        let reported_size = Arc::new(Mutex::new(size));
        let term = Term::new(
            Config::default(),
            &size,
            SessionEventListener {
                pty_writer: response_sender,
                size: Arc::clone(&reported_size),
            },
        );

        Ok(Self {
            term,
            processor: Processor::new(),
            emulator_responses: response_receiver,
            master: pair.master,
            writer,
            child,
            output,
            size,
            reported_size,
            closed: false,
            failure: None,
            graphics: SessionGraphicsStore::new(session_id),
            graphics_broker: GraphicsProtocolBroker::default(),
            graphics_input: Vec::new(),
            selection: None,
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

    pub fn session_id(&self) -> SessionId {
        self.graphics.session()
    }

    pub fn graphics(&self, surface: Rect) -> Vec<GraphicsSubmission> {
        self.graphics
            .visible_submissions_at(surface, self.scrollback_lines())
    }

    pub fn graphics_diagnostics(&self) -> &[crate::graphics::GraphicsDiagnostic] {
        self.graphics.diagnostics()
    }

    pub fn set_kitty_graphics_support(&mut self, supported: bool) {
        self.graphics.set_outer_kitty_graphics(supported);
    }

    pub fn begin_selection(&mut self, position: (u16, u16)) {
        self.selection = Some(Selection {
            anchor: position,
            active: position,
        });
    }

    pub fn update_selection(&mut self, position: (u16, u16)) {
        if let Some(selection) = &mut self.selection {
            selection.active = position;
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn selected_text(&self, area: Rect) -> Option<String> {
        let selection = self.selection?;
        if selection.anchor == selection.active {
            return None;
        }
        let scene = self.render(area, false);
        let left = selection.anchor.0.min(selection.active.0);
        let right = selection.anchor.0.max(selection.active.0);
        let top = selection.anchor.1.min(selection.active.1);
        let bottom = selection.anchor.1.max(selection.active.1);
        let mut lines = Vec::new();
        for row in top..=bottom {
            let mut line = String::new();
            for column in left..=right {
                let x = area.x.saturating_add(column);
                let y = area.y.saturating_add(row);
                if let Some(cell) = scene.cell_at(x, y)
                    && cell.width != crate::scene::CellWidth::Continuation
                {
                    line.push(cell.symbol);
                }
            }
            lines.push(line.trim_end().to_owned());
        }
        Some(lines.join("\n"))
    }

    pub fn poll_output(&mut self) -> Result<bool, SessionError> {
        if self.closed {
            return Ok(false);
        }
        let mut changed = false;
        while let Ok(result) = self.output.receiver.try_recv() {
            match result {
                Ok(bytes) => {
                    changed = self.consume_output(&bytes)? || changed || !bytes.is_empty();
                }
                Err(error) => {
                    let message = error.to_string();
                    self.failure = Some(message.clone());
                    return Err(SessionError::Io(message));
                }
            }
        }
        if self
            .child
            .try_wait()
            .map_err(|error| SessionError::Io(error.to_string()))?
            .is_some()
        {
            self.closed = true;
        }
        Ok(changed)
    }

    fn consume_output(&mut self, bytes: &[u8]) -> Result<bool, SessionError> {
        self.graphics_input.extend_from_slice(bytes);
        let (events, remainder) = extract_kitty_events(&self.graphics_input);
        self.graphics_input = remainder;
        let mut changed = false;
        for event in events {
            match event {
                KittyStreamEvent::Plain(plain) => {
                    if !plain.is_empty() {
                        self.processor.advance(&mut self.term, &plain);
                        self.flush_emulator_responses()?;
                        changed = true;
                    }
                }
                KittyStreamEvent::Command(parameters, payload) => {
                    let response = match self.graphics.apply_kitty_command_with_grid_context(
                        &parameters,
                        &payload,
                        self.cursor_position(),
                        (self.size.cell_width(), self.size.cell_height()),
                        self.scrollback_lines(),
                    ) {
                        Ok(response) => response,
                        Err(error) => {
                            let image = parameters
                                .split(|byte| *byte == b',')
                                .find_map(|parameter| parameter.strip_prefix(b"i="))
                                .and_then(|value| std::str::from_utf8(value).ok())
                                .and_then(|value| value.parse::<u32>().ok());
                            self.graphics.record_diagnostic(image, error.to_string());
                            image
                                .filter(|image| *image != 0)
                                .map(|_| kitty_error_response(&parameters, &error))
                        }
                    };
                    if let Some(response) = response
                        && !self.graphics_broker.queue_child(response)
                    {
                        self.graphics.record_diagnostic(
                            None,
                            "child graphics response queue is full; response was dropped",
                        );
                    }
                    changed = true;
                }
            }
        }
        self.flush_emulator_responses()?;
        Ok(changed)
    }

    fn flush_emulator_responses(&mut self) -> Result<(), SessionError> {
        let mut response = Vec::new();
        while let Ok(text) = self.emulator_responses.try_recv() {
            response.extend_from_slice(text.as_bytes());
        }
        if !response.is_empty() && !self.graphics_broker.queue_child(response) {
            self.graphics.record_diagnostic(
                None,
                "child emulator response queue is full; response was dropped",
            );
        }
        for queued in self.graphics_broker.drain_child() {
            self.writer
                .write_all(queued.bytes())
                .and_then(|_| self.writer.flush())
                .map_err(|error| SessionError::Io(error.to_string()))?;
        }
        Ok(())
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

    pub fn write_paste(&mut self, text: &str) -> Result<(), SessionError> {
        let bytes = paste_bytes(text, self.term.mode().contains(TermMode::BRACKETED_PASTE));
        self.write_bytes(&bytes)
    }

    pub fn write_mouse(
        &mut self,
        mouse: MouseEvent,
        origin: (u16, u16),
    ) -> Result<(), SessionError> {
        let bytes = mouse_bytes(mouse, origin)
            .ok_or_else(|| SessionError::Io("unsupported mouse event".to_owned()))?;
        self.write_bytes(&bytes)
    }

    pub fn cursor_position(&self) -> (u16, u16) {
        let cursor = self.term.grid().cursor.point;
        (cursor.column.0 as u16, cursor.line.0 as u16)
    }

    pub fn alternate_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    pub fn scrollback_lines(&self) -> usize {
        self.term.grid().history_size()
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
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
            })
            .map_err(|error| SessionError::Resize(error.to_string()))?;
        self.term.resize(size);
        *self
            .reported_size
            .lock()
            .expect("terminal size mutex poisoned") = size;
        self.size = size;
        Ok(())
    }

    pub fn render(&self, area: Rect, focused: bool) -> Scene {
        self.render_with_theme(area, focused, Theme::fallback())
    }

    pub fn render_with_theme(&self, area: Rect, focused: bool, theme: Theme) -> Scene {
        self.render_with_theme_and_cursor(area, focused, theme, true)
    }

    pub fn render_with_theme_and_cursor(
        &self,
        area: Rect,
        focused: bool,
        theme: Theme,
        cursor_visible: bool,
    ) -> Scene {
        let mut scene = Scene::new(area);
        let default_style = CellStyle::new(theme.foreground(), theme.background());
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
                color_to_scene(cell.fg, theme.foreground()),
                color_to_scene(cell.bg, theme.background()),
            );
            if cell.flags.contains(Flags::BOLD) {
                style = style.bold();
            }
            if cell.flags.contains(Flags::DIM) {
                style = style.dim();
            }
            scene.set(x, y, cell.c, style);
        }
        if focused && cursor_visible && self.term.mode().contains(TermMode::SHOW_CURSOR) {
            let cursor = self.term.grid().cursor.point;
            let x = area.x.saturating_add(cursor.column.0 as u16);
            let y = area.y.saturating_add(cursor.line.0 as u16);
            if let Some(cell) = scene.cell_at(x, y).copied() {
                let cursor_style = CellStyle::new(cell.style.background, cell.style.foreground);
                scene.set(x, y, cell.symbol, cursor_style);
            }
        }
        if let Some(selection) = self.selection {
            let left = selection.anchor.0.min(selection.active.0);
            let right = selection.anchor.0.max(selection.active.0);
            let top = selection.anchor.1.min(selection.active.1);
            let bottom = selection.anchor.1.max(selection.active.1);
            for row in top..=bottom {
                for column in left..=right {
                    let x = area.x.saturating_add(column);
                    let y = area.y.saturating_add(row);
                    if let Some(cell) = scene.cell_at(x, y).copied() {
                        let selected_style =
                            CellStyle::new(cell.style.background, cell.style.foreground);
                        scene.set(x, y, cell.symbol, selected_style);
                    }
                }
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

fn allocate_session_id() -> SessionId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
    SessionId::new(NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed))
}

type KittyCommand = (Vec<u8>, Vec<u8>);
type KittyExtraction = (Vec<u8>, Vec<KittyCommand>, Vec<u8>);

enum KittyStreamEvent {
    Plain(Vec<u8>),
    Command(Vec<u8>, Vec<u8>),
}

/// Returns `(plain_bytes, kitty_commands, pending_bytes)` for parser stress tooling.
///
/// The parser itself remains session-owned; this bounded summary lets fuzz
/// targets exercise chunking and terminator handling without constructing a PTY.
pub fn kitty_stream_stats(buffer: &[u8]) -> (usize, usize, usize) {
    let (plain, commands, remainder) = extract_kitty_commands(buffer);
    (plain.len(), commands.len(), remainder.len())
}

fn extract_kitty_commands(buffer: &[u8]) -> KittyExtraction {
    let (events, remainder) = extract_kitty_events(buffer);
    let mut plain = Vec::new();
    let mut commands = Vec::new();
    for event in events {
        match event {
            KittyStreamEvent::Plain(bytes) => plain.extend(bytes),
            KittyStreamEvent::Command(parameters, payload) => commands.push((parameters, payload)),
        }
    }
    (plain, commands, remainder)
}

fn extract_kitty_events(buffer: &[u8]) -> (Vec<KittyStreamEvent>, Vec<u8>) {
    const PREFIX: &[u8] = b"\x1b_G";
    const TERMINATOR: &[u8] = b"\x1b\\";
    let mut events = Vec::new();
    let mut index = 0;
    while index < buffer.len() {
        let Some(prefix_offset) = buffer[index..]
            .windows(PREFIX.len())
            .position(|window| window == PREFIX)
        else {
            let suffix_len = partial_prefix_suffix_len(&buffer[index..], PREFIX);
            let plain_end = buffer.len().saturating_sub(suffix_len);
            if plain_end > index {
                events.push(KittyStreamEvent::Plain(buffer[index..plain_end].to_vec()));
            }
            return (events, buffer[plain_end..].to_vec());
        };
        let prefix = index + prefix_offset;
        if prefix > index {
            events.push(KittyStreamEvent::Plain(buffer[index..prefix].to_vec()));
        }
        let command_start = prefix + PREFIX.len();
        let Some(terminator_offset) = find_bytes(&buffer[command_start..], TERMINATOR) else {
            return (events, buffer[prefix..].to_vec());
        };
        let end = command_start + terminator_offset;
        let Some(separator) = buffer[command_start..end]
            .iter()
            .position(|byte| *byte == b';')
        else {
            events.push(KittyStreamEvent::Plain(
                buffer[prefix..end + TERMINATOR.len()].to_vec(),
            ));
            index = end + TERMINATOR.len();
            continue;
        };
        events.push(KittyStreamEvent::Command(
            buffer[command_start..command_start + separator].to_vec(),
            buffer[command_start + separator + 1..end].to_vec(),
        ));
        index = end + TERMINATOR.len();
    }
    (events, Vec::new())
}

fn partial_prefix_suffix_len(buffer: &[u8], prefix: &[u8]) -> usize {
    (1..prefix.len().min(buffer.len() + 1))
        .rev()
        .find(|length| buffer.ends_with(&prefix[..*length]))
        .unwrap_or(0)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn default_command() -> CommandBuilder {
    if let Some(shell) = std::env::var_os("SHELL") {
        CommandBuilder::new(shell)
    } else {
        CommandBuilder::new("sh")
    }
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    wakeup: Option<SessionWakeup>,
) -> Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        loop {
            let mut buffer = vec![0; 4096];
            match reader.read(&mut buffer) {
                Ok(0) => {
                    if let Some(wakeup) = &wakeup {
                        wakeup.notify();
                    }
                    break;
                }
                Ok(length) => {
                    buffer.truncate(length);
                    if sender.send(Ok(buffer)).is_err() {
                        break;
                    }
                    if let Some(wakeup) = &wakeup {
                        wakeup.notify();
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    if let Some(wakeup) = &wakeup {
                        wakeup.notify();
                    }
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

fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

fn mouse_bytes(mouse: MouseEvent, origin: (u16, u16)) -> Option<Vec<u8>> {
    let x = mouse.column.saturating_sub(origin.0).saturating_add(1);
    let y = mouse.row.saturating_sub(origin.1).saturating_add(1);
    let modifiers = mouse.modifiers;
    let modifier_bits = u16::from(modifiers.contains(KeyModifiers::SHIFT)) * 4
        + u16::from(modifiers.contains(KeyModifiers::ALT)) * 8
        + u16::from(modifiers.contains(KeyModifiers::CONTROL)) * 16;
    let (button, suffix) = match mouse.kind {
        MouseEventKind::Down(button) => (mouse_button(button) + modifier_bits, 'M'),
        MouseEventKind::Up(button) => (mouse_button(button) + modifier_bits, 'm'),
        MouseEventKind::Drag(button) => (32 + mouse_button(button) + modifier_bits, 'M'),
        MouseEventKind::Moved => (35 + modifier_bits, 'M'),
        MouseEventKind::ScrollUp => (64 + modifier_bits, 'M'),
        MouseEventKind::ScrollDown => (65 + modifier_bits, 'M'),
        MouseEventKind::ScrollLeft => (66 + modifier_bits, 'M'),
        MouseEventKind::ScrollRight => (67 + modifier_bits, 'M'),
    };
    Some(format!("\x1b[<{button};{x};{y}{suffix}").into_bytes())
}

fn mouse_button(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
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
    let index = match color {
        NamedColor::Black => 0,
        NamedColor::Red => 1,
        NamedColor::Green => 2,
        NamedColor::Yellow => 3,
        NamedColor::Blue => 4,
        NamedColor::Magenta => 5,
        NamedColor::Cyan => 6,
        NamedColor::White => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        _ => return None,
    };
    Some(Color::ansi(index))
}

fn indexed_color(index: u8) -> Color {
    match index {
        index @ 0..=15 => Color::ansi(index),
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
    fn pty_wakeup_notifications_are_coalesced_until_consumed() {
        let (_sender, receiver, wakeup) = ui_event_channel();
        wakeup.notify();
        wakeup.notify();

        assert!(matches!(receiver.try_recv(), Ok(UiEvent::PtyOutput)));
        assert!(receiver.try_recv().is_err());

        wakeup.clear_pending();
        wakeup.notify();
        assert!(matches!(receiver.try_recv(), Ok(UiEvent::PtyOutput)));
    }

    #[test]
    fn cursor_visibility_can_be_toggled_without_changing_terminal_state() {
        let mut session = TerminalSession::spawn_with_args(
            Some("sh"),
            &["-c", "sleep 5"],
            TerminalSize::new(20, 4),
        )
        .unwrap();
        session.processor.advance(&mut session.term, b"\x1b[?25h");

        let visible = session.render_with_theme_and_cursor(
            Rect::new(0, 0, 20, 4),
            true,
            Theme::fallback(),
            true,
        );
        let hidden = session.render_with_theme_and_cursor(
            Rect::new(0, 0, 20, 4),
            true,
            Theme::fallback(),
            false,
        );
        assert_ne!(
            visible.cell_at(0, 0).unwrap().style,
            hidden.cell_at(0, 0).unwrap().style
        );
        assert_eq!(session.cursor_position(), (0, 0));
        session.shutdown().unwrap();
    }

    #[test]
    fn primary_device_attribute_queries_are_answered_to_the_child_pty() {
        let mut session = TerminalSession::spawn_with_args(
            Some("sh"),
            &[
                "-c",
                "stty -icanon min 1 time 0; printf '\\033[c'; response=$(dd bs=1 count=7 2>/dev/null); if [ \"$response\" = \"$(printf '\\033[?1;2c')\" ]; then printf 'primary-ok'; fi; sleep 5",
            ],
            TerminalSize::new(40, 8),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut primary_ok = false;
        while Instant::now() < deadline {
            session.poll_output().unwrap();
            let scene = session.render(Rect::new(0, 0, 40, 8), false);
            let rendered: String = scene.cells().iter().map(|cell| cell.symbol).collect();
            if rendered.contains("primary-ok") {
                primary_ok = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(primary_ok, "child did not receive the DA1 response");
        session.shutdown().unwrap();
    }

    #[test]
    fn kitty_graphics_queries_are_answered_before_terminal_queries() {
        let mut session = TerminalSession::spawn_with_args(
            Some("sh"),
            &[
                "-c",
                "stty -icanon min 1 time 0; printf '\\033_Ga=q,i=7,t=d,s=1,v=1,f=24;MTIz\\033\\\\\\033[c'; response=$(dd bs=1 count=11 2>/dev/null); da=$(dd bs=1 count=7 2>/dev/null); if [ \"$response\" = \"$(printf '\\033_Gi=7;OK\\033\\\\')\" ] && [ \"$da\" = \"$(printf '\\033[?1;2c')\" ]; then printf 'kitty-query-ok'; fi; sleep 5",
            ],
            TerminalSize::new(40, 8),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut query_ok = false;
        while Instant::now() < deadline {
            session.poll_output().unwrap();
            let scene = session.render(Rect::new(0, 0, 40, 8), false);
            let rendered: String = scene.cells().iter().map(|cell| cell.symbol).collect();
            if rendered.contains("kitty-query-ok") {
                query_ok = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(query_ok, "child did not receive Kitty and DA responses");
        session.shutdown().unwrap();
    }

    #[test]
    fn pixel_size_queries_are_formatted_from_current_session_geometry() {
        let mut session =
            TerminalSession::spawn(Some("sh"), TerminalSize::with_pixels(40, 8, 400, 160)).unwrap();
        assert_eq!(
            session.master.get_size().unwrap(),
            PtySize {
                rows: 8,
                cols: 40,
                pixel_width: 400,
                pixel_height: 160,
            }
        );
        session.processor.advance(&mut session.term, b"\x1b[14t");

        assert_eq!(
            session.emulator_responses.try_recv().unwrap(),
            "\x1b[4;160;400t"
        );
        session.shutdown().unwrap();
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
    fn separate_sessions_retain_independent_terminal_state() {
        let mut first = TerminalSession::spawn_with_args(
            Some("sh"),
            &["-c", "printf 'first'; sleep 5"],
            TerminalSize::new(20, 4),
        )
        .unwrap();
        let mut second = TerminalSession::spawn_with_args(
            Some("sh"),
            &["-c", "printf 'second'; sleep 5"],
            TerminalSize::new(20, 4),
        )
        .unwrap();
        wait_for_output(&mut first);
        wait_for_output(&mut second);

        assert_eq!(
            first
                .render(Rect::new(0, 0, 20, 4), false)
                .cell_at(0, 0)
                .unwrap()
                .symbol,
            'f'
        );
        assert_eq!(
            second
                .render(Rect::new(0, 0, 20, 4), false)
                .cell_at(0, 0)
                .unwrap()
                .symbol,
            's'
        );
        first.shutdown().unwrap();
        second.shutdown().unwrap();
    }

    #[test]
    fn exited_processes_are_reported_without_a_forced_kill() {
        let mut session = TerminalSession::spawn_with_args(
            Some("sh"),
            &["-c", "exit 0"],
            TerminalSize::new(20, 4),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !session.is_closed() && Instant::now() < deadline {
            let _ = session.poll_output().unwrap();
            thread::sleep(Duration::from_millis(10));
        }
        assert!(session.is_closed());
        session.shutdown().unwrap();
    }

    #[test]
    fn terminal_session_resizes_and_rejects_invalid_sizes() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(40, 8)).unwrap();

        session
            .resize(TerminalSize::with_pixels(60, 12, 600, 240))
            .unwrap();
        assert_eq!(session.size(), TerminalSize::with_pixels(60, 12, 600, 240));
        assert_eq!(
            session.resize(TerminalSize::new(1, 0)),
            Err(SessionError::InvalidSize(TerminalSize::new(1, 0)))
        );
        session.shutdown().unwrap();
        assert!(session.is_closed());
    }

    #[test]
    fn terminal_modes_and_styles_are_reported_from_emulator_output() {
        let mut session = TerminalSession::spawn_with_args(
            Some("sh"),
            &[
                "-c",
                "printf '\\033[?1049h\\033[31mred'; read value; printf '\\033[?1049lMAIN'; sleep 5",
            ],
            TerminalSize::new(40, 8),
        )
        .unwrap();
        wait_for_output(&mut session);
        let scene = session.render(Rect::new(0, 0, 40, 8), false);

        assert!(session.alternate_screen());
        assert_eq!(session.cursor_position(), (3, 0));
        assert_eq!(scene.cell_at(0, 0).unwrap().symbol, 'r');
        assert_eq!(
            scene.cell_at(0, 0).unwrap().style.foreground,
            Color::ansi(1)
        );
        session.write_bytes(b"\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while session.alternate_screen() && Instant::now() < deadline {
            let _ = session.poll_output().unwrap();
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!session.alternate_screen());
        let main_scene = session.render(Rect::new(0, 0, 40, 8), false);
        assert_eq!(main_scene.cell_at(0, 0).unwrap().symbol, 'M');
        session.shutdown().unwrap();

        let mut scrollback = TerminalSession::spawn_with_args(
            Some("sh"),
            &["-c", "yes x | head -n 20; sleep 5"],
            TerminalSize::new(40, 8),
        )
        .unwrap();
        wait_for_output(&mut scrollback);
        assert!(scrollback.scrollback_lines() > 0);
        scrollback.shutdown().unwrap();
    }

    #[test]
    fn selection_tracks_dragged_cells_and_copies_visible_text() {
        let mut session = TerminalSession::spawn_with_args(
            Some("sh"),
            &["-c", "printf 'copy me'; sleep 5"],
            TerminalSize::new(20, 4),
        )
        .unwrap();
        wait_for_output(&mut session);
        let area = Rect::new(0, 0, 20, 4);
        session.begin_selection((0, 0));
        session.update_selection((6, 0));

        assert_eq!(session.selected_text(area).as_deref(), Some("copy me"));
        session.clear_selection();
        assert_eq!(session.selected_text(area), None);
        session.shutdown().unwrap();
    }

    #[test]
    fn paste_and_mouse_encoding_use_terminal_protocol_sequences() {
        assert_eq!(
            mouse_bytes(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 4,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                },
                (2, 3),
            ),
            Some(b"\x1b[<0;3;3M".to_vec())
        );
        assert_eq!(
            paste_bytes("paste", true),
            b"\x1b[200~paste\x1b[201~".to_vec()
        );
        assert_eq!(paste_bytes("paste", false), b"paste".to_vec());
    }

    #[test]
    fn kitty_apc_commands_are_removed_from_text_and_survive_chunk_boundaries() {
        let first = b"before\x1b_Ga=T,f=24,i=1;AQ";
        let second = b"ID\x1b\\after";
        let (plain_first, commands_first, remainder) = extract_kitty_commands(first);
        assert_eq!(plain_first, b"before");
        assert!(commands_first.is_empty());
        assert_eq!(remainder, first[6..]);

        let mut combined = remainder;
        combined.extend_from_slice(second);
        let (plain_second, commands_second, remainder) = extract_kitty_commands(&combined);
        assert_eq!(plain_second, b"after");
        assert_eq!(
            commands_second,
            vec![(b"a=T,f=24,i=1".to_vec(), b"AQID".to_vec())]
        );
        assert!(remainder.is_empty());
    }

    #[test]
    fn parser_handles_repeated_graphics_and_text_without_unbounded_pending_state() {
        let mut input = Vec::new();
        for index in 1..=256u32 {
            input.extend_from_slice(b"text");
            input.extend_from_slice(format!("\x1b_Ga=T,f=24,i={index};AQID\x1b\\").as_bytes());
        }
        let (plain, commands, remainder) = extract_kitty_commands(&input);
        assert!(remainder.is_empty());
        assert_eq!(plain.len(), 256 * 4);
        assert_eq!(commands.len(), 256);
        assert_eq!(commands[0].0, b"a=T,f=24,i=1");
        assert_eq!(commands[255].0, b"a=T,f=24,i=256");
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
