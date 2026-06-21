use std::io;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
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

    #[error("replace payload length changed from {old} to {new}")]
    ReplaceSizeMismatch { old: u64, new: u64 },

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

    #[error("adapter diagnostic: {0}")]
    AdapterDiagnostic(&'static str),
}
