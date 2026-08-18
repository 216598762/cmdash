use std::collections::BTreeMap;

const APC_PREFIX: &[u8] = b"\x1b_G";
const APC_SUFFIX: &[u8] = b"\x1b\\";
const TMUX_PREFIX: &[u8] = b"\x1bPtmux;";
const MAX_STREAM_BYTES: usize = 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeadlessPixel {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl HeadlessPixel {
    pub const TRANSPARENT: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
        alpha: 0,
    };

    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha: 255,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceRect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Resource {
    format: u8,
    width: u16,
    height: u16,
    pixel_width: u16,
    pixel_height: u16,
    z: i32,
    image_number: u32,
    payload: Vec<u8>,
    pixels: Option<Vec<HeadlessPixel>>,
    animation_frames: BTreeMap<u32, Frame>,
}

/// An animation frame added via Kitty's `a=f` action, mirroring the store's
/// `GraphicsAnimationFrame`. `pixels` holds the transmitted rectangle bytes for
/// a delta frame, or the full coalesced image for a standalone keyframe.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Frame {
    pixels: Vec<HeadlessPixel>,
    width: u16,
    height: u16,
    x: u16,
    y: u16,
    base_frame: u32,
    compose_mode: u8,
    bgcolor: Option<HeadlessPixel>,
}

impl Frame {
    /// Whether this frame already holds the full image at the origin with no
    /// base frame to coalesce and no background canvas to fill.
    fn is_full_keyframe(&self, image_width: u16, image_height: u16) -> bool {
        self.base_frame == 0
            && self.bgcolor.is_none()
            && self.x == 0
            && self.y == 0
            && self.width == image_width
            && self.height == image_height
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlessPlaceholder {
    pub image_id: u32,
    pub x: u16,
    pub y: u16,
    pub row: u16,
    pub column: u16,
    pub z: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlessPlacement {
    pub image_id: u32,
    pub placement_id: Option<u32>,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub z: i32,
    source: Option<SourceRect>,
}

impl HeadlessPlacement {
    /// The placement's source crop as `(x, y, width, height)` in pixels.
    pub fn source(&self) -> Option<(u16, u16, u16, u16)> {
        self.source.map(|source| (source.x, source.y, source.width, source.height))
    }
}

#[derive(Clone, Debug)]
struct PendingUpload {
    parameters: BTreeMap<String, String>,
    payload: Vec<u8>,
}

/// A deliberately small, deterministic terminal-side Kitty model.
///
/// It is not a complete terminal emulator. It models the state needed to
/// validate cmdash's outer adapters: APC framing, tmux passthrough unwrapping,
/// direct/chunked uploads, placements, deletion, Unicode-placeholder cells, and
/// an optional one-pixel-per-cell RGB framebuffer for deterministic acceptance
/// tests.
#[derive(Clone, Debug, Default)]
pub struct HeadlessKittyTerminal {
    resources: BTreeMap<u32, Resource>,
    placements: Vec<HeadlessPlacement>,
    pending_upload: Option<PendingUpload>,
    actions: Vec<&'static str>,
    placeholder_cells: Vec<HeadlessPlaceholder>,
    text: String,
    cursor: (u16, u16),
    foreground: Option<(u8, u8, u8)>,
    viewport: Option<(u16, u16)>,
    acknowledgements: Vec<Vec<u8>>,
    pending_input: Vec<u8>,
    virtual_placements: BTreeMap<u32, (u16, u16, i32)>,
    next_image_id: u32,
    framebuffer: Option<Framebuffer>,
}

#[derive(Clone, Debug)]
enum RenderLayer {
    Placement(HeadlessPlacement),
    Placeholder(HeadlessPlaceholder),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Framebuffer {
    width: u16,
    height: u16,
    pixels: Vec<HeadlessPixel>,
}

impl Framebuffer {
    fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            pixels: vec![HeadlessPixel::TRANSPARENT; usize::from(width) * usize::from(height)],
        }
    }

    fn clear(&mut self) {
        self.pixels.fill(HeadlessPixel::TRANSPARENT);
    }

    fn get(&self, x: u16, y: u16) -> Option<HeadlessPixel> {
        (x < self.width && y < self.height)
            .then(|| self.pixels[usize::from(y) * usize::from(self.width) + usize::from(x)])
    }

    fn blend(&mut self, x: u16, y: u16, source: HeadlessPixel) {
        if x >= self.width || y >= self.height {
            return;
        }
        let index = usize::from(y) * usize::from(self.width) + usize::from(x);
        let destination = self.pixels[index];
        let source_alpha = u16::from(source.alpha);
        let destination_alpha = u16::from(destination.alpha);
        let output_alpha =
            source_alpha.saturating_add(destination_alpha.saturating_mul(255 - source_alpha) / 255);
        if output_alpha == 0 {
            self.pixels[index] = HeadlessPixel::TRANSPARENT;
            return;
        }
        let channel = |foreground: u8, background: u8| {
            let foreground = u16::from(foreground) * source_alpha;
            let background = u16::from(background) * destination_alpha * (255 - source_alpha) / 255;
            ((foreground + background) / output_alpha).min(255) as u8
        };
        self.pixels[index] = HeadlessPixel {
            red: channel(source.red, destination.red),
            green: channel(source.green, destination.green),
            blue: channel(source.blue, destination.blue),
            alpha: output_alpha.min(255) as u8,
        };
    }
}

impl HeadlessKittyTerminal {
    pub fn replay(bytes: &[u8]) -> Result<Self, String> {
        Self::replay_with_viewport(bytes, None)
    }

    pub fn with_viewport(viewport: Option<(u16, u16)>) -> Self {
        Self {
            viewport,
            ..Self::default()
        }
    }

    /// Creates a terminal whose framebuffer uses one pixel for each logical
    /// terminal cell. This deliberately avoids GUI, font, and image-decoder
    /// dependencies while still proving that commands produce visible pixels.
    pub fn with_framebuffer(width: u16, height: u16) -> Self {
        Self {
            viewport: Some((width, height)),
            framebuffer: Some(Framebuffer::new(width, height)),
            ..Self::default()
        }
    }

    pub fn replay_with_framebuffer(bytes: &[u8], width: u16, height: u16) -> Result<Self, String> {
        let mut terminal = Self::with_framebuffer(width, height);
        terminal.feed(bytes)?;
        terminal.finish()?;
        Ok(terminal)
    }

    pub fn framebuffer_size(&self) -> Option<(u16, u16)> {
        self.framebuffer
            .as_ref()
            .map(|framebuffer| (framebuffer.width, framebuffer.height))
    }

    pub fn pixel(&self, x: u16, y: u16) -> Option<HeadlessPixel> {
        self.framebuffer.as_ref()?.get(x, y)
    }

    pub fn visible_pixel_count(&self) -> usize {
        self.framebuffer
            .as_ref()
            .map(|framebuffer| {
                framebuffer
                    .pixels
                    .iter()
                    .filter(|pixel| pixel.alpha != 0)
                    .count()
            })
            .unwrap_or(0)
    }

    pub fn replay_with_viewport(
        bytes: &[u8],
        viewport: Option<(u16, u16)>,
    ) -> Result<Self, String> {
        let mut terminal = Self::with_viewport(viewport);
        terminal.feed(bytes)?;
        terminal.finish()?;
        Ok(terminal)
    }

    /// Feeds one bounded raw-input chunk. Framing may span any number of
    /// chunks; callers must invoke [`Self::finish`] when the stream ends.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        if self.pending_input.len().saturating_add(bytes.len()) > MAX_STREAM_BYTES {
            return Err("headless Kitty stream exceeds the bounded input limit".to_owned());
        }
        self.pending_input.extend_from_slice(bytes);
        Ok(())
    }

