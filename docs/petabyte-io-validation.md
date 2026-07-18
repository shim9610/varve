# Petabyte I/O Specification Validation

The first clean validator rejected Spec v2 until the following findings were
resolved. This file records the disposition used by the final specification.

| Finding | Disposition |
|---|---|
| Non-durable redb changes can grow across an indefinitely unsynced generation | Every bounded sidecar batch commits with `Durability::Immediate` and quick repair enabled. Batch bytes and records are runtime bounded. |
| A large redb database may perform full crash repair on open | Every mutating redb transaction enables quick repair, which also enables two-phase commit. The dependency is pinned to the validated 4.1 database behavior. |
| Savepoint call ordering was underspecified | Final spec fixes exact transaction order and allows exactly one generation savepoint. |
| Truncate-before-savepoint validation could destroy data | Restore is staged and fully validated in an uncommitted redb transaction before native mutation. Native truncate/sync happens before committing the already-validated restore. |
| Macro `key_index` declaration had no canonical core plan | Generated `DiskIndexPlan` carries canonical descriptors and a digest; core validates it against `FormatSpec`. |
| Global keyed chains conflict with non-disk keyed blocks | Scalable append/delete methods are omitted and core calls rejected for keyed blocks lacking a disk descriptor when keyed chaining is enabled. |
| File/record/scan limits remained total ceilings | Scalable normal operations do not apply total file, total record, or scan budgets. Explicit scans receive separate runtime budgets. Per-record/logical/cache/batch limits remain. |
| Batch errors could hide partially written chunks | `BatchAppendError` carries the compact written summary and poisons the dirty generation; explicit restore rolls it back to the last durable checkpoint. |
| Typed I/O boundary did not close old raw routes | Scalable sidecar point reads require private untrusted-pointer to checked-span conversion. `SnapshotFile` growth becomes fallible. Resident raw metadata remains outside the scalable contract and cannot feed scalable I/O internally. |
| Creation/concurrency were unspecified | Initial clean state is published only after native sync; sidecar replacement is atomic and parent-synced where supported. One writer is cross-process locked; separately opened cross-process readers during a writer may receive `IndexBusy`. |
| PiB and memory gates were nondeterministic | Bounds are tested with virtual typed snapshot extents and test I/O counters; process-isolated release probes cover real files and allocation/RSS slopes. Platform sparse probes are supplemental. |

With these amendments there is no remaining specification blocker. The feature
must remain `high-cardinality-dev` if any durability or bounded-memory gate
cannot be demonstrated by the implementation.

## Implementation Adversarial Follow-Up

A later clean-context implementation review found the following concrete
defects. The current implementation and regression suite record these
dispositions:

| Finding | Implemented disposition |
|---|---|
| Dirty `.vks` bootstrap or dirty `.vki` rebuild could promote an unsynced native tail | Bootstrap refuses every existing `.vks`; rebuild inspects an existing `.vki` and refuses dirty state. Dedicated tests preserve native length and dirty metadata. |
| Restore validated format-specific tails after truncation/sidecar commit | The staged checkpoint is validated against base EOF before `set_len`; a forged undeclared tail test proves native bytes and dirty state remain untouched. |
| Rebuild used a fixed 16,384-record commit cadence even for smaller configured batches | Rebuild checks `DiskIndexWriteBatch::can_accept_*`, commits, opens a new transaction, and retries. A `max_records = 2` five-record rebuild passes. |
| Finite `max_sidecar_len` became a total `.vki` lifetime ceiling | Paged `.vks`/`.vki` opens no longer compare total backing-file length to a materialization limit; a one-byte policy can open a larger valid index. |
| Low-level keyed-chain mutation could bypass the disk plan | Runtime checks cover all low-level routes. Under `high-cardinality-dev`, manual `VarveBlock` implementations must declare `IS_KEYED`; omission has a compile-fail fixture, so manual keyedness cannot default to false. |
| Atomic sidecar replacement lacked parent-directory durability and regression coverage | Every successful replacement calls parent sync. Unix requires directory `sync_all`; Windows attempts it and tolerates only documented unsupported-style errors. A call-observation test fails if the sync call is removed. |
| Disk-index causes were flattened to strings | `Error::DiskIndex` retains `Box<DiskIndexError>` as its source; redb database contention maps to `Error::IndexBusy`. |
| Record and key encoders could allocate before checking their configured output limits | A bounded `Encoder` stops accepting bytes at the limit and is wired into scalable record, tombstone, index-update, rebuild, and point-lookup key paths. End-to-end tests prove no native mutation on payload overflow. |
