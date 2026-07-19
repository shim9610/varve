//! Contract coverage for the v2 computed schema hash (review finding
//! API2-01).
//!
//! Fixed/matrix payload bytes follow field declaration order, so the computed
//! hash must cover the encoding ordinal: two blocks with the same field
//! id/name/type set in different declaration order have different canonical
//! bytes and must never share a hash. The hash also folds in the per-block
//! identities (endian override, keyedness, generated codec fingerprint) that
//! `BlockDescriptor` alone does not carry, and the `schema_hash = 0` opt-out
//! semantics are locked in at open time.

use std::fs::remove_file;
use std::path::{Path, PathBuf};

use varve::{
    BlockDescriptor, BlockKind, Endian, Error, FieldDescriptor, FieldPresence, FormatSpec,
    IndexPolicy, IntegrityPolicy, ManifestPolicy, ReadLimits, RecoveryPolicy, VarveBlock, WireType,
    encode_to_vec, varve_format,
};

const FIELD_ALPHA: FieldDescriptor = FieldDescriptor {
    id: 1,
    name: "alpha",
    wire_type: WireType::U32,
    presence: FieldPresence::Required,
};

const FIELD_BETA: FieldDescriptor = FieldDescriptor {
    id: 2,
    name: "beta",
    wire_type: WireType::U64,
    presence: FieldPresence::Required,
};

/// Fields declared alpha-then-beta: canonical fixed bytes are u32 then u64.
static BLOCKS_ALPHA_BETA: &[BlockDescriptor] = &[BlockDescriptor {
    id: 7,
    name: "Sample",
    version: 1,
    kind: BlockKind::Fixed,
    fields: &[FIELD_ALPHA, FIELD_BETA],
}];

/// Same field id/name/type set declared beta-then-alpha: canonical fixed
/// bytes are u64 then u32, so the wire layout differs.
static BLOCKS_BETA_ALPHA: &[BlockDescriptor] = &[BlockDescriptor {
    id: 7,
    name: "Sample",
    version: 1,
    kind: BlockKind::Fixed,
    fields: &[FIELD_BETA, FIELD_ALPHA],
}];

fn spec_with_blocks(blocks: &'static [BlockDescriptor]) -> FormatSpec {
    FormatSpec::new(
        b"SHCT",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        blocks,
    )
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

/// The review's exact hole: reordered field declarations previously hashed
/// identically because fields were sorted by id and the encoding ordinal was
/// not hashed.
#[test]
fn reordered_field_declarations_change_computed_schema_hash() {
    assert_ne!(
        spec_with_blocks(BLOCKS_ALPHA_BETA).computed_schema_hash(),
        spec_with_blocks(BLOCKS_BETA_ALPHA).computed_schema_hash()
    );
}

/// Identical, independently constructed specs still hash identically.
#[test]
fn identical_schemas_still_match() {
    static BLOCKS_ALPHA_BETA_AGAIN: &[BlockDescriptor] = &[BlockDescriptor {
        id: 7,
        name: "Sample",
        version: 1,
        kind: BlockKind::Fixed,
        fields: &[FIELD_ALPHA, FIELD_BETA],
    }];

    assert_eq!(
        spec_with_blocks(BLOCKS_ALPHA_BETA).computed_schema_hash(),
        spec_with_blocks(BLOCKS_ALPHA_BETA_AGAIN).computed_schema_hash()
    );
    // Identity attachment is deterministic too.
    static IDENTITIES: &[(u32, Option<Endian>, bool, u64)] = &[(7, None, false, 0xA1)];
    assert_eq!(
        spec_with_blocks(BLOCKS_ALPHA_BETA)
            .with_block_identities(IDENTITIES)
            .computed_schema_hash(),
        spec_with_blocks(BLOCKS_ALPHA_BETA)
            .with_block_identities(IDENTITIES)
            .computed_schema_hash()
    );
}

/// A per-block endian override is a wire-layout input and must change the
/// hash, as must the spec-level endian.
#[test]
fn endian_variants_change_computed_schema_hash() {
    static DEFAULT_ENDIAN: &[(u32, Option<Endian>, bool, u64)] = &[(7, None, false, 0xA1)];
    static LITTLE: &[(u32, Option<Endian>, bool, u64)] = &[(7, Some(Endian::Little), false, 0xA1)];
    static BIG: &[(u32, Option<Endian>, bool, u64)] = &[(7, Some(Endian::Big), false, 0xA1)];

    let base = spec_with_blocks(BLOCKS_ALPHA_BETA);
    let default_endian = base
        .with_block_identities(DEFAULT_ENDIAN)
        .computed_schema_hash();
    let little = base.with_block_identities(LITTLE).computed_schema_hash();
    let big = base.with_block_identities(BIG).computed_schema_hash();
    assert_ne!(default_endian, little);
    assert_ne!(default_endian, big);
    assert_ne!(little, big);

    // Spec-level endian was already covered by v1 and stays covered.
    let mut big_spec = base;
    big_spec.endian = Endian::Big;
    assert_ne!(base.computed_schema_hash(), big_spec.computed_schema_hash());
}

/// Keyedness and the generated codec fingerprint are decode-semantics inputs
/// and must change the hash; absence of identities must also differ from
/// presence.
#[test]
fn keyedness_and_codec_identity_change_computed_schema_hash() {
    static UNKEYED: &[(u32, Option<Endian>, bool, u64)] = &[(7, None, false, 0xA1)];
    static KEYED: &[(u32, Option<Endian>, bool, u64)] = &[(7, None, true, 0xA1)];
    static OTHER_CODEC: &[(u32, Option<Endian>, bool, u64)] = &[(7, None, false, 0xB2)];

    let base = spec_with_blocks(BLOCKS_ALPHA_BETA);
    let unkeyed = base.with_block_identities(UNKEYED).computed_schema_hash();
    let keyed = base.with_block_identities(KEYED).computed_schema_hash();
    let other_codec = base
        .with_block_identities(OTHER_CODEC)
        .computed_schema_hash();
    assert_ne!(unkeyed, keyed);
    assert_ne!(unkeyed, other_codec);
    assert_ne!(base.computed_schema_hash(), unkeyed);
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 7, version = 1, kind = "fixed")]
struct GeneratedSample {
    alpha: u32,
    beta: u64,
}

