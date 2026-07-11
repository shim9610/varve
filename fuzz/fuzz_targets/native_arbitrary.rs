#![no_main]

use libfuzzer_sys::fuzz_target;
use varve_fuzz::{FuzzNativeFormat, FuzzPoint, FuzzRecord, write_fuzz_file};

fuzz_target!(|data: &[u8]| {
    let Some(path) = write_fuzz_file("native", data) else {
        return;
    };

    {
        if let Ok(reader) = FuzzNativeFormat::open_reader(&path) {
            let reader = reader.into_inner();
            if let Ok(blocks) = reader.blocks::<FuzzPoint>() {
                for index in 0..blocks.len().min(8) {
                    let _ = blocks.get(index);
                }
            }
            if let Ok(blocks) = reader.blocks::<FuzzRecord>() {
                for index in 0..blocks.len().min(8) {
                    let _ = blocks.get(index);
                }
            }

            // SAFETY: this target owns the file and does not mutate it while
            // the mapping or any returned window is alive.
            if let Ok(mapped) = unsafe { reader.mmap_payloads() } {
                for entry in reader.index_entries().iter().take(16) {
                    let _ = mapped.payload_window(entry);
                }
            }
        }
    }

    let _ = FuzzNativeFormat::diagnose_file(&path);
    let _ = FuzzNativeFormat::spec().open_recover_with_report(&path);
});
