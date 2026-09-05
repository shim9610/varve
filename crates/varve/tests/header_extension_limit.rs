//! The file-header extension ceiling, and the fact that a format declares it.
//!
//! The ceiling was a private constant — 64 KiB — so a `header_slots` region
//! larger than that was not a trade-off the owner could make, it was a refusal
//! with no option behind it. It is a `ReadLimits` field now, which puts it where
//! every other allocation ceiling already is.
//!
//! What these establish, in order:
//!
//! - `the_undeclared_ceiling_is_still_64_kib` — the default is inert: a format
//!   that says nothing reserves, accepts and refuses exactly what it did.
//! - `a_region_over_the_undeclared_ceiling_is_refused` — the negative control
//!   for the test below it. Without this, a passing `a_declared_ceiling_...`
//!   would prove nothing about the declaration.
//! - `a_declared_ceiling_admits_a_larger_region` — the point of the change, end
//!   to end: create, write, reopen, read.
//! - `the_reader_enforces_the_declared_ceiling` — the ceiling is not merely a
//!   create-time assertion; it is the same number on the read path, which is
//!   what stops a hostile header naming a region and having open allocate it
//!   before a single block is parsed.

use varve::{Error, VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 1, version = 1, kind = "variable")]
struct Sample {
    #[varve(field_id = 1)]
    value: u64,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 2, version = 1, kind = "variable")]
struct Wide {
    #[varve(field_id = 1)]
    label: String,
    #[varve(field_id = 2)]
    at: u64,
}

// No `header_extension` key: the ceiling is the library default, and the
// region is sized to sit just under it.
varve_format! {
    pub struct Undeclared {
        magic: b"HEXTUNDC";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;
        header_slots {
            capacity: 65000;
            integrity: rolling;
            blocks: [Wide];
        }
        blocks: [Sample, Wide];
    }
}

// Identical but for the capacity, which is over the undeclared ceiling and is
// *not* accompanied by a declaration. This one must be refused.
varve_format! {
    pub struct OverBudget {
        magic: b"HEXTOVER";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;
        header_slots {
            capacity: 262_144;
            integrity: rolling;
            blocks: [Wide];
        }
        blocks: [Sample, Wide];
    }
}

// The same over-budget capacity, with the ceiling declared to admit it.
varve_format! {
    pub struct Declared {
        magic: b"HEXTDECL";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
            header_extension: 524_288;
        }
        endian: little;
        schema_hash: computed;
        header_slots {
            capacity: 262_144;
            integrity: rolling;
            blocks: [Wide];
        }
        blocks: [Sample, Wide];
    }
}

fn wide(label: &str) -> Wide {
    Wide {
        label: label.to_string(),
        at: 42,
    }
}

#[test]
fn the_undeclared_ceiling_is_still_64_kib() {
    use varve::__core::ReadLimits;
    assert_eq!(
        ReadLimits::MISSING.effective_max_file_header_extension_len(),
        64 * 1024,
        "a format that declares nothing keeps the ceiling it had as a constant",
    );
    assert_eq!(
        ReadLimits::STANDARD.effective_max_file_header_extension_len(),
        64 * 1024,
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("undeclared.varve");
    let mut writer = Undeclared::create(&path).expect("create just under the ceiling");
    writer.write_header_block(&wide("under")).expect("write");
    drop(writer);

    let reader = Undeclared::open_readonly(&path).expect("open");
    assert_eq!(
        reader.read_header_block::<Wide>().expect("read"),
        Some(wide("under")),
    );
}

#[test]
fn a_region_over_the_undeclared_ceiling_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("over.varve");
    match OverBudget::create(&path) {
        Err(Error::InvalidFormatSpec(message)) => assert!(
            message.contains("file-header extension limit"),
            "unexpected refusal: {message}",
        ),
        other => panic!("expected the over-budget region to be refused, got {other:?}"),
    }
}

#[test]
fn a_declared_ceiling_admits_a_larger_region() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("declared.varve");

    let mut writer = Declared::create(&path).expect("create with a declared ceiling");
    writer.push(&Sample { value: 7 }).expect("append");
    writer.write_header_block(&wide("over")).expect("write");
    drop(writer);

    let reader = Declared::open_readonly(&path).expect("reopen");
    assert_eq!(
        reader.read_header_block::<Wide>().expect("read"),
        Some(wide("over")),
        "a region above the old constant round-trips",
    );
}

#[test]
fn the_reader_enforces_the_declared_ceiling() {
    // The same number bounds the read path, so it is a ceiling on what open
    // will allocate for a header it has not parsed yet, not a create-time
    // assertion that a written file could then walk past.
    use varve::__core::ReadLimits;
    assert_eq!(
        ReadLimits::STANDARD
            .with_max_file_header_extension_len(512 * 1024)
            .effective_max_file_header_extension_len(),
        512 * 1024,
    );
    assert_eq!(
        ReadLimits::TRUSTED_UNBOUNDED.effective_max_file_header_extension_len(),
        u64::from(u32::MAX),
        "unbounded means the largest the on-disk u32 length field can name",
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("declared.varve");
    let mut writer = Declared::create(&path).expect("create");
    writer.write_header_block(&wide("over")).expect("write");
    drop(writer);

    // A reader whose ceiling is the default refuses the very file the declaring
    // writer produced: the ceiling travels with the spec, not with the file.
    if Undeclared::open_readonly(&path).is_ok() {
        panic!("a reader with the default ceiling must not accept the larger region");
    }
}
