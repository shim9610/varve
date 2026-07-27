use std::borrow::Borrow;
use std::fs;
use std::path::Path;

use crate::collections::{MaterializationBudget, ensure_registered_block};
use crate::disk_index::{
    DiskIndexDescriptor as DiskIndexedBlock, DiskIndexEntry, DiskIndexError, DiskIndexFrontier,
    DiskIndexMetadata, DiskIndexOptions, DiskIndexPhysicalRecord, DiskIndexPlan,
    DiskIndexRecordPointer, DiskIndexState, DiskIndexStore, DiskIndexTail, DiskIndexUpdate,
    DiskIndexWriteBatch, PRIMARY_GENERATION_WINDOW, VarveDiskKey, read_metadata_read_only,
    sidecar_path, tail_limit_for_spec,
};
use crate::file::{
    NativeStreamScanner, RECORD_FOOTER_LEN, RECORD_HEADER_LEN, ReplaceDurability, WriterLock,
    decode_stream_tombstone_key, prepare_stream_tombstone_record, prepare_stream_user_record,
    publish_temp_path_atomically, read_file_header, read_stream_entry_at,
};
use crate::native_layout::{decode_native_record_footer, read_native_record_header};
use crate::scalable_extent::UntrustedRecordPointer;
use crate::stream::{
    StreamCheckpoint, StreamMutationPermit, StreamTail, primary_generation, primary_identity,
    verify_primary_generation,
};
use crate::traits::KeyedBlockContract;
use crate::{
    AppendInfo, BatchAppendError, BatchAppendInfo, BatchOptions, Error, FormatSpec,
    RecordIndexEntry, Result, ScanOptions, ScanProgress, SnapshotFile, StreamOptions,
    TOMBSTONE_BLOCK_ID, VarveBlock, VarveKeyedBlock, VarveStreamReader, VarveStreamWriter,
};

const INDEX_BATCH_RECORDS: usize = 16_384;

/// The context this writer names when it refuses a mutation. The flag itself
/// belongs to the stream writer underneath; see
/// `VarveIndexedWriter::ensure_not_poisoned`.
const INDEXED_WRITER_POISON_CONTEXT: &str = "indexed";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskIndexRebuildReport {
    pub records: u64,
    pub scanned_bytes: u64,
}

/// Scaling observability for the disk-index release gates.
///
/// These are thread-local test counters, not runtime metrics: they exist so
/// the suite can pin PERF-04 (registry cost per open must not scale with the
/// number of live sidecar identities) and API-03 (no captured codec runs
/// before schema identity is validated) as executable contracts. They are
/// hosted here because `varve-core`'s re-export list is outside this module's
/// ownership; see the openIssues note about moving them to a dedicated
/// exported handle.
#[cfg(feature = "scalable-fault-injection")]
impl DiskIndexRebuildReport {
    pub fn registry_slots_inspected() -> u64 {
        crate::disk_index::scaling_counters::registry_slots_inspected()
    }

    pub fn primary_generation_scans() -> u64 {
        crate::disk_index::scaling_counters::primary_generation_scans()
    }

    pub fn descriptor_decoder_calls() -> u64 {
        crate::disk_index::scaling_counters::descriptor_decoder_calls()
    }

    pub fn reset_scaling_counters() {
        crate::disk_index::scaling_counters::reset();
    }
}

/// Rebuilds the disk-index sidecar from the native log.
///
/// Under CRC integrity policies each indexed record's payload (plan puts and
/// tombstones) is read and checksum-verified exactly once; payloads of
/// records outside the plan are not read at all. Use
/// [`VarveStreamReader::verify_all`] for a whole-file integrity scan.
///
/// [`Error::PublishedButParentSyncPending`] means the rebuilt sidecar was
/// already published at its pathname and only the parent-directory entry's
/// durability is unconfirmed; the published sidecar is preserved.
///
/// # Non-cooperating writers (F-09)
///
/// Cooperating writers are serialized by the writer lock this call holds. A
/// process that replaces the primary *pathname* without taking that lock is
/// outside the contract, and no userspace library can make a rebuild atomic
/// against it. The rebuild bounds what such a writer can cause: the primary's
/// identity is verified immediately before publication, the verified object is
/// held open across it so its identity cannot be recycled, and the identity is
/// re-checked once the sidecar is visible. If the replacement landed inside
/// that interval the rebuilt sidecar is removed again and the call fails with
/// `DiskIndexError::IdentityMismatch` — or, if the stale sidecar could not be
/// removed, with a `DiskIndexError::CheckpointMismatch` naming what must be
/// deleted. The residual cost is a rebuild the caller must repeat, never a
/// sidecar a consumer would accept for the wrong primary.
pub fn rebuild_disk_index(
    spec: FormatSpec,
    path: impl AsRef<Path>,
    options: DiskIndexOptions,
    plan: DiskIndexPlan,
) -> Result<DiskIndexRebuildReport> {
    rebuild_disk_index_with_progress(
        spec,
        path,
        options,
        plan,
        crate::stream::silent_scan_options(),
        |_| {},
    )
}

pub fn rebuild_disk_index_with_progress<F>(
    spec: FormatSpec,
    path: impl AsRef<Path>,
    options: DiskIndexOptions,
    plan: DiskIndexPlan,
    scan: ScanOptions<'_>,
    observer: F,
) -> Result<DiskIndexRebuildReport>
where
    F: FnMut(ScanProgress),
{
    let plan = plan.validate(spec).map_err(index_error)?;
    let path = std::fs::canonicalize(path.as_ref())?;
    let _lock = WriterLock::acquire(&path)?;
    reject_dirty_rebuild_source(&path, options)?;
    let stream = VarveStreamReader::open_native(
        spec,
        &path,
        StreamOptions {
            limits: options.limits,
            state: options,
        },
    )?;
    rebuild_index(
        &path,
        options,
        stream.spec(),
        stream.snapshot(),
        plan,
        scan,
        observer,
    )
}

pub struct VarveIndexedReader {
    stream: VarveStreamReader,
    index: crate::disk_index::DiskIndexSnapshot,
    _store: DiskIndexStore,
    indexed_blocks: Vec<u32>,
}

impl VarveIndexedReader {
    pub fn open(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: DiskIndexOptions,
        plan: DiskIndexPlan,
    ) -> Result<Self> {
        let plan = plan.validate(spec).map_err(index_error)?;
        let blocks = plan.descriptors();
        let path = std::fs::canonicalize(path.as_ref())?;
        let stream = VarveStreamReader::open_native(
            spec,
            &path,
            StreamOptions {
                limits: options.limits,
                state: options,
            },
        )?;
        let identity = primary_identity(stream.spec(), stream.snapshot())?;
        let store = DiskIndexStore::open(sidecar_path(&path), options).map_err(index_error)?;
        let index = store
            .begin_snapshot_with_plan(identity, plan, stream.snapshot().len())
            .map_err(index_error)?;
        verify_primary_generation(stream.spec(), index.primary_generation(), stream.snapshot())?;
        let stream = stream.pin_logical_len(index.committed_eof())?;
        Ok(Self {
            stream,
            index,
            _store: store,
            indexed_blocks: blocks.iter().map(|block| block.block_id).collect(),
        })
    }

    /// Returns the physical event stream. The scanner is created only when
    /// this explicit iteration API is called; opening the reader does not scan.
    pub fn events(&self) -> Result<crate::StreamEvents> {
        self.stream.events()
    }

    /// Explicitly validates every native record in the pinned snapshot.
    pub fn verify_all(&self) -> Result<u64> {
        self.stream.verify_all()
    }

