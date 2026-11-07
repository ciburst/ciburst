//! spec 11.6: the files in `brand/` are the generator's, and each renders as drawn.

use std::fs;
use std::path::{Path, PathBuf};

use ciburst_brand::{BURST, INK_DARK, INK_LIGHT, RENDERS, TILE};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn generated() -> Vec<(&'static str, String)> {
    let font = fs::read(repo().join(ciburst_brand::FONT)).unwrap();
    ciburst_brand::files(&font).unwrap()
}

fn rgb(hex: &str) -> [u8; 3] {
    let v = u32::from_str_radix(&hex[1..], 16).unwrap();
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

#[test]
fn the_files_in_brand_are_the_generators() {
    let generated = generated();
    for (name, svg) in &generated {
        let path = repo().join("brand").join(name);
        let checked_in = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .replace("\r\n", "\n");
        assert!(
            &checked_in == svg,
            "brand/{name} differs from the generator: cargo run -p ciburst-brand -- svg"
        );
    }
    let mut on_disk: Vec<String> = fs::read_dir(repo().join("brand"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut names: Vec<String> = generated.iter().map(|(n, _)| n.to_string()).collect();
    names.sort();
    assert_eq!(on_disk, names, "brand/ holds only the generator's files");
}

/// Each rendered file shows its ink and the burst's amber, and the avatar fills its square with
/// the tile.
#[test]
fn each_file_renders_with_its_colors() {
    let generated = generated();
    for (name, height) in RENDERS {
        let svg = &generated.iter().find(|(n, _)| n == name).unwrap().1;
        let pixmap = ciburst_brand::raster(svg, *height).unwrap();
        assert_eq!(pixmap.height(), *height, "{name}");
        let has = |hex: &str| {
            let [r, g, b] = rgb(hex);
            pixmap
                .pixels()
                .iter()
                .any(|p| p.alpha() == 255 && [p.red(), p.green(), p.blue()] == [r, g, b])
        };
        let ink = if name.contains("light") {
            INK_LIGHT
        } else {
            INK_DARK
        };
        assert!(has(ink), "{name} shows its ink {ink}");
        assert!(has(BURST), "{name} shows the burst {BURST}");
        let corner = pixmap.pixel(0, 0).unwrap();
        if *name == "avatar.svg" {
            assert_eq!(pixmap.width(), 512);
            assert_eq!(
                [corner.red(), corner.green(), corner.blue(), corner.alpha()],
                [rgb(TILE)[0], rgb(TILE)[1], rgb(TILE)[2], 255],
                "the avatar's corner is the tile"
            );
        } else if *name != "logo-tile.svg" {
            assert_eq!(corner.alpha(), 0, "{name} is transparent around the logo");
        }
    }
}

/// The site and the README take the logo from `brand/` (D76).
#[test]
fn the_readme_and_the_site_use_these_files() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    assert_eq!(
        readme.lines().next(),
        Some("# ![ciburst](brand/logo-tile.svg)")
    );
    let site = fs::read_to_string(repo().join("crates/site/src/lib.rs")).unwrap();
    assert!(site.contains("include_str!(\"../../../brand/logo.svg\")"));
    assert!(site.contains("include_str!(\"../../../brand/favicon.svg\")"));
}
