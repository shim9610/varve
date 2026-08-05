//! F33: a `#[varve(key = "...")]` naming a raw-identifier field must not make
//! the derive panic.
//!
//! `parse_key_fields` used to rebuild the parsed identifier with
//! `Ident::new(&ident.to_string(), span)`, and `Ident::new` panics on a raw
//! identifier ("r#type" is not a valid identifier). Both surfaces below route
//! through that one function: the derive attribute directly, and the DSL by
//! round-tripping its `key: [...]` list back out as a `#[varve(key = "...")]`
//! attribute string.
//!
//! Before the fix this whole test binary fails to build with
//! `error: proc-macro derive panicked / = help: message: "r#type" is not a
//! valid identifier`, so the compile failure is itself the finding.

use varve::{VarveBlock, VarveKeyedBlock, varve_format};

/// Surface 1: the derive attribute.
#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 771, version = 1, kind = "variable", key = "r#type")]
struct DerivedRawKey {
    #[varve(field_id = 1)]
    label: String,
    #[varve(field_id = 2)]
    r#type: u64,
}

// Surface 2: the DSL, which re-emits the key list as a string attribute.
varve_format! {
    pub format RawIdentKeyFormat {
        magic: b"RAWK";
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
        }
        endian: little;
        schema_hash: computed;
        index: [scan_on_open, block_offset_chain, keyed_offset_chain];
        commit: transaction_marker(on_flush);
        blocks {
            variable DslRawKey(id = 772, key = [r#type]) {
                label: String,
                r#type: u64,
            }
        }
    }
}

/// The key must be the `r#type` field, not the first field and not a field
/// named `type` with the prefix stripped (no such field exists).
#[test]
fn derived_raw_identifier_key_binds_the_raw_field() {
    let block = DerivedRawKey {
        label: "first".to_string(),
        r#type: 41,
    };
    assert_eq!(VarveKeyedBlock::key(&block), 41u64);
    const { assert!(<DerivedRawKey as VarveBlock>::IS_KEYED) };

    let other = DerivedRawKey {
        label: "second".to_string(),
        r#type: 42,
    };
    assert_ne!(VarveKeyedBlock::key(&block), VarveKeyedBlock::key(&other));
}

#[test]
fn dsl_raw_identifier_key_survives_the_attribute_round_trip() -> varve::Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("raw_ident_key.vrv");

    {
        let mut writer = RawIdentKeyFormat::create_writer(&path)?;
        writer.push_dsl_raw_key(&DslRawKey {
            label: "alpha".to_string(),
            r#type: 7,
        })?;
        writer.push_dsl_raw_key(&DslRawKey {
            label: "beta".to_string(),
            r#type: 9,
        })?;
        writer.commit()?;
        writer.flush()?;
    }

    let reader = RawIdentKeyFormat::open_reader(&path)?;
    let block = DslRawKey {
        label: "alpha".to_string(),
        r#type: 7,
    };
    assert_eq!(VarveKeyedBlock::key(&block), 7u64);

    let keyed = reader.into_inner().keyed_blocks::<DslRawKey>()?;
    assert_eq!(keyed.len(), 2);
    let mut keys: Vec<u64> = keyed.keys().copied().collect();
    keys.sort_unstable();
    assert_eq!(keys, vec![7u64, 9u64]);
    // Keyed lookup by the raw field's value finds the right record.
    assert_eq!(
        keyed
            .get(&7u64)?
            .expect("record keyed by r#type == 7")
            .label,
        "alpha"
    );
    assert_eq!(
        keyed
            .get(&9u64)?
            .expect("record keyed by r#type == 9")
            .label,
        "beta"
    );

    Ok(())
}