    pub fn verify_all_with_progress<F>(&self, scan: ScanOptions<'_>, observer: F) -> Result<u64>
    where
        F: FnMut(ScanProgress),
    {
        self.stream.verify_all_with_progress(scan, observer)
    }

    /// Returns a lazy typed scan over one block type.
    pub fn blocks<T: VarveBlock>(&self) -> Result<crate::StreamingBlocks<T>> {
        self.stream.blocks::<T>()
    }

    pub fn lookup<T>(&self, key: &T::Key) -> Result<DiskIndexEntry>
    where
        T: VarveKeyedBlock,
        T::Key: VarveDiskKey,
    {
        // API2-03: every public keyed generic entry point evaluates the
        // compile-time keyedness contract post-monomorphization; the
        // registration in `ensure_indexed` stays as the runtime backstop.
        let () = KeyedBlockContract::<T>::OK;
        self.ensure_indexed::<T>()?;
        let pointer = self.index.lookup_pointer(T::ID, key).map_err(index_error)?;
        self.validate_candidate::<T>(key, pointer)?;
        Ok(pointer.entry)
    }

    pub fn get<T>(&self, key: &T::Key) -> Result<Option<T>>
    where
        T: VarveKeyedBlock,
        T::Key: VarveDiskKey,
    {
        // API2-03: compile-time keyedness contract at the public entry point.
        let () = KeyedBlockContract::<T>::OK;
        self.ensure_indexed::<T>()?;
        let pointer = self.index.lookup_pointer(T::ID, key).map_err(index_error)?;
        match pointer.entry {
            DiskIndexEntry::Missing => Ok(None),
            DiskIndexEntry::Tombstone { .. } => {
                self.validate_candidate::<T>(key, pointer)?;
                Ok(None)
            }
            DiskIndexEntry::Put { .. } => Ok(Some(self.decode_put_candidate::<T>(key, pointer)?)),
        }
    }

    pub fn resident_state(&self) -> crate::StreamResidentState {
        self.stream.resident_state()
    }

    /// Historical distinct key cardinality of the pinned sidecar snapshot: the
    /// number of distinct `(block, key)` pairs ever indexed, including keys
    /// whose latest entry is a tombstone.
    ///
    /// Sidecar capacity follows this metric (`K`-ever), not the live key
    /// count: tombstones replace latest values without deleting rows, and a
    /// sidecar rebuild re-creates tombstone rows from the native log.
    /// Reclaiming the historical rows requires compacting the native file and
    /// rebuilding the sidecar together.
    pub fn historical_distinct_keys(&self) -> Result<u64> {
        self.index.historical_distinct_keys().map_err(index_error)
    }

    fn ensure_indexed<T: VarveKeyedBlock>(&self) -> Result<()> {
        ensure_registered_block::<T>(self.stream.spec())?;
        if self.indexed_blocks.contains(&T::ID) {
            Ok(())
        } else {
            Err(index_invariant(
                "block is not declared with key_index = disk",
            ))
        }
    }

    /// Reads the native record entry for one index candidate.
    ///
    /// With `IntegrityPolicy::None`, `read_stream_entry_at` performs a bounded
    /// frame read and no payload I/O. Under CRC policies it would additionally
    /// stream the whole payload from the file to checksum it, and the typed
    /// decode that follows every candidate would then read the payload a
    /// second time. In that case read only the frame here: each caller feeds
    /// the entry into `read_logical_payload_snapshot`, which verifies the full
    /// record checksum against this entry with a single payload read.
    fn candidate_entry(&self, offset: u64, physical_len: u64) -> Result<RecordIndexEntry> {
        if self.stream.spec().integrity_policy == crate::IntegrityPolicy::None {
            read_stream_entry_at(
                self.stream.spec(),
                self.stream.snapshot(),
                offset,
                physical_len,
            )
        } else {
            read_stream_entry_frame(
                self.stream.spec(),
                self.stream.snapshot(),
                offset,
                physical_len,
            )
        }
    }

    fn validate_candidate<T>(&self, key: &T::Key, pointer: DiskIndexRecordPointer) -> Result<()>
    where
        T: VarveKeyedBlock,
        T::Key: VarveDiskKey,
    {
        let (offset, expected_sequence) = match pointer.entry {
            DiskIndexEntry::Missing => {
                if pointer.physical.is_some() {
                    return Err(Error::InvalidIndexCheckpoint);
                }
                return Ok(());
            }
            DiskIndexEntry::Put {
                record_offset,
                sequence,
            }
            | DiskIndexEntry::Tombstone {
                record_offset,
                sequence,
            } => (record_offset, sequence),
        };
        let physical = pointer.physical.ok_or(Error::InvalidIndexCheckpoint)?;
        let entry = self.candidate_entry(offset, physical.physical_len)?;
        validate_physical_candidate(self.stream.spec(), &entry, physical)?;
        if entry.sequence != expected_sequence {
            return Err(index_invariant(
                "candidate sequence does not match native record",
            ));
        }
        match pointer.entry {
            DiskIndexEntry::Put { .. } => {
                self.decode_put_entry::<T>(key, &entry)?;
            }
            DiskIndexEntry::Tombstone { .. } => {
                if entry.block_id != TOMBSTONE_BLOCK_ID {
                    return Err(index_invariant("tombstone candidate targets a user block"));
                }
                let decoded = decode_stream_tombstone_key::<T>(
                    self.stream.spec(),
                    self.stream.snapshot(),
                    &entry,
                )?;
                if decoded.as_ref() != Some(key) {
                    return Err(index_invariant("tombstone candidate key mismatch"));
                }
            }
            DiskIndexEntry::Missing => {}
        }
        Ok(())
    }

    fn decode_put_candidate<T>(&self, key: &T::Key, pointer: DiskIndexRecordPointer) -> Result<T>
    where
        T: VarveKeyedBlock,
        T::Key: VarveDiskKey,
    {
        let (offset, expected_sequence) = match pointer.entry {
            DiskIndexEntry::Put {
                record_offset,
                sequence,
            } => (record_offset, sequence),
            _ => return Err(Error::InvalidIndexCheckpoint),
        };
        let physical = pointer.physical.ok_or(Error::InvalidIndexCheckpoint)?;
        let entry = self.candidate_entry(offset, physical.physical_len)?;
        validate_physical_candidate(self.stream.spec(), &entry, physical)?;
        if entry.sequence != expected_sequence {
            return Err(index_invariant(
                "candidate sequence does not match native record",
            ));
        }
        self.decode_put_entry::<T>(key, &entry)
    }

    fn decode_put_entry<T>(&self, key: &T::Key, entry: &RecordIndexEntry) -> Result<T>
    where
        T: VarveKeyedBlock,
        T::Key: VarveDiskKey,
    {
        if entry.block_id != T::ID || entry.block_version != T::VERSION {
            return Err(index_invariant("put candidate targets the wrong block"));
        }
        let logical_len =
            entry.logical_payload_len_snapshot(self.stream.spec(), self.stream.snapshot())?;
        let mut budget = MaterializationBudget::new(self.stream.spec());
        budget.consume(logical_len)?;
        let payload =
            entry.read_logical_payload_snapshot(self.stream.spec(), self.stream.snapshot())?;
        let value: T = budget.decode(&payload, T::ENDIAN.unwrap_or(self.stream.spec().endian))?;
        if &value.key() != key {
            return Err(index_invariant("put candidate key mismatch"));
        }
        Ok(value)
    }
}

pub struct VarveIndexedWriter {
    stream: VarveStreamWriter,
    index: DiskIndexStore,
    indexed_blocks: Vec<u32>,
    batch: Option<DiskIndexWriteBatch>,
    batch_records: usize,
    batch_last_sequence: Option<u64>,
    dirty: bool,
}

impl VarveIndexedWriter {
    pub fn create(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: DiskIndexOptions,
        plan: DiskIndexPlan,
    ) -> Result<Self> {
        let plan = plan.validate(spec).map_err(index_error)?;
        let blocks = plan.descriptors();
        let path = canonical_primary_path(path.as_ref())?;
        let mut stream = VarveStreamWriter::create_unmanaged(
            spec,
            &path,
            StreamOptions {
                limits: options.limits,
                state: options,
            },
        )?;
        crate::scalable_fault_point("create.native_sync");
        let sync = stream.sync();
        crate::scalable_fault_point("create.native_sync");
        sync?;
        let checkpoint = stream.checkpoint();
        let identity = primary_identity(stream.spec(), stream.snapshot())?;
        let metadata = DiskIndexMetadata::new_disk(
            identity,
            plan.digest(),
            checkpoint_frontier(&checkpoint),
            tail_limit_for_spec(stream.spec()).map_err(index_error)?,
        )
        .with_primary_generation(primary_generation(
            stream.spec(),
            stream.snapshot(),
            stream.snapshot().len(),
        )?);
        let index = create_index_store(
            &path,
            options,
            metadata,
            &checkpoint_disk_tails(&checkpoint),
        )?;
        Ok(Self::new(stream, index, blocks))
    }

    pub fn restore_checkpoint_and_open(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: DiskIndexOptions,
        plan: DiskIndexPlan,
    ) -> Result<Self> {
        let plan = plan.validate(spec).map_err(index_error)?;
        let blocks = plan.descriptors();
        let path = std::fs::canonicalize(path.as_ref())?;
        let (stream, index) = VarveStreamWriter::restore_checkpointed(
            spec,
            &path,
            StreamOptions {
                limits: options.limits,
                state: options,
            },
            options,
            sidecar_path(&path),
            plan.mode(),
        )?;
        Ok(Self::new(stream, index, blocks))
    }

    pub fn open(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: DiskIndexOptions,
        plan: DiskIndexPlan,
    ) -> Result<Self> {
        let plan = plan.validate(spec).map_err(index_error)?;
        let blocks = plan.descriptors();
        let path = std::fs::canonicalize(path.as_ref())?;
        let (stream, index) = VarveStreamWriter::open_checkpointed(
            spec,
            &path,
            StreamOptions {
                limits: options.limits,
                state: options,
            },
            |identity, physical_len| {
                let store =
                    DiskIndexStore::open(sidecar_path(&path), options).map_err(index_error)?;
                let state = store
                    .validate_clean_writer(
                        identity,
                        plan.mode(),
                        physical_len,
                        tail_limit_for_spec(spec).map_err(index_error)?,
                    )
                    .map_err(index_error)?;
                Ok((
                    stream_checkpoint_from_state(&state),
                    state.metadata.primary_generation,
                    store,
                ))
            },
        )?;
        Ok(Self::new(stream, index, blocks))
    }

    fn new(stream: VarveStreamWriter, index: DiskIndexStore, blocks: &[DiskIndexedBlock]) -> Self {
        Self {
            stream,
            index,
            indexed_blocks: blocks.iter().map(|block| block.block_id).collect(),
            batch: None,
            batch_records: 0,
            batch_last_sequence: None,
            dirty: false,
        }
    }

    pub fn push_info<T>(&mut self, value: &T) -> Result<AppendInfo>
    where
        T: VarveKeyedBlock,
        T::Key: VarveDiskKey,
    {
        // API2-03: compile-time keyedness contract at the public entry point.
        let () = KeyedBlockContract::<T>::OK;
        let _permit = self.ensure_writable::<T>()?;
        let key = value.key();
        self.ensure_batch()?;
        let canonical_key =
            DiskIndexUpdate::encode_key(&key, self.index.max_key_bytes()).map_err(index_error)?;
        let previous = if self.stream.spec().index_policy.keyed_offset_chain {
            previous_offset(
                self.batch
                    .as_ref()
                    .expect("batch initialized")
                    .lookup_pointer_canonical(T::ID, &canonical_key)
                    .map_err(index_error)?
                    .entry,
            )
        } else {
            None
        };
        let old_eof = self.stream.snapshot().len();
        let sequence = self.stream.next_sequence()?;
        let record = prepare_stream_user_record(
            self.stream.spec(),
            value,
            sequence,
            old_eof,
            self.stream.previous_block(T::ID),
            previous,
        )?;
        let info = record.info;
        let physical = prepared_physical(self.stream.spec(), &record)?;
        let update = DiskIndexUpdate::from_canonical_with_record(
            T::ID,
            canonical_key,
            DiskIndexEntry::Put {
                record_offset: info.record_offset,
                sequence: info.sequence,
            },
            physical,
            self.index.max_key_bytes(),
        )
        .map_err(index_error)?;
        let tail = record_tail(self.stream.spec(), T::ID, info);
        self.ensure_update_capacity(&update, tail.is_some())?;
        // The permit is re-taken here rather than carried from the entry
        // guard: `ensure_update_capacity` can commit a sidecar chunk, which is
        // itself a step that can poison this writer.
        let permit = self.ensure_not_poisoned()?;
        self.stream
            .append_prepared_chunk(permit, &record.bytes, &[(T::ID, info)])?;
        self.publish_update(old_eof, info, update, tail)
    }

