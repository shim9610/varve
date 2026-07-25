# Varve Self-Check Guide

This guide is for application developers who need to decide whether a failure is
caused by a Varve library bug, a bad format declaration, a wrong API call, a
feature flag mismatch, or damaged file bytes.

## Static Format Diagnostics

Run diagnostics before creating long-lived files:

```rust
let report = AppFormat::diagnostics();
assert!(report.passed(), "{report:#?}");
println!("computed schema hash: {:#018x}", report.computed_schema_hash);
```

`FormatSpec::diagnostics()` checks the static registry, schema hash state,
matrix declarations, and required feature gates. Each diagnostic has:

- `severity`: `Info`, `Warning`, or `Error`
- `domain`: where to look first
- `code`: stable machine-readable category
- `message` and optional `hint`

## File Diagnostics

Use file diagnostics when a real file fails to open or read:

```rust
let report = AppFormat::diagnose_file("data.varve");
for item in &report.items {
    eprintln!("{:?} {:?} {}: {}", item.severity, item.domain, item.code, item.message);
}
```

This opens the file read-only, validates record payloads logically, checks
compression and CRC feature gates, compares embedded manifests when present, and
reports matrix recovery findings.

## End-To-End Self Test

Self-tests create a disposable Varve file, write caller-provided sample values,
reopen it read-only, and verify that the generated/runtime APIs roundtrip those
values:

```rust
let report = AppFormat::self_test("self-check.varve")
    .with_block(Point { x: 1, y: 2 })
    .with_keyed_block(User { id: 7, name: "Ada".into(), flags: 0 })
    .cleanup(true)
    .run();

assert!(report.passed(), "{report:#?}");
```

For matrix formats:

```rust
let report = MatrixFormat::self_test("matrix-check.varve")
    .with_dims(MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]))
    .with_matrix_cell(MatrixKey::new(0, 0), Cell { value: 9 })
    .with_uncommitted_matrix_cell(MatrixKey::new(1, 1), Cell { value: 11 })
    .with_matrix_aux("scratch", 0, [1, 2, 3, 4])
    .cleanup(true)
    .run();
```

Use a temporary path. The self-test is non-destructive: it creates the target
with exclusive create (`create_new` for append formats,
`create_new_with_dims` for matrix formats) and never truncates or deletes a
pre-existing file. The matrix path holds the one exclusively created,
lock-bound handle from the claim through matrix initialization, so there is no
window in which the claimed pathname is re-opened. A pre-existing path is
reported as a failed step in the `CallerUsage` domain (message: "target path
already exists; the self-test never truncates or deletes pre-existing files",
backed by an `AlreadyExists` I/O error) — for append formats at the
`create file` step, for matrix formats at the `create matrix file` step —
before any modification. `cleanup(true)` only ever removes files the run
itself created (the created native file and its `.lock`), verified by
identity: the native file is deleted only while the pathname still resolves to
the file object this run created, and the marker only after re-acquiring it
through the standard writer-lock protocol. A file or marker swapped in by
another process survives cleanup.

Cleanup that cannot complete now says so (F-08). Removing the run's own
`.lock` marker requires re-acquiring it through the writer-lock protocol and
capturing its object identity; either can fail, and both used to return
silently, so a run could report `passed = true` with zero cleanup failures
while a live marker survived and the *next* run was refused by it. Each failure
is now pushed as a failed `cleanup` step in the `Environment` domain, naming
the marker path and advising that it be removed by hand. The marker itself is
still deliberately preserved in both cases: being unable to prove the marker is
ours is exactly the reason not to delete it. `ObjectRemoval::NotOwned` remains
a clean outcome for the same reason — the name resolves to something this run
did not create — so a marker replaced by another process still ends the run
without a cleanup failure.

`WriterLock::drop` still discards a marker-clear error, because `Drop` cannot
report. The consequence is no longer invisible — the next run's re-acquisition
fails loudly — but a caller that never runs the self-test still gets no signal
from that path.

## Resource-Limit Failures

Resource errors are policy results, not automatically library defects:

