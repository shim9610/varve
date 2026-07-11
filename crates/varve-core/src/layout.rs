use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};

use crate::{
    Endian, Error, FileHeaderDescriptor, FormatSpec, LayoutAnchor, LayoutFieldDescriptor,
    LayoutFieldSource, LayoutFieldType, LayoutFinalize, LayoutPartKind, LayoutPlan, LayoutPreset,
    ReadLimits, Result, SegmentDescriptor, SegmentRepeat,
    format::ReadLimitKey,
    snapshot::{SnapshotCursor, SnapshotFile},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutValue {
    Bytes(Vec<u8>),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I64(i64),
}

impl LayoutValue {
    pub fn to_bytes(&self, field: &'static str) -> Result<Vec<u8>> {
        match self {
            Self::Bytes(bytes) => Ok(bytes.clone()),
            _ => Err(Error::LayoutFieldTypeMismatch(field)),
        }
    }

    pub fn to_u8(&self, field: &'static str) -> Result<u8> {
        match self {
            Self::U8(value) => Ok(*value),
            Self::U16(value) => {
                u8::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
            }
            Self::U32(value) => {
                u8::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
            }
            Self::U64(value) => {
                u8::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
            }
            Self::I64(value) => {
                u8::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
            }
            Self::Bytes(_) => Err(Error::LayoutFieldTypeMismatch(field)),
        }
    }

    pub fn to_u16(&self, field: &'static str) -> Result<u16> {
        match self {
            Self::U8(value) => Ok(u16::from(*value)),
            Self::U16(value) => Ok(*value),
            Self::U32(value) => {
                u16::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
            }
            Self::U64(value) => {
                u16::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
            }
            Self::I64(value) => {
                u16::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
            }
            Self::Bytes(_) => Err(Error::LayoutFieldTypeMismatch(field)),
        }
    }

    pub fn to_u32(&self, field: &'static str) -> Result<u32> {
        match self {
            Self::U8(value) => Ok(u32::from(*value)),
            Self::U16(value) => Ok(u32::from(*value)),
            Self::U32(value) => Ok(*value),
            Self::U64(value) => {
                u32::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
            }
            Self::I64(value) => {
                u32::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
            }
            Self::Bytes(_) => Err(Error::LayoutFieldTypeMismatch(field)),
        }
    }

    pub fn to_u64(&self, field: &'static str) -> Result<u64> {
        match self {
            Self::U8(value) => Ok(u64::from(*value)),
            Self::U16(value) => Ok(u64::from(*value)),
            Self::U32(value) => Ok(u64::from(*value)),
            Self::U64(value) => Ok(*value),
            Self::I64(value) => {
                u64::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
            }
            Self::Bytes(_) => Err(Error::LayoutFieldTypeMismatch(field)),
        }
    }

    pub fn to_i64(&self, field: &'static str) -> Result<i64> {
        match self {
            Self::U8(value) => Ok(i64::from(*value)),
            Self::U16(value) => Ok(i64::from(*value)),
            Self::U32(value) => Ok(i64::from(*value)),
            Self::U64(value) => {
                i64::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
            }
            Self::I64(value) => Ok(*value),
            Self::Bytes(_) => Err(Error::LayoutFieldTypeMismatch(field)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutFieldValue {
    pub name: &'static str,
    pub value: LayoutValue,
}

#[derive(Clone, Copy, Debug)]
pub struct SegmentWrite<'a> {
    pub name: &'static str,
    pub fields: &'a [LayoutFieldValue],
    pub footer_fields: &'a [LayoutFieldValue],
    pub metadata: &'a [u8],
    pub raw: &'a [u8],
}

#[derive(Clone, Copy)]
pub struct SegmentWriteStream<'a, M, R> {
    pub name: &'static str,
    pub fields: &'a [LayoutFieldValue],
    pub footer_fields: &'a [LayoutFieldValue],
    pub write_metadata: M,
    pub write_raw: R,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutSegmentInfo {
    pub name: &'static str,
    pub fields: Vec<LayoutFieldValue>,
    pub footer_fields: Vec<LayoutFieldValue>,
    pub segment_start: u64,
    pub lead_in_len: u64,
    pub metadata_offset: u64,
    pub metadata_len: u64,
    pub raw_offset: u64,
    pub raw_len: u64,
    pub footer_offset: u64,
    pub footer_len: u64,
    pub segment_end: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutFileInfo {
    pub plan: LayoutPlan,
    pub file_header_len: u64,
    pub file_header_fields: Vec<LayoutFieldValue>,
    pub segments: Vec<LayoutSegmentInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutScanReport {
    pub plan: LayoutPlan,
    pub file_header_len: u64,
    pub file_header_fields: Vec<LayoutFieldValue>,
    pub segments: Vec<LayoutSegmentInfo>,
    pub tail: Option<LayoutTailInfo>,
}

impl LayoutScanReport {
    pub fn is_complete(&self) -> bool {
        self.tail.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutTailInfo {
    pub offset: u64,
    pub file_len: u64,
    pub kind: LayoutTailKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutTailKind {
    TruncatedHeader,
    TruncatedLeadIn,
    InvalidSegmentBounds,
    LiteralMismatch,
    AmbiguousSegment,
    NoMatchingSegment,
    RepeatedOnceSegment,
}

impl LayoutSegmentInfo {
    pub fn field(&self, name: &str) -> Option<&LayoutValue> {
        self.fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| &field.value)
    }

    pub fn footer_field(&self, name: &str) -> Option<&LayoutValue> {
        self.footer_fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| &field.value)
    }
}

#[derive(Debug)]
pub struct LayoutWriter {
    spec: FormatSpec,
    path: PathBuf,
    file: File,
    segment_counts: Vec<SegmentCount>,
    index_bytes: u64,
    poisoned: bool,
    _lock: crate::file::WriterLock,
}

#[derive(Debug)]
pub struct LayoutReader {
    spec: FormatSpec,
    path: PathBuf,
    snapshot: SnapshotFile,
    file_header_len: u64,
    file_header_fields: Vec<LayoutFieldValue>,
    segments: Vec<LayoutSegmentInfo>,
}

#[derive(Clone, Copy, Debug)]
struct Patch {
    offset: u64,
    field: LayoutFieldDescriptor,
}

#[derive(Clone, Copy, Debug)]
struct Anchors {
    segment_start: u64,
    after_lead_in: u64,
    metadata_start: u64,
    raw_region_start: u64,
    segment_end: u64,
    footer_start: u64,
    footer_end: u64,
}

#[derive(Clone, Debug)]
struct SegmentDispatch {
    descriptor: SegmentDescriptor,
    key: Option<Vec<u8>>,
    lead_in_len: u64,
}

#[derive(Clone, Debug)]
struct SegmentCount {
    name: &'static str,
    count: u64,
}

struct LayoutFileHeaderRead {
    len: u64,
    fields: Vec<LayoutFieldValue>,
    index_bytes: u64,
}

#[derive(Debug)]
struct LayoutSegmentReadError {
    error: Error,
    recoverable_tail: bool,
}

impl LayoutSegmentReadError {
    fn recoverable(error: Error) -> Self {
        Self {
            error,
            recoverable_tail: true,
        }
    }
}

impl From<Error> for LayoutSegmentReadError {
    fn from(error: Error) -> Self {
        Self {
            error,
            recoverable_tail: false,
        }
    }
}

struct CountingWriter<'a, W: Write> {
    inner: &'a mut W,
    bytes_written: u64,
    start_offset: u64,
    max_region_len: Option<u64>,
    max_file_len: Option<u64>,
    max_scan_len: Option<u64>,
    failure: Option<CountingWriteFailure>,
}

impl<'a, W: Write> CountingWriter<'a, W> {
    fn new(
        inner: &'a mut W,
        start_offset: u64,
        max_region_len: Option<u64>,
        max_file_len: Option<u64>,
        max_scan_len: Option<u64>,
    ) -> Self {
        Self {
            inner,
            bytes_written: 0,
            start_offset,
            max_region_len,
            max_file_len,
            max_scan_len,
            failure: None,
        }
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    fn take_failure(&mut self) -> Option<Error> {
        self.failure.take().map(CountingWriteFailure::into_error)
    }

    fn reject(&mut self, failure: CountingWriteFailure) -> io::Result<usize> {
        self.failure = Some(failure);
        Err(io::Error::other("layout stream exceeded configured bounds"))
    }
}

#[derive(Clone, Copy, Debug)]
enum CountingWriteFailure {
    LimitExceeded {
        resource: &'static str,
        actual: u64,
        limit: u64,
    },
    ArithmeticOverflow {
        resource: &'static str,
    },
}

impl CountingWriteFailure {
    fn into_error(self) -> Error {
        match self {
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => Error::LimitExceeded {
                resource,
                actual,
                limit,
            },
            Self::ArithmeticOverflow { resource } => Error::ResourceArithmeticOverflow { resource },
        }
    }
}

impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let requested = match u64::try_from(buf.len()) {
            Ok(requested) => requested,
            Err(_) => {
                return self.reject(CountingWriteFailure::ArithmeticOverflow {
                    resource: "layout stream length",
                });
            }
        };
        let prospective_region_len = match self.bytes_written.checked_add(requested) {
            Some(len) => len,
            None => {
                return self.reject(CountingWriteFailure::ArithmeticOverflow {
                    resource: "layout region length",
                });
            }
        };
        if let Some(limit) = self.max_region_len
            && prospective_region_len > limit
        {
            return self.reject(CountingWriteFailure::LimitExceeded {
                resource: "record payload length",
                actual: prospective_region_len,
                limit,
            });
        }
        let prospective_file_len = match self.start_offset.checked_add(prospective_region_len) {
            Some(len) => len,
            None => {
                return self.reject(CountingWriteFailure::ArithmeticOverflow {
                    resource: "layout file growth",
                });
            }
        };
        if let Some(limit) = self.max_file_len
            && prospective_file_len > limit
        {
            return self.reject(CountingWriteFailure::LimitExceeded {
                resource: "file length",
                actual: prospective_file_len,
                limit,
            });
        }
        if let Some(limit) = self.max_scan_len
            && prospective_file_len > limit
        {
            return self.reject(CountingWriteFailure::LimitExceeded {
                resource: "scan bytes",
                actual: prospective_file_len,
                limit,
            });
        }
        let written = self.inner.write(buf)?;
        self.bytes_written = self
            .bytes_written
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::other("layout stream write length overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl FormatSpec {
    pub fn create_layout_writer<P: AsRef<Path>>(self, path: P) -> Result<LayoutWriter> {
        let spec = ordinary_layout_spec(self);
        spec.validate()?;
        LayoutWriter::create_inner(spec, path, &[])
    }

    pub fn create_layout_writer_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<LayoutWriter> {
        self.tighten_read_limits(limits).create_layout_writer(path)
    }

    pub fn create_layout_writer_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<LayoutWriter> {
        let spec = self.authorize_trusted_read();
        spec.validate()?;
        LayoutWriter::create_inner(spec, path, &[])
    }

    pub fn create_layout_writer_with_header<P: AsRef<Path>>(
        self,
        path: P,
        fields: &[LayoutFieldValue],
    ) -> Result<LayoutWriter> {
        let spec = ordinary_layout_spec(self);
        spec.validate()?;
        LayoutWriter::create_inner(spec, path, fields)
    }

    pub fn create_layout_writer_with_header_and_limits<P: AsRef<Path>>(
        self,
        path: P,
        fields: &[LayoutFieldValue],
        limits: ReadLimits,
    ) -> Result<LayoutWriter> {
        self.tighten_read_limits(limits)
            .create_layout_writer_with_header(path, fields)
    }

    pub fn create_layout_writer_with_header_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
        fields: &[LayoutFieldValue],
    ) -> Result<LayoutWriter> {
        let spec = self.authorize_trusted_read();
        spec.validate()?;
        LayoutWriter::create_inner(spec, path, fields)
    }

    pub fn open_layout_writer<P: AsRef<Path>>(self, path: P) -> Result<LayoutWriter> {
        let spec = ordinary_layout_spec(self);
        spec.validate()?;
        LayoutWriter::open_inner(spec, path)
    }

    pub fn open_layout_writer_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<LayoutWriter> {
        self.tighten_read_limits(limits).open_layout_writer(path)
    }

    pub fn open_layout_writer_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<LayoutWriter> {
        let spec = self.authorize_trusted_read();
        spec.validate()?;
        LayoutWriter::open_inner(spec, path)
    }

    pub fn open_layout_reader<P: AsRef<Path>>(self, path: P) -> Result<LayoutReader> {
        let spec = ordinary_layout_spec(self);
        spec.validate()?;
        LayoutReader::open_inner(spec, path)
    }

    pub fn open_layout_reader_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<LayoutReader> {
        self.tighten_read_limits(limits).open_layout_reader(path)
    }

    pub fn open_layout_reader_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<LayoutReader> {
        let spec = self.authorize_trusted_read();
        spec.validate()?;
        LayoutReader::open_inner(spec, path)
    }

    pub fn inspect_layout_file<P: AsRef<Path>>(self, path: P) -> Result<LayoutFileInfo> {
        let spec = ordinary_layout_spec(self);
        spec.validate()?;
        inspect_layout_file_inner(spec, path)
    }

    pub fn inspect_layout_file_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<LayoutFileInfo> {
        self.tighten_read_limits(limits).inspect_layout_file(path)
    }

    pub fn inspect_layout_file_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<LayoutFileInfo> {
        let spec = self.authorize_trusted_read();
        spec.validate()?;
        inspect_layout_file_inner(spec, path)
    }

    pub fn inspect_layout_file_report<P: AsRef<Path>>(self, path: P) -> Result<LayoutScanReport> {
        let spec = ordinary_layout_spec(self);
        spec.validate()?;
        inspect_layout_file_report_inner(spec, path)
    }

    pub fn inspect_layout_file_report_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<LayoutScanReport> {
        self.tighten_read_limits(limits)
            .inspect_layout_file_report(path)
    }

    pub fn inspect_layout_file_report_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<LayoutScanReport> {
        let spec = self.authorize_trusted_read();
        spec.validate()?;
        inspect_layout_file_report_inner(spec, path)
    }
}

fn inspect_layout_file_inner<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<LayoutFileInfo> {
    if spec.layout.is_varve_native_default() {
        inspect_native_layout_file(spec, path)
    } else {
        let reader = LayoutReader::open_inner(spec, path)?;
        Ok(LayoutFileInfo {
            plan: spec.effective_layout(),
            file_header_len: reader.file_header_len,
            file_header_fields: reader.file_header_fields,
            segments: reader.segments,
        })
    }
}

fn inspect_layout_file_report_inner<P: AsRef<Path>>(
    spec: FormatSpec,
    path: P,
) -> Result<LayoutScanReport> {
    if spec.layout.is_varve_native_default() {
        let info = inspect_native_layout_file(spec, path)?;
        return Ok(LayoutScanReport {
            plan: info.plan,
            file_header_len: info.file_header_len,
            file_header_fields: info.file_header_fields,
            segments: info.segments,
            tail: None,
        });
    }

    ensure_custom_layout_spec(spec)?;
    ensure_layout_open_limits(spec)?;
    let snapshot = SnapshotFile::new(File::open(path.as_ref())?)?;
    spec.read_limits
        .check(ReadLimitKey::FileLen, snapshot.len())?;
    let file_len = snapshot.len();
    let header = match read_file_header(spec, &snapshot) {
        Ok(header) => header,
        Err(error) => {
            if let Some(tail) = layout_tail_info(&error, 0, file_len) {
                let file_header_len = spec
                    .layout
                    .file_header()
                    .map(file_header_len)
                    .transpose()?
                    .unwrap_or(0);
                return Ok(LayoutScanReport {
                    plan: spec.effective_layout(),
                    file_header_len,
                    file_header_fields: Vec::new(),
                    segments: Vec::new(),
                    tail: Some(tail),
                });
            }
            return Err(error);
        }
    };
    let (segments, tail) =
        scan_layout_segments_report(spec, &snapshot, header.len, header.index_bytes)?;
    Ok(LayoutScanReport {
        plan: spec.effective_layout(),
        file_header_len: header.len,
        file_header_fields: header.fields,
        segments,
        tail,
    })
}

impl LayoutWriter {
    pub fn create<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        spec.validate()?;
        Self::create_inner(spec, path, &[])
    }

    pub fn create_with_limits<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        limits: ReadLimits,
    ) -> Result<Self> {
        Self::create(spec.tighten_read_limits(limits), path)
    }

    pub fn create_trusted_unbounded<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        let spec = spec.authorize_trusted_read();
        spec.validate()?;
        Self::create_inner(spec, path, &[])
    }

    pub fn create_with_header<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        fields: &[LayoutFieldValue],
    ) -> Result<Self> {
        spec.validate()?;
        Self::create_inner(spec, path, fields)
    }

    pub fn create_with_header_and_limits<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        fields: &[LayoutFieldValue],
        limits: ReadLimits,
    ) -> Result<Self> {
        Self::create_with_header(spec.tighten_read_limits(limits), path, fields)
    }

    pub fn create_with_header_trusted_unbounded<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        fields: &[LayoutFieldValue],
    ) -> Result<Self> {
        let spec = spec.authorize_trusted_read();
        spec.validate()?;
        Self::create_inner(spec, path, fields)
    }

    fn create_inner<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        fields: &[LayoutFieldValue],
    ) -> Result<Self> {
        ensure_custom_layout_spec(spec)?;
        ensure_layout_writer_create_limits(spec)?;
        let (header_len, index_bytes) = match spec.layout.file_header() {
            Some(header) => {
                ensure_no_unexpected_fields_for(header.fields, fields)?;
                (
                    file_header_len(header)?,
                    layout_fields_resident_bytes(header.fields)?,
                )
            }
            None if fields.is_empty() => (0, 0),
            None => return Err(Error::LayoutFieldUnexpected(fields[0].name.to_string())),
        };
        spec.read_limits.check(ReadLimitKey::FileLen, header_len)?;
        spec.read_limits
            .check(ReadLimitKey::ScanBytes, header_len)?;
        spec.read_limits
            .check(ReadLimitKey::IndexBytes, index_bytes)?;
        let path = path.as_ref().to_path_buf();
        let lock = crate::file::WriterLock::acquire(&path)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        if let Some(header) = spec.layout.file_header() {
            write_static_layout_fields(&mut file, header.fields, fields, spec.endian)?;
        }
        Ok(Self {
            spec,
            path,
            file,
            segment_counts: initial_segment_counts(spec)?,
            index_bytes,
            poisoned: false,
            _lock: lock,
        })
    }

    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        spec.validate()?;
        Self::open_inner(spec, path)
    }

    pub fn open_with_limits<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        limits: ReadLimits,
    ) -> Result<Self> {
        Self::open(spec.tighten_read_limits(limits), path)
    }

    pub fn open_trusted_unbounded<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        let spec = spec.authorize_trusted_read();
        spec.validate()?;
        Self::open_inner(spec, path)
    }

    fn open_inner<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        ensure_custom_layout_spec(spec)?;
        ensure_layout_writer_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        let lock = crate::file::WriterLock::acquire(&path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        spec.read_limits
            .check(ReadLimitKey::FileLen, snapshot.len())?;
        let header = read_file_header(spec, &snapshot)?;
        let (segments, index_bytes) =
            scan_layout_segments(spec, &snapshot, header.len, header.index_bytes)?;
        let segment_counts = segment_counts_from_infos(spec, &segments)?;
        file.seek(SeekFrom::Start(snapshot.len()))?;
        Ok(Self {
            spec,
            path,
            file,
            segment_counts,
            index_bytes,
            poisoned: false,
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn spec(&self) -> FormatSpec {
        self.spec.ordinary_read()
    }

    pub fn write_segment(&mut self, segment: SegmentWrite<'_>) -> Result<LayoutSegmentInfo> {
        self.write_segment_streamed(SegmentWriteStream {
            name: segment.name,
            fields: segment.fields,
            footer_fields: segment.footer_fields,
            write_metadata: |writer: &mut dyn Write| {
                writer.write_all(segment.metadata)?;
                Ok(())
            },
            write_raw: |writer: &mut dyn Write| {
                writer.write_all(segment.raw)?;
                Ok(())
            },
        })
    }

    pub fn write_segment_streamed<M, R>(
        &mut self,
        segment: SegmentWriteStream<'_, M, R>,
    ) -> Result<LayoutSegmentInfo>
    where
        M: FnOnce(&mut dyn Write) -> Result<()>,
        R: FnOnce(&mut dyn Write) -> Result<()>,
    {
        self.ensure_not_poisoned()?;
        let descriptor = segment_descriptor(self.spec, segment.name)?;
        self.ensure_segment_can_write(descriptor)?;
        let footer_fields = descriptor.footer.map(|footer| footer.fields).unwrap_or(&[]);
        prevalidate_patchable_layout_fields(
            descriptor.lead_in.fields,
            segment.fields,
            self.spec.endian,
        )?;
        prevalidate_patchable_layout_fields(
            footer_fields,
            segment.footer_fields,
            self.spec.endian,
        )?;
        let lead_in_len = descriptor_lead_in_len(descriptor)?;
        let footer_len = descriptor_footer_len(descriptor)?;
        let original_eof = self.file.metadata()?.len();
        self.spec
            .read_limits
            .check(ReadLimitKey::FileLen, original_eof)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::ScanBytes, original_eof)?;
        let original_cursor = self.file.stream_position()?;
        let after_lead_in =
            original_eof
                .checked_add(lead_in_len)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "layout file growth",
                })?;
        let minimum_segment_end =
            after_lead_in
                .checked_add(footer_len)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "layout file growth",
                })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::FileLen, minimum_segment_end)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::ScanBytes, minimum_segment_end)?;
        let next_index_bytes = self
            .index_bytes
            .checked_add(layout_segment_resident_bytes(descriptor)?)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "layout index bytes",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::IndexBytes, next_index_bytes)?;
        let max_payload_len = self
            .spec
            .read_limits
            .require(ReadLimitKey::RecordPayloadLen)?;
        let max_file_len = self.spec.read_limits.require(ReadLimitKey::FileLen)?;
        let max_scan_len = self.spec.read_limits.require(ReadLimitKey::ScanBytes)?;
        let (segment_count_index, original_segment_count, next_segment_count) =
            self.segment_count_checkpoint(descriptor.name)?;

        self.poisoned = true;
        let result = (|| {
            self.file.seek(SeekFrom::Start(original_eof))?;
            let segment_start = original_eof;
            let mut patches = Vec::new();
            write_patchable_layout_fields(
                &mut self.file,
                descriptor.lead_in.fields,
                segment.fields,
                self.spec.endian,
                &mut patches,
            )?;
            let metadata_offset = after_lead_in;
            let mut metadata_writer = CountingWriter::new(
                &mut self.file,
                metadata_offset,
                max_payload_len,
                max_file_len,
                max_scan_len,
            );
            let metadata_result = (segment.write_metadata)(&mut metadata_writer);
            if let Some(error) = metadata_writer.take_failure() {
                return Err(error);
            }
            metadata_result?;
            let metadata_len = metadata_writer.bytes_written();
            let raw_offset = metadata_offset.checked_add(metadata_len).ok_or(
                Error::ResourceArithmeticOverflow {
                    resource: "layout file growth",
                },
            )?;
            let mut raw_writer = CountingWriter::new(
                &mut self.file,
                raw_offset,
                max_payload_len,
                max_file_len,
                max_scan_len,
            );
            let raw_result = (segment.write_raw)(&mut raw_writer);
            if let Some(error) = raw_writer.take_failure() {
                return Err(error);
            }
            raw_result?;
            let raw_len = raw_writer.bytes_written();
            let footer_offset =
                raw_offset
                    .checked_add(raw_len)
                    .ok_or(Error::ResourceArithmeticOverflow {
                        resource: "layout file growth",
                    })?;
            let footer_end =
                footer_offset
                    .checked_add(footer_len)
                    .ok_or(Error::ResourceArithmeticOverflow {
                        resource: "layout file growth",
                    })?;
            self.spec
                .read_limits
                .check(ReadLimitKey::FileLen, footer_end)?;
            self.spec
                .read_limits
                .check(ReadLimitKey::ScanBytes, footer_end)?;
            if let Some(footer) = descriptor.footer {
                write_patchable_layout_fields(
                    &mut self.file,
                    footer.fields,
                    segment.footer_fields,
                    self.spec.endian,
                    &mut patches,
                )?;
            }
            let segment_end = footer_end;
            let anchors = Anchors {
                segment_start,
                after_lead_in,
                metadata_start: metadata_offset,
                raw_region_start: raw_offset,
                segment_end,
                footer_start: footer_offset,
                footer_end,
            };
            let fields =
                collect_written_layout_values(descriptor.lead_in.fields, segment.fields, anchors)?;
            let footer_fields = descriptor
                .footer
                .map(|footer| {
                    collect_written_layout_values(footer.fields, segment.footer_fields, anchors)
                })
                .unwrap_or_else(|| Ok(Vec::new()))?;

            for patch in patches {
                let value = finalized_value(patch.field, anchors)?;
                self.file.seek(SeekFrom::Start(patch.offset))?;
                write_layout_value(&mut self.file, patch.field, &value, self.spec.endian)?;
            }
            self.file.seek(SeekFrom::Start(segment_end))?;

            Ok(LayoutSegmentInfo {
                name: descriptor.name,
                fields,
                footer_fields,
                segment_start,
                lead_in_len,
                metadata_offset,
                metadata_len,
                raw_offset,
                raw_len,
                footer_offset,
                footer_len,
                segment_end,
            })
        })();

        match result {
            Ok(info) => {
                self.segment_counts[segment_count_index].count = next_segment_count;
                self.index_bytes = next_index_bytes;
                self.poisoned = false;
                Ok(info)
            }
            Err(error) => {
                if let Err(source) = self.rollback_segment_write(
                    original_eof,
                    original_cursor,
                    segment_count_index,
                    original_segment_count,
                ) {
                    return Err(Error::WriteRollbackFailed {
                        operation: "layout segment write",
                        source,
                    });
                }
                self.poisoned = false;
                Err(error)
            }
        }
    }

    pub fn flush(&mut self) -> Result<()> {
        self.ensure_not_poisoned()?;
        self.file.flush()?;
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        self.ensure_not_poisoned()?;
        self.file.sync_all()?;
        Ok(())
    }

    fn ensure_not_poisoned(&self) -> Result<()> {
        if self.poisoned {
            Err(Error::WriterPoisoned("layout"))
        } else {
            Ok(())
        }
    }

    fn ensure_segment_can_write(&self, descriptor: SegmentDescriptor) -> Result<()> {
        if descriptor.repeat == SegmentRepeat::Once
            && self
                .segment_counts
                .iter()
                .any(|count| count.name == descriptor.name && count.count > 0)
        {
            return Err(Error::LayoutRepeatedOnceSegment {
                segment: descriptor.name.to_string(),
                offset: self.file.metadata()?.len(),
            });
        }
        let segment_count = self.segment_counts.iter().try_fold(0u64, |total, count| {
            total
                .checked_add(count.count)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "layout segment count",
                })
        })?;
        let prospective_count =
            segment_count
                .checked_add(1)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "layout segment count",
                })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Segments, prospective_count)?;
        Ok(())
    }

    fn segment_count_checkpoint(&self, name: &'static str) -> Result<(usize, u64, u64)> {
        let (index, count) = self
            .segment_counts
            .iter()
            .enumerate()
            .find(|(_, count)| count.name == name)
            .map(|(index, count)| (index, count.count))
            .ok_or(Error::InvalidFormatSpec("layout segment count is missing"))?;
        let next = count
            .checked_add(1)
            .ok_or(Error::InvalidFormatSpec("layout segment count overflow"))?;
        Ok((index, count, next))
    }

    fn rollback_segment_write(
        &mut self,
        original_eof: u64,
        original_cursor: u64,
        segment_count_index: usize,
        original_segment_count: u64,
    ) -> io::Result<()> {
        self.segment_counts[segment_count_index].count = original_segment_count;
        let truncate_error = self.file.set_len(original_eof).err();
        let seek_error = self
            .file
            .seek(SeekFrom::Start(original_cursor))
            .map(|_| ())
            .err();
        match truncate_error.or(seek_error) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl LayoutReader {
    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        spec.validate()?;
        Self::open_inner(spec, path)
    }

    pub fn open_with_limits<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        limits: ReadLimits,
    ) -> Result<Self> {
        Self::open(spec.tighten_read_limits(limits), path)
    }

    pub fn open_trusted_unbounded<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        let spec = spec.authorize_trusted_read();
        spec.validate()?;
        Self::open_inner(spec, path)
    }

    fn open_inner<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        ensure_custom_layout_spec(spec)?;
        ensure_layout_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        let snapshot = SnapshotFile::new(File::open(&path)?)?;
        spec.read_limits
            .check(ReadLimitKey::FileLen, snapshot.len())?;
        let header = read_file_header(spec, &snapshot)?;
        let (segments, _) = scan_layout_segments(spec, &snapshot, header.len, header.index_bytes)?;
        Ok(Self {
            spec,
            path,
            snapshot,
            file_header_len: header.len,
            file_header_fields: header.fields,
            segments,
        })
    }

    pub fn spec(&self) -> FormatSpec {
        self.spec.ordinary_read()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_header_len(&self) -> u64 {
        self.file_header_len
    }

    pub fn file_header_fields(&self) -> &[LayoutFieldValue] {
        &self.file_header_fields
    }

    pub fn file_header_field(&self, name: &str) -> Option<&LayoutValue> {
        self.file_header_fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| &field.value)
    }

    pub fn segments(&self) -> &[LayoutSegmentInfo] {
        &self.segments
    }

    pub fn read_metadata(&self, index: usize) -> Result<Vec<u8>> {
        let segment = self
            .segments
            .get(index)
            .ok_or(Error::LayoutInvalidSegmentBounds { offset: 0 })?;
        self.read_snapshot_range(segment.metadata_offset, segment.metadata_len)
    }

    pub fn read_metadata_range(&self, index: usize, offset: u64, len: u64) -> Result<Vec<u8>> {
        let segment = self
            .segments
            .get(index)
            .ok_or(Error::LayoutInvalidSegmentBounds { offset: 0 })?;
        let absolute = checked_subrange(
            segment.metadata_offset,
            segment.metadata_len,
            offset,
            len,
            segment.segment_start,
        )?;
        self.read_snapshot_range(absolute, len)
    }

    pub fn read_raw(&self, index: usize) -> Result<Vec<u8>> {
        let segment = self
            .segments
            .get(index)
            .ok_or(Error::LayoutInvalidSegmentBounds { offset: 0 })?;
        self.read_snapshot_range(segment.raw_offset, segment.raw_len)
    }

    pub fn read_raw_range(&self, index: usize, offset: u64, len: u64) -> Result<Vec<u8>> {
        let segment = self
            .segments
            .get(index)
            .ok_or(Error::LayoutInvalidSegmentBounds { offset: 0 })?;
        let absolute = checked_subrange(
            segment.raw_offset,
            segment.raw_len,
            offset,
            len,
            segment.segment_start,
        )?;
        self.read_snapshot_range(absolute, len)
    }

    fn read_snapshot_range(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
        let limit = self
            .spec
            .read_limits
            .require(ReadLimitKey::RecordPayloadLen)?
            .unwrap_or(u64::MAX);
        self.snapshot
            .read_vec_at(offset, len, limit, "record payload length")
    }
}

