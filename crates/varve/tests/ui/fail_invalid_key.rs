use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(id = 415, kind = "variable", key = "id-number")]
struct InvalidKey {
    #[varve(field_id = 1)]
    id: u32,
}

fn main() {}
