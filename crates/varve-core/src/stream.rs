use std::borrow::Borrow;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use crate::collections::{MaterializationBudget, ensure_registered_block};
use crate::disk_index::{
    DiskIndexBatchOptions, DiskIndexDigest, DiskIndexError, DiskIndexFrontier, DiskIndexMetadata,
    DiskIndexMode, DiskIndexOptions, DiskIndexPrimaryGeneration, DiskIndexSnapshot, DiskIndexStore,
    DiskIndexTail, DiskIndexWriteBatch, PRIMARY_GENERATION_WINDOW, batch_item_fixed_bytes,
    state_sidecar_path, tail_limit_for_spec,
};
use crate::file::{
    NativeStreamScanner, PreparedStreamRecord, ReplaceDurability, WriterLock,
    fresh_stream_creation_nonce, prepare_stream_creation_nonce_record,
    prepare_stream_manifest_record, prepare_stream_tombstone_record, prepare_stream_user_record,
    publish_temp_path_atomically, read_file_header, read_stream_creation_nonce, write_file_header,
};
use crate::scan_control::{ScanCancelled, ScanProgressDriver};
use crate::traits::KeyedBlockContract;
use crate::{
    AppendInfo, BlockEvent, BlockKind, CREATION_NONCE_BLOCK_ID, CommitPolicy, Error, FormatSpec,
    IntegrityPolicy, LayoutPreset, MANIFEST_BLOCK_ID, ManifestPolicy, ResourceLimits, Result,
    ScanOptions, ScanProgress, ScanProgressOptions, SnapshotFile, TOMBSTONE_BLOCK_ID, VarveBlock,
    VarveKeyedBlock,
};

