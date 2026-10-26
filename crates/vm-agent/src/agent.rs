//! Slots, state reports and the drain protocol of spec section 6.1; the spill runner of M6.2
//! rules R6 and R7.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::time::Duration;

use ciburst_core::http::Request;
use ciburst_core::log::Log;
use ciburst_core::types::{VmReport, VmReportAnswer, VmState};
use ciburst_core::vm;

use crate::config::Config;
use crate::system::{SLOT_LABEL, SlotCommand, System};

pub const POLL: Duration = Duration::from_secs(5);
pub const RUNNER: &str = "gitlab-runner";
pub const SLOT_ROOT: &str = "/var/lib/ciburst";
const HEARTBEAT: Duration = Duration::from_secs(vm::HEARTBEAT_S);
/// Shortest report interval an answer may ask for (M6.2 rule R7); the longest is [`HEARTBEAT`].
const REPORT_MIN: Duration = Duration::from_secs(10);
const DRAIN_GRACE: Duration = Duration::from_secs(vm::DRAIN_GRACE_S);
const TERMINATE_FOR: Duration = Duration::from_secs(600);
const BACKOFF_FIRST: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// The GitLab runner a slot's process polls with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    /// The pool's own runner, `RUNNER_TOKEN`.
    Pool,
    /// The pool's spill runner, `RUNNER_SPILL_TOKEN`, which also takes lane jobs.
    Spill,
}

impl Runner {
    fn name(self) -> &'static str {
        match self {
            Runner::Pool => "pool",
            Runner::Spill => "spill",
        }
    }
}

/// `run-single` reads its config file only with `--runner`, so the path is never created.
pub fn slot_command(config: &Config, slot: u32, runner: Runner) -> SlotCommand {
    let token = match (runner, &config.spill_token) {
        (Runner::Spill, Some(token)) => token,
        _ => &config.runner_token,
    };
    let mut args: Vec<String> = [
        "run-single",
        "--url",
        &config.server_url,
        "--token",
        token,
        "--executor",
        "docker",
        "--docker-image",
        &config.default_image,
        "--docker-privileged",
        "--docker-volumes",
        "/cache",
        // Docker-in-Docker with TLS needs the client certificates shared by service and job.
        "--docker-volumes",
        "/certs/client",
        // Slots share token, system id and concurrent id 0, so the runner would give two
        // jobs of one project the same volumes (`/builds` too); with a cache dir per slot
        // they become host directories of that slot.
        "--docker-cache-dir",
        &format!("{SLOT_ROOT}/slot-{slot}/volumes"),
        // Tells which slot runs a job, so a token switch leaves busy slots alone.
        "--docker-container-labels",
        &format!("{SLOT_LABEL}:{slot}"),
        "--max-builds",
        "0",
        "--wait-timeout",
        "0",
        "--config",
        &format!("{SLOT_ROOT}/slot-{slot}/config.toml"),
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if let Some(cache) = &config.cache {
        for (flag, value) in [
            ("--cache-type", "s3"),
            ("--cache-s3-server-address", &cache.server_address),
            ("--cache-s3-bucket-name", &cache.bucket_name),
            ("--cache-s3-bucket-location", &cache.bucket_location),
            ("--cache-s3-access-key", &cache.access_key),
            ("--cache-s3-secret-key", &cache.secret_key),
        ] {
            args.push(flag.to_string());
            args.push(value.to_string());
        }
        if cache.shared {
            args.push("--cache-shared".to_string());
        }
        // D44: cache archives in parallel with the fastest compression; the bucket is in the
        // VM's region, so bytes are cheap and archiving was the slow part.
        for variable in ["FF_USE_FASTZIP=true", "CACHE_COMPRESSION_LEVEL=fastest"] {
            args.push("--env".to_string());
            args.push(variable.to_string());
        }
    }
    SlotCommand {
        slot,
        program: RUNNER.to_string(),
        args,
    }
}

/// Running jobs from `docker ps` lines `<job id> <slot>`, one per container (a job with
/// services has several).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Jobs {
    /// Distinct job ids: the busy slots.
    pub busy: u32,
    /// Slots that run a job.
    pub slots: BTreeSet<u32>,
    /// Jobs none of whose containers carries the slot label.
    pub unlabeled: BTreeSet<String>,
}

pub fn jobs(docker_ps: &str) -> Jobs {
    let mut slot_of: BTreeMap<&str, Option<u32>> = BTreeMap::new();
    for line in docker_ps.lines() {
        let mut fields = line.split_whitespace();
        let Some(job) = fields.next() else { continue };
        let slot = fields.next().and_then(|slot| slot.parse::<u32>().ok());
        let known = slot_of.entry(job).or_default();
        *known = known.or(slot);
    }
    Jobs {
        busy: u32::try_from(slot_of.len()).unwrap_or(u32::MAX),
        slots: slot_of.values().flatten().copied().collect(),
        unlabeled: slot_of
            .iter()
            .filter(|(_, slot)| slot.is_none())
            .map(|(job, _)| (*job).to_string())
            .collect(),
    }
}

/// What a dispatcher before M6.2 answers, and what holds before the first answer.
fn no_answer() -> VmReportAnswer {
    VmReportAnswer {
        spill: false,
        next_report_s: vm::HEARTBEAT_S,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No report has succeeded yet.
    Starting,
    Idle,
    Busy,
    /// Reported `draining` at `since`; slots keep polling for the grace.
    Draining {
        since: Duration,
    },
    /// Slots got `SIGQUIT`; still reported as `draining`.
    Quitting,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Idle => "idle",
            Phase::Busy => "busy",
            Phase::Draining { .. } => "draining",
            Phase::Quitting => "quitting",
        }
    }

    fn draining(self) -> bool {
        matches!(self, Phase::Draining { .. } | Phase::Quitting)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    /// Runner of the slot's process, `None` while no process runs.
    process: Option<Runner>,
    /// The process got `SIGQUIT`: it requests no further job and exits after the current one.
    quit: bool,
}

pub struct Agent {
    config: Config,
    sys: System,
    log: Log,
    phase: Phase,
    /// Index `slot - 1`.
    slots: Vec<Slot>,
    /// Busy slots from the last successful `docker ps`, `None` before the first.
    busy: Option<u32>,
    /// Last report the dispatcher accepted, and when.
    reported: Option<(VmReport, Duration)>,
    /// The dispatcher's answer to that report.
    answer: VmReportAnswer,
    /// Jobs already warned about for a missing slot label.
    unlabeled: BTreeSet<String>,
}

impl Agent {
    pub fn new(config: Config, sys: System, log: Log) -> Agent {
        let slots = config.slots as usize;
        Agent {
            config,
            sys,
            log,
            phase: Phase::Starting,
            slots: vec![Slot::default(); slots],
            busy: None,
            reported: None,
            answer: no_answer(),
            unlabeled: BTreeSet::new(),
        }
    }

