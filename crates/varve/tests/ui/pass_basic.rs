use varve::{varve_format, FieldPresence, VarveBlock, WireType};

#[derive(Clone, VarveBlock)]
#[varve(id = 100, kind = "fixed")]
struct A {
    value: u32,
}

#[derive(Clone, VarveBlock)]
#[varve(id = 101, kind = "variable", key = "id")]
struct B {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    text: String,
}

varve_format! {
    pub struct BasicFormat {
        magic: b"BASIC";
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
        manifest: none;
        blocks: [A, B];
    }
}

fn main() {
    let _ = BasicFormat::spec();
    let a_fields = <A as VarveBlock>::FIELDS;
    assert_eq!(a_fields.len(), 1);
    assert_eq!(a_fields[0].id, 1);
    assert_eq!(a_fields[0].name, "value");
    assert_eq!(a_fields[0].wire_type, WireType::U32);
    assert_eq!(a_fields[0].presence, FieldPresence::Required);

    let b_fields = <B as VarveBlock>::FIELDS;
    assert_eq!(b_fields.len(), 2);
    assert_eq!(b_fields[0].id, 1);
    assert_eq!(b_fields[1].wire_type, WireType::String);
}
