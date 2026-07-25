# Varve — working instructions

## Push manual

Two remotes, and they are not interchangeable.

| Remote | URL | Visibility | Gets |
| --- | --- | --- | --- |
| `origin` | `github.com/shim9610/varve` | **public** | product only: source, tests, the 14 published docs, README, CHANGELOG |
| `dev` | `github.com/shim9610/varve-dev` | **private** | everything, including the internal working documents |

**Default: push to `dev`. Pushing to `origin` needs the owner to ask for it in
that turn.** "Push it" without a named remote means `dev`.

### Before any push to `origin`

1. Show the owner the file list that would go out and wait for approval. Never
   `git add docs/` or `git add -A` and push on that basis — that is exactly how
   66 internal documents were published on 2026-07-25.
2. Confirm `git diff --cached --name-only | grep internal-docs` is empty.
   `.gitignore` does **not** untrack an already-tracked file, so a `git mv` into
   `.internal-docs/` stages a *rename* and would republish the file at its new
   path. Untrack with `git rm --cached` when that happens.
3. Confirm nothing published references a file that is not published:
   sweep every basename under `.internal-docs/` against `docs/`, `README.md`,
   `CHANGELOG.md`, `crates/`, `tools/` and `.github/`, excluding `target/`.

### Branches

- `main` — the release line, on both remotes.
- `codex/dev-next` — the working branch, on both remotes.
- `internal-docs` — **`dev` only.** Never push it to `origin`. It is the one
  branch where `/.internal-docs/` is un-ignored, so the internal documents are
  tracked there and nowhere else. On every other branch that directory is
  gitignored and its files are absent from the working tree, which means they
  cannot be staged by accident rather than merely should not be.

To read or edit an internal document: `git checkout internal-docs`, work, commit,
`git push dev internal-docs`, then `git checkout codex/dev-next`.

## What is published

`docs/` holds 14 files: explanation and usage only. Anything that is process —
review reports, per-worker notes, the invariant checklist, routing/spec/
validation artifacts, design notes for unshipped features — belongs on
`internal-docs`. When adding a document, decide which it is before writing it.

## CI

`.github/workflows/ci.yml` runs on push to `main`, on pull requests, and by hand.
Every job carries `if: github.repository == 'shim9610/varve'` because GitHub-hosted
runners are free for public repositories but metered for private ones, and this
file exists in a tree that is pushed to both remotes. Do not remove those guards.
If the public repository ever goes private, remove the automatic triggers instead.

## Design policies

These are the owner's standing requirements. They override any design a review or
an agent proposes, and they are not trade-offs to balance:

- **All reads take `&self`.** No read entry point may require `&mut self`; one
  handle serves concurrent readers. Positional reads
  (`SnapshotFile::read_exact_at`) are the mechanism.
- **TB-scale files must work.** Open reads a header, pages fault in on demand,
  memory is bounded by the working set — never by total file content. An eager
  load at open is not acceptable.
- **The append hot path is sacred.** Continuous high-rate streaming append is the
  primary workload: no per-record syscall, no per-record allocation, no new
  O(N) per-operation work.
- **Capabilities are options, never decisions handed back to the owner.**
  Document "want capability X → enable option Y on block Z → costs W". The DSL
  already works this way (`index: [...]`, `IndexPolicy`, per-block attributes).
  Presenting an engineering trade-off as a blocking question is wrong.
- **Defaults are inert.** A new option defaults off, and all-off is byte-identical
  to the previous behaviour. A user who does not opt in pays nothing.

## Reporting

Say what was measured, not what was intended. This project's recurring failure is
work reported as complete when only part of it was built — a "paged and lazy"
requirement shipped twice with only the paged half. If a criterion was not
measured, say so; if half was achieved, say which half.

## Local gates

CI is not the gate for uncommitted work. Before reporting anything green:

```
cargo run -p varve-test-runner -- test --workspace --all-features
cargo clippy --workspace --all-features --all-targets -- -D warnings
```

Cap parallelism — `CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=4`, one cargo command at
a time, never backgrounded. This machine has kernel-panicked under heavy build
load. Matrix tests with near-unbounded configured limits have repeatedly grown
temp files to many GiB when a fixture offset went stale; kill a long matrix test
rather than waiting for it.
