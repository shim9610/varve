use varve::varve_format;

varve_format! {
    pub format DuplicateLimitKey {
        magic: b"DUP_LIMIT";
        version: 1;
        limits {
            file_len: 1;
            file_len: 2;
        }
        blocks {
            fixed Item(id = 1) {
                value: u32,
            }
        }
    }
}

fn main() {}
