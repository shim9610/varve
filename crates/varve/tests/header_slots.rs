//! The editable fixed-size header region.
//!
//! What these establish, in order:
//!
//! - `a_declared_region_is_reserved_at_create` — the region exists in a fresh
//!   file, is the declared size, and holds nothing.
//! - `a_written_block_survives_a_reopen` — the write reaches the disk, not just
//!   the cached header.
//! - `rewriting_a_block_does_not_consume_more_of_the_region` — the property
//!   that makes a *fixed* region usable indefinitely.
//! - `the_region_does_not_move_the_append_log` — a header edit does not
//!   invalidate a record offset, which is the whole reason it is reserved
//!   rather than appended.
//! - `a_block_that_does_not_fit_is_refused_before_anything_is_written` — the
//!   refusal is not a half-write.
//! - `an_undeclared_block_is_refused` — the declaration is the permission.
//! - `rolling_integrity_catches_a_corrupted_region` and
//!   `sealed_integrity_refuses_writes_after_the_seal` — the two halves of the
//!   CRC question: verify continuously, or fix the value at a chosen point.
//! - `integrity_none_ignores_a_corrupted_region` — the exemption is real, not
//!   nominal.

use std::io::{Read, Seek, SeekFrom, Write};

use varve::{Error, VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 1, version = 1, kind = "variable")]
struct Sample {
    #[varve(field_id = 1)]
    value: u64,
}

/// The block the region carries: a small piece of metadata a caller wants to
/// revise without republishing the file.
#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 2, version = 1, kind = "variable")]
struct Watermark {
    #[varve(field_id = 1)]
    label: String,
    #[varve(field_id = 2)]
    at: u64,
}

/// Declared in the format but *not* in `header_slots`, so a write of it is
/// refused. Distinct from an unregistered id: this one the format knows.
#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 3, version = 1, kind = "variable")]
struct NotInTheRegion {
    #[varve(field_id = 1)]
    value: u64,
}

varve_format! {
    pub struct Plain {
        magic: b"HSLOTPLN";
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
        blocks: [Sample, Watermark, NotInTheRegion];
    }
}

varve_format! {
    pub struct Exempt {
        magic: b"HSLOTNON";
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
            capacity: 256;
            integrity: none;
            blocks: [Watermark];
        }
        blocks: [Sample, Watermark, NotInTheRegion];
    }
}

varve_format! {
    pub struct Rolling {
        magic: b"HSLOTROL";
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
            capacity: 256;
            integrity: rolling;
            blocks: [Watermark];
        }
        blocks: [Sample, Watermark, NotInTheRegion];
    }
}

varve_format! {
    pub struct Sealing {
        magic: b"HSLOTSEA";
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
            capacity: 256;
            integrity: sealed;
            blocks: [Watermark];
        }
        blocks: [Sample, Watermark, NotInTheRegion];
    }
}

fn watermark(label: &str) -> Watermark {
    Watermark {
        label: label.to_string(),
        at: 42,
    }
}

#[test]
fn a_declared_region_is_reserved_at_create() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    let file = Rolling::create(&path).expect("create");

    assert_eq!(file.header_slots_capacity(), 256);
    assert_eq!(file.header_slots_used().expect("used"), 0);
    assert_eq!(file.header_slots_free_bytes().expect("free"), 256);
    assert_eq!(
        file.read_header_block::<Watermark>().expect("read"),
        None,
        "a fresh region holds nothing"
    );
}

#[test]
fn an_undeclared_region_reports_no_capacity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    let file = Plain::create(&path).expect("create");

    assert_eq!(file.header_slots_capacity(), 0);
    assert_eq!(file.read_header_block::<Watermark>().expect("read"), None);
    assert!(matches!(
        file_write(file, &watermark("x")),
        Err(Error::InvalidFormatSpec(_))
    ));
}

fn file_write(mut file: varve::VarveFile, block: &Watermark) -> varve::Result<()> {
    file.write_header_block(block)
}

