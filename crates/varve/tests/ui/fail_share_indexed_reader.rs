use std::sync::Arc;
use varve::VarveIndexedReader;

fn share(reader: Arc<VarveIndexedReader>) {
    std::thread::spawn(move || {
        let _ = reader.events();
    });
}

fn main() {}
