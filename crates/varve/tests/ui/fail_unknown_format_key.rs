use varve::{varve_format, VarveBlock};

#[derive(Clone, VarveBlock)]
#[varve(id = 413, kind = "fixed")]
struct A {
    value: u32,
}

varve_format! {
    pub struct BadFormat {
        magic: b"BADFMT";
        version: 1;
        endian: little;
        cache: none;
        blocks: [A];
    }
}

fn main() {}
