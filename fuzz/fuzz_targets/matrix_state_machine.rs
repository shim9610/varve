#![no_main]
use libfuzzer_sys::fuzz_target;
#[path = "../../crates/varve/tests/support/matrix_model.rs"]
mod model;
fuzz_target!(|data: &[u8]| {
    model::run(data).expect("valid matrix operation history must succeed");
});
