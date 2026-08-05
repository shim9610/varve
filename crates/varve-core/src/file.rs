use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions, remove_file};
use std::hash::Hash;
use std::io::{Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{
    BlockDescriptor, BlockKind, BlockVec, CommitPolicy, CompressionAlgorithm,
    CompressionHeaderMode, CompressionLevel, CompressionPolicy, Endian, Error, FormatSpec,
    IndexPolicy, IntegrityPolicy, IntegrityVerification, KeyedBlockVec, ManifestPolicy,
    MatrixCellStatus, MatrixCommitEvent, MatrixDimensions, MatrixKey, MatrixRecoveryAction,
    MatrixRecoveryReport, MatrixResumeSignal, RecoveryPolicy, Result, SnapshotFile,
    VariableCompression, VarveBlock, VarveEncode, VarveKeyedBlock, VarveMatrixBlock, VarveMerge,
    VarveMigration, VarveReplaceBlock, WireType,
    codec::encode_to_vec_limited,
    collections::MaterializationBudget,
    format::ReadLimitKey,
    native_layout::{
        decode_native_internal_key_envelope, decode_native_internal_op_envelope,
        decode_native_record_footer, encode_native_record_footer, encode_native_record_header,
        native_file_header_len, native_record_footer_len, native_record_header_len,
        read_native_file_header, read_native_record_header, write_native_file_header,
        write_native_record_header,
    },
    writer_permit::{GuardedWriter, MutationPermit, PoisonFlag},
};

pub const TOMBSTONE_BLOCK_ID: u32 = 0xFFFF_FFFE;
pub const OP_BLOCK_ID: u32 = 0xFFFF_FFFD;
pub const METADATA_BLOCK_ID: u32 = 0xFFFF_FFFC;
pub const INDEX_BLOCK_ID: u32 = 0xFFFF_FFFB;
pub const MANIFEST_BLOCK_ID: u32 = 0xFFFF_FFFA;
pub const COMMIT_BLOCK_ID: u32 = 0xFFFF_FFF9;
/// Carries the per-create nonce of a stream/indexed primary (STO-01).
pub const CREATION_NONCE_BLOCK_ID: u32 = 0xFFFF_FFF8;
/// Carries one internal segment: the records a single commit point added.
///
/// A segment is varve's own lookup unit and has no declaration surface. See
/// [`IndexPolicy::segment_on_flush`].
pub const SEGMENT_BLOCK_ID: u32 = 0xFFFF_FFF7;
const RESERVED_BLOCK_ID_START: u32 = 0xFFFF_FF00;
pub(crate) const RECORD_HEADER_LEN: u64 = 32;
pub(crate) const RECORD_FOOTER_LEN: u64 = 32;
const RECORD_FLAG_COMPRESSED: u16 = 0x0001;
pub(crate) const RECORD_FLAG_INTERNAL: u16 = 0x8000;
const RECORD_KNOWN_FLAGS: u16 = RECORD_FLAG_COMPRESSED | RECORD_FLAG_INTERNAL;
pub(crate) const RECORD_FOOTER_MAGIC: &[u8; 4] = b"VRF1";
pub(crate) const RECORD_FOOTER_VERSION: u16 = 1;
pub(crate) const RECORD_FOOTER_FLAG_PREV_SAME_BLOCK: u16 = 0x0001;
pub(crate) const RECORD_FOOTER_FLAG_PREV_SAME_KEY: u16 = 0x0002;
pub(crate) const RECORD_FOOTER_KNOWN_FLAGS: u16 =
    RECORD_FOOTER_FLAG_PREV_SAME_BLOCK | RECORD_FOOTER_FLAG_PREV_SAME_KEY;
const COMMIT_PAYLOAD_MAGIC: &[u8; 4] = b"VCMT";
const INDEX_CHECKPOINT_MAGIC: &[u8; 4] = b"VIDX";
const INDEX_CHECKPOINT_VERSION: u16 = 3;
const SEGMENT_MAGIC: &[u8; 4] = b"VSEG";
const SEGMENT_VERSION: u16 = 1;
/// magic(4) + version(2) + flags(2) + covered_start(8) + entry_count(8)
/// + preceding_records(8).
const SEGMENT_PREFIX_LEN: u64 = 4 + 2 + 2 + 8 + 8 + 8;
/// The segment record's own start offset, written at the end of its payload.
///
/// Open arrives at EOF holding a footer and nothing else: the footer carries
/// no self offset and the header that would give one is `payload_len` bytes
/// further back, a distance only the header states. This trailer is the one
/// value that closes that circle, and it costs eight bytes of payload rather
/// than a byte of format.
const SEGMENT_TRAILER_LEN: u64 = 8;
/// One serialized index entry, in the layout `read_index_entry_payload`
/// decodes at checkpoint version 3.
const SEGMENT_ENTRY_LEN: u64 = 4 + 2 + 2 + 8 + 8 + 8 + 8 + 4 + 4 + 8 + 8 + 8 + 1;
/// Carries one matrix chunk: `rows_per_chunk` rows of every matrix block.
///
/// A chunk is what makes a matrix dimension grow past the extent declared at
/// create. Chunk 0 *is* the matrix region; chunk `k > 0` is a record with the
/// region's layout. See [`GrowingMatrixDimension`].
pub const MATRIX_CHUNK_BLOCK_ID: u32 = 0xFFFF_FFF6;
const MATRIX_CHUNK_MAGIC: &[u8; 4] = b"VMCK";
const MATRIX_CHUNK_VERSION: u16 = 1;
/// Prefix flag: every block region is `commit map | crc table | slots`.
///
/// Set when the format's integrity policy is a crc32 one. A chunk read
/// otherwise has nothing to check: the record footer's crc covers the whole
/// payload and is verified on a *record* read, which a positional cell read is
/// not, so a single flipped bit in a sealed chunk came back as data. The matrix
/// region solves this with a per-cell checksum and so does a chunk.
const MATRIX_CHUNK_FLAG_CELL_CRC: u16 = 0x0001;
/// Prefix flag: every block's slot region is a sub-block index followed by
/// compressed sub-blocks, rather than plain cells.
///
/// A chunk is written whole, so an unfilled one costs its full size — measured
/// identical at 5% and 100% filled. Only the slot regions are compressed: the
/// prefix, the descriptors, the commit maps and the checksum tables stay plain,
/// so addressing a cell, reading its commit bit and checking its checksum are
/// unchanged, and only the `stride`-byte positional read becomes a sub-block
/// decode.
const MATRIX_CHUNK_FLAG_COMPRESSED_SLOTS: u16 = 0x0002;
const MATRIX_CHUNK_KNOWN_FLAGS: u16 =
    MATRIX_CHUNK_FLAG_CELL_CRC | MATRIX_CHUNK_FLAG_COMPRESSED_SLOTS;
/// Uncompressed bytes a slot sub-block covers, before rounding to whole cells.
///
/// A cell's sub-block is `ordinal / cells_per_sub_block(stride)` — arithmetic,
/// not a search — and both sides derive the divisor from `stride` alone so they
/// cannot disagree.
const MATRIX_CHUNK_SUB_BLOCK_BYTES: u64 = 8192;
/// One `u32` offset per sub-block, plus a terminator, so a sub-block's extent
/// is two reads and no prefix sum.
const MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN: u64 = 4;
/// Bytes of stored checksum per cell.
const MATRIX_CHUNK_CRC_LEN: u64 = 4;
/// magic(4) + version(2) + flags(2) + chunk_index(8) + first_row(8) + rows(8)
/// + block_count(4) + reserved(4).
const MATRIX_CHUNK_PREFIX_LEN: u64 = 4 + 2 + 2 + 8 + 8 + 8 + 4 + 4;
/// block_id(4) + reserved(4) + stride(8) + cells(8) + commit_len(8).
///
/// Every block's descriptor precedes every block's data, so a reader computes
/// any block's slot offset from the descriptors alone — one small positional
/// read, whatever the chunk's size. That is what keeps a cell read from
/// materialising the chunk.
const MATRIX_CHUNK_BLOCK_DESC_LEN: u64 = 4 + 4 + 8 + 8 + 8;
/// Descriptor bytes a cell read keeps on the stack.
///
/// Eighteen blocks. A chunked cell read must not allocate — that is the whole
/// point of addressing a chunk by arithmetic — and every format anyone has
/// declared fits well inside this.
const MATRIX_CHUNK_INLINE_DESCRIPTOR_BYTES: usize = 512;
/// Smallest live-tail growth that forces a fresh full index checkpoint.
///
/// Below this floor the geometric-spacing rule in [`VarveFile::needs_index_checkpoint`]
/// would fire on nearly every flush, reintroducing the cumulative O(N^2)
/// checkpoint bytes that PERF-02 removes. The floor keeps small files cheap to
/// recover while capping the per-flush overhead of a rapidly growing append log.
const INDEX_CHECKPOINT_MIN_RECORDS: usize = 16;
const MANIFEST_PAYLOAD_VERSION: u16 = 5;
const WRITER_LOCK_MAGIC: &str = "varve-lock-v1";
const COMPRESSION_ENVELOPE_MAGIC: &[u8; 4] = b"VCMP";
const COMPRESSION_ENVELOPE_VERSION: u8 = 1;
const FILE_COMPRESSION_MAGIC: &[u8; 4] = b"VCHD";
const FILE_COMPRESSION_VERSION: u8 = 1;
/// `VCHD`'s payload, excluding its magic.
///
/// `VCHD` predates the block framing below and carries no length prefix, so its
/// length is fixed by its own layout: version, algorithm, level kind, and the
/// `only_if_smaller` flag, then the exact level and the two length bounds.
const FILE_COMPRESSION_PAYLOAD_LEN: usize = 1 + 1 + 1 + 1 + 4 + 8 + 8;
const MATRIX_SIDECAR_MAGIC: &[u8; 4] = b"VSID";
// v2 bound the sidecar to the native file's OS-object identity and matrix
// layout generation (DUR-04/05). v3 additionally binds it to the per-create
// creation nonce stamped into the native matrix file, so recreating a matrix
// on the same OS file object invalidates every earlier sidecar (DUR2-03).
// v1/v2 envelopes are refused as stale/regenerable.
const MATRIX_SIDECAR_VERSION: u16 = 3;
// Fixed header: 48-byte v1 prefix + 32-byte native fingerprint + 8-byte matrix
// layout generation + 16-byte matrix creation nonce.
const MATRIX_SIDECAR_FIXED_LEN: usize = 104;
const MATRIX_SIDECAR_FINGERPRINT_OFFSET: usize = 48;
const MATRIX_SIDECAR_LAYOUT_GENERATION_OFFSET: usize = 80;
const MATRIX_SIDECAR_CREATION_NONCE_OFFSET: usize = 88;
// Per-create identity region stamped between the native file header and the
// matrix layout header of every matrix file (DUR2-03). `create_with_dims`
// reuses the OS file object when the pathname already exists, so OS identity,
// schema hash and layout offsets are all stable across a same-dims recreate;
// the nonce is the only value that distinguishes the new logical matrix from
// the one every existing sidecar was published against.
const MATRIX_CREATION_NONCE_MAGIC: &[u8; 4] = b"VMNC";
const MATRIX_CREATION_NONCE_VERSION: u16 = 1;
const MATRIX_CREATION_NONCE_LEN: usize = 16;
// Magic (4) + version (2) + reserved (2) + nonce (16).
const MATRIX_CREATION_NONCE_REGION_LEN: usize = 8 + MATRIX_CREATION_NONCE_LEN;
#[cfg(feature = "integrity")]
const STREAM_BUFFER_LEN: usize = 64 * 1024;
const WRITER_LOCK_MAX_LEN: u64 = 16 * 1024;
const PHYSICAL_PAYLOAD_RESOURCE: &str = "payload";
const LOGICAL_PAYLOAD_RESOURCE: &str = "logical payload";
const WRITER_POISON_CONTEXT: &str = "file";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaManifest {
    pub payload_version: u16,
    pub format_version: u16,
    pub endian: Endian,
    pub schema_hash: u64,
    pub extension: Option<String>,
    pub index_policy: IndexPolicy,
    pub commit_policy: CommitPolicy,
    pub integrity_policy: IntegrityPolicy,
    pub recovery_policy: RecoveryPolicy,
    pub manifest_policy: ManifestPolicy,
    pub compression_policy: CompressionPolicy,
    pub blocks: Vec<SchemaBlockDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaBlockDescriptor {
    pub id: u32,
    pub name: String,
    pub version: u16,
    pub kind: BlockKind,
    pub fields: Vec<SchemaFieldDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaFieldDescriptor {
    pub id: u32,
    pub name: String,
    pub wire_type: WireType,
    pub presence: crate::FieldPresence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixSidecarManifest {
    pub format_magic: Vec<u8>,
    pub format_version: u16,
    pub schema_hash: u64,
    pub category: String,
    pub generation: u64,
    pub payload_len: u64,
    pub payload_crc32: u32,
    /// Fingerprint of the native matrix file's OS object (volume + file ID on
    /// Windows, device + inode on Unix) folded with the schema hash. Binds the
    /// sidecar to one specific native file so a same-spec sibling cannot adopt
    /// it (DUR-04).
    pub native_fingerprint: [u8; 32],
    /// The matrix layout generation the sidecar was published against, so a
    /// reader whose native layout has changed rejects the stale sidecar.
    pub matrix_layout_generation: u64,
    /// The per-create creation nonce of the native matrix file the sidecar was
    /// published against. Recreating a matrix on the same OS file object keeps
    /// the OS identity and layout offsets stable, so this nonce is what makes
    /// every pre-recreate sidecar refusable as stale (DUR2-03).
    pub matrix_creation_nonce: [u8; MATRIX_CREATION_NONCE_LEN],
}

/// Identity of the native matrix file a sidecar is bound to.
///
/// Recomputed by every reader from its own open native file and matrix layout,
/// then compared against the values recorded in the sidecar envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MatrixNativeIdentity {
    fingerprint: [u8; 32],
    layout_generation: u64,
    creation_nonce: [u8; MATRIX_CREATION_NONCE_LEN],
}

pub trait MatrixNumeric: Copy {
    const BYTE_LEN: usize;

    fn read_matrix_numeric(bytes: &[u8], endian: Endian) -> Self;
}

macro_rules! impl_matrix_integer {
    ($ty:ty, $len:expr) => {
        impl MatrixNumeric for $ty {
            const BYTE_LEN: usize = $len;

            fn read_matrix_numeric(bytes: &[u8], endian: Endian) -> Self {
                let bytes: [u8; $len] = bytes.try_into().expect("validated matrix numeric width");
                match endian {
                    Endian::Little => <$ty>::from_le_bytes(bytes),
                    Endian::Big => <$ty>::from_be_bytes(bytes),
                }
            }
        }
    };
}

impl MatrixNumeric for u8 {
    const BYTE_LEN: usize = 1;

    fn read_matrix_numeric(bytes: &[u8], _endian: Endian) -> Self {
        bytes[0]
    }
}

impl MatrixNumeric for i8 {
    const BYTE_LEN: usize = 1;

    fn read_matrix_numeric(bytes: &[u8], _endian: Endian) -> Self {
        bytes[0] as i8
    }
}

impl_matrix_integer!(u16, 2);
impl_matrix_integer!(i16, 2);
impl_matrix_integer!(u32, 4);
impl_matrix_integer!(i32, 4);
impl_matrix_integer!(u64, 8);
impl_matrix_integer!(i64, 8);
impl_matrix_integer!(u128, 16);
impl_matrix_integer!(i128, 16);

impl MatrixNumeric for f32 {
    const BYTE_LEN: usize = 4;

    fn read_matrix_numeric(bytes: &[u8], endian: Endian) -> Self {
        let raw = u32::read_matrix_numeric(bytes, endian);
        Self::from_bits(raw)
    }
}

impl MatrixNumeric for f64 {
    const BYTE_LEN: usize = 8;

    fn read_matrix_numeric(bytes: &[u8], endian: Endian) -> Self {
        let raw = u64::read_matrix_numeric(bytes, endian);
        Self::from_bits(raw)
    }
}

pub trait MatrixDurabilityBarrier {
    fn sync_matrix_data(&mut self, file: &mut File) -> Result<()>;
    fn sync_matrix_commit(&mut self, file: &mut File) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FileMatrixDurabilityBarrier;

impl MatrixDurabilityBarrier for FileMatrixDurabilityBarrier {
    fn sync_matrix_data(&mut self, file: &mut File) -> Result<()> {
        file.sync_data()?;
        Ok(())
    }

    fn sync_matrix_commit(&mut self, file: &mut File) -> Result<()> {
        file.sync_all()?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    ReadOnly,
    ReadWrite,
}

/// Mechanical enforcement for F-01: a replacement target cannot be addressed
/// without having been version-checked.
///
/// Every replacement entry point selects its target by block id and then
/// writes `T`'s payload into (or under) the stored record's header. Three of
/// the four copied the stored `block_version` while substituting the new
/// payload, which is a persistent type/version disagreement on disk whenever
/// `schema_hash = 0` lets a differently-versioned program open the file. Two
/// paths carried an ad-hoc check and two did not, and review alone had not
/// caught that in seven rounds.
///
/// The check is therefore no longer a step a path can forget: it is a
/// precondition of *writing* the target. [`ReplacementTarget`] has a private
/// field, lives in its own module so nothing else can name that field, and has
/// exactly one constructor - [`ReplacementTarget::resolve`], which performs
/// both the selection and the version refusal.
///
/// **What is and is not enforced, stated precisely.** Reading `self.index` is
/// not restricted: any code in this file may walk it and compute an ordinal,
/// and round 12's first attempt claimed otherwise. What a caller cannot do is
/// *act* on such an ordinal. The bytes of an already-indexed record are
/// reachable only through [`RecordFile::overwrite_indexed_record`], which takes
/// a [`RecordOverwrite`] by value; the only constructor of a [`RecordOverwrite`]
/// is [`RecordOverwrite::prepare`], which takes a [`ReplacementTarget`] by
/// value. So a replacement path added next month that resolves its own ordinal
/// over `self.index` has nothing it can hand to the writer, and does not
/// compile. See `mod record_file` for the other half (the raw handle is
/// unreachable, so the seek/write pair cannot be duplicated either).
///
/// Proved by `crates/varve/tests/ui/fail_fabricated_replacement_target.rs` and
/// `fail_fabricated_record_overwrite.rs` (compile-fail), and by
/// `crates/varve/tests/enforcement_gates.rs`.
pub(crate) mod replacement_target {
    use super::{Error, KeyedTails, Result, VarveBlock};

    #[derive(Clone, Copy, Debug)]
    pub struct ReplacementTarget {
        position: usize,
    }

    impl ReplacementTarget {
        /// Selects the `ordinal`-th record of `T::ID` and refuses a stored
        /// `block_version` that is not `T::VERSION`.
        ///
        /// This is the only way to build a [`ReplacementTarget`].
        /// Takes the index itself, not a slice of it.
        ///
        /// What this needs is a forward scan that stops at the nth match and
        /// then one positional read — both of which a demand-filled index can
        /// serve. Asking for `&[RecordIndexEntry]` asked for every entry at
        /// once, which is the shape that keeps the index resident.
        pub(super) fn resolve<T: VarveBlock>(
            index: &super::ResidentIndex,
            ordinal: usize,
        ) -> Result<Self> {
            let position = index
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.block_id == T::ID)
                .nth(ordinal)
                .map(|(position, _)| position)
                .ok_or(Error::UnexpectedEof)?;
            let actual = index.entry_at(position)?.block_version;
            if actual != T::VERSION {
                return Err(Error::BlockVersionMismatch {
                    block_id: T::ID,
                    expected: T::VERSION,
                    actual,
                });
            }
            Ok(Self { position })
        }

        /// The resident-index position of the version-checked target.
        pub(super) fn position(self) -> usize {
            self.position
        }
    }

    /// Mechanical enforcement for F-02: the permission to rewrite the bytes of
    /// an already-indexed record, which cannot be obtained without dropping the
    /// block's resident keyed-tail map first.
    ///
    /// The cache maps canonical key bytes to tail offsets and maintained keyed
    /// appends read their predecessor from it, so a record whose stored bytes
    /// change may no longer carry the key the cache filed it under. Round 12
    /// put that invalidation inside the writing function, which made it a
    /// property of *today's* single caller rather than of the operation: a new
    /// seek/write pair elsewhere in the file skipped it and compiled.
    ///
    /// Now the invalidation happens in the constructor of the permission, and
    /// the permission is consumed by value by the only function that can write
    /// those bytes. It is infallible and allocation-free (a `HashMap::remove`),
    /// so it adds no fallible step in either direction of invariant 3, and it
    /// runs strictly before the first byte, so it also covers a half-completed
    /// write, a poisoned writer inspected afterwards, and a caller that
    /// violates the same-key contract of the `unsafe` in-place entry point.
    #[derive(Debug)]
    pub struct RecordOverwrite {
        record_offset: u64,
        payload_offset: u64,
    }

    impl RecordOverwrite {
        /// Consumes a version-checked [`ReplacementTarget`], drops the target
        /// block's keyed-tail map, and yields the write permission.
        ///
        /// This is the only way to build a [`RecordOverwrite`], and a
        /// [`ReplacementTarget`] is the only way to call it.
        /// One positional read, so it takes the index rather than a slice.
        pub(super) fn prepare(
            target: ReplacementTarget,
            index: &super::ResidentIndex,
            keyed_tails: &mut KeyedTails,
        ) -> Result<Self> {
            let entry = index.entry_at(target.position())?;
            let record_offset = entry.record_offset;
            let payload_offset = entry.payload_offset;
            keyed_tails.invalidate(entry.block_id);
            Ok(Self {
                record_offset,
                payload_offset,
            })
        }

        pub(super) fn record_offset(&self) -> u64 {
            self.record_offset
        }

        pub(super) fn payload_offset(&self) -> u64 {
            self.payload_offset
        }
    }
}

use replacement_target::{RecordOverwrite, ReplacementTarget};

/// Mechanical enforcement for shape A/B on the primary file handle: the record
/// bytes of an open generation cannot be written except through one of two
/// gated operations.
///
/// The round-12 review found that `overwrite_record_bytes_in_place` was "the
/// only function in the crate that rewrites the bytes of an already-indexed
/// record" — true of the code as written, and unenforced: a new
/// `self.file.seek(..)` / `self.file.write_all(..)` pair anywhere in this
/// ~12,000-line file compiled and skipped both the version refusal (F-01) and
/// the keyed-tail invalidation (F-02). That is the same "correct today,
/// described in prose" configuration that F-03 was in.
///
/// [`RecordFile`] owns the handle in a module that keeps the field unnameable,
/// and exposes no general-purpose write. `Write`, `Seek` and `Read` are not
/// implemented for it and it hands out no `&File` (`impl Write for &File` would
/// make that equivalent to a mutable handle). The only two ways to put record
/// bytes on disk are:
///
/// * [`RecordFile::append_record_at_end`], which seeks to the end itself and
///   refuses if the resulting offset is not the one the caller budgeted for, so
///   it structurally cannot land inside an existing record; and
/// * [`RecordFile::overwrite_indexed_record`], which consumes a
///   [`RecordOverwrite`] — see `mod replacement_target` for what producing one
///   costs.
///
/// One deliberate exception, named here rather than left to be discovered:
/// [`RecordFile::matrix_region`] hands `crate::matrix` the `&mut File` its
/// functions take. The matrix region is disjoint from the record region and has
/// its own shape-A gate (`matrix.rs`, `mod page_index`). The gate that keeps
/// that exception honest is a source assertion, not the compiler:
/// `crates/varve/tests/enforcement_gates.rs::the_primary_handle_escape_is_only_for_the_matrix_region`.
mod record_file {
    use super::{Error, RecordOverwrite, ReservedIndexSlot, Result, opened_file_identity};
    use crate::snapshot::WrittenThrough;
    use std::fs::{File, Metadata};
    use std::io::{Seek, SeekFrom, Write};

    #[cfg(any(test, feature = "scalable-fault-injection"))]
    std::thread_local! {
        /// `fstat`s this thread has issued through [`RecordFile::metadata`].
        ///
        /// Fault-testing hook only. The append path must not advance it: it
        /// takes the end of file from the snapshot the writer already
        /// maintains, and `append_record_at_end`'s `SEEK_END` check is what
        /// keeps that value honest.
        static RECORD_FILE_METADATA_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// Counts one `RecordFile::metadata` syscall on this thread. Inert outside
    /// tests and without the `scalable-fault-injection` feature.
    #[inline]
    fn count_metadata_call() {
        #[cfg(any(test, feature = "scalable-fault-injection"))]
        RECORD_FILE_METADATA_CALLS.with(|count| count.set(count.get().saturating_add(1)));
    }

    /// Reads and clears this thread's `RecordFile::metadata` syscall count.
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    pub(super) fn take_record_file_metadata_calls() -> u64 {
        RECORD_FILE_METADATA_CALLS.with(|count| count.replace(0))
    }

    #[derive(Debug)]
    pub struct RecordFile {
        file: File,
        /// Owner of this file's per-thread private read handles. Holds only an
        /// id and a cold-path `Mutex<Vec<Arc<File>>>`, so `RecordFile` stays
        /// `Sync` and the read path stays lock-free; see
        /// `matrix.rs`'s `mod region_reader`.
        matrix_read_pool: crate::matrix::MatrixReadPool,
    }

    impl RecordFile {
        pub(super) fn new(file: File) -> Self {
            Self {
                file,
                matrix_read_pool: crate::matrix::MatrixReadPool::new(),
            }
        }

        pub(super) fn metadata(&self) -> std::io::Result<Metadata> {
            count_metadata_call();
            self.file.metadata()
        }

        pub(super) fn stream_position(&mut self) -> std::io::Result<u64> {
            self.file.stream_position()
        }

        /// Moves the handle's cursor. Reads and cursor restoration only: this
        /// type implements no write that follows the cursor.
        pub(super) fn seek_to(&mut self, offset: u64) -> std::io::Result<u64> {
            self.file.seek(SeekFrom::Start(offset))
        }

        pub(super) fn set_len(&mut self, len: u64) -> std::io::Result<()> {
            self.file.set_len(len)
        }

        pub(super) fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }

        pub(super) fn sync_all(&self) -> std::io::Result<()> {
            self.file.sync_all()
        }

        pub(super) fn sync_data(&self) -> std::io::Result<()> {
            self.file.sync_data()
        }

        /// OS-object identity bytes of the open handle.
        ///
        /// Returned as bytes rather than as a `&File` on purpose: `&File`
        /// implements `Write`, so lending one out would reopen exactly the hole
        /// this module closes.
        pub(super) fn object_identity(&self) -> Result<Vec<u8>> {
            opened_file_identity(&self.file)
        }

        /// The named exception: the handle `crate::matrix`'s functions take.
        ///
        /// The matrix region is disjoint from the record region; its own
        /// shape-A enforcement is `matrix.rs`'s `mod page_index`. Every call
        /// site of this accessor must be an argument to a `crate::matrix::`
        /// call, which `enforcement_gates.rs` asserts.
        pub(super) fn matrix_region(&mut self) -> &mut File {
            &mut self.file
        }

        /// The read-only half of the same exception, over a **shared** borrow.
        ///
        /// This is what lets every matrix read entry point take `&self`: the
        /// returned value reads positionally (`pread`/`seek_read`) and moves no
        /// cursor, so it needs no exclusive borrow and two threads may use two
        /// of them against this handle at once. Unlike `matrix_region` it does
        /// not lend out the `&File` — `MatrixRegionReader`'s field is private to
        /// its own module, so the `impl Write for &File` route is unreachable
        /// through it. It is therefore a strictly narrower escape than the one
        /// above, and needs no source gate of its own.
        ///
        /// It also carries this file's [`crate::matrix::MatrixReadPool`], which
        /// is what makes the shared borrow worth having on Windows: without it
        /// N threads sharing one handle are measurably *slower* than one thread,
        /// because `ReadFile` serialises on the file object. The pool hands each
        /// reading thread its own file object derived from this handle.
        pub(super) fn matrix_region_reader(&self) -> crate::matrix::MatrixRegionReader<'_> {
            crate::matrix::MatrixRegionReader::with_pool(&self.file, &self.matrix_read_pool)
        }

        /// Appends one record at the current end of file.
        ///
        /// Seeks to the end itself and refuses if that is not `expected_offset`
        /// — the offset the caller budgeted, checked against limits and
        /// recorded in its rollback snapshot. A caller cannot use this to write
        /// into an already-indexed record even by passing an arbitrary offset.
        ///
        /// # Why it takes the reservation token
        ///
        /// Shape A is *"authoritative disk write, then fallible mirror
        /// update"*. Holding a [`ReservedIndexSlot`] is proof that the resident
        /// index has already been charged against `ReadLimitKey::IndexBytes`
        /// and `try_reserve`d for this record, so the only work left for the
        /// mirror after this write is an infallible `push`. Demanding it here
        /// is what makes the ordering a compile-time property: an append helper
        /// written next month cannot reach this function at all until it has
        /// done the fallible half, and the reservation therefore cannot be
        /// moved below the write. The token is zero-sized and is borrowed, not
        /// consumed, because the caller still needs it to install the entry
        /// afterwards.
        ///
        /// # Why it returns a [`WrittenThrough`]
        ///
        /// This function owns the offset the write actually went to and every
        /// byte count it wrote, so the fact "the file physically reaches
        /// `offset + bytes`" is created here, inside the module that owns the
        /// raw handle. Handing it back as a witness is what lets the caller
        /// rebind its snapshot without an `fstat` — see
        /// [`crate::snapshot::SnapshotFile::with_written_len`]. It is built
        /// from `offset`, the value `SEEK_END` returned, not from
        /// `expected_offset`: it describes where the bytes went, not where
        /// they were meant to go.
        pub(super) fn append_record_at_end(
            &mut self,
            _reserved: &ReservedIndexSlot,
            expected_offset: u64,
            header_bytes: &[u8],
            payload: &[u8],
            footer: Option<&[u8]>,
            after_header: impl FnOnce() -> Result<()>,
        ) -> Result<WrittenThrough> {
            let offset = self.file.seek(SeekFrom::End(0))?;
            if offset != expected_offset {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "append offset is not the end of the file",
                )));
            }
            self.file.write_all(header_bytes)?;
            after_header()?;
            self.file.write_all(payload)?;
            if let Some(footer) = footer {
                self.file.write_all(footer)?;
            }
            let written = [
                header_bytes.len(),
                payload.len(),
                footer.map_or(0, <[u8]>::len),
            ]
            .into_iter()
            .try_fold(offset, |end, part| {
                u64::try_from(part)
                    .ok()
                    .and_then(|part| end.checked_add(part))
            })
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "appended record extent",
            })?;
            Ok(WrittenThrough::after_write(written))
        }

        /// Rewrites an already-indexed record's header and payload in place.
        ///
        /// Takes the [`RecordOverwrite`] by value; producing one performs the
        /// version refusal (F-01) and drops the block's keyed-tail map (F-02),
        /// both strictly before the first byte reaches disk.
        pub(super) fn overwrite_indexed_record(
            &mut self,
            write: RecordOverwrite,
            header_bytes: &[u8],
            payload: &[u8],
        ) -> Result<()> {
            self.file.seek(SeekFrom::Start(write.record_offset()))?;
            self.file.write_all(header_bytes)?;
            self.file.seek(SeekFrom::Start(write.payload_offset()))?;
            self.file.write_all(payload)?;
            self.file.flush()?;
            Ok(())
        }
    }
}

use record_file::RecordFile;

/// Mechanical enforcement for **shape A** on the resident record index: the
/// mirror install that follows the authoritative append cannot be reached
/// without its reservation.
///
/// `VarveFile::index` is the in-memory mirror of the records on disk. The
/// append core reserves one slot for it, writes the record, and then pushes the
/// entry — the reservation is roughly eighty lines above the push, and until
/// round 12 the only thing keeping them in that order was a source comment.
/// That is the exact configuration round 12's F-03 was in `matrix.rs`: a
/// correct ordering, described in prose, in a function nobody was going to
/// re-derive. An edit that moved the charge or the `try_reserve` below the
/// write would leave an `AllocationFailed` with the record on disk, absent from
/// the index, on a writer that is not poisoned.
///
/// [`ReservedIndexSlot`] removes the choice. It has a private field and lives
/// in its own module, so it can only come from [`ResidentIndex::reserve`],
/// which does the limit checks and the `try_reserve`; and
/// [`ResidentIndex::install`] takes it **by value** and returns `()`, so
/// the push is infallible, allocation-free, and unreachable without the
/// reservation having already succeeded. Cost on the hot path: none — the
/// token is zero-sized and the two functions hold exactly the code that was
/// inline before.
///
/// # Why the mirror itself lives here too
///
/// Re-verification of the first cut showed that the token bound less than it
/// claimed, because the mirror was still an ordinary `Vec` field of
/// `VarveFile`. Both of these compiled inside `file.rs`:
///
/// ```text
/// let mut mirror = core::mem::take(&mut self.index);   // grow around the token
/// mirror.insert(0, entry);
/// self.index = mirror;
/// ```
///
/// and, worse for invariant 3,
///
/// ```text
/// self.file.append_record_at_end(..)?;                 // authoritative write
/// let slot = ReservedIndexSlot::reserve(&mut self.index, charge)?;  // fallible, AFTER it
/// slot.install(&mut self.index, entry);
/// ```
///
/// The token proved that a reservation *existed*, not that it *preceded* the
/// write, and it did not stop a second route to the `Vec`. Two changes close
/// both, and neither is a rule a later function can fail to repeat:
///
/// * the `Vec` is a private field of [`ResidentIndex`], declared in this
///   module. Outside it the field cannot be named, so `mem::take`, `insert`,
///   `push`, `append` and `extend` are unreachable; the only growth in the
///   crate is [`ResidentIndex::install`], and the only way to call it is to
///   hold the token.
/// * the append itself demands the token: [`RecordFile::append_record_at_end`]
///   takes `&ReservedIndexSlot`. A caller that has not already charged the
///   limit and reserved the slot has nothing to write *with*, so the fallible
///   mirror half cannot be moved below the authoritative write — that ordering
///   is now a signature, not a comment.
///
/// What is deliberately *not* forbidden: reading the mirror (it derefs to a
/// slice), mutating fields of an entry already in it
/// ([`ResidentIndex::entry_mut`], used by the in-place replacement path to
/// restamp a sequence and checksum), shrinking it
/// ([`ResidentIndex::truncate`], the append rollback), and replacing a whole
/// generation ([`ResidentIndex::adopt_generation`], the rewrite paths, which
/// build their `Vec` with `try_reserve_exact` before the new file exists).
/// **Correction, round 16.** The three above cannot add an entry the disk does
/// not have; `adopt_generation` can. It adopts whatever `Vec` it is handed, and
/// re-verification compiled and ran
/// `ResidentIndex::adopt_generation(vec![RecordIndexEntry { committed: true, .. }])`
/// to a mirror of length one for a record no disk holds. What holds it today is
/// its eight call sites — open, and the rewrite paths that build the `Vec` from
/// a generation they have just published — not a check. The cheap checked
/// replacement (take the backing `&File` and run each entry through
/// `RecordIndexEntry::validate_payload_extent`) was measured and rejected: a
/// zero-length entry against a zero-length file passes it. Refusing that needs a
/// header read per entry, i.e. O(records) I/O added to open for index policies
/// that do not scan. Checklist open item 29.
pub(crate) mod resident_index {
    use super::{RecordIndexEntry, Result};

    /// Proof that the resident record index has spare capacity for one entry.
    ///
    /// Zero-sized, so demanding it costs nothing at run time; produced only by
    /// `ResidentIndex::reserve`, so demanding it costs a fallible reservation
    /// at compile time.
    #[derive(Debug)]
    #[must_use = "a reserved index slot is the proof that the mirror can accept \
                  the entry; drop it only if the append is abandoned"]
    pub struct ReservedIndexSlot(());

    /// The in-memory mirror of the records on disk.
    ///
    /// The backing `Vec` is unnameable outside this module; see the module
    /// documentation for what that forbids and what it deliberately allows.
    #[derive(Debug)]
    pub struct ResidentIndex {
        entries: Vec<RecordIndexEntry>,
    }

    impl ResidentIndex {
        /// Adopts the entries of a freshly built generation.
        ///
        /// Used at open time (the index scanned from the file) and by the
        /// rewrite replacement paths (the index of the temporary generation
        /// that has just become the file). Both hand over a `Vec` that already
        /// describes records on disk, so this cannot introduce an entry the
        /// disk does not have; and both build it with `try_reserve_exact`
        /// before any byte is published, so no allocation is deferred past a
        /// commit point.
        pub(crate) fn adopt_generation(entries: Vec<RecordIndexEntry>) -> Self {
            Self { entries }
        }

        /// Performs every fallible part of growing the mirror by one entry.
        ///
        /// `charge` is the caller's already-computed limit check; it runs here
        /// so that the charge and the reservation cannot be separated either.
        pub(crate) fn reserve(
            &mut self,
            charge: impl FnOnce() -> Result<u64>,
        ) -> Result<ReservedIndexSlot> {
            let index_bytes = charge()?;
            self.entries
                .try_reserve(1)
                .map_err(|_| super::Error::AllocationFailed {
                    resource: "record index",
                    requested: index_bytes,
                })?;
            Ok(ReservedIndexSlot(()))
        }

        /// Installs the entry into capacity that is already reserved.
        ///
        /// Infallible and allocation-free by construction: this is the only
        /// growth of the resident index in the crate.
        pub(crate) fn install(&mut self, slot: ReservedIndexSlot, entry: RecordIndexEntry) {
            let ReservedIndexSlot(()) = slot;
            self.entries.push(entry);
        }

        /// Drops the tail beyond `len`, for the append rollback. Shrinking can
        /// only make the mirror describe fewer records than the disk holds,
        /// which is the direction rollback restores.
        pub(crate) fn truncate(&mut self, len: usize) {
            self.entries.truncate(len);
        }

        /// Mutable access to an entry that is already in the mirror, for the
        /// in-place replacement path's sequence and checksum restamp. The
        /// entry count cannot change through this handle.
        pub(crate) fn entry_mut(&mut self, position: usize) -> &mut RecordIndexEntry {
            &mut self.entries[position]
        }
    }

    impl ResidentIndex {
        /// Entries in order.
        ///
        /// A method and not a `Deref` to `[RecordIndexEntry]`. The slice
        /// contract said "every entry, contiguous, borrowed at once", which is
        /// the one shape a demand-filled index cannot produce — and it was
        /// implicit, so nothing had to ask for it deliberately. Every caller
        /// that genuinely needs the whole array now says so by name.
        pub(crate) fn iter(&self) -> std::slice::Iter<'_, RecordIndexEntry> {
            self.entries.iter()
        }

        pub(crate) fn len(&self) -> usize {
            self.entries.len()
        }

        /// Only the unit tests ask; production code asks `len`.
        #[cfg(test)]
        pub(crate) fn is_empty(&self) -> bool {
            self.entries.is_empty()
        }

        /// The entry at `position`, or `UnexpectedEof` if there is none.
        ///
        /// Positions reaching this come from `ReplacementTarget::resolve`, which
        /// found them by walking this same index, so the miss arm is
        /// unreachable today — but a demand-filled store can fail to produce an
        /// entry for reasons a slice index cannot, and a panicking `[i]` leaves
        /// nowhere to put that.
        pub(crate) fn entry_at(&self, position: usize) -> crate::Result<&RecordIndexEntry> {
            self.entries
                .get(position)
                .ok_or(crate::Error::UnexpectedEof)
        }

        /// The whole array, borrowed contiguously.
        ///
        /// This is the shape that keeps the index resident. Every caller is a
        /// place laziness has to be designed for rather than dropped in, so the
        /// name is deliberately awkward and the call sites are the worklist.
        pub(crate) fn as_contiguous_slice(&self) -> &[RecordIndexEntry] {
            &self.entries
        }
    }
}

use resident_index::{ReservedIndexSlot, ResidentIndex};

/// An owned snapshot of a file's record index.
///
/// Derefs to `[RecordIndexEntry]`, so it reads exactly like the borrowed slice
/// this replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntries {
    entries: Vec<RecordIndexEntry>,
}

impl core::ops::Deref for IndexEntries {
    type Target = [RecordIndexEntry];

    fn deref(&self) -> &[RecordIndexEntry] {
        &self.entries
    }
}

impl IntoIterator for IndexEntries {
    type Item = RecordIndexEntry;
    type IntoIter = std::vec::IntoIter<RecordIndexEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a> IntoIterator for &'a IndexEntries {
    type Item = &'a RecordIndexEntry;
    type IntoIter = std::slice::Iter<'a, RecordIndexEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RecordIndexEntry {
    pub block_id: u32,
    pub block_version: u16,
    pub flags: u16,
    pub sequence: u64,
    pub record_offset: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub checksum: u32,
    pub uncompressed_len_hint: u32,
    pub footer_offset: Option<u64>,
    pub prev_same_block_offset: Option<u64>,
    pub prev_same_key_offset: Option<u64>,
    pub committed: bool,
}

impl RecordIndexEntry {
    /// Reads stored payload bytes by reopening the object currently named by
    /// `path`.
    ///
    /// This is a low-level, non-snapshot convenience API. It does not prove
    /// that the path still names the object from which this entry was indexed,
    /// does not establish index membership or checksum validity, and applies
    /// the standard physical payload ceiling. Prefer a typed
    /// reader/collection, which remains bound to its opened snapshot. For an
    /// explicitly path-based untrusted-input tool, use
    /// [`Self::read_payload_limited`] after establishing the entry's provenance.
    pub fn read_payload(&self, path: &Path) -> Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        self.validate_payload_extent(file_len)?;
        crate::ReadLimits::STANDARD.check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        self.read_payload_file_validated(&mut file)
    }

    /// Reads stored payload bytes from the object currently named by `path`
    /// after applying an allocation ceiling.
    ///
    /// This remains path-based and non-snapshot: pathname replacement can make
    /// it read a different object with a compatible extent. The limit does not
    /// establish index membership or checksum validity. High-level readers do
    /// not use this method.
    pub fn read_payload_limited(&self, path: &Path, limit: u64) -> Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        self.validate_payload_extent(file_len)?;
        ensure_payload_limit(PHYSICAL_PAYLOAD_RESOURCE, self.payload_len, limit)?;
        self.read_payload_file_validated(&mut file)
    }

    pub fn is_compressed(&self) -> bool {
        self.flags & RECORD_FLAG_COMPRESSED != 0
    }

    /// Reads and decodes logical payload bytes through the current pathname.
    ///
    /// This low-level method is non-snapshot and resolves the format's runtime
    /// physical and logical allocation ceilings. It does not replace index-membership or record
    /// integrity validation. Use a typed snapshot reader for ordinary reads,
    /// or [`Self::read_logical_payload_limited`] for explicitly path-based
    /// tooling that has separately established entry provenance.
    pub fn read_logical_payload(&self, spec: FormatSpec, path: &Path) -> Result<Vec<u8>> {
        self.read_logical_payload_limited(spec, path, u64::MAX, u64::MAX)
    }

    /// Reads and decodes through the current pathname with separate stored and
    /// logical allocation ceilings.
    ///
    /// The limits bound allocation but do not turn this path-based helper into
    /// an opened-object snapshot API.
    pub fn read_logical_payload_limited(
        &self,
        spec: FormatSpec,
        path: &Path,
        physical_limit: u64,
        logical_limit: u64,
    ) -> Result<Vec<u8>> {
        let spec = spec.resolve_entrypoint();
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        self.validate_payload_extent(file_len)?;
        spec.read_limits
            .check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        ensure_payload_limit(PHYSICAL_PAYLOAD_RESOURCE, self.payload_len, physical_limit)?;
        let logical_len = self.logical_payload_len_before_allocation(spec, &mut file)?;
        spec.read_limits
            .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;
        ensure_payload_limit(LOGICAL_PAYLOAD_RESOURCE, logical_len, logical_limit)?;
        let payload = self.read_payload_file_validated(&mut file)?;
        decode_record_payload(spec, self, payload)
    }

    pub(crate) fn logical_payload_len_snapshot(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
    ) -> Result<u64> {
        spec.read_limits
            .check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        self.validate_payload_extent(snapshot.len())?;
        let logical_len = if !self.is_compressed() {
            self.payload_len
        } else {
            self.logical_payload_len_from_snapshot(spec, snapshot)?
        };
        spec.read_limits
            .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;
        Ok(logical_len)
    }

    pub(crate) fn read_payload_snapshot(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
    ) -> Result<Vec<u8>> {
        spec.read_limits
            .check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        self.validate_payload_extent(snapshot.len())?;
        let payload = snapshot.read_vec_at(
            self.payload_offset,
            self.payload_len,
            limit_or_max(spec.read_limits.require(ReadLimitKey::RecordPayloadLen)?),
            ReadLimitKey::RecordPayloadLen.resource(),
        )?;
        self.verify_snapshot_record(spec, snapshot, &payload)?;
        Ok(payload)
    }

    pub(crate) fn read_logical_payload_snapshot(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
    ) -> Result<Vec<u8>> {
        let logical_len = self.logical_payload_len_snapshot(spec, snapshot)?;
        let payload = self.read_payload_snapshot(spec, snapshot)?;
        let decoded = decode_record_payload(spec, self, payload)?;
        let actual =
            u64::try_from(decoded.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        if actual != logical_len {
            return Err(Error::DecompressedLengthMismatch {
                expected: logical_len,
                actual,
            });
        }
        Ok(decoded)
    }

    pub fn checked_physical_end(&self) -> Result<u64> {
        self.payload_offset
            .checked_add(self.payload_len)
            .and_then(|end| {
                end.checked_add(u64::from(self.footer_offset.is_some()) * RECORD_FOOTER_LEN)
            })
            .ok_or(Error::LengthOverflow {
                value: self.payload_len,
            })
    }

    pub fn physical_end(&self) -> u64 {
        self.payload_offset
            .saturating_add(self.payload_len)
            .saturating_add(u64::from(self.footer_offset.is_some()) * RECORD_FOOTER_LEN)
    }
}

impl From<&RecordIndexEntry> for AppendInfo {
    fn from(entry: &RecordIndexEntry) -> Self {
        Self {
            sequence: entry.sequence,
            record_offset: entry.record_offset,
            payload_offset: entry.payload_offset,
            payload_len: entry.payload_len,
            footer_offset: entry.footer_offset,
            prev_same_block_offset: entry.prev_same_block_offset,
            prev_same_key_offset: entry.prev_same_key_offset,
            committed: entry.committed,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BlockEvent {
    pub block_id: u32,
    pub block_version: u16,
    pub sequence: u64,
    pub record_offset: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendInfo {
    pub sequence: u64,
    pub record_offset: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub footer_offset: Option<u64>,
    pub prev_same_block_offset: Option<u64>,
    pub prev_same_key_offset: Option<u64>,
    pub committed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplacementInfo {
    pub sequence: u64,
    pub record_offset: u64,
    pub old_payload_len: u64,
    pub new_payload_len: u64,
    pub old_physical_len: u64,
    pub new_physical_len: u64,
}

impl ReplacementInfo {
    pub fn translate_record_offset(&self, old: u64) -> Result<u64> {
        if old <= self.record_offset {
            return Ok(old);
        }
        if self.new_physical_len >= self.old_physical_len {
            old.checked_add(self.new_physical_len - self.old_physical_len)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "replacement record offset",
                })
        } else {
            old.checked_sub(self.old_physical_len - self.new_physical_len)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "replacement record offset",
                })
        }
    }
}

impl From<&RecordIndexEntry> for BlockEvent {
    fn from(entry: &RecordIndexEntry) -> Self {
        Self {
            block_id: entry.block_id,
            block_version: entry.block_version,
            sequence: entry.sequence,
            record_offset: entry.record_offset,
            payload_offset: entry.payload_offset,
            payload_len: entry.payload_len,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    pub original_len: u64,
    pub recovered_len: u64,
    pub records_preserved: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterLockInfo {
    pub path: PathBuf,
    pub target_path: PathBuf,
    pub process_id: u32,
    pub created_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterLockBreakPolicy {
    Refuse,
    BreakIfProcessAbsent,
    BreakIfOlderThan(Duration),
    BreakIfProcessAbsentAndOlderThan(Duration),
}

/// What one user record actually stores, and where those bytes live.
///
/// `bytes` is a [`Cow`] rather than a `Vec` because the four uncompressed
/// routes through [`prepare_user_record_payload`] — no compression declared for
/// the block (the default), a non-`Variable` block kind, a payload under
/// `min_uncompressed_len`, and `only_if_smaller` losing — store the caller's
/// bytes unchanged. Owning them meant one payload-sized allocation, memcpy and
/// free on every append of every default configuration, for a buffer every
/// caller immediately reborrows as `&payload.bytes` and drops. Only the arm
/// that actually produces new bytes (compression, and the envelope around it)
/// owns them.
#[derive(Clone, Debug)]
struct StoredPayload<'a> {
    flags: u16,
    uncompressed_len_hint: u32,
    bytes: Cow<'a, [u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SequenceState {
    Available(u64),
    Exhausted,
}

impl SequenceState {
    fn available(self) -> Result<u64> {
        match self {
            Self::Available(sequence) => Ok(sequence),
            Self::Exhausted => Err(Error::SequenceExhausted),
        }
    }

    fn after_publishing(sequence: u64) -> Self {
        match sequence.checked_add(1) {
            Some(next) => Self::Available(next),
            None => Self::Exhausted,
        }
    }
}

/// O(1) writer-side state behind [`VarveFile::needs_index_checkpoint`]
/// (PERF2-02).
///
/// The original predicate reverse-searched the resident index for the last
/// checkpoint and re-counted the suffix on every flush, which made the
/// cumulative flush CPU O(N^2) for flush-per-record workloads even after the
/// checkpoint *bytes* were made amortized O(N). This state mirrors exactly
/// what that scan recomputed and is maintained incrementally at the single
/// index append site, restored on append rollback, and recovered with one
/// bounded reverse walk wherever the resident index is loaded or replaced
/// wholesale (open, recovery, and generation rebinds).
///
/// Invariant: at all times this equals `CheckpointCadence::from_index(&index)`
/// for the current resident index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CheckpointCadence {
    /// Entries appended since the last full checkpoint whose presence would
    /// change the serialized checkpoint (everything but commit markers).
    eligible_since_checkpoint: usize,
    /// Geometric growth threshold derived from the last checkpoint's index
    /// position: `max(INDEX_CHECKPOINT_MIN_RECORDS, records_at_last_checkpoint / 2)`.
    next_threshold: usize,
}

impl CheckpointCadence {
    /// Cadence for a freshly created file with an empty resident index.
    fn new_empty() -> Self {
        Self {
            eligible_since_checkpoint: 0,
            next_threshold: INDEX_CHECKPOINT_MIN_RECORDS,
        }
    }

    /// Recovers the cadence with a single reverse walk that stops at the last
    /// checkpoint record, so reopen pays the live-tail length exactly once
    /// instead of every flush paying it again.
    /// The rule, kept as the specification `derive_index_state` must match.
    ///
    /// No longer called in production — one forward pass now derives this and
    /// the two structures beside it — but it is what the invariant above names,
    /// and `derived_state_matches_the_three_rules_it_replaced` compares the two
    /// on every shape that distinguishes them.
    #[cfg(test)]
    fn from_index(index: &[RecordIndexEntry]) -> Self {
        let mut eligible: usize = 0;
        let mut touched: u64 = 0;
        for (position, entry) in index.iter().enumerate().rev() {
            touched = touched.saturating_add(1);
            if entry.block_id == INDEX_BLOCK_ID {
                note_checkpoint_cadence_index_touches(touched);
                return Self {
                    eligible_since_checkpoint: eligible,
                    next_threshold: core::cmp::max(INDEX_CHECKPOINT_MIN_RECORDS, position / 2),
                };
            }
            if !matches!(entry.block_id, COMMIT_BLOCK_ID | SEGMENT_BLOCK_ID) {
                eligible = eligible.saturating_add(1);
            }
        }
        note_checkpoint_cadence_index_touches(touched);
        Self {
            eligible_since_checkpoint: eligible,
            next_threshold: INDEX_CHECKPOINT_MIN_RECORDS,
        }
    }

    /// Advances the cadence for the entry just pushed at `position` in the
    /// resident index. A checkpoint record resets the tail count and derives
    /// the next geometric threshold from its own position (the number of
    /// entries it serialized); commit markers and segment records never alter
    /// checkpoint identity; every other record grows the eligible tail by one.
    fn note_appended(&mut self, position: usize, block_id: u32) {
        note_checkpoint_cadence_index_touches(1);
        if block_id == INDEX_BLOCK_ID {
            self.eligible_since_checkpoint = 0;
            self.next_threshold = core::cmp::max(INDEX_CHECKPOINT_MIN_RECORDS, position / 2);
        } else if !matches!(block_id, COMMIT_BLOCK_ID | SEGMENT_BLOCK_ID) {
            self.eligible_since_checkpoint = self.eligible_since_checkpoint.saturating_add(1);
        }
    }
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    static CHECKPOINT_CADENCE_INDEX_TOUCHES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Counts resident index entries examined by the checkpoint flush-cadence
/// machinery on this thread (PERF2-02 regression evidence). Inert without the
/// `scalable-fault-injection` feature.
#[inline]
fn note_checkpoint_cadence_index_touches(count: u64) {
    #[cfg(feature = "scalable-fault-injection")]
    CHECKPOINT_CADENCE_INDEX_TOUCHES
        .with(|touches| touches.set(touches.get().saturating_add(count)));
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = count;
}

/// The payload one sealed chunk occupies, given the declared dimensions.
///
/// Computed at create so the two ceilings a chunk crosses — the buffer it is
/// held in and the record it is written as — are reconciled before any write is
/// accepted rather than at the first seal.
fn chunk_payload_len_for(spec: FormatSpec, dims: &MatrixDimensions) -> Result<u64> {
    let Some(growing) = spec.growing_matrix else {
        return Ok(0);
    };
    let cell_crc = !matches!(spec.integrity_policy, IntegrityPolicy::None);
    let mut len = MATRIX_CHUNK_PREFIX_LEN
        .checked_add(
            MATRIX_CHUNK_BLOCK_DESC_LEN
                .checked_mul(spec.matrix_blocks.len() as u64)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk descriptors",
                })?,
        )
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "matrix chunk prefix",
        })?;
    for block in spec.matrix_blocks {
        let width = dims
            .get(block.dimensions[1])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[1].to_string()))?;
        let cells =
            growing
                .rows_per_chunk
                .checked_mul(width)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk cells",
                })?;
        let slots =
            cells
                .checked_mul(block.slot_stride)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk slot region",
                })?;
        let crc = if cell_crc {
            cells
                .checked_mul(MATRIX_CHUNK_CRC_LEN)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk checksum table",
                })?
        } else {
            0
        };
        // The uncompressed size plus the sub-block index. Compression can only
        // make the sealed record smaller, so bounding the uncompressed form is
        // the conservative check — a spec must not depend on its data
        // compressing in order to fit.
        let index = if spec.chunk_compression.is_some() {
            sub_block_count(cells, compressed_cell_width(block.slot_stride, cell_crc))
                .checked_add(1)
                .and_then(|entries| entries.checked_mul(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN))
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk sub-block index",
                })?
        } else {
            0
        };
        len = len
            .checked_add(cells.div_ceil(8))
            .and_then(|len| len.checked_add(crc))
            .and_then(|len| len.checked_add(index))
            .and_then(|len| len.checked_add(slots))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "matrix chunk payload length",
            })?;
    }
    Ok(len)
}

/// Writes one block's slot region as a sub-block index plus compressed
/// sub-blocks.
///
/// The index is `sub_block_count + 1` little-endian `u32` offsets relative to
/// the end of the index, so a sub-block's extent is `off[i]..off[i + 1]` — two
/// four-byte reads and no prefix sum.
///
/// **A sub-block is stored raw when compressing it does not shrink it**, and
/// the reader tells the two apart by length alone: stored length equal to the
/// uncompressed length means raw. The writer therefore never emits a compressed
/// sub-block of exactly that length, which costs nothing — such a sub-block is
/// stored raw instead, byte for byte the same thing.
fn encode_compressed_slots(
    payload: &mut Vec<u8>,
    block: &OpenChunkBlock,
    compression: VariableCompression,
) -> Result<()> {
    let cell_crc = !block.crc.is_empty();
    let width = compressed_cell_width(block.stride, cell_crc);
    let per = cells_per_sub_block(width);
    let count = sub_block_count(block.cells, width);
    let count_usize = usize::try_from(count).map_err(|_| Error::InvalidMatrixChunk)?;
    let stride = usize::try_from(block.stride).map_err(|_| Error::InvalidMatrixChunk)?;
    let width_usize = usize::try_from(width).map_err(|_| Error::InvalidMatrixChunk)?;
    let per_usize = usize::try_from(per).map_err(|_| Error::InvalidMatrixChunk)?;
    let cells = usize::try_from(block.cells).map_err(|_| Error::InvalidMatrixChunk)?;

    let mut stored: Vec<Vec<u8>> = Vec::new();
    stored
        .try_reserve_exact(count_usize)
        .map_err(|_| Error::AllocationFailed {
            resource: "matrix chunk sub-blocks",
            requested: count,
        })?;
    let mut packed = Vec::new();
    for index in 0..count_usize {
        let first = index * per_usize;
        let last = ((index + 1) * per_usize).min(cells);
        packed.clear();
        packed
            .try_reserve((last.saturating_sub(first)) * width_usize)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix chunk sub-block",
                requested: width,
            })?;
        for cell in first..last {
            packed.extend_from_slice(&block.slots[cell * stride..(cell + 1) * stride]);
            if cell_crc {
                packed.extend_from_slice(&block.crc[cell * 4..(cell + 1) * 4]);
            }
        }
        let compressed =
            compress_with_algorithm(compression.algorithm, compression.level, &packed)?;
        if compressed.len() >= packed.len() {
            stored.push(packed.clone());
        } else {
            stored.push(compressed);
        }
    }

    let mut offset = 0u32;
    for bytes in &stored {
        payload.extend_from_slice(&offset.to_le_bytes());
        offset = offset
            .checked_add(u32::try_from(bytes.len()).map_err(|_| Error::InvalidMatrixChunk)?)
            .ok_or(Error::InvalidMatrixChunk)?;
    }
    payload.extend_from_slice(&offset.to_le_bytes());
    for bytes in &stored {
        payload.extend_from_slice(bytes);
    }
    Ok(())
}

/// Bytes a compressed sub-block holds per cell.
///
/// **The checksum travels with its cell.** Compressing only the slot region
/// left the per-cell checksum table — four bytes per cell, all zeros for every
/// never-written cell — as the dominant term: measured 1,606,396 B plain
/// against 1,083,604 B with slots-only compression, a 33% saving where the
/// slots alone are half the file. Packing each cell beside its checksum makes
/// one decode yield both and lets the zeros collapse together.
const fn compressed_cell_width(stride: u64, cell_crc: bool) -> u64 {
    if cell_crc {
        stride + MATRIX_CHUNK_CRC_LEN
    } else {
        stride
    }
}

/// Cells one compressed sub-block covers, for a given packed cell width.
///
/// Derived from the width alone so the writer and the reader cannot disagree
/// about where a cell lives.
const fn cells_per_sub_block(stride: u64) -> u64 {
    if stride == 0 || stride >= MATRIX_CHUNK_SUB_BLOCK_BYTES {
        1
    } else {
        MATRIX_CHUNK_SUB_BLOCK_BYTES / stride
    }
}

/// Sub-blocks a block's slot region is cut into.
fn sub_block_count(cells: u64, stride: u64) -> u64 {
    cells.div_ceil(cells_per_sub_block(stride)).max(1)
}

/// A zeroed buffer of `len` bytes, allocated fallibly.
///
/// A chunk's buffers are sized by a declared knob, so an over-large
/// `rows_per_chunk` must report `AllocationFailed` rather than abort.
fn try_zeroed_vec(len: u64, resource: &'static str) -> Result<Vec<u8>> {
    let capacity = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested: len,
        })?;
    buffer.resize(capacity, 0);
    Ok(buffer)
}

/// One matrix block's slice of the chunk being filled.
#[derive(Debug)]
struct OpenChunkBlock {
    block_id: u32,
    stride: u64,
    /// `rows * dimension 1`, the cells this chunk holds for this block.
    cells: u64,
    /// One bit per cell: written this session, whatever the bytes say.
    ///
    /// The region keeps the same thing (`current_write_bits`) for the same
    /// reason. Without it, `commit_matrix_cell` decided "was this written" by
    /// testing whether the slot read as all zeros, so a legitimate value that
    /// encodes to zeros — `0u32`, an empty flag word, a zeroed struct — could
    /// never be committed in a chunk while committing fine in the region.
    /// Not written to disk: the commit map is what a reader consults.
    written: Vec<u8>,
    /// One bit per cell, the chunk-local form of the region's commit map.
    commit: Vec<u8>,
    /// `crc32` of each cell's slot bytes, or empty when the format declares no
    /// integrity policy. The chunk-local form of the region's checksum table.
    crc: Vec<u8>,
    /// `cells * stride` bytes, addressed exactly as the region's slot region is.
    slots: Vec<u8>,
}

/// A chunk record's prefix, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChunkPrefix {
    index: u64,
    first_row: u64,
    rows: u64,
    block_count: u32,
    cell_crc: bool,
    compressed_slots: bool,
}

/// One sealed chunk, located.
///
/// `block_count` is carried so a cell lookup does not re-read the prefix that
/// finding the chunk already read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChunkLocator {
    index: u64,
    record_offset: u64,
    payload_len: u64,
    block_count: u32,
    cell_crc: bool,
    compressed_slots: bool,
}

/// Every sealed chunk, by chunk index, ordered once.
///
/// **This exists because the first version did not have it, and that is the
/// mistake this project keeps making.** `find_chunk_record` filtered the whole
/// resident index and collected it into a fresh `Vec` on *every chunked cell
/// read* — `O(total records)` plus a heap allocation per read, behind a doc
/// comment that claimed `O(log chunks)` and "no state built at open". It is the
/// same shape [`CheckpointCadence`] (PERF2-02), [`BlockTails`] (PERF2-05) and
/// [`SegmentCursor`] each exist to remove, in a file where all three are
/// already written down.
///
/// Built at most once per handle, on the first chunked access rather than at
/// open, and extended in place when the writer seals. Lookup is a binary search
/// over memory: no walk, no allocation, no read.
///
/// Invariant: equals the `MATRIX_CHUNK_BLOCK_ID` entries of the resident index,
/// in index order, at all times.
#[derive(Debug, Default)]
struct ChunkDirectory {
    chunks: Vec<ChunkLocator>,
}

impl ChunkDirectory {
    fn find(&self, index: u64) -> Option<ChunkLocator> {
        self.chunks
            .binary_search_by_key(&index, |locator| locator.index)
            .ok()
            .map(|position| self.chunks[position])
    }

    /// The newest sealed chunk index, which is the last element because chunks
    /// are sealed in increasing order.
    fn newest(&self) -> Option<u64> {
        self.chunks.last().map(|locator| locator.index)
    }

    fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    fn note_sealed(&mut self, locator: ChunkLocator) {
        self.chunks.push(locator);
    }
}

/// The chunk a growing matrix is currently filling.
///
/// Held in memory until sealed, which is the one cost this design has and the
/// reason `rows_per_chunk` is a declared knob rather than a constant. Sealing
/// writes it as one ordinary record, so nothing else in the file format learns
/// that chunks exist.
///
/// Only the newest chunk is open. A write addressing an older one is refused
/// rather than dropped — see `Error::MatrixChunkSealed`.
#[derive(Debug)]
struct OpenChunk {
    index: u64,
    first_row: u64,
    rows: u64,
    /// How the slot regions are stored when this chunk is sealed.
    compression: Option<VariableCompression>,
    blocks: Vec<OpenChunkBlock>,
    /// Whether any cell has been committed since the chunk was opened. A chunk
    /// nothing committed is not written: an empty chunk and an absent chunk
    /// answer every read identically, and writing one would grow the file on
    /// every idle flush the way an empty segment once did.
    dirty: bool,
}

impl OpenChunk {
    fn block_position(&self, block_id: u32) -> Option<usize> {
        self.blocks
            .iter()
            .position(|block| block.block_id == block_id)
    }

    fn payload_len(&self) -> Result<u64> {
        let mut len = MATRIX_CHUNK_PREFIX_LEN
            .checked_add(
                MATRIX_CHUNK_BLOCK_DESC_LEN
                    .checked_mul(u64::try_from(self.blocks.len()).map_err(|_| {
                        Error::ResourceArithmeticOverflow {
                            resource: "matrix chunk block count",
                        }
                    })?)
                    .ok_or(Error::ResourceArithmeticOverflow {
                        resource: "matrix chunk descriptors",
                    })?,
            )
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "matrix chunk prefix",
            })?;
        for block in &self.blocks {
            let commit = u64::try_from(block.commit.len()).map_err(|_| {
                Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk commit map",
                }
            })?;
            let slots = u64::try_from(block.slots.len()).map_err(|_| {
                Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk slot region",
                }
            })?;
            let crc =
                u64::try_from(block.crc.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk checksum table",
                })?;
            let index = if self.compression.is_some() {
                sub_block_count(
                    block.cells,
                    compressed_cell_width(block.stride, !block.crc.is_empty()),
                )
                .checked_add(1)
                .and_then(|entries| entries.checked_mul(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN))
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk sub-block index",
                })?
            } else {
                0
            };
            len = len
                .checked_add(commit)
                .and_then(|len| len.checked_add(crc))
                .and_then(|len| len.checked_add(index))
                .and_then(|len| len.checked_add(slots))
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk payload length",
                })?;
        }
        Ok(len)
    }

    fn encode(&self) -> Result<Vec<u8>> {
        // The uncompressed size, which is exact when plain and an upper bound
        // when compressing. Reserving the bound is the point: a compressed
        // chunk must never need more than the ceiling `create` checked.
        let payload_len = self.payload_len()?;
        let capacity = usize::try_from(payload_len)
            .map_err(|_| Error::LengthOverflow { value: payload_len })?;
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(capacity)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix chunk payload",
                requested: payload_len,
            })?;
        let block_count =
            u32::try_from(self.blocks.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "matrix chunk block count",
            })?;
        let mut flags = if self.blocks.iter().any(|block| !block.crc.is_empty()) {
            MATRIX_CHUNK_FLAG_CELL_CRC
        } else {
            0
        };
        if self.compression.is_some() {
            flags |= MATRIX_CHUNK_FLAG_COMPRESSED_SLOTS;
        }
        payload.extend_from_slice(MATRIX_CHUNK_MAGIC);
        payload.extend_from_slice(&MATRIX_CHUNK_VERSION.to_le_bytes());
        payload.extend_from_slice(&flags.to_le_bytes());
        payload.extend_from_slice(&self.index.to_le_bytes());
        payload.extend_from_slice(&self.first_row.to_le_bytes());
        payload.extend_from_slice(&self.rows.to_le_bytes());
        payload.extend_from_slice(&block_count.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        for block in &self.blocks {
            let commit_len = u64::try_from(block.commit.len()).map_err(|_| {
                Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk commit map",
                }
            })?;
            payload.extend_from_slice(&block.block_id.to_le_bytes());
            payload.extend_from_slice(&0u32.to_le_bytes());
            payload.extend_from_slice(&block.stride.to_le_bytes());
            payload.extend_from_slice(&block.cells.to_le_bytes());
            payload.extend_from_slice(&commit_len.to_le_bytes());
        }
        for block in &self.blocks {
            payload.extend_from_slice(&block.commit);
            match self.compression {
                Some(compression) => {
                    // No separate checksum table: each cell is packed beside
                    // its own, inside the sub-block.
                    encode_compressed_slots(&mut payload, block, compression)?;
                }
                None => {
                    payload.extend_from_slice(&block.crc);
                    payload.extend_from_slice(&block.slots);
                }
            }
        }
        Ok(payload)
    }
}

/// Where one block's bytes sit inside a chunk record's payload.
///
/// Produced by reading the prefix and the descriptors — a fixed, small read
/// whatever the chunk holds — so a cell read is that read plus one `stride`-byte
/// positional read, and never the chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChunkBlockLocation {
    commit_offset: u64,
    /// `None` when the chunk carries no per-cell checksums.
    crc_offset: Option<u64>,
    slots_offset: u64,
    /// One past the last byte of this block's slot region, so a compressed
    /// sub-block's extent is bounded without consulting the record again.
    slots_end: u64,
    compressed_slots: bool,
    /// Whether cells carry a checksum at all, which a compressed block needs
    /// even though it has no separate table.
    cell_crc: bool,
    stride: u64,
    cells: u64,
}

/// O(1) writer-side state for the internal segment chain.
///
/// A segment covers the records one commit point added, so writing one needs
/// two things: where in the resident index that run begins, and where it begins
/// on disk. Both are derivable by walking back to the newest segment record,
/// and neither is derived that way — the walk would be `Theta(g)` per commit
/// point and `Theta(N)` on a file that has records but no segment record yet,
/// which is the same shape PERF2-02 and PERF2-05 removed from the flush and
/// append paths.
///
/// Invariant: at all times this equals `SegmentCursor::from_index(&index)` for
/// the current resident index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SegmentCursor {
    /// Resident-index position just past the newest segment record: the first
    /// entry the next segment covers.
    next_position: usize,
    /// File offset just past the newest segment record, or `None` when the file
    /// holds no segment record and coverage therefore starts at the append log.
    covered_start: Option<u64>,
}

impl SegmentCursor {
    fn new_empty() -> Self {
        Self {
            next_position: 0,
            covered_start: None,
        }
    }

    /// Recovers the cursor with one reverse walk that stops at the newest
    /// segment record, so a reopen or a generation rebind pays the live tail
    /// once instead of every commit point paying it again.
    ///
    /// `physical_end` saturates rather than overflowing. The entries here come
    /// from a completed scan or a validated generation, so it cannot saturate
    /// in practice; if it ever did, the next segment would record a coverage
    /// start the following open cannot match, and that open falls back to the
    /// full scan rather than trusting the chain.
    /// The rule, kept as the specification `derive_index_state` must match.
    #[cfg(test)]
    fn from_index(entries: &[RecordIndexEntry]) -> Self {
        match entries
            .iter()
            .rposition(|entry| entry.block_id == SEGMENT_BLOCK_ID)
        {
            Some(position) => Self {
                next_position: position + 1,
                covered_start: Some(entries[position].physical_end()),
            },
            None => Self::new_empty(),
        }
    }

    /// Advances the cursor for the entry just pushed at `position`.
    ///
    /// `record_end` is the physical end the append path budgeted and checked
    /// before writing, so this stays infallible on the far side of the
    /// authoritative write (INVARIANT 3).
    fn note_appended(&mut self, position: usize, block_id: u32, record_end: u64) {
        if block_id == SEGMENT_BLOCK_ID {
            self.next_position = position + 1;
            self.covered_start = Some(record_end);
        }
    }
}

/// O(1)-append / O(log B) resident block-offset tails (PERF2-05).
///
/// The append path used to reverse-scan the resident index for the newest
/// record of the same block, costing `Theta(g)` per append (`g` = distance
/// back to that record) and `O(N*B)` overall, degenerating to `Theta(N^2)`
/// when every record carries a distinct block id. This mirrors exactly what
/// that scan recomputed - the record offset of the newest entry per block id -
/// and never reads the resident index on the append path.
///
/// `B` is the number of distinct block ids present, which is bounded by the
/// format's declared block set plus varve's internal ids, so the sorted vector
/// stays tiny and stops allocating once every block id has appeared.
///
/// Invariant: at all times this equals `BlockTails::from_index(&index)` for
/// the current resident index. It is maintained at the single index append
/// site and rebuilt wherever the resident index is replaced or truncated
/// wholesale (open, recovery, generation rebinds, append rollback).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct BlockTails {
    /// Sorted by block id.
    tails: Vec<(u32, u64)>,
}

impl BlockTails {
    fn new_empty() -> Self {
        Self { tails: Vec::new() }
    }

    /// Recovers the tails with one forward pass over the resident index, so a
    /// reopen pays the pass exactly once instead of every append paying its own
    /// reverse scan.
    ///
    /// The pass keeps the newest offset per block id in a hash map and orders
    /// the distinct ids exactly once, so construction is `O(N + B log B)` time
    /// and `O(B)` memory. Replaying the pass through [`Self::note_appended`]
    /// instead - which is what this did originally - inserts every first-seen
    /// id into the sorted vector, and an index whose first appearances are in
    /// descending id order moves `0 + 1 + ... + (B - 1)` tuples. That made the
    /// real bound `O(N log B + B^2)`, not the `O(N log B)` the comment here
    /// used to claim (PERF3-03). The append path keeps the sorted-vector
    /// insertion: it pays at most one insertion per distinct id over the whole
    /// life of the file and buys `O(log B)` lookups with no hashing on the hot
    /// path.
    /// The rule, kept as the specification `derive_index_state` must match.
    #[cfg(test)]
    fn from_index(index: &[RecordIndexEntry]) -> Self {
        note_block_tail_index_touches(index.len() as u64);
        let mut newest: HashMap<u32, u64> = HashMap::new();
        for entry in index {
            newest.insert(entry.block_id, entry.record_offset);
        }
        Self::from_newest(&newest)
    }

    /// Orders an already-collected newest-per-block map.
    ///
    /// The scan collects into a map rather than replaying `note_appended`, for
    /// the reason `from_index` does: replaying inserts every first-seen id into
    /// the sorted vector, and an index whose first appearances are in descending
    /// id order moves `0 + 1 + ... + (B - 1)` tuples (PERF3-03).
    fn from_newest(newest: &HashMap<u32, u64>) -> Self {
        let mut tails: Vec<(u32, u64)> = newest.iter().map(|(id, offset)| (*id, *offset)).collect();
        tails.sort_unstable_by_key(|(block_id, _)| *block_id);
        Self { tails }
    }

    fn tail(&self, block_id: u32) -> Option<u64> {
        self.position(block_id)
            .map(|position| self.tails[position].1)
    }

    fn position(&self, block_id: u32) -> Option<usize> {
        self.tails
            .binary_search_by_key(&block_id, |(id, _)| *id)
            .ok()
    }

    /// Puts a block's tail back where it was before an append that rolled
    /// back. `None` means the block had no record yet, so the entry is removed.
    fn restore(&mut self, block_id: u32, previous: Option<u64>) {
        match (self.position(block_id), previous) {
            (Some(position), Some(offset)) => self.tails[position].1 = offset,
            (Some(position), None) => {
                self.tails.remove(position);
            }
            (None, _) => {}
        }
    }

    /// Advances the tail for the entry just pushed. Updating a block id the
    /// table already holds writes one `u64` and allocates nothing; a block id
    /// is inserted at most once per distinct id in the file.
    fn note_appended(&mut self, block_id: u32, record_offset: u64) {
        match self.tails.binary_search_by_key(&block_id, |(id, _)| *id) {
            Ok(position) => self.tails[position].1 = record_offset,
            Err(position) => {
                // Every insertion shifts the tuples above it; counting them is
                // what makes the historical quadratic construction term
                // observable to a regression test (PERF3-03).
                note_block_tail_entries_moved((self.tails.len() - position) as u64);
                self.tails.insert(position, (block_id, record_offset));
            }
        }
    }
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    static BLOCK_TAIL_INDEX_TOUCHES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Counts resident index entries examined by the block-offset-chain
/// predecessor machinery on this thread (PERF2-05 regression evidence). The
/// append path must never advance this counter. Inert without the
/// `scalable-fault-injection` feature.
#[inline]
fn note_block_tail_index_touches(count: u64) {
    #[cfg(feature = "scalable-fault-injection")]
    BLOCK_TAIL_INDEX_TOUCHES.with(|touches| touches.set(touches.get().saturating_add(count)));
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = count;
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    static BLOCK_TAIL_ENTRIES_MOVED: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Counts tuples displaced by sorted-vector insertion in the block-tail table
/// on this thread (PERF3-03 regression evidence). Index *visits* cannot see
/// this cost, which is why the quadratic construction term survived the
/// previous review. Inert without the `scalable-fault-injection` feature.
#[inline]
fn note_block_tail_entries_moved(count: u64) {
    #[cfg(feature = "scalable-fault-injection")]
    BLOCK_TAIL_ENTRIES_MOVED.with(|moved| moved.set(moved.get().saturating_add(count)));
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = count;
}

/// Lazily built resident keyed tails for the generic keyed append path
/// (API2-05).
///
/// One tail map per keyed block id. The resident file is not generic over its
/// block types, so keys are held as their canonical internal key payload -
/// the same encoding tombstone records carry - rather than as `T::Key`. A map
/// is built on first use from [`VarveFile::key_tail_offsets`] and then
/// maintained in O(1) per append, so the generic keyed entry points link
/// predecessors exactly like the generated keyed writer does. Any
/// caller-supplied predecessor (the generated writer's own path) invalidates
/// the cached map for that block id, because the file cannot learn which key
/// that record carried.
#[derive(Debug, Default)]
struct KeyedTails {
    tails: HashMap<u32, KeyedTailMap>,
}

/// One block id's tail map plus the running total of the key payload bytes it
/// owns (API3-02).
///
/// The total is maintained incrementally - one addition on the append that
/// introduces a key, nothing at all when an existing key is overwritten - so
/// charging the cache against [`ReadLimits::max_keyed_tail_bytes`] stays O(1)
/// per append. Recomputing it by summing key lengths would be Theta(distinct
/// keys) on the append hot path and is exactly what this field exists to
/// avoid.
#[derive(Debug, Default)]
struct KeyedTailMap {
    offsets: HashMap<Vec<u8>, u64>,
    key_payload_bytes: u64,
}

impl KeyedTailMap {
    /// Bytes this map is accountable for: its inline storage plus the key
    /// payload heap it owns. Excludes `HashMap` control bytes and load-factor
    /// slack, which the charge deliberately does not claim to cover.
    fn charge_for(&self, additional_entries: usize, additional_key_bytes: u64) -> Result<u64> {
        let inline = allocation_bytes::<(Vec<u8>, u64)>(
            self.offsets.len().saturating_add(additional_entries),
            "keyed tail offsets",
        )?;
        inline
            .checked_add(self.key_payload_bytes)
            .and_then(|total| total.checked_add(additional_key_bytes))
            .ok_or(Error::AllocationFailed {
                resource: "keyed tail offsets",
                requested: u64::MAX,
            })
    }
}

impl KeyedTails {
    fn new_empty() -> Self {
        Self {
            tails: HashMap::new(),
        }
    }

    fn invalidate(&mut self, block_id: u32) {
        self.tails.remove(&block_id);
    }

    fn invalidate_all(&mut self) {
        self.tails.clear();
    }
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    /// Armed pre-append keyed-tail reservation failures (API3-01).
    static INJECTED_KEYED_TAIL_RESERVATION_FAILURES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
    /// Armed post-append keyed-tail reservation losses (API3-01).
    static INJECTED_KEYED_TAIL_COMMIT_LOSSES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
    /// Armed post-commit-marker durability failures (round 10, invariant 3).
    static INJECTED_COMMIT_DURABILITY_FAILURES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Consumes one armed keyed-tail reservation failure (API3-01).
///
/// Inert without the `scalable-fault-injection` feature. Thread-local so
/// concurrently running tests cannot arm each other's writers.
#[inline]
fn take_injected_keyed_tail_reservation_failure(requested: u64) -> Result<()> {
    #[cfg(feature = "scalable-fault-injection")]
    {
        let armed = INJECTED_KEYED_TAIL_RESERVATION_FAILURES.with(|count| {
            let current = count.get();
            if current != 0 {
                count.set(current - 1);
            }
            current != 0
        });
        if armed {
            return Err(Error::AllocationFailed {
                resource: "keyed tail offsets",
                requested,
            });
        }
    }
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = requested;
    Ok(())
}

/// Consumes one armed post-append keyed-tail reservation loss (API3-01).
///
/// Models the otherwise unreachable state in which the slot reserved before
/// the append is no longer usable afterwards, so the infallible commit's
/// discard-the-cache fallback can be exercised deterministically. Inert
/// without the `scalable-fault-injection` feature.
#[inline]
fn take_injected_keyed_tail_commit_loss() -> bool {
    #[cfg(feature = "scalable-fault-injection")]
    {
        INJECTED_KEYED_TAIL_COMMIT_LOSSES.with(|count| {
            let current = count.get();
            if current != 0 {
                count.set(current - 1);
            }
            current != 0
        })
    }
    #[cfg(not(feature = "scalable-fault-injection"))]
    {
        false
    }
}

/// Consumes one armed post-commit-marker durability failure (round 10).
///
/// Models a `flush`/`fsync` that fails *after* the commit marker has been
/// appended, which a real filesystem will not produce on demand. Inert without
/// the `scalable-fault-injection` feature.
#[inline]
fn take_injected_commit_durability_failure() -> Result<()> {
    #[cfg(feature = "scalable-fault-injection")]
    {
        let armed = INJECTED_COMMIT_DURABILITY_FAILURES.with(|count| {
            let current = count.get();
            if current != 0 {
                count.set(current - 1);
            }
            current != 0
        });
        if armed {
            return Err(Error::Io(std::io::Error::other(
                "injected post-commit durability failure",
            )));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct AppendSnapshot {
    eof: u64,
    cursor: u64,
    sequence_state: SequenceState,
    index_len: usize,
    checkpoint_cadence: CheckpointCadence,
    segment_cursor: SegmentCursor,
    /// The block this append is about to move the tail of, and where that tail
    /// pointed before. Two words, so the rollback needs no allocation.
    block_id: u32,
    previous_block_tail: Option<u64>,
    uncommitted_since_commit: bool,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WriteFault {
    #[default]
    None,
    AppendAfterHeader,
    AppendAfterHeaderWithRollbackFailure,
    RollbackFailure,
    RebindAfterPublish,
    /// F-07 double fault: the parent-directory sync fails *and* the rebind
    /// that follows the same publication fails.
    ///
    /// One arming produces both, because taking the parent-sync failure re-arms
    /// the injector as [`WriteFault::RebindAfterPublish`]. That ordering is the
    /// one the code under test sees: `replace_path_atomically` syncs the parent
    /// directory before it returns, and the writer rebinds afterwards.
    ParentSyncThenRebindAfterPublish,
}

#[cfg(test)]
std::thread_local! {
    static WRITE_FAULT: std::cell::Cell<WriteFault> = const {
        std::cell::Cell::new(WriteFault::None)
    };
}

#[cfg(test)]
fn inject_write_fault(fault: WriteFault) {
    WRITE_FAULT.set(fault);
}

#[cfg(test)]
fn fail_append_after_header_if_requested() -> std::io::Result<()> {
    WRITE_FAULT.with(|fault| match fault.get() {
        WriteFault::AppendAfterHeader => {
            fault.set(WriteFault::None);
            Err(std::io::Error::other("injected append write failure"))
        }
        WriteFault::AppendAfterHeaderWithRollbackFailure => {
            fault.set(WriteFault::RollbackFailure);
            Err(std::io::Error::other("injected append write failure"))
        }
        _ => Ok(()),
    })
}

#[cfg(test)]
fn fail_rollback_if_requested() -> std::io::Result<()> {
    WRITE_FAULT.with(|fault| {
        if fault.get() == WriteFault::RollbackFailure {
            fault.set(WriteFault::None);
            Err(std::io::Error::other("injected rollback failure"))
        } else {
            Ok(())
        }
    })
}

/// F-07: injects a parent-directory sync failure and arms the rebind failure
/// that must follow it in the same publication.
#[cfg(test)]
fn fail_parent_sync_if_requested() -> Result<()> {
    WRITE_FAULT.with(|fault| {
        if fault.get() == WriteFault::ParentSyncThenRebindAfterPublish {
            fault.set(WriteFault::RebindAfterPublish);
            Err(Error::Io(std::io::Error::other(
                "injected parent-directory sync failure",
            )))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
fn fail_rebind_after_publish_if_requested() -> std::io::Result<()> {
    WRITE_FAULT.with(|fault| {
        if fault.get() == WriteFault::RebindAfterPublish {
            fault.set(WriteFault::None);
            Err(std::io::Error::other(
                "injected post-publication rebind failure",
            ))
        } else {
            Ok(())
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordHeaderFields {
    pub(crate) block_id: u32,
    pub(crate) block_version: u16,
    pub(crate) flags: u16,
    pub(crate) sequence: u64,
    pub(crate) payload_len: u64,
    pub(crate) checksum: u32,
    pub(crate) uncompressed_len_hint: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordFooterFields {
    pub(crate) prev_same_block_offset: Option<u64>,
    pub(crate) prev_same_key_offset: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaceStrategy {
    FixedCopyOnWrite,
    RewriteFile,
}

#[cfg(feature = "mmap")]
/// A read-only mapping paired with an immutable snapshot of the validated
/// append-log index.
///
/// Varve constructs this type only after mapping the already-open backing
/// object, validating every record extent against the mapped length, and
/// copying the index into the owner. Safe window methods accept only entries in
/// that copied snapshot and use checked slice bounds. The mapping owns its OS
/// mapping, so returned slices cannot outlive it.
///
/// These guarantees rely on the caller having upheld the unsafe constructor's
/// requirement that the backing object remains immutable and valid for this
/// value's full lifetime.
#[derive(Debug)]
pub struct MmapPayloads {
    spec: FormatSpec,
    // Sorted by `record_offset` (append-log order), so snapshot membership is
    // a binary search on this one copy instead of a second full HashSet copy
    // of every entry (PERF2-07).
    index: Vec<RecordIndexEntry>,
    by_block: HashMap<u32, Vec<usize>>,
    mmap: memmap2::Mmap,
}

#[cfg(feature = "mmap")]
impl MmapPayloads {
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn index_entries(&self) -> &[RecordIndexEntry] {
        &self.index
    }

    pub fn payload_window(&self, entry: &RecordIndexEntry) -> Result<&[u8]> {
        // Entries occupy disjoint physical extents, so `record_offset` is
        // unique within the snapshot: binary search finds the only candidate
        // and the full equality compare keeps the exact-match contract that
        // the former per-entry hash-set copy provided (PERF2-07).
        let position = self
            .index
            .binary_search_by_key(&entry.record_offset, |candidate| candidate.record_offset)
            .map_err(|_| Error::MmapEntryNotInSnapshot)?;
        if self.index[position] != *entry {
            return Err(Error::MmapEntryNotInSnapshot);
        }
        self.payload_window_unchecked(entry)
    }

    pub fn block_payload_window<T: VarveBlock>(&self, block_index: usize) -> Result<Option<&[u8]>> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let Some(position) = self
            .by_block
            .get(&T::ID)
            .and_then(|positions| positions.get(block_index))
            .copied()
        else {
            return Ok(None);
        };
        let entry = &self.index[position];
        if entry.block_version != T::VERSION {
            return Err(Error::BlockVersionMismatch {
                block_id: T::ID,
                expected: T::VERSION,
                actual: entry.block_version,
            });
        }
        Ok(Some(self.payload_window_unchecked(entry)?))
    }

    #[cfg(feature = "zero-copy")]
    /// Returns a raw fixed block reference directly backed by the mmap snapshot.
    ///
    /// # Safety
    ///
    /// The caller must ensure the mapped file bytes are not mutated for the
    /// lifetime of the returned reference, including by other handles,
    /// threads, or processes. The block's `VarveRawFixedBlock` implementation
    /// must also match the actual payload layout.
    pub unsafe fn raw_fixed<T: crate::VarveRawFixedBlock>(
        &self,
        block_index: usize,
    ) -> Result<Option<&T>> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        if T::KIND != BlockKind::Fixed {
            return Err(Error::ZeroCopyBlockKindMismatch { actual: T::KIND });
        }
        let expected_endian = T::ENDIAN.unwrap_or(self.spec.endian);
        if T::RAW_ENDIAN != expected_endian {
            return Err(Error::ZeroCopyEndianMismatch {
                expected: expected_endian,
                actual: T::RAW_ENDIAN,
            });
        }

        let Some(bytes) = self.block_payload_window::<T>(block_index)? else {
            return Ok(None);
        };
        let expected_len = std::mem::size_of::<T>();
        if bytes.len() != expected_len {
            return Err(Error::ZeroCopyPayloadSizeMismatch {
                expected: expected_len,
                actual: bytes.len() as u64,
            });
        }
        let required = std::mem::align_of::<T>();
        let address = bytes.as_ptr() as usize;
        if required > 1 && !address.is_multiple_of(required) {
            return Err(Error::ZeroCopyAlignmentMismatch { required, address });
        }
        T::ref_from_bytes(bytes)
            .map(Some)
            .map_err(|_| Error::ZeroCopyPayloadSizeMismatch {
                expected: expected_len,
                actual: bytes.len() as u64,
            })
    }

    fn payload_window_unchecked(&self, entry: &RecordIndexEntry) -> Result<&[u8]> {
        let start =
            usize::try_from(entry.payload_offset).map_err(|_| Error::MmapPayloadOutOfBounds {
                offset: entry.payload_offset,
                len: entry.payload_len,
            })?;
        let len =
            usize::try_from(entry.payload_len).map_err(|_| Error::MmapPayloadOutOfBounds {
                offset: entry.payload_offset,
                len: entry.payload_len,
            })?;
        let end = start
            .checked_add(len)
            .ok_or(Error::MmapPayloadOutOfBounds {
                offset: entry.payload_offset,
                len: entry.payload_len,
            })?;
        self.mmap
            .get(start..end)
            .ok_or(Error::MmapPayloadOutOfBounds {
                offset: entry.payload_offset,
                len: entry.payload_len,
            })
    }
}

#[cfg(feature = "mmap")]
/// A read-only mapping paired with an immutable snapshot of a validated matrix
/// layout.
///
/// Varve validates the complete matrix extent against the mapped length before
/// constructing this type. Safe accessors use checked slot offsets and verify
/// configured CRC evidence. The mapping owns its OS mapping, so returned slices
/// cannot outlive it.
///
/// These guarantees rely on the caller having upheld the unsafe constructor's
/// requirement that the backing object remains immutable and valid for this
/// value's full lifetime.
#[derive(Debug)]
pub struct MmapMatrix {
    spec: FormatSpec,
    layout: crate::matrix::MatrixLayout,
    mmap: memmap2::Mmap,
}

#[cfg(feature = "mmap")]
impl MmapMatrix {
    pub fn cell_payload_window<T: VarveMatrixBlock>(&self, key: MatrixKey) -> Result<&[u8]> {
        let (offset, len, crc_offset) =
            crate::matrix::cell_payload_parts::<T>(self.spec, &self.layout, key)?;
        let payload = self.window(offset, len)?;
        if let Some(crc_offset) = crc_offset {
            let expected = self.read_crc(crc_offset)?;
            crate::matrix::verify_payload_crc_bytes(offset, payload, expected)?;
        }
        Ok(payload)
    }

    pub fn cell_numeric<T, N>(&self, key: MatrixKey) -> Result<N>
    where
        T: VarveMatrixBlock,
        N: MatrixNumeric,
    {
        self.cell_numeric_at::<T, N>(key, 0)
    }

    pub fn cell_numeric_at<T, N>(&self, key: MatrixKey, byte_offset: u64) -> Result<N>
    where
        T: VarveMatrixBlock,
        N: MatrixNumeric,
    {
        let payload = self.cell_payload_window::<T>(key)?;
        let start = usize::try_from(byte_offset).map_err(|_| Error::MatrixNumericOutOfBounds {
            offset: byte_offset,
            len: N::BYTE_LEN as u64,
            payload_len: payload.len() as u64,
        })?;
        let end = start
            .checked_add(N::BYTE_LEN)
            .ok_or(Error::MatrixNumericOutOfBounds {
                offset: byte_offset,
                len: N::BYTE_LEN as u64,
                payload_len: payload.len() as u64,
            })?;
        let bytes = payload
            .get(start..end)
            .ok_or(Error::MatrixNumericOutOfBounds {
                offset: byte_offset,
                len: N::BYTE_LEN as u64,
                payload_len: payload.len() as u64,
            })?;
        Ok(N::read_matrix_numeric(
            bytes,
            T::ENDIAN.unwrap_or(self.spec.endian),
        ))
    }

    #[cfg(feature = "zero-copy")]
    /// Returns a raw matrix cell reference directly backed by the mmap snapshot.
    ///
    /// # Safety
    ///
    /// The caller must ensure the mapped file bytes are not mutated for the
    /// lifetime of the returned reference, including by other handles,
    /// threads, or processes. The block's `VarveRawMatrixBlock` implementation
    /// must also match the actual slot layout.
    pub unsafe fn raw_cell<T: crate::VarveRawMatrixBlock>(&self, key: MatrixKey) -> Result<&T> {
        if T::KIND != BlockKind::Matrix {
            return Err(Error::ZeroCopyBlockKindMismatch { actual: T::KIND });
        }
        let expected_endian = T::ENDIAN.unwrap_or(self.spec.endian);
        if T::RAW_ENDIAN != expected_endian {
            return Err(Error::ZeroCopyEndianMismatch {
                expected: expected_endian,
                actual: T::RAW_ENDIAN,
            });
        }
        let bytes = self.cell_payload_window::<T>(key)?;
        let expected_len = std::mem::size_of::<T>();
        if bytes.len() != expected_len {
            return Err(Error::ZeroCopyPayloadSizeMismatch {
                expected: expected_len,
                actual: bytes.len() as u64,
            });
        }
        let required = std::mem::align_of::<T>();
        let address = bytes.as_ptr() as usize;
        if required > 1 && !address.is_multiple_of(required) {
            return Err(Error::ZeroCopyAlignmentMismatch { required, address });
        }
        T::ref_from_bytes(bytes).map_err(|_| Error::ZeroCopyPayloadSizeMismatch {
            expected: expected_len,
            actual: bytes.len() as u64,
        })
    }

    fn window(&self, offset: u64, len: u64) -> Result<&[u8]> {
        let start =
            usize::try_from(offset).map_err(|_| Error::MmapPayloadOutOfBounds { offset, len })?;
        let len_usize =
            usize::try_from(len).map_err(|_| Error::MmapPayloadOutOfBounds { offset, len })?;
        let end = start
            .checked_add(len_usize)
            .ok_or(Error::MmapPayloadOutOfBounds { offset, len })?;
        self.mmap
            .get(start..end)
            .ok_or(Error::MmapPayloadOutOfBounds { offset, len })
    }

    fn read_crc(&self, offset: u64) -> Result<u32> {
        let bytes = self.window(offset, 4)?;
        let mut crc = [0; 4];
        crc.copy_from_slice(bytes);
        Ok(u32::from_le_bytes(crc))
    }
}

#[cfg(feature = "mmap")]
fn validate_mmap_range(offset: u64, len: u64, mapped_len: u64) -> Result<()> {
    let end = offset
        .checked_add(len)
        .ok_or(Error::MmapPayloadOutOfBounds { offset, len })?;
    if end > mapped_len {
        return Err(Error::MmapPayloadOutOfBounds { offset, len });
    }
    Ok(())
}

#[cfg(feature = "mmap")]
fn validate_mmap_index_entry(entry: &RecordIndexEntry, mapped_len: u64) -> Result<()> {
    let expected_payload_offset = entry.record_offset.checked_add(RECORD_HEADER_LEN).ok_or(
        Error::MmapPayloadOutOfBounds {
            offset: entry.record_offset,
            len: RECORD_HEADER_LEN,
        },
    )?;
    if entry.payload_offset != expected_payload_offset {
        return Err(Error::MmapPayloadOutOfBounds {
            offset: entry.record_offset,
            len: RECORD_HEADER_LEN,
        });
    }
    validate_mmap_range(entry.payload_offset, entry.payload_len, mapped_len)?;
    let payload_end = entry.payload_offset.checked_add(entry.payload_len).ok_or(
        Error::MmapPayloadOutOfBounds {
            offset: entry.payload_offset,
            len: entry.payload_len,
        },
    )?;
    if let Some(footer_offset) = entry.footer_offset {
        if footer_offset != payload_end {
            return Err(Error::MmapPayloadOutOfBounds {
                offset: footer_offset,
                len: RECORD_FOOTER_LEN,
            });
        }
        validate_mmap_range(footer_offset, RECORD_FOOTER_LEN, mapped_len)?;
    }
    let physical_end = entry
        .checked_physical_end()
        .map_err(|_| Error::MmapPayloadOutOfBounds {
            offset: entry.record_offset,
            len: u64::MAX,
        })?;
    let record_len =
        physical_end
            .checked_sub(entry.record_offset)
            .ok_or(Error::MmapPayloadOutOfBounds {
                offset: entry.record_offset,
                len: u64::MAX,
            })?;
    validate_mmap_range(entry.record_offset, record_len, mapped_len)
}

/// Seals the open chunk on the way out.
///
/// **Every other byte a writer accepts is on disk before the call returns.** A
/// chunked cell is the one exception: it lives in the open chunk until a seal.
/// Without this, dropping a writer without `flush` lost every committed cell in
/// that chunk, silently, and this library had no other way to lose committed
/// data.
///
/// Best effort, and that is a real limitation rather than a hedge: `drop`
/// cannot report a failure, so a caller who needs to know the seal succeeded
/// must call `flush`, `commit` or `sync` and read the error. What this
/// guarantees is that the ordinary case — a writer that goes out of scope —
/// does not lose data.
impl Drop for VarveFile {
    fn drop(&mut self) {
        if self.mode == OpenMode::ReadWrite && self.open_chunk.is_some() {
            let _ = self.seal_open_chunk();
        }
    }
}

#[derive(Debug)]
pub struct VarveFile {
    spec: FormatSpec,
    path: PathBuf,
    // Shape A/B, mechanical enforcement: the primary handle is not a `File`.
    // See `mod record_file` for what that forbids.
    file: RecordFile,
    snapshot: SnapshotFile,
    mode: OpenMode,
    // Shape A, mechanical enforcement: the mirror is not a `Vec`.
    // See `mod resident_index` for what that forbids.
    index: ResidentIndex,
    // The extension region this file actually carries, which is not always the
    // one `spec` would write: an unknown block is skipped at open, so a file
    // from a later release has a longer region than this build produces. Every
    // header-derived offset comes from here, and a rewrite writes it back
    // verbatim rather than regenerating it — regenerating would drop the
    // unknown block and silently move the append log.
    header_extensions: Vec<u8>,
    matrix: Option<crate::matrix::MatrixLayout>,
    // Present exactly when `matrix` is present: read once at open/create time
    // so sidecar identity checks never touch the file per operation (DUR2-03).
    matrix_creation_nonce: Option<[u8; MATRIX_CREATION_NONCE_LEN]>,
    sequence_state: SequenceState,
    // O(1) flush-cadence state for `needs_index_checkpoint`; must equal
    // `CheckpointCadence::from_index(&index)` at all times (PERF2-02).
    checkpoint_cadence: CheckpointCadence,
    // O(1) append-side block-offset-chain predecessors; must equal
    // `BlockTails::from_index(&index)` at all times (PERF2-05).
    block_tails: BlockTails,
    // The matrix chunk being filled, when a growing dimension is declared. See
    // `OpenChunk`; `None` until a write lands past the declared extent.
    open_chunk: Option<OpenChunk>,
    // Every sealed chunk, ordered once and searched in memory. Empty until the
    // first chunked access, so a handle that never touches a chunk builds
    // nothing. See `ChunkDirectory` for why this is not derived per read.
    //
    // `OnceLock` rather than `Option`, because building it happens on a read
    // path and every read entry point takes `&self` — that is a standing design
    // policy, not a convenience.
    chunk_directory: std::sync::OnceLock<ChunkDirectory>,
    // O(1) commit-point state for the internal segment chain; must equal
    // `SegmentCursor::from_index(&index)` at all times.
    segment_cursor: SegmentCursor,
    // Whether a record that a commit marker would cover has been appended since
    // the last one. Tracked here rather than read off the resident index,
    // because a non-resident block's records are not in it: deriving the answer
    // from the index made `flush` decide there was nothing to commit and skip
    // the marker, which left every such record permanently uncommitted and
    // invisible to every reader.
    uncommitted_since_commit: bool,
    // Lazily built keyed-offset-chain predecessors for the generic keyed
    // append path; a cached map is either absent or exact (API2-05).
    keyed_tails: KeyedTails,
    // True while this handle has created a pathname whose directory entry has
    // not been made durable yet. Cleared by the first successful durability
    // request, so the parent sync happens once per created file and never on
    // the append path (DUR3-01).
    pending_pathname_parent_sync: bool,
    poison: PoisonFlag,
    _lock: Option<WriterLock>,
}

#[derive(Debug)]
pub struct VarveReader {
    file: VarveFile,
}

impl VarveReader {
    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarveFile::open_readonly(spec, path)?,
        })
    }

    pub fn into_inner(self) -> VarveFile {
        self.file
    }

    pub fn spec(&self) -> FormatSpec {
        self.file.spec()
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn mode(&self) -> OpenMode {
        self.file.mode()
    }

    pub fn metadata(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.file.metadata(key)
    }

    pub fn all_metadata(&self) -> Result<Vec<(String, Vec<u8>)>> {
        self.file.all_metadata()
    }

    pub fn schema_manifest(&self) -> Result<Option<SchemaManifest>> {
        self.file.schema_manifest()
    }

    pub fn blocks<T: VarveBlock>(&self) -> Result<BlockVec<T>> {
        self.file.blocks::<T>()
    }

    pub fn blocks_migrated<From, To, M>(&self) -> Result<Vec<To>>
    where
        From: VarveBlock,
        To: VarveBlock,
        M: VarveMigration<From, To>,
    {
        self.file.blocks_migrated::<From, To, M>()
    }

    pub fn keyed_blocks<T>(&self) -> Result<KeyedBlockVec<T::Key, T>>
    where
        T: VarveKeyedBlock,
    {
        self.file.keyed_blocks::<T>()
    }

    pub fn materialized_keyed_blocks<T>(&self) -> Result<HashMap<T::Key, T>>
    where
        T: VarveMerge,
        T::Key: Eq + Hash,
    {
        self.file.materialized_keyed_blocks::<T>()
    }

    pub fn scan(&self) -> impl Iterator<Item = BlockEvent> + '_ {
        self.file.scan()
    }

    /// Verifies every record's stored checksum. See [`VarveFile::verify_all`].
    pub fn verify_all(&self) -> Result<usize> {
        self.file.verify_all()
    }

    pub fn index_entries(&self) -> IndexEntries {
        self.file.index_entries()
    }

    /// Fills the caller's buffer with the record index; see
    /// [`VarveFile::index_entries_into`].
    pub fn index_entries_into(&self, out: &mut Vec<RecordIndexEntry>) -> Result<()> {
        self.file.index_entries_into(out)
    }

    pub fn key_tail_offsets<T>(&self) -> Result<HashMap<T::Key, u64>>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.key_tail_offsets::<T>()
    }

    pub fn read_matrix_cell<T: VarveMatrixBlock>(&self, key: MatrixKey) -> Result<T> {
        self.file.read_matrix_cell(key)
    }

    pub fn matrix_cell_payload<T: VarveMatrixBlock>(&self, key: MatrixKey) -> Result<Vec<u8>> {
        self.file.matrix_cell_payload::<T>(key)
    }

    pub fn matrix_aux_len(&self, name: &str) -> Result<u64> {
        self.file.matrix_aux_len(name)
    }

    pub fn read_matrix_aux(&self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        self.file.read_matrix_aux(name, offset, len)
    }

    pub fn matrix_cell_status<T: VarveMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        self.file.matrix_cell_status::<T>(key)
    }

    pub fn is_matrix_single_committed(&self, name: &str) -> Result<bool> {
        self.file.is_matrix_single_committed(name)
    }

    pub fn is_matrix_channel_committed(&self, name: &str, channel: u64) -> Result<bool> {
        self.file.is_matrix_channel_committed(name, channel)
    }

    pub fn matrix_resume_signal(&self, category: &str) -> Result<MatrixResumeSignal> {
        self.file.matrix_resume_signal(category)
    }

    pub fn matrix_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_sidecar_resume_signal(category, sidecar_path)
    }

    pub fn matrix_verified_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_verified_sidecar_resume_signal(category, sidecar_path)
    }

    pub fn matrix_verified_sidecar_resume_signal_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_verified_sidecar_resume_signal_with_generation(
                category,
                sidecar_path,
                expected_generation,
            )
    }

    pub fn read_matrix_sidecar<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        self.file.read_matrix_sidecar(category, sidecar_path)
    }

    pub fn read_matrix_sidecar_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        self.file
            .read_matrix_sidecar_with_generation(category, sidecar_path, expected_generation)
    }

    pub fn matrix_recovery_report(&self) -> MatrixRecoveryReport {
        self.file.matrix_recovery_report()
    }

    /// Verifies this matrix's commit metadata now, and reports what was found.
    ///
    /// See [`VarveFile::verify_matrix_metadata`]: the same pass a
    /// `MatrixMetadataVerification::AtOpen` open runs, on demand, retaining one
    /// page buffer. It reports; it does not arm the quarantine.
    pub fn verify_matrix_metadata(&self) -> Result<MatrixRecoveryReport> {
        self.file.verify_matrix_metadata()
    }

    #[cfg(feature = "mmap")]
    /// Maps the file's indexed record payloads as a read-only snapshot.
    ///
    /// # Safety
    ///
    /// The caller must prevent mutation, truncation, replacement, backing-file
    /// invalidation, and any other modification through every handle, thread,
    /// and process for the full lifetime of the returned mapping.
    ///
    /// Once that condition holds, Varve keeps safe accessors sound by cloning
    /// the existing backing handle, creating a read-only mapping, validating
    /// every indexed extent with checked arithmetic, rejecting entries outside
    /// the copied snapshot, and tying every returned slice to the mapping owner.
    /// Use owned reads instead when external immutability cannot be guaranteed.
    pub unsafe fn mmap_payloads(&self) -> Result<MmapPayloads> {
        // SAFETY: The caller accepts the complete file-backed mapping contract.
        unsafe { self.file.mmap_payloads() }
    }

    #[cfg(feature = "mmap")]
    /// Maps the file's matrix regions as a read-only snapshot.
    ///
    /// # Safety
    ///
    /// The caller must prevent mutation, truncation, replacement, backing-file
    /// invalidation, and any other modification through every handle, thread,
    /// and process for the full lifetime of the returned mapping.
    ///
    /// Once that condition holds, Varve keeps safe accessors sound by cloning
    /// the existing backing handle, creating a read-only mapping, validating
    /// the complete matrix extent, checking every requested slot, and tying
    /// every returned slice to the mapping owner. Use owned reads instead when
    /// external immutability cannot be guaranteed.
    pub unsafe fn mmap_matrix(&self) -> Result<MmapMatrix> {
        // SAFETY: The caller accepts the complete file-backed mapping contract.
        unsafe { self.file.mmap_matrix() }
    }
}

#[derive(Debug)]
pub struct VarveWriter {
    file: VarveFile,
}

impl VarveWriter {
    pub fn create<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarveFile::create(spec, path)?,
        })
    }

    /// Creates a new native file, failing if `path` already exists.
    ///
    /// See [`VarveFile::create_new`].
    pub fn create_new<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarveFile::create_new(spec, path)?,
        })
    }

    pub fn create_with_dims<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<Self> {
        Ok(Self {
            file: VarveFile::create_with_dims(spec, path, dims)?,
        })
    }

    /// Creates a new native matrix file, failing if `path` already exists.
    ///
    /// See [`VarveFile::create_new_with_dims`].
    pub fn create_new_with_dims<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<Self> {
        Ok(Self {
            file: VarveFile::create_new_with_dims(spec, path, dims)?,
        })
    }

    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarveFile::open(spec, path)?,
        })
    }

    pub fn open_with_lock_policy<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        policy: WriterLockBreakPolicy,
    ) -> Result<Self> {
        Ok(Self {
            file: VarveFile::open_with_lock_policy(spec, path, policy)?,
        })
    }

    pub fn open_recover<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarveFile::open_recover(spec, path)?,
        })
    }

    pub fn open_recover_with_report<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
    ) -> Result<(Self, RecoveryReport)> {
        let (file, report) = VarveFile::open_recover_with_report(spec, path)?;
        Ok((Self { file }, report))
    }

    pub fn into_inner(self) -> VarveFile {
        self.file
    }

    pub fn spec(&self) -> FormatSpec {
        self.file.spec()
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn mode(&self) -> OpenMode {
        self.file.mode()
    }

    /// Verifies every record's stored checksum. See [`VarveFile::verify_all`].
    pub fn verify_all(&self) -> Result<usize> {
        self.file.verify_all()
    }

    pub fn index_entries(&self) -> IndexEntries {
        self.file.index_entries()
    }

    /// Fills the caller's buffer with the record index; see
    /// [`VarveFile::index_entries_into`].
    pub fn index_entries_into(&self, out: &mut Vec<RecordIndexEntry>) -> Result<()> {
        self.file.index_entries_into(out)
    }

    pub fn key_tail_offsets<T>(&self) -> Result<HashMap<T::Key, u64>>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.key_tail_offsets::<T>()
    }

    // F-01 (round 9): `VarveWriter::reserve_keyed_tail_slot` has been
    // removed.
    //
    // It existed so a generated keyed writer could reserve a slot in its own
    // `HashMap<T::Key, u64>` before the append that would fill it. That
    // reserved the map's inline table, but it could not stop the *later*
    // `HashMap::insert` from invoking the caller's `Hash` or `Eq` after the
    // record was already authoritative - a stateful or panicking
    // implementation could therefore fail after publication and leave the
    // writer holding a stale predecessor, which makes the next same-key
    // mutation link *around* the committed event.
    //
    // The generated writers no longer own a typed tail map at all. They route
    // through `VarveWriter::push_keyed_info` and `VarveWriter::delete_info`,
    // which maintain the same byte-keyed resident cache the generic keyed API
    // uses (`VarveFile::push_keyed_info`): its keys are the canonical
    // internal key payload, so the only step after the authoritative append is
    // a `HashMap<Vec<u8>, u64>` insert into a slot reserved beforehand. No
    // user-defined trait method can run there.
    //
    pub fn push<T: VarveBlock>(&mut self, block: &T) -> Result<u64> {
        self.file.push(block)
    }

    pub fn push_info<T: VarveBlock>(&mut self, block: &T) -> Result<AppendInfo> {
        self.file.push_info(block)
    }

    /// Appends a keyed block through the generic API while maintaining the
    /// keyed predecessor chain. See [`VarveFile::push_keyed`].
    pub fn push_keyed<T>(&mut self, block: &T) -> Result<u64>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.push_keyed(block)
    }

    /// Appends a keyed block through the generic API while maintaining the
    /// keyed predecessor chain. See [`VarveFile::push_keyed_info`].
    pub fn push_keyed_info<T>(&mut self, block: &T) -> Result<AppendInfo>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.push_keyed_info(block)
    }

    pub fn push_with_prev_key_info<T: VarveBlock>(
        &mut self,
        block: &T,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        self.file
            .push_with_prev_key_info(block, prev_same_key_offset)
    }

    pub fn delete<T>(&mut self, key: &T::Key) -> Result<u64>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.delete::<T>(key)
    }

    /// Appends a tombstone through the maintained keyed path. See
    /// [`VarveFile::delete_info`]. This is the entry point the generated
    /// `delete_<block>` writer methods use.
    pub fn delete_info<T>(&mut self, key: &T::Key) -> Result<AppendInfo>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.delete_info::<T>(key)
    }

    /// Builds `T`'s keyed tail map now. See [`VarveFile::prime_keyed_tails`].
    /// The generated keyed writers call this once per keyed block at
    /// construction.
    #[doc(hidden)]
    pub fn prime_keyed_tails<T>(&mut self) -> Result<()>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.prime_keyed_tails::<T>()
    }

    pub fn delete_with_prev_key_info<T>(
        &mut self,
        key: &T::Key,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo>
    where
        T: VarveKeyedBlock,
    {
        self.file
            .delete_with_prev_key_info::<T>(key, prev_same_key_offset)
    }

    pub fn push_op<T>(&mut self, key: &T::Key, op: &T::Op) -> Result<u64>
    where
        T: VarveMerge,
    {
        self.file.push_op::<T>(key, op)
    }

    pub fn write_metadata(&mut self, key: &str, value: &[u8]) -> Result<u64> {
        self.file.write_metadata(key, value)
    }

    /// Replaces a native user record while preserving its sequence number.
    ///
    /// [`Error::PublishedButRebindFailed`] means publication already succeeded.
    /// Do not retry blindly; discard this poisoned writer and reopen the path.
    ///
    /// [`Error::PublishedButParentSyncPending`] also means publication already
    /// succeeded; the writer was rebound to the published generation and stays
    /// usable, but the rename is not yet guaranteed durable against power loss
    /// until the parent directory is synced (for example by a later successful
    /// publication or an explicit directory sync).
    pub fn replace_block<T: VarveReplaceBlock>(
        &mut self,
        index: usize,
        block: &T,
    ) -> Result<ReplacementInfo> {
        self.file.replace_block(index, block)
    }

    /// Publishes a same-size fixed replacement through a copy-on-write file
    /// generation.
    ///
    /// [`Error::PublishedButRebindFailed`] means publication already succeeded.
    /// Do not retry blindly; discard this poisoned writer and reopen the path.
    ///
    /// [`Error::PublishedButParentSyncPending`] also means publication already
    /// succeeded; the writer was rebound to the published generation and stays
    /// usable, but the rename is not yet guaranteed durable against power loss
    /// until the parent directory is synced (for example by a later successful
    /// publication or an explicit directory sync).
    pub fn replace_fixed<T: VarveBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        self.file.replace_fixed(index, block)
    }

    /// Replaces a fixed record by mutating the backing object directly.
    ///
    /// # Errors
    ///
    /// See [`VarveFile::replace_fixed_in_place_exclusive`]: a stored record
    /// whose version differs from `T::VERSION` is refused with
    /// [`Error::BlockVersionMismatch`] (F-01).
    ///
    /// # Safety
    ///
    /// See [`VarveFile::replace_fixed_in_place_exclusive`] for the full
    /// contract, including the requirement that a keyed record's key must not
    /// change (F-02).
    ///
    /// The caller must exclude every reader, writer, mapping, raw reference,
    /// handle, thread, and process for this operation and for the lifetime of
    /// every view that could observe the affected object.
    pub unsafe fn replace_fixed_in_place_exclusive<T: VarveBlock>(
        &mut self,
        index: usize,
        block: &T,
    ) -> Result<u64> {
        // SAFETY: The caller accepts the exclusivity contract documented above.
        unsafe { self.file.replace_fixed_in_place_exclusive(index, block) }
    }

    /// Publishes a replacement by rewriting the complete file generation.
    ///
    /// [`Error::PublishedButRebindFailed`] means publication already succeeded.
    /// Do not retry blindly; discard this poisoned writer and reopen the path.
    ///
    /// [`Error::PublishedButParentSyncPending`] also means publication already
    /// succeeded; the writer was rebound to the published generation and stays
    /// usable, but the rename is not yet guaranteed durable against power loss
    /// until the parent directory is synced (for example by a later successful
    /// publication or an explicit directory sync).
    pub fn replace_rewrite<T: VarveBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        self.file.replace_rewrite(index, block)
    }

    pub fn replace<T: VarveBlock>(
        &mut self,
        index: usize,
        block: &T,
        strategy: ReplaceStrategy,
    ) -> Result<u64> {
        self.file.replace(index, block, strategy)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()
    }

    pub fn commit(&mut self) -> Result<AppendInfo> {
        self.file.commit()
    }

    pub fn commit_durable(&mut self) -> Result<AppendInfo> {
        self.file.commit_durable()
    }

    pub fn sync(&mut self) -> Result<()> {
        self.file.sync()
    }

    pub fn write_matrix_cell<T: VarveMatrixBlock>(
        &mut self,
        key: MatrixKey,
        value: &T,
    ) -> Result<()> {
        self.file.write_matrix_cell(key, value)
    }

    /// Copies one cell's bytes out of `source`.
    ///
    /// `source` is a **shared** borrow: the read side of a byte copy needs no
    /// exclusive handle, so a copy may run while other threads are reading the
    /// same source handle. Callers that were passing `&mut VarveReader` still
    /// compile — `&mut T` coerces to `&T`.
    pub fn copy_matrix_cell_bytes_from<From, To>(
        &mut self,
        source: &VarveReader,
        key: MatrixKey,
    ) -> Result<()>
    where
        From: VarveMatrixBlock,
        To: VarveMatrixBlock,
    {
        ensure_matrix_byte_copy_compatible::<From, To>()?;
        let payload = source.matrix_cell_payload::<From>(key)?;
        self.file.write_matrix_cell_payload::<To>(key, &payload)?;
        self.file.commit_matrix_cell::<To>(key)
    }

    /// Writes, syncs, commits, syncs the commit, then calls `hook`.
    ///
    /// The hook runs **after** the cell is committed and durable. If it
    /// returns an error the write is *not* rolled back: the failure is
    /// reported as [`Error::MatrixCommittedButHookFailed`], carrying the
    /// [`MatrixCommitEvent`] the hook was given, so a caller can retry the
    /// notification alone. Retrying the whole call instead repeats the durable
    /// write and re-runs the hook.
    ///
    /// If the durability request that follows the commit fails, the cell is
    /// still committed and the hook is not run; that is reported as
    /// [`Error::MatrixCommittedButDurabilityUnproven`], also carrying the
    /// event.
    ///
    /// Those two variants are the only published outcomes of this call. Every
    /// other error variant means the cell was not committed by this call.
    ///
    /// That statement is about the value this call **returns** (F-06). Both
    /// variants box their event and their source, so building either one
    /// allocates after the cell is already authoritative; those allocations
    /// are shape-sized rather than content-sized, and the crate allocates
    /// shape-sized memory infallibly. If the allocator refuses one, the
    /// process terminates rather than returning some other error, so no caller
    /// observes a *different* outcome — the cell is committed, and a reader
    /// that opens the file afterwards sees it. See "Allocator Failure And
    /// Published Outcomes" in `docs/durability-model.md`.
    pub fn write_matrix_cell_durable<T, F>(
        &mut self,
        key: MatrixKey,
        value: &T,
        hook: F,
    ) -> Result<()>
    where
        T: VarveMatrixBlock,
        F: FnOnce(MatrixCommitEvent) -> Result<()>,
    {
        self.file.write_matrix_cell_durable(key, value, hook)
    }

    /// [`Self::write_matrix_cell_durable`] with a caller-supplied durability
    /// barrier; the same post-commit contract applies, including
    /// [`Error::MatrixCommittedButHookFailed`] and
    /// [`Error::MatrixCommittedButDurabilityUnproven`].
    pub fn write_matrix_cell_durable_with_barrier<T, B, F>(
        &mut self,
        key: MatrixKey,
        value: &T,
        barrier: &mut B,
        hook: F,
    ) -> Result<()>
    where
        T: VarveMatrixBlock,
        B: MatrixDurabilityBarrier + ?Sized,
        F: FnOnce(MatrixCommitEvent) -> Result<()>,
    {
        self.file
            .write_matrix_cell_durable_with_barrier(key, value, barrier, hook)
    }

    pub fn read_matrix_cell<T: VarveMatrixBlock>(&self, key: MatrixKey) -> Result<T> {
        self.file.read_matrix_cell(key)
    }

    pub fn matrix_cell_payload<T: VarveMatrixBlock>(&self, key: MatrixKey) -> Result<Vec<u8>> {
        self.file.matrix_cell_payload::<T>(key)
    }

    pub fn matrix_aux_len(&self, name: &str) -> Result<u64> {
        self.file.matrix_aux_len(name)
    }

    pub fn read_matrix_aux(&self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        self.file.read_matrix_aux(name, offset, len)
    }

    pub fn write_matrix_aux(&mut self, name: &str, offset: u64, payload: &[u8]) -> Result<()> {
        self.file.write_matrix_aux(name, offset, payload)
    }

    pub fn matrix_cell_status<T: VarveMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        self.file.matrix_cell_status::<T>(key)
    }

    pub fn commit_matrix_cell<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        self.file.commit_matrix_cell::<T>(key)
    }

    pub fn clear_matrix_cell<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        self.file.clear_matrix_cell::<T>(key)
    }

    pub fn clear_matrix_cell_by_category(&mut self, category: &str, key: MatrixKey) -> Result<()> {
        self.file.clear_matrix_cell_by_category(category, key)
    }

    pub fn clear_matrix_category(&mut self, category: &str) -> Result<u64> {
        self.file.clear_matrix_category(category)
    }

    pub fn apply_matrix_recovery_action(&mut self, action: &MatrixRecoveryAction) -> Result<()> {
        self.file.apply_matrix_recovery_action(action)
    }

    pub fn rebuild_matrix_commit_from_crc<T: VarveMatrixBlock>(&mut self) -> Result<u64> {
        self.file.rebuild_matrix_commit_from_crc::<T>()
    }

    pub fn is_matrix_single_committed(&self, name: &str) -> Result<bool> {
        self.file.is_matrix_single_committed(name)
    }

    pub fn set_matrix_single_committed(&mut self, name: &str, value: bool) -> Result<()> {
        self.file.set_matrix_single_committed(name, value)
    }

    pub fn is_matrix_channel_committed(&self, name: &str, channel: u64) -> Result<bool> {
        self.file.is_matrix_channel_committed(name, channel)
    }

    pub fn set_matrix_channel_committed(
        &mut self,
        name: &str,
        channel: u64,
        value: bool,
    ) -> Result<()> {
        self.file.set_matrix_channel_committed(name, channel, value)
    }

    pub fn matrix_resume_signal(&self, category: &str) -> Result<MatrixResumeSignal> {
        self.file.matrix_resume_signal(category)
    }

    pub fn matrix_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_sidecar_resume_signal(category, sidecar_path)
    }

    pub fn matrix_verified_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_verified_sidecar_resume_signal(category, sidecar_path)
    }

    pub fn matrix_verified_sidecar_resume_signal_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_verified_sidecar_resume_signal_with_generation(
                category,
                sidecar_path,
                expected_generation,
            )
    }

    pub fn write_matrix_sidecar<P: AsRef<Path>>(
        &mut self,
        category: &str,
        sidecar_path: P,
        generation: u64,
        payload: &[u8],
    ) -> Result<MatrixSidecarManifest> {
        self.file
            .write_matrix_sidecar(category, sidecar_path, generation, payload)
    }

    pub fn read_matrix_sidecar<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        self.file.read_matrix_sidecar(category, sidecar_path)
    }

    pub fn read_matrix_sidecar_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        self.file
            .read_matrix_sidecar_with_generation(category, sidecar_path, expected_generation)
    }

    pub fn matrix_recovery_report(&self) -> MatrixRecoveryReport {
        self.file.matrix_recovery_report()
    }

    /// Verifies this matrix's commit metadata now, and reports what was found.
    ///
    /// See [`VarveFile::verify_matrix_metadata`]: the same pass a
    /// `MatrixMetadataVerification::AtOpen` open runs, on demand, retaining one
    /// page buffer. It reports; it does not arm the quarantine.
    pub fn verify_matrix_metadata(&self) -> Result<MatrixRecoveryReport> {
        self.file.verify_matrix_metadata()
    }
}

/// The witness this file writer's disk-touching methods demand, typed by the
/// writer it speaks for. See `crate::writer_permit`.
pub(crate) type FileMutationPermit = MutationPermit<VarveFile>;

impl GuardedWriter for VarveFile {
    fn poison_flag(&self) -> &PoisonFlag {
        &self.poison
    }
}

impl VarveFile {
    pub fn create<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Self::create_impl(spec, path.as_ref(), false)
    }

    /// Creates a new native file, failing if `path` already exists.
    ///
    /// Unlike [`VarveFile::create`], this never truncates or reuses an
    /// existing file, so callers that must prove they created and own the
    /// file (for example diagnostic scaffolding) cannot destroy caller data.
    pub fn create_new<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Self::create_impl(spec, path.as_ref(), true)
    }

    fn create_impl(spec: FormatSpec, path: &Path, exclusive: bool) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_write_limits(spec)?;
        check_initial_native_file_len(spec)?;
        if spec.has_matrix_blocks() {
            return Err(Error::MatrixDimensionsRequired);
        }
        let path = path.to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        // Everything past the acquisition runs inside `with_writer_lock`, so a
        // failure gives the claim back before the error propagates instead of at
        // scope exit; see `release_writer_lock_after_failure`.
        with_writer_lock(lock, |lock| {
            Self::create_locked(spec, path, exclusive, lock)
        })
    }

    fn create_locked(
        spec: FormatSpec,
        path: PathBuf,
        exclusive: bool,
        lock: &mut WriterLock,
    ) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        if exclusive {
            options.create_new(true);
        } else {
            options.create(true);
        }
        let mut file = options.open(&path)?;
        // DUR2-05: bind the single-writer object lock before any destructive
        // initialization. Opening with `.truncate(true)` would clear the new
        // object inside the pre-bind window where a concurrent creator that
        // lost the race could still hold an unbound handle to it.
        lock.bind_native(&file, &path)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let header_extensions = write_file_header(spec, &mut file)?;
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        let mut file = Self {
            spec,
            path,
            file: RecordFile::new(file),
            snapshot,
            mode: OpenMode::ReadWrite,
            index: ResidentIndex::adopt_generation(Vec::new()),
            header_extensions,
            matrix: None,
            matrix_creation_nonce: None,
            sequence_state: SequenceState::Available(0),
            checkpoint_cadence: CheckpointCadence::new_empty(),
            block_tails: BlockTails::new_empty(),
            segment_cursor: SegmentCursor::new_empty(),
            uncommitted_since_commit: false,
            keyed_tails: KeyedTails::new_empty(),
            // DUR3-01: this handle established the pathname, so its
            // directory entry is not durable until the first durability
            // request syncs the parent directory.
            open_chunk: None,
            chunk_directory: std::sync::OnceLock::new(),
            pending_pathname_parent_sync: true,
            poison: PoisonFlag::healthy(),
            _lock: None,
        };
        file.write_embedded_manifest_if_needed()?;
        Ok(file)
    }

    pub fn create_with_dims<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<Self> {
        Self::create_with_dims_impl(spec, path.as_ref(), dims, false)
    }

    /// Creates a new native matrix file, failing if `path` already exists.
    ///
    /// Unlike [`VarveFile::create_with_dims`], this never reuses or truncates
    /// an existing file object: the exclusive create *is* the ownership claim,
    /// and the same handle is kept locked from that claim through the complete
    /// matrix initialization. Diagnostic scaffolding (the format self-test)
    /// must use this instead of claiming the pathname with a separate handle
    /// and then re-opening it by name (API2-02).
    pub fn create_new_with_dims<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<Self> {
        Self::create_with_dims_impl(spec, path.as_ref(), dims, true)
    }

    fn create_with_dims_impl(
        spec: FormatSpec,
        path: &Path,
        dims: MatrixDimensions,
        exclusive: bool,
    ) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_write_limits(spec)?;
        check_initial_native_file_len(spec)?;
        if !spec.has_matrix_blocks() {
            return Self::create_impl(spec, path, exclusive);
        }
        // Chunk 0 *is* the matrix region, and `chunk_for_row` divides by
        // `rows_per_chunk` to find it. If the declared extent were any other
        // value the two would disagree: with an extent of 100 and chunks of 4,
        // row 50 routes to chunk 12 while the region already holds it, and the
        // region's rows 4..100 become unreachable. Refused at create, where the
        // dimension value first exists.
        if let Some(growing) = spec.growing_matrix {
            let declared = dims
                .get(growing.name)
                .ok_or_else(|| Error::MatrixDimensionMissing(growing.name.to_string()))?;
            if declared != growing.rows_per_chunk {
                return Err(Error::MatrixSizeMismatch {
                    expected: growing.rows_per_chunk,
                    actual: declared,
                });
            }
            // A chunk is buffered against `MatrixSlotRegionLen` and sealed
            // against `RecordPayloadLen`, and nothing reconciled them: a spec
            // could pass `validate`, accept writes, and then die at the first
            // seal with the data already in RAM and no way to get it out.
            // Refused here, before a byte is accepted.
            let sealed = chunk_payload_len_for(spec, &dims)?;
            spec.read_limits
                .check(ReadLimitKey::RecordPayloadLen, sealed)?;
            spec.read_limits
                .check(ReadLimitKey::MatrixSlotRegionLen, sealed)?;
        }
        let path = path.to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        // As in `create_impl`: a failure past the acquisition gives the claim
        // back before the error propagates.
        with_writer_lock(lock, |lock| {
            Self::create_with_dims_locked(spec, path, dims, exclusive, lock)
        })
    }

    fn create_with_dims_locked(
        spec: FormatSpec,
        path: PathBuf,
        dims: MatrixDimensions,
        exclusive: bool,
        lock: &mut WriterLock,
    ) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        if exclusive {
            options.create_new(true);
        } else {
            options.create(true);
        }
        let mut file = options.open(&path)?;
        // DUR2-05: bind the single-writer object lock before any destructive
        // initialization; see `create_impl`.
        lock.bind_native(&file, &path)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let header_extensions = write_file_header(spec, &mut file)?;
        // DUR2-03: stamp a fresh creation nonce so this logical matrix is
        // distinguishable from any earlier matrix that lived in the same OS
        // file object at the same layout offsets.
        let creation_nonce = fresh_matrix_creation_nonce();
        write_matrix_creation_nonce_region(&mut file, creation_nonce)?;
        let header_len = file.stream_position()?;
        let matrix = crate::matrix::create_layout(spec, &mut file, header_len, &dims)?;
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        let mut file = Self {
            spec,
            path,
            file: RecordFile::new(file),
            snapshot,
            mode: OpenMode::ReadWrite,
            index: ResidentIndex::adopt_generation(Vec::new()),
            header_extensions,
            matrix: Some(matrix),
            matrix_creation_nonce: Some(creation_nonce),
            sequence_state: SequenceState::Available(0),
            checkpoint_cadence: CheckpointCadence::new_empty(),
            block_tails: BlockTails::new_empty(),
            segment_cursor: SegmentCursor::new_empty(),
            uncommitted_since_commit: false,
            keyed_tails: KeyedTails::new_empty(),
            // DUR3-01: this handle established the pathname, so its
            // directory entry is not durable until the first durability
            // request syncs the parent directory.
            open_chunk: None,
            chunk_directory: std::sync::OnceLock::new(),
            pending_pathname_parent_sync: true,
            poison: PoisonFlag::healthy(),
            _lock: None,
        };
        file.write_embedded_manifest_if_needed()?;
        Ok(file)
    }

    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        // Every way this open can fail - a rejected header, a version or schema
        // mismatch, a read-limit refusal, a corrupt tail - happens after the
        // claim was taken, so all of them release it here. See
        // `with_writer_lock`.
        with_writer_lock(lock, |lock| Self::open_locked(spec, path, lock))
    }

    fn open_locked(spec: FormatSpec, path: PathBuf, lock: &mut WriterLock) -> Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        lock.bind_native(&file, &path)?;
        let captured_len = check_open_file_len(spec, &file)?;
        let (header_len, header_extensions) = read_file_header_parts(spec, &mut file)?;
        let (matrix, matrix_creation_nonce) = split_matrix_state(read_matrix_layout_if_needed(
            spec,
            &mut file,
            header_len,
            captured_len,
        )?);
        let append_start = append_log_start(header_len, matrix.as_ref());
        let index = load_index(spec, &mut file, append_start, ScanIntent::Writer)?;
        // The sequence high-water mark and the block tails come from the scan
        // rather than from the index, because the index is filtered: a
        // non-resident block's records are on disk and not in it.
        let sequence_state = index.sequence_state();
        let block_tails = index.block_tails();
        let index = index.entries;
        truncate_uncommitted_tail_if_needed(spec, &mut file, append_start, &index)?;
        let derived = derive_index_state(index.iter(), false);
        let checkpoint_cadence = derived.checkpoint_cadence;
        let segment_cursor = derived.segment_cursor;
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        Ok(Self {
            spec,
            path,
            file: RecordFile::new(file),
            snapshot,
            mode: OpenMode::ReadWrite,
            index: ResidentIndex::adopt_generation(index),
            header_extensions,
            matrix,
            matrix_creation_nonce,
            sequence_state,
            checkpoint_cadence,
            block_tails,
            open_chunk: None,
            chunk_directory: std::sync::OnceLock::new(),
            segment_cursor,
            uncommitted_since_commit: false,
            keyed_tails: KeyedTails::new_empty(),
            pending_pathname_parent_sync: false,
            poison: PoisonFlag::healthy(),
            _lock: None,
        })
    }

    pub fn open_with_lock_policy<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        policy: WriterLockBreakPolicy,
    ) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        let lock = WriterLock::acquire_with_policy(&path, policy)?;
        // A broken stale claim that then fails to open must not become a new
        // stale claim of this process's own making.
        with_writer_lock(lock, |lock| Self::open_locked(spec, path, lock))
    }

    pub fn open_readonly<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new().read(true).open(&path)?;
        let captured_len = check_open_file_len(spec, &file)?;
        let (header_len, header_extensions) = read_file_header_parts(spec, &mut file)?;
        let (matrix, matrix_creation_nonce) = split_matrix_state(read_matrix_layout_if_needed(
            spec,
            &mut file,
            header_len,
            captured_len,
        )?);
        let append_start = append_log_start(header_len, matrix.as_ref());
        let scanned = load_index(spec, &mut file, append_start, ScanIntent::ReadOnly)?;
        let sequence_state = scanned.sequence_state();
        let block_tails = scanned.block_tails();
        // The snapshot comes from the scan, not from the resident list. The
        // list is filtered — a non-resident block's records are on disk and not
        // in it, exactly as `open_locked`'s comment says — so binding the
        // snapshot to its last entry stopped a read-only handle at the last
        // *resident* record. Under a markerless policy that ends on a
        // non-resident block, that is before every record of that block, and
        // `block_chain`, the published way to reach one, walks through the
        // snapshot.
        //
        // It is still not the file's physical length: a torn trailing record
        // must leave the snapshot at the end of the last complete record, and
        // under a marker policy the uncommitted tail a writer open would delete
        // must not be visible here either. Both facts belong to the scan.
        let logical_len = scanned.physical_end(append_start);
        let index = scanned.entries;
        let derived = derive_index_state(index.iter(), false);
        let checkpoint_cadence = derived.checkpoint_cadence;
        let segment_cursor = derived.segment_cursor;
        let snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, logical_len)?;
        Ok(Self {
            spec,
            path,
            file: RecordFile::new(file),
            snapshot,
            mode: OpenMode::ReadOnly,
            index: ResidentIndex::adopt_generation(index),
            header_extensions,
            matrix,
            matrix_creation_nonce,
            sequence_state,
            checkpoint_cadence,
            block_tails,
            open_chunk: None,
            chunk_directory: std::sync::OnceLock::new(),
            segment_cursor,
            uncommitted_since_commit: false,
            keyed_tails: KeyedTails::new_empty(),
            pending_pathname_parent_sync: false,
            poison: PoisonFlag::healthy(),
            _lock: None,
        })
    }

    pub fn open_recover<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self::open_recover_with_report(spec, path)?.0)
    }

    pub fn open_recover_with_report<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
    ) -> Result<(Self, RecoveryReport)> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        // This shape is a pair, not a bare `VarveFile`, so it uses
        // `with_writer_lock_value` and installs the claim itself.
        let ((mut file, report), lock) =
            with_writer_lock_value(lock, |lock| Self::open_recover_locked(spec, path, lock))?;
        file._lock = Some(lock);
        Ok((file, report))
    }

    fn open_recover_locked(
        spec: FormatSpec,
        path: PathBuf,
        lock: &mut WriterLock,
    ) -> Result<(Self, RecoveryReport)> {
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        lock.bind_native(&file, &path)?;
        let original_len = file.metadata()?.len();
        let (header_len, header_extensions) = read_file_header_parts(spec, &mut file)?;
        let (matrix, matrix_creation_nonce) = split_matrix_state(read_matrix_layout_if_needed(
            spec,
            &mut file,
            header_len,
            original_len,
        )?);
        let append_start = append_log_start(header_len, matrix.as_ref());
        let index = load_index(spec, &mut file, append_start, ScanIntent::Recover)?;
        let sequence_state = index.sequence_state();
        let block_tails = index.block_tails();
        let index = index.entries;
        truncate_uncommitted_tail_if_needed(spec, &mut file, append_start, &index)?;
        let recovered_len = file.metadata()?.len();
        let derived = derive_index_state(index.iter(), false);
        let checkpoint_cadence = derived.checkpoint_cadence;
        let segment_cursor = derived.segment_cursor;
        let records_preserved = index.len();
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        Ok((
            Self {
                spec,
                path,
                file: RecordFile::new(file),
                snapshot,
                mode: OpenMode::ReadWrite,
                index: ResidentIndex::adopt_generation(index),
                header_extensions,
                matrix,
                matrix_creation_nonce,
                sequence_state,
                checkpoint_cadence,
                block_tails,
                open_chunk: None,
                chunk_directory: std::sync::OnceLock::new(),
                segment_cursor,
                uncommitted_since_commit: false,
                keyed_tails: KeyedTails::new_empty(),
                pending_pathname_parent_sync: false,
                poison: PoisonFlag::healthy(),
                _lock: None,
            },
            RecoveryReport {
                original_len,
                recovered_len,
                records_preserved,
            },
        ))
    }

    pub fn spec(&self) -> FormatSpec {
        self.spec.ordinary_read()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn snapshot(&self) -> &SnapshotFile {
        &self.snapshot
    }

    pub fn mode(&self) -> OpenMode {
        self.mode
    }

    pub fn push<T: VarveBlock>(&mut self, block: &T) -> Result<u64> {
        Ok(self.push_info(block)?.sequence)
    }

    /// Appends a block without a keyed predecessor link.
    ///
    /// API2-05: a keyed block cannot go through this entry point when the
    /// format enables `keyed_offset_chain`. `T: VarveBlock` exposes no key, so
    /// this path can only write `prev_same_key_offset = None`, which silently
    /// truncates the physical keyed chain as soon as a second record shares a
    /// key. Such calls are rejected with
    /// [`Error::KeyedChainRequiresKeyedApi`]; use the generated keyed writer
    /// method or [`VarveFile::push_keyed_info`], both of which maintain the
    /// chain. The bounded streaming and indexed writers already reject the
    /// same combination.
    pub fn push_info<T: VarveBlock>(&mut self, block: &T) -> Result<AppendInfo> {
        if self.spec.index_policy.keyed_offset_chain && T::IS_KEYED {
            return Err(Error::KeyedChainRequiresKeyedApi { block_id: T::ID });
        }
        self.push_with_prev_key_info(block, None)
    }

    /// Appends a keyed block through the generic API, linking it to the
    /// previous record with the same key.
    ///
    /// This is the generic equivalent of the generated `push_<block>` writer
    /// method. The predecessor comes from a per-block-id tail map that is
    /// built once from the resident index on first use for a given block type
    /// (`O(N)` decode of that block's records, exactly what the generated
    /// writer pays at open) and then maintained in O(1) per append. Formats
    /// without `keyed_offset_chain` skip the map entirely.
    ///
    /// Interleaving this with [`VarveFile::push_with_prev_key_info`] for the
    /// same block id is supported but drops the cached map: a caller-supplied
    /// link carries no key the file can observe, so the next call here rebuilds.
    pub fn push_keyed<T>(&mut self, block: &T) -> Result<u64>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        Ok(self.push_keyed_info(block)?.sequence)
    }

    /// Appends a keyed block through the generic API, linking it to the
    /// previous record with the same key. See [`VarveFile::push_keyed`].
    pub fn push_keyed_info<T>(&mut self, block: &T) -> Result<AppendInfo>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        // API2-03: keyed generic entry points evaluate the compile-time
        // keyedness contract post-monomorphization.
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        if !self.spec.index_policy.keyed_offset_chain {
            return self.push_with_prev_key_info_unlinked(block, None);
        }
        let _permit = self.ensure_write()?;
        let key = encode_internal_key_payload::<T>(self.spec, &block.key())?;
        // API3-01: every fallible part of the tail-cache update happens here,
        // strictly before the append that the update describes. See
        // [`VarveFile::reserve_keyed_tail_slot`].
        let previous = self.reserve_keyed_tail_slot::<T>(&key)?;
        let info = self.push_with_prev_key_info_unlinked(block, previous)?;
        self.commit_keyed_tail(T::ID, key, info.record_offset);
        Ok(info)
    }

    /// Returns the maintained keyed tails for `T`, building them from the
    /// resident index the first time this block type is used.
    fn keyed_tail_map<T>(&mut self) -> Result<&mut KeyedTailMap>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        if !self.keyed_tails.tails.contains_key(&T::ID) {
            let spec = self.spec;
            let typed = self.key_tail_offsets::<T>()?;
            // API3-05: `typed` stays alive for the whole transcoding loop
            // below, so every charge taken here adds its inline storage to the
            // map being built. The charge therefore gates the *peak* of the
            // build, not just what is retained afterwards.
            let typed_inline =
                allocation_bytes::<(T::Key, u64)>(typed.len(), "keyed tail offsets")?;
            let requested = allocation_bytes::<(Vec<u8>, u64)>(typed.len(), "keyed tail offsets")?;
            let mut built = KeyedTailMap::default();
            spec.read_limits.check(
                ReadLimitKey::KeyedTailBytes,
                typed_inline
                    .checked_add(requested)
                    .ok_or(Error::AllocationFailed {
                        resource: "keyed tail offsets",
                        requested: u64::MAX,
                    })?,
            )?;
            built
                .offsets
                .try_reserve(typed.len())
                .map_err(|_| Error::AllocationFailed {
                    resource: "keyed tail offsets",
                    requested,
                })?;
            for (key, offset) in &typed {
                let payload = encode_internal_key_payload::<T>(spec, key)?;
                // The key payload heap is charged as it accumulates, one
                // payload at a time. The single payload just encoded is
                // already allocated when it is charged - it is bounded by the
                // per-key limits that governed the record it came from - but
                // no *second* payload is taken until this one is admitted.
                let live = typed_inline
                    .checked_add(requested)
                    .and_then(|total| total.checked_add(built.key_payload_bytes))
                    .and_then(|total| total.checked_add(payload.len() as u64))
                    .ok_or(Error::AllocationFailed {
                        resource: "keyed tail offsets",
                        requested: u64::MAX,
                    })?;
                spec.read_limits.check(ReadLimitKey::KeyedTailBytes, live)?;
                built.key_payload_bytes =
                    built.key_payload_bytes.saturating_add(payload.len() as u64);
                built.offsets.insert(payload, *offset);
            }
            // API3-02: the completed map is charged to the same runtime limit
            // the incremental growth is charged to, so a file whose distinct
            // keys alone exceed the configured keyed-tail budget is refused
            // here rather than retained.
            spec.read_limits
                .check(ReadLimitKey::KeyedTailBytes, built.charge_for(0, 0)?)?;
            let requested = allocation_bytes::<(u32, KeyedTailMap)>(
                self.keyed_tails.tails.len().saturating_add(1),
                "keyed tail cache",
            )?;
            self.keyed_tails
                .tails
                .try_reserve(1)
                .map_err(|_| Error::AllocationFailed {
                    resource: "keyed tail cache",
                    requested,
                })?;
            self.keyed_tails.tails.insert(T::ID, built);
        }
        Ok(self
            .keyed_tails
            .tails
            .get_mut(&T::ID)
            .expect("keyed tail map for this block id was just installed"))
    }

    /// Resolves `key`'s current predecessor *and* reserves the tail-cache slot
    /// the append that follows will occupy (API3-01).
    ///
    /// The generic keyed mutation paths used to append first and only then
    /// grow the resident tail cache. That ordering could return `Err` from a
    /// cache reservation after the record or tombstone was already written,
    /// indexed and published in writer state: the caller read "nothing
    /// happened" while the record existed, and - worse - the stale cached
    /// predecessor survived, so the next generic keyed mutation on the same
    /// writer linked *around* the record that had in fact succeeded, silently
    /// truncating the physical keyed chain.
    ///
    /// Every fallible step - building the map from the resident index,
    /// the allocation arithmetic, and the `try_reserve` - therefore happens
    /// here, before the append. Only the infallible
    /// [`VarveFile::commit_keyed_tail`] runs afterwards, so a generic keyed
    /// mutation can no longer fail after it has become authoritative.
    ///
    /// A key already present needs no capacity: its slot is overwritten in
    /// place.
    ///
    /// API3-02: the growth is additionally *charged*, before it is taken,
    /// against [`ReadLimits::max_keyed_tail_bytes`]. `try_reserve` alone only
    /// reports an allocation the allocator refuses; the charge is what makes a
    /// large-but-satisfiable cache refusable by configured policy.
    ///
    /// The charged value is the map's inline storage,
    /// `(len + 1) * size_of::<(Vec<u8>, u64)>()`, plus the key payload bytes
    /// the map owns - here the payloads already resident plus the one about to
    /// be inserted. It is checked per keyed block id, not summed across block
    /// ids, and it excludes `HashMap` control bytes and load-factor slack.
    fn reserve_keyed_tail_slot<T>(&mut self, key: &[u8]) -> Result<Option<u64>>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        let spec = self.spec;
        let tails = self.keyed_tail_map::<T>()?;
        if let Some(previous) = tails.offsets.get(key).copied() {
            return Ok(Some(previous));
        }
        let requested = tails.charge_for(1, key.len() as u64)?;
        // API3-02: charge the growth to a runtime resource limit, not only
        // to the allocator. `try_reserve` alone turns an *impossible*
        // allocation into a typed error but places no policy ceiling on a
        // large-but-satisfiable cache, so a file with many distinct keys could
        // grow the resident tail cache without any configured bound refusing
        // it.
        spec.read_limits
            .check(ReadLimitKey::KeyedTailBytes, requested)?;
        take_injected_keyed_tail_reservation_failure(requested)?;
        tails
            .offsets
            .try_reserve(1)
            .map_err(|_| Error::AllocationFailed {
                resource: "keyed tail offsets",
                requested,
            })?;
        Ok(None)
    }

    /// Records `key`'s new tail after the append became authoritative
    /// (API3-01). Infallible by construction.
    ///
    /// [`VarveFile::reserve_keyed_tail_slot`] already reserved the slot, so the
    /// insert cannot allocate. If the reservation is nevertheless not
    /// observable - the map was dropped in between, or capacity is somehow
    /// gone - the cached map for this block id is *discarded* rather than left
    /// holding the superseded predecessor. Dropping it is allocation-free and
    /// always safe: the next generic keyed mutation rebuilds it from the
    /// resident index, which already contains this record. The one outcome
    /// this must never produce is a surviving stale predecessor.
    fn commit_keyed_tail(&mut self, block_id: u32, key: Vec<u8>, record_offset: u64) {
        let reservation_lost = take_injected_keyed_tail_commit_loss();
        match self.keyed_tails.tails.get_mut(&block_id) {
            Some(tails)
                if !reservation_lost
                    && (tails.offsets.contains_key(&key)
                        || tails.offsets.capacity() > tails.offsets.len()) =>
            {
                let payload_len = key.len() as u64;
                if tails.offsets.insert(key, record_offset).is_none() {
                    tails.key_payload_bytes = tails.key_payload_bytes.saturating_add(payload_len);
                }
            }
            _ => self.keyed_tails.invalidate(block_id),
        }
    }

    pub fn push_with_prev_key_info<T: VarveBlock>(
        &mut self,
        block: &T,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        // API2-05: the caller owns this block's keyed chain, so any tail map
        // this file cached for the block id can no longer be trusted - the
        // record's key is not observable here.
        self.keyed_tails.invalidate(T::ID);
        self.push_with_prev_key_info_unlinked(block, prev_same_key_offset)
    }

    fn push_with_prev_key_info_unlinked<T: VarveBlock>(
        &mut self,
        block: &T,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        let permit = self.ensure_write()?;
        self.ensure_user_block::<T>()?;
        if T::KIND == BlockKind::Matrix {
            return Err(Error::BlockKindMismatch {
                expected: BlockKind::Fixed,
                actual: T::KIND,
            });
        }
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        let payload = encode_logical_payload_limited(self.spec, block, endian)?;
        self.write_user_record(
            &permit,
            T::ID,
            T::VERSION,
            T::KIND,
            &payload,
            prev_same_key_offset,
        )
    }

    /// Appends a tombstone for `key`, linking it to the previous record with
    /// the same key.
    ///
    /// API2-05: this used to write `prev_same_key_offset = None` and truncate
    /// the physical keyed chain. It now resolves and maintains the predecessor
    /// through the same tail map as [`VarveFile::push_keyed_info`].
    pub fn delete<T>(&mut self, key: &T::Key) -> Result<u64>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        Ok(self.delete_info::<T>(key)?.sequence)
    }

    /// Appends a tombstone for `key` and returns the full [`AppendInfo`].
    ///
    /// This is [`VarveFile::delete`] with the append description the generated
    /// keyed writers return, and it is the single implementation both routes
    /// use. Its ordering is the one property that matters: every fallible step
    /// (the tail-map build, the key encoding, the slot charge and reservation)
    /// happens strictly before the tombstone is appended, and the only step
    /// after the authoritative mutation is the infallible, allocation-free,
    /// user-code-free `VarveFile::commit_keyed_tail` (invariant 3).
    pub fn delete_info<T>(&mut self, key: &T::Key) -> Result<AppendInfo>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        if !self.spec.index_policy.keyed_offset_chain {
            return self.delete_with_prev_key_info_unlinked::<T>(key, None);
        }
        let _permit = self.ensure_write()?;
        // The tombstone payload *is* the canonical internal key payload, and
        // it is also the tail map's key. Encoding it once here and handing the
        // same bytes to both consumers keeps the delete path at one key
        // encoding per record; it used to encode twice.
        let key_payload = encode_internal_key_payload::<T>(self.spec, key)?;
        // API3-01: reserve before the tombstone becomes authoritative; see
        // [`VarveFile::reserve_keyed_tail_slot`].
        let previous = self.reserve_keyed_tail_slot::<T>(&key_payload)?;
        let info = self.write_tombstone_payload::<T>(&key_payload, previous)?;
        self.commit_keyed_tail(T::ID, key_payload, info.record_offset);
        Ok(info)
    }

    /// Builds the keyed tail map for `T` now rather than on first keyed
    /// mutation (F-01).
    ///
    /// The generated keyed writers call this at construction, which is where
    /// they have always paid for their tail map: a file whose distinct key
    /// count exceeds [`crate::ReadLimits::max_keyed_tail_bytes`] is refused when the
    /// writer opens, not on some later append. Formats without
    /// `keyed_offset_chain` maintain no map at all, so this does nothing for
    /// them and allocates nothing.
    pub fn prime_keyed_tails<T>(&mut self) -> Result<()>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        if !self.spec.index_policy.keyed_offset_chain {
            return Ok(());
        }
        self.keyed_tail_map::<T>()?;
        Ok(())
    }

    pub fn delete_with_prev_key_info<T>(
        &mut self,
        key: &T::Key,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo>
    where
        T: VarveKeyedBlock,
    {
        // API2-05: the caller owns this block's keyed chain; drop the cached
        // tail map so a later maintained append rebuilds it.
        self.keyed_tails.invalidate(T::ID);
        self.delete_with_prev_key_info_unlinked::<T>(key, prev_same_key_offset)
    }

    fn delete_with_prev_key_info_unlinked<T>(
        &mut self,
        key: &T::Key,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo>
    where
        T: VarveKeyedBlock,
    {
        // API2-03: keyed generic entry points evaluate the compile-time
        // keyedness contract post-monomorphization; the registration below
        // stays as the runtime backstop. This terminal site also covers the
        // VarveFile::delete and VarveReader/VarveWriter delete wrappers.
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        let _permit = self.ensure_write()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let payload = encode_internal_key_payload::<T>(self.spec, key)?;
        self.write_tombstone_payload::<T>(&payload, prev_same_key_offset)
    }

    /// Appends a tombstone whose canonical internal key payload is already
    /// encoded.
    ///
    /// Split out of `delete_with_prev_key_info_unlinked` so the maintained
    /// delete path can encode the key exactly once and use the same bytes for
    /// the tail map and for the record.
    fn write_tombstone_payload<T>(
        &mut self,
        payload: &[u8],
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo>
    where
        T: VarveKeyedBlock,
    {
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        let permit = self.ensure_write()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        self.write_record_with_prev_key(
            &permit,
            TOMBSTONE_BLOCK_ID,
            1,
            RECORD_FLAG_INTERNAL,
            0,
            payload,
            prev_same_key_offset,
        )
    }

    pub fn push_op<T>(&mut self, key: &T::Key, op: &T::Op) -> Result<u64>
    where
        T: VarveMerge,
    {
        let _permit = self.ensure_write()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let payload = encode_internal_op_payload::<T>(self.spec, key, op)?;
        self.write_record(OP_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, &payload)
    }

    pub fn write_metadata(&mut self, key: &str, value: &[u8]) -> Result<u64> {
        // Encoded as (String, Vec<u8>): an 8-byte length prefix per part.
        const METADATA_ENVELOPE_LEN: u64 = 16;
        let _permit = self.ensure_write()?;
        // DEF-02: reject an oversized entry with the typed limit error before
        // the caller's key and value are cloned or encoded.
        let entry_len = (key.len() as u64)
            .checked_add(value.len() as u64)
            .and_then(|len| len.checked_add(METADATA_ENVELOPE_LEN))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "metadata entry length",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::LogicalPayloadLen, entry_len)?;
        let payload = encode_logical_payload_limited(
            self.spec,
            &(key.to_string(), value.to_vec()),
            self.spec.endian,
        )?;
        self.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, &payload)
    }

    /// The newest value written for `key`, or `None`.
    ///
    /// Visits every metadata record — latest-wins needs the whole walk, so
    /// there is no early break — but only *materializes* the ones it returns.
    /// [`split_metadata_payload`] compares the stored key against the borrowed
    /// payload, and the decode that allocates a `String` and a `Vec<u8>` runs
    /// only for a record that matches and wins.
    ///
    /// `max_materialized_bytes` bounds one record here, not the sum over the
    /// walk: a lookup keeps at most one value alive, and charging the sum
    /// refused a healthy file whose *total* metadata exceeded the ceiling
    /// (`STANDARD` sets it to 1 GiB) even when the requested entry was the
    /// first record in the file.
    pub fn metadata(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut found = None;
        let mut budget = MaterializationBudget::new(self.spec);
        for (record_ordinal, entry) in self.index.iter().enumerate() {
            if entry.block_id != METADATA_BLOCK_ID {
                continue;
            }
            budget.reset();
            let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
            budget.consume(logical_len)?;
            let payload = entry.read_payload_snapshot(self.spec, &self.snapshot)?;
            let (stored_key, _) = split_metadata_payload(&payload, self.spec.endian)?;
            if stored_key != key {
                continue;
            }
            let order = MergeOrder::for_record(0, entry.sequence, record_ordinal);
            let should_replace = found
                .as_ref()
                .is_none_or(|(old_order, _): &(MergeOrder, Vec<u8>)| order >= *old_order);
            if should_replace {
                // Decoded through the accounted decoder exactly as before, so
                // what a returned value costs the budget is unchanged.
                let (_, value): (String, Vec<u8>) = budget.decode(&payload, self.spec.endian)?;
                found = Some((order, value));
            }
        }
        Ok(found.map(|(_, value)| value))
    }

    pub fn all_metadata(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let mut values = Vec::new();
        let mut budget = MaterializationBudget::new(self.spec);
        for entry in self
            .index
            .iter()
            .filter(|entry| entry.block_id == METADATA_BLOCK_ID)
        {
            let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
            budget.consume(logical_len)?;
            let payload = entry.read_payload_snapshot(self.spec, &self.snapshot)?;
            values.try_reserve(1).map_err(|_| Error::AllocationFailed {
                resource: "metadata entries",
                requested: logical_len,
            })?;
            values.push(budget.decode(&payload, self.spec.endian)?);
        }
        Ok(values)
    }

    pub fn schema_manifest(&self) -> Result<Option<SchemaManifest>> {
        let Some(entry) = self
            .index
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.block_id == MANIFEST_BLOCK_ID)
            .max_by_key(|(record_ordinal, entry)| {
                MergeOrder::for_record(0, entry.sequence, *record_ordinal)
            })
            .map(|(_, entry)| entry)
        else {
            return Ok(None);
        };
        let mut budget = MaterializationBudget::new(self.spec);
        let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
        budget.consume(logical_len)?;
        let payload = entry.read_payload_snapshot(self.spec, &self.snapshot)?;
        Ok(Some(decode_schema_manifest(&payload, &mut budget)?))
    }

    /// Replaces a native user record while preserving its sequence number.
    ///
    /// The new generation is validated and synced before atomic publication.
    /// Existing readers remain attached to the old generation.
    pub fn replace_block<T: VarveReplaceBlock>(
        &mut self,
        index: usize,
        block: &T,
    ) -> Result<ReplacementInfo> {
        let _permit = self.ensure_write()?;
        if !self.spec.layout.is_varve_native_default() {
            return Err(Error::InvalidFormatSpec(
                "replacement is not supported for custom physical layouts",
            ));
        }
        if self.matrix.is_some() || T::KIND == BlockKind::Matrix {
            return Err(Error::InvalidFormatSpec(
                "replacement is not supported for matrix storage",
            ));
        }
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        // F-01: resolution and the target-version refusal are one step, and
        // there is no other way to address the target. See `ReplacementTarget`.
        let target_position = ReplacementTarget::resolve::<T>(&self.index, index)?.position();
        let target = self.index.entry_at(target_position)?;
        let mut materialization = MaterializationBudget::new(self.spec);
        let old_logical_len = target.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
        materialization.consume(old_logical_len)?;
        let old_payload = target.read_logical_payload_snapshot(self.spec, &self.snapshot)?;
        let old: T = materialization.decode(&old_payload, T::ENDIAN.unwrap_or(self.spec.endian))?;
        T::validate_replacement(&old, block)?;

        let encoded = encode_logical_payload_limited(
            self.spec,
            block,
            T::ENDIAN.unwrap_or(self.spec.endian),
        )?;
        let replacement = prepare_user_record_payload(self.spec, T::ID, T::KIND, &encoded)?;
        let replacement_len = u64::try_from(replacement.bytes.len())
            .map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, replacement_len)?;
        let old_physical_len = target
            .checked_physical_end()?
            .checked_sub(target.record_offset)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "replacement physical length",
            })?;
        let new_physical_len = RECORD_HEADER_LEN
            .checked_add(replacement_len)
            .and_then(|len| len.checked_add(record_footer_len(self.spec)))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "replacement physical length",
            })?;
        let info = ReplacementInfo {
            sequence: target.sequence,
            record_offset: target.record_offset,
            old_payload_len: target.payload_len,
            new_payload_len: replacement_len,
            old_physical_len,
            new_physical_len,
        };

        self.ensure_generation_rewrite_allowed()?;
        self.validate_source_generation()?;
        let index_bytes = index_bytes_for_count(self.index.len())?;
        self.spec
            .read_limits
            .check(ReadLimitKey::IndexBytes, index_bytes)?;
        let mut new_index = Vec::new();
        new_index
            .try_reserve_exact(self.index.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "replacement index",
                requested: index_bytes,
            })?;

        let rewrite_append_start = append_log_start_for_file(self)?;
        // Segment coverage in the new generation, tracked exactly as the
        // writer's `SegmentCursor` tracks it in the old one.
        let mut segment_start = 0usize;
        let mut segment_covered_start: Option<u64> = None;
        let (temp_path, mut temp_file) = create_rewrite_temp_file(&self.path)?;
        let prepare_result = (|| -> Result<()> {
            if let Ok(metadata) = self.file.metadata() {
                temp_file.set_permissions(metadata.permissions())?;
            }
            // Write back the region this file carries, not the one `spec`
            // would generate: regenerating drops any block this build does not
            // know and moves the append log out from under the index.
            write_native_file_header(&mut temp_file, self.spec, &self.header_extensions)?;
            for (position, source_entry) in self.index.iter().enumerate() {
                let mut updated = source_entry.clone();
                let checkpoint_payload;
                let payload = if position == target_position {
                    updated.flags = replacement.flags;
                    updated.uncompressed_len_hint = replacement.uncompressed_len_hint;
                    RewritePayload::Bytes(&replacement.bytes)
                } else if source_entry.block_id == INDEX_BLOCK_ID {
                    checkpoint_payload = encode_index_checkpoint_payload(
                        self.spec,
                        &new_index,
                        temp_file.stream_position()?,
                    )?;
                    RewritePayload::Bytes(&checkpoint_payload)
                } else if source_entry.block_id == SEGMENT_BLOCK_ID {
                    // A segment payload is record offsets, and this rewrite
                    // moves them. Copying the bytes would publish a generation
                    // whose chain describes the file it replaced.
                    let record_offset = temp_file.stream_position()?;
                    checkpoint_payload = encode_segment_payload(
                        self.spec,
                        new_index[segment_start..].iter(),
                        segment_covered_start.unwrap_or(rewrite_append_start),
                        u64::try_from(segment_start).map_err(|_| {
                            Error::ResourceArithmeticOverflow {
                                resource: "segment entry count",
                            }
                        })?,
                        record_offset,
                    )?;
                    RewritePayload::Bytes(&checkpoint_payload)
                } else {
                    RewritePayload::Snapshot {
                        offset: source_entry.payload_offset,
                        len: source_entry.payload_len,
                    }
                };
                updated = rewrite_replacement_record_streaming(
                    self.spec,
                    &self.snapshot,
                    &mut temp_file,
                    updated,
                    payload,
                    info,
                    &new_index,
                )?;
                if updated.block_id == SEGMENT_BLOCK_ID {
                    segment_covered_start = Some(updated.checked_physical_end()?);
                    segment_start = new_index.len() + 1;
                }
                // What makes `find_rewritten_by_offset`'s binary search legal:
                // each record is written at `output.stream_position()` and
                // pushed in that order, so the prefix is strictly increasing in
                // `record_offset`. Asserted here, where the invariant is made,
                // because that is O(1) — asserting sortedness inside the lookup
                // would reintroduce the O(N) it exists to remove.
                debug_assert!(
                    new_index
                        .last()
                        .is_none_or(|last| last.record_offset < updated.record_offset)
                );
                new_index.push(updated);
            }
            temp_file.flush()?;
            temp_file.sync_all()?;
            validate_replacement_generation_file(
                self.spec,
                &mut temp_file,
                append_log_start_for_file(self)?,
                &new_index,
            )?;
            Ok(())
        })();
        if let Err(error) = prepare_result {
            drop(temp_file);
            let _ = remove_file(&temp_path);
            return Err(error);
        }
        drop(temp_file);

        match replace_path_atomically(&temp_path, &self.path) {
            Ok(ReplaceDurability::Durable) => self.rebind_replacement_generation(info, new_index),
            Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
                // Publication already happened: the pathname resolves to the
                // new generation, so the writer must move there regardless.
                //
                // F-07: `?` here would drop `sync_error` if the rebind failed
                // too, and `PublishedButRebindFailed` alone cannot say that
                // the published pathname is not yet durable. Both facts are
                // preserved instead.
                match self.rebind_replacement_generation(info, new_index) {
                    Ok(_) => Err(Error::PublishedButParentSyncPending {
                        path: self.path.display().to_string(),
                        source: Box::new(sync_error),
                    }),
                    Err(rebind_error) => Err(rebind_error.with_pending_parent_sync(sync_error)),
                }
            }
            Err(error) => Err(self.fail_publication(&temp_path, error)),
        }
    }

    /// Publishes a same-size fixed replacement through a copy-on-write file
    /// generation.
    ///
    /// [`Error::PublishedButRebindFailed`] means publication already succeeded.
    /// Do not retry blindly; discard this poisoned writer and reopen the path.
    ///
    /// [`Error::PublishedButParentSyncPending`] also means publication already
    /// succeeded; the writer was rebound to the published generation and stays
    /// usable, but the rename is not yet guaranteed durable against power loss
    /// until the parent directory is synced (for example by a later successful
    /// publication or an explicit directory sync).
    /// Refuses an in-place record mutation on a segment-chained format.
    ///
    /// A segment payload records the header fields of every record it covers,
    /// and an in-place replacement restamps the sequence and checksum of a
    /// record that is already covered. Nothing rewrites the segment behind it,
    /// so the chain would keep describing the record as it was - and open would
    /// have no way to notice, because it reads no data record.
    ///
    /// Patching the covering segment instead was considered and is the shape
    /// §4A.5 rejected for the index anchor: an in-place rewrite of a derived
    /// structure, with a tear window a lock-free `open_readonly` can observe.
    /// So this is a capability trade rather than a repair: in-place fixed
    /// replacement and the segment chain are alternatives, and
    /// `replace_block` - which publishes a whole new generation, and re-encodes
    /// every segment payload against the new offsets - is available under both.
    /// Refuses a whole-generation rewrite for a format with a non-resident
    /// block.
    ///
    /// Both rewrite paths rebuild the new generation by iterating the resident
    /// index, which no longer mirrors every record: a non-resident block's
    /// records would be absent from the copy and silently dropped from the
    /// published file. This is a format-level refusal rather than a per-block
    /// one, because the loss is of blocks the caller did not name.
    fn ensure_generation_rewrite_allowed(&self) -> Result<()> {
        if self.spec.has_non_resident_blocks() {
            return Err(Error::InvalidFormatSpec(
                "a generation rewrite is not supported for a format with a non-resident block",
            ));
        }
        Ok(())
    }

    fn ensure_in_place_replacement_allowed(&self) -> Result<()> {
        if self.spec.index_policy.segment_on_flush {
            return Err(Error::InvalidFormatSpec(
                "in-place replacement is not supported for segment_on_flush formats",
            ));
        }
        Ok(())
    }

    pub fn replace_fixed<T: VarveBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        let _permit = self.ensure_write()?;
        self.ensure_in_place_replacement_allowed()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        if T::KIND != BlockKind::Fixed {
            return Err(Error::BlockKindMismatch {
                expected: BlockKind::Fixed,
                actual: T::KIND,
            });
        }
        // F-01: resolution and the target-version refusal are one step, and
        // there is no other way to address the target. See `ReplacementTarget`.
        let target_position = ReplacementTarget::resolve::<T>(&self.index, index)?.position();

        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        let payload = encode_logical_payload_limited(self.spec, block, endian)?;
        let payload_len =
            u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::LogicalPayloadLen, payload_len)?;
        let old_len = self.index.entry_at(target_position)?.payload_len;
        if old_len != payload_len {
            return Err(Error::ReplaceSizeMismatch {
                old: old_len,
                new: payload_len,
            });
        }
        self.ensure_generation_rewrite_allowed()?;
        self.validate_source_generation()?;

        let sequence = self.sequence_state.available()?;
        let entry = self.index.entry_at(target_position)?;
        let mut footer_bytes = [0; RECORD_FOOTER_LEN as usize];
        let footer = if let Some(footer_offset) = entry.footer_offset {
            self.snapshot
                .read_exact_at(footer_offset, &mut footer_bytes)?;
            &footer_bytes[..]
        } else {
            &[]
        };
        let header = RecordHeaderFields {
            block_id: entry.block_id,
            block_version: entry.block_version,
            flags: entry.flags,
            sequence,
            payload_len: entry.payload_len,
            checksum: 0,
            uncompressed_len_hint: entry.uncompressed_len_hint,
        };
        let checksum =
            checksum_record_fields(self.spec, entry.record_offset, header, &payload, footer)?;
        let header_bytes = encode_native_record_header(
            RecordHeaderFields { checksum, ..header },
            entry.record_offset,
            record_footer_len(self.spec),
        )?;
        let record_offset = entry.record_offset;
        let payload_offset = entry.payload_offset;

        // Every entry, so the count is the length and no counting pass is
        // needed to learn it.
        let mut new_index =
            clone_matching_entries(self.spec, &self.index, self.index.len(), |_| true)?;
        new_index[target_position].sequence = sequence;
        new_index[target_position].checksum = checksum;

        let (temp_path, mut temp_file) = create_rewrite_temp_file(&self.path)?;
        let prepare_result = (|| -> Result<()> {
            if let Ok(metadata) = self.file.metadata() {
                temp_file.set_permissions(metadata.permissions())?;
            }
            self.snapshot
                .copy_range_to(0, self.snapshot.len(), &mut temp_file)?;
            temp_file.seek(SeekFrom::Start(record_offset))?;
            temp_file.write_all(&header_bytes)?;
            temp_file.seek(SeekFrom::Start(payload_offset))?;
            temp_file.write_all(&payload)?;
            temp_file.flush()?;
            temp_file.sync_all()?;
            validate_generation_file(
                self.spec,
                &mut temp_file,
                append_log_start_for_file(self)?,
                &new_index,
            )?;
            Ok(())
        })();
        match prepare_result {
            Ok(()) => {}
            Err(error) => {
                drop(temp_file);
                let _ = remove_file(&temp_path);
                return Err(error);
            }
        }
        drop(temp_file);

        match replace_path_atomically(&temp_path, &self.path) {
            Ok(ReplaceDurability::Durable) => self.rebind_published_generation(sequence, new_index),
            Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
                // Publication already happened: the pathname resolves to the
                // new generation, so the writer must move there regardless.
                //
                // F-07: `?` here would drop `sync_error` if the rebind failed
                // too, and `PublishedButRebindFailed` alone cannot say that
                // the published pathname is not yet durable. Both facts are
                // preserved instead.
                match self.rebind_published_generation(sequence, new_index) {
                    Ok(_) => Err(Error::PublishedButParentSyncPending {
                        path: self.path.display().to_string(),
                        source: Box::new(sync_error),
                    }),
                    Err(rebind_error) => Err(rebind_error.with_pending_parent_sync(sync_error)),
                }
            }
            Err(error) => Err(self.fail_publication(&temp_path, error)),
        }
    }

    /// Replaces a fixed record by mutating this file object directly.
    ///
    /// # Errors
    ///
    /// F-01: the target is selected by block id, so a version mismatch between
    /// `T` and the stored record is refused with
    /// [`Error::BlockVersionMismatch`], exactly as [`VarveFile::replace_fixed`]
    /// does. Otherwise `T`'s payload would be written under the stored
    /// record's older version header. The refusal is not a step this function
    /// performs; it is a precondition of resolving the target at all. See
    /// `ReplacementTarget`.
    ///
    /// # Safety
    ///
    /// The caller must exclude every reader, writer, mapping, raw reference,
    /// handle, thread, and process for this operation and for the lifetime of
    /// every view that could observe the affected object.
    ///
    /// For a keyed block in a format with `keyed_offset_chain`, the caller
    /// must additionally **not change the record's key** (F-02). This entry
    /// point takes `T: VarveBlock`, which exposes no key, so the requirement
    /// cannot be checked here, and unlike the copy-on-write paths there is no
    /// new generation in which the chain could be rebuilt: records appended
    /// after this one already carry `prev_same_key_offset` pointers into it,
    /// and rewriting its key in place would leave them linking a record that
    /// now claims a different key. Ordinary keyed replacement enforces the
    /// same-key contract through
    /// [`crate::VarveReplaceBlock::validate_replacement`]; here it is part of
    /// the unsafe contract.
    ///
    /// What this function *does* guarantee, whether or not the contract is
    /// honoured, is that no resident keyed-tail cache survives the mutation:
    /// the affected block's tail map is dropped **before** the first byte is
    /// written, so a stale predecessor can never be read afterwards, not even
    /// if the write fails part-way or the contract above is violated.
    pub unsafe fn replace_fixed_in_place_exclusive<T: VarveBlock>(
        &mut self,
        index: usize,
        block: &T,
    ) -> Result<u64> {
        let permit = self.ensure_write()?;
        self.ensure_in_place_replacement_allowed()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        if T::KIND != BlockKind::Fixed {
            return Err(Error::BlockKindMismatch {
                expected: BlockKind::Fixed,
                actual: T::KIND,
            });
        }
        // F-01: resolution and the target-version refusal are one step, and
        // there is no other way to address the target. See `ReplacementTarget`.
        let target = ReplacementTarget::resolve::<T>(&self.index, index)?;
        let target_position = target.position();
        let payload = encode_logical_payload_limited(
            self.spec,
            block,
            T::ENDIAN.unwrap_or(self.spec.endian),
        )?;
        let payload_len =
            u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::LogicalPayloadLen, payload_len)?;
        let entry = self.index.entry_at(target_position)?;
        if entry.payload_len != payload_len {
            return Err(Error::ReplaceSizeMismatch {
                old: entry.payload_len,
                new: payload_len,
            });
        }
        self.ensure_generation_rewrite_allowed()?;
        self.validate_source_generation()?;
        let sequence = self.sequence_state.available()?;
        let entry = self.index.entry_at(target_position)?;
        let mut footer_bytes = [0; RECORD_FOOTER_LEN as usize];
        let footer = if let Some(footer_offset) = entry.footer_offset {
            self.snapshot
                .read_exact_at(footer_offset, &mut footer_bytes)?;
            &footer_bytes[..]
        } else {
            &[]
        };
        let header = RecordHeaderFields {
            block_id: entry.block_id,
            block_version: entry.block_version,
            flags: entry.flags,
            sequence,
            payload_len,
            checksum: 0,
            uncompressed_len_hint: entry.uncompressed_len_hint,
        };
        let checksum =
            checksum_record_fields(self.spec, entry.record_offset, header, &payload, footer)?;
        let header_bytes = encode_native_record_header(
            RecordHeaderFields { checksum, ..header },
            entry.record_offset,
            record_footer_len(self.spec),
        )?;
        let write_result =
            self.overwrite_record_bytes_in_place(&permit, target, &header_bytes, &payload);
        if let Err(error) = write_result {
            self.poison.poison();
            return Err(error);
        }
        let entry = self.index.entry_mut(target_position);
        entry.sequence = sequence;
        entry.checksum = checksum;
        self.publish_sequence(sequence);
        Ok(sequence)
    }

    /// Publishes a replacement by rewriting the complete file generation.
    ///
    /// [`Error::PublishedButRebindFailed`] means publication already succeeded.
    /// Do not retry blindly; discard this poisoned writer and reopen the path.
    ///
    /// [`Error::PublishedButParentSyncPending`] also means publication already
    /// succeeded; the writer was rebound to the published generation and stays
    /// usable, but the rename is not yet guaranteed durable against power loss
    /// until the parent directory is synced (for example by a later successful
    /// publication or an explicit directory sync).
    pub fn replace_rewrite<T: VarveBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        let _permit = self.ensure_write()?;
        if self.spec.spec_needs_record_footer() {
            return Err(Error::InvalidFormatSpec(
                "replace is not supported for record-footer formats",
            ));
        }
        if self.matrix.is_some() {
            return Err(Error::InvalidFormatSpec(
                "native rewrite is not supported for matrix files",
            ));
        }
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        // F-01: resolution and the target-version refusal are one step, and
        // there is no other way to address the target. See `ReplacementTarget`.
        let target_position = ReplacementTarget::resolve::<T>(&self.index, index)?.position();
        let sequence = self.sequence_state.available()?;
        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        let encoded = encode_logical_payload_limited(self.spec, block, endian)?;
        let replacement = prepare_user_record_payload(self.spec, T::ID, T::KIND, &encoded)?;
        let replacement_len = u64::try_from(replacement.bytes.len())
            .map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, replacement_len)?;
        self.ensure_generation_rewrite_allowed()?;
        self.validate_source_generation()?;

        let index_bytes = index_bytes_for_count(self.index.len())?;
        self.spec
            .read_limits
            .check(ReadLimitKey::IndexBytes, index_bytes)?;
        let mut new_index = Vec::new();
        new_index
            .try_reserve_exact(self.index.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "rewrite index",
                requested: index_bytes,
            })?;

        let (temp_path, mut temp_file) = create_rewrite_temp_file(&self.path)?;
        let prepare_result = (|| -> Result<()> {
            if let Ok(metadata) = self.file.metadata() {
                temp_file.set_permissions(metadata.permissions())?;
            }
            // Write back the region this file carries, not the one `spec`
            // would generate: regenerating drops any block this build does not
            // know and moves the append log out from under the index.
            write_native_file_header(&mut temp_file, self.spec, &self.header_extensions)?;
            for (position, source_entry) in self.index.iter().enumerate() {
                let mut updated = source_entry.clone();
                let checkpoint_payload;
                let payload = if position == target_position {
                    updated.sequence = sequence;
                    updated.flags = replacement.flags;
                    updated.uncompressed_len_hint = replacement.uncompressed_len_hint;
                    RewritePayload::Bytes(&replacement.bytes)
                } else if source_entry.block_id == INDEX_BLOCK_ID {
                    checkpoint_payload = encode_index_checkpoint_payload(
                        self.spec,
                        &new_index,
                        temp_file.stream_position()?,
                    )?;
                    RewritePayload::Bytes(&checkpoint_payload)
                } else {
                    RewritePayload::Snapshot {
                        offset: source_entry.payload_offset,
                        len: source_entry.payload_len,
                    }
                };
                updated = rewrite_record_streaming(
                    self.spec,
                    &self.snapshot,
                    &mut temp_file,
                    updated,
                    payload,
                )?;
                new_index.push(updated);
            }
            temp_file.flush()?;
            temp_file.sync_all()?;
            validate_generation_file(
                self.spec,
                &mut temp_file,
                append_log_start_for_file(self)?,
                &new_index,
            )?;
            Ok(())
        })();
        match prepare_result {
            Ok(()) => {}
            Err(error) => {
                drop(temp_file);
                let _ = remove_file(&temp_path);
                return Err(error);
            }
        }
        drop(temp_file);

        match replace_path_atomically(&temp_path, &self.path) {
            Ok(ReplaceDurability::Durable) => self.rebind_published_generation(sequence, new_index),
            Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
                // Publication already happened: the pathname resolves to the
                // new generation, so the writer must move there regardless.
                //
                // F-07: `?` here would drop `sync_error` if the rebind failed
                // too, and `PublishedButRebindFailed` alone cannot say that
                // the published pathname is not yet durable. Both facts are
                // preserved instead.
                match self.rebind_published_generation(sequence, new_index) {
                    Ok(_) => Err(Error::PublishedButParentSyncPending {
                        path: self.path.display().to_string(),
                        source: Box::new(sync_error),
                    }),
                    Err(rebind_error) => Err(rebind_error.with_pending_parent_sync(sync_error)),
                }
            }
            Err(error) => Err(self.fail_publication(&temp_path, error)),
        }
    }

    pub fn replace<T: VarveBlock>(
        &mut self,
        index: usize,
        block: &T,
        strategy: ReplaceStrategy,
    ) -> Result<u64> {
        match strategy {
            ReplaceStrategy::FixedCopyOnWrite => self.replace_fixed(index, block),
            ReplaceStrategy::RewriteFile => self.replace_rewrite(index, block),
        }
    }

    pub fn flush(&mut self) -> Result<()> {
        let _permit = self.ensure_not_poisoned()?;
        self.write_embedded_manifest_if_needed()?;
        // First, before the checkpoint and before the marker. A chunk is an
        // ordinary record: sealing after the marker would leave it outside the
        // committed prefix, and sealing after the checkpoint would leave it out
        // of the index that checkpoint serialises.
        if self.mode == OpenMode::ReadWrite {
            self.seal_open_chunk()?;
        }
        if self.mode == OpenMode::ReadWrite
            && self.spec.index_policy.checkpoint_on_flush
            && self.needs_index_checkpoint()
        {
            self.write_index_checkpoint()?;
        }
        if self.mode == OpenMode::ReadWrite
            && self.spec.commit_policy.marker_on_flush()
            && self.has_uncommitted_since_last_commit()
        {
            self.write_commit_marker()?;
        }
        // Last, and after the marker: the chain is found from the end of the
        // file, so anything appended behind this record hides it.
        self.write_index_segment_if_needed();
        self.file.flush()?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<AppendInfo> {
        let _permit = self.ensure_write()?;
        if !self.spec.commit_policy.is_transaction_marker() {
            return Err(Error::InvalidFormatSpec(
                "commit markers require transaction_marker commit policy",
            ));
        }
        self.write_embedded_manifest_if_needed()?;
        self.seal_open_chunk()?;
        if self.spec.index_policy.checkpoint_on_flush && self.needs_index_checkpoint() {
            self.write_index_checkpoint()?;
        }
        if !self.has_uncommitted_since_last_commit()
            && let Some(entry) = self
                .index
                .iter()
                .rev()
                .find(|entry| entry.block_id == COMMIT_BLOCK_ID)
        {
            let info = AppendInfo::from(entry);
            self.write_index_segment_if_needed();
            return Ok(info);
        }
        let info = self.write_commit_marker()?;
        self.write_index_segment_if_needed();
        Ok(info)
    }

    pub fn commit_durable(&mut self) -> Result<AppendInfo> {
        let _permit = self.ensure_write()?;
        if !self.spec.commit_policy.is_transaction_marker() {
            return Err(Error::InvalidFormatSpec(
                "commit markers require transaction_marker commit policy",
            ));
        }
        self.write_embedded_manifest_if_needed()?;
        self.seal_open_chunk()?;
        if self.spec.index_policy.checkpoint_on_flush && self.needs_index_checkpoint() {
            self.write_index_checkpoint()?;
        }
        if !self.has_uncommitted_since_last_commit()
            && let Some(entry) = self
                .index
                .iter()
                .rev()
                .find(|entry| entry.block_id == COMMIT_BLOCK_ID)
        {
            let info = AppendInfo::from(entry);
            self.write_index_segment_if_needed();
            self.file.flush()?;
            self.file.sync_all()?;
            // DUR3-01: a durable commit on a file this handle created must
            // also establish the pathname; this happens once, not per commit.
            self.sync_created_pathname_once()?;
            return Ok(info);
        }
        self.file.flush()?;
        self.file.sync_data()?;
        let info = self.write_commit_marker()?;
        // The segment closes the commit point, so it is appended after the
        // marker and before the durability request that makes both durable.
        // `write_index_segment_if_needed` cannot report a failure for the same
        // reason the two steps below do not report theirs as a bare `Err`: the
        // commit has happened.
        self.write_index_segment_if_needed();
        // INVARIANT 3: `write_commit_marker` is the authoritative commit. From
        // here the marker record is in the file and a reader that opens it
        // after a clean process exit sees the transaction as committed, so a
        // failure of the durability request that follows must not be reported
        // as a bare `Err` — that would entitle the caller to believe nothing
        // happened and re-run the transaction, appending a second marker for
        // work already recorded. Both remaining steps therefore report typed
        // published outcomes: content durability as
        // `CommittedButDurabilityUnproven`, and the created pathname's
        // directory entry as `PublishedButParentSyncPending`, which
        // `sync_created_pathname_once` already produces and which leaves the
        // request pending so a later `sync` retries it.
        if let Err(source) = self.flush_and_sync_all() {
            return Err(Error::CommittedButDurabilityUnproven {
                sequence: info.sequence,
                source: Box::new(source),
            });
        }
        self.sync_created_pathname_once()?;
        Ok(info)
    }

    /// Flushes the buffered writer and syncs the file contents.
    ///
    /// Split out so the post-commit-marker durability request in
    /// [`Self::commit_durable`] has exactly one failure point to classify.
    fn flush_and_sync_all(&mut self) -> Result<()> {
        take_injected_commit_durability_failure()?;
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Requests durable persistence of everything written so far.
    ///
    /// This syncs the file's contents and, for a pathname this handle created,
    /// its parent directory entry exactly once (DUR3-01). Without the second
    /// step a power loss could leave a fully synced object with no name on
    /// platforms that require an explicit directory sync, which would not be
    /// "durably persisted" in any useful sense; the atomic replacement path
    /// already syncs the parent and reports a pending parent sync, so creation
    /// now makes the same guarantee through the same machinery.
    ///
    /// The parent sync costs one directory fsync per created file, never
    /// recurs, and is not on the append path. A failed parent sync is reported
    /// as [`Error::PublishedButParentSyncPending`] - the contents are durable
    /// and the pathname is visible, only its directory entry's durability is
    /// unconfirmed - and leaves the request pending, so a later `sync` retries
    /// it.
    pub fn sync(&mut self) -> Result<()> {
        let _permit = self.ensure_not_poisoned()?;
        // A committed chunked cell lives in memory until its chunk is sealed,
        // so syncing the file without sealing made `sync()` return `Ok` having
        // made nothing durable — while the same call on a region row did.
        if self.mode == OpenMode::ReadWrite {
            self.seal_open_chunk()?;
        }
        self.file.sync_all()?;
        self.sync_created_pathname_once()
    }

    /// Makes a pathname created by this handle durable, at most once.
    fn sync_created_pathname_once(&mut self) -> Result<()> {
        if !self.pending_pathname_parent_sync {
            return Ok(());
        }
        match sync_parent_directory(&self.path) {
            Ok(()) => {
                self.pending_pathname_parent_sync = false;
                Ok(())
            }
            Err(error) => Err(Error::PublishedButParentSyncPending {
                path: self.path.display().to_string(),
                source: Box::new(error),
            }),
        }
    }

    /// The record offset of the newest record of `block_id`, or `None` if the
    /// file holds none.
    ///
    /// H2. The entry point of the block offset chain, and the only way to reach
    /// a non-resident block: from here each record's footer names its
    /// predecessor. `&self`, answered from the maintained tail table without
    /// touching the resident index (PERF2-05).
    pub fn block_tail_offset(&self, block_id: u32) -> Option<u64> {
        self.block_tails.tail(block_id)
    }

    /// Walks a block's records newest-first through the footer chain.
    ///
    /// H2, and the only way to read a non-resident block. Takes `&self`, reads
    /// positionally through the open snapshot, and materialises one entry at a
    /// time — the resident index is never consulted and never grown, so this
    /// costs the working set rather than the record count.
    ///
    /// Refuses without `block_offset_chain`: without it the footer carries no
    /// predecessor, and a walk would silently stop after one record rather than
    /// report that it cannot answer.
    pub fn block_chain(&self, block_id: u32) -> Result<BlockChain<'_>> {
        if !self.spec.index_policy.block_offset_chain {
            return Err(Error::InvalidFormatSpec(
                "block_chain requires block_offset_chain",
            ));
        }
        Ok(BlockChain {
            spec: self.spec,
            snapshot: &self.snapshot,
            block_id,
            next: self.block_tails.tail(block_id),
            visited: 0,
        })
    }

    /// Decodes the record at `record_offset` as `T`.
    ///
    /// The companion to [`Self::block_chain`]: the walk yields offsets and
    /// entries, this turns one into a value. Positional and `&self`, so a
    /// non-resident block is read without anything entering the resident index.
    pub fn read_block_at<T: VarveBlock>(&self, record_offset: u64) -> Result<T> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let entry = read_record_entry_positional(self.spec, &self.snapshot, record_offset)?;
        if entry.block_id != T::ID {
            return Err(Error::UnregisteredBlock(entry.block_id));
        }
        if entry.block_version != T::VERSION {
            return Err(Error::BlockVersionMismatch {
                block_id: T::ID,
                expected: T::VERSION,
                actual: entry.block_version,
            });
        }
        let mut budget = MaterializationBudget::new(self.spec);
        let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
        budget.consume(logical_len)?;
        let payload = entry.read_logical_payload_snapshot(self.spec, &self.snapshot)?;
        budget.decode(&payload, T::ENDIAN.unwrap_or(self.spec.endian))
    }

    pub fn blocks<T: VarveBlock>(&self) -> Result<BlockVec<T>> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        // A non-resident block has no entries here, and an empty collection
        // would say "nothing was written" rather than "not through this door".
        crate::collections::ensure_resident_block::<T>(self.spec)?;
        // Filtered, so the count is not free: one pass to learn it, because
        // the charge and the reservation both have to happen before the copy.
        let count = self
            .index
            .iter()
            .filter(|entry| entry.block_id == T::ID)
            .count();
        let entries = clone_matching_entries(self.spec, &self.index, count, |entry| {
            entry.block_id == T::ID
        })?;
        Ok(BlockVec::new(self.spec, self.snapshot.clone(), entries))
    }

    pub fn blocks_migrated<From, To, M>(&self) -> Result<Vec<To>>
    where
        From: VarveBlock,
        To: VarveBlock,
        M: VarveMigration<From, To>,
    {
        if From::ID != To::ID {
            return Err(Error::MigrationBlockIdMismatch {
                from: From::ID,
                to: To::ID,
            });
        }
        let mut migrated = Vec::new();
        let mut budget = MaterializationBudget::new(self.spec);
        for entry in self
            .index
            .iter()
            .filter(|entry| entry.block_id == From::ID && entry.block_version == From::VERSION)
        {
            let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
            budget.consume(logical_len)?;
            let payload = entry.read_logical_payload_snapshot(self.spec, &self.snapshot)?;
            let from: From = budget.decode(&payload, From::ENDIAN.unwrap_or(self.spec.endian))?;
            migrated
                .try_reserve(1)
                .map_err(|_| Error::AllocationFailed {
                    resource: "migrated blocks",
                    requested: logical_len,
                })?;
            migrated.push(M::migrate(from)?);
        }
        Ok(migrated)
    }

    pub fn keyed_blocks<T>(&self) -> Result<KeyedBlockVec<T::Key, T>>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash + Clone,
    {
        // API2-03: compile-time keyedness contract (also covers the
        // VarveReader::keyed_blocks wrapper); registration is the runtime
        // backstop.
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let mut entries = Vec::new();
        let mut state: HashMap<T::Key, (MergeOrder, Option<RecordIndexEntry>)> = HashMap::new();
        let mut budget = MaterializationBudget::new(self.spec);
        for (record_ordinal, entry) in self.index.iter().enumerate() {
            // One record's materialization at a time. Every decoded block is
            // dropped once its key is taken; what survives the loop is index
            // entries and keys, each charged as it is taken through
            // `index_bytes_for_count`. See `MaterializationBudget`.
            budget.reset();
            let order = MergeOrder::for_record(0, entry.sequence, record_ordinal);
            match entry.block_id {
                id if id == T::ID => {
                    if entry.block_version != T::VERSION {
                        return Err(Error::BlockVersionMismatch {
                            block_id: T::ID,
                            expected: T::VERSION,
                            actual: entry.block_version,
                        });
                    }
                    let logical_len =
                        entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
                    budget.consume(logical_len)?;
                    let payload = entry.read_logical_payload_snapshot(self.spec, &self.snapshot)?;
                    let block: T =
                        budget.decode(&payload, T::ENDIAN.unwrap_or(self.spec.endian))?;
                    let key = block.key();
                    if should_apply(state.get(&key), order) {
                        let requested = index_bytes_for_count(state.len().saturating_add(1))?;
                        state.try_reserve(1).map_err(|_| Error::AllocationFailed {
                            resource: "keyed index",
                            requested,
                        })?;
                        state.insert(key, (order, Some(entry.clone())));
                    }
                    let requested = index_bytes_for_count(entries.len().saturating_add(1))?;
                    entries
                        .try_reserve(1)
                        .map_err(|_| Error::AllocationFailed {
                            resource: "block index",
                            requested,
                        })?;
                    entries.push(entry.clone());
                }
                TOMBSTONE_BLOCK_ID => {
                    let logical_len =
                        entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
                    budget.consume(logical_len)?;
                    let payload = entry.read_payload_snapshot(self.spec, &self.snapshot)?;
                    if let Some(key) =
                        decode_internal_key_payload::<T>(self.spec.endian, &payload, &mut budget)?
                        && should_apply(state.get(&key), order)
                    {
                        let requested = index_bytes_for_count(state.len().saturating_add(1))?;
                        state.try_reserve(1).map_err(|_| Error::AllocationFailed {
                            resource: "keyed index",
                            requested,
                        })?;
                        state.insert(key, (order, None));
                    }
                }
                _ => {}
            }
        }
        let mut by_key = HashMap::new();
        let requested = index_bytes_for_count(state.len())?;
        by_key
            .try_reserve(state.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "keyed index",
                requested,
            })?;
        for (key, (_, entry)) in state {
            if let Some(entry) = entry {
                by_key.insert(key, entry);
            }
        }
        let blocks = BlockVec::new(self.spec, self.snapshot.clone(), entries);
        Ok(KeyedBlockVec::from_parts(blocks, by_key))
    }

    pub fn materialized_keyed_blocks<T>(&self) -> Result<HashMap<T::Key, T>>
    where
        T: VarveMerge,
        T::Key: Eq + Hash,
    {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let mut state: HashMap<T::Key, (MergeOrder, Option<T>)> = HashMap::new();
        let mut budget = MaterializationBudget::new(self.spec);
        apply_merge_entries::<T>(
            self.spec,
            &self.snapshot,
            &self.index,
            MergeShard::single_file(),
            &mut state,
            &mut budget,
            u64::MAX,
        )?;
        let requested = allocation_bytes::<(T::Key, T)>(state.len(), "materialized keyed map")?;
        let mut values = HashMap::new();
        values
            .try_reserve(state.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "materialized keyed map",
                requested,
            })?;
        for (key, (_, value)) in state {
            if let Some(value) = value {
                values.insert(key, value);
            }
        }
        Ok(values)
    }

    pub fn scan(&self) -> impl Iterator<Item = BlockEvent> + '_ {
        self.index.iter().map(BlockEvent::from)
    }

    /// Verifies every record's stored checksum against its bytes.
    ///
    /// Available on demand so that choosing
    /// [`IntegrityVerification::OnDemand`] defers the announcement
    /// [`IntegrityVerification::AtOpen`] makes while opening, rather than
    /// losing it. Reads every payload, so it costs a full pass over the file;
    /// that cost is the reason it is not the default at open.
    ///
    /// **What "every record" covers.** Two passes, because two things hold the
    /// records: the resident index, and — for a block declared
    /// `resident: false` — that block's footer chain, which is the only way
    /// back to records the index deliberately does not mirror
    /// ([`Self::block_chain`]). The chain pass used to be missing, so a
    /// non-resident block's records were silently omitted; a file whose only
    /// appends were non-resident reported `Ok(0)`, indistinguishable from an
    /// empty file. Both passes call the same `read_payload_snapshot` the read
    /// path calls, so there is still exactly one verification in the crate.
    ///
    /// This is *not* bit-for-bit what `AtOpen` does. `AtOpen` verifies every
    /// physical record as scan frames it, including records after the commit
    /// boundary that this handle excludes; this pass answers for the records
    /// the handle can see.
    ///
    /// **Bounded by the snapshot.** The chain pass reads through the same
    /// snapshot as every other read on this handle, and a read-only handle
    /// sizes that snapshot from the last *resident* record. Under a commit
    /// policy that leaves no resident record after the block's newest one —
    /// [`CommitPolicy::None`], which is what a format declaring no commit
    /// policy gets — the walk therefore reports
    /// [`Error::SnapshotRangeOutOfBounds`] rather than a clean verdict over
    /// bytes it never read. That is the refusal [`Self::block_chain`] already
    /// gives on the same handle: such a block is unreachable read-only in that
    /// configuration, and only `verify_all`'s `Ok` said otherwise. A read-write
    /// handle sizes the snapshot from the file and is unaffected, as is any
    /// policy that ends the file with a resident record
    /// (`transaction_marker`). Measured, 12 non-resident records under
    /// `CommitPolicy::None`: read-write `Ok(12)`, read-only
    /// `Err(SnapshotRangeOutOfBounds { offset: 1347, len: 32, snapshot_len: 27 })`.
    ///
    /// Takes `&self` and reads positionally, so it can run on a shared handle
    /// while other readers use it. One entry and one payload are materialised
    /// at a time; nothing enters the resident index.
    ///
    /// Returns the number of records verified. A mismatch is
    /// [`Error::ChecksumMismatch`] naming the offending record's offset.
    pub fn verify_all(&self) -> Result<usize> {
        if self.spec.integrity_policy == IntegrityPolicy::None {
            return Ok(0);
        }
        let mut verified = 0usize;
        for entry in self.index.iter() {
            // `read_payload_snapshot` is the same check the read path performs,
            // which is exactly the point: there is one verification in the
            // crate, and this method chooses when it runs rather than adding a
            // second copy of it.
            let _ = entry.read_payload_snapshot(self.spec, &self.snapshot)?;
            verified += 1;
        }
        // A non-resident block's records are never in `self.index` (a
        // descriptor may only name a declared user block, and only ids below
        // `RESERVED_BLOCK_ID_START` can be declared), so this adds to the pass
        // above and cannot double-count it.
        for descriptor in self.spec.block_residency {
            if descriptor.resident {
                continue;
            }
            // `FormatSpec::validate` refuses a non-resident block without
            // `block_offset_chain`, so this cannot be the refusal `block_chain`
            // raises for a format that has no chain; it is propagated rather
            // than asserted away.
            for entry in self.block_chain(descriptor.block_id)? {
                let entry = entry?;
                let _ = entry.read_payload_snapshot(self.spec, &self.snapshot)?;
                verified += 1;
            }
        }
        Ok(verified)
    }

    /// A snapshot of every index entry, owned by the caller.
    ///
    /// **Owned rather than borrowed, and that is the point.** Returning
    /// `&[RecordIndexEntry]` required the entries to live contiguously inside
    /// this handle for as long as any caller held the borrow — which is exactly
    /// the requirement that keeps the whole index resident and makes a
    /// demand-filled store impossible. An owned snapshot only has to exist
    /// while its holder does.
    ///
    /// `IndexEntries` derefs to `[RecordIndexEntry]`, so `len()`, indexing,
    /// `iter()` and passing it where a slice is expected all read the same as
    /// before. What changed is who owns the bytes.
    ///
    /// **This allocates.** [`index_entries_into`](Self::index_entries_into)
    /// takes the caller's buffer instead and is the form to use in a loop: the
    /// storage is the caller's business, and a library that decides it for them
    /// allocates once per call for no reason.
    pub fn index_entries(&self) -> IndexEntries {
        let mut entries = Vec::new();
        // The infallible surface is kept because 100-odd call sites read it as
        // a slice; a caller that wants the refusal calls `_into`.
        let _ = self.index_entries_into(&mut entries);
        IndexEntries { entries }
    }

    /// Fills the caller's buffer with the record index.
    ///
    /// `out` is cleared and then extended, so a caller that reuses one buffer
    /// across calls allocates once and never again. The same shape the append
    /// path uses for record staging, and for the same reason: the library does
    /// not get to decide where the caller's data lives.
    ///
    /// The copy is charged against `ReadLimitKey::IndexBytes`, like the index
    /// it copies.
    pub fn index_entries_into(&self, out: &mut Vec<RecordIndexEntry>) -> Result<()> {
        out.clear();
        let count = self.index.len();
        let requested = index_bytes_for_count(count)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::IndexBytes, requested)?;
        out.try_reserve(count)
            .map_err(|_| Error::AllocationFailed {
                resource: "index entry snapshot",
                requested,
            })?;
        out.extend(self.index.iter().cloned());
        Ok(())
    }

    /// Builds the per-key tail offsets for `T` from the resident index.
    ///
    /// API3-05: every entry this admits is charged to
    /// [`crate::ReadLimits::max_keyed_tail_bytes`] *before* the memory is taken, so
    /// the configured ceiling bounds the **peak** of the build and not merely
    /// what is retained afterwards.
    ///
    /// This is the entry point `VarveFile::keyed_tail_map` uses to seed the
    /// resident keyed-tail cache - the cache the generic keyed API maintains
    /// and the one the generated keyed writers prime at construction through
    /// [`VarveFile::prime_keyed_tails`] (F-01). Before
    /// this charge existed, both were guarded by `try_reserve` alone: a file
    /// with `N` distinct keys forced an `N`-entry resident map whatever the
    /// configured ceiling said, and `max_keyed_tail_bytes` - including
    /// `UNTRUSTED`'s 256 MiB - only refused *further* growth within the
    /// session.
    ///
    /// The charged value is the structural peak of this function, which is
    /// larger than the map it returns: the build keeps a transient
    /// `(MergeOrder, u64)` per key so it can resolve the winning record, and
    /// that map is still alive when the returned map is reserved. The charge
    /// is therefore `n * size_of::<(T::Key, (MergeOrder, u64))>()` while the
    /// transient map grows to `n`, and the sum of both maps' inline storage at
    /// the final reservation. Opening a file consequently charges more than
    /// the steady-state map costs; the charge is a conservative model of what
    /// is actually allocated, not of what survives.
    ///
    /// This build can only account for inline storage, because the map it
    /// returns holds `T::Key` rather than a canonical byte payload; heap owned
    /// by a `T::Key` is not charged here. The *retained* cache this seeds is
    /// byte-keyed, and its charge does include the key payload heap it owns
    /// (see `KeyedTailMap::charge_for`).
    pub fn key_tail_offsets<T>(&self) -> Result<HashMap<T::Key, u64>>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        // API2-03: compile-time keyedness contract (also covers the
        // VarveReader/VarveWriter key_tail_offsets wrappers); registration is
        // the runtime backstop.
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let mut tails: HashMap<T::Key, (MergeOrder, u64)> = HashMap::new();
        let mut budget = MaterializationBudget::new(self.spec);
        for (record_ordinal, entry) in self.index.iter().enumerate() {
            // One record's materialization at a time (see
            // `MaterializationBudget`). Every decoded block is dropped once its
            // key is taken, so the peak here is one payload however many
            // records the block has; what the build retains is keys and
            // offsets, and those are charged to `max_keyed_tail_bytes` below.
            //
            // This is on the *write* path — `push_keyed` seeds the tail cache
            // from here — so a budget that drained across the walk could refuse
            // the first append to an undamaged file whose records for `T` total
            // more than `max_materialized_bytes` (`STANDARD`: 1 GiB).
            budget.reset();
            let order = MergeOrder::for_record(0, entry.sequence, record_ordinal);
            match entry.block_id {
                id if id == T::ID => {
                    let logical_len =
                        entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
                    budget.consume(logical_len)?;
                    let payload = entry.read_logical_payload_snapshot(self.spec, &self.snapshot)?;
                    let block: T =
                        budget.decode(&payload, T::ENDIAN.unwrap_or(self.spec.endian))?;
                    let key = block.key();
                    if tails.get(&key).is_none_or(|(old, _)| order >= *old) {
                        let requested = allocation_bytes::<(T::Key, (MergeOrder, u64))>(
                            tails.len().saturating_add(1),
                            "key tail offsets",
                        )?;
                        // API3-05: charge the growth before it is taken, so a
                        // file whose distinct key count alone exceeds the
                        // configured ceiling is refused during the build
                        // rather than after the whole map is resident.
                        self.spec
                            .read_limits
                            .check(ReadLimitKey::KeyedTailBytes, requested)?;
                        tails.try_reserve(1).map_err(|_| Error::AllocationFailed {
                            resource: "key tail offsets",
                            requested,
                        })?;
                        tails.insert(key, (order, entry.record_offset));
                    }
                }
                TOMBSTONE_BLOCK_ID => {
                    let logical_len =
                        entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
                    budget.consume(logical_len)?;
                    let payload = entry.read_payload_snapshot(self.spec, &self.snapshot)?;
                    if let Some(key) =
                        decode_internal_key_payload::<T>(self.spec.endian, &payload, &mut budget)?
                        && tails.get(&key).is_none_or(|(old, _)| order >= *old)
                    {
                        let requested = allocation_bytes::<(T::Key, (MergeOrder, u64))>(
                            tails.len().saturating_add(1),
                            "key tail offsets",
                        )?;
                        // API3-05: charge the growth before it is taken, so a
                        // file whose distinct key count alone exceeds the
                        // configured ceiling is refused during the build
                        // rather than after the whole map is resident.
                        self.spec
                            .read_limits
                            .check(ReadLimitKey::KeyedTailBytes, requested)?;
                        tails.try_reserve(1).map_err(|_| Error::AllocationFailed {
                            resource: "key tail offsets",
                            requested,
                        })?;
                        tails.insert(key, (order, entry.record_offset));
                    }
                }
                _ => {}
            }
        }
        let requested = allocation_bytes::<(T::Key, u64)>(tails.len(), "key tail offsets")?;
        // API3-05: the transient ordering map is still alive while the
        // returned map is reserved, so the peak charged here is the sum of
        // both.
        let peak =
            allocation_bytes::<(T::Key, (MergeOrder, u64))>(tails.len(), "key tail offsets")?
                .checked_add(requested)
                .ok_or(Error::AllocationFailed {
                    resource: "key tail offsets",
                    requested: u64::MAX,
                })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::KeyedTailBytes, peak)?;
        let mut offsets = HashMap::new();
        offsets
            .try_reserve(tails.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "key tail offsets",
                requested,
            })?;
        offsets.extend(tails.into_iter().map(|(key, (_, offset))| (key, offset)));
        Ok(offsets)
    }

    // -----------------------------------------------------------------------
    // Growing matrices: chunk routing
    // -----------------------------------------------------------------------

    /// The two gates every region path passes before addressing a cell.
    ///
    /// `ensure_fatal_access_allowed` is the layout-wide stop, and
    /// `ensure_commit_publishable` is the per-category quarantine. The chunk
    /// path consulted neither, so a file whose matrix had been fenced off was
    /// still readable and writable through any chunked row.
    fn ensure_chunk_access_allowed(&self, category: &str) -> Result<()> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::ensure_chunk_access_allowed(matrix, category)
    }

    /// Which chunk a row belongs to, and its row inside that chunk.
    ///
    /// `None` when no dimension grows, or when the row is inside the declared
    /// extent — chunk 0 is the matrix region, so those rows take the path they
    /// always took and this feature is invisible to them.
    fn chunk_for_row(&self, row: u64) -> Option<(u64, u64)> {
        let rows = self.spec.growing_rows_per_chunk()?;
        let chunk = row / rows;
        if chunk == 0 {
            return None;
        }
        Some((chunk, row % rows))
    }

    /// Cells per row for one matrix block, i.e. the extent of dimension 1.
    fn chunk_row_width(&self, block: &crate::format::MatrixBlockDescriptor) -> Result<u64> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        matrix
            .dimension(block.dimensions[1])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[1].to_string()))
    }

    /// Opens chunk `index`, sealing whatever chunk was open before it.
    ///
    /// Refuses a chunk older than the open one. That refusal is the whole
    /// contract of sealing: a value that arrives late is reported, not dropped.
    fn open_chunk_at(&mut self, index: u64) -> Result<()> {
        match &self.open_chunk {
            Some(open) if open.index == index => return Ok(()),
            Some(open) if open.index > index => {
                return Err(Error::MatrixChunkSealed {
                    chunk: index,
                    open: Some(open.index),
                });
            }
            Some(_) => self.seal_open_chunk()?,
            None => {}
        }
        let sealed = self.sealed_chunk_through_now()?;
        if let Some(sealed) = sealed
            && index <= sealed
        {
            return Err(Error::MatrixChunkSealed {
                chunk: index,
                open: self.open_chunk.as_ref().map(|open| open.index),
            });
        }
        let rows = self
            .spec
            .growing_rows_per_chunk()
            .ok_or(Error::MatrixLayoutMissing)?;
        let first_row = index
            .checked_mul(rows)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "matrix chunk first row",
            })?;
        let cell_crc = !matches!(self.spec.integrity_policy, IntegrityPolicy::None);
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(self.spec.matrix_blocks.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix chunk blocks",
                requested: self.spec.matrix_blocks.len() as u64,
            })?;
        for descriptor in self.spec.matrix_blocks {
            let width = self.chunk_row_width(descriptor)?;
            let cells = rows
                .checked_mul(width)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk cells",
                })?;
            self.spec
                .read_limits
                .check(ReadLimitKey::MatrixCells, cells)?;
            let slot_len = cells.checked_mul(descriptor.slot_stride).ok_or(
                Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk slot region",
                },
            )?;
            self.spec
                .read_limits
                .check(ReadLimitKey::MatrixSlotRegionLen, slot_len)?;
            let commit_len = cells.div_ceil(8);
            blocks.push(OpenChunkBlock {
                block_id: descriptor.block_id,
                stride: descriptor.slot_stride,
                cells,
                written: try_zeroed_vec(commit_len, "matrix chunk write map")?,
                commit: try_zeroed_vec(commit_len, "matrix chunk commit map")?,
                crc: if cell_crc {
                    try_zeroed_vec(
                        cells.checked_mul(MATRIX_CHUNK_CRC_LEN).ok_or(
                            Error::ResourceArithmeticOverflow {
                                resource: "matrix chunk checksum table",
                            },
                        )?,
                        "matrix chunk checksum table",
                    )?
                } else {
                    Vec::new()
                },
                slots: try_zeroed_vec(slot_len, "matrix chunk slot region")?,
            });
        }
        self.open_chunk = Some(OpenChunk {
            index,
            first_row,
            rows,
            compression: self.spec.chunk_compression,
            blocks,
            dirty: false,
        });
        Ok(())
    }

    /// The newest sealed chunk index.
    ///
    /// The directory's last element. The first version walked the resident
    /// index in reverse on every chunk transition and cached only a *positive*
    /// answer, so a writer whose chunks were never dirty-sealed repeated the
    /// full walk at every chunk boundary — on the append path.
    fn sealed_chunk_through_now(&mut self) -> Result<Option<u64>> {
        Ok(self.chunk_directory()?.newest())
    }

    /// Writes the open chunk as one ordinary internal record.
    ///
    /// **The chunk is released only after the record is on disk.** The first
    /// version `take()`d it up front and then ran three fallible steps — the
    /// encode, the payload ceiling, the append — so any failure destroyed the
    /// data *and* left `open_chunk` empty, which made a retried `flush()`
    /// return `Ok(())` for a chunk that no longer existed anywhere. Silent loss
    /// reported as success.
    ///
    /// **A chunk with no committed cell stays open rather than being dropped.**
    /// It was dropped before, so `write` → `flush` → `commit` lost the write on
    /// a chunked row while working on a region row, and the row could become
    /// permanently unwritable once a later chunk sealed past it. There is
    /// nothing to publish — a reader cannot see an uncommitted cell — so
    /// holding it costs a commit point nothing, and an idle flush still writes
    /// no record, which is the property this guard was for.
    fn seal_open_chunk(&mut self) -> Result<()> {
        let (payload, index, block_count, cell_crc, compressed_slots) = {
            let Some(chunk) = self.open_chunk.as_ref() else {
                return Ok(());
            };
            if !chunk.dirty {
                return Ok(());
            }
            (
                chunk.encode()?,
                chunk.index,
                u32::try_from(chunk.blocks.len()).map_err(|_| {
                    Error::ResourceArithmeticOverflow {
                        resource: "matrix chunk block count",
                    }
                })?,
                chunk.blocks.iter().any(|block| !block.crc.is_empty()),
                chunk.compression.is_some(),
            )
        };
        let payload_len = payload.len() as u64;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
        // Taken from the append itself, not from a second `metadata()` call:
        // the two would disagree if anything landed between them.
        let permit = self.ensure_write()?;
        let info = self.write_record_with_prev_key(
            &permit,
            MATRIX_CHUNK_BLOCK_ID,
            MATRIX_CHUNK_VERSION,
            RECORD_FLAG_INTERNAL,
            0,
            &payload,
            None,
        )?;
        let record_offset = info.record_offset;
        // On disk. Only now does the handle stop holding it.
        self.open_chunk = None;
        // Extend the directory rather than invalidate it: a writer sealing its
        // millionth chunk must not pay a rebuild, and the newest entry is what
        // answers "which chunks are sealed" on the next write.
        if let Some(directory) = self.chunk_directory.get_mut() {
            directory.note_sealed(ChunkLocator {
                index,
                record_offset,
                payload_len,
                block_count,
                cell_crc,
                compressed_slots,
            });
        }
        Ok(())
    }

    fn chunk_block_slice<T: VarveMatrixBlock>(
        &mut self,
        chunk_index: u64,
        local_row: u64,
        key: MatrixKey,
    ) -> Result<(usize, u64)> {
        // Every region path goes through these two before touching a cell and
        // the chunk path went through neither: a quarantined category stayed
        // writable through a chunked row, and a layout whose fatal-access gate
        // had fired was still addressable.
        self.ensure_chunk_access_allowed(T::CATEGORY)?;
        self.open_chunk_at(chunk_index)?;
        let width = self
            .spec
            .matrix_blocks
            .iter()
            .find(|block| block.block_id == T::ID)
            .ok_or(Error::MatrixBlockMissing(T::ID))
            .and_then(|block| self.chunk_row_width(block))?;
        if key.ch >= width {
            return Err(Error::MatrixKeyOutOfBounds {
                scan: key.scan,
                ch: key.ch,
            });
        }
        let ordinal = local_row
            .checked_mul(width)
            .and_then(|base| base.checked_add(key.ch))
            .ok_or(Error::InvalidMatrixLayout)?;
        let chunk = self.open_chunk.as_ref().ok_or(Error::InvalidMatrixChunk)?;
        let position = chunk
            .block_position(T::ID)
            .ok_or(Error::MatrixBlockMissing(T::ID))?;
        Ok((position, ordinal))
    }

    /// Writes one already-encoded cell into the open chunk.
    ///
    /// The shared tail of both write entry points: `write_matrix_cell` encodes
    /// and calls this, `write_matrix_cell_payload` calls it directly. Keeping
    /// one body is the point — the payload entry point spent its first version
    /// unrouted, refusing a chunked row with `MatrixKeyOutOfBounds` while
    /// `write_matrix_cell` accepted the same key.
    fn write_chunk_cell_payload<T: VarveMatrixBlock>(
        &mut self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
        payload: &[u8],
    ) -> Result<()> {
        let (position, ordinal) = self.chunk_block_slice::<T>(chunk_index, local_row, key)?;
        let chunk = self.open_chunk.as_mut().ok_or(Error::InvalidMatrixChunk)?;
        let block = &mut chunk.blocks[position];
        if payload.len() as u64 != block.stride {
            return Err(Error::MatrixSizeMismatch {
                expected: block.stride,
                actual: payload.len() as u64,
            });
        }
        let start = usize::try_from(
            ordinal
                .checked_mul(block.stride)
                .ok_or(Error::InvalidMatrixChunk)?,
        )
        .map_err(|_| Error::InvalidMatrixLayout)?;
        block.slots[start..start + payload.len()].copy_from_slice(payload);
        if !block.crc.is_empty() {
            let at = usize::try_from(
                ordinal
                    .checked_mul(MATRIX_CHUNK_CRC_LEN)
                    .ok_or(Error::InvalidMatrixChunk)?,
            )
            .map_err(|_| Error::InvalidMatrixLayout)?;
            let checksum = crc32_bytes(payload)?;
            block.crc[at..at + 4].copy_from_slice(&checksum.to_le_bytes());
        }
        let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
        block.written[byte] |= 1u8 << (ordinal % 8);
        Ok(())
    }

    pub fn write_matrix_cell<T: VarveMatrixBlock>(
        &mut self,
        key: MatrixKey,
        value: &T,
    ) -> Result<()> {
        let _permit = self.ensure_write()?;
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            let (position, ordinal) = self.chunk_block_slice::<T>(chunk_index, local_row, key)?;
            let stride = self
                .open_chunk
                .as_ref()
                .ok_or(Error::InvalidMatrixChunk)?
                .blocks[position]
                .stride;
            // Bounded exactly as the region path bounds it (DEF-02): one byte
            // past the stride is enough to report the true length, and no
            // further buffering is possible whatever the encode does.
            let encoded = match crate::codec::encode_to_vec_limited(
                value,
                T::ENDIAN.unwrap_or(self.spec.endian),
                stride.saturating_add(1),
                "matrix chunk slot payload",
            ) {
                Ok(encoded) => encoded,
                Err(Error::LimitExceeded { actual, .. }) => {
                    return Err(Error::MatrixSizeMismatch {
                        expected: stride,
                        actual,
                    });
                }
                Err(error) => return Err(error),
            };
            // One body, shared with `write_matrix_cell_payload`. This branch
            // used to keep its own copy of the store, and when the write
            // bitmap arrived only the shared one learned to set it — so the
            // typed write silently stopped marking cells written.
            let _ = (position, ordinal);
            return self.write_chunk_cell_payload::<T>(key, chunk_index, local_row, &encoded);
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::write_cell(self.spec, matrix, self.file.matrix_region(), key, value)
        };
        self.finish_matrix_mutation(result)
    }

    pub fn write_matrix_cell_payload<T: VarveMatrixBlock>(
        &mut self,
        key: MatrixKey,
        payload: &[u8],
    ) -> Result<()> {
        let _permit = self.ensure_write()?;
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            return self.write_chunk_cell_payload::<T>(key, chunk_index, local_row, payload);
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::write_cell_payload::<T>(
                self.spec,
                matrix,
                self.file.matrix_region(),
                key,
                payload,
            )
        };
        self.finish_matrix_mutation(result)
    }

    /// Copies one cell's bytes out of `source`; see the note on
    /// [`VarveWriter::copy_matrix_cell_bytes_from`] for why the source borrow is
    /// shared.
    pub fn copy_matrix_cell_bytes_from<From, To>(
        &mut self,
        source: &VarveFile,
        key: MatrixKey,
    ) -> Result<()>
    where
        From: VarveMatrixBlock,
        To: VarveMatrixBlock,
    {
        ensure_matrix_byte_copy_compatible::<From, To>()?;
        let payload = source.matrix_cell_payload::<From>(key)?;
        self.write_matrix_cell_payload::<To>(key, &payload)?;
        self.commit_matrix_cell::<To>(key)
    }

    /// Writes, syncs, commits, syncs the commit, then calls `hook`.
    ///
    /// The hook runs **after** the cell is committed and durable, so its
    /// failure cannot un-commit the cell. It is therefore reported as the
    /// typed published outcome [`Error::MatrixCommittedButHookFailed`],
    /// carrying the [`MatrixCommitEvent`] the hook was given.
    ///
    /// The commit sync is the only other step after the commit, and it is
    /// governed the same way (round 11): if it fails the cell is committed but
    /// its durability is unproven and the hook has not run, which is reported
    /// as [`Error::MatrixCommittedButDurabilityUnproven`], again carrying the
    /// event.
    ///
    /// Every other error variant means the cell was not committed by this
    /// call, so a result-driven retry can distinguish "nothing happened, retry
    /// the write" from "committed, retry only the notification" and from
    /// "committed, re-establish durability".
    ///
    /// The scope of that guarantee is a process that continues to run (F-06):
    /// both variants allocate two boxes after the commit, and a refused
    /// shape-sized allocation aborts rather than substituting another outcome.
    /// See "Allocator Failure And Published Outcomes" in
    /// `docs/durability-model.md`.
    pub fn write_matrix_cell_durable<T, F>(
        &mut self,
        key: MatrixKey,
        value: &T,
        hook: F,
    ) -> Result<()>
    where
        T: VarveMatrixBlock,
        F: FnOnce(MatrixCommitEvent) -> Result<()>,
    {
        let mut barrier = FileMatrixDurabilityBarrier;
        self.write_matrix_cell_durable_with_barrier(key, value, &mut barrier, hook)
    }

    /// [`Self::write_matrix_cell_durable`] with a caller-supplied durability
    /// barrier; the same post-commit contract applies, including
    /// [`Error::MatrixCommittedButHookFailed`] and
    /// [`Error::MatrixCommittedButDurabilityUnproven`].
    pub fn write_matrix_cell_durable_with_barrier<T, B, F>(
        &mut self,
        key: MatrixKey,
        value: &T,
        barrier: &mut B,
        hook: F,
    ) -> Result<()>
    where
        T: VarveMatrixBlock,
        B: MatrixDurabilityBarrier + ?Sized,
        F: FnOnce(MatrixCommitEvent) -> Result<()>,
    {
        // A chunked row has no durability point of its own: the cell lives in
        // memory until its chunk is sealed, and the seal is what a barrier
        // could make durable. Refused by name rather than left to fail as
        // `MatrixKeyOutOfBounds`, which said nothing about why.
        if self.chunk_for_row(key.scan).is_some() {
            return Err(Error::InvalidFormatSpec(
                "a per-cell durability barrier does not apply to a chunked row; \
                 a chunk becomes durable when it is sealed",
            ));
        }
        // INVARIANT 3 (F-04). The commit event is pure layout geometry - block
        // index, ordinal, slot offset and stride - so it is derived *before*
        // the authoritative commit rather than after it. Previously
        // `commit_event` ran after the commit was durable and could return a
        // bare `Err` (fatal-forensics gating, block lookup, ordinal
        // arithmetic) for a cell that was already committed and synced.
        let event = {
            let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::commit_event::<T>(self.spec, matrix, key)?
        };
        self.write_matrix_cell(key, value)?;
        // Pre-publication. Nothing is committed yet, so a failure here is a
        // plain refusal: the slot bytes may be on disk but the commit bit is
        // not, and an uncommitted slot is not visible to any reader.
        let sync_data = barrier.sync_matrix_data(self.file.matrix_region());
        self.poison_after_started_matrix_error(sync_data)?;
        // THE AUTHORITATIVE COMMIT. `commit_matrix_cell` puts the commit bit in
        // the file; from this line on the cell is committed and a reader that
        // opens the file after a clean process exit sees it. INVARIANT 3
        // therefore governs *every* remaining step of this function, and there
        // are exactly two: the commit sync and the hook. Both report typed
        // published outcomes; nothing else follows them.
        self.commit_matrix_cell::<T>(key)?;
        // Step 1 of 2 after the commit (round 11). This used to return a bare
        // `Err` (poison only), which is indistinguishable from "nothing
        // happened" even though the cell is committed and readable - the same
        // defect as F-04 one step earlier, and the same shape the round-10
        // sweep fixed in `commit_durable`. It now reports the matrix twin of
        // `CommittedButDurabilityUnproven`, carrying the committed event so
        // the caller can issue the notification after re-establishing
        // durability instead of repeating the write. Poisoning is retained: a
        // refused durability request mid-publication leaves this handle unfit
        // to continue, and the recovery is to reopen and `sync`.
        //
        // F-06, and the crate's one policy on allocator failure. Both post-
        // commit variants box their event and their source, so constructing
        // either one allocates twice after the cell is authoritative, and a
        // harness that failed the very next allocation terminated the process
        // at exactly this line while the reopened file held the committed
        // value. That is deliberate and is now stated rather than implied.
        // These are *shape-sized* allocations - 56 bytes, fixed by the type,
        // not by any file length or caller count - and the crate allocates
        // those infallibly, matching Rust's abort-on-OOM default; only
        // content-sized allocations are charged and `try_reserve`d into
        // `Error::AllocationFailed`. The consequence is the one the docs now
        // publish: an allocator refusal here ends the process instead of
        // returning a *different* outcome, so no caller ever observes a wrong
        // one, and the cell is committed on disk either way. Pre-staging the
        // boxes before the commit was considered and rejected: it cannot
        // remove `Box::new(source)` (the source is only produced by the
        // failure, and `Error` is recursive so it cannot be carried inline),
        // the caller must allocate to format or propagate the outcome anyway,
        // and it would put two allocations on the success path of every
        // durable cell write to serve a path that ends in `abort`. See
        // "Allocator Failure And Published Outcomes" in
        // `docs/durability-model.md`.
        if let Err(source) = barrier.sync_matrix_commit(self.file.matrix_region()) {
            self.poison.poison();
            return Err(Error::MatrixCommittedButDurabilityUnproven {
                event: Box::new(event),
                source: Box::new(source),
            });
        }
        // Step 2 of 2. The cell is committed and durable from here on. The hook is the
        // documented post-publication notification step, so its failure is a
        // *published* outcome, not a rollback: it is reported as
        // `Error::MatrixCommittedButHookFailed` carrying the committed event,
        // so a caller can retry the notification alone instead of repeating
        // the durable write and duplicating the hook's external work.
        hook(event.clone()).map_err(|source| Error::MatrixCommittedButHookFailed {
            event: Box::new(event),
            source: Box::new(source),
        })
    }

    /// Every sealed chunk as `(record offset, payload length)`, oldest first.
    ///
    /// Chunks are written in increasing index order — only the newest is open —
    /// so this list is sorted by chunk index, which is what makes the binary
    /// search below legal.
    ///
    /// **`payload_len` is the point, not a convenience.** Every offset the chunk
    /// decode computes comes from bytes in the file, and without the record's
    /// own extent to bound them a crafted chunk reads whatever it names. The
    /// framing already established this length and already charged it against
    /// `RecordPayloadLen`; the decode below treats it as the wall.
    fn build_chunk_directory(&self) -> Result<ChunkDirectory> {
        let mut chunks = Vec::new();
        for entry in self
            .index
            .iter()
            .filter(|entry| entry.block_id == MATRIX_CHUNK_BLOCK_ID)
        {
            let prefix = self.read_chunk_prefix(entry.record_offset, entry.payload_len)?;
            // `rows` and `first_row` were decoded and thrown away by every
            // caller, and the row width was taken from the record's `cells`
            // divided by *this spec's* `rows_per_chunk` — so a file written
            // under a different `rows_per_chunk` was not refused, it was
            // reinterpreted, and a read returned another row's bytes.
            let rows_per_chunk = self
                .spec
                .growing_rows_per_chunk()
                .ok_or(Error::InvalidMatrixChunk)?;
            if prefix.rows != rows_per_chunk
                || prefix.first_row
                    != prefix
                        .index
                        .checked_mul(rows_per_chunk)
                        .ok_or(Error::InvalidMatrixChunk)?
                || u64::from(prefix.block_count) != self.spec.matrix_blocks.len() as u64
            {
                return Err(Error::InvalidMatrixChunk);
            }
            let cell_crc = !matches!(self.spec.integrity_policy, IntegrityPolicy::None);
            if prefix.cell_crc != cell_crc
                || prefix.compressed_slots != self.spec.chunk_compression.is_some()
            {
                return Err(Error::InvalidMatrixChunk);
            }
            // The binary search below is only legal on an ordered list, and
            // the writer's ordering is a property of *this* build's sealing
            // rule, not of the bytes. A file from anywhere else must be
            // refused rather than searched.
            if chunks
                .last()
                .is_some_and(|last: &ChunkLocator| last.index >= prefix.index)
            {
                return Err(Error::InvalidMatrixChunk);
            }
            chunks.try_reserve(1).map_err(|_| Error::AllocationFailed {
                resource: "matrix chunk directory",
                requested: chunks.len() as u64 + 1,
            })?;
            chunks.push(ChunkLocator {
                index: prefix.index,
                record_offset: entry.record_offset,
                payload_len: entry.payload_len,
                block_count: prefix.block_count,
                cell_crc: prefix.cell_crc,
                compressed_slots: prefix.compressed_slots,
            });
        }
        Ok(ChunkDirectory { chunks })
    }

    /// The sealed-chunk directory, built at most once per handle.
    ///
    /// On failure nothing is cached, so a later call retries rather than
    /// caching a half-built answer.
    fn chunk_directory(&self) -> Result<&ChunkDirectory> {
        if let Some(directory) = self.chunk_directory.get() {
            return Ok(directory);
        }
        let built = self.build_chunk_directory()?;
        Ok(self.chunk_directory.get_or_init(|| built))
    }

    /// Reads one chunk record's prefix: its index, first row and row count.
    ///
    /// `MATRIX_CHUNK_PREFIX_LEN` bytes whatever the chunk holds. This is the
    /// read that keeps a cell lookup off the chunk's own size.
    fn read_chunk_prefix(&self, record_offset: u64, payload_len: u64) -> Result<ChunkPrefix> {
        if payload_len < MATRIX_CHUNK_PREFIX_LEN {
            return Err(Error::InvalidMatrixChunk);
        }
        let payload_offset = record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::InvalidMatrixChunk)?;
        let mut prefix = [0u8; MATRIX_CHUNK_PREFIX_LEN as usize];
        self.snapshot.read_exact_at(payload_offset, &mut prefix)?;
        note_chunk_bytes_read(MATRIX_CHUNK_PREFIX_LEN);
        if &prefix[..4] != MATRIX_CHUNK_MAGIC {
            return Err(Error::InvalidMatrixChunk);
        }
        if u16::from_le_bytes([prefix[4], prefix[5]]) != MATRIX_CHUNK_VERSION {
            return Err(Error::InvalidMatrixChunk);
        }
        let flags = u16::from_le_bytes([prefix[6], prefix[7]]);
        if flags & !MATRIX_CHUNK_KNOWN_FLAGS != 0 {
            return Err(Error::InvalidMatrixChunk);
        }
        let index = u64::from_le_bytes(prefix[8..16].try_into().expect("slice"));
        let first_row = u64::from_le_bytes(prefix[16..24].try_into().expect("slice"));
        let rows = u64::from_le_bytes(prefix[24..32].try_into().expect("slice"));
        let block_count = u32::from_le_bytes(prefix[32..36].try_into().expect("slice"));
        // The descriptor table has to fit in this record. Without this the field
        // is a bare `u32` from disk: `0xFFFF_FFFF` asked for a 137,438,953,440
        // byte reservation out of a 1 KB file, which `try_reserve` refused on
        // this host and a host with overcommit would have accepted and then
        // faulted in. No `ReadLimits` ceiling was charged, including under
        // `STANDARD`, which is the setting for untrusted input.
        let descriptors_len = MATRIX_CHUNK_BLOCK_DESC_LEN
            .checked_mul(u64::from(block_count))
            .ok_or(Error::InvalidMatrixChunk)?;
        if descriptors_len > payload_len - MATRIX_CHUNK_PREFIX_LEN {
            return Err(Error::InvalidMatrixChunk);
        }
        Ok(ChunkPrefix {
            index,
            first_row,
            rows,
            block_count,
            cell_crc: flags & MATRIX_CHUNK_FLAG_CELL_CRC != 0,
            compressed_slots: flags & MATRIX_CHUNK_FLAG_COMPRESSED_SLOTS != 0,
        })
    }

    /// Finds the sealed chunk with this index, by binary search over the chunk
    /// records — `O(log chunks)` prefix reads, and no state built at open.
    fn find_chunk_record(&self, chunk_index: u64) -> Result<Option<ChunkLocator>> {
        Ok(self.chunk_directory()?.find(chunk_index))
    }

    /// Where one block's commit map and slot region sit inside a chunk record.
    ///
    /// **Every offset here is derived from bytes in the file, so every one of
    /// them is bounded by `payload_len` — the record's own extent, which the
    /// framing established and already charged.** Without that wall a crafted
    /// `commit_len` moved a block's slot region onto a *neighbouring record* and
    /// a cell read returned that record's bytes decoded as a value: no error, no
    /// refusal, and `IntegrityPolicy::Crc32` did not catch it, because a
    /// positional cell read never verifies a footer under the default
    /// `IntegrityVerification::OnDemand`.
    fn chunk_block_location(
        &self,
        record_offset: u64,
        payload_len: u64,
        block_count: u32,
        cell_crc: bool,
        compressed_slots: bool,
        block_id: u32,
    ) -> Result<Option<ChunkBlockLocation>> {
        let payload_offset = record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::InvalidMatrixChunk)?;
        let payload_end = payload_offset
            .checked_add(payload_len)
            .ok_or(Error::InvalidMatrixChunk)?;
        // `read_chunk_prefix` already proved this fits; recomputed rather than
        // threaded so the bound and its use cannot drift apart.
        let descriptors_len = MATRIX_CHUNK_BLOCK_DESC_LEN
            .checked_mul(u64::from(block_count))
            .filter(|len| *len <= payload_len - MATRIX_CHUNK_PREFIX_LEN)
            .ok_or(Error::InvalidMatrixChunk)?;
        let descriptors_len_usize =
            usize::try_from(descriptors_len).map_err(|_| Error::InvalidMatrixChunk)?;
        // A stack buffer for every format anyone declares, so a cell read
        // allocates nothing. `MATRIX_CHUNK_INLINE_DESCRIPTORS` blocks is 18 at
        // 28 bytes each; past that the read is rare enough to take a heap
        // buffer, and a hostile `block_count` cannot force one because
        // `descriptors_len` is bounded by this record's own payload above.
        let mut inline = [0u8; MATRIX_CHUNK_INLINE_DESCRIPTOR_BYTES];
        let mut spilled;
        let descriptors: &mut [u8] = if descriptors_len_usize <= inline.len() {
            &mut inline[..descriptors_len_usize]
        } else {
            spilled = try_zeroed_vec(descriptors_len, "matrix chunk descriptors")?;
            &mut spilled[..descriptors_len_usize]
        };
        self.snapshot.read_exact_at(
            payload_offset
                .checked_add(MATRIX_CHUNK_PREFIX_LEN)
                .ok_or(Error::InvalidMatrixChunk)?,
            descriptors,
        )?;
        note_chunk_bytes_read(descriptors_len);
        let mut cursor = payload_offset
            .checked_add(MATRIX_CHUNK_PREFIX_LEN)
            .and_then(|offset| offset.checked_add(descriptors_len))
            .ok_or(Error::InvalidMatrixChunk)?;
        let mut found = None;
        for position in 0..block_count as usize {
            let base = position * MATRIX_CHUNK_BLOCK_DESC_LEN as usize;
            let id = u32::from_le_bytes(descriptors[base..base + 4].try_into().expect("slice"));
            let stride =
                u64::from_le_bytes(descriptors[base + 8..base + 16].try_into().expect("slice"));
            let cells =
                u64::from_le_bytes(descriptors[base + 16..base + 24].try_into().expect("slice"));
            let commit_len =
                u64::from_le_bytes(descriptors[base + 24..base + 32].try_into().expect("slice"));
            // The commit map, then the slots, then the next block — each must
            // land at or before the end of this record's payload.
            let crc_offset = cursor
                .checked_add(commit_len)
                .filter(|offset| *offset <= payload_end)
                .ok_or(Error::InvalidMatrixChunk)?;
            // A compressed block packs each checksum beside its cell inside the
            // sub-block, so there is no separate table to skip.
            let crc_len = if cell_crc && !compressed_slots {
                cells
                    .checked_mul(MATRIX_CHUNK_CRC_LEN)
                    .ok_or(Error::InvalidMatrixChunk)?
            } else {
                0
            };
            let slots_offset = crc_offset
                .checked_add(crc_len)
                .filter(|offset| *offset <= payload_end)
                .ok_or(Error::InvalidMatrixChunk)?;
            // A plain slot region's length is arithmetic. A compressed one's
            // is whatever its own index terminator says, which is a value from
            // the file and so is bounded by `payload_end` like every other.
            let slots_len = if compressed_slots {
                let count = sub_block_count(cells, compressed_cell_width(stride, cell_crc));
                let index_len = count
                    .checked_add(1)
                    .and_then(|entries| entries.checked_mul(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN))
                    .filter(|len| {
                        slots_offset
                            .checked_add(*len)
                            .is_some_and(|end| end <= payload_end)
                    })
                    .ok_or(Error::InvalidMatrixChunk)?;
                let terminator = slots_offset
                    .checked_add(index_len)
                    .and_then(|end| end.checked_sub(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN))
                    .ok_or(Error::InvalidMatrixChunk)?;
                let mut bytes = [0u8; MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN as usize];
                self.snapshot.read_exact_at(terminator, &mut bytes)?;
                note_chunk_bytes_read(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN);
                index_len
                    .checked_add(u64::from(u32::from_le_bytes(bytes)))
                    .ok_or(Error::InvalidMatrixChunk)?
            } else {
                cells.checked_mul(stride).ok_or(Error::InvalidMatrixChunk)?
            };
            let next = slots_offset
                .checked_add(slots_len)
                .filter(|offset| *offset <= payload_end)
                .ok_or(Error::InvalidMatrixChunk)?;
            // Every descriptor must be the one this spec would have written.
            // `stride` and `cells` were previously taken from the record and
            // never compared, so a crafted or foreign `cells` widened the row
            // pitch and every key resolved to a different cell.
            let descriptor = self
                .spec
                .matrix_blocks
                .iter()
                .find(|block| block.block_id == id)
                .ok_or(Error::InvalidMatrixChunk)?;
            let expected_cells = self
                .spec
                .growing_rows_per_chunk()
                .and_then(|rows| rows.checked_mul(self.chunk_row_width(descriptor).ok()?))
                .ok_or(Error::InvalidMatrixChunk)?;
            if stride != descriptor.slot_stride || cells != expected_cells {
                return Err(Error::InvalidMatrixChunk);
            }
            if id == block_id {
                // The commit map must cover the cells it claims, or the bit read
                // for a high ordinal lands in the slot region.
                if commit_len < cells.div_ceil(8) {
                    return Err(Error::InvalidMatrixChunk);
                }
                found = Some(ChunkBlockLocation {
                    commit_offset: cursor,
                    crc_offset: (cell_crc && !compressed_slots).then_some(crc_offset),
                    slots_offset,
                    slots_end: next,
                    compressed_slots,
                    cell_crc,
                    stride,
                    cells,
                });
            }
            cursor = next;
        }
        Ok(found)
    }

    /// The chunk-local ordinal of `key`, and where its block's bytes are.
    ///
    /// `Ok(None)` means no chunk record holds that row — which is not an error:
    /// a chunk nothing committed is never written, so its rows read exactly as
    /// a written chunk's uncommitted rows do.
    fn locate_chunk_cell<T: VarveMatrixBlock>(
        &self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<Option<(ChunkBlockLocation, u64)>> {
        let Some(locator) = self.find_chunk_record(chunk_index)? else {
            return Ok(None);
        };
        // `block_count` comes from the directory, so finding the chunk and
        // locating a block inside it cost one prefix read between them rather
        // than one each.
        let Some(location) = self.chunk_block_location(
            locator.record_offset,
            locator.payload_len,
            locator.block_count,
            locator.cell_crc,
            locator.compressed_slots,
            T::ID,
        )?
        else {
            return Err(Error::MatrixBlockMissing(T::ID));
        };
        // The spec's `rows_per_chunk` is legitimate as the divisor only because
        // the directory build refused any chunk whose own `rows` disagreed with
        // it. Before that check existed this line was the defect.
        let width = location
            .cells
            .checked_div(
                self.spec
                    .growing_rows_per_chunk()
                    .ok_or(Error::InvalidMatrixChunk)?,
            )
            .ok_or(Error::InvalidMatrixChunk)?;
        if key.ch >= width {
            return Err(Error::MatrixKeyOutOfBounds {
                scan: key.scan,
                ch: key.ch,
            });
        }
        let ordinal = local_row
            .checked_mul(width)
            .and_then(|base| base.checked_add(key.ch))
            .filter(|ordinal| *ordinal < location.cells)
            .ok_or(Error::InvalidMatrixChunk)?;
        Ok(Some((location, ordinal)))
    }

    fn chunk_cell_is_committed(&self, location: ChunkBlockLocation, ordinal: u64) -> Result<bool> {
        let mut byte = [0u8; 1];
        self.snapshot.read_exact_at(
            location
                .commit_offset
                .checked_add(ordinal / 8)
                .ok_or(Error::InvalidMatrixChunk)?,
            &mut byte,
        )?;
        note_chunk_bytes_read(1);
        Ok(byte[0] & (1u8 << (ordinal % 8)) != 0)
    }

    /// One cell out of the chunk this handle is still filling, if that is where
    /// the row lives.
    ///
    /// **The open chunk is not on disk.** Without this, a writer could not read
    /// back a cell it had just written and committed — `read_matrix_cell` said
    /// `MatrixNotCommitted` until the next seal, which is a write-then-read
    /// inconsistency and not a property anyone would want. Found by sweeping
    /// every matrix entry point against a chunked row; the tests that existed
    /// all read through a fresh reader after a flush and could not see it.
    /// The open chunk's block and cell ordinal for a key, or `None` when the row
    /// is not in the open chunk.
    ///
    /// Shared by the payload read and the status read so that answering "is it
    /// committed" costs no allocation.
    fn open_chunk_cell<T: VarveMatrixBlock>(
        &self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<Option<(&OpenChunkBlock, u64)>> {
        let Some(chunk) = self.open_chunk.as_ref() else {
            return Ok(None);
        };
        if chunk.index != chunk_index {
            return Ok(None);
        }
        let Some(position) = chunk.block_position(T::ID) else {
            return Err(Error::MatrixBlockMissing(T::ID));
        };
        let block = &chunk.blocks[position];
        let width = block
            .cells
            .checked_div(chunk.rows)
            .ok_or(Error::InvalidMatrixChunk)?;
        if key.ch >= width {
            return Err(Error::MatrixKeyOutOfBounds {
                scan: key.scan,
                ch: key.ch,
            });
        }
        let ordinal = local_row
            .checked_mul(width)
            .and_then(|base| base.checked_add(key.ch))
            .filter(|ordinal| *ordinal < block.cells)
            .ok_or(Error::InvalidMatrixChunk)?;
        Ok(Some((block, ordinal)))
    }

    fn read_open_chunk_cell_payload<T: VarveMatrixBlock>(
        &self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<Option<Vec<u8>>> {
        let Some((block, ordinal)) = self.open_chunk_cell::<T>(key, chunk_index, local_row)? else {
            return Ok(None);
        };
        let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
        if block.commit[byte] & (1u8 << (ordinal % 8)) == 0 {
            return Err(Error::MatrixNotCommitted);
        }
        let start = usize::try_from(
            ordinal
                .checked_mul(block.stride)
                .ok_or(Error::InvalidMatrixChunk)?,
        )
        .map_err(|_| Error::InvalidMatrixLayout)?;
        let len = usize::try_from(block.stride).map_err(|_| Error::InvalidMatrixLayout)?;
        Ok(Some(block.slots[start..start + len].to_vec()))
    }

    /// Whether the open chunk holds a committed value for this cell.
    ///
    /// `Ok(None)` means the row is not in the open chunk, so the sealed records
    /// own the answer.
    fn open_chunk_cell_status<T: VarveMatrixBlock>(
        &self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<Option<MatrixCellStatus>> {
        // Reads the commit bit and stops. The first version answered this
        // boolean by calling `read_open_chunk_cell_payload`, which copies the
        // slot into a fresh `Vec` — a malloc, a memcpy and a free per status
        // call, for data sitting in memory two fields away.
        let Some((block, ordinal)) = self.open_chunk_cell::<T>(key, chunk_index, local_row)? else {
            return Ok(None);
        };
        let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
        Ok(Some(if block.commit[byte] & (1u8 << (ordinal % 8)) == 0 {
            MatrixCellStatus::NotCommitted
        } else {
            MatrixCellStatus::Committed
        }))
    }

    /// One cell out of a sealed chunk, read positionally.
    ///
    /// Two small reads and one `stride`-byte read. The chunk itself is never
    /// materialised, whatever it holds.
    fn read_chunk_cell_payload<T: VarveMatrixBlock>(
        &self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<Vec<u8>> {
        if let Some(payload) =
            self.read_open_chunk_cell_payload::<T>(key, chunk_index, local_row)?
        {
            return Ok(payload);
        }
        let Some((location, ordinal)) = self.locate_chunk_cell::<T>(key, chunk_index, local_row)?
        else {
            return Err(Error::MatrixNotCommitted);
        };
        if !self.chunk_cell_is_committed(location, ordinal)? {
            return Err(Error::MatrixNotCommitted);
        }
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, location.stride)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::MaterializedBytes, location.stride)?;
        let payload = if location.compressed_slots {
            let (payload, stored) = self.read_compressed_chunk_cell(location, ordinal)?;
            // The checksum came out of the same decode as the cell, so a
            // damaged sub-block cannot produce a matching pair.
            if let Some(expected) = stored {
                let actual = crc32_bytes(&payload)?;
                if actual != expected {
                    return Err(Error::MatrixChecksumMismatch {
                        offset: location.slots_offset,
                        expected,
                        actual,
                    });
                }
            }
            return Ok(payload);
        } else {
            let mut payload = try_zeroed_vec(location.stride, "matrix chunk slot payload")?;
            self.snapshot.read_exact_at(
                location
                    .slots_offset
                    .checked_add(
                        ordinal
                            .checked_mul(location.stride)
                            .ok_or(Error::InvalidMatrixChunk)?,
                    )
                    .ok_or(Error::InvalidMatrixChunk)?,
                &mut payload,
            )?;
            note_chunk_bytes_read(location.stride);
            payload
        };
        self.verify_chunk_cell(location, ordinal, &payload)?;
        Ok(payload)
    }

    /// One cell out of a compressed slot region.
    ///
    /// The sub-block is arithmetic — `ordinal / cells_per_sub_block(stride)` —
    /// so finding it costs two four-byte reads of the offset index and no
    /// search. What it costs over a plain chunk is exactly one sub-block
    /// decode, and that is the whole price of `with_chunk_compression`.
    ///
    /// The per-cell checksum still guards the result: it is computed over the
    /// *uncompressed* cell bytes and stored plain, so a corrupted sub-block
    /// decodes to something whose checksum does not match, and the caller sees
    /// the mismatch rather than the bytes.
    fn read_compressed_chunk_cell(
        &self,
        location: ChunkBlockLocation,
        ordinal: u64,
    ) -> Result<(Vec<u8>, Option<u32>)> {
        let cell_crc = location.cell_crc;
        let width = compressed_cell_width(location.stride, cell_crc);
        let per = cells_per_sub_block(width);
        let count = sub_block_count(location.cells, width);
        let index = ordinal / per;
        if index >= count {
            return Err(Error::InvalidMatrixChunk);
        }
        let index_len = count
            .checked_add(1)
            .and_then(|entries| entries.checked_mul(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN))
            .ok_or(Error::InvalidMatrixChunk)?;
        let entry_at = location
            .slots_offset
            .checked_add(
                index
                    .checked_mul(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN)
                    .ok_or(Error::InvalidMatrixChunk)?,
            )
            .ok_or(Error::InvalidMatrixChunk)?;
        let mut pair = [0u8; 2 * MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN as usize];
        self.snapshot.read_exact_at(entry_at, &mut pair)?;
        note_chunk_bytes_read(2 * MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN);
        let start = u64::from(u32::from_le_bytes(pair[..4].try_into().expect("slice")));
        let end = u64::from(u32::from_le_bytes(pair[4..].try_into().expect("slice")));
        if end < start {
            return Err(Error::InvalidMatrixChunk);
        }
        let stored_len = end - start;
        let data_at = location
            .slots_offset
            .checked_add(index_len)
            .and_then(|base| base.checked_add(start))
            .filter(|at| {
                at.checked_add(stored_len)
                    .is_some_and(|end| end <= location.slots_end)
            })
            .ok_or(Error::InvalidMatrixChunk)?;

        // The uncompressed extent of this sub-block: a full one everywhere but
        // possibly the last.
        let cells_here = per.min(location.cells.saturating_sub(index * per));
        let raw_len = cells_here
            .checked_mul(width)
            .ok_or(Error::InvalidMatrixChunk)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::MaterializedBytes, raw_len)?;

        let mut stored = try_zeroed_vec(stored_len, "matrix chunk sub-block")?;
        self.snapshot.read_exact_at(data_at, &mut stored)?;
        note_chunk_bytes_read(stored_len);

        // Equal lengths mean the writer stored it raw, which it does whenever
        // compressing did not shrink it. The writer never emits a compressed
        // sub-block of exactly the uncompressed length, so this is unambiguous.
        let raw = if stored_len == raw_len {
            stored
        } else {
            let compression = self
                .spec
                .chunk_compression
                .ok_or(Error::InvalidMatrixChunk)?;
            let decoded = decompress_with_algorithm(compression.algorithm, &stored, raw_len)?;
            if decoded.len() as u64 != raw_len {
                return Err(Error::DecompressedLengthMismatch {
                    expected: raw_len,
                    actual: decoded.len() as u64,
                });
            }
            decoded
        };

        let at = usize::try_from(
            (ordinal % per)
                .checked_mul(width)
                .ok_or(Error::InvalidMatrixChunk)?,
        )
        .map_err(|_| Error::InvalidMatrixChunk)?;
        let stride = usize::try_from(location.stride).map_err(|_| Error::InvalidMatrixChunk)?;
        let payload = raw
            .get(at..at + stride)
            .map(<[u8]>::to_vec)
            .ok_or(Error::InvalidMatrixChunk)?;
        let stored = if cell_crc {
            let bytes = raw
                .get(at + stride..at + stride + 4)
                .ok_or(Error::InvalidMatrixChunk)?;
            Some(u32::from_le_bytes(bytes.try_into().expect("slice")))
        } else {
            None
        };
        Ok((payload, stored))
    }

    /// Checks one chunked cell against the checksum stored beside it.
    ///
    /// **The record footer's crc does not cover this read.** It covers the
    /// record, and it is verified on a *record* read; a positional cell read is
    /// not one, so under the default `IntegrityVerification::OnDemand` a single
    /// flipped bit in a sealed chunk came back as data with no error, while the
    /// identical flip one row earlier — in the matrix region, which keeps
    /// per-cell checksums — was refused. A chunk keeps them now, for the same
    /// reason and in the same shape.
    fn verify_chunk_cell(
        &self,
        location: ChunkBlockLocation,
        ordinal: u64,
        payload: &[u8],
    ) -> Result<()> {
        let Some(crc_offset) = location.crc_offset else {
            return Ok(());
        };
        let at = crc_offset
            .checked_add(
                ordinal
                    .checked_mul(MATRIX_CHUNK_CRC_LEN)
                    .ok_or(Error::InvalidMatrixChunk)?,
            )
            .ok_or(Error::InvalidMatrixChunk)?;
        let mut stored = [0u8; MATRIX_CHUNK_CRC_LEN as usize];
        self.snapshot.read_exact_at(at, &mut stored)?;
        note_chunk_bytes_read(MATRIX_CHUNK_CRC_LEN);
        let expected = u32::from_le_bytes(stored);
        let actual = crc32_bytes(payload)?;
        if actual != expected {
            return Err(Error::MatrixChecksumMismatch {
                offset: location
                    .slots_offset
                    .checked_add(
                        ordinal
                            .checked_mul(location.stride)
                            .ok_or(Error::InvalidMatrixChunk)?,
                    )
                    .ok_or(Error::InvalidMatrixChunk)?,
                expected,
                actual,
            });
        }
        Ok(())
    }

    pub fn read_matrix_cell<T: VarveMatrixBlock>(&self, key: MatrixKey) -> Result<T> {
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            let payload = self.read_chunk_cell_payload::<T>(key, chunk_index, local_row)?;
            return crate::codec::decode_from_slice(
                &payload,
                T::ENDIAN.unwrap_or(self.spec.endian),
            );
        }
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::read_cell(self.spec, matrix, self.file.matrix_region_reader(), key)
    }

    /// Whether a chunked cell has a committed value.
    ///
    /// Answers for a chunked row the same way the region answers for a region
    /// row: a value that never arrived is `NotCommitted`, permanently and by
    /// design. Sealing asks no question about completeness.
    fn chunked_cell_status<T: VarveMatrixBlock>(
        &self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<MatrixCellStatus> {
        if let Some(status) = self.open_chunk_cell_status::<T>(key, chunk_index, local_row)? {
            return Ok(status);
        }
        match self.locate_chunk_cell::<T>(key, chunk_index, local_row)? {
            Some((location, ordinal)) if self.chunk_cell_is_committed(location, ordinal)? => {
                Ok(MatrixCellStatus::Committed)
            }
            _ => Ok(MatrixCellStatus::NotCommitted),
        }
    }

    pub fn matrix_cell_payload<T: VarveMatrixBlock>(&self, key: MatrixKey) -> Result<Vec<u8>> {
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            return self.read_chunk_cell_payload::<T>(key, chunk_index, local_row);
        }
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::read_cell_payload::<T>(
            self.spec,
            matrix,
            self.file.matrix_region_reader(),
            key,
        )
    }

    pub fn matrix_aux_len(&self, name: &str) -> Result<u64> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::aux_len(matrix, name)
    }

    pub fn read_matrix_aux(&self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::read_aux_at_len(
            matrix,
            self.file.matrix_region_reader(),
            self.snapshot.len(),
            name,
            offset,
            len,
        )
    }

    pub fn write_matrix_aux(&mut self, name: &str, offset: u64, payload: &[u8]) -> Result<()> {
        let _permit = self.ensure_write()?;
        let result = {
            let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::write_aux_at_len(
                matrix,
                self.file.matrix_region(),
                self.snapshot.len(),
                name,
                offset,
                payload,
            )
        };
        self.finish_matrix_mutation(result)
    }

    pub fn matrix_cell_status<T: VarveMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            return self.chunked_cell_status::<T>(key, chunk_index, local_row);
        }
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::cell_status::<T>(self.spec, matrix, key)
    }

    pub fn commit_matrix_cell<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        let _permit = self.ensure_write()?;
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            let (position, ordinal) = self.chunk_block_slice::<T>(chunk_index, local_row, key)?;
            let chunk = self.open_chunk.as_mut().ok_or(Error::InvalidMatrixChunk)?;
            let block = &mut chunk.blocks[position];
            let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
            let bit = 1u8 << (ordinal % 8);
            // The region path refuses a commit of a cell that was never
            // written, and answers from its write bitmap. This asked the slot
            // bytes instead — "are they all zero" — which is not the same
            // question: a cell holding a legitimate zero could never be
            // committed.
            if block.commit[byte] & bit == 0 && block.written[byte] & bit == 0 {
                return Err(Error::MatrixCellNotWritten);
            }
            block.commit[byte] |= bit;
            chunk.dirty = true;
            return Ok(());
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::commit_cell::<T>(self.spec, matrix, self.file.matrix_region(), key)
        };
        self.finish_matrix_mutation(result)
    }

    /// Clears one chunked cell: its commit bit and its slot bytes.
    ///
    /// Only in the open chunk. A sealed chunk is a written record, and a record
    /// is not rewritten — the same refusal a late write gets, for the same
    /// reason.
    fn clear_chunk_cell<T: VarveMatrixBlock>(
        &mut self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<()> {
        match &self.open_chunk {
            Some(open) if open.index == chunk_index => {}
            _ => {
                // `open` is what the caller may still write to. With no chunk
                // open there is none, and the first version reported the
                // refused chunk as its own opener — `{ chunk: 3, open: 3 }`,
                // which reads as a contradiction.
                return Err(Error::MatrixChunkSealed {
                    chunk: chunk_index,
                    open: self.open_chunk.as_ref().map(|open| open.index),
                });
            }
        }
        let (position, ordinal) = self.chunk_block_slice::<T>(chunk_index, local_row, key)?;
        let chunk = self.open_chunk.as_mut().ok_or(Error::InvalidMatrixChunk)?;
        let block = &mut chunk.blocks[position];
        let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
        block.commit[byte] &= !(1u8 << (ordinal % 8));
        block.written[byte] &= !(1u8 << (ordinal % 8));
        let start = usize::try_from(
            ordinal
                .checked_mul(block.stride)
                .ok_or(Error::InvalidMatrixChunk)?,
        )
        .map_err(|_| Error::InvalidMatrixLayout)?;
        let len = usize::try_from(block.stride).map_err(|_| Error::InvalidMatrixLayout)?;
        block.slots[start..start + len].fill(0);
        Ok(())
    }

    /// [`Self::clear_chunk_cell`] addressed by block id rather than by type, for
    /// the by-category entry point.
    fn clear_chunk_cell_by_id(
        &mut self,
        block_id: u32,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<()> {
        match &self.open_chunk {
            Some(open) if open.index == chunk_index => {}
            _ => {
                // `open` is what the caller may still write to. With no chunk
                // open there is none, and the first version reported the
                // refused chunk as its own opener — `{ chunk: 3, open: 3 }`,
                // which reads as a contradiction.
                return Err(Error::MatrixChunkSealed {
                    chunk: chunk_index,
                    open: self.open_chunk.as_ref().map(|open| open.index),
                });
            }
        }
        let chunk = self.open_chunk.as_mut().ok_or(Error::InvalidMatrixChunk)?;
        let position = chunk
            .block_position(block_id)
            .ok_or(Error::MatrixBlockMissing(block_id))?;
        let rows = chunk.rows;
        let block = &mut chunk.blocks[position];
        let width = block
            .cells
            .checked_div(rows)
            .ok_or(Error::InvalidMatrixChunk)?;
        if key.ch >= width {
            return Err(Error::MatrixKeyOutOfBounds {
                scan: key.scan,
                ch: key.ch,
            });
        }
        let ordinal = local_row
            .checked_mul(width)
            .and_then(|base| base.checked_add(key.ch))
            .filter(|ordinal| *ordinal < block.cells)
            .ok_or(Error::InvalidMatrixChunk)?;
        let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
        block.commit[byte] &= !(1u8 << (ordinal % 8));
        block.written[byte] &= !(1u8 << (ordinal % 8));
        let start = usize::try_from(
            ordinal
                .checked_mul(block.stride)
                .ok_or(Error::InvalidMatrixChunk)?,
        )
        .map_err(|_| Error::InvalidMatrixLayout)?;
        let len = usize::try_from(block.stride).map_err(|_| Error::InvalidMatrixLayout)?;
        block.slots[start..start + len].fill(0);
        Ok(())
    }

    pub fn clear_matrix_cell<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        let _permit = self.ensure_write()?;
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            return self.clear_chunk_cell::<T>(key, chunk_index, local_row);
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::clear_cell::<T>(self.spec, matrix, self.file.matrix_region(), key)
        };
        self.finish_matrix_mutation(result)
    }

    pub fn clear_matrix_cell_by_category(&mut self, category: &str, key: MatrixKey) -> Result<()> {
        let _permit = self.ensure_write()?;
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            let block_id = self
                .spec
                .matrix_blocks
                .iter()
                .find(|block| block.category == category)
                .map(|block| block.block_id)
                .ok_or_else(|| Error::MatrixCommitMissing(category.to_string()))?;
            return self.clear_chunk_cell_by_id(block_id, key, chunk_index, local_row);
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::clear_cell_by_category(
                self.spec,
                matrix,
                self.file.matrix_region(),
                category,
                key,
            )
        };
        self.finish_matrix_mutation(result)
    }

    /// Clears every committed cell of a category and reports how many.
    ///
    /// **Refused for a growing matrix with sealed chunks.** It cleared the
    /// matrix region only, so a caller asking for a clean category got one
    /// silently: chunked rows stayed committed, stayed readable, and were not
    /// in the count. A sealed chunk is a written record and records are not
    /// rewritten, so there is no clearing it — saying so is the only honest
    /// answer. The open chunk *is* cleared, and counted.
    pub fn clear_matrix_category(&mut self, category: &str) -> Result<u64> {
        let _permit = self.ensure_write()?;
        let mut cleared = 0u64;
        if self.spec.growing_matrix.is_some() {
            if !self.chunk_directory()?.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "clear_matrix_category cannot clear a sealed chunk; a sealed chunk is a \
                     written record",
                ));
            }
            cleared = self.clear_open_chunk_category(category)?;
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::clear_category(self.spec, matrix, self.file.matrix_region(), category)
        };
        Ok(cleared + self.finish_matrix_mutation(result)?)
    }

    /// Clears one category's cells in the open chunk, returning how many.
    fn clear_open_chunk_category(&mut self, category: &str) -> Result<u64> {
        let Some(block_id) = self
            .spec
            .matrix_blocks
            .iter()
            .find(|block| block.category == category)
            .map(|block| block.block_id)
        else {
            return Ok(0);
        };
        let Some(chunk) = self.open_chunk.as_mut() else {
            return Ok(0);
        };
        let Some(position) = chunk.block_position(block_id) else {
            return Ok(0);
        };
        let block = &mut chunk.blocks[position];
        let cleared = block
            .commit
            .iter()
            .map(|byte| u64::from(byte.count_ones()))
            .sum();
        block.commit.fill(0);
        block.written.fill(0);
        block.slots.fill(0);
        block.crc.fill(0);
        Ok(cleared)
    }

    pub fn apply_matrix_recovery_action(&mut self, action: &MatrixRecoveryAction) -> Result<()> {
        match action {
            MatrixRecoveryAction::ClearCell { category, key } => {
                self.clear_matrix_cell_by_category(category, *key)
            }
            MatrixRecoveryAction::ClearCategory { category } => {
                self.clear_matrix_category(category).map(|_| ())
            }
            MatrixRecoveryAction::RebuildCommitMap { .. } => Err(Error::InvalidFormatSpec(
                "typed rebuild requires rebuild_matrix_commit_from_crc",
            )),
            MatrixRecoveryAction::Resume
            | MatrixRecoveryAction::Restart
            | MatrixRecoveryAction::DiscardSidecar => Ok(()),
        }
    }

    /// Rebuilds a commit map from the per-cell checksum table.
    ///
    /// **Refused for a format with a growing dimension.** The table it reads is
    /// the matrix region's; a chunk carries no per-cell checksums of its own —
    /// its record footer crc covers the whole payload instead. Without this
    /// refusal the call returned `Ok(0)` on a file full of chunks, which reads
    /// as "the commit map was already right" rather than "this answered for the
    /// region and nothing else".
    pub fn rebuild_matrix_commit_from_crc<T: VarveMatrixBlock>(&mut self) -> Result<u64> {
        let _permit = self.ensure_write()?;
        if self.spec.growing_matrix.is_some() {
            return Err(Error::InvalidFormatSpec(
                "rebuild_matrix_commit_from_crc covers the matrix region only; \
                 a growing matrix's chunks carry no per-cell checksum table",
            ));
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::rebuild_commit_map_from_crc::<T>(
                self.spec,
                matrix,
                self.file.matrix_region(),
            )
        };
        self.finish_matrix_mutation(result)
    }

    pub fn is_matrix_single_committed(&self, name: &str) -> Result<bool> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::is_single_committed(matrix, name)
    }

    pub fn set_matrix_single_committed(&mut self, name: &str, value: bool) -> Result<()> {
        let _permit = self.ensure_write()?;
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::set_single_committed(matrix, self.file.matrix_region(), name, value)
        };
        self.finish_matrix_mutation(result)
    }

    pub fn is_matrix_channel_committed(&self, name: &str, channel: u64) -> Result<bool> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::is_channel_committed(matrix, name, channel)
    }

    pub fn set_matrix_channel_committed(
        &mut self,
        name: &str,
        channel: u64,
        value: bool,
    ) -> Result<()> {
        let _permit = self.ensure_write()?;
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::set_channel_committed(
                matrix,
                self.file.matrix_region(),
                name,
                channel,
                value,
            )
        };
        self.finish_matrix_mutation(result)
    }

    /// What a resumed writer should do with this category.
    ///
    /// A growing matrix with an unsealed chunk is never `Clean`: the chunk is
    /// live state this handle holds and the next handle will not, so reporting
    /// a clean category over it told a caller the acquisition had finished when
    /// it had not.
    pub fn matrix_resume_signal(&self, category: &str) -> Result<MatrixResumeSignal> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        let signal = crate::matrix::resume_signal(matrix, category)?;
        if self.open_chunk.is_some() && matches!(signal, MatrixResumeSignal::Clean) {
            return Ok(MatrixResumeSignal::Partial {
                committed: 0,
                total: 0,
            });
        }
        Ok(signal)
    }

    pub fn matrix_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::sidecar_resume_signal(matrix, category, sidecar_path.as_ref().exists())
    }

    pub fn matrix_verified_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.matrix_verified_sidecar_resume_signal_inner(category, sidecar_path.as_ref(), None)
    }

    pub fn matrix_verified_sidecar_resume_signal_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<MatrixResumeSignal> {
        self.matrix_verified_sidecar_resume_signal_inner(
            category,
            sidecar_path.as_ref(),
            Some(expected_generation),
        )
    }

    fn matrix_verified_sidecar_resume_signal_inner(
        &self,
        category: &str,
        path: &Path,
        expected_generation: Option<u64>,
    ) -> Result<MatrixResumeSignal> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        if !path.exists() {
            return crate::matrix::sidecar_resume_signal(matrix, category, false);
        }
        let identity = self.matrix_native_identity()?;
        match read_matrix_sidecar_file(self.spec, category, path, expected_generation, identity) {
            Ok(_) => crate::matrix::sidecar_resume_signal(matrix, category, true),
            Err(Error::IntegrityFeatureDisabled) => Err(Error::IntegrityFeatureDisabled),
            Err(error @ Error::LimitExceeded { .. })
            | Err(error @ Error::MissingResourceLimit { .. })
            | Err(error @ Error::TrustedUnboundedRequiresExplicitApi { .. })
            | Err(error @ Error::ResourceArithmeticOverflow { .. })
            | Err(error @ Error::LengthOverflow { .. })
            | Err(error @ Error::AllocationFailed { .. }) => Err(error),
            Err(_) => Ok(MatrixResumeSignal::DiscardRecommended),
        }
    }

    /// Publishes a matrix resume sidecar bound to this native file.
    ///
    /// Ordering contract: the native matrix data this sidecar summarizes must
    /// already be durable (native -> sidecar). Callers write and sync native
    /// cells/commits first, then call this, so a sidecar can never advertise
    /// progress that is not yet present in the native file. Publication itself
    /// is a same-directory temp write + fsync + atomic replace + parent sync,
    /// so a crash mid-publish leaves the previous sidecar intact.
    pub fn write_matrix_sidecar<P: AsRef<Path>>(
        &mut self,
        category: &str,
        sidecar_path: P,
        generation: u64,
        payload: &[u8],
    ) -> Result<MatrixSidecarManifest> {
        let _permit = self.ensure_write()?;
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::resume_signal(matrix, category)?;
        let identity = self.matrix_native_identity()?;
        write_matrix_sidecar_file(
            self.spec,
            category,
            sidecar_path,
            generation,
            payload,
            identity,
        )
    }

    pub fn read_matrix_sidecar<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::resume_signal(matrix, category)?;
        let identity = self.matrix_native_identity()?;
        read_matrix_sidecar_file(self.spec, category, sidecar_path, None, identity)
    }

    pub fn read_matrix_sidecar_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::resume_signal(matrix, category)?;
        let identity = self.matrix_native_identity()?;
        read_matrix_sidecar_file(
            self.spec,
            category,
            sidecar_path,
            Some(expected_generation),
            identity,
        )
    }

    /// Computes the native-file identity a matrix sidecar is bound to.
    ///
    /// Reuses the same OS-object identity bytes as the scalable sidecar identity
    /// machinery (volume + file ID on Windows, device + inode on Unix) and folds
    /// in the schema hash, then pairs it with the matrix layout generation.
    fn matrix_native_identity(&self) -> Result<MatrixNativeIdentity> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        let creation_nonce = self
            .matrix_creation_nonce
            .ok_or(Error::MatrixLayoutMissing)?;
        let file = self.snapshot.try_clone_file()?;
        let fingerprint = native_object_fingerprint(self.spec, &file)?;
        Ok(MatrixNativeIdentity {
            fingerprint,
            layout_generation: matrix.append_log_start(),
            creation_nonce,
        })
    }

    pub fn matrix_recovery_report(&self) -> MatrixRecoveryReport {
        match self.matrix.as_ref() {
            Some(matrix) => crate::matrix::recovery_report(matrix),
            None => MatrixRecoveryReport {
                findings: Vec::new(),
                recommended_actions: Vec::new(),
            },
        }
    }

    /// Verifies this matrix's commit metadata now, and reports what was found.
    ///
    /// The on-demand half of `MatrixMetadataVerification`. A matrix opened under
    /// `MatrixMetadataVerification::AtOpen` — the default — has already had this
    /// pass run, and its findings are in `matrix_recovery_report`; one opened
    /// under `OnDemand` has not, and this is how a caller asks. Running it twice
    /// is allowed and answers the same thing.
    ///
    /// What it does: reads every page of every commit map named by the persisted
    /// page index **unioned with** the platform allocation map — the second term
    /// is what sees stray bytes in a page nothing ever published — authenticates
    /// each against its stored digest, and reports the `Recoverable` commit-map
    /// findings and the `RebuildCommitMap` / `ClearCategory` recommendations that
    /// follow. Cost: `O(live pages + allocated pages)` bytes read, one 4096-byte
    /// buffer retained, no matter the matrix's size.
    ///
    /// What it does **not** do: arm the quarantine
    /// (`Error::MatrixCommitQuarantined`) or gate a writer. That gate is derived
    /// once, at open, from the findings the layout is assembled with; a caller who
    /// needs a damaged category failed closed reopens with `AtOpen`.
    ///
    /// `Err(Error::MatrixLayoutMissing)` when this file has no matrix.
    pub fn verify_matrix_metadata(&self) -> Result<MatrixRecoveryReport> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        // A private duplicate of the descriptor, so the pass needs no borrow of
        // the handle a writer holds and can run under `&self` like every other
        // matrix read.
        let mut file = self.snapshot.try_clone_file()?;
        crate::matrix::verify_matrix_metadata(matrix, &mut file)
    }

    pub fn inspect_writer_lock<P: AsRef<Path>>(path: P) -> Result<Option<WriterLockInfo>> {
        read_writer_lock_info(path.as_ref())
    }

    /// OS-object identity bytes of this writer's native file.
    ///
    /// Diagnostic scaffolding captures this at creation time so cleanup can
    /// refuse to delete a file that another process has since swapped in at
    /// the same pathname (API2-02).
    pub(crate) fn native_object_identity(&self) -> Result<Vec<u8>> {
        self.file.object_identity()
    }

    /// Arms `count` injected post-publication parent-directory sync failures.
    ///
    /// Fault-testing hook only: each armed failure makes the next
    /// `replace.parent_sync` boundary return an error after a publication has
    /// already succeeded, which is otherwise hard to reproduce on demand.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_parent_sync_failures(count: u64) {
        INJECTED_PARENT_SYNC_FAILURES.store(count, std::sync::atomic::Ordering::Release);
    }

    /// Arms `count` injected indeterminate replacement failures (DUR2-01).
    ///
    /// Fault-testing hook only: each armed failure makes the next atomic
    /// pathname publication fail with
    /// [`Error::ReplacePublicationIndeterminate`] before touching the target,
    /// which models an unreconciled Windows `ReplaceFileW` 1176/1177 outcome
    /// that cannot be produced on demand by a real filesystem.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_replace_indeterminate_failures(count: u64) {
        INJECTED_REPLACE_INDETERMINATE_FAILURES.store(count, std::sync::atomic::Ordering::Release);
    }

    /// Arms `count` injected rewrite-temp preparation failures (STO4-P2).
    ///
    /// Fault-testing hook only: each armed failure makes the next internal
    /// merge/compact publication fail immediately after creating its rewrite
    /// temp and before the scope that used to be responsible for deleting it.
    /// The observable contract is that no `.rewrite.` artifact survives.
    /// Thread-local.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_rewrite_temp_preparation_failures(count: u64) {
        INJECTED_REWRITE_TEMP_PREPARATION_FAILURES.with(|armed| armed.set(count));
    }

    /// Arms `count` injected generic keyed-tail reservation failures (API3-01).
    ///
    /// Fault-testing hook only: each armed failure makes the next generic
    /// keyed push/delete fail its pre-append tail-cache reservation with
    /// [`Error::AllocationFailed`]. The observable contract under this fault is
    /// that the append did **not** happen, which is exactly what the previous
    /// append-then-reserve ordering could not guarantee. Thread-local.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_keyed_tail_reservation_failures(count: u64) {
        INJECTED_KEYED_TAIL_RESERVATION_FAILURES.with(|armed| armed.set(count));
    }

    /// Arms `count` injected post-append keyed-tail reservation losses
    /// (API3-01).
    ///
    /// Fault-testing hook only: each armed loss makes the next generic keyed
    /// push/delete behave as if the slot reserved before the append were no
    /// longer usable afterwards. The mutation still succeeds; the cached tail
    /// map for that block id must be discarded rather than left holding the
    /// superseded predecessor, so the following mutation still links to the
    /// record that succeeded. Thread-local.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_keyed_tail_commit_losses(count: u64) {
        INJECTED_KEYED_TAIL_COMMIT_LOSSES.with(|armed| armed.set(count));
    }

    /// Arms `count` injected post-commit-marker durability failures (round 10).
    ///
    /// Fault-testing hook only: each armed failure makes the next
    /// [`Self::commit_durable`] fail its `flush`/`sync_all` *after* the commit
    /// marker has been appended. The observable contract under this fault is
    /// that the caller receives [`Error::CommittedButDurabilityUnproven`]
    /// rather than a bare `Err`, because the marker bytes are in the file and
    /// re-running the transaction would append a second marker for work that
    /// is already recorded. Thread-local.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_commit_durability_failures(count: u64) {
        INJECTED_COMMIT_DURABILITY_FAILURES.with(|armed| armed.set(count));
    }

    /// Returns the cumulative number of resident index entries this thread's
    /// checkpoint flush-cadence machinery has examined (PERF2-02).
    ///
    /// Fault-testing hook only: regression tests delta-measure this around a
    /// flush-per-record workload to pin the cadence work at O(1) per flush
    /// (near-linear cumulative touches) instead of the historical O(N) rescan
    /// per flush. The counter is thread-local so concurrently running tests
    /// cannot pollute each other's measurements.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn checkpoint_cadence_index_touches() -> u64 {
        CHECKPOINT_CADENCE_INDEX_TOUCHES.with(|touches| touches.get())
    }

    /// Returns the cumulative number of resident index entries this thread's
    /// block-offset-chain predecessor machinery has examined (PERF2-05).
    ///
    /// Fault-testing hook only. The append path resolves predecessors from the
    /// maintained tail table and must never advance this counter, so a
    /// regression test can delta-measure an append window and assert zero for
    /// any record count; only wholesale index loads (open, recovery, rebind,
    /// rollback) pay one pass.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn block_tail_index_touches() -> u64 {
        BLOCK_TAIL_INDEX_TOUCHES.with(|touches| touches.get())
    }

    /// Returns the cumulative number of block-tail tuples this thread has
    /// displaced by inserting into the sorted tail vector (PERF3-03).
    ///
    /// Fault-testing hook only. Wholesale tail construction must not advance
    /// this counter at all: it orders the distinct block ids once instead of
    /// inserting each first-seen id, so a reverse-ordered index no longer pays
    /// the `Theta(B^2)` movement the index-visit counter above cannot see.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn block_tail_entries_moved() -> u64 {
        BLOCK_TAIL_ENTRIES_MOVED.with(|moved| moved.get())
    }

    /// Returns the cumulative number of records this thread has framed - read a
    /// header, an extent and a footer for - while building an index.
    ///
    /// Fault-testing hook only. This is the unit `IndexPolicy::segment_on_flush`
    /// exists to reduce: a scan frames every record in the file, and a segment
    /// chain frames one record per commit point and no data record at all, so a
    /// regression test can delta-measure an open and tell the two apart by the
    /// count rather than by the wall clock.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn records_framed() -> u64 {
        RECORDS_FRAMED.with(|framed| framed.get())
    }

    /// Reads and clears the number of index-entry comparisons this thread has
    /// made resolving a rewritten record's chain predecessor.
    ///
    /// Fault-testing hook only. `replace_block` validates every rewritten
    /// record's predecessor against the prefix it has already written; a
    /// front-to-back scan of that growing prefix costs `Theta(N^2)` and the
    /// sorted lookup that replaced it costs `O(N log N)`, which a regression
    /// test tells apart by this count rather than by the clock.
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    #[doc(hidden)]
    pub fn take_replacement_predecessor_probes() -> u64 {
        REPLACEMENT_PREDECESSOR_PROBES.with(|probes| probes.replace(0))
    }

    /// Reads and clears the number of `fstat`s this thread has issued to bind
    /// a snapshot's length (`snapshot.rs`'s `checked_snapshot_bounds`).
    ///
    /// Fault-testing hook only. The append path rebinds through
    /// `SnapshotFile::with_written_len`, which proves `len <= physical_len`
    /// from the write that just returned, so an append window must leave this
    /// at zero for any record count. Open still pays one per snapshot bind,
    /// which is what tells a real fix apart from a neutered counter.
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    #[doc(hidden)]
    pub fn take_snapshot_bounds_fstats() -> u64 {
        crate::snapshot::take_snapshot_bounds_fstats()
    }

    /// Record bytes this thread has walked while scanning, read and cleared.
    ///
    /// Fault-testing hook only. An open that follows the on-disk segment chain
    /// walks the chain; an open that scans walks the file. This is the number
    /// that tells them apart.
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    #[doc(hidden)]
    pub fn take_open_scan_bytes() -> u64 {
        take_open_scan_bytes()
    }

    /// Reads and clears the number of `fstat`s this thread has issued through
    /// the writer's own record handle (`RecordFile::metadata`).
    ///
    /// Fault-testing hook only, and a sibling of the counter above rather than
    /// the same one: this is the `AppendSnapshot` end-of-file probe, which the
    /// writer now answers from the snapshot it maintains.
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    #[doc(hidden)]
    pub fn take_record_file_metadata_calls() -> u64 {
        record_file::take_record_file_metadata_calls()
    }

    /// Cumulative bytes this thread has read to answer chunked cell reads.
    ///
    /// Fault-testing hook only. This is the unit a growing matrix's read path
    /// exists to keep small: a chunk is one record, so "read the cell" and
    /// "read the chunk" differ by orders of magnitude and a regression test can
    /// tell them apart by an absolute byte count rather than a ratio.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn chunk_bytes_read() -> u64 {
        CHUNK_BYTES_READ.with(|read| read.get())
    }

    #[cfg(feature = "mmap")]
    /// Maps the file's indexed record payloads as a read-only snapshot.
    ///
    /// # Safety
    ///
    /// The caller must prevent mutation, truncation, replacement, backing-file
    /// invalidation, and any other modification through every handle, thread,
    /// and process for the full lifetime of the returned mapping. The mapping
    /// remains tied to this already-open backing object even if its path is
    /// renamed or replaced.
    ///
    /// Once that condition holds, Varve keeps safe accessors sound by creating
    /// a read-only mapping from a clone of the existing handle, validating every
    /// indexed extent with checked arithmetic, rejecting entries outside the
    /// copied snapshot, and tying every returned slice to the mapping owner.
    /// Use owned reads instead when external immutability cannot be guaranteed.
    pub unsafe fn mmap_payloads(&self) -> Result<MmapPayloads> {
        let mapped_len = self.snapshot.len();
        // `MmapLen` alone. `FileLen` was checked here too, against the same
        // value — the mapping's length is the snapshot's length — so it bounded
        // nothing the line below does not, and it made a caller who declared a
        // mapping ceiling also have to declare a file ceiling to use it.
        self.spec
            .read_limits
            .check(ReadLimitKey::MmapLen, mapped_len)?;
        let record_count =
            u64::try_from(self.index.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Records, record_count)?;
        let mmap_index_bytes = mmap_index_bytes_for_count(self.index.len())?;
        self.spec
            .read_limits
            .check(ReadLimitKey::IndexBytes, mmap_index_bytes)?;

        let file = self.snapshot.try_clone_file()?;
        let current_len = file.metadata()?.len();
        if mapped_len > current_len {
            return Err(Error::MmapPayloadOutOfBounds {
                offset: 0,
                len: mapped_len,
            });
        }
        // **One validation pass, against the stronger bound.** This used to
        // walk every entry against `current_len`, map the file, then walk every
        // entry again against `mapped_len`. `mapped_len <= current_len` is
        // enforced immediately above, so the second bound implies the first:
        // one pass against `mapped_len` is both the stronger check and the
        // earlier one, and it fails before the mapping is taken rather than
        // after.
        for entry in self.index.iter() {
            validate_mmap_index_entry(entry, mapped_len)?;
        }
        let map_len =
            usize::try_from(mapped_len).map_err(|_| Error::LengthOverflow { value: mapped_len })?;
        // SAFETY: The caller guarantees that the cloned backing object remains
        // immutable and valid for the mapping's entire lifetime.
        let mmap = unsafe { memmap2::MmapOptions::new().len(map_len).map(&file)? };
        let mut index = Vec::new();
        index
            .try_reserve_exact(self.index.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "mmap index",
                requested: mmap_index_bytes,
            })?;
        let mut by_block: HashMap<u32, Vec<usize>> = HashMap::new();
        by_block
            .try_reserve(
                self.index
                    .len()
                    .min(self.spec.blocks.len().saturating_add(6)),
            )
            .map_err(|_| Error::AllocationFailed {
                resource: "mmap block lookup",
                requested: mmap_index_bytes,
            })?;
        // **One build pass.** The copy, the per-block position lists and the
        // ordering witness were three separate walks of the same entries; they
        // are one now. The ordering check in particular was a `windows(2)` over
        // the finished copy, which is a whole extra traversal in debug builds to
        // learn something each step already knows.
        //
        // Records occupy contiguous ascending physical extents, so the copied
        // index is strictly offset-ordered; `payload_window` relies on that for
        // its binary-search membership check (PERF2-07).
        let mut previous_offset: Option<u64> = None;
        for (position, entry) in self.index.iter().enumerate() {
            debug_assert!(
                previous_offset.is_none_or(|previous| previous < entry.record_offset),
                "resident index must be strictly ordered by record offset",
            );
            previous_offset = Some(entry.record_offset);
            index.push(entry.clone());
            let positions = by_block.entry(entry.block_id).or_default();
            positions
                .try_reserve(1)
                .map_err(|_| Error::AllocationFailed {
                    resource: "mmap block positions",
                    requested: mmap_index_bytes,
                })?;
            positions.push(position);
        }
        Ok(MmapPayloads {
            spec: self.spec,
            index,
            by_block,
            mmap,
        })
    }

    #[cfg(feature = "mmap")]
    /// Maps the file's matrix regions as a read-only snapshot.
    ///
    /// # Safety
    ///
    /// The caller must prevent mutation, truncation, replacement, backing-file
    /// invalidation, and any other modification through every handle, thread,
    /// and process for the full lifetime of the returned mapping. The mapping
    /// remains tied to this already-open backing object even if its path is
    /// renamed or replaced.
    ///
    /// Once that condition holds, Varve keeps safe accessors sound by creating
    /// a read-only mapping from a clone of the existing handle, validating the
    /// complete matrix extent, checking every requested slot, and tying every
    /// returned slice to the mapping owner. Use owned reads instead when
    /// external immutability cannot be guaranteed.
    pub unsafe fn mmap_matrix(&self) -> Result<MmapMatrix> {
        let layout = self
            .matrix
            .as_ref()
            .ok_or(Error::MatrixLayoutMissing)?
            .clone();
        let mapped_len = self.snapshot.len();
        // `MmapLen` alone. `FileLen` was checked here too, against the same
        // value — the mapping's length is the snapshot's length — so it bounded
        // nothing the line below does not, and it made a caller who declared a
        // mapping ceiling also have to declare a file ceiling to use it.
        self.spec
            .read_limits
            .check(ReadLimitKey::MmapLen, mapped_len)?;
        let record_count =
            u64::try_from(self.index.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Records, record_count)?;
        self.spec.read_limits.check(
            ReadLimitKey::IndexBytes,
            index_bytes_for_count(self.index.len())?,
        )?;
        let file = self.snapshot.try_clone_file()?;
        let current_len = file.metadata()?.len();
        if mapped_len > current_len {
            return Err(Error::MmapPayloadOutOfBounds {
                offset: 0,
                len: mapped_len,
            });
        }
        validate_mmap_range(0, layout.append_log_start(), current_len)?;
        let map_len =
            usize::try_from(mapped_len).map_err(|_| Error::LengthOverflow { value: mapped_len })?;
        // SAFETY: The caller guarantees that the cloned backing object remains
        // immutable and valid for the mapping's entire lifetime.
        let mmap = unsafe { memmap2::MmapOptions::new().len(map_len).map(&file)? };
        validate_mmap_range(0, layout.append_log_start(), mapped_len)?;
        Ok(MmapMatrix {
            spec: self.spec,
            layout,
            mmap,
        })
    }

    /// The write-side guard: poison check plus open-mode check, returning the
    /// witness the guarded operations demand. See [`Self::ensure_not_poisoned`].
    fn ensure_write(&self) -> Result<FileMutationPermit> {
        let permit = self.ensure_not_poisoned()?;
        match self.mode {
            OpenMode::ReadWrite => Ok(permit),
            OpenMode::ReadOnly => Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "file was opened read-only",
            ))),
        }
    }

    /// Overwrites an already-indexed record's header and payload in place.
    ///
    /// F-01 and F-02, mechanical enforcement. This function holds no addressing
    /// and no invalidation of its own: it takes a [`ReplacementTarget`], whose
    /// only constructor performed the `T::VERSION` refusal, and turns it into a
    /// [`RecordOverwrite`], whose only constructor drops the target block's
    /// resident keyed-tail map. The write is then performed by
    /// `RecordFile::overwrite_indexed_record`, which consumes the
    /// [`RecordOverwrite`] by value and is the only route to those bytes,
    /// because the raw handle is unnameable outside `mod record_file`.
    ///
    /// So the two properties below are not properties of this function that a
    /// sibling could fail to repeat; they are properties of *addressing an
    /// already-indexed record at all*:
    ///
    /// * The invalidation runs **before** the first byte reaches disk, so it
    ///   also covers a write that fails half-way, a poisoned writer that is
    ///   later inspected, and a caller that violates the same-key contract.
    /// * It is infallible and allocation-free (a `HashMap::remove`), so it
    ///   introduces no fallible step of its own, in either direction of
    ///   invariant 3.
    ///
    /// Round 12 stated the same guarantee for a function that a new seek/write
    /// pair elsewhere in this file could simply bypass, and re-verification
    /// compiled exactly that bypass. The bypass no longer builds.
    fn overwrite_record_bytes_in_place(
        &mut self,
        permit: &FileMutationPermit,
        target: ReplacementTarget,
        header_bytes: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        let _ = permit;
        // Both preconditions are discharged by constructing the permission:
        // the version refusal happened in `ReplacementTarget::resolve`, and
        // `prepare` drops the keyed-tail map before the write below.
        let write = RecordOverwrite::prepare(target, &self.index, &mut self.keyed_tails)?;
        self.file
            .overwrite_indexed_record(write, header_bytes, payload)
    }

    fn rebind_published_generation(
        &mut self,
        sequence: u64,
        new_index: Vec<RecordIndexEntry>,
    ) -> Result<u64> {
        let rebind = (|| -> Result<(File, SnapshotFile)> {
            #[cfg(test)]
            fail_rebind_after_publish_if_requested()?;
            let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
            // The published generation is a new file object; the single-writer
            // object lock must move with the writer.
            if let Some(lock) = self._lock.as_mut() {
                lock.bind_native(&file, &self.path)?;
            }
            let snapshot = SnapshotFile::new(file.try_clone()?)?;
            Ok((file, snapshot))
        })();

        match rebind {
            Ok((file, snapshot)) => {
                self.file = RecordFile::new(file);
                self.snapshot = snapshot;
                self.index = ResidentIndex::adopt_generation(new_index);
                // The resident index was replaced wholesale; recover the O(1)
                // flush-cadence state once for the new generation (PERF2-02)
                // and the O(1) block tails with it (PERF2-05). Record offsets
                // moved, so every cached generic keyed tail is stale (API2-05).
                let derived = derive_index_state(self.index.iter(), true);
                self.checkpoint_cadence = derived.checkpoint_cadence;
                self.block_tails = derived.block_tails.expect("tails were requested");
                self.segment_cursor = derived.segment_cursor;
                self.keyed_tails.invalidate_all();
                self.publish_sequence(sequence);
                Ok(sequence)
            }
            Err(source) => {
                self.poison.poison();
                // F-07: the caller folds in a parent-directory sync failure it
                // already observed, via `Error::with_pending_parent_sync`.
                Err(Error::PublishedButRebindFailed {
                    sequence,
                    source: Box::new(source),
                    parent_sync: None,
                })
            }
        }
    }

    fn rebind_replacement_generation(
        &mut self,
        info: ReplacementInfo,
        new_index: Vec<RecordIndexEntry>,
    ) -> Result<ReplacementInfo> {
        let rebind = (|| -> Result<(File, SnapshotFile)> {
            #[cfg(test)]
            fail_rebind_after_publish_if_requested()?;
            let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
            // The published generation is a new file object; the single-writer
            // object lock must move with the writer.
            if let Some(lock) = self._lock.as_mut() {
                lock.bind_native(&file, &self.path)?;
            }
            let snapshot = SnapshotFile::new(file.try_clone()?)?;
            Ok((file, snapshot))
        })();

        match rebind {
            Ok((file, snapshot)) => {
                self.file = RecordFile::new(file);
                self.snapshot = snapshot;
                self.index = ResidentIndex::adopt_generation(new_index);
                // The resident index was replaced wholesale; recover the O(1)
                // flush-cadence state once for the new generation (PERF2-02)
                // and the O(1) block tails with it (PERF2-05). Record offsets
                // moved, so every cached generic keyed tail is stale (API2-05).
                let derived = derive_index_state(self.index.iter(), true);
                self.checkpoint_cadence = derived.checkpoint_cadence;
                self.block_tails = derived.block_tails.expect("tails were requested");
                self.segment_cursor = derived.segment_cursor;
                self.keyed_tails.invalidate_all();
                Ok(info)
            }
            Err(source) => {
                self.poison.poison();
                // F-07: the caller folds in a parent-directory sync failure it
                // already observed, via `Error::with_pending_parent_sync`.
                Err(Error::PublishedButRebindFailed {
                    sequence: info.sequence,
                    source: Box::new(source),
                    parent_sync: None,
                })
            }
        }
    }

    fn validate_source_generation(&self) -> Result<()> {
        let current_len = self.file.metadata()?.len();
        if current_len != self.snapshot.len() {
            return Err(Error::InvalidCanonicalEncoding(
                "native file changed after it was indexed",
            ));
        }
        let mut file = self.snapshot.try_clone_file()?;
        validate_generation_file(
            self.spec,
            &mut file,
            append_log_start_for_file(self)?,
            self.index.as_contiguous_slice(),
        )
    }

    /// The one poison check for this writer, and the only source of the
    /// witness its guarded operations demand.
    ///
    /// Shape B, mechanical enforcement (round 12). `PoisonFlag` owns the state
    /// and lives in `crate::writer_permit`, so nothing in this ~12000-line file
    /// can read or assign it. Every function that puts bytes into an already
    /// published generation — the append core, and the one in-place record
    /// rewrite — takes the [`MutationPermit`] this returns, by reference, so a
    /// mutating method written later cannot reach the disk without having asked
    /// for one. See `crate::writer_permit` for what that makes impossible.
    fn ensure_not_poisoned(&self) -> Result<FileMutationPermit> {
        self.writer_permit(WRITER_POISON_CONTEXT)
    }

    /// Handles a failed atomic publication for a copy-on-write writer.
    ///
    /// Pre-publication failures leave the target untouched, so the temp file
    /// is deleted and the writer stays usable. An indeterminate outcome
    /// ([`Error::ReplacePublicationIndeterminate`]) means the pathname state
    /// is unknown: the temp file is preserved for out-of-band reconciliation
    /// and the writer is poisoned so a blind retry cannot publish over an
    /// unknown generation (DUR2-01).
    fn fail_publication(&mut self, temp_path: &Path, error: Error) -> Error {
        if matches!(error, Error::ReplacePublicationIndeterminate { .. }) {
            self.poison.poison();
        } else {
            let _ = remove_file(temp_path);
        }
        error
    }

    /// Classifies the outcome of a matrix mutation and poisons the writer when
    /// the failure could have left disk and memory disagreeing.
    ///
    /// INVARIANT 3. The narrow [`Error::Io`] predicate is **deliberate**, and
    /// round 10 re-derived it rather than inheriting it. The review's F-03
    /// offered two corrections — prepare the allocation before the disk write,
    /// or poison on every error raised after on-disk mutation begins — and the
    /// first was taken. That choice is what makes this predicate correct: every
    /// disk write in `crate::matrix` is now paired with an *infallible*
    /// in-memory install (`commit_byte_write`, `commit_current_write_bit`, the
    /// page-index mirror resynchronisation), so a matrix mutation is a sequence
    /// of individually atomic sub-steps. A typed refusal raised between two of
    /// them — `LimitExceeded` from a later page charge, `AllocationFailed` from
    /// a later first-touch page, `InvalidMatrixLayout` from later geometry —
    /// leaves the file and memory agreeing on everything the earlier sub-steps
    /// did, so the writer stays usable and the caller can retry. That contract
    /// is asserted by
    /// `matrix_integrity_scaling.rs::a_failed_page_allocation_cannot_leave_a_bit_on_disk`,
    /// which injects exactly such a failure at both first-touch boundaries and
    /// requires the same writer to keep working.
    ///
    /// Poisoning on *any* post-write error was implemented and rejected on that
    /// evidence: it converts those clean refusals into an unusable writer.
    /// `Error::Io` remains the right trigger because a failed write is the one
    /// case where what reached the file is unknown.
    ///
    /// The obligation this leaves on future work is therefore not "widen the
    /// predicate" but "keep the pairing": any new fallible step placed *between*
    /// a matrix disk write and its in-memory twin reopens F-03, and must be
    /// moved ahead of the write instead.
    fn finish_matrix_mutation<T>(&mut self, result: Result<T>) -> Result<T> {
        if matches!(&result, Err(Error::Io(_))) {
            self.poison.poison();
        }
        result
    }

    /// Poisons on *any* error from a durability barrier phase of a durable
    /// matrix write.
    ///
    /// Stricter than [`Self::finish_matrix_mutation`] on purpose: these phases
    /// bracket the authoritative commit, so an error means a durability request
    /// was refused mid-publication and this handle is unfit to continue. Since
    /// round 11 the only caller is the **pre**-commit data sync, whose failure
    /// is a plain refusal (nothing is published yet). The post-commit sync
    /// poisons at its own call site so it can return the typed published
    /// outcome [`Error::MatrixCommittedButDurabilityUnproven`] instead of the
    /// bare error this helper propagates; do not route it back through here.
    fn poison_after_started_matrix_error<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.poison.poison();
        }
        result
    }

    fn ensure_user_block<T: VarveBlock>(&self) -> Result<()> {
        if T::ID >= RESERVED_BLOCK_ID_START {
            return Err(Error::ReservedBlockId(T::ID));
        }
        Ok(())
    }

    fn publish_sequence(&mut self, sequence: u64) {
        debug_assert_eq!(self.sequence_state, SequenceState::Available(sequence));
        self.sequence_state = SequenceState::after_publishing(sequence);
    }

    #[cfg(test)]
    fn push_info_for_test(&mut self, block_id: u32, payload: &[u8]) -> Result<AppendInfo> {
        let permit = self.ensure_write()?;
        self.write_record_with_prev_key(&permit, block_id, 1, 0, 0, payload, None)
    }

    pub(crate) fn write_record(
        &mut self,
        block_id: u32,
        block_version: u16,
        flags: u16,
        payload: &[u8],
    ) -> Result<u64> {
        let permit = self.ensure_write()?;
        Ok(self
            .write_record_with_prev_key(&permit, block_id, block_version, flags, 0, payload, None)?
            .sequence)
    }

    fn write_user_record(
        &mut self,
        permit: &FileMutationPermit,
        block_id: u32,
        block_version: u16,
        kind: BlockKind,
        payload: &[u8],
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        let payload = prepare_user_record_payload(self.spec, block_id, kind, payload)?;
        self.write_record_with_prev_key(
            permit,
            block_id,
            block_version,
            payload.flags,
            payload.uncompressed_len_hint,
            &payload.bytes,
            prev_same_key_offset,
        )
    }

    /// The crate's single record-append core.
    ///
    /// Shape B, mechanical enforcement (round 12). It used to call
    /// [`Self::ensure_write`] itself, which is a convention: a sibling append
    /// helper written next month could seek and `write_all` without one, which
    /// is precisely the defect F-05 was in `stream.rs`. It now demands the
    /// [`MutationPermit`] instead, so the check is the caller's precondition
    /// and the compiler enforces that every route to an append has made it.
    // The permit is a zero-sized witness, not data: the argument count is one
    // higher than clippy's default because the poison check became a
    // compile-time precondition instead of a remembered first line.
    #[allow(clippy::too_many_arguments)]
    fn write_record_with_prev_key(
        &mut self,
        permit: &FileMutationPermit,
        block_id: u32,
        block_version: u16,
        flags: u16,
        uncompressed_len_hint: u32,
        payload: &[u8],
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        let _ = permit;
        let payload_len =
            u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
        let logical_len = record_logical_len_from_parts(
            self.spec,
            block_id,
            flags,
            uncompressed_len_hint,
            payload,
        )?;
        self.spec
            .read_limits
            .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;

        let record_count =
            self.index
                .len()
                .checked_add(1)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "record count",
                })?;
        self.spec.read_limits.check(
            ReadLimitKey::Records,
            u64::try_from(record_count).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?,
        )?;
        // Shape A, mechanical enforcement (round 12). The limit charge and the
        // mirror reservation are a precondition of the write below, and the
        // token they produce is the only thing that can install the entry
        // afterwards. See `mod reserved_index_slot`.
        let spec = self.spec;
        // H1, and the only per-block branch the index append has ever had.
        // Internal records are always resident: the manifest, commit markers,
        // checkpoints and segments are varve's own bookkeeping and every open
        // path expects to find them.
        let resident = block_id >= RESERVED_BLOCK_ID_START || spec.block_is_resident(block_id);
        let index_slot = self.index.reserve(|| {
            let index_bytes = index_bytes_for_count(record_count)?;
            spec.read_limits
                .check(ReadLimitKey::IndexBytes, index_bytes)?;
            Ok(index_bytes)
        })?;

        let sequence = self.sequence_state.available()?;
        let snapshot = AppendSnapshot {
            // PERF: the end of file is state this writer already maintains, so
            // asking the kernel for it once per record bought nothing. Every
            // construction of `self.snapshot` seeds it from the PHYSICAL length
            // (`SnapshotFile::new` at create, at `open_locked`, at
            // `open_recover_locked` and at both rebind sites) and every
            // successful append rewrites it from the witness the write itself
            // produced; `rollback_append` truncates back to the same `eof` it
            // started from and leaves `self.snapshot` alone.
            //
            // This is not blind trust. `RecordFile::append_record_at_end` still
            // performs its own `seek(SeekFrom::End(0))` and refuses any offset
            // that is not the one budgeted here, so a value that ever drifted
            // yields the existing typed refusal and a rollback — never a write
            // over live data. Removing that check along with the syscall is the
            // positional-write redesign, and is deliberately not what this is.
            eof: self.snapshot.len(),
            cursor: self.file.stream_position()?,
            sequence_state: self.sequence_state,
            index_len: self.index.len(),
            checkpoint_cadence: self.checkpoint_cadence,
            segment_cursor: self.segment_cursor,
            block_id,
            previous_block_tail: self.block_tails.tail(block_id),
            uncommitted_since_commit: self.uncommitted_since_commit,
        };
        let record_offset = snapshot.eof;
        let payload_offset = record_offset.checked_add(RECORD_HEADER_LEN).ok_or(
            Error::ResourceArithmeticOverflow {
                resource: "file length",
            },
        )?;
        let payload_end =
            payload_offset
                .checked_add(payload_len)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "file length",
                })?;
        let prospective_len = payload_end
            .checked_add(record_footer_len(self.spec))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "file length",
            })?;
        // No file-length ceiling here. `max_file_len` refuses on a number
        // rather than on a resource: the scan is bounded incrementally by
        // `ScanBytes`, the index by `Records` and `IndexBytes`, and a mapping
        // by `MmapLen`, each against the thing it actually consumes. A length
        // check on top of those refuses to touch a file whose data is already
        // there, and on the append path it bounds nothing at all — appending to
        // a 1 TB file costs exactly what appending to a 1 GB file costs.
        // PERF2-05: the predecessor comes from the maintained tail table, not
        // from a reverse scan of the resident index.
        let prev_same_block_offset = if self.spec.index_policy.block_offset_chain {
            self.block_tails.tail(block_id)
        } else {
            None
        };
        let prev_same_key_offset = if self.spec.index_policy.keyed_offset_chain {
            prev_same_key_offset
        } else {
            None
        };
        let footer = if self.spec.spec_needs_record_footer() {
            Some(encode_record_footer(RecordFooterFields {
                prev_same_block_offset,
                prev_same_key_offset,
            })?)
        } else {
            None
        };
        let header = RecordHeaderFields {
            block_id,
            block_version,
            flags,
            sequence,
            payload_len,
            checksum: 0,
            uncompressed_len_hint,
        };
        let footer_bytes = footer.as_ref().map_or(&[][..], |bytes| &bytes[..]);
        let checksum =
            checksum_record_fields(self.spec, record_offset, header, payload, footer_bytes)?;
        let header_bytes = encode_native_record_header(
            RecordHeaderFields { checksum, ..header },
            record_offset,
            record_footer_len(self.spec),
        )?;
        let footer_offset = if footer.is_some() {
            payload_end
                .checked_add(RECORD_FOOTER_LEN)
                .ok_or(Error::LengthOverflow { value: payload_len })?;
            Some(payload_end)
        } else {
            None
        };
        // The append is the second and last route to record bytes on disk
        // (`mod record_file`). It seeks to the end itself and refuses an
        // offset that is not the one budgeted above, so this path cannot land
        // inside an already-indexed record even if `record_offset` were wrong.
        let write_result = self.file.append_record_at_end(
            &index_slot,
            record_offset,
            &header_bytes,
            payload,
            footer.as_ref().map(|bytes| &bytes[..]),
            || {
                #[cfg(test)]
                fail_append_after_header_if_requested()?;
                Ok(())
            },
        );
        let written = match write_result {
            Ok(written) => written,
            Err(error) => return Err(self.rollback_append(snapshot, error)),
        };

        // The budgeted extent and the extent actually written are computed
        // independently — one from the header/payload/footer lengths above, the
        // other from the offset `SEEK_END` returned plus the bytes `write_all`
        // took — so they agreeing is a real check on both.
        debug_assert_eq!(written.end_offset(), prospective_len);
        // The write above is the proof that the file reaches this offset, so
        // the rebind needs no `fstat` of its own — that was the second
        // per-record metadata syscall on this path. `with_written_len` is still
        // fallible and still growth-only, so the rollback arm is unchanged.
        let new_snapshot = match self.snapshot.with_written_len(written) {
            Ok(snapshot) => snapshot,
            Err(error) => return Err(self.rollback_append(snapshot, error)),
        };

        let committed =
            !self.spec.commit_policy.is_transaction_marker() || block_id == COMMIT_BLOCK_ID;
        let entry = RecordIndexEntry {
            block_id,
            block_version,
            flags,
            sequence,
            record_offset,
            payload_offset,
            payload_len,
            checksum,
            uncompressed_len_hint,
            footer_offset,
            prev_same_block_offset,
            prev_same_key_offset,
            committed,
        };
        let info = AppendInfo::from(&entry);
        // A non-resident block's record is written, sequenced, chained and
        // recoverable exactly as any other; it is simply not mirrored in
        // memory, which is what stops resident cost tracking record count.
        //
        // Three things are deliberately *not* skipped with it. Sequences are
        // file-global and gapless, so `publish_sequence` below runs either way.
        // `BlockTails` is what makes the footer chain walkable and is
        // `O(blocks)`, not `O(records)` - it is the only way back to a
        // non-resident record, so it is maintained for every block. And the
        // record's own footer chain went to disk above, not to memory.
        if resident {
            self.index.install(index_slot, entry);
            // Single index append site: keep the O(1) flush-cadence state in
            // lockstep with the resident index (PERF2-02). It counts resident
            // entries, so a non-resident record must not advance it.
            self.checkpoint_cadence
                .note_appended(self.index.len() - 1, block_id);
            self.segment_cursor
                .note_appended(self.index.len() - 1, block_id, prospective_len);
        } else {
            drop(index_slot);
        }
        self.block_tails.note_appended(block_id, record_offset);
        // The marker itself clears the flag; a segment record is written after
        // the marker it certifies and must not set it again, or a repeated idle
        // flush would write marker, segment, marker, segment forever.
        self.uncommitted_since_commit = match block_id {
            COMMIT_BLOCK_ID => false,
            SEGMENT_BLOCK_ID => self.uncommitted_since_commit,
            _ => true,
        };
        self.snapshot = new_snapshot;
        self.publish_sequence(sequence);
        Ok(info)
    }

    fn rollback_append(&mut self, snapshot: AppendSnapshot, operation_error: Error) -> Error {
        // The tail states are advanced only after the record is fully written,
        // so this normally truncates nothing; rebuilding when it does keeps
        // the `BlockTails::from_index` invariant unconditional (PERF2-05).
        let truncated = self.index.len() > snapshot.index_len;
        self.index.truncate(snapshot.index_len);
        // Restore the one tail this append moved rather than rebuilding the
        // table from the resident index.
        //
        // **Unreachable today, and kept anyway.** Both rollback points are
        // above `block_tails.note_appended`, so a rollback always finds the
        // tail where it started and this restores it to itself — exactly as
        // the rebuild it replaces never actually ran, for the same reason.
        // What changed is which one is *correct if the ordering ever moves*:
        // rebuilding from the resident index drops a non-resident block's tail
        // entirely, and that tail is the only way back to its records. This is
        // also `O(1)` and allocation-free, which the rebuild was not
        // (PERF2-05).
        self.block_tails
            .restore(snapshot.block_id, snapshot.previous_block_tail);
        self.uncommitted_since_commit = snapshot.uncommitted_since_commit;
        if truncated {
            self.keyed_tails.invalidate_all();
        }
        self.sequence_state = snapshot.sequence_state;
        self.checkpoint_cadence = snapshot.checkpoint_cadence;
        self.segment_cursor = snapshot.segment_cursor;

        #[cfg(test)]
        let truncate_result =
            fail_rollback_if_requested().and_then(|()| self.file.set_len(snapshot.eof));
        #[cfg(not(test))]
        let truncate_result = self.file.set_len(snapshot.eof);
        let cursor_result = self.file.seek_to(snapshot.cursor).map(|_| ());
        if let Some(source) = truncate_result.err().or_else(|| cursor_result.err()) {
            self.poison.poison();
            Error::WriteRollbackFailed {
                operation: "append record",
                source,
            }
        } else {
            operation_error
        }
    }

    /// The payload length a full checkpoint of the current index would need.
    fn index_checkpoint_payload_len(&self) -> Result<u64> {
        const CHECKPOINT_PREFIX_LEN: u64 = 4 + 2 + 8 + 8;
        let entry_count =
            u64::try_from(self.index.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "checkpoint entry count",
            })?;
        entry_count
            .checked_mul(SEGMENT_ENTRY_LEN)
            .and_then(|bytes| bytes.checked_add(CHECKPOINT_PREFIX_LEN))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "checkpoint payload length",
            })
    }

    /// Whether a full checkpoint of the current index would fit the format's
    /// own record ceilings.
    ///
    /// The index only grows, so once this is false it stays false and the
    /// answer costs one multiply per flush. Every way it can be false is a
    /// reason `write_index_checkpoint` would refuse, which is why the decision
    /// belongs here: a checkpoint is derived, and declining to write one costs
    /// the next open a scan it already knows how to do.
    fn index_checkpoint_fits(&self) -> bool {
        let Ok(payload_len) = self.index_checkpoint_payload_len() else {
            return false;
        };
        let limits = &self.spec.read_limits;
        u64::try_from(self.index.len())
            .is_ok_and(|count| limits.check(ReadLimitKey::Records, count).is_ok())
            && limits
                .check(ReadLimitKey::RecordPayloadLen, payload_len)
                .is_ok()
            && limits
                .check(ReadLimitKey::LogicalPayloadLen, payload_len)
                .is_ok()
    }

    fn write_index_checkpoint(&mut self) -> Result<u64> {
        const CHECKPOINT_PREFIX_LEN: u64 = 4 + 2 + 8 + 8;
        const CHECKPOINT_ENTRY_LEN: u64 = 4 + 2 + 2 + 8 + 8 + 8 + 8 + 4 + 4 + 8 + 8 + 8 + 1;
        let covered_offset = self.file.metadata()?.len();
        let entry_count =
            u64::try_from(self.index.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "checkpoint entry count",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Records, entry_count)?;
        let payload_len = entry_count
            .checked_mul(CHECKPOINT_ENTRY_LEN)
            .and_then(|bytes| bytes.checked_add(CHECKPOINT_PREFIX_LEN))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "checkpoint payload length",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::LogicalPayloadLen, payload_len)?;
        let mut payload = Vec::new();
        let payload_capacity = usize::try_from(payload_len)
            .map_err(|_| Error::LengthOverflow { value: payload_len })?;
        payload
            .try_reserve_exact(payload_capacity)
            .map_err(|_| Error::AllocationFailed {
                resource: "checkpoint payload",
                requested: payload_len,
            })?;
        payload.extend_from_slice(INDEX_CHECKPOINT_MAGIC);
        payload.extend_from_slice(&INDEX_CHECKPOINT_VERSION.to_le_bytes());
        payload.extend_from_slice(&covered_offset.to_le_bytes());
        payload.extend_from_slice(&entry_count.to_le_bytes());
        for entry in self.index.iter() {
            push_index_entry_bytes(&mut payload, entry, entry.committed);
        }
        self.write_record(INDEX_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, &payload)
    }

    /// Appends the segment record that closes this commit point.
    ///
    /// It must be the last record the commit point writes. Open finds the
    /// chain by reading the last record footer in the file, so any record
    /// appended after this one hides it — which costs the next open a full
    /// scan, never correctness.
    fn write_segment_record(&mut self) -> Result<()> {
        let start = self.segment_cursor.next_position;
        debug_assert!(start <= self.index.len());
        let covered_start = match self.segment_cursor.covered_start {
            Some(offset) => offset,
            None => append_log_start_for_file(self)?,
        };
        let preceding_records =
            u64::try_from(start).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "segment entry count",
            })?;
        // The record lands at the current end of file, and the trailer has to
        // name that offset before the header that would state it exists. The
        // append core budgets the same value from the same source and refuses
        // to write anywhere else, so the two cannot disagree.
        let record_offset = self.file.metadata()?.len();
        let payload = encode_segment_payload(
            self.spec,
            self.index.iter().skip(start),
            covered_start,
            preceding_records,
            record_offset,
        )?;
        let permit = self.ensure_write()?;
        let info = self.write_record_with_prev_key(
            &permit,
            SEGMENT_BLOCK_ID,
            SEGMENT_VERSION,
            RECORD_FLAG_INTERNAL,
            0,
            &payload,
            None,
        )?;
        debug_assert_eq!(info.record_offset, record_offset);
        Ok(())
    }

    /// Whether this commit point should close a segment.
    ///
    /// Three conditions, and the third is the one that matters: a segment is
    /// only written once everything it would cover is committed. Under a
    /// transaction-marker policy that means the commit marker is already down.
    /// A segment that covered uncommitted records would outlive them — the next
    /// writer open truncates them — and would then describe a file that no
    /// longer exists.
    fn needs_index_segment(&self) -> bool {
        self.spec.index_policy.segment_on_flush
            && self.segment_cursor.next_position < self.index.len()
            && !(self.spec.commit_policy.is_transaction_marker()
                && self.has_uncommitted_since_last_commit())
    }

    /// Writes the segment record if one is due, and reports nothing if it
    /// could not be written.
    ///
    /// INVARIANT 3. Every caller runs this *after* the record its operation is
    /// accountable for is in the file, and for a commit point that record is
    /// the commit marker. The segment is derived from records that are already
    /// durable, so its failure cannot make the operation not have happened; the
    /// next open answers a missing or torn segment with the full scan. Turning
    /// it into an error here would tell a caller to re-run a transaction that
    /// is already committed.
    ///
    /// A failed append rolls itself back, and a failed *rollback* poisons the
    /// writer, so a failure this swallows is still visible: the next operation
    /// on this handle refuses.
    fn write_index_segment_if_needed(&mut self) {
        if self.mode != OpenMode::ReadWrite || !self.needs_index_segment() {
            return;
        }
        let _ = self.write_segment_record();
    }

    fn write_embedded_manifest_if_needed(&mut self) -> Result<()> {
        if self.spec.manifest_policy != ManifestPolicy::Embedded
            || self
                .index
                .iter()
                .any(|entry| entry.block_id == MANIFEST_BLOCK_ID)
        {
            return Ok(());
        }
        let payload = encode_schema_manifest(self.spec)?;
        self.write_record(
            MANIFEST_BLOCK_ID,
            MANIFEST_PAYLOAD_VERSION,
            RECORD_FLAG_INTERNAL,
            &payload,
        )?;
        Ok(())
    }

    fn write_commit_marker(&mut self) -> Result<AppendInfo> {
        let permit = self.ensure_write()?;
        self.write_record_with_prev_key(
            &permit,
            COMMIT_BLOCK_ID,
            1,
            RECORD_FLAG_INTERNAL,
            0,
            COMMIT_PAYLOAD_MAGIC,
            None,
        )
    }

    fn has_uncommitted_since_last_commit(&self) -> bool {
        self.uncommitted_since_commit
    }

    /// Decides whether `flush`/`commit` should serialize a fresh full index
    /// checkpoint.
    ///
    /// This throttles only the *checkpoint* serialization; native-record
    /// durability in `flush` is untouched. Two rules apply (PERF-02):
    ///
    /// (a) *Skip when unchanged.* If nothing but commit markers has been
    ///     appended since the last checkpoint, the checkpoint would be
    ///     byte-identical, so it is suppressed.
    ///
    /// (b) *Geometric spacing.* A new full checkpoint is written only once the
    ///     live tail has grown by at least
    ///     `max(INDEX_CHECKPOINT_MIN_RECORDS, records_at_last_checkpoint / 2)`
    ///     records. Because each checkpoint serializes the whole index, spacing
    ///     the checkpoints geometrically bounds the total checkpoint bytes to
    ///     O(N) with O(log N) checkpoints, instead of the O(N^2) bytes produced
    ///     by a full checkpoint on every flush.
    ///
    /// Open/recovery does not depend on this cadence: the scan reads every
    /// native record and merely validates whatever checkpoints it encounters,
    /// so sparse checkpoints and a checkpoint-less tail both recover correctly.
    ///
    /// The decision itself is O(1) (PERF2-02): [`CheckpointCadence`] carries
    /// the eligible-tail count and the geometric threshold that the original
    /// implementation recomputed by reverse-searching the resident index on
    /// every flush, which made cumulative flush CPU O(N^2) for
    /// flush-per-record workloads.
    /// (c) *Skip when it would not fit.* A full checkpoint serializes the whole
    ///     index into one record, so past `(max_record_payload_len - 22) / 73`
    ///     entries — 919,299 on the 64 MiB default — there is no record that
    ///     can hold it. `write_index_checkpoint` answered that with
    ///     `LimitExceeded`, and because `flush` propagates, a file simply
    ///     stopped being flushable at that record count. The checkpoint is
    ///     derived: not writing one costs the next open the scan it already
    ///     falls back to, so the ceiling belongs in the decision, not in the
    ///     write.
    fn needs_index_checkpoint(&self) -> bool {
        // Rule (a): commit markers never grow the eligible tail, so an
        // unchanged checkpoint is suppressed by `eligible_since_checkpoint`
        // staying at zero. Rule (b): the threshold was derived from the last
        // checkpoint's position, whose entry count is a faithful proxy for
        // that checkpoint's byte cost.
        let new_records = self.checkpoint_cadence.eligible_since_checkpoint;
        new_records != 0
            && new_records >= self.checkpoint_cadence.next_threshold
            && self.index_checkpoint_fits()
    }
}

/// Newest-first walk of one block's footer chain.
///
/// Each step is two positional reads — the record header, then its footer — and
/// yields the entry it just framed. Nothing is retained: the walk holds one
/// offset, so a block with more records than memory is still walkable.
#[derive(Debug)]
pub struct BlockChain<'a> {
    spec: FormatSpec,
    snapshot: &'a SnapshotFile,
    block_id: u32,
    next: Option<u64>,
    visited: u64,
}

impl Iterator for BlockChain<'_> {
    type Item = Result<RecordIndexEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next?;
        match self.step(offset) {
            Ok(entry) => Some(Ok(entry)),
            Err(error) => {
                // A failed step ends the walk rather than repeating itself.
                self.next = None;
                Some(Err(error))
            }
        }
    }
}

impl BlockChain<'_> {
    fn step(&mut self, offset: u64) -> Result<RecordIndexEntry> {
        self.visited = self
            .visited
            .checked_add(1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Records, self.visited)?;
        let entry = read_record_entry_positional(self.spec, self.snapshot, offset)?;
        if entry.block_id != self.block_id {
            return Err(Error::InvalidCanonicalEncoding(
                "block offset chain left its block",
            ));
        }
        // Strictly decreasing, so a crafted or damaged chain cannot loop: the
        // walk is bounded by the file whatever the footers claim. Defence in
        // depth rather than the first line - `decode_native_record_footer`
        // already refuses a predecessor at or past its own record, so a whole
        // file carrying one is refused at open and never reaches here. This
        // walk frames records that scan never looked at, which is why it
        // carries the check itself.
        self.next = match entry.prev_same_block_offset {
            Some(previous) if previous >= offset => {
                return Err(Error::InvalidCanonicalEncoding(
                    "block offset chain does not decrease",
                ));
            }
            previous => previous,
        };
        Ok(entry)
    }
}

/// Frames one record through the snapshot, without touching the resident index.
fn read_record_entry_positional(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    record_offset: u64,
) -> Result<RecordIndexEntry> {
    let mut header = [0u8; RECORD_HEADER_LEN as usize];
    snapshot.read_exact_at(record_offset, &mut header)?;
    let decoded = read_native_record_header(&mut &header[..], record_offset)?;
    let fields = decoded.fields;
    let payload_offset =
        record_offset
            .checked_add(decoded.lead_in_len)
            .ok_or(Error::CorruptTail {
                offset: record_offset,
            })?;
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, fields.payload_len)?;
    let payload_end = payload_offset
        .checked_add(fields.payload_len)
        .ok_or(Error::CorruptTail {
            offset: record_offset,
        })?;
    let mut entry = RecordIndexEntry {
        block_id: fields.block_id,
        block_version: fields.block_version,
        flags: fields.flags,
        sequence: fields.sequence,
        record_offset,
        payload_offset,
        payload_len: fields.payload_len,
        checksum: fields.checksum,
        uncompressed_len_hint: fields.uncompressed_len_hint,
        footer_offset: None,
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        committed: true,
    };
    validate_record_entry(spec, &entry)?;
    if spec.spec_needs_record_footer() {
        let mut footer = [0u8; RECORD_FOOTER_LEN as usize];
        snapshot.read_exact_at(payload_end, &mut footer)?;
        let decoded = decode_record_footer(&footer, payload_end, record_offset)?;
        entry.footer_offset = Some(payload_end);
        entry.prev_same_block_offset = decoded.prev_same_block_offset;
        entry.prev_same_key_offset = decoded.prev_same_key_offset;
    }
    entry.validate_payload_extent(snapshot.len())?;
    Ok(entry)
}

fn append_log_start_for_file(file: &VarveFile) -> Result<u64> {
    if let Some(matrix) = &file.matrix {
        return Ok(matrix.append_log_start());
    }
    // From the file's own region, not from `spec`: a block this build does not
    // know is skipped at open rather than refused, so a file can carry a longer
    // header than this build writes. Deriving the boundary from `spec` would
    // put the append log inside that block's bytes.
    let extension_len = u64::try_from(file.header_extensions.len()).map_err(|_| {
        Error::ResourceArithmeticOverflow {
            resource: "file-header length",
        }
    })?;
    Ok(native_file_header_len(file.spec, extension_len))
}

fn validate_generation_file(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    expected_entries: &[RecordIndexEntry],
) -> Result<()> {
    validate_generation_file_inner(spec, file, append_start, expected_entries, false)
}

fn validate_replacement_generation_file(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    expected_entries: &[RecordIndexEntry],
) -> Result<()> {
    validate_generation_file_inner(spec, file, append_start, expected_entries, true)
}

fn validate_generation_file_inner(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    expected_entries: &[RecordIndexEntry],
    strict_checkpoints: bool,
) -> Result<()> {
    let file_len = check_open_file_len(spec, file)?;
    let header_len = read_file_header(spec, file)?;
    if append_start < header_len || append_start > file_len {
        return Err(Error::InvalidCanonicalEncoding(
            "invalid native append-log boundary",
        ));
    }
    let mut accounting = ScanAccounting::default();
    accounting.advance(spec, append_start)?;
    let mut offset = append_start;
    for (position, expected) in expected_entries.iter().enumerate() {
        if expected.record_offset != offset {
            return Err(Error::InvalidCanonicalEncoding(
                "native record offsets are not contiguous",
            ));
        }
        let actual = match read_record_entry_at(
            spec,
            file,
            file_len,
            offset,
            // Generation validation proves a just-published file matches what
            // was indexed; verifying is the point of this walk, so it does not
            // consult the open-time policy.
            ScanChecks {
                partial_boundary: None,
                checksum_boundary: None,
                verify_checksums: true,
            },
            &mut accounting,
        )? {
            RecordRead::Entry(entry) => entry,
            RecordRead::RecoverableTail(tail) => {
                return Err(Error::CorruptTail {
                    offset: tail.offset,
                });
            }
        };
        if !physical_record_headers_match(expected, &actual) {
            return Err(Error::InvalidCanonicalEncoding(
                "native record changed after it was indexed",
            ));
        }
        if actual.block_id == INDEX_BLOCK_ID
            && (1..=INDEX_CHECKPOINT_VERSION).contains(&actual.block_version)
        {
            // Only the strict path uses the entries; everything else needed
            // the two ceilings, which the prefix decides.
            if !strict_checkpoints {
                let prefix = actual.read_payload_prefix_file_with_len(
                    file,
                    file_len,
                    INDEX_CHECKPOINT_PREFIX_LEN,
                )?;
                check_index_checkpoint_limits(spec, &prefix)?;
            }
            if strict_checkpoints {
                let payload = actual.read_payload_file_with_len(file, file_len)?;
                inspect_index_checkpoint(
                    spec,
                    &payload,
                    append_start,
                    file_len,
                    &actual,
                    position,
                    expected_entries.iter().take(position),
                )?;
                let checkpoint = decode_index_checkpoint(spec, &payload, file_len)?;
                validate_index_checkpoint(
                    append_start,
                    file_len,
                    &actual,
                    &checkpoint,
                    position,
                    expected_entries.iter().take(position),
                )?;
            }
        }
        offset = actual.checked_physical_end()?;
    }
    if offset != file_len {
        return Err(Error::CorruptTail { offset });
    }
    validate_unique_sequences(expected_entries)
}

fn physical_record_headers_match(left: &RecordIndexEntry, right: &RecordIndexEntry) -> bool {
    left.block_id == right.block_id
        && left.block_version == right.block_version
        && left.flags == right.flags
        && left.sequence == right.sequence
        && left.record_offset == right.record_offset
        && left.payload_offset == right.payload_offset
        && left.payload_len == right.payload_len
        && left.checksum == right.checksum
        && left.uncompressed_len_hint == right.uncompressed_len_hint
        && left.footer_offset == right.footer_offset
        && left.prev_same_block_offset == right.prev_same_block_offset
        && left.prev_same_key_offset == right.prev_same_key_offset
}

#[derive(Clone, Copy)]
enum RewritePayload<'a> {
    Bytes(&'a [u8]),
    Snapshot { offset: u64, len: u64 },
}

fn rewrite_record_streaming(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    output: &mut File,
    mut entry: RecordIndexEntry,
    payload: RewritePayload<'_>,
) -> Result<RecordIndexEntry> {
    let payload_len = match payload {
        RewritePayload::Bytes(bytes) => {
            u64::try_from(bytes.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?
        }
        RewritePayload::Snapshot { len, .. } => len,
    };
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    let logical_len = match payload {
        RewritePayload::Bytes(bytes) => record_logical_len_from_parts(
            spec,
            entry.block_id,
            entry.flags,
            entry.uncompressed_len_hint,
            bytes,
        )?,
        RewritePayload::Snapshot { .. } => entry.logical_payload_len_snapshot(spec, snapshot)?,
    };
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;

    let record_offset = output.stream_position()?;
    let payload_offset =
        record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "rewrite file length",
            })?;
    let record_end =
        payload_offset
            .checked_add(payload_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "rewrite file length",
            })?;
    let header = RecordHeaderFields {
        block_id: entry.block_id,
        block_version: entry.block_version,
        flags: entry.flags,
        sequence: entry.sequence,
        payload_len,
        checksum: 0,
        uncompressed_len_hint: entry.uncompressed_len_hint,
    };
    write_record_header(output, spec, record_offset, header)?;
    match payload {
        RewritePayload::Bytes(bytes) => output.write_all(bytes)?,
        RewritePayload::Snapshot { offset, len } => {
            snapshot.copy_range_to(offset, len, output)?;
        }
    }

    entry.record_offset = record_offset;
    entry.payload_offset = payload_offset;
    entry.payload_len = payload_len;
    entry.footer_offset = None;
    entry.prev_same_block_offset = None;
    entry.prev_same_key_offset = None;
    entry.checksum = match spec.integrity_policy {
        IntegrityPolicy::None => 0,
        IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader => {
            checksum_record_file(spec, output, &entry, &[])?
        }
    };
    output.seek(SeekFrom::Start(record_offset))?;
    write_record_header(
        output,
        spec,
        record_offset,
        RecordHeaderFields {
            checksum: entry.checksum,
            ..header
        },
    )?;
    output.seek(SeekFrom::Start(record_end))?;
    Ok(entry)
}

fn rewrite_replacement_record_streaming(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    output: &mut File,
    mut entry: RecordIndexEntry,
    payload: RewritePayload<'_>,
    replacement: ReplacementInfo,
    rewritten_prefix: &[RecordIndexEntry],
) -> Result<RecordIndexEntry> {
    let payload_len = match payload {
        RewritePayload::Bytes(bytes) => {
            u64::try_from(bytes.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?
        }
        RewritePayload::Snapshot { len, .. } => len,
    };
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    let logical_len = match payload {
        RewritePayload::Bytes(bytes) => record_logical_len_from_parts(
            spec,
            entry.block_id,
            entry.flags,
            entry.uncompressed_len_hint,
            bytes,
        )?,
        RewritePayload::Snapshot { .. } => entry.logical_payload_len_snapshot(spec, snapshot)?,
    };
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;

    let record_offset = output.stream_position()?;
    let payload_offset =
        record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "replacement file length",
            })?;
    let payload_end =
        payload_offset
            .checked_add(payload_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "replacement file length",
            })?;
    let record_end = payload_end.checked_add(record_footer_len(spec)).ok_or(
        Error::ResourceArithmeticOverflow {
            resource: "replacement file length",
        },
    )?;

    entry.record_offset = record_offset;
    entry.payload_offset = payload_offset;
    entry.payload_len = payload_len;
    entry.footer_offset = spec.spec_needs_record_footer().then_some(payload_end);
    entry.prev_same_block_offset = entry
        .prev_same_block_offset
        .map(|offset| replacement.translate_record_offset(offset))
        .transpose()?;
    entry.prev_same_key_offset = entry
        .prev_same_key_offset
        .map(|offset| replacement.translate_record_offset(offset))
        .transpose()?;

    let header = RecordHeaderFields {
        block_id: entry.block_id,
        block_version: entry.block_version,
        flags: entry.flags,
        sequence: entry.sequence,
        payload_len,
        checksum: 0,
        uncompressed_len_hint: entry.uncompressed_len_hint,
    };
    write_record_header(output, spec, record_offset, header)?;
    match payload {
        RewritePayload::Bytes(bytes) => output.write_all(bytes)?,
        RewritePayload::Snapshot { offset, len } => snapshot.copy_range_to(offset, len, output)?,
    }
    let encoded_footer = if spec.spec_needs_record_footer() {
        let footer = encode_record_footer(RecordFooterFields {
            prev_same_block_offset: entry.prev_same_block_offset,
            prev_same_key_offset: entry.prev_same_key_offset,
        })?;
        output.write_all(&footer)?;
        Some(footer)
    } else {
        None
    };
    let footer = encoded_footer.as_ref().map_or(&[][..], |bytes| &bytes[..]);
    validate_replacement_predecessors(output, &entry, rewritten_prefix)?;

    entry.checksum = match spec.integrity_policy {
        IntegrityPolicy::None => 0,
        IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader => {
            checksum_record_file(spec, output, &entry, footer)?
        }
    };
    output.seek(SeekFrom::Start(record_offset))?;
    write_record_header(
        output,
        spec,
        record_offset,
        RecordHeaderFields {
            checksum: entry.checksum,
            ..header
        },
    )?;
    output.seek(SeekFrom::Start(record_end))?;
    Ok(entry)
}

/// Finds the already-rewritten record that sits at `offset`.
///
/// `rewritten_prefix` is `replace_block`'s `new_index` under construction. Each
/// entry is written at `output.stream_position()` and pushed in that order, so
/// the slice is strictly increasing in `record_offset` — the `debug_assert!` at
/// the `push` states that where the invariant is created, which is O(1), unlike
/// asserting sortedness here on every lookup.
///
/// This used to be `iter().find(..)`, which made `replace_block` quadratic in
/// the record count for any spec that carries a record footer with a
/// predecessor offset (`block_offset_chain`, `keyed_offset_chain`,
/// `segment_on_flush`). A `HashMap<u64, usize>` was the obvious alternative and
/// is worse here: it adds uncharged bytes per record to a path that already
/// materialises the whole index, while the sortedness makes an
/// allocation-free `O(log N)` lookup available for nothing.
///
/// Bounded failure mode if that sortedness were ever broken: `binary_search_by`
/// returns `Ok` only when a comparison answered `Equal`, so the entry it hands
/// back always satisfies `record_offset == offset` and can never substitute a
/// different record's predecessor. The worst it can do is miss an entry that is
/// present, which is the existing `Error::InvalidRecordFooter` refusal.
fn find_rewritten_by_offset(
    rewritten_prefix: &[RecordIndexEntry],
    offset: u64,
) -> Option<&RecordIndexEntry> {
    rewritten_prefix
        .binary_search_by(|candidate| {
            note_replacement_predecessor_probe();
            candidate.record_offset.cmp(&offset)
        })
        .ok()
        .map(|position| &rewritten_prefix[position])
}

fn validate_replacement_predecessors(
    output: &mut File,
    entry: &RecordIndexEntry,
    rewritten_prefix: &[RecordIndexEntry],
) -> Result<()> {
    if let Some(offset) = entry.prev_same_block_offset {
        let Some(previous) = find_rewritten_by_offset(rewritten_prefix, offset) else {
            return Err(Error::InvalidRecordFooter { offset });
        };
        if offset >= entry.record_offset || previous.block_id != entry.block_id {
            return Err(Error::InvalidRecordFooter { offset });
        }
    }
    if let Some(offset) = entry.prev_same_key_offset {
        let Some(previous) = find_rewritten_by_offset(rewritten_prefix, offset) else {
            return Err(Error::InvalidRecordFooter { offset });
        };
        if offset >= entry.record_offset
            || replacement_chain_block_id(output, previous)?
                != replacement_chain_block_id(output, entry)?
        {
            return Err(Error::InvalidRecordFooter { offset });
        }
    }
    Ok(())
}

fn replacement_chain_block_id(file: &mut File, entry: &RecordIndexEntry) -> Result<u32> {
    if !matches!(entry.block_id, TOMBSTONE_BLOCK_ID | OP_BLOCK_ID) {
        return Ok(entry.block_id);
    }
    if entry.payload_len < 4 {
        return Err(Error::InvalidRecordFooter {
            offset: entry.record_offset,
        });
    }
    file.seek(SeekFrom::Start(entry.payload_offset))?;
    let mut block_id = [0; 4];
    file.read_exact(&mut block_id)?;
    Ok(u32::from_le_bytes(block_id))
}

fn encode_index_checkpoint_payload(
    spec: FormatSpec,
    entries: &[RecordIndexEntry],
    covered_offset: u64,
) -> Result<Vec<u8>> {
    const PREFIX_LEN: u64 = 4 + 2 + 8 + 8;
    const ENTRY_LEN: u64 = 4 + 2 + 2 + 8 + 8 + 8 + 8 + 4 + 4 + 8 + 8 + 8 + 1;
    let count = u64::try_from(entries.len()).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "checkpoint entry count",
    })?;
    let payload_len = count
        .checked_mul(ENTRY_LEN)
        .and_then(|bytes| bytes.checked_add(PREFIX_LEN))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "checkpoint payload length",
        })?;
    spec.read_limits.check(ReadLimitKey::Records, count)?;
    spec.read_limits.check(
        ReadLimitKey::IndexBytes,
        index_bytes_for_count(entries.len())?,
    )?;
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, payload_len)?;
    let capacity =
        usize::try_from(payload_len).map_err(|_| Error::LengthOverflow { value: payload_len })?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource: "checkpoint payload",
            requested: payload_len,
        })?;
    payload.extend_from_slice(INDEX_CHECKPOINT_MAGIC);
    payload.extend_from_slice(&INDEX_CHECKPOINT_VERSION.to_le_bytes());
    payload.extend_from_slice(&covered_offset.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    for entry in entries {
        push_index_entry_bytes(&mut payload, entry, entry.committed);
    }
    Ok(payload)
}

/// Serializes one index entry in the layout [`read_index_entry_payload`]
/// decodes at checkpoint version 3.
///
/// `committed` is a parameter rather than a field read because the two writers
/// answer it differently: a checkpoint records the writer's live view, while a
/// segment is only ever written once its whole coverage is committed, so it
/// records what a scan of the same bytes would produce.
fn push_index_entry_bytes(payload: &mut Vec<u8>, entry: &RecordIndexEntry, committed: bool) {
    payload.extend_from_slice(&entry.block_id.to_le_bytes());
    payload.extend_from_slice(&entry.block_version.to_le_bytes());
    payload.extend_from_slice(&entry.flags.to_le_bytes());
    payload.extend_from_slice(&entry.sequence.to_le_bytes());
    payload.extend_from_slice(&entry.record_offset.to_le_bytes());
    payload.extend_from_slice(&entry.payload_offset.to_le_bytes());
    payload.extend_from_slice(&entry.payload_len.to_le_bytes());
    payload.extend_from_slice(&entry.checksum.to_le_bytes());
    payload.extend_from_slice(&entry.uncompressed_len_hint.to_le_bytes());
    payload.extend_from_slice(&entry.footer_offset.unwrap_or(0).to_le_bytes());
    payload.extend_from_slice(&entry.prev_same_block_offset.unwrap_or(0).to_le_bytes());
    payload.extend_from_slice(&entry.prev_same_key_offset.unwrap_or(0).to_le_bytes());
    payload.push(u8::from(committed));
}

/// The payload length a segment record covering `count` entries occupies.
fn segment_payload_len(count: u64) -> Result<u64> {
    count
        .checked_mul(SEGMENT_ENTRY_LEN)
        .and_then(|bytes| bytes.checked_add(SEGMENT_PREFIX_LEN))
        .and_then(|bytes| bytes.checked_add(SEGMENT_TRAILER_LEN))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "segment payload length",
        })
}

/// Serializes one segment: the records a single commit point added, and the
/// segment record's own start offset as the closing trailer.
/// Encodes a segment's entry array from a stream.
///
/// `ExactSizeIterator` because the payload length is charged and reserved
/// before the first entry is written — the count has to be known, and a
/// `skip(start)` over the index knows it without the index being a slice.
fn encode_segment_payload<'a>(
    spec: FormatSpec,
    entries: impl ExactSizeIterator<Item = &'a RecordIndexEntry>,
    covered_start: u64,
    preceding_records: u64,
    record_offset: u64,
) -> Result<Vec<u8>> {
    let count = u64::try_from(entries.len()).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "segment entry count",
    })?;
    let payload_len = segment_payload_len(count)?;
    spec.read_limits.check(ReadLimitKey::Records, count)?;
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, payload_len)?;
    let capacity =
        usize::try_from(payload_len).map_err(|_| Error::LengthOverflow { value: payload_len })?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource: "segment payload",
            requested: payload_len,
        })?;
    payload.extend_from_slice(SEGMENT_MAGIC);
    payload.extend_from_slice(&SEGMENT_VERSION.to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&covered_start.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    payload.extend_from_slice(&preceding_records.to_le_bytes());
    for entry in entries {
        // A segment is written only after every record it covers is committed,
        // so this is not the writer's live `committed` bit - which stays false
        // for a data record under a marker policy - but what a scan of these
        // same bytes reports. The two open paths must agree entry for entry.
        push_index_entry_bytes(&mut payload, entry, true);
    }
    payload.extend_from_slice(&record_offset.to_le_bytes());
    Ok(payload)
}

/// One segment, as read back from its record.
#[derive(Debug)]
struct DecodedSegment {
    /// File offset of the first record this segment covers.
    covered_start: u64,
    /// Records in the file before `covered_start`.
    preceding_records: u64,
    entries: Vec<RecordIndexEntry>,
}

/// What the chain walk's first pass reads out of a segment's fixed prefix.
///
/// Only `covered_start`: the pass exists to check that the links join up, and
/// `preceding_records` is checked in pass two against the index actually built,
/// where it means something. Reading it here would be a number compared to
/// nothing.
#[derive(Clone, Copy, Debug)]
struct SegmentPrefix {
    covered_start: u64,
}

/// Decodes only the fixed prefix, which is everything the chain walk's first
/// pass needs to check that the links join up.
fn decode_segment_prefix(prefix: &[u8]) -> Result<SegmentPrefix> {
    let prefix_len = SEGMENT_PREFIX_LEN as usize;
    if prefix.len() < prefix_len || &prefix[..4] != SEGMENT_MAGIC {
        return Err(Error::InvalidIndexSegment);
    }
    let mut u16_buf = [0; 2];
    u16_buf.copy_from_slice(&prefix[4..6]);
    if u16::from_le_bytes(u16_buf) != SEGMENT_VERSION {
        return Err(Error::InvalidIndexSegment);
    }
    u16_buf.copy_from_slice(&prefix[6..8]);
    if u16::from_le_bytes(u16_buf) != 0 {
        return Err(Error::InvalidIndexSegment);
    }
    let mut u64_buf = [0; 8];
    u64_buf.copy_from_slice(&prefix[8..16]);
    Ok(SegmentPrefix {
        covered_start: u64::from_le_bytes(u64_buf),
    })
}

fn decode_segment_payload(
    spec: FormatSpec,
    payload: &[u8],
    record_offset: u64,
    file_len: u64,
) -> Result<DecodedSegment> {
    let prefix_len = SEGMENT_PREFIX_LEN as usize;
    let trailer_len = SEGMENT_TRAILER_LEN as usize;
    let entry_len = SEGMENT_ENTRY_LEN as usize;
    if payload.len() < prefix_len + trailer_len || &payload[..4] != SEGMENT_MAGIC {
        return Err(Error::InvalidIndexSegment);
    }
    let mut u16_buf = [0; 2];
    u16_buf.copy_from_slice(&payload[4..6]);
    if u16::from_le_bytes(u16_buf) != SEGMENT_VERSION {
        return Err(Error::InvalidIndexSegment);
    }
    u16_buf.copy_from_slice(&payload[6..8]);
    if u16::from_le_bytes(u16_buf) != 0 {
        return Err(Error::InvalidIndexSegment);
    }
    let mut u64_buf = [0; 8];
    u64_buf.copy_from_slice(&payload[8..16]);
    let covered_start = u64::from_le_bytes(u64_buf);
    u64_buf.copy_from_slice(&payload[16..24]);
    let count = u64::from_le_bytes(u64_buf);
    u64_buf.copy_from_slice(&payload[24..32]);
    let preceding_records = u64::from_le_bytes(u64_buf);
    // The trailer is what let this record be found from EOF at all; if it does
    // not name this record, the bytes at EOF were not this record's.
    u64_buf.copy_from_slice(&payload[payload.len() - trailer_len..]);
    if u64::from_le_bytes(u64_buf) != record_offset {
        return Err(Error::InvalidIndexSegment);
    }

    spec.read_limits.check(ReadLimitKey::Records, count)?;
    let count_usize = usize::try_from(count).map_err(|_| Error::LengthOverflow { value: count })?;
    let resident_bytes = index_bytes_for_count(count_usize)?;
    spec.read_limits
        .check(ReadLimitKey::IndexBytes, resident_bytes)?;
    let expected_len = usize::try_from(segment_payload_len(count)?)
        .map_err(|_| Error::LengthOverflow { value: count })?;
    if payload.len() != expected_len {
        return Err(Error::InvalidIndexSegment);
    }

    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count_usize)
        .map_err(|_| Error::AllocationFailed {
            resource: "segment index",
            requested: resident_bytes,
        })?;
    let mut position = prefix_len;
    for _ in 0..count_usize {
        let entry = read_index_entry_payload(
            &payload[position..position + entry_len],
            INDEX_CHECKPOINT_VERSION,
        );
        if entry.checked_physical_end()? > file_len {
            return Err(Error::InvalidIndexSegment);
        }
        // A segment never covers another segment: they are written last, so
        // the record after one is the first record of the next segment. A
        // chain that claimed otherwise could hide records between two links.
        if entry.block_id == SEGMENT_BLOCK_ID {
            return Err(Error::InvalidIndexSegment);
        }
        entries.push(entry);
        position += entry_len;
    }
    Ok(DecodedSegment {
        covered_start,
        preceding_records,
        entries,
    })
}

fn encode_schema_manifest(spec: FormatSpec) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&MANIFEST_PAYLOAD_VERSION.to_le_bytes());
    payload.extend_from_slice(&spec.version.to_le_bytes());
    payload.push(spec.endian.to_byte());
    payload.extend_from_slice(&spec.schema_hash.to_le_bytes());
    payload.push(index_policy_byte(spec.index_policy));
    payload.push(integrity_policy_byte(spec.integrity_policy));
    payload.push(recovery_policy_byte(spec.recovery_policy));
    payload.push(manifest_policy_byte(spec.manifest_policy));
    payload.push(commit_policy_byte(spec.commit_policy));
    match spec.extension {
        Some(extension) => {
            payload.push(1);
            let extension = extension.as_bytes();
            let extension_len =
                u16::try_from(extension.len()).map_err(|_| Error::InvalidSchemaManifest)?;
            payload.extend_from_slice(&extension_len.to_le_bytes());
            payload.extend_from_slice(extension);
        }
        None => payload.push(0),
    }
    encode_manifest_compression_policy(&mut payload, spec.compression_policy);
    payload.extend_from_slice(&(spec.blocks.len() as u32).to_le_bytes());
    for block in spec.blocks {
        payload.extend_from_slice(&block.id.to_le_bytes());
        payload.extend_from_slice(&block.version.to_le_bytes());
        payload.push(block_kind_byte(block.kind));
        let name = block.name.as_bytes();
        let name_len = u16::try_from(name.len()).map_err(|_| Error::InvalidSchemaManifest)?;
        payload.extend_from_slice(&name_len.to_le_bytes());
        payload.extend_from_slice(name);
        payload.extend_from_slice(&(block.fields.len() as u32).to_le_bytes());
        for field in block.fields {
            payload.extend_from_slice(&field.id.to_le_bytes());
            payload.push(wire_type_byte(field.wire_type));
            payload.push(field_presence_byte(field.presence));
            let name = field.name.as_bytes();
            let name_len = u16::try_from(name.len()).map_err(|_| Error::InvalidSchemaManifest)?;
            payload.extend_from_slice(&name_len.to_le_bytes());
            payload.extend_from_slice(name);
        }
    }
    Ok(payload)
}

fn decode_schema_manifest(
    payload: &[u8],
    budget: &mut MaterializationBudget,
) -> Result<SchemaManifest> {
    let mut cursor = ManifestCursor { payload, offset: 0 };
    let payload_version = cursor.read_u16()?;
    if !(1..=MANIFEST_PAYLOAD_VERSION).contains(&payload_version) {
        return Err(Error::InvalidSchemaManifest);
    }
    let format_version = cursor.read_u16()?;
    let endian = Endian::from_byte(cursor.read_u8()?).ok_or(Error::InvalidSchemaManifest)?;
    let schema_hash = cursor.read_u64()?;
    let index_policy = index_policy_from_byte(cursor.read_u8()?)?;
    let integrity_policy = integrity_policy_from_byte(cursor.read_u8()?)?;
    let recovery_policy = recovery_policy_from_byte(cursor.read_u8()?)?;
    let manifest_policy = manifest_policy_from_byte(cursor.read_u8()?)?;
    let commit_policy = if payload_version >= 4 {
        commit_policy_from_byte(cursor.read_u8()?)?
    } else {
        CommitPolicy::None
    };
    let (extension, compression_policy) = if payload_version >= 3 {
        let extension = match cursor.read_u8()? {
            0 => None,
            1 => {
                let len = cursor.read_u16()? as usize;
                Some(cursor.read_string(len, budget)?)
            }
            _ => return Err(Error::InvalidSchemaManifest),
        };
        let compression_policy = decode_manifest_compression_policy(&mut cursor)?;
        (extension, compression_policy)
    } else {
        (None, CompressionPolicy::None)
    };
    let block_count = cursor.read_u32()? as usize;
    let minimum_block_len = if payload_version >= 2 { 13 } else { 9 };
    preflight_manifest_count(block_count, minimum_block_len, cursor.remaining())?;
    consume_manifest_items::<SchemaBlockDescriptor>(budget, block_count)?;
    let mut blocks = Vec::new();
    blocks
        .try_reserve_exact(block_count)
        .map_err(|_| manifest_allocation_failed::<SchemaBlockDescriptor>(block_count))?;
    for _ in 0..block_count {
        let id = cursor.read_u32()?;
        let version = cursor.read_u16()?;
        let kind = block_kind_from_byte(cursor.read_u8()?)?;
        let name_len = cursor.read_u16()? as usize;
        let name = cursor.read_string(name_len, budget)?;
        let fields = if payload_version >= 2 {
            let field_count = cursor.read_u32()? as usize;
            preflight_manifest_count(field_count, 8, cursor.remaining())?;
            consume_manifest_items::<SchemaFieldDescriptor>(budget, field_count)?;
            let mut fields = Vec::new();
            fields
                .try_reserve_exact(field_count)
                .map_err(|_| manifest_allocation_failed::<SchemaFieldDescriptor>(field_count))?;
            for _ in 0..field_count {
                let id = cursor.read_u32()?;
                let wire_type = wire_type_from_byte(cursor.read_u8()?)?;
                let presence = field_presence_from_byte(cursor.read_u8()?)?;
                let name_len = cursor.read_u16()? as usize;
                let name = cursor.read_string(name_len, budget)?;
                fields.push(SchemaFieldDescriptor {
                    id,
                    name,
                    wire_type,
                    presence,
                });
            }
            fields
        } else {
            Vec::new()
        };
        blocks.push(SchemaBlockDescriptor {
            id,
            name,
            version,
            kind,
            fields,
        });
    }
    if cursor.remaining() != 0 {
        return Err(Error::InvalidSchemaManifest);
    }
    Ok(SchemaManifest {
        payload_version,
        format_version,
        endian,
        schema_hash,
        extension,
        index_policy,
        commit_policy,
        integrity_policy,
        recovery_policy,
        manifest_policy,
        compression_policy,
        blocks,
    })
}

struct ManifestCursor<'a> {
    payload: &'a [u8],
    offset: usize,
}

impl ManifestCursor<'_> {
    fn remaining(&self) -> usize {
        self.payload.len().saturating_sub(self.offset)
    }

    fn read_exact(&mut self, len: usize) -> Result<&[u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(Error::InvalidSchemaManifest)?;
        if end > self.payload.len() {
            return Err(Error::InvalidSchemaManifest);
        }
        let value = &self.payload[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16> {
        let mut bytes = [0; 2];
        bytes.copy_from_slice(self.read_exact(2)?);
        Ok(u16::from_le_bytes(bytes))
    }

    fn read_u32(&mut self) -> Result<u32> {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(self.read_exact(4)?);
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_u64(&mut self) -> Result<u64> {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(self.read_exact(8)?);
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_i32(&mut self) -> Result<i32> {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(self.read_exact(4)?);
        Ok(i32::from_le_bytes(bytes))
    }

    fn read_string(&mut self, len: usize, budget: &mut MaterializationBudget) -> Result<String> {
        let bytes = self.read_exact(len)?;
        let value = std::str::from_utf8(bytes).map_err(|_| Error::InvalidSchemaManifest)?;
        let requested = u64::try_from(len).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "schema manifest string",
        })?;
        budget.consume(requested)?;
        let mut output = String::new();
        output
            .try_reserve_exact(len)
            .map_err(|_| Error::AllocationFailed {
                resource: "schema manifest string",
                requested,
            })?;
        output.push_str(value);
        Ok(output)
    }
}

fn preflight_manifest_count(count: usize, minimum_item_len: usize, remaining: usize) -> Result<()> {
    let minimum_extent = count
        .checked_mul(minimum_item_len)
        .ok_or(Error::InvalidSchemaManifest)?;
    if minimum_extent > remaining {
        return Err(Error::InvalidSchemaManifest);
    }
    Ok(())
}

fn consume_manifest_items<T>(budget: &mut MaterializationBudget, count: usize) -> Result<()> {
    let requested = count
        .checked_mul(size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "schema manifest output",
        })?;
    budget.consume(requested)
}

fn manifest_allocation_failed<T>(count: usize) -> Error {
    let requested = count
        .checked_mul(size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .unwrap_or(u64::MAX);
    Error::AllocationFailed {
        resource: "schema manifest output",
        requested,
    }
}

fn block_kind_byte(kind: BlockKind) -> u8 {
    match kind {
        BlockKind::Fixed => 1,
        BlockKind::Variable => 2,
        BlockKind::Matrix => 3,
        BlockKind::Internal => 4,
    }
}

fn block_kind_from_byte(value: u8) -> Result<BlockKind> {
    match value {
        1 => Ok(BlockKind::Fixed),
        2 => Ok(BlockKind::Variable),
        3 => Ok(BlockKind::Matrix),
        4 => Ok(BlockKind::Internal),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

fn wire_type_byte(wire_type: WireType) -> u8 {
    wire_type as u8
}

fn wire_type_from_byte(value: u8) -> Result<WireType> {
    WireType::from_u16(u16::from(value)).ok_or(Error::InvalidSchemaManifest)
}

fn field_presence_byte(presence: crate::FieldPresence) -> u8 {
    match presence {
        crate::FieldPresence::Required => 1,
        crate::FieldPresence::Defaulted => 2,
    }
}

fn field_presence_from_byte(value: u8) -> Result<crate::FieldPresence> {
    match value {
        1 => Ok(crate::FieldPresence::Required),
        2 => Ok(crate::FieldPresence::Defaulted),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

fn index_policy_byte(policy: IndexPolicy) -> u8 {
    (if policy.scan_on_open { 1 } else { 0 })
        | (if policy.checkpoint_on_flush {
            1 << 1
        } else {
            0
        })
        | (if policy.block_offset_chain { 1 << 2 } else { 0 })
        | (if policy.keyed_offset_chain { 1 << 3 } else { 0 })
        | (if policy.segment_on_flush { 1 << 4 } else { 0 })
}

fn index_policy_from_byte(value: u8) -> Result<IndexPolicy> {
    match value {
        1 => Ok(IndexPolicy::ScanOnOpen),
        2 => Ok(IndexPolicy::CheckpointOnFlush),
        3..=31 => Ok(IndexPolicy::new(
            value & 0x01 != 0,
            value & 0x02 != 0,
            value & 0x04 != 0,
            value & 0x08 != 0,
        )
        .with_segment_on_flush(value & 0x10 != 0)),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

fn commit_policy_byte(policy: CommitPolicy) -> u8 {
    match policy {
        CommitPolicy::None => 1,
        CommitPolicy::RecordFooter => 2,
        CommitPolicy::TransactionMarker(crate::TransactionMarkerMode::OnFlush) => 3,
        CommitPolicy::TransactionMarker(crate::TransactionMarkerMode::Explicit) => 4,
    }
}

fn commit_policy_from_byte(value: u8) -> Result<CommitPolicy> {
    match value {
        1 => Ok(CommitPolicy::None),
        2 => Ok(CommitPolicy::RecordFooter),
        3 => Ok(CommitPolicy::TransactionMarker(
            crate::TransactionMarkerMode::OnFlush,
        )),
        4 => Ok(CommitPolicy::TransactionMarker(
            crate::TransactionMarkerMode::Explicit,
        )),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

fn integrity_policy_byte(policy: IntegrityPolicy) -> u8 {
    match policy {
        IntegrityPolicy::None => 1,
        IntegrityPolicy::Crc32 => 2,
        IntegrityPolicy::Crc32WithHeader => 3,
    }
}

fn integrity_policy_from_byte(value: u8) -> Result<IntegrityPolicy> {
    match value {
        1 => Ok(IntegrityPolicy::None),
        2 => Ok(IntegrityPolicy::Crc32),
        3 => Ok(IntegrityPolicy::Crc32WithHeader),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

fn recovery_policy_byte(policy: RecoveryPolicy) -> u8 {
    match policy {
        RecoveryPolicy::Strict => 1,
        RecoveryPolicy::TruncateTail => 2,
    }
}

fn recovery_policy_from_byte(value: u8) -> Result<RecoveryPolicy> {
    match value {
        1 => Ok(RecoveryPolicy::Strict),
        2 => Ok(RecoveryPolicy::TruncateTail),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

fn manifest_policy_byte(policy: ManifestPolicy) -> u8 {
    match policy {
        ManifestPolicy::None => 1,
        ManifestPolicy::Embedded => 2,
    }
}

fn manifest_policy_from_byte(value: u8) -> Result<ManifestPolicy> {
    match value {
        1 => Ok(ManifestPolicy::None),
        2 => Ok(ManifestPolicy::Embedded),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

fn encode_manifest_compression_policy(payload: &mut Vec<u8>, policy: CompressionPolicy) {
    match policy {
        CompressionPolicy::None => payload.push(0),
        CompressionPolicy::VariableBlocks(compression) => {
            payload.push(1);
            payload.push(compression_algorithm_byte(compression.algorithm));
            payload.push(compression_level_kind_byte(compression.level));
            payload.extend_from_slice(&compression_level_exact(compression.level).to_le_bytes());
            payload.push(compression_header_mode_byte(compression.header_mode));
            payload.extend_from_slice(&compression.min_uncompressed_len.to_le_bytes());
            payload.push(u8::from(compression.only_if_smaller));
            payload.extend_from_slice(&compression.max_uncompressed_len.to_le_bytes());
        }
    }
}

fn decode_manifest_compression_policy(
    cursor: &mut ManifestCursor<'_>,
) -> Result<CompressionPolicy> {
    match cursor.read_u8()? {
        0 => Ok(CompressionPolicy::None),
        1 => {
            let algorithm = compression_algorithm_from_byte(cursor.read_u8()?)
                .map_err(|_| Error::InvalidSchemaManifest)?;
            let level_kind = cursor.read_u8()?;
            let level_exact = cursor.read_i32()?;
            let level = compression_level_from_parts(level_kind, level_exact)
                .map_err(|_| Error::InvalidSchemaManifest)?;
            let header_mode = compression_header_mode_from_byte(cursor.read_u8()?)
                .map_err(|_| Error::InvalidSchemaManifest)?;
            let min_uncompressed_len = cursor.read_u64()?;
            let only_if_smaller = match cursor.read_u8()? {
                0 => false,
                1 => true,
                _ => return Err(Error::InvalidSchemaManifest),
            };
            let max_uncompressed_len = cursor.read_u64()?;
            Ok(CompressionPolicy::VariableBlocks(VariableCompression {
                algorithm,
                level,
                header_mode,
                min_uncompressed_len,
                only_if_smaller,
                max_uncompressed_len,
            }))
        }
        _ => Err(Error::InvalidSchemaManifest),
    }
}

fn prepare_user_record_payload<'a>(
    spec: FormatSpec,
    block_id: u32,
    kind: BlockKind,
    logical_payload: &'a [u8],
) -> Result<StoredPayload<'a>> {
    spec.read_limits.check(
        ReadLimitKey::LogicalPayloadLen,
        u64::try_from(logical_payload.len())
            .map_err(|_| Error::LengthOverflow { value: u64::MAX })?,
    )?;
    let Some(compression) = variable_compression_for_block(spec, block_id) else {
        return Ok(uncompressed_user_payload(logical_payload));
    };
    if kind != BlockKind::Variable {
        return Ok(uncompressed_user_payload(logical_payload));
    }

    let logical_len = logical_payload.len() as u64;
    if logical_len > compression.max_uncompressed_len {
        return Err(Error::DecompressedLengthLimitExceeded {
            actual: logical_len,
            limit: compression.max_uncompressed_len,
        });
    }
    if logical_len < compression.min_uncompressed_len {
        return Ok(uncompressed_user_payload(logical_payload));
    }

    let compressed =
        compress_with_algorithm(compression.algorithm, compression.level, logical_payload)?;
    let stored_bytes = match compression.header_mode {
        CompressionHeaderMode::RecordExplicit => {
            encode_compression_envelope(compression, logical_len, &compressed)?
        }
        CompressionHeaderMode::FileExplicit | CompressionHeaderMode::FormatContract => compressed,
    };

    if compression.only_if_smaller && stored_bytes.len() >= logical_payload.len() {
        return Ok(uncompressed_user_payload(logical_payload));
    }

    let uncompressed_len_hint = match u32::try_from(logical_len) {
        Ok(value) => value,
        Err(_) if compression.header_mode == CompressionHeaderMode::RecordExplicit => 0,
        Err(_) => return Err(Error::LengthOverflow { value: logical_len }),
    };
    Ok(StoredPayload {
        flags: RECORD_FLAG_COMPRESSED,
        uncompressed_len_hint,
        bytes: Cow::Owned(stored_bytes),
    })
}

/// The stored form of a payload that is stored exactly as it was handed in.
///
/// Borrows. The bytes are the caller's and are read, not kept: every caller
/// reborrows `&payload.bytes` into the record writer and drops the
/// `StoredPayload` on the same statement.
fn uncompressed_user_payload(logical_payload: &[u8]) -> StoredPayload<'_> {
    StoredPayload {
        flags: 0,
        uncompressed_len_hint: 0,
        bytes: Cow::Borrowed(logical_payload),
    }
}

fn record_logical_len_from_parts(
    spec: FormatSpec,
    block_id: u32,
    flags: u16,
    uncompressed_len_hint: u32,
    payload: &[u8],
) -> Result<u64> {
    if flags & RECORD_FLAG_COMPRESSED == 0 {
        return u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX });
    }
    let compression =
        variable_compression_for_block(spec, block_id).ok_or(Error::InvalidCompressionHeader)?;
    let logical_len = match compression.header_mode {
        CompressionHeaderMode::RecordExplicit => {
            let (_, logical_len, _) = decode_compression_envelope(payload)?;
            logical_len
        }
        CompressionHeaderMode::FileExplicit | CompressionHeaderMode::FormatContract => {
            if uncompressed_len_hint == 0 {
                return Err(Error::InvalidCompressionHeader);
            }
            u64::from(uncompressed_len_hint)
        }
    };
    if logical_len > compression.max_uncompressed_len {
        return Err(Error::DecompressedLengthLimitExceeded {
            actual: logical_len,
            limit: compression.max_uncompressed_len,
        });
    }
    Ok(logical_len)
}

fn decode_record_payload(
    spec: FormatSpec,
    entry: &RecordIndexEntry,
    physical_payload: Vec<u8>,
) -> Result<Vec<u8>> {
    if !entry.is_compressed() {
        return Ok(physical_payload);
    }
    validate_record_entry(spec, entry)?;
    let compression = variable_compression_for_block(spec, entry.block_id)
        .ok_or(Error::InvalidCompressionHeader)?;
    let (algorithm, expected_len, compressed_payload) = match compression.header_mode {
        CompressionHeaderMode::RecordExplicit => decode_compression_envelope(&physical_payload)?,
        CompressionHeaderMode::FileExplicit | CompressionHeaderMode::FormatContract => {
            if entry.uncompressed_len_hint == 0 {
                return Err(Error::InvalidCompressionHeader);
            }
            (
                compression.algorithm,
                u64::from(entry.uncompressed_len_hint),
                physical_payload.as_slice(),
            )
        }
    };
    if expected_len > compression.max_uncompressed_len {
        return Err(Error::DecompressedLengthLimitExceeded {
            actual: expected_len,
            limit: compression.max_uncompressed_len,
        });
    }
    let decoded = decompress_with_algorithm(algorithm, compressed_payload, expected_len)?;
    let actual_len = decoded.len() as u64;
    if actual_len != expected_len {
        return Err(Error::DecompressedLengthMismatch {
            expected: expected_len,
            actual: actual_len,
        });
    }
    Ok(decoded)
}

fn encode_compression_envelope(
    compression: VariableCompression,
    uncompressed_len: u64,
    compressed_payload: &[u8],
) -> Result<Vec<u8>> {
    let capacity =
        compressed_payload
            .len()
            .checked_add(20)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "compression envelope",
            })?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource: "compression envelope",
            requested: u64::try_from(capacity).unwrap_or(u64::MAX),
        })?;
    payload.extend_from_slice(COMPRESSION_ENVELOPE_MAGIC);
    payload.push(COMPRESSION_ENVELOPE_VERSION);
    payload.push(compression_algorithm_byte(compression.algorithm));
    payload.push(compression_level_kind_byte(compression.level));
    payload.push(0);
    payload.extend_from_slice(&compression_level_exact(compression.level).to_le_bytes());
    payload.extend_from_slice(&uncompressed_len.to_le_bytes());
    payload.extend_from_slice(compressed_payload);
    Ok(payload)
}

fn decode_compression_envelope(payload: &[u8]) -> Result<(CompressionAlgorithm, u64, &[u8])> {
    const ENVELOPE_PREFIX_LEN: usize = 4 + 1 + 1 + 1 + 1 + 4 + 8;
    if payload.len() < ENVELOPE_PREFIX_LEN || &payload[..4] != COMPRESSION_ENVELOPE_MAGIC {
        return Err(Error::InvalidCompressionHeader);
    }
    if payload[4] != COMPRESSION_ENVELOPE_VERSION || payload[7] != 0 {
        return Err(Error::InvalidCompressionHeader);
    }
    let algorithm = compression_algorithm_from_byte(payload[5])?;
    let level_kind = payload[6];
    let mut level_exact = [0; 4];
    level_exact.copy_from_slice(&payload[8..12]);
    let _level = compression_level_from_parts(level_kind, i32::from_le_bytes(level_exact))?;
    let mut len = [0; 8];
    len.copy_from_slice(&payload[12..20]);
    Ok((
        algorithm,
        u64::from_le_bytes(len),
        &payload[ENVELOPE_PREFIX_LEN..],
    ))
}

fn variable_compression(spec: FormatSpec) -> Option<VariableCompression> {
    match spec.compression_policy {
        CompressionPolicy::None => None,
        CompressionPolicy::VariableBlocks(compression) => Some(compression),
    }
}

fn variable_compression_for_block(spec: FormatSpec, block_id: u32) -> Option<VariableCompression> {
    spec.block_compression
        .iter()
        .find(|descriptor| descriptor.block_id == block_id)
        .map(|descriptor| descriptor.compression)
        .or_else(|| variable_compression(spec))
}

fn uses_file_explicit_compression(spec: FormatSpec) -> bool {
    matches!(
        variable_compression(spec).map(|compression| compression.header_mode),
        Some(CompressionHeaderMode::FileExplicit)
    )
}

fn uses_format_contract_compression(spec: FormatSpec) -> bool {
    matches!(
        variable_compression(spec).map(|compression| compression.header_mode),
        Some(CompressionHeaderMode::FormatContract)
    )
}

fn validate_record_entry(spec: FormatSpec, entry: &RecordIndexEntry) -> Result<()> {
    let unknown_flags = entry.flags & !RECORD_KNOWN_FLAGS;
    if unknown_flags != 0 {
        return Err(Error::UnknownRecordFlags(unknown_flags));
    }
    if !entry.is_compressed() {
        return Ok(());
    }
    if entry.flags & RECORD_FLAG_INTERNAL != 0 || entry.block_id >= RESERVED_BLOCK_ID_START {
        return Err(Error::InvalidCompressionHeader);
    }
    let descriptor = spec
        .block(entry.block_id)
        .ok_or(Error::UnregisteredBlock(entry.block_id))?;
    if descriptor.kind != BlockKind::Variable {
        return Err(Error::InvalidCompressionHeader);
    }
    let compression = variable_compression_for_block(spec, entry.block_id)
        .ok_or(Error::InvalidCompressionHeader)?;
    ensure_compression_algorithm_available(compression.algorithm)
}

fn file_header_extensions(spec: FormatSpec) -> Result<Vec<u8>> {
    let Some(compression) = variable_compression(spec) else {
        return Ok(Vec::new());
    };
    if compression.header_mode != CompressionHeaderMode::FileExplicit {
        return Ok(Vec::new());
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(FILE_COMPRESSION_MAGIC);
    payload.push(FILE_COMPRESSION_VERSION);
    payload.push(compression_algorithm_byte(compression.algorithm));
    payload.push(compression_level_kind_byte(compression.level));
    payload.push(u8::from(compression.only_if_smaller));
    payload.extend_from_slice(&compression_level_exact(compression.level).to_le_bytes());
    payload.extend_from_slice(&compression.min_uncompressed_len.to_le_bytes());
    payload.extend_from_slice(&compression.max_uncompressed_len.to_le_bytes());
    Ok(payload)
}

/// One block of the file-header extension region.
struct HeaderExtensionBlock<'a> {
    magic: [u8; 4],
    /// The block's bytes *including* its magic and any length prefix, so a
    /// known block is compared against what this spec would write without
    /// re-encoding the framing.
    encoded: &'a [u8],
}

/// Whether this build decodes a block, as opposed to stepping over it.
///
/// The whole point of the walk: a magic that is not on this list is skipped,
/// so a file carrying a block from a later release still opens here.
fn is_known_header_extension_magic(magic: &[u8; 4]) -> bool {
    magic == FILE_COMPRESSION_MAGIC
}

/// Walk the extension region as a block sequence.
///
/// The framing is `magic[4] | len: u32 | payload[len]`, with one exception:
/// `VCHD` shipped before the framing existed and has no length prefix, so it is
/// identified by magic and its payload length is the constant above. Every
/// block added after this walk carries the prefix, which is what makes an
/// unknown one skippable.
///
/// A truncated block, a length that overruns the region, or a repeated magic is
/// refused — skipping an unknown block is forward compatibility, but a region
/// that cannot be framed at all is a corrupt header.
fn parse_file_header_extension_blocks(region: &[u8]) -> Result<Vec<HeaderExtensionBlock<'_>>> {
    let mut blocks: Vec<HeaderExtensionBlock<'_>> = Vec::new();
    let mut offset = 0usize;
    while offset < region.len() {
        let start = offset;
        let magic: [u8; 4] = region
            .get(offset..offset + 4)
            .ok_or(Error::InvalidCompressionHeader)?
            .try_into()
            .map_err(|_| Error::InvalidCompressionHeader)?;
        offset += 4;
        let payload_len = if &magic == FILE_COMPRESSION_MAGIC {
            FILE_COMPRESSION_PAYLOAD_LEN
        } else {
            let len: [u8; 4] = region
                .get(offset..offset + 4)
                .ok_or(Error::InvalidCompressionHeader)?
                .try_into()
                .map_err(|_| Error::InvalidCompressionHeader)?;
            offset += 4;
            usize::try_from(u32::from_le_bytes(len)).map_err(|_| Error::InvalidCompressionHeader)?
        };
        offset = offset
            .checked_add(payload_len)
            .ok_or(Error::InvalidCompressionHeader)?;
        if offset > region.len() {
            return Err(Error::InvalidCompressionHeader);
        }
        if blocks.iter().any(|block| block.magic == magic) {
            return Err(Error::InvalidCompressionHeader);
        }
        blocks.push(HeaderExtensionBlock {
            magic,
            encoded: &region[start..offset],
        });
    }
    Ok(blocks)
}

/// Judge the region a file carries against the one this spec would write.
///
/// A block this build knows must be present exactly when it is expected and
/// must match byte for byte; a block it does not know is stepped over. The
/// equality fast path keeps the ordinary case — a file written by this build —
/// off the walk entirely.
///
/// This buys forward compatibility from the release that carries it, not
/// backward compatibility with the ones that do not: a reader older than this
/// walk still refuses any region it would not have written itself.
fn validate_file_header_extensions(spec: FormatSpec, extensions: &[u8]) -> Result<()> {
    let expected = file_header_extensions(spec)?;
    if expected == extensions {
        return Ok(());
    }
    let present = parse_file_header_extension_blocks(extensions)?;
    let expected_blocks = parse_file_header_extension_blocks(&expected)?;
    for expected_block in &expected_blocks {
        let found = present
            .iter()
            .find(|block| block.magic == expected_block.magic)
            .ok_or(Error::InvalidCompressionHeader)?;
        if found.encoded != expected_block.encoded {
            return Err(Error::InvalidCompressionHeader);
        }
    }
    for block in &present {
        if !is_known_header_extension_magic(&block.magic) {
            continue;
        }
        if !expected_blocks
            .iter()
            .any(|expected_block| expected_block.magic == block.magic)
        {
            return Err(Error::InvalidCompressionHeader);
        }
    }
    Ok(())
}

fn compression_algorithm_byte(algorithm: CompressionAlgorithm) -> u8 {
    match algorithm {
        CompressionAlgorithm::Zstd => 1,
    }
}

fn compression_algorithm_from_byte(value: u8) -> Result<CompressionAlgorithm> {
    match value {
        1 => Ok(CompressionAlgorithm::Zstd),
        other => Err(Error::UnsupportedCompressionAlgorithm(other)),
    }
}

fn compression_level_kind_byte(level: CompressionLevel) -> u8 {
    match level {
        CompressionLevel::Fast => 1,
        CompressionLevel::Default => 2,
        CompressionLevel::Best => 3,
        CompressionLevel::Exact(_) => 4,
    }
}

fn compression_level_exact(level: CompressionLevel) -> i32 {
    match level {
        CompressionLevel::Fast | CompressionLevel::Default | CompressionLevel::Best => 0,
        CompressionLevel::Exact(value) => value,
    }
}

fn compression_level_from_parts(kind: u8, exact: i32) -> Result<CompressionLevel> {
    match kind {
        1 => Ok(CompressionLevel::Fast),
        2 => Ok(CompressionLevel::Default),
        3 => Ok(CompressionLevel::Best),
        4 => Ok(CompressionLevel::Exact(exact)),
        _ => Err(Error::InvalidCompressionHeader),
    }
}

fn compression_header_mode_byte(mode: CompressionHeaderMode) -> u8 {
    match mode {
        CompressionHeaderMode::RecordExplicit => 1,
        CompressionHeaderMode::FileExplicit => 2,
        CompressionHeaderMode::FormatContract => 3,
    }
}

fn compression_header_mode_from_byte(value: u8) -> Result<CompressionHeaderMode> {
    match value {
        1 => Ok(CompressionHeaderMode::RecordExplicit),
        2 => Ok(CompressionHeaderMode::FileExplicit),
        3 => Ok(CompressionHeaderMode::FormatContract),
        _ => Err(Error::InvalidCompressionHeader),
    }
}

#[cfg(feature = "compression-zstd")]
fn ensure_compression_algorithm_available(algorithm: CompressionAlgorithm) -> Result<()> {
    match algorithm {
        CompressionAlgorithm::Zstd => Ok(()),
    }
}

#[cfg(not(feature = "compression-zstd"))]
fn ensure_compression_algorithm_available(_algorithm: CompressionAlgorithm) -> Result<()> {
    Err(Error::CompressionFeatureDisabled)
}

#[cfg(feature = "compression-zstd")]
fn compress_with_algorithm(
    algorithm: CompressionAlgorithm,
    level: CompressionLevel,
    payload: &[u8],
) -> Result<Vec<u8>> {
    match algorithm {
        CompressionAlgorithm::Zstd => {
            zstd::bulk::compress(payload, level.to_zstd_level()).map_err(Error::Io)
        }
    }
}

#[cfg(not(feature = "compression-zstd"))]
fn compress_with_algorithm(
    algorithm: CompressionAlgorithm,
    _level: CompressionLevel,
    _payload: &[u8],
) -> Result<Vec<u8>> {
    ensure_compression_algorithm_available(algorithm)?;
    unreachable!("compression algorithm availability returned Ok without a backend")
}

#[cfg(feature = "compression-zstd")]
fn decompress_with_algorithm(
    algorithm: CompressionAlgorithm,
    payload: &[u8],
    expected_len: u64,
) -> Result<Vec<u8>> {
    let capacity = usize::try_from(expected_len).map_err(|_| Error::LengthOverflow {
        value: expected_len,
    })?;
    match algorithm {
        CompressionAlgorithm::Zstd => zstd::bulk::decompress(payload, capacity).map_err(Error::Io),
    }
}

#[cfg(not(feature = "compression-zstd"))]
fn decompress_with_algorithm(
    algorithm: CompressionAlgorithm,
    _payload: &[u8],
    _expected_len: u64,
) -> Result<Vec<u8>> {
    ensure_compression_algorithm_available(algorithm)?;
    unreachable!("compression algorithm availability returned Ok without a backend")
}

/// Pre-flight cost estimate for the resident keyed merge/compact family
/// (PERF2-03).
///
/// # This is an estimate, not an upper bound (PERF4-04)
///
/// Every field is derived from record *counts* only: producing it decodes
/// nothing and retains no value, so a caller can size or refuse a merge before
/// paying for it. That cheapness is exactly why it cannot be a bound. Only
/// [`Self::input_records`], [`Self::key_bearing_records`] and
/// [`Self::max_distinct_keys`] are true upper bounds - they are counts. Every
/// *byte* field counts **inline structural storage only**:
/// `count * size_of::<...>()` for the container's own elements.
///
/// [`Self::peak_resident_structural_bytes`] therefore **excludes**, and the
/// real peak exceeds it by:
///
/// - the heap owned by individual `Key` and `T` values. A `String` key or a
///   `Vec` field is one pointer-sized triple in the map's entry array and an
///   arbitrary allocation outside it, which the count model cannot see. For
///   heap-owning types the shortfall is unbounded;
/// - `HashMap` load-factor slack, control bytes and power-of-two bucket
///   rounding: the map allocates strictly more than
///   `entries * size_of::<entry>()`;
/// - per-record decode scratch held while a value is being applied;
/// - allocator metadata and fragmentation.
///
/// It was previously published as an upper bound. It never was one, and a
/// sizing contract that understates by an unbounded amount is worse than one
/// that says plainly what it counts - so the method was renamed rather than
/// left to read as a guarantee. Callers that need a hard ceiling must bound
/// the operation instead of sizing it, with
/// [`merge_keyed_files_with_key_limit`], which refuses at a typed boundary.
///
/// The structural peak is the merge state plus the output vector - reserved
/// while the map and its values are still alive - plus the largest input's
/// resident index and the transient that input's open holds alongside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyedMergeEstimate {
    /// Records across all inputs. An exact count.
    pub input_records: u64,
    /// Records that can introduce a distinct key: values of `T`, tombstones,
    /// and merge ops. An exact count.
    pub key_bearing_records: u64,
    /// Upper bound on `K-ever`, the number of distinct keys the merge state
    /// retains. Tombstoned keys count: the collector keeps their entries.
    pub max_distinct_keys: u64,
    /// Resident index bytes for the largest single input, which is opened
    /// whole while its records are applied:
    /// `records * size_of::<RecordIndexEntry>()`.
    pub largest_input_index_bytes: u64,
    /// Transient bytes the largest single input's open can allocate *on top of*
    /// its resident index, and which are live at the same time as that index:
    /// the sequence-uniqueness witness copies one `u64` per record when the
    /// file's sequences are not already ascending in offset order (PERF3-05).
    pub largest_input_open_transient_bytes: u64,
    /// Structural bytes of the merge-state map's entries at
    /// [`Self::max_distinct_keys`] keys:
    /// `max_distinct_keys * size_of::<(Key, (MergeOrder, Option<T>))>()`.
    ///
    /// This is not a bound on the map: it excludes hash-table load-factor
    /// slack, control bytes and bucket rounding, and all heap owned by
    /// individual keys and values.
    pub max_state_bytes: u64,
    /// Structural bytes of the output vector, which is reserved for the
    /// surviving entries while the merge state and its values are still alive
    /// (PERF4-04): `max_distinct_keys * size_of::<(MergeOrder, T)>()`.
    ///
    /// Like [`Self::max_state_bytes`] this counts inline storage only.
    pub max_output_values_bytes: u64,
}

impl KeyedMergeEstimate {
    /// Structural bytes resident at the merge's peak. **Not an upper bound** -
    /// see the type-level documentation for exactly what it omits.
    ///
    /// The merge state lives for the whole operation while one input at a time
    /// is opened, and the output vector is reserved before the state is
    /// dropped, so this sums [`Self::max_state_bytes`],
    /// [`Self::max_output_values_bytes`], [`Self::largest_input_index_bytes`]
    /// and [`Self::largest_input_open_transient_bytes`]. Saturates instead of
    /// overflowing: a saturated estimate is still a refusal signal.
    #[must_use]
    pub fn peak_resident_structural_bytes(&self) -> u64 {
        self.max_state_bytes
            .saturating_add(self.max_output_values_bytes)
            .saturating_add(self.largest_input_index_bytes)
            .saturating_add(self.largest_input_open_transient_bytes)
    }
}

/// Estimates the resident cost of merging `base` with `deltas` for block `T`.
///
/// Opens each input read-only in turn and counts index entries; nothing is
/// decoded and no value is retained, so the peak cost of the estimate itself
/// is one input's resident index. Use it to decide whether
/// [`merge_keyed_files`] fits in available memory, or pass the result's
/// `max_distinct_keys` to [`merge_keyed_files_with_key_limit`].
///
/// The byte fields of the result are structural estimates and not upper
/// bounds; read [`KeyedMergeEstimate`] before treating any of them as a
/// ceiling.
pub fn estimate_keyed_merge<T, P>(
    spec: FormatSpec,
    base: P,
    deltas: &[P],
) -> Result<KeyedMergeEstimate>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let spec = spec.ordinary_read();
    spec.validate()?;
    crate::collections::ensure_registered_block::<T>(spec)?;
    let mut estimate = KeyedMergeEstimate::default();
    accumulate_keyed_merge_estimate::<T, _>(spec, base, &mut estimate)?;
    for delta in deltas {
        accumulate_keyed_merge_estimate::<T, _>(spec, delta, &mut estimate)?;
    }
    estimate.max_distinct_keys = estimate.key_bearing_records;
    let keys = usize::try_from(estimate.max_distinct_keys).unwrap_or(usize::MAX);
    estimate.max_state_bytes =
        allocation_bytes::<(T::Key, (MergeOrder, Option<T>))>(keys, "merge state")?;
    // PERF4-04: `collect_merged_keyed_values` reserves the output vector while
    // the state map and every surviving value are still alive, so the two
    // allocations overlap and both belong in the structural peak.
    estimate.max_output_values_bytes = allocation_bytes::<(MergeOrder, T)>(keys, "merged values")?;
    Ok(estimate)
}

fn accumulate_keyed_merge_estimate<T, P>(
    spec: FormatSpec,
    path: P,
    estimate: &mut KeyedMergeEstimate,
) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let file = VarveFile::open_readonly(spec, path)?;
    let records =
        u64::try_from(file.index.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "record count",
        })?;
    let key_bearing = file
        .index
        .iter()
        .filter(|entry| {
            entry.block_id == T::ID
                || entry.block_id == TOMBSTONE_BLOCK_ID
                || entry.block_id == OP_BLOCK_ID
        })
        .count();
    let key_bearing =
        u64::try_from(key_bearing).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "record count",
        })?;
    estimate.input_records = estimate.input_records.saturating_add(records);
    estimate.key_bearing_records = estimate.key_bearing_records.saturating_add(key_bearing);
    estimate.largest_input_index_bytes = estimate
        .largest_input_index_bytes
        .max(index_bytes_for_count(file.index.len())?);
    // PERF3-05: the open that produced `file` also validates sequence
    // uniqueness, which can copy every sequence into an `8N` temporary held
    // alongside the resident index. Charging it here keeps
    // `peak_resident_structural_bytes` from ignoring a live allocation it can
    // actually see.
    estimate.largest_input_open_transient_bytes = estimate
        .largest_input_open_transient_bytes
        .max(sequence_uniqueness_transient_bytes(file.index.len())?);
    Ok(())
}

/// Merges `base` with `deltas` into `output`, keeping the last write per key.
///
/// # Scale contract
///
/// This is a **resident** operation and is deliberately not PB-scale. It opens
/// each input as a whole [`VarveFile`] and accumulates one map entry per
/// distinct key ever seen - including keys whose latest record is a tombstone -
/// plus the live values that survive to the output:
///
/// - time: `Theta(records + decoded bytes) + O(N log N) + O(K-live log K-live)`,
///   where the `O(N log N)` term is the per-input open's sequence-uniqueness
///   sort over that input's `N` records. It degrades to `Theta(N)` for the
///   ordinary case of a file whose sequences ascend with offset, but the sort
///   is the guaranteed bound (PERF3-05);
/// - memory: `O(K-ever + largest resident input index + 8N uniqueness
///   temporary for that input + retained live values)`, where `K-ever` is the
///   number of distinct keys across all inputs.
///
/// Nothing here spills to disk, so `K-ever` must fit in memory. Varve exports
/// no bounded-memory external merge/compact; the scalable stream and indexed
/// writers cover bounded *ingest*, not bounded merge. Callers whose key
/// cardinality is not known to be resident-sized should size the operation
/// first with [`estimate_keyed_merge`] or bound it with
/// [`merge_keyed_files_with_key_limit`], which fails with a typed limit error
/// instead of exhausting memory.
pub fn merge_keyed_files<T, P>(spec: FormatSpec, base: P, deltas: &[P], output: P) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    merge_keyed_files_with_key_limit::<T, P>(spec, base, deltas, output, u64::MAX)
}

/// [`merge_keyed_files`] with an explicit ceiling on distinct retained keys.
///
/// The collector checks `max_distinct_keys` before it admits each new key, so
/// an input whose cardinality exceeds the caller's memory budget fails with
/// [`Error::LimitExceeded`] (`resource = "merge distinct keys"`) at the
/// boundary instead of being discovered by the allocator. The ceiling counts
/// tombstoned keys, matching what the state actually retains.
pub fn merge_keyed_files_with_key_limit<T, P>(
    spec: FormatSpec,
    base: P,
    deltas: &[P],
    output: P,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let spec = spec.ordinary_read();
    spec.validate()?;
    let final_values = collect_merged_keyed_values::<T, P>(spec, base, deltas, max_distinct_keys)?;
    write_keyed_values_atomically(
        spec,
        output.as_ref(),
        final_values.into_iter().map(|(_, value)| value),
    )
}

/// Compacts `base` plus `deltas` into `output`, dropping superseded records.
///
/// # Scale contract
///
/// Identical to [`merge_keyed_files`]: resident, `O(K-ever + largest resident
/// input index + 8N uniqueness temporary for that input + retained live
/// values)` memory, not PB-scale. See that
/// function for the full bound, [`estimate_keyed_merge`] for a pre-flight
/// estimate, and [`compact_keyed_files_with_key_limit`] for a typed guard.
pub fn compact_keyed_files<T, P>(spec: FormatSpec, base: P, deltas: &[P], output: P) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    compact_keyed_files_with_key_limit::<T, P>(spec, base, deltas, output, u64::MAX)
}

/// [`compact_keyed_files`] with an explicit ceiling on distinct retained keys.
///
/// See [`merge_keyed_files_with_key_limit`] for the guard semantics.
pub fn compact_keyed_files_with_key_limit<T, P>(
    spec: FormatSpec,
    base: P,
    deltas: &[P],
    output: P,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let spec = spec.ordinary_read();
    spec.validate()?;
    let final_values = collect_merged_keyed_values::<T, P>(spec, base, deltas, max_distinct_keys)?;
    write_keyed_values_atomically(
        spec,
        output.as_ref(),
        final_values.into_iter().map(|(_, value)| value),
    )
}

fn collect_merged_keyed_values<T, P>(
    spec: FormatSpec,
    base: P,
    deltas: &[P],
    max_distinct_keys: u64,
) -> Result<Vec<(MergeOrder, T)>>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    crate::collections::ensure_registered_block::<T>(spec)?;
    let mut state: HashMap<T::Key, (MergeOrder, Option<T>)> = HashMap::new();
    let mut budget = MaterializationBudget::new(spec);
    apply_merge_file::<T, _>(
        spec,
        base,
        MergeShard { ordinal: 0 },
        &mut state,
        &mut budget,
        max_distinct_keys,
    )?;
    for (index, delta) in deltas.iter().enumerate() {
        apply_merge_file::<T, _>(
            spec,
            delta,
            MergeShard { ordinal: index + 1 },
            &mut state,
            &mut budget,
            max_distinct_keys,
        )?;
    }

    let requested = allocation_bytes::<(MergeOrder, T)>(state.len(), "merged values")?;
    let mut final_values = Vec::new();
    final_values
        .try_reserve_exact(state.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "merged values",
            requested,
        })?;
    final_values.extend(
        state
            .into_values()
            .filter_map(|(order, value)| value.map(|value| (order, value))),
    );
    final_values.sort_by_key(|(order, _)| *order);
    Ok(final_values)
}

fn write_keyed_values_atomically<T, I>(spec: FormatSpec, output: &Path, values: I) -> Result<()>
where
    T: VarveBlock,
    I: IntoIterator<Item = T>,
{
    let _lock = WriterLock::acquire(output)?;
    // STO4-P2: the permission copy below is fallible and used to sit between
    // the temp's creation and the first scope that cleaned it up, so its `?`
    // could leave an empty temp behind. The guard owns the temp from creation,
    // so *every* early return from here on unlinks it; the deliberate
    // retentions below disarm it explicitly.
    let mut temp = RewriteTemp::create(output)?;
    take_injected_rewrite_temp_preparation_failure()?;
    if let Ok(metadata) = std::fs::metadata(output) {
        temp.file().set_permissions(metadata.permissions())?;
    }
    temp.close();
    let temp_path = temp.path().to_path_buf();

    let result = (|| {
        let mut out = VarveFile::create(spec, &temp_path)?;
        for value in values {
            // API2-05: `values` comes from a per-key map, so the merged
            // generation holds exactly one record per surviving key and an
            // absent keyed predecessor is the correct link. Stating it
            // explicitly keeps this off the generic push, which refuses keyed
            // blocks precisely because it cannot know that.
            out.push_with_prev_key_info(&value, None)?;
        }
        out.flush()?;
        out.sync()
    })();
    if let Err(error) = result {
        // Delete eagerly rather than at drop: the marker cleanup below is only
        // correct once the temp itself is gone.
        temp.cleanup_now();
        remove_rewrite_temp_lock_marker(&temp_path);
        return Err(error);
    }

    let published = match replace_path_atomically(&temp_path, output) {
        // The rename consumed the temp pathname; there is nothing left to
        // unlink and the guard must not try.
        Ok(ReplaceDurability::Durable) => {
            temp.retain();
            Ok(())
        }
        // Publication already happened; the temp file no longer exists and the
        // target pathname resolves to the merged generation, so the caller
        // must not treat this as "target unchanged".
        Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
            temp.retain();
            Err(Error::PublishedButParentSyncPending {
                path: output.display().to_string(),
                source: Box::new(sync_error),
            })
        }
        Err(error) => {
            if matches!(error, Error::ReplacePublicationIndeterminate { .. }) {
                // DUR2-01: the temp may be the only surviving copy of the new
                // generation. Preserving it is the whole point of this arm, so
                // the guard is disarmed rather than allowed to unlink it.
                temp.retain();
            } else {
                temp.cleanup_now();
            }
            Err(error)
        }
    };
    remove_rewrite_temp_lock_marker(&temp_path);
    published
}

/// Removes the writer-lock marker that an internal rewrite temp left behind
/// (STO3-01).
///
/// Opening the temp through [`VarveFile::create`] creates `<temp>.lock`.
/// Dropping that writer clears the marker's *contents* but deliberately keeps
/// the file: for a real user path the marker is a stable identity, and
/// unlinking it while another process may already hold the OS lock on that
/// inode would let a third process create a fresh marker at the same pathname
/// and "acquire" a lock on a different inode - exactly the race the persistent
/// marker exists to prevent. None of that applies to this pathname:
///
/// - it was generated by [`create_rewrite_temp_file`] with `create_new`, so
///   this operation exclusively owns it;
/// - the only way to regenerate the same name is another rewrite of the same
///   output in the same process, and the caller holds the output's writer lock
///   across the whole publication, so no such rewrite can be in flight;
/// - by the time this runs the temp has been published or deleted, so the
///   marker names a file that no longer exists.
///
/// The caller must therefore have dropped the temp's `VarveFile` (releasing its
/// [`WriterLock`]) and must still hold the target's writer lock. If the temp is
/// still present - the indeterminate-publication case, where it is preserved
/// for out-of-band reconciliation - its marker is left with it.
fn remove_rewrite_temp_lock_marker(temp_path: &Path) {
    if temp_path.exists() {
        return;
    }
    let _ = remove_file(lock_path(temp_path));
}

/// Deletes a publication temp file after a pre-publication failure, but
/// preserves it after [`Error::ReplacePublicationIndeterminate`]: in that
/// state the temp may be the only surviving copy of the new generation and is
/// required for out-of-band reconciliation (DUR2-01).
fn preserve_temp_on_indeterminate(temp_path: &Path, error: Error) -> Error {
    if !matches!(error, Error::ReplacePublicationIndeterminate { .. }) {
        let _ = remove_file(temp_path);
    }
    error
}

fn apply_merge_file<T, P>(
    spec: FormatSpec,
    path: P,
    shard: MergeShard,
    state: &mut HashMap<T::Key, (MergeOrder, Option<T>)>,
    budget: &mut MaterializationBudget,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let file = VarveFile::open_readonly(spec, path)?;
    apply_merge_entries::<T>(
        spec,
        &file.snapshot,
        &file.index,
        shard,
        state,
        budget,
        max_distinct_keys,
    )
}

/// Refuses a key the merge state has not seen once the caller's `K-ever`
/// ceiling is reached (PERF2-03), so a cardinality the caller cannot afford
/// fails typed at the boundary instead of in the allocator.
fn admit_merge_key<T>(
    state: &HashMap<T::Key, (MergeOrder, Option<T>)>,
    key: &T::Key,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
{
    if state.contains_key(key) {
        return Ok(());
    }
    let admitted = u64::try_from(state.len()).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "merge distinct keys",
    })?;
    if admitted >= max_distinct_keys {
        return Err(Error::LimitExceeded {
            resource: "merge distinct keys",
            actual: admitted.saturating_add(1),
            limit: max_distinct_keys,
        });
    }
    Ok(())
}

fn apply_merge_entries<T>(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    entries: &ResidentIndex,
    shard: MergeShard,
    state: &mut HashMap<T::Key, (MergeOrder, Option<T>)>,
    budget: &mut MaterializationBudget,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
{
    for (record_ordinal, entry) in entries.iter().enumerate() {
        let order = MergeOrder::for_record(shard.ordinal, entry.sequence, record_ordinal);
        match entry.block_id {
            id if id == T::ID => {
                if entry.block_version != T::VERSION {
                    return Err(Error::BlockVersionMismatch {
                        block_id: T::ID,
                        expected: T::VERSION,
                        actual: entry.block_version,
                    });
                }
                let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
                budget.consume(logical_len)?;
                let payload = entry.read_logical_payload_snapshot(spec, snapshot)?;
                let block: T = budget.decode(&payload, T::ENDIAN.unwrap_or(spec.endian))?;
                let key = block.key();
                if should_apply(state.get(&key), order) {
                    admit_merge_key::<T>(state, &key, max_distinct_keys)?;
                    state.try_reserve(1).map_err(|_| Error::AllocationFailed {
                        resource: "merge state",
                        requested: logical_len,
                    })?;
                    state.insert(key, (order, Some(block)));
                }
            }
            TOMBSTONE_BLOCK_ID => {
                let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
                budget.consume(logical_len)?;
                let payload = entry.read_payload_snapshot(spec, snapshot)?;
                let Some(key) = decode_internal_key_payload::<T>(spec.endian, &payload, budget)?
                else {
                    continue;
                };
                if should_apply(state.get(&key), order) {
                    admit_merge_key::<T>(state, &key, max_distinct_keys)?;
                    state.try_reserve(1).map_err(|_| Error::AllocationFailed {
                        resource: "merge state",
                        requested: logical_len,
                    })?;
                    state.insert(key, (order, None));
                }
            }
            OP_BLOCK_ID => {
                let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
                budget.consume(logical_len)?;
                let payload = entry.read_payload_snapshot(spec, snapshot)?;
                let Some((key, op)) =
                    decode_internal_op_payload::<T>(spec.endian, &payload, budget)?
                else {
                    continue;
                };
                if !should_apply(state.get(&key), order) {
                    continue;
                }
                let Some((_, Some(mut value))) = state.remove(&key) else {
                    return Err(Error::MissingMergeTarget);
                };
                value.apply_op(op)?;
                state.insert(key, (order, Some(value)));
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MergeShard {
    ordinal: usize,
}

impl MergeShard {
    fn single_file() -> Self {
        Self { ordinal: 0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct MergeOrder {
    shard_ordinal: usize,
    sequence: u64,
    record_ordinal: usize,
}

impl MergeOrder {
    const fn for_record(shard_ordinal: usize, sequence: u64, record_ordinal: usize) -> Self {
        Self {
            shard_ordinal,
            sequence,
            record_ordinal,
        }
    }
}

fn should_apply<T>(current: Option<&(MergeOrder, Option<T>)>, order: MergeOrder) -> bool {
    current.is_none_or(|(old_order, _)| order >= *old_order)
}

/// Frames a metadata record's payload as `(key, value)` without copying either.
///
/// [`VarveFile::write_metadata`] encodes `(String, Vec<u8>)` through the tuple
/// codec, which is exactly `u64 key_len | key | u64 value_len | value`, and
/// metadata goes through `write_record` rather than `write_user_record`, so it
/// is never compressed. That framing is what the borrowed pair reads back.
///
/// It exists so a lookup can compare the key *before* deciding to materialize
/// the value: the decoder allocates a `String` and a `Vec<u8>` per record,
/// which for a lookup is one copy per record that is thrown away. The errors
/// are the ones `decode_complete` raises on the same bytes —
/// [`Error::UnexpectedEof`], [`Error::LengthOverflow`], [`Error::InvalidUtf8`]
/// and [`Error::TrailingBytes`] — because the byte-for-byte answer must not
/// depend on which of the two readers looked.
///
/// The `0` materialization allowance is not a limit being enforced: nothing is
/// materialized here, so nothing is charged. Callers charge the record they
/// then decode.
fn split_metadata_payload(payload: &[u8], endian: Endian) -> Result<(&str, &[u8])> {
    let mut decoder = crate::Decoder::new_limited(payload, endian, 0);
    let key_len = decoder.read_len()?;
    if key_len > decoder.remaining() {
        return Err(Error::UnexpectedEof);
    }
    let key = decoder.read_exact(key_len)?;
    let key = std::str::from_utf8(key).map_err(|_| Error::InvalidUtf8)?;
    let value_len = decoder.read_len()?;
    if value_len > decoder.remaining() {
        return Err(Error::UnexpectedEof);
    }
    let value = decoder.read_exact(value_len)?;
    if decoder.remaining() != 0 {
        return Err(Error::TrailingBytes {
            remaining: decoder.remaining(),
        });
    }
    Ok((key, value))
}

/// Encodes a writer-supplied value with the format's logical-payload limit as
/// the encode hard bound (DEF-02).
///
/// Every resident writer entry point routes its value encoding through this
/// bound so an oversized value fails with the typed limit error while the
/// encoder stops buffering at the limit, instead of materializing an
/// arbitrarily large encoding first and only then failing the limit check.
fn encode_logical_payload_limited<T: VarveEncode>(
    spec: FormatSpec,
    value: &T,
    endian: Endian,
) -> Result<Vec<u8>> {
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    encode_to_vec_limited(
        value,
        endian,
        logical_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )
}

fn encode_internal_key_payload<T>(spec: FormatSpec, key: &T::Key) -> Result<Vec<u8>>
where
    T: VarveKeyedBlock,
{
    const ENVELOPE_LEN: u64 = 12;
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    let key_limit = logical_limit
        .checked_sub(ENVELOPE_LEN)
        .ok_or(Error::LimitExceeded {
            resource: ReadLimitKey::LogicalPayloadLen.resource(),
            actual: ENVELOPE_LEN,
            limit: logical_limit,
        })?;
    let key_payload = encode_to_vec_limited(
        key,
        spec.endian,
        key_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )?;
    let total_len = ENVELOPE_LEN.checked_add(key_payload.len() as u64).ok_or(
        Error::ResourceArithmeticOverflow {
            resource: "internal key payload length",
        },
    )?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, total_len)?;
    let total_len_usize =
        usize::try_from(total_len).map_err(|_| Error::LengthOverflow { value: total_len })?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(total_len_usize)
        .map_err(|_| Error::AllocationFailed {
            resource: "internal key payload",
            requested: total_len,
        })?;
    payload.extend_from_slice(&T::ID.to_le_bytes());
    payload.extend_from_slice(&(key_payload.len() as u64).to_le_bytes());
    payload.extend_from_slice(&key_payload);
    Ok(payload)
}

fn decode_internal_key_payload<T>(
    endian: Endian,
    payload: &[u8],
    budget: &mut MaterializationBudget,
) -> Result<Option<T::Key>>
where
    T: VarveKeyedBlock,
{
    let Some(key) = decode_native_internal_key_envelope(payload, T::ID)? else {
        return Ok(None);
    };
    Ok(Some(budget.decode(key, endian)?))
}

#[cfg(feature = "high-cardinality-dev")]
pub(crate) fn decode_stream_tombstone_key<T: VarveKeyedBlock>(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    entry: &RecordIndexEntry,
) -> Result<Option<T::Key>> {
    if entry.block_id != TOMBSTONE_BLOCK_ID {
        return Ok(None);
    }
    if entry.block_version != 1 || entry.flags != RECORD_FLAG_INTERNAL {
        return Err(Error::CorruptTail {
            offset: entry.record_offset,
        });
    }
    let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
    let mut budget = MaterializationBudget::new(spec);
    budget.consume(logical_len)?;
    let payload = entry.read_logical_payload_snapshot(spec, snapshot)?;
    decode_internal_key_payload::<T>(spec.endian, &payload, &mut budget)
}

fn encode_internal_op_payload<T>(spec: FormatSpec, key: &T::Key, op: &T::Op) -> Result<Vec<u8>>
where
    T: VarveMerge,
{
    const ENVELOPE_LEN: u64 = 20;
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    let key_limit = logical_limit
        .checked_sub(ENVELOPE_LEN)
        .ok_or(Error::LimitExceeded {
            resource: ReadLimitKey::LogicalPayloadLen.resource(),
            actual: ENVELOPE_LEN,
            limit: logical_limit,
        })?;
    let key_payload = encode_to_vec_limited(
        key,
        spec.endian,
        key_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )?;
    // DEF-02: the op encoder inherits whatever budget the key left over, so
    // the combined encoding can never buffer past the logical payload limit.
    let op_limit = key_limit - key_payload.len() as u64;
    let op_payload = encode_to_vec_limited(
        op,
        spec.endian,
        op_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )?;
    let total_len = ENVELOPE_LEN
        .checked_add(key_payload.len() as u64)
        .and_then(|len| len.checked_add(op_payload.len() as u64))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "internal op payload length",
        })?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, total_len)?;
    let total_len_usize =
        usize::try_from(total_len).map_err(|_| Error::LengthOverflow { value: total_len })?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(total_len_usize)
        .map_err(|_| Error::AllocationFailed {
            resource: "internal op payload",
            requested: total_len,
        })?;
    payload.extend_from_slice(&T::ID.to_le_bytes());
    payload.extend_from_slice(&(key_payload.len() as u64).to_le_bytes());
    payload.extend_from_slice(&key_payload);
    payload.extend_from_slice(&(op_payload.len() as u64).to_le_bytes());
    payload.extend_from_slice(&op_payload);
    Ok(payload)
}

fn decode_internal_op_payload<T>(
    endian: Endian,
    payload: &[u8],
    budget: &mut MaterializationBudget,
) -> Result<Option<(T::Key, T::Op)>>
where
    T: VarveMerge,
{
    let Some((key, op)) = decode_native_internal_op_envelope(payload, T::ID)? else {
        return Ok(None);
    };
    let key = budget.decode(key, endian)?;
    let op = budget.decode(op, endian)?;
    Ok(Some((key, op)))
}

/// Write the header this spec describes, and hand back the extension region it
/// put there so the caller can record what this file actually carries.
pub(crate) fn write_file_header(spec: FormatSpec, file: &mut File) -> Result<Vec<u8>> {
    let extensions = file_header_extensions(spec)?;
    write_native_file_header(file, spec, &extensions)?;
    Ok(extensions)
}

pub(crate) fn read_file_header(spec: FormatSpec, file: &mut File) -> Result<u64> {
    Ok(read_file_header_parts(spec, file)?.0)
}

/// The header length *and* the extension region the file carries.
///
/// The two are not interchangeable with what `spec` would produce: once an
/// unknown block is skippable rather than refused, a file's region can be
/// longer than this build would write, and every offset derived from the header
/// has to come from the file rather than from the spec.
pub(crate) fn read_file_header_parts(spec: FormatSpec, file: &mut File) -> Result<(u64, Vec<u8>)> {
    file.seek(SeekFrom::Start(0))?;
    let header = read_native_file_header(file, spec)?;
    let hash = header.schema_hash;
    if spec.schema_hash != 0 && hash != spec.schema_hash {
        return Err(Error::SchemaHashMismatch {
            expected: spec.schema_hash,
            actual: hash,
        });
    }
    if uses_format_contract_compression(spec) {
        let computed = spec.computed_schema_hash();
        if hash != computed {
            return Err(Error::SchemaHashMismatch {
                expected: computed,
                actual: hash,
            });
        }
    }
    if header.has_extension_len {
        validate_file_header_extensions(spec, &header.extensions)?;
        Ok((header.header_len, header.extensions))
    } else {
        if uses_file_explicit_compression(spec) {
            return Err(Error::InvalidCompressionHeader);
        }
        Ok((native_file_header_len(spec, 0), Vec::new()))
    }
}

fn read_matrix_layout_if_needed(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    captured_len: u64,
) -> Result<Option<(crate::matrix::MatrixLayout, [u8; MATRIX_CREATION_NONCE_LEN])>> {
    if spec.has_matrix_blocks() {
        // The creation nonce region sits between the native file header and
        // the matrix layout header (DUR2-03).
        let creation_nonce = read_matrix_creation_nonce_region(file, header_len)?;
        let layout_start = header_len
            .checked_add(MATRIX_CREATION_NONCE_REGION_LEN as u64)
            .ok_or(Error::InvalidMatrixLayout)?;
        let layout = crate::matrix::read_layout_at_len(spec, file, layout_start, captured_len)?;
        Ok(Some((layout, creation_nonce)))
    } else {
        Ok(None)
    }
}

#[allow(clippy::type_complexity)]
fn split_matrix_state(
    state: Option<(crate::matrix::MatrixLayout, [u8; MATRIX_CREATION_NONCE_LEN])>,
) -> (
    Option<crate::matrix::MatrixLayout>,
    Option<[u8; MATRIX_CREATION_NONCE_LEN]>,
) {
    match state {
        Some((layout, nonce)) => (Some(layout), Some(nonce)),
        None => (None, None),
    }
}

fn append_log_start(header_len: u64, matrix: Option<&crate::matrix::MatrixLayout>) -> u64 {
    matrix.map_or(header_len, crate::matrix::MatrixLayout::append_log_start)
}

/// Produces a fresh 128-bit matrix creation nonce (DUR2-03).
///
/// This is a uniqueness value, not a cryptographic secret: it only has to
/// differ between two `create_with_dims` calls that reuse the same OS file
/// object. Wall-clock nanoseconds, the process id and a per-process counter
/// are mixed through FNV-1a into two independent 64-bit lanes.
fn fresh_matrix_creation_nonce() -> [u8; MATRIX_CREATION_NONCE_LEN] {
    use std::sync::atomic::{AtomicU64, Ordering};

    static CREATION_COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = CREATION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let pid = u64::from(std::process::id());

    const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
    const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
    let mut nonce = [0u8; MATRIX_CREATION_NONCE_LEN];
    for lane in 0u64..2 {
        let mut hash = FNV_OFFSET_BASIS;
        for word in [lane, nanos as u64, (nanos >> 64) as u64, pid, counter] {
            for byte in word.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
        }
        let lane = lane as usize;
        nonce[lane * 8..(lane + 1) * 8].copy_from_slice(&hash.to_le_bytes());
    }
    // The raw entropy words survive the mix: XOR them back in so even a
    // degenerate clock (nanos == 0 on every call) cannot collapse two creates
    // in the same process to the same nonce (the counter still differs).
    for (index, byte) in nanos
        .to_le_bytes()
        .iter()
        .chain(counter.to_le_bytes().iter())
        .take(MATRIX_CREATION_NONCE_LEN)
        .enumerate()
    {
        nonce[index] ^= byte.rotate_left((index % 7) as u32);
    }
    nonce
}

fn write_matrix_creation_nonce_region(
    file: &mut File,
    nonce: [u8; MATRIX_CREATION_NONCE_LEN],
) -> Result<()> {
    let mut region = [0u8; MATRIX_CREATION_NONCE_REGION_LEN];
    region[0..4].copy_from_slice(MATRIX_CREATION_NONCE_MAGIC);
    region[4..6].copy_from_slice(&MATRIX_CREATION_NONCE_VERSION.to_le_bytes());
    // region[6..8] stays zero (reserved).
    region[8..].copy_from_slice(&nonce);
    file.write_all(&region)?;
    Ok(())
}

fn read_matrix_creation_nonce_region(
    file: &mut File,
    offset: u64,
) -> Result<[u8; MATRIX_CREATION_NONCE_LEN]> {
    file.seek(SeekFrom::Start(offset))?;
    let mut region = [0u8; MATRIX_CREATION_NONCE_REGION_LEN];
    file.read_exact(&mut region)?;
    if &region[0..4] != MATRIX_CREATION_NONCE_MAGIC
        || region[4..6] != MATRIX_CREATION_NONCE_VERSION.to_le_bytes()
        || region[6..8] != [0; 2]
    {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut nonce = [0u8; MATRIX_CREATION_NONCE_LEN];
    nonce.copy_from_slice(&region[8..]);
    Ok(nonce)
}

pub(crate) fn ensure_native_write_limits(spec: FormatSpec) -> Result<()> {
    for key in [
        ReadLimitKey::FileLen,
        ReadLimitKey::Records,
        ReadLimitKey::IndexBytes,
        ReadLimitKey::RecordPayloadLen,
        ReadLimitKey::LogicalPayloadLen,
    ] {
        spec.read_limits.require(key)?;
    }
    Ok(())
}

pub(crate) fn ensure_native_open_limits(spec: FormatSpec) -> Result<()> {
    ensure_native_write_limits(spec)?;
    spec.read_limits.require(ReadLimitKey::ScanBytes)?;
    Ok(())
}

pub(crate) fn check_initial_native_file_len(spec: FormatSpec) -> Result<()> {
    let extension_len = u64::try_from(file_header_extensions(spec)?.len()).map_err(|_| {
        Error::ResourceArithmeticOverflow {
            resource: "file length",
        }
    })?;
    let mut header_len = native_file_header_len(spec, extension_len);
    if spec.has_matrix_blocks() {
        header_len = header_len
            .checked_add(MATRIX_CREATION_NONCE_REGION_LEN as u64)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "file length",
            })?;
    }
    spec.read_limits.check(ReadLimitKey::FileLen, header_len)
}

pub(crate) fn check_open_file_len(_spec: FormatSpec, file: &File) -> Result<u64> {
    Ok(file.metadata()?.len())
}

fn ensure_matrix_byte_copy_compatible<From, To>() -> Result<()>
where
    From: VarveMatrixBlock,
    To: VarveMatrixBlock,
{
    if From::DIMENSIONS != To::DIMENSIONS || From::SLOT_STRIDE != To::SLOT_STRIDE {
        return Err(Error::InvalidFormatSpec(
            "matrix byte-copy requires matching dimensions and slot_stride",
        ));
    }
    Ok(())
}

fn record_footer_len(spec: FormatSpec) -> u64 {
    if spec.spec_needs_record_footer() {
        native_record_footer_len()
    } else {
        0
    }
}

fn nonzero_offset(offset: u64) -> Option<u64> {
    if offset == 0 { None } else { Some(offset) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanIntent {
    ReadOnly,
    Writer,
    Recover,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecoverableTail {
    offset: u64,
    truncate_to: u64,
}

#[derive(Debug)]
enum RecordRead {
    Entry(RecordIndexEntry),
    RecoverableTail(RecoverableTail),
}

std::thread_local! {
    /// Bytes this thread has advanced over while scanning records, all opens
    /// summed. The single charge point for `ReadLimitKey::ScanBytes` is
    /// `ScanAccounting::advance`, so this is every record byte an open walks —
    /// and the number that separates an open that follows the on-disk index
    /// from one that walks the file.
    static OPEN_SCAN_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[inline]
fn count_open_scan_bytes(bytes: u64) {
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    OPEN_SCAN_BYTES.with(|total| total.set(total.get().saturating_add(bytes)));
    #[cfg(not(any(test, feature = "scalable-fault-injection")))]
    let _ = bytes;
}

/// Reads and clears this thread's scanned-record byte total.
#[cfg(any(test, feature = "scalable-fault-injection"))]
pub(crate) fn take_open_scan_bytes() -> u64 {
    OPEN_SCAN_BYTES.with(|total| total.replace(0))
}

#[derive(Clone, Copy, Debug, Default)]
struct ScanAccounting {
    advanced: u64,
}

impl ScanAccounting {
    fn advance(&mut self, spec: FormatSpec, bytes: u64) -> Result<()> {
        let advanced =
            self.advanced
                .checked_add(bytes)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "scan bytes",
                })?;
        spec.read_limits.check(ReadLimitKey::ScanBytes, advanced)?;
        count_open_scan_bytes(bytes);
        self.advanced = advanced;
        Ok(())
    }
}

/// What a single record read is allowed to tolerate, and what it must check.
///
/// One value rather than three parameters because the three are one decision:
/// how this particular walk treats a record it cannot fully validate. Splitting
/// them let a caller pass a recovery boundary while silently skipping the check
/// that produces the evidence for it.
#[derive(Clone, Copy, Debug)]
struct ScanChecks {
    /// Offset a torn record may be truncated back to, if tolerated.
    partial_boundary: Option<u64>,
    /// Offset a checksum mismatch may be truncated back to, if tolerated.
    checksum_boundary: Option<u64>,
    /// Whether the stored checksum is recomputed from the payload here.
    ///
    /// False only on the open path under `IntegrityVerification::OnDemand`,
    /// where the identical check runs on the read that returns the record.
    verify_checksums: bool,
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    static RECORDS_FRAMED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CHUNK_BYTES_READ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts bytes read to answer a chunked cell read on this thread. Inert
/// without the `scalable-fault-injection` feature.
#[inline]
fn note_chunk_bytes_read(bytes: u64) {
    #[cfg(feature = "scalable-fault-injection")]
    CHUNK_BYTES_READ.with(|read| read.set(read.get().saturating_add(bytes)));
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = bytes;
}

/// Counts records framed while building an index on this thread. Inert without
/// the `scalable-fault-injection` feature.
#[inline]
fn note_record_framed() {
    #[cfg(feature = "scalable-fault-injection")]
    RECORDS_FRAMED.with(|framed| framed.set(framed.get().saturating_add(1)));
}

#[cfg(any(test, feature = "scalable-fault-injection"))]
std::thread_local! {
    /// Entry comparisons this thread has made looking up a rewritten record's
    /// chain predecessor (`find_rewritten_by_offset`).
    ///
    /// Fault-testing hook only. This is the unit the front-to-back scan made
    /// quadratic: a `replace_block` over N records probed ~N^2/2 times and now
    /// probes at most `N * ceil(log2 N)`. A regression test asserts the count,
    /// not the wall clock.
    static REPLACEMENT_PREDECESSOR_PROBES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one predecessor-lookup comparison on this thread. Inert outside tests
/// and without the `scalable-fault-injection` feature.
#[inline]
fn note_replacement_predecessor_probe() {
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    REPLACEMENT_PREDECESSOR_PROBES.with(|probes| probes.set(probes.get().saturating_add(1)));
}

fn read_record_entry_at(
    spec: FormatSpec,
    file: &mut File,
    file_len: u64,
    offset: u64,
    checks: ScanChecks,
    accounting: &mut ScanAccounting,
) -> Result<RecordRead> {
    note_record_framed();
    let ScanChecks {
        partial_boundary,
        checksum_boundary,
        verify_checksums,
    } = checks;
    let remaining = file_len
        .checked_sub(offset)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "scan offset",
        })?;
    if remaining < RECORD_HEADER_LEN {
        accounting.advance(spec, remaining)?;
        return Ok(RecordRead::RecoverableTail(RecoverableTail {
            offset,
            truncate_to: offset,
        }));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut entry = read_record_header(file, spec, offset)?;
    validate_record_entry(spec, &entry)?;
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, entry.payload_len)?;
    if !entry.is_compressed() {
        spec.read_limits
            .check(ReadLimitKey::LogicalPayloadLen, entry.payload_len)?;
    }
    let payload_end = entry.payload_offset.checked_add(entry.payload_len).ok_or(
        Error::ResourceArithmeticOverflow {
            resource: "record extent",
        },
    )?;
    let record_end = payload_end.checked_add(record_footer_len(spec)).ok_or(
        Error::ResourceArithmeticOverflow {
            resource: "record extent",
        },
    )?;
    let record_len = record_end
        .checked_sub(offset)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "record extent",
        })?;
    accounting.advance(spec, record_len)?;
    if record_end > file_len {
        let Some(truncate_to) = partial_boundary else {
            return Err(Error::CorruptTail { offset });
        };
        return Ok(RecordRead::RecoverableTail(RecoverableTail {
            offset,
            truncate_to,
        }));
    }

    let logical_len = entry.logical_payload_len_before_allocation(spec, file)?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;

    let footer = if spec.spec_needs_record_footer() {
        let footer_offset = payload_end;
        let footer_bytes = read_record_footer_bytes(file, footer_offset)?;
        let footer = decode_record_footer(&footer_bytes, footer_offset, entry.record_offset)?;
        entry.footer_offset = Some(footer_offset);
        entry.prev_same_block_offset = footer.prev_same_block_offset;
        entry.prev_same_key_offset = footer.prev_same_key_offset;
        Some(footer_bytes)
    } else {
        None
    };

    // SCAN-01: this is the single line that made open read the whole file. The
    // check itself is not removed - it is *relocated* to the read that returns
    // the record, where `read_payload_snapshot` already performs the identical
    // one on every typed read. See `IntegrityVerification` for why that is a
    // choice about when damage is announced rather than whether bytes are
    // checked, and for the one caller that must never take this branch off:
    // recovery, whose truncation evidence *is* the mismatch.
    if verify_checksums
        && matches!(
            spec.integrity_policy,
            IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader
        )
    {
        let expected = checksum_record_file(
            spec,
            file,
            &entry,
            footer.as_ref().map(|bytes| &bytes[..]).unwrap_or(&[]),
        )?;
        if expected != entry.checksum {
            if let Some(truncate_to) = checksum_boundary {
                return Ok(RecordRead::RecoverableTail(RecoverableTail {
                    offset,
                    truncate_to,
                }));
            }
            return Err(Error::ChecksumMismatch { offset });
        }
    }

    if entry.block_id == COMMIT_BLOCK_ID {
        if entry.payload_len != COMMIT_PAYLOAD_MAGIC.len() as u64 {
            return Err(Error::InvalidCommitMarker { offset });
        }
        let mut payload = [0; COMMIT_PAYLOAD_MAGIC.len()];
        file.seek(SeekFrom::Start(entry.payload_offset))?;
        file.read_exact(&mut payload)?;
        if &payload != COMMIT_PAYLOAD_MAGIC {
            return Err(Error::InvalidCommitMarker { offset });
        }
    }

    Ok(RecordRead::Entry(entry))
}

#[cfg(feature = "high-cardinality-dev")]
#[derive(Debug)]
pub(crate) struct NativeStreamScanner {
    spec: FormatSpec,
    file: File,
    snapshot: SnapshotFile,
    offset: u64,
    accounting: ScanAccounting,
    records: u64,
    previous_sequence: Option<u64>,
}

#[cfg(all(test, feature = "high-cardinality-dev"))]
thread_local! {
    static STREAM_SCANNER_CONSTRUCTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static STREAM_SCANNER_ENTRIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static STREAM_POINT_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(all(test, feature = "high-cardinality-dev"))]
pub(crate) fn reset_stream_io_counters() {
    STREAM_SCANNER_CONSTRUCTIONS.set(0);
    STREAM_SCANNER_ENTRIES.set(0);
    STREAM_POINT_READS.set(0);
}

#[cfg(all(test, feature = "high-cardinality-dev"))]
pub(crate) fn stream_io_counters() -> (u64, u64, u64) {
    (
        STREAM_SCANNER_CONSTRUCTIONS.get(),
        STREAM_SCANNER_ENTRIES.get(),
        STREAM_POINT_READS.get(),
    )
}

#[cfg(feature = "high-cardinality-dev")]
impl NativeStreamScanner {
    pub(crate) fn from_snapshot(spec: FormatSpec, snapshot: SnapshotFile) -> Result<Self> {
        let mut file = snapshot.try_clone_file()?;
        let header_len = read_file_header(spec, &mut file)?;
        let mut accounting = ScanAccounting::default();
        accounting.advance(spec, header_len)?;
        #[cfg(test)]
        STREAM_SCANNER_CONSTRUCTIONS.set(STREAM_SCANNER_CONSTRUCTIONS.get() + 1);
        Ok(Self {
            spec,
            file,
            snapshot,
            offset: header_len,
            accounting,
            records: 0,
            previous_sequence: None,
        })
    }

    pub(crate) fn next_entry(&mut self) -> Result<Option<RecordIndexEntry>> {
        if self.offset == self.snapshot.len() {
            return Ok(None);
        }
        #[cfg(test)]
        STREAM_SCANNER_ENTRIES.set(STREAM_SCANNER_ENTRIES.get() + 1);
        let partial_boundary = self.spec.spec_needs_record_footer().then_some(self.offset);
        let entry = match read_record_entry_at(
            self.spec,
            &mut self.file,
            self.snapshot.len(),
            self.offset,
            // An explicit scan is a caller asking to walk records; it is not
            // the open path this policy exists to shorten.
            ScanChecks {
                partial_boundary,
                checksum_boundary: None,
                verify_checksums: true,
            },
            &mut self.accounting,
        )? {
            RecordRead::Entry(entry) => entry,
            RecordRead::RecoverableTail(tail) => {
                self.offset = tail.truncate_to;
                return Ok(None);
            }
        };
        if matches!(
            entry.block_id,
            OP_BLOCK_ID | INDEX_BLOCK_ID | COMMIT_BLOCK_ID | SEGMENT_BLOCK_ID
        ) {
            return Err(Error::StreamingUnsupported);
        }
        if self
            .previous_sequence
            .is_some_and(|previous| entry.sequence <= previous)
        {
            return Err(Error::StreamingUnsupported);
        }
        self.records = self
            .records
            .checked_add(1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Records, self.records)?;
        self.previous_sequence = Some(entry.sequence);
        self.offset = entry.checked_physical_end()?;
        Ok(Some(entry))
    }

    pub(crate) const fn logical_eof(&self) -> u64 {
        self.offset
    }

    pub(crate) fn snapshot(&self) -> &SnapshotFile {
        &self.snapshot
    }

    pub(crate) const fn spec(&self) -> FormatSpec {
        self.spec
    }
}

#[cfg(feature = "high-cardinality-dev")]
pub(crate) fn read_stream_entry_at(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    offset: u64,
    physical_len: u64,
) -> Result<RecordIndexEntry> {
    let validated = snapshot.validate(
        crate::scalable_extent::UntrustedRecordPointer::new(offset, physical_len),
        spec.spec_needs_record_footer(),
    )?;
    let span = validated.span();
    #[cfg(test)]
    STREAM_POINT_READS.set(STREAM_POINT_READS.get() + 1);
    let mut file = snapshot.try_clone_file()?;
    let entry = read_stream_entry_at_file(spec, &mut file, snapshot, offset)?;
    if entry.record_offset != span.record_offset().get()
        || entry.payload_offset != span.payload_offset().get()
        || entry.payload_len != span.payload_len().get()
        || entry.checked_physical_end()? != span.end().get()
    {
        return Err(Error::InvalidIndexCheckpoint);
    }
    Ok(entry)
}

#[cfg(feature = "high-cardinality-dev")]
pub(crate) fn read_stream_entry_at_file(
    spec: FormatSpec,
    file: &mut File,
    snapshot: &SnapshotFile,
    offset: u64,
) -> Result<RecordIndexEntry> {
    let mut accounting = ScanAccounting::default();
    let partial_boundary = spec.spec_needs_record_footer().then_some(offset);
    match read_record_entry_at(
        spec,
        file,
        snapshot.len(),
        offset,
        // A single record read by offset: the caller named this record, so the
        // check belongs here and costs one payload, not a file.
        ScanChecks {
            partial_boundary,
            checksum_boundary: None,
            verify_checksums: true,
        },
        &mut accounting,
    )? {
        RecordRead::Entry(entry) => Ok(entry),
        RecordRead::RecoverableTail(_) => Err(Error::CorruptTail { offset }),
    }
}

#[cfg(feature = "high-cardinality-dev")]
#[derive(Debug)]
pub(crate) struct PreparedStreamRecord {
    pub(crate) bytes: Vec<u8>,
    pub(crate) info: AppendInfo,
    pub(crate) block_id: u32,
    pub(crate) block_version: u16,
    pub(crate) flags: u16,
    pub(crate) checksum: u32,
}

#[cfg(feature = "high-cardinality-dev")]
/// What [`prepare_stream_record_into`] knows about the record it just appended
/// to the caller's buffer.
///
/// The same fields as [`PreparedStreamRecord`] with the buffer taken out, plus
/// `len` — because a caller staging several records into one buffer needs to
/// know where this one ended, and `bytes.len()` is the answer it no longer has.
pub(crate) struct PreparedRecordParts {
    pub(crate) len: usize,
    pub(crate) info: AppendInfo,
    pub(crate) block_id: u32,
    pub(crate) block_version: u16,
    pub(crate) flags: u16,
    pub(crate) checksum: u32,
}

#[cfg(feature = "high-cardinality-dev")]
impl PreparedStreamRecord {
    /// The same record described as [`PreparedRecordParts`], for the callers
    /// that still take the owning form.
    pub(crate) fn parts(&self) -> PreparedRecordParts {
        PreparedRecordParts {
            len: self.bytes.len(),
            info: self.info,
            block_id: self.block_id,
            block_version: self.block_version,
            flags: self.flags,
            checksum: self.checksum,
        }
    }
}

/// The owning form, for the callers that want a record and nothing else: the
/// creation nonce, the manifest, a tombstone, the probe. None of them is a hot
/// path, and each allocates exactly the buffer it hands back.
#[cfg(feature = "high-cardinality-dev")]
#[allow(clippy::too_many_arguments)]
fn prepare_stream_record(
    spec: FormatSpec,
    block_id: u32,
    block_version: u16,
    flags: u16,
    uncompressed_len_hint: u32,
    payload: &[u8],
    sequence: u64,
    record_offset: u64,
    prev_same_block_offset: Option<u64>,
    prev_same_key_offset: Option<u64>,
) -> Result<PreparedStreamRecord> {
    let mut bytes = Vec::new();
    let parts = prepare_stream_record_into(
        spec,
        block_id,
        block_version,
        flags,
        uncompressed_len_hint,
        payload,
        sequence,
        record_offset,
        prev_same_block_offset,
        prev_same_key_offset,
        &mut bytes,
    )?;
    debug_assert_eq!(bytes.len(), parts.len);
    Ok(PreparedStreamRecord {
        bytes,
        info: parts.info,
        block_id: parts.block_id,
        block_version: parts.block_version,
        flags: parts.flags,
        checksum: parts.checksum,
    })
}

/// [`prepare_stream_user_record`], staging into the caller's buffer.
///
/// The single-record push path holds one buffer for the life of the writer and
/// clears it per record; the batch path appends record after record into the
/// chunk it is about to write. Both used to take a fresh `Vec` per record from
/// the owning form below, and the batch path then copied it into the chunk
/// buffer and dropped it.
#[cfg(feature = "high-cardinality-dev")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_stream_user_record_into<T: VarveBlock>(
    spec: FormatSpec,
    value: &T,
    sequence: u64,
    record_offset: u64,
    prev_same_block_offset: Option<u64>,
    prev_same_key_offset: Option<u64>,
    out: &mut Vec<u8>,
) -> Result<PreparedRecordParts> {
    let endian = T::ENDIAN.unwrap_or(spec.endian);
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    let logical = encode_to_vec_limited(
        value,
        endian,
        logical_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )?;
    let stored = prepare_user_record_payload(spec, T::ID, T::KIND, &logical)?;
    prepare_stream_record_into(
        spec,
        T::ID,
        T::VERSION,
        stored.flags,
        stored.uncompressed_len_hint,
        &stored.bytes,
        sequence,
        record_offset,
        prev_same_block_offset,
        prev_same_key_offset,
        out,
    )
}

/// The owning form. Only the in-crate probe and the unit tests below want a
/// record they can hold; every production path stages through
/// [`prepare_stream_user_record_into`].
#[cfg(all(test, feature = "high-cardinality-dev"))]
pub(crate) fn prepare_stream_user_record<T: VarveBlock>(
    spec: FormatSpec,
    value: &T,
    sequence: u64,
    record_offset: u64,
    prev_same_block_offset: Option<u64>,
    prev_same_key_offset: Option<u64>,
) -> Result<PreparedStreamRecord> {
    let endian = T::ENDIAN.unwrap_or(spec.endian);
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    let logical = encode_to_vec_limited(
        value,
        endian,
        logical_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )?;
    let stored = prepare_user_record_payload(spec, T::ID, T::KIND, &logical)?;
    prepare_stream_record(
        spec,
        T::ID,
        T::VERSION,
        stored.flags,
        stored.uncompressed_len_hint,
        &stored.bytes,
        sequence,
        record_offset,
        prev_same_block_offset,
        prev_same_key_offset,
    )
}

#[cfg(feature = "high-cardinality-dev")]
pub(crate) fn prepare_stream_tombstone_record<T: VarveKeyedBlock>(
    spec: FormatSpec,
    key: &T::Key,
    sequence: u64,
    record_offset: u64,
    prev_same_block_offset: Option<u64>,
    prev_same_key_offset: Option<u64>,
) -> Result<PreparedStreamRecord> {
    let payload = encode_internal_key_payload::<T>(spec, key)?;
    prepare_stream_record(
        spec,
        TOMBSTONE_BLOCK_ID,
        1,
        RECORD_FLAG_INTERNAL,
        0,
        &payload,
        sequence,
        record_offset,
        prev_same_block_offset,
        prev_same_key_offset,
    )
}

#[cfg(feature = "high-cardinality-dev")]
pub(crate) fn prepare_stream_manifest_record(
    spec: FormatSpec,
    sequence: u64,
    record_offset: u64,
) -> Result<PreparedStreamRecord> {
    let payload = encode_schema_manifest(spec)?;
    prepare_stream_record(
        spec,
        MANIFEST_BLOCK_ID,
        1,
        RECORD_FLAG_INTERNAL,
        0,
        &payload,
        sequence,
        record_offset,
        None,
        None,
    )
}

/// Produces a fresh 128-bit creation nonce for a stream/indexed primary.
///
/// Same uniqueness-not-secrecy contract as the matrix creation nonce: it only
/// has to differ between two `create` calls, including two that reuse the same
/// OS file object.
#[cfg(feature = "high-cardinality-dev")]
pub(crate) fn fresh_stream_creation_nonce() -> [u8; MATRIX_CREATION_NONCE_LEN] {
    fresh_matrix_creation_nonce()
}

/// Frames the creation-nonce record (STO-01).
///
/// The nonce lives *inside the primary* because that is the only place an
/// equal-length in-place rewrite cannot preserve: a nonce in the sidecar, the
/// writer-lock file or any companion file survives the rewrite and detects
/// nothing. It is written exactly once, at create, as the first record of the
/// log, so it costs nothing per append.
///
/// The payload is the bare 16-byte nonce under a dedicated reserved block id
/// rather than a keyed metadata envelope. That keeps the record at the smallest
/// possible size, so it fits inside any payload limit that admits a 16-byte
/// user record and needs no exemption from the caller's read limits.
#[cfg(feature = "high-cardinality-dev")]
pub(crate) fn prepare_stream_creation_nonce_record(
    spec: FormatSpec,
    nonce: [u8; MATRIX_CREATION_NONCE_LEN],
    sequence: u64,
    record_offset: u64,
) -> Result<PreparedStreamRecord> {
    prepare_stream_record(
        spec,
        CREATION_NONCE_BLOCK_ID,
        1,
        RECORD_FLAG_INTERNAL,
        0,
        &nonce,
        sequence,
        record_offset,
        None,
        None,
    )
}

/// Recovers the creation nonce of a stream/indexed primary, or `None` when the
/// primary carries none (a legacy primary, or one bootstrapped from a resident
/// `VarveFile`).
///
/// Cost is one bounded point read of the first record, performed only at open
/// and at create ??never on an append or lookup path.
///
/// Any failure to read or decode the leading record is reported as "no nonce"
/// rather than as an error. That is fail-closed, not permissive: a primary that
/// *was* created with a nonce recorded a fingerprint that folds it in, so
/// answering `None` can only ever produce a *different* fingerprint and refuse
/// the sidecar. It must never turn a damaged leading record into an open-time
/// error on a path whose job is to compute an identity.
#[cfg(feature = "high-cardinality-dev")]
pub(crate) fn read_stream_creation_nonce(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    header_len: u64,
) -> Option<[u8; MATRIX_CREATION_NONCE_LEN]> {
    if header_len >= snapshot.len() {
        return None;
    }
    let mut file = snapshot.try_clone_file().ok()?;
    let entry = read_stream_entry_at_file(spec, &mut file, snapshot, header_len).ok()?;
    if entry.block_id != CREATION_NONCE_BLOCK_ID
        || entry.flags & RECORD_FLAG_INTERNAL == 0
        || entry.block_version != 1
        || entry.payload_len != MATRIX_CREATION_NONCE_LEN as u64
    {
        return None;
    }
    let payload = entry.read_payload_snapshot(spec, snapshot).ok()?;
    <[u8; MATRIX_CREATION_NONCE_LEN]>::try_from(payload.as_slice()).ok()
}

#[cfg(feature = "high-cardinality-dev")]
/// Appends one encoded record to `out` and returns what the writer needs to
/// know about it.
///
/// **Appends rather than replaces**, which is what lets the batch path stage a
/// whole chunk through one buffer: each record lands directly where it will be
/// written from, instead of into a per-record `Vec` that is copied into the
/// chunk and dropped.
///
/// This used to build that per-record `Vec` itself — `try_reserve_exact` plus
/// three `extend_from_slice` — so continuous append allocated once per record,
/// on the path the project's design policy names as sacred.
#[allow(clippy::too_many_arguments)]
fn prepare_stream_record_into(
    spec: FormatSpec,
    block_id: u32,
    block_version: u16,
    flags: u16,
    uncompressed_len_hint: u32,
    payload: &[u8],
    sequence: u64,
    record_offset: u64,
    prev_same_block_offset: Option<u64>,
    prev_same_key_offset: Option<u64>,
    out: &mut Vec<u8>,
) -> Result<PreparedRecordParts> {
    let prev_same_block_offset = spec
        .index_policy
        .block_offset_chain
        .then_some(prev_same_block_offset)
        .flatten();
    let prev_same_key_offset = spec
        .index_policy
        .keyed_offset_chain
        .then_some(prev_same_key_offset)
        .flatten();
    let payload_len =
        u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    let logical_len =
        record_logical_len_from_parts(spec, block_id, flags, uncompressed_len_hint, payload)?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;
    let payload_offset =
        record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "file length",
            })?;
    let payload_end =
        payload_offset
            .checked_add(payload_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "file length",
            })?;
    let encoded_footer = if spec.spec_needs_record_footer() {
        Some(encode_record_footer(RecordFooterFields {
            prev_same_block_offset,
            prev_same_key_offset,
        })?)
    } else {
        None
    };
    let footer = encoded_footer.as_ref().map_or(&[][..], |bytes| &bytes[..]);
    let _record_end =
        payload_end
            .checked_add(footer.len() as u64)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "file length",
            })?;
    let header = RecordHeaderFields {
        block_id,
        block_version,
        flags,
        sequence,
        payload_len,
        checksum: 0,
        uncompressed_len_hint,
    };
    let checksum = checksum_record_fields(spec, record_offset, header, payload, footer)?;
    let header = encode_native_record_header(
        RecordHeaderFields { checksum, ..header },
        record_offset,
        record_footer_len(spec),
    )?;
    let total_len = header
        .len()
        .checked_add(payload.len())
        .and_then(|len| len.checked_add(footer.len()))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "record buffer length",
        })?;
    // `try_reserve` and not `try_reserve_exact`: `out` is the caller's buffer
    // and outlives this record, so exact-fitting it to one record is what would
    // make the next record reallocate. A fresh `Vec` reserves exactly `total_len`
    // on its first record either way, because `try_reserve` on an empty `Vec`
    // asks the allocator for what was requested.
    out.try_reserve(total_len)
        .map_err(|_| Error::AllocationFailed {
            resource: "record buffer",
            requested: total_len as u64,
        })?;
    out.extend_from_slice(&header);
    out.extend_from_slice(payload);
    out.extend_from_slice(footer);
    Ok(PreparedRecordParts {
        len: total_len,
        block_id,
        block_version,
        flags,
        checksum,
        info: AppendInfo {
            sequence,
            record_offset,
            payload_offset,
            payload_len,
            footer_offset: (!footer.is_empty()).then_some(payload_end),
            prev_same_block_offset,
            prev_same_key_offset,
            committed: true,
        },
    })
}

/// Whether this open may take the segment chain instead of the record scan.
///
/// Two exclusions, and both are about what the scan does that the walk does
/// not. A **recovery** scan verifies every record and truncates on the
/// mismatch; that evidence is the point of the open, and a walk that reads no
/// data record cannot produce it. [`IntegrityVerification::AtOpen`] asks for
/// the same verification of every record for the same reason. Under either,
/// the walk would be answering a cheaper question than the one asked.
/// What an open recovers from the file, beyond the resident index itself.
///
/// The index is filtered — a non-resident block's records are on disk and not
/// in it — so two things that used to be derived *from* the index cannot be any
/// more, and are carried out of the walk instead.
#[derive(Debug)]
struct ScannedIndex {
    entries: Vec<RecordIndexEntry>,
    /// The highest sequence any record in the file carries, resident or not.
    ///
    /// `SequenceState::from_index` took the maximum in the resident index. With
    /// a non-resident block that maximum is stale, and the writer would re-issue
    /// sequence numbers already on disk — silently, and for exactly the block
    /// the option exists to serve. Sequences are file-global and gapless, so
    /// the high-water mark has to come from every record.
    sequence_high_water: Option<u64>,
    /// The high-water mark as of the newest commit marker.
    ///
    /// A writer open discards everything after that marker, so those sequences
    /// go with it and may be re-issued — which is what the pre-H1 behaviour did
    /// by taking the maximum of the already-truncated index. Every non-resident
    /// survivor sits at or before the marker, so this bounds them all; the one
    /// record that survives *past* the marker is the trailing segment, and it
    /// is resident, so the final entry list covers it.
    sequence_high_water_at_commit: Option<u64>,
    /// The tails as of the newest commit marker, for the same reason.
    ///
    /// A tail left pointing into the truncated region makes the next append
    /// write a footer naming a record past the end of the file, and the reopen
    /// after that fails with `InvalidRecordFooter`.
    newest_at_commit: HashMap<u32, u64>,
    /// Newest record offset per block id, resident or not.
    ///
    /// `BlockTails::from_index` cannot answer this any more: it would drop the
    /// tail of a non-resident block, and that tail is the only way back to its
    /// records. Collected in a map and ordered exactly once at the end, because
    /// a per-record `note_appended` reintroduces the `Theta(B^2)` sorted-vector
    /// insertion term PERF3-03 removed.
    newest: HashMap<u32, u64>,
    /// Where the last record the walk accepted physically ends, resident or
    /// not.
    ///
    /// The fourth thing derived from a filtered list, and the one that did not
    /// get this treatment until now. `open_readonly` used to bind its snapshot
    /// to the last *resident* entry's end, so under a markerless policy that
    /// ends on a non-resident block the snapshot stopped before every record of
    /// that block — not merely before the tail. `block_chain` is the published
    /// way to reach a non-resident block and it walks through the snapshot, so
    /// the whole chain became unreachable through a read-only handle.
    ///
    /// It is deliberately NOT `file.metadata()?.len()`: a torn trailing record
    /// must still leave the snapshot at the end of the last *complete* record,
    /// which is a fact only the scan has.
    physical_end: Option<u64>,
    /// The same, as of the newest commit marker.
    ///
    /// A marker format discards everything after that marker, and a read-only
    /// handle must not expose records a writer open would delete. Uncommitted
    /// non-resident records past the marker are dropped with it, which is what
    /// stops this from becoming "the physical end of the file".
    physical_end_at_commit: Option<u64>,
}

impl ScannedIndex {
    /// Records one record's contribution to the parts that outlive filtering.
    ///
    /// `physical_end` is where this record ends, which both call sites already
    /// hold: the segment walk computes it one statement earlier as the offset
    /// the next record must begin at, and the scan assigns it to `offset` five
    /// lines earlier for the same reason. Neither pays a new computation.
    fn note(&mut self, entry: &RecordIndexEntry, physical_end: u64) {
        self.sequence_high_water = Some(match self.sequence_high_water {
            Some(seen) => seen.max(entry.sequence),
            None => entry.sequence,
        });
        self.newest.insert(entry.block_id, entry.record_offset);
        self.physical_end = Some(physical_end);
        if entry.block_id == COMMIT_BLOCK_ID {
            self.sequence_high_water_at_commit = self.sequence_high_water;
            self.physical_end_at_commit = self.physical_end;
            // O(B) and allocation-free after the first marker: the keys are
            // already there, only the values move.
            self.newest_at_commit.clear();
            self.newest_at_commit
                .extend(self.newest.iter().map(|(id, offset)| (*id, *offset)));
        }
    }

    /// Drops the recovered state to what survives a commit-boundary truncation.
    ///
    /// `entries` is the surviving resident list. Everything non-resident that
    /// survives sits at or before the newest commit marker, so the snapshots
    /// taken there bound it; the one record that survives *past* the marker is
    /// the trailing segment, which is resident and therefore in `entries`.
    fn commit_boundary_applied(&mut self, entries: &[RecordIndexEntry]) -> Result<()> {
        let surviving = entries.iter().map(|entry| entry.sequence).max();
        self.sequence_high_water = match (self.sequence_high_water_at_commit, surviving) {
            (Some(at_commit), Some(resident)) => Some(at_commit.max(resident)),
            (at_commit, resident) => at_commit.or(resident),
        };
        // `max`, not "take the marker's end": `committed_prefix_len`
        // deliberately keeps one trailing SEGMENT_BLOCK_ID record past the
        // marker, and that record's bytes are inside the boundary too.
        let surviving_end = entries
            .last()
            .map(RecordIndexEntry::checked_physical_end)
            .transpose()?;
        self.physical_end = match (self.physical_end_at_commit, surviving_end) {
            (Some(at_commit), Some(resident)) => Some(at_commit.max(resident)),
            (at_commit, resident) => at_commit.or(resident),
        };
        self.newest.clear();
        self.newest.extend(
            self.newest_at_commit
                .iter()
                .map(|(id, offset)| (*id, *offset)),
        );
        for entry in entries {
            self.newest.insert(entry.block_id, entry.record_offset);
        }
        Ok(())
    }

    /// Orders the collected tails exactly once (PERF3-03).
    fn block_tails(&self) -> BlockTails {
        BlockTails::from_newest(&self.newest)
    }

    fn sequence_state(&self) -> SequenceState {
        match self.sequence_high_water {
            None => SequenceState::Available(0),
            Some(u64::MAX) => SequenceState::Exhausted,
            Some(sequence) => SequenceState::Available(sequence + 1),
        }
    }

    /// The byte range a handle opened on this scan may read.
    ///
    /// `append_start` is the answer for a file whose append log the walk found
    /// empty — the header is all there is.
    fn physical_end(&self, append_start: u64) -> u64 {
        self.physical_end.unwrap_or(append_start)
    }
}

/// Whether this open may try the segment chain instead of the record scan.
///
/// **`segment_on_flush` is deliberately not consulted.** It is the *writer's*
/// policy — whether flushing appends a segment — and asking it here made the
/// reader consult its own configuration instead of the file in front of it: a
/// file written with a chain, opened by a spec that does not declare one, had
/// its index ignored and was walked header-to-EOF. The chain is a property of
/// the bytes, so the bytes decide. `load_index_from_segments` already answers
/// `Ok(None)` for a file whose chain does not describe it, and
/// `read_segment_tip_offset` costs one seek and a 40-byte read to find that
/// out, so a file without a chain pays that and falls back to the scan.
///
/// The other two conditions are about what the scan does that the walk does
/// not, and they stay: a recovery open exists to look at every record, and
/// `AtOpen` verification is a promise to checksum every record at open.
fn segment_chain_open_is_allowed(spec: FormatSpec, intent: ScanIntent) -> bool {
    let _ = spec.index_policy.segment_on_flush;
    intent != ScanIntent::Recover
        && spec
            .read_limits
            .resolve()
            .effective_integrity_verification()
            != IntegrityVerification::AtOpen
}

fn load_index(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    intent: ScanIntent,
) -> Result<ScannedIndex> {
    if segment_chain_open_is_allowed(spec, intent)
        && let Some(scanned) = load_index_from_segments(spec, file, header_len)?
    {
        return Ok(scanned);
    }
    // Sequence uniqueness is validated exactly once, inside
    // `scan_records_from`, on the complete scanned entry list *before* any
    // commit-boundary truncation; a truncated prefix of a duplicate-free list
    // is still duplicate-free, so revalidating here would only repeat the
    // N-element copy+sort on every open (PERF2-07).
    scan_records_from(spec, file, header_len, intent)
}

/// Rebuilds the resident index from the internal segment chain, or reports
/// that this file's chain does not describe it.
///
/// `Ok(None)` is the ordinary answer for a file the chain cannot account for:
/// one written before segments were enabled, one whose writer appended past
/// its last commit point, one truncated mid-record, one whose chain is stale.
/// Every one of those is answered by the full scan, which is what open did
/// before and is never an error.
///
/// **A resource refusal is one of them, and the first version of this got that
/// backwards.** It propagated `LimitExceeded` and its siblings, reasoning that
/// quietly doing more work is how a ceiling gets bypassed. Falling back
/// bypasses nothing: the scan builds the identical index through the same
/// `reserve_scanned_entry` and charges `Records`, `IndexBytes`, `ScanBytes` and
/// `RecordPayloadLen` itself, so it either opens inside the limits or refuses
/// honestly. Propagating cost two real failures. A writer never charges
/// `Segments`, so past `max_segments` commit points it built a file its own
/// reader refused while a scan read it perfectly — 65,537 flushes under
/// `ReadLimits::UNTRUSTED`. And `read_segment_tip_offset` returns an
/// *unconfirmed* offset, so the framing read charges `RecordPayloadLen` against
/// whatever `payload_len` happens to sit there: four single-bit flips in the
/// eight trailer bytes at `file_len - 40` turned a scannable file into a hard
/// `LimitExceeded`.
///
/// Only a *spec-level* refusal still propagates: `MissingResourceLimit` and
/// `TrustedUnboundedRequiresExplicitApi` are statements about the caller's
/// configuration, not about these bytes, and the scan answers them the same
/// way.
fn load_index_from_segments(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
) -> Result<Option<ScannedIndex>> {
    match walk_segment_chain(spec, file, append_start) {
        Ok(scanned) => Ok(Some(scanned)),
        // Only a *spec-level* refusal propagates: the format declared no
        // ceiling, or demanded the explicit unbounded API. Those describe the
        // caller's configuration and are the same answer the scan would give.
        Err(error @ Error::MissingResourceLimit { .. })
        | Err(error @ Error::TrustedUnboundedRequiresExplicitApi { .. }) => Err(error),
        Err(_) => Ok(None),
    }
}

/// Finds the newest segment record from the end of the file.
///
/// The last 32 bytes are a record footer if this file ends in a record at all,
/// and the eight before them are that record's payload trailer if that record
/// is a segment. Neither alone is proof — the footer carries no block id and
/// the trailer is only eight bytes of payload — so the offset this returns is a
/// candidate that the caller confirms by reading the header there.
fn read_segment_tip_offset(file: &mut File, append_start: u64, file_len: u64) -> Result<u64> {
    let probe_len = SEGMENT_TRAILER_LEN + RECORD_FOOTER_LEN;
    let minimum = RECORD_HEADER_LEN + SEGMENT_PREFIX_LEN + probe_len;
    if file_len
        .checked_sub(append_start)
        .is_none_or(|available| available < minimum)
    {
        return Err(Error::InvalidIndexSegment);
    }
    let probe_offset = file_len - probe_len;
    let mut probe = [0u8; (SEGMENT_TRAILER_LEN + RECORD_FOOTER_LEN) as usize];
    file.seek(SeekFrom::Start(probe_offset))?;
    file.read_exact(&mut probe)?;
    let trailer_len = SEGMENT_TRAILER_LEN as usize;
    if &probe[trailer_len..trailer_len + 4] != RECORD_FOOTER_MAGIC {
        return Err(Error::InvalidIndexSegment);
    }
    let mut version = [0; 2];
    version.copy_from_slice(&probe[trailer_len + 4..trailer_len + 6]);
    if u16::from_le_bytes(version) != RECORD_FOOTER_VERSION {
        return Err(Error::InvalidIndexSegment);
    }
    let mut trailer = [0; SEGMENT_TRAILER_LEN as usize];
    trailer.copy_from_slice(&probe[..trailer_len]);
    let record_offset = u64::from_le_bytes(trailer);
    if record_offset < append_start || record_offset > probe_offset - SEGMENT_PREFIX_LEN {
        return Err(Error::InvalidIndexSegment);
    }
    Ok(record_offset)
}

/// Walks the segment chain backwards from the end of the file and materializes
/// the index it describes.
///
/// Every link is checked against the file it claims to describe rather than
/// trusted: the record at each offset must be a segment record, its extent must
/// end exactly where the following link said its coverage began, and the
/// entries it carries must tile its coverage with no gap and no overlap. The
/// oldest link must reach the start of the append log. A chain that fails any
/// of these describes some other file, and the caller falls back to the scan.
///
/// Termination is structural: each link's predecessor offset must be strictly
/// smaller and no smaller than `append_start`, so the walk is bounded by the
/// segments in the file whatever the bytes say.
fn walk_segment_chain(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
) -> Result<ScannedIndex> {
    let file_len = file.metadata()?.len();
    let mut accounting = ScanAccounting::default();
    accounting.advance(spec, append_start)?;

    // **Pass one carries the links, not the segments they describe.** This used
    // to collect `(RecordIndexEntry, DecodedSegment)` per link, and a
    // `DecodedSegment` owns the entries it describes — so the whole file's index
    // was materialised here and then materialised again into `scanned.entries`
    // below, two live copies at peak.
    //
    // A `RecordIndexEntry` per link is bounded by the segment count, which is
    // what `ReadLimitKey::Segments` already bounds; the entry arrays are what
    // followed the record count. So the links stay and the segments go: pass one
    // reads each link's fixed `SEGMENT_PREFIX_LEN` prefix for the one number it
    // needs to check that the links join up, and pass two reads the entry array
    // one segment at a time.
    //
    // Keeping the entry also keeps the accounting honest: each segment record's
    // header is read and charged exactly once, as before.
    let mut links: Vec<RecordIndexEntry> = Vec::new();
    let mut next = Some(read_segment_tip_offset(file, append_start, file_len)?);
    // What the link being read must end at: the end of the file for the tip,
    // and the coverage start of its successor for every link behind it.
    let mut expected_end = file_len;
    while let Some(record_offset) = next {
        let entry = match read_record_entry_at(
            spec,
            file,
            file_len,
            record_offset,
            // A chain link is either wholly there or it is not a link. There is
            // no recoverable tail here and no boundary to truncate back to: the
            // scan owns that decision, and this walk defers to it by failing.
            ScanChecks {
                partial_boundary: None,
                checksum_boundary: None,
                verify_checksums: true,
            },
            &mut accounting,
        )? {
            RecordRead::Entry(entry) => entry,
            RecordRead::RecoverableTail(_) => return Err(Error::InvalidIndexSegment),
        };
        if entry.block_id != SEGMENT_BLOCK_ID
            || entry.block_version != SEGMENT_VERSION
            || entry.flags != RECORD_FLAG_INTERNAL
            || entry.checked_physical_end()? != expected_end
        {
            return Err(Error::InvalidIndexSegment);
        }
        let prefix = entry.read_payload_prefix_file_with_len(file, file_len, SEGMENT_PREFIX_LEN)?;
        let prefix = decode_segment_prefix(&prefix)?;
        if prefix.covered_start < append_start || prefix.covered_start > record_offset {
            return Err(Error::InvalidIndexSegment);
        }
        expected_end = prefix.covered_start;
        next = match entry.prev_same_block_offset {
            Some(previous) if previous >= record_offset || previous < append_start => {
                return Err(Error::InvalidIndexSegment);
            }
            previous => previous,
        };
        let count = u64::try_from(links.len())
            .ok()
            .and_then(|links| links.checked_add(1))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "segment count",
            })?;
        spec.read_limits.check(ReadLimitKey::Segments, count)?;
        links.try_reserve(1).map_err(|_| Error::AllocationFailed {
            resource: "segment chain",
            requested: count,
        })?;
        links.push(entry);
    }
    // The oldest link must reach the append log, or records written before the
    // chain began are outside every segment and would be lost.
    if expected_end != append_start {
        return Err(Error::InvalidIndexSegment);
    }

    let mut scanned = ScannedIndex {
        entries: Vec::new(),
        sequence_high_water: None,
        sequence_high_water_at_commit: None,
        newest: HashMap::new(),
        newest_at_commit: HashMap::new(),
        physical_end: None,
        physical_end_at_commit: None,
    };
    // **Pass two, oldest link first, one segment live at a time.** Each link is
    // re-read and decoded here and its entries are moved straight into
    // `scanned`, so the peak is one segment plus the index being built rather
    // than the index twice.
    let mut running = append_start;
    for link in links.into_iter().rev() {
        let payload = link.read_payload_file_with_len(file, file_len)?;
        let segment = decode_segment_payload(spec, &payload, link.record_offset, file_len)?;
        drop(payload);
        // `preceding_records` is the writer's *resident* index position, and
        // the filter below drops exactly what the writer never installed, so
        // the two counts stay comparable with a non-resident block in play.
        let count = u64::try_from(scanned.entries.len()).map_err(|_| {
            Error::ResourceArithmeticOverflow {
                resource: "record count",
            }
        })?;
        if segment.covered_start != running || segment.preceding_records != count {
            return Err(Error::InvalidIndexSegment);
        }
        for covered in segment.entries {
            running = push_scanned_entry(spec, &mut scanned, covered, running)?;
        }
        // The segment record closes its own coverage, so it sits immediately
        // after the last record it describes.
        if link.record_offset != running {
            return Err(Error::InvalidIndexSegment);
        }
        running = push_scanned_entry(spec, &mut scanned, link, running)?;
    }
    if running != file_len {
        return Err(Error::InvalidIndexSegment);
    }
    let entries = scanned.entries;
    // Same single witness the scan carries, on the same shape of list
    // (PERF2-07): a chain is written by this crate but read from a file
    // anyone can hand over.
    validate_unique_sequences(&entries)?;
    // The scan ends by cutting its list to the committed prefix, and the walk
    // must not hand back a list the scan would have cut. A chain that tiles the
    // whole append log without a commit marker in it is structurally perfect
    // and still a lie: `truncate_uncommitted_tail_if_needed` runs next, cuts
    // the file to the header, and would leave this handle holding an index of
    // records that are no longer on disk — after which the next append lands on
    // an offset the index already claims and the file is unreadable by either
    // path. Refusing routes it to the scan, which already knows how to truncate
    // and to report the empty index that goes with it.
    if spec.commit_policy.is_transaction_marker()
        && committed_prefix_len(&entries) != Some(entries.len())
    {
        return Err(Error::InvalidIndexSegment);
    }
    Ok(ScannedIndex { entries, ..scanned })
}

/// Appends one entry to an index under construction, charging the same limits
/// the record scan charges, and reports where the record after it must begin.
fn push_scanned_entry(
    spec: FormatSpec,
    scanned: &mut ScannedIndex,
    entry: RecordIndexEntry,
    expected_offset: u64,
) -> Result<u64> {
    if entry.record_offset != expected_offset
        || entry.payload_offset
            != expected_offset
                .checked_add(RECORD_HEADER_LEN)
                .ok_or(Error::InvalidIndexSegment)?
    {
        return Err(Error::InvalidIndexSegment);
    }
    let end = entry.checked_physical_end()?;
    // Every record contributes its sequence, its block tail and the physical
    // end of the walk; only a resident one is materialised. The chain carries
    // every record precisely so this filter can happen here rather than on
    // disk.
    scanned.note(&entry, end);
    if record_is_resident(spec, entry.block_id) {
        reserve_scanned_entry(spec, &mut scanned.entries)?;
        scanned.entries.push(entry);
    }
    Ok(end)
}

/// Whether a record of `block_id` is mirrored in the resident index.
///
/// varve's own bookkeeping is always resident: the manifest, commit markers,
/// checkpoints and segment records are what the open paths navigate by.
fn record_is_resident(spec: FormatSpec, block_id: u32) -> bool {
    block_id >= RESERVED_BLOCK_ID_START || spec.block_is_resident(block_id)
}

/// Charges the record and index-byte ceilings for one more index entry and
/// reserves room for it.
fn reserve_scanned_entry(spec: FormatSpec, entries: &mut Vec<RecordIndexEntry>) -> Result<()> {
    let record_count = u64::try_from(entries.len())
        .map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "record count",
        })?
        .checked_add(1)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "record count",
        })?;
    spec.read_limits
        .check(ReadLimitKey::Records, record_count)?;
    let index_bytes = index_bytes_for_count(usize::try_from(record_count).map_err(|_| {
        Error::LengthOverflow {
            value: record_count,
        }
    })?)?;
    spec.read_limits
        .check(ReadLimitKey::IndexBytes, index_bytes)?;
    entries.try_reserve(1).map_err(|_| Error::AllocationFailed {
        resource: "record index",
        requested: index_bytes,
    })
}

/// How many leading entries a transaction-marker format treats as committed.
///
/// The commit marker is the boundary, and everything after it is a writer's
/// unfinished work — with exactly one exception, bounded at one entry. A
/// segment record is appended *after* the marker whose transaction it closes,
/// because open finds the chain at the end of the file and a record behind the
/// segment would hide it. That segment describes only records the marker
/// already committed, so it is inside the boundary rather than past it.
///
/// Returns `None` when the file holds no commit marker, which means nothing in
/// it is committed.
fn committed_prefix_len(entries: &[RecordIndexEntry]) -> Option<usize> {
    let position = entries
        .iter()
        .rposition(|entry| entry.block_id == COMMIT_BLOCK_ID)?;
    let kept = position + 1;
    // `entries.len() == kept + 1` was too strict: it kept the segment only when
    // it was the very last entry, so one uncommitted record appended after a
    // flush took the segment down with it. The writer then restarted coverage
    // from the older surviving segment, and every later segment re-covered
    // everything since — an O(N) payload and an O(N) allocation on every flush,
    // until `segment_payload_len` passed `max_record_payload_len` and segments
    // stopped being written at all, silently. The segment describes only
    // records the marker already committed, so keeping it is exactly as safe
    // whatever follows it.
    if entries.len() > kept && entries[kept].block_id == SEGMENT_BLOCK_ID {
        Some(kept + 1)
    } else {
        Some(kept)
    }
}

fn truncate_uncommitted_tail_if_needed(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    entries: &[RecordIndexEntry],
) -> Result<()> {
    if !spec.commit_policy.is_transaction_marker() {
        return Ok(());
    }
    let committed_end = match committed_prefix_len(entries) {
        Some(len) => entries[len - 1].checked_physical_end()?,
        None => header_len,
    };
    if file.metadata()?.len() > committed_end {
        file.set_len(committed_end)?;
    }
    Ok(())
}

#[derive(Debug)]
struct IndexCheckpoint {
    covered_offset: u64,
    entries: Vec<RecordIndexEntry>,
}

fn decode_index_checkpoint(
    spec: FormatSpec,
    payload: &[u8],
    file_len: u64,
) -> Result<IndexCheckpoint> {
    const PREFIX_LEN: usize = 4 + 2 + 8 + 8;
    const ENTRY_LEN_V1: usize = 4 + 2 + 2 + 8 + 8 + 8 + 8 + 4;
    const ENTRY_LEN_V2: usize = ENTRY_LEN_V1 + 4;
    const ENTRY_LEN_V3: usize = ENTRY_LEN_V2 + 8 + 8 + 8 + 1;
    if payload.len() < PREFIX_LEN || &payload[..4] != INDEX_CHECKPOINT_MAGIC {
        return Err(Error::InvalidIndexCheckpoint);
    }
    let mut version = [0; 2];
    version.copy_from_slice(&payload[4..6]);
    let checkpoint_version = u16::from_le_bytes(version);
    if checkpoint_version == 0 || checkpoint_version > INDEX_CHECKPOINT_VERSION {
        return Err(Error::InvalidIndexCheckpoint);
    }
    let entry_len = match checkpoint_version {
        1 => ENTRY_LEN_V1,
        2 => ENTRY_LEN_V2,
        3 => ENTRY_LEN_V3,
        _ => return Err(Error::InvalidIndexCheckpoint),
    };
    let mut covered_offset = [0; 8];
    covered_offset.copy_from_slice(&payload[6..14]);
    let covered_offset = u64::from_le_bytes(covered_offset);
    let mut count = [0; 8];
    count.copy_from_slice(&payload[14..22]);
    let count = u64::from_le_bytes(count);
    spec.read_limits.check(ReadLimitKey::Records, count)?;
    let count_usize = usize::try_from(count).map_err(|_| Error::LengthOverflow { value: count })?;
    let resident_bytes = index_bytes_for_count(count_usize)?;
    spec.read_limits
        .check(ReadLimitKey::IndexBytes, resident_bytes)?;
    let expected_len = PREFIX_LEN
        .checked_add(
            count_usize
                .checked_mul(entry_len)
                .ok_or(Error::InvalidIndexCheckpoint)?,
        )
        .ok_or(Error::InvalidIndexCheckpoint)?;
    if payload.len() != expected_len {
        return Err(Error::InvalidIndexCheckpoint);
    }

    let mut position = PREFIX_LEN;
    for _ in 0..count_usize {
        let entry =
            read_index_entry_payload(&payload[position..position + entry_len], checkpoint_version);
        if entry.checked_physical_end()? > file_len {
            return Err(Error::InvalidIndexCheckpoint);
        }
        position += entry_len;
    }

    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count_usize)
        .map_err(|_| Error::AllocationFailed {
            resource: "checkpoint index",
            requested: resident_bytes,
        })?;
    let mut position = PREFIX_LEN;
    for _ in 0..count_usize {
        entries.push(read_index_entry_payload(
            &payload[position..position + entry_len],
            checkpoint_version,
        ));
        position += entry_len;
    }
    Ok(IndexCheckpoint {
        covered_offset,
        entries,
    })
}

/// Bytes of an index checkpoint payload that precede its entry array.
const INDEX_CHECKPOINT_PREFIX_LEN: u64 = 4 + 2 + 8 + 8;

/// Runs the two ceilings an index checkpoint declares, from its prefix alone.
///
/// This replaces reading the whole payload and decoding it. `inspect_index_
/// checkpoint` did exactly that on every checkpoint record an open walked past
/// — and then dropped the result on the floor (`let _ = validate_index_
/// checkpoint(...)`), so the entire read, allocation and decode existed to
/// raise `LimitExceeded` from two fields. A checkpoint payload IS the index as
/// of its own position, and checkpoints are geometrically spaced, so that was
/// O(N) entries read and discarded per open of a `checkpoint_on_flush` format.
///
/// `count` sits at bytes 14..22, so the ceilings are decided by 22 bytes. The
/// errors raised are the same ones from the same fields.
fn check_index_checkpoint_limits(spec: FormatSpec, prefix: &[u8]) -> Result<()> {
    let prefix_len = INDEX_CHECKPOINT_PREFIX_LEN as usize;
    if prefix.len() < prefix_len || &prefix[..4] != INDEX_CHECKPOINT_MAGIC {
        // Not a checkpoint this build understands. `decode_index_checkpoint`
        // answered the same shape with a non-limit error, which the caller
        // swallowed, so there is nothing to raise here either.
        return Ok(());
    }
    let mut version = [0; 2];
    version.copy_from_slice(&prefix[4..6]);
    if !(1..=INDEX_CHECKPOINT_VERSION).contains(&u16::from_le_bytes(version)) {
        return Ok(());
    }
    let mut count = [0; 8];
    count.copy_from_slice(&prefix[14..22]);
    let count = u64::from_le_bytes(count);
    spec.read_limits.check(ReadLimitKey::Records, count)?;
    let count_usize = usize::try_from(count).map_err(|_| Error::LengthOverflow { value: count })?;
    spec.read_limits.check(
        ReadLimitKey::IndexBytes,
        index_bytes_for_count(count_usize)?,
    )
}

fn inspect_index_checkpoint<'a>(
    spec: FormatSpec,
    payload: &[u8],
    header_len: u64,
    file_len: u64,
    checkpoint_record: &RecordIndexEntry,
    observed_prefix_len: usize,
    observed_prefix: impl Iterator<Item = &'a RecordIndexEntry>,
) -> Result<()> {
    match decode_index_checkpoint(spec, payload, file_len) {
        Ok(checkpoint) => {
            let _ = validate_index_checkpoint(
                header_len,
                file_len,
                checkpoint_record,
                &checkpoint,
                observed_prefix_len,
                observed_prefix,
            );
            Ok(())
        }
        Err(error @ Error::LimitExceeded { .. })
        | Err(error @ Error::MissingResourceLimit { .. })
        | Err(error @ Error::TrustedUnboundedRequiresExplicitApi { .. })
        | Err(error @ Error::ResourceArithmeticOverflow { .. })
        | Err(error @ Error::LengthOverflow { .. })
        | Err(error @ Error::AllocationFailed { .. }) => Err(error),
        Err(_) => Ok(()),
    }
}

fn read_index_entry_payload(payload: &[u8], checkpoint_version: u16) -> RecordIndexEntry {
    let mut u32_buf = [0; 4];
    let mut u16_buf = [0; 2];
    let mut u64_buf = [0; 8];

    u32_buf.copy_from_slice(&payload[0..4]);
    let block_id = u32::from_le_bytes(u32_buf);
    u16_buf.copy_from_slice(&payload[4..6]);
    let block_version = u16::from_le_bytes(u16_buf);
    u16_buf.copy_from_slice(&payload[6..8]);
    let flags = u16::from_le_bytes(u16_buf);
    u64_buf.copy_from_slice(&payload[8..16]);
    let sequence = u64::from_le_bytes(u64_buf);
    u64_buf.copy_from_slice(&payload[16..24]);
    let record_offset = u64::from_le_bytes(u64_buf);
    u64_buf.copy_from_slice(&payload[24..32]);
    let payload_offset = u64::from_le_bytes(u64_buf);
    u64_buf.copy_from_slice(&payload[32..40]);
    let payload_len = u64::from_le_bytes(u64_buf);
    u32_buf.copy_from_slice(&payload[40..44]);
    let checksum = u32::from_le_bytes(u32_buf);
    let uncompressed_len_hint = if checkpoint_version >= 2 {
        u32_buf.copy_from_slice(&payload[44..48]);
        u32::from_le_bytes(u32_buf)
    } else {
        0
    };
    let (footer_offset, prev_same_block_offset, prev_same_key_offset, committed) =
        if checkpoint_version >= 3 {
            u64_buf.copy_from_slice(&payload[48..56]);
            let footer_offset = nonzero_offset(u64::from_le_bytes(u64_buf));
            u64_buf.copy_from_slice(&payload[56..64]);
            let prev_same_block_offset = nonzero_offset(u64::from_le_bytes(u64_buf));
            u64_buf.copy_from_slice(&payload[64..72]);
            let prev_same_key_offset = nonzero_offset(u64::from_le_bytes(u64_buf));
            let committed = payload[72] != 0;
            (
                footer_offset,
                prev_same_block_offset,
                prev_same_key_offset,
                committed,
            )
        } else {
            (None, None, None, true)
        };

    RecordIndexEntry {
        block_id,
        block_version,
        flags,
        sequence,
        record_offset,
        payload_offset,
        payload_len,
        checksum,
        uncompressed_len_hint,
        footer_offset,
        prev_same_block_offset,
        prev_same_key_offset,
        committed,
    }
}

fn validate_index_checkpoint<'a>(
    header_len: u64,
    file_len: u64,
    checkpoint_record: &RecordIndexEntry,
    checkpoint: &IndexCheckpoint,
    observed_prefix_len: usize,
    observed_prefix: impl Iterator<Item = &'a RecordIndexEntry>,
) -> Result<()> {
    if checkpoint.covered_offset != checkpoint_record.record_offset
        || checkpoint.covered_offset < header_len
        || checkpoint.covered_offset > file_len
    {
        return Err(Error::InvalidIndexCheckpoint);
    }
    if checkpoint.entries.len() != observed_prefix_len
        || !checkpoint.entries.iter().zip(observed_prefix).all(
            |(checkpoint_entry, observed_entry)| {
                record_headers_match(checkpoint_entry, observed_entry)
            },
        )
    {
        return Err(Error::InvalidIndexCheckpoint);
    }

    // The records a checkpoint covers are contiguous, because the append log
    // is. `<` accepted a *gap* between one record's end and the next record's
    // start, so a one-entry checkpoint could claim `covered_offset` far past
    // the record it listed and validate: every record in between is on disk,
    // absent from the checkpoint, and therefore absent from `index_entries()`
    // and from every typed read. `!=` is the check that was meant, and the
    // terminal equality below closes the same hole at the far end - without it
    // the gap simply moves to after the last entry.
    let mut previous_offset = header_len;
    for entry in &checkpoint.entries {
        let expected_payload_offset = entry
            .record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::InvalidIndexCheckpoint)?;
        if entry.record_offset < header_len
            || entry.record_offset != previous_offset
            || entry.payload_offset != expected_payload_offset
            || entry.record_offset == checkpoint_record.record_offset
        {
            return Err(Error::InvalidIndexCheckpoint);
        }
        let physical_end = entry
            .checked_physical_end()
            .map_err(|_| Error::InvalidIndexCheckpoint)?;
        if physical_end > checkpoint.covered_offset || physical_end > file_len {
            return Err(Error::InvalidIndexCheckpoint);
        }
        previous_offset = physical_end;
    }
    if previous_offset != checkpoint.covered_offset {
        return Err(Error::InvalidIndexCheckpoint);
    }
    Ok(())
}

fn record_headers_match(left: &RecordIndexEntry, right: &RecordIndexEntry) -> bool {
    left.block_id == right.block_id
        && left.block_version == right.block_version
        && left.flags == right.flags
        && left.sequence == right.sequence
        && left.record_offset == right.record_offset
        && left.payload_offset == right.payload_offset
        && left.payload_len == right.payload_len
        && left.checksum == right.checksum
        && left.uncompressed_len_hint == right.uncompressed_len_hint
        && left.footer_offset == right.footer_offset
        && left.prev_same_block_offset == right.prev_same_block_offset
        && left.prev_same_key_offset == right.prev_same_key_offset
        && left.committed == right.committed
}

fn scan_records_from(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    intent: ScanIntent,
) -> Result<ScannedIndex> {
    let file_len = file.metadata()?.len();
    let mut offset = header_len;
    let mut entries = Vec::new();
    let mut scanned = ScannedIndex {
        entries: Vec::new(),
        sequence_high_water: None,
        sequence_high_water_at_commit: None,
        newest: HashMap::new(),
        newest_at_commit: HashMap::new(),
        physical_end: None,
        physical_end_at_commit: None,
    };
    let mut latest_commit_end = None;
    let mut accounting = ScanAccounting::default();
    accounting.advance(spec, header_len)?;
    while offset < file_len {
        let partial_boundary = match spec.commit_policy {
            CommitPolicy::None
                if intent == ScanIntent::Recover
                    && spec.recovery_policy == RecoveryPolicy::TruncateTail =>
            {
                Some(offset)
            }
            CommitPolicy::None => None,
            CommitPolicy::RecordFooter => Some(offset),
            CommitPolicy::TransactionMarker(_) => Some(latest_commit_end.unwrap_or(header_len)),
        };
        let checksum_boundary = if spec.commit_policy.is_transaction_marker() {
            latest_commit_end
        } else {
            None
        };
        // A recovery scan verifies whatever the policy says, because a checksum
        // mismatch is the evidence `RecoveryPolicy::TruncateTail` truncates on:
        // `checksum_boundary` above is only reachable through this check.
        // Dropping it here would turn a recoverable tail into a silently
        // accepted one, which is the failure this whole policy must not create.
        let verify_checksums = intent == ScanIntent::Recover
            || spec
                .read_limits
                .resolve()
                .effective_integrity_verification()
                == IntegrityVerification::AtOpen;
        let entry = match read_record_entry_at(
            spec,
            file,
            file_len,
            offset,
            ScanChecks {
                partial_boundary,
                checksum_boundary,
                verify_checksums,
            },
            &mut accounting,
        )? {
            RecordRead::Entry(entry) => entry,
            RecordRead::RecoverableTail(tail) => match intent {
                ScanIntent::ReadOnly => break,
                ScanIntent::Recover if spec.recovery_policy == RecoveryPolicy::TruncateTail => {
                    file.set_len(tail.truncate_to)?;
                    break;
                }
                ScanIntent::Writer if spec.commit_policy.is_transaction_marker() => break,
                ScanIntent::Writer | ScanIntent::Recover => {
                    return Err(Error::CorruptTail {
                        offset: tail.offset,
                    });
                }
            },
        };
        offset = entry
            .checked_physical_end()
            .map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "record extent",
            })?;
        // Every record contributes its sequence, its block tail and the
        // physical end of the walk even when it is not materialised: the
        // high-water mark is what the next append continues from, the tail is
        // the only way back to a non-resident record, and the end is the byte
        // range a read-only handle may look at. `offset` was just advanced to
        // this record's end.
        scanned.note(&entry, offset);
        let resident = record_is_resident(spec, entry.block_id);
        if resident {
            reserve_scanned_entry(spec, &mut entries)?;
        }
        if entry.block_id == INDEX_BLOCK_ID
            && (1..=INDEX_CHECKPOINT_VERSION).contains(&entry.block_version)
        {
            let prefix = entry.read_payload_prefix_file_with_len(
                file,
                file_len,
                INDEX_CHECKPOINT_PREFIX_LEN,
            )?;
            check_index_checkpoint_limits(spec, &prefix)?;
        }
        if entry.block_id == COMMIT_BLOCK_ID {
            latest_commit_end = Some(offset);
        }
        if resident {
            entries.push(entry);
        }
    }
    // Single sequence-uniqueness witness for the whole load path: `load_index`
    // relies on this check and must not repeat it (PERF2-07). It runs on the
    // full scanned list, so the commit-boundary truncation below can only
    // shrink an already-validated set.
    validate_unique_sequences(&entries)?;
    if spec.commit_policy.is_transaction_marker() {
        let Some(committed) = committed_prefix_len(&entries) else {
            scanned.commit_boundary_applied(&[])?;
            return Ok(ScannedIndex {
                entries: Vec::new(),
                ..scanned
            });
        };
        entries.truncate(committed);
        for entry in &mut entries {
            entry.committed = true;
        }
        scanned.commit_boundary_applied(&entries)?;
    }
    Ok(ScannedIndex { entries, ..scanned })
}

impl RecordIndexEntry {
    fn read_payload_file_with_len(&self, file: &mut File, file_len: u64) -> Result<Vec<u8>> {
        self.validate_payload_extent(file_len)?;
        self.read_payload_file_validated(file)
    }

    /// Reads the first `len` bytes of this record's payload.
    ///
    /// For a segment link, everything the chain walk's first pass needs —
    /// `covered_start`, the entry count and `preceding_records` — sits in the
    /// fixed prefix, so the pass does not have to pull the whole entry array
    /// off disk and throw it away.
    fn read_payload_prefix_file_with_len(
        &self,
        file: &mut File,
        file_len: u64,
        len: u64,
    ) -> Result<Vec<u8>> {
        self.validate_payload_extent(file_len)?;
        if self.payload_len < len {
            return Err(Error::InvalidIndexSegment);
        }
        file.seek(SeekFrom::Start(self.payload_offset))?;
        let mut prefix = try_alloc_bytes(len, PHYSICAL_PAYLOAD_RESOURCE)?;
        file.read_exact(&mut prefix)?;
        Ok(prefix)
    }

    fn read_payload_file_validated(&self, file: &mut File) -> Result<Vec<u8>> {
        file.seek(SeekFrom::Start(self.payload_offset))?;
        let mut payload = try_alloc_bytes(self.payload_len, PHYSICAL_PAYLOAD_RESOURCE)?;
        file.read_exact(&mut payload)?;
        Ok(payload)
    }

    fn validate_payload_extent(&self, file_len: u64) -> Result<()> {
        let payload_end =
            self.payload_offset
                .checked_add(self.payload_len)
                .ok_or(Error::LengthOverflow {
                    value: self.payload_len,
                })?;
        if payload_end > file_len {
            return Err(Error::UnexpectedEof);
        }
        if let Some(footer_offset) = self.footer_offset {
            if footer_offset != payload_end {
                return Err(Error::CorruptTail {
                    offset: self.record_offset,
                });
            }
            if self.checked_physical_end()? > file_len {
                return Err(Error::UnexpectedEof);
            }
        }
        Ok(())
    }

    fn logical_payload_len_before_allocation(
        &self,
        spec: FormatSpec,
        file: &mut File,
    ) -> Result<u64> {
        if !self.is_compressed() {
            return Ok(self.payload_len);
        }
        validate_record_entry(spec, self)?;
        let compression = variable_compression_for_block(spec, self.block_id)
            .ok_or(Error::InvalidCompressionHeader)?;
        let expected_len = match compression.header_mode {
            CompressionHeaderMode::RecordExplicit => {
                if self.payload_len < 20 {
                    return Err(Error::InvalidCompressionHeader);
                }
                let mut prefix = [0; 20];
                file.seek(SeekFrom::Start(self.payload_offset))?;
                file.read_exact(&mut prefix)?;
                let (_, expected_len, _) = decode_compression_envelope(&prefix)?;
                expected_len
            }
            CompressionHeaderMode::FileExplicit | CompressionHeaderMode::FormatContract => {
                if self.uncompressed_len_hint == 0 {
                    return Err(Error::InvalidCompressionHeader);
                }
                u64::from(self.uncompressed_len_hint)
            }
        };
        if expected_len > compression.max_uncompressed_len {
            return Err(Error::DecompressedLengthLimitExceeded {
                actual: expected_len,
                limit: compression.max_uncompressed_len,
            });
        }
        Ok(expected_len)
    }

    fn logical_payload_len_from_snapshot(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
    ) -> Result<u64> {
        validate_record_entry(spec, self)?;
        let compression = variable_compression_for_block(spec, self.block_id)
            .ok_or(Error::InvalidCompressionHeader)?;
        let expected_len = match compression.header_mode {
            CompressionHeaderMode::RecordExplicit => {
                if self.payload_len < 20 {
                    return Err(Error::InvalidCompressionHeader);
                }
                let mut prefix = [0; 20];
                snapshot.read_exact_at(self.payload_offset, &mut prefix)?;
                let (_, expected_len, _) = decode_compression_envelope(&prefix)?;
                expected_len
            }
            CompressionHeaderMode::FileExplicit | CompressionHeaderMode::FormatContract => {
                if self.uncompressed_len_hint == 0 {
                    return Err(Error::InvalidCompressionHeader);
                }
                u64::from(self.uncompressed_len_hint)
            }
        };
        if expected_len > compression.max_uncompressed_len {
            return Err(Error::DecompressedLengthLimitExceeded {
                actual: expected_len,
                limit: compression.max_uncompressed_len,
            });
        }
        Ok(expected_len)
    }

    fn verify_snapshot_record(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
        payload: &[u8],
    ) -> Result<()> {
        if spec.integrity_policy == IntegrityPolicy::None {
            return Ok(());
        }

        let mut header_bytes = [0; RECORD_HEADER_LEN as usize];
        snapshot.read_exact_at(self.record_offset, &mut header_bytes)?;
        let mut header_reader = header_bytes.as_slice();
        let actual = read_native_record_header(&mut header_reader, self.record_offset)?.fields;
        if actual.block_id != self.block_id
            || actual.block_version != self.block_version
            || actual.flags != self.flags
            || actual.sequence != self.sequence
            || actual.payload_len != self.payload_len
            || actual.checksum != self.checksum
            || actual.uncompressed_len_hint != self.uncompressed_len_hint
        {
            return Err(Error::ChecksumMismatch {
                offset: self.record_offset,
            });
        }

        let mut footer_bytes = [0; RECORD_FOOTER_LEN as usize];
        let footer = if let Some(footer_offset) = self.footer_offset {
            snapshot.read_exact_at(footer_offset, &mut footer_bytes)?;
            let decoded = decode_record_footer(&footer_bytes, footer_offset, self.record_offset)?;
            if decoded.prev_same_block_offset != self.prev_same_block_offset
                || decoded.prev_same_key_offset != self.prev_same_key_offset
            {
                return Err(Error::ChecksumMismatch {
                    offset: self.record_offset,
                });
            }
            &footer_bytes[..]
        } else {
            &[]
        };
        let header = RecordHeaderFields {
            block_id: self.block_id,
            block_version: self.block_version,
            flags: self.flags,
            sequence: self.sequence,
            payload_len: self.payload_len,
            checksum: 0,
            uncompressed_len_hint: self.uncompressed_len_hint,
        };
        let actual_checksum =
            checksum_record_fields(spec, self.record_offset, header, payload, footer)?;
        if actual_checksum != self.checksum {
            return Err(Error::ChecksumMismatch {
                offset: self.record_offset,
            });
        }
        Ok(())
    }
}

const fn limit_or_max(limit: Option<u64>) -> u64 {
    match limit {
        Some(limit) => limit,
        None => u64::MAX,
    }
}

fn ensure_payload_limit(resource: &'static str, actual: u64, limit: u64) -> Result<()> {
    if actual > limit {
        Err(Error::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

fn index_bytes_for_count(count: usize) -> Result<u64> {
    let count = u64::try_from(count).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "index bytes",
    })?;
    let entry_size = u64::try_from(size_of::<RecordIndexEntry>()).map_err(|_| {
        Error::ResourceArithmeticOverflow {
            resource: "index bytes",
        }
    })?;
    count
        .checked_mul(entry_size)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "index bytes",
        })
}

fn allocation_bytes<T>(count: usize, resource: &'static str) -> Result<u64> {
    u64::try_from(count)
        .map_err(|_| Error::ResourceArithmeticOverflow { resource })?
        .checked_mul(size_of::<T>() as u64)
        .ok_or(Error::ResourceArithmeticOverflow { resource })
}

#[cfg(feature = "mmap")]
fn mmap_index_bytes_for_count(count: usize) -> Result<u64> {
    let count = u64::try_from(count).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "mmap index bytes",
    })?;
    // One index entry copy plus one `by_block` position per record; the
    // former second full entry copy for the membership hash set is gone
    // (PERF2-07).
    let entry_bytes = u64::try_from(size_of::<RecordIndexEntry>())
        .map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "mmap index bytes",
        })?
        .checked_add(size_of::<usize>() as u64)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "mmap index bytes",
        })?;
    count
        .checked_mul(entry_bytes)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "mmap index bytes",
        })
}

/// Bytes the sequence-uniqueness witness allocates for `records` entries.
///
/// This is the transient every resident input open can pay on top of its
/// resident index, so [`KeyedMergeEstimate`] has to charge it (PERF3-05).
fn sequence_uniqueness_transient_bytes(records: usize) -> Result<u64> {
    u64::try_from(records)
        .map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "sequence uniqueness index",
        })?
        .checked_mul(size_of::<u64>() as u64)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "sequence uniqueness index",
        })
}

/// Rejects a native file that reuses a record sequence.
///
/// Fast path (PERF3-05): the writer hands out sequences from a monotonically
/// increasing counter, so an ordinary append-only file scanned in offset order
/// is strictly increasing. Verifying that costs one allocation-free pass and
/// proves uniqueness outright. Only an input whose records are *not* in
/// ascending sequence order - a rewritten or externally reordered generation -
/// falls back to the sorted copy, which costs `O(N log N)` time and an explicit
/// `8N` temporary. The fallback is still the published worst case, so the
/// resident merge estimate charges it unconditionally rather than assuming the
/// fast path.
fn validate_unique_sequences(entries: &[RecordIndexEntry]) -> Result<()> {
    if entries
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence)
    {
        return Ok(());
    }
    let requested = sequence_uniqueness_transient_bytes(entries.len())?;
    let mut sequences = Vec::new();
    sequences
        .try_reserve_exact(entries.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "sequence uniqueness index",
            requested,
        })?;
    sequences.extend(entries.iter().map(|entry| entry.sequence));
    sequences.sort_unstable();
    if sequences.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(Error::InvalidCanonicalEncoding(
            "duplicate native record sequence",
        ));
    }
    Ok(())
}

/// Clones the entries a predicate selects, charged and reserved before the
/// first one is copied.
///
/// **`count` is the caller's**, because the caller is what knows whether it can
/// be had for free. This used to run the predicate over the whole source once
/// to count and once to copy — and its heaviest caller passes `|_| true`, where
/// the count is `source.len()` and the first pass was walking the entire index
/// to learn something it already knew.
///
/// A count is still required rather than optional: the `IndexBytes` charge and
/// the exact reservation must both precede the copy, which is the invariant that
/// keeps a refused limit from leaving a half-built generation behind.
fn clone_matching_entries<F>(
    spec: FormatSpec,
    source: &ResidentIndex,
    count: usize,
    mut matches: F,
) -> Result<Vec<RecordIndexEntry>>
where
    F: FnMut(&RecordIndexEntry) -> bool,
{
    let requested = index_bytes_for_count(count)?;
    spec.read_limits
        .check(ReadLimitKey::IndexBytes, requested)?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count)
        .map_err(|_| Error::AllocationFailed {
            resource: "block index",
            requested,
        })?;
    entries.extend(source.iter().filter(|entry| matches(entry)).cloned());
    debug_assert_eq!(
        entries.len(),
        count,
        "declared count did not match the predicate"
    );
    Ok(entries)
}

/// The three per-handle structures every open and every generation rebind
/// derives from the resident index.
struct DerivedIndexState {
    checkpoint_cadence: CheckpointCadence,
    segment_cursor: SegmentCursor,
    /// `None` when the caller did not ask for tails, which an open does not:
    /// it already has them from the scan or the chain walk, and building the
    /// newest-per-block map here would be work thrown away.
    block_tails: Option<BlockTails>,
}

/// Derives all three in ONE forward pass.
///
/// They used to be three separate walks of the same entries — one forward
/// (`BlockTails`) and two reverse (`CheckpointCadence`, `SegmentCursor`) — run
/// back to back at every open and at both generation rebinds. The two reverse
/// walks stop at the newest checkpoint and the newest segment respectively, so
/// on a file that carries neither they each walk the whole index; the forward
/// one has no early exit at all. Three traversals, and a single forward pass
/// answers all of them, because "the last entry with this block id" is just the
/// last one a forward pass saw.
///
/// Each field is still computed by exactly the rule its own `from_index`
/// states, so the invariants those doc comments assert are unchanged. The unit
/// tests below compare this against all three.
fn derive_index_state<'a>(
    entries: impl Iterator<Item = &'a RecordIndexEntry>,
    with_tails: bool,
) -> DerivedIndexState {
    let mut touched: u64 = 0;
    let mut newest: HashMap<u32, u64> = HashMap::new();
    // Position of, and entry at, the newest record of each kind the derived
    // structures key on.
    let mut newest_checkpoint: Option<usize> = None;
    let mut newest_segment: Option<(usize, u64)> = None;
    // Eligible records since the newest checkpoint, counted forward: reset each
    // time a checkpoint is seen, which leaves exactly the tail the reverse walk
    // used to accumulate.
    let mut eligible: usize = 0;
    for (position, entry) in entries.enumerate() {
        touched = touched.saturating_add(1);
        if with_tails {
            newest.insert(entry.block_id, entry.record_offset);
        }
        match entry.block_id {
            INDEX_BLOCK_ID => {
                newest_checkpoint = Some(position);
                eligible = 0;
            }
            SEGMENT_BLOCK_ID => newest_segment = Some((position, entry.physical_end())),
            COMMIT_BLOCK_ID => {}
            _ => eligible = eligible.saturating_add(1),
        }
    }
    let state = DerivedIndexState {
        checkpoint_cadence: CheckpointCadence {
            eligible_since_checkpoint: eligible,
            next_threshold: match newest_checkpoint {
                Some(position) => core::cmp::max(INDEX_CHECKPOINT_MIN_RECORDS, position / 2),
                None => INDEX_CHECKPOINT_MIN_RECORDS,
            },
        },
        segment_cursor: match newest_segment {
            Some((position, physical_end)) => SegmentCursor {
                next_position: position + 1,
                covered_start: Some(physical_end),
            },
            None => SegmentCursor::new_empty(),
        },
        block_tails: with_tails.then(|| BlockTails::from_newest(&newest)),
    };
    if with_tails {
        note_block_tail_index_touches(touched);
    }
    note_checkpoint_cadence_index_touches(touched);
    state
}

fn read_record_header(file: &mut File, _spec: FormatSpec, offset: u64) -> Result<RecordIndexEntry> {
    let decoded = read_native_record_header(file, offset)?;
    debug_assert_eq!(decoded.lead_in_len, RECORD_HEADER_LEN);
    let header = decoded.fields;
    Ok(RecordIndexEntry {
        block_id: header.block_id,
        block_version: header.block_version,
        flags: header.flags,
        sequence: header.sequence,
        record_offset: offset,
        payload_offset: offset
            .checked_add(decoded.lead_in_len)
            .ok_or(Error::CorruptTail { offset })?,
        payload_len: header.payload_len,
        checksum: header.checksum,
        uncompressed_len_hint: header.uncompressed_len_hint,
        footer_offset: None,
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        committed: true,
    })
}

fn write_record_header(
    file: &mut File,
    spec: FormatSpec,
    record_offset: u64,
    header: RecordHeaderFields,
) -> Result<()> {
    let footer_len = record_footer_len(spec);
    debug_assert_eq!(native_record_header_len(), RECORD_HEADER_LEN);
    write_native_record_header(file, header, record_offset, footer_len)
}

fn encode_record_footer(footer: RecordFooterFields) -> Result<[u8; RECORD_FOOTER_LEN as usize]> {
    encode_native_record_footer(footer)
}

fn read_record_footer_bytes(
    file: &mut File,
    offset: u64,
) -> Result<[u8; RECORD_FOOTER_LEN as usize]> {
    file.seek(SeekFrom::Start(offset))?;
    let mut footer = [0; RECORD_FOOTER_LEN as usize];
    file.read_exact(&mut footer)?;
    Ok(footer)
}

fn decode_record_footer(
    bytes: &[u8],
    footer_offset: u64,
    record_offset: u64,
) -> Result<RecordFooterFields> {
    decode_native_record_footer(bytes, footer_offset, record_offset)
}

fn checksum_record_fields(
    spec: FormatSpec,
    record_offset: u64,
    mut header: RecordHeaderFields,
    payload: &[u8],
    footer: &[u8],
) -> Result<u32> {
    header.checksum = 0;
    let header_bytes = if spec.integrity_policy == IntegrityPolicy::Crc32WithHeader {
        Some(encode_native_record_header(
            header,
            record_offset,
            record_footer_len(spec),
        )?)
    } else {
        None
    };
    checksum_record_bytes(
        spec,
        header_bytes.as_ref().map(|bytes| &bytes[..]).unwrap_or(&[]),
        payload,
        footer,
    )
}

fn checksum_record_file(
    spec: FormatSpec,
    file: &mut File,
    entry: &RecordIndexEntry,
    footer: &[u8],
) -> Result<u32> {
    let header = RecordHeaderFields {
        block_id: entry.block_id,
        block_version: entry.block_version,
        flags: entry.flags,
        sequence: entry.sequence,
        payload_len: entry.payload_len,
        checksum: 0,
        uncompressed_len_hint: entry.uncompressed_len_hint,
    };
    let header_bytes = if spec.integrity_policy == IntegrityPolicy::Crc32WithHeader {
        Some(encode_native_record_header(
            header,
            entry.record_offset,
            record_footer_len(spec),
        )?)
    } else {
        None
    };
    crc32_record_file(
        file,
        entry.payload_offset,
        entry.payload_len,
        header_bytes.as_ref().map(|bytes| &bytes[..]).unwrap_or(&[]),
        footer,
    )
}

#[cfg(feature = "integrity")]
fn crc32_record_file(
    file: &mut File,
    payload_offset: u64,
    payload_len: u64,
    header: &[u8],
    footer: &[u8],
) -> Result<u32> {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(header);
    file.seek(SeekFrom::Start(payload_offset))?;
    let mut remaining = payload_len;
    let mut buffer = [0; STREAM_BUFFER_LEN];
    while remaining != 0 {
        let chunk_len = usize::try_from(remaining.min(STREAM_BUFFER_LEN as u64)).map_err(|_| {
            Error::ResourceArithmeticOverflow {
                resource: "checksum buffer length",
            }
        })?;
        file.read_exact(&mut buffer[..chunk_len])?;
        hasher.update(&buffer[..chunk_len]);
        remaining -= chunk_len as u64;
    }
    hasher.update(footer);
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
fn crc32_record_file(
    _file: &mut File,
    _payload_offset: u64,
    _payload_len: u64,
    _header: &[u8],
    _footer: &[u8],
) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

fn checksum_record_bytes(
    spec: FormatSpec,
    header: &[u8],
    payload: &[u8],
    footer: &[u8],
) -> Result<u32> {
    match spec.integrity_policy {
        IntegrityPolicy::None => Ok(0),
        IntegrityPolicy::Crc32 => crc32_record_bytes(&[], payload, footer),
        IntegrityPolicy::Crc32WithHeader => crc32_record_bytes(header, payload, footer),
    }
}

fn write_matrix_sidecar_file<P: AsRef<Path>>(
    spec: FormatSpec,
    category: &str,
    sidecar_path: P,
    generation: u64,
    payload: &[u8],
    identity: MatrixNativeIdentity,
) -> Result<MatrixSidecarManifest> {
    let payload_len =
        u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
    let format_magic_len =
        u64::try_from(spec.magic.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "matrix sidecar",
        })?;
    let category_len_u64 =
        u64::try_from(category.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "matrix sidecar",
        })?;
    crate::matrix::matrix_sidecar_write_len(
        spec,
        MATRIX_SIDECAR_FIXED_LEN as u64,
        format_magic_len,
        category_len_u64,
        payload_len,
    )?;
    let manifest = matrix_sidecar_manifest_for(spec, category, generation, payload, identity)?;
    let magic_len =
        u16::try_from(manifest.format_magic.len()).map_err(|_| Error::InvalidMatrixSidecar)?;
    let category_len =
        u16::try_from(manifest.category.len()).map_err(|_| Error::InvalidMatrixSidecar)?;
    let header_len = MATRIX_SIDECAR_FIXED_LEN
        .checked_add(magic_len as usize)
        .and_then(|len| len.checked_add(category_len as usize))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "sidecar header length",
        })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(header_len)
        .map_err(|_| Error::AllocationFailed {
            resource: "sidecar header",
            requested: header_len as u64,
        })?;
    bytes.extend_from_slice(MATRIX_SIDECAR_MAGIC);
    bytes.extend_from_slice(&MATRIX_SIDECAR_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&manifest.format_version.to_le_bytes());
    bytes.extend_from_slice(&magic_len.to_le_bytes());
    bytes.extend_from_slice(&category_len.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&manifest.schema_hash.to_le_bytes());
    bytes.extend_from_slice(&manifest.generation.to_le_bytes());
    bytes.extend_from_slice(&manifest.payload_len.to_le_bytes());
    bytes.extend_from_slice(&manifest.payload_crc32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&manifest.native_fingerprint);
    bytes.extend_from_slice(&manifest.matrix_layout_generation.to_le_bytes());
    bytes.extend_from_slice(&manifest.matrix_creation_nonce);
    debug_assert_eq!(bytes.len(), MATRIX_SIDECAR_FIXED_LEN);
    bytes.extend_from_slice(&manifest.format_magic);
    bytes.extend_from_slice(manifest.category.as_bytes());

    // Publish atomically via a same-directory RAII temp: write the full sidecar
    // into a private temp, fsync it, then atomically replace the destination and
    // sync the parent directory. A crash before the replace leaves the previous
    // sidecar untouched; the old in-place `File::create` truncated it eagerly
    // and could expose a partial sidecar (DUR-05).
    let sidecar_path = sidecar_path.as_ref();
    let (temp_path, mut temp_file) = create_rewrite_temp_file(sidecar_path)?;
    let write_result = (|| -> Result<()> {
        temp_file.write_all(&bytes)?;
        temp_file.write_all(payload)?;
        temp_file.flush()?;
        temp_file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        drop(temp_file);
        let _ = remove_file(&temp_path);
        return Err(error);
    }
    drop(temp_file);

    match replace_path_atomically(&temp_path, sidecar_path) {
        Ok(ReplaceDurability::Durable) => Ok(manifest),
        Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
            // The replacement is already visible at `sidecar_path`; only the
            // parent-directory entry's durability is pending. Do not delete the
            // now-published file, and surface the durability gap so a caller
            // that needs a hard guarantee can react (a regenerable sidecar may
            // simply be rewritten).
            Err(Error::PublishedButParentSyncPending {
                path: sidecar_path.display().to_string(),
                source: Box::new(sync_error),
            })
        }
        Err(error) => Err(preserve_temp_on_indeterminate(&temp_path, error)),
    }
}

fn read_matrix_sidecar_file<P: AsRef<Path>>(
    spec: FormatSpec,
    category: &str,
    sidecar_path: P,
    expected_generation: Option<u64>,
    identity: MatrixNativeIdentity,
) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
    let mut file = File::open(sidecar_path)?;
    let file_len = file.metadata()?.len();
    crate::matrix::check_matrix_sidecar_file_len(spec, file_len)?;
    if file_len < MATRIX_SIDECAR_FIXED_LEN as u64 {
        return Err(Error::InvalidMatrixSidecar);
    }
    let mut fixed = [0; MATRIX_SIDECAR_FIXED_LEN];
    file.read_exact(&mut fixed)?;
    if &fixed[0..4] != MATRIX_SIDECAR_MAGIC {
        return Err(Error::InvalidMatrixSidecar);
    }
    let version = u16::from_le_bytes(fixed[4..6].try_into().expect("slice"));
    if version != MATRIX_SIDECAR_VERSION {
        // A different envelope version (legacy v1 lacked native identity,
        // legacy v2 lacked the matrix creation nonce) is refused as stale and
        // regenerable rather than trusted.
        return Err(Error::MatrixSidecarMismatch("sidecar version"));
    }
    let format_version = u16::from_le_bytes(fixed[8..10].try_into().expect("slice"));
    let flags = u16::from_le_bytes(fixed[6..8].try_into().expect("slice"));
    let magic_len = u64::from(u16::from_le_bytes(fixed[10..12].try_into().expect("slice")));
    let category_len = u64::from(u16::from_le_bytes(fixed[12..14].try_into().expect("slice")));
    let reserved = u16::from_le_bytes(fixed[14..16].try_into().expect("slice"));
    let schema_hash = u64::from_le_bytes(fixed[16..24].try_into().expect("slice"));
    let generation = u64::from_le_bytes(fixed[24..32].try_into().expect("slice"));
    let payload_len = u64::from_le_bytes(fixed[32..40].try_into().expect("slice"));
    let payload_crc32 = u32::from_le_bytes(fixed[40..44].try_into().expect("slice"));
    let trailing_reserved = u32::from_le_bytes(fixed[44..48].try_into().expect("slice"));
    let mut native_fingerprint = [0u8; 32];
    native_fingerprint.copy_from_slice(
        &fixed[MATRIX_SIDECAR_FINGERPRINT_OFFSET..MATRIX_SIDECAR_LAYOUT_GENERATION_OFFSET],
    );
    let matrix_layout_generation = u64::from_le_bytes(
        fixed[MATRIX_SIDECAR_LAYOUT_GENERATION_OFFSET..MATRIX_SIDECAR_CREATION_NONCE_OFFSET]
            .try_into()
            .expect("slice"),
    );
    let mut matrix_creation_nonce = [0u8; MATRIX_CREATION_NONCE_LEN];
    matrix_creation_nonce
        .copy_from_slice(&fixed[MATRIX_SIDECAR_CREATION_NONCE_OFFSET..MATRIX_SIDECAR_FIXED_LEN]);
    let plan = crate::matrix::matrix_sidecar_read_plan(
        spec,
        file_len,
        MATRIX_SIDECAR_FIXED_LEN as u64,
        magic_len,
        category_len,
        payload_len,
        flags,
        reserved,
        trailing_reserved,
    )?;
    // DEF-03: only the small fixed header and the bounded magic/category
    // prefix are read before identity validation. A sidecar that belongs to a
    // different native file, schema, category or generation is rejected here,
    // before the payload is ever allocated, read or hashed.
    file.seek(SeekFrom::Start(plan.format_magic_offset))?;
    let mut format_magic = try_alloc_bytes(magic_len, "sidecar format magic")?;
    file.read_exact(&mut format_magic)?;
    file.seek(SeekFrom::Start(plan.category_offset))?;
    let mut category_bytes = try_alloc_bytes(category_len, "sidecar category")?;
    file.read_exact(&mut category_bytes)?;
    let category_text =
        String::from_utf8(category_bytes).map_err(|_| Error::InvalidMatrixSidecar)?;
    let manifest = MatrixSidecarManifest {
        format_magic,
        format_version,
        schema_hash,
        category: category_text,
        generation,
        payload_len,
        payload_crc32,
        native_fingerprint,
        matrix_layout_generation,
        matrix_creation_nonce,
    };
    validate_matrix_sidecar_manifest(spec, category, expected_generation, identity, &manifest)?;
    file.seek(SeekFrom::Start(plan.payload_offset))?;
    let mut payload = try_alloc_bytes(plan.payload_len, "sidecar payload")?;
    file.read_exact(&mut payload)?;
    let actual = crc32_bytes(&payload)?;
    if actual != payload_crc32 {
        return Err(Error::MatrixSidecarChecksumMismatch {
            expected: payload_crc32,
            actual,
        });
    }
    Ok((manifest, payload))
}

fn try_alloc_bytes(len: u64, resource: &'static str) -> Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested: u64::try_from(len).unwrap_or(u64::MAX),
        })?;
    bytes.resize(len, 0);
    Ok(bytes)
}

fn matrix_sidecar_manifest_for(
    spec: FormatSpec,
    category: &str,
    generation: u64,
    payload: &[u8],
    identity: MatrixNativeIdentity,
) -> Result<MatrixSidecarManifest> {
    if category.is_empty() {
        return Err(Error::InvalidMatrixSidecar);
    }
    let mut format_magic = try_alloc_bytes(
        u64::try_from(spec.magic.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "matrix sidecar",
        })?,
        "sidecar format magic",
    )?;
    format_magic.copy_from_slice(spec.magic);
    let category_len =
        u64::try_from(category.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "matrix sidecar",
        })?;
    let mut category_text = String::new();
    category_text
        .try_reserve_exact(category.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "sidecar category",
            requested: category_len,
        })?;
    category_text.push_str(category);
    Ok(MatrixSidecarManifest {
        format_magic,
        format_version: spec.version,
        schema_hash: spec.computed_schema_hash(),
        category: category_text,
        generation,
        payload_len: payload
            .len()
            .try_into()
            .map_err(|_| Error::InvalidMatrixSidecar)?,
        payload_crc32: crc32_bytes(payload)?,
        native_fingerprint: identity.fingerprint,
        matrix_layout_generation: identity.layout_generation,
        matrix_creation_nonce: identity.creation_nonce,
    })
}

fn validate_matrix_sidecar_manifest(
    spec: FormatSpec,
    category: &str,
    expected_generation: Option<u64>,
    identity: MatrixNativeIdentity,
    manifest: &MatrixSidecarManifest,
) -> Result<()> {
    if manifest.format_magic != spec.magic {
        return Err(Error::MatrixSidecarMismatch("format magic"));
    }
    if manifest.format_version != spec.version {
        return Err(Error::MatrixSidecarMismatch("format version"));
    }
    if manifest.schema_hash != spec.computed_schema_hash() {
        return Err(Error::MatrixSidecarMismatch("schema hash"));
    }
    if manifest.category != category {
        return Err(Error::MatrixSidecarMismatch("category"));
    }
    // Bind to a specific native file and its matrix layout: a same-spec,
    // same-generation sibling produces a different OS-object fingerprint and is
    // rejected (DUR-04).
    if manifest.native_fingerprint != identity.fingerprint {
        return Err(Error::MatrixSidecarMismatch("native identity"));
    }
    if manifest.matrix_layout_generation != identity.layout_generation {
        return Err(Error::MatrixSidecarMismatch("matrix layout generation"));
    }
    // Same OS object, schema and layout offsets are not enough: a matrix
    // recreated in the same file object gets a fresh creation nonce, so every
    // sidecar published against the previous logical matrix is refused as
    // stale and regenerable (DUR2-03).
    if manifest.matrix_creation_nonce != identity.creation_nonce {
        return Err(Error::MatrixSidecarMismatch("creation nonce"));
    }
    if let Some(expected_generation) = expected_generation
        && manifest.generation != expected_generation
    {
        return Err(Error::MatrixSidecarMismatch("generation"));
    }
    Ok(())
}

/// Folds the native matrix file's OS-object identity and schema hash into a
/// 32-byte fingerprint.
///
/// Mirrors the scalable sidecar identity machinery
/// (`stream::primary_identity` / `opened_file_identity`), which is compiled
/// only under `high-cardinality-dev`; the matrix module is always built, so the
/// OS-object identity is recomputed here rather than reused across the feature
/// boundary. Uses the CRC helper so the fingerprint honors the same
/// `integrity` gate the rest of the sidecar envelope requires.
fn native_object_fingerprint(spec: FormatSpec, file: &File) -> Result<[u8; 32]> {
    let object_identity = opened_file_identity(file)?;
    let schema_hash = if spec.schema_hash == 0 {
        spec.computed_schema_hash()
    } else {
        spec.schema_hash
    };
    let mut fingerprint = [0u8; 32];
    for lane in 0..8u32 {
        let mut buffer = Vec::with_capacity(4 + 8 + object_identity.len());
        buffer.extend_from_slice(&lane.to_le_bytes());
        buffer.extend_from_slice(&schema_hash.to_le_bytes());
        buffer.extend_from_slice(&object_identity);
        let value = crc32_bytes(&buffer)?;
        fingerprint[(lane as usize) * 4..(lane as usize + 1) * 4]
            .copy_from_slice(&value.to_le_bytes());
    }
    Ok(fingerprint)
}

#[cfg(unix)]
pub(crate) fn opened_file_identity(file: &File) -> Result<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&metadata.dev().to_le_bytes());
    bytes.extend_from_slice(&metadata.ino().to_le_bytes());
    Ok(bytes)
}

#[cfg(windows)]
pub(crate) fn opened_file_identity(file: &File) -> Result<Vec<u8>> {
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

/// A file object held open, together with the OS identity read from that open
/// handle (F-05 follow-up).
///
/// [`opened_file_identity`] is `(st_dev, st_ino)` on Unix and
/// `(volume serial, file index)` on Windows. Neither is a durable name for a
/// file *object*: an inode number is a slot in an allocator, and a filesystem
/// hands a just-freed slot straight back to the next create. Identity bytes
/// copied out of a handle that is then closed therefore say nothing about a
/// later object bearing the same bytes — which is exactly the comparison every
/// identity-checked deletion in this crate was making. Measured on Linux
/// (overlayfs over ext4): unlinking a file and immediately creating another at
/// the same name reproduces the *same* `(dev, ino)`, so the check said "same
/// object" about a file it had never seen.
///
/// An open descriptor pins its inode: the number cannot be handed to another
/// object while any handle on it lives. Holding the handle for as long as the
/// identity is used therefore restores the property the comparison assumes.
/// The invariant this type exists to make structural is:
///
/// > identity bytes are only ever compared while a handle on the object they
/// > were read from is still open.
///
/// The handle is opened read-only and never lent out, so a pin can neither
/// write the object it holds nor be turned into something that can.
#[derive(Debug)]
pub(crate) struct PinnedObject {
    /// Never read through; held open purely so the identity below stays a name
    /// for this object. `_` because that is the whole contract.
    _handle: File,
    identity: Vec<u8>,
}

impl PinnedObject {
    /// Opens `path` read-only and keeps the handle, so the identity read from
    /// it names that object for as long as the returned value lives.
    ///
    /// This is the constructor for callers whose claim is "the object I am
    /// about to delete is the one I just opened at this name" — the lock
    /// marker and the stale sidecar. It cannot prove that the object at the
    /// name is the one the caller created earlier; use [`Self::open_verified`]
    /// when there is an earlier handle to check against.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let handle = open_pin_handle(path)?;
        let identity = opened_file_identity(&handle)?;
        Ok(Self {
            _handle: handle,
            identity,
        })
    }

    /// As [`Self::open`], but refuses unless the object opened is still the one
    /// `expected` names.
    ///
    /// Sound *only* while the caller holds the object `expected` was read from
    /// open — which pins it, so equal identity bytes prove the same object
    /// rather than a recycled slot. The self-test calls this with its writer
    /// still alive, which is what closes the window between creating the
    /// artifact and pinning it.
    pub(crate) fn open_verified(path: &Path, expected: &[u8]) -> Result<Self> {
        let pinned = Self::open(path)?;
        if pinned.identity() != expected {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the pathname stopped naming the expected file object",
            )));
        }
        Ok(pinned)
    }

    /// The identity of the pinned object. Valid as a name for that object only
    /// while `self` lives, which is why it borrows from `self`.
    pub(crate) fn identity(&self) -> &[u8] {
        &self.identity
    }
}

/// Opens the handle a [`PinnedObject`] holds: read-only, and on Unix without
/// following a symlink at the final component, so a pin can never end up
/// holding a file the name merely points at.
#[cfg(unix)]
fn open_pin_handle(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?)
}

/// Windows counterpart. Deliberately *does* follow a reparse point, because the
/// Windows removal arm resolves the same pathname the same way: pinning the
/// link while deleting the target would compare two different objects.
///
/// The default `OpenOptions` share mode is read/write/delete, which is what
/// keeps the pin compatible with the removal that follows it: the removal opens
/// the same object for `DELETE`, which an existing handle only permits if that
/// handle shares deletion, and it opens with `FILE_SHARE_READ`, which only
/// admits existing handles whose granted access is read. A writable pin would
/// fail the second test and turn every cleanup into a sharing violation.
#[cfg(windows)]
fn open_pin_handle(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().read(true).open(path)?)
}

/// RAII owner of a rewrite/publication temp file (STO4-P2).
///
/// [`create_rewrite_temp_file`] hands back a bare `(PathBuf, File)`, so every
/// fallible step between creation and the caller's first cleanup scope is a
/// path that can leak an unpublished temp. This owns the temp from creation
/// instead: any early return - including one from a `?` the author did not
/// think about - unlinks it on drop.
///
/// Cleanup is *given up*, never assumed, through [`RewriteTemp::retain`]:
/// after a successful rename the pathname is already consumed, and after
/// [`Error::ReplacePublicationIndeterminate`] the temp may be the only
/// surviving copy of the new generation and must be preserved for out-of-band
/// reconciliation (DUR2-01). [`RewriteTemp::cleanup_now`] deletes eagerly for
/// callers whose follow-up work (lock-marker removal) is only correct once the
/// temp is gone.
struct RewriteTemp {
    path: PathBuf,
    /// Closed by [`RewriteTemp::close`] before publication; Windows cannot
    /// rename or delete a file through a live handle.
    file: Option<File>,
    armed: bool,
}

impl RewriteTemp {
    fn create(target: &Path) -> Result<Self> {
        let (path, file) = create_rewrite_temp_file(target)?;
        Ok(Self {
            path,
            file: Some(file),
            armed: true,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The open handle. Panics only if called after [`RewriteTemp::close`],
    /// which is a caller bug rather than a runtime condition.
    fn file(&self) -> &File {
        self.file
            .as_ref()
            .expect("rewrite temp handle used after close")
    }

    /// Releases the handle while keeping the file and the cleanup obligation.
    fn close(&mut self) {
        self.file = None;
    }

    /// Closes and deletes the temp now, and gives up the obligation.
    fn cleanup_now(&mut self) {
        self.file = None;
        if self.armed {
            self.armed = false;
            let _ = remove_file(&self.path);
        }
    }

    /// Gives up the cleanup obligation: the temp was published, or is
    /// deliberately preserved.
    fn retain(&mut self) {
        self.armed = false;
    }
}

impl Drop for RewriteTemp {
    fn drop(&mut self) {
        self.file = None;
        if self.armed {
            let _ = remove_file(&self.path);
        }
    }
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    /// Armed post-creation rewrite-temp preparation failures (STO4-P2).
    static INJECTED_REWRITE_TEMP_PREPARATION_FAILURES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Consumes one armed rewrite-temp preparation failure (STO4-P2).
///
/// Models the permission copy that sits between the temp's creation and the
/// publication scope: a real `set_permissions` error cannot be produced on
/// demand on every host, and it was exactly the step whose `?` used to leave an
/// empty temp behind. Inert without the `scalable-fault-injection` feature.
#[inline]
fn take_injected_rewrite_temp_preparation_failure() -> Result<()> {
    #[cfg(feature = "scalable-fault-injection")]
    {
        let armed = INJECTED_REWRITE_TEMP_PREPARATION_FAILURES.with(|count| {
            let current = count.get();
            if current != 0 {
                count.set(current - 1);
            }
            current != 0
        });
        if armed {
            return Err(std::io::Error::other("injected rewrite temp preparation failure").into());
        }
    }
    Ok(())
}

pub(crate) fn create_rewrite_temp_file(path: &Path) -> Result<(PathBuf, File)> {
    let base_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| OsString::from("varve"));
    for attempt in 0..1000u32 {
        let mut file_name = OsString::from(".");
        file_name.push(&base_name);
        file_name.push(format!(".rewrite.{}.{}.tmp", std::process::id(), attempt));
        let temp_path = path
            .parent()
            .map(|parent| parent.join(&file_name))
            .unwrap_or_else(|| PathBuf::from(&file_name));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not create a unique varve rewrite temp file",
    )
    .into())
}

/// Classification of a failed Windows `ReplaceFileW` call (DUR2-01).
///
/// Derived from the documented contract
/// (<https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew>):
/// `ERROR_UNABLE_TO_REMOVE_REPLACED` (1175) leaves the replaced file intact
/// under its original name, `ERROR_UNABLE_TO_MOVE_REPLACEMENT` (1176) and
/// `ERROR_UNABLE_TO_MOVE_REPLACEMENT_2` (1177) can leave the two files'
/// names, streams and attributes partially moved (the target pathname may
/// already have changed), and every other error occurs before any mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplacePublicationFailure {
    /// `ERROR_UNABLE_TO_REMOVE_REPLACED` (1175): the replaced file is intact
    /// under its original name; nothing was mutated.
    ReplacedFileIntact,
    /// Any other documented pre-publication error: both files retain their
    /// original names.
    PrePublication,
    /// 1176/1177: the pathname state is indeterminate. The replacement temp
    /// file must be preserved and a writer bound to the target must not
    /// blindly retry the publication.
    IndeterminatePublication,
}

const ERROR_UNABLE_TO_REMOVE_REPLACED: i32 = 1175;
const ERROR_UNABLE_TO_MOVE_REPLACEMENT: i32 = 1176;
const ERROR_UNABLE_TO_MOVE_REPLACEMENT_2: i32 = 1177;

/// Classifies a raw `ReplaceFileW` OS error per the Microsoft contract.
///
/// Pure and platform-independent so the classification is unit-testable on
/// every host; only the Windows publication path feeds it live errors.
pub fn classify_replace_publication_error(raw_os_error: i32) -> ReplacePublicationFailure {
    match raw_os_error {
        ERROR_UNABLE_TO_REMOVE_REPLACED => ReplacePublicationFailure::ReplacedFileIntact,
        ERROR_UNABLE_TO_MOVE_REPLACEMENT | ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 => {
            ReplacePublicationFailure::IndeterminatePublication
        }
        _ => ReplacePublicationFailure::PrePublication,
    }
}

/// Outcome of a pathname publication whose rename step succeeded.
///
/// `Err` from [`replace_path_atomically`] means the publication itself failed.
/// With one exception the target pathname still resolves to the previous
/// generation and the caller may delete its temp file; the exception is
/// [`Error::ReplacePublicationIndeterminate`], after which the pathname state
/// is unknown, the replacement temp file must be preserved for reconciliation
/// and any writer bound to the target must be poisoned instead of retrying
/// (DUR2-01). Once the rename has happened the target is already the new
/// generation, so a later durability failure must not be reported as a plain
/// error: callers must keep operating on the published generation (rebind or
/// poison the writer) and surface [`Error::PublishedButParentSyncPending`]
/// instead.
#[derive(Debug)]
pub(crate) enum ReplaceDurability {
    /// The replacement is visible at the target path and the parent-directory
    /// entry was synced.
    Durable,
    /// The replacement is visible at the target path, but the parent-directory
    /// sync failed, so the rename is not yet guaranteed to survive power loss.
    ParentSyncPending(Error),
}

/// Publishes a [`tempfile::TempPath`]-guarded replacement at `target` via
/// [`replace_path_atomically`], taking ownership of the RAII guard so its
/// pathname deletion can be disarmed when the publication outcome is
/// indeterminate.
///
/// After [`Error::ReplacePublicationIndeterminate`] the replacement temp may
/// be the only surviving copy of the new generation and must be preserved for
/// out-of-band reconciliation, so the guard is disarmed before the error
/// propagates (DUR2-01). Every other `Err` is a documented pre-publication
/// failure ??the target pathname is untouched ??so dropping the guard deletes
/// the unpublished temp as usual; on `Ok` the rename already consumed the temp
/// pathname and the guard's drop is a no-op.
#[cfg(feature = "high-cardinality-dev")]
pub(crate) fn publish_temp_path_atomically(
    temporary: tempfile::TempPath,
    target: &Path,
) -> Result<ReplaceDurability> {
    match replace_path_atomically(&temporary, target) {
        Err(error) => {
            if matches!(error, Error::ReplacePublicationIndeterminate { .. }) {
                // `keep` only forgets the guard; if it ever reports an error
                // it hands the guard back, so forget it manually rather than
                // letting the drop delete the replacement we must preserve.
                if let Err(persist_error) = temporary.keep() {
                    std::mem::forget(persist_error.path);
                }
            }
            Err(error)
        }
        published => published,
    }
}

#[cfg(not(windows))]
pub(crate) fn replace_path_atomically(
    replacement: &Path,
    target: &Path,
) -> Result<ReplaceDurability> {
    #[cfg(feature = "scalable-fault-injection")]
    take_injected_replace_indeterminate(replacement, target)?;
    crate::scalable_fault_point("replace.atomic");
    let replace = std::fs::rename(replacement, target);
    crate::scalable_fault_point("replace.atomic");
    replace?;
    match sync_parent_directory(target) {
        Ok(()) => Ok(ReplaceDurability::Durable),
        Err(error) => Ok(ReplaceDurability::ParentSyncPending(error)),
    }
}

#[cfg(windows)]
pub(crate) fn replace_path_atomically(
    replacement: &Path,
    target: &Path,
) -> Result<ReplaceDurability> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    #[cfg(feature = "scalable-fault-injection")]
    take_injected_replace_indeterminate(replacement, target)?;

    if !target.exists() {
        crate::scalable_fault_point("replace.atomic");
        let replace = std::fs::rename(replacement, target);
        crate::scalable_fault_point("replace.atomic");
        replace?;
        return match sync_parent_directory(target) {
            Ok(()) => Ok(ReplaceDurability::Durable),
            Err(error) => Ok(ReplaceDurability::ParentSyncPending(error)),
        };
    }

    // Captured before the call so an indeterminate 1176/1177 failure can be
    // reconciled cheaply afterwards: if the target pathname then resolves to
    // this exact OS object, the replacement de facto owns the target name.
    let replacement_identity = File::open(replacement)
        .ok()
        .and_then(|file| opened_file_identity(&file).ok());

    let replacement_wide: Vec<u16> = replacement
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let target_wide: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    crate::scalable_fault_point("replace.atomic");
    let ok = unsafe {
        ReplaceFileW(
            target_wide.as_ptr(),
            replacement_wide.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    crate::scalable_fault_point("replace.atomic");
    if ok == 0 {
        let os_error = std::io::Error::last_os_error();
        return match classify_replace_publication_error(os_error.raw_os_error().unwrap_or(0)) {
            // 1175 is a documented no-mutation outcome and every other code is
            // a documented pre-publication failure: both names are unchanged,
            // so the caller may treat this as "target untouched" and delete
            // its temp file.
            ReplacePublicationFailure::ReplacedFileIntact
            | ReplacePublicationFailure::PrePublication => Err(os_error.into()),
            ReplacePublicationFailure::IndeterminatePublication => {
                if replacement_identity
                    .as_deref()
                    .is_some_and(|identity| path_resolves_to_object(target, identity))
                {
                    // Reconciled: the replacement object now owns the target
                    // pathname, so the publication effectively happened.
                    match sync_parent_directory(target) {
                        Ok(()) => Ok(ReplaceDurability::Durable),
                        Err(error) => Ok(ReplaceDurability::ParentSyncPending(error)),
                    }
                } else {
                    // Unreconciled: names may be partially moved. The caller
                    // must preserve the replacement temp file and poison any
                    // writer bound to the target (DUR2-01).
                    Err(Error::ReplacePublicationIndeterminate {
                        path: target.display().to_string(),
                        replacement: replacement.display().to_string(),
                        source: os_error,
                    })
                }
            }
        };
    }
    match sync_parent_directory(target) {
        Ok(()) => Ok(ReplaceDurability::Durable),
        Err(error) => Ok(ReplaceDurability::ParentSyncPending(error)),
    }
}

/// Returns whether `path` currently resolves to the OS file object with the
/// given identity bytes. Any open or identity failure counts as "no".
#[cfg(windows)]
fn path_resolves_to_object(path: &Path, identity: &[u8]) -> bool {
    File::open(path)
        .ok()
        .and_then(|file| opened_file_identity(&file).ok())
        .is_some_and(|current| current == identity)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<()> {
    #[cfg(test)]
    record_parent_directory_sync();
    #[cfg(test)]
    fail_parent_sync_if_requested()?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    crate::scalable_fault_point("replace.parent_sync");
    #[cfg(feature = "scalable-fault-injection")]
    take_injected_parent_sync_failure()?;
    let sync = File::open(parent).and_then(|directory| directory.sync_all());
    crate::scalable_fault_point("replace.parent_sync");
    sync?;
    Ok(())
}

#[cfg(windows)]
fn sync_parent_directory(path: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    #[cfg(test)]
    record_parent_directory_sync();
    #[cfg(test)]
    fail_parent_sync_if_requested()?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    // DUR2-02: FlushFileBuffers requires GENERIC_WRITE on the handle
    // (https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers),
    // so the directory must be opened with write access; a read-only directory
    // handle fails the flush with ERROR_ACCESS_DENIED on NTFS and the earlier
    // code silently promoted that refusal to `Durable`. Any open or flush
    // failure now propagates so the caller reports `ParentSyncPending`
    // instead of a false durability claim.
    let directory = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(parent)?;
    crate::scalable_fault_point("replace.parent_sync");
    #[cfg(feature = "scalable-fault-injection")]
    take_injected_parent_sync_failure()?;
    let sync = directory.sync_all();
    crate::scalable_fault_point("replace.parent_sync");
    sync?;
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
static INJECTED_PARENT_SYNC_FAILURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "scalable-fault-injection")]
fn take_injected_parent_sync_failure() -> Result<()> {
    use std::sync::atomic::Ordering;

    let mut current = INJECTED_PARENT_SYNC_FAILURES.load(Ordering::Acquire);
    while current != 0 {
        match INJECTED_PARENT_SYNC_FAILURES.compare_exchange(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                return Err(std::io::Error::other("injected parent-directory sync failure").into());
            }
            Err(observed) => current = observed,
        }
    }
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
static INJECTED_REPLACE_INDETERMINATE_FAILURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "scalable-fault-injection")]
fn take_injected_replace_indeterminate(replacement: &Path, target: &Path) -> Result<()> {
    use std::sync::atomic::Ordering;

    let mut current = INJECTED_REPLACE_INDETERMINATE_FAILURES.load(Ordering::Acquire);
    while current != 0 {
        match INJECTED_REPLACE_INDETERMINATE_FAILURES.compare_exchange(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // The publication is not attempted, so from the caller's
                // perspective this behaves exactly like an unreconciled
                // 1176/1177: the pathname state is unknown and the temp file
                // still exists.
                return Err(Error::ReplacePublicationIndeterminate {
                    path: target.display().to_string(),
                    replacement: replacement.display().to_string(),
                    source: std::io::Error::other("injected indeterminate replacement failure"),
                });
            }
            Err(observed) => current = observed,
        }
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static PARENT_DIRECTORY_SYNC_CALLS: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
fn record_parent_directory_sync() {
    PARENT_DIRECTORY_SYNC_CALLS.set(PARENT_DIRECTORY_SYNC_CALLS.get() + 1);
}

#[cfg(test)]
fn reset_parent_directory_sync_calls() {
    PARENT_DIRECTORY_SYNC_CALLS.set(0);
}

#[cfg(test)]
fn parent_directory_sync_calls() -> u64 {
    PARENT_DIRECTORY_SYNC_CALLS.get()
}

#[cfg(feature = "integrity")]
fn crc32_bytes(payload: &[u8]) -> Result<u32> {
    Ok(crc32fast::hash(payload))
}

#[cfg(not(feature = "integrity"))]
fn crc32_bytes(_payload: &[u8]) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

#[cfg(feature = "integrity")]
fn crc32_record_bytes(header: &[u8], payload: &[u8], footer: &[u8]) -> Result<u32> {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(header);
    hasher.update(payload);
    hasher.update(footer);
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
fn crc32_record_bytes(_header: &[u8], _payload: &[u8], _footer: &[u8]) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

/// Opens the diagnostic `<target>.lock` marker for mutation, refusing any
/// object that is not a dedicated, unaliased regular file (F-07).
///
/// Acquiring the marker truncates it and writes pid/target metadata through
/// the handle, and dropping the lock truncates it again. Those writes must
/// never reach an object the marker path merely *points at*. Two aliasing
/// shapes are refused, on both platforms:
///
/// * a final-component symbolic link / reparse point, which would redirect the
///   writes to an arbitrary target. Unix refuses at `open` with `O_NOFOLLOW`;
///   Windows opens the reparse point itself with `FILE_FLAG_OPEN_REPARSE_POINT`
///   (so no write can reach the target) and then rejects it by attribute.
/// * a hard link, i.e. a marker object with more than one name. The extra name
///   is another path whose content this truncation would destroy.
///
/// Anything that is not a regular file (a directory, or on Unix a device or
/// FIFO) is refused for the same reason.
///
/// This is hardening of a *diagnostic* object only. Authoritative
/// single-writer exclusion is a native lock on the target file object
/// ([`probe_native_target_lock`]) and never depended on the marker, so a
/// refusal here removes no exclusion strength - it converts a silent foreign
/// write into [`Error::WriterLockMarkerNotDedicated`].
fn open_dedicated_lock_marker(path: &Path) -> Result<File> {
    let file = open_lock_marker_without_following(path)?;
    verify_dedicated_lock_marker(path, &file)?;
    Ok(file)
}

#[cfg(unix)]
fn open_lock_marker_without_following(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => Ok(file),
        // `O_NOFOLLOW` on a symlinked final component reports ELOOP on Linux
        // and EMLINK on some BSDs. Both mean the same thing here.
        Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP) | Some(libc::EMLINK)) => {
            Err(Error::WriterLockMarkerNotDedicated {
                path: path.display().to_string(),
                reason: "the final path component is a symbolic link",
            })
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn open_lock_marker_without_following(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    // Opening the reparse point itself rather than its target guarantees that
    // no write performed through this handle can reach the target, whatever
    // `verify_dedicated_lock_marker` then decides. The flag is ignored for an
    // ordinary file and for the creating case.
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?)
}

#[cfg(not(any(unix, windows)))]
fn open_lock_marker_without_following(path: &Path) -> Result<File> {
    // INVARIANT 5: refuse explicitly rather than silently offering weaker
    // protection on a platform where neither guarantee can be expressed.
    let _ = path;
    Err(Error::WriterLockMarkerNotDedicated {
        path: path.display().to_string(),
        reason: "this platform cannot open the marker without following links",
    })
}

#[cfg(unix)]
fn verify_dedicated_lock_marker(path: &Path, file: &File) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(Error::WriterLockMarkerNotDedicated {
            path: path.display().to_string(),
            reason: "the marker path does not name a regular file",
        });
    }
    if metadata.nlink() != 1 {
        return Err(Error::WriterLockMarkerNotDedicated {
            path: path.display().to_string(),
            reason: "the marker object has more than one hard link",
        });
    }
    Ok(())
}

#[cfg(windows)]
fn verify_dedicated_lock_marker(path: &Path, file: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        GetFileInformationByHandle,
    };

    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // The handle belongs to `file`, the output points to initialized writable
    // storage, and the OS call does not outlive either value.
    let ok =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, information.as_mut_ptr()) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // A successful call initializes every field of BY_HANDLE_FILE_INFORMATION.
    let information = unsafe { information.assume_init() };
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(Error::WriterLockMarkerNotDedicated {
            path: path.display().to_string(),
            reason: "the final path component is a reparse point",
        });
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(Error::WriterLockMarkerNotDedicated {
            path: path.display().to_string(),
            reason: "the marker path does not name a regular file",
        });
    }
    // `nNumberOfLinks` is supported by NTFS and ReFS; filesystems that cannot
    // report it (some network redirectors) return 1, which is the same answer
    // an unaliased file gives, so this check never becomes stricter than the
    // filesystem can substantiate.
    if information.nNumberOfLinks != 1 {
        return Err(Error::WriterLockMarkerNotDedicated {
            path: path.display().to_string(),
            reason: "the marker object has more than one hard link",
        });
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn verify_dedicated_lock_marker(path: &Path, _file: &File) -> Result<()> {
    Err(Error::WriterLockMarkerNotDedicated {
        path: path.display().to_string(),
        reason: "this platform cannot prove the marker is a dedicated regular file",
    })
}

#[derive(Debug)]
pub(crate) struct WriterLock {
    file: File,
    /// Pathname of `file`, kept so a release can remove the marker object when
    /// truncating its content cannot be proved to have worked. See
    /// [`clear_and_verify_writer_lock_marker`].
    marker_path: PathBuf,
    // Authoritative single-writer lock, held on the native file object itself.
    // Hard links and other path aliases all resolve to the same object, so an
    // object lock cannot be bypassed the way the path-derived ".lock" marker
    // can. The marker file above remains diagnostic metadata (pid, timestamps,
    // break-policy machinery) plus a fast same-path exclusion.
    native_guard: Option<File>,
    /// Set by [`WriterLock::release`] so the drop that follows it does not
    /// repeat work that was already reported on.
    released: bool,
}

impl WriterLock {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        Self::acquire_with_policy(path, WriterLockBreakPolicy::Refuse)
    }

    pub(crate) fn acquire_with_policy(
        target_path: &Path,
        policy: WriterLockBreakPolicy,
    ) -> Result<Self> {
        let path = lock_path(target_path);
        // F-07: acquisition truncates and rewrites this object, so it must be
        // a dedicated, unaliased regular file before anything is mutated.
        let mut file = open_dedicated_lock_marker(&path)?;
        match try_lock_writer_guard(&file) {
            Ok(()) => {}
            Err(WriterGuardLockError::WouldBlock) => {
                return match policy {
                    WriterLockBreakPolicy::Refuse => {
                        Err(Error::WriterLockHeld(path.display().to_string()))
                    }
                    _ => Err(Error::WriterLockBreakRefused(path.display().to_string())),
                };
            }
            Err(WriterGuardLockError::Io(error)) => return Err(error.into()),
        }

        let marker_present = file.metadata()?.len() != 0;
        if marker_present && policy == WriterLockBreakPolicy::Refuse {
            return Err(Error::WriterLockHeld(path.display().to_string()));
        }
        if marker_present {
            let info = read_writer_lock_info_file(&path, &mut file)?
                .ok_or_else(|| Error::WriterLockMalformed(path.display().to_string()))?;
            if !should_break_writer_lock(&info, policy) {
                return Err(Error::WriterLockBreakRefused(path.display().to_string()));
            }
        }

        // An object lock held by a live writer can never be broken: the OS
        // releases it only when that writer's handles close, so a conflict here
        // always means an active writer, regardless of the marker break policy.
        let native_guard = probe_native_target_lock(target_path)?;

        let info = WriterLockInfo {
            path: path.clone(),
            target_path: absolute_target_path(target_path)?,
            process_id: std::process::id(),
            created_unix_ms: unix_time_ms(),
        };
        if let Err(error) = write_writer_lock_info(&mut file, &info) {
            let _ = clear_and_verify_writer_lock_marker(&mut file, &path);
            return Err(error);
        }
        Ok(Self {
            file,
            marker_path: path,
            native_guard,
            released: false,
        })
    }

    /// Gives back both things an acquisition took, in that order, and reports a
    /// failure instead of discarding it.
    ///
    /// An acquisition takes the authoritative object lock on the target and the
    /// marker's content, and a *live* writer keeps both on purpose. Anything
    /// that acquired the lock and then failed - a refused open, a rejected
    /// header, a corrupt tail - never became a writer and must keep neither, or
    /// the next open is refused for a claim nobody holds.
    ///
    /// Two properties this has that [`Drop`] alone cannot:
    ///
    /// * it **reports**. `Drop` discarded `clear_writer_lock_info`'s error, so a
    ///   marker whose truncation failed became a silent `Error::WriterLockHeld`
    ///   on the next open, with nothing pointing at the cause.
    /// * it is **ordered**: the object lock goes first. Clearing the marker
    ///   first publishes "no writer here" while this process still holds the
    ///   object lock, so a competing writer that reads the marker inside that
    ///   window is refused by a lock that is already being given up.
    pub(crate) fn release(mut self) -> Result<()> {
        self.give_back_native_guard();
        let result = clear_and_verify_writer_lock_marker(&mut self.file, &self.marker_path);
        // Last, because clearing the marker is only safe while its own lock is
        // still held: see `clear_and_verify_writer_lock_marker`.
        let _ = unlock_writer_guard(&self.file);
        self.released = true;
        result
    }

    /// Releases the authoritative object lock, completely and now.
    ///
    /// `self.native_guard = None` was not enough: on Unix the guard is a `dup`
    /// of the writer's own handle, so dropping it leaves the lock held by the
    /// shared open file description, and any concurrently forked child holds
    /// that description too until it `exec`s. See [`unlock_writer_guard`].
    fn give_back_native_guard(&mut self) {
        if let Some(guard) = self.native_guard.take() {
            let _ = unlock_native_guard(&guard);
        }
    }

    /// Moves the authoritative object lock onto the writer's own native handle.
    ///
    /// Writers must call this on the handle they keep open for their lifetime:
    /// once for a freshly created file (the acquire-time probe cannot lock a
    /// file that does not exist yet) and again whenever a publication rebinds
    /// the writer to a new file generation. Dropping the previous guard first
    /// is required because two exclusive range locks on the same file object
    /// conflict even within one process; a competing writer that wins the
    /// resulting microscopic window makes this call fail, which aborts the
    /// caller instead of ever admitting two writers.
    pub(crate) fn bind_native(&mut self, native: &File, target_path: &Path) -> Result<()> {
        // Unlock, not merely drop: a dropped duplicate keeps the lock alive on
        // Unix, which would make the re-lock below conflict with the guard it
        // is replacing rather than with a competing writer.
        self.give_back_native_guard();
        let guard = native.try_clone()?;
        match try_lock_native_guard(&guard) {
            Ok(()) => {
                self.native_guard = Some(guard);
                Ok(())
            }
            Err(WriterGuardLockError::WouldBlock) => {
                Err(Error::WriterLockHeld(target_path.display().to_string()))
            }
            Err(WriterGuardLockError::Io(error)) => Err(error.into()),
        }
    }
}

/// Locks the native file object behind `target_path` if the file exists.
///
/// A missing file has no aliases, so there is nothing to lock yet; creators
/// call [`WriterLock::bind_native`] on the handle they create instead.
fn probe_native_target_lock(target_path: &Path) -> Result<Option<File>> {
    let native = match OpenOptions::new().read(true).open(target_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match try_lock_native_guard(&native) {
        Ok(()) => Ok(Some(native)),
        Err(WriterGuardLockError::WouldBlock) => {
            Err(Error::WriterLockHeld(target_path.display().to_string()))
        }
        Err(WriterGuardLockError::Io(error)) => Err(error.into()),
    }
}

enum WriterGuardLockError {
    WouldBlock,
    Io(std::io::Error),
}

#[cfg(not(windows))]
fn try_lock_writer_guard(file: &File) -> std::result::Result<(), WriterGuardLockError> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(WriterGuardLockError::WouldBlock),
        Err(std::fs::TryLockError::Error(error)) => Err(WriterGuardLockError::Io(error)),
    }
}

#[cfg(windows)]
fn try_lock_writer_guard(file: &File) -> std::result::Result<(), WriterGuardLockError> {
    try_lock_exclusive_range(file, WRITER_LOCK_MAX_LEN)
}

// Advisory whole-file lock on Unix; both native and marker guards share it.
#[cfg(not(windows))]
fn try_lock_native_guard(file: &File) -> std::result::Result<(), WriterGuardLockError> {
    try_lock_writer_guard(file)
}

// Windows range locks are mandatory, so the native guard must live at an
// offset no real data access can ever overlap. Record extents are bounded by
// the file length, which can never reach this reserved offset, so ordinary
// readers and the writer's own data I/O are unaffected.
#[cfg(windows)]
const NATIVE_WRITER_GUARD_OFFSET: u64 = u64::MAX - 1;

#[cfg(windows)]
fn try_lock_native_guard(file: &File) -> std::result::Result<(), WriterGuardLockError> {
    try_lock_exclusive_range(file, NATIVE_WRITER_GUARD_OFFSET)
}

/// Gives a writer guard back *explicitly*, instead of letting the handle's close
/// do it.
///
/// Closing is not equivalent on Unix. `flock` binds the lock to the open file
/// description, not to the descriptor, and a description outlives the
/// descriptor that created it whenever any duplicate remains open - including
/// duplicates this process cannot see or reach:
///
/// * `File::try_clone` is `dup`, so the native guard shares one description
///   with the writer's own data handle. Dropping the guard alone releases
///   nothing while that handle lives.
/// * a `fork` anywhere in the process (every `std::process::Command::spawn` is
///   one) duplicates every open descriptor into the child. The child's copies
///   are `O_CLOEXEC` and so close at `exec`, but until they do, the child holds
///   descriptions this process has already dropped - and the lock on them.
///   Releasing by close therefore does not take effect when the release
///   happens, but at some unrelated later instant in another process. That is
///   how a released writer lock came back as [`Error::WriterLockHeld`] for a
///   claim nobody held: `flock` reported `EAGAIN` with no lock owner anywhere
///   in this process.
///
/// `LOCK_UN` releases the description's lock for every descriptor that refers
/// to it, in this process and in any pre-`exec` child, so it is the only
/// release that is complete at the moment it returns. Windows range locks are
/// per-handle and are dropped at close, but unlocking the exact range first is
/// the same statement made promptly, and `ERROR_NOT_LOCKED` on an
/// already-released range is ignorable either way.
#[cfg(not(windows))]
fn unlock_writer_guard(file: &File) -> std::io::Result<()> {
    file.unlock()
}

#[cfg(windows)]
fn unlock_writer_guard(file: &File) -> std::io::Result<()> {
    unlock_exclusive_range(file, WRITER_LOCK_MAX_LEN)
}

#[cfg(not(windows))]
fn unlock_native_guard(file: &File) -> std::io::Result<()> {
    file.unlock()
}

#[cfg(windows)]
fn unlock_native_guard(file: &File) -> std::io::Result<()> {
    unlock_exclusive_range(file, NATIVE_WRITER_GUARD_OFFSET)
}

#[cfg(windows)]
fn unlock_exclusive_range(file: &File, offset: u64) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0_0};

    let mut overlapped = OVERLAPPED::default();
    overlapped.Anonymous.Anonymous = OVERLAPPED_0_0 {
        Offset: offset as u32,
        OffsetHigh: (offset >> 32) as u32,
    };
    // SAFETY: mirrors `try_lock_exclusive_range` - `file` is open for the
    // duration, the OVERLAPPED value is initialized for the same synchronous
    // one-byte range, and no pointer outlives the call.
    let unlocked =
        unsafe { UnlockFileEx(file.as_raw_handle() as HANDLE, 0, 1, 0, &mut overlapped) };
    if unlocked != 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

#[cfg(windows)]
fn try_lock_exclusive_range(
    file: &File,
    offset: u64,
) -> std::result::Result<(), WriterGuardLockError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0_0};

    let mut overlapped = OVERLAPPED::default();
    overlapped.Anonymous.Anonymous = OVERLAPPED_0_0 {
        Offset: offset as u32,
        OffsetHigh: (offset >> 32) as u32,
    };
    // SAFETY: `file` remains open for the lifetime of the acquired lock, the
    // OVERLAPPED value is initialized for a synchronous one-byte range lock,
    // and the pointer is valid for the duration of this call.
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if locked != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Err(WriterGuardLockError::WouldBlock)
    } else {
        Err(WriterGuardLockError::Io(error))
    }
}

/// Clears stale writer-lock metadata without opening or scanning the target file.
///
/// The policy is always explicit. `Refuse` preserves the default single-writer
/// behavior, while process-aware policies only break a lock after their
/// predicate succeeds. An OS-level exclusive lock is acquired before metadata
/// is inspected or changed, so an active or concurrently recovering writer
/// cannot be displaced.
pub fn clear_stale_writer_lock(
    target_path: impl AsRef<Path>,
    policy: WriterLockBreakPolicy,
) -> Result<()> {
    // `release` is the reporting route: it clears the marker, proves it clear,
    // and drops the object lock first, so this cannot return `Ok(())` over a
    // marker that is still refusing.
    WriterLock::acquire_with_policy(target_path.as_ref(), policy)?.release()
}

/// Releases a writer lock that an open acquired and then could not use, and
/// returns the error to report.
///
/// An open that fails never became a writer, so the claim it took has to be
/// given back before the error leaves the function - not at some later scope
/// exit, and not silently. The original failure is what the caller asked about
/// and is what propagates; a release failure replaces it, because a stranded
/// claim is the more serious news and is the thing that would otherwise resurface
/// as `Error::WriterLockHeld` on a file nobody holds.
/// Runs `body` while holding `lock`, and gives the claim back explicitly if it
/// fails.
///
/// `WriterLock` releases on drop, so the OS-level guard comes back either way.
/// What drop cannot do is *report*, and the marker is the half that matters
/// here: `Drop` clears its contents on a best-effort basis and discards the
/// result, so a clear that failed left a non-empty marker behind and the next
/// acquisition refused with `WriterLockHeld` under the default
/// [`WriterLockBreakPolicy::Refuse`] — a live writer and a failed open became
/// indistinguishable. That is what made a failed open poison the pathname.
///
/// On success the lock is handed to the caller inside the value it built, whose
/// own drop owns it from then on. On failure it is released here, and a release
/// error replaces the original: a caller that is told the open failed can retry,
/// but a caller told only the *first* reason while the pathname is still claimed
/// would retry forever.
/// `body` builds the file while borrowing the lock and leaves `_lock` empty;
/// this installs the lock on success, so the claim lives exactly as long as the
/// handle. Deliberately not generic and deliberately not `mem::forget`: the
/// borrow means `body` cannot take ownership, and forgetting the lock here would
/// hold the OS guard and the marker for the life of the process.
fn with_writer_lock(
    lock: WriterLock,
    body: impl FnOnce(&mut WriterLock) -> Result<VarveFile>,
) -> Result<VarveFile> {
    let (mut file, lock) = with_writer_lock_value(lock, body)?;
    debug_assert!(
        file._lock.is_none(),
        "the body under `with_writer_lock` must leave `_lock` empty; \
         installing it twice would drop the first claim early"
    );
    file._lock = Some(lock);
    Ok(file)
}

/// The same lifecycle for anything an acquisition builds that is not a
/// [`VarveFile`].
///
/// [`with_writer_lock`] can install the claim itself because it knows the one
/// field to put it in. The layout writer and the recovering open build other
/// shapes - a different struct, a `(file, report)` pair - so they get the lock
/// handed back on success and install it themselves. What matters is identical:
/// `body` only *borrows* the lock, so it cannot take ownership and cannot leak
/// it, and a failure releases here with a report rather than at scope exit in
/// silence.
pub(crate) fn with_writer_lock_value<T>(
    mut lock: WriterLock,
    body: impl FnOnce(&mut WriterLock) -> Result<T>,
) -> Result<(T, WriterLock)> {
    match body(&mut lock) {
        Ok(value) => Ok((value, lock)),
        Err(error) => Err(release_writer_lock_after_failure(lock, error)),
    }
}

fn release_writer_lock_after_failure(lock: WriterLock, error: Error) -> Error {
    match lock.release() {
        Ok(()) => error,
        Err(release_error) => release_error,
    }
}

fn write_writer_lock_info(file: &mut File, info: &WriterLockInfo) -> Result<()> {
    let target = info.target_path.to_string_lossy();
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(
        format!(
            "{WRITER_LOCK_MAGIC}\npid={}\ncreated_unix_ms={}\ntarget={target}\n",
            info.process_id, info.created_unix_ms
        )
        .as_bytes(),
    )?;
    file.flush()?;
    Ok(())
}

fn clear_writer_lock_info(file: &mut File) -> Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.flush()?;
    Ok(())
}

/// Clears the marker's content and *proves* it is gone, removing the marker
/// object if it is not.
///
/// The next acquisition decides from one observation only -
/// `file.metadata()?.len() != 0` in [`WriterLock::acquire_with_policy`] - and
/// under the default `WriterLockBreakPolicy::Refuse` a non-zero length is
/// `Error::WriterLockHeld` without the content ever being read. So a release
/// whose truncation silently did not take does not merely lose a diagnostic: it
/// leaves the file unopenable by the default policy until someone runs
/// `clear_stale_writer_lock`. Checking the length afterwards is what makes the
/// truncation's success a fact rather than an assumption.
///
/// When the length is still non-zero the marker object is removed instead.
/// That is safe precisely here and only here: the caller still holds the
/// marker's own OS lock, so no acquisition can be part-way through reading it,
/// and a caller that is releasing has no writer role left for the marker to
/// describe. Removal is a fallback, not the normal path - a marker file that
/// truncates correctly is left in place, zero length, exactly as before - so
/// nothing that depends on the marker surviving normal use changes.
fn clear_and_verify_writer_lock_marker(file: &mut File, path: &Path) -> Result<()> {
    let cleared = clear_writer_lock_info(file);
    // `u64::MAX` for an unreadable length: unprovable is treated as non-empty,
    // never as empty.
    let observed = file
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(u64::MAX);
    if observed == 0 {
        return Ok(());
    }
    remove_file(path)?;
    // The removal made the marker inert, which is what the next acquisition
    // reads; report the truncation failure only if it did not already succeed.
    cleared
}

fn read_writer_lock_info(target_path: &Path) -> Result<Option<WriterLockInfo>> {
    let path = lock_path(target_path);
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Error::WriterLockMalformed(path.display().to_string())),
    };
    read_writer_lock_info_file(&path, &mut file)
}

fn read_writer_lock_info_file(path: &Path, file: &mut File) -> Result<Option<WriterLockInfo>> {
    let len = file
        .metadata()
        .map_err(|_| Error::WriterLockMalformed(path.display().to_string()))?
        .len();
    if len == 0 {
        return Ok(None);
    }
    if len > WRITER_LOCK_MAX_LEN {
        return Err(Error::WriterLockMalformed(path.display().to_string()));
    }
    let capacity =
        usize::try_from(len).map_err(|_| Error::WriterLockMalformed(path.display().to_string()))?;
    let mut contents = String::new();
    contents
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource: "writer lock metadata",
            requested: len,
        })?;
    file.seek(SeekFrom::Start(0))
        .map_err(|_| Error::WriterLockMalformed(path.display().to_string()))?;
    file.take(WRITER_LOCK_MAX_LEN.saturating_add(1))
        .read_to_string(&mut contents)
        .map_err(|_| Error::WriterLockMalformed(path.display().to_string()))?;
    if contents.len() as u64 > WRITER_LOCK_MAX_LEN {
        return Err(Error::WriterLockMalformed(path.display().to_string()));
    }
    parse_writer_lock_info(path, &contents).map(Some)
}

fn parse_writer_lock_info(path: &Path, contents: &str) -> Result<WriterLockInfo> {
    let mut lines = contents.lines();
    if lines.next() != Some(WRITER_LOCK_MAGIC) {
        return Err(Error::WriterLockMalformed(path.display().to_string()));
    }
    let mut process_id = None;
    let mut created_unix_ms = None;
    let mut target_path = None;
    for line in lines {
        let Some((key, value)) = line.split_once('=') else {
            return Err(Error::WriterLockMalformed(path.display().to_string()));
        };
        match key {
            "pid" => {
                process_id = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| Error::WriterLockMalformed(path.display().to_string()))?,
                );
            }
            "created_unix_ms" => {
                created_unix_ms = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| Error::WriterLockMalformed(path.display().to_string()))?,
                );
            }
            "target" => {
                if value.is_empty() {
                    return Err(Error::WriterLockMalformed(path.display().to_string()));
                }
                target_path = Some(PathBuf::from(value));
            }
            _ => return Err(Error::WriterLockMalformed(path.display().to_string())),
        }
    }
    Ok(WriterLockInfo {
        path: path.to_path_buf(),
        target_path: target_path
            .ok_or_else(|| Error::WriterLockMalformed(path.display().to_string()))?,
        process_id: process_id
            .ok_or_else(|| Error::WriterLockMalformed(path.display().to_string()))?,
        created_unix_ms: created_unix_ms
            .ok_or_else(|| Error::WriterLockMalformed(path.display().to_string()))?,
    })
}

fn should_break_writer_lock(info: &WriterLockInfo, policy: WriterLockBreakPolicy) -> bool {
    match policy {
        WriterLockBreakPolicy::Refuse => false,
        WriterLockBreakPolicy::BreakIfProcessAbsent => process_is_absent(info.process_id),
        WriterLockBreakPolicy::BreakIfOlderThan(age) => lock_is_older_than(info, age),
        WriterLockBreakPolicy::BreakIfProcessAbsentAndOlderThan(age) => {
            process_is_absent(info.process_id) && lock_is_older_than(info, age)
        }
    }
}

fn lock_is_older_than(info: &WriterLockInfo, age: Duration) -> bool {
    let now = unix_time_ms();
    now.saturating_sub(info.created_unix_ms) as u128 >= age.as_millis()
}

fn unix_time_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn absolute_target_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(windows)]
fn process_is_absent(process_id: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    if process_id == std::process::id() {
        return false;
    }
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
            0,
            process_id,
        )
    };
    if handle.is_null() {
        return std::io::Error::last_os_error().raw_os_error()
            == Some(ERROR_INVALID_PARAMETER as i32);
    }
    // A terminated Windows process remains openable while any process still
    // holds a handle to its signaled kernel object. Query the wait state instead
    // of treating every successful OpenProcess call as a live writer.
    let wait = unsafe { WaitForSingleObject(handle, 0) };
    unsafe {
        CloseHandle(handle);
    }
    match wait {
        WAIT_OBJECT_0 => true,
        WAIT_TIMEOUT => false,
        _ => false,
    }
}

#[cfg(unix)]
fn process_is_absent(process_id: u32) -> bool {
    if process_id == std::process::id() {
        return false;
    }
    let Ok(process_id) = libc::pid_t::try_from(process_id) else {
        return false;
    };
    if process_id <= 0 {
        return false;
    }

    // SAFETY: signal 0 performs an existence/permission check only. Positive
    // PIDs avoid process-group semantics, and no pointer crosses the FFI.
    if unsafe { libc::kill(process_id, 0) } == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(not(any(unix, windows)))]
fn process_is_absent(process_id: u32) -> bool {
    let _ = process_id;
    false
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        // Same order and same guarantee as `WriterLock::release`, which is the
        // reporting route; this is the route a live writer's ordinary close
        // takes, and the backstop for any path that still relies on scope exit.
        // `Drop` cannot report, so the marker's emptiness is made *unconditional*
        // here rather than hoped for: see `clear_and_verify_writer_lock_marker`.
        // Both locks are given back by unlocking, never by letting the handle
        // close - a closing handle releases nothing that a duplicate still
        // holds, and `fork` makes duplicates this process cannot see. See
        // `unlock_writer_guard`.
        self.give_back_native_guard();
        let _ = clear_and_verify_writer_lock_marker(&mut self.file, &self.marker_path);
        let _ = unlock_writer_guard(&self.file);
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut lock_name: OsString = path.as_os_str().to_os_string();
    lock_name.push(".lock");
    PathBuf::from(lock_name)
}

#[allow(dead_code)]
fn _descriptor_for<T: VarveBlock>() -> BlockDescriptor {
    BlockDescriptor {
        id: T::ID,
        name: std::any::type_name::<T>(),
        version: T::VERSION,
        kind: T::KIND,
        fields: &[],
    }
}

#[allow(dead_code)]
struct TypedMarker<T>(PhantomData<T>);

#[cfg(test)]
mod tests {
    /// One forward pass must answer exactly what the three walks it replaced
    /// answered, on every shape that tells them apart.
    ///
    /// `derive_index_state` folds `CheckpointCadence::from_index` (a reverse
    /// walk stopping at the newest checkpoint), `SegmentCursor::from_index` (an
    /// `rposition` for the newest segment) and `BlockTails::from_index` (a full
    /// forward pass) into one traversal. The three rules are kept beside it as
    /// `#[cfg(test)]` specifications precisely so this can compare against
    /// them rather than against a restatement of the new code.
    ///
    /// The shapes matter more than the count. An index with no checkpoint and
    /// no segment is the case where both reverse walks degenerate to full
    /// walks; a checkpoint at the very end and one at the very start bracket
    /// the `position / 2` threshold; interleaved commit and segment records are
    /// what separate "eligible" from "seen", since those two ids are the ones
    /// the cadence does not count.
    #[test]
    fn derived_state_matches_the_three_rules_it_replaced() {
        fn entry(block_id: u32, sequence: u64) -> RecordIndexEntry {
            RecordIndexEntry {
                block_id,
                block_version: 1,
                flags: 0,
                sequence,
                record_offset: sequence * 16,
                payload_offset: sequence * 16 + 8,
                payload_len: 8,
                checksum: 0,
                uncompressed_len_hint: 0,
                footer_offset: None,
                prev_same_block_offset: None,
                prev_same_key_offset: None,
                committed: false,
            }
        }
        fn build(ids: &[u32]) -> Vec<RecordIndexEntry> {
            ids.iter()
                .enumerate()
                .map(|(position, id)| entry(*id, position as u64))
                .collect()
        }

        const USER: u32 = 7;
        let shapes: Vec<(&str, Vec<RecordIndexEntry>)> = vec![
            ("empty", build(&[])),
            (
                "no checkpoint, no segment",
                build(&[USER, USER, USER, USER]),
            ),
            ("checkpoint last", build(&[USER, USER, INDEX_BLOCK_ID])),
            (
                "checkpoint first",
                build(&[INDEX_BLOCK_ID, USER, USER, USER]),
            ),
            (
                "two checkpoints, newest wins",
                build(&[INDEX_BLOCK_ID, USER, INDEX_BLOCK_ID, USER, USER]),
            ),
            ("segment only", build(&[USER, SEGMENT_BLOCK_ID, USER])),
            (
                "segment last, so the cursor sits past the end",
                build(&[USER, USER, SEGMENT_BLOCK_ID]),
            ),
            (
                "commit and segment interleaved, neither is eligible",
                build(&[
                    USER,
                    COMMIT_BLOCK_ID,
                    SEGMENT_BLOCK_ID,
                    USER,
                    COMMIT_BLOCK_ID,
                    INDEX_BLOCK_ID,
                    USER,
                    SEGMENT_BLOCK_ID,
                    USER,
                ]),
            ),
        ];

        for (name, index) in shapes {
            let derived = derive_index_state(index.iter(), true);
            let cadence = CheckpointCadence::from_index(&index);
            assert_eq!(
                derived.checkpoint_cadence.eligible_since_checkpoint,
                cadence.eligible_since_checkpoint,
                "{name}: eligible count diverged from the reverse walk"
            );
            assert_eq!(
                derived.checkpoint_cadence.next_threshold, cadence.next_threshold,
                "{name}: next threshold diverged from the reverse walk"
            );
            let cursor = SegmentCursor::from_index(&index);
            assert_eq!(
                derived.segment_cursor.next_position, cursor.next_position,
                "{name}: segment cursor position diverged from the rposition"
            );
            assert_eq!(
                derived.segment_cursor.covered_start, cursor.covered_start,
                "{name}: segment coverage diverged from the rposition"
            );
            // `BlockTails` is `PartialEq`, so this compares the whole
            // structure — the ordered table as well as the tails — rather than
            // spot-checking ids the fused pass happens to get right.
            assert_eq!(
                derived.block_tails.as_ref().expect("tails were requested"),
                &BlockTails::from_index(&index),
                "{name}: block tails diverged from the forward walk"
            );
        }
    }

    use super::*;
    use crate::{VarveDecode, VarveEncode};

    /// One index entry for a record of `payload_len` bytes at `record_offset`,
    /// with no footer, as a checkpoint would hold it.
    fn checkpoint_test_entry(record_offset: u64, payload_len: u64) -> RecordIndexEntry {
        RecordIndexEntry {
            block_id: 7,
            block_version: 1,
            flags: 0,
            sequence: record_offset,
            record_offset,
            payload_offset: record_offset + RECORD_HEADER_LEN,
            payload_len,
            checksum: 0,
            uncompressed_len_hint: 0,
            footer_offset: None,
            prev_same_block_offset: None,
            prev_same_key_offset: None,
            committed: true,
        }
    }

    fn checkpoint_record_at(record_offset: u64) -> RecordIndexEntry {
        let mut entry = checkpoint_test_entry(record_offset, 0);
        entry.block_id = INDEX_BLOCK_ID;
        entry
    }

    // The records a checkpoint covers are contiguous, and the predicate used to
    // compare `record_offset < previous_offset`. That accepts a *gap*: a
    // one-entry checkpoint could name `covered_offset` far past the record it
    // listed, and every record in between is on disk, absent from the
    // checkpoint, and therefore absent from `index_entries()` and from every
    // typed read.
    #[test]
    fn a_checkpoint_that_skips_records_is_refused() {
        let header_len = 32;
        let first = checkpoint_test_entry(header_len, 8);
        let skipped = checkpoint_test_entry(first.physical_end(), 8);
        let record = checkpoint_record_at(skipped.physical_end());

        let contiguous = IndexCheckpoint {
            covered_offset: record.record_offset,
            entries: vec![first.clone(), skipped.clone()],
        };
        assert!(
            validate_index_checkpoint(
                header_len,
                record.physical_end(),
                &record,
                &contiguous,
                contiguous.entries.len(),
                contiguous.entries.iter(),
            )
            .is_ok(),
            "a checkpoint that tiles its coverage must still validate",
        );

        // The same file, with the middle record simply left out. The observed
        // prefix is what the scan saw, so it agrees with the checkpoint - the
        // hole is only visible as an offset gap.
        let with_gap = IndexCheckpoint {
            covered_offset: record.record_offset,
            entries: vec![first.clone()],
        };
        assert!(matches!(
            validate_index_checkpoint(
                header_len,
                record.physical_end(),
                &record,
                &with_gap,
                with_gap.entries.len(),
                with_gap.entries.iter(),
            ),
            Err(Error::InvalidIndexCheckpoint),
        ));
    }

    // The same hole, moved to the far end: every entry is contiguous, and the
    // checkpoint simply stops before its own record. Only the terminal
    // `previous_offset == covered_offset` catches this one.
    #[test]
    fn a_checkpoint_that_stops_short_of_its_coverage_is_refused() {
        let header_len = 32;
        let first = checkpoint_test_entry(header_len, 8);
        let trailing = checkpoint_test_entry(first.physical_end(), 8);
        let record = checkpoint_record_at(trailing.physical_end());

        let short = IndexCheckpoint {
            covered_offset: record.record_offset,
            entries: vec![first.clone()],
        };
        assert!(matches!(
            validate_index_checkpoint(
                header_len,
                record.physical_end(),
                &record,
                &short,
                short.entries.len(),
                short.entries.iter(),
            ),
            Err(Error::InvalidIndexCheckpoint),
        ));
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct MatrixTestCell {
        value: u32,
    }

    impl VarveEncode for MatrixTestCell {
        const WIRE_TYPE: crate::WireType = crate::WireType::U32;

        fn encode_varve(&self, encoder: &mut crate::Encoder) -> Result<()> {
            self.value.encode_varve(encoder)
        }
    }

    impl VarveDecode for MatrixTestCell {
        const WIRE_TYPE: crate::WireType = crate::WireType::U32;

        fn decode_varve(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
            Ok(Self {
                value: u32::decode_varve(decoder)?,
            })
        }
    }

    impl VarveBlock for MatrixTestCell {
        const ID: u32 = 41;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Matrix;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = false;
        const SCHEMA_FINGERPRINT: u64 = 0x4649_4C45_0000_0029;
    }

    impl VarveMatrixBlock for MatrixTestCell {
        const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
        const CATEGORY: &'static str = "cells";
        const SLOT_STRIDE: u64 = 4;
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct ReplaceTestBlock {
        value: u64,
    }

    impl VarveEncode for ReplaceTestBlock {
        const WIRE_TYPE: crate::WireType = crate::WireType::U64;

        fn encode_varve(&self, encoder: &mut crate::Encoder) -> Result<()> {
            self.value.encode_varve(encoder)
        }
    }

    impl VarveDecode for ReplaceTestBlock {
        const WIRE_TYPE: crate::WireType = crate::WireType::U64;

        fn decode_varve(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
            Ok(Self {
                value: u64::decode_varve(decoder)?,
            })
        }
    }

    impl VarveBlock for ReplaceTestBlock {
        const ID: u32 = 42;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = false;
        const SCHEMA_FINGERPRINT: u64 = 0x4649_4C45_0000_002A;
    }

    impl VarveReplaceBlock for ReplaceTestBlock {
        fn validate_replacement(_old: &Self, _new: &Self) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ReplaceString(String);

    impl VarveEncode for ReplaceString {
        const WIRE_TYPE: crate::WireType = crate::WireType::String;

        fn encode_varve(&self, encoder: &mut crate::Encoder) -> Result<()> {
            self.0.encode_varve(encoder)
        }
    }

    impl VarveDecode for ReplaceString {
        const WIRE_TYPE: crate::WireType = crate::WireType::String;

        fn decode_varve(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
            Ok(Self(String::decode_varve(decoder)?))
        }
    }

    impl VarveBlock for ReplaceString {
        const ID: u32 = 43;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Variable;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = false;
        const SCHEMA_FINGERPRINT: u64 = 0x4649_4C45_0000_002B;
    }

    impl VarveReplaceBlock for ReplaceString {
        fn validate_replacement(_old: &Self, _new: &Self) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ReplaceKeyed {
        key: u64,
        value: String,
    }

    impl VarveEncode for ReplaceKeyed {
        const WIRE_TYPE: crate::WireType = <(u64, String) as VarveEncode>::WIRE_TYPE;

        fn encode_varve(&self, encoder: &mut crate::Encoder) -> Result<()> {
            (self.key, self.value.clone()).encode_varve(encoder)
        }
    }

    impl VarveDecode for ReplaceKeyed {
        const WIRE_TYPE: crate::WireType = <(u64, String) as VarveDecode>::WIRE_TYPE;

        fn decode_varve(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
            let (key, value) = <(u64, String)>::decode_varve(decoder)?;
            Ok(Self { key, value })
        }
    }

    impl VarveBlock for ReplaceKeyed {
        const ID: u32 = 44;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Variable;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = true;
        const SCHEMA_FINGERPRINT: u64 = 0x4649_4C45_0000_002C;
    }

    impl VarveKeyedBlock for ReplaceKeyed {
        type Key = u64;

        fn key(&self) -> Self::Key {
            self.key
        }
    }

    impl VarveReplaceBlock for ReplaceKeyed {
        fn validate_replacement(old: &Self, new: &Self) -> Result<()> {
            if old.key == new.key {
                Ok(())
            } else {
                Err(Error::ReplacementKeyMismatch)
            }
        }
    }

    /// A format with one declared block kept out of the resident index, and the
    /// footer chain that makes it reachable.
    /// A failed seal must leave the chunk where it was.
    ///
    /// `seal_open_chunk` `take()`d the chunk and *then* ran three fallible
    /// steps, so any failure destroyed the data and emptied `open_chunk` — and
    /// the retry a caller would naturally make returned `Ok(())` for a chunk
    /// that no longer existed anywhere. Silent loss reported as success.
    ///
    /// **The state is constructed rather than reached through the API, and the
    /// reason is worth stating.** The reachable way to make a seal fail was a
    /// record payload ceiling below the chunk's sealed size, and `create` now
    /// refuses that pairing outright — fixing a different defect closed the
    /// public route to this one. `a_chunk_that_could_never_be_sealed_is_refused_at_create`
    /// covers the refusal; this covers what happens if a seal fails anyway,
    /// which an I/O error still can.
    #[test]
    fn a_failed_seal_keeps_the_chunk() {
        let spec = residency_test_spec();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("seal.varve");
        let mut file = VarveFile::create(spec, &path).expect("create");

        file.open_chunk = Some(OpenChunk {
            index: 1,
            first_row: 4,
            rows: 4,
            compression: None,
            blocks: vec![OpenChunkBlock {
                block_id: 900,
                stride: 4,
                cells: 32,
                written: vec![0xFF; 4],
                commit: vec![0xFF; 4],
                crc: Vec::new(),
                slots: vec![7; 128],
            }],
            dirty: true,
        });

        // A ceiling the encoded chunk cannot fit under.
        file.spec = file
            .spec
            .with_read_limits(file.spec.read_limits.with_max_record_payload_len(16));

        assert!(matches!(
            file.seal_open_chunk(),
            Err(Error::LimitExceeded { .. })
        ));
        assert!(
            file.open_chunk.is_some(),
            "a failed seal must leave the chunk where it was",
        );
        // The retry reports the same failure rather than Ok.
        assert!(matches!(
            file.seal_open_chunk(),
            Err(Error::LimitExceeded { .. })
        ));

        // Let it succeed, and confirm the chunk is released exactly once.
        file.spec = file
            .spec
            .with_read_limits(file.spec.read_limits.with_max_record_payload_len(u64::MAX));
        file.seal_open_chunk().expect("seal");
        assert!(file.open_chunk.is_none());
    }

    fn residency_test_spec() -> FormatSpec {
        const BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
            id: 12,
            name: "line",
            version: 1,
            kind: BlockKind::Variable,
            fields: &[],
        }];
        const RESIDENCY: &[crate::BlockResidencyDescriptor] = &[crate::BlockResidencyDescriptor {
            block_id: 12,
            resident: false,
        }];
        FormatSpec::new(
            b"VSRES",
            1,
            Endian::Little,
            0,
            IndexPolicy::BlockOffsetChain,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .with_block_residency(RESIDENCY)
        .with_read_limits(crate::ReadLimits::finite_all(u64::MAX))
    }

    // §5.4. A rolled-back append of a non-resident record must leave nothing
    // behind: the file length, the sequence and - the part residency makes
    // load-bearing - the block tail, which is the only way back to that block's
    // records.
    //
    // This does *not* exercise `BlockTails::restore`. Both rollback points sit
    // above `block_tails.note_appended`, so a rollback always finds the tail
    // where it started; the restore is unreachable and is kept for the ordering
    // rather than for today. Reverting it to the old rebuild-from-index, or
    // deleting it outright, leaves this test green, and that is recorded rather
    // than papered over.
    #[test]
    fn a_rolled_back_non_resident_append_leaves_the_tail_alone() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("residency-rollback.varve");
        let mut file = VarveFile::create(residency_test_spec(), &path)?;

        file.write_record(12, 1, 0, b"first")?;
        let tail_before = file.block_tails.tail(12);
        assert!(
            tail_before.is_some(),
            "the tail is published for a non-resident block"
        );
        assert!(file.index.is_empty(), "and nothing is resident");
        let len_before = file.file.metadata()?.len();
        let sequence_before = file.sequence_state;

        inject_write_fault(WriteFault::AppendAfterHeader);
        assert!(matches!(
            file.write_record(12, 1, 0, b"rolled back"),
            Err(Error::Io(_))
        ));

        assert_eq!(
            file.block_tails.tail(12),
            tail_before,
            "the rolled-back record must not stay reachable through the tail",
        );
        assert_eq!(file.file.metadata()?.len(), len_before);
        assert_eq!(file.sequence_state, sequence_before);
        assert!(!file.poison.is_refusing());

        // The next append chains onto the surviving record, not the ghost.
        let info = file.push_info_for_test(12, b"second")?;
        assert_eq!(info.prev_same_block_offset, tail_before);
        Ok(())
    }

    // The first append of a block publishes no tail if it rolls back, so the
    // next surviving record is still the head of the chain.
    #[test]
    fn a_rolled_back_first_append_leaves_no_tail() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("residency-first.varve");
        let mut file = VarveFile::create(residency_test_spec(), &path)?;
        assert_eq!(file.block_tails.tail(12), None);

        inject_write_fault(WriteFault::AppendAfterHeader);
        assert!(matches!(
            file.write_record(12, 1, 0, b"x"),
            Err(Error::Io(_))
        ));
        assert_eq!(
            file.block_tails.tail(12),
            None,
            "a rollback of the first append must leave no tail behind",
        );

        let info = file.push_info_for_test(12, b"real")?;
        assert_eq!(
            info.prev_same_block_offset, None,
            "the first surviving record names no predecessor",
        );
        Ok(())
    }

    fn test_spec() -> FormatSpec {
        FormatSpec::new(
            b"VSTEST",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            &[],
        )
        .with_read_limits(crate::ReadLimits::finite_all(u64::MAX))
    }

    fn matrix_test_spec() -> FormatSpec {
        static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
            id: MatrixTestCell::ID,
            name: "MatrixTestCell",
            version: MatrixTestCell::VERSION,
            kind: BlockKind::Matrix,
            fields: MatrixTestCell::FIELDS,
        }];
        static DIMENSIONS: &[crate::MatrixDimensionDescriptor] = &[
            crate::MatrixDimensionDescriptor { name: "scan" },
            crate::MatrixDimensionDescriptor { name: "ch" },
        ];
        static COMMITS: &[crate::MatrixCommitDescriptor] = &[crate::MatrixCommitDescriptor {
            name: "cells",
            kind: crate::MatrixCommitKind::Cell,
        }];
        static MATRIX_BLOCKS: &[crate::MatrixBlockDescriptor] = &[crate::MatrixBlockDescriptor {
            block_id: MatrixTestCell::ID,
            dimensions: MatrixTestCell::DIMENSIONS,
            category: MatrixTestCell::CATEGORY,
            slot_stride: MatrixTestCell::SLOT_STRIDE,
        }];

        FormatSpec::new(
            b"VSMTX",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .with_matrix_spec(DIMENSIONS, COMMITS, MATRIX_BLOCKS)
        .with_read_limits(crate::ReadLimits::finite_all(u64::MAX))
    }

    fn replace_test_spec() -> FormatSpec {
        static BLOCKS: &[BlockDescriptor] = &[
            BlockDescriptor {
                id: ReplaceTestBlock::ID,
                name: "ReplaceTestBlock",
                version: ReplaceTestBlock::VERSION,
                kind: BlockKind::Fixed,
                fields: ReplaceTestBlock::FIELDS,
            },
            BlockDescriptor {
                id: ReplaceString::ID,
                name: "ReplaceString",
                version: ReplaceString::VERSION,
                kind: BlockKind::Variable,
                fields: ReplaceString::FIELDS,
            },
            BlockDescriptor {
                id: ReplaceKeyed::ID,
                name: "ReplaceKeyed",
                version: ReplaceKeyed::VERSION,
                kind: BlockKind::Variable,
                fields: ReplaceKeyed::FIELDS,
            },
        ];

        FormatSpec::new(
            b"VSRPL",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .with_read_limits(crate::ReadLimits::finite_all(u64::MAX))
    }

    fn replacement_policy_spec() -> FormatSpec {
        let spec = replace_test_spec()
            .with_index_policy(IndexPolicy::new(true, true, true, false))
            .with_commit_policy(CommitPolicy::RecordFooter);
        #[cfg(feature = "integrity")]
        {
            spec.with_integrity_policy(IntegrityPolicy::Crc32WithHeader)
        }
        #[cfg(not(feature = "integrity"))]
        {
            spec
        }
    }

    #[test]
    fn replacement_info_translates_grow_and_shrink_offsets() -> Result<()> {
        let grow = ReplacementInfo {
            sequence: 7,
            record_offset: 100,
            old_payload_len: 8,
            new_payload_len: 18,
            old_physical_len: 40,
            new_physical_len: 50,
        };
        assert_eq!(grow.translate_record_offset(99)?, 99);
        assert_eq!(grow.translate_record_offset(100)?, 100);
        assert_eq!(grow.translate_record_offset(140)?, 150);

        let shrink = ReplacementInfo {
            old_payload_len: 18,
            new_payload_len: 8,
            old_physical_len: 50,
            new_physical_len: 40,
            ..grow
        };
        assert_eq!(shrink.translate_record_offset(150)?, 140);
        Ok(())
    }

    #[test]
    fn replace_block_grows_shrinks_and_preserves_sequence_chains_and_snapshot() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("resized.varve");
        let spec = replacement_policy_spec();
        let mut writer = VarveFile::create(spec, &path)?;
        writer.push(&ReplaceString("a".into()))?;
        writer.flush()?;
        writer.push(&ReplaceTestBlock { value: 9 })?;
        writer.push(&ReplaceString("tail".into()))?;
        writer.flush()?;

        let old_reader = VarveFile::open_readonly(spec, &path)?;
        let old_values = old_reader.blocks::<ReplaceString>()?;
        let old_tail_offset = writer
            .index_entries()
            .iter()
            .filter(|entry| entry.block_id == ReplaceString::ID)
            .nth(1)
            .expect("tail record")
            .record_offset;
        let sequence = writer
            .index_entries()
            .iter()
            .find(|entry| entry.block_id == ReplaceString::ID)
            .expect("target record")
            .sequence;

        let grown = writer.replace_block(
            0,
            &ReplaceString("a replacement that is substantially longer".into()),
        )?;
        assert_eq!(grown.sequence, sequence);
        assert!(grown.new_physical_len > grown.old_physical_len);
        let translated_tail = grown.translate_record_offset(old_tail_offset)?;
        let entries = writer.index_entries();
        let tail = entries
            .iter()
            .filter(|entry| entry.block_id == ReplaceString::ID)
            .nth(1)
            .expect("rewritten tail");
        assert_eq!(tail.record_offset, translated_tail);
        assert_eq!(tail.prev_same_block_offset, Some(grown.record_offset));
        assert_eq!(old_values.get(0)?, Some(ReplaceString("a".into())));

        let shrunk = writer.replace_block(0, &ReplaceString(String::new()))?;
        assert_eq!(shrunk.sequence, sequence);
        assert!(shrunk.new_physical_len < shrunk.old_physical_len);
        let equal = writer.replace_block(0, &ReplaceString("x".into()))?;
        let equal_again = writer.replace_block(0, &ReplaceString("y".into()))?;
        assert_eq!(equal.new_physical_len, equal_again.new_physical_len);

        let reopened = VarveFile::open_readonly(spec, &path)?;
        assert_eq!(
            reopened.blocks::<ReplaceString>()?.get(0)?,
            Some(ReplaceString("y".into()))
        );
        assert_eq!(
            reopened.blocks::<ReplaceString>()?.get(1)?,
            Some(ReplaceString("tail".into()))
        );
        assert_no_rewrite_temps(directory.path())?;
        Ok(())
    }

    #[test]
    fn replace_block_validates_key_before_publication() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("keyed.varve");
        let spec = replace_test_spec();
        let mut writer = VarveFile::create(spec, &path)?;
        writer.push(&ReplaceKeyed {
            key: 1,
            value: "old".into(),
        })?;
        let original = std::fs::read(&path)?;

        assert!(matches!(
            writer.replace_block(
                0,
                &ReplaceKeyed {
                    key: 2,
                    value: "new".into(),
                },
            ),
            Err(Error::ReplacementKeyMismatch)
        ));
        assert_eq!(std::fs::read(&path)?, original);
        assert_no_rewrite_temps(directory.path())?;
        Ok(())
    }

    #[test]
    fn replace_block_translates_keyed_predecessors_after_target() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("keyed-chain.varve");
        let spec =
            replacement_policy_spec().with_index_policy(IndexPolicy::new(true, true, true, true));
        let mut writer = VarveFile::create(spec, &path)?;
        // API2-05: the generic push refuses keyed blocks on a keyed-chaining
        // format; the maintaining path is the equivalent entry point.
        let first = writer.push_keyed_info(&ReplaceKeyed {
            key: 1,
            value: "a".into(),
        })?;
        let second = writer.push_with_prev_key_info(
            &ReplaceKeyed {
                key: 1,
                value: "b".into(),
            },
            Some(first.record_offset),
        )?;
        writer.push_with_prev_key_info(
            &ReplaceKeyed {
                key: 1,
                value: "c".into(),
            },
            Some(second.record_offset),
        )?;
        writer.flush()?;

        let info = writer.replace_block(
            0,
            &ReplaceKeyed {
                key: 1,
                value: "a much longer first value".into(),
            },
        )?;
        let snapshot = writer.index_entries();
        let keyed: Vec<_> = snapshot
            .iter()
            .filter(|entry| entry.block_id == ReplaceKeyed::ID)
            .collect();
        assert_eq!(
            keyed[2].prev_same_key_offset,
            Some(info.translate_record_offset(second.record_offset)?)
        );
        VarveFile::open_readonly(spec, &path)?;
        Ok(())
    }

    #[test]
    fn replace_block_preserves_transaction_visibility_and_next_sequence() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("transaction.varve");
        let spec = replace_test_spec()
            .with_index_policy(IndexPolicy::new(true, true, true, false))
            .with_commit_policy(CommitPolicy::TransactionMarker(
                crate::TransactionMarkerMode::Explicit,
            ));
        let mut writer = VarveFile::create(spec, &path)?;
        writer.push(&ReplaceString("committed-old".into()))?;
        writer.commit()?;
        let old_reader = VarveFile::open_readonly(spec, &path)?;
        writer.push(&ReplaceString("uncommitted".into()))?;
        let next_sequence = writer.sequence_state.available()?;

        let info = writer.replace_block(0, &ReplaceString("committed-new".into()))?;
        assert_eq!(info.sequence, 0);
        assert_eq!(writer.sequence_state.available()?, next_sequence);
        assert_eq!(
            old_reader.blocks::<ReplaceString>()?.get(0)?,
            Some(ReplaceString("committed-old".into()))
        );
        let new_reader = VarveFile::open_readonly(spec, &path)?;
        let visible = new_reader.blocks::<ReplaceString>()?;
        assert_eq!(visible.len(), 1);
        assert_eq!(visible.get(0)?, Some(ReplaceString("committed-new".into())));
        assert_eq!(writer.push(&ReplaceTestBlock { value: 11 })?, next_sequence);
        Ok(())
    }

    #[test]
    fn replacement_publish_rebind_failure_is_explicit_and_poisoned() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replacement-rebind.varve");
        let spec = replace_test_spec();
        let mut writer = VarveFile::create(spec, &path)?;
        writer.push(&ReplaceString("old".into()))?;
        let old_reader = VarveFile::open_readonly(spec, &path)?;
        inject_write_fault(WriteFault::RebindAfterPublish);

        assert!(matches!(
            writer.replace_block(0, &ReplaceString("new and longer".into())),
            Err(Error::PublishedButRebindFailed { sequence: 0, .. })
        ));
        assert_eq!(
            old_reader.blocks::<ReplaceString>()?.get(0)?,
            Some(ReplaceString("old".into()))
        );
        assert_eq!(
            VarveFile::open_readonly(spec, &path)?
                .blocks::<ReplaceString>()?
                .get(0)?,
            Some(ReplaceString("new and longer".into()))
        );
        assert!(matches!(writer.flush(), Err(Error::WriterPoisoned("file"))));
        assert_no_rewrite_temps(directory.path())?;
        Ok(())
    }

    #[test]
    fn max_sequence_publishes_once_then_reopens_exhausted() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("max.varve");
        let mut file = VarveFile::create(test_spec(), &path)?;
        file.sequence_state = SequenceState::Available(u64::MAX);

        assert_eq!(
            file.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"max")?,
            u64::MAX
        );
        assert_eq!(file.sequence_state, SequenceState::Exhausted);
        let original_len = file.file.metadata()?.len();
        let original_cursor = file.file.stream_position()?;
        let original_index_len = file.index.len();
        assert!(matches!(
            file.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"again"),
            Err(Error::SequenceExhausted)
        ));
        assert_eq!(file.file.metadata()?.len(), original_len);
        assert_eq!(file.file.stream_position()?, original_cursor);
        assert_eq!(file.index.len(), original_index_len);
        drop(file);

        let mut reopened = VarveFile::open(test_spec(), &path)?;
        assert_eq!(reopened.sequence_state, SequenceState::Exhausted);
        assert!(matches!(
            reopened.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"again"),
            Err(Error::SequenceExhausted)
        ));
        Ok(())
    }

    #[test]
    fn append_failure_rolls_back_and_preserves_sequence() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("rollback.varve");
        let mut file = VarveFile::create(test_spec(), &path)?;
        file.file.seek_to(0)?;
        let original_len = file.file.metadata()?.len();
        inject_write_fault(WriteFault::AppendAfterHeader);

        assert!(matches!(
            file.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"failed"),
            Err(Error::Io(_))
        ));
        assert_eq!(file.file.metadata()?.len(), original_len);
        assert_eq!(file.file.stream_position()?, 0);
        assert!(file.index.is_empty());
        assert_eq!(file.sequence_state, SequenceState::Available(0));
        assert!(!file.poison.is_refusing());
        assert_eq!(
            file.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"ok")?,
            0
        );
        Ok(())
    }

    #[test]
    fn rollback_failure_poisons_mutation_flush_and_sync() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("poison.varve");
        let mut file = VarveFile::create(test_spec(), &path)?;
        inject_write_fault(WriteFault::AppendAfterHeaderWithRollbackFailure);

        assert!(matches!(
            file.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"failed"),
            Err(Error::WriteRollbackFailed {
                operation: "append record",
                ..
            })
        ));
        assert!(matches!(
            file.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"again"),
            Err(Error::WriterPoisoned("file"))
        ));
        assert!(matches!(file.flush(), Err(Error::WriterPoisoned("file"))));
        assert!(matches!(file.sync(), Err(Error::WriterPoisoned("file"))));
        Ok(())
    }

    #[test]
    fn partial_matrix_overwrite_is_withdrawn_and_poisons_writer() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("matrix-partial.varve");
        let spec = matrix_test_spec();
        let key = MatrixKey::new(0, 0);
        let mut file = VarveFile::create_with_dims(
            spec,
            &path,
            MatrixDimensions::from_pairs([("scan", 1), ("ch", 1)]),
        )?;
        file.write_matrix_cell(key, &MatrixTestCell { value: 11 })?;
        file.commit_matrix_cell::<MatrixTestCell>(key)?;
        let original_len = file.file.metadata()?.len();

        crate::matrix::inject_partial_slot_write_failure();
        assert!(matches!(
            file.write_matrix_cell(key, &MatrixTestCell { value: 22 }),
            Err(Error::Io(_))
        ));
        assert_eq!(file.file.metadata()?.len(), original_len);
        assert_eq!(
            file.matrix_cell_status::<MatrixTestCell>(key)?,
            MatrixCellStatus::NotCommitted
        );
        assert!(matches!(
            file.write_matrix_cell(key, &MatrixTestCell { value: 33 }),
            Err(Error::WriterPoisoned("file"))
        ));
        assert!(matches!(file.flush(), Err(Error::WriterPoisoned("file"))));
        assert!(matches!(file.sync(), Err(Error::WriterPoisoned("file"))));
        drop(file);

        let reopened = VarveFile::open_readonly(spec, &path)?;
        assert_eq!(
            reopened.matrix_cell_status::<MatrixTestCell>(key)?,
            MatrixCellStatus::NotCommitted
        );
        assert!(matches!(
            reopened.read_matrix_cell::<MatrixTestCell>(key),
            Err(Error::MatrixNotCommitted)
        ));
        Ok(())
    }

    #[test]
    fn fixed_publish_rebind_failure_is_explicit_and_poisoned() -> Result<()> {
        assert_publish_rebind_failure(false)
    }

    #[test]
    fn rewrite_publish_rebind_failure_is_explicit_and_poisoned() -> Result<()> {
        assert_publish_rebind_failure(true)
    }

    fn assert_publish_rebind_failure(rewrite: bool) -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(if rewrite {
            "rewrite-rebind.varve"
        } else {
            "fixed-rebind.varve"
        });
        let spec = replace_test_spec();
        {
            let mut initial = VarveFile::create(spec, &path)?;
            initial.push(&ReplaceTestBlock { value: 1 })?;
            initial.flush()?;
        }

        let old_reader = VarveFile::open_readonly(spec, &path)?;
        let old_blocks = old_reader.blocks::<ReplaceTestBlock>()?;
        let mut writer = VarveFile::open(spec, &path)?;
        inject_write_fault(WriteFault::RebindAfterPublish);
        let error = if rewrite {
            writer
                .replace_rewrite(0, &ReplaceTestBlock { value: 2 })
                .expect_err("injected rewrite rebind failure must be returned")
        } else {
            writer
                .replace_fixed(0, &ReplaceTestBlock { value: 2 })
                .expect_err("injected fixed rebind failure must be returned")
        };
        match error {
            Error::PublishedButRebindFailed {
                sequence,
                source,
                parent_sync,
            } => {
                assert_eq!(sequence, 1);
                assert!(matches!(*source, Error::Io(_)));
                // F-07: no parent-directory sync failure happened here, so the
                // second durability fact is absent rather than fabricated.
                assert!(parent_sync.is_none());
            }
            other => panic!("unexpected publication error: {other:?}"),
        }

        assert_eq!(old_blocks.get(0)?, Some(ReplaceTestBlock { value: 1 }));
        assert_eq!(
            VarveFile::open_readonly(spec, &path)?
                .blocks::<ReplaceTestBlock>()?
                .get(0)?,
            Some(ReplaceTestBlock { value: 2 })
        );
        assert!(matches!(
            writer.push(&ReplaceTestBlock { value: 3 }),
            Err(Error::WriterPoisoned("file"))
        ));
        assert_no_rewrite_temps(directory.path())?;

        drop(writer);
        let mut reopened = VarveFile::open(spec, &path)?;
        assert_eq!(reopened.push(&ReplaceTestBlock { value: 3 })?, 2);
        Ok(())
    }

    /// F-07 double fault, one per replacement API.
    ///
    /// A replacement publication learns two independent durability facts in a
    /// fixed order: whether the parent-directory sync succeeded, and whether
    /// the writer could rebind to the published generation. Only the second
    /// had a path out, so the pre-fix code called rebind with `?` and, when
    /// both failed, returned `PublishedButRebindFailed` alone - silently
    /// dropping the already-known fact that the published pathname may not
    /// survive power loss. Both facts must now arrive together.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ReplacementApi {
        Block,
        Fixed,
        Rewrite,
    }

    fn assert_double_publication_fault_reports_both_facts(api: ReplacementApi) -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(match api {
            ReplacementApi::Block => "double-block.varve",
            ReplacementApi::Fixed => "double-fixed.varve",
            ReplacementApi::Rewrite => "double-rewrite.varve",
        });
        let spec = replace_test_spec();
        {
            let mut initial = VarveFile::create(spec, &path)?;
            initial.push(&ReplaceTestBlock { value: 1 })?;
            initial.flush()?;
        }

        let mut writer = VarveFile::open(spec, &path)?;
        // One arming produces both failures in the order the code sees them:
        // the parent-directory sync inside `replace_path_atomically`, then the
        // rebind that follows it.
        inject_write_fault(WriteFault::ParentSyncThenRebindAfterPublish);
        let error = match api {
            ReplacementApi::Block => writer
                .replace_block(0, &ReplaceTestBlock { value: 2 })
                .map(|info| info.sequence)
                .expect_err("injected double publication fault must be returned"),
            ReplacementApi::Fixed => writer
                .replace_fixed(0, &ReplaceTestBlock { value: 2 })
                .expect_err("injected double publication fault must be returned"),
            ReplacementApi::Rewrite => writer
                .replace_rewrite(0, &ReplaceTestBlock { value: 2 })
                .expect_err("injected double publication fault must be returned"),
        };
        inject_write_fault(WriteFault::None);

        match error {
            Error::PublishedButRebindFailed {
                source,
                parent_sync,
                ..
            } => {
                assert!(matches!(*source, Error::Io(_)), "rebind fact: {source:?}");
                let parent_sync = parent_sync.expect(
                    "the parent-directory sync failure observed before the rebind must survive",
                );
                assert!(
                    matches!(*parent_sync, Error::Io(_)),
                    "parent-sync fact: {parent_sync:?}",
                );
                // The rendered message must carry both facts, because that is
                // what an operator reads out of a log line.
                let rendered = Error::PublishedButRebindFailed {
                    sequence: 0,
                    source,
                    parent_sync: Some(parent_sync),
                }
                .to_string();
                assert!(
                    rendered.contains("could not rebind"),
                    "message lost the rebind fact: {rendered}"
                );
                assert!(
                    rendered.contains("parent-directory sync"),
                    "message lost the parent-sync fact: {rendered}"
                );
            }
            other => panic!("unexpected double-fault outcome: {other:?}"),
        }

        // Everything the single-fault contract already promised still holds:
        // publication happened, and the writer is poisoned.
        assert_eq!(
            VarveFile::open_readonly(spec, &path)?
                .blocks::<ReplaceTestBlock>()?
                .get(0)?,
            Some(ReplaceTestBlock { value: 2 })
        );
        assert!(matches!(
            writer.push(&ReplaceTestBlock { value: 3 }),
            Err(Error::WriterPoisoned("file"))
        ));
        assert_no_rewrite_temps(directory.path())?;
        Ok(())
    }

    #[test]
    fn block_replacement_double_publication_fault_reports_both_facts() -> Result<()> {
        assert_double_publication_fault_reports_both_facts(ReplacementApi::Block)
    }

    #[test]
    fn fixed_replacement_double_publication_fault_reports_both_facts() -> Result<()> {
        assert_double_publication_fault_reports_both_facts(ReplacementApi::Fixed)
    }

    #[test]
    fn rewrite_replacement_double_publication_fault_reports_both_facts() -> Result<()> {
        assert_double_publication_fault_reports_both_facts(ReplacementApi::Rewrite)
    }

    #[cfg(windows)]
    #[test]
    fn windows_sharing_violation_preserves_generation_and_allows_retry() -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("sharing.varve");
        let spec = replace_test_spec();
        {
            let mut initial = VarveFile::create(spec, &path)?;
            initial.push(&ReplaceTestBlock { value: 1 })?;
            initial.flush()?;
        }

        let old_reader = VarveFile::open_readonly(spec, &path)?;
        let old_blocks = old_reader.blocks::<ReplaceTestBlock>()?;
        let mut writer = VarveFile::open(spec, &path)?;
        let blocker = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)?;

        assert!(matches!(
            writer.replace_fixed(0, &ReplaceTestBlock { value: 2 }),
            Err(Error::Io(_))
        ));
        assert_eq!(
            writer.blocks::<ReplaceTestBlock>()?.get(0)?,
            Some(ReplaceTestBlock { value: 1 })
        );
        assert_eq!(old_blocks.get(0)?, Some(ReplaceTestBlock { value: 1 }));
        assert_no_rewrite_temps(directory.path())?;

        drop(blocker);
        writer.replace_fixed(0, &ReplaceTestBlock { value: 2 })?;
        assert_eq!(old_blocks.get(0)?, Some(ReplaceTestBlock { value: 1 }));
        assert_eq!(
            VarveFile::open_readonly(spec, &path)?
                .blocks::<ReplaceTestBlock>()?
                .get(0)?,
            Some(ReplaceTestBlock { value: 2 })
        );
        assert_no_rewrite_temps(directory.path())?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_concurrent_replacefilew_keeps_whole_generations_and_old_handle() -> Result<()> {
        use std::sync::{Arc, Barrier};

        for round in 0..64u32 {
            let directory = tempfile::tempdir()?;
            let target = directory.path().join("target.bin");
            let replacement_a = directory.path().join("a.bin");
            let replacement_b = directory.path().join("b.bin");
            let old = vec![0x11; 16 * 1024];
            let a = vec![0xA5; 16 * 1024];
            let b = vec![0x5A; 16 * 1024];
            std::fs::write(&target, &old)?;
            std::fs::write(&replacement_a, &a)?;
            std::fs::write(&replacement_b, &b)?;
            let mut old_handle = File::open(&target)?;

            let barrier = Arc::new(Barrier::new(3));
            let first_barrier = Arc::clone(&barrier);
            let first_target = target.clone();
            let first = std::thread::spawn(move || {
                first_barrier.wait();
                replace_path_atomically(&replacement_a, &first_target)
            });
            let second_barrier = Arc::clone(&barrier);
            let second_target = target.clone();
            let second = std::thread::spawn(move || {
                second_barrier.wait();
                replace_path_atomically(&replacement_b, &second_target)
            });
            barrier.wait();
            let first = first.join().expect("first ReplaceFileW thread panicked");
            let second = second.join().expect("second ReplaceFileW thread panicked");

            assert!(
                first.is_ok() || second.is_ok(),
                "round {round}: both replacements failed"
            );
            let unreconciled_indeterminate_loser = [&first, &second]
                .into_iter()
                .any(|result| matches!(result, Err(Error::ReplacePublicationIndeterminate { .. })));
            match std::fs::read(&target) {
                Ok(published) => {
                    assert!(
                        published == old || published == a || published == b,
                        "round {round}: target was a partial or mixed generation (len={}, first={:?}, first_result={first:?}, second_result={second:?})",
                        published.len(),
                        published.first()
                    );
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && unreconciled_indeterminate_loser =>
                {
                    // Documented ReplaceFileW outcome: on 1176 with no backup
                    // name, the losing replace can unlink the target name. The
                    // loser must have reported the unreconciled
                    // ReplacePublicationIndeterminate error (DUR2-01) so its
                    // caller preserves the temp file and poisons the writer.
                }
                Err(error) => panic!(
                    "round {round}: reading target failed without an indeterminate loser \
                     (error={error:?}, first_result={first:?}, second_result={second:?})"
                ),
            }
            let mut retained = Vec::new();
            old_handle.seek(SeekFrom::Start(0))?;
            old_handle.read_to_end(&mut retained)?;
            assert_eq!(retained, old, "round {round}: old handle was rebound");
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_replacefilew_rejects_retained_replacement_handle() -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let directory = tempfile::tempdir()?;
        let target = directory.path().join("target.bin");
        let replacement = directory.path().join("replacement.bin");
        let old = vec![0x11; 4 * 1024];
        let new = vec![0xA5; 4 * 1024];
        std::fs::write(&target, &old)?;
        std::fs::write(&replacement, &new)?;
        let mut retained = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&replacement)?;

        assert!(matches!(
            replace_path_atomically(&replacement, &target),
            Err(Error::Io(_))
        ));
        assert_eq!(std::fs::read(&target)?, old);
        retained.seek(SeekFrom::Start(0))?;
        let mut retained_bytes = Vec::new();
        retained.read_to_end(&mut retained_bytes)?;
        assert_eq!(retained_bytes, new);
        drop(retained);
        replace_path_atomically(&replacement, &target)?;
        assert_eq!(std::fs::read(&target)?, new);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_external_truncate_returns_error_without_rebinding_or_panicking() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("truncate.varve");
        let spec = replace_test_spec();
        {
            let mut writer = VarveFile::create(spec, &path)?;
            writer.push(&ReplaceTestBlock { value: 7 })?;
            writer.flush()?;
        }

        let reader = VarveFile::open_readonly(spec, &path)?;
        let blocks = reader.blocks::<ReplaceTestBlock>()?;
        OpenOptions::new().write(true).open(&path)?.set_len(0)?;
        assert!(blocks.get(0).is_err());
        Ok(())
    }

    fn assert_no_rewrite_temps(directory: &Path) -> Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let name = entry?.file_name();
            assert!(
                !name.to_string_lossy().contains(".rewrite."),
                "rewrite temp leaked after failed publication"
            );
        }
        Ok(())
    }

    #[test]
    fn failed_matrix_commit_publish_keeps_memory_uncommitted_and_poisons_writer() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("matrix-commit-failure.varve");
        let spec = matrix_test_spec();
        let key = MatrixKey::new(0, 0);
        let mut file = VarveFile::create_with_dims(
            spec,
            &path,
            MatrixDimensions::from_pairs([("scan", 1), ("ch", 1)]),
        )?;
        file.write_matrix_cell(key, &MatrixTestCell { value: 44 })?;

        crate::matrix::inject_bitmap_write_failure();
        assert!(matches!(
            file.commit_matrix_cell::<MatrixTestCell>(key),
            Err(Error::Io(_))
        ));
        assert_eq!(
            file.matrix_cell_status::<MatrixTestCell>(key)?,
            MatrixCellStatus::NotCommitted
        );
        assert!(matches!(file.sync(), Err(Error::WriterPoisoned("file"))));
        drop(file);

        let reopened = VarveFile::open_readonly(spec, &path)?;
        assert_eq!(
            reopened.matrix_cell_status::<MatrixTestCell>(key)?,
            MatrixCellStatus::NotCommitted
        );
        Ok(())
    }

    #[test]
    fn schema_manifest_rejects_huge_and_truncated_counts_before_iteration() {
        let mut prefix = Vec::new();
        prefix.extend_from_slice(&1u16.to_le_bytes());
        prefix.extend_from_slice(&1u16.to_le_bytes());
        prefix.push(Endian::Little.to_byte());
        prefix.extend_from_slice(&0u64.to_le_bytes());
        prefix.extend_from_slice(&[0, 0, 0, 0]);

        let mut huge = prefix.clone();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut budget = MaterializationBudget::new(test_spec());
        assert!(matches!(
            decode_schema_manifest(&huge, &mut budget),
            Err(Error::InvalidSchemaManifest)
        ));

        let mut truncated = prefix;
        truncated.extend_from_slice(&1u32.to_le_bytes());
        truncated.extend_from_slice(&[0; 8]);
        let mut budget = MaterializationBudget::new(test_spec());
        assert!(matches!(
            decode_schema_manifest(&truncated, &mut budget),
            Err(Error::InvalidSchemaManifest)
        ));
    }

    #[test]
    fn checkpoint_rejects_oversized_extent_before_index_allocation() -> Result<()> {
        let entry = RecordIndexEntry {
            block_id: ReplaceTestBlock::ID,
            block_version: ReplaceTestBlock::VERSION,
            flags: 0,
            sequence: 0,
            record_offset: 32,
            payload_offset: u64::MAX - 1,
            payload_len: 4,
            checksum: 0,
            uncompressed_len_hint: 0,
            footer_offset: None,
            prev_same_block_offset: None,
            prev_same_key_offset: None,
            committed: true,
        };
        let payload = encode_index_checkpoint_payload(test_spec(), &[entry], 128)?;

        let decoded =
            std::panic::catch_unwind(|| decode_index_checkpoint(test_spec(), &payload, 128));
        assert!(
            decoded.is_ok(),
            "oversized checkpoint extent caused a panic"
        );
        assert!(matches!(
            decoded.expect("checked above"),
            Err(Error::LengthOverflow { .. }) | Err(Error::InvalidIndexCheckpoint)
        ));
        Ok(())
    }

    /// DUR3-01: `sync` on a file this handle created must make the pathname
    /// durable, not only its contents - and must do so exactly once, never per
    /// sync and never on the append path.
    #[test]
    fn create_then_sync_makes_the_new_pathname_durable_once() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("create-parent-sync.varve");
        let spec = replace_test_spec();

        reset_parent_directory_sync_calls();
        let mut writer = VarveFile::create(spec, &path)?;
        writer.push(&ReplaceTestBlock { value: 1 })?;
        writer.push(&ReplaceTestBlock { value: 2 })?;
        writer.flush()?;
        assert_eq!(
            parent_directory_sync_calls(),
            0,
            "creating and appending must not sync the parent directory"
        );

        writer.sync()?;
        assert_eq!(
            parent_directory_sync_calls(),
            1,
            "the first sync must make the created pathname durable"
        );

        writer.sync()?;
        writer.push(&ReplaceTestBlock { value: 3 })?;
        writer.flush()?;
        writer.sync()?;
        assert_eq!(
            parent_directory_sync_calls(),
            1,
            "the pathname is durable already; later syncs must not repeat it"
        );
        drop(writer);

        // A handle that merely opens an existing pathname created nothing, so
        // it owes no directory entry.
        reset_parent_directory_sync_calls();
        let mut reopened = VarveFile::open(spec, &path)?;
        reopened.sync()?;
        assert_eq!(
            parent_directory_sync_calls(),
            0,
            "opening an existing pathname must not sync the parent directory"
        );
        Ok(())
    }

    /// PERF3-03: wholesale tail construction must have no quadratic term. The
    /// worst case is an index whose first appearances descend by block id,
    /// which made every first-seen id shift the whole sorted vector.
    #[test]
    fn block_tail_construction_moves_nothing_for_reverse_ordered_block_ids() {
        const BLOCKS: u32 = 1_024;

        fn entry(block_id: u32, sequence: u64) -> RecordIndexEntry {
            RecordIndexEntry {
                block_id,
                block_version: 1,
                flags: 0,
                sequence,
                record_offset: sequence * 16,
                payload_offset: sequence * 16 + 8,
                payload_len: 8,
                checksum: 0,
                uncompressed_len_hint: 0,
                footer_offset: None,
                prev_same_block_offset: None,
                prev_same_key_offset: None,
                committed: false,
            }
        }

        let mut index = Vec::new();
        for (sequence, block_id) in (0..BLOCKS).rev().enumerate() {
            index.push(entry(block_id, sequence as u64));
        }
        // A second descending round proves the tails track the *newest* record
        // per block id, not the first.
        for (offset, block_id) in (0..BLOCKS).rev().enumerate() {
            index.push(entry(block_id, (BLOCKS as usize + offset) as u64));
        }

        let tails = BlockTails::from_index(&index);
        assert_eq!(tails.tails.len(), BLOCKS as usize);
        for (position, (block_id, _)) in tails.tails.iter().enumerate() {
            assert_eq!(*block_id, position as u32, "tails must stay sorted by id");
        }
        for entry in &index[BLOCKS as usize..] {
            assert_eq!(
                tails.tail(entry.block_id),
                Some(entry.record_offset),
                "tail must be the newest record for its block id"
            );
        }

        // Identical result to the incremental path, which is the maintained
        // invariant; the difference is only how much movement it costs.
        let mut incremental = BlockTails::new_empty();
        for entry in &index {
            incremental.note_appended(entry.block_id, entry.record_offset);
        }
        assert_eq!(incremental, tails);
    }

    #[test]
    fn atomic_replace_invokes_parent_directory_sync() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("parent-sync-target.bin");
        let replacement = directory.path().join("parent-sync-replacement.bin");
        std::fs::write(&target, b"old")?;
        std::fs::write(&replacement, b"new")?;
        OpenOptions::new()
            .write(true)
            .open(&replacement)?
            .sync_all()?;

        reset_parent_directory_sync_calls();
        replace_path_atomically(&replacement, &target)?;

        assert_eq!(parent_directory_sync_calls(), 1);
        assert_eq!(std::fs::read(target)?, b"new");

        let new_target = directory.path().join("parent-sync-new-target.bin");
        let new_replacement = directory.path().join("parent-sync-new-replacement.bin");
        std::fs::write(&new_replacement, b"first")?;
        OpenOptions::new()
            .write(true)
            .open(&new_replacement)?
            .sync_all()?;
        reset_parent_directory_sync_calls();
        replace_path_atomically(&new_replacement, &new_target)?;

        assert_eq!(parent_directory_sync_calls(), 1);
        assert_eq!(std::fs::read(new_target)?, b"first");
        Ok(())
    }

    /// PERF: an append must ask the kernel for nothing it already knows.
    ///
    /// Two distinct per-record metadata syscalls used to sit on
    /// `write_record_with_prev_key`: `self.file.metadata()?.len()` for the
    /// `AppendSnapshot`'s end of file, and a second one inside
    /// `checked_snapshot_bounds` reached through `self.snapshot.with_len(..)`.
    /// Both facts are now taken from state the writer maintains and from the
    /// write that just returned, so both counters must stay at zero across the
    /// whole append window whatever the record count.
    ///
    /// The open at the top is not incidental: it pins the snapshot counter
    /// *nonzero* where the fstat is still correct, so a "fix" that simply
    /// stopped incrementing the counter fails here rather than passing.
    #[test]
    fn an_append_window_issues_no_metadata_syscall() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("append-syscalls.varve");

        let _ = VarveFile::take_snapshot_bounds_fstats();
        let mut file = VarveFile::create(test_spec(), &path)?;
        assert!(
            VarveFile::take_snapshot_bounds_fstats() > 0,
            "binding a snapshot at open must still cost the fstat that proves the length",
        );

        let _ = VarveFile::take_record_file_metadata_calls();
        for index in 0..1_000u32 {
            file.push_info_for_test(METADATA_BLOCK_ID, &index.to_le_bytes())?;
        }

        assert_eq!(
            VarveFile::take_record_file_metadata_calls(),
            0,
            "the append took the end of file from the snapshot it maintains, not from an fstat",
        );
        assert_eq!(
            VarveFile::take_snapshot_bounds_fstats(),
            0,
            "the snapshot rebind took the completed write as its proof, not an fstat",
        );

        // The records really are there and really are readable through the
        // rebound snapshot, so this is not "no syscalls because no work".
        assert_eq!(file.index_entries().len(), 1_000);
        let last = file
            .index_entries()
            .last()
            .expect("appended record")
            .clone();
        assert_eq!(file.snapshot.len(), last.checked_physical_end()?);
        Ok(())
    }

    /// The witness must not be usable to shorten a snapshot past bytes it has
    /// not proven: `with_written_len` is growth-only, which is what keeps it
    /// safe to hand a value the filesystem was never asked about.
    #[test]
    fn a_written_witness_cannot_shrink_a_snapshot() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("witness-shrink.varve");
        let mut file = VarveFile::create(test_spec(), &path)?;
        file.push_info_for_test(METADATA_BLOCK_ID, b"payload")?;

        let len = file.snapshot.len();
        assert!(
            file.snapshot
                .with_written_len(crate::snapshot::WrittenThrough::after_write(len))
                .is_ok()
        );
        assert!(matches!(
            file.snapshot
                .with_written_len(crate::snapshot::WrittenThrough::after_write(len - 1)),
            Err(Error::SnapshotRangeOutOfBounds { .. }),
        ));
        Ok(())
    }

    /// PERF: `replace_block` must not rescan the prefix it has already written.
    ///
    /// Every rewritten record validates its chain predecessor against the
    /// prefix already in the new generation. That prefix is built in strictly
    /// increasing `record_offset` order, so the lookup is a binary search; it
    /// used to be `iter().find(..)`, which made the whole call `Theta(N^2)` for
    /// any spec carrying a predecessor offset in its footer.
    ///
    /// The assertion is a comparison count, not a wall clock. At N = 4096 the
    /// two are ~50k against ~8.4M, and the gap grows by 2x per doubling.
    #[test]
    fn replace_block_finds_predecessors_without_rescanning_the_prefix() -> Result<()> {
        const RECORDS: u64 = 4096;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replace-probes.varve");
        let spec = replacement_policy_spec();
        assert!(
            spec.index_policy.block_offset_chain,
            "without a predecessor offset in the footer both lookup arms are skipped and there \
             is nothing to measure",
        );
        let mut writer = VarveFile::create(spec, &path)?;
        for index in 0..RECORDS {
            writer.push(&ReplaceString(format!("v{index}")))?;
        }
        writer.flush()?;
        let entries = writer.index_entries().len() as u64;

        let _ = VarveFile::take_replacement_predecessor_probes();
        writer.replace_block(0, &ReplaceString("replaced".into()))?;
        let probes = VarveFile::take_replacement_predecessor_probes();

        // At most ceil(log2 N) = 12 probes per chained record; 32 leaves room
        // for the internal records the rewrite also carries.
        let ceiling = 32 * entries;
        assert!(
            probes <= ceiling,
            "predecessor lookup cost {probes} probes over {entries} entries, ceiling {ceiling}; \
             a front-to-back scan of the growing prefix costs ~N^2/2",
        );

        // And the replacement actually happened, so this is not "cheap because
        // the validation was deleted".
        let replaced = VarveFile::open_readonly(spec, &path)?;
        let values = replaced.blocks::<ReplaceString>()?;
        assert_eq!(values.len() as u64, RECORDS);
        assert_eq!(values.get(0)?.expect("record 0").0, "replaced");
        assert_eq!(
            values.get(RECORDS as usize - 1)?.expect("last record").0,
            format!("v{}", RECORDS - 1),
        );
        Ok(())
    }
}

/// **The in-crate bypass catalogue for `file.rs`** (round 15).
///
/// The companion of `matrix.rs::bypass_catalogue`, and it exists for the same
/// reason: the round-14 proofs were `trybuild` fixtures compiled from an
/// outside crate, which say nothing about the file the defects live in. Every
/// line below was compiled from this module — a sibling of `mod
/// resident_index`, `mod replacement_target` and `mod record_file`, with every
/// privilege a future defect in this file would have — and observed to fail
/// with the quoted diagnostic.
///
/// ```text
/// // (1) mint the append path's reservation token without reserving
/// let _ = ReservedIndexSlot(());
/// //  error[E0423]: cannot initialize a tuple struct which contains private
/// //               fields  (`pub struct ReservedIndexSlot(());`)
///
/// // (2) grow the resident mirror around the token
/// index.entries.push(entry);
/// //  error[E0616]: field `entries` of struct `resident_index::ResidentIndex`
/// //               is private
///
/// // (3) rebuild the mirror by literal to get at the `Vec`
/// let _ = ResidentIndex { entries: Vec::new() };
/// //  error[E0451]: field `entries` of struct `resident_index::ResidentIndex`
/// //               is private
///
/// // (4) mint a version-checked replacement target without the version check
/// let _ = ReplacementTarget { position: 0 };
/// //  error[E0451]: field `position` of struct
/// //               `replacement_target::ReplacementTarget` is private
///
/// // (5) mint the in-place write permission without dropping the keyed tails
/// let _ = RecordOverwrite { record_offset: 0, payload_offset: 0 };
/// //  error[E0451]: fields `record_offset` and `payload_offset` of struct
/// //               `replacement_target::RecordOverwrite` are private
///
/// // (6) fabricate the poison-check witness
/// let _: MutationPermit<VarveFile> = MutationPermit(PhantomData);
/// //  error[E0603]: tuple struct constructor `MutationPermit` is private
///
/// // (7) launder one out of a throwaway flag (round 14's closed hole,
/// //     re-checked here from a different module than the one that closed it)
/// let _: MutationPermit<VarveFile> = PoisonFlag::healthy().issue("x")?;
/// //  error[E0624]: method `issue` is private
/// ```
///
/// Retained on purpose, with the reason at the declaration: `ResidentIndex::
/// adopt_generation` (open and the rewrite paths hand over a `Vec` that already
/// describes records on disk), `ResidentIndex::truncate` (the append rollback)
/// and `ResidentIndex::entry_mut` (the in-place restamp). None of them can add
/// an entry the disk does not have, which is the property the token protects.
#[cfg(test)]
mod bypass_catalogue {
    use super::*;

    /// The legitimate route still works, and still costs the reservation: the
    /// only producer of the token is the fallible half of the append.
    #[test]
    fn the_checked_route_still_produces_the_reservation_it_should() {
        let mut index = ResidentIndex::adopt_generation(Vec::new());
        let slot = index.reserve(|| Ok(64)).expect("reservation");
        index.install(slot, sample_index_entry());
        assert_eq!(index.len(), 1);
    }

    /// And a charge that refuses yields no token at all, so the append that
    /// would have followed it cannot be spelled.
    #[test]
    fn a_refused_charge_yields_no_reservation() {
        let mut index = ResidentIndex::adopt_generation(Vec::new());
        assert!(
            index
                .reserve(|| Err(Error::AllocationFailed {
                    resource: "record index",
                    requested: 1,
                }))
                .is_err()
        );
        assert!(index.is_empty());
    }

    fn sample_index_entry() -> RecordIndexEntry {
        RecordIndexEntry {
            block_id: 1,
            block_version: 1,
            flags: 0,
            sequence: 1,
            record_offset: 0,
            payload_offset: 0,
            payload_len: 0,
            checksum: 0,
            uncompressed_len_hint: 0,
            footer_offset: None,
            prev_same_block_offset: None,
            prev_same_key_offset: None,
            committed: true,
        }
    }
}
