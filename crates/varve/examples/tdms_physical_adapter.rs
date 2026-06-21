use std::env;
use std::fs::read;
use std::path::PathBuf;

#[path = "tdms_physical/common.rs"]
mod common;

fn main() -> varve::Result<()> {
    let mut args = env::args_os();
    let _program = args.next();
    let command = args
        .next()
        .and_then(|value| value.into_string().ok())
        .expect("usage: tdms_physical_adapter <write|append|read|read-bytes|inspect> <file.tdms>");
    let path = args
        .next()
        .map(PathBuf::from)
        .expect("usage: tdms_physical_adapter <write|append|read|read-bytes|inspect> <file.tdms>");

    match command.as_str() {
        "write" => {
            common::write_example(&path)?;
            println!("{}", path.display());
        }
        "append" => {
            common::append_example(&path)?;
            println!("Varve-based adapter example appended {}", path.display());
        }
        "read" => {
            common::read_and_verify(&path)?;
            println!(
                "Varve-based adapter example parsed TDMS file {}",
                path.display()
            );
        }
        "read-bytes" => {
            let bytes = read(&path)?;
            common::read_and_verify_bytes(&bytes)?;
            println!(
                "Varve-based byte-backed adapter example parsed TDMS file {}",
                path.display()
            );
        }
        "inspect" => {
            let report = common::inspect_example(&path)?;
            println!("adapter inspection status: {:?}", report.status());
            for diagnostic in report.diagnostics {
                println!(
                    "{:?} {:?}: {}",
                    diagnostic.status, diagnostic.domain, diagnostic.message
                );
            }
        }
        _ => {
            panic!(
                "usage: tdms_physical_adapter <write|append|read|read-bytes|inspect> <file.tdms>"
            )
        }
    }
    Ok(())
}
