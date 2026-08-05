//! F32: every generated matrix read entry point takes `&self`.
//!
//! The core handles it delegates to (`VarveReader::read_matrix_cell`,
//! `matrix_aux_len`, `read_matrix_aux`) are all `&self`; the generated typed
//! wrapper took `&mut self` for the cell read and the aux read, on the inherent
//! impl AND in the trait declaration. Every function in this file borrows the
//! reader shared, so none of it compiles unless the receivers are `&self`.

use std::path::PathBuf;

use varve::{MatrixCellStatus, varve_format};

varve_format! {
    pub format SharedMatrixFormat {
        magic: b"SHMX";
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
        dims {
            scan: u32,
            ch: u32,
        }
        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
        };
        aux {
            thumbnail: 16,
        }
        blocks {
            matrix SharedCell(id = 701, dims = [scan, ch], category = analysis) {
                value: u32,
            }
        }
    }
}

const SCANS: u64 = 4;
const CHANNELS: u64 = 4;

/// (a) The inherent cell read, reached through a shared reference.
///
/// Before the fix this is `error[E0596]: cannot borrow *reader as mutable, as
/// it is behind a & reference`.
fn read_via_generated(
    reader: &SharedMatrixFormatReader,
    key: SharedCellKey,
) -> varve::Result<SharedCell> {
    reader.shared_cell(key)
}

/// (a) The inherent aux read, reached through a shared reference.
fn read_aux_via_generated(reader: &SharedMatrixFormatReader) -> varve::Result<Vec<u8>> {
    reader.read_thumbnail_aux(2, 3)
}

/// (b) The same two reads through the generated READ TRAIT, so the trait
/// declaration is exercised and not just the inherent impl. A fix that relaxed
/// only `MatrixMethodTarget::Inherent` still fails to compile here.
fn read_via_trait<R: SharedMatrixFormatRead>(
    reader: &R,
    key: SharedCellKey,
) -> varve::Result<SharedCell> {
    reader.shared_cell(key)
}

fn read_aux_via_trait<R: SharedMatrixFormatRead>(reader: &R) -> varve::Result<Vec<u8>> {
    reader.read_thumbnail_aux(2, 3)
}

fn status_via_trait<R: SharedMatrixFormatRead>(
    reader: &R,
    key: SharedCellKey,
) -> varve::Result<MatrixCellStatus> {
    reader.shared_cell_status(key)
}

fn expected_value(scan: u64, ch: u64) -> u32 {
    (scan * 100 + ch) as u32
}

fn build(path: &PathBuf) -> varve::Result<()> {
    let mut writer = SharedMatrixFormat::create_writer_with_dims(
        path,
        SharedMatrixFormatDims {
            scan: SCANS as u32,
            ch: CHANNELS as u32,
        },
    )?;
    writer.write_thumbnail_aux(2, &[5, 6, 7])?;
    for scan in 0..SCANS {
        for ch in 0..CHANNELS {
            let key = SharedCellKey { scan, ch };
            writer.write_shared_cell(
                key,
                &SharedCell {
                    value: expected_value(scan, ch),
                },
            )?;
            writer.commit_shared_cell(key)?;
        }
    }
    writer.flush()?;
    Ok(())
}

#[test]
fn generated_matrix_reads_take_shared_self() -> varve::Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("shared_matrix.vrv");
    build(&path)?;

    // The handle is NOT `mut`. Nothing below may require exclusivity.
    let reader = SharedMatrixFormat::open_reader(&path)?;

    for scan in 0..SCANS {
        for ch in 0..CHANNELS {
            let key = SharedCellKey { scan, ch };
            assert_eq!(
                read_via_generated(&reader, key)?,
                SharedCell {
                    value: expected_value(scan, ch)
                }
            );
            assert_eq!(
                read_via_trait(&reader, key)?,
                SharedCell {
                    value: expected_value(scan, ch)
                }
            );
            assert_eq!(status_via_trait(&reader, key)?, MatrixCellStatus::Committed);
        }
    }

    assert_eq!(read_aux_via_generated(&reader)?, vec![5, 6, 7]);
    assert_eq!(read_aux_via_trait(&reader)?, vec![5, 6, 7]);
    assert_eq!(reader.thumbnail_aux_len()?, 16);

    Ok(())
}

/// (c) One generated reader shared across four threads, every thread reading
/// every cell. This needs `&self` twice over: the borrow checker for the shared
/// reference, and `Sync` for the handle.
#[test]
fn one_generated_reader_serves_concurrent_matrix_readers() -> varve::Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("shared_matrix_threads.vrv");
    build(&path)?;

    let reader = SharedMatrixFormat::open_reader(&path)?;
    let reader_ref = &reader;

    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(move || {
                for scan in 0..SCANS {
                    for ch in 0..CHANNELS {
                        let key = SharedCellKey { scan, ch };
                        let cell = read_via_generated(reader_ref, key).expect("cell read");
                        assert_eq!(cell.value, expected_value(scan, ch));
                        assert_eq!(
                            read_via_trait(reader_ref, key).expect("cell read via trait"),
                            cell
                        );
                        assert_eq!(
                            read_aux_via_generated(reader_ref).expect("aux read"),
                            vec![5, 6, 7]
                        );
                    }
                }
            });
        }
    });

    Ok(())
}
