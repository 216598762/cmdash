//! Developer helper: regenerate the 1×1 transparent RGBA PNG
//! fixture used by `wiring_smoke::kitty_decode_smoke`.
//!
//! Run with:
//!
//! ```text
//! cargo run --example gen_fixture -p cmdash
//! ```
//!
//! Writes `crates/cmdash/tests/fixtures/img1x1.png` so the
//! integration test can `include_bytes!` it. The fixture must
//! be a valid PNG decodable by `image::load_from_memory`
//! (CRC-correct); Pillow's `Image.save` produces well-formed
//! output by default, so this just round-trips through it
//! rather than hand-crafting every byte.

use image::{ImageBuffer, Rgba};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let png_bytes: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_pixel(1, 1, Rgba([0, 0, 0, 0]));
    let out_path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "fixtures",
        "img1x1.png",
    ]
    .iter()
    .collect();
    std::fs::create_dir_all(out_path.parent().expect("fixtures dir"))?;
    png_bytes.save(&out_path)?;
    let len = std::fs::metadata(&out_path)?.len();
    println!("wrote {} ({} bytes)", out_path.display(), len);
    Ok(())
}
