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
/// The largest extension region a reader will accept.
///
/// The region's length field is a `u32`, so an untrusted header could otherwise
/// name a 4 GiB region and have open allocate it before a single block is
/// parsed. Header blocks are tens of bytes; 64 KiB is far above any use and
/// keeps the allocation bounded, which is what open being cheap depends on.
pub(crate) const MAX_FILE_HEADER_EXTENSION_LEN: u64 = 64 * 1024;
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

/// Which record field an entry of `RECORD_HEADER_FIELDS` /
/// `RECORD_FOOTER_FIELDS` *is*.
///
/// `NativeField::name` stays what it was — it is what
/// `native_field_source_to_plan` publishes into `LayoutPlanField` and what
/// `Error::LayoutFieldTypeMismatch` / `Error::LayoutFieldMissing` carry — but
/// it is no longer what encode and decode dispatch on. The four dispatch sites
/// match this enum exhaustively, so a table entry added without a role is a
/// compile error instead of an `Error::InvalidFormatSpec` raised once per
/// record framed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeRecordField {
    BlockId,
    BlockVersion,
    RecordFlags,
    Sequence,
    PayloadLen,
    Checksum,
    UncompressedLenHint,
    FooterMagic,
    FooterVersion,
    FooterFlags,
    PrevSameBlockOffset,
    PrevSameKeyOffset,
    FooterCrc32,
    FooterReserved,
}

/// A record header/footer table entry: the wire description plus the role that
/// selects the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NativeRecordFieldDef {
    role: NativeRecordField,
    field: NativeField,
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

const RECORD_HEADER_FIELDS: &[NativeRecordFieldDef] = &[
    NativeRecordFieldDef {
        role: NativeRecordField::BlockId,
        field: NativeField {
            name: "block_id",
            ty: NativeFieldType::U32,
            source: NativeFieldSource::Caller,
        },
    },
    NativeRecordFieldDef {
        role: NativeRecordField::BlockVersion,
        field: NativeField {
            name: "block_version",
            ty: NativeFieldType::U16,
            source: NativeFieldSource::Caller,
        },
    },
    NativeRecordFieldDef {
        role: NativeRecordField::RecordFlags,
        field: NativeField {
            name: "flags",
            ty: NativeFieldType::U16,
            source: NativeFieldSource::Native("record_flags"),
        },
    },
    NativeRecordFieldDef {
        role: NativeRecordField::Sequence,
        field: NativeField {
            name: "sequence",
            ty: NativeFieldType::U64,
            source: NativeFieldSource::Native("sequence"),
        },
    },
    NativeRecordFieldDef {
        role: NativeRecordField::PayloadLen,
        field: NativeField {
            name: "payload_len",
            ty: NativeFieldType::U64,
            source: NativeFieldSource::Finalize(LayoutFinalize {
                target: LayoutAnchor::FooterStart,
                relative_to: LayoutAnchor::RawRegionStart,
            }),
        },
    },
    NativeRecordFieldDef {
        role: NativeRecordField::Checksum,
        field: NativeField {
            name: "checksum",
            ty: NativeFieldType::U32,
            source: NativeFieldSource::Native("checksum"),
        },
    },
    NativeRecordFieldDef {
        role: NativeRecordField::UncompressedLenHint,
        field: NativeField {
            name: "uncompressed_len_hint",
            ty: NativeFieldType::U32,
            source: NativeFieldSource::Native("uncompressed_len_hint"),
        },
    },
];

