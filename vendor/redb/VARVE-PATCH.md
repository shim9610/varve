# Varve's read-only savepoint extension

Base: crates.io `redb` 4.3.0, upstream commit
`2de02fa48d1b8e265f388ea90151bf86f452e562`. The MIT and Apache-2.0 license files
are retained. This is a local patch, not an upstream redb release.

`ReadTransaction::persistent_savepoint_prefix()` returns at most two ascending
persistent savepoint IDs from the same snapshot as its data tables.
`WriteTransaction::persistent_savepoint_prefix()` queries staged state with
bounded enumeration, without materializing the complete savepoint list.

The prefix is parsed from the protected system root before durable and
non-durable publication, and copied with the root under the existing header-state
mutex. Ordinary readers never access live system-tree pages. Open and successful
integrity repair initialize the prefix from the file. The exclusive-writer open
loads existing savepoint registrations without constructing a write transaction.
The experimental multiprocess path reads the prefix under its existing shared
header hold; Varve does not enable that feature.

`ReadableDatabase::begin_read_with_savepoint()` returns the current user root
and oldest persistent savepoint's user root from one publication. The cached
prefix includes two fixed-size descriptors (id, transaction id, user root).
Both exact transaction ids receive independent read references before releasing
the existing tracker/header synchronization. Keeping only the older reference
would not protect current non-durable pages from non-durable reclamation.
Historical read transactions do not retain the savepoint system catalog.

No on-disk format, page reclamation protocol, durability setting, or new lock is
introduced. Changes are in `db.rs`, `transaction_tracker.rs`, `transactions.rs`
and `tree_store/page_store/{page_manager,savepoint}.rs`. Additional regressions are in
`tests/read_savepoint_prefix.rs`.

The crates.io archive retains `examples/derive_value_impl.rs` but omits its
path-only `redb-derive` development dependency. The local manifest restores the
published `redb-derive` 0.1.0 solely to execute upstream all-target checks.
Its example imports the `Value` trait required by that released derive macro.
The standalone lockfile is retained and its yanked test-only `chacha20` 0.10.1
was refreshed to 0.10.2. One pre-existing trailing comma was removed for the
Rust 1.95 Clippy checks. The multiprocess cache test accounts for the new
system-catalog read; failed-repair tests also reject unavailable summaries
without disabling unaffected read-only table access.

Varve now uses its own index and checkpoint implementation at runtime. This
patched standalone crate is retained only as a development/test oracle and for
fuzz comparisons. It is not embedded in the published `varve-core` source.
`scripts/check_runtime_dependencies.py` verifies that redb is absent from the
production dependency graph. The standalone crate remains the reference for
upstream all-feature, integration, and documentation tests.
