use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{
    Endian, Error, FileHeaderDescriptor, FormatSpec, LayoutAnchor, LayoutFieldDescriptor,
    LayoutFieldSource, LayoutFieldType, LayoutFinalize, LayoutPartKind, LayoutPlan, LayoutPreset,
    Result, SegmentDescriptor, SegmentRepeat,
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
    _lock: crate::file::WriterLock,
}

#[derive(Debug)]
pub struct LayoutReader {
    spec: FormatSpec,
    path: PathBuf,
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

impl FormatSpec {
    pub fn create_layout_writer<P: AsRef<Path>>(self, path: P) -> Result<LayoutWriter> {
        self.validate()?;
        LayoutWriter::create(self, path)
    }

    pub fn create_layout_writer_with_header<P: AsRef<Path>>(
        self,
        path: P,
        fields: &[LayoutFieldValue],
    ) -> Result<LayoutWriter> {
        self.validate()?;
        LayoutWriter::create_with_header(self, path, fields)
    }

    pub fn open_layout_writer<P: AsRef<Path>>(self, path: P) -> Result<LayoutWriter> {
        self.validate()?;
        LayoutWriter::open(self, path)
    }

    pub fn open_layout_reader<P: AsRef<Path>>(self, path: P) -> Result<LayoutReader> {
        self.validate()?;
        LayoutReader::open(self, path)
    }

    pub fn inspect_layout_file<P: AsRef<Path>>(self, path: P) -> Result<LayoutFileInfo> {
        self.validate()?;
        if self.layout.is_varve_native_default() {
            inspect_native_layout_file(self, path)
        } else {
            let reader = LayoutReader::open(self, path)?;
            Ok(LayoutFileInfo {
                plan: self.effective_layout(),
                file_header_len: reader.file_header_len,
                file_header_fields: reader.file_header_fields,
                segments: reader.segments,
            })
        }
    }
}

impl LayoutWriter {
    pub fn create<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Self::create_with_header(spec, path, &[])
    }

    pub fn create_with_header<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        fields: &[LayoutFieldValue],
    ) -> Result<Self> {
        ensure_custom_layout_spec(spec)?;
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
        } else if !fields.is_empty() {
            return Err(Error::LayoutFieldUnexpected(fields[0].name.to_string()));
        }
        Ok(Self {
            spec,
            path,
            file,
            segment_counts: initial_segment_counts(spec)?,
            _lock: lock,
        })
    }

    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        ensure_custom_layout_spec(spec)?;
        let path = path.as_ref().to_path_buf();
        let lock = crate::file::WriterLock::acquire(&path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let (file_header_len, _) = read_file_header(spec, &mut file)?;
        let segments = scan_layout_segments(spec, &mut file, file_header_len)?;
        let segment_counts = segment_counts_from_infos(spec, &segments)?;
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            spec,
            path,
            file,
            segment_counts,
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn spec(&self) -> FormatSpec {
        self.spec
    }

    pub fn write_segment(&mut self, segment: SegmentWrite<'_>) -> Result<LayoutSegmentInfo> {
        let descriptor = segment_descriptor(self.spec, segment.name)?;
        self.ensure_segment_can_write(descriptor)?;
        ensure_no_unexpected_fields_for(descriptor.lead_in.fields, segment.fields)?;
        let footer_fields = descriptor.footer.map(|footer| footer.fields).unwrap_or(&[]);
        ensure_no_unexpected_fields_for(footer_fields, segment.footer_fields)?;

        let segment_start = self.file.seek(SeekFrom::End(0))?;
        let mut patches = Vec::new();
        write_patchable_layout_fields(
            &mut self.file,
            descriptor.lead_in.fields,
            segment.fields,
            self.spec.endian,
            &mut patches,
        )?;

        let lead_in_len = descriptor_lead_in_len(descriptor)?;
        let after_lead_in =
            segment_start
                .checked_add(lead_in_len)
                .ok_or(Error::LayoutInvalidSegmentBounds {
                    offset: segment_start,
                })?;
        let metadata_offset = after_lead_in;
        self.file.write_all(segment.metadata)?;
        let metadata_len = segment.metadata.len() as u64;
        let raw_offset =
            metadata_offset
                .checked_add(metadata_len)
                .ok_or(Error::LayoutInvalidSegmentBounds {
                    offset: segment_start,
                })?;
        self.file.write_all(segment.raw)?;
        let raw_len = segment.raw.len() as u64;
        let footer_offset =
            raw_offset
                .checked_add(raw_len)
                .ok_or(Error::LayoutInvalidSegmentBounds {
                    offset: segment_start,
                })?;
        if let Some(footer) = descriptor.footer {
            write_patchable_layout_fields(
                &mut self.file,
                footer.fields,
                segment.footer_fields,
                self.spec.endian,
                &mut patches,
            )?;
        }
        let footer_len = descriptor_footer_len(descriptor)?;
        let footer_end =
            footer_offset
                .checked_add(footer_len)
                .ok_or(Error::LayoutInvalidSegmentBounds {
                    offset: segment_start,
                })?;
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

        let info = LayoutSegmentInfo {
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
        };
        self.increment_segment_count(descriptor.name);
        Ok(info)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
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
        Ok(())
    }

    fn increment_segment_count(&mut self, name: &'static str) {
        if let Some(count) = self
            .segment_counts
            .iter_mut()
            .find(|count| count.name == name)
        {
            count.count += 1;
        }
    }
}

