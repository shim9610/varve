use std::env;
use std::fs::remove_file;
use std::path::PathBuf;

use varve::varve_format;

const TDMS_VERSION: u32 = 4713;
const TDMS_TOC_METADATA: u32 = 1 << 1;
const TDMS_TOC_NEW_OBJECT_LIST: u32 = 1 << 2;
const TDMS_TOC_RAW_DATA: u32 = 1 << 3;
const TDMS_RAW_INDEX_NONE: u32 = 0xFFFF_FFFF;
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

fn main() -> varve::Result<()> {
    let path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("varve-tdms-compat.tdms"));
    cleanup(&path);

    let first_metadata = tdms_metadata(&[
        TdmsObject {
            path: "/",
            raw_data_index: RawDataIndex::None,
            properties: vec![TdmsProperty::String {
                name: "title",
                value: "Varve TDMS adapter proof smoke",
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
                TdmsProperty::String {
                    name: "unit_string",
                    value: "V",
                },
                TdmsProperty::F64 {
                    name: "wf_increment",
                    value: 0.001,
                },
            ],
        },
    ]);
    let second_metadata = tdms_metadata(&[TdmsObject {
        path: "/'Measured Data'/'Amplitude'",
        raw_data_index: RawDataIndex::New {
            data_type: TDMS_TYPE_DOUBLE_FLOAT,
            dimensions: 1,
            values: 2,
        },
        properties: vec![TdmsProperty::I64 {
            name: "sample_count",
            value: 6,
        }],
    }]);
    let first_raw = f64_bytes(&[0.10, 0.20, 0.30, 0.40]);
    let second_raw = f64_bytes(&[0.50, 0.60]);

    let mut writer = TdmsCompatFormat::create_layout_writer(&path)?;
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

    println!("{}", path.display());
    Ok(())
}

#[derive(Clone, Debug)]
struct TdmsObject<'a> {
    path: &'a str,
    raw_data_index: RawDataIndex,
    properties: Vec<TdmsProperty<'a>>,
}

#[derive(Clone, Copy, Debug)]
enum RawDataIndex {
    None,
    New {
        data_type: u32,
        dimensions: u32,
        values: u64,
    },
}

#[derive(Clone, Debug)]
enum TdmsProperty<'a> {
    String { name: &'a str, value: &'a str },
    I64 { name: &'a str, value: i64 },
    F64 { name: &'a str, value: f64 },
}

fn tdms_metadata(objects: &[TdmsObject<'_>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_u32(&mut bytes, objects.len() as u32);
    for object in objects {
        push_string(&mut bytes, object.path);
        push_raw_data_index(&mut bytes, object.raw_data_index);
        push_u32(&mut bytes, object.properties.len() as u32);
        for property in &object.properties {
            match property {
                TdmsProperty::String { name, value } => {
                    push_string(&mut bytes, name);
                    push_u32(&mut bytes, TDMS_TYPE_STRING);
                    push_string(&mut bytes, value);
                }
                TdmsProperty::I64 { name, value } => {
                    push_string(&mut bytes, name);
                    push_u32(&mut bytes, TDMS_TYPE_I64);
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
                TdmsProperty::F64 { name, value } => {
                    push_string(&mut bytes, name);
                    push_u32(&mut bytes, TDMS_TYPE_DOUBLE_FLOAT);
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
    }
    bytes
}

fn push_raw_data_index(bytes: &mut Vec<u8>, index: RawDataIndex) {
    match index {
        RawDataIndex::None => push_u32(bytes, TDMS_RAW_INDEX_NONE),
        RawDataIndex::New {
            data_type,
            dimensions,
            values,
        } => {
            push_u32(bytes, TDMS_RAW_INDEX_LEN);
            push_u32(bytes, data_type);
            push_u32(bytes, dimensions);
            push_u64(bytes, values);
        }
    }
}

fn f64_bytes(values: &[f64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_string(bytes: &mut Vec<u8>, value: &str) {
    let value = value.as_bytes();
    push_u32(
        bytes,
        u32::try_from(value.len()).expect("TDMS string fits u32"),
    );
    bytes.extend_from_slice(value);
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
