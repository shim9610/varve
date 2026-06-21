use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(id = 404, kind = "variable", key = "missing")]
struct MissingKey {
    #[varve(field_id = 1)]
    id: u64,
}

fn main() {}

