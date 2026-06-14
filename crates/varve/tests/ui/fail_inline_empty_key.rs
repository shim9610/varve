use varve::varve_format;

varve_format! {
    pub format EmptyInlineKeyFormat {
        magic: b"EMPTYKEY";
        version: 1;
        endian: little;
        blocks {
            variable BadUser(id = 1, key = []) {
                id: u64,
                name: String,
            }
        }
    }
}

fn main() {}
