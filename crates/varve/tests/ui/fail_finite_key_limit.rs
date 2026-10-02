use varve::varve_format;
varve_format! {
    pub format TooMany {
        magic: b"LIMIT"; version: 1; schema_hash: computed;
        blocks {
            fixed Frame(id = 1, key = [scan, frame],
                key_domain = [scan = 0..64, frame = 0..65]) {
                scan: u16, frame: u16,
            }
        }
    }
}
fn main() {}
