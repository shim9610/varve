use std::env;
use std::path::PathBuf;

#[path = "tdms_physical/common.rs"]
mod common;

fn main() -> varve::Result<()> {
    let mut args = env::args_os();
    let _program = args.next();
    let command = args
        .next()
        .and_then(|value| value.into_string().ok())
        .expect("usage: tdms_physical_adapter <write|read> <file.tdms>");
    let path = args
        .next()
        .map(PathBuf::from)
        .expect("usage: tdms_physical_adapter <write|read> <file.tdms>");

    match command.as_str() {
        "write" => {
            common::write_example(&path)?;
            println!("{}", path.display());
        }
        "read" => {
            common::read_and_verify(&path)?;
            println!(
                "Varve-based adapter example parsed TDMS file {}",
                path.display()
            );
        }
        _ => panic!("usage: tdms_physical_adapter <write|read> <file.tdms>"),
    }
    Ok(())
}
