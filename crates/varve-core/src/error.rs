use std::io;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid file magic")]
    InvalidMagic,

    #[error("unsupported container marker")]
    UnsupportedContainer,

    #[error("unsupported endian byte {0}")]
    UnsupportedEndian(u8),

    #[error("format endian mismatch: expected {expected:?}, got {actual:?}")]
    EndianMismatch {
        expected: crate::Endian,
        actual: crate::Endian,
    },

    #[error("payload ended before a full value could be decoded")]
    UnexpectedEof,

    /// A position-based read was asked of a handle that keeps no record
    /// directory.
    ///
    /// The handle was opened through `open_readonly_without_directory`, which
    /// hands the directory to the caller instead of keeping one. The reads are
    /// not gone — they need to be told where the directory is:
    /// `file.with_directory(&index).blocks::<T>()`.
    ///
    /// Deliberately an error and not an empty answer. An empty `BlockVec` here
    /// would be indistinguishable from an empty file, which is the one way a
    /// missing directory could corrupt a caller's conclusions rather than
    /// merely inconvenience them.
    #[error(
        "this handle keeps no record directory; pass the one you were given to          `with_directory(..)` and repeat the {operation} there"
    )]
    NoResidentDirectory { operation: &'static str },

    #[error("length value {value} does not fit on this platform")]
    LengthOverflow { value: u64 },

    #[error("string payload is not valid UTF-8")]
    InvalidUtf8,

    #[error("decoder left {remaining} trailing bytes")]
    TrailingBytes { remaining: usize },

    #[error("invalid canonical encoding: {0}")]
    InvalidCanonicalEncoding(&'static str),

    #[error("{resource} length {actual} exceeds limit {limit}")]
    LimitExceeded {
        resource: &'static str,
        actual: u64,
        limit: u64,
    },

    #[error("missing finite read limit for {resource}")]
    MissingResourceLimit { resource: &'static str },

    #[error("trusted-unbounded {resource} access requires a named trusted-unbounded API")]
    TrustedUnboundedRequiresExplicitApi { resource: &'static str },

    #[error("{resource} arithmetic overflow")]
    ResourceArithmeticOverflow { resource: &'static str },

    #[error("failed to reserve {requested} bytes for {resource}")]
    AllocationFailed {
        resource: &'static str,
        requested: u64,
    },

    #[error(
        "snapshot range is out of bounds: offset {offset}, len {len}, snapshot_len {snapshot_len}"
    )]
    SnapshotRangeOutOfBounds {
        offset: u64,
        len: u64,
        snapshot_len: u64,
    },

    #[error("missing required field {field} ({field_id})")]
    MissingField { field: &'static str, field_id: u32 },

    #[error("wire type mismatch for field {field}: expected {expected:?}, got {actual:?}")]
    WireTypeMismatch {
        field: &'static str,
        expected: crate::WireType,
        actual: crate::WireType,
    },

    #[error("unknown wire type {0}")]
    UnknownWireType(u16),

    #[error("record at offset {offset} has a corrupt or incomplete tail")]
    CorruptTail { offset: u64 },

    #[error("block id {0} is reserved by varve")]
    ReservedBlockId(u32),

    #[error("block id {0} is not registered in this format")]
    UnregisteredBlock(u32),

    #[error("invalid format spec: {0}")]
    InvalidFormatSpec(&'static str),

    #[error("invalid or stale index checkpoint")]
    InvalidIndexCheckpoint,

    /// A segment record did not describe the file it was found in.
    ///
    /// Never reaches a caller through `open`: the segment chain is derived, so
    /// open answers this by falling back to the full record scan. It exists so
    /// the walk can say *why* it stopped, and so a diagnostic tool that reads a
    /// segment directly gets more than a bare `false`.
    #[error("invalid or stale index segment")]
    InvalidIndexSegment,

    /// A read that resolves through the resident index was asked for a block
    /// declared non-resident.
    ///
    /// The loud half of the residency trade. An empty collection would be
    /// indistinguishable from "nothing was ever written", so a caller who opted
    /// a block out and then read it through the wrong door would see their data
    /// as missing rather than as unreachable by that door.
    #[error("block {block_id} is declared non-resident; read it through the block offset chain")]
    BlockNotResident { block_id: u32 },

    #[error("block version mismatch for block {block_id}: expected {expected}, got {actual}")]
    BlockVersionMismatch {
        block_id: u32,
        expected: u16,
        actual: u16,
    },

    #[error("block kind mismatch: expected {expected:?}, got {actual:?}")]
    BlockKindMismatch {
        expected: crate::BlockKind,
        actual: crate::BlockKind,
    },

    #[error(
        "block {block_id} schema fingerprint mismatch: registered {registered:#018x}, declared {declared:#018x}"
    )]
    BlockSchemaFingerprintMismatch {
        block_id: u32,
        registered: u64,
        declared: u64,
    },

    #[error(
        "block {block_id} keyedness mismatch: registered keyed={registered}, declared keyed={declared}"
    )]
    BlockKeyednessMismatch {
        block_id: u32,
        registered: bool,
        declared: bool,
    },

    #[error("replace payload length changed from {old} to {new}")]
    ReplaceSizeMismatch { old: u64, new: u64 },

    #[error("replacement changed the block key")]
    ReplacementKeyMismatch,

    #[error("checksum requested, but the integrity feature is not enabled")]
    IntegrityFeatureDisabled,

    #[error("checksum mismatch at offset {offset}")]
    ChecksumMismatch { offset: u64 },

    #[error("compression requested, but the compression-zstd feature is not enabled")]
    CompressionFeatureDisabled,

    #[error("unsupported compression algorithm {0}")]
    UnsupportedCompressionAlgorithm(u8),

    #[error("invalid compression header")]
    InvalidCompressionHeader,

    #[error("invalid chunked bytes envelope")]
    InvalidChunkedBytes,

    #[error(
        "chunk checksum mismatch for chunk {chunk_index}: expected {expected:#010x}, got {actual:#010x}"
    )]
    ChunkChecksumMismatch {
        chunk_index: u32,
        expected: u32,
        actual: u32,
    },

    #[error("unknown record flag bits {0:#06x}")]
    UnknownRecordFlags(u16),

    #[error("invalid record footer at offset {offset}")]
    InvalidRecordFooter { offset: u64 },

    #[error("invalid commit marker at offset {offset}")]
    InvalidCommitMarker { offset: u64 },

    #[error("decompressed length mismatch: expected {expected}, got {actual}")]
    DecompressedLengthMismatch { expected: u64, actual: u64 },

    #[error("decompressed length {actual} exceeds limit {limit}")]
    DecompressedLengthLimitExceeded { actual: u64, limit: u64 },

    #[error("format schema hash mismatch: expected {expected}, got {actual}")]
    SchemaHashMismatch { expected: u64, actual: u64 },

    #[error("format version mismatch: expected {expected}, got {actual}")]
    FormatVersionMismatch { expected: u16, actual: u16 },

    #[error("writer lock is already held for {0}")]
    WriterLockHeld(String),

    #[error("writer lock metadata is malformed for {0}")]
    WriterLockMalformed(String),

    #[error("writer lock break was refused for {0}")]
    WriterLockBreakRefused(String),

    /// The diagnostic `<target>.lock` marker path does not name a dedicated,
    /// unaliased regular file, so Varve refuses to truncate or write it (F-07).
    ///
    /// The marker carries pid/timestamp diagnostics and the break-policy
    /// machinery; acquiring it truncates and rewrites the object it names.
    /// A pre-placed symbolic link, or a hard link from the marker path to an
    /// unrelated empty file, would therefore route those writes at a foreign
    /// object. Authoritative single-writer exclusion never depended on the
    /// marker - it is held on the target file object itself - so refusing here
    /// costs no exclusion strength.
    #[error("writer lock marker at {path} is not a dedicated regular file ({reason})")]
    WriterLockMarkerNotDedicated { path: String, reason: &'static str },

    #[error("append record sequence is exhausted")]
    SequenceExhausted,

    #[error("{0} writer is poisoned after an uncertain write failure")]
    WriterPoisoned(&'static str),

    #[error(
        "block {block_id} is keyed and this format chains keyed offsets, but the generic append \
         cannot maintain the predecessor chain; use the generated keyed writer method or \
         VarveFile::push_keyed_info"
    )]
    KeyedChainRequiresKeyedApi { block_id: u32 },

    #[cfg(feature = "high-cardinality-dev")]
    #[error("the requested operation is unsupported by bounded streaming handles")]
    StreamingUnsupported,

    #[cfg(feature = "high-cardinality-dev")]
    #[error("batch option {field} must be greater than zero")]
    InvalidBatchOptions { field: &'static str },

    #[cfg(feature = "high-cardinality-dev")]
    #[error(
        "explicit scan cancelled after {records} records and {bytes} record-region bytes",
        records = .progress.records,
        bytes = .progress.scanned_bytes,
    )]
    ScanCancelled { progress: crate::ScanProgress },

    #[cfg(feature = "high-cardinality-dev")]
    #[error("the derived disk index is busy")]
    IndexBusy,

    #[cfg(feature = "high-cardinality-dev")]
    #[error("disk index error: {0}")]
    DiskIndex(#[source] Box<crate::disk_index::DiskIndexError>),

    #[cfg(feature = "high-cardinality-dev")]
    #[error("record sequence {sequence} was published, but the disk index is stale: {source}")]
    PublishedButIndexStale {
        sequence: u64,
        #[source]
        source: Box<Error>,
    },

    #[error("failed to roll back {operation}: {source}")]
    WriteRollbackFailed {
        operation: &'static str,
        #[source]
        source: io::Error,
    },

    /// The replacement generation was published, but the writer could not
    /// rebind to it and was poisoned.
    ///
    /// `parent_sync` carries the *second* durability fact when two independent
    /// failures happen in the same publication (F-07). The parent-directory
    /// sync runs first, immediately after the atomic rename; when it fails the
    /// publication is still real, so the writer rebinds anyway and the caller
    /// normally learns about the pending sync through
    /// [`Error::PublishedButParentSyncPending`]. If the rebind then fails too,
    /// that variant is never constructed, and this one is returned instead —
    /// so without this field the already-known fact that the published
    /// pathname is not yet proved durable against power loss would be dropped
    /// on the floor. `Some(_)` therefore means: the new generation is visible
    /// at the pathname, the writer is unusable, **and** the rename is not yet
    /// guaranteed to survive power loss. `None` means only the first two.
    #[error(
        "replacement generation with sequence {sequence} was published, but the writer could not \
         rebind: {source}{parent_sync_note}",
        parent_sync_note = .parent_sync
            .as_ref()
            .map(|error| format!(
                "; the parent-directory sync for the published pathname also failed, so the \
                 publication is not yet known durable against power loss: {error}"
            ))
            .unwrap_or_default(),
    )]
    PublishedButRebindFailed {
        sequence: u64,
        #[source]
        source: Box<Error>,
        /// The parent-directory sync failure that preceded the rebind failure,
        /// when both happened in the same publication.
        parent_sync: Option<Box<Error>>,
    },

    /// The commit marker was appended, but the durability request that follows
    /// it failed (round 10, invariant 3).
    ///
    /// This is a *published* outcome in the same family as
    /// [`Error::PublishedButRebindFailed`] and
    /// [`Error::MatrixCommittedButHookFailed`]. A caller that receives it must
    /// not treat the commit as not-performed: the marker bytes are in the file
    /// and a reader that opens the file after a clean process exit sees the
    /// transaction as committed. What is *not* established is that the bytes
    /// survive a power loss. The correct response is to retry
    /// [`crate::VarveFile::sync`], not to re-run the transaction — re-running
    /// it appends a second marker for work that is already recorded.
    #[error(
        "commit marker with sequence {sequence} was appended, but durability could not be \
         established: {source}"
    )]
    CommittedButDurabilityUnproven {
        sequence: u64,
        #[source]
        source: Box<Error>,
    },

    #[error(
        "replacement was published at {path}, but parent-directory durability is not yet confirmed: {source}"
    )]
    PublishedButParentSyncPending {
        path: String,
        #[source]
        source: Box<Error>,
    },

    #[error(
        "replacement publication at {path} failed in an indeterminate state: the OS reports the \
         file names may be partially moved; the replacement file was preserved at {replacement} \
         and the writer is unusable until the pathname is reconciled out of band: {source}"
    )]
    ReplacePublicationIndeterminate {
        path: String,
        replacement: String,
        #[source]
        source: io::Error,
    },

    #[error("merge op referenced a missing target")]
    MissingMergeTarget,

    #[error("migration block id mismatch: from {from}, to {to}")]
    MigrationBlockIdMismatch { from: u32, to: u32 },

    #[error("invalid embedded schema manifest")]
    InvalidSchemaManifest,

    #[error("mmap payload entry is not part of the mapped snapshot")]
    MmapEntryNotInSnapshot,

    #[error("mmap payload window is out of bounds: offset {offset}, len {len}")]
    MmapPayloadOutOfBounds { offset: u64, len: u64 },

    #[error("zero-copy raw block kind mismatch: got {actual:?}")]
    ZeroCopyBlockKindMismatch { actual: crate::BlockKind },

    #[error("zero-copy raw endian mismatch: expected {expected:?}, got {actual:?}")]
    ZeroCopyEndianMismatch {
        expected: crate::Endian,
        actual: crate::Endian,
    },

    #[error("zero-copy raw payload length mismatch: expected {expected}, got {actual}")]
    ZeroCopyPayloadSizeMismatch { expected: usize, actual: u64 },

    #[error("zero-copy raw payload alignment mismatch: required {required}, address {address}")]
    ZeroCopyAlignmentMismatch { required: usize, address: usize },

    #[error("matrix dimensions are required for this format")]
    MatrixDimensionsRequired,

    #[error("matrix layout is missing")]
    MatrixLayoutMissing,

    #[error("matrix dimension {0} is missing")]
    MatrixDimensionMissing(String),

    #[error("matrix dimension {name} mismatch: expected {expected}, got {actual}")]
    MatrixDimensionMismatch {
        name: String,
        expected: u64,
        actual: u64,
    },

    #[error("matrix block {0} is missing from the layout")]
    MatrixBlockMissing(u32),

    #[error("matrix commit category {0} is missing from the layout")]
    MatrixCommitMissing(String),

    #[error("matrix commit category {0} is quarantined after integrity failure")]
    MatrixCommitQuarantined(String),

    #[error(
        "matrix recovery report contains fatal findings; default access is fail-closed, use FormatSpec::with_matrix_fatal_forensics for forensic access"
    )]
    MatrixFatalCorruption,

    /// The durable matrix write completed and the cell is committed and synced;
    /// only the caller's post-commit notification hook failed (F-04).
    ///
    /// This is a *published* outcome in the same family as
    /// [`Error::PublishedButParentSyncPending`] and
    /// [`Error::PublishedButRebindFailed`]: a caller that receives it must not
    /// treat the write as not-performed. Re-running
    /// [`crate::VarveFile::write_matrix_cell_durable`] would repeat the write
    /// and re-run the hook, so external work driven by the hook can be
    /// duplicated. The committed event is carried so the caller can retry the
    /// notification alone.
    #[error(
        "matrix cell (scan {scan}, ch {ch}) for block {block_id} was committed and made durable, \
         but the post-commit notification hook failed: {source}",
        block_id = .event.block_id,
        scan = .event.key.scan,
        ch = .event.key.ch,
    )]
    MatrixCommittedButHookFailed {
        // Boxed to keep `Error` (and every `Result` in the crate) inside the
        // `clippy::result_large_err` budget; the inline event would add 40
        // bytes to a type returned from every fallible function.
        event: Box<crate::MatrixCommitEvent>,
        #[source]
        source: Box<Error>,
    },

    /// The matrix commit bit reached the file, but the durability request that
    /// follows it failed (round 11, invariant 3).
    ///
    /// This is the matrix twin of [`Error::CommittedButDurabilityUnproven`] and
    /// sits in the same published-outcome family as
    /// [`Error::MatrixCommittedButHookFailed`]: the cell **is** committed, and
    /// a reader that opens the file after a clean process exit sees it as
    /// committed. What is *not* established is that the commit survives a power
    /// loss. The post-commit hook was **not** run, so no notification was
    /// emitted for this cell.
    ///
    /// The writer is poisoned when this is returned, because the failure is a
    /// durability request the operating system refused mid-publication. Recover
    /// by reopening the file and calling [`crate::VarveFile::sync`]; the cell
    /// itself does not need to be rewritten, and the carried event is the one
    /// the hook would have been given, so the notification can be issued once
    /// durability is re-established.
    #[error(
        "matrix cell (scan {scan}, ch {ch}) for block {block_id} was committed, but the durability \
         request after the commit failed: {source}",
        block_id = .event.block_id,
        scan = .event.key.scan,
        ch = .event.key.ch,
    )]
    MatrixCommittedButDurabilityUnproven {
        // Boxed for the same `clippy::result_large_err` reason as above.
        event: Box<crate::MatrixCommitEvent>,
        #[source]
        source: Box<Error>,
    },

    #[error("matrix key is out of bounds: scan {scan}, ch {ch}")]
    MatrixKeyOutOfBounds { scan: u64, ch: u64 },

    #[error("matrix aux region {0} is missing from the layout")]
    MatrixAuxMissing(String),

    #[error(
        "matrix aux region {name} range is out of bounds: offset {offset}, len {len}, byte_len {byte_len}"
    )]
    MatrixAuxOutOfBounds {
        name: String,
        offset: u64,
        len: u64,
        byte_len: u64,
    },

    #[error("matrix payload size mismatch: expected {expected}, got {actual}")]
    MatrixSizeMismatch { expected: u64, actual: u64 },

    #[error(
        "matrix numeric view is out of bounds: offset {offset}, len {len}, payload_len {payload_len}"
    )]
    MatrixNumericOutOfBounds {
        offset: u64,
        len: u64,
        payload_len: u64,
    },

    #[error("matrix cell is not committed")]
    MatrixNotCommitted,

    #[error("matrix cell was not written before commit")]
    MatrixCellNotWritten,

    /// A lazy writer open was asked for on a spec that also writes an index
    /// checkpoint or an index segment at each commit point.
    ///
    /// Both records serialize the resident index — a checkpoint writes the
    /// whole of it, a segment one entry per record it covers — and a lazy
    /// writer has no resident index to serialize. The refusal is up front
    /// rather than a silent no-op because a format that declared either one
    /// asked for a specific recovery shape, and quietly not writing it would
    /// leave files that open slower than their spec says they should.
    ///
    /// A segment and a digest solve the same problem: what an open needs that
    /// lives in no single record. Pick one. The table in
    /// `VarveFile::open_readonly_lazy` measures both.
    #[error(
        "a lazy writer open cannot serve {policy}, which serializes the resident index it does \
         not keep; a digest and an index segment answer the same question, so declare one"
    )]
    LazyWriterIndexPolicy { policy: &'static str },

    /// A write or commit addressed a chunk that is no longer the open one.
    ///
    /// **Nothing in the crate constructs this any more, and that is deliberate
    /// rather than an oversight.** One chunk is buffered at a time, which is
    /// what bounds a growing matrix's memory, but *which* chunk is not fixed: a
    /// write to a row belonging to an already-written chunk writes the open one
    /// out and loads that chunk back. Every path that used to refuse now
    /// reloads, and the cases where a reload is impossible say so by name
    /// through [`Error::MatrixChunkNotReopenable`] instead of through this.
    ///
    /// It is kept because the enum is `#[non_exhaustive]` in the other
    /// direction only — removing a variant breaks a caller matching on it, and
    /// a caller who wrote that arm against 0.5.0 should get a dead arm rather
    /// than a compile error. `open` was the chunk a caller could still write
    /// to, or `None` when there was none.
    ///
    /// **Closed, not necessarily written.** This was `MatrixChunkSealed`, and
    /// both the name and the message claimed the chunk had been written out as
    /// a record. That is true only when it held a committed cell: a chunk the
    /// writer moved past with nothing committed is dropped without a record
    /// ever existing, and the old wording sent anyone debugging that case
    /// looking through the file for a record that was never there. What the
    /// refusal actually means is that this chunk is closed to writes.
    #[error("matrix chunk {chunk} is closed{}", match open {
        Some(open) => format!("; the open chunk is {open}"),
        None => String::from("; no chunk is open"),
    })]
    MatrixChunkClosed { chunk: u64, open: Option<u64> },

    /// A write addressed an already-written chunk that cannot be loaded back.
    ///
    /// Reopening a written chunk rewrites its record where it already sits,
    /// which needs the rewritten payload to be exactly as long as the one on
    /// disk. Two format options break that, and each names itself here rather
    /// than arriving as a generic refusal:
    ///
    /// * `"chunk compression"` — a compressed chunk's payload length is a
    ///   function of its *contents*, so a changed cell changes the length and
    ///   the record no longer fits its slot. Nothing is lost by refusing: the
    ///   option is off by default, and a format that does not set it reopens.
    /// * `"segment_on_flush"` — the same reason
    ///   [`crate::VarveFile::replace_fixed`] refuses in-place replacement for
    ///   these formats.
    ///
    /// Both are refusals to *modify*; reads of the chunk are unaffected.
    #[error("matrix chunk {chunk} cannot be reopened: {reason}")]
    MatrixChunkNotReopenable { chunk: u64, reason: &'static str },

    #[error("invalid matrix chunk record")]
    InvalidMatrixChunk,

    #[error("invalid matrix sidecar envelope")]
    InvalidMatrixSidecar,

    #[error("matrix sidecar does not match parent: {0}")]
    MatrixSidecarMismatch(&'static str),

    #[error("matrix sidecar checksum mismatch: expected {expected:#010x}, got {actual:#010x}")]
    MatrixSidecarChecksumMismatch { expected: u32, actual: u32 },

    #[error(
        "matrix checksum mismatch at offset {offset}: expected {expected:#010x}, got {actual:#010x}"
    )]
    MatrixChecksumMismatch {
        offset: u64,
        expected: u32,
        actual: u32,
    },

    #[error("invalid matrix layout")]
    InvalidMatrixLayout,

    #[error("layout field {0} is missing")]
    LayoutFieldMissing(&'static str),

    #[error("layout field {0} was not expected")]
    LayoutFieldUnexpected(String),

    #[error("layout field {0} has the wrong value type")]
    LayoutFieldTypeMismatch(&'static str),

    #[error("layout literal mismatch for field {field} at offset {offset}")]
    LayoutLiteralMismatch { field: &'static str, offset: u64 },

    #[error("layout lead-in is truncated at offset {offset}")]
    LayoutTruncatedLeadIn { offset: u64 },

    #[error("layout file header is truncated at offset {offset}")]
    LayoutTruncatedHeader { offset: u64 },

    #[error("layout segment bounds are invalid at offset {offset}")]
    LayoutInvalidSegmentBounds { offset: u64 },

    #[error("layout segment {0} is not declared")]
    LayoutSegmentMissing(String),

    #[error("layout segment dispatch is ambiguous at offset {offset}")]
    LayoutAmbiguousSegment { offset: u64 },

    #[error("no layout segment descriptor matched at offset {offset}")]
    LayoutNoMatchingSegment { offset: u64 },

    #[error("layout segment {segment} is repeat once but appears again at offset {offset}")]
    LayoutRepeatedOnceSegment { segment: String, offset: u64 },

    #[error("layout segment {segment} index {index} is out of bounds")]
    LayoutSegmentIndexOutOfBounds { segment: String, index: usize },

    #[error(
        "adapter byte range is out of bounds: offset {offset}, len {len}, available {available}"
    )]
    AdapterBounds {
        offset: u64,
        len: u64,
        available: u64,
    },

    #[error("adapter unsupported tagged value type {type_id}")]
    AdapterUnsupportedType { type_id: u64 },

    #[error("adapter invalid length {value}")]
    AdapterInvalidLength { value: u64 },

    #[error("invalid adapter file extension {0:?}")]
    InvalidAdapterExtension(String),

    #[error("adapter diagnostic: {0}")]
    AdapterDiagnostic(&'static str),
}

impl Error {
    /// Folds an already-known parent-directory sync failure into a published
    /// outcome that would otherwise drop it (F-07).
    ///
    /// A replacement publication learns two independent durability facts in a
    /// fixed order: whether the parent-directory sync succeeded, and whether
    /// the writer could rebind to the published generation. Only the second
    /// one has a `?`-style path out, so a double fault used to return
    /// [`Error::PublishedButRebindFailed`] alone and silently discard the
    /// first. Applying this to the rebind error preserves both.
    ///
    /// Any other error is returned unchanged: the parent-sync fact is only
    /// meaningful alongside an outcome that already says the publication
    /// happened, and every other error from a replacement path means it did
    /// not.
    #[must_use]
    pub(crate) fn with_pending_parent_sync(self, parent_sync: Error) -> Error {
        match self {
            Error::PublishedButRebindFailed {
                sequence,
                source,
                parent_sync: None,
            } => Error::PublishedButRebindFailed {
                sequence,
                source,
                parent_sync: Some(Box::new(parent_sync)),
            },
            other => other,
        }
    }
}
