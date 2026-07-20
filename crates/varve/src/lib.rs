#![forbid(unsafe_op_in_unsafe_fn)]
//! Format-first, append-friendly custom binary formats for Rust.
//!
//! Varve turns a *declaration* of a binary format into the format: you write
//! the header identity, resource limits, durability and integrity policies, and
//! the block set once, and the macro generates the on-disk contract together
//! with the typed Rust API that reads and writes it. Files are append-friendly
//! — records are added at the tail, readers take a snapshot at open — with an
//! optional preallocated matrix storage mode for dense, directly addressed
//! cells.
//!
//! The README is deliberately not inlined here: it is packaged alongside this
//! crate, but its path differs between the repository checkout and the
//! published `.crate`, so `include_str!` would break one of the two.
//!
//! # Crate layout
//!
//! `varve` is a facade. It re-exports the entire runtime from
//! [`varve_core`](https://docs.rs/varve-core) and the two procedural macros
//! from [`varve_macros`](https://docs.rs/varve-macros). Depend on `varve`
//! alone; the other two crates are implementation packages and their direct
//! use is not a supported configuration.
//!
//! | Entry point | Purpose |
//! | --- | --- |
//! | [`varve_format!`] | declare a whole format: header identity, limits, policies, blocks, and the typed API generated from them |
//! | [`VarveBlock`] (derive) | declare one stored record type outside a format declaration |
//! | [`FormatSpec`] | the resolved, immutable format contract a handle is opened against |
//! | [`VarveFile`], [`VarveReader`], [`VarveWriter`] | untyped handles; the generated wrappers sit on top of these |
//! | [`VarveEncode`] / [`VarveDecode`] | field and block payload codecs, including custom ones |
//!
//! # Getting started
//!
//! ```
//! use varve::varve_format;
//!
//! varve_format! {
//!     pub format AppFormat {
//!         magic: b"APPF";
//!         version: 1;
//!         endian: little;
//!         schema_hash: computed;
//!         manifest: embedded;
//!         index: [scan_on_open, block_offset_chain, keyed_offset_chain];
//!         commit: transaction_marker(on_flush);
//!
//!         blocks {
//!             fixed Point(id = 1) {
//!                 x: u32,
//!                 y: u32,
//!             }
//!
//!             variable User(id = 2, key = [id]) {
//!                 id: u64,
//!                 name: String,
//!             }
//!         }
//!     }
//! }
//!
//! # fn main() -> varve::Result<()> {
//! # let dir = std::env::temp_dir().join(format!("varve-doctest-{}", std::process::id()));
//! # std::fs::create_dir_all(&dir).unwrap();
//! # let path = dir.join("app.appf");
//! {
//!     let mut writer = AppFormat::create_writer(&path)?;
//!     writer.push_point(&Point { x: 1, y: 2 })?;
//!     writer.push_user(&User { id: 7, name: "Ada".to_string() })?;
//!     writer.flush()?;
//! }
//!
//! let reader = AppFormat::open_reader(&path)?;
//! let points: Vec<Point> = reader.points()?.iter().collect::<varve::Result<_>>()?;
//! assert_eq!(points, vec![Point { x: 1, y: 2 }]);
//! assert_eq!(reader.users()?.get(&7)?.unwrap().name, "Ada");
//! # std::fs::remove_dir_all(&dir).unwrap();
//! # Ok(())
//! # }
//! ```
//!
//! Before wiring a new format into application logic, run
//! [`FormatSelfTest`]: it exercises the generated APIs, codecs, keyed lookup,
//! and a file round-trip for representative values, and reports which layer to
//! look at when something disagrees.
//!
//! # Cargo features
//!
//! All features are off by default; the default build is the pure-Rust core
//! with no optional dependencies.
//!
//! | Feature | Adds | Stability |
//! | --- | --- | --- |
//! | `integrity` | CRC32 record integrity, matrix and sidecar CRC evidence | stable |
//! | `compression-zstd` | zstd compression of variable-block payloads and of [`ChunkedBytes`] | stable |
//! | `mmap` | read-only memory-mapped payload and matrix windows | stable, `unsafe` entry points |
//! | `zero-copy` | raw typed views over mapped bytes; implies `mmap` | stable, `unsafe` entry points |
//! | `high-cardinality-dev` | the disk-backed index, streaming, and indexed handles | **experimental**: the surface and the sidecar layout may change without a major version |
//! | `scalable-fault-injection` | fault-injection counters and hooks used by the scalable-path tests; implies `high-cardinality-dev` | **test infrastructure**: not a production feature, and the counters it exposes are `#[doc(hidden)]` |
//!
//! Requesting a feature-gated operation without its feature is a typed error
//! ([`Error::CompressionFeatureDisabled`], [`Error::IntegrityFeatureDisabled`])
//! rather than a silent fallback.
//!
//! # Guides
//!
//! The repository carries the long-form documentation; these are the entry
//! points, under `docs/` at
//! <https://github.com/shim9610/varve>:
//!
//! | Guide | Covers |
//! | --- | --- |
//! | `quickstart.md` | shortest path from declaration to a file |
//! | `declaration-and-internals.md` | the full `varve_format!` grammar and what it generates |
//! | `api-reference.md` | the complete public surface, feature by feature |
//! | `custom-codec-guide.md` | writing a codec, and the mandatory `SCHEMA_ID` identity contract |
//! | `durability-model.md` | what `flush`, `sync`, and `commit_durable` promise |
//! | `recovery-model.md` | how damaged files are classified and what may be rebuilt |
//! | `matrix-final-spec.md` | the preallocated matrix storage mode |
//! | `performance.md` | the scale contracts and their measured bounds |
//! | `migration-guide.md` | version-to-version behaviour changes |
//!
//! [`ChunkedBytes`]: crate::ChunkedBytes
//! [`Error::CompressionFeatureDisabled`]: crate::Error::CompressionFeatureDisabled
//! [`Error::IntegrityFeatureDisabled`]: crate::Error::IntegrityFeatureDisabled
//! [`FormatSelfTest`]: crate::FormatSelfTest
//! [`FormatSpec`]: crate::FormatSpec
//! [`VarveDecode`]: crate::VarveDecode
//! [`VarveEncode`]: crate::VarveEncode
//! [`VarveFile`]: crate::VarveFile
//! [`VarveReader`]: crate::VarveReader
//! [`VarveWriter`]: crate::VarveWriter

pub use varve_core::*;
pub use varve_macros::{VarveBlock, varve_format};

#[doc(hidden)]
pub mod __core {
    pub use varve_core::*;
}