    /// Parses all buffered input and rejects an incomplete terminal sequence or
    /// an unfinished Kitty upload.
    pub fn finish(&mut self) -> Result<(), String> {
        let bytes = std::mem::take(&mut self.pending_input);
        self.feed_inner(&bytes)?;
        if self.pending_upload.is_some() {
            return Err("unterminated chunked Kitty upload".to_owned());
        }
        Ok(())
    }

    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }

    pub fn virtual_placement_count(&self) -> usize {
        self.virtual_placements.len()
    }

    pub fn placeholder_count(&self) -> usize {
        self.placeholder_cells.len()
    }

    pub fn placeholder_cells(&self) -> &[HeadlessPlaceholder] {
        &self.placeholder_cells
    }

    /// Returns the topmost placeholder at each visible cell.
    ///
    /// The raw placeholder list retains every parsed cell for diagnostics. The
    /// visible view applies terminal viewport clipping first, then resolves
    /// overlapping cells by z-index, matching the semantic result that an
    /// outer terminal can display.
    pub fn visible_placeholder_cells(&self) -> Vec<HeadlessPlaceholder> {
        let mut visible = BTreeMap::new();
        for cell in &self.placeholder_cells {
            if self
                .viewport
                .is_some_and(|(columns, rows)| cell.x >= columns || cell.y >= rows)
            {
                continue;
            }
            visible
                .entry((cell.x, cell.y))
                .and_modify(|current: &mut HeadlessPlaceholder| {
                    if cell.z >= current.z {
                        *current = cell.clone();
                    }
                })
                .or_insert_with(|| cell.clone());
        }
        visible.into_values().collect()
    }

    pub fn placements(&self) -> &[HeadlessPlacement] {
        &self.placements
    }

    pub fn placements_in_z_order(&self) -> Vec<HeadlessPlacement> {
        let mut placements = self.placements.clone();
        // Equal-z overlaps tie-break by ascending image id, matching Kitty.
        placements.sort_by_key(|placement| (placement.z, placement.image_id));
        placements
    }

    pub fn actions(&self) -> &[&'static str] {
        &self.actions
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn resource_payload(&self, image_id: u32) -> Option<&[u8]> {
        self.resources
            .get(&image_id)
            .map(|resource| resource.payload.as_slice())
    }

    /// The resource's current wire format (`100` for a still-raw PNG/GIF, `32`
    /// once a composition has decoded it to RGBA).
    pub fn resource_format(&self, image_id: u32) -> Option<u8> {
        self.resources.get(&image_id).map(|resource| resource.format)
    }

    /// The resource's decoded root-frame pixels.
    pub fn resource_pixels(&self, image_id: u32) -> Option<&[HeadlessPixel]> {
        self.resources
            .get(&image_id)
            .and_then(|resource| resource.pixels.as_deref())
    }

    /// The number of animation frames stored for an image (excluding the root).
    pub fn animation_frame_count(&self, image_id: u32) -> Option<usize> {
        self.resources
            .get(&image_id)
            .map(|resource| resource.animation_frames.len())
    }

    /// A stored animation frame's raw (delta or keyframe) pixels.
    pub fn animation_frame_pixels(
        &self,
        image_id: u32,
        frame: u32,
    ) -> Option<&[HeadlessPixel]> {
        self.resources
            .get(&image_id)
            .and_then(|resource| resource.animation_frames.get(&frame))
            .map(|frame| frame.pixels.as_slice())
    }

    /// Responses the headless terminal would send after accepting commands.
    pub fn acknowledgements(&self) -> &[Vec<u8>] {
        &self.acknowledgements
    }

    /// The terminal's cursor position (column, row) after the last command.
    pub fn cursor(&self) -> (u16, u16) {
        self.cursor
    }

    fn feed_inner(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index..].starts_with(TMUX_PREFIX) {
                let body_start = index + TMUX_PREFIX.len();
                let end = find_tmux_terminator(bytes, body_start)
                    .ok_or_else(|| "unterminated tmux passthrough sequence".to_owned())?;
                let inner = unescape_tmux(&bytes[body_start..end])?;
                self.feed_inner(&inner)?;
                index = end + APC_SUFFIX.len();
                continue;
            }
            if bytes[index..].starts_with(APC_PREFIX) {
                let body_start = index + APC_PREFIX.len();
                let end_offset = find_bytes(&bytes[body_start..], APC_SUFFIX)
                    .ok_or_else(|| "unterminated Kitty APC sequence".to_owned())?;
                let end = body_start + end_offset;
                self.apply_apc(&bytes[body_start..end])?;
                index = end + APC_SUFFIX.len();
                continue;
            }
            if bytes[index..].starts_with(b"\x1b[") {
                let final_offset = bytes[index + 2..]
                    .iter()
                    .position(|byte| (0x40..=0x7e).contains(byte))
                    .ok_or_else(|| "unterminated CSI sequence".to_owned())?;
                let end = index + 2 + final_offset + 1;
                self.apply_csi(&bytes[index..end])?;
                index = end;
                continue;
            }
            if bytes[index] == 0x1b {
                return Err(format!("unsupported outer escape at byte {index}"));
            }
            let text = std::str::from_utf8(&bytes[index..])
                .map_err(|error| format!("invalid UTF-8 outside Kitty protocol: {error}"))?;
            let character = text
                .chars()
                .next()
                .ok_or_else(|| "missing UTF-8 character".to_owned())?;
            let character_bytes = character.len_utf8();
            if character == '\u{10eeee}' {
                index += self.apply_placeholder(text.as_bytes(), character_bytes)?;
            } else {
                self.text.push(character);
                self.cursor.0 = self.cursor.0.saturating_add(1);
                index += character_bytes;
            }
        }
        Ok(())
    }

    fn apply_csi(&mut self, sequence: &[u8]) -> Result<(), String> {
        let final_byte = *sequence
            .last()
            .ok_or_else(|| "empty CSI sequence".to_owned())?;
        let body = &sequence[2..sequence.len() - 1];
        match final_byte {
            b'H' | b'f' => {
                let mut values = body.split(|byte| *byte == b';');
                let row = parse_csi_value(values.next())?;
                let column = parse_csi_value(values.next())?;
                self.cursor = (column.saturating_sub(1), row.saturating_sub(1));
            }
            b'm' => self.apply_sgr(body)?,
            _ => {}
        }
        Ok(())
    }

    fn apply_sgr(&mut self, body: &[u8]) -> Result<(), String> {
        let values = body
            .split(|byte| *byte == b';')
            .map(|value| {
                std::str::from_utf8(value)
                    .ok()
                    .and_then(|value| value.parse::<u16>().ok())
                    .unwrap_or(0)
            })
            .collect::<Vec<_>>();
        let mut index = 0;
        while index < values.len() {
            match values[index] {
                0 | 39 => self.foreground = None,
                38 if values.get(index + 1) == Some(&2) && values.len() > index + 4 => {
                    self.foreground = Some((
                        values[index + 2].min(255) as u8,
                        values[index + 3].min(255) as u8,
                        values[index + 4].min(255) as u8,
                    ));
                    index += 4;
                }
                _ => {}
            }
            index += 1;
        }
        Ok(())
    }

    fn apply_placeholder(&mut self, bytes: &[u8], base_len: usize) -> Result<usize, String> {
        let mut offset = base_len;
        let mut marks = [0_u16; 3];
        for mark in &mut marks {
            let (character, length) = next_character(bytes, offset)?;
            *mark = combining_mark_index(character).ok_or_else(|| {
                format!(
                    "unknown Kitty placeholder combining mark U+{:04X}",
                    character as u32
                )
            })?;
            offset += length;
        }
        let (red, green, blue) = self
            .foreground
            .ok_or_else(|| "Kitty placeholder has no RGB image-id color".to_owned())?;
        let image_id = (u32::from(marks[2]) << 24)
            | (u32::from(red) << 16)
            | (u32::from(green) << 8)
            | u32::from(blue);
        let z = self
            .resources
            .get(&image_id)
            .map(|resource| resource.z)
            .ok_or_else(|| format!("placeholder references unknown image {image_id}"))?;
        self.placeholder_cells.push(HeadlessPlaceholder {
            image_id,
            x: self.cursor.0,
            y: self.cursor.1,
            row: marks[0],
            column: marks[1],
            z,
        });
        self.cursor.0 = self.cursor.0.saturating_add(1);
        self.render_frame();
        Ok(offset)
    }

    fn apply_apc(&mut self, body: &[u8]) -> Result<(), String> {
        let (parameter_bytes, payload) =
            if let Some(separator) = body.iter().position(|byte| *byte == b';') {
                (&body[..separator], &body[separator + 1..])
            } else {
                let action = body
                    .split(|byte| *byte == b',')
                    .find_map(|field| field.strip_prefix(b"a="))
                    .and_then(|value| value.first().copied());
                if !matches!(
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
                    return Err("Kitty APC has no payload separator".to_owned());
                }
                (body, &[] as &[u8])
            };
        let parameters = parse_parameters(parameter_bytes)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err("Kitty APC payload exceeds the bounded input limit".to_owned());
        }
        let more = parameter(&parameters, "m").unwrap_or(0);
        if self.pending_upload.is_some() {
            let mut pending = self.pending_upload.take().expect("pending upload exists");
            pending.payload.extend_from_slice(payload);
            if pending.payload.len() > MAX_PAYLOAD_BYTES {
                return Err("chunked Kitty payload exceeds the bounded input limit".to_owned());
            }
            for (key, value) in parameters {
                if key != "m" {
                    pending.parameters.insert(key, value);
                }
            }
            if more != 0 {
                self.pending_upload = Some(pending);
                return Ok(());
            }
            self.apply_graphics_command(pending.parameters, &pending.payload)
        } else if more != 0 {
            if parameter_string(&parameters, "a") != Some("T") {
                return Err("only transmit commands may begin a chunked upload".to_owned());
            }
            self.pending_upload = Some(PendingUpload {
                parameters,
                payload: payload.to_vec(),
            });
            Ok(())
        } else {
            self.apply_graphics_command(parameters, payload)
        }
    }

    fn apply_graphics_command(
        &mut self,
        parameters: BTreeMap<String, String>,
        payload: &[u8],
    ) -> Result<(), String> {
        match parameter_string(&parameters, "a") {
            Some("T") | Some("t") => {
                let image_id = self.resolve_transmit_image(&parameters)?;
                let image_number = parameter_u32(&parameters, "I").unwrap_or(0);
                let format = parameter_u32(&parameters, "f")? as u8;
                let width = parameter_u16(&parameters, "c", 1)?;
                let height = parameter_u16(&parameters, "r", 1)?;
                if image_id == 0 {
                    return Err("headless model requires a nonzero image id".to_owned());
                }
                // A non-raw (PNG/GIF) image contributes its natural pixel
                // dimensions; `s`/`v` override them when present. Raw formats
                // take their dimensions from `s`/`v` directly.
                let (pixels, pixel_width, pixel_height) = if format == 100 {
                    let natural = decode_raster(payload);
                    let (natural_width, natural_height) = natural
                        .as_ref()
                        .map(|(_, width, height)| (*width, *height))
                        .unwrap_or((width, height));
                    let pixel_width = parameter_u16(&parameters, "s", natural_width)?;
                    let pixel_height = parameter_u16(&parameters, "v", natural_height)?;
                    (natural.map(|(pixels, _, _)| pixels), pixel_width, pixel_height)
                } else {
                    let pixel_width = parameter_u16(&parameters, "s", width)?;
                    let pixel_height = parameter_u16(&parameters, "v", height)?;
                    (
                        decode_rgba_pixels(format, payload, pixel_width, pixel_height),
                        pixel_width,
                        pixel_height,
                    )
                };
                self.resources.insert(
                    image_id,
                    Resource {
                        format,
                        width,
                        height,
                        pixel_width,
                        pixel_height,
                        z: parameter_i32(&parameters, "z", 0)?,
                        image_number,
                        payload: payload.to_vec(),
                        pixels,
                        animation_frames: BTreeMap::new(),
                    },
                );
                self.placements
                    .retain(|placement| placement.image_id != image_id);
                self.virtual_placements.remove(&image_id);
                self.actions.push("transmit");
                if parameter_string(&parameters, "a") == Some("T") {
                    if parameter_u32(&parameters, "U").unwrap_or(0) != 1 {
                        self.place(image_id, &parameters, width, height)?;
                    } else {
                        self.virtual_placements.insert(
                            image_id,
                            (width, height, parameter_i32(&parameters, "z", 0)?),
                        );
                    }
                }
            }
            Some("p") | Some("P") => {
                let image_id = self.resolve_reference_image(&parameters)?;
                let resource = self
                    .resources
                    .get(&image_id)
                    .ok_or_else(|| format!("placement references unknown image {image_id}"))?;
                if parameter_u32(&parameters, "U").unwrap_or(0) == 1 {
                    self.virtual_placements.insert(
                        image_id,
                        (
                            parameter_u16(&parameters, "c", resource.width)?,
                            parameter_u16(&parameters, "r", resource.height)?,
                            parameter_i32(&parameters, "z", resource.z)?,
                        ),
                    );
                } else {
                    self.place(image_id, &parameters, resource.width, resource.height)?;
                }
                self.actions.push("place");
            }
            Some("d") | Some("D") => match parameter_string(&parameters, "d") {
                Some("i") | Some("I") => {
                    let image_id = parameter_u32(&parameters, "i")?;
                    let uppercase = parameter_string(&parameters, "d") == Some("I");
                    // A `p` key narrows the delete to one placement of the
                    // image, keeping the image data for its other placements
                    // (Kitty's `id_filter_func`). Without it, lowercase `d=i`
                    // releases the placements but retains the image data so
                    // a scrolled-away image can be re-displayed without
                    // retransmission (verified against a real Kitty); only
                    // uppercase `d=I` frees the data too.
                    let placement_id = parameter_u32(&parameters, "p")
                        .ok()
                        .filter(|placement_id| *placement_id != 0);
                    if let Some(placement_id) = placement_id {
                        self.placements.retain(|placement| {
                            !(placement.image_id == image_id
                                && placement.placement_id == Some(placement_id))
                        });
                    } else {
                        if uppercase {
                            self.resources.remove(&image_id);
                        }
                        self.virtual_placements.remove(&image_id);
                        self.placements
                            .retain(|placement| placement.image_id != image_id);
                    }
                    self.actions.push("delete");
                    self.acknowledgements.push(kitty_acknowledgement(image_id));
                }
                Some("p") | Some("P") => {
                    // Delete placements intersecting the 1-based x/y cell.
                    // Virtual placements have no physical location, so this
                    // selector never affects them (matching Kitty).
                    let column = parameter_u32(&parameters, "x")
                        .map(|value| value.saturating_sub(1))
                        .unwrap_or(0);
                    let row = parameter_u32(&parameters, "y")
                        .map(|value| value.saturating_sub(1))
                        .unwrap_or(0);
                    self.placements.retain(|placement| {
                        !(u32::from(placement.x) <= column
                            && column < u32::from(placement.x.saturating_add(placement.width))
                            && u32::from(placement.y) <= row
                            && row < u32::from(placement.y.saturating_add(placement.height)))
                    });
                    self.actions.push("delete");
                }
                Some("a") | Some("A") => {
                    // Delete all visible real placements; virtual placements
                    // and retained image data survive (matching Kitty).
                    self.placements.clear();
                    self.actions.push("delete");
                }
                _ => return Err("unsupported Kitty delete selector".to_owned()),
            },
            Some("f") | Some("F") => self.apply_frame(&parameters, payload)?,
            Some("c") | Some("C") => self.compose_animation_frame(&parameters)?,
            Some("q") | Some("Q") => self.actions.push("query"),
            None => return Err("unsupported Kitty APC without an action".to_owned()),
            Some(action) => return Err(format!("unsupported Kitty action {action}")),
        }
        self.render_frame();
        Ok(())
    }

    /// Applies a Kitty `a=f` animation-frame command, mirroring the store's
    /// `handle_animation_frame_load_command`. Frame 1 is the root frame; a new
    /// frame is stored as a delta with its composition metadata, while editing
    /// an existing frame coalesces and re-composes it.
    fn apply_frame(
        &mut self,
        parameters: &BTreeMap<String, String>,
        payload: &[u8],
    ) -> Result<(), String> {
        let image_id = self.resolve_reference_image(parameters)?;
        let (pixel_width, pixel_height, format) = {
            let resource = self
                .resources
                .get(&image_id)
                .ok_or_else(|| format!("animation frame references unknown image {image_id}"))?;
            (resource.pixel_width, resource.pixel_height, resource.format)
        };
        let requested_frame = parameter_u32_default(parameters, "r", 0)?;
        let frame = if requested_frame == 0 {
            self.resources
                .get(&image_id)
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
        if frame == 0 {
            return Err("animation frame number must be nonzero".to_owned());
        }
        let edits_existing = frame == 1
            || self
                .resources
                .get(&image_id)
                .is_some_and(|resource| resource.animation_frames.contains_key(&frame));
        let rect_width = parameter_u16(parameters, "s", pixel_width)?;
        let rect_height = parameter_u16(parameters, "v", pixel_height)?;
        let offset_x = parameter_u16(parameters, "x", 0)?;
        let offset_y = parameter_u16(parameters, "y", 0)?;
        let base_frame = parameter_u32_default(parameters, "c", 0)?;
        let compose_mode = parameter(parameters, "X").unwrap_or(0);
        let bgcolor = match parameter_string(parameters, "Y") {
            Some(raw) => Some(parse_bgcolor(raw)?),
            None => None,
        };
        // A non-raw (PNG/GIF) image cannot be composed byte-for-byte, so a
        // delta frame for one is rejected exactly like the store.
        let composes = base_frame != 0
            || bgcolor.is_some()
            || compose_mode != 0
            || offset_x != 0
            || offset_y != 0
            || rect_width != pixel_width
            || rect_height != pixel_height;
        if composes && format == 100 {
            return Err("cannot compose animation frames for a non-raw (PNG/GIF) image".to_owned());
        }
        if base_frame != 0
            && base_frame != 1
            && !self
                .resources
                .get(&image_id)
                .is_some_and(|resource| resource.animation_frames.contains_key(&base_frame))
        {
            return Err(format!("animation frame references unknown frame {base_frame}"));
        }
        let frame_pixels = decode_rgba_pixels(format, payload, rect_width, rect_height)
            .ok_or_else(|| "animation frame payload does not match its dimensions".to_owned())?;

        let (stored, width, height, x, y, base, mode, color) = if edits_existing && format != 100 {
            let mut under = self.coalesce_frame(image_id, frame)?;
            compose_rect(
                &mut under,
                pixel_width,
                &frame_pixels,
                offset_x,
                offset_y,
                rect_width,
                rect_height,
                compose_mode == 0 && format != 24,
            );
            (under, pixel_width, pixel_height, 0, 0, 0, 0, None)
        } else {
            (
                frame_pixels,
                rect_width,
                rect_height,
                offset_x,
                offset_y,
                base_frame,
                compose_mode,
                bgcolor,
            )
        };
        if frame == 1 {
            let resource = self
                .resources
                .get_mut(&image_id)
                .expect("resource validated above");
            resource.pixels = Some(stored);
            resource.pixel_width = width;
            resource.pixel_height = height;
        } else {
            self.resources
                .get_mut(&image_id)
                .expect("resource validated above")
                .animation_frames
                .insert(
                    frame,
                    Frame {
                        pixels: stored,
                        width,
                        height,
                        x,
                        y,
                        base_frame: base,
                        compose_mode: mode,
                        bgcolor: color,
                    },
                );
        }
        self.actions.push("frame");
        Ok(())
    }

    /// Applies a Kitty `a=c` animation-frame composition, mirroring the
    /// store's `compose_animation_frame`: `r`/`c` are the source/destination
    /// frame numbers, `X`/`Y` the source origin, `x`/`y` the destination
    /// origin, `w`/`h` the rectangle size, and `C` the mode (0 alpha-blend,
    /// 1 overwrite). A non-raw (PNG/GIF) root frame is decoded to RGBA so it
    /// can be composed, and composing onto it converts the format to 32.
    fn compose_animation_frame(
        &mut self,
        parameters: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        let image_id = self.resolve_reference_image(parameters)?;
        let (format, pixel_width, pixel_height) = {
            let resource = self
                .resources
                .get(&image_id)
                .ok_or_else(|| format!("composition references unknown image {image_id}"))?;
            (resource.format, resource.pixel_width, resource.pixel_height)
        };
        if !matches!(format, 24 | 32 | 100) {
            return Err("cannot compose animation frames for an unsupported format".to_owned());
        }
        let source_frame = parameter_u32_default(parameters, "r", 1)?;
        let destination_frame = parameter_u32_default(parameters, "c", 1)?;
        let source_x = parameter_u16(parameters, "X", 0)?;
        let source_y = parameter_u16(parameters, "Y", 0)?;
        let destination_x = parameter_u16(parameters, "x", 0)?;
        let destination_y = parameter_u16(parameters, "y", 0)?;
        let compose_mode = parameter(parameters, "C").unwrap_or(0);
        let width = parameter_u16(parameters, "w", 0)?;
        let height = parameter_u16(parameters, "h", 0)?;
        let width = if width == 0 { pixel_width } else { width };
        let height = if height == 0 { pixel_height } else { height };
        if width == 0 || height == 0 {
            return Err("composition rectangle must be nonzero".to_owned());
        }
        if source_x.saturating_add(width) > pixel_width
            || source_y.saturating_add(height) > pixel_height
            || destination_x.saturating_add(width) > pixel_width
            || destination_y.saturating_add(height) > pixel_height
        {
            return Err("composition rectangle is out of bounds".to_owned());
        }
        if source_frame == destination_frame
            && rectangles_overlap(
                (source_x, source_y),
                (destination_x, destination_y),
                (width, height),
            )
        {
            return Err("same-frame composition rectangles overlap".to_owned());
        }
        let source_exists = source_frame == 1
            || self
                .resources
                .get(&image_id)
                .is_some_and(|resource| resource.animation_frames.contains_key(&source_frame));
        let destination_exists = destination_frame == 1
            || self
                .resources
                .get(&image_id)
                .is_some_and(|resource| resource.animation_frames.contains_key(&destination_frame));
        if !source_exists || !destination_exists {
            return Err(format!("composition references an unknown frame of image {image_id}"));
        }

        let source_full = self.coalesce_frame(image_id, source_frame)?;
        let mut destination_full = self.coalesce_frame(image_id, destination_frame)?;
        // Read the source rectangle into an owned buffer so a same-frame
        // composition cannot observe its own writes.
        let mut source_rect = Vec::with_capacity(usize::from(width) * usize::from(height));
        for row in 0..height {
            let start = usize::from(source_y.saturating_add(row)) * usize::from(pixel_width)
                + usize::from(source_x);
            source_rect.extend_from_slice(
                &source_full[start..start + usize::from(width)],
            );
        }
        let blends = format != 24 && compose_mode == 0;
        for row in 0..height {
            let destination_row = usize::from(destination_y.saturating_add(row))
                * usize::from(pixel_width)
                + usize::from(destination_x);
            for column in 0..width {
                let destination_index = destination_row + usize::from(column);
                let source_index = usize::from(row) * usize::from(width) + usize::from(column);
                if blends {
                    blend_onto(
                        &mut destination_full[destination_index],
                        source_rect[source_index],
                    );
                } else {
                    destination_full[destination_index] = source_rect[source_index];
                }
            }
        }

        let resource = self
            .resources
            .get_mut(&image_id)
            .expect("resource validated above");
        if destination_frame == 1 {
            // Composing onto a non-raw root decodes it to RGBA, so the
            // resource now stores raw RGBA (format 32).
            if resource.format == 100 {
                resource.format = 32;
            }
            resource.pixels = Some(destination_full);
        } else {
            let animation_frame = resource
                .animation_frames
                .get_mut(&destination_frame)
                .expect("frame validated above");
            animation_frame.pixels = destination_full;
            animation_frame.width = pixel_width;
            animation_frame.height = pixel_height;
            animation_frame.x = 0;
            animation_frame.y = 0;
            animation_frame.base_frame = 0;
            animation_frame.compose_mode = 0;
            animation_frame.bgcolor = None;
        }
        self.actions.push("compose");
        Ok(())
    }

    /// Coalesces an animation frame into a full-image pixel buffer, applying
    /// any `a=f` composition metadata. Frame 1 is the root frame; a delta
    /// frame composes its rectangle onto its `c` base frame (or a `Y`
    /// background canvas when standalone). Mirrors the store's
    /// `get_coalesced_frame_data` chain resolution.
    fn coalesce_frame(&self, image_id: u32, frame: u32) -> Result<Vec<HeadlessPixel>, String> {
        self.coalesce_frame_depth(image_id, frame, 0)
    }

    fn coalesce_frame_depth(
        &self,
        image_id: u32,
        frame: u32,
        depth: u32,
    ) -> Result<Vec<HeadlessPixel>, String> {
        if depth > 32 {
            return Err("animation frame reference chain is too deep".to_owned());
        }
        let resource = self
            .resources
            .get(&image_id)
            .ok_or_else(|| format!("image {image_id} not found"))?;
        let (image_width, image_height) = (resource.pixel_width, resource.pixel_height);
        if frame == 1 {
            return resource
                .pixels
                .clone()
                .ok_or_else(|| format!("image {image_id} has no decoded pixels"));
        }
        let animation_frame = resource
            .animation_frames
            .get(&frame)
            .ok_or_else(|| format!("animation frame {frame} not found"))?;
        if animation_frame.is_full_keyframe(image_width, image_height) {
            return Ok(animation_frame.pixels.clone());
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
        let mut under = if base_frame != 0 {
            self.coalesce_frame_depth(image_id, base_frame, depth + 1)?
        } else {
            let total = usize::from(image_width) * usize::from(image_height);
            match bgcolor {
                Some(color) => vec![color; total],
                None => vec![HeadlessPixel::TRANSPARENT; total],
            }
        };
        compose_rect(
            &mut under,
            image_width,
            &animation_frame.pixels,
            x,
            y,
            width,
            height,
            compose_mode == 0 && resource.format != 24,
        );
        Ok(under)
    }

    fn place(
        &mut self,
        image_id: u32,
        parameters: &BTreeMap<String, String>,
        default_width: u16,
        default_height: u16,
    ) -> Result<(), String> {
        let placement_id = parameter_u32(parameters, "p")
            .ok()
            .filter(|placement_id| *placement_id != 0);
        if let Some(placement_id) = placement_id {
            self.placements.retain(|placement| {
                !(placement.image_id == image_id && placement.placement_id == Some(placement_id))
            });
        }
        let width = parameter_u16(parameters, "c", default_width)?;
        let height = parameter_u16(parameters, "r", default_height)?;
        // Relative placements (P/Q) are anchored to their parent's top-left
        // cell plus an H/V cell offset, and never move the cursor.
        let (x, y, moves_cursor) = if parameters.contains_key("P") || parameters.contains_key("Q") {
            let parent_image = parameter_u32(parameters, "P")?;
            let parent_placement_id = parameter_u32(parameters, "Q")?;
            let Some((parent_x, parent_y)) =
                self.resolve_relative_parent(parent_image, parent_placement_id)?
            else {
                // A virtual parent with no placeholder cells yet has no
                // physical location; the child is invisible (not placed).
                return Ok(());
            };
            let horizontal = parameter_i32(parameters, "H", 0)?;
            let vertical = parameter_i32(parameters, "V", 0)?;
            let x = (i32::from(parent_x) + horizontal).clamp(0, i32::from(u16::MAX)) as u16;
            let y = (i32::from(parent_y) + vertical).clamp(0, i32::from(u16::MAX)) as u16;
            (x, y, false)
        } else {
            (
                self.cursor.0,
                self.cursor.1,
                parameter(parameters, "C").unwrap_or(0) != 1,
            )
        };
        self.placements.push(HeadlessPlacement {
            image_id,
            placement_id,
            x,
            y,
            width,
            height,
            z: parameter_i32(parameters, "z", 0)?,
            source: source_rect(parameters)?,
        });
        // A real graphics terminal advances the cursor right by `c` cells and
        // down by `r` cells after a placement, unless the client requested a
        // static cursor with C=1. Virtual placements (U=1) never reach this
        // path, matching the protocol's physical-location exclusion.
        if moves_cursor {
            self.cursor.0 = self.cursor.0.saturating_add(width);
            self.cursor.1 = self.cursor.1.saturating_add(height);
        }
        Ok(())
    }

    /// Resolves a relative placement's parent to a physical cell origin.
    ///
    /// A normal parent contributes its own `x`/`y`; a virtual (`U=1`) parent
    /// has no cell of its own and instead contributes the min x / min y of its
    /// Unicode placeholder cells (Kitty's `resolve_cell_ref`). `Ok(None)`
    /// means the parent is a virtual placement with no placeholder cells yet,
    /// so the relative child is invisible rather than mis-anchored.
    fn resolve_relative_parent(
        &self,
        parent_image: u32,
        parent_placement_id: u32,
    ) -> Result<Option<(u16, u16)>, String> {
        if let Some(parent) = self.placements.iter().find(|placement| {
            placement.image_id == parent_image
                && placement.placement_id == Some(parent_placement_id)
        }) {
            return Ok(Some((parent.x, parent.y)));
        }
        if !self.virtual_placements.contains_key(&parent_image) {
            return Err("relative placement references a missing parent".to_owned());
        }
        let mut min_x: Option<u16> = None;
        let mut min_y: Option<u16> = None;
        for cell in &self.placeholder_cells {
            if cell.image_id != parent_image {
                continue;
            }
            min_x = Some(min_x.map_or(cell.x, |current| current.min(cell.x)));
            min_y = Some(min_y.map_or(cell.y, |current| current.min(cell.y)));
        }
        Ok(min_x.zip(min_y))
    }

    /// Resolves the image id for a transmit command: an explicit `i` id, or a
    /// fresh id allocated for a numbered (`I`) image.
    fn resolve_transmit_image(&mut self, parameters: &BTreeMap<String, String>) -> Result<u32, String> {
        if parameters.contains_key("i") && parameters.contains_key("I") {
            return Err("i and I are mutually exclusive".to_owned());
        }
        if let Some(raw) = parameters.get("i") {
            let image_id = raw
                .parse::<u32>()
                .map_err(|error| format!("invalid Kitty APC i: {error}"))?;
            if image_id == 0 {
                return Err("headless model requires a nonzero image id".to_owned());
            }
            return Ok(image_id);
        }
        if parameters.contains_key("I") {
            self.next_image_id = self.next_image_id.saturating_add(1).max(1);
            return Ok(self.next_image_id);
        }
        Err("headless model requires an i or I image key".to_owned())
    }

    /// Resolves the image id for a command that references an already
    /// transmitted image: an explicit `i` id, or the newest image with a
    /// given `I` number.
    fn resolve_reference_image(&self, parameters: &BTreeMap<String, String>) -> Result<u32, String> {
        if parameters.contains_key("i") && parameters.contains_key("I") {
            return Err("i and I are mutually exclusive".to_owned());
        }
        if let Some(raw) = parameters.get("i") {
            return raw
                .parse::<u32>()
                .map_err(|error| format!("invalid Kitty APC i: {error}"));
        }
        if let Some(raw) = parameters.get("I") {
            let number = raw
                .parse::<u32>()
                .map_err(|error| format!("invalid Kitty APC I: {error}"))?;
            return self
                .resources
                .iter()
                .filter(|(_, resource)| resource.image_number == number)
                .map(|(image_id, _)| *image_id)
                .max()
                .ok_or_else(|| format!("placement references unknown image number {number}"));
        }
        Err("headless model requires an i or I image key".to_owned())
    }

    fn render_frame(&mut self) {
        let Some(mut framebuffer) = self.framebuffer.take() else {
            return;
        };
        framebuffer.clear();

        let mut layers = self
            .placements
            .iter()
            .cloned()
            .map(|placement| {
                (
                    placement.z,
                    placement.image_id,
                    RenderLayer::Placement(placement),
                )
            })
            .collect::<Vec<_>>();
        layers.extend(
            self.visible_placeholder_cells()
                .into_iter()
                .map(|cell| (cell.z, cell.image_id, RenderLayer::Placeholder(cell))),
        );
        layers.sort_by_key(|(z, image_id, _)| (*z, *image_id));

        for (_, image_id, layer) in layers {
            let Some(resource) = self.resources.get(&image_id).cloned() else {
                continue;
            };
            match layer {
                RenderLayer::Placement(placement) => render_image(
                    &mut framebuffer,
                    &resource,
                    placement.x,
                    placement.y,
                    placement.width,
                    placement.height,
                    placement.source,
                ),
                RenderLayer::Placeholder(cell) => {
                    let Some(&(width, height, _)) = self.virtual_placements.get(&image_id) else {
                        continue;
                    };
                    if width == 0 || height == 0 || cell.column >= width || cell.row >= height {
                        continue;
                    }
                    let source = SourceRect {
                        x: cell.column.saturating_mul(resource.pixel_width) / width,
                        y: cell.row.saturating_mul(resource.pixel_height) / height,
                        width: (resource.pixel_width / width).max(1),
                        height: (resource.pixel_height / height).max(1),
                    };
                    render_image(
                        &mut framebuffer,
                        &resource,
                        cell.x,
                        cell.y,
                        1,
                        1,
                        Some(source),
                    );
                }
            }
        }
        self.framebuffer = Some(framebuffer);
    }
}

fn source_rect(parameters: &BTreeMap<String, String>) -> Result<Option<SourceRect>, String> {
    let has_crop = ["x", "y", "w", "h"]
        .iter()
        .any(|key| parameters.contains_key(*key));
    if !has_crop {
        return Ok(None);
    }
    let value = |key: &str, default: u16| {
        parameters
            .get(key)
            .map(|value| {
                value
                    .parse::<u16>()
                    .map_err(|error| format!("invalid Kitty crop {key}: {error}"))
            })
            .unwrap_or(Ok(default))
    };
    let x = value("x", 0)?;
    let y = value("y", 0)?;
    let width = value("w", u16::MAX)?;
    let height = value("h", u16::MAX)?;
    if width == 0 || height == 0 {
        return Err("Kitty source crop must be nonzero".to_owned());
    }
    Ok(Some(SourceRect {
        x,
        y,
        width,
        height,
    }))
}

/// Decodes a raw RGB/RGBA payload into pixels. Format 100 is treated as RGBA
/// for `a=f` frame payloads, which a non-raw image stores in decoded form.
fn decode_rgba_pixels(
    format: u8,
    payload: &[u8],
    width: u16,
    height: u16,
) -> Option<Vec<HeadlessPixel>> {
    let channels = match format {
        24 => 3usize,
        32 | 100 => 4usize,
        _ => return None,
    };
    let decoded = decode_base64(payload)?;
    let expected = usize::from(width)
        .checked_mul(usize::from(height))?
        .checked_mul(channels)?;
    if decoded.len() != expected {
        return None;
    }
    Some(
        decoded
            .chunks_exact(channels)
            .map(|pixel| HeadlessPixel {
                red: pixel[0],
                green: pixel[1],
                blue: pixel[2],
                alpha: if channels == 4 { pixel[3] } else { 255 },
            })
            .collect(),
    )
}

/// Decodes a PNG or GIF payload into RGBA pixels plus its natural dimensions,
/// so a non-raw (`f=100`) frame can be composed like a raw one.
fn decode_raster(payload: &[u8]) -> Option<(Vec<HeadlessPixel>, u16, u16)> {
    let bytes = decode_base64(payload)?;
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        decode_png_rgba(&bytes)
    } else if bytes.starts_with(b"GIF") {
        decode_gif_rgba(&bytes)
    } else {
        None
    }
}

