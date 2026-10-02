use std::sync::Arc;
use varve::VarveReader;

fn share(reader: Arc<VarveReader>) {
    std::thread::spawn(move || reader.matrix_aux_len("thumbnail"));
}

fn main() {}
