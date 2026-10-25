use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::time::Instant;

use crate::check::is_own_job;
use crate::config::{CacheType, Config, Pool};
use crate::ledger::{GROUP_WAIT_S, GROUP_WINDOW_S, Intent, Ledger};
use crate::log::Log;
use crate::provision::keys;
use crate::types::{Provider, Secrets, StartRequest, VmCapacity, VmReport, VmState};
use crate::vm;
use crate::{Error, Result};

/// M7 rule R4: the pause before the one retry of a start that failed with a retryable error.
pub const START_RETRY_PAUSE: Duration = Duration::from_millis(300);
/// M7 rule R4: a retry begins only this long after the request arrived, at the latest.
pub const START_RETRY_WITHIN: Duration = Duration::from_secs(5);

/// When a request arrived, on tokio's clock, which tests pause and advance (M7 rule R1).
#[derive(Debug, Clone, Copy)]
struct Arrival(Instant);

impl Arrival {
    fn now() -> Arrival {
        Arrival(Instant::now())
    }

    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }

    /// Whole milliseconds since the request arrived.
    fn ms(&self) -> u64 {
        u64::try_from(self.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Status and JSON body of an answer; a `Value::Null` body means no body at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DispatchResponse {
    pub status: u16,
    pub body: Value,
}

/// Environment a run receives on top of its pool definition. Secret values only. A VM run also
/// gets `CIBURST_VM_SECRET`, from which the provider derives the VM's own token; one-job runs
/// never see it, because their jobs could read it.
pub fn run_env(
    config: &Config,
    secrets: &Secrets,
    pool: &Pool,
    vm: bool,
) -> Result<BTreeMap<String, String>> {
    let token = secrets
        .runner_tokens
        .get(&pool.tag)
        .ok_or_else(|| Error::other(format!("no runner token for pool {}", pool.tag)))?;
    let mut env = BTreeMap::from([("RUNNER_TOKEN".to_string(), token.clone())]);
    if config.cache.kind == CacheType::S3 {
        let cache = secrets
            .cache
            .as_ref()
            .ok_or_else(|| Error::other("cache is s3 but no cache key is configured"))?;
        env.insert("CACHE_S3_ACCESS_KEY".to_string(), cache.access_key.clone());
        env.insert("CACHE_S3_SECRET_KEY".to_string(), cache.secret_key.clone());
    }
    if vm {
        let secret = secrets
            .vm_secret
            .as_ref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::other(format!("no VM secret for pool {}", pool.tag)))?;
        env.insert(keys::VM_SECRET.to_string(), secret.clone());
    }
    Ok(env)
}

/// Routes one request: GitLab job events, VM reports, VM termination and the sweep trigger.
/// Pure apart from the provider calls.
pub struct Dispatcher {
    config: Arc<Config>,
    provider: Arc<dyn Provider>,
    secrets: Arc<Secrets>,
    log: Log,
    dispatch_url: Option<String>,
    /// Burst intents (M4 rule R3); `None` keeps the M3 rule.
    ledger: Option<Arc<dyn Ledger>>,
    /// Pause between recording an intent and reading the burst.
    group_wait: Duration,
}

impl Dispatcher {
    pub fn new(
        config: Arc<Config>,
        provider: Arc<dyn Provider>,
        secrets: Arc<Secrets>,
        log: Log,
    ) -> Dispatcher {
        Dispatcher {
            config,
            provider,
            secrets,
            log,
            dispatch_url: None,
            ledger: None,
            group_wait: Duration::from_secs(GROUP_WAIT_S),
        }
    }

    /// The burst ledger (M4 rule R7).
    pub fn with_ledger(mut self, ledger: Option<Arc<dyn Ledger>>) -> Dispatcher {
        self.ledger = ledger;
        self
    }

    /// The pause between recording an intent and reading the burst; zero in tests.
    pub fn with_group_wait(mut self, wait: Duration) -> Dispatcher {
        self.group_wait = wait;
        self
    }

    /// Entry of every request. `POST /vm` and `POST /terminate` come from VMs with their
    /// capability token, a body with `ciburst_sweep` from the cron trigger, everything else is a
    /// GitLab webhook. `now` is unix seconds. Every answer with a body carries `ms`, the whole
    /// milliseconds since the request arrived (M7 rule R1).
    pub async fn handle_request(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
        now: u64,
    ) -> DispatchResponse {
        let at = Arrival::now();
        let response = self.route(method, path, headers, body, now, at).await;
        stamp(response, at)
    }

    async fn route(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
        now: u64,
        at: Arrival,
    ) -> DispatchResponse {
        if method.eq_ignore_ascii_case("POST") {
            let path = path.trim_end_matches('/');
            if path == vm::PATH_REPORT {
                return self.vm_report(headers, body, now, at).await;
            }
            if path == vm::PATH_TERMINATE {
                return self.vm_terminate(headers, at).await;
            }
        }
        if let Some(given) = sweep_arg(body) {
            return self.sweep(given.as_deref(), now, at).await;
        }
        self.webhook(headers, body, now, at).await
    }

    /// The GitLab webhook at the current time.
    /// The dispatcher's own base URL, which VMs call back on. Without it the URL is taken from
    /// the `Host` header of the authenticated webhook request.
    pub fn with_dispatch_url(mut self, url: Option<String>) -> Dispatcher {
        self.dispatch_url = url.map(|u| u.trim_end_matches('/').to_string());
        self
    }

    pub async fn handle(&self, headers: &[(String, String)], body: &[u8]) -> DispatchResponse {
        let at = Arrival::now();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        stamp(self.webhook(headers, body, now, at).await, at)
    }

    /// Log lines of a request end with `ms` (M7 rule R1).
    fn info(&self, at: Arrival, message: &str, fields: &[(&str, &str)]) {
        let ms = at.ms().to_string();
        self.log.info(message, &with_ms(fields, &ms));
    }

    fn warn(&self, at: Arrival, message: &str, fields: &[(&str, &str)]) {
        let ms = at.ms().to_string();
        self.log.warn(message, &with_ms(fields, &ms));
    }

    fn error(&self, at: Arrival, message: &str, fields: &[(&str, &str)]) {
        let ms = at.ms().to_string();
        self.log.error(message, &with_ms(fields, &ms));
    }

    async fn webhook(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        now: u64,
        at: Arrival,
    ) -> DispatchResponse {
        if !token_matches(
            header(headers, "x-gitlab-token"),
            self.secrets.hook_token.as_deref(),
        ) {
            return unauthorized();
        }
        let Ok(event) = serde_json::from_slice::<Value>(body) else {
            return reply(400, "bad_request", json!({}));
        };
        if event.get("object_kind").and_then(Value::as_str) != Some("build") {
            return ignored("not_job_event", json!({}));
        }
        let status = event
            .get("build_status")
            .and_then(Value::as_str)
            .unwrap_or("");
        if status != "pending" {
            return ignored("status", json!({ "status": status }));
        }
        let project_id = event.get("project_id").and_then(Value::as_u64).unwrap_or(0);
        if !self.config.gitlab.project_ids.contains(&project_id) {
            return ignored("project", json!({}));
        }
        let name = event
            .get("build_name")
            .and_then(Value::as_str)
            .unwrap_or("");
        if is_own_job(name) {
            return ignored("own_job", json!({ "name": name }));
        }

        let stage = event.get("build_stage").and_then(Value::as_str);
        let Some(pool) = self.config.pool_for(name, stage).cloned() else {
            return ignored("unmapped_stage", json!({ "stage": stage.unwrap_or("") }));
        };
        let labels = BTreeMap::from([
            ("project".to_string(), project_id.to_string()),
            ("pipeline".to_string(), number(&event, "pipeline_id")),
            ("job".to_string(), number(&event, "build_id")),
            ("stage".to_string(), stage.unwrap_or("").to_string()),
        ]);
        let mut fields: Vec<(&str, &str)> = labels
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if let Some(vm_pool) = self.config.vm_first_pool(&pool)
            && let Some((run_id, free)) = self
                .claim_for_lane(vm_pool, &pool, &labels["job"], now, at)
                .await
        {
            let free = free.to_string();
            fields.extend([
                ("pool", vm_pool.tag.as_str()),
                ("run", run_id.as_str()),
                ("free", free.as_str()),
                ("lane", pool.tag.as_str()),
            ]);
            self.info(at, "claimed", &fields);
            return reply(
                200,
                "claimed",
                json!({ "pool": vm_pool.tag, "lane": pool.tag, "runId": run_id }),
            );
        }
        fields.push(("pool", &pool.tag));
        let callback = self.dispatch_url.clone().or_else(|| {
            header(headers, "x-forwarded-host")
                .or_else(|| header(headers, "host"))
                .filter(|h| !h.is_empty())
                .map(|h| format!("https://{h}"))
        });
        match self
            .schedule(&pool, labels.clone(), now, callback.as_deref(), at)
            .await
        {
            Ok(Decision::Claimed { run_id, free }) => {
                let free = free.to_string();
                fields.push(("run", &run_id));
                fields.push(("free", &free));
                self.info(at, "claimed", &fields);
                reply(200, "claimed", json!({ "pool": pool.tag, "runId": run_id }))
            }
            Ok(Decision::AtCapacity(live)) => {
                let live = live.to_string();
                fields.push(("live", &live));
                self.info(at, "at_capacity", &fields);
                reply(200, "at_capacity", json!({ "pool": pool.tag }))
            }
            Ok(Decision::Started { run_id, taken }) => {
                let taken = taken.to_string();
                fields.push(("run", &run_id));
                fields.push(("taken", &taken));
                self.info(at, "started", &fields);
                reply(200, "started", json!({ "pool": pool.tag, "runId": run_id }))
            }
            Ok(Decision::Grouped { leader }) => {
                fields.push(("leader", &leader));
                self.info(at, "grouped", &fields);
                reply(
                    200,
                    "grouped",
                    json!({ "pool": pool.tag, "leader": leader }),
                )
            }
            Ok(Decision::Duplicate) => {
                self.info(at, "duplicate", &fields);
                reply(200, "duplicate", json!({ "pool": pool.tag }))
            }
            Err(error) => {
                let message = error.to_string();
                fields.push(("error", &message));
                self.error(at, "start_failed", &fields);
                reply(500, "error", json!({}))
            }
        }
    }

    /// Spec 6.1, scheduling, M4 rules R1 to R6: a free slot of a claim candidate is claimed;
    /// otherwise the job joins the burst ledger, or without one a VM starts unless `maxRuns`
    /// VMs are live (not draining). Providers without VMs answer `None` from `capacity` and
    /// count live runs.
    async fn schedule(
        &self,
        pool: &Pool,
        labels: BTreeMap<String, String>,
        now: u64,
        callback: Option<&str>,
        at: Arrival,
    ) -> Result<Decision> {
        let Some(vms) = self.provider.capacity(&pool.tag).await? else {
            let live = self
                .provider
                .list(Some(&pool.tag))
                .await?
                .into_iter()
                .filter(|run| run.state.is_live())
                .count();
            if live >= pool.max_runs as usize {
                return Ok(Decision::AtCapacity(live));
            }
            return self.start(pool, labels, false, None, 1, at).await;
        };
        let job = labels.get("job").cloned().unwrap_or_default();
        if let Some((target, free)) = claim_target(&vms, &job, now) {
            match self.provider.claim(&target.run_id).await {
                Ok(()) => {
                    return Ok(Decision::Claimed {
                        run_id: target.run_id.clone(),
                        free,
                    });
                }
                Err(error) => {
                    // A failed claim must not leave the job unserved; a new VM costs one hour.
                    let message = error.to_string();
                    self.warn(
                        at,
                        "claim_failed",
                        &[("run", &target.run_id), ("error", &message)],
                    );
                    return self.start(pool, labels, true, callback, 1, at).await;
                }
            }
        }
        if let Some(ledger) = self.ledger.as_deref().filter(|_| !job.is_empty()) {
            match self.burst(ledger, pool, &job, now).await {
                Ok(Burst::Duplicate) => return Ok(Decision::Duplicate),
                Ok(Burst::Follower { leader }) => return Ok(Decision::Grouped { leader }),
                Ok(Burst::Leader { group, taken }) => {
                    let cutoff = now.saturating_sub(GROUP_WINDOW_S);
                    let live_old = vms
                        .iter()
                        .filter(|v| v.state != VmState::Draining && v.created_at < cutoff)
                        .count();
                    let live = live_old + group;
                    if live >= pool.max_runs as usize {
                        return Ok(Decision::AtCapacity(live));
                    }
                    return self.start(pool, labels, true, callback, taken, at).await;
                }
                Err(error) => {
                    let message = error.to_string();
                    self.warn(
                        at,
                        "ledger_failed",
                        &[("pool", &pool.tag), ("job", &job), ("error", &message)],
                    );
                }
            }
        }
        let live = vms.iter().filter(|v| v.state != VmState::Draining).count();
        if live >= pool.max_runs as usize {
            return Ok(Decision::AtCapacity(live));
        }
        self.start(pool, labels, true, callback, 1, at).await
    }

    /// D47: a pending job of `lane` claims a free slot of `vm_pool` when one of its VMs is a
    /// claim candidate (M4 rules R1, R2); `None` leaves it to the lane. A failed read or claim
    /// logs `claim_failed` and also leaves it to the lane, which never uses the ledger.
    async fn claim_for_lane(
        &self,
        vm_pool: &Pool,
        lane: &Pool,
        job: &str,
        now: u64,
        at: Arrival,
    ) -> Option<(String, u64)> {
        let vms = match self.provider.capacity(&vm_pool.tag).await {
            Ok(vms) => vms?,
            Err(error) => {
                let message = error.to_string();
                self.warn(
                    at,
                    "claim_failed",
                    &[
                        ("pool", &vm_pool.tag),
                        ("lane", &lane.tag),
                        ("error", &message),
                    ],
                );
                return None;
            }
        };
        let (target, free) = claim_target(&vms, job, now)?;
        match self.provider.claim(&target.run_id).await {
            Ok(()) => Some((target.run_id.clone(), free)),
            Err(error) => {
                let message = error.to_string();
                self.warn(
                    at,
                    "claim_failed",
                    &[
                        ("run", &target.run_id),
                        ("lane", &lane.tag),
                        ("error", &message),
                    ],
                );
                None
            }
        }
    }

    /// M4 rule R3: records the intent, waits for the rest of the burst and ranks the intents of
    /// the window. An error means the ledger failed (R4).
    async fn burst(&self, ledger: &dyn Ledger, pool: &Pool, job: &str, now: u64) -> Result<Burst> {
        ledger.record(&pool.tag, now, job).await?;
        if !self.group_wait.is_zero() {
            tokio::time::sleep(self.group_wait).await;
        }
        let intents = ledger
            .since(&pool.tag, now.saturating_sub(GROUP_WINDOW_S))
            .await?;
        rank_burst(&intents, job, now, pool.slots)
            .ok_or_else(|| Error::other("the ledger lacks the intent just recorded"))
    }

    /// Starts a run; `taken` slots of a new VM are already assigned (M4 rule R6). A retryable
    /// failure is retried once after [`START_RETRY_PAUSE`] when the retry begins within
    /// [`START_RETRY_WITHIN`] of the request's arrival (M7 rule R4).
    async fn start(
        &self,
        pool: &Pool,
        labels: BTreeMap<String, String>,
        vm: bool,
        callback: Option<&str>,
        taken: u32,
        at: Arrival,
    ) -> Result<Decision> {
        let mut env = run_env(&self.config, &self.secrets, pool, vm)?;
        if vm && let Some(url) = callback {
            env.insert("CIBURST_DISPATCH_URL".to_string(), url.to_string());
        }
        let job = labels.get("job").cloned().unwrap_or_default();
        let request = StartRequest {
            pool: pool.clone(),
            env,
            labels,
            taken,
        };
        let run_id = match self.provider.start(request.clone()).await {
            Ok(run_id) => run_id,
            Err(error)
                if error.is_retryable()
                    && at.elapsed() + START_RETRY_PAUSE < START_RETRY_WITHIN =>
            {
                let message = error.to_string();
                self.warn(
                    at,
                    "start_retry",
                    &[("pool", &pool.tag), ("job", &job), ("error", &message)],
                );
                tokio::time::sleep(START_RETRY_PAUSE).await;
                self.provider.start(request).await?
            }
            Err(error) => return Err(error),
        };
        Ok(Decision::Started { run_id, taken })
    }

    /// The run id of a request that carries a valid capability token.
    fn vm_caller<'a>(&self, headers: &'a [(String, String)]) -> Option<&'a str> {
        let run = header(headers, vm::HEADER_RUN)?;
        let token = header(headers, vm::HEADER_TOKEN)?;
        let secret = self.secrets.vm_secret.as_deref()?;
        vm::verify(secret, run, token).then_some(run)
    }

    /// A refused VM or cron request. The run id is the caller's claim, shortened because the
    /// header is unauthenticated input.
    fn refuse(&self, route: &str, headers: &[(String, String)], at: Arrival) -> DispatchResponse {
        let run: String = header(headers, vm::HEADER_RUN)
            .unwrap_or("")
            .chars()
            .take(64)
            .collect();
        self.warn(at, "unauthorized", &[("route", route), ("run", &run)]);
        unauthorized()
    }

    async fn vm_report(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        now: u64,
        at: Arrival,
    ) -> DispatchResponse {
        let Some(run) = self.vm_caller(headers) else {
            return self.refuse(vm::PATH_REPORT, headers, at);
        };
        let Ok(report) = serde_json::from_slice::<VmReport>(body) else {
            return reply(400, "bad_request", json!({}));
        };
        match self.provider.report(run, &report, now).await {
            Ok(()) => {
                let free = report.free.to_string();
                self.info(
                    at,
                    "reported",
                    &[
                        ("run", run),
                        ("state", report.state.as_str()),
                        ("free", &free),
                    ],
                );
                reply(200, "reported", json!({}))
            }
            Err(error) => {
                let message = error.to_string();
                self.error(at, "report_failed", &[("run", run), ("error", &message)]);
                reply(500, "error", json!({}))
            }
        }
    }

    async fn vm_terminate(&self, headers: &[(String, String)], at: Arrival) -> DispatchResponse {
        let Some(run) = self.vm_caller(headers) else {
            return self.refuse(vm::PATH_TERMINATE, headers, at);
        };
        match self.provider.stop(run).await {
            Ok(()) => {
                self.info(at, "terminated", &[("run", run)]);
                reply(200, "terminated", json!({}))
            }
            Err(error) => {
                let message = error.to_string();
                self.error(at, "terminate_failed", &[("run", run), ("error", &message)]);
                reply(500, "error", json!({}))
            }
        }
    }

    async fn sweep(&self, given: Option<&str>, now: u64, at: Arrival) -> DispatchResponse {
        if !token_matches(given, self.secrets.sweep_token.as_deref()) {
            self.warn(at, "unauthorized", &[("route", "sweep")]);
            return unauthorized();
        }
        match self.provider.sweep(now).await {
            Ok(runs) => {
                let count = runs.len().to_string();
                let joined = runs.join(",");
                self.info(at, "swept", &[("count", &count), ("runs", &joined)]);
                reply(200, "swept", json!({ "runs": runs }))
            }
            Err(error) => {
                let message = error.to_string();
                self.error(at, "sweep_failed", &[("error", &message)]);
                reply(500, "error", json!({}))
            }
        }
    }
}

