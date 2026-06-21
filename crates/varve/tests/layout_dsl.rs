use std::fs::{read, remove_file, write};
use std::path::PathBuf;

use varve::{
    Error, LayoutFieldValue, LayoutPlanFieldSource, LayoutPlanFieldType, LayoutPlanLen,
    LayoutPlanPartKind, LayoutPreset, LayoutValue, SegmentRepeat, SegmentWrite, VarveBlock,
    varve_format,
};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 50, version = 1, kind = "fixed")]
struct NativePoint {
    x: u32,
    y: u32,
}

varve_format! {
    pub struct NativeDefaultFormat {
        magic: b"NATV";
        version: 1;
        endian: little;
        schema_hash: computed;
        blocks: [NativePoint];
    }
}

varve_format! {
    pub struct NativeExplicitFormat {
        magic: b"NATV";
        version: 1;
        endian: little;
        schema_hash: computed;
        preset: varve_native;
        blocks: [NativePoint];
    }
}

varve_format! {
    pub struct NativeFooterFormat {
        magic: b"NATV3";
        version: 1;
        endian: little;
        schema_hash: computed;
        commit: record_footer;
        blocks: [NativePoint];
    }
}

varve_format! {
    pub struct NativeChainFormat {
        magic: b"NATVC";
        version: 1;
        endian: little;
        schema_hash: computed;
        index: [scan_on_open, block_offset_chain];
        blocks: [NativePoint];
    }
}

varve_format! {
    pub format TdmsPhysicalFormat {
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
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata TdmsMetadata;
                raw_region TdmsRaw;
            }
        }
    }
}

