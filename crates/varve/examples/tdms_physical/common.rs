#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::remove_file;
use std::path::{Path, PathBuf};

use varve::{
    BinaryCursor, BinaryWriter, ChunkEntry, ChunkIndexBuilder, ChunkLayout, Endian, SegmentReducer,
    TaggedValueCodec, reduce_segments_by_ref, varve_format,
};

const TDMS_VERSION: u32 = 4713;
const TDMS_TOC_METADATA: u32 = 1 << 1;
const TDMS_TOC_NEW_OBJECT_LIST: u32 = 1 << 2;
const TDMS_TOC_RAW_DATA: u32 = 1 << 3;
const TDMS_TOC_INTERLEAVED_DATA: u32 = 1 << 5;
const TDMS_RAW_INDEX_NONE: u32 = 0xFFFF_FFFF;
const TDMS_RAW_INDEX_SAME_AS_PREVIOUS: u32 = 0;
const TDMS_RAW_INDEX_LEN: u32 = 20;
const TDMS_TYPE_I64: u32 = 4;
const TDMS_TYPE_DOUBLE_FLOAT: u32 = 10;
const TDMS_TYPE_STRING: u32 = 0x20;

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

pub fn write_example(path: &Path) -> varve::Result<()> {
    cleanup(path);
    let first_metadata = tdms_metadata(&[
        TdmsObject {
            path: "/",
            raw_data_index: RawDataIndex::None,
            properties: vec![TdmsProperty {
                name: "title",
                value: TdmsPropertyValue::String("Varve TDMS adapter proof smoke".to_string()),
            }],
        },
        TdmsObject {
            path: "/'Measured Data'",
            raw_data_index: RawDataIndex::None,
            properties: Vec::new(),
        },
        TdmsObject {
            path: "/'Measured Data'/'Amplitude'",
            raw_data_index: RawDataIndex::New {
                data_type: TDMS_TYPE_DOUBLE_FLOAT,
                dimensions: 1,
                values: 4,
            },
            properties: vec![
                TdmsProperty {
                    name: "unit_string",
                    value: TdmsPropertyValue::String("V".to_string()),
                },
                TdmsProperty {
                    name: "wf_increment",
                    value: TdmsPropertyValue::F64(0.001),
                },
            ],
        },
    ])?;
    let second_metadata = tdms_metadata(&[TdmsObject {
        path: "/'Measured Data'/'Amplitude'",
        raw_data_index: RawDataIndex::New {
            data_type: TDMS_TYPE_DOUBLE_FLOAT,
            dimensions: 1,
            values: 2,
        },
        properties: vec![TdmsProperty {
            name: "sample_count",
            value: TdmsPropertyValue::I64(6),
        }],
    }])?;
    let first_raw = f64_bytes(&[0.10, 0.20, 0.30, 0.40])?;
    let second_raw = f64_bytes(&[0.50, 0.60])?;

    let mut writer = TdmsCompatFormat::create_layout_writer(path)?;
    writer.write_tdms_segment(TdmsCompatFormatTdmsSegmentLayoutWrite {
        fields: TdmsCompatFormatTdmsSegmentLayoutFields {
            toc_mask: TDMS_TOC_METADATA | TDMS_TOC_NEW_OBJECT_LIST | TDMS_TOC_RAW_DATA,
            version: TDMS_VERSION,
        },
        footer_fields: TdmsCompatFormatTdmsSegmentLayoutFooterFields,
        metadata: &first_metadata,
        raw: &first_raw,
    })?;
    writer.write_tdms_segment(TdmsCompatFormatTdmsSegmentLayoutWrite {
        fields: TdmsCompatFormatTdmsSegmentLayoutFields {
            toc_mask: TDMS_TOC_METADATA | TDMS_TOC_RAW_DATA,
            version: TDMS_VERSION,
        },
        footer_fields: TdmsCompatFormatTdmsSegmentLayoutFooterFields,
        metadata: &second_metadata,
        raw: &second_raw,
    })?;
    writer.flush()?;
    Ok(())
}

