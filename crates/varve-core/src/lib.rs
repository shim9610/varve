#![forbid(unsafe_op_in_unsafe_fn)]

mod adapter;
mod chunks;
mod codec;
mod collections;
mod diagnostics;
#[cfg(feature = "high-cardinality-dev")]
mod disk_index;
mod error;
mod file;
mod format;
#[cfg(feature = "high-cardinality-dev")]
mod indexed;
mod layout;
mod matrix;
mod merge;
mod native_layout;
#[cfg(all(test, feature = "high-cardinality-dev"))]
mod pib_probe;
mod scalable_extent;
#[cfg(feature = "scalable-fault-injection")]
#[doc(hidden)]
pub mod scalable_fault;
#[cfg(feature = "high-cardinality-dev")]
mod scan_control;
mod snapshot;
#[cfg(feature = "high-cardinality-dev")]
mod stream;
mod traits;
mod writer_permit;

/// Test-only window onto the crate's mechanical-enforcement types.
///
/// Round 12's mandate was that the two recurring defect shapes be made
/// impossible to express rather than enumerated again, and that the enforcement
/// be *proved* to bind. A proof that the compiler rejects the mistake has to
/// live in a crate that is not this one, because a compile-fail test cannot be
/// written inside the crate under test — so the enforcement types are exposed
/// here, unchanged and by re-export rather than by copy, for the `trybuild`
/// fixtures in `crates/varve/tests/ui/`.
///
/// This is not API. It is `#[doc(hidden)]`, gated behind the same test-only
/// feature as the fault injectors, and carries no method that can weaken an
/// invariant: the constructors that matter (`PoisonFlag::issue`, which is
/// private even to the rest of `varve-core`,
/// `ReservedIndexSlot::reserve`, `ReplacementTarget::resolve` and
/// `RecordOverwrite::prepare`) stay crate-private, which is precisely the
/// property the fixtures assert.
#[cfg(feature = "scalable-fault-injection")]
#[doc(hidden)]
pub mod enforcement_probe {
    pub use crate::file::replacement_target::{RecordOverwrite, ReplacementTarget};
    pub use crate::file::resident_index::{ReservedIndexSlot, ResidentIndex};
    pub use crate::matrix::crc_valid_evidence::{CompleteCrcValidEvidence, CrcValidEvidence};
    pub use crate::matrix::fatal_access::{FatalAccessAllowed, FatalAccessGate};
    pub use crate::writer_permit::{MutationInFlight, MutationPermit, PoisonFlag};
}

pub use adapter::{
    AdapterCheckReport, AdapterCheckStatus, AdapterDiagnostic, AdapterDiagnosticDomain,
    AdapterInputFile, AdapterTailStatus, BinaryCursor, BinaryWriter, ChunkEntry, ChunkIndex,
    ChunkIndexBuilder, ChunkIndexEntry, ChunkLayout, DEFAULT_SIDECAR_FINGERPRINT_SCAN_LIMIT,
    LengthPrefix, SegmentReducer, SegmentReductionReport, SidecarIdentity, SidecarMode,
    SidecarPolicy, SidecarReport, TaggedValue, TaggedValueCodec, reduce_segments,
    reduce_segments_by_ref,
};
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
#[cfg(feature = "high-cardinality-dev")]
pub use disk_index::{
    DiskIndexBatchOptions, DiskIndexDescriptor as DiskIndexedBlock, DiskIndexDigest,
    DiskIndexEntry, DiskIndexError, DiskIndexMode, DiskIndexOptions, DiskIndexPlan, VarveDiskKey,
    sidecar_path as disk_index_sidecar_path,
};
pub use error::{Error, Result};
pub use file::{
    AppendInfo, BlockEvent, COMMIT_BLOCK_ID, CREATION_NONCE_BLOCK_ID, FileMatrixDurabilityBarrier,
    INDEX_BLOCK_ID, KeyedMergeEstimate, MANIFEST_BLOCK_ID, METADATA_BLOCK_ID,
    MatrixDurabilityBarrier, MatrixNumeric, MatrixSidecarManifest, OP_BLOCK_ID, OpenMode,
    RecordIndexEntry, RecoveryReport, ReplacePublicationFailure, ReplaceStrategy, ReplacementInfo,
    SchemaBlockDescriptor, SchemaFieldDescriptor, SchemaManifest, TOMBSTONE_BLOCK_ID, VarveFile,
    VarveReader, VarveWriter, WriterLockBreakPolicy, WriterLockInfo,
    classify_replace_publication_error, clear_stale_writer_lock, compact_keyed_files,
    compact_keyed_files_with_key_limit, estimate_keyed_merge, merge_keyed_files,
    merge_keyed_files_with_key_limit,
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
    MatrixCommitKind, MatrixDimensionDescriptor, MatrixMetadataResidency, MetadataDescriptor,
    RawRegionDescriptor, ReadLimit, ReadLimits, RecoveryPolicy, ResourceLimits, SegmentDescriptor,
    SegmentRepeat, TransactionMarkerMode, VariableCompression,
};
#[cfg(feature = "high-cardinality-dev")]
pub use indexed::{
    DiskIndexRebuildReport, VarveIndexedReader, VarveIndexedWriter, rebuild_disk_index,
    rebuild_disk_index_with_progress,
};
pub use layout::{
    LayoutFieldValue, LayoutFileInfo, LayoutReader, LayoutScanReport, LayoutSegmentInfo,
    LayoutTailInfo, LayoutTailKind, LayoutValue, LayoutWriter, SegmentWrite, SegmentWriteStream,
};
pub use matrix::{
    MatrixCellStatus, MatrixCommitEvent, MatrixCorruptionKind, MatrixCorruptionSeverity,
    MatrixDimensionValue, MatrixDimensions, MatrixKey, MatrixRecoveryAction, MatrixRecoveryFinding,
    MatrixRecoveryReport, MatrixResumeSignal, PackedBitmap,
};
pub use merge::{
    MergeAction, SequencedMergeAction, VarveMerge, compact_keyed_file,
    compact_keyed_file_with_key_limit,
};
#[cfg(feature = "high-cardinality-dev")]
pub use scan_control::{
    ScanCancellationToken, ScanOptions, ScanProgress, ScanProgressOptions, ScanProgressPhase,
};
#[allow(unused_imports)]
pub(crate) use snapshot::SnapshotFile;
#[cfg(feature = "high-cardinality-dev")]
pub use stream::{
    BatchAppendError, BatchAppendInfo, BatchOptions, StreamBootstrapReport, StreamEvents,
    StreamOptions, StreamResidentState, StreamingBlocks, VarveStreamReader, VarveStreamWriter,
    bootstrap_stream_checkpoint, bootstrap_stream_checkpoint_with_progress,
};
pub use traits::{
    VarveBlock, VarveKey, VarveKeyedBlock, VarveMatrixBlock, VarveMigration, VarveReplaceBlock,
};
#[cfg(feature = "zero-copy")]
pub use traits::{VarveRawFixedBlock, VarveRawMatrixBlock};
#[cfg(feature = "zero-copy")]
pub use zerocopy;

#[inline(always)]
pub(crate) fn scalable_fault_point(point: &'static str) {
    #[cfg(feature = "scalable-fault-injection")]
    scalable_fault::fault_point(point);
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = point;
}
