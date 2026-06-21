use varve::VarveBlock;

#[derive(Clone, VarveBlock)]
#[varve(id = 412, kind = "fixed")]
struct TupleStruct(u32);

fn main() {}
