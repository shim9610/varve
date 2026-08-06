//! Public-API surface fixture: every documented field type must still compile
//! as a derived block field and must still round-trip.
//!
//! # Why this crate exists
//!
//! Round 3 introduced a mandatory non-zero `VarveEncode::SCHEMA_ID` /
//! `VarveDecode::SCHEMA_ID` contract and made `#[derive(VarveBlock)]` reject
//! any field whose codec identity is zero. The built-in public types
//! `ChunkedBytes` and `PackedBitmap` were never given an identity, so the
//! derive macro started rejecting documented public API — and nothing in the
//! test suite noticed, because no test derived a block over the *public*
//! surface as a downstream user does.
//!
//! This crate is that missing gate. It is deliberately outside the workspace
//! (its own `[workspace]` table plus a path dependency, exactly like
//! `tools/rename-fixture`) so it resolves `varve` the way a real consumer does.
//! It must COMPILE and RUN; CI builds it as its own job.
//!
//! # What is covered
//!
//! The type list is taken from the documentation, not from guesswork:
//!
//! * `docs/custom-codec-guide.md`: "scalars, `bool`, `String`, `Vec<u8>`,
//!   selected typed vectors, fixed arrays, tuples, `Option`, `BTreeMap`, and
//!   `HashMap`".
//! * `docs/spec.md`: `ChunkedBytes` "can be used inside variable fields", and
//!   `ChunkedBytes` and `PackedBitmap` are "both usable as ordinary derived
//!   variable fields". `PackedBitmap` is the LSB-first packed bitmap primitive
//!   and a documented public value type, so it must work as an ordinary derived
//!   *variable* field even though it is deliberately **not** a fixed-width
//!   matrix field.
//! * `docs/spec.md` ("Macro Contract") / `docs/api-reference.md`: fixed,
//!   variable, and matrix block kinds, and the matrix rule that slots are
//!   fixed-width scalar or fixed-array types.
//!
//! Every codec below is exercised three ways: a compile-time identity
//! assertion, a value round-trip through `encode_to_vec`/`decode_from_slice`,
//! and — for the block kinds a format can store — a real file round-trip
//! through a `varve_format!` declaration.

use std::collections::{BTreeMap, HashMap};

use varve::{
    ChunkedBytes, CompressionLevel, Endian, PackedBitmap, VarveBlock, VarveDecode, VarveEncode,
    decode_from_slice, encode_to_vec, varve_format,
};

/// Compile-time proof that a codec declares a usable identity.
///
/// This is the exact contract `#[derive(VarveBlock)]` enforces on every field.
/// Asserting it as an associated const makes the failure a compile error that
/// names the type, which is what a downstream user would see.
struct CodecIdentity<T: VarveEncode + VarveDecode>(std::marker::PhantomData<T>);

impl<T: VarveEncode + VarveDecode> CodecIdentity<T> {
    const OK: () = {
        assert!(
            <T as VarveEncode>::SCHEMA_ID != 0,
            "public codec declares no encode identity; derived fields of this type cannot compile"
        );
        assert!(
            <T as VarveDecode>::SCHEMA_ID != 0,
            "public codec declares no decode identity; derived fields of this type cannot compile"
        );
    };
}

/// Round-trips one value through the public codec entry points and forces the
/// compile-time identity assertion for its type.
fn roundtrip<T>(label: &str, value: T)
where
    T: VarveEncode + VarveDecode + PartialEq + std::fmt::Debug,
{
    let () = CodecIdentity::<T>::OK;
    for endian in [Endian::Little, Endian::Big] {
        let encoded = encode_to_vec(&value, endian)
            .unwrap_or_else(|error| panic!("{label}: encode failed: {error}"));
        let decoded: T = decode_from_slice(&encoded, endian)
            .unwrap_or_else(|error| panic!("{label}: decode failed: {error}"));
        assert_eq!(
            decoded, value,
            "{label}: value changed across the roundtrip"
        );
    }
    println!("  codec ok: {label}");
}

// ---------------------------------------------------------------------------
// Derived blocks over the documented field types.
// ---------------------------------------------------------------------------

/// Fixed blocks are positional canonical payloads and cannot omit fields, so
/// every field is required (`docs/spec.md`, "Macro Contract"). Scalars and fixed
/// arrays are the documented shapes for them.
#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 1, version = 1, kind = "fixed")]
struct FixedScalars {
    flag: bool,
    a_u8: u8,
    a_i8: i8,
    a_u16: u16,
    a_i16: i16,
    a_u32: u32,
    a_i32: i32,
    a_u64: u64,
    a_i64: i64,
    a_u128: u128,
    a_i128: i128,
    a_f32: f32,
    a_f64: f64,
    quad: [u32; 4],
    grid: [[u8; 2]; 3],
}