/// Decodes a PNG into RGBA pixels, normalizing every color type to RGBA via
/// the same `EXPAND | STRIP_16 | ALPHA` transformations as the store.
fn decode_png_rgba(bytes: &[u8]) -> Option<(Vec<HeadlessPixel>, u16, u16)> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(
        png::Transformations::EXPAND
            | png::Transformations::STRIP_16
            | png::Transformations::ALPHA,
    );
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).ok()?;
    let (width, height) = (info.width, info.height);
    if width == 0 || height == 0 || width > u16::MAX as u32 || height > u16::MAX as u32 {
        return None;
    }
    // With EXPAND + ALPHA the output is RGBA or grayscale+alpha; normalize
    // both to RGBA.
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
    let pixels = rgba
        .chunks_exact(4)
        .map(|pixel| HeadlessPixel {
            red: pixel[0],
            green: pixel[1],
            blue: pixel[2],
            alpha: pixel[3],
        })
        .collect();
    Some((pixels, width as u16, height as u16))
}

/// Decodes a GIF's first frame into RGBA pixels, compositing its opaque pixels
/// onto a transparent canvas.
fn decode_gif_rgba(bytes: &[u8]) -> Option<(Vec<HeadlessPixel>, u16, u16)> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options.read_info(bytes).ok()?;
    let width = decoder.width();
    let height = decoder.height();
    if width == 0 || height == 0 {
        return None;
    }
    let mut canvas = vec![HeadlessPixel::TRANSPARENT; usize::from(width) * usize::from(height)];
    let frame = decoder.read_next_frame().ok()??;
    let frame_width = usize::from(frame.width);
    let frame_height = usize::from(frame.height);
    let left = usize::from(frame.left);
    let top = usize::from(frame.top);
    if left.checked_add(frame_width)? > usize::from(width)
        || top.checked_add(frame_height)? > usize::from(height)
    {
        return None;
    }
    for row in 0..frame_height {
        for column in 0..frame_width {
            let source = &frame.buffer[(row * frame_width + column) * 4..][..4];
            if source[3] != 0 {
                canvas[(top + row) * usize::from(width) + left + column] = HeadlessPixel {
                    red: source[0],
                    green: source[1],
                    blue: source[2],
                    alpha: source[3],
                };
            }
        }
    }
    Some((canvas, width, height))
}