varve_format! {
    pub struct HashContractFormat {
        magic: b"SHCF";
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
        manifest: none;
        blocks: [GeneratedSample];
    }
}

/// `varve_format!` attaches per-block identities before pinning the computed
/// hash, so the pinned value covers the generated codec identity.
#[test]
fn generated_format_pins_identity_covered_hash() {
    let spec = HashContractFormat::spec();
    assert_ne!(spec.schema_hash, 0);
    assert_eq!(spec.schema_hash, spec.computed_schema_hash());
    assert_eq!(spec.block_identities.len(), 1);
    assert_eq!(
        spec.block_identity(GeneratedSample::ID),
        Some((
            GeneratedSample::ID,
            <GeneratedSample as VarveBlock>::ENDIAN,
            <GeneratedSample as VarveBlock>::IS_KEYED,
            <GeneratedSample as VarveBlock>::SCHEMA_FINGERPRINT,
        ))
    );
    // A spec identical except for its identities cannot share the hash.
    assert_ne!(
        spec.computed_schema_hash(),
        spec.with_block_identities(&[]).computed_schema_hash()
    );
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
        "varve_schema_hash_contract_{name}_{}.vrv",
        std::process::id()
    ));
    TempPath { path, _dir: dir }
}

fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}

/// Locks the documented `schema_hash` opt-out semantics: leaving the declared
/// hash at 0 disables the open-time comparison entirely, while a pinned
/// non-zero hash fails closed against a mismatched stored value.
#[test]
fn omitted_schema_hash_zero_disables_open_comparison() -> varve::Result<()> {
    let path = temp_path("omitted_zero");
    cleanup(&path);

    let alpha_beta = spec_with_blocks(BLOCKS_ALPHA_BETA);
    let beta_alpha = spec_with_blocks(BLOCKS_BETA_ALPHA);
    let pinned = alpha_beta.with_computed_schema_hash();
    {
        let file = varve::VarveFile::create(pinned, &path)?;
        drop(file);
    }

    // schema_hash = 0: comparison disabled, the reordered schema opens even
    // though its computed hash differs from the stored one.
    assert_ne!(
        beta_alpha.computed_schema_hash(),
        alpha_beta.computed_schema_hash()
    );
    let opened = varve::VarveFile::open_readonly(beta_alpha, &path)?;
    drop(opened);

    // A pinned mismatching hash fails closed.
    let pinned_beta_alpha = beta_alpha.with_computed_schema_hash();
    match varve::VarveFile::open_readonly(pinned_beta_alpha, &path) {
        Err(Error::SchemaHashMismatch { expected, actual }) => {
            assert_eq!(expected, beta_alpha.computed_schema_hash());
            assert_eq!(actual, alpha_beta.computed_schema_hash());
        }
        other => panic!("expected SchemaHashMismatch, got {other:?}"),
    }

    cleanup(&path);
    Ok(())
}

