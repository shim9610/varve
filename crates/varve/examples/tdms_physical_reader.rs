use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use varve::varve_format;

const TDMS_TOC_METADATA: u32 = 1 << 1;
const TDMS_TOC_RAW_DATA: u32 = 1 << 3;
const TDMS_TOC_INTERLEAVED_DATA: u32 = 1 << 5;
const TDMS_RAW_INDEX_NONE: u32 = 0xFFFF_FFFF;
const TDMS_RAW_INDEX_SAME_AS_PREVIOUS: u32 = 0;
const TDMS_RAW_INDEX_LEN: u32 = 20;
const TDMS_TYPE_DOUBLE_FLOAT: u32 = 10;
const TDMS_TYPE_STRING: u32 = 0x20;

varve_format! {
    pub format TdmsCompatReadFormat {
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: tdms_physical_reader <file.tdms>");
    let reader = TdmsCompatReadFormat::open_layout_reader(&path)?;
    let mut state = TdmsState::default();

    for (index, segment) in reader.tdms_segments()?.iter().enumerate() {
        assert_eq!(segment.tag()?, b"TDSm");
        assert!(segment.toc_mask()? & TDMS_TOC_METADATA != 0);
        assert!(segment.toc_mask()? & TDMS_TOC_RAW_DATA != 0);
        assert_eq!(segment.toc_mask()? & TDMS_TOC_INTERLEAVED_DATA, 0);
        let metadata = parse_tdms_metadata(&reader.read_tdms_segment_metadata(index)?);
        let raw = reader.read_tdms_segment_raw(index)?;
        state.apply_segment(metadata, &raw);
    }

    assert_eq!(
        state.properties["/"]["title"],
        PropertyValue::String("npTDMS multichannel smoke".to_string())
    );
    assert_eq!(
        state.properties["/'Bench'"]["operator"],
        PropertyValue::String("Ada".to_string())
    );
    assert_eq!(
        state.properties["/'Bench'/'Voltage'"]["unit_string"],
        PropertyValue::String("V".to_string())
    );
    assert_eq!(
        state.properties["/'Bench'/'Current'"]["unit_string"],
        PropertyValue::String("A".to_string())
    );
    assert_eq!(
        state.samples["/'Bench'/'Voltage'"],
        vec![1.0, 2.0, 3.0, 4.0]
    );
    assert_eq!(
        state.samples["/'Bench'/'Current'"],
        vec![0.10, 0.20, 0.30, 0.40]
    );

    println!(
        "Varve-based example parsed npTDMS multichannel file {}",
        path.display()
    );
    Ok(())
}

#[derive(Clone, Debug, Default)]
struct TdmsState {
    properties: HashMap<String, HashMap<String, PropertyValue>>,
    raw_indexes: HashMap<String, RawDataIndex>,
    samples: HashMap<String, Vec<f64>>,
}

impl TdmsState {
    fn apply_segment(&mut self, metadata: TdmsMetadata, raw: &[u8]) {
        let mut raw_order = Vec::new();
        for object in metadata.objects {
            self.properties
                .entry(object.path.clone())
                .or_default()
                .extend(object.properties);
            match object.raw_data_index {
                RawDataIndex::None => {}
                RawDataIndex::SameAsPrevious => {
                    if let Some(index) = self.raw_indexes.get(&object.path) {
                        raw_order.push((object.path, *index));
                    }
                }
                index => {
                    self.raw_indexes.insert(object.path.clone(), index);
                    raw_order.push((object.path, index));
                }
            }
        }

        let mut offset = 0usize;
        for (path, index) in raw_order {
            let RawDataIndex::F64 { values } = index else {
                continue;
            };
            let byte_len = (values as usize) * 8;
            let end = offset + byte_len;
            assert!(end <= raw.len(), "TDMS raw segment is truncated");
            self.samples
                .entry(path.clone())
                .or_default()
                .extend(read_f64_values(&raw[offset..end]));
            offset = end;
        }
        assert_eq!(offset, raw.len(), "TDMS raw segment has trailing bytes");
    }
}

#[derive(Clone, Debug)]
struct TdmsMetadata {
    objects: Vec<TdmsObject>,
}

#[derive(Clone, Debug)]
struct TdmsObject {
    path: String,
    raw_data_index: RawDataIndex,
    properties: HashMap<String, PropertyValue>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawDataIndex {
    None,
    SameAsPrevious,
    F64 { values: u64 },
}

#[derive(Clone, Debug, PartialEq)]
enum PropertyValue {
    String(String),
}

fn parse_tdms_metadata(bytes: &[u8]) -> TdmsMetadata {
    let mut cursor = TdmsCursor { bytes, position: 0 };
    let objects = (0..cursor.u32())
        .map(|_| {
            let path = cursor.string();
            let raw_data_index = cursor.raw_data_index();
            let properties = (0..cursor.u32())
                .map(|_| {
                    let name = cursor.string();
                    let value = cursor.property_value();
                    (name, value)
                })
                .collect();
            TdmsObject {
                path,
                raw_data_index,
                properties,
            }
        })
        .collect();
    assert_eq!(cursor.remaining(), 0);
    TdmsMetadata { objects }
}

struct TdmsCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> TdmsCursor<'a> {
    fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn bytes(&mut self, len: usize) -> &'a [u8] {
        let end = self.position + len;
        assert!(end <= self.bytes.len(), "TDMS metadata is truncated");
        let value = &self.bytes[self.position..end];
        self.position = end;
        value
    }

    fn u32(&mut self) -> u32 {
        let mut value = [0; 4];
        value.copy_from_slice(self.bytes(4));
        u32::from_le_bytes(value)
    }

    fn u64(&mut self) -> u64 {
        let mut value = [0; 8];
        value.copy_from_slice(self.bytes(8));
        u64::from_le_bytes(value)
    }

    fn string(&mut self) -> String {
        let len = self.u32() as usize;
        String::from_utf8(self.bytes(len).to_vec()).expect("TDMS string metadata is UTF-8")
    }

    fn raw_data_index(&mut self) -> RawDataIndex {
        match self.u32() {
            TDMS_RAW_INDEX_NONE => RawDataIndex::None,
            TDMS_RAW_INDEX_SAME_AS_PREVIOUS => RawDataIndex::SameAsPrevious,
            TDMS_RAW_INDEX_LEN => {
                let data_type = self.u32();
                let dimensions = self.u32();
                let values = self.u64();
                assert_eq!(data_type, TDMS_TYPE_DOUBLE_FLOAT);
                assert_eq!(dimensions, 1);
                RawDataIndex::F64 { values }
            }
            other => panic!("unsupported TDMS raw data index length {other}"),
        }
    }

    fn property_value(&mut self) -> PropertyValue {
        match self.u32() {
            TDMS_TYPE_STRING => PropertyValue::String(self.string()),
            other => panic!("unsupported TDMS property type {other}"),
        }
    }
}

fn read_f64_values(bytes: &[u8]) -> Vec<f64> {
    bytes
        .chunks_exact(8)
        .map(|chunk| {
            let mut value = [0; 8];
            value.copy_from_slice(chunk);
            f64::from_le_bytes(value)
        })
        .collect()
}
