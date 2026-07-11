use varve::varve_format;

varve_format! {
    pub format UnknownLimitKey {
        magic: b"UNKNOWN_LIMIT";
        version: 1;
        limits {
            file_len: 1;
            mystery_bytes: 1;
        }
        blocks {
            fixed Item(id = 1) {
                value: u32,
            }
        }
    }
}

fn main() {}
