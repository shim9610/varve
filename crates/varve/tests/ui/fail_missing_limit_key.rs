use varve::varve_format;

varve_format! {
    pub format MissingLimitKey {
        magic: b"MISS_LIMIT";
        version: 1;
        limits {
            file_len: 1;
        }
        blocks {
            fixed Item(id = 1) {
                value: u32,
            }
        }
    }
}

fn main() {}
