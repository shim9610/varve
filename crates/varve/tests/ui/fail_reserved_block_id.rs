use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(id = 4294967040, kind = "fixed")]
struct Reserved {
    value: u32,
}

fn main() {}

