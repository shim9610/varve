//! F-10 companion: dropping the primitive-spelling whitelist must not drop the
//! rejection. A tuple can never denote a type with a fixed matrix slot stride
//! whatever it resolves to, so the macro still refuses it with a readable
//! diagnostic pointing at the offending field.

use varve::varve_format;

varve_format! {
    pub format TupleMatrixFormat {
        magic: b"TMTX";
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
                value: (u32, u32),
            }
        }
    }
}

fn main() {}
