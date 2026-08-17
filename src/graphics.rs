use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    fs::File,
    io::{Read, Seek, SeekFrom},
    time::{Duration, Instant},
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

/// A screen-scoped graphics erase requested by the terminal emulator.
///
/// These are observed from the VT stream (not from the graphics protocol's own
/// delete action) so images are erased in the same scope a real terminal
/// erases text: `ED 2` clears the visible screen, `ED 0`/`ED 1` clear from the
/// cursor to the bottom/top of the screen, `ED 3` clears the scrollback, a
/// reset clears everything, and switching screens clears the alternate buffer.
///
/// The `ED 0`/`ED 1` variants carry the cursor's zero-based grid row so the
/// erase scope matches the emulator's row granularity: like Kitty, a partial
/// screen clear removes whole image rows, not just the cells after the cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphicsErase {
    /// Erase images visible on the given screen (`ED 2`).
    ClearScreen(GraphicsScreen),
    /// Erase images from the cursor row to the bottom of the screen (`ED 0`).
    ClearBelow(GraphicsScreen, u16),
    /// Erase images from the top of the screen to the cursor row (`ED 1`).
    ClearAbove(GraphicsScreen, u16),
    /// Erase images scrolled into the scrollback buffer (`ED 3`).
    ClearScrollback,
    /// Erase all images and resources (RIS reset).
    All,
    /// Erase images on the alternate screen (screen switch).
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

/// The maximum depth of a relative-placement parent chain. Kitty requires a
/// chain of at least 8 to be accepted; exceeding it is reported as `ETOODEEP`.
const MAX_RELATIVE_DEPTH: usize = 8;

/// A relative placement's parent reference and cell offset.
///
/// `image`/`placement_id` identify the parent placement (Kitty's `P`/`Q` keys);
/// `cell_offset_x`/`cell_offset_y` are the `H`/`V` cell offsets from the
/// parent's top-left cell. Positive offsets move right/down, negative left/up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphicsPlacementParent {
    image: u32,
    placement_id: u32,
    cell_offset_x: i32,
    cell_offset_y: i32,
}

impl GraphicsPlacementParent {
    pub const fn new(image: u32, placement_id: u32, cell_offset_x: i32, cell_offset_y: i32) -> Self {
        Self {
            image,
            placement_id,
            cell_offset_x,
            cell_offset_y,
        }
    }

    pub const fn image(self) -> u32 {
        self.image
    }

    pub const fn placement_id(self) -> u32 {
        self.placement_id
    }

    pub const fn cell_offset_x(self) -> i32 {
        self.cell_offset_x
    }

