#[test]
fn macro_compile_contracts() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/pass_basic.rs");
    tests.pass("tests/ui/pass_policies.rs");
    tests.pass("tests/ui/pass_format_dsl.rs");
    tests.pass("tests/ui/pass_matrix_aux.rs");
    tests.pass("tests/ui/pass_layout.rs");
    tests.pass("tests/ui/pass_macro_hygiene.rs");
    tests.pass("tests/ui/pass_read_limits.rs");
    tests.pass("tests/ui/pass_optional_partial_limits.rs");
    tests.pass("tests/ui/pass_replacement_api.rs");
    tests.pass("tests/ui/pass_packed_bitmap_variable_field.rs");
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
    // API-04: a matrix slot needs a stride the compiler knows, and
    // `PackedBitmap` encodes a variable byte string, so it is not a fixed-width
    // matrix field — and neither is an unrelated user type that merely shares
    // its name.
    tests.compile_fail("tests/ui/fail_matrix_packed_bitmap_field.rs");
    tests.compile_fail("tests/ui/fail_matrix_shadowed_packed_bitmap.rs");
    tests.compile_fail("tests/ui/fail_duplicate_format_key.rs");
    tests.compile_fail("tests/ui/fail_generic_derive.rs");
    tests.compile_fail("tests/ui/fail_unknown_limit_key.rs");
    tests.compile_fail("tests/ui/fail_duplicate_limit_key.rs");
    tests.compile_fail("tests/ui/fail_manual_block_fingerprint_mismatch.rs");
    tests.compile_fail("tests/ui/fail_custom_codec_missing_schema_id.rs");
    tests.compile_fail("tests/ui/fail_container_codec_missing_schema_id.rs");
    tests.compile_fail("tests/ui/fail_keyed_contradiction.rs");
    tests.compile_fail("tests/ui/fail_keyed_contradiction_merge.rs");
    #[cfg(not(feature = "high-cardinality-dev"))]
    tests.compile_fail("tests/ui/fail_key_index_requires_feature.rs");
}

#[cfg(feature = "high-cardinality-dev")]
#[test]
fn high_cardinality_compile_contracts() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/pass_high_cardinality_macro.rs");
    tests.pass("tests/ui/pass_petabyte_plan_batch.rs");
    tests.compile_fail("tests/ui/fail_key_index_matrix.rs");
    tests.compile_fail("tests/ui/fail_key_index_value.rs");
    tests.compile_fail("tests/ui/fail_key_index_without_key.rs");
    tests.compile_fail("tests/ui/fail_petabyte_chain_stream_keyed_batch.rs");
    tests.compile_fail("tests/ui/fail_petabyte_chain_unindexed_keyed_mutation.rs");
    tests.compile_fail("tests/ui/fail_manual_block_missing_keyedness.rs");
    tests.compile_fail("tests/ui/fail_keyed_contradiction_stream_delete.rs");
    tests.compile_fail("tests/ui/fail_keyed_contradiction_indexed_delete.rs");
}

#[cfg(feature = "mmap")]
#[test]
fn mmap_safety_contracts() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/pass_mmap_unsafe.rs");
    tests.compile_fail("tests/ui/fail_mmap_requires_unsafe.rs");
}