    pub fn delete_info<T>(&mut self, key: &T::Key) -> Result<AppendInfo>
    where
        T: VarveKeyedBlock,
        T::Key: VarveDiskKey,
    {
        // API2-03: compile-time keyedness contract at the public entry point.
        let () = KeyedBlockContract::<T>::OK;
        let _permit = self.ensure_writable::<T>()?;
        self.ensure_batch()?;
        let canonical_key =
            DiskIndexUpdate::encode_key(key, self.index.max_key_bytes()).map_err(index_error)?;
        let previous = if self.stream.spec().index_policy.keyed_offset_chain {
            previous_offset(
                self.batch
                    .as_ref()
                    .expect("batch initialized")
                    .lookup_pointer_canonical(T::ID, &canonical_key)
                    .map_err(index_error)?
                    .entry,
            )
        } else {
            None
        };
        let old_eof = self.stream.snapshot().len();
        let sequence = self.stream.next_sequence()?;
        let record = prepare_stream_tombstone_record::<T>(
            self.stream.spec(),
            key,
            sequence,
            old_eof,
            self.stream.previous_block(TOMBSTONE_BLOCK_ID),
            previous,
        )?;
        let info = record.info;
        let physical = prepared_physical(self.stream.spec(), &record)?;
        let update = DiskIndexUpdate::from_canonical_with_record(
            T::ID,
            canonical_key,
            DiskIndexEntry::Tombstone {
                record_offset: info.record_offset,
                sequence: info.sequence,
            },
            physical,
            self.index.max_key_bytes(),
        )
        .map_err(index_error)?;
        let tail = record_tail(self.stream.spec(), TOMBSTONE_BLOCK_ID, info);
        self.ensure_update_capacity(&update, tail.is_some())?;
        let permit = self.ensure_not_poisoned()?;
        self.stream
            .append_prepared_chunk(permit, &record.bytes, &[(TOMBSTONE_BLOCK_ID, info)])?;
        self.publish_update(old_eof, info, update, tail)
    }

    pub fn push_iter<T, I>(
        &mut self,
        values: I,
        options: BatchOptions,
    ) -> std::result::Result<BatchAppendInfo, BatchAppendError>
    where
        T: VarveKeyedBlock,
        T::Key: VarveDiskKey,
        I: IntoIterator,
        I::Item: Borrow<T>,
    {
        // API2-03: compile-time keyedness contract at the public entry point.
        let () = KeyedBlockContract::<T>::OK;
        let start = self.stream.snapshot().len();
        let mut written = BatchAppendInfo {
            start_offset: start,
            end_offset: start,
            ..BatchAppendInfo::default()
        };
        let result = self.push_indexed_iter_inner::<T, I>(values, options, &mut written);
        if let Err(source) = result {
            self.batch.take();
            self.stream.poison();
            return Err(BatchAppendError { written, source });
        }
        Ok(written)
    }

    pub fn push_unindexed_info<T: VarveBlock>(&mut self, value: &T) -> Result<AppendInfo> {
        let _permit = self.ensure_not_poisoned()?;
        ensure_registered_block::<T>(self.stream.spec())?;
        if self.indexed_blocks.contains(&T::ID) {
            return Err(index_invariant(
                "disk-indexed blocks cannot use the unindexed append path",
            ));
        }
        self.ensure_batch()?;
        self.ensure_coverage_capacity(self.stream.spec().index_policy.block_offset_chain)?;
        let old_eof = self.stream.snapshot().len();
        let info = self.stream.push_info(value)?;
        self.publish_coverage::<T>(old_eof, info, false)
    }

    pub fn push_unindexed_iter<T, I>(
        &mut self,
        values: I,
        options: BatchOptions,
    ) -> std::result::Result<BatchAppendInfo, BatchAppendError>
    where
        T: VarveBlock,
        I: IntoIterator,
        I::Item: Borrow<T>,
    {
        let start = self.stream.snapshot().len();
        let mut written = BatchAppendInfo {
            start_offset: start,
            end_offset: start,
            ..BatchAppendInfo::default()
        };
        let result = self.push_unindexed_iter_inner::<T, I>(values, options, &mut written);
        if let Err(source) = result {
            self.batch.take();
            self.stream.poison();
            return Err(BatchAppendError { written, source });
        }
        Ok(written)
    }

    pub fn delete_unindexed_info<T: VarveKeyedBlock>(
        &mut self,
        key: &T::Key,
    ) -> Result<AppendInfo> {
        // API2-03: compile-time keyedness contract at the public entry point.
        let () = KeyedBlockContract::<T>::OK;
        let _permit = self.ensure_not_poisoned()?;
        ensure_registered_block::<T>(self.stream.spec())?;
        if self.indexed_blocks.contains(&T::ID) {
            return Err(index_invariant(
                "disk-indexed blocks cannot use the unindexed delete path",
            ));
        }
        if self.stream.spec().index_policy.keyed_offset_chain {
            return Err(Error::StreamingUnsupported);
        }
        self.ensure_batch()?;
        self.ensure_coverage_capacity(self.stream.spec().index_policy.block_offset_chain)?;
        let old_eof = self.stream.snapshot().len();
        let info = self.stream.delete_with_prev_key_info::<T>(key, None)?;
        self.publish_coverage::<T>(old_eof, info, true)
    }

    pub fn flush(&mut self) -> Result<()> {
        let _permit = self.ensure_not_poisoned()?;
        self.stream.flush()
    }

    pub fn sync(&mut self) -> Result<()> {
        let _permit = self.ensure_not_poisoned()?;
        self.commit_pending_batch()?;
        self.stream.sync()?;
        if self.dirty {
            self.index
                .publish_clean(checkpoint_frontier(&self.stream.checkpoint()))
                .map_err(index_error)?;
        }
        self.batch_records = 0;
        self.batch_last_sequence = None;
        self.dirty = false;
        Ok(())
    }

    pub fn resident_state(&self) -> crate::StreamResidentState {
        self.stream.resident_state()
    }

    /// Historical distinct key cardinality of the last committed sidecar
    /// root; records appended since the last committed batch are not yet
    /// visible. See [`VarveIndexedReader::historical_distinct_keys`] for the
    /// capacity and reclaim semantics.
    pub fn historical_distinct_keys(&self) -> Result<u64> {
        self.index.historical_distinct_keys().map_err(index_error)
    }

    fn push_indexed_iter_inner<T, I>(
        &mut self,
        values: I,
        options: BatchOptions,
        written: &mut BatchAppendInfo,
    ) -> Result<()>
    where
        T: VarveKeyedBlock,
        T::Key: VarveDiskKey,
        I: IntoIterator,
        I::Item: Borrow<T>,
    {
        let options = options.validate()?;
        let _permit = self.ensure_writable::<T>()?;
        self.commit_pending_batch()?;

        let max_records = options.max_records.min(INDEX_BATCH_RECORDS);
        let mut bytes = Vec::new();
        let mut records = Vec::new();
        let mut next_sequence = Some(self.stream.next_sequence()?);
        let mut next_offset = self.stream.snapshot().len();
        let mut previous_block = self.stream.previous_block(T::ID);

        for value in values {
            self.ensure_batch()?;
            let value = value.borrow();
            let key = value.key();
            let canonical_key = DiskIndexUpdate::encode_key(&key, self.index.max_key_bytes())
                .map_err(index_error)?;
            let previous_key = if self.stream.spec().index_policy.keyed_offset_chain {
                previous_offset(
                    self.batch
                        .as_ref()
                        .expect("batch initialized")
                        .lookup_pointer_canonical(T::ID, &canonical_key)
                        .map_err(index_error)?
                        .entry,
                )
            } else {
                None
            };
            let sequence = next_sequence.ok_or(Error::SequenceExhausted)?;
            let record = prepare_stream_user_record(
                self.stream.spec(),
                value,
                sequence,
                next_offset,
                previous_block,
                previous_key,
            )?;
            let exceeds_records = records.len() >= max_records;
            let exceeds_bytes = !bytes.is_empty()
                && bytes
                    .len()
                    .checked_add(record.bytes.len())
                    .is_none_or(|len| len > options.max_bytes);
            if exceeds_records || exceeds_bytes {
                self.publish_prepared_chunk(&bytes, &records, written)?;
                bytes.clear();
                records.clear();
                self.ensure_batch()?;
            }

            let old_eof = next_offset;
            next_offset = next_offset.checked_add(record.bytes.len() as u64).ok_or(
                Error::ResourceArithmeticOverflow {
                    resource: "indexed batch native offset",
                },
            )?;
            let physical = prepared_physical(self.stream.spec(), &record)?;
            let update = DiskIndexUpdate::from_canonical_with_record(
                T::ID,
                canonical_key,
                DiskIndexEntry::Put {
                    record_offset: record.info.record_offset,
                    sequence,
                },
                physical,
                self.index.max_key_bytes(),
            )
            .map_err(index_error)?;
            let tail = record_tail(self.stream.spec(), T::ID, record.info);
            let fits_index = self
                .batch
                .as_ref()
                .expect("batch initialized")
                .can_accept_update(&update, tail.is_some())
                .map_err(index_error)?;
            if !fits_index && !records.is_empty() {
                self.publish_prepared_chunk(&bytes, &records, written)?;
                bytes.clear();
                records.clear();
                self.ensure_batch()?;
            }
            self.batch
                .as_mut()
                .expect("batch initialized")
                .apply_update_with_tail(old_eof, next_offset, &update, tail)
                .map_err(index_error)?;

            next_sequence = sequence.checked_add(1);
            previous_block = Some(record.info.record_offset);
            reserve_prepared(&mut bytes, &mut records, record.bytes.len())?;
            bytes.extend_from_slice(&record.bytes);
            records.push((T::ID, record.info));
            self.batch_records += 1;
            self.batch_last_sequence = Some(sequence);

            if bytes.len() >= options.max_bytes || records.len() >= max_records {
                self.publish_prepared_chunk(&bytes, &records, written)?;
                bytes.clear();
                records.clear();
            }
        }
        if !records.is_empty() {
            self.publish_prepared_chunk(&bytes, &records, written)?;
        }
        Ok(())
    }

