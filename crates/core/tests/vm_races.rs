//! Spec 6.1 on a simulated clock: VMs boot, their `run-single` slots ask GitLab for a job every
//! 3 s, the agent counts busy slots every 5 s, reports on change and every `HEARTBEAT_S`, drains
//! at minute 55 with a 30 s grace and `SIGQUIT`, and posts `/terminate` after its last slot
//! exited. The real `Dispatcher` and the fake provider see every webhook, report, termination and
//! the sweep cron. Each scenario asserts that every pending job starts within one job duration
//! plus one boot, that no VM ends while a slot holds a job, and a bound on VM-hours.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::config::parse_config;
use ciburst_core::dispatch::{DispatchResponse, Dispatcher};
use ciburst_core::fake::FakeProvider;
use ciburst_core::log::Log;
use ciburst_core::store::MemoryStore;
use ciburst_core::types::{
    Capabilities, Outputs, Provider, Rotation, Run, Secrets, StartRequest, Step, VmCapacity,
    VmReport, VmState,
};
use ciburst_core::vm;
use serde_json::{Value, json};

/// Unix time of simulated second 0; a multiple of the sweep period, so crons fall on `n * 1800`.
const T0: u64 = 1_800_000_000;
const MIN: u64 = 60;
const HOUR: u64 = 3600;
const BOOT_S: u64 = 120;
const POLL_S: u64 = 3;
const DETECT_S: u64 = 5;
/// The guest's uptime starts after the server is created, so its hour lags the billed one.
const KERNEL_S: u64 = 20;
const SWEEP_EVERY_S: u64 = 1800;
const HOOK: &str = "hook-token";
const VM_SECRET: &str = "vm-secret";

/// Lets two dispatcher calls read capacity before either claims, the double-claim race.
struct Racy {
    inner: Arc<FakeProvider>,
    interleave: AtomicBool,
}

#[async_trait]
impl Provider for Racy {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
        self.inner.plan(rotate).await
    }
    async fn apply(&self, steps: &[Step]) -> Result<Outputs> {
        self.inner.apply(steps).await
    }
    async fn destroy(&self) -> Result<()> {
        self.inner.destroy().await
    }
    async fn start(&self, request: StartRequest) -> Result<String> {
        self.inner.start(request).await
    }
    async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
        self.inner.list(pool).await
    }
    async fn stop(&self, run_id: &str) -> Result<()> {
        self.inner.stop(run_id).await
    }
    async fn capacity(&self, pool: &str) -> Result<Option<Vec<VmCapacity>>> {
        let capacity = self.inner.capacity(pool).await;
        if self.interleave.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        capacity
    }
    async fn claim(&self, run_id: &str) -> Result<()> {
        self.inner.claim(run_id).await
    }
    async fn report(&self, run_id: &str, report: &VmReport, now: u64) -> Result<()> {
        self.inner.report(run_id, report, now).await
    }
    async fn sweep(&self, now: u64) -> Result<Vec<String>> {
        self.inner.sweep(now).await
    }
}

#[derive(Debug)]
struct Job {
    pending_at: u64,
    duration: u64,
    /// When and on which VM a slot took it.
    started: Option<(u64, String)>,
    /// Failed on GitLab's side because its VM hung.
    failed: bool,
}

#[derive(Debug, Clone, Default)]
struct Slot {
    job: Option<usize>,
    ends_at: u64,
    next_poll: u64,
    quit: bool,
    exited: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Booting,
    Serving,
    /// Draining since the given second, slots still polling.
    Grace(u64),
    /// `SIGQUIT` sent, waiting for the slots to exit.
    Quitting,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Terminated,
    Swept,
}

#[derive(Debug)]
struct Vm {
    run_id: String,
    token: String,
    created_at: u64,
    boots: bool,
    hung: bool,
    slots: Vec<Slot>,
    seen_busy: u32,
    next_detect: u64,
    phase: Phase,
    reported: Option<(VmState, u32)>,
    last_report: u64,
    reports: Vec<(u64, VmState)>,
    ended: Option<(u64, End)>,
    jobs_taken: usize,
}

impl Vm {
    fn busy(&self) -> u32 {
        self.slots.iter().filter(|s| s.job.is_some()).count() as u32
    }

