use std::collections::HashMap;
use std::fs::{OpenOptions, remove_file};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;
use varve::{
    Endian, Error, RecoveryPolicy, VarveBlock, VarveMerge, WireType, decode_from_slice,
    encode_to_vec, varve_format,
};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 50, version = 1, kind = "fixed")]
struct PropPoint {
    x: u32,
    y: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 51, version = 1, kind = "variable")]
struct PropVariable {
    #[varve(field_id = 1)]
    id: u32,
    #[varve(field_id = 2)]
    name: String,
    #[varve(field_id = 3)]
    values: Vec<i64>,
    #[varve(field_id = 4)]
    payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 52, version = 1, kind = "variable", key = "id")]
struct PropUser {
    #[varve(field_id = 1)]
    id: u32,
    #[varve(field_id = 2)]
    name: String,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 53, version = 1, kind = "variable")]
struct PropUserOp {
    #[varve(field_id = 1)]
    name: String,
}

impl VarveMerge for PropUser {
    type Op = PropUserOp;

    fn apply_op(&mut self, op: Self::Op) -> varve::Result<()> {
        self.name = op.name;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 54, version = 1, kind = "variable")]
struct PropRewrite {
    #[varve(field_id = 1)]
    id: u32,
    #[varve(field_id = 2)]
    text: String,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 55, version = 1, kind = "variable")]
struct PropScalarEdges {
    #[varve(field_id = 1)]
    unsigned: (u8, u16, u32, u64),
    #[varve(field_id = 2)]
    signed: (i8, i16, i32, i64),
    #[varve(field_id = 3)]
    wide: (u128, i128),
    #[varve(field_id = 4)]
    floats: (f32, f64),
    #[varve(field_id = 5)]
    maybe: Option<(i32, String)>,
}

varve_format! {
    pub struct PropFormat {
        magic: b"PROP";
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
        blocks: [PropPoint, PropVariable, PropUser, PropUserOp, PropRewrite, PropScalarEdges];
    }
}

#[derive(Clone, Debug)]
enum KeyedStep {
    Put { key: u32, name: String },
    Rename { key: u32, name: String },
    Delete { key: u32 },
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn fixed_blocks_roundtrip(points in prop::collection::vec((any::<u32>(), any::<u32>()), 0..32)) {
        let path = temp_path();
        cleanup(&path);

        {
            let mut file = PropFormat::create(&path).unwrap();
            for (x, y) in &points {
                file.push(&PropPoint { x: *x, y: *y }).unwrap();
            }
            file.flush().unwrap();
        }

        let file = PropFormat::open_readonly(&path).unwrap();
        let read = file.blocks::<PropPoint>().unwrap();
        prop_assert_eq!(read.len(), points.len());
        for (index, (x, y)) in points.iter().enumerate() {
            prop_assert_eq!(read.get(index).unwrap(), Some(PropPoint { x: *x, y: *y }));
        }

        cleanup(&path);
    }

    #[test]
    fn variable_blocks_roundtrip(
        values in prop::collection::vec(prop_variable_strategy(), 0..24)
    ) {
        let path = temp_path();
        cleanup(&path);

        {
            let mut file = PropFormat::create(&path).unwrap();
            for value in &values {
                file.push(value).unwrap();
            }
            file.flush().unwrap();
        }

        let file = PropFormat::open_readonly(&path).unwrap();
        let read = file.blocks::<PropVariable>().unwrap();
        prop_assert_eq!(read.len(), values.len());
        for (index, expected) in values.iter().enumerate() {
            prop_assert_eq!(read.get(index).unwrap(), Some(expected.clone()));
        }

        cleanup(&path);
    }

    #[test]
    fn variable_unknown_fields_are_skipped_and_unknown_wire_types_rejected(
        value in prop_variable_strategy(),
        unknown_payload in prop::collection::vec(any::<u8>(), 0..16)
    ) {
        let mut encoded = encode_to_vec(&value, Endian::Little).unwrap();
        let unknown_payload = encode_to_vec(&unknown_payload, Endian::Little).unwrap();
        append_field(&mut encoded, 9_999, WireType::Bytes, &unknown_payload);

        let decoded: PropVariable = decode_from_slice(&encoded, Endian::Little).unwrap();
        prop_assert_eq!(decoded, value);

        encoded.extend_from_slice(&10_000u32.to_le_bytes());
        encoded.extend_from_slice(&u16::MAX.to_le_bytes());
        encoded.extend_from_slice(&0u16.to_le_bytes());
        encoded.extend_from_slice(&0u64.to_le_bytes());
        prop_assert!(matches!(
            decode_from_slice::<PropVariable>(&encoded, Endian::Little),
            Err(Error::UnknownWireType(u16::MAX))
        ));
    }

