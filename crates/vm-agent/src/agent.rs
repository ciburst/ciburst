//! The runner process, state reports and the drain protocol of spec section 6.1; the member
//! runners of M6.3 rule R5 as entries of the runner's `config.toml`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use ciburst_core::http::Request;
use ciburst_core::log::Log;
use ciburst_core::types::{VmReport, VmReportAnswer, VmState};
use ciburst_core::vm;

use crate::config::Config;
use crate::runners::{RunnerConfig, render};
use crate::system::{ProcessCommand, System};

pub const POLL: Duration = Duration::from_secs(5);
pub const RUNNER: &str = "gitlab-runner";
pub const CONFIG_FILE: &str = "config.toml";
const HEARTBEAT: Duration = Duration::from_secs(vm::HEARTBEAT_S);
/// Shortest report interval an answer may ask for (M6.2 rule R7); the longest is [`HEARTBEAT`].
const REPORT_MIN: Duration = Duration::from_secs(10);
const DRAIN_GRACE: Duration = Duration::from_secs(vm::DRAIN_GRACE_S);
const TERMINATE_FOR: Duration = Duration::from_secs(600);
const BACKOFF_FIRST: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

pub fn config_path(config: &Config) -> PathBuf {
    PathBuf::from(&config.state_dir).join(CONFIG_FILE)
}

/// One `gitlab-runner run` for every runner entry; it reloads `config.toml` when it changes.
pub fn runner_command(config: &Config) -> ProcessCommand {
    ProcessCommand {
        program: RUNNER.to_string(),
        args: vec![
            "run".to_string(),
            "--config".to_string(),
            config_path(config).to_string_lossy().into_owned(),
            "--working-directory".to_string(),
            config.state_dir.clone(),
        ],
    }
}

/// Running jobs: the distinct job ids of `docker ps`, one container per line (a job with
/// services has several).
pub fn busy(docker_ps: &str) -> u32 {
    let jobs: BTreeSet<&str> = docker_ps
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    u32::try_from(jobs.len()).unwrap_or(u32::MAX)
}