/// A file created with the hash omitted stores 0, and opening it with a
/// pinned spec also fails closed: omission is recorded, not inferred.
#[test]
fn file_created_with_omitted_hash_rejects_pinned_open() -> varve::Result<()> {
    let path = temp_path("pinned_open");
    cleanup(&path);

    let alpha_beta = spec_with_blocks(BLOCKS_ALPHA_BETA);
    {
        let file = varve::VarveFile::create(alpha_beta, &path)?;
        drop(file);
    }

    match varve::VarveFile::open_readonly(alpha_beta.with_computed_schema_hash(), &path) {
        Err(Error::SchemaHashMismatch { expected, actual }) => {
            assert_eq!(expected, alpha_beta.computed_schema_hash());
            assert_eq!(actual, 0);
        }
        other => panic!("expected SchemaHashMismatch, got {other:?}"),
    }

    cleanup(&path);
    Ok(())
}

/// API-04 reproducer: two crates can spell a nested field type identically
/// while their custom codecs emit different bytes. Before transitive codec
/// identity the outer fingerprint and the computed format hash both matched.
macro_rules! nested_codec_module {
    ($module:ident, $schema_id:expr, $first:ident, $second:ident) => {
        mod $module {
            use varve::{
                Decoder, Encoder, VarveBlock, VarveDecode, VarveEncode, WireType, varve_format,
            };

            /// Hand-written nested codec. Both modules spell it `Payload`.
            #[derive(Clone, Debug, PartialEq)]
            pub struct Payload {
                pub left: u32,
                pub right: u32,
            }

            impl VarveEncode for Payload {
                const WIRE_TYPE: WireType = WireType::Nested;
                const SCHEMA_ID: u64 = $schema_id;

                fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
                    self.$first.encode_varve(encoder)?;
                    self.$second.encode_varve(encoder)
                }
            }

            impl VarveDecode for Payload {
                const WIRE_TYPE: WireType = WireType::Nested;
                const SCHEMA_ID: u64 = $schema_id;

                fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
                    let $first = u32::decode_varve(decoder)?;
                    let $second = u32::decode_varve(decoder)?;
                    Ok(Self { $first, $second })
                }
            }

            #[derive(Clone, Debug, PartialEq, VarveBlock)]
            #[varve(id = 8, version = 1, kind = "fixed")]
            pub struct Wrapper {
                pub payload: Payload,
            }

            varve_format! {
                pub struct NestedCodecFormat {
                    magic: b"SHCN";
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
                    manifest: none;
                    blocks: [Wrapper];
                }
            }
        }
    };
}

nested_codec_module!(nested_codec_alpha, 0xA1A1_0000_0000_0001, left, right);
nested_codec_module!(nested_codec_beta, 0xB2B2_0000_0000_0002, right, left);

#[test]
fn custom_nested_codecs_with_equal_spelling_cannot_share_an_identity() {
    let alpha = nested_codec_alpha::NestedCodecFormat::spec();
    let beta = nested_codec_beta::NestedCodecFormat::spec();

    // The declared schema really is spelled identically: same block, same
    // field, same coarse wire type. Only the codec differs.
    let alpha_block = alpha.block(8).expect("alpha block descriptor");
    let beta_block = beta.block(8).expect("beta block descriptor");
    assert_eq!(alpha_block.name, beta_block.name);
    assert_eq!(alpha_block.version, beta_block.version);
    assert_eq!(alpha_block.kind, beta_block.kind);
    assert_eq!(alpha_block.fields.len(), beta_block.fields.len());
    assert_eq!(alpha_block.fields[0].id, beta_block.fields[0].id);
    assert_eq!(alpha_block.fields[0].name, beta_block.fields[0].name);
    assert_eq!(
        alpha_block.fields[0].wire_type,
        beta_block.fields[0].wire_type
    );

    // The codecs genuinely disagree on the bytes.
    let alpha_bytes = encode_to_vec(
        &nested_codec_alpha::Wrapper {
            payload: nested_codec_alpha::Payload { left: 1, right: 2 },
        },
        Endian::Little,
    )
    .expect("encode alpha");
    let beta_bytes = encode_to_vec(
        &nested_codec_beta::Wrapper {
            payload: nested_codec_beta::Payload { left: 1, right: 2 },
        },
        Endian::Little,
    )
    .expect("encode beta");
    assert_ne!(alpha_bytes, beta_bytes);

    // ... so neither the block fingerprint nor the format hash may match.
    assert_ne!(
        <nested_codec_alpha::Wrapper as VarveBlock>::SCHEMA_FINGERPRINT,
        <nested_codec_beta::Wrapper as VarveBlock>::SCHEMA_FINGERPRINT
    );
    assert_ne!(alpha.computed_schema_hash(), beta.computed_schema_hash());
    assert_ne!(alpha.schema_hash, beta.schema_hash);
}