#[test]
fn a_written_block_survives_a_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    {
        let mut file = Rolling::create(&path).expect("create");
        file.write_header_block(&watermark("first"))
            .expect("write header block");
        assert_eq!(
            file.read_header_block::<Watermark>().expect("read"),
            Some(watermark("first")),
            "the writer sees its own write"
        );
    }

    let reopened = Rolling::open(&path).expect("open");
    assert_eq!(
        reopened.read_header_block::<Watermark>().expect("read"),
        Some(watermark("first")),
        "the write reached the disk, not only the cached header"
    );
}

#[test]
fn rewriting_a_block_does_not_consume_more_of_the_region() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    let mut file = Rolling::create(&path).expect("create");

    file.write_header_block(&watermark("first")).expect("write");
    let after_first = file.header_slots_used().expect("used");
    assert!(after_first > 0);

    for i in 0..200 {
        file.write_header_block(&watermark("first"))
            .unwrap_or_else(|e| panic!("write {i}: {e}"));
    }

    assert_eq!(
        file.header_slots_used().expect("used"),
        after_first,
        "a rewrite replaces the entry rather than appending one",
    );
    assert_eq!(
        file.read_header_block::<Watermark>().expect("read"),
        Some(watermark("first")),
    );
}

#[test]
fn removing_a_block_frees_its_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    let mut file = Rolling::create(&path).expect("create");

    file.write_header_block(&watermark("first")).expect("write");
    assert!(file.header_slots_used().expect("used") > 0);

    file.remove_header_block::<Watermark>().expect("remove");
    assert_eq!(file.header_slots_used().expect("used"), 0);
    assert_eq!(file.read_header_block::<Watermark>().expect("read"), None);
    file.remove_header_block::<Watermark>()
        .expect("removing what is not there succeeds");
}

#[test]
fn the_region_does_not_move_the_append_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    let mut file = Rolling::create(&path).expect("create");

    file.push(&Sample { value: 7 }).expect("push");
    file.flush().expect("flush");
    let len_before = std::fs::metadata(&path).expect("metadata").len();

    file.write_header_block(&watermark("a long label that occupies real bytes"))
        .expect("write");
    file.write_header_block(&watermark("s")).expect("rewrite");

    assert_eq!(
        std::fs::metadata(&path).expect("metadata").len(),
        len_before,
        "editing a fixed region changes no length",
    );
    let samples = file.blocks::<Sample>().expect("read records");
    assert_eq!(samples.len(), 1);
    assert_eq!(
        samples.get(0).expect("get").expect("a Sample").value,
        7,
        "the record written before the edit still reads back",
    );

    file.push(&Sample { value: 8 }).expect("push after edit");
    file.flush().expect("flush");
    let samples = file.blocks::<Sample>().expect("read records");
    assert_eq!(samples.len(), 2);
    assert_eq!(samples.get(1).expect("get").expect("a Sample").value, 8);
}

#[test]
fn a_block_that_does_not_fit_is_refused_before_anything_is_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    let mut file = Rolling::create(&path).expect("create");

    file.write_header_block(&watermark("keep me"))
        .expect("write");
    let used = file.header_slots_used().expect("used");

    let too_big = Watermark {
        label: "x".repeat(1024),
        at: 1,
    };
    let err = file
        .write_header_block(&too_big)
        .expect_err("must be refused");
    assert!(
        matches!(err, Error::LimitExceeded { .. }),
        "expected LimitExceeded, got {err:?}"
    );

    assert_eq!(
        file.header_slots_used().expect("used"),
        used,
        "a refused write leaves the region as it was",
    );
    assert_eq!(
        file.read_header_block::<Watermark>().expect("read"),
        Some(watermark("keep me")),
    );
}

#[test]
fn an_undeclared_block_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    let mut file = Rolling::create(&path).expect("create");

    let err = file
        .write_header_block(&NotInTheRegion { value: 1 })
        .expect_err("a block the region does not declare must be refused");
    assert!(
        matches!(err, Error::InvalidFormatSpec(_)),
        "expected InvalidFormatSpec, got {err:?}"
    );
}

