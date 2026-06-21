use std::env;
use std::fs::remove_file;
use std::path::{Path, PathBuf};

use varve::{
    BinaryCursor, BinaryWriter, ChunkEntry, ChunkIndexBuilder, ChunkLayout, Endian, varve_format,
};

const WIDTH: u32 = 3;
const HEIGHT: u32 = 2;
const X_PIXELS_PER_METER: u32 = 2835;
const Y_PIXELS_PER_METER: u32 = 2835;

varve_format! {
    pub format BmpCompatFormat {
        magic: b"BMP";
        version: 1;
        endian: little;
        schema_hash: computed;
        extension: "bmp";
        preset: none;

        layout {
            segment BitmapImage repeat once {
                lead_in BitmapHeader {
                    bytes signature = b"BM";
                    u32 file_size = finalize(target = segment_end, relative_to = segment_start);
                    u32 reserved = 0;
                    u32 pixel_data_offset = finalize(target = raw_region_start, relative_to = segment_start);
                    u32 dib_header_size = 40;
                    u32 width;
                    u32 height;
                    u16 planes = 1;
                    u16 bits_per_pixel = 24;
                    u32 compression = 0;
                    u32 image_size = finalize(target = segment_end, relative_to = raw_region_start);
                    u32 x_pixels_per_meter;
                    u32 y_pixels_per_meter;
                    u32 colors_used = 0;
                    u32 important_colors = 0;
                }

                metadata BitmapMetadata;
                raw_region BitmapPixels;
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os();
    let _program = args.next();
    let command = args
        .next()
        .and_then(|value| value.into_string().ok())
        .expect("usage: bmp_physical <write|read> <file.bmp>");
    let path = args
        .next()
        .map(PathBuf::from)
        .expect("usage: bmp_physical <write|read> <file.bmp>");

    match command.as_str() {
        "write" => write_bmp(&path)?,
        "read" => read_bmp(&path)?,
        _ => panic!("usage: bmp_physical <write|read> <file.bmp>"),
    }

    println!("Varve BMP {command} verified {}", path.display());
    Ok(())
}

fn write_bmp(path: &Path) -> varve::Result<()> {
    cleanup(path);
    let pixels = encode_bmp_pixels(WIDTH, HEIGHT, &expected_pixels())?;
    let mut writer = BmpCompatFormat::create_layout_writer(path)?;
    writer.write_bitmap_image(BmpCompatFormatBitmapImageLayoutWrite {
        fields: BmpCompatFormatBitmapImageLayoutFields {
            width: WIDTH,
            height: HEIGHT,
            x_pixels_per_meter: X_PIXELS_PER_METER,
            y_pixels_per_meter: Y_PIXELS_PER_METER,
        },
        footer_fields: BmpCompatFormatBitmapImageLayoutFooterFields,
        metadata: b"",
        raw: &pixels,
    })?;
    writer.flush()?;
    Ok(())
}

fn read_bmp(path: &Path) -> varve::Result<()> {
    let reader = BmpCompatFormat::open_layout_reader(path)?;
    let image = reader.bitmap_image(0)?.expect("bitmap segment");
    assert_eq!(image.signature()?, b"BM");
    assert_eq!(image.width()?, WIDTH);
    assert_eq!(image.height()?, HEIGHT);
    assert_eq!(image.dib_header_size()?, 40);
    assert_eq!(image.planes()?, 1);
    assert_eq!(image.bits_per_pixel()?, 24);
    assert_eq!(image.compression()?, 0);
    assert_eq!(image.pixel_data_offset()?, 54);
    assert_eq!(image.file_size()?, 54 + image.image_size()?);
    assert_eq!(reader.read_bitmap_image_metadata(0)?, b"");

    let raw = reader.read_bitmap_image_raw(0)?;
    assert_eq!(raw.len() as u32, image.image_size()?);
    let stride = bmp_row_stride(WIDTH);
    let row_len = u64::from(WIDTH * 3);
    let mut chunks = ChunkIndexBuilder::new();
    for stored_y in 0..HEIGHT {
        chunks.push(
            ChunkEntry {
                key: stored_y,
                segment_index: 0,
                byte_offset: u64::from(stored_y) * stride as u64,
                byte_len: row_len,
                value_count: u64::from(WIDTH),
                layout: ChunkLayout::Strided { byte_stride: 3 },
            },
            image.as_layout_segment_info(),
        )?;
    }
    let chunk_index = chunks.finish()?;
    assert_eq!(chunk_index.entries().len(), HEIGHT as usize);
    assert_eq!(chunk_index.entries_for(&0).count(), 1);
    assert_eq!(
        decode_bmp_pixels(WIDTH, HEIGHT, &raw)?,
        expected_pixels(),
        "BMP pixel payload mismatch"
    );
    Ok(())
}

fn expected_pixels() -> Vec<(u8, u8, u8)> {
    vec![
        (255, 0, 0),
        (0, 255, 0),
        (0, 0, 255),
        (0, 255, 255),
        (255, 0, 255),
        (255, 255, 0),
    ]
}

fn encode_bmp_pixels(
    width: u32,
    height: u32,
    rgb_top_down: &[(u8, u8, u8)],
) -> varve::Result<Vec<u8>> {
    let stride = bmp_row_stride(width);
    let row_len = (width * 3) as usize;
    let mut writer = BinaryWriter::with_capacity(Endian::Little, stride * height as usize);
    for stored_y in 0..height as usize {
        let top_y = height as usize - 1 - stored_y;
        for x in 0..width as usize {
            let (r, g, b) = rgb_top_down
                .get(top_y * width as usize + x)
                .copied()
                .expect("BMP source pixel is in bounds");
            writer.bytes(&[b, g, r])?;
        }
        writer.bytes(&vec![0; stride - row_len])?;
    }
    Ok(writer.into_inner())
}

fn decode_bmp_pixels(width: u32, height: u32, raw: &[u8]) -> varve::Result<Vec<(u8, u8, u8)>> {
    let stride = bmp_row_stride(width);
    assert_eq!(raw.len(), stride * height as usize);
    let mut cursor = BinaryCursor::new(raw, Endian::Little);
    let mut stored_rows = Vec::with_capacity(height as usize);
    for _ in 0..height as usize {
        let mut row_pixels = Vec::with_capacity(width as usize);
        for _ in 0..width as usize {
            let [b, g, r]: [u8; 3] = cursor
                .bytes(3)?
                .try_into()
                .expect("BMP decoded pixel has exactly 3 bytes");
            row_pixels.push((r, g, b));
        }
        cursor.bytes(stride - (width as usize * 3))?;
        stored_rows.push(row_pixels);
    }
    cursor.finish()?;
    let rgb = stored_rows.into_iter().rev().flatten().collect();
    Ok(rgb)
}

fn bmp_row_stride(width: u32) -> usize {
    ((width as usize * 3) + 3) & !3
}

fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
