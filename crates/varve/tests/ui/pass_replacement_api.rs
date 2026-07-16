use std::path::Path;

use varve::{ResourceLimits, VarveBlock, VarveReplaceBlock, varve_format};

#[derive(Clone, VarveBlock)]
#[varve(id = 90, kind = "fixed")]
struct DerivedBlock {
    value: u32,
}

fn accepts_replace_block<T: VarveReplaceBlock>() {}

varve_format! {
    pub format ReplacementFormat {
        magic: b"REPLACE_API";
        version: 1;
        blocks {
            fixed Point(id = 1) {
                value: u32,
            }

            variable User(id = 2, key = [id]) {
                id: u64,
                name: String,
            }
        }
    }
}

fn typecheck(path: &Path) -> varve::Result<()> {
    let limits = ResourceLimits::missing().with_max_record_payload_len(4096);
    let _ = ReplacementFormat::create_with_resource_limits(path, limits);
    let _ = ReplacementFormat::create_writer_with_resource_limits(path, limits);
    let _ = ReplacementFormat::open_with_resource_limits(path, limits);
    let _ = ReplacementFormat::open_writer_with_resource_limits(path, limits);
    let _ = ReplacementFormat::open_readonly_with_resource_limits(path, limits);
    let _ = ReplacementFormat::open_reader_with_resource_limits(path, limits);
    let _ = ReplacementFormat::open_recover_with_resource_limits(path, limits);
    let _ = ReplacementFormat::open_recover_writer_with_resource_limits(path, limits);
    let _ = ReplacementFormat::open_recover_with_report_and_resource_limits(path, limits);
    let _ = ReplacementFormat::open_recover_writer_with_report_and_resource_limits(path, limits);

    let mut writer = ReplacementFormat::create_writer(path)?;
    let _ = writer.replace_point(0, &Point { value: 2 });
    let user = User {
        id: 1,
        name: String::new(),
    };
    let _ = writer.replace_user(0, &user);
    let _ = ReplacementFormatWrite::replace_user(&mut writer, 0, &user);
    Ok(())
}

fn main() {
    accepts_replace_block::<DerivedBlock>();
    accepts_replace_block::<Point>();
    accepts_replace_block::<User>();
    let _ = typecheck;
}
