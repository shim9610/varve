/// Whether the caller has asked for the `.stderr`-comparing cases to be skipped.
///
/// A `compile_fail` case compares rustc's *diagnostic prose* against a checked-in
/// snapshot, so a snapshot can only ever match one compiler. rustc renamed one
/// `E0599` phrase — "no function or associated item named" became "no associated
/// function or constant named" — and that alone turned two enforcement fixtures
/// red on CI's moving `stable` while they passed on the pinned MSRV that the
/// snapshots were taken against and that the local gate uses. Blessing the newer
/// wording would simply move the failure onto every developer machine.
///
/// So the snapshots are owned by ONE toolchain: the declared MSRV. The pinned
/// `msrv` CI job runs the whole suite and is where these cases are gated; the
/// moving-`stable` job sets `VARVE_SKIP_UI_SNAPSHOTS=1` and skips them, because
/// what it exists to catch is a break on a newer compiler, not a reworded
/// diagnostic. Unset by default, so a plain `cargo test` still runs them.
///
/// `pass` cases are unaffected — they assert compilation succeeds and compare no
/// text — but they live in the same `TestCases` batch as the `compile_fail` ones,
/// so a skipped test function skips both. The `msrv` job covers them.
fn ui_snapshots_skipped() -> bool {
    match std::env::var("VARVE_SKIP_UI_SNAPSHOTS") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// Prints why a UI test function returned without asserting anything, so a green
/// run that skipped them cannot be mistaken for a green run that checked them.
fn note_ui_snapshots_skipped(which: &str) {
    println!(
        "{which}: skipped because VARVE_SKIP_UI_SNAPSHOTS is set. These cases \
         compare rustc diagnostic text against snapshots taken on the declared \
         MSRV; the pinned `msrv` CI job is where they are gated."
    );
}

#[test]
fn macro_compile_contracts() {
    if ui_snapshots_skipped() {
        note_ui_snapshots_skipped("macro_compile_contracts");
        return;
    }
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
    // F-10: matrix field eligibility is decided by the generated `SLOT_STRIDE`
    // (which resolves `VarveEncode::WIRE_TYPE`), not by the source spelling, so
    // an alias of a supported scalar compiles.
    tests.pass("tests/ui/pass_matrix_type_alias.rs");
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
    // F-10: the permissive shape check still refuses source forms that can
    // never denote a fixed-stride type.
    tests.compile_fail("tests/ui/fail_matrix_tuple_field.rs");
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
    if ui_snapshots_skipped() {
        note_ui_snapshots_skipped("high_cardinality_compile_contracts");
        return;
    }
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

/// Round 12's enforcement pass: the proof that the two recurring defect shapes
/// are refused by the compiler rather than by review.
///
/// Every previous round closed one of these classes by *reading* — round 10
/// rewrote 736 lines of `matrix.rs` under the banner of an exhaustive
/// invariant-3 sweep and still shipped `compact_page_index`, which had the
/// exact shape the sweep was hunting, in the file the sweep had rewritten.
/// These fixtures are the part that does not decay: they keep holding for code
/// nobody has written yet, and they fail loudly the day someone relaxes a
/// private field to make an edit easier.
///
/// Gated on `scalable-fault-injection` because the enforcement types are
/// crate-private and are exposed to an external crate only through the
/// `#[doc(hidden)]` `varve_core::enforcement_probe`, which that test-only
/// feature compiles.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn mechanical_enforcement_contracts() {
    if ui_snapshots_skipped() {
        note_ui_snapshots_skipped("mechanical_enforcement_contracts");
        return;
    }
    let tests = trybuild::TestCases::new();
    // SHAPE B: the poison-check witness cannot be forged.
    tests.compile_fail("tests/ui/fail_fabricated_mutation_permit.rs");
    // SHAPE B: a mutation window cannot be opened without the witness.
    tests.compile_fail("tests/ui/fail_unpermitted_mutation_window.rs");
    // SHAPE B (round 14): the witness names the writer it speaks for, so a
    // permit taken from one writer cannot be spent on another. The in-crate
    // half of the same fix — that a throwaway `PoisonFlag` cannot mint one at
    // all — is gated by tests/enforcement_gates.rs.
    tests.compile_fail("tests/ui/fail_cross_writer_mutation_permit.rs");
    // SHAPE A: the post-append mirror install cannot be reached without its
    // reservation.
    tests.compile_fail("tests/ui/fail_fabricated_index_reservation.rs");
    // F-01 (round 13): a replacement target cannot be addressed without the
    // stored-version refusal that its only constructor performs.
    tests.compile_fail("tests/ui/fail_fabricated_replacement_target.rs");
    // F-02 (round 13): the permission to rewrite an already-indexed record
    // cannot be obtained without dropping the block's keyed-tail map.
    tests.compile_fail("tests/ui/fail_fabricated_record_overwrite.rs");
    // F-04 (round 14): the CRC-validity bitmap cannot be read as evidence of
    // absence without the completeness witness, and the witness cannot be
    // forged.
    tests.compile_fail("tests/ui/fail_fabricated_crc_valid_completeness.rs");
    // SHAPE B (round 14): matrix state cannot be addressed without the
    // fail-closed fatal-recovery witness, and the witness cannot be forged.
    tests.compile_fail("tests/ui/fail_fabricated_fatal_access.rs");
    // ROUND 15, the minting half of both of the above. Forging a witness was
    // never how these defects were written; minting an unchecked *gate* and
    // taking a legitimate witness off it was. The constructors that made that a
    // one-liner are deleted, and these two fixtures fail the day either
    // returns. Note what they cannot cover: an outside crate could not reach
    // either constructor anyway, so the fixtures are trip-wires rather than
    // proofs. The proof is in-crate — `matrix.rs::bypass_catalogue`,
    // `file.rs::bypass_catalogue` and `tests/enforcement_gates.rs`.
    tests.compile_fail("tests/ui/fail_minted_fatal_access_gate.rs");
    tests.compile_fail("tests/ui/fail_minted_crc_valid_completeness.rs");
}

#[cfg(feature = "mmap")]
#[test]
fn mmap_safety_contracts() {
    if ui_snapshots_skipped() {
        note_ui_snapshots_skipped("mmap_safety_contracts");
        return;
    }
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/pass_mmap_unsafe.rs");
    tests.compile_fail("tests/ui/fail_mmap_requires_unsafe.rs");
}
