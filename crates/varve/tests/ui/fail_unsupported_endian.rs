use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(id = 411, kind = "fixed", endian = "middle")]
struct UnsupportedEndian {
    value: u32,
}

fn main() {}