    fn push_unindexed_iter_inner<T, I>(
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
        let options = options.validate()?;
        let _permit = self.ensure_not_poisoned()?;
        ensure_registered_block::<T>(self.stream.spec())?;
        if self.indexed_blocks.contains(&T::ID) {
            return Err(index_invariant(
                "disk-indexed blocks cannot use the unindexed batch path",
            ));
        }
        if self.stream.spec().index_policy.keyed_offset_chain && T::IS_KEYED {
            return Err(Error::StreamingUnsupported);
        }
        self.commit_pending_batch()?;

        let max_records = options.max_records.min(INDEX_BATCH_RECORDS);
        let mut bytes = Vec::new();
        let mut records = Vec::new();
        let mut next_sequence = Some(self.stream.next_sequence()?);
        let mut next_offset = self.stream.snapshot().len();
        let mut previous_block = self.stream.previous_block(T::ID);

        for value in values {
            self.ensure_batch()?;
            let sequence = next_sequence.ok_or(Error::SequenceExhausted)?;
            let record = prepare_stream_user_record(
                self.stream.spec(),
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
                self.publish_prepared_chunk(&bytes, &records, written)?;
                bytes.clear();
                records.clear();
                self.ensure_batch()?;
            }

            let old_eof = next_offset;
            next_offset = next_offset.checked_add(record.bytes.len() as u64).ok_or(
                Error::ResourceArithmeticOverflow {
                    resource: "unindexed batch native offset",
                },
            )?;
            let tail = record_tail(self.stream.spec(), T::ID, record.info);
            let fits_index = self
                .batch
                .as_ref()
                .expect("batch initialized")
                .can_accept_coverage(tail.is_some())
                .map_err(index_error)?;
            if !fits_index && !records.is_empty() {
                self.publish_prepared_chunk(&bytes, &records, written)?;
                bytes.clear();
                records.clear();
                self.ensure_batch()?;
            }
            self.batch
                .as_mut()
                .expect("batch initialized")
                .advance_coverage_with_tail(old_eof, next_offset, sequence, tail)
                .map_err(index_error)?;

            next_sequence = sequence.checked_add(1);
            previous_block = Some(record.info.record_offset);
            reserve_prepared(&mut bytes, &mut records, record.bytes.len())?;
            bytes.extend_from_slice(&record.bytes);
            records.push((T::ID, record.info));
            self.batch_records += 1;
            self.batch_last_sequence = Some(sequence);

            if bytes.len() >= options.max_bytes || records.len() >= max_records {
                self.publish_prepared_chunk(&bytes, &records, written)?;
                bytes.clear();
                records.clear();
            }
        }
        if !records.is_empty() {
            self.publish_prepared_chunk(&bytes, &records, written)?;
        }
        Ok(())
    }

    fn publish_prepared_chunk(
        &mut self,
        bytes: &[u8],
        records: &[(u32, AppendInfo)],
        written: &mut BatchAppendInfo,
    ) -> Result<()> {
        // One fresh poison check per published chunk, not one per batch: a
        // chunk earlier in the same batch can have poisoned the writer.
        let permit = self.ensure_not_poisoned()?;
        self.stream
            .append_prepared_chunk_summarized(permit, bytes, records, written)?;
        self.commit_pending_batch()
    }

    /// Commits the pending sidecar transaction, classifying *every* failure
    /// against whether natively published records are staged in it.
    ///
    /// Invariant 3: see `VarveStreamWriter::commit_state_chunk`. The
    /// single-record path already wrapped this call (in `finish_record`) and
    /// the batch path did not, so the wrapping now lives inside the function
    /// that owns the post-publication work, where it covers both callers and
    /// any future one.
    fn commit_pending_batch(&mut self) -> Result<()> {
        let staged = self.batch_last_sequence;
        match self.commit_pending_batch_inner() {
            Ok(()) => Ok(()),
            Err(error) => Err(self.published_index_error(staged, error)),
        }
    }

    /// Wraps `error` as a published outcome when `staged` names records that
    /// are already in the native file, and poisons the writer.
    fn published_index_error(&mut self, staged: Option<u64>, error: Error) -> Error {
        let Some(sequence) = staged else {
            return error;
        };
        self.stream.poison();
        match error {
            already @ Error::PublishedButIndexStale { .. } => already,
            source => Error::PublishedButIndexStale {
                sequence,
                source: Box::new(source),
            },
        }
    }

    fn commit_pending_batch_inner(&mut self) -> Result<()> {
        // STO-01: see VarveStreamWriter::commit_state_chunk. The witness stops
        // being recomputed once its bounded window is full.
        if let Some(batch) = self.batch.as_ref()
            && batch.primary_generation().len < PRIMARY_GENERATION_WINDOW
        {
            crate::stream::take_injected_generation_restamp_failure()?;
            let generation = primary_generation(
                self.stream.spec(),
                self.stream.snapshot(),
                self.stream.snapshot().len(),
            )?;
            self.batch
                .as_mut()
                .expect("batch present")
                .set_primary_generation(generation)
                .map_err(index_error)?;
        }
        let Some(batch) = self.batch.take() else {
            return Ok(());
        };
        if let Err(error) = batch.commit() {
            // Classified by `commit_pending_batch`: with staged records this
            // becomes `PublishedButIndexStale`; with none there is nothing
            // published to report and the plain sidecar error is the truth.
            return Err(index_error(error));
        }
        self.batch_records = 0;
        self.batch_last_sequence = None;
        Ok(())
    }

    fn publish_update(
        &mut self,
        old_eof: u64,
        info: AppendInfo,
        update: DiskIndexUpdate,
        tail: Option<DiskIndexTail>,
    ) -> Result<AppendInfo> {
        if let Err(error) = self
            .batch
            .as_mut()
            .expect("batch initialized")
            .apply_update_with_tail(old_eof, self.stream.snapshot().len(), &update, tail)
        {
            self.stream.poison();
            return Err(Error::PublishedButIndexStale {
                sequence: info.sequence,
                source: Box::new(index_error(error)),
            });
        }
        self.finish_record(info.sequence)?;
        Ok(info)
    }

    fn publish_coverage<T: VarveBlock>(
        &mut self,
        old_eof: u64,
        info: AppendInfo,
        tombstone: bool,
    ) -> Result<AppendInfo> {
        let _expected_id = if tombstone { TOMBSTONE_BLOCK_ID } else { T::ID };
        let result = self
            .batch
            .as_mut()
            .expect("batch initialized")
            .advance_coverage_with_tail(
                old_eof,
                self.stream.snapshot().len(),
                info.sequence,
                record_tail(self.stream.spec(), _expected_id, info),
            )
            .map(|_| ())
            .map_err(index_error);
        if let Err(error) = result {
            self.stream.poison();
            return Err(Error::PublishedButIndexStale {
                sequence: info.sequence,
                source: Box::new(error),
            });
        }
        self.finish_record(info.sequence)?;
        Ok(info)
    }

    fn finish_record(&mut self, sequence: u64) -> Result<()> {
        self.batch_records += 1;
        self.batch_last_sequence = Some(sequence);
        if self.batch_records < INDEX_BATCH_RECORDS {
            return Ok(());
        }
        self.commit_pending_batch().map_err(|error| match error {
            Error::PublishedButIndexStale { .. } => error,
            source => Error::PublishedButIndexStale {
                sequence,
                source: Box::new(source),
            },
        })
    }

    fn ensure_update_capacity(&mut self, update: &DiskIndexUpdate, has_tail: bool) -> Result<()> {
        let fits = self
            .batch
            .as_ref()
            .expect("batch initialized")
            .can_accept_update(update, has_tail)
            .map_err(index_error)?;
        if fits {
            return Ok(());
        }
        if self.batch_records != 0 {
            self.commit_pending_batch()?;
            self.ensure_batch()?;
        }
        if self
            .batch
            .as_ref()
            .expect("batch initialized")
            .can_accept_update(update, has_tail)
            .map_err(index_error)?
        {
            Ok(())
        } else {
            Err(index_invariant(
                "one disk-index update exceeds the configured sidecar batch bounds",
            ))
        }
    }

    fn ensure_coverage_capacity(&mut self, has_tail: bool) -> Result<()> {
        let fits = self
            .batch
            .as_ref()
            .expect("batch initialized")
            .can_accept_coverage(has_tail)
            .map_err(index_error)?;
        if fits {
            return Ok(());
        }
        if self.batch_records != 0 {
            self.commit_pending_batch()?;
            self.ensure_batch()?;
        }
        if self
            .batch
            .as_ref()
            .expect("batch initialized")
            .can_accept_coverage(has_tail)
            .map_err(index_error)?
        {
            Ok(())
        } else {
            Err(index_invariant(
                "one sidecar coverage update exceeds the configured batch bounds",
            ))
        }
    }

    fn ensure_batch(&mut self) -> Result<()> {
        if !self.dirty {
            self.index.mark_dirty().map_err(index_error)?;
            self.dirty = true;
        }
        if self.batch.is_none() {
            self.batch = Some(self.index.begin_write_batch().map_err(index_error)?);
        }
        Ok(())
    }

    fn ensure_writable<T: VarveKeyedBlock>(&self) -> Result<StreamMutationPermit> {
        let permit = self.ensure_not_poisoned()?;
        ensure_registered_block::<T>(self.stream.spec())?;
        if self.indexed_blocks.contains(&T::ID) {
            Ok(permit)
        } else {
            Err(index_invariant(
                "block is not declared with key_index = disk",
            ))
        }
    }

