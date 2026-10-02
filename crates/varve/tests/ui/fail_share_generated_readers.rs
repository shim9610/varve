use std::sync::Arc;
use varve::varve_format;

varve_format! {
    pub format ThreadOwned {
        magic: b"THRD";
        version: 1;
        blocks {
            fixed Item(id = 1, key = [key], key_index = disk) { key: u64 }
        }
    }
}

fn stream(reader: Arc<ThreadOwnedStreamReader>) {
    std::thread::spawn(move || {
        let _ = reader.events();
    });
}

fn indexed(reader: Arc<ThreadOwnedIndexedReader>) {
    std::thread::spawn(move || {
        let _ = reader.get_item(&0);
    });
}

fn main() {}
