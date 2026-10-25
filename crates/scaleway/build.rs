//! Packs the source of the dispatcher function into `OUT_DIR/function.zip`, which `host.rs`
//! embeds. Scaleway compiles the uploaded source, so the zip is a self-contained Cargo package:
//!
//! ```text
//! Cargo.toml              ciburst-dispatch-scaleway, bare [workspace], no inheritance, deps on vendor/
//! Cargo.lock              the workspace lock
//! src/                    crates/dispatch-scaleway/src
//! vendor/<dir>/           every crate the function depends on by path, directly or through
//!                         another vendored crate (Cargo.toml, src), without build scripts
//! ```
//!
//! Path dependencies are `../<dir>` siblings below `crates/`, so they stay siblings below
//! `vendor/`. The test fakes in `src/testing` are left out: only the `testing` feature compiles
//! them, and the function never enables it.
//!
//! The vendored copy of this crate has no build script, so it embeds an empty zip and the
//! packing does not recurse. Entries are sorted and carry fixed timestamps and permissions, so
//! the same sources give the same bytes, and therefore the same digest, on every platform.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

/// The directory below `src` that holds the test fakes.
const TESTING_DIR: &str = "testing";

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let crates = manifest_dir
        .parent()
        .expect("crates/scaleway lives below crates/")
        .to_path_buf();
    let root = crates
        .parent()
        .expect("crates/ lives in the workspace root")
        .to_path_buf();
    let dispatch = crates.join("dispatch-scaleway");
    let vendored = path_dependencies(&dispatch);

    for path in [root.join("Cargo.toml"), root.join("Cargo.lock")]
        .into_iter()
        .chain(
            std::iter::once(&dispatch)
                .chain(vendored.values())
                .flat_map(|dir| [dir.join("Cargo.toml"), dir.join("src")]),
        )
    {
        println!("cargo::rerun-if-changed={}", path.display());
    }

    let workspace = read_text(&root.join("Cargo.toml"));
    let inherited = Inherited::parse(&workspace);
    let mut files: Vec<(String, Vec<u8>)> = vec![
        (
            "Cargo.toml".to_string(),
            root_manifest(
                &inherited.resolve(&read_text(&dispatch.join("Cargo.toml"))),
                &workspace,
            )
            .into_bytes(),
        ),
        ("Cargo.lock".to_string(), read(&root.join("Cargo.lock"))),
    ];
    collect(&dispatch.join("src"), "src", &mut files);
    for (name, dir) in &vendored {
        files.push((
            format!("vendor/{name}/Cargo.toml"),
            vendored_manifest(&inherited.resolve(&read_text(&dir.join("Cargo.toml")))).into_bytes(),
        ));
        collect(&dir.join("src"), &format!("vendor/{name}/src"), &mut files);
    }
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

/// Every crate `crate_dir` depends on by path, directly or through another of them, by
/// directory name.
fn path_dependencies(crate_dir: &Path) -> BTreeMap<String, PathBuf> {
    let mut found = BTreeMap::new();
    let mut queue = vec![crate_dir.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let manifest = dir.join("Cargo.toml");
        for relative in dependency_paths(&read_text(&manifest)) {
            let name = sibling(&relative).unwrap_or_else(|| {
                panic!(
                    "{}: path dependency {relative} is not a ../<dir> sibling",
                    manifest.display()
                )
            });
            let path = dir.parent().expect("a crate has a parent").join(name);
            if found.insert(name.to_string(), path.clone()).is_none() {
                queue.push(path);
            }
        }
    }
    found
}

/// The `path` values of a manifest's normal dependencies, target-specific ones included; dev
/// and build dependencies do not reach the function.
fn dependency_paths(manifest: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut table = String::new();
    for line in manifest.lines() {
        if let Some(name) = table_name(line) {
            table = name.to_string();
            continue;
        }
        let dotted = table.starts_with("dependencies.") || table.contains(".dependencies.");
        let inline = table == "dependencies" || table.ends_with(".dependencies");
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if dotted && key == "path" {
            paths.push(value.trim_matches('"').to_string());
        } else if inline && value.starts_with('{') {
            for item in split_items(inner(value)) {
                if let Some((k, v)) = item.split_once('=')
                    && k.trim() == "path"
                {
                    paths.push(v.trim().trim_matches('"').to_string());
                }
            }
        }
    }
    paths
}

/// `<dir>` of a `../<dir>` path.
fn sibling(relative: &str) -> Option<&str> {
    relative
        .strip_prefix("../")
        .filter(|name| !name.is_empty() && !name.contains(['/', '\\']) && *name != "..")
}

/// Adds every file below `dir` under the zip prefix `prefix`, with `/` separators; a
/// `testing` directory right below a `src` directory stays out.
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
            let testing = name == TESTING_DIR && (prefix == "src" || prefix.ends_with("/src"));
            if !testing {
                collect(&path, &zip_name, files);
            }
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

