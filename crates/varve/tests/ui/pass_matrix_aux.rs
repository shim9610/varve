use varve::{MatrixCellStatus, ReadLimits, varve_format};

varve_format! {
    pub format MatrixAuxFormat {
        magic: b"MAUX";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            record_payload: 67_108_864;
            materialized_bytes: 1_073_741_824;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
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
            thumbnail: 128,
        }

        blocks {
            matrix AuxCell(id = 20, dims = [scan, ch], category = analysis) {
                value: u32,
            }
        }
    }
}

fn assert_reader_api<R: MatrixAuxFormatRead>(reader: &mut R) -> varve::Result<()> {
    let _ = reader.thumbnail_aux_len()?;
    let _ = reader.read_thumbnail_aux(0, 4)?;
    let _ = reader.aux_cell(AuxCellKey { scan: 0, ch: 0 });
    Ok(())
}

fn assert_writer_api<W: MatrixAuxFormatWrite>(writer: &mut W) -> varve::Result<()> {
    let key = AuxCellKey { scan: 0, ch: 0 };
    let _ = writer.thumbnail_aux_len()?;
    let _ = writer.read_thumbnail_aux(0, 4)?;
    writer.write_thumbnail_aux(0, &[1, 2, 3, 4])?;
    writer.write_aux_cell(key, &AuxCell { value: 7 })?;
    writer.commit_aux_cell(key)?;
    let _ = writer.clear_analysis_category()?;
    let status: MatrixCellStatus = writer.aux_cell_status(key)?;
    let _ = status;
    Ok(())
}

fn main() {
    let spec = MatrixAuxFormat::spec();
    assert_eq!(spec.matrix_aux.len(), 1);
    assert_eq!(spec.matrix_aux[0].name, "thumbnail");
    assert_eq!(spec.matrix_aux[0].byte_len, 128);
    assert_ne!(spec.schema_hash, 0);

    fn _typecheck_matrix_limit_boundaries(path: &std::path::Path) {
        let dims = MatrixAuxFormatDims { scan: 2, ch: 2 };
        let limits = ReadLimits::finite_all(4096);
        let _ = MatrixAuxFormat::create_writer_with_dims_and_limits(path, dims, limits);
        let _ = MatrixAuxFormat::create_writer_with_dims_trusted_unbounded(path, dims);
    }
    let _ = _typecheck_matrix_limit_boundaries;
}