    /// Runs until the VM was handed to the dispatcher for termination (0) or the dispatcher
    /// could not be reached for that (1).
    pub async fn run(&mut self) -> i32 {
        self.log.info(
            "agent started",
            &[
                ("run", &self.config.run_id),
                ("pool", &self.config.pool),
                ("slots", &self.config.slots.to_string()),
                ("timeout_s", &self.config.timeout_s.to_string()),
            ],
        );
        loop {
            if let Some(code) = self.tick().await {
                return code;
            }
            self.sys.clock.sleep(POLL).await;
        }
    }

    pub async fn tick(&mut self) -> Option<i32> {
        self.reap();
        if !self.phase.draining() {
            self.start_stopped_slots();
        }
        if self.phase == Phase::Quitting && !self.slots.iter().any(|s| s.process.is_some()) {
            return Some(self.terminate().await);
        }
        let jobs = match self.sys.docker.job_labels().await {
            Ok(output) => Some(jobs(&output)),
            Err(error) => {
                self.log
                    .warn("docker ps failed", &[("error", &error.to_string())]);
                None
            }
        };
        if let Some(jobs) = &jobs {
            self.busy = Some(jobs.busy);
            self.warn_unlabeled(jobs);
        }
        self.step(jobs.as_ref().map(|jobs| jobs.busy)).await;
        self.report_if_due().await;
        if let Some(jobs) = &jobs {
            self.switch_idle_slots(jobs);
        }
        None
    }

    async fn step(&mut self, busy: Option<u32>) {
        let now = self.sys.clock.now();
        match self.phase {
            Phase::Starting | Phase::Quitting => {}
            Phase::Idle | Phase::Busy => {
                let Some(busy) = busy else { return };
                if busy == 0 && self.in_drain_window() {
                    let report = VmReport {
                        state: VmState::Draining,
                        free: 0,
                        slots: self.config.slots,
                        spill: self.spill(),
                    };
                    // The grace starts only once the dispatcher stopped counting this VM.
                    if self.send_report(&report).await {
                        self.transition(Phase::Draining { since: now });
                        return;
                    }
                }
                let next = if busy > 0 { Phase::Busy } else { Phase::Idle };
                if next != self.phase {
                    self.transition(next);
                }
            }
            Phase::Draining { since } => {
                if busy.is_some_and(|busy| busy > 0) {
                    self.transition(Phase::Busy);
                } else if now.saturating_sub(since) >= DRAIN_GRACE {
                    self.quit_slots();
                    self.transition(Phase::Quitting);
                }
            }
        }
    }

    /// Free slots as reported in `phase`: running slots without a job, none while draining.
    fn free_in(&self, phase: Phase) -> Option<u32> {
        let busy = self.busy?;
        let running = self.slots.iter().filter(|s| s.process.is_some()).count();
        Some(if phase.draining() {
            0
        } else {
            u32::try_from(running)
                .unwrap_or(u32::MAX)
                .saturating_sub(busy)
        })
    }

    /// Slots whose process polls with the spill runner.
    fn spill(&self) -> u32 {
        let spill = self
            .slots
            .iter()
            .filter(|s| s.process == Some(Runner::Spill))
            .count();
        u32::try_from(spill).unwrap_or(u32::MAX)
    }

    /// The runner every idle slot should poll with, after the last answer (M6.2 rule R6).
    fn wanted(&self) -> Runner {
        if self.answer.spill && self.config.spill_token.is_some() {
            Runner::Spill
        } else {
            Runner::Pool
        }
    }

    fn current_report(&self) -> Option<VmReport> {
        let state = match self.phase {
            Phase::Starting if self.busy? > 0 => VmState::Busy,
            Phase::Starting | Phase::Idle => VmState::Idle,
            Phase::Busy => VmState::Busy,
            Phase::Draining { .. } | Phase::Quitting => VmState::Draining,
        };
        Some(VmReport {
            state,
            free: self.free_in(self.phase)?,
            slots: self.config.slots,
            spill: self.spill(),
        })
    }

    /// Seconds until the next unchanged report: the last answer's `nextReportS`, clamped.
    fn report_interval(&self) -> Duration {
        Duration::from_secs(self.answer.next_report_s).clamp(REPORT_MIN, HEARTBEAT)
    }

    async fn report_if_due(&mut self) {
        let Some(report) = self.current_report() else {
            return;
        };
        let now = self.sys.clock.now();
        let due = match &self.reported {
            None => true,
            Some((last, at)) => {
                *last != report || now.saturating_sub(*at) >= self.report_interval()
            }
        };
        if due && self.send_report(&report).await && self.phase == Phase::Starting {
            let next = if report.state == VmState::Busy {
                Phase::Busy
            } else {
                Phase::Idle
            };
            self.transition(next);
        }
    }

    async fn send_report(&mut self, report: &VmReport) -> bool {
        let body = serde_json::to_value(report).expect("a report serializes");
        let request = self.request(vm::PATH_REPORT).json(&body);
        match self.sys.http.send(request).await {
            Ok(response) if response.ok() => {
                let answer = response.json::<VmReportAnswer>().unwrap_or_else(|error| {
                    self.log
                        .warn("report answer unreadable", &[("error", &error.to_string())]);
                    no_answer()
                });
                self.take_answer(answer);
                self.reported = Some((report.clone(), self.sys.clock.now()));
                true
            }
            Ok(response) => {
                self.log.warn(
                    "report failed",
                    &[
                        ("state", report.state.as_str()),
                        ("status", &response.status.to_string()),
                    ],
                );
                false
            }
            Err(error) => {
                self.log.warn(
                    "report failed",
                    &[
                        ("state", report.state.as_str()),
                        ("error", &error.to_string()),
                    ],
                );
                false
            }
        }
    }

    fn take_answer(&mut self, answer: VmReportAnswer) {
        if answer.spill != self.answer.spill {
            self.log
                .info("spill", &[("allowed", &answer.spill.to_string())]);
        }
        self.answer = answer;
    }

    fn request(&self, path: &str) -> Request {
        Request::new("POST", format!("{}{path}", self.config.dispatch_url))
            .header(vm::HEADER_RUN, self.config.run_id.as_str())
            .header(vm::HEADER_TOKEN, self.config.token.as_str())
    }

    fn in_drain_window(&self) -> bool {
        match self.sys.uptime.seconds() {
            Ok(uptime) => uptime % 3600 >= vm::DRAIN_MINUTE * 60,
            Err(error) => {
                self.log
                    .warn("uptime unreadable", &[("error", &error.to_string())]);
                false
            }
        }
    }

    fn transition(&mut self, next: Phase) {
        let free = self
            .free_in(next)
            .map(|free| free.to_string())
            .unwrap_or_default();
        self.log.info(
            "state",
            &[
                ("from", self.phase.name()),
                ("to", next.name()),
                ("free", &free),
            ],
        );
        self.phase = next;
    }

    fn slot_numbers(&self) -> RangeInclusive<u32> {
        1..=self.config.slots
    }