fn checked_subrange(
    base: u64,
    total_len: u64,
    offset: u64,
    len: u64,
    error_offset: u64,
) -> Result<u64> {
    let end = offset
        .checked_add(len)
        .ok_or(Error::LayoutInvalidSegmentBounds {
            offset: error_offset,
        })?;
    if end > total_len {
        return Err(Error::LayoutInvalidSegmentBounds {
            offset: error_offset,
        });
    }
    base.checked_add(offset)
        .ok_or(Error::LayoutInvalidSegmentBounds {
            offset: error_offset,
        })
}

fn ordinary_layout_spec(spec: FormatSpec) -> FormatSpec {
    spec.ordinary_read()
}

fn ensure_layout_open_limits(spec: FormatSpec) -> Result<()> {
    for key in [
        ReadLimitKey::FileLen,
        ReadLimitKey::ScanBytes,
        ReadLimitKey::Segments,
        ReadLimitKey::IndexBytes,
    ] {
        spec.read_limits.require(key)?;
    }
    Ok(())
}

fn ensure_layout_writer_create_limits(spec: FormatSpec) -> Result<()> {
    ensure_layout_open_limits(spec)?;
    spec.read_limits.require(ReadLimitKey::RecordPayloadLen)?;
    Ok(())
}

fn ensure_layout_writer_open_limits(spec: FormatSpec) -> Result<()> {
    ensure_layout_open_limits(spec)?;
    spec.read_limits.require(ReadLimitKey::RecordPayloadLen)?;
    Ok(())
}

