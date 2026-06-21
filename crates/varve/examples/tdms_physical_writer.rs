use std::env;
use std::path::PathBuf;

#[path = "tdms_physical/common.rs"]
mod common;

fn main() -> varve::Result<()> {
    let path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("varve-tdms-compat.tdms"));
    common::write_example(&path)?;
    println!("{}", path.display());
    Ok(())
}