/// Every container and helper codec the API reference lists as usable in a
/// variable field, in one block.
///
/// The two fields that matter most for the regression this crate exists to
/// catch are `blob: ChunkedBytes` and `mask: PackedBitmap`: both are public,
/// both are documented, and both stopped compiling here when the identity
/// contract landed without identities for them.
#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 2, version = 1, kind = "variable", key = "id")]
struct EveryVariableCodec {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    text: String,
    #[varve(field_id = 3)]
    bytes: Vec<u8>,
    #[varve(field_id = 4)]
    bools: Vec<bool>,
    #[varve(field_id = 5)]
    i8s: Vec<i8>,
    #[varve(field_id = 6)]
    u16s: Vec<u16>,
    #[varve(field_id = 7)]
    i16s: Vec<i16>,
    #[varve(field_id = 8)]
    u32s: Vec<u32>,
    #[varve(field_id = 9)]
    i32s: Vec<i32>,
    #[varve(field_id = 10)]
    u64s: Vec<u64>,
    #[varve(field_id = 11)]
    i64s: Vec<i64>,
    #[varve(field_id = 12)]
    u128s: Vec<u128>,
    #[varve(field_id = 13)]
    i128s: Vec<i128>,
    #[varve(field_id = 14)]
    f32s: Vec<f32>,
    #[varve(field_id = 15)]
    f64s: Vec<f64>,
    #[varve(field_id = 16)]
    strings: Vec<String>,
    #[varve(field_id = 17)]
    fixed_array: [u32; 4],
    #[varve(field_id = 18)]
    nested_array: [[u8; 2]; 3],
    #[varve(field_id = 19)]
    pair: (u32, String),
    #[varve(field_id = 20)]
    triple: (u8, u16, u32),
    #[varve(field_id = 21)]
    quad_tuple: (u8, u16, u32, u64),
    #[varve(field_id = 22)]
    maybe_scalar: Option<u32>,
    #[varve(field_id = 23)]
    maybe_text: Option<String>,
    #[varve(field_id = 24)]
    ordered_map: BTreeMap<u32, String>,
    #[varve(field_id = 25)]
    hash_map: HashMap<u64, u32>,
    #[varve(field_id = 26)]
    blob: ChunkedBytes,
    #[varve(field_id = 27)]
    mask: PackedBitmap,
    #[varve(field_id = 28)]
    unit: (),
}

// ---------------------------------------------------------------------------
// The same surface through `varve_format!`, stored in a real file.
// ---------------------------------------------------------------------------

varve_format! {
    pub format PublicApiFormat {
        magic: b"PAPI";
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
        extension: "papi";
        dims {
            scan: u32,
            ch: u32,
        }
        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
        };
        blocks {
            fixed Sample(id = 10, version = 1) {
                a: u32,
                b: f64,
                tag: [u8; 4],
            }

            variable Record(id = 11, version = 1, key = [id]) {
                id: u64,
                name: String,
                payload: Vec<u8>,
                readings: Vec<f64>,
                labels: Vec<String>,
                lookup: BTreeMap<u32, String>,
                optional: Option<u32>,
                blob: ChunkedBytes,
                mask: PackedBitmap,
            }

            // Matrix slots must be fixed-width scalar or fixed-array types
            // (docs/api-reference.md, `matrix_field_is_fixed_width`). A
            // `PackedBitmap` slot is deliberately NOT expressible here: it owns
            // heap storage and has no compile-time stride. The negative case is
            // covered by the workspace trybuild fixtures
            // `fail_matrix_packed_bitmap_field.rs` and
            // `fail_matrix_shadowed_packed_bitmap.rs`.
            matrix Cell(id = 12, dims = [scan, ch], category = analysis) {
                value: u32,
                extra: [u16; 2],
            }
        }
    }
}

fn sample_chunked() -> ChunkedBytes {
    let payload: Vec<u8> = (0..600u32).map(|index| (index % 97) as u8).collect();
    ChunkedBytes::from_zstd_chunks(&payload, 64, CompressionLevel::Fast)
        .expect("ChunkedBytes::from_zstd_chunks is a documented public constructor")
}

fn sample_bitmap() -> PackedBitmap {
    let mut mask = PackedBitmap::new(37).expect("PackedBitmap::new is a documented constructor");
    for ordinal in [0u64, 5, 8, 31, 36] {
        mask.set(ordinal, true).expect("bit ordinal is in range");
    }
    mask
}