    /// The writer's poison guard, and the only source of the witness the
    /// stream's mutating entry points demand.
    ///
    /// Round 12, F-05: this used to read a *second* poison flag owned by this
    /// struct, which could and did disagree with the stream's own. When a
    /// native append failed and its truncate-or-seek rollback failed too, the
    /// stream poisoned itself and this flag stayed clear, so a later
    /// single-record put or delete passed this guard and then called
    /// `append_prepared_chunk` directly — a method that did not consult the
    /// stream's flag either. The two flags are now one: this reads the
    /// stream's flag, and every site in this file that used to set the local
    /// one now calls `VarveStreamWriter::poison`. The indexed writer cannot
    /// present itself as healthy after any stream error, and the stream cannot
    /// present itself as healthy after any indexed error.
    fn ensure_not_poisoned(&self) -> Result<StreamMutationPermit> {
        self.stream.mutation_permit(INDEXED_WRITER_POISON_CONTEXT)
    }
}

impl Drop for VarveIndexedWriter {
    fn drop(&mut self) {
        // redb waits for active write transactions while closing the database.
        // Abort the bounded batch before field drop reaches the store owner.
        self.batch.take();
    }
}

fn rebuild_index<F>(
    native_path: &Path,
    options: DiskIndexOptions,
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    plan: DiskIndexPlan,
    scan: ScanOptions<'_>,
    mut observer: F,
) -> Result<DiskIndexRebuildReport>
where
    F: FnMut(ScanProgress),
{
    let mut header_file = snapshot.try_clone_file()?;
    let header_eof = read_file_header(spec, &mut header_file)?;
    let identity = primary_identity(spec, snapshot)?;
    let mut progress =
        crate::scan_control::ScanProgressDriver::new(scan, header_eof, snapshot.len());
    progress
        .start(&mut observer)
        .map_err(crate::stream::scan_cancelled)?;
    let path = sidecar_path(native_path);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::Builder::new()
        .prefix(".varve-index-")
        .suffix(".vki.tmp")
        .tempfile_in(parent)?
        .into_temp_path();
    fs::remove_file(&temporary)?;
    let metadata = DiskIndexMetadata::new_disk(
        identity,
        plan.digest(),
        DiskIndexFrontier::empty(header_eof),
        tail_limit_for_spec(spec).map_err(index_error)?,
    );
    let store = DiskIndexStore::create(&temporary, options, metadata).map_err(index_error)?;
    let mut batch = store.begin_write_batch().map_err(index_error)?;
    let mut batch_records = 0usize;
    let mut records = 0u64;
    // Under CRC policies the sequential scanner would stream every record's
    // payload through the checksum pre-pass, and `extract_plan_update` would
    // then materialize each indexed payload a second time. Scan frames only:
    // extraction performs the single payload read for indexed records
    // (tombstones and plan puts) and verifies the record checksum over that
    // in-memory buffer against the real spec. Payloads of records outside the
    // plan are not read at all; `VarveStreamReader::verify_all` remains the
    // whole-file integrity scan.
    let scan_spec = spec.with_integrity_policy(crate::IntegrityPolicy::None);
    let mut scanner = NativeStreamScanner::from_snapshot(scan_spec, snapshot.clone())?;
    debug_assert_eq!(
        scanner.spec().integrity_policy,
        crate::IntegrityPolicy::None,
        "rebuild requires an integrity-stripped scanner spec",
    );
    let mut covered = header_eof;
    while let Some(entry) = scanner.next_entry()? {
        let end = entry.checked_physical_end()?;
        // Resolves the descriptor once per record by block id and decodes each
        // tombstone key exactly once, independent of the plan size.
        let update = crate::disk_index::extract_plan_update(
            spec,
            snapshot,
            plan,
            &entry,
            options.max_key_bytes,
        )?;
        let tail = record_tail(spec, entry.block_id, AppendInfo::from(&entry));
        let has_tail = tail.is_some();
        let fits = if let Some(update) = update.as_ref() {
            batch
                .can_accept_update(update, has_tail)
                .map_err(index_error)?
        } else {
            batch.can_accept_coverage(has_tail).map_err(index_error)?
        };
        if !fits && batch_records != 0 {
            batch
                .set_primary_generation(primary_generation(spec, snapshot, covered)?)
                .map_err(index_error)?;
            batch.commit().map_err(index_error)?;
            batch = store.begin_write_batch().map_err(index_error)?;
            batch_records = 0;
        }
        let fits = if let Some(update) = update.as_ref() {
            batch
                .can_accept_update(update, has_tail)
                .map_err(index_error)?
        } else {
            batch.can_accept_coverage(has_tail).map_err(index_error)?
        };
        if !fits {
            return Err(index_invariant(
                "one rebuild update exceeds the configured sidecar batch bounds",
            ));
        }
        if let Some(update) = update {
            batch
                .apply_update_with_tail(covered, end, &update, tail)
                .map_err(index_error)?;
        } else {
            batch
                .advance_coverage_with_tail(covered, end, entry.sequence, tail)
                .map_err(index_error)?;
        }
        covered = end;
        records = records
            .checked_add(1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "disk index rebuild record count",
            })?;
        batch_records += 1;
        progress
            .record_completed(scanner.logical_eof(), &mut observer)
            .map_err(crate::stream::scan_cancelled)?;
    }
    batch
        .set_primary_generation(primary_generation(spec, snapshot, covered)?)
        .map_err(index_error)?;
    batch.commit().map_err(index_error)?;
    let working = store.read_metadata().map_err(index_error)?.working;
    if working.eof != covered || working.record_count != records {
        return Err(Error::InvalidIndexCheckpoint);
    }
    store.publish_clean(working).map_err(index_error)?;
    drop(store);
    progress
        .complete(&mut observer)
        .map_err(crate::stream::scan_cancelled)?;
    // The native pathname can be atomically replaced by another generation
    // while the rebuild scans its retained snapshot. Re-resolve the pathname
    // just before publication and refuse to publish a sidecar for a primary
    // object the scan never observed.
    // F-09: the verified primary is held open across publication. It excludes
    // nothing a `WriterLock` does not already exclude, but it does stop the
    // operating system from handing the scanned object's identity to a
    // different file while the sidecar is published, so the post-publication
    // re-check below cannot be fooled by an identity that was recycled.
    let current = SnapshotFile::new(fs::File::open(native_path)?)?;
    if primary_identity(spec, &current)? != identity {
        return Err(index_error(DiskIndexError::IdentityMismatch));
    }
    // Drop the process-local shared-database entry for the file being
    // replaced so no later open can upgrade a database backed by the old
    // file object, even if the OS reuses its native identity.
    crate::disk_index::invalidate_shared_database(&path);
    // DUR2-01: publication takes ownership of the temp guard so an
    // indeterminate outcome preserves the replacement for out-of-band
    // reconciliation instead of the guard blind-deleting it by pathname.
    let durability = publish_temp_path_atomically(temporary, &path)?;
    #[cfg(test)]
    crate::stream::interpose_after_state_publication(native_path);
    // F-09: publication is not one syscall with the identity check above. If
    // the primary was replaced in between, the sidecar now at `path` was built
    // for a file object the pathname no longer names, and leaving it there
    // would displace whatever newer sidecar arrived with the replacement.
    // Retire it and report a typed mismatch.
    //
    // Neither step below may become a bare `Err` on its own: the sidecar is
    // already published, so a primary that cannot be re-read is resolved
    // *against* publication (fail closed, retire the sidecar) instead of
    // surfacing as a plain IO error that leaves the caller unable to tell what
    // is on disk.
    // Held open, not merely stat-ed: the retirement below deletes by pathname
    // and can only tell "still my sidecar" from "somebody else's file at the
    // same name" while this handle keeps the identity from being reissued.
    let published = crate::file::PinnedObject::open(&path).ok();
    let republished = fs::File::open(native_path)
        .ok()
        .and_then(|file| SnapshotFile::new(file).ok())
        .and_then(|snapshot| primary_identity(spec, &snapshot).ok());
    if republished.as_ref() != Some(&identity) {
        return Err(index_error(
            crate::stream::retire_sidecar_for_replaced_primary(&path, published),
        ));
    }
    drop(current);
    match durability {
        ReplaceDurability::Durable => {}
        ReplaceDurability::ParentSyncPending(sync_error) => {
            // The rebuilt sidecar is already visible at `path`; only the
            // parent-directory entry's durability is pending. Preserve the
            // published sidecar and surface the durability gap exactly like
            // the native and matrix publication paths.
            return Err(Error::PublishedButParentSyncPending {
                path: path.display().to_string(),
                source: Box::new(sync_error),
            });
        }
    }
    Ok(DiskIndexRebuildReport {
        records,
        scanned_bytes: covered.saturating_sub(header_eof),
    })
}

fn reject_dirty_rebuild_source(native_path: &Path, options: DiskIndexOptions) -> Result<()> {
    let path = sidecar_path(native_path);
    if !path.exists() {
        return Ok(());
    }
    if read_metadata_read_only(&path, options)
        .map_err(index_error)?
        .state
        == DiskIndexState::Dirty
    {
        return Err(index_error(DiskIndexError::CleanStateRequired));
    }
    Ok(())
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
    StreamCheckpoint {
        logical_eof: state.metadata.committed.eof,
        record_count: state.metadata.committed.record_count,
        next_sequence: state.metadata.committed.next_sequence,
        block_tails: state
            .tails
            .iter()
            .map(|tail| StreamTail {
                block_id: tail.block_id,
                record_offset: tail.record_offset,
                sequence: tail.sequence,
            })
            .collect(),
    }
}

fn create_index_store(
    native_path: &Path,
    options: DiskIndexOptions,
    metadata: DiskIndexMetadata,
    tails: &[DiskIndexTail],
) -> Result<DiskIndexStore> {
    let sidecar = sidecar_path(native_path);
    let parent = sidecar.parent().unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::Builder::new()
        .prefix(".varve-index-")
        .suffix(".vki.tmp")
        .tempfile_in(parent)?
        .into_temp_path();
    fs::remove_file(&temporary)?;
    crate::scalable_fault_point("create.sidecar_complete");
    let store = DiskIndexStore::create_with_tails(&temporary, options, metadata, tails)
        .map_err(index_error);
    crate::scalable_fault_point("create.sidecar_complete");
    let store = store?;
    drop(store);
    // See `rebuild_index`: the replaced file's shared-database entry must not
    // survive publication of the new sidecar object.
    crate::disk_index::invalidate_shared_database(&sidecar);
    // DUR2-01: the temp guard is handed to publication so an indeterminate
    // outcome preserves the replacement for reconciliation.
    match publish_temp_path_atomically(temporary, &sidecar)? {
        ReplaceDurability::Durable => {}
        ReplaceDurability::ParentSyncPending(sync_error) => {
            // Publication already happened: keep the published sidecar and
            // report the pending parent-directory durability instead of
            // silently promoting it to full success.
            return Err(Error::PublishedButParentSyncPending {
                path: sidecar.display().to_string(),
                source: Box::new(sync_error),
            });
        }
    }
    DiskIndexStore::open_validated(&sidecar, options, metadata.identity, metadata.mode)
        .map_err(index_error)
}

