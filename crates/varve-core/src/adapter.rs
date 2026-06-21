use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Endian, Error, LayoutScanReport, LayoutSegmentInfo, LayoutTailInfo, Result};

#[derive(Clone, Debug)]
pub struct BinaryCursor<'a> {
    bytes: &'a [u8],
    position: usize,
    endian: Endian,
}

impl<'a> BinaryCursor<'a> {
    pub const fn new(bytes: &'a [u8], endian: Endian) -> Self {
        Self {
            bytes,
            position: 0,
            endian,
        }
    }

    pub const fn endian(&self) -> Endian {
        self.endian
    }

    pub const fn position(&self) -> usize {
        self.position
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(len)
            .ok_or(Error::LengthOverflow { value: u64::MAX })?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(Error::UnexpectedEof)?;
        self.position = end;
        Ok(bytes)
    }

    pub fn finish(&self) -> Result<()> {
        let remaining = self.remaining();
        if remaining == 0 {
            Ok(())
        } else {
            Err(Error::TrailingBytes { remaining })
        }
    }

    pub fn u8(&mut self) -> Result<u8> {
        self.bytes(1)?.first().copied().ok_or(Error::UnexpectedEof)
    }

    pub fn i8(&mut self) -> Result<i8> {
        Ok(i8::from_ne_bytes([self.u8()?]))
    }

    pub fn u16(&mut self) -> Result<u16> {
        let mut value = [0; 2];
        value.copy_from_slice(self.bytes(2)?);
        Ok(match self.endian {
            Endian::Little => u16::from_le_bytes(value),
            Endian::Big => u16::from_be_bytes(value),
        })
    }

    pub fn i16(&mut self) -> Result<i16> {
        let mut value = [0; 2];
        value.copy_from_slice(self.bytes(2)?);
        Ok(match self.endian {
            Endian::Little => i16::from_le_bytes(value),
            Endian::Big => i16::from_be_bytes(value),
        })
    }

    pub fn u32(&mut self) -> Result<u32> {
        let mut value = [0; 4];
        value.copy_from_slice(self.bytes(4)?);
        Ok(match self.endian {
            Endian::Little => u32::from_le_bytes(value),
            Endian::Big => u32::from_be_bytes(value),
        })
    }

    pub fn i32(&mut self) -> Result<i32> {
        let mut value = [0; 4];
        value.copy_from_slice(self.bytes(4)?);
        Ok(match self.endian {
            Endian::Little => i32::from_le_bytes(value),
            Endian::Big => i32::from_be_bytes(value),
        })
    }

    pub fn u64(&mut self) -> Result<u64> {
        let mut value = [0; 8];
        value.copy_from_slice(self.bytes(8)?);
        Ok(match self.endian {
            Endian::Little => u64::from_le_bytes(value),
            Endian::Big => u64::from_be_bytes(value),
        })
    }

    pub fn i64(&mut self) -> Result<i64> {
        let mut value = [0; 8];
        value.copy_from_slice(self.bytes(8)?);
        Ok(match self.endian {
            Endian::Little => i64::from_le_bytes(value),
            Endian::Big => i64::from_be_bytes(value),
        })
    }

    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }

    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.u64()?))
    }

    pub fn array_f64(&mut self, count: usize) -> Result<Vec<f64>> {
        (0..count).map(|_| self.f64()).collect()
    }

    pub fn len_prefixed_bytes<P: LengthPrefix>(&mut self) -> Result<&'a [u8]> {
        let len = P::read(self)?;
        self.bytes(len)
    }

    pub fn len_prefixed_string<P: LengthPrefix>(&mut self) -> Result<String> {
        let bytes = self.len_prefixed_bytes::<P>()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| Error::InvalidUtf8)
    }
}

#[derive(Clone, Debug)]
pub struct BinaryWriter {
    bytes: Vec<u8>,
    endian: Endian,
}

impl BinaryWriter {
    pub const fn new(endian: Endian) -> Self {
        Self {
            bytes: Vec::new(),
            endian,
        }
    }

