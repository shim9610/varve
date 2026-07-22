# Varve

Varve is a Rust library for building append-friendly, segment-oriented custom
binary formats from typed block definitions. It is not one fixed file format.
The main workflow is format-first: declare a file contract with
`varve_format!`, then use the generated typed reader and writer APIs.

The workspace contains:

- `varve`: public facade crate
- `varve-core`: runtime, codecs, file I/O, diagnostics
- `varve-macros`: `varve_format!` and derive support

## What This Is And Is Not Ready For

Read this before adopting. The detail behind every line is in
**[Known Limitations](docs/known-limitations.md)**; upgrading from 0.3.0 is
covered in **[API Changes](docs/api-changes.md)**.

**Ready for:**

- Declaring a custom binary format and getting typed readers and writers for it.
- Append-log files whose record count and distinct-key count fit comfortably in
  RAM alongside the application. This is the default path.
- Bounded-memory ingest and point lookup over data far larger than RAM — behind
  the `high-cardinality-dev` feature, which has never shipped in a released
  version and whose API may still change.
- Preallocated matrix storage with **fixed dimensions** and a live set whose
  commit-map page count fits the process memory budget.
- Reading a file with hostile-input ceilings applied (`ReadLimits::UNTRUSTED`).

**Not ready for:**

- **Matrices that grow.** Dimensions are fixed at create time and there is no
  grow path, so a matrix cannot represent an indefinitely growing stream. Use the
  stream/indexed APIs for that.
- **Frequent matrix opens.** Opening a matrix is not `O(1)`. It is independent of
  file size, but proportional to the candidate page set: a matrix with **one live
  page** still reads about **131 KB over 32 pages**. Resident commit metadata is
  fixed at open, does not track the working set, and is never evicted under the
  default policy.
- **Matrices under a tight `max_matrix_bitmap_bytes`.** That ceiling is an
  *admission* limit, not a cache bound. A matrix whose committed state exceeds it
  **cannot be opened at all**.
- **Petabyte-scale merge or compact.** Keyed merge and compact are resident-only.
  Varve exports no bounded-memory external merge or compact.
- **Channel-selective reads.** `docs/channel-view-design.md` is a design
  document, not a feature; nothing in it is callable.
- **Live views of a file another handle is writing.** Resident readers and eager
  matrix readers are snapshots as of open.