fn every_variable_codec() -> EveryVariableCodec {
    let mut ordered_map = BTreeMap::new();
    ordered_map.insert(1u32, "one".to_string());
    ordered_map.insert(2u32, "two".to_string());
    let mut hash_map = HashMap::new();
    hash_map.insert(7u64, 70u32);
    hash_map.insert(9u64, 90u32);
    EveryVariableCodec {
        id: 42,
        text: "public surface".to_string(),
        bytes: vec![1, 2, 3, 4],
        bools: vec![true, false, true],
        i8s: vec![-1, 2, -3],
        u16s: vec![1, 2, 3],
        i16s: vec![-1, 2, -3],
        u32s: vec![1, 2, 3],
        i32s: vec![-1, 2, -3],
        u64s: vec![1, 2, 3],
        i64s: vec![-1, 2, -3],
        u128s: vec![1, 2, 3],
        i128s: vec![-1, 2, -3],
        f32s: vec![1.5, -2.25],
        f64s: vec![1.5, -2.25],
        strings: vec!["a".to_string(), "b".to_string()],
        fixed_array: [1, 2, 3, 4],
        nested_array: [[1, 2], [3, 4], [5, 6]],
        pair: (5, "pair".to_string()),
        triple: (1, 2, 3),
        quad_tuple: (1, 2, 3, 4),
        maybe_scalar: Some(11),
        maybe_text: Some("some".to_string()),
        ordered_map,
        hash_map,
        blob: sample_chunked(),
        mask: sample_bitmap(),
        unit: (),
    }
}

/// Value-level coverage of every documented codec, independent of any block.
fn check_every_public_codec() {
    println!("public codec roundtrips:");
    roundtrip("()", ());
    roundtrip("bool", true);
    roundtrip("u8", 0xABu8);
    roundtrip("i8", -5i8);
    roundtrip("u16", 0xABCDu16);
    roundtrip("i16", -5i16);
    roundtrip("u32", 0xABCD_EF01u32);
    roundtrip("i32", -5i32);
    roundtrip("u64", 0xABCD_EF01_2345_6789u64);
    roundtrip("i64", -5i64);
    roundtrip("u128", u128::MAX / 3);
    roundtrip("i128", -5i128);
    roundtrip("f32", 1.5f32);
    roundtrip("f64", -2.25f64);
    roundtrip("String", "text".to_string());
    roundtrip("Vec<u8>", vec![1u8, 2, 3]);
    roundtrip("Vec<bool>", vec![true, false]);
    roundtrip("Vec<i8>", vec![-1i8, 2]);
    roundtrip("Vec<u16>", vec![1u16, 2]);
    roundtrip("Vec<i16>", vec![-1i16, 2]);
    roundtrip("Vec<u32>", vec![1u32, 2]);
    roundtrip("Vec<i32>", vec![-1i32, 2]);
    roundtrip("Vec<u64>", vec![1u64, 2]);
    roundtrip("Vec<i64>", vec![-1i64, 2]);
    roundtrip("Vec<u128>", vec![1u128, 2]);
    roundtrip("Vec<i128>", vec![-1i128, 2]);
    roundtrip("Vec<f32>", vec![1.5f32, -2.5]);
    roundtrip("Vec<f64>", vec![1.5f64, -2.5]);
    roundtrip("Vec<String>", vec!["a".to_string()]);
    roundtrip("[u32; 4]", [1u32, 2, 3, 4]);
    roundtrip("[[u8; 2]; 3]", [[1u8, 2], [3, 4], [5, 6]]);
    roundtrip("(u32, String)", (1u32, "a".to_string()));
    roundtrip("(u8, u16, u32)", (1u8, 2u16, 3u32));
    roundtrip("(u8, u16, u32, u64)", (1u8, 2u16, 3u32, 4u64));
    roundtrip("Option<u32>::Some", Some(7u32));
    roundtrip("Option<u32>::None", Option::<u32>::None);
    roundtrip("Option<String>", Some("s".to_string()));
    roundtrip(
        "BTreeMap<u32, String>",
        BTreeMap::from([(1u32, "a".to_string()), (2, "b".to_string())]),
    );
    roundtrip("HashMap<u64, u32>", HashMap::from([(1u64, 2u32)]));
    // The two built-ins whose missing identities were the regression.
    roundtrip("ChunkedBytes", sample_chunked());
    roundtrip("PackedBitmap", sample_bitmap());
}

