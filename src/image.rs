//! Feature-gated image decoding for dashboard and script-widget images.
//!
//! Compiled only with the `image` feature. The terminal graphics protocol
//! slice (`f=100` PNG/GIF) keeps using the narrower `png`/`gif` crates
//! regardless, because Kitty's in-band transfer is PNG-only; this module is
//! the decode hook for the dashboard/script-widget image path the `image`
//! feature unlocks (JPEG/BMP today; WebP is not vendored for offline builds).

use image::ImageFormat;

/// A decoded image: packed `width * height * 4` RGBA8 bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decodes JPEG or BMP bytes into RGBA8.
///
/// Returns `None` for unrecognized payloads and for formats the graphics
/// protocol path owns (PNG/GIF are handled by `SessionGraphicsStore`), so the
/// two decode paths never overlap. The `image` crate's format sniffing keeps
/// the `jpeg`/`bmp` decoders (and their budgets) out of the default build.
pub fn decode_image(bytes: &[u8]) -> Option<DecodedImage> {
    let format = image::guess_format(bytes).ok()?;
    match format {
        ImageFormat::Jpeg | ImageFormat::Bmp => {}
        _ => return None,
    }
    let decoded = image::load_from_memory_with_format(bytes, format).ok()?;
    let rgba = decoded.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some(DecodedImage {
        width,
        height,
        rgba: rgba.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a valid 1x1 24-bit BMP encoding an opaque red pixel.
    fn one_pixel_bmp() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"BM");
        bytes.extend_from_slice(&58u32.to_le_bytes()); // file size (54 header + 4 pixels)
        bytes.extend_from_slice(&0u32.to_le_bytes()); // reserved
        bytes.extend_from_slice(&54u32.to_le_bytes()); // pixel-data offset
        bytes.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
        bytes.extend_from_slice(&1u32.to_le_bytes()); // width
        bytes.extend_from_slice(&1u32.to_le_bytes()); // height
        bytes.extend_from_slice(&1u16.to_le_bytes()); // planes
        bytes.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
        bytes.extend_from_slice(&0u32.to_le_bytes()); // compression (BI_RGB)
        bytes.extend_from_slice(&4u32.to_le_bytes()); // image size
        bytes.extend_from_slice(&0u32.to_le_bytes()); // horizontal ppm
        bytes.extend_from_slice(&0u32.to_le_bytes()); // vertical ppm
        bytes.extend_from_slice(&0u32.to_le_bytes()); // colors used
        bytes.extend_from_slice(&0u32.to_le_bytes()); // important colors
        bytes.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00]); // BGR red + row pad
        bytes
    }

    #[test]
    fn decode_image_reads_a_bmp_into_rgba() {
        let decoded = decode_image(&one_pixel_bmp()).expect("1x1 BMP should decode");
        assert_eq!(decoded.width, 1);
        assert_eq!(decoded.height, 1);
        assert_eq!(decoded.rgba, vec![0xFF, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn decode_image_reads_a_jpeg_into_rgba() {
        let encoded = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2,
            1,
            image::Rgb([0x12, 0x34, 0x56]),
        ));
        let mut bytes = Vec::new();
        encoded
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Jpeg)
            .expect("test JPEG should encode");
        let decoded = decode_image(&bytes).expect("2x1 JPEG should decode");
        assert_eq!(decoded.width, 2);
        assert_eq!(decoded.height, 1);
        assert_eq!(decoded.rgba.len(), 8); // 2 pixels * 4 RGBA bytes
    }

    #[test]
    fn decode_image_rejects_protocol_and_unknown_formats() {
        // PNG is owned by the graphics protocol slice, not this decode path.
        assert!(decode_image(b"\x89PNG\r\n\x1a\n").is_none());
        assert!(decode_image(b"not an image").is_none());
        assert!(decode_image(&[]).is_none());
    }
}