    #[test]
    fn keyed_put_op_tombstone_materialization_matches_model(
        steps in prop::collection::vec(keyed_step_strategy(), 0..40)
    ) {
        let path = temp_path();
        cleanup(&path);
        let mut last_put = HashMap::new();
        let mut materialized = HashMap::new();

        {
            let mut file = PropFormat::create(&path).unwrap();
            for key in 0..4u32 {
                let name = format!("seed{key}");
                let user = PropUser { id: key, name };
                last_put.insert(key, user.clone());
                materialized.insert(key, user.clone());
                file.push(&user).unwrap();
            }

            for step in steps {
                match step {
                    KeyedStep::Put { key, name } => {
                        let user = PropUser { id: key, name };
                        last_put.insert(key, user.clone());
                        materialized.insert(key, user.clone());
                        file.push(&user).unwrap();
                    }
                    KeyedStep::Rename { key, name } if materialized.contains_key(&key) => {
                        file.push_op::<PropUser>(&key, &PropUserOp { name: name.clone() }).unwrap();
                        materialized.get_mut(&key).unwrap().name = name;
                    }
                    KeyedStep::Rename { key, name } => {
                        let user = PropUser { id: key, name };
                        last_put.insert(key, user.clone());
                        materialized.insert(key, user.clone());
                        file.push(&user).unwrap();
                    }
                    KeyedStep::Delete { key } => {
                        file.delete::<PropUser>(&key).unwrap();
                        last_put.remove(&key);
                        materialized.remove(&key);
                    }
                }
            }
            file.flush().unwrap();
        }

        let file = PropFormat::open_readonly(&path).unwrap();
        let keyed = file.keyed_blocks::<PropUser>().unwrap();
        for key in 0..4u32 {
            prop_assert_eq!(keyed.get(&key).unwrap(), last_put.get(&key).cloned());
        }
        prop_assert_eq!(file.materialized_keyed_blocks::<PropUser>().unwrap(), materialized);

        cleanup(&path);
    }

    #[test]
    fn rewrite_preserves_surrounding_variable_records_and_updates_target(
        records in prop::collection::vec(rewrite_strategy(), 1..16),
        replace_index in 0usize..16,
        replacement in rewrite_strategy()
    ) {
        let path = temp_path();
        cleanup(&path);
        let target_index = replace_index % records.len();
        let mut expected = records.clone();
        expected[target_index] = replacement.clone();

        {
            let mut file = PropFormat::create(&path).unwrap();
            file.push(&PropPoint { x: 1, y: 2 }).unwrap();
            for record in &records {
                file.push(record).unwrap();
            }
            file.push(&PropPoint { x: 3, y: 4 }).unwrap();
            file.replace_rewrite(target_index, &replacement).unwrap();
            file.flush().unwrap();
        }

        let file = PropFormat::open_readonly(&path).unwrap();
        let read = file.blocks::<PropRewrite>().unwrap();
        prop_assert_eq!(read.len(), expected.len());
        for (index, expected_record) in expected.into_iter().enumerate() {
            prop_assert_eq!(read.get(index).unwrap(), Some(expected_record));
        }
        prop_assert_eq!(file.blocks::<PropPoint>().unwrap().len(), 2);

        cleanup(&path);
    }

    #[test]
    fn recovery_truncates_incomplete_tail_without_dropping_complete_records(
        values in prop::collection::vec(any::<u32>(), 0..24),
        use_payload_tail in any::<bool>()
    ) {
        let path = temp_path();
        cleanup(&path);

        {
            let mut file = PropFormat::create(&path).unwrap();
            for value in &values {
                file.push(&PropPoint { x: *value, y: value.wrapping_mul(3) }).unwrap();
            }
            file.flush().unwrap();
        }

        let original_len = std::fs::metadata(&path).unwrap().len();
        if use_payload_tail {
            append_incomplete_payload(&path);
        } else {
            append_partial_record_header(&path);
        }
        let corrupt_len = std::fs::metadata(&path).unwrap().len();

        if use_payload_tail {
            let strict_rejected = matches!(
                PropFormat::open_readonly(&path),
                Err(Error::CorruptTail { .. })
            );
            prop_assert!(strict_rejected);
        } else {
            let readonly = PropFormat::open_readonly(&path).unwrap();
            prop_assert_eq!(readonly.blocks::<PropPoint>().unwrap().len(), values.len());
            prop_assert_eq!(std::fs::metadata(&path).unwrap().len(), corrupt_len);
        }

        let spec = PropFormat::spec().with_recovery_policy(RecoveryPolicy::TruncateTail);
        let (file, report) = spec.open_recover_with_report(&path).unwrap();
        prop_assert_eq!(report.original_len, corrupt_len);
        prop_assert_eq!(report.recovered_len, original_len);
        prop_assert_eq!(report.records_preserved, values.len());
        prop_assert_eq!(std::fs::metadata(&path).unwrap().len(), original_len);
        prop_assert_eq!(file.blocks::<PropPoint>().unwrap().len(), values.len());

        cleanup(&path);
    }

