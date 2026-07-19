use std::fs::{OpenOptions, read, remove_file, write};
use std::panic::catch_unwind;
use std::path::PathBuf;

use varve::{
    Error, LayoutFieldValue, LayoutPlanFieldSource, LayoutPlanFieldType, LayoutPlanLen,
    LayoutPlanPartKind, LayoutPreset, LayoutTailKind, LayoutValue, SegmentRepeat, SegmentWrite,
    VarveBlock, varve_format,
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
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;
        blocks: [NativePoint];
    }
}

varve_format! {
    pub struct NativeExplicitFormat {
        magic: b"NATV";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
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
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
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
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
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
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
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

varve_format! {
    pub format BmpPhysicalFormat {
        magic: b"BMPX";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;
        extension: "bmp";
        preset: none;

        layout {
            segment BitmapImage repeat once {
                lead_in BitmapHeader {
                    bytes signature = b"BM";
                    u32 file_size = finalize(target = segment_end, relative_to = segment_start);
                    u32 reserved = 0;
                    u32 pixel_data_offset = finalize(target = raw_region_start, relative_to = segment_start);
                    u32 dib_header_size = 40;
                    u32 width;
                    u32 height;
                    u16 planes = 1;
                    u16 bits_per_pixel = 24;
                    u32 compression = 0;
                    u32 image_size = finalize(target = segment_end, relative_to = raw_region_start);
                    u32 x_pixels_per_meter = 2835;
                    u32 y_pixels_per_meter = 2835;
                    u32 colors_used = 0;
                    u32 important_colors = 0;
                }

                metadata BitmapMetadata;
                raw_region BitmapPixels;
            }
        }
    }
}

varve_format! {
    pub format FramedPhysicalFormat {
        magic: b"FRAM";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
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

varve_format! {
    pub format HeaderCallerPhysicalFormat {
        magic: b"HEAD";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;
        extension: "head";
        preset: none;

        layout {
            file_header CallerHeader {
                bytes signature = b"HDCT";
                u16 kind;
            }

            segment HeaderDataSegment repeat until_eof {
                lead_in HeaderDataLeadIn {
                    bytes tag = b"DATA";
                    u32 kind;
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata HeaderDataMetadata;
                raw_region HeaderDataRaw;
            }
        }
    }
}

varve_format! {
    pub format ImplicitPhysicalFormat {
        magic: b"IMPL";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;

        layout {
            segment ImplicitSegment repeat until_eof {
                lead_in ImplicitLeadIn {
                    bytes tag = b"IMPL";
                    u32 kind;
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata ImplicitMetadata;
                raw_region ImplicitRaw;
            }
        }
    }
}

varve_format! {
    pub format MultiPhysicalFormat {
        magic: b"MULT";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;

        layout {
            segment ControlSegment repeat once {
                lead_in ControlLeadIn {
                    bytes tag = b"CTRL";
                    u16 version = 1;
                    u32 code;
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata ControlMetadata;
                raw_region ControlRaw;
            }

            segment DataSegment repeat until_eof {
                lead_in DataLeadIn {
                    bytes tag = b"DATA";
                    u32 channel;
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata DataMetadata;
                raw_region DataRaw;

                footer DataFooter {
                    bytes seal = b"DONE";
                    u64 segment_len = finalize(target = segment_end, relative_to = segment_start);
                }
            }
        }
    }
}

varve_format! {
    pub format SameTagPhysicalFormat {
        magic: b"STAG";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;

        layout {
            segment MetaSegment repeat until_eof {
                lead_in MetaLeadIn {
                    bytes tag = b"STAG";
                    u16 kind = 1;
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata MetaMetadata;
                raw_region MetaRaw;
            }

            segment RawSegment repeat until_eof {
                lead_in RawLeadIn {
                    bytes tag = b"STAG";
                    u16 kind = 2;
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata RawMetadata;
                raw_region RawRaw;
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

    let default_bytes = read(&default_path)?;
    assert_eq!(&default_bytes[0..4], b"NATV");
    assert_eq!(&default_bytes[4..10], b"VARVE1");
    assert_eq!(u16_at(&default_bytes, 10), 1);
    assert_eq!(default_bytes[12], 1);
    assert_eq!(default_bytes[13], 0);
    assert_eq!(
        u64_at(&default_bytes, 14),
        NativeDefaultFormat::spec().schema_hash
    );

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
fn native_file_header_layout_codec_preserves_varve3_extension_len_zero() -> varve::Result<()> {
    let path = temp_path("native_file_header_codec_v3");
    cleanup(&path);

    {
        let mut file = NativeFooterFormat::create(&path)?;
        file.push(&NativePoint { x: 5, y: 6 })?;
        file.flush()?;
    }

    let bytes = read(&path)?;
    assert_eq!(&bytes[0..5], b"NATV3");
    assert_eq!(&bytes[5..11], b"VARVE3");
    assert_eq!(u16_at(&bytes, 11), 1);
    assert_eq!(bytes[13], 1);
    assert_eq!(bytes[14], 0);
    assert_eq!(u64_at(&bytes, 15), NativeFooterFormat::spec().schema_hash);
    assert_eq!(u32_at(&bytes, 23), 0);

    let inspected = NativeFooterFormat::inspect_layout_file(&path)?;
    assert_eq!(inspected.file_header_len, 27);
    assert_eq!(inspected.segments[0].segment_start, 27);

    cleanup(&path);
    Ok(())
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
    assert_eq!(reader.read_metadata_range(0, 1, 2)?, b"bc");
    assert!(matches!(
        reader.read_metadata_range(0, 2, 2),
        Err(Error::LayoutInvalidSegmentBounds { .. })
    ));
    assert_eq!(reader.read_raw(0)?, raw);
    assert_eq!(reader.read_raw_range(0, 1, 2)?, b"at");

    let inspected = FramedPhysicalFormat::inspect_layout_file(&path)?;
    assert_eq!(inspected.plan.preset, LayoutPreset::None);
    assert_eq!(inspected.file_header_len, 6);
    assert_eq!(inspected.segments.as_slice(), reader.segments());

    cleanup(&path);
    Ok(())
}

#[test]
fn custom_layout_streams_metadata_and_raw_without_prebuffering() -> varve::Result<()> {
    let path = temp_path("framed_physical_streamed_layout");
    cleanup(&path);

    {
        let mut writer = FramedPhysicalFormat::create_layout_writer(&path)?;
        let info = writer.write_data_segment_streamed(
            FramedPhysicalFormatDataSegmentLayoutFields { kind: 11 },
            FramedPhysicalFormatDataSegmentLayoutFooterFields,
            |out| {
                out.write_all(b"meta-")?;
                out.write_all(b"stream")?;
                Ok(())
            },
            |out| {
                for chunk in [b"raw-" as &[u8], b"stream"] {
                    out.write_all(chunk)?;
                }
                Ok(())
            },
        )?;
        assert_eq!(info.kind()?, 11);
        assert_eq!(info.next_segment_offset()?, 33);
        assert_eq!(info.raw_data_offset()?, 11);
        assert_eq!(info.footer_segment_len()?, 57);
        writer.flush()?;
    }

    let reader = FramedPhysicalFormat::open_layout_reader(&path)?;
    assert_eq!(reader.read_data_segment_metadata(0)?, b"meta-stream");
    assert_eq!(reader.read_data_segment_raw(0)?, b"raw-stream");

    cleanup(&path);
    Ok(())
}

#[test]
fn custom_layout_report_preserves_complete_prefix_before_truncated_tail() -> varve::Result<()> {
    let path = temp_path("tdms_physical_report_truncated_tail");
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
            metadata: b"complete",
            raw: &f64_bytes(&[1.0]),
        })?;
        writer.write_segment(SegmentWrite {
            name: "TdmsSegment",
            fields: &fields,
            footer_fields: &[],
            metadata: b"tail",
            raw: &f64_bytes(&[2.0, 3.0]),
        })?;
        writer.flush()?;
    }

    let original_len = read(&path)?.len() as u64;
    OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_len(original_len - 4)?;

    assert!(matches!(
        TdmsPhysicalFormat::open_layout_reader(&path),
        Err(Error::LayoutInvalidSegmentBounds { .. })
    ));

    let report = TdmsPhysicalFormat::inspect_layout_file_report(&path)?;
    assert!(!report.is_complete());
    assert_eq!(report.segments.len(), 1);
    assert_eq!(
        report.segments[0].field("version"),
        Some(&LayoutValue::U32(4713))
    );
    let tail = report.tail.expect("truncated tail report");
    assert_eq!(tail.offset, report.segments[0].segment_end);
    assert_eq!(tail.file_len, original_len - 4);
    assert_eq!(tail.kind, LayoutTailKind::InvalidSegmentBounds);

    cleanup(&path);
    Ok(())
}

#[test]
fn bmp_style_layout_uses_u32_finalized_offsets() -> varve::Result<()> {
    let path = temp_path("bmp_style_physical_layout");
    cleanup(&path);

    let pixels = vec![0, 0, 255, 255, 255, 255, 0, 0, 0, 255, 0, 255, 0, 0, 0, 0];

    {
        let mut writer = BmpPhysicalFormat::create_layout_writer(&path)?;
        let info = writer.write_bitmap_image(BmpPhysicalFormatBitmapImageLayoutWrite {
            fields: BmpPhysicalFormatBitmapImageLayoutFields {
                width: 2,
                height: 2,
            },
            footer_fields: BmpPhysicalFormatBitmapImageLayoutFooterFields,
            metadata: b"",
            raw: &pixels,
        })?;
        assert_eq!(info.file_size()?, 70);
        assert_eq!(info.pixel_data_offset()?, 54);
        assert_eq!(info.image_size()?, 16);
        writer.flush()?;
    }

    let bytes = read(&path)?;
    assert_eq!(&bytes[0..2], b"BM");
    assert_eq!(u32_at(&bytes, 2), 70);
    assert_eq!(u32_at(&bytes, 10), 54);
    assert_eq!(u32_at(&bytes, 34), 16);
    assert_eq!(&bytes[54..], pixels.as_slice());

    let reader = BmpPhysicalFormat::open_layout_reader(&path)?;
    let image = reader.bitmap_image(0)?.expect("bitmap image segment");
    assert_eq!(image.signature()?, b"BM");
    assert_eq!(image.file_size()?, 70);
    assert_eq!(image.pixel_data_offset()?, 54);
    assert_eq!(image.width()?, 2);
    assert_eq!(image.height()?, 2);
    assert_eq!(image.image_size()?, 16);
    assert_eq!(reader.read_bitmap_image_metadata(0)?, b"");
    assert_eq!(reader.read_bitmap_image_raw(0)?, pixels);
    assert_eq!(
        reader.read_bitmap_image_raw_range(0, 8, 3)?,
        vec![0, 255, 0]
    );
    assert!(matches!(
        reader.read_bitmap_image_raw_range(0, 15, 2),
        Err(Error::LayoutInvalidSegmentBounds { .. })
    ));

    cleanup(&path);
    Ok(())
}

#[test]
fn typed_layout_api_writes_and_reads_tdms_style_segments() -> varve::Result<()> {
    let path = temp_path("tdms_typed_physical_layout");
    cleanup(&path);

    let metadata = b"typed-objects";
    let raw = f64_bytes(&[2.5, 3.5]);

    {
        let mut writer = TdmsPhysicalFormat::create_layout_writer(&path)?;
        let info = writer.write_tdms_segment(TdmsPhysicalFormatTdmsSegmentLayoutWrite {
            fields: TdmsPhysicalFormatTdmsSegmentLayoutFields {
                toc_mask: 0x1120,
                version: 4713,
            },
            footer_fields: TdmsPhysicalFormatTdmsSegmentLayoutFooterFields,
            metadata,
            raw: &raw,
        })?;
        assert_eq!(info.tag()?, b"TDSm");
        assert_eq!(info.toc_mask()?, 0x1120);
        assert_eq!(info.version()?, 4713);
        assert_eq!(
            info.next_segment_offset()?,
            (metadata.len() + raw.len()) as u64
        );
        assert_eq!(info.raw_data_offset()?, metadata.len() as u64);
        writer.flush()?;
    }

    let bytes = read(&path)?;
    assert_eq!(&bytes[0..4], b"TDSm");
    assert_eq!(u32_at(&bytes, 4), 0x1120);
    assert_eq!(u64_at(&bytes, 12), (metadata.len() + raw.len()) as u64);

    let reader = TdmsPhysicalFormat::open_layout_reader(&path)?;
    let segments = reader.tdms_segments()?;
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].toc_mask()?, 0x1120);
    assert_eq!(segments[0].raw_data_offset()?, metadata.len() as u64);
    assert_eq!(reader.tdms_segment(0)?.unwrap().version()?, 4713);
    assert_eq!(reader.read_tdms_segment_metadata(0)?, metadata);
    assert_eq!(
        reader.read_tdms_segment_metadata_range(0, 6, 7)?,
        b"objects"
    );
    assert_eq!(reader.read_tdms_segment_raw(0)?, raw);
    assert_eq!(
        f64_values(&reader.read_tdms_segment_raw_range(0, 8, 8)?),
        vec![3.5]
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn typed_layout_api_handles_caller_file_header_fields() -> varve::Result<()> {
    let path = temp_path("typed_header_physical_layout");
    cleanup(&path);

    {
        let mut writer = HeaderCallerPhysicalFormat::create_layout_writer_with_typed_header(
            &path,
            HeaderCallerPhysicalFormatCallerHeaderLayoutFields { kind: 42 },
        )?;
        let info = writer.write_header_data_segment(
            HeaderCallerPhysicalFormatHeaderDataSegmentLayoutWrite {
                fields: HeaderCallerPhysicalFormatHeaderDataSegmentLayoutFields { kind: 7 },
                footer_fields: HeaderCallerPhysicalFormatHeaderDataSegmentLayoutFooterFields,
                metadata: b"m",
                raw: b"raw",
            },
        )?;
        assert_eq!(info.kind()?, 7);
        writer.flush()?;
    }

    let bytes = read(&path)?;
    assert_eq!(&bytes[0..4], b"HDCT");
    assert_eq!(u16_at(&bytes, 4), 42);
    assert_eq!(&bytes[6..10], b"DATA");
    assert_eq!(u32_at(&bytes, 10), 7);

    let reader = HeaderCallerPhysicalFormat::open_layout_reader(&path)?;
    assert_eq!(reader.file_header_len(), 6);
    assert_eq!(reader.file_header_fields().len(), 2);
    assert_eq!(
        reader.file_header_field("kind"),
        Some(&LayoutValue::U16(42))
    );
    let header = reader.file_header();
    assert_eq!(header.signature()?, b"HDCT".to_vec());
    assert_eq!(header.kind()?, 42);
    assert_eq!(header.fields().len(), 2);
    let segment = reader.header_data_segment(0)?.unwrap();
    assert_eq!(segment.kind()?, 7);
    assert_eq!(reader.read_header_data_segment_metadata(0)?, b"m");
    assert_eq!(reader.read_header_data_segment_raw(0)?, b"raw");

    let inspected = HeaderCallerPhysicalFormat::inspect_layout_file(&path)?;
    assert_eq!(inspected.file_header_len, 6);
    assert_eq!(inspected.file_header_fields.len(), 2);
    assert_eq!(
        inspected.file_header_fields[1],
        LayoutFieldValue {
            name: "kind",
            value: LayoutValue::U16(42),
        }
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn custom_layout_declaration_defaults_to_byte_zero_physical_layout() -> varve::Result<()> {
    let path = temp_path("implicit_physical_layout");
    cleanup(&path);

    assert_eq!(
        ImplicitPhysicalFormat::spec().layout.preset,
        LayoutPreset::None
    );
    {
        let mut writer = ImplicitPhysicalFormat::create_layout_writer(&path)?;
        writer.write_implicit_segment(ImplicitPhysicalFormatImplicitSegmentLayoutWrite {
            fields: ImplicitPhysicalFormatImplicitSegmentLayoutFields { kind: 9 },
            footer_fields: ImplicitPhysicalFormatImplicitSegmentLayoutFooterFields,
            metadata: b"",
            raw: b"abc",
        })?;
        writer.flush()?;
    }

    let bytes = read(&path)?;
    assert_eq!(&bytes[0..4], b"IMPL");
    assert_eq!(u32_at(&bytes, 4), 9);
    assert_eq!(&bytes[24..27], b"abc");
    assert_eq!(
        ImplicitPhysicalFormat::open_layout_reader(&path)?
            .segments()
            .len(),
        1
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn multi_segment_layout_dispatches_by_leading_literal_tag() -> varve::Result<()> {
    let path = temp_path("multi_physical_layout");
    cleanup(&path);

    {
        let mut writer = MultiPhysicalFormat::create_layout_writer(&path)?;
        let control =
            writer.write_control_segment(MultiPhysicalFormatControlSegmentLayoutWrite {
                fields: MultiPhysicalFormatControlSegmentLayoutFields { code: 99 },
                footer_fields: MultiPhysicalFormatControlSegmentLayoutFooterFields,
                metadata: b"control-meta",
                raw: b"",
            })?;
        assert_eq!(control.tag()?, b"CTRL");
        assert_eq!(control.version()?, 1);
        assert_eq!(control.code()?, 99);

        writer.write_data_segment(MultiPhysicalFormatDataSegmentLayoutWrite {
            fields: MultiPhysicalFormatDataSegmentLayoutFields { channel: 7 },
            footer_fields: MultiPhysicalFormatDataSegmentLayoutFooterFields,
            metadata: b"data-a",
            raw: b"aaaa",
        })?;
        writer.write_data_segment(MultiPhysicalFormatDataSegmentLayoutWrite {
            fields: MultiPhysicalFormatDataSegmentLayoutFields { channel: 8 },
            footer_fields: MultiPhysicalFormatDataSegmentLayoutFooterFields,
            metadata: b"data-bb",
            raw: b"bbbbbb",
        })?;

        assert!(matches!(
            writer.write_control_segment(MultiPhysicalFormatControlSegmentLayoutWrite {
                fields: MultiPhysicalFormatControlSegmentLayoutFields { code: 100 },
                footer_fields: MultiPhysicalFormatControlSegmentLayoutFooterFields,
                metadata: b"second",
                raw: b"",
            }),
            Err(Error::LayoutRepeatedOnceSegment { .. })
        ));
        writer.flush()?;
    }

    let reader = MultiPhysicalFormat::open_layout_reader(&path)?;
    assert_eq!(reader.segments().len(), 3);
    assert_eq!(reader.segments()[0].name, "ControlSegment");
    assert_eq!(reader.segments()[1].name, "DataSegment");
    assert_eq!(reader.segments()[2].name, "DataSegment");
    assert_eq!(reader.read_metadata(1)?, b"data-a");
    assert_eq!(reader.read_data_segment_metadata(0)?, b"data-a");
    assert_eq!(reader.read_data_segment_metadata(1)?, b"data-bb");
    assert_eq!(reader.read_data_segment_raw(0)?, b"aaaa");
    assert_eq!(reader.read_data_segment_raw(1)?, b"bbbbbb");

    let controls = reader.control_segments()?;
    let data = reader.data_segments()?;
    assert_eq!(controls.len(), 1);
    assert_eq!(data.len(), 2);
    assert_eq!(reader.control_segment(0)?.unwrap().code()?, 99);
    assert_eq!(reader.data_segment(0)?.unwrap().channel()?, 7);
    assert_eq!(reader.data_segment(1)?.unwrap().channel()?, 8);
    assert!(reader.data_segment(2)?.is_none());

    cleanup(&path);
    Ok(())
}

#[test]
fn multi_segment_layout_dispatches_by_extended_literal_prefix() -> varve::Result<()> {
    let path = temp_path("same_tag_physical_layout");
    cleanup(&path);

    {
        let mut writer = SameTagPhysicalFormat::create_layout_writer(&path)?;
        writer.write_raw_segment(SameTagPhysicalFormatRawSegmentLayoutWrite {
            fields: SameTagPhysicalFormatRawSegmentLayoutFields,
            footer_fields: SameTagPhysicalFormatRawSegmentLayoutFooterFields,
            metadata: b"raw-meta",
            raw: b"raw",
        })?;
        writer.write_meta_segment(SameTagPhysicalFormatMetaSegmentLayoutWrite {
            fields: SameTagPhysicalFormatMetaSegmentLayoutFields,
            footer_fields: SameTagPhysicalFormatMetaSegmentLayoutFooterFields,
            metadata: b"meta-meta",
            raw: b"meta",
        })?;
        writer.flush()?;
    }

    let bytes = read(&path)?;
    assert_eq!(&bytes[0..4], b"STAG");
    assert_eq!(u16_at(&bytes, 4), 2);

    let reader = SameTagPhysicalFormat::open_layout_reader(&path)?;
    assert_eq!(reader.segments().len(), 2);
    assert_eq!(reader.segments()[0].name, "RawSegment");
    assert_eq!(reader.segments()[1].name, "MetaSegment");
    assert_eq!(reader.read_raw_segment_raw(0)?, b"raw");
    assert_eq!(reader.read_meta_segment_raw(0)?, b"meta");
    assert_eq!(reader.raw_segment(0)?.unwrap().kind()?, 2);
    assert_eq!(reader.meta_segment(0)?.unwrap().kind()?, 1);

    cleanup(&path);
    Ok(())
}

#[test]
fn multi_segment_layout_rejects_repeated_once_segment_on_scan() -> varve::Result<()> {
    let path = temp_path("multi_physical_repeated_once_scan");
    cleanup(&path);

    let mut bytes = control_segment_bytes(1, b"first");
    bytes.extend_from_slice(&control_segment_bytes(2, b"second"));
    write(&path, &bytes)?;

    assert!(matches!(
        MultiPhysicalFormat::open_layout_reader(&path),
        Err(Error::LayoutRepeatedOnceSegment { .. })
    ));

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
    assert_eq!(u64_at(&bytes, 12), (metadata.len() + raw.len()) as u64);
    assert_eq!(u64_at(&bytes, 20), metadata.len() as u64);
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
        Some(&LayoutValue::U64((metadata.len() + raw.len()) as u64))
    );
    assert_eq!(
        segment.field("raw_data_offset"),
        Some(&LayoutValue::U64(metadata.len() as u64))
    );
    assert_eq!(segment.metadata_len, metadata.len() as u64);
    assert_eq!(segment.raw_len, raw.len() as u64);
    assert_eq!(reader.read_metadata(0)?, metadata);
    assert_eq!(reader.read_metadata_range(0, 0, 7)?, b"objects");
    assert_eq!(reader.read_raw(0)?, raw);
    assert_eq!(
        f64_values(&reader.read_tdms_segment_raw_range(0, 8, 8)?),
        vec![0.50]
    );

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
fn tdms_physical_adapter_can_encode_metadata_and_raw_with_public_api() -> varve::Result<()> {
    let path = temp_path("tdms_physical_adapter");
    cleanup(&path);

    let first_metadata = TdmsCompatMetadata {
        objects: vec![
            TdmsCompatObject {
                path: s("/"),
                raw_data_index: TdmsRawDataIndex::None,
                properties: vec![TdmsCompatProperty::String {
                    name: s("title"),
                    value: s("Varve physical adapter smoke"),
                }],
            },
            TdmsCompatObject {
                path: s("/'Measured Data'"),
                raw_data_index: TdmsRawDataIndex::None,
                properties: Vec::new(),
            },
            TdmsCompatObject {
                path: s("/'Measured Data'/'Amplitude'"),
                raw_data_index: TdmsRawDataIndex::New {
                    data_type: TDMS_TYPE_DOUBLE_FLOAT,
                    dimensions: 1,
                    values: 4,
                },
                properties: vec![
                    TdmsCompatProperty::String {
                        name: s("unit"),
                        value: s("V"),
                    },
                    TdmsCompatProperty::F64 {
                        name: s("wf_increment"),
                        value: 0.001,
                    },
                ],
            },
        ],
    };
    let second_metadata = TdmsCompatMetadata {
        objects: vec![TdmsCompatObject {
            path: s("/'Measured Data'/'Amplitude'"),
            raw_data_index: TdmsRawDataIndex::New {
                data_type: TDMS_TYPE_DOUBLE_FLOAT,
                dimensions: 1,
                values: 2,
            },
            properties: vec![TdmsCompatProperty::I64 {
                name: s("sample_count"),
                value: 6,
            }],
        }],
    };
    let first_metadata_bytes = encode_tdms_compat_metadata(&first_metadata);
    let second_metadata_bytes = encode_tdms_compat_metadata(&second_metadata);
    let first_raw = f64_bytes(&[0.10, 0.20, 0.30, 0.40]);
    let second_raw = f64_bytes(&[0.50, 0.60]);

    {
        let mut writer = TdmsPhysicalFormat::create_layout_writer(&path)?;
        writer.write_tdms_segment(TdmsPhysicalFormatTdmsSegmentLayoutWrite {
            fields: TdmsPhysicalFormatTdmsSegmentLayoutFields {
                toc_mask: TDMS_TOC_METADATA | TDMS_TOC_NEW_OBJECT_LIST | TDMS_TOC_RAW_DATA,
                version: TDMS_VERSION,
            },
            footer_fields: TdmsPhysicalFormatTdmsSegmentLayoutFooterFields,
            metadata: &first_metadata_bytes,
            raw: &first_raw,
        })?;
        writer.write_tdms_segment(TdmsPhysicalFormatTdmsSegmentLayoutWrite {
            fields: TdmsPhysicalFormatTdmsSegmentLayoutFields {
                toc_mask: TDMS_TOC_METADATA | TDMS_TOC_RAW_DATA,
                version: TDMS_VERSION,
            },
            footer_fields: TdmsPhysicalFormatTdmsSegmentLayoutFooterFields,
            metadata: &second_metadata_bytes,
            raw: &second_raw,
        })?;
        writer.flush()?;
    }

    let bytes = read(&path)?;
    assert_eq!(&bytes[0..4], b"TDSm");

    let reader = TdmsPhysicalFormat::open_layout_reader(&path)?;
    let segments = reader.tdms_segments()?;
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].tag()?, b"TDSm");
    assert_eq!(segments[0].version()?, TDMS_VERSION);
    assert_eq!(
        segments[0].next_segment_offset()?,
        (first_metadata_bytes.len() + first_raw.len()) as u64
    );
    assert_eq!(
        segments[0].raw_data_offset()?,
        first_metadata_bytes.len() as u64
    );
    assert_eq!(
        segments[1].next_segment_offset()?,
        (second_metadata_bytes.len() + second_raw.len()) as u64
    );
    assert_eq!(
        segments[1].raw_data_offset()?,
        second_metadata_bytes.len() as u64
    );

    let decoded_first = decode_tdms_compat_metadata(&reader.read_tdms_segment_metadata(0)?);
    let decoded_second = decode_tdms_compat_metadata(&reader.read_tdms_segment_metadata(1)?);
    assert_eq!(decoded_first, first_metadata);
    assert_eq!(decoded_second, second_metadata);
    assert_eq!(
        f64_values(&reader.read_tdms_segment_raw(0)?),
        vec![0.10, 0.20, 0.30, 0.40]
    );
    assert_eq!(
        f64_values(&reader.read_tdms_segment_raw(1)?),
        vec![0.50, 0.60]
    );
    assert_eq!(segments[1].segment_start(), segments[0].segment_end());

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
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
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

    for (next_offset, raw_offset) in [(4, 8), (u64::MAX, 0), (0, u64::MAX)] {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"TDSm");
        bytes.extend_from_slice(&0x1110u32.to_le_bytes());
        bytes.extend_from_slice(&4713u32.to_le_bytes());
        bytes.extend_from_slice(&next_offset.to_le_bytes());
        bytes.extend_from_slice(&raw_offset.to_le_bytes());
        bytes.extend_from_slice(&[0; 8]);
        write(&bad_bounds, &bytes)?;
        let opened = catch_unwind(|| TdmsPhysicalFormat::open_layout_reader(&bad_bounds));
        assert!(opened.is_ok(), "hostile layout offset caused a panic");
        assert!(matches!(
            opened.expect("checked above"),
            Err(Error::LayoutInvalidSegmentBounds { .. })
                | Err(Error::ResourceArithmeticOverflow { .. })
        ));
    }

    cleanup(&bad_tag);
    cleanup(&truncated);
    cleanup(&bad_bounds);
    Ok(())
}

#[test]
fn custom_layout_rejects_corrupt_declared_file_header_and_footer() -> varve::Result<()> {
    let bad_header = temp_path("custom_bad_file_header");
    let good_footer = temp_path("custom_good_footer");
    let bad_footer = temp_path("custom_bad_footer");
    cleanup(&bad_header);
    cleanup(&good_footer);
    cleanup(&bad_footer);

    let mut header_bytes = Vec::new();
    header_bytes.extend_from_slice(b"BADC");
    header_bytes.extend_from_slice(&42u16.to_le_bytes());
    write(&bad_header, &header_bytes)?;
    assert!(matches!(
        HeaderCallerPhysicalFormat::open_layout_reader(&bad_header),
        Err(Error::LayoutLiteralMismatch { .. })
    ));

    {
        let mut writer = FramedPhysicalFormat::create_layout_writer(&good_footer)?;
        writer.write_data_segment(FramedPhysicalFormatDataSegmentLayoutWrite {
            fields: FramedPhysicalFormatDataSegmentLayoutFields { kind: 7 },
            footer_fields: FramedPhysicalFormatDataSegmentLayoutFooterFields,
            metadata: b"abc",
            raw: b"data",
        })?;
        writer.flush()?;
    }

    let mut bytes = read(&good_footer)?;
    let footer_offset = FramedPhysicalFormat::open_layout_reader(&good_footer)?.segments()[0]
        .footer_offset as usize;
    bytes[footer_offset..footer_offset + 4].copy_from_slice(b"BAD!");
    write(&bad_footer, &bytes)?;
    assert!(matches!(
        FramedPhysicalFormat::open_layout_reader(&bad_footer),
        Err(Error::LayoutLiteralMismatch { .. })
    ));

    cleanup(&bad_header);
    cleanup(&good_footer);
    cleanup(&bad_footer);
    Ok(())
}

const TDMS_VERSION: u32 = 4713;
const TDMS_TOC_METADATA: u32 = 1 << 1;
const TDMS_TOC_NEW_OBJECT_LIST: u32 = 1 << 2;
const TDMS_TOC_RAW_DATA: u32 = 1 << 3;
const TDMS_RAW_INDEX_NONE: u32 = 0xFFFF_FFFF;
const TDMS_RAW_INDEX_LEN: u32 = 20;
const TDMS_TYPE_I64: u32 = 4;
const TDMS_TYPE_DOUBLE_FLOAT: u32 = 10;
const TDMS_TYPE_STRING: u32 = 0x20;

#[derive(Clone, Debug, PartialEq)]
struct TdmsCompatMetadata {
    objects: Vec<TdmsCompatObject>,
}

#[derive(Clone, Debug, PartialEq)]
struct TdmsCompatObject {
    path: String,
    raw_data_index: TdmsRawDataIndex,
    properties: Vec<TdmsCompatProperty>,
}

#[derive(Clone, Debug, PartialEq)]
enum TdmsRawDataIndex {
    None,
    New {
        data_type: u32,
        dimensions: u32,
        values: u64,
    },
}

#[derive(Clone, Debug, PartialEq)]
enum TdmsCompatProperty {
    String { name: String, value: String },
    I64 { name: String, value: i64 },
    F64 { name: String, value: f64 },
}

fn encode_tdms_compat_metadata(metadata: &TdmsCompatMetadata) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_u32(&mut bytes, metadata.objects.len() as u32);
    for object in &metadata.objects {
        push_tdms_string(&mut bytes, &object.path);
        push_tdms_raw_index(&mut bytes, &object.raw_data_index);
        push_u32(&mut bytes, object.properties.len() as u32);
        for property in &object.properties {
            match property {
                TdmsCompatProperty::String { name, value } => {
                    push_tdms_string(&mut bytes, name);
                    push_u32(&mut bytes, TDMS_TYPE_STRING);
                    push_tdms_string(&mut bytes, value);
                }
                TdmsCompatProperty::I64 { name, value } => {
                    push_tdms_string(&mut bytes, name);
                    push_u32(&mut bytes, TDMS_TYPE_I64);
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
                TdmsCompatProperty::F64 { name, value } => {
                    push_tdms_string(&mut bytes, name);
                    push_u32(&mut bytes, TDMS_TYPE_DOUBLE_FLOAT);
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
    }
    bytes
}

fn push_tdms_raw_index(bytes: &mut Vec<u8>, index: &TdmsRawDataIndex) {
    match index {
        TdmsRawDataIndex::None => push_u32(bytes, TDMS_RAW_INDEX_NONE),
        TdmsRawDataIndex::New {
            data_type,
            dimensions,
            values,
        } => {
            push_u32(bytes, TDMS_RAW_INDEX_LEN);
            push_u32(bytes, *data_type);
            push_u32(bytes, *dimensions);
            push_u64(bytes, *values);
        }
    }
}

fn decode_tdms_compat_metadata(bytes: &[u8]) -> TdmsCompatMetadata {
    let mut cursor = TdmsCompatCursor { bytes, position: 0 };
    let objects = (0..cursor.u32())
        .map(|_| {
            let path = cursor.string();
            let raw_data_index = cursor.raw_data_index();
            let properties = (0..cursor.u32())
                .map(|_| {
                    let name = cursor.string();
                    match cursor.u32() {
                        TDMS_TYPE_STRING => TdmsCompatProperty::String {
                            name,
                            value: cursor.string(),
                        },
                        TDMS_TYPE_I64 => TdmsCompatProperty::I64 {
                            name,
                            value: cursor.i64(),
                        },
                        TDMS_TYPE_DOUBLE_FLOAT => TdmsCompatProperty::F64 {
                            name,
                            value: cursor.f64(),
                        },
                        other => panic!("unknown TDMS property type {other}"),
                    }
                })
                .collect();
            TdmsCompatObject {
                path,
                raw_data_index,
                properties,
            }
        })
        .collect();
    assert_eq!(cursor.remaining(), 0);
    TdmsCompatMetadata { objects }
}

struct TdmsCompatCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> TdmsCompatCursor<'a> {
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

    fn i64(&mut self) -> i64 {
        let mut value = [0; 8];
        value.copy_from_slice(self.bytes(8));
        i64::from_le_bytes(value)
    }

    fn f64(&mut self) -> f64 {
        let mut value = [0; 8];
        value.copy_from_slice(self.bytes(8));
        f64::from_le_bytes(value)
    }

    fn string(&mut self) -> String {
        let len = self.u32() as usize;
        String::from_utf8(self.bytes(len).to_vec()).expect("TDMS metadata string is UTF-8")
    }

    fn raw_data_index(&mut self) -> TdmsRawDataIndex {
        match self.u32() {
            TDMS_RAW_INDEX_NONE => TdmsRawDataIndex::None,
            TDMS_RAW_INDEX_LEN => TdmsRawDataIndex::New {
                data_type: self.u32(),
                dimensions: self.u32(),
                values: self.u64(),
            },
            other => panic!("unsupported TDMS raw data index length {other}"),
        }
    }
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_tdms_string(bytes: &mut Vec<u8>, value: &str) {
    let value = value.as_bytes();
    push_u32(
        bytes,
        u32::try_from(value.len()).expect("TDMS string fits u32"),
    );
    bytes.extend_from_slice(value);
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

fn s(value: &str) -> String {
    value.to_string()
}

fn control_segment_bytes(code: u32, metadata: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"CTRL");
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&code.to_le_bytes());
    bytes.extend_from_slice(&(metadata.len() as i64).to_le_bytes());
    bytes.extend_from_slice(&(metadata.len() as i64).to_le_bytes());
    bytes.extend_from_slice(metadata);
    bytes
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

struct TempPath {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for TempPath {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

impl AsRef<std::path::Path> for TempPath {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

fn temp_path(name: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir
        .path()
        .join(format!("varve_{name}_{}.bin", std::process::id()));
    TempPath { path, _dir: dir }
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
