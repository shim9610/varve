# Migration Guide

Varve migrations are explicit. Normal typed reads reject block version
mismatches; migration code names both the source and target block types and
performs semantic conversion in Rust.

## Version Blocks Deliberately

Keep the same block id when the new type represents the same logical block, and
bump the block version when existing bytes no longer decode as the new type.

```rust
use varve::{VarveBlock, VarveMigration};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 20, version = 1, kind = "fixed")]
struct CounterV1 {
    value: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 20, version = 2, kind = "fixed")]
struct CounterV2 {
    value: u32,
    doubled: u32,
}

struct CounterMigration;

impl VarveMigration<CounterV1, CounterV2> for CounterMigration {
    fn migrate(from: CounterV1) -> varve::Result<CounterV2> {
        Ok(CounterV2 {
            value: from.value,
            doubled: from.value.saturating_mul(2),
        })
    }
}
```

`From::ID` and `To::ID` must match. `blocks_migrated` reads records matching
`From::VERSION`, decodes `From`, applies the migration, and returns owned target
values.

```rust
let file = OldFormat::open_readonly("old.varve")?;
let values = file.blocks_migrated::<CounterV1, CounterV2, CounterMigration>()?;
```

## Publish Migrated Data

`blocks_migrated` does not rewrite the file. To publish a migrated file, create
an output with the new format and push the migrated values.

```rust
let old = OldFormat::open_readonly("old.varve")?;
let migrated = old.blocks_migrated::<CounterV1, CounterV2, CounterMigration>()?;

let mut out = NewFormat::create("new.varve")?;
for value in &migrated {
    out.push(value)?;
}
out.flush()?;
out.sync()?;
```

For keyed data, first decide the migration boundary:

- Migrate raw historical puts when every old record should be converted.
- Materialize old keyed state first when tombstones and ops define the intended
  final state.
- Compact before or after migration when the output should drop old ops,
  tombstones, and unrelated block ids.

## Matrix Byte Copy Scaffold

For preallocated matrix data, unchanged fixed-stride cell payloads can be copied
without decoding and re-encoding:

```rust
let mut old = OldMatrixFormat::open_reader("old.varve")?;
let mut out = NewMatrixFormat::create_writer_with_dims("new.varve", dims)?;

out.copy_matrix_cell_bytes_from::<OldCell, NewCell>(&mut old, key)?;
```

`OldCell::DIMENSIONS` and `NewCell::DIMENSIONS` must match exactly, and
`SLOT_STRIDE` must be equal. The source cell must be committed; the read path
still verifies enabled source CRC evidence. The target writer writes the bytes
into the target slot and commits the target cell. Semantic conversions,
keyspace iteration, and output publication policy remain caller-owned.

## Field Evolution

For variable blocks, preserve field ids for unchanged fields. Add new fields
with new ids and a valid default when old records may be decoded by the new
type. Use an explicit migration when a field is renamed, split, joined, deleted
without a default, or changes meaning.

Embedded manifests can help inspect what was written, but the caller's
`FormatSpec` remains authoritative for typed reads and migrations.

## Performance Check

Run the ignored smoke tests when a migration changes scan volume, materializes
large keyed state, rewrites output files, or changes codecs used by migrated
blocks:

```powershell
cargo test -p varve --test perf_smoke -- --ignored --nocapture
cargo test -p varve --features mmap --test perf_smoke -- --ignored --nocapture
cargo test -p varve --features mmap,zero-copy --test perf_smoke -- --ignored --nocapture
```

For expensive semantic conversions, add a local migration-sized case and compare
small, medium, and large data before integrating.