/// Parses Kitty's `Y` background-canvas color, a packed 0xRRGGBBAA value.
fn parse_bgcolor(raw: &str) -> Result<HeadlessPixel, String> {
    let value = raw
        .parse::<u32>()
        .map_err(|error| format!("invalid Kitty APC Y: {error}"))?;
    Ok(HeadlessPixel {
        red: ((value >> 24) & 0xff) as u8,
        green: ((value >> 16) & 0xff) as u8,
        blue: ((value >> 8) & 0xff) as u8,
        alpha: (value & 0xff) as u8,
    })
}

/// Whether two same-image rectangles overlap, matching the store's
/// `rect_overlap` same-frame composition guard.
fn rectangles_overlap(a: (u16, u16), b: (u16, u16), size: (u16, u16)) -> bool {
    let (a_x, a_y) = a;
    let (b_x, b_y) = b;
    let (width, height) = size;
    let x_overlaps = a_x.max(b_x) < a_x.min(b_x).saturating_add(width);
    let y_overlaps = a_y.max(b_y) < a_y.min(b_y).saturating_add(height);
    x_overlaps && y_overlaps
}

/// Source-over alpha blends a source pixel onto a destination pixel, matching
/// Kitty's `alpha_blend` for animation composition.
fn blend_onto(destination: &mut HeadlessPixel, source: HeadlessPixel) {
    let source_alpha = u16::from(source.alpha);
    if source_alpha == 0 {
        return;
    }
    if source_alpha == 255 {
        *destination = source;
        return;
    }
    let destination_alpha = u16::from(destination.alpha);
    let output_alpha = source_alpha.saturating_add(destination_alpha.saturating_mul(255 - source_alpha) / 255);
    if output_alpha == 0 {
        *destination = HeadlessPixel::TRANSPARENT;
        return;
    }
    let channel = |foreground: u8, background: u8| {
        let foreground = u16::from(foreground) * source_alpha;
        let background = u16::from(background) * destination_alpha * (255 - source_alpha) / 255;
        ((foreground + background) / output_alpha).min(255) as u8
    };
    *destination = HeadlessPixel {
        red: channel(source.red, destination.red),
        green: channel(source.green, destination.green),
        blue: channel(source.blue, destination.blue),
        alpha: output_alpha.min(255) as u8,
    };
}

