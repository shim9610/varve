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
**[Known Limitations](docs/known-limitations.md)**; upgrading from 0.4.0 or 0.3.0
is covered in **[API Changes](docs/api-changes.md)**.

**Ready for:**

- Declaring a custom binary format and getting typed readers and writers for it.
- Append-log files whose record count and distinct-key count fit comfortably in
  RAM alongside the application. This is the default path, and the only part of
  the library with an executed test history behind it. Budget **16 bytes of
  resident directory per record** — the record's offset and its committed bit;
  everything else is rebuilt from the record when a read asks. By default open
  scans the whole file to build it, and two opt-in policies change that:
  `segment_on_flush` makes the open walk one record per commit point, and
  `open_digest_on_flush` plus `open_readonly_lazy` makes it frame one record and
  build no directory at all, leaving the caller to build only the part it needs
  with `record_map`. Measured on a 50,000-record file: **100,316 read syscalls
  at open, 516, and 20.**
- Bounded-memory ingest and point lookup over data far larger than RAM — with two
  caveats that a reader should weigh before choosing Varve for this workload.
  It is behind the `high-cardinality-dev` feature, has never shipped in a released
  version, and its API may still change. **And its four modules (`disk_index.rs`,
  `stream.rs`, `indexed.rs`, `scan_control.rs`) have not been walked against the
  project's five internal invariants**, while the matrix and resident paths have.
  The only path offered for the larger-than-RAM workload is the least-audited code
  in the tree.
- Preallocated matrix storage with **fixed dimensions**, a live page count whose
  page-index mirror (~96 bytes per live page) fits the process memory budget — the
  commit-map payload itself is demand-cached and bounded, so it does not have to —
  at most 16,000,000 cells unless you raise `max_matrix_cells` explicitly, on a
  sparse-capable filesystem.
- Reading a file with hostile-input ceilings applied (`ReadLimits::UNTRUSTED`).
  The typed refusals and bounded allocations are real and tested: no production
  read path uses an unknown length as a framing primitive. Lengths, counts and
  offsets are decoded with checked arithmetic, validated in full against the
  captured file snapshot or the enclosing slice, charged against the resolved
  runtime limit for that resource, and only then converted, reserved fallibly,
  and read as exactly that range. **EOF is not a framing mechanism** — append
  logs stop at the captured snapshot length and sidecars must match their declared
  total length exactly. Read the fuzz/Miri/ASan line below before treating this as
  a hardened surface.

**Not ready for:**

- **Measurements from one platform.** The test suite is executed on both Linux
  and Windows — CI runs `ubuntu-latest` and `windows-latest` on every push to
  `main`, and both were green — so behaviour is no longer Windows-only. The
  *numbers* still are: every syscall count, residency figure and open cost in
  these documents was measured on Windows x86_64, and the constants differ on a
  filesystem with different extent and allocation behaviour. Targets other than
  those two remain compile-verified only, and additionally lose the allocation
  map and hole punching by construction.
- **Matrices larger than 16,000,000 cells out of the box.** That is the default
  `max_matrix_cells`, checked per matrix block **and** as a running aggregate
  across all of them, so a 4096 x 4096 matrix fails at *create* with
  `LimitExceeded { resource: "matrix cells" }` until the ceiling is raised.
- **Matrices on a filesystem that cannot represent holes.** Sparseness is
  requested best-effort and failure is ignored. On a non-sparse volume the
  declared extent is really allocated, create cost and open cost become
  proportional to the *declared* cell count, and `clear_category` writes
  `cells / 8` bytes instead of punching a hole. There is no error — only slowness.

- **Matrices that grow.** Dimensions are fixed at create time and there is no
  grow path, so a matrix cannot represent an indefinitely growing stream. Use the
  stream/indexed APIs for that.
