use varve::varve_format;

varve_format! {
    pub format MatrixAuxFormat {
        magic: b"MAUX";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            record_payload: 67_108_864;
            materialized_bytes: 1_073_741_824;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
        }
        endian: little;
        schema_hash: computed;

        dims {
            scan: u32,
            ch: u32,
        }

        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
        };

        aux {
            thumbnail: 128,
        }

        blocks {
            matrix AuxCell(id = 20, dims = [scan, ch], category = analysis) {
                value: u32,
            }
        }
    }
}

fn share(reader: std::sync::Arc<MatrixAuxFormatReader>) {
    std::thread::spawn(move || reader.thumbnail_aux_len());
}

fn main() {}