/// Composes a pixel rectangle (`over`, sized `over_width` x `over_height`) onto
/// a full-frame buffer (`under`, `under_width` wide) at `(over_x, over_y)`.
#[allow(clippy::too_many_arguments)]
fn compose_rect(
    under: &mut [HeadlessPixel],
    under_width: u16,
    over: &[HeadlessPixel],
    over_x: u16,
    over_y: u16,
    over_width: u16,
    over_height: u16,
    blend: bool,
) {
    for row in 0..over_height {
        let under_row = usize::from(over_y.saturating_add(row)) * usize::from(under_width)
            + usize::from(over_x);
        for column in 0..over_width {
            let destination_index = under_row + usize::from(column);
            let source_index = usize::from(row) * usize::from(over_width) + usize::from(column);
            if blend {
                blend_onto(&mut under[destination_index], over[source_index]);
            } else {
                under[destination_index] = over[source_index];
            }
        }
    }
}

fn decode_base64(payload: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut accumulator = 0_u32;
    let mut bits = 0_u8;
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
        bits = bits.saturating_add(6);
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1_u32 << bits).saturating_sub(1);
        }
    }
    Some(output)
}

fn render_image(
    framebuffer: &mut Framebuffer,
    resource: &Resource,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    crop: Option<SourceRect>,
) {
    let Some(pixels) = resource.pixels.as_ref() else {
        return;
    };
    if width == 0 || height == 0 || resource.pixel_width == 0 || resource.pixel_height == 0 {
        return;
    }
    let crop = crop.unwrap_or(SourceRect {
        x: 0,
        y: 0,
        width: resource.pixel_width,
        height: resource.pixel_height,
    });
    let crop_x = crop.x.min(resource.pixel_width.saturating_sub(1));
    let crop_y = crop.y.min(resource.pixel_height.saturating_sub(1));
    let crop_width = crop
        .width
        .min(resource.pixel_width.saturating_sub(crop_x))
        .max(1);
    let crop_height = crop
        .height
        .min(resource.pixel_height.saturating_sub(crop_y))
        .max(1);
    for destination_y in 0..height {
        for destination_x in 0..width {
            let source_x = crop_x.saturating_add(destination_x.saturating_mul(crop_width) / width);
            let source_y =
                crop_y.saturating_add(destination_y.saturating_mul(crop_height) / height);
            let index =
                usize::from(source_y) * usize::from(resource.pixel_width) + usize::from(source_x);
            if let Some(pixel) = pixels.get(index).copied() {
                framebuffer.blend(
                    x.saturating_add(destination_x),
                    y.saturating_add(destination_y),
                    pixel,
                );
            }
        }
    }
}