    fn reap(&mut self) {
        for slot in self.slot_numbers() {
            let index = (slot - 1) as usize;
            if self.slots[index].process.is_none() {
                continue;
            }
            match self.sys.slots.try_exit(slot) {
                Ok(Some(code)) => {
                    let quit = self.slots[index].quit;
                    self.slots[index] = Slot::default();
                    let (slot, code) = (slot.to_string(), code.to_string());
                    let fields = [("slot", slot.as_str()), ("code", code.as_str())];
                    if quit || self.phase == Phase::Quitting {
                        self.log.info("slot exited", &fields);
                    } else {
                        self.log.warn("slot exited", &fields);
                    }
                }
                Ok(None) => {}
                Err(error) => self.log.warn(
                    "slot status unknown",
                    &[("slot", &slot.to_string()), ("error", &error.to_string())],
                ),
            }
        }
    }

    fn start_stopped_slots(&mut self) {
        let runner = self.wanted();
        for slot in self.slot_numbers() {
            let index = (slot - 1) as usize;
            if self.slots[index].process.is_some() {
                continue;
            }
            match self
                .sys
                .slots
                .spawn(&slot_command(&self.config, slot, runner))
            {
                Ok(()) => {
                    self.slots[index].process = Some(runner);
                    self.log.info(
                        "slot started",
                        &[("slot", &slot.to_string()), ("runner", runner.name())],
                    );
                }
                Err(error) => self.log.warn(
                    "slot start failed",
                    &[("slot", &slot.to_string()), ("error", &error.to_string())],
                ),
            }
        }
    }

    /// M6.2 rule R6: an idle slot whose process polls with the other runner gets `SIGQUIT`; the
    /// start after its exit picks the wanted runner. Slots with a job, and every slot while a
    /// job's slot is unknown, keep their process.
    fn switch_idle_slots(&mut self, jobs: &Jobs) {
        if !matches!(self.phase, Phase::Idle | Phase::Busy) || !jobs.unlabeled.is_empty() {
            return;
        }
        let wanted = self.wanted();
        for slot in self.slot_numbers() {
            let index = (slot - 1) as usize;
            let Slot { process, quit } = self.slots[index];
            let Some(runner) = process else { continue };
            if runner == wanted || quit || jobs.slots.contains(&slot) {
                continue;
            }
            // A job the slot took after `docker ps` runs to its end: `SIGQUIT` stops requests only.
            match self.sys.slots.quit(slot) {
                Ok(()) => {
                    self.slots[index].quit = true;
                    self.log.info(
                        "slot switching",
                        &[
                            ("slot", &slot.to_string()),
                            ("from", runner.name()),
                            ("to", wanted.name()),
                        ],
                    );
                }
                Err(error) => self.log.warn(
                    "slot quit failed",
                    &[("slot", &slot.to_string()), ("error", &error.to_string())],
                ),
            }
        }
    }

    fn warn_unlabeled(&mut self, jobs: &Jobs) {
        self.unlabeled.retain(|job| jobs.unlabeled.contains(job));
        for job in &jobs.unlabeled {
            if self.unlabeled.insert(job.clone()) {
                self.log.warn("job without slot label", &[("job", job)]);
            }
        }
    }

    /// The drain's `SIGQUIT`; a slot that is switching already got one.
    fn quit_slots(&mut self) {
        for slot in self.slot_numbers() {
            let index = (slot - 1) as usize;
            if self.slots[index].process.is_none() || self.slots[index].quit {
                continue;
            }
            match self.sys.slots.quit(slot) {
                Ok(()) => self.slots[index].quit = true,
                Err(error) => self.log.warn(
                    "slot quit failed",
                    &[("slot", &slot.to_string()), ("error", &error.to_string())],
                ),
            }
        }
    }

    /// `POST /terminate` with exponential backoff for ten minutes.
    async fn terminate(&mut self) -> i32 {
        let start = self.sys.clock.now();
        let mut delay = BACKOFF_FIRST;
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            let mut request = self.request(vm::PATH_TERMINATE);
            request.body = Some(Vec::new());
            let failure = match self.sys.http.send(request).await {
                Ok(response) if response.ok() => {
                    self.log
                        .info("terminate accepted", &[("attempts", &attempts.to_string())]);
                    return 0;
                }
                Ok(response) => ("status", response.status.to_string()),
                Err(error) => ("error", error.to_string()),
            };
            self.log.warn(
                "terminate failed",
                &[("attempt", &attempts.to_string()), (failure.0, &failure.1)],
            );
            if self.sys.clock.now().saturating_sub(start) + delay > TERMINATE_FOR {
                self.log
                    .error("terminate gave up", &[("attempts", &attempts.to_string())]);
                return 1;
            }
            self.sys.clock.sleep(delay).await;
            delay = (delay * 2).min(BACKOFF_MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use ciburst_core::http::{HttpClient, Response};
    use serde_json::{Value, json};

    use super::*;
    use crate::config::tests::{SAMPLE, sample};
    use crate::config::{Config, parse};
    use crate::system::{Clock, Docker, Slots, Uptime};

    const POOL_TOKEN: &str = "glrt-runner-secret";
    const SPILL_TOKEN: &str = "glrt-spill-secret";

    fn secs(duration: Duration) -> u64 {
        duration.as_secs()
    }

    #[derive(Clone)]
    struct FakeClock {
        now: Arc<Mutex<Duration>>,
        boot_uptime: u64,
    }

    impl FakeClock {
        fn elapsed(&self) -> u64 {
            secs(*self.now.lock().unwrap())
        }
    }

    #[async_trait]
    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            *self.now.lock().unwrap()
        }

        async fn sleep(&self, duration: Duration) {
            *self.now.lock().unwrap() += duration;
        }
    }

    impl Uptime for FakeClock {
        fn seconds(&self) -> io::Result<u64> {
            Ok(self.boot_uptime + self.elapsed())
        }
    }

    /// A job's containers: `id` runs on `slot` from `from` to before `until`, with `containers`
    /// containers; `slot` is `None` when the containers lack the slot label.
    struct Job {
        id: &'static str,
        slot: Option<u32>,
        from: u64,
        until: u64,
        containers: usize,
    }

    #[derive(Clone)]
    struct FakeDocker {
        clock: FakeClock,
        jobs: Arc<Mutex<Vec<Job>>>,
        failing: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl Docker for FakeDocker {
        async fn job_labels(&self) -> io::Result<String> {
            if *self.failing.lock().unwrap() {
                return Err(io::Error::other("daemon down"));
            }
            let now = self.clock.elapsed();
            let mut out = String::new();
            for job in self.jobs.lock().unwrap().iter() {
                if job.from <= now && now < job.until {
                    // `docker ps` prints an empty label as nothing after the separator.
                    let slot = job.slot.map(|slot| slot.to_string()).unwrap_or_default();
                    for _ in 0..job.containers {
                        out.push_str(&format!("{} {slot}\n", job.id));
                    }
                }
            }
            Ok(out)
        }
    }