const STREAM_WRITER_POISON_CONTEXT: &str = "stream";
const INTERNAL_BLOCK_IDS: [u32; 3] = [
    MANIFEST_BLOCK_ID,
    TOMBSTONE_BLOCK_ID,
    CREATION_NONCE_BLOCK_ID,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamOptions {
    pub limits: ResourceLimits,
    pub state: DiskIndexOptions,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            limits: ResourceLimits::MISSING,
            state: DiskIndexOptions::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchOptions {
    pub max_records: usize,
    pub max_bytes: usize,
}

impl BatchOptions {
    pub(crate) const fn validate(self) -> Result<Self> {
        if self.max_records == 0 {
            return Err(Error::InvalidBatchOptions {
                field: "max_records",
            });
        }
        if self.max_bytes == 0 {
            return Err(Error::InvalidBatchOptions { field: "max_bytes" });
        }
        Ok(self)
    }
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self {
            max_records: 16_384,
            max_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BatchAppendInfo {
    pub records: u64,
    pub first_sequence: Option<u64>,
    pub last_sequence: Option<u64>,
    pub start_offset: u64,
    pub end_offset: u64,
    pub write_calls: u64,
}

#[derive(Debug)]
pub struct BatchAppendError {
    pub written: BatchAppendInfo,
    pub source: Error,
}

impl std::fmt::Display for BatchAppendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "batch append stopped after {} written records: {}",
            self.written.records, self.source
        )
    }
}

impl std::error::Error for BatchAppendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamResidentState {
    pub declared_block_tails: usize,
    pub retained_record_entries: usize,
    pub retained_key_entries: usize,
    pub active_payload_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamBootstrapReport {
    pub records: u64,
    pub scanned_bytes: u64,
}

/// Scans a native stream file and publishes a clean state sidecar for it.
///
/// # Non-cooperating writers (F-09)
///
/// Cooperating writers are serialized by the writer lock this call holds. A
/// process that replaces the primary *pathname* without taking that lock is
/// outside the contract, and no userspace library can make publication atomic
/// against it. This call bounds what such a writer can cause: the primary's
/// identity is verified immediately before publication, the verified object is
/// held open across it so its identity cannot be recycled behind an unlink, and
/// the identity is re-checked once the sidecar is visible. If the replacement
/// landed inside that interval the published sidecar is removed again and the
/// call fails with `DiskIndexError::IdentityMismatch` — or, if the stale
/// sidecar could not be removed, with a `DiskIndexError::CheckpointMismatch`
/// naming what must be deleted. The residual cost is a bootstrap the caller
/// must repeat, never a sidecar a consumer would accept for the wrong primary.
pub fn bootstrap_stream_checkpoint(
    spec: FormatSpec,
    path: impl AsRef<Path>,
    options: StreamOptions,
) -> Result<StreamBootstrapReport> {
    bootstrap_stream_checkpoint_with_progress(spec, path, options, silent_scan_options(), |_| {})
}

pub fn bootstrap_stream_checkpoint_with_progress<F>(
    spec: FormatSpec,
    path: impl AsRef<Path>,
    options: StreamOptions,
    scan: ScanOptions<'_>,
    mut observer: F,
) -> Result<StreamBootstrapReport>
where
    F: FnMut(ScanProgress),
{
    let path = std::fs::canonicalize(path.as_ref())?;
    let _lock = WriterLock::acquire(&path)?;
    let sidecar = state_sidecar_path(&path);
    if sidecar.exists() {
        return Err(state_error(
            crate::disk_index::DiskIndexError::CheckpointMismatch(
                "state sidecar already exists; restore a dirty checkpoint or remove a known-stale clean sidecar explicitly",
            ),
        ));
    }
    let reader = VarveStreamReader::open_native(spec, &path, options)?;
    let mut header_file = reader.snapshot.try_clone_file()?;
    let header_eof = read_file_header(reader.spec, &mut header_file)?;
    let mut progress = ScanProgressDriver::new(scan, header_eof, reader.snapshot.len());
    progress.start(&mut observer).map_err(scan_cancelled)?;
    let mut scanner = NativeStreamScanner::from_snapshot(reader.spec, reader.snapshot.clone())?;
    let mut block_tails = initial_block_tails(reader.spec);
    let mut record_count = 0u64;
    let mut next_sequence = Some(0u64);
    while let Some(entry) = scanner.next_entry()? {
        record_count = record_count
            .checked_add(1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "stream bootstrap record count",
            })?;
        next_sequence = entry.sequence.checked_add(1);
        set_tail(
            &mut block_tails,
            entry.block_id,
            entry.record_offset,
            entry.sequence,
        );
        progress
            .record_completed(scanner.logical_eof(), &mut observer)
            .map_err(scan_cancelled)?;
    }
    if scanner.logical_eof() != reader.snapshot.len() {
        return Err(Error::CorruptTail {
            offset: scanner.logical_eof(),
        });
    }
    let checkpoint = StreamCheckpoint {
        logical_eof: reader.snapshot.len(),
        record_count,
        next_sequence,
        block_tails: block_tails
            .into_iter()
            .filter_map(|(_, tail)| tail)
            .collect(),
    };
    let identity = primary_identity(reader.spec, &reader.snapshot)?;
    let metadata = DiskIndexMetadata::new_state(
        identity,
        checkpoint_frontier(&checkpoint),
        tail_limit_for_spec(reader.spec).map_err(state_error)?,
    )
    .with_primary_generation(primary_generation(
        reader.spec,
        &reader.snapshot,
        checkpoint.logical_eof,
    )?);
    progress.complete(&mut observer).map_err(scan_cancelled)?;
    // The checkpoint is only valid for the exact file object the retained
    // snapshot scanned. Re-resolve the pathname immediately before publishing
    // and refuse to publish for a swapped-in generation.
    let snapshot_identity = opened_file_identity(&reader.snapshot.try_clone_file()?)?;
    // F-09: the verified handle is kept open across publication. Cooperating
    // writers are already excluded by the `WriterLock` held above; against a
    // pathname mutator that ignores that protocol, holding the object open is
    // what stops the operating system from recycling its identity behind an
    // unlink, so the post-publication re-check below cannot be satisfied by a
    // different object wearing the same device/inode pair.
    let verified = OpenOptions::new().read(true).open(&path)?;
    let current_identity = opened_file_identity(&verified)?;
    if current_identity != snapshot_identity {
        return Err(state_error(
            crate::disk_index::DiskIndexError::IdentityMismatch,
        ));
    }
    create_state_store(
        &path,
        state_options(options),
        metadata,
        &checkpoint_disk_tails(&checkpoint),
    )?;
    #[cfg(test)]
    interpose_after_state_publication(&path);
    // F-09: publication is not a single syscall with the check above, so
    // re-resolve once more now that the sidecar is visible. If the primary was
    // replaced in that interval the sidecar just published describes a file
    // object that no longer holds the pathname, and leaving it there would
    // displace whatever newer sidecar the replacement brought with it. Retire
    // it and report a typed mismatch rather than return success or leave a
    // stale sidecar behind silently.
    //
    // Neither step below may turn into a bare `Err` on its own: the sidecar is
    // already published, so an unreadable pathname is resolved *against*
    // publication (fail closed, retire the sidecar) rather than reported as a
    // plain IO failure that would leave a caller unable to tell what is on
    // disk.
    let sidecar = state_sidecar_path(&path);
    let published_identity = OpenOptions::new()
        .read(true)
        .open(&sidecar)
        .ok()
        .and_then(|file| opened_file_identity(&file).ok());
    let republished_identity = OpenOptions::new()
        .read(true)
        .open(&path)
        .ok()
        .and_then(|file| opened_file_identity(&file).ok());
    if republished_identity.as_deref() != Some(snapshot_identity.as_slice()) {
        return Err(state_error(retire_sidecar_for_replaced_primary(
            &sidecar,
            published_identity.as_deref(),
        )));
    }
    drop(verified);
    Ok(StreamBootstrapReport {
        records: record_count,
        scanned_bytes: checkpoint.logical_eof.saturating_sub(header_eof),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StreamTail {
    pub(crate) block_id: u32,
    pub(crate) record_offset: u64,
    pub(crate) sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StreamCheckpoint {
    pub(crate) logical_eof: u64,
    pub(crate) record_count: u64,
    pub(crate) next_sequence: Option<u64>,
    pub(crate) block_tails: Vec<StreamTail>,
}

impl StreamCheckpoint {
    fn validate(&self, spec: FormatSpec, header_len: u64, physical_len: u64) -> Result<()> {
        if self.logical_eof != physical_len || self.logical_eof < header_len {
            return Err(Error::SnapshotRangeOutOfBounds {
                offset: 0,
                len: self.logical_eof,
                snapshot_len: physical_len,
            });
        }
        let expected = initial_block_tails(spec);
        let mut previous_id = None;
        for tail in &self.block_tails {
            if previous_id.is_some_and(|id| id >= tail.block_id)
                || expected
                    .binary_search_by_key(&tail.block_id, |(id, _)| *id)
                    .is_err()
            {
                return Err(state_error(
                    crate::disk_index::DiskIndexError::CheckpointMismatch(
                        "checkpoint block tails do not match the format",
                    ),
                ));
            }
            previous_id = Some(tail.block_id);
            if tail.record_offset < header_len || tail.record_offset >= self.logical_eof {
                return Err(state_error(
                    crate::disk_index::DiskIndexError::CheckpointMismatch(
                        "checkpoint block tail is outside the native snapshot",
                    ),
                ));
            }
            if self.next_sequence.is_some_and(|next| tail.sequence >= next) {
                return Err(state_error(
                    crate::disk_index::DiskIndexError::CheckpointMismatch(
                        "checkpoint block tail sequence is outside the committed frontier",
                    ),
                ));
            }
        }
        Ok(())
    }
}

pub struct VarveStreamReader {
    spec: FormatSpec,
    snapshot: SnapshotFile,
    _state: Option<StreamReaderState>,
}

struct StreamReaderState {
    _snapshot: DiskIndexSnapshot,
    _store: DiskIndexStore,
}

impl VarveStreamReader {
    pub fn open(spec: FormatSpec, path: impl AsRef<Path>, options: StreamOptions) -> Result<Self> {
        let path = std::fs::canonicalize(path.as_ref())?;
        let mut reader = Self::open_native(spec, &path, options)?;
        let physical_len = reader.snapshot.len();
        let identity = primary_identity(reader.spec, &reader.snapshot)?;
        let store = DiskIndexStore::open(state_sidecar_path(&path), state_options(options))
            .map_err(state_error)?;
        let snapshot = store
            .begin_snapshot_with_mode(identity, DiskIndexMode::StateOnly, physical_len)
            .map_err(state_error)?;
        verify_primary_generation(reader.spec, snapshot.primary_generation(), &reader.snapshot)?;
        reader.snapshot = reader.snapshot.with_len(snapshot.committed_eof())?;
        reader._state = Some(StreamReaderState {
            _snapshot: snapshot,
            _store: store,
        });
        Ok(reader)
    }

    pub(crate) fn open_native(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: StreamOptions,
    ) -> Result<Self> {
        let spec = stream_spec(spec, options, false)?;
        let mut file = OpenOptions::new().read(true).open(path)?;
        let captured_len = file.metadata()?.len();
        let header_len = read_file_header(spec, &mut file)?;
        if captured_len < header_len {
            return Err(Error::CorruptTail { offset: header_len });
        }
        let snapshot = SnapshotFile::from_file_with_len(file, captured_len)?;
        Ok(Self {
            spec,
            snapshot,
            _state: None,
        })
    }

    pub(crate) fn pin_logical_len(mut self, logical_len: u64) -> Result<Self> {
        self.snapshot = self.snapshot.with_len(logical_len)?;
        Ok(self)
    }

    pub fn verify_all(&self) -> Result<u64> {
        self.verify_all_with_progress(silent_scan_options(), |_| {})
    }

    pub fn verify_all_with_progress<F>(&self, scan: ScanOptions<'_>, mut observer: F) -> Result<u64>
    where
        F: FnMut(ScanProgress),
    {
        let mut header_file = self.snapshot.try_clone_file()?;
        let header_eof = read_file_header(self.spec, &mut header_file)?;
        let mut progress = ScanProgressDriver::new(scan, header_eof, self.snapshot.len());
        progress.start(&mut observer).map_err(scan_cancelled)?;
        let mut scanner = NativeStreamScanner::from_snapshot(self.spec, self.snapshot.clone())?;
        let mut records = 0u64;
        while scanner.next_entry()?.is_some() {
            records = records
                .checked_add(1)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "verified record count",
                })?;
            progress
                .record_completed(scanner.logical_eof(), &mut observer)
                .map_err(scan_cancelled)?;
        }
        if scanner.logical_eof() != self.snapshot.len() {
            return Err(Error::CorruptTail {
                offset: scanner.logical_eof(),
            });
        }
        progress.complete(&mut observer).map_err(scan_cancelled)?;
        Ok(records)
    }

    pub fn events(&self) -> Result<StreamEvents> {
        Ok(StreamEvents {
            scanner: NativeStreamScanner::from_snapshot(self.spec, self.snapshot.clone())?,
            finished: false,
        })
    }

    pub fn blocks<T: VarveBlock>(&self) -> Result<StreamingBlocks<T>> {
        ensure_registered_block::<T>(self.spec)?;
        if T::KIND == BlockKind::Matrix {
            return Err(Error::StreamingUnsupported);
        }
        // Under CRC policies the sequential scanner would stream every
        // record's payload through the checksum, and the typed decode below
        // would then read the payload a second time. Scan frames without that
        // pre-pass: `read_logical_payload_snapshot` verifies the full record
        // checksum over the one payload read whose buffer feeds the typed
        // decode, and skipped foreign-block payloads are not read at all.
        // `verify_all` remains the whole-file integrity scan.
        let scan_spec = self.spec.with_integrity_policy(IntegrityPolicy::None);
        Ok(StreamingBlocks {
            scanner: NativeStreamScanner::from_snapshot(scan_spec, self.snapshot.clone())?,
            spec: self.spec,
            finished: false,
            _marker: PhantomData,
        })
    }

    pub fn resident_state(&self) -> StreamResidentState {
        resident_state(self.spec)
    }

    pub(crate) const fn spec(&self) -> FormatSpec {
        self.spec
    }

    pub(crate) fn snapshot(&self) -> &SnapshotFile {
        &self.snapshot
    }
}

#[derive(Debug)]
pub struct StreamEvents {
    scanner: NativeStreamScanner,
    finished: bool,
}

impl Iterator for StreamEvents {
    type Item = Result<BlockEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match self.scanner.next_entry() {
            Ok(Some(entry)) => Some(Ok(BlockEvent::from(&entry))),
            Ok(None) => {
                self.finished = true;
                None
            }
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }
}

#[derive(Debug)]
pub struct StreamingBlocks<T> {
    scanner: NativeStreamScanner,
    /// The reader's real spec. The scanner carries an integrity-stripped copy
    /// so the frame scan does not checksum payloads; typed decode validates
    /// each decoded record's checksum against this spec instead.
    spec: FormatSpec,
    finished: bool,
    _marker: PhantomData<T>,
}

impl<T: VarveBlock> Iterator for StreamingBlocks<T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        // The scanner must not checksum payloads: the typed decode below owns
        // the single, checksum-verified payload read.
        debug_assert_eq!(
            self.scanner.spec().integrity_policy,
            IntegrityPolicy::None,
            "streaming typed scan requires an integrity-stripped scanner spec",
        );
        loop {
            let entry = match self.scanner.next_entry() {
                Ok(Some(entry)) => entry,
                Ok(None) => {
                    self.finished = true;
                    return None;
                }
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            };
            if entry.block_id != T::ID {
                continue;
            }
            if entry.block_version != T::VERSION {
                self.finished = true;
                return Some(Err(Error::BlockVersionMismatch {
                    block_id: T::ID,
                    expected: T::VERSION,
                    actual: entry.block_version,
                }));
            }
            let result = (|| {
                let logical_len =
                    entry.logical_payload_len_snapshot(self.spec, self.scanner.snapshot())?;
                let mut budget = MaterializationBudget::new(self.spec);
                budget.consume(logical_len)?;
                // The single payload read: verifies the record checksum over
                // the in-memory buffer and hands that buffer to typed decode.
                let payload =
                    entry.read_logical_payload_snapshot(self.spec, self.scanner.snapshot())?;
                budget.decode(&payload, T::ENDIAN.unwrap_or(self.spec.endian))
            })();
            return Some(result);
        }
    }
}

pub struct VarveStreamWriter {
    spec: FormatSpec,
    file: File,
    snapshot: SnapshotFile,
    sequence: Option<u64>,
    record_count: u64,
    block_tails: Vec<(u32, Option<StreamTail>)>,
    state: Option<StreamWriterState>,
    poisoned: bool,
    _lock: WriterLock,
}

struct StreamWriterState {
    store: DiskIndexStore,
    batch: Option<DiskIndexWriteBatch>,
    chunk_records: usize,
    batch_records: usize,
    batch_last_sequence: Option<u64>,
    dirty: bool,
}

impl StreamWriterState {
    fn new(store: DiskIndexStore, chunk_records: usize) -> Self {
        Self {
            store,
            batch: None,
            chunk_records,
            batch_records: 0,
            batch_last_sequence: None,
            dirty: false,
        }
    }
}

impl Drop for StreamWriterState {
    fn drop(&mut self) {
        // redb waits for active write transactions while closing the database.
        // Abort the bounded batch before field drop reaches the store owner.
        self.batch.take();
    }
}

impl VarveStreamWriter {
    pub fn create(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: StreamOptions,
    ) -> Result<Self> {
        let path = canonical_write_path(path.as_ref())?;
        let mut writer = Self::create_unmanaged(spec, &path, options)?;
        crate::scalable_fault_point("create.native_sync");
        let sync = writer.file.sync_all();
        crate::scalable_fault_point("create.native_sync");
        sync?;
        let checkpoint = writer.checkpoint();
        let identity = primary_identity(writer.spec, &writer.snapshot)?;
        let metadata = DiskIndexMetadata::new_state(
            identity,
            checkpoint_frontier(&checkpoint),
            tail_limit_for_spec(writer.spec).map_err(state_error)?,
        )
        .with_primary_generation(primary_generation(
            writer.spec,
            &writer.snapshot,
            writer.snapshot.len(),
        )?);
        let store = create_state_store(
            &path,
            state_options(options),
            metadata,
            &checkpoint_disk_tails(&checkpoint),
        )?;
        writer.state = Some(StreamWriterState::new(
            store,
            state_chunk_records(writer.spec, state_options(options).batch),
        ));
        Ok(writer)
    }

    pub fn restore_checkpoint_and_open(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: StreamOptions,
    ) -> Result<Self> {
        let path = std::fs::canonicalize(path.as_ref())?;
        let (mut writer, store) = Self::restore_checkpointed(
            spec,
            &path,
            options,
            state_options(options),
            state_sidecar_path(&path),
            DiskIndexMode::StateOnly,
        )?;
        writer.state = Some(StreamWriterState::new(
            store,
            state_chunk_records(writer.spec, state_options(options).batch),
        ));
        Ok(writer)
    }

    pub(crate) fn create_unmanaged(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: StreamOptions,
    ) -> Result<Self> {
        let spec = stream_spec(spec, options, true)?;
        let path = canonical_write_path(path.as_ref())?;
        let mut lock = WriterLock::acquire(&path)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        // Bind the authoritative object lock onto the freshly created file. The
        // acquire-time probe cannot lock a file that did not exist yet, so
        // without this a hard-link alias opened after creation would win a
        // second writer role (DUR-02). Mirrors VarveFile::create_impl and
        // VarveLayoutWriter::create_inner.
        //
        // DUR2-05: the bind must happen before any destructive initialization.
        // Opening with `.truncate(true)` would clear the object inside the
        // pre-bind window, so a losing concurrent creator could truncate the
        // winner's freshly initialized file before its own bind fails. Open
        // without truncate, bind, then truncate through the bound handle.
        lock.bind_native(&file, &path)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        write_file_header(spec, &mut file)?;
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        let mut writer = Self {
            spec,
            file,
            snapshot,
            sequence: Some(0),
            record_count: 0,
            block_tails: initial_block_tails(spec),
            state: None,
            poisoned: false,
            _lock: lock,
        };
        // STO-01: stamp the per-create nonce as the very first record of the
        // log, before anything else can be appended. It has to live inside the
        // primary because an equal-length in-place rewrite preserves the OS
        // object, the header bytes and the schema hash — everything except the
        // primary's own content. One record at create; zero cost per append.
        {
            let sequence = writer.next_sequence()?;
            let offset = writer.snapshot.len();
            let record = prepare_stream_creation_nonce_record(
                spec,
                fresh_stream_creation_nonce(),
                sequence,
                offset,
            )?;
            writer.append_prepared(CREATION_NONCE_BLOCK_ID, record)?;
        }
        if spec.manifest_policy == ManifestPolicy::Embedded {
            let sequence = writer.next_sequence()?;
            let offset = writer.snapshot.len();
            let record = prepare_stream_manifest_record(spec, sequence, offset)?;
            writer.append_prepared(MANIFEST_BLOCK_ID, record)?;
        }
        Ok(writer)
    }

    pub fn open(spec: FormatSpec, path: impl AsRef<Path>, options: StreamOptions) -> Result<Self> {
        let path = std::fs::canonicalize(path.as_ref())?;
        let (mut writer, store) =
            Self::open_checkpointed(spec, &path, options, |identity, physical_len| {
                let store = DiskIndexStore::open(state_sidecar_path(&path), state_options(options))
                    .map_err(state_error)?;
                let state = store
                    .validate_clean_writer(
                        identity,
                        DiskIndexMode::StateOnly,
                        physical_len,
                        tail_limit_for_spec(spec).map_err(state_error)?,
                    )
                    .map_err(state_error)?;
                Ok((
                    stream_checkpoint_from_state(&state),
                    state.metadata.primary_generation,
                    store,
                ))
            })?;
        writer.state = Some(StreamWriterState::new(
            store,
            state_chunk_records(writer.spec, state_options(options).batch),
        ));
        Ok(writer)
    }

    pub(crate) fn open_checkpointed<T>(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: StreamOptions,
        load_checkpoint: impl FnOnce(
            crate::disk_index::DiskIndexIdentity,
            u64,
        )
            -> Result<(StreamCheckpoint, DiskIndexPrimaryGeneration, T)>,
    ) -> Result<(Self, T)> {
        let spec = stream_spec(spec, options, false)?;
        let path = std::fs::canonicalize(path.as_ref())?;
        let lock = WriterLock::acquire(&path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let physical_len = file.metadata()?.len();
        let header_len = read_file_header(spec, &mut file)?;
        let physical_snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, physical_len)?;
        let identity = primary_identity(spec, &physical_snapshot)?;
        let (checkpoint, generation, loaded) = load_checkpoint(identity, physical_len)?;
        // STO-01: the sidecar must belong to this generation of the primary,
        // not merely to the same file object with the same header bytes.
        verify_primary_generation(spec, generation, &physical_snapshot)?;
        checkpoint.validate(spec, header_len, physical_len)?;
        file.seek(SeekFrom::Start(checkpoint.logical_eof))?;
        let snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, checkpoint.logical_eof)?;
        Ok((
            Self {
                spec,
                file,
                snapshot,
                sequence: checkpoint.next_sequence,
                record_count: checkpoint.record_count,
                block_tails: block_tails_from_checkpoint(spec, &checkpoint.block_tails),
                state: None,
                poisoned: false,
                _lock: lock,
            },
            loaded,
        ))
    }

    pub(crate) fn restore_checkpointed(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: StreamOptions,
        index_options: DiskIndexOptions,
        sidecar: impl AsRef<Path>,
        mode: DiskIndexMode,
    ) -> Result<(Self, DiskIndexStore)> {
        let spec = stream_spec(spec, options, false)?;
        let path = std::fs::canonicalize(path.as_ref())?;
        let lock = WriterLock::acquire(&path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let physical_len = file.metadata()?.len();
        let header_len = read_file_header(spec, &mut file)?;
        let physical_snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, physical_len)?;
        let identity = primary_identity(spec, &physical_snapshot)?;
        let store = DiskIndexStore::open(sidecar, index_options).map_err(state_error)?;
        let guard = store
            .stage_restore(
                identity,
                mode,
                physical_len,
                tail_limit_for_spec(spec).map_err(state_error)?,
            )
            .map_err(state_error)?;
        verify_primary_generation(spec, guard.primary_generation(), &physical_snapshot)?;
        let checkpoint = stream_checkpoint_from_frontier(guard.frontier(), guard.tails());
        let base_eof = guard.base_eof();
        checkpoint.validate(spec, header_len, base_eof)?;
        crate::scalable_fault_point("restore.native_truncate");
        let truncate = file.set_len(base_eof);
        crate::scalable_fault_point("restore.native_truncate");
        truncate?;
        crate::scalable_fault_point("restore.native_sync");
        let sync = file.sync_all();
        crate::scalable_fault_point("restore.native_sync");
        sync?;
        let observed_len = file.metadata()?.len();
        guard
            .commit_after_native_sync(observed_len)
            .map_err(state_error)?;
        file.seek(SeekFrom::Start(base_eof))?;
        let snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, base_eof)?;
        Ok((
            Self {
                spec,
                file,
                snapshot,
                sequence: checkpoint.next_sequence,
                record_count: checkpoint.record_count,
                block_tails: block_tails_from_checkpoint(spec, &checkpoint.block_tails),
                state: None,
                poisoned: false,
                _lock: lock,
            },
            store,
        ))
    }

    pub fn push_info<T: VarveBlock>(&mut self, value: &T) -> Result<AppendInfo> {
        if self.spec.index_policy.keyed_offset_chain && T::IS_KEYED {
            return Err(Error::StreamingUnsupported);
        }
        self.push_with_prev_key_info(value, None)
    }

    pub fn push_iter<T, I>(
        &mut self,
        values: I,
        options: BatchOptions,
    ) -> std::result::Result<BatchAppendInfo, BatchAppendError>
    where
        T: VarveBlock,
        I: IntoIterator,
        I::Item: Borrow<T>,
    {
        let options = options.validate().map_err(|source| BatchAppendError {
            written: BatchAppendInfo {
                start_offset: self.snapshot.len(),
                end_offset: self.snapshot.len(),
                ..BatchAppendInfo::default()
            },
            source,
        })?;
        let start_offset = self.snapshot.len();
        let mut written = BatchAppendInfo {
            start_offset,
            end_offset: start_offset,
            ..BatchAppendInfo::default()
        };
        if let Err(source) = self.push_iter_inner::<T, I>(values, options, &mut written) {
            if written.records != 0 {
                self.poisoned = true;
            }
            return Err(BatchAppendError { written, source });
        }
        Ok(written)
    }

    pub fn push_with_prev_key_info<T: VarveBlock>(
        &mut self,
        value: &T,
        previous: Option<u64>,
    ) -> Result<AppendInfo> {
        self.ensure_writable()?;
        ensure_registered_block::<T>(self.spec)?;
        if self.spec.index_policy.keyed_offset_chain && T::IS_KEYED {
            return Err(Error::StreamingUnsupported);
        }
        if T::ID >= 0xFFFF_FF00 {
            return Err(Error::ReservedBlockId(T::ID));
        }
        if T::KIND == BlockKind::Matrix {
            return Err(Error::StreamingUnsupported);
        }
        let sequence = self.next_sequence()?;
        let offset = self.snapshot.len();
        let previous_block = self.previous_block(T::ID);
        let record = prepare_stream_user_record(
            self.spec,
            value,
            sequence,
            offset,
            previous_block,
            previous,
        )?;
        self.append_prepared(T::ID, record)
    }

    pub fn delete_with_prev_key_info<T: VarveKeyedBlock>(
        &mut self,
        key: &T::Key,
        previous: Option<u64>,
    ) -> Result<AppendInfo> {
        // API2-03: every public keyed generic entry point evaluates the
        // compile-time keyedness contract post-monomorphization; the
        // registration below stays as the runtime backstop.
        let () = KeyedBlockContract::<T>::OK;
        self.ensure_writable()?;
        ensure_registered_block::<T>(self.spec)?;
        if self.spec.index_policy.keyed_offset_chain {
            return Err(Error::StreamingUnsupported);
        }
        let sequence = self.next_sequence()?;
        let offset = self.snapshot.len();
        let record = prepare_stream_tombstone_record::<T>(
            self.spec,
            key,
            sequence,
            offset,
            self.previous_block(TOMBSTONE_BLOCK_ID),
            previous,
        )?;
        self.append_prepared(TOMBSTONE_BLOCK_ID, record)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.ensure_writable()?;
        self.commit_state_chunk()?;
        self.file.flush()?;
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        self.ensure_writable()?;
        self.commit_state_chunk()?;
        crate::scalable_fault_point("sync.native_sync");
        let sync = self.file.sync_all();
        crate::scalable_fault_point("sync.native_sync");
        sync?;
        let frontier = checkpoint_frontier(&self.checkpoint());
        if let Some(state) = self.state.as_mut()
            && state.dirty
        {
            if let Err(error) = state.store.publish_clean(frontier) {
                self.poisoned = true;
                return Err(state_error(error));
            }
            state.dirty = false;
        }
        Ok(())
    }

    pub fn resident_state(&self) -> StreamResidentState {
        resident_state(self.spec)
    }

    pub(crate) const fn spec(&self) -> FormatSpec {
        self.spec
    }

    pub(crate) fn snapshot(&self) -> &SnapshotFile {
        &self.snapshot
    }

    pub(crate) fn checkpoint(&self) -> StreamCheckpoint {
        StreamCheckpoint {
            logical_eof: self.snapshot.len(),
            record_count: self.record_count,
            next_sequence: self.sequence,
            block_tails: self
                .block_tails
                .iter()
                .filter_map(|(_, tail)| tail.clone())
                .collect(),
        }
    }

    fn push_iter_inner<T, I>(
        &mut self,
        values: I,
        options: BatchOptions,
        written: &mut BatchAppendInfo,
    ) -> Result<()>
    where
        T: VarveBlock,
        I: IntoIterator,
        I::Item: Borrow<T>,
    {
        self.ensure_writable()?;
        ensure_registered_block::<T>(self.spec)?;
        if T::ID >= 0xFFFF_FF00 {
            return Err(Error::ReservedBlockId(T::ID));
        }
        if T::KIND == BlockKind::Matrix
            || (self.spec.index_policy.keyed_offset_chain && T::IS_KEYED)
        {
            return Err(Error::StreamingUnsupported);
        }

        let max_records = self.state.as_ref().map_or(options.max_records, |state| {
            options.max_records.min(state.chunk_records)
        });
        let mut bytes = Vec::new();
        let mut records: Vec<(u32, AppendInfo)> = Vec::new();
        let mut next_sequence = self.sequence;
        let mut next_offset = self.snapshot.len();
        let mut previous_block = self.previous_block(T::ID);

        for value in values {
            let sequence = next_sequence.ok_or(Error::SequenceExhausted)?;
            let record = prepare_stream_user_record(
                self.spec,
                value.borrow(),
                sequence,
                next_offset,
                previous_block,
                None,
            )?;
            let exceeds_records = records.len() >= max_records;
            let exceeds_bytes = !bytes.is_empty()
                && bytes
                    .len()
                    .checked_add(record.bytes.len())
                    .is_none_or(|len| len > options.max_bytes);
            if exceeds_records || exceeds_bytes {
                self.append_prepared_chunk(&bytes, &records)?;
                update_batch_summary(written, &records, self.snapshot.len())?;
                bytes.clear();
                records.clear();
            }

            next_offset = next_offset.checked_add(record.bytes.len() as u64).ok_or(
                Error::ResourceArithmeticOverflow {
                    resource: "batch native offset",
                },
            )?;
            next_sequence = sequence.checked_add(1);
            previous_block = Some(record.info.record_offset);
            bytes
                .try_reserve(record.bytes.len())
                .map_err(|_| Error::AllocationFailed {
                    resource: "batch record bytes",
                    requested: record.bytes.len() as u64,
                })?;
            records
                .try_reserve(1)
                .map_err(|_| Error::AllocationFailed {
                    resource: "batch record descriptors",
                    requested: std::mem::size_of::<(u32, AppendInfo)>() as u64,
                })?;
            bytes.extend_from_slice(&record.bytes);
            records.push((T::ID, record.info));

            if bytes.len() >= options.max_bytes || records.len() >= max_records {
                self.append_prepared_chunk(&bytes, &records)?;
                update_batch_summary(written, &records, self.snapshot.len())?;
                bytes.clear();
                records.clear();
            }
        }
        if !records.is_empty() {
            self.append_prepared_chunk(&bytes, &records)?;
            update_batch_summary(written, &records, self.snapshot.len())?;
        }
        Ok(())
    }

    fn append_prepared(
        &mut self,
        block_id: u32,
        record: PreparedStreamRecord,
    ) -> Result<AppendInfo> {
        let info = record.info;
        self.append_prepared_chunk(&record.bytes, &[(block_id, info)])?;
        Ok(info)
    }

    pub(crate) fn append_prepared_chunk(
        &mut self,
        bytes: &[u8],
        records: &[(u32, AppendInfo)],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let record_delta =
            u64::try_from(records.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?;
        let next_count = self.record_count.checked_add(record_delta).ok_or(
            Error::ResourceArithmeticOverflow {
                resource: "record count",
            },
        )?;
        let eof = self.snapshot.len();
        if records[0].1.record_offset != eof {
            return Err(Error::InvalidCanonicalEncoding(
                "prepared batch does not begin at native eof",
            ));
        }
        let byte_len =
            u64::try_from(bytes.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        let new_eof = eof
            .checked_add(byte_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "file length",
            })?;
        self.reserve_state_chunk(records.len())?;
        self.file.seek(SeekFrom::Start(eof))?;
        crate::scalable_fault_point("append.native_chunk_write");
        let write = self.file.write_all(bytes);
        crate::scalable_fault_point("append.native_chunk_write");
        if let Err(error) = write {
            // Nothing was staged for this chunk yet, so the pending sidecar
            // transaction still matches the rolled-back native EOF.
            let truncate_result = self.file.set_len(eof);
            let seek_result = self.file.seek(SeekFrom::Start(eof));
            if let Some(source) = truncate_result.err().or_else(|| seek_result.err()) {
                self.poisoned = true;
                return Err(Error::WriteRollbackFailed {
                    operation: "append stream record",
                    source,
                });
            }
            return Err(error.into());
        }
        self.snapshot = match self.snapshot.with_len(new_eof) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let truncate_result = self.file.set_len(eof);
                let seek_result = self.file.seek(SeekFrom::Start(eof));
                if let Some(source) = truncate_result.err().or_else(|| seek_result.err()) {
                    self.poisoned = true;
                    return Err(Error::WriteRollbackFailed {
                        operation: "append stream record",
                        source,
                    });
                }
                return Err(error);
            }
        };
        let last_sequence = records.last().expect("non-empty prepared chunk").1.sequence;
        self.sequence = last_sequence.checked_add(1);
        self.record_count = next_count;
        for (block_id, info) in records {
            set_tail(
                &mut self.block_tails,
                *block_id,
                info.record_offset,
                info.sequence,
            );
        }
        // The records are published in the native file, so any sidecar failure
        // from here on must poison the writer.
        if let Err(error) = self.stage_state_records(records, new_eof) {
            self.poisoned = true;
            return Err(Error::PublishedButIndexStale {
                sequence: last_sequence,
                source: Box::new(error),
            });
        }
        if self
            .state
            .as_ref()
            .is_some_and(|state| state.batch_records >= state.chunk_records)
        {
            self.commit_state_chunk()?;
        }
        Ok(())
    }

    /// Opens the writer's dirty generation and makes sure a pending sidecar
    /// transaction with room for `incoming` coverage items exists. Runs before
    /// the native write so setup failures leave the record unpublished.
    fn reserve_state_chunk(&mut self, incoming: usize) -> Result<()> {
        let Some(state) = self.state.as_mut() else {
            return Ok(());
        };
        if !state.dirty {
            state.store.begin_generation().map_err(state_error)?;
            state.dirty = true;
        }
        if state.batch.is_some()
            && state.batch_records.saturating_add(incoming) > state.chunk_records
        {
            self.commit_state_chunk()?;
        }
        if let Some(state) = self.state.as_mut()
            && state.batch.is_none()
        {
            state.batch = Some(state.store.begin_write_batch().map_err(state_error)?);
        }
        Ok(())
    }

    fn stage_state_records(&mut self, records: &[(u32, AppendInfo)], final_eof: u64) -> Result<()> {
        let Some(state) = self.state.as_mut() else {
            return Ok(());
        };
        let chain = self.spec.index_policy.block_offset_chain;
        let batch = state.batch.as_mut().ok_or(Error::InvalidIndexCheckpoint)?;
        for (index, (block_id, info)) in records.iter().enumerate() {
            let end = records
                .get(index + 1)
                .map_or(final_eof, |(_, next)| next.record_offset);
            // Only the final record of each block in this chunk hands a tail to
            // the sidecar; intermediate tails would be superseded inside the
            // same transaction anyway.
            let tail = (chain
                && records[index + 1..]
                    .iter()
                    .all(|(candidate, _)| candidate != block_id))
            .then_some(DiskIndexTail {
                block_id: *block_id,
                record_offset: info.record_offset,
                sequence: info.sequence,
            });
            batch
                .advance_coverage_with_tail(info.record_offset, end, info.sequence, tail)
                .map_err(state_error)?;
            state.batch_records += 1;
            state.batch_last_sequence = Some(info.sequence);
        }
        Ok(())
    }

    fn commit_state_chunk(&mut self) -> Result<()> {
        // STO-01: re-stamp the generation witness while its window is still
        // filling. Once the window is full it is frozen for the file's life,
        // so steady-state appends pay nothing here.
        let pending = match self.state.as_ref().and_then(|state| state.batch.as_ref()) {
            Some(batch) if batch.primary_generation().len < PRIMARY_GENERATION_WINDOW => Some(
                primary_generation(self.spec, &self.snapshot, self.snapshot.len())?,
            ),
            _ => None,
        };
        let Some(state) = self.state.as_mut() else {
            return Ok(());
        };
        if let (Some(generation), Some(batch)) = (pending, state.batch.as_mut()) {
            batch
                .set_primary_generation(generation)
                .map_err(state_error)?;
        }
        let Some(batch) = state.batch.take() else {
            return Ok(());
        };
        let sequence = state.batch_last_sequence.take();
        state.batch_records = 0;
        if let Err(error) = batch.commit() {
            self.poisoned = true;
            return match sequence {
                // The staged records were already published natively.
                Some(sequence) => Err(Error::PublishedButIndexStale {
                    sequence,
                    source: Box::new(state_error(error)),
                }),
                None => Err(state_error(error)),
            };
        }
        Ok(())
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.poisoned {
            Err(Error::WriterPoisoned(STREAM_WRITER_POISON_CONTEXT))
        } else {
            Ok(())
        }
    }

    pub(crate) fn next_sequence(&self) -> Result<u64> {
        self.sequence.ok_or(Error::SequenceExhausted)
    }

    pub(crate) fn previous_block(&self, block_id: u32) -> Option<u64> {
        if !self.spec.index_policy.block_offset_chain {
            return None;
        }
        // `block_tails` is sorted by block id; see `initial_block_tails`.
        self.block_tails
            .binary_search_by_key(&block_id, |(candidate, _)| *candidate)
            .ok()
            .and_then(|index| {
                self.block_tails[index]
                    .1
                    .as_ref()
                    .map(|tail| tail.record_offset)
            })
    }
}

