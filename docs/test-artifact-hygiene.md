# Test Artifact Hygiene

Varve's completion gate includes test-artifact cleanup. A test command is not
successful merely because its assertions pass: its temporary session must also
be removed and verified absent.

## Required Runner

Run the full suite through the workspace test runner:

```text
cargo run -p varve-test-runner
```

With no arguments it runs `cargo test --workspace --all-features`. Arguments
are forwarded to Cargo, so focused and default-feature runs use the same
cleanup contract:

```text
cargo run -p varve-test-runner -- test --workspace
cargo run -p varve-test-runner -- test -p varve --test roundtrip
cargo run -p varve-test-runner -- run -p varve --example perf_bench
```

The runner creates a new `varve-test-session-*` directory with an atomic
`create_dir` operation. It sets `TEMP`, `TMP`, `TMPDIR`, and
`VARVE_TEST_SESSION_ROOT` for the child Cargo process. Consequently,
`std::env::temp_dir()`, `tempfile`, Python temporary-file helpers, and child
processes use the same owned session root.

## Success And Failure

On success the runner recursively removes only the uniquely named session it
created, retries short-lived Windows sharing failures, and verifies that the
path no longer exists. Cleanup failure changes the command to failure.

On test or command failure the runner deliberately retains the session and
prints its absolute path. This preserves the files needed for diagnosis. A
later run never reuses that path.

Process-interruption tests place child artifacts under the inherited session;
the parent test owns their lifecycle.

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
