use varve::varve_format;
varve_format! {
    pub format Unchecked {
        magic: b"HASH"; version: 1;
        blocks {
            fixed Item(id = 1, key = [id], key_domain = [id = 0..4]) { id: u16 }
        }
    }
}
fn main() {}
