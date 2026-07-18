// PERF-02 regression: a resident format with `checkpoint_on_flush` must not
// serialize the whole index on every flush. Before the geometric-spacing fix,
// flush-after-every-record produced cumulative checkpoint bytes of
// `22 + 94N + 73N^2` (see the 2026-07-19 adversarial review), so doubling the
// record count nearly quadrupled the file. This test pins the amortized O(N)
// behavior and verifies that sparse checkpoints and a checkpoint-less tail
// still recover every native record.

use std::fs::remove_file;
use std::path::PathBuf;

use varve::{INDEX_BLOCK_ID, VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 40, version = 1, kind = "fixed")]
struct GrowthBlock {
    value: u32,
}

varve_format! {
    pub struct GrowthFormat {
        magic: b"GROWTH00";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        index: checkpoint_on_flush;
        blocks: [GrowthBlock];
    }
}

/// Writes `records` blocks, flushing after every single append, and returns the
/// resulting file size in bytes.
fn write_flush_every_record(path: &PathBuf, records: u32) -> varve::Result<u64> {
    {
        let mut file = GrowthFormat::create(path)?;
        for value in 0..records {
            file.push(&GrowthBlock { value })?;
            file.flush()?;
        }
    }
    Ok(std::fs::metadata(path)?.len())
}

#[test]
fn checkpoint_on_flush_grows_linearly_not_quadratically() -> varve::Result<()> {
    let path_64 = temp_path("checkpoint_growth_64");
    let path_128 = temp_path("checkpoint_growth_128");
    cleanup(&path_64);
    cleanup(&path_128);

    let size_64 = write_flush_every_record(&path_64, 64)?;
    let size_128 = write_flush_every_record(&path_128, 128)?;

    // With per-flush full checkpoints the 73N^2 term made size(128) roughly 4x
    // size(64). Geometric spacing keeps the checkpoint bytes O(N), so doubling
    // the records stays well under a 3x growth factor even with generous slack
    // for the base per-record cost.
    assert!(
        size_128 < size_64 * 3,
        "checkpoint bytes look quadratic: size(64)={size_64}, size(128)={size_128}",
    );

    cleanup(&path_64);
    cleanup(&path_128);
    Ok(())
}

#[test]
fn sparse_checkpoints_reopen_sees_every_record() -> varve::Result<()> {
    let path = temp_path("checkpoint_growth_reopen");
    cleanup(&path);

    let records = 128u32;
    write_flush_every_record(&path, records)?;

    let file = GrowthFormat::open_readonly(&path)?;
    let blocks = file.blocks::<GrowthBlock>()?;
    assert_eq!(blocks.len() as u32, records);
    for value in 0..records {
        assert_eq!(
            blocks.get(value as usize)?,
            Some(GrowthBlock { value }),
            "record {value} did not survive reopen",
        );
    }

    // Geometric spacing must leave the growing tail uncheckpointed: after the
    // last full checkpoint many more records were appended without a fresh one.
    // Recovery therefore has to replay a checkpoint-less tail, which the
    // assertions above already exercised; here we just confirm the cadence is
    // actually sparse rather than one-checkpoint-per-flush.
    let checkpoints = file
        .index_entries()
        .iter()
        .filter(|entry| entry.block_id == INDEX_BLOCK_ID)
        .count();
    assert!(
        checkpoints >= 1 && (checkpoints as u32) < records,
        "expected sparse checkpoints, found {checkpoints} for {records} records",
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn checkpointless_tail_recovers_from_empty_file() -> varve::Result<()> {
    // A short file whose record count never crosses the checkpoint floor stores
    // no checkpoint at all; the entire history is a checkpoint-less tail that
    // must still reopen intact.
    let path = temp_path("checkpoint_growth_no_checkpoint");
    cleanup(&path);

    write_flush_every_record(&path, 3)?;

    let file = GrowthFormat::open_readonly(&path)?;
    let blocks = file.blocks::<GrowthBlock>()?;
    assert_eq!(blocks.len(), 3);
    for value in 0..3u32 {
        assert_eq!(blocks.get(value as usize)?, Some(GrowthBlock { value }));
    }
    let checkpoints = file
        .index_entries()
        .iter()
        .filter(|entry| entry.block_id == INDEX_BLOCK_ID)
        .count();
    assert_eq!(checkpoints, 0, "tiny file should not spend a checkpoint");

    cleanup(&path);
    Ok(())
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "varve_{name}_{}_{}.vrv",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ))
}

fn cleanup(path: &PathBuf) {
    match remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", path.display()),
    }
    let lock = path.with_extension("vrv.lock");
    match remove_file(&lock) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", lock.display()),
    }
}
