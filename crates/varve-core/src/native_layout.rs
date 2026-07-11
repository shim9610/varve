use std::io::{Read, Write};

use crate::{
    CompressionHeaderMode, CompressionPolicy, Endian, Error, FormatSpec, LayoutAnchor,
    LayoutFinalize, LayoutPlanField, LayoutPlanFieldSource, LayoutPlanFieldType, LayoutPlanLen,
    Result,
    file::{RecordFooterFields, RecordHeaderFields},
    layout::LayoutValue,
};

const CONTAINER_MARKER_V1: &[u8; 6] = b"VARVE1";
const CONTAINER_MARKER_V2: &[u8; 6] = b"VARVE2";
const CONTAINER_MARKER_V3: &[u8; 6] = b"VARVE3";
const FILE_HEADER_FIXED_LEN: u64 = 6 + 2 + 1 + 1 + 8;
const FILE_EXPLICIT_COMPRESSION_HEADER_LEN: u64 = 28;
const INTERNAL_PREFIX_LEN: usize = 4 + 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeFieldSource {
    LiteralBytes(&'static [u8]),
    LiteralU64(u64),
    Caller,
    Native(&'static str),
    Finalize(LayoutFinalize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NativeField {
    name: &'static str,
    ty: NativeFieldType,
    source: NativeFieldSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeFieldType {
    Bytes { len: u64 },
    U8,
    U16,
    U32,
    U64,
}

impl NativeFieldType {
    const fn byte_len(self) -> u64 {
        match self {
            Self::Bytes { len } => len,
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
            Self::U64 => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DecodedRecordHeader {
    pub(crate) fields: RecordHeaderFields,
    pub(crate) lead_in_len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DecodedFileHeader {
    pub(crate) schema_hash: u64,
    pub(crate) extensions: Vec<u8>,
    pub(crate) has_extension_len: bool,
    pub(crate) header_len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeFileHeaderField {
    Magic,
    ContainerMarker,
    FormatVersion,
    Endian,
    Flags,
    SchemaHash,
    ExtensionLen,
    Extensions,
}

const RECORD_HEADER_FIELDS: &[NativeField] = &[
    NativeField {
        name: "block_id",
        ty: NativeFieldType::U32,
        source: NativeFieldSource::Caller,
    },
    NativeField {
        name: "block_version",
        ty: NativeFieldType::U16,
        source: NativeFieldSource::Caller,
    },
    NativeField {
        name: "flags",
        ty: NativeFieldType::U16,
        source: NativeFieldSource::Native("record_flags"),
    },
    NativeField {
        name: "sequence",
        ty: NativeFieldType::U64,
        source: NativeFieldSource::Native("sequence"),
    },
    NativeField {
        name: "payload_len",
        ty: NativeFieldType::U64,
        source: NativeFieldSource::Finalize(LayoutFinalize {
            target: LayoutAnchor::FooterStart,
            relative_to: LayoutAnchor::RawRegionStart,
        }),
    },
    NativeField {
        name: "checksum",
        ty: NativeFieldType::U32,
        source: NativeFieldSource::Native("checksum"),
    },
    NativeField {
        name: "uncompressed_len_hint",
        ty: NativeFieldType::U32,
        source: NativeFieldSource::Native("uncompressed_len_hint"),
    },
];

const RECORD_FOOTER_FIELDS: &[NativeField] = &[
    NativeField {
        name: "magic",
        ty: NativeFieldType::Bytes { len: 4 },
        source: NativeFieldSource::LiteralBytes(crate::file::RECORD_FOOTER_MAGIC),
    },
    NativeField {
        name: "footer_version",
        ty: NativeFieldType::U16,
        source: NativeFieldSource::LiteralU64(crate::file::RECORD_FOOTER_VERSION as u64),
    },
    NativeField {
        name: "footer_flags",
        ty: NativeFieldType::U16,
        source: NativeFieldSource::Native("footer_flags"),
    },
    NativeField {
        name: "prev_same_block_offset",
        ty: NativeFieldType::U64,
        source: NativeFieldSource::Native("prev_same_block_offset"),
    },
    NativeField {
        name: "prev_same_key_offset",
        ty: NativeFieldType::U64,
        source: NativeFieldSource::Native("prev_same_key_offset"),
    },
    NativeField {
        name: "footer_crc32",
        ty: NativeFieldType::U32,
        source: NativeFieldSource::LiteralU64(0),
    },
    NativeField {
        name: "reserved",
        ty: NativeFieldType::U32,
        source: NativeFieldSource::LiteralU64(0),
    },
];

pub(crate) fn native_file_header_plan_fields(spec: FormatSpec) -> Vec<LayoutPlanField> {
    let extension_len = native_file_header_plan_extension_len(spec);
    let mut fields = Vec::new();
    visit_native_file_header_fields(spec, extension_len, |field| {
        fields.push(native_file_header_field_to_plan(spec, field, extension_len));
        Ok(())
    })
    .expect("native file-header field visitor cannot fail while collecting plan fields");
    fields
}

pub(crate) fn native_file_header_len(spec: FormatSpec, extension_len: u64) -> u64 {
    checked_native_file_header_len(spec, extension_len)
        .expect("native file-header length is constrained to u32 extensions")
}

fn checked_native_file_header_len(spec: FormatSpec, extension_len: u64) -> Result<u64> {
    let ext_len_field = if extension_len == 0 && !spec.spec_needs_record_footer() {
        0
    } else {
        4
    };
    let magic_len =
        u64::try_from(spec.magic.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
    magic_len
        .checked_add(FILE_HEADER_FIXED_LEN)
        .and_then(|len| len.checked_add(ext_len_field))
        .and_then(|len| len.checked_add(extension_len))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "native file-header length",
        })
}

pub(crate) fn write_native_file_header<W: Write>(
    writer: &mut W,
    spec: FormatSpec,
    extensions: &[u8],
) -> Result<u64> {
    let extension_len =
        u64::try_from(extensions.len()).map_err(|_| Error::InvalidCompressionHeader)?;
    let marker = native_container_marker_for_extensions(spec, extension_len);
    visit_native_file_header_fields(spec, extension_len, |field| {
        let value = native_file_header_write_value(spec, field, marker, extensions)?;
        write_native_value(
            writer,
            native_file_header_field(spec, field, extension_len),
            &value,
        )
    })?;
    checked_native_file_header_len(spec, extension_len)
}

pub(crate) fn read_native_file_header<R: Read>(
    reader: &mut R,
    spec: FormatSpec,
) -> Result<DecodedFileHeader> {
    let magic = read_native_value(
        reader,
        native_file_header_field(spec, NativeFileHeaderField::Magic, 0),
    )?;
    match magic {
        LayoutValue::Bytes(actual) if actual == spec.magic => {}
        _ => return Err(Error::InvalidMagic),
    }

    let marker_value = read_native_value(
        reader,
        native_file_header_field(spec, NativeFileHeaderField::ContainerMarker, 0),
    )?;
    let marker = match marker_value {
        LayoutValue::Bytes(actual) => native_marker_from_bytes(&actual)?,
        _ => return Err(Error::UnsupportedContainer),
    };
    if spec.spec_needs_record_footer() != (marker == *CONTAINER_MARKER_V3) {
        return Err(Error::UnsupportedContainer);
    }

    let version = read_native_value(
        reader,
        native_file_header_field(spec, NativeFileHeaderField::FormatVersion, 0),
    )?;
    let version = value_as_u16("format_version", &version)?;
    if version != spec.version {
        return Err(Error::FormatVersionMismatch {
            expected: spec.version,
            actual: version,
        });
    }

    let endian = read_native_value(
        reader,
        native_file_header_field(spec, NativeFileHeaderField::Endian, 0),
    )?;
    let endian = value_as_u8("endian", &endian)?;
    let endian = Endian::from_byte(endian).ok_or(Error::UnsupportedEndian(endian))?;
    if endian != spec.endian {
        return Err(Error::EndianMismatch {
            expected: spec.endian,
            actual: endian,
        });
    }

    let flags = read_native_value(
        reader,
        native_file_header_field(spec, NativeFileHeaderField::Flags, 0),
    )?;
    if value_as_u8("flags", &flags)? != 0 {
        return Err(Error::InvalidCanonicalEncoding(
            "native file-header reserved flags must be zero",
        ));
    }

    let hash = read_native_value(
        reader,
        native_file_header_field(spec, NativeFileHeaderField::SchemaHash, 0),
    )?;
    let schema_hash = value_as_u64("schema_hash", &hash)?;

    let has_extension_len = marker == *CONTAINER_MARKER_V2 || marker == *CONTAINER_MARKER_V3;
    let extensions = if has_extension_len {
        let extension_len = read_native_value(
            reader,
            native_file_header_field(spec, NativeFileHeaderField::ExtensionLen, 0),
        )?;
        let extension_len = value_as_u32("extension_len", &extension_len)?;
        let extension_len = u64::from(extension_len);
        if extension_len != native_file_header_plan_extension_len(spec) {
            return Err(Error::InvalidCompressionHeader);
        }
        match read_native_value(
            reader,
            native_file_header_field(spec, NativeFileHeaderField::Extensions, extension_len),
        )? {
            LayoutValue::Bytes(bytes) => bytes,
            _ => return Err(Error::InvalidCompressionHeader),
        }
    } else {
        Vec::new()
    };
    let extension_len =
        u64::try_from(extensions.len()).map_err(|_| Error::InvalidCompressionHeader)?;
    Ok(DecodedFileHeader {
        schema_hash,
        extensions,
        has_extension_len,
        header_len: checked_native_file_header_len(spec, extension_len)?,
    })
}

pub(crate) fn native_record_header_plan_fields() -> Vec<LayoutPlanField> {
    native_fields_to_plan(RECORD_HEADER_FIELDS)
}

pub(crate) fn native_record_footer_plan_fields() -> Vec<LayoutPlanField> {
    native_fields_to_plan(RECORD_FOOTER_FIELDS)
}

pub(crate) fn native_record_header_len() -> u64 {
    native_fields_len(RECORD_HEADER_FIELDS)
}

pub(crate) fn native_record_footer_len() -> u64 {
    native_fields_len(RECORD_FOOTER_FIELDS)
}

pub(crate) fn ensure_native_record_layout_contract() -> Result<()> {
    if native_record_header_len() != crate::file::RECORD_HEADER_LEN {
        return Err(Error::InvalidFormatSpec(
            "native record header layout length mismatch",
        ));
    }
    if native_record_footer_len() != crate::file::RECORD_FOOTER_LEN {
        return Err(Error::InvalidFormatSpec(
            "native record footer layout length mismatch",
        ));
    }
    Ok(())
}

pub(crate) fn ensure_native_file_header_layout_contract(spec: FormatSpec) -> Result<()> {
    let plan_extension_len = native_file_header_plan_extension_len(spec);
    let mut visited_len = 0u64;
    visit_native_file_header_fields(spec, plan_extension_len, |field| {
        visited_len = visited_len
            .checked_add(
                native_file_header_field(spec, field, plan_extension_len)
                    .ty
                    .byte_len(),
            )
            .ok_or(Error::InvalidFormatSpec(
                "native file header layout length overflow",
            ))?;
        Ok(())
    })?;
    if visited_len != checked_native_file_header_len(spec, plan_extension_len)? {
        return Err(Error::InvalidFormatSpec(
            "native file header layout length mismatch",
        ));
    }
    Ok(())
}

pub(crate) fn write_native_record_header<W: Write>(
    writer: &mut W,
    header: RecordHeaderFields,
    record_offset: u64,
    footer_len: u64,
) -> Result<()> {
    let bytes = encode_native_record_header(header, record_offset, footer_len)?;
    writer.write_all(&bytes)?;
    Ok(())
}

pub(crate) fn encode_native_record_header(
    header: RecordHeaderFields,
    record_offset: u64,
    footer_len: u64,
) -> Result<[u8; crate::file::RECORD_HEADER_LEN as usize]> {
    ensure_native_record_layout_contract()?;
    let anchors = NativeRecordAnchors::new(record_offset, header.payload_len, footer_len)?;
    let mut bytes = [0; crate::file::RECORD_HEADER_LEN as usize];
    let mut cursor = &mut bytes[..];
    for field in RECORD_HEADER_FIELDS {
        let value = native_header_value(*field, header, anchors)?;
        write_native_value(&mut cursor, *field, &value)?;
    }
    Ok(bytes)
}

pub(crate) fn read_native_record_header<R: Read>(
    reader: &mut R,
    _offset: u64,
) -> Result<DecodedRecordHeader> {
    ensure_native_record_layout_contract()?;
    let mut bytes = [0; crate::file::RECORD_HEADER_LEN as usize];
    reader.read_exact(&mut bytes)?;
    let mut cursor = bytes.as_slice();

    let mut block_id = None;
    let mut block_version = None;
    let mut flags = None;
    let mut sequence = None;
    let mut payload_len = None;
    let mut checksum = None;
    let mut uncompressed_len_hint = None;

    for field in RECORD_HEADER_FIELDS {
        let value = read_native_value(&mut cursor, *field)?;
        match field.name {
            "block_id" => block_id = Some(value_as_u32(field.name, &value)?),
            "block_version" => block_version = Some(value_as_u16(field.name, &value)?),
            "flags" => flags = Some(value_as_u16(field.name, &value)?),
            "sequence" => sequence = Some(value_as_u64(field.name, &value)?),
            "payload_len" => payload_len = Some(value_as_u64(field.name, &value)?),
            "checksum" => checksum = Some(value_as_u32(field.name, &value)?),
            "uncompressed_len_hint" => {
                uncompressed_len_hint = Some(value_as_u32(field.name, &value)?);
            }
            _ => {
                return Err(Error::InvalidFormatSpec(
                    "unknown native record header field",
                ));
            }
        }
    }
    if !cursor.is_empty() {
        return Err(Error::InvalidFormatSpec(
            "native record header fields did not consume the header",
        ));
    }

    Ok(DecodedRecordHeader {
        fields: RecordHeaderFields {
            block_id: block_id.ok_or(Error::LayoutFieldMissing("block_id"))?,
            block_version: block_version.ok_or(Error::LayoutFieldMissing("block_version"))?,
            flags: flags.ok_or(Error::LayoutFieldMissing("flags"))?,
            sequence: sequence.ok_or(Error::LayoutFieldMissing("sequence"))?,
            payload_len: payload_len.ok_or(Error::LayoutFieldMissing("payload_len"))?,
            checksum: checksum.ok_or(Error::LayoutFieldMissing("checksum"))?,
            uncompressed_len_hint: uncompressed_len_hint
                .ok_or(Error::LayoutFieldMissing("uncompressed_len_hint"))?,
        },
        lead_in_len: native_record_header_len(),
    })
}

pub(crate) fn encode_native_record_footer(footer: RecordFooterFields) -> Result<Vec<u8>> {
    let footer_len = native_record_footer_len();
    let footer_len =
        usize::try_from(footer_len).map_err(|_| Error::LengthOverflow { value: footer_len })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(footer_len)
        .map_err(|_| Error::AllocationFailed {
            resource: "native record footer",
            requested: u64::try_from(footer_len).unwrap_or(u64::MAX),
        })?;
    for field in RECORD_FOOTER_FIELDS {
        let value = native_footer_value(*field, footer)?;
        write_native_value(&mut bytes, *field, &value)?;
    }
    Ok(bytes)
}

pub(crate) fn decode_native_record_footer(
    bytes: &[u8],
    footer_offset: u64,
    record_offset: u64,
) -> Result<RecordFooterFields> {
    let footer_len =
        usize::try_from(native_record_footer_len()).map_err(|_| Error::InvalidRecordFooter {
            offset: footer_offset,
        })?;
    if bytes.len() != footer_len {
        return Err(Error::InvalidRecordFooter {
            offset: footer_offset,
        });
    }

    let mut cursor = bytes;
    let mut flags = None;
    let mut prev_same_block_offset = None;
    let mut prev_same_key_offset = None;

    for field in RECORD_FOOTER_FIELDS {
        let value =
            read_native_value(&mut cursor, *field).map_err(|_| Error::InvalidRecordFooter {
                offset: footer_offset,
            })?;
        match field.source {
            NativeFieldSource::LiteralBytes(expected) => {
                if !matches!(value, LayoutValue::Bytes(actual) if actual == expected) {
                    return Err(Error::InvalidRecordFooter {
                        offset: footer_offset,
                    });
                }
            }
            NativeFieldSource::LiteralU64(expected) => {
                if value_as_u64(field.name, &value).ok() != Some(expected) {
                    return Err(Error::InvalidRecordFooter {
                        offset: footer_offset,
                    });
                }
            }
            NativeFieldSource::Native("footer_flags") => {
                flags = Some(value_as_u16(field.name, &value).map_err(|_| {
                    Error::InvalidRecordFooter {
                        offset: footer_offset,
                    }
                })?);
            }
            NativeFieldSource::Native("prev_same_block_offset") => {
                prev_same_block_offset = Some(value_as_u64(field.name, &value).map_err(|_| {
                    Error::InvalidRecordFooter {
                        offset: footer_offset,
                    }
                })?);
            }
            NativeFieldSource::Native("prev_same_key_offset") => {
                prev_same_key_offset = Some(value_as_u64(field.name, &value).map_err(|_| {
                    Error::InvalidRecordFooter {
                        offset: footer_offset,
                    }
                })?);
            }
            _ => {
                return Err(Error::InvalidRecordFooter {
                    offset: footer_offset,
                });
            }
        }
    }
    if !cursor.is_empty() {
        return Err(Error::InvalidRecordFooter {
            offset: footer_offset,
        });
    }

    let flags = flags.ok_or(Error::InvalidRecordFooter {
        offset: footer_offset,
    })?;
    if flags & !crate::file::RECORD_FOOTER_KNOWN_FLAGS != 0 {
        return Err(Error::InvalidRecordFooter {
            offset: footer_offset,
        });
    }
    let prev_same_block_offset = validate_footer_offset(
        flags,
        crate::file::RECORD_FOOTER_FLAG_PREV_SAME_BLOCK,
        prev_same_block_offset.unwrap_or(0),
        record_offset,
        footer_offset,
    )?;
    let prev_same_key_offset = validate_footer_offset(
        flags,
        crate::file::RECORD_FOOTER_FLAG_PREV_SAME_KEY,
        prev_same_key_offset.unwrap_or(0),
        record_offset,
        footer_offset,
    )?;
    Ok(RecordFooterFields {
        prev_same_block_offset,
        prev_same_key_offset,
    })
}

#[derive(Clone, Copy, Debug)]
struct NativeRecordAnchors {
    raw_region_start: u64,
    footer_start: u64,
}

impl NativeRecordAnchors {
    fn new(record_offset: u64, payload_len: u64, footer_len: u64) -> Result<Self> {
        let raw_region_start = record_offset
            .checked_add(native_record_header_len())
            .ok_or(Error::LayoutInvalidSegmentBounds {
                offset: record_offset,
            })?;
        let footer_start =
            raw_region_start
                .checked_add(payload_len)
                .ok_or(Error::LayoutInvalidSegmentBounds {
                    offset: record_offset,
                })?;
        footer_start
            .checked_add(footer_len)
            .ok_or(Error::LayoutInvalidSegmentBounds {
                offset: record_offset,
            })?;
        Ok(Self {
            raw_region_start,
            footer_start,
        })
    }

    fn value(self, anchor: LayoutAnchor) -> Result<u64> {
        match anchor {
            LayoutAnchor::RawRegionStart => Ok(self.raw_region_start),
            LayoutAnchor::FooterStart => Ok(self.footer_start),
            _ => Err(Error::InvalidFormatSpec(
                "unsupported native record finalize anchor",
            )),
        }
    }
}

fn native_header_value(
    field: NativeField,
    header: RecordHeaderFields,
    anchors: NativeRecordAnchors,
) -> Result<LayoutValue> {
    match field.source {
        NativeFieldSource::Caller if field.name == "block_id" => {
            Ok(LayoutValue::U32(header.block_id))
        }
        NativeFieldSource::Caller if field.name == "block_version" => {
            Ok(LayoutValue::U16(header.block_version))
        }
        NativeFieldSource::Native("record_flags") => Ok(LayoutValue::U16(header.flags)),
        NativeFieldSource::Native("sequence") => Ok(LayoutValue::U64(header.sequence)),
        NativeFieldSource::Native("checksum") => Ok(LayoutValue::U32(header.checksum)),
        NativeFieldSource::Native("uncompressed_len_hint") => {
            Ok(LayoutValue::U32(header.uncompressed_len_hint))
        }
        NativeFieldSource::Finalize(finalize) => {
            let target = anchors.value(finalize.target)?;
            let relative = anchors.value(finalize.relative_to)?;
            Ok(LayoutValue::U64(target.checked_sub(relative).ok_or(
                Error::LayoutInvalidSegmentBounds { offset: target },
            )?))
        }
        _ => Err(Error::InvalidFormatSpec(
            "unknown native record header source",
        )),
    }
}

fn native_footer_value(field: NativeField, footer: RecordFooterFields) -> Result<LayoutValue> {
    match field.source {
        NativeFieldSource::LiteralBytes(bytes) => Ok(LayoutValue::Bytes(bytes.to_vec())),
        NativeFieldSource::LiteralU64(value) => Ok(LayoutValue::U64(value)),
        NativeFieldSource::Native("footer_flags") => {
            let mut flags = 0u16;
            if footer.prev_same_block_offset.is_some() {
                flags |= crate::file::RECORD_FOOTER_FLAG_PREV_SAME_BLOCK;
            }
            if footer.prev_same_key_offset.is_some() {
                flags |= crate::file::RECORD_FOOTER_FLAG_PREV_SAME_KEY;
            }
            Ok(LayoutValue::U16(flags))
        }
        NativeFieldSource::Native("prev_same_block_offset") => {
            Ok(LayoutValue::U64(footer.prev_same_block_offset.unwrap_or(0)))
        }
        NativeFieldSource::Native("prev_same_key_offset") => {
            Ok(LayoutValue::U64(footer.prev_same_key_offset.unwrap_or(0)))
        }
        _ => Err(Error::InvalidFormatSpec(
            "unknown native record footer source",
        )),
    }
}

fn validate_footer_offset(
    flags: u16,
    flag: u16,
    value: u64,
    record_offset: u64,
    footer_offset: u64,
) -> Result<Option<u64>> {
    match flags & flag {
        0 if value == 0 => Ok(None),
        set if set == flag && value != 0 && value < record_offset => Ok(Some(value)),
        _ => Err(Error::InvalidRecordFooter {
            offset: footer_offset,
        }),
    }
}

pub(crate) fn decode_native_internal_key_envelope(
    payload: &[u8],
    target_block_id: u32,
) -> Result<Option<&[u8]>> {
    let (target, key_len) = decode_internal_prefix(payload)?;
    let key_end =
        checked_internal_payload_end(INTERNAL_PREFIX_LEN, key_len, "internal key payload range")?;
    ensure_internal_payload_consumed(key_end, payload.len())?;
    let key = payload
        .get(INTERNAL_PREFIX_LEN..key_end)
        .ok_or(Error::UnexpectedEof)?;
    if target != target_block_id {
        return Ok(None);
    }
    Ok(Some(key))
}

pub(crate) fn decode_native_internal_op_envelope(
    payload: &[u8],
    target_block_id: u32,
) -> Result<Option<(&[u8], &[u8])>> {
    let (target, key_len) = decode_internal_prefix(payload)?;
    let key_end =
        checked_internal_payload_end(INTERNAL_PREFIX_LEN, key_len, "internal op key range")?;
    let op_len_end = key_end
        .checked_add(8)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "internal op length range",
        })?;
    if op_len_end > payload.len() {
        return Err(Error::UnexpectedEof);
    }
    let key = payload
        .get(INTERNAL_PREFIX_LEN..key_end)
        .ok_or(Error::UnexpectedEof)?;
    let op_len = read_internal_u64(payload, key_end)?;
    let op_end = checked_internal_payload_end(op_len_end, op_len, "internal op payload range")?;
    ensure_internal_payload_consumed(op_end, payload.len())?;
    let op = payload
        .get(op_len_end..op_end)
        .ok_or(Error::UnexpectedEof)?;
    if target != target_block_id {
        return Ok(None);
    }
    Ok(Some((key, op)))
}

fn decode_internal_prefix(payload: &[u8]) -> Result<(u32, u64)> {
    if payload.len() < INTERNAL_PREFIX_LEN {
        return Err(Error::UnexpectedEof);
    }
    Ok((
        read_internal_u32(payload, 0)?,
        read_internal_u64(payload, 4)?,
    ))
}

fn read_internal_u32(payload: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "internal u32 range",
        })?;
    let bytes = payload.get(offset..end).ok_or(Error::UnexpectedEof)?;
    Ok(u32::from_le_bytes(
        bytes.try_into().map_err(|_| Error::UnexpectedEof)?,
    ))
}

fn read_internal_u64(payload: &[u8], offset: usize) -> Result<u64> {
    let end = offset
        .checked_add(8)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "internal u64 range",
        })?;
    let bytes = payload.get(offset..end).ok_or(Error::UnexpectedEof)?;
    Ok(u64::from_le_bytes(
        bytes.try_into().map_err(|_| Error::UnexpectedEof)?,
    ))
}

fn checked_internal_payload_end(
    start: usize,
    encoded_len: u64,
    resource: &'static str,
) -> Result<usize> {
    let len =
        usize::try_from(encoded_len).map_err(|_| Error::LengthOverflow { value: encoded_len })?;
    start
        .checked_add(len)
        .ok_or(Error::ResourceArithmeticOverflow { resource })
}

fn ensure_internal_payload_consumed(end: usize, payload_len: usize) -> Result<()> {
    if end > payload_len {
        return Err(Error::UnexpectedEof);
    }
    if end < payload_len {
        return Err(Error::TrailingBytes {
            remaining: payload_len - end,
        });
    }
    Ok(())
}

fn visit_native_file_header_fields<F>(
    spec: FormatSpec,
    extension_len: u64,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(NativeFileHeaderField) -> Result<()>,
{
    visit(NativeFileHeaderField::Magic)?;
    visit(NativeFileHeaderField::ContainerMarker)?;
    visit(NativeFileHeaderField::FormatVersion)?;
    visit(NativeFileHeaderField::Endian)?;
    visit(NativeFileHeaderField::Flags)?;
    visit(NativeFileHeaderField::SchemaHash)?;
    if native_file_header_has_extension_len(spec, extension_len) {
        visit(NativeFileHeaderField::ExtensionLen)?;
    }
    if extension_len > 0 {
        visit(NativeFileHeaderField::Extensions)?;
    }
    Ok(())
}

fn native_file_header_field(
    spec: FormatSpec,
    field: NativeFileHeaderField,
    extension_len: u64,
) -> NativeField {
    match field {
        NativeFileHeaderField::Magic => NativeField {
            name: "magic",
            ty: NativeFieldType::Bytes {
                len: u64::try_from(spec.magic.len())
                    .expect("native file-header magic length must fit u64"),
            },
            source: NativeFieldSource::Native("magic"),
        },
        NativeFileHeaderField::ContainerMarker => NativeField {
            name: "container_marker",
            ty: NativeFieldType::Bytes { len: 6 },
            source: NativeFieldSource::Native("container_marker"),
        },
        NativeFileHeaderField::FormatVersion => NativeField {
            name: "format_version",
            ty: NativeFieldType::U16,
            source: NativeFieldSource::Native("format_version"),
        },
        NativeFileHeaderField::Endian => NativeField {
            name: "endian",
            ty: NativeFieldType::U8,
            source: NativeFieldSource::Native("endian"),
        },
        NativeFileHeaderField::Flags => NativeField {
            name: "flags",
            ty: NativeFieldType::U8,
            source: NativeFieldSource::LiteralU64(0),
        },
        NativeFileHeaderField::SchemaHash => NativeField {
            name: "schema_hash",
            ty: NativeFieldType::U64,
            source: NativeFieldSource::Native("schema_hash"),
        },
        NativeFileHeaderField::ExtensionLen => NativeField {
            name: "extension_len",
            ty: NativeFieldType::U32,
            source: NativeFieldSource::Native("extension_len"),
        },
        NativeFileHeaderField::Extensions => NativeField {
            name: "extensions",
            ty: NativeFieldType::Bytes { len: extension_len },
            source: NativeFieldSource::Native("file_explicit_compression_header"),
        },
    }
}

fn native_file_header_field_to_plan(
    spec: FormatSpec,
    field: NativeFileHeaderField,
    extension_len: u64,
) -> LayoutPlanField {
    let native_field = native_file_header_field(spec, field, extension_len);
    LayoutPlanField {
        name: native_field.name.to_string(),
        ty: match field {
            NativeFileHeaderField::Magic => LayoutPlanFieldType::Bytes {
                len: LayoutPlanLen::Fixed(
                    u64::try_from(spec.magic.len())
                        .expect("native file-header magic length must fit u64"),
                ),
            },
            _ => native_field_type_to_plan(native_field.ty),
        },
        source: match field {
            NativeFileHeaderField::Magic => {
                LayoutPlanFieldSource::LiteralBytes(spec.magic.to_vec())
            }
            NativeFileHeaderField::ContainerMarker => {
                LayoutPlanFieldSource::LiteralBytes(native_container_marker_for_plan(spec).to_vec())
            }
            NativeFileHeaderField::Flags => LayoutPlanFieldSource::LiteralU64(0),
            _ => native_field_source_to_plan(native_field.source),
        },
        endian: Some(Endian::Little),
    }
}

fn native_file_header_write_value(
    spec: FormatSpec,
    field: NativeFileHeaderField,
    marker: &[u8; 6],
    extensions: &[u8],
) -> Result<LayoutValue> {
    Ok(match field {
        NativeFileHeaderField::Magic => LayoutValue::Bytes(spec.magic.to_vec()),
        NativeFileHeaderField::ContainerMarker => LayoutValue::Bytes(marker.to_vec()),
        NativeFileHeaderField::FormatVersion => LayoutValue::U16(spec.version),
        NativeFileHeaderField::Endian => LayoutValue::U8(spec.endian.to_byte()),
        NativeFileHeaderField::Flags => LayoutValue::U8(0),
        NativeFileHeaderField::SchemaHash => LayoutValue::U64(spec.schema_hash),
        NativeFileHeaderField::ExtensionLen => {
            let len =
                u32::try_from(extensions.len()).map_err(|_| Error::InvalidCompressionHeader)?;
            LayoutValue::U32(len)
        }
        NativeFileHeaderField::Extensions => LayoutValue::Bytes(extensions.to_vec()),
    })
}

fn native_marker_from_bytes(bytes: &[u8]) -> Result<[u8; 6]> {
    if bytes == CONTAINER_MARKER_V1 {
        Ok(*CONTAINER_MARKER_V1)
    } else if bytes == CONTAINER_MARKER_V2 {
        Ok(*CONTAINER_MARKER_V2)
    } else if bytes == CONTAINER_MARKER_V3 {
        Ok(*CONTAINER_MARKER_V3)
    } else {
        Err(Error::UnsupportedContainer)
    }
}

fn native_container_marker_for_extensions(
    spec: FormatSpec,
    extension_len: u64,
) -> &'static [u8; 6] {
    if spec.spec_needs_record_footer() {
        CONTAINER_MARKER_V3
    } else if extension_len == 0 {
        CONTAINER_MARKER_V1
    } else {
        CONTAINER_MARKER_V2
    }
}

fn native_container_marker_for_plan(spec: FormatSpec) -> &'static [u8; 6] {
    native_container_marker_for_extensions(spec, native_file_header_plan_extension_len(spec))
}

fn native_file_header_plan_extension_len(spec: FormatSpec) -> u64 {
    if native_uses_file_explicit_compression(spec) {
        FILE_EXPLICIT_COMPRESSION_HEADER_LEN
    } else {
        0
    }
}

fn native_file_header_has_extension_len(spec: FormatSpec, extension_len: u64) -> bool {
    spec.spec_needs_record_footer() || extension_len > 0
}

fn native_uses_file_explicit_compression(spec: FormatSpec) -> bool {
    matches!(
        spec.compression_policy,
        CompressionPolicy::VariableBlocks(compression)
            if compression.header_mode == CompressionHeaderMode::FileExplicit
    )
}

fn native_fields_to_plan(fields: &[NativeField]) -> Vec<LayoutPlanField> {
    fields
        .iter()
        .map(|field| LayoutPlanField {
            name: field.name.to_string(),
            ty: native_field_type_to_plan(field.ty),
            source: native_field_source_to_plan(field.source),
            endian: Some(Endian::Little),
        })
        .collect()
}

fn native_field_type_to_plan(ty: NativeFieldType) -> LayoutPlanFieldType {
    match ty {
        NativeFieldType::Bytes { len } => LayoutPlanFieldType::Bytes {
            len: LayoutPlanLen::Fixed(len),
        },
        NativeFieldType::U8 => LayoutPlanFieldType::U8,
        NativeFieldType::U16 => LayoutPlanFieldType::U16,
        NativeFieldType::U32 => LayoutPlanFieldType::U32,
        NativeFieldType::U64 => LayoutPlanFieldType::U64,
    }
}

fn native_field_source_to_plan(source: NativeFieldSource) -> LayoutPlanFieldSource {
    match source {
        NativeFieldSource::LiteralBytes(bytes) => {
            LayoutPlanFieldSource::LiteralBytes(bytes.to_vec())
        }
        NativeFieldSource::LiteralU64(value) => LayoutPlanFieldSource::LiteralU64(value),
        NativeFieldSource::Caller => LayoutPlanFieldSource::Caller,
        NativeFieldSource::Native(name) => LayoutPlanFieldSource::Native(name),
        NativeFieldSource::Finalize(finalize) => LayoutPlanFieldSource::Finalize(finalize),
    }
}

fn native_fields_len(fields: &[NativeField]) -> u64 {
    fields.iter().map(|field| field.ty.byte_len()).sum()
}

fn write_native_value<W: Write>(
    writer: &mut W,
    field: NativeField,
    value: &LayoutValue,
) -> Result<()> {
    match (field.ty, value) {
        (NativeFieldType::Bytes { len }, LayoutValue::Bytes(bytes))
            if usize::try_from(len).ok() == Some(bytes.len()) =>
        {
            writer.write_all(bytes)?;
            Ok(())
        }
        (NativeFieldType::U8, LayoutValue::U8(value)) => {
            writer.write_all(&[*value])?;
            Ok(())
        }
        (NativeFieldType::U8, LayoutValue::U64(value)) => {
            let value =
                u8::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field.name))?;
            writer.write_all(&[value])?;
            Ok(())
        }
        (NativeFieldType::U16, LayoutValue::U16(value)) => {
            writer.write_all(&value.to_le_bytes())?;
            Ok(())
        }
        (NativeFieldType::U16, LayoutValue::U64(value)) => {
            let value =
                u16::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field.name))?;
            writer.write_all(&value.to_le_bytes())?;
            Ok(())
        }
        (NativeFieldType::U32, LayoutValue::U32(value)) => {
            writer.write_all(&value.to_le_bytes())?;
            Ok(())
        }
        (NativeFieldType::U32, LayoutValue::U64(value)) => {
            let value =
                u32::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field.name))?;
            writer.write_all(&value.to_le_bytes())?;
            Ok(())
        }
        (NativeFieldType::U64, LayoutValue::U64(value)) => {
            writer.write_all(&value.to_le_bytes())?;
            Ok(())
        }
        _ => Err(Error::LayoutFieldTypeMismatch(field.name)),
    }
}

