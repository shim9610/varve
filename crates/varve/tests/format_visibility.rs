//! F36: a format declared `pub(crate)` generates `pub(crate)` handles.
//!
//! The token-level discriminator for this lives in `varve-macros`
//! (`declared_visibility_reaches_the_generated_record_api` and its layout
//! sibling), because a *narrower* visibility never enables anything a test can
//! call — the failure it removes is an over-broad export, which only a
//! compile-fail case can observe directly.
//!
//! What this file proves is the other half, and the half the change could
//! plausibly break: narrowing every generated item at once must still compile
//! and still work. `#vis fn create_writer` returns `#vis struct ScopedWriter`,
//! `#vis struct ScopedLayoutChunkLayoutWrite` has `pub` fields of `#vis` field
//! structs, and each is a place `error[E0446]` (private type in public
//! interface) would fire if some type had been left behind at `pub`.

#![deny(private_interfaces, private_bounds)]

mod inner {
    // NOTE: `#![deny(unreachable_pub)]` was tried here as a discriminator and
    // MEASURED to be inert — rustc suppresses that lint inside an external
    // macro's expansion, so it stays silent whether the generated items are
    // `pub` or `pub(crate)`. The discriminator is the token-level unit test in
    // `varve-macros` instead.
    use varve::varve_format;

    varve_format! {
        pub(crate) format Scoped {
            magic: b"SCPD";
            version: 1;
            limits {
                file_len: 1_073_741_824;
                records: 100_000;
                index_bytes: 16_777_216;
                scan_bytes: 1_073_741_824;
                record_payload: 1_048_576;
                logical_payload: 4_194_304;
                materialized_bytes: 16_777_216;
                segments: 100_000;
            }
            endian: little;
            schema_hash: computed;
            index: [scan_on_open, block_offset_chain, keyed_offset_chain];
            commit: transaction_marker(on_flush);
            blocks {
                variable ScopedUser(id = 881, key = [id]) {
                    id: u64,
                    name: String,
                }
            }
        }
    }

    varve_format! {
        pub(crate) format ScopedLayout {
            magic: b"SCPL";
            version: 1;
            limits {
                file_len: 1_073_741_824;
                records: 100_000;
                index_bytes: 16_777_216;
                scan_bytes: 1_073_741_824;
                record_payload: 1_048_576;
                logical_payload: 4_194_304;
                materialized_bytes: 16_777_216;
                segments: 100_000;
            }
            endian: little;
            schema_hash: computed;
            extension: "scpl";
            preset: none;

            layout {
                file_header ScopedFileHeader {
                    bytes signature = b"SCP!";
                    u16 header_version = 1;
                }

                segment Chunk repeat until_eof {
                    lead_in ChunkLeadIn {
                        bytes tag = b"CHNK";
                        u32 kind;
                        i64 next_segment_offset =
                            finalize(target = segment_end, relative_to = after_lead_in);
                        i64 raw_data_offset =
                            finalize(target = raw_region_start, relative_to = after_lead_in);
                    }

                    metadata ChunkMetadata;
                    raw_region ChunkRaw;
                }
            }
        }
    }
}

// Every one of these paths is `pub(crate)` after the fix, so naming them here
// is the proof that the narrowed items are still usable from the rest of the
// crate — and that the reader/writer return types line up with the handles.
use inner::{
    Scoped, ScopedLayout, ScopedLayoutChunkLayoutFields, ScopedLayoutChunkLayoutWrite, ScopedRead,
    ScopedUser, ScopedWrite,
};

/// Generic over the generated read trait: the trait must be nameable at the
/// same visibility as the reader that implements it.
fn latest_name<R: ScopedRead>(reader: &R, key: u64) -> varve::Result<Option<String>> {
    Ok(reader.scoped_users()?.get(&key)?.map(|user| user.name))
}

#[test]
fn scoped_format_handles_are_usable_within_the_crate() -> varve::Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("scoped.vrv");

    {
        let mut writer: inner::ScopedWriter = Scoped::create_writer(&path)?;
        ScopedWrite::push_scoped_user(
            &mut writer,
            &ScopedUser {
                id: 1,
                name: "alpha".to_string(),
            },
        )?;
        writer.push_scoped_user(&ScopedUser {
            id: 2,
            name: "beta".to_string(),
        })?;
        writer.flush()?;
    }

    let reader: inner::ScopedReader = Scoped::open_reader(&path)?;
    assert_eq!(latest_name(&reader, 1)?.as_deref(), Some("alpha"));
    assert_eq!(latest_name(&reader, 2)?.as_deref(), Some("beta"));
    assert_eq!(latest_name(&reader, 3)?, None);

    Ok(())
}

#[test]
fn scoped_layout_handles_are_usable_within_the_crate() -> varve::Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("scoped_layout.scpl");

    {
        let mut writer: inner::ScopedLayoutLayoutWriter =
            ScopedLayout::create_layout_writer(&path)?;
        // `ScopedLayoutChunkLayoutWrite` carries `pub` fields whose types are the
        // narrowed field structs; constructing it here is the E0446 check.
        writer.write_chunk(ScopedLayoutChunkLayoutWrite {
            fields: ScopedLayoutChunkLayoutFields { kind: 3 },
            footer_fields: inner::ScopedLayoutChunkLayoutFooterFields,
            metadata: b"meta",
            raw: b"raw",
        })?;
        writer.write_chunk_streamed(
            ScopedLayoutChunkLayoutFields { kind: 4 },
            inner::ScopedLayoutChunkLayoutFooterFields,
            |out| {
                out.write_all(b"meta2")?;
                Ok(())
            },
            |out| {
                out.write_all(b"raw2")?;
                Ok(())
            },
        )?;
        writer.flush()?;
    }

    let reader: inner::ScopedLayoutLayoutReader = ScopedLayout::open_layout_reader(&path)?;
    let first: inner::ScopedLayoutChunkLayoutInfo =
        reader.chunk(0)?.expect("first chunk is present");
    assert_eq!(first.kind()?, 3);
    assert_eq!(reader.read_chunk_metadata(0)?, b"meta");
    assert_eq!(reader.read_chunk_raw(0)?, b"raw");
    assert_eq!(reader.chunk(1)?.expect("second chunk").kind()?, 4);
    assert_eq!(reader.read_chunk_raw(1)?, b"raw2");
    assert!(reader.chunk(2)?.is_none());

    Ok(())
}