/// Flips one byte inside the region's entry area.
///
/// The offset is found by searching for the `VHSL` magic rather than computed,
/// which is why no format in this file uses a magic starting with those four
/// bytes — the search would match the format magic at offset 0 and the flip
/// would land in the schema hash instead of the region.
///
/// so this test does not restate the header layout and does not go stale when
/// another extension block is added before it.
fn corrupt_region_byte(path: &std::path::Path) {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .expect("open")
        .read_to_end(&mut bytes)
        .expect("read");
    let at = bytes
        .windows(4)
        .position(|w| w == b"VHSL")
        .expect("the region is in the file")
        // magic + len + version + flags + capacity + used + checksum
        + 4
        + 4
        + 16;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("reopen");
    file.seek(SeekFrom::Start(at as u64)).expect("seek");
    file.write_all(&[bytes[at] ^ 0xFF]).expect("flip");
    file.sync_all().expect("sync");
}

#[test]
fn rolling_integrity_catches_a_corrupted_region() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    {
        let mut file = Rolling::create(&path).expect("create");
        file.write_header_block(&watermark("first")).expect("write");
    }
    corrupt_region_byte(&path);

    let file = Rolling::open(&path).expect("open still succeeds; the region is not the header");
    let err = file
        .read_header_block::<Watermark>()
        .expect_err("a corrupted region must be reported");
    assert!(
        matches!(err, Error::ChecksumMismatch { .. }),
        "expected ChecksumMismatch, got {err:?}"
    );
}

#[test]
fn integrity_none_ignores_a_corrupted_region() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    {
        let mut file = Exempt::create(&path).expect("create");
        file.write_header_block(&watermark("first")).expect("write");
    }
    corrupt_region_byte(&path);

    let file = Exempt::open(&path).expect("open");
    // The exemption is the whole point: the checksum is written but never
    // consulted, so a changed byte is a changed value and not an error. It is
    // still framed, so the walk either finds a block or does not.
    let read = file.read_header_block::<Watermark>();
    assert!(
        read.is_ok(),
        "integrity: none must not verify; got {:?}",
        read.err()
    );
}

#[test]
fn sealed_integrity_refuses_writes_after_the_seal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    let mut file = Sealing::create(&path).expect("create");

    file.write_header_block(&watermark("before"))
        .expect("write");
    assert!(!file.header_slots_sealed().expect("sealed"));

    file.seal_header_slots().expect("seal");
    assert!(file.header_slots_sealed().expect("sealed"));

    let err = file
        .write_header_block(&watermark("after"))
        .expect_err("a sealed region must refuse a write");
    assert!(
        matches!(err, Error::InvalidFormatSpec(_)),
        "expected InvalidFormatSpec, got {err:?}"
    );
    assert_eq!(
        file.read_header_block::<Watermark>().expect("read"),
        Some(watermark("before")),
        "the sealed value is what was there when it was sealed",
    );

    drop(file);
    let reopened = Sealing::open(&path).expect("open");
    assert!(
        reopened.header_slots_sealed().expect("sealed"),
        "the seal is on the disk, not in the handle",
    );
    assert_eq!(
        reopened.read_header_block::<Watermark>().expect("read"),
        Some(watermark("before")),
    );
}

#[test]
fn sealing_is_refused_on_every_other_integrity() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, path) in [
        ("rolling", dir.path().join("r.varve")),
        ("none", dir.path().join("n.varve")),
        ("undeclared", dir.path().join("p.varve")),
    ] {
        let mut file = match name {
            "rolling" => Rolling::create(&path),
            "none" => Exempt::create(&path),
            _ => Plain::create(&path),
        }
        .expect("create");
        let err = file
            .seal_header_slots()
            .expect_err("sealing must be refused when integrity is not `sealed`");
        assert!(
            matches!(err, Error::InvalidFormatSpec(_)),
            "{name}: expected InvalidFormatSpec, got {err:?}"
        );
    }
}

#[test]
fn a_sealed_region_is_verified_on_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    {
        let mut file = Sealing::create(&path).expect("create");
        file.write_header_block(&watermark("first")).expect("write");
        file.seal_header_slots().expect("seal");
    }
    corrupt_region_byte(&path);

    let file = Sealing::open(&path).expect("open");
    let err = file
        .read_header_block::<Watermark>()
        .expect_err("a sealed region must be verified");
    assert!(
        matches!(err, Error::ChecksumMismatch { .. }),
        "expected ChecksumMismatch, got {err:?}"
    );
}

