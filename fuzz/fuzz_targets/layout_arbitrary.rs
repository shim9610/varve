#![no_main]

use libfuzzer_sys::fuzz_target;
use varve_fuzz::{FuzzLayoutFormat, write_fuzz_file};

fuzz_target!(|data: &[u8]| {
    let Some(path) = write_fuzz_file("layout", data) else {
        return;
    };

    let _ = FuzzLayoutFormat::inspect_layout_file_report(&path);
    if let Ok(reader) = FuzzLayoutFormat::open_layout_reader(&path) {
        // `segments()` is a lazy iterator now rather than a materialized list,
        // so the count is not available without walking it. Walking at most 16
        // keeps the per-input work bounded, which is what the old `.min(16)`
        // was for.
        for index in 0..reader.segments().take(16).count() {
            let _ = reader.read_metadata(index);
            let _ = reader.read_raw(index);
        }
    }
});
