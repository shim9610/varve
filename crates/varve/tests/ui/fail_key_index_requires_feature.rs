use varve::varve_format;

varve_format! {
    pub format FeatureGateFormat {
        magic: b"KIDF";
        version: 1;
        blocks {
            variable Item(id = 1, key = [id], key_index = disk) {
                id: u64,
            }
        }
    }
}

fn main() {}
