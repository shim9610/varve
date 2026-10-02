use varve::varve_format;
varve_format! {
    pub format Checked {
        magic: b"RAW"; version: 1; schema_hash: computed;
        blocks {
            fixed Frame(id = 1, key = [scan, frame],
                key_values = [First = (1, 2)]) { scan: u16, frame: u16 }
        }
    }
}
fn main() {
    let _ = Frame { scan: 999, frame: 999 };
    let _ = Frame { key: 999 };
}