fn stream_spec(spec: FormatSpec, options: StreamOptions, create: bool) -> Result<FormatSpec> {
    let spec = spec
        .with_resource_limits(options.limits)
        .resolve_entrypoint();
    spec.validate()?;
    let _ = create;
    if spec.has_matrix_blocks()
        || spec.commit_policy.is_transaction_marker()
        || spec.index_policy.checkpoint_on_flush
        || spec.layout.preset != LayoutPreset::VarveNative
    {
        return Err(Error::StreamingUnsupported);
    }
    if !matches!(
        spec.commit_policy,
        CommitPolicy::None | CommitPolicy::RecordFooter
    ) {
        return Err(Error::StreamingUnsupported);
    }
    Ok(spec)
}

pub(crate) fn update_batch_summary(
    summary: &mut BatchAppendInfo,
    records: &[(u32, AppendInfo)],
    end_offset: u64,
) -> Result<()> {
    let first = records.first().expect("non-empty prepared chunk").1;
    let last = records.last().expect("non-empty prepared chunk").1;
    summary.first_sequence.get_or_insert(first.sequence);
    summary.last_sequence = Some(last.sequence);
    summary.records = summary
        .records
        .checked_add(u64::try_from(records.len()).map_err(|_| {
            Error::ResourceArithmeticOverflow {
                resource: "batch record count",
            }
        })?)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "batch record count",
        })?;
    summary.end_offset = end_offset;
    summary.write_calls =
        summary
            .write_calls
            .checked_add(1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "batch write call count",
            })?;
    Ok(())
}

