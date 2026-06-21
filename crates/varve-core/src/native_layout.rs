use std::io::{Read, Write};

use crate::{
    Endian, Error, LayoutAnchor, LayoutFinalize, LayoutPlanField, LayoutPlanFieldSource,
    LayoutPlanFieldType, LayoutPlanLen, Result,
    file::{RecordFooterFields, RecordHeaderFields},
    layout::LayoutValue,
};

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
    U16,
    U32,
    U64,
}

impl NativeFieldType {
    const fn byte_len(self) -> u64 {
        match self {
            Self::Bytes { len } => len,
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

pub(crate) fn write_native_record_header<W: Write>(
    writer: &mut W,
    header: RecordHeaderFields,
    record_offset: u64,
    footer_len: u64,
) -> Result<()> {
    ensure_native_record_layout_contract()?;
    let anchors = NativeRecordAnchors::new(record_offset, header.payload_len, footer_len)?;
    let mut bytes = [0; crate::file::RECORD_HEADER_LEN as usize];
    let mut cursor = &mut bytes[..];
    for field in RECORD_HEADER_FIELDS {
        let value = native_header_value(*field, header, anchors)?;
        write_native_value(&mut cursor, *field, &value)?;
    }
    writer.write_all(&bytes)?;
    Ok(())
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
    let mut bytes = Vec::with_capacity(native_record_footer_len() as usize);
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
    if bytes.len() as u64 != native_record_footer_len() {
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
        set if set == flag && value < record_offset => Ok(Some(value)),
        _ => Err(Error::InvalidRecordFooter {
            offset: footer_offset,
        }),
    }
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
            if len == bytes.len() as u64 =>
        {
            writer.write_all(bytes)?;
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

fn value_as_u16(field: &'static str, value: &LayoutValue) -> Result<u16> {
    match value {
        LayoutValue::U16(value) => Ok(*value),
        LayoutValue::U64(value) => {
            u16::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
        }
        _ => Err(Error::LayoutFieldTypeMismatch(field)),
    }
}

fn value_as_u32(field: &'static str, value: &LayoutValue) -> Result<u32> {
    match value {
        LayoutValue::U32(value) => Ok(*value),
        LayoutValue::U64(value) => {
            u32::try_from(*value).map_err(|_| Error::LayoutFieldTypeMismatch(field))
        }
        _ => Err(Error::LayoutFieldTypeMismatch(field)),
    }
}

fn value_as_u64(field: &'static str, value: &LayoutValue) -> Result<u64> {
    match value {
        LayoutValue::U16(value) => Ok(u64::from(*value)),
        LayoutValue::U32(value) => Ok(u64::from(*value)),
        LayoutValue::U64(value) => Ok(*value),
        _ => Err(Error::LayoutFieldTypeMismatch(field)),
    }
}
