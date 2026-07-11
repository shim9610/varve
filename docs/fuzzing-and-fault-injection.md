# Fuzzing And Fault Injection

This document records the reproducible hostile-input and Windows replacement
stress harnesses. These checks complement deterministic integration tests; a
finite fuzz campaign is evidence, not a proof that every input is safe.

## Covered Surfaces

| Target | Input and exercised behavior |
| --- | --- |
| `native_arbitrary` | arbitrary native files; open, scan, typed fixed/keyed variable reads, diagnostics, recovery, mmap snapshot |
| `codec_arbitrary` | arbitrary bytes across scalar, collection, map, tuple, string, and zero-width codec paths |
| `layout_arbitrary` | arbitrary custom-layout lead-ins, metadata/raw regions, reports, and recovery boundaries |
| `matrix_arbitrary` | arbitrary matrix envelopes and sidecars; cell, aux, sidecar, and recovery reads |

`fuzz/examples/generate_corpus.rs` creates deterministic valid seeds before a
campaign. `fuzz/dictionaries/varve.dict` supplies native wire tokens. Generated
corpora, artifacts, coverage, and build products are ignored; source targets,
the dictionary, and the independent `fuzz/Cargo.lock` are retained.

## Windows ASan Setup

Install:

- current Rust nightly with `rust-src`
- `cargo-fuzz`
- Visual Studio 2022 Build Tools with the MSVC C++ toolchain and C++ AddressSanitizer component

```powershell
rustup toolchain install nightly --component rust-src
cargo install cargo-fuzz
```

The runner locates the newest x64 MSVC ASan runtime through `vswhere`, adds it
to `PATH`, enables fail-fast `ASAN_OPTIONS`, generates seeds, and places builds
under the root ignored `target` directory:

```powershell
.\scripts\run-security-fuzz.ps1 -Target all -Seconds 60 -Jobs 1
```

Use one target while developing:

```powershell
.\scripts\run-security-fuzz.ps1 -Target native_arbitrary -Seconds 300
```

A nonzero exit or a file under `fuzz/artifacts` is a failed gate. Preserve and
promote any reproducer to a deterministic regression before fixing it.

Audit the independent fuzz dependency graph as well as the publishable
workspace graph:

```powershell
cargo audit --file fuzz\Cargo.lock
cargo deny --manifest-path fuzz\Cargo.toml check --config fuzz\deny.toml
```

The fuzz policy explicitly permits NCSA because `libfuzzer-sys` is licensed
under `(MIT OR Apache-2.0) AND NCSA`; this permissive test-tool allowance is kept
out of the public workspace's license policy.

## Miri

The owned codec paths are also checked with strict provenance and symbolic
alignment checking:

```powershell
cargo +nightly miri setup
$env:MIRIFLAGS='-Zmiri-strict-provenance -Zmiri-symbolic-alignment-check'
cargo +nightly miri test -p varve --test codec_hardening truncated_and_oversized_payloads_return_errors_without_panicking
cargo +nightly miri test -p varve --test codec_hardening maps_accept_only_strictly_increasing_keys
cargo +nightly miri test -p varve --test codec_hardening variable_field_lengths_are_checked_before_payload_access
```

Miri does not replace ASan or exercise Windows filesystem calls.

## Windows Replacement Faults

The `varve-core` unit suite contains real Windows-handle tests, not a modeled
rename substitute:

- a retained handle without delete sharing makes replacement fail, preserves
  the old generation, leaks no temp file, and permits a later retry;
- two synchronized `ReplaceFileW` calls race for 64 rounds; the pathname is
  always one complete old/A/B generation, never mixed or partial;
- an already-open reader remains bound to its old file object;
- external same-object truncation returns an error without panic or pathname
  rebinding;
- publication followed by injected rebind failure returns
  `PublishedButRebindFailed`, poisons the writer, and leaves the newly published
  generation reopenable.

Run the Windows faults normally:

```powershell
cargo test -p varve-core --all-features --lib windows_ -- --nocapture
```

The final validation also ran the full 21-test `varve-core` library suite under
MSVC ASan using nightly `-Zsanitizer=address` and `-Zbuild-std`.

## Recorded Evidence

Validation date: 2026-07-11, Windows x86_64 MSVC.

| Campaign | Executions | Result |
| --- | ---: | --- |
| codec ASan setup smoke, 11 seconds | 784,098 | no crash, timeout, or ASan finding |
| all four targets, 30 seconds each | completed | no artifact or sanitizer finding |
| final `native_arbitrary`, 16 seconds | 1,043 | pass |
| final `codec_arbitrary`, 16 seconds | 676,910 | pass |
| final `layout_arbitrary`, 16 seconds | 1,750 | pass |
| final `matrix_arbitrary`, 16 seconds | 1,687 | pass |
| strict Miri, three codec regressions | 3 tests | pass |
| ASan `varve-core --all-features --lib` | 21 tests | pass |

The campaigns exposed one real compile-time macro hygiene defect: generated
inline blocks referenced an associated constant through an unqualified trait.
The macro now uses a fully qualified `VarveBlock` path, and a pass test verifies
that callers do not need to import that trait.

Fault planning also exposed an ambiguous post-publication state. Replacement
can be atomically visible while reopening the path for the writer fails. The
writer now returns `PublishedButRebindFailed { sequence, source }` and becomes
poisoned. The caller must discard it and reopen the path; blindly retrying the
same logical update can duplicate the operation.

## Assurance Boundary

- Campaign duration and corpus quality bound what fuzzing demonstrates.
- User-defined `VarveEncode`/`VarveDecode` implementations are trusted Rust
  code and need their own fuzz targets when they allocate or use unsafe code.
- ASan and Miri do not establish crash durability under power loss.
- External same-object mutation still requires filesystem coordination; CRCs
  detect only bytes covered by the selected integrity policy.
- Hard-link substitution, reparse-point policy, advisory locking, and 32-bit
  target behavior require separate platform validation.