fn canonical_primary_path(path: &Path) -> Result<std::path::PathBuf> {
    if path.exists() {
        return Ok(std::fs::canonicalize(path)?);
    }
    let parent = std::fs::canonicalize(path.parent().unwrap_or_else(|| Path::new(".")))?;
    let name = path
        .file_name()
        .ok_or(Error::InvalidFormatSpec("file path has no name"))?;
    Ok(parent.join(name))
}

fn previous_offset(entry: DiskIndexEntry) -> Option<u64> {
    match entry {
        DiskIndexEntry::Put { record_offset, .. }
        | DiskIndexEntry::Tombstone { record_offset, .. } => Some(record_offset),
        DiskIndexEntry::Missing => None,
    }
}

/// Reads only the record frame (header, and footer when the spec carries one)
/// at an extent-validated offset, without touching the payload.
///
/// Unlike `read_stream_entry_at`, this performs no payload checksum pre-pass.
/// It is only sound for index candidates whose caller both matches the entry
/// against the sidecar's CRC-protected physical descriptor
/// (`validate_physical_candidate`) and then reads the payload through
/// `RecordIndexEntry::read_logical_payload_snapshot`, which re-reads the
/// frame and verifies the full record checksum over the in-memory payload.
fn read_stream_entry_frame(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    offset: u64,
    physical_len: u64,
) -> Result<RecordIndexEntry> {
    let validated = snapshot.validate(
        UntrustedRecordPointer::new(offset, physical_len),
        spec.spec_needs_record_footer(),
    )?;
    let span = validated.span();
    let mut header_bytes = [0u8; RECORD_HEADER_LEN as usize];
    snapshot.read_exact_at(offset, &mut header_bytes)?;
    let fields = read_native_record_header(&mut header_bytes.as_slice(), offset)?.fields;
    let mut entry = RecordIndexEntry {
        block_id: fields.block_id,
        block_version: fields.block_version,
        flags: fields.flags,
        sequence: fields.sequence,
        record_offset: span.record_offset().get(),
        payload_offset: span.payload_offset().get(),
        payload_len: fields.payload_len,
        checksum: fields.checksum,
        uncompressed_len_hint: fields.uncompressed_len_hint,
        footer_offset: None,
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        committed: true,
    };
    if spec.spec_needs_record_footer() {
        let footer_offset = span.footer_offset().get();
        let mut footer_bytes = [0u8; RECORD_FOOTER_LEN as usize];
        snapshot.read_exact_at(footer_offset, &mut footer_bytes)?;
        let footer = decode_native_record_footer(&footer_bytes, footer_offset, offset)?;
        entry.footer_offset = Some(footer_offset);
        entry.prev_same_block_offset = footer.prev_same_block_offset;
        entry.prev_same_key_offset = footer.prev_same_key_offset;
    }
    // The header's own length claim must agree with the extent-validated span.
    if entry.payload_len != span.payload_len().get()
        || entry.checked_physical_end()? != span.end().get()
    {
        return Err(Error::InvalidIndexCheckpoint);
    }
    Ok(entry)
}

fn validate_physical_candidate(
    spec: FormatSpec,
    entry: &crate::RecordIndexEntry,
    physical: DiskIndexPhysicalRecord,
) -> Result<()> {
    if entry.block_id != physical.block_id
        || entry.block_version != physical.block_version
        || entry.flags != physical.flags
    {
        return Err(Error::InvalidIndexCheckpoint);
    }
    let expected_crc =
        (spec.integrity_policy != crate::IntegrityPolicy::None).then_some(entry.checksum);
    if physical.native_crc != expected_crc {
        return Err(Error::InvalidIndexCheckpoint);
    }
    Ok(())
}