enum Decision {
    Claimed {
        run_id: String,
        free: u64,
    },
    AtCapacity(usize),
    Started {
        run_id: String,
        taken: u32,
    },
    /// Rides on the VM its burst leader starts (M4 rule R3).
    Grouped {
        leader: String,
    },
    /// The job's intent was already recorded by an earlier event (M4 rule R3).
    Duplicate,
}

/// Seconds after its last report that a VM still counts as capacity: two heartbeats and a
/// minute, so one lost report does not matter but a hung VM stops absorbing claims.
pub const CAPACITY_FRESH_S: u64 = 2 * vm::HEARTBEAT_S + 60;

/// M4 rule R1: a VM takes a claim when it has a free slot, was created more than
/// [`GROUP_WINDOW_S`] ago (younger VMs are accounted for by the burst ledger), and is `idle` or
/// `busy` with a report within [`CAPACITY_FRESH_S`], or `starting` and younger than
/// [`vm::SWEEP_STARTING_S`]. `draining` VMs stop soon and silent ones may hang.
fn is_candidate(vm: &VmCapacity, now: u64) -> bool {
    let age = now.saturating_sub(vm.created_at);
    if vm.free == 0 || age <= GROUP_WINDOW_S {
        return false;
    }
    match vm.state {
        VmState::Idle | VmState::Busy => vm
            .seen
            .is_some_and(|seen| now.saturating_sub(seen) <= CAPACITY_FRESH_S),
        VmState::Starting => age < vm::SWEEP_STARTING_S,
        VmState::Draining => false,
    }
}

fn claim_candidates(vms: &[VmCapacity], now: u64) -> Vec<&VmCapacity> {
    vms.iter().filter(|v| is_candidate(v, now)).collect()
}

/// The VM `job` claims (M4 rules R1, R2) and the free slots of every candidate.
fn claim_target<'a>(vms: &'a [VmCapacity], job: &str, now: u64) -> Option<(&'a VmCapacity, u64)> {
    let candidates = claim_candidates(vms, now);
    let target = spread(&candidates, job)?;
    Some((target, candidates.iter().map(|v| u64::from(v.free)).sum()))
}

/// M4 rule R2: among the candidates with the most free slots, sorted by run id, the one at
/// index `fnv1a64(job) % count`, so parallel claims of one burst spread over the VMs.
fn spread<'a>(candidates: &[&'a VmCapacity], job: &str) -> Option<&'a VmCapacity> {
    let most = candidates.iter().map(|v| v.free).max()?;
    let mut top: Vec<&VmCapacity> = candidates
        .iter()
        .copied()
        .filter(|v| v.free == most)
        .collect();
    top.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    let index = fnv1a64(job.as_bytes()) % top.len() as u64;
    top.get(index as usize).copied()
}

/// FNV-1a, 64 bit.
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// A job id in the burst order: a number when it parses as one, so `42` and a zero-padded
/// `00000000000000000042` read back from a ledger are the same job. Numbers order before
/// other ids, which keeps the order total when ids of both kinds meet.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum JobKey {
    Number(u64),
    Text(String),
}

