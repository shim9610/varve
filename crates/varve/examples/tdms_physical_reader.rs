use std::env;
use std::path::PathBuf;

#[path = "tdms_physical/common.rs"]
mod common;

fn main() -> varve::Result<()> {
    let path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: tdms_physical_reader <file.tdms>");
    common::read_and_verify(&path)?;
    println!(
        "Varve-based example parsed TDMS scalar type matrix file {}",
        path.display()
    );
    Ok(())
}
