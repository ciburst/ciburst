//! `ciburst-brand svg [repository root]`: writes the logo's files into `brand/`.
//! `ciburst-brand png <out dir> [repository root]`: renders them as PNG (spec 11.6).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage: ciburst-brand svg [repository root] | png <out dir> [repository root]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let written: Result<Vec<PathBuf>, String> = match args.as_slice() {
        ["svg"] => ciburst_brand::write_svg(Path::new(".")),
        ["svg", root] => ciburst_brand::write_svg(Path::new(root)),
        ["png", out] => ciburst_brand::write_png(Path::new("."), Path::new(out)),
        ["png", out, root] => ciburst_brand::write_png(Path::new(root), Path::new(out)),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match written {
        Ok(files) => {
            for file in files {
                println!("{}", file.display());
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("ciburst-brand: {error}");
            ExitCode::FAILURE
        }
    }
}
