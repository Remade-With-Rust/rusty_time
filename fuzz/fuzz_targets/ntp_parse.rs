#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_time_core::ntp;

fuzz_target!(|data: &[u8]| {
    // Parsing must never panic, and a successful parse must re-serialize.
    if let Ok(packet) = ntp::NtpPacket::parse(data) {
        let _ = packet.to_bytes();
    }
    // Extension iteration must terminate and never panic on any input.
    for _ in ntp::extension_fields(data).take(1024) {}
});