fn initial_block_tails(spec: FormatSpec) -> Vec<(u32, Option<StreamTail>)> {
    if !spec.index_policy.block_offset_chain {
        return Vec::new();
    }
    let mut tails = Vec::with_capacity(spec.blocks.len() + INTERNAL_BLOCK_IDS.len());
    tails.extend(spec.blocks.iter().map(|block| (block.id, None)));
    tails.extend(INTERNAL_BLOCK_IDS.map(|block_id| (block_id, None)));
    tails.sort_unstable_by_key(|(block_id, _)| *block_id);
    tails
}

fn block_tails_from_checkpoint(
    spec: FormatSpec,
    checkpoint: &[StreamTail],
) -> Vec<(u32, Option<StreamTail>)> {
    let mut tails = initial_block_tails(spec);
    for tail in checkpoint {
        if let Ok(index) = tails.binary_search_by_key(&tail.block_id, |(block_id, _)| *block_id) {
            tails[index].1 = Some(tail.clone());
        }
    }
    tails
}

fn set_tail(tails: &mut [(u32, Option<StreamTail>)], block_id: u32, offset: u64, sequence: u64) {
    // `tails` is sorted by block id; see `initial_block_tails`.
    if let Ok(index) = tails.binary_search_by_key(&block_id, |(candidate, _)| *candidate) {
        tails[index].1 = Some(StreamTail {
            block_id,
            record_offset: offset,
            sequence,
        });
    }
}