fn parse_parameters(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let mut parameters = BTreeMap::new();
    for field in bytes.split(|byte| *byte == b',') {
        if field.is_empty() {
            continue;
        }
        let separator = field
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or_else(|| "Kitty APC control field has no equals sign".to_owned())?;
        let key = std::str::from_utf8(&field[..separator])
            .map_err(|error| format!("invalid Kitty APC key: {error}"))?;
        let value = std::str::from_utf8(&field[separator + 1..])
            .map_err(|error| format!("invalid Kitty APC value: {error}"))?;
        parameters.insert(key.to_owned(), value.to_owned());
    }
    Ok(parameters)
}

fn parameter<'a>(parameters: &'a BTreeMap<String, String>, key: &str) -> Option<u8> {
    parameters.get(key)?.parse().ok()
}

fn parameter_string<'a>(parameters: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    parameters.get(key).map(String::as_str)
}

fn parameter_u32(parameters: &BTreeMap<String, String>, key: &str) -> Result<u32, String> {
    parameters
        .get(key)
        .ok_or_else(|| format!("Kitty APC is missing {key}"))?
        .parse()
        .map_err(|error| format!("invalid Kitty APC {key}: {error}"))
}

fn parameter_u32_default(
    parameters: &BTreeMap<String, String>,
    key: &str,
    default: u32,
) -> Result<u32, String> {
    parameters
        .get(key)
        .map(|value| {
            value
                .parse()
                .map_err(|error| format!("invalid Kitty APC {key}: {error}"))
        })
        .unwrap_or(Ok(default))
}

