//! Packs the source of the dispatcher function into `OUT_DIR/function.zip`, which `host.rs`
//! embeds. Scaleway compiles the uploaded source, so the zip is a self-contained Cargo package:
//!
//! ```text
//! Cargo.toml              ciburst-dispatch-scaleway, own [workspace], deps on vendor/
//! Cargo.lock              the workspace lock
//! src/                    crates/dispatch-scaleway/src
//! vendor/core/            crates/core (Cargo.toml, src)
//! vendor/scaleway/        crates/scaleway (Cargo.toml, src), without this build script
//! ```
//!
//! The vendored copy of this crate has no build script, so it embeds an empty zip and the
//! packing does not recurse. Entries are sorted and carry fixed timestamps and permissions, so
//! the same sources give the same bytes, and therefore the same digest, on every platform.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crates/scaleway lives two levels below the workspace root")
        .to_path_buf();
    let core = root.join("crates").join("core");
    let dispatch = root.join("crates").join("dispatch-scaleway");
    let scaleway = manifest_dir;

    for path in [
        root.join("Cargo.toml"),
        root.join("Cargo.lock"),
        core.join("Cargo.toml"),
        core.join("src"),
        scaleway.join("Cargo.toml"),
        scaleway.join("src"),
        dispatch.join("Cargo.toml"),
        dispatch.join("src"),
    ] {
        println!("cargo::rerun-if-changed={}", path.display());
    }

    let mut files: Vec<(String, Vec<u8>)> = vec![
        (
            "Cargo.toml".to_string(),
            root_manifest(
                &read_text(&dispatch.join("Cargo.toml")),
                &read_text(&root.join("Cargo.toml")),
            )
            .into_bytes(),
        ),
        ("Cargo.lock".to_string(), read(&root.join("Cargo.lock"))),
        (
            "vendor/core/Cargo.toml".to_string(),
            vendored_manifest(&read_text(&core.join("Cargo.toml"))).into_bytes(),
        ),
        (
            "vendor/scaleway/Cargo.toml".to_string(),
            vendored_manifest(&read_text(&scaleway.join("Cargo.toml"))).into_bytes(),
        ),
    ];
    collect(&dispatch.join("src"), "src", &mut files);
    collect(&core.join("src"), "vendor/core/src", &mut files);
    collect(&scaleway.join("src"), "vendor/scaleway/src", &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("function.zip");
    fs::write(&out, pack(&files)).expect("write function.zip");
    println!("cargo::rustc-cfg=ciburst_embedded_code");
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn read_text(path: &Path) -> String {
    String::from_utf8(read(path)).unwrap_or_else(|_| panic!("{} is not UTF-8", path.display()))
}

/// Adds every file below `dir` under the zip prefix `prefix`, with `/` separators.
fn collect(dir: &Path, prefix: &str, files: &mut Vec<(String, Vec<u8>)>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_else(|| panic!("{} has a non UTF-8 name", path.display()))
            .to_string();
        let zip_name = format!("{prefix}/{name}");
        if path.is_dir() {
            collect(&path, &zip_name, files);
        } else {
            files.push((zip_name, read(&path)));
        }
    }
}

fn pack(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let options = |mode: u32| {
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(DateTime::default())
            .unix_permissions(mode)
    };
    let mut directories = BTreeSet::new();
    for (name, _) in files {
        let mut parts: Vec<&str> = name.split('/').collect();
        parts.pop();
        for end in 1..=parts.len() {
            directories.insert(format!("{}/", parts[..end].join("/")));
        }
    }
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for directory in &directories {
        writer
            .add_directory(directory.as_str(), options(0o755))
            .expect("zip directory");
    }
    for (name, bytes) in files {
        writer
            .start_file(name.as_str(), options(0o644))
            .expect("zip file");
        writer.write_all(bytes).expect("zip write");
    }
    writer.finish().expect("zip finish").into_inner()
}

/// The function package's manifest: path dependencies point into `vendor/`, dev and build
/// dependencies are dropped, and the workspace tables are copied without `members`, so the
/// package detaches from any parent and its vendored crates still inherit versions and
/// dependency specs.
fn root_manifest(package: &str, workspace: &str) -> String {
    let mut text = strip_tables(package, &["dev-dependencies", "build-dependencies"]);
    for (from, to) in [
        ("path = \"../core\"", "path = \"vendor/core\""),
        ("path = \"../scaleway\"", "path = \"vendor/scaleway\""),
    ] {
        assert!(
            text.contains(from),
            "dispatch-scaleway/Cargo.toml no longer has {from}"
        );
        text = text.replace(from, to);
    }
    text.push('\n');
    text.push_str(&detached_workspace(workspace));
    text
}

/// A vendored crate's manifest without dev and build dependencies and without a build script.
fn vendored_manifest(manifest: &str) -> String {
    strip_tables(manifest, &["dev-dependencies", "build-dependencies"])
        .lines()
        .filter(|line| {
            let key = line.trim_start();
            !(key.starts_with("build ") || key.starts_with("build="))
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

fn table_name(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.starts_with('[') && line.ends_with(']') {
        Some(line.trim_matches(|c| c == '[' || c == ']').trim())
    } else {
        None
    }
}

/// Drops the tables named in `names`, their dotted subtables and target-specific variants.
fn strip_tables(manifest: &str, names: &[&str]) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in manifest.lines() {
        if let Some(table) = table_name(line) {
            skipping = names.iter().any(|name| {
                table == *name
                    || table.starts_with(&format!("{name}."))
                    || table.ends_with(&format!(".{name}"))
            });
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// `[workspace]` without `members`, `default-members` and `exclude`, plus
/// `[workspace.package]`, `[workspace.dependencies]` and the `[profile.*]` tables.
fn detached_workspace(workspace: &str) -> String {
    let mut out = String::new();
    let mut table = String::new();
    let mut depth = 0i32;
    for line in workspace.lines() {
        if depth > 0 {
            depth += bracket_balance(line);
            continue;
        }
        if let Some(name) = table_name(line) {
            table = name.to_string();
        }
        let keep = table == "workspace"
            || table == "workspace.package"
            || table == "workspace.dependencies"
            || table.starts_with("profile.");
        if !keep {
            continue;
        }
        let key = line.split('=').next().unwrap_or("").trim();
        if table == "workspace" && matches!(key, "members" | "default-members" | "exclude") {
            depth = bracket_balance(line);
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    assert!(
        out.contains("[workspace]"),
        "the workspace manifest has no [workspace] table"
    );
    out
}

fn bracket_balance(line: &str) -> i32 {
    line.chars()
        .map(|c| match c {
            '[' => 1,
            ']' => -1,
            _ => 0,
        })
        .sum()
}