fn resident_state(spec: FormatSpec) -> StreamResidentState {
    StreamResidentState {
        declared_block_tails: if spec.index_policy.block_offset_chain {
            spec.blocks.len()
        } else {
            0
        },
        retained_record_entries: 0,
        retained_key_entries: 0,
        active_payload_bytes: 0,
    }
}

fn canonical_write_path(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(std::fs::canonicalize(path)?);
    }
    let parent = std::fs::canonicalize(path.parent().unwrap_or_else(|| Path::new(".")))?;
    let name = path
        .file_name()
        .ok_or(Error::InvalidFormatSpec("file path has no name"))?;
    Ok(parent.join(name))
}

fn state_options(options: StreamOptions) -> DiskIndexOptions {
    options.state
}

fn state_chunk_records(spec: FormatSpec, batch: DiskIndexBatchOptions) -> usize {
    let per_record = batch_item_fixed_bytes(spec.index_policy.block_offset_chain);
    batch.max_records.min(batch.max_bytes / per_record).max(1)
}

/// Test-only interposition point between sidecar publication and the
/// post-publication identity re-check (F-09), where a non-cooperating pathname
/// mutator would have to land to make the published sidecar stale.
///
/// Armed hooks are held in a path-keyed table, and consumption removes **only**
/// the entry whose primary matches (F-04). An earlier single-slot cell let one
/// test's registration silently replace another still-armed test's hook, which
/// made the parallel library run nondeterministic.
#[cfg(test)]
pub(crate) fn interpose_after_state_publication(primary: &Path) {
    let mut armed = PUBLICATION_INTERPOSITION
        .lock()
        .expect("publication interposition");
    let Some(position) = armed
        .iter()
        .position(|(target, _)| target.as_path() == primary)
    else {
        return;
    };
    let (_, hook) = armed.swap_remove(position);
    drop(armed);
    hook();
}

#[cfg(test)]
type PublicationInterposition = std::sync::Mutex<Vec<(PathBuf, Box<dyn FnOnce() + Send>)>>;

#[cfg(test)]
static PUBLICATION_INTERPOSITION: PublicationInterposition = std::sync::Mutex::new(Vec::new());

/// Arms a test-only hook for one primary path.
///
/// Registering a second hook for a path that still has one armed is a test
/// defect - the first hook would be lost silently - so it panics rather than
/// overwriting. Hooks for *different* paths coexist, which is what lets the
/// stream and indexed sidecar tests run in parallel.
#[cfg(test)]
pub(crate) fn set_publication_interposition(primary: &Path, hook: Box<dyn FnOnce() + Send>) {
    let mut armed = PUBLICATION_INTERPOSITION
        .lock()
        .expect("publication interposition");
    assert!(
        !armed.iter().any(|(target, _)| target.as_path() == primary),
        "a publication interposition is already armed for {}",
        primary.display(),
    );
    armed.push((primary.to_path_buf(), hook));
}

/// Retires a sidecar that was published for a primary the pathname stopped
/// naming during publication, and reports which of the two outcomes happened
/// (F-09).
///
/// The removal is identity-checked and directory-confined, exactly like the
/// self-test's destructive cleanup, so this can never delete a sidecar some
/// other party published at that pathname. Whether the stale sidecar was
/// removed or had to be left in place changes what the caller must do next, so
/// the two cases are different typed errors rather than one.
///
/// This does not make sidecar publication atomic with respect to a
/// non-cooperating pathname mutator; nothing available to a userspace library
/// can. It bounds the damage such a writer can do to a rebuild the caller must
/// repeat, which is the documented non-cooperating-writer boundary: Varve
/// serializes cooperating writers with [`crate::file::WriterLock`], and a
/// process that replaces the primary without it is outside that contract.
pub(crate) fn retire_sidecar_for_replaced_primary(
    sidecar: &Path,
    published_identity: Option<&[u8]>,
) -> crate::disk_index::DiskIndexError {
    let Some(published_identity) = published_identity else {
        // The sidecar's own identity could not be captured, so removing it
        // would be a deletion by unverified pathname — the very thing this
        // helper exists to avoid. Report what the caller must clean up.
        return crate::disk_index::DiskIndexError::CheckpointMismatch(
            "the primary was replaced while its sidecar was published; the sidecar this call \
             published is stale and could not be identified for removal, so delete it before \
             reusing the file",
        );
    };
    match crate::diagnostics::remove_path_if_same_object(sidecar, published_identity) {
        // Removed, or already replaced by whoever else is writing this
        // pathname: either way this call left no sidecar of its own behind.
        crate::diagnostics::ObjectRemoval::Removed
        | crate::diagnostics::ObjectRemoval::NotOwned => {
            crate::disk_index::DiskIndexError::IdentityMismatch
        }
        crate::diagnostics::ObjectRemoval::Refused(_)
        | crate::diagnostics::ObjectRemoval::Failed => {
            crate::disk_index::DiskIndexError::CheckpointMismatch(
                "the primary was replaced while its sidecar was published; the sidecar this call \
                 published is stale and could not be removed, so delete it before reusing the file",
            )
        }
    }
}

fn state_error(error: crate::disk_index::DiskIndexError) -> Error {
    match error {
        crate::disk_index::DiskIndexError::Busy => Error::IndexBusy,
        error => Error::DiskIndex(Box::new(error)),
    }
}

