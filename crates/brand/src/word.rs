//! The word `ciburst` as outlines of the site's Source Sans 3 (spec 11.6), so no file of the logo
//! needs the font.

use crate::n;
use skrifa::instance::Size;
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::raw::TableProvider;
use skrifa::{FontRef, MetadataProvider, Tag};

pub const TEXT: &str = "ciburst";
/// Bold, the weight of the site's headings.
const WEIGHT: f32 = 700.0;
/// Letter spacing in em.
const TRACKING: f32 = -0.01;

/// The outlines in font units, y down, the baseline at 0, the first letter's origin at 0.
pub struct Word {
    pub path: String,
    /// Left, top, right, bottom of the outlines.
    pub bounds: (f64, f64, f64, f64),
}

struct Pen {
    d: String,
    dx: f32,
    bounds: (f64, f64, f64, f64),
}

impl Pen {
    fn point(&mut self, x: f32, y: f32) -> String {
        let (x, y) = (f64::from(x + self.dx), -f64::from(y));
        let (x0, y0, x1, y1) = self.bounds;
        self.bounds = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        format!("{} {}", n(x), n(y))
    }
}

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        let p = self.point(x, y);
        self.d.push_str(&format!("M{p}"));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.point(x, y);
        self.d.push_str(&format!("L{p}"));
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let c = self.point(cx, cy);
        let p = self.point(x, y);
        self.d.push_str(&format!("Q{c} {p}"));
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let c1 = self.point(cx0, cy0);
        let c2 = self.point(cx1, cy1);
        let p = self.point(x, y);
        self.d.push_str(&format!("C{c1} {c2} {p}"));
    }
    fn close(&mut self) {
        self.d.push('Z');
    }
}

/// The word from the WOFF2 file of Source Sans 3's variable font.
pub fn word(woff2: &[u8]) -> Result<Word, String> {
    let ttf = wuff::decompress_woff2(woff2).map_err(|e| format!("font: {e:?}"))?;
    let font = FontRef::new(&ttf).map_err(|e| format!("font: {e}"))?;
    if !font
        .axes()
        .iter()
        .any(|axis| axis.tag() == Tag::new(b"wght"))
    {
        return Err("font: no weight axis".into());
    }
    let location = font.axes().location([("wght", WEIGHT)]);
    let units_per_em = font
        .head()
        .map_err(|e| format!("font: {e}"))?
        .units_per_em();
    let tracking = TRACKING * f32::from(units_per_em);
    let metrics = font.glyph_metrics(Size::unscaled(), &location);
    let outlines = font.outline_glyphs();
    let charmap = font.charmap();
    let mut pen = Pen {
        d: String::new(),
        dx: 0.0,
        bounds: (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
    };
    for ch in TEXT.chars() {
        let glyph = charmap
            .map(ch)
            .ok_or_else(|| format!("font: no glyph for {ch}"))?;
        outlines
            .get(glyph)
            .ok_or_else(|| format!("font: no outline for {ch}"))?
            .draw(
                DrawSettings::unhinted(Size::unscaled(), &location),
                &mut pen,
            )
            .map_err(|e| format!("font: {ch}: {e}"))?;
        let advance = metrics
            .advance_width(glyph)
            .ok_or_else(|| format!("font: no advance for {ch}"))?;
        pen.dx += advance + tracking;
    }
    Ok(Word {
        path: pen.d,
        bounds: pen.bounds,
    })
}