    pub const fn cell_offset_y(self) -> i32 {
        self.cell_offset_y
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
    /// Sub-cell pixel offset within the anchor cell (Kitty `X`), so the image
    /// can be positioned below cell granularity.
    cell_x_offset: u16,
    /// Sub-cell pixel offset within the anchor cell (Kitty `Y`).
    cell_y_offset: u16,
    /// The on-screen pixel size the source rectangle is scaled to. Kitty aligns
    /// the drawn image to the bottom-right of the placement's cell extent, so
    /// the sub-cell offset is subtracted from the cell span.
    drawn_width: u32,
    drawn_height: u32,
    /// Terminal cell pixel size captured when the placement was created, used
    /// to convert cell-aligned occlusion clips into pixel coordinates.
    cell_width_pixels: u16,
    cell_height_pixels: u16,
    /// When `Some`, this placement is positioned relative to another placement
    /// (Kitty's `P`/`Q`/`H`/`V` keys) instead of the terminal cursor. Its cell
    /// origin is derived from the parent at render time, and its lifetime is
    /// tied to the parent's.
    parent: Option<GraphicsPlacementParent>,
    /// Whether this is a virtual placement (Kitty `U=1`): an invisible
    /// prototype for Unicode-placeholder images. Virtual placements never
    /// render, never scroll, and are only deleted by the id/number/range
    /// selectors (`i/I`, `n/N`, `r/R`), matching Kitty's `is_virtual_ref`.
    virtual_placement: bool,
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

    pub const fn cell_x_offset(&self) -> u16 {
        self.cell_x_offset
    }

    pub const fn cell_y_offset(&self) -> u16 {
        self.cell_y_offset
    }

    pub const fn drawn_width(&self) -> u32 {
        self.drawn_width
    }

    pub const fn drawn_height(&self) -> u32 {
        self.drawn_height
    }

    pub const fn cell_width_pixels(&self) -> u16 {
        self.cell_width_pixels
    }

    pub const fn cell_height_pixels(&self) -> u16 {
        self.cell_height_pixels
    }

    pub const fn anchor(&self) -> GraphicsGridAnchor {
        self.anchor
    }

    pub const fn parent(&self) -> Option<GraphicsPlacementParent> {
        self.parent
    }

    pub const fn is_virtual(&self) -> bool {
        self.virtual_placement
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

/// A Unicode-placeholder cell observed in the child's text grid.
///
/// Kitty's `U=1` virtual placements have no physical cell of their own; their
/// real location is the set of U+10EEEE placeholder glyphs the client writes
/// into the grid. The store tracks those cells so a relative placement
/// anchored to a virtual parent resolves against the min x / min y of the
/// placeholder cells (Kitty's `resolve_cell_ref`) instead of the cursor that
/// happened to be active when the virtual placement was created.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphicsPlaceholderCell {
    column: u16,
    row: u16,
    scrollback: usize,
}

impl GraphicsPlaceholderCell {
    pub const fn new(column: u16, row: u16, scrollback: usize) -> Self {
        Self {
            column,
            row,
            scrollback,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphicsSubmission {
    resource: GraphicsResourceId,
    format: u8,
    generation: u64,
    encoded_payload: Vec<u8>,
    pixel_width: u32,
    pixel_height: u32,
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
    ClipboardResponse(Vec<u8>),
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
            let osc = self.pending[index..]
                .windows(2)
                .position(|window| window == b"\x1b]");
            let apc = self.pending[index..]
                .windows(3)
                .position(|window| window == b"\x1b_G");
            let Some(offset) = [csi, osc, apc].into_iter().flatten().min() else {
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
            if self.pending[start..].starts_with(b"\x1b]") {
                // OSC is terminated by BEL or ST; clipboard read responses
                // (`ESC ] 52 ; ...`) are routed separately so they never leak
                // into the keyboard decoder.
                let rest = &self.pending[start + 2..];
                let bel = rest.iter().position(|byte| *byte == 0x07);
                let st = find_bytes(rest, b"\x1b\\");
                let Some(term_offset) = (match (bel, st) {
                    (Some(bel), Some(st)) => Some(bel.min(st)),
                    (Some(offset), None) | (None, Some(offset)) => Some(offset),
                    (None, None) => None,
                }) else {
                    self.pending = self.pending[start..].to_vec();
                    return events;
                };
                let term_len = usize::from(rest[term_offset] == 0x1b) + 1;
                let end = start + 2 + term_offset + term_len;
                let sequence = &self.pending[start..end];
                if sequence.starts_with(b"\x1b]52;") {
                    events.push(OuterInputEvent::ClipboardResponse(sequence.to_vec()));
                } else {
                    events.push(OuterInputEvent::TerminalInput(sequence.to_vec()));
                }
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

    pub const fn pixel_width(&self) -> u32 {
        self.pixel_width
    }

    pub const fn pixel_height(&self) -> u32 {
        self.pixel_height
    }

    pub const fn placement(&self) -> &GraphicsPlacement {
        &self.placement
    }

    pub fn terminal_image_id(&self) -> u32 {
        terminal_image_id(self.resource)
    }

    pub fn clipped_to(&self, clip: Rect) -> Option<Self> {
        let area = intersect(self.placement.area(), clip)?;
        let clipped = clip_placement(
            &self.placement,
            u32::from(area.x.saturating_sub(self.placement.x())),
            u32::from(area.y.saturating_sub(self.placement.y())),
            area.width,
            area.height,
            (self.pixel_width, self.pixel_height),
        )?;
        Some(Self {
            resource: self.resource,
            format: self.format,
            generation: self.generation,
            encoded_payload: self.encoded_payload.clone(),
            pixel_width: self.pixel_width,
            pixel_height: self.pixel_height,
            placement: GraphicsPlacement {
                x: area.x,
                y: area.y,
                width: area.width,
                height: area.height,
                source: clipped.source,
                cell_x_offset: clipped.cell_x_offset,
                cell_y_offset: clipped.cell_y_offset,
                drawn_width: clipped.drawn_width,
                drawn_height: clipped.drawn_height,
                ..self.placement
            },
        })
    }
}

/// The pixel-accurate result of clipping a placement to a cell-aligned region:
/// the visible source crop plus the sub-cell geometry the clipped placement
/// must re-anchor at so a subsequent clip stays exact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PlacementClip {
    source: Option<GraphicsSourceRect>,
    cell_x_offset: u16,
    cell_y_offset: u16,
    drawn_width: u32,
    drawn_height: u32,
}

/// Computes the source rectangle (in pixels) for the visible sub-region of a
/// placement, so an occluded placement is rendered from the correct part of the
/// image instead of re-scaling the whole image into the clipped cells.
///
/// `offset_x`/`offset_y` are the clipped region's displacement from the
/// placement's top-left cell (in cells) and `clip_width`/`clip_height` its
/// extent. The image itself sits at a sub-cell `X`/`Y` pixel offset, so the
/// crop is derived in pixel space rather than as whole-cell fractions of the
/// source. When the clip covers the full placement the original source and
/// offsets are returned unchanged so the common case emits no crop parameters.
fn clip_placement(
    placement: &GraphicsPlacement,
    offset_x: u32,
    offset_y: u32,
    clip_width: u16,
    clip_height: u16,
    pixel_size: (u32, u32),
) -> Option<PlacementClip> {
    let full_width = u32::from(placement.width());
    let full_height = u32::from(placement.height());
    if offset_x == 0
        && offset_y == 0
        && u32::from(clip_width) == full_width
        && u32::from(clip_height) == full_height
    {
        return Some(PlacementClip {
            source: placement.source(),
            cell_x_offset: placement.cell_x_offset(),
            cell_y_offset: placement.cell_y_offset(),
            drawn_width: placement.drawn_width(),
            drawn_height: placement.drawn_height(),
        });
    }
    let (source_x, source_y, source_width, source_height) = match placement.source() {
        Some(source) => (source.x(), source.y(), source.width(), source.height()),
        None => (0, 0, pixel_size.0, pixel_size.1),
    };
    // Without a source size there is nothing to crop: keep the original
    // source (usually `None`) and only re-anchor the clipped cell extent.
    if source_width == 0 || source_height == 0 {
        return Some(PlacementClip {
            source: placement.source(),
            cell_x_offset: if offset_x == 0 {
                placement.cell_x_offset()
            } else {
                0
            },
            cell_y_offset: if offset_y == 0 {
                placement.cell_y_offset()
            } else {
                0
            },
            drawn_width: placement.drawn_width(),
            drawn_height: placement.drawn_height(),
        });
    }
    let cell_width = u32::from(placement.cell_width_pixels());
    let cell_height = u32::from(placement.cell_height_pixels());
    let drawn_width = placement.drawn_width();
    let drawn_height = placement.drawn_height();
    // No cell pixel size or drawn size means the sub-cell geometry is unknown;
    // fall back to whole-cell fractions of the source.
    if cell_width == 0 || cell_height == 0 || drawn_width == 0 || drawn_height == 0 {
        let left = u64::from(source_x)
            + u64::from(offset_x) * u64::from(source_width) / u64::from(full_width.max(1));
        let top = u64::from(source_y)
            + u64::from(offset_y) * u64::from(source_height) / u64::from(full_height.max(1));
        let right = u64::from(source_x)
            + (u64::from(offset_x) + u64::from(clip_width)) * u64::from(source_width)
                / u64::from(full_width.max(1));
        let bottom = u64::from(source_y)
            + (u64::from(offset_y) + u64::from(clip_height)) * u64::from(source_height)
                / u64::from(full_height.max(1));
        return finish_placement_clip(
            (left, top, right.saturating_sub(left), bottom.saturating_sub(top)),
            (
                if offset_x == 0 {
                    placement.cell_x_offset()
                } else {
                    0
                },
                if offset_y == 0 {
                    placement.cell_y_offset()
                } else {
                    0
                },
            ),
            (drawn_width, drawn_height),
        );
    }
    // Kitty clamps sub-cell offsets to the last pixel of the anchor cell.
    let x_off = u32::from(placement.cell_x_offset()).min(cell_width.saturating_sub(1));
    let y_off = u32::from(placement.cell_y_offset()).min(cell_height.saturating_sub(1));
    // The drawn image spans `[x_off, x_off + drawn_width)` pixels within the
    // placement's cell extent; intersect that with the clip's cell rectangle
    // converted to pixels, then map the intersection back into source pixels.
    let visible_left = x_off.max(offset_x.saturating_mul(cell_width));
    let visible_top = y_off.max(offset_y.saturating_mul(cell_height));
    let visible_right = x_off.saturating_add(drawn_width).min(
        offset_x
            .saturating_add(u32::from(clip_width))
            .saturating_mul(cell_width),
    );
    let visible_bottom = y_off.saturating_add(drawn_height).min(
        offset_y
            .saturating_add(u32::from(clip_height))
            .saturating_mul(cell_height),
    );
    let left = source_x.saturating_add(scale_into_source(
        visible_left.saturating_sub(x_off),
        source_width,
        drawn_width,
    ));
    let top = source_y.saturating_add(scale_into_source(
        visible_top.saturating_sub(y_off),
        source_height,
        drawn_height,
    ));
    let right = source_x.saturating_add(scale_into_source(
        visible_right.saturating_sub(x_off),
        source_width,
        drawn_width,
    ));
    let bottom = source_y.saturating_add(scale_into_source(
        visible_bottom.saturating_sub(y_off),
        source_height,
        drawn_height,
    ));
    // The clipped placement re-anchors at the clip's top-left cell, so only
    // the sub-cell remainder of the original offset survives. It is always
    // zero once the clip moves past the placement's first cell.
    let cell_x_offset =
        (x_off.saturating_sub(offset_x.saturating_mul(cell_width))).min(u32::from(u16::MAX)) as u16;
    let cell_y_offset =
        (y_off.saturating_sub(offset_y.saturating_mul(cell_height))).min(u32::from(u16::MAX)) as u16;
    finish_placement_clip(
        (
            u64::from(left),
            u64::from(top),
            u64::from(right.saturating_sub(left)),
            u64::from(bottom.saturating_sub(top)),
        ),
        (cell_x_offset, cell_y_offset),
        (
            visible_right.saturating_sub(visible_left),
            visible_bottom.saturating_sub(visible_top),
        ),
    )
}

fn finish_placement_clip(
    crop: (u64, u64, u64, u64),
    cell_offset: (u16, u16),
    drawn: (u32, u32),
) -> Option<PlacementClip> {
    let (crop_x, crop_y, width, height) = crop;
    if width == 0 || height == 0 {
        return None;
    }
    Some(PlacementClip {
        source: Some(GraphicsSourceRect::new(
            crop_x.min(u64::from(u32::MAX)) as u32,
            crop_y.min(u64::from(u32::MAX)) as u32,
            width.min(u64::from(u32::MAX)) as u32,
            height.min(u64::from(u32::MAX)) as u32,
        )),
        cell_x_offset: cell_offset.0,
        cell_y_offset: cell_offset.1,
        drawn_width: drawn.0,
        drawn_height: drawn.1,
    })
}

/// Scales a pixel delta within the drawn extent back into the source image's
/// pixel coordinates, matching the inverse of the placement's source-to-screen
/// scaling.
fn scale_into_source(delta_pixels: u32, source_span: u32, drawn_span: u32) -> u32 {
    (u128::from(delta_pixels) * u128::from(source_span) / u128::from(drawn_span.max(1)))
        .min(u128::from(u32::MAX)) as u32
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
    ParentNotFound(u32),
    RelativeCycle,
    RelativeDepthExceeded,
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
            Self::ParentNotFound(image) => {
                write!(formatter, "Kitty graphics parent placement for image {image} was not found")
            }
            Self::RelativeCycle => {
                formatter.write_str("Kitty graphics relative placement would create a cycle")
            }
            Self::RelativeDepthExceeded => {
                formatter.write_str("Kitty graphics relative placement chain exceeds the maximum depth")
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
    /// The raw transmitted rectangle bytes for a delta frame, or the full
    /// coalesced pixel buffer for a standalone keyframe.
    payload: Vec<u8>,
    gap_ms: Option<i32>,
    /// Kitty `a=f` frame-composition metadata. `x`/`y`/`width`/`height` are
    /// the pixel rectangle this frame's payload occupies within the full
    /// image; `base_frame` is the `c` frame the delta is composed onto (0 for
    /// a standalone frame); `compose_mode` is `X` (0 alpha-blend, 1 replace);
    /// and `bgcolor` is the `Y` background canvas color for standalone frames.
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    base_frame: u32,
    compose_mode: u32,
    bgcolor: Option<u32>,
    /// Kitty's per-frame `N=1` transient hint. Unlike the resource-level
    /// hint (which drives eviction), this is tracked per frame so composition
    /// can mark a result transient when any of its source frames is transient.
    transient: bool,
}

impl GraphicsAnimationFrame {
    /// Whether this frame already holds the full image at the origin, with no
    /// base frame to coalesce and no background canvas to fill. Such a frame
    /// is a keyframe that can be read directly without composition.
    fn is_full_keyframe(&self, image_width: u32, image_height: u32) -> bool {
        self.base_frame == 0
            && self.bgcolor.is_none()
            && self.x == 0
            && self.y == 0
            && self.width == image_width
            && self.height == image_height
    }
}

/// A coalesced animation frame's pixel buffer plus its per-frame transient
/// flag, carried alongside Kitty's `CoalescedFrameData`.
struct CoalescedFrame {
    payload: Vec<u8>,
    transient: bool,
}

/// Kitty's default inter-frame gap in milliseconds.
const DEFAULT_ANIMATION_GAP_MS: u32 = 40;

/// Kitty's frame-gap normalization: a positive `z` is used verbatim, an absent
/// or zero `z` falls back to `default_ms`, and a negative `z` collapses to a
/// gapless (0 ms) frame.
fn normalized_gap_ms(gap: Option<i32>, default_ms: u32) -> u32 {
    match gap {
        None => default_ms,
        Some(gap) if gap > 0 => gap as u32,
        Some(0) => default_ms,
        Some(_) => 0,
    }
}

/// Total animation duration across every frame (root plus extras), matching
/// Kitty's `animation_duration`. An animation with zero duration never plays.
fn animation_duration_ms(resource: &GraphicsResource) -> u32 {
    let mut total = normalized_gap_ms(resource.animation_root_gap_ms, 0);
    for frame in resource.animation_frames.values() {
        total = total.saturating_add(normalized_gap_ms(
            frame.gap_ms,
            DEFAULT_ANIMATION_GAP_MS,
        ));
    }
    total
}

/// The effective gap of one frame in the playback sequence. Frame 1 is the
/// root frame (default gap 0); extra frames default to `DEFAULT_ANIMATION_GAP_MS`.
fn frame_gap_ms(resource: &GraphicsResource, frame: u32) -> u32 {
    if frame == 1 {
        normalized_gap_ms(resource.animation_root_gap_ms, 0)
    } else {
        resource
            .animation_frames
            .get(&frame)
            .map_or(DEFAULT_ANIMATION_GAP_MS, |animation_frame| {
                normalized_gap_ms(animation_frame.gap_ms, DEFAULT_ANIMATION_GAP_MS)
            })
    }
}

/// Whether a resource's animation should keep advancing on the scheduler,
/// mirroring Kitty's `image_is_animatable`: not stopped, has extra frames, a
/// non-zero total duration, and (for `Running`) an unexhausted loop count. A
/// `Loading` animation plays through once and stops when it wraps to the root.
fn animation_is_animatable(resource: &GraphicsResource) -> bool {
    if resource.animation_state == GraphicsAnimationState::Stopped
        || resource.animation_frames.is_empty()
        || animation_duration_ms(resource) == 0
    {
        return false;
    }
    let max_loops = resource.animation_loops.saturating_sub(1);
    match resource.animation_state {
        GraphicsAnimationState::Stopped => false,
        GraphicsAnimationState::Loading => resource.animation_current_loop == 0,
        GraphicsAnimationState::Running => {
            max_loops == 0 || resource.animation_current_loop < max_loops
        }
    }
}

/// The next frame in the playback sequence `1, 2, ..., frame_count`, returning
/// whether the sequence wrapped back to the root frame.
fn next_animation_frame(frame_count: u32, frame: u32) -> (u32, bool) {
    if frame < frame_count {
        (frame + 1, false)
    } else {
        (1, true)
    }
}

/// Parameters for a Kitty `a=c` animation frame composition.
#[derive(Clone, Copy, Debug)]
struct AnimationCompose {
    source_frame: u32,
    destination_frame: u32,
    source_offset: (u32, u32),
    destination_offset: (u32, u32),
    compose_mode: u32,
    width: u32,
    height: u32,
}

#[derive(Clone, Debug)]
struct GraphicsResource {
    format: u8,
    generation: u64,
    pixel_width: u32,
    pixel_height: u32,
    /// The Kitty `I` image number the image was created with (0 when created
    /// with an explicit `i` id). Used to resolve `I` references to the newest
    /// image with that number.
    image_number: u32,
    /// The Kitty `N=1` transient usage hint: a transient image is a good
    /// eviction candidate under storage pressure.
    transient: bool,
    decoded_payload: Vec<u8>,
    encoded_payload: Vec<u8>,
    animation_frames: BTreeMap<u32, GraphicsAnimationFrame>,
    animation_state: GraphicsAnimationState,
    animation_current_frame: u32,
    /// The Kitty `v` animation-control key: `0` is unspecified, `1` loops
    /// forever, and any larger value loops `v - 1` times.
    animation_loops: u32,
    /// The Kitty `a=a,r=1,z=<gap>` root-frame gap (0 when absent, matching
    /// Kitty's zero-initialized root frame).
    animation_root_gap_ms: Option<i32>,
    /// When the currently-displayed animation frame was shown; `None` until the
    /// animation first advances.
    animation_frame_started_at: Option<Instant>,
    /// Number of completed animation loops (Kitty's `current_loop`).
    animation_current_loop: u32,
    /// Bumped each time the served animation frame changes so the outer
    /// terminal re-uploads the new pixel payload.
    animation_revision: u64,
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
    /// Unicode-placeholder cells observed in the child's text grid, keyed by
    /// image id. Refreshed by the session after each plain-output chunk so a
    /// relative placement can anchor to its virtual parent's placeholder
    /// cells as they scroll and move with the grid.
    placeholder_cells: BTreeMap<u32, Vec<GraphicsPlaceholderCell>>,
    /// Cursor advancement (columns, rows) implied by the most recent
    /// placement command, when the client used the default moving-cursor
    /// policy (`C=0`). A real Kitty terminal moves the cursor right by the
    /// placement columns and down by its rows after placing an image, so
    /// trailing text and subsequent images do not overlap it.
    last_cursor_advance: Option<(u16, u16)>,
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
            placeholder_cells: BTreeMap::new(),
            last_cursor_advance: None,
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

    /// Replaces the tracked Unicode-placeholder cells (keyed by image id) with
    /// a fresh scan of the child's text grid. The session calls this after
    /// feeding plain output so a relative placement anchored to a virtual
    /// (`U=1`) parent resolves against the placeholder glyphs' current cells.
    pub fn set_placeholder_cells(
        &mut self,
        cells: BTreeMap<u32, Vec<GraphicsPlaceholderCell>>,
    ) {
        self.placeholder_cells = cells;
    }

    /// Total number of tracked Unicode-placeholder cells across all images.
    pub fn placeholder_cell_count(&self) -> usize {
        self.placeholder_cells.values().map(Vec::len).sum()
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

    /// The per-frame `N=1` transient hint for a frame. Frame 1 reports the
    /// resource-level (root frame) hint; extra frames report their own stored
    /// hint, propagated from any composed source frame.
    pub fn animation_frame_transient(&self, image: u32, frame: u32) -> Option<bool> {
        let resource = self.resources.get(&image)?;
        if frame == 1 {
            Some(resource.transient)
        } else {
            resource.animation_frames.get(&frame).map(|frame| frame.transient)
        }
    }

    /// The fully coalesced pixel buffer for an animation frame, applying any
    /// `a=f` frame-composition metadata (base frame, background canvas, and
    /// rectangle offsets). Frame 1 is the root frame. Returns `None` for an
    /// unknown image/frame or a non-raw (PNG/GIF) image whose frames cannot
    /// be coalesced byte-for-byte.
    pub fn coalesced_frame_bytes(&self, image: u32, frame: u32) -> Option<Vec<u8>> {
        self.coalesce_frame(image, frame).ok()
    }

    pub fn animation_loops(&self, image: u32) -> Option<u32> {
        self.resources.get(&image).map(|resource| resource.animation_loops)
    }

    pub fn animation_current_frame(&self, image: u32) -> Option<u32> {
        self.resources
            .get(&image)
            .map(|resource| resource.animation_current_frame)
    }

    pub fn animation_revision(&self, image: u32) -> Option<u64> {
        self.resources
            .get(&image)
            .map(|resource| resource.animation_revision)
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
        self.placeholder_cells.clear();
    }

    /// Evicts full-screen primary placements that have scrolled above the top
    /// of the retained scrollback history, and releases the decoded bytes of
    /// any image whose last placement is removed. This mirrors Kitty's
    /// `grman_scroll_images` free-past-limit behavior so a bounded scrollback
    /// cannot keep image data alive indefinitely.
    ///
    /// `current_region_scroll` is the monotonic scroll displacement tracked by
    /// the session's scroll observer; it keeps counting after the history cap
    /// is reached, which is what lets a placement be recognized as fully
    /// scrolled out. Returns whether any placement was evicted.
    pub fn evict_beyond_scrollback_limit(
        &mut self,
        scrollback_limit: usize,
        current_screen: GraphicsScreen,
        current_region: GraphicsScrollRegion,
        current_region_scroll: i64,
    ) -> bool {
        let limit = i64::try_from(scrollback_limit).unwrap_or(i64::MAX);
        let mut evicted = false;
        let mut unreferenced_images = BTreeSet::new();

        self.placements.retain(|_, placement| {
            // Relative placements follow their parent's lifetime rather than
            // their own scroll position; they are removed by the orphan prune
            // below once a scrolled-out parent is evicted.
            if placement.parent().is_some() {
                return true;
            }
            // Virtual placements never scroll, so a scrollback limit can never
            // evict them (Kitty's scroll filter skips virtual refs).
            if placement.is_virtual() {
                return true;
            }
            let anchor = placement.anchor();
            // Only full-screen primary placements live in the shared scrollback
            // buffer and are subject to the history limit; partial-region
            // placements stay anchored to their live region.
            let participates = anchor.screen() == current_screen
                && anchor.scroll_region().is_full_screen()
                && current_region.is_full_screen();
            if !participates {
                return true;
            }
            let scroll_delta = current_region_scroll.saturating_sub(anchor.region_scroll());
            if scroll_delta > i64::from(anchor.row()).saturating_add(limit) {
                unreferenced_images.insert(placement.resource().image());
                evicted = true;
                return false;
            }
            true
        });
        if self.prune_orphaned_relatives() {
            evicted = true;
        }

        for image in unreferenced_images {
            if self
                .placements
                .values()
                .all(|placement| placement.resource().image() != image)
                && let Some(resource) = self.resources.remove(&image)
            {
                self.decoded_bytes = self
                    .decoded_bytes
                    .saturating_sub(resource_storage_bytes(&resource));
                if self.last_image_id == Some(image) {
                    self.last_image_id = None;
                }
            }
        }

        evicted
    }

    /// Re-anchors image placements after a terminal resize so they keep
    /// tracking the cell grid exactly like a real graphics terminal.
    ///
    /// A column change makes `alacritty_terminal` reflow (rewrap) text, which
    /// moves lines in and out of the scrollback without scrolling content
    /// uniformly. Our anchor resolution derives the screen row from the
    /// scrollback depth, so that rewrap would otherwise spuriously shift every
    /// full-screen placement. Re-capturing each placement's current grid row
    /// against the new scrollback depth preserves it, matching how Kitty keeps
    /// a placement's `start_row` through a reflow.
    ///
    /// Row-only resizes are already modeled by the scrollback-based anchor
    /// resolution (the emulator pushes whole lines into history), so they are
    /// intentionally left untouched. Partial-region placements and relative
    /// placements are also left alone: the former never enter the shared
    /// scrollback window, and the latter follow their re-anchored parent.
    pub fn reanchor_on_resize(
        &mut self,
        old_columns: u16,
        new_columns: u16,
        old_scrollback: usize,
        new_scrollback: usize,
        old_region: GraphicsScrollRegion,
        old_region_scroll: i64,
    ) -> bool {
        if old_columns == new_columns {
            return false;
        }
        let new_scrollback_i64 = i64::try_from(new_scrollback).unwrap_or(i64::MAX);
        let mut changed = false;
        for placement in self.placements.values_mut() {
            if placement.parent.is_some()
                || placement.is_virtual()
                || !placement.anchor.scroll_region().is_full_screen()
            {
                continue;
            }
            let anchor = placement.anchor;
            let row = anchor.resolve_row_with_state(old_scrollback, old_region, old_region_scroll);
            // Fold a negative (in-history) row into the captured scrollback so
            // the placement keeps resolving to the same position above the top
            // of the visible screen.
            let (new_row, new_scrollback) = if row >= 0 {
                (u16::try_from(row).unwrap_or(u16::MAX), new_scrollback)
            } else {
                (
                    0,
                    new_scrollback_i64
                        .saturating_sub(i64::from(row.saturating_neg()))
                        .max(0) as usize,
                )
            };
            placement.anchor = GraphicsGridAnchor::new(anchor.column(), new_row, new_scrollback)
                .with_screen(anchor.screen())
                .with_scroll_region(anchor.scroll_region(), anchor.region_scroll());
            changed = true;
        }
        changed
    }

    /// Applies a terminal-emulator erase operation, removing placements in the
    /// same scope a real terminal erases text. Returns whether anything was
    /// removed.
    pub fn apply_erase(
        &mut self,
        erase: GraphicsErase,
        current_scrollback: usize,
        current_region: GraphicsScrollRegion,
        current_region_scroll: i64,
    ) -> bool {
        match erase {
            GraphicsErase::ClearScreen(screen) => self.erase_visible(
                screen,
                current_scrollback,
                current_region,
                current_region_scroll,
            ),
            GraphicsErase::ClearBelow(screen, cursor_row) => self.erase_rows(
                screen,
                current_scrollback,
                current_region,
                current_region_scroll,
                |row| row >= i32::from(cursor_row),
            ),
            GraphicsErase::ClearAbove(screen, cursor_row) => self.erase_rows(
                screen,
                current_scrollback,
                current_region,
                current_region_scroll,
                |row| row >= 0 && row <= i32::from(cursor_row),
            ),
            GraphicsErase::ClearScrollback => self.erase_rows(
                GraphicsScreen::Primary,
                current_scrollback,
                current_region,
                current_region_scroll,
                |row| row < 0,
            ),
            GraphicsErase::All => {
                let before = self.placements.len() + self.resources.len();
                self.clear();
                self.placements.len() + self.resources.len() != before
            }
            GraphicsErase::Alternate => self.erase_screen(GraphicsScreen::Alternate),
        }
    }

    /// Removes placements on `screen` whose resolved row is within the visible
    /// viewport (`ED 2`). History placements (negative rows) are retained so a
    /// scrolled-away image survives a screen clear, exactly as in Kitty.
    fn erase_visible(
        &mut self,
        screen: GraphicsScreen,
        current_scrollback: usize,
        current_region: GraphicsScrollRegion,
        current_region_scroll: i64,
    ) -> bool {
        self.erase_rows(
            screen,
            current_scrollback,
            current_region,
            current_region_scroll,
            |row| row >= 0,
        )
    }

    /// Removes placements on `screen` whose resolved row satisfies `within`.
    /// Relative children resolve through their parent chain, so a child of an
    /// erased parent is removed too. Pixel data is deliberately retained: the
    /// triggering operation decides whether resources are reclaimed.
    fn erase_rows(
        &mut self,
        screen: GraphicsScreen,
        current_scrollback: usize,
        current_region: GraphicsScrollRegion,
        current_region_scroll: i64,
        mut within: impl FnMut(i32) -> bool,
    ) -> bool {
        let mut to_remove = Vec::new();
        for (key, placement) in &self.placements {
            if placement.anchor.screen() != screen {
                continue;
            }
            // Virtual placements have no physical location, so screen erases
            // (`ED 0/1/2/3`) and `d=a`/`d=A` never affect them.
            if placement.is_virtual() {
                continue;
            }
            let matches = self
                .resolve_origin(
                    placement,
                    current_scrollback,
                    current_region,
                    current_region_scroll,
                    0,
                    0,
                )
                .is_some_and(|(_, row)| within(row));
            if matches {
                to_remove.push(*key);
            }
        }
        let mut removed = false;
        for key in to_remove {
            if self.placements.remove(&key).is_some() {
                removed = true;
            }
        }
        removed | self.prune_orphaned_relatives()
    }

    /// Removes every placement on `screen`, used when the alternate screen is
    /// reset on entry.
    fn erase_screen(&mut self, screen: GraphicsScreen) -> bool {
        let before = self.placements.len();
        self.placements
            .retain(|_, placement| placement.anchor.screen() != screen);
        (self.placements.len() != before) | self.prune_orphaned_relatives()
    }

    pub fn record_diagnostic(&mut self, image: Option<u32>, message: impl Into<String>) {
        self.diagnose(image, message);
    }

    pub fn set_outer_kitty_graphics(&mut self, supported: bool) {
        self.outer_kitty_graphics = supported;
    }

    /// Returns and clears the cursor advancement recorded by the most recent
    /// successful placement command. Callers feed the equivalent cursor
    /// movement into the child terminal emulator so that a displayed image
    /// behaves exactly as it would in a real graphics terminal.
    pub fn take_last_cursor_advance(&mut self) -> Option<(u16, u16)> {
        self.last_cursor_advance.take()
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
        // A command that does not create a placement never moves the cursor.
        self.last_cursor_advance = None;
        let values = parse_parameters(parameters)?;
        let action = values
            .get("a")
            .and_then(|value| value.as_bytes().first())
            .copied();
        // Kitty's `i` (image id) and `I` (image number) keys are mutually
        // exclusive: `I` allocates a fresh id on transmit and otherwise
        // resolves to the newest image with that number.
        let image_number = parameter_u32(&values, "I", 0)?;
        if values.contains_key("i") && values.contains_key("I") {
            return Err(GraphicsError::InvalidParameter(
                "i and I are mutually exclusive".to_owned(),
            ));
        }
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
        if image == 0
            && image_number == 0
            && matches!(action, Some(b'f' | b'F' | b'a' | b'A' | b'c' | b'C'))
        {
            image = self.last_image_id.unwrap_or(0);
        }
        // `I=N` on a non-transmit command resolves to the newest existing
        // image with that number; transmit commands allocate a fresh id below.
        if image_number != 0 && !matches!(action, Some(b'T' | b't')) {
            image = self
                .resolve_image_number(image_number)
                .ok_or(GraphicsError::ImageNotFound(image_number))?;
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
            // Kitty's `handle_command` requires an explicit `i=` image id
            // for a query command (`q_iid = g->id`); without one it logs an
            // error and emits no response at all.
            if requested_image == 0 {
                self.diagnose(None, "Kitty graphics query without an image id; ignoring");
                return Ok(None);
            }
            let transfer = values.get("t").map(String::as_str).unwrap_or("d");
            let supported = matches!(transfer, "d" | "f" | "s" | "t");
            if !supported || !self.outer_kitty_graphics {
                let (message, is_ok) = if supported {
                    ("ENOTSUP:outer terminal does not support Kitty graphics", false)
                } else {
                    ("ENOTSUP:unsupported Kitty transfer mode", false)
                };
                if !should_emit_response(quiet, is_ok) {
                    return Ok(None);
                }
                return Ok(Some(kitty_response(image, None, message)));
            }
            // A query loads and validates the payload exactly like a
            // transmit (`handle_add_command` with `is_query=true`), and
            // replies OK only when the image would load: the transfer
            // medium is resolved (base64/zlib or file/shared-memory read),
            // the format is checked, and raw RGB/RGBA data must carry
            // exactly `bpp * s * v` bytes (Kitty's `process_image_data`
            // data-size check). The image is not retained afterwards.
            let decoded_payload = resolve_transfer_payload(
                &values,
                encoded_payload,
                compression,
                self.limits.max_decoded_bytes,
            )?;
            if decoded_payload.len() > self.limits.max_decoded_bytes {
                return Err(GraphicsError::InvalidPayload);
            }
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
                return Err(GraphicsError::InvalidParameter(format!(
                    "unsupported Kitty graphics format {format}"
                )));
            }
            let width = parameter_u32(&values, "s", 0)?;
            let height = parameter_u32(&values, "v", 0)?;
            if matches!(format, 24 | 32) {
                if width == 0 || height == 0 {
                    return Err(GraphicsError::InvalidParameter(
                        "s/v dimensions are required for raw query payloads".to_owned(),
                    ));
                }
                const MAX_IMAGE_DIMENSION: u32 = 10000;
                if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
                    return Err(GraphicsError::InvalidParameter(
                        "query image dimensions exceed the Kitty maximum".to_owned(),
                    ));
                }
                let bytes_per_pixel = if format == 24 { 3 } else { 4 };
                let required = usize::try_from(width).unwrap_or(usize::MAX)
                    * usize::try_from(height).unwrap_or(usize::MAX)
                    * bytes_per_pixel;
                if decoded_payload.len() != required {
                    return Err(GraphicsError::InvalidPayload);
                }
            } else if natural_dimensions(&decoded_payload, 100)
                .or_else(|| natural_dimensions(&decoded_payload, 24))
                .is_none()
            {
                // f=100 PNG/GIF payloads must carry a parseable header.
                return Err(GraphicsError::InvalidPayload);
            }
            if should_emit_response(quiet, true) {
                return Ok(Some(kitty_response(image, None, "OK")));
            }
            return Ok(None);
        }

        let mut response = None;
        match action {
            Some(b'T') | Some(b't') => {
                // Transmitting with an image number always allocates a fresh
                // id, so the number can be reused without colliding with the
                // image it previously referred to.
                let image = if image_number != 0 || image == 0 {
                    self.allocate_internal_image_id()?
                } else {
                    image
                };
                let decoded_payload = resolve_transfer_payload(
                    &values,
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
                // Animated GIF auto-animation: when an `f=100` payload decodes
                // to more than one GIF frame, expand it into coalesced RGBA
                // frames (the root frame plus one animation frame per extra GIF
                // frame) so it plays back the way a graphical terminal would.
                let decoded_gif = if format == 100 {
                    decode_gif_animation(&decoded_payload, self.limits.max_decoded_bytes)
                } else {
                    None
                };
                // Kitty's `N` key is a usage-hint bitmask; the `N=1` bit marks
                // an image (and its auto-extracted frames) transient so it is
                // evicted before retained ones.
                let transient = parameter_u32(&values, "N", 0)? & 1 != 0;
                let (pixel_width, pixel_height, total_storage_bytes) = if let Some(gif) = &decoded_gif {
                    (
                        gif.width,
                        gif.height,
                        gif.root_rgba.len().saturating_add(
                            gif.extra_frames
                                .iter()
                                .map(|frame| frame.rgba.len())
                                .sum::<usize>(),
                        ),
                    )
                } else {
                    let natural = natural_dimensions(&decoded_payload, format);
                    (
                        parameter_u32(&values, "s", natural.map_or(0, |size| size.0))?,
                        parameter_u32(&values, "v", natural.map_or(0, |size| size.1))?,
                        decoded_payload.len(),
                    )
                };
                let (animation_frames, animation_state, animation_loops, animation_root_gap_ms) =
                    if let Some(gif) = &decoded_gif {
                        let frames: BTreeMap<u32, GraphicsAnimationFrame> = gif
                            .extra_frames
                            .iter()
                            .enumerate()
                            .map(|(index, frame)| {
                                (
                                    index as u32 + 2,
                                    GraphicsAnimationFrame {
                                        payload: frame.rgba.clone(),
                                        gap_ms: Some(frame.gap_ms as i32),
                                        width: gif.width,
                                        height: gif.height,
                                        x: 0,
                                        y: 0,
                                        base_frame: 0,
                                        compose_mode: 0,
                                        bgcolor: None,
                                        transient,
                                    },
                                )
                            })
                            .collect();
                        (
                            frames,
                            GraphicsAnimationState::Running,
                            gif.animation_loops,
                            Some(gif.root_gap_ms as i32),
                        )
                    } else {
                        (BTreeMap::new(), GraphicsAnimationState::Stopped, 0, None)
                    };
                let mut previous_bytes =
                    self.resources.get(&image).map_or(0, resource_storage_bytes);
                let mut projected_bytes = self
                    .decoded_bytes
                    .saturating_sub(previous_bytes)
                    .saturating_add(total_storage_bytes);
                if projected_bytes > self.limits.max_decoded_bytes {
                    // Under storage pressure, evict transient images before
                    // retained ones (and unreferenced images first) so a
                    // transient-heavy session degrades gracefully instead of
                    // rejecting the upload outright.
                    let excess = projected_bytes - self.limits.max_decoded_bytes;
                    let freed = self.evict_to_make_room(excess, image);
                    if freed > 0 {
                        self.diagnose(
                            Some(image),
                            format!("evicted {freed} bytes of image data to make room for a Kitty upload"),
                        );
                    }
                    previous_bytes = self.resources.get(&image).map_or(0, resource_storage_bytes);
                    projected_bytes = self
                        .decoded_bytes
                        .saturating_sub(previous_bytes)
                        .saturating_add(total_storage_bytes);
                    if projected_bytes > self.limits.max_decoded_bytes {
                        self.diagnose(
                            Some(image),
                            format!(
                                "session graphics store exceeds {} byte limit after eviction",
                                self.limits.max_decoded_bytes
                            ),
                        );
                        return Ok(None);
                    }
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
                // Re-transmitting an existing `i` id preserves any image number
                // it was first created with (Kitty keeps `client_number` on an
                // existing image); a fresh `I=N` transmit sets it to `N`.
                let resource_image_number = if image_number != 0 {
                    image_number
                } else {
                    self.resources
                        .get(&image)
                        .map_or(0, |resource| resource.image_number)
                };
                let generation = self.next_resource_generation;
                self.next_resource_generation =
                    self.next_resource_generation.wrapping_add(1).max(1);
                self.decoded_bytes = projected_bytes;
                let (resolved_format, resolved_root, resolved_encoded) = match decoded_gif {
                    Some(gif) => {
                        let encoded = encode_base64_payload(&gif.root_rgba);
                        (32u8, gif.root_rgba, encoded)
                    }
                    None => {
                        let encoded = encode_base64_payload(&decoded_payload);
                        (format, decoded_payload, encoded)
                    }
                };
                self.resources.insert(
                    image,
                    GraphicsResource {
                        format: resolved_format,
                        generation,
                        pixel_width,
                        pixel_height,
                        image_number: resource_image_number,
                        transient,
                        encoded_payload: resolved_encoded,
                        decoded_payload: resolved_root,
                        animation_frames,
                        animation_state,
                        animation_current_frame: 1,
                        animation_loops,
                        animation_root_gap_ms,
                        animation_frame_started_at: None,
                        animation_current_loop: 0,
                        animation_revision: 0,
                    },
                );
                self.last_image_id = Some(image);
                // Re-transmission replaces the image and all of its old
                // placements, as required by the Kitty protocol.
                self.remove_image_placements(image);
                // `a=t` only transmits the image data; a real terminal does
                // not display it until a separate `a=p` or `a=T` command
                // creates a placement, so only uppercase `T` places here.
                if action == Some(b'T') {
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
                }
                if should_emit_response(quiet, true) {
                    response = if image_number != 0 {
                        Some(kitty_response_with_number(
                            image,
                            image_number,
                            placement_id(&values),
                            "OK",
                        ))
                    } else if requested_image != 0 {
                        Some(kitty_response(image, placement_id(&values), "OK"))
                    } else {
                        None
                    };
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
                if should_emit_response(quiet, true) {
                    response = if image_number != 0 {
                        Some(kitty_response_with_number(
                            image,
                            image_number,
                            placement_id(&values),
                            "OK",
                        ))
                    } else {
                        Some(kitty_response(image, placement_id(&values), "OK"))
                    };
                }
            }
            Some(b'f') => {
                let decoded = resolve_transfer_payload(
                    &values,
                    encoded_payload,
                    compression,
                    self.limits.max_decoded_bytes,
                )?;
                let requested_frame = parameter_u32(&values, "r", 0)?;
                let (pixel_width, pixel_height, format) = {
                    let resource = self
                        .resources
                        .get(&image)
                        .ok_or(GraphicsError::ImageNotFound(image))?;
                    (resource.pixel_width, resource.pixel_height, resource.format)
                };
                // `r=0` appends the next frame number (Kitty allocates the
                // next slot after the newest existing frame).
                let frame = if requested_frame == 0 {
                    self.resources
                        .get(&image)
                        .map_or(2, |resource| {
                            resource
                                .animation_frames
                                .keys()
                                .next_back()
                                .copied()
                                .unwrap_or(1)
                                .saturating_add(1)
                        })
                } else {
                    requested_frame
                };
                if frame == 0 || frame as usize > self.limits.max_placements {
                    return Err(GraphicsError::InvalidParameter(
                        "animation frame".to_owned(),
                    ));
                }
                // Frame 1 is the root frame; anything else is only an edit if
                // it already exists in the animation frame map.
                let edits_existing = frame == 1
                    || self
                        .resources
                        .get(&image)
                        .is_some_and(|resource| resource.animation_frames.contains_key(&frame));

                // Kitty's `a=f` frame-composition keys: `s`/`v` are the pixel
                // dimensions of the transmitted rectangle, `x`/`y` its offset,
                // `c` the base frame a new delta composes onto, `X` the
                // composition mode (0 alpha-blend, 1 replace), and `Y` the
                // background canvas color for standalone partial frames.
                let rect_width = parameter_u32(&values, "s", pixel_width)?;
                let rect_height = parameter_u32(&values, "v", pixel_height)?;
                let offset_x = parameter_u32(&values, "x", 0)?;
                let offset_y = parameter_u32(&values, "y", 0)?;
                let base_frame = parameter_u32(&values, "c", 0)?;
                let compose_mode = parameter_u32(&values, "X", 0)?;
                let bgcolor = values
                    .get("Y")
                    .map(|raw| {
                        raw.parse::<u32>()
                            .map_err(|_| GraphicsError::InvalidParameter(raw.clone()))
                    })
                    .transpose()?;
                // The transmitted frame's own `N=1` transient hint. It is OR'd
                // with any base frame's chain below so a delta inherits its
                // ancestor's transient status.
                let transmitted_transient = parameter_u32(&values, "N", 0)? & 1 != 0;

                let bytes_per_pixel = match format {
                    24 => 3usize,
                    32 => 4usize,
                    _ => 0usize,
                };
                let composes = base_frame != 0
                    || bgcolor.is_some()
                    || compose_mode != 0
                    || offset_x != 0
                    || offset_y != 0
                    || rect_width != pixel_width
                    || rect_height != pixel_height;
                if composes {
                    if bytes_per_pixel == 0 {
                        self.diagnose(
                            Some(image),
                            "cannot compose animation frames for a non-raw (PNG/GIF) image"
                                .to_owned(),
                        );
                        return Err(GraphicsError::InvalidParameter(
                            "animation frame composition".to_owned(),
                        ));
                    }
                    if pixel_width == 0 || pixel_height == 0 {
                        self.diagnose(
                            Some(image),
                            "cannot compose an animation frame for an image with unknown dimensions"
                                .to_owned(),
                        );
                        return Err(GraphicsError::InvalidParameter(
                            "animation frame composition".to_owned(),
                        ));
                    }
                    if rect_width == 0 || rect_height == 0 {
                        return Err(GraphicsError::InvalidParameter(
                            "animation frame composition".to_owned(),
                        ));
                    }
                    if offset_x.saturating_add(rect_width) > pixel_width
                        || offset_y.saturating_add(rect_height) > pixel_height
                    {
                        return Err(GraphicsError::InvalidParameter(
                            "animation frame composition".to_owned(),
                        ));
                    }
                }
                if base_frame != 0
                    && base_frame != 1
                    && !self
                        .resources
                        .get(&image)
                        .is_some_and(|resource| resource.animation_frames.contains_key(&base_frame))
                {
                    return Err(GraphicsError::InvalidParameter(
                        "animation frame".to_owned(),
                    ));
                }

                let gap_ms = values
                    .get("z")
                    .map(|raw| {
                        raw.parse::<i32>()
                            .map_err(|_| GraphicsError::InvalidParameter(raw.clone()))
                    })
                    .transpose()?;

                // Editing an existing frame coalesces its current pixels,
                // composes the new rectangle on top, and stores the result as a
                // full keyframe (Kitty's `handle_animation_frame_load_command`
                // edit path). A new frame is stored as a delta, deferring
                // coalescing until it is rendered or itself edited. Non-raw
                // (PNG/GIF) payloads cannot be composed byte-for-byte, so an
                // edit of one replaces the stored frame verbatim instead.
                let (new_payload, new_width, new_height, new_x, new_y, new_base, new_mode, new_bgcolor, new_transient) =
                    if edits_existing && bytes_per_pixel != 0 {
                        let mut under = self.coalesce_frame_metadata(image, frame)?;
                        // Editing an existing frame keeps its coalesced
                        // transient status and ORs in the transmitted hint
                        // (Kitty's `frame->transient = cfd.transient ||
                        // transmitted_frame.transient`).
                        let transient = under.transient || transmitted_transient;
                        compose_rect_onto(
                            &mut under.payload,
                            &decoded,
                            RectCompose {
                                under_width: pixel_width,
                                bytes_per_pixel,
                                over_x: offset_x,
                                over_y: offset_y,
                                over_width: rect_width,
                                over_height: rect_height,
                                compose_mode,
                            },
                        );
                        (
                            under.payload,
                            pixel_width,
                            pixel_height,
                            0u32,
                            0u32,
                            0u32,
                            0u32,
                            None,
                            transient,
                        )
                    } else if edits_existing {
                        // A non-raw frame cannot be coalesced byte-for-byte, so
                        // the replacement keeps the existing chain's transient
                        // status OR'd with the transmitted hint.
                        (
                            decoded,
                            rect_width,
                            rect_height,
                            offset_x,
                            offset_y,
                            0u32,
                            0u32,
                            None,
                            self.frame_chain_transient(image, frame) || transmitted_transient,
                        )
                    } else {
                        // A new delta inherits its base frame's chain transient
                        // (Kitty's `transmitted_frame.transient ||=
                        // frame_chain_is_transient(img, other_frame)`).
                        (
                            decoded,
                            rect_width,
                            rect_height,
                            offset_x,
                            offset_y,
                            base_frame,
                            compose_mode,
                            bgcolor,
                            if base_frame != 0 {
                                transmitted_transient
                                    || self.frame_chain_transient(image, base_frame)
                            } else {
                                transmitted_transient
                            },
                        )
                    };

                let previous_frame_bytes = if frame == 1 {
                    self.resources
                        .get(&image)
                        .map_or(0, |resource| resource.decoded_payload.len())
                } else {
                    self.resources
                        .get(&image)
                        .and_then(|resource| resource.animation_frames.get(&frame))
                        .map_or(0, |existing| existing.payload.len())
                };
                let projected_bytes = self
                    .decoded_bytes
                    .saturating_sub(previous_frame_bytes)
                    .saturating_add(new_payload.len());
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

                if frame == 1 {
                    let resource = self
                        .resources
                        .get_mut(&image)
                        .expect("resource validated above");
                    resource.encoded_payload = encode_base64_payload(&new_payload);
                    resource.decoded_payload = new_payload;
                    resource.transient = new_transient;
                } else {
                    self.resources
                        .get_mut(&image)
                        .expect("resource validated above")
                        .animation_frames
                        .insert(
                            frame,
                            GraphicsAnimationFrame {
                                payload: new_payload,
                                gap_ms,
                                width: new_width,
                                height: new_height,
                                x: new_x,
                                y: new_y,
                                base_frame: new_base,
                                compose_mode: new_mode,
                                bgcolor: new_bgcolor,
                                transient: new_transient,
                            },
                        );
                }
                self.decoded_bytes = projected_bytes;
                if should_emit_response(quiet, true) {
                    response = Some(if image_number != 0 {
                        kitty_response_with_number(image, image_number, None, "OK")
                    } else {
                        kitty_response(image, None, "OK")
                    });
                }
            }
            Some(b'a') => {
                let resource = self
                    .resources
                    .get_mut(&image)
                    .ok_or(GraphicsError::ImageNotFound(image))?;
                if let Some(state) = values.get("s") {
                    let previous = resource.animation_state;
                    resource.animation_state = match state.as_str() {
                        "1" => GraphicsAnimationState::Stopped,
                        "2" => GraphicsAnimationState::Loading,
                        "3" => GraphicsAnimationState::Running,
                        _ => return Err(GraphicsError::InvalidParameter(state.clone())),
                    };
                    // Kitty resets the loop counter on any state command and
                    // re-anchors the frame clock when playback (re)starts from
                    // a stopped state.
                    resource.animation_current_loop = 0;
                    if previous == GraphicsAnimationState::Stopped {
                        resource.animation_frame_started_at = None;
                    }
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
                    if resource.animation_current_frame != frame {
                        resource.animation_current_frame = frame;
                        resource.animation_revision =
                            resource.animation_revision.wrapping_add(1).max(1);
                    }
                }
                if let Some(loops) = values.get("v") {
                    resource.animation_loops = loops
                        .parse::<u32>()
                        .map_err(|_| GraphicsError::InvalidParameter(loops.clone()))?;
                }
                if let (Some(frame), Some(gap)) = (values.get("r"), values.get("z")) {
                    let frame = frame
                        .parse::<u32>()
                        .map_err(|_| GraphicsError::InvalidParameter(frame.clone()))?;
                    let gap_ms = gap
                        .parse::<i32>()
                        .map_err(|_| GraphicsError::InvalidParameter(gap.clone()))?;
                    if frame == 1 {
                        resource.animation_root_gap_ms = Some(gap_ms);
                    } else {
                        let target = resource.animation_frames.get_mut(&frame).ok_or_else(|| {
                            GraphicsError::InvalidParameter("animation frame".to_owned())
                        })?;
                        target.gap_ms = Some(gap_ms);
                    }
                }
                if should_emit_response(quiet, true) {
                    response = Some(if image_number != 0 {
                        kitty_response_with_number(image, image_number, None, "OK")
                    } else {
                        kitty_response(image, None, "OK")
                    });
                }
            }
            Some(b'c') => {
                // Kitty's `a=c` composes a pixel rectangle from one animation
                // frame (the `r` key) onto another (the `c` key). `X`/`Y` are
                // the source rectangle origin, `x`/`y` the destination origin,
                // `w`/`h` the shared rectangle size (defaulting to the full
                // image), and `C` the composition mode (0 alpha-blend, 1
                // overwrite).
                self.compose_animation_frame(
                    image,
                    AnimationCompose {
                        source_frame: parameter_u32(&values, "r", 1)?,
                        destination_frame: parameter_u32(&values, "c", 1)?,
                        source_offset: (
                            parameter_u32(&values, "X", 0)?,
                            parameter_u32(&values, "Y", 0)?,
                        ),
                        destination_offset: (
                            parameter_u32(&values, "x", 0)?,
                            parameter_u32(&values, "y", 0)?,
                        ),
                        compose_mode: parameter_u32(&values, "C", 0)?,
                        width: parameter_u32(&values, "w", 0)?,
                        height: parameter_u32(&values, "h", 0)?,
                    },
                )?;
                if should_emit_response(quiet, true) {
                    response = Some(if image_number != 0 {
                        kitty_response_with_number(image, image_number, None, "OK")
                    } else {
                        kitty_response(image, None, "OK")
                    });
                }
            }
            Some(b'd') | Some(b'D') => {
                // Any delete command aborts an in-progress chunked upload.
                self.pending_upload = None;
                match values.get("d").map(String::as_str) {
                    Some("a") | Some("A") => {
                        // Delete visible placements. Lowercase retains pixel
                        // data so a scrolled-away image can still be
                        // re-displayed; uppercase also frees unreferenced data.
                        self.erase_visible(screen, scrollback, scroll_region, region_scroll);
                        if values.get("d").is_some_and(|value| value == "A") {
                            self.free_unreferenced_resources();
                        }
                    }
                    Some("c") | Some("C") => {
                        // Delete placements intersecting the current cursor
                        // cell (the cursor is in zero-based grid coordinates).
                        let column = i32::from(cursor.0);
                        let row = i32::from(cursor.1);
                        self.remove_placements_where(
                            scrollback,
                            scroll_region,
                            region_scroll,
                            |placement, x, y| {
                                !placement.is_virtual()
                                    && placement.anchor.screen() == screen
                                    && rect_contains_cell(
                                        (
                                            x,
                                            y,
                                            i32::from(placement.width()),
                                            i32::from(placement.height()),
                                        ),
                                        column,
                                        row,
                                    )
                            },
                        );
                        if values.get("d").is_some_and(|value| value == "C") {
                            self.free_unreferenced_resources();
                        }
                    }
                    Some("p") | Some("P") => {
                        // Delete placements intersecting the cell at (x, y);
                        // x/y are 1-based (x=1,y=1 is the top-left cell).
                        let column = parameter_u32(&values, "x", 1)?.saturating_sub(1) as i32;
                        let row = parameter_u32(&values, "y", 1)?.saturating_sub(1) as i32;
                        self.remove_placements_where(
                            scrollback,
                            scroll_region,
                            region_scroll,
                            |placement, x, y| {
                                !placement.is_virtual()
                                    && placement.anchor.screen() == screen
                                    && rect_contains_cell(
                                        (
                                            x,
                                            y,
                                            i32::from(placement.width()),
                                            i32::from(placement.height()),
                                        ),
                                        column,
                                        row,
                                    )
                            },
                        );
                        if values.get("d").is_some_and(|value| value == "P") {
                            self.free_unreferenced_resources();
                        }
                    }
                    Some("q") | Some("Q") => {
                        // Delete placements intersecting the cell at (x, y)
                        // that also carry the given z-index.
                        let column = parameter_u32(&values, "x", 1)?.saturating_sub(1) as i32;
                        let row = parameter_u32(&values, "y", 1)?.saturating_sub(1) as i32;
                        let z = parameter_i16(&values, "z", 0)?;
                        self.remove_placements_where(
                            scrollback,
                            scroll_region,
                            region_scroll,
                            |placement, x, y| {
                                !placement.is_virtual()
                                    && placement.anchor.screen() == screen
                                    && placement.z_index() == z
                                    && rect_contains_cell(
                                        (
                                            x,
                                            y,
                                            i32::from(placement.width()),
                                            i32::from(placement.height()),
                                        ),
                                        column,
                                        row,
                                    )
                            },
                        );
                        if values.get("d").is_some_and(|value| value == "Q") {
                            self.free_unreferenced_resources();
                        }
                    }
                    Some("x") | Some("X") => {
                        // Delete placements intersecting the given column
                        // (1-based `x` key).
                        let column = parameter_u32(&values, "x", 1)?.saturating_sub(1) as i32;
                        self.remove_placements_where(
                            scrollback,
                            scroll_region,
                            region_scroll,
                            |placement, x, y| {
                                !placement.is_virtual()
                                    && placement.anchor.screen() == screen
                                    && rect_intersects_column(
                                        (
                                            x,
                                            y,
                                            i32::from(placement.width()),
                                            i32::from(placement.height()),
                                        ),
                                        column,
                                    )
                            },
                        );
                        if values.get("d").is_some_and(|value| value == "X") {
                            self.free_unreferenced_resources();
                        }
                    }
                    Some("y") | Some("Y") => {
                        // Delete placements intersecting the given row
                        // (1-based `y` key).
                        let row = parameter_u32(&values, "y", 1)?.saturating_sub(1) as i32;
                        self.remove_placements_where(
                            scrollback,
                            scroll_region,
                            region_scroll,
                            |placement, x, y| {
                                !placement.is_virtual()
                                    && placement.anchor.screen() == screen
                                    && rect_intersects_row(
                                        (
                                            x,
                                            y,
                                            i32::from(placement.width()),
                                            i32::from(placement.height()),
                                        ),
                                        row,
                                    )
                            },
                        );
                        if values.get("d").is_some_and(|value| value == "Y") {
                            self.free_unreferenced_resources();
                        }
                    }
                    Some("z") | Some("Z") => {
                        // Delete placements with the given z-index on the
                        // active screen.
                        let z = parameter_i16(&values, "z", 0)?;
                        self.remove_placements_where(
                            scrollback,
                            scroll_region,
                            region_scroll,
                            |placement, _, _| {
                                !placement.is_virtual()
                                    && placement.anchor.screen() == screen
                                    && placement.z_index() == z
                            },
                        );
                        if values.get("d").is_some_and(|value| value == "Z") {
                            self.free_unreferenced_resources();
                        }
                    }
                    Some("f") | Some("F") => {
                        // `d=f`/`d=F` delete one frame (`r=<frame>`, defaulting
                        // to the root) with renumbering and gap rebalance,
                        // matching Kitty's `handle_delete_frame_command`.
                        // `d=F` additionally frees the entire image when no
                        // extra frames remain.
                        let frame = parameter_u32(&values, "r", 0)?;
                        self.delete_animation_frame(
                            image,
                            frame,
                            values.get("d").is_some_and(|value| value == "F"),
                        );
                    }
                    Some("i") | Some("I") | Some("n") | Some("N") if image != 0 => {
                        // `d=i` targets an image id; `d=n` targets the newest
                        // image with the `I` number (already resolved into
                        // `image`). A `p` key narrows either to one placement.
                        if let Some(placement) = placement_id(&values) {
                            let key = (u64::from(image) << 32) | u64::from(placement);
                            self.placements.remove(&key);
                            self.prune_orphaned_relatives();
                        } else {
                            self.remove_image_placements(image);
                        }
                        if values
                            .get("d")
                            .is_some_and(|value| matches!(value.as_str(), "I" | "N"))
                        {
                            self.free_resource_if_unreferenced(image);
                        }
                    }
                    Some("r") | Some("R") => {
                        // Delete every image whose id is in the inclusive range
                        // [x, y]. Lowercase retains pixel data; uppercase also
                        // frees it once unreferenced.
                        let lower = parameter_u32(&values, "x", 0)?;
                        let upper = parameter_u32(&values, "y", 0)?;
                        let uppercase = values.get("d").is_some_and(|value| value == "R");
                        let range = self
                            .resources
                            .keys()
                            .copied()
                            .filter(|image| *image >= lower && *image <= upper)
                            .collect::<Vec<_>>();
                        for image in range {
                            self.remove_image_placements(image);
                            if uppercase {
                                self.free_resource_if_unreferenced(image);
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
                }
            }
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
        self.prune_orphaned_relatives();
    }

    /// Resolves a placement's current cell rectangle `(column, row, width,
    /// height)` in live grid coordinates, or `None` if its relative parent
    /// chain is broken.
    fn resolved_cell_rect(
        &self,
        placement: &GraphicsPlacement,
        current_scrollback: usize,
        current_region: GraphicsScrollRegion,
        current_region_scroll: i64,
    ) -> Option<(i32, i32, i32, i32)> {
        let (column, row) = self.resolve_origin(
            placement,
            current_scrollback,
            current_region,
            current_region_scroll,
            0,
            0,
        )?;
        Some((
            column,
            row,
            i32::from(placement.width()),
            i32::from(placement.height()),
        ))
    }

    /// Removes every placement for which `matches` returns true, given the
    /// placement and its resolved cell origin. Returns whether anything was
    /// removed (including cascaded relative children).
    fn remove_placements_where(
        &mut self,
        current_scrollback: usize,
        current_region: GraphicsScrollRegion,
        current_region_scroll: i64,
        mut matches: impl FnMut(&GraphicsPlacement, i32, i32) -> bool,
    ) -> bool {
        let mut to_remove = Vec::new();
        for (key, placement) in &self.placements {
            if let Some((column, row, _, _)) = self.resolved_cell_rect(
                placement,
                current_scrollback,
                current_region,
                current_region_scroll,
            ) && matches(placement, column, row)
            {
                to_remove.push(*key);
            }
        }
        let mut removed = false;
        for key in to_remove {
            if self.placements.remove(&key).is_some() {
                removed = true;
            }
        }
        removed | self.prune_orphaned_relatives()
    }

    /// Removes relative placements whose parent chain is broken, mirroring
    /// Kitty's rule that a relative placement is deleted along with its parent.
    /// Returns whether any placement was removed. Pixel data is deliberately
    /// left untouched: the triggering operation (lowercase vs. uppercase
    /// delete, erase, or eviction) decides whether resources are reclaimed.
    fn prune_orphaned_relatives(&mut self) -> bool {
        if !self.placements.values().any(|placement| placement.parent().is_some()) {
            return false;
        }
        // Compute, to a fixpoint, which placements still have a resolvable
        // (rooted) parent chain. A non-relative placement is always rooted; a
        // relative placement is rooted iff its parent is rooted.
        let mut rooted: BTreeSet<u64> = BTreeSet::new();
        loop {
            let mut progress = false;
            for (key, placement) in &self.placements {
                if rooted.contains(key) {
                    continue;
                }
                let is_rooted = match placement.parent() {
                    None => true,
                    Some(parent) => rooted.contains(&relative_parent_key(parent)),
                };
                if is_rooted {
                    rooted.insert(*key);
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }
        let before = self.placements.len();
        self.placements.retain(|key, _| rooted.contains(key));
        self.placements.len() != before
    }

    /// Resolves a placement's absolute cell origin in current grid
    /// coordinates, walking relative-parent chains. Returns `None` if the
    /// parent chain is broken or deeper than [`MAX_RELATIVE_DEPTH`].
    ///
    /// `view_offset` shifts full-screen history positions (history navigation);
    /// pass `0` for operations that reason about the live screen (erases).
    fn resolve_origin(
        &self,
        placement: &GraphicsPlacement,
        current_scrollback: usize,
        current_region: GraphicsScrollRegion,
        current_region_scroll: i64,
        view_offset: i32,
        depth: usize,
    ) -> Option<(i32, i32)> {
        if depth > MAX_RELATIVE_DEPTH {
            return None;
        }
        match placement.parent() {
            None => {
                // A virtual placement has no physical cell of its own; its
                // origin is the min x / min y of the Unicode placeholder cells
                // the client wrote for it (Kitty's `resolve_cell_ref`).
                if placement.is_virtual() {
                    return self.virtual_placement_origin(
                        placement,
                        current_scrollback,
                        current_region,
                        current_region_scroll,
                        view_offset,
                    );
                }
                let mut row = placement.anchor().resolve_row_with_state(
                    current_scrollback,
                    current_region,
                    current_region_scroll,
                );
                if placement.anchor().scroll_region().is_full_screen()
                    && current_region.is_full_screen()
                {
                    row = row.saturating_add(view_offset);
                }
                Some((i32::from(placement.anchor().column()), row))
            }
            Some(parent) => {
                let parent_placement = self.placements.get(&relative_parent_key(parent))?;
                let (column, row) = self.resolve_origin(
                    parent_placement,
                    current_scrollback,
                    current_region,
                    current_region_scroll,
                    view_offset,
                    depth + 1,
                )?;
                Some((
                    column.saturating_add(parent.cell_offset_x()),
                    row.saturating_add(parent.cell_offset_y()),
                ))
            }
        }
    }

    /// Resolves a virtual placement's origin from the min x / min y of its
    /// Unicode placeholder cells (Kitty's `resolve_cell_ref`). Each cell is
    /// stored in the same screen-relative + scrollback frame as a normal
    /// placement anchor, so it resolves to the same absolute history row and
    /// scrolls with the text grid. Returns `None` when no placeholder cells
    /// have been observed for the image yet (the relative child is then
    /// invisible, exactly like Kitty skipping an unresolvable virtual parent).
    fn virtual_placement_origin(
        &self,
        placement: &GraphicsPlacement,
        current_scrollback: usize,
        current_region: GraphicsScrollRegion,
        current_region_scroll: i64,
        view_offset: i32,
    ) -> Option<(i32, i32)> {
        let cells = self
            .placeholder_cells
            .get(&placement.resource().image())?;
        let mut min_column: Option<i32> = None;
        let mut min_row: Option<i32> = None;
        for cell in cells {
            let anchor =
                GraphicsGridAnchor::new(cell.column, cell.row, cell.scrollback);
            let mut row = anchor.resolve_row_with_state(
                current_scrollback,
                current_region,
                current_region_scroll,
            );
            // Placeholder glyphs are text, so history navigation shifts them
            // exactly like a full-screen placement.
            if current_region.is_full_screen() {
                row = row.saturating_add(view_offset);
            }
            let column = i32::from(cell.column);
            min_column = Some(min_column.map_or(column, |current| current.min(column)));
            min_row = Some(min_row.map_or(row, |current| current.min(row)));
        }
        match (min_column, min_row) {
            (Some(column), Some(row)) => Some((column, row)),
            _ => None,
        }
    }

    /// Releases `image`'s pixel data when no placement still references it.
    ///
    /// This is the uppercase-delete behavior: data survives a lowercase delete
    /// (so an image can be re-displayed without retransmission) but is freed
    /// once the last placement — including any in scrollback — is gone.
    fn free_resource_if_unreferenced(&mut self, image: u32) {
        if self
            .placements
            .values()
            .all(|placement| placement.resource().image() != image)
            && let Some(resource) = self.resources.remove(&image)
        {
            self.decoded_bytes = self
                .decoded_bytes
                .saturating_sub(resource_storage_bytes(&resource));
            if self.last_image_id == Some(image) {
                self.last_image_id = None;
            }
        }
    }

    /// Releases pixel data for every image with no remaining placements,
    /// preserving any image still referenced by a scrollback placement.
    fn free_unreferenced_resources(&mut self) {
        let referenced: BTreeSet<u32> = self
            .placements
            .values()
            .map(|placement| placement.resource().image())
            .collect();
        let unreferenced: Vec<u32> = self
            .resources
            .keys()
            .copied()
            .filter(|image| !referenced.contains(image))
            .collect();
        for image in unreferenced {
            self.free_resource_if_unreferenced(image);
        }
    }

    /// Removes an image's decoded data and every placement that displays it,
    /// returning the number of storage bytes reclaimed.
    fn evict_image(&mut self, image: u32) -> usize {
        self.remove_image_placements(image);
        let Some(resource) = self.resources.remove(&image) else {
            return 0;
        };
        let bytes = resource_storage_bytes(&resource);
        self.decoded_bytes = self.decoded_bytes.saturating_sub(bytes);
        if self.last_image_id == Some(image) {
            self.last_image_id = None;
        }
        bytes
    }

    /// Evicts images until at least `bytes` of decoded storage is freed,
    /// following Kitty's quota order: unreferenced images first (they can be
    /// re-uploaded), then transient images before retained ones, then oldest
    /// before newest. `keep` (the image being uploaded) is never evicted.
    /// Returns the number of bytes actually freed.
    fn evict_to_make_room(&mut self, bytes: usize, keep: u32) -> usize {
        let mut freed = 0usize;
        let referenced: BTreeSet<u32> = self
            .placements
            .values()
            .map(|placement| placement.resource().image())
            .collect();

        // Pass 1: unreferenced images, regardless of their transient hint.
        let unreferenced = self
            .resources
            .keys()
            .copied()
            .filter(|image| *image != keep && !referenced.contains(image))
            .collect::<Vec<_>>();
        for image in unreferenced {
            freed = freed.saturating_add(self.evict_image(image));
            if freed >= bytes {
                return freed;
            }
        }

        // Pass 2: referenced images, transient first then oldest first.
        let mut victims = self
            .resources
            .keys()
            .copied()
            .filter(|image| *image != keep)
            .collect::<Vec<_>>();
        victims.sort_by_key(|image| {
            let resource = &self.resources[image];
            (!resource.transient, resource.generation)
        });
        for image in victims {
            freed = freed.saturating_add(self.evict_image(image));
            if freed >= bytes {
                return freed;
            }
        }
        freed
    }

    /// Coalesces an animation frame into a full-image pixel buffer, applying
    /// any `a=f` composition metadata. Frame 1 is the root frame. A delta
    /// frame composes its rectangle onto its `c` base frame (or onto a `Y`
    /// background canvas when standalone); a keyframe is returned directly.
    /// This mirrors Kitty's `get_coalesced_frame_data` chain resolution.
    fn coalesce_frame(&self, image: u32, frame: u32) -> Result<Vec<u8>, GraphicsError> {
        Ok(self.coalesce_frame_metadata(image, frame)?.payload)
    }

    /// Coalesces `frame` into its full pixel buffer and per-frame transient
    /// flag, mirroring Kitty's `get_coalesced_frame_data` (whose
    /// `CoalescedFrameData` carries `transient` OR'd along the reference
    /// chain).
    fn coalesce_frame_metadata(
        &self,
        image: u32,
        frame: u32,
    ) -> Result<CoalescedFrame, GraphicsError> {
        self.coalesce_frame_depth(image, frame, 0)
    }

    fn coalesce_frame_depth(
        &self,
        image: u32,
        frame: u32,
        depth: u32,
    ) -> Result<CoalescedFrame, GraphicsError> {
        if depth > 32 {
            return Err(GraphicsError::InvalidParameter(
                "animation frame reference chain".to_owned(),
            ));
        }
        let resource = self
            .resources
            .get(&image)
            .ok_or(GraphicsError::ImageNotFound(image))?;
        let bytes_per_pixel = match resource.format {
            24 => 3usize,
            // A PNG/GIF (`f=100`) frame is decoded to RGBA8 so it can be
            // composed on pixels like a raw frame.
            32 | 100 => 4usize,
            _ => {
                return Err(GraphicsError::InvalidParameter(
                    "animation frame composition".to_owned(),
                ))
            }
        };
        let (image_width, image_height) = (resource.pixel_width, resource.pixel_height);
        if frame == 1 {
            let transient = resource.transient;
            if resource.format == 100 {
                let (pixels, width, height) =
                    decode_raster_image(&resource.decoded_payload, self.limits.max_decoded_bytes)
                        .ok_or_else(|| {
                            GraphicsError::InvalidParameter(
                                "animation frame composition".to_owned(),
                            )
                        })?;
                if width != resource.pixel_width || height != resource.pixel_height {
                    return Err(GraphicsError::InvalidParameter(
                        "animation frame composition".to_owned(),
                    ));
                }
                return Ok(CoalescedFrame {
                    payload: pixels,
                    transient,
                });
            }
            return Ok(CoalescedFrame {
                payload: resource.decoded_payload.clone(),
                transient,
            });
        }
        let animation_frame = resource
            .animation_frames
            .get(&frame)
            .ok_or_else(|| {
                GraphicsError::InvalidParameter("animation frame".to_owned())
            })?;
        if animation_frame.is_full_keyframe(image_width, image_height) {
            return Ok(CoalescedFrame {
                payload: animation_frame.payload.clone(),
                transient: animation_frame.transient,
            });
        }
        let (base_frame, bgcolor, x, y, width, height, compose_mode) = (
            animation_frame.base_frame,
            animation_frame.bgcolor,
            animation_frame.x,
            animation_frame.y,
            animation_frame.width,
            animation_frame.height,
            animation_frame.compose_mode,
        );
        let (mut under, mut transient) = if base_frame != 0 {
            let base = self.coalesce_frame_depth(image, base_frame, depth + 1)?;
            (base.payload, base.transient)
        } else {
            let total_bytes = (image_width as usize)
                .saturating_mul(image_height as usize)
                .saturating_mul(bytes_per_pixel);
            let buffer = match bgcolor {
                Some(color) => fill_bgcolor_buffer(total_bytes, bytes_per_pixel, color),
                None => vec![0u8; total_bytes],
            };
            // A background canvas is not a frame, so it contributes no
            // transient hint of its own.
            (buffer, false)
        };
        compose_rect_onto(
            &mut under,
            &animation_frame.payload,
            RectCompose {
                under_width: image_width,
                bytes_per_pixel,
                over_x: x,
                over_y: y,
                over_width: width,
                over_height: height,
                compose_mode,
            },
        );
        transient = transient || animation_frame.transient;
        Ok(CoalescedFrame {
            payload: under,
            transient,
        })
    }

    /// Whether any frame in `frame`'s base-frame reference chain carries the
    /// per-frame transient hint, mirroring Kitty's `frame_chain_is_transient`.
    fn frame_chain_transient(&self, image: u32, frame: u32) -> bool {
        let mut current = frame;
        let mut depth = 0u32;
        while depth <= 32 {
            if current == 1 {
                return self
                    .resources
                    .get(&image)
                    .is_some_and(|resource| resource.transient);
            }
            let Some(resource) = self.resources.get(&image) else {
                return false;
            };
            let Some(animation_frame) = resource.animation_frames.get(&current) else {
                return false;
            };
            if animation_frame.transient {
                return true;
            }
            if animation_frame.base_frame == 0 {
                return false;
            }
            current = animation_frame.base_frame;
            depth += 1;
        }
        false
    }

    /// Advances every playing animation to `now`, mirroring Kitty's
    /// `scan_active_animations`, and returns the duration until the next frame
    /// deadline (`None` when nothing is animating).
    pub fn advance_animations(&mut self, now: Instant) -> Option<Duration> {
        let animatable: Vec<u32> = self
            .resources
            .iter()
            .filter(|(_, resource)| animation_is_animatable(resource))
            .map(|(image, _)| *image)
            .collect();
        let mut next_deadline: Option<Instant> = None;
        for image in animatable {
            if let Some(deadline) = self.advance_image_animation(image, now)
                && deadline > now
            {
                next_deadline = Some(match next_deadline {
                    Some(earliest) => earliest.min(deadline),
                    None => deadline,
                });
            }
        }
        next_deadline.map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Advances a single image's animation to `now`, returning the next frame
    /// deadline (`None` when the image is missing).
    fn advance_image_animation(&mut self, image: u32, now: Instant) -> Option<Instant> {
        let (state, max_loops, frame_count, mut frame, mut loop_count, mut anchor) = {
            let resource = self.resources.get(&image)?;
            (
                resource.animation_state,
                resource.animation_loops.saturating_sub(1),
                resource.animation_frames.len() as u32 + 1,
                resource.animation_current_frame,
                resource.animation_current_loop,
                resource.animation_frame_started_at.unwrap_or(now),
            )
        };
        let mut changed = false;
        loop {
            let gap = frame_gap_ms(self.resources.get(&image)?, frame);
            if gap == 0 {
                // A gapless frame advances immediately (Kitty's `while (!gap)` skip).
                let (next, wrapped) = next_animation_frame(frame_count, frame);
                if wrapped {
                    loop_count = loop_count.saturating_add(1);
                    // A `Loading` animation plays once and stops when it wraps
                    // back to the root; `Running` honors its loop count.
                    if state == GraphicsAnimationState::Loading
                        || (max_loops != 0 && loop_count >= max_loops)
                    {
                        break;
                    }
                }
                frame = next;
                changed = true;
                continue;
            }
            let deadline = anchor + Duration::from_millis(u64::from(gap));
            if now < deadline {
                break;
            }
            let (next, wrapped) = next_animation_frame(frame_count, frame);
            if wrapped {
                loop_count = loop_count.saturating_add(1);
                if state == GraphicsAnimationState::Loading
                    || (max_loops != 0 && loop_count >= max_loops)
                {
                    break;
                }
            }
            frame = next;
            anchor = deadline;
            changed = true;
        }
        {
            let resource = self.resources.get_mut(&image)?;
            resource.animation_current_frame = frame;
            resource.animation_current_loop = loop_count;
            resource.animation_frame_started_at = Some(anchor);
            if changed {
                resource.animation_revision = resource.animation_revision.wrapping_add(1).max(1);
            }
        }
        let gap = frame_gap_ms(self.resources.get(&image)?, frame);
        Some(anchor + Duration::from_millis(u64::from(gap)))
    }

    /// The pixel payload and generation a resource should serve: the coalesced
    /// current animation frame when one is selected, otherwise the root frame.
    fn served_graphics_payload(&self, image: u32, resource: &GraphicsResource) -> (Vec<u8>, u64) {
        if resource.animation_current_frame != 1
            && resource.animation_frames.contains_key(&resource.animation_current_frame)
        {
            match self.coalesce_frame(image, resource.animation_current_frame) {
                Ok(bytes) => (
                    encode_base64_payload(&bytes),
                    resource.generation.wrapping_add(resource.animation_revision),
                ),
                Err(_) => (resource.encoded_payload.clone(), resource.generation),
            }
        } else {
            (resource.encoded_payload.clone(), resource.generation)
        }
    }

    /// Deletes one animation frame (the Kitty `d=f`/`d=F` action), matching
    /// `handle_delete_frame_command`: `r=<frame>` selects the frame (0/absent
    /// means the root frame, and an out-of-range number clamps to the last
    /// extra frame). Deleting the root promotes the first extra frame to the
    /// new root; the remaining extra frames are renumbered down by one so
    /// playback stays contiguous, and the current frame index is adjusted with
    /// Kitty's rules. The animation clock re-anchors and the served-payload
    /// revision bumps so the outer terminal re-uploads. When no extra frames
    /// remain, `d=f` is a no-op while `d=F` frees the entire image.
    fn delete_animation_frame(&mut self, image: u32, frame: u32, free_image: bool) {
        let has_extras = self
            .resources
            .get(&image)
            .is_some_and(|resource| !resource.animation_frames.is_empty());
        if !has_extras {
            if free_image {
                self.evict_image(image);
            }
            return;
        }
        let (max_frame, current_frame) = {
            let resource = self.resources.get(&image).expect("checked above");
            (
                resource.animation_frames.len() as u32 + 1,
                resource.animation_current_frame,
            )
        };
        let removed_frame = if frame == 0 { 1 } else { frame.min(max_frame) };
        // The root frame must be a full keyframe, so a promoted extra frame is
        // coalesced before it becomes the new root.
        let promoted = if removed_frame == 1 {
            self.coalesce_frame(image, 2).ok()
        } else {
            None
        };
        let mut removed_bytes;
        let new_current_frame;
        {
            let resource = self.resources.get_mut(&image).expect("checked above");
            let old_root_bytes = resource.decoded_payload.len();
            if removed_frame == 1 {
                // Promote the first extra frame to the new root and renumber
                // the remaining extra frames down by one.
                let promoted_frame = resource.animation_frames.remove(&2).expect("has extras");
                removed_bytes = old_root_bytes.saturating_add(promoted_frame.payload.len());
                resource.animation_root_gap_ms = promoted_frame.gap_ms;
                let new_root = match promoted {
                    Some(bytes) => bytes,
                    None => promoted_frame.payload,
                };
                removed_bytes = removed_bytes.saturating_sub(new_root.len());
                resource.decoded_payload = new_root;
                resource.encoded_payload = encode_base64_payload(&resource.decoded_payload);
                let mut renumbered = BTreeMap::new();
                for (key, animation_frame) in resource.animation_frames.iter() {
                    renumbered.insert(key - 1, animation_frame.clone());
                }
                resource.animation_frames = renumbered;
            } else {
                let removed =
                    resource.animation_frames.remove(&removed_frame).expect("clamped frame");
                removed_bytes = removed.payload.len();
                let mut renumbered = BTreeMap::new();
                for (key, animation_frame) in resource.animation_frames.iter() {
                    let new_key = if *key > removed_frame { *key - 1 } else { *key };
                    renumbered.insert(new_key, animation_frame.clone());
                }
                resource.animation_frames = renumbered;
            }
            // Adjust the current frame like Kitty's `current_frame_index`
            // rules: clamp past-the-end indexes, keep the index of a removed
            // frame (which now shows whatever shifted into its slot), and
            // decrement indexes after the removed frame.
            let new_max = resource.animation_frames.len() as u32 + 1;
            new_current_frame = if current_frame > new_max {
                new_max
            } else if removed_frame < current_frame {
                current_frame - 1
            } else {
                current_frame
            };
            resource.animation_current_frame = new_current_frame;
            // The served frame and the animation clock both changed.
            resource.animation_frame_started_at = None;
            resource.animation_revision = resource.animation_revision.wrapping_add(1).max(1);
        }
        self.decoded_bytes = self.decoded_bytes.saturating_sub(removed_bytes);
    }

    /// Composes a pixel rectangle from one animation frame onto another, the
    /// Kitty `a=c` action. `C=0` (default) alpha-blends the source rectangle
    /// onto the destination and `C=1` overwrites it. Only raw RGB/RGBA payloads
    /// can be composed; a PNG/GIF resource is rejected with a diagnostic.
    fn compose_animation_frame(
        &mut self,
        image: u32,
        compose: AnimationCompose,
    ) -> Result<(), GraphicsError> {
        let AnimationCompose {
            source_frame,
            destination_frame,
            source_offset,
            destination_offset,
            compose_mode,
            width,
            height,
        } = compose;
        let (format, pixel_width, pixel_height, source_exists, destination_exists) = {
            let resource = self
                .resources
                .get(&image)
                .ok_or(GraphicsError::ImageNotFound(image))?;
            (
                resource.format,
                resource.pixel_width,
                resource.pixel_height,
                source_frame == 1 || resource.animation_frames.contains_key(&source_frame),
                destination_frame == 1
                    || resource.animation_frames.contains_key(&destination_frame),
            )
        };
        if !source_exists || !destination_exists {
            return Err(GraphicsError::ImageNotFound(image));
        }
        // A PNG/GIF (`f=100`) image decodes to RGBA8 so `a=c` can compose on
        // pixels instead of rejecting the frame.
        let bytes_per_pixel = match format {
            24 => 3usize,
            32 | 100 => 4usize,
            _ => {
                self.diagnose(
                    Some(image),
                    "cannot compose animation frames for a non-raw (PNG/GIF) image".to_owned(),
                );
                return Err(GraphicsError::InvalidParameter(
                    "animation frame composition".to_owned(),
                ));
            }
        };
        let width = if width == 0 { pixel_width } else { width };
        let height = if height == 0 { pixel_height } else { height };
        let (source_x, source_y) = source_offset;
        let (destination_x, destination_y) = destination_offset;
        if source_x.saturating_add(width) > pixel_width
            || source_y.saturating_add(height) > pixel_height
            || destination_x.saturating_add(width) > pixel_width
            || destination_y.saturating_add(height) > pixel_height
        {
            return Err(GraphicsError::InvalidParameter(
                "animation frame composition".to_owned(),
            ));
        }
        if source_frame == destination_frame
            && rect_overlap(source_offset, destination_offset, (width, height))
        {
            return Err(GraphicsError::InvalidParameter(
                "animation frame composition".to_owned(),
            ));
        }
        // Coalesce the source and destination frames into full-image buffers
        // so `a=c` composes the rendered pixels of a delta frame rather than
        // its raw partial rectangle.
        let source_full = self.coalesce_frame_metadata(image, source_frame)?;
        let mut destination_full = self.coalesce_frame_metadata(image, destination_frame)?;
        // The result is transient when either source frame is (Kitty's
        // `bool transient = src_data.transient || dest_data.transient`).
        let destination_transient = source_full.transient || destination_full.transient;

        // Read the source rectangle into an owned buffer so a same-frame
        // composition cannot observe its own writes.
        let row_bytes = usize::try_from(width)
            .unwrap_or(usize::MAX)
            .saturating_mul(bytes_per_pixel);
        let mut source_rect = Vec::with_capacity(
            row_bytes.saturating_mul(usize::try_from(height).unwrap_or(usize::MAX)),
        );
        for row in 0..height {
            let start = (u64::from(source_y) + u64::from(row))
                .saturating_mul(u64::from(pixel_width))
                .saturating_add(u64::from(source_x))
                as usize;
            let start = start.saturating_mul(bytes_per_pixel);
            let end = start.saturating_add(row_bytes);
            source_rect.extend_from_slice(source_full.payload.get(start..end).unwrap_or(&[]));
        }
        // Write the rectangle into the destination frame.
        let blends = bytes_per_pixel == 4 && compose_mode == 0;
        for row in 0..height {
            let destination_row = (u64::from(destination_y) + u64::from(row))
                .saturating_mul(u64::from(pixel_width))
                .saturating_add(u64::from(destination_x))
                as usize;
            for column in 0..width {
                let destination_index =
                    (destination_row + column as usize) * bytes_per_pixel;
                let source_index = ((row * width + column) as usize) * bytes_per_pixel;
                if blends {
                    alpha_blend_onto(
                        &mut destination_full.payload[destination_index..destination_index + 4],
                        &source_rect[source_index..source_index + 4],
                    );
                } else {
                    destination_full.payload[destination_index..destination_index + bytes_per_pixel]
                        .copy_from_slice(
                            &source_rect[source_index..source_index + bytes_per_pixel],
                        );
                }
            }
        }
        // Store the composed result back, updating decoded-byte accounting if
        // coalescing a delta frame grew its stored size.
        let previous_destination_bytes = if destination_frame == 1 {
            self.resources
                .get(&image)
                .map_or(0, |resource| resource.decoded_payload.len())
        } else {
            self.resources
                .get(&image)
                .and_then(|resource| resource.animation_frames.get(&destination_frame))
                .map_or(0, |frame| frame.payload.len())
        };
        self.decoded_bytes = self
            .decoded_bytes
            .saturating_sub(previous_destination_bytes)
            .saturating_add(destination_full.payload.len());
        {
            let resource = self
                .resources
                .get_mut(&image)
                .expect("resource validated above");
            if destination_frame == 1 {
                // Composing onto the root of a PNG/GIF image decodes it to
                // RGBA8, so the resource now stores raw RGBA (format 32).
                if resource.format == 100 {
                    resource.format = 32;
                }
                resource.encoded_payload = encode_base64_payload(&destination_full.payload);
                resource.decoded_payload = destination_full.payload;
                resource.transient = destination_transient;
            } else {
                let animation_frame = resource
                    .animation_frames
                    .get_mut(&destination_frame)
                    .expect("frame validated above");
                animation_frame.payload = destination_full.payload;
                animation_frame.transient = destination_transient;
                animation_frame.width = pixel_width;
                animation_frame.height = pixel_height;
                animation_frame.x = 0;
                animation_frame.y = 0;
                animation_frame.base_frame = 0;
                animation_frame.compose_mode = 0;
                animation_frame.bgcolor = None;
            }
        }
        Ok(())
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

    /// Resolves a Kitty `I` image number to the newest surviving image with
    /// that number. Images created with `I` always receive a monotonically
    /// increasing internal id, so the largest id is the newest. Deleted images
    /// are absent from `resources`, so a number whose newest image was deleted
    /// naturally falls back to the next-newest surviving image.
    fn resolve_image_number(&self, number: u32) -> Option<u32> {
        self.resources
            .iter()
            .filter(|(_, resource)| resource.image_number == number)
            .map(|(image, _)| *image)
            .max()
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
        // Kitty's `U=1` key marks a virtual placement: an invisible prototype
        // for Unicode-placeholder images that never renders or scrolls and is
        // only removed by the id/number/range delete selectors.
        let virtual_placement = values.get("U").is_some_and(|value| value == "1");
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
        // Kitty's X/Y keys are sub-cell pixel offsets from the anchor cell's
        // top-left corner, so the image can be placed below cell granularity.
        let cell_x_offset = parameter_u16(values, "X", 0)?;
        let cell_y_offset = parameter_u16(values, "Y", 0)?;
        let (width, height) =
            placement_dimensions(values, pixel_size, cell_size, (cell_x_offset, cell_y_offset))?;
        let source = source_rect(values, pixel_size)?;
        let (drawn_width, drawn_height) = drawn_dimensions(
            width,
            height,
            values.contains_key("c"),
            values.contains_key("r"),
            cell_size,
            (cell_x_offset, cell_y_offset),
            source.map_or(pixel_size, |rect| (rect.width(), rect.height())),
        );
        // Kitty's P/Q keys mark a placement as relative to another placement;
        // H/V are the signed cell offsets from the parent's top-left cell.
        let parent = if values.contains_key("P") || values.contains_key("Q") {
            // Virtual placements (U=1) cannot themselves be relative.
            if virtual_placement {
                return Err(GraphicsError::InvalidParameter(
                    "virtual placements cannot be relative".to_owned(),
                ));
            }
            let parent_image = parameter_u32(values, "P", 0)?;
            let parent_placement_id = parameter_u32(values, "Q", 0)?;
            if parent_image == 0 || parent_placement_id == 0 {
                return Err(GraphicsError::ParentNotFound(parent_image.max(1)));
            }
            let parent = GraphicsPlacementParent::new(
                parent_image,
                parent_placement_id,
                parameter_i32(values, "H", 0)?,
                parameter_i32(values, "V", 0)?,
            );
            self.validate_relative_parent(parent, key)?;
            Some(parent)
        } else {
            None
        };
        // The Kitty protocol moves the cursor after placing an image unless
        // the client explicitly requests a static cursor with C=1. Absence of
        // the C key is equivalent to C=0 (move the cursor). Relative placements
        // never move the cursor, regardless of C.
        let cursor_static = values.get("C").is_some_and(|value| value == "1");
        // A relative placement lives in its parent's group, so it inherits the
        // parent's screen and scrolling region and resolves against them.
        let (anchor_screen, anchor_region, anchor_region_scroll) = parent.map_or(
            (screen, scroll_region, region_scroll),
            |parent| {
                let parent_anchor =
                    self.placements[&relative_parent_key(parent)].anchor();
                (
                    parent_anchor.screen(),
                    parent_anchor.scroll_region(),
                    parent_anchor.region_scroll(),
                )
            },
        );
        // Record the resolved cell origin for diagnostics/consumers that read
        // `x`/`y` without re-resolving relative parents.
        let (logical_x, logical_y) = parent.map_or(
            (i32::from(cursor.0), i32::from(cursor.1)),
            |parent| {
                let origin = self
                    .resolve_origin(
                        &self.placements[&relative_parent_key(parent)],
                        scrollback,
                        scroll_region,
                        region_scroll,
                        0,
                        0,
                    )
                    .unwrap_or((i32::from(cursor.0), i32::from(cursor.1)));
                (
                    origin.0.saturating_add(parent.cell_offset_x()),
                    origin.1.saturating_add(parent.cell_offset_y()),
                )
            },
        );
        let placement = GraphicsPlacement {
            resource: GraphicsResourceId::new(self.session, image),
            placement_id: requested_placement_id,
            x: clamp_to_u16(logical_x),
            y: clamp_to_u16(logical_y),
            width,
            height,
            z_index: parameter_i16(values, "z", 0)?,
            source,
            cursor_static,
            cell_x_offset,
            cell_y_offset,
            drawn_width,
            drawn_height,
            cell_width_pixels: cell_size.0,
            cell_height_pixels: cell_size.1,
            anchor: GraphicsGridAnchor::new(cursor.0, cursor.1, scrollback)
                .with_screen(anchor_screen)
                .with_scroll_region(anchor_region, anchor_region_scroll),
            parent,
            virtual_placement,
        };
        // Relative and virtual placements never move the cursor, regardless of
        // the `C` key (Kitty excludes `unicode_placement` from cursor motion).
        self.last_cursor_advance = if parent.is_some() || cursor_static || virtual_placement {
            None
        } else {
            Some((width, height))
        };
        self.placements.insert(key, placement);
        Ok(())
    }

    /// Validates a relative placement's parent chain: the parent must exist
    /// (`ENOPARENT`), must not create a cycle through `new_key` (`ECYCLE`), and
    /// the resulting chain must not exceed [`MAX_RELATIVE_DEPTH`] (`ETOODEEP`).
    fn validate_relative_parent(
        &self,
        parent: GraphicsPlacementParent,
        new_key: u64,
    ) -> Result<(), GraphicsError> {
        let mut key = relative_parent_key(parent);
        let mut depth = 1usize;
        loop {
            let placement = self
                .placements
                .get(&key)
                .ok_or(GraphicsError::ParentNotFound(parent.image()))?;
            if key == new_key {
                return Err(GraphicsError::RelativeCycle);
            }
            match placement.parent() {
                None => return Ok(()),
                Some(up) => {
                    depth += 1;
                    if depth > MAX_RELATIVE_DEPTH {
                        return Err(GraphicsError::RelativeDepthExceeded);
                    }
                    key = relative_parent_key(up);
                }
            }
        }
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
        view_offset: usize,
    ) -> Vec<GraphicsSubmission> {
        let view_offset = i32::try_from(view_offset).unwrap_or(i32::MAX);
        let mut submissions = self
            .placements
            .values()
            .filter_map(|placement| {
                if placement.anchor.screen() != current_screen {
                    return None;
                }
                // Virtual placements (U=1) are invisible prototypes; they never
                // produce a rendered submission.
                if placement.is_virtual() {
                    return None;
                }
                let resource = self.resources.get(&placement.resource.image())?;
                let (column, resolved_y) = self.resolve_origin(
                    placement,
                    current_scrollback,
                    current_region,
                    current_region_scroll,
                    view_offset,
                    0,
                )?;
                let placement_area = (
                    i32::from(surface.x) + column,
                    i32::from(surface.y) + resolved_y,
                    placement.width,
                    placement.height,
                );
                let clipped_area = intersect_signed(placement_area, surface)?;
                let offset_x = u32::try_from(i32::from(clipped_area.x) - placement_area.0).unwrap_or(0);
                let offset_y = u32::try_from(i32::from(clipped_area.y) - placement_area.1).unwrap_or(0);
                let clipped = clip_placement(
                    placement,
                    offset_x,
                    offset_y,
                    clipped_area.width,
                    clipped_area.height,
                    (resource.pixel_width, resource.pixel_height),
                )?;
                // An animation serves the coalesced current frame (with a
                // bumped generation so the outer terminal re-uploads it) rather
                // than the static root frame.
                let (served_payload, served_generation) =
                    self.served_graphics_payload(placement.resource.image(), resource);
                Some(GraphicsSubmission {
                    resource: placement.resource,
                    format: resource.format,
                    generation: served_generation,
                    encoded_payload: served_payload,
                    pixel_width: resource.pixel_width,
                    pixel_height: resource.pixel_height,
                    placement: GraphicsPlacement {
                        x: clipped_area.x,
                        y: clipped_area.y,
                        width: clipped_area.width,
                        height: clipped_area.height,
                        source: clipped.source,
                        cell_x_offset: clipped.cell_x_offset,
                        cell_y_offset: clipped.cell_y_offset,
                        drawn_width: clipped.drawn_width,
                        drawn_height: clipped.drawn_height,
                        ..*placement
                    },
                })
            })
            .collect::<Vec<_>>();
        // Kitty draws equal-z placements in ascending image-id order (lower
        // ids first, higher ids occluding them), so the tie-break must be the
        // image id rather than insertion order.
        submissions.sort_by_key(|submission| {
            (
                submission.placement.z_index(),
                submission.placement.resource.image(),
            )
        });
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

/// Whether two same-size rectangles at `a` and `b` overlap on either axis.
/// Mirrors Kitty's same-frame source/destination overlap rejection.
fn rect_overlap(a: (u32, u32), b: (u32, u32), size: (u32, u32)) -> bool {
    let (a_x, a_y) = a;
    let (b_x, b_y) = b;
    let (width, height) = size;
    let x_overlaps = a_x.max(b_x) < a_x.min(b_x).saturating_add(width);
    let y_overlaps = a_y.max(b_y) < a_y.min(b_y).saturating_add(height);
    x_overlaps && y_overlaps
}

/// Source-over alpha blends a 4-byte RGBA source pixel onto a destination
/// pixel, matching Kitty's `alpha_blend` for animation composition.
fn alpha_blend_onto(destination: &mut [u8], source: &[u8]) {
    let source_alpha = u32::from(source[3]);
    if source_alpha == 0 {
        return;
    }
    if source_alpha == 255 {
        destination.copy_from_slice(source);
        return;
    }
    let destination_alpha = u32::from(destination[3]);
    let result_alpha = source_alpha + destination_alpha * (255 - source_alpha) / 255;
    if result_alpha == 0 {
        destination.copy_from_slice(&[0, 0, 0, 0]);
        return;
    }
    for channel in 0..3 {
        destination[channel] = ((u32::from(source[channel]) * source_alpha
            + u32::from(destination[channel]) * destination_alpha * (255 - source_alpha) / 255)
            / result_alpha) as u8;
    }
    destination[3] = result_alpha as u8;
}

/// Parameters for composing one pixel rectangle onto a full-frame buffer,
/// shared by `a=f` frame coalescing and `a=c` animation composition.
#[derive(Clone, Copy, Debug)]
struct RectCompose {
    under_width: u32,
    bytes_per_pixel: usize,
    over_x: u32,
    over_y: u32,
    over_width: u32,
    over_height: u32,
    compose_mode: u32,
}

/// Fills a full-frame buffer with Kitty's `Y` background canvas color,
/// expressed as a packed 0xRRGGBBAA value (the same layout as
/// `get_coalesced_frame_data_standalone` in Kitty's `graphics.c`).
fn fill_bgcolor_buffer(total_bytes: usize, bytes_per_pixel: usize, color: u32) -> Vec<u8> {
    let red = ((color >> 24) & 0xff) as u8;
    let green = ((color >> 16) & 0xff) as u8;
    let blue = ((color >> 8) & 0xff) as u8;
    let alpha = (color & 0xff) as u8;
    let mut buffer = Vec::with_capacity(total_bytes);
    let pixel_count = total_bytes / bytes_per_pixel.max(1);
    for _ in 0..pixel_count {
        if bytes_per_pixel == 4 {
            buffer.extend_from_slice(&[red, green, blue, alpha]);
        } else {
            buffer.extend_from_slice(&[red, green, blue]);
        }
    }
    buffer
}

/// Composes a pixel rectangle (`over`, sized `over_width` x `over_height`) onto
/// a full-frame buffer (`under`, `under_width` wide) at `(over_x, over_y)`.
/// `compose_mode == 0` alpha-blends (for 4-byte RGBA pixels) and `compose_mode
/// == 1` overwrites, matching Kitty's `compose` / `alpha_blend` semantics.
fn compose_rect_onto(under: &mut [u8], over: &[u8], compose: RectCompose) {
    let RectCompose {
        under_width,
        bytes_per_pixel,
        over_x,
        over_y,
        over_width,
        over_height,
        compose_mode,
    } = compose;
    let blends = bytes_per_pixel == 4 && compose_mode == 0;
    for row in 0..over_height {
        let under_row = (u64::from(over_y) + u64::from(row))
            .saturating_mul(u64::from(under_width))
            .saturating_add(u64::from(over_x)) as usize;
        for column in 0..over_width {
            let destination_index = (under_row + column as usize) * bytes_per_pixel;
            let source_index = ((row * over_width + column) as usize) * bytes_per_pixel;
            if blends {
                alpha_blend_onto(
                    &mut under[destination_index..destination_index + 4],
                    &over[source_index..source_index + 4],
                );
            } else {
                under[destination_index..destination_index + bytes_per_pixel]
                    .copy_from_slice(&over[source_index..source_index + bytes_per_pixel]);
            }
        }
    }
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

fn parameter_u16(
    values: &BTreeMap<String, String>,
    key: &str,
    default: u16,
) -> Result<u16, GraphicsError> {
    values
        .get(key)
        .map(|value| {
            value
                .parse()
                .map_err(|_| GraphicsError::InvalidParameter(value.clone()))
        })
        .unwrap_or(Ok(default))
}

/// Resolves the cell extent (columns, rows) of a placement, mirroring how a
/// real Kitty terminal computes its destination rectangle.
///
/// Explicit `c`/`r` values win. When only one is given, the other is derived
/// from the source image's aspect ratio so the image is not distorted; when
/// neither is given, the natural image size is converted to cells using the
/// reported cell pixel size.
fn placement_dimensions(
    values: &BTreeMap<String, String>,
    pixel_size: (u32, u32),
    cell_size: (u16, u16),
    (cell_x_offset, cell_y_offset): (u16, u16),
) -> Result<(u16, u16), GraphicsError> {
    let parse = |key: &str| -> Result<Option<u16>, GraphicsError> {
        values
            .get(key)
            .map(|value| {
                value
                    .parse::<u16>()
                    .map(|parsed| parsed.max(1))
                    .map_err(|_| GraphicsError::InvalidParameter(value.clone()))
            })
            .transpose()
    };
    let columns = parse("c")?;
    let rows = parse("r")?;
    let (pixel_width, pixel_height) = pixel_size;
    let (cell_width, cell_height) = cell_size;

    match (columns, rows) {
        (Some(columns), Some(rows)) => Ok((columns, rows)),
        (Some(columns), None) => {
            if pixel_width == 0 || pixel_height == 0 || cell_width == 0 || cell_height == 0 {
                return Ok((
                    columns,
                    natural_extent(pixel_height, cell_height, cell_y_offset),
                ));
            }
            let width_pixels =
                u128::from(columns) * u128::from(cell_width) + u128::from(cell_x_offset);
            let height_pixels =
                width_pixels * u128::from(pixel_height) / u128::from(pixel_width);
            let rows = ceil_extent(height_pixels, cell_height);
            Ok((columns, rows))
        }
        (None, Some(rows)) => {
            if pixel_width == 0 || pixel_height == 0 || cell_width == 0 || cell_height == 0 {
                return Ok((
                    natural_extent(pixel_width, cell_width, cell_x_offset),
                    rows,
                ));
            }
            let height_pixels =
                u128::from(rows) * u128::from(cell_height) + u128::from(cell_y_offset);
            let width_pixels =
                height_pixels * u128::from(pixel_width) / u128::from(pixel_height);
            let columns = ceil_extent(width_pixels, cell_width);
            Ok((columns, rows))
        }
        (None, None) => Ok((
            natural_extent(pixel_width, cell_width, cell_x_offset),
            natural_extent(pixel_height, cell_height, cell_y_offset),
        )),
    }
}

/// Converts a natural image dimension in pixels to a cell count, adding the
/// sub-cell pixel offset so an image that starts partway into its first cell
/// still occupies the correct number of cells.
///
/// Pixel-size ioctls are unavailable on a number of terminals. When that
/// happens the known natural pixel geometry is preserved instead of collapsing
/// the placement to 1x1; the backend can later refine this estimate once a
/// cell size is available.
fn natural_extent(pixels: u32, cell_pixels: u16, offset: u16) -> u16 {
    if pixels == 0 {
        return 1;
    }
    if cell_pixels == 0 {
        return pixels
            .saturating_add(u32::from(offset))
            .min(u32::from(u16::MAX)) as u16;
    }
    ceil_extent(u128::from(pixels) + u128::from(offset), cell_pixels)
}

fn ceil_extent(pixels: u128, cell_pixels: u16) -> u16 {
    pixels
        .div_ceil(u128::from(cell_pixels.max(1)))
        .min(u128::from(u16::MAX))
        .max(1) as u16
}

/// Computes the on-screen pixel size a placement's source rectangle is scaled
/// to, mirroring how a real Kitty terminal renders a placement. The drawn
/// rectangle is aligned to the bottom-right of the placement's cell extent, so
/// the sub-cell `X`/`Y` offset is subtracted from the cell span rather than
/// added to it (see Kitty's `grman_update_layers`).
///
/// `columns_given`/`rows_given` record which of `c`/`r` the client supplied;
/// when only one is given the other is derived from the source aspect ratio.
/// When neither is given the image is drawn at its natural size.
fn drawn_dimensions(
    columns: u16,
    rows: u16,
    columns_given: bool,
    rows_given: bool,
    cell_size: (u16, u16),
    (cell_x_offset, cell_y_offset): (u16, u16),
    source: (u32, u32),
) -> (u32, u32) {
    let (cell_width, cell_height) = cell_size;
    let (source_width, source_height) = source;
    // Without a known cell size the on-screen scaling cannot be derived; the
    // natural source size is the only sensible estimate.
    if cell_width == 0 || cell_height == 0 {
        return (source_width, source_height);
    }
    let cell_width = u32::from(cell_width);
    let cell_height = u32::from(cell_height);
    // Kitty clamps sub-cell offsets to the last pixel of the anchor cell.
    let x = u32::from(cell_x_offset).min(cell_width.saturating_sub(1));
    let y = u32::from(cell_y_offset).min(cell_height.saturating_sub(1));
    let columns = u32::from(columns);
    let rows = u32::from(rows);
    let width_px = if columns_given {
        columns.saturating_mul(cell_width).saturating_sub(x)
    } else if rows_given {
        aspect_scale(
            rows.saturating_mul(cell_height).saturating_sub(y),
            source_width,
            source_height,
        )
    } else {
        source_width
    };
    let height_px = if rows_given {
        rows.saturating_mul(cell_height).saturating_sub(y)
    } else if columns_given {
        aspect_scale(
            columns.saturating_mul(cell_width).saturating_sub(x),
            source_height,
            source_width,
        )
    } else {
        source_height
    };
    (width_px.max(1), height_px.max(1))
}

/// Scales `pixels` by the aspect ratio `num`/`den`, saturating at the `u32`
/// maximum. A zero numerator or denominator indicates an unknown dimension, in
/// which case the input is returned unchanged.
fn aspect_scale(pixels: u32, num: u32, den: u32) -> u32 {
    if num == 0 || den == 0 {
        return pixels;
    }
    (u128::from(pixels) * u128::from(num) / u128::from(den))
        .min(u128::from(u32::MAX)) as u32
}

fn placement_id(values: &BTreeMap<String, String>) -> Option<u32> {
    values
        .get("p")
        .and_then(|value| value.parse().ok())
        .filter(|id| *id != 0)
}

/// The `self.placements` map key for a parent placement identified by Kitty's
/// `P`/`Q` keys (parent image id and parent placement id).
fn relative_parent_key(parent: GraphicsPlacementParent) -> u64 {
    (u64::from(parent.image()) << 32) | u64::from(parent.placement_id())
}

/// Clamps a possibly-negative cell coordinate into the `u16` range used by
/// placement metadata (positions below the grid are clipped at render time).
fn clamp_to_u16(value: i32) -> u16 {
    value.clamp(0, i32::from(u16::MAX)) as u16
}

/// Whether a cell rectangle `(x, y, width, height)` contains the cell at
/// `(column, row)`.
fn rect_contains_cell(rect: (i32, i32, i32, i32), column: i32, row: i32) -> bool {
    let (x, y, width, height) = rect;
    column >= x && column < x.saturating_add(width) && row >= y && row < y.saturating_add(height)
}

/// Whether a cell rectangle spans the given column.
fn rect_intersects_column(rect: (i32, i32, i32, i32), column: i32) -> bool {
    let (x, _, width, _) = rect;
    column >= x && column < x.saturating_add(width)
}

/// Whether a cell rectangle spans the given row.
fn rect_intersects_row(rect: (i32, i32, i32, i32), row: i32) -> bool {
    let (_, y, _, height) = rect;
    row >= y && row < y.saturating_add(height)
}

/// Parses a signed cell offset (`H`/`V`) for a relative placement.
fn parameter_i32(
    values: &BTreeMap<String, String>,
    key: &str,
    default: i32,
) -> Result<i32, GraphicsError> {
    values
        .get(key)
        .map(|value| {
            value
                .parse()
                .map_err(|_| GraphicsError::InvalidParameter(value.clone()))
        })
        .unwrap_or(Ok(default))
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

/// Whether a command response should be emitted under a Kitty `q` quiet
/// value, mirroring `finish_command_response`: `q=1` suppresses success (`OK`)
/// responses and any `q >= 2` suppresses every response (success and failure).
pub(crate) fn should_emit_response(quiet: u8, is_ok: bool) -> bool {
    quiet == 0 || (!is_ok && quiet < 2)
}

fn kitty_response(image: u32, placement: Option<u32>, message: &str) -> Vec<u8> {
    let placement = placement.map_or(String::new(), |id| format!(",p={id}"));
    format!("\x1b_Gi={image}{placement};{message}\x1b\\").into_bytes()
}

/// Builds a response that reports an assigned image id together with the `I`
/// image number a client used to create it.
fn kitty_response_with_number(
    image: u32,
    number: u32,
    placement: Option<u32>,
    message: &str,
) -> Vec<u8> {
    let placement = placement.map_or(String::new(), |id| format!(",p={id}"));
    format!("\x1b_Gi={image},I={number}{placement};{message}\x1b\\").into_bytes()
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
    let number = values
        .get("I")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let placement = placement_id(&values);
    let message = match error {
        GraphicsError::ImageNotFound(_) => format!("ENOENT:{error}"),
        GraphicsError::ParentNotFound(_) => format!("ENOPARENT:{error}"),
        GraphicsError::RelativeCycle => format!("ECYCLE:{error}"),
        GraphicsError::RelativeDepthExceeded => format!("ETOODEEP:{error}"),
        _ => format!("EINVAL:{error}"),
    };
    if number != 0 {
        kitty_response_with_number(image, number, placement, &message)
    } else {
        kitty_response(image, placement, &message)
    }
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
        100 => {
            if payload.len() >= 10 && payload.starts_with(b"GIF") {
                Some((
                    u32::from(u16::from_le_bytes([payload[6], payload[7]])),
                    u32::from(u16::from_le_bytes([payload[8], payload[9]])),
                ))
            } else if payload.len() >= 24 && payload.starts_with(b"\x89PNG\r\n\x1a\n") {
                Some((
                    u32::from_be_bytes(payload[16..20].try_into().ok()?),
                    u32::from_be_bytes(payload[20..24].try_into().ok()?),
                ))
            } else {
                None
            }
        }
        24 | 32 if payload.len() >= 24 && payload.starts_with(b"\x89PNG\r\n\x1a\n") => Some((
            u32::from_be_bytes(payload[16..20].try_into().ok()?),
            u32::from_be_bytes(payload[20..24].try_into().ok()?),
        )),
        _ => None,
    }
}

/// A decoded animated-GIF frame set: the first frame is the root image and
/// each subsequent frame is a full-canvas RGBA keyframe with its own delay.
struct DecodedGifAnimation {
    width: u32,
    height: u32,
    root_rgba: Vec<u8>,
    root_gap_ms: u32,
    extra_frames: Vec<GifDecodedFrame>,
    /// Kitty's `v` loop count (`1` loops forever, larger values loop `v - 1`
    /// times, mapped from the GIF Netscape loop count).
    animation_loops: u32,
}

struct GifDecodedFrame {
    rgba: Vec<u8>,
    gap_ms: u32,
}

/// Decodes an `f=100` payload that contains an animated GIF into coalesced
/// full-canvas RGBA frames. Returns `None` for non-GIF payloads, static GIFs
/// (a single frame), or GIFs that cannot be decoded within the storage budget.
fn decode_gif_animation(payload: &[u8], max_decoded_bytes: usize) -> Option<DecodedGifAnimation> {
    if !payload.starts_with(b"GIF") {
        return None;
    }
    let limit = std::num::NonZeroU64::new(max_decoded_bytes.try_into().ok()?)?;
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    options.set_memory_limit(gif::MemoryLimit::Bytes(limit));
    let mut decoder = options.read_info(payload).ok()?;
    let width = u32::from(decoder.width());
    let height = u32::from(decoder.height());
    if width == 0 || height == 0 {
        return None;
    }
    let canvas_bytes = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if canvas_bytes > max_decoded_bytes {
        return None;
    }
    let canvas_width = width as usize;
    let canvas_height = height as usize;
    let mut canvas = vec![0u8; canvas_bytes];
    let mut frames: Vec<(Vec<u8>, u32)> = Vec::new();
    let mut any_nonzero_gap = false;
    loop {
        let frame = decoder.read_next_frame().ok()?;
        let Some(frame) = frame else { break };
        let frame_width = frame.width as usize;
        let frame_height = frame.height as usize;
        let left = frame.left as usize;
        let top = frame.top as usize;
        if left.checked_add(frame_width)? > canvas_width
            || top.checked_add(frame_height)? > canvas_height
        {
            return None;
        }
        // Save the pre-composition canvas for `Previous` disposal, which
        // restores the canvas to its state before this frame was drawn.
        let before = if frame.dispose == gif::DisposalMethod::Previous {
            Some(canvas.clone())
        } else {
            None
        };
        // Composite this frame's opaque pixels onto the canvas (transparent
        // pixels leave the underlying canvas untouched, as in a browser).
        for row in 0..frame_height {
            let src_row = &frame.buffer[row * frame_width * 4..(row + 1) * frame_width * 4];
            let dst_row = (top + row) * canvas_width * 4 + left * 4;
            for column in 0..frame_width {
                let src = &src_row[column * 4..column * 4 + 4];
                if src[3] != 0 {
                    canvas[dst_row + column * 4..dst_row + column * 4 + 4].copy_from_slice(src);
                }
            }
        }
        let gap_ms = u32::from(frame.delay) * 10;
        if gap_ms != 0 {
            any_nonzero_gap = true;
        }
        frames.push((canvas.clone(), gap_ms));
        // Apply the frame's disposal to prepare the canvas for the next frame.
        match frame.dispose {
            gif::DisposalMethod::Background => {
                for row in top..top + frame_height {
                    canvas
                        [(row * canvas_width + left) * 4..(row * canvas_width + left + frame_width) * 4]
                        .fill(0);
                }
            }
            gif::DisposalMethod::Previous => {
                canvas = before.expect("previous canvas is saved above");
            }
            gif::DisposalMethod::Any | gif::DisposalMethod::Keep => {}
        }
    }
    if frames.len() < 2 {
        // A static GIF has nothing to animate; leave it a static image.
        return None;
    }
    // Browsers and Kitty's `kitten icat` render an all-zero-delay GIF with a
    // 100 ms per-frame gap rather than an infinitely fast one.
    if !any_nonzero_gap {
        for (_, gap) in &mut frames {
            *gap = 100;
        }
    }
    let mut frames = frames.into_iter();
    let (root_rgba, root_gap_ms) = frames.next().expect("at least one frame");
    let extra_frames = frames
        .map(|(rgba, gap_ms)| GifDecodedFrame { rgba, gap_ms })
        .collect();
    Some(DecodedGifAnimation {
        width,
        height,
        root_rgba,
        root_gap_ms,
        extra_frames,
        animation_loops: gif_repeat_to_animation_loops(decoder.repeat()),
    })
}

/// Maps a GIF Netscape loop count onto Kitty's `v` animation-control key.
/// `Infinite` loops forever; `Finite(n)` repeats `n` times (a total of `n + 1`
/// plays, so a GIF without a loop extension plays exactly once).
fn gif_repeat_to_animation_loops(repeat: gif::Repeat) -> u32 {
    match repeat {
        gif::Repeat::Infinite => 1,
        gif::Repeat::Finite(loops) => u32::from(loops).saturating_add(2),
    }
}

/// Decodes a PNG or GIF payload into RGBA8 pixels plus its dimensions, so a
/// non-raw (`f=100`) frame can be composed on pixels like a raw one. Returns
/// `None` for payloads that are not a recognized image or that cannot be
/// decoded within the storage budget.
fn decode_raster_image(
    payload: &[u8],
    max_decoded_bytes: usize,
) -> Option<(Vec<u8>, u32, u32)> {
    if payload.starts_with(b"\x89PNG\r\n\x1a\n") {
        decode_png_rgba(payload, max_decoded_bytes)
    } else if payload.starts_with(b"GIF") {
        decode_gif_rgba(payload, max_decoded_bytes)
    } else {
        None
    }
}

/// Decodes a PNG's first frame into RGBA8 pixels.
fn decode_png_rgba(payload: &[u8], max_decoded_bytes: usize) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new_with_limits(
        payload,
        png::Limits {
            bytes: max_decoded_bytes,
        },
    );
    decoder.set_transformations(
        png::Transformations::EXPAND
            | png::Transformations::STRIP_16
            | png::Transformations::ALPHA,
    );
    let mut reader = decoder.read_info().ok()?;
    let (width, height) = reader.info().size();
    if width == 0 || height == 0 {
        return None;
    }
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).ok()?;
    // With EXPAND + ALPHA the output is either RGBA or grayscale+alpha;
    // normalize both to RGBA8.
    let rgba = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity(buffer.len().saturating_mul(2));
            for pixel in buffer.chunks_exact(2) {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
            rgba
        }
        _ => return None,
    };
    if rgba.len() > max_decoded_bytes {
        return None;
    }
    Some((rgba, width, height))
}

/// Decodes a GIF's first frame (the root) into RGBA8 pixels, compositing its
/// opaque pixels onto a transparent canvas.
fn decode_gif_rgba(payload: &[u8], max_decoded_bytes: usize) -> Option<(Vec<u8>, u32, u32)> {
    let limit = std::num::NonZeroU64::new(max_decoded_bytes.try_into().ok()?)?;
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    options.set_memory_limit(gif::MemoryLimit::Bytes(limit));
    let mut decoder = options.read_info(payload).ok()?;
    let width = u32::from(decoder.width());
    let height = u32::from(decoder.height());
    if width == 0 || height == 0 {
        return None;
    }
    let canvas_bytes = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if canvas_bytes > max_decoded_bytes {
        return None;
    }
    let canvas_width = width as usize;
    let canvas_height = height as usize;
    let mut canvas = vec![0u8; canvas_bytes];
    let frame = decoder.read_next_frame().ok()??;
    let frame_width = frame.width as usize;
    let frame_height = frame.height as usize;
    let left = frame.left as usize;
    let top = frame.top as usize;
    if left.checked_add(frame_width)? > canvas_width
        || top.checked_add(frame_height)? > canvas_height
    {
        return None;
    }
    for row in 0..frame_height {
        let src_row = &frame.buffer[row * frame_width * 4..(row + 1) * frame_width * 4];
        let dst_row = (top + row) * canvas_width * 4 + left * 4;
        for column in 0..frame_width {
            let src = &src_row[column * 4..column * 4 + 4];
            if src[3] != 0 {
                canvas[dst_row + column * 4..dst_row + column * 4 + 4].copy_from_slice(src);
            }
        }
    }
    Some((canvas, width, height))
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

/// Inflates a zlib stream, bounding the decompressed size so a malicious
/// payload cannot exhaust memory.
fn decompress_zlib(
    encoded: &[u8],
    max_decoded_bytes: usize,
) -> Result<Vec<u8>, GraphicsError> {
    let mut decoder = ZlibDecoder::new(encoded);
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
    decompress_zlib(&encoded, max_decoded_bytes)
}

/// The maximum length of a Kitty file/shared-memory transfer name, matching
/// Kitty's `payload_sz > 2048` filename guard.
const MAX_TRANSFER_NAME_BYTES: usize = 2048;

/// Reads a byte range from an open file, honoring Kitty's `S` (size) and `O`
/// (offset) keys. `data_size == 0` reads the remainder of the file from the
/// offset. Returns `InvalidPayload` when the range is out of bounds or larger
/// than `cap` (used to enforce the decoded-storage budget).
fn read_file_range(
    file: &mut File,
    data_size: u32,
    data_offset: u32,
    cap: usize,
) -> Result<Vec<u8>, GraphicsError> {
    let metadata = file.metadata().map_err(|_| GraphicsError::InvalidPayload)?;
    let total = metadata.len();
    let offset = u64::from(data_offset);
    let size = if data_size == 0 {
        total.saturating_sub(offset)
    } else {
        u64::from(data_size)
    };
    if offset > total || size > total.saturating_sub(offset) {
        return Err(GraphicsError::InvalidPayload);
    }
    if size > cap as u64 {
        return Err(GraphicsError::InvalidPayload);
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| GraphicsError::InvalidPayload)?;
    let mut bytes = vec![0u8; size as usize];
    file.read_exact(&mut bytes)
        .map_err(|_| GraphicsError::InvalidPayload)?;
    Ok(bytes)
}

/// Reads a POSIX shared-memory object (`t=s`), unlinking its name afterwards
/// like Kitty does.
#[cfg(unix)]
fn read_shared_memory(
    name: &str,
    data_size: u32,
    data_offset: u32,
    cap: usize,
) -> Result<Vec<u8>, GraphicsError> {
    use std::os::fd::FromRawFd;
    let cname = std::ffi::CString::new(name).map_err(|_| GraphicsError::InvalidPayload)?;
    let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC, 0) };
    if fd < 0 {
        return Err(GraphicsError::InvalidPayload);
    }
    let result = {
        let mut file = unsafe { File::from_raw_fd(fd) };
        read_file_range(&mut file, data_size, data_offset, cap)
    };
    unsafe { libc::shm_unlink(cname.as_ptr()) };
    result
}

#[cfg(not(unix))]
fn read_shared_memory(
    _name: &str,
    _data_size: u32,
    _data_offset: u32,
    _cap: usize,
) -> Result<Vec<u8>, GraphicsError> {
    Err(GraphicsError::UnsupportedTransfer("s".to_owned()))
}

/// Reads a Kitty file (`t=f`), temporary-file (`t=t`), or shared-memory (`t=s`)
/// transfer into a raw byte buffer. Kitty deletes a `t=t` file only when its
/// name carries the `tty-graphics-protocol` marker (its own temp-file
/// convention), so a program cannot use `t=t` to remove an arbitrary path.
fn read_transfer_payload(
    filename: &[u8],
    transfer: &str,
    data_size: u32,
    data_offset: u32,
    cap: usize,
) -> Result<Vec<u8>, GraphicsError> {
    if filename.len() > MAX_TRANSFER_NAME_BYTES {
        return Err(GraphicsError::InvalidParameter(
            "transfer filename".to_owned(),
        ));
    }
    let name = std::str::from_utf8(filename)
        .map_err(|_| GraphicsError::InvalidParameter("transfer filename".to_owned()))?;
    let bytes = match transfer {
        "s" => read_shared_memory(name, data_size, data_offset, cap)?,
        "f" | "t" => {
            let mut file = File::open(name).map_err(|_| GraphicsError::InvalidPayload)?;
            read_file_range(&mut file, data_size, data_offset, cap)?
        }
        other => return Err(GraphicsError::UnsupportedTransfer(other.to_owned())),
    };
    if transfer == "t" && name.contains("tty-graphics-protocol") {
        let _ = std::fs::remove_file(name);
    }
    Ok(bytes)
}

/// Resolves the raw image bytes for a command, accounting for its Kitty
/// transfer mode: direct (`t=d`) payloads are base64 (and optionally zlib)
/// encoded, while file/shared-memory (`t=f`/`t=t`/`t=s`) payloads name a path
/// whose contents are the (optionally zlib-compressed) image data.
fn resolve_transfer_payload(
    values: &BTreeMap<String, String>,
    encoded_payload: &[u8],
    compression: &str,
    max_decoded_bytes: usize,
) -> Result<Vec<u8>, GraphicsError> {
    let transfer = values.get("t").map(String::as_str).unwrap_or("d");
    match transfer {
        "d" => decode_graphics_payload(encoded_payload, compression, max_decoded_bytes),
        "f" | "t" | "s" => {
            let filename = decode_base64(encoded_payload).ok_or(GraphicsError::InvalidPayload)?;
            let data_size = parameter_u32(values, "S", 0)?;
            let data_offset = parameter_u32(values, "O", 0)?;
            let raw = read_transfer_payload(
                &filename,
                transfer,
                data_size,
                data_offset,
                max_decoded_bytes.saturating_mul(2).saturating_add(4),
            )?;
            if compression == "z" {
                decompress_zlib(&raw, max_decoded_bytes)
            } else {
                if raw.len() > max_decoded_bytes {
                    return Err(GraphicsError::InvalidPayload);
                }
                Ok(raw)
            }
        }
        other => Err(GraphicsError::UnsupportedTransfer(other.to_owned())),
    }
}

pub(crate) fn decode_base64(payload: &[u8]) -> Option<Vec<u8>> {
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
    fn equal_z_placements_tie_break_by_image_id_not_insertion_order() {
        let mut store = SessionGraphicsStore::new(SessionId::new(90));
        // Insert the higher image id first so the old insertion-order
        // tie-break would have kept it first; Kitty orders equal-z placements
        // by ascending image id instead.
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=91,c=1,r=1,C=1,z=3,q=2",
                b"BAUG",
                (0, 0),
                (10, 10),
            )
            .unwrap();
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=90,c=1,r=1,C=1,z=3,q=2",
                b"AQID",
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let ids = store
            .visible_submissions(Rect::new(0, 0, 10, 10))
            .iter()
            .map(|submission| submission.resource().image())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![90, 91]);
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
    fn reflow_reanchors_placements_to_their_grid_row_when_columns_change() {
        let mut store = SessionGraphicsStore::new(SessionId::new(20));
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=9,c=1,r=1,q=2",
                b"AQID",
                (1, 4),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                GraphicsScrollRegion::new(0, 6, 6),
                0,
            )
            .unwrap();

        // A column reflow (8 -> 4) rewraps text and grows the scrollback from
        // 0 to 6 lines without scrolling content uniformly; the placement must
        // keep its grid row 4 instead of being shifted up into history.
        assert!(store.reanchor_on_resize(8, 4, 0, 6, GraphicsScrollRegion::new(0, 6, 6), 0));

        let visible = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            6,
            GraphicsScreen::Primary,
            GraphicsScrollRegion::new(0, 6, 6),
            0,
            0,
        );
        assert_eq!(visible[0].placement().area(), Rect::new(1, 4, 1, 1));
    }

    #[test]
    fn reflow_reanchors_history_placements_to_the_same_relative_row() {
        let mut store = SessionGraphicsStore::new(SessionId::new(21));
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=9,c=1,r=1,q=2",
                b"AQID",
                (1, 1),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                GraphicsScrollRegion::new(0, 6, 6),
                0,
            )
            .unwrap();

        // Before the resize the placement is nine lines into history, visible
        // again at its original row when the view scrolls back ten lines.
        let before = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            10,
            GraphicsScreen::Primary,
            GraphicsScrollRegion::new(0, 6, 6),
            0,
            10,
        );
        assert_eq!(before[0].placement().area(), Rect::new(1, 1, 1, 1));

        // A column reflow grows the scrollback from 10 to 14 lines; the
        // placement must keep resolving to the same relative position.
        store.reanchor_on_resize(8, 4, 10, 14, GraphicsScrollRegion::new(0, 6, 6), 0);

        let live = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            14,
            GraphicsScreen::Primary,
            GraphicsScrollRegion::new(0, 6, 6),
            0,
            0,
        );
        assert!(live.is_empty());

        let scrolled = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            14,
            GraphicsScreen::Primary,
            GraphicsScrollRegion::new(0, 6, 6),
            0,
            10,
        );
        assert_eq!(scrolled[0].placement().area(), Rect::new(1, 1, 1, 1));
    }

    #[test]
    fn row_only_resize_leaves_placements_on_the_scrollback_model() {
        let mut store = SessionGraphicsStore::new(SessionId::new(22));
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=9,c=1,r=1,q=2",
                b"AQID",
                (1, 4),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                GraphicsScrollRegion::new(0, 6, 6),
                0,
            )
            .unwrap();

        // A vertical-only resize must not re-anchor: the scrollback depth
        // change is a real scroll the linear model already tracks.
        assert!(!store.reanchor_on_resize(8, 8, 0, 3, GraphicsScrollRegion::new(0, 6, 6), 0));

        let visible = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            3,
            GraphicsScreen::Primary,
            GraphicsScrollRegion::new(0, 6, 6),
            0,
            0,
        );
        assert_eq!(visible[0].placement().area(), Rect::new(1, 1, 1, 1));
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
            0,
        );
        let scrolled = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            0,
            GraphicsScreen::Primary,
            region,
            1,
            0,
        );
        let different_region = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            0,
            GraphicsScreen::Primary,
            GraphicsScrollRegion::new(0, 6, 6),
            1,
            0,
        );
        assert_eq!(initial[0].placement().area(), Rect::new(1, 4, 1, 1));
        assert_eq!(scrolled[0].placement().area(), Rect::new(1, 3, 1, 1));
        assert_eq!(
            different_region[0].placement().area(),
            Rect::new(1, 4, 1, 1)
        );
    }

    #[test]
    fn scrollback_view_offset_reshows_full_screen_placements_in_history() {
        let mut store = SessionGraphicsStore::new(SessionId::new(18));
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=9,c=1,r=1,q=2",
                b"AQID",
                (1, 1),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                GraphicsScrollRegion::new(0, 6, 6),
                0,
            )
            .unwrap();

        // Ten lines of history later the placement sits at grid line -9, so it
        // is clipped out of the live viewport...
        let live = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            10,
            GraphicsScreen::Primary,
            GraphicsScrollRegion::new(0, 6, 6),
            0,
            0,
        );
        assert!(live.is_empty());

        // ...and scrolling the view to the top brings it back to screen row 1.
        let scrolled = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            10,
            GraphicsScreen::Primary,
            GraphicsScrollRegion::new(0, 6, 6),
            0,
            10,
        );
        assert_eq!(scrolled[0].placement().area(), Rect::new(1, 1, 1, 1));
    }

    #[test]
    fn scrollback_view_offset_leaves_partial_region_placements_fixed() {
        let mut store = SessionGraphicsStore::new(SessionId::new(19));
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

        // A partial-region placement must not follow the history view offset.
        let scrolled = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            0,
            GraphicsScreen::Primary,
            region,
            0,
            4,
        );
        assert_eq!(scrolled[0].placement().area(), Rect::new(1, 4, 1, 1));
    }

