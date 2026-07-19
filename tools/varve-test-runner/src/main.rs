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
            OsString::from("--workspace"),
            OsString::from("--all-features"),
        ]
    } else {
        cargo_args
    };
    reject_recursive_invocation(&cargo_args)?;

    let session = TestSession::create()?;
    let session_root = session.path().to_path_buf();
    eprintln!("Varve test artifacts: {}", session_root.display());

    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let status = Command::new(cargo)
        .args(&cargo_args)
        .current_dir(workspace_root())
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
        let retained = session.preserve();
        eprintln!(
            "Varve test artifacts retained after failure: {}",
            retained.display()
        );
        Ok(ExitCode::from(
            status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1),
        ))
    }
}

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("test runner must live under tools/<crate>")
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

    fn preserve(mut self) -> PathBuf {
        self.root.take().expect("active test session")
    }
}

impl Drop for TestSession {
    fn drop(&mut self) {
        if let Some(root) = &self.root {
            eprintln!(
                "Varve test artifacts retained because the runner did not complete: {}",
                root.display()
            );
        }
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
        let retained = session.preserve();
        assert!(retained.join("failure.bin").is_file());
        cleanup_owned_session(&retained)
    }

    #[test]
    fn cleanup_refuses_paths_outside_owned_session_namespace() {
        assert!(cleanup_owned_session(Path::new(".")).is_err());
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
