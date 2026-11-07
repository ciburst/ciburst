//! `ciburst-site <out dir> [repository root]`: writes the site (spec 11.5).
//! `ciburst-site --flow [repository root]`: writes the README's animation, `docs/flow.svg`.

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--flow") {
        let root = args.get(1).map_or(".", String::as_str);
        return match ciburst_site::write_readme_flow(Path::new(root)) {
            Ok(file) => {
                println!("{}", file.display());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("ciburst-site: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let (out, root) = match args.as_slice() {
        [out] => (out.as_str(), "."),
        [out, root] => (out.as_str(), root.as_str()),
        _ => {
            eprintln!("usage: ciburst-site <out dir> [repository root] | --flow [repository root]");
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