varve_format! {
    pub format FramedPhysicalFormat {
        magic: b"FRAM";
        version: 1;
        endian: little;
        schema_hash: computed;
        extension: "frame";
        preset: none;

        layout {
            file_header FileHeader {
                bytes signature = b"VRV!";
                u16 header_version = 1;
            }

            segment DataSegment repeat until_eof {
                lead_in DataLeadIn {
                    bytes tag = b"SEGM";
                    u32 kind;
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata DataMetadata;
                raw_region DataRaw;

                footer DataFooter {
                    bytes seal = b"END!";
                    u64 segment_len = finalize(target = segment_end, relative_to = segment_start);
                }
            }
        }
    }
}

#[test]
fn explicit_varve_native_preset_preserves_native_bytes() -> varve::Result<()> {
    let default_path = temp_path("layout_native_default");
    let explicit_path = temp_path("layout_native_explicit");
    cleanup(&default_path);
    cleanup(&explicit_path);

    {
        let mut file = NativeDefaultFormat::create(&default_path)?;
        file.push(&NativePoint { x: 1, y: 2 })?;
        file.flush()?;
    }
    {
        let mut file = NativeExplicitFormat::create(&explicit_path)?;
        file.push(&NativePoint { x: 1, y: 2 })?;
        file.flush()?;
    }

    assert_eq!(
        NativeDefaultFormat::spec().schema_hash,
        NativeExplicitFormat::spec().schema_hash
    );
    assert_eq!(read(&default_path)?, read(&explicit_path)?);

    let inspected = NativeDefaultFormat::inspect_layout_file(&default_path)?;
    assert_eq!(inspected.plan.preset, LayoutPreset::VarveNative);
    assert_eq!(inspected.file_header_len, 22);
    assert_eq!(inspected.segments.len(), 1);
    assert_eq!(
        inspected.segments[0].field("block_id"),
        Some(&LayoutValue::U32(NativePoint::ID))
    );
    assert_eq!(
        inspected.segments[0].field("payload_len"),
        Some(&LayoutValue::U64(8))
    );
    assert_eq!(inspected.segments[0].raw_len, 8);

    cleanup(&default_path);
    cleanup(&explicit_path);
    Ok(())
}

#[test]
fn varve_native_preset_exposes_effective_layout_plan() {
    let plan = NativeDefaultFormat::spec().effective_layout();
    assert_eq!(plan.parts.len(), 2);

    let LayoutPlanPartKind::FileHeader(header) = &plan.parts[0].kind else {
        panic!("expected native file header");
    };
    assert_eq!(header.name, "VarveFileHeader");
    assert_eq!(header.fields[0].name, "magic");
    assert_eq!(
        header.fields[0].ty,
        LayoutPlanFieldType::Bytes {
            len: LayoutPlanLen::Fixed(4)
        }
    );
    assert_eq!(
        header.fields[1].source,
        LayoutPlanFieldSource::LiteralBytes(b"VARVE1".to_vec())
    );

    let LayoutPlanPartKind::Segment(segment) = &plan.parts[1].kind else {
        panic!("expected native record segment");
    };
    assert_eq!(segment.name, "VarveRecord");
    assert_eq!(segment.repeat, SegmentRepeat::UntilEof);
    assert_eq!(segment.lead_in.name, "VarveRecordHeader");
    assert!(segment.footer.is_none());
    assert_eq!(segment.raw_region.name, "Payload");
    assert!(
        segment
            .lead_in
            .fields
            .iter()
            .any(|field| field.name == "payload_len"
                && matches!(field.source, LayoutPlanFieldSource::Finalize(_)))
    );
}

#[test]
fn varve3_native_preset_exposes_footer_layout_plan() {
    let plan = NativeFooterFormat::spec().effective_layout();
    let LayoutPlanPartKind::FileHeader(header) = &plan.parts[0].kind else {
        panic!("expected native file header");
    };
    assert_eq!(
        header.fields[1].source,
        LayoutPlanFieldSource::LiteralBytes(b"VARVE3".to_vec())
    );
    assert!(
        header
            .fields
            .iter()
            .any(|field| field.name == "extension_len")
    );

    let LayoutPlanPartKind::Segment(segment) = &plan.parts[1].kind else {
        panic!("expected native record segment");
    };
    let footer = segment.footer.as_ref().expect("VARVE3 footer plan");
    assert_eq!(footer.name, "VarveRecordFooter");
    assert_eq!(
        footer.fields[0].source,
        LayoutPlanFieldSource::LiteralBytes(b"VRF1".to_vec())
    );
    assert!(
        footer
            .fields
            .iter()
            .any(|field| field.name == "prev_same_block_offset")
    );
}

#[test]
fn native_record_layout_codec_preserves_header_and_footer_bytes() -> varve::Result<()> {
    let path = temp_path("native_layout_codec_v3");
    cleanup(&path);

    {
        let mut file = NativeFooterFormat::create(&path)?;
        file.push(&NativePoint { x: 9, y: 10 })?;
        file.flush()?;
    }

    let bytes = read(&path)?;
    let inspected = NativeFooterFormat::inspect_layout_file(&path)?;
    assert_eq!(inspected.segments.len(), 1);
    let segment = &inspected.segments[0];
    let offset = segment.segment_start as usize;

    assert_eq!(u32_at(&bytes, offset), NativePoint::ID);
    assert_eq!(u16_at(&bytes, offset + 4), 1);
    assert_eq!(u16_at(&bytes, offset + 6), 0);
    assert_eq!(u64_at(&bytes, offset + 8), 0);
    assert_eq!(u64_at(&bytes, offset + 16), 8);
    assert_eq!(u32_at(&bytes, offset + 24), 0);
    assert_eq!(u32_at(&bytes, offset + 28), 0);
    assert_eq!(segment.raw_offset, segment.segment_start + 32);
    assert_eq!(segment.raw_len, 8);

    let footer = segment.footer_offset as usize;
    assert_eq!(&bytes[footer..footer + 4], b"VRF1");
    assert_eq!(u16_at(&bytes, footer + 4), 1);
    assert_eq!(u16_at(&bytes, footer + 6), 0);
    assert_eq!(u64_at(&bytes, footer + 8), 0);
    assert_eq!(u64_at(&bytes, footer + 16), 0);
    assert_eq!(u32_at(&bytes, footer + 24), 0);
    assert_eq!(u32_at(&bytes, footer + 28), 0);

    cleanup(&path);
    Ok(())
}

#[test]
fn native_record_layout_codec_preserves_block_offset_chain_footer() -> varve::Result<()> {
    let path = temp_path("native_layout_codec_chain");
    cleanup(&path);

    {
        let mut file = NativeChainFormat::create(&path)?;
        file.push(&NativePoint { x: 1, y: 2 })?;
        file.push(&NativePoint { x: 3, y: 4 })?;
        file.flush()?;
    }

    let bytes = read(&path)?;
    let inspected = NativeChainFormat::inspect_layout_file(&path)?;
    assert_eq!(inspected.segments.len(), 2);
    let first = &inspected.segments[0];
    let second = &inspected.segments[1];
    assert_eq!(
        first.footer_field("prev_same_block_offset"),
        Some(&LayoutValue::U64(0))
    );
    assert_eq!(
        second.footer_field("prev_same_block_offset"),
        Some(&LayoutValue::U64(first.segment_start))
    );

    let first_footer = first.footer_offset as usize;
    let second_footer = second.footer_offset as usize;
    assert_eq!(u16_at(&bytes, first_footer + 6), 0);
    assert_eq!(u64_at(&bytes, first_footer + 8), 0);
    assert_eq!(u16_at(&bytes, second_footer + 6), 1);
    assert_eq!(u64_at(&bytes, second_footer + 8), first.segment_start);

    cleanup(&path);
    Ok(())
}

#[test]
fn native_record_layout_codec_rejects_invalid_footer_fields() -> varve::Result<()> {
    let path = temp_path("native_layout_codec_footer_invalid_base");
    let bad_flags = temp_path("native_layout_codec_footer_bad_flags");
    let bad_reserved = temp_path("native_layout_codec_footer_bad_reserved");
    cleanup(&path);
    cleanup(&bad_flags);
    cleanup(&bad_reserved);

    {
        let mut file = NativeFooterFormat::create(&path)?;
        file.push(&NativePoint { x: 9, y: 10 })?;
        file.flush()?;
    }

    let bytes = read(&path)?;
    let inspected = NativeFooterFormat::inspect_layout_file(&path)?;
    let footer = inspected.segments[0].footer_offset as usize;

    let mut corrupted = bytes.clone();
    corrupted[footer + 6..footer + 8].copy_from_slice(&0x8000u16.to_le_bytes());
    write(&bad_flags, &corrupted)?;
    assert!(matches!(
        NativeFooterFormat::open_readonly(&bad_flags),
        Err(Error::InvalidRecordFooter { .. })
    ));

    let mut corrupted = bytes;
    corrupted[footer + 28..footer + 32].copy_from_slice(&1u32.to_le_bytes());
    write(&bad_reserved, &corrupted)?;
    assert!(matches!(
        NativeFooterFormat::open_readonly(&bad_reserved),
        Err(Error::InvalidRecordFooter { .. })
    ));

    cleanup(&path);
    cleanup(&bad_flags);
    cleanup(&bad_reserved);
    Ok(())
}

#[test]
fn custom_layout_writes_file_header_and_segment_footer() -> varve::Result<()> {
    let path = temp_path("framed_physical_layout");
    cleanup(&path);

    let fields = [LayoutFieldValue {
        name: "kind",
        value: LayoutValue::U32(7),
    }];
    let raw = b"data";

    {
        let mut writer = FramedPhysicalFormat::create_layout_writer(&path)?;
        let info = writer.write_segment(SegmentWrite {
            name: "DataSegment",
            fields: &fields,
            footer_fields: &[],
            metadata: b"abc",
            raw,
        })?;
        assert_eq!(info.segment_start, 6);
        assert_eq!(info.lead_in_len, 24);
        assert_eq!(info.metadata_offset, 30);
        assert_eq!(info.raw_offset, 33);
        assert_eq!(info.footer_offset, 37);
        assert_eq!(info.footer_len, 12);
        assert_eq!(info.segment_end, 49);
        assert_eq!(info.field("kind"), Some(&LayoutValue::U32(7)));
        assert_eq!(
            info.footer_field("segment_len"),
            Some(&LayoutValue::U64(43))
        );
        writer.flush()?;
    }

    let bytes = read(&path)?;
    assert_eq!(&bytes[0..4], b"VRV!");
    assert_eq!(u16_at(&bytes, 4), 1);
    assert_eq!(&bytes[6..10], b"SEGM");
    assert_eq!(u32_at(&bytes, 10), 7);
    assert_eq!(i64_at(&bytes, 14), 19);
    assert_eq!(i64_at(&bytes, 22), 3);
    assert_eq!(&bytes[30..33], b"abc");
    assert_eq!(&bytes[33..37], raw);
    assert_eq!(&bytes[37..41], b"END!");
    assert_eq!(u64_at(&bytes, 41), 43);

    let reader = FramedPhysicalFormat::open_layout_reader(&path)?;
    assert_eq!(reader.file_header_len(), 6);
    assert_eq!(reader.segments().len(), 1);
    assert_eq!(
        reader.segments()[0].field("kind"),
        Some(&LayoutValue::U32(7))
    );
    assert_eq!(
        reader.segments()[0].footer_field("segment_len"),
        Some(&LayoutValue::U64(43))
    );
    assert_eq!(reader.read_metadata(0)?, b"abc");
    assert_eq!(reader.read_raw(0)?, raw);

    let inspected = FramedPhysicalFormat::inspect_layout_file(&path)?;
    assert_eq!(inspected.plan.preset, LayoutPreset::None);
    assert_eq!(inspected.file_header_len, 6);
    assert_eq!(inspected.segments.as_slice(), reader.segments());

    cleanup(&path);
    Ok(())
}

#[test]
fn tdms_style_layout_writes_physical_leadin_offsets_and_raw_region() -> varve::Result<()> {
    let path = temp_path("tdms_physical_layout");
    cleanup(&path);

    let metadata = b"objects-and-properties";
    let raw = f64_bytes(&[0.25, 0.50, 0.75]);
    let fields = [
        LayoutFieldValue {
            name: "toc_mask",
            value: LayoutValue::U32(0x1110),
        },
        LayoutFieldValue {
            name: "version",
            value: LayoutValue::U32(4713),
        },
    ];

    {
        let mut writer = TdmsPhysicalFormat::create_layout_writer(&path)?;
        let info = writer.write_segment(SegmentWrite {
            name: "TdmsSegment",
            fields: &fields,
            footer_fields: &[],
            metadata,
            raw: &raw,
        })?;
        assert_eq!(info.segment_start, 0);
        assert_eq!(info.lead_in_len, 28);
        assert_eq!(info.metadata_offset, 28);
        assert_eq!(info.raw_offset, 28 + metadata.len() as u64);
        assert_eq!(
            info.segment_end,
            28 + metadata.len() as u64 + raw.len() as u64
        );
        writer.flush()?;
    }

    let bytes = read(&path)?;
    assert_eq!(&bytes[0..4], b"TDSm");
    assert_eq!(u32_at(&bytes, 4), 0x1110);
    assert_eq!(u32_at(&bytes, 8), 4713);
    assert_eq!(i64_at(&bytes, 12), (metadata.len() + raw.len()) as i64);
    assert_eq!(i64_at(&bytes, 20), metadata.len() as i64);
    let raw_start = 28 + metadata.len();
    assert_eq!(&bytes[28..raw_start], metadata);
    assert_eq!(&bytes[raw_start..], raw);
    assert_eq!(f64_values(&bytes[raw_start..]), vec![0.25, 0.50, 0.75]);

    let reader = TdmsPhysicalFormat::open_layout_reader(&path)?;
    assert_eq!(reader.segments().len(), 1);
    let segment = &reader.segments()[0];
    assert_eq!(segment.field("toc_mask"), Some(&LayoutValue::U32(0x1110)));
    assert_eq!(segment.field("version"), Some(&LayoutValue::U32(4713)));
    assert_eq!(
        segment.field("next_segment_offset"),
        Some(&LayoutValue::I64((metadata.len() + raw.len()) as i64))
    );
    assert_eq!(
        segment.field("raw_data_offset"),
        Some(&LayoutValue::I64(metadata.len() as i64))
    );
    assert_eq!(segment.metadata_len, metadata.len() as u64);
    assert_eq!(segment.raw_len, raw.len() as u64);
    assert_eq!(reader.read_metadata(0)?, metadata);
    assert_eq!(reader.read_raw(0)?, raw);

    cleanup(&path);
    Ok(())
}

#[test]
fn tdms_style_layout_reopens_and_appends_multiple_segments() -> varve::Result<()> {
    let path = temp_path("tdms_physical_append");
    cleanup(&path);
    let fields = [
        LayoutFieldValue {
            name: "toc_mask",
            value: LayoutValue::U32(0x1108),
        },
        LayoutFieldValue {
            name: "version",
            value: LayoutValue::U32(4713),
        },
    ];

    {
        let mut writer = TdmsPhysicalFormat::create_layout_writer(&path)?;
        writer.write_segment(SegmentWrite {
            name: "TdmsSegment",
            fields: &fields,
            footer_fields: &[],
            metadata: b"meta-a",
            raw: &f64_bytes(&[1.0, 2.0]),
        })?;
        writer.flush()?;
    }

    {
        let mut writer = TdmsPhysicalFormat::open_layout_writer(&path)?;
        writer.write_segment(SegmentWrite {
            name: "TdmsSegment",
            fields: &fields,
            footer_fields: &[],
            metadata: b"meta-bb",
            raw: &f64_bytes(&[3.0]),
        })?;
        writer.flush()?;
    }

    let reader = TdmsPhysicalFormat::open_layout_reader(&path)?;
    assert_eq!(reader.segments().len(), 2);
    assert_eq!(reader.read_metadata(0)?, b"meta-a");
    assert_eq!(f64_values(&reader.read_raw(0)?), vec![1.0, 2.0]);
    assert_eq!(reader.read_metadata(1)?, b"meta-bb");
    assert_eq!(f64_values(&reader.read_raw(1)?), vec![3.0]);
    assert_eq!(
        reader.segments()[1].field("toc_mask"),
        Some(&LayoutValue::U32(0x1108))
    );
    assert_eq!(
        reader.segments()[1].segment_start,
        reader.segments()[0].segment_end
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn tdms_style_layout_rejects_corrupt_physical_segments() -> varve::Result<()> {
    let bad_tag = temp_path("tdms_bad_tag");
    let truncated = temp_path("tdms_truncated");
    let bad_bounds = temp_path("tdms_bad_bounds");
    cleanup(&bad_tag);
    cleanup(&truncated);
    cleanup(&bad_bounds);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"BAD!");
    bytes.extend_from_slice(&0x1110u32.to_le_bytes());
    bytes.extend_from_slice(&4713u32.to_le_bytes());
    bytes.extend_from_slice(&0i64.to_le_bytes());
    bytes.extend_from_slice(&0i64.to_le_bytes());
    write(&bad_tag, &bytes)?;
    assert!(matches!(
        TdmsPhysicalFormat::open_layout_reader(&bad_tag),
        Err(Error::LayoutLiteralMismatch { .. })
    ));

    write(&truncated, b"TDSm")?;
    assert!(matches!(
        TdmsPhysicalFormat::open_layout_reader(&truncated),
        Err(Error::LayoutTruncatedLeadIn { .. })
    ));

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"TDSm");
    bytes.extend_from_slice(&0x1110u32.to_le_bytes());
    bytes.extend_from_slice(&4713u32.to_le_bytes());
    bytes.extend_from_slice(&4i64.to_le_bytes());
    bytes.extend_from_slice(&8i64.to_le_bytes());
    bytes.extend_from_slice(&[0; 8]);
    write(&bad_bounds, &bytes)?;
    assert!(matches!(
        TdmsPhysicalFormat::open_layout_reader(&bad_bounds),
        Err(Error::LayoutInvalidSegmentBounds { .. })
    ));

    cleanup(&bad_tag);
    cleanup(&truncated);
    cleanup(&bad_bounds);
    Ok(())
}

fn f64_bytes(values: &[f64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn f64_values(bytes: &[u8]) -> Vec<f64> {
    bytes
        .chunks_exact(8)
        .map(|chunk| {
            let mut value = [0; 8];
            value.copy_from_slice(chunk);
            f64::from_le_bytes(value)
        })
        .collect()
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    let mut value = [0; 2];
    value.copy_from_slice(&bytes[offset..offset + 2]);
    u16::from_le_bytes(value)
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(value)
}

fn i64_at(bytes: &[u8], offset: usize) -> i64 {
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[offset..offset + 8]);
    i64::from_le_bytes(value)
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(value)
}

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("varve_{name}_{}.bin", std::process::id()));
    path
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
