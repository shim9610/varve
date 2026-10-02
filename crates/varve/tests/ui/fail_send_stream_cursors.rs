use varve::{StreamEvents, StreamingBlocks, varve_format};
varve_format! {
    pub format Feed {
        magic: b"SEND";
        version: 1;
        schema_hash: computed;
        index: block_offset_chain;
        blocks { fixed Sample(id = 1) { value: u64 } }
    }
}
fn send_events(cursor: StreamEvents) {
    std::thread::spawn(move || { for event in cursor { event.unwrap(); } });
}
fn send_blocks(cursor: StreamingBlocks<Sample>) {
    std::thread::spawn(move || { for value in cursor { value.unwrap(); } });
}
fn main() {}