    #[derive(Default)]
    struct SlotState {
        spawns: Vec<(u64, u32, Vec<String>)>,
        quits: Vec<(u64, u32)>,
        exits: Vec<(u64, u32)>,
        /// Running slots and when they exit, if scheduled.
        running: BTreeMap<u32, Option<u64>>,
        /// Seconds a slot takes to exit after `SIGQUIT`.
        quit_delay: BTreeMap<u32, u64>,
        /// Unexpected exits: slot and time.
        crashes: Vec<(u32, u64)>,
        /// Slots whose process cannot start.
        broken: Vec<u32>,
    }

    struct FakeSlots {
        clock: FakeClock,
        state: Arc<Mutex<SlotState>>,
    }

    impl Slots for FakeSlots {
        fn spawn(&mut self, command: &SlotCommand) -> io::Result<()> {
            let mut state = self.state.lock().unwrap();
            if state.broken.contains(&command.slot) {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no gitlab-runner"));
            }
            assert!(
                !state.running.contains_key(&command.slot),
                "slot {} spawned twice",
                command.slot
            );
            state
                .spawns
                .push((self.clock.elapsed(), command.slot, command.args.clone()));
            state.running.insert(command.slot, None);
            Ok(())
        }

        fn try_exit(&mut self, slot: u32) -> io::Result<Option<i32>> {
            let now = self.clock.elapsed();
            let mut state = self.state.lock().unwrap();
            if let Some(position) = state
                .crashes
                .iter()
                .position(|(s, at)| *s == slot && *at <= now)
            {
                state.crashes.remove(position);
                state.running.remove(&slot);
                state.exits.push((now, slot));
                return Ok(Some(1));
            }
            match state.running.get(&slot) {
                Some(Some(at)) if *at <= now => {
                    state.running.remove(&slot);
                    state.exits.push((now, slot));
                    Ok(Some(0))
                }
                Some(_) => Ok(None),
                None => Err(io::Error::new(io::ErrorKind::NotFound, "no such slot")),
            }
        }

        fn quit(&mut self, slot: u32) -> io::Result<()> {
            let now = self.clock.elapsed();
            let mut state = self.state.lock().unwrap();
            state.quits.push((now, slot));
            let delay = state.quit_delay.get(&slot).copied().unwrap_or(0);
            state.running.insert(slot, Some(now + delay));
            Ok(())
        }
    }

    type Responder = Box<dyn Fn(&str, u64) -> u16 + Send + Sync>;
    type Answerer = Box<dyn Fn(u64) -> Value + Send + Sync>;

    #[derive(Clone)]
    struct Sent {
        at: u64,
        path: String,
        body: Option<Value>,
        run: String,
        token: String,
    }

    struct FakeHttp {
        clock: FakeClock,
        sent: Arc<Mutex<Vec<Sent>>>,
        respond: Arc<Mutex<Responder>>,
        answer: Arc<Mutex<Answerer>>,
    }

    #[async_trait]
    impl HttpClient for FakeHttp {
        async fn send(&self, request: Request) -> ciburst_core::Result<Response> {
            let now = self.clock.elapsed();
            let path = request
                .url
                .strip_prefix("https://dispatch.example.invalid")
                .expect("dispatch url")
                .to_string();
            assert_eq!(request.method, "POST");
            let body = request
                .body
                .as_ref()
                .filter(|body| !body.is_empty())
                .map(|body| serde_json::from_slice(body).expect("json body"));
            self.sent.lock().unwrap().push(Sent {
                at: now,
                path: path.clone(),
                body,
                run: request.header_value(vm::HEADER_RUN).unwrap_or("").into(),
                token: request.header_value(vm::HEADER_TOKEN).unwrap_or("").into(),
            });
            let status = (self.respond.lock().unwrap())(&path, now);
            if status == 0 {
                return Err(ciburst_core::Error::Http("connection refused".into()));
            }
            if path == vm::PATH_REPORT {
                let answer = (self.answer.lock().unwrap())(now);
                return Ok(Response::json_body(status, &answer));
            }
            Ok(Response::empty(status))
        }
    }

    /// The answer of a dispatcher before M6.2.
    fn old_answer() -> Value {
        json!({"result": "reported", "ms": 3})
    }

    fn spill_answer(spill: bool, next_report_s: u64) -> Value {
        json!({"result": "reported", "spill": spill, "nextReportS": next_report_s, "ms": 3})
    }

    struct Harness {
        clock: FakeClock,
        docker: FakeDocker,
        slots: Arc<Mutex<SlotState>>,
        sent: Arc<Mutex<Vec<Sent>>>,
        respond: Arc<Mutex<Responder>>,
        answer: Arc<Mutex<Answerer>>,
        lines: Arc<Mutex<Vec<String>>>,
        agent: Agent,
        config: Config,
    }

    /// An agent with `slots` slots whose VM booted `boot_uptime` seconds before the test starts.
    fn harness(slots: u32, boot_uptime: u64) -> Harness {
        let text = SAMPLE.replace("CIBURST_SLOTS=2", &format!("CIBURST_SLOTS={slots}"));
        harness_from(&text, boot_uptime)
    }

    fn harness_from(text: &str, boot_uptime: u64) -> Harness {
        let config = parse(text).unwrap();
        let clock = FakeClock {
            now: Arc::new(Mutex::new(Duration::ZERO)),
            boot_uptime,
        };
        let docker = FakeDocker {
            clock: clock.clone(),
            jobs: Arc::new(Mutex::new(Vec::new())),
            failing: Arc::new(Mutex::new(false)),
        };
        let slot_state = Arc::new(Mutex::new(SlotState::default()));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let respond: Arc<Mutex<Responder>> = Arc::new(Mutex::new(Box::new(|_, _| 200)));
        let answer: Arc<Mutex<Answerer>> = Arc::new(Mutex::new(Box::new(|_| old_answer())));
        let (log, lines) = Log::memory();
        let sys = System {
            clock: Arc::new(clock.clone()),
            uptime: Arc::new(clock.clone()),
            slots: Box::new(FakeSlots {
                clock: clock.clone(),
                state: Arc::clone(&slot_state),
            }),
            docker: Arc::new(docker.clone()),
            http: Arc::new(FakeHttp {
                clock: clock.clone(),
                sent: Arc::clone(&sent),
                respond: Arc::clone(&respond),
                answer: Arc::clone(&answer),
            }),
        };
        Harness {
            agent: Agent::new(config.clone(), sys, log),
            clock,
            docker,
            slots: slot_state,
            sent,
            respond,
            answer,
            lines,
            config,
        }
    }

    impl Harness {
        fn job(&self, id: &'static str, slot: u32, from: u64, until: u64, containers: usize) {
            self.add_job(id, Some(slot), from, until, containers);
        }

        fn unlabeled_job(&self, id: &'static str, from: u64, until: u64, containers: usize) {
            self.add_job(id, None, from, until, containers);
        }

        fn add_job(
            &self,
            id: &'static str,
            slot: Option<u32>,
            from: u64,
            until: u64,
            containers: usize,
        ) {
            self.docker.jobs.lock().unwrap().push(Job {
                id,
                slot,
                from,
                until,
                containers,
            });
        }

