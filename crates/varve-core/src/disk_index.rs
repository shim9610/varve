use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsString,
    fmt, fs,
    hash::Hash,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, PoisonError, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};

use redb::{
    Builder, Database, Durability, ReadTransaction, ReadableDatabase, ReadableTable,
    ReadableTableMetadata, TableDefinition, WriteTransaction,
};

use crate::collections::MaterializationBudget;
use crate::native_layout::decode_native_internal_key_envelope;
use crate::{
    Endian, Error, FormatSpec, IntegrityPolicy, RecordIndexEntry, ResourceLimits, SnapshotFile,
    TOMBSTONE_BLOCK_ID, VarveDecode, VarveEncode, VarveKeyedBlock, codec::encode_to_vec_limited,
};

const DEFAULT_CACHE_BYTES: usize = 8 * 1024 * 1024;
const MIN_CACHE_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_KEY_BYTES: usize = 1024 * 1024;
const DEFAULT_BATCH_RECORDS: usize = 16_384;
const DEFAULT_BATCH_BYTES: usize = 64 * 1024 * 1024;

const META_MAGIC: [u8; 8] = *b"VARVEVKI";
// v3 adds the primary generation witness (STO-01). A v2 sidecar is refused
// with the typed `MetadataVersion` error and is stale-regenerable by rebuild.
const META_VERSION: u16 = 3;
const META_LEN: usize = 300;
const LATEST_LEN: usize = 52;
const TAIL_LEN: usize = 32;
const BATCH_UPDATE_FIXED_BYTES: usize = 32 + 8 + LATEST_LEN;
const BATCH_TAIL_BYTES: usize = 4 + TAIL_LEN;
const META_KEY: &[u8] = b"state";

const META_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("meta");
const LATEST_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("latest");
const TAILS_TABLE: TableDefinition<u32, &[u8]> = TableDefinition::new("tails");

const SEQUENCE_COMMITTED_EXHAUSTED: u8 = 1 << 0;
const SEQUENCE_WORKING_EXHAUSTED: u8 = 1 << 1;
const SEQUENCE_BASE_EXHAUSTED: u8 = 1 << 2;
const SEQUENCE_FLAGS: u8 =
    SEQUENCE_COMMITTED_EXHAUSTED | SEQUENCE_WORKING_EXHAUSTED | SEQUENCE_BASE_EXHAUSTED;

const LATEST_HAS_PHYSICAL: u8 = 1 << 0;
const LATEST_HAS_NATIVE_CRC: u8 = 1 << 1;
const LATEST_FLAGS: u8 = LATEST_HAS_PHYSICAL | LATEST_HAS_NATIVE_CRC;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexBatchOptions {
    pub max_records: usize,
    pub max_bytes: usize,
}