/// Block-level coverage: the derive macro's identity gate runs over every field
/// of these two types, so they compiling at all is the regression test.
fn check_derived_blocks() {
    println!("derived block roundtrips:");
    let fixed = FixedScalars {
        flag: true,
        a_u8: 1,
        a_i8: -1,
        a_u16: 2,
        a_i16: -2,
        a_u32: 3,
        a_i32: -3,
        a_u64: 4,
        a_i64: -4,
        a_u128: 5,
        a_i128: -5,
        a_f32: 1.5,
        a_f64: -2.25,
        quad: [1, 2, 3, 4],
        grid: [[1, 2], [3, 4], [5, 6]],
    };
    let encoded = encode_to_vec(&fixed, Endian::Little).expect("encode fixed block");
    let decoded: FixedScalars = decode_from_slice(&encoded, Endian::Little).expect("decode fixed");
    assert_eq!(decoded, fixed);
    assert_ne!(FixedScalars::SCHEMA_FINGERPRINT, 0);
    println!(
        "  block ok: FixedScalars ({} fields)",
        FixedScalars::FIELDS.len()
    );

    let variable = every_variable_codec();
    let encoded = encode_to_vec(&variable, Endian::Little).expect("encode variable block");
    let decoded: EveryVariableCodec =
        decode_from_slice(&encoded, Endian::Little).expect("decode variable");
    assert_eq!(decoded, variable);
    assert_eq!(
        decoded
            .blob
            .decode_to_vec()
            .expect("decode chunked payload")
            .len(),
        600
    );
    assert!(decoded.mask.get(5).expect("bit in range"));
    assert!(!decoded.mask.get(6).expect("bit in range"));
    assert_ne!(EveryVariableCodec::SCHEMA_FINGERPRINT, 0);
    println!(
        "  block ok: EveryVariableCodec ({} fields)",
        EveryVariableCodec::FIELDS.len()
    );
}

/// File-level coverage: the same public types stored and read back through a
/// generated format, including a matrix block.
fn check_format_roundtrip(dir: &std::path::Path) {
    println!("format file roundtrip:");
    let spec = PublicApiFormat::spec();
    assert_ne!(spec.computed_schema_hash(), 0);
    let path = dir.join("public-api.papi");

    let sample = Sample {
        a: 7,
        b: 2.5,
        tag: [1, 2, 3, 4],
    };
    let record = Record {
        id: 9,
        name: "record".to_string(),
        payload: vec![9, 8, 7],
        readings: vec![1.5, 2.5],
        labels: vec!["x".to_string()],
        lookup: BTreeMap::from([(1u32, "one".to_string())]),
        optional: Some(3),
        blob: sample_chunked(),
        mask: sample_bitmap(),
    };
    let cell = Cell {
        value: 1234,
        extra: [7, 8],
    };

    let key = CellKey { scan: 1, ch: 2 };
    {
        // The generated, format-first surface, which is what the docs put in
        // front of users.
        let mut writer =
            PublicApiFormat::create_writer_with_dims(&path, PublicApiFormatDims { scan: 4, ch: 4 })
                .expect("create matrix-bearing file");
        writer.push_sample(&sample).expect("push fixed block");
        writer.push_record(&record).expect("push variable block");
        writer.write_cell(key, &cell).expect("write matrix cell");
        writer.commit_cell(key).expect("commit matrix cell");
        writer.flush().expect("flush");
    }

    // Matrix reads take `&self`, so this binding no longer needs `mut`.
    let reader = PublicApiFormat::open_reader(&path).expect("reopen");
    let samples: Vec<Sample> = reader
        .samples()
        .expect("fixed collection")
        .iter()
        .collect::<Result<_, _>>()
        .expect("read fixed blocks");
    assert_eq!(samples, vec![sample]);
    // A keyed variable block reads back through `KeyedBlockVec`, so both the
    // keyed lookup and the underlying block view are exercised.
    let keyed = reader.records().expect("variable collection");
    assert_eq!(
        keyed.get(&record.id).expect("keyed lookup"),
        Some(record.clone())
    );
    let records: Vec<Record> = keyed
        .as_blocks()
        .iter()
        .collect::<Result<_, _>>()
        .expect("read variable blocks");
    assert_eq!(records, vec![record]);
    assert_eq!(reader.cell(key).expect("read matrix cell"), cell);
    println!("  file ok: fixed + variable + matrix blocks round-tripped");
}

fn main() {
    check_every_public_codec();
    check_derived_blocks();

    let dir = std::env::temp_dir().join(format!("varve-public-api-fixture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture temp dir");
    let result = std::panic::catch_unwind(|| check_format_roundtrip(&dir));
    let cleanup = std::fs::remove_dir_all(&dir);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    cleanup.expect("fixture temp dir cleanup");

    println!("public-api fixture OK");
}
