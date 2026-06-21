use std::collections::HashMap;
use std::fs::{read, remove_file, write};
use std::path::{Path, PathBuf};

use super::example_data::{
    append_tdms_channel_values, changed_tdms_channel_values, channel_tdms_objects,
    encode_channel_values, expected_chunk_count, first_tdms_channel_values, initial_tdms_objects,
    same_tdms_channel_values,
};
use varve::{
    AdapterCheckReport, AdapterCheckStatus, AdapterDiagnosticDomain, AdapterInputFile,
    AdapterTailStatus, BinaryCursor, BinaryWriter, ChunkEntry, ChunkIndexBuilder, ChunkLayout,
    Endian, SegmentReducer, SidecarIdentity, SidecarMode, SidecarPolicy, TaggedValueCodec,
    reduce_segments_by_ref, varve_format,
};

const TDMS_VERSION: u32 = 4713;
const TDMS_TOC_METADATA: u32 = 1 << 1;
const TDMS_TOC_NEW_OBJECT_LIST: u32 = 1 << 2;
const TDMS_TOC_RAW_DATA: u32 = 1 << 3;
const TDMS_TOC_INTERLEAVED_DATA: u32 = 1 << 5;
const TDMS_RAW_INDEX_NONE: u32 = 0xFFFF_FFFF;
const TDMS_RAW_INDEX_SAME_AS_PREVIOUS: u32 = 0;
const TDMS_RAW_INDEX_LEN: u32 = 20;
const TDMS_TYPE_I8: u32 = 1;
const TDMS_TYPE_I16: u32 = 2;
const TDMS_TYPE_I32: u32 = 3;
const TDMS_TYPE_I64: u32 = 4;
const TDMS_TYPE_U8: u32 = 5;
const TDMS_TYPE_U16: u32 = 6;
const TDMS_TYPE_U32: u32 = 7;
const TDMS_TYPE_U64: u32 = 8;
const TDMS_TYPE_SINGLE_FLOAT: u32 = 9;
const TDMS_TYPE_DOUBLE_FLOAT: u32 = 10;
const TDMS_TYPE_SINGLE_FLOAT_WITH_UNIT: u32 = 0x19;
const TDMS_TYPE_DOUBLE_FLOAT_WITH_UNIT: u32 = 0x1A;
const TDMS_TYPE_STRING: u32 = 0x20;
const TDMS_TYPE_BOOLEAN: u32 = 0x21;
const TDMS_TYPE_TIMESTAMP: u32 = 0x44;
const TDMS_TYPE_COMPLEX_SINGLE_FLOAT: u32 = 0x08000c;
const TDMS_TYPE_COMPLEX_DOUBLE_FLOAT: u32 = 0x10000d;
const TDMS_INDEX_SIDECAR_MAGIC: &[u8; 4] = b"VTIX";