#[test]
fn an_unsealed_region_is_not_verified_under_sealed_integrity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    {
        let mut file = Sealing::create(&path).expect("create");
        file.write_header_block(&watermark("first")).expect("write");
    }
    corrupt_region_byte(&path);

    // Before the seal the checksum is advisory: the region is still being
    // edited, so a mismatch says nothing a caller could act on. This is the
    // difference between `sealed` and `rolling`, stated as a test so that
    // making `sealed` verify early would fail here rather than pass silently.
    let file = Sealing::open(&path).expect("open");
    assert!(file.read_header_block::<Watermark>().is_ok());
}

// A format whose `replace` routes to `replace_block`, which republishes the
// whole generation into a new file rather than patching this one.
varve_format! {
    pub struct Republishing {
        magic: b"HSLOTREP";
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
        index: block_offset_chain;
        header_slots {
            capacity: 256;
            integrity: rolling;
            blocks: [Watermark];
        }
        blocks: [Sample, Watermark, NotInTheRegion];
    }
}

#[test]
fn the_region_survives_a_republishing_replacement() {
    // The route that rebuilds the file from scratch is the one that can drop
    // the region without anything failing to compile: it writes a fresh header
    // for the new generation. `VBTT` is deliberately reset to cold there
    // because every offset it records becomes a lie; this region records no
    // offsets, so it must be carried across verbatim instead.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    let mut file = Republishing::create(&path).expect("create");

    file.write_header_block(&watermark("carried"))
        .expect("write header block");
    file.push(&Sample { value: 1 }).expect("push");
    file.flush().expect("flush");

    file.replace(0, &Sample { value: 99 }).expect("replace");

    assert_eq!(
        file.read_header_block::<Watermark>().expect("read"),
        Some(watermark("carried")),
        "the writer's own handle still sees the region after a republish",
    );

    drop(file);
    let reopened = Republishing::open(&path).expect("open");
    assert_eq!(
        reopened.read_header_block::<Watermark>().expect("read"),
        Some(watermark("carried")),
        "and so does the published generation on disk",
    );
    let samples = reopened.blocks::<Sample>().expect("read records");
    assert_eq!(
        samples.get(0).expect("get").expect("a Sample").value,
        99,
        "the replacement itself landed, so this is a file that really changed",
    );
}

#[test]
fn a_region_written_after_a_replacement_is_still_editable() {
    // The cached header a `replace_block` rebinds is a different `Vec` from the
    // one it opened with. If the extent were computed once and kept, the next
    // write would land at a stale offset; this catches that.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    let mut file = Republishing::create(&path).expect("create");
    file.push(&Sample { value: 1 }).expect("push");
    file.flush().expect("flush");
    file.replace(0, &Sample { value: 99 }).expect("replace");

    file.write_header_block(&watermark("after"))
        .expect("write after a republish");
    drop(file);

    let reopened = Republishing::open(&path).expect("open");
    assert_eq!(
        reopened.read_header_block::<Watermark>().expect("read"),
        Some(watermark("after")),
    );
}

#[test]
fn a_reader_opened_before_a_write_does_not_see_it() {
    // The region is decoded from the header this handle read at open, which is
    // what makes `read_header_block` take `&self` and cost no I/O. The price is
    // stated here rather than left to be discovered: an already-open handle is
    // a snapshot of the region as it was, not a live view of it.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.varve");
    let mut writer = Rolling::create(&path).expect("create");
    writer
        .write_header_block(&watermark("first"))
        .expect("write");
    drop(writer);

    let reader = Rolling::open_readonly(&path).expect("open reader");
    assert_eq!(
        reader.read_header_block::<Watermark>().expect("read"),
        Some(watermark("first")),
    );

    let mut writer = Rolling::open(&path).expect("reopen writer");
    writer
        .write_header_block(&watermark("second"))
        .expect("write");

    assert_eq!(
        reader.read_header_block::<Watermark>().expect("read"),
        Some(watermark("first")),
        "the already-open reader still sees the region as it was at its open",
    );

    drop(reader);
    let fresh = Rolling::open_readonly(&path).expect("open a fresh reader");
    assert_eq!(
        fresh.read_header_block::<Watermark>().expect("read"),
        Some(watermark("second")),
        "a reader opened after the write sees it",
    );
}
