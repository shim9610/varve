#![no_main]

use libfuzzer_sys::fuzz_target;
use varve_fuzz::{FuzzCell, FuzzCellKey, FuzzMatrixFormat, FuzzMatrixFormatDims, write_fuzz_file};

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    // Start from a valid primary + nonce-named .vmg pair to reach generation
    // heads, radix pages, payload checksums and follow, not only header rejection.
    if data[0] & 1 != 0 {
        use std::io::{Seek, SeekFrom, Write};
        let dir = tempfile::tempdir().expect("fuzz directory");
        let path = dir.path().join("matrix.varve");
        let mut writer = FuzzMatrixFormat::create_writer_with_dims(
            &path,
            FuzzMatrixFormatDims { row: 2, column: 2 },
        )
        .expect("valid matrix fixture");
        let key = FuzzCellKey { row: 0, column: 0 };
        writer
            .write_fuzz_cell(key, &FuzzCell { value: 17 })
            .unwrap();
        writer.commit_fuzz_cell(key).unwrap();
        writer.sync().unwrap();
        let mut pinned = FuzzMatrixFormat::open_reader(&path).unwrap();
        writer
            .write_fuzz_cell(key, &FuzzCell { value: 23 })
            .unwrap();
        writer.commit_fuzz_cell(key).unwrap();
        writer.sync().unwrap();
        let companion = writer.matrix_generation_path().unwrap().to_owned();
        drop(writer);
        let target = if data[0] & 2 == 0 { &companion } else { &path };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(target)
            .unwrap();
        let len = file.metadata().unwrap().len();
        let offset = u64::from(u16::from_le_bytes([data[1], data[2]])) % len;
        if data[0] & 4 == 0 {
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&data[3..data.len().min(259)]).unwrap();
        } else {
            file.set_len(offset).unwrap();
        }
        drop(file);
        let _ = pinned.follow();
        let _ = pinned.fuzz_cell_status(key);
        let _ = pinned.fuzz_cell(key);
        if let Ok(reader) = FuzzMatrixFormat::open_reader(&path) {
            let _ = reader.fuzz_cell_status(key);
            let _ = reader.fuzz_cell(key);
            let _ = reader.read_scratch_aux(0, 16);
        }
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
