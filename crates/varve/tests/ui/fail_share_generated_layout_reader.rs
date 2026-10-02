use varve::varve_format;

varve_format! {
    pub format LocalLayout {
        magic: b"LAY";
        version: 1;
        layout {
            segment DataSegment repeat until_eof {
                lead_in DataLeadIn {
                    bytes tag = b"SEGM";
                    u32 kind;
                }
                metadata DataMetadata;
                raw_region DataRaw;
            }
        }
    }
}

fn share(reader: std::sync::Arc<LocalLayoutLayoutReader>) {
    std::thread::spawn(move || reader.segment_count());
}

fn main() {}