fn ensure_custom_layout_spec(spec: FormatSpec) -> Result<()> {
    if spec.layout.preset != LayoutPreset::None {
        return Err(Error::InvalidFormatSpec(
            "layout reader/writer requires preset: none",
        ));
    }
    if spec.layout.first_segment().is_none() {
        return Err(Error::InvalidFormatSpec(
            "layout reader/writer requires a segment descriptor",
        ));
    }
    Ok(())
}

fn inspect_native_layout_file<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<LayoutFileInfo> {
    let path = path.as_ref();
    let mut header_file = File::open(path)?;
    let file_header_len = crate::file::read_file_header(spec, &mut header_file)?;
    let file_header_fields = read_native_file_header_fields(path, spec, file_header_len)?;
    let file = crate::file::VarveFile::open_readonly(spec, path)?;
    let segments = file
        .index_entries()
        .iter()
        .map(native_record_to_layout_segment)
        .collect::<Result<Vec<_>>>()?;
    Ok(LayoutFileInfo {
        plan: spec.effective_layout(),
        file_header_len,
        file_header_fields,
        segments,
    })
}

fn read_native_file_header_fields(
    path: &Path,
    spec: FormatSpec,
    file_header_len: u64,
) -> Result<Vec<LayoutFieldValue>> {
    let mut file = File::open(path)?;
    let header_len = usize::try_from(file_header_len).map_err(|_| Error::LengthOverflow {
        value: file_header_len,
    })?;
    let mut bytes = vec![0; header_len];
    file.read_exact(&mut bytes)?;
    let mut position = 0usize;
    let mut fields = Vec::new();

    let magic_len = spec.magic.len();
    fields.push(LayoutFieldValue {
        name: "magic",
        value: LayoutValue::Bytes(take_bytes(&bytes, &mut position, magic_len)?.to_vec()),
    });
    fields.push(LayoutFieldValue {
        name: "container_marker",
        value: LayoutValue::Bytes(take_bytes(&bytes, &mut position, 6)?.to_vec()),
    });
    fields.push(LayoutFieldValue {
        name: "format_version",
        value: LayoutValue::U16(read_u16_from_header(&bytes, &mut position)?),
    });
    fields.push(LayoutFieldValue {
        name: "endian",
        value: LayoutValue::U8(*take_bytes(&bytes, &mut position, 1)?.first().ok_or(
            Error::LayoutTruncatedHeader {
                offset: position as u64,
            },
        )?),
    });
    fields.push(LayoutFieldValue {
        name: "flags",
        value: LayoutValue::U8(*take_bytes(&bytes, &mut position, 1)?.first().ok_or(
            Error::LayoutTruncatedHeader {
                offset: position as u64,
            },
        )?),
    });
    fields.push(LayoutFieldValue {
        name: "schema_hash",
        value: LayoutValue::U64(read_u64_from_header(&bytes, &mut position)?),
    });

    if position < bytes.len() {
        let extension_len = read_u32_from_header(&bytes, &mut position)?;
        fields.push(LayoutFieldValue {
            name: "extension_len",
            value: LayoutValue::U32(extension_len),
        });
        if extension_len > 0 {
            let extension_len =
                usize::try_from(extension_len).map_err(|_| Error::LengthOverflow {
                    value: u64::from(extension_len),
                })?;
            fields.push(LayoutFieldValue {
                name: "extensions",
                value: LayoutValue::Bytes(
                    take_bytes(&bytes, &mut position, extension_len)?.to_vec(),
                ),
            });
        }
    }

    Ok(fields)
}

