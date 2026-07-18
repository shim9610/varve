use varve::{varve_format, DiskIndexOptions, StreamOptions, VarveKeyedBlock};

varve_format! {
    pub format HighCardinalityFormat {
        magic: b"HCIX";
        version: 1;
        blocks {
            fixed Account(id = 1, key = [id], key_index = memory) {
                id: u64,
                active: bool,
            }

            variable Frame(id = 2, key = [scan, frame], key_index = disk) {
                scan: u32,
                frame: u32,
                payload: Vec<u8>,
            }
        }
    }
}

fn stream_reader_api(reader: &HighCardinalityFormatStreamReader) -> varve::Result<()> {
    let _events = reader.events()?;
    let _accounts = reader.accounts()?;
    let _frames = reader.frames()?;
    let _state = reader.resident_state();
    Ok(())
}

fn stream_writer_api(writer: &mut HighCardinalityFormatStreamWriter) -> varve::Result<()> {
    let account = Account {
        id: 1,
        active: true,
    };
    let frame = Frame {
        scan: 2,
        frame: 3,
        payload: vec![4],
    };
    writer.push_account(&account)?;
    writer.delete_account(&account.id)?;
    writer.push_frame(&frame)?;
    let frame_key = frame.key();
    writer.delete_frame(&frame_key)?;
    writer.flush()?;
    writer.sync()?;
    let _state = writer.resident_state();
    Ok(())
}

fn indexed_api(
    reader: &HighCardinalityFormatIndexedReader,
    writer: &mut HighCardinalityFormatIndexedWriter,
) -> varve::Result<()> {
    let key = (2, 3);
    let _frame = reader.get_frame(&key)?;
    let frame = Frame {
        scan: key.0,
        frame: key.1,
        payload: Vec::new(),
    };
    writer.push_frame(&frame)?;
    writer.delete_frame(&key)?;
    writer.flush()?;
    writer.sync()?;
    Ok(())
}

fn constructors(path: &std::path::Path) {
    let _ = HighCardinalityFormat::open_stream_reader(path, StreamOptions::default());
    let _ = HighCardinalityFormat::create_stream_writer(path, StreamOptions::default());
    let _ = HighCardinalityFormat::open_stream_writer(path, StreamOptions::default());
    let _ = HighCardinalityFormat::open_indexed_reader(path, DiskIndexOptions::default());
    let _ = HighCardinalityFormat::create_indexed_writer(path, DiskIndexOptions::default());
    let _ = HighCardinalityFormat::open_indexed_writer(path, DiskIndexOptions::default());
}

fn public_index_types(digest: varve::DiskIndexDigest) -> varve::DiskIndexMode {
    let mode = varve::DiskIndexMode::DiskPlan(digest);
    assert_eq!(mode.plan_digest(), Some(digest));
    mode
}

fn main() {
    let _ = (
        stream_reader_api,
        stream_writer_api,
        indexed_api,
        constructors,
        public_index_types,
    );
}
