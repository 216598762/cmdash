#![no_main]

use cmdash::PluginManifestV1;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(source) = std::str::from_utf8(data) {
        let _ = PluginManifestV1::parse(source);
    }
});
