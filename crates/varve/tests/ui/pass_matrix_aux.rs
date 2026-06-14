use varve::{MatrixCellStatus, VarveBlock, varve_format};

varve_format! {
    pub format MatrixAuxFormat {
        magic: b"MAUX";
        version: 1;
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
}
