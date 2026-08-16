use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    io::Read,
};

use flate2::read::ZlibDecoder;
use ratatui::layout::Rect;

use crate::state::SessionId;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GraphicsResourceId {
    session: SessionId,
    image: u32,
}

impl GraphicsResourceId {
    pub const fn new(session: SessionId, image: u32) -> Self {
        Self { session, image }
    }

    pub const fn session(self) -> SessionId {
        self.session
    }

    pub const fn image(self) -> u32 {
        self.image
    }
}

/// The emulator screen that owns a graphics placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphicsScreen {
    Primary,
    Alternate,
}

/// A DECSTBM scrolling region in zero-based emulator-grid coordinates.
///
/// `screen_lines == 0` is the compatibility value used by the store-only APIs
/// when the caller has no terminal dimensions; it means the complete screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphicsScrollRegion {
    top: u16,
    bottom: u16,
    screen_lines: u16,
}

impl GraphicsScrollRegion {
    pub const fn new(top: u16, bottom: u16, screen_lines: u16) -> Self {
        Self {
            top,
            bottom,
            screen_lines,
        }
    }

    pub const fn unbounded() -> Self {
        Self::new(0, 0, 0)
    }

    pub const fn top(self) -> u16 {
        self.top
    }

    pub const fn bottom(self) -> u16 {
        self.bottom
    }

    pub const fn screen_lines(self) -> u16 {
        self.screen_lines
    }

    pub const fn is_full_screen(self) -> bool {
        self.screen_lines == 0 || (self.top == 0 && self.bottom >= self.screen_lines)
    }
}

/// The emulator-grid location that owns a graphics placement.
///
/// `scrollback` is captured from the child emulator when the placement is
/// created. Resolving against the current history lets a placement move with
/// content that scrolls above it instead of remaining at an outer absolute row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphicsGridAnchor {
    column: u16,
    row: u16,
    scrollback: usize,
    screen: GraphicsScreen,
    scroll_region: GraphicsScrollRegion,
    region_scroll: i64,
}

impl GraphicsGridAnchor {
    pub const fn new(column: u16, row: u16, scrollback: usize) -> Self {
        Self {
            column,
            row,
            scrollback,
            screen: GraphicsScreen::Primary,
            scroll_region: GraphicsScrollRegion::unbounded(),
            region_scroll: 0,
        }
    }

    pub const fn column(self) -> u16 {
        self.column
    }

    pub const fn row(self) -> u16 {
        self.row
    }

    pub const fn scrollback(self) -> usize {
        self.scrollback
    }

    pub const fn screen(self) -> GraphicsScreen {
        self.screen
    }

    pub const fn scroll_region(self) -> GraphicsScrollRegion {
        self.scroll_region
    }

    pub const fn region_scroll(self) -> i64 {
        self.region_scroll
    }

    pub const fn with_screen(mut self, screen: GraphicsScreen) -> Self {
        self.screen = screen;
        self
    }

    pub const fn with_scroll_region(
        mut self,
        scroll_region: GraphicsScrollRegion,
        region_scroll: i64,
    ) -> Self {
        self.scroll_region = scroll_region;
        self.region_scroll = region_scroll;
        self
    }

    pub fn resolve_row(self, current_scrollback: usize) -> i32 {
        i32::from(self.row) + self.scrollback as i32 - current_scrollback as i32
    }

    pub fn resolve_row_with_state(
        self,
        current_scrollback: usize,
        current_region: GraphicsScrollRegion,
        current_region_scroll: i64,
    ) -> i32 {
        let mut row = i32::from(self.row);
        if self.scroll_region.is_full_screen() && current_region.is_full_screen() {
            row += self.scrollback as i32 - current_scrollback as i32;
        } else if self.scroll_region == current_region {
            row -= i32::try_from(current_region_scroll - self.region_scroll).unwrap_or_else(|_| {
                if current_region_scroll >= self.region_scroll {
                    i32::MAX
                } else {
                    i32::MIN
                }
            });
        }
        row
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphicsPlacement {
    resource: GraphicsResourceId,
    placement_id: Option<u32>,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    z_index: i16,
    source: Option<GraphicsSourceRect>,
    cursor_static: bool,
    anchor: GraphicsGridAnchor,
}

/// A source rectangle in the logical image, in pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphicsSourceRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl GraphicsSourceRect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub const fn x(self) -> u32 {
        self.x
    }

    pub const fn y(self) -> u32 {
        self.y
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn height(self) -> u32 {
        self.height
    }
}

impl GraphicsPlacement {
    pub const fn resource(&self) -> GraphicsResourceId {
        self.resource
    }

    pub const fn placement_id(&self) -> Option<u32> {
        self.placement_id
    }

    pub const fn x(&self) -> u16 {
        self.x
    }

    pub const fn y(&self) -> u16 {
        self.y
    }

    pub const fn width(&self) -> u16 {
        self.width
    }

    pub const fn height(&self) -> u16 {
        self.height
    }

    pub const fn z_index(&self) -> i16 {
        self.z_index
    }

    pub const fn source(&self) -> Option<GraphicsSourceRect> {
        self.source
    }

    pub const fn cursor_static(&self) -> bool {
        self.cursor_static
    }

    pub const fn anchor(&self) -> GraphicsGridAnchor {
        self.anchor
    }

    pub const fn area(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, self.height)
    }
}

/// A backend-neutral placeholder region derived from a logical graphics
/// placement. The compositor carries this separately from the image resource;
/// an adapter may later encode it as Kitty combining-mark cells or another
/// terminal-specific representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphicsPlaceholderLayer {
    resource: GraphicsResourceId,
    area: Rect,
    z_index: i16,
}

impl GraphicsPlaceholderLayer {
    pub const fn new(resource: GraphicsResourceId, area: Rect, z_index: i16) -> Self {
        Self {
            resource,
            area,
            z_index,
        }
    }

    pub const fn resource(&self) -> GraphicsResourceId {
        self.resource
    }

    pub const fn area(&self) -> Rect {
        self.area
    }

    pub const fn z_index(&self) -> i16 {
        self.z_index
    }

    pub fn from_submission(submission: &GraphicsSubmission) -> Self {
        Self::new(
            submission.resource(),
            submission.placement().area(),
            submission.placement().z_index(),
        )
    }

