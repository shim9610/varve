use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(kind = "fixed")]
struct MissingId {
    value: u32,
}

fn main() {}
