# Varve

Varve is a Rust library for building append-friendly, segment-oriented custom
binary formats from typed block definitions. The main workflow is
format-first: declare a file contract with `varve_format!`, then use the
generated typed reader and writer APIs.

The workspace contains:

- `varve`: public facade crate
- `varve-core`: runtime, codecs, file I/O, diagnostics
- `varve-macros`: `varve_format!` and derive support

## Start Here

| Document | Use it for |
| --- | --- |
| [Quickstart](docs/quickstart.md) | shortest path from format declaration to write/read |
| [How Varve Works](docs/how-it-works.md) | mental model of generated code, append logs, matrix storage, durability |
| [API Reference](docs/api-reference.md) | practical public API map |
| [Format Author Guide](docs/format-author-guide.md) | policy choices, compression, commit modes, matrix blocks |
| [Self-Check Guide](docs/self-check-guide.md) | deciding whether a failure is format, caller, data, feature, environment, or library |
| [Architecture](docs/architecture.md) | current architectural outline and implementation status |
| [Requirements Boundary](docs/requirements-boundary.md) | what Varve owns versus what callers should implement |
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

Varve is usable as an alpha library for experimentation and controlled internal
projects. It already includes append-log blocks, keyed collections,
transaction/footer commit policies, schema manifests, diagnostics, merge and
compact helpers, variable-block compression, matrix storage, mmap, and opt-in
zero-copy. Treat the wire format as pre-stabilization until the first v0.1
release is cut and pinned.

## Local Verification

```powershell
cargo fmt --check
cargo test
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
```
