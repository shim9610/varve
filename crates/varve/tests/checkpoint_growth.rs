// PERF-02 regression: a resident format with `checkpoint_on_flush` must not
// serialize the whole index on every flush. Before the geometric-spacing fix,
// flush-after-every-record produced cumulative checkpoint bytes of
// `22 + 94N + 73N^2` (see the 2026-07-19 adversarial review), so doubling the
// record count nearly quadrupled the file. This test pins the amortized O(N)
// behavior and verifies that sparse checkpoints and a checkpoint-less tail
// still recover every native record.
//
// PERF2-02 regression: the checkpoint *decision* must also be O(1) per flush.
// The first fix linearized the file bytes but still reverse-searched the
// resident index on every flush (179,392 predicate entry touches at 1,024
// records vs 11,253,678 at 8,192 in the 2026-07-19 re-verification), keeping
// cumulative flush CPU O(N^2). The cadence-touch test below delta-measures a
// thread-local counter of index entries examined by the cadence machinery and
// pins near-linear cumulative touches, and the reopen test pins that the O(1)
// writer state recovered at open makes exactly the same checkpoint decisions
// as an uninterrupted writer.

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

/// Returns the index positions of every full checkpoint record in `path`.
fn checkpoint_positions(path: &PathBuf) -> varve::Result<Vec<usize>> {
    let file = GrowthFormat::open_readonly(path)?;
    Ok(file
        .index_entries()
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.block_id == INDEX_BLOCK_ID)
        .map(|(position, _)| position)
        .collect())
}

// PERF2-02: the O(1) flush-cadence state recovered at reopen must make the
// writer take exactly the same checkpoint decisions as a writer that never
// restarted. Any recovery bug (forgetting the live tail, resetting the
// geometric threshold, or double-counting the suffix) shifts or duplicates a
// checkpoint record and diverges the two index layouts.
#[test]
fn reopened_writer_recovers_checkpoint_cadence() -> varve::Result<()> {
    let continuous = temp_path("checkpoint_growth_cadence_cont");
    let restarted = temp_path("checkpoint_growth_cadence_restart");
    cleanup(&continuous);
    cleanup(&restarted);

    let records = 96u32;
    let split = 48u32;
    write_flush_every_record(&continuous, records)?;

    {
        let mut file = GrowthFormat::create(&restarted)?;
        for value in 0..split {
            file.push(&GrowthBlock { value })?;
            file.flush()?;
        }
    }
    {
        let mut file = GrowthFormat::open(&restarted)?;
        // Nothing eligible was appended since the last flush of the previous
        // session, so a bare flush after reopen must not spend a checkpoint;
        // if it did, the layouts below could not match.
        file.flush()?;
        for value in split..records {
            file.push(&GrowthBlock { value })?;
            file.flush()?;
        }
    }

    let continuous_checkpoints = checkpoint_positions(&continuous)?;
    let restarted_checkpoints = checkpoint_positions(&restarted)?;
    assert!(
        !continuous_checkpoints.is_empty(),
        "workload too small to spend any checkpoint; the comparison is vacuous",
    );
    assert_eq!(
        restarted_checkpoints, continuous_checkpoints,
        "reopened writer diverged from the uninterrupted checkpoint cadence",
    );

    // The restart must also not have lost any data.
    let file = GrowthFormat::open_readonly(&restarted)?;
    let blocks = file.blocks::<GrowthBlock>()?;
    assert_eq!(blocks.len() as u32, records);
    for value in 0..records {
        assert_eq!(blocks.get(value as usize)?, Some(GrowthBlock { value }));
    }
    drop(file);

    cleanup(&continuous);
    cleanup(&restarted);
    Ok(())
}

// PERF2-02: cumulative index-entry touches by the checkpoint cadence machinery
// must stay near-linear in the number of flush-per-record appends. The
// pre-fix predicate re-walked the resident index on every flush, so doubling
// the record count roughly quadrupled the touches; the O(1) state keeps the
// ratio at ~2x. The counter is thread-local, so concurrent tests in this
// binary cannot pollute the delta measurements.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn checkpoint_cadence_touches_grow_linearly() -> varve::Result<()> {
    use varve::VarveFile;

    fn measure(records: u32, name: &str) -> varve::Result<u64> {
        let path = temp_path(name);
        cleanup(&path);
        let before = VarveFile::checkpoint_cadence_index_touches();
        write_flush_every_record(&path, records)?;
        let after = VarveFile::checkpoint_cadence_index_touches();
        cleanup(&path);
        Ok(after - before)
    }

    let touches_256 = measure(256, "checkpoint_growth_touches_256")?;
    let touches_512 = measure(512, "checkpoint_growth_touches_512")?;

    // O(1) cadence work per append/flush: generous constant-factor slack, but
    // nowhere near the ~8,500 touches the per-flush rescan needed at N=256.
    assert!(
        touches_256 <= 4 * 256 + 64,
        "cadence touches look super-linear at 256 records: {touches_256}",
    );
    assert!(
        touches_512 <= 4 * 512 + 64,
        "cadence touches look super-linear at 512 records: {touches_512}",
    );
    // Near-linear growth: doubling the input must not triple the touches
    // (the quadratic predicate produced ~4x here).
    assert!(
        touches_512 <= 3 * touches_256,
        "cadence touches grow super-linearly: 256 -> {touches_256}, 512 -> {touches_512}",
    );
    Ok(())
}

#[cfg(not(feature = "scalable-fault-injection"))]
#[test]
#[ignore = "requires the scalable-fault-injection feature"]
fn checkpoint_cadence_touches_grow_linearly() {}

struct TempPath {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for TempPath {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

impl AsRef<std::path::Path> for TempPath {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

fn temp_path(name: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir.path().join(format!(
        "varve_{name}_{}_{}.vrv",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    TempPath { path, _dir: dir }
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
