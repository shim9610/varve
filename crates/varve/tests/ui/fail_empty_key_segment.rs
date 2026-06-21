use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(id = 416, kind = "variable", key = "id,")]
struct EmptyKeySegment {
    #[varve(field_id = 1)]
    id: u32,
}

fn main() {}
