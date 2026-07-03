use varve::{varve_format, VarveBlock};

struct Ok;
struct Some;
struct None;
struct Vec;

varve_format! {
    pub format HygieneFormat {
        magic: b"HYG";
        version: 1;
        endian: little;
        extension: "hyg";
        blocks {
            variable HygieneUser(id = 1, key = [id]) {
                id: u64,
                name: String,
            }
        }
    }
}

fn main() {
    let spec = HygieneFormat::spec();
    assert_eq!(spec.extension, ::core::option::Option::Some("hyg"));
    let _ = HygieneFormat::open_reader("missing.varve");
}