impl JobKey {
    fn parse(job: &str) -> JobKey {
        job.parse()
            .map(JobKey::Number)
            .unwrap_or_else(|_| JobKey::Text(job.to_string()))
    }
}

impl fmt::Display for JobKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobKey::Number(n) => write!(f, "{n}"),
            JobKey::Text(s) => f.write_str(s),
        }
    }
}

/// A job's place in its burst (M4 rule R3).
#[derive(Debug, PartialEq, Eq)]
enum Burst {
    /// The job's earliest intent is older than this event.
    Duplicate,
    /// Rank `r % slots != 0`: the job rides on the VM of the job at rank `r - r % slots`.
    Follower { leader: String },
    /// Rank `r % slots == 0`: group `r / slots` starts a VM with `taken` slots assigned.
    Leader { group: usize, taken: u32 },
}

/// Ranks `job` among the intents of the window `[now - GROUP_WINDOW_S, ..]`: each job's
/// earliest intent, ordered by time, then job id. `None` when the job has no intent there.
fn rank_burst(intents: &[Intent], job: &str, now: u64, slots: u32) -> Option<Burst> {
    let since = now.saturating_sub(GROUP_WINDOW_S);
    let mut earliest: BTreeMap<JobKey, u64> = BTreeMap::new();
    for intent in intents.iter().filter(|i| i.at >= since) {
        let at = earliest
            .entry(JobKey::parse(&intent.job))
            .or_insert(intent.at);
        *at = (*at).min(intent.at);
    }
    let mine = JobKey::parse(job);
    if *earliest.get(&mine)? < now {
        return Some(Burst::Duplicate);
    }
    let mut order: Vec<(u64, JobKey)> = earliest.into_iter().map(|(job, at)| (at, job)).collect();
    order.sort();
    let rank = order.iter().position(|(_, job)| *job == mine)?;
    let slots = slots.max(1) as usize;
    let leader = rank - rank % slots;
    if leader != rank {
        return Some(Burst::Follower {
            leader: order[leader].1.to_string(),
        });
    }
    let taken = (order.len() - rank).min(slots);
    Some(Burst::Leader {
        group: rank / slots,
        taken: u32::try_from(taken).unwrap_or(u32::MAX),
    })
}

/// `Some` when the body is a JSON object with the sweep key; the inner value is the given
/// token when it is a string.
fn sweep_arg(body: &[u8]) -> Option<Option<String>> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let given = value.as_object()?.get(vm::SWEEP_ARG)?;
    Some(given.as_str().map(String::from))
}

fn unauthorized() -> DispatchResponse {
    DispatchResponse {
        status: 401,
        body: Value::Null,
    }
}

/// M7 rule R1: a body gets `ms`, the whole milliseconds since the request arrived.
fn stamp(mut response: DispatchResponse, at: Arrival) -> DispatchResponse {
    if let Value::Object(map) = &mut response.body {
        map.insert("ms".to_string(), Value::from(at.ms()));
    }
    response
}

fn with_ms<'a>(fields: &[(&'a str, &'a str)], ms: &'a str) -> Vec<(&'a str, &'a str)> {
    let mut all = fields.to_vec();
    all.push(("ms", ms));
    all
}

fn reply(status: u16, result: &str, mut extra: Value) -> DispatchResponse {
    if let Value::Object(map) = &mut extra {
        map.insert("result".to_string(), Value::String(result.to_string()));
    }
    DispatchResponse {
        status,
        body: extra,
    }
}

fn ignored(reason: &str, mut extra: Value) -> DispatchResponse {
    if let Value::Object(map) = &mut extra {
        map.insert("reason".to_string(), Value::String(reason.to_string()));
    }
    reply(200, "ignored", extra)
}

