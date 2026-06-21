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
