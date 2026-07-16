# Varve Quickstart

This guide shows the shortest path from format declaration to reading and
writing a file. For deeper API lists, see `docs/api-reference.md`. For failure
triage, see `docs/self-check-guide.md`.

## 1. Declare A Format

Prefer the format-first macro. It generates block structs, typed reader/writer
wrappers, and the static `FormatSpec`.

```rust
use varve::varve_format;

varve_format! {
    pub format AppFormat {
        magic: b"APP";
        version: 1;
        endian: little;
        schema_hash: computed;
        manifest: embedded;
        index: [scan_on_open, block_offset_chain, keyed_offset_chain];
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
```

Generated items include:

- `Point`, `User`
- `AppFormat`
- `AppFormatReader`, `AppFormatWriter`
- `AppFormatRead`, `AppFormatWrite`
- typed methods such as `push_point`, `push_user`, `points`, and `users`

The `limits` block is optional and may contain only the defaults a format author
wants to suggest. Limits are resolved when a reader or writer is opened; they
are not encoded into the file or schema. Ordinary opens use an allocation-safe
standard policy. Use `open_reader_with_resource_limits` or
`create_writer_with_resource_limits` to raise or lower policy for that handle.
Legacy `*_with_limits` calls only tighten the resolved policy.

## 2. Self-Check The Format

Run this before wiring Varve into application logic. It verifies that generated
APIs, codecs, keyed lookup, and file roundtrip agree for representative values.

```rust
let report = AppFormat::self_test("app.selfcheck.varve")
    .with_block(Point { x: 1, y: 2 })
    .with_keyed_block(User {
        id: 7,
        name: "Ada".to_string(),
        flags: 0,
    })
    .cleanup(true)
    .run();

assert!(report.passed(), "{report:#?}");
```

If this fails, inspect `report.steps`. The `domain` field tells you whether to
look at the format declaration, caller usage, feature flags, file bytes,
environment, or a possible library invariant.

## 3. Write Data

```rust
let mut writer = AppFormat::create_writer("app.varve")?;

writer.push_point(&Point { x: 1, y: 2 })?;
writer.push_user(&User {
    id: 7,
    name: "Ada".to_string(),
    flags: 0,
})?;

writer.flush()?; // writes buffered bytes, manifests, checkpoints, commit marker
writer.sync()?;  // asks the OS for durable persistence
```

`create_writer` truncates the target file. Use `open_writer` to append to an
existing file.

Native records can be replaced even when their encoded size changes. Varve
streams a new file generation and publishes it atomically; already-open readers
keep their previous snapshot.

```rust
writer.replace_user(
    0,
    &User {
        id: 7, // keyed replacement must preserve the key
        name: "Ada Lovelace".to_string(),
        flags: 0,
    },
)?;
```

## 4. Read Data

```rust
let reader = AppFormat::open_reader("app.varve")?;

let points = reader.points()?;
let first = points.get(0)?;

let users = reader.users()?;
let ada = users.get(&7)?;
```

Append-log readers are snapshot-on-open. They do not live-tail a writer. Open a
new reader when you want a later committed append snapshot. The snapshot pins
the opened object and logical EOF; it does not block another process from
mutating that same object, so coordinate writers and select an integrity policy
when corruption detection is required. Matrix commit maps are also captured on
open, but matrix slot bytes are in-place storage; do not overlap a matrix reader
with writes to slots it may read.

## 5. Diagnose Existing Files

```rust
let report = AppFormat::diagnose_file("app.varve");
for item in &report.items {
    eprintln!("{:?} {:?} {}: {}", item.severity, item.domain, item.code, item.message);
}
```

This checks static format health, open compatibility, payload decoding,
compression/integrity feature gates, embedded manifest consistency, and matrix
recovery findings when applicable.

## Matrix Quickstart

Matrix blocks are for bounded, runtime-sized grids with direct cell addressing.
They are not append-log records.

```rust
use varve::varve_format;

varve_format! {
    pub format AnalysisFormat {
        magic: b"ANL";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            record_payload: 67_108_864;
            materialized_bytes: 268_435_456;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
        }
        endian: little;
        schema_hash: computed;

        dims {
            scan: u32,
            ch: u32,
        }

        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
            singles = [master_grid];
            per_channel = [threshold];
        };

        aux {
            thumbnail: 1024,
        }

        blocks {
            matrix Cell(id = 10, dims = [scan, ch], category = analysis) {
                value: u32,
            }
        }
    }
}

let dims = AnalysisFormatDims { scan: 2, ch: 3 };
let mut writer = AnalysisFormat::create_writer_with_dims("analysis.varve", dims)?;
let key = CellKey { scan: 1, ch: 2 };

writer.write_cell(key, &Cell { value: 99 })?;
writer.commit_cell(key)?;
writer.write_thumbnail_aux(0, &[1, 2, 3, 4])?;
writer.flush()?;

let mut reader = AnalysisFormat::open_reader("analysis.varve")?;
let value = reader.cell(key)?;
let thumbnail_prefix = reader.read_thumbnail_aux(0, 4)?;
```

Matrix reads trust commit bits. Written but uncommitted cells return
`Error::MatrixNotCommitted`.

## Choosing The Right Storage Shape

| Need | Use |
| --- | --- |
| Small fixed-shape records | `fixed` block |
| Evolvable records with optional/default fields | `variable` block |
| Latest value by key | `variable` or `fixed` with `key = [...]` |
| Append-friendly updates | `push`, `push_op`, `delete`, compact later |
| Bounded direct cell access | `matrix` block |
| Noncommit preallocated bytes near matrix data | `aux { name: len }` |

## Minimum Verification Before Real Data

Run:

```powershell
cargo fmt --check
cargo test
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

For format-specific confidence, also add a test in your application that calls
`YourFormat::self_test(temp_path)` with representative values.
