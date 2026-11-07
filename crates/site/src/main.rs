//! `ciburst-site <out dir> [repository root]`: writes the site (spec 11.5).

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (out, root) = match args.as_slice() {
        [out] => (out.as_str(), "."),
        [out, root] => (out.as_str(), root.as_str()),
        _ => {
            eprintln!("usage: ciburst-site <out dir> [repository root]");
            return ExitCode::from(2);
        }
    };
    match ciburst_site::build(Path::new(root), Path::new(out)) {
        Ok(files) => {
            println!("{} files in {out}", files.len());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("ciburst-site: {error}");
            ExitCode::FAILURE
        }
    }
}
