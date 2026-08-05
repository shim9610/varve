use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SESSION_PREFIX: &str = "varve-test-session-";
const CREATE_ATTEMPTS: u32 = 128;
const CLEANUP_ATTEMPTS: u32 = 20;
const CLEANUP_RETRY_DELAY: Duration = Duration::from_millis(100);

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("varve test runner failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<ExitCode> {
    let cargo_args: Vec<OsString> = env::args_os().skip(1).collect();
    let cargo_args = if cargo_args.is_empty() {
        vec![
            OsString::from("test"),
            OsString::from("--locked"),
            OsString::from("--workspace"),
            OsString::from("--all-features"),
        ]
    } else {
        cargo_args
    };
    let cargo_args = ensure_locked_resolution(cargo_args);
    reject_recursive_invocation(&cargo_args)?;

    // Resolved before the session directory is made, so a runner invoked
    // outside a workspace fails without leaving one behind.
    let workspace = workspace_root()?;

    let session = TestSession::create()?;
    let session_root = session.path().to_path_buf();
    // The workspace is printed, not merely used: the failure this replaced was
    // silent precisely because nothing said which tree was under test.
    eprintln!("Varve workspace under test: {}", workspace.display());
    eprintln!("Varve test artifacts: {}", session_root.display());

    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let status = Command::new(cargo)
        .args(&cargo_args)
        .current_dir(&workspace)
        .env("TEMP", &session_root)
        .env("TMP", &session_root)
        .env("TMPDIR", &session_root)
        .env("VARVE_TEST_SESSION_ROOT", &session_root)
        .status()?;

    if status.success() {
        session.cleanup()?;
        eprintln!("Varve test artifact cleanup: verified empty");
        Ok(ExitCode::SUCCESS)
    } else {
        match session.preserve() {
            Some(retained) => eprintln!(
                "Varve test artifacts retained after failure: {}",
                retained.display()
            ),
            None => eprintln!(
                "Varve test artifact cleanup: the failed session produced no artifacts, \
                 so nothing was retained"
            ),
        }
        Ok(ExitCode::from(
            status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1),
        ))
    }
}

/// The workspace whose tests this invocation is about, resolved from where the
/// runner was **invoked** rather than from where it was **built**.
///
/// This used to be `env!("CARGO_MANIFEST_DIR")`, and that is a compile-time
/// constant baked into the binary. Two checkouts that share a
/// `CARGO_TARGET_DIR` — a second worktree, a second clone, anyone who sets that
/// variable once in a shell profile — produce the *same* unit hash for this
/// crate, so cargo reuses the artifact instead of rebuilding it. Measured on
/// 2026-08-05 with two worktrees over one target directory: built from tree A,
/// then `cargo build -p varve-test-runner` in tree B reported `Finished` with
/// no `Compiling` line, one artifact existed
/// (`varve_test_runner-1cfa41ebddd3b020`), and the uplifted binary still
/// carried tree A's path. Invoked from tree B it spawned cargo with
/// `PWD=/home/user/rt-A`.
///
/// That is not a slow test run, it is a **green report about a tree nobody
/// asked about**: on 2026-08-05 it ran 50 of 63 targets and reported
/// `EXIT=0, 776 passed, 0 failed` while the tree in front of the operator had a
/// failing test. Nothing in the output named the root it used, so a passing run
/// was indistinguishable from a correct one.
///
/// Walking up from the current directory cannot be stale: there is no cached
/// answer to be wrong. When the runner is invoked from outside any workspace it
/// now refuses, which is the loud failure the old code turned into a quiet one.
fn workspace_root() -> io::Result<PathBuf> {
    workspace_root_from(&env::current_dir()?)
}

/// The [`workspace_root`] walk, over an explicit starting directory so it can
/// be tested without moving the process's own current directory.
fn workspace_root_from(start: &Path) -> io::Result<PathBuf> {
    for directory in start.ancestors() {
        let manifest = directory.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let text = fs::read_to_string(&manifest)?;
        if text
            .lines()
            .any(|line| line.trim_end().trim_start() == "[workspace]")
        {
            return Ok(directory.to_path_buf());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "no Cargo.toml declaring [workspace] at or above {}; \
             run the test runner from inside the workspace",
            start.display()
        ),
    ))
}

