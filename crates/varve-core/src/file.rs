use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions, remove_file};
use std::hash::Hash;
use std::io::{Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{
    BlockDescriptor, BlockKind, BlockVec, CommitPolicy, CompressionAlgorithm,
    CompressionHeaderMode, CompressionLevel, CompressionPolicy, Endian, Error, FormatSpec,
    IndexPolicy, IntegrityPolicy, KeyedBlockVec, ManifestPolicy, MatrixCellStatus,
    MatrixCommitEvent, MatrixDimensions, MatrixKey, MatrixRecoveryAction, MatrixRecoveryReport,
    MatrixResumeSignal, RecoveryPolicy, Result, VariableCompression, VarveBlock, VarveKeyedBlock,
    VarveMatrixBlock, VarveMerge, VarveMigration, WireType, decode_from_slice, encode_to_vec,
    native_layout::{
        decode_native_record_footer, encode_native_record_footer, encode_native_record_header,
        native_file_header_len, native_record_footer_len, native_record_header_len,
        read_native_file_header, read_native_record_header, write_native_file_header,
        write_native_record_header,
    },
};

pub const TOMBSTONE_BLOCK_ID: u32 = 0xFFFF_FFFE;
pub const OP_BLOCK_ID: u32 = 0xFFFF_FFFD;
pub const METADATA_BLOCK_ID: u32 = 0xFFFF_FFFC;
pub const INDEX_BLOCK_ID: u32 = 0xFFFF_FFFB;
pub const MANIFEST_BLOCK_ID: u32 = 0xFFFF_FFFA;
pub const COMMIT_BLOCK_ID: u32 = 0xFFFF_FFF9;
const RESERVED_BLOCK_ID_START: u32 = 0xFFFF_FF00;
pub(crate) const RECORD_HEADER_LEN: u64 = 32;
pub(crate) const RECORD_FOOTER_LEN: u64 = 32;
const RECORD_FLAG_COMPRESSED: u16 = 0x0001;
const RECORD_FLAG_INTERNAL: u16 = 0x8000;
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
const MANIFEST_PAYLOAD_VERSION: u16 = 5;
const WRITER_LOCK_MAGIC: &str = "varve-lock-v1";
const COMPRESSION_ENVELOPE_MAGIC: &[u8; 4] = b"VCMP";
const COMPRESSION_ENVELOPE_VERSION: u8 = 1;
const FILE_COMPRESSION_MAGIC: &[u8; 4] = b"VCHD";
const FILE_COMPRESSION_VERSION: u8 = 1;
const MATRIX_SIDECAR_MAGIC: &[u8; 4] = b"VSID";
const MATRIX_SIDECAR_VERSION: u16 = 1;
const MATRIX_SIDECAR_FIXED_LEN: usize = 48;

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
    pub fn read_payload(&self, path: &Path) -> Result<Vec<u8>> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(self.payload_offset))?;
        let payload_len = usize::try_from(self.payload_len).map_err(|_| Error::LengthOverflow {
            value: self.payload_len,
        })?;
        let mut payload = vec![0; payload_len];
        file.read_exact(&mut payload)?;
        Ok(payload)
    }

    pub fn is_compressed(&self) -> bool {
        self.flags & RECORD_FLAG_COMPRESSED != 0
    }

    pub fn read_logical_payload(&self, spec: FormatSpec, path: &Path) -> Result<Vec<u8>> {
        let payload = self.read_payload(path)?;
        decode_record_payload(spec, self, &payload)
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
    FixedInPlace,
    RewriteFile,
}

