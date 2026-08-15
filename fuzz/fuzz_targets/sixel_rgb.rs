#![no_main]

use cmdash::sixel::{SixelImage, encode_rgb};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let width = u16::from(data[0] % 64).max(1);
    let height = u16::from(data[1] % 64).max(1);
    let pixels = &data[2..];
    let expected = usize::from(width) * usize::from(height) * 3;
    if pixels.len() >= expected {
        let _ = encode_rgb(SixelImage {
            width,
            height,
            rgb: &pixels[..expected],
        });
    }
});
