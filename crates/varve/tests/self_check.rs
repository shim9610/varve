use std::fs::{remove_file, write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use varve::{
    DiagnosticDomain, DiagnosticSeverity, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
    ManifestPolicy, MatrixAuxDescriptor, MatrixBlockDescriptor, MatrixCommitDescriptor,
    MatrixCommitKind, MatrixDimensionDescriptor, MatrixDimensions, MatrixKey, ReadLimits,
    RecoveryPolicy, SelfTestStepStatus, VarveBlock, VarveDecode, VarveEncode, VarveMatrixBlock,
    WireType, varve_format,
};

varve_format! {
    pub format SelfCheckFormat {
        magic: b"SCHK";
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
        manifest: embedded;
        index: [scan_on_open, block_offset_chain, keyed_offset_chain];

        blocks {
            fixed Point(id = 700) {
                x: u32,
                y: u32,
            }

            variable User(id = 701, key = [id]) {
                id: u64,
                name: String,
                flags: u32 = default,
            }

            variable Trace(id = 702) {
                id: u64,
                samples: Vec<u64>,
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct MatrixCell {
    value: u32,
}

impl VarveEncode for MatrixCell {
    const WIRE_TYPE: WireType = WireType::Bytes;

    fn encode_varve(&self, encoder: &mut varve::Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for MatrixCell {
    const WIRE_TYPE: WireType = WireType::Bytes;

    fn decode_varve(decoder: &mut varve::Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u32::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for MatrixCell {
    const ID: u32 = 710;
    const VERSION: u16 = 1;
    const KIND: varve::BlockKind = varve::BlockKind::Matrix;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x80696EFA53043EA2;
    const IS_KEYED: bool = false;
}

impl varve::VarveMatrixBlock for MatrixCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

fn matrix_spec() -> FormatSpec {
    static BLOCKS: &[varve::BlockDescriptor] = &[varve::BlockDescriptor {
        id: MatrixCell::ID,
        name: "MatrixCell",
        version: MatrixCell::VERSION,
        kind: varve::BlockKind::Matrix,
        fields: MatrixCell::FIELDS,
    }];
    static DIMS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: MatrixCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: MatrixCell::ID,
        dimensions: MatrixCell::DIMENSIONS,
        category: MatrixCell::CATEGORY,
        slot_stride: MatrixCell::SLOT_STRIDE,
    }];
    static AUX: &[MatrixAuxDescriptor] = &[MatrixAuxDescriptor {
        name: "scratch",
        byte_len: 16,
    }];
    FormatSpec::new(
        b"MSCHK",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_matrix_spec(DIMS, COMMITS, MATRIX_BLOCKS)
    .with_matrix_aux(AUX)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

#[test]
fn generated_format_self_test_roundtrips_append_and_keyed_blocks() {
    let path = temp_path("append");
    cleanup(&path);

    let report = SelfCheckFormat::self_test(&path)
        .with_block(Point { x: 1, y: 2 })
        .with_keyed_block(User {
            id: 42,
            name: "Ada".to_string(),
            flags: 7,
        })
        .cleanup(true)
        .run();

    assert!(report.passed(), "{report:#?}");
    assert!(
        report
            .steps
            .iter()
            .any(|step| step.name.contains("write block 700"))
    );
    cleanup(&path);
}

#[test]
fn matrix_self_test_roundtrips_committed_uncommitted_and_aux_paths() {
    let path = temp_path("matrix");
    cleanup(&path);

    let report = matrix_spec()
        .self_test(&path)
        .with_dims(MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]))
        .with_matrix_cell(MatrixKey::new(0, 0), MatrixCell { value: 9 })
        .with_uncommitted_matrix_cell(MatrixKey::new(1, 1), MatrixCell { value: 11 })
        .with_matrix_aux("scratch", 4, [1, 2, 3, 4])
        .cleanup(true)
        .run();

    assert!(report.passed(), "{report:#?}");
    cleanup(&path);
}

#[test]
fn self_test_never_truncates_or_deletes_pre_existing_append_target() {
    let path = temp_path("preexisting_append");
    cleanup(&path);
    let sentinel: &[u8] = b"caller data that must survive the self-test";
    write(&path, sentinel).expect("write sentinel fixture");

    let report = SelfCheckFormat::self_test(&path)
        .with_block(Point { x: 1, y: 2 })
        .cleanup(true)
        .run();

    assert!(!report.passed(), "{report:#?}");
    let failure = report.failures().next().expect("expected a create failure");
    assert_eq!(failure.name, "create file");
    assert_eq!(failure.domain, Some(DiagnosticDomain::CallerUsage));
    assert!(failure.message.contains("already exists"), "{failure:#?}");

    let bytes =
        std::fs::read(&path).expect("pre-existing file must still exist after cleanup(true)");
    assert_eq!(bytes, sentinel, "pre-existing bytes must be untouched");
    cleanup(&path);
}

#[test]
fn self_test_never_truncates_or_deletes_pre_existing_matrix_target() {
    let path = temp_path("preexisting_matrix");
    cleanup(&path);
    let sentinel: &[u8] = b"matrix caller data that must survive";
    write(&path, sentinel).expect("write sentinel fixture");

    let report = matrix_spec()
        .self_test(&path)
        .with_dims(MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]))
        .with_matrix_cell(MatrixKey::new(0, 0), MatrixCell { value: 9 })
        .cleanup(true)
        .run();

    assert!(!report.passed(), "{report:#?}");
    // API2-02: the exclusive create is itself the claim, so the pre-existing
    // target is rejected in the single "create matrix file" step instead of a
    // separate pathname claim that a concurrent swap could race.
    let failure = report.failures().next().expect("expected a create failure");
    assert_eq!(failure.name, "create matrix file");
    assert_eq!(failure.domain, Some(DiagnosticDomain::CallerUsage));
    assert!(failure.message.contains("already exists"), "{failure:#?}");

    let bytes =
        std::fs::read(&path).expect("pre-existing file must still exist after cleanup(true)");
    assert_eq!(bytes, sentinel, "pre-existing bytes must be untouched");
    assert!(
        !lock_marker(&path).exists(),
        "the failed claim must tidy the marker it created"
    );
    cleanup(&path);
}

#[test]
fn self_test_cleanup_removes_files_created_by_the_run() {
    let path = temp_path("cleanup_owned");
    cleanup(&path);

    let report = SelfCheckFormat::self_test(&path)
        .with_block(Point { x: 3, y: 4 })
        .cleanup(true)
        .run();

    assert!(report.passed(), "{report:#?}");
    assert!(
        !path.exists(),
        "cleanup(true) must remove the file this run created"
    );

    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    assert!(
        !PathBuf::from(lock).exists(),
        "cleanup(true) must remove the lock marker this run created"
    );
}

#[test]
fn matrix_self_test_reports_missing_dims_as_caller_usage() {
    let path = temp_path("missing_dims");
    cleanup(&path);

    let report = matrix_spec()
        .self_test(&path)
        .with_matrix_cell(MatrixKey::new(0, 0), MatrixCell { value: 9 })
        .cleanup(true)
        .run();

    let failure = report.failures().next().expect("failure");
    assert_eq!(failure.status, SelfTestStepStatus::Failed);
    assert_eq!(failure.domain, Some(DiagnosticDomain::CallerUsage));
    assert!(failure.message.contains("matrix dimensions"));
    cleanup(&path);
}

#[test]
fn diagnostics_classify_file_mismatch_and_bad_format_spec() {
    let path = temp_path("bad_file");
    cleanup(&path);
    write(&path, b"not a varve file").expect("write corrupt fixture");

    let report = SelfCheckFormat::diagnose_file(&path);
    assert!(!report.passed());
    assert!(report.items.iter().any(|item| {
        item.severity == DiagnosticSeverity::Error
            && item.domain == DiagnosticDomain::FileData
            && item.code == "file.open_readonly.failed"
    }));

    static DUP_BLOCKS: &[varve::BlockDescriptor] = &[
        varve::BlockDescriptor {
            id: 1,
            name: "A",
            version: 1,
            kind: varve::BlockKind::Fixed,
            fields: &[],
        },
        varve::BlockDescriptor {
            id: 1,
            name: "B",
            version: 1,
            kind: varve::BlockKind::Fixed,
            fields: &[],
        },
    ];
    let bad_spec = FormatSpec::new(
        b"BAD",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        DUP_BLOCKS,
    );
    let spec_report = bad_spec.diagnostics();
    assert!(!spec_report.passed());
    assert!(spec_report.items.iter().any(|item| {
        item.domain == DiagnosticDomain::FormatDefinition
            && item.code == "format.spec.invalid"
            && item.message.contains("duplicate block id")
    }));

    assert_eq!(
        varve::classify_error(&Error::CompressionFeatureDisabled),
        DiagnosticDomain::FeatureGate
    );
    cleanup(&path);
}

/// When set, the next [`SwapProbe`] decode replaces the file at the stored
/// pathname with [`IMPOSTOR_BYTES`], modeling a concurrent process swapping
/// the pathname while the self-test still holds the original object open.
static SWAP_ON_DECODE: Mutex<Option<PathBuf>> = Mutex::new(None);

const IMPOSTOR_BYTES: &[u8] = b"impostor file swapped in behind the self-test";

#[derive(Clone, Debug, PartialEq)]
struct SwapProbe {
    value: u32,
}

impl VarveEncode for SwapProbe {
    const WIRE_TYPE: WireType = WireType::Bytes;

    fn encode_varve(&self, encoder: &mut varve::Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for SwapProbe {
    const WIRE_TYPE: WireType = WireType::Bytes;

    fn decode_varve(decoder: &mut varve::Decoder<'_>) -> varve::Result<Self> {
        if let Some(target) = SWAP_ON_DECODE.lock().unwrap().take() {
            // The self-test reader still holds the original file object open,
            // so this models a hostile pathname swap between the run's last
            // probe and its identity-checked cleanup (API2-02).
            let _ = remove_file(&target);
            let _ = write(&target, IMPOSTOR_BYTES);
        }
        Ok(Self {
            value: u32::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for SwapProbe {
    const ID: u32 = 720;
    const VERSION: u16 = 1;
    const KIND: varve::BlockKind = varve::BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x5A17C0DE0A9B02F1;
    const IS_KEYED: bool = false;
}

fn swap_spec() -> FormatSpec {
    static BLOCKS: &[varve::BlockDescriptor] = &[varve::BlockDescriptor {
        id: SwapProbe::ID,
        name: "SwapProbe",
        version: SwapProbe::VERSION,
        kind: varve::BlockKind::Fixed,
        fields: SwapProbe::FIELDS,
    }];
    FormatSpec::new(
        b"SWPCK",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

#[test]
fn self_test_cleanup_refuses_to_delete_a_swapped_in_file_object() {
    let path = temp_path("cleanup_swap");
    cleanup(&path);

    *SWAP_ON_DECODE.lock().unwrap() = Some(path.to_path_buf());
    let report = swap_spec()
        .self_test(&path)
        .with_block(SwapProbe { value: 5 })
        .cleanup(true)
        .run();

    assert!(report.passed(), "{report:#?}");
    // API2-02: cleanup(true) verified the native object identity before
    // deleting by pathname, so the swapped-in impostor must survive even
    // though this run created (and would otherwise clean up) the pathname.
    let bytes = std::fs::read(&*path)
        .expect("cleanup(true) must not delete a file object swapped in at the pathname");
    assert_eq!(bytes, IMPOSTOR_BYTES);
    // The unowned lock marker is still tidied through the writer-lock
    // protocol.
    assert!(!lock_marker(&path).exists());
    cleanup(&path);
}

/// Fixed block whose encoding streams `len` zero bytes without any upfront
/// buffer, so the encoder's output budget — not the value — bounds how much
/// is ever materialized.
#[derive(Clone, Debug, PartialEq)]
struct BigBlob {
    len: u64,
}

impl VarveEncode for BigBlob {
    const WIRE_TYPE: WireType = WireType::Bytes;

    fn encode_varve(&self, encoder: &mut varve::Encoder) -> varve::Result<()> {
        const CHUNK: [u8; 4096] = [0u8; 4096];
        let mut remaining = self.len;
        while remaining > 0 {
            let take = remaining.min(CHUNK.len() as u64) as usize;
            encoder.write_all(&CHUNK[..take]);
            remaining -= take as u64;
        }
        Ok(())
    }
}

impl VarveDecode for BigBlob {
    const WIRE_TYPE: WireType = WireType::Bytes;

    fn decode_varve(decoder: &mut varve::Decoder<'_>) -> varve::Result<Self> {
        let len = decoder.remaining();
        decoder.read_exact(len)?;
        Ok(Self { len: len as u64 })
    }
}

impl VarveBlock for BigBlob {
    const ID: u32 = 721;
    const VERSION: u16 = 1;
    const KIND: varve::BlockKind = varve::BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x2B1608B10BB10B01;
    const IS_KEYED: bool = false;
}

fn blob_spec() -> FormatSpec {
    static BLOCKS: &[varve::BlockDescriptor] = &[varve::BlockDescriptor {
        id: BigBlob::ID,
        name: "BigBlob",
        version: BigBlob::VERSION,
        kind: varve::BlockKind::Fixed,
        fields: BigBlob::FIELDS,
    }];
    FormatSpec::new(
        b"BLOBC",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(ReadLimits::finite_all(64_000))
}

#[test]
fn writer_encode_paths_reject_oversized_values_with_typed_limit_error() {
    let path = temp_path("encode_limit");
    cleanup(&path);

    let mut writer = blob_spec().create(&path).expect("create blob file");

    // DEF-02: the resident push encoder is hard-bounded by the logical
    // payload limit, so a 16 MiB value fails with the typed limit error. The
    // rejected encoding is never materialized: `Encoder::write_all` checks
    // the budget before extending its buffer (allocation-freedom is verified
    // by inspection there, since observing allocations is not portable).
    let err = writer
        .push(&BigBlob {
            len: 16 * 1024 * 1024,
        })
        .expect_err("oversized push must fail");
    assert!(
        matches!(
            err,
            Error::LimitExceeded {
                resource: "logical payload length",
                ..
            }
        ),
        "{err:?}"
    );

    // Metadata entries are rejected before the caller's key and value are
    // even cloned for encoding.
    let value = vec![0u8; 128 * 1024];
    let err = writer
        .write_metadata("k", &value)
        .expect_err("oversized metadata must fail");
    assert!(
        matches!(
            err,
            Error::LimitExceeded {
                resource: "logical payload length",
                ..
            }
        ),
        "{err:?}"
    );

    // The typed rejection happens before any file mutation, so the writer
    // stays usable.
    writer
        .push(&BigBlob { len: 32 })
        .expect("small push after rejection");
    drop(writer);
    cleanup(&path);
}

#[test]
fn generated_variable_push_rejects_oversized_field_with_typed_limit_error() {
    let path = temp_path("encode_limit_variable");
    cleanup(&path);

    let mut writer = SelfCheckFormat::spec()
        .create_writer_with_limits(&path, ReadLimits::finite_all(64_000))
        .expect("create limited writer");

    // DEF-02: the generated variable encoder runs inside the limit-bounded
    // entry-point encoder, so the record is refused with the typed limit
    // error instead of being fully materialized first. Generated nested
    // fields inherit the parent's remaining budget through
    // `Encoder::encode_nested_to_vec` (pinned by
    // `generated_nested_field_encode_inherits_the_entry_point_budget` below).
    let err = writer
        .push_keyed(&User {
            id: 7,
            name: "x".repeat(1024 * 1024),
            flags: 0,
        })
        .expect_err("oversized variable push must fail");
    assert!(
        matches!(
            err,
            Error::LimitExceeded {
                resource: "logical payload length",
                ..
            }
        ),
        "{err:?}"
    );

    writer
        .push_keyed(&User {
            id: 7,
            name: "small".to_string(),
            flags: 0,
        })
        .expect("small push after rejection");
    drop(writer);
    cleanup(&path);
}

#[test]
fn generated_nested_field_encode_inherits_the_entry_point_budget() {
    let path = temp_path("encode_limit_nested");
    cleanup(&path);

    let mut writer = SelfCheckFormat::spec()
        .create_writer_with_limits(&path, ReadLimits::finite_all(64_000))
        .expect("create limited writer");

    // DEF-02 (varve-macros): the generated variable encoder stages each
    // nested field through `Encoder::encode_nested_to_vec`, which caps the
    // child encoder at the entry point's REMAINING budget. A ~2 MiB
    // `Vec<u64>` field (262_144 element writes of 8 bytes each) must be cut
    // off near the 64 KB logical budget instead of being fully materialized
    // into an unbounded child buffer first. The pin: `actual` in the typed
    // error is the child length observed at the first over-budget write
    // (<= limit + one 8-byte element); before the fix the child buffered the
    // whole ~2 MiB encoding and `actual` reported that full length.
    let err = writer
        .push(&Trace {
            id: 9,
            samples: vec![0u64; 262_144],
        })
        .expect_err("oversized nested field must fail");
    match err {
        Error::LimitExceeded {
            resource: "logical payload length",
            actual,
            limit,
        } => {
            assert!(
                limit <= 64_000,
                "child limit must be the entry point's remaining budget, got {limit}"
            );
            assert!(
                actual <= limit + 8,
                "encode must stop at the first over-budget write \
                 (actual {actual}, limit {limit})"
            );
        }
        other => panic!("expected the typed logical payload limit error, got {other:?}"),
    }

    // The typed rejection happens before any file mutation, so the writer
    // stays usable.
    writer
        .push(&Trace {
            id: 9,
            samples: vec![1, 2, 3],
        })
        .expect("small push after rejection");
    drop(writer);
    cleanup(&path);
}

struct TempPath {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for TempPath {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

impl AsRef<std::path::Path> for TempPath {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

fn temp_path(name: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir.path().join(format!(
        "varve_self_check_{name}_{}.vrv",
        std::process::id()
    ));
    TempPath { path, _dir: dir }
}

fn lock_marker(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let _ = remove_file(lock_marker(path));
}