/// The identity is transitive, not just per-field: a block whose only nested
/// field is another derived block inherits that block's codec identity.
#[test]
fn nested_block_codec_identity_is_transitive() {
    assert_eq!(
        <nested_codec_alpha::Payload as varve::VarveEncode>::SCHEMA_ID,
        0xA1A1_0000_0000_0001
    );
    assert_eq!(
        <nested_codec_alpha::Wrapper as varve::VarveEncode>::SCHEMA_ID,
        <nested_codec_alpha::Wrapper as VarveBlock>::SCHEMA_FINGERPRINT
    );
    assert_ne!(
        <nested_codec_alpha::Wrapper as varve::VarveEncode>::SCHEMA_ID,
        <nested_codec_beta::Wrapper as varve::VarveEncode>::SCHEMA_ID
    );
}

/// Built-in codecs declare structural identities that separate types the
/// coarse wire type cannot.
#[test]
fn built_in_codec_identities_are_structural() {
    use varve::VarveEncode;

    assert_ne!(<u32 as VarveEncode>::SCHEMA_ID, 0);
    assert_ne!(
        <u32 as VarveEncode>::SCHEMA_ID,
        <i32 as VarveEncode>::SCHEMA_ID
    );
    assert_ne!(
        <Vec<u8> as VarveEncode>::SCHEMA_ID,
        <String as VarveEncode>::SCHEMA_ID
    );
    // Containers fold their elements, so same-shape containers over different
    // elements never share an identity.
    assert_ne!(
        <Option<u32> as VarveEncode>::SCHEMA_ID,
        <Option<i32> as VarveEncode>::SCHEMA_ID
    );
    assert_ne!(
        <Vec<u32> as VarveEncode>::SCHEMA_ID,
        <Vec<u64> as VarveEncode>::SCHEMA_ID
    );
    assert_ne!(
        <(u32, u64) as VarveEncode>::SCHEMA_ID,
        <(u64, u32) as VarveEncode>::SCHEMA_ID
    );
}

/// The same API-04 defect one wire type over: a hand-written codec does not
/// have to be `WireType::Nested` to collide. These two modules spell the field
/// type `Packed` identically, both declare `WireType::U64`, and both are the
/// single field of a derived `Outer` block - only the packing (and therefore
/// the emitted bytes) differs. Without a declared `SCHEMA_ID` the fingerprints
/// matched while the bytes did not, which is the exact shape of the report's
/// finding.
macro_rules! packed_codec_module {
    ($module:ident, $schema_id:expr, $high:ident, $low:ident) => {
        mod $module {
            use varve::{
                Decoder, Encoder, VarveBlock, VarveDecode, VarveEncode, WireType, varve_format,
            };

            /// Hand-written NON-nested codec. Both modules spell it `Packed`.
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            pub struct Packed {
                pub left: u32,
                pub right: u32,
            }

            impl VarveEncode for Packed {
                const WIRE_TYPE: WireType = WireType::U64;
                const SCHEMA_ID: u64 = $schema_id;

                fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
                    let packed = (u64::from(self.$high) << 32) | u64::from(self.$low);
                    packed.encode_varve(encoder)
                }
            }

            impl VarveDecode for Packed {
                const WIRE_TYPE: WireType = WireType::U64;
                const SCHEMA_ID: u64 = $schema_id;

                fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
                    let packed = u64::decode_varve(decoder)?;
                    let $high = (packed >> 32) as u32;
                    let $low = packed as u32;
                    Ok(Self { $high, $low })
                }
            }

            #[derive(Clone, Debug, PartialEq, VarveBlock)]
            #[varve(id = 9, version = 1, kind = "fixed")]
            pub struct Outer {
                pub packed: Packed,
            }

            varve_format! {
                pub struct PackedCodecFormat {
                    magic: b"SHCP";
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
                    manifest: none;
                    blocks: [Outer];
                }
            }
        }
    };
}

packed_codec_module!(packed_codec_alpha, 0xC3C3_0000_0000_0001, left, right);
packed_codec_module!(packed_codec_beta, 0xD4D4_0000_0000_0002, right, left);

