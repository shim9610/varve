//! DUR3-01 regression: a pathname with no directory component.
//!
//! `Path::new("app.varve").parent()` is `Some("")`, not `None`, so the
//! `unwrap_or_else(|| Path::new("."))` fallback in the parent-directory sync
//! was unreachable and the helper opened the empty path instead of the working
//! directory. `sync()` on a file created under a bare name reported
//! `PublishedButParentSyncPending { source: Io(NotFound) }` — a durability
//! failure raised for a file that was never in danger — and the rewrite
//! republication behind `replace` failed the same way after it had already
//! published. The scalable writers hit the same empty parent one step earlier,
//! in the `canonicalize` that resolves their write path.
//!
//! The only pathname whose `parent()` is empty is a single-component relative
//! one, so exercising it means controlling the working directory, and
//! `set_current_dir` is process-global: every other test in this binary runs in
//! the same process and would see the change. The work therefore happens in a
//! re-executed child of this test binary, spawned with `current_dir` set to a
//! temporary directory — the subprocess pattern `scalable_crash_faults` and
//! `policies` already use for process-scoped state.

use std::path::Path;
use std::process::{Command, Stdio};

use varve::{VarveBlock, varve_format};

/// Set only for the child, so the ignored test below stays inert if some
/// invocation runs ignored tests in the parent's working directory.
const CHILD_ENV: &str = "VARVE_BARE_FILENAME_CHILD";

const BARE_NAME: &str = "bare-parent-sync.varve";

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 1, version = 1, kind = "fixed")]
struct Reading {
    at: u64,
    value: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 2, version = 1, kind = "variable")]
struct Note {
    #[varve(field_id = 1)]
    text: String,
}

varve_format! {
    pub struct BareFormat {
        magic: b"BAREPAR";
        version: 1;
        endian: little;
        blocks: [Reading, Note];
    }
}

#[test]
fn a_bare_filename_is_a_usable_pathname() {
    let directory = tempfile::tempdir().expect("create the child working directory");
    let output = Command::new(std::env::current_exe().expect("locate the test executable"))
        .args(["--ignored", "--exact", "bare_filename_child"])
        .env(CHILD_ENV, "1")
        .current_dir(directory.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run the bare-filename child");
    assert!(
        output.status.success(),
        "a bare filename was not a usable pathname: {}\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    // The child worked in its own directory, so the file it created is here
    // and not next to the test binary.
    assert!(
        directory.path().join(BARE_NAME).is_file(),
        "the child reported success without creating the file",
    );
}

#[test]
#[ignore = "invoked only as the bare-filename subprocess"]
fn bare_filename_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }

    // The premise, pinned: `parent()` of a single-component relative path is
    // an empty path, not `None`. That is what defeated the `.` fallback.
    assert_eq!(Path::new(BARE_NAME).parent(), Some(Path::new("")));

    let mut file = BareFormat::create(BARE_NAME).expect("create under a bare filename");
    file.push(&Reading { at: 1, value: 7 })
        .expect("append to a file created under a bare filename");
    file.push(&Note {
        text: "first".to_string(),
    })
    .expect("append a variable record");
    // DUR3-01: the created pathname's parent directory is synced here, once.
    file.sync()
        .expect("sync a file created under a bare filename");
    // The request is cleared on success, so a second call must stay clean.
    file.sync().expect("sync again");

    // The rewrite republication publishes to the same bare pathname and syncs
    // the parent afterwards.
    file.replace(
        0,
        &Note {
            text: "second".to_string(),
        },
    )
    .expect("replace a record in a file opened under a bare filename");
    file.sync().expect("sync after a republication");
    drop(file);

    let reopened = BareFormat::open_readonly(BARE_NAME).expect("reopen under a bare filename");
    assert_eq!(
        reopened
            .blocks::<Reading>()
            .expect("readings")
            .get(0)
            .expect("first reading"),
        Some(Reading { at: 1, value: 7 })
    );
    assert_eq!(
        reopened
            .blocks::<Note>()
            .expect("notes")
            .get(0)
            .expect("first note"),
        Some(Note {
            text: "second".to_string()
        })
    );
    drop(reopened);

    bare_filename_stream();
}

/// The scalable writer resolves its write path with `canonicalize`, which
/// fails on the empty parent one step before any parent sync.
#[cfg(feature = "high-cardinality-dev")]
fn bare_filename_stream() {
    use varve::{StreamOptions, VarveStreamReader, VarveStreamWriter};

    const BARE_STREAM_NAME: &str = "bare-parent-sync-stream.varve";

    let mut writer = VarveStreamWriter::create(
        BareFormat::spec(),
        BARE_STREAM_NAME,
        StreamOptions::default(),
    )
    .expect("create a stream writer under a bare filename");
    writer
        .push_info(&Reading { at: 2, value: 9 })
        .expect("append to the stream writer");
    writer.sync().expect("sync the stream writer");
    drop(writer);

    let reader = VarveStreamReader::open(
        BareFormat::spec(),
        BARE_STREAM_NAME,
        StreamOptions::default(),
    )
    .expect("open a stream reader under a bare filename");
    let readings = reader
        .blocks::<Reading>()
        .expect("stream readings")
        .collect::<varve::Result<Vec<_>>>()
        .expect("collect stream readings");
    assert_eq!(readings, [Reading { at: 2, value: 9 }]);
}

#[cfg(not(feature = "high-cardinality-dev"))]
fn bare_filename_stream() {}
