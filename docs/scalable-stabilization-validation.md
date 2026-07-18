# Scalable I/O Stabilization Validation

Validation date: 2026-07-18. Host: Windows x86_64 MSVC, NTFS.

This report records executed evidence for
`scalable-stabilization-final-spec.md`. Compilation alone is not counted as a
completed gate.

## Gate Status

| Gate | Status | Executed evidence |
| --- | --- | --- |
| scan progress and cancellation | pass | 7 driver tests plus generated stream/indexed integration tests |
| process-interruption persistence matrix | pass | every traced before/after occurrence; final post-fix run, 510.96 seconds |
| redb sidecar robustness campaigns | pass | 3 ASan targets, 120 seconds each, 7,195 executions, 0 artifacts |
| isolated test artifacts | pass | fresh atomic sessions, failure retention, success removal and absence verification |
| real 1 PiB sparse-offset I/O | open on this host | NTFS rejected the positional write with Windows error 87 |

The feature remains `high-cardinality-dev` because the real 1 PiB gate has not
passed on a supporting filesystem. The 1 TiB smoke is useful execution
evidence, but it is not substituted for the 1 PiB requirement.

## Scan Control

Executed:

```powershell
cargo test -p varve-core --features high-cardinality-dev scan_control
cargo test -p varve --test high_cardinality --features high-cardinality-dev
```

Verified properties:

- `Started`, monotonic coalesced `Running`, and final `Complete` callbacks;
- exact record, byte, absolute offset, and snapshot-length reporting;
- pre-scan, record-boundary, callback, and final-callback cancellation;
- `Error::ScanCancelled` carries the exact latest state;
- cancelled bootstrap publishes no `.vks`;
- cancelled rebuild leaves the prior `.vki` byte-for-byte unchanged;
- allocation instrumentation observed zero allocations in progress bookkeeping.

## Persistence Boundaries

Executed:

```powershell
cargo test -p varve --test scalable_crash_faults `
  --features high-cardinality-dev,scalable-fault-injection `
  enabled::scalable_crash_fault_matrix -- --exact
```

The parent first discovered the complete hook sequence, then ran each
occurrence in an isolated child process. Fixtures forced multiple native chunks
and sidecar transactions. The matrix covered create, append, clean publication,
restore convergence, stream bootstrap, disk-index rebuild/replacement, and
parent-directory synchronization.

An interrupted writer leaves its lock marker. The new
`clear_stale_writer_lock` operation checks an explicit policy and resolves that
marker without opening or scanning native data. Tests prove a live process is
refused, an exited process can be cleared, and 16 simultaneous recovery calls
allow exactly one writer. The OS-level guard is outside the metadata byte range,
so live metadata remains inspectable on Windows. The default remains refusal.

The final workspace rerun exposed one Windows-specific stale-lock false
refusal after an aborted child: `OpenProcess` could still open the terminated,
signaled process object while another handle retained it. Recovery now requests
`SYNCHRONIZE` and uses a zero-time wait to distinguish that state from a live
process. A deterministic test retains the waited child's handle while clearing
the marker. Live PIDs, PID reuse, access failures, and unknown wait results
remain conservative refusals. The complete crash matrix then passed in 510.96
seconds.

## Sparse Offset Probe

Executed 1 PiB command:

```powershell
cargo test -p varve-core --features high-cardinality-dev,integrity `
  pib_probe::real_file_positional_io_at_one_pib -- --ignored --exact --nocapture
```

Result: the file was marked sparse, but NTFS rejected the write at `1 << 50`
with Windows error 87. Optional mode reported a precise skip. Required mode is
therefore not satisfied on this host.

The same production path was executed at 1 TiB:

| Field | Result |
| --- | ---: |
| filesystem | NTFS |
| record offset | 1,099,511,627,776 |
| logical EOF | 1,099,511,627,848 |
| allocated bytes | 65,536 |

It passed checked pointer conversion, one-byte-past-EOF rejection before I/O or
allocation, one positional read, native framing, sequence, typed decode,
footer, and CRC verification.

## Sidecar Campaigns

All runs used MSVC AddressSanitizer, a 10-second per-input timeout, 1 MiB input
ceiling, and 1 GiB RSS ceiling.

| Target | Time | Executions | Final RSS | Artifacts |
| --- | ---: | ---: | ---: | ---: |
| `sidecar_arbitrary` | 120 s | 1,723 | 281 MiB | 0 |
| `sidecar_mutation` | 120 s | 2,717 | 191 MiB | 0 |
| `sidecar_state_machine` | 120 s | 2,755 | 190 MiB | 0 |

The per-input throughput is intentionally lower than codec-only targets because
each input creates isolated files and exercises redb open/recovery/read paths.

## Performance State

The accepted 2026-07-17 release baseline remains:

| Operation | Baseline |
| --- | ---: |
| append 1,000,000 unique keys | 4.072 s |
| sync and clean publication | 116.2 ms |
| clean reopen | 9.35 ms |
| 10,000 lookups | 141.5 ms |
| peak allocator delta | 12,346,781 bytes |

Four later 2026-07-18 runs without the unrelated `find.exe` processes observed
append times of 6.844-7.011 s and 10,000 lookup times of 199.5-215.3 ms. Native
write calls remained 62, native and sidecar lengths were unchanged, and the
allocator peak remained 12,346,781 bytes. The active Windows plan was then
identified as `Power saver` (`a1841308-3541-4fab-bc81-f71556f20b4a`), while the
frozen comparison protocol requires the same power plan. These numbers are
therefore retained as non-acceptance observations rather than called a code
regression. An independent clean-context review ranked power throttling as the
most plausible explanation and found no evidence of added scans, writes, or
transaction boundaries.

The indexed append path now canonicalizes each sidecar key once rather than
once for lookup and again for insertion. A direct count regression test covers
batch put, single put, and tombstone paths. Timing impact still requires an
A/B/A run under controlled power settings.

## Mechanical Verification

Passed:

- `cargo check --workspace --all-features`;
- `cargo fmt --all -- --check`;
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`;
- `cargo test --workspace` through `varve-test-runner` (46.6 seconds);
- `cargo test --workspace --all-features` through `varve-test-runner` (663.4
  seconds for the complete pre-classifier run); the post-change workspace run
  passed every preceding suite, exposed the Windows stale-process classifier at
  the crash matrix, and the corrected complete matrix passed in 510.96 seconds;
- generated high-cardinality trybuild pass/fail contracts;
- focused scalable integration tests;
- `varve-core --all-features --lib` (91 passed, 2 ignored);
- runner self-tests for fresh sessions, namespace refusal, collision avoidance,
  failure retention, and verified success cleanup;
- root and independent fuzz-workspace `cargo audit`;
- root and independent fuzz-workspace `cargo deny` advisories, bans, licenses,
  and source policies.

One workspace run observed a single transient Windows concurrent replacement
`NotFound`. The same test subsequently passed five isolated executions (320
internal rounds) and the full 90-test `varve-core` parallel suite. The final
workspace rerun also passed; the transient result is retained here rather than
hidden.

Remaining before this report can be marked complete:

1. execute the real required 1 PiB probe on a filesystem that accepts it;
2. rerun the million-key release probe under the same controlled power plan.
