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
    IndexPolicy, IntegrityPolicy, KeyedBlockVec, ManifestPolicy, MatrixCellStatus,
    MatrixCommitEvent, MatrixDimensions, MatrixKey, MatrixRecoveryAction, MatrixRecoveryReport,
    MatrixResumeSignal, RecoveryPolicy, Result, SnapshotFile, VariableCompression, VarveBlock,
    VarveEncode, VarveKeyedBlock, VarveMatrixBlock, VarveMerge, VarveMigration, VarveReplaceBlock,
    WireType,
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
    use super::{Error, KeyedTails, RecordIndexEntry, Result, VarveBlock};

    #[derive(Clone, Copy, Debug)]
    pub struct ReplacementTarget {
        position: usize,
    }

    impl ReplacementTarget {
        /// Selects the `ordinal`-th record of `T::ID` and refuses a stored
        /// `block_version` that is not `T::VERSION`.
        ///
        /// This is the only way to build a [`ReplacementTarget`].
        pub(super) fn resolve<T: VarveBlock>(
            index: &[RecordIndexEntry],
            ordinal: usize,
        ) -> Result<Self> {
            let position = index
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.block_id == T::ID)
                .nth(ordinal)
                .map(|(position, _)| position)
                .ok_or(Error::UnexpectedEof)?;
            let actual = index[position].block_version;
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
        pub(super) fn prepare(
            target: ReplacementTarget,
            index: &[RecordIndexEntry],
            keyed_tails: &mut KeyedTails,
        ) -> Self {
            let entry = &index[target.position()];
            keyed_tails.invalidate(entry.block_id);
            Self {
                record_offset: entry.record_offset,
                payload_offset: entry.payload_offset,
            }
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
    use std::fs::{File, Metadata};
    use std::io::{Seek, SeekFrom, Write};

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
        pub(super) fn append_record_at_end(
            &mut self,
            _reserved: &ReservedIndexSlot,
            expected_offset: u64,
            header_bytes: &[u8],
            payload: &[u8],
            footer: Option<&[u8]>,
            after_header: impl FnOnce() -> Result<()>,
        ) -> Result<()> {
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
            Ok(())
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
    use core::ops::Deref;

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

    impl Deref for ResidentIndex {
        type Target = [RecordIndexEntry];

        fn deref(&self) -> &[RecordIndexEntry] {
            &self.entries
        }
    }
}

use resident_index::{ReservedIndexSlot, ResidentIndex};

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

#[derive(Clone, Debug)]
struct StoredPayload {
    flags: u16,
    uncompressed_len_hint: u32,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SequenceState {
    Available(u64),
    Exhausted,
}

impl SequenceState {
    fn from_index(index: &[RecordIndexEntry]) -> Self {
        match index.iter().map(|entry| entry.sequence).max() {
            None => Self::Available(0),
            Some(u64::MAX) => Self::Exhausted,
            Some(sequence) => Self::Available(sequence + 1),
        }
    }

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
            if entry.block_id != COMMIT_BLOCK_ID {
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
    /// entries it serialized); commit markers never alter checkpoint identity;
    /// every other record grows the eligible tail by one.
    fn note_appended(&mut self, position: usize, block_id: u32) {
        note_checkpoint_cadence_index_touches(1);
        if block_id == INDEX_BLOCK_ID {
            self.eligible_since_checkpoint = 0;
            self.next_threshold = core::cmp::max(INDEX_CHECKPOINT_MIN_RECORDS, position / 2);
        } else if block_id != COMMIT_BLOCK_ID {
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
    fn from_index(index: &[RecordIndexEntry]) -> Self {
        note_block_tail_index_touches(index.len() as u64);
        let mut newest: HashMap<u32, u64> = HashMap::new();
        for entry in index {
            newest.insert(entry.block_id, entry.record_offset);
        }
        let mut tails: Vec<(u32, u64)> = newest.into_iter().collect();
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

    pub fn index_entries(&self) -> &[RecordIndexEntry] {
        self.file.index_entries()
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

    pub fn index_entries(&self) -> &[RecordIndexEntry] {
        self.file.index_entries()
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
        let mut lock = WriterLock::acquire(&path)?;
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
        write_file_header(spec, &mut file)?;
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        let mut file = Self {
            spec,
            path,
            file: RecordFile::new(file),
            snapshot,
            mode: OpenMode::ReadWrite,
            index: ResidentIndex::adopt_generation(Vec::new()),
            matrix: None,
            matrix_creation_nonce: None,
            sequence_state: SequenceState::Available(0),
            checkpoint_cadence: CheckpointCadence::new_empty(),
            block_tails: BlockTails::new_empty(),
            keyed_tails: KeyedTails::new_empty(),
            // DUR3-01: this handle established the pathname, so its
            // directory entry is not durable until the first durability
            // request syncs the parent directory.
            pending_pathname_parent_sync: true,
            poison: PoisonFlag::healthy(),
            _lock: Some(lock),
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
        let path = path.to_path_buf();
        let mut lock = WriterLock::acquire(&path)?;
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
        write_file_header(spec, &mut file)?;
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
            matrix: Some(matrix),
            matrix_creation_nonce: Some(creation_nonce),
            sequence_state: SequenceState::Available(0),
            checkpoint_cadence: CheckpointCadence::new_empty(),
            block_tails: BlockTails::new_empty(),
            keyed_tails: KeyedTails::new_empty(),
            // DUR3-01: this handle established the pathname, so its
            // directory entry is not durable until the first durability
            // request syncs the parent directory.
            pending_pathname_parent_sync: true,
            poison: PoisonFlag::healthy(),
            _lock: Some(lock),
        };
        file.write_embedded_manifest_if_needed()?;
        Ok(file)
    }

    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        let mut lock = WriterLock::acquire(&path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        lock.bind_native(&file, &path)?;
        let captured_len = check_open_file_len(spec, &file)?;
        let header_len = read_file_header(spec, &mut file)?;
        let (matrix, matrix_creation_nonce) = split_matrix_state(read_matrix_layout_if_needed(
            spec,
            &mut file,
            header_len,
            captured_len,
        )?);
        let append_start = append_log_start(header_len, matrix.as_ref());
        let index = load_index(spec, &mut file, append_start, ScanIntent::Writer)?;
        truncate_uncommitted_tail_if_needed(spec, &mut file, append_start, &index)?;
        let sequence_state = SequenceState::from_index(&index);
        let checkpoint_cadence = CheckpointCadence::from_index(&index);
        let block_tails = BlockTails::from_index(&index);
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        Ok(Self {
            spec,
            path,
            file: RecordFile::new(file),
            snapshot,
            mode: OpenMode::ReadWrite,
            index: ResidentIndex::adopt_generation(index),
            matrix,
            matrix_creation_nonce,
            sequence_state,
            checkpoint_cadence,
            block_tails,
            keyed_tails: KeyedTails::new_empty(),
            pending_pathname_parent_sync: false,
            poison: PoisonFlag::healthy(),
            _lock: Some(lock),
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
        let mut lock = WriterLock::acquire_with_policy(&path, policy)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        lock.bind_native(&file, &path)?;
        let captured_len = check_open_file_len(spec, &file)?;
        let header_len = read_file_header(spec, &mut file)?;
        let (matrix, matrix_creation_nonce) = split_matrix_state(read_matrix_layout_if_needed(
            spec,
            &mut file,
            header_len,
            captured_len,
        )?);
        let append_start = append_log_start(header_len, matrix.as_ref());
        let index = load_index(spec, &mut file, append_start, ScanIntent::Writer)?;
        truncate_uncommitted_tail_if_needed(spec, &mut file, append_start, &index)?;
        let sequence_state = SequenceState::from_index(&index);
        let checkpoint_cadence = CheckpointCadence::from_index(&index);
        let block_tails = BlockTails::from_index(&index);
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        Ok(Self {
            spec,
            path,
            file: RecordFile::new(file),
            snapshot,
            mode: OpenMode::ReadWrite,
            index: ResidentIndex::adopt_generation(index),
            matrix,
            matrix_creation_nonce,
            sequence_state,
            checkpoint_cadence,
            block_tails,
            keyed_tails: KeyedTails::new_empty(),
            pending_pathname_parent_sync: false,
            poison: PoisonFlag::healthy(),
            _lock: Some(lock),
        })
    }

    pub fn open_readonly<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new().read(true).open(&path)?;
        let captured_len = check_open_file_len(spec, &file)?;
        let header_len = read_file_header(spec, &mut file)?;
        let (matrix, matrix_creation_nonce) = split_matrix_state(read_matrix_layout_if_needed(
            spec,
            &mut file,
            header_len,
            captured_len,
        )?);
        let append_start = append_log_start(header_len, matrix.as_ref());
        let index = load_index(spec, &mut file, append_start, ScanIntent::ReadOnly)?;
        let sequence_state = SequenceState::from_index(&index);
        let checkpoint_cadence = CheckpointCadence::from_index(&index);
        let block_tails = BlockTails::from_index(&index);
        let logical_len = validated_snapshot_len(append_start, &index)?;
        let snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, logical_len)?;
        Ok(Self {
            spec,
            path,
            file: RecordFile::new(file),
            snapshot,
            mode: OpenMode::ReadOnly,
            index: ResidentIndex::adopt_generation(index),
            matrix,
            matrix_creation_nonce,
            sequence_state,
            checkpoint_cadence,
            block_tails,
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
        let mut lock = WriterLock::acquire(&path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        lock.bind_native(&file, &path)?;
        let original_len = file.metadata()?.len();
        spec.read_limits
            .check(ReadLimitKey::FileLen, original_len)?;
        let header_len = read_file_header(spec, &mut file)?;
        let (matrix, matrix_creation_nonce) = split_matrix_state(read_matrix_layout_if_needed(
            spec,
            &mut file,
            header_len,
            original_len,
        )?);
        let append_start = append_log_start(header_len, matrix.as_ref());
        let index = load_index(spec, &mut file, append_start, ScanIntent::Recover)?;
        truncate_uncommitted_tail_if_needed(spec, &mut file, append_start, &index)?;
        let recovered_len = file.metadata()?.len();
        let sequence_state = SequenceState::from_index(&index);
        let checkpoint_cadence = CheckpointCadence::from_index(&index);
        let block_tails = BlockTails::from_index(&index);
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
                matrix,
                matrix_creation_nonce,
                sequence_state,
                checkpoint_cadence,
                block_tails,
                keyed_tails: KeyedTails::new_empty(),
                pending_pathname_parent_sync: false,
                poison: PoisonFlag::healthy(),
                _lock: Some(lock),
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

    pub fn metadata(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut found = None;
        let mut budget = MaterializationBudget::new(self.spec);
        for (record_ordinal, entry) in self.index.iter().enumerate() {
            if entry.block_id != METADATA_BLOCK_ID {
                continue;
            }
            let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
            budget.consume(logical_len)?;
            let payload = entry.read_payload_snapshot(self.spec, &self.snapshot)?;
            let (stored_key, value): (String, Vec<u8>) =
                budget.decode(&payload, self.spec.endian)?;
            if stored_key == key {
                let order = MergeOrder::for_record(0, entry.sequence, record_ordinal);
                let should_replace = found
                    .as_ref()
                    .is_none_or(|(old_order, _): &(MergeOrder, Vec<u8>)| order >= *old_order);
                if should_replace {
                    found = Some((order, value));
                }
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
        let target = &self.index[target_position];
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

        let (temp_path, mut temp_file) = create_rewrite_temp_file(&self.path)?;
        let prepare_result = (|| -> Result<()> {
            if let Ok(metadata) = self.file.metadata() {
                temp_file.set_permissions(metadata.permissions())?;
            }
            write_file_header(self.spec, &mut temp_file)?;
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
    pub fn replace_fixed<T: VarveBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        let _permit = self.ensure_write()?;
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
        let old_len = self.index[target_position].payload_len;
        if old_len != payload_len {
            return Err(Error::ReplaceSizeMismatch {
                old: old_len,
                new: payload_len,
            });
        }
        self.spec
            .read_limits
            .check(ReadLimitKey::FileLen, self.snapshot.len())?;
        self.validate_source_generation()?;

        let sequence = self.sequence_state.available()?;
        let entry = &self.index[target_position];
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

        let mut new_index = clone_matching_entries(self.spec, &self.index, |_| true)?;
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
        let entry = &self.index[target_position];
        if entry.payload_len != payload_len {
            return Err(Error::ReplaceSizeMismatch {
                old: entry.payload_len,
                new: payload_len,
            });
        }
        self.validate_source_generation()?;
        let sequence = self.sequence_state.available()?;
        let entry = &self.index[target_position];
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
        let replacement = encode_logical_payload_limited(self.spec, block, endian)?;
        let replacement = prepare_user_record_payload(self.spec, T::ID, T::KIND, &replacement)?;
        let replacement_len = u64::try_from(replacement.bytes.len())
            .map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, replacement_len)?;
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
            write_file_header(self.spec, &mut temp_file)?;
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
                native_file_header_len(
                    self.spec,
                    u64::try_from(file_header_extensions(self.spec)?.len()).map_err(|_| {
                        Error::ResourceArithmeticOverflow {
                            resource: "file-header length",
                        }
                    })?,
                ),
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
            return Ok(AppendInfo::from(entry));
        }
        self.write_commit_marker()
    }

    pub fn commit_durable(&mut self) -> Result<AppendInfo> {
        let _permit = self.ensure_write()?;
        if !self.spec.commit_policy.is_transaction_marker() {
            return Err(Error::InvalidFormatSpec(
                "commit markers require transaction_marker commit policy",
            ));
        }
        self.write_embedded_manifest_if_needed()?;
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

    pub fn blocks<T: VarveBlock>(&self) -> Result<BlockVec<T>> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let entries =
            clone_matching_entries(self.spec, &self.index, |entry| entry.block_id == T::ID)?;
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

    pub fn index_entries(&self) -> &[RecordIndexEntry] {
        &self.index
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

    pub fn write_matrix_cell<T: VarveMatrixBlock>(
        &mut self,
        key: MatrixKey,
        value: &T,
    ) -> Result<()> {
        let _permit = self.ensure_write()?;
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

    pub fn read_matrix_cell<T: VarveMatrixBlock>(&self, key: MatrixKey) -> Result<T> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::read_cell(self.spec, matrix, self.file.matrix_region_reader(), key)
    }

    pub fn matrix_cell_payload<T: VarveMatrixBlock>(&self, key: MatrixKey) -> Result<Vec<u8>> {
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
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::cell_status::<T>(self.spec, matrix, key)
    }

    pub fn commit_matrix_cell<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        let _permit = self.ensure_write()?;
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::commit_cell::<T>(self.spec, matrix, self.file.matrix_region(), key)
        };
        self.finish_matrix_mutation(result)
    }

    pub fn clear_matrix_cell<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        let _permit = self.ensure_write()?;
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::clear_cell::<T>(self.spec, matrix, self.file.matrix_region(), key)
        };
        self.finish_matrix_mutation(result)
    }

    pub fn clear_matrix_cell_by_category(&mut self, category: &str, key: MatrixKey) -> Result<()> {
        let _permit = self.ensure_write()?;
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

    pub fn clear_matrix_category(&mut self, category: &str) -> Result<u64> {
        let _permit = self.ensure_write()?;
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::clear_category(self.spec, matrix, self.file.matrix_region(), category)
        };
        self.finish_matrix_mutation(result)
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

    pub fn rebuild_matrix_commit_from_crc<T: VarveMatrixBlock>(&mut self) -> Result<u64> {
        let _permit = self.ensure_write()?;
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

    pub fn matrix_resume_signal(&self, category: &str) -> Result<MatrixResumeSignal> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::resume_signal(matrix, category)
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
        self.spec
            .read_limits
            .check(ReadLimitKey::FileLen, mapped_len)?;
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
        for entry in self.index.iter() {
            validate_mmap_index_entry(entry, current_len)?;
        }
        let map_len =
            usize::try_from(mapped_len).map_err(|_| Error::LengthOverflow { value: mapped_len })?;
        // SAFETY: The caller guarantees that the cloned backing object remains
        // immutable and valid for the mapping's entire lifetime.
        let mmap = unsafe { memmap2::MmapOptions::new().len(map_len).map(&file)? };
        for entry in self.index.iter() {
            validate_mmap_index_entry(entry, mapped_len)?;
        }
        let mut index = Vec::new();
        index
            .try_reserve_exact(self.index.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "mmap index",
                requested: mmap_index_bytes,
            })?;
        index.extend(self.index.iter().cloned());
        // Records occupy contiguous ascending physical extents, so the copied
        // index is strictly offset-ordered; `payload_window` relies on this
        // for its binary-search membership check (PERF2-07).
        debug_assert!(
            index
                .windows(2)
                .all(|pair| pair[0].record_offset < pair[1].record_offset),
            "resident index must be strictly ordered by record offset",
        );
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
        for (position, entry) in self.index.iter().enumerate() {
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
        self.spec
            .read_limits
            .check(ReadLimitKey::FileLen, mapped_len)?;
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
        let write = RecordOverwrite::prepare(target, &self.index, &mut self.keyed_tails);
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
                self.checkpoint_cadence = CheckpointCadence::from_index(&self.index);
                self.block_tails = BlockTails::from_index(&self.index);
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
                self.checkpoint_cadence = CheckpointCadence::from_index(&self.index);
                self.block_tails = BlockTails::from_index(&self.index);
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
            &self.index,
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
        let index_slot = self.index.reserve(|| {
            let index_bytes = index_bytes_for_count(record_count)?;
            spec.read_limits
                .check(ReadLimitKey::IndexBytes, index_bytes)?;
            Ok(index_bytes)
        })?;

        let sequence = self.sequence_state.available()?;
        let snapshot = AppendSnapshot {
            eof: self.file.metadata()?.len(),
            cursor: self.file.stream_position()?,
            sequence_state: self.sequence_state,
            index_len: self.index.len(),
            checkpoint_cadence: self.checkpoint_cadence,
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
        self.spec
            .read_limits
            .check(ReadLimitKey::FileLen, prospective_len)?;
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
        let footer_bytes = footer.as_deref().unwrap_or(&[]);
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
            footer.as_deref(),
            || {
                #[cfg(test)]
                fail_append_after_header_if_requested()?;
                Ok(())
            },
        );
        if let Err(error) = write_result {
            return Err(self.rollback_append(snapshot, error));
        }

        let new_snapshot = match self.snapshot.with_len(prospective_len) {
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
        self.index.install(index_slot, entry);
        // Single index append site: keep the O(1) flush-cadence state in
        // lockstep with the resident index (PERF2-02), and the block tails
        // with it (PERF2-05). Both run only after the record is durable in the
        // append log, so the rollback path never has to undo them.
        self.checkpoint_cadence
            .note_appended(self.index.len() - 1, block_id);
        self.block_tails.note_appended(block_id, record_offset);
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
        if truncated {
            self.block_tails = BlockTails::from_index(&self.index);
            self.keyed_tails.invalidate_all();
        }
        self.sequence_state = snapshot.sequence_state;
        self.checkpoint_cadence = snapshot.checkpoint_cadence;

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
            payload.push(u8::from(entry.committed));
        }
        self.write_record(INDEX_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, &payload)
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
        // Every entry after the last commit marker is by definition not a
        // commit marker, so "any non-commit entry after the last commit"
        // reduces to "the newest entry is not a commit marker". The former
        // reverse search re-walked the index on every flush, adding another
        // O(N^2) cumulative flush cost for marker-on-flush formats (PERF2-02).
        self.index
            .last()
            .is_some_and(|entry| entry.block_id != COMMIT_BLOCK_ID)
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
    fn needs_index_checkpoint(&self) -> bool {
        // Rule (a): commit markers never grow the eligible tail, so an
        // unchanged checkpoint is suppressed by `eligible_since_checkpoint`
        // staying at zero. Rule (b): the threshold was derived from the last
        // checkpoint's position, whose entry count is a faithful proxy for
        // that checkpoint's byte cost.
        let new_records = self.checkpoint_cadence.eligible_since_checkpoint;
        new_records != 0 && new_records >= self.checkpoint_cadence.next_threshold
    }
}

fn append_log_start_for_file(file: &VarveFile) -> Result<u64> {
    if let Some(matrix) = &file.matrix {
        return Ok(matrix.append_log_start());
    }
    let extension_len = u64::try_from(file_header_extensions(file.spec)?.len()).map_err(|_| {
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
            None,
            None,
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
            let payload = actual.read_payload_file_with_len(file, file_len)?;
            inspect_index_checkpoint(
                spec,
                &payload,
                append_start,
                file_len,
                &actual,
                &expected_entries[..position],
            )?;
            if strict_checkpoints {
                let checkpoint = decode_index_checkpoint(spec, &payload, file_len)?;
                validate_index_checkpoint(
                    append_start,
                    file_len,
                    &actual,
                    &checkpoint,
                    &expected_entries[..position],
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
    spec.read_limits.check(ReadLimitKey::FileLen, record_end)?;
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
    spec.read_limits.check(ReadLimitKey::FileLen, record_end)?;

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
    let footer = if spec.spec_needs_record_footer() {
        let footer = encode_record_footer(RecordFooterFields {
            prev_same_block_offset: entry.prev_same_block_offset,
            prev_same_key_offset: entry.prev_same_key_offset,
        })?;
        output.write_all(&footer)?;
        footer
    } else {
        Vec::new()
    };
    validate_replacement_predecessors(output, &entry, rewritten_prefix)?;

    entry.checksum = match spec.integrity_policy {
        IntegrityPolicy::None => 0,
        IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader => {
            checksum_record_file(spec, output, &entry, &footer)?
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

fn validate_replacement_predecessors(
    output: &mut File,
    entry: &RecordIndexEntry,
    rewritten_prefix: &[RecordIndexEntry],
) -> Result<()> {
    if let Some(offset) = entry.prev_same_block_offset {
        let Some(previous) = rewritten_prefix
            .iter()
            .find(|candidate| candidate.record_offset == offset)
        else {
            return Err(Error::InvalidRecordFooter { offset });
        };
        if offset >= entry.record_offset || previous.block_id != entry.block_id {
            return Err(Error::InvalidRecordFooter { offset });
        }
    }
    if let Some(offset) = entry.prev_same_key_offset {
        let Some(previous) = rewritten_prefix
            .iter()
            .find(|candidate| candidate.record_offset == offset)
        else {
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
        payload.push(u8::from(entry.committed));
    }
    Ok(payload)
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
}

fn index_policy_from_byte(value: u8) -> Result<IndexPolicy> {
    match value {
        1 => Ok(IndexPolicy::ScanOnOpen),
        2 => Ok(IndexPolicy::CheckpointOnFlush),
        3..=15 => Ok(IndexPolicy::new(
            value & 0x01 != 0,
            value & 0x02 != 0,
            value & 0x04 != 0,
            value & 0x08 != 0,
        )),
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

fn prepare_user_record_payload(
    spec: FormatSpec,
    block_id: u32,
    kind: BlockKind,
    logical_payload: &[u8],
) -> Result<StoredPayload> {
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
        bytes: stored_bytes,
    })
}

fn uncompressed_user_payload(logical_payload: &[u8]) -> StoredPayload {
    StoredPayload {
        flags: 0,
        uncompressed_len_hint: 0,
        bytes: logical_payload.to_vec(),
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

fn validate_file_header_extensions(spec: FormatSpec, extensions: &[u8]) -> Result<()> {
    let expected = file_header_extensions(spec)?;
    if expected == extensions {
        return Ok(());
    }
    if expected.is_empty() && extensions.is_empty() {
        return Ok(());
    }
    Err(Error::InvalidCompressionHeader)
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
    entries: &[RecordIndexEntry],
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

pub(crate) fn write_file_header(spec: FormatSpec, file: &mut File) -> Result<()> {
    let extensions = file_header_extensions(spec)?;
    write_native_file_header(file, spec, &extensions)?;
    Ok(())
}

pub(crate) fn read_file_header(spec: FormatSpec, file: &mut File) -> Result<u64> {
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
        Ok(header.header_len)
    } else {
        if uses_file_explicit_compression(spec) {
            return Err(Error::InvalidCompressionHeader);
        }
        Ok(native_file_header_len(spec, 0))
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

pub(crate) fn check_open_file_len(spec: FormatSpec, file: &File) -> Result<u64> {
    let file_len = file.metadata()?.len();
    spec.read_limits.check(ReadLimitKey::FileLen, file_len)?;
    Ok(file_len)
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
        self.advanced = advanced;
        Ok(())
    }
}

fn read_record_entry_at(
    spec: FormatSpec,
    file: &mut File,
    file_len: u64,
    offset: u64,
    partial_boundary: Option<u64>,
    checksum_boundary: Option<u64>,
    accounting: &mut ScanAccounting,
) -> Result<RecordRead> {
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

    if matches!(
        spec.integrity_policy,
        IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader
    ) {
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
            partial_boundary,
            None,
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
            OP_BLOCK_ID | INDEX_BLOCK_ID | COMMIT_BLOCK_ID
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
        partial_boundary,
        None,
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
    let footer = if spec.spec_needs_record_footer() {
        encode_record_footer(RecordFooterFields {
            prev_same_block_offset,
            prev_same_key_offset,
        })?
    } else {
        Vec::new()
    };
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
    let checksum = checksum_record_fields(spec, record_offset, header, payload, &footer)?;
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
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total_len)
        .map_err(|_| Error::AllocationFailed {
            resource: "record buffer",
            requested: total_len as u64,
        })?;
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&footer);
    Ok(PreparedStreamRecord {
        bytes,
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

fn load_index(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    intent: ScanIntent,
) -> Result<Vec<RecordIndexEntry>> {
    // Sequence uniqueness is validated exactly once, inside
    // `scan_records_from`, on the complete scanned entry list *before* any
    // commit-boundary truncation; a truncated prefix of a duplicate-free list
    // is still duplicate-free, so revalidating here would only repeat the
    // N-element copy+sort on every open (PERF2-07).
    scan_records_from(spec, file, header_len, intent)
}

fn validated_snapshot_len(append_start: u64, entries: &[RecordIndexEntry]) -> Result<u64> {
    match entries.last() {
        Some(entry) => entry.checked_physical_end(),
        None => Ok(append_start),
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
    let committed_end = match entries
        .iter()
        .rev()
        .find(|entry| entry.block_id == COMMIT_BLOCK_ID)
    {
        Some(entry) => entry.checked_physical_end()?,
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

fn inspect_index_checkpoint(
    spec: FormatSpec,
    payload: &[u8],
    header_len: u64,
    file_len: u64,
    checkpoint_record: &RecordIndexEntry,
    observed_prefix: &[RecordIndexEntry],
) -> Result<()> {
    match decode_index_checkpoint(spec, payload, file_len) {
        Ok(checkpoint) => {
            let _ = validate_index_checkpoint(
                header_len,
                file_len,
                checkpoint_record,
                &checkpoint,
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

fn validate_index_checkpoint(
    header_len: u64,
    file_len: u64,
    checkpoint_record: &RecordIndexEntry,
    checkpoint: &IndexCheckpoint,
    observed_prefix: &[RecordIndexEntry],
) -> Result<()> {
    if checkpoint.covered_offset != checkpoint_record.record_offset
        || checkpoint.covered_offset < header_len
        || checkpoint.covered_offset > file_len
    {
        return Err(Error::InvalidIndexCheckpoint);
    }
    if checkpoint.entries.len() != observed_prefix.len()
        || !checkpoint.entries.iter().zip(observed_prefix).all(
            |(checkpoint_entry, observed_entry)| {
                record_headers_match(checkpoint_entry, observed_entry)
            },
        )
    {
        return Err(Error::InvalidIndexCheckpoint);
    }

    let mut previous_offset = header_len;
    for entry in &checkpoint.entries {
        let expected_payload_offset = entry
            .record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::InvalidIndexCheckpoint)?;
        if entry.record_offset < header_len
            || entry.record_offset < previous_offset
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
) -> Result<Vec<RecordIndexEntry>> {
    let file_len = file.metadata()?.len();
    spec.read_limits.check(ReadLimitKey::FileLen, file_len)?;
    let mut offset = header_len;
    let mut entries = Vec::new();
    let mut latest_commit_position = None;
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
        let entry = match read_record_entry_at(
            spec,
            file,
            file_len,
            offset,
            partial_boundary,
            checksum_boundary,
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
        entries
            .try_reserve(1)
            .map_err(|_| Error::AllocationFailed {
                resource: "record index",
                requested: index_bytes,
            })?;
        if entry.block_id == INDEX_BLOCK_ID
            && (1..=INDEX_CHECKPOINT_VERSION).contains(&entry.block_version)
        {
            let payload = entry.read_payload_file_with_len(file, file_len)?;
            inspect_index_checkpoint(spec, &payload, header_len, file_len, &entry, &entries)?;
        }
        if entry.block_id == COMMIT_BLOCK_ID {
            latest_commit_position = Some(entries.len());
            latest_commit_end = Some(offset);
        }
        entries.push(entry);
    }
    // Single sequence-uniqueness witness for the whole load path: `load_index`
    // relies on this check and must not repeat it (PERF2-07). It runs on the
    // full scanned list, so the commit-boundary truncation below can only
    // shrink an already-validated set.
    validate_unique_sequences(&entries)?;
    if spec.commit_policy.is_transaction_marker() {
        let Some(position) = latest_commit_position else {
            return Ok(Vec::new());
        };
        entries.truncate(position + 1);
        for entry in &mut entries {
            entry.committed = true;
        }
    }
    Ok(entries)
}

impl RecordIndexEntry {
    fn read_payload_file_with_len(&self, file: &mut File, file_len: u64) -> Result<Vec<u8>> {
        self.validate_payload_extent(file_len)?;
        self.read_payload_file_validated(file)
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

fn clone_matching_entries<F>(
    spec: FormatSpec,
    source: &[RecordIndexEntry],
    mut matches: F,
) -> Result<Vec<RecordIndexEntry>>
where
    F: FnMut(&RecordIndexEntry) -> bool,
{
    let count = source.iter().filter(|entry| matches(entry)).count();
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
    Ok(entries)
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

fn encode_record_footer(footer: RecordFooterFields) -> Result<Vec<u8>> {
    let bytes = encode_native_record_footer(footer)?;
    debug_assert_eq!(bytes.len() as u64, RECORD_FOOTER_LEN);
    Ok(bytes)
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
    // Authoritative single-writer lock, held on the native file object itself.
    // Hard links and other path aliases all resolve to the same object, so an
    // object lock cannot be bypassed the way the path-derived ".lock" marker
    // can. The marker file above remains diagnostic metadata (pid, timestamps,
    // break-policy machinery) plus a fast same-path exclusion.
    native_guard: Option<File>,
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
            let _ = clear_writer_lock_info(&mut file);
            return Err(error);
        }
        Ok(Self { file, native_guard })
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
        self.native_guard = None;
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
fn try_lock_native_guard(file: &File) -> std::result::Result<(), WriterGuardLockError> {
    const NATIVE_WRITER_GUARD_OFFSET: u64 = u64::MAX - 1;
    try_lock_exclusive_range(file, NATIVE_WRITER_GUARD_OFFSET)
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
    let mut lock = WriterLock::acquire_with_policy(target_path.as_ref(), policy)?;
    clear_writer_lock_info(&mut lock.file)?;
    Ok(())
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
        let _ = clear_writer_lock_info(&mut self.file);
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
    use super::*;
    use crate::{VarveDecode, VarveEncode};

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
        let keyed: Vec<_> = writer
            .index_entries()
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