fn take_bytes<'a>(bytes: &'a [u8], position: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = position
        .checked_add(len)
        .ok_or(Error::LayoutTruncatedHeader {
            offset: *position as u64,
        })?;
    if end > bytes.len() {
        return Err(Error::LayoutTruncatedHeader {
            offset: *position as u64,
        });
    }
    let value = &bytes[*position..end];
    *position = end;
    Ok(value)
}

fn read_u16_from_header(bytes: &[u8], position: &mut usize) -> Result<u16> {
    let mut value = [0; 2];
    value.copy_from_slice(take_bytes(bytes, position, 2)?);
    Ok(u16::from_le_bytes(value))
}

fn read_u32_from_header(bytes: &[u8], position: &mut usize) -> Result<u32> {
    let mut value = [0; 4];
    value.copy_from_slice(take_bytes(bytes, position, 4)?);
    Ok(u32::from_le_bytes(value))
}

fn read_u64_from_header(bytes: &[u8], position: &mut usize) -> Result<u64> {
    let mut value = [0; 8];
    value.copy_from_slice(take_bytes(bytes, position, 8)?);
    Ok(u64::from_le_bytes(value))
}

fn native_record_to_layout_segment(
    entry: &crate::file::RecordIndexEntry,
) -> Result<LayoutSegmentInfo> {
    let segment_end = entry.checked_physical_end()?;
    let footer_offset = entry.footer_offset.unwrap_or(segment_end);
    let footer_len = if entry.footer_offset.is_some() {
        crate::file::RECORD_FOOTER_LEN
    } else {
        0
    };
    let mut footer_fields = Vec::new();
    if entry.footer_offset.is_some() {
        footer_fields.push(LayoutFieldValue {
            name: "prev_same_block_offset",
            value: LayoutValue::U64(entry.prev_same_block_offset.unwrap_or(0)),
        });
        footer_fields.push(LayoutFieldValue {
            name: "prev_same_key_offset",
            value: LayoutValue::U64(entry.prev_same_key_offset.unwrap_or(0)),
        });
    }
    Ok(LayoutSegmentInfo {
        name: "VarveRecord",
        fields: vec![
            LayoutFieldValue {
                name: "block_id",
                value: LayoutValue::U32(entry.block_id),
            },
            LayoutFieldValue {
                name: "block_version",
                value: LayoutValue::U16(entry.block_version),
            },
            LayoutFieldValue {
                name: "flags",
                value: LayoutValue::U16(entry.flags),
            },
            LayoutFieldValue {
                name: "sequence",
                value: LayoutValue::U64(entry.sequence),
            },
            LayoutFieldValue {
                name: "payload_len",
                value: LayoutValue::U64(entry.payload_len),
            },
            LayoutFieldValue {
                name: "checksum",
                value: LayoutValue::U32(entry.checksum),
            },
            LayoutFieldValue {
                name: "uncompressed_len_hint",
                value: LayoutValue::U32(entry.uncompressed_len_hint),
            },
        ],
        footer_fields,
        segment_start: entry.record_offset,
        lead_in_len: crate::file::RECORD_HEADER_LEN,
        metadata_offset: entry.payload_offset,
        metadata_len: 0,
        raw_offset: entry.payload_offset,
        raw_len: entry.payload_len,
        footer_offset,
        footer_len,
        segment_end,
    })
}

fn segment_descriptor(spec: FormatSpec, name: &str) -> Result<SegmentDescriptor> {
    spec.layout
        .parts
        .iter()
        .find_map(|part| match part.kind {
            LayoutPartKind::Segment(segment) if segment.name == name => Some(segment),
            _ => None,
        })
        .ok_or_else(|| Error::LayoutSegmentMissing(name.to_string()))
}

fn segment_descriptors(spec: FormatSpec) -> Result<Vec<SegmentDescriptor>> {
    let segments: Vec<_> = spec
        .layout
        .parts
        .iter()
        .filter_map(|part| match part.kind {
            LayoutPartKind::Segment(segment) => Some(segment),
            _ => None,
        })
        .collect();
    if segments.is_empty() {
        return Err(Error::InvalidFormatSpec("missing layout segment"));
    }
    Ok(segments)
}

fn initial_segment_counts(spec: FormatSpec) -> Result<Vec<SegmentCount>> {
    Ok(segment_descriptors(spec)?
        .into_iter()
        .map(|segment| SegmentCount {
            name: segment.name,
            count: 0,
        })
        .collect())
}

fn segment_counts_from_infos(
    spec: FormatSpec,
    segments: &[LayoutSegmentInfo],
) -> Result<Vec<SegmentCount>> {
    let mut counts = initial_segment_counts(spec)?;
    for segment in segments {
        increment_segment_count(&mut counts, segment.name)?;
    }
    Ok(counts)
}

fn descriptor_lead_in_len(segment: SegmentDescriptor) -> Result<u64> {
    descriptor_fields_len(segment.lead_in.fields)
}

fn descriptor_footer_len(segment: SegmentDescriptor) -> Result<u64> {
    segment
        .footer
        .map(|footer| descriptor_fields_len(footer.fields))
        .unwrap_or(Ok(0))
}

fn descriptor_fields_len(fields: &[LayoutFieldDescriptor]) -> Result<u64> {
    fields.iter().try_fold(0u64, |acc, field| {
        acc.checked_add(field.ty.byte_len())
            .ok_or(Error::InvalidFormatSpec("layout fields length overflow"))
    })
}

fn ensure_no_unexpected_fields_for(
    descriptors: &[LayoutFieldDescriptor],
    fields: &[LayoutFieldValue],
) -> Result<()> {
    for (index, field) in fields.iter().enumerate() {
        for other in &fields[(index + 1)..] {
            if field.name == other.name {
                return Err(Error::InvalidFormatSpec("duplicate layout caller field"));
            }
        }
        let expected = descriptors.iter().any(|descriptor| {
            descriptor.name == field.name && matches!(descriptor.source, LayoutFieldSource::Caller)
        });
        if !expected {
            return Err(Error::LayoutFieldUnexpected(field.name.to_string()));
        }
    }
    Ok(())
}

fn prevalidate_patchable_layout_fields(
    descriptors: &[LayoutFieldDescriptor],
    fields: &[LayoutFieldValue],
    endian: Endian,
) -> Result<()> {
    ensure_no_unexpected_fields_for(descriptors, fields)?;
    descriptor_fields_len(descriptors)?;
    let mut sink = io::sink();
    for field in descriptors {
        match field.source {
            LayoutFieldSource::LiteralBytes(bytes) => {
                if !matches!(field.ty, LayoutFieldType::Bytes { len } if len == bytes.len() as u64)
                {
                    return Err(Error::LayoutFieldTypeMismatch(field.name));
                }
            }
            LayoutFieldSource::LiteralU64(value) => {
                write_layout_value(&mut sink, *field, &LayoutValue::U64(value), endian)?;
            }
            LayoutFieldSource::LiteralI64(value) => {
                write_layout_value(&mut sink, *field, &LayoutValue::I64(value), endian)?;
            }
            LayoutFieldSource::Caller => {
                let value = caller_field_value(fields, field.name)?;
                write_layout_value(&mut sink, *field, value, endian)?;
            }
            LayoutFieldSource::Finalize(_) => {
                if matches!(field.ty, LayoutFieldType::Bytes { .. }) {
                    return Err(Error::LayoutFieldTypeMismatch(field.name));
                }
            }
        }
    }
    Ok(())
}

