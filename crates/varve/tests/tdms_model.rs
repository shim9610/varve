use std::fs::remove_file;
use std::path::PathBuf;

use varve::{CommitPolicy, IndexPolicy, TransactionMarkerMode, VarveBlock, varve_format};

const TDMS_OBJECT_FILE: u8 = 0;
const TDMS_OBJECT_GROUP: u8 = 1;
const TDMS_OBJECT_CHANNEL: u8 = 2;

const TDMS_TYPE_BOOL: u32 = 1;
const TDMS_TYPE_I64: u32 = 2;
const TDMS_TYPE_F64: u32 = 3;
const TDMS_TYPE_STRING: u32 = 4;
const TDMS_TYPE_TIMESTAMP: u32 = 5;

const TDMS_CHANNEL_F64: u32 = 3;

varve_format! {
    pub format TdmsModelFormat {
        magic: b"VTDMS";
        version: 1;
        endian: little;
        schema_hash: computed;
        extension: "vtdms";
        index: [scan_on_open, checkpoint_on_flush, block_offset_chain, keyed_offset_chain];
        commit: transaction_marker(on_flush);
        manifest: embedded;

        blocks {
            fixed TdmsSegment(id = 100) {
                segment_index: u64,
                toc_mask: u32,
                object_count: u32,
                raw_chunk_count: u32,
            }

            variable TdmsObject(id = 101, key = [path]) {
                path: String,
                kind: u8,
                group: String = default,
                channel: String = default,
            }

            variable TdmsProperty(id = 102, key = [object_path, name]) {
                object_path: String,
                name: String,
                value_type: u32,
                bool_value: bool = default,
                int_value: i64 = default,
                float_value: f64 = default,
                string_value: String = default,
                timestamp_seconds: i64 = default,
                timestamp_fractions: u64 = default,
            }

            variable TdmsChannelChunk(id = 103, key = [group, channel, chunk_index]) {
                group: String,
                channel: String,
                chunk_index: u64,
                start_index: u64,
                data_type: u32,
                values_f64: Vec<f64> = default,
                values_i64: Vec<i64> = default,
            }
        }
    }
}