fn prepared_physical(
    spec: FormatSpec,
    record: &crate::file::PreparedStreamRecord,
) -> Result<DiskIndexPhysicalRecord> {
    let physical_len =
        u64::try_from(record.bytes.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
    Ok(DiskIndexPhysicalRecord {
        physical_len,
        block_id: record.block_id,
        block_version: record.block_version,
        flags: record.flags,
        native_crc: (spec.integrity_policy != crate::IntegrityPolicy::None)
            .then_some(record.checksum),
    })
}

fn record_tail(spec: FormatSpec, block_id: u32, info: AppendInfo) -> Option<DiskIndexTail> {
    spec.index_policy
        .block_offset_chain
        .then_some(DiskIndexTail {
            block_id,
            record_offset: info.record_offset,
            sequence: info.sequence,
        })
}

fn reserve_prepared(
    bytes: &mut Vec<u8>,
    records: &mut Vec<(u32, AppendInfo)>,
    record_len: usize,
) -> Result<()> {
    bytes
        .try_reserve(record_len)
        .map_err(|_| Error::AllocationFailed {
            resource: "indexed batch record bytes",
            requested: record_len as u64,
        })?;
    records
        .try_reserve(1)
        .map_err(|_| Error::AllocationFailed {
            resource: "indexed batch record descriptors",
            requested: std::mem::size_of::<(u32, AppendInfo)>() as u64,
        })?;
    Ok(())
}

fn index_error(error: DiskIndexError) -> Error {
    match error {
        DiskIndexError::Busy => Error::IndexBusy,
        error => Error::DiskIndex(Box::new(error)),
    }
}

fn index_invariant(reason: &'static str) -> Error {
    index_error(DiskIndexError::CheckpointMismatch(reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BlockDescriptor, BlockKind, Decoder, Encoder, IndexPolicy, IntegrityPolicy, ManifestPolicy,
        ReadLimits, RecoveryPolicy, VarveBlock, VarveDecode, VarveEncode, WireType,
    };

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Item {
        key: u64,
        value: String,
    }

    impl VarveEncode for Item {
        const WIRE_TYPE: WireType = <(u64, String) as VarveEncode>::WIRE_TYPE;

        fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
            (self.key, self.value.clone()).encode_varve(encoder)
        }
    }

    impl VarveDecode for Item {
        const WIRE_TYPE: WireType = <(u64, String) as VarveDecode>::WIRE_TYPE;

        fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
            let (key, value) = <(u64, String)>::decode_varve(decoder)?;
            Ok(Self { key, value })
        }
    }

    impl VarveBlock for Item {
        const ID: u32 = 7;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Variable;
        const ENDIAN: Option<crate::Endian> = None;
        const SCHEMA_FINGERPRINT: u64 = 0x96935207C3557FD5;
        const IS_KEYED: bool = true;
    }

    impl VarveKeyedBlock for Item {
        type Key = u64;

        fn key(&self) -> Self::Key {
            self.key
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Item2 {
        key: u64,
        value: String,
    }

    impl VarveEncode for Item2 {
        const WIRE_TYPE: WireType = <(u64, String) as VarveEncode>::WIRE_TYPE;

        fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
            (self.key, self.value.clone()).encode_varve(encoder)
        }
    }

    impl VarveDecode for Item2 {
        const WIRE_TYPE: WireType = <(u64, String) as VarveDecode>::WIRE_TYPE;

        fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
            let (key, value) = <(u64, String)>::decode_varve(decoder)?;
            Ok(Self { key, value })
        }
    }

    impl VarveBlock for Item2 {
        const ID: u32 = 9;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Variable;
        const ENDIAN: Option<crate::Endian> = None;
        const SCHEMA_FINGERPRINT: u64 = 0x31D44A30EA485585;
        const IS_KEYED: bool = true;
    }

    impl VarveKeyedBlock for Item2 {
        type Key = u64;

        fn key(&self) -> Self::Key {
            self.key
        }
    }

    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: Item::ID,
        name: "Item",
        version: Item::VERSION,
        kind: Item::KIND,
        fields: &[],
    }];

    static BLOCKS_MULTI: &[BlockDescriptor] = &[
        BlockDescriptor {
            id: Item::ID,
            name: "Item",
            version: Item::VERSION,
            kind: Item::KIND,
            fields: &[],
        },
        BlockDescriptor {
            id: Item2::ID,
            name: "Item2",
            version: Item2::VERSION,
            kind: Item2::KIND,
            fields: &[],
        },
    ];

    fn index_plan() -> DiskIndexPlan {
        const INDEXED: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Item>()];
        DiskIndexPlan::canonical(spec(), INDEXED).unwrap()
    }

    fn spec_multi() -> FormatSpec {
        FormatSpec::new(
            b"VIDY",
            1,
            crate::Endian::Little,
            0,
            IndexPolicy::KeyedOffsetChain,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS_MULTI,
        )
        .with_read_limits(ReadLimits::STANDARD)
    }

    fn plan_multi() -> DiskIndexPlan {
        const INDEXED: &[DiskIndexedBlock] = &[
            DiskIndexedBlock::of::<Item>(),
            DiskIndexedBlock::of::<Item2>(),
        ];
        DiskIndexPlan::canonical(spec_multi(), INDEXED).unwrap()
    }

    fn spec() -> FormatSpec {
        FormatSpec::new(
            b"VIDX",
            1,
            crate::Endian::Little,
            0,
            IndexPolicy::KeyedOffsetChain,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .with_read_limits(ReadLimits::STANDARD)
    }

    #[test]
    fn indexed_roundtrip_overwrite_tombstone_reopen_and_rebuild() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("items.varve");
        let options = DiskIndexOptions::default();

        let mut writer = VarveIndexedWriter::create(spec(), &path, options, index_plan())?;
        assert!(matches!(
            writer.push_unindexed_info(&Item {
                key: 99,
                value: "must reject".into(),
            }),
            Err(Error::DiskIndex(_))
        ));
        let first = writer.push_info(&Item {
            key: 11,
            value: "first".into(),
        })?;
        let second = writer.push_info(&Item {
            key: 11,
            value: "second".into(),
        })?;
        assert_eq!(second.prev_same_key_offset, Some(first.record_offset));
        writer.sync()?;
        drop(writer);

        let reader = VarveIndexedReader::open(spec(), &path, options, index_plan())?;
        assert_eq!(
            reader.get::<Item>(&11)?,
            Some(Item {
                key: 11,
                value: "second".into()
            })
        );
        drop(reader);

        let mut writer = VarveIndexedWriter::open(spec(), &path, options, index_plan())?;
        assert_eq!(
            writer.delete_info::<Item>(&11)?.prev_same_key_offset,
            Some(second.record_offset)
        );
        writer.sync()?;
        drop(writer);
        let reader = VarveIndexedReader::open(spec(), &path, options, index_plan())?;
        assert_eq!(reader.get::<Item>(&11)?, None);
        drop(reader);

        fs::remove_file(sidecar_path(&path))?;
        assert!(matches!(
            VarveIndexedWriter::open(spec(), &path, options, index_plan()),
            Err(Error::DiskIndex(_))
        ));
        let report = rebuild_disk_index(spec(), &path, options, index_plan())?;
        // Three user records plus the internal creation-nonce record (STO-01).
        assert_eq!(report.records, 4);
        assert!(report.scanned_bytes > 0);
        let mut writer = VarveIndexedWriter::open(spec(), &path, options, index_plan())?;
        writer.sync()?;
        drop(writer);
        let reader = VarveIndexedReader::open(spec(), &path, options, index_plan())?;
        assert_eq!(reader.get::<Item>(&11)?, None);
        Ok(())
    }

    #[test]
    fn equal_length_primary_files_reject_swapped_sidecars() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let first_path = directory.path().join("first.varve");
        let second_path = directory.path().join("second.varve");
        let options = DiskIndexOptions::default();

        for (path, key, value) in [(&first_path, 1, "aaaa"), (&second_path, 2, "bbbb")] {
            let mut writer = VarveIndexedWriter::create(spec(), path, options, index_plan())?;
            writer.push_info(&Item {
                key,
                value: value.into(),
            })?;
            writer.sync()?;
        }
        assert_eq!(
            fs::metadata(&first_path)?.len(),
            fs::metadata(&second_path)?.len()
        );
        fs::copy(sidecar_path(&first_path), sidecar_path(&second_path))?;
        let error = match VarveIndexedReader::open(spec(), &second_path, options, index_plan()) {
            Ok(_) => panic!("sidecar from another primary object must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, Error::DiskIndex(_)));
        Ok(())
    }

    #[test]
    fn clean_index_open_and_append_do_not_scan_native_records() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("constant-index.varve");
        let options = DiskIndexOptions::default();
        let values: Vec<_> = (0..128)
            .map(|key| Item {
                key,
                value: format!("value-{key}"),
            })
            .collect();
        {
            let mut writer = VarveIndexedWriter::create(spec(), &path, options, index_plan())?;
            writer
                .push_iter::<Item, _>(&values, BatchOptions::default())
                .map_err(|error| error.source)?;
            writer.sync()?;
        }

        crate::file::reset_stream_io_counters();
        let reader = VarveIndexedReader::open(spec(), &path, options, index_plan())?;
        assert_eq!(crate::file::stream_io_counters(), (0, 0, 0));
        assert_eq!(reader.get::<Item>(&64)?, Some(values[64].clone()));
        assert_eq!(crate::file::stream_io_counters(), (0, 0, 1));
        crate::file::reset_stream_io_counters();
        assert!(matches!(
            reader.lookup::<Item>(&64)?,
            DiskIndexEntry::Put { .. }
        ));
        assert_eq!(crate::file::stream_io_counters(), (0, 0, 1));
        drop(reader);

        crate::file::reset_stream_io_counters();
        let mut writer = VarveIndexedWriter::open(spec(), &path, options, index_plan())?;
        writer
            .push_iter::<Item, _>(
                [Item {
                    key: 128,
                    value: "value-128".into(),
                }],
                BatchOptions::default(),
            )
            .map_err(|error| error.source)?;
        writer.sync()?;
        assert_eq!(crate::file::stream_io_counters(), (0, 0, 0));
        Ok(())
    }

    #[test]
    fn indexed_mutations_encode_each_sidecar_key_once() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("single-key-encoding.varve");
        let options = DiskIndexOptions::default();
        let mut writer = VarveIndexedWriter::create(spec(), &path, options, index_plan())?;

        crate::disk_index::reset_disk_key_encode_calls();
        writer
            .push_iter::<Item, _>(
                [
                    Item {
                        key: 1,
                        value: "one".into(),
                    },
                    Item {
                        key: 2,
                        value: "two".into(),
                    },
                ],
                BatchOptions::default(),
            )
            .map_err(|error| error.source)?;
        assert_eq!(crate::disk_index::disk_key_encode_calls(), 2);

        crate::disk_index::reset_disk_key_encode_calls();
        writer.push_info(&Item {
            key: 1,
            value: "replacement".into(),
        })?;
        assert_eq!(crate::disk_index::disk_key_encode_calls(), 1);

        crate::disk_index::reset_disk_key_encode_calls();
        writer.delete_info::<Item>(&2)?;
        assert_eq!(crate::disk_index::disk_key_encode_calls(), 1);
        Ok(())
    }

    #[test]
    fn dirty_index_restore_uses_savepoint_without_native_scan() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("restore-index.varve");
        let options = DiskIndexOptions::default();
        {
            let mut writer = VarveIndexedWriter::create(spec(), &path, options, index_plan())?;
            writer.push_info(&Item {
                key: 1,
                value: "committed".into(),
            })?;
            writer.sync()?;
            writer.push_info(&Item {
                key: 2,
                value: "dirty".into(),
            })?;
        }
        assert!(matches!(
            VarveIndexedWriter::open(spec(), &path, options, index_plan()),
            Err(Error::DiskIndex(_))
        ));

        crate::file::reset_stream_io_counters();
        let writer =
            VarveIndexedWriter::restore_checkpoint_and_open(spec(), &path, options, index_plan())?;
        assert_eq!(crate::file::stream_io_counters(), (0, 0, 0));
        drop(writer);
        let reader = VarveIndexedReader::open(spec(), &path, options, index_plan())?;
        assert!(reader.get::<Item>(&1)?.is_some());
        assert_eq!(reader.get::<Item>(&2)?, None);
        Ok(())
    }

    #[test]
    fn rebuild_refuses_to_promote_an_existing_dirty_generation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("dirty-rebuild.varve");
        let options = DiskIndexOptions::default();
        {
            let mut writer = VarveIndexedWriter::create(spec(), &path, options, index_plan())?;
            writer.push_info(&Item {
                key: 1,
                value: "clean".into(),
            })?;
            writer.sync()?;
            writer.push_info(&Item {
                key: 2,
                value: "dirty".into(),
            })?;
        }
        let dirty_len = fs::metadata(&path)?.len();
        assert!(matches!(
            rebuild_disk_index(spec(), &path, options, index_plan()),
            Err(Error::DiskIndex(_))
        ));
        assert_eq!(fs::metadata(&path)?.len(), dirty_len);
        let store = DiskIndexStore::open(sidecar_path(&path), options).map_err(index_error)?;
        assert_eq!(
            store.read_metadata().map_err(index_error)?.state,
            DiskIndexState::Dirty
        );
        Ok(())
    }

    /// F-09, rebuild half. Same contract as the stream bootstrap test: a
    /// primary replaced between the pre-publication identity check and the
    /// sidecar becoming visible must produce a typed mismatch, and the sidecar
    /// this call published must not be left standing in place of whatever
    /// sidecar the replacement generation brought with it.
    #[test]
    fn rebuild_retires_a_sidecar_published_for_a_replaced_primary() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replaced-rebuild.varve");
        let replacement = directory.path().join("rebuild-replacement.varve");
        let options = DiskIndexOptions::default();
        for (target, key) in [(&path, 1u64), (&replacement, 2)] {
            let mut writer = VarveIndexedWriter::create(spec(), target, options, index_plan())?;
            writer.push_info(&Item {
                key,
                value: format!("value-{key}"),
            })?;
            writer.sync()?;
        }
        fs::remove_file(sidecar_path(&path))?;
        fs::remove_file(sidecar_path(&replacement))?;

        let swapped = path.clone();
        let source = replacement.clone();
        let interposed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = std::sync::Arc::clone(&interposed);
        crate::stream::set_publication_interposition(
            &fs::canonicalize(&path)?,
            Box::new(move || {
                assert!(
                    sidecar_path(&swapped).exists(),
                    "the interposition must run after the sidecar is published",
                );
                fs::rename(&source, &swapped).expect("replace the primary during publication");
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
            }),
        );

        match rebuild_disk_index(spec(), &path, options, index_plan()) {
            Err(Error::DiskIndex(error)) if matches!(*error, DiskIndexError::IdentityMismatch) => {}
            other => panic!("expected a typed identity mismatch, got {other:?}"),
        }
        assert!(
            interposed.load(std::sync::atomic::Ordering::SeqCst),
            "the swap never reached the publication interval under test",
        );
        assert!(
            !sidecar_path(&path).exists(),
            "a sidecar published for a replaced primary must not be left behind",
        );
        Ok(())
    }

    #[test]
    fn rebuild_splits_at_the_configured_sidecar_batch_bound() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("small-rebuild-batch.varve");
        let create_options = DiskIndexOptions::default();
        {
            let mut writer =
                VarveIndexedWriter::create(spec(), &path, create_options, index_plan())?;
            for key in 0..5 {
                writer.push_info(&Item {
                    key,
                    value: format!("value-{key}"),
                })?;
            }
            writer.sync()?;
        }
        fs::remove_file(sidecar_path(&path))?;
        let options = DiskIndexOptions {
            max_key_bytes: 1024,
            batch: crate::disk_index::DiskIndexBatchOptions {
                max_records: 2,
                max_bytes: 4096,
            },
            ..DiskIndexOptions::default()
        };
        let report = rebuild_disk_index(spec(), &path, options, index_plan())?;
        // Five user records plus the internal creation-nonce record (STO-01).
        assert_eq!(report.records, 6);
        let reader = VarveIndexedReader::open(spec(), &path, options, index_plan())?;
        assert_eq!(reader.get::<Item>(&4)?.unwrap().value, "value-4");
        Ok(())
    }

    #[test]
    fn finite_sidecar_read_policy_does_not_cap_index_lifetime() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("sidecar-policy.varve");
        let options = DiskIndexOptions::default();
        {
            let mut writer = VarveIndexedWriter::create(spec(), &path, options, index_plan())?;
            writer.push_info(&Item {
                key: 1,
                value: "value".into(),
            })?;
            writer.sync()?;
        }
        assert!(fs::metadata(sidecar_path(&path))?.len() > 1);
        let bounded = DiskIndexOptions {
            limits: crate::ResourceLimits::MISSING.with_max_sidecar_len(1),
            ..options
        };
        let reader = VarveIndexedReader::open(spec(), &path, bounded, index_plan())?;
        assert_eq!(reader.get::<Item>(&1)?.unwrap().value, "value");
        Ok(())
    }

    #[test]
    fn concurrent_handles_share_the_sidecar_database() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("typed-busy.varve");
        let options = DiskIndexOptions::default();
        let mut writer = VarveIndexedWriter::create(spec(), &path, options, index_plan())?;
        writer.push_info(&Item {
            key: 1,
            value: "one".into(),
        })?;
        writer.sync()?;
        // Independent handles share one process-local database, so a reader
        // opens beside a synced writer and serves the committed state.
        let reader = VarveIndexedReader::open(spec(), &path, options, index_plan())?;
        assert_eq!(reader.get::<Item>(&1)?.unwrap().value, "one");
        // A handle holding an uncommitted sidecar batch still yields a typed
        // busy error for conflicting opens instead of blocking inside redb.
        writer.push_info(&Item {
            key: 2,
            value: "two".into(),
        })?;
        assert!(matches!(
            VarveIndexedReader::open(spec(), &path, options, index_plan()),
            Err(Error::IndexBusy)
        ));
        // The pre-existing reader keeps serving its pinned snapshot.
        assert_eq!(reader.get::<Item>(&1)?.unwrap().value, "one");
        writer.sync()?;
        let fresh = VarveIndexedReader::open(spec(), &path, options, index_plan())?;
        assert_eq!(fresh.get::<Item>(&2)?.unwrap().value, "two");
        Ok(())
    }

    #[test]
    fn rebuild_decodes_each_tombstone_key_once_across_descriptors() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("single-decode-rebuild.varve");
        let options = DiskIndexOptions::default();
        {
            let mut writer =
                VarveIndexedWriter::create(spec_multi(), &path, options, plan_multi())?;
            writer.push_info(&Item {
                key: 1,
                value: "a".into(),
            })?;
            writer.push_info(&Item2 {
                key: 2,
                value: "b".into(),
            })?;
            writer.delete_info::<Item>(&1)?;
            writer.delete_info::<Item2>(&2)?;
            writer.sync()?;
        }
        fs::remove_file(sidecar_path(&path))?;
        crate::disk_index::reset_tombstone_key_decode_calls();
        let report = rebuild_disk_index(spec_multi(), &path, options, plan_multi())?;
        // Four user records plus the internal creation-nonce record (STO-01).
        assert_eq!(report.records, 5);
        // One decode per tombstone record, not one per plan descriptor.
        assert_eq!(crate::disk_index::tombstone_key_decode_calls(), 2);
        let reader = VarveIndexedReader::open(spec_multi(), &path, options, plan_multi())?;
        assert_eq!(reader.get::<Item>(&1)?, None);
        assert_eq!(reader.get::<Item2>(&2)?, None);
        // Tombstoned keys stay in the latest table: capacity follows K-ever.
        assert_eq!(reader.historical_distinct_keys()?, 2);
        Ok(())
    }

    /// PERF2-03: a CRC rebuild must traverse each indexed payload exactly
    /// once. The scanner runs with an integrity-stripped spec (frame reads
    /// only), and `extract_plan_update` owns the single checksum-verified
    /// payload read; payloads of records outside the plan are never
    /// materialized.
    #[cfg(feature = "integrity")]
    #[test]
    fn crc_rebuild_materializes_each_indexed_payload_exactly_once() -> Result<()> {
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct Blob(String);

        impl VarveEncode for Blob {
            const WIRE_TYPE: WireType = <String as VarveEncode>::WIRE_TYPE;

            fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
                self.0.encode_varve(encoder)
            }
        }

        impl VarveDecode for Blob {
            const WIRE_TYPE: WireType = <String as VarveDecode>::WIRE_TYPE;

            fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
                Ok(Self(String::decode_varve(decoder)?))
            }
        }

        impl VarveBlock for Blob {
            const ID: u32 = 12;
            const VERSION: u16 = 1;
            const KIND: BlockKind = BlockKind::Variable;
            const ENDIAN: Option<crate::Endian> = None;
            const SCHEMA_FINGERPRINT: u64 = 0x424C_4F42_0000_000C;
            const IS_KEYED: bool = false;
        }

        static BLOCKS_CRC: &[BlockDescriptor] = &[
            BlockDescriptor {
                id: Item::ID,
                name: "Item",
                version: Item::VERSION,
                kind: Item::KIND,
                fields: &[],
            },
            BlockDescriptor {
                id: Blob::ID,
                name: "Blob",
                version: Blob::VERSION,
                kind: Blob::KIND,
                fields: &[],
            },
        ];

        fn spec_crc() -> FormatSpec {
            FormatSpec::new(
                b"VIDC",
                1,
                crate::Endian::Little,
                0,
                IndexPolicy::KeyedOffsetChain,
                IntegrityPolicy::Crc32WithHeader,
                RecoveryPolicy::Strict,
                ManifestPolicy::None,
                BLOCKS_CRC,
            )
            .with_read_limits(ReadLimits::STANDARD)
        }
        fn plan_crc() -> DiskIndexPlan {
            const INDEXED: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Item>()];
            DiskIndexPlan::canonical(spec_crc(), INDEXED).unwrap()
        }

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("single-payload-read.varve");
        let options = DiskIndexOptions::default();
        {
            let mut writer = VarveIndexedWriter::create(spec_crc(), &path, options, plan_crc())?;
            writer.push_info(&Item {
                key: 1,
                value: "indexed-one".into(),
            })?;
            writer.push_info(&Item {
                key: 2,
                value: "indexed-two".into(),
            })?;
            // Blob is registered in the format but outside the plan: its
            // payload must not be materialized by the rebuild at all.
            writer.push_unindexed_info(&Blob("unindexed".into()))?;
            writer.delete_info::<Item>(&1)?;
            writer.sync()?;
        }
        fs::remove_file(sidecar_path(&path))?;

        crate::disk_index::reset_plan_payload_reads();
        let report = rebuild_disk_index(spec_crc(), &path, options, plan_crc())?;
        // Four user records plus the internal creation-nonce record (STO-01).
        assert_eq!(report.records, 5);
        // Two indexed puts and one tombstone: three payload materializations,
        // never a scanner checksum pre-pass on top and never the unindexed
        // record's payload.
        assert_eq!(crate::disk_index::plan_payload_reads(), 3);

        let reader = VarveIndexedReader::open(spec_crc(), &path, options, plan_crc())?;
        assert_eq!(reader.get::<Item>(&1)?, None);
        assert_eq!(reader.get::<Item>(&2)?.unwrap().value, "indexed-two");
        Ok(())
    }

    #[test]
    fn tombstone_records_use_the_mirrored_internal_flag() -> Result<()> {
        // Pins `disk_index::TOMBSTONE_RECORD_FLAGS` (a mirror of the private
        // `file::RECORD_FLAG_INTERNAL`) against the real tombstone writer.
        let record = prepare_stream_tombstone_record::<Item>(spec(), &7u64, 0, 64, None, None)?;
        assert_eq!(record.flags, crate::disk_index::TOMBSTONE_RECORD_FLAGS);
        assert_eq!(record.block_version, 1);
        Ok(())
    }

    #[test]
    fn point_lookup_stops_canonical_key_encoding_at_max_key_bytes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bounded-key.varve");
        let options = DiskIndexOptions {
            max_key_bytes: 8,
            ..DiskIndexOptions::default()
        };
        {
            let mut writer = VarveIndexedWriter::create(spec(), &path, options, index_plan())?;
            writer.push_info(&Item {
                key: 1,
                value: "value".into(),
            })?;
            writer.sync()?;
        }
        let reader = VarveIndexedReader::open(spec(), &path, options, index_plan())?;
        assert!(matches!(
            reader.index.lookup_pointer(Item::ID, &"x".repeat(1024)),
            Err(DiskIndexError::Primary(Error::LimitExceeded {
                resource: "disk index key",
                actual: 1032,
                limit: 8,
            }))
        ));
        Ok(())
    }

    #[test]
    fn sidecar_batch_bound_splits_before_native_publication() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bounded-sidecar.varve");
        let options = DiskIndexOptions {
            max_key_bytes: 1024,
            batch: crate::disk_index::DiskIndexBatchOptions {
                max_records: 2,
                max_bytes: 4096,
            },
            ..DiskIndexOptions::default()
        };
        let values: Vec<_> = (0..5)
            .map(|key| Item {
                key,
                value: format!("batch-{key}"),
            })
            .collect();
        let mut writer = VarveIndexedWriter::create(spec(), &path, options, index_plan())?;
        let report = writer
            .push_iter::<Item, _>(
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
        drop(writer);

        let mut writer = VarveIndexedWriter::open(spec(), &path, options, index_plan())?;
        for key in 5..10 {
            writer.push_info(&Item {
                key,
                value: format!("single-{key}"),
            })?;
        }
        writer.sync()?;
        drop(writer);
        let reader = VarveIndexedReader::open(spec(), &path, options, index_plan())?;
        assert_eq!(reader.get::<Item>(&9)?.unwrap().value, "single-9");
        Ok(())
    }
}