impl LayoutReader {
    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        ensure_custom_layout_spec(spec)?;
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new().read(true).open(&path)?;
        let (file_header_len, file_header_fields) = read_file_header(spec, &mut file)?;
        let segments = scan_layout_segments(spec, &mut file, file_header_len)?;
        Ok(Self {
            spec,
            path,
            file_header_len,
            file_header_fields,
            segments,
        })
    }

    pub fn spec(&self) -> FormatSpec {
        self.spec
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
        read_range(&self.path, segment.metadata_offset, segment.metadata_len)
    }

    pub fn read_raw(&self, index: usize) -> Result<Vec<u8>> {
        let segment = self
            .segments
            .get(index)
            .ok_or(Error::LayoutInvalidSegmentBounds { offset: 0 })?;
        read_range(&self.path, segment.raw_offset, segment.raw_len)
    }
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
        .collect();
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

fn native_record_to_layout_segment(entry: &crate::file::RecordIndexEntry) -> LayoutSegmentInfo {
    let footer_offset = entry
        .footer_offset
        .unwrap_or(entry.payload_offset + entry.payload_len);
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
    LayoutSegmentInfo {
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
        segment_end: entry.physical_end(),
    }
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
        if let Some(count) = counts.iter_mut().find(|count| count.name == segment.name) {
            count.count += 1;
        }
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

fn read_file_header(spec: FormatSpec, file: &mut File) -> Result<(u64, Vec<LayoutFieldValue>)> {
    let Some(header) = spec.layout.file_header() else {
        return Ok((0, Vec::new()));
    };
    let header_len = file_header_len(header)?;
    let file_len = file.metadata()?.len();
    if file_len < header_len {
        return Err(Error::LayoutTruncatedHeader { offset: 0 });
    }
    let fields = read_static_layout_fields(file, header.fields, 0, spec.endian)?;
    Ok((header_len, fields))
}

fn file_header_len(header: FileHeaderDescriptor) -> Result<u64> {
    descriptor_fields_len(header.fields)
}

fn read_static_layout_fields(
    file: &mut File,
    descriptors: &[LayoutFieldDescriptor],
    start_offset: u64,
    endian: Endian,
) -> Result<Vec<LayoutFieldValue>> {
    file.seek(SeekFrom::Start(start_offset))?;
    let mut position = start_offset;
    let mut fields = Vec::new();
    for field in descriptors {
        let value = read_layout_value(file, *field, endian)?;
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
                .ok_or(Error::LayoutInvalidSegmentBounds {
                    offset: start_offset,
                })?;
    }
    Ok(fields)
}

fn scan_layout_segments(
    spec: FormatSpec,
    file: &mut File,
    start_offset: u64,
) -> Result<Vec<LayoutSegmentInfo>> {
    let dispatch = segment_dispatch_table(spec)?;
    let file_len = file.metadata()?.len();
    let mut offset = start_offset;
    let mut segments = Vec::new();
    let mut segment_counts = initial_segment_counts(spec)?;
    while offset < file_len {
        let dispatch_index = select_segment_descriptor(&dispatch, file, offset, file_len)?;
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
        let info = read_layout_segment_at(spec, file, descriptor, offset, file_len)?;
        if info.segment_end <= offset {
            return Err(Error::LayoutInvalidSegmentBounds { offset });
        }
        offset = info.segment_end;
        if let Some(count) = segment_counts
            .iter_mut()
            .find(|count| count.name == descriptor.name)
        {
            count.count += 1;
        }
        segments.push(info);
    }
    if offset != file_len {
        return Err(Error::LayoutInvalidSegmentBounds { offset });
    }
    Ok(segments)
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
    dispatch: &[SegmentDispatch],
    file: &mut File,
    offset: u64,
    file_len: u64,
) -> Result<usize> {
    if dispatch.len() == 1 {
        let lead_in_len = dispatch[0].lead_in_len;
        if file_len - offset < lead_in_len {
            return Err(Error::LayoutTruncatedLeadIn { offset });
        }
        return Ok(0);
    }

    let mut matches = Vec::new();
    let mut fallback = None;
    let mut saw_truncated = false;
    for (index, candidate) in dispatch.iter().enumerate() {
        if let Some(key) = &candidate.key {
            if file_len - offset < key.len() as u64 {
                saw_truncated = true;
                continue;
            }
            if bytes_at(file, offset, key.len())? == *key {
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

fn bytes_at(file: &mut File, offset: u64, len: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
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
    file: &mut File,
    descriptor: SegmentDescriptor,
    segment_start: u64,
    file_len: u64,
) -> Result<LayoutSegmentInfo> {
    let lead_in_len = descriptor_lead_in_len(descriptor)?;
    let footer_len = descriptor_footer_len(descriptor)?;
    let after_lead_in =
        segment_start
            .checked_add(lead_in_len)
            .ok_or(Error::LayoutInvalidSegmentBounds {
                offset: segment_start,
            })?;
    file.seek(SeekFrom::Start(segment_start))?;
    let mut position = segment_start;
    let mut finalized = Vec::new();
    let mut fields = Vec::new();
    for field in descriptor.lead_in.fields {
        let value_offset = position;
        let value = read_layout_value(file, *field, spec.endian)?;
        let stored_value = value.clone();
        match field.source {
            LayoutFieldSource::LiteralBytes(expected) => match value {
                LayoutValue::Bytes(actual) if actual == expected => {}
                _ => {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: value_offset,
                    });
                }
            },
            LayoutFieldSource::LiteralU64(expected) => {
                if !numeric_u64_matches(&value, expected) {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: value_offset,
                    });
                }
            }
            LayoutFieldSource::LiteralI64(expected) => {
                if !matches!(value, LayoutValue::I64(actual) if actual == expected) {
                    return Err(Error::LayoutLiteralMismatch {
                        field: field.name,
                        offset: value_offset,
                    });
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
                .ok_or(Error::LayoutInvalidSegmentBounds {
                    offset: segment_start,
                })?;
    }

    let raw_rel = finalized_value_for(
        &finalized,
        LayoutAnchor::RawRegionStart,
        LayoutAnchor::AfterLeadIn,
        segment_start,
    )?;
    let end_rel = finalized_value_for(
        &finalized,
        LayoutAnchor::SegmentEnd,
        LayoutAnchor::AfterLeadIn,
        segment_start,
    )?;
    if raw_rel < 0 || end_rel < 0 || raw_rel > end_rel {
        return Err(Error::LayoutInvalidSegmentBounds {
            offset: segment_start,
        });
    }
    let raw_rel = u64::try_from(raw_rel).map_err(|_| Error::LayoutInvalidSegmentBounds {
        offset: segment_start,
    })?;
    let end_rel = u64::try_from(end_rel).map_err(|_| Error::LayoutInvalidSegmentBounds {
        offset: segment_start,
    })?;
    let metadata_offset = after_lead_in;
    let raw_offset =
        after_lead_in
            .checked_add(raw_rel)
            .ok_or(Error::LayoutInvalidSegmentBounds {
                offset: segment_start,
            })?;
    let segment_end =
        after_lead_in
            .checked_add(end_rel)
            .ok_or(Error::LayoutInvalidSegmentBounds {
                offset: segment_start,
            })?;
    let footer_offset =
        segment_end
            .checked_sub(footer_len)
            .ok_or(Error::LayoutInvalidSegmentBounds {
                offset: segment_start,
            })?;
    if raw_offset < metadata_offset
        || footer_offset < raw_offset
        || segment_end < footer_offset
        || segment_end > file_len
    {
        return Err(Error::LayoutInvalidSegmentBounds {
            offset: segment_start,
        });
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
            });
        }
    }
    let footer_fields = if let Some(footer) = descriptor.footer {
        read_footer_layout_fields(file, footer.fields, footer_offset, spec.endian, anchors)?
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
    file: &mut File,
    descriptors: &[LayoutFieldDescriptor],
    footer_offset: u64,
    endian: Endian,
    anchors: Anchors,
) -> Result<Vec<LayoutFieldValue>> {
    file.seek(SeekFrom::Start(footer_offset))?;
    let mut position = footer_offset;
    let mut fields = Vec::new();
    for field in descriptors {
        let value = read_layout_value(file, *field, endian)?;
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
                .ok_or(Error::LayoutInvalidSegmentBounds {
                    offset: footer_offset,
                })?;
    }
    Ok(fields)
}

fn finalized_value_for(
    values: &[(LayoutFieldDescriptor, LayoutFinalize, LayoutValue)],
    target: LayoutAnchor,
    relative_to: LayoutAnchor,
    segment_start: u64,
) -> Result<i128> {
    values
        .iter()
        .find_map(|(_, finalize, value)| {
            if finalize.target == target && finalize.relative_to == relative_to {
                Some(layout_value_to_i128(value))
            } else {
                None
            }
        })
        .transpose()?
        .ok_or(Error::LayoutInvalidSegmentBounds {
            offset: segment_start,
        })
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

fn read_layout_value(
    file: &mut File,
    field: LayoutFieldDescriptor,
    format_endian: Endian,
) -> Result<LayoutValue> {
    let endian = field.endian.unwrap_or(format_endian);
    Ok(match field.ty {
        LayoutFieldType::Bytes { len } => {
            let len = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
            let mut bytes = vec![0; len];
            file.read_exact(&mut bytes)?;
            LayoutValue::Bytes(bytes)
        }
        LayoutFieldType::U8 => {
            let mut bytes = [0; 1];
            file.read_exact(&mut bytes)?;
            LayoutValue::U8(bytes[0])
        }
        LayoutFieldType::U16 => LayoutValue::U16(read_u16(file, endian)?),
        LayoutFieldType::U32 => LayoutValue::U32(read_u32(file, endian)?),
        LayoutFieldType::U64 => LayoutValue::U64(read_u64(file, endian)?),
        LayoutFieldType::I64 => LayoutValue::I64(read_i64(file, endian)?),
    })
}

fn write_zeroes(file: &mut File, len: u64) -> Result<()> {
    let len = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
    file.write_all(&vec![0; len])?;
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

fn read_u16(file: &mut File, endian: Endian) -> Result<u16> {
    let mut bytes = [0; 2];
    file.read_exact(&mut bytes)?;
    Ok(match endian {
        Endian::Little => u16::from_le_bytes(bytes),
        Endian::Big => u16::from_be_bytes(bytes),
    })
}

fn read_u32(file: &mut File, endian: Endian) -> Result<u32> {
    let mut bytes = [0; 4];
    file.read_exact(&mut bytes)?;
    Ok(match endian {
        Endian::Little => u32::from_le_bytes(bytes),
        Endian::Big => u32::from_be_bytes(bytes),
    })
}

fn read_u64(file: &mut File, endian: Endian) -> Result<u64> {
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes)?;
    Ok(match endian {
        Endian::Little => u64::from_le_bytes(bytes),
        Endian::Big => u64::from_be_bytes(bytes),
    })
}

fn read_i64(file: &mut File, endian: Endian) -> Result<i64> {
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes)?;
    Ok(match endian {
        Endian::Little => i64::from_le_bytes(bytes),
        Endian::Big => i64::from_be_bytes(bytes),
    })
}

fn read_range(path: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
