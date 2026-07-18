# High-Cardinality Index Work Routing

All work is dev-only and follows `docs/high-cardinality-index-spec.md`.

## Worker A: Streaming core

Owns `crates/varve-core/src/stream.rs` and narrowly required visibility/refactor
changes in `file.rs` and `snapshot.rs`. Implements bounded scanner, stream
reader/writer, iterators, resident metrics, supported-policy gates, and tests
local to the module. Must not edit macros, manifests, disk index, or docs.

## Worker B: Disk index

Owns `crates/varve-core/src/disk_index.rs`, workspace/core/ facade Cargo
manifests, and dependency lock updates. Uses optional `redb = 4.1` behind
`high-cardinality-dev`; implements bounded cache options, metadata/key/value
codecs, MVCC lookup, rebuild, and sidecar validation. May add module-local
tests. Must not edit stream or macros.

## Worker C: Macro DSL and generation

Owns `crates/varve-macros/src/lib.rs` and new macro UI fixtures only. Parses
`key_index = memory|disk`, validates placement/feature use, leaves resident
generation unchanged, and generates stream/indexed typed wrappers against the
frozen core interface. Must not edit runtime core or manifests.

## Main integration

Owns shared exports/errors, cross-module writer orchestration, public integration
tests, fault harness, counting allocator/stress/performance tests, docs, and all
merge conflict resolution. No worker output is accepted before independent
spec/diff verification.

