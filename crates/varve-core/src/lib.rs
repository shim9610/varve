mod chunks;
mod codec;
mod collections;
mod diagnostics;
mod error;
mod file;
mod format;
mod layout;
mod matrix;
mod merge;
mod traits;

pub use chunks::ChunkedBytes;
pub use codec::{
    Decoder, Encoder, FieldHeader, VarveDecode, VarveEncode, WireType, decode_from_slice,
    encode_to_vec, read_field_header, write_field,
};
pub use collections::{BlockIter, BlockVec, KeyedBlockVec};
pub use diagnostics::{
    Diagnostic, DiagnosticDomain, DiagnosticSeverity, FormatDiagnostics, FormatSelfTest,
    FormatSelfTestReport, SelfTestStepReport, SelfTestStepStatus, classify_error, diagnose_file,
    diagnose_spec, error_hint,
};
pub use error::{Error, Result};
pub use file::{
    AppendInfo, BlockEvent, COMMIT_BLOCK_ID, FileMatrixDurabilityBarrier, INDEX_BLOCK_ID,
    MANIFEST_BLOCK_ID, METADATA_BLOCK_ID, MatrixDurabilityBarrier, MatrixNumeric,
    MatrixSidecarManifest, OP_BLOCK_ID, OpenMode, RecordIndexEntry, RecoveryReport,
    ReplaceStrategy, SchemaBlockDescriptor, SchemaFieldDescriptor, SchemaManifest,
    TOMBSTONE_BLOCK_ID, VarveFile, VarveReader, VarveWriter, WriterLockBreakPolicy, WriterLockInfo,
    compact_keyed_files, merge_keyed_files,
};
#[cfg(feature = "mmap")]
pub use file::{MmapMatrix, MmapPayloads};
pub use format::{
    BlockCompressionDescriptor, BlockDescriptor, BlockKind, CommitPolicy, CompressionAlgorithm,
    CompressionHeaderMode, CompressionLevel, CompressionPolicy, Endian, FieldDescriptor,
    FieldPresence, FileHeaderDescriptor, FooterDescriptor, FormatSpec, FormatSpecBuilder,
    IndexPolicy, IntegrityPolicy, LayoutAnchor, LayoutBytesSource, LayoutFieldDescriptor,
    LayoutFieldSource, LayoutFieldType, LayoutFinalize, LayoutPartDescriptor, LayoutPartKind,
    LayoutPlan, LayoutPlanField, LayoutPlanFieldGroup, LayoutPlanFieldSource, LayoutPlanFieldType,
    LayoutPlanLen, LayoutPlanPartDescriptor, LayoutPlanPartKind, LayoutPlanRegion,
    LayoutPlanRegionSource, LayoutPlanSegment, LayoutPreset, LayoutSpec, LeadInDescriptor,
    ManifestPolicy, MatrixAuxDescriptor, MatrixBlockDescriptor, MatrixCommitDescriptor,
    MatrixCommitKind, MatrixDimensionDescriptor, MetadataDescriptor, RawRegionDescriptor,
    RecoveryPolicy, SegmentDescriptor, SegmentRepeat, TransactionMarkerMode, VariableCompression,
};
pub use layout::{
    LayoutFieldValue, LayoutReader, LayoutSegmentInfo, LayoutValue, LayoutWriter, SegmentWrite,
};
pub use matrix::{
    MatrixCellStatus, MatrixCommitEvent, MatrixCorruptionKind, MatrixCorruptionSeverity,
    MatrixDimensionValue, MatrixDimensions, MatrixKey, MatrixRecoveryAction, MatrixRecoveryFinding,
    MatrixRecoveryReport, MatrixResumeSignal, PackedBitmap,
};
pub use merge::{MergeAction, SequencedMergeAction, VarveMerge, compact_keyed_file};
pub use traits::{VarveBlock, VarveKey, VarveKeyedBlock, VarveMatrixBlock, VarveMigration};
#[cfg(feature = "zero-copy")]
pub use traits::{VarveRawFixedBlock, VarveRawMatrixBlock};
#[cfg(feature = "zero-copy")]
pub use zerocopy;