    #[test]
    fn scalar_edges_roundtrip_signed_unsigned_floats_tuples_and_options(
        value in scalar_edges_strategy()
    ) {
        let path = temp_path();
        cleanup(&path);

        {
            let mut file = PropFormat::create(&path).unwrap();
            file.push(&value).unwrap();
            file.flush().unwrap();
        }

        let file = PropFormat::open_readonly(&path).unwrap();
        let decoded = file.blocks::<PropScalarEdges>().unwrap().get(0).unwrap().unwrap();
        prop_assert_eq!(decoded.unsigned, value.unsigned);
        prop_assert_eq!(decoded.signed, value.signed);
        prop_assert_eq!(decoded.wide, value.wide);
        prop_assert_eq!(decoded.floats.0.to_bits(), value.floats.0.to_bits());
        prop_assert_eq!(decoded.floats.1.to_bits(), value.floats.1.to_bits());
        prop_assert_eq!(decoded.maybe, value.maybe.clone());

        let tuple = (value.signed.2, value.unsigned.2, value.maybe.clone());
        let tuple_encoded = encode_to_vec(&tuple, Endian::Little).unwrap();
        prop_assert_eq!(
            decode_from_slice::<(i32, u32, Option<(i32, String)>)>(
                &tuple_encoded,
                Endian::Little
            )
            .unwrap(),
            tuple
        );

        cleanup(&path);
    }
}

fn prop_variable_strategy() -> impl Strategy<Value = PropVariable> {
    (
        any::<u32>(),
        "[a-z0-9]{0,16}",
        prop::collection::vec(any::<i64>(), 0..12),
        prop::collection::vec(any::<u8>(), 0..24),
    )
        .prop_map(|(id, name, values, payload)| PropVariable {
            id,
            name,
            values,
            payload,
        })
}

fn keyed_step_strategy() -> impl Strategy<Value = KeyedStep> {
    prop_oneof![
        (0u32..4, "[a-z0-9]{0,12}").prop_map(|(key, name)| KeyedStep::Put { key, name }),
        (0u32..4, "[a-z0-9]{0,12}").prop_map(|(key, name)| KeyedStep::Rename { key, name }),
        (0u32..4).prop_map(|key| KeyedStep::Delete { key }),
    ]
}

fn rewrite_strategy() -> impl Strategy<Value = PropRewrite> {
    (any::<u32>(), "[a-z0-9]{0,32}").prop_map(|(id, text)| PropRewrite { id, text })
}

fn scalar_edges_strategy() -> impl Strategy<Value = PropScalarEdges> {
    (
        (any::<u8>(), any::<u16>(), any::<u32>(), any::<u64>()),
        (any::<i8>(), any::<i16>(), any::<i32>(), any::<i64>()),
        (any::<u128>(), any::<i128>()),
        (any::<f32>(), any::<f64>()),
        prop::option::of((any::<i32>(), "[a-z0-9]{0,16}")),
    )
        .prop_map(|(unsigned, signed, wide, floats, maybe)| PropScalarEdges {
            unsigned,
            signed,
            wide,
            floats,
            maybe,
        })
}

fn append_field(encoded: &mut Vec<u8>, field_id: u32, wire_type: WireType, payload: &[u8]) {
    encoded.extend_from_slice(&field_id.to_le_bytes());
    encoded.extend_from_slice(&(wire_type as u16).to_le_bytes());
    encoded.extend_from_slice(&0u16.to_le_bytes());
    encoded.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    encoded.extend_from_slice(payload);
}

fn append_partial_record_header(path: &PathBuf) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(&[1, 2, 3, 4]).unwrap();
}

fn append_incomplete_payload(path: &PathBuf) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(&PropPoint::ID.to_le_bytes()).unwrap();
    file.write_all(&PropPoint::VERSION.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&99u64.to_le_bytes()).unwrap();
    file.write_all(&100u64.to_le_bytes()).unwrap();
    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.write_all(&[0xAA]).unwrap();
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

fn temp_path() -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir.path().join(format!(
        "varve_prop_{}_{}_{}.vrv",
        std::process::id(),
        std::thread::current().name().unwrap_or("anon"),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    TempPath { path, _dir: dir }
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
