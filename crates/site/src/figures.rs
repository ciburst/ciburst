//! The two drawings of the home page, SVG generated at build time (spec 11.5): a measured
//! pipeline of the demo and the VM hour.

use serde::Deserialize;

use crate::markdown::escape;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pipeline {
    pub caption: String,
    pub axis_s: f64,
    pub jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub name: String,
    pub runner: String,
    pub kind: Kind,
    /// When the job went pending, seconds after the pipeline started.
    pub start_s: f64,
    /// When a runner took it.
    pub run_s: f64,
    pub end_s: f64,
    /// When the lane run exited after its idle window; lanes only.
    pub exit_s: Option<f64>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Lane,
    Vm,
}

pub fn parse_pipeline(json: &str) -> Result<Pipeline, String> {
    let p: Pipeline = serde_json::from_str(json).map_err(|e| format!("pipeline.json: {e}"))?;
    for job in &p.jobs {
        let exit = job.exit_s.unwrap_or(job.end_s);
        let ordered = 0.0 <= job.start_s
            && job.start_s <= job.run_s
            && job.run_s <= job.end_s
            && job.end_s <= exit
            && exit <= p.axis_s;
        if !ordered || (job.kind == Kind::Lane) != job.exit_s.is_some() {
            return Err(format!(
                "pipeline.json: job {} needs start <= run <= end <= exit <= axisS, and exitS exactly for lanes",
                job.name
            ));
        }
    }
    Ok(p)
}

const X0: f64 = 160.0;
const WIDTH: f64 = 540.0;
const ROW: f64 = 26.0;
const GAP: f64 = 12.0;
const TOP: f64 = 22.0;

fn rect(class: &str, x: f64, y: f64, w: f64, h: f64) -> String {
    format!(
        "<rect class=\"{class}\" x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\"/>"
    )
}

fn axis(out: &mut String, x0: f64, x1: f64, y: f64, ticks: &[(f64, String)]) {
    out.push_str(&format!(
        "<line class=\"p-axis\" x1=\"{x0:.1}\" y1=\"{y:.1}\" x2=\"{x1:.1}\" y2=\"{y:.1}\"/>"
    ));
    let last = ticks.len().saturating_sub(1);
    for (i, (x, label)) in ticks.iter().enumerate() {
        out.push_str(&format!(
            "<line class=\"p-axis\" x1=\"{x:.1}\" y1=\"{y:.1}\" x2=\"{x:.1}\" y2=\"{:.1}\"/>",
            y + 5.0
        ));
        let anchor = if i == last { "end" } else { "middle" };
        out.push_str(&format!(
            "<text class=\"p-tick\" x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"{anchor}\">{}</text>",
            y + 18.0,
            escape(label)
        ));
    }
}

/// The pipeline: one row per job, lane runs hatched while they start, solid while the job runs,
/// light while they wait for the next job; VM jobs solid on the band of a running VM.
pub fn pipeline_figure(p: &Pipeline) -> String {
    let scale = WIDTH / p.axis_s;
    let x = |s: f64| X0 + s * scale;
    let height = TOP + p.jobs.len() as f64 * (ROW + GAP) + 36.0;
    let axis_y = TOP + p.jobs.len() as f64 * (ROW + GAP) + 4.0;
    let mut svg = format!(
        "<svg viewBox=\"0 0 720 {height:.0}\" role=\"img\" aria-label=\"{}\">",
        escape(&p.caption)
    );
    for (i, job) in p.jobs.iter().enumerate() {
        let y = TOP + i as f64 * (ROW + GAP);
        match job.kind {
            Kind::Lane => {
                let exit = job.exit_s.unwrap_or(job.end_s);
                svg.push_str(&rect(
                    "p-lane-start",
                    x(job.start_s),
                    y,
                    (job.run_s - job.start_s) * scale,
                    ROW,
                ));
                svg.push_str(&rect(
                    "p-lane",
                    x(job.run_s),
                    y,
                    (job.end_s - job.run_s) * scale,
                    ROW,
                ));
                svg.push_str(&rect(
                    "p-lane-wait",
                    x(job.end_s),
                    y,
                    (exit - job.end_s) * scale,
                    ROW,
                ));
            }
            Kind::Vm => {
                svg.push_str(&rect("p-vm-up", X0, y, WIDTH, ROW));
                svg.push_str(&rect(
                    "p-vm",
                    x(job.run_s),
                    y,
                    (job.end_s - job.run_s) * scale,
                    ROW,
                ));
            }
        }
        svg.push_str(&format!(
            "<text class=\"p-name\" x=\"150\" y=\"{:.1}\" text-anchor=\"end\">{}</text>",
            y + 11.0,
            escape(&job.name)
        ));
        svg.push_str(&format!(
            "<text class=\"p-where\" x=\"150\" y=\"{:.1}\" text-anchor=\"end\">{}</text>",
            y + 23.0,
            escape(&job.runner)
        ));
    }
    let step = 20.0;
    let mut ticks = Vec::new();
    let mut s = 0.0;
    while s <= p.axis_s + 0.001 {
        ticks.push((x(s), format!("{s:.0}")));
        s += step;
    }
    if let Some(last) = ticks.last_mut() {
        last.1.push_str(" s");
    }
    axis(&mut svg, X0, X0 + WIDTH, axis_y, &ticks);
    svg.push_str("</svg>");
    format!(
        "<figure class=\"fig\"><div class=\"fig-scroll\">{svg}</div>\
         <ul class=\"legend\">\
         <li><span class=\"k k-lane-start\"></span>Serverless Job starting</li>\
         <li><span class=\"k k-lane\"></span>job in a lane</li>\
         <li><span class=\"k k-lane-wait\"></span>waiting 60 s for the next job, then gone</li>\
         <li><span class=\"k k-vm\"></span>job on a VM</li></ul>\
         <figcaption>{}</figcaption></figure>",
        escape(&p.caption)
    )
}