        fn respond(&self, responder: impl Fn(&str, u64) -> u16 + Send + Sync + 'static) {
            *self.respond.lock().unwrap() = Box::new(responder);
        }

        /// The body of every `/vm` answer, by the time of the report.
        fn answer(&self, answerer: impl Fn(u64) -> Value + Send + Sync + 'static) {
            *self.answer.lock().unwrap() = Box::new(answerer);
        }

        /// Ticks every [`POLL`] up to and including second `until`, or until the agent ends.
        async fn run_until(&mut self, until: u64) -> Option<i32> {
            while self.clock.elapsed() <= until {
                if let Some(code) = self.agent.tick().await {
                    return Some(code);
                }
                self.clock.sleep(POLL).await;
            }
            None
        }

        async fn run_to_end(&mut self) -> i32 {
            self.run_until(100_000).await.expect("agent ends")
        }

        /// Accepted and failed `/vm` reports as (time, state, free).
        fn reports(&self) -> Vec<(u64, String, u64)> {
            self.reports_with_spill()
                .into_iter()
                .map(|(at, state, free, _)| (at, state, free))
                .collect()
        }

        /// Accepted and failed `/vm` reports as (time, state, free, spill).
        fn reports_with_spill(&self) -> Vec<(u64, String, u64, u64)> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|sent| sent.path == vm::PATH_REPORT)
                .map(|sent| {
                    let body = sent.body.as_ref().expect("report body");
                    (
                        sent.at,
                        body["state"].as_str().unwrap().to_string(),
                        body["free"].as_u64().unwrap(),
                        body["spill"].as_u64().unwrap(),
                    )
                })
                .collect()
        }

