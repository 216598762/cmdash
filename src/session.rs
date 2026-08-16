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
    vte::ansi::{Color as AnsiColor, Handler, Mode, NamedColor, PrivateMode, Processor},
};

use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

const MAX_GRAPHICS_PROTOCOL_CAPTURE_BYTES: usize = 256 * 1024;

use crate::{
    appearance::Theme,
    graphics::{
        GraphicsProtocolAdapter, GraphicsProtocolBroker, GraphicsProtocolEvent, GraphicsScreen,
        GraphicsScrollRegion, GraphicsSubmission, SessionGraphicsStore, kitty_error_response,
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
    /// Bytes classified by the process-wide raw-input owner as outer-terminal
    /// graphics responses, before crossterm's event decoder sees them.
    OuterInput(Vec<u8>),
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
struct ScrollScreenState {
    region: GraphicsScrollRegion,
    region_scroll: i64,
    cursor: (u16, u16),
    input_needs_wrap: bool,
    origin: bool,
    linefeed_newline: bool,
    line_wrap: bool,
}

impl ScrollScreenState {
    const fn new(_columns: u16, rows: u16) -> Self {
        Self {
            region: GraphicsScrollRegion::new(0, rows, rows),
            region_scroll: 0,
            cursor: (0, 0),
            input_needs_wrap: false,
            origin: false,
            linefeed_newline: false,
            line_wrap: true,
        }
    }

    fn reset_region_cursor(&mut self) {
        self.cursor = (0, if self.origin { self.region.top() } else { 0 });
        self.input_needs_wrap = false;
    }

    fn scroll_up(&mut self, lines: usize) {
        let height = usize::from(self.region.bottom().saturating_sub(self.region.top()));
        self.region_scroll = self
            .region_scroll
            .saturating_add(i64::try_from(lines.min(height)).unwrap_or(i64::MAX));
    }

    fn scroll_down(&mut self, lines: usize) {
        let height = usize::from(self.region.bottom().saturating_sub(self.region.top()));
        self.region_scroll = self
            .region_scroll
            .saturating_sub(i64::try_from(lines.min(height)).unwrap_or(i64::MAX));
    }

    fn linefeed(&mut self, rows: u16) {
        let next = self.cursor.1.saturating_add(1);
        if self.cursor.1 >= self.region.top() && next == self.region.bottom() {
            self.scroll_up(1);
        } else if next < rows {
            self.cursor.1 = next;
        }
        self.input_needs_wrap = false;
    }

    fn carriage_return(&mut self) {
        self.cursor.0 = 0;
        self.input_needs_wrap = false;
    }
}

/// Observes the same VT stream as alacritty-terminal for the state it keeps
/// private: DECSTBM margins and the scroll displacement caused by them.
///
/// The emulator remains the source of truth for rendered cells. This bounded
/// observer exists only so retained graphics can resolve their logical anchors
/// without depending on private emulator fields.
#[derive(Clone, Debug)]
struct ScrollRegionTracker {
    columns: u16,
    rows: u16,
    active: GraphicsScreen,
    primary: ScrollScreenState,
    alternate: ScrollScreenState,
}

impl ScrollRegionTracker {
    const fn new(columns: u16, rows: u16) -> Self {
        Self {
            columns,
            rows,
            active: GraphicsScreen::Primary,
            primary: ScrollScreenState::new(columns, rows),
            alternate: ScrollScreenState::new(columns, rows),
        }
    }

    fn current(&self) -> ScrollScreenState {
        match self.active {
            GraphicsScreen::Primary => self.primary,
            GraphicsScreen::Alternate => self.alternate,
        }
    }

    fn current_region(&self) -> GraphicsScrollRegion {
        self.current().region
    }

    fn current_region_scroll(&self) -> i64 {
        self.current().region_scroll
    }

    fn active_screen(&self) -> GraphicsScreen {
        self.active
    }

    fn current_mut(&mut self) -> &mut ScrollScreenState {
        match self.active {
            GraphicsScreen::Primary => &mut self.primary,
            GraphicsScreen::Alternate => &mut self.alternate,
        }
    }

    fn resize(&mut self, columns: u16, rows: u16) {
        self.columns = columns;
        self.rows = rows;
        self.primary = ScrollScreenState::new(columns, rows);
        self.alternate = ScrollScreenState::new(columns, rows);
    }

    fn switch_screen(&mut self, screen: GraphicsScreen) {
        self.active = screen;
    }

    fn move_cursor(&mut self, line: i32, column: usize) {
        let rows = self.rows.max(1);
        let columns = self.columns;
        let state = self.current_mut();
        let (top, bottom) = if state.origin {
            (state.region.top(), state.region.bottom().saturating_sub(1))
        } else {
            (0, rows.saturating_sub(1))
        };
        let line = line.clamp(i32::from(top), i32::from(bottom));
        state.cursor = (
            u16::try_from(column)
                .unwrap_or(u16::MAX)
                .min(columns.saturating_sub(1)),
            u16::try_from(line).unwrap_or(bottom),
        );
        state.input_needs_wrap = false;
    }
}

impl Handler for ScrollRegionTracker {
    fn input(&mut self, character: char) {
        let width = character.width().unwrap_or(0);
        if width == 0 {
            return;
        }
        let columns = self.columns.max(1);
        let rows = self.rows;
        let state = self.current_mut();
        if state.input_needs_wrap {
            state.linefeed(rows);
            state.carriage_return();
        }
        if width > 1 && state.cursor.0.saturating_add(width as u16) > columns {
            if state.line_wrap {
                state.linefeed(rows);
                state.carriage_return();
            } else {
                state.input_needs_wrap = true;
                return;
            }
        }
        if state.cursor.0.saturating_add(width as u16) < columns {
            state.cursor.0 = state.cursor.0.saturating_add(width as u16);
        } else {
            state.input_needs_wrap = true;
        }
    }

    fn goto(&mut self, line: i32, column: usize) {
        self.move_cursor(line, column);
    }

    fn goto_line(&mut self, line: i32) {
        let column = usize::from(self.current().cursor.0);
        self.move_cursor(line, column);
    }

    fn goto_col(&mut self, column: usize) {
        let line = i32::from(self.current().cursor.1);
        self.move_cursor(line, column);
    }

    fn move_up(&mut self, lines: usize) {
        let current = self.current().cursor;
        self.move_cursor(
            i32::from(current.1).saturating_sub(i32::try_from(lines).unwrap_or(i32::MAX)),
            usize::from(current.0),
        );
    }

    fn move_down(&mut self, lines: usize) {
        let current = self.current().cursor;
        self.move_cursor(
            i32::from(current.1).saturating_add(i32::try_from(lines).unwrap_or(i32::MAX)),
            usize::from(current.0),
        );
    }

    fn move_forward(&mut self, columns: usize) {
        let current = self.current().cursor;
        self.move_cursor(
            i32::from(current.1),
            usize::from(current.0).saturating_add(columns),
        );
    }

    fn move_backward(&mut self, columns: usize) {
        let current = self.current().cursor;
        self.move_cursor(
            i32::from(current.1),
            usize::from(current.0).saturating_sub(columns),
        );
    }

    fn move_down_and_cr(&mut self, lines: usize) {
        self.move_down(lines);
        self.current_mut().carriage_return();
    }

    fn move_up_and_cr(&mut self, lines: usize) {
        self.move_up(lines);
        self.current_mut().carriage_return();
    }

    fn backspace(&mut self) {
        let state = self.current_mut();
        state.cursor.0 = state.cursor.0.saturating_sub(1);
        state.input_needs_wrap = false;
    }

    fn carriage_return(&mut self) {
        self.current_mut().carriage_return();
    }

    fn linefeed(&mut self) {
        let rows = self.rows;
        self.current_mut().linefeed(rows);
    }

    fn newline(&mut self) {
        let linefeed_newline = self.current().linefeed_newline;
        self.linefeed();
        if linefeed_newline {
            self.current_mut().carriage_return();
        }
    }

    fn scroll_up(&mut self, lines: usize) {
        self.current_mut().scroll_up(lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        self.current_mut().scroll_down(lines);
    }

    fn reverse_index(&mut self) {
        let state = self.current_mut();
        if state.cursor.1 == state.region.top() {
            state.scroll_down(1);
        } else {
            state.cursor.1 = state.cursor.1.saturating_sub(1);
        }
        state.input_needs_wrap = false;
    }

    fn set_mode(&mut self, mode: Mode) {
        if mode.raw() == 20 {
            self.current_mut().linefeed_newline = true;
        }
    }

    fn unset_mode(&mut self, mode: Mode) {
        if mode.raw() == 20 {
            self.current_mut().linefeed_newline = false;
        }
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        match mode.raw() {
            6 => self.current_mut().origin = true,
            7 => self.current_mut().line_wrap = true,
            47 | 1047 | 1049 => self.switch_screen(GraphicsScreen::Alternate),
            _ => {}
        }
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        match mode.raw() {
            6 => self.current_mut().origin = false,
            7 => self.current_mut().line_wrap = false,
            47 | 1047 | 1049 => self.switch_screen(GraphicsScreen::Primary),
            _ => {}
        }
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        let rows = usize::from(self.rows);
        let top = top.saturating_sub(1).min(rows);
        let bottom = bottom.unwrap_or(rows).min(rows);
        if top >= bottom {
            return;
        }
        let screen_lines = self.rows;
        let new_region = GraphicsScrollRegion::new(top as u16, bottom as u16, screen_lines);
        let state = self.current_mut();
        if state.region != new_region {
            state.region_scroll = 0;
        }
        state.region = new_region;
        state.reset_region_cursor();
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
    scroll_processor: Processor,
    scroll_tracker: ScrollRegionTracker,
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
    graphics_protocol: GraphicsProtocolAdapter,
    graphics_protocol_capture: Vec<u8>,
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
            scroll_processor: Processor::new(),
            scroll_tracker: ScrollRegionTracker::new(size.columns, size.rows),
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
            graphics_protocol: GraphicsProtocolAdapter::default(),
            graphics_protocol_capture: Vec::new(),
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
        self.graphics.visible_submissions_with_scroll_state(
            surface,
            self.scrollback_lines(),
            self.scroll_tracker.active_screen(),
            self.scroll_tracker.current_region(),
            self.scroll_tracker.current_region_scroll(),
        )
    }

    pub fn graphics_diagnostics(&self) -> &[crate::graphics::GraphicsDiagnostic] {
        self.graphics.diagnostics()
    }

    pub fn graphics_animation_frame_count(&self, image: u32) -> Option<usize> {
        self.graphics.animation_frame_count(image)
    }

    pub fn graphics_animation_state(
        &self,
        image: u32,
    ) -> Option<crate::graphics::GraphicsAnimationState> {
        self.graphics.animation_state(image)
    }

    /// Returns the bounded raw PTY capture used by protocol conformance tests
    /// and diagnostics. It includes text alongside graphics bytes and is never
    /// used as the source of rendering state.
    pub fn graphics_protocol_capture(&self) -> &[u8] {
        &self.graphics_protocol_capture
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
        let remaining = MAX_GRAPHICS_PROTOCOL_CAPTURE_BYTES
            .saturating_sub(self.graphics_protocol_capture.len());
        self.graphics_protocol_capture
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        let events = match self.graphics_protocol.feed(bytes) {
            Ok(events) => events,
            Err(error) => {
                self.graphics.record_diagnostic(
                    None,
                    format!("Kitty graphics protocol stream rejected: {error:?}"),
                );
                return Ok(false);
            }
        };
        let mut changed = false;
        for event in events {
            match event {
                GraphicsProtocolEvent::Plain(plain) => {
                    if !plain.is_empty() {
                        self.processor.advance(&mut self.term, &plain);
                        self.scroll_processor
                            .advance(&mut self.scroll_tracker, &plain);
                        self.flush_emulator_responses()?;
                        changed = true;
                    }
                }
                GraphicsProtocolEvent::Command(command) => {
                    let parameters = command.parameters();
                    let payload = command.payload();
                    let response = match self.graphics.apply_kitty_command_with_scroll_region(
                        &parameters,
                        &payload,
                        self.cursor_position(),
                        (self.size.cell_width(), self.size.cell_height()),
                        self.scrollback_lines(),
                        self.scroll_tracker.active_screen(),
                        self.scroll_tracker.current_region(),
                        self.scroll_tracker.current_region_scroll(),
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
                GraphicsProtocolEvent::Malformed { bytes, reason } => {
                    self.graphics.record_diagnostic(
                        None,
                        format!(
                            "malformed Kitty graphics sequence ({} bytes): {reason}",
                            bytes.len()
                        ),
                    );
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
        self.scroll_tracker.resize(size.columns, size.rows);
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
        let cursor_point = self.term.grid().cursor.point;
        let mut cursor_cell = (
            area.x.saturating_add(cursor_point.column.0 as u16),
            area.y.saturating_add(cursor_point.line.0 as u16),
        );
        for indexed in self.term.grid().display_iter() {
            let point = indexed.point;
            let cell = indexed.cell;
            let x = area.x.saturating_add(point.column.0 as u16);
            let y = area.y.saturating_add(point.line.0 as u16);
            if x >= area.x.saturating_add(area.width) || y >= area.y.saturating_add(area.height) {
                continue;
            }
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                if point == cursor_point {
                    cursor_cell.0 = cursor_cell.0.saturating_sub(1);
                }
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
        if focused {
            let terminal_cursor_visible =
                cursor_visible && self.term.mode().contains(TermMode::SHOW_CURSOR);
            scene.set_cursor(cursor_cell.0, cursor_cell.1, terminal_cursor_visible);
            if terminal_cursor_visible
                && let Some(cell) = scene.cell_at(cursor_cell.0, cursor_cell.1).copied()
            {
                let cursor_style = CellStyle::new(cell.style.background, cell.style.foreground);
                scene.set(cursor_cell.0, cursor_cell.1, cell.symbol, cursor_style);
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
        self.graphics.clear();
        let _ = self.graphics_protocol.finish();
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
    let mut adapter = GraphicsProtocolAdapter::new(4 * 1024 * 1024, 2 * 1024 * 1024);
    let events = match adapter.feed(buffer) {
        Ok(events) => events,
        Err(_) => return (Vec::new(), buffer.to_vec()),
    };
    let mapped = events
        .into_iter()
        .filter_map(|event| match event {
            GraphicsProtocolEvent::Plain(bytes) => Some(KittyStreamEvent::Plain(bytes)),
            GraphicsProtocolEvent::Command(command) => Some(KittyStreamEvent::Command(
                command.parameters().to_vec(),
                command.payload().to_vec(),
            )),
            GraphicsProtocolEvent::Malformed { bytes, .. } => Some(KittyStreamEvent::Plain(bytes)),
        })
        .collect();
    (mapped, adapter.pending_bytes().to_vec())
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
    use crate::SceneCursor;
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
    fn cursor_on_a_wide_continuation_cell_anchors_to_the_lead_glyph() {
        let mut session = TerminalSession::spawn_with_args(
            Some("sh"),
            &["-c", "sleep 5"],
            TerminalSize::new(20, 4),
        )
        .unwrap();
        session
            .processor
            .advance(&mut session.term, "界".as_bytes());
        session
            .processor
            .advance(&mut session.term, b"\x1b[?25h\x1b[1;2H");
        assert_eq!(session.cursor_position(), (1, 0));

        let scene = session.render_with_theme_and_cursor(
            Rect::new(2, 3, 20, 4),
            true,
            Theme::fallback(),
            true,
        );

        assert_eq!(scene.cursor(), Some(SceneCursor::new(2, 3, true)));
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
    fn decstbm_tracker_moves_anchors_when_a_partial_region_scrolls() {
        let mut parser = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
        let mut tracker = ScrollRegionTracker::new(20, 6);
        parser.advance(&mut tracker, b"\x1b[2;5r\x1b[5;1H");
        assert_eq!(tracker.current_region(), GraphicsScrollRegion::new(1, 5, 6));
        assert_eq!(tracker.current_region_scroll(), 0);
        assert_eq!(tracker.current().cursor, (0, 4));

        parser.advance(&mut tracker, b"\n");
        assert_eq!(tracker.current_region_scroll(), 1);
        assert_eq!(tracker.current().cursor, (0, 4));

        parser.advance(&mut tracker, b"\x1b[r");
        assert_eq!(tracker.current_region(), GraphicsScrollRegion::new(0, 6, 6));
        assert_eq!(tracker.current_region_scroll(), 0);
        assert_eq!(tracker.current().cursor, (0, 0));

        parser.advance(&mut tracker, b"\x1b[?1049h\x1b[3;5r");
        assert_eq!(tracker.active_screen(), GraphicsScreen::Alternate);
        assert_eq!(tracker.current_region(), GraphicsScrollRegion::new(2, 5, 6));
        parser.advance(&mut tracker, b"\x1b[?1049l");
        assert_eq!(tracker.active_screen(), GraphicsScreen::Primary);
        assert_eq!(tracker.current_region(), GraphicsScrollRegion::new(0, 6, 6));

        tracker.resize(20, 8);
        assert_eq!(tracker.current_region(), GraphicsScrollRegion::new(0, 8, 8));
        assert_eq!(tracker.current_region_scroll(), 0);
    }

    #[test]
    fn session_graphics_follow_decstbm_scrolling_without_primary_scrollback() {
        let mut session = TerminalSession::spawn_with_args(
            Some("sh"),
            &["-c", "sleep 5"],
            TerminalSize::new(20, 6),
        )
        .unwrap();
        session
            .consume_output(b"\x1b[2;5r\x1b[5;1H\x1b_Ga=T,f=24,i=33,c=1,r=1,q=2;AQID\x1b\\")
            .unwrap();
        assert_eq!(session.scrollback_lines(), 0);
        assert_eq!(
            session.graphics(Rect::new(0, 0, 20, 6))[0].placement().y(),
            4
        );

        session.consume_output(b"\n").unwrap();
        assert_eq!(
            session.graphics(Rect::new(0, 0, 20, 6))[0].placement().y(),
            3
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
