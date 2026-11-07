//! The ciburst logo (spec 11.6, D76): every file in `brand/` from the mark's geometry and the
//! site's Source Sans 3, and PNG renders of them.

mod mark;
mod word;

use std::fs;
use std::path::{Path, PathBuf};

use resvg::{tiny_skia, usvg};

/// The font of the word, the site's own (spec 11.5).
pub const FONT: &str = "crates/site/assets/fonts/source-sans-3-latin.woff2";
/// The logo's files, below the repository root.
pub const DIR: &str = "brand";

/// Ink on light and on dark backgrounds, the burst on both, the tile behind the dark logo.
pub const INK_LIGHT: &str = "#17202B";
pub const INK_DARK: &str = "#E7EBF0";
pub const BURST: &str = "#E3A54E";
pub const TILE: &str = "#10151C";

/// The ink of the reader's color scheme, for files that use `currentColor`.
const SCHEME: &str =
    "<style>svg{color:#17202B}@media (prefers-color-scheme:dark){svg{color:#E7EBF0}}</style>";

/// The mark's height and its top above the baseline, the gap to the word and the margin, each in
/// heights of the word above the baseline.
const MARK_HEIGHT: f64 = 1.25;
const MARK_TOP: f64 = 1.125;
const GAP: f64 = 0.32;
const MARGIN: f64 = 0.08;

/// The README's tile: the margin around the logo in logo heights, the corner radius in tile
/// heights, the width it shows at.
const TILE_MARGIN: f64 = 0.42;
const TILE_RADIUS: f64 = 0.14;
const TILE_WIDTH: f64 = 300.0;

/// The profile picture's size and its margin around the mark, in pixels.
const AVATAR: f64 = 512.0;
const AVATAR_MARGIN: f64 = 72.0;

/// The files `png` renders, with their height in pixels; the others follow the reader's color
/// scheme and are for browsers only.
pub const RENDERS: &[(&str, u32)] = &[
    ("avatar.svg", 512),
    ("logo-light.svg", 240),
    ("logo-dark.svg", 240),
    ("logo-tile.svg", 210),
    ("mark-light.svg", 512),
    ("mark-dark.svg", 512),
];

/// A length with at most two decimals, without trailing zeros.
pub(crate) fn n(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0" } else { s }.to_string()
}

struct ViewBox(f64, f64, f64, f64);

impl ViewBox {
    fn attr(&self) -> String {
        format!("{} {} {} {}", n(self.0), n(self.1), n(self.2), n(self.3))
    }
}

/// The mark and the word side by side, the baseline at 0.
fn logo(ink: &str, id: &str, word: &word::Word) -> (ViewBox, String) {
    let (x0, y0, x1, y1) = mark::bounds();
    let above = -word.bounds.1;
    let scale = above * MARK_HEIGHT / (y1 - y0);
    let top = -above * MARK_TOP;
    let word_x = (x1 - x0) * scale + above * GAP;
    let margin = above * MARGIN;
    let view = ViewBox(
        -margin,
        top - margin,
        word_x + word.bounds.2 + 2.0 * margin,
        (y1 - y0) * scale + 2.0 * margin,
    );
    let body = format!(
        "<g transform=\"translate({} {}) scale({})\">{}</g>\
         <path d=\"{}\" transform=\"translate({} 0)\" fill=\"{ink}\"/>",
        n(-x0 * scale),
        n(top - y0 * scale),
        format_args!("{scale:.4}"),
        mark::mark(ink, BURST, id),
        word.path,
        n(word_x),
    );
    (view, body)
}

/// The mark centered in a square of `size` with `margin` around it.
fn fitted(ink: &str, id: &str, size: f64, margin: f64) -> String {
    let (x0, y0, x1, y1) = mark::bounds();
    let (w, h) = (x1 - x0, y1 - y0);
    let scale = (size - 2.0 * margin) / w.max(h);
    format!(
        "<g transform=\"translate({} {}) scale({scale:.4})\">{}</g>",
        n(size / 2.0 - scale * (x0 + w / 2.0)),
        n(size / 2.0 - scale * (y0 + h / 2.0)),
        mark::mark(ink, BURST, id),
    )
}

fn svg(view: &str, size: &str, body: &str) -> String {
    format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{view}\"{size}>{body}</svg>\n")
}

