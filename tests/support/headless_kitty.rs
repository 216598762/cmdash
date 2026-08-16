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
    z: i16,
    payload: Vec<u8>,
    pixels: Option<Vec<HeadlessPixel>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlessPlaceholder {
    pub image_id: u32,
    pub x: u16,
    pub y: u16,
    pub row: u16,
    pub column: u16,
    pub z: i16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlessPlacement {
    pub image_id: u32,
    pub placement_id: Option<u32>,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub z: i16,
    source: Option<SourceRect>,
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
    virtual_placements: BTreeMap<u32, (u16, u16, i16)>,
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
        placements.sort_by_key(|placement| placement.z);
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

    /// Responses the headless terminal would send after accepting commands.
    pub fn acknowledgements(&self) -> &[Vec<u8>] {
        &self.acknowledgements
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
                let image_id = parameter_u32(&parameters, "i")?;
                let format = parameter_u32(&parameters, "f")? as u8;
                let width = parameter_u16(&parameters, "c", 1)?;
                let height = parameter_u16(&parameters, "r", 1)?;
                let pixel_width = parameter_u16(&parameters, "s", width)?;
                let pixel_height = parameter_u16(&parameters, "v", height)?;
                if image_id == 0 {
                    return Err("headless model requires a nonzero image id".to_owned());
                }
                self.resources.insert(
                    image_id,
                    Resource {
                        format,
                        width,
                        height,
                        pixel_width,
                        pixel_height,
                        z: parameter_i16(&parameters, "z", 0)?,
                        payload: payload.to_vec(),
                        pixels: decode_pixels(format, payload, pixel_width, pixel_height),
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
                            (width, height, parameter_i16(&parameters, "z", 0)?),
                        );
                    }
                }
            }
            Some("p") | Some("P") => {
                let image_id = parameter_u32(&parameters, "i")?;
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
                            parameter_i16(&parameters, "z", resource.z)?,
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
                    self.resources.remove(&image_id);
                    self.virtual_placements.remove(&image_id);
                    self.placements
                        .retain(|placement| placement.image_id != image_id);
                    self.actions.push("delete");
                    self.acknowledgements.push(kitty_acknowledgement(image_id));
                }
                Some("p") | Some("P") => {
                    self.placements.clear();
                    self.actions.push("delete");
                }
                Some("a") | Some("A") => {
                    self.resources.clear();
                    self.virtual_placements.clear();
                    self.placements.clear();
                    self.actions.push("delete");
                }
                _ => return Err("unsupported Kitty delete selector".to_owned()),
            },
            Some("q") | Some("Q") => self.actions.push("query"),
            None => return Err("unsupported Kitty APC without an action".to_owned()),
            Some(action) => return Err(format!("unsupported Kitty action {action}")),
        }
        self.render_frame();
        Ok(())
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
        self.placements.push(HeadlessPlacement {
            image_id,
            placement_id,
            x: self.cursor.0,
            y: self.cursor.1,
            width: parameter_u16(parameters, "c", default_width)?,
            height: parameter_u16(parameters, "r", default_height)?,
            z: parameter_i16(parameters, "z", 0)?,
            source: source_rect(parameters)?,
        });
        Ok(())
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

fn decode_pixels(
    format: u8,
    payload: &[u8],
    width: u16,
    height: u16,
) -> Option<Vec<HeadlessPixel>> {
    if !matches!(format, 24 | 32) {
        return None;
    }
    let decoded = decode_base64(payload)?;
    let channels = usize::from(format / 8);
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
                alpha: if format == 32 { pixel[3] } else { 255 },
            })
            .collect(),
    )
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
        })
        .unwrap_or(Ok(default))
}

fn parameter_i16(
    parameters: &BTreeMap<String, String>,
    key: &str,
    default: i16,
) -> Result<i16, String> {
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