| Error | Meaning | First action |
| --- | --- | --- |
| `MissingResourceLimit` | A low-level or explicitly unresolved policy reached an operation that requires a resolved value. Ordinary generated open/create APIs resolve standard runtime defaults automatically. | Report this if it occurs through an ordinary generated API; otherwise resolve the `FormatSpec` or use a generated runtime-policy method. |
| `TrustedUnboundedRequiresExplicitApi` | An ordinary API received a trusted-unbounded field. | Use finite limits for untrusted data; use the visibly named trusted API only for controlled input. |
| `LimitExceeded` | A decoded claim or operation exceeded a selected runtime ceiling before the large read/allocation. | Verify the file claim, then raise the call-time resource policy only if the dataset is legitimate. |
| `ResourceArithmeticOverflow` | A claimed range/product cannot be represented safely. | Treat the file as invalid; do not retry with a larger limit. |
| `AllocationFailed` | A checked reservation failed even though the numeric ceiling allowed it. | Reduce the operation/dataset or available-memory pressure. |

When reporting a suspected Varve bug, include the selected `ReadLimits`, the
failing API, and whether the input was opened through an ordinary or explicitly
trusted method. This distinguishes a format-policy mistake from an allocation
or validation ordering defect.

## External Compatibility Harnesses

When custom physical layout behavior is in question, run the optional Python
harnesses to compare Varve output with established readers and writers:

```powershell
python -m venv .venv-tdms
.\.venv-tdms\Scripts\python.exe -m pip install -r scripts\requirements-tdms-harness.txt
.\.venv-tdms\Scripts\python.exe scripts\verify_tdms_with_nptdms.py
.\.venv-tdms\Scripts\python.exe scripts\verify_nptdms_multichannel_with_varve.py
.\.venv-tdms\Scripts\python.exe scripts\verify_bmp_with_pillow.py
```

The TDMS harnesses check Varve-authored example files against `npTDMS` and
`npTDMS`-authored scalar type matrix files against a Varve-based example
adapter. The covered TDMS proof set includes signed/unsigned integer widths,
single/double floats, booleans, strings, timestamps, complex single/double
floats, changed raw-data-index segments, `same-as-previous` raw-index reuse,
mixed objects in one segment, and Varve append into an npTDMS-authored file.
The Varve-authored direction also covers npTDMS-readable
`SingleFloatWithUnit`/`DoubleFloatWithUnit` channel type ids; the reverse
npTDMS-authored direction omits those two because npTDMS 1.10.0 does not author
them correctly through its normal `ChannelObject` writer path. The BMP harness
checks a non-TDMS layout in both directions with Pillow.

For TDMS-style work, remember that the repository contains adapter proofs, not
a Varve-provided TDMS reader/writer. Varve owns the generic physical-layout
capabilities — declaring headers, lead-ins, metadata and raw regions and footers,
validating literal and caller fields, finalized offsets and segment bounds,
appending complete segments, and exposing offsets, lengths, byte ranges and
tolerant scan reports. An external TDMS adapter owns TDMS semantics: object
paths, raw-data-index grammar, property typing, timestamp conversion, waveform
time tracks, scaling, DAQmx raw scalers, interleaving, and any export. Use that
split to decide whether a failure belongs to Varve's generic API or to
caller-owned TDMS semantics. A quick rule:

- If strict layout open succeeds, `inspect_layout_file_report` is complete, and
  the adapter can read the required metadata/raw byte ranges, failures in TDMS
  object paths, property typing, scaling, timestamps, channel slicing, or export
  are caller/adapter issues.
- If a valid external physical layout cannot be declared, cannot stream or read
  required byte ranges, or cannot report a truncated tail before TDMS semantics
  run, treat it as a Varve API limitation.

## Domain Meaning

- `FormatDefinition`: the declared schema or policy is suspicious.
- `CallerUsage`: API call, dimensions, key, block type, commit state, or sample
  setup is wrong.
- `FeatureGate`: enable the required Cargo feature, such as `integrity` or
  `compression-zstd`.
- `FileData`: the bytes on disk do not match the supplied static format or
  failed integrity/decode checks.
- `Environment`: filesystem permissions, writer lock, or concurrent process
  issue.
- `LibraryInvariant`: a self-test roundtrip mismatch after successful write and
  read. First check custom codecs; if those are simple/correct, minimize and
  report as a likely Varve bug.

## Practical Rule

If `diagnostics()` passes and a self-test using your actual generated format and
sample values passes, but your application path still fails, start by checking
caller-owned policy: dimensions, key construction, commit timing, sidecar
generation, migration functions, and custom codec semantics.
