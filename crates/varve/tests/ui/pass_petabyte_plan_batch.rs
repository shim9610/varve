use std::path::Path;

use varve::{
    BatchAppendError, BatchAppendInfo, BatchOptions, DiskIndexOptions,
    DiskIndexPlan, DiskIndexRebuildReport, Result as VarveResult, ScanOptions,
    StreamBootstrapReport, StreamOptions, WriterLockBreakPolicy, varve_format,
};

varve_format! {
    pub format PetabytePlanBatchFormat {
        magic: b"PTPB";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            variable Frame(id = 8, key = [scan, frame], key_index = disk) {
                scan: u32,
                frame: u32,
                payload: Vec<u8>,
            }

            fixed Sample(id = 4) {
                value: u64,
            }

            variable Observation(id = 2, key = [id], key_index = disk) {
                id: u64,
                payload: Vec<u8>,
            }

            fixed Account(id = 6, key = [id], key_index = memory) {
                id: u64,
                active: bool,
            }
        }
    }
}

fn generated_plan() -> DiskIndexPlan {
    PetabytePlanBatchFormat::disk_index_plan().unwrap()
}

fn stream_borrowed_batch(
    writer: &mut PetabytePlanBatchFormatStreamWriter,
    samples: &[Sample],
) -> Result<BatchAppendInfo, BatchAppendError> {
    writer.push_samples(samples, BatchOptions::default())
}

fn indexed_owned_batch(
    writer: &mut PetabytePlanBatchFormatIndexedWriter,
    observations: Vec<Observation>,
) -> Result<BatchAppendInfo, BatchAppendError> {
    writer.push_observations(observations, BatchOptions::default())
}

fn indexed_borrowed_batches(
    writer: &mut PetabytePlanBatchFormatIndexedWriter,
    frames: &[Frame],
    samples: &[Sample],
) {
    let _: Result<BatchAppendInfo, BatchAppendError> =
        writer.push_frames(frames, BatchOptions::default());
    let _: Result<BatchAppendInfo, BatchAppendError> =
        writer.push_samples(samples, BatchOptions::default());
}

fn indexed_explicit_scans(
    reader: &PetabytePlanBatchFormatIndexedReader,
) -> VarveResult<()> {
    let _events = reader.events()?;
    let _frames = reader.frames()?;
    let _samples = reader.samples()?;
    let _verified = reader.verify_all()?;
    let _verified = reader.verify_all_with_progress(ScanOptions::default(), |_| {})?;
    Ok(())
}

fn bootstrap_stream(path: &Path) -> VarveResult<StreamBootstrapReport> {
    PetabytePlanBatchFormat::bootstrap_stream_checkpoint(path, StreamOptions::default())
}

fn bootstrap_stream_with_progress(path: &Path) -> VarveResult<StreamBootstrapReport> {
    PetabytePlanBatchFormat::bootstrap_stream_checkpoint_with_progress(
        path,
        StreamOptions::default(),
        ScanOptions::default(),
        |_| {},
    )
}

fn restore_stream(
    path: &Path,
) -> VarveResult<PetabytePlanBatchFormatStreamWriter> {
    PetabytePlanBatchFormat::restore_stream_writer(path, StreamOptions::default())
}

fn rebuild_index(path: &Path) -> VarveResult<DiskIndexRebuildReport> {
    PetabytePlanBatchFormat::rebuild_disk_index(path, DiskIndexOptions::default())
}

fn rebuild_index_with_progress(path: &Path) -> VarveResult<DiskIndexRebuildReport> {
    PetabytePlanBatchFormat::rebuild_disk_index_with_progress(
        path,
        DiskIndexOptions::default(),
        ScanOptions::default(),
        |_| {},
    )
}

fn restore_index(
    path: &Path,
) -> VarveResult<PetabytePlanBatchFormatIndexedWriter> {
    PetabytePlanBatchFormat::restore_indexed_writer(path, DiskIndexOptions::default())
}

fn clear_stale_lock(path: &Path) -> VarveResult<()> {
    PetabytePlanBatchFormat::clear_stale_writer_lock(
        path,
        WriterLockBreakPolicy::BreakIfProcessAbsent,
    )
}

fn main() {
    let _ = (
        generated_plan,
        stream_borrowed_batch,
        indexed_owned_batch,
        indexed_borrowed_batches,
        indexed_explicit_scans,
        bootstrap_stream,
        bootstrap_stream_with_progress,
        restore_stream,
        rebuild_index,
        rebuild_index_with_progress,
        restore_index,
        clear_stale_lock,
    );
}