        fn terminates(&self) -> Vec<u64> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|sent| sent.path == vm::PATH_TERMINATE)
                .map(|sent| sent.at)
                .collect()
        }

        fn quits(&self) -> Vec<(u64, u32)> {
            self.slots.lock().unwrap().quits.clone()
        }

        /// Slot starts as (time, slot, runner of the token it got).
        fn spawns(&self) -> Vec<(u64, u32, &'static str)> {
            self.slots
                .lock()
                .unwrap()
                .spawns
                .iter()
                .map(|(at, slot, args)| {
                    let token = args.iter().skip_while(|arg| *arg != "--token").nth(1);
                    let runner = match token.map(String::as_str) {
                        Some(POOL_TOKEN) => "pool",
                        Some(SPILL_TOKEN) => "spill",
                        _ => "unknown",
                    };
                    (*at, *slot, runner)
                })
                .collect()
        }

        fn lines(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }

        fn assert_lines(&self, expected: &[&str]) {
            let lines = self.lines();
            for line in expected {
                assert!(
                    lines.contains(&line.to_string()),
                    "{line} not in {lines:#?}"
                );
            }
        }
    }

    fn report(at: u64, state: &str, free: u64) -> (u64, String, u64) {
        (at, state.to_string(), free)
    }

    fn spilled(at: u64, state: &str, free: u64, spill: u64) -> (u64, String, u64, u64) {
        (at, state.to_string(), free, spill)
    }

    #[test]
    fn slot_flags_follow_the_contract() {
        let config = sample();
        let command = slot_command(&config, 2, Runner::Pool);
        assert_eq!(command.slot, 2);
        assert_eq!(command.program, "gitlab-runner");
        assert_eq!(
            command.args,
            [
                "run-single",
                "--url",
                "https://gitlab.example.invalid",
                "--token",
                "glrt-runner-secret",
                "--executor",
                "docker",
                "--docker-image",
                "registry.example.invalid/ciburst/node:1",
                "--docker-privileged",
                "--docker-volumes",
                "/cache",
                "--docker-volumes",
                "/certs/client",
                "--docker-cache-dir",
                "/var/lib/ciburst/slot-2/volumes",
                "--docker-container-labels",
                "ciburst.slot:2",
                "--max-builds",
                "0",
                "--wait-timeout",
                "0",
                "--config",
                "/var/lib/ciburst/slot-2/config.toml",
                "--cache-type",
                "s3",
                "--cache-s3-server-address",
                "s3.fr-par.scw.cloud",
                "--cache-s3-bucket-name",
                "ci-cache-bucket",
                "--cache-s3-bucket-location",
                "fr-par",
                "--cache-s3-access-key",
                "SCWACCESSKEYSAMPLE",
                "--cache-s3-secret-key",
                "cache-secret-key",
                "--cache-shared",
                "--env",
                "FF_USE_FASTZIP=true",
                "--env",
                "CACHE_COMPRESSION_LEVEL=fastest",
            ]
        );

        // The spill runner changes the token only: same slot dir, cache dir and label.
        let spill = slot_command(&config, 2, Runner::Spill);
        assert_eq!(spill.args.len(), command.args.len());
        let differing: Vec<(&str, &str)> = command
            .args
            .iter()
            .zip(&spill.args)
            .filter(|(pool, spill)| pool != spill)
            .map(|(pool, spill)| (pool.as_str(), spill.as_str()))
            .collect();
        assert_eq!(differing, [(POOL_TOKEN, SPILL_TOKEN)]);

        let mut config = sample();
        config.spill_token = None;
        let command = slot_command(&config, 2, Runner::Spill);
        assert_eq!(command.args[4], "glrt-runner-secret");

        config.cache = None;
        let command = slot_command(&config, 1, Runner::Pool);
        assert_eq!(command.args.len(), 24);
        assert_eq!(command.args[23], "/var/lib/ciburst/slot-1/config.toml");
        assert!(!command.args.iter().any(|arg| arg.starts_with("--cache")));
    }

    #[test]
    fn jobs_count_distinct_ids_and_map_them_to_slots() {
        assert_eq!(jobs(""), Jobs::default());
        assert_eq!(jobs("\n \n"), Jobs::default());
        assert_eq!(jobs("812\n").busy, 1);
        assert_eq!(jobs("812\n812\n813\n 813 \n\n").busy, 2);

        let parsed = jobs("812 1\n812 1\n813 2\n 813  2 \n814 \n815 x\n816\n816 3\n");
        assert_eq!(parsed.busy, 5);
        assert_eq!(parsed.slots, BTreeSet::from([1, 2, 3]));
        assert_eq!(
            parsed.unlabeled,
            BTreeSet::from(["814".to_string(), "815".to_string()])
        );
    }

    #[tokio::test]
    async fn spawns_every_slot_with_its_own_config_and_reports_idle() {
        let mut h = harness(2, 0);
        assert_eq!(h.run_until(0).await, None);
        let spawns = h.slots.lock().unwrap().spawns.clone();
        assert_eq!(spawns.len(), 2);
        assert_eq!(spawns[0].1, 1);
        assert_eq!(spawns[1].1, 2);
        assert_eq!(spawns[1].2, slot_command(&h.config, 2, Runner::Pool).args);
        assert_eq!(h.reports(), [report(0, "idle", 2)]);
        let sent = h.sent.lock().unwrap()[0].clone();
        assert_eq!(sent.run, h.config.run_id);
        assert_eq!(sent.token, h.config.token);
        assert_eq!(
            sent.body.unwrap(),
            json!({"state": "idle", "free": 2, "slots": 2, "spill": 0})
        );
        h.assert_lines(&[
            r#"info slot started slot="1" runner="pool""#,
            r#"info state from="starting" to="idle" free="2""#,
        ]);
    }

    #[tokio::test]
    async fn reports_on_every_change_and_every_heartbeat() {
        let mut h = harness(2, 0);
        h.job("101", 1, 20, 60, 2);
        h.job("102", 2, 30, 60, 1);
        assert_eq!(h.run_until(1300).await, None);
        assert_eq!(
            h.reports(),
            [
                report(0, "idle", 2),
                report(20, "busy", 1),
                report(30, "busy", 0),
                report(60, "idle", 2),
                report(660, "idle", 2),
                report(1260, "idle", 2),
            ]
        );
    }

    #[tokio::test]
    async fn starting_lasts_until_the_first_report_succeeds() {
        let mut h = harness(1, 3290);
        h.respond(|_, now| if now < 12 { 0 } else { 200 });
        assert_eq!(h.run_until(15).await, None);
        assert_eq!(
            h.reports(),
            [
                report(0, "idle", 1),
                report(5, "idle", 1),
                report(10, "idle", 1),
                report(15, "idle", 1),
            ]
        );
        let lines = h.lines();
        let first_state = lines
            .iter()
            .position(|l| l.starts_with("info state"))
            .unwrap();
        assert_eq!(
            lines[first_state],
            r#"info state from="starting" to="idle" free="1""#
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("warn report failed"))
                .count(),
            3
        );
        assert!(h.quits().is_empty());
    }

    #[tokio::test]
    async fn failed_change_reports_are_retried() {
        let mut h = harness(1, 0);
        h.job("7", 1, 10, 100, 1);
        h.respond(|_, now| if (10..20).contains(&now) { 503 } else { 200 });
        assert_eq!(h.run_until(25).await, None);
        assert_eq!(
            h.reports(),
            [
                report(0, "idle", 1),
                report(10, "busy", 0),
                report(15, "busy", 0),
                report(20, "busy", 0),
            ]
        );
    }

    #[tokio::test]
    async fn idle_at_minute_55_drains_and_terminates() {
        // Uptime 54:50 at the start: the window opens at t = 10.
        let mut h = harness(2, 3290);
        let code = h.run_to_end().await;
        assert_eq!(code, 0);
        assert_eq!(
            h.reports(),
            [report(0, "idle", 2), report(10, "draining", 0)]
        );
        assert_eq!(h.quits(), [(40, 1), (40, 2)]);
        assert_eq!(h.terminates(), [45]);
        h.assert_lines(&[
            r#"info state from="idle" to="draining" free="0""#,
            r#"info state from="draining" to="quitting" free="0""#,
            r#"info terminate accepted attempts="1""#,
        ]);
    }

    #[tokio::test]
    async fn no_drain_before_minute_55() {
        let mut h = harness(1, 0);
        assert_eq!(h.run_until(3295).await, None);
        assert!(h.reports().iter().all(|(_, state, _)| state == "idle"));
        assert!(h.quits().is_empty());
        assert_eq!(h.run_until(3300).await, None);
        assert_eq!(h.reports().last().unwrap(), &report(3300, "draining", 0));
    }

    #[tokio::test]
    async fn a_job_in_the_grace_cancels_the_drain() {
        let mut h = harness(2, 3290);
        h.job("4711", 1, 20, 100, 2);
        let code = h.run_to_end().await;
        assert_eq!(code, 0);
        assert_eq!(
            h.reports(),
            [
                report(0, "idle", 2),
                report(10, "draining", 0),
                report(20, "busy", 1),
                report(100, "draining", 0),
            ]
        );
        // No SIGQUIT while the job ran; the second drain waits its own full grace.
        assert_eq!(h.quits(), [(130, 1), (130, 2)]);
        assert_eq!(h.terminates(), [135]);
        h.assert_lines(&[r#"info state from="draining" to="busy" free="1""#]);
    }

    #[tokio::test]
    async fn sigquit_only_after_the_grace() {
        let mut h = harness(1, 3300);
        assert_eq!(h.run_until(25).await, None);
        assert_eq!(
            h.reports(),
            [report(0, "idle", 1), report(5, "draining", 0)]
        );
        assert!(h.quits().is_empty());
        assert_eq!(h.run_until(30).await, None);
        assert!(h.quits().is_empty());
        assert_eq!(h.run_until(35).await, None);
        assert_eq!(h.quits(), [(35, 1)]);
    }

    #[tokio::test]
    async fn a_failed_draining_report_delays_the_grace() {
        let mut h = harness(1, 3290);
        h.respond(|path, now| {
            if path == "/vm" && (10..20).contains(&now) {
                0
            } else {
                200
            }
        });
        assert_eq!(h.run_until(45).await, None);
        assert_eq!(
            h.reports(),
            [
                report(0, "idle", 1),
                report(10, "draining", 0),
                report(15, "draining", 0),
                report(20, "draining", 0),
            ]
        );
        assert!(h.quits().is_empty());
        assert_eq!(h.run_until(60).await, Some(0));
        assert_eq!(h.quits(), [(50, 1)]);
        assert_eq!(h.terminates(), [55]);
    }

    #[tokio::test]
    async fn terminate_waits_for_every_slot_to_exit() {
        let mut h = harness(2, 3290);
        h.slots.lock().unwrap().quit_delay.insert(2, 12);
        let code = h.run_to_end().await;
        assert_eq!(code, 0);
        assert_eq!(h.quits(), [(40, 1), (40, 2)]);
        let exits = h.slots.lock().unwrap().exits.clone();
        assert_eq!(exits, [(45, 1), (55, 2)]);
        assert_eq!(h.terminates(), [55]);
        // Still draining while the last job finishes: heartbeats keep the VM from the sweep.
        assert!(
            h.reports()
                .iter()
                .skip(1)
                .all(|(_, state, _)| state == "draining")
        );
    }

    #[tokio::test]
    async fn quitting_slots_keep_heartbeats() {
        let mut h = harness(1, 3290);
        h.slots.lock().unwrap().quit_delay.insert(1, 1300);
        assert_eq!(h.run_until(1340).await, Some(0));
        assert_eq!(
            h.reports(),
            [
                report(0, "idle", 1),
                report(10, "draining", 0),
                report(610, "draining", 0),
                report(1210, "draining", 0),
            ]
        );
        assert_eq!(h.terminates(), [1340]);
    }

    #[tokio::test]
    async fn terminate_retries_with_backoff() {
        let mut h = harness(1, 3290);
        let attempts = Arc::new(Mutex::new(0));
        let counter = Arc::clone(&attempts);
        h.respond(move |path, _| {
            if path != "/terminate" {
                return 200;
            }
            let mut count = counter.lock().unwrap();
            *count += 1;
            if *count <= 3 { 503 } else { 200 }
        });
        assert_eq!(h.run_to_end().await, 0);
        assert_eq!(h.terminates(), [45, 47, 51, 59]);
        let sent = h.sent.lock().unwrap().last().unwrap().clone();
        assert_eq!(sent.body, None);
        assert_eq!(sent.token, h.config.token);
    }

    #[tokio::test]
    async fn terminate_gives_up_after_ten_minutes() {
        let mut h = harness(1, 3290);
        h.respond(|path, _| if path == "/terminate" { 0 } else { 200 });
        assert_eq!(h.run_to_end().await, 1);
        let offsets: Vec<u64> = h.terminates().iter().map(|at| at - 45).collect();
        assert_eq!(
            offsets,
            [0, 2, 6, 14, 30, 62, 122, 182, 242, 302, 362, 422, 482, 542]
        );
        h.assert_lines(&[r#"error terminate gave up attempts="14""#]);
    }

    #[tokio::test]
    async fn an_unexpected_exit_restarts_the_slot() {
        let mut h = harness(2, 0);
        h.slots.lock().unwrap().crashes.push((1, 12));
        assert_eq!(h.run_until(20).await, None);
        assert_eq!(
            h.spawns(),
            [(0, 1, "pool"), (0, 2, "pool"), (15, 1, "pool")]
        );
        let lines = h.lines();
        assert!(lines.contains(&r#"warn slot exited slot="1" code="1""#.into()));
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.as_str() == r#"info slot started slot="1" runner="pool""#)
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn no_restart_while_draining() {
        let mut h = harness(2, 3290);
        h.slots.lock().unwrap().crashes.push((1, 22));
        assert_eq!(h.run_to_end().await, 0);
        let spawns = h.slots.lock().unwrap().spawns.len();
        assert_eq!(spawns, 2);
        assert_eq!(h.quits(), [(40, 2)]);
        assert_eq!(h.terminates(), [45]);
    }

    #[tokio::test]
    async fn a_cancelled_drain_restarts_stopped_slots() {
        let mut h = harness(2, 3290);
        h.slots.lock().unwrap().crashes.push((1, 12));
        h.job("9", 2, 20, 30, 1);
        assert_eq!(h.run_until(30).await, None);
        assert_eq!(
            h.spawns(),
            [(0, 1, "pool"), (0, 2, "pool"), (25, 1, "pool")]
        );
    }

    #[tokio::test]
    async fn free_counts_running_slots_only() {
        let mut h = harness(2, 0);
        h.slots.lock().unwrap().broken.push(2);
        assert_eq!(h.run_until(5).await, None);
        assert_eq!(h.reports(), [report(0, "idle", 1)]);
        let lines = h.lines();
        let failed = r#"warn slot start failed slot="2" error="no gitlab-runner""#;
        assert_eq!(lines.iter().filter(|l| l.as_str() == failed).count(), 2);
        h.slots.lock().unwrap().broken.clear();
        assert_eq!(h.run_until(10).await, None);
        assert_eq!(h.reports(), [report(0, "idle", 1), report(10, "idle", 2)]);
    }

    #[tokio::test]
    async fn docker_failures_hold_the_state() {
        let mut h = harness(1, 0);
        *h.docker.failing.lock().unwrap() = true;
        assert_eq!(h.run_until(10).await, None);
        assert!(h.reports().is_empty());
        *h.docker.failing.lock().unwrap() = false;
        assert_eq!(h.run_until(15).await, None);
        assert_eq!(h.reports(), [report(15, "idle", 1)]);
    }

    #[tokio::test]
    async fn spill_switches_only_idle_slots() {
        let mut h = harness(2, 0);
        h.job("101", 1, 0, 1000, 2);
        h.answer(|_| spill_answer(true, 600));
        assert_eq!(h.run_until(100).await, None);
        assert_eq!(h.quits(), [(0, 2)]);
        assert_eq!(
            h.spawns(),
            [(0, 1, "pool"), (0, 2, "pool"), (5, 2, "spill")]
        );
        assert_eq!(
            h.reports_with_spill(),
            [spilled(0, "busy", 1, 0), spilled(5, "busy", 1, 1)]
        );
        let sent = h.sent.lock().unwrap()[1].clone();
        assert_eq!(
            sent.body.unwrap(),
            json!({"state": "busy", "free": 1, "slots": 2, "spill": 1})
        );
        h.assert_lines(&[
            r#"info spill allowed="true""#,
            r#"info slot switching slot="2" from="pool" to="spill""#,
            r#"info slot exited slot="2" code="0""#,
            r#"info slot started slot="2" runner="spill""#,
        ]);
        assert!(
            !h.lines()
                .iter()
                .any(|l| l.contains("slot exited slot=\"1\""))
        );
    }

    #[tokio::test]
    async fn a_busy_slot_switches_after_its_job() {
        let mut h = harness(2, 0);
        h.job("101", 1, 0, 30, 1);
        h.answer(|_| spill_answer(true, 600));
        assert_eq!(h.run_until(60).await, None);
        assert_eq!(h.quits(), [(0, 2), (30, 1)]);
        assert_eq!(
            h.spawns(),
            [
                (0, 1, "pool"),
                (0, 2, "pool"),
                (5, 2, "spill"),
                (35, 1, "spill"),
            ]
        );
        assert_eq!(
            h.reports_with_spill(),
            [
                spilled(0, "busy", 1, 0),
                spilled(5, "busy", 1, 1),
                spilled(30, "idle", 2, 1),
                spilled(35, "idle", 2, 2),
            ]
        );
    }

    #[tokio::test]
    async fn a_job_taken_after_the_idle_check_runs_to_its_end() {
        let mut h = harness(2, 0);
        // Slot 2 looked idle at second 0, but its last request returned a job: `SIGQUIT` lets
        // the job run, and the process exits when the job ends.
        h.job("77", 2, 1, 40, 1);
        h.slots.lock().unwrap().quit_delay.insert(2, 40);
        h.answer(|_| spill_answer(true, 600));
        assert_eq!(h.run_until(60).await, None);
        assert_eq!(h.quits(), [(0, 1), (0, 2)]);
        let exits = h.slots.lock().unwrap().exits.clone();
        assert_eq!(exits, [(5, 1), (40, 2)]);
        assert_eq!(
            h.spawns(),
            [
                (0, 1, "pool"),
                (0, 2, "pool"),
                (5, 1, "spill"),
                (40, 2, "spill"),
            ]
        );
        assert_eq!(
            h.reports_with_spill(),
            [
                spilled(0, "idle", 2, 0),
                spilled(5, "busy", 1, 1),
                spilled(40, "idle", 2, 2),
            ]
        );
        h.assert_lines(&[r#"info slot exited slot="2" code="0""#]);
    }

    #[tokio::test]
    async fn spill_off_switches_back() {
        let mut h = harness(2, 0);
        h.answer(|now| spill_answer(now < 20, 15));
        assert_eq!(h.run_until(40).await, None);
        assert_eq!(h.quits(), [(0, 1), (0, 2), (20, 1), (20, 2)]);
        assert_eq!(
            h.spawns(),
            [
                (0, 1, "pool"),
                (0, 2, "pool"),
                (5, 1, "spill"),
                (5, 2, "spill"),
                (25, 1, "pool"),
                (25, 2, "pool"),
            ]
        );
        assert_eq!(
            h.reports_with_spill(),
            [
                spilled(0, "idle", 2, 0),
                spilled(5, "idle", 2, 2),
                spilled(20, "idle", 2, 2),
                spilled(25, "idle", 2, 0),
                spilled(40, "idle", 2, 0),
            ]
        );
        h.assert_lines(&[
            r#"info spill allowed="false""#,
            r#"info slot switching slot="1" from="spill" to="pool""#,
        ]);
    }

    #[tokio::test]
    async fn without_a_spill_token_no_slot_switches() {
        let text = SAMPLE
            .lines()
            .filter(|line| !line.starts_with("RUNNER_SPILL_TOKEN="))
            .collect::<Vec<_>>()
            .join("\n");
        let mut h = harness_from(&text, 0);
        h.answer(|_| spill_answer(true, 15));
        assert_eq!(h.run_until(60).await, None);
        assert!(h.quits().is_empty());
        assert_eq!(h.spawns(), [(0, 1, "pool"), (0, 2, "pool")]);
        assert_eq!(
            h.reports_with_spill(),
            [0, 15, 30, 45, 60].map(|at| spilled(at, "idle", 2, 0))
        );
    }

    #[tokio::test]
    async fn no_switch_before_the_first_answer() {
        let mut h = harness(2, 0);
        h.respond(|_, now| if now < 12 { 503 } else { 200 });
        h.answer(|_| spill_answer(true, 600));
        assert_eq!(h.run_until(10).await, None);
        assert_eq!(h.reports().len(), 3);
        assert!(h.quits().is_empty());
        assert_eq!(h.run_until(15).await, None);
        assert_eq!(h.quits(), [(15, 1), (15, 2)]);
        assert_eq!(h.spawns(), [(0, 1, "pool"), (0, 2, "pool")]);
    }

    #[tokio::test]
    async fn the_report_cadence_follows_next_report_s_within_10_to_600_s() {
        let mut h = harness(1, 0);
        h.answer(|now| {
            let next = match now {
                0..100 => 30,
                100..200 => 3,
                _ => 5000,
            };
            spill_answer(false, next)
        });
        assert_eq!(h.run_until(1000).await, None);
        let times: Vec<u64> = h.reports().into_iter().map(|(at, _, _)| at).collect();
        assert_eq!(
            times,
            [
                0, 30, 60, 90, 120, 130, 140, 150, 160, 170, 180, 190, 200, 800
            ]
        );
    }

    #[tokio::test]
    async fn an_answer_without_the_new_fields_means_no_spill_and_ten_minutes() {
        let mut h = harness(2, 0);
        h.answer(|now| match now {
            0..10 => spill_answer(true, 15),
            10..600 => old_answer(),
            _ => Value::Null,
        });
        assert_eq!(h.run_until(1300).await, None);
        assert_eq!(h.quits(), [(0, 1), (0, 2), (20, 1), (20, 2)]);
        assert_eq!(
            h.reports_with_spill(),
            [
                spilled(0, "idle", 2, 0),
                spilled(5, "idle", 2, 2),
                spilled(20, "idle", 2, 2),
                spilled(25, "idle", 2, 0),
                spilled(625, "idle", 2, 0),
                spilled(1225, "idle", 2, 0),
            ]
        );
        let unreadable = r#"warn report answer unreadable error="json: invalid type: null, expected struct VmReportAnswer at line 1 column 4""#;
        assert_eq!(
            h.lines()
                .iter()
                .filter(|l| l.as_str() == unreadable)
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn a_job_without_slot_label_holds_every_switch() {
        let mut h = harness(2, 0);
        h.unlabeled_job("55", 0, 30, 2);
        h.answer(|_| spill_answer(true, 600));
        assert_eq!(h.run_until(40).await, None);
        assert_eq!(h.quits(), [(30, 1), (30, 2)]);
        let warned = r#"warn job without slot label job="55""#;
        assert_eq!(h.lines().iter().filter(|l| l.as_str() == warned).count(), 1);
    }

    #[tokio::test]
    async fn no_switching_while_draining() {
        let mut h = harness(2, 3290);
        // A dispatcher answers a draining VM with spill false; this one does not.
        h.answer(|now| spill_answer(now >= 10, 15));
        assert_eq!(h.run_to_end().await, 0);
        assert_eq!(h.quits(), [(40, 1), (40, 2)]);
        assert_eq!(h.spawns(), [(0, 1, "pool"), (0, 2, "pool")]);
        assert_eq!(h.terminates(), [45]);
        assert_eq!(
            h.reports_with_spill(),
            [
                spilled(0, "idle", 2, 0),
                spilled(10, "draining", 0, 0),
                spilled(25, "draining", 0, 0),
                spilled(40, "draining", 0, 0),
            ]
        );
    }

    #[tokio::test]
    async fn a_switch_in_flight_keeps_the_drain_grace() {
        let mut h = harness(2, 3290);
        h.slots.lock().unwrap().quit_delay.insert(2, 12);
        h.answer(|_| spill_answer(true, 15));
        assert_eq!(h.run_to_end().await, 0);
        // Slot 2 exits at 12, in the grace, and stays stopped; the drain's `SIGQUIT` comes at
        // the end of the full grace and only for the slot that did not get one.
        assert_eq!(h.quits(), [(0, 1), (0, 2), (40, 1)]);
        assert_eq!(
            h.spawns(),
            [(0, 1, "pool"), (0, 2, "pool"), (5, 1, "spill")]
        );
        assert_eq!(h.terminates(), [45]);
        assert_eq!(
            h.reports_with_spill(),
            [
                spilled(0, "idle", 2, 0),
                spilled(5, "idle", 2, 1),
                spilled(10, "draining", 0, 1),
                spilled(25, "draining", 0, 1),
                spilled(40, "draining", 0, 1),
            ]
        );
    }

    #[tokio::test]
    async fn logs_never_carry_a_secret() {
        let mut h = harness(2, 3290);
        h.slots.lock().unwrap().crashes.push((2, 3));
        h.respond(|path, now| {
            if path == "/terminate" && now < 50 {
                401
            } else {
                200
            }
        });
        h.answer(|_| spill_answer(true, 15));
        assert_eq!(h.run_to_end().await, 0);
        assert!(h.spawns().iter().any(|(_, _, runner)| *runner == "spill"));
        let lines = h.lines();
        assert!(!lines.is_empty());
        for line in &lines {
            for secret in [
                "vm-token-secret",
                POOL_TOKEN,
                SPILL_TOKEN,
                "cache-secret-key",
                "SCWACCESSKEYSAMPLE",
            ] {
                assert!(!line.contains(secret), "{line}");
            }
        }
    }
}
