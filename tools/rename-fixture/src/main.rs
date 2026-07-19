//! API2-05 regression fixture: the `varve` dependency is renamed to `vv` in
//! Cargo.toml and there is deliberately NO `extern crate vv as varve`
//! workaround anywhere. Every macro expansion below must reference the facade
//! through the resolved name, otherwise this crate fails to compile with
//! E0433 ("cannot find `varve` in the crate root").
//!
//! Compiling this crate is the regression test; the `main` body additionally
//! round-trips a file so the generated code is exercised, not just
//! type-checked.

use vv::{VarveBlock, varve_format};

// Exercises the derive path, including the `IDENT: ::varve::__core::…`
// type-ascription shape (const items, typed params) that a colon-run rewrite
// bug once missed.
#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 7, version = 1, kind = "fixed")]
struct Point {
    x: u32,
    y: u32,
}

// Exercises the full varve_format! expansion (spec construction, block
// registration, generated reader/writer APIs, keyed blocks).
varve_format! {
    pub format RenamedFormat {
        magic: b"RNMD";
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
        extension: "vrnm";
        index: [scan_on_open, checkpoint_on_flush, block_offset_chain, keyed_offset_chain];
        commit: transaction_marker(on_flush);
        manifest: embedded;
        blocks {
            fixed Cell(id = 1) {
                a: u32,
                b: u32,
            }

            variable Item(id = 2, key = [id]) {
                id: u64,
                name: String,
            }
        }
    }
}

fn main() {
    let spec = RenamedFormat::spec();
    assert!(spec.computed_schema_hash() != 0);
    assert_eq!(Point::ID, 7);

    // Runtime round-trip in a self-cleaning temp directory.
    let dir = std::env::temp_dir().join(format!("varve-rename-fixture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let result = std::panic::catch_unwind(|| {
        let path = dir.join("renamed.vrnm");
        {
            let mut file = vv::VarveFile::create(spec, &path).unwrap();
            file.push(&Cell { a: 1, b: 2 }).unwrap();
            file.push(&Item {
                id: 9,
                name: "renamed".to_string(),
            })
            .unwrap();
            file.flush().unwrap();
        }
        let file = vv::VarveFile::open_readonly(spec, &path).unwrap();
        let cells = file.blocks::<Cell>().unwrap();
        let cells: Vec<Cell> = cells.iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(cells, vec![Cell { a: 1, b: 2 }]);
    });
    let cleanup = std::fs::remove_dir_all(&dir);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    cleanup.unwrap();
    println!("renamed-dependency fixture OK");
}