#[test]
fn tdms_data_model_reader_writer_can_be_built_on_public_api() -> varve::Result<()> {
    let path = temp_path("tdms_model");
    cleanup(&path);

    let spec = TdmsModelFormat::spec();
    assert_eq!(spec.extension, Some("vtdms"));
    assert_eq!(spec.index_policy, IndexPolicy::new(true, true, true, true));
    assert_eq!(
        spec.commit_policy,
        CommitPolicy::TransactionMarker(TransactionMarkerMode::OnFlush)
    );
    assert_ne!(spec.schema_hash, 0);

    {
        let mut writer = TdmsModelFormat::create_writer(&path)?;
        write_first_tdms_segment(&mut writer)?;
        write_second_tdms_segment(&mut writer)?;
        writer.flush()?;
    }

    let reader = TdmsModelFormat::open_reader(&path)?;

    let segments = reader.tdms_segments()?;
    assert_eq!(segments.len(), 2);
    assert_eq!(
        segments.get(0)?,
        Some(TdmsSegment {
            segment_index: 0,
            toc_mask: 0x1110,
            object_count: 3,
            raw_chunk_count: 1,
        })
    );
    assert_eq!(
        segments.get(1)?,
        Some(TdmsSegment {
            segment_index: 1,
            toc_mask: 0x1108,
            object_count: 1,
            raw_chunk_count: 1,
        })
    );

    let objects = reader.tdms_objects()?;
    assert_eq!(objects.len(), 3);
    assert_eq!(objects.get(&s("/"))?.unwrap().kind, TDMS_OBJECT_FILE);
    assert_eq!(
        objects.get(&s("/'Measured Data'"))?,
        Some(TdmsObject {
            path: s("/'Measured Data'"),
            kind: TDMS_OBJECT_GROUP,
            group: s("Measured Data"),
            channel: String::new(),
        })
    );
    assert_eq!(
        objects.get(&s("/'Measured Data'/'Amplitude'"))?,
        Some(TdmsObject {
            path: s("/'Measured Data'/'Amplitude'"),
            kind: TDMS_OBJECT_CHANNEL,
            group: s("Measured Data"),
            channel: s("Amplitude"),
        })
    );

    let properties = reader.tdms_properties()?;
    assert_eq!(properties.len(), 6);
    assert_eq!(
        properties.get(&(s("/"), s("title")))?.unwrap().string_value,
        "Varve TDMS-style adapter model smoke"
    );
    assert_eq!(
        properties
            .get(&(s("/'Measured Data'/'Amplitude'"), s("unit")))?
            .unwrap()
            .string_value,
        "mV"
    );
    assert_eq!(
        properties
            .get(&(s("/'Measured Data'/'Amplitude'"), s("wf_start_time")))?
            .unwrap()
            .timestamp_seconds,
        3_786_912_000
    );
    assert!(
        properties
            .get(&(s("/'Measured Data'"), s("calibrated")))?
            .unwrap()
            .bool_value
    );
    assert_eq!(
        properties
            .get(&(s("/'Measured Data'"), s("sample_rate_hz")))?
            .unwrap()
            .float_value,
        1_000.0
    );
    assert_eq!(
        properties
            .get(&(s("/'Measured Data'/'Amplitude'"), s("sample_count")))?
            .unwrap()
            .int_value,
        10
    );

    let chunks = reader.tdms_channel_chunks()?;
    assert_eq!(chunks.len(), 2);
    let mut samples = Vec::new();
    let mut expected_start = 0;
    for chunk in chunks.as_blocks().iter() {
        let chunk = chunk?;
        assert_eq!(chunk.group, "Measured Data");
        assert_eq!(chunk.channel, "Amplitude");
        assert_eq!(chunk.start_index, expected_start);
        assert_eq!(chunk.data_type, TDMS_CHANNEL_F64);
        expected_start += chunk.values_f64.len() as u64;
        samples.extend(chunk.values_f64);
    }
    assert_eq!(expected_start, 10);
    assert_eq!(
        samples,
        vec![0.10, 0.20, 0.30, 0.40, 0.50, 0.60, 0.70, 0.80, 0.90, 1.00]
    );

    let raw_file = TdmsModelFormat::open_readonly(&path)?;
    let segment_ids: Vec<_> = raw_file.scan().map(|event| event.block_id).collect();
    assert!(segment_ids.contains(&TdmsSegment::ID));
    assert!(segment_ids.contains(&TdmsObject::ID));
    assert!(segment_ids.contains(&TdmsProperty::ID));
    assert!(segment_ids.contains(&TdmsChannelChunk::ID));

    cleanup(&path);
    Ok(())
}

fn write_first_tdms_segment(writer: &mut TdmsModelFormatWriter) -> varve::Result<()> {
    writer.push_tdms_segment(&TdmsSegment {
        segment_index: 0,
        toc_mask: 0x1110,
        object_count: 3,
        raw_chunk_count: 1,
    })?;
    writer.push_tdms_object(&TdmsObject {
        path: s("/"),
        kind: TDMS_OBJECT_FILE,
        group: String::new(),
        channel: String::new(),
    })?;
    writer.push_tdms_object(&TdmsObject {
        path: s("/'Measured Data'"),
        kind: TDMS_OBJECT_GROUP,
        group: s("Measured Data"),
        channel: String::new(),
    })?;
    writer.push_tdms_object(&TdmsObject {
        path: s("/'Measured Data'/'Amplitude'"),
        kind: TDMS_OBJECT_CHANNEL,
        group: s("Measured Data"),
        channel: s("Amplitude"),
    })?;
    writer.push_tdms_property(&string_property(
        "/",
        "title",
        "Varve TDMS-style adapter model smoke",
    ))?;
    writer.push_tdms_property(&bool_property("/'Measured Data'", "calibrated", true))?;
    writer.push_tdms_property(&float_property(
        "/'Measured Data'",
        "sample_rate_hz",
        1_000.0,
    ))?;
    writer.push_tdms_property(&string_property(
        "/'Measured Data'/'Amplitude'",
        "unit",
        "V",
    ))?;
    writer.push_tdms_property(&timestamp_property(
        "/'Measured Data'/'Amplitude'",
        "wf_start_time",
        3_786_912_000,
        0,
    ))?;
    writer.push_tdms_channel_chunk(&TdmsChannelChunk {
        group: s("Measured Data"),
        channel: s("Amplitude"),
        chunk_index: 0,
        start_index: 0,
        data_type: TDMS_CHANNEL_F64,
        values_f64: vec![0.10, 0.20, 0.30, 0.40, 0.50],
        values_i64: Vec::new(),
    })?;
    Ok(())
}

