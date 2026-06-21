use varve::{
    LayoutFieldSource, LayoutFieldType, LayoutPartKind, LayoutPreset, SegmentRepeat, varve_format,
};

varve_format! {
    pub format LayoutOnlyFormat {
        magic: b"LAY";
        version: 1;
        endian: little;
        schema_hash: computed;
        extension: "tdms";
        preset: none;

        layout {
            file_header TdmsFileHeader {
                bytes tag = b"TDMF";
                u16 version = 1;
            }

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

                footer TdmsFooter {
                    bytes seal = b"DONE";
                    u64 segment_len = finalize(target = segment_end, relative_to = segment_start);
                }
            }
        }
    }
}

fn main() {
    let spec = LayoutOnlyFormat::spec();
    assert_eq!(spec.layout.preset, LayoutPreset::None);
    assert_ne!(spec.schema_hash, 0);
    assert_eq!(spec.layout.parts.len(), 2);
    let LayoutPartKind::FileHeader(header) = spec.layout.parts[0].kind else {
        panic!("expected file header layout part");
    };
    assert_eq!(header.fields.len(), 2);
    let LayoutPartKind::Segment(segment) = spec.layout.parts[1].kind else {
        panic!("expected segment layout part");
    };
    assert_eq!(segment.name, "TdmsSegment");
    assert_eq!(segment.repeat, SegmentRepeat::UntilEof);
    assert_eq!(segment.lead_in.fields.len(), 5);
    assert!(segment.footer.is_some());
    assert_eq!(segment.lead_in.fields[0].ty, LayoutFieldType::Bytes { len: 4 });
    match segment.lead_in.fields[0].source {
        LayoutFieldSource::LiteralBytes(bytes) => assert_eq!(bytes, b"TDSm"),
        _ => panic!("expected literal tag"),
    }

    let _header = LayoutOnlyFormatTdmsFileHeaderLayoutFields;
    let _write = LayoutOnlyFormatTdmsSegmentLayoutWrite {
        fields: LayoutOnlyFormatTdmsSegmentLayoutFields {
            toc_mask: 1,
            version: 4713,
        },
        footer_fields: LayoutOnlyFormatTdmsSegmentLayoutFooterFields,
        metadata: b"metadata",
        raw: b"raw",
    };
    fn _accept_writer(_: LayoutOnlyFormatLayoutWriter) {}
    fn _accept_reader(_: LayoutOnlyFormatLayoutReader) {}
}
