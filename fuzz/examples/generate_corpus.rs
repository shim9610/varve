use std::fs;
use std::path::Path;

use varve::{Endian, LayoutFieldValue, LayoutValue, SegmentWrite, encode_to_vec};
use varve_fuzz::{
    FuzzCell, FuzzCellKey, FuzzLayoutFormat, FuzzMatrixFormat, FuzzMatrixFormatDims,
    FuzzNativeFormat, FuzzPoint, FuzzRecord,
};

fn main() -> varve::Result<()> {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
    let scratch = std::env::temp_dir().join(format!("varve-fuzz-seeds-{}", std::process::id()));
    fs::create_dir_all(&scratch)?;

    generate_native(&corpus, &scratch)?;
    generate_codec(&corpus)?;
    generate_layout(&corpus, &scratch)?;
    generate_matrix(&corpus, &scratch)?;
    generate_sidecars(&corpus)?;
    fs::remove_dir_all(&scratch)?;
    Ok(())
}

fn generate_native(corpus: &Path, scratch: &Path) -> varve::Result<()> {
    let output = corpus.join("native_arbitrary");
    fs::create_dir_all(&output)?;
    let path = scratch.join("native.varve");
    let mut writer = FuzzNativeFormat::create_writer(&path)?;
    writer.push_fuzz_point(&FuzzPoint { x: 7, y: -11 })?;
    writer.push_fuzz_record(&FuzzRecord {
        id: 42,
        name: "seed".to_owned(),
        payload: vec![0xA5; 128],
        values: vec![-1, 0, 1],
    })?;
    writer.flush()?;
    writer.sync()?;
    drop(writer);
    fs::copy(path, output.join("valid-native.varve"))?;
    Ok(())
}

fn generate_codec(corpus: &Path) -> varve::Result<()> {
    let output = corpus.join("codec_arbitrary");
    fs::create_dir_all(&output)?;
    let point = encode_to_vec(&FuzzPoint { x: 1, y: -2 }, Endian::Little)?;
    let record = encode_to_vec(
        &FuzzRecord {
            id: 3,
            name: "codec".to_owned(),
            payload: vec![1, 2, 3, 4],
            values: vec![5, 6],
        },
        Endian::Little,
    )?;
    fs::write(output.join("point"), prefixed(0, &point))?;
    fs::write(output.join("record"), prefixed(1, &record))?;
    fs::write(
        output.join("hostile-zero-map"),
        prefixed(8, &u64::MAX.to_le_bytes()),
    )?;
    Ok(())
}

fn generate_layout(corpus: &Path, scratch: &Path) -> varve::Result<()> {
    let output = corpus.join("layout_arbitrary");
    fs::create_dir_all(&output)?;
    let path = scratch.join("layout.bin");
    let fields = [LayoutFieldValue {
        name: "kind",
        value: LayoutValue::U32(7),
    }];
    let mut writer = FuzzLayoutFormat::create_layout_writer(&path)?;
    writer.write_segment(SegmentWrite {
        name: "Data",
        fields: &fields,
        footer_fields: &[],
        metadata: b"metadata",
        raw: b"raw-payload",
    })?;
    writer.flush()?;
    writer.sync()?;
    drop(writer);
    fs::copy(path, output.join("valid-layout.bin"))?;
    Ok(())
}

fn generate_matrix(corpus: &Path, scratch: &Path) -> varve::Result<()> {
    let output = corpus.join("matrix_arbitrary");
    fs::create_dir_all(&output)?;
    let matrix = scratch.join("matrix.varve");
    let sidecar = scratch.join("matrix.sidecar");
    let mut writer = FuzzMatrixFormat::create_writer_with_dims(
        &matrix,
        FuzzMatrixFormatDims { row: 2, column: 2 },
    )?;
    let key = FuzzCellKey { row: 0, column: 0 };
    writer.write_fuzz_cell(key, &FuzzCell { value: 99 })?;
    writer.commit_fuzz_cell(key)?;
    writer.write_scratch_aux(0, b"seed")?;
    writer.flush()?;
    writer.sync()?;
    let mut writer = writer.into_inner();
    writer.write_matrix_sidecar("fuzz", &sidecar, 1, b"sidecar-seed")?;
    writer.flush()?;
    writer.sync()?;
    drop(writer);

    let matrix_bytes = fs::read(matrix)?;
    let sidecar_bytes = fs::read(sidecar)?;
    let mut envelope = Vec::with_capacity(4 + matrix_bytes.len() + sidecar_bytes.len());
    envelope.extend_from_slice(&(matrix_bytes.len() as u32).to_le_bytes());
    envelope.extend_from_slice(&matrix_bytes);
    envelope.extend_from_slice(&sidecar_bytes);
    fs::write(output.join("valid-matrix-envelope"), envelope)?;
    Ok(())
}

fn generate_sidecars(corpus: &Path) -> varve::Result<()> {
    let arbitrary = corpus.join("sidecar_arbitrary");
    fs::create_dir_all(&arbitrary)?;
    fs::write(arbitrary.join("empty"), [])?;
    fs::write(arbitrary.join("redb-like-header"), b"redb\0\0\0\0")?;
    fs::write(
        arbitrary.join("bounded-page-pattern"),
        (0..=255).cycle().take(16 * 1024).collect::<Vec<_>>(),
    )?;

    let mutation = corpus.join("sidecar_mutation");
    fs::create_dir_all(&mutation)?;
    fs::write(mutation.join("empty-synced-valid"), [0x80, 0])?;
    fs::write(mutation.join("populated-unique-synced-valid"), [0x80, 1])?;
    fs::write(mutation.join("repeated-delete-unsynced-valid"), [0x81, 7])?;
    fs::write(
        mutation.join("populated-multi-byte-mutation"),
        [0x00, 13, 0, 0, 1, 0, 64, 0xA5, 1, 0, 0xFF],
    )?;

    let machine = corpus.join("sidecar_state_machine");
    fs::create_dir_all(&machine)?;
    fs::write(machine.join("empty"), [])?;
    write_operations(
        &machine.join("populated-unique-synced"),
        &[
            [0, 0, 1, 0],
            [1, 1, 10, 4],
            [1, 2, 20, 4],
            [4, 3, 30, 0],
            [5, 0, 0, 0],
        ],
    )?;
    write_operations(
        &machine.join("repeated-delete-multiple-batches-mixed"),
        &[
            [7, 0, 10, 7],
            [8, 0, 20, 7],
            [1, 3, 30, 8],
            [1, 3, 31, 8],
            [2, 3, 0, 0],
            [3, 3, 0, 0],
            [0, 0, 40, 0],
            [4, 4, 50, 0],
            [5, 0, 0, 0],
        ],
    )?;
    write_operations(
        &machine.join("populated-unsynced"),
        &[[1, 1, 1, 8], [0, 0, 2, 0], [3, 0, 0, 0]],
    )?;
    Ok(())
}

fn write_operations(path: &Path, operations: &[[u8; 4]]) -> varve::Result<()> {
    let bytes: Vec<_> = operations.iter().flatten().copied().collect();
    fs::write(path, bytes)?;
    Ok(())
}

fn prefixed(selector: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(1 + payload.len());
    bytes.push(selector);
    bytes.extend_from_slice(payload);
    bytes
}
