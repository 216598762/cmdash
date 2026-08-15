use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
};

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
}

impl GraphicsGridAnchor {
    pub const fn new(column: u16, row: u16, scrollback: usize) -> Self {
        Self {
            column,
            row,
            scrollback,
            screen: GraphicsScreen::Primary,
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

    pub const fn with_screen(mut self, screen: GraphicsScreen) -> Self {
        self.screen = screen;
        self
    }

    pub fn resolve_row(self, current_scrollback: usize) -> i32 {
        i32::from(self.row) + self.scrollback as i32 - current_scrollback as i32
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
    anchor: GraphicsGridAnchor,
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

    pub const fn anchor(&self) -> GraphicsGridAnchor {
        self.anchor
    }

    pub const fn area(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, self.height)
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

#[derive(Clone, Debug)]
struct GraphicsResource {
    format: u8,
    generation: u64,
    pixel_width: u32,
    pixel_height: u32,
    decoded_payload: Vec<u8>,
    encoded_payload: Vec<u8>,
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

    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }

    pub fn decoded_bytes(&self, image: u32) -> Option<&[u8]> {
        self.resources
            .get(&image)
            .map(|resource| resource.decoded_payload.as_slice())
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
        let values = parse_parameters(parameters)?;
        let action = values
            .get("a")
            .and_then(|value| value.as_bytes().first())
            .copied();
        let image = values
            .get("i")
            .map(|value| {
                value
                    .parse::<u32>()
                    .map_err(|_| GraphicsError::InvalidImageId)
            })
            .transpose()?
            .unwrap_or(0);
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
        let max_encoded_bytes = self.limits.max_decoded_bytes.saturating_mul(2);
        let requested_image = image;

        if action.is_none() && self.pending_upload.is_some() {
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
            return self.apply_kitty_command_with_grid_state(
                &parameters,
                &pending.encoded_payload,
                cursor,
                cell_size,
                scrollback,
                screen,
            );
        }

        if more != 0 {
            if !matches!(action, Some(b'T') | Some(b't')) {
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
                let decoded_payload =
                    decode_base64(encoded_payload).ok_or(GraphicsError::InvalidPayload)?;
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
                let previous_bytes = self
                    .resources
                    .get(&image)
                    .map_or(0, |resource| resource.decoded_payload.len());
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
                        decoded_payload,
                        encoded_payload: encoded_payload.to_vec(),
                    },
                );
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
                    image, &values, cursor, cell_size, scrollback, screen, dimensions,
                )?;
                if quiet != 2 {
                    response = Some(kitty_response(image, placement_id(&values), "OK"));
                }
            }
            Some(b'd') | Some(b'D') => match values.get("d").map(String::as_str) {
                Some("a") => self.clear(),
                Some("p") => {
                    self.placements.clear();
                }
                _ if image != 0 => {
                    if let Some(resource) = self.resources.remove(&image) {
                        self.decoded_bytes = self
                            .decoded_bytes
                            .saturating_sub(resource.decoded_payload.len());
                    }
                    self.remove_image_placements(image);
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
            anchor: GraphicsGridAnchor::new(cursor.0, cursor.1, scrollback).with_screen(screen),
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
        let mut submissions = self
            .placements
            .values()
            .filter_map(|placement| {
                if placement.anchor.screen() != current_screen {
                    return None;
                }
                let resource = self.resources.get(&placement.resource.image())?;
                let resolved_y = placement.anchor.resolve_row(current_scrollback);
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
    if pixels == 0 || cell_pixels == 0 {
        return Ok(1);
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
    fn unsupported_formats_are_reported_and_ignored() {
        let mut store = SessionGraphicsStore::new(SessionId::new(6));
        store.apply_kitty_command(b"a=T,f=1,i=1", b"AQID").unwrap();
        assert_eq!(store.resource_count(), 0);
        assert!(store.diagnostics()[0].message().contains("unsupported"));
    }
}
