use varve::{CommitPolicy, IndexPolicy, VarveBlock, varve_format};

varve_format! {
    pub format GeneratedFormat {
        magic: b"GEN";
        version: 1;
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
