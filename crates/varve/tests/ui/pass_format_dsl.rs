use varve::{CommitPolicy, IndexPolicy, VarveBlock, varve_format};

varve_format! {
    pub format GeneratedFormat {
        magic: b"GEN";
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
        index: [scan_on_open, block_offset_chain, keyed_offset_chain];
        commit: record_footer;
        blocks {
            fixed GeneratedPoint(id = 1) {
                x: u32,
                y: u32,
            }

            variable GeneratedUser(id = 2, key = [id]) {
                id: u64,
                name: String,
                tags: Vec<String> = default,
            }
        }
    }
}

fn main() {
    let spec = GeneratedFormat::spec();
    assert_eq!(
        spec.index_policy,
        IndexPolicy::new(true, false, true, true)
    );
    assert_eq!(spec.commit_policy, CommitPolicy::RecordFooter);
    assert_ne!(spec.schema_hash, 0);

    let _point = GeneratedPoint { x: 1, y: 2 };
    let user = GeneratedUser {
        id: 9,
        name: "Ada".to_string(),
        tags: Vec::new(),
    };
    assert_eq!(GeneratedUser::ID, 2);
    assert_eq!(<GeneratedUser as varve::VarveKeyedBlock>::key(&user), 9);
}
