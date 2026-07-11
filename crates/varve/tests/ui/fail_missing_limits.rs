use varve::varve_format;

varve_format! {
    pub format MissingLimits {
        magic: b"NO_LIMITS";
        version: 1;
        blocks {
            fixed Item(id = 1) {
                value: u32,
            }
        }
    }
}

fn main() {}