fn write_second_tdms_segment(writer: &mut TdmsModelFormatWriter) -> varve::Result<()> {
    writer.push_tdms_segment(&TdmsSegment {
        segment_index: 1,
        toc_mask: 0x1108,
        object_count: 1,
        raw_chunk_count: 1,
    })?;
    writer.push_tdms_property(&string_property(
        "/'Measured Data'/'Amplitude'",
        "unit",
        "mV",
    ))?;
    writer.push_tdms_property(&int_property(
        "/'Measured Data'/'Amplitude'",
        "sample_count",
        10,
    ))?;
    writer.push_tdms_channel_chunk(&TdmsChannelChunk {
        group: s("Measured Data"),
        channel: s("Amplitude"),
        chunk_index: 1,
        start_index: 5,
        data_type: TDMS_CHANNEL_F64,
        values_f64: vec![0.60, 0.70, 0.80, 0.90, 1.00],
        values_i64: Vec::new(),
    })?;
    Ok(())
}

fn string_property(object_path: &str, name: &str, value: &str) -> TdmsProperty {
    TdmsProperty {
        object_path: s(object_path),
        name: s(name),
        value_type: TDMS_TYPE_STRING,
        bool_value: false,
        int_value: 0,
        float_value: 0.0,
        string_value: s(value),
        timestamp_seconds: 0,
        timestamp_fractions: 0,
    }
}

fn bool_property(object_path: &str, name: &str, value: bool) -> TdmsProperty {
    TdmsProperty {
        object_path: s(object_path),
        name: s(name),
        value_type: TDMS_TYPE_BOOL,
        bool_value: value,
        int_value: 0,
        float_value: 0.0,
        string_value: String::new(),
        timestamp_seconds: 0,
        timestamp_fractions: 0,
    }
}

fn int_property(object_path: &str, name: &str, value: i64) -> TdmsProperty {
    TdmsProperty {
        object_path: s(object_path),
        name: s(name),
        value_type: TDMS_TYPE_I64,
        bool_value: false,
        int_value: value,
        float_value: 0.0,
        string_value: String::new(),
        timestamp_seconds: 0,
        timestamp_fractions: 0,
    }
}

fn float_property(object_path: &str, name: &str, value: f64) -> TdmsProperty {
    TdmsProperty {
        object_path: s(object_path),
        name: s(name),
        value_type: TDMS_TYPE_F64,
        bool_value: false,
        int_value: 0,
        float_value: value,
        string_value: String::new(),
        timestamp_seconds: 0,
        timestamp_fractions: 0,
    }
}

fn timestamp_property(object_path: &str, name: &str, seconds: i64, fractions: u64) -> TdmsProperty {
    TdmsProperty {
        object_path: s(object_path),
        name: s(name),
        value_type: TDMS_TYPE_TIMESTAMP,
        bool_value: false,
        int_value: 0,
        float_value: 0.0,
        string_value: String::new(),
        timestamp_seconds: seconds,
        timestamp_fractions: fractions,
    }
}

fn s(value: &str) -> String {
    value.to_string()
}

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("varve_{name}_{}.vtdms", std::process::id()));
    path
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
