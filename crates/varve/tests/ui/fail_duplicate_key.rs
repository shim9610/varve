use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(id = 403, kind = "variable", key = "id,id")]
struct DuplicateKey {
    #[varve(field_id = 1)]
    id: u64,
}

fn main() {}