/// Every file of `brand/` by name, from the WOFF2 file of Source Sans 3.
pub fn files(woff2: &[u8]) -> Result<Vec<(&'static str, String)>, String> {
    let word = word::word(woff2)?;
    let mut files = Vec::new();

    let (view, body) = logo("currentColor", "m", &word);
    files.push((
        "logo.svg",
        svg(&view.attr(), "", &format!("{SCHEME}{body}")),
    ));
    for (name, ink) in [("logo-light.svg", INK_LIGHT), ("logo-dark.svg", INK_DARK)] {
        let (view, body) = logo(ink, "m", &word);
        files.push((name, svg(&view.attr(), "", &body)));
    }

    let (view, body) = logo(INK_DARK, "m", &word);
    let margin = view.3 * TILE_MARGIN;
    let (w, h) = (view.2 + 2.0 * margin, view.3 + 2.0 * margin);
    files.push((
        "logo-tile.svg",
        svg(
            &format!("0 0 {} {}", n(w), n(h)),
            &format!(
                " width=\"{}\" height=\"{}\"",
                n(TILE_WIDTH),
                n((TILE_WIDTH * h / w).round())
            ),
            &format!(
                "<rect width=\"{}\" height=\"{}\" rx=\"{}\" fill=\"{TILE}\"/>\
                 <svg x=\"{m}\" y=\"{m}\" width=\"{}\" height=\"{}\" viewBox=\"{}\">{body}</svg>",
                n(w),
                n(h),
                n(h * TILE_RADIUS),
                n(view.2),
                n(view.3),
                view.attr(),
                m = n(margin),
            ),
        ),
    ));

    let (x0, y0, x1, y1) = mark::bounds();
    let pad = (x1 - x0) * 0.02;
    let view = ViewBox(x0 - pad, y0 - pad, x1 - x0 + 2.0 * pad, y1 - y0 + 2.0 * pad).attr();
    for (name, ink) in [("mark-light.svg", INK_LIGHT), ("mark-dark.svg", INK_DARK)] {
        files.push((name, svg(&view, "", &mark::mark(ink, BURST, "m"))));
    }
    files.push((
        "favicon.svg",
        svg(
            "0 0 64 64",
            "",
            &format!("{SCHEME}{}", fitted("currentColor", "m", 64.0, 2.0)),
        ),
    ));
    files.push((
        "avatar.svg",
        svg(
            &format!("0 0 {a} {a}", a = n(AVATAR)),
            &format!(" width=\"{a}\" height=\"{a}\"", a = n(AVATAR)),
            &format!(
                "<rect width=\"{a}\" height=\"{a}\" fill=\"{TILE}\"/>{}",
                fitted(INK_DARK, "m", AVATAR, AVATAR_MARGIN),
                a = n(AVATAR)
            ),
        ),
    ));
    Ok(files)
}

/// `svg` drawn `height` pixels high, its width to scale.
pub fn raster(svg: &str, height: u32) -> Result<tiny_skia::Pixmap, String> {
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).map_err(|e| e.to_string())?;
    let size = tree.size();
    let scale = height as f32 / size.height();
    let width = (size.width() * scale).round() as u32;
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or("empty drawing")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap)
}

/// `svg` as a PNG file `height` pixels high.
pub fn png(svg: &str, height: u32) -> Result<Vec<u8>, String> {
    raster(svg, height)?.encode_png().map_err(|e| e.to_string())
}

fn write(path: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path.to_path_buf())
}

/// Writes every file of `brand/` below the repository at `root`.
pub fn write_svg(root: &Path) -> Result<Vec<PathBuf>, String> {
    let font = root.join(FONT);
    let woff2 = fs::read(&font).map_err(|e| format!("{}: {e}", font.display()))?;
    files(&woff2)?
        .into_iter()
        .map(|(name, svg)| write(&root.join(DIR).join(name), svg.as_bytes()))
        .collect()
}

/// Renders the files of `brand/` listed in `RENDERS` into `out` as PNG.
pub fn write_png(root: &Path, out: &Path) -> Result<Vec<PathBuf>, String> {
    RENDERS
        .iter()
        .map(|(name, height)| {
            let path = root.join(DIR).join(name);
            let svg = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let png = png(&svg, *height).map_err(|e| format!("{name}: {e}"))?;
            write(&out.join(name.replace(".svg", ".png")), &png)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_drop_trailing_zeros() {
        assert_eq!(n(27.0), "27");
        assert_eq!(n(3.5), "3.5");
        assert_eq!(n(-0.001), "0");
        assert_eq!(n(26.766), "26.77");
    }
}