fn caller_field_value<'a>(
    fields: &'a [LayoutFieldValue],
    name: &'static str,
) -> Result<&'a LayoutValue> {
    fields
        .iter()
        .find(|field| field.name == name)
        .map(|field| &field.value)
        .ok_or(Error::LayoutFieldMissing(name))
}

fn write_static_layout_fields(
    file: &mut File,
    descriptors: &[LayoutFieldDescriptor],
    fields: &[LayoutFieldValue],
    endian: Endian,
) -> Result<()> {
    ensure_no_unexpected_fields_for(descriptors, fields)?;
    for field in descriptors {
        match field.source {
            LayoutFieldSource::LiteralBytes(bytes) => file.write_all(bytes)?,
            LayoutFieldSource::LiteralU64(value) => {
                write_numeric(file, *field, &LayoutValue::U64(value), endian)?;
            }
            LayoutFieldSource::LiteralI64(value) => {
                write_numeric(file, *field, &LayoutValue::I64(value), endian)?;
            }
            LayoutFieldSource::Caller => {
                let value = caller_field_value(fields, field.name)?;
                write_layout_value(file, *field, value, endian)?;
            }
            LayoutFieldSource::Finalize(_) => {
                return Err(Error::InvalidFormatSpec(
                    "layout file_header cannot contain finalized fields",
                ));
            }
        }
    }
    Ok(())
}

fn write_patchable_layout_fields(
    file: &mut File,
    descriptors: &[LayoutFieldDescriptor],
    fields: &[LayoutFieldValue],
    endian: Endian,
    patches: &mut Vec<Patch>,
) -> Result<()> {
    for field in descriptors {
        let offset = file.stream_position()?;
        match field.source {
            LayoutFieldSource::LiteralBytes(bytes) => file.write_all(bytes)?,
            LayoutFieldSource::LiteralU64(value) => {
                write_numeric(file, *field, &LayoutValue::U64(value), endian)?;
            }
            LayoutFieldSource::LiteralI64(value) => {
                write_numeric(file, *field, &LayoutValue::I64(value), endian)?;
            }
            LayoutFieldSource::Caller => {
                let value = caller_field_value(fields, field.name)?;
                write_layout_value(file, *field, value, endian)?;
            }
            LayoutFieldSource::Finalize(_) => {
                write_zeroes(file, field.ty.byte_len())?;
                patches.push(Patch {
                    offset,
                    field: *field,
                });
            }
        }
    }
    Ok(())
}

fn collect_written_layout_values(
    descriptors: &[LayoutFieldDescriptor],
    caller_fields: &[LayoutFieldValue],
    anchors: Anchors,
) -> Result<Vec<LayoutFieldValue>> {
    descriptors
        .iter()
        .map(|field| {
            let value = match field.source {
                LayoutFieldSource::LiteralBytes(bytes) => LayoutValue::Bytes(bytes.to_vec()),
                LayoutFieldSource::LiteralU64(value) => {
                    normalize_layout_value(*field, &LayoutValue::U64(value))?
                }
                LayoutFieldSource::LiteralI64(value) => {
                    normalize_layout_value(*field, &LayoutValue::I64(value))?
                }
                LayoutFieldSource::Caller => {
                    normalize_layout_value(*field, caller_field_value(caller_fields, field.name)?)?
                }
                LayoutFieldSource::Finalize(_) => finalized_value(*field, anchors)?,
            };
            Ok(LayoutFieldValue {
                name: field.name,
                value,
            })
        })
        .collect()
}

fn read_file_header(spec: FormatSpec, snapshot: &SnapshotFile) -> Result<LayoutFileHeaderRead> {
    let Some(header) = spec.layout.file_header() else {
        spec.read_limits.check(ReadLimitKey::ScanBytes, 0)?;
        spec.read_limits.check(ReadLimitKey::IndexBytes, 0)?;
        return Ok(LayoutFileHeaderRead {
            len: 0,
            fields: Vec::new(),
            index_bytes: 0,
        });
    };
    let header_len = file_header_len(header)?;
    let index_bytes = layout_fields_resident_bytes(header.fields)?;
    spec.read_limits
        .check(ReadLimitKey::ScanBytes, header_len)?;
    spec.read_limits
        .check(ReadLimitKey::IndexBytes, index_bytes)?;
    if snapshot.len() < header_len {
        return Err(Error::LayoutTruncatedHeader { offset: 0 });
    }
    let fields = read_static_layout_fields(snapshot, header.fields, 0, spec.endian)?;
    Ok(LayoutFileHeaderRead {
        len: header_len,
        fields,
        index_bytes,
    })
}

fn file_header_len(header: FileHeaderDescriptor) -> Result<u64> {
    descriptor_fields_len(header.fields)
}

fn read_static_layout_fields(
    snapshot: &SnapshotFile,
    descriptors: &[LayoutFieldDescriptor],
    start_offset: u64,
    endian: Endian,
) -> Result<Vec<LayoutFieldValue>> {
    let byte_len = descriptor_fields_len(descriptors)?;
    let bytes = snapshot.read_vec_at(
        start_offset,
        byte_len,
        byte_len,
        "layout file-header fields",
    )?;
    let mut position = start_offset;
    let mut relative = 0usize;
    let mut fields = Vec::new();
    fields
        .try_reserve_exact(descriptors.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "layout field index",
            requested: layout_fields_resident_bytes(descriptors).unwrap_or(u64::MAX),
        })?;
    for field in descriptors {
        let value = read_layout_value_from_slice(&bytes, relative, *field, endian)?;
        let stored_value = value.clone();
        match field.source {
            LayoutFieldSource::LiteralBytes(expected) => match value {
                LayoutValue::Bytes(actual) if actual == expected => {}
                _ => {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: position,
                    });
                }
            },
            LayoutFieldSource::LiteralU64(expected) => {
                if !numeric_u64_matches(&value, expected) {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: position,
                    });
                }
            }
            LayoutFieldSource::LiteralI64(expected) => {
                if !matches!(value, LayoutValue::I64(actual) if actual == expected) {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: position,
                    });
                }
            }
            LayoutFieldSource::Caller => {}
            LayoutFieldSource::Finalize(_) => {
                return Err(Error::InvalidFormatSpec(
                    "layout file_header cannot contain finalized fields",
                ));
            }
        }
        fields.push(LayoutFieldValue {
            name: field.name,
            value: stored_value,
        });
        position =
            position
                .checked_add(field.ty.byte_len())
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "layout field offset",
                })?;
        relative = relative
            .checked_add(usize::try_from(field.ty.byte_len()).map_err(|_| {
                Error::LengthOverflow {
                    value: field.ty.byte_len(),
                }
            })?)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "layout field offset",
            })?;
    }
    Ok(fields)
}

fn scan_layout_segments(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    start_offset: u64,
    initial_index_bytes: u64,
) -> Result<(Vec<LayoutSegmentInfo>, u64)> {
    let dispatch = segment_dispatch_table(spec)?;
    let file_len = snapshot.len();
    spec.read_limits
        .check(ReadLimitKey::ScanBytes, start_offset)?;
    spec.read_limits
        .check(ReadLimitKey::IndexBytes, initial_index_bytes)?;
    let mut offset = start_offset;
    let mut index_bytes = initial_index_bytes;
    let mut segments = Vec::new();
    let mut segment_counts = initial_segment_counts(spec)?;
    let mut cursor = snapshot.cursor_at(start_offset)?;
    while offset < file_len {
        check_pending_layout_segment_limits(spec, &dispatch, segments.len(), index_bytes)?;
        let dispatch_index =
            select_segment_descriptor(spec, &dispatch, &mut cursor, offset, file_len)?;
        let descriptor = dispatch[dispatch_index].descriptor;
        if descriptor.repeat == SegmentRepeat::Once
            && segment_counts
                .iter()
                .any(|count| count.name == descriptor.name && count.count > 0)
        {
            return Err(Error::LayoutRepeatedOnceSegment {
                segment: descriptor.name.to_string(),
                offset,
            });
        }
        let next_index_bytes =
            reserve_layout_segment(spec, &mut segments, descriptor, index_bytes)?;
        let info = read_layout_segment_at(spec, &mut cursor, descriptor, offset, file_len)
            .map_err(|failure| failure.error)?;
        if info.segment_end <= offset {
            return Err(Error::LayoutInvalidSegmentBounds { offset });
        }
        offset = info.segment_end;
        increment_segment_count(&mut segment_counts, descriptor.name)?;
        segments.push(info);
        index_bytes = next_index_bytes;
    }
    if offset != file_len {
        return Err(Error::LayoutInvalidSegmentBounds { offset });
    }
    Ok((segments, index_bytes))
}

fn scan_layout_segments_report(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    start_offset: u64,
    initial_index_bytes: u64,
) -> Result<(Vec<LayoutSegmentInfo>, Option<LayoutTailInfo>)> {
    let dispatch = segment_dispatch_table(spec)?;
    let file_len = snapshot.len();
    spec.read_limits
        .check(ReadLimitKey::ScanBytes, start_offset)?;
    spec.read_limits
        .check(ReadLimitKey::IndexBytes, initial_index_bytes)?;
    let mut offset = start_offset;
    let mut index_bytes = initial_index_bytes;
    let mut segments = Vec::new();
    let mut segment_counts = initial_segment_counts(spec)?;
    let mut cursor = snapshot.cursor_at(start_offset)?;
    while offset < file_len {
        check_pending_layout_segment_limits(spec, &dispatch, segments.len(), index_bytes)?;
        let dispatch_index =
            match select_segment_descriptor(spec, &dispatch, &mut cursor, offset, file_len) {
                Ok(index) => index,
                Err(error) => {
                    if let Some(tail) = layout_tail_info(&error, offset, file_len) {
                        return Ok((segments, Some(tail)));
                    }
                    return Err(error);
                }
            };
        let descriptor = dispatch[dispatch_index].descriptor;
        if descriptor.repeat == SegmentRepeat::Once
            && segment_counts
                .iter()
                .any(|count| count.name == descriptor.name && count.count > 0)
        {
            return Err(Error::LayoutRepeatedOnceSegment {
                segment: descriptor.name.to_string(),
                offset,
            });
        }
        let next_index_bytes =
            reserve_layout_segment(spec, &mut segments, descriptor, index_bytes)?;
        let info = match read_layout_segment_at(spec, &mut cursor, descriptor, offset, file_len) {
            Ok(info) => info,
            Err(failure) if failure.recoverable_tail => {
                return Ok((
                    segments,
                    Some(LayoutTailInfo {
                        offset,
                        file_len,
                        kind: LayoutTailKind::InvalidSegmentBounds,
                    }),
                ));
            }
            Err(failure) => {
                if let Some(tail) = layout_tail_info(&failure.error, offset, file_len) {
                    return Ok((segments, Some(tail)));
                }
                return Err(failure.error);
            }
        };
        if info.segment_end <= offset {
            return Err(Error::LayoutInvalidSegmentBounds { offset });
        }
        offset = info.segment_end;
        increment_segment_count(&mut segment_counts, descriptor.name)?;
        segments.push(info);
        index_bytes = next_index_bytes;
    }
    Ok((segments, None))
}