pub fn read_and_verify(path: &Path) -> varve::Result<()> {
    let reader = TdmsCompatFormat::open_layout_reader(path)?;
    let segments = reader.tdms_segments()?;
    let mut reducer_input = Vec::new();

    for (index, segment) in segments.iter().enumerate() {
        assert_eq!(segment.tag()?, b"TDSm");
        assert!(segment.toc_mask()? & TDMS_TOC_METADATA != 0);
        assert!(segment.toc_mask()? & TDMS_TOC_RAW_DATA != 0);
        assert_eq!(segment.toc_mask()? & TDMS_TOC_INTERLEAVED_DATA, 0);
        let metadata = parse_tdms_metadata(&reader.read_tdms_segment_metadata(index)?)?;
        let raw = reader.read_tdms_segment_raw(index)?;
        reducer_input.push((
            segment.as_layout_segment_info(),
            TdmsSegmentPayload { metadata, raw },
        ));
    }

    let report = reduce_segments_by_ref::<TdmsReducer, _>(reducer_input)?;
    assert_eq!(report.segments_applied, segments.len());
    let chunk_index = report.state.chunks.clone().finish()?;
    match report.state.property("/", "title") {
        TdmsPropertyValue::String(title) if title == "npTDMS multichannel smoke" => {
            assert_eq!(chunk_index.entries().len(), 4);
            assert_eq!(
                report.state.property("/'Bench'", "operator"),
                TdmsPropertyValue::String("Ada".to_string())
            );
            assert_eq!(
                report.state.property("/'Bench'/'Voltage'", "unit_string"),
                TdmsPropertyValue::String("V".to_string())
            );
            assert_eq!(
                report.state.property("/'Bench'/'Current'", "unit_string"),
                TdmsPropertyValue::String("A".to_string())
            );
            assert_eq!(
                report.state.samples("/'Bench'/'Voltage'"),
                vec![1.0, 2.0, 3.0, 4.0]
            );
            assert_eq!(
                report.state.samples("/'Bench'/'Current'"),
                vec![0.10, 0.20, 0.30, 0.40]
            );
        }
        TdmsPropertyValue::String(title) if title == "Varve TDMS adapter proof smoke" => {
            assert_eq!(chunk_index.entries().len(), 2);
            assert_eq!(
                report
                    .state
                    .property("/'Measured Data'/'Amplitude'", "unit_string"),
                TdmsPropertyValue::String("V".to_string())
            );
            assert_eq!(
                report
                    .state
                    .property("/'Measured Data'/'Amplitude'", "sample_count"),
                TdmsPropertyValue::I64(6)
            );
            assert_eq!(
                report.state.samples("/'Measured Data'/'Amplitude'"),
                vec![0.10, 0.20, 0.30, 0.40, 0.50, 0.60]
            );
        }
        other => panic!("unexpected TDMS example title {other:?}"),
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct TdmsObject<'a> {
    path: &'a str,
    raw_data_index: RawDataIndex,
    properties: Vec<TdmsProperty>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawDataIndex {
    None,
    SameAsPrevious,
    New {
        data_type: u32,
        dimensions: u32,
        values: u64,
    },
}

#[derive(Clone, Debug)]
struct TdmsProperty {
    name: &'static str,
    value: TdmsPropertyValue,
}

#[derive(Clone, Debug, PartialEq)]
enum TdmsPropertyValue {
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
            other => Err(varve::Error::AdapterUnsupportedType {
                type_id: u64::from(other),
            }),
        }
    }

    fn encode(value: &Self::Value, writer: &mut BinaryWriter) -> varve::Result<Self::TypeId> {
        match value {
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
        } => {
            writer.u32(TDMS_RAW_INDEX_LEN)?;
            writer.u32(data_type)?;
            writer.u32(dimensions)?;
            writer.u64(values)
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
            if data_type != TDMS_TYPE_DOUBLE_FLOAT || dimensions != 1 {
                return Err(varve::Error::AdapterDiagnostic(
                    "example reader supports only one-dimensional f64 raw data",
                ));
            }
            Ok(RawDataIndex::New {
                data_type,
                dimensions,
                values,
            })
        }
        other => Err(varve::Error::AdapterInvalidLength {
            value: u64::from(other),
        }),
    }
}

fn f64_bytes(values: &[f64]) -> varve::Result<Vec<u8>> {
    let mut writer = BinaryWriter::with_capacity(Endian::Little, values.len() * 8);
    writer.array_f64(values)?;
    Ok(writer.into_inner())
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
struct TdmsState {
    properties: HashMap<String, HashMap<String, TdmsPropertyValue>>,
    raw_indexes: HashMap<String, RawDataIndex>,
    samples: HashMap<String, Vec<f64>>,
    chunks: ChunkIndexBuilder<String>,
    segments_seen: usize,
}

impl TdmsState {
    fn property(&self, path: &str, name: &str) -> TdmsPropertyValue {
        self.properties
            .get(path)
            .and_then(|properties| properties.get(name))
            .cloned()
            .expect("TDMS property exists")
    }

    fn samples(&self, path: &str) -> Vec<f64> {
        self.samples.get(path).cloned().expect("TDMS samples exist")
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
            let RawDataIndex::New { values, .. } = index else {
                continue;
            };
            let byte_len = values
                .checked_mul(8)
                .ok_or(varve::Error::AdapterInvalidLength { value: values })?;
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
            state
                .samples
                .entry(path)
                .or_default()
                .extend(read_f64_values(raw_values, values as usize)?);
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

fn read_f64_values(bytes: &[u8], count: usize) -> varve::Result<Vec<f64>> {
    let mut cursor = BinaryCursor::new(bytes, Endian::Little);
    let values = cursor.array_f64(count)?;
    cursor.finish()?;
    Ok(values)
}

pub fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
