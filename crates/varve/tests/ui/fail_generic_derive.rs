use varve::VarveBlock;

#[derive(VarveBlock)]
#[varve(id = 1, kind = "fixed")]
struct GenericBlock<T> {
    value: T,
}

fn main() {}
