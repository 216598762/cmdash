//! Optional sixel output for dashboard-provided RGB images.
//!
//! Terminal-originated Kitty resources remain owned by `SessionGraphicsStore`.
//! This adapter is deliberately separate: enabling the `sixel` feature adds a
//! small encoder without changing the default build or the retained scene ABI.

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

/// Encodes an RGB image as a conservative two-color sixel stream.
///
/// The first implementation intentionally uses a monochrome threshold rather
/// than introducing an image quantizer dependency. Callers can use it for
/// small status images while richer palettes remain a future optimization.
pub fn encode_rgb(image: SixelImage<'_>) -> Result<Vec<u8>, SixelError> {
    if image.width == 0 || image.height == 0 {
        return Err(SixelError::EmptyImage);
    }
    let expected = image.width as usize * image.height as usize * 3;
    if image.rgb.len() != expected {
        return Err(SixelError::InvalidDimensions);
    }

    let mut output = Vec::with_capacity(expected / 2 + 64);
    output.extend_from_slice(b"\x1bPq#0;2;0;0;0#1;2;100;100;100");
    for band_start in (0..image.height as usize).step_by(6) {
        if band_start != 0 {
            output.push(b'-');
        }
        output.extend_from_slice(b"#1");
        for column in 0..image.width as usize {
            let mut bits = 0u8;
            for offset in 0..6 {
                let row = band_start + offset;
                if row >= image.height as usize {
                    continue;
                }
                let pixel = (row * image.width as usize + column) * 3;
                let luminance = u16::from(image.rgb[pixel]) * 30
                    + u16::from(image.rgb[pixel + 1]) * 59
                    + u16::from(image.rgb[pixel + 2]) * 11;
                if luminance >= 12_750 {
                    bits |= 1 << offset;
                }
            }
            output.push(b'?'.saturating_add(bits));
        }
        output.push(b'$');
    }
    output.extend_from_slice(b"\x1b\\");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_small_rgb_images_as_sixel() {
        let image = SixelImage {
            width: 2,
            height: 1,
            rgb: &[255, 255, 255, 0, 0, 0],
        };
        let encoded = encode_rgb(image).unwrap();

        assert!(encoded.starts_with(b"\x1bPq"));
        assert!(encoded.ends_with(b"\x1b\\"));
        assert!(encoded.contains(&b'@'));
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
