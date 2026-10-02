#[test]
fn finite_key_compile_contracts() {
    if std::env::var("VARVE_SKIP_UI_SNAPSHOTS").is_ok_and(|value| !value.is_empty() && value != "0")
    {
        eprintln!("finite-key UI snapshots skipped by VARVE_SKIP_UI_SNAPSHOTS");
        return;
    }
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/fail_finite_key_limit.rs");
    tests.compile_fail("tests/ui/fail_finite_key_hash.rs");
    tests.compile_fail("tests/ui/fail_finite_key_raw_fields.rs");
}
