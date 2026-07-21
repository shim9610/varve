# Test Artifact Hygiene

Varve's completion gate includes test-artifact cleanup. A test command is not
successful merely because its assertions pass: its temporary session must also
be removed and verified absent.

## Required Runner

Run the full suite through the workspace test runner:

```text
cargo run -p varve-test-runner
```

With no arguments it runs `cargo test --locked --workspace --all-features`.
Arguments are forwarded to Cargo, so focused and default-feature runs use the
same cleanup contract:

```text
cargo run -p varve-test-runner -- test --workspace
cargo run -p varve-test-runner -- test -p varve --test roundtrip
cargo run -p varve-test-runner -- run -p varve --example perf_bench
```

Locked resolution is the default (REL-01). The runner inserts `--locked` after
the Cargo subcommand — never after a literal `--`, where the arguments belong to
the test binary — so the local completion gate resolves dependencies exactly the
way CI does. Without it a stale `Cargo.lock` was silently refreshed by the very
command meant to prove the committed tree builds, and the local gate then passed
on a lockfile CI had never seen. An explicit `--locked`, `--frozen`, or
`--offline` from the caller is preserved, so deliberately updating a lockfile
remains possible.

The runner creates a new `varve-test-session-*` directory with an atomic
`create_dir` operation. It sets `TEMP`, `TMP`, `TMPDIR`, and
`VARVE_TEST_SESSION_ROOT` for the child Cargo process. Consequently,
`std::env::temp_dir()`, `tempfile`, Python temporary-file helpers, and child
processes use the same owned session root.

CI runs the default-feature and all-feature test suites through this runner, plus
the two singleton feature configurations that own `cfg`-exclusive behaviour
(compression without integrity, integrity without compression) and the pinned
MSRV job (`.github/workflows/ci.yml`), so an artifact leaked past the session
directory fails the build.

## Success And Failure

On success the runner recursively removes only the uniquely named session it
created, retries short-lived Windows sharing failures, and verifies that the
path no longer exists. Cleanup failure changes the command to failure.

On test or command failure the runner deliberately retains the session and
prints its absolute path. This preserves the files needed for diagnosis. A
later run never reuses that path.

Retention preserves *evidence*, so a failed session that produced no files at
all is removed instead of retained, and the runner says so rather than printing
a path to an empty directory (F-10). The same rule applies on every path that
leaves the runner without reaching either outcome — an early I/O error, a
panic, or a directly executed test binary in `tools/varve-test-runner`, which
creates real sessions in the real temp directory: dropping an untouched session
removes it, dropping one that holds artifacts keeps it. Removal is best effort;
if it cannot be proved to have happened, the path is retained and printed,
because losing a directory that might hold evidence is the worse error.

The policy this makes literally true is "a run that produced nothing leaves no
session path". Before it, a failed or interrupted run could deposit a zero-byte
`varve-test-session-*` directory that no later run ever collected.

Process-interruption tests place child artifacts under the inherited session;
the parent test owns their lifecycle.

## Unattended Process-Boundary Tests

A test that deliberately crashes a child process must also leave no operating
system dialog behind, or the suite stops being unattended. On Windows an
aborted child is handed to Windows Error Reporting, which starts `WerFault.exe`
and may show an error box. The crash child in
`crates/varve/tests/scalable_crash_faults.rs` suppresses that at its own entry
point, before it induces the fault (see
[`fuzzing-and-fault-injection.md`](fuzzing-and-fault-injection.md)). Any new
test that crashes a child must do the same; suppression in the parent process
does not propagate.

## Per-Test Owned Directories

The runner is the outer safety net, not the only one. Each integration test
owns its scratch directory directly: path helpers (`temp_path`, `TempMatrix`,
crash-test roots) create a fresh `tempfile::tempdir()` and return a guard that
keeps the directory alive for the test's lifetime. Dropping the guard removes
the directory recursively, so the native file, any sidecar, and the empty
`.lock` writer marker are collected together — including on assertion failure
and panic, and even when the suite is run with plain `cargo test` outside the
runner.

Empty `.lock` markers are intentional in production: they persist beside a
native file to keep the single-writer claim race-free, and Varve never deletes
a marker it cannot prove it owns. Test cleanup is therefore a directory-scope
concern, never a marker-scope one. Scavenging of crash-orphaned temp files on
production paths is deliberately not implemented; if added, it must be a
separate opt-in API that verifies age, PID, exclusive object ownership, and
name format before touching anything.

## File-Creation Tests

Fresh creation remains part of the test. Each session starts as a newly created
empty directory, and a test chooses a nonexistent filename inside it before it
calls `Format::create` or another creation API. There is no preexisting file to
silently accept or overwrite.

Tests for existing-file behavior are explicit exceptions: they first create a
fixture and then assert the requested refusal, append, recovery, replacement,
or truncation semantics. They still run inside the unique session.

## Build Cache

Cargo's `target` directories are build caches, not test data. They are retained
between development runs because deleting them after every suite forces full
recompilation and causes more SSD writes. Test-session cleanup never removes a
`target` directory.

Use `cargo clean` for the root workspace and
`cargo clean --manifest-path fuzz/Cargo.toml` for the independent fuzz
workspace when reclaiming build-cache space is intentional. The final release
cleanup can remove these caches after verification; routine runs should reuse
them.

## Fuzz Scratch Files

Fuzz targets use an RAII `FuzzFile` guard. Normal iterations remove both the
scratch input and its writer-lock marker. Reproducer inputs remain in the fuzz
corpus or artifact directory rather than relying on an untracked temporary
file.
