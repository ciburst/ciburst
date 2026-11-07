//! The animation of a pipeline through ciburst (spec 11.5): a measured run of a web app's
//! pipeline, from the push to the moment nothing runs. One SVG with CSS keyframes, no script, in a
//! wide layout for screens and the README and a narrow one for phones. Each animated element
//! carries its state at `stillS` as its own style, which readers who ask for less motion see.

use serde::Deserialize;

use crate::markdown::escape;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Flow {
    pub caption: String,
    /// How long a lane run waits for another job of its image before it ends.
    pub idle_s: f64,
    /// The moment shown without motion.
    pub still_s: f64,
    pub vm: Vm,
    pub stages: Vec<String>,
    pub jobs: Vec<FlowJob>,
    pub captions: Vec<Caption>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Vm {
    pub slots: usize,
    /// When the dispatcher created the VM, and when its first job started on it.
    pub boot_s: f64,
    pub ready_s: f64,
    /// The minute of its paid hour at which the idle VM ends itself.
    pub end_minute: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FlowJob {
    pub name: String,
    pub stage: String,
    /// The lane's tag, or `vm`.
    pub on: String,
    pub pending_s: f64,
    pub start_s: f64,
    pub end_s: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Caption {
    pub at_s: f64,
    pub text: String,
}

impl FlowJob {
    fn lane(&self) -> Option<&str> {
        (self.on != "vm").then_some(self.on.as_str())
    }
}

impl Flow {
    fn vm_end(&self) -> f64 {
        self.vm.boot_s + self.vm.end_minute * 60.0
    }

    /// When a job's lane run ends: its idle window after the job.
    fn exit(&self, job: &FlowJob) -> f64 {
        job.end_s + self.idle_s
    }
}

/// Lane runs of one tag side by side, at most.
const TILES: usize = 4;

pub fn parse(json: &str) -> Result<Flow, String> {
    let f: Flow = serde_json::from_str(json).map_err(|e| format!("flow.json: {e}"))?;
    let err = |m: String| Err(format!("flow.json: {m}"));
    for job in &f.jobs {
        if !f.stages.contains(&job.stage) {
            return err(format!(
                "{}: stage {} is not in stages",
                job.name, job.stage
            ));
        }
        if !(0.0 <= job.pending_s && job.pending_s <= job.start_s && job.start_s <= job.end_s) {
            return err(format!("{}: needs pendingS <= startS <= endS", job.name));
        }
        if job.lane().is_none() && job.start_s < f.vm.ready_s {
            return err(format!("{}: starts before the VM is ready", job.name));
        }
    }
    if !(f.vm.boot_s <= f.vm.ready_s && f.vm.slots >= 1) {
        return err("vm: needs bootS <= readyS and a slot".into());
    }
    let last = f.jobs.iter().map(|j| f.exit(j)).fold(0.0, f64::max);
    if f.vm_end() < last {
        return err("vm: ends before the last lane".into());
    }
    if f.captions.first().map(|c| c.at_s) != Some(0.0)
        || f.captions.windows(2).any(|w| w[1].at_s <= w[0].at_s)
        || f.captions.last().map(|c| (c.at_s - f.vm_end()).abs() < 0.5) != Some(true)
    {
        return err("captions: from 0, in order, the last at the VM's end".into());
    }
    if !(0.0..f.vm_end()).contains(&f.still_s) {
        return err("stillS: within the run".into());
    }
    for (tag, max) in [("vm", f.vm.slots), ("", TILES)] {
        for job in &f.jobs {
            let same = |j: &&FlowJob| {
                if tag == "vm" {
                    j.lane().is_none() && job.lane().is_none()
                } else {
                    j.lane().is_some() && j.lane() == job.lane()
                }
            };
            let (from, to) = if tag == "vm" {
                (job.start_s, job.end_s)
            } else {
                (job.pending_s, f.exit(job))
            };
            let _ = to;
            let busy = f
                .jobs
                .iter()
                .filter(same)
                .filter(|j| {
                    let (a, b) = if tag == "vm" {
                        (j.start_s, j.end_s)
                    } else {
                        (j.pending_s, f.exit(j))
                    };
                    a <= from && from < b
                })
                .count();
            if busy > max {
                return err(format!(
                    "{}: more than {max} at once on {}",
                    job.name, job.on
                ));
            }
        }
    }
    Ok(f)
}

/// Lengths with at most two decimals, no trailing zeros.
fn n(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0" } else { s }.to_string()
}

/// `m:ss` of a moment of the pipeline.
fn clock(s: f64) -> String {
    let s = s.round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Seconds of animation before the first event, for each burst of job events, and after the
/// VM's end.
const PUSH: f64 = 1.6;
const TRAVEL: f64 = 1.6;
const HOLD: f64 = 3.6;
const FADE: f64 = 0.15;
/// The animation seconds a step between two events gets: from `STEP_MIN` for an instant to
/// `STEP_MIN + STEP_MAX` for a long wait, `STEP_SCALE` pipeline seconds setting the curve.
const STEP_MIN: f64 = 0.06;
const STEP_MAX: f64 = 1.1;
const STEP_SCALE: f64 = 45.0;

/// Maps the pipeline's seconds to the animation's: each step between two events gets time to be
/// seen, long waits pass quickly, and each burst of job events pauses the clock while the events
/// travel.
struct Timeline {
    points: Vec<(f64, f64)>,
    bursts: Vec<(f64, f64)>,
    /// The animation second each burst's events leave, before its pause.
    departs: Vec<f64>,
    total: f64,
}

impl Timeline {
    fn new(f: &Flow) -> Timeline {
        let mut pendings: Vec<f64> = f.jobs.iter().map(|j| j.pending_s).collect();
        pendings.sort_by(f64::total_cmp);
        let mut bursts: Vec<(f64, f64)> = Vec::new();
        for p in pendings {
            match bursts.last_mut() {
                Some(b) if p - b.1 <= 3.0 => b.1 = p,
                _ => bursts.push((p, p)),
            }
        }
        let mut events: Vec<f64> = vec![0.0, f.vm.boot_s, f.vm.ready_s, f.vm_end()];
        for j in &f.jobs {
            events.extend([j.pending_s, j.start_s, j.end_s]);
            if j.lane().is_some() {
                events.push(f.exit(j));
            }
        }
        events.extend(f.captions.iter().map(|c| c.at_s));
        events.sort_by(f64::total_cmp);
        events.dedup_by(|a, b| (*a - *b).abs() < 0.3);
        let mut points = vec![(0.0, PUSH)];
        let mut anim = PUSH;
        let mut departs = vec![PUSH; bursts.len()];
        for w in events.windows(2) {
            let (r0, r1) = (w[0], w[1]);
            // The pause follows the last event at or before the burst's last job event.
            if let Some(i) = bursts.iter().position(|b| r0 <= b.1 && b.1 < r1) {
                departs[i] = anim;
                anim += TRAVEL;
                points.push((r0, anim));
            }
            anim += STEP_MIN + STEP_MAX * (1.0 - (-(r1 - r0) / STEP_SCALE).exp());
            points.push((r1, anim));
        }
        let total = anim + HOLD;
        Timeline {
            points,
            bursts,
            departs,
            total,
        }
    }

    /// The animation second of a pipeline second: at a burst's last event, before its pause.
    fn at(&self, t: f64) -> f64 {
        let p = &self.points;
        if t <= p[0].0 {
            return p[0].1;
        }
        for w in p.windows(2) {
            let ((r0, a0), (r1, a1)) = (w[0], w[1]);
            if r1 <= r0 {
                continue;
            }
            if t <= r1 {
                return a0 + (t - r0) / (r1 - r0) * (a1 - a0);
            }
        }
        p[p.len() - 1].1
    }

    /// When a job's event leaves the pipeline and when it arrives, staggered within its burst.
    fn travel(&self, f: &Flow, job: &FlowJob) -> (f64, f64) {
        let i = self
            .bursts
            .iter()
            .position(|b| b.0 <= job.pending_s && job.pending_s <= b.1)
            .expect("every job event belongs to a burst");
        let burst = self.bursts[i];
        let mut peers: Vec<&FlowJob> = f
            .jobs
            .iter()
            .filter(|j| burst.0 <= j.pending_s && j.pending_s <= burst.1)
            .collect();
        peers.sort_by(|a, b| a.pending_s.total_cmp(&b.pending_s));
        let k = peers.iter().position(|j| j.name == job.name).unwrap_or(0) as f64;
        let count = peers.len() as f64;
        let step = (0.4 / count).min(0.08);
        let start = self.departs[i];
        (start + step * k, start + TRAVEL - step * (count - 1.0 - k))
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Layout {
    Wide,
    Narrow,
}

type Rect = (f64, f64, f64, f64);

/// Where everything is drawn, per layout.
struct Geo {
    id: &'static str,
    w: f64,
    h: f64,
    panel: Rect,
    title_y: f64,
    stage_y: f64,
    col_x0: f64,
    col_w: f64,
    chip_w: f64,
    chip_h: f64,
    chip_y0: f64,
    chip_step: f64,
    names: bool,
    dispatch: Rect,
    junction_y: f64,
    lanes: Rect,
    lanes_in: f64,
    row_y0: f64,
    row_step: f64,
    tag_x: f64,
    tile_x0: f64,
    tile_w: f64,
    tile_h: f64,
    tile_step: f64,
    vm: Rect,
    vm_in: f64,
    vm_box: Rect,
    slot_x: f64,
    slot_w: f64,
    slot_h: f64,
    slot_y0: f64,
    slot_step: f64,
    caption_y: f64,
    caption_chars: usize,
    caption_x: f64,
    token: (f64, f64),
    big: f64,
    small: f64,
}

fn geo(layout: Layout) -> Geo {
    match layout {
        Layout::Wide => Geo {
            id: "fw",
            w: 960.0,
            h: 590.0,
            panel: (12.0, 12.0, 936.0, 250.0),
            title_y: 36.0,
            stage_y: 54.0,
            col_x0: 24.0,
            col_w: 182.4,
            chip_w: 170.0,
            chip_h: 22.0,
            chip_y0: 62.0,
            chip_step: 27.0,
            names: true,
            dispatch: (418.0, 284.0, 124.0, 44.0),
            junction_y: 346.0,
            lanes: (12.0, 364.0, 600.0, 168.0),
            lanes_in: 480.0,
            row_y0: 398.0,
            row_step: 32.0,
            tag_x: 24.0,
            tile_x0: 110.0,
            tile_w: 118.0,
            tile_h: 24.0,
            tile_step: 122.0,
            vm: (636.0, 364.0, 312.0, 168.0),
            vm_in: 920.0,
            vm_box: (648.0, 396.0, 288.0, 86.0),
            slot_x: 660.0,
            slot_w: 264.0,
            slot_h: 26.0,
            slot_y0: 408.0,
            slot_step: 34.0,
            caption_y: 566.0,
            caption_chars: 112,
            caption_x: 74.0,
            token: (22.0, 10.0),
            big: 15.0,
            small: 11.0,
        },
        Layout::Narrow => Geo {
            id: "fn",
            w: 400.0,
            h: 640.0,
            panel: (8.0, 8.0, 384.0, 184.0),
            title_y: 28.0,
            stage_y: 44.0,
            col_x0: 16.0,
            col_w: 73.6,
            chip_w: 66.0,
            chip_h: 14.0,
            chip_y0: 52.0,
            chip_step: 19.0,
            names: false,
            dispatch: (142.0, 210.0, 116.0, 40.0),
            junction_y: 264.0,
            lanes: (8.0, 278.0, 376.0, 152.0),
            lanes_in: 340.0,
            row_y0: 310.0,
            row_step: 28.0,
            tag_x: 16.0,
            tile_x0: 92.0,
            tile_w: 68.0,
            tile_h: 20.0,
            tile_step: 72.0,
            vm: (8.0, 442.0, 376.0, 122.0),
            vm_in: 392.0,
            vm_box: (16.0, 472.0, 360.0, 70.0),
            slot_x: 26.0,
            slot_w: 340.0,
            slot_h: 24.0,
            slot_y0: 481.0,
            slot_step: 28.0,
            caption_y: 590.0,
            caption_chars: 52,
            caption_x: 56.0,
            token: (16.0, 8.0),
            big: 13.0,
            small: 10.0,
        },
    }
}

/// Keyframes of one drawing, each element's under its own name.
struct Anim<'a> {
    id: &'a str,
    css: String,
    count: usize,
    total: f64,
    still: f64,
}

impl Anim<'_> {
    /// Frames `(second, declarations)` from 0 to the loop's end. Returns the element's style:
    /// its keyframes' name and its state at the still moment.
    fn frames(&mut self, frames: &[(f64, String)], still: &str) -> String {
        let name = format!("{}{}", self.id, self.count);
        self.count += 1;
        let mut body = String::new();
        let mut last = -1.0;
        for (t, decl) in frames {
            let pct = (t / self.total * 100.0).clamp(0.0, 100.0);
            let pct = if pct <= last { last + 0.01 } else { pct };
            last = pct;
            body.push_str(&format!("{}%{{{decl}}}", n(pct)));
        }
        self.css.push_str(&format!("@keyframes {name}{{{body}}}"));
        format!("--a:{name};{still}")
    }

    /// States that each begin at a second; changes take `FADE`.
    fn states(&mut self, states: &[(f64, String)]) -> String {
        let mut frames = vec![(0.0, states[0].1.clone())];
        for w in states.windows(2) {
            frames.push((w[1].0, w[0].1.clone()));
            frames.push((w[1].0 + FADE, w[1].1.clone()));
        }
        frames.push((self.total, states[states.len() - 1].1.clone()));
        let still = states
            .iter()
            .rev()
            .find(|(t, _)| *t <= self.still)
            .map_or(states[0].1.clone(), |s| s.1.clone());
        self.frames(&frames, &still)
    }

    /// Visible between two seconds.
    fn window(&mut self, on: f64, off: f64) -> String {
        self.states(&[
            (0.0, "opacity:0".into()),
            (on, "opacity:1".into()),
            (off, "opacity:0".into()),
        ])
    }
}

fn rect(class: &str, r: Rect, rx: f64, style: &str) -> String {
    let style = if style.is_empty() {
        String::new()
    } else {
        format!(" style=\"{style}\"")
    };
    format!(
        "<rect class=\"{class}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"{}\"{style}/>",
        n(r.0),
        n(r.1),
        n(r.2),
        n(r.3),
        n(rx)
    )
}

fn text(class: &str, x: f64, y: f64, anchor: &str, size: f64, s: &str, style: &str) -> String {
    let style = if style.is_empty() {
        String::new()
    } else {
        format!(" style=\"{style}\"")
    };
    format!(
        "<text class=\"{class}\" x=\"{}\" y=\"{}\" text-anchor=\"{anchor}\" font-size=\"{}\"{style}>{}</text>",
        n(x),
        n(y),
        n(size),
        escape(s)
    )
}

fn wrap(s: &str, chars: usize) -> Vec<String> {
    let mut lines: Vec<String> = vec![String::new()];
    for word in s.split(' ') {
        let line = lines.last_mut().expect("a line");
        if !line.is_empty() && line.len() + 1 + word.len() > chars {
            lines.push(word.to_string());
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    lines
}

/// The rules the drawing needs beyond its keyframes. `--paper` and the other colors come from
/// the page, or from the README file's tile.
const STYLE: &str = "\
.fl-t{font-family:var(--sans);fill:var(--ink)}.fl-2{fill:var(--ink-2)}.fl-b{font-weight:600}\
.fl-num{font-weight:700;font-variant-numeric:tabular-nums}\
.fl-panel{fill:var(--sunk);stroke:var(--rule)}.fl-box{fill:var(--paper);stroke:var(--ink-2);stroke-width:1.5}\
.fl-line{stroke:var(--ink-2);stroke-width:1.5;fill:none}\
.fl-lane-line{stroke:var(--lane);stroke-width:2;fill:none}.fl-vm-line{stroke:var(--vm);stroke-width:2;fill:none}\
.fl-lane{fill:var(--lane)}.fl-vm{fill:var(--vm)}.fl-stop{fill:var(--stop)}\
.fl-pool{fill:none;stroke:var(--rule)}.fl-chip{stroke-width:1}\
.fl-tile{stroke:var(--lane);stroke-width:1.5}.fl-halo{paint-order:stroke;stroke-width:3px;stroke-linejoin:round}\
.fl-slot{fill:none;stroke:var(--rule);stroke-dasharray:4 3}.fl-machine{fill:none;stroke-width:1.5}\
.fl-token{fill:var(--ink)}.fl-on{fill:var(--paper)}\
.fl-hatch-bg{fill:var(--paper)}.fl-hatch-line{stroke:var(--lane);stroke-width:1.5}\
.fl-vhatch-line{stroke:var(--vm);stroke-width:1.5}";

/// The animation as one `<svg>`. `standalone` is the README file's own colors and font, drawn on
/// a tile; without it the page provides them.
pub fn flow_svg(f: &Flow, layout: Layout, standalone: Option<&str>) -> String {
    let g = geo(layout);
    let tl = Timeline::new(f);
    let mut a = Anim {
        id: g.id,
        css: String::new(),
        count: 0,
        total: tl.total,
        still: tl.at(f.still_s),
    };
    let id = g.id;
    let mut fixed = String::new();
    let mut moving = String::new();
    let mut top = String::new();

    // GitLab's pipeline: a column per stage, a chip per job.
    fixed.push_str(&rect("fl-panel", g.panel, 6.0, ""));
    fixed.push_str(&text(
        "fl-t fl-b",
        g.col_x0,
        g.title_y,
        "start",
        g.big - 1.0,
        "GitLab pipeline",
        "",
    ));
    let mut chip_at: Vec<(f64, f64)> = Vec::new();
    for (i, stage) in f.stages.iter().enumerate() {
        let x = g.col_x0 + g.col_w * i as f64;
        fixed.push_str(&text(
            "fl-t fl-2",
            x,
            g.stage_y,
            "start",
            g.small,
            stage,
            "",
        ));
    }
    for job in &f.jobs {
        let col = f.stages.iter().position(|s| *s == job.stage).unwrap_or(0);
        let row = f
            .jobs
            .iter()
            .filter(|j| j.stage == job.stage)
            .position(|j| j.name == job.name)
            .unwrap_or(0);
        let r = (
            g.col_x0 + g.col_w * col as f64,
            g.chip_y0 + g.chip_step * row as f64,
            g.chip_w,
            g.chip_h,
        );
        chip_at.push((r.0 + r.2 / 2.0, r.1 + r.3 / 2.0));
        let color = if job.lane().is_some() {
            "var(--lane)"
        } else {
            "var(--vm)"
        };
        let waiting = "fill:var(--paper);stroke:var(--ink-2);stroke-dasharray:3 2".to_string();
        let running = format!("fill:{color};stroke:{color};stroke-dasharray:none");
        let passed = "fill:var(--sunk);stroke:var(--rule);stroke-dasharray:none".to_string();
        let start = tl.at(job.start_s);
        let end = tl.at(job.end_s);
        let style = a.states(&[(0.0, waiting), (start, running), (end, passed)]);
        moving.push_str(&rect("fla fl-chip", r, 3.0, &style));
        if g.names {
            let style = a.states(&[
                (0.0, "fill:var(--ink)".into()),
                (start, "fill:var(--paper)".into()),
                (end, "fill:var(--ink-2)".into()),
            ]);
            moving.push_str(&text(
                "fla fl-t",
                r.0 + 8.0,
                r.1 + r.3 / 2.0 + 4.0,
                "start",
                g.small + 1.0,
                &job.name,
                &style,
            ));
        }
    }

    // Job events to the dispatcher, and the ways to the lanes and the VM.
    let (dx, dy, dw, dh) = g.dispatch;
    let dcx = dx + dw / 2.0;
    let panel_bottom = g.panel.1 + g.panel.3;
    fixed.push_str(&format!(
        "<path class=\"fl-line\" d=\"M{} {}V{}\"/>",
        n(dcx),
        n(panel_bottom),
        n(dy)
    ));
    fixed.push_str(&text(
        "fl-t fl-2",
        dcx + 8.0,
        (panel_bottom + dy) / 2.0 + 4.0,
        "start",
        g.small,
        "job events",
        "",
    ));
    let lanes_top = g.lanes.1;
    let vm_top = g.vm.1;
    fixed.push_str(&format!(
        "<path class=\"fl-lane-line\" d=\"M{c} {b}V{j}H{x}V{t}\"/>",
        c = n(dcx),
        b = n(dy + dh),
        j = n(g.junction_y),
        x = n(g.lanes_in),
        t = n(lanes_top)
    ));
    fixed.push_str(&format!(
        "<path class=\"fl-vm-line\" d=\"M{c} {j}H{x}V{t}\"/>",
        c = n(dcx),
        j = n(g.junction_y),
        x = n(g.vm_in),
        t = n(vm_top)
    ));
    top.push_str(&rect("fl-box", g.dispatch, 6.0, ""));
    top.push_str(&text(
        "fl-t fl-b",
        dcx,
        dy + dh / 2.0 - 1.0,
        "middle",
        g.big - 2.0,
        "dispatcher",
        "",
    ));
    top.push_str(&text(
        "fl-t fl-2",
        dcx,
        dy + dh / 2.0 + g.small + 2.0,
        "middle",
        g.small,
        "Scaleway Function",
        "",
    ));

    // The pools.
    fixed.push_str(&rect("fl-pool", g.lanes, 6.0, ""));
    fixed.push_str(&text(
        "fl-t fl-b fl-lane",
        g.lanes.0 + 12.0,
        g.lanes.1 + 20.0,
        "start",
        g.big - 2.0,
        "Lanes: Serverless Jobs, billed per second",
        "",
    ));
    fixed.push_str(&rect("fl-pool", g.vm, 6.0, ""));
    fixed.push_str(&text(
        "fl-t fl-b fl-vm",
        g.vm.0 + 12.0,
        g.vm.1 + 20.0,
        "start",
        g.big - 2.0,
        &format!("VM: Docker executor, {} slots", f.vm.slots),
        "",
    ));
    let mut tags: Vec<&str> = Vec::new();
    for job in &f.jobs {
        if let Some(tag) = job.lane()
            && !tags.contains(&tag)
        {
            tags.push(tag);
        }
    }
    for (i, tag) in tags.iter().enumerate() {
        let y = g.row_y0 + g.row_step * i as f64;
        fixed.push_str(&text(
            "fl-t fl-b fl-lane",
            g.tag_x,
            y + g.tile_h / 2.0 + 4.0,
            "start",
            g.small + 1.0,
            tag,
            "",
        ));
    }

    // Each lane run: opens with the job's event, starts, runs the job, waits, closes.
    let mut tile_of: Vec<Option<(f64, f64)>> = Vec::new();
    let mut taken: Vec<(String, usize, f64)> = Vec::new();
    let mut order: Vec<&FlowJob> = f.jobs.iter().collect();
    order.sort_by(|x, y| x.pending_s.total_cmp(&y.pending_s));
    let mut place: Vec<(String, f64, f64)> = Vec::new();
    for job in &order {
        let Some(tag) = job.lane() else { continue };
        let row = tags.iter().position(|t| *t == tag).unwrap_or(0);
        let col = (0..TILES)
            .find(|c| {
                !taken
                    .iter()
                    .any(|(t, cc, until)| t == tag && cc == c && *until > job.pending_s)
            })
            .unwrap_or(0);
        taken.push((tag.to_string(), col, f.exit(job)));
        place.push((
            job.name.clone(),
            g.tile_x0 + g.tile_step * col as f64,
            g.row_y0 + g.row_step * row as f64,
        ));
    }
    for job in &f.jobs {
        tile_of.push(
            place
                .iter()
                .find(|(nm, _, _)| *nm == job.name)
                .map(|(_, x, y)| (*x, *y)),
        );
    }
    for (job, tile) in f.jobs.iter().zip(&tile_of) {
        let Some((x, y)) = *tile else { continue };
        let (_, arrive) = tl.travel(f, job);
        let start = tl.at(job.start_s);
        let end = tl.at(job.end_s);
        let exit = tl.at(f.exit(job));
        let shut = format!(
            "opacity:1;transform:translateX({}px) scaleX(0)",
            n(g.tile_w)
        );
        let open = a.frames(
            &[
                (0.0, "opacity:0;transform:translateX(0) scaleX(0)".into()),
                (arrive, "opacity:0;transform:translateX(0) scaleX(0)".into()),
                (
                    arrive + 0.05,
                    "opacity:1;transform:translateX(0) scaleX(0)".into(),
                ),
                (
                    arrive + 0.3,
                    "opacity:1;transform:translateX(0) scaleX(1)".into(),
                ),
                (exit, "opacity:1;transform:translateX(0) scaleX(1)".into()),
                (exit + 0.3, shut.clone()),
                (exit + 0.35, format!("{shut};opacity:0")),
                (a.total, format!("{shut};opacity:0")),
            ],
            if (job.pending_s..f.exit(job)).contains(&f.still_s) {
                "opacity:1;transform:none"
            } else {
                "opacity:0"
            },
        );
        let fill = a.states(&[
            (0.0, format!("fill:url(#{id}-hatch)")),
            (start, "fill:var(--lane)".into()),
            (
                end,
                "fill:color-mix(in srgb,var(--lane) 22%,var(--paper))".into(),
            ),
        ]);
        let mut tile = rect("fla fl-tile", (x, y, g.tile_w, g.tile_h), 3.0, &fill);
        if g.names {
            let label = a.states(&[
                (0.0, "fill:var(--ink);stroke:var(--paper)".into()),
                (start, "fill:var(--paper);stroke:var(--lane)".into()),
                (end, "fill:var(--ink);stroke:transparent".into()),
            ]);
            tile.push_str(&text(
                "fla fl-t fl-b fl-halo",
                x + 7.0,
                y + g.tile_h / 2.0 + 4.0,
                "start",
                g.small,
                &job.name,
                &label,
            ));
        }
        moving.push_str(&format!(
            "<g class=\"fla\" style=\"{open};transform-box:fill-box;transform-origin:left\">{tile}</g>"
        ));
    }

    // The VM: none, booting, its slots, the end at minute 55.
    let boot = tl.at(f.vm.boot_s);
    let ready = tl.at(f.vm.ready_s);
    let vm_end = tl.at(f.vm_end());
    let machine = a.states(&[
        (0.0, "stroke:var(--rule);stroke-dasharray:4 3".into()),
        (boot, "stroke:var(--vm);stroke-dasharray:none".into()),
        (
            vm_end - 0.4,
            "stroke:var(--stop);stroke-dasharray:none".into(),
        ),
        (
            vm_end + 0.3,
            "stroke:var(--rule);stroke-dasharray:4 3".into(),
        ),
    ]);
    let (bx, by, bw, bh) = g.vm_box;
    let booting = a.window(boot, ready);
    moving.push_str(&format!(
        "<g class=\"fla\" style=\"{booting}\">{}{}</g>",
        rect("", g.vm_box, 6.0, "").replace("class=\"\"", &format!("fill=\"url(#{id}-vhatch)\"")),
        text(
            "fl-t fl-b fl-halo fl-vm",
            bx + 12.0,
            by + bh / 2.0 + 5.0,
            "start",
            g.big - 2.0,
            &format!("booting, {:.0} s", f.vm.ready_s - f.vm.boot_s),
            "stroke:var(--paper)"
        )
    ));
    moving.push_str(&rect("fla fl-machine", g.vm_box, 6.0, &machine));
    let none = a.states(&[
        (0.0, "opacity:1".into()),
        (boot, "opacity:0".into()),
        (vm_end + 0.3, "opacity:1".into()),
    ]);
    moving.push_str(&format!(
        "<g class=\"fla\" style=\"{none}\">{}</g>",
        text(
            "fl-t fl-2",
            bx + bw / 2.0,
            by + bh / 2.0 + 4.0,
            "middle",
            g.small + 1.0,
            "no VM",
            ""
        )
    ));
    let mut slots = String::new();
    for i in 0..f.vm.slots.min(2) {
        let y = g.slot_y0 + g.slot_step * i as f64;
        slots.push_str(&rect("fl-slot", (g.slot_x, y, g.slot_w, g.slot_h), 3.0, ""));
        slots.push_str(&text(
            "fl-t fl-2",
            g.slot_x + g.slot_w - 8.0,
            y + g.slot_h / 2.0 + 4.0,
            "end",
            g.small,
            &format!("slot {}", i + 1),
            "",
        ));
    }
    let up = a.window(ready, vm_end);
    moving.push_str(&format!("<g class=\"fla\" style=\"{up}\">{slots}</g>"));
    let mut slot_until = vec![0.0f64; f.vm.slots.min(2)];
    let mut vm_jobs: Vec<&FlowJob> = f.jobs.iter().filter(|j| j.lane().is_none()).collect();
    vm_jobs.sort_by(|x, y| x.start_s.total_cmp(&y.start_s));
    let mut slot_of: Vec<(String, f64)> = Vec::new();
    for job in vm_jobs {
        let i = (0..slot_until.len())
            .find(|i| slot_until[*i] <= job.start_s)
            .unwrap_or(0);
        slot_until[i] = job.end_s;
        let y = g.slot_y0 + g.slot_step * i as f64;
        slot_of.push((job.name.clone(), y));
        let start = tl.at(job.start_s);
        let end = tl.at(job.end_s);
        let progress = if (job.start_s..job.end_s).contains(&f.still_s) {
            (f.still_s - job.start_s) / (job.end_s - job.start_s)
        } else {
            1.0
        };
        let grow = a.frames(
            &[
                (0.0, "transform:scaleX(0)".into()),
                (start, "transform:scaleX(0)".into()),
                (end, "transform:scaleX(1)".into()),
                (a.total, "transform:scaleX(1)".into()),
            ],
            &format!("transform:scaleX({})", n(progress)),
        );
        let shown = a.window(start, end + 0.2);
        moving.push_str(&format!(
            "<g class=\"fla\" style=\"{shown}\"><g class=\"fla\" style=\"{grow};transform-box:fill-box;transform-origin:left\">{}</g>{}</g>",
            rect("fl-vm", (g.slot_x, y, g.slot_w, g.slot_h), 3.0, ""),
            text(
                "fl-t fl-b fl-on fl-halo",
                g.slot_x + 8.0,
                y + g.slot_h / 2.0 + 4.5,
                "start",
                g.small + 1.0,
                &job.name,
                "stroke:var(--vm)"
            )
        ));
    }
    let stop = a.window(vm_end - 0.4, a.total);
    moving.push_str(&format!(
        "<g class=\"fla\" style=\"{stop}\">{}</g>",
        text(
            "fl-t fl-b fl-stop",
            g.vm.0 + g.vm.2 - 12.0,
            g.vm.1 + g.vm.3 - 10.0,
            "end",
            g.small + 1.0,
            &format!("minute {:.0}: the idle VM ends itself", f.vm.end_minute),
            ""
        )
    ));

    // Job events: from the pipeline through the dispatcher to a lane run or the VM.
    let (tw, th) = g.token;
    let mut pulses: Vec<f64> = Vec::new();
    for (job, (cx, cy)) in f.jobs.iter().zip(&chip_at) {
        let (depart, arrive) = tl.travel(f, job);
        let span = arrive - depart;
        let (pool_x, dest) =
            match tile_of[f.jobs.iter().position(|j| j.name == job.name).unwrap_or(0)] {
                Some((x, y)) => (g.lanes_in, (x + g.tile_w / 2.0, y + g.tile_h / 2.0)),
                None => {
                    let y = slot_of
                        .iter()
                        .find(|(nm, _)| *nm == job.name)
                        .map_or(g.slot_y0, |(_, y)| *y);
                    (g.vm_in, (g.slot_x + g.slot_w / 2.0, y + g.slot_h / 2.0))
                }
            };
        let pool_top = if pool_x == g.lanes_in {
            lanes_top
        } else {
            vm_top
        };
        pulses.push(depart + span * 0.35);
        let points = [
            (depart, *cx, *cy, 0.0),
            (depart + 0.06, *cx, *cy, 1.0),
            (depart + span * 0.3, dcx, dy + dh / 2.0, 1.0),
            (depart + span * 0.42, dcx, dy + dh / 2.0, 1.0),
            (depart + span * 0.55, dcx, g.junction_y, 1.0),
            (depart + span * 0.72, pool_x, g.junction_y, 1.0),
            (depart + span * 0.84, pool_x, pool_top + 8.0, 1.0),
            (arrive - 0.04, dest.0, dest.1, 1.0),
            (arrive + 0.08, dest.0, dest.1, 0.0),
        ];
        let at = |x: f64, y: f64, o: f64| {
            format!(
                "transform:translate({}px,{}px);opacity:{}",
                n(x - tw / 2.0),
                n(y - th / 2.0),
                n(o)
            )
        };
        let mut frames = vec![(0.0, at(*cx, *cy, 0.0))];
        frames.extend(points.iter().map(|&(t, x, y, o)| (t, at(x, y, o))));
        frames.push((a.total, at(dest.0, dest.1, 0.0)));
        let style = a.frames(&frames, "opacity:0");
        moving.push_str(&format!(
            "<g class=\"fla\" style=\"{style}\">{}</g>",
            rect("fl-token", (0.0, 0.0, tw, th), th / 2.0, "")
        ));
    }
    pulses.sort_by(f64::total_cmp);
    pulses.dedup_by(|x, y| (*x - *y).abs() < 0.3);
    let mut frames = vec![(0.0, "stroke-width:1.5px".to_string())];
    for t in pulses {
        frames.push((t - 0.1, "stroke-width:1.5px".into()));
        frames.push((t, "stroke-width:4px".into()));
        frames.push((t + 0.3, "stroke-width:1.5px".into()));
    }
    frames.push((a.total, "stroke-width:1.5px".into()));
    let pulse = a.frames(&frames, "stroke-width:1.5px");
    top = top.replacen(
        "class=\"fl-box\"",
        &format!("class=\"fla fl-box\" style=\"{pulse}\""),
        1,
    );

    // Captions, each with the pipeline's clock.
    for (i, c) in f.captions.iter().enumerate() {
        let on = if i == 0 { 0.0 } else { tl.at(c.at_s) + FADE };
        let off = f
            .captions
            .get(i + 1)
            .map_or(a.total - 0.3, |next| tl.at(next.at_s));
        let shown = a.window(on, off);
        let mut lines = String::new();
        for (k, line) in wrap(&c.text, g.caption_chars).iter().enumerate() {
            lines.push_str(&text(
                "fl-t",
                g.caption_x,
                g.caption_y + k as f64 * (g.big + 4.0),
                "start",
                g.big,
                line,
                "",
            ));
        }
        moving.push_str(&format!(
            "<g class=\"fla\" style=\"{shown}\">{}{lines}</g>",
            text(
                "fl-t fl-num",
                g.panel.0,
                g.caption_y,
                "start",
                g.big,
                &clock(c.at_s),
                ""
            )
        ));
    }

    let scene = a.frames(
        &[
            (0.0, "opacity:0".into()),
            (0.25, "opacity:1".into()),
            (a.total - 0.35, "opacity:1".into()),
            (a.total, "opacity:0".into()),
        ],
        "opacity:1",
    );
    let defs = format!(
        "<defs><pattern id=\"{id}-hatch\" width=\"8\" height=\"8\" patternUnits=\"userSpaceOnUse\">\
         <rect class=\"fl-hatch-bg\" width=\"8\" height=\"8\"/>\
         <path class=\"fl-hatch-line\" d=\"M-2 2L2 -2M0 8L8 0M6 10L10 6\"/></pattern>\
         <pattern id=\"{id}-vhatch\" width=\"8\" height=\"8\" patternUnits=\"userSpaceOnUse\">\
         <rect class=\"fl-hatch-bg\" width=\"8\" height=\"8\"/>\
         <path class=\"fl-vhatch-line\" d=\"M-2 2L2 -2M0 8L8 0M6 10L10 6\"/></pattern></defs>"
    );
    let motion = format!(
        ".{id}{{--loop:{}s}}.{id} .fla{{animation:var(--a) var(--loop) linear infinite}}\
         @media (prefers-reduced-motion:reduce){{.{id} .fla{{animation:none}}}}",
        n(tl.total)
    );
    let (view, size, tile, shift) = match standalone {
        Some(_) => (
            format!("0 0 {} {}", n(g.w + 32.0), n(g.h + 32.0)),
            format!(" width=\"{}\" height=\"{}\"", n(g.w + 32.0), n(g.h + 32.0)),
            format!(
                "<rect width=\"{}\" height=\"{}\" rx=\"12\" fill=\"var(--paper)\"/>",
                n(g.w + 32.0),
                n(g.h + 32.0)
            ),
            " transform=\"translate(16 16)\"",
        ),
        None => (
            format!("0 0 {} {}", n(g.w), n(g.h)),
            String::new(),
            String::new(),
            "",
        ),
    };
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" class=\"{id}\" viewBox=\"{view}\"{size} role=\"img\" \
         aria-label=\"{}\"><style>{}{STYLE}{}{motion}</style>{defs}{tile}<g{shift}>{fixed}\
         <g class=\"fla\" style=\"{scene}\">{moving}</g>{top}</g></svg>",
        escape(&f.caption),
        standalone.unwrap_or(""),
        a.css
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow() -> Flow {
        parse(include_str!("../assets/flow.json")).unwrap()
    }

    #[test]
    fn the_clock_runs_forward_and_pauses_for_each_burst_of_events() {
        let f = flow();
        let tl = Timeline::new(&f);
        assert!(tl.points.windows(2).all(|w| w[1].1 > w[0].1));
        for job in &f.jobs {
            let (depart, arrive) = tl.travel(&f, job);
            assert!(depart < arrive, "{}", job.name);
            assert!(
                arrive <= tl.at(job.start_s) + 1e-9,
                "{} starts before its event arrives",
                job.name
            );
            assert!(tl.at(job.start_s) < tl.at(job.end_s) + 1e-9, "{}", job.name);
        }
        assert!(
            (36.0..56.0).contains(&tl.total),
            "a pass lasts {} s",
            tl.total
        );
    }

    #[test]
    fn the_flow_refuses_a_job_on_the_vm_before_it_is_ready() {
        let json =
            include_str!("../assets/flow.json").replace("\"startS\": 172.3", "\"startS\": 150.0");
        assert!(
            parse(&json)
                .unwrap_err()
                .contains("site-image-dev: starts before the VM is ready")
        );
    }

    #[test]
    fn clock_and_wrap() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(3402.1), "56:42");
        assert_eq!(wrap("a bb ccc dd", 6), ["a bb", "ccc dd"]);
    }
}