const RECORD_FOOTER_FIELDS: &[NativeRecordFieldDef] = &[
    NativeRecordFieldDef {
        role: NativeRecordField::FooterMagic,
        field: NativeField {
            name: "magic",
            ty: NativeFieldType::Bytes { len: 4 },
            source: NativeFieldSource::LiteralBytes(crate::file::RECORD_FOOTER_MAGIC),
        },
    },
    NativeRecordFieldDef {
        role: NativeRecordField::FooterVersion,
        field: NativeField {
            name: "footer_version",
            ty: NativeFieldType::U16,
            source: NativeFieldSource::LiteralU64(crate::file::RECORD_FOOTER_VERSION as u64),
        },
    },
    NativeRecordFieldDef {
        role: NativeRecordField::FooterFlags,
        field: NativeField {
            name: "footer_flags",
            ty: NativeFieldType::U16,
            source: NativeFieldSource::Native("footer_flags"),
        },
    },
    NativeRecordFieldDef {
        role: NativeRecordField::PrevSameBlockOffset,
        field: NativeField {
            name: "prev_same_block_offset",
            ty: NativeFieldType::U64,
            source: NativeFieldSource::Native("prev_same_block_offset"),
        },
    },
    NativeRecordFieldDef {
        role: NativeRecordField::PrevSameKeyOffset,
        field: NativeField {
            name: "prev_same_key_offset",
            ty: NativeFieldType::U64,
            source: NativeFieldSource::Native("prev_same_key_offset"),
        },
    },
    NativeRecordFieldDef {
        role: NativeRecordField::FooterCrc32,
        field: NativeField {
            name: "footer_crc32",
            ty: NativeFieldType::U32,
            source: NativeFieldSource::LiteralU64(0),
        },
    },
    NativeRecordFieldDef {
        role: NativeRecordField::FooterReserved,
        field: NativeField {
            name: "reserved",
            ty: NativeFieldType::U32,
            source: NativeFieldSource::LiteralU64(0),
        },
    },
];

/// Sum of the declared field widths, folded at compile time.
///
/// `NativeFieldType::byte_len` is already `const fn`, so the two per-record
/// folds this used to drive — `read_native_record_header`'s `lead_in_len` and
/// `NativeRecordAnchors::new` — become constants.
const fn native_record_fields_len(fields: &[NativeRecordFieldDef]) -> u64 {
    let mut total = 0;
    let mut index = 0;
    while index < fields.len() {
        total += fields[index].field.ty.byte_len();
        index += 1;
    }
    total
}

const RECORD_HEADER_FIELDS_LEN: u64 = native_record_fields_len(RECORD_HEADER_FIELDS);
const RECORD_FOOTER_FIELDS_LEN: u64 = native_record_fields_len(RECORD_FOOTER_FIELDS);

