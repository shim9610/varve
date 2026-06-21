#[test]
fn macro_compile_contracts() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/pass_basic.rs");
    tests.pass("tests/ui/pass_policies.rs");
    tests.pass("tests/ui/pass_format_dsl.rs");
    tests.pass("tests/ui/pass_matrix_aux.rs");
    tests.pass("tests/ui/pass_layout.rs");
    tests.compile_fail("tests/ui/fail_duplicate_id.rs");
    tests.compile_fail("tests/ui/fail_duplicate_field_id.rs");
    tests.compile_fail("tests/ui/fail_zero_field_id.rs");
    tests.compile_fail("tests/ui/fail_missing_block_id.rs");
    tests.compile_fail("tests/ui/fail_reserved_block_id.rs");
    tests.compile_fail("tests/ui/fail_unsupported_kind.rs");
    tests.compile_fail("tests/ui/fail_unsupported_endian.rs");
    tests.compile_fail("tests/ui/fail_tuple_struct.rs");
    tests.compile_fail("tests/ui/fail_unknown_format_key.rs");
    tests.compile_fail("tests/ui/fail_empty_key.rs");
    tests.compile_fail("tests/ui/fail_invalid_key.rs");
    tests.compile_fail("tests/ui/fail_empty_key_segment.rs");
    tests.compile_fail("tests/ui/fail_duplicate_key.rs");
    tests.compile_fail("tests/ui/fail_missing_key.rs");
    tests.compile_fail("tests/ui/fail_fixed_default.rs");
    tests.compile_fail("tests/ui/fail_inline_empty_key.rs");
    tests.compile_fail("tests/ui/fail_matrix_unbounded_field.rs");
}
