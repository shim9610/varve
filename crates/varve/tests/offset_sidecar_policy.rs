//! The offset sidecar's declaration surface, and the inertness of not using it.
//!
//! The sidecar is a derived cache (see `.internal-docs/offset-sidecar-spec.md`
//! §1). Two properties follow, and this file is where they are held:
//!
//! - **A format that does not declare it pays nothing and changes nothing** —
//!   including its computed schema hash, which is stored in newly created
//!   files, so a changed hash would be a wire change for a format that changed
//!   nothing.
//! - **A format that does declare it changes no byte of the main file either.**
//!   The scope is not part of the format's identity, because a file's identity
//!   must not depend on a cache. Concretely: a handle that declared a sidecar
//!   and one that did not must produce the same schema hash, or they could not
//!   read each other's files.

use varve::{IndexPolicy, OffsetSidecarScope, VarveBlock, varve_format};

varve_format! {
    pub format PlainFormat {
        magic: b"SCP0";
        version: 1;
        endian: little;
        schema_hash: computed;
        index: [scan_on_open, checkpoint_on_flush];
        commit: transaction_marker(on_flush);
        blocks {
            fixed Sample(id = 1) {
                a: u32,
            }
            variable Note(id = 2) {
                text: String,
            }
        }
    }
}

varve_format! {
    pub format UnifiedFormat {
        magic: b"SCP0";
        version: 1;
        endian: little;
        schema_hash: computed;
        index: [scan_on_open, checkpoint_on_flush, offset_sidecar];
        commit: transaction_marker(on_flush);
        blocks {
            fixed Sample2(id = 1) {
                a: u32,
            }
            variable Note2(id = 2) {
                text: String,
            }
        }
    }
}

varve_format! {
    pub format PerBlockFormat {
        magic: b"SCP1";
        version: 1;
        endian: little;
        schema_hash: computed;
        index: [scan_on_open, offset_sidecar(per_block)];
        commit: transaction_marker(on_flush);
        blocks {
            fixed Sample3(id = 1) {
                a: u32,
            }
            variable Note3(id = 2) {
                text: String,
            }
        }
    }
}

varve_format! {
    pub format SelectedBlocksFormat {
        magic: b"SCP2";
        version: 1;
        endian: little;
        schema_hash: computed;
        index: [scan_on_open, offset_sidecar(blocks(Note4))];
        commit: transaction_marker(on_flush);
        blocks {
            fixed Sample4(id = 1) {
                a: u32,
            }
            variable Note4(id = 2) {
                text: String,
            }
        }
    }
}

#[test]
fn an_undeclared_format_defaults_to_no_sidecar() {
    let spec = PlainFormat::spec();
    assert_eq!(
        spec.index_policy.offset_sidecar,
        OffsetSidecarScope::None,
        "a new option must default off"
    );
    assert!(!spec.index_policy.offset_sidecar.is_enabled());
    assert_eq!(
        spec.index_policy,
        IndexPolicy::new(true, true, false, false),
        "the four-argument constructor must still describe an undeclared format \
         exactly, or every existing caller silently changed meaning"
    );
}

#[test]
fn the_dsl_records_each_of_the_three_scopes() {
    assert_eq!(
        UnifiedFormat::spec().index_policy.offset_sidecar,
        OffsetSidecarScope::Unified
    );
    assert_eq!(
        PerBlockFormat::spec().index_policy.offset_sidecar,
        OffsetSidecarScope::PerBlock
    );
    assert_eq!(
        SelectedBlocksFormat::spec().index_policy.offset_sidecar,
        OffsetSidecarScope::Blocks(&[Note4::ID])
    );
}

#[test]
fn a_unified_scope_does_not_claim_to_serve_a_block() {
    // Unified is a single array in append order, so it cannot answer "the j-th
    // record of block B" without the block filter. Saying otherwise here would
    // let a typed collection be served from an array that does not index it.
    let unified = OffsetSidecarScope::Unified;
    assert!(unified.is_enabled());
    assert!(!unified.covers_block(1));
    assert!(!unified.covers_block(2));

    assert!(OffsetSidecarScope::PerBlock.covers_block(1));
    assert!(OffsetSidecarScope::PerBlock.covers_block(2));

    let selected = OffsetSidecarScope::Blocks(&[2]);
    assert!(!selected.covers_block(1));
    assert!(selected.covers_block(2));

    assert!(!OffsetSidecarScope::None.is_enabled());
    assert!(!OffsetSidecarScope::None.covers_block(1));
}

#[test]
fn declaring_a_sidecar_does_not_change_the_schema_hash() {
    // Derived from ONE spec by changing only the scope. Two separately declared
    // formats would differ by block name as well -- names are hashed -- so they
    // cannot isolate the variable under test.
    let base = PlainFormat::spec();
    let baseline = base.computed_schema_hash();
    for scope in [
        OffsetSidecarScope::Unified,
        OffsetSidecarScope::PerBlock,
        OffsetSidecarScope::Blocks(&[2]),
    ] {
        let mut spec = base;
        spec.index_policy = spec.index_policy.with_offset_sidecar(scope);
        assert_eq!(
            spec.computed_schema_hash(),
            baseline,
            "the sidecar scope must not be part of the format's identity: if it \
             were, a reader that declared {scope:?} could not open a file written \
             by a reader that declared none"
        );
    }
}

#[test]
fn a_sidecar_scope_naming_an_undeclared_block_is_refused() {
    let mut spec = PlainFormat::spec();
    spec.index_policy = spec
        .index_policy
        .with_offset_sidecar(OffsetSidecarScope::Blocks(&[999]));
    assert!(
        spec.validate().is_err(),
        "a block id no block declares would open a sidecar nothing ever writes"
    );
}

#[test]
fn an_empty_or_repeated_sidecar_block_list_is_refused() {
    let mut spec = PlainFormat::spec();
    spec.index_policy = spec
        .index_policy
        .with_offset_sidecar(OffsetSidecarScope::Blocks(&[]));
    assert!(spec.validate().is_err(), "an empty list declares nothing");

    let mut spec = PlainFormat::spec();
    spec.index_policy = spec
        .index_policy
        .with_offset_sidecar(OffsetSidecarScope::Blocks(&[1, 1]));
    assert!(
        spec.validate().is_err(),
        "a repeated id would open one sidecar path twice, so two write buffers \
         would race for one array"
    );
}

#[test]
fn every_declared_scope_still_validates() {
    for spec in [
        PlainFormat::spec(),
        UnifiedFormat::spec(),
        PerBlockFormat::spec(),
        SelectedBlocksFormat::spec(),
    ] {
        spec.validate().expect("declared spec must validate");
    }
}
