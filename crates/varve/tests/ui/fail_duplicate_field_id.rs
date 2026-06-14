use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(id = 401, kind = "variable")]
struct BadFields {
    #[varve(field_id = 1)]
    left: u32,
    #[varve(field_id = 1)]
    right: u32,
}

fn main() {}