#[cfg(feature = "mmap")]
#[derive(Debug)]
pub struct MmapPayloads {
    spec: FormatSpec,
    index: Vec<RecordIndexEntry>,
    entry_set: std::collections::HashSet<RecordIndexEntry>,
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
        if !self.entry_set.contains(entry) {
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

#[derive(Debug)]
pub struct VarveFile {
    spec: FormatSpec,
    path: PathBuf,
    file: File,
    mode: OpenMode,
    index: Vec<RecordIndexEntry>,
    matrix: Option<crate::matrix::MatrixLayout>,
    next_sequence: u64,
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

    pub fn read_matrix_cell<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<T> {
        self.file.read_matrix_cell(key)
    }

    pub fn matrix_cell_payload<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<Vec<u8>> {
        self.file.matrix_cell_payload::<T>(key)
    }

    pub fn matrix_aux_len(&self, name: &str) -> Result<u64> {
        self.file.matrix_aux_len(name)
    }

    pub fn read_matrix_aux(&mut self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
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
    pub fn mmap_payloads(&self) -> Result<MmapPayloads> {
        self.file.mmap_payloads()
    }

    #[cfg(feature = "mmap")]
    pub fn mmap_matrix(&self) -> Result<MmapMatrix> {
        self.file.mmap_matrix()
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

    pub fn create_with_dims<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<Self> {
        Ok(Self {
            file: VarveFile::create_with_dims(spec, path, dims)?,
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

    pub fn push<T: VarveBlock>(&mut self, block: &T) -> Result<u64> {
        self.file.push(block)
    }

    pub fn push_info<T: VarveBlock>(&mut self, block: &T) -> Result<AppendInfo> {
        self.file.push_info(block)
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
    {
        self.file.delete::<T>(key)
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

    pub fn replace_fixed<T: VarveBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        self.file.replace_fixed(index, block)
    }

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

    pub fn copy_matrix_cell_bytes_from<From, To>(
        &mut self,
        source: &mut VarveReader,
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

    pub fn read_matrix_cell<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<T> {
        self.file.read_matrix_cell(key)
    }

    pub fn matrix_cell_payload<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<Vec<u8>> {
        self.file.matrix_cell_payload::<T>(key)
    }

    pub fn matrix_aux_len(&self, name: &str) -> Result<u64> {
        self.file.matrix_aux_len(name)
    }

    pub fn read_matrix_aux(&mut self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
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

impl VarveFile {
    pub fn create<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        spec.validate()?;
        if spec.has_matrix_blocks() {
            return Err(Error::MatrixDimensionsRequired);
        }
        let path = path.as_ref().to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        write_file_header(spec, &mut file)?;
        let mut file = Self {
            spec,
            path,
            file,
            mode: OpenMode::ReadWrite,
            index: Vec::new(),
            matrix: None,
            next_sequence: 0,
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
        spec.validate()?;
        if !spec.has_matrix_blocks() {
            return Self::create(spec, path);
        }
        let path = path.as_ref().to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        write_file_header(spec, &mut file)?;
        let header_len = file.stream_position()?;
        let matrix = crate::matrix::create_layout(spec, &mut file, header_len, &dims)?;
        let mut file = Self {
            spec,
            path,
            file,
            mode: OpenMode::ReadWrite,
            index: Vec::new(),
            matrix: Some(matrix),
            next_sequence: 0,
            _lock: Some(lock),
        };
        file.write_embedded_manifest_if_needed()?;
        Ok(file)
    }

    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        spec.validate()?;
        let path = path.as_ref().to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let header_len = read_file_header(spec, &mut file)?;
        let matrix = read_matrix_layout_if_needed(spec, &mut file, header_len)?;
        let append_start = append_log_start(header_len, matrix.as_ref());
        let index = load_index(spec, &mut file, append_start, RecoveryPolicy::Strict)?;
        truncate_uncommitted_tail_if_needed(spec, &mut file, append_start, &index)?;
        let next_sequence = index.iter().map(|entry| entry.sequence).max().unwrap_or(0) + 1;
        Ok(Self {
            spec,
            path,
            file,
            mode: OpenMode::ReadWrite,
            index,
            matrix,
            next_sequence,
            _lock: Some(lock),
        })
    }

    pub fn open_with_lock_policy<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        policy: WriterLockBreakPolicy,
    ) -> Result<Self> {
        spec.validate()?;
        let path = path.as_ref().to_path_buf();
        let lock = WriterLock::acquire_with_policy(&path, policy)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let header_len = read_file_header(spec, &mut file)?;
        let matrix = read_matrix_layout_if_needed(spec, &mut file, header_len)?;
        let append_start = append_log_start(header_len, matrix.as_ref());
        let index = load_index(spec, &mut file, append_start, RecoveryPolicy::Strict)?;
        truncate_uncommitted_tail_if_needed(spec, &mut file, append_start, &index)?;
        let next_sequence = index.iter().map(|entry| entry.sequence).max().unwrap_or(0) + 1;
        Ok(Self {
            spec,
            path,
            file,
            mode: OpenMode::ReadWrite,
            index,
            matrix,
            next_sequence,
            _lock: Some(lock),
        })
    }

    pub fn open_readonly<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        spec.validate()?;
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new().read(true).open(&path)?;
        let header_len = read_file_header(spec, &mut file)?;
        let matrix = read_matrix_layout_if_needed(spec, &mut file, header_len)?;
        let append_start = append_log_start(header_len, matrix.as_ref());
        let index = load_index(spec, &mut file, append_start, RecoveryPolicy::Strict)?;
        let next_sequence = index.iter().map(|entry| entry.sequence).max().unwrap_or(0) + 1;
        Ok(Self {
            spec,
            path,
            file,
            mode: OpenMode::ReadOnly,
            index,
            matrix,
            next_sequence,
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
        spec.validate()?;
        let path = path.as_ref().to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let original_len = file.metadata()?.len();
        let header_len = read_file_header(spec, &mut file)?;
        let matrix = read_matrix_layout_if_needed(spec, &mut file, header_len)?;
        let append_start = append_log_start(header_len, matrix.as_ref());
        let index = load_index(spec, &mut file, append_start, spec.recovery_policy)?;
        truncate_uncommitted_tail_if_needed(spec, &mut file, append_start, &index)?;
        let recovered_len = file.metadata()?.len();
        let next_sequence = index.iter().map(|entry| entry.sequence).max().unwrap_or(0) + 1;
        let records_preserved = index.len();
        Ok((
            Self {
                spec,
                path,
                file,
                mode: OpenMode::ReadWrite,
                index,
                matrix,
                next_sequence,
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
        self.spec
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn mode(&self) -> OpenMode {
        self.mode
    }

    pub fn push<T: VarveBlock>(&mut self, block: &T) -> Result<u64> {
        Ok(self.push_info(block)?.sequence)
    }

    pub fn push_info<T: VarveBlock>(&mut self, block: &T) -> Result<AppendInfo> {
        self.push_with_prev_key_info(block, None)
    }

    pub fn push_with_prev_key_info<T: VarveBlock>(
        &mut self,
        block: &T,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        self.ensure_write()?;
        self.ensure_user_block::<T>()?;
        if T::KIND == BlockKind::Matrix {
            return Err(Error::BlockKindMismatch {
                expected: BlockKind::Fixed,
                actual: T::KIND,
            });
        }
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        let payload = encode_to_vec(block, endian)?;
        self.write_user_record(T::ID, T::VERSION, T::KIND, &payload, prev_same_key_offset)
    }

    pub fn delete<T>(&mut self, key: &T::Key) -> Result<u64>
    where
        T: VarveKeyedBlock,
    {
        Ok(self.delete_with_prev_key_info::<T>(key, None)?.sequence)
    }

    pub fn delete_with_prev_key_info<T>(
        &mut self,
        key: &T::Key,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo>
    where
        T: VarveKeyedBlock,
    {
        self.ensure_write()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let payload = encode_internal_key_payload::<T>(self.spec.endian, key)?;
        self.write_record_with_prev_key(
            TOMBSTONE_BLOCK_ID,
            1,
            RECORD_FLAG_INTERNAL,
            0,
            &payload,
            prev_same_key_offset,
        )
    }

    pub fn push_op<T>(&mut self, key: &T::Key, op: &T::Op) -> Result<u64>
    where
        T: VarveMerge,
    {
        self.ensure_write()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let payload = encode_internal_op_payload::<T>(self.spec.endian, key, op)?;
        self.write_record(OP_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, &payload)
    }

    pub fn write_metadata(&mut self, key: &str, value: &[u8]) -> Result<u64> {
        self.ensure_write()?;
        let payload = encode_to_vec(&(key.to_string(), value.to_vec()), self.spec.endian)?;
        self.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, &payload)
    }

    pub fn metadata(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut found = None;
        for entry in self
            .index
            .iter()
            .filter(|entry| entry.block_id == METADATA_BLOCK_ID)
        {
            let payload = entry.read_payload(&self.path)?;
            let (stored_key, value): (String, Vec<u8>) =
                decode_from_slice(&payload, self.spec.endian)?;
            if stored_key == key {
                let should_replace = found
                    .as_ref()
                    .is_none_or(|(sequence, _): &(u64, Vec<u8>)| entry.sequence >= *sequence);
                if should_replace {
                    found = Some((entry.sequence, value));
                }
            }
        }
        Ok(found.map(|(_, value)| value))
    }

    pub fn all_metadata(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let mut values = Vec::new();
        for entry in self
            .index
            .iter()
            .filter(|entry| entry.block_id == METADATA_BLOCK_ID)
        {
            let payload = entry.read_payload(&self.path)?;
            values.push(decode_from_slice(&payload, self.spec.endian)?);
        }
        Ok(values)
    }

    pub fn schema_manifest(&self) -> Result<Option<SchemaManifest>> {
        let Some(entry) = self
            .index
            .iter()
            .filter(|entry| entry.block_id == MANIFEST_BLOCK_ID)
            .max_by_key(|entry| entry.sequence)
        else {
            return Ok(None);
        };
        let payload = entry.read_payload(&self.path)?;
        Ok(Some(decode_schema_manifest(&payload)?))
    }

    pub fn replace_fixed<T: VarveBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        self.ensure_write()?;
        if self.spec.spec_needs_record_footer() {
            return Err(Error::InvalidFormatSpec(
                "replace is not supported for record-footer formats",
            ));
        }
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        if T::KIND != BlockKind::Fixed {
            return Err(Error::BlockKindMismatch {
                expected: BlockKind::Fixed,
                actual: T::KIND,
            });
        }
        let target_position = self
            .index
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.block_id == T::ID)
            .nth(index)
            .map(|(position, _)| position)
            .ok_or(Error::UnexpectedEof)?;

        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        let payload = encode_to_vec(block, endian)?;
        let old_len = self.index[target_position].payload_len;
        if old_len != payload.len() as u64 {
            return Err(Error::ReplaceSizeMismatch {
                old: old_len,
                new: payload.len() as u64,
            });
        }

        let sequence = self.next_sequence();
        let entry = &mut self.index[target_position];
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
            checksum_record_fields(self.spec, entry.record_offset, header, &payload, &[])?;
        entry.sequence = sequence;
        entry.checksum = checksum;
        self.file.seek(SeekFrom::Start(entry.record_offset))?;
        write_record_header(
            &mut self.file,
            self.spec,
            entry.record_offset,
            RecordHeaderFields { checksum, ..header },
        )?;
        self.file.seek(SeekFrom::Start(entry.payload_offset))?;
        self.file.write_all(&payload)?;
        Ok(sequence)
    }

    pub fn replace_rewrite<T: VarveBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        self.ensure_write()?;
        if self.spec.spec_needs_record_footer() {
            return Err(Error::InvalidFormatSpec(
                "replace is not supported for record-footer formats",
            ));
        }
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let target_position = self
            .index
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.block_id == T::ID)
            .nth(index)
            .map(|(position, _)| position)
            .ok_or(Error::UnexpectedEof)?;
        let sequence = self.next_sequence;
        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        let replacement = encode_to_vec(block, endian)?;
        let replacement = prepare_user_record_payload(self.spec, T::ID, T::KIND, &replacement)?;

        let mut records = Vec::with_capacity(self.index.len());
        for (position, entry) in self.index.iter().enumerate() {
            let payload = if position == target_position {
                replacement.bytes.clone()
            } else {
                entry.read_payload(&self.path)?
            };
            let mut updated = entry.clone();
            if position == target_position {
                updated.sequence = sequence;
                updated.flags = replacement.flags;
                updated.payload_len = payload.len() as u64;
                updated.uncompressed_len_hint = replacement.uncompressed_len_hint;
            }
            records.push((updated, payload));
        }

        let (temp_path, mut temp_file) = create_rewrite_temp_file(&self.path)?;
        if let Ok(metadata) = self.file.metadata() {
            temp_file.set_permissions(metadata.permissions())?;
        }
        write_file_header(self.spec, &mut temp_file)?;
        let mut new_index = Vec::with_capacity(records.len());
        for (mut entry, payload) in records {
            let offset = temp_file.stream_position()?;
            let header = RecordHeaderFields {
                block_id: entry.block_id,
                block_version: entry.block_version,
                flags: entry.flags,
                sequence: entry.sequence,
                payload_len: payload.len() as u64,
                checksum: 0,
                uncompressed_len_hint: entry.uncompressed_len_hint,
            };
            entry.checksum = checksum_record_fields(self.spec, offset, header, &payload, &[])?;
            write_record_header(
                &mut temp_file,
                self.spec,
                offset,
                RecordHeaderFields {
                    checksum: entry.checksum,
                    ..header
                },
            )?;
            temp_file.write_all(&payload)?;
            entry.record_offset = offset;
            entry.payload_offset = offset + native_record_header_len();
            entry.payload_len = payload.len() as u64;
            new_index.push(entry);
        }
        temp_file.flush()?;
        temp_file.sync_all()?;
        drop(temp_file);

        if let Err(error) = replace_path_atomically(&temp_path, &self.path) {
            let _ = remove_file(&temp_path);
            return Err(error);
        }

        self.file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        self.index = new_index;
        self.next_sequence = self.next_sequence.saturating_add(1);
        Ok(sequence)
    }

    pub fn replace<T: VarveBlock>(
        &mut self,
        index: usize,
        block: &T,
        strategy: ReplaceStrategy,
    ) -> Result<u64> {
        match strategy {
            ReplaceStrategy::FixedInPlace => self.replace_fixed(index, block),
            ReplaceStrategy::RewriteFile => self.replace_rewrite(index, block),
        }
    }

    pub fn flush(&mut self) -> Result<()> {
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
        self.ensure_write()?;
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
        self.ensure_write()?;
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
            self.file.flush()?;
            self.file.sync_all()?;
            return Ok(AppendInfo::from(entry));
        }
        self.file.flush()?;
        self.file.sync_data()?;
        let info = self.write_commit_marker()?;
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(info)
    }

    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    pub fn blocks<T: VarveBlock>(&self) -> Result<BlockVec<T>> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let entries = self
            .index
            .iter()
            .filter(|entry| entry.block_id == T::ID)
            .cloned()
            .collect();
        Ok(BlockVec::new(self.spec, self.path.clone(), entries))
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
        for entry in self
            .index
            .iter()
            .filter(|entry| entry.block_id == From::ID && entry.block_version == From::VERSION)
        {
            let payload = entry.read_logical_payload(self.spec, &self.path)?;
            let from: From = decode_from_slice(&payload, From::ENDIAN.unwrap_or(self.spec.endian))?;
            migrated.push(M::migrate(from)?);
        }
        Ok(migrated)
    }

    pub fn keyed_blocks<T>(&self) -> Result<KeyedBlockVec<T::Key, T>>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash + Clone,
    {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let mut entries = Vec::new();
        let mut by_key: HashMap<T::Key, RecordIndexEntry> = HashMap::new();
        for entry in &self.index {
            match entry.block_id {
                id if id == T::ID => {
                    if entry.block_version != T::VERSION {
                        return Err(Error::BlockVersionMismatch {
                            block_id: T::ID,
                            expected: T::VERSION,
                            actual: entry.block_version,
                        });
                    }
                    let payload = entry.read_logical_payload(self.spec, &self.path)?;
                    let block: T =
                        decode_from_slice(&payload, T::ENDIAN.unwrap_or(self.spec.endian))?;
                    let key = block.key();
                    let should_replace = by_key
                        .get(&key)
                        .is_none_or(|old| entry.sequence > old.sequence);
                    if should_replace {
                        by_key.insert(key, entry.clone());
                    }
                    entries.push(entry.clone());
                }
                TOMBSTONE_BLOCK_ID => {
                    let payload = entry.read_payload(&self.path)?;
                    if let Some(key) = decode_internal_key_payload::<T>(self.spec.endian, &payload)?
                    {
                        let should_remove = by_key
                            .get(&key)
                            .is_none_or(|old| entry.sequence > old.sequence);
                        if should_remove {
                            by_key.remove(&key);
                        }
                    }
                }
                _ => {}
            }
        }
        let blocks = BlockVec::new(self.spec, self.path.clone(), entries);
        Ok(KeyedBlockVec::from_parts(blocks, by_key))
    }

    pub fn materialized_keyed_blocks<T>(&self) -> Result<HashMap<T::Key, T>>
    where
        T: VarveMerge,
        T::Key: Eq + Hash,
    {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let mut state: HashMap<T::Key, (MergeOrder, Option<T>)> = HashMap::new();
        apply_merge_entries::<T>(
            self.spec,
            &self.path,
            &self.index,
            MergeShard::single_file(),
            &mut state,
        )?;
        Ok(state
            .into_iter()
            .filter_map(|(key, (_, value))| value.map(|value| (key, value)))
            .collect())
    }

    pub fn scan(&self) -> impl Iterator<Item = BlockEvent> + '_ {
        self.index.iter().map(BlockEvent::from)
    }

    pub fn index_entries(&self) -> &[RecordIndexEntry] {
        &self.index
    }

    pub fn key_tail_offsets<T>(&self) -> Result<HashMap<T::Key, u64>>
    where
        T: VarveKeyedBlock,
        T::Key: Eq + Hash,
    {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let mut tails = HashMap::new();
        for entry in &self.index {
            match entry.block_id {
                id if id == T::ID => {
                    let payload = entry.read_logical_payload(self.spec, &self.path)?;
                    let block: T =
                        decode_from_slice(&payload, T::ENDIAN.unwrap_or(self.spec.endian))?;
                    tails.insert(block.key(), entry.record_offset);
                }
                TOMBSTONE_BLOCK_ID => {
                    let payload = entry.read_payload(&self.path)?;
                    if let Some(key) = decode_internal_key_payload::<T>(self.spec.endian, &payload)?
                    {
                        tails.insert(key, entry.record_offset);
                    }
                }
                _ => {}
            }
        }
        Ok(tails)
    }

    pub fn write_matrix_cell<T: VarveMatrixBlock>(
        &mut self,
        key: MatrixKey,
        value: &T,
    ) -> Result<()> {
        self.ensure_write()?;
        let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::write_cell(self.spec, matrix, &mut self.file, key, value)
    }

    pub fn write_matrix_cell_payload<T: VarveMatrixBlock>(
        &mut self,
        key: MatrixKey,
        payload: &[u8],
    ) -> Result<()> {
        self.ensure_write()?;
        let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::write_cell_payload::<T>(self.spec, matrix, &mut self.file, key, payload)
    }

    pub fn copy_matrix_cell_bytes_from<From, To>(
        &mut self,
        source: &mut VarveFile,
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
        self.write_matrix_cell(key, value)?;
        barrier.sync_matrix_data(&mut self.file)?;
        self.commit_matrix_cell::<T>(key)?;
        barrier.sync_matrix_commit(&mut self.file)?;
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        let event = crate::matrix::commit_event::<T>(self.spec, matrix, key)?;
        hook(event)
    }

    pub fn read_matrix_cell<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<T> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::read_cell(self.spec, matrix, &mut self.file, key)
    }

    pub fn matrix_cell_payload<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<Vec<u8>> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::read_cell_payload::<T>(self.spec, matrix, &mut self.file, key)
    }

    pub fn matrix_aux_len(&self, name: &str) -> Result<u64> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::aux_len(matrix, name)
    }

    pub fn read_matrix_aux(&mut self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::read_aux(matrix, &mut self.file, name, offset, len)
    }

    pub fn write_matrix_aux(&mut self, name: &str, offset: u64, payload: &[u8]) -> Result<()> {
        self.ensure_write()?;
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::write_aux(matrix, &mut self.file, name, offset, payload)
    }

    pub fn matrix_cell_status<T: VarveMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::cell_status::<T>(self.spec, matrix, key)
    }

    pub fn commit_matrix_cell<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        self.ensure_write()?;
        let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::commit_cell::<T>(self.spec, matrix, &mut self.file, key)
    }

    pub fn clear_matrix_cell<T: VarveMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        self.ensure_write()?;
        let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::clear_cell::<T>(self.spec, matrix, &mut self.file, key)
    }

    pub fn clear_matrix_cell_by_category(&mut self, category: &str, key: MatrixKey) -> Result<()> {
        self.ensure_write()?;
        let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::clear_cell_by_category(self.spec, matrix, &mut self.file, category, key)
    }

    pub fn clear_matrix_category(&mut self, category: &str) -> Result<u64> {
        self.ensure_write()?;
        let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::clear_category(self.spec, matrix, &mut self.file, category)
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
        self.ensure_write()?;
        let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::rebuild_commit_map_from_crc::<T>(self.spec, matrix, &mut self.file)
    }

    pub fn is_matrix_single_committed(&self, name: &str) -> Result<bool> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::is_single_committed(matrix, name)
    }

