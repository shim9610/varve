#![no_main]

use libfuzzer_sys::fuzz_target;
use varve_fuzz::header_tails::run_header_tails;

fuzz_target!(|data: &[u8]| run_header_tails(data));
