#![forbid(unsafe_op_in_unsafe_fn)]

use varve::{VarveFile, VarveReader};

unsafe fn map_payloads(reader: &VarveReader) {
    // SAFETY: This function's caller promises that every handle keeps the
    // backing file immutable and valid for the returned mapping's lifetime.
    let _mapping = unsafe { reader.mmap_payloads() };
}

unsafe fn map_reader_matrix(reader: &VarveReader) {
    // SAFETY: This function carries the complete backing-file contract.
    let _mapping = unsafe { reader.mmap_matrix() };
}

unsafe fn map_file_payloads(file: &VarveFile) {
    // SAFETY: This function carries the complete backing-file contract.
    let _mapping = unsafe { file.mmap_payloads() };
}

unsafe fn map_file_matrix(file: &VarveFile) {
    // SAFETY: This function carries the complete backing-file contract.
    let _mapping = unsafe { file.mmap_matrix() };
}

fn main() {
    let _ = (
        map_payloads,
        map_reader_matrix,
        map_file_payloads,
        map_file_matrix,
    );
}
