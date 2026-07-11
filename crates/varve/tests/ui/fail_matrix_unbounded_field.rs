use varve::varve_format;

varve_format! {
    pub format BadMatrixFormat {
        magic: b"BMTX";
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
        dims {
            scan: u32,
            ch: u32,
        }
        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
        };
        blocks {
            matrix BadCell(id = 1, dims = [scan, ch], category = analysis) {
                value: String,
            }
        }
    }
}

fn main() {}