#[test]
fn custom_non_nested_codecs_with_equal_spelling_cannot_share_an_identity() {
    let alpha = packed_codec_alpha::PackedCodecFormat::spec();
    let beta = packed_codec_beta::PackedCodecFormat::spec();

    // Everything the declared schema can see is identical, including the
    // coarse wire type, which here is not Nested.
    let alpha_block = alpha.block(9).expect("alpha block descriptor");
    let beta_block = beta.block(9).expect("beta block descriptor");
    assert_eq!(alpha_block.name, beta_block.name);
    assert_eq!(alpha_block.version, beta_block.version);
    assert_eq!(alpha_block.kind, beta_block.kind);
    assert_eq!(alpha_block.fields.len(), beta_block.fields.len());
    assert_eq!(alpha_block.fields[0].id, beta_block.fields[0].id);
    assert_eq!(alpha_block.fields[0].name, beta_block.fields[0].name);
    assert_eq!(
        alpha_block.fields[0].wire_type,
        beta_block.fields[0].wire_type
    );
    assert_eq!(alpha_block.fields[0].wire_type, WireType::U64);

    // The codecs genuinely disagree on the bytes.
    let alpha_bytes = encode_to_vec(
        &packed_codec_alpha::Outer {
            packed: packed_codec_alpha::Packed { left: 1, right: 2 },
        },
        Endian::Little,
    )
    .expect("encode alpha");
    let beta_bytes = encode_to_vec(
        &packed_codec_beta::Outer {
            packed: packed_codec_beta::Packed { left: 1, right: 2 },
        },
        Endian::Little,
    )
    .expect("encode beta");
    assert_ne!(alpha_bytes, beta_bytes);

    // ... so neither the block fingerprint nor the format hash may match.
    assert_ne!(
        <packed_codec_alpha::Outer as VarveBlock>::SCHEMA_FINGERPRINT,
        <packed_codec_beta::Outer as VarveBlock>::SCHEMA_FINGERPRINT
    );
    assert_ne!(alpha.computed_schema_hash(), beta.computed_schema_hash());
    assert_ne!(alpha.schema_hash, beta.schema_hash);
}

/// A codec that declares no identity cannot acquire one by being wrapped: the
/// container fold propagates the absence outward as `0`. Otherwise
/// `Option<Packed>` would hash two *different* identity-less `Packed` codecs to
/// the same value and reopen the collision one container deep, while also
/// slipping past the derive's per-field assertion.
#[test]
fn containers_do_not_launder_a_missing_codec_identity() {
    use varve::{Decoder, Encoder, VarveDecode, VarveEncode};

    /// Deliberately identity-less: it declares no `SCHEMA_ID`. It is never a
    /// field of a derived block, because that no longer compiles.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Anonymous(u64);

    impl VarveEncode for Anonymous {
        const WIRE_TYPE: WireType = WireType::U64;

        fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
            self.0.encode_varve(encoder)
        }
    }

    impl VarveDecode for Anonymous {
        const WIRE_TYPE: WireType = WireType::U64;

        fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
            Ok(Self(u64::decode_varve(decoder)?))
        }
    }

    assert_eq!(<Anonymous as VarveEncode>::SCHEMA_ID, 0);
    assert_eq!(<Anonymous as VarveDecode>::SCHEMA_ID, 0);
    assert_eq!(<Option<Anonymous> as VarveEncode>::SCHEMA_ID, 0);
    assert_eq!(<Option<Anonymous> as VarveDecode>::SCHEMA_ID, 0);
    assert_eq!(<[Anonymous; 4] as VarveEncode>::SCHEMA_ID, 0);
    assert_eq!(<[Anonymous; 4] as VarveDecode>::SCHEMA_ID, 0);
    assert_eq!(<(u32, Anonymous) as VarveEncode>::SCHEMA_ID, 0);
    assert_eq!(<(u32, Anonymous) as VarveDecode>::SCHEMA_ID, 0);
    assert_eq!(<Option<Option<Anonymous>> as VarveEncode>::SCHEMA_ID, 0);

    // The same containers over an identified element still carry an identity,
    // so the rule above is absence propagation, not a blanket zero.
    assert_ne!(<Option<u64> as VarveEncode>::SCHEMA_ID, 0);
    assert_ne!(<[u64; 4] as VarveEncode>::SCHEMA_ID, 0);
    assert_ne!(<(u32, u64) as VarveEncode>::SCHEMA_ID, 0);
    // A zero-length array is a layout, not a missing element identity.
    assert_ne!(<[u64; 0] as VarveEncode>::SCHEMA_ID, 0);
}