/// The length contract, discharged by the compiler.
///
/// This used to be `ensure_native_record_layout_contract`, re-derived from the
/// tables on every encode and every decode. A table that no longer sums to the
/// published 32-byte header or footer now fails the build instead of failing
/// `FormatSpec::validate` at open.
const _: () = {
    assert!(RECORD_HEADER_FIELDS_LEN == crate::file::RECORD_HEADER_LEN);
    assert!(RECORD_FOOTER_FIELDS_LEN == crate::file::RECORD_FOOTER_LEN);
};

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
        // The region is no longer required to be exactly what this spec would
        // write: a block whose magic this build does not know is skipped, and
        // that is only possible if a longer region can be read at all. What
        // the region *contains* is judged by the block walk in `file.rs`.
        if extension_len > MAX_FILE_HEADER_EXTENSION_LEN {
            return Err(Error::InvalidCompressionHeader);
        }
        // No writer emits a length field for an empty region — it picks
        // `VARVE1` — and `checked_native_file_header_len` reconstructs the
        // field's presence from the length alone, so it would compute a header
        // four bytes short for this combination. Refuse it instead.
        if extension_len == 0 && !spec.spec_needs_record_footer() {
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

pub(crate) const fn native_record_header_len() -> u64 {
    RECORD_HEADER_FIELDS_LEN
}

pub(crate) const fn native_record_footer_len() -> u64 {
    RECORD_FOOTER_FIELDS_LEN
}

/// Kept so `FormatSpec::validate` keeps its call site; the check itself now
/// lives in the `const _: ()` assertion beside the field tables, which the
/// compiler discharges once instead of this running per encode and per decode.
pub(crate) fn ensure_native_record_layout_contract() -> Result<()> {
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
    let anchors = NativeRecordAnchors::new(record_offset, header.payload_len, footer_len)?;
    let mut bytes = [0; crate::file::RECORD_HEADER_LEN as usize];
    let mut cursor = &mut bytes[..];
    for def in RECORD_HEADER_FIELDS {
        let value = native_header_value(*def, header, anchors)?;
        write_native_value(&mut cursor, def.field, &value)?;
    }
    Ok(bytes)
}

pub(crate) fn read_native_record_header<R: Read>(
    reader: &mut R,
    _offset: u64,
) -> Result<DecodedRecordHeader> {
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

    for def in RECORD_HEADER_FIELDS {
        let name = def.field.name;
        let value = read_native_value(&mut cursor, def.field)?;
        match def.role {
            NativeRecordField::BlockId => block_id = Some(value_as_u32(name, &value)?),
            NativeRecordField::BlockVersion => block_version = Some(value_as_u16(name, &value)?),
            NativeRecordField::RecordFlags => flags = Some(value_as_u16(name, &value)?),
            NativeRecordField::Sequence => sequence = Some(value_as_u64(name, &value)?),
            NativeRecordField::PayloadLen => payload_len = Some(value_as_u64(name, &value)?),
            NativeRecordField::Checksum => checksum = Some(value_as_u32(name, &value)?),
            NativeRecordField::UncompressedLenHint => {
                uncompressed_len_hint = Some(value_as_u32(name, &value)?);
            }
            NativeRecordField::FooterMagic
            | NativeRecordField::FooterVersion
            | NativeRecordField::FooterFlags
            | NativeRecordField::PrevSameBlockOffset
            | NativeRecordField::PrevSameKeyOffset
            | NativeRecordField::FooterCrc32
            | NativeRecordField::FooterReserved => {
                return Err(Error::InvalidFormatSpec(
                    "footer role in the native record header table",
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

/// Encodes the record footer into a stack array, as the identical-size header
/// beside it already does.
///
/// The footer is exactly `RECORD_FOOTER_LEN` bytes — the `const _: ()` above
/// makes the compiler prove `RECORD_FOOTER_FIELDS_LEN` equals it — so the `Vec`
/// this used to build was a heap allocation of a statically known 32 bytes,
/// taken and dropped once per appended record. Continuous high-rate streaming
/// append is the primary workload and is specified to make no per-record
/// allocation; this was one.
///
/// The cursor must land exactly at the end. The `Vec` form got that check for
/// free — its length *was* what the fields wrote, and `encode_record_footer`
/// compared it — so a fixed array has to state it, or a field table that stopped
/// short would silently emit trailing zeros as a valid-looking footer.
pub(crate) fn encode_native_record_footer(
    footer: RecordFooterFields,
) -> Result<[u8; crate::file::RECORD_FOOTER_LEN as usize]> {
    let mut bytes = [0; crate::file::RECORD_FOOTER_LEN as usize];
    let mut cursor = &mut bytes[..];
    for def in RECORD_FOOTER_FIELDS {
        // The magic is a `&'static [u8]` in the field table and four bytes wide.
        // Routing it through `LayoutValue::Bytes(Vec<u8>)` allocated a heap
        // buffer for that constant on every appended record; writing it here
        // emits the identical bytes and touches no allocator.
        if let NativeRecordField::FooterMagic = def.role {
            write_literal_bytes_field(&mut cursor, def.field)?;
            continue;
        }
        let value = native_footer_value(*def, footer)?;
        write_native_value(&mut cursor, def.field, &value)?;
    }
    if !cursor.is_empty() {
        return Err(Error::InvalidFormatSpec(
            "native record footer fields did not fill the footer",
        ));
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
    let mut mutable_flags = 0u32;

    for def in RECORD_FOOTER_FIELDS {
        let invalid = || Error::InvalidRecordFooter {
            offset: footer_offset,
        };
        // Compared against the cursor in place. The generic reader allocates a
        // `Vec` the size of the field to hold bytes it only ever compares, and
        // the footer is decoded once per record on every scan.
        if let NativeRecordField::FooterMagic = def.role {
            let expected = literal_bytes_of(def.field).ok_or_else(invalid)?;
            let (actual, rest) = cursor
                .split_at_checked(expected.len())
                .ok_or_else(invalid)?;
            if actual != expected {
                return Err(invalid());
            }
            cursor = rest;
            continue;
        }
        let name = def.field.name;
        let value =
            read_native_value(&mut cursor, def.field).map_err(|_| Error::InvalidRecordFooter {
                offset: footer_offset,
            })?;
        match def.role {
            NativeRecordField::FooterMagic => return Err(invalid()),
            NativeRecordField::FooterVersion | NativeRecordField::FooterCrc32 => {
                let NativeFieldSource::LiteralU64(expected) = def.field.source else {
                    return Err(invalid());
                };
                if value_as_u64(name, &value).ok() != Some(expected) {
                    return Err(invalid());
                }
            }
            // Handed back rather than checked here. Whether a non-zero value is
            // legitimate depends on the spec's `LivenessPolicy`, which this
            // decoder does not carry; `decode_record_footer` refuses it under
            // `LivenessPolicy::None`, so the strictness this field has always
            // had is unchanged for a format that did not opt in.
            NativeRecordField::FooterReserved => {
                mutable_flags = u32::try_from(value_as_u64(name, &value).map_err(|_| invalid())?)
                    .map_err(|_| invalid())?;
            }
            NativeRecordField::FooterFlags => {
                flags = Some(value_as_u16(name, &value).map_err(|_| invalid())?);
            }
            NativeRecordField::PrevSameBlockOffset => {
                prev_same_block_offset = Some(value_as_u64(name, &value).map_err(|_| invalid())?);
            }
            NativeRecordField::PrevSameKeyOffset => {
                prev_same_key_offset = Some(value_as_u64(name, &value).map_err(|_| invalid())?);
            }
            NativeRecordField::BlockId
            | NativeRecordField::BlockVersion
            | NativeRecordField::RecordFlags
            | NativeRecordField::Sequence
            | NativeRecordField::PayloadLen
            | NativeRecordField::Checksum
            | NativeRecordField::UncompressedLenHint => {
                return Err(invalid());
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
        mutable_flags,
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
    def: NativeRecordFieldDef,
    header: RecordHeaderFields,
    anchors: NativeRecordAnchors,
) -> Result<LayoutValue> {
    match def.role {
        NativeRecordField::BlockId => Ok(LayoutValue::U32(header.block_id)),
        NativeRecordField::BlockVersion => Ok(LayoutValue::U16(header.block_version)),
        NativeRecordField::RecordFlags => Ok(LayoutValue::U16(header.flags)),
        NativeRecordField::Sequence => Ok(LayoutValue::U64(header.sequence)),
        NativeRecordField::PayloadLen => {
            let NativeFieldSource::Finalize(finalize) = def.field.source else {
                return Err(Error::InvalidFormatSpec(
                    "native record payload_len must be a finalize field",
                ));
            };
            let target = anchors.value(finalize.target)?;
            let relative = anchors.value(finalize.relative_to)?;
            Ok(LayoutValue::U64(target.checked_sub(relative).ok_or(
                Error::LayoutInvalidSegmentBounds { offset: target },
            )?))
        }
        NativeRecordField::Checksum => Ok(LayoutValue::U32(header.checksum)),
        NativeRecordField::UncompressedLenHint => {
            Ok(LayoutValue::U32(header.uncompressed_len_hint))
        }
        NativeRecordField::FooterMagic
        | NativeRecordField::FooterVersion
        | NativeRecordField::FooterFlags
        | NativeRecordField::PrevSameBlockOffset
        | NativeRecordField::PrevSameKeyOffset
        | NativeRecordField::FooterCrc32
        | NativeRecordField::FooterReserved => Err(Error::InvalidFormatSpec(
            "footer role in the native record header table",
        )),
    }
}

/// The literal bytes a fixed-width byte field declares, when its declared
/// width agrees with them.
///
/// Both the encoder and the decoder need exactly this, and both used to obtain
/// it by building a `Vec`.
fn literal_bytes_of(field: NativeField) -> Option<&'static [u8]> {
    let NativeFieldSource::LiteralBytes(bytes) = field.source else {
        return None;
    };
    let NativeFieldType::Bytes { len } = field.ty else {
        return None;
    };
    (usize::try_from(len).ok() == Some(bytes.len())).then_some(bytes)
}

/// Writes a literal byte field's own bytes, with the same width check
/// [`write_native_value`] applies to a `LayoutValue::Bytes`.
fn write_literal_bytes_field<W: Write>(writer: &mut W, field: NativeField) -> Result<()> {
    let bytes = literal_bytes_of(field).ok_or(Error::LayoutFieldTypeMismatch(field.name))?;
    writer.write_all(bytes)?;
    Ok(())
}

fn native_footer_value(
    def: NativeRecordFieldDef,
    footer: RecordFooterFields,
) -> Result<LayoutValue> {
    match def.role {
        NativeRecordField::FooterMagic => {
            let NativeFieldSource::LiteralBytes(bytes) = def.field.source else {
                return Err(Error::InvalidFormatSpec(
                    "native record footer magic must be a literal",
                ));
            };
            Ok(LayoutValue::Bytes(bytes.to_vec()))
        }
        NativeRecordField::FooterVersion | NativeRecordField::FooterCrc32 => {
            let NativeFieldSource::LiteralU64(value) = def.field.source else {
                return Err(Error::InvalidFormatSpec(
                    "native record footer constant must be a literal",
                ));
            };
            Ok(LayoutValue::U64(value))
        }
        // The one field of a written record that is not fixed at append time.
        // `LivenessPolicy::None` never sets a bit here, so it stays the literal
        // zero it has always been and the encoded footer is byte-identical.
        NativeRecordField::FooterReserved => Ok(LayoutValue::U64(u64::from(footer.mutable_flags))),
        NativeRecordField::FooterFlags => {
            let mut flags = 0u16;
            if footer.prev_same_block_offset.is_some() {
                flags |= crate::file::RECORD_FOOTER_FLAG_PREV_SAME_BLOCK;
            }
            if footer.prev_same_key_offset.is_some() {
                flags |= crate::file::RECORD_FOOTER_FLAG_PREV_SAME_KEY;
            }
            Ok(LayoutValue::U16(flags))
        }
        NativeRecordField::PrevSameBlockOffset => {
            Ok(LayoutValue::U64(footer.prev_same_block_offset.unwrap_or(0)))
        }
        NativeRecordField::PrevSameKeyOffset => {
            Ok(LayoutValue::U64(footer.prev_same_key_offset.unwrap_or(0)))
        }
        NativeRecordField::BlockId
        | NativeRecordField::BlockVersion
        | NativeRecordField::RecordFlags
        | NativeRecordField::Sequence
        | NativeRecordField::PayloadLen
        | NativeRecordField::Checksum
        | NativeRecordField::UncompressedLenHint => Err(Error::InvalidFormatSpec(
            "header role in the native record footer table",
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
            // The region held exactly one block until `header_tails`, and the
            // source name said so. It can now hold two, so the name has to
            // widen — but only for the specs that can carry the second one, so
            // every plan published before this is byte-identical.
            source: NativeFieldSource::Native(if spec.index_policy.header_tails {
                "file_header_extension_region"
            } else {
                "file_explicit_compression_header"
            }),
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

/// The extension region length the plan publishes, which must be the length the
/// writer actually emits.
///
/// It is derived here rather than taken from `file_header_extensions` because
/// the plan is infallible and that function is not; every term must therefore
/// be a length the writer agrees with. `header_tails_region_len` is the writer's
/// own derivation, imported rather than restated — restating it is what made
/// the plan describe a header short by the whole `VBTT` region.
fn native_file_header_plan_extension_len(spec: FormatSpec) -> u64 {
    let compression = if native_uses_file_explicit_compression(spec) {
        FILE_EXPLICIT_COMPRESSION_HEADER_LEN
    } else {
        0
    };
    compression
        .saturating_add(crate::file::header_tails_region_len(spec))
        .saturating_add(crate::file::liveness_region_len(spec))
        .saturating_add(crate::format::header_slots_region_len(spec))
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

fn native_fields_to_plan(fields: &[NativeRecordFieldDef]) -> Vec<LayoutPlanField> {
    fields
        .iter()
        .map(|def| LayoutPlanField {
            name: def.field.name.to_string(),
            ty: native_field_type_to_plan(def.field.ty),
            source: native_field_source_to_plan(def.field.source),
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
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(len)
                .map_err(|_| Error::AllocationFailed {
                    resource: "native layout byte field",
                    requested: u64::try_from(len).unwrap_or(u64::MAX),
                })?;
            bytes.resize(len, 0);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Distinct, non-symmetric values in every field: a transposition of two
    /// roles moves at least two bytes.
    const PROBE_HEADER: RecordHeaderFields = RecordHeaderFields {
        block_id: 0x1122_3344,
        block_version: 0x5566,
        flags: 0x7788,
        sequence: 0x99AA_BBCC_DDEE_FF00,
        payload_len: 0,
        checksum: 0x0102_0304,
        uncompressed_len_hint: 0x0506_0708,
    };

    const PROBE_FOOTER: RecordFooterFields = RecordFooterFields {
        prev_same_block_offset: Some(0x1122_3344_5566_7788),
        prev_same_key_offset: Some(0x0102_0304_0506_0708),
        // Zero, so the captured bytes below stay the bytes this format has
        // always written: `LivenessPolicy::None` never sets a bit here, and
        // that is what "the option is inert when off" means at this layer.
        mutable_flags: 0,
    };

    /// Captured from the build before the role dispatch replaced the
    /// field-name dispatch, by printing `encode_native_record_header`'s output
    /// for `PROBE_HEADER`.
    const PROBE_HEADER_BYTES: [u8; crate::file::RECORD_HEADER_LEN as usize] = [
        0x44, 0x33, 0x22, 0x11, // block_id
        0x66, 0x55, // block_version
        0x88, 0x77, // flags
        0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, // sequence
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // payload_len
        0x04, 0x03, 0x02, 0x01, // checksum
        0x08, 0x07, 0x06, 0x05, // uncompressed_len_hint
    ];

    /// Same capture, for `encode_native_record_footer(PROBE_FOOTER)`.
    const PROBE_FOOTER_BYTES: [u8; crate::file::RECORD_FOOTER_LEN as usize] = [
        0x56, 0x52, 0x46, 0x31, // magic "VRF1"
        0x01, 0x00, // footer_version
        0x03, 0x00, // footer_flags: prev_same_block | prev_same_key
        0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // prev_same_block_offset
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // prev_same_key_offset
        0x00, 0x00, 0x00, 0x00, // footer_crc32
        0x00, 0x00, 0x00, 0x00, // reserved
    ];

    /// The safety assertion for the role dispatch, not a speed assertion.
    ///
    /// A `role` transposed between two same-width adjacent fields — `checksum`
    /// and `uncompressed_len_hint`, say — round-trips symmetrically and is
    /// invisible to every encode/decode test in the workspace. It is not
    /// invisible here: every probe field holds a distinct, non-symmetric value,
    /// so any transposition moves at least two bytes away from the literal
    /// captured from the pre-change build.
    #[test]
    fn native_record_header_bytes_are_unchanged_by_the_role_dispatch() {
        let header = encode_native_record_header(PROBE_HEADER, 0x40, 32)
            .expect("probe header encodes within a 32-byte header");
        assert_eq!(header, PROBE_HEADER_BYTES);

        let footer = encode_native_record_footer(PROBE_FOOTER).expect("probe footer encodes");
        assert_eq!(footer.as_slice(), PROBE_FOOTER_BYTES.as_slice());
    }

    /// The decoders must read back exactly what the byte literals hold, so a
    /// role transposed on the *decode* side alone is caught too.
    #[test]
    fn native_record_header_and_footer_decode_the_captured_bytes() {
        let decoded = read_native_record_header(&mut PROBE_HEADER_BYTES.as_slice(), 0x40)
            .expect("captured header decodes");
        assert_eq!(decoded.fields, PROBE_HEADER);
        assert_eq!(decoded.lead_in_len, crate::file::RECORD_HEADER_LEN);

        let record_offset = 0x1122_3344_5566_7789;
        let footer = decode_native_record_footer(
            PROBE_FOOTER_BYTES.as_slice(),
            record_offset + 64,
            record_offset,
        )
        .expect("captured footer decodes");
        assert_eq!(footer, PROBE_FOOTER);
    }

    /// The length contract is discharged at compile time; assert that the two
    /// published constants are what the tables actually sum to, so a reader of
    /// this module does not have to trust the `const _: ()` block alone.
    #[test]
    fn record_field_tables_sum_to_the_published_lengths() {
        assert_eq!(native_record_header_len(), crate::file::RECORD_HEADER_LEN);
        assert_eq!(native_record_footer_len(), crate::file::RECORD_FOOTER_LEN);
    }

    /// The plan's extension length must be the length the writer emits.
    ///
    /// `native_file_header_plan_extension_len` restates the writer's terms, and
    /// a term left out of it does not fail to compile — it publishes a header
    /// length short by exactly that block, which puts the first record inside
    /// it. So the two are compared directly, over every combination of the
    /// blocks that make up the region.
    #[test]
    fn the_plan_extension_length_is_the_length_the_writer_emits() {
        use crate::{
            CommitPolicy, FormatSpecBuilder, HeaderSlotIntegrity, HeaderSlots, IndexPolicy,
            LivenessPolicy, ReadLimits,
        };

        const HEADER_BLOCK: u32 = 7;
        const BLOCKS: &[crate::BlockDescriptor] = &[crate::BlockDescriptor {
            id: HEADER_BLOCK,
            name: "header_block",
            version: 1,
            kind: crate::BlockKind::Variable,
            fields: &[],
        }];
        let base = FormatSpecBuilder::new()
            .magic(b"PLNEXTLN")
            .blocks(BLOCKS)
            .read_limits(ReadLimits::TRUSTED_UNBOUNDED)
            .build()
            .expect("base spec");

        let chained = base
            .with_index_policy(IndexPolicy::BlockOffsetChain)
            .with_commit_policy(CommitPolicy::TransactionMarker(
                crate::TransactionMarkerMode::OnFlush,
            ));
        let slots = HeaderSlots::new(512, HeaderSlotIntegrity::Rolling, &[HEADER_BLOCK]);

        // Mutated only by the `integrity`-gated block below, so without that
        // feature the `mut` is genuinely unused. Annotated rather than
        // restructured, so both halves of the case list stay in one place.
        #[cfg_attr(not(feature = "integrity"), allow(unused_mut))]
        let mut cases: Vec<(&str, FormatSpec)> = vec![
            ("bare", base),
            ("header_slots", base.with_header_slots(slots)),
            (
                "liveness",
                chained.with_liveness_policy(LivenessPolicy::FooterFlags),
            ),
            (
                "liveness + header_slots",
                chained
                    .with_liveness_policy(LivenessPolicy::FooterFlags)
                    .with_header_slots(slots),
            ),
        ];

        // `header_tails` checksums its slots and so demands `IntegrityPolicy::Crc32`,
        // which is refused at create when the `integrity` feature is off. The
        // region under test demands nothing of the sort — that is what its own
        // feature-independent checksum buys — so only these two cases are gated.
        #[cfg(feature = "integrity")]
        {
            let tails = chained
                .with_integrity_policy(crate::IntegrityPolicy::Crc32)
                .with_index_policy(IndexPolicy {
                    header_tails: true,
                    ..IndexPolicy::BlockOffsetChain
                });
            cases.push(("header_tails", tails));
            cases.push((
                "all three",
                tails
                    .with_liveness_policy(LivenessPolicy::FooterFlags)
                    .with_header_slots(slots),
            ));
        }

        for (name, spec) in cases {
            spec.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
            let written = crate::file::file_header_extensions(spec)
                .unwrap_or_else(|e| panic!("{name}: {e}"))
                .len() as u64;
            assert_eq!(
                native_file_header_plan_extension_len(spec),
                written,
                "{name}: the plan and the writer disagree about the extension region",
            );
        }
    }
}