pub(crate) const fn silent_scan_options() -> ScanOptions<'static> {
    ScanOptions {
        progress: ScanProgressOptions {
            every_records: None,
            every_bytes: None,
        },
        cancellation: None,
    }
}

pub(crate) const fn scan_cancelled(cancelled: ScanCancelled) -> Error {
    Error::ScanCancelled {
        progress: cancelled.progress,
    }
}

fn checkpoint_frontier(checkpoint: &StreamCheckpoint) -> DiskIndexFrontier {
    DiskIndexFrontier::new(
        checkpoint.logical_eof,
        checkpoint.record_count,
        checkpoint.next_sequence,
    )
}

fn checkpoint_disk_tails(checkpoint: &StreamCheckpoint) -> Vec<DiskIndexTail> {
    checkpoint
        .block_tails
        .iter()
        .map(|tail| DiskIndexTail {
            block_id: tail.block_id,
            record_offset: tail.record_offset,
            sequence: tail.sequence,
        })
        .collect()
}

fn stream_checkpoint_from_state(
    state: &crate::disk_index::DiskIndexPersistentState,
) -> StreamCheckpoint {
    stream_checkpoint_from_frontier(state.metadata.committed, &state.tails)
}

fn stream_checkpoint_from_frontier(
    frontier: DiskIndexFrontier,
    tails: &[DiskIndexTail],
) -> StreamCheckpoint {
    StreamCheckpoint {
        logical_eof: frontier.eof,
        record_count: frontier.record_count,
        next_sequence: frontier.next_sequence,
        block_tails: tails
            .iter()
            .map(|tail| StreamTail {
                block_id: tail.block_id,
                record_offset: tail.record_offset,
                sequence: tail.sequence,
            })
            .collect(),
    }
}

fn create_state_store(
    native_path: &Path,
    options: DiskIndexOptions,
    metadata: DiskIndexMetadata,
    tails: &[DiskIndexTail],
) -> Result<DiskIndexStore> {
    let sidecar = state_sidecar_path(native_path);
    let parent = sidecar.parent().unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::Builder::new()
        .prefix(".varve-state-")
        .suffix(".vks.tmp")
        .tempfile_in(parent)?
        .into_temp_path();
    fs::remove_file(&temporary)?;
    crate::scalable_fault_point("create.sidecar_complete");
    let store = DiskIndexStore::create_with_tails(&temporary, options, metadata, tails)
        .map_err(state_error);
    crate::scalable_fault_point("create.sidecar_complete");
    let store = store?;
    drop(store);
    // DUR2-01: the temp guard is handed to publication so an indeterminate
    // outcome preserves the replacement for reconciliation.
    match publish_temp_path_atomically(temporary, &sidecar)? {
        ReplaceDurability::Durable => {}
        ReplaceDurability::ParentSyncPending(sync_error) => {
            // Publication already happened: keep the published state sidecar
            // and report the pending parent-directory durability instead of
            // silently promoting it to full success.
            return Err(Error::PublishedButParentSyncPending {
                path: sidecar.display().to_string(),
                source: Box::new(sync_error),
            });
        }
    }
    DiskIndexStore::open_validated(&sidecar, options, metadata.identity, metadata.mode)
        .map_err(state_error)
}

/// Computes the primary generation witness for `snapshot` (STO-01).
///
/// The witness has two independent parts:
///
/// * The **per-create nonce** stamped inside the primary at create. This is
///   what makes the witness a true generation identity: an equal-length
///   in-place rewrite preserves the OS object, the header bytes and the schema
///   hash, but a primary created by a different `create` call carries a
///   different nonce no matter how much content the two generations share.
///   It is folded in explicitly rather than relied upon to fall inside the
///   content window below, so the guarantee does not depend on record sizes.
/// * A **content digest** over the first `min(eof, window)` bytes, bounded by
///   [`PRIMARY_GENERATION_WINDOW`]. This is defence in depth against accidental
///   truncation/overwrite of a primary that carries no nonce (legacy primaries,
///   and primaries bootstrapped from a resident `VarveFile`). It is a CRC-based
///   accident detector, not an adversary-resistant hash.
///
/// Both parts are bounded work: one small read of the leading window plus one
/// bounded point read of the leading record, regardless of file size. This runs
/// at create, at open, and on commits only while the window is still filling —
/// never per record and never per lookup.
pub(crate) fn primary_generation(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    len: u64,
) -> Result<DiskIndexPrimaryGeneration> {
    crate::disk_index::primary_generation_scan();
    let len = len.min(PRIMARY_GENERATION_WINDOW);
    let prefix = snapshot.read_vec_at(0, len, PRIMARY_GENERATION_WINDOW, "primary generation")?;
    let header_len = {
        let mut file = snapshot.try_clone_file()?;
        read_file_header(spec, &mut file)?
    };
    // A primary that carries no nonce folds in a zero value *and* a zero
    // presence flag, so "carries no nonce" can never collide with "carries an
    // all-zero nonce".
    let (nonce_present, nonce) = match read_stream_creation_nonce(spec, snapshot, header_len) {
        Some(nonce) => (1u8, nonce),
        None => (0u8, [0u8; 16]),
    };
    let mut digest = [0u8; 32];
    for lane in 0..8u32 {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(b"varve-primary-generation-v2");
        hasher.update(&lane.to_le_bytes());
        hasher.update(&[nonce_present]);
        hasher.update(&nonce);
        hasher.update(&len.to_le_bytes());
        hasher.update(&prefix);
        digest[(lane as usize) * 4..(lane as usize + 1) * 4]
            .copy_from_slice(&hasher.finalize().to_le_bytes());
    }
    Ok(DiskIndexPrimaryGeneration {
        len,
        digest: DiskIndexDigest::from_bytes(digest),
    })
}

/// Rejects a sidecar that was published against a different generation of the
/// same file object: an equal-length in-place rewrite preserves OS identity,
/// schema hash and header bytes, but not the recorded leading bytes.
pub(crate) fn verify_primary_generation(
    spec: FormatSpec,
    recorded: DiskIndexPrimaryGeneration,
    snapshot: &SnapshotFile,
) -> Result<()> {
    if recorded.len > snapshot.len() {
        return Err(state_error(DiskIndexError::PrimaryGenerationMismatch));
    }
    if primary_generation(spec, snapshot, recorded.len)? != recorded {
        return Err(state_error(DiskIndexError::PrimaryGenerationMismatch));
    }
    Ok(())
}

pub(crate) fn primary_identity(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
) -> Result<crate::disk_index::DiskIndexIdentity> {
    let mut file = snapshot.try_clone_file()?;
    let header_len = read_file_header(spec, &mut file)?;
    let header = snapshot.read_vec_at(0, header_len, header_len, "file header")?;
    let object_identity = opened_file_identity(&file)?;
    let schema_hash = if spec.schema_hash == 0 {
        spec.computed_schema_hash()
    } else {
        spec.schema_hash
    };
    let mut fingerprint = [0u8; 32];
    for lane in 0..8u32 {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&lane.to_le_bytes());
        hasher.update(&schema_hash.to_le_bytes());
        hasher.update(&object_identity);
        hasher.update(&header);
        fingerprint[(lane as usize) * 4..(lane as usize + 1) * 4]
            .copy_from_slice(&hasher.finalize().to_le_bytes());
    }
    Ok(crate::disk_index::DiskIndexIdentity {
        schema_hash,
        primary_fingerprint: fingerprint,
    })
}

#[cfg(unix)]
fn opened_file_identity(file: &std::fs::File) -> Result<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&metadata.dev().to_le_bytes());
    bytes.extend_from_slice(&metadata.ino().to_le_bytes());
    Ok(bytes)
}