    #[test]
    fn placements_past_the_scrollback_limit_are_evicted_and_resources_freed() {
        let mut store = SessionGraphicsStore::new(SessionId::new(21));
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=9,c=1,r=1,q=2",
                b"AQID",
                (0, 0),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                GraphicsScrollRegion::new(0, 6, 6),
                0,
            )
            .unwrap();
        assert_eq!(store.placement_count(), 1);
        assert!(store.decoded_bytes(9).is_some());

        // At exactly `row + limit` scroll lines the placement is still the
        // topmost retained line, so it survives.
        assert!(!store.evict_beyond_scrollback_limit(
            10,
            GraphicsScreen::Primary,
            GraphicsScrollRegion::new(0, 6, 6),
            10,
        ));
        assert_eq!(store.placement_count(), 1);

        // One more scroll line pushes it above the retained history.
        assert!(store.evict_beyond_scrollback_limit(
            10,
            GraphicsScreen::Primary,
            GraphicsScrollRegion::new(0, 6, 6),
            11,
        ));
        assert_eq!(store.placement_count(), 0);
        assert_eq!(store.decoded_bytes(9), None);
    }

    #[test]
    fn partial_region_placements_are_not_evicted_by_the_scrollback_limit() {
        let mut store = SessionGraphicsStore::new(SessionId::new(22));
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

        // A huge full-screen scroll displacement must not touch a placement
        // anchored to a partial (DECSTBM) region.
        assert!(!store.evict_beyond_scrollback_limit(
            1,
            GraphicsScreen::Primary,
            region,
            1_000_000,
        ));
        assert_eq!(store.placement_count(), 1);
    }

    #[test]
    fn clear_screen_erases_visible_placements_and_preserves_history() {
        let mut store = SessionGraphicsStore::new(SessionId::new(23));
        let full = GraphicsScrollRegion::new(0, 6, 6);
        // Anchored before any scrolling: resolves to a history row once five
        // lines of scrollback exist.
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=1,c=1,r=1,q=2",
                b"AQID",
                (0, 0),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        // Anchored at the current scrollback depth: still visible.
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=2,c=1,r=1,q=2",
                b"AQID",
                (0, 0),
                (10, 20),
                5,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        assert_eq!(store.placement_count(), 2);

        assert!(store.apply_erase(
            GraphicsErase::ClearScreen(GraphicsScreen::Primary),
            5,
            full,
            0,
        ));
        assert_eq!(store.placement_count(), 1);
        // Pixel data is retained for re-display, matching Kitty's cache.
        assert!(store.decoded_bytes(1).is_some());
        assert!(store.decoded_bytes(2).is_some());

        // Only the history placement survives and re-shows when scrolled back.
        let scrolled = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            5,
            GraphicsScreen::Primary,
            full,
            0,
            5,
        );
        assert_eq!(scrolled.len(), 1);
        assert_eq!(scrolled[0].resource().image(), 1);
    }

    #[test]
    fn reset_erases_all_graphics_and_resources() {
        let mut store = SessionGraphicsStore::new(SessionId::new(24));
        let full = GraphicsScrollRegion::new(0, 6, 6);
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=7,c=1,r=1,q=2",
                b"AQID",
                (0, 0),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        assert_eq!(store.placement_count(), 1);

        assert!(store.apply_erase(GraphicsErase::All, 0, full, 0));
        assert_eq!(store.placement_count(), 0);
        assert_eq!(store.resource_count(), 0);
        assert_eq!(store.decoded_bytes_total(), 0);
    }

    #[test]
    fn alternate_erase_removes_only_alternate_placements() {
        let mut store = SessionGraphicsStore::new(SessionId::new(25));
        let full = GraphicsScrollRegion::new(0, 6, 6);
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=1,c=1,r=1,q=2",
                b"AQID",
                (0, 0),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=2,c=1,r=1,q=2",
                b"AQID",
                (0, 0),
                (10, 20),
                0,
                GraphicsScreen::Alternate,
                full,
                0,
            )
            .unwrap();
        assert_eq!(store.placement_count(), 2);

        assert!(store.apply_erase(GraphicsErase::Alternate, 0, full, 0));
        assert_eq!(store.placement_count(), 1);
        assert!(store.decoded_bytes(1).is_some());
        assert!(store.decoded_bytes(2).is_some());
    }

    #[test]
    fn clear_below_erases_from_cursor_row_to_the_bottom() {
        let mut store = SessionGraphicsStore::new(SessionId::new(26));
        let full = GraphicsScrollRegion::new(0, 6, 6);
        // Visible on the top row: must survive an `ED 0` at row 2.
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=1,c=1,r=1,q=2",
                b"AQID",
                (0, 0),
                (10, 20),
                5,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        // Visible on the cursor row: erased.
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=2,c=1,r=1,q=2",
                b"AQID",
                (0, 2),
                (10, 20),
                5,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        // Visible on the bottom row: erased.
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=3,c=1,r=1,q=2",
                b"AQID",
                (0, 5),
                (10, 20),
                5,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        // History placement anchored before the five lines of scrollback: kept.
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=4,c=1,r=1,q=2",
                b"AQID",
                (0, 2),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        assert_eq!(store.placement_count(), 4);

        assert!(store.apply_erase(
            GraphicsErase::ClearBelow(GraphicsScreen::Primary, 2),
            5,
            full,
            0,
        ));
        assert_eq!(store.placement_count(), 2);
        // The live viewport only shows the surviving top-row placement; the
        // history placement re-shows when scrolled back.
        let live = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            5,
            GraphicsScreen::Primary,
            full,
            0,
            0,
        );
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].resource().image(), 1);
        let scrolled = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            5,
            GraphicsScreen::Primary,
            full,
            0,
            5,
        );
        assert_eq!(scrolled.len(), 2);
        assert!(scrolled.iter().any(|s| s.resource().image() == 4));
    }

    #[test]
    fn clear_above_erases_from_top_to_cursor_row_but_keeps_history() {
        let mut store = SessionGraphicsStore::new(SessionId::new(27));
        let full = GraphicsScrollRegion::new(0, 6, 6);
        // Visible on the top row: erased by `ED 1` at row 2.
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=1,c=1,r=1,q=2",
                b"AQID",
                (0, 0),
                (10, 20),
                5,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        // Visible on the cursor row: erased.
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=2,c=1,r=1,q=2",
                b"AQID",
                (0, 2),
                (10, 20),
                5,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        // Visible below the cursor row: kept.
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=3,c=1,r=1,q=2",
                b"AQID",
                (0, 5),
                (10, 20),
                5,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        // History placement: kept (scrollback is outside `ED 1`'s scope).
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=4,c=1,r=1,q=2",
                b"AQID",
                (0, 0),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        assert_eq!(store.placement_count(), 4);

        assert!(store.apply_erase(
            GraphicsErase::ClearAbove(GraphicsScreen::Primary, 2),
            5,
            full,
            0,
        ));
        assert_eq!(store.placement_count(), 2);
        let live = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            5,
            GraphicsScreen::Primary,
            full,
            0,
            0,
        );
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].resource().image(), 3);
    }

    #[test]
    fn clear_scrollback_erases_only_history_placements() {
        let mut store = SessionGraphicsStore::new(SessionId::new(28));
        let full = GraphicsScrollRegion::new(0, 6, 6);
        // Visible placement: kept.
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=1,c=1,r=1,q=2",
                b"AQID",
                (0, 3),
                (10, 20),
                5,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        // Two history placements: erased.
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=2,c=1,r=1,q=2",
                b"AQID",
                (0, 3),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        store
            .apply_kitty_command_with_scroll_region(
                b"a=T,f=24,i=3,c=1,r=1,q=2",
                b"AQID",
                (0, 0),
                (10, 20),
                0,
                GraphicsScreen::Primary,
                full,
                0,
            )
            .unwrap();
        assert_eq!(store.placement_count(), 3);

        assert!(store.apply_erase(GraphicsErase::ClearScrollback, 5, full, 0));
        assert_eq!(store.placement_count(), 1);
        // Pixel data is retained, matching the emulator-erase cache.
        assert!(store.decoded_bytes(1).is_some());
        assert!(store.decoded_bytes(2).is_some());
        assert!(store.decoded_bytes(3).is_some());
        let live = store.visible_submissions_with_scroll_state(
            Rect::new(0, 0, 8, 6),
            5,
            GraphicsScreen::Primary,
            full,
            0,
            0,
        );
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].resource().image(), 1);
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
    fn outer_input_demultiplexer_routes_osc52_clipboard_responses_apart_from_keys() {
        let mut demux = GraphicsInputDemultiplexer::new(256);
        assert_eq!(
            demux.feed(b"a\x1b]52;c;aGVsbG8=\x07b"),
            vec![
                OuterInputEvent::TerminalInput(b"a".to_vec()),
                OuterInputEvent::ClipboardResponse(b"\x1b]52;c;aGVsbG8=\x07".to_vec()),
                OuterInputEvent::TerminalInput(b"b".to_vec()),
            ]
        );

        // ST-terminated responses are recognized too.
        assert_eq!(
            demux.feed(b"\x1b]52;pc;aGVsbG8=\x1b\\"),
            vec![OuterInputEvent::ClipboardResponse(
                b"\x1b]52;pc;aGVsbG8=\x1b\\".to_vec()
            )]
        );

        // Non-clipboard OSC stays on the terminal-input path so the keyboard
        // decoder owns it rather than the clipboard reader.
        assert_eq!(
            demux.feed(b"\x1b]0;title\x07"),
            vec![OuterInputEvent::TerminalInput(b"\x1b]0;title\x07".to_vec())]
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
    fn graphics_queries_acknowledge_supported_transfer_modes() {
        let mut store = SessionGraphicsStore::new(SessionId::new(7));
        // A direct query carries a loadable 1x1 RGB payload (3 bytes).
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
        // File, temporary-file, and shared-memory queries load a real 1x1
        // RGB payload (3 bytes) and are acknowledged like a transmit.
        let file_path = unique_temp_path("query-file");
        std::fs::write(&file_path, [1, 2, 3]).unwrap();
        let file_name = encode_base64_for_test(file_path.to_str().unwrap().as_bytes());
        let response = store
            .apply_kitty_command_with_context(
                b"a=q,i=32,t=f,s=1,v=1,f=24",
                &file_name,
                (0, 0),
                (10, 20),
            )
            .unwrap()
            .unwrap();
        assert_eq!(response, b"\x1b_Gi=32;OK\x1b\\".to_vec());

        // `t=t` reads the temp file and deletes it (marker name), like Kitty.
        let temp_path = unique_temp_path("query-tty-graphics-protocol-file");
        std::fs::write(&temp_path, [4, 5, 6]).unwrap();
        let temp_name = encode_base64_for_test(temp_path.to_str().unwrap().as_bytes());
        let response = store
            .apply_kitty_command_with_context(
                b"a=q,i=33,t=t,s=1,v=1,f=24",
                &temp_name,
                (0, 0),
                (10, 20),
            )
            .unwrap()
            .unwrap();
        assert_eq!(response, b"\x1b_Gi=33;OK\x1b\\".to_vec());
        assert!(
            !temp_path.exists(),
            "t=t query should delete the temp file after reading"
        );
        let _ = std::fs::remove_file(&file_path);

        #[cfg(unix)]
        {
            use std::ffi::CString;
            let name = format!("/cmdash-query-shm-{}", unique_temp_suffix());
            let cname = CString::new(name.as_str()).unwrap();
            let pixels = [7, 8, 9];
            unsafe {
                let fd = libc::shm_open(
                    cname.as_ptr(),
                    libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
                    0o600,
                );
                assert!(fd >= 0, "shm_open failed");
                assert_eq!(libc::ftruncate(fd, pixels.len() as libc::off_t), 0);
                let written =
                    libc::write(fd, pixels.as_ptr() as *const libc::c_void, pixels.len());
                assert_eq!(written, pixels.len() as libc::ssize_t);
                libc::close(fd);
            }
            let shm_name = encode_base64_for_test(name.as_bytes());
            let response = store
                .apply_kitty_command_with_context(
                    b"a=q,i=34,t=s,s=1,v=1,f=24",
                    &shm_name,
                    (0, 0),
                    (10, 20),
                )
                .unwrap()
                .unwrap();
            assert_eq!(response, b"\x1b_Gi=34;OK\x1b\\".to_vec());
        }
        // Queries never retain the image.
        assert_eq!(store.resource_count(), 0);
    }

    #[test]
    fn query_without_an_image_id_emits_no_response() {
        let mut store = SessionGraphicsStore::new(SessionId::new(8));
        // Kitty logs "Query graphics command without image id" and emits no
        // response at all when a query lacks the `i=` key.
        assert!(store
            .apply_kitty_command_with_context(b"a=q,t=d,s=1,v=1,f=24", b"MTIz", (0, 0), (0, 0))
            .unwrap()
            .is_none());
        assert!(store
            .apply_kitty_command_with_context(b"a=q,i=0,t=d,s=1,v=1,f=24", b"MTIz", (0, 0), (0, 0))
            .unwrap()
            .is_none());
        assert_eq!(store.resource_count(), 0);
    }

    #[test]
    fn query_validates_the_payload_before_replying_ok() {
        let mut store = SessionGraphicsStore::new(SessionId::new(9));
        // A valid 1x1 RGB payload loads and replies OK.
        assert!(store
            .apply_kitty_command_with_context(b"a=q,i=9,t=d,s=1,v=1,f=24", b"MTIz", (0, 0), (0, 0))
            .unwrap()
            .is_some());
        // Data size must match bpp * s * v: 2 bytes for a 1x1 RGB is invalid.
        assert_eq!(
            store
                .apply_kitty_command_with_context(b"a=q,i=10,t=d,s=1,v=1,f=24", b"MTI=", (0, 0), (0, 0))
                .unwrap_err(),
            GraphicsError::InvalidPayload
        );
        // Raw query payloads require explicit s/v dimensions.
        assert!(store
            .apply_kitty_command_with_context(b"a=q,i=11,t=d,f=24", b"MTIz", (0, 0), (0, 0))
            .is_err());
        // Unsupported formats are rejected.
        assert!(store
            .apply_kitty_command_with_context(b"a=q,i=12,t=d,s=1,v=1,f=7", b"MTIz", (0, 0), (0, 0))
            .is_err());
        // f=100 payloads must carry a parseable GIF/PNG header.
        assert!(store
            .apply_kitty_command_with_context(b"a=q,i=13,t=d,f=100", b"bm90IGEgaW1hZ2U=", (0, 0), (0, 0))
            .is_err());
        // Nothing was retained by any of the queries.
        assert_eq!(store.resource_count(), 0);
    }

    #[test]
    fn query_failing_to_load_reports_the_transfer_error() {
        let mut store = SessionGraphicsStore::new(SessionId::new(10));
        // A file query pointing at a missing path fails to load and returns
        // the transfer error instead of OK.
        let missing = encode_base64_for_test(b"/no/such/cmdash-query-file");
        assert!(store
            .apply_kitty_command_with_context(b"a=q,i=10,t=f,s=1,v=1,f=24", &missing, (0, 0), (0, 0))
            .is_err());
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

    fn unique_temp_suffix() -> u64 {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let pid = u64::from(std::process::id());
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        (pid << 32) ^ count
    }

    fn unique_temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("cmdash-{tag}-{}", unique_temp_suffix()))
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
    fn placements_advance_the_cursor_unless_the_client_requests_a_static_cursor() {
        let mut store = SessionGraphicsStore::new(SessionId::new(30));
        assert_eq!(store.take_last_cursor_advance(), None);

        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=30,c=2,r=1,q=2",
                b"AQID",
                (0, 0),
                (10, 20),
            )
            .unwrap();
        assert_eq!(store.take_last_cursor_advance(), Some((2, 1)));
        assert_eq!(store.take_last_cursor_advance(), None);

        store
            .apply_kitty_command_with_context(b"a=p,i=30,c=3,r=2,C=1,q=2", b"", (0, 0), (10, 20))
            .unwrap();
        assert_eq!(store.take_last_cursor_advance(), None);
    }

    #[test]
    fn lowercase_transmit_stores_the_image_without_creating_a_placement() {
        let mut store = SessionGraphicsStore::new(SessionId::new(33));
        store
            .apply_kitty_command(b"a=t,f=24,i=33,s=2,v=1,q=2", b"AQID")
            .unwrap();
        assert_eq!(store.resource_count(), 1);
        assert_eq!(store.placement_count(), 0);
        assert_eq!(store.take_last_cursor_advance(), None);

        store
            .apply_kitty_command_with_context(b"a=p,i=33,c=2,r=1,q=2", b"", (0, 0), (10, 20))
            .unwrap();
        assert_eq!(store.placement_count(), 1);
        assert_eq!(store.take_last_cursor_advance(), Some((2, 1)));
    }

    #[test]
    fn placement_dimensions_derive_the_missing_extent_from_the_aspect_ratio() {
        let mut store = SessionGraphicsStore::new(SessionId::new(31));
        // A 20x10 pixel source in 10x10 cells: four explicit columns must
        // yield two rows instead of the natural one row, preserving aspect.
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=31,s=20,v=10,c=4,q=2",
                b"AQID",
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let submissions = store.visible_submissions(Rect::new(0, 0, 20, 20));
        assert_eq!(submissions[0].placement().area(), Rect::new(0, 0, 4, 2));

        // Two explicit rows must yield four columns for the same source.
        let mut rows_given = SessionGraphicsStore::new(SessionId::new(32));
        rows_given
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=32,s=20,v=10,r=2,q=2",
                b"AQID",
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let submissions = rows_given.visible_submissions(Rect::new(0, 0, 20, 20));
        assert_eq!(submissions[0].placement().area(), Rect::new(0, 0, 4, 2));
    }

    #[test]
    fn sub_cell_offsets_are_retained_and_shift_the_natural_extent() {
        let mut store = SessionGraphicsStore::new(SessionId::new(34));
        // A 20x10 pixel image in 10x10 cells with an X=5 sub-cell offset:
        // the offset is retained, and the natural width becomes
        // ceil((20 + 5) / 10) == 3 cells instead of 2.
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=34,s=20,v=10,X=5,Y=3,q=2",
                b"AQID",
                (2, 1),
                (10, 10),
            )
            .unwrap();
        let submissions = store.visible_submissions(Rect::new(0, 0, 20, 20));
        let placement = submissions[0].placement();
        assert_eq!(placement.cell_x_offset(), 5);
        assert_eq!(placement.cell_y_offset(), 3);
        assert_eq!(placement.area(), Rect::new(2, 1, 3, 2));
    }

    #[test]
    fn clipped_placements_carry_the_visible_source_crop() {
        let mut store = SessionGraphicsStore::new(SessionId::new(35));
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=35,s=100,v=100,c=10,r=10,q=2",
                b"AQID",
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let submission = store
            .visible_submissions(Rect::new(0, 0, 20, 20))
            .into_iter()
            .next()
            .unwrap();

        // Clipping to the right half selects the right half of the image.
        let clipped = submission.clipped_to(Rect::new(5, 0, 5, 10)).unwrap();
        assert_eq!(clipped.placement().area(), Rect::new(5, 0, 5, 10));
        assert_eq!(
            clipped.placement().source(),
            Some(GraphicsSourceRect::new(50, 0, 50, 100))
        );

        // A clip that covers the whole placement leaves the source unchanged.
        assert_eq!(
            submission
                .clipped_to(Rect::new(0, 0, 10, 10))
                .unwrap()
                .placement()
                .source(),
            None
        );

        // A client-specified crop is the base for any further clipping.
        let mut cropped = SessionGraphicsStore::new(SessionId::new(36));
        cropped
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=36,s=200,v=200,c=10,r=10,x=40,y=20,w=100,h=100,q=2",
                b"AQID",
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let submission = cropped
            .visible_submissions(Rect::new(0, 0, 20, 20))
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            submission.placement().source(),
            Some(GraphicsSourceRect::new(40, 20, 100, 100))
        );
        let clipped = submission.clipped_to(Rect::new(5, 0, 5, 10)).unwrap();
        assert_eq!(
            clipped.placement().source(),
            Some(GraphicsSourceRect::new(90, 20, 50, 100))
        );
    }

    #[test]
    fn sub_cell_offsets_shift_the_horizontal_occlusion_crop_in_pixels() {
        let mut store = SessionGraphicsStore::new(SessionId::new(37));
        // A 30x10 image drawn at its natural size with an X=6 sub-cell offset
        // in 10x10 cells occupies cells 0..4 (ceil((30 + 6) / 10)). Clipping
        // past the first cell must account for the six pixels the image began
        // inside its anchor cell rather than treating the cell span as a
        // uniform whole-cell fraction of the source.
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=37,s=30,v=10,X=6,q=2",
                b"AQID",
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let submission = store
            .visible_submissions(Rect::new(0, 0, 20, 20))
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(submission.placement().area(), Rect::new(0, 0, 4, 1));

        // Cutting the left cell starts the visible image at source pixel
        // (10 - 6) = 4, not the whole-cell fraction 30 / 4 == 7.
        let clipped = submission.clipped_to(Rect::new(1, 0, 3, 1)).unwrap();
        assert_eq!(clipped.placement().area(), Rect::new(1, 0, 3, 1));
        assert_eq!(
            clipped.placement().source(),
            Some(GraphicsSourceRect::new(4, 0, 26, 10))
        );
        assert_eq!(clipped.placement().cell_x_offset(), 0);
        assert_eq!(clipped.placement().cell_y_offset(), 0);

        // Keeping the anchor cell preserves the offset and crops only the
        // trailing cells, so the re-emitted placement draws the same pixels.
        let clipped = submission.clipped_to(Rect::new(0, 0, 2, 1)).unwrap();
        assert_eq!(clipped.placement().area(), Rect::new(0, 0, 2, 1));
        assert_eq!(clipped.placement().cell_x_offset(), 6);
        assert_eq!(
            clipped.placement().source(),
            Some(GraphicsSourceRect::new(0, 0, 14, 10))
        );
    }

    #[test]
    fn sub_cell_offsets_shift_the_vertical_occlusion_crop_in_pixels() {
        let mut store = SessionGraphicsStore::new(SessionId::new(38));
        // A 10x30 image with a Y=6 offset occupies rows 0..4; clipping past
        // the first row shifts the visible source down by six pixels.
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=38,s=10,v=30,Y=6,q=2",
                b"AQID",
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let submission = store
            .visible_submissions(Rect::new(0, 0, 20, 20))
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(submission.placement().area(), Rect::new(0, 0, 1, 4));

        let clipped = submission.clipped_to(Rect::new(0, 1, 1, 3)).unwrap();
        assert_eq!(clipped.placement().area(), Rect::new(0, 1, 1, 3));
        assert_eq!(
            clipped.placement().source(),
            Some(GraphicsSourceRect::new(0, 4, 10, 26))
        );
        assert_eq!(clipped.placement().cell_y_offset(), 0);
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
    fn quiet_response_rule_matches_kitty() {
        // Kitty's `finish_command_response`: `q=1` suppresses success (`OK`)
        // responses and any `q >= 2` suppresses every response.
        assert!(should_emit_response(0, true));
        assert!(should_emit_response(0, false));
        assert!(!should_emit_response(1, true));
        assert!(should_emit_response(1, false));
        assert!(!should_emit_response(2, true));
        assert!(!should_emit_response(2, false));
    }

    #[test]
    fn quiet_key_suppresses_success_and_query_responses() {
        let mut store = SessionGraphicsStore::new(SessionId::new(82));
        // q=0 (default) emits an OK acknowledgement for a successful upload.
        let response = store
            .apply_kitty_command_with_context(b"a=T,f=24,i=1,q=0", b"AQID", (0, 0), (0, 0))
            .unwrap()
            .expect("default quiet must emit an OK response");
        assert!(String::from_utf8_lossy(&response).contains("OK"));

        // q=1 and q=2 both suppress the success response.
        assert!(store
            .apply_kitty_command_with_context(b"a=T,f=24,i=2,q=1", b"BAUG", (0, 0), (0, 0))
            .unwrap()
            .is_none());
        assert!(store
            .apply_kitty_command_with_context(b"a=T,f=24,i=3,q=2", b"CAUI", (0, 0), (0, 0))
            .unwrap()
            .is_none());

        // Query responses follow the same rule.
        let query_ok = store
            .apply_kitty_command_with_context(
                b"a=q,i=1,t=d,s=1,v=1,f=24,q=0",
                b"MTIz",
                (0, 0),
                (0, 0),
            )
            .unwrap()
            .expect("q=0 query must emit a response");
        assert!(String::from_utf8_lossy(&query_ok).contains("OK"));
        assert!(store
            .apply_kitty_command_with_context(
                b"a=q,i=1,t=d,s=1,v=1,f=24,q=1",
                b"MTIz",
                (0, 0),
                (0, 0),
            )
            .unwrap()
            .is_none());
        assert!(store
            .apply_kitty_command_with_context(
                b"a=q,i=1,t=d,s=1,v=1,f=24,q=2",
                b"MTIz",
                (0, 0),
                (0, 0),
            )
            .unwrap()
            .is_none());
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
    fn transient_images_are_evicted_before_retained_ones_under_pressure() {
        let limits = GraphicsLimits {
            max_decoded_bytes: 8,
            max_resources: 8,
            max_placements: 8,
        };
        let mut store = SessionGraphicsStore::with_limits(SessionId::new(70), limits);
        // A retained image (no N hint) and a transient image (N=1), both
        // referenced by a placement.
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=1,c=1,r=1,q=2", b"AQID", (0, 0), (0, 0))
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=2,N=1,c=1,r=1,q=2", b"BAUG", (1, 0), (0, 0))
            .unwrap();
        assert_eq!(store.resource_count(), 2);
        assert_eq!(store.decoded_bytes(2), Some(&[4u8, 5, 6][..]));

        // The third upload overflows the 8-byte budget; the transient image 2
        // is evicted before the retained image 1.
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=3,c=1,r=1,q=2", b"CAUI", (2, 0), (0, 0))
            .unwrap();
        assert_eq!(store.resource_count(), 2);
        assert_eq!(store.decoded_bytes(1), Some(&[1u8, 2, 3][..]));
        assert_eq!(store.decoded_bytes(2), None);
        assert!(store.decoded_bytes(3).is_some());
        assert!(store
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message().contains("evicted")));
    }

    #[test]
    fn compose_propagates_source_transient_onto_a_retained_root() {
        let mut store = SessionGraphicsStore::new(SessionId::new(801));
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=801,s=1,v=1,q=2",
                &encode_base64_for_test(&[1, 1, 1, 255]),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        // Frame 2 is transient (N=1).
        store
            .apply_kitty_command_with_context(
                b"a=f,i=801,r=2,N=1,q=2",
                &encode_base64_for_test(&[9, 9, 9, 255]),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        assert_eq!(store.animation_frame_transient(801, 1), Some(false));
        assert_eq!(store.animation_frame_transient(801, 2), Some(true));

        // Composing a transient frame onto a retained root marks the root
        // transient (Kitty's `dest_frame->transient = src || dest`).
        store
            .apply_kitty_command(b"a=c,i=801,r=2,c=1,C=1,q=2", b"")
            .unwrap();
        assert_eq!(store.animation_frame_transient(801, 1), Some(true));
    }

    #[test]
    fn compose_propagates_root_transient_onto_a_retained_frame() {
        let mut store = SessionGraphicsStore::new(SessionId::new(802));
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=802,s=1,v=1,N=1,q=2",
                &encode_base64_for_test(&[1, 1, 1, 255]),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        store
            .apply_kitty_command_with_context(
                b"a=f,i=802,r=2,q=2",
                &encode_base64_for_test(&[9, 9, 9, 255]),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        assert_eq!(store.animation_frame_transient(802, 1), Some(true));
        assert_eq!(store.animation_frame_transient(802, 2), Some(false));

        // Composing the transient root onto a retained extra frame marks the
        // frame transient.
        store
            .apply_kitty_command(b"a=c,i=802,r=1,c=2,C=1,q=2", b"")
            .unwrap();
        assert_eq!(store.animation_frame_transient(802, 2), Some(true));
    }

    #[test]
    fn frame_delta_inherits_its_base_frames_transient_hint() {
        let mut store = SessionGraphicsStore::new(SessionId::new(803));
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=803,s=1,v=1,N=1,q=2",
                &encode_base64_for_test(&[1, 1, 1, 255]),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        // A new delta composed onto the transient root inherits its transient
        // status even though this frame carries no N hint of its own.
        store
            .apply_kitty_command_with_context(
                b"a=f,i=803,r=2,c=1,q=2",
                &encode_base64_for_test(&[9, 9, 9, 255]),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        assert_eq!(store.animation_frame_transient(803, 2), Some(true));
    }

    #[test]
    fn unreferenced_images_are_evicted_first_under_pressure() {
        let limits = GraphicsLimits {
            max_decoded_bytes: 6,
            max_resources: 8,
            max_placements: 8,
        };
        let mut store = SessionGraphicsStore::with_limits(SessionId::new(71), limits);
        // An unreferenced retained image (transmit-only) and a referenced one.
        store.apply_kitty_command(b"a=t,f=24,i=1,q=2", b"AQID").unwrap();
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=2,c=1,r=1,q=2", b"BAUG", (0, 0), (0, 0))
            .unwrap();
        assert_eq!(store.resource_count(), 2);

        // The third upload overflows; the unreferenced image 1 is evicted
        // first (even though retained), keeping the referenced image 2.
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=3,c=1,r=1,q=2", b"CAUI", (1, 0), (0, 0))
            .unwrap();
        assert_eq!(store.decoded_bytes(1), None);
        assert_eq!(store.decoded_bytes(2), Some(&[4u8, 5, 6][..]));
        assert!(store.decoded_bytes(3).is_some());
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
    fn compose_overwrites_a_source_rectangle_into_the_destination_frame() {
        let mut store = SessionGraphicsStore::new(SessionId::new(60));
        // A 2x2 RGBA root frame.
        let root = [
            1, 1, 1, 255, 2, 2, 2, 255,
            3, 3, 3, 255, 4, 4, 4, 255,
        ];
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=60,s=2,v=2,q=2",
                &encode_base64_for_test(&root),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        // Frame 2 has distinct pixels.
        let frame2 = [
            9, 9, 9, 255, 8, 8, 8, 255,
            7, 7, 7, 255, 6, 6, 6, 255,
        ];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=60,r=2,q=2",
                &encode_base64_for_test(&frame2),
                (0, 0),
                (10, 10),
            )
            .unwrap();

        // Compose frame 2's pixel (1,0) onto root pixel (0,1), overwriting it.
        store
            .apply_kitty_command(b"a=c,i=60,r=2,c=1,X=1,Y=0,x=0,y=1,w=1,h=1,C=1,q=2", b"")
            .unwrap();

        let decoded = store.decoded_bytes(60).unwrap();
        assert_eq!(&decoded[0..4], &[1, 1, 1, 255]);
        assert_eq!(&decoded[4..8], &[2, 2, 2, 255]);
        assert_eq!(&decoded[8..12], &[8, 8, 8, 255]);
        assert_eq!(&decoded[12..16], &[4, 4, 4, 255]);
    }

    #[test]
    fn compose_alpha_blends_onto_the_destination_frame() {
        let mut store = SessionGraphicsStore::new(SessionId::new(61));
        // A 1x1 RGBA root frame: opaque blue.
        let root = [0, 0, 255, 255];
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=61,s=1,v=1,q=2",
                &encode_base64_for_test(&root),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        // Frame 2: half-transparent red.
        let frame2 = [255, 0, 0, 128];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=61,r=2,q=2",
                &encode_base64_for_test(&frame2),
                (0, 0),
                (10, 10),
            )
            .unwrap();

        // The default C=0 alpha-blends frame 2 over the root frame.
        store
            .apply_kitty_command(b"a=c,i=61,r=2,c=1,q=2", b"")
            .unwrap();

        assert_eq!(store.decoded_bytes(61).unwrap(), &[128, 0, 127, 255][..]);
    }

    #[test]
    fn compose_into_a_non_root_frame_updates_only_that_frame() {
        let mut store = SessionGraphicsStore::new(SessionId::new(64));
        let root = [
            1, 1, 1, 255, 2, 2, 2, 255,
            3, 3, 3, 255, 4, 4, 4, 255,
        ];
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=64,s=2,v=2,q=2",
                &encode_base64_for_test(&root),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let frame2 = [
            9, 9, 9, 255, 8, 8, 8, 255,
            7, 7, 7, 255, 6, 6, 6, 255,
        ];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=64,r=2,q=2",
                &encode_base64_for_test(&frame2),
                (0, 0),
                (10, 10),
            )
            .unwrap();

        // Copy root pixel (1,1) into frame 2's pixel (0,0); the root frame is
        // untouched and only frame 2 changes.
        store
            .apply_kitty_command(b"a=c,i=64,r=1,c=2,X=1,Y=1,x=0,y=0,w=1,h=1,C=1,q=2", b"")
            .unwrap();

        assert_eq!(
            store.animation_frame_bytes(64, 2),
            Some(&[4, 4, 4, 255, 8, 8, 8, 255, 7, 7, 7, 255, 6, 6, 6, 255][..])
        );
        assert_eq!(store.decoded_bytes(64).unwrap(), &root[..]);
    }

    #[test]
    fn compose_rejects_missing_frames_out_of_bounds_and_overlapping_rectangles() {
        let mut store = SessionGraphicsStore::new(SessionId::new(62));
        let root = [
            1, 1, 1, 255, 2, 2, 2, 255,
            3, 3, 3, 255, 4, 4, 4, 255,
        ];
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=62,s=2,v=2,q=2",
                &encode_base64_for_test(&root),
                (0, 0),
                (10, 10),
            )
            .unwrap();

        // A missing source frame is ENOENT.
        assert_eq!(
            store.apply_kitty_command(b"a=c,i=62,r=9,c=1", b""),
            Err(GraphicsError::ImageNotFound(62))
        );
        // A rectangle that extends past the image edge is EINVAL.
        assert_eq!(
            store.apply_kitty_command(b"a=c,i=62,r=1,c=1,X=1,Y=0,x=0,y=0,w=2,h=1", b""),
            Err(GraphicsError::InvalidParameter(
                "animation frame composition".to_owned()
            ))
        );
        // Composing a frame onto itself with overlapping rectangles is EINVAL.
        assert_eq!(
            store.apply_kitty_command(b"a=c,i=62,r=1,c=1,X=0,Y=0,x=0,y=0,w=2,h=2", b""),
            Err(GraphicsError::InvalidParameter(
                "animation frame composition".to_owned()
            ))
        );
    }

    #[test]
    fn animation_control_sets_the_loop_count() {
        let mut store = SessionGraphicsStore::new(SessionId::new(63));
        store
            .apply_kitty_command(b"a=T,f=24,i=63,c=1,r=1,q=2", b"AQID")
            .unwrap();
        assert_eq!(store.animation_loops(63), Some(0));
        store.apply_kitty_command(b"a=a,i=63,v=3", b"").unwrap();
        assert_eq!(store.animation_loops(63), Some(3));
    }

    #[test]
    fn frame_composition_composes_a_delta_onto_its_base_frame() {
        let mut store = SessionGraphicsStore::new(SessionId::new(70));
        let red = [
            255, 0, 0, 255, 255, 0, 0, 255,
            255, 0, 0, 255, 255, 0, 0, 255,
        ];
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=70,s=2,v=2,q=2",
                &encode_base64_for_test(&red),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let green = [
            0, 255, 0, 255, 0, 255, 0, 255,
            0, 255, 0, 255, 0, 255, 0, 255,
        ];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=70,r=2,q=2",
                &encode_base64_for_test(&green),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        // A 1x1 delta composed onto frame 2 at (1,1).
        let blue = [0, 0, 255, 255];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=70,r=3,c=2,x=1,y=1,s=1,v=1,q=2",
                &encode_base64_for_test(&blue),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        assert_eq!(
            store.coalesced_frame_bytes(70, 3).unwrap(),
            vec![
                0, 255, 0, 255, 0, 255, 0, 255,
                0, 255, 0, 255, 0, 0, 255, 255,
            ]
        );
        // The stored delta keeps only the 1-pixel rectangle, not the whole
        // coalesced image.
        assert_eq!(store.animation_frame_bytes(70, 3), Some(&blue[..]));
    }

    #[test]
    fn frame_composition_fills_a_background_canvas_for_partial_frames() {
        let mut store = SessionGraphicsStore::new(SessionId::new(71));
        let root = [0u8; 16];
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=71,s=2,v=2,q=2",
                &encode_base64_for_test(&root),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        // `Y` fills the 2x2 canvas with 0x11223344; the two-pixel strip lands
        // in column x=1.
        let strip = [
            255, 255, 255, 255, // (1,0)
            0, 0, 0, 255, // (1,1)
        ];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=71,r=2,Y=287454020,x=1,y=0,s=1,v=2,q=2",
                &encode_base64_for_test(&strip),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        assert_eq!(
            store.coalesced_frame_bytes(71, 2).unwrap(),
            vec![
                0x11, 0x22, 0x33, 0x44, 255, 255, 255, 255,
                0x11, 0x22, 0x33, 0x44, 0, 0, 0, 255,
            ]
        );
    }

    #[test]
    fn frame_edit_coalesces_the_existing_frame_into_a_keyframe() {
        let mut store = SessionGraphicsStore::new(SessionId::new(72));
        let red = [
            255, 0, 0, 255, 255, 0, 0, 255,
            255, 0, 0, 255, 255, 0, 0, 255,
        ];
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=72,s=2,v=2,q=2",
                &encode_base64_for_test(&red),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let green = [
            0, 255, 0, 255, 0, 255, 0, 255,
            0, 255, 0, 255, 0, 255, 0, 255,
        ];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=72,r=2,q=2",
                &encode_base64_for_test(&green),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        // Editing the existing frame 2 coalesces its green pixels, overwrites
        // the top-left pixel with blue, and stores a full keyframe.
        let blue = [0, 0, 255, 255];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=72,r=2,x=0,y=0,s=1,v=1,X=1,q=2",
                &encode_base64_for_test(&blue),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let expected = vec![
            0, 0, 255, 255, 0, 255, 0, 255,
            0, 255, 0, 255, 0, 255, 0, 255,
        ];
        assert_eq!(store.coalesced_frame_bytes(72, 2).unwrap(), expected);
        // The edit collapsed the delta into a full keyframe.
        assert_eq!(
            store.animation_frame_bytes(72, 2),
            Some(
                &[
                    0, 0, 255, 255, 0, 255, 0, 255,
                    0, 255, 0, 255, 0, 255, 0, 255,
                ][..]
            )
        );
    }

    #[test]
    fn frame_composition_blends_and_replaces_based_on_the_x_key() {
        let mut store = SessionGraphicsStore::new(SessionId::new(73));
        let root = [0, 0, 255, 255]; // opaque blue
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=73,s=1,v=1,q=2",
                &encode_base64_for_test(&root),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let half_transparent_red = [255, 0, 0, 128];
        // Default `X=0` alpha-blends onto the base frame.
        store
            .apply_kitty_command_with_context(
                b"a=f,i=73,r=2,c=1,s=1,v=1,q=2",
                &encode_base64_for_test(&half_transparent_red),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        assert_eq!(
            store.coalesced_frame_bytes(73, 2).unwrap(),
            vec![128, 0, 127, 255]
        );
        // `X=1` overwrites instead of blending.
        store
            .apply_kitty_command_with_context(
                b"a=f,i=73,r=3,c=1,s=1,v=1,X=1,q=2",
                &encode_base64_for_test(&half_transparent_red),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        assert_eq!(
            store.coalesced_frame_bytes(73, 3).unwrap(),
            vec![255, 0, 0, 128]
        );
    }

    #[test]
    fn compose_reads_a_delta_frame_source_through_coalescing() {
        let mut store = SessionGraphicsStore::new(SessionId::new(74));
        let root = [0u8; 16]; // 2x2 transparent
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=74,s=2,v=2,q=2",
                &encode_base64_for_test(&root),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let blue = [0, 0, 255, 255];
        // Frame 2 is a 1x1 delta composed onto the root at (1,1).
        store
            .apply_kitty_command_with_context(
                b"a=f,i=74,r=2,c=1,x=1,y=1,s=1,v=1,X=1,q=2",
                &encode_base64_for_test(&blue),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        // `a=c` copies frame 2's rendered (1,1) pixel into the root's (0,0),
        // so the delta source must be coalesced before reading.
        store
            .apply_kitty_command(
                b"a=c,i=74,r=2,c=1,X=1,Y=1,x=0,y=0,w=1,h=1,C=1,q=2",
                b"",
            )
            .unwrap();
        let decoded = store.decoded_bytes(74).unwrap();
        assert_eq!(&decoded[0..4], &[0, 0, 255, 255]);
    }

    /// Encodes an RGBA image as a PNG for the non-raw composition tests.
    fn png_fixture(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut output, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(rgba).unwrap();
            writer.finish().unwrap();
        }
        output
    }

    /// Encodes a single opaque 1x1 pixel as a static GIF.
    fn static_gif_fixture(rgb: [u8; 3]) -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut output, 1, 1, &rgb).unwrap();
            let frame = gif::Frame {
                width: 1,
                height: 1,
                buffer: std::borrow::Cow::Owned(vec![0]),
                ..gif::Frame::default()
            };
            encoder.write_frame(&frame).unwrap();
        }
        output
    }

    #[test]
    fn compose_decodes_a_png_root_frame_into_the_destination() {
        let mut store = SessionGraphicsStore::new(SessionId::new(76));
        let red = [
            255, 0, 0, 255, 255, 0, 0, 255,
            255, 0, 0, 255, 255, 0, 0, 255,
        ];
        let png = png_fixture(2, 2, &red);
        store
            .apply_kitty_command_with_context(
                b"a=T,f=100,i=76,q=2",
                &encode_base64_for_test(&png),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let green = [
            0, 255, 0, 255, 0, 255, 0, 255,
            0, 255, 0, 255, 0, 255, 0, 255,
        ];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=76,r=2,q=2",
                &encode_base64_for_test(&green),
                (0, 0),
                (10, 10),
            )
            .unwrap();

        // Compose the PNG root over frame 2 (overwriting it); the root must be
        // decoded to RGBA before composing.
        store
            .apply_kitty_command(
                b"a=c,i=76,r=1,c=2,X=0,Y=0,x=0,y=0,w=2,h=2,C=1,q=2",
                b"",
            )
            .unwrap();

        assert_eq!(store.animation_frame_bytes(76, 2), Some(&red[..]));
    }

    #[test]
    fn compose_into_a_png_root_decodes_it_and_converts_to_rgba() {
        let mut store = SessionGraphicsStore::new(SessionId::new(77));
        let red = [
            255, 0, 0, 255, 255, 0, 0, 255,
            255, 0, 0, 255, 255, 0, 0, 255,
        ];
        let png = png_fixture(2, 2, &red);
        store
            .apply_kitty_command_with_context(
                b"a=T,f=100,i=77,q=2",
                &encode_base64_for_test(&png),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let green = [
            0, 255, 0, 255, 0, 255, 0, 255,
            0, 255, 0, 255, 0, 255, 0, 255,
        ];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=77,r=2,q=2",
                &encode_base64_for_test(&green),
                (0, 0),
                (10, 10),
            )
            .unwrap();

        // Compose frame 2 over the PNG root; the root is decoded to RGBA and
        // the resource's wire format becomes 32.
        store
            .apply_kitty_command(
                b"a=c,i=77,r=2,c=1,X=0,Y=0,x=0,y=0,w=2,h=2,C=1,q=2",
                b"",
            )
            .unwrap();

        assert_eq!(store.decoded_bytes(77).unwrap(), &green[..]);
        let submissions = store.visible_submissions(Rect::new(0, 0, 4, 2));
        assert_eq!(submissions[0].format(), 32);
    }

    #[test]
    fn compose_decodes_a_gif_root_frame_into_the_destination() {
        let mut store = SessionGraphicsStore::new(SessionId::new(78));
        let gif = static_gif_fixture([255, 0, 0]);
        store
            .apply_kitty_command_with_context(
                b"a=T,f=100,i=78,q=2",
                &encode_base64_for_test(&gif),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let green = [0, 255, 0, 255];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=78,r=2,q=2",
                &encode_base64_for_test(&green),
                (0, 0),
                (10, 10),
            )
            .unwrap();

        // Compose the GIF root over frame 2, decoding it to RGBA.
        store
            .apply_kitty_command(
                b"a=c,i=78,r=1,c=2,X=0,Y=0,x=0,y=0,w=1,h=1,C=1,q=2",
                b"",
            )
            .unwrap();

        assert_eq!(
            store.animation_frame_bytes(78, 2),
            Some(&[255, 0, 0, 255][..])
        );
    }

    #[test]
    fn frame_composition_rejects_missing_base_and_out_of_bounds_rectangles() {
        let mut store = SessionGraphicsStore::new(SessionId::new(75));
        let root = [0u8; 16];
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=75,s=2,v=2,q=2",
                &encode_base64_for_test(&root),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        // `c` referencing a nonexistent base frame.
        assert_eq!(
            store.apply_kitty_command(b"a=f,i=75,r=2,c=9,s=1,v=1", b"AAAA"),
            Err(GraphicsError::InvalidParameter(
                "animation frame".to_owned()
            ))
        );
        // A partial rectangle that extends past the image edge.
        assert_eq!(
            store.apply_kitty_command(b"a=f,i=75,r=2,x=2,y=0,s=1,v=1", b"AAAA"),
            Err(GraphicsError::InvalidParameter(
                "animation frame composition".to_owned()
            ))
        );
    }

    /// Builds a two-frame 2x1 animated GIF for auto-animation tests: frame 1
    /// is red+green with no delay, frame 2 is blue+green with a 100 ms delay
    /// (both opaque, covering the full canvas).
    fn animated_gif_for_test() -> Vec<u8> {
        let palette = [255, 0, 0, 0, 255, 0, 0, 0, 255]; // red, green, blue
        let mut output = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut output, 2, 1, &palette).unwrap();
            encoder.set_repeat(gif::Repeat::Finite(3)).unwrap();
            let first = gif::Frame {
                width: 2,
                height: 1,
                buffer: std::borrow::Cow::Owned(vec![0, 1]),
                ..gif::Frame::default()
            };
            encoder.write_frame(&first).unwrap();
            let second = gif::Frame {
                delay: 10, // 100 ms
                width: 2,
                height: 1,
                buffer: std::borrow::Cow::Owned(vec![2, 1]),
                ..gif::Frame::default()
            };
            encoder.write_frame(&second).unwrap();
        }
        output
    }

    #[test]
    fn gif_repeat_maps_to_kitty_animation_loops() {
        assert_eq!(gif_repeat_to_animation_loops(gif::Repeat::Infinite), 1);
        assert_eq!(gif_repeat_to_animation_loops(gif::Repeat::Finite(0)), 2);
        assert_eq!(gif_repeat_to_animation_loops(gif::Repeat::Finite(1)), 3);
        assert_eq!(gif_repeat_to_animation_loops(gif::Repeat::Finite(3)), 5);
    }

    #[test]
    fn animated_gif_payload_auto_extracts_coalesced_rgba_frames() {
        let gif = animated_gif_for_test();
        let mut store = SessionGraphicsStore::new(SessionId::new(90));
        store
            .apply_kitty_command_with_context(
                b"a=T,f=100,i=90,q=2",
                &encode_base64_for_test(&gif),
                (0, 0),
                (10, 10),
            )
            .unwrap();

        assert_eq!(store.animation_frame_count(90), Some(1));
        assert_eq!(store.animation_state(90), Some(GraphicsAnimationState::Running));
        // GIF `Finite(3)` -> `v = 5` (`max_loops = 4`).
        assert_eq!(store.animation_loops(90), Some(5));

        // The root frame is the first GIF frame coalesced to full-canvas RGBA.
        assert_eq!(
            store.decoded_bytes(90),
            Some(&[255, 0, 0, 255, 0, 255, 0, 255][..])
        );
        // The extra frame is the second GIF frame's coalesced RGBA.
        assert_eq!(
            store.animation_frame_bytes(90, 2),
            Some(&[0, 0, 255, 255, 0, 255, 0, 255][..])
        );

        let submissions = store.visible_submissions(Rect::new(0, 0, 4, 2));
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].format(), 32);
        assert_eq!(submissions[0].pixel_width(), 2);
        assert_eq!(submissions[0].pixel_height(), 1);
    }

    #[test]
    fn animated_gif_playback_advances_and_serves_the_next_frame() {
        let gif = animated_gif_for_test();
        let mut store = SessionGraphicsStore::new(SessionId::new(91));
        store
            .apply_kitty_command_with_context(
                b"a=T,f=100,i=91,q=2",
                &encode_base64_for_test(&gif),
                (0, 0),
                (10, 10),
            )
            .unwrap();

        // The gapless root frame is skipped, landing on frame 2 (100 ms gap).
        let t0 = Instant::now();
        assert_eq!(
            store.advance_animations(t0),
            Some(Duration::from_millis(100))
        );
        assert_eq!(store.animation_current_frame(91), Some(2));
        let submissions = store.visible_submissions(Rect::new(0, 0, 4, 2));
        assert_eq!(
            submissions[0].encoded_payload(),
            encode_base64_payload(&[0, 0, 255, 255, 0, 255, 0, 255]).as_slice()
        );
    }

    #[test]
    fn static_gif_payload_stays_a_static_format_100_image() {
        let palette = [255, 0, 0]; // red
        let mut output = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut output, 1, 1, &palette).unwrap();
            let frame = gif::Frame {
                width: 1,
                height: 1,
                buffer: std::borrow::Cow::Owned(vec![0]),
                ..gif::Frame::default()
            };
            encoder.write_frame(&frame).unwrap();
        }
        let mut store = SessionGraphicsStore::new(SessionId::new(92));
        store
            .apply_kitty_command_with_context(
                b"a=T,f=100,i=92,q=2",
                &encode_base64_for_test(&output),
                (0, 0),
                (10, 10),
            )
            .unwrap();

        assert_eq!(store.animation_frame_count(92), Some(0));
        assert_eq!(store.animation_state(92), Some(GraphicsAnimationState::Stopped));
        let submissions = store.visible_submissions(Rect::new(0, 0, 2, 1));
        assert_eq!(submissions[0].format(), 100);
        assert_eq!(submissions[0].encoded_payload(), encode_base64_for_test(&output));
    }

    #[test]
    fn animation_advances_frames_and_serves_the_coalesced_current_frame() {
        let mut store = SessionGraphicsStore::new(SessionId::new(80));
        let black = [0, 0, 0, 255];
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=80,s=1,v=1,q=2",
                &encode_base64_for_test(&black),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let red = [255, 0, 0, 255];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=80,r=2,s=1,v=1,z=100,q=2",
                &encode_base64_for_test(&red),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let green = [0, 255, 0, 255];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=80,r=3,s=1,v=1,z=100,q=2",
                &encode_base64_for_test(&green),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        store.apply_kitty_command(b"a=a,i=80,s=3,q=2", b"").unwrap();

        let t0 = Instant::now();
        // The gapless root frame is skipped, landing on frame 2 (100 ms gap).
        assert_eq!(
            store.advance_animations(t0),
            Some(Duration::from_millis(100))
        );
        assert_eq!(store.animation_current_frame(80), Some(2));
        assert_eq!(store.animation_revision(80), Some(1));

        let submissions = store.visible_submissions(Rect::new(0, 0, 1, 1));
        assert_eq!(submissions.len(), 1);
        assert_eq!(
            submissions[0].encoded_payload(),
            encode_base64_payload(&red).as_slice()
        );

        // Past frame 2's deadline the animation advances to frame 3.
        assert_eq!(
            store.advance_animations(t0 + Duration::from_millis(150)),
            Some(Duration::from_millis(50))
        );
        assert_eq!(store.animation_current_frame(80), Some(3));
        let submissions = store.visible_submissions(Rect::new(0, 0, 1, 1));
        assert_eq!(
            submissions[0].encoded_payload(),
            encode_base64_payload(&green).as_slice()
        );
    }

    #[test]
    fn animation_stops_after_the_loop_limit() {
        let mut store = SessionGraphicsStore::new(SessionId::new(81));
        let black = [0, 0, 0, 255];
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=81,s=1,v=1,q=2",
                &encode_base64_for_test(&black),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let red = [255, 0, 0, 255];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=81,r=2,s=1,v=1,z=10,q=2",
                &encode_base64_for_test(&red),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let green = [0, 255, 0, 255];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=81,r=3,s=1,v=1,z=10,q=2",
                &encode_base64_for_test(&green),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        // `v=2` means `max_loops = 1`: play through once, then stop.
        store.apply_kitty_command(b"a=a,i=81,s=3,v=2,q=2", b"").unwrap();

        let t0 = Instant::now();
        store.advance_animations(t0);
        assert_eq!(store.animation_current_frame(81), Some(2));
        store.advance_animations(t0 + Duration::from_millis(10));
        assert_eq!(store.animation_current_frame(81), Some(3));
        // Wrapping to the root exhausts the loop count; the last frame stays
        // displayed and nothing is scheduled further.
        assert_eq!(
            store.advance_animations(t0 + Duration::from_millis(20)),
            None
        );
        assert_eq!(store.animation_current_frame(81), Some(3));
        assert_eq!(
            store.advance_animations(t0 + Duration::from_millis(100)),
            None
        );
    }

    #[test]
    fn loading_animation_plays_once_and_stops_at_the_wrap() {
        let mut store = SessionGraphicsStore::new(SessionId::new(82));
        let black = [0, 0, 0, 255];
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=82,s=1,v=1,q=2",
                &encode_base64_for_test(&black),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let red = [255, 0, 0, 255];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=82,r=2,s=1,v=1,z=10,q=2",
                &encode_base64_for_test(&red),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let green = [0, 255, 0, 255];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=82,r=3,s=1,v=1,z=10,q=2",
                &encode_base64_for_test(&green),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        store.apply_kitty_command(b"a=a,i=82,s=2,q=2", b"").unwrap();

        let t0 = Instant::now();
        store.advance_animations(t0);
        assert_eq!(store.animation_current_frame(82), Some(2));
        store.advance_animations(t0 + Duration::from_millis(10));
        assert_eq!(store.animation_current_frame(82), Some(3));
        // A `Loading` animation stops at the wrap rather than looping.
        store.advance_animations(t0 + Duration::from_millis(20));
        assert_eq!(store.animation_current_frame(82), Some(3));
        assert_eq!(
            store.advance_animations(t0 + Duration::from_millis(100)),
            None
        );
    }

    #[test]
    fn gapless_frames_are_skipped_during_playback() {
        let mut store = SessionGraphicsStore::new(SessionId::new(83));
        let black = [0, 0, 0, 255];
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=83,s=1,v=1,q=2",
                &encode_base64_for_test(&black),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let red = [255, 0, 0, 255];
        // A negative `z` collapses to a gapless (0 ms) frame.
        store
            .apply_kitty_command_with_context(
                b"a=f,i=83,r=2,s=1,v=1,z=-1,q=2",
                &encode_base64_for_test(&red),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let green = [0, 255, 0, 255];
        store
            .apply_kitty_command_with_context(
                b"a=f,i=83,r=3,s=1,v=1,z=50,q=2",
                &encode_base64_for_test(&green),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        store.apply_kitty_command(b"a=a,i=83,s=3,q=2", b"").unwrap();

        let t0 = Instant::now();
        // Root (gapless) and frame 2 (gapless) are both skipped, landing on
        // frame 3, whose 50 ms gap schedules the next wake.
        assert_eq!(
            store.advance_animations(t0),
            Some(Duration::from_millis(50))
        );
        assert_eq!(store.animation_current_frame(83), Some(3));
        let submissions = store.visible_submissions(Rect::new(0, 0, 1, 1));
        assert_eq!(
            submissions[0].encoded_payload(),
            encode_base64_payload(&green).as_slice()
        );
    }

    fn frame_delete_fixture(session: u32) -> (SessionGraphicsStore, [u8; 4], [u8; 4], [u8; 4]) {
        let mut store = SessionGraphicsStore::new(SessionId::new(u64::from(session)));
        let black = [0, 0, 0, 255];
        store
            .apply_kitty_command_with_context(
                format!("a=T,f=32,i={session},s=1,v=1,q=2").as_bytes(),
                &encode_base64_for_test(&black),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let red = [255, 0, 0, 255];
        store
            .apply_kitty_command_with_context(
                format!("a=f,i={session},r=2,s=1,v=1,z=10,q=2").as_bytes(),
                &encode_base64_for_test(&red),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let green = [0, 255, 0, 255];
        store
            .apply_kitty_command_with_context(
                format!("a=f,i={session},r=3,s=1,v=1,z=20,q=2").as_bytes(),
                &encode_base64_for_test(&green),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let blue = [0, 0, 255, 255];
        store
            .apply_kitty_command_with_context(
                format!("a=f,i={session},r=4,s=1,v=1,z=30,q=2").as_bytes(),
                &encode_base64_for_test(&blue),
                (0, 0),
                (10, 10),
            )
            .unwrap();
        (store, red, green, blue)
    }

    #[test]
    fn delete_frame_removes_an_extra_frame_and_renumbers() {
        let (mut store, red, _green, blue) = frame_delete_fixture(95);
        store.apply_kitty_command(b"a=d,d=f,i=95,r=3,q=2", b"").unwrap();
        assert_eq!(store.animation_frame_count(95), Some(2));
        // Frames after the removed one renumber down by one.
        assert_eq!(store.animation_frame_bytes(95, 2), Some(&red[..]));
        assert_eq!(store.animation_frame_bytes(95, 3), Some(&blue[..]));
        assert_eq!(store.animation_frame_bytes(95, 4), None);
        // The removed frame's storage was reclaimed (4 bytes per 1x1 RGBA).
        assert_eq!(store.decoded_bytes_total(), 12);
    }

    #[test]
    fn delete_frame_promotes_the_first_extra_frame_to_the_root() {
        let (mut store, red, green, _blue) = frame_delete_fixture(96);
        store.apply_kitty_command(b"a=d,d=f,i=96,r=1,q=2", b"").unwrap();
        // The first extra frame becomes the new root and the rest renumber.
        assert_eq!(store.decoded_bytes(96), Some(&red[..]));
        assert_eq!(store.animation_frame_count(96), Some(2));
        assert_eq!(store.animation_frame_bytes(96, 2), Some(&green[..]));
        assert_eq!(store.animation_frame_bytes(96, 3), Some(&[0, 0, 255, 255][..]));
    }

    #[test]
    fn delete_frame_adjusts_the_current_frame_index() {
        let (mut store, _red, green, blue) = frame_delete_fixture(97);
        // Removing a frame before the current one shifts it down.
        store.apply_kitty_command(b"a=a,i=97,c=3,q=2", b"").unwrap();
        store.apply_kitty_command(b"a=d,d=f,i=97,r=2,q=2", b"").unwrap();
        assert_eq!(store.animation_current_frame(97), Some(2));
        assert_eq!(store.animation_frame_bytes(97, 2), Some(&green[..]));

        // Removing the current frame keeps the slot, which now holds the next
        // frame.
        store.apply_kitty_command(b"a=a,i=97,c=2,q=2", b"").unwrap();
        store.apply_kitty_command(b"a=d,d=f,i=97,r=2,q=2", b"").unwrap();
        assert_eq!(store.animation_current_frame(97), Some(2));
        assert_eq!(store.animation_frame_bytes(97, 2), Some(&blue[..]));

        // Removing the last frame clamps the current frame to the new last
        // slot (the root once nothing remains).
        store.apply_kitty_command(b"a=a,i=97,c=2,q=2", b"").unwrap();
        store.apply_kitty_command(b"a=d,d=f,i=97,r=2,q=2", b"").unwrap();
        assert_eq!(store.animation_current_frame(97), Some(1));
        assert_eq!(store.animation_frame_count(97), Some(0));
    }

    #[test]
    fn delete_frame_without_extras_noops_but_uppercase_frees_the_image() {
        let mut store = SessionGraphicsStore::new(SessionId::new(98));
        store
            .apply_kitty_command(b"a=T,f=24,i=98,c=1,r=1,q=2", b"AQID")
            .unwrap();
        assert_eq!(store.resource_count(), 1);
        assert_eq!(store.placement_count(), 1);
        // `d=f` with no extra frames is a no-op.
        store.apply_kitty_command(b"a=d,d=f,i=98,q=2", b"").unwrap();
        assert_eq!(store.resource_count(), 1);
        assert_eq!(store.placement_count(), 1);
        // `d=F` frees the entire image.
        store.apply_kitty_command(b"a=d,d=F,i=98,q=2", b"").unwrap();
        assert_eq!(store.resource_count(), 0);
        assert_eq!(store.placement_count(), 0);
    }

    #[test]
    fn delete_frame_rebalances_the_animation_schedule() {
        let (mut store, _red, _green, _blue) = frame_delete_fixture(99);
        store.apply_kitty_command(b"a=a,i=99,s=3,q=2", b"").unwrap();
        let t0 = Instant::now();
        // The gapless root is skipped, landing on frame 2 (10 ms gap).
        assert_eq!(
            store.advance_animations(t0),
            Some(Duration::from_millis(10))
        );
        assert_eq!(store.animation_current_frame(99), Some(2));
        // Delete frame 3 (20 ms gap): playback now cycles the 10 ms and the
        // renumbered 30 ms frame.
        store.apply_kitty_command(b"a=d,d=f,i=99,r=3,q=2", b"").unwrap();
        assert_eq!(store.animation_frame_count(99), Some(2));
        // The clock re-anchored, so the next deadline is 10 ms out again.
        assert_eq!(
            store.advance_animations(t0 + Duration::from_millis(5)),
            Some(Duration::from_millis(10))
        );
        assert_eq!(store.animation_current_frame(99), Some(2));
        // Frame 3 now holds the old 30 ms frame's content.
        assert_eq!(
            store.animation_frame_bytes(99, 3),
            Some(&[0, 0, 255, 255][..])
        );
    }

    #[test]
    fn transfer_negotiation_acknowledges_file_and_shared_memory() {
        let mut store = SessionGraphicsStore::new(SessionId::new(26));
        // f=100 queries must point at a file whose payload has a parseable
        // GIF/PNG header.
        let gif_header = b"GIF89a\x03\x00\x02\x00";
        let file_path = unique_temp_path("query-gif");
        std::fs::write(&file_path, gif_header).unwrap();
        let file_name = encode_base64_for_test(file_path.to_str().unwrap().as_bytes());
        let response = store
            .apply_kitty_command_with_context(b"a=q,i=26,t=f,f=100", &file_name, (0, 0), (0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(response, b"\x1b_Gi=26;OK\x1b\\".to_vec());
        let _ = std::fs::remove_file(&file_path);

        let temp_path = unique_temp_path("query-tty-graphics-protocol-gif");
        std::fs::write(&temp_path, gif_header).unwrap();
        let temp_name = encode_base64_for_test(temp_path.to_str().unwrap().as_bytes());
        let response = store
            .apply_kitty_command_with_context(b"a=q,i=27,t=t,f=100", &temp_name, (0, 0), (0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(response, b"\x1b_Gi=27;OK\x1b\\".to_vec());
        assert!(
            !temp_path.exists(),
            "t=t query should delete the temp file after reading"
        );

        #[cfg(unix)]
        {
            use std::ffi::CString;
            let name = format!("/cmdash-query-gif-shm-{}", unique_temp_suffix());
            let cname = CString::new(name.as_str()).unwrap();
            unsafe {
                let fd = libc::shm_open(
                    cname.as_ptr(),
                    libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
                    0o600,
                );
                assert!(fd >= 0, "shm_open failed");
                assert_eq!(libc::ftruncate(fd, gif_header.len() as libc::off_t), 0);
                let written = libc::write(
                    fd,
                    gif_header.as_ptr() as *const libc::c_void,
                    gif_header.len(),
                );
                assert_eq!(written, gif_header.len() as libc::ssize_t);
                libc::close(fd);
            }
            let shm_name = encode_base64_for_test(name.as_bytes());
            let response = store
                .apply_kitty_command_with_context(b"a=q,i=28,t=s,f=100", &shm_name, (0, 0), (0, 0))
                .unwrap()
                .unwrap();
            assert_eq!(response, b"\x1b_Gi=28;OK\x1b\\".to_vec());
        }
        assert_eq!(store.resource_count(), 0);
    }

    #[test]
    fn file_transfer_loads_an_image_from_a_named_path() {
        let mut store = SessionGraphicsStore::new(SessionId::new(90));
        let path = unique_temp_path("transfer");
        let pixels = [10, 20, 30, 255];
        std::fs::write(&path, pixels).unwrap();
        let filename = encode_base64_for_test(path.to_str().unwrap().as_bytes());
        let parameters = "a=T,f=32,i=90,s=1,v=1,t=f,q=2";
        store
            .apply_kitty_command_with_context(parameters.as_bytes(), &filename, (0, 0), (10, 10))
            .unwrap();
        let submissions = store.visible_submissions(Rect::new(0, 0, 1, 1));
        assert_eq!(submissions.len(), 1);
        assert_eq!(
            submissions[0].encoded_payload(),
            encode_base64_for_test(&pixels).as_slice()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_transfer_honors_size_and_offset_keys() {
        let mut store = SessionGraphicsStore::new(SessionId::new(91));
        let path = unique_temp_path("offset");
        // Two 1x1 RGBA pixels in one file: [red, green].
        let data = [255, 0, 0, 255, 0, 255, 0, 255];
        std::fs::write(&path, data).unwrap();
        let filename = encode_base64_for_test(path.to_str().unwrap().as_bytes());
        // `S=4,O=4` reads only the second pixel (the green one).
        let parameters = "a=T,f=32,i=91,s=1,v=1,t=f,S=4,O=4,q=2";
        store
            .apply_kitty_command_with_context(parameters.as_bytes(), &filename, (0, 0), (10, 10))
            .unwrap();
        let submissions = store.visible_submissions(Rect::new(0, 0, 1, 1));
        assert_eq!(
            submissions[0].encoded_payload(),
            encode_base64_for_test(&[0, 255, 0, 255]).as_slice()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn temp_file_transfer_deletes_only_the_kitty_marker_file() {
        let mut store = SessionGraphicsStore::new(SessionId::new(92));
        let marker = std::env::temp_dir().join(format!(
            "tty-graphics-protocol-cmdash-{}",
            unique_temp_suffix()
        ));
        let plain = unique_temp_path("keep");
        let pixels = [1, 2, 3, 255];
        std::fs::write(&marker, pixels).unwrap();
        std::fs::write(&plain, pixels).unwrap();

        // `t=t` with the kitty marker name is deleted after reading.
        let filename = encode_base64_for_test(marker.to_str().unwrap().as_bytes());
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=92,s=1,v=1,t=t,q=2",
                &filename,
                (0, 0),
                (10, 10),
            )
            .unwrap();
        assert!(!marker.exists());

        // `t=t` with a plain name is left in place.
        let filename = encode_base64_for_test(plain.to_str().unwrap().as_bytes());
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=92,s=1,v=1,t=t,q=2",
                &filename,
                (0, 0),
                (10, 10),
            )
            .unwrap();
        assert!(plain.exists());
        let _ = std::fs::remove_file(&plain);
    }

    #[test]
    fn file_transfer_rejects_missing_files_and_out_of_bounds_ranges() {
        let mut store = SessionGraphicsStore::new(SessionId::new(93));
        let missing = unique_temp_path("missing");
        let filename = encode_base64_for_test(missing.to_str().unwrap().as_bytes());
        assert_eq!(
            store.apply_kitty_command_with_context(
                b"a=T,f=32,i=93,s=1,v=1,t=f,q=2",
                &filename,
                (0, 0),
                (10, 10),
            ),
            Err(GraphicsError::InvalidPayload)
        );

        let path = unique_temp_path("range");
        std::fs::write(&path, [1, 2, 3, 4]).unwrap();
        let filename = encode_base64_for_test(path.to_str().unwrap().as_bytes());
        assert_eq!(
            store.apply_kitty_command_with_context(
                b"a=T,f=32,i=93,s=1,v=1,t=f,S=8,O=0,q=2",
                &filename,
                (0, 0),
                (10, 10),
            ),
            Err(GraphicsError::InvalidPayload)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn shared_memory_transfer_loads_and_unlinks() {
        use std::ffi::CString;
        let mut store = SessionGraphicsStore::new(SessionId::new(94));
        let name = format!("/cmdash-shm-test-{}", unique_temp_suffix());
        let cname = CString::new(name.as_str()).unwrap();
        let pixels = [9, 8, 7, 255];
        unsafe {
            let fd =
                libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR | libc::O_EXCL, 0o600);
            assert!(fd >= 0, "shm_open failed");
            assert_eq!(libc::ftruncate(fd, pixels.len() as libc::off_t), 0);
            let written = libc::write(fd, pixels.as_ptr() as *const libc::c_void, pixels.len());
            assert_eq!(written, pixels.len() as libc::ssize_t);
            libc::close(fd);
        }
        let filename = encode_base64_for_test(name.as_bytes());
        store
            .apply_kitty_command_with_context(
                b"a=T,f=32,i=94,s=1,v=1,t=s,q=2",
                &filename,
                (0, 0),
                (10, 10),
            )
            .unwrap();
        let submissions = store.visible_submissions(Rect::new(0, 0, 1, 1));
        assert_eq!(
            submissions[0].encoded_payload(),
            encode_base64_for_test(&pixels).as_slice()
        );
        // The shm object is unlinked after reading.
        unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0);
            assert!(fd < 0, "shared-memory object should have been unlinked");
        }
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
    fn lowercase_delete_retains_pixel_data_while_uppercase_delete_frees_it() {
        let mut store = SessionGraphicsStore::new(SessionId::new(28));
        store
            .apply_kitty_command(b"a=T,f=24,i=28,c=1,r=1,q=2", b"AQID")
            .unwrap();
        assert_eq!(store.placement_count(), 1);
        assert_eq!(store.resource_count(), 1);
        assert_eq!(store.decoded_bytes(28), Some(&[1u8, 2, 3][..]));

        // Lowercase `d=a` releases the visible placement but retains pixel data
        // so a scrolled-away image can be re-displayed without retransmission.
        store.apply_kitty_command(b"a=d,d=a,i=28", b"").unwrap();
        assert_eq!(store.placement_count(), 0);
        assert_eq!(store.resource_count(), 1);
        assert_eq!(store.decoded_bytes(28), Some(&[1u8, 2, 3][..]));

        // Re-place the retained image without a payload, then uppercase `d=A`
        // releases the placement and frees the now-unreferenced data.
        store.apply_kitty_command(b"a=p,i=28,q=2", b"").unwrap();
        assert_eq!(store.placement_count(), 1);
        store.apply_kitty_command(b"a=d,d=A,i=28", b"").unwrap();
        assert_eq!(store.placement_count(), 0);
        assert_eq!(store.resource_count(), 0);
        assert_eq!(store.decoded_bytes(28), None);
    }

    #[test]
    fn uppercase_delete_by_image_frees_data_only_when_unreferenced() {
        let mut store = SessionGraphicsStore::new(SessionId::new(29));
        store
            .apply_kitty_command(b"a=T,f=24,i=29,q=2", b"AQID")
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=p,i=29,p=1,q=2", b"", (2, 0), (0, 0))
            .unwrap();
        assert_eq!(store.placement_count(), 2);

        // Lowercase `d=i` removes a placement but keeps the pixel data alive.
        store.apply_kitty_command(b"a=d,d=i,i=29,p=1", b"").unwrap();
        assert_eq!(store.placement_count(), 1);
        assert_eq!(store.resource_count(), 1);

        // Uppercase `d=I` frees data only once the last placement is gone.
        store.apply_kitty_command(b"a=d,d=I,i=29", b"").unwrap();
        assert_eq!(store.placement_count(), 0);
        assert_eq!(store.resource_count(), 0);
        assert_eq!(store.decoded_bytes(29), None);
    }

    #[test]
    fn relative_placement_resolves_to_parent_offset_and_leaves_cursor_unmoved() {
        let mut store = SessionGraphicsStore::new(SessionId::new(40));
        store.apply_kitty_command(b"a=t,f=24,i=40", b"AQID").unwrap();
        // Parent placement at cell (2, 3) with placement id 1.
        store
            .apply_kitty_command_with_context(
                b"a=p,i=40,p=1,c=1,r=1,q=2",
                b"",
                (2, 3),
                (10, 20),
            )
            .unwrap();
        assert_eq!(store.take_last_cursor_advance(), Some((1, 1)));
        // Child relative to the parent with H=4,V=2. The cursor position must
        // be ignored and the cursor must not advance.
        store
            .apply_kitty_command_with_context(
                b"a=p,i=40,p=2,P=40,Q=1,H=4,V=2,c=1,r=1,q=2",
                b"",
                (0, 0),
                (10, 20),
            )
            .unwrap();
        assert_eq!(store.take_last_cursor_advance(), None);

        let submissions = store.visible_submissions(Rect::new(0, 0, 20, 20));
        assert_eq!(submissions.len(), 2);
        assert_eq!(submissions[0].placement().area(), Rect::new(2, 3, 1, 1));
        assert_eq!(submissions[1].placement().area(), Rect::new(6, 5, 1, 1));
    }

    #[test]
    fn relative_placements_are_removed_with_their_parent() {
        let mut store = SessionGraphicsStore::new(SessionId::new(41));
        store
            .apply_kitty_command(b"a=T,f=24,i=41,q=2", b"AQID")
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=p,i=41,p=1,q=2", b"", (1, 0), (0, 0))
            .unwrap();
        store
            .apply_kitty_command_with_context(
                b"a=p,i=41,p=2,P=41,Q=1,q=2",
                b"",
                (0, 0),
                (0, 0),
            )
            .unwrap();
        assert_eq!(store.placement_count(), 3);

        // Deleting the parent placement cascades to its relative child.
        store.apply_kitty_command(b"a=d,d=i,i=41,p=1", b"").unwrap();
        assert_eq!(store.placement_count(), 1);
        assert_eq!(store.resource_count(), 1);
    }

    #[test]
    fn relative_placement_errors_map_to_protocol_codes() {
        let mut store = SessionGraphicsStore::new(SessionId::new(42));
        store.apply_kitty_command(b"a=t,f=24,i=42", b"AQID").unwrap();

        // Missing parent maps to ENOPARENT.
        let error = store
            .apply_kitty_command(b"a=p,i=42,p=1,P=99,Q=1", b"")
            .unwrap_err();
        assert_eq!(error, GraphicsError::ParentNotFound(99));
        assert!(String::from_utf8_lossy(&kitty_error_response(
            b"a=p,i=42,p=1,P=99,Q=1",
            &error
        ))
        .contains("ENOPARENT"));

        // A cycle through existing placements maps to ECYCLE.
        store
            .apply_kitty_command_with_context(b"a=p,i=42,p=1,q=2", b"", (1, 0), (0, 0))
            .unwrap();
        store
            .apply_kitty_command_with_context(
                b"a=p,i=42,p=2,P=42,Q=1,q=2",
                b"",
                (0, 0),
                (0, 0),
            )
            .unwrap();
        let error = store
            .apply_kitty_command(b"a=p,i=42,p=1,P=42,Q=2", b"")
            .unwrap_err();
        assert_eq!(error, GraphicsError::RelativeCycle);
        assert!(String::from_utf8_lossy(&kitty_error_response(
            b"a=p,i=42,p=1,P=42,Q=2",
            &error
        ))
        .contains("ECYCLE"));

        // A virtual placement cannot be relative (EINVAL).
        let error = store
            .apply_kitty_command(b"a=p,i=42,p=3,U=1,P=42,Q=1", b"")
            .unwrap_err();
        assert!(matches!(error, GraphicsError::InvalidParameter(_)));
    }

    #[test]
    fn relative_placement_chains_are_bounded_and_report_etoodeep() {
        let mut store = SessionGraphicsStore::new(SessionId::new(43));
        store.apply_kitty_command(b"a=t,f=24,i=43", b"AQID").unwrap();
        store
            .apply_kitty_command_with_context(b"a=p,i=43,p=1,q=2", b"", (0, 0), (0, 0))
            .unwrap();
        // p=2..=9 are relative placements at depths 1..8, all allowed.
        for id in 2..=9 {
            let parameters = format!("a=p,i=43,p={id},P=43,Q={},q=2", id - 1);
            store.apply_kitty_command(parameters.as_bytes(), b"").unwrap();
        }
        assert_eq!(store.placement_count(), 9);

        // The 9th relative placement (depth 9) exceeds the limit.
        let error = store
            .apply_kitty_command(b"a=p,i=43,p=10,P=43,Q=9", b"")
            .unwrap_err();
        assert_eq!(error, GraphicsError::RelativeDepthExceeded);
        assert!(String::from_utf8_lossy(&kitty_error_response(
            b"a=p,i=43,p=10,P=43,Q=9",
            &error
        ))
        .contains("ETOODEEP"));
    }

    #[test]
    fn image_number_transmit_allocates_a_fresh_id_and_reports_it() {
        let mut store = SessionGraphicsStore::new(SessionId::new(53));
        let response = store
            .apply_kitty_command_with_context(b"a=t,f=24,I=13", b"AQID", (0, 0), (0, 0))
            .unwrap()
            .expect("numbered transmit should be acknowledged");
        let text = String::from_utf8_lossy(&response);
        assert!(text.contains("i=1,I=13;OK"), "unexpected response: {text}");
        assert_eq!(store.resource_count(), 1);

        // A second transmit with the same number allocates another fresh id.
        let response = store
            .apply_kitty_command_with_context(b"a=t,f=24,I=13", b"BAUG", (0, 0), (0, 0))
            .unwrap()
            .expect("second numbered transmit should be acknowledged");
        let text = String::from_utf8_lossy(&response);
        assert!(text.contains("i=2,I=13;OK"), "unexpected response: {text}");
        assert_eq!(store.resource_count(), 2);
    }

    #[test]
    fn image_number_resolves_to_newest_image_and_rejects_both_keys() {
        let mut store = SessionGraphicsStore::new(SessionId::new(54));
        store
            .apply_kitty_command_with_context(b"a=t,f=24,I=7", b"AQID", (0, 0), (0, 0))
            .unwrap()
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=t,f=24,I=7", b"BAUG", (0, 0), (0, 0))
            .unwrap()
            .unwrap();

        // a=p with I=7 resolves to the newest (highest id) image.
        let response = store
            .apply_kitty_command_with_context(b"a=p,I=7,c=1,r=1", b"", (0, 0), (0, 0))
            .unwrap()
            .expect("numbered placement should be acknowledged");
        assert!(String::from_utf8_lossy(&response).contains("i=2,I=7"));
        let submissions = store.visible_submissions(Rect::new(0, 0, 4, 4));
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].resource().image(), 2);

        // A command carrying both i and I is an EINVAL.
        let error = store.apply_kitty_command(b"a=p,i=1,I=7", b"").unwrap_err();
        assert!(matches!(error, GraphicsError::InvalidParameter(_)));
        let response = kitty_error_response(b"a=p,i=1,I=7", &error);
        assert!(String::from_utf8_lossy(&response).contains("EINVAL"));
    }

    #[test]
    fn image_number_falls_back_after_the_newest_is_deleted() {
        let mut store = SessionGraphicsStore::new(SessionId::new(55));
        store
            .apply_kitty_command_with_context(b"a=t,f=24,I=9", b"AQID", (0, 0), (0, 0))
            .unwrap()
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=t,f=24,I=9", b"BAUG", (0, 0), (0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(store.resource_count(), 2);

        // Hard-deleting the newest image frees its resource, so I=9 falls back
        // to the surviving older image.
        store.apply_kitty_command(b"a=d,d=I,i=2", b"").unwrap();
        assert_eq!(store.resource_count(), 1);

        store
            .apply_kitty_command_with_context(b"a=p,I=9,c=1,r=1,q=2", b"", (0, 0), (0, 0))
            .unwrap();
        let submissions = store.visible_submissions(Rect::new(0, 0, 4, 4));
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].resource().image(), 1);
    }

    #[test]
    fn cell_column_and_row_delete_selectors_target_the_right_placements() {
        let mut store = SessionGraphicsStore::new(SessionId::new(62));
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=1,c=2,r=2,q=2", b"AQID", (0, 0), (0, 0))
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=2,c=1,r=1,q=2", b"BAUG", (5, 5), (0, 0))
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=3,c=1,r=1,q=2", b"CAUI", (5, 0), (0, 0))
            .unwrap();
        assert_eq!(store.placement_count(), 3);

        // d=p,x=1,y=1 targets the top-left cell, removing the 2x2 image.
        store.apply_kitty_command(b"a=d,d=p,x=1,y=1", b"").unwrap();
        assert_eq!(store.placement_count(), 2);

        // d=y,y=1 targets the first row, removing the (5,0) image.
        store.apply_kitty_command(b"a=d,d=y,y=1", b"").unwrap();
        assert_eq!(store.placement_count(), 1);

        // d=x,x=6 targets column 6 (0-based column 5), removing the (5,5) image.
        store.apply_kitty_command(b"a=d,d=x,x=6", b"").unwrap();
        assert_eq!(store.placement_count(), 0);
    }

    #[test]
    fn cursor_and_z_index_delete_selectors_target_the_right_placements() {
        let mut store = SessionGraphicsStore::new(SessionId::new(63));
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=1,c=1,r=1,q=2", b"AQID", (3, 4), (0, 0))
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=2,c=1,r=1,z=-1,q=2", b"BAUG", (3, 4), (0, 0))
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=3,c=1,r=1,z=-1,q=2", b"CAUI", (7, 7), (0, 0))
            .unwrap();
        assert_eq!(store.placement_count(), 3);

        // d=c at the cursor (3,4) removes both placements at that cell.
        store
            .apply_kitty_command_with_context(b"a=d,d=c", b"", (3, 4), (0, 0))
            .unwrap();
        assert_eq!(store.placement_count(), 1);

        // d=z,z=-1 removes the remaining z=-1 placement.
        store.apply_kitty_command(b"a=d,d=z,z=-1", b"").unwrap();
        assert_eq!(store.placement_count(), 0);
    }

    #[test]
    fn cell_and_z_index_delete_selector_filters_by_both() {
        let mut store = SessionGraphicsStore::new(SessionId::new(64));
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=1,c=1,r=1,z=-1,q=2", b"AQID", (3, 4), (0, 0))
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=2,c=1,r=1,z=-1,q=2", b"BAUG", (7, 7), (0, 0))
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=3,c=1,r=1,q=2", b"CAUI", (3, 4), (0, 0))
            .unwrap();
        assert_eq!(store.placement_count(), 3);

        // d=q targets cell (4,5) [1-based -> (3,4)] with z=-1: only image 1.
        store.apply_kitty_command(b"a=d,d=q,x=4,y=5,z=-1", b"").unwrap();
        assert_eq!(store.placement_count(), 2);
        let images = store
            .visible_submissions(Rect::new(0, 0, 12, 12))
            .iter()
            .map(|submission| submission.resource().image())
            .collect::<Vec<_>>();
        assert_eq!(images, vec![2, 3]);
    }

    #[test]
    fn virtual_placements_are_invisible_and_immune_to_position_selectors() {
        let mut store = SessionGraphicsStore::new(SessionId::new(70));
        // A virtual placement (U=1) at (1,0) and a real placement at (5,0).
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=1,c=1,r=1,U=1,q=2",
                b"AQID",
                (1, 0),
                (0, 0),
            )
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=2,c=1,r=1,q=2", b"BAUG", (5, 0), (0, 0))
            .unwrap();
        assert_eq!(store.placement_count(), 2);
        // The virtual placement never renders; only the real placement does.
        let submissions = store.visible_submissions(Rect::new(0, 0, 8, 4));
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].resource().image(), 2);

        // Position selectors (d=p targeting the virtual cell) skip it.
        store.apply_kitty_command(b"a=d,d=p,x=2,y=1", b"").unwrap();
        assert_eq!(store.placement_count(), 2);

        // d=a (delete visible) also skips virtual placements but removes the
        // real one.
        store.apply_kitty_command(b"a=d,d=a", b"").unwrap();
        assert_eq!(store.placement_count(), 1);

        // Only the id selector removes the virtual placement.
        store.apply_kitty_command(b"a=d,d=i,i=1", b"").unwrap();
        assert_eq!(store.placement_count(), 0);
        assert_eq!(store.resource_count(), 2);
    }

    #[test]
    fn virtual_placements_are_deleted_by_id_number_and_range_selectors() {
        let mut store = SessionGraphicsStore::new(SessionId::new(71));
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=10,c=1,r=1,U=1,q=2", b"AQID", (0, 0), (0, 0))
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=20,c=1,r=1,U=1,q=2", b"BAUG", (2, 0), (0, 0))
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=30,c=1,r=1,q=2", b"CAUI", (4, 0), (0, 0))
            .unwrap();
        assert_eq!(store.placement_count(), 3);

        // d=r removes the two virtual placements in the id range, retaining the
        // real placement outside it.
        store.apply_kitty_command(b"a=d,d=r,x=10,y=20", b"").unwrap();
        assert_eq!(store.placement_count(), 1);
        let submissions = store.visible_submissions(Rect::new(0, 0, 8, 4));
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].resource().image(), 30);

        // A numbered (I) virtual placement is likewise deleted by d=n.
        store
            .apply_kitty_command_with_context(b"a=T,f=24,I=7,c=1,r=1,U=1,q=2", b"BAUG", (0, 0), (0, 0))
            .unwrap();
        assert_eq!(store.placement_count(), 2);
        store.apply_kitty_command(b"a=d,d=n,I=7", b"").unwrap();
        assert_eq!(store.placement_count(), 1);
    }

    #[test]
    fn relative_placements_anchor_to_a_virtual_parent_placeholder_cells() {
        let mut store = SessionGraphicsStore::new(SessionId::new(72));
        // A virtual parent with placement id 1; the creating cursor (2,1) must
        // not move and must not define the child's origin.
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=1,c=1,r=1,U=1,p=1,q=2",
                b"AQID",
                (2, 1),
                (10, 20),
            )
            .unwrap();
        assert_eq!(store.take_last_cursor_advance(), None);

        // Before any placeholder glyphs are written the virtual parent has no
        // physical cell, so a relative child is invisible (not mis-anchored).
        store
            .apply_kitty_command_with_context(
                b"a=T,f=24,i=2,p=2,P=1,Q=1,H=3,V=2,c=1,r=1,q=2",
                b"BAUG",
                (0, 0),
                (10, 20),
            )
            .unwrap();
        assert_eq!(store.visible_submissions(Rect::new(0, 0, 20, 20)).len(), 0);

        // The client then writes placeholder cells at (5,2) and (3,6); the
        // virtual parent's origin is the independent min x / min y = (3,2).
        let cells = [
            (5, 2, 0),
            (3, 6, 0),
        ]
        .into_iter()
        .map(|(column, row, scrollback)| {
            GraphicsPlaceholderCell::new(column, row, scrollback)
        })
        .collect();
        store.set_placeholder_cells([(1, cells)].into_iter().collect());
        assert_eq!(store.placeholder_cell_count(), 2);

        // The child now resolves to (3,2) + (H=3,V=2) = (6,4).
        let submissions = store.visible_submissions(Rect::new(0, 0, 20, 20));
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].placement().area(), Rect::new(6, 4, 1, 1));

        // Deleting the virtual parent by id cascades to its relative child.
        store.apply_kitty_command(b"a=d,d=i,i=1", b"").unwrap();
        assert_eq!(store.placement_count(), 0);
    }

    #[test]
    fn delete_newest_image_number_targets_the_newest_image() {
        let mut store = SessionGraphicsStore::new(SessionId::new(65));
        store
            .apply_kitty_command_with_context(b"a=T,f=24,I=5,c=1,r=1,q=2", b"AQID", (0, 0), (0, 0))
            .unwrap();
        store
            .apply_kitty_command_with_context(b"a=T,f=24,I=5,c=1,r=1,q=2", b"BAUG", (2, 0), (0, 0))
            .unwrap();
        assert_eq!(store.placement_count(), 2);
        assert_eq!(store.resource_count(), 2);

        // Lowercase d=n releases the newest image's placements but keeps data.
        store.apply_kitty_command(b"a=d,d=n,I=5", b"").unwrap();
        assert_eq!(store.placement_count(), 1);
        assert_eq!(store.resource_count(), 2);
        let submissions = store.visible_submissions(Rect::new(0, 0, 8, 4));
        assert_eq!(submissions[0].resource().image(), 1);

        // Uppercase d=N frees the newest surviving image's data.
        store.apply_kitty_command(b"a=d,d=N,I=5", b"").unwrap();
        assert_eq!(store.resource_count(), 1);
        assert_eq!(store.decoded_bytes(2), None);
    }

    #[test]
    fn delete_range_removes_images_in_the_id_range() {
        let mut store = SessionGraphicsStore::new(SessionId::new(66));
        store.apply_kitty_command(b"a=T,f=24,i=10,q=2", b"AQID").unwrap();
        store.apply_kitty_command(b"a=T,f=24,i=20,q=2", b"BAUG").unwrap();
        store.apply_kitty_command(b"a=T,f=24,i=30,q=2", b"CAUI").unwrap();
        assert_eq!(store.resource_count(), 3);
        assert_eq!(store.placement_count(), 3);

        // Lowercase d=r removes placements of images 10..20 but retains data.
        store.apply_kitty_command(b"a=d,d=r,x=10,y=20", b"").unwrap();
        assert_eq!(store.placement_count(), 1);
        assert_eq!(store.resource_count(), 3);

        // Uppercase d=R frees data for images in the range.
        store.apply_kitty_command(b"a=d,d=R,x=10,y=20", b"").unwrap();
        assert_eq!(store.resource_count(), 1);
        assert!(store.decoded_bytes(30).is_some());
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