    pub fn with_capacity(endian: Endian, capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
            endian,
        }
    }

    pub const fn endian(&self) -> Endian {
        self.endian
    }

    pub fn position(&self) -> usize {
        self.bytes.len()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.bytes
    }

    pub fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub fn u8(&mut self, value: u8) -> Result<()> {
        self.bytes.push(value);
        Ok(())
    }

    pub fn i8(&mut self, value: i8) -> Result<()> {
        self.bytes.extend_from_slice(&value.to_ne_bytes());
        Ok(())
    }

    pub fn u16(&mut self, value: u16) -> Result<()> {
        self.bytes.extend_from_slice(&match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
        Ok(())
    }

    pub fn i16(&mut self, value: i16) -> Result<()> {
        self.bytes.extend_from_slice(&match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
        Ok(())
    }

    pub fn u32(&mut self, value: u32) -> Result<()> {
        self.bytes.extend_from_slice(&match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
        Ok(())
    }

    pub fn i32(&mut self, value: i32) -> Result<()> {
        self.bytes.extend_from_slice(&match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
        Ok(())
    }

    pub fn u64(&mut self, value: u64) -> Result<()> {
        self.bytes.extend_from_slice(&match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
        Ok(())
    }

    pub fn i64(&mut self, value: i64) -> Result<()> {
        self.bytes.extend_from_slice(&match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
        Ok(())
    }

    pub fn f32(&mut self, value: f32) -> Result<()> {
        self.u32(value.to_bits())
    }

    pub fn f64(&mut self, value: f64) -> Result<()> {
        self.u64(value.to_bits())
    }

    pub fn array_f64(&mut self, values: &[f64]) -> Result<()> {
        for value in values {
            self.f64(*value)?;
        }
        Ok(())
    }

    pub fn len_prefixed_bytes<P: LengthPrefix>(&mut self, bytes: &[u8]) -> Result<()> {
        P::write(self, bytes.len())?;
        self.bytes(bytes)
    }

    pub fn len_prefixed_string<P: LengthPrefix>(&mut self, value: &str) -> Result<()> {
        self.len_prefixed_bytes::<P>(value.as_bytes())
    }
}

pub trait LengthPrefix: Sized {
    fn read(cursor: &mut BinaryCursor<'_>) -> Result<usize>;
    fn write(writer: &mut BinaryWriter, len: usize) -> Result<()>;
}

macro_rules! impl_length_prefix {
    ($ty:ty, $read:ident, $write:ident) => {
        impl LengthPrefix for $ty {
            fn read(cursor: &mut BinaryCursor<'_>) -> Result<usize> {
                usize::try_from(cursor.$read()?)
                    .map_err(|_| Error::LengthOverflow { value: u64::MAX })
            }

            fn write(writer: &mut BinaryWriter, len: usize) -> Result<()> {
                let value = <$ty>::try_from(len)
                    .map_err(|_| Error::LengthOverflow { value: len as u64 })?;
                writer.$write(value)
            }
        }
    };
}

impl_length_prefix!(u8, u8, u8);
impl_length_prefix!(u16, u16, u16);
impl_length_prefix!(u32, u32, u32);
impl_length_prefix!(u64, u64, u64);

pub trait TaggedValueCodec {
    type Value;
    type TypeId;

    fn decode(type_id: Self::TypeId, cursor: &mut BinaryCursor<'_>) -> Result<Self::Value>;
    fn encode(value: &Self::Value, writer: &mut BinaryWriter) -> Result<Self::TypeId>;
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaggedValue {
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChunkLayout {
    Contiguous,
    Interleaved { stride: u64 },
    Strided { byte_stride: u64 },
    User(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkEntry<K> {
    pub key: K,
    pub segment_index: usize,
    pub byte_offset: u64,
    pub byte_len: u64,
    pub value_count: u64,
    pub layout: ChunkLayout,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkIndexEntry<K> {
    pub key: K,
    pub segment_index: usize,
    pub raw_offset: u64,
    pub byte_offset: u64,
    pub byte_len: u64,
    pub value_count: u64,
    pub layout: ChunkLayout,
}

impl<K> ChunkIndexEntry<K> {
    pub fn absolute_offset(&self) -> Result<u64> {
        self.raw_offset
            .checked_add(self.byte_offset)
            .ok_or(Error::AdapterBounds {
                offset: self.raw_offset,
                len: self.byte_offset,
                available: u64::MAX,
            })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChunkIndex<K> {
    entries: Vec<ChunkIndexEntry<K>>,
}

impl<K> ChunkIndex<K> {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[ChunkIndexEntry<K>] {
        &self.entries
    }
}

impl<K: PartialEq> ChunkIndex<K> {
    pub fn entries_for<'a>(&'a self, key: &'a K) -> impl Iterator<Item = &'a ChunkIndexEntry<K>> {
        self.entries.iter().filter(move |entry| &entry.key == key)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ChunkIndexBuilder<K> {
    entries: Vec<ChunkIndexEntry<K>>,
}

impl<K> ChunkIndexBuilder<K> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn push(&mut self, entry: ChunkEntry<K>, segment: &LayoutSegmentInfo) -> Result<&mut Self> {
        let end = entry
            .byte_offset
            .checked_add(entry.byte_len)
            .ok_or(Error::AdapterBounds {
                offset: entry.byte_offset,
                len: entry.byte_len,
                available: segment.raw_len,
            })?;
        if end > segment.raw_len {
            return Err(Error::AdapterBounds {
                offset: entry.byte_offset,
                len: entry.byte_len,
                available: segment.raw_len,
            });
        }
        self.entries.push(ChunkIndexEntry {
            key: entry.key,
            segment_index: entry.segment_index,
            raw_offset: segment.raw_offset,
            byte_offset: entry.byte_offset,
            byte_len: entry.byte_len,
            value_count: entry.value_count,
            layout: entry.layout,
        });
        Ok(self)
    }

    pub fn finish(self) -> Result<ChunkIndex<K>> {
        Ok(ChunkIndex {
            entries: self.entries,
        })
    }
}

pub trait SegmentReducer {
    type Metadata;
    type State;

    fn initial() -> Self::State;
    fn apply_segment(
        state: &mut Self::State,
        segment: &LayoutSegmentInfo,
        metadata: Self::Metadata,
    ) -> Result<()>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentReductionReport<S> {
    pub state: S,
    pub segments_applied: usize,
}

pub fn reduce_segments<R, I>(segments: I) -> Result<SegmentReductionReport<R::State>>
where
    R: SegmentReducer,
    I: IntoIterator<Item = (LayoutSegmentInfo, R::Metadata)>,
{
    let mut state = R::initial();
    let mut segments_applied = 0;
    for (segment, metadata) in segments {
        R::apply_segment(&mut state, &segment, metadata)?;
        segments_applied += 1;
    }
    Ok(SegmentReductionReport {
        state,
        segments_applied,
    })
}

pub fn reduce_segments_by_ref<'a, R, I>(segments: I) -> Result<SegmentReductionReport<R::State>>
where
    R: SegmentReducer,
    I: IntoIterator<Item = (&'a LayoutSegmentInfo, R::Metadata)>,
{
    let mut state = R::initial();
    let mut segments_applied = 0;
    for (segment, metadata) in segments {
        R::apply_segment(&mut state, segment, metadata)?;
        segments_applied += 1;
    }
    Ok(SegmentReductionReport {
        state,
        segments_applied,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarMode {
    Required,
    Optional,
    GeneratedOnFlush,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidecarPolicy {
    pub extension: &'static str,
    pub mode: SidecarMode,
    pub verify_main_len: bool,
    pub verify_main_fingerprint: bool,
}

impl SidecarPolicy {
    pub fn sidecar_path<P: AsRef<Path>>(&self, main: P) -> PathBuf {
        let main = main.as_ref();
        let mut path = main.to_path_buf();
        path.set_extension(self.extension);
        path
    }

    pub fn check_presence<P: AsRef<Path>>(&self, main: P) -> Result<AdapterCheckStatus> {
        let path = self.sidecar_path(main);
        let exists = path.exists();
        Ok(match (self.mode, exists) {
            (SidecarMode::Required, false) => AdapterCheckStatus::Failed,
            (_, true) => AdapterCheckStatus::Passed,
            _ => AdapterCheckStatus::Warning,
        })
    }

    pub fn inspect<P: AsRef<Path>>(
        &self,
        main: P,
        expected: Option<&SidecarIdentity>,
    ) -> Result<SidecarReport> {
        let main = main.as_ref();
        let path = self.sidecar_path(main);
        let present = path.exists();
        let actual = if present {
            Some(SidecarIdentity::from_main_file(main)?)
        } else {
            None
        };
        let status = if !present && self.mode == SidecarMode::Required {
            AdapterCheckStatus::Failed
        } else if let (Some(expected), Some(actual)) = (expected, actual.as_ref()) {
            let len_mismatch = self.verify_main_len && expected.main_len != actual.main_len;
            let fingerprint_mismatch = self.verify_main_fingerprint
                && expected.main_fingerprint != actual.main_fingerprint;
            if len_mismatch || fingerprint_mismatch {
                AdapterCheckStatus::Failed
            } else {
                AdapterCheckStatus::Passed
            }
        } else if present {
            AdapterCheckStatus::Passed
        } else {
            AdapterCheckStatus::Warning
        };
        Ok(SidecarReport {
            path,
            present,
            status,
            actual,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidecarIdentity {
    pub main_len: u64,
    pub main_fingerprint: u64,
}

impl SidecarIdentity {
    pub fn from_main_file<P: AsRef<Path>>(main: P) -> Result<Self> {
        let main = main.as_ref();
        let metadata = fs::metadata(main)?;
        Ok(Self {
            main_len: metadata.len(),
            main_fingerprint: fingerprint_file(main)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarReport {
    pub path: PathBuf,
    pub present: bool,
    pub status: AdapterCheckStatus,
    pub actual: Option<SidecarIdentity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterCheckStatus {
    Passed,
    Warning,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterDiagnostic {
    pub status: AdapterCheckStatus,
    pub domain: AdapterDiagnosticDomain,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterDiagnosticDomain {
    Declaration,
    PhysicalLayout,
    Cursor,
    ChunkIndex,
    Reducer,
    Sidecar,
    Domain,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdapterCheckReport {
    pub diagnostics: Vec<AdapterDiagnostic>,
    pub physical_tail: Option<LayoutTailInfo>,
}

impl AdapterCheckReport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_layout_report(report: &LayoutScanReport) -> Self {
        let mut check = Self::new();
        check.physical_tail = report.tail.clone();
        if let Some(tail) = &report.tail {
            check.push(
                AdapterCheckStatus::Warning,
                AdapterDiagnosticDomain::PhysicalLayout,
                format!(
                    "physical scan ended with tail {:?} at {}",
                    tail.kind, tail.offset
                ),
            );
        }
        check
    }

    pub fn push(
        &mut self,
        status: AdapterCheckStatus,
        domain: AdapterDiagnosticDomain,
        message: impl Into<String>,
    ) {
        self.diagnostics.push(AdapterDiagnostic {
            status,
            domain,
            message: message.into(),
        });
    }

    pub fn status(&self) -> AdapterCheckStatus {
        if self
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.status == AdapterCheckStatus::Failed)
        {
            AdapterCheckStatus::Failed
        } else if self.physical_tail.is_some()
            || self
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.status == AdapterCheckStatus::Warning)
        {
            AdapterCheckStatus::Warning
        } else {
            AdapterCheckStatus::Passed
        }
    }

    pub fn passed(&self) -> bool {
        self.status() == AdapterCheckStatus::Passed
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterTailStatus {
    pub tail: LayoutTailInfo,
    pub expected_end: Option<u64>,
    pub available_len: u64,
    pub evidence: Option<String>,
}

impl AdapterTailStatus {
    pub fn new(tail: LayoutTailInfo, expected_end: Option<u64>, evidence: Option<String>) -> Self {
        let available_len = tail.file_len.saturating_sub(tail.offset);
        Self {
            tail,
            expected_end,
            available_len,
            evidence,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AdapterInputFile {
    path: PathBuf,
    temporary: bool,
}

impl AdapterInputFile {
    pub fn from_path<P: AsRef<Path>>(path: P) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            temporary: false,
        }
    }

    pub fn from_bytes(extension: &str, bytes: &[u8]) -> Result<Self> {
        let mut path = std::env::temp_dir();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let extension = extension.trim_start_matches('.');
        path.push(format!(
            "varve-adapter-input-{}-{timestamp}.{extension}",
            std::process::id()
        ));
        fs::write(&path, bytes)?;
        Ok(Self {
            path,
            temporary: true,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_temporary(&self) -> bool {
        self.temporary
    }
}

impl Drop for AdapterInputFile {
    fn drop(&mut self) {
        if self.temporary {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn fingerprint_file(path: &Path) -> Result<u64> {
    let mut file = fs::File::open(path)?;
    let mut hasher = StableHasher::default();
    let mut buffer = [0; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        buffer[..read].hash(&mut hasher);
    }
    Ok(hasher.finish())
}

struct StableHasher(u64);

impl Default for StableHasher {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for StableHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }
}