/// Values of `[workspace.package]` and `[workspace.dependencies]`, written into each manifest
/// because Scaleway's Rust builder fails on workspace inheritance (probed 2026-10-24).
struct Inherited {
    package: BTreeMap<String, String>,
    dependencies: BTreeMap<String, String>,
}

impl Inherited {
    fn parse(workspace: &str) -> Inherited {
        let mut package = BTreeMap::new();
        let mut dependencies = BTreeMap::new();
        let mut table = String::new();
        for line in workspace.lines() {
            if let Some(name) = table_name(line) {
                table = name.to_string();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim().to_string(), value.trim().to_string());
            match table.as_str() {
                "workspace.package" => {
                    package.insert(key, value);
                }
                "workspace.dependencies" => {
                    dependencies.insert(key, value);
                }
                _ => {}
            }
        }
        Inherited {
            package,
            dependencies,
        }
    }

    /// The manifest with every `key.workspace = true` and `{ workspace = true, ... }` replaced
    /// by the workspace's value; dependency features are merged.
    fn resolve(&self, manifest: &str) -> String {
        let mut out = String::new();
        let mut table = String::new();
        for line in manifest.lines() {
            if let Some(name) = table_name(line) {
                table = name.to_string();
                out.push_str(line);
                out.push('\n');
                continue;
            }
            out.push_str(&self.resolve_line(&table, line));
            out.push('\n');
        }
        assert!(
            !out.contains("workspace = true") && !out.contains(".workspace"),
            "unresolved workspace inheritance left in a function manifest"
        );
        out
    }

    fn resolve_line(&self, table: &str, line: &str) -> String {
        let Some((key, value)) = line.split_once('=') else {
            return line.to_string();
        };
        let (key, value) = (key.trim(), value.trim());
        if let Some(name) = key.strip_suffix(".workspace") {
            assert_eq!(value, "true", "{line}");
            let source = if table == "package" {
                &self.package
            } else {
                &self.dependencies
            };
            let inherited = source
                .get(name)
                .unwrap_or_else(|| panic!("{name} is not in the workspace tables"));
            return format!("{name} = {inherited}");
        }
        if value.starts_with('{') && value.contains("workspace = true") {
            let base = self
                .dependencies
                .get(key)
                .unwrap_or_else(|| panic!("{key} is not in [workspace.dependencies]"));
            let base = if base.starts_with('"') {
                format!("version = {base}")
            } else {
                inner(base).to_string()
            };
            let local: Vec<&str> = split_items(inner(value))
                .into_iter()
                .filter(|item| item.replace(' ', "") != "workspace=true")
                .collect();
            let mut features = BTreeSet::new();
            let mut items = Vec::new();
            for item in split_items(&base).into_iter().chain(local) {
                match item.split_once('=') {
                    Some((k, v)) if k.trim() == "features" => {
                        for feature in v.trim().trim_matches(|c| c == '[' || c == ']').split(',') {
                            let feature = feature.trim();
                            if !feature.is_empty() {
                                features.insert(feature.to_string());
                            }
                        }
                    }
                    _ => items.push(item.to_string()),
                }
            }
            if !features.is_empty() {
                let list: Vec<String> = features.into_iter().collect();
                items.push(format!("features = [{}]", list.join(", ")));
            }
            return format!("{key} = {{ {} }}", items.join(", "));
        }
        line.to_string()
    }
}

/// The content of an inline table without its braces.
fn inner(table: &str) -> &str {
    table
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .trim()
}

/// Top-level `key = value` items of an inline table body, keeping arrays whole.
fn split_items(body: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (index, c) in body.char_indices() {
        match c {
            '[' | '{' => depth += 1,
            ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                items.push(body[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    items.push(body[start..].trim());
    items.into_iter().filter(|item| !item.is_empty()).collect()
}

/// The function package's manifest: path dependencies point into `vendor/`, dev and build
/// dependencies are dropped, and a bare `[workspace]` with the release profile detaches the
/// package from any parent.
fn root_manifest(package: &str, workspace: &str) -> String {
    let mut text = strip_tables(package, &["dev-dependencies", "build-dependencies"]);
    for relative in dependency_paths(&text) {
        let name = sibling(&relative).expect("checked by path_dependencies");
        text = text.replace(&format!("\"{relative}\""), &format!("\"vendor/{name}\""));
    }
    assert!(
        !text.contains("\"../"),
        "a path outside vendor/ is left in the function manifest"
    );
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

/// `[workspace]` without `members`, `default-members` and `exclude`, plus the `[profile.*]`
/// tables; the inheritance tables are resolved into the manifests instead.
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
        let keep = table == "workspace" || table.starts_with("profile.");
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
