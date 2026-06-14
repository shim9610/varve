use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(id = 417, kind = "fixed")]
struct FixedDefault {
    #[varve(default)]
    value: u32,
}

fn main() {}
