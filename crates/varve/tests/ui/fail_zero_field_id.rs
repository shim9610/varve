use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(id = 402, kind = "variable")]
struct BadZero {
    #[varve(field_id = 0)]
    value: u32,
}

fn main() {}