impl Default for DiskIndexBatchOptions {
    fn default() -> Self {
        Self {
            max_records: DEFAULT_BATCH_RECORDS,
            max_bytes: DEFAULT_BATCH_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexOptions {
    pub cache_bytes: usize,
    pub max_key_bytes: usize,
    pub batch: DiskIndexBatchOptions,
    pub limits: ResourceLimits,
}

impl Default for DiskIndexOptions {
    fn default() -> Self {
        Self {
            cache_bytes: DEFAULT_CACHE_BYTES,
            max_key_bytes: DEFAULT_MAX_KEY_BYTES,
            batch: DiskIndexBatchOptions::default(),
            // redb performs paged access; its file length is not an allocation request.
            // Callers can opt into a finite sidecar policy per open when required.
            limits: ResourceLimits::MISSING.with_max_sidecar_len(u64::MAX),
        }
    }
}

impl DiskIndexOptions {
    fn validate(self) -> DiskIndexResult<Self> {
        if self.cache_bytes < MIN_CACHE_BYTES {
            return Err(DiskIndexError::CacheTooSmall {
                actual: self.cache_bytes,
                minimum: MIN_CACHE_BYTES,
            });
        }
        if self.max_key_bytes == 0 {
            return Err(DiskIndexError::InvalidBatchOptions(
                "max_key_bytes must be nonzero",
            ));
        }
        if self.max_key_bytes > u32::MAX as usize {
            return Err(DiskIndexError::InvalidBatchOptions(
                "max_key_bytes exceeds the sidecar key representation",
            ));
        }
        if self.batch.max_records == 0 {
            return Err(DiskIndexError::InvalidBatchOptions(
                "max_records must be nonzero",
            ));
        }
        if self.batch.max_bytes == 0 {
            return Err(DiskIndexError::InvalidBatchOptions(
                "max_bytes must be nonzero",
            ));
        }
        if self.batch.max_bytes < batch_item_fixed_bytes(true) {
            return Err(DiskIndexError::InvalidBatchOptions(
                "max_bytes is too small for one checkpoint item",
            ));
        }
        if self.max_key_bytes > self.batch.max_bytes {
            return Err(DiskIndexError::InvalidBatchOptions(
                "max_key_bytes must not exceed max_bytes",
            ));
        }
        Ok(self)
    }
}

pub type DiskIndexResult<T> = std::result::Result<T, DiskIndexError>;

/// Observability counters for the scaling contracts the release gates pin
/// (PERF-04 registry cost, API-03 codec invocation). Thread-local and only
/// compiled under `scalable-fault-injection`; the release build keeps the
/// call sites as empty inline functions.
#[cfg(feature = "scalable-fault-injection")]
pub(crate) mod scaling_counters {
    use std::cell::Cell;

    thread_local! {
        static REGISTRY_SLOTS_INSPECTED: Cell<u64> = const { Cell::new(0) };
        static DESCRIPTOR_DECODER_CALLS: Cell<u64> = const { Cell::new(0) };
        static PRIMARY_GENERATION_SCANS: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn add_registry_slots_inspected(slots: u64) {
        REGISTRY_SLOTS_INSPECTED.with(|value| value.set(value.get().saturating_add(slots)));
    }

    pub(crate) fn add_descriptor_decoder_call() {
        DESCRIPTOR_DECODER_CALLS.with(|value| value.set(value.get().saturating_add(1)));
    }

    pub(crate) fn add_primary_generation_scan() {
        PRIMARY_GENERATION_SCANS.with(|value| value.set(value.get().saturating_add(1)));
    }

    pub fn primary_generation_scans() -> u64 {
        PRIMARY_GENERATION_SCANS.with(Cell::get)
    }

    pub fn registry_slots_inspected() -> u64 {
        REGISTRY_SLOTS_INSPECTED.with(Cell::get)
    }

    pub fn descriptor_decoder_calls() -> u64 {
        DESCRIPTOR_DECODER_CALLS.with(Cell::get)
    }

    pub fn reset() {
        REGISTRY_SLOTS_INSPECTED.with(|value| value.set(0));
        DESCRIPTOR_DECODER_CALLS.with(|value| value.set(0));
        PRIMARY_GENERATION_SCANS.with(|value| value.set(0));
    }
}

#[inline(always)]
fn registry_slots_inspected(slots: u64) {
    #[cfg(feature = "scalable-fault-injection")]
    scaling_counters::add_registry_slots_inspected(slots);
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = slots;
}

#[inline(always)]
pub(crate) fn primary_generation_scan() {
    #[cfg(feature = "scalable-fault-injection")]
    scaling_counters::add_primary_generation_scan();
}

#[inline(always)]
fn descriptor_decoder_call() {
    #[cfg(feature = "scalable-fault-injection")]
    scaling_counters::add_descriptor_decoder_call();
}

#[derive(Debug)]
#[non_exhaustive]
pub enum DiskIndexError {
    CacheTooSmall {
        actual: usize,
        minimum: usize,
    },
    InvalidBatchOptions(&'static str),
    KeyTooLong {
        actual: usize,
        limit: usize,
    },
    MetadataMissing,
    MetadataLength {
        actual: usize,
        expected: usize,
    },
    MetadataMagic,
    MetadataVersion {
        actual: u16,
    },
    MetadataChecksum,
    MetadataReserved,
    MetadataState {
        actual: u8,
    },
    MetadataMode {
        actual: u8,
    },
    MetadataFlags {
        actual: u8,
    },
    MetadataInvariant(&'static str),
    LatestLength {
        actual: usize,
        expected: usize,
    },
    LatestChecksum,
    LatestReserved,
    LatestInvariant(&'static str),
    TailLength {
        actual: usize,
        expected: usize,
    },
    TailChecksum,
    TailReserved,
    TailKeyMismatch {
        key: u32,
        encoded: u32,
    },
    TailLimitExceeded {
        actual: u64,
        limit: u32,
    },
    TailLimitMismatch {
        expected: u32,
        actual: u32,
    },
    TailCountMismatch {
        expected: u32,
        actual: u32,
    },
    TailDigestMismatch,
    InvalidTail(&'static str),
    IdentityMismatch,
    /// The primary no longer matches the generation the sidecar was published
    /// against: the leading bytes of the file changed underneath it, outside a
    /// declared header tail region, which is excluded from the witness (STO-01).
    PrimaryGenerationMismatch,
    ModeMismatch {
        expected: DiskIndexMode,
        actual: DiskIndexMode,
    },
    PlanEmpty,
    PlanNotCanonical,
    PlanCodecIdentityMissing {
        block_id: u32,
    },
    PlanBlockMissing {
        block_id: u32,
    },
    PlanBlockVersionMismatch {
        block_id: u32,
        expected: u16,
        actual: u16,
    },
    PlanDigestMismatch,
    PlanDigestIsZero,
    CleanStateRequired,
    DirtyStateRequired,
    UnexpectedSavepoints {
        expected: Option<u64>,
        first_actual: Option<u64>,
        actual_count: usize,
    },
    CheckpointMismatch(&'static str),
    CoverageMismatch {
        expected: u64,
        actual: u64,
    },
    NativeTooShort {
        required: u64,
        actual: u64,
    },
    NativeLengthMismatch {
        expected: u64,
        actual: u64,
    },
    SequenceMismatch {
        expected: Option<u64>,
        actual: u64,
    },
    RecordCountExhausted,
    GenerationExhausted,
    RecordExtentMismatch {
        expected_end: u64,
        actual_end: u64,
    },
    BatchFull {
        records: usize,
        bytes: usize,
        max_records: usize,
        max_bytes: usize,
    },
    KeyTableUnavailable,
    Busy,
    SavepointOperation {
        operation: &'static str,
        error: String,
    },
    Storage(String),
    Primary(Error),
    Io(std::io::Error),
}

impl fmt::Display for DiskIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CacheTooSmall { actual, minimum } => write!(
                f,
                "disk index cache size {actual} is below the {minimum} byte minimum"
            ),
            Self::InvalidBatchOptions(reason) => {
                write!(f, "invalid disk index batch options: {reason}")
            }
            Self::KeyTooLong { actual, limit } => write!(
                f,
                "canonical disk index key is too long: {actual} bytes exceeds {limit}"
            ),
            Self::MetadataMissing => write!(f, "disk index metadata row is missing"),
            Self::MetadataLength { actual, expected } => write!(
                f,
                "disk index metadata has length {actual}; expected {expected}"
            ),
            Self::MetadataMagic => write!(f, "disk index metadata has the wrong magic"),
            Self::MetadataVersion { actual } => write!(
                f,
                "disk index metadata version {actual} is unsupported; expected {META_VERSION}"
            ),
            Self::MetadataChecksum => write!(f, "disk index metadata checksum mismatch"),
            Self::MetadataReserved => {
                write!(f, "disk index metadata reserved bytes are nonzero")
            }
            Self::MetadataState { actual } => {
                write!(f, "disk index metadata has unknown state {actual}")
            }
            Self::MetadataMode { actual } => {
                write!(f, "disk index metadata has unknown mode {actual}")
            }
            Self::MetadataFlags { actual } => {
                write!(f, "disk index metadata has unknown flags {actual:#04x}")
            }
            Self::MetadataInvariant(reason) => {
                write!(f, "disk index metadata invariant failed: {reason}")
            }
            Self::LatestLength { actual, expected } => write!(
                f,
                "disk index entry has length {actual}; expected {expected}"
            ),
            Self::LatestChecksum => write!(f, "disk index entry checksum mismatch"),
            Self::LatestReserved => write!(f, "disk index entry reserved bytes are nonzero"),
            Self::LatestInvariant(reason) => {
                write!(f, "disk index entry invariant failed: {reason}")
            }
            Self::TailLength { actual, expected } => write!(
                f,
                "disk index tail has length {actual}; expected {expected}"
            ),
            Self::TailChecksum => write!(f, "disk index tail checksum mismatch"),
            Self::TailReserved => write!(f, "disk index tail reserved bytes are nonzero"),
            Self::TailKeyMismatch { key, encoded } => write!(
                f,
                "disk index tail key {key} does not match encoded block {encoded}"
            ),
            Self::TailLimitExceeded { actual, limit } => write!(
                f,
                "disk index has {actual} tails, exceeding declared bound {limit}"
            ),
            Self::TailLimitMismatch { expected, actual } => write!(
                f,
                "disk index tail bound mismatch: expected {expected}, got {actual}"
            ),
            Self::TailCountMismatch { expected, actual } => write!(
                f,
                "disk index tail count mismatch: expected {expected}, got {actual}"
            ),
            Self::TailDigestMismatch => write!(f, "disk index tail digest mismatch"),
            Self::InvalidTail(reason) => write!(f, "invalid disk index tail: {reason}"),
            Self::IdentityMismatch => write!(f, "disk index does not match the primary file"),
            Self::PrimaryGenerationMismatch => write!(
                f,
                "disk index was published against a different generation of the primary file"
            ),
            Self::ModeMismatch { expected, actual } => write!(
                f,
                "disk index mode mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::PlanEmpty => write!(f, "disk index plan has no descriptors"),
            Self::PlanNotCanonical => {
                write!(f, "disk index plan descriptors are not strictly ordered")
            }
            Self::PlanCodecIdentityMissing { block_id } => write!(
                f,
                "disk index descriptor {block_id} has no key codec identity"
            ),
            Self::PlanBlockMissing { block_id } => write!(
                f,
                "disk index descriptor references undeclared block {block_id}"
            ),
            Self::PlanBlockVersionMismatch {
                block_id,
                expected,
                actual,
            } => write!(
                f,
                "disk index block {block_id} version mismatch: expected {expected}, got {actual}"
            ),
            Self::PlanDigestMismatch => write!(f, "disk index plan digest mismatch"),
            Self::PlanDigestIsZero => write!(f, "disk index plan digest may not be zero"),
            Self::CleanStateRequired => write!(f, "disk index clean state is required"),
            Self::DirtyStateRequired => write!(f, "disk index dirty state is required"),
            Self::UnexpectedSavepoints {
                expected,
                first_actual,
                actual_count,
            } => write!(
                f,
                "unexpected persistent savepoints: expected {expected:?}, first actual {first_actual:?}, count {actual_count}"
            ),
            Self::CheckpointMismatch(reason) => {
                write!(f, "disk index checkpoint mismatch: {reason}")
            }
            Self::CoverageMismatch { expected, actual } => write!(
                f,
                "disk index coverage mismatch: expected {expected}, got {actual}"
            ),
            Self::NativeTooShort { required, actual } => write!(
                f,
                "native file length {actual} is below required checkpoint EOF {required}"
            ),
            Self::NativeLengthMismatch { expected, actual } => write!(
                f,
                "native file length mismatch: expected {expected}, got {actual}"
            ),
            Self::SequenceMismatch { expected, actual } => write!(
                f,
                "disk index sequence mismatch: expected {expected:?}, got {actual}"
            ),
            Self::RecordCountExhausted => write!(f, "disk index record count is exhausted"),
            Self::GenerationExhausted => write!(f, "disk index generation is exhausted"),
            Self::RecordExtentMismatch {
                expected_end,
                actual_end,
            } => write!(
                f,
                "disk index record extent mismatch: expected end {expected_end}, got {actual_end}"
            ),
            Self::BatchFull {
                records,
                bytes,
                max_records,
                max_bytes,
            } => write!(
                f,
                "disk index batch would reach {records} records/{bytes} bytes, exceeding {max_records} records/{max_bytes} bytes"
            ),
            Self::KeyTableUnavailable => {
                write!(f, "disk key lookup is unavailable in state-only mode")
            }
            Self::Busy => write!(f, "disk index database is already open for writing"),
            Self::SavepointOperation { operation, error } => {
                write!(f, "disk index savepoint {operation} failed: {error}")
            }
            Self::Storage(error) => write!(f, "disk index storage error: {error}"),
            Self::Primary(error) => write!(f, "primary file error during index operation: {error}"),
            Self::Io(error) => write!(f, "disk index I/O error: {error}"),
        }
    }
}

impl std::error::Error for DiskIndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Primary(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DiskIndexError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<Error> for DiskIndexError {
    fn from(value: Error) -> Self {
        Self::Primary(value)
    }
}

fn storage(error: impl fmt::Display) -> DiskIndexError {
    DiskIndexError::Storage(error.to_string())
}

fn database(error: redb::DatabaseError) -> DiskIndexError {
    match error {
        redb::DatabaseError::DatabaseAlreadyOpen => DiskIndexError::Busy,
        error => storage(error),
    }
}

fn savepoint_error(operation: &'static str, error: impl fmt::Display) -> DiskIndexError {
    DiskIndexError::SavepointOperation {
        operation,
        error: error.to_string(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiskIndexDigest([u8; 32]);

impl DiskIndexDigest {
    pub const ZERO: Self = Self([0; 32]);

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub const fn is_zero(self) -> bool {
        let mut index = 0;
        while index < self.0.len() {
            if self.0[index] != 0 {
                return false;
            }
            index += 1;
        }
        true
    }
}

impl fmt::Debug for DiskIndexDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DiskIndexDigest(")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        f.write_str(")")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskIndexMode {
    StateOnly,
    DiskPlan(DiskIndexDigest),
}

impl DiskIndexMode {
    pub const fn plan_digest(self) -> Option<DiskIndexDigest> {
        match self {
            Self::StateOnly => None,
            Self::DiskPlan(digest) => Some(digest),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexIdentity {
    pub schema_hash: u64,
    pub primary_fingerprint: [u8; 32],
}

/// Bytes of the primary covered by the generation witness (STO-01).
///
/// The window is a small constant: the witness must catch a rewrite of the
/// primary's leading records, and verifying it costs one bounded read at open
/// regardless of file size. Growing it would not change the class of rewrite
/// it detects, only how far into the file the check reaches.
pub(crate) const PRIMARY_GENERATION_WINDOW: u64 = 4096;

/// Witness that the sidecar still belongs to the primary generation it was
/// published against.
///
/// Schema hash, OS object identity and the deterministic file header are all
/// stable across an in-place rewrite of a primary by an equal-length primary
/// of the same format, so they cannot distinguish two logical generations of
/// one file object. `digest` covers the first `len` bytes of the primary as of
/// the recorded commit, which no such rewrite preserves unless it reproduces
/// those bytes exactly.
///
/// One exclusion, and it is the only part of a primary that may change without
/// a new generation: a declared `IndexPolicy::header_tails` region's payload is
/// blanked before the digest is taken, because a commit rewrites it in place
/// and the witness would otherwise become a commit counter. The region's magic
/// and declared length stay covered, which is what still catches a region that
/// changed size — the change that moves every record offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexPrimaryGeneration {
    pub len: u64,
    pub digest: DiskIndexDigest,
}

impl DiskIndexPrimaryGeneration {
    pub const EMPTY: Self = Self {
        len: 0,
        digest: DiskIndexDigest([0u8; 32]),
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskIndexState {
    Dirty,
    Clean,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexFrontier {
    pub eof: u64,
    pub record_count: u64,
    pub next_sequence: Option<u64>,
}

impl DiskIndexFrontier {
    pub const fn new(eof: u64, record_count: u64, next_sequence: Option<u64>) -> Self {
        Self {
            eof,
            record_count,
            next_sequence,
        }
    }

    pub const fn empty(eof: u64) -> Self {
        Self::new(eof, 0, Some(0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexCheckpoint {
    pub savepoint_id: u64,
    pub base: DiskIndexFrontier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexMetadata {
    pub identity: DiskIndexIdentity,
    pub primary_generation: DiskIndexPrimaryGeneration,
    pub mode: DiskIndexMode,
    pub state: DiskIndexState,
    pub generation: u64,
    pub committed: DiskIndexFrontier,
    pub working: DiskIndexFrontier,
    pub checkpoint: Option<DiskIndexCheckpoint>,
    pub tail_limit: u32,
    pub committed_tail_count: u32,
    pub working_tail_count: u32,
    pub committed_tail_digest: DiskIndexDigest,
    pub working_tail_digest: DiskIndexDigest,
}

impl DiskIndexMetadata {
    pub fn new_state(
        identity: DiskIndexIdentity,
        frontier: DiskIndexFrontier,
        tail_limit: u32,
    ) -> Self {
        Self::new_with_mode(identity, DiskIndexMode::StateOnly, frontier, tail_limit)
    }

    pub fn new_disk(
        identity: DiskIndexIdentity,
        plan_digest: DiskIndexDigest,
        frontier: DiskIndexFrontier,
        tail_limit: u32,
    ) -> Self {
        Self::new_with_mode(
            identity,
            DiskIndexMode::DiskPlan(plan_digest),
            frontier,
            tail_limit,
        )
    }

    fn new_with_mode(
        identity: DiskIndexIdentity,
        mode: DiskIndexMode,
        frontier: DiskIndexFrontier,
        tail_limit: u32,
    ) -> Self {
        let empty_digest = tail_digest(std::iter::empty());
        Self {
            identity,
            primary_generation: DiskIndexPrimaryGeneration::EMPTY,
            mode,
            state: DiskIndexState::Clean,
            generation: 0,
            committed: frontier,
            working: frontier,
            checkpoint: None,
            tail_limit,
            committed_tail_count: 0,
            working_tail_count: 0,
            committed_tail_digest: empty_digest,
            working_tail_digest: empty_digest,
        }
    }

    #[must_use]
    pub fn with_primary_generation(mut self, generation: DiskIndexPrimaryGeneration) -> Self {
        self.primary_generation = generation;
        self
    }
}

type ExtractPutUpdate =
    fn(FormatSpec, &SnapshotFile, &RecordIndexEntry, usize) -> crate::Result<DiskIndexUpdate>;

type ExtractTombstoneUpdate = fn(
    FormatSpec,
    &RecordIndexEntry,
    &[u8],
    &mut MaterializationBudget,
    usize,
) -> crate::Result<DiskIndexUpdate>;

type RegisterDescriptorBlock = fn(FormatSpec) -> crate::Result<()>;

type BlockIdentityTable = &'static [(u32, Option<crate::Endian>, bool, u64)];

#[derive(Clone, Copy)]
pub struct DiskIndexDescriptor {
    pub block_id: u32,
    pub block_version: u16,
    pub key_wire_type: u16,
    /// API-03: the block schema fingerprint of the concrete `T` whose decode
    /// and key-extraction function pointers this descriptor captured. Without
    /// it a descriptor built for one block type validated — and then decoded
    /// primary bytes — against a format that declares a different schema for
    /// the same block id.
    pub schema_fingerprint: u64,
    key_codec_identity: fn() -> String,
    register: RegisterDescriptorBlock,
    extract_put: ExtractPutUpdate,
    extract_tombstone: ExtractTombstoneUpdate,
}

impl DiskIndexDescriptor {
    pub const fn of<T>() -> Self
    where
        T: VarveKeyedBlock,
        T::Key: VarveDiskKey,
    {
        // API2-03: compile-time keyedness contract at the descriptor root,
        // covering the whole disk-index plan path for `T`.
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        Self {
            block_id: T::ID,
            block_version: T::VERSION,
            key_wire_type: <T::Key as VarveEncode>::WIRE_TYPE as u16,
            schema_fingerprint: T::SCHEMA_FINGERPRINT,
            key_codec_identity: disk_key_codec_identity::<T::Key>,
            register: register_descriptor_block::<T>,
            extract_put: extract_descriptor_put::<T>,
            extract_tombstone: extract_descriptor_tombstone::<T>,
        }
    }

    pub fn key_codec_identity(self) -> String {
        (self.key_codec_identity)()
    }

    /// Runs the format's authoritative block-identity gate for the concrete
    /// type this descriptor captured. Plan construction calls it once per
    /// descriptor, before any captured codec can reach file bytes; the gate is
    /// itself cached per (format, block), so it is allocation- and syscall-free
    /// after the first call.
    fn check_registered(self, spec: FormatSpec) -> crate::Result<()> {
        (self.register)(spec)
    }
}

fn register_descriptor_block<T>(spec: FormatSpec) -> crate::Result<()>
where
    T: VarveKeyedBlock,
    T::Key: VarveDiskKey,
{
    crate::collections::ensure_registered_block::<T>(spec)
}

impl fmt::Debug for DiskIndexDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key_codec_identity = self.key_codec_identity();
        f.debug_struct("DiskIndexDescriptor")
            .field("block_id", &self.block_id)
            .field("block_version", &self.block_version)
            .field("key_wire_type", &self.key_wire_type)
            .field("schema_fingerprint", &self.schema_fingerprint)
            .field("key_codec_identity", &key_codec_identity)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DiskIndexPlan {
    descriptors: &'static [DiskIndexDescriptor],
    digest: DiskIndexDigest,
    /// Witness that this exact plan value already passed [`Self::validate`]
    /// against the referenced format tables. Block existence, version and
    /// block-identity checks depend only on `FormatSpec::blocks` and
    /// `FormatSpec::block_identities`, so matching slices (compared as fat
    /// pointers) prove the earlier validation still holds and open paths do
    /// not repeat the per-descriptor walk, the registration gate, or the
    /// digest recomputation that canonicalization already performed. Both
    /// tables are part of the witness: two specs that share a descriptor
    /// table but declare different identities must not share it.
    validated_blocks: Option<(&'static [crate::BlockDescriptor], BlockIdentityTable)>,
}

impl DiskIndexPlan {
    pub fn canonical(
        spec: FormatSpec,
        descriptors: &'static [DiskIndexDescriptor],
    ) -> DiskIndexResult<Self> {
        validate_descriptors(spec, descriptors)?;
        let digest = compute_plan_digest(descriptors)?;
        Ok(Self {
            descriptors,
            digest,
            validated_blocks: Some((spec.blocks, spec.block_identities)),
        })
    }

    pub const fn from_generated(
        descriptors: &'static [DiskIndexDescriptor],
        digest: DiskIndexDigest,
    ) -> Self {
        Self {
            descriptors,
            digest,
            validated_blocks: None,
        }
    }

    pub fn validate(self, spec: FormatSpec) -> DiskIndexResult<Self> {
        if self.digest.is_zero() {
            return Err(DiskIndexError::PlanDigestIsZero);
        }
        if self.validated_blocks.is_some_and(|(blocks, identities)| {
            std::ptr::eq(blocks, spec.blocks) && std::ptr::eq(identities, spec.block_identities)
        }) {
            return Ok(self);
        }
        validate_descriptors(spec, self.descriptors)?;
        if compute_plan_digest(self.descriptors)? != self.digest {
            return Err(DiskIndexError::PlanDigestMismatch);
        }
        Ok(Self {
            validated_blocks: Some((spec.blocks, spec.block_identities)),
            ..self
        })
    }

    pub const fn descriptors(self) -> &'static [DiskIndexDescriptor] {
        self.descriptors
    }

    pub const fn digest(self) -> DiskIndexDigest {
        self.digest
    }

    pub const fn mode(self) -> DiskIndexMode {
        DiskIndexMode::DiskPlan(self.digest)
    }

    pub fn descriptor(self, block_id: u32) -> Option<DiskIndexDescriptor> {
        self.descriptors
            .binary_search_by_key(&block_id, |descriptor| descriptor.block_id)
            .ok()
            .map(|index| self.descriptors[index])
    }
}

fn validate_descriptors(
    spec: FormatSpec,
    descriptors: &[DiskIndexDescriptor],
) -> DiskIndexResult<()> {
    if descriptors.is_empty() {
        return Err(DiskIndexError::PlanEmpty);
    }
    // Format blocks are not required to be sorted, so index them once by id
    // and resolve each descriptor with a binary search: O((D + B) log B)
    // instead of the previous per-descriptor linear scan (O(D * B)).
    let mut blocks: Vec<(u32, u16)> = spec
        .blocks
        .iter()
        .map(|block| (block.id, block.version))
        .collect();
    blocks.sort_unstable_by_key(|(id, _)| *id);
    let mut previous = None;
    for descriptor in descriptors {
        if previous.is_some_and(|block_id| block_id >= descriptor.block_id) {
            return Err(DiskIndexError::PlanNotCanonical);
        }
        previous = Some(descriptor.block_id);
        if descriptor.key_codec_identity().is_empty() {
            return Err(DiskIndexError::PlanCodecIdentityMissing {
                block_id: descriptor.block_id,
            });
        }
        let block = blocks
            .binary_search_by_key(&descriptor.block_id, |(id, _)| *id)
            .map(|index| blocks[index])
            .map_err(|_| DiskIndexError::PlanBlockMissing {
                block_id: descriptor.block_id,
            })?;
        if block.1 != descriptor.block_version {
            return Err(DiskIndexError::PlanBlockVersionMismatch {
                block_id: descriptor.block_id,
                expected: descriptor.block_version,
                actual: block.1,
            });
        }
        // API-03: the format's immutable block identity — keyedness and schema
        // fingerprint included — is the authority for the concrete type whose
        // decode and key-extraction pointers this descriptor captured. It runs
        // here, at plan construction, so no captured codec can reach primary
        // bytes through a plan the format never declared.
        descriptor
            .check_registered(spec)
            .map_err(DiskIndexError::Primary)?;
    }
    Ok(())
}

fn compute_plan_digest(descriptors: &[DiskIndexDescriptor]) -> DiskIndexResult<DiskIndexDigest> {
    let descriptor_count =
        u32::try_from(descriptors.len()).map_err(|_| DiskIndexError::PlanNotCanonical)?;
    // Materialize each codec identity once instead of re-allocating the
    // string for every digest lane.
    //
    // Uncharged on purpose, and it is the exception invariant 1 names rather
    // than an oversight: `descriptors` is the declared disk plan, so its length
    // is the format's own block count, fixed by the program at compile time. No
    // file content and no caller-supplied count reaches it. Anything sized from
    // a *decoded* count belongs on the charged path instead.
    let mut codecs = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        let codec = descriptor.key_codec_identity();
        u32::try_from(codec.len()).map_err(|_| DiskIndexError::PlanCodecIdentityMissing {
            block_id: descriptor.block_id,
        })?;
        codecs.push(codec);
    }
    let mut digest = [0u8; 32];
    for lane in 0..8u32 {
        let mut hasher = crc32fast::Hasher::new();
        // v2 folds the block schema fingerprint into the plan digest (API-03),
        // so a sidecar published for one block schema is refused as stale for
        // a plan whose descriptors decode a different one.
        hasher.update(b"varve-disk-index-plan-v2");
        hasher.update(&lane.to_le_bytes());
        hasher.update(&descriptor_count.to_le_bytes());
        for (descriptor, codec) in descriptors.iter().zip(&codecs) {
            let codec = codec.as_bytes();
            let codec_len = codec.len() as u32;
            hasher.update(&descriptor.block_id.to_le_bytes());
            hasher.update(&descriptor.block_version.to_le_bytes());
            hasher.update(&descriptor.key_wire_type.to_le_bytes());
            hasher.update(&descriptor.schema_fingerprint.to_le_bytes());
            hasher.update(&codec_len.to_le_bytes());
            hasher.update(codec);
        }
        digest[(lane as usize) * 4..(lane as usize + 1) * 4]
            .copy_from_slice(&hasher.finalize().to_le_bytes());
    }
    let digest = DiskIndexDigest(digest);
    if digest.is_zero() {
        return Err(DiskIndexError::PlanDigestIsZero);
    }
    Ok(digest)
}

pub fn tail_limit_for_spec(spec: FormatSpec) -> DiskIndexResult<u32> {
    if !spec.index_policy.block_offset_chain {
        return Ok(0);
    }
    let count = spec
        .blocks
        .len()
        .checked_add(2)
        .ok_or(DiskIndexError::MetadataInvariant(
            "tail declaration count overflow",
        ))?;
    u32::try_from(count)
        .map_err(|_| DiskIndexError::MetadataInvariant("tail declaration count exceeds u32"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskIndexEntry {
    Missing,
    Put { record_offset: u64, sequence: u64 },
    Tombstone { record_offset: u64, sequence: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexPhysicalRecord {
    pub physical_len: u64,
    pub block_id: u32,
    pub block_version: u16,
    pub flags: u16,
    pub native_crc: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexRecordPointer {
    pub entry: DiskIndexEntry,
    pub target_block_id: u32,
    pub physical: Option<DiskIndexPhysicalRecord>,
}

/// Marker for keys whose Varve encoding is deterministic and equal for `Eq` values.
///
/// Implementations must preserve that contract across all values and versions. The
/// sidecar always encodes these values little-endian, independently of file endian.
pub trait VarveDiskKey:
    VarveEncode + VarveDecode + Eq + Hash + Clone + Send + Sync + 'static
{
    fn disk_codec_identity() -> String;
}

macro_rules! disk_scalar_keys {
    ($($ty:ty => $id:literal),+ $(,)?) => {
        $(
            impl VarveDiskKey for $ty {
                fn disk_codec_identity() -> String {
                    $id.into()
                }
            }
        )+
    };
}

disk_scalar_keys!(
    () => "varve-key/unit/v1",
    bool => "varve-key/bool/v1",
    u8 => "varve-key/u8/v1",
    i8 => "varve-key/i8/v1",
    u16 => "varve-key/u16/v1",
    i16 => "varve-key/i16/v1",
    u32 => "varve-key/u32/v1",
    i32 => "varve-key/i32/v1",
    u64 => "varve-key/u64/v1",
    i64 => "varve-key/i64/v1",
    u128 => "varve-key/u128/v1",
    i128 => "varve-key/i128/v1",
    String => "varve-key/string/v1"
);

impl<A: VarveDiskKey, B: VarveDiskKey> VarveDiskKey for (A, B) {
    fn disk_codec_identity() -> String {
        format!(
            "varve-key/tuple2/v1({},{})",
            A::disk_codec_identity(),
            B::disk_codec_identity()
        )
    }
}

impl<A: VarveDiskKey, B: VarveDiskKey, C: VarveDiskKey> VarveDiskKey for (A, B, C) {
    fn disk_codec_identity() -> String {
        format!(
            "varve-key/tuple3/v1({},{},{})",
            A::disk_codec_identity(),
            B::disk_codec_identity(),
            C::disk_codec_identity()
        )
    }
}

impl<A: VarveDiskKey, B: VarveDiskKey, C: VarveDiskKey, D: VarveDiskKey> VarveDiskKey
    for (A, B, C, D)
{
    fn disk_codec_identity() -> String {
        format!(
            "varve-key/tuple4/v1({},{},{},{})",
            A::disk_codec_identity(),
            B::disk_codec_identity(),
            C::disk_codec_identity(),
            D::disk_codec_identity()
        )
    }
}

fn disk_key_codec_identity<K: VarveDiskKey>() -> String {
    K::disk_codec_identity()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskIndexUpdate {
    pub block_id: u32,
    pub canonical_key: Vec<u8>,
    pub entry: DiskIndexEntry,
    pub physical: Option<DiskIndexPhysicalRecord>,
}

impl DiskIndexUpdate {
    pub(crate) fn encode_key<K: VarveDiskKey>(
        key: &K,
        max_key_bytes: usize,
    ) -> DiskIndexResult<Vec<u8>> {
        encode_disk_key(key, max_key_bytes)
    }

    pub(crate) fn from_canonical_with_record(
        block_id: u32,
        canonical_key: Vec<u8>,
        entry: DiskIndexEntry,
        physical: DiskIndexPhysicalRecord,
        max_key_bytes: usize,
    ) -> DiskIndexResult<Self> {
        ensure_key_limit(canonical_key.len(), max_key_bytes)?;
        Ok(Self {
            block_id,
            canonical_key,
            entry,
            physical: Some(physical),
        })
    }

    pub fn from_key_with_record<K: VarveDiskKey>(
        block_id: u32,
        key: &K,
        entry: DiskIndexEntry,
        physical: DiskIndexPhysicalRecord,
        max_key_bytes: usize,
    ) -> DiskIndexResult<Self> {
        Self::from_canonical_with_record(
            block_id,
            Self::encode_key(key, max_key_bytes)?,
            entry,
            physical,
            max_key_bytes,
        )
    }
}

fn encode_disk_key<K: VarveDiskKey>(key: &K, max_key_bytes: usize) -> DiskIndexResult<Vec<u8>> {
    #[cfg(test)]
    DISK_KEY_ENCODE_CALLS.with(|calls| calls.set(calls.get() + 1));
    Ok(encode_to_vec_limited(
        key,
        Endian::Little,
        u64::try_from(max_key_bytes).unwrap_or(u64::MAX),
        "disk index key",
    )?)
}

#[cfg(test)]
std::thread_local! {
    static DISK_KEY_ENCODE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_disk_key_encode_calls() {
    DISK_KEY_ENCODE_CALLS.set(0);
}

#[cfg(test)]
pub(crate) fn disk_key_encode_calls() -> usize {
    DISK_KEY_ENCODE_CALLS.get()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexTail {
    pub block_id: u32,
    pub record_offset: u64,
    pub sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskIndexPersistentState {
    pub metadata: DiskIndexMetadata,
    pub tails: Vec<DiskIndexTail>,
}

/// Mirror of the private `file::RECORD_FLAG_INTERNAL` flag. Tombstone records
/// are always written with block version 1 and exactly this flag set; the
/// mirror is pinned against the real writer by
/// `indexed::tests::tombstone_records_use_the_mirrored_internal_flag`.
pub(crate) const TOMBSTONE_RECORD_FLAGS: u16 = 0x8000;

#[cfg(test)]
std::thread_local! {
    static TOMBSTONE_KEY_DECODE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PLAN_PAYLOAD_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_tombstone_key_decode_calls() {
    TOMBSTONE_KEY_DECODE_CALLS.set(0);
}

#[cfg(test)]
pub(crate) fn tombstone_key_decode_calls() -> usize {
    TOMBSTONE_KEY_DECODE_CALLS.get()
}

/// Test hook: number of native record payloads materialized by plan
/// extraction. Rebuild must read each indexed payload exactly once. The
/// asserting test requires a CRC policy, so the accessors are unused in
/// integrity-less builds.
#[cfg(test)]
#[cfg_attr(not(feature = "integrity"), allow(dead_code))]
pub(crate) fn reset_plan_payload_reads() {
    PLAN_PAYLOAD_READS.set(0);
}

#[cfg(test)]
#[cfg_attr(not(feature = "integrity"), allow(dead_code))]
pub(crate) fn plan_payload_reads() -> usize {
    PLAN_PAYLOAD_READS.get()
}

/// Extracts the sidecar update for one scanned native record.
///
/// Resolves the record's descriptor once through the plan's binary search and
/// decodes each tombstone key exactly once, instead of iterating every
/// descriptor and re-decoding the tombstone payload per descriptor.
pub(crate) fn extract_plan_update(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    plan: DiskIndexPlan,
    entry: &RecordIndexEntry,
    max_key_bytes: usize,
) -> crate::Result<Option<DiskIndexUpdate>> {
    if entry.block_id == TOMBSTONE_BLOCK_ID {
        // Same shape gate as `file::decode_stream_tombstone_key`: reject any
        // tombstone that does not carry version 1 and exactly the internal flag.
        if entry.block_version != 1 || entry.flags != TOMBSTONE_RECORD_FLAGS {
            return Err(Error::CorruptTail {
                offset: entry.record_offset,
            });
        }
        let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
        let mut budget = MaterializationBudget::new(spec);
        budget.consume(logical_len)?;
        #[cfg(test)]
        PLAN_PAYLOAD_READS.with(|reads| reads.set(reads.get() + 1));
        let payload = entry.read_logical_payload_snapshot(spec, snapshot)?;
        // Envelope prefix: target block id (u32 LE) followed by the key length.
        let target = payload
            .get(0..4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("fixed slice")))
            .ok_or(Error::UnexpectedEof)?;
        // Validates the full envelope shape (length accounting and trailing
        // bytes) even when the target block is not part of this plan.
        let Some(key_payload) = decode_native_internal_key_envelope(&payload, target)? else {
            return Ok(None);
        };
        let Some(descriptor) = plan.descriptor(target) else {
            return Ok(None);
        };
        Ok(Some((descriptor.extract_tombstone)(
            spec,
            entry,
            key_payload,
            &mut budget,
            max_key_bytes,
        )?))
    } else {
        let Some(descriptor) = plan.descriptor(entry.block_id) else {
            return Ok(None);
        };
        Ok(Some((descriptor.extract_put)(
            spec,
            snapshot,
            entry,
            max_key_bytes,
        )?))
    }
}

fn extract_descriptor_put<T>(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    entry: &RecordIndexEntry,
    max_key_bytes: usize,
) -> crate::Result<DiskIndexUpdate>
where
    T: VarveKeyedBlock,
    T::Key: VarveDiskKey,
{
    debug_assert_eq!(entry.block_id, T::ID);
    descriptor_decoder_call();
    if entry.block_version != T::VERSION {
        return Err(Error::BlockVersionMismatch {
            block_id: T::ID,
            expected: T::VERSION,
            actual: entry.block_version,
        });
    }
    let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
    let mut budget = MaterializationBudget::new(spec);
    budget.consume(logical_len)?;
    #[cfg(test)]
    PLAN_PAYLOAD_READS.with(|reads| reads.set(reads.get() + 1));
    let payload = entry.read_logical_payload_snapshot(spec, snapshot)?;
    let value: T = budget.decode(&payload, T::ENDIAN.unwrap_or(spec.endian))?;
    let indexed = DiskIndexEntry::Put {
        record_offset: entry.record_offset,
        sequence: entry.sequence,
    };
    let physical = record_physical_descriptor(spec, entry)?;
    DiskIndexUpdate::from_key_with_record(T::ID, &value.key(), indexed, physical, max_key_bytes)
        .map_err(|error| Error::DiskIndex(Box::new(error)))
}

fn extract_descriptor_tombstone<T>(
    spec: FormatSpec,
    entry: &RecordIndexEntry,
    key_payload: &[u8],
    budget: &mut MaterializationBudget,
    max_key_bytes: usize,
) -> crate::Result<DiskIndexUpdate>
where
    T: VarveKeyedBlock,
    T::Key: VarveDiskKey,
{
    #[cfg(test)]
    TOMBSTONE_KEY_DECODE_CALLS.with(|calls| calls.set(calls.get() + 1));
    descriptor_decoder_call();
    // Tombstone keys are decoded with the file endianness, matching
    // `file::decode_stream_tombstone_key`.
    let key: T::Key = budget.decode(key_payload, spec.endian)?;
    let indexed = DiskIndexEntry::Tombstone {
        record_offset: entry.record_offset,
        sequence: entry.sequence,
    };
    let physical = record_physical_descriptor(spec, entry)?;
    DiskIndexUpdate::from_key_with_record(T::ID, &key, indexed, physical, max_key_bytes)
        .map_err(|error| Error::DiskIndex(Box::new(error)))
}

fn record_physical_descriptor(
    spec: FormatSpec,
    entry: &RecordIndexEntry,
) -> crate::Result<DiskIndexPhysicalRecord> {
    let physical_end = entry.checked_physical_end()?;
    let physical_len = physical_end
        .checked_sub(entry.record_offset)
        .ok_or(Error::CorruptTail {
            offset: entry.record_offset,
        })?;
    Ok(DiskIndexPhysicalRecord {
        physical_len,
        block_id: entry.block_id,
        block_version: entry.block_version,
        flags: entry.flags,
        native_crc: (spec.integrity_policy != IntegrityPolicy::None).then_some(entry.checksum),
    })
}

/// Process-local shared-database coordinator.
///
/// redb allows one open [`Database`] per file, so independent in-process
/// handles previously failed with [`DiskIndexError::Busy`] even when both only
/// needed read snapshots. The registry shares one `Database` (plus one
/// write-admission gate) per native file identity — Windows volume serial and
/// file index, Unix device and inode — so any number of read snapshots can
/// coexist, and a reader can open beside a synced writer. Write transactions
/// are admitted one at a time through the gate and fail fast with
/// [`DiskIndexError::Busy`] instead of blocking behind another handle's
/// long-lived batch or restore transaction.
///
/// Cross-process exclusivity is unchanged: a second process still receives
/// [`DiskIndexError::Busy`] from redb's own file lock.
///
/// Sidecar rebuild/replacement must call [`invalidate_shared_database`] on the
/// destination path before publishing the replacement, so a database backed by
/// the replaced file object can never be upgraded for the new file — even if
/// the operating system later reuses the old native file identity.
struct SharedSidecar {
    database: Weak<Database>,
    write_gate: Arc<AtomicUsize>,
}

impl SharedSidecar {
    fn empty() -> Self {
        Self {
            database: Weak::new(),
            write_gate: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// Per-identity initialization cell. The process-global registry mutex only
/// guards the identity -> slot map; the (possibly slow) redb open of a cache
/// miss runs under the slot's own mutex, so unrelated sidecar opens never
/// serialize behind each other while racing opens of one identity still
/// cannot both reach redb.
type SharedSidecarSlot = Arc<Mutex<SharedSidecar>>;

/// Smallest map size that is ever swept. Below it a sweep is cheaper than the
/// bookkeeping that would avoid it.
const MIN_REGISTRY_PRUNE_SLOTS: usize = 16;

/// Process-global identity -> slot map with amortized reclamation (PERF-04).
///
/// The map is only swept once it has grown past `prune_at`, and the next
/// threshold is set to twice the surviving size. Each sweep is `Theta(S)` but
/// is preceded by at least `S/2` insertions, so the global mutex is held for
/// `O(1)` on the common path and `S` sequential opens cost `O(S)` slot checks
/// in total instead of the previous `Theta(S^2)`.
struct SharedSidecarRegistry {
    slots: HashMap<Vec<u8>, SharedSidecarSlot>,
    prune_at: usize,
}

impl SharedSidecarRegistry {
    fn new() -> Self {
        Self {
            slots: HashMap::new(),
            prune_at: MIN_REGISTRY_PRUNE_SLOTS,
        }
    }

    /// Drops slots whose database is gone and that no in-flight open still
    /// holds. A slot mutex is only probed with `try_lock`: a held lock means an
    /// open is in progress, so the slot is live and must be kept — and the
    /// global lock is therefore never blocked on a slow slot initialization.
    fn prune(&mut self) {
        registry_slots_inspected(self.slots.len() as u64);
        self.slots.retain(|_, slot| {
            if Arc::strong_count(slot) > 1 {
                return true;
            }
            match slot.try_lock() {
                Ok(shared) => shared.database.strong_count() != 0,
                Err(std::sync::TryLockError::WouldBlock) => true,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    poisoned.into_inner().database.strong_count() != 0
                }
            }
        });
        self.prune_at = self
            .slots
            .len()
            .saturating_mul(2)
            .max(MIN_REGISTRY_PRUNE_SLOTS);
    }

    fn prune_if_grown(&mut self) {
        if self.slots.len() > self.prune_at {
            self.prune();
        }
    }
}

fn shared_sidecar_registry() -> &'static Mutex<SharedSidecarRegistry> {
    static REGISTRY: OnceLock<Mutex<SharedSidecarRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(SharedSidecarRegistry::new()))
}

fn lock_shared_registry() -> std::sync::MutexGuard<'static, SharedSidecarRegistry> {
    shared_sidecar_registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Removes the process-local shared-database entry for the sidecar currently
/// named by `path`. Best-effort: a missing or unreadable path can only leave
/// entries whose `Weak` is dead or whose identity no longer resolves from any
/// pathname; growth past the prune threshold reclaims those.
pub(crate) fn invalidate_shared_database(path: &Path) {
    let identity = sidecar_path_identity(path).ok();
    let mut registry = lock_shared_registry();
    if let Some(identity) = identity {
        registry_slots_inspected(1);
        registry.slots.remove(&identity);
    }
    registry.prune_if_grown();
}

fn shared_sidecar_slot(identity: &[u8]) -> SharedSidecarSlot {
    let mut registry = lock_shared_registry();
    registry_slots_inspected(1);
    if let Some(slot) = registry.slots.get(identity) {
        return Arc::clone(slot);
    }
    let slot = Arc::new(Mutex::new(SharedSidecar::empty()));
    registry.slots.insert(identity.to_vec(), Arc::clone(&slot));
    registry.prune_if_grown();
    slot
}

fn open_shared_database(
    path: &Path,
    options: DiskIndexOptions,
    create: bool,
) -> DiskIndexResult<(Arc<Database>, Arc<AtomicUsize>)> {
    if create {
        // Creation targets a fresh temporary path; a live database for the
        // same identity would already have failed redb's own open check.
        let database = Arc::new(open_database(path, options, true)?);
        let identity = sidecar_path_identity(path)?;
        let write_gate = Arc::new(AtomicUsize::new(0));
        let slot = shared_sidecar_slot(&identity);
        let mut shared = slot.lock().unwrap_or_else(PoisonError::into_inner);
        *shared = SharedSidecar {
            database: Arc::downgrade(&database),
            write_gate: Arc::clone(&write_gate),
        };
        drop(shared);
        return Ok((database, write_gate));
    }
    let identity = sidecar_path_identity(path)?;
    // The global registry lock is only held for the map access inside
    // `shared_sidecar_slot`; the slot mutex serializes the miss-open-insert
    // sequence per identity, so two racing opens of the same file cannot both
    // reach redb while opens of unrelated sidecars proceed concurrently.
    let slot = shared_sidecar_slot(&identity);
    let mut shared = slot.lock().unwrap_or_else(PoisonError::into_inner);
    // A hit returns without re-probing the pathname, and that is sound for a
    // reason worth stating, because the miss path below *does* re-probe.
    //
    // Identity bytes read from a handle that is then closed name nothing on
    // their own: a filesystem hands a just-freed inode straight back to the
    // next create, so equal bytes can mean a different object. What rules that
    // out here is the upgrade itself. A live `Database` holds the sidecar open,
    // an open handle pins its inode, and a pinned inode cannot be handed to
    // anything else -- so a successful upgrade is the proof a re-probe would be
    // looking for. The miss path has no such handle, which is why it pays for
    // one.
    if let Some(database) = shared.database.upgrade() {
        return Ok((database, Arc::clone(&shared.write_gate)));
    }
    let database = Arc::new(open_database(path, options, false)?);
    // The pathname may have been atomically replaced between the identity
    // probe and the database open; report the transient conflict as busy
    // rather than registering a database under the wrong identity.
    if sidecar_path_identity(path)? != identity {
        return Err(DiskIndexError::Busy);
    }
    let write_gate = Arc::new(AtomicUsize::new(0));
    *shared = SharedSidecar {
        database: Arc::downgrade(&database),
        write_gate: Arc::clone(&write_gate),
    };
    Ok((database, write_gate))
}

fn sidecar_path_identity(path: &Path) -> DiskIndexResult<Vec<u8>> {
    opened_sidecar_identity(&fs::File::open(path)?)
}

// Mirrors the private `stream::opened_file_identity` helper; the sidecar
// registry must key on the same native identity the sidecar fingerprint uses.
#[cfg(unix)]
fn opened_sidecar_identity(file: &fs::File) -> DiskIndexResult<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&metadata.dev().to_le_bytes());
    bytes.extend_from_slice(&metadata.ino().to_le_bytes());
    Ok(bytes)
}

#[cfg(windows)]
fn opened_sidecar_identity(file: &fs::File) -> DiskIndexResult<Vec<u8>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // The handle belongs to `file`, the output points to initialized writable
    // storage, and the OS call does not outlive either value.
    let ok =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, information.as_mut_ptr()) };
    if ok == 0 {
        return Err(DiskIndexError::Io(std::io::Error::last_os_error()));
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

/// Exclusive write admission for one shared sidecar database. Every write
/// transaction holds the gate for its lifetime, so a conflicting handle
/// observes a typed [`DiskIndexError::Busy`] immediately instead of blocking
/// inside redb behind an unbounded batch or restore transaction.
struct WriteGateGuard {
    gate: Arc<AtomicUsize>,
}

impl WriteGateGuard {
    fn acquire(gate: &Arc<AtomicUsize>) -> DiskIndexResult<Self> {
        if gate
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(DiskIndexError::Busy);
        }
        Ok(Self {
            gate: Arc::clone(gate),
        })
    }
}

impl Drop for WriteGateGuard {
    fn drop(&mut self) {
        self.gate.store(0, Ordering::Release);
    }
}

/// Last committed tail collection of this handle, keyed by the metadata tail
/// digest it was committed under. Lets the next write batch skip re-reading
/// and re-validating every tail row when nothing else committed in between:
/// the digest comparison against the freshly read metadata proves the cached
/// map is exactly the persisted collection.
struct TailCache {
    digest: DiskIndexDigest,
    tails: BTreeMap<u32, DiskIndexTail>,
}

fn store_tail_cache(
    cache: &Mutex<Option<TailCache>>,
    digest: DiskIndexDigest,
    tails: BTreeMap<u32, DiskIndexTail>,
) {
    *cache.lock().unwrap_or_else(PoisonError::into_inner) = Some(TailCache { digest, tails });
}

/// Takes the cached tail map when it provably matches the current working
/// root. Ownership moves into the batch; the cache is refilled on commit, so
/// an aborted or failed batch can never leak staged tails back into it.
fn take_tail_cache(
    cache: &Mutex<Option<TailCache>>,
    metadata: DiskIndexMetadata,
) -> Option<BTreeMap<u32, DiskIndexTail>> {
    let mut slot = cache.lock().unwrap_or_else(PoisonError::into_inner);
    let cached = slot.take()?;
    if cached.digest == metadata.working_tail_digest
        && u32::try_from(cached.tails.len()).ok() == Some(metadata.working_tail_count)
    {
        Some(cached.tails)
    } else {
        None
    }
}

pub struct DiskIndexStore {
    database: Arc<Database>,
    options: DiskIndexOptions,
    write_gate: Arc<AtomicUsize>,
    tail_cache: Arc<Mutex<Option<TailCache>>>,
}

impl DiskIndexStore {
    pub fn create(
        path: impl AsRef<Path>,
        options: DiskIndexOptions,
        metadata: DiskIndexMetadata,
    ) -> DiskIndexResult<Self> {
        Self::create_with_tails(path, options, metadata, &[])
    }

    pub fn create_with_tails(
        path: impl AsRef<Path>,
        options: DiskIndexOptions,
        metadata: DiskIndexMetadata,
        tails: &[DiskIndexTail],
    ) -> DiskIndexResult<Self> {
        let options = options.validate()?;
        let path = canonical_sidecar_path(path.as_ref())?;
        let (database, write_gate) = open_shared_database(&path, options, true)?;
        let store = Self {
            database,
            options,
            write_gate,
            tail_cache: Arc::new(Mutex::new(None)),
        };
        store.initialize(metadata, tails)?;
        Ok(store)
    }

    /// Opens the sidecar database.
    ///
    /// Handles within one process share a single underlying [`Database`] per
    /// native file identity, so independent readers — or a reader beside a
    /// synced writer — open concurrently. A handle that currently holds a
    /// write transaction (an uncommitted batch, a staged restore, or a
    /// short-lived validation) makes conflicting write attempts fail with
    /// [`DiskIndexError::Busy`]. Cross-process exclusivity is unchanged:
    /// redb's file lock rejects a second process with the same typed error.
    /// The shared database keeps the cache configuration of the handle that
    /// first opened it.
    pub fn open(path: impl AsRef<Path>, options: DiskIndexOptions) -> DiskIndexResult<Self> {
        let options = options.validate()?;
        let path = canonical_sidecar_path(path.as_ref())?;
        let (database, write_gate) = open_shared_database(&path, options, false)?;
        let store = Self {
            database,
            options,
            write_gate,
            tail_cache: Arc::new(Mutex::new(None)),
        };
        store.validate_envelope()?;
        Ok(store)
    }

    pub fn open_validated(
        path: impl AsRef<Path>,
        options: DiskIndexOptions,
        identity: DiskIndexIdentity,
        mode: DiskIndexMode,
    ) -> DiskIndexResult<Self> {
        let store = Self::open(path, options)?;
        let metadata = store.read_metadata()?;
        validate_identity_mode(metadata, identity, mode)?;
        Ok(store)
    }

    pub fn read_metadata(&self) -> DiskIndexResult<DiskIndexMetadata> {
        let transaction = self.database.begin_read().map_err(storage)?;
        read_metadata(&transaction)
    }

    pub(crate) const fn max_key_bytes(&self) -> usize {
        self.options.max_key_bytes
    }

    pub fn read_state(&self) -> DiskIndexResult<DiskIndexPersistentState> {
        let transaction = self.database.begin_read().map_err(storage)?;
        read_state(&transaction)
    }

    /// Returns the historical distinct key cardinality of the latest-key
    /// table: the number of distinct `(block, key)` pairs ever indexed,
    /// including keys whose latest entry is a tombstone.
    ///
    /// Sidecar capacity is proportional to this metric — historical distinct
    /// keys (`K`-ever) — not to the number of live keys: tombstones replace
    /// latest values but never delete rows, and rebuild re-creates tombstone
    /// rows from the native log. The metric only shrinks when the native file
    /// is compacted and the sidecar is rebuilt together. Reads the last
    /// committed root; an open uncommitted batch is not visible.
    pub fn historical_distinct_keys(&self) -> DiskIndexResult<u64> {
        let transaction = self.database.begin_read().map_err(storage)?;
        let metadata = read_metadata(&transaction)?;
        if !matches!(metadata.mode, DiskIndexMode::DiskPlan(_)) {
            return Err(DiskIndexError::KeyTableUnavailable);
        }
        let table = transaction.open_table(LATEST_TABLE).map_err(storage)?;
        table.len().map_err(storage)
    }

    /// Begins a write transaction while holding the shared write-admission
    /// gate. Every write transaction on the shared database goes through this
    /// helper, so a conflicting handle fails fast with a typed busy error
    /// instead of blocking inside redb.
    fn begin_gated_quick_immediate(&self) -> DiskIndexResult<(WriteGateGuard, WriteTransaction)> {
        let gate = WriteGateGuard::acquire(&self.write_gate)?;
        let transaction = begin_quick_immediate(&self.database)?;
        Ok((gate, transaction))
    }

    pub fn validate_protocol(&self) -> DiskIndexResult<DiskIndexPersistentState> {
        let (_gate, transaction) = self.begin_gated_quick_immediate()?;
        let savepoints = savepoint_summary(&transaction)?;
        let state = read_state_write(&transaction)?;
        validate_savepoint_set(state.metadata, savepoints)?;
        transaction.abort().map_err(storage)?;
        Ok(state)
    }

    fn validate_envelope(&self) -> DiskIndexResult<DiskIndexMetadata> {
        let (_gate, transaction) = self.begin_gated_quick_immediate()?;
        let savepoints = savepoint_summary(&transaction)?;
        let metadata = read_metadata_write(&transaction)?;
        validate_savepoint_set(metadata, savepoints)?;
        transaction.abort().map_err(storage)?;
        Ok(metadata)
    }

    fn validate_protocol_bounded(
        &self,
        expected_tail_limit: u32,
    ) -> DiskIndexResult<DiskIndexPersistentState> {
        let (_gate, transaction) = self.begin_gated_quick_immediate()?;
        let savepoints = savepoint_summary(&transaction)?;
        let state = read_state_write_bounded(&transaction, expected_tail_limit)?;
        validate_savepoint_set(state.metadata, savepoints)?;
        transaction.abort().map_err(storage)?;
        Ok(state)
    }

    pub fn begin_snapshot_with_mode(
        &self,
        identity: DiskIndexIdentity,
        mode: DiskIndexMode,
        physical_native_eof: u64,
    ) -> DiskIndexResult<DiskIndexSnapshot> {
        let transaction = self.database.begin_read().map_err(storage)?;
        let metadata = read_metadata(&transaction)?;
        validate_reader_metadata(metadata, identity, mode, physical_native_eof)?;
        Ok(DiskIndexSnapshot {
            transaction,
            metadata,
            max_key_bytes: self.options.max_key_bytes,
        })
    }

    pub fn begin_snapshot_with_plan(
        &self,
        identity: DiskIndexIdentity,
        plan: DiskIndexPlan,
        physical_native_eof: u64,
    ) -> DiskIndexResult<DiskIndexSnapshot> {
        self.begin_snapshot_with_mode(identity, plan.mode(), physical_native_eof)
    }

    pub fn validate_clean_writer(
        &self,
        identity: DiskIndexIdentity,
        mode: DiskIndexMode,
        physical_native_eof: u64,
        expected_tail_limit: u32,
    ) -> DiskIndexResult<DiskIndexPersistentState> {
        let state = self.validate_protocol_bounded(expected_tail_limit)?;
        validate_identity_mode(state.metadata, identity, mode)?;
        if state.metadata.state != DiskIndexState::Clean {
            return Err(DiskIndexError::CleanStateRequired);
        }
        if physical_native_eof != state.metadata.committed.eof {
            return Err(DiskIndexError::NativeLengthMismatch {
                expected: state.metadata.committed.eof,
                actual: physical_native_eof,
            });
        }
        Ok(state)
    }

    /// Begins a dirty generation by durably saving the clean root before any
    /// user table is opened in the write transaction.
    pub fn begin_generation(&self) -> DiskIndexResult<DiskIndexMetadata> {
        // redb 4.1 marks a transaction dirty even when listing its system
        // savepoint table. Assert the clean/no-savepoint precondition in a
        // separate transaction so savepoint creation is the first operation in
        // the generation transaction.
        let clean = self.validate_protocol()?;
        if clean.metadata.state != DiskIndexState::Clean {
            return Err(DiskIndexError::CleanStateRequired);
        }

        let (_gate, transaction) = self.begin_gated_quick_immediate()?;
        let savepoint_id = transaction
            .persistent_savepoint()
            .map_err(|error| savepoint_error("create", error))?;
        let savepoints = savepoint_summary(&transaction)?;
        validate_expected_savepoint(Some(savepoint_id), savepoints)?;

        let current = read_metadata_write(&transaction)?;
        if current != clean.metadata {
            return Err(DiskIndexError::CheckpointMismatch(
                "clean root changed before savepoint creation",
            ));
        }
        let mut dirty = current;
        dirty.state = DiskIndexState::Dirty;
        dirty.checkpoint = Some(DiskIndexCheckpoint {
            savepoint_id,
            base: current.committed,
        });
        dirty.working = current.committed;
        dirty.working_tail_count = current.committed_tail_count;
        dirty.working_tail_digest = current.committed_tail_digest;
        validate_metadata(dirty)?;
        write_metadata(&transaction, dirty)?;
        crate::scalable_fault_point("generation.commit");
        let commit = transaction.commit();
        crate::scalable_fault_point("generation.commit");
        commit.map_err(storage)?;
        Ok(dirty)
    }

    pub fn mark_dirty(&self) -> DiskIndexResult<DiskIndexMetadata> {
        self.begin_generation()
    }

    #[cfg(test)]
    pub fn apply_update(
        &self,
        expected_covered_eof: u64,
        new_covered_eof: u64,
        update: &DiskIndexUpdate,
    ) -> DiskIndexResult<DiskIndexMetadata> {
        if self.read_metadata()?.state == DiskIndexState::Clean {
            self.begin_generation()?;
        }
        let mut batch = self.begin_write_batch()?;
        let metadata = batch.apply_update(expected_covered_eof, new_covered_eof, update)?;
        batch.commit()?;
        Ok(metadata)
    }

    pub(crate) fn begin_write_batch(&self) -> DiskIndexResult<DiskIndexWriteBatch> {
        // Compatibility for explicit rebuilds that start from a just-created clean root.
        if self.read_metadata()?.state == DiskIndexState::Clean {
            self.begin_generation()?;
        }

        let (gate, transaction) = self.begin_gated_quick_immediate()?;
        let savepoints = savepoint_summary(&transaction)?;
        let metadata = read_metadata_write(&transaction)?;
        if metadata.state != DiskIndexState::Dirty {
            return Err(DiskIndexError::DirtyStateRequired);
        }
        validate_savepoint_set(metadata, savepoints)?;
        // Reuse this handle's last committed tail map when the working tail
        // digest proves nothing else committed in between; only a cache miss
        // pays the full per-tail read and validation pass.
        let tails = match take_tail_cache(&self.tail_cache, metadata) {
            Some(tails) => tails,
            None => read_tail_map_write(&transaction, metadata)?,
        };
        Ok(DiskIndexWriteBatch {
            transaction,
            _gate: gate,
            metadata,
            tails,
            changed_tails: BTreeSet::new(),
            pending_latest: BTreeMap::new(),
            tail_cache: Arc::clone(&self.tail_cache),
            options: self.options,
            records: 0,
            bytes: 0,
        })
    }

    pub(crate) fn publish_clean(
        &self,
        expected_working: DiskIndexFrontier,
    ) -> DiskIndexResult<DiskIndexMetadata> {
        let preflight = self.read_state()?;
        if preflight.metadata.state != DiskIndexState::Dirty {
            return Err(DiskIndexError::DirtyStateRequired);
        }
        if preflight.metadata.working != expected_working {
            return Err(DiskIndexError::CheckpointMismatch(
                "working frontier changed before clean publication",
            ));
        }
        let checkpoint = preflight
            .metadata
            .checkpoint
            .ok_or(DiskIndexError::MetadataInvariant(
                "dirty metadata has no checkpoint",
            ))?;

        let (_gate, transaction) = self.begin_gated_quick_immediate()?;
        let savepoints = savepoint_summary(&transaction)?;
        validate_expected_savepoint(Some(checkpoint.savepoint_id), savepoints)?;
        let current = read_state_write(&transaction)?;
        if current != preflight {
            return Err(DiskIndexError::CheckpointMismatch(
                "working root changed during clean publication",
            ));
        }

        let mut clean = current.metadata;
        clean.generation = clean
            .generation
            .checked_add(1)
            .ok_or(DiskIndexError::GenerationExhausted)?;
        clean.state = DiskIndexState::Clean;
        clean.committed = clean.working;
        clean.committed_tail_count = clean.working_tail_count;
        clean.committed_tail_digest = clean.working_tail_digest;
        clean.checkpoint = None;
        validate_metadata(clean)?;

        if !transaction
            .delete_persistent_savepoint(checkpoint.savepoint_id)
            .map_err(|error| savepoint_error("delete during clean publication", error))?
        {
            return Err(DiskIndexError::UnexpectedSavepoints {
                expected: Some(checkpoint.savepoint_id),
                first_actual: None,
                actual_count: 0,
            });
        }
        write_metadata(&transaction, clean)?;
        crate::scalable_fault_point("publish.clean_commit");
        let commit = transaction.commit();
        crate::scalable_fault_point("publish.clean_commit");
        commit.map_err(storage)?;
        Ok(clean)
    }

    #[cfg(test)]
    pub fn mark_clean(&self, expected_covered_eof: u64) -> DiskIndexResult<DiskIndexMetadata> {
        let metadata = self.read_metadata()?;
        if metadata.working.eof != expected_covered_eof {
            return Err(DiskIndexError::CoverageMismatch {
                expected: expected_covered_eof,
                actual: metadata.working.eof,
            });
        }
        // Preserve the existing writer's idempotent no-write `sync` behavior.
        // New integration should call `publish_clean`, which remains dirty-only.
        if metadata.state == DiskIndexState::Clean {
            return Ok(metadata);
        }
        self.publish_clean(metadata.working)
    }

    /// Restores the clean root in an uncommitted redb transaction and validates
    /// it completely. The returned guard must stay alive while the caller checks,
    /// truncates, and syncs the native file.
    pub fn stage_restore(
        &self,
        expected_identity: DiskIndexIdentity,
        expected_mode: DiskIndexMode,
        physical_native_eof: u64,
        expected_tail_limit: u32,
    ) -> DiskIndexResult<DiskIndexRestoreGuard> {
        let transaction = self.database.begin_read().map_err(storage)?;
        let dirty = read_state_bounded(&transaction, expected_tail_limit)?;
        drop(transaction);
        if dirty.metadata.state != DiskIndexState::Dirty {
            return Err(DiskIndexError::DirtyStateRequired);
        }
        validate_identity_mode(dirty.metadata, expected_identity, expected_mode)?;
        let checkpoint = dirty
            .metadata
            .checkpoint
            .ok_or(DiskIndexError::MetadataInvariant(
                "dirty metadata has no checkpoint",
            ))?;
        if physical_native_eof < checkpoint.base.eof {
            return Err(DiskIndexError::NativeTooShort {
                required: checkpoint.base.eof,
                actual: physical_native_eof,
            });
        }

        let (gate, mut transaction) = self.begin_gated_quick_immediate()?;
        let savepoints = savepoint_summary(&transaction)?;
        validate_expected_savepoint(Some(checkpoint.savepoint_id), savepoints)?;
        let savepoint = transaction
            .get_persistent_savepoint(checkpoint.savepoint_id)
            .map_err(|error| savepoint_error("fetch for restore", error))?;
        crate::scalable_fault_point("restore.stage");
        let restore = transaction.restore_savepoint(&savepoint);
        crate::scalable_fault_point("restore.stage");
        restore.map_err(|error| savepoint_error("stage restore", error))?;
        drop(savepoint);

        // This is intentionally the first user-table access in the restore transaction.
        let staged = read_state_write_bounded(&transaction, expected_tail_limit)?;
        validate_restored_root(&dirty.metadata, &staged)?;
        Ok(DiskIndexRestoreGuard {
            transaction: Some(transaction),
            _gate: gate,
            savepoint_id: checkpoint.savepoint_id,
            staged,
        })
    }

    fn initialize(
        &self,
        mut metadata: DiskIndexMetadata,
        tails: &[DiskIndexTail],
    ) -> DiskIndexResult<()> {
        if metadata.state != DiskIndexState::Clean || metadata.checkpoint.is_some() {
            return Err(DiskIndexError::CleanStateRequired);
        }
        let tail_map = canonical_tail_map(tails, metadata.tail_limit, metadata.committed)?;
        let count =
            u32::try_from(tail_map.len()).map_err(|_| DiskIndexError::TailLimitExceeded {
                actual: tail_map.len() as u64,
                limit: metadata.tail_limit,
            })?;
        let digest = tail_digest(tail_map.values().copied());
        metadata.committed_tail_count = count;
        metadata.working_tail_count = count;
        metadata.committed_tail_digest = digest;
        metadata.working_tail_digest = digest;
        validate_metadata(metadata)?;

        let (_gate, transaction) = self.begin_gated_quick_immediate()?;
        let savepoints = savepoint_summary(&transaction)?;
        validate_expected_savepoint(None, savepoints)?;

        transaction.delete_table(META_TABLE).map_err(storage)?;
        transaction.delete_table(TAILS_TABLE).map_err(storage)?;
        transaction.delete_table(LATEST_TABLE).map_err(storage)?;
        {
            let mut table = transaction.open_table(META_TABLE).map_err(storage)?;
            table
                .insert(META_KEY, encode_metadata(metadata).as_slice())
                .map_err(storage)?;
        }
        {
            let mut table = transaction.open_table(TAILS_TABLE).map_err(storage)?;
            for tail in tail_map.values() {
                table
                    .insert(tail.block_id, encode_tail(*tail).as_slice())
                    .map_err(storage)?;
            }
        }
        if matches!(metadata.mode, DiskIndexMode::DiskPlan(_)) {
            transaction.open_table(LATEST_TABLE).map_err(storage)?;
        }
        transaction.commit().map_err(storage)?;
        store_tail_cache(&self.tail_cache, digest, tail_map);
        Ok(())
    }
}

pub(crate) struct DiskIndexWriteBatch {
    transaction: WriteTransaction,
    // Holds write admission on the shared database for the batch lifetime.
    _gate: WriteGateGuard,
    metadata: DiskIndexMetadata,
    tails: BTreeMap<u32, DiskIndexTail>,
    changed_tails: BTreeSet<u32>,
    /// Latest-table rows staged by this batch. Buffering the encoded rows in
    /// memory keeps the per-record hot path free of redb table opens: commit
    /// opens `LATEST_TABLE` once and drains the map. Memory stays bounded by
    /// the batch byte budget, whose per-item accounting covers each staged
    /// key and encoded value.
    pending_latest: BTreeMap<Vec<u8>, [u8; LATEST_LEN]>,
    tail_cache: Arc<Mutex<Option<TailCache>>>,
    options: DiskIndexOptions,
    records: usize,
    bytes: usize,
}

impl DiskIndexWriteBatch {
    pub(crate) fn can_accept_update(
        &self,
        update: &DiskIndexUpdate,
        has_tail: bool,
    ) -> DiskIndexResult<bool> {
        self.can_accept_item(checked_item_bytes(update.canonical_key.len(), has_tail)?)
    }

    pub(crate) fn can_accept_coverage(&self, has_tail: bool) -> DiskIndexResult<bool> {
        self.can_accept_item(checked_item_bytes(0, has_tail)?)
    }

    #[cfg(test)]
    pub(crate) fn lookup<K: VarveDiskKey>(
        &self,
        block_id: u32,
        key: &K,
    ) -> DiskIndexResult<DiskIndexEntry> {
        Ok(self.lookup_pointer(block_id, key)?.entry)
    }

    #[cfg(test)]
    pub(crate) fn lookup_pointer<K: VarveDiskKey>(
        &self,
        block_id: u32,
        key: &K,
    ) -> DiskIndexResult<DiskIndexRecordPointer> {
        let canonical = encode_disk_key(key, self.options.max_key_bytes)?;
        self.lookup_pointer_canonical(block_id, &canonical)
    }

    /// Builds the composite key this batch stores a row under.
    ///
    /// The only constructor of [`CompositeKey`], and the reason that type has a
    /// private field: a caller who has one has it from here, with this batch's
    /// `max_key_bytes`, so a canonical key cannot be passed where a composite is
    /// required.
    pub(crate) fn composite_key_for(
        &self,
        block_id: u32,
        canonical_key: &[u8],
    ) -> DiskIndexResult<CompositeKey> {
        Ok(CompositeKey(composite_key(
            block_id,
            canonical_key,
            self.options.max_key_bytes,
        )?))
    }

    /// Kept as the single-call form the batch's own tests use. The append path
    /// goes through [`Self::composite_key_for`] and
    /// [`Self::lookup_pointer_composite`] instead, because it needs the
    /// composite key afterwards and this would throw it away.
    #[cfg(test)]
    pub(crate) fn lookup_pointer_canonical(
        &self,
        block_id: u32,
        canonical_key: &[u8],
    ) -> DiskIndexResult<DiskIndexRecordPointer> {
        ensure_key_limit(canonical_key.len(), self.options.max_key_bytes)?;
        if !matches!(self.metadata.mode, DiskIndexMode::DiskPlan(_)) {
            return Err(DiskIndexError::KeyTableUnavailable);
        }
        let composite = self.composite_key_for(block_id, canonical_key)?;
        self.lookup_pointer_composite(block_id, &composite)
    }

    /// The lookup, given a composite key that has already been built.
    ///
    /// Split out so that a caller which must build the composite anyway — the
    /// keyed-offset-chain path, which looks the previous record up and then
    /// stages a row under the same key — builds it once and hands the same
    /// value on to [`Self::apply_update_with_tail`].
    pub(crate) fn lookup_pointer_composite(
        &self,
        block_id: u32,
        composite: &CompositeKey,
    ) -> DiskIndexResult<DiskIndexRecordPointer> {
        if !matches!(self.metadata.mode, DiskIndexMode::DiskPlan(_)) {
            return Err(DiskIndexError::KeyTableUnavailable);
        }
        // Rows staged by this batch shadow the last committed table row.
        if let Some(value) = self.pending_latest.get(composite.as_slice()) {
            return latest_pointer(block_id, decode_latest(value)?);
        }
        let table = self.transaction.open_table(LATEST_TABLE).map_err(storage)?;
        let Some(value) = table.get(composite.as_slice()).map_err(storage)? else {
            return Ok(DiskIndexRecordPointer {
                entry: DiskIndexEntry::Missing,
                target_block_id: block_id,
                physical: None,
            });
        };
        latest_pointer(block_id, decode_latest(value.value())?)
    }

    #[cfg(test)]
    pub(crate) fn apply_update(
        &mut self,
        expected_covered_eof: u64,
        new_covered_eof: u64,
        update: &DiskIndexUpdate,
    ) -> DiskIndexResult<DiskIndexMetadata> {
        self.apply_update_with_tail(expected_covered_eof, new_covered_eof, update, None, None)
    }

    /// `composite` is the row's key when the caller already built it — the
    /// keyed-offset-chain path does, to look the previous record up — and
    /// `None` when it did not, which is the default policy. Either way exactly
    /// one composite key is built per staged row, and it is *moved* into
    /// `pending_latest` rather than copied.
    pub(crate) fn apply_update_with_tail(
        &mut self,
        expected_covered_eof: u64,
        new_covered_eof: u64,
        update: &DiskIndexUpdate,
        tail: Option<DiskIndexTail>,
        composite: Option<CompositeKey>,
    ) -> DiskIndexResult<DiskIndexMetadata> {
        if !matches!(self.metadata.mode, DiskIndexMode::DiskPlan(_)) {
            return Err(DiskIndexError::KeyTableUnavailable);
        }
        ensure_key_limit(update.canonical_key.len(), self.options.max_key_bytes)?;
        let (record_offset, sequence, kind) = entry_parts(update.entry)?;
        let next = self.next_frontier(expected_covered_eof, new_covered_eof, sequence)?;
        validate_physical_update(update, kind, record_offset, new_covered_eof)?;
        if let Some(tail) = tail {
            validate_tail(tail, next)?;
            self.validate_tail_capacity(tail.block_id)?;
        }
        let item_bytes = checked_item_bytes(update.canonical_key.len(), tail.is_some())?;
        self.ensure_batch_capacity(item_bytes)?;

        let key = match composite {
            Some(key) => {
                // Checked by inspection rather than by rebuilding: rebuilding
                // would allocate the very key this parameter exists to avoid
                // allocating, and `debug_assert` is live in every test run, so
                // the measurement would report no improvement. Reading the
                // three fields back out of the bytes tests the same claim for
                // no allocation at all.
                debug_assert!(
                    composite_key_matches(key.as_slice(), update.block_id, &update.canonical_key),
                    "a composite key built by the caller does not encode this update's \
                     block id and canonical key"
                );
                key.into_vec()
            }
            None => composite_key(
                update.block_id,
                &update.canonical_key,
                self.options.max_key_bytes,
            )?,
        };
        let value = encode_latest(
            kind,
            record_offset,
            sequence,
            update.block_id,
            update.physical,
        );
        // Stage the row in memory; commit opens the latest table once for
        // the whole batch instead of once per record. A later update of the
        // same key within the batch overwrites the staged row, exactly like
        // the redb insert it replaces.
        self.pending_latest.insert(key, value);
        self.metadata.working = next;
        if let Some(tail) = tail {
            self.tails.insert(tail.block_id, tail);
            self.changed_tails.insert(tail.block_id);
        }
        self.records += 1;
        self.bytes += item_bytes;
        Ok(self.metadata)
    }

    #[cfg(test)]
    pub(crate) fn advance_coverage(
        &mut self,
        expected_covered_eof: u64,
        new_covered_eof: u64,
        sequence: u64,
    ) -> DiskIndexResult<DiskIndexMetadata> {
        self.advance_coverage_with_tail(expected_covered_eof, new_covered_eof, sequence, None)
    }

    pub(crate) fn advance_coverage_with_tail(
        &mut self,
        expected_covered_eof: u64,
        new_covered_eof: u64,
        sequence: u64,
        tail: Option<DiskIndexTail>,
    ) -> DiskIndexResult<DiskIndexMetadata> {
        let next = self.next_frontier(expected_covered_eof, new_covered_eof, sequence)?;
        if let Some(tail) = tail {
            validate_tail(tail, next)?;
            self.validate_tail_capacity(tail.block_id)?;
        }
        let item_bytes = checked_item_bytes(0, tail.is_some())?;
        self.ensure_batch_capacity(item_bytes)?;
        self.metadata.working = next;
        if let Some(tail) = tail {
            self.tails.insert(tail.block_id, tail);
            self.changed_tails.insert(tail.block_id);
        }
        self.records += 1;
        self.bytes += item_bytes;
        Ok(self.metadata)
    }

    /// Re-stamps the primary generation witness for this batch (STO-01).
    ///
    /// Only called while the witness window is still growing — once it covers
    /// [`PRIMARY_GENERATION_WINDOW`] bytes it is frozen for the file's life, so
    /// steady-state appends do no witness work at all.
    pub(crate) fn set_primary_generation(
        &mut self,
        generation: DiskIndexPrimaryGeneration,
    ) -> DiskIndexResult<()> {
        self.metadata.primary_generation = generation;
        Ok(())
    }

    pub(crate) const fn primary_generation(&self) -> DiskIndexPrimaryGeneration {
        self.metadata.primary_generation
    }

    #[cfg(test)]
    pub(crate) const fn working_frontier(&self) -> DiskIndexFrontier {
        self.metadata.working
    }

    #[cfg(test)]
    pub(crate) const fn records(&self) -> usize {
        self.records
    }

    #[cfg(test)]
    pub(crate) const fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn commit(mut self) -> DiskIndexResult<()> {
        if self.records == 0 && self.changed_tails.is_empty() {
            debug_assert!(self.pending_latest.is_empty());
            // Nothing was staged: the map is still exactly the committed
            // collection, so hand it back for the next batch.
            let digest = self.metadata.working_tail_digest;
            let tails = std::mem::take(&mut self.tails);
            self.transaction.abort().map_err(storage)?;
            store_tail_cache(&self.tail_cache, digest, tails);
            return Ok(());
        }

        let tail_count =
            u32::try_from(self.tails.len()).map_err(|_| DiskIndexError::TailLimitExceeded {
                actual: self.tails.len() as u64,
                limit: self.metadata.tail_limit,
            })?;
        let digest = if self.changed_tails.is_empty() {
            self.metadata.working_tail_digest
        } else {
            tail_digest(self.tails.values().copied())
        };
        self.metadata.working_tail_count = tail_count;
        self.metadata.working_tail_digest = digest;
        validate_metadata(self.metadata)?;
        // Only tails staged by this batch need revalidation against the final
        // frontier: every unchanged tail was validated when its map was read
        // (or previously committed), against a frontier this batch has only
        // advanced monotonically.
        for block_id in &self.changed_tails {
            let tail = self
                .tails
                .get(block_id)
                .ok_or(DiskIndexError::InvalidTail("changed tail disappeared"))?;
            validate_tail(*tail, self.metadata.working)?;
        }

        if !self.pending_latest.is_empty() {
            // The single latest-table open of the batch.
            let mut latest = self.transaction.open_table(LATEST_TABLE).map_err(storage)?;
            for (key, value) in &self.pending_latest {
                latest
                    .insert(key.as_slice(), value.as_slice())
                    .map_err(storage)?;
            }
        }
        if !self.changed_tails.is_empty() {
            let mut table = self.transaction.open_table(TAILS_TABLE).map_err(storage)?;
            for block_id in &self.changed_tails {
                let tail = self
                    .tails
                    .get(block_id)
                    .ok_or(DiskIndexError::InvalidTail("changed tail disappeared"))?;
                table
                    .insert(*block_id, encode_tail(*tail).as_slice())
                    .map_err(storage)?;
            }
        }
        write_metadata(&self.transaction, self.metadata)?;
        crate::scalable_fault_point("append.sidecar_batch_commit");
        let commit = self.transaction.commit();
        crate::scalable_fault_point("append.sidecar_batch_commit");
        commit.map_err(storage)?;
        store_tail_cache(&self.tail_cache, digest, self.tails);
        Ok(())
    }

    fn next_frontier(
        &self,
        expected_eof: u64,
        new_eof: u64,
        sequence: u64,
    ) -> DiskIndexResult<DiskIndexFrontier> {
        if self.metadata.working.eof != expected_eof {
            return Err(DiskIndexError::CoverageMismatch {
                expected: expected_eof,
                actual: self.metadata.working.eof,
            });
        }
        if new_eof <= expected_eof {
            return Err(DiskIndexError::MetadataInvariant(
                "working EOF must advance for every record",
            ));
        }
        if self.metadata.working.next_sequence != Some(sequence) {
            return Err(DiskIndexError::SequenceMismatch {
                expected: self.metadata.working.next_sequence,
                actual: sequence,
            });
        }
        let record_count = self
            .metadata
            .working
            .record_count
            .checked_add(1)
            .ok_or(DiskIndexError::RecordCountExhausted)?;
        Ok(DiskIndexFrontier {
            eof: new_eof,
            record_count,
            next_sequence: sequence.checked_add(1),
        })
    }

    fn validate_tail_capacity(&self, block_id: u32) -> DiskIndexResult<()> {
        if self.tails.contains_key(&block_id) {
            return Ok(());
        }
        let actual = self.tails.len().saturating_add(1) as u64;
        if actual > u64::from(self.metadata.tail_limit) {
            return Err(DiskIndexError::TailLimitExceeded {
                actual,
                limit: self.metadata.tail_limit,
            });
        }
        Ok(())
    }

    fn ensure_batch_capacity(&self, item_bytes: usize) -> DiskIndexResult<()> {
        let records = self.records.saturating_add(1);
        let bytes = self.bytes.saturating_add(item_bytes);
        if records > self.options.batch.max_records || bytes > self.options.batch.max_bytes {
            return Err(DiskIndexError::BatchFull {
                records,
                bytes,
                max_records: self.options.batch.max_records,
                max_bytes: self.options.batch.max_bytes,
            });
        }
        Ok(())
    }

    fn can_accept_item(&self, item_bytes: usize) -> DiskIndexResult<bool> {
        let records = self.records.saturating_add(1);
        let bytes = self.bytes.saturating_add(item_bytes);
        Ok(records <= self.options.batch.max_records && bytes <= self.options.batch.max_bytes)
    }
}

pub struct DiskIndexRestoreGuard {
    transaction: Option<WriteTransaction>,
    // Holds write admission on the shared database until restore completes.
    _gate: WriteGateGuard,
    savepoint_id: u64,
    staged: DiskIndexPersistentState,
}

impl DiskIndexRestoreGuard {
    pub const fn base_eof(&self) -> u64 {
        self.staged.metadata.committed.eof
    }

    pub const fn frontier(&self) -> DiskIndexFrontier {
        self.staged.metadata.committed
    }

    pub fn tails(&self) -> &[DiskIndexTail] {
        &self.staged.tails
    }

    pub const fn primary_generation(&self) -> DiskIndexPrimaryGeneration {
        self.staged.metadata.primary_generation
    }

    /// Commits only after the caller has truncated and synced the native file
    /// and has re-observed exactly the checkpoint EOF.
    pub(crate) fn commit_after_native_sync(
        mut self,
        observed_native_eof: u64,
    ) -> DiskIndexResult<DiskIndexMetadata> {
        let expected = self.base_eof();
        if observed_native_eof != expected {
            return Err(DiskIndexError::NativeLengthMismatch {
                expected,
                actual: observed_native_eof,
            });
        }
        let transaction = self
            .transaction
            .take()
            .ok_or(DiskIndexError::CheckpointMismatch(
                "restore transaction already completed",
            ))?;
        let savepoints = savepoint_summary(&transaction)?;
        validate_expected_savepoint(Some(self.savepoint_id), savepoints)?;
        if !transaction
            .delete_persistent_savepoint(self.savepoint_id)
            .map_err(|error| savepoint_error("delete after restore", error))?
        {
            return Err(DiskIndexError::UnexpectedSavepoints {
                expected: Some(self.savepoint_id),
                first_actual: None,
                actual_count: 0,
            });
        }
        crate::scalable_fault_point("restore.commit");
        let commit = transaction.commit();
        crate::scalable_fault_point("restore.commit");
        commit.map_err(storage)?;
        Ok(self.staged.metadata)
    }

    #[cfg(test)]
    pub fn abort(mut self) -> DiskIndexResult<()> {
        let transaction = self
            .transaction
            .take()
            .ok_or(DiskIndexError::CheckpointMismatch(
                "restore transaction already completed",
            ))?;
        transaction.abort().map_err(storage)
    }
}

pub struct DiskIndexSnapshot {
    transaction: ReadTransaction,
    metadata: DiskIndexMetadata,
    max_key_bytes: usize,
}

impl DiskIndexSnapshot {
    pub const fn committed_eof(&self) -> u64 {
        self.metadata.committed.eof
    }

    pub const fn primary_generation(&self) -> DiskIndexPrimaryGeneration {
        self.metadata.primary_generation
    }

    /// Historical distinct key cardinality of this snapshot's latest-key
    /// table: distinct `(block, key)` pairs ever indexed, including keys whose
    /// latest entry is a tombstone. See
    /// [`DiskIndexStore::historical_distinct_keys`] for reclaim semantics.
    pub fn historical_distinct_keys(&self) -> DiskIndexResult<u64> {
        if !matches!(self.metadata.mode, DiskIndexMode::DiskPlan(_)) {
            return Err(DiskIndexError::KeyTableUnavailable);
        }
        let table = self.transaction.open_table(LATEST_TABLE).map_err(storage)?;
        table.len().map_err(storage)
    }

    #[cfg(test)]
    pub fn lookup<K: VarveDiskKey>(
        &self,
        block_id: u32,
        key: &K,
    ) -> DiskIndexResult<DiskIndexEntry> {
        Ok(self.lookup_pointer(block_id, key)?.entry)
    }

    pub fn lookup_pointer<K: VarveDiskKey>(
        &self,
        block_id: u32,
        key: &K,
    ) -> DiskIndexResult<DiskIndexRecordPointer> {
        let encoded = encode_disk_key(key, self.max_key_bytes)?;
        self.lookup_pointer_canonical(block_id, &encoded)
    }

    pub fn lookup_pointer_canonical(
        &self,
        block_id: u32,
        canonical_key: &[u8],
    ) -> DiskIndexResult<DiskIndexRecordPointer> {
        ensure_key_limit(canonical_key.len(), self.max_key_bytes)?;
        if !matches!(self.metadata.mode, DiskIndexMode::DiskPlan(_)) {
            return Err(DiskIndexError::KeyTableUnavailable);
        }
        let key = composite_key(block_id, canonical_key, self.max_key_bytes)?;
        let table = self.transaction.open_table(LATEST_TABLE).map_err(storage)?;
        let Some(value) = table.get(key.as_slice()).map_err(storage)? else {
            return Ok(DiskIndexRecordPointer {
                entry: DiskIndexEntry::Missing,
                target_block_id: block_id,
                physical: None,
            });
        };
        latest_pointer(block_id, decode_latest(value.value())?)
    }
}

/// Bounded source used by authoritative native scanners during sidecar rebuild.
#[cfg(test)]
pub trait DiskIndexRebuildSource {
    fn next_update(&mut self) -> crate::Result<Option<DiskIndexRebuildEntry>>;
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskIndexRebuildEntry {
    pub update: DiskIndexUpdate,
    pub covered_eof: u64,
}

/// Builds a clean replacement from a bounded native scanner. Publication and
/// directory sync are deliberately left to the writer-lock/fault-injection owner.
#[cfg(test)]
pub fn build_replacement(
    directory: &Path,
    options: DiskIndexOptions,
    metadata: DiskIndexMetadata,
    source: &mut impl DiskIndexRebuildSource,
) -> DiskIndexResult<PreparedDiskIndex> {
    let temporary = tempfile::Builder::new()
        .prefix(".varve-index-")
        .suffix(".vki.tmp")
        .tempfile_in(directory)?;
    let temp_path = temporary.into_temp_path();
    let store = DiskIndexStore::create(&temp_path, options, metadata)?;
    store.begin_generation()?;
    let mut covered_eof = metadata.committed.eof;
    while let Some(entry) = source.next_update()? {
        store.apply_update(covered_eof, entry.covered_eof, &entry.update)?;
        covered_eof = entry.covered_eof;
    }
    store.mark_clean(covered_eof)?;
    drop(store);
    Ok(PreparedDiskIndex { path: temp_path })
}

#[cfg(test)]
pub struct PreparedDiskIndex {
    path: tempfile::TempPath,
}

#[cfg(test)]
impl PreparedDiskIndex {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn begin_quick_immediate(database: &Database) -> DiskIndexResult<WriteTransaction> {
    let mut transaction = database.begin_write().map_err(storage)?;
    transaction.set_quick_repair(true);
    transaction
        .set_durability(Durability::Immediate)
        .map_err(storage)?;
    Ok(transaction)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SavepointSummary {
    count: usize,
    first: Option<u64>,
}

fn savepoint_summary(transaction: &WriteTransaction) -> DiskIndexResult<SavepointSummary> {
    let mut savepoints = transaction
        .list_persistent_savepoints()
        .map_err(|error| savepoint_error("list", error))?;
    let first = savepoints.next();
    // The protocol accepts zero savepoints or exactly one expected id, so
    // enumerate at most two entries instead of counting a possibly huge
    // hostile savepoint set; `count` therefore saturates at 2.
    let count = match first {
        None => 0,
        Some(_) => 1usize.saturating_add(usize::from(savepoints.next().is_some())),
    };
    Ok(SavepointSummary { count, first })
}

fn validate_savepoint_set(
    metadata: DiskIndexMetadata,
    actual: SavepointSummary,
) -> DiskIndexResult<()> {
    let expected = metadata
        .checkpoint
        .map(|checkpoint| checkpoint.savepoint_id);
    validate_expected_savepoint(expected, actual)
}

fn validate_expected_savepoint(
    expected: Option<u64>,
    actual: SavepointSummary,
) -> DiskIndexResult<()> {
    let valid = match expected {
        None => actual.count == 0,
        Some(expected) => actual.count == 1 && actual.first == Some(expected),
    };
    if valid {
        Ok(())
    } else {
        Err(DiskIndexError::UnexpectedSavepoints {
            expected,
            first_actual: actual.first,
            actual_count: actual.count,
        })
    }
}

fn validate_identity_mode(
    metadata: DiskIndexMetadata,
    identity: DiskIndexIdentity,
    mode: DiskIndexMode,
) -> DiskIndexResult<()> {
    if metadata.identity != identity {
        return Err(DiskIndexError::IdentityMismatch);
    }
    if metadata.mode != mode {
        return Err(DiskIndexError::ModeMismatch {
            expected: mode,
            actual: metadata.mode,
        });
    }
    Ok(())
}

fn validate_reader_metadata(
    metadata: DiskIndexMetadata,
    identity: DiskIndexIdentity,
    mode: DiskIndexMode,
    physical_native_eof: u64,
) -> DiskIndexResult<()> {
    validate_identity_mode(metadata, identity, mode)?;
    if metadata.state != DiskIndexState::Clean {
        return Err(DiskIndexError::CleanStateRequired);
    }
    if physical_native_eof < metadata.committed.eof {
        return Err(DiskIndexError::NativeTooShort {
            required: metadata.committed.eof,
            actual: physical_native_eof,
        });
    }
    Ok(())
}

fn validate_restored_root(
    dirty: &DiskIndexMetadata,
    staged: &DiskIndexPersistentState,
) -> DiskIndexResult<()> {
    let checkpoint = dirty.checkpoint.ok_or(DiskIndexError::CheckpointMismatch(
        "dirty root has no checkpoint",
    ))?;
    let clean = staged.metadata;
    if clean.state != DiskIndexState::Clean {
        return Err(DiskIndexError::CheckpointMismatch(
            "restored root is not clean",
        ));
    }
    if clean.identity != dirty.identity {
        return Err(DiskIndexError::CheckpointMismatch(
            "restored identity differs from dirty root",
        ));
    }
    if clean.mode != dirty.mode {
        return Err(DiskIndexError::CheckpointMismatch(
            "restored mode differs from dirty root",
        ));
    }
    if clean.generation != dirty.generation {
        return Err(DiskIndexError::CheckpointMismatch(
            "restored generation differs from dirty root",
        ));
    }
    if clean.committed != checkpoint.base || clean.working != checkpoint.base {
        return Err(DiskIndexError::CheckpointMismatch(
            "restored frontier differs from checkpoint base",
        ));
    }
    if clean.committed != dirty.committed {
        return Err(DiskIndexError::CheckpointMismatch(
            "dirty committed frontier differs from restored root",
        ));
    }
    if clean.committed_tail_count != dirty.committed_tail_count
        || clean.committed_tail_digest != dirty.committed_tail_digest
    {
        return Err(DiskIndexError::CheckpointMismatch(
            "restored tails differ from committed checkpoint tails",
        ));
    }
    Ok(())
}

fn canonical_sidecar_path(path: &Path) -> DiskIndexResult<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent)?;
    let name = path.file_name().ok_or(DiskIndexError::MetadataInvariant(
        "sidecar path has no file name",
    ))?;
    Ok(parent.join(name))
}

pub fn sidecar_path(native_path: impl AsRef<Path>) -> PathBuf {
    append_sidecar_extension(native_path.as_ref(), ".vki")
}

pub fn state_sidecar_path(native_path: impl AsRef<Path>) -> PathBuf {
    append_sidecar_extension(native_path.as_ref(), ".vks")
}

pub(crate) fn read_metadata_read_only(
    path: impl AsRef<Path>,
    options: DiskIndexOptions,
) -> DiskIndexResult<DiskIndexMetadata> {
    let options = options.validate()?;
    let path = canonical_sidecar_path(path.as_ref())?;
    let mut builder = Builder::new();
    builder.set_cache_size(options.cache_bytes);
    let database = builder.open_read_only(path).map_err(database)?;
    let transaction = database.begin_read().map_err(storage)?;
    read_metadata(&transaction)
}

fn append_sidecar_extension(path: &Path, extension: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_owned();
    value.push(extension);
    PathBuf::from(value)
}

fn open_database(
    path: &Path,
    options: DiskIndexOptions,
    create: bool,
) -> DiskIndexResult<Database> {
    let mut builder = Database::builder();
    builder.set_cache_size(options.cache_bytes);
    if create {
        builder.create(path).map_err(database)
    } else {
        builder.open(path).map_err(database)
    }
}

fn read_metadata(transaction: &ReadTransaction) -> DiskIndexResult<DiskIndexMetadata> {
    let table = transaction.open_table(META_TABLE).map_err(storage)?;
    let value = table
        .get(META_KEY)
        .map_err(storage)?
        .ok_or(DiskIndexError::MetadataMissing)?;
    decode_metadata(value.value())
}

fn read_metadata_write(transaction: &WriteTransaction) -> DiskIndexResult<DiskIndexMetadata> {
    let table = transaction.open_table(META_TABLE).map_err(storage)?;
    let value = table
        .get(META_KEY)
        .map_err(storage)?
        .ok_or(DiskIndexError::MetadataMissing)?;
    decode_metadata(value.value())
}

fn write_metadata(
    transaction: &WriteTransaction,
    metadata: DiskIndexMetadata,
) -> DiskIndexResult<()> {
    let mut table = transaction.open_table(META_TABLE).map_err(storage)?;
    table
        .insert(META_KEY, encode_metadata(metadata).as_slice())
        .map_err(storage)?;
    Ok(())
}

fn read_state(transaction: &ReadTransaction) -> DiskIndexResult<DiskIndexPersistentState> {
    let metadata = read_metadata(transaction)?;
    let table = transaction.open_table(TAILS_TABLE).map_err(storage)?;
    let tails = collect_tails(&table, metadata.tail_limit)?;
    validate_working_tails(metadata, &tails)?;
    Ok(DiskIndexPersistentState { metadata, tails })
}

fn read_state_bounded(
    transaction: &ReadTransaction,
    expected_tail_limit: u32,
) -> DiskIndexResult<DiskIndexPersistentState> {
    let metadata = read_metadata(transaction)?;
    validate_expected_tail_limit(metadata, expected_tail_limit)?;
    let table = transaction.open_table(TAILS_TABLE).map_err(storage)?;
    let tails = collect_tails(&table, expected_tail_limit)?;
    validate_working_tails(metadata, &tails)?;
    Ok(DiskIndexPersistentState { metadata, tails })
}

fn read_state_write(transaction: &WriteTransaction) -> DiskIndexResult<DiskIndexPersistentState> {
    let metadata = read_metadata_write(transaction)?;
    let table = transaction.open_table(TAILS_TABLE).map_err(storage)?;
    let tails = collect_tails(&table, metadata.tail_limit)?;
    validate_working_tails(metadata, &tails)?;
    Ok(DiskIndexPersistentState { metadata, tails })
}

fn read_state_write_bounded(
    transaction: &WriteTransaction,
    expected_tail_limit: u32,
) -> DiskIndexResult<DiskIndexPersistentState> {
    let metadata = read_metadata_write(transaction)?;
    validate_expected_tail_limit(metadata, expected_tail_limit)?;
    let table = transaction.open_table(TAILS_TABLE).map_err(storage)?;
    let tails = collect_tails(&table, expected_tail_limit)?;
    validate_working_tails(metadata, &tails)?;
    Ok(DiskIndexPersistentState { metadata, tails })
}

fn validate_expected_tail_limit(metadata: DiskIndexMetadata, expected: u32) -> DiskIndexResult<()> {
    if metadata.tail_limit != expected {
        return Err(DiskIndexError::TailLimitMismatch {
            expected,
            actual: metadata.tail_limit,
        });
    }
    Ok(())
}

fn read_tail_map_write(
    transaction: &WriteTransaction,
    metadata: DiskIndexMetadata,
) -> DiskIndexResult<BTreeMap<u32, DiskIndexTail>> {
    let table = transaction.open_table(TAILS_TABLE).map_err(storage)?;
    let tails = collect_tails(&table, metadata.tail_limit)?;
    validate_working_tails(metadata, &tails)?;
    Ok(tails
        .into_iter()
        .map(|tail| (tail.block_id, tail))
        .collect())
}

fn collect_tails<T>(table: &T, limit: u32) -> DiskIndexResult<Vec<DiskIndexTail>>
where
    T: ReadableTable<u32, &'static [u8]>,
{
    let count = table.len().map_err(storage)?;
    if count > u64::from(limit) {
        return Err(DiskIndexError::TailLimitExceeded {
            actual: count,
            limit,
        });
    }
    let capacity = usize::try_from(count).map_err(|_| DiskIndexError::TailLimitExceeded {
        actual: count,
        limit,
    })?;
    let mut tails = Vec::new();
    tails
        .try_reserve_exact(capacity)
        .map_err(|_| DiskIndexError::TailLimitExceeded {
            actual: count,
            limit,
        })?;
    for entry in table.iter().map_err(storage)? {
        let (key, value) = entry.map_err(storage)?;
        tails.push(decode_tail(key.value(), value.value())?);
    }
    Ok(tails)
}

fn validate_working_tails(
    metadata: DiskIndexMetadata,
    tails: &[DiskIndexTail],
) -> DiskIndexResult<()> {
    let actual_count =
        u32::try_from(tails.len()).map_err(|_| DiskIndexError::TailLimitExceeded {
            actual: tails.len() as u64,
            limit: metadata.tail_limit,
        })?;
    if actual_count != metadata.working_tail_count {
        return Err(DiskIndexError::TailCountMismatch {
            expected: metadata.working_tail_count,
            actual: actual_count,
        });
    }
    if tail_digest(tails.iter().copied()) != metadata.working_tail_digest {
        return Err(DiskIndexError::TailDigestMismatch);
    }
    for tail in tails {
        validate_tail(*tail, metadata.working)?;
    }
    Ok(())
}

fn canonical_tail_map(
    tails: &[DiskIndexTail],
    limit: u32,
    frontier: DiskIndexFrontier,
) -> DiskIndexResult<BTreeMap<u32, DiskIndexTail>> {
    if tails.len() as u64 > u64::from(limit) {
        return Err(DiskIndexError::TailLimitExceeded {
            actual: tails.len() as u64,
            limit,
        });
    }
    let mut map = BTreeMap::new();
    for tail in tails {
        validate_tail(*tail, frontier)?;
        if map.insert(tail.block_id, *tail).is_some() {
            return Err(DiskIndexError::InvalidTail("duplicate block id"));
        }
    }
    Ok(map)
}

fn validate_tail(tail: DiskIndexTail, frontier: DiskIndexFrontier) -> DiskIndexResult<()> {
    if tail.record_offset >= frontier.eof {
        return Err(DiskIndexError::InvalidTail(
            "record offset is outside the working snapshot",
        ));
    }
    if frontier
        .next_sequence
        .is_some_and(|next| tail.sequence >= next)
    {
        return Err(DiskIndexError::InvalidTail(
            "sequence is not before the working frontier",
        ));
    }
    Ok(())
}

fn validate_metadata(metadata: DiskIndexMetadata) -> DiskIndexResult<()> {
    if metadata.primary_generation.len > PRIMARY_GENERATION_WINDOW
        || metadata.primary_generation.len > metadata.working.eof
    {
        return Err(DiskIndexError::MetadataInvariant(
            "primary generation window exceeds its bound",
        ));
    }
    match metadata.mode {
        DiskIndexMode::StateOnly => {}
        DiskIndexMode::DiskPlan(digest) if digest.is_zero() => {
            return Err(DiskIndexError::PlanDigestIsZero);
        }
        DiskIndexMode::DiskPlan(_) => {}
    }
    if metadata.committed.eof > metadata.working.eof {
        return Err(DiskIndexError::MetadataInvariant(
            "working EOF precedes committed EOF",
        ));
    }
    if metadata.committed.record_count > metadata.working.record_count {
        return Err(DiskIndexError::MetadataInvariant(
            "working record count precedes committed count",
        ));
    }
    match (
        metadata.committed.next_sequence,
        metadata.working.next_sequence,
    ) {
        (Some(committed), Some(working)) if working < committed => {
            return Err(DiskIndexError::MetadataInvariant(
                "working sequence precedes committed sequence",
            ));
        }
        (None, Some(_)) => {
            return Err(DiskIndexError::MetadataInvariant(
                "working sequence resumed after exhaustion",
            ));
        }
        _ => {}
    }
    if metadata.committed_tail_count > metadata.tail_limit
        || metadata.working_tail_count > metadata.tail_limit
    {
        return Err(DiskIndexError::TailLimitExceeded {
            actual: u64::from(
                metadata
                    .committed_tail_count
                    .max(metadata.working_tail_count),
            ),
            limit: metadata.tail_limit,
        });
    }
    match metadata.state {
        DiskIndexState::Clean => {
            if metadata.checkpoint.is_some() {
                return Err(DiskIndexError::MetadataInvariant(
                    "clean metadata carries a checkpoint",
                ));
            }
            if metadata.committed != metadata.working {
                return Err(DiskIndexError::MetadataInvariant(
                    "clean committed and working frontiers differ",
                ));
            }
            if metadata.committed_tail_count != metadata.working_tail_count
                || metadata.committed_tail_digest != metadata.working_tail_digest
            {
                return Err(DiskIndexError::MetadataInvariant(
                    "clean committed and working tails differ",
                ));
            }
        }
        DiskIndexState::Dirty => {
            let checkpoint = metadata
                .checkpoint
                .ok_or(DiskIndexError::MetadataInvariant(
                    "dirty metadata has no checkpoint",
                ))?;
            if checkpoint.savepoint_id == 0 {
                return Err(DiskIndexError::MetadataInvariant(
                    "dirty checkpoint has savepoint id zero",
                ));
            }
            if checkpoint.base != metadata.committed {
                return Err(DiskIndexError::MetadataInvariant(
                    "checkpoint base differs from committed frontier",
                ));
            }
        }
    }
    Ok(())
}

/// The bytes a row is stored under: `block_id`, the canonical key's length, and
/// the canonical key.
///
/// A newtype rather than a `Vec<u8>` because the hazard this fix introduces is
/// passing the *canonical* key where a composite is required, which would
/// silently remap every row instead of failing. One private field and one
/// constructor ([`DiskIndexBatch::composite_key_for`]) make that
/// unrepresentable; [`DiskIndexBatch::apply_update_with_tail`] additionally
/// `debug_assert`s any caller-supplied value against the one it would have
/// built itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompositeKey(Vec<u8>);

impl CompositeKey {
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the key. The `Vec` moves on into the staged row, so building it
    /// once and handing it over costs one allocation in total rather than two.
    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

/// Whether `composite` is exactly what [`composite_key`] would have produced
/// for `block_id` and `canonical_key`, decided by reading the encoding back
/// rather than by building it again. Allocates nothing.
fn composite_key_matches(composite: &[u8], block_id: u32, canonical_key: &[u8]) -> bool {
    let Some((prefix, key)) = composite.split_at_checked(8) else {
        return false;
    };
    let Ok(len) = u32::try_from(canonical_key.len()) else {
        return false;
    };
    prefix[..4] == block_id.to_be_bytes()
        && prefix[4..] == len.to_be_bytes()
        && key == canonical_key
}

fn composite_key(block_id: u32, canonical_key: &[u8], limit: usize) -> DiskIndexResult<Vec<u8>> {
    ensure_key_limit(canonical_key.len(), limit)?;
    let len = u32::try_from(canonical_key.len()).map_err(|_| DiskIndexError::KeyTooLong {
        actual: canonical_key.len(),
        limit,
    })?;
    let capacity = 8usize
        .checked_add(canonical_key.len())
        .ok_or(DiskIndexError::KeyTooLong {
            actual: canonical_key.len(),
            limit,
        })?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| DiskIndexError::KeyTooLong {
            actual: canonical_key.len(),
            limit,
        })?;
    output.extend_from_slice(&block_id.to_be_bytes());
    output.extend_from_slice(&len.to_be_bytes());
    output.extend_from_slice(canonical_key);
    Ok(output)
}

fn ensure_key_limit(actual: usize, limit: usize) -> DiskIndexResult<()> {
    if actual > limit {
        Err(DiskIndexError::KeyTooLong { actual, limit })
    } else {
        Ok(())
    }
}

fn checked_item_bytes(key_len: usize, has_tail: bool) -> DiskIndexResult<usize> {
    BATCH_UPDATE_FIXED_BYTES
        .checked_add(key_len)
        .and_then(|value| {
            if has_tail {
                value.checked_add(BATCH_TAIL_BYTES)
            } else {
                Some(value)
            }
        })
        .ok_or(DiskIndexError::MetadataInvariant(
            "batch byte accounting overflow",
        ))
}

pub(crate) const fn batch_item_fixed_bytes(has_tail: bool) -> usize {
    if has_tail {
        BATCH_UPDATE_FIXED_BYTES + BATCH_TAIL_BYTES
    } else {
        BATCH_UPDATE_FIXED_BYTES
    }
}

fn validate_physical_update(
    update: &DiskIndexUpdate,
    kind: LatestKind,
    record_offset: u64,
    new_eof: u64,
) -> DiskIndexResult<()> {
    let Some(physical) = update.physical else {
        return Ok(());
    };
    if physical.physical_len == 0 || physical.block_version == 0 {
        return Err(DiskIndexError::LatestInvariant(
            "physical descriptor has zero length or version",
        ));
    }
    let actual_end =
        record_offset
            .checked_add(physical.physical_len)
            .ok_or(DiskIndexError::LatestInvariant(
                "physical record extent overflows",
            ))?;
    if actual_end != new_eof {
        return Err(DiskIndexError::RecordExtentMismatch {
            expected_end: new_eof,
            actual_end,
        });
    }
    let expected_block = match kind {
        LatestKind::Put => update.block_id,
        LatestKind::Tombstone => TOMBSTONE_BLOCK_ID,
    };
    if physical.block_id != expected_block {
        return Err(DiskIndexError::LatestInvariant(
            "physical record block does not match entry kind",
        ));
    }
    Ok(())
}

fn encode_metadata(metadata: DiskIndexMetadata) -> [u8; META_LEN] {
    let mut bytes = [0u8; META_LEN];
    bytes[0..8].copy_from_slice(&META_MAGIC);
    bytes[8..10].copy_from_slice(&META_VERSION.to_le_bytes());
    bytes[10] = match metadata.state {
        DiskIndexState::Dirty => 0,
        DiskIndexState::Clean => 1,
    };
    bytes[11] = match metadata.mode {
        DiskIndexMode::StateOnly => 0,
        DiskIndexMode::DiskPlan(_) => 1,
    };
    let mut sequence_flags = 0u8;
    if metadata.committed.next_sequence.is_none() {
        sequence_flags |= SEQUENCE_COMMITTED_EXHAUSTED;
    }
    if metadata.working.next_sequence.is_none() {
        sequence_flags |= SEQUENCE_WORKING_EXHAUSTED;
    }
    if metadata
        .checkpoint
        .is_some_and(|checkpoint| checkpoint.base.next_sequence.is_none())
    {
        sequence_flags |= SEQUENCE_BASE_EXHAUSTED;
    }
    bytes[12] = sequence_flags;
    bytes[16..24].copy_from_slice(&metadata.identity.schema_hash.to_le_bytes());
    bytes[24..56].copy_from_slice(&metadata.identity.primary_fingerprint);
    if let DiskIndexMode::DiskPlan(digest) = metadata.mode {
        bytes[56..88].copy_from_slice(digest.as_bytes());
    }
    bytes[88..96].copy_from_slice(&metadata.generation.to_le_bytes());
    encode_frontier(&mut bytes[96..120], metadata.committed);
    encode_frontier(&mut bytes[120..144], metadata.working);
    if let Some(checkpoint) = metadata.checkpoint {
        bytes[144..152].copy_from_slice(&checkpoint.savepoint_id.to_le_bytes());
        encode_frontier(&mut bytes[152..176], checkpoint.base);
    }
    bytes[176..180].copy_from_slice(&metadata.tail_limit.to_le_bytes());
    bytes[180..184].copy_from_slice(&metadata.committed_tail_count.to_le_bytes());
    bytes[184..188].copy_from_slice(&metadata.working_tail_count.to_le_bytes());
    bytes[192..224].copy_from_slice(metadata.committed_tail_digest.as_bytes());
    bytes[224..256].copy_from_slice(metadata.working_tail_digest.as_bytes());
    bytes[256..264].copy_from_slice(&metadata.primary_generation.len.to_le_bytes());
    bytes[264..296].copy_from_slice(metadata.primary_generation.digest.as_bytes());
    let crc = crc32fast::hash(&bytes[..296]);
    bytes[296..300].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn decode_metadata(bytes: &[u8]) -> DiskIndexResult<DiskIndexMetadata> {
    if bytes.len() != META_LEN {
        return Err(DiskIndexError::MetadataLength {
            actual: bytes.len(),
            expected: META_LEN,
        });
    }
    if bytes[0..8] != META_MAGIC {
        return Err(DiskIndexError::MetadataMagic);
    }
    let version = read_u16(bytes, 8);
    if version != META_VERSION {
        return Err(DiskIndexError::MetadataVersion { actual: version });
    }
    let stored_crc = read_u32(bytes, 296);
    if crc32fast::hash(&bytes[..296]) != stored_crc {
        return Err(DiskIndexError::MetadataChecksum);
    }
    if bytes[13..16] != [0; 3] || bytes[188..192] != [0; 4] {
        return Err(DiskIndexError::MetadataReserved);
    }
    let sequence_flags = bytes[12];
    if sequence_flags & !SEQUENCE_FLAGS != 0 {
        return Err(DiskIndexError::MetadataFlags {
            actual: sequence_flags,
        });
    }
    let state = match bytes[10] {
        0 => DiskIndexState::Dirty,
        1 => DiskIndexState::Clean,
        actual => return Err(DiskIndexError::MetadataState { actual }),
    };
    let plan_digest = DiskIndexDigest(bytes[56..88].try_into().expect("fixed metadata slice"));
    let mode = match bytes[11] {
        0 if plan_digest.is_zero() => DiskIndexMode::StateOnly,
        0 => {
            return Err(DiskIndexError::MetadataInvariant(
                "state-only metadata carries a plan digest",
            ));
        }
        1 if !plan_digest.is_zero() => DiskIndexMode::DiskPlan(plan_digest),
        1 => return Err(DiskIndexError::PlanDigestIsZero),
        actual => return Err(DiskIndexError::MetadataMode { actual }),
    };
    let committed = decode_frontier(
        &bytes[96..120],
        sequence_flags & SEQUENCE_COMMITTED_EXHAUSTED != 0,
    )?;
    let working = decode_frontier(
        &bytes[120..144],
        sequence_flags & SEQUENCE_WORKING_EXHAUSTED != 0,
    )?;
    let checkpoint = match state {
        DiskIndexState::Dirty => Some(DiskIndexCheckpoint {
            savepoint_id: read_u64(bytes, 144),
            base: decode_frontier(
                &bytes[152..176],
                sequence_flags & SEQUENCE_BASE_EXHAUSTED != 0,
            )?,
        }),
        DiskIndexState::Clean => {
            if bytes[144..176] != [0; 32] || sequence_flags & SEQUENCE_BASE_EXHAUSTED != 0 {
                return Err(DiskIndexError::MetadataInvariant(
                    "clean metadata carries checkpoint bytes",
                ));
            }
            None
        }
    };
    let metadata = DiskIndexMetadata {
        identity: DiskIndexIdentity {
            schema_hash: read_u64(bytes, 16),
            primary_fingerprint: bytes[24..56].try_into().expect("fixed metadata slice"),
        },
        primary_generation: DiskIndexPrimaryGeneration {
            len: read_u64(bytes, 256),
            digest: DiskIndexDigest(bytes[264..296].try_into().expect("fixed metadata slice")),
        },
        mode,
        state,
        generation: read_u64(bytes, 88),
        committed,
        working,
        checkpoint,
        tail_limit: read_u32(bytes, 176),
        committed_tail_count: read_u32(bytes, 180),
        working_tail_count: read_u32(bytes, 184),
        committed_tail_digest: DiskIndexDigest(
            bytes[192..224].try_into().expect("fixed metadata slice"),
        ),
        working_tail_digest: DiskIndexDigest(
            bytes[224..256].try_into().expect("fixed metadata slice"),
        ),
    };
    validate_metadata(metadata)?;
    Ok(metadata)
}

fn encode_frontier(bytes: &mut [u8], frontier: DiskIndexFrontier) {
    bytes[0..8].copy_from_slice(&frontier.eof.to_le_bytes());
    bytes[8..16].copy_from_slice(&frontier.record_count.to_le_bytes());
    bytes[16..24].copy_from_slice(&frontier.next_sequence.unwrap_or(0).to_le_bytes());
}

fn decode_frontier(bytes: &[u8], exhausted: bool) -> DiskIndexResult<DiskIndexFrontier> {
    let encoded_sequence = read_u64(bytes, 16);
    if exhausted && encoded_sequence != 0 {
        return Err(DiskIndexError::MetadataInvariant(
            "exhausted sequence carries a nonzero value",
        ));
    }
    Ok(DiskIndexFrontier {
        eof: read_u64(bytes, 0),
        record_count: read_u64(bytes, 8),
        next_sequence: (!exhausted).then_some(encoded_sequence),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LatestKind {
    Put,
    Tombstone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LatestValue {
    kind: LatestKind,
    record_offset: u64,
    sequence: u64,
    target_block_id: u32,
    physical: Option<DiskIndexPhysicalRecord>,
}

fn entry_parts(entry: DiskIndexEntry) -> DiskIndexResult<(u64, u64, LatestKind)> {
    match entry {
        DiskIndexEntry::Missing => Err(DiskIndexError::LatestInvariant(
            "missing cannot be stored as a latest value",
        )),
        DiskIndexEntry::Put {
            record_offset,
            sequence,
        } => Ok((record_offset, sequence, LatestKind::Put)),
        DiskIndexEntry::Tombstone {
            record_offset,
            sequence,
        } => Ok((record_offset, sequence, LatestKind::Tombstone)),
    }
}

fn encode_latest(
    kind: LatestKind,
    record_offset: u64,
    sequence: u64,
    target_block_id: u32,
    physical: Option<DiskIndexPhysicalRecord>,
) -> [u8; LATEST_LEN] {
    let mut bytes = [0u8; LATEST_LEN];
    bytes[0] = match kind {
        LatestKind::Put => 0,
        LatestKind::Tombstone => 1,
    };
    bytes[8..16].copy_from_slice(&record_offset.to_le_bytes());
    bytes[24..32].copy_from_slice(&sequence.to_le_bytes());
    bytes[32..36].copy_from_slice(&target_block_id.to_le_bytes());
    if let Some(physical) = physical {
        bytes[1] |= LATEST_HAS_PHYSICAL;
        bytes[2..4].copy_from_slice(&physical.block_version.to_le_bytes());
        bytes[4..6].copy_from_slice(&physical.flags.to_le_bytes());
        bytes[16..24].copy_from_slice(&physical.physical_len.to_le_bytes());
        bytes[36..40].copy_from_slice(&physical.block_id.to_le_bytes());
        if let Some(native_crc) = physical.native_crc {
            bytes[1] |= LATEST_HAS_NATIVE_CRC;
            bytes[40..44].copy_from_slice(&native_crc.to_le_bytes());
        }
    }
    let crc = crc32fast::hash(&bytes[..48]);
    bytes[48..52].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn decode_latest(bytes: &[u8]) -> DiskIndexResult<LatestValue> {
    if bytes.len() != LATEST_LEN {
        return Err(DiskIndexError::LatestLength {
            actual: bytes.len(),
            expected: LATEST_LEN,
        });
    }
    if crc32fast::hash(&bytes[..48]) != read_u32(bytes, 48) {
        return Err(DiskIndexError::LatestChecksum);
    }
    if bytes[6..8] != [0; 2] || bytes[44..48] != [0; 4] {
        return Err(DiskIndexError::LatestReserved);
    }
    let flags = bytes[1];
    if flags & !LATEST_FLAGS != 0 {
        return Err(DiskIndexError::LatestInvariant("unknown flags"));
    }
    let kind = match bytes[0] {
        0 => LatestKind::Put,
        1 => LatestKind::Tombstone,
        _ => return Err(DiskIndexError::LatestInvariant("unknown entry kind")),
    };
    let has_physical = flags & LATEST_HAS_PHYSICAL != 0;
    let has_native_crc = flags & LATEST_HAS_NATIVE_CRC != 0;
    if has_native_crc && !has_physical {
        return Err(DiskIndexError::LatestInvariant(
            "native CRC exists without a physical descriptor",
        ));
    }
    let physical = if has_physical {
        let value = DiskIndexPhysicalRecord {
            physical_len: read_u64(bytes, 16),
            block_id: read_u32(bytes, 36),
            block_version: read_u16(bytes, 2),
            flags: read_u16(bytes, 4),
            native_crc: has_native_crc.then(|| read_u32(bytes, 40)),
        };
        if value.physical_len == 0 || value.block_version == 0 {
            return Err(DiskIndexError::LatestInvariant(
                "physical descriptor has zero length or version",
            ));
        }
        Some(value)
    } else {
        if bytes[2..6] != [0; 4] || bytes[16..24] != [0; 8] || bytes[36..44] != [0; 8] {
            return Err(DiskIndexError::LatestReserved);
        }
        None
    };
    Ok(LatestValue {
        kind,
        record_offset: read_u64(bytes, 8),
        sequence: read_u64(bytes, 24),
        target_block_id: read_u32(bytes, 32),
        physical,
    })
}

fn latest_pointer(
    expected_block_id: u32,
    latest: LatestValue,
) -> DiskIndexResult<DiskIndexRecordPointer> {
    if latest.target_block_id != expected_block_id {
        return Err(DiskIndexError::LatestInvariant("target block id mismatch"));
    }
    let entry = match latest.kind {
        LatestKind::Put => DiskIndexEntry::Put {
            record_offset: latest.record_offset,
            sequence: latest.sequence,
        },
        LatestKind::Tombstone => DiskIndexEntry::Tombstone {
            record_offset: latest.record_offset,
            sequence: latest.sequence,
        },
    };
    Ok(DiskIndexRecordPointer {
        entry,
        target_block_id: latest.target_block_id,
        physical: latest.physical,
    })
}

fn encode_tail(tail: DiskIndexTail) -> [u8; TAIL_LEN] {
    let mut bytes = [0u8; TAIL_LEN];
    bytes[0..4].copy_from_slice(&tail.block_id.to_le_bytes());
    bytes[8..16].copy_from_slice(&tail.record_offset.to_le_bytes());
    bytes[16..24].copy_from_slice(&tail.sequence.to_le_bytes());
    let crc = crc32fast::hash(&bytes[..28]);
    bytes[28..32].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn decode_tail(key: u32, bytes: &[u8]) -> DiskIndexResult<DiskIndexTail> {
    if bytes.len() != TAIL_LEN {
        return Err(DiskIndexError::TailLength {
            actual: bytes.len(),
            expected: TAIL_LEN,
        });
    }
    if crc32fast::hash(&bytes[..28]) != read_u32(bytes, 28) {
        return Err(DiskIndexError::TailChecksum);
    }
    if bytes[4..8] != [0; 4] || bytes[24..28] != [0; 4] {
        return Err(DiskIndexError::TailReserved);
    }
    let encoded = read_u32(bytes, 0);
    if encoded != key {
        return Err(DiskIndexError::TailKeyMismatch { key, encoded });
    }
    Ok(DiskIndexTail {
        block_id: key,
        record_offset: read_u64(bytes, 8),
        sequence: read_u64(bytes, 16),
    })
}

fn tail_digest(tails: impl IntoIterator<Item = DiskIndexTail> + Clone) -> DiskIndexDigest {
    let mut digest = [0u8; 32];
    for lane in 0..8u32 {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(b"varve-disk-index-tails-v1");
        hasher.update(&lane.to_le_bytes());
        for tail in tails.clone() {
            hasher.update(&tail.block_id.to_le_bytes());
            hasher.update(&tail.record_offset.to_le_bytes());
            hasher.update(&tail.sequence.to_le_bytes());
        }
        digest[(lane as usize) * 4..(lane as usize + 1) * 4]
            .copy_from_slice(&hasher.finalize().to_le_bytes());
    }
    DiskIndexDigest(digest)
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("fixed slice"))
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed slice"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("fixed slice"))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crate::{
        BlockDescriptor, BlockKind, Decoder, Encoder, IndexPolicy, ManifestPolicy, ReadLimits,
        RecoveryPolicy, Result, VarveBlock, WireType,
    };

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct PlanItem(u64);

    impl VarveEncode for PlanItem {
        const WIRE_TYPE: WireType = WireType::U64;

        fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
            self.0.encode_varve(encoder)
        }
    }

    impl VarveDecode for PlanItem {
        const WIRE_TYPE: WireType = WireType::U64;

        fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
            Ok(Self(u64::decode_varve(decoder)?))
        }
    }

    impl VarveBlock for PlanItem {
        const ID: u32 = 10;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const SCHEMA_FINGERPRINT: u64 = 0x795638FD38DC1901;
        const IS_KEYED: bool = true;
    }

    impl VarveKeyedBlock for PlanItem {
        type Key = u64;

        fn key(&self) -> Self::Key {
            self.0
        }
    }

    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: PlanItem::ID,
        name: "PlanItem",
        version: PlanItem::VERSION,
        kind: PlanItem::KIND,
        fields: &[],
    }];
    static DESCRIPTORS: &[DiskIndexDescriptor] = &[DiskIndexDescriptor::of::<PlanItem>()];

    fn spec() -> FormatSpec {
        FormatSpec::new(
            b"VDIX",
            1,
            Endian::Little,
            42,
            IndexPolicy::KeyedOffsetChain,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .with_read_limits(ReadLimits::STANDARD)
    }

    fn identity() -> DiskIndexIdentity {
        DiskIndexIdentity {
            schema_hash: 42,
            primary_fingerprint: [7; 32],
        }
    }

    fn plan() -> DiskIndexPlan {
        DiskIndexPlan::canonical(spec(), DESCRIPTORS).unwrap()
    }

    fn metadata(eof: u64) -> DiskIndexMetadata {
        DiskIndexMetadata::new_disk(
            identity(),
            plan().digest(),
            DiskIndexFrontier::empty(eof),
            4,
        )
    }

    fn physical(block_id: u32, physical_len: u64) -> DiskIndexPhysicalRecord {
        DiskIndexPhysicalRecord {
            physical_len,
            block_id,
            block_version: 1,
            flags: 0,
            native_crc: Some(0x1234_5678),
        }
    }

    fn update(
        key: u64,
        entry: DiskIndexEntry,
        physical: DiskIndexPhysicalRecord,
    ) -> DiskIndexUpdate {
        DiskIndexUpdate::from_key_with_record(
            10,
            &key,
            entry,
            physical,
            DiskIndexOptions::default().max_key_bytes,
        )
        .unwrap()
    }

    fn persistent_savepoints(store: &DiskIndexStore) -> Vec<u64> {
        let transaction = store.database.begin_write().unwrap();
        let savepoints = transaction.list_persistent_savepoints().unwrap().collect();
        transaction.abort().unwrap();
        savepoints
    }

    fn rewrite_crc(bytes: &mut [u8; META_LEN]) {
        let crc = crc32fast::hash(&bytes[..296]);
        bytes[296..300].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn canonical_plan_is_deterministic_and_validates_generated_digest() {
        let plan = plan();
        assert_eq!(plan.descriptor(10).unwrap().block_version, 1);
        assert!(!plan.digest().is_zero());
        assert_eq!(
            DiskIndexPlan::canonical(spec(), DESCRIPTORS)
                .unwrap()
                .digest(),
            plan.digest()
        );

        let wrong =
            DiskIndexPlan::from_generated(DESCRIPTORS, DiskIndexDigest::from_bytes([9; 32]));
        assert!(matches!(
            wrong.validate(spec()),
            Err(DiskIndexError::PlanDigestMismatch)
        ));
    }

    #[test]
    fn canonical_witness_does_not_skip_validation_against_another_spec() {
        static BLOCKS_V2: &[BlockDescriptor] = &[BlockDescriptor {
            id: PlanItem::ID,
            name: "PlanItem",
            version: 2,
            kind: PlanItem::KIND,
            fields: &[],
        }];
        let other = FormatSpec::new(
            b"VDIX",
            1,
            Endian::Little,
            42,
            IndexPolicy::KeyedOffsetChain,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS_V2,
        )
        .with_read_limits(ReadLimits::STANDARD);

        // The canonical witness only elides revalidation for the exact block
        // slice it was canonicalized against; a spec with different blocks
        // still runs the full descriptor check.
        let plan = plan();
        assert!(plan.validate(spec()).is_ok());
        assert!(matches!(
            plan.validate(other),
            Err(DiskIndexError::PlanBlockVersionMismatch {
                block_id: 10,
                expected: 1,
                actual: 2,
            })
        ));
    }

    #[test]
    fn batch_options_reject_less_than_one_checkpoint_item() {
        let options = DiskIndexOptions {
            max_key_bytes: 1,
            batch: DiskIndexBatchOptions {
                max_records: 1,
                max_bytes: batch_item_fixed_bytes(true) - 1,
            },
            ..DiskIndexOptions::default()
        };
        assert!(matches!(
            options.validate(),
            Err(DiskIndexError::InvalidBatchOptions(
                "max_bytes is too small for one checkpoint item"
            ))
        ));
    }

    #[test]
    fn metadata_v2_codec_rejects_specific_corruption() {
        let metadata = metadata(64);
        let encoded = encode_metadata(metadata);
        assert_eq!(decode_metadata(&encoded).unwrap(), metadata);

        assert!(matches!(
            decode_metadata(&encoded[..META_LEN - 1]),
            Err(DiskIndexError::MetadataLength { .. })
        ));
        let mut magic = encoded;
        magic[0] ^= 1;
        assert!(matches!(
            decode_metadata(&magic),
            Err(DiskIndexError::MetadataMagic)
        ));
        let mut version = encoded;
        version[8..10].copy_from_slice(&1u16.to_le_bytes());
        assert!(matches!(
            decode_metadata(&version),
            Err(DiskIndexError::MetadataVersion { actual: 1 })
        ));
        let mut checksum = encoded;
        checksum[96] ^= 1;
        assert!(matches!(
            decode_metadata(&checksum),
            Err(DiskIndexError::MetadataChecksum)
        ));
        let mut reserved = encoded;
        reserved[13] = 1;
        rewrite_crc(&mut reserved);
        assert!(matches!(
            decode_metadata(&reserved),
            Err(DiskIndexError::MetadataReserved)
        ));
        let mut bad_mode = encoded;
        bad_mode[11] = 0;
        rewrite_crc(&mut bad_mode);
        assert!(matches!(
            decode_metadata(&bad_mode),
            Err(DiskIndexError::MetadataInvariant(_))
        ));
    }

    #[test]
    fn latest_codec_carries_physical_extent_and_crc() {
        let physical = physical(10, 36);
        let encoded = encode_latest(LatestKind::Put, 64, 0, 10, Some(physical));
        assert_eq!(
            decode_latest(&encoded).unwrap(),
            LatestValue {
                kind: LatestKind::Put,
                record_offset: 64,
                sequence: 0,
                target_block_id: 10,
                physical: Some(physical),
            }
        );
        let mut corrupt = encoded;
        corrupt[16] ^= 1;
        assert!(matches!(
            decode_latest(&corrupt),
            Err(DiskIndexError::LatestChecksum)
        ));
    }

    #[test]
    fn arbitrary_sidecar_codec_bytes_never_panic() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for case in 0..4_096u32 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = (state as usize) % (META_LEN + 33);
            let mut bytes = vec![0u8; len];
            for (index, byte) in bytes.iter_mut().enumerate() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = (state as u8) ^ (index as u8) ^ (case as u8);
            }
            let result = std::panic::catch_unwind(|| {
                let _ = decode_metadata(&bytes);
                let _ = decode_latest(&bytes);
                let _ = decode_tail(case, &bytes);
            });
            assert!(result.is_ok(), "sidecar codec panicked for case {case}");
        }
    }

    #[test]
    fn begin_batch_and_clean_use_one_persistent_savepoint() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.vki");
        let store =
            DiskIndexStore::create(&path, DiskIndexOptions::default(), metadata(64)).unwrap();

        let dirty = store.begin_generation().unwrap();
        let savepoint_id = dirty.checkpoint.unwrap().savepoint_id;
        assert_eq!(persistent_savepoints(&store), [savepoint_id]);

        let mut batch = store.begin_write_batch().unwrap();
        batch
            .apply_update_with_tail(
                64,
                100,
                &update(
                    9,
                    DiskIndexEntry::Put {
                        record_offset: 64,
                        sequence: 0,
                    },
                    physical(10, 36),
                ),
                Some(DiskIndexTail {
                    block_id: 10,
                    record_offset: 64,
                    sequence: 0,
                }),
                None,
            )
            .unwrap();
        assert_eq!(batch.records(), 1);
        assert!(batch.bytes() > 0);
        batch.commit().unwrap();

        let working = DiskIndexFrontier::new(100, 1, Some(1));
        let dirty = store.read_state().unwrap();
        assert_eq!(dirty.metadata.working, working);
        assert_eq!(dirty.metadata.working_tail_count, 1);
        let clean = store.publish_clean(working).unwrap();
        assert_eq!(clean.state, DiskIndexState::Clean);
        assert_eq!(clean.generation, 1);
        assert_eq!(clean.committed, working);
        assert!(clean.checkpoint.is_none());
        assert!(persistent_savepoints(&store).is_empty());

        let pointer = store
            .begin_snapshot_with_plan(identity(), plan(), 100)
            .unwrap()
            .lookup_pointer(10, &9u64)
            .unwrap();
        assert_eq!(pointer.physical, Some(physical(10, 36)));
    }

    #[test]
    fn staged_restore_aborts_without_mutation_then_commits_after_native_sync() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.vki");
        let store =
            DiskIndexStore::create(&path, DiskIndexOptions::default(), metadata(64)).unwrap();
        store.begin_generation().unwrap();
        let mut batch = store.begin_write_batch().unwrap();
        batch
            .apply_update(
                64,
                100,
                &update(
                    9,
                    DiskIndexEntry::Put {
                        record_offset: 64,
                        sequence: 0,
                    },
                    physical(10, 36),
                ),
            )
            .unwrap();
        batch.commit().unwrap();

        let guard = store
            .stage_restore(identity(), plan().mode(), 100, 4)
            .unwrap();
        assert_eq!(guard.base_eof(), 64);
        assert_eq!(guard.frontier(), DiskIndexFrontier::empty(64));
        guard.abort().unwrap();
        assert_eq!(store.read_metadata().unwrap().state, DiskIndexState::Dirty);
        assert_eq!(persistent_savepoints(&store).len(), 1);

        assert!(matches!(
            store.stage_restore(identity(), plan().mode(), 63, 4),
            Err(DiskIndexError::NativeTooShort { .. })
        ));
        let guard = store
            .stage_restore(identity(), plan().mode(), 100, 4)
            .unwrap();
        assert!(matches!(
            guard.commit_after_native_sync(65),
            Err(DiskIndexError::NativeLengthMismatch { .. })
        ));
        assert_eq!(store.read_metadata().unwrap().state, DiskIndexState::Dirty);

        let guard = store
            .stage_restore(identity(), plan().mode(), 100, 4)
            .unwrap();
        let clean = guard.commit_after_native_sync(64).unwrap();
        assert_eq!(clean.state, DiskIndexState::Clean);
        assert_eq!(clean.generation, 0);
        assert!(persistent_savepoints(&store).is_empty());
        assert_eq!(
            store
                .begin_snapshot_with_plan(identity(), plan(), 64)
                .unwrap()
                .lookup(10, &9u64)
                .unwrap(),
            DiskIndexEntry::Missing
        );
    }

    #[test]
    fn dirty_checkpoint_survives_reopen_and_restores() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.vki");
        {
            let store =
                DiskIndexStore::create(&path, DiskIndexOptions::default(), metadata(64)).unwrap();
            store.begin_generation().unwrap();
            let mut batch = store.begin_write_batch().unwrap();
            batch.advance_coverage(64, 90, 0).unwrap();
            batch.commit().unwrap();
        }

        let store = DiskIndexStore::open(&path, DiskIndexOptions::default()).unwrap();
        assert_eq!(store.read_metadata().unwrap().working.eof, 90);
        store
            .stage_restore(identity(), plan().mode(), 90, 4)
            .unwrap()
            .commit_after_native_sync(64)
            .unwrap();
        assert_eq!(store.read_metadata().unwrap().committed.eof, 64);
    }

    #[test]
    fn unexpected_savepoint_is_typed_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.vki");
        let store =
            DiskIndexStore::create(&path, DiskIndexOptions::default(), metadata(64)).unwrap();
        let transaction = begin_quick_immediate(&store.database).unwrap();
        transaction.persistent_savepoint().unwrap();
        transaction.commit().unwrap();

        assert!(matches!(
            store.validate_protocol(),
            Err(DiskIndexError::UnexpectedSavepoints {
                expected: None,
                actual_count: 1,
                ..
            })
        ));
    }

    #[test]
    fn savepoint_validation_short_circuits_instead_of_counting_all() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.vki");
        let store =
            DiskIndexStore::create(&path, DiskIndexOptions::default(), metadata(64)).unwrap();
        for _ in 0..3 {
            let transaction = begin_quick_immediate(&store.database).unwrap();
            transaction.persistent_savepoint().unwrap();
            transaction.commit().unwrap();
        }

        // Three persistent savepoints exist, but validation only needs to know
        // that more than the expected one is present: the reported count
        // saturates at two instead of enumerating a possibly hostile set.
        assert_eq!(persistent_savepoints(&store).len(), 3);
        match store.validate_protocol() {
            Err(DiskIndexError::UnexpectedSavepoints {
                expected: None,
                first_actual,
                actual_count,
            }) => {
                assert!(first_actual.is_some());
                assert_eq!(actual_count, 2);
            }
            other => panic!("expected saturated savepoint mismatch, got {other:?}"),
        }
    }

    #[test]
    fn on_disk_metadata_corruption_is_specific() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.vki");
        let store =
            DiskIndexStore::create(&path, DiskIndexOptions::default(), metadata(64)).unwrap();
        let transaction = begin_quick_immediate(&store.database).unwrap();
        {
            let mut table = transaction.open_table(META_TABLE).unwrap();
            let mut encoded = encode_metadata(metadata(64));
            encoded[96] ^= 1;
            table.insert(META_KEY, encoded.as_slice()).unwrap();
        }
        transaction.commit().unwrap();
        assert!(matches!(
            store.read_metadata(),
            Err(DiskIndexError::MetadataChecksum)
        ));
    }

    #[test]
    fn writer_rejects_untrusted_tail_bound_before_tail_table_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tail-bound.vki");
        let store =
            DiskIndexStore::create(&path, DiskIndexOptions::default(), metadata(64)).unwrap();
        let transaction = begin_quick_immediate(&store.database).unwrap();
        {
            let mut table = transaction.open_table(META_TABLE).unwrap();
            let mut encoded = encode_metadata(metadata(64));
            encoded[176..180].copy_from_slice(&u32::MAX.to_le_bytes());
            rewrite_crc(&mut encoded);
            table.insert(META_KEY, encoded.as_slice()).unwrap();
        }
        transaction.commit().unwrap();

        assert!(matches!(
            store.validate_clean_writer(identity(), plan().mode(), 64, 4),
            Err(DiskIndexError::TailLimitMismatch {
                expected: 4,
                actual: u32::MAX
            })
        ));
    }

    #[test]
    fn state_only_mode_rejects_key_tables_but_tracks_coverage() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.vks");
        let metadata = DiskIndexMetadata::new_state(identity(), DiskIndexFrontier::empty(64), 0);
        let store = DiskIndexStore::create(&path, DiskIndexOptions::default(), metadata).unwrap();
        store.begin_generation().unwrap();
        let mut batch = store.begin_write_batch().unwrap();
        assert!(matches!(
            batch.lookup(10, &9u64),
            Err(DiskIndexError::KeyTableUnavailable)
        ));
        batch.advance_coverage(64, 80, 0).unwrap();
        batch.commit().unwrap();
        store
            .publish_clean(DiskIndexFrontier::new(80, 1, Some(1)))
            .unwrap();
        assert!(matches!(
            store
                .begin_snapshot_with_mode(identity(), DiskIndexMode::StateOnly, 80)
                .unwrap()
                .lookup(10, &9u64),
            Err(DiskIndexError::KeyTableUnavailable)
        ));
    }

    #[test]
    fn bounded_batch_rejects_the_next_record_without_advancing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.vki");
        let options = DiskIndexOptions {
            batch: DiskIndexBatchOptions {
                max_records: 1,
                max_bytes: 4096,
            },
            max_key_bytes: 1024,
            ..DiskIndexOptions::default()
        };
        let store = DiskIndexStore::create(&path, options, metadata(64)).unwrap();
        store.begin_generation().unwrap();
        let mut batch = store.begin_write_batch().unwrap();
        batch.advance_coverage(64, 80, 0).unwrap();
        assert!(matches!(
            batch.advance_coverage(80, 96, 1),
            Err(DiskIndexError::BatchFull { .. })
        ));
        assert_eq!(
            batch.working_frontier(),
            DiskIndexFrontier::new(80, 1, Some(1))
        );
        batch.commit().unwrap();
    }

    #[test]
    fn reader_allows_physical_tail_but_writer_requires_exact_eof() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.vki");
        let store =
            DiskIndexStore::create(&path, DiskIndexOptions::default(), metadata(64)).unwrap();
        assert_eq!(
            store
                .begin_snapshot_with_plan(identity(), plan(), 100)
                .unwrap()
                .committed_eof(),
            64
        );
        assert!(matches!(
            store.validate_clean_writer(identity(), plan().mode(), 100, 4),
            Err(DiskIndexError::NativeLengthMismatch { .. })
        ));
        assert!(matches!(
            store.begin_snapshot_with_plan(identity(), plan(), 63),
            Err(DiskIndexError::NativeTooShort { .. })
        ));
    }

    #[test]
    fn rebuild_streams_into_a_clean_prepared_sidecar() {
        struct Source(VecDeque<DiskIndexRebuildEntry>);

        impl DiskIndexRebuildSource for Source {
            fn next_update(&mut self) -> crate::Result<Option<DiskIndexRebuildEntry>> {
                Ok(self.0.pop_front())
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let update = update(
            9,
            DiskIndexEntry::Put {
                record_offset: 64,
                sequence: 0,
            },
            physical(10, 36),
        );
        let mut source = Source(VecDeque::from([DiskIndexRebuildEntry {
            update,
            covered_eof: 100,
        }]));
        let prepared = build_replacement(
            directory.path(),
            DiskIndexOptions::default(),
            metadata(64),
            &mut source,
        )
        .unwrap();
        let store = DiskIndexStore::open(prepared.path(), DiskIndexOptions::default()).unwrap();
        assert_eq!(
            store
                .begin_snapshot_with_plan(identity(), plan(), 100)
                .unwrap()
                .lookup(10, &9u64)
                .unwrap(),
            DiskIndexEntry::Put {
                record_offset: 64,
                sequence: 0,
            }
        );
    }

    #[test]
    fn generation_exhaustion_is_typed_without_deleting_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.vki");
        let metadata = DiskIndexMetadata {
            generation: u64::MAX,
            ..metadata(64)
        };
        let store = DiskIndexStore::create(&path, DiskIndexOptions::default(), metadata).unwrap();
        store.begin_generation().unwrap();
        assert!(matches!(
            store.mark_clean(64),
            Err(DiskIndexError::GenerationExhausted)
        ));
        assert_eq!(store.read_metadata().unwrap().state, DiskIndexState::Dirty);
        assert_eq!(persistent_savepoints(&store).len(), 1);
    }
}