fn check_pending_layout_segment_limits(
    spec: FormatSpec,
    dispatch: &[SegmentDispatch],
    segment_count: usize,
    index_bytes: u64,
) -> Result<()> {
    let prospective_count = u64::try_from(segment_count)
        .map_err(|_| Error::LengthOverflow { value: u64::MAX })?
        .checked_add(1)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "layout segment count",
        })?;
    spec.read_limits
        .check(ReadLimitKey::Segments, prospective_count)?;

    let minimum_entry_bytes = dispatch
        .iter()
        .map(|candidate| layout_segment_resident_bytes(candidate.descriptor))
        .try_fold(None, |minimum, bytes| {
            let bytes = bytes?;
            Ok::<_, Error>(Some(
                minimum.map_or(bytes, |current: u64| current.min(bytes)),
            ))
        })?
        .ok_or(Error::InvalidFormatSpec("missing layout segment"))?;
    let minimum_index_bytes =
        index_bytes
            .checked_add(minimum_entry_bytes)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "layout index bytes",
            })?;
    spec.read_limits
        .check(ReadLimitKey::IndexBytes, minimum_index_bytes)?;
    Ok(())
}

fn reserve_layout_segment(
    spec: FormatSpec,
    segments: &mut Vec<LayoutSegmentInfo>,
    descriptor: SegmentDescriptor,
    index_bytes: u64,
) -> Result<u64> {
    let segment_count = u64::try_from(segments.len())
        .map_err(|_| Error::LengthOverflow { value: u64::MAX })?
        .checked_add(1)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "layout segment count",
        })?;
    spec.read_limits
        .check(ReadLimitKey::Segments, segment_count)?;
    let entry_bytes = layout_segment_resident_bytes(descriptor)?;
    let next_index_bytes =
        index_bytes
            .checked_add(entry_bytes)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "layout index bytes",
            })?;
    spec.read_limits
        .check(ReadLimitKey::IndexBytes, next_index_bytes)?;
    segments
        .try_reserve(1)
        .map_err(|_| Error::AllocationFailed {
            resource: "layout segment index",
            requested: next_index_bytes,
        })?;
    Ok(next_index_bytes)
}

fn increment_segment_count(counts: &mut [SegmentCount], name: &'static str) -> Result<()> {
    let count = counts
        .iter_mut()
        .find(|count| count.name == name)
        .ok_or(Error::InvalidFormatSpec("layout segment count is missing"))?;
    count.count = count
        .count
        .checked_add(1)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "layout segment count",
        })?;
    Ok(())
}

fn layout_segment_resident_bytes(descriptor: SegmentDescriptor) -> Result<u64> {
    let base = u64::try_from(size_of::<LayoutSegmentInfo>())
        .map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
    let lead_in = layout_fields_resident_bytes(descriptor.lead_in.fields)?;
    let footer = descriptor
        .footer
        .map(|footer| layout_fields_resident_bytes(footer.fields))
        .unwrap_or(Ok(0))?;
    base.checked_add(lead_in)
        .and_then(|bytes| bytes.checked_add(footer))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "layout index bytes",
        })
}

fn layout_fields_resident_bytes(descriptors: &[LayoutFieldDescriptor]) -> Result<u64> {
    let field_size = u64::try_from(size_of::<LayoutFieldValue>())
        .map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
    let field_count =
        u64::try_from(descriptors.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
    let values = field_count
        .checked_mul(field_size)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "layout index bytes",
        })?;
    descriptors.iter().try_fold(values, |bytes, descriptor| {
        let owned_bytes = match descriptor.ty {
            LayoutFieldType::Bytes { len } => len,
            _ => 0,
        };
        bytes
            .checked_add(owned_bytes)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "layout index bytes",
            })
    })
}

fn layout_tail_info(error: &Error, default_offset: u64, file_len: u64) -> Option<LayoutTailInfo> {
    let (offset, kind) = match error {
        Error::LayoutTruncatedHeader { offset } => (*offset, LayoutTailKind::TruncatedHeader),
        Error::LayoutTruncatedLeadIn { offset } => (*offset, LayoutTailKind::TruncatedLeadIn),
        _ => return None,
    };
    Some(LayoutTailInfo {
        offset: if offset == 0 { default_offset } else { offset },
        file_len,
        kind,
    })
}

fn segment_dispatch_table(spec: FormatSpec) -> Result<Vec<SegmentDispatch>> {
    segment_descriptors(spec)?
        .into_iter()
        .map(|descriptor| {
            Ok(SegmentDispatch {
                descriptor,
                key: segment_dispatch_key(descriptor, spec.endian)?,
                lead_in_len: descriptor_lead_in_len(descriptor)?,
            })
        })
        .collect()
}

fn select_segment_descriptor(
    spec: FormatSpec,
    dispatch: &[SegmentDispatch],
    cursor: &mut SnapshotCursor,
    offset: u64,
    file_len: u64,
) -> Result<usize> {
    if dispatch.len() == 1 {
        let lead_in_len = dispatch[0].lead_in_len;
        check_layout_scan_extent(spec, offset, lead_in_len)?;
        if file_len - offset < lead_in_len {
            return Err(Error::LayoutTruncatedLeadIn { offset });
        }
        return Ok(0);
    }

    let mut matches = Vec::new();
    matches
        .try_reserve_exact(dispatch.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "layout segment dispatch",
            requested: u64::try_from(dispatch.len()).unwrap_or(u64::MAX),
        })?;
    let mut fallback = None;
    let mut saw_truncated = false;
    for (index, candidate) in dispatch.iter().enumerate() {
        if let Some(key) = &candidate.key {
            let key_len =
                u64::try_from(key.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
            check_layout_scan_extent(spec, offset, key_len)?;
            if file_len - offset < key_len {
                saw_truncated = true;
                continue;
            }
            if bytes_at(cursor, offset, key.len())? == *key {
                check_layout_scan_extent(spec, offset, candidate.lead_in_len)?;
                if file_len - offset < candidate.lead_in_len {
                    return Err(Error::LayoutTruncatedLeadIn { offset });
                }
                matches.push(index);
            }
        } else {
            if fallback.is_some() {
                return Err(Error::InvalidFormatSpec(
                    "layout has multiple fallback segment descriptors",
                ));
            }
            fallback = Some(index);
        }
    }

    match matches.len() {
        1 => Ok(matches[0]),
        n if n > 1 => Err(Error::LayoutAmbiguousSegment { offset }),
        _ => {
            if let Some(index) = fallback {
                check_layout_scan_extent(spec, offset, dispatch[index].lead_in_len)?;
                if file_len - offset < dispatch[index].lead_in_len {
                    return Err(Error::LayoutTruncatedLeadIn { offset });
                }
                Ok(index)
            } else if saw_truncated {
                Err(Error::LayoutTruncatedLeadIn { offset })
            } else {
                Err(Error::LayoutNoMatchingSegment { offset })
            }
        }
    }
}

fn check_layout_scan_extent(spec: FormatSpec, offset: u64, len: u64) -> Result<u64> {
    let end = offset
        .checked_add(len)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "layout scan bytes",
        })?;
    spec.read_limits.check(ReadLimitKey::ScanBytes, end)?;
    Ok(end)
}

fn bytes_at(cursor: &mut SnapshotCursor, offset: u64, len: usize) -> Result<Vec<u8>> {
    let len = u64::try_from(len).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
    cursor.read_vec_at(offset, len, len, "layout dispatch key")
}

fn segment_dispatch_key(
    descriptor: SegmentDescriptor,
    format_endian: Endian,
) -> Result<Option<Vec<u8>>> {
    let mut key = Vec::new();
    for field in descriptor.lead_in.fields {
        let value = match field.source {
            LayoutFieldSource::LiteralBytes(bytes) => LayoutValue::Bytes(bytes.to_vec()),
            LayoutFieldSource::LiteralU64(value) => {
                normalize_layout_value(*field, &LayoutValue::U64(value))?
            }
            LayoutFieldSource::LiteralI64(value) => {
                normalize_layout_value(*field, &LayoutValue::I64(value))?
            }
            LayoutFieldSource::Caller | LayoutFieldSource::Finalize(_) => break,
        };
        write_layout_value(&mut key, *field, &value, format_endian)?;
    }
    Ok((!key.is_empty()).then_some(key))
}

pub(crate) fn validate_layout_segment_dispatch(spec: FormatSpec) -> Result<()> {
    let dispatch = segment_dispatch_table(spec)?;
    let mut fallback_count = 0usize;
    for (index, candidate) in dispatch.iter().enumerate() {
        if candidate.key.is_none() {
            fallback_count += 1;
        }
        for other in &dispatch[(index + 1)..] {
            if candidate.descriptor.name == other.descriptor.name {
                return Err(Error::InvalidFormatSpec("duplicate layout segment name"));
            }
            if dispatch_keys_overlap(candidate.key.as_deref(), other.key.as_deref()) {
                return Err(Error::InvalidFormatSpec(
                    "layout segment dispatch keys are ambiguous",
                ));
            }
        }
    }
    if fallback_count > 1 {
        return Err(Error::InvalidFormatSpec(
            "layout has multiple fallback segment descriptors",
        ));
    }
    Ok(())
}

fn dispatch_keys_overlap(left: Option<&[u8]>, right: Option<&[u8]>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.starts_with(right) || right.starts_with(left),
        (None, None) => true,
        _ => false,
    }
}

