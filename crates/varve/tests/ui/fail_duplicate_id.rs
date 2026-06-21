use varve::{varve_format, VarveBlock};

#[derive(Clone, VarveBlock)]
#[varve(id = 200, kind = "fixed")]
struct A {
    value: u32,
}

#[derive(Clone, VarveBlock)]
#[varve(id = 200, kind = "fixed")]
struct B {
    value: u32,
}

varve_format! {
    pub struct BadFormat {
        magic: b"BAD";
        version: 1;
        endian: little;
        blocks: [A, B];
    }
}

fn main() {
    let _ = BadFormat::spec();
}