    pub fn set_matrix_single_committed(&mut self, name: &str, value: bool) -> Result<()> {
        self.ensure_write()?;
        let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::set_single_committed(matrix, &mut self.file, name, value)
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
        self.ensure_write()?;
        let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::set_channel_committed(matrix, &mut self.file, name, channel, value)
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
        match read_matrix_sidecar_file(self.spec, category, path, expected_generation) {
            Ok(_) => crate::matrix::sidecar_resume_signal(matrix, category, true),
            Err(Error::IntegrityFeatureDisabled) => Err(Error::IntegrityFeatureDisabled),
            Err(_) => Ok(MatrixResumeSignal::DiscardRecommended),
        }
    }

    pub fn write_matrix_sidecar<P: AsRef<Path>>(
        &mut self,
        category: &str,
        sidecar_path: P,
        generation: u64,
        payload: &[u8],
    ) -> Result<MatrixSidecarManifest> {
        self.ensure_write()?;
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::resume_signal(matrix, category)?;
        write_matrix_sidecar_file(self.spec, category, sidecar_path, generation, payload)
    }

    pub fn read_matrix_sidecar<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::resume_signal(matrix, category)?;
        read_matrix_sidecar_file(self.spec, category, sidecar_path, None)
    }

    pub fn read_matrix_sidecar_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::resume_signal(matrix, category)?;
        read_matrix_sidecar_file(self.spec, category, sidecar_path, Some(expected_generation))
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

    #[cfg(feature = "mmap")]
    pub fn mmap_payloads(&self) -> Result<MmapPayloads> {
        let file = File::open(&self.path)?;
        // Mmap access is opt-in and read-only. Varve keeps owned canonical
        // decoding as the default path and treats this mapping as a snapshot
        // of the file/index at the time this method is called.
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };
        let entry_set = self.index.iter().cloned().collect();
        let mut by_block: HashMap<u32, Vec<usize>> = HashMap::new();
        for (position, entry) in self.index.iter().enumerate() {
            by_block.entry(entry.block_id).or_default().push(position);
        }
        Ok(MmapPayloads {
            spec: self.spec,
            index: self.index.clone(),
            entry_set,
            by_block,
            mmap,
        })
    }

    #[cfg(feature = "mmap")]
    pub fn mmap_matrix(&self) -> Result<MmapMatrix> {
        let layout = self
            .matrix
            .as_ref()
            .ok_or(Error::MatrixLayoutMissing)?
            .clone();
        let file = File::open(&self.path)?;
        // Matrix mmap is a read-only snapshot of the current file contents and
        // VMAT layout, separate from append-log payload mmap windows.
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };
        Ok(MmapMatrix {
            spec: self.spec,
            layout,
            mmap,
        })
    }

    fn ensure_write(&self) -> Result<()> {
        match self.mode {
            OpenMode::ReadWrite => Ok(()),
            OpenMode::ReadOnly => Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "file was opened read-only",
            ))),
        }
    }

    fn ensure_user_block<T: VarveBlock>(&self) -> Result<()> {
        if T::ID >= RESERVED_BLOCK_ID_START {
            return Err(Error::ReservedBlockId(T::ID));
        }
        Ok(())
    }

    fn next_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        sequence
    }

    fn write_record(
        &mut self,
        block_id: u32,
        block_version: u16,
        flags: u16,
        payload: &[u8],
    ) -> Result<u64> {
        Ok(self
            .write_record_with_prev_key(block_id, block_version, flags, 0, payload, None)?
            .sequence)
    }

    fn write_user_record(
        &mut self,
        block_id: u32,
        block_version: u16,
        kind: BlockKind,
        payload: &[u8],
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        let payload = prepare_user_record_payload(self.spec, block_id, kind, payload)?;
        self.write_record_with_prev_key(
            block_id,
            block_version,
            payload.flags,
            payload.uncompressed_len_hint,
            &payload.bytes,
            prev_same_key_offset,
        )
    }

    fn write_record_with_prev_key(
        &mut self,
        block_id: u32,
        block_version: u16,
        flags: u16,
        uncompressed_len_hint: u32,
        payload: &[u8],
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        let sequence = self.next_sequence();
        self.file.seek(SeekFrom::End(0))?;
        let record_offset = self.file.stream_position()?;
        let prev_same_block_offset = if self.spec.index_policy.block_offset_chain {
            self.index
                .iter()
                .rev()
                .find(|entry| entry.block_id == block_id)
                .map(|entry| entry.record_offset)
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
            payload_len: payload.len() as u64,
            checksum: 0,
            uncompressed_len_hint,
        };
        let footer_bytes = footer.as_deref().unwrap_or(&[]);
        let checksum =
            checksum_record_fields(self.spec, record_offset, header, payload, footer_bytes)?;
        write_record_header(
            &mut self.file,
            self.spec,
            record_offset,
            RecordHeaderFields { checksum, ..header },
        )?;
        self.file.write_all(payload)?;
        let footer_offset = if let Some(footer) = footer {
            let footer_offset = record_offset + RECORD_HEADER_LEN + payload.len() as u64;
            self.file.write_all(&footer)?;
            Some(footer_offset)
        } else {
            None
        };
        let committed =
            !self.spec.commit_policy.is_transaction_marker() || block_id == COMMIT_BLOCK_ID;
        let entry = RecordIndexEntry {
            block_id,
            block_version,
            flags,
            sequence,
            record_offset,
            payload_offset: record_offset + RECORD_HEADER_LEN,
            payload_len: payload.len() as u64,
            checksum,
            uncompressed_len_hint,
            footer_offset,
            prev_same_block_offset,
            prev_same_key_offset,
            committed,
        };
        let info = AppendInfo::from(&entry);
        self.index.push(entry);
        Ok(info)
    }

    fn write_index_checkpoint(&mut self) -> Result<u64> {
        self.file.seek(SeekFrom::End(0))?;
        let covered_offset = self.file.stream_position()?;
        let mut payload = Vec::new();
        let entries: Vec<_> = self.index.iter().collect();
        payload.extend_from_slice(INDEX_CHECKPOINT_MAGIC);
        payload.extend_from_slice(&INDEX_CHECKPOINT_VERSION.to_le_bytes());
        payload.extend_from_slice(&covered_offset.to_le_bytes());
        payload.extend_from_slice(&(entries.len() as u64).to_le_bytes());
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
        self.write_record_with_prev_key(
            COMMIT_BLOCK_ID,
            1,
            RECORD_FLAG_INTERNAL,
            0,
            COMMIT_PAYLOAD_MAGIC,
            None,
        )
    }

    fn has_uncommitted_since_last_commit(&self) -> bool {
        let start = self
            .index
            .iter()
            .rposition(|entry| entry.block_id == COMMIT_BLOCK_ID)
            .map_or(0, |position| position + 1);
        self.index[start..]
            .iter()
            .any(|entry| entry.block_id != COMMIT_BLOCK_ID)
    }

    fn needs_index_checkpoint(&self) -> bool {
        if self.index.is_empty() {
            return false;
        }
        let start = self
            .index
            .iter()
            .rposition(|entry| entry.block_id == INDEX_BLOCK_ID)
            .map_or(0, |position| position + 1);
        self.index[start..]
            .iter()
            .any(|entry| entry.block_id != COMMIT_BLOCK_ID)
    }
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

