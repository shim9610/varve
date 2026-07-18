use varve::{BatchOptions, varve_format};

varve_format! {
    pub format PetabyteChainStreamFormat {
        magic: b"PTCS";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            variable Frame(id = 1, key = [id], key_index = disk) {
                id: u64,
                payload: Vec<u8>,
            }
        }
    }
}

fn keyed_stream_batch_is_not_exposed(
    writer: &mut PetabyteChainStreamFormatStreamWriter,
    frames: &[Frame],
) {
    let _ = writer.push_frames(frames, BatchOptions::default());
}

fn main() {}