fn number(event: &Value, key: &str) -> String {
    event
        .get(key)
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn token_matches(given: Option<&str>, expected: Option<&str>) -> bool {
    match (given, expected) {
        (Some(given), Some(expected)) if !expected.is_empty() => {
            given.len() == expected.len() && bool::from(given.as_bytes().ct_eq(expected.as_bytes()))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;
    use crate::fake::FakeProvider;
    use crate::ledger::MemoryLedger;
    use crate::store::MemoryStore;
    use crate::types::{KeyPair, Run};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn config() -> Arc<Config> {
        Arc::new(
            parse_config(
                r#"{
                  "name": "demo",
                  "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
                  "host": { "type": "fake" }, "provider": { "type": "fake" },
                  "defaultPool": "ci",
                  "pools": [
                    { "tag": "ci", "image": "img/node", "stages": ["test"] },
                    { "tag": "build", "image": "img/kaniko", "stages": ["build"], "maxRuns": 1 }
                  ],
                  "cache": { "type": "s3" }
                }"#,
            )
            .unwrap(),
        )
    }

    fn secrets() -> Arc<Secrets> {
        Arc::new(Secrets {
            hook_token: Some("hook-secret".into()),
            runner_tokens: BTreeMap::from([
                ("ci".to_string(), "glrt-ci".to_string()),
                ("build".to_string(), "glrt-build".to_string()),
            ]),
            dispatch: None,
            cache: Some(KeyPair {
                access_key: "AK".into(),
                secret_key: "SK".into(),
            }),
            vm_secret: None,
            sweep_token: None,
            provider_env: BTreeMap::new(),
            dispatcher_env: BTreeMap::new(),
        })
    }

    fn event(stage: &str, status: &str, project: u64, job: u64) -> Vec<u8> {
        json!({
            "object_kind": "build", "build_id": job, "build_stage": stage,
            "build_status": status, "pipeline_id": 9, "project_id": project
        })
        .to_string()
        .into_bytes()
    }

    fn headers(token: &str) -> Vec<(String, String)> {
        vec![
            ("X-Gitlab-Token".to_string(), token.to_string()),
            ("X-Gitlab-Event".to_string(), "Job Hook".to_string()),
        ]
    }

    struct Recording {
        inner: Arc<FakeProvider>,
        started: Mutex<Vec<StartRequest>>,
        fail: bool,
        /// Claims fail with a retryable error, as a 503 of the tag update would.
        fail_claim: std::sync::atomic::AtomicBool,
        /// Pools asked for capacity and runs claimed, in order.
        asked: Mutex<Vec<String>>,
        claimed: Mutex<Vec<String>>,
        /// Errors of the next start calls, one per call.
        start_errors: Mutex<VecDeque<Error>>,
        start_calls: AtomicU64,
        claim_calls: AtomicU64,
        /// How long every provider call of a request takes, and a start on top of it.
        delay: Mutex<Duration>,
        start_delay: Mutex<Duration>,
    }

    impl Recording {
        fn new(inner: Arc<FakeProvider>, fail: bool) -> Recording {
            Recording {
                inner,
                started: Mutex::new(Vec::new()),
                fail,
                fail_claim: Default::default(),
                asked: Mutex::new(Vec::new()),
                claimed: Mutex::new(Vec::new()),
                start_errors: Mutex::new(VecDeque::new()),
                start_calls: AtomicU64::new(0),
                claim_calls: AtomicU64::new(0),
                delay: Mutex::new(Duration::ZERO),
                start_delay: Mutex::new(Duration::ZERO),
            }
        }

        fn fail_starts(&self, errors: impl IntoIterator<Item = Error>) {
            self.start_errors.lock().unwrap().extend(errors);
        }

        fn start_calls(&self) -> u64 {
            self.start_calls.load(Ordering::SeqCst)
        }

        async fn pause(&self) {
            let delay = *self.delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
    }

    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[async_trait]
    impl Provider for Recording {
        fn capabilities(&self) -> crate::types::Capabilities {
            self.inner.capabilities()
        }
        async fn plan(&self, rotate: &[crate::types::Rotation]) -> Result<Vec<crate::types::Step>> {
            self.inner.plan(rotate).await
        }
        async fn apply(&self, steps: &[crate::types::Step]) -> Result<crate::types::Outputs> {
            self.inner.apply(steps).await
        }
        async fn destroy(&self) -> Result<()> {
            self.inner.destroy().await
        }
        async fn start(&self, request: StartRequest) -> Result<String> {
            self.start_calls.fetch_add(1, Ordering::SeqCst);
            self.pause().await;
            let start_delay = *self.start_delay.lock().unwrap();
            if !start_delay.is_zero() {
                tokio::time::sleep(start_delay).await;
            }
            if self.fail {
                return Err(Error::provider("quota"));
            }
            if let Some(error) = self.start_errors.lock().unwrap().pop_front() {
                return Err(error);
            }
            self.started.lock().unwrap().push(request.clone());
            self.inner.start(request).await
        }
        async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
            self.pause().await;
            self.inner.list(pool).await
        }
        async fn stop(&self, run_id: &str) -> Result<()> {
            self.pause().await;
            self.inner.stop(run_id).await
        }
        async fn capacity(&self, pool: &str) -> Result<Option<Vec<VmCapacity>>> {
            self.pause().await;
            self.asked.lock().unwrap().push(pool.to_string());
            self.inner.capacity(pool).await
        }
        async fn claim(&self, run_id: &str) -> Result<()> {
            self.claim_calls.fetch_add(1, Ordering::SeqCst);
            self.pause().await;
            if self.fail_claim.load(Ordering::SeqCst) {
                return Err(Error::retryable("tag update: 503"));
            }
            self.claimed.lock().unwrap().push(run_id.to_string());
            self.inner.claim(run_id).await
        }
        async fn report(&self, run_id: &str, report: &VmReport, now: u64) -> Result<()> {
            self.pause().await;
            self.inner.report(run_id, report, now).await
        }
        async fn sweep(&self, now: u64) -> Result<Vec<String>> {
            self.pause().await;
            self.inner.sweep(now).await
        }
    }

    async fn setup(fail: bool) -> (Dispatcher, Arc<Recording>, Arc<Mutex<Vec<String>>>) {
        let config = config();
        let inner = Arc::new(FakeProvider::new(
            config.clone(),
            Arc::new(MemoryStore::new()),
        ));
        let plan = inner.plan(&[]).await.unwrap();
        inner.apply(&plan).await.unwrap();
        let recording = Arc::new(Recording::new(inner, fail));
        let (log, lines) = Log::memory();
        let dispatcher = Dispatcher::new(config, recording.clone(), secrets(), log);
        (dispatcher, recording, lines)
    }

    #[tokio::test(start_paused = true)]
    async fn rejects_bad_tokens() {
        let (d, _, _) = setup(false).await;
        assert_eq!(d.handle(&[], b"{}").await.status, 401);
        assert_eq!(
            d.handle(&headers("nope"), &event("test", "pending", 7, 1))
                .await
                .status,
            401
        );
        assert_eq!(
            d.handle(&headers("hook-secret-longer"), b"{}").await.status,
            401
        );
        let empty = Dispatcher::new(
            config(),
            Arc::new(FakeProvider::new(config(), Arc::new(MemoryStore::new()))),
            Arc::new(Secrets::default()),
            Log::silent(),
        );
        assert_eq!(empty.handle(&headers(""), b"{}").await.status, 401);
    }

    #[tokio::test(start_paused = true)]
    async fn answers_400_for_bad_json() {
        let (d, _, _) = setup(false).await;
        let response = d.handle(&headers("hook-secret"), b"{not json").await;
        assert_eq!(response.status, 400);
        assert_eq!(response.body, json!({ "result": "bad_request", "ms": 0 }));
    }

    #[tokio::test(start_paused = true)]
    async fn ignores_other_events() {
        let (d, recording, _) = setup(false).await;
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (b"{\"object_kind\":\"pipeline\"}".to_vec(), "not_job_event"),
            (b"null".to_vec(), "not_job_event"),
            (event("test", "running", 7, 1), "status"),
            (event("test", "pending", 8, 1), "project"),
        ];
        for (body, reason) in cases {
            let response = d.handle(&headers("hook-secret"), &body).await;
            assert_eq!(response.status, 200);
            assert_eq!(response.body["result"], "ignored");
            assert_eq!(response.body["reason"], reason);
        }
        assert!(recording.started.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn ignores_the_components_own_jobs() {
        let (d, recording, _) = setup(false).await;
        let named = |name: &str, project: u64| {
            let mut event: Value =
                serde_json::from_slice(&event("build", "pending", project, 5)).unwrap();
            event["build_name"] = name.into();
            event.to_string().into_bytes()
        };
        let response = d
            .handle(&headers("hook-secret"), &named("ciburst-setup", 7))
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body["result"], "ignored");
        assert_eq!(response.body["reason"], "own_job");
        assert_eq!(response.body["name"], "ciburst-setup");
        let other = d
            .handle(&headers("hook-secret"), &named("ciburst-check", 8))
            .await;
        assert_eq!(
            other.body["reason"], "project",
            "the project check comes first"
        );
        assert!(recording.started.lock().unwrap().is_empty());

        let response = d
            .handle(&headers("hook-secret"), &named("ciburst", 7))
            .await;
        assert_eq!(response.body["result"], "started");
    }

    #[tokio::test(start_paused = true)]
    async fn starts_in_the_pool_of_the_stage() {
        let (d, recording, lines) = setup(false).await;
        let response = d
            .handle(&headers("hook-secret"), &event("build", "pending", 7, 42))
            .await;
        assert_eq!(response.status, 200, "{}", response.body);
        assert_eq!(response.body["result"], "started");
        assert_eq!(response.body["pool"], "build");
        assert!(
            response.body["runId"]
                .as_str()
                .unwrap()
                .starts_with("fake-")
        );
        let started = recording.started.lock().unwrap();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].pool.tag, "build");
        assert_eq!(started[0].env["RUNNER_TOKEN"], "glrt-build");
        assert_eq!(started[0].env["CACHE_S3_ACCESS_KEY"], "AK");
        assert_eq!(started[0].env["CACHE_S3_SECRET_KEY"], "SK");
        assert_eq!(started[0].labels["job"], "42");
        assert_eq!(started[0].labels["pipeline"], "9");
        assert_eq!(started[0].labels["stage"], "build");
        let lines = lines.lock().unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("info started") && l.contains("pool=\"build\""))
        );
        assert!(!lines.iter().any(|l| l.contains("glrt-")));
    }

    #[tokio::test(start_paused = true)]
    async fn default_pool_and_case_insensitive_headers() {
        let (d, _, _) = setup(false).await;
        let headers = vec![("x-gitlab-token".to_string(), "hook-secret".to_string())];
        let response = d.handle(&headers, &event("deploy", "pending", 7, 1)).await;
        assert_eq!(response.body["result"], "started");
        assert_eq!(response.body["pool"], "ci");
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_default_pool_unmapped_stages_start_nothing() {
        let (d, recording, _) = setup(false).await;
        let mut config = (*d.config).clone();
        config.default_pool = None;
        let d = Dispatcher::new(
            Arc::new(config),
            d.provider.clone(),
            d.secrets.clone(),
            Log::silent(),
        );
        let response = d
            .handle(&headers("hook-secret"), &event("deploy", "pending", 7, 1))
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body["result"], "ignored");
        assert_eq!(response.body["reason"], "unmapped_stage");
        assert_eq!(response.body["stage"], "deploy");
        assert!(recording.inner.list(None).await.unwrap().is_empty());
        let mapped = d
            .handle(&headers("hook-secret"), &event("build", "pending", 7, 2))
            .await;
        assert_eq!(mapped.body["result"], "started");
    }

    #[tokio::test(start_paused = true)]
    async fn at_capacity_until_a_run_ends() {
        let (d, recording, _) = setup(false).await;
        let first = d
            .handle(&headers("hook-secret"), &event("build", "pending", 7, 1))
            .await;
        assert_eq!(first.body["result"], "started");
        let second = d
            .handle(&headers("hook-secret"), &event("build", "pending", 7, 2))
            .await;
        assert_eq!(second.body["result"], "at_capacity");
        assert_eq!(second.body["pool"], "build");
        recording
            .inner
            .stop(first.body["runId"].as_str().unwrap())
            .await
            .unwrap();
        let third = d
            .handle(&headers("hook-secret"), &event("build", "pending", 7, 3))
            .await;
        assert_eq!(third.body["result"], "started");
    }

    #[tokio::test(start_paused = true)]
    async fn answers_500_when_the_provider_fails() {
        let (d, _, lines) = setup(true).await;
        let response = d
            .handle(&headers("hook-secret"), &event("test", "pending", 7, 1))
            .await;
        assert_eq!(response.status, 500);
        assert_eq!(response.body["result"], "error");
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.starts_with("error start_failed") && l.contains("quota"))
        );
    }

    #[test]
    fn run_env_requires_tokens() {
        let config = config();
        let pool = config.pools[0].clone();
        assert!(run_env(&config, &Secrets::default(), &pool, false).is_err());
        let mut partial = Secrets::default();
        partial.runner_tokens.insert("ci".into(), "t".into());
        assert!(
            run_env(&config, &partial, &pool, false)
                .unwrap_err()
                .to_string()
                .contains("no cache key")
        );
        let mut no_cache = (*config).clone();
        no_cache.cache.kind = CacheType::None;
        assert_eq!(
            run_env(&no_cache, &partial, &pool, false).unwrap()["RUNNER_TOKEN"],
            "t"
        );
        assert!(
            run_env(&no_cache, &partial, &pool, true)
                .unwrap_err()
                .to_string()
                .contains("no VM secret for pool ci")
        );
        partial.vm_secret = Some("vm-secret".into());
        let one_job = run_env(&no_cache, &partial, &pool, false).unwrap();
        assert!(!one_job.contains_key(keys::VM_SECRET), "{one_job:?}");
        assert_eq!(
            run_env(&no_cache, &partial, &pool, true).unwrap()[keys::VM_SECRET],
            "vm-secret"
        );
    }

    const NOW: u64 = 1_800_000_000;

    fn capacity(
        id: &str,
        state: VmState,
        free: u32,
        seen: Option<u64>,
        created_at: u64,
    ) -> VmCapacity {
        VmCapacity {
            run_id: id.into(),
            state,
            free,
            seen,
            created_at,
        }
    }

    #[test]
    fn claim_candidates_follow_r1_and_its_boundaries() {
        let old = NOW - GROUP_WINDOW_S - 1;
        let fresh = Some(NOW - CAPACITY_FRESH_S);
        let cases = [
            (capacity("idle", VmState::Idle, 1, fresh, old), true),
            (capacity("busy", VmState::Busy, 1, fresh, old), true),
            (capacity("full", VmState::Busy, 0, fresh, old), false),
            (
                capacity(
                    "stale",
                    VmState::Idle,
                    2,
                    Some(NOW - CAPACITY_FRESH_S - 1),
                    old,
                ),
                false,
            ),
            (capacity("unseen", VmState::Idle, 2, None, old), false),
            (
                capacity("draining", VmState::Draining, 2, fresh, old),
                false,
            ),
            (
                capacity(
                    "in-window",
                    VmState::Idle,
                    2,
                    Some(NOW),
                    NOW - GROUP_WINDOW_S,
                ),
                false,
            ),
            (
                capacity("created-now", VmState::Idle, 2, Some(NOW), NOW),
                false,
            ),
            (
                capacity("future", VmState::Idle, 2, Some(NOW), NOW + 5),
                false,
            ),
            (capacity("starting", VmState::Starting, 1, None, old), true),
            (
                capacity(
                    "starting-young",
                    VmState::Starting,
                    1,
                    None,
                    NOW - GROUP_WINDOW_S,
                ),
                false,
            ),
            (
                capacity(
                    "starting-599",
                    VmState::Starting,
                    1,
                    None,
                    NOW - vm::SWEEP_STARTING_S + 1,
                ),
                true,
            ),
            (
                capacity(
                    "starting-600",
                    VmState::Starting,
                    1,
                    None,
                    NOW - vm::SWEEP_STARTING_S,
                ),
                false,
            ),
            (
                capacity("starting-full", VmState::Starting, 0, None, old),
                false,
            ),
        ];
        for (vm, expected) in &cases {
            assert_eq!(is_candidate(vm, NOW), *expected, "{vm:?}");
        }
        let vms: Vec<VmCapacity> = cases.iter().map(|(vm, _)| vm.clone()).collect();
        let ids: Vec<&str> = claim_candidates(&vms, NOW)
            .iter()
            .map(|v| v.run_id.as_str())
            .collect();
        assert_eq!(ids, ["idle", "busy", "starting", "starting-599"]);
        assert_eq!(GROUP_WINDOW_S, 30);
    }

    #[test]
    fn fnv1a64_matches_the_reference_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn spread_picks_by_job_among_the_vms_with_most_free_slots() {
        let vm = |id: &str, free: u32| capacity(id, VmState::Idle, free, Some(NOW), 0);
        let pick = |vms: &[VmCapacity], job: &str| {
            let refs: Vec<&VmCapacity> = vms.iter().collect();
            spread(&refs, job).map(|v| v.run_id.clone())
        };
        assert_eq!(pick(&[], "1"), None);
        assert_eq!(
            pick(&[vm("a", 1), vm("b", 2), vm("c", 1)], "7").as_deref(),
            Some("b")
        );

        let top = [vm("c", 2), vm("a", 2), vm("low", 1), vm("b", 2)];
        let reversed: Vec<VmCapacity> = top.iter().rev().cloned().collect();
        for job in 0..50 {
            let job = job.to_string();
            let expected = ["a", "b", "c"][(fnv1a64(job.as_bytes()) % 3) as usize];
            assert_eq!(pick(&top, &job).as_deref(), Some(expected), "job {job}");
            assert_eq!(
                pick(&reversed, &job).as_deref(),
                Some(expected),
                "job {job}"
            );
        }

        let mut hits: BTreeMap<String, usize> = BTreeMap::new();
        for job in 1..=300 {
            *hits
                .entry(pick(&top, &job.to_string()).unwrap())
                .or_default() += 1;
        }
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert!(hits.values().all(|&n| (70..=130).contains(&n)), "{hits:?}");

        let pair = [vm("x", 1), vm("y", 1)];
        let burst: Vec<String> = (100..110)
            .map(|job| pick(&pair, &job.to_string()).unwrap())
            .collect();
        assert!(
            burst.windows(2).all(|w| w[0] != w[1]),
            "consecutive jobs alternate over two VMs: {burst:?}"
        );
    }

    fn intent(at: u64, job: &str) -> Intent {
        Intent {
            at,
            job: job.to_string(),
        }
    }

    #[test]
    fn a_burst_ranks_by_time_then_job_number() {
        let intents = [
            intent(NOW, "10"),
            intent(NOW, "9"),
            intent(NOW - 1, "11"),
            intent(NOW, "12"),
            intent(NOW + 1, "8"),
        ];
        let rank = |job: &str, slots: u32| rank_burst(&intents, job, NOW, slots);
        // Order: 11 (earlier), 9, 10, 12, 8 (later).
        assert_eq!(
            rank("9", 2),
            Some(Burst::Follower {
                leader: "11".into()
            })
        );
        assert_eq!(rank("10", 2), Some(Burst::Leader { group: 1, taken: 2 }));
        assert_eq!(
            rank("12", 2),
            Some(Burst::Follower {
                leader: "10".into()
            })
        );
        assert_eq!(rank("8", 1), Some(Burst::Leader { group: 4, taken: 1 }));
        assert_eq!(
            rank("10", 3),
            Some(Burst::Follower {
                leader: "11".into()
            })
        );
        assert_eq!(
            rank_burst(&intents, "8", NOW + 1, 2),
            Some(Burst::Leader { group: 2, taken: 1 })
        );
        assert_eq!(
            rank_burst(&intents, "8", NOW + 1, 3),
            Some(Burst::Follower {
                leader: "12".into()
            })
        );
        assert_eq!(
            rank("11", 3),
            Some(Burst::Duplicate),
            "11 is older than now"
        );
        assert_eq!(rank("13", 2), None, "no intent of the job");
    }

    #[test]
    fn a_burst_keeps_each_jobs_earliest_intent_inside_the_window() {
        let window = NOW - GROUP_WINDOW_S;
        let intents = [
            intent(window - 1, "1"),
            intent(window, "2"),
            intent(NOW, "3"),
            intent(NOW, "3"),
            intent(NOW + 2, "2"),
            intent(NOW + 1, "3"),
            intent(NOW, "00000000000000000004"),
        ];
        assert_eq!(
            rank_burst(&intents, "3", NOW, 2),
            Some(Burst::Follower { leader: "2".into() }),
            "1 left the window, 2 counts once at its earliest"
        );
        assert_eq!(
            rank_burst(&intents, "4", NOW, 2),
            Some(Burst::Leader { group: 1, taken: 1 }),
            "a zero-padded id is the same job"
        );
        assert_eq!(
            rank_burst(&intents, "3", NOW + 1, 2),
            Some(Burst::Duplicate)
        );
        assert_eq!(
            rank_burst(&intents, "2", NOW + 2, 2),
            Some(Burst::Leader { group: 1, taken: 1 }),
            "an intent that left the window does not make a duplicate"
        );
        assert_eq!(rank_burst(&intents, "1", NOW, 2), None);
    }

    #[test]
    fn a_burst_orders_text_ids_after_numbers_and_by_string() {
        let intents = [
            intent(NOW, "b"),
            intent(NOW, "10"),
            intent(NOW, "a"),
            intent(NOW, "9"),
        ];
        let leaders: Vec<Option<Burst>> = ["9", "10", "a", "b"]
            .iter()
            .map(|job| rank_burst(&intents, job, NOW, 1))
            .collect();
        assert_eq!(
            leaders,
            (0..4)
                .map(|group| Some(Burst::Leader { group, taken: 1 }))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            rank_burst(&intents, "a", NOW, 0),
            Some(Burst::Leader { group: 2, taken: 1 }),
            "slots 0 counts as 1"
        );
    }

    #[test]
    fn a_leader_takes_the_slots_of_its_group() {
        for slots in 1..=4u32 {
            for n in 1..=9usize {
                let intents: Vec<Intent> =
                    (0..n).map(|job| intent(NOW, &job.to_string())).collect();
                let mut leaders = 0;
                let mut assigned = 0;
                for job in 0..n {
                    match rank_burst(&intents, &job.to_string(), NOW, slots).unwrap() {
                        Burst::Leader { group, taken } => {
                            assert_eq!(group, leaders);
                            assert!(taken >= 1 && taken <= slots);
                            leaders += 1;
                            assigned += taken as usize;
                        }
                        Burst::Follower { leader } => {
                            let leader: usize = leader.parse().unwrap();
                            assert_eq!(leader, job - job % slots as usize);
                        }
                        Burst::Duplicate => panic!("no duplicate in {n} jobs"),
                    }
                }
                assert_eq!(leaders, n.div_ceil(slots as usize), "slots {slots}, n {n}");
                assert_eq!(assigned, n, "every job has a slot, slots {slots}");
            }
        }
    }

    const T0: u64 = 1_800_000_000;
    const VM_SECRET: &str = "vm-secret";

    fn vm_config() -> Arc<Config> {
        Arc::new(
            parse_config(
                r#"{
                  "name": "demo",
                  "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
                  "host": { "type": "fake" }, "provider": { "type": "fake" },
                  "defaultPool": "vm",
                  "pools": [
                    { "tag": "vm", "image": "img/node", "runtime": "instances", "slots": 2, "maxRuns": 2 },
                    { "tag": "jobs", "image": "img/kaniko", "stages": ["build"], "runtime": "jobs" }
                  ],
                  "cache": { "type": "s3" }
                }"#,
            )
            .unwrap(),
        )
    }

    fn vm_secrets() -> Secrets {
        Secrets {
            hook_token: Some("hook-secret".into()),
            runner_tokens: BTreeMap::from([
                ("vm".to_string(), "glrt-vm".to_string()),
                ("jobs".to_string(), "glrt-jobs".to_string()),
            ]),
            dispatch: None,
            cache: Some(KeyPair {
                access_key: "AK".into(),
                secret_key: "SK".into(),
            }),
            vm_secret: Some(VM_SECRET.into()),
            sweep_token: Some("sweep-secret".into()),
            provider_env: BTreeMap::new(),
            dispatcher_env: BTreeMap::new(),
        }
    }

    struct VmSetup {
        dispatcher: Dispatcher,
        recording: Arc<Recording>,
        lines: Arc<Mutex<Vec<String>>>,
        /// Creation time of new VMs.
        clock: Arc<AtomicU64>,
    }

    /// VMs of this setup are created at `T0`; requests arrive at `T0 + 60` unless given a time.
    async fn vm_setup(secrets: Secrets) -> VmSetup {
        vm_setup_with(secrets, None).await
    }

    async fn vm_setup_with(secrets: Secrets, ledger: Option<Arc<dyn Ledger>>) -> VmSetup {
        setup_of(vm_config(), secrets, ledger).await
    }

    async fn setup_of(
        config: Arc<Config>,
        secrets: Secrets,
        ledger: Option<Arc<dyn Ledger>>,
    ) -> VmSetup {
        let clock = Arc::new(AtomicU64::new(T0));
        let created = clock.clone();
        let inner = Arc::new(
            FakeProvider::new(config.clone(), Arc::new(MemoryStore::new()))
                .with_clock(Arc::new(move || created.load(Ordering::SeqCst))),
        );
        inner.apply(&inner.plan(&[]).await.unwrap()).await.unwrap();
        let recording = Arc::new(Recording::new(inner, false));
        let (log, lines) = Log::memory();
        let dispatcher = Dispatcher::new(config, recording.clone(), Arc::new(secrets), log)
            .with_ledger(ledger)
            .with_group_wait(Duration::ZERO);
        VmSetup {
            dispatcher,
            recording,
            lines,
            clock,
        }
    }

    impl VmSetup {
        async fn post(
            &self,
            path: &str,
            headers: &[(String, String)],
            body: &[u8],
        ) -> DispatchResponse {
            self.dispatcher
                .handle_request("POST", path, headers, body, T0 + 60)
                .await
        }

        async fn pending(&self, stage: &str, job: u64) -> DispatchResponse {
            self.pending_at(stage, job, T0 + 60).await
        }

        async fn pending_at(&self, stage: &str, job: u64, now: u64) -> DispatchResponse {
            self.dispatcher
                .handle_request(
                    "POST",
                    "/",
                    &headers("hook-secret"),
                    &event(stage, "pending", 7, job),
                    now,
                )
                .await
        }

        fn taken(&self) -> Vec<u32> {
            self.recording
                .started
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.taken)
                .collect()
        }

        async fn report(&self, run: &str, state: VmState, free: u32) {
            let body = serde_json::to_vec(&VmReport {
                state,
                free,
                slots: 2,
            })
            .unwrap();
            let response = self
                .post("/vm", &vm_headers(run, &vm::token(VM_SECRET, run)), &body)
                .await;
            assert_eq!(response.status, 200, "{}", response.body);
        }

        async fn vms(&self) -> Vec<VmCapacity> {
            self.recording.inner.capacity("vm").await.unwrap().unwrap()
        }

        fn lines(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }
    }

    fn vm_headers(run: &str, token: &str) -> Vec<(String, String)> {
        vec![
            (vm::HEADER_RUN.to_string(), run.to_string()),
            (vm::HEADER_TOKEN.to_string(), token.to_string()),
        ]
    }

    fn run_of(response: &DispatchResponse) -> String {
        response.body["runId"].as_str().unwrap().to_string()
    }

    #[tokio::test(start_paused = true)]
    async fn starts_vms_until_max_runs_not_counting_draining_ones() {
        let s = vm_setup(vm_secrets()).await;
        let first = s.pending_at("test", 1, T0).await;
        assert_eq!(first.body["result"], "started", "{}", first.body);
        assert_eq!(first.body["pool"], "vm");
        let second = s.pending_at("test", 2, T0).await;
        assert_eq!(
            second.body["result"], "started",
            "a VM inside the window offers no slot"
        );
        let third = s.pending_at("test", 3, T0).await;
        assert_eq!(third.status, 200);
        assert_eq!(
            third.body,
            json!({ "result": "at_capacity", "pool": "vm", "ms": 0 })
        );

        s.report(&run_of(&first), VmState::Draining, 2).await;
        let fourth = s.pending_at("test", 4, T0 + GROUP_WINDOW_S).await;
        assert_eq!(
            fourth.body["result"], "started",
            "a draining VM frees its place"
        );
        let fifth = s.pending_at("test", 5, T0 + GROUP_WINDOW_S + 1).await;
        assert_eq!(
            fifth.body["result"], "claimed",
            "a starting VM out of the window is capacity"
        );
        assert!([run_of(&second), run_of(&fourth)].contains(&run_of(&fifth)));
        assert_eq!(s.taken(), [1, 1, 1]);

        let started = s.recording.started.lock().unwrap().clone();
        assert_eq!(started.len(), 3);
        assert!(started.iter().all(|r| r.env[keys::VM_SECRET] == VM_SECRET));
        let lines = s.lines();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("info at_capacity") && l.contains("live=\"2\""))
        );
        assert!(!lines.iter().any(|l| l.contains(VM_SECRET)), "{lines:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn claims_a_free_slot_instead_of_starting() {
        let s = vm_setup(vm_secrets()).await;
        let a = run_of(&s.pending_at("test", 1, T0).await);
        let b = run_of(&s.pending_at("test", 2, T0).await);
        s.report(&a, VmState::Busy, 1).await;
        s.report(&b, VmState::Idle, 2).await;

        let claimed = s.pending("test", 3).await;
        assert_eq!(claimed.status, 200);
        assert_eq!(
            claimed.body,
            json!({ "result": "claimed", "pool": "vm", "runId": b, "ms": 0 })
        );
        let free: BTreeMap<String, u32> = s
            .vms()
            .await
            .into_iter()
            .map(|v| (v.run_id, v.free))
            .collect();
        assert_eq!(free[&a], 1);
        assert_eq!(free[&b], 1);
        assert_eq!(s.recording.started.lock().unwrap().len(), 2);
        assert!(s.lines().iter().any(|l| l.starts_with("info claimed")
            && l.contains(&format!("run=\"{b}\""))
            && l.contains("free=\"3\"")
            && l.contains("job=\"3\"")));

        for job in 4..=5 {
            assert_eq!(s.pending("test", job).await.body["result"], "claimed");
        }
        assert!(s.vms().await.iter().all(|v| v.free == 0));
        let full = s.pending("test", 6).await;
        assert_eq!(full.body["result"], "at_capacity");
    }

    #[tokio::test(start_paused = true)]
    async fn one_job_pools_keep_counting_runs_and_never_get_the_vm_secret() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = vm_setup_with(vm_secrets(), Some(ledger.clone())).await;
        let response = s.pending("build", 1).await;
        assert_eq!(response.body["result"], "started");
        assert_eq!(response.body["pool"], "jobs");
        assert_eq!(s.taken(), [1]);
        assert!(ledger.all().is_empty(), "one-job pools skip the ledger");
        let started = s.recording.started.lock().unwrap();
        assert!(!started[0].env.contains_key(keys::VM_SECRET));
    }

    #[tokio::test(start_paused = true)]
    async fn a_vm_pool_without_a_vm_secret_fails_to_start() {
        let mut secrets = vm_secrets();
        secrets.vm_secret = None;
        let s = vm_setup(secrets).await;
        let response = s.pending("test", 1).await;
        assert_eq!(response.status, 500);
        assert!(
            s.lines()
                .iter()
                .any(|l| l.starts_with("error start_failed") && l.contains("no VM secret"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn vm_requests_need_the_token_of_their_run() {
        let s = vm_setup(vm_secrets()).await;
        let run = run_of(&s.pending_at("test", 1, T0).await);
        let other = run_of(&s.pending_at("test", 2, T0).await);
        let body = serde_json::to_vec(&VmReport {
            state: VmState::Idle,
            free: 2,
            slots: 2,
        })
        .unwrap();
        let good = vm::token(VM_SECRET, &run);
        let rejected: Vec<Vec<(String, String)>> = vec![
            Vec::new(),
            vec![(vm::HEADER_RUN.to_string(), run.clone())],
            vec![(vm::HEADER_TOKEN.to_string(), good.clone())],
            vm_headers(&run, "0000"),
            vm_headers(&run, &vm::token("other-secret", &run)),
            vm_headers(&other, &good),
            vm_headers(&run, &good[..63]),
            headers("hook-secret"),
        ];
        for path in [vm::PATH_REPORT, vm::PATH_TERMINATE] {
            for headers in &rejected {
                let response = s.post(path, headers, &body).await;
                assert_eq!(response.status, 401, "{path} {headers:?}");
                assert_eq!(response.body, Value::Null, "401 has no body");
            }
        }
        let no_secret = {
            let mut secrets = vm_secrets();
            secrets.vm_secret = None;
            vm_setup(secrets).await
        };
        let response = no_secret
            .post(vm::PATH_REPORT, &vm_headers(&run, &good), &body)
            .await;
        assert_eq!(response.status, 401);
        assert!(s.vms().await.iter().all(|v| v.state == VmState::Starting));
        let lines = s.lines();
        assert!(!lines.iter().any(|l| l.contains(&good)));
        for route in [vm::PATH_REPORT, vm::PATH_TERMINATE] {
            assert!(
                lines
                    .iter()
                    .any(|l| l
                        == &format!(r#"warn unauthorized route="{route}" run="{run}" ms="0""#)),
                "{lines:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_vm_reports_its_state() {
        let s = vm_setup(vm_secrets()).await;
        let run = run_of(&s.pending("test", 1).await);
        let headers = vm_headers(&run, &vm::token(VM_SECRET, &run));
        let response = s
            .post("/vm/", &headers, br#"{"state":"busy","free":1,"slots":2}"#)
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body, json!({ "result": "reported", "ms": 0 }));
        let vm = &s.vms().await[0];
        assert_eq!(
            (vm.state, vm.free, vm.seen),
            (VmState::Busy, 1, Some(T0 + 60))
        );
        let bad = s.post("/vm", &headers, br#"{"state":"asleep"}"#).await;
        assert_eq!(bad.status, 400);
        assert_eq!(bad.body["result"], "bad_request");
        let lines = s.lines();
        assert!(lines.iter().any(|l| l.starts_with("info reported")
            && l.contains("state=\"busy\"")
            && l.contains("free=\"1\"")));
        assert!(
            !lines
                .iter()
                .any(|l| l.contains(&vm::token(VM_SECRET, &run)))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_report_for_a_gone_vm_answers_500() {
        let s = vm_setup(vm_secrets()).await;
        let run = run_of(&s.pending("test", 1).await);
        s.recording.inner.stop(&run).await.unwrap();
        let response = s
            .post(
                "/vm",
                &vm_headers(&run, &vm::token(VM_SECRET, &run)),
                br#"{"state":"idle","free":2,"slots":2}"#,
            )
            .await;
        assert_eq!(response.status, 500);
        assert!(
            s.lines()
                .iter()
                .any(|l| l.starts_with("error report_failed"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_vm_terminates_itself_only() {
        let s = vm_setup(vm_secrets()).await;
        let run = run_of(&s.pending_at("test", 1, T0).await);
        let other = run_of(&s.pending_at("test", 2, T0).await);
        let response = s
            .post(
                "/terminate",
                &vm_headers(&run, &vm::token(VM_SECRET, &run)),
                b"",
            )
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body, json!({ "result": "terminated", "ms": 0 }));
        let vms = s.vms().await;
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0].run_id, other);
        let runs = s.recording.inner.list(Some("vm")).await.unwrap();
        assert!(runs.iter().any(|r| r.run_id == run && !r.state.is_live()));
        assert!(s.lines().iter().any(|l| l.starts_with("info terminated")));

        let get = s
            .dispatcher
            .handle_request(
                "GET",
                "/terminate",
                &vm_headers(&other, &vm::token(VM_SECRET, &other)),
                b"",
                T0,
            )
            .await;
        assert_eq!(get.status, 401, "only POST reaches the VM routes");
        assert_eq!(s.vms().await.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_cron_body_sweeps_with_the_sweep_token() {
        let s = vm_setup(vm_secrets()).await;
        let run = run_of(&s.pending("test", 1).await);
        let sweep = |token: Value| json!({ vm::SWEEP_ARG: token }).to_string().into_bytes();
        for body in [
            sweep(json!("wrong")),
            sweep(json!(7)),
            sweep(Value::Null),
            sweep(json!("sweep-secret-x")),
            sweep(json!("hook-secret")),
        ] {
            let response = s.post("/", &[], &body).await;
            assert_eq!(response.status, 401);
            assert_eq!(response.body, Value::Null);
        }
        let late = T0 + vm::SWEEP_STARTING_S + 1;
        let early = s
            .dispatcher
            .handle_request("POST", "/", &[], &sweep(json!("sweep-secret")), T0 + 60)
            .await;
        assert_eq!(early.status, 200);
        assert_eq!(
            early.body,
            json!({ "result": "swept", "runs": [], "ms": 0 })
        );
        let response = s
            .dispatcher
            .handle_request("POST", "/", &[], &sweep(json!("sweep-secret")), late)
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.body,
            json!({ "result": "swept", "runs": [run], "ms": 0 })
        );
        assert!(s.vms().await.is_empty());
        let lines = s.lines();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("info swept") && l.contains("count=\"1\""))
        );
        assert!(!lines.iter().any(|l| l.contains("sweep-secret")));
        assert_eq!(
            lines
                .iter()
                .filter(|l| *l == r#"warn unauthorized route="sweep" ms="0""#)
                .count(),
            5
        );

        let mut secrets = vm_secrets();
        secrets.sweep_token = None;
        let no_hook = vm_setup(secrets).await;
        let response = no_hook.post("/", &[], &sweep(json!(""))).await;
        assert_eq!(response.status, 401);
    }

    #[tokio::test(start_paused = true)]
    async fn other_requests_reach_the_webhook() {
        let (d, recording, _) = setup(false).await;
        let response = d
            .handle_request(
                "POST",
                "/",
                &headers("hook-secret"),
                &event("test", "pending", 7, 1),
                T0,
            )
            .await;
        assert_eq!(response.body["result"], "started");
        let unauthorized = d
            .handle_request(
                "POST",
                "/anything",
                &[],
                &event("test", "pending", 7, 2),
                T0,
            )
            .await;
        assert_eq!(unauthorized.status, 401);
        assert_eq!(unauthorized.body, Value::Null);
        assert_eq!(recording.started.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_vm_is_not_capacity() {
        let s = vm_setup(vm_secrets()).await;
        let run = run_of(&s.pending("test", 1).await);
        s.report(&run, VmState::Idle, 2).await;
        let fresh = s
            .dispatcher
            .handle_request(
                "POST",
                "/",
                &headers("hook-secret"),
                &event("test", "pending", 7, 2),
                T0 + 60,
            )
            .await;
        assert_eq!(fresh.body["result"], "claimed");
        let stale = s
            .dispatcher
            .handle_request(
                "POST",
                "/",
                &headers("hook-secret"),
                &event("test", "pending", 7, 3),
                T0 + 60 + CAPACITY_FRESH_S + 1,
            )
            .await;
        assert_eq!(stale.body["result"], "started", "{}", stale.body);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_claim_starts_a_vm() {
        let s = vm_setup(vm_secrets()).await;
        let run = run_of(&s.pending("test", 1).await);
        s.report(&run, VmState::Idle, 2).await;
        s.recording
            .fail_claim
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let response = s.pending("test", 2).await;
        assert_eq!(response.body["result"], "started", "{}", response.body);
        assert_ne!(run_of(&response), run);
        assert!(s.lines().iter().any(|l| l.starts_with("warn claim_failed")));
        assert_eq!(s.taken(), [1, 1]);
        assert_eq!(s.recording.claim_calls.load(Ordering::SeqCst), 1);
        assert!(!s.lines().iter().any(|l| l.contains("start_retry")));
    }

    #[tokio::test(start_paused = true)]
    async fn vms_get_the_callback_url_from_the_host_header() {
        let s = vm_setup(vm_secrets()).await;
        let mut with_host = headers("hook-secret");
        with_host.push(("Host".to_string(), "dispatch.example".to_string()));
        let response = s
            .post("/", &with_host, &event("test", "pending", 7, 1))
            .await;
        assert_eq!(response.body["result"], "started", "{}", response.body);
        let started = s.recording.started.lock().unwrap();
        assert_eq!(
            started[0]
                .env
                .get("CIBURST_DISPATCH_URL")
                .map(String::as_str),
            Some("https://dispatch.example")
        );
    }

    impl VmSetup {
        fn set_clock(&self, now: u64) {
            self.clock.store(now, Ordering::SeqCst);
        }
    }

    async fn record(ledger: &MemoryLedger, at: u64, jobs: &[u64]) {
        for job in jobs {
            ledger.record("vm", at, &job.to_string()).await.unwrap();
        }
    }

    fn grouped(leader: &str) -> Value {
        json!({ "result": "grouped", "pool": "vm", "leader": leader, "ms": 0 })
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_leader_starts_a_vm_for_its_group_and_followers_ride_on_it() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = vm_setup_with(vm_secrets(), Some(ledger.clone())).await;
        record(&ledger, T0, &[10, 11, 12]).await;

        let leader = s.pending_at("test", 10, T0).await;
        assert_eq!(leader.body["result"], "started", "{}", leader.body);
        assert_eq!(s.pending_at("test", 11, T0).await.body, grouped("10"));
        let second = s.pending_at("test", 12, T0).await;
        assert_eq!(second.body["result"], "started", "{}", second.body);
        assert_eq!(
            s.pending_at("test", 13, T0).await.body,
            grouped("12"),
            "a late intent of the window rides on the partial group"
        );
        assert_eq!(
            s.pending_at("test", 14, T0).await.body,
            json!({ "result": "at_capacity", "pool": "vm", "ms": 0 }),
            "rank 4 is group 2 and maxRuns is 2"
        );
        assert_eq!(s.pending_at("test", 15, T0).await.body, grouped("14"));
        assert_eq!(s.taken(), [2, 1]);

        let mut free: Vec<u32> = s.vms().await.iter().map(|v| v.free).collect();
        free.sort();
        assert_eq!(free, [0, 1], "free = slots - taken");
        let lines = s.lines();
        assert!(lines.iter().any(|l| l.starts_with("info started")
            && l.contains("job=\"10\"")
            && l.contains("taken=\"2\"")));
        assert!(lines.iter().any(|l| l.starts_with("info grouped")
            && l.contains("job=\"11\"")
            && l.contains("leader=\"10\"")
            && l.contains("pool=\"vm\"")));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("info at_capacity") && l.contains("live=\"2\""))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_duplicate_event_starts_nothing() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = vm_setup_with(vm_secrets(), Some(ledger.clone())).await;
        let first = s.pending_at("test", 20, T0).await;
        assert_eq!(first.body["result"], "started");
        let again = s.pending_at("test", 20, T0 + 5).await;
        assert_eq!(again.status, 200);
        assert_eq!(
            again.body,
            json!({ "result": "duplicate", "pool": "vm", "ms": 0 })
        );
        assert_eq!(s.taken(), [1]);
        assert!(
            s.lines()
                .iter()
                .any(|l| l.starts_with("info duplicate") && l.contains("job=\"20\""))
        );
        let next = s.pending_at("test", 21, T0 + 5).await;
        assert_eq!(
            next.body,
            grouped("20"),
            "the duplicate's second intent does not shift the ranks"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn leaders_count_toward_max_runs_by_rank_beside_the_old_vms() {
        for (old_state, rank_two) in [
            (VmState::Busy, "at_capacity"),
            (VmState::Draining, "started"),
        ] {
            let ledger = Arc::new(MemoryLedger::new());
            let s = vm_setup_with(vm_secrets(), Some(ledger.clone())).await;
            let old = run_of(&s.pending_at("test", 1, T0).await);
            s.report(&old, old_state, 0).await;

            let now = T0 + 60;
            s.set_clock(now);
            record(&ledger, now, &[2, 3, 4, 5]).await;
            let first = s.pending_at("test", 2, now).await;
            assert_eq!(first.body["result"], "started", "{old_state:?}");
            assert_eq!(s.pending_at("test", 3, now).await.body, grouped("2"));
            let second = s.pending_at("test", 4, now).await;
            assert_eq!(second.body["result"], rank_two, "{old_state:?}");
            assert_eq!(s.pending_at("test", 5, now).await.body, grouped("4"));
            let expected: &[u32] = if old_state == VmState::Busy {
                &[1, 2]
            } else {
                &[1, 2, 2]
            };
            assert_eq!(s.taken(), expected, "{old_state:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_claim_candidate_keeps_the_job_out_of_the_ledger() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = vm_setup_with(vm_secrets(), Some(ledger.clone())).await;
        let run = run_of(&s.pending_at("test", 1, T0).await);
        assert_eq!(ledger.all().len(), 1);
        s.report(&run, VmState::Idle, 2).await;
        let claimed = s.pending("test", 2).await;
        assert_eq!(claimed.body["result"], "claimed");
        assert_eq!(ledger.all().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_event_without_a_job_id_keeps_the_m3_rule() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = vm_setup_with(vm_secrets(), Some(ledger.clone())).await;
        let body = json!({
            "object_kind": "build", "build_stage": "test", "build_status": "pending",
            "pipeline_id": 9, "project_id": 7
        })
        .to_string()
        .into_bytes();
        for _ in 0..2 {
            let response = s
                .dispatcher
                .handle_request("POST", "/", &headers("hook-secret"), &body, T0)
                .await;
            assert_eq!(response.body["result"], "started", "{}", response.body);
        }
        assert!(ledger.all().is_empty());
        assert_eq!(s.taken(), [1, 1]);
    }

    #[derive(Clone, Copy, Debug)]
    enum Fault {
        Record,
        Since,
        Forget,
    }

    struct FaultyLedger(Fault);

    #[async_trait]
    impl Ledger for FaultyLedger {
        async fn record(&self, _pool: &str, _at: u64, _job: &str) -> Result<()> {
            match self.0 {
                Fault::Record => Err(Error::provider("bucket refused the write")),
                _ => Ok(()),
            }
        }
        async fn since(&self, _pool: &str, _since: u64) -> Result<Vec<Intent>> {
            match self.0 {
                Fault::Since => Err(Error::provider("bucket refused the list")),
                _ => Ok(vec![intent(T0, "99")]),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_ledger_falls_back_to_the_m3_rule() {
        for fault in [Fault::Record, Fault::Since, Fault::Forget] {
            let s = vm_setup_with(vm_secrets(), Some(Arc::new(FaultyLedger(fault)))).await;
            let results: Vec<Value> = [
                s.pending_at("test", 1, T0).await,
                s.pending_at("test", 2, T0).await,
                s.pending_at("test", 3, T0).await,
            ]
            .into_iter()
            .map(|r| r.body["result"].clone())
            .collect();
            assert_eq!(
                results,
                [json!("started"), json!("started"), json!("at_capacity")],
                "{fault:?}"
            );
            assert_eq!(s.taken(), [1, 1], "{fault:?}");
            let failed: Vec<String> = s
                .lines()
                .into_iter()
                .filter(|l| l.starts_with("warn ledger_failed"))
                .collect();
            assert_eq!(failed.len(), 3, "{fault:?}");
            assert!(
                failed[0].starts_with(r#"warn ledger_failed pool="vm" job="1" error="#),
                "{failed:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn parallel_events_record_before_any_reads_during_the_group_wait() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = vm_setup_with(vm_secrets(), None).await;
        let dispatcher = Dispatcher::new(
            vm_config(),
            s.recording.clone(),
            Arc::new(vm_secrets()),
            Log::silent(),
        )
        .with_ledger(Some(ledger.clone()));
        assert_eq!(dispatcher.group_wait, Duration::from_secs(GROUP_WAIT_S));
        let hook = headers("hook-secret");
        let bodies: Vec<Vec<u8>> = [31, 30, 32]
            .into_iter()
            .map(|job| event("test", "pending", 7, job))
            .collect();
        let begin = tokio::time::Instant::now();
        let (a, b, c) = tokio::join!(
            dispatcher.handle_request("POST", "/", &hook, &bodies[0], T0),
            dispatcher.handle_request("POST", "/", &hook, &bodies[1], T0),
            dispatcher.handle_request("POST", "/", &hook, &bodies[2], T0),
        );
        assert_eq!(begin.elapsed(), Duration::from_secs(GROUP_WAIT_S));
        let waited = GROUP_WAIT_S * 1000;
        assert_eq!(
            a.body,
            json!({ "result": "grouped", "pool": "vm", "leader": "30", "ms": waited }),
            "ms counts the group wait"
        );
        assert_eq!(b.body["result"], "started", "{}", b.body);
        assert_eq!(b.body["ms"], waited);
        assert_eq!(c.body["result"], "started", "{}", c.body);
        let mut taken = s.taken();
        taken.sort();
        assert_eq!(taken, [1, 2]);
        assert_eq!(ledger.all().len(), 3);
    }

    fn lane_config(vm_pool: bool) -> Arc<Config> {
        let vm = if vm_pool {
            r#"{ "tag": "vm", "image": "ubuntu:24.04", "runtime": "instances", "slots": 2, "maxRuns": 2 },"#
        } else {
            ""
        };
        let default = if vm_pool { "vm" } else { "node-24" };
        Arc::new(
            parse_config(&format!(
                r#"{{
                  "name": "demo",
                  "gitlab": {{ "url": "https://gitlab.example", "projectIds": [7] }},
                  "host": {{ "type": "fake" }}, "provider": {{ "type": "fake" }},
                  "defaultPool": "{default}",
                  "pools": [
                    {vm}
                    {{ "tag": "node-24", "image": "node:24-bookworm-slim", "runtime": "jobs",
                      "jobs": ["lint", "unit-*"], "maxRuns": 2 }},
                    {{ "tag": "rust-1", "image": "rust:1.96.0-bookworm", "runtime": "jobs",
                      "jobs": ["clippy"], "vmFirst": false }}
                  ],
                  "cache": {{ "type": "s3" }}
                }}"#
            ))
            .unwrap(),
        )
    }

    fn lane_secrets() -> Secrets {
        let mut secrets = vm_secrets();
        secrets.runner_tokens = ["vm", "node-24", "rust-1"]
            .into_iter()
            .map(|tag| (tag.to_string(), format!("glrt-{tag}")))
            .collect();
        secrets
    }

    impl VmSetup {
        async fn named_at(&self, name: &str, job: u64, now: u64) -> DispatchResponse {
            let mut body: Value =
                serde_json::from_slice(&event("test", "pending", 7, job)).unwrap();
            body["build_name"] = name.into();
            self.dispatcher
                .handle_request(
                    "POST",
                    "/",
                    &headers("hook-secret"),
                    body.to_string().as_bytes(),
                    now,
                )
                .await
        }

        fn started_pools(&self) -> Vec<String> {
            self.recording
                .started
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.pool.tag.clone())
                .collect()
        }
    }

    /// A VM of pool `vm` created at `T0` that reports `free` idle slots at `T0 + 60`.
    async fn idle_vm(s: &VmSetup, job: u64, free: u32) -> String {
        let run = run_of(&s.pending_at("test", job, T0).await);
        s.report(&run, VmState::Idle, free).await;
        run
    }

    #[tokio::test(start_paused = true)]
    async fn a_lane_job_claims_a_free_vm_slot_first() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = setup_of(lane_config(true), lane_secrets(), Some(ledger.clone())).await;
        let vm = idle_vm(&s, 1, 2).await;
        let claimed = s.named_at("lint", 2, T0 + 60).await;
        assert_eq!(claimed.status, 200);
        assert_eq!(
            claimed.body,
            json!({ "result": "claimed", "pool": "vm", "lane": "node-24", "runId": vm, "ms": 0 })
        );
        assert_eq!(s.vms().await[0].free, 1);
        assert!(s.lines().contains(&format!(
            r#"info claimed job="2" pipeline="9" project="7" stage="test" pool="vm" run="{vm}" free="2" lane="node-24" ms="0""#
        )));
        let second = s.named_at("unit-api", 3, T0 + 60).await;
        assert_eq!(second.body["result"], "claimed", "{}", second.body);
        assert_eq!(second.body["lane"], "node-24");
        assert_eq!(s.vms().await[0].free, 0);
        let full = s.named_at("lint", 4, T0 + 60).await;
        assert_eq!(full.body["result"], "started", "{}", full.body);
        assert_eq!(full.body["pool"], "node-24");
        assert_eq!(s.started_pools(), ["vm", "node-24"]);
        assert_eq!(*s.recording.claimed.lock().unwrap(), [vm.clone(), vm]);
        assert_eq!(ledger.all().len(), 1, "only the VM job's intent");
    }

    #[tokio::test(start_paused = true)]
    async fn a_lane_job_without_a_claim_candidate_starts_its_lane() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = setup_of(lane_config(true), lane_secrets(), Some(ledger.clone())).await;
        let first = s.named_at("lint", 1, T0).await;
        assert_eq!(first.body["result"], "started", "{}", first.body);
        assert_eq!(first.body["pool"], "node-24");
        assert!(run_of(&first).starts_with("fake-"));
        idle_vm(&s, 2, 2).await;
        let in_window = s.named_at("unit-api", 3, T0 + GROUP_WINDOW_S).await;
        assert_eq!(
            in_window.body["result"], "started",
            "a VM inside the window offers no slot"
        );
        assert_eq!(in_window.body["pool"], "node-24");
        assert_eq!(s.started_pools(), ["node-24", "vm", "node-24"]);
        assert!(s.recording.claimed.lock().unwrap().is_empty());
        assert_eq!(ledger.all().len(), 1, "lanes never use the ledger");
        let started = s.recording.started.lock().unwrap();
        assert_eq!(started[0].env["RUNNER_TOKEN"], "glrt-node-24");
        assert!(!started[0].env.contains_key(keys::VM_SECRET));
        assert!(
            s.lines()
                .iter()
                .any(|l| l.starts_with("info started") && l.contains(r#"pool="node-24""#))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_lane_with_vm_first_false_never_claims() {
        let s = setup_of(lane_config(true), lane_secrets(), None).await;
        idle_vm(&s, 1, 2).await;
        let response = s.named_at("clippy", 2, T0 + 60).await;
        assert_eq!(response.body["result"], "started", "{}", response.body);
        assert_eq!(response.body["pool"], "rust-1");
        assert_eq!(s.vms().await[0].free, 2);
        assert!(s.recording.claimed.lock().unwrap().is_empty());
        assert_eq!(*s.recording.asked.lock().unwrap(), ["vm", "rust-1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_vm_pool_a_lane_never_claims() {
        let s = setup_of(lane_config(false), lane_secrets(), None).await;
        let response = s.named_at("lint", 1, T0 + 60).await;
        assert_eq!(response.body["result"], "started", "{}", response.body);
        assert_eq!(response.body["pool"], "node-24");
        let other = s.pending("deploy", 2).await;
        assert_eq!(other.body["pool"], "node-24", "the default pool");
        assert_eq!(*s.recording.asked.lock().unwrap(), ["node-24", "node-24"]);
        assert!(s.recording.claimed.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_lane_at_max_runs_answers_at_capacity() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = setup_of(lane_config(true), lane_secrets(), Some(ledger.clone())).await;
        for job in 1..=2 {
            assert_eq!(s.named_at("lint", job, T0).await.body["result"], "started");
        }
        let third = s.named_at("unit-api", 3, T0).await;
        assert_eq!(third.status, 200);
        assert_eq!(
            third.body,
            json!({ "result": "at_capacity", "pool": "node-24", "ms": 0 })
        );
        assert!(s.lines().iter().any(|l| l.starts_with("info at_capacity")
            && l.contains(r#"pool="node-24""#)
            && l.contains(r#"live="2""#)));
        assert!(ledger.all().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_claim_leaves_the_job_to_its_lane() {
        let s = setup_of(lane_config(true), lane_secrets(), None).await;
        let vm = idle_vm(&s, 1, 2).await;
        s.recording.fail_claim.store(true, Ordering::SeqCst);
        let response = s.named_at("lint", 2, T0 + 60).await;
        assert_eq!(response.body["result"], "started", "{}", response.body);
        assert_eq!(response.body["pool"], "node-24");
        assert!(s.lines().iter().any(|l| l.starts_with(&format!(
            r#"warn claim_failed run="{vm}" lane="node-24" error="#
        ))));
        assert_eq!(s.vms().await[0].free, 2);
        assert_eq!(
            s.recording.claim_calls.load(Ordering::SeqCst),
            1,
            "a retryable claim failure is not retried"
        );
    }

    /// The value of the `ms` field that ends a log line.
    fn ms_of(line: &str) -> Option<u64> {
        line.rsplit_once(" ms=\"")?
            .1
            .strip_suffix('"')?
            .parse()
            .ok()
    }

    #[tokio::test(start_paused = true)]
    async fn every_answer_and_log_line_carries_the_milliseconds_since_arrival() {
        let s = vm_setup(vm_secrets()).await;
        let run = run_of(&s.pending("test", 1).await);
        let token = vm::token(VM_SECRET, &run);
        let before = s.lines().len();
        *s.recording.delay.lock().unwrap() = Duration::from_millis(250);
        let hook = headers("hook-secret");
        let report = serde_json::to_vec(&VmReport {
            state: VmState::Idle,
            free: 1,
            slots: 2,
        })
        .unwrap();
        let pipeline = json!({
            "object_kind": "pipeline",
            "object_attributes": { "id": 9, "status": "pending" },
            "project": { "id": 7 }
        });
        let sweep = json!({ vm::SWEEP_ARG: "sweep-secret" });
        let cases = [
            (
                "/",
                hook.clone(),
                event("test", "pending", 7, 2),
                "claimed",
                500,
            ),
            (
                "/",
                hook.clone(),
                event("build", "pending", 7, 3),
                "started",
                1000,
            ),
            (
                "/",
                hook.clone(),
                pipeline.to_string().into_bytes(),
                "ignored",
                0,
            ),
            (
                "/",
                hook.clone(),
                event("test", "running", 7, 2),
                "ignored",
                0,
            ),
            ("/", hook.clone(), b"{not json".to_vec(), "bad_request", 0),
            ("/vm", vm_headers(&run, &token), report, "reported", 250),
            (
                "/",
                Vec::new(),
                sweep.to_string().into_bytes(),
                "swept",
                250,
            ),
            (
                "/terminate",
                vm_headers(&run, &token),
                Vec::new(),
                "terminated",
                250,
            ),
        ];
        for (path, headers, body, result, ms) in cases {
            let response = s.post(path, &headers, &body).await;
            assert_eq!(response.body["result"], result, "{path} {}", response.body);
            assert_eq!(response.body["ms"], ms, "{path} {}", response.body);
        }
        s.recording.fail_starts([Error::provider("scaleway: 400")]);
        let failed = s.pending("build", 4).await;
        assert_eq!(failed.status, 500);
        assert_eq!(failed.body, json!({ "result": "error", "ms": 1000 }));
        let refused = s.post("/vm", &vm_headers(&run, "0"), b"{}").await;
        assert_eq!(
            (refused.status, refused.body),
            (401, Value::Null),
            "401 keeps no body"
        );

        let lines = s.lines()[before..].to_vec();
        let decisions: Vec<(&str, Option<u64>)> = lines
            .iter()
            .map(|line| (line.split(' ').nth(1).unwrap_or(""), ms_of(line)))
            .collect();
        assert_eq!(
            decisions,
            [
                ("claimed", Some(500)),
                ("started", Some(1000)),
                ("reported", Some(250)),
                ("swept", Some(250)),
                ("terminated", Some(250)),
                ("start_failed", Some(1000)),
                ("unauthorized", Some(0)),
            ],
            "{lines:?}"
        );
    }

    /// A VM job's answer when the provider's start calls fail with `errors` and each takes
    /// `start_delay`.
    async fn start_failing(errors: Vec<Error>, start_delay: Duration) -> (VmSetup, Value) {
        let s = vm_setup(vm_secrets()).await;
        s.recording.fail_starts(errors);
        *s.recording.start_delay.lock().unwrap() = start_delay;
        let response = s.pending("test", 1).await;
        let answer = json!({ "status": response.status, "body": response.body });
        (s, answer)
    }

    #[tokio::test(start_paused = true)]
    async fn a_retryable_start_failure_is_retried_once_after_300_ms() {
        let (s, answer) =
            start_failing(vec![Error::retryable("scaleway: 503")], Duration::ZERO).await;
        assert_eq!(answer["status"], 200);
        assert_eq!(answer["body"]["result"], "started", "{answer}");
        assert_eq!(answer["body"]["ms"], 300);
        assert_eq!(s.recording.start_calls(), 2);
        let lines = s.lines();
        assert_eq!(
            lines[0],
            r#"warn start_retry pool="vm" job="1" error="scaleway: 503" ms="0""#
        );
        assert!(lines[1].starts_with("info started"), "{lines:?}");
        assert_eq!(ms_of(&lines[1]), Some(300));

        let (s, answer) = start_failing(
            vec![
                Error::retryable("scaleway: 503"),
                Error::retryable("scaleway: 429"),
            ],
            Duration::ZERO,
        )
        .await;
        assert_eq!(answer["status"], 500, "one retry only");
        assert_eq!(answer["body"], json!({ "result": "error", "ms": 300 }));
        assert_eq!(s.recording.start_calls(), 2);
        assert!(
            s.lines()[1].starts_with(r#"error start_failed job="1" pipeline="9" project="7" stage="test" pool="vm" error="scaleway: 429""#),
            "{:?}",
            s.lines()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn other_start_failures_are_not_retried() {
        let (s, answer) =
            start_failing(vec![Error::provider("scaleway: 400")], Duration::ZERO).await;
        assert_eq!(answer["status"], 500);
        assert_eq!(answer["body"], json!({ "result": "error", "ms": 0 }));
        assert_eq!(s.recording.start_calls(), 1);
        assert!(!s.lines().iter().any(|l| l.contains("start_retry")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_begins_within_5_s_of_the_arrival_only() {
        let (s, answer) = start_failing(
            vec![Error::retryable("scaleway: 503")],
            Duration::from_millis(4_699),
        )
        .await;
        assert_eq!(answer["body"]["result"], "started", "{answer}");
        assert_eq!(answer["body"]["ms"], 4_699 + 300 + 4_699);
        assert_eq!(s.recording.start_calls(), 2);

        let (s, answer) = start_failing(
            vec![Error::retryable("scaleway: 503")],
            Duration::from_millis(4_700),
        )
        .await;
        assert_eq!(answer["status"], 500, "the retry would begin at 5 s");
        assert_eq!(answer["body"]["ms"], 4_700);
        assert_eq!(s.recording.start_calls(), 1);
        assert!(!s.lines().iter().any(|l| l.contains("start_retry")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_lane_start_is_retried_too() {
        let s = setup_of(lane_config(false), lane_secrets(), None).await;
        s.recording.fail_starts([Error::retryable("scaleway: 503")]);
        let response = s.named_at("lint", 1, T0).await;
        assert_eq!(response.body["result"], "started", "{}", response.body);
        assert_eq!(response.body["pool"], "node-24");
        assert_eq!(response.body["ms"], 300);
        assert_eq!(s.recording.start_calls(), 2);
        assert_eq!(s.started_pools(), ["node-24"]);
    }
}