    pub fn clipped_to(&self, clip: Rect) -> Option<Self> {
        let area = intersect(self.area, clip)?;
        Some(Self { area, ..*self })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphicsSubmission {
    resource: GraphicsResourceId,
    format: u8,
    generation: u64,
    encoded_payload: Vec<u8>,
    placement: GraphicsPlacement,
}

/// The destination for a graphics protocol response. Child PTY responses must
/// never be confused with replies from the outer terminal probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphicsResponseDestination {
    ChildPty,
    OuterTerminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphicsResponse {
    destination: GraphicsResponseDestination,
    bytes: Vec<u8>,
}

impl GraphicsResponse {
    pub fn new(destination: GraphicsResponseDestination, bytes: Vec<u8>) -> Self {
        Self { destination, bytes }
    }

    pub const fn destination(&self) -> GraphicsResponseDestination {
        self.destination
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Bounded response routing between the child session and the outer terminal.
///
/// Parsing and storage remain session-owned, but all response writes pass
/// through this broker so a future outer-terminal reader can feed probe replies
/// without ever writing them into a child PTY.
#[derive(Clone, Debug)]
pub struct GraphicsProtocolBroker {
    child: VecDeque<GraphicsResponse>,
    outer: VecDeque<GraphicsResponse>,
    max_pending: usize,
}

impl Default for GraphicsProtocolBroker {
    fn default() -> Self {
        Self::new(64)
    }
}

impl GraphicsProtocolBroker {
    pub fn new(max_pending: usize) -> Self {
        Self {
            child: VecDeque::new(),
            outer: VecDeque::new(),
            max_pending: max_pending.max(1),
        }
    }

    pub fn queue(&mut self, destination: GraphicsResponseDestination, bytes: Vec<u8>) -> bool {
        let queue = match destination {
            GraphicsResponseDestination::ChildPty => &mut self.child,
            GraphicsResponseDestination::OuterTerminal => &mut self.outer,
        };
        if queue.len() >= self.max_pending {
            return false;
        }
        queue.push_back(GraphicsResponse::new(destination, bytes));
        true
    }

    pub fn queue_child(&mut self, bytes: Vec<u8>) -> bool {
        self.queue(GraphicsResponseDestination::ChildPty, bytes)
    }

    pub fn queue_outer(&mut self, bytes: Vec<u8>) -> bool {
        self.queue(GraphicsResponseDestination::OuterTerminal, bytes)
    }

    pub fn drain_child(&mut self) -> impl Iterator<Item = GraphicsResponse> + '_ {
        self.child.drain(..)
    }

    pub fn drain_outer(&mut self) -> impl Iterator<Item = GraphicsResponse> + '_ {
        self.outer.drain(..)
    }

    pub fn pending_child(&self) -> usize {
        self.child.len()
    }

    pub fn pending_outer(&self) -> usize {
        self.outer.len()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OuterInputEvent {
    GraphicsResponse(Vec<u8>),
    TerminalInput(Vec<u8>),
}

/// A Kitty command after terminal framing has been removed.
///
/// This is deliberately independent of `SessionGraphicsStore`: the adapter
/// owns byte-stream framing and bounded input, while the store owns Kitty
/// resource semantics and retained state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphicsProtocolCommand {
    parameters: Vec<u8>,
    payload: Vec<u8>,
}

impl GraphicsProtocolCommand {
    pub fn new(parameters: Vec<u8>, payload: Vec<u8>) -> Self {
        Self {
            parameters,
            payload,
        }
    }

    pub fn parameters(&self) -> &[u8] {
        &self.parameters
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphicsProtocolEvent {
    Plain(Vec<u8>),
    Command(GraphicsProtocolCommand),
    Malformed { bytes: Vec<u8>, reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphicsProtocolError {
    InputTooLarge,
    PayloadTooLarge,
    UnterminatedSequence,
}

/// Bounded, incremental Kitty framing for child and passthrough streams.
///
/// Supported framing includes 7-bit APC (`ESC _ G`), C1 APC (`0x9f`), C1 ST
/// (`0x9c`), and tmux's `DCS tmux;` wrapper with doubled ESC bytes. It does not
/// interpret Kitty parameters or store image data; callers can therefore test
/// protocol framing independently from a terminal emulator and resource store.
#[derive(Clone, Debug)]
pub struct GraphicsProtocolAdapter {
    pending: Vec<u8>,
    max_input_bytes: usize,
    max_payload_bytes: usize,
}

impl Default for GraphicsProtocolAdapter {
    fn default() -> Self {
        Self::new(4 * 1024 * 1024, 2 * 1024 * 1024)
    }
}

impl GraphicsProtocolAdapter {
    pub fn new(max_input_bytes: usize, max_payload_bytes: usize) -> Self {
        Self {
            pending: Vec::new(),
            max_input_bytes: max_input_bytes.max(64),
            max_payload_bytes: max_payload_bytes.max(1),
        }
    }

    pub fn pending_bytes(&self) -> &[u8] {
        &self.pending
    }

    pub fn feed(
        &mut self,
        bytes: &[u8],
    ) -> Result<Vec<GraphicsProtocolEvent>, GraphicsProtocolError> {
        if self.pending.len().saturating_add(bytes.len()) > self.max_input_bytes {
            self.pending.clear();
            return Err(GraphicsProtocolError::InputTooLarge);
        }
        self.pending.extend_from_slice(bytes);
        let (events, consumed) =
            match parse_protocol_buffer(&self.pending, self.max_payload_bytes, false) {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.pending.clear();
                    return Err(error);
                }
            };
        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<GraphicsProtocolEvent>, GraphicsProtocolError> {
        let events = self.feed(&[])?;
        if self.pending.is_empty() {
            Ok(events)
        } else {
            Err(GraphicsProtocolError::UnterminatedSequence)
        }
    }
}

fn parse_protocol_buffer(
    buffer: &[u8],
    max_payload_bytes: usize,
    _nested: bool,
) -> Result<(Vec<GraphicsProtocolEvent>, usize), GraphicsProtocolError> {
    const APC: &[u8] = b"\x1b_G";
    const TMUX: &[u8] = b"\x1bPtmux;";
    let mut events = Vec::new();
    let mut index = 0;

    while index < buffer.len() {
        let next = [
            find_sequence(&buffer[index..], APC),
            buffer[index..].iter().position(|byte| *byte == 0x9f),
            find_sequence(&buffer[index..], TMUX),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(offset) = next else {
            let keep = partial_protocol_prefix_len(&buffer[index..]);
            let plain_end = buffer.len().saturating_sub(keep);
            if plain_end > index {
                events.push(GraphicsProtocolEvent::Plain(
                    buffer[index..plain_end].to_vec(),
                ));
            }
            return Ok((events, plain_end));
        };
        let start = index + offset;
        if start > index {
            events.push(GraphicsProtocolEvent::Plain(buffer[index..start].to_vec()));
        }

        if buffer[start..].starts_with(TMUX) {
            let body_start = start + TMUX.len();
            let Some((body_end, terminator_len)) = find_tmux_terminator(&buffer[body_start..])
            else {
                return Ok((events, start));
            };
            let mut inner = Vec::with_capacity(body_end);
            let mut cursor = 0;
            while cursor < body_end {
                if buffer[body_start + cursor..].starts_with(b"\x1b\x1b") {
                    inner.push(0x1b);
                    cursor += 2;
                } else {
                    inner.push(buffer[body_start + cursor]);
                    cursor += 1;
                }
            }
            let (inner_events, inner_consumed) =
                parse_protocol_buffer(&inner, max_payload_bytes, true)?;
            if inner_consumed != inner.len() {
                events.push(GraphicsProtocolEvent::Malformed {
                    bytes: inner,
                    reason: "tmux passthrough contained an incomplete Kitty command".to_owned(),
                });
            } else {
                events.extend(inner_events);
            }
            index = body_start + body_end + terminator_len;
            continue;
        }

        let (body_start, c1) = if buffer[start..].starts_with(b"\x9f") {
            (start + 1, true)
        } else {
            (start + APC.len(), false)
        };
        let terminator = if c1 {
            find_c1_terminator(&buffer[body_start..]).map(|end| (end, 1))
        } else {
            find_sequence(&buffer[body_start..], b"\x1b\\").map(|end| (end, 2))
        };
        let Some((body_end, terminator_len)) = terminator else {
            return Ok((events, start));
        };
        let body = &buffer[body_start..body_start + body_end];
        if body.len() > max_payload_bytes {
            return Err(GraphicsProtocolError::PayloadTooLarge);
        }
        let command_end = body_start + body_end + terminator_len;
        let Some(separator) = body.iter().position(|byte| *byte == b';') else {
            // Kitty control actions such as animation frame/control and
            // deletion may omit the payload separator entirely. Treat only a
            // recognized action as an empty-payload command; malformed text
            // remains recoverable as a diagnostic event.
            let action = body
                .split(|byte| *byte == b',')
                .find_map(|field| field.strip_prefix(b"a="))
                .and_then(|value| value.first().copied());
            if matches!(
                action,
                Some(
                    b'T' | b't'
                        | b'p'
                        | b'P'
                        | b'f'
                        | b'F'
                        | b'a'
                        | b'A'
                        | b'c'
                        | b'C'
                        | b'd'
                        | b'D'
                        | b'q'
                        | b'Q'
                )
            ) {
                events.push(GraphicsProtocolEvent::Command(
                    GraphicsProtocolCommand::new(body.to_vec(), Vec::new()),
                ));
            } else {
                events.push(GraphicsProtocolEvent::Malformed {
                    bytes: buffer[start..command_end].to_vec(),
                    reason: "Kitty APC has no parameter/payload separator".to_owned(),
                });
            }
            index = command_end;
            continue;
        };
        let payload = &body[separator + 1..];
        if payload.len() > max_payload_bytes {
            return Err(GraphicsProtocolError::PayloadTooLarge);
        }
        events.push(GraphicsProtocolEvent::Command(
            GraphicsProtocolCommand::new(body[..separator].to_vec(), payload.to_vec()),
        ));
        index = command_end;
    }

    Ok((events, index))
}

fn find_sequence(buffer: &[u8], sequence: &[u8]) -> Option<usize> {
    buffer
        .windows(sequence.len())
        .position(|window| window == sequence)
}

fn find_c1_terminator(buffer: &[u8]) -> Option<usize> {
    buffer.iter().position(|byte| *byte == 0x9c)
}

fn find_tmux_terminator(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < buffer.len() {
        if buffer[index] == 0x1b {
            if buffer.get(index + 1) == Some(&0x1b) {
                index += 2;
            } else if buffer.get(index + 1) == Some(&b'\\') {
                return Some((index, 2));
            } else {
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    None
}

fn partial_protocol_prefix_len(buffer: &[u8]) -> usize {
    [
        b"\x1b_G".as_slice(),
        b"\x1bPtmux;".as_slice(),
        b"\x9f".as_slice(),
    ]
    .into_iter()
    .map(|prefix| {
        (1..prefix.len().min(buffer.len() + 1))
            .rev()
            .find(|length| buffer.ends_with(&prefix[..*length]))
            .unwrap_or(0)
    })
    .max()
    .unwrap_or(0)
}

/// Splits raw outer-terminal input into probe responses and bytes that belong
/// to the normal keyboard/event decoder. It retains incomplete escape prefixes
/// across reads and never treats an ordinary CSI keyboard sequence as a probe
/// response.
#[derive(Clone, Debug)]
pub struct GraphicsInputDemultiplexer {
    pending: Vec<u8>,
    max_pending: usize,
}

impl Default for GraphicsInputDemultiplexer {
    fn default() -> Self {
        Self::new(16 * 1024)
    }
}

impl GraphicsInputDemultiplexer {
    pub fn new(max_pending: usize) -> Self {
        Self {
            pending: Vec::new(),
            max_pending: max_pending.max(64),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<OuterInputEvent> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > self.max_pending {
            let overflow = self.pending.len() - self.max_pending;
            self.pending.drain(..overflow);
        }
        let mut events = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            let csi = self.pending[index..]
                .windows(2)
                .position(|window| window == b"\x1b[");
            let apc = self.pending[index..]
                .windows(3)
                .position(|window| window == b"\x1b_G");
            let Some(offset) = (match (csi, apc) {
                (Some(csi), Some(apc)) => Some(csi.min(apc)),
                (Some(offset), None) | (None, Some(offset)) => Some(offset),
                (None, None) => None,
            }) else {
                let keep = usize::from(self.pending.last() == Some(&0x1b));
                let end = self.pending.len().saturating_sub(keep);
                if end > index {
                    events.push(OuterInputEvent::TerminalInput(
                        self.pending[index..end].to_vec(),
                    ));
                }
                self.pending = self.pending[end..].to_vec();
                return events;
            };
            let start = index + offset;
            if start > index {
                events.push(OuterInputEvent::TerminalInput(
                    self.pending[index..start].to_vec(),
                ));
            }
            if self.pending[start..].starts_with(b"\x1b_G") {
                let Some(end) = find_bytes(&self.pending[start + 3..], b"\x1b\\") else {
                    self.pending = self.pending[start..].to_vec();
                    return events;
                };
                let end = start + 3 + end + 2;
                events.push(OuterInputEvent::GraphicsResponse(
                    self.pending[start..end].to_vec(),
                ));
                index = end;
                continue;
            }
            let Some(final_offset) = self.pending[start + 2..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
            else {
                self.pending = self.pending[start..].to_vec();
                return events;
            };
            let end = start + 2 + final_offset + 1;
            let sequence = &self.pending[start..end];
            if sequence.ends_with(b"c") && sequence.starts_with(b"\x1b[?")
                || sequence.ends_with(b"t") && sequence.starts_with(b"\x1b[4;")
            {
                events.push(OuterInputEvent::GraphicsResponse(sequence.to_vec()));
            } else {
                events.push(OuterInputEvent::TerminalInput(sequence.to_vec()));
            }
            index = end;
        }
        self.pending.clear();
        events
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

impl GraphicsSubmission {
    pub const fn resource(&self) -> GraphicsResourceId {
        self.resource
    }

    pub const fn format(&self) -> u8 {
        self.format
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn encoded_payload(&self) -> &[u8] {
        &self.encoded_payload
    }

    pub const fn placement(&self) -> &GraphicsPlacement {
        &self.placement
    }

    pub fn terminal_image_id(&self) -> u32 {
        terminal_image_id(self.resource)
    }

    pub fn clipped_to(&self, clip: Rect) -> Option<Self> {
        let area = intersect(self.placement.area(), clip)?;
        Some(Self {
            resource: self.resource,
            format: self.format,
            generation: self.generation,
            encoded_payload: self.encoded_payload.clone(),
            placement: GraphicsPlacement {
                x: area.x,
                y: area.y,
                width: area.width,
                height: area.height,
                ..self.placement
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphicsLimits {
    pub max_decoded_bytes: usize,
    pub max_resources: usize,
    pub max_placements: usize,
}

impl Default for GraphicsLimits {
    fn default() -> Self {
        Self {
            max_decoded_bytes: 4 * 1024 * 1024,
            max_resources: 256,
            max_placements: 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphicsDiagnostic {
    image: Option<u32>,
    message: String,
}

impl GraphicsDiagnostic {
    pub const fn image(&self) -> Option<u32> {
        self.image
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphicsError {
    MissingAction,
    InvalidParameter(String),
    InvalidImageId,
    ImageNotFound(u32),
    InvalidPayload,
    UnsupportedTransfer(String),
}

impl fmt::Display for GraphicsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingAction => formatter.write_str("Kitty graphics command has no action"),
            Self::InvalidParameter(parameter) => {
                write!(formatter, "invalid Kitty graphics parameter {parameter:?}")
            }
            Self::InvalidImageId => formatter.write_str("Kitty graphics image id must be nonzero"),
            Self::ImageNotFound(image) => {
                write!(formatter, "Kitty graphics image {image} was not found")
            }
            Self::InvalidPayload => formatter.write_str("invalid Kitty graphics base64 payload"),
            Self::UnsupportedTransfer(transfer) => {
                write!(
                    formatter,
                    "unsupported Kitty graphics transfer mode {transfer:?}"
                )
            }
        }
    }
}

impl std::error::Error for GraphicsError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphicsAnimationState {
    Stopped,
    Loading,
    Running,
}

#[derive(Clone, Debug)]
struct GraphicsAnimationFrame {
    payload: Vec<u8>,
    gap_ms: Option<i32>,
}

#[derive(Clone, Debug)]
struct GraphicsResource {
    format: u8,
    generation: u64,
    pixel_width: u32,
    pixel_height: u32,
    decoded_payload: Vec<u8>,
    encoded_payload: Vec<u8>,
    animation_frames: BTreeMap<u32, GraphicsAnimationFrame>,
    animation_state: GraphicsAnimationState,
    animation_current_frame: u32,
}

#[derive(Clone, Debug)]
struct PendingUpload {
    parameters: BTreeMap<String, String>,
    encoded_payload: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct SessionGraphicsStore {
    session: SessionId,
    resources: BTreeMap<u32, GraphicsResource>,
    placements: BTreeMap<u64, GraphicsPlacement>,
    limits: GraphicsLimits,
    decoded_bytes: usize,
    pending_upload: Option<PendingUpload>,
    next_internal_image_id: u32,
    next_placement_key: u64,
    next_resource_generation: u64,
    last_image_id: Option<u32>,
    outer_kitty_graphics: bool,
    diagnostics: Vec<GraphicsDiagnostic>,
}

impl SessionGraphicsStore {
    pub fn new(session: SessionId) -> Self {
        Self::with_limits(session, GraphicsLimits::default())
    }

    pub fn with_limits(session: SessionId, limits: GraphicsLimits) -> Self {
        Self {
            session,
            resources: BTreeMap::new(),
            placements: BTreeMap::new(),
            limits,
            decoded_bytes: 0,
            pending_upload: None,
            next_internal_image_id: 1,
            next_placement_key: 1,
            next_resource_generation: 1,
            last_image_id: None,
            outer_kitty_graphics: true,
            diagnostics: Vec::new(),
        }
    }

    pub const fn session(&self) -> SessionId {
        self.session
    }

    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Total decoded bytes retained by this session's graphics resources and
    /// animation frames. This is the value enforced by `GraphicsLimits`.
    pub const fn decoded_bytes_total(&self) -> usize {
        self.decoded_bytes
    }

    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }

    pub fn decoded_bytes(&self, image: u32) -> Option<&[u8]> {
        self.resources
            .get(&image)
            .map(|resource| resource.decoded_payload.as_slice())
    }

    pub fn animation_frame_count(&self, image: u32) -> Option<usize> {
        self.resources
            .get(&image)
            .map(|resource| resource.animation_frames.len())
    }
    pub fn animation_state(&self, image: u32) -> Option<GraphicsAnimationState> {
        self.resources
            .get(&image)
            .map(|resource| resource.animation_state)
    }

    pub fn animation_frame_bytes(&self, image: u32, frame: u32) -> Option<&[u8]> {
        self.resources
            .get(&image)
            .and_then(|resource| resource.animation_frames.get(&frame))
            .map(|frame| frame.payload.as_slice())
    }

    pub const fn limits(&self) -> GraphicsLimits {
        self.limits
    }

    pub fn diagnostics(&self) -> &[GraphicsDiagnostic] {
        &self.diagnostics
    }

    pub fn clear_diagnostics(&mut self) {
        self.diagnostics.clear();
    }

    pub fn clear(&mut self) {
        self.resources.clear();
        self.placements.clear();
        self.pending_upload = None;
        self.decoded_bytes = 0;
        self.last_image_id = None;
    }

    pub fn record_diagnostic(&mut self, image: Option<u32>, message: impl Into<String>) {
        self.diagnose(image, message);
    }

    pub fn set_outer_kitty_graphics(&mut self, supported: bool) {
        self.outer_kitty_graphics = supported;
    }

    fn diagnose(&mut self, image: Option<u32>, message: impl Into<String>) {
        if self.diagnostics.len() >= 16 {
            self.diagnostics.remove(0);
        }
        self.diagnostics.push(GraphicsDiagnostic {
            image,
            message: message.into(),
        });
    }

    pub fn apply_kitty_command(
        &mut self,
        parameters: &[u8],
        encoded_payload: &[u8],
    ) -> Result<(), GraphicsError> {
        self.apply_kitty_command_with_context(parameters, encoded_payload, (0, 0), (0, 0))
            .map(|_| ())
    }

    /// Applies a Kitty command and optionally returns the response that must be
    /// written to the application PTY. `cursor` is relative to the session's
    /// terminal grid, while `cell_size` contains pixel dimensions when known.
    pub fn apply_kitty_command_with_context(
        &mut self,
        parameters: &[u8],
        encoded_payload: &[u8],
        cursor: (u16, u16),
        cell_size: (u16, u16),
    ) -> Result<Option<Vec<u8>>, GraphicsError> {
        self.apply_kitty_command_with_grid_context(
            parameters,
            encoded_payload,
            cursor,
            cell_size,
            0,
        )
    }

    /// Applies a Kitty command while retaining the child emulator's current
    /// scrollback depth as part of every new placement anchor.
    pub fn apply_kitty_command_with_grid_context(
        &mut self,
        parameters: &[u8],
        encoded_payload: &[u8],
        cursor: (u16, u16),
        cell_size: (u16, u16),
        scrollback: usize,
    ) -> Result<Option<Vec<u8>>, GraphicsError> {
        self.apply_kitty_command_with_grid_state(
            parameters,
            encoded_payload,
            cursor,
            cell_size,
            scrollback,
            GraphicsScreen::Primary,
        )
    }

    pub fn apply_kitty_command_with_grid_state(
        &mut self,
        parameters: &[u8],
        encoded_payload: &[u8],
        cursor: (u16, u16),
        cell_size: (u16, u16),
        scrollback: usize,
        screen: GraphicsScreen,
    ) -> Result<Option<Vec<u8>>, GraphicsError> {
        self.apply_kitty_command_with_scroll_region(
            parameters,
            encoded_payload,
            cursor,
            cell_size,
            scrollback,
            screen,
            GraphicsScrollRegion::unbounded(),
            0,
        )
    }

    pub fn apply_kitty_command_with_scroll_region(
        &mut self,
        parameters: &[u8],
        encoded_payload: &[u8],
        cursor: (u16, u16),
        cell_size: (u16, u16),
        scrollback: usize,
        screen: GraphicsScreen,
        scroll_region: GraphicsScrollRegion,
        region_scroll: i64,
    ) -> Result<Option<Vec<u8>>, GraphicsError> {
        let values = parse_parameters(parameters)?;
        let action = values
            .get("a")
            .and_then(|value| value.as_bytes().first())
            .copied();
        let requested_image = values
            .get("i")
            .map(|value| {
                value
                    .parse::<u32>()
                    .map_err(|_| GraphicsError::InvalidImageId)
            })
            .transpose()?
            .unwrap_or(0);
        let mut image = requested_image;
        if image == 0 && matches!(action, Some(b'f' | b'F' | b'a' | b'A' | b'c' | b'C')) {
            image = self.last_image_id.unwrap_or(0);
        }
        let quiet = values
            .get("q")
            .map(|value| {
                value
                    .parse::<u8>()
                    .map_err(|_| GraphicsError::InvalidParameter(value.clone()))
            })
            .transpose()?
            .unwrap_or(0);
        let more = values
            .get("m")
            .map(|value| {
                value
                    .parse::<u8>()
                    .map_err(|_| GraphicsError::InvalidParameter(value.clone()))
            })
            .transpose()?
            .unwrap_or(0);
        let compression = values.get("o").map(String::as_str).unwrap_or("");
        if !compression.is_empty() && compression != "z" {
            return Err(GraphicsError::InvalidParameter("o".to_owned()));
        }
        let max_encoded_bytes = self.limits.max_decoded_bytes.saturating_mul(2);
        if self.pending_upload.is_some()
            && (action.is_none() || matches!(action, Some(b'T' | b't' | b'f')))
        {
            let mut pending = self.pending_upload.take().expect("pending upload checked");
            if pending
                .encoded_payload
                .len()
                .saturating_add(encoded_payload.len())
                > max_encoded_bytes
            {
                self.diagnose(
                    Some(image),
                    format!("Kitty graphics upload exceeds {max_encoded_bytes} encoded bytes"),
                );
                return Ok(None);
            }
            pending.encoded_payload.extend_from_slice(encoded_payload);
            if more != 0 {
                self.pending_upload = Some(pending);
                return Ok(None);
            }
            for (key, value) in values {
                if key != "m" {
                    pending.parameters.insert(key, value);
                }
            }
            pending.parameters.insert("m".to_owned(), "0".to_owned());
            let parameters = serialize_parameters(&pending.parameters);
            return self.apply_kitty_command_with_scroll_region(
                &parameters,
                &pending.encoded_payload,
                cursor,
                cell_size,
                scrollback,
                screen,
                scroll_region,
                region_scroll,
            );
        }

        if more != 0 {
            if !matches!(action, Some(b'T') | Some(b't') | Some(b'f')) {
                return Err(GraphicsError::InvalidParameter("m".to_owned()));
            }
            let transfer = values.get("t").map(String::as_str).unwrap_or("d");
            if transfer != "d" {
                return Err(GraphicsError::UnsupportedTransfer(transfer.to_owned()));
            }
            if encoded_payload.len() > max_encoded_bytes {
                self.diagnose(
                    Some(image),
                    format!("Kitty graphics upload exceeds {max_encoded_bytes} encoded bytes"),
                );
                return Ok(None);
            }
            self.pending_upload = Some(PendingUpload {
                parameters: values,
                encoded_payload: encoded_payload.to_vec(),
            });
            return Ok(None);
        }

        if action == Some(b'q') || action == Some(b'Q') {
            let transfer = values.get("t").map(String::as_str).unwrap_or("d");
            let message = if transfer == "d" && self.outer_kitty_graphics {
                "OK"
            } else if transfer == "d" {
                "ENOTSUP:outer terminal does not support Kitty graphics"
            } else {
                "ENOTSUP:direct transfer is the only supported Kitty mode"
            };
            return Ok(Some(kitty_response(image, None, message)));
        }

        let mut response = None;
        match action {
            Some(b'T') | Some(b't') => {
                let image = if image == 0 {
                    self.allocate_internal_image_id()?
                } else {
                    image
                };
                let transfer = values.get("t").map(String::as_str).unwrap_or("d");
                if transfer != "d" {
                    return Err(GraphicsError::UnsupportedTransfer(transfer.to_owned()));
                }
                let decoded_payload = decode_graphics_payload(
                    encoded_payload,
                    compression,
                    self.limits.max_decoded_bytes,
                )?;
                let format = values
                    .get("f")
                    .map(|value| {
                        value
                            .parse::<u8>()
                            .map_err(|_| GraphicsError::InvalidParameter(value.clone()))
                    })
                    .transpose()?
                    .unwrap_or(32);
                if !matches!(format, 24 | 32 | 100) {
                    self.diagnose(
                        Some(image),
                        format!("unsupported Kitty graphics format {format}"),
                    );
                    return Ok(None);
                }
                if decoded_payload.len() > self.limits.max_decoded_bytes {
                    self.diagnose(
                        Some(image),
                        format!(
                            "graphics payload exceeds {} byte limit",
                            self.limits.max_decoded_bytes
                        ),
                    );
                    return Ok(None);
                }
                let previous_bytes = self.resources.get(&image).map_or(0, resource_storage_bytes);
                let projected_bytes = self
                    .decoded_bytes
                    .saturating_sub(previous_bytes)
                    .saturating_add(decoded_payload.len());
                if projected_bytes > self.limits.max_decoded_bytes {
                    self.diagnose(
                        Some(image),
                        format!(
                            "session graphics store exceeds {} byte limit",
                            self.limits.max_decoded_bytes
                        ),
                    );
                    return Ok(None);
                }
                if !self.resources.contains_key(&image)
                    && self.resources.len() >= self.limits.max_resources
                {
                    self.diagnose(
                        Some(image),
                        format!(
                            "session graphics store exceeds {} resource limit",
                            self.limits.max_resources
                        ),
                    );
                    return Ok(None);
                }
                let natural = natural_dimensions(&decoded_payload, format);
                let pixel_width = parameter_u32(&values, "s", natural.map_or(0, |size| size.0))?;
                let pixel_height = parameter_u32(&values, "v", natural.map_or(0, |size| size.1))?;
                let generation = self.next_resource_generation;
                self.next_resource_generation =
                    self.next_resource_generation.wrapping_add(1).max(1);
                self.decoded_bytes = projected_bytes;
                self.resources.insert(
                    image,
                    GraphicsResource {
                        format,
                        generation,
                        pixel_width,
                        pixel_height,
                        encoded_payload: encode_base64_payload(&decoded_payload),
                        decoded_payload,
                        animation_frames: BTreeMap::new(),
                        animation_state: GraphicsAnimationState::Stopped,
                        animation_current_frame: 1,
                    },
                );
                self.last_image_id = Some(image);
                // Re-transmission replaces the image and all of its old
                // placements, as required by the Kitty protocol.
                self.remove_image_placements(image);
                self.insert_placement(
                    image,
                    &values,
                    cursor,
                    cell_size,
                    scrollback,
                    screen,
                    scroll_region,
                    region_scroll,
                    (pixel_width, pixel_height),
                )?;
                if quiet != 2 && requested_image != 0 {
                    response = Some(kitty_response(image, placement_id(&values), "OK"));
                }
            }
            Some(b'p') | Some(b'P') => {
                let resource = self
                    .resources
                    .get(&image)
                    .ok_or(GraphicsError::ImageNotFound(image))?;
                let dimensions = (resource.pixel_width, resource.pixel_height);
                self.insert_placement(
                    image,
                    &values,
                    cursor,
                    cell_size,
                    scrollback,
                    screen,
                    scroll_region,
                    region_scroll,
                    dimensions,
                )?;
                if quiet != 2 {
                    response = Some(kitty_response(image, placement_id(&values), "OK"));
                }
            }
            Some(b'f') => {
                let decoded = decode_graphics_payload(
                    encoded_payload,
                    compression,
                    self.limits.max_decoded_bytes,
                )?;
                let frame = parameter_u32(&values, "r", 0)?;
                let resource = self
                    .resources
                    .get_mut(&image)
                    .ok_or(GraphicsError::ImageNotFound(image))?;
                let frame = if frame == 0 {
                    resource
                        .animation_frames
                        .keys()
                        .next_back()
                        .copied()
                        .unwrap_or(1)
                        .saturating_add(1)
                } else {
                    frame
                };
                if frame == 0 || frame as usize > self.limits.max_placements {
                    return Err(GraphicsError::InvalidParameter(
                        "animation frame".to_owned(),
                    ));
                }
                let previous_frame_bytes = resource
                    .animation_frames
                    .get(&frame)
                    .map_or(0, |existing| existing.payload.len());
                let projected_bytes = self
                    .decoded_bytes
                    .saturating_sub(previous_frame_bytes)
                    .saturating_add(decoded.len());
                if projected_bytes > self.limits.max_decoded_bytes {
                    self.diagnose(
                        Some(image),
                        format!(
                            "animation frame exceeds {} byte limit",
                            self.limits.max_decoded_bytes
                        ),
                    );
                    return Ok(None);
                }
                let gap_ms = values
                    .get("z")
                    .map(|raw| {
                        raw.parse::<i32>()
                            .map_err(|_| GraphicsError::InvalidParameter(raw.clone()))
                    })
                    .transpose()?;
                resource.animation_frames.insert(
                    frame,
                    GraphicsAnimationFrame {
                        payload: decoded,
                        gap_ms,
                    },
                );
                self.decoded_bytes = projected_bytes;
                if quiet != 2 && image != 0 {
                    response = Some(kitty_response(image, None, "OK"));
                }
            }
            Some(b'a') => {
                let resource = self
                    .resources
                    .get_mut(&image)
                    .ok_or(GraphicsError::ImageNotFound(image))?;
                if let Some(state) = values.get("s") {
                    resource.animation_state = match state.as_str() {
                        "1" => GraphicsAnimationState::Stopped,
                        "2" => GraphicsAnimationState::Loading,
                        "3" => GraphicsAnimationState::Running,
                        _ => return Err(GraphicsError::InvalidParameter(state.clone())),
                    };
                }
                if let Some(frame) = values.get("c") {
                    let frame = frame
                        .parse::<u32>()
                        .map_err(|_| GraphicsError::InvalidParameter(frame.clone()))?;
                    if frame != 1 && !resource.animation_frames.contains_key(&frame) {
                        return Err(GraphicsError::InvalidParameter(
                            "animation frame".to_owned(),
                        ));
                    }
                    resource.animation_current_frame = frame;
                }
                if let (Some(frame), Some(gap)) = (values.get("r"), values.get("z")) {
                    let frame = frame
                        .parse::<u32>()
                        .map_err(|_| GraphicsError::InvalidParameter(frame.clone()))?;
                    let gap_ms = gap
                        .parse::<i32>()
                        .map_err(|_| GraphicsError::InvalidParameter(gap.clone()))?;
                    let target = resource.animation_frames.get_mut(&frame).ok_or_else(|| {
                        GraphicsError::InvalidParameter("animation frame".to_owned())
                    })?;
                    target.gap_ms = Some(gap_ms);
                }
                if quiet != 2 {
                    response = Some(kitty_response(image, None, "OK"));
                }
            }
            Some(b'c') => {
                let resource = self
                    .resources
                    .get_mut(&image)
                    .ok_or(GraphicsError::ImageNotFound(image))?;
                let source = parameter_u32(&values, "r", 1)?;
                let destination = parameter_u32(&values, "c", 1)?;
                let source_exists = source == 1 || resource.animation_frames.contains_key(&source);
                let destination_exists =
                    destination == 1 || resource.animation_frames.contains_key(&destination);
                if !source_exists || !destination_exists {
                    return Err(GraphicsError::ImageNotFound(image));
                }
                resource.animation_current_frame = destination;
                if quiet != 2 {
                    response = Some(kitty_response(image, None, "OK"));
                }
            }
            Some(b'd') | Some(b'D') => match values.get("d").map(String::as_str) {
                Some("a") | Some("A") => {
                    self.clear();
                    self.last_image_id = None;
                }
                Some("p") | Some("P") => {
                    self.placements.clear();
                }
                Some("f") | Some("F") => {
                    if let Some(resource) = self.resources.get_mut(&image) {
                        resource.animation_frames.clear();
                        resource.animation_state = GraphicsAnimationState::Stopped;
                        resource.animation_current_frame = 1;
                    }
                }
                Some("i") | Some("I") if image != 0 => {
                    if let Some(placement) = placement_id(&values) {
                        let key = (u64::from(image) << 32) | u64::from(placement);
                        self.placements.remove(&key);
                    } else {
                        self.remove_image_placements(image);
                    }
                    if values.get("d").is_some_and(|value| value == "I")
                        && self
                            .placements
                            .values()
                            .all(|placement| placement.resource().image() != image)
                    {
                        if let Some(resource) = self.resources.remove(&image) {
                            self.decoded_bytes = self
                                .decoded_bytes
                                .saturating_sub(resource_storage_bytes(&resource));
                        }
                        if self.last_image_id == Some(image) {
                            self.last_image_id = None;
                        }
                    }
                }
                _ if image != 0 => {
                    if let Some(resource) = self.resources.remove(&image) {
                        self.decoded_bytes = self
                            .decoded_bytes
                            .saturating_sub(resource_storage_bytes(&resource));
                    }
                    self.remove_image_placements(image);
                    if self.last_image_id == Some(image) {
                        self.last_image_id = None;
                    }
                }
                _ => return Err(GraphicsError::InvalidParameter("d".to_owned())),
            },
            Some(action) => {
                return Err(GraphicsError::InvalidParameter(
                    (action as char).to_string(),
                ));
            }
            None => return Err(GraphicsError::MissingAction),
        }
        Ok(response)
    }

    fn remove_image_placements(&mut self, image: u32) {
        self.placements
            .retain(|_, placement| placement.resource().image() != image);
    }

    fn allocate_internal_image_id(&mut self) -> Result<u32, GraphicsError> {
        let start = self.next_internal_image_id.max(1);
        let mut candidate = start;
        for _ in 0..=self.limits.max_resources {
            if !self.resources.contains_key(&candidate) {
                self.next_internal_image_id = candidate.wrapping_add(1).max(1);
                return Ok(candidate);
            }
            candidate = candidate.wrapping_add(1).max(1);
        }
        Err(GraphicsError::InvalidImageId)
    }

    fn insert_placement(
        &mut self,
        image: u32,
        values: &BTreeMap<String, String>,
        cursor: (u16, u16),
        cell_size: (u16, u16),
        scrollback: usize,
        screen: GraphicsScreen,
        scroll_region: GraphicsScrollRegion,
        region_scroll: i64,
        pixel_size: (u32, u32),
    ) -> Result<(), GraphicsError> {
        let requested_placement_id = placement_id(values);
        let key = if let Some(placement_id) = requested_placement_id {
            (u64::from(image) << 32) | u64::from(placement_id)
        } else {
            self.allocate_placement_key()
        };
        if !self.placements.contains_key(&key)
            && self.placements.len() >= self.limits.max_placements
        {
            self.diagnose(
                Some(image),
                format!(
                    "session graphics store exceeds {} placement limit",
                    self.limits.max_placements
                ),
            );
            return Ok(());
        }
        let placement = GraphicsPlacement {
            resource: GraphicsResourceId::new(self.session, image),
            placement_id: requested_placement_id,
            x: cursor.0,
            y: cursor.1,
            width: placement_dimension(values, "c", pixel_size.0, cell_size.0)?,
            height: placement_dimension(values, "r", pixel_size.1, cell_size.1)?,
            z_index: parameter_i16(values, "z", 0)?,
            source: source_rect(values, pixel_size)?,
            // cmdash's own outer adapters use C=1 to prevent a graphics replay
            // from disturbing the composed text cursor. The child protocol
            // still records an explicit C=0 as the moving-cursor form.
            cursor_static: values.get("C").map_or(true, |value| value == "1"),
            anchor: GraphicsGridAnchor::new(cursor.0, cursor.1, scrollback)
                .with_screen(screen)
                .with_scroll_region(scroll_region, region_scroll),
        };
        self.placements.insert(key, placement);
        Ok(())
    }

    fn allocate_placement_key(&mut self) -> u64 {
        loop {
            let key = self.next_placement_key;
            self.next_placement_key = self.next_placement_key.wrapping_add(1).max(1);
            if !self.placements.contains_key(&key) {
                return key;
            }
        }
    }

    pub fn visible_submissions(&self, surface: Rect) -> Vec<GraphicsSubmission> {
        self.visible_submissions_at(surface, 0)
    }

    /// Resolves logical child-grid anchors against the current scrollback depth
    /// and projects them into the surface's outer coordinates.
    pub fn visible_submissions_at(
        &self,
        surface: Rect,
        current_scrollback: usize,
    ) -> Vec<GraphicsSubmission> {
        self.visible_submissions_with_state(surface, current_scrollback, GraphicsScreen::Primary)
    }

    pub fn visible_submissions_with_state(
        &self,
        surface: Rect,
        current_scrollback: usize,
        current_screen: GraphicsScreen,
    ) -> Vec<GraphicsSubmission> {
        self.visible_submissions_with_scroll_state(
            surface,
            current_scrollback,
            current_screen,
            GraphicsScrollRegion::unbounded(),
            0,
        )
    }

    pub fn visible_submissions_with_scroll_state(
        &self,
        surface: Rect,
        current_scrollback: usize,
        current_screen: GraphicsScreen,
        current_region: GraphicsScrollRegion,
        current_region_scroll: i64,
    ) -> Vec<GraphicsSubmission> {
        let mut submissions = self
            .placements
            .values()
            .filter_map(|placement| {
                if placement.anchor.screen() != current_screen {
                    return None;
                }
                let resource = self.resources.get(&placement.resource.image())?;
                let resolved_y = placement.anchor.resolve_row_with_state(
                    current_scrollback,
                    current_region,
                    current_region_scroll,
                );
                let placement_area = (
                    i32::from(surface.x) + i32::from(placement.anchor.column()),
                    i32::from(surface.y) + resolved_y,
                    placement.width,
                    placement.height,
                );
                let clipped_area = intersect_signed(placement_area, surface)?;
                Some(GraphicsSubmission {
                    resource: placement.resource,
                    format: resource.format,
                    generation: resource.generation,
                    encoded_payload: resource.encoded_payload.clone(),
                    placement: GraphicsPlacement {
                        x: clipped_area.x,
                        y: clipped_area.y,
                        width: clipped_area.width,
                        height: clipped_area.height,
                        ..*placement
                    },
                })
            })
            .collect::<Vec<_>>();
        submissions.sort_by_key(|submission| submission.placement.z_index());
        submissions
    }
}

fn parse_parameters(parameters: &[u8]) -> Result<BTreeMap<String, String>, GraphicsError> {
    let mut values = BTreeMap::new();
    for parameter in parameters.split(|byte| *byte == b',') {
        if parameter.is_empty() {
            continue;
        }
        let separator = parameter
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or_else(|| {
                GraphicsError::InvalidParameter(String::from_utf8_lossy(parameter).into_owned())
            })?;
        let key = String::from_utf8_lossy(&parameter[..separator]).into_owned();
        let value = String::from_utf8_lossy(&parameter[separator + 1..]).into_owned();
        values.insert(key, value);
    }
    Ok(values)
}

fn serialize_parameters(values: &BTreeMap<String, String>) -> Vec<u8> {
    values
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
        .into_bytes()
}

fn resource_storage_bytes(resource: &GraphicsResource) -> usize {
    resource.decoded_payload.len().saturating_add(
        resource
            .animation_frames
            .values()
            .map(|frame| frame.payload.len())
            .sum::<usize>(),
    )
}

fn parameter_u32(
    values: &BTreeMap<String, String>,
    key: &str,
    default: u32,
) -> Result<u32, GraphicsError> {
    values
        .get(key)
        .map(|value| {
            value
                .parse()
                .map_err(|_| GraphicsError::InvalidParameter(value.clone()))
        })
        .unwrap_or(Ok(default))
}

fn placement_dimension(
    values: &BTreeMap<String, String>,
    key: &str,
    pixels: u32,
    cell_pixels: u16,
) -> Result<u16, GraphicsError> {
    if let Some(value) = values.get(key) {
        return value
            .parse::<u16>()
            .map(|value| value.max(1))
            .map_err(|_| GraphicsError::InvalidParameter(value.clone()));
    }
    if pixels == 0 {
        return Ok(1);
    }
    // Pixel-size ioctls are unavailable on a number of terminals. Preserve
    // known natural geometry instead of collapsing the placement to 1x1; the
    // backend can later refine this estimate when a cell size is available.
    if cell_pixels == 0 {
        return Ok(pixels.min(u32::from(u16::MAX)) as u16);
    }
    Ok(pixels
        .div_ceil(u32::from(cell_pixels))
        .min(u32::from(u16::MAX)) as u16)
}

fn placement_id(values: &BTreeMap<String, String>) -> Option<u32> {
    values
        .get("p")
        .and_then(|value| value.parse().ok())
        .filter(|id| *id != 0)
}

fn source_rect(
    values: &BTreeMap<String, String>,
    natural_size: (u32, u32),
) -> Result<Option<GraphicsSourceRect>, GraphicsError> {
    let has_crop = ["x", "y", "w", "h"]
        .iter()
        .any(|key| values.contains_key(*key));
    if !has_crop
        || (natural_size == (0, 0) && !values.contains_key("w") && !values.contains_key("h"))
    {
        // Without source dimensions, x/y alone cannot describe a bounded
        // crop. Preserve compatibility with clients that use x/y as
        // application-local placement metadata and wait for w/h or natural
        // dimensions before validating a source rectangle.
        return Ok(None);
    }
    let value = |key: &str, default: u32| {
        values
            .get(key)
            .map(|raw| {
                raw.parse::<u32>()
                    .map_err(|_| GraphicsError::InvalidParameter(raw.clone()))
            })
            .unwrap_or(Ok(default))
    };
    let x = value("x", 0)?;
    let y = value("y", 0)?;
    let width = value("w", natural_size.0.saturating_sub(x))?;
    let height = value("h", natural_size.1.saturating_sub(y))?;
    if width == 0 || height == 0 {
        return Err(GraphicsError::InvalidParameter(
            "source crop must be nonzero".to_owned(),
        ));
    }
    if natural_size.0 != 0 && (x >= natural_size.0 || width > natural_size.0.saturating_sub(x))
        || natural_size.1 != 0 && (y >= natural_size.1 || height > natural_size.1.saturating_sub(y))
    {
        return Err(GraphicsError::InvalidParameter(
            "source crop is outside image bounds".to_owned(),
        ));
    }
    Ok(Some(GraphicsSourceRect::new(x, y, width, height)))
}

fn kitty_response(image: u32, placement: Option<u32>, message: &str) -> Vec<u8> {
    let placement = placement.map_or(String::new(), |id| format!(",p={id}"));
    format!("\x1b_Gi={image}{placement};{message}\x1b\\").into_bytes()
}

/// Builds a bounded protocol error response for a command that could not be
/// applied. The response is intentionally best-effort: malformed parameters
/// may not contain a usable image or placement ID.
pub fn kitty_error_response(parameters: &[u8], error: &GraphicsError) -> Vec<u8> {
    let values = parse_parameters(parameters).unwrap_or_default();
    let image = values
        .get("i")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let placement = placement_id(&values);
    let message = match error {
        GraphicsError::ImageNotFound(_) => format!("ENOENT:{error}"),
        _ => format!("EINVAL:{error}"),
    };
    kitty_response(image, placement, &message)
}

fn parameter_i16(
    values: &BTreeMap<String, String>,
    key: &str,
    default: i16,
) -> Result<i16, GraphicsError> {
    values
        .get(key)
        .map(|value| {
            value
                .parse()
                .map_err(|_| GraphicsError::InvalidParameter(value.clone()))
        })
        .unwrap_or(Ok(default))
}

pub fn terminal_image_id(resource: GraphicsResourceId) -> u32 {
    let session = resource.session().get() as u32;
    session.wrapping_mul(0x0010_0001) ^ resource.image()
}

fn natural_dimensions(payload: &[u8], format: u8) -> Option<(u32, u32)> {
    match format {
        100 if payload.len() >= 10 && payload.starts_with(b"GIF") => Some((
            u32::from(u16::from_le_bytes([payload[6], payload[7]])),
            u32::from(u16::from_le_bytes([payload[8], payload[9]])),
        )),
        24 | 32 if payload.len() >= 24 && payload.starts_with(b"\x89PNG\r\n\x1a\n") => Some((
            u32::from_be_bytes(payload[16..20].try_into().ok()?),
            u32::from_be_bytes(payload[20..24].try_into().ok()?),
        )),
        _ => None,
    }
}

fn encode_base64_payload(bytes: &[u8]) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = u32::from(chunk.get(1).copied().unwrap_or(0));
        let third = u32::from(chunk.get(2).copied().unwrap_or(0));
        let combined = (first << 16) | (second << 8) | third;
        output.push(TABLE[((combined >> 18) & 63) as usize]);
        output.push(TABLE[((combined >> 12) & 63) as usize]);
        output.push(if chunk.len() > 1 {
            TABLE[((combined >> 6) & 63) as usize]
        } else {
            b'='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(combined & 63) as usize]
        } else {
            b'='
        });
    }
    output
}

fn decode_graphics_payload(
    payload: &[u8],
    compression: &str,
    max_decoded_bytes: usize,
) -> Result<Vec<u8>, GraphicsError> {
    if compression == "z" && payload.len() > max_decoded_bytes.saturating_mul(2).saturating_add(4) {
        return Err(GraphicsError::InvalidPayload);
    }
    let encoded = decode_base64(payload).ok_or(GraphicsError::InvalidPayload)?;
    if compression != "z" {
        return Ok(encoded);
    }
    let mut decoder = ZlibDecoder::new(encoded.as_slice());
    let mut decoded = Vec::new();
    decoder
        .by_ref()
        .take(max_decoded_bytes.saturating_add(1) as u64)
        .read_to_end(&mut decoded)
        .map_err(|_| GraphicsError::InvalidPayload)?;
    if decoded.len() > max_decoded_bytes {
        return Err(GraphicsError::InvalidPayload);
    }
    Ok(decoded)
}

fn decode_base64(payload: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut accumulator = 0u32;
    let mut bits = 0u8;
    for byte in payload
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
    {
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    Some(output)
}

fn intersect(first: Rect, second: Rect) -> Option<Rect> {
    intersect_signed(
        (
            i32::from(first.x),
            i32::from(first.y),
            first.width,
            first.height,
        ),
        second,
    )
}

fn intersect_signed(first: (i32, i32, u16, u16), second: Rect) -> Option<Rect> {
    let left = first.0.max(i32::from(second.x));
    let top = first.1.max(i32::from(second.y));
    let right = (first.0 + i32::from(first.2)).min(i32::from(second.x) + i32::from(second.width));
    let bottom = (first.1 + i32::from(first.3)).min(i32::from(second.y) + i32::from(second.height));
    if left >= right || top >= bottom {
        return None;
    }
    Some(Rect::new(
        u16::try_from(left).ok()?,
        u16::try_from(top).ok()?,
        u16::try_from(right - left).ok()?,
        u16::try_from(bottom - top).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_adapter_handles_split_apc_and_c1_apc() {
        let mut adapter = GraphicsProtocolAdapter::new(1024, 128);
        assert!(adapter.feed(b"text\x1b_Ga=T,f=24,i=1;AQ").unwrap().len() >= 1);
        let events = adapter.feed(b"ID\x1b\\\x9fa=p,i=1;\x9c").unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            GraphicsProtocolEvent::Command(command) if command.parameters() == b"a=T,f=24,i=1"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            GraphicsProtocolEvent::Command(command) if command.parameters() == b"a=p,i=1"
        )));
        assert!(adapter.pending_bytes().is_empty());
    }

    #[test]
    fn protocol_adapter_unwraps_tmux_and_reports_incomplete_finish() {
        let command = b"\x1b_Ga=T,f=24,i=2;AQID\x1b\\";
        let mut stream = b"\x1bPtmux;".to_vec();
        for byte in command {
            if *byte == 0x1b {
                stream.push(0x1b);
            }
            stream.push(*byte);
        }
        stream.extend_from_slice(b"\x1b\\");

        let mut adapter = GraphicsProtocolAdapter::new(1024, 128);
        let events = adapter.feed(&stream).unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            GraphicsProtocolEvent::Command(command) if command.parameters() == b"a=T,f=24,i=2"
        )));

        let mut incomplete = GraphicsProtocolAdapter::new(1024, 128);
        incomplete.feed(b"\x1b_Ga=T;AQID").unwrap();
        assert_eq!(
            incomplete.finish(),
            Err(GraphicsProtocolError::UnterminatedSequence)
        );
    }

    #[test]
    fn protocol_adapter_rejects_bounded_payloads_without_storage_side_effects() {
        let mut adapter = GraphicsProtocolAdapter::new(64, 4);
        assert_eq!(
            adapter.feed(b"\x1b_Ga=T;AQIDBAUG\x1b\\"),
            Err(GraphicsProtocolError::PayloadTooLarge)
        );
        assert!(adapter.pending_bytes().is_empty());
    }

    #[test]
    fn natural_geometry_survives_missing_cell_size() {
        let mut store = SessionGraphicsStore::new(SessionId::new(22));
        let gif = b"GIF89a\x03\x00\x02\x00";
        store
            .apply_kitty_command_with_context(
                b"a=T,f=100,i=22,q=2",
                &encode_base64_for_test(gif),
                (0, 0),
                (0, 0),
            )
            .unwrap();
        assert_eq!(
            store.visible_submissions(Rect::new(0, 0, 8, 8))[0]
                .placement()
                .area(),
            Rect::new(0, 0, 3, 2)
        );
    }

    #[test]
    fn resources_and_placements_are_namespaced_by_session() {
        let mut first = SessionGraphicsStore::new(SessionId::new(1));
        let mut second = SessionGraphicsStore::new(SessionId::new(2));
        first.apply_kitty_command(b"a=T,f=24,i=1", b"AQID").unwrap();
        first
            .apply_kitty_command(b"a=p,i=1,x=1,y=2,c=3,r=4", b"")
            .unwrap();
        second
            .apply_kitty_command(b"a=T,f=24,i=1", b"BAUG")
            .unwrap();
        second
            .apply_kitty_command(b"a=p,i=1,x=1,y=2,c=3,r=4", b"")
            .unwrap();

        let first_submission = first.visible_submissions(Rect::new(0, 0, 20, 10));
        let second_submission = second.visible_submissions(Rect::new(0, 0, 20, 10));
        assert_ne!(
            first_submission[0].resource(),
            second_submission[0].resource()
        );
        assert_ne!(
            first_submission[0].terminal_image_id(),
            second_submission[0].terminal_image_id()
        );
        assert_ne!(
            first_submission[0].encoded_payload(),
            second_submission[0].encoded_payload()
        );
        assert_eq!(first.decoded_bytes(1), Some(&[1, 2, 3][..]));
    }

    #[test]
    fn placements_restore_with_surface_translation_and_clipping() {
        let mut store = SessionGraphicsStore::new(SessionId::new(3));
        store
            .apply_kitty_command_with_context(b"a=T,f=100,i=7,c=4,r=2", b"AQID", (2, 1), (10, 20))
            .unwrap();

        let submissions = store.visible_submissions(Rect::new(10, 5, 8, 4));
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].placement().area(), Rect::new(12, 6, 4, 2));
        assert_eq!(submissions[0].format(), 100);
    }

    #[test]
    fn alternate_screen_placements_do_not_leak_into_primary_screen() {
        let mut store = SessionGraphicsStore::new(SessionId::new(20));
        store
            .apply_kitty_command_with_grid_state(
                b"a=T,f=24,i=20,c=1,r=1,q=2",
                b"AQID",
                (1, 1),
                (10, 20),
                0,
                GraphicsScreen::Alternate,
            )
            .unwrap();
        assert!(
            store
                .visible_submissions_with_state(Rect::new(0, 0, 4, 4), 0, GraphicsScreen::Primary)
                .is_empty()
        );
        assert_eq!(
            store
                .visible_submissions_with_state(Rect::new(0, 0, 4, 4), 0, GraphicsScreen::Alternate)
                .len(),
            1
        );
    }

    #[test]
    fn grid_anchors_follow_content_as_scrollback_grows() {
        let mut store = SessionGraphicsStore::new(SessionId::new(16));
        store
            .apply_kitty_command_with_grid_context(
                b"a=T,f=24,i=8,c=2,r=2,q=2",
                b"AQID",
                (1, 4),
                (10, 20),
                3,
            )
            .unwrap();

        let initial = store.visible_submissions_at(Rect::new(0, 0, 8, 8), 3);
        let scrolled = store.visible_submissions_at(Rect::new(0, 0, 8, 8), 5);
        assert_eq!(initial[0].placement().area(), Rect::new(1, 4, 2, 2));
        assert_eq!(scrolled[0].placement().area(), Rect::new(1, 2, 2, 2));
        assert_eq!(scrolled[0].placement().anchor().scrollback(), 3);
    }

    #[test]
    fn partial_scroll_regions_move_only_matching_graphics_anchors() {
        let mut store = SessionGraphicsStore::new(SessionId::new(17));
        let region = GraphicsScrollRegion::new(1, 5, 6);
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=9,c=1,r=1,q=2",
                b"AQID",
                (1, 4),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                region,
                0,
            )
            .unwrap();

        let initial = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            0,
            GraphicsScreen::Primary,
            region,
            0,
        );
        let scrolled = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            0,
            GraphicsScreen::Primary,
            region,
            1,
        );
        let different_region = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            0,
            GraphicsScreen::Primary,
            GraphicsScrollRegion::new(0, 6, 6),
            1,
        );
        assert_eq!(initial[0].placement().area(), Rect::new(1, 4, 1, 1));
        assert_eq!(scrolled[0].placement().area(), Rect::new(1, 3, 1, 1));
        assert_eq!(
            different_region[0].placement().area(),
            Rect::new(1, 4, 1, 1)
        );
    }

    #[test]
    fn outer_input_demultiplexer_preserves_keyboard_sequences_and_splits_probe_replies() {
        let mut demux = GraphicsInputDemultiplexer::new(256);
        let events = demux.feed(b"key\x1b_Gi=0;OK\x1b\\\x1b[A");
        assert_eq!(
            events,
            vec![
                OuterInputEvent::TerminalInput(b"key".to_vec()),
                OuterInputEvent::GraphicsResponse(b"\x1b_Gi=0;OK\x1b\\".to_vec()),
                OuterInputEvent::TerminalInput(b"\x1b[A".to_vec()),
            ]
        );

        let mut split = GraphicsInputDemultiplexer::new(256);
        assert!(split.feed(b"\x1b_Gi=0;OK\x1b").is_empty());
        assert_eq!(
            split.feed(b"\\").first(),
            Some(&OuterInputEvent::GraphicsResponse(
                b"\x1b_Gi=0;OK\x1b\\".to_vec()
            ))
        );
    }

    #[test]
    fn graphics_response_broker_keeps_child_and_outer_queues_separate() {
        let mut broker = GraphicsProtocolBroker::new(1);
        assert!(broker.queue_child(b"child".to_vec()));
        assert!(!broker.queue_child(b"overflow".to_vec()));
        assert!(broker.queue_outer(b"outer".to_vec()));
        assert_eq!(broker.pending_child(), 1);
        assert_eq!(broker.pending_outer(), 1);
        assert_eq!(broker.drain_child().next().unwrap().bytes(), b"child");
        assert_eq!(broker.drain_outer().next().unwrap().bytes(), b"outer");
    }

    #[test]
    fn graphics_queries_acknowledge_direct_transfer_and_reject_unsafe_modes() {
        let mut store = SessionGraphicsStore::new(SessionId::new(7));
        assert_eq!(
            store
                .apply_kitty_command_with_context(
                    b"a=q,i=31,t=d,s=1,v=1,f=24",
                    b"MTIz",
                    (0, 0),
                    (10, 20),
                )
                .unwrap(),
            Some(b"\x1b_Gi=31;OK\x1b\\".to_vec())
        );
        let unsupported = store
            .apply_kitty_command_with_context(
                b"a=q,i=32,t=f,s=1,v=1,f=24",
                b"L3RtcA==",
                (0, 0),
                (10, 20),
            )
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&unsupported).contains("ENOTSUP"));
        assert_eq!(store.resource_count(), 0);
    }

    #[test]
    fn natural_gif_dimensions_are_used_when_cell_geometry_is_missing() {
        let mut store = SessionGraphicsStore::new(SessionId::new(21));
        let gif = b"GIF89a\x03\x00\x02\x00";
        store
            .apply_kitty_command_with_context(
                b"a=T,f=100,i=21,q=2",
                &encode_base64_for_test(gif),
                (0, 0),
                (2, 2),
            )
            .unwrap();
        assert_eq!(
            store.visible_submissions(Rect::new(0, 0, 8, 8))[0]
                .placement()
                .area(),
            Rect::new(0, 0, 2, 1)
        );
    }

    fn encode_base64_for_test(bytes: &[u8]) -> Vec<u8> {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut output = Vec::new();
        for chunk in bytes.chunks(3) {
            let value = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            output.push(TABLE[((value >> 18) & 63) as usize]);
            output.push(TABLE[((value >> 12) & 63) as usize]);
            output.push(if chunk.len() > 1 {
                TABLE[((value >> 6) & 63) as usize]
            } else {
                b'='
            });
            output.push(if chunk.len() > 2 {
                TABLE[(value & 63) as usize]
            } else {
                b'='
            });
        }
        output
    }

    #[test]
    fn transmit_and_display_creates_a_cursor_relative_placement() {
        let mut store = SessionGraphicsStore::new(SessionId::new(8));
        assert_eq!(
            store
                .apply_kitty_command_with_context(
                    b"a=T,f=24,i=9,s=20,v=40,q=2",
                    b"AQID",
                    (2, 3),
                    (10, 20),
                )
                .unwrap(),
            None
        );
        let submissions = store.visible_submissions(Rect::new(0, 0, 10, 10));
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].placement().area(), Rect::new(2, 3, 2, 2));
    }

    #[test]
    fn transmit_and_display_allocates_an_internal_id_when_client_omits_one() {
        let mut store = SessionGraphicsStore::new(SessionId::new(9));
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,s=2,v=1,c=2,r=1,q=2",
                b"AQIDBAUG",
                (1, 2),
                (10, 20),
            )
            .unwrap();

        let submissions = store.visible_submissions(Rect::new(0, 0, 8, 8));
        assert_eq!(submissions.len(), 1);
        assert_ne!(submissions[0].resource().image(), 0);
        assert_eq!(submissions[0].placement().area(), Rect::new(1, 2, 2, 1));
    }

    #[test]
    fn multiple_placements_and_placement_ids_have_independent_lifetimes() {
        let mut store = SessionGraphicsStore::new(SessionId::new(15));
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=5,c=1,r=1,q=2",
                b"AQID",
                (0, 0),
                (10, 20),
            )
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=p,i=5,p=10,c=2,r=1,q=2", b"", (2, 0), (10, 20))
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=p,i=5,p=11,c=1,r=2,q=2", b"", (0, 2), (10, 20))
            .unwrap();
        assert_eq!(store.placement_count(), 3);

        store
            .apply_kitty_command_with_context(b"a=p,i=5,p=10,c=3,r=1,q=2", b"", (1, 1), (10, 20))
            .unwrap();
        assert_eq!(store.placement_count(), 3);
        let submissions = store.visible_submissions(Rect::new(0, 0, 8, 8));
        assert!(submissions.iter().any(|item| {
            item.placement().placement_id() == Some(10)
                && item.placement().area() == Rect::new(1, 1, 3, 1)
        }));
        assert!(
            submissions
                .iter()
                .any(|item| item.placement().placement_id() == Some(11))
        );
    }

    #[test]
    fn protocol_errors_include_image_and_placement_context() {
        let response = kitty_error_response(b"a=p,i=99,p=7", &GraphicsError::ImageNotFound(99));
        let response = String::from_utf8(response).unwrap();
        assert!(response.contains("Gi=99,p=7;ENOENT:"));
        assert!(response.contains("image 99"));
    }

    #[test]
    fn direct_queries_fail_closed_when_the_outer_terminal_lacks_kitty_support() {
        let mut store = SessionGraphicsStore::new(SessionId::new(10));
        store.set_outer_kitty_graphics(false);
        let response = store
            .apply_kitty_command_with_context(
                b"a=q,i=1,t=d,s=1,v=1,f=24",
                b"MTIz",
                (0, 0),
                (10, 20),
            )
            .unwrap()
            .unwrap();

        assert!(String::from_utf8_lossy(&response).contains("outer terminal"));
    }

    #[test]
    fn delete_commands_remove_resources_and_placements() {
        let mut store = SessionGraphicsStore::new(SessionId::new(4));
        store.apply_kitty_command(b"a=T,i=1", b"AQID").unwrap();
        store.apply_kitty_command(b"a=d,i=1", b"").unwrap();
        assert_eq!(store.resource_count(), 0);
        assert_eq!(store.placement_count(), 0);
        assert_eq!(store.decoded_bytes(1), None);
    }

    #[test]
    fn clearing_a_store_cancels_pending_uploads_and_reclaims_resources() {
        let mut store = SessionGraphicsStore::new(SessionId::new(22));
        store
            .apply_kitty_command(b"a=T,f=24,i=22,m=1", b"AQID")
            .unwrap();
        assert_eq!(store.resource_count(), 0);
        store
            .apply_kitty_command(b"a=d,d=a", b"")
            .expect("delete-all should clear pending state");
        store
            .apply_kitty_command(b"a=T,f=24,i=22,q=2", b"BAUG")
            .unwrap();
        assert_eq!(store.resource_count(), 1);
        assert_eq!(store.placement_count(), 1);
    }

    #[test]
    fn limits_record_diagnostics_without_corrupting_existing_resources() {
        let limits = GraphicsLimits {
            max_decoded_bytes: 2,
            max_resources: 1,
            max_placements: 1,
        };
        let mut store = SessionGraphicsStore::with_limits(SessionId::new(5), limits);
        store.apply_kitty_command(b"a=T,f=24,i=1", b"AQID").unwrap();
        assert_eq!(store.resource_count(), 0);
        assert_eq!(store.diagnostics().len(), 1);

        store.apply_kitty_command(b"a=T,f=24,i=1", b"AQ").unwrap();
        assert_eq!(store.resource_count(), 1);
        store.apply_kitty_command(b"a=T,f=24,i=2", b"AQ").unwrap();
        assert_eq!(store.resource_count(), 1);
        assert!(store.diagnostics().len() >= 2);
    }

    #[test]
    fn compressed_payloads_are_decoded_before_storage_and_placement() {
        use flate2::{Compression, write::ZlibEncoder};
        use std::io::Write;

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(b"AQID").unwrap();
        let compressed = encoder.finish().unwrap();
        let encoded = encode_base64_for_test(&compressed);
        let mut store = SessionGraphicsStore::new(SessionId::new(23));
        store
            .apply_kitty_command(b"a=T,f=24,i=23,c=1,r=1,o=z,q=2", &encoded)
            .unwrap();
        assert_eq!(store.decoded_bytes(23), Some(&b"AQID"[..]));
        assert_eq!(store.placement_count(), 1);
    }

    #[test]
    fn source_crops_and_cursor_policy_are_retained_as_logical_placement_data() {
        let mut store = SessionGraphicsStore::new(SessionId::new(24));
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=24,s=10,v=10,x=2,y=3,w=4,h=5,C=0,c=2,r=2,q=2",
                b"AQID",
                (1, 1),
                (10, 10),
            )
            .unwrap();
        let submissions = store.visible_submissions(Rect::new(0, 0, 8, 8));
        let placement = submissions[0].placement();
        assert_eq!(
            placement.source(),
            Some(GraphicsSourceRect::new(2, 3, 4, 5))
        );
        assert!(!placement.cursor_static());
    }

    #[test]
    fn animation_frames_and_controls_are_bounded_and_acknowledged() {
        let mut store = SessionGraphicsStore::new(SessionId::new(25));
        store
            .apply_kitty_command(b"a=T,f=24,i=25,c=1,r=1,q=2", b"AQID")
            .unwrap();
        assert_eq!(
            store
                .apply_kitty_command_with_context(b"a=f,i=25,r=2,z=40", b"BAUG", (0, 0), (0, 0))
                .unwrap(),
            Some(b"\x1b_Gi=25;OK\x1b\\".to_vec())
        );
        assert_eq!(store.animation_frame_count(25), Some(1));
        assert_eq!(
            store.animation_frame_bytes(25, 2),
            Some(&b"\x04\x05\x06"[..])
        );
        store.apply_kitty_command(b"a=a,i=25,s=3,c=2", b"").unwrap();
        assert_eq!(
            store.animation_state(25),
            Some(GraphicsAnimationState::Running)
        );
        store.apply_kitty_command(b"a=d,d=f,i=25", b"").unwrap();
        assert_eq!(store.animation_frame_count(25), Some(0));
    }

    #[test]
    fn transfer_negotiation_rejects_file_and_shared_memory_without_claiming_success() {
        let mut store = SessionGraphicsStore::new(SessionId::new(26));
        for transfer in ["f", "s", "t"] {
            let response = store
                .apply_kitty_command_with_context(
                    format!("a=q,i=26,t={transfer},f=100").as_bytes(),
                    b"fixture",
                    (0, 0),
                    (0, 0),
                )
                .unwrap()
                .unwrap();
            assert!(String::from_utf8_lossy(&response).contains("ENOTSUP"));
        }
        assert_eq!(store.resource_count(), 0);
    }

    #[test]
    fn placement_delete_selector_can_remove_one_placement_without_destroying_resource() {
        let mut store = SessionGraphicsStore::new(SessionId::new(27));
        store
            .apply_kitty_command(b"a=T,f=24,i=27,q=2", b"AQID")
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=p,i=27,p=7,q=2", b"", (2, 0), (0, 0))
            .unwrap();
        assert_eq!(store.placement_count(), 2);
        store.apply_kitty_command(b"a=d,d=i,i=27,p=7", b"").unwrap();
        assert_eq!(store.placement_count(), 1);
        assert_eq!(store.resource_count(), 1);
    }

    #[test]
    fn unsupported_formats_are_reported_and_ignored() {
        let mut store = SessionGraphicsStore::new(SessionId::new(6));
        store.apply_kitty_command(b"a=T,f=1,i=1", b"AQID").unwrap();
        assert_eq!(store.resource_count(), 0);
        assert!(store.diagnostics()[0].message().contains("unsupported"));
    }

    #[test]
    fn protocol_adapter_accepts_payloadless_control_actions() {
        let mut adapter = GraphicsProtocolAdapter::default();
        let events = adapter
            .feed(b"\x1b_Ga=f,i=1,r=2\x1b\\\x1b_Ga=d,d=i,i=1\x1b\\")
            .unwrap();
        assert_eq!(adapter.finish().unwrap(), Vec::new());
        assert_eq!(
            events
                .into_iter()
                .filter_map(|event| match event {
                    GraphicsProtocolEvent::Command(command) => Some(command),
                    _ => None,
                })
                .map(|command| command.parameters().to_vec())
                .collect::<Vec<_>>(),
            vec![b"a=f,i=1,r=2".to_vec(), b"a=d,d=i,i=1".to_vec()]
        );
    }
}
