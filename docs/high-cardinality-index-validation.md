# High-Cardinality Development Validation

Date: 2026-07-17

Historical snapshot. The follow-up execution record is
[`scalable-stabilization-validation.md`](scalable-stabilization-validation.md).

Status: implemented and validated behind `high-cardinality-dev`; not yet a
stable public API commitment.

## Implemented

- generated `key_index = disk` declarations and typed indexed reader/writer APIs;
- bounded native stream reader/writer handles with no retained record or key map;
- one redb sidecar for all disk-indexed blocks with configurable global cache;
- composite-key latest put/tombstone lookup with native record/key revalidation;
- OS opened-object identity in the sidecar fingerprint, including an equal-size
  cross-file sidecar-swap rejection test;
- bounded native and sidecar batches derived from the same checked prepared
  records, with no post-append native reread;
- clean reader/writer open from fixed sidecar state with no native record scan;
- atomic temporary-sidecar replacement during explicit rebuild;
- canonical primary paths before writer-lock and sidecar derivation;
- mixed indexed/unindexed block append with sidecar coverage advancement;
- durable dirty state, explicit `sync`, writer poisoning after native publication
  when index publication cannot be completed;
- deterministic drop ordering so active redb transactions abort before the
  database owner closes;
- private-field checked offset/length/snapshot/pointer types before point I/O;
- generated indexed readers with both point lookup and explicit lazy scans.

## Evidence

- default and all-feature workspace suites pass;
- clippy with all targets/features and `-D warnings` passes;
- macro compile-pass/fail contracts pass;
- 10,000-key generated API test covers mixed metadata, overwrite, tombstone,
  reopen and representative equality lookup;
- repeated-key versus unique-key allocator peak comparison passes;
- the release one-million unique composite-key probe records about 245,600
  appends/s, 14.2 us warm equality lookup, 9.35 ms clean reopen, 62 native
  `write_all` calls, a 12,346,781-byte allocator peak delta, and zero Varve
  retained-key entries;
- `cargo audit` reports no known advisory match;
- `cargo deny check` passes advisories, bans, licenses and sources.

## Remaining Before Stabilization

- ordinary open intentionally returns a typed error instead of automatically
  rebuilding; bootstrap/rebuild/restore are explicit operations;
- long scans now expose exact progress and cooperative cancellation;
- process-interruption boundary tests and sidecar input campaigns now pass;
- the current NTFS host passes the 1 TiB sparse-offset smoke but rejects the
  required 1 PiB position, so that platform gate remains open;
- the million-key release probe still needs a clean-host rerun after unrelated
  CPU saturation invalidated the latest comparison;
- semantic sidecar forgery with internally recomputed redb data is outside the
  threat model; malformed databases and sidecars copied from another primary
  object are rejected;
- cross-process indexed reader/writer coordination needs a dedicated fault and
  contention suite before removing the development feature gate.