#[cfg(windows)]
fn opened_file_identity(file: &std::fs::File) -> Result<Vec<u8>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // The handle belongs to `file`, the output points to initialized writable storage,
    // and the OS call does not outlive either value.
    let ok =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, information.as_mut_ptr()) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // A successful call initializes every field of BY_HANDLE_FILE_INFORMATION.
    let information = unsafe { information.assume_init() };
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    let mut bytes = Vec::with_capacity(12);
    bytes.extend_from_slice(&information.dwVolumeSerialNumber.to_le_bytes());
    bytes.extend_from_slice(&file_index.to_le_bytes());
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BlockDescriptor, Decoder, Encoder, Endian, IndexPolicy, IntegrityPolicy, ReadLimits,
        RecoveryPolicy, TransactionMarkerMode, VarveDecode, VarveEncode, WireType,
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestBlock(u64);

    impl VarveEncode for TestBlock {
        const WIRE_TYPE: WireType = WireType::U64;

        fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
            self.0.encode_varve(encoder)
        }
    }

    impl VarveDecode for TestBlock {
        const WIRE_TYPE: WireType = WireType::U64;

        fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
            Ok(Self(u64::decode_varve(decoder)?))
        }
    }

    impl VarveBlock for TestBlock {
        const ID: u32 = 17;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = false;
        const SCHEMA_FINGERPRINT: u64 = 0x5354_5245_414D_0017;
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct KeyedTestBlock(u64);

    impl VarveEncode for KeyedTestBlock {
        const WIRE_TYPE: WireType = WireType::U64;

        fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
            self.0.encode_varve(encoder)
        }
    }

    impl VarveDecode for KeyedTestBlock {
        const WIRE_TYPE: WireType = WireType::U64;

        fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
            Ok(Self(u64::decode_varve(decoder)?))
        }
    }

    impl VarveBlock for KeyedTestBlock {
        const ID: u32 = 18;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = true;
        const SCHEMA_FINGERPRINT: u64 = 0x5354_5245_414D_0018;
    }

    impl VarveKeyedBlock for KeyedTestBlock {
        type Key = u64;

        fn key(&self) -> Self::Key {
            self.0
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct OversizedBlock;

    impl VarveEncode for OversizedBlock {
        const WIRE_TYPE: WireType = WireType::Bytes;

        fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
            encoder.write_all(&[0xA5; 1024]);
            Ok(())
        }
    }

    impl VarveDecode for OversizedBlock {
        const WIRE_TYPE: WireType = WireType::Bytes;

        fn decode_varve(_decoder: &mut Decoder<'_>) -> Result<Self> {
            Ok(Self)
        }
    }

    impl VarveBlock for OversizedBlock {
        const ID: u32 = 19;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Variable;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = false;
        const SCHEMA_FINGERPRINT: u64 = 0x5354_5245_414D_0019;
    }

    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: TestBlock::ID,
        name: "TestBlock",
        version: TestBlock::VERSION,
        kind: TestBlock::KIND,
        fields: &[],
    }];

    static KEYED_BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: KeyedTestBlock::ID,
        name: "KeyedTestBlock",
        version: KeyedTestBlock::VERSION,
        kind: KeyedTestBlock::KIND,
        fields: &[],
    }];

    static OVERSIZED_BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: OversizedBlock::ID,
        name: "OversizedBlock",
        version: OversizedBlock::VERSION,
        kind: OversizedBlock::KIND,
        fields: &[],
    }];

    fn spec(index_policy: IndexPolicy, commit_policy: CommitPolicy) -> FormatSpec {
        FormatSpec::new(
            b"VSTREAM",
            1,
            Endian::Little,
            0,
            index_policy,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .with_commit_policy(commit_policy)
        .with_read_limits(ReadLimits::finite_all(u64::MAX))
    }

    fn keyed_spec() -> FormatSpec {
        FormatSpec::new(
            b"VSTRKEY",
            1,
            Endian::Little,
            0,
            IndexPolicy::KeyedOffsetChain,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            KEYED_BLOCKS,
        )
        .with_commit_policy(CommitPolicy::RecordFooter)
        .with_read_limits(ReadLimits::finite_all(u64::MAX))
    }

    fn bounded_encode_spec() -> FormatSpec {
        FormatSpec::new(
            b"VSTRBND",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            OVERSIZED_BLOCKS,
        )
        .with_read_limits(
            ReadLimits::STANDARD
                .with_max_logical_payload_len(16)
                .with_max_record_payload_len(16),
        )
    }

    #[test]
    fn stream_writer_matches_resident_native_bytes() -> Result<()> {
        #[allow(unused_mut)]
        let mut policies = vec![
            (
                IndexPolicy::ScanOnOpen,
                CommitPolicy::None,
                IntegrityPolicy::None,
            ),
            (
                IndexPolicy::BlockOffsetChain,
                CommitPolicy::RecordFooter,
                IntegrityPolicy::None,
            ),
        ];
        #[cfg(feature = "integrity")]
        policies.push((
            IndexPolicy::BlockOffsetChain,
            CommitPolicy::RecordFooter,
            IntegrityPolicy::Crc32WithHeader,
        ));
        for (index_policy, commit_policy, integrity_policy) in policies {
            let directory = tempfile::tempdir()?;
            let resident_path = directory.path().join("resident.varve");
            let stream_path = directory.path().join("stream.varve");
            let spec = spec(index_policy, commit_policy).with_integrity_policy(integrity_policy);
            let mut resident = crate::VarveFile::create(spec, &resident_path)?;
            // STO-01: a stream primary opens with an internal creation-nonce
            // record, so the resident file gets an equally shaped leading
            // record. Sequences and record offsets then line up exactly and the
            // user-record region must still match byte for byte.
            resident.write_record(
                crate::CREATION_NONCE_BLOCK_ID,
                1,
                crate::file::RECORD_FLAG_INTERNAL,
                &[0u8; 16],
            )?;
            let mut stream =
                VarveStreamWriter::create(spec, &stream_path, StreamOptions::default())?;
            for value in [3, 5, 8, 13] {
                let block = TestBlock(value);
                assert_eq!(resident.push_info(&block)?, stream.push_info(&block)?);
            }
            resident.flush()?;
            stream.flush()?;
            let resident_bytes = std::fs::read(&resident_path)?;
            let stream_bytes = std::fs::read(&stream_path)?;
            assert_eq!(resident_bytes.len(), stream_bytes.len());
            let mut file = std::fs::File::open(&stream_path)?;
            let header_len = crate::file::read_file_header(spec, &mut file)?;
            let nonce_record_len = crate::file::RECORD_HEADER_LEN
                + 16
                + if spec.spec_needs_record_footer() {
                    crate::file::RECORD_FOOTER_LEN
                } else {
                    0
                };
            let user_start = usize::try_from(header_len + nonce_record_len).unwrap();
            assert_eq!(
                resident_bytes[..header_len as usize],
                stream_bytes[..header_len as usize]
            );
            assert_eq!(
                resident_bytes[user_start..],
                stream_bytes[user_start..],
                "user records must encode identically in the resident and streaming writers"
            );
        }
        Ok(())
    }

    #[test]
    fn batch_writer_coalesces_records_and_matches_single_append() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let single_path = directory.path().join("single.varve");
        let batch_path = directory.path().join("batch.varve");
        let spec = spec(IndexPolicy::ScanOnOpen, CommitPolicy::None);
        let values: Vec<_> = (0..100).map(TestBlock).collect();

        let mut single = VarveStreamWriter::create(spec, &single_path, StreamOptions::default())?;
        for value in &values {
            single.push_info(value)?;
        }
        single.sync()?;

        let mut batch = VarveStreamWriter::create(spec, &batch_path, StreamOptions::default())?;
        let info = batch
            .push_iter::<TestBlock, _>(
                &values,
                BatchOptions {
                    max_records: 16,
                    max_bytes: usize::MAX,
                },
            )
            .map_err(|error| error.source)?;
        batch.sync()?;

        assert_eq!(info.records, 100);
        // Sequence 0 belongs to the internal creation-nonce record (STO-01).
        assert_eq!(info.first_sequence, Some(1));
        assert_eq!(info.last_sequence, Some(100));
        assert_eq!(info.write_calls, 7);
        assert_stream_bodies_match(spec, &single_path, &batch_path)?;
        Ok(())
    }

    /// Asserts two stream primaries are byte-identical apart from their
    /// per-create nonce, which is by construction unique to each create.
    fn assert_stream_bodies_match(spec: FormatSpec, left: &Path, right: &Path) -> Result<()> {
        let left_bytes = fs::read(left)?;
        let right_bytes = fs::read(right)?;
        assert_eq!(left_bytes.len(), right_bytes.len());
        let mut file = File::open(left)?;
        let header_len = crate::file::read_file_header(spec, &mut file)?;
        let nonce_record_len = crate::file::RECORD_HEADER_LEN
            + 16
            + if spec.spec_needs_record_footer() {
                crate::file::RECORD_FOOTER_LEN
            } else {
                0
            };
        let header_end = usize::try_from(header_len).unwrap();
        let body_start = usize::try_from(header_len + nonce_record_len).unwrap();
        assert_eq!(left_bytes[..header_end], right_bytes[..header_end]);
        assert_eq!(left_bytes[body_start..], right_bytes[body_start..]);
        Ok(())
    }

    #[test]
    fn clean_open_append_and_sync_construct_no_scanner() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("constant-open.varve");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        let values: Vec<_> = (0..64).map(TestBlock).collect();
        {
            let mut writer = VarveStreamWriter::create(spec, &path, StreamOptions::default())?;
            writer
                .push_iter::<TestBlock, _>(&values, BatchOptions::default())
                .map_err(|error| error.source)?;
            writer.sync()?;
        }

        crate::file::reset_stream_io_counters();
        let reader = VarveStreamReader::open(spec, &path, StreamOptions::default())?;
        assert_eq!(crate::file::stream_io_counters(), (0, 0, 0));
        drop(reader);

        let mut writer = VarveStreamWriter::open(spec, &path, StreamOptions::default())?;
        writer
            .push_iter::<TestBlock, _>([TestBlock(64)], BatchOptions::default())
            .map_err(|error| error.source)?;
        writer.sync()?;
        assert_eq!(crate::file::stream_io_counters(), (0, 0, 0));
        drop(writer);

        let reader = VarveStreamReader::open(spec, &path, StreamOptions::default())?;
        let mut events = reader.events()?;
        assert!(events.next().transpose()?.is_some());
        let (scanners, entries, point_reads) = crate::file::stream_io_counters();
        assert_eq!(scanners, 1);
        assert_eq!(entries, 1);
        assert_eq!(point_reads, 0);
        Ok(())
    }

    #[test]
    fn dirty_checkpoint_restore_truncates_without_scanning() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("restore.varve");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        let base_len;
        {
            let mut writer = VarveStreamWriter::create(spec, &path, StreamOptions::default())?;
            writer.push_info(&TestBlock(1))?;
            writer.sync()?;
            base_len = std::fs::metadata(&path)?.len();
            writer.push_info(&TestBlock(2))?;
        }
        assert!(std::fs::metadata(&path)?.len() > base_len);
        assert!(matches!(
            VarveStreamWriter::open(spec, &path, StreamOptions::default()),
            Err(Error::DiskIndex(_))
        ));

        crate::file::reset_stream_io_counters();
        let mut writer =
            VarveStreamWriter::restore_checkpoint_and_open(spec, &path, StreamOptions::default())?;
        assert_eq!(std::fs::metadata(&path)?.len(), base_len);
        assert_eq!(crate::file::stream_io_counters(), (0, 0, 0));
        writer.push_info(&TestBlock(3))?;
        writer.sync()?;
        drop(writer);

        let reader = VarveStreamReader::open(spec, &path, StreamOptions::default())?;
        assert_eq!(
            reader.blocks::<TestBlock>()?.collect::<Result<Vec<_>>>()?,
            [TestBlock(1), TestBlock(3)]
        );
        Ok(())
    }

    #[test]
    fn bootstrap_refuses_to_promote_an_existing_dirty_generation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("dirty-bootstrap.varve");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        {
            let mut writer = VarveStreamWriter::create(spec, &path, StreamOptions::default())?;
            writer.push_info(&TestBlock(1))?;
            writer.sync()?;
            writer.push_info(&TestBlock(2))?;
        }
        let dirty_len = fs::metadata(&path)?.len();
        assert!(matches!(
            bootstrap_stream_checkpoint(spec, &path, StreamOptions::default()),
            Err(Error::DiskIndex(_))
        ));
        assert_eq!(fs::metadata(&path)?.len(), dirty_len);
        let store = DiskIndexStore::open(state_sidecar_path(&path), DiskIndexOptions::default())
            .map_err(state_error)?;
        assert_eq!(
            store.read_metadata().map_err(state_error)?.state,
            crate::disk_index::DiskIndexState::Dirty
        );
        Ok(())
    }

    /// F-09. A pathname mutator that ignores `WriterLock` can replace the
    /// primary after the pre-publication identity check and before the state
    /// sidecar is visible. The published sidecar then describes a file object
    /// the pathname no longer names, and leaving it there would displace the
    /// replacement's own newer sidecar.
    ///
    /// The interposition lands in exactly that interval. Bootstrap must return
    /// a typed mismatch rather than success, and must take the sidecar it
    /// published back out again rather than leave an old-generation sidecar
    /// standing.
    #[test]
    fn bootstrap_retires_a_sidecar_published_for_a_replaced_primary() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replaced-bootstrap.varve");
        let replacement = directory.path().join("replacement.varve");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        for (target, value) in [(&path, 1u64), (&replacement, 2)] {
            let mut writer = VarveStreamWriter::create(spec, target, StreamOptions::default())?;
            writer.push_info(&TestBlock(value))?;
            writer.sync()?;
        }
        // Bootstrap promotes a native file that has no state sidecar yet.
        fs::remove_file(state_sidecar_path(&path))?;
        fs::remove_file(state_sidecar_path(&replacement))?;

        let swapped = path.clone();
        let source = replacement.clone();
        let interposed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = std::sync::Arc::clone(&interposed);
        set_publication_interposition(
            &fs::canonicalize(&path)?,
            Box::new(move || {
                // The interval under test only exists once the sidecar is
                // published, so the test is meaningless if this is not already
                // true when the swap lands.
                assert!(
                    state_sidecar_path(&swapped).exists(),
                    "the interposition must run after the sidecar is published",
                );
                fs::rename(&source, &swapped).expect("replace the primary during publication");
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
            }),
        );

        let result = bootstrap_stream_checkpoint(spec, &path, StreamOptions::default());
        match result {
            Err(Error::DiskIndex(error))
                if matches!(*error, crate::disk_index::DiskIndexError::IdentityMismatch) => {}
            other => panic!("expected a typed identity mismatch, got {other:?}"),
        }
        assert!(
            interposed.load(std::sync::atomic::Ordering::SeqCst),
            "the swap never reached the publication interval under test",
        );
        assert!(
            !state_sidecar_path(&path).exists(),
            "a sidecar published for a replaced primary must not be left behind",
        );
        Ok(())
    }

    #[test]
    fn restore_validates_format_tails_before_native_truncate() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("invalid-restore.varve");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        let first;
        {
            let mut writer = VarveStreamWriter::create(spec, &path, StreamOptions::default())?;
            first = writer.push_info(&TestBlock(1))?;
            writer.sync()?;
        }
        let clean_len = fs::metadata(&path)?.len();
        fs::remove_file(state_sidecar_path(&path))?;
        let native = VarveStreamReader::open_native(spec, &path, StreamOptions::default())?;
        let identity = primary_identity(spec, native.snapshot())?;
        drop(native);
        let store = create_state_store(
            &path,
            DiskIndexOptions::default(),
            DiskIndexMetadata::new_state(
                identity,
                // Two records: the internal creation nonce plus the one push,
                // so the exclusive sequence frontier moves from 1 to 2.
                DiskIndexFrontier::new(clean_len, 2, Some(2)),
                tail_limit_for_spec(spec).map_err(state_error)?,
            ),
            &[DiskIndexTail {
                block_id: 99,
                record_offset: first.record_offset,
                sequence: first.sequence,
            }],
        )?;
        store.begin_generation().map_err(state_error)?;
        drop(store);
        let mut file = OpenOptions::new().append(true).open(&path)?;
        file.write_all(b"dirty-tail")?;
        file.sync_all()?;
        drop(file);
        let dirty_len = fs::metadata(&path)?.len();

        assert!(matches!(
            VarveStreamWriter::restore_checkpoint_and_open(spec, &path, StreamOptions::default()),
            Err(Error::DiskIndex(_))
        ));
        assert_eq!(fs::metadata(&path)?.len(), dirty_len);
        assert!(dirty_len > clean_len);
        let store = DiskIndexStore::open(state_sidecar_path(&path), DiskIndexOptions::default())
            .map_err(state_error)?;
        assert_eq!(
            store.read_metadata().map_err(state_error)?.state,
            crate::disk_index::DiskIndexState::Dirty
        );
        Ok(())
    }

    #[test]
    fn low_level_stream_methods_cannot_bypass_keyed_chain_indexing() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("keyed-bypass.varve");
        let mut writer = VarveStreamWriter::create(keyed_spec(), &path, StreamOptions::default())?;
        // Baseline rather than a literal: a fresh primary already holds its
        // internal creation-nonce record (STO-01). The contract under test is
        // that the rejected calls append nothing on top of it.
        let baseline = writer.checkpoint().record_count;
        assert!(matches!(
            writer.push_with_prev_key_info(&KeyedTestBlock(1), None),
            Err(Error::StreamingUnsupported)
        ));
        assert!(matches!(
            writer.delete_with_prev_key_info::<KeyedTestBlock>(&1, None),
            Err(Error::StreamingUnsupported)
        ));
        assert_eq!(writer.checkpoint().record_count, baseline);
        Ok(())
    }

    #[test]
    fn scalable_writer_stops_encoding_at_the_runtime_payload_limit() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bounded-encode.varve");
        let mut writer =
            VarveStreamWriter::create(bounded_encode_spec(), &path, StreamOptions::default())?;
        let clean_len = fs::metadata(&path)?.len();
        let baseline = writer.checkpoint().record_count;
        assert!(matches!(
            writer.push_info(&OversizedBlock),
            Err(Error::LimitExceeded {
                resource: "logical payload length",
                actual: 1024,
                limit: 16,
            })
        ));
        assert_eq!(fs::metadata(&path)?.len(), clean_len);
        assert_eq!(writer.checkpoint().record_count, baseline);
        writer.sync()?;
        Ok(())
    }

    #[test]
    fn state_sidecar_bound_splits_native_batch() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bounded-state.varve");
        let options = StreamOptions {
            state: DiskIndexOptions {
                max_key_bytes: 1024,
                batch: DiskIndexBatchOptions {
                    max_records: 2,
                    max_bytes: 4096,
                },
                ..DiskIndexOptions::default()
            },
            ..StreamOptions::default()
        };
        let values: Vec<_> = (0..5).map(TestBlock).collect();
        let mut writer = VarveStreamWriter::create(
            spec(IndexPolicy::ScanOnOpen, CommitPolicy::None),
            &path,
            options,
        )?;
        let report = writer
            .push_iter::<TestBlock, _>(
                &values,
                BatchOptions {
                    max_records: 100,
                    max_bytes: usize::MAX,
                },
            )
            .map_err(|error| error.source)?;
        assert_eq!(report.records, 5);
        assert_eq!(report.write_calls, 3);
        writer.sync()?;
        Ok(())
    }

    #[test]
    fn reopen_and_owned_iterators_remain_bounded() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bounded.varve");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        {
            let mut writer = VarveStreamWriter::create(spec, &path, StreamOptions::default())?;
            for value in 0..128 {
                writer.push_info(&TestBlock(value))?;
            }
            let state = writer.resident_state();
            assert_eq!(state.declared_block_tails, 1);
            assert_eq!(state.retained_record_entries, 0);
            assert_eq!(state.retained_key_entries, 0);
            assert_eq!(state.active_payload_bytes, 0);
            writer.sync()?;
        }
        {
            let mut writer = VarveStreamWriter::open(spec, &path, StreamOptions::default())?;
            // Sequence 0 belongs to the internal creation-nonce record, so the
            // 129th user append carries sequence 129 (STO-01).
            assert_eq!(writer.push_info(&TestBlock(128))?.sequence, 129);
            writer.sync()?;
        }
        let reader = VarveStreamReader::open(spec, &path, StreamOptions::default())?;
        let blocks = reader.blocks::<TestBlock>()?;
        drop(reader);
        let values = blocks.collect::<Result<Vec<_>>>()?;
        assert_eq!(values.len(), 129);
        assert_eq!(values.last(), Some(&TestBlock(128)));
        Ok(())
    }

    #[test]
    fn non_increasing_sequences_are_streaming_unsupported() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("sequence.varve");
        let spec = spec(IndexPolicy::ScanOnOpen, CommitPolicy::None);
        let mut file = File::create(&path)?;
        write_file_header(spec, &mut file)?;
        let first_offset = file.stream_position()?;
        let first = prepare_stream_user_record(spec, &TestBlock(1), 4, first_offset, None, None)?;
        file.write_all(&first.bytes)?;
        let second_offset = file.stream_position()?;
        let second = prepare_stream_user_record(spec, &TestBlock(2), 4, second_offset, None, None)?;
        file.write_all(&second.bytes)?;
        drop(file);

        let reader = VarveStreamReader::open_native(spec, path, StreamOptions::default())?;
        let mut events = reader.events()?;
        assert!(events.next().unwrap().is_ok());
        assert!(matches!(
            events.next().unwrap(),
            Err(Error::StreamingUnsupported)
        ));
        Ok(())
    }

    #[test]
    fn unsupported_policy_is_rejected_before_create() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unsupported.varve");
        let spec = spec(
            IndexPolicy::ScanOnOpen,
            CommitPolicy::TransactionMarker(TransactionMarkerMode::Explicit),
        );
        assert!(matches!(
            VarveStreamWriter::create(spec, &path, StreamOptions::default()),
            Err(Error::StreamingUnsupported)
        ));
        assert!(!path.exists());
    }
}