fn read_layout_segment_at(
    spec: FormatSpec,
    cursor: &mut SnapshotCursor,
    descriptor: SegmentDescriptor,
    segment_start: u64,
    file_len: u64,
) -> std::result::Result<LayoutSegmentInfo, LayoutSegmentReadError> {
    let lead_in_len = descriptor_lead_in_len(descriptor)?;
    let footer_len = descriptor_footer_len(descriptor)?;
    let after_lead_in = check_layout_scan_extent(spec, segment_start, lead_in_len)?;
    let lead_in_bytes = cursor.read_vec_at(
        segment_start,
        lead_in_len,
        lead_in_len,
        "layout lead-in fields",
    )?;
    let mut position = segment_start;
    let mut relative = 0usize;
    let mut finalized = Vec::new();
    finalized
        .try_reserve_exact(descriptor.lead_in.fields.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "layout finalized fields",
            requested: u64::try_from(descriptor.lead_in.fields.len()).unwrap_or(u64::MAX),
        })?;
    let mut fields = Vec::new();
    fields
        .try_reserve_exact(descriptor.lead_in.fields.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "layout field index",
            requested: layout_fields_resident_bytes(descriptor.lead_in.fields).unwrap_or(u64::MAX),
        })?;
    for field in descriptor.lead_in.fields {
        let value_offset = position;
        let value = read_layout_value_from_slice(&lead_in_bytes, relative, *field, spec.endian)?;
        let stored_value = value.clone();
        match field.source {
            LayoutFieldSource::LiteralBytes(expected) => match value {
                LayoutValue::Bytes(actual) if actual == expected => {}
                _ => {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: value_offset,
                    }
                    .into());
                }
            },
            LayoutFieldSource::LiteralU64(expected) => {
                if !numeric_u64_matches(&value, expected) {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: value_offset,
                    }
                    .into());
                }
            }
            LayoutFieldSource::LiteralI64(expected) => {
                if !matches!(value, LayoutValue::I64(actual) if actual == expected) {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: value_offset,
                    }
                    .into());
                }
            }
            LayoutFieldSource::Caller => {}
            LayoutFieldSource::Finalize(finalize) => finalized.push((*field, finalize, value)),
        }
        fields.push(LayoutFieldValue {
            name: field.name,
            value: stored_value,
        });
        position =
            position
                .checked_add(field.ty.byte_len())
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "layout field offset",
                })?;
        relative = relative
            .checked_add(usize::try_from(field.ty.byte_len()).map_err(|_| {
                Error::LengthOverflow {
                    value: field.ty.byte_len(),
                }
            })?)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "layout field offset",
            })?;
    }

    let metadata_offset = after_lead_in;
    let raw_offset = finalized_target_absolute(
        &finalized,
        LayoutAnchor::RawRegionStart,
        segment_start,
        after_lead_in,
        None,
    )?;
    let segment_end = finalized_target_absolute(
        &finalized,
        LayoutAnchor::SegmentEnd,
        segment_start,
        after_lead_in,
        Some(raw_offset),
    )?;
    let footer_offset =
        segment_end
            .checked_sub(footer_len)
            .ok_or(Error::LayoutInvalidSegmentBounds {
                offset: segment_start,
            })?;
    if raw_offset < metadata_offset || footer_offset < raw_offset || segment_end < footer_offset {
        return Err(Error::LayoutInvalidSegmentBounds {
            offset: segment_start,
        }
        .into());
    }
    let metadata_len = raw_offset - metadata_offset;
    let raw_len = footer_offset - raw_offset;
    let anchors = Anchors {
        segment_start,
        after_lead_in,
        metadata_start: metadata_offset,
        raw_region_start: raw_offset,
        segment_end,
        footer_start: footer_offset,
        footer_end: segment_end,
    };
    for (field, finalize, value) in finalized {
        let expected = anchor_value(finalize.target, anchors)?
            .checked_sub(anchor_value(finalize.relative_to, anchors)?)
            .ok_or(Error::LayoutInvalidSegmentBounds {
                offset: segment_start,
            })?;
        if !layout_value_matches_offset(value, expected) {
            return Err(Error::LayoutLiteralMismatch {
                field: field.name,
                offset: segment_start,
            }
            .into());
        }
    }
    check_layout_scan_extent(spec, 0, segment_end)?;
    if segment_end > file_len {
        return Err(LayoutSegmentReadError::recoverable(
            Error::LayoutInvalidSegmentBounds {
                offset: segment_start,
            },
        ));
    }
    let footer_fields = if let Some(footer) = descriptor.footer {
        read_footer_layout_fields(cursor, footer.fields, footer_offset, spec.endian, anchors)?
    } else {
        Vec::new()
    };
    Ok(LayoutSegmentInfo {
        name: descriptor.name,
        fields,
        footer_fields,
        segment_start,
        lead_in_len,
        metadata_offset,
        metadata_len,
        raw_offset,
        raw_len,
        footer_offset,
        footer_len,
        segment_end,
    })
}

fn read_footer_layout_fields(
    cursor: &mut SnapshotCursor,
    descriptors: &[LayoutFieldDescriptor],
    footer_offset: u64,
    endian: Endian,
    anchors: Anchors,
) -> Result<Vec<LayoutFieldValue>> {
    let footer_len = descriptor_fields_len(descriptors)?;
    let bytes = cursor.read_vec_at(
        footer_offset,
        footer_len,
        footer_len,
        "layout footer fields",
    )?;
    let mut position = footer_offset;
    let mut relative = 0usize;
    let mut fields = Vec::new();
    fields
        .try_reserve_exact(descriptors.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "layout field index",
            requested: layout_fields_resident_bytes(descriptors).unwrap_or(u64::MAX),
        })?;
    for field in descriptors {
        let value = read_layout_value_from_slice(&bytes, relative, *field, endian)?;
        let stored_value = value.clone();
        match field.source {
            LayoutFieldSource::LiteralBytes(expected) => match value {
                LayoutValue::Bytes(actual) if actual == expected => {}
                _ => {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: position,
                    });
                }
            },
            LayoutFieldSource::LiteralU64(expected) => {
                if !numeric_u64_matches(&value, expected) {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: position,
                    });
                }
            }
            LayoutFieldSource::LiteralI64(expected) => {
                if !matches!(value, LayoutValue::I64(actual) if actual == expected) {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: position,
                    });
                }
            }
            LayoutFieldSource::Caller => {}
            LayoutFieldSource::Finalize(_) => {
                let expected = finalized_value(*field, anchors)?;
                if !layout_value_matches_offset_value(value, expected) {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: position,
                    });
                }
            }
        }
        fields.push(LayoutFieldValue {
            name: field.name,
            value: stored_value,
        });
        position =
            position
                .checked_add(field.ty.byte_len())
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "layout footer field offset",
                })?;
        relative = relative
            .checked_add(usize::try_from(field.ty.byte_len()).map_err(|_| {
                Error::LengthOverflow {
                    value: field.ty.byte_len(),
                }
            })?)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "layout footer field offset",
            })?;
    }
    Ok(fields)
}

fn finalized_target_absolute(
    values: &[(LayoutFieldDescriptor, LayoutFinalize, LayoutValue)],
    target: LayoutAnchor,
    segment_start: u64,
    after_lead_in: u64,
    raw_region_start: Option<u64>,
) -> Result<u64> {
    for (_, finalize, value) in values {
        if finalize.target != target {
            continue;
        }
        let Some(relative) = known_scan_anchor_value(
            finalize.relative_to,
            segment_start,
            after_lead_in,
            raw_region_start,
        ) else {
            continue;
        };
        let absolute = i128::from(relative)
            .checked_add(layout_value_to_i128(value)?)
            .ok_or(Error::LayoutInvalidSegmentBounds {
                offset: segment_start,
            })?;
        if absolute < 0 || absolute > i128::from(u64::MAX) {
            return Err(Error::LayoutInvalidSegmentBounds {
                offset: segment_start,
            });
        }
        return Ok(absolute as u64);
    }
    Err(Error::LayoutInvalidSegmentBounds {
        offset: segment_start,
    })
}

fn known_scan_anchor_value(
    anchor: LayoutAnchor,
    segment_start: u64,
    after_lead_in: u64,
    raw_region_start: Option<u64>,
) -> Option<u64> {
    match anchor {
        LayoutAnchor::SegmentStart => Some(segment_start),
        LayoutAnchor::AfterLeadIn | LayoutAnchor::MetadataStart => Some(after_lead_in),
        LayoutAnchor::RawRegionStart => raw_region_start,
        LayoutAnchor::SegmentEnd | LayoutAnchor::FooterStart | LayoutAnchor::FooterEnd => None,
    }
}

fn layout_value_to_i128(value: &LayoutValue) -> Result<i128> {
    match value {
        LayoutValue::U8(value) => Ok(i128::from(*value)),
        LayoutValue::U16(value) => Ok(i128::from(*value)),
        LayoutValue::U32(value) => Ok(i128::from(*value)),
        LayoutValue::U64(value) => Ok(i128::from(*value)),
        LayoutValue::I64(value) => Ok(i128::from(*value)),
        LayoutValue::Bytes(_) => Err(Error::LayoutInvalidSegmentBounds { offset: 0 }),
    }
}

fn numeric_u64_matches(value: &LayoutValue, expected: u64) -> bool {
    match value {
        LayoutValue::U8(actual) => u64::from(*actual) == expected,
        LayoutValue::U16(actual) => u64::from(*actual) == expected,
        LayoutValue::U32(actual) => u64::from(*actual) == expected,
        LayoutValue::U64(actual) => *actual == expected,
        LayoutValue::I64(actual) => u64::try_from(*actual) == Ok(expected),
        LayoutValue::Bytes(_) => false,
    }
}

fn normalize_layout_value(
    field: LayoutFieldDescriptor,
    value: &LayoutValue,
) -> Result<LayoutValue> {
    Ok(match (field.ty, value) {
        (LayoutFieldType::Bytes { len }, LayoutValue::Bytes(bytes))
            if len == bytes.len() as u64 =>
        {
            LayoutValue::Bytes(bytes.clone())
        }
        (LayoutFieldType::U8, LayoutValue::U8(value)) => LayoutValue::U8(*value),
        (LayoutFieldType::U8, LayoutValue::U64(value)) => LayoutValue::U8(
            u8::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field.name))?,
        ),
        (LayoutFieldType::U16, LayoutValue::U8(value)) => LayoutValue::U16(u16::from(*value)),
        (LayoutFieldType::U16, LayoutValue::U16(value)) => LayoutValue::U16(*value),
        (LayoutFieldType::U16, LayoutValue::U64(value)) => LayoutValue::U16(
            u16::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field.name))?,
        ),
        (LayoutFieldType::U32, LayoutValue::U8(value)) => LayoutValue::U32(u32::from(*value)),
        (LayoutFieldType::U32, LayoutValue::U16(value)) => LayoutValue::U32(u32::from(*value)),
        (LayoutFieldType::U32, LayoutValue::U32(value)) => LayoutValue::U32(*value),
        (LayoutFieldType::U32, LayoutValue::U64(value)) => LayoutValue::U32(
            u32::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field.name))?,
        ),
        (LayoutFieldType::U64, LayoutValue::U8(value)) => LayoutValue::U64(u64::from(*value)),
        (LayoutFieldType::U64, LayoutValue::U16(value)) => LayoutValue::U64(u64::from(*value)),
        (LayoutFieldType::U64, LayoutValue::U32(value)) => LayoutValue::U64(u64::from(*value)),
        (LayoutFieldType::U64, LayoutValue::U64(value)) => LayoutValue::U64(*value),
        (LayoutFieldType::I64, LayoutValue::U8(value)) => LayoutValue::I64(i64::from(*value)),
        (LayoutFieldType::I64, LayoutValue::U16(value)) => LayoutValue::I64(i64::from(*value)),
        (LayoutFieldType::I64, LayoutValue::U32(value)) => LayoutValue::I64(i64::from(*value)),
        (LayoutFieldType::I64, LayoutValue::U64(value)) => LayoutValue::I64(
            i64::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field.name))?,
        ),
        (LayoutFieldType::I64, LayoutValue::I64(value)) => LayoutValue::I64(*value),
        _ => return Err(Error::LayoutFieldTypeMismatch(field.name)),
    })
}