- **Anything depending on a fuzz, Miri or ASan pass on this release**, or on the
  Unix code paths having been executed. Neither has happened; see
  [Known Limitations §6](docs/known-limitations.md#6-not-verified).

## What Varve Provides

- Compile-time format declarations with generated typed reader and writer APIs.
- Fixed and variable blocks with canonical encoding instead of Rust memory
  layout copying by default.
- Append-friendly record framing, offset-chain metadata, commit policies,
  checksum hooks, keyed collections, and lazy scan/index support.
- Declarative hostile-input ceilings for file, scan, payload, index, matrix,
  sidecar, materialization, and mmap resources.
- Variable payload compression policies that can be declared globally, per
  block, or left unspecified for caller-defined behavior.
- Adapter primitives for physical formats that need custom lead-ins, table of
  contents masks, raw data offsets, next-segment offsets, or externally defined
  record framing.

## Capability Boundary

Varve has two API families with different scale contracts. Pick deliberately.

| Family | Entry points | State | Suited to |
| --- | --- | --- | --- |
| Resident | `VarveFile`, generated typed readers/writers, keyed collections, `merge_keyed_files`, `compact_keyed_file(s)` | index and merge state live in memory, sized by record and key counts | files that fit comfortably in RAM alongside the application |
| Scalable (`high-cardinality-dev`) | `VarveStreamWriter`, `VarveIndexedWriter`, their readers, and `.vks`/`.vki` sidecars | bounded resident state, sized by declared blocks and bounded buffers | high-rate append and point lookup over data far larger than RAM |

The scalable family covers bounded ingest and lookup only. **Keyed merge and
compact are resident-only and explicitly not petabyte-scale**: they retain one
map entry per distinct key ever seen, tombstoned keys included, and nothing
spills to disk. Use `estimate_keyed_merge` to size a run in advance, or the
`*_with_key_limit` variants to fail with a typed limit error at a chosen key
ceiling instead of exhausting memory.

Matrix (preallocated) storage is a third mode. Its integrity metadata is paged
and sparse, so create-time metadata I/O and per-mutation integrity cost are
bounded by the cells actually used rather than by the declared cell count.

**Open-time reads and post-open bitmap residency are bounded by the live
*page* count, not by the working set, and open is not `O(1)`.** Under the
default `MatrixMetadataResidency::EagerVerified` policy, open reads the union of
the pages named by the persisted page index and the pages the platform's
allocation map reports as written; a matrix with one live page measured 131,240
bytes over 32 pages, and one with 64 live pages measured 591,384 bytes and
524,288 resident. Residency does not change as the caller touches cells and is
never evicted. `max_matrix_bitmap_bytes` is an admission limit under this policy:
a matrix whose live set exceeds it cannot be opened. The opt-in
`MatrixMetadataResidency::Lazy { cache_bytes }` policy bounds residency by a
declared ceiling with LRU eviction and reads only the persisted page index at
open, at the cost of moving corruption detection to first touch and giving up a
consistent snapshot across pages.

Matrix dimensions are fixed at create time; there is no grow path.

Full numbers, arithmetic you can apply to your own cell count, and the trade-offs
of each policy are in
[Known Limitations §1](docs/known-limitations.md#1-matrix-opening-a-matrix-is-not-o1-and-its-metadata-residency-is-not-a-cache).

Varve should own the reusable binary-format mechanics. Application-specific
meaning, domain transforms, and compatibility with an external specification
remain caller code.

## Start Here

| Document | Use it for |
| --- | --- |
| [Quickstart](docs/quickstart.md) | shortest path from format declaration to write/read |
| [Known Limitations](docs/known-limitations.md) | what a user actually hits: matrix open cost and residency, resident-API scale, unimplemented features, and what is not verified |
| [API Changes](docs/api-changes.md) | migrating from 0.3.0: new/changed/removed items, behaviour changes at unchanged signatures, and what happens to existing files |
| [Changelog](CHANGELOG.md) | release changes and source-compatibility notes |
| [Declaration And Internals](docs/declaration-and-internals.md) | how the DSL maps to generated Rust API, native bytes, records, fields, indexes, commits, and custom physical layouts |
| [How Varve Works](docs/how-it-works.md) | mental model of generated code, append logs, matrix storage, durability |
| [API Reference](docs/api-reference.md) | practical public API map |
| [Format Author Guide](docs/format-author-guide.md) | policy choices, compression, commit modes, matrix blocks |
| [Self-Check Guide](docs/self-check-guide.md) | deciding whether a failure is format, caller, data, feature, environment, or library |
| [Security Review](docs/adversarial-security-review-2026-07-10.md) | hostile-input threat model, remediated findings, and residual caller obligations |
| [Security Validation](docs/security-remediation-validation.md) | exact verification commands, compatibility evidence, and assurance limits |
| [Read And Allocation Safety](docs/read-allocation-safety.md) | invariant and coverage for length validation, bounded allocation, exact reads, and snapshot extents |
| [Fuzzing And Fault Injection](docs/fuzzing-and-fault-injection.md) | ASan fuzz targets, Miri checks, Windows race tests, and reproducible commands |
| [Performance](docs/performance.md) | repeatable regression protocol, benchmark paths, and integration gate |
| [Scalable I/O](docs/scalable-io.md) | petabyte-scale stream/indexed APIs, batching, sidecars, recovery, and exact cost model |
| [Test Artifact Hygiene](docs/test-artifact-hygiene.md) | fresh test-file creation, success cleanup, failure retention, and build-cache policy |
| [Durability Model](docs/durability-model.md) | flush, sync, transaction-marker, matrix, and replacement ordering |
| [Architecture](docs/architecture.md) | current architectural outline and implementation status |
| [Requirements Boundary](docs/requirements-boundary.md) | what Varve owns versus what callers should implement |
| [Adapter Toolkit Design](docs/adapter-toolkit-design.md) | generic adapter primitives for external binary formats |
| [npTDMS Adapter Boundary](docs/nptdms-adapter-boundary.md) | how npTDMS-documented behavior maps to Varve generic APIs versus external adapter code |

## Minimal Shape

```rust
use varve::varve_format;

varve_format! {
    pub format AppFormat {
        magic: b"APP";
        version: 1;
        schema_hash: computed;
        commit: transaction_marker(on_flush);

        blocks {
            fixed Point(id = 1) {
                x: u32,
                y: u32,
            }

            variable User(id = 2, key = [id]) {
                id: u64,
                name: String,
                flags: u32 = default,
            }
        }
    }
}

let mut writer = AppFormat::create_writer("app.varve")?;
writer.push_point(&Point { x: 1, y: 2 })?;
writer.push_user(&User { id: 7, name: "Ada".to_string(), flags: 0 })?;
writer.flush()?;

let reader = AppFormat::open_reader("app.varve")?;
let user = reader.users()?.get(&7)?;
```

The user declares the format and block types. Varve generates the typed
methods, block metadata, required field encoding, record headers, commit
handling, offset-chain metadata, and diagnostics.

Resource limits are runtime policy, not part of the file format. Ordinary
opens use Varve's allocation-safe standard policy. Use generated
`*_with_resource_limits` methods to raise or lower policy for a handle, or the
legacy `*_with_limits` methods when only fieldwise tightening is desired. An
optional, partial `limits { ... }` declaration can provide format defaults but
never becomes a permanent wire-format ceiling.

## Examples

| Example | Role |
| --- | --- |
| `crates/varve/examples/bmp_physical.rs` | Small physical-format adapter proof for BMP-style headers and raw pixel data, verified against Pillow. |
| `crates/varve/examples/perf_bench.rs` | Local throughput check for append, open, scan, and block access paths. |
| `crates/varve/examples/tdms_physical_reader.rs` | Thin reader entry point for the TDMS physical adapter example. |
| `crates/varve/examples/tdms_physical_writer.rs` | Thin writer entry point for the TDMS physical adapter example. |
| `crates/varve/examples/tdms_physical_adapter.rs` | CLI wrapper around the TDMS adapter for write, append, read, read-bytes, and inspect flows. |
| `crates/varve/examples/tdms_physical/common/adapter.rs` | Large TDMS physical adapter proof using Varve's generic adapter APIs. |
| `crates/varve/examples/tdms_physical/common/example_data.rs` | Multi-channel TDMS scenario fixtures. |
| `crates/varve/examples/tdms_physical/common/verify.rs` | Regression checks for TDMS round trips and segment-layout behavior. |

The TDMS example is intentionally larger than a quickstart. Its job is to prove
that Varve can support a demanding external binary format without hard-coding
TDMS-specific behavior into the library. It also acts as a regression harness so
combined changes do not silently break segment appends, metadata reuse, scalar
types, or cross-tool compatibility.

## Self-Check

Generated formats expose diagnostics and end-to-end self-test helpers:

```rust
let diagnostics = AppFormat::diagnostics();
let file_report = AppFormat::diagnose_file("data.varve");
let self_test = AppFormat::self_test("self-check.varve")
    .with_block(Point { x: 1, y: 2 })
    .cleanup(true)
    .run();
```

These reports classify failures as format definition, caller usage, feature
gate, file data, environment, or library invariant issues. See
[Self-Check Guide](docs/self-check-guide.md).

## Status

Varve 0.4.0 is usable as an alpha library for experimentation and controlled
deployments, at moderate scale through the generated APIs. It includes append-log blocks, keyed collections,
transaction/footer commit policies, schema manifests, diagnostics, merge and
compact helpers, variable-block compression, matrix storage, mmap, and opt-in
zero-copy. Valid native 0.1 append-log wire bytes remain readable in 0.4.0, but
the Rust API is still pre-1.0 and may evolve through semver-signaled minor
releases. Two artifact classes are **not** covered by that statement in 0.4.0
and are rejected with a typed stale-regenerable error rather than read: matrix
sidecars written before VMAT layout v4, and stream/indexed primaries and disk
sidecars written before the generation nonce. Both are regenerated from source
data; see [CHANGELOG](CHANGELOG.md) and
[Migration Guide](docs/migration-guide.md).
The petabyte-scale stream/disk-index API remains behind
`high-cardinality-dev` and has never shipped in a released version.
Progress/cancellation, process-interruption recovery, and sidecar robustness
gates are implemented and executed. The scale gates are not: both positional-I/O
probes (`real_file_positional_io_at_one_pib` and its 1 TiB smoke sibling) are
`#[ignore]`d and have never been executed on any host, and the 1 TiB probe has no
required-mode escape at all. Varve's petabyte-scale claims rest on the cost model
and on tests at far smaller scales, not on a demonstration at that scale.

For the full assurance picture — including that CI has never run on this code and
that no fuzz, Miri or ASan run covers it — see
[Known Limitations §6](docs/known-limitations.md#6-not-verified).

## Local Verification

```powershell
cargo fmt --all -- --check
cargo run -p varve-test-runner -- test --workspace
cargo run -p varve-test-runner -- test --workspace --all-features
cargo run -p varve-test-runner -- clippy --workspace --all-targets --all-features -- -D warnings
```

The runner gives every command a fresh temporary root, preserves that root on
failure, and requires verified cleanup on success. Cargo build caches remain
reusable and are not treated as test data.

CI (`.github/workflows/ci.yml`) runs on Ubuntu and Windows: `cargo fmt --all
-- --check`; a per-feature Clippy matrix with `-D warnings` and `--locked`
covering no-default-features, default, each optional feature alone
(`integrity`, `mmap`, `zero-copy`, `compression-zstd`, `high-cardinality-dev`,
`scalable-fault-injection`), and the all-feature workspace union; default and
all-feature test runs through `varve-test-runner` so a leaked test artifact
fails the build; test runs for the two singleton feature configurations that own
`cfg`-exclusive behaviour (compression without integrity, integrity without
compression), which the default and all-feature runs both compile out; a job
pinned to the declared MSRV (1.95.0) that checks and tests against it and fails
if the pin and the manifests disagree; a `package contents` job asserting that
every published `.crate` file list contains `README.md`, `LICENSE-MIT`, and
`LICENSE-APACHE`; a `package archives + staged consumer` job that builds and
verifies the three real `.crate` archives and then compiles and runs a
downstream consumer against the extracted archives; a `rustdoc (-D warnings)`
job over the workspace's published documentation surface; a blocking
`cargo deny` / `cargo audit` supply-chain job; a
blocking locked fuzz-workspace job (`cargo metadata --locked`, `cargo check
--locked --all-targets`, `cargo audit`, `cargo deny`, and a final
`git diff --exit-code -- fuzz/Cargo.lock`, in that order, so no step can hide a
stale lockfile by regenerating it); a renamed-dependency macro fixture; and a
clean-archive job that builds only the committed tree (`git archive` +
`cargo metadata`/`cargo check --locked`, including the fuzz workspace and both
out-of-workspace fixtures) so an uncommitted workspace member cannot pass CI.

Two of those jobs build crates that deliberately sit outside the workspace,
because some contracts only exist from a consumer's point of view:
`tools/rename-fixture` proves the macros work when the `varve` dependency is
renamed, and `tools/public-api-fixture` derives fixed, variable, and matrix
blocks over **every** public codec and field type the documentation lists and
round-trips each one. The second exists because a mandatory field-codec identity
contract was once added without any test that a documented public type still
compiled as a derived field, and it silently broke `ChunkedBytes` and
`PackedBitmap` fields; nothing inside the workspace noticed, because nothing
inside the workspace derived a block over the public surface the way a consumer
does.

Every CI action is pinned to a commit id and every installed cargo tool to an
exact version, so a future run of the workflow evaluates the same *gate code* it
evaluates today. That is the whole of the claim: the runner images
(`ubuntu-latest`, `windows-latest`) and the `stable` toolchain every job except
`msrv (1.95.0)` requests are rolling by design, because catching a break on a
newer compiler or image is exactly what they are for. A CI run is therefore
**not** reproducible, and a red run on an unchanged commit is an expected
outcome rather than a workflow defect. `msrv (1.95.0)` is the only job that
evaluates a fixed toolchain.

Publication is gated separately from merging. `package contents` proves the file
list of each `.crate`; `package archives + staged consumer` builds the three
`.crate` archives for real (`cargo package --locked`, verification enabled),
extracts them, and rebuilds the public-API fixture's source against the
extracted trees through `[patch.crates-io]`, so the bytes a crates.io user
downloads are compiled and run before release rather than only listed.
`rustdoc (-D warnings)` builds the published documentation surface for all three
crates with warnings denied.

Job results block merges only where branch protection lists them as required
status checks; adding a job to the workflow does not by itself make it a gate.

Optional compatibility harnesses use Python reference libraries:

```powershell
.\.venv-tdms\Scripts\python.exe scripts\verify_tdms_with_nptdms.py
.\.venv-tdms\Scripts\python.exe scripts\verify_nptdms_multichannel_with_varve.py
.\.venv-tdms\Scripts\python.exe scripts\verify_bmp_with_pillow.py
```

## License

Varve is licensed under either of:

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option. See [LICENSE](LICENSE) for the short dual-license notice.
