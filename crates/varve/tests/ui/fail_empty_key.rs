use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(id = 414, kind = "variable", key = "")]
struct EmptyKey {
    #[varve(field_id = 1)]
    id: u32,
}

fn main() {}