fn layout_value_matches_offset(value: LayoutValue, expected: u64) -> bool {
    match value {
        LayoutValue::U64(actual) => actual == expected,
        LayoutValue::I64(actual) => u64::try_from(actual) == Ok(expected),
        _ => numeric_u64_matches(&value, expected),
    }
}

fn layout_value_matches_offset_value(value: LayoutValue, expected: LayoutValue) -> bool {
    match expected {
        LayoutValue::U64(expected) => layout_value_matches_offset(value, expected),
        LayoutValue::I64(expected) => match value {
            LayoutValue::I64(actual) => actual == expected,
            LayoutValue::U64(actual) => i64::try_from(actual) == Ok(expected),
            LayoutValue::U32(actual) => i64::from(actual) == expected,
            LayoutValue::U16(actual) => i64::from(actual) == expected,
            LayoutValue::U8(actual) => i64::from(actual) == expected,
            LayoutValue::Bytes(_) => false,
        },
        _ => value == expected,
    }
}

fn finalized_value(field: LayoutFieldDescriptor, anchors: Anchors) -> Result<LayoutValue> {
    let LayoutFieldSource::Finalize(finalize) = field.source else {
        return Err(Error::InvalidFormatSpec("layout field is not finalized"));
    };
    let target = anchor_value(finalize.target, anchors)?;
    let relative = anchor_value(finalize.relative_to, anchors)?;
    let value = target
        .checked_sub(relative)
        .ok_or(Error::LayoutInvalidSegmentBounds {
            offset: anchors.segment_start,
        })?;
    match field.ty {
        LayoutFieldType::U8 => Ok(LayoutValue::U8(u8::try_from(value).map_err(|_| {
            Error::LayoutInvalidSegmentBounds {
                offset: anchors.segment_start,
            }
        })?)),
        LayoutFieldType::U16 => Ok(LayoutValue::U16(u16::try_from(value).map_err(|_| {
            Error::LayoutInvalidSegmentBounds {
                offset: anchors.segment_start,
            }
        })?)),
        LayoutFieldType::U32 => Ok(LayoutValue::U32(u32::try_from(value).map_err(|_| {
            Error::LayoutInvalidSegmentBounds {
                offset: anchors.segment_start,
            }
        })?)),
        LayoutFieldType::U64 => Ok(LayoutValue::U64(value)),
        LayoutFieldType::I64 => Ok(LayoutValue::I64(i64::try_from(value).map_err(|_| {
            Error::LayoutInvalidSegmentBounds {
                offset: anchors.segment_start,
            }
        })?)),
        _ => Err(Error::LayoutFieldTypeMismatch(field.name)),
    }
}

fn anchor_value(anchor: LayoutAnchor, anchors: Anchors) -> Result<u64> {
    Ok(match anchor {
        LayoutAnchor::SegmentStart => anchors.segment_start,
        LayoutAnchor::AfterLeadIn => anchors.after_lead_in,
        LayoutAnchor::MetadataStart => anchors.metadata_start,
        LayoutAnchor::RawRegionStart => anchors.raw_region_start,
        LayoutAnchor::SegmentEnd => anchors.segment_end,
        LayoutAnchor::FooterStart => anchors.footer_start,
        LayoutAnchor::FooterEnd => anchors.footer_end,
    })
}

fn write_layout_value<W: Write>(
    writer: &mut W,
    field: LayoutFieldDescriptor,
    value: &LayoutValue,
    format_endian: Endian,
) -> Result<()> {
    match (&field.ty, value) {
        (LayoutFieldType::Bytes { len }, LayoutValue::Bytes(bytes))
            if *len == bytes.len() as u64 =>
        {
            writer.write_all(bytes)?;
            Ok(())
        }
        (LayoutFieldType::U8, LayoutValue::U8(value)) => {
            writer.write_all(&[*value])?;
            Ok(())
        }
        _ => write_numeric(writer, field, value, format_endian),
    }
}

fn write_numeric<W: Write>(
    writer: &mut W,
    field: LayoutFieldDescriptor,
    value: &LayoutValue,
    format_endian: Endian,
) -> Result<()> {
    let endian = field.endian.unwrap_or(format_endian);
    match (field.ty, value) {
        (LayoutFieldType::U8, LayoutValue::U64(value)) => {
            let value =
                u8::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field.name))?;
            writer.write_all(&[value])?;
            Ok(())
        }
        (LayoutFieldType::U16, LayoutValue::U16(value)) => write_u16(writer, *value, endian),
        (LayoutFieldType::U16, LayoutValue::U64(value)) => {
            let value =
                u16::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field.name))?;
            write_u16(writer, value, endian)
        }
        (LayoutFieldType::U32, LayoutValue::U32(value)) => write_u32(writer, *value, endian),
        (LayoutFieldType::U32, LayoutValue::U64(value)) => {
            let value =
                u32::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field.name))?;
            write_u32(writer, value, endian)
        }
        (LayoutFieldType::U64, LayoutValue::U64(value)) => write_u64(writer, *value, endian),
        (LayoutFieldType::I64, LayoutValue::I64(value)) => write_i64(writer, *value, endian),
        (LayoutFieldType::U16, LayoutValue::U8(value)) => {
            write_u16(writer, u16::from(*value), endian)
        }
        (LayoutFieldType::U32, LayoutValue::U8(value)) => {
            write_u32(writer, u32::from(*value), endian)
        }
        (LayoutFieldType::U32, LayoutValue::U16(value)) => {
            write_u32(writer, u32::from(*value), endian)
        }
        (LayoutFieldType::U64, LayoutValue::U8(value)) => {
            write_u64(writer, u64::from(*value), endian)
        }
        (LayoutFieldType::U64, LayoutValue::U16(value)) => {
            write_u64(writer, u64::from(*value), endian)
        }
        (LayoutFieldType::U64, LayoutValue::U32(value)) => {
            write_u64(writer, u64::from(*value), endian)
        }
        (LayoutFieldType::I64, LayoutValue::U8(value)) => {
            write_i64(writer, i64::from(*value), endian)
        }
        (LayoutFieldType::I64, LayoutValue::U16(value)) => {
            write_i64(writer, i64::from(*value), endian)
        }
        (LayoutFieldType::I64, LayoutValue::U32(value)) => {
            write_i64(writer, i64::from(*value), endian)
        }
        (LayoutFieldType::I64, LayoutValue::U64(value)) => {
            let value =
                i64::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field.name))?;
            write_i64(writer, value, endian)
        }
        _ => Err(Error::LayoutFieldTypeMismatch(field.name)),
    }
}

fn read_layout_value_from_slice(
    bytes: &[u8],
    offset: usize,
    field: LayoutFieldDescriptor,
    format_endian: Endian,
) -> Result<LayoutValue> {
    let endian = field.endian.unwrap_or(format_endian);
    let field_len = usize::try_from(field.ty.byte_len()).map_err(|_| Error::LengthOverflow {
        value: field.ty.byte_len(),
    })?;
    let end = offset
        .checked_add(field_len)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "layout field range",
        })?;
    let field_bytes = bytes.get(offset..end).ok_or(Error::UnexpectedEof)?;
    Ok(match field.ty {
        LayoutFieldType::Bytes { len } => {
            let mut value = Vec::new();
            value
                .try_reserve_exact(field_len)
                .map_err(|_| Error::AllocationFailed {
                    resource: "layout field bytes",
                    requested: len,
                })?;
            value.extend_from_slice(field_bytes);
            LayoutValue::Bytes(value)
        }
        LayoutFieldType::U8 => LayoutValue::U8(field_bytes[0]),
        LayoutFieldType::U16 => {
            let bytes = field_bytes.try_into().map_err(|_| Error::UnexpectedEof)?;
            LayoutValue::U16(match endian {
                Endian::Little => u16::from_le_bytes(bytes),
                Endian::Big => u16::from_be_bytes(bytes),
            })
        }
        LayoutFieldType::U32 => {
            let bytes = field_bytes.try_into().map_err(|_| Error::UnexpectedEof)?;
            LayoutValue::U32(match endian {
                Endian::Little => u32::from_le_bytes(bytes),
                Endian::Big => u32::from_be_bytes(bytes),
            })
        }
        LayoutFieldType::U64 => {
            let bytes = field_bytes.try_into().map_err(|_| Error::UnexpectedEof)?;
            LayoutValue::U64(match endian {
                Endian::Little => u64::from_le_bytes(bytes),
                Endian::Big => u64::from_be_bytes(bytes),
            })
        }
        LayoutFieldType::I64 => {
            let bytes = field_bytes.try_into().map_err(|_| Error::UnexpectedEof)?;
            LayoutValue::I64(match endian {
                Endian::Little => i64::from_le_bytes(bytes),
                Endian::Big => i64::from_be_bytes(bytes),
            })
        }
    })
}

fn write_zeroes(file: &mut File, len: u64) -> Result<()> {
    let zeroes = [0; 64];
    let mut written = 0u64;
    while written < len {
        let chunk_len = usize::try_from((len - written).min(zeroes.len() as u64))
            .map_err(|_| Error::LengthOverflow { value: len })?;
        file.write_all(&zeroes[..chunk_len])?;
        written = written
            .checked_add(u64::try_from(chunk_len).unwrap_or(u64::MAX))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "layout zero-fill length",
            })?;
    }
    Ok(())
}

fn write_u16<W: Write>(writer: &mut W, value: u16, endian: Endian) -> Result<()> {
    let bytes = match endian {
        Endian::Little => value.to_le_bytes(),
        Endian::Big => value.to_be_bytes(),
    };
    writer.write_all(&bytes)?;
    Ok(())
}

fn write_u32<W: Write>(writer: &mut W, value: u32, endian: Endian) -> Result<()> {
    let bytes = match endian {
        Endian::Little => value.to_le_bytes(),
        Endian::Big => value.to_be_bytes(),
    };
    writer.write_all(&bytes)?;
    Ok(())
}

fn write_u64<W: Write>(writer: &mut W, value: u64, endian: Endian) -> Result<()> {
    let bytes = match endian {
        Endian::Little => value.to_le_bytes(),
        Endian::Big => value.to_be_bytes(),
    };
    writer.write_all(&bytes)?;
    Ok(())
}

fn write_i64<W: Write>(writer: &mut W, value: i64, endian: Endian) -> Result<()> {
    let bytes = match endian {
        Endian::Little => value.to_le_bytes(),
        Endian::Big => value.to_be_bytes(),
    };
    writer.write_all(&bytes)?;
    Ok(())
}
