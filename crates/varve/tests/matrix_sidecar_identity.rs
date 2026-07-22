// DUR-04/05 regression for matrix sidecar identity and publication.
//
// DUR-04: a sidecar must be bound to one specific native file. Before manifest
// v2 a same-spec, same-generation sibling file happily adopted another file's
// sidecar. v2 records the native OS-object fingerprint and matrix layout
// generation, so file B rejects file A's sidecar.
//
// DUR-05: publication is a same-directory temp write + atomic replace, so a
// failed publish that never reaches the replace step leaves the previously
// published sidecar fully intact instead of truncating it in place.

#![cfg(feature = "integrity")]

use std::fs::remove_file;
use std::path::PathBuf;

use varve::{
    BlockDescriptor, BlockKind, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
    ManifestPolicy, MatrixBlockDescriptor, MatrixCommitDescriptor, MatrixCommitKind,
    MatrixDimensionDescriptor, MatrixDimensions, MatrixKey, MatrixResumeSignal, ReadLimits,
    RecoveryPolicy, VarveBlock, VarveDecode, VarveEncode, VarveMatrixBlock,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct IdentityCell {
    value: u32,
}

impl VarveEncode for IdentityCell {
    const WIRE_TYPE: varve::WireType = varve::WireType::Nested;

    fn encode_varve(&self, encoder: &mut varve::Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for IdentityCell {
    const WIRE_TYPE: varve::WireType = varve::WireType::Nested;

    fn decode_varve(decoder: &mut varve::Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u32::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for IdentityCell {
    const ID: u32 = 300;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Matrix;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x51DE_CA12_1DEA_7117;
    const IS_KEYED: bool = false;
}

impl VarveMatrixBlock for IdentityCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

fn identity_spec() -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: IdentityCell::ID,
        name: "IdentityCell",
        version: IdentityCell::VERSION,
        kind: BlockKind::Matrix,
        fields: &[],
    }];
    static DIMS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: "analysis",
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: IdentityCell::ID,
        dimensions: IdentityCell::DIMENSIONS,
        category: IdentityCell::CATEGORY,
        slot_stride: IdentityCell::SLOT_STRIDE,
    }];
    FormatSpec::new(
        b"MID",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::Crc32,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_matrix_spec(DIMS, COMMITS, MATRIX_BLOCKS)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

#[test]
fn same_spec_sibling_file_sidecar_is_rejected() -> varve::Result<()> {
    let path_a = temp_path("sidecar_identity_a");
    let path_b = temp_path("sidecar_identity_b");
    let sidecar_owner = temp_path("sidecar_identity_state");
    let sidecar = sidecar_owner.with_extension("sidecar");
    cleanup(&path_a);
    cleanup(&path_b);
    let _ = remove_file(&sidecar);

    let spec = identity_spec();

    // File A publishes a sidecar bound to its own native OS object.
    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path_a, dims)?;
        writer.write_matrix_cell(MatrixKey::new(0, 0), &IdentityCell { value: 1 })?;
        writer.commit_matrix_cell::<IdentityCell>(MatrixKey::new(0, 0))?;
        writer.write_matrix_sidecar("analysis", &sidecar, 7, b"resume-a")?;
        writer.flush()?;
    }

    // A reads its own sidecar back: identity matches.
    {
        let reader = spec.open_reader(&path_a)?;
        let (manifest, payload) = reader.read_matrix_sidecar("analysis", &sidecar)?;
        assert_eq!(manifest.generation, 7);
        assert_eq!(payload, b"resume-a");
    }

    // File B is a different native file with the identical spec, dims and
    // generation. It must reject A's sidecar on native identity, not adopt it.
    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path_b, dims)?;
        writer.write_matrix_cell(MatrixKey::new(0, 0), &IdentityCell { value: 2 })?;
        writer.commit_matrix_cell::<IdentityCell>(MatrixKey::new(0, 0))?;
        writer.flush()?;
    }
    {
        let reader_b = spec.open_reader(&path_b)?;
        assert!(
            matches!(
                reader_b.read_matrix_sidecar("analysis", &sidecar),
                Err(Error::MatrixSidecarMismatch("native identity"))
            ),
            "file B must not accept file A's sidecar",
        );
        // Reading with an explicit generation also rejects on identity, since
        // the native fingerprint is checked before the generation.
        assert!(matches!(
            reader_b.read_matrix_sidecar_with_generation("analysis", &sidecar, 7),
            Err(Error::MatrixSidecarMismatch("native identity"))
        ));
    }

    cleanup(&path_a);
    cleanup(&path_b);
    let _ = remove_file(&sidecar);
    Ok(())
}

