use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(id = 410, kind = "stream")]
struct UnsupportedKind {
    value: u32,
}

fn main() {}
