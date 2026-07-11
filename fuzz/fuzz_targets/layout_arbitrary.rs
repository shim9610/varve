#![no_main]

use libfuzzer_sys::fuzz_target;
use varve_fuzz::{FuzzLayoutFormat, write_fuzz_file};

fuzz_target!(|data: &[u8]| {
    let Some(path) = write_fuzz_file("layout", data) else {
        return;
    };

    let _ = FuzzLayoutFormat::inspect_layout_file_report(&path);
    if let Ok(reader) = FuzzLayoutFormat::open_layout_reader(&path) {
        for index in 0..reader.segments().len().min(16) {
            let _ = reader.read_metadata(index);
            let _ = reader.read_raw(index);
        }
    }
});
