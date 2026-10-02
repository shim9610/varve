//! Shared deterministic/fuzz oracle: working, published and pinned generations are independent.
use varve::{MatrixCellStatus, MatrixKey, varve_format};
varve_format! {
    pub format Model {
        magic: b"MMDL";
        version: 1;
        integrity: crc32_with_header;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        aux { note: 8, }
        blocks { matrix Entry(id = 1, dims = [row, col], category = data) { value: u64, } }
    }
}
pub fn run(data: &[u8]) -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("model.varve");
    let mut writer = Model::create_writer_with_dims(&path, ModelDims { row: 8, col: 1 })?;
    writer.sync()?;
    let mut reader = Model::open_reader(&path)?;
    let mut working = [None; 8];
    let mut published = working;
    let mut captured = working;
    let mut working_aux = [0; 8];
    let mut published_aux = working_aux;
    let mut captured_aux = working_aux;
    for (step, op) in data.chunks(3).take(128).enumerate() {
        let row = usize::from(op.get(1).copied().unwrap_or(0) % 8);
        let key = EntryKey {
            row: row as u64,
            col: 0,
        };
        match op[0] % 10 {
            0 | 1 => {
                let value = ((step as u64) << 8) | u64::from(op.get(2).copied().unwrap_or(0));
                writer.write_entry(key, &Entry { value })?;
                writer.commit_entry(key)?;
                working[row] = Some(value);
            }
            2 => {
                writer
                    .inner_mut()
                    .clear_matrix_cell::<Entry>(MatrixKey::new(row as u64, 0))?;
                working[row] = None;
            }
            3 => {
                writer.sync()?;
                published = working;
                published_aux = working_aux;
            }
            4 => writer.flush()?,
            5 => {
                reader.follow()?;
                captured = published;
                captured_aux = published_aux;
            }
            6 => {
                drop(writer);
                writer = Model::open_writer(&path)?;
                working = published;
                working_aux = published_aux;
            }
            7 => {
                writer.inner_mut().clear_matrix_category("data")?;
                working = [None; 8];
            }
            8 => {
                writer.sync()?;
                published = working;
                published_aux = working_aux;
                writer.compact_matrix()?;
            }
            _ => {
                working_aux = (step as u64).to_le_bytes();
                writer.write_note_aux(0, &working_aux)?;
            }
        }
        let fresh = Model::open_reader(&path)?;
        for (view, model, aux) in [
            (&reader, &captured, captured_aux),
            (&fresh, &published, published_aux),
        ] {
            assert_eq!(view.read_note_aux(0, 8)?, aux, "step={step}");
            for (row, expected) in model.iter().enumerate() {
                let key = EntryKey {
                    row: row as u64,
                    col: 0,
                };
                if let Some(value) = expected {
                    assert_eq!(view.entry_status(key)?, MatrixCellStatus::Committed);
                    assert_eq!(view.entry(key)?.value, *value, "step={step} row={row}");
                } else {
                    assert_eq!(
                        view.entry_status(key)?,
                        MatrixCellStatus::NotCommitted,
                        "step={step} row={row}"
                    );
                }
            }
        }
    }
    Ok(())
}