fn decode_schema_manifest(payload: &[u8]) -> Result<SchemaManifest> {
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
                Some(cursor.read_string(len)?)
            }
            _ => return Err(Error::InvalidSchemaManifest),
        };
        let compression_policy = decode_manifest_compression_policy(&mut cursor)?;
        (extension, compression_policy)
    } else {
        (None, CompressionPolicy::None)
    };
    let block_count = cursor.read_u32()? as usize;
    let mut blocks = Vec::new();
    for _ in 0..block_count {
        let id = cursor.read_u32()?;
        let version = cursor.read_u16()?;
        let kind = block_kind_from_byte(cursor.read_u8()?)?;
        let name_len = cursor.read_u16()? as usize;
        let name = cursor.read_string(name_len)?;
        let fields = if payload_version >= 2 {
            let field_count = cursor.read_u32()? as usize;
            let mut fields = Vec::new();
            for _ in 0..field_count {
                let id = cursor.read_u32()?;
                let wire_type = wire_type_from_byte(cursor.read_u8()?)?;
                let presence = field_presence_from_byte(cursor.read_u8()?)?;
                let name_len = cursor.read_u16()? as usize;
                let name = cursor.read_string(name_len)?;
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

    fn read_string(&mut self, len: usize) -> Result<String> {
        String::from_utf8(self.read_exact(len)?.to_vec()).map_err(|_| Error::InvalidSchemaManifest)
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
            encode_compression_envelope(compression, logical_len, &compressed)
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

fn decode_record_payload(
    spec: FormatSpec,
    entry: &RecordIndexEntry,
    physical_payload: &[u8],
) -> Result<Vec<u8>> {
    if !entry.is_compressed() {
        return Ok(physical_payload.to_vec());
    }
    validate_record_entry(spec, entry)?;
    let compression = variable_compression_for_block(spec, entry.block_id)
        .ok_or(Error::InvalidCompressionHeader)?;
    let (algorithm, expected_len, compressed_payload) = match compression.header_mode {
        CompressionHeaderMode::RecordExplicit => decode_compression_envelope(physical_payload)?,
        CompressionHeaderMode::FileExplicit | CompressionHeaderMode::FormatContract => {
            if entry.uncompressed_len_hint == 0 {
                return Err(Error::InvalidCompressionHeader);
            }
            (
                compression.algorithm,
                u64::from(entry.uncompressed_len_hint),
                physical_payload,
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
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(20 + compressed_payload.len());
    payload.extend_from_slice(COMPRESSION_ENVELOPE_MAGIC);
    payload.push(COMPRESSION_ENVELOPE_VERSION);
    payload.push(compression_algorithm_byte(compression.algorithm));
    payload.push(compression_level_kind_byte(compression.level));
    payload.push(0);
    payload.extend_from_slice(&compression_level_exact(compression.level).to_le_bytes());
    payload.extend_from_slice(&uncompressed_len.to_le_bytes());
    payload.extend_from_slice(compressed_payload);
    payload
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

pub fn merge_keyed_files<T, P>(spec: FormatSpec, base: P, deltas: &[P], output: P) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let final_values = collect_merged_keyed_values::<T, P>(spec, base, deltas)?;
    write_keyed_values_atomically(
        spec,
        output.as_ref(),
        final_values.into_iter().map(|(_, value)| value),
    )
}

pub fn compact_keyed_files<T, P>(spec: FormatSpec, base: P, deltas: &[P], output: P) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let final_values = collect_merged_keyed_values::<T, P>(spec, base, deltas)?;
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
) -> Result<Vec<(MergeOrder, T)>>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    crate::collections::ensure_registered_block::<T>(spec)?;
    let mut state: HashMap<T::Key, (MergeOrder, Option<T>)> = HashMap::new();
    apply_merge_file::<T, _>(spec, base, MergeShard { ordinal: 0 }, &mut state)?;
    for (index, delta) in deltas.iter().enumerate() {
        apply_merge_file::<T, _>(spec, delta, MergeShard { ordinal: index + 1 }, &mut state)?;
    }

    let mut final_values: Vec<(MergeOrder, T)> = state
        .into_values()
        .filter_map(|(order, value)| value.map(|value| (order, value)))
        .collect();
    final_values.sort_by_key(|(order, _)| *order);
    Ok(final_values)
}

fn write_keyed_values_atomically<T, I>(spec: FormatSpec, output: &Path, values: I) -> Result<()>
where
    T: VarveBlock,
    I: IntoIterator<Item = T>,
{
    let _lock = WriterLock::acquire(output)?;
    let (temp_path, temp_file) = create_rewrite_temp_file(output)?;
    if let Ok(metadata) = std::fs::metadata(output) {
        temp_file.set_permissions(metadata.permissions())?;
    }
    drop(temp_file);

    let result = (|| {
        let mut out = VarveFile::create(spec, &temp_path)?;
        for value in values {
            out.push(&value)?;
        }
        out.flush()?;
        out.sync()
    })();
    if let Err(error) = result {
        let _ = remove_file(&temp_path);
        return Err(error);
    }

    if let Err(error) = replace_path_atomically(&temp_path, output) {
        let _ = remove_file(&temp_path);
        return Err(error);
    }
    Ok(())
}

fn apply_merge_file<T, P>(
    spec: FormatSpec,
    path: P,
    shard: MergeShard,
    state: &mut HashMap<T::Key, (MergeOrder, Option<T>)>,
) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let file = VarveFile::open_readonly(spec, path)?;
    apply_merge_entries::<T>(spec, &file.path, &file.index, shard, state)
}

fn apply_merge_entries<T>(
    spec: FormatSpec,
    path: &Path,
    entries: &[RecordIndexEntry],
    shard: MergeShard,
    state: &mut HashMap<T::Key, (MergeOrder, Option<T>)>,
) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
{
    for (record_ordinal, entry) in entries.iter().enumerate() {
        let order = MergeOrder {
            shard_ordinal: shard.ordinal,
            sequence: entry.sequence,
            record_ordinal,
        };
        match entry.block_id {
            id if id == T::ID => {
                if entry.block_version != T::VERSION {
                    return Err(Error::BlockVersionMismatch {
                        block_id: T::ID,
                        expected: T::VERSION,
                        actual: entry.block_version,
                    });
                }
                let payload = entry.read_logical_payload(spec, path)?;
                let block: T = decode_from_slice(&payload, T::ENDIAN.unwrap_or(spec.endian))?;
                let key = block.key();
                if should_apply(state.get(&key), order) {
                    state.insert(key, (order, Some(block)));
                }
            }
            TOMBSTONE_BLOCK_ID => {
                let payload = entry.read_payload(path)?;
                let Some(key) = decode_internal_key_payload::<T>(spec.endian, &payload)? else {
                    continue;
                };
                if should_apply(state.get(&key), order) {
                    state.insert(key, (order, None));
                }
            }
            OP_BLOCK_ID => {
                let payload = entry.read_payload(path)?;
                let Some((key, op)) = decode_internal_op_payload::<T>(spec.endian, &payload)?
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

fn should_apply<T>(current: Option<&(MergeOrder, Option<T>)>, order: MergeOrder) -> bool {
    current.is_none_or(|(old_order, _)| order >= *old_order)
}

fn encode_internal_key_payload<T>(endian: Endian, key: &T::Key) -> Result<Vec<u8>>
where
    T: VarveKeyedBlock,
{
    let key_payload = encode_to_vec(key, endian)?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&T::ID.to_le_bytes());
    payload.extend_from_slice(&(key_payload.len() as u64).to_le_bytes());
    payload.extend_from_slice(&key_payload);
    Ok(payload)
}

fn decode_internal_key_payload<T>(endian: Endian, payload: &[u8]) -> Result<Option<T::Key>>
where
    T: VarveKeyedBlock,
{
    if payload.len() < 12 {
        return Err(Error::UnexpectedEof);
    }
    let mut target = [0; 4];
    target.copy_from_slice(&payload[..4]);
    if u32::from_le_bytes(target) != T::ID {
        return Ok(None);
    }
    let mut len = [0; 8];
    len.copy_from_slice(&payload[4..12]);
    let key_len = usize::try_from(u64::from_le_bytes(len)).map_err(|_| Error::LengthOverflow {
        value: u64::from_le_bytes(len),
    })?;
    let key_start: usize = 12;
    let key_end = key_start.checked_add(key_len).ok_or(Error::UnexpectedEof)?;
    if key_end > payload.len() {
        return Err(Error::UnexpectedEof);
    }
    Ok(Some(decode_from_slice(
        &payload[key_start..key_end],
        endian,
    )?))
}

fn encode_internal_op_payload<T>(endian: Endian, key: &T::Key, op: &T::Op) -> Result<Vec<u8>>
where
    T: VarveMerge,
{
    let key_payload = encode_to_vec(key, endian)?;
    let op_payload = encode_to_vec(op, endian)?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&T::ID.to_le_bytes());
    payload.extend_from_slice(&(key_payload.len() as u64).to_le_bytes());
    payload.extend_from_slice(&key_payload);
    payload.extend_from_slice(&(op_payload.len() as u64).to_le_bytes());
    payload.extend_from_slice(&op_payload);
    Ok(payload)
}

fn decode_internal_op_payload<T>(endian: Endian, payload: &[u8]) -> Result<Option<(T::Key, T::Op)>>
where
    T: VarveMerge,
{
    if payload.len() < 12 {
        return Err(Error::UnexpectedEof);
    }
    let mut target = [0; 4];
    target.copy_from_slice(&payload[..4]);
    if u32::from_le_bytes(target) != T::ID {
        return Ok(None);
    }
    let mut key_len = [0; 8];
    key_len.copy_from_slice(&payload[4..12]);
    let key_len_value = u64::from_le_bytes(key_len);
    let key_len = usize::try_from(key_len_value).map_err(|_| Error::LengthOverflow {
        value: key_len_value,
    })?;
    let key_start: usize = 12;
    let key_end = key_start.checked_add(key_len).ok_or(Error::UnexpectedEof)?;
    let op_len_end = key_end.checked_add(8).ok_or(Error::UnexpectedEof)?;
    if op_len_end > payload.len() {
        return Err(Error::UnexpectedEof);
    }
    let key = decode_from_slice(&payload[key_start..key_end], endian)?;
    let mut op_len = [0; 8];
    op_len.copy_from_slice(&payload[key_end..op_len_end]);
    let op_len_value = u64::from_le_bytes(op_len);
    let op_len = usize::try_from(op_len_value).map_err(|_| Error::LengthOverflow {
        value: op_len_value,
    })?;
    let op_start = op_len_end;
    let op_end = op_start.checked_add(op_len).ok_or(Error::UnexpectedEof)?;
    if op_end > payload.len() {
        return Err(Error::UnexpectedEof);
    }
    let op = decode_from_slice(&payload[op_start..op_end], endian)?;
    Ok(Some((key, op)))
}

fn write_file_header(spec: FormatSpec, file: &mut File) -> Result<()> {
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
) -> Result<Option<crate::matrix::MatrixLayout>> {
    if spec.has_matrix_blocks() {
        Ok(Some(crate::matrix::read_layout(spec, file, header_len)?))
    } else {
        Ok(None)
    }
}

fn append_log_start(header_len: u64, matrix: Option<&crate::matrix::MatrixLayout>) -> u64 {
    matrix.map_or(header_len, crate::matrix::MatrixLayout::append_log_start)
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

fn handle_structural_tail(
    file: &mut File,
    offset: u64,
    allow_tail: bool,
    recovery_policy: RecoveryPolicy,
) -> Result<Option<RecordIndexEntry>> {
    if allow_tail || recovery_policy == RecoveryPolicy::TruncateTail {
        if recovery_policy == RecoveryPolicy::TruncateTail {
            file.set_len(offset)?;
        }
        return Ok(None);
    }
    Err(Error::CorruptTail { offset })
}

fn read_record_entry_at(
    spec: FormatSpec,
    file: &mut File,
    file_len: u64,
    offset: u64,
    allow_tail: bool,
    recovery_policy: RecoveryPolicy,
) -> Result<Option<RecordIndexEntry>> {
    if file_len - offset < RECORD_HEADER_LEN {
        return handle_structural_tail(file, offset, allow_tail, recovery_policy);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut entry = read_record_header(file, spec, offset)?;
    validate_record_entry(spec, &entry)?;
    let payload_end = entry
        .payload_offset
        .checked_add(entry.payload_len)
        .ok_or(Error::CorruptTail { offset })?;
    let record_end = payload_end
        .checked_add(record_footer_len(spec))
        .ok_or(Error::CorruptTail { offset })?;
    if record_end > file_len {
        return handle_structural_tail(file, offset, allow_tail, recovery_policy);
    }

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
        let payload = entry.read_payload_file(file)?;
        let header = RecordHeaderFields {
            block_id: entry.block_id,
            block_version: entry.block_version,
            flags: entry.flags,
            sequence: entry.sequence,
            payload_len: entry.payload_len,
            checksum: 0,
            uncompressed_len_hint: entry.uncompressed_len_hint,
        };
        let expected = checksum_record_fields(
            spec,
            entry.record_offset,
            header,
            &payload,
            footer.as_deref().unwrap_or(&[]),
        )?;
        if expected != entry.checksum {
            if spec.commit_policy.is_transaction_marker() && allow_tail {
                if recovery_policy == RecoveryPolicy::TruncateTail {
                    file.set_len(offset)?;
                }
                return Ok(None);
            }
            return Err(Error::ChecksumMismatch { offset });
        }
    }

    if entry.block_id == COMMIT_BLOCK_ID {
        let payload = entry.read_payload_file(file)?;
        if payload != COMMIT_PAYLOAD_MAGIC {
            return Err(Error::InvalidCommitMarker { offset });
        }
    }

    Ok(Some(entry))
}

fn load_index(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    recovery_policy: RecoveryPolicy,
) -> Result<Vec<RecordIndexEntry>> {
    if spec.commit_policy.is_transaction_marker() {
        return scan_records_from(spec, file, header_len, recovery_policy);
    }
    if spec.index_policy.checkpoint_on_flush
        && let Some(checkpoint) =
            find_latest_valid_checkpoint(spec, file, header_len, recovery_policy)?
    {
        let mut entries = checkpoint.entries;
        let mut tail = scan_records_from(spec, file, checkpoint.covered_offset, recovery_policy)?;
        entries.append(&mut tail);
        return Ok(entries);
    }
    scan_records_from(spec, file, header_len, recovery_policy)
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
    let committed_end = entries
        .iter()
        .rev()
        .find(|entry| entry.block_id == COMMIT_BLOCK_ID)
        .map_or(header_len, RecordIndexEntry::physical_end);
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

fn find_latest_valid_checkpoint(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    recovery_policy: RecoveryPolicy,
) -> Result<Option<IndexCheckpoint>> {
    let file_len = file.metadata()?.len();
    let mut offset = header_len;
    let mut latest = None;
    let mut prefix_entries = Vec::new();
    while offset < file_len {
        let allow_tail = spec.commit_policy == CommitPolicy::RecordFooter;
        let Some(entry) =
            read_record_entry_at(spec, file, file_len, offset, allow_tail, recovery_policy)?
        else {
            break;
        };
        if entry.block_id == INDEX_BLOCK_ID
            && (1..=INDEX_CHECKPOINT_VERSION).contains(&entry.block_version)
        {
            let payload = entry.read_payload_file(file)?;
            if let Ok(checkpoint) = decode_index_checkpoint(&payload)
                && validate_index_checkpoint(
                    spec,
                    file,
                    header_len,
                    file_len,
                    &entry,
                    &checkpoint,
                    &prefix_entries,
                )
                .is_ok()
            {
                latest = Some(checkpoint);
            }
        }
        offset = entry.physical_end();
        prefix_entries.push(entry);
    }
    Ok(latest)
}

fn decode_index_checkpoint(payload: &[u8]) -> Result<IndexCheckpoint> {
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
    let count_usize = usize::try_from(count).map_err(|_| Error::LengthOverflow { value: count })?;
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

    let mut entries = Vec::with_capacity(count_usize);
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
    spec: FormatSpec,
    file: &mut File,
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
        if entry.record_offset < header_len
            || entry.record_offset < previous_offset
            || entry.payload_offset != entry.record_offset + RECORD_HEADER_LEN
            || entry.record_offset == checkpoint_record.record_offset
        {
            return Err(Error::InvalidIndexCheckpoint);
        }
        let physical_end = entry.physical_end();
        if physical_end > checkpoint.covered_offset || physical_end > file_len {
            return Err(Error::InvalidIndexCheckpoint);
        }
        file.seek(SeekFrom::Start(entry.record_offset))?;
        let actual = read_record_entry_at(
            spec,
            file,
            file_len,
            entry.record_offset,
            false,
            RecoveryPolicy::Strict,
        )?;
        let Some(actual) = actual else {
            return Err(Error::InvalidIndexCheckpoint);
        };
        if !record_headers_match(entry, &actual) {
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
    recovery_policy: RecoveryPolicy,
) -> Result<Vec<RecordIndexEntry>> {
    let file_len = file.metadata()?.len();
    let mut offset = header_len;
    let mut entries = Vec::new();
    let mut latest_commit_position = None;
    while offset < file_len {
        let allow_tail = spec.commit_policy == CommitPolicy::RecordFooter
            || (spec.commit_policy.is_transaction_marker() && latest_commit_position.is_some());
        let entry =
            match read_record_entry_at(spec, file, file_len, offset, allow_tail, recovery_policy) {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_error)
                    if spec.commit_policy.is_transaction_marker()
                        && latest_commit_position.is_some() =>
                {
                    if recovery_policy == RecoveryPolicy::TruncateTail {
                        file.set_len(offset)?;
                    }
                    break;
                }
                Err(error) => return Err(error),
            };
        offset = entry.physical_end();
        if entry.block_id == COMMIT_BLOCK_ID {
            latest_commit_position = Some(entries.len());
        }
        entries.push(entry);
    }
    if spec.commit_policy.is_transaction_marker() {
        let Some(position) = latest_commit_position else {
            if recovery_policy == RecoveryPolicy::TruncateTail {
                file.set_len(header_len)?;
            }
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
    fn read_payload_file(&self, file: &mut File) -> Result<Vec<u8>> {
        file.seek(SeekFrom::Start(self.payload_offset))?;
        let payload_len = usize::try_from(self.payload_len).map_err(|_| Error::LengthOverflow {
            value: self.payload_len,
        })?;
        let mut payload = vec![0; payload_len];
        file.read_exact(&mut payload)?;
        Ok(payload)
    }
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
        payload_offset: offset + decoded.lead_in_len,
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

fn read_record_footer_bytes(file: &mut File, offset: u64) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut footer = vec![0; RECORD_FOOTER_LEN as usize];
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
) -> Result<MatrixSidecarManifest> {
    let manifest = matrix_sidecar_manifest_for(spec, category, generation, payload)?;
    let magic_len =
        u16::try_from(manifest.format_magic.len()).map_err(|_| Error::InvalidMatrixSidecar)?;
    let category_len =
        u16::try_from(manifest.category.len()).map_err(|_| Error::InvalidMatrixSidecar)?;
    let mut bytes =
        Vec::with_capacity(MATRIX_SIDECAR_FIXED_LEN + magic_len as usize + category_len as usize);
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
    debug_assert_eq!(bytes.len(), MATRIX_SIDECAR_FIXED_LEN);
    bytes.extend_from_slice(&manifest.format_magic);
    bytes.extend_from_slice(manifest.category.as_bytes());
    bytes.extend_from_slice(payload);

    let mut file = File::create(sidecar_path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(manifest)
}

fn read_matrix_sidecar_file<P: AsRef<Path>>(
    spec: FormatSpec,
    category: &str,
    sidecar_path: P,
    expected_generation: Option<u64>,
) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
    let bytes = std::fs::read(sidecar_path)?;
    if bytes.len() < MATRIX_SIDECAR_FIXED_LEN || &bytes[0..4] != MATRIX_SIDECAR_MAGIC {
        return Err(Error::InvalidMatrixSidecar);
    }
    let version = u16::from_le_bytes(bytes[4..6].try_into().expect("slice"));
    if version != MATRIX_SIDECAR_VERSION {
        return Err(Error::InvalidMatrixSidecar);
    }
    let format_version = u16::from_le_bytes(bytes[8..10].try_into().expect("slice"));
    let magic_len = u16::from_le_bytes(bytes[10..12].try_into().expect("slice")) as usize;
    let category_len = u16::from_le_bytes(bytes[12..14].try_into().expect("slice")) as usize;
    let schema_hash = u64::from_le_bytes(bytes[16..24].try_into().expect("slice"));
    let generation = u64::from_le_bytes(bytes[24..32].try_into().expect("slice"));
    let payload_len = u64::from_le_bytes(bytes[32..40].try_into().expect("slice"));
    let payload_crc32 = u32::from_le_bytes(bytes[40..44].try_into().expect("slice"));
    let magic_start = MATRIX_SIDECAR_FIXED_LEN;
    let category_start = magic_start
        .checked_add(magic_len)
        .ok_or(Error::InvalidMatrixSidecar)?;
    let payload_start = category_start
        .checked_add(category_len)
        .ok_or(Error::InvalidMatrixSidecar)?;
    let payload_len_usize =
        usize::try_from(payload_len).map_err(|_| Error::InvalidMatrixSidecar)?;
    let payload_end = payload_start
        .checked_add(payload_len_usize)
        .ok_or(Error::InvalidMatrixSidecar)?;
    if payload_end != bytes.len() {
        return Err(Error::InvalidMatrixSidecar);
    }
    let format_magic = bytes
        .get(magic_start..category_start)
        .ok_or(Error::InvalidMatrixSidecar)?
        .to_vec();
    let category_bytes = bytes
        .get(category_start..payload_start)
        .ok_or(Error::InvalidMatrixSidecar)?;
    let category_text =
        String::from_utf8(category_bytes.to_vec()).map_err(|_| Error::InvalidMatrixSidecar)?;
    let payload = bytes
        .get(payload_start..payload_end)
        .ok_or(Error::InvalidMatrixSidecar)?
        .to_vec();
    let actual = crc32_bytes(&payload)?;
    if actual != payload_crc32 {
        return Err(Error::MatrixSidecarChecksumMismatch {
            expected: payload_crc32,
            actual,
        });
    }
    let manifest = MatrixSidecarManifest {
        format_magic,
        format_version,
        schema_hash,
        category: category_text,
        generation,
        payload_len,
        payload_crc32,
    };
    validate_matrix_sidecar_manifest(spec, category, expected_generation, &manifest)?;
    Ok((manifest, payload))
}

fn matrix_sidecar_manifest_for(
    spec: FormatSpec,
    category: &str,
    generation: u64,
    payload: &[u8],
) -> Result<MatrixSidecarManifest> {
    if category.is_empty() {
        return Err(Error::InvalidMatrixSidecar);
    }
    Ok(MatrixSidecarManifest {
        format_magic: spec.magic.to_vec(),
        format_version: spec.version,
        schema_hash: spec.computed_schema_hash(),
        category: category.to_string(),
        generation,
        payload_len: payload
            .len()
            .try_into()
            .map_err(|_| Error::InvalidMatrixSidecar)?,
        payload_crc32: crc32_bytes(payload)?,
    })
}

fn validate_matrix_sidecar_manifest(
    spec: FormatSpec,
    category: &str,
    expected_generation: Option<u64>,
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
    if let Some(expected_generation) = expected_generation
        && manifest.generation != expected_generation
    {
        return Err(Error::MatrixSidecarMismatch("generation"));
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

#[cfg(not(windows))]
pub(crate) fn replace_path_atomically(replacement: &Path, target: &Path) -> Result<()> {
    std::fs::rename(replacement, target)?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn replace_path_atomically(replacement: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    if !target.exists() {
        std::fs::rename(replacement, target)?;
        return Ok(());
    }

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
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
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

#[derive(Debug)]
pub(crate) struct WriterLock {
    path: PathBuf,
    _file: File,
}

impl WriterLock {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        Self::acquire_with_policy(path, WriterLockBreakPolicy::Refuse)
    }

    fn acquire_with_policy(target_path: &Path, policy: WriterLockBreakPolicy) -> Result<Self> {
        let path = lock_path(target_path);
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let info = WriterLockInfo {
                        path: path.clone(),
                        target_path: absolute_target_path(target_path)?,
                        process_id: std::process::id(),
                        created_unix_ms: unix_time_ms(),
                    };
                    if let Err(error) = write_writer_lock_info(&mut file, &info) {
                        let _ = remove_file(&path);
                        return Err(error);
                    }
                    return Ok(Self { path, _file: file });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let Some(info) = read_writer_lock_info(target_path)? else {
                        continue;
                    };
                    if should_break_writer_lock(&info, policy) {
                        remove_file(&info.path)?;
                        continue;
                    }
                    return match policy {
                        WriterLockBreakPolicy::Refuse => {
                            Err(Error::WriterLockHeld(path.display().to_string()))
                        }
                        _ => Err(Error::WriterLockBreakRefused(path.display().to_string())),
                    };
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(Error::WriterLockHeld(path.display().to_string()))
    }
}

fn write_writer_lock_info(file: &mut File, info: &WriterLockInfo) -> Result<()> {
    let target = info.target_path.to_string_lossy();
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

fn read_writer_lock_info(target_path: &Path) -> Result<Option<WriterLockInfo>> {
    let path = lock_path(target_path);
    if !path.exists() {
        return Ok(None);
    }
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Error::WriterLockMalformed(path.display().to_string())),
    };
    parse_writer_lock_info(&path, &contents).map(Some)
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
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    if process_id == std::process::id() {
        return false;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if handle.is_null() {
        return std::io::Error::last_os_error().raw_os_error()
            == Some(ERROR_INVALID_PARAMETER as i32);
    }
    unsafe {
        CloseHandle(handle);
    }
    false
}

#[cfg(not(windows))]
fn process_is_absent(process_id: u32) -> bool {
    let _ = process_id;
    false
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = remove_file(&self.path);
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
