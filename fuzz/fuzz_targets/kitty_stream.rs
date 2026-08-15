#![no_main]

use cmdash::kitty_stream_stats;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = kitty_stream_stats(data);
});