    fn ended_by(&self) -> Option<End> {
        self.ended.map(|(_, how)| how)
    }

    fn hours(&self, now: u64) -> u64 {
        let until = self.ended.map(|(t, _)| t).unwrap_or(now);
        until.saturating_sub(self.created_at).div_ceil(HOUR).max(1)
    }

    fn states(&self) -> Vec<VmState> {
        let mut states: Vec<VmState> = Vec::new();
        for (_, state) in &self.reports {
            if states.last() != Some(state) {
                states.push(*state);
            }
        }
        states
    }
}

struct Decision {
    result: String,
    run: Option<String>,
}

struct Sim {
    /// Simulated seconds since `T0`.
    now: Arc<AtomicU64>,
    provider: Arc<Racy>,
    dispatcher: Dispatcher,
    slots: u32,
    jobs: Vec<Job>,
    vms: Vec<Vm>,
    decisions: Vec<Decision>,
    violations: Vec<String>,
    lines: Arc<Mutex<Vec<String>>>,
    next_boots: bool,
}

impl Sim {
    async fn new(slots: u32, max_runs: u32) -> Sim {
        let config = Arc::new(
            parse_config(
                &json!({
                    "name": "sim",
                    "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
                    "host": { "type": "fake" },
                    "provider": { "type": "fake" },
                    "defaultPool": "vm",
                    "pools": [{
                        "tag": "vm", "image": "img/node", "runtime": "instances",
                        "slots": slots, "maxRuns": max_runs
                    }]
                })
                .to_string(),
            )
            .unwrap(),
        );
        let now = Arc::new(AtomicU64::new(0));
        let clock = now.clone();
        let inner = Arc::new(
            FakeProvider::new(config.clone(), Arc::new(MemoryStore::new()))
                .with_clock(Arc::new(move || T0 + clock.load(Ordering::SeqCst))),
        );
        inner.apply(&inner.plan(&[]).await.unwrap()).await.unwrap();
        let provider = Arc::new(Racy {
            inner,
            interleave: AtomicBool::new(false),
        });
        let secrets = Secrets {
            hook_token: Some(HOOK.into()),
            runner_tokens: BTreeMap::from([("vm".to_string(), "glrt-vm".to_string())]),
            dispatch: None,
            cache: None,
            vm_secret: Some(VM_SECRET.into()),
        };
        let (log, lines) = Log::memory();
        Sim {
            now,
            dispatcher: Dispatcher::new(config, provider.clone(), Arc::new(secrets), log),
            provider,
            slots,
            jobs: Vec::new(),
            vms: Vec::new(),
            decisions: Vec::new(),
            violations: Vec::new(),
            lines,
            next_boots: true,
        }
    }