fn parameter_u16(
    parameters: &BTreeMap<String, String>,
    key: &str,
    default: u16,
) -> Result<u16, String> {
    parameters
        .get(key)
        .map(|value| {
            value
                .parse()
                .map_err(|error| format!("invalid Kitty APC {key}: {error}"))
        })        .unwrap_or(Ok(default))
}




fn parameter_i32(
    parameters: &BTreeMap<String, String>,
    key: &str,
    default: i32,
) -> Result<i32, String> {
    parameters
        .get(key)
        .map(|value| {
            value
                .parse()
                .map_err(|error| format!("invalid Kitty APC {key}: {error}"))
        })
        .unwrap_or(Ok(default))
}

fn parse_csi_value(value: Option<&[u8]>) -> Result<u16, String> {
    let value = value.unwrap_or(b"1");
    if value.is_empty() {
        return Ok(1);
    }
    std::str::from_utf8(value)
        .map_err(|error| format!("invalid CSI parameter: {error}"))?
        .parse()
        .map_err(|error| format!("invalid CSI parameter: {error}"))
}

fn next_character(bytes: &[u8], offset: usize) -> Result<(char, usize), String> {
    let text = std::str::from_utf8(&bytes[offset..])
        .map_err(|error| format!("invalid placeholder UTF-8: {error}"))?;
    let character = text
        .chars()
        .next()
        .ok_or_else(|| "placeholder is missing combining marks".to_owned())?;
    Ok((character, character.len_utf8()))
}

fn combining_mark_index(character: char) -> Option<u16> {
    match character as u32 {
        0x305 => Some(0),
        0x30d => Some(1),
        0x30e => Some(2),
        0x310 => Some(3),
        _ => None,
    }
}

fn find_tmux_terminator(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    while index + 1 < bytes.len() {
        if bytes[index] != 0x1b {
            index += 1;
            continue;
        }
        if bytes[index + 1] == 0x1b {
            index += 2;
            continue;
        }
        if bytes[index + 1] == b'\\' {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn kitty_acknowledgement(image_id: u32) -> Vec<u8> {
    format!("\x1b_Gi={image_id};OK\x1b\\").into_bytes()
}

fn unescape_tmux(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut unescaped = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            if bytes.get(index + 1) != Some(&0x1b) {
                return Err("tmux passthrough contains an unescaped ESC".to_owned());
            }
            unescaped.push(0x1b);
            index += 2;
        } else {
            unescaped.push(bytes[index]);
            index += 1;
        }
    }
    Ok(unescaped)
}
