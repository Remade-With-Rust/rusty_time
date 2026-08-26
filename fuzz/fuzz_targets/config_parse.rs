#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_time_core::config;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = core::str::from_utf8(data) {
        let _ = config::parse(text);
    }
});