varve_format! {
    pub format TdmsCompatFormat {
        magic: b"TDMS";
        version: 1;
        endian: little;
        schema_hash: computed;
        extension: "tdms";
        preset: none;

        layout {
            segment TdmsSegment repeat until_eof {
                lead_in TdmsLeadIn {
                    bytes tag = b"TDSm";
                    u32 toc_mask;
                    u32 version;
                    u64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    u64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata TdmsMetadata;
                raw_region TdmsRaw;
            }
        }
    }
}

pub(super) const GROUP_NAME: &str = "Measured Data";
pub(super) const GROUP_PATH: &str = "/'Measured Data'";

pub fn write_example(path: &Path) -> varve::Result<()> {
    cleanup(path);
    let first_channels = first_tdms_channel_values(true);
    let second_channels = changed_tdms_channel_values(true);
    let third_channels = same_tdms_channel_values(true);
    let first_metadata = tdms_metadata(&initial_tdms_objects(&first_channels)?)?;
    let second_metadata = tdms_metadata(&channel_tdms_objects(
        &second_channels,
        RawIndexMode::New,
        3,
        None,
    )?)?;
    let third_metadata = tdms_metadata(&channel_tdms_objects(
        &third_channels,
        RawIndexMode::SameAsPrevious,
        4,
        Some("same raw index reused"),
    )?)?;
    let first_raw = encode_channel_values(&first_channels)?;
    let second_raw = encode_channel_values(&second_channels)?;
    let third_raw = encode_channel_values(&third_channels)?;

    let mut writer = TdmsCompatFormat::create_layout_writer(path)?;
    let mut segment_count = 0u32;
    writer.write_tdms_segment(TdmsCompatFormatTdmsSegmentLayoutWrite {
        fields: TdmsCompatFormatTdmsSegmentLayoutFields {
            toc_mask: TDMS_TOC_METADATA | TDMS_TOC_NEW_OBJECT_LIST | TDMS_TOC_RAW_DATA,
            version: TDMS_VERSION,
        },
        footer_fields: TdmsCompatFormatTdmsSegmentLayoutFooterFields,
        metadata: &first_metadata,
        raw: &first_raw,
    })?;
    segment_count += 1;
    writer.write_tdms_segment(TdmsCompatFormatTdmsSegmentLayoutWrite {
        fields: TdmsCompatFormatTdmsSegmentLayoutFields {
            toc_mask: TDMS_TOC_METADATA | TDMS_TOC_RAW_DATA,
            version: TDMS_VERSION,
        },
        footer_fields: TdmsCompatFormatTdmsSegmentLayoutFooterFields,
        metadata: &second_metadata,
        raw: &second_raw,
    })?;
    segment_count += 1;
    writer.write_tdms_segment(TdmsCompatFormatTdmsSegmentLayoutWrite {
        fields: TdmsCompatFormatTdmsSegmentLayoutFields {
            toc_mask: TDMS_TOC_METADATA | TDMS_TOC_RAW_DATA,
            version: TDMS_VERSION,
        },
        footer_fields: TdmsCompatFormatTdmsSegmentLayoutFooterFields,
        metadata: &third_metadata,
        raw: &third_raw,
    })?;
    segment_count += 1;
    writer.flush()?;
    write_index_sidecar(
        path,
        segment_count,
        expected_chunk_count(false, false) as u32,
    )?;
    Ok(())
}

pub fn append_example(path: &Path) -> varve::Result<()> {
    let report = load_tdms_state(path)?;
    let (metadata, raw, added_chunks) = match report.state.property("/", "title") {
        TdmsPropertyValue::String(title)
            if title == "Varve TDMS adapter proof smoke"
                || title == "npTDMS scalar type matrix smoke" =>
        {
            let include_unit_channels = title == "Varve TDMS adapter proof smoke";
            let append_channels = append_tdms_channel_values(include_unit_channels);
            let metadata = tdms_metadata(&channel_tdms_objects(
                &append_channels,
                RawIndexMode::SameAsPrevious,
                5,
                None,
            )?)?;
            (
                metadata,
                encode_channel_values(&append_channels)?,
                append_channels.len() as u32,
            )
        }
        other => panic!("unexpected TDMS example title for append {other:?}"),
    };

    let mut writer = TdmsCompatFormat::open_layout_writer(path)?;
    writer.write_tdms_segment(TdmsCompatFormatTdmsSegmentLayoutWrite {
        fields: TdmsCompatFormatTdmsSegmentLayoutFields {
            toc_mask: TDMS_TOC_METADATA | TDMS_TOC_RAW_DATA,
            version: report.append_version,
        },
        footer_fields: TdmsCompatFormatTdmsSegmentLayoutFooterFields,
        metadata: &metadata,
        raw: &raw,
    })?;
    writer.flush()?;

    write_index_sidecar(
        path,
        (report.segments_applied + 1) as u32,
        report.chunk_count as u32 + added_chunks,
    )?;
    Ok(())
}

pub fn inspect_example(path: &Path) -> varve::Result<AdapterCheckReport> {
    let input = AdapterInputFile::from_path(path);
    let layout_report = TdmsCompatFormat::inspect_layout_file_report(input.path())?;
    let mut report = AdapterCheckReport::from_layout_report(&layout_report);

    if let Some(tail) = report.physical_tail.clone() {
        let tail = AdapterTailStatus::new(
            tail,
            None,
            Some("custom TDMS-style physical scan did not reach EOF".to_string()),
        );
        report.push(
            AdapterCheckStatus::Warning,
            AdapterDiagnosticDomain::PhysicalLayout,
            format!(
                "adapter tail evidence: {} bytes remain after offset {}",
                tail.available_len, tail.tail.offset
            ),
        );
    }

    let expected = SidecarIdentity::from_main_file(input.path())?;
    let sidecar_policy = tdms_sidecar_policy();
    let sidecar_report = sidecar_policy.inspect(input.path(), Some(&expected))?;
    if sidecar_report.present {
        match read_index_sidecar(input.path()) {
            Ok(sidecar) if sidecar.matches(&expected, layout_report.segments.len() as u32) => {
                report.push(
                    AdapterCheckStatus::Passed,
                    AdapterDiagnosticDomain::Sidecar,
                    format!(
                        "TDMS example sidecar {} covers {} segments and {} chunks",
                        sidecar_report.path.display(),
                        sidecar.segment_count,
                        sidecar.chunk_count
                    ),
                );
            }
            Ok(_) => report.push(
                AdapterCheckStatus::Failed,
                AdapterDiagnosticDomain::Sidecar,
                format!(
                    "TDMS example sidecar {} is stale for the main file",
                    sidecar_report.path.display()
                ),
            ),
            Err(error) => report.push(
                AdapterCheckStatus::Failed,
                AdapterDiagnosticDomain::Sidecar,
                format!(
                    "TDMS example sidecar {} could not be parsed: {error}",
                    sidecar_report.path.display()
                ),
            ),
        }
    } else {
        report.push(
            sidecar_report.status,
            AdapterDiagnosticDomain::Sidecar,
            format!(
                "optional TDMS example sidecar {} is not present",
                sidecar_report.path.display()
            ),
        );
    }

    Ok(report)
}

pub(super) fn load_tdms_state(path: &Path) -> varve::Result<TdmsReadReport> {
    let reader = TdmsCompatFormat::open_layout_reader(path)?;
    let segments = reader.tdms_segments()?;
    let mut reducer_input = Vec::new();
    let mut append_version = TDMS_VERSION;

    for (index, segment) in segments.iter().enumerate() {
        assert_eq!(segment.tag()?, b"TDSm");
        assert!(segment.toc_mask()? & TDMS_TOC_METADATA != 0);
        assert!(segment.toc_mask()? & TDMS_TOC_RAW_DATA != 0);
        assert_eq!(segment.toc_mask()? & TDMS_TOC_INTERLEAVED_DATA, 0);
        append_version = segment.version()?;
        let metadata = parse_tdms_metadata(&reader.read_tdms_segment_metadata(index)?)?;
        let raw = reader.read_tdms_segment_raw(index)?;
        reducer_input.push((
            segment.as_layout_segment_info(),
            TdmsSegmentPayload { metadata, raw },
        ));
    }

    let report = reduce_segments_by_ref::<TdmsReducer, _>(reducer_input)?;
    assert_eq!(report.segments_applied, segments.len());
    let chunk_count = report.state.chunks.clone().finish()?.entries().len();
    Ok(TdmsReadReport {
        state: report.state,
        segments_applied: report.segments_applied,
        chunk_count,
        append_version,
    })
}

#[derive(Debug)]
pub(super) struct TdmsReadReport {
    pub(super) state: TdmsState,
    pub(super) segments_applied: usize,
    pub(super) chunk_count: usize,
    pub(super) append_version: u32,
}

#[derive(Clone, Debug)]
pub(super) struct TdmsObject<'a> {
    pub(super) path: &'a str,
    pub(super) raw_data_index: RawDataIndex,
    pub(super) properties: Vec<TdmsProperty>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RawDataIndex {
    None,
    SameAsPrevious,
    New {
        data_type: u32,
        dimensions: u32,
        values: u64,
        byte_len: Option<u64>,
    },
}

#[derive(Clone, Debug)]
pub(super) struct TdmsProperty {
    pub(super) name: &'static str,
    pub(super) value: TdmsPropertyValue,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum TdmsPropertyValue {
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
}

struct TdmsPropertyCodec;

impl TaggedValueCodec for TdmsPropertyCodec {
    type Value = TdmsPropertyValue;
    type TypeId = u32;

    fn decode(type_id: Self::TypeId, cursor: &mut BinaryCursor<'_>) -> varve::Result<Self::Value> {
        match type_id {
            TDMS_TYPE_I64 => Ok(TdmsPropertyValue::I64(cursor.i64()?)),
            TDMS_TYPE_DOUBLE_FLOAT => Ok(TdmsPropertyValue::F64(cursor.f64()?)),
            TDMS_TYPE_STRING => Ok(TdmsPropertyValue::String(
                cursor.len_prefixed_string::<u32>()?,
            )),
            TDMS_TYPE_BOOLEAN => Ok(TdmsPropertyValue::Bool(cursor.u8()? != 0)),
            other => Err(varve::Error::AdapterUnsupportedType {
                type_id: u64::from(other),
            }),
        }
    }

    fn encode(value: &Self::Value, writer: &mut BinaryWriter) -> varve::Result<Self::TypeId> {
        match value {
            TdmsPropertyValue::Bool(value) => {
                writer.u8(u8::from(*value))?;
                Ok(TDMS_TYPE_BOOLEAN)
            }
            TdmsPropertyValue::I64(value) => {
                writer.i64(*value)?;
                Ok(TDMS_TYPE_I64)
            }
            TdmsPropertyValue::F64(value) => {
                writer.f64(*value)?;
                Ok(TDMS_TYPE_DOUBLE_FLOAT)
            }
            TdmsPropertyValue::String(value) => {
                writer.len_prefixed_string::<u32>(value)?;
                Ok(TDMS_TYPE_STRING)
            }
        }
    }
}

fn tdms_metadata(objects: &[TdmsObject<'_>]) -> varve::Result<Vec<u8>> {
    let mut writer = BinaryWriter::new(Endian::Little);
    writer.u32(objects.len() as u32)?;
    for object in objects {
        writer.len_prefixed_string::<u32>(object.path)?;
        write_raw_data_index(&mut writer, object.raw_data_index)?;
        writer.u32(object.properties.len() as u32)?;
        for property in &object.properties {
            writer.len_prefixed_string::<u32>(property.name)?;
            let mut payload = BinaryWriter::new(Endian::Little);
            let type_id = TdmsPropertyCodec::encode(&property.value, &mut payload)?;
            writer.u32(type_id)?;
            writer.bytes(payload.as_slice())?;
        }
    }
    Ok(writer.into_inner())
}

fn write_raw_data_index(writer: &mut BinaryWriter, index: RawDataIndex) -> varve::Result<()> {
    match index {
        RawDataIndex::None => writer.u32(TDMS_RAW_INDEX_NONE),
        RawDataIndex::SameAsPrevious => writer.u32(TDMS_RAW_INDEX_SAME_AS_PREVIOUS),
        RawDataIndex::New {
            data_type,
            dimensions,
            values,
            byte_len,
        } => {
            writer.u32(TDMS_RAW_INDEX_LEN)?;
            writer.u32(data_type)?;
            writer.u32(dimensions)?;
            writer.u64(values)?;
            if data_type == TDMS_TYPE_STRING {
                writer.u64(byte_len.ok_or(varve::Error::AdapterDiagnostic(
                    "TDMS string raw index requires an explicit byte length",
                ))?)?;
            }
            Ok(())
        }
    }
}

fn parse_tdms_metadata(bytes: &[u8]) -> varve::Result<TdmsMetadata> {
    let mut cursor = BinaryCursor::new(bytes, Endian::Little);
    let object_count = cursor.u32()?;
    let mut objects = Vec::new();
    for _ in 0..object_count {
        let path = cursor.len_prefixed_string::<u32>()?;
        let raw_data_index = read_raw_data_index(&mut cursor)?;
        let property_count = cursor.u32()?;
        let mut properties = HashMap::new();
        for _ in 0..property_count {
            let name = cursor.len_prefixed_string::<u32>()?;
            let type_id = cursor.u32()?;
            let value = TdmsPropertyCodec::decode(type_id, &mut cursor)?;
            properties.insert(name, value);
        }
        objects.push(TdmsDecodedObject {
            path,
            raw_data_index,
            properties,
        });
    }
    cursor.finish()?;
    Ok(TdmsMetadata { objects })
}

fn read_raw_data_index(cursor: &mut BinaryCursor<'_>) -> varve::Result<RawDataIndex> {
    match cursor.u32()? {
        TDMS_RAW_INDEX_NONE => Ok(RawDataIndex::None),
        TDMS_RAW_INDEX_SAME_AS_PREVIOUS => Ok(RawDataIndex::SameAsPrevious),
        TDMS_RAW_INDEX_LEN => {
            let data_type = cursor.u32()?;
            let dimensions = cursor.u32()?;
            let values = cursor.u64()?;
            if !is_supported_tdms_channel_type(data_type) || dimensions != 1 {
                return Err(varve::Error::AdapterDiagnostic(
                    "example reader supports only one-dimensional non-DAQmx TDMS raw channel data",
                ));
            }
            let byte_len = if data_type == TDMS_TYPE_STRING {
                Some(cursor.u64()?)
            } else {
                None
            };
            Ok(RawDataIndex::New {
                data_type,
                dimensions,
                values,
                byte_len,
            })
        }
        other => Err(varve::Error::AdapterInvalidLength {
            value: u64::from(other),
        }),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TdmsTimestampValue {
    pub(super) second_fractions: u64,
    pub(super) seconds: i64,
}

#[derive(Clone, Debug)]
pub(super) struct TdmsChannelValues {
    pub(super) name: &'static str,
    pub(super) values: TdmsRawValues,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RawIndexMode {
    New,
    SameAsPrevious,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum TdmsRawValues {
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    F32WithUnit(Vec<f32>),
    F64WithUnit(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
    Timestamp(Vec<TdmsTimestampValue>),
    ComplexF32(Vec<(f32, f32)>),
    ComplexF64(Vec<(f64, f64)>),
}

impl TdmsRawValues {
    fn data_type(&self) -> u32 {
        match self {
            Self::I8(_) => TDMS_TYPE_I8,
            Self::I16(_) => TDMS_TYPE_I16,
            Self::I32(_) => TDMS_TYPE_I32,
            Self::I64(_) => TDMS_TYPE_I64,
            Self::U8(_) => TDMS_TYPE_U8,
            Self::U16(_) => TDMS_TYPE_U16,
            Self::U32(_) => TDMS_TYPE_U32,
            Self::U64(_) => TDMS_TYPE_U64,
            Self::F32(_) => TDMS_TYPE_SINGLE_FLOAT,
            Self::F64(_) => TDMS_TYPE_DOUBLE_FLOAT,
            Self::F32WithUnit(_) => TDMS_TYPE_SINGLE_FLOAT_WITH_UNIT,
            Self::F64WithUnit(_) => TDMS_TYPE_DOUBLE_FLOAT_WITH_UNIT,
            Self::Bool(_) => TDMS_TYPE_BOOLEAN,
            Self::String(_) => TDMS_TYPE_STRING,
            Self::Timestamp(_) => TDMS_TYPE_TIMESTAMP,
            Self::ComplexF32(_) => TDMS_TYPE_COMPLEX_SINGLE_FLOAT,
            Self::ComplexF64(_) => TDMS_TYPE_COMPLEX_DOUBLE_FLOAT,
        }
    }

    fn value_count(&self) -> u64 {
        (match self {
            Self::I8(values) => values.len(),
            Self::I16(values) => values.len(),
            Self::I32(values) => values.len(),
            Self::I64(values) => values.len(),
            Self::U8(values) => values.len(),
            Self::U16(values) => values.len(),
            Self::U32(values) => values.len(),
            Self::U64(values) => values.len(),
            Self::F32(values) => values.len(),
            Self::F64(values) => values.len(),
            Self::F32WithUnit(values) => values.len(),
            Self::F64WithUnit(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
            Self::Timestamp(values) => values.len(),
            Self::ComplexF32(values) => values.len(),
            Self::ComplexF64(values) => values.len(),
        }) as u64
    }

    pub(super) fn raw_index(&self) -> varve::Result<RawDataIndex> {
        let data_type = self.data_type();
        let byte_len = if data_type == TDMS_TYPE_STRING {
            Some(self.encode()?.len() as u64)
        } else {
            None
        };
        Ok(RawDataIndex::New {
            data_type,
            dimensions: 1,
            values: self.value_count(),
            byte_len,
        })
    }

    pub(super) fn encode(&self) -> varve::Result<Vec<u8>> {
        let mut writer = BinaryWriter::new(Endian::Little);
        match self {
            Self::I8(values) => {
                for value in values {
                    writer.i8(*value)?;
                }
            }
            Self::I16(values) => {
                for value in values {
                    writer.i16(*value)?;
                }
            }
            Self::I32(values) => {
                for value in values {
                    writer.i32(*value)?;
                }
            }
            Self::I64(values) => {
                for value in values {
                    writer.i64(*value)?;
                }
            }
            Self::U8(values) => {
                for value in values {
                    writer.u8(*value)?;
                }
            }
            Self::U16(values) => {
                for value in values {
                    writer.u16(*value)?;
                }
            }
            Self::U32(values) => {
                for value in values {
                    writer.u32(*value)?;
                }
            }
            Self::U64(values) => {
                for value in values {
                    writer.u64(*value)?;
                }
            }
            Self::F32(values) => {
                for value in values {
                    writer.f32(*value)?;
                }
            }
            Self::F64(values) => writer.array_f64(values)?,
            Self::F32WithUnit(values) => {
                for value in values {
                    writer.f32(*value)?;
                }
            }
            Self::F64WithUnit(values) => writer.array_f64(values)?,
            Self::Bool(values) => {
                for value in values {
                    writer.u8(u8::from(*value))?;
                }
            }
            Self::String(values) => {
                let encoded: Vec<_> = values.iter().map(|value| value.as_bytes()).collect();
                let mut offset = 0u32;
                for value in &encoded {
                    offset = offset
                        .checked_add(value.len() as u32)
                        .ok_or(varve::Error::LengthOverflow { value: u64::MAX })?;
                    writer.u32(offset)?;
                }
                for value in encoded {
                    writer.bytes(value)?;
                }
            }
            Self::Timestamp(values) => {
                for value in values {
                    writer.u64(value.second_fractions)?;
                    writer.i64(value.seconds)?;
                }
            }
            Self::ComplexF32(values) => {
                for (real, imaginary) in values {
                    writer.f32(*real)?;
                    writer.f32(*imaginary)?;
                }
            }
            Self::ComplexF64(values) => {
                for (real, imaginary) in values {
                    writer.f64(*real)?;
                    writer.f64(*imaginary)?;
                }
            }
        }
        Ok(writer.into_inner())
    }

    fn decode(data_type: u32, bytes: &[u8], count: usize) -> varve::Result<Self> {
        let mut cursor = BinaryCursor::new(bytes, Endian::Little);
        let values = match data_type {
            TDMS_TYPE_I8 => Self::I8(
                (0..count)
                    .map(|_| cursor.i8())
                    .collect::<varve::Result<_>>()?,
            ),
            TDMS_TYPE_I16 => Self::I16(
                (0..count)
                    .map(|_| cursor.i16())
                    .collect::<varve::Result<_>>()?,
            ),
            TDMS_TYPE_I32 => Self::I32(
                (0..count)
                    .map(|_| cursor.i32())
                    .collect::<varve::Result<_>>()?,
            ),
            TDMS_TYPE_I64 => Self::I64(
                (0..count)
                    .map(|_| cursor.i64())
                    .collect::<varve::Result<_>>()?,
            ),
            TDMS_TYPE_U8 => Self::U8(
                (0..count)
                    .map(|_| cursor.u8())
                    .collect::<varve::Result<_>>()?,
            ),
            TDMS_TYPE_U16 => Self::U16(
                (0..count)
                    .map(|_| cursor.u16())
                    .collect::<varve::Result<_>>()?,
            ),
            TDMS_TYPE_U32 => Self::U32(
                (0..count)
                    .map(|_| cursor.u32())
                    .collect::<varve::Result<_>>()?,
            ),
            TDMS_TYPE_U64 => Self::U64(
                (0..count)
                    .map(|_| cursor.u64())
                    .collect::<varve::Result<_>>()?,
            ),
            TDMS_TYPE_SINGLE_FLOAT => Self::F32(
                (0..count)
                    .map(|_| cursor.f32())
                    .collect::<varve::Result<_>>()?,
            ),
            TDMS_TYPE_DOUBLE_FLOAT => Self::F64(cursor.array_f64(count)?),
            TDMS_TYPE_SINGLE_FLOAT_WITH_UNIT => Self::F32WithUnit(
                (0..count)
                    .map(|_| cursor.f32())
                    .collect::<varve::Result<_>>()?,
            ),
            TDMS_TYPE_DOUBLE_FLOAT_WITH_UNIT => Self::F64WithUnit(cursor.array_f64(count)?),
            TDMS_TYPE_BOOLEAN => Self::Bool(
                (0..count)
                    .map(|_| Ok(cursor.u8()? != 0))
                    .collect::<varve::Result<_>>()?,
            ),
            TDMS_TYPE_STRING => {
                let mut offsets = Vec::with_capacity(count);
                for _ in 0..count {
                    offsets.push(cursor.u32()? as usize);
                }
                let data = cursor.bytes(cursor.remaining())?;
                let mut start = 0usize;
                let mut strings = Vec::with_capacity(count);
                for end in offsets {
                    let bytes = data.get(start..end).ok_or(varve::Error::AdapterBounds {
                        offset: start as u64,
                        len: end.saturating_sub(start) as u64,
                        available: data.len() as u64,
                    })?;
                    strings.push(
                        String::from_utf8(bytes.to_vec()).map_err(|_| varve::Error::InvalidUtf8)?,
                    );
                    start = end;
                }
                if start != data.len() {
                    return Err(varve::Error::TrailingBytes {
                        remaining: data.len() - start,
                    });
                }
                Self::String(strings)
            }
            TDMS_TYPE_TIMESTAMP => {
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    values.push(TdmsTimestampValue {
                        second_fractions: cursor.u64()?,
                        seconds: cursor.i64()?,
                    });
                }
                Self::Timestamp(values)
            }
            TDMS_TYPE_COMPLEX_SINGLE_FLOAT => {
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    values.push((cursor.f32()?, cursor.f32()?));
                }
                Self::ComplexF32(values)
            }
            TDMS_TYPE_COMPLEX_DOUBLE_FLOAT => {
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    values.push((cursor.f64()?, cursor.f64()?));
                }
                Self::ComplexF64(values)
            }
            other => {
                return Err(varve::Error::AdapterUnsupportedType {
                    type_id: u64::from(other),
                });
            }
        };
        cursor.finish()?;
        Ok(values)
    }

    pub(super) fn extend_with(&mut self, values: Self) -> varve::Result<()> {
        match (self, values) {
            (Self::I8(current), Self::I8(next)) => current.extend(next),
            (Self::I16(current), Self::I16(next)) => current.extend(next),
            (Self::I32(current), Self::I32(next)) => current.extend(next),
            (Self::I64(current), Self::I64(next)) => current.extend(next),
            (Self::U8(current), Self::U8(next)) => current.extend(next),
            (Self::U16(current), Self::U16(next)) => current.extend(next),
            (Self::U32(current), Self::U32(next)) => current.extend(next),
            (Self::U64(current), Self::U64(next)) => current.extend(next),
            (Self::F32(current), Self::F32(next)) => current.extend(next),
            (Self::F64(current), Self::F64(next)) => current.extend(next),
            (Self::F32WithUnit(current), Self::F32WithUnit(next)) => current.extend(next),
            (Self::F64WithUnit(current), Self::F64WithUnit(next)) => current.extend(next),
            (Self::Bool(current), Self::Bool(next)) => current.extend(next),
            (Self::String(current), Self::String(next)) => current.extend(next),
            (Self::Timestamp(current), Self::Timestamp(next)) => current.extend(next),
            (Self::ComplexF32(current), Self::ComplexF32(next)) => current.extend(next),
            (Self::ComplexF64(current), Self::ComplexF64(next)) => current.extend(next),
            _ => {
                return Err(varve::Error::AdapterDiagnostic(
                    "TDMS channel changed raw type across chunks",
                ));
            }
        }
        Ok(())
    }
}

fn is_supported_tdms_channel_type(data_type: u32) -> bool {
    matches!(
        data_type,
        TDMS_TYPE_I8
            | TDMS_TYPE_I16
            | TDMS_TYPE_I32
            | TDMS_TYPE_I64
            | TDMS_TYPE_U8
            | TDMS_TYPE_U16
            | TDMS_TYPE_U32
            | TDMS_TYPE_U64
            | TDMS_TYPE_SINGLE_FLOAT
            | TDMS_TYPE_DOUBLE_FLOAT
            | TDMS_TYPE_SINGLE_FLOAT_WITH_UNIT
            | TDMS_TYPE_DOUBLE_FLOAT_WITH_UNIT
            | TDMS_TYPE_STRING
            | TDMS_TYPE_BOOLEAN
            | TDMS_TYPE_TIMESTAMP
            | TDMS_TYPE_COMPLEX_SINGLE_FLOAT
            | TDMS_TYPE_COMPLEX_DOUBLE_FLOAT
    )
}

fn tdms_type_byte_len(data_type: u32, values: u64, byte_len: Option<u64>) -> varve::Result<u64> {
    if data_type == TDMS_TYPE_STRING {
        return byte_len.ok_or(varve::Error::AdapterDiagnostic(
            "TDMS string raw index is missing its total byte length",
        ));
    }

    let size = match data_type {
        TDMS_TYPE_I8 | TDMS_TYPE_U8 | TDMS_TYPE_BOOLEAN => 1,
        TDMS_TYPE_I16 | TDMS_TYPE_U16 => 2,
        TDMS_TYPE_I32
        | TDMS_TYPE_U32
        | TDMS_TYPE_SINGLE_FLOAT
        | TDMS_TYPE_SINGLE_FLOAT_WITH_UNIT => 4,
        TDMS_TYPE_I64
        | TDMS_TYPE_U64
        | TDMS_TYPE_DOUBLE_FLOAT
        | TDMS_TYPE_DOUBLE_FLOAT_WITH_UNIT
        | TDMS_TYPE_COMPLEX_SINGLE_FLOAT => 8,
        TDMS_TYPE_TIMESTAMP | TDMS_TYPE_COMPLEX_DOUBLE_FLOAT => 16,
        other => {
            return Err(varve::Error::AdapterUnsupportedType {
                type_id: u64::from(other),
            });
        }
    };
    values
        .checked_mul(size)
        .ok_or(varve::Error::AdapterInvalidLength { value: values })
}

pub(super) fn channel_path(name: &str) -> &'static str {
    match name {
        "Int8" => "/'Measured Data'/'Int8'",
        "Int16" => "/'Measured Data'/'Int16'",
        "Int32" => "/'Measured Data'/'Int32'",
        "Int64" => "/'Measured Data'/'Int64'",
        "Uint8" => "/'Measured Data'/'Uint8'",
        "Uint16" => "/'Measured Data'/'Uint16'",
        "Uint32" => "/'Measured Data'/'Uint32'",
        "Uint64" => "/'Measured Data'/'Uint64'",
        "Float32" => "/'Measured Data'/'Float32'",
        "Float64" => "/'Measured Data'/'Float64'",
        "Float32Unit" => "/'Measured Data'/'Float32Unit'",
        "Float64Unit" => "/'Measured Data'/'Float64Unit'",
        "Boolean" => "/'Measured Data'/'Boolean'",
        "String" => "/'Measured Data'/'String'",
        "Timestamp" => "/'Measured Data'/'Timestamp'",
        "Complex64" => "/'Measured Data'/'Complex64'",
        "Complex128" => "/'Measured Data'/'Complex128'",
        _ => panic!("unknown TDMS example channel {name}"),
    }
}

fn tdms_sidecar_policy() -> SidecarPolicy {
    SidecarPolicy {
        extension: "vtidx",
        mode: SidecarMode::Optional,
        verify_main_len: true,
        verify_main_fingerprint: true,
    }
}

fn write_index_sidecar(path: &Path, segment_count: u32, chunk_count: u32) -> varve::Result<()> {
    let identity = SidecarIdentity::from_main_file(path)?;
    let mut writer = BinaryWriter::new(Endian::Little);
    writer.bytes(TDMS_INDEX_SIDECAR_MAGIC)?;
    writer.u16(1)?;
    writer.u16(0)?;
    writer.u64(identity.main_len)?;
    writer.u64(identity.main_fingerprint)?;
    writer.u32(segment_count)?;
    writer.u32(chunk_count)?;
    write(tdms_sidecar_policy().sidecar_path(path), writer.as_slice())?;
    Ok(())
}

#[derive(Clone, Debug)]
struct TdmsIndexSidecar {
    main_len: u64,
    main_fingerprint: u64,
    segment_count: u32,
    chunk_count: u32,
}

impl TdmsIndexSidecar {
    fn matches(&self, identity: &SidecarIdentity, segment_count: u32) -> bool {
        self.main_len == identity.main_len
            && self.main_fingerprint == identity.main_fingerprint
            && self.segment_count == segment_count
    }
}

fn read_index_sidecar(path: &Path) -> varve::Result<TdmsIndexSidecar> {
    let bytes = read(tdms_sidecar_policy().sidecar_path(path))?;
    let mut cursor = BinaryCursor::new(&bytes, Endian::Little);
    if cursor.bytes(4)? != &TDMS_INDEX_SIDECAR_MAGIC[..] {
        return Err(varve::Error::AdapterDiagnostic(
            "TDMS example sidecar magic mismatch",
        ));
    }
    let version = cursor.u16()?;
    let _flags = cursor.u16()?;
    if version != 1 {
        return Err(varve::Error::AdapterInvalidLength {
            value: u64::from(version),
        });
    }
    let sidecar = TdmsIndexSidecar {
        main_len: cursor.u64()?,
        main_fingerprint: cursor.u64()?,
        segment_count: cursor.u32()?,
        chunk_count: cursor.u32()?,
    };
    cursor.finish()?;
    Ok(sidecar)
}

#[derive(Clone, Debug)]
struct TdmsMetadata {
    objects: Vec<TdmsDecodedObject>,
}

#[derive(Clone, Debug)]
struct TdmsDecodedObject {
    path: String,
    raw_data_index: RawDataIndex,
    properties: HashMap<String, TdmsPropertyValue>,
}

#[derive(Clone, Debug)]
struct TdmsSegmentPayload {
    metadata: TdmsMetadata,
    raw: Vec<u8>,
}

#[derive(Debug, Default)]
pub(super) struct TdmsState {
    properties: HashMap<String, HashMap<String, TdmsPropertyValue>>,
    raw_indexes: HashMap<String, RawDataIndex>,
    raw_values: HashMap<String, TdmsRawValues>,
    chunks: ChunkIndexBuilder<String>,
    segments_seen: usize,
}

impl TdmsState {
    pub(super) fn property_opt(&self, path: &str, name: &str) -> Option<TdmsPropertyValue> {
        self.properties
            .get(path)
            .and_then(|properties| properties.get(name))
            .cloned()
    }

    pub(super) fn property(&self, path: &str, name: &str) -> TdmsPropertyValue {
        self.property_opt(path, name).expect("TDMS property exists")
    }

    pub(super) fn raw_values(&self, path: &str) -> TdmsRawValues {
        self.raw_values
            .get(path)
            .cloned()
            .expect("TDMS raw values exist")
    }
}

struct TdmsReducer;

impl SegmentReducer for TdmsReducer {
    type Metadata = TdmsSegmentPayload;
    type State = TdmsState;

    fn initial() -> Self::State {
        TdmsState::default()
    }

    fn apply_segment(
        state: &mut Self::State,
        segment: &varve::LayoutSegmentInfo,
        payload: Self::Metadata,
    ) -> varve::Result<()> {
        let segment_index = state.segments_seen;
        state.segments_seen += 1;
        let mut raw_order = Vec::new();
        for object in payload.metadata.objects {
            state
                .properties
                .entry(object.path.clone())
                .or_default()
                .extend(object.properties);
            match object.raw_data_index {
                RawDataIndex::None => {}
                RawDataIndex::SameAsPrevious => {
                    if let Some(index) = state.raw_indexes.get(&object.path) {
                        raw_order.push((object.path, *index));
                    }
                }
                index => {
                    state.raw_indexes.insert(object.path.clone(), index);
                    raw_order.push((object.path, index));
                }
            }
        }

        let mut byte_offset = 0u64;
        for (path, index) in raw_order {
            let RawDataIndex::New {
                data_type,
                values,
                byte_len,
                ..
            } = index
            else {
                continue;
            };
            let byte_len = tdms_type_byte_len(data_type, values, byte_len)?;
            let end = byte_offset
                .checked_add(byte_len)
                .ok_or(varve::Error::AdapterBounds {
                    offset: byte_offset,
                    len: byte_len,
                    available: payload.raw.len() as u64,
                })?;
            let raw_values = payload.raw.get(byte_offset as usize..end as usize).ok_or(
                varve::Error::AdapterBounds {
                    offset: byte_offset,
                    len: byte_len,
                    available: payload.raw.len() as u64,
                },
            )?;
            state.chunks.push(
                ChunkEntry {
                    key: path.clone(),
                    segment_index,
                    byte_offset,
                    byte_len,
                    value_count: values,
                    layout: ChunkLayout::Contiguous,
                },
                segment,
            )?;
            let decoded = TdmsRawValues::decode(data_type, raw_values, values as usize)?;
            if let Some(current) = state.raw_values.get_mut(&path) {
                current.extend_with(decoded)?;
            } else {
                state.raw_values.insert(path, decoded);
            }
            byte_offset = end;
        }
        if byte_offset != payload.raw.len() as u64 {
            return Err(varve::Error::AdapterBounds {
                offset: byte_offset,
                len: payload.raw.len() as u64 - byte_offset,
                available: payload.raw.len() as u64,
            });
        }
        Ok(())
    }
}

pub fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let _ = remove_file(tdms_sidecar_policy().sidecar_path(path));
    let mut tdms_index = path.as_os_str().to_os_string();
    tdms_index.push("_index");
    let _ = remove_file(PathBuf::from(tdms_index));
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