/// Minute at which an idle VM ends itself, before its second hour starts (README, Cost and
/// limits; spec section 6.2).
pub const VM_END_MINUTE: f64 = 55.0;
/// Boot in minutes, about 90 s.
const BOOT_MIN: f64 = 1.5;
/// Jobs of three pipelines in the two slots of a DEV1-M: slot, start and end in minutes.
const HOUR_JOBS: &[(usize, f64, f64)] = &[
    (0, 2.0, 3.6),
    (0, 21.0, 24.0),
    (0, 38.0, 39.2),
    (1, 2.0, 2.6),
    (1, 21.2, 23.0),
];

/// The VM hour: boot, two slots, idle until minute 55.
pub fn hour_figure() -> String {
    let x0 = 88.0;
    let scale = 10.0;
    let x = |m: f64| x0 + m * scale;
    let mut svg = String::from(
        "<svg viewBox=\"0 0 720 112\" role=\"img\" aria-label=\"One VM hour on a 60 minute axis: \
         about 90 seconds of boot, jobs of three pipelines in two slots, then idle until minute 55, \
         when the VM ends itself.\">",
    );
    svg.push_str(&rect("p-vm-up", x0, 20.0, VM_END_MINUTE * scale, 52.0));
    for slot in 0..2 {
        let y = 22.0 + slot as f64 * 26.0;
        svg.push_str(&rect("h-boot", x0, y, BOOT_MIN * scale, 22.0));
        for (_, start, end) in HOUR_JOBS.iter().filter(|(s, _, _)| *s == slot) {
            svg.push_str(&rect("p-vm", x(*start), y, (end - start) * scale, 22.0));
        }
        svg.push_str(&format!(
            "<text class=\"p-where\" x=\"80\" y=\"{:.1}\" text-anchor=\"end\">slot {}</text>",
            y + 15.0,
            slot + 1
        ));
    }
    let end = x(VM_END_MINUTE);
    svg.push_str(&format!(
        "<line class=\"h-stop\" x1=\"{end:.1}\" y1=\"16\" x2=\"{end:.1}\" y2=\"78\"/>\
         <text class=\"h-stoptext\" x=\"{:.1}\" y=\"12\" text-anchor=\"end\">VM ends itself, minute 55</text>",
        end - 6.0
    ));
    let ticks: Vec<(f64, String)> = [0.0, 15.0, 30.0, 45.0, VM_END_MINUTE, 60.0]
        .iter()
        .map(|m| {
            (
                x(*m),
                if *m == 60.0 {
                    "60 min".to_string()
                } else {
                    format!("{m:.0}")
                },
            )
        })
        .collect();
    axis(&mut svg, x0, x(60.0), 84.0, &ticks);
    svg.push_str("</svg>");
    format!(
        "<figure class=\"fig\"><div class=\"fig-scroll\">{svg}</div>\
         <figcaption>One DEV1-M hour, 0.03 EUR: about 90 s of boot (hatched), two slots for the jobs \
         of three pipelines, then idle until minute 55, when the VM ends itself before a second \
         hour starts.</figcaption></figure>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_measured_pipeline_parses_and_draws_to_scale() {
        let p = parse_pipeline(include_str!("../assets/pipeline.json")).unwrap();
        assert_eq!(p.jobs.len(), 4);
        let svg = pipeline_figure(&p);
        // node-test runs from 32.7 s to 37.1 s: x = 160 + 32.7 * 5.4.
        assert!(
            svg.contains("class=\"p-lane\" x=\"336.6\" y=\"22.0\" width=\"23.8\""),
            "{svg}"
        );
        assert!(svg.contains(">100 s</text>"), "{svg}");
    }

    #[test]
    fn a_job_out_of_order_is_refused() {
        let json = r#"{ "caption": "c", "axisS": 10, "jobs": [
            { "name": "a", "runner": "VM", "kind": "vm", "startS": 5, "runS": 4, "endS": 6 } ] }"#;
        assert!(parse_pipeline(json).unwrap_err().contains("job a"));
        let lane_without_exit = r#"{ "caption": "c", "axisS": 10, "jobs": [
            { "name": "b", "runner": "lane", "kind": "lane", "startS": 1, "runS": 2, "endS": 3 } ] }"#;
        assert!(parse_pipeline(lane_without_exit).is_err());
    }

    #[test]
    fn the_hour_ends_at_minute_55() {
        let svg = hour_figure();
        assert!(svg.contains("x1=\"638.0\""), "{svg}");
        assert!(svg.contains(">60 min</text>"), "{svg}");
    }
}
