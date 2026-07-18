#![no_main]

use libfuzzer_sys::fuzz_target;
use varve_fuzz::sidecar::run_mutation;

fuzz_target!(|data: &[u8]| run_mutation(data));