// DUR2-03: recreating a matrix on the same pathname reuses the same OS file
// object with identical schema, dimensions and layout offsets, so before the
// per-create creation nonce every sidecar published against the previous
// logical matrix (the review reproduced a stale generation-41 sidecar being
// accepted) was indistinguishable from a fresh one. The recreated matrix must
// refuse the pre-recreate sidecar as stale.
#[test]
fn same_object_matrix_recreation_rejects_stale_sidecar() -> varve::Result<()> {
    let path = temp_path("sidecar_identity_recreate");
    let sidecar_owner = temp_path("sidecar_identity_recreate_state");
    let sidecar = sidecar_owner.with_extension("sidecar");
    cleanup(&path);
    let _ = remove_file(&sidecar);

    let spec = identity_spec();

    // The first logical matrix publishes a sidecar at generation 41.
    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        writer.write_matrix_cell(MatrixKey::new(0, 0), &IdentityCell { value: 1 })?;
        writer.commit_matrix_cell::<IdentityCell>(MatrixKey::new(0, 0))?;
        writer.write_matrix_sidecar("analysis", &sidecar, 41, b"stale-resume")?;
        writer.flush()?;
    }

    // Recreate the matrix at the same pathname with the same dimensions. The
    // OS file object, schema hash and layout offsets are all unchanged; only
    // the creation nonce distinguishes the new logical matrix.
    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        writer.write_matrix_cell(MatrixKey::new(0, 0), &IdentityCell { value: 2 })?;
        writer.commit_matrix_cell::<IdentityCell>(MatrixKey::new(0, 0))?;
        writer.flush()?;
    }
    {
        let reader = spec.open_reader(&path)?;
        assert!(
            matches!(
                reader.read_matrix_sidecar("analysis", &sidecar),
                Err(Error::MatrixSidecarMismatch("creation nonce"))
            ),
            "a recreated matrix must not accept the pre-recreate sidecar",
        );
        // A caller-supplied expected generation must not substitute for the
        // native creation identity: even asking for the stale sidecar's own
        // generation is refused on the nonce.
        assert!(matches!(
            reader.read_matrix_sidecar_with_generation("analysis", &sidecar, 41),
            Err(Error::MatrixSidecarMismatch("creation nonce"))
        ));
        // The verified resume signal treats the stale sidecar as regenerable.
        assert_eq!(
            reader.matrix_verified_sidecar_resume_signal("analysis", &sidecar)?,
            MatrixResumeSignal::DiscardRecommended
        );
    }

    // The recreated matrix can publish and read back its own fresh sidecar.
    {
        let mut writer = spec.open_writer(&path)?;
        writer.write_matrix_sidecar("analysis", &sidecar, 1, b"fresh-resume")?;
        writer.flush()?;
    }
    {
        let reader = spec.open_reader(&path)?;
        let (manifest, payload) = reader.read_matrix_sidecar("analysis", &sidecar)?;
        assert_eq!(manifest.generation, 1);
        assert_eq!(payload, b"fresh-resume");
    }

    cleanup(&path);
    let _ = remove_file(&sidecar);
    Ok(())
}

// API2-02: the exclusive matrix creation path keeps one handle from the
// ownership claim through initialization, so diagnostics never has to close a
// claim handle and re-open the pathname with truncation.
#[test]
fn create_new_with_dims_is_exclusive_and_initializes_the_claimed_handle() -> varve::Result<()> {
    let path = temp_path("sidecar_identity_create_new");
    cleanup(&path);

    let spec = identity_spec();
    let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
    {
        let mut writer = varve::VarveFile::create_new_with_dims(spec, &path, dims)?;
        writer.write_matrix_cell(MatrixKey::new(0, 0), &IdentityCell { value: 7 })?;
        writer.commit_matrix_cell::<IdentityCell>(MatrixKey::new(0, 0))?;
        writer.flush()?;
    }

    // A pre-existing target is refused instead of truncated.
    let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
    match varve::VarveFile::create_new_with_dims(spec, &path, dims) {
        Err(Error::Io(error)) => {
            assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }

    // The refused create left the existing matrix untouched.
    {
        let reader = varve::VarveFile::open_readonly(spec, &path)?;
        assert_eq!(
            reader.read_matrix_cell::<IdentityCell>(MatrixKey::new(0, 0))?,
            IdentityCell { value: 7 }
        );
    }

    cleanup(&path);
    Ok(())
}

#[test]
fn failed_publish_leaves_original_sidecar_intact() -> varve::Result<()> {
    let path = temp_path("sidecar_publish_intact");
    let sidecar_owner = temp_path("sidecar_publish_state");
    let sidecar = sidecar_owner.with_extension("sidecar");
    cleanup(&path);
    let _ = remove_file(&sidecar);

    let spec = identity_spec();
    let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
    let mut writer = spec.create_writer_with_dims(&path, dims)?;
    writer.write_matrix_cell(MatrixKey::new(0, 0), &IdentityCell { value: 9 })?;
    writer.commit_matrix_cell::<IdentityCell>(MatrixKey::new(0, 0))?;

    // Publish generation 1 successfully.
    writer.write_matrix_sidecar("analysis", &sidecar, 1, b"generation-one")?;

    // A second publish that fails before the atomic replace ever runs (an
    // unknown commit category is rejected up front) must not touch the already
    // published sidecar. The old in-place `File::create` would have truncated
    // it the moment it opened the destination.
    let failed = writer.write_matrix_sidecar("missing_category", &sidecar, 2, b"generation-two");
    assert!(failed.is_err(), "the second publish should fail");

    // The original generation-1 sidecar is still complete and readable.
    let reader = spec.open_reader(&path)?;
    let (manifest, payload) = reader.read_matrix_sidecar("analysis", &sidecar)?;
    assert_eq!(manifest.generation, 1);
    assert_eq!(payload, b"generation-one");

    // No stray temp files were left in the sidecar's directory.
    let dir = sidecar.parent().expect("sidecar parent");
    let sidecar_name = sidecar.file_name().expect("sidecar file name");
    let leftover: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .filter(|name| {
            let name = name.to_string_lossy();
            name.contains(&*sidecar_name.to_string_lossy()) && name.contains(".rewrite.")
        })
        .collect();
    assert!(
        leftover.is_empty(),
        "publication left temp files behind: {leftover:?}",
    );

    drop(reader);
    drop(writer);
    cleanup(&path);
    let _ = remove_file(&sidecar);
    Ok(())
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
        "varve_{name}_{}_{}.vrv",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    TempPath { path, _dir: dir }
}

fn cleanup(path: &PathBuf) {
    match remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", path.display()),
    }
    let lock = path.with_extension("vrv.lock");
    match remove_file(&lock) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", lock.display()),
    }
}
