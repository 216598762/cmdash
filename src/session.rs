use std::{
    collections::BTreeMap,
    fmt,
    io::{self, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use alacritty_terminal::{
    event::{Event, EventListener, WindowSize as EmulatorWindowSize},
    grid::{Dimensions, Scroll},
    term::{Config, Term, TermMode, cell::Flags},
    vte::ansi::{ClearMode, Color as AnsiColor, Handler, Mode, NamedColor, PrivateMode, Processor},
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
    backend::kitty_diacritic_index,
    graphics::{
        GraphicsErase, GraphicsPlaceholderCell, GraphicsProtocolAdapter, GraphicsProtocolBroker,
        GraphicsProtocolEvent, GraphicsScreen, GraphicsScrollRegion, GraphicsSubmission,
        SessionGraphicsStore, kitty_error_response, should_emit_response,
    },
    scene::{CellStyle, Color, Scene, Underline},
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
    pending_erases: Vec<GraphicsErase>,
}

impl ScrollRegionTracker {
    const fn new(columns: u16, rows: u16) -> Self {
        Self {
            columns,
            rows,
            active: GraphicsScreen::Primary,
            primary: ScrollScreenState::new(columns, rows),
            alternate: ScrollScreenState::new(columns, rows),
            pending_erases: Vec::new(),
        }
    }

    fn take_erases(&mut self) -> Vec<GraphicsErase> {
        std::mem::take(&mut self.pending_erases)
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

    fn reset(&mut self) {
        self.primary = ScrollScreenState::new(self.columns, self.rows);
        self.alternate = ScrollScreenState::new(self.columns, self.rows);
        self.active = GraphicsScreen::Primary;
    }

    fn switch_screen(&mut self, screen: GraphicsScreen) {
        self.active = screen;
        // Entering or leaving the alternate screen discards its images; a
        // real terminal resets the alternate buffer on entry.
        self.pending_erases.push(GraphicsErase::Alternate);
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

    fn clear_screen(&mut self, mode: ClearMode) {
        // `ED 2` clears the visible screen; Kitty erases visible images rather
        // than scrolling them into history. `ED 0`/`ED 1` erase from the
        // cursor to the bottom/top of the screen at row granularity, and
        // `ED 3` clears the scrollback (primary screen only).
        let cursor_row = self.current().cursor.1;
        let erase = match mode {
            ClearMode::All => GraphicsErase::ClearScreen(self.active),
            ClearMode::Below => GraphicsErase::ClearBelow(self.active, cursor_row),
            ClearMode::Above => GraphicsErase::ClearAbove(self.active, cursor_row),
            ClearMode::Saved => GraphicsErase::ClearScrollback,
        };
        self.pending_erases.push(erase);
    }

    fn reset_state(&mut self) {
        // RIS clears the scrollback and both screens, so every retained image
        // is erased and the tracker returns to the primary screen.
        self.reset();
        self.pending_erases.push(GraphicsErase::All);
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
    scrollback_limit: usize,
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
        // Enable the emulator's Kitty keyboard protocol handling so the child
        // can negotiate `CSI u` key encoding: `CSI > 1 u` pushes the
        // disambiguation mode, `CSI ? u` is answered with the active flags,
        // and `key_bytes` consults `term.mode()` when forwarding keys.
        let config = Config {
            kitty_keyboard: true,
            ..Config::default()
        };
        let term = Term::new(
            config,
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
            scrollback_limit: 10_000,
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
            self.scrollback_offset(),
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

    /// Advances this session's Kitty animation frames to `now`, returning the
    /// duration until the next frame deadline (`None` when nothing is playing).
    pub fn advance_graphics_animations(&mut self, now: Instant) -> Option<Duration> {
        self.graphics.advance_animations(now)
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
        let mut changed = false;
        let mut reader_finished = false;
        // Always drain available output, even after the child has exited: a
        // slow reader thread may still be delivering the final buffered bytes,
        // and an early `closed` flag would strand them.
        loop {
            match self.output.receiver.try_recv() {
                Ok(Ok(bytes)) => {
                    changed = self.consume_output(&bytes)? || changed || !bytes.is_empty();
                }
                Ok(Err(error)) => {
                    let message = error.to_string();
                    self.failure = Some(message.clone());
                    return Err(SessionError::Io(message));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    reader_finished = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }
        // Reap the child when it exits. The session is only considered closed
        // once the reader has also drained the PTY to EOF, so the child-exit
        // observation cannot race ahead of the final output.
        let child_exited = self
            .child
            .try_wait()
            .map_err(|error| SessionError::Io(error.to_string()))?
            .is_some();
        if reader_finished && child_exited {
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
                        // Capture the scrollback depth *before* the emulator
                        // consumes this chunk: `ED 2` pushes the viewport into
                        // history and `ED 3` clears it, so resolving an erase
                        // against the post-feed depth would mis-anchor images
                        // (visible images would slide into history, and a
                        // scrolled-out image would resurrect on `ED 3`).
                        let scrollback_before = self.scrollback_lines();
                        self.processor.advance(&mut self.term, &plain);
                        self.scroll_processor
                            .advance(&mut self.scroll_tracker, &plain);
                        self.flush_emulator_responses()?;
                        let erases = self.scroll_tracker.take_erases();
                        if !erases.is_empty() {
                            let region = self.scroll_tracker.current_region();
                            let region_scroll = self.scroll_tracker.current_region_scroll();
                            for erase in erases {
                                self.graphics.apply_erase(
                                    erase, scrollback_before, region, region_scroll,
                                );
                            }
                        }
                        // Refresh the store's view of the Unicode placeholder
                        // cells now that this chunk has been written into the
                        // grid. A relative placement anchored to a virtual
                        // (`U=1`) parent resolves against these cells, so they
                        // must be current before any following command event
                        // is processed.
                        self.graphics.set_placeholder_cells(self.scan_placeholder_cells());
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
                            // Kitty suppresses failure responses when `q=2`;
                            // the diagnostic above is still recorded either way.
                            if suppress_graphics_error_response(parameters) {
                                None
                            } else {
                                image
                                    .filter(|image| *image != 0)
                                    .map(|_| kitty_error_response(&parameters, &error))
                            }
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
                    // A real Kitty terminal advances its cursor past a placed
                    // image (right by `c` cells, down by `r` cells) unless the
                    // client requested C=1. Emulate that so trailing text and
                    // subsequent images follow the image instead of stacking
                    // on its top-left cell.
                    if let Some((columns, rows)) = self.graphics.take_last_cursor_advance() {
                        let advance = graphics_cursor_advance_bytes(columns, rows);
                        if !advance.is_empty() {
                            self.processor.advance(&mut self.term, &advance);
                            self.scroll_processor.advance(&mut self.scroll_tracker, &advance);
                        }
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
        // Drop placements (and their decoded bytes) that have scrolled above
        // the top of the bounded history, exactly like a real graphics
        // terminal frees images once they pass the scrollback limit.
        if self.graphics.evict_beyond_scrollback_limit(
            self.scrollback_limit,
            self.scroll_tracker.active_screen(),
            self.scroll_tracker.current_region(),
            self.scroll_tracker.current_region_scroll(),
        ) {
            changed = true;
        }
        self.flush_emulator_responses()?;
        Ok(changed)
    }

    /// Scans the visible text grid for Kitty Unicode-placeholder glyphs
    /// (U+10EEEE + combining marks) and returns the image-id -> cell map the
    /// graphics store uses to resolve virtual-parent origins.
    fn scan_placeholder_cells(&self) -> BTreeMap<u32, Vec<GraphicsPlaceholderCell>> {
        let mut cells: BTreeMap<u32, Vec<GraphicsPlaceholderCell>> = BTreeMap::new();
        let scrollback = self.scrollback_lines();
        for indexed in self.term.grid().display_iter() {
            let cell = indexed.cell;
            if cell.c != '\u{10eeee}' {
                continue;
            }
            // History lines (negative `line`) are outside the visible screen;
            // they carry an unrepresentable `u16` row, so they are skipped.
            if indexed.point.line.0 < 0 {
                continue;
            }
            // The lower 24 bits of the image id are the foreground color and
            // the high 8 bits are the third combining mark.
            let AnsiColor::Spec(rgb) = cell.fg else {
                continue;
            };
            let high = cell
                .zerowidth()
                .and_then(|marks| marks.get(2))
                .and_then(|mark| kitty_diacritic_index(*mark))
                .unwrap_or(0);
            let image = (u32::from(high) << 24)
                | (u32::from(rgb.r) << 16)
                | (u32::from(rgb.g) << 8)
                | u32::from(rgb.b);
            cells.entry(image).or_default().push(GraphicsPlaceholderCell::new(
                indexed.point.column.0 as u16,
                indexed.point.line.0 as u16,
                scrollback,
            ));
        }
        cells
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
        self.scroll_to_bottom();
        let bytes = key_bytes(key, *self.term.mode())
            .ok_or_else(|| SessionError::Io(format!("unsupported key event {:?}", key.code)))?;
        self.write_bytes(&bytes)
    }

    pub fn write_paste(&mut self, text: &str) -> Result<(), SessionError> {
        self.scroll_to_bottom();
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

    /// Number of history lines currently scrolled above the live viewport.
    pub fn scrollback_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Scrolls the scrollback viewport, returning whether the view changed.
    ///
    /// The emulator's grid is the source of truth: this reuses its
    /// `display_offset` machinery so the same code that pins the view during
    /// new output also drives explicit history navigation.
    pub fn scroll_display(&mut self, scroll: Scroll) -> bool {
        let before = self.term.grid().display_offset();
        self.term.scroll_display(scroll);
        self.term.grid().display_offset() != before
    }

    /// Whether mouse wheel events should be delivered to the child application
    /// instead of scrolling the terminal's own scrollback.
    ///
    /// A full-featured terminal forwards wheel events when the alternate screen
    /// is active (apps such as `less` draw their own scrolling) or the
    /// application has enabled mouse reporting, so its scroll is captured.
    pub fn captures_mouse_scroll(&self) -> bool {
        let mode = self.term.mode();
        mode.contains(TermMode::ALT_SCREEN) || mode.intersects(TermMode::MOUSE_MODE)
    }

    /// Scrolls the viewport back to the live screen, like a real terminal that
    /// returns to the bottom when new input is typed while viewing history.
    fn scroll_to_bottom(&mut self) {
        if self.term.grid().display_offset() != 0 {
            self.term.scroll_display(Scroll::Bottom);
        }
    }

    /// The configured maximum number of scrollback history lines.
    pub const fn scrollback_limit(&self) -> usize {
        self.scrollback_limit
    }

    /// Re-bounds the emulator's scrollback history and evicts any retained
    /// image data that no longer fits inside it.
    pub fn set_scrollback_limit(&mut self, limit: usize) {
        self.scrollback_limit = limit;
        self.term.grid_mut().update_history(limit);
        self.graphics.evict_beyond_scrollback_limit(
            self.scrollback_limit,
            self.scroll_tracker.active_screen(),
            self.scroll_tracker.current_region(),
            self.scroll_tracker.current_region_scroll(),
        );
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
        // Capture the pre-resize graphics state so image placements can be
        // re-anchored across the emulator's text reflow below.
        let old_columns = self.size.columns;
        let old_scrollback = self.scrollback_lines();
        let old_region = self.scroll_tracker.current_region();
        let old_region_scroll = self.scroll_tracker.current_region_scroll();
        self.term.resize(size);
        // A column change rewraps text and moves its scrollback depth without
        // scrolling content uniformly, so full-screen placements must keep
        // their grid row instead of being shifted by the rewrap.
        self.graphics.reanchor_on_resize(
            old_columns,
            size.columns,
            old_scrollback,
            self.scrollback_lines(),
            old_region,
            old_region_scroll,
        );
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
            if cell.flags.contains(Flags::ITALIC) {
                style = style.italic();
            }
            style = style.underline_style(cell_underline(cell.flags));
            if let Some(color) = cell.underline_color().and_then(underline_color_to_scene) {
                style = style.underline_color(color);
            }
            if cell.flags.contains(Flags::STRIKEOUT) {
                style = style.strikeout();
            }
            if cell.flags.contains(Flags::INVERSE) {
                style = style.reverse();
            }
            if cell.flags.contains(Flags::HIDDEN) {
                style = style.hidden();
            }
            scene.set(x, y, cell.c, style);
        }
        if focused {
            // When the viewport is scrolled back into history the live cursor
            // belongs to the bottom of the buffer, so it must not be drawn
            // over the scrolled view.
            if self.term.grid().display_offset() != 0 {
                scene.clear_cursor();
            } else {
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
        // `poll_output` may already have reaped a short-lived child; signalling
        // a reaped PID could hit a reused process, so only kill a child that is
        // still running.
        let child_reaped = self
            .child
            .try_wait()
            .map_err(|error| SessionError::Io(error.to_string()))?
            .is_some();
        let kill_result = if child_reaped {
            Ok(())
        } else {
            self.child.kill()
        };
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

/// Emits the VT cursor movement implied by a Kitty image placement: the cursor
/// is advanced right by the placement's columns and down by its rows, matching
/// the protocol's default (`C=0`) behavior in a real graphics terminal.
fn graphics_cursor_advance_bytes(columns: u16, rows: u16) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(12);
    if columns != 0 {
        bytes.extend_from_slice(format!("\x1b[{columns}C").as_bytes());
    }
    if rows != 0 {
        bytes.extend_from_slice(format!("\x1b[{rows}B").as_bytes());
    }
    bytes
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

/// Whether a failed Kitty command's error response should be suppressed under
/// the command's `q` quiet key, matching Kitty's `finish_command_response`:
/// `q=1` still delivers failure responses while any `q >= 2` suppresses them.
fn suppress_graphics_error_response(parameters: &[u8]) -> bool {
    let quiet = parameters
        .split(|byte| *byte == b',')
        .find_map(|parameter| parameter.strip_prefix(b"q="))
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(0);
    !should_emit_response(quiet, false)
}

/// Encodes a key event for the child PTY, honoring the Kitty keyboard
/// protocol mode the child negotiated through the emulator (`TermMode`).
///
/// In the default (legacy) mode text keys keep their C0/ESC/plain encoding and
/// functional keys keep their `CSI ... ~`/`CSI letter` forms, so existing
/// programs are unaffected. Once the child enables disambiguation (`CSI > 1 u`)
/// or all-keys-as-escapes (`CSI = 8 u`), modified and ambiguous keys are sent
/// as `CSI number ; modifier u` sequences instead.
fn key_bytes(key: KeyEvent, mode: TermMode) -> Option<Vec<u8>> {
    let modifiers = key.modifiers;
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    let super_mod = modifiers.contains(KeyModifiers::SUPER);
    let report_all = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    let disambiguate = mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) || report_all;

    match key.code {
        KeyCode::Char(character) => {
            let code = unshifted_codepoint(character);
            // Combinations whose legacy encoding is missing or ambiguous use
            // CSI u, as do ctrl/alt text keys once disambiguation is on.
            let needs_csi = report_all
                || super_mod
                || (ctrl && shift)
                || (alt && shift)
                || (disambiguate && (ctrl || alt));
            if needs_csi {
                return Some(csi_u_bytes(code, modifiers));
            }
            // Legacy text-key encoding: ESC prefix for alt, C0 mapping for
            // ctrl, and the (already shifted) character otherwise.
            let mut bytes = Vec::new();
            if alt {
                bytes.push(0x1b);
            }
            if ctrl {
                if let Some(control) = legacy_ctrl_byte(character) {
                    bytes.push(control);
                    return Some(bytes);
                }
                // ctrl on a key with no C0 mapping has no legacy form.
                return Some(csi_u_bytes(code, modifiers));
            }
            bytes.extend(character.to_string().bytes());
            Some(bytes)
        }
        KeyCode::Enter => {
            if report_all {
                Some(csi_u_bytes(13, modifiers))
            } else if alt {
                Some(b"\x1b\r".to_vec())
            } else {
                Some(vec![b'\r'])
            }
        }
        KeyCode::Tab => {
            if report_all {
                Some(csi_u_bytes(9, modifiers))
            } else if shift {
                Some(b"\x1b[Z".to_vec())
            } else if alt {
                Some(b"\x1b\t".to_vec())
            } else {
                Some(vec![b'\t'])
            }
        }
        KeyCode::BackTab => {
            if report_all {
                Some(csi_u_bytes(9, modifiers))
            } else {
                Some(b"\x1b[Z".to_vec())
            }
        }
        KeyCode::Backspace => {
            if report_all {
                Some(csi_u_bytes(127, modifiers))
            } else if alt {
                Some(b"\x1b\x7f".to_vec())
            } else if ctrl {
                Some(vec![0x08])
            } else {
                Some(vec![0x7f])
            }
        }
        KeyCode::Esc => {
            if disambiguate && !modifiers.is_empty() {
                Some(csi_u_bytes(27, modifiers))
            } else if alt {
                Some(b"\x1b\x1b".to_vec())
            } else {
                Some(vec![0x1b])
            }
        }
        KeyCode::Up => Some(csi_letter_bytes('A', modifiers)),
        KeyCode::Down => Some(csi_letter_bytes('B', modifiers)),
        KeyCode::Right => Some(csi_letter_bytes('C', modifiers)),
        KeyCode::Left => Some(csi_letter_bytes('D', modifiers)),
        KeyCode::Home => Some(csi_letter_bytes('H', modifiers)),
        KeyCode::End => Some(csi_letter_bytes('F', modifiers)),
        KeyCode::Insert => Some(csi_tilde_bytes(2, modifiers)),
        KeyCode::Delete => Some(csi_tilde_bytes(3, modifiers)),
        KeyCode::PageUp => Some(csi_tilde_bytes(5, modifiers)),
        KeyCode::PageDown => Some(csi_tilde_bytes(6, modifiers)),
        KeyCode::F(number) => {
            function_key_code(number).map(|code| csi_tilde_bytes(code, modifiers))
        }
        _ => None,
    }
}

/// The Kitty keyboard protocol modifier encoding: the active-modifier bitmask
/// (shift 1, alt 2, ctrl 4, super 8) plus one, so "no modifiers" is `1`.
fn kitty_modifier_code(modifiers: KeyModifiers) -> u16 {
    let mut bits = 0u16;
    if modifiers.contains(KeyModifiers::SHIFT) {
        bits |= 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        bits |= 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        bits |= 4;
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        bits |= 8;
    }
    bits + 1
}

/// Encodes a key as `CSI <code> ; <modifier> u`.
fn csi_u_bytes(code: u32, modifiers: KeyModifiers) -> Vec<u8> {
    format!("\x1b[{code};{}u", kitty_modifier_code(modifiers)).into_bytes()
}

/// Encodes a functional key as `CSI <number> ; <modifier> ~`, omitting the
/// modifier field when none are present.
fn csi_tilde_bytes(number: u16, modifiers: KeyModifiers) -> Vec<u8> {
    if modifiers.is_empty() {
        format!("\x1b[{number}~").into_bytes()
    } else {
        format!("\x1b[{number};{}~", kitty_modifier_code(modifiers)).into_bytes()
    }
}

/// Encodes a cursor/functional key as `CSI 1 ; <modifier> <letter>`, omitting
/// the `1 ;` prefix when no modifiers are present (so plain arrows stay
/// `CSI A` for legacy compatibility).
fn csi_letter_bytes(letter: char, modifiers: KeyModifiers) -> Vec<u8> {
    if modifiers.is_empty() {
        format!("\x1b[{letter}").into_bytes()
    } else {
        format!("\x1b[1;{}{letter}", kitty_modifier_code(modifiers)).into_bytes()
    }
}

/// The unshifted (base-layout) Unicode codepoint for a CSI u key code. Letters
/// are lowercased and the common US-layout shifted symbols map back to their
/// unshifted key.
fn unshifted_codepoint(character: char) -> u32 {
    match character {
        'A'..='Z' => u32::from(character.to_ascii_lowercase()),
        '!' => u32::from('1'),
        '@' => u32::from('2'),
        '#' => u32::from('3'),
        '$' => u32::from('4'),
        '%' => u32::from('5'),
        '^' => u32::from('6'),
        '&' => u32::from('7'),
        '*' => u32::from('8'),
        '(' => u32::from('9'),
        ')' => u32::from('0'),
        _ => u32::from(character),
    }
}

/// Maps a text key with ctrl held down to its legacy C0 control byte, using
/// Kitty's superset of the VT-100 table. Returns `None` for keys that have no
/// control-code mapping.
fn legacy_ctrl_byte(character: char) -> Option<u8> {
    match character {
        ' ' | '@' => Some(0),
        'a'..='z' | 'A'..='Z' => Some(character.to_ascii_lowercase() as u8 - b'a' + 1),
        '[' => Some(27),
        '\\' => Some(28),
        ']' => Some(29),
        '^' | '~' => Some(30),
        '_' | '?' => Some(31),
        '0' => Some(b'0'),
        '1' => Some(b'1'),
        '2' => Some(0),
        '3' => Some(27),
        '4' => Some(28),
        '5' => Some(29),
        '6' => Some(30),
        '7' => Some(31),
        '8' => Some(127),
        '9' => Some(b'9'),
        '/' => Some(31),
        _ => None,
    }
}

/// The Kitty functional-key `~` code for F1 through F12. F13+ are not exposed
/// by the legacy encoding and are currently unsupported.
fn function_key_code(number: u8) -> Option<u16> {
    Some(match number {
        1 => 11,
        2 => 12,
        3 => 13,
        4 => 14,
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    })
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

/// Maps a cell's underline flag bits to the scene's underline style.
///
/// alacritty encodes the underline variant as mutually exclusive flag bits
/// (`UNDERCURL`, `DOTTED_UNDERLINE`, `DASHED_UNDERLINE`, `DOUBLE_UNDERLINE`)
/// layered on top of the plain `UNDERLINE` bit.
fn cell_underline(flags: Flags) -> Underline {
    if flags.contains(Flags::UNDERCURL) {
        Underline::Curly
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        Underline::Dotted
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        Underline::Dashed
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        Underline::Double
    } else if flags.contains(Flags::UNDERLINE) {
        Underline::Plain
    } else {
        Underline::None
    }
}

/// Converts an underline color to a scene color, or `None` when the color is
/// a named terminal default (foreground/background) that should be inherited
/// rather than emitted as an explicit SGR 58 value.
fn underline_color_to_scene(color: AnsiColor) -> Option<Color> {
    match color {
        AnsiColor::Spec(rgb) => Some(Color::rgb(rgb.r, rgb.g, rgb.b)),
        AnsiColor::Indexed(index) => Some(indexed_color(index)),
        AnsiColor::Named(named) => named_color(named),
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
    fn graphics_error_responses_follow_kitty_quiet_suppression() {
        // q=0 and q=1 deliver failure responses; q=2 suppresses them (matching
        // Kitty's `finish_command_response`).
        assert!(!suppress_graphics_error_response(b"a=p,i=999,q=0"));
        assert!(!suppress_graphics_error_response(b"a=p,i=999,q=1"));
        assert!(suppress_graphics_error_response(b"a=p,i=999,q=2"));
        // A missing or malformed `q` key defaults to no suppression.
        assert!(!suppress_graphics_error_response(b"a=p,i=999"));
        assert!(!suppress_graphics_error_response(b"a=p,i=999,q=bogus"));
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
        // C=1 keeps the cursor on the region's bottom line so the linefeed
        // below exercises DECSTBM scrolling rather than cursor movement.
        session
            .consume_output(b"\x1b[2;5r\x1b[5;1H\x1b_Ga=T,f=24,i=33,c=1,r=1,C=1,q=2;AQID\x1b\\")
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
    fn session_graphics_follow_the_scrollback_view_as_text_scrolls() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(20, 6)).unwrap();
        // C=1 keeps the cursor still so the placement anchors at row 0.
        session
            .consume_output(b"\x1b_Ga=T,f=24,i=33,c=1,r=1,C=1,q=2;AQID\x1b\\")
            .unwrap();
        assert_eq!(session.scrollback_lines(), 0);
        assert_eq!(
            session.graphics(Rect::new(0, 0, 20, 6))[0].placement().y(),
            0
        );

        // Scroll twelve lines of text; the image follows into history.
        for line in 0..12u8 {
            session
                .consume_output(format!("row{line}\r\n").as_bytes())
                .unwrap();
        }
        let history = session.scrollback_lines();
        assert!(history > 0);
        // In the live viewport the image has scrolled out and is clipped away.
        assert!(session.graphics(Rect::new(0, 0, 20, 6)).is_empty());

        // Scrolling the view to the top re-shows the image on its original row.
        assert!(session.scroll_display(Scroll::Top));
        assert_eq!(session.scrollback_offset(), history);
        let graphics = session.graphics(Rect::new(0, 0, 20, 6));
        assert_eq!(graphics.len(), 1);
        assert_eq!(graphics[0].placement().y(), 0);
        session.shutdown().unwrap();
    }

    #[test]
    fn scrollback_limit_bounds_history_and_evicts_scrolled_out_graphics() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(20, 4)).unwrap();
        session.set_scrollback_limit(3);
        assert_eq!(session.scrollback_limit(), 3);

        // C=1 keeps the cursor still so the placement anchors at row 0.
        session
            .consume_output(b"\x1b_Ga=T,f=24,i=33,c=1,r=1,C=1,q=2;AQID\x1b\\")
            .unwrap();
        assert_eq!(session.graphics(Rect::new(0, 0, 20, 4)).len(), 1);

        // Eight lines scroll past a three-line history cap.
        for line in 0..8u8 {
            session
                .consume_output(format!("row{line}\r\n").as_bytes())
                .unwrap();
        }
        assert_eq!(session.scrollback_lines(), 3);

        // The image scrolled above the retained history and was evicted, so
        // nothing remains even after scrolling the view to the top.
        session.scroll_display(Scroll::Top);
        assert!(session.graphics(Rect::new(0, 0, 20, 4)).is_empty());
        session.shutdown().unwrap();
    }

    #[test]
    fn session_graphics_follow_clear_screen_reset_and_screen_switches() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(20, 4)).unwrap();
        let place = b"\x1b_Ga=T,f=24,i=33,c=1,r=1,C=1,q=2;AQID\x1b\\";

        // ED 2 clears the visible image.
        session.consume_output(place).unwrap();
        assert_eq!(session.graphics(Rect::new(0, 0, 20, 4)).len(), 1);
        session.consume_output(b"\x1b[2J").unwrap();
        assert!(session.graphics(Rect::new(0, 0, 20, 4)).is_empty());

        // RIS (reset) clears all retained graphics.
        session.consume_output(place).unwrap();
        assert_eq!(session.graphics(Rect::new(0, 0, 20, 4)).len(), 1);
        session.consume_output(b"\x1bc").unwrap();
        assert!(session.graphics(Rect::new(0, 0, 20, 4)).is_empty());

        // Entering the alternate screen, placing an image, and leaving erases
        // the alternate screen's image.
        session.consume_output(b"\x1b[?1049h").unwrap();
        session.consume_output(place).unwrap();
        assert_eq!(session.graphics(Rect::new(0, 0, 20, 4)).len(), 1);
        session.consume_output(b"\x1b[?1049l").unwrap();
        assert!(session.graphics(Rect::new(0, 0, 20, 4)).is_empty());
        session.shutdown().unwrap();
    }

    #[test]
    fn session_graphics_follow_partial_and_scrollback_erases() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(20, 4)).unwrap();
        // `ED 0` erases from the cursor row down. Place images on rows 0 and 2,
        // move the cursor to row 1, and erase below: only the row-0 image stays.
        session
            .consume_output(b"\x1b_Ga=T,f=24,i=1,c=1,r=1,C=1,q=2;AQID\x1b\\")
            .unwrap();
        session
            .consume_output(b"\x1b[3;1H\x1b_Ga=T,f=24,i=2,c=1,r=1,C=1,q=2;AQID\x1b\\")
            .unwrap();
        assert_eq!(session.graphics(Rect::new(0, 0, 20, 4)).len(), 2);
        session.consume_output(b"\x1b[2;1H\x1b[0J").unwrap();
        let below = session.graphics(Rect::new(0, 0, 20, 4));
        assert_eq!(below.len(), 1);
        assert_eq!(below[0].placement().y(), 0);

        // `ED 1` erases from the top down to the cursor row. Re-place the
        // bottom image, move to row 1, and erase above: the row-2 image stays.
        session
            .consume_output(b"\x1b[3;1H\x1b_Ga=T,f=24,i=3,c=1,r=1,C=1,q=2;AQID\x1b\\")
            .unwrap();
        session.consume_output(b"\x1b[2;1H\x1b[1J").unwrap();
        let above = session.graphics(Rect::new(0, 0, 20, 4));
        assert_eq!(above.len(), 1);
        assert_eq!(above[0].placement().y(), 2);

        // `ED 3` clears the scrollback. Push an image into history with text,
        // then clear the scrollback: the scrolled-out image must be gone, not
        // merely hidden.
        session.consume_output(b"\x1bc").unwrap();
        session
            .consume_output(b"\x1b_Ga=T,f=24,i=4,c=1,r=1,C=1,q=2;AQID\x1b\\")
            .unwrap();
        for line in 0..5u8 {
            session
                .consume_output(format!("line{line}\r\n").as_bytes())
                .unwrap();
        }
        assert!(session.scrollback_lines() > 0);
        assert!(session.graphics(Rect::new(0, 0, 20, 4)).is_empty());
        session.consume_output(b"\x1b[3J").unwrap();
        assert_eq!(session.scrollback_lines(), 0);
        assert!(session.graphics(Rect::new(0, 0, 20, 4)).is_empty());
        session.shutdown().unwrap();
    }

    #[test]
    fn text_reflows_and_graphics_reanchor_across_a_column_resize() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(20, 6)).unwrap();
        // A full-width line followed by a short second line.
        session
            .consume_output(b"abcdefghijklmnopqrst\r\nABCDE")
            .unwrap();
        // Anchor a static-cursor image on the second line at column 5.
        session
            .consume_output(b"\x1b_Ga=T,f=24,i=33,c=1,r=1,C=1,q=2;AQID\x1b\\")
            .unwrap();
        assert_eq!(
            session.graphics(Rect::new(0, 0, 20, 6))[0].placement().area(),
            Rect::new(5, 1, 1, 1)
        );
        assert_eq!(session.scrollback_lines(), 0);

        // Shrink to 10 columns: the 20-column line rewraps into two rows and
        // pushes a line into scrollback, but the image must keep its grid cell
        // (row 1, column 5) instead of being shifted by the rewrap.
        session.resize(TerminalSize::new(10, 6)).unwrap();
        assert!(session.scrollback_lines() > 0);
        let scene = session.render(Rect::new(0, 0, 10, 6), false);
        // The wrapped continuation now occupies row 0 and "ABCDE" stays on the
        // line below it, proving the text reflowed rather than truncating.
        assert_eq!(scene.cell_at(0, 0).unwrap().symbol, 'k');
        assert_eq!(scene.cell_at(0, 1).unwrap().symbol, 'A');
        assert_eq!(
            session.graphics(Rect::new(0, 0, 10, 6))[0].placement().area(),
            Rect::new(5, 1, 1, 1)
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
    fn text_attributes_survive_rendering() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(20, 4)).unwrap();
        // italic, underline, strikeout, reverse, and hidden on the same cell.
        session
            .processor
            .advance(&mut session.term, b"\x1b[3;4;9;7;8mX\x1b[0m");
        let scene = session.render(Rect::new(0, 0, 20, 4), false);
        let style = scene.cell_at(0, 0).unwrap().style;
        assert!(style.italic);
        assert_eq!(style.underline, Underline::Plain);
        assert!(style.strikeout);
        assert!(style.reverse);
        assert!(style.hidden);
        session.shutdown().unwrap();
    }

    #[test]
    fn underline_styles_and_color_survive_rendering() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(20, 4)).unwrap();
        session.processor.advance(
            &mut session.term,
            b"\x1b[4ma\x1b[4:2mb\x1b[4:3mc\x1b[4:4md\x1b[4:5me\x1b[0m\x1b[4m\x1b[58;2;255;0;0mf\x1b[0m",
        );
        let scene = session.render(Rect::new(0, 0, 20, 4), false);
        let styles: Vec<_> = (0..6)
            .map(|column| scene.cell_at(column, 0).unwrap().style)
            .collect();
        assert_eq!(styles[0].underline, Underline::Plain);
        assert_eq!(styles[1].underline, Underline::Double);
        assert_eq!(styles[2].underline, Underline::Curly);
        assert_eq!(styles[3].underline, Underline::Dotted);
        assert_eq!(styles[4].underline, Underline::Dashed);
        assert_eq!(styles[5].underline, Underline::Plain);
        assert_eq!(styles[5].underline_color, Some(Color::rgb(255, 0, 0)));
        assert_eq!(styles[0].underline_color, None);
        session.shutdown().unwrap();
    }

    #[test]
    fn underline_flag_mapping_is_exhaustive() {
        assert_eq!(cell_underline(Flags::empty()), Underline::None);
        assert_eq!(cell_underline(Flags::UNDERLINE), Underline::Plain);
        assert_eq!(cell_underline(Flags::DOUBLE_UNDERLINE), Underline::Double);
        assert_eq!(cell_underline(Flags::UNDERCURL), Underline::Curly);
        assert_eq!(cell_underline(Flags::DOTTED_UNDERLINE), Underline::Dotted);
        assert_eq!(cell_underline(Flags::DASHED_UNDERLINE), Underline::Dashed);
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
        let legacy = TermMode::empty();
        assert_eq!(
            key_bytes(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                legacy,
            ),
            Some(vec![3])
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), legacy),
            Some(vec![b'\r'])
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), legacy),
            Some(b"\x1b[A".to_vec())
        );
        // Modified arrows keep the `CSI 1 ; modifier letter` form.
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL), legacy),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn kitty_keyboard_disambiguation_encodes_modified_keys_as_csi_u() {
        let disambiguate = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(
            key_bytes(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                disambiguate,
            ),
            Some(b"\x1b[99;5u".to_vec())
        );
        assert_eq!(
            key_bytes(
                KeyEvent::new(
                    KeyCode::Char('C'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                disambiguate,
            ),
            Some(b"\x1b[99;6u".to_vec())
        );
        assert_eq!(
            key_bytes(
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
                disambiguate,
            ),
            Some(b"\x1b[97;3u".to_vec())
        );
        // Plain text keys are unaffected by disambiguation.
        assert_eq!(
            key_bytes(
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
                disambiguate,
            ),
            Some(b"a".to_vec())
        );
    }

    #[test]
    fn kitty_keyboard_legacy_ctrl_and_alt_mapping_is_faithful() {
        let legacy = TermMode::empty();
        assert_eq!(
            key_bytes(
                KeyEvent::new(KeyCode::Char('2'), KeyModifiers::CONTROL),
                legacy,
            ),
            Some(vec![0])
        );
        assert_eq!(
            key_bytes(
                KeyEvent::new(KeyCode::Char('8'), KeyModifiers::CONTROL),
                legacy,
            ),
            Some(vec![127])
        );
        assert_eq!(
            key_bytes(
                KeyEvent::new(KeyCode::Char('0'), KeyModifiers::CONTROL),
                legacy,
            ),
            Some(b"0".to_vec())
        );
        // alt prefixes with ESC in legacy mode.
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT), legacy),
            Some(b"\x1bx".to_vec())
        );
        // ctrl+shift is ambiguous even in legacy mode and uses CSI u.
        assert_eq!(
            key_bytes(
                KeyEvent::new(
                    KeyCode::Char('I'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                legacy,
            ),
            Some(b"\x1b[105;6u".to_vec())
        );
    }

    #[test]
    fn kitty_keyboard_mode_is_negotiated_through_the_emulator() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(20, 4)).unwrap();
        assert!(
            !session
                .term
                .mode()
                .contains(TermMode::DISAMBIGUATE_ESC_CODES)
        );

        // `CSI > 1 u` pushes the disambiguation flag.
        session.consume_output(b"\x1b[>1u").unwrap();
        assert!(
            session
                .term
                .mode()
                .contains(TermMode::DISAMBIGUATE_ESC_CODES)
        );

        // `CSI = 8 ; 2 u` unions in report-all-keys-as-escapes.
        session.consume_output(b"\x1b[=8;2u").unwrap();
        assert!(
            session
                .term
                .mode()
                .contains(TermMode::REPORT_ALL_KEYS_AS_ESC)
        );

        // `CSI < u` pops the stack, restoring the prior mode.
        session.consume_output(b"\x1b[<u").unwrap();
        assert!(
            !session
                .term
                .mode()
                .contains(TermMode::REPORT_ALL_KEYS_AS_ESC)
        );
        session.shutdown().unwrap();
    }

    #[test]
    fn kitty_keyboard_mode_query_is_answered_to_the_child_pty() {
        let mut session = TerminalSession::spawn_with_args(
            Some("sh"),
            &[
                "-c",
                "stty -icanon min 1 time 0; printf '\\033[>1u\\033[?u'; response=$(dd bs=1 count=5 2>/dev/null); if [ \"$response\" = \"$(printf '\\033[?1u')\" ]; then printf 'kb-ok'; fi; sleep 5",
            ],
            TerminalSize::new(40, 8),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut kb_ok = false;
        while Instant::now() < deadline {
            session.poll_output().unwrap();
            let scene = session.render(Rect::new(0, 0, 40, 8), false);
            let rendered: String = scene.cells().iter().map(|cell| cell.symbol).collect();
            if rendered.contains("kb-ok") {
                kb_ok = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(kb_ok, "child did not receive the keyboard mode response");
        session.shutdown().unwrap();
    }

    #[test]
    fn scrollback_view_offset_scrolls_into_history_and_hides_the_cursor() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(20, 4)).unwrap();
        for line in 0..10u8 {
            session
                .processor
                .advance(&mut session.term, format!("row{line}\r\n").as_bytes());
        }
        assert!(session.scrollback_lines() > 0);
        assert_eq!(session.scrollback_offset(), 0);

        assert!(session.scroll_display(Scroll::PageUp));
        assert!(session.scrollback_offset() > 0);

        let scene = session.render_with_theme_and_cursor(
            Rect::new(0, 0, 20, 4),
            true,
            Theme::fallback(),
            true,
        );
        assert_eq!(scene.cursor(), None);

        assert!(session.scroll_display(Scroll::Bottom));
        assert_eq!(session.scrollback_offset(), 0);
        session.shutdown().unwrap();
    }

    #[test]
    fn typing_while_scrolled_returns_to_the_live_viewport() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(20, 4)).unwrap();
        for line in 0..10u8 {
            session
                .processor
                .advance(&mut session.term, format!("row{line}\r\n").as_bytes());
        }
        assert!(session.scroll_display(Scroll::Top));
        assert_eq!(session.scrollback_offset(), session.scrollback_lines());

        session
            .write_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(session.scrollback_offset(), 0);
        session.shutdown().unwrap();
    }

    #[test]
    fn mouse_scroll_is_captured_only_when_the_app_requests_it() {
        let mut session = TerminalSession::spawn(Some("sh"), TerminalSize::new(20, 4)).unwrap();
        assert!(!session.captures_mouse_scroll());

        session.processor.advance(&mut session.term, b"\x1b[?1000h");
        assert!(session.captures_mouse_scroll());
        session.processor.advance(&mut session.term, b"\x1b[?1000l");
        assert!(!session.captures_mouse_scroll());

        session.processor.advance(&mut session.term, b"\x1b[?1049h");
        assert!(session.captures_mouse_scroll());
        session.processor.advance(&mut session.term, b"\x1b[?1049l");
        assert!(!session.captures_mouse_scroll());
        session.shutdown().unwrap();
    }
}
