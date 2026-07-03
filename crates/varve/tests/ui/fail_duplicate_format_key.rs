use varve::varve_format;

varve_format! {
    pub format DuplicateKeyFormat {
        magic: b"DUP";
        magic: b"DUP2";
        version: 1;
        endian: little;
        blocks {
            fixed Item(id = 1) {
                value: u32,
            }
        }
    }
}

fn main() {}