- **Frequent matrix opens.** Opening a matrix is not `O(1)`, because by default
  opening it *verifies* it. Open is independent of file size but proportional to
  the candidate page set: a matrix with **one live page** still reads about
  **70 KB over 17 pages** (Windows/NTFS). Declaring
  `MatrixMetadataVerification::OnDemand` takes that to **32 bytes over 0 pages**,
  and what you give up is the matrix announcing commit-map damage at open — see
  the [Capability Boundary](#capability-boundary) below for the exact trade.
  Residency is no longer part of this problem: an open retains **no** commit-map
  payload. What no declaration removes is the persisted page index, read in full
  at open — 8 bytes read and ~96 bytes resident per live page — so open is
  `O(live pages)` and never `O(1)`. At a million live pages that is 8 MB read and
  ~96 MB resident before a cell is addressed.
- **Matrices whose live page count is large against a tightened
  `max_matrix_bitmap_bytes`.** That ceiling now bounds the demand cache, so a
  matrix whose live set is wider than it **opens and is served**. What can still
  refuse an open is the page-index mirror above: it is charged against the same
  ceiling and is not evictable.
- **Petabyte-scale merge or compact.** Keyed merge and compact are resident-only.
  Varve exports no bounded-memory external merge or compact.
- **Channel-selective reads.** Reading a subset of channels without paying full
  block I/O has been designed but not implemented: there is no `channel_view`
  type, no DSL key, and no generated method anywhere in the workspace. Nothing
  about it is callable. One conclusion from that design holds regardless, because
  it is a property of the current on-disk layout: an interleaved payload cannot be
  read channel-selectively below full block I/O without re-emitting the payload.
- **Live views of a file another handle is writing.** Resident readers are
  snapshots as of open. Matrix readers are not snapshots at all: each commit-map
  page is as of the first read that faulted it in, and since 0.5.0 no policy pins
  a whole-map instant. A reader that needs one must coordinate it.
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
bounded by the cells actually used rather than by the declared cell count —
**provided the filesystem represents the unwritten extent as holes.** That is
requested best-effort at create and the failure is ignored, so on exFAT/FAT32,
some network and virtual volumes, or after a copy by a tool that expands holes,
the declared extent is genuinely allocated and both the create-time and the
open-time cost become proportional to the declared cell count instead. Nothing
reports this; the symptom is slowness.

By default a matrix is capped at **16,000,000 cells and 16,000,000 per dimension**
(`ReadLimits::STANDARD`), checked per matrix block and as a running aggregate
across every matrix block in the format. Larger matrices need
`with_max_matrix_cells` / `with_max_matrix_dimension` or a `limits { }`
declaration, or create fails with
`LimitExceeded { resource: "matrix cells" }`.

**Open-time reads are bounded by the candidate *page* count, not by the working
set, so open is not `O(1)` — but residency is.** An undeclared open resolves to
`MatrixMetadataResidency::Lazy { DEFAULT_CACHE_BYTES }` plus
`MatrixMetadataVerification::AtOpen`: it retains **no** commit-map payload, and
then verifies, which reads the union of the pages named by the persisted page index
and the pages the platform's allocation map reports as written. Measured: a matrix
with one live page reads 69,800 bytes over 17 pages, one with 64 live pages reads
267,800 bytes over 65 pages, and **both hold 0 resident bitmap bytes**. Residency
afterwards follows the working set exactly — one 4096-byte page per distinct
commit-map page addressed, evicted LRU at the declared bound — so
`max_matrix_bitmap_bytes` bounds a cache and a live set wider than it opens
normally. Declaring `MatrixMetadataVerification::OnDemand` takes the same opens
down to 32 and 1,040 bytes and 0 pages; what you give up is the matrix announcing
commit-map damage at open, the whole-category quarantine, and the strict-recovery
writer gate. What remains unavoidable is the persisted page index: 8 bytes read and
~96 bytes resident per live page.

**What `OnDemand` actually costs you, since it is the one knob here worth
understanding.** The commit-map half of `MatrixRecoveryReport` is produced by the
verification pass and by nothing else. With verification off, the `Recoverable`
`MatrixCorruptionKind::CommitMap` finding is not produced, a damaged category is
not failed closed **as a whole** (`Error::MatrixCommitQuarantined` does not fire,
so only the pages a read touches refuse), the `RebuildCommitMap` / `ClearCategory`
recommendations are absent, and a `RecoveryPolicy::Strict` writer is not stopped
from mutating a category whose commit map holds a damaged page. One class is worse
than deferred rather than merely later: the candidate set is the persisted page
index **unioned with** the platform's allocation map, and that second term is the
only thing that ever examines a page the matrix never published, so **stray bytes
there are never looked at at all**. `verify_matrix_metadata()` on a reader, writer
or `VarveFile` runs the same pass on demand and recovers the findings and the
recommendations — but not the quarantine and not the writer gate, which are derived
once, at open. No read answers from unverified bytes under any policy: every page
faulted in is authenticated against its stored digest first, so this is a choice
about *when damage is announced*, not about whether bytes are checked. Full table
in [Known Limitations §1.4](docs/known-limitations.md#14-lazy-is-the-default-verification-is-what-still-happens-at-open).

Matrix dimensions are fixed at create time; there is no grow path.

**Platform support, since it changes the matrix cost model rather than only the
confidence in it:**

| Target | Status |
| --- | --- |
| Windows x86_64 MSVC | executed, and the platform every measured number in these documents comes from. |
| Linux x86_64 | executed. Three defects were found here that every Windows run had passed: advisory locks belong to the open file description and `fork` duplicates it, ext4 hands a just-freed inode straight back to the next create, and extent reporting is precise where NTFS rounds to the allocation run. The allocation map and hole punching exist here too, so the same cost model applies — with **different constants, none of them measured**. |
| macOS and other targets | no allocation map (matrix open visits only pages the index names) and no hole punch (`clear_category` writes `cells / 8` bytes). Compile paths exist; nothing executed. |

Full numbers, arithmetic you can apply to your own cell count, and the trade-offs
of each policy are in
[Known Limitations §1](docs/known-limitations.md#1-matrix-opening-a-matrix-is-not-o1-because-opening-it-verifies-it).

Varve should own the reusable binary-format mechanics. Application-specific
meaning, domain transforms, and compatibility with an external specification
remain caller code.

## Start Here

| Document | Use it for |
| --- | --- |
| [Quickstart](docs/quickstart.md) | shortest path from format declaration to write/read |
| [Known Limitations](docs/known-limitations.md) | what a user actually hits: matrix open cost and residency, resident-API scale, unimplemented features, and what is not verified |
| [How Varve Works](docs/how-it-works.md) | mental model of the declaration, the generated code, append logs, matrix storage, and durability |
| [API Reference](docs/api-reference.md) | practical public API map, including the resource-limit surface and the generic adapter toolkit |
| [Format Author Guide](docs/format-author-guide.md) | policy choices, compression, commit modes, matrix blocks |
| [Custom Codec Guide](docs/custom-codec-guide.md) | writing `VarveEncode`/`VarveDecode` for a field type with its own stable wire meaning |
| [Update And Compact Guide](docs/update-compact-guide.md) | keyed put/op/tombstone/compact workflows, and when direct replacement is the right tool instead |
| [Scalable I/O](docs/scalable-io.md) | the experimental `high-cardinality-dev` stream/indexed APIs, batching, sidecars, recovery, and exact cost model |
| [Durability Model](docs/durability-model.md) | flush, sync, transaction-marker, matrix, and replacement ordering |
| [Recovery Model](docs/recovery-model.md) | matrix corruption classification, findings, and the primitive recovery actions a caller may apply |
| [Self-Check Guide](docs/self-check-guide.md) | deciding whether a failure is format, caller, data, feature, environment, or library |
| [API Changes](docs/api-changes.md) | migrating from 0.3.0: new/changed/removed items, behaviour changes at unchanged signatures, and what happens to existing files |
| [Migration Guide](docs/migration-guide.md) | explicit block-version migration, and the wire changes that have no automatic path |
| [Format Spec](docs/spec.md) | the implementation contract: wire layout, versions, adapter boundary, and the obligations each surface carries |
| [Changelog](CHANGELOG.md) | release changes and source-compatibility notes |

That table is the whole published set. The project also keeps internal working
notes — adversarial review reports, architecture and design studies, a
contributor invariant checklist, performance and fuzz records, and the routing
artifacts of each hardening round. Those are development process material and are
not published. Nothing in the documents above depends on reading them: where a
published document used to cite one, it now states the substance directly, and
the assurance those notes record — together with its limits — is summarised under
[Status](#status) below and in
[Known Limitations §6](docs/known-limitations.md#6-not-verified).

## Add It To Your Project

Varve is **not on crates.io**, so `cargo add varve` will not find it. Depend on
the git repository and pin a tag:

```toml
[dependencies]
varve = { git = "https://github.com/shim9610/varve", tag = "v0.6.0" }
```

Pin the tag rather than tracking `main`: `main` moves, and this project is at a
stage where it moves in ways that change behaviour at an unchanged signature —
see [API Changes](docs/api-changes.md).

Requires Rust **1.95** or newer (`rust-version = "1.95"`).

Every capability beyond the base format is an optional feature, off by default;
a build that enables none is the smallest one. Enable what a format declaration
actually asks for:

```toml
varve = { git = "https://github.com/shim9610/varve", tag = "v0.6.0",
          features = ["integrity", "compression-zstd"] }
```

| Feature | Turns on |
| --- | --- |
| `integrity` | per-record and per-page checksum verification |
| `compression-zstd` | the zstd codec for compressed blocks |
| `mmap` | memory-mapped reads |
| `zero-copy` | borrowed reads that avoid a copy out of the page cache — also enables `mmap` |
| `high-cardinality-dev` | the experimental stream/indexed APIs — see [Scalable I/O](docs/scalable-io.md) |
| `scalable-fault-injection` | test-only fault injection; also enables `high-cardinality-dev`, and is not for production builds |

[Format Author Guide](docs/format-author-guide.md) says which declaration
choices require which feature, and what each costs.

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

Two fields on `ReadLimits` are *declarations* rather than ceilings:
`matrix_metadata_residency` and `matrix_metadata_verification`. Neither has a
"tighter" direction to meet, so both compose by precedence instead — and, as of
0.5.0, a **silence never overwrites a declaration**. A `*_with_resource_limits`
call that does not mention a policy leaves the spec's alone; one that does mention
it wins. `*_with_limits` keeps the spec's and now also takes a runtime one where
the spec declared none. Through 0.4.0 this was broken in both directions: raising
`max_matrix_bitmap_bytes` alone silently reverted a declared
`Lazy { cache_bytes }` to the eager policy, discarding the only bound on matrix
metadata memory and often failing the open on the very admission limit you were
raising. **If you wrote a workaround for that, it is no longer needed.** There is
still no `varve_format!` DSL key for either policy; declare them on the spec with
`FormatSpec::with_read_limits` or pass them in a `ReadLimits` value. See
[Known Limitations §1.6](docs/known-limitations.md#16-the-matrix-residency-and-verification-policies-have-no-varve_format-dsl-key).

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

The boundary it demonstrates is the general one for external formats. Varve owns
the reusable mechanics: declaring and validating file headers, segment lead-ins,
metadata and raw regions and footers, finalized offsets and segment bounds;
appending complete segments; and exposing offsets, lengths, field values, byte
ranges and tolerant scan reports. Above that sit generic adapter primitives —
tagged-value codecs, chunk index builders over raw regions, segment reducers, and
adapter self-checks. The adapter author owns the domain meaning: for TDMS that
means object paths, raw-data-index grammar, property typing, timestamp and
waveform conversion, scaling, DAQmx raw scalers, interleaving, sidecar policy and
export. **Varve provides no TDMS reader or writer**; the files under
`crates/varve/examples/` are adapter proofs, not a supported TDMS API.

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

Varve 0.6.0 is usable as an alpha library for experimentation and controlled
deployments. Through the **stable, released** APIs that means moderate scale —
files whose record and key counts fit in RAM. The larger-than-RAM path exists but
is behind `high-cardinality-dev`, has never shipped, and is the least audited code
in the tree; "far larger than RAM" in the capability table above describes that
feature-gated family, not the default one. It includes append-log blocks, keyed
collections, transaction/footer commit policies, schema manifests, diagnostics,
merge and compact helpers, variable-block compression, matrix storage, mmap, and
opt-in zero-copy. Valid native 0.1 append-log wire bytes remain readable in 0.6.0,
but the Rust API is still pre-1.0 and may evolve through semver-signaled minor
releases.

**Four artifact classes are not covered by that statement in 0.6.0.** They are
rejected with a typed error rather than misread, but two of them hold data and two
are regenerable, and the difference is what it costs you:

| Class | Outcome | Cost to you |
| --- | --- | --- |
| **Matrix native file** (`VMAT` v1/v2/v3) | `Error::FormatVersionMismatch { expected: 4, .. }` | **recreate the matrix and copy its contents yourself** — Varve has no migration tool |
| **Native file pinned with a computed schema hash** from 0.3.0 or earlier | `Error::SchemaHashMismatch` | **recreate the file, or re-derive the pinned literal** — the hash algorithm changed twice |
| Matrix sidecar written before sidecar v3 | `Error::MatrixSidecarMismatch("sidecar version")` | regenerate; sidecars are resume state, not data |
| Stream/indexed primaries and disk sidecars written before the generation nonce | typed refusal; `rebuild_disk_index` for the sidecar | regenerate. Hypothetical in practice — this family never shipped |

See [API Changes](docs/api-changes.md#read-this-first-files-written-by-an-older-version),
[CHANGELOG](CHANGELOG.md) and [Migration Guide](docs/migration-guide.md).

On the evidence behind that table: **no test in the suite opens a file written by an
older version.** The append-log claim was checked by hand on 2026-07-25 — files
written by the real 0.1.0 and 0.3.0 builds opened under 0.4.0 with every field
intact, and a 0.3.0 computed-hash-pinned file was refused as documented — but that
was a single manual run over a two-field format, and the matrix and sidecar refusal
rows rest on source inspection alone. See
[Known Limitations §6.7](docs/known-limitations.md#67-the-backward-compatibility-table-what-is-now-executed-and-what-still-is-not).
The petabyte-scale stream/disk-index API remains behind
`high-cardinality-dev` and has never shipped in a released version.
Progress/cancellation, process-interruption recovery, and sidecar robustness
gates are implemented and executed. The scale gates are not: both positional-I/O
probes (`real_file_positional_io_at_one_pib` and its 1 TiB smoke sibling) are
`#[ignore]`d and have never been executed on any host, and the 1 TiB probe has no
required-mode escape at all. Varve's petabyte-scale claims rest on the cost model
and on tests at far smaller scales, not on a demonstration at that scale.

The performance checks are in the same position. They are regression guards
rather than product benchmarks, and **they run in no job**:
`crates/varve/tests/perf_smoke.rs` is entirely `#[ignore]`d, as is the
one-million-key RSS/allocator stress probe in
`crates/varve/tests/high_cardinality.rs`. Every performance number in this
documentation therefore comes from a manual run on a named date and host, on
Windows x86_64; none of it is continuously enforced.

On security assurance specifically: an adversarial hostile-input review was
performed on 2026-07-10 against the pre-0.2 hardening implementation. It treated
file bytes, matrix metadata, sidecars, file paths and concurrent external
filesystem activity as untrusted, and the format declaration, generated code and
custom codec implementations as trusted. Its findings were remediated for 0.2.0
and the remediation was verified on 2026-07-10 and 2026-07-11 across fail-closed
resource limits, open-object snapshots, strict recovery classification,
copy-on-write fixed replacement, canonical decoding, native and matrix structural
validation, sidecar and lock bounds, and generated API trust boundaries. **That
review predates 0.4.0 and every hardening round recorded in the changelog, and it
is not a substitute for the fuzz, Miri and ASan runs that have not happened.**

For the full assurance picture — including that no fuzz, Miri or ASan run covers
this code, and which platform each number came from — see
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