/// Make locked dependency resolution the runner's default (REL-01).
///
/// The runner is the documented local completion gate, and CI runs every Cargo
/// command with `--locked`. Without this, the documented no-argument invocation
/// (and any forwarded command that omits the flag) resolves *unlocked*, so a
/// stale `Cargo.lock` is silently refreshed by the very command that is
/// supposed to prove the committed tree builds. The local gate then passes on a
/// lockfile CI has never seen.
///
/// The flag is inserted immediately after the Cargo subcommand, which is where
/// Cargo accepts it, and never after a literal `--` (everything there belongs
/// to the test binary, not to Cargo).
///
/// An explicit resolution mode always wins: `--locked`, `--frozen` (which
/// implies locked), and `--offline` are left exactly as the caller wrote them,
/// so deliberately updating a lockfile stays possible with
/// `cargo run -p varve-test-runner -- test --offline` … or simply by calling
/// `cargo` directly.
fn ensure_locked_resolution(mut args: Vec<OsString>) -> Vec<OsString> {
    let cargo_args_end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let already_specified = args[..cargo_args_end]
        .iter()
        .any(|arg| arg == "--locked" || arg == "--frozen" || arg == "--offline");
    if already_specified {
        return args;
    }
    // The subcommand is the first argument Cargo does not read as a flag.
    let insert_at = args[..cargo_args_end]
        .iter()
        .position(|arg| !arg.to_string_lossy().starts_with('-'))
        .map_or(cargo_args_end, |subcommand| subcommand + 1);
    args.insert(insert_at, OsString::from("--locked"));
    args
}

fn reject_recursive_invocation(args: &[OsString]) -> io::Result<()> {
    let invokes_runner = args
        .windows(2)
        .any(|pair| pair[0] == "-p" && pair[1] == "varve-test-runner");
    if invokes_runner {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing recursive varve-test-runner invocation",
        ));
    }
    Ok(())
}

struct TestSession {
    root: Option<PathBuf>,
}

impl TestSession {
    fn create() -> io::Result<Self> {
        Self::create_in(&env::temp_dir())
    }

    fn create_in(parent: &Path) -> io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self::create_in_with_nonce(parent, nonce)
    }

    fn create_in_with_nonce(parent: &Path, nonce: u128) -> io::Result<Self> {
        fs::create_dir_all(parent)?;
        for attempt in 0..CREATE_ATTEMPTS {
            let root = parent.join(format!(
                "{SESSION_PREFIX}{}-{nonce}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&root) {
                Ok(()) => return Ok(Self { root: Some(root) }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique Varve test session directory",
        ))
    }

    fn path(&self) -> &Path {
        self.root.as_deref().expect("active test session")
    }

    fn cleanup(mut self) -> io::Result<()> {
        let root = self.root.take().expect("active test session");
        cleanup_owned_session(&root)
    }

    /// Gives up ownership of a failed session so it survives for diagnosis.
    ///
    /// F-10: retention exists to preserve *evidence*. A session that produced
    /// no artifacts has none, so an empty one is removed and `None` is
    /// returned rather than leaving an empty `varve-test-session-*` directory
    /// in the user's temp directory forever. That residue was the only way the
    /// documented "a successful test leaves no session path" policy failed to
    /// be literally true: a failed or interrupted run, or a directly executed
    /// test binary in this crate, deposited a zero-byte directory that no
    /// later run ever collects.
    ///
    /// Removal is best effort. If it cannot be proved to have happened the
    /// path is retained, because losing a directory that might hold evidence
    /// is the worse error.
    fn preserve(mut self) -> Option<PathBuf> {
        let root = self.root.take().expect("active test session");
        if is_empty_directory(&root) && cleanup_owned_session(&root).is_ok() {
            return None;
        }
        Some(root)
    }
}

impl Drop for TestSession {
    fn drop(&mut self) {
        let Some(root) = self.root.take() else {
            return;
        };
        // F-10: the same rule as `preserve`, applied to every path that leaves
        // this scope without calling either method - a `?` in `run`, a panic,
        // or a failing assertion in this crate's own unit tests, which create
        // real sessions in the real temp directory.
        if is_empty_directory(&root) && cleanup_owned_session(&root).is_ok() {
            return;
        }
        eprintln!(
            "Varve test artifacts retained because the runner did not complete: {}",
            root.display()
        );
    }
}

/// Whether `root` is a directory that contains nothing at all.
///
/// A read error answers `false`: an unreadable directory is exactly the case
/// where removing it is not justified.
fn is_empty_directory(root: &Path) -> bool {
    match fs::read_dir(root) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => false,
    }
}

