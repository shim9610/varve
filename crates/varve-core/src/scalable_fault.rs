//! Test-only crash fault injection for scalable persistence paths.
//!
//! The module is compiled only with `scalable-fault-injection`. Setting the
//! environment variables below does nothing until [`arm_from_env`] is called
//! explicitly by a subprocess dedicated to fault testing.

#![cfg(feature = "scalable-fault-injection")]

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

pub const FAULT_ENV: &str = "VARVE_SCALABLE_FAULT";
pub const TRACE_ENV: &str = "VARVE_SCALABLE_FAULT_TRACE";

pub const REQUIRED_POINTS: &[&str] = &[
    "generation.commit",
    "append.native_chunk_write",
    "append.sidecar_batch_commit",
    "sync.native_sync",
    "publish.clean_commit",
    "restore.stage",
    "restore.native_truncate",
    "restore.native_sync",
    "restore.commit",
    "create.native_sync",
    "create.sidecar_complete",
    "replace.atomic",
    "replace.parent_sync",
];

static ARMED: AtomicBool = AtomicBool::new(false);
static REGISTRY: OnceLock<Mutex<Option<Registry>>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    Trace,
    Abort { point: String, occurrence: u64 },
}

struct Registry {
    mode: Mode,
    occurrences: BTreeMap<&'static str, u64>,
    trace: File,
}

/// Keeps one process-local fault configuration armed.
///
/// Dropping the guard disarms all hooks. The guard is intentionally neither
/// cloneable nor transferable between processes.
#[derive(Debug)]
#[must_use = "dropping the guard disarms scalable fault injection"]
pub struct ArmGuard {
    _private: (),
}

#[derive(Debug)]
pub enum ArmError {
    AlreadyArmed,
    MissingVariable(&'static str),
    NonUnicodeVariable(&'static str),
    InvalidConfiguration(String),
    UnknownPoint(String),
    TraceIo { path: PathBuf, source: io::Error },
}

impl fmt::Display for ArmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyArmed => formatter.write_str("scalable fault injection is already armed"),
            Self::MissingVariable(name) => write!(formatter, "missing environment variable {name}"),
            Self::NonUnicodeVariable(name) => {
                write!(
                    formatter,
                    "environment variable {name} is not valid Unicode"
                )
            }
            Self::InvalidConfiguration(value) => write!(
                formatter,
                "invalid {FAULT_ENV} value {value:?}; expected trace or abort:<point>:<occurrence>"
            ),
            Self::UnknownPoint(point) => {
                write!(formatter, "unknown scalable fault point {point:?}")
            }
            Self::TraceIo { path, source } => {
                write!(
                    formatter,
                    "cannot open fault trace {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ArmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TraceIo { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Explicitly arms this process from the hidden subprocess-test environment.
///
/// `VARVE_SCALABLE_FAULT=trace` records all hook events. An abort run uses
/// `abort:<point>:<occurrence>`, where occurrences are one-based per point.
/// The hook immediately before a persistence primitive and the hook
/// immediately after it are distinct consecutive occurrences.
#[doc(hidden)]
pub fn arm_from_env() -> Result<ArmGuard, ArmError> {
    let configuration = env_value(FAULT_ENV)?;
    let trace_path = PathBuf::from(env_value(TRACE_ENV)?);
    let mode = parse_mode(&configuration)?;
    let trace = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&trace_path)
        .map_err(|source| ArmError::TraceIo {
            path: trace_path,
            source,
        })?;

    let registry = REGISTRY.get_or_init(|| Mutex::new(None));
    let mut slot = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_some() {
        return Err(ArmError::AlreadyArmed);
    }
    *slot = Some(Registry {
        mode,
        occurrences: BTreeMap::new(),
        trace,
    });
    ARMED.store(true, Ordering::Release);
    Ok(ArmGuard { _private: () })
}

/// Records one named boundary and aborts when the armed selection matches it.
///
/// Main-agent integration calls this immediately before and immediately after
/// each persistence primitive named in [`REQUIRED_POINTS`]. Calls are inert
/// until the child process explicitly invokes [`arm_from_env`].
#[doc(hidden)]
pub fn fault_point(point: &'static str) {
    if !ARMED.load(Ordering::Acquire) {
        return;
    }
    if !REQUIRED_POINTS.contains(&point) {
        panic!("unregistered scalable fault point {point:?}");
    }

    let registry = REGISTRY
        .get()
        .expect("armed scalable fault registry was not initialized");
    let mut slot = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(registry) = slot.as_mut() else {
        return;
    };
    let occurrence = registry.occurrences.entry(point).or_insert(0);
    *occurrence = occurrence
        .checked_add(1)
        .expect("scalable fault occurrence counter overflowed");
    let occurrence = *occurrence;

    writeln!(registry.trace, "v1\t{point}\t{occurrence}")
        .and_then(|()| registry.trace.sync_data())
        .unwrap_or_else(|error| panic!("cannot persist scalable fault trace: {error}"));

    if matches!(
        &registry.mode,
        Mode::Abort {
            point: selected,
            occurrence: selected_occurrence,
        } if selected == point && *selected_occurrence == occurrence
    ) {
        std::process::abort();
    }
}

impl Drop for ArmGuard {
    fn drop(&mut self) {
        if let Some(registry) = REGISTRY.get() {
            let mut slot = registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = None;
        }
        ARMED.store(false, Ordering::Release);
    }
}

fn env_value(name: &'static str) -> Result<String, ArmError> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Err(ArmError::MissingVariable(name)),
        Err(env::VarError::NotUnicode(_)) => Err(ArmError::NonUnicodeVariable(name)),
    }
}

fn parse_mode(value: &str) -> Result<Mode, ArmError> {
    if value == "trace" {
        return Ok(Mode::Trace);
    }
    let Some(selection) = value.strip_prefix("abort:") else {
        return Err(ArmError::InvalidConfiguration(value.to_owned()));
    };
    let Some((point, occurrence)) = selection.rsplit_once(':') else {
        return Err(ArmError::InvalidConfiguration(value.to_owned()));
    };
    if !REQUIRED_POINTS.contains(&point) {
        return Err(ArmError::UnknownPoint(point.to_owned()));
    }
    let occurrence = occurrence
        .parse::<u64>()
        .ok()
        .filter(|occurrence| *occurrence != 0)
        .ok_or_else(|| ArmError::InvalidConfiguration(value.to_owned()))?;
    Ok(Mode::Abort {
        point: point.to_owned(),
        occurrence,
    })
}
