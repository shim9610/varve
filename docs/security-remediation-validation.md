# Security Remediation Validation

Date: 2026-07-10, supplemented 2026-07-11. Target: Varve 0.2.0 release
candidate.

## Scope

This validation covers fail-closed resource limits, open-object snapshots,
strict recovery classification, copy-on-write fixed replacement, canonical
decoding, native and matrix structural validation, sidecar and lock bounds, and
generated API trust boundaries.

The implementation preserves valid native wire bytes and schema hashes.
`ReadLimits` is runtime policy and is intentionally excluded from schema
fingerprints.

## Mechanical Verification

The following commands passed on the final working tree:

```powershell
cargo test --workspace
cargo test --workspace --all-features
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings
$env:RUSTDOCFLAGS='-D warnings'; cargo doc --workspace --all-features --no-deps
cargo fmt --all -- --check
git diff --check
cargo audit
cargo deny check
.\scripts\run-security-fuzz.ps1 -Target all -Seconds 15 -Jobs 1
```

The test runs include compile-pass/compile-fail macro contracts, native and
custom-layout snapshots, record and field canonicality, strict recovery,
copy-on-write and unsafe-exclusive replacement behavior, cumulative
materialization accounting, matrix extent and sidecar limits, mmap windows,
zero-copy contracts, compression, transaction markers, merge/compact, and
self-check diagnostics.

## Independent Verification

An independent final static review reported no blocker, high, or separate
medium finding. It accepted the ordinary/trusted API
separation, pre-allocation native and matrix limits, opened-object snapshot
reads, fatal resource-error recovery classification, copy-on-write fixed
replacement, duplicate sequence rejection, and canonical field/map/internal
envelope validation.

The independent review did not rerun tests or performance commands; release
integration executed those separately and they are recorded in this document.
Its Windows `ReplaceFileW` and same-object mutation questions were followed by
real Windows-handle fault tests and an ASan-instrumented rerun. The pathname
remained one complete generation during concurrent publication, retained
readers stayed on their opened object, and external truncation returned an
error without a panic or pathname rebind.

`cargo audit` scanned 75 locked dependencies with no reported advisory. Cargo
deny passed the advisory, ban, license, and source checks.

## Source Compatibility

The final semver comparison against the prior public revision recognizes
0.1.0 to 0.2.0 as the required pre-1.0 breaking minor update and reports that no
further version change is needed. The intentional source changes include adding
`FormatSpec.read_limits`, removing `ReplaceStrategy::FixedInPlace`, adding
`ReplaceStrategy::FixedCopyOnWrite`, and requiring ordered keys for canonical
map decoding. Because the facade re-exports core types, facade callers using
them are source-affected. The changes are documented in `migration-guide.md`
rather than retaining the old misleading safe in-place policy.

## External Compatibility

The Python compatibility environment used npTDMS 1.10.0 and Pillow 12.2.0.
All three harnesses passed after the final layout-read optimization:

```powershell
.\.venv-tdms\Scripts\python.exe scripts\verify_tdms_with_nptdms.py
.\.venv-tdms\Scripts\python.exe scripts\verify_nptdms_multichannel_with_varve.py
.\.venv-tdms\Scripts\python.exe scripts\verify_bmp_with_pillow.py
```

Coverage includes Varve-created TDMS read and append verified by npTDMS,
npTDMS-created multi-channel scalar data read and appended by the Varve-based
adapter, value checks after append, sidecar inspection, byte-backed reads, and
BMP read/write in both Varve-to-Pillow and Pillow-to-Varve directions.

## Performance Gate

The release comparison uses 10,000 records, one warmup, and five recorded
runs. The final same-machine comparison measured fixed codec at -0.5%, variable
codec at +5.4%, append at -9.3%, open/scan at -45.2%, and merge/compact between
-77.5% and -81.2%. The only observed slowdown remains below the 15% integration
gate. Complete historical and same-machine measurements are retained in
`performance.md`.

The all-feature ignored smoke suite also passed for compression, mmap,
zero-copy, custom layout, matrix, and integrity paths. The custom-layout scan
measured 71.506 ms after replacing repeated small reads with a snapshot-bound
buffered cursor, versus the 117.587 ms pre-remediation reference.

## Fuzzing And Platform Fault Injection

Four `cargo-fuzz` targets exercise arbitrary native files, built-in codecs,
custom layouts, and matrix-plus-sidecar inputs under MSVC AddressSanitizer. A
30-second-per-target campaign and a final post-fix campaign completed with no
crash, timeout, ASan finding, or artifact. The final campaign executed 1,043
native, 676,910 codec, 1,750 layout, and 1,687 matrix inputs in 16 seconds per
target. Three allocation/codec regressions also passed strict Miri checks.

The Windows fault suite uses actual `ReplaceFileW` calls. It covers sharing
violations and retry, 64 synchronized publication races, old-handle snapshot
retention, external truncate, retained replacement handles, and injected
post-publication rebind failure. The complete 21-test `varve-core` library suite
then passed under ASan. Commands and the exact assurance boundary are recorded
in `fuzzing-and-fault-injection.md`.

These campaigns found and fixed a macro hygiene defect and clarified the
post-publication contract with `PublishedButRebindFailed`. The latter poisons
the current writer and requires reopen/reconciliation instead of blind retry.

## Residual Assurance Boundary

- Format authors must choose finite ceilings that fit their domain. Values that
  are technically finite but excessively large can still permit denial of
  service.
- Custom `VarveEncode` and `VarveDecode` implementations are trusted code and
  must enforce equivalent canonical and allocation rules.
- Unsafe mmap/raw-layout/zero-copy APIs and
  `replace_fixed_in_place_exclusive` retain their documented caller proofs.
- An append-log snapshot pins object identity and logical EOF, not a private
  byte copy. Same-object writes by an uncoordinated external process require
  filesystem coordination and an integrity policy for covered-byte detection.
- Matrix slot bytes remain intentionally in-place storage and require external
  reader/writer coordination.
- Fuzzing and sanitizer campaigns are finite and cannot prove absence of
  defects. Custom codecs, power-loss durability, hard-link/reparse-point policy,
  and 32-bit targets require their own validation.