    fn t(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    fn vm(&self, run: &str) -> &Vm {
        self.vms.iter().find(|v| v.run_id == run).expect("known VM")
    }

    fn vm_index(&self, run: &str) -> usize {
        self.vms
            .iter()
            .position(|v| v.run_id == run)
            .expect("known VM")
    }

    fn results(&self) -> Vec<&str> {
        self.decisions.iter().map(|d| d.result.as_str()).collect()
    }

    fn run_of(&self, job: usize) -> &str {
        self.decisions[job]
            .run
            .as_deref()
            .expect("decision with a run")
    }

    fn vm_hours(&self) -> u64 {
        self.vms.iter().map(|v| v.hours(self.t())).sum()
    }

    async fn request(
        &self,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> DispatchResponse {
        self.dispatcher
            .handle_request("POST", path, headers, body, T0 + self.t())
            .await
    }

    fn webhook(&mut self, duration: u64) -> (usize, Vec<u8>) {
        let index = self.jobs.len();
        self.jobs.push(Job {
            pending_at: self.t(),
            duration,
            started: None,
            failed: false,
        });
        let body = json!({
            "object_kind": "build", "build_status": "pending", "build_id": index,
            "pipeline_id": 1, "project_id": 7, "build_stage": "test"
        });
        (index, body.to_string().into_bytes())
    }

    fn record(&mut self, response: DispatchResponse) {
        assert_eq!(response.status, 200, "{}", response.body);
        let result = response.body["result"].as_str().unwrap().to_string();
        let run = response.body["runId"].as_str().map(String::from);
        if result == "started" {
            let run_id = run.clone().unwrap();
            let boots = std::mem::replace(&mut self.next_boots, true);
            self.vms.push(Vm {
                token: vm::token(VM_SECRET, &run_id),
                run_id,
                created_at: self.t(),
                boots,
                hung: false,
                slots: vec![Slot::default(); self.slots as usize],
                seen_busy: 0,
                next_detect: 0,
                phase: Phase::Booting,
                reported: None,
                last_report: 0,
                reports: Vec::new(),
                ended: None,
                jobs_taken: 0,
            });
        }
        self.decisions.push(Decision { result, run });
    }

    /// A job goes pending now; GitLab sends its event.
    async fn pending(&mut self, duration: u64) -> usize {
        let (index, body) = self.webhook(duration);
        let response = self.request("/", &gitlab_headers(), &body).await;
        self.record(response);
        index
    }

    /// Two jobs go pending at once and both dispatcher calls read capacity before either claims.
    async fn pending_together(&mut self, duration: u64) -> (usize, usize) {
        let (a, body_a) = self.webhook(duration);
        let (b, body_b) = self.webhook(duration);
        let headers = gitlab_headers();
        self.provider.interleave.store(true, Ordering::SeqCst);
        let (first, second) = tokio::join!(
            self.request("/", &headers, &body_a),
            self.request("/", &headers, &body_b)
        );
        self.provider.interleave.store(false, Ordering::SeqCst);
        self.record(first);
        self.record(second);
        (a, b)
    }

    /// The VM started next never finishes cloud-init and never reports.
    fn next_never_boots(&mut self) {
        self.next_boots = false;
    }

    /// The VM stops answering; GitLab fails its running jobs.
    fn hang(&mut self, run: &str) {
        let index = self.vm_index(run);
        self.vms[index].hung = true;
        for slot in &self.vms[index].slots {
            if let Some(job) = slot.job {
                self.jobs[job].failed = true;
            }
        }
    }

    async fn run_to(&mut self, until: u64) {
        while self.t() < until {
            self.now.fetch_add(1, Ordering::SeqCst);
            self.tick().await;
        }
    }

    async fn tick(&mut self) {
        let t = self.t();
        if t.is_multiple_of(SWEEP_EVERY_S) {
            self.sweep().await;
        }
        for index in 0..self.vms.len() {
            self.step(index, t).await;
        }
    }

    async fn sweep(&mut self) {
        let body = json!({ vm::SWEEP_ARG: HOOK }).to_string().into_bytes();
        let response = self.request("/", &[], &body).await;
        assert_eq!(response.status, 200, "{}", response.body);
        assert_eq!(response.body["result"], "swept");
        let swept: Vec<String> = response.body["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap().to_string())
            .collect();
        for run in swept {
            let index = self.vm_index(&run);
            self.end(index, End::Swept);
        }
    }

    fn end(&mut self, index: usize, how: End) {
        let t = self.t();
        let vm = &mut self.vms[index];
        if vm.ended.is_some() {
            self.violations
                .push(format!("{} ended twice at {t}", vm.run_id));
            return;
        }
        if !vm.hung && vm.busy() > 0 {
            self.violations.push(format!(
                "{} ended at {t} by {how:?} while a slot ran a job",
                vm.run_id
            ));
        }
        vm.ended = Some((t, how));
        vm.phase = Phase::Ended;
    }

    fn next_pending(&self) -> Option<usize> {
        self.jobs
            .iter()
            .position(|j| j.started.is_none() && !j.failed)
    }

    async fn step(&mut self, index: usize, t: u64) {
        {
            let vm = &mut self.vms[index];
            if vm.ended.is_some() || vm.hung || !vm.boots {
                return;
            }
            if vm.phase == Phase::Booting {
                if t < vm.created_at + BOOT_S {
                    return;
                }
                vm.phase = Phase::Serving;
                vm.next_detect = t;
                for slot in &mut vm.slots {
                    slot.next_poll = t;
                }
            }
            for slot in &mut vm.slots {
                if slot.job.is_some() && slot.ends_at <= t {
                    slot.job = None;
                    slot.next_poll = t;
                    slot.exited = slot.quit;
                }
            }
        }

        for s in 0..self.vms[index].slots.len() {
            let slot = &self.vms[index].slots[s];
            if slot.exited || slot.quit || slot.job.is_some() || slot.next_poll > t {
                continue;
            }
            match self.next_pending() {
                Some(job) => {
                    let run = self.vms[index].run_id.clone();
                    self.jobs[job].started = Some((t, run));
                    let ends_at = t + self.jobs[job].duration;
                    let vm = &mut self.vms[index];
                    vm.slots[s].job = Some(job);
                    vm.slots[s].ends_at = ends_at;
                    vm.jobs_taken += 1;
                }
                None => self.vms[index].slots[s].next_poll = t + POLL_S,
            }
        }

        let (report, terminate) = {
            let slots = self.slots;
            let vm = &mut self.vms[index];
            if t >= vm.next_detect {
                vm.seen_busy = vm.busy();
                vm.next_detect = t + DETECT_S;
            }
            let uptime = t.saturating_sub(vm.created_at + KERNEL_S);
            let drain_time = uptime % HOUR >= vm::DRAIN_MINUTE * MIN;
            match vm.phase {
                Phase::Serving if vm.seen_busy == 0 && drain_time => vm.phase = Phase::Grace(t),
                Phase::Grace(_) if vm.seen_busy > 0 => vm.phase = Phase::Serving,
                Phase::Grace(since) if t - since >= vm::DRAIN_GRACE_S => {
                    for slot in &mut vm.slots {
                        slot.quit = true;
                        slot.exited = slot.job.is_none();
                    }
                    vm.phase = Phase::Quitting;
                }
                _ => {}
            }
            let state = match vm.phase {
                Phase::Serving if vm.seen_busy > 0 => VmState::Busy,
                Phase::Serving => VmState::Idle,
                _ => VmState::Draining,
            };
            let current = (state, slots - vm.seen_busy);
            let report = (vm.reported != Some(current) || t - vm.last_report >= vm::HEARTBEAT_S)
                .then(|| {
                    vm.reported = Some(current);
                    vm.last_report = t;
                    vm.reports.push((t, state));
                    VmReport {
                        state,
                        free: current.1,
                        slots,
                    }
                });
            let terminate = vm.phase == Phase::Quitting && vm.slots.iter().all(|slot| slot.exited);
            (report, terminate)
        };

        let headers = vm_headers(&self.vms[index]);
        if let Some(report) = report {
            let body = serde_json::to_vec(&report).unwrap();
            let response = self.request(vm::PATH_REPORT, &headers, &body).await;
            if response.status != 200 {
                self.violations.push(format!(
                    "report of {} at {t}: {}",
                    self.vms[index].run_id, response.status
                ));
            }
        }
        if terminate {
            let response = self.request(vm::PATH_TERMINATE, &headers, b"").await;
            if response.status == 200 {
                self.end(index, End::Terminated);
            }
        }
    }

    /// Every job started within `bound` seconds of going pending, except `exempt`; no VM ended
    /// while running a job; the provider lists exactly the VMs that did not end; no secret in
    /// the log.
    async fn check(&self, bound: u64, exempt: &[usize]) {
        for (index, job) in self.jobs.iter().enumerate() {
            let Some((started, _)) = &job.started else {
                panic!("job {index} never started: {job:?}");
            };
            let waited = started - job.pending_at;
            if !exempt.contains(&index) {
                assert!(
                    waited <= bound,
                    "job {index} waited {waited} s, bound {bound}: {job:?}"
                );
            }
        }
        assert!(self.violations.is_empty(), "{:?}", self.violations);
        let mut live: Vec<String> = self
            .provider
            .inner
            .capacity("vm")
            .await
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|v| v.run_id)
            .collect();
        live.sort();
        let mut expected: Vec<String> = self
            .vms
            .iter()
            .filter(|v| v.ended.is_none())
            .map(|v| v.run_id.clone())
            .collect();
        expected.sort();
        assert_eq!(live, expected);
        let lines = self.lines.lock().unwrap();
        assert!(
            !lines.iter().any(|l| l.contains(HOOK)
                || l.contains(VM_SECRET)
                || self.vms.iter().any(|v| l.contains(&v.token))),
            "a secret in the log"
        );
    }
}

fn gitlab_headers() -> Vec<(String, String)> {
    vec![("X-Gitlab-Token".to_string(), HOOK.to_string())]
}

fn vm_headers(vm: &Vm) -> Vec<(String, String)> {
    vec![
        (vm::HEADER_RUN.to_string(), vm.run_id.clone()),
        (vm::HEADER_TOKEN.to_string(), vm.token.clone()),
    ]
}

/// One job duration plus one boot; a slot asks for a job every `POLL_S`.
fn bound(duration: u64) -> u64 {
    duration + BOOT_S + POLL_S
}

/// Second of the first drain of a VM created at `created`.
fn drain_at(created: u64) -> u64 {
    created + KERNEL_S + vm::DRAIN_MINUTE * MIN
}

#[tokio::test]
async fn a_burst_on_an_empty_pool_starts_one_vm_per_job_up_to_max_runs() {
    let mut sim = Sim::new(1, 3).await;
    for _ in 0..4 {
        sim.pending(10 * MIN).await;
    }
    assert_eq!(
        sim.results(),
        ["started", "started", "started", "at_capacity"]
    );
    sim.run_to(2 * HOUR).await;
    sim.check(bound(10 * MIN), &[]).await;
    let waited = sim.jobs[3].started.as_ref().unwrap().0 - sim.jobs[3].pending_at;
    assert!(
        waited >= 10 * MIN,
        "the fourth job waits for a slot: {waited}"
    );
    assert_eq!(sim.vms.len(), 3);
    for vm in &sim.vms {
        let (ended, how) = vm.ended.unwrap();
        assert_eq!(how, End::Terminated);
        assert!(ended - vm.created_at < HOUR, "{vm:?}");
        assert_eq!(
            vm.states(),
            [VmState::Busy, VmState::Idle, VmState::Draining]
        );
    }
    assert_eq!(sim.vm_hours(), 3);

    let mut wide = Sim::new(2, 3).await;
    for _ in 0..5 {
        wide.pending(10 * MIN).await;
    }
    assert_eq!(
        wide.results(),
        [
            "started",
            "started",
            "started",
            "at_capacity",
            "at_capacity"
        ]
    );
    wide.run_to(2 * HOUR).await;
    wide.check(BOOT_S + POLL_S, &[]).await;
    assert_eq!(wide.vm_hours(), 3);
}

#[tokio::test]
async fn a_warm_vm_takes_new_jobs_within_its_paid_hour() {
    let mut sim = Sim::new(2, 2).await;
    let first = sim.pending(5 * MIN).await;
    let a = sim.run_of(first).to_string();
    sim.run_to(20 * MIN).await;
    let second = sim.pending(5 * MIN).await;
    let third = sim.pending(5 * MIN).await;
    assert_eq!(sim.results(), ["started", "claimed", "claimed"]);
    assert_eq!(sim.run_of(second), a);
    assert_eq!(sim.run_of(third), a);
    sim.run_to(30 * MIN).await;
    for job in [second, third] {
        let (started, run) = sim.jobs[job].started.clone().unwrap();
        assert_eq!(run, a);
        assert!(started - sim.jobs[job].pending_at <= POLL_S);
    }

    sim.run_to(70 * MIN).await;
    assert_eq!(sim.vm(&a).ended_by(), Some(End::Terminated));
    let late = sim.pending(5 * MIN).await;
    assert_eq!(sim.decisions[late].result, "started");
    sim.run_to(3 * HOUR).await;
    sim.check(bound(5 * MIN), &[]).await;
    assert_eq!(sim.vm_hours(), 2);
}

#[tokio::test]
async fn a_double_claim_leaves_one_job_waiting_for_the_next_free_slot() {
    let mut sim = Sim::new(1, 2).await;
    let first = sim.pending(5 * MIN).await;
    let a = sim.run_of(first).to_string();
    sim.run_to(10 * MIN).await;
    assert_eq!(sim.vm(&a).reported, Some((VmState::Idle, 1)));

    let (second, third) = sim.pending_together(10 * MIN).await;
    assert_eq!(sim.results(), ["started", "claimed", "claimed"]);
    assert_eq!(sim.run_of(second), a);
    assert_eq!(sim.run_of(third), a, "both calls saw the same free slot");

    sim.run_to(2 * HOUR).await;
    sim.check(bound(10 * MIN), &[]).await;
    let (started_second, _) = sim.jobs[second].started.clone().unwrap();
    let (started_third, on) = sim.jobs[third].started.clone().unwrap();
    assert_eq!(on, a);
    assert_eq!(
        started_third,
        started_second + 10 * MIN,
        "the second job runs when the slot frees"
    );
    assert_eq!(sim.vms.len(), 1);
    assert_eq!(sim.vm_hours(), 1);
}

#[tokio::test]
async fn jobs_pending_during_a_drain_are_served() {
    let mut sim = Sim::new(1, 2).await;
    let first = sim.pending(5 * MIN).await;
    let a = sim.run_of(first).to_string();
    let ia = sim.vm_index(&a);

    // Claimed while idle; the VM reports draining before its slot asks again and takes the job
    // in the grace.
    let drain = drain_at(0);
    sim.run_to(drain - 1).await;
    sim.vms[ia].slots[0].next_poll = drain + 2;
    sim.vms[ia].next_detect = drain;
    let claimed = sim.pending(10 * MIN).await;
    assert_eq!(sim.decisions[claimed].result, "claimed");
    sim.run_to(drain + 10).await;
    assert_eq!(sim.jobs[claimed].started, Some((drain + 2, a.clone())));
    let around: Vec<VmState> = sim.vms[ia]
        .reports
        .iter()
        .filter(|(t, _)| *t >= drain)
        .map(|(_, s)| *s)
        .collect();
    assert_eq!(around, [VmState::Draining, VmState::Busy]);

    // Pending after the draining report: the dispatcher starts a VM, the draining one takes the
    // job in its grace, the new VM idles for its paid hour.
    let drain = drain_at(HOUR);
    sim.run_to(drain).await;
    assert_eq!(sim.vms[ia].phase, Phase::Grace(drain));
    sim.vms[ia].slots[0].next_poll = drain + 2;
    let seen_draining = sim.pending(10 * MIN).await;
    assert_eq!(sim.decisions[seen_draining].result, "started");
    let b = sim.run_of(seen_draining).to_string();
    sim.run_to(drain + 10).await;
    assert_eq!(
        sim.jobs[seen_draining].started,
        Some((drain + 2, a.clone()))
    );

    // Pending one second before SIGQUIT and taken by the last request: the job runs to its end,
    // then the VM terminates. It outlasts the sweep's draining limit, so the 12600 s sweep sees
    // a VM draining for longer than that which still reports, and must keep it.
    let drain = drain_at(2 * HOUR);
    let quit = drain + vm::DRAIN_GRACE_S;
    sim.run_to(quit - 1).await;
    assert_eq!(sim.vm(&b).ended_by(), Some(End::Terminated));
    assert_eq!(sim.vms[ia].phase, Phase::Grace(drain));
    sim.vms[ia].slots[0].next_poll = quit;
    sim.vms[ia].next_detect = quit + 1;
    let last = sim.pending(40 * MIN).await;
    assert_eq!(sim.decisions[last].result, "started");
    let c = sim.run_of(last).to_string();
    sim.run_to(quit).await;
    assert_eq!(sim.jobs[last].started, Some((quit, a.clone())));
    assert_eq!(sim.vms[ia].phase, Phase::Quitting);
    let sweep = 7 * SWEEP_EVERY_S;
    assert!(sweep > drain + vm::SWEEP_DRAINING_S && sweep < quit + 40 * MIN);
    sim.run_to(4 * HOUR).await;
    assert_eq!(
        sim.vm(&a).ended,
        Some((quit + 40 * MIN, End::Terminated)),
        "ends when its last slot exits"
    );

    sim.check(bound(40 * MIN), &[]).await;
    for extra in [&b, &c] {
        let vm = sim.vm(extra);
        assert_eq!(vm.jobs_taken, 0);
        assert_eq!(vm.ended_by(), Some(End::Terminated));
        assert_eq!(vm.hours(sim.t()), 1, "one VM-hour per race");
    }
    assert_eq!(sim.vm(&a).hours(sim.t()), 4);
    assert_eq!(sim.vm_hours(), 6);
}

#[tokio::test]
async fn a_vm_booted_for_a_job_taken_elsewhere_idles_for_one_hour() {
    let mut sim = Sim::new(1, 2).await;
    let first = sim.pending(5 * MIN).await;
    let a = sim.run_of(first).to_string();
    sim.run_to(400).await;
    assert_eq!(sim.vm(&a).reported, Some((VmState::Busy, 0)));
    let second = sim.pending(10 * MIN).await;
    assert_eq!(sim.decisions[second].result, "started");
    let b = sim.run_of(second).to_string();
    sim.run_to(2 * HOUR).await;
    sim.check(bound(10 * MIN), &[]).await;
    assert_eq!(
        sim.jobs[second].started,
        Some((BOOT_S + 5 * MIN, a.clone())),
        "the busy VM's slot asks again when its job ends"
    );
    let extra = sim.vm(&b);
    assert_eq!(extra.jobs_taken, 0);
    assert_eq!(extra.states(), [VmState::Idle, VmState::Draining]);
    assert_eq!(extra.ended_by(), Some(End::Terminated));
    assert_eq!(extra.hours(sim.t()), 1);
    assert_eq!(sim.vm_hours(), 2);
}

#[tokio::test]
async fn silent_and_stuck_vms_are_swept_and_healthy_ones_are_not() {
    let mut sim = Sim::new(1, 3).await;
    let long = sim.pending(2 * HOUR).await;
    let a = sim.run_of(long).to_string();
    sim.next_never_boots();
    let orphan = sim.pending(5 * MIN).await;
    let stuck = sim.run_of(orphan).to_string();
    sim.run_to(10 * MIN).await;
    sim.hang(&a);
    let last_seen = sim.vm(&a).last_report;

    sim.run_to(15 * MIN).await;
    let third = sim.pending(5 * MIN).await;
    assert_eq!(sim.decisions[third].result, "started");
    let c = sim.run_of(third).to_string();

    sim.run_to(SWEEP_EVERY_S).await;
    assert_eq!(sim.vm(&stuck).ended, Some((SWEEP_EVERY_S, End::Swept)));
    assert_eq!(sim.vm(&a).ended, None, "seen within the silence limit");
    sim.run_to(2 * SWEEP_EVERY_S).await;
    let (swept_at, how) = sim.vm(&a).ended.unwrap();
    assert_eq!(how, End::Swept);
    assert!(swept_at <= last_seen + vm::SWEEP_SILENT_S + SWEEP_EVERY_S);

    sim.run_to(3 * HOUR).await;
    // The job whose VM never booted waits for the next pending event, which starts a VM that
    // takes the oldest job first.
    sim.check(bound(2 * HOUR), &[orphan]).await;
    assert_eq!(
        sim.jobs[orphan].started,
        Some((15 * MIN + BOOT_S, c.clone()))
    );
    assert!(sim.jobs[long].failed);
    assert_eq!(sim.vm(&c).ended_by(), Some(End::Terminated));
    assert_eq!(sim.vm_hours(), 3);
}

#[tokio::test]
async fn a_long_job_keeps_its_vm_through_every_sweep() {
    let mut sim = Sim::new(1, 1).await;
    let long = sim.pending(150 * MIN).await;
    let a = sim.run_of(long).to_string();
    sim.run_to(5000).await;
    let waiting = sim.pending(10 * MIN).await;
    assert_eq!(sim.decisions[waiting].result, "at_capacity");
    sim.run_to(5 * HOUR).await;
    sim.check(bound(150 * MIN), &[]).await;

    let vm = sim.vm(&a);
    let job_end = BOOT_S + 150 * MIN;
    assert_eq!(sim.jobs[waiting].started, Some((job_end, a.clone())));
    let (ended, how) = vm.ended.unwrap();
    assert_eq!(how, End::Terminated, "never swept");
    assert!(ended > job_end + 10 * MIN);
    let gaps = vm.reports.windows(2).map(|w| w[1].0 - w[0].0);
    assert!(
        gaps.clone().all(|gap| gap <= vm::HEARTBEAT_S),
        "{:?}",
        vm.reports
    );
    assert_eq!(vm.hours(sim.t()), 3);
}

#[tokio::test]
async fn the_sweep_needs_the_hook_token() {
    let sim = Sim::new(1, 1).await;
    for given in [json!("wrong"), json!(""), Value::Null] {
        let body = json!({ vm::SWEEP_ARG: given }).to_string().into_bytes();
        assert_eq!(sim.request("/", &[], &body).await.status, 401);
    }
}