/// What a dispatcher before M6.2 answers, and what holds before the first answer.
fn no_answer() -> VmReportAnswer {
    VmReportAnswer {
        runners: Vec::new(),
        next_report_s: vm::HEARTBEAT_S,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No report has succeeded yet.
    Starting,
    Idle,
    Busy,
    /// Reported `draining` at `since`; the runner keeps polling for the grace.
    Draining {
        since: Duration,
    },
    /// The runner got `SIGQUIT`; still reported as `draining`.
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

/// The running `gitlab-runner run`.
#[derive(Debug, Clone, Copy, Default)]
struct Running {
    /// It got `SIGQUIT`: it requests no further job and exits after the running ones.
    quit: bool,
}

pub struct Agent {
    config: Config,
    sys: System,
    log: Log,
    phase: Phase,
    process: Option<Running>,
    /// Busy slots from the last successful `docker ps`, `None` before the first.
    busy: Option<u32>,
    /// Last report the dispatcher accepted, and when.
    reported: Option<(VmReport, Duration)>,
    /// The dispatcher's answer to that report.
    answer: VmReportAnswer,
    /// The `config.toml` on disk, `None` before the first write.
    written: Option<RunnerConfig>,
}

impl Agent {
    pub fn new(config: Config, sys: System, log: Log) -> Agent {
        Agent {
            config,
            sys,
            log,
            phase: Phase::Starting,
            process: None,
            busy: None,
            reported: None,
            answer: no_answer(),
            written: None,
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
        if self.phase != Phase::Quitting {
            self.write_config();
        }
        if !self.phase.draining() {
            self.start_runner();
        }
        if self.phase == Phase::Quitting {
            if self.process.is_none() {
                return Some(self.terminate().await);
            }
            self.quit_runner();
        }
        let busy = match self.sys.docker.job_labels().await {
            Ok(output) => Some(busy(&output)),
            Err(error) => {
                self.log
                    .warn("docker ps failed", &[("error", &error.to_string())]);
                None
            }
        };
        if busy.is_some() {
            self.busy = busy;
        }
        self.step(busy).await;
        self.report_if_due().await;
        // The answer's runners reach the runner without waiting for the next poll.
        if self.phase != Phase::Quitting {
            self.write_config();
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
                    self.quit_runner();
                    self.transition(Phase::Quitting);
                }
            }
        }
    }

    /// Free slots as reported in `phase`: `slots - busy` while the runner runs, none while
    /// draining.
    fn free_in(&self, phase: Phase) -> Option<u32> {
        let busy = self.busy?;
        Some(if phase.draining() || self.process.is_none() {
            0
        } else {
            self.config.slots.saturating_sub(busy)
        })
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
                    // A garbled answer drops no member's runner.
                    VmReportAnswer {
                        runners: self.answer.runners.clone(),
                        ..no_answer()
                    }
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

    /// Writes `config.toml` when the wanted runners differ from the file's; the runner reloads
    /// it within about a second (`docs/RUNBOOK.md`, M6.3 phase 0).
    fn write_config(&mut self) {
        let rendered = render(&self.config, &self.answer.runners);
        if self
            .written
            .as_ref()
            .is_some_and(|written| written.content == rendered.content)
        {
            return;
        }
        match self
            .sys
            .files
            .write_atomic(&config_path(&self.config), &rendered.content)
        {
            Ok(()) => {
                self.log.info(
                    "config written",
                    &[("runners", &rendered.runners.to_string())],
                );
                self.written = Some(rendered);
            }
            Err(error) => self
                .log
                .warn("config write failed", &[("error", &error.to_string())]),
        }
    }

    fn reap(&mut self) {
        let Some(running) = self.process else { return };
        match self.sys.process.try_exit() {
            Ok(Some(code)) => {
                self.process = None;
                let code = code.to_string();
                if running.quit || self.phase == Phase::Quitting {
                    self.log.info("runner exited", &[("code", &code)]);
                } else {
                    self.log.warn("runner exited", &[("code", &code)]);
                }
            }
            Ok(None) => {}
            Err(error) => self
                .log
                .warn("runner status unknown", &[("error", &error.to_string())]),
        }
    }

    /// Starts the runner once `config.toml` exists, and again after an unexpected exit.
    fn start_runner(&mut self) {
        if self.process.is_some() || self.written.is_none() {
            return;
        }
        match self.sys.process.spawn(&runner_command(&self.config)) {
            Ok(pid) => {
                self.process = Some(Running::default());
                self.log
                    .info("runner started", &[("pid", &pid.to_string())]);
            }
            Err(error) => self
                .log
                .warn("runner start failed", &[("error", &error.to_string())]),
        }
    }

    /// The drain's one `SIGQUIT`.
    fn quit_runner(&mut self) {
        let Some(running) = &mut self.process else {
            return;
        };
        if running.quit {
            return;
        }
        match self.sys.process.quit() {
            Ok(()) => running.quit = true,
            Err(error) => self
                .log
                .warn("runner quit failed", &[("error", &error.to_string())]),
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
    use std::io;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use ciburst_core::http::{HttpClient, Response};
    use serde_json::{Value, json};

    use super::*;
    use crate::config::tests::{SAMPLE, sample};
    use crate::config::{Config, parse};
    use crate::system::{Clock, Docker, Files, Process, Uptime};

    const SECRETS: [&str; 6] = [
        "vm-token-secret",
        "glrt-runner-secret",
        "glrt-member-a",
        "glrt-member-b",
        "cache-secret-key",
        "SCWACCESSKEYSAMPLE",
    ];

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

    /// A job's containers: `id` runs from `from` to before `until` with `containers`
    /// containers.
    struct Job {
        id: &'static str,
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
                    for _ in 0..job.containers {
                        out.push_str(&format!("{}\n", job.id));
                    }
                }
            }
            Ok(out)
        }
    }

    #[derive(Default)]
    struct ProcState {
        spawns: Vec<(u64, Vec<String>)>,
        quits: Vec<u64>,
        exits: Vec<(u64, i32)>,
        /// `Some(exit time)` while running, the time once `SIGQUIT` scheduled the exit.
        running: Option<Option<u64>>,
        /// Seconds the runner takes to exit after `SIGQUIT`.
        quit_delay: u64,
        /// Unexpected exits.
        crashes: Vec<u64>,
        /// The binary cannot start.
        broken: bool,
    }

    struct FakeProcess {
        clock: FakeClock,
        state: Arc<Mutex<ProcState>>,
    }

    impl Process for FakeProcess {
        fn spawn(&mut self, command: &ProcessCommand) -> io::Result<u32> {
            let mut state = self.state.lock().unwrap();
            if state.broken {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no gitlab-runner"));
            }
            assert!(state.running.is_none(), "runner spawned twice");
            state
                .spawns
                .push((self.clock.elapsed(), command.args.clone()));
            state.running = Some(None);
            Ok(100 + u32::try_from(state.spawns.len()).unwrap())
        }

        fn try_exit(&mut self) -> io::Result<Option<i32>> {
            let now = self.clock.elapsed();
            let mut state = self.state.lock().unwrap();
            let Some(exit_at) = state.running else {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no runner process"));
            };
            if let Some(position) = state.crashes.iter().position(|at| *at <= now) {
                state.crashes.remove(position);
                state.running = None;
                state.exits.push((now, 1));
                return Ok(Some(1));
            }
            match exit_at {
                Some(at) if at <= now => {
                    state.running = None;
                    state.exits.push((now, 0));
                    Ok(Some(0))
                }
                _ => Ok(None),
            }
        }

        fn quit(&mut self) -> io::Result<()> {
            let now = self.clock.elapsed();
            let mut state = self.state.lock().unwrap();
            assert!(state.running.is_some(), "SIGQUIT without a runner");
            state.quits.push(now);
            let delay = state.quit_delay;
            state.running = Some(Some(now + delay));
            Ok(())
        }
    }

    #[derive(Default)]
    struct FileState {
        writes: Vec<(u64, PathBuf, String)>,
        failing: bool,
    }

    struct FakeFiles {
        clock: FakeClock,
        state: Arc<Mutex<FileState>>,
    }

    impl Files for FakeFiles {
        fn write_atomic(&self, path: &Path, content: &str) -> io::Result<()> {
            let mut state = self.state.lock().unwrap();
            if state.failing {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only"));
            }
            state.writes.push((
                self.clock.elapsed(),
                path.to_path_buf(),
                content.to_string(),
            ));
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

    fn next_answer(next_report_s: u64) -> Value {
        json!({"result": "reported", "nextReportS": next_report_s, "ms": 3})
    }

    /// An M6.3 answer with member runners as (project id, token).
    fn member_answer(next_report_s: u64, members: &[(u64, &str)]) -> Value {
        let runners: Vec<Value> = members
            .iter()
            .map(|(project, token)| json!({"projectId": project, "token": token}))
            .collect();
        json!({
            "result": "reported",
            "nextReportS": next_report_s,
            "runners": runners,
            "ms": 3,
        })
    }

    struct Harness {
        clock: FakeClock,
        docker: FakeDocker,
        process: Arc<Mutex<ProcState>>,
        files: Arc<Mutex<FileState>>,
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
        let process = Arc::new(Mutex::new(ProcState::default()));
        let files = Arc::new(Mutex::new(FileState::default()));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let respond: Arc<Mutex<Responder>> = Arc::new(Mutex::new(Box::new(|_, _| 200)));
        let answer: Arc<Mutex<Answerer>> = Arc::new(Mutex::new(Box::new(|_| old_answer())));
        let (log, lines) = Log::memory();
        let sys = System {
            clock: Arc::new(clock.clone()),
            uptime: Arc::new(clock.clone()),
            process: Box::new(FakeProcess {
                clock: clock.clone(),
                state: Arc::clone(&process),
            }),
            files: Arc::new(FakeFiles {
                clock: clock.clone(),
                state: Arc::clone(&files),
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
            process,
            files,
            sent,
            respond,
            answer,
            lines,
            config,
        }
    }

    impl Harness {
        fn job(&self, id: &'static str, from: u64, until: u64, containers: usize) {
            self.docker.jobs.lock().unwrap().push(Job {
                id,
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
        /// Accepted and failed `/vm` reports as (time, state, free).
        fn reports(&self) -> Vec<(u64, String, u64)> {
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

        fn quits(&self) -> Vec<u64> {
            self.process.lock().unwrap().quits.clone()
        }

        fn spawns(&self) -> Vec<u64> {
            let state = self.process.lock().unwrap();
            state.spawns.iter().map(|(at, _)| *at).collect()
        }

        /// Every `config.toml` written, as (time, runner entry names).
        fn configs(&self) -> Vec<(u64, Vec<String>)> {
            let state = self.files.lock().unwrap();
            state
                .writes
                .iter()
                .map(|(at, path, content)| {
                    assert_eq!(path, &config_path(&self.config));
                    let names = content
                        .lines()
                        .filter_map(|line| line.strip_prefix("name = \""))
                        .map(|name| name.trim_end_matches('"').to_string())
                        .collect();
                    (*at, names)
                })
                .collect()
        }

        fn lines(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }

        fn count(&self, line: &str) -> usize {
            self.lines().iter().filter(|l| l.as_str() == line).count()
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

    fn names(at: u64, names: &[&str]) -> (u64, Vec<String>) {
        (at, names.iter().map(|name| name.to_string()).collect())
    }

    #[test]
    fn the_runner_command_reads_the_state_dir_config() {
        let command = runner_command(&sample());
        assert_eq!(command.program, "gitlab-runner");
        assert_eq!(
            command.args,
            [
                "run",
                "--config",
                &PathBuf::from("/var/lib/ciburst")
                    .join("config.toml")
                    .to_string_lossy(),
                "--working-directory",
                "/var/lib/ciburst",
            ]
        );
    }

    #[test]
    fn busy_counts_distinct_job_ids() {
        assert_eq!(busy(""), 0);
        assert_eq!(busy("\n \n"), 0);
        assert_eq!(busy("812\n"), 1);
        assert_eq!(busy("812\n812\n813\n 813 \n\n"), 2);
    }

    #[tokio::test]
    async fn writes_the_config_then_starts_one_runner_and_reports_idle() {
        let mut h = harness(2, 0);
        assert_eq!(h.run_until(0).await, None);
        assert_eq!(h.configs(), [names(0, &["pool"])]);
        let written = h.files.lock().unwrap().writes[0].2.clone();
        assert_eq!(written, render(&h.config, &[]).content);
        let spawns = h.process.lock().unwrap().spawns.clone();
        assert_eq!(spawns, [(0, runner_command(&h.config).args)]);
        assert_eq!(h.reports(), [report(0, "idle", 2)]);
        let sent = h.sent.lock().unwrap()[0].clone();
        assert_eq!(sent.run, h.config.run_id);
        assert_eq!(sent.token, h.config.token);
        assert_eq!(
            sent.body.unwrap(),
            json!({"state": "idle", "free": 2, "slots": 2})
        );
        h.assert_lines(&[
            r#"info config written runners="1""#,
            r#"info runner started pid="101""#,
            r#"info state from="starting" to="idle" free="2""#,
        ]);
    }

    #[tokio::test]
    async fn reports_on_every_change_and_every_heartbeat() {
        let mut h = harness(2, 0);
        h.job("101", 20, 60, 2);
        h.job("102", 30, 60, 1);
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
        assert_eq!(h.spawns(), [0]);
        assert_eq!(h.configs().len(), 1);
    }

    #[tokio::test]
    async fn free_never_drops_below_zero() {
        let mut h = harness(1, 0);
        h.job("1", 0, 100, 1);
        h.job("2", 0, 100, 1);
        assert_eq!(h.run_until(0).await, None);
        assert_eq!(h.reports(), [report(0, "busy", 0)]);
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
        h.job("7", 10, 100, 1);
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
    async fn idle_at_minute_55_drains_with_one_sigquit_and_terminates() {
        // Uptime 54:50 at the start: the window opens at t = 10.
        let mut h = harness(2, 3290);
        let code = h.run_to_end().await;
        assert_eq!(code, 0);
        assert_eq!(
            h.reports(),
            [report(0, "idle", 2), report(10, "draining", 0)]
        );
        assert_eq!(h.quits(), [40]);
        assert_eq!(h.process.lock().unwrap().exits, [(45, 0)]);
        assert_eq!(h.terminates(), [45]);
        h.assert_lines(&[
            r#"info state from="idle" to="draining" free="0""#,
            r#"info state from="draining" to="quitting" free="0""#,
            r#"info runner exited code="0""#,
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
        h.job("4711", 20, 100, 2);
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
        assert_eq!(h.quits(), [130]);
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
        assert_eq!(h.quits(), [35]);
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
        assert_eq!(h.quits(), [50]);
        assert_eq!(h.terminates(), [55]);
    }

    #[tokio::test]
    async fn terminate_waits_for_the_runner_to_exit() {
        let mut h = harness(2, 3290);
        h.process.lock().unwrap().quit_delay = 12;
        let code = h.run_to_end().await;
        assert_eq!(code, 0);
        assert_eq!(h.quits(), [40]);
        assert_eq!(h.process.lock().unwrap().exits, [(55, 0)]);
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
    async fn quitting_keeps_heartbeats_and_sends_sigquit_once() {
        let mut h = harness(1, 3290);
        h.process.lock().unwrap().quit_delay = 1300;
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
        assert_eq!(h.quits(), [40]);
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
    async fn an_unexpected_exit_restarts_the_runner() {
        let mut h = harness(2, 0);
        h.process.lock().unwrap().crashes.push(12);
        assert_eq!(h.run_until(20).await, None);
        assert_eq!(h.spawns(), [0, 15]);
        assert_eq!(h.count(r#"warn runner exited code="1""#), 1);
        h.assert_lines(&[
            r#"info runner started pid="101""#,
            r#"info runner started pid="102""#,
        ]);
        // The file stays as it was: the new process reads the same runners.
        assert_eq!(h.configs().len(), 1);
        assert!(h.quits().is_empty());
    }

    #[tokio::test]
    async fn no_restart_while_draining() {
        let mut h = harness(2, 3290);
        h.process.lock().unwrap().crashes.push(22);
        assert_eq!(h.run_to_end().await, 0);
        assert_eq!(h.spawns(), [0]);
        assert!(h.quits().is_empty());
        assert_eq!(h.terminates(), [45]);
    }

    #[tokio::test]
    async fn a_cancelled_drain_restarts_a_stopped_runner() {
        let mut h = harness(2, 3290);
        h.process.lock().unwrap().crashes.push(12);
        h.job("9", 20, 30, 1);
        assert_eq!(h.run_until(30).await, None);
        assert_eq!(h.spawns(), [0, 25]);
    }

    #[tokio::test]
    async fn without_a_runner_no_slot_is_free() {
        let mut h = harness(2, 0);
        h.process.lock().unwrap().broken = true;
        assert_eq!(h.run_until(5).await, None);
        assert_eq!(h.reports(), [report(0, "idle", 0)]);
        assert_eq!(
            h.count(r#"warn runner start failed error="no gitlab-runner""#),
            2
        );
        h.process.lock().unwrap().broken = false;
        assert_eq!(h.run_until(10).await, None);
        assert_eq!(h.reports(), [report(0, "idle", 0), report(10, "idle", 2)]);
    }

    #[tokio::test]
    async fn no_runner_before_the_config_is_written() {
        let mut h = harness(1, 0);
        h.files.lock().unwrap().failing = true;
        assert_eq!(h.run_until(5).await, None);
        assert!(h.spawns().is_empty());
        assert_eq!(h.count(r#"warn config write failed error="read-only""#), 4);
        h.files.lock().unwrap().failing = false;
        assert_eq!(h.run_until(10).await, None);
        assert_eq!(h.spawns(), [10]);
        assert_eq!(h.configs(), [names(10, &["pool"])]);
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
    async fn member_runners_follow_the_answer_and_the_file_changes_only_with_them() {
        let mut h = harness(2, 0);
        h.answer(|now| match now {
            0..20 => member_answer(15, &[(42, "glrt-member-a")]),
            20..40 => member_answer(15, &[(77, "glrt-member-b"), (42, "glrt-member-a")]),
            _ => member_answer(15, &[(77, "glrt-member-b")]),
        });
        assert_eq!(h.run_until(120).await, None);
        assert_eq!(
            h.configs(),
            [
                names(0, &["pool"]),
                names(0, &["pool", "member-42"]),
                names(30, &["pool", "member-42", "member-77"]),
                names(45, &["pool", "member-77"]),
            ]
        );
        let last = h.files.lock().unwrap().writes.last().unwrap().2.clone();
        assert!(last.contains("token = \"glrt-member-b\""));
        assert!(!last.contains("glrt-member-a"));
        // One process throughout: gitlab-runner reloads the file itself.
        assert_eq!(h.spawns(), [0]);
        assert!(h.quits().is_empty());
        h.assert_lines(&[
            r#"info config written runners="1""#,
            r#"info config written runners="2""#,
            r#"info config written runners="3""#,
        ]);
        assert_eq!(h.count(r#"info config written runners="2""#), 2);
        assert!(
            h.reports()
                .iter()
                .all(|(_, state, free)| state == "idle" && *free == 2)
        );
    }

    #[tokio::test]
    async fn an_unreadable_answer_keeps_the_members() {
        let mut h = harness(2, 0);
        h.answer(|now| match now {
            0..10 => member_answer(15, &[(42, "glrt-member-a")]),
            _ => Value::Null,
        });
        assert_eq!(h.run_until(30).await, None);
        assert_eq!(
            h.configs(),
            [names(0, &["pool"]), names(0, &["pool", "member-42"])]
        );
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
            next_answer(next)
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
    async fn an_answer_without_the_new_fields_means_ten_minutes() {
        let mut h = harness(2, 0);
        h.answer(|now| match now {
            0..10 => next_answer(15),
            10..600 => old_answer(),
            _ => Value::Null,
        });
        assert_eq!(h.run_until(1300).await, None);
        assert_eq!(
            h.reports(),
            [
                report(0, "idle", 2),
                report(15, "idle", 2),
                report(615, "idle", 2),
                report(1215, "idle", 2),
            ]
        );
        let unreadable = r#"warn report answer unreadable error="json: invalid type: null, expected struct VmReportAnswer at line 1 column 4""#;
        assert_eq!(h.count(unreadable), 2);
    }

    #[tokio::test]
    async fn the_drain_keeps_the_members_and_the_grace() {
        let mut h = harness(2, 3290);
        h.answer(|_| member_answer(15, &[(42, "glrt-member-a")]));
        assert_eq!(h.run_to_end().await, 0);
        assert_eq!(
            h.configs(),
            [names(0, &["pool"]), names(0, &["pool", "member-42"])]
        );
        assert_eq!(h.quits(), [40]);
        assert_eq!(h.terminates(), [45]);
        assert_eq!(
            h.reports(),
            [
                report(0, "idle", 2),
                report(10, "draining", 0),
                report(25, "draining", 0),
                report(40, "draining", 0),
            ]
        );
    }

    #[tokio::test]
    async fn no_config_write_after_sigquit() {
        let mut h = harness(1, 3290);
        h.process.lock().unwrap().quit_delay = 100;
        h.answer(|now| {
            if now < 40 {
                member_answer(10, &[])
            } else {
                member_answer(10, &[(42, "glrt-member-a")])
            }
        });
        assert_eq!(h.run_to_end().await, 0);
        assert_eq!(h.configs(), [names(0, &["pool"])]);
        assert_eq!(h.terminates(), [140]);
    }

    #[tokio::test]
    async fn logs_never_carry_a_secret() {
        let mut h = harness(2, 3290);
        h.process.lock().unwrap().crashes.push(3);
        h.respond(|path, now| {
            if path == "/terminate" && now < 50 {
                401
            } else {
                200
            }
        });
        h.answer(|_| member_answer(15, &[(42, "glrt-member-a"), (77, "glrt-member-b")]));
        assert_eq!(h.run_to_end().await, 0);
        assert!(
            h.configs()
                .iter()
                .any(|(_, names)| names.contains(&"member-77".to_string()))
        );
        let lines = h.lines();
        assert!(!lines.is_empty());
        for line in &lines {
            for secret in SECRETS {
                assert!(!line.contains(secret), "{line}");
            }
        }
    }
}