fn read_native_value<R: Read>(reader: &mut R, field: NativeField) -> Result<LayoutValue> {
    Ok(match field.ty {
        NativeFieldType::Bytes { len } => {
            let len = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
            let mut bytes = vec![0; len];
            reader.read_exact(&mut bytes)?;
            LayoutValue::Bytes(bytes)
        }
        NativeFieldType::U8 => {
            let mut bytes = [0; 1];
            reader.read_exact(&mut bytes)?;
            LayoutValue::U8(bytes[0])
        }
        NativeFieldType::U16 => {
            let mut bytes = [0; 2];
            reader.read_exact(&mut bytes)?;
            LayoutValue::U16(u16::from_le_bytes(bytes))
        }
        NativeFieldType::U32 => {
            let mut bytes = [0; 4];
            reader.read_exact(&mut bytes)?;
            LayoutValue::U32(u32::from_le_bytes(bytes))
        }
        NativeFieldType::U64 => {
            let mut bytes = [0; 8];
            reader.read_exact(&mut bytes)?;
            LayoutValue::U64(u64::from_le_bytes(bytes))
        }
    })
}

fn value_as_u8(field: &'static str, value: &LayoutValue) -> Result<u8> {
    match value {
        LayoutValue::U8(value) => Ok(*value),
        LayoutValue::U64(value) => {
            u8::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
        }
        _ => Err(Error::LayoutFieldTypeMismatch(field)),
    }
}

fn value_as_u16(field: &'static str, value: &LayoutValue) -> Result<u16> {
    match value {
        LayoutValue::U8(value) => Ok(u16::from(*value)),
        LayoutValue::U16(value) => Ok(*value),
        LayoutValue::U64(value) => {
            u16::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
        }
        _ => Err(Error::LayoutFieldTypeMismatch(field)),
    }
}

fn value_as_u32(field: &'static str, value: &LayoutValue) -> Result<u32> {
    match value {
        LayoutValue::U8(value) => Ok(u32::from(*value)),
        LayoutValue::U16(value) => Ok(u32::from(*value)),
        LayoutValue::U32(value) => Ok(*value),
        LayoutValue::U64(value) => {
            u32::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
        }
        _ => Err(Error::LayoutFieldTypeMismatch(field)),
    }
}

fn value_as_u64(field: &'static str, value: &LayoutValue) -> Result<u64> {
    match value {
        LayoutValue::U8(value) => Ok(u64::from(*value)),
        LayoutValue::U16(value) => Ok(u64::from(*value)),
        LayoutValue::U32(value) => Ok(u64::from(*value)),
        LayoutValue::U64(value) => Ok(*value),
        _ => Err(Error::LayoutFieldTypeMismatch(field)),
    }
}
