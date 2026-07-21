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
| `sidecar_arbitrary` | capped arbitrary `.vks` and `.vki` bytes paired with valid native files; public open, recovery, verification, and indexed lookup |
| `sidecar_mutation` | capped mutations of native `.vks` and `.vki` companions across clean/dirty and empty/populated fixtures |
| `sidecar_state_machine` | up to 64 modeled operations and 256 records across stream/indexed append, sync, recovery, tombstone, batch, and lookup paths |

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
.\scripts\run-security-fuzz.ps1 -Target all -Seconds 120 -Jobs 1
```

Use one target while developing:

```powershell
.\scripts\run-security-fuzz.ps1 -Target native_arbitrary -Seconds 300
.\scripts\run-security-fuzz.ps1 -Target sidecar_state_machine -Seconds 120
```

The runner clamps each `sidecar_*` campaign to at least 120 seconds.

### The Artifact Gate

A nonzero exit or a file under `fuzz/artifacts` is a failed gate, and the
runner enforces both halves of that sentence rather than the exit code alone
(F-09). The gate means **any** artifact, not only one this campaign produced:

- before Cargo is invoked at all, the runner enumerates `fuzz/artifacts`
  recursively. If any file is present it prints each path and exits `2` without
  running. An unpromoted reproducer is unfinished work, and running a campaign
  on top of one is how a stale artifact comes to look like a fresh pass;
- after each target, and once more after the last one, it enumerates the
  directory again. Any file present prints each path and exits `3`. This check
  runs *before* the campaign's own exit code is acted on, so a crashing target
  reports the reproducer it left as well as its status;
- a nonzero exit from corpus generation or from a campaign exits with that
  same code.

The runner never deletes anything under `fuzz/artifacts`. A reproducer is the
only copy of an input that reached a defect, so clearing the directory is an
explicit operator action taken **after** the input has been promoted to a
deterministic regression test. That promotion, not deletion, is how a campaign
returns to a clean gate.

Audit the independent fuzz dependency graph as well as the publishable
workspace graph:

```powershell
cargo audit --file fuzz\Cargo.lock
cargo deny --manifest-path fuzz\Cargo.toml check --config fuzz\deny.toml
```

The fuzz policy explicitly permits NCSA because `libfuzzer-sys` is licensed
under `(MIT OR Apache-2.0) AND NCSA`; this permissive test-tool allowance is kept
out of the public workspace's license policy.

CI enforces this graph in the `fuzz workspace (locked)` job
(`.github/workflows/ci.yml`), and the clean-source-archive job checks the fuzz
workspace as well. Both use locked resolution, so a `fuzz/Cargo.lock` that no
longer describes the fuzz dependency graph fails the build (REL-01). Locked
resolution runs before any caching or policy tool, because `cargo deny` and
`Swatinem/rust-cache` both perform an unlocked `cargo metadata` that would
regenerate a stale lockfile and hide the defect; a final
`git diff --exit-code -- fuzz/Cargo.lock` proves nothing in the job rewrote it.
Run the same gate locally with:

```powershell
cargo metadata --locked --manifest-path fuzz\Cargo.toml --format-version 1
cargo check --locked --all-targets --manifest-path fuzz\Cargo.toml
```

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
| `sidecar_arbitrary`, 120 seconds, 2026-07-18 | 1,723 | pass; 0 artifacts |
| `sidecar_mutation`, 120 seconds, 2026-07-18 | 2,717 | pass; 0 artifacts |
| `sidecar_state_machine`, 120 seconds, 2026-07-18 | 2,755 | pass; 0 artifacts |

The 2026-07-18 scalable persistence matrix also reran every discovered
before/after hook occurrence in an isolated subprocess. It covered create,
multi-chunk append, clean publication, restore, stream bootstrap, disk-index
rebuild, atomic replacement, and parent-directory synchronization in 67.37
seconds with all boundary oracles passing. Environment variables are inert
unless the child explicitly arms the test-only registry.

Each of those children ends in `std::process::abort`, so on Windows the OS
would otherwise start `WerFault.exe` once per aborted child and can raise an
interactive error dialog. The child entry point in
`crates/varve/tests/scalable_crash_faults.rs` therefore calls
`SetErrorMode`/`SetThreadErrorMode` with `SEM_NOGPFAULTERRORBOX` and
`WerSetFlags` with `WER_FAULT_REPORTING_NO_UI`,
`WER_FAULT_REPORTING_FLAG_NOHEAP`, and
`WER_FAULT_REPORTING_FLAG_DISABLE_SNAPSHOT_CRASH` before it induces the fault
(TEST-01). Suppression has to happen inside the child, because the abort is
raised by the injected fault point itself. `WerSetFlags` returns `S_OK` on this
host; the measured saving is modest where WER UI is already disabled
system-wide (an isolated abort probe fell from about 247 ms to about 208 ms per
child, and the whole test moved from 67.8 s before to 66.1-67.4 s after, which
is inside this host's run-to-run noise). The guarantee it buys is that the gate
stays unattended and raises no dialog, not a large wall-time win on a host that
already has WER UI off.

The real sparse-offset probe passed at 1 TiB on NTFS with a 65,536-byte
allocation and exact typed value/framing/CRC verification. The same host
rejected the required 1 PiB positional write with Windows error 87; that gate
is recorded as unsupported on this filesystem, not as a pass.

The campaigns exposed one real compile-time macro hygiene defect: generated
inline blocks referenced an associated constant through an unqualified trait.
The macro now uses a fully qualified `VarveBlock` path, and a pass test verifies
that callers do not need to import that trait.

Fault planning also exposed an ambiguous post-publication state. Replacement
can be atomically visible while reopening the path for the writer fails. The
writer now returns
`PublishedButRebindFailed { sequence, source, parent_sync }` and becomes
poisoned. The caller must discard it and reopen the path; blindly retrying the
same logical update can duplicate the operation. `parent_sync` is `Some(_)`
only when the parent-directory sync for the published pathname failed in the
same publication, which adds the second durability fact: the generation is
visible at the pathname but the rename is not yet proved durable against power
loss (F-07).

## Assurance Boundary

- Campaign duration and corpus quality bound what fuzzing demonstrates.
- User-defined `VarveEncode`/`VarveDecode` implementations are trusted Rust
  code and need their own fuzz targets when they allocate or use unsafe code.
- ASan and Miri do not establish crash durability under power loss.
- External same-object mutation still requires filesystem coordination; CRCs
  detect only bytes covered by the selected integrity policy.
- Hard-link substitution, reparse-point policy, advisory locking, and 32-bit
  target behavior require separate platform validation.
