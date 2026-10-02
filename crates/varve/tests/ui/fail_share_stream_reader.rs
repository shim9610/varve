use std::sync::Arc;
use varve::VarveStreamReader;

fn share(reader: Arc<VarveStreamReader>) {
    std::thread::spawn(move || {
        let _ = reader.events();
    });
}

fn main() {}
