#![no_main]

use cmdash::AppConfig;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(source) = std::str::from_utf8(data) {
        let _ = AppConfig::parse_with_migrations(source);
    }
});
