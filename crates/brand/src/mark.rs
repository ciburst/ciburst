//! The mark (spec 11.6): a cog in front of a burst that bursts out on its right. Lengths in mark
//! units, the cog's center at the origin, angles in degrees clockwise from the right.

use crate::n;

/// Outer radius, root radius and hole of the cog.
const OUTER: f64 = 34.0;
const ROOT: f64 = 27.0;
const HOLE: f64 = 11.0;
const TEETH: u32 = 8;
/// Half the angle a tooth covers at its top and at its root.
const TOP_HALF: f64 = 10.0;
const ROOT_HALF: f64 = 15.0;
/// The first tooth's angle: no tooth points straight at the burst.
const START: f64 = 22.5;

/// The burst's center, its spikes as (angle, radius) and the radius of the notch between two.
const BURST_CENTER: (f64, f64) = (24.0, 0.0);
const SPIKES: [(f64, f64); 13] = [
    (-6.0, 56.0),
    (17.0, 40.0),
    (38.0, 50.0),
    (63.0, 38.0),
    (88.0, 46.0),
    (120.0, 30.0),
    (160.0, 28.0),
    (200.0, 28.0),
    (240.0, 30.0),
    (268.0, 47.0),
    (292.0, 37.0),
    (316.0, 52.0),
    (340.0, 39.0),
];
const NOTCH: f64 = 27.0;

/// The cut between cog and burst, as the width of a stroke around the cog: half of it each side.
const CUT: f64 = 7.0;
/// The cog's own stroke, which rounds its corners.
const ROUNDING: f64 = 1.5;

fn polar(center: (f64, f64), radius: f64, degrees: f64) -> (f64, f64) {
    let a = degrees.to_radians();
    (center.0 + radius * a.cos(), center.1 + radius * a.sin())
}

fn at(p: (f64, f64)) -> String {
    format!("{} {}", n(p.0), n(p.1))
}

/// The cog's outline without the hole.
fn cog() -> String {
    let pitch = 360.0 / f64::from(TEETH);
    let mut d = format!("M{}", at(polar((0.0, 0.0), ROOT, START - ROOT_HALF)));
    for i in 0..TEETH {
        let a = START + f64::from(i) * pitch;
        d.push_str(&format!(
            "L{}A{o} {o} 0 0 1 {}L{}A{r} {r} 0 0 1 {}",
            at(polar((0.0, 0.0), OUTER, a - TOP_HALF)),
            at(polar((0.0, 0.0), OUTER, a + TOP_HALF)),
            at(polar((0.0, 0.0), ROOT, a + ROOT_HALF)),
            at(polar((0.0, 0.0), ROOT, a + pitch - ROOT_HALF)),
            o = n(OUTER),
            r = n(ROOT),
        ));
    }
    d.push('Z');
    d
}

fn hole() -> String {
    format!(
        "M{h} 0A{h} {h} 0 1 0 -{h} 0A{h} {h} 0 1 0 {h} 0Z",
        h = n(HOLE)
    )
}

/// The burst's corners: each spike, then the notch halfway to the next.
fn burst_points() -> Vec<(f64, f64)> {
    let mut points = Vec::new();
    for (i, &(a, r)) in SPIKES.iter().enumerate() {
        let mut next = SPIKES[(i + 1) % SPIKES.len()].0;
        if next < a {
            next += 360.0;
        }
        points.push(polar(BURST_CENTER, r, a));
        points.push(polar(BURST_CENTER, NOTCH, (a + next) / 2.0));
    }
    points
}

fn burst() -> String {
    let mut d = String::new();
    for (i, p) in burst_points().into_iter().enumerate() {
        d.push(if i == 0 { 'M' } else { 'L' });
        d.push_str(&at(p));
    }
    d.push('Z');
    d
}

/// The mark's extent: left, top, right, bottom.
pub fn bounds() -> (f64, f64, f64, f64) {
    burst_points().into_iter().fold(
        (-OUTER, -OUTER, OUTER, OUTER),
        |(x0, y0, x1, y1), (x, y)| (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
    )
}

/// The mark as SVG elements in mark units. `id` names its mask, unique within one file.
pub fn mark(ink: &str, burst_fill: &str, id: &str) -> String {
    let (x0, y0, x1, y1) = bounds();
    let cog = cog();
    format!(
        "<mask id=\"{id}\" maskUnits=\"userSpaceOnUse\" x=\"{mx}\" y=\"{my}\" width=\"{mw}\" height=\"{mh}\">\
         <rect x=\"{mx}\" y=\"{my}\" width=\"{mw}\" height=\"{mh}\" fill=\"#fff\"/>\
         <path d=\"{cog}\" stroke=\"#000\" stroke-width=\"{cut}\" stroke-linejoin=\"round\"/></mask>\
         <path d=\"{burst}\" fill=\"{burst_fill}\" mask=\"url(#{id})\"/>\
         <path d=\"{cog}{hole}\" fill=\"{ink}\" fill-rule=\"evenodd\" stroke=\"{ink}\" \
         stroke-width=\"{rounding}\" stroke-linejoin=\"round\"/>",
        mx = n(x0 - 2.0),
        my = n(y0 - 2.0),
        mw = n(x1 - x0 + 4.0),
        mh = n(y1 - y0 + 4.0),
        cut = n(CUT),
        burst = burst(),
        hole = hole(),
        rounding = n(ROUNDING),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cog_closes_where_it_started() {
        let d = cog();
        let first = d[1..].split('L').next().unwrap();
        let last_arc = d
            .trim_end_matches('Z')
            .rsplit(' ')
            .take(2)
            .collect::<Vec<_>>();
        assert_eq!(format!("{} {}", last_arc[1], last_arc[0]), first);
        assert_eq!(d.matches('A').count(), 2 * TEETH as usize);
    }

    #[test]
    fn the_last_notch_lies_between_the_last_and_the_first_spike() {
        let points = burst_points();
        assert_eq!(points.len(), 2 * SPIKES.len());
        let notch = points[points.len() - 1];
        let expected = polar(BURST_CENTER, NOTCH, 347.0);
        assert!((notch.0 - expected.0).abs() < 1e-9 && (notch.1 - expected.1).abs() < 1e-9);
    }

    #[test]
    fn the_burst_reaches_past_the_cog_on_the_right_only() {
        let (x0, _, x1, _) = bounds();
        assert_eq!(x0, -OUTER, "the cog is the left edge");
        assert!(x1 > OUTER + 40.0, "the burst sticks out on the right: {x1}");
    }
}
