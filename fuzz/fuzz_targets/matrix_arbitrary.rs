#![no_main]

use libfuzzer_sys::fuzz_target;
use varve_fuzz::{FuzzCellKey, FuzzMatrixFormat, write_fuzz_file};

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let split_seed = u32::from_le_bytes(data[..4].try_into().expect("fixed prefix")) as usize;
    let payload = &data[4..];
    let split = split_seed.min(payload.len());
    let Some(matrix_path) = write_fuzz_file("matrix", &payload[..split]) else {
        return;
    };
    let Some(sidecar_path) = write_fuzz_file("matrix-sidecar", &payload[split..]) else {
        return;
    };

    if let Ok(reader) = FuzzMatrixFormat::open_reader(&matrix_path) {
        let key = FuzzCellKey { row: 0, column: 0 };
        let _ = reader.fuzz_cell_status(key);
        let _ = reader.fuzz_cell(key);
        let _ = reader.read_scratch_aux(0, 16);
        let reader = reader.into_inner();
        let _ = reader.matrix_recovery_report();
        let _ = reader.read_matrix_sidecar("fuzz", &sidecar_path);
    }
});
