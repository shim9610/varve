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

    #[error(
        "replacement generation with sequence {sequence} was published, but the writer could not rebind: {source}"
    )]
    PublishedButRebindFailed {
        sequence: u64,
        #[source]
        source: Box<Error>,
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
