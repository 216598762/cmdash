//! Optional sixel output for dashboard-provided RGB images.
//!
//! Terminal-originated Kitty resources remain owned by `SessionGraphicsStore`.
//! This adapter is deliberately separate: enabling the `sixel` feature adds a
//! small encoder without changing the default build or the retained scene ABI.

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SixelSubmission {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    encoded: Vec<u8>,
}

impl SixelSubmission {
    pub fn new(x: u16, y: u16, image: SixelImage<'_>) -> Result<Self, SixelError> {
        let width = image.width;
        let height = image.height;
        let encoded = encode_rgb(image)?;
        Ok(Self {
            x,
            y,
            width,
            height,
            encoded,
        })
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

    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }

    pub fn clipped_to(&self, clip: ratatui::layout::Rect) -> Option<Self> {
        let right = self.x.saturating_add(self.width);
        let bottom = self.y.saturating_add(self.height);
        let clip_right = clip.x.saturating_add(clip.width);
        let clip_bottom = clip.y.saturating_add(clip.height);
        (self.x >= clip.x && self.y >= clip.y && right <= clip_right && bottom <= clip_bottom)
            .then(|| self.clone())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SixelImage<'a> {
    pub width: u16,
    pub height: u16,
    pub rgb: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SixelError {
    EmptyImage,
    InvalidDimensions,
}

impl std::fmt::Display for SixelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyImage => formatter.write_str("sixel image cannot be empty"),
            Self::InvalidDimensions => formatter.write_str("sixel RGB data has invalid dimensions"),
        }
    }
}

impl std::error::Error for SixelError {}

const PALETTE: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (128, 0, 0),
    (0, 128, 0),
    (128, 128, 0),
    (0, 0, 128),
    (128, 0, 128),
    (0, 128, 128),
    (192, 192, 192),
    (64, 64, 64),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (0, 0, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

fn palette_index(red: u8, green: u8, blue: u8) -> usize {
    PALETTE
        .iter()
        .enumerate()
        .min_by_key(|&(_, &(palette_red, palette_green, palette_blue))| {
            let distance = |actual: u8, expected: u8| {
                let difference = i32::from(actual) - i32::from(expected);
                difference * difference
            };
            distance(red, palette_red)
                + distance(green, palette_green)
                + distance(blue, palette_blue)
        })
        .map(|(index, _)| index)
        .unwrap_or(0)
}

/// Encodes an RGB image as a bounded 16-color sixel stream.
pub fn encode_rgb(image: SixelImage<'_>) -> Result<Vec<u8>, SixelError> {
    if image.width == 0 || image.height == 0 {
        return Err(SixelError::EmptyImage);
    }
    let expected = image.width as usize * image.height as usize * 3;
    if image.rgb.len() != expected {
        return Err(SixelError::InvalidDimensions);
    }

    let mut output = Vec::with_capacity(expected + 128);
    output.extend_from_slice(b"\x1bPq");
    for (index, &(red, green, blue)) in PALETTE.iter().enumerate() {
        output.extend_from_slice(
            format!(
                "#{index};2;{};{};{}",
                u16::from(red) * 100 / 255,
                u16::from(green) * 100 / 255,
                u16::from(blue) * 100 / 255
            )
            .as_bytes(),
        );
    }

    for (band_index, band_start) in (0..image.height as usize).step_by(6).enumerate() {
        if band_index > 0 {
            output.push(b'-');
        }
        let mut first_color = true;
        for color in 0..PALETTE.len() {
            let has_pixels = (0..image.width as usize).any(|column| {
                (0..6).any(|offset| {
                    let row = band_start + offset;
                    if row >= image.height as usize {
                        return false;
                    }
                    let pixel = (row * image.width as usize + column) * 3;
                    palette_index(image.rgb[pixel], image.rgb[pixel + 1], image.rgb[pixel + 2])
                        == color
                })
            });
            if !has_pixels {
                continue;
            }
            if !first_color {
                output.push(b'$');
            }
            first_color = false;
            output.extend_from_slice(format!("#{color}").as_bytes());
            for column in 0..image.width as usize {
                let mut bits = 0u8;
                for offset in 0..6 {
                    let row = band_start + offset;
                    if row >= image.height as usize {
                        continue;
                    }
                    let pixel = (row * image.width as usize + column) * 3;
                    if palette_index(image.rgb[pixel], image.rgb[pixel + 1], image.rgb[pixel + 2])
                        == color
                    {
                        bits |= 1 << offset;
                    }
                }
                output.push(b'?'.saturating_add(bits));
            }
        }
    }
    output.extend_from_slice(b"\x1b\\");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_small_rgb_images_with_a_bounded_palette() {
        let image = SixelImage {
            width: 2,
            height: 1,
            rgb: &[255, 255, 255, 0, 0, 0],
        };
        let encoded = encode_rgb(image).unwrap();

        assert!(encoded.starts_with(b"\x1bPq#0;2;0;0;0"));
        assert!(encoded.ends_with(b"\x1b\\"));
        assert!(encoded.windows(3).any(|window| window == b"#15"));
    }

    #[test]
    fn rejects_invalid_rgb_buffers() {
        assert_eq!(
            encode_rgb(SixelImage {
                width: 2,
                height: 1,
                rgb: &[0, 0, 0],
            }),
            Err(SixelError::InvalidDimensions)
        );
    }
}