fn cleanup_owned_session(root: &Path) -> io::Result<()> {
    validate_owned_session(root)?;
    for attempt in 0..CLEANUP_ATTEMPTS {
        match fs::remove_dir_all(root) {
            Ok(()) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) if attempt + 1 < CLEANUP_ATTEMPTS => {
                let _ = error;
                thread::sleep(CLEANUP_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
    if root.exists() {
        return Err(io::Error::other(format!(
            "test session still exists after cleanup: {}",
            root.display()
        )));
    }
    Ok(())
}

fn validate_owned_session(root: &Path) -> io::Result<()> {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if !name.starts_with(SESSION_PREFIX) || root.parent().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to remove non-session path: {}", root.display()),
        ));
    }
    if let Ok(metadata) = fs::symlink_metadata(root)
        && metadata.file_type().is_symlink()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to remove symlinked session path: {}",
                root.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two workspaces, side by side, and the answer must follow the argument.
    ///
    /// This is the discriminating half. The predecessor was
    /// `env!("CARGO_MANIFEST_DIR")`, a constant: it returns the same path
    /// whatever it is asked about, so it answers `a` for a start inside `b` and
    /// fails here. So does the likeliest wrong repair — caching the first
    /// resolved root in a `OnceLock` — because the second call would return the
    /// first call's answer. Only a resolver that reads its argument passes both
    /// halves.
    #[test]
    fn the_workspace_root_follows_the_starting_directory() -> io::Result<()> {
        let session = TestSession::create()?;
        let a = session.path().join("a");
        let b = session.path().join("b");
        for root in [&a, &b] {
            fs::create_dir_all(root.join("tools/varve-test-runner/src"))?;
            fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n")?;
            // A member manifest with no `[workspace]` table: the walk must pass
            // through it rather than stop at the first Cargo.toml it meets.
            fs::write(
                root.join("tools/varve-test-runner/Cargo.toml"),
                "[package]\nname = \"varve-test-runner\"\n",
            )?;
        }

        assert_eq!(workspace_root_from(&a)?, a);
        assert_eq!(workspace_root_from(&b)?, b);
        assert_eq!(workspace_root_from(&a.join("tools/varve-test-runner"))?, a);
        assert_eq!(
            workspace_root_from(&b.join("tools/varve-test-runner/src"))?,
            b
        );
        session.cleanup()
    }

    /// Outside any workspace the runner refuses instead of guessing, because a
    /// guess is what silently tested the wrong tree.
    #[test]
    fn a_start_outside_any_workspace_is_refused() -> io::Result<()> {
        let session = TestSession::create()?;
        let orphan = session.path().join("orphan");
        fs::create_dir_all(&orphan)?;
        let error = workspace_root_from(&orphan)
            .expect_err("a directory under no workspace manifest must not resolve");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            error.to_string().contains("[workspace]"),
            "the refusal must say what was looked for, got: {error}"
        );
        session.cleanup()
    }

    #[test]
    fn sessions_are_fresh_unique_and_cleanup_is_verified() -> io::Result<()> {
        let first = TestSession::create()?;
        let second = TestSession::create()?;
        assert_ne!(first.path(), second.path());
        assert_eq!(fs::read_dir(first.path())?.count(), 0);
        assert_eq!(fs::read_dir(second.path())?.count(), 0);

        let first_path = first.path().to_path_buf();
        fs::write(first.path().join("created-by-test.bin"), b"test")?;
        first.cleanup()?;
        assert!(!first_path.exists());
        second.cleanup()?;
        Ok(())
    }

    #[test]
    fn failed_session_can_be_preserved_for_diagnosis() -> io::Result<()> {
        let session = TestSession::create()?;
        fs::write(session.path().join("failure.bin"), b"evidence")?;
        let retained = session.preserve().expect("evidence must be retained");
        assert!(retained.join("failure.bin").is_file());
        cleanup_owned_session(&retained)
    }

    /// F-10: an empty session is not evidence, and leaving it behind is the
    /// only way a run that touched nothing can violate the documented
    /// "a successful test leaves no session path" policy.
    #[test]
    fn a_failed_session_that_produced_nothing_is_not_retained() -> io::Result<()> {
        let session = TestSession::create()?;
        let path = session.path().to_path_buf();
        assert!(
            session.preserve().is_none(),
            "an empty session is not evidence"
        );
        assert!(!path.exists(), "the empty session path must be gone");
        Ok(())
    }

    /// The same rule on the path that calls neither `cleanup` nor `preserve`:
    /// an early `?`, a panic, or a directly executed test binary in this
    /// crate.
    #[test]
    fn dropping_an_untouched_session_removes_it() -> io::Result<()> {
        let session = TestSession::create()?;
        let path = session.path().to_path_buf();
        drop(session);
        assert!(!path.exists(), "an empty dropped session must be removed");
        Ok(())
    }

    /// ... but a drop must never destroy artifacts, however it was reached.
    #[test]
    fn dropping_a_session_with_artifacts_retains_it() -> io::Result<()> {
        let session = TestSession::create()?;
        let path = session.path().to_path_buf();
        fs::write(path.join("evidence.bin"), b"evidence")?;
        drop(session);
        assert!(
            path.join("evidence.bin").is_file(),
            "drop must not destroy diagnostic artifacts"
        );
        cleanup_owned_session(&path)
    }

    #[test]
    fn cleanup_refuses_paths_outside_owned_session_namespace() {
        assert!(cleanup_owned_session(Path::new(".")).is_err());
    }

    fn locked(args: &[&str]) -> Vec<String> {
        ensure_locked_resolution(args.iter().map(OsString::from).collect())
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// REL-01: the local completion gate must resolve dependencies exactly the
    /// way CI does, so a stale lockfile fails the command instead of being
    /// silently refreshed by it.
    #[test]
    fn forwarded_commands_default_to_locked_resolution() {
        assert_eq!(
            locked(&["test", "--workspace"]),
            ["test", "--locked", "--workspace"]
        );
        assert_eq!(
            locked(&["test", "-p", "varve", "--test", "roundtrip"]),
            ["test", "--locked", "-p", "varve", "--test", "roundtrip"]
        );
        assert_eq!(
            locked(&["run", "-p", "varve", "--example", "perf_bench"]),
            ["run", "--locked", "-p", "varve", "--example", "perf_bench"]
        );
    }

    /// The flag belongs to Cargo, so it must never land in the arguments the
    /// test binary receives after `--`.
    #[test]
    fn locked_is_inserted_before_the_test_binary_arguments() {
        assert_eq!(
            locked(&["test", "--workspace", "--", "--nocapture"]),
            ["test", "--locked", "--workspace", "--", "--nocapture"]
        );
        // A `--locked` that appears only *after* the separator is an argument
        // of the test binary and does not satisfy Cargo.
        assert_eq!(
            locked(&["test", "--", "--locked"]),
            ["test", "--locked", "--", "--locked"]
        );
    }

    /// An explicit resolution mode is the caller's decision and is preserved.
    #[test]
    fn explicit_resolution_modes_are_left_alone() {
        for explicit in ["--locked", "--frozen", "--offline"] {
            assert_eq!(
                locked(&["test", explicit, "--workspace"]),
                ["test", explicit, "--workspace"],
                "{explicit} must not be overridden"
            );
        }
    }

    #[test]
    fn preexisting_candidate_is_never_reused() -> io::Result<()> {
        let parent = TestSession::create()?;
        let nonce = 7;
        let collision = parent
            .path()
            .join(format!("{SESSION_PREFIX}{}-{nonce}-0", std::process::id()));
        fs::create_dir(&collision)?;
        fs::write(collision.join("must-not-be-overwritten"), b"existing")?;

        let session = TestSession::create_in_with_nonce(parent.path(), nonce)?;
        assert_ne!(session.path(), collision);
        assert_eq!(fs::read_dir(session.path())?.count(), 0);
        assert!(collision.join("must-not-be-overwritten").is_file());

        session.cleanup()?;
        parent.cleanup()
    }
}
