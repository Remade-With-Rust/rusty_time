#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_time_nts::records;

fuzz_target!(|data: &[u8]| {
    for item in records::records(data).take(4096) {
        if let Ok(r) = item {
            let mut out = Vec::new();
            records::write_record(&mut out, r.critical, r.record_type, r.body);
        }
    }
});
