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
use crate::fleet::FleetStore;
use crate::fleet::Member;
use crate::ledger::{
    Closed, GROUP_WAIT_S, GROUP_WINDOW_S, Intent, Ledger, PipelineRecords, Queued, Running,
    Schedule, VmJobs,
};
use crate::log::Log;
use crate::provision::keys;
use crate::queue::{QUEUED_MAX_AGE_S, RuleInput, RulePlan, RunningJob, next_report_s, start_rule};
use crate::time::parse_gitlab_time;
use crate::types::{
    Provider, Secrets, StartRequest, VmCapacity, VmReport, VmReportAnswer, VmState,
};
use crate::vm;
use crate::{Error, Result};

mod members;

pub use members::{FLEET_CACHE, FleetCache, JOIN_BUDGET, PATH_FLEET, PATH_JOIN, PATH_LEAVE};
use members::{Routed, RunnerOf};

/// spec 6.6: the pause before the one retry of a start that failed with a retryable error.
pub const START_RETRY_PAUSE: Duration = Duration::from_millis(300);
/// spec 6.6: a retry begins only this long after the request arrived, at the latest.
pub const START_RETRY_WITHIN: Duration = Duration::from_secs(5);

/// spec 6.5: the burst ledger records a pipeline's warm VM as this prefix and the pipeline
/// id, where a job records its job id.
pub const WARM_PREFIX: &str = "warm:";

/// spec 6.5: statuses of the builds a pipeline event counts, jobs yet to run.
const WAITING_STATUSES: [&str; 5] = [
    "created",
    "pending",
    "waiting_for_resource",
    "preparing",
    "scheduled",
];

/// spec 6.5: the statuses that end a pipeline and its records.
const FINAL_STATUSES: [&str; 4] = ["success", "failed", "canceled", "skipped"];

/// D56: the ids of a pipeline event's builds with status `manual`.
fn manual_jobs(event: &Value) -> Vec<String> {
    event
        .get("builds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|build| build.get("status").and_then(Value::as_str) == Some("manual"))
        .filter_map(|build| build.get("id").and_then(Value::as_u64))
        .map(|id| id.to_string())
        .collect()
}

/// spec 6.4: the job statuses the schedule tracks, `running` and the final ones.
const TRACKED_STATUSES: [&str; 5] = ["running", "success", "failed", "canceled", "skipped"];

/// spec 6.4: VMs started for the queue carry the label `job` with this prefix and the id of
/// the oldest job that needs one.
pub const QUEUE_PREFIX: &str = "queue:";

/// A running entry older than this is a lost finished event: no VM, and so no job, lives longer
/// ([`vm::SWEEP_MAX_AGE_S`]).
pub const RUNNING_MAX_AGE_S: u64 = vm::SWEEP_MAX_AGE_S;

/// When a request arrived, on tokio's clock, which tests pause and advance (spec 5.2).
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

/// Environment a run receives on top of its definition. Secret values only. The runner token
/// is `token` when given (a member's lane, D60), else the host's runner of the pool's runner
/// tag. A VM run also gets `CIBURST_VM_SECRET`, from which the provider derives the VM's own
/// token; lane runs never see it, because their jobs could read it.
pub fn run_env(
    config: &Config,
    secrets: &Secrets,
    pool: &Pool,
    vm: bool,
    token: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    let token = match token {
        Some(token) => token.to_string(),
        None => secrets
            .runner_tokens
            .get(pool.runner_tag())
            .cloned()
            .ok_or_else(|| Error::other(format!("no runner token for {}", pool.tag)))?,
    };
    let mut env = BTreeMap::from([("RUNNER_TOKEN".to_string(), token)]);
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

/// Routes one request: GitLab job and pipeline events, VM reports, VM termination and the
/// sweep trigger. Pure apart from the provider, ledger and record calls.
pub struct Dispatcher {
    /// The config of the environment; [`Dispatcher::config`] answers the fleet's when it has one.
    base: Arc<Config>,
    provider: Arc<dyn Provider>,
    secrets: Arc<Secrets>,
    log: Log,
    dispatch_url: Option<String>,
    /// Burst intents (spec 6.3); `None` starts one VM per job (spec 6.3).
    ledger: Option<Arc<dyn Ledger>>,
    /// Warm start records (spec 6.5); `None` starts no warm VM.
    records: Option<Arc<dyn PipelineRecords>>,
    /// Queue-first scheduling (D53, spec 6.4); `None` queues nothing.
    schedule: Option<Arc<dyn Schedule>>,
    /// Pause between recording an intent and reading the burst.
    group_wait: Duration,
    /// Fleet state (D58); `None` serves the projects of the config.
    fleet: Option<Arc<dyn FleetStore>>,
    fleet_cache: Arc<FleetCache>,
}

impl Dispatcher {
    pub fn new(
        config: Arc<Config>,
        provider: Arc<dyn Provider>,
        secrets: Arc<Secrets>,
        log: Log,
    ) -> Dispatcher {
        Dispatcher {
            base: config,
            provider,
            secrets,
            log,
            dispatch_url: None,
            ledger: None,
            records: None,
            schedule: None,
            group_wait: Duration::from_secs(GROUP_WAIT_S),
            fleet: None,
            fleet_cache: Arc::new(FleetCache::new()),
        }
    }

    /// The burst ledger (spec 6.3).
    pub fn with_ledger(mut self, ledger: Option<Arc<dyn Ledger>>) -> Dispatcher {
        self.ledger = ledger;
        self
    }

    /// The warm start records (spec 6.5). Without them a pipeline event answers every VM
    /// pool `disabled`.
    pub fn with_records(mut self, records: Option<Arc<dyn PipelineRecords>>) -> Dispatcher {
        self.records = records;
        self
    }

    /// The queue-first schedule (spec 6.4). Without it VM jobs never queue and job events other
    /// than `pending` are ignored.
    pub fn with_schedule(mut self, schedule: Option<Arc<dyn Schedule>>) -> Dispatcher {
        self.schedule = schedule;
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
    /// milliseconds since the request arrived (spec 5.2).
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
        let path = path.trim_end_matches('/');
        let post = method.eq_ignore_ascii_case("POST");
        if post && path == vm::PATH_TERMINATE {
            return self.vm_terminate(headers, at).await;
        }
        if let Some(given) = sweep_arg(body) {
            return self.sweep(given.as_deref(), now, at).await;
        }
        self.refresh_fleet(at).await;
        if method.eq_ignore_ascii_case("GET") && path == PATH_FLEET {
            return self.fleet_info(headers, now, at).await;
        }
        if post {
            match path {
                vm::PATH_REPORT => return self.vm_report(headers, body, now, at).await,
                PATH_JOIN => return self.join(headers, body, now, at).await,
                PATH_LEAVE => return self.leave(headers, now, at).await,
                _ => {}
            }
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
        self.refresh_fleet(at).await;
        stamp(self.webhook(headers, body, now, at).await, at)
    }

    /// Log lines of a request end with `ms` (spec 5.2).
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
        let token = header(headers, "x-gitlab-token");
        let Ok(event) = serde_json::from_slice::<Value>(body) else {
            if !token_matches(token, self.secrets.hook_token.as_deref()) {
                return unauthorized();
            }
            return reply(400, "bad_request", json!({}));
        };
        // Job events name the project in `project_id`, pipeline events in `project.id`.
        let project_id = event.get("project_id").and_then(Value::as_u64).or_else(|| {
            event
                .get("project")
                .and_then(|project| project.get("id"))
                .and_then(Value::as_u64)
        });
        if !self.authorized(token, project_id) {
            return unauthorized();
        }
        match event.get("object_kind").and_then(Value::as_str) {
            Some("build") => self.job_event(headers, &event, now, at).await,
            Some("pipeline") => self.pipeline_event(headers, &event, now, at).await,
            _ => ignored("not_job_event", json!({})),
        }
    }

    /// The URL VMs call back on: the configured one, else the host of the webhook request.
    fn callback(&self, headers: &[(String, String)]) -> Option<String> {
        self.dispatch_url.clone().or_else(|| {
            header(headers, "x-forwarded-host")
                .or_else(|| header(headers, "host"))
                .filter(|h| !h.is_empty())
                .map(|h| format!("https://{h}"))
        })
    }

    async fn job_event(
        &self,
        headers: &[(String, String)],
        event: &Value,
        now: u64,
        at: Arrival,
    ) -> DispatchResponse {
        let status = event
            .get("build_status")
            .and_then(Value::as_str)
            .unwrap_or("");
        let schedule = self.schedule.as_deref();
        let tracked = schedule.is_some() && TRACKED_STATUSES.contains(&status);
        if status != "pending" && !tracked {
            return ignored("status", json!({ "status": status }));
        }
        let project_id = event.get("project_id").and_then(Value::as_u64).unwrap_or(0);
        let Some(member) = self.member(project_id) else {
            return ignored("project", json!({}));
        };
        let config = self.config();
        let name = event
            .get("build_name")
            .and_then(Value::as_str)
            .unwrap_or("");
        if is_own_job(name) {
            return ignored("own_job", json!({ "name": name }));
        }
        // D60 to D63: the project's own `ignoreJobs`, then its own lanes, else the VM runner.
        let pool = match self.route_of(&member, name) {
            Routed::Ignored => return ignored("ignore_jobs", json!({})),
            Routed::Lane(pool) => pool,
            Routed::Vm => {
                let pool = config.vm_pool();
                // spec 8.5: only the project's own VM runner takes the job.
                if self.runner_of(project_id, &pool.tag) == RunnerOf::Missing {
                    return ignored("no_runner", json!({ "pool": pool.tag }));
                }
                pool
            }
        };
        let stage = event.get("build_stage").and_then(Value::as_str);
        let labels = BTreeMap::from([
            ("project".to_string(), project_id.to_string()),
            ("pipeline".to_string(), number(event, "pipeline_id")),
            ("job".to_string(), number(event, "build_id")),
            ("stage".to_string(), stage.unwrap_or("").to_string()),
        ]);
        let mut fields: Vec<(&str, &str)> = labels
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if let Some(schedule) = schedule.filter(|_| tracked) {
            return self
                .track(schedule, &pool, status, event, fields, now, at)
                .await;
        }
        // Records are keyed by GitLab's numeric pipeline id only.
        let pipeline = event
            .get("pipeline_id")
            .and_then(Value::as_u64)
            .map(|id| id.to_string());
        if let Some(pipeline) = &pipeline
            && self.stale(event, pipeline, &labels["job"], at).await
        {
            self.info(at, "stale", &fields);
            return ignored("pipeline_closed", json!({ "pipeline": pipeline }));
        }
        let callback = self.callback(headers);
        fields.push(("pool", &pool.tag));
        match self
            .schedule(&pool, name, labels.clone(), now, callback.as_deref(), at)
            .await
        {
            Ok(Decision::Queued { wait_s }) => {
                let eta = wait_s.map_or_else(|| "null".to_string(), |s| s.to_string());
                fields.push(("eta_s", &eta));
                self.info(at, "queued", &fields);
                reply(200, "queued", json!({ "pool": pool.tag, "etaS": wait_s }))
            }
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

    /// Spec 6.1, scheduling, spec 6.3: a free slot of a claim candidate is claimed;
    /// otherwise, with a schedule and a live VM, the job queues unless the start rule gives it a
    /// VM (spec 6.4); else it joins the burst ledger, or without one a VM starts unless
    /// `maxRuns` VMs are live (not draining), where it queues with a schedule. Providers without
    /// VMs answer `None` from `capacity` and count live runs.
    async fn schedule(
        &self,
        pool: &Pool,
        name: &str,
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
        let schedule = self.schedule.as_deref().filter(|_| !job.is_empty());
        let mut wait_s = None;
        let entry = Queued {
            job: job.clone(),
            name: name.to_string(),
            pipeline: labels.get("pipeline").cloned().unwrap_or_default(),
            at: now,
        };
        if let Some(schedule) = schedule {
            match self
                .queue_first(schedule, pool, &vms, &entry, now, callback, at)
                .await
            {
                QueueFirst::Queued { wait_s } => return Ok(Decision::Queued { wait_s }),
                QueueFirst::Burst { wait_s: wait } => wait_s = wait,
            }
        }
        let decision = self
            .burst_path(pool, &vms, labels, &job, now, callback, at)
            .await?;
        match (decision, schedule) {
            // spec 6.4: at `maxRuns` live VMs a VM job queues instead.
            (Decision::AtCapacity(live), Some(schedule)) => {
                match schedule.enqueue(&pool.tag, &entry).await {
                    Ok(()) => Ok(Decision::Queued { wait_s }),
                    Err(error) => {
                        self.schedule_failed(at, "enqueue", &pool.tag, &error);
                        Ok(Decision::AtCapacity(live))
                    }
                }
            }
            (decision, _) => Ok(decision),
        }
    }

    /// spec 6.4 for a pending VM job without a free slot: with no live VM it takes
    /// the burst path; else the start rule runs with the job as the newest queued one. A job the
    /// rule gives a VM takes the burst path when no VM has a ready slot (start rule step 3); every
    /// other job queues, and the VMs the rule starts for the queue start. A schedule
    /// error takes the burst path.
    #[allow(clippy::too_many_arguments)]
    async fn queue_first(
        &self,
        schedule: &dyn Schedule,
        pool: &Pool,
        vms: &[VmCapacity],
        entry: &Queued,
        now: u64,
        callback: Option<&str>,
        at: Arrival,
    ) -> QueueFirst {
        let live = live_count(vms);
        if live == 0 {
            return QueueFirst::Burst { wait_s: None };
        }
        let mut queued = match self.live_queue(schedule, &pool.tag, now).await {
            Ok(queued) => queued,
            Err((op, error)) => {
                self.schedule_failed(at, op, &pool.tag, &error);
                return QueueFirst::Burst { wait_s: None };
            }
        };
        // A repeated event keeps the job's place in the queue.
        let known = queued.iter().any(|q| q.job == entry.job);
        if !known {
            queued.push(entry.clone());
        }
        let plan = match self.evaluate(schedule, pool, vms, &queued, now).await {
            Ok(plan) => plan,
            Err((op, error)) => {
                self.schedule_failed(at, op, &pool.tag, &error);
                return QueueFirst::Burst { wait_s: None };
            }
        };
        let (wait_s, needs_vm) = plan
            .verdict(&entry.job)
            .map_or((None, true), |v| (v.wait_s, v.needs_vm));
        // The burst ledger hands the free slots of the VMs of its window to late followers, the
        // start rule to the oldest queued jobs. While such slots exist the start rule alone
        // decides, so no job rides on a slot a queued job already expects; a job already queued
        // stays there too.
        let ready = ready_slots(vms, now);
        if needs_vm && live < pool.max_runs as usize && !known && ready == 0 {
            return QueueFirst::Burst { wait_s };
        }
        if !known && let Err(error) = schedule.enqueue(&pool.tag, entry).await {
            self.schedule_failed(at, "enqueue", &pool.tag, &error);
            return QueueFirst::Burst { wait_s };
        }
        self.start_for_queue(schedule, pool, &queued, &plan, now, callback, at)
            .await;
        QueueFirst::Queued { wait_s }
    }

    /// spec 6.3: the job joins the burst ledger and a leader starts a VM for its group;
    /// without a ledger, or when it fails, a VM starts unless `maxRuns` VMs are live.
    #[allow(clippy::too_many_arguments)]
    async fn burst_path(
        &self,
        pool: &Pool,
        vms: &[VmCapacity],
        labels: BTreeMap<String, String>,
        job: &str,
        now: u64,
        callback: Option<&str>,
        at: Arrival,
    ) -> Result<Decision> {
        if let Some(ledger) = self.ledger.as_deref().filter(|_| !job.is_empty()) {
            match self.burst(ledger, pool, job, now).await {
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
                // Only a warm intent is covered; a job with such an id takes the fallback of spec 6.3.
                Ok(Burst::Covered) => {}
                Err(error) => {
                    let message = error.to_string();
                    self.warn(
                        at,
                        "ledger_failed",
                        &[("pool", &pool.tag), ("job", job), ("error", &message)],
                    );
                }
            }
        }
        let live = live_count(vms);
        if live >= pool.max_runs as usize {
            return Ok(Decision::AtCapacity(live));
        }
        self.start(pool, labels, true, callback, 1, at).await
    }

    /// spec 6.4 over the queue `queued` of VM pool `pool`, reading the running jobs and
    /// their typical durations; an empty queue reads nothing more. A running job that started
    /// more than [`RUNNING_MAX_AGE_S`] ago is a lost finished event and is removed.
    async fn evaluate(
        &self,
        schedule: &dyn Schedule,
        pool: &Pool,
        vms: &[VmCapacity],
        queued: &[Queued],
        now: u64,
    ) -> std::result::Result<RulePlan, (&'static str, Error)> {
        let mut running = Vec::new();
        if !queued.is_empty() {
            let (jobs, lost): (Vec<Running>, Vec<Running>) = schedule
                .running(&pool.tag)
                .await
                .map_err(|e| ("running", e))?
                .into_iter()
                .partition(|job| now.saturating_sub(job.started) <= RUNNING_MAX_AGE_S);
            for job in lost {
                schedule
                    .finished(&pool.tag, &job.job, &job.name, None)
                    .await
                    .map_err(|e| ("finished", e))?;
            }
            let mut typical: BTreeMap<String, Option<u64>> = BTreeMap::new();
            for job in &jobs {
                if !typical.contains_key(&job.name) {
                    let value = schedule
                        .typical(&pool.tag, &job.name)
                        .await
                        .map_err(|e| ("typical", e))?;
                    typical.insert(job.name.clone(), value);
                }
            }
            running = jobs
                .iter()
                .map(|job| RunningJob {
                    started: job.started,
                    typical: typical.get(&job.name).copied().flatten(),
                })
                .collect();
        }
        Ok(start_rule(&RuleInput {
            now,
            queued,
            running: &running,
            ready: ready_slots(vms, now),
            slots: pool.slots,
            max_queue_s: pool.max_queue_s(),
            max_runs: pool.max_runs,
            live: live_count(vms),
        }))
    }

    /// spec 6.4, start rule step 6: starts the VMs `plan` gives the queue, each with every slot free and
    /// the labels `job=queue:<oldest job that needs one>` and its `pipeline`, once the pool's
    /// start marker is taken. A start begins only within [`START_RETRY_WITHIN`] of the arrival,
    /// so the answer stays inside GitLab's 10 s; the next evaluation starts the rest.
    #[allow(clippy::too_many_arguments)]
    async fn start_for_queue(
        &self,
        schedule: &dyn Schedule,
        pool: &Pool,
        queued: &[Queued],
        plan: &RulePlan,
        now: u64,
        callback: Option<&str>,
        at: Arrival,
    ) {
        if plan.start == 0 {
            return;
        }
        let Some(oldest) = plan
            .verdicts
            .iter()
            .find(|v| v.needs_vm)
            .and_then(|v| queued.iter().find(|q| q.job == v.job))
        else {
            return;
        };
        match schedule.take_start(&pool.tag, now).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                self.schedule_failed(at, "take_start", &pool.tag, &error);
                return;
            }
        }
        let job = format!("{QUEUE_PREFIX}{}", oldest.job);
        let labels = BTreeMap::from([
            ("job".to_string(), job.clone()),
            ("pipeline".to_string(), oldest.pipeline.clone()),
        ]);
        let mut runs = Vec::new();
        for _ in 0..plan.start {
            if at.elapsed() >= START_RETRY_WITHIN {
                break;
            }
            match self
                .start_run(pool, labels.clone(), true, callback, 0, at)
                .await
            {
                Ok(run) => runs.push(run),
                Err(error) => {
                    let message = error.to_string();
                    self.error(
                        at,
                        "start_failed",
                        &[("pool", &pool.tag), ("job", &job), ("error", &message)],
                    );
                    break;
                }
            }
        }
        if !runs.is_empty() {
            let (vms, needing, runs) = (
                runs.len().to_string(),
                plan.needing.to_string(),
                runs.join(","),
            );
            self.info(
                at,
                "queue_started",
                &[
                    ("pool", &pool.tag),
                    ("job", &job),
                    ("vms", &vms),
                    ("needing", &needing),
                    ("runs", &runs),
                ],
            );
        }
    }

    /// The queue of `pool` without entries older than [`QUEUED_MAX_AGE_S`], which it removes
    /// (spec 6.4).
    async fn live_queue(
        &self,
        schedule: &dyn Schedule,
        pool: &str,
        now: u64,
    ) -> std::result::Result<Vec<Queued>, (&'static str, Error)> {
        let (fresh, stale): (Vec<Queued>, Vec<Queued>) = schedule
            .queued(pool)
            .await
            .map_err(|e| ("queued", e))?
            .into_iter()
            .partition(|q| now.saturating_sub(q.at) <= QUEUED_MAX_AGE_S);
        for job in stale {
            schedule
                .finished(pool, &job.job, &job.name, None)
                .await
                .map_err(|e| ("finished", e))?;
        }
        Ok(fresh)
    }

    /// spec 6.4: a `running` job event of a VM job or lane job records it as running, a
    /// final one as finished, with its duration for `success` and `failed`. A schedule error
    /// answers as without a schedule.
    #[allow(clippy::too_many_arguments)]
    async fn track(
        &self,
        schedule: &dyn Schedule,
        pool: &Pool,
        status: &str,
        event: &Value,
        mut fields: Vec<(&str, &str)>,
        now: u64,
        at: Arrival,
    ) -> DispatchResponse {
        let job = number(event, "build_id");
        if job.is_empty() {
            return ignored("status", json!({ "status": status }));
        }
        let name = event
            .get("build_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let (op, done, duration) = if status == "running" {
            let started = event
                .get("build_started_at")
                .and_then(Value::as_str)
                .and_then(parse_gitlab_time)
                .unwrap_or(now);
            let running = Running { job, name, started };
            ("started", schedule.started(&pool.tag, &running).await, None)
        } else {
            let duration = matches!(status, "success" | "failed")
                .then(|| event.get("build_duration").and_then(Value::as_f64))
                .flatten()
                .map(|seconds| seconds.max(0.0).round() as u64);
            let done = schedule.finished(&pool.tag, &job, &name, duration).await;
            ("finished", done, duration)
        };
        if let Err(error) = done {
            self.schedule_failed(at, op, &pool.tag, &error);
            return ignored("status", json!({ "status": status }));
        }
        let duration = duration.map(|d| d.to_string());
        fields.extend([("pool", pool.tag.as_str()), ("status", status)]);
        if let Some(duration) = &duration {
            fields.push(("duration_s", duration));
        }
        self.info(at, "tracked", &fields);
        reply(
            200,
            "tracked",
            json!({ "status": status, "pool": pool.tag }),
        )
    }

    /// spec 6.4: the start rule over the queue of `pool` and its VM starts.
    #[allow(clippy::too_many_arguments)]
    async fn reevaluate(
        &self,
        schedule: &dyn Schedule,
        pool: &Pool,
        vms: &[VmCapacity],
        queued: &[Queued],
        now: u64,
        callback: Option<&str>,
        at: Arrival,
    ) {
        if !queued.is_empty() {
            match self.evaluate(schedule, pool, vms, queued, now).await {
                Ok(plan) => {
                    self.start_for_queue(schedule, pool, queued, &plan, now, callback, at)
                        .await
                }
                Err((op, error)) => self.schedule_failed(at, op, &pool.tag, &error),
            }
        }
    }

    /// The VMs of `pool`; `None` for a pool without VMs and after a failed read, which logs
    /// `capacity_failed`.
    async fn vms_of(&self, pool: &Pool, at: Arrival) -> Option<Vec<VmCapacity>> {
        match self.provider.capacity(&pool.tag).await {
            Ok(vms) => vms,
            Err(error) => {
                self.capacity_failed(at, &pool.tag, &error);
                None
            }
        }
    }

    /// spec 6.4: a schedule error never fails an answer; the caller goes on as without one.
    fn schedule_failed(&self, at: Arrival, op: &str, pool: &str, error: &Error) {
        let message = error.to_string();
        self.warn(
            at,
            "schedule_failed",
            &[("op", op), ("pool", pool), ("error", &message)],
        );
    }

    fn capacity_failed(&self, at: Arrival, pool: &str, error: &Error) {
        let message = error.to_string();
        self.warn(
            at,
            "capacity_failed",
            &[("pool", pool), ("error", &message)],
        );
    }

    /// spec 6.5: a failed record call never fails an answer; the caller goes on as without
    /// records.
    fn records_failed(&self, at: Arrival, op: &str, pipeline: &str, error: &Error) {
        let message = error.to_string();
        self.warn(
            at,
            "records_failed",
            &[("op", op), ("pipeline", pipeline), ("error", &message)],
        );
    }

    /// spec 6.5: a pending or running pipeline gets the warm step, a final status
    /// deletes the pipeline's records.
    /// D56: a pending event of a job that existed before its pipeline's final event and was not
    /// manual then. GitLab delivered it late, the job has run, and starting a VM or a lane run
    /// for it would only cost money. A retried job is a new job created after the close.
    async fn stale(&self, event: &Value, pipeline: &str, job: &str, at: Arrival) -> bool {
        let Some(records) = &self.records else {
            return false;
        };
        let closed = match records.closed(pipeline).await {
            Ok(Some(closed)) => closed,
            Ok(None) => return false,
            Err(error) => {
                self.records_failed(at, "closed", pipeline, &error);
                return false;
            }
        };
        let created = event
            .get("build_created_at")
            .and_then(Value::as_str)
            .and_then(crate::time::parse_gitlab_time);
        !closed.manual.iter().any(|id| id == job) && created.is_some_and(|c| c < closed.at)
    }

    async fn pipeline_event(
        &self,
        headers: &[(String, String)],
        event: &Value,
        now: u64,
        at: Arrival,
    ) -> DispatchResponse {
        let project_id = event
            .get("project")
            .and_then(|project| project.get("id"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let Some(member) = self.member(project_id) else {
            return ignored("project", json!({}));
        };
        let attributes = event.get("object_attributes");
        let Some(id) = attributes.and_then(|a| a.get("id")).and_then(Value::as_u64) else {
            return reply(400, "bad_request", json!({}));
        };
        let status = attributes
            .and_then(|a| a.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let pipeline = id.to_string();
        let project = project_id.to_string();
        let names = [
            ("pipeline", pipeline.as_str()),
            ("project", project.as_str()),
        ];
        if FINAL_STATUSES.contains(&status) {
            if let Some(records) = &self.records {
                if let Err(error) = records.close(&pipeline).await {
                    self.records_failed(at, "close", &pipeline, &error);
                }
                let closed = Closed {
                    at: now,
                    manual: manual_jobs(event),
                };
                if let Err(error) = records.mark_closed(&pipeline, &closed).await {
                    self.records_failed(at, "mark_closed", &pipeline, &error);
                }
            }
            self.info(at, "closed", &names);
            return reply(200, "closed", json!({ "pipeline": id }));
        }
        if status != "pending" && status != "running" {
            return ignored("status", json!({ "status": status }));
        }
        let vm_jobs = self.vm_jobs_of(&member, event);
        if vm_jobs.is_empty() {
            return ignored("no_vm_jobs", json!({}));
        }
        let enabled = match &self.records {
            None => false,
            Some(records) => match records.open(&pipeline, &vm_jobs).await {
                Ok(true) => true,
                Ok(false) => {
                    self.info(at, "duplicate", &names);
                    return reply(200, "duplicate", json!({ "pipeline": id }));
                }
                Err(error) => {
                    self.records_failed(at, "open", &pipeline, &error);
                    false
                }
            },
        };
        let callback = self.callback(headers);
        match self
            .warm(&names, &vm_jobs, enabled, callback.as_deref(), now, at)
            .await
        {
            Ok(pools) => reply(200, "warm", json!({ "pipeline": id, "pools": pools })),
            Err(response) => response,
        }
    }

    /// spec 6.5: the jobs of a pipeline event of `member` that will go pending, under the
    /// VM runner's tag: waiting, neither manual nor `on_failure`, not own, and routed to the VM
    /// runner by the project's own lanes and `ignoreJobs` (D60, D61).
    fn vm_jobs_of(&self, member: &Member, event: &Value) -> VmJobs {
        let mut jobs = VmJobs::new();
        let config = self.config();
        let builds = event.get("builds").and_then(Value::as_array);
        for build in builds.into_iter().flatten() {
            let text = |key: &str| build.get(key).and_then(Value::as_str).unwrap_or("");
            let name = text("name");
            let counted = WAITING_STATUSES.contains(&text("status"))
                && !matches!(text("when"), "manual" | "on_failure")
                && build.get("manual").and_then(Value::as_bool) != Some(true)
                && !is_own_job(name);
            let Some(id) = build.get("id").and_then(Value::as_u64).filter(|_| counted) else {
                continue;
            };
            if matches!(self.route_of(member, name), Routed::Vm) {
                jobs.entry(config.vm.tag.clone())
                    .or_default()
                    .push(id.to_string());
            }
        }
        jobs
    }

    /// spec 6.5 for the VM runner when the pipeline has VM jobs: `disabled` without
    /// records or with `warmStart` false; `capacity` with a claim candidate, or with an intent
    /// ranked before the warm intent after the ledger's wait; `at_capacity` at `maxRuns` live
    /// VMs; else `started`, a VM with every slot free that leads its burst group. A failed
    /// capacity read or start answers 500.
    async fn warm(
        &self,
        names: &[(&str, &str); 2],
        vm_jobs: &VmJobs,
        enabled: bool,
        callback: Option<&str>,
        now: u64,
        at: Arrival,
    ) -> std::result::Result<Vec<Value>, DispatchResponse> {
        let warm_job = format!("{WARM_PREFIX}{}", names[0].1);
        let vm_pool = self.config().vm_pool();
        let pools: Vec<(&Pool, usize)> = vm_jobs
            .get(&vm_pool.tag)
            .map(Vec::len)
            .filter(|count| *count > 0)
            .map(|count| (&vm_pool, count))
            .into_iter()
            .collect();
        let mut steps = Vec::new();
        for (pool, _) in &pools {
            let step = if !enabled || !pool.warm_start() {
                Warm::Done("disabled")
            } else {
                match self.provider.capacity(&pool.tag).await {
                    Err(error) => return Err(self.warm_failed(names, pool, &error, at)),
                    Ok(None) => Warm::Done("disabled"),
                    Ok(Some(vms)) if !claim_candidates(&vms, now).is_empty() => {
                        Warm::Done("capacity")
                    }
                    Ok(Some(vms)) => {
                        let ranked = match &self.ledger {
                            None => false,
                            Some(ledger) => match ledger.record(&pool.tag, now, &warm_job).await {
                                Ok(()) => true,
                                Err(error) => {
                                    self.ledger_failed(pool, &warm_job, &error, at);
                                    false
                                }
                            },
                        };
                        Warm::Ranking { vms, ranked }
                    }
                }
            };
            steps.push(step);
        }
        let waits = steps
            .iter()
            .any(|step| matches!(step, Warm::Ranking { ranked: true, .. }));
        if waits && !self.group_wait.is_zero() {
            tokio::time::sleep(self.group_wait).await;
        }
        let mut answers = Vec::new();
        for ((pool, count), step) in pools.into_iter().zip(steps) {
            let (action, run_id) = match step {
                Warm::Done(action) => (action, None),
                Warm::Ranking { vms, ranked } => {
                    let live = vms.iter().filter(|v| v.state != VmState::Draining).count();
                    if ranked && self.warm_covered(pool, &warm_job, now, at).await {
                        ("capacity", None)
                    } else if live >= pool.max_runs as usize {
                        ("at_capacity", None)
                    } else {
                        let labels = BTreeMap::from([
                            ("project".to_string(), names[1].1.to_string()),
                            ("pipeline".to_string(), names[0].1.to_string()),
                            ("job".to_string(), warm_job.clone()),
                        ]);
                        match self.start_run(pool, labels, true, callback, 0, at).await {
                            Ok(run_id) => ("started", Some(run_id)),
                            Err(error) => return Err(self.warm_failed(names, pool, &error, at)),
                        }
                    }
                }
            };
            let count_text = count.to_string();
            let mut fields = names.to_vec();
            fields.extend([("pool", pool.tag.as_str()), ("action", action)]);
            if let Some(run_id) = &run_id {
                fields.push(("run", run_id));
            }
            fields.push(("vm_jobs", &count_text));
            self.info(at, "warm", &fields);
            let mut answer = json!({ "pool": pool.tag, "action": action, "vmJobs": count });
            if let Some(run_id) = run_id {
                answer["runId"] = run_id.into();
            }
            answers.push(answer);
        }
        Ok(answers)
    }

    /// Whether an intent of the window ranks before the warm intent `warm` just recorded, so
    /// that the VMs of the window take the pipeline's jobs (spec 6.5). A failed read counts
    /// as none, as a failed ledger falls back to starting (spec 6.3).
    async fn warm_covered(&self, pool: &Pool, warm: &str, now: u64, at: Arrival) -> bool {
        let Some(ledger) = &self.ledger else {
            return false;
        };
        let ranked = ledger
            .since(&pool.tag, now.saturating_sub(GROUP_WINDOW_S))
            .await
            .and_then(|intents| {
                rank_burst(&intents, warm, now, pool.slots)
                    .ok_or_else(|| Error::other("the ledger lacks the intent just recorded"))
            });
        match ranked {
            Ok(Burst::Leader { .. }) => false,
            Ok(_) => true,
            Err(error) => {
                self.ledger_failed(pool, warm, &error, at);
                false
            }
        }
    }

    fn ledger_failed(&self, pool: &Pool, job: &str, error: &Error, at: Arrival) {
        let message = error.to_string();
        self.warn(
            at,
            "ledger_failed",
            &[("pool", &pool.tag), ("job", job), ("error", &message)],
        );
    }

    /// A failed warm step logs `start_failed` and answers 500, as a job event does.
    fn warm_failed(
        &self,
        names: &[(&str, &str); 2],
        pool: &Pool,
        error: &Error,
        at: Arrival,
    ) -> DispatchResponse {
        let message = error.to_string();
        let mut fields = names.to_vec();
        fields.extend([("pool", pool.tag.as_str()), ("error", message.as_str())]);
        self.error(at, "start_failed", &fields);
        reply(500, "error", json!({}))
    }

    /// spec 6.3: records the intent, waits for the rest of the burst and ranks the intents of
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

    /// Starts a run; `taken` slots of a new VM are already assigned (spec 6.3). A retryable
    /// failure is retried once after [`START_RETRY_PAUSE`] when the retry begins within
    /// [`START_RETRY_WITHIN`] of the request's arrival (spec 6.6).
    async fn start(
        &self,
        pool: &Pool,
        labels: BTreeMap<String, String>,
        vm: bool,
        callback: Option<&str>,
        taken: u32,
        at: Arrival,
    ) -> Result<Decision> {
        let run_id = self
            .start_run(pool, labels, vm, callback, taken, at)
            .await?;
        Ok(Decision::Started { run_id, taken })
    }

    /// See [`Dispatcher::start`]; answers the run id.
    async fn start_run(
        &self,
        pool: &Pool,
        labels: BTreeMap<String, String>,
        vm: bool,
        callback: Option<&str>,
        taken: u32,
        at: Arrival,
    ) -> Result<String> {
        // D60: a member's lane run takes the job with the runner of the member's own lane.
        let config = self.config();
        let token = match pool.project {
            Some(project) if project != config.host_project() => Some(
                self.member(project)
                    .and_then(|member| {
                        member
                            .lanes
                            .into_iter()
                            .find(|lane| lane.tag == pool.runner_tag())
                    })
                    .map(|lane| lane.runner_token)
                    .ok_or_else(|| Error::other(format!("no runner token for {}", pool.tag)))?,
            ),
            _ => None,
        };
        let mut env = run_env(&config, &self.secrets, pool, vm, token.as_deref())?;
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
        Ok(run_id)
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
                let answer = self
                    .report_answer(run, &report, self.callback(headers).as_deref(), now, at)
                    .await;
                let (free, next) = (report.free.to_string(), answer.next_report_s.to_string());
                self.info(
                    at,
                    "reported",
                    &[
                        ("run", run),
                        ("state", report.state.as_str()),
                        ("free", &free),
                        ("next_report_s", &next),
                    ],
                );
                let mut body = json!({ "nextReportS": answer.next_report_s });
                if !answer.runners.is_empty() {
                    body["runners"] = json!(answer.runners);
                }
                reply(200, "reported", body)
            }
            Err(error) => {
                let message = error.to_string();
                self.error(at, "report_failed", &[("run", run), ("error", &message)]);
                reply(500, "error", json!({}))
            }
        }
    }

    /// spec 6.2 and 6.4, after the provider stored the report of `run`: the member runners
    /// of the VM runner (spec 8.5), the start rule over its queue, and the seconds until the
    /// next report. Without a schedule nothing queues.
    async fn report_answer(
        &self,
        run: &str,
        report: &VmReport,
        callback: Option<&str>,
        now: u64,
        at: Arrival,
    ) -> VmReportAnswer {
        let busy = report.state == VmState::Busy;
        let mut quiet = VmReportAnswer {
            runners: Vec::new(),
            next_report_s: next_report_s(false, busy),
        };
        let pool = self.config().vm_pool();
        let members = !self.member_runners(&pool.tag).is_empty();
        let schedule = self.schedule.as_deref();
        if schedule.is_none() && !members {
            return quiet;
        }
        let Some(vms) = self
            .vms_of(&pool, at)
            .await
            .filter(|vms| vms.iter().any(|vm| vm.run_id == run))
        else {
            return quiet;
        };
        quiet.runners = self.member_runners(&pool.tag);
        let Some(schedule) = schedule else {
            return quiet;
        };
        let queued = match self.live_queue(schedule, &pool.tag, now).await {
            Ok(queued) => queued,
            Err((op, error)) => {
                self.schedule_failed(at, op, &pool.tag, &error);
                return quiet;
            }
        };
        self.reevaluate(schedule, &pool, &vms, &queued, now, callback, at)
            .await;
        VmReportAnswer {
            runners: quiet.runners,
            next_report_s: next_report_s(!queued.is_empty(), busy),
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
    /// Rides on the VM its burst leader starts (spec 6.3).
    Grouped {
        leader: String,
    },
    /// The job's intent was already recorded by an earlier event (spec 6.3).
    Duplicate,
    /// The job waits for a paid slot, expected in `wait_s` seconds (spec 6.4).
    Queued {
        wait_s: Option<u64>,
    },
}

/// The start rule's answer for a pending VM job (spec 6.4); `wait_s` is the job's expected
/// wait, which a burst path that ends at `maxRuns` answers with.
enum QueueFirst {
    Queued { wait_s: Option<u64> },
    Burst { wait_s: Option<u64> },
}

/// A VM pool of the warm step between the capacity read and the read of the burst window.
enum Warm {
    Done(&'static str),
    /// No claim candidate; `ranked` when the warm intent is in the ledger.
    Ranking {
        vms: Vec<VmCapacity>,
        ranked: bool,
    },
}

/// Seconds after its last report that a VM still counts as capacity: two heartbeats and a
/// minute, so one lost report does not matter but a hung VM stops absorbing claims.
pub const CAPACITY_FRESH_S: u64 = 2 * vm::HEARTBEAT_S + 60;

/// spec 6.3: a VM takes a claim when it has a free slot, was created more than
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

/// VMs that are not draining.
fn live_count(vms: &[VmCapacity]) -> usize {
    vms.iter().filter(|v| v.state != VmState::Draining).count()
}

/// spec 6.4, start rule step 3: the free slots of VMs in state `starting` younger than
/// [`vm::SWEEP_STARTING_S`], and of the claim candidates, whose free slots take a queued job as
/// soon as GitLab hands it out.
fn ready_slots(vms: &[VmCapacity], now: u64) -> u32 {
    vms.iter()
        .filter(|v| {
            let starting = v.state == VmState::Starting
                && now.saturating_sub(v.created_at) < vm::SWEEP_STARTING_S;
            starting || is_candidate(v, now)
        })
        .map(|v| v.free)
        .sum()
}

/// The VM `job` claims (spec 6.3) and the free slots of every candidate.
fn claim_target<'a>(vms: &'a [VmCapacity], job: &str, now: u64) -> Option<(&'a VmCapacity, u64)> {
    let candidates = claim_candidates(vms, now);
    let target = spread(&candidates, job)?;
    Some((target, candidates.iter().map(|v| u64::from(v.free)).sum()))
}

/// spec 6.3: among the candidates with the most free slots, sorted by run id, the one at
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

    /// The warm intent of a pipeline (spec 6.5).
    fn is_warm(&self) -> bool {
        matches!(self, JobKey::Text(text) if text.starts_with(WARM_PREFIX))
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

/// A job's place in its burst (spec 6.3, 6.5).
#[derive(Debug, PartialEq, Eq)]
enum Burst {
    /// The job's earliest intent is older than this event.
    Duplicate,
    /// The job takes an open position of the current group and rides on its leader's VM.
    Follower { leader: String },
    /// Group `group` starts a VM with `taken` slots assigned: the leader and its followers, or
    /// none for a warm intent.
    Leader { group: usize, taken: u32 },
    /// A warm intent behind other intents: the window's VMs take the pipeline's jobs.
    Covered,
}

/// Ranks `job` among the intents of the window `[now - GROUP_WINDOW_S, ..]`: each job's
/// earliest intent, ordered by time, then job id, numbers before other ids. A job takes an
/// open position of the current group, or leads a new group of `slots` positions and takes
/// one. A warm intent first in the window leads group 0 and takes no position; behind other
/// intents it is covered and takes no part. `None` when the job has no intent there.
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
    let slots = slots.max(1) as usize;
    // Per group its leader and the positions still open.
    let mut groups: Vec<(&JobKey, usize)> = Vec::new();
    let mut placed = None;
    for (rank, (_, key)) in order.iter().enumerate() {
        let place = if key.is_warm() {
            if rank == 0 {
                groups.push((key, slots));
                Burst::Leader { group: 0, taken: 0 }
            } else {
                Burst::Covered
            }
        } else {
            match groups.last_mut() {
                Some((leader, open)) if *open > 0 => {
                    *open -= 1;
                    Burst::Follower {
                        leader: leader.to_string(),
                    }
                }
                _ => {
                    groups.push((key, slots - 1));
                    Burst::Leader {
                        group: groups.len() - 1,
                        taken: 0,
                    }
                }
            }
        };
        if *key == mine {
            placed = Some(place);
        }
    }
    match placed? {
        Burst::Leader { group, .. } if !mine.is_warm() => {
            let taken = slots - groups[group].1;
            Some(Burst::Leader {
                group,
                taken: u32::try_from(taken).unwrap_or(u32::MAX),
            })
        }
        place => Some(place),
    }
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

/// spec 5.2: a body gets `ms`, the whole milliseconds since the request arrived.
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
    use crate::ledger::{MemoryLedger, MemoryPipelineRecords, MemorySchedule};
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
                  "vm": { "image": "img/node" },
                  "lanes": [
                    { "tag": "ci", "image": "img/node", "jobs": ["test"] },
                    { "tag": "build", "image": "img/kaniko", "jobs": ["build"], "maxRuns": 1 }
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
                ("vm".to_string(), "glrt-vm".to_string()),
                ("ci".to_string(), "glrt-ci".to_string()),
                ("build".to_string(), "glrt-build".to_string()),
            ]),
            dispatch: None,
            cache: Some(KeyPair {
                access_key: "AK".into(),
                secret_key: "SK".into(),
            }),
            vm_secret: Some("vm-secret".into()),
            sweep_token: None,
            dispatcher_env: BTreeMap::new(),
            fleet_key: None,
        })
    }

    /// A job event of a job named and staged `stage`; lanes take jobs by name (D60).
    fn event(stage: &str, status: &str, project: u64, job: u64) -> Vec<u8> {
        json!({
            "object_kind": "build", "build_id": job, "build_name": stage, "build_stage": stage,
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
            (b"{\"object_kind\":\"push\"}".to_vec(), "not_job_event"),
            (b"{\"object_kind\":\"pipeline\"}".to_vec(), "project"),
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
        assert_eq!(response.body["pool"], "7-build");
        assert!(
            response.body["runId"]
                .as_str()
                .unwrap()
                .starts_with("fake-")
        );
        let started = recording.started.lock().unwrap();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].pool.tag, "7-build");
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
                .any(|l| l.starts_with("info started") && l.contains("pool=\"7-build\""))
        );
        assert!(!lines.iter().any(|l| l.contains("glrt-")));
    }

    /// D63: a job that no lane takes goes to the VM runner; stages route nothing.
    #[tokio::test(start_paused = true)]
    async fn a_job_no_lane_takes_goes_to_the_vm_runner_and_headers_ignore_case() {
        let (d, recording, _) = setup(false).await;
        let headers = vec![("x-gitlab-token".to_string(), "hook-secret".to_string())];
        let mut body: Value = serde_json::from_slice(&event("build", "pending", 7, 1)).unwrap();
        body["build_name"] = "deploy".into();
        let response = d.handle(&headers, body.to_string().as_bytes()).await;
        assert_eq!(response.body["result"], "started", "{}", response.body);
        assert_eq!(response.body["pool"], "vm");
        assert_eq!(
            recording.started.lock().unwrap()[0].env["RUNNER_TOKEN"],
            "glrt-vm"
        );
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
        assert_eq!(second.body["pool"], "7-build");
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
        let pool = config.lanes[0].pool(7);
        assert!(run_env(&config, &Secrets::default(), &pool, false, None).is_err());
        let mut partial = Secrets::default();
        partial.runner_tokens.insert("ci".into(), "t".into());
        assert!(
            run_env(&config, &partial, &pool, false, None)
                .unwrap_err()
                .to_string()
                .contains("no cache key")
        );
        let mut no_cache = (*config).clone();
        no_cache.cache.kind = CacheType::None;
        assert_eq!(
            run_env(&no_cache, &partial, &pool, false, None).unwrap()["RUNNER_TOKEN"],
            "t"
        );
        assert!(
            run_env(&no_cache, &partial, &pool, true, None)
                .unwrap_err()
                .to_string()
                .contains("no VM secret for pool 7-ci")
        );
        partial.vm_secret = Some("vm-secret".into());
        let one_job = run_env(&no_cache, &partial, &pool, false, None).unwrap();
        assert!(!one_job.contains_key(keys::VM_SECRET), "{one_job:?}");
        assert_eq!(
            run_env(&no_cache, &partial, &pool, true, None).unwrap()[keys::VM_SECRET],
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
                        Burst::Duplicate | Burst::Covered => panic!("{job} of {n} jobs"),
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
                  "vm": { "image": "img/node", "slots": 2, "maxRuns": 2 },
                  "lanes": [ { "tag": "jobs", "image": "img/kaniko", "jobs": ["build", "compile"] } ],
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
            dispatcher_env: BTreeMap::new(),
            fleet_key: None,
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
    async fn lanes_keep_counting_runs_and_never_get_the_vm_secret() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = vm_setup_with(vm_secrets(), Some(ledger.clone())).await;
        let response = s.pending("build", 1).await;
        assert_eq!(response.body["result"], "started");
        assert_eq!(response.body["pool"], "7-jobs");
        assert_eq!(s.taken(), [1]);
        assert!(ledger.all().is_empty(), "lanes skip the ledger");
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
        assert_eq!(
            response.body,
            json!({ "result": "reported", "nextReportS": 30, "ms": 0 }),
            "a busy VM reports every 30 s"
        );
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

    fn lane_config() -> Arc<Config> {
        Arc::new(
            parse_config(
                r#"{
                  "name": "demo",
                  "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
                  "host": { "type": "fake" }, "provider": { "type": "fake" },
                  "vm": { "image": "ubuntu:24.04", "slots": 2, "maxRuns": 2 },
                  "lanes": [
                    { "tag": "node-24", "image": "node:24-bookworm-slim",
                      "jobs": ["lint", "unit-*"], "maxRuns": 2 },
                    { "tag": "rust-1", "image": "rust:1.96.0-bookworm", "jobs": ["clippy"] }
                  ],
                  "cache": { "type": "s3" }
                }"#,
            )
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
    async fn a_lane_job_never_takes_a_vm_slot() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = setup_of(lane_config(), lane_secrets(), Some(ledger.clone())).await;
        idle_vm(&s, 1, 2).await;
        let lane = s.named_at("lint", 2, T0 + 60).await;
        assert_eq!(lane.body["result"], "started", "{}", lane.body);
        assert_eq!(lane.body["pool"], "7-node-24");
        assert_eq!(s.vms().await[0].free, 2);
        assert!(s.recording.claimed.lock().unwrap().is_empty());
        assert_eq!(
            *s.recording.asked.lock().unwrap(),
            ["vm", "7-node-24"],
            "D62: no read of the VM runner"
        );
        assert_eq!(s.started_pools(), ["vm", "7-node-24"]);
        assert_eq!(ledger.all().len(), 1, "only the VM job's intent");
    }

    #[tokio::test(start_paused = true)]
    async fn a_lane_job_without_a_claim_candidate_starts_its_lane() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = setup_of(lane_config(), lane_secrets(), Some(ledger.clone())).await;
        let first = s.named_at("lint", 1, T0).await;
        assert_eq!(first.body["result"], "started", "{}", first.body);
        assert_eq!(first.body["pool"], "7-node-24");
        assert!(run_of(&first).starts_with("fake-"));
        idle_vm(&s, 2, 2).await;
        let in_window = s.named_at("unit-api", 3, T0 + GROUP_WINDOW_S).await;
        assert_eq!(
            in_window.body["result"], "started",
            "a VM inside the window offers no slot"
        );
        assert_eq!(in_window.body["pool"], "7-node-24");
        assert_eq!(s.started_pools(), ["7-node-24", "vm", "7-node-24"]);
        assert!(s.recording.claimed.lock().unwrap().is_empty());
        assert_eq!(ledger.all().len(), 1, "lanes never use the ledger");
        let started = s.recording.started.lock().unwrap();
        assert_eq!(started[0].env["RUNNER_TOKEN"], "glrt-node-24");
        assert!(!started[0].env.contains_key(keys::VM_SECRET));
        assert!(
            s.lines()
                .iter()
                .any(|l| l.starts_with("info started") && l.contains(r#"pool="7-node-24""#))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_lane_job_never_claims_an_idle_vm_slot() {
        let s = setup_of(lane_config(), lane_secrets(), None).await;
        idle_vm(&s, 1, 2).await;
        let response = s.named_at("clippy", 2, T0 + 60).await;
        assert_eq!(response.body["result"], "started", "{}", response.body);
        assert_eq!(response.body["pool"], "7-rust-1");
        assert_eq!(s.vms().await[0].free, 2);
        assert!(s.recording.claimed.lock().unwrap().is_empty());
        assert_eq!(*s.recording.asked.lock().unwrap(), ["vm", "7-rust-1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_lane_at_max_runs_answers_at_capacity() {
        let ledger = Arc::new(MemoryLedger::new());
        let s = setup_of(lane_config(), lane_secrets(), Some(ledger.clone())).await;
        for job in 1..=2 {
            assert_eq!(s.named_at("lint", job, T0).await.body["result"], "started");
        }
        let third = s.named_at("unit-api", 3, T0).await;
        assert_eq!(third.status, 200);
        assert_eq!(
            third.body,
            json!({ "result": "at_capacity", "pool": "7-node-24", "ms": 0 })
        );
        assert!(s.lines().iter().any(|l| l.starts_with("info at_capacity")
            && l.contains(r#"pool="7-node-24""#)
            && l.contains(r#"live="2""#)));
        assert!(ledger.all().is_empty());
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
                750,
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
        assert_eq!(failed.body, json!({ "result": "error", "ms": 750 }));
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
                ("started", Some(750)),
                ("reported", Some(250)),
                ("swept", Some(250)),
                ("terminated", Some(250)),
                ("start_failed", Some(750)),
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
        let s = setup_of(lane_config(), lane_secrets(), None).await;
        s.recording.fail_starts([Error::retryable("scaleway: 503")]);
        let response = s.named_at("lint", 1, T0).await;
        assert_eq!(response.body["result"], "started", "{}", response.body);
        assert_eq!(response.body["pool"], "7-node-24");
        assert_eq!(response.body["ms"], 300);
        assert_eq!(s.recording.start_calls(), 2);
        assert_eq!(s.started_pools(), ["7-node-24"]);
    }

    const W: &str = "warm:9";

    #[test]
    fn a_warm_intent_first_in_the_window_leads_a_group_of_free_slots() {
        let leader = |group| Some(Burst::Leader { group, taken: 0 });
        let follows = |leader: &str| {
            Some(Burst::Follower {
                leader: leader.into(),
            })
        };

        let warm_then_two = [intent(NOW, W), intent(NOW + 1, "1"), intent(NOW + 1, "2")];
        assert_eq!(rank_burst(&warm_then_two, W, NOW, 2), leader(0));
        assert_eq!(rank_burst(&warm_then_two, "1", NOW + 1, 2), follows(W));
        assert_eq!(
            rank_burst(&warm_then_two, "2", NOW + 1, 2),
            follows(W),
            "one VM: the warm VM takes both jobs"
        );

        let warm_then_three = [
            intent(NOW, W),
            intent(NOW + 1, "1"),
            intent(NOW + 1, "2"),
            intent(NOW + 1, "3"),
        ];
        assert_eq!(
            rank_burst(&warm_then_three, "3", NOW + 1, 2),
            Some(Burst::Leader { group: 1, taken: 1 }),
            "two VMs: the warm VM and one for the third job"
        );

        let two_then_warm = [intent(NOW, "1"), intent(NOW, "2"), intent(NOW, W)];
        assert_eq!(
            rank_burst(&two_then_warm, "1", NOW, 2),
            Some(Burst::Leader { group: 0, taken: 2 })
        );
        assert_eq!(rank_burst(&two_then_warm, "2", NOW, 2), follows("1"));
        assert_eq!(
            rank_burst(&two_then_warm, W, NOW, 2),
            Some(Burst::Covered),
            "one VM: a job of the same second ranks first, the warm intent takes no part"
        );
        let later = [
            intent(NOW, "1"),
            intent(NOW, "2"),
            intent(NOW, W),
            intent(NOW + 1, "3"),
        ];
        assert_eq!(
            rank_burst(&later, "3", NOW + 1, 2),
            Some(Burst::Leader { group: 1, taken: 1 }),
            "a covered warm intent opens no group"
        );

        let one_then_warm = [intent(NOW - 20, "1"), intent(NOW, W), intent(NOW + 1, "2")];
        assert_eq!(rank_burst(&one_then_warm, W, NOW, 2), Some(Burst::Covered));
        assert_eq!(
            rank_burst(&one_then_warm, "2", NOW + 1, 2),
            follows("1"),
            "the job fills the open slot of the window's VM"
        );

        let two_pipelines = [intent(NOW, "warm:8"), intent(NOW, W), intent(NOW + 1, "1")];
        assert_eq!(rank_burst(&two_pipelines, "warm:8", NOW, 1), leader(0));
        assert_eq!(rank_burst(&two_pipelines, W, NOW, 1), Some(Burst::Covered));
        assert_eq!(
            rank_burst(&two_pipelines, "1", NOW + 1, 1),
            follows("warm:8")
        );

        let left = [intent(NOW - GROUP_WINDOW_S - 1, "1"), intent(NOW, W)];
        assert_eq!(
            rank_burst(&left, W, NOW, 2),
            leader(0),
            "an intent that left the window"
        );
        assert_eq!(
            rank_burst(&[intent(NOW - 5, W)], W, NOW, 2),
            Some(Burst::Duplicate)
        );
    }

    fn records_config(extra: &str) -> Arc<Config> {
        Arc::new(
            parse_config(&format!(
                r#"{{
                  "name": "demo",
                  "gitlab": {{ "url": "https://gitlab.example", "projectIds": [7] }},
                  "host": {{ "type": "fake" }}, "provider": {{ "type": "fake" }},
                  "vm": {{ "image": "img/node", "slots": 2, "maxRuns": 2 }},
                  "lanes": [ {{ "tag": "jobs", "image": "img/kaniko", "jobs": ["build", "compile"] }} ],
                  {extra}
                  "cache": {{ "type": "s3" }}
                }}"#
            ))
            .unwrap(),
        )
    }

    async fn warm_setup(
        config: Arc<Config>,
        secrets: Secrets,
        ledger: Option<Arc<dyn Ledger>>,
        records: Option<Arc<dyn PipelineRecords>>,
    ) -> VmSetup {
        let mut s = setup_of(config, secrets, ledger).await;
        s.dispatcher = s.dispatcher.with_records(records);
        s
    }

    fn build(id: u64, name: &str, stage: &str) -> Value {
        json!({
            "id": id, "name": name, "stage": stage, "status": "created",
            "when": "on_success", "manual": false, "allow_failure": false
        })
    }

    fn pipeline_body(id: Value, status: &str, project: u64, builds: &[Value]) -> Vec<u8> {
        json!({
            "object_kind": "pipeline",
            "object_attributes": { "id": id, "status": status },
            "project": { "id": project },
            "builds": builds
        })
        .to_string()
        .into_bytes()
    }

    fn pipeline_headers() -> Vec<(String, String)> {
        vec![
            ("X-Gitlab-Token".to_string(), "hook-secret".to_string()),
            ("X-Gitlab-Event".to_string(), "Pipeline Hook".to_string()),
        ]
    }

    impl VmSetup {
        async fn pipeline_at(
            &self,
            id: u64,
            status: &str,
            builds: &[Value],
            now: u64,
        ) -> DispatchResponse {
            let body = pipeline_body(json!(id), status, 7, builds);
            self.dispatcher
                .handle_request("POST", "/", &pipeline_headers(), &body, now)
                .await
        }

        /// A pending job event of job `job` named `name` in pipeline `pipeline`.
        async fn job_at(
            &self,
            name: &str,
            stage: &str,
            job: u64,
            pipeline: u64,
            now: u64,
        ) -> DispatchResponse {
            let body = json!({
                "object_kind": "build", "build_id": job, "build_name": name,
                "build_stage": stage, "build_status": "pending",
                "pipeline_id": pipeline, "project_id": 7
            });
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
    }

    fn vm_jobs(pairs: &[(&str, &[&str])]) -> Option<VmJobs> {
        Some(
            pairs
                .iter()
                .map(|(pool, jobs)| {
                    (
                        pool.to_string(),
                        jobs.iter().map(|j| j.to_string()).collect(),
                    )
                })
                .collect(),
        )
    }

    /// D56: after its pipeline's final event, a pending event of a job that existed before the
    /// close starts nothing; a manual job played later and a retried job still start.
    #[tokio::test(start_paused = true)]
    async fn late_pending_events_of_a_closed_pipeline_start_nothing() {
        let records = Arc::new(MemoryPipelineRecords::new());
        let s = warm_setup(
            records_config(""),
            vm_secrets(),
            None,
            Some(records.clone()),
        )
        .await;
        let now = T0 + 600;
        s.set_clock(now);
        let mut manual = build(4, "release", "deploy");
        manual["status"] = "manual".into();
        let closed = s
            .pipeline_at(9, "success", &[build(2, "unit", "test"), manual], now)
            .await;
        assert_eq!(closed.body["result"], "closed");
        assert_eq!(
            records.closed("9").await.unwrap(),
            Some(Closed {
                at: now,
                manual: vec!["4".to_string()]
            })
        );
        let pending = |job: u64, created: &str| {
            json!({
                "object_kind": "build", "build_id": job, "build_name": "unit",
                "build_stage": "test", "build_status": "pending", "pipeline_id": 9,
                "project_id": 7, "build_created_at": created
            })
            .to_string()
        };
        let before = crate::time::rfc3339(T0);
        let after = crate::time::rfc3339(now + 5);
        let late = s
            .dispatcher
            .handle_request(
                "POST",
                "/",
                &headers("hook-secret"),
                pending(2, &before).as_bytes(),
                now + 2000,
            )
            .await;
        assert_eq!(
            late.body,
            json!({ "result": "ignored", "reason": "pipeline_closed", "pipeline": "9", "ms": 0 })
        );
        assert!(s.recording.started.lock().unwrap().is_empty());
        assert!(s.lines().iter().any(|l| l.starts_with("info stale ")));

        for (job, created) in [(4, before.as_str()), (5, after.as_str())] {
            let served = s
                .dispatcher
                .handle_request(
                    "POST",
                    "/",
                    &headers("hook-secret"),
                    pending(job, created).as_bytes(),
                    now + 10,
                )
                .await;
            assert_ne!(
                served.body["reason"], "pipeline_closed",
                "job {job}: {}",
                served.body
            );
        }
        assert_eq!(
            s.recording.started.lock().unwrap().len(),
            2,
            "both start capacity"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_pipeline_event_starts_one_warm_vm_that_its_vm_jobs_ride_on() {
        let ledger = Arc::new(MemoryLedger::new());
        let records = Arc::new(MemoryPipelineRecords::new());
        let s = warm_setup(
            records_config(""),
            vm_secrets(),
            Some(ledger.clone()),
            Some(records.clone()),
        )
        .await;
        let now = T0 + 60;
        s.set_clock(now);
        let mut manual = build(4, "release", "deploy");
        manual["status"] = "manual".into();
        manual["when"] = "manual".into();
        manual["manual"] = true.into();
        let mut on_failure = build(6, "cleanup", "deploy");
        on_failure["when"] = "on_failure".into();
        let mut done = build(7, "lint", "test");
        done["status"] = "success".into();
        let mut pending = build(8, "smoke", "test");
        pending["status"] = "pending".into();
        let builds = [
            build(1, "compile", "build"),
            build(2, "unit", "test"),
            build(3, "e2e", "deploy"),
            manual,
            build(5, "ciburst-check", ".pre"),
            on_failure,
            done,
            pending,
        ];
        let warm = s.pipeline_at(9, "pending", &builds, now).await;
        assert_eq!(warm.status, 200, "{}", warm.body);
        let run = warm.body["pools"][0]["runId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert_eq!(
            warm.body,
            json!({
                "result": "warm", "pipeline": 9, "ms": 0,
                "pools": [{ "pool": "vm", "action": "started", "runId": run, "vmJobs": 3 }]
            })
        );
        assert_eq!(s.taken(), [0], "every slot of the warm VM is free");
        assert_eq!(s.vms().await[0].free, 2);
        let started = s.recording.started.lock().unwrap()[0].clone();
        assert_eq!(started.labels["job"], W);
        assert_eq!(started.labels["pipeline"], "9");
        assert_eq!(started.labels["project"], "7");
        assert_eq!(started.env[keys::VM_SECRET], VM_SECRET);
        assert_eq!(
            ledger.all(),
            [(
                "vm".to_string(),
                Intent {
                    at: now,
                    job: W.to_string()
                }
            )]
        );
        assert_eq!(
            records.vm_jobs("9").await.unwrap(),
            vm_jobs(&[("vm", &["2", "3", "8"])])
        );
        assert!(s.lines().contains(&format!(
            r#"info warm pipeline="9" project="7" pool="vm" action="started" run="{run}" vm_jobs="3" ms="0""#
        )));

        let unit = s.job_at("unit", "test", 2, 9, now + 1).await;
        assert_eq!(
            unit.body,
            grouped(W),
            "a stage 1 VM job rides on the warm VM"
        );
        let again = s.pipeline_at(9, "running", &builds, now + 2).await;
        assert_eq!(
            again.body,
            json!({ "result": "duplicate", "pipeline": 9, "ms": 0 })
        );
        assert!(
            s.lines()
                .contains(&r#"info duplicate pipeline="9" project="7" ms="0""#.to_string())
        );
        let e2e = s.job_at("e2e", "deploy", 3, 9, now + 60).await;
        assert_eq!(
            e2e.body,
            json!({ "result": "claimed", "pool": "vm", "runId": run, "ms": 0 }),
            "a stage 2 VM job claims the warm VM"
        );
        assert_eq!(s.taken(), [0], "one VM for the pipeline");

        let closed = s.pipeline_at(9, "success", &builds, now + 300).await;
        assert_eq!(
            closed.body,
            json!({ "result": "closed", "pipeline": 9, "ms": 0 })
        );
        assert_eq!(records.vm_jobs("9").await.unwrap(), None);
        assert!(
            s.lines()
                .contains(&r#"info closed pipeline="9" project="7" ms="0""#.to_string())
        );
    }

    fn warm_pools(response: &DispatchResponse) -> Value {
        response.body["pools"].clone()
    }

    #[tokio::test(start_paused = true)]
    async fn the_burst_window_and_claim_candidates_take_the_pipeline_instead() {
        let ledger = Arc::new(MemoryLedger::new());
        let records = Arc::new(MemoryPipelineRecords::new());
        let s = warm_setup(
            records_config(""),
            vm_secrets(),
            Some(ledger.clone()),
            Some(records.clone()),
        )
        .await;
        let first = s.pending_at("test", 1, T0).await;
        assert_eq!(first.body["result"], "started");
        let warm = s
            .pipeline_at(9, "pending", &[build(2, "unit", "test")], T0 + 5)
            .await;
        assert_eq!(
            warm.body,
            json!({
                "result": "warm", "pipeline": 9, "ms": 0,
                "pools": [{ "pool": "vm", "action": "capacity", "vmJobs": 1 }]
            }),
            "a job event of the window ranks before the warm intent"
        );
        assert!(s.lines().contains(
            &r#"info warm pipeline="9" project="7" pool="vm" action="capacity" vm_jobs="1" ms="0""#
                .to_string()
        ));
        assert_eq!(
            s.job_at("unit", "test", 2, 9, T0 + 6).await.body,
            grouped("1"),
            "the covered warm intent leaves the open slot to the job"
        );
        assert_eq!(s.taken(), [1]);

        s.report(&run_of(&first), VmState::Busy, 1).await;
        let intents = ledger.all().len();
        let warm = s
            .pipeline_at(10, "pending", &[build(12, "unit", "test")], T0 + 60)
            .await;
        assert_eq!(
            warm_pools(&warm),
            json!([{ "pool": "vm", "action": "capacity", "vmJobs": 1 }]),
            "a claim candidate"
        );
        assert_eq!(
            ledger.all().len(),
            intents,
            "candidates come before the ledger"
        );
        assert_eq!(s.taken(), [1]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_warm_step_respects_max_runs_warm_start_and_missing_records() {
        let records = Arc::new(MemoryPipelineRecords::new());
        let s = warm_setup(
            records_config(""),
            vm_secrets(),
            None,
            Some(records.clone()),
        )
        .await;
        for job in [1, 2] {
            let run = run_of(&s.pending_at("test", job, T0).await);
            s.report(&run, VmState::Busy, 0).await;
        }
        let full = s
            .pipeline_at(9, "pending", &[build(3, "unit", "test")], T0 + 60)
            .await;
        assert_eq!(
            warm_pools(&full),
            json!([{ "pool": "vm", "action": "at_capacity", "vmJobs": 1 }])
        );
        assert_eq!(s.taken(), [1, 1]);

        let ledger = Arc::new(MemoryLedger::new());
        let mut config = (*records_config("")).clone();
        config.vm.warm_start = Some(false);
        let cold = Dispatcher::new(
            Arc::new(config),
            s.recording.clone(),
            Arc::new(vm_secrets()),
            Log::silent(),
        )
        .with_ledger(Some(ledger.clone()))
        .with_records(Some(records.clone()));
        let response = cold
            .handle_request(
                "POST",
                "/",
                &pipeline_headers(),
                &pipeline_body(json!(11), "pending", 7, &[build(1, "unit", "test")]),
                T0,
            )
            .await;
        assert_eq!(
            warm_pools(&response),
            json!([{ "pool": "vm", "action": "disabled", "vmJobs": 1 }])
        );
        assert_eq!(
            records.vm_jobs("11").await.unwrap(),
            vm_jobs(&[("vm", &["1"])]),
            "the record still serves the reservation"
        );

        let bare = vm_setup_with(vm_secrets(), Some(ledger.clone())).await;
        let response = bare
            .pipeline_at(12, "pending", &[build(1, "unit", "test")], T0)
            .await;
        assert_eq!(
            response.body,
            json!({
                "result": "warm", "pipeline": 12, "ms": 0,
                "pools": [{ "pool": "vm", "action": "disabled", "vmJobs": 1 }]
            }),
            "without records"
        );
        let again = bare
            .pipeline_at(12, "running", &[build(1, "unit", "test")], T0)
            .await;
        assert_eq!(again.body["result"], "warm");
        assert!(bare.recording.started.lock().unwrap().is_empty());
        assert!(bare.recording.asked.lock().unwrap().is_empty());
        assert!(ledger.all().is_empty());
        assert_eq!(s.taken(), [1, 1]);
    }

    #[tokio::test(start_paused = true)]
    async fn pipeline_events_answer_by_project_and_status() {
        let records = Arc::new(MemoryPipelineRecords::new());
        let s = warm_setup(
            records_config(""),
            vm_secrets(),
            None,
            Some(records.clone()),
        )
        .await;
        let unit = [build(1, "unit", "test")];
        let dispatcher = &s.dispatcher;
        let post = |body: Vec<u8>| async move {
            dispatcher
                .handle_request("POST", "/", &pipeline_headers(), &body, T0)
                .await
        };
        let other = post(pipeline_body(json!(9), "pending", 8, &unit)).await;
        assert_eq!(
            other.body,
            json!({ "result": "ignored", "reason": "project", "ms": 0 })
        );
        let wrong_token = s
            .dispatcher
            .handle_request(
                "POST",
                "/",
                &headers("nope"),
                &pipeline_body(json!(9), "pending", 7, &unit),
                T0,
            )
            .await;
        assert_eq!(wrong_token.status, 401);
        for id in [json!(null), json!("9"), json!(-1)] {
            let bad = post(pipeline_body(id.clone(), "pending", 7, &unit)).await;
            assert_eq!(bad.status, 400, "{id}");
            assert_eq!(bad.body, json!({ "result": "bad_request", "ms": 0 }));
        }
        for status in ["created", "manual", "scheduled", "waiting_for_resource", ""] {
            let response = post(pipeline_body(json!(9), status, 7, &unit)).await;
            assert_eq!(
                response.body,
                json!({ "result": "ignored", "reason": "status", "status": status, "ms": 0 })
            );
        }
        let lane_only = [build(1, "compile", "build")];
        let mut manual = build(2, "unit", "test");
        manual["when"] = "manual".into();
        let mut flagged = build(3, "unit", "test");
        flagged["manual"] = true.into();
        let mut running = build(4, "unit", "test");
        running["status"] = "running".into();
        for builds in [
            &lane_only[..],
            &[manual][..],
            &[flagged][..],
            &[running][..],
            &[][..],
        ] {
            let response = post(pipeline_body(json!(9), "pending", 7, builds)).await;
            assert_eq!(
                response.body,
                json!({ "result": "ignored", "reason": "no_vm_jobs", "ms": 0 })
            );
        }
        assert_eq!(records.vm_jobs("9").await.unwrap(), None, "nothing written");
        for status in ["success", "failed", "canceled", "skipped"] {
            let response = post(pipeline_body(json!(9), status, 7, &unit)).await;
            assert_eq!(
                response.body,
                json!({ "result": "closed", "pipeline": 9, "ms": 0 })
            );
        }
        assert!(s.recording.started.lock().unwrap().is_empty());
        assert!(s.recording.asked.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn ignore_jobs_start_nothing() {
        let records = Arc::new(MemoryPipelineRecords::new());
        let s = warm_setup(
            records_config(r#""ignoreJobs": ["pages", "deploy-*"],"#),
            vm_secrets(),
            None,
            Some(records.clone()),
        )
        .await;
        for (name, stage) in [("pages", "test"), ("deploy-prod", "build")] {
            let response = s.job_at(name, stage, 1, 9, T0).await;
            assert_eq!(response.status, 200);
            assert_eq!(
                response.body,
                json!({ "result": "ignored", "reason": "ignore_jobs", "ms": 0 })
            );
        }
        let response = s
            .pipeline_at(
                9,
                "pending",
                &[build(1, "pages", "test"), build(2, "deploy-prod", "test")],
                T0,
            )
            .await;
        assert_eq!(response.body["reason"], "no_vm_jobs");
        let own = s.job_at("ciburst-setup", "test", 3, 9, T0).await;
        assert_eq!(own.body["reason"], "own_job");
        assert!(s.recording.started.lock().unwrap().is_empty());
        assert!(s.recording.asked.lock().unwrap().is_empty());
        assert_eq!(records.vm_jobs("9").await.unwrap(), None);
        assert_eq!(
            s.job_at("unit", "test", 4, 9, T0).await.body["result"],
            "started"
        );
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Op {
        Open,
        VmJobs,
        Seen,
        Close,
        OpenPipelines,
    }

    impl Op {
        fn name(self) -> &'static str {
            match self {
                Op::Open => "open",
                Op::VmJobs => "vm_jobs",
                Op::Seen => "seen",
                Op::Close => "close",
                Op::OpenPipelines => "open_pipelines",
            }
        }
    }

    struct FaultyRecords {
        inner: MemoryPipelineRecords,
        failing: Op,
    }

    impl FaultyRecords {
        fn check(&self, op: Op) -> Result<()> {
            if op == self.failing {
                return Err(Error::provider("bucket refused"));
            }
            Ok(())
        }
    }

    #[async_trait]
    impl PipelineRecords for FaultyRecords {
        async fn open(&self, pipeline: &str, vm_jobs: &VmJobs) -> Result<bool> {
            self.check(Op::Open)?;
            self.inner.open(pipeline, vm_jobs).await
        }
        async fn vm_jobs(&self, pipeline: &str) -> Result<Option<VmJobs>> {
            self.check(Op::VmJobs)?;
            self.inner.vm_jobs(pipeline).await
        }
        async fn seen(&self, pipeline: &str, job: &str) -> Result<()> {
            self.check(Op::Seen)?;
            self.inner.seen(pipeline, job).await
        }
        async fn seen_jobs(&self, pipeline: &str) -> Result<Vec<String>> {
            self.inner.seen_jobs(pipeline).await
        }
        async fn close(&self, pipeline: &str) -> Result<()> {
            self.check(Op::Close)?;
            self.inner.close(pipeline).await
        }
        async fn open_pipelines(&self) -> Result<Vec<crate::ledger::OpenPipeline>> {
            self.check(Op::OpenPipelines)?;
            self.inner.open_pipelines().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_record_never_fails_an_answer() {
        for op in [Op::Open, Op::Close] {
            let records = Arc::new(FaultyRecords {
                inner: MemoryPipelineRecords::new(),
                failing: op,
            });
            let s = warm_setup(lane_config(), lane_secrets(), None, Some(records)).await;
            idle_vm(&s, 1, 2).await;
            let now = T0 + 60;
            let builds = [build(11, "lint", "test"), build(13, "e2e", "test")];
            let warm = s.pipeline_at(20, "pending", &builds, now).await;
            let action = if op == Op::Open {
                "disabled"
            } else {
                "capacity"
            };
            assert_eq!(
                warm_pools(&warm),
                json!([{ "pool": "vm", "action": action, "vmJobs": 1 }]),
                "{op:?}"
            );
            let lint = s.job_at("lint", "test", 11, 20, now).await;
            assert_eq!(lint.body["result"], "started", "{op:?} {}", lint.body);
            assert_eq!(lint.body["pool"], "7-node-24", "{op:?}");
            let e2e = s.job_at("e2e", "test", 13, 20, now).await;
            assert_eq!(e2e.body["result"], "claimed", "{op:?} {}", e2e.body);
            let closed = s.pipeline_at(20, "failed", &builds, now).await;
            assert_eq!(closed.body["result"], "closed", "{op:?}");
            let failed: Vec<String> = s
                .lines()
                .into_iter()
                .filter(|l| l.starts_with("warn records_failed"))
                .collect();
            let prefix = format!(
                r#"warn records_failed op="{}" pipeline="20" error="bucket refused""#,
                op.name()
            );
            assert!(failed.iter().any(|l| l.starts_with(&prefix)), "{failed:?}");
            let op = format!(r#"warn records_failed op="{}" "#, op.name());
            assert!(failed.iter().all(|l| l.starts_with(&op)), "{failed:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_warm_start_answers_500() {
        let records = Arc::new(MemoryPipelineRecords::new());
        let s = warm_setup(
            records_config(""),
            vm_secrets(),
            None,
            Some(records.clone()),
        )
        .await;
        s.recording.fail_starts([Error::provider("scaleway: 400")]);
        let failed = s
            .pipeline_at(9, "pending", &[build(1, "unit", "test")], T0)
            .await;
        assert_eq!(failed.status, 500);
        assert_eq!(failed.body, json!({ "result": "error", "ms": 0 }));
        assert!(s.lines().contains(
            &r#"error start_failed pipeline="9" project="7" pool="vm" error="scaleway: 400" ms="0""#
                .to_string()
        ));

        s.recording.fail_starts([Error::retryable("scaleway: 503")]);
        let retried = s
            .pipeline_at(10, "pending", &[build(2, "unit", "test")], T0)
            .await;
        assert_eq!(retried.body["pools"][0]["action"], "started");
        assert_eq!(retried.body["ms"], 300);
        assert_eq!(s.recording.start_calls(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pipeline_event_and_parallel_job_events_agree_on_one_vm() {
        for (job_at, expected) in [(T0, "capacity"), (T0 + 1, "started")] {
            let ledger = Arc::new(MemoryLedger::new());
            let mut s = warm_setup(
                records_config(""),
                vm_secrets(),
                Some(ledger.clone()),
                Some(Arc::new(MemoryPipelineRecords::new())),
            )
            .await;
            s.set_clock(T0);
            s.dispatcher = s
                .dispatcher
                .with_group_wait(Duration::from_secs(GROUP_WAIT_S));
            let builds = [build(2, "unit", "test"), build(3, "e2e", "test")];
            let (warm, unit, e2e) = tokio::join!(
                s.pipeline_at(9, "pending", &builds, T0),
                s.job_at("unit", "test", 2, 9, job_at),
                s.job_at("e2e", "test", 3, 9, job_at),
            );
            assert_eq!(warm.body["pools"][0]["action"], expected, "{job_at}");
            let results = [unit.body["result"].clone(), e2e.body["result"].clone()];
            let vms = s.vms().await;
            assert_eq!(vms.len(), 1, "{job_at}: {results:?}");
            if expected == "capacity" {
                assert_eq!(
                    results,
                    [json!("started"), json!("grouped")],
                    "jobs of the same second rank first"
                );
                assert_eq!((s.taken(), vms[0].free), (vec![2], 0));
            } else {
                assert_eq!(results, [json!("grouped"), json!("grouped")]);
                assert_eq!(unit.body["leader"], W);
                assert_eq!((s.taken(), vms[0].free), (vec![0], 2));
            }
            assert_eq!(ledger.all().len(), 3);
        }
    }

    /// A dispatcher with ledger, records and `schedule`; the records take the setup's clock.
    async fn queue_setup(
        config: Arc<Config>,
        secrets: Secrets,
        schedule: Arc<dyn Schedule>,
    ) -> (VmSetup, Arc<MemoryPipelineRecords>) {
        let mut s = setup_of(config, secrets, Some(Arc::new(MemoryLedger::new()))).await;
        let clock = s.clock.clone();
        let records = Arc::new(
            MemoryPipelineRecords::new().with_clock(Arc::new(move || clock.load(Ordering::SeqCst))),
        );
        s.dispatcher = s
            .dispatcher
            .with_records(Some(records.clone()))
            .with_schedule(Some(schedule));
        (s, records)
    }

    /// Unix second `t` as GitLab's hooks write it.
    fn gitlab_time(t: u64) -> String {
        let at = crate::time::DateTime::from_unix(t);
        format!("{} {} UTC", at.date(), at.time())
    }

    fn reported(next: u64) -> Value {
        json!({ "result": "reported", "nextReportS": next, "ms": 0 })
    }

    impl VmSetup {
        /// A job event of job `job` named `name` in pipeline `pipeline` with `status` and the
        /// fields of `extra`, at `now`.
        async fn event_at(
            &self,
            name: &str,
            job: u64,
            pipeline: u64,
            status: &str,
            extra: Value,
            now: u64,
        ) -> DispatchResponse {
            let mut body = json!({
                "object_kind": "build", "build_id": job, "build_name": name,
                "build_stage": "test", "build_status": status,
                "pipeline_id": pipeline, "project_id": 7
            });
            if let (Value::Object(body), Value::Object(extra)) = (&mut body, extra) {
                body.extend(extra);
            }
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

        async fn pending_job(&self, name: &str, job: u64, now: u64) -> DispatchResponse {
            self.event_at(name, job, 9, "pending", json!({}), now).await
        }

        /// The running event of job `job`, which started at `started`.
        async fn running(&self, name: &str, job: u64, started: u64) -> DispatchResponse {
            let extra = json!({ "build_started_at": gitlab_time(started), "build_duration": 0.05 });
            self.event_at(name, job, 9, "running", extra, started).await
        }

        async fn finished(
            &self,
            name: &str,
            job: u64,
            status: &str,
            duration: f64,
            now: u64,
        ) -> DispatchResponse {
            let extra = json!({ "build_duration": duration });
            self.event_at(name, job, 9, status, extra, now).await
        }

        async fn report_at(
            &self,
            run: &str,
            state: VmState,
            free: u32,
            now: u64,
        ) -> DispatchResponse {
            let body = serde_json::to_vec(&VmReport {
                state,
                free,
                slots: 2,
            })
            .unwrap();
            let headers = vm_headers(run, &vm::token(VM_SECRET, run));
            self.dispatcher
                .handle_request("POST", "/vm", &headers, &body, now)
                .await
        }
    }

    /// A VM of pool `vm` started at `T0` for job 1 of `image-a`; jobs 1 and 2 (`image-b`) run on
    /// it since `T0 + 2` and it reports both slots busy at `T0 + 60`. `typical` are the
    /// durations of one earlier finished run of each name.
    async fn busy_vm(s: &VmSetup, typical: [Option<f64>; 2]) -> String {
        for (id, name, duration) in [(101, "image-a", typical[0]), (102, "image-b", typical[1])] {
            if let Some(duration) = duration {
                let done = s.finished(name, id, "success", duration, T0).await;
                assert_eq!(done.body["result"], "tracked", "{}", done.body);
            }
        }
        let vm = run_of(&s.pending_job("image-a", 1, T0).await);
        s.pending_job("image-b", 2, T0).await;
        for (name, job) in [("image-a", 1), ("image-b", 2)] {
            let running = s.running(name, job, T0 + 2).await;
            assert_eq!(running.body["result"], "tracked", "{}", running.body);
        }
        let report = s.report_at(&vm, VmState::Busy, 0, T0 + 60).await;
        assert_eq!(report.body["result"], "reported", "{}", report.body);
        vm
    }

    #[tokio::test(start_paused = true)]
    async fn job_events_track_running_and_finished_jobs() {
        let schedule = Arc::new(MemorySchedule::new());
        let (s, _) = queue_setup(lane_config(), lane_secrets(), schedule.clone()).await;
        let extra = json!({ "build_started_at": gitlab_time(T0 - 30), "build_duration": 0.05 });
        let running = s.event_at("image", 5, 9, "running", extra, T0).await;
        assert_eq!(running.status, 200);
        assert_eq!(
            running.body,
            json!({ "result": "tracked", "status": "running", "pool": "vm", "ms": 0 })
        );
        assert!(s.lines().contains(
            &r#"info tracked job="5" pipeline="9" project="7" stage="test" pool="vm" status="running" ms="0""#
                .to_string()
        ));
        let lint = s.event_at("lint", 6, 9, "running", json!({}), T0 + 1).await;
        assert_eq!(lint.body["pool"], "7-node-24", "a lane job under its lane");
        let odd = json!({ "build_started_at": "yesterday" });
        s.event_at("image", 7, 9, "running", odd, T0 + 2).await;
        let running_of = |pool: &'static str| {
            let schedule = schedule.clone();
            async move {
                let mut jobs = schedule.running(pool).await.unwrap();
                jobs.sort_by(|a, b| a.job.cmp(&b.job));
                jobs
            }
        };
        let run = |job: &str, name: &str, started| Running {
            job: job.into(),
            name: name.into(),
            started,
        };
        assert_eq!(
            running_of("vm").await,
            [run("5", "image", T0 - 30), run("7", "image", T0 + 2)],
            "no start time is the event's time"
        );
        assert_eq!(running_of("7-node-24").await, [run("6", "lint", T0 + 1)]);

        let done = s.finished("image", 5, "success", 61.6, T0 + 32).await;
        assert_eq!(
            done.body,
            json!({ "result": "tracked", "status": "success", "pool": "vm", "ms": 0 })
        );
        assert!(s.lines().contains(
            &r#"info tracked job="5" pipeline="9" project="7" stage="test" pool="vm" status="success" duration_s="62" ms="0""#
                .to_string()
        ));
        assert_eq!(schedule.typical("vm", "image").await.unwrap(), Some(62));
        s.finished("image", 7, "failed", 30.4, T0 + 33).await;
        assert_eq!(schedule.typical("vm", "image").await.unwrap(), Some(52));
        s.finished("lint", 6, "canceled", 12.0, T0 + 34).await;
        s.finished("image", 8, "skipped", 0.0, T0 + 34).await;
        assert_eq!(schedule.typical("7-node-24", "lint").await.unwrap(), None);
        assert_eq!(schedule.typical("vm", "image").await.unwrap(), Some(52));
        assert!(running_of("vm").await.is_empty());
        assert!(running_of("7-node-24").await.is_empty());

        for status in ["created", "waiting_for_resource", "preparing", "manual"] {
            let other = s.event_at("image", 9, 9, status, json!({}), T0).await;
            assert_eq!(
                other.body,
                json!({ "result": "ignored", "reason": "status", "status": status, "ms": 0 })
            );
        }
        let own = s
            .event_at("ciburst-check", 10, 9, "running", json!({}), T0)
            .await;
        assert_eq!(own.body["reason"], "own_job");
        assert!(s.recording.started.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_vm_job_behind_a_slot_that_frees_soon_queues_and_that_slot_takes_it() {
        let schedule = Arc::new(MemorySchedule::new());
        let (s, _) = queue_setup(lane_config(), lane_secrets(), schedule.clone()).await;
        let vm = busy_vm(&s, [Some(60.0), Some(600.0)]).await;
        let deploy = s.pending_job("deploy", 3, T0 + 60).await;
        assert_eq!(deploy.status, 200);
        assert_eq!(
            deploy.body,
            json!({ "result": "queued", "pool": "vm", "etaS": 2, "ms": 0 })
        );
        assert!(s.lines().contains(
            &r#"info queued job="3" pipeline="9" project="7" stage="test" pool="vm" eta_s="2" ms="0""#
                .to_string()
        ));
        assert_eq!(
            schedule.queued("vm").await.unwrap(),
            [Queued {
                job: "3".into(),
                name: "deploy".into(),
                pipeline: "9".into(),
                at: T0 + 60,
            }]
        );
        assert_eq!(
            s.report_at(&vm, VmState::Busy, 0, T0 + 61).await.body,
            reported(15),
            "a queued VM job: every 15 s"
        );
        let done = s.finished("image-a", 1, "success", 60.4, T0 + 62).await;
        assert_eq!(done.body["result"], "tracked");
        assert_eq!(
            s.report_at(&vm, VmState::Busy, 1, T0 + 62).await.body,
            reported(15),
            "the free slot takes the queued job, no VM starts"
        );
        assert_eq!(
            s.running("deploy", 3, T0 + 63).await.body,
            json!({ "result": "tracked", "status": "running", "pool": "vm", "ms": 0 })
        );
        assert!(schedule.queued("vm").await.unwrap().is_empty());
        assert_eq!(s.started_pools(), ["vm"], "one VM");
        assert_eq!(schedule.typical("vm", "image-a").await.unwrap(), Some(60));
        assert_eq!(
            s.report_at(&vm, VmState::Busy, 0, T0 + 64).await.body,
            reported(30)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_vm_job_whose_wait_would_pass_max_queue_s_starts_a_vm() {
        let schedule = Arc::new(MemorySchedule::new());
        let (s, _) = queue_setup(lane_config(), lane_secrets(), schedule.clone()).await;
        busy_vm(&s, [Some(400.0), Some(600.0)]).await;
        let deploy = s.pending_job("deploy", 3, T0 + 60).await;
        assert_eq!(deploy.body["result"], "started", "{}", deploy.body);
        assert_eq!(s.taken(), [1, 1]);
        assert!(schedule.queued("vm").await.unwrap().is_empty());

        let mut eager = (*lane_config()).clone();
        eager.vm.max_queue_s = Some(0);
        let (s, _) = queue_setup(
            Arc::new(eager),
            lane_secrets(),
            Arc::new(MemorySchedule::new()),
        )
        .await;
        busy_vm(&s, [Some(60.0), Some(600.0)]).await;
        let deploy = s.pending_job("deploy", 3, T0 + 60).await;
        assert_eq!(
            deploy.body["result"], "started",
            "maxQueueS 0 buys a VM for any wait"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_durations_queue_until_max_queue_s_passed() {
        let schedule = Arc::new(MemorySchedule::new());
        let (s, _) = queue_setup(lane_config(), lane_secrets(), schedule.clone()).await;
        let vm = busy_vm(&s, [None, None]).await;
        let deploy = s.pending_job("deploy", 3, T0 + 60).await;
        assert_eq!(
            deploy.body,
            json!({ "result": "queued", "pool": "vm", "etaS": 0, "ms": 0 })
        );
        let again = s.pending_job("deploy", 3, T0 + 100).await;
        assert_eq!(again.body["result"], "queued");
        assert_eq!(
            schedule.queued("vm").await.unwrap()[0].at,
            T0 + 60,
            "a repeated event keeps the job's place"
        );
        assert_eq!(
            s.report_at(&vm, VmState::Busy, 0, T0 + 150).await.body,
            reported(15)
        );
        assert_eq!(s.started_pools(), ["vm"], "90 s are allowed");
        s.set_clock(T0 + 151);
        assert_eq!(
            s.report_at(&vm, VmState::Busy, 0, T0 + 151).await.body,
            reported(15)
        );
        let started = s.recording.started.lock().unwrap().clone();
        assert_eq!(started.len(), 2);
        assert_eq!(started[1].taken, 0, "every slot free");
        assert_eq!(
            started[1].labels,
            BTreeMap::from([
                ("job".to_string(), "queue:3".to_string()),
                ("pipeline".to_string(), "9".to_string()),
            ])
        );
        assert_eq!(started[1].env["RUNNER_TOKEN"], "glrt-vm");
        let queued_vm = started.len();
        let run = s
            .vms()
            .await
            .iter()
            .map(|v| v.run_id.clone())
            .find(|r| *r != vm);
        assert!(s.lines().contains(&format!(
            r#"info queue_started pool="vm" job="queue:3" vms="1" needing="1" runs="{}" ms="0""#,
            run.unwrap()
        )));
        s.report_at(&vm, VmState::Busy, 0, T0 + 170).await;
        assert_eq!(
            s.recording.started.lock().unwrap().len(),
            queued_vm,
            "the starting VM takes the job"
        );
        assert_eq!(schedule.queued("vm").await.unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn two_evaluations_at_once_start_one_vm() {
        let (s, _) = queue_setup(
            lane_config(),
            lane_secrets(),
            Arc::new(MemorySchedule::new()),
        )
        .await;
        let vm = busy_vm(&s, [None, None]).await;
        s.pending_job("deploy", 3, T0 + 60).await;
        *s.recording.delay.lock().unwrap() = Duration::from_millis(100);
        let (a, b) = tokio::join!(
            s.report_at(&vm, VmState::Busy, 0, T0 + 151),
            s.report_at(&vm, VmState::Busy, 0, T0 + 151),
        );
        assert_eq!((a.status, b.status), (200, 200));
        assert_eq!(s.started_pools(), ["vm", "vm"], "one VM for the queue");
        let starts = s
            .lines()
            .iter()
            .filter(|l| l.starts_with("info queue_started"))
            .count();
        assert_eq!(starts, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn at_max_runs_a_vm_job_queues_instead_of_answering_at_capacity() {
        let schedule = Arc::new(MemorySchedule::new());
        let (s, _) = queue_setup(vm_config(), vm_secrets(), schedule.clone()).await;
        let a = run_of(&s.pending_job("image-a", 1, T0).await);
        s.report_at(&a, VmState::Busy, 0, T0 + 60).await;
        let second = s.pending_job("image-b", 2, T0 + 60).await;
        assert_eq!(
            second.body["result"], "started",
            "no running job will free a slot"
        );
        let b = run_of(&second);
        s.report_at(&b, VmState::Busy, 0, T0 + 61).await;
        let third = s.pending_job("deploy", 3, T0 + 62).await;
        assert_eq!(
            third.body,
            json!({ "result": "queued", "pool": "vm", "etaS": null, "ms": 0 })
        );
        assert!(s.lines().contains(
            &r#"info queued job="3" pipeline="9" project="7" stage="test" pool="vm" eta_s="null" ms="0""#
                .to_string()
        ));
        assert_eq!(
            s.report_at(&a, VmState::Busy, 0, T0 + 200).await.body,
            reported(15)
        );
        assert_eq!(s.started_pools(), ["vm", "vm"], "maxRuns 2");
        assert_eq!(schedule.queued("vm").await.unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_past_max_runs_queues_its_last_jobs() {
        let ledger = Arc::new(MemoryLedger::new());
        let schedule = Arc::new(MemorySchedule::new());
        let s = vm_setup_with(vm_secrets(), Some(ledger.clone())).await;
        let dispatcher = Dispatcher::new(
            vm_config(),
            s.recording.clone(),
            Arc::new(vm_secrets()),
            Log::silent(),
        )
        .with_ledger(Some(ledger.clone()))
        .with_schedule(Some(schedule.clone()));
        let hook = headers("hook-secret");
        let bodies: Vec<Vec<u8>> = (1..=5)
            .map(|job| event("test", "pending", 7, job))
            .collect();
        let (a, b, c, d, e) = tokio::join!(
            dispatcher.handle_request("POST", "/", &hook, &bodies[0], T0),
            dispatcher.handle_request("POST", "/", &hook, &bodies[1], T0),
            dispatcher.handle_request("POST", "/", &hook, &bodies[2], T0),
            dispatcher.handle_request("POST", "/", &hook, &bodies[3], T0),
            dispatcher.handle_request("POST", "/", &hook, &bodies[4], T0),
        );
        let results: Vec<Value> = [a, b, c, d, e]
            .into_iter()
            .map(|r| r.body["result"].clone())
            .collect();
        assert_eq!(
            results,
            ["started", "grouped", "started", "grouped", "queued"],
            "rank 4 is group 2 and maxRuns is 2"
        );
        let queued = schedule.queued("vm").await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].job, "5");
    }

    #[tokio::test(start_paused = true)]
    async fn queued_entries_older_than_30_minutes_are_dropped_when_read() {
        let schedule = Arc::new(MemorySchedule::new());
        let (s, _) = queue_setup(lane_config(), lane_secrets(), schedule.clone()).await;
        let vm = busy_vm(&s, [Some(60.0), Some(600.0)]).await;
        let lost = |job: &str, at: u64| Queued {
            job: job.into(),
            name: "lost".into(),
            pipeline: "8".into(),
            at,
        };
        let now = T0 + 2000;
        schedule
            .enqueue("vm", &lost("90", now - 1801))
            .await
            .unwrap();
        schedule
            .enqueue("vm", &lost("91", now - 1800))
            .await
            .unwrap();
        let ghost = |job: &str, started: u64| Running {
            job: job.into(),
            name: "ghost".into(),
            started,
        };
        schedule
            .started("vm", &ghost("80", now - RUNNING_MAX_AGE_S - 1))
            .await
            .unwrap();
        schedule
            .started("vm", &ghost("81", now - RUNNING_MAX_AGE_S))
            .await
            .unwrap();
        s.report_at(&vm, VmState::Busy, 0, now).await;
        assert_eq!(
            schedule.queued("vm").await.unwrap(),
            [lost("91", now - 1800)]
        );
        let mut running: Vec<String> = schedule
            .running("vm")
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.job)
            .collect();
        running.sort();
        assert_eq!(running, ["1", "2", "81"], "a running entry lives 24 h");
        assert!(
            s.lines()
                .iter()
                .any(|l| l.starts_with(r#"info queue_started pool="vm" job="queue:91""#)),
            "30 minutes still count"
        );
        assert_eq!(
            s.started_pools(),
            ["vm", "vm"],
            "the lost lane job starts no run"
        );
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum ScheduleOp {
        Enqueue,
        Queued,
        Started,
        Running,
        Finished,
        Typical,
        TakeStart,
    }

    struct FaultySchedule {
        inner: MemorySchedule,
        failing: ScheduleOp,
    }

    impl FaultySchedule {
        fn new(failing: ScheduleOp) -> Arc<FaultySchedule> {
            Arc::new(FaultySchedule {
                inner: MemorySchedule::new(),
                failing,
            })
        }

        fn check(&self, op: ScheduleOp) -> Result<()> {
            if op == self.failing {
                return Err(Error::provider("bucket refused"));
            }
            Ok(())
        }
    }

    #[async_trait]
    impl Schedule for FaultySchedule {
        async fn enqueue(&self, pool: &str, job: &Queued) -> Result<()> {
            self.check(ScheduleOp::Enqueue)?;
            self.inner.enqueue(pool, job).await
        }
        async fn queued(&self, pool: &str) -> Result<Vec<Queued>> {
            self.check(ScheduleOp::Queued)?;
            self.inner.queued(pool).await
        }
        async fn started(&self, pool: &str, job: &Running) -> Result<()> {
            self.check(ScheduleOp::Started)?;
            self.inner.started(pool, job).await
        }
        async fn running(&self, pool: &str) -> Result<Vec<Running>> {
            self.check(ScheduleOp::Running)?;
            self.inner.running(pool).await
        }
        async fn finished(
            &self,
            pool: &str,
            job: &str,
            name: &str,
            duration_s: Option<u64>,
        ) -> Result<()> {
            self.check(ScheduleOp::Finished)?;
            self.inner.finished(pool, job, name, duration_s).await
        }
        async fn typical(&self, pool: &str, name: &str) -> Result<Option<u64>> {
            self.check(ScheduleOp::Typical)?;
            self.inner.typical(pool, name).await
        }
        async fn take_start(&self, pool: &str, now: u64) -> Result<bool> {
            self.check(ScheduleOp::TakeStart)?;
            self.inner.take_start(pool, now).await
        }
    }

    fn schedule_warnings(s: &VmSetup) -> Vec<String> {
        s.lines()
            .into_iter()
            .filter(|l| l.starts_with("warn schedule_failed"))
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_schedule_falls_back_to_the_burst_path() {
        for (failing, op) in [
            (ScheduleOp::Queued, "queued"),
            (ScheduleOp::Running, "running"),
            (ScheduleOp::Typical, "typical"),
            (ScheduleOp::Enqueue, "enqueue"),
        ] {
            let (s, _) =
                queue_setup(lane_config(), lane_secrets(), FaultySchedule::new(failing)).await;
            let vm = busy_vm(&s, [Some(60.0), Some(600.0)]).await;
            let deploy = s.pending_job("deploy", 3, T0 + 60).await;
            assert_eq!(deploy.status, 200, "{op}");
            assert_eq!(deploy.body["result"], "started", "{op}: {}", deploy.body);
            let warnings = schedule_warnings(&s);
            assert!(
                warnings.iter().any(|l| l.starts_with(&format!(
                    r#"warn schedule_failed op="{op}" pool="vm" error="bucket refused""#
                ))),
                "{op}: {warnings:?}"
            );
            let report = s.report_at(&vm, VmState::Busy, 0, T0 + 61).await;
            assert_eq!(report.status, 200, "{op}");
            if failing == ScheduleOp::Queued {
                assert_eq!(report.body, reported(30), "nothing spills");
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_schedule_never_fails_an_answer() {
        let (s, _) = queue_setup(
            lane_config(),
            lane_secrets(),
            FaultySchedule::new(ScheduleOp::Started),
        )
        .await;
        let running = s.running("image", 5, T0).await;
        assert_eq!(
            running.body,
            json!({ "result": "ignored", "reason": "status", "status": "running", "ms": 0 })
        );
        assert_eq!(
            schedule_warnings(&s),
            [r#"warn schedule_failed op="started" pool="vm" error="bucket refused" ms="0""#]
        );

        let (s, _) = queue_setup(
            lane_config(),
            lane_secrets(),
            FaultySchedule::new(ScheduleOp::Finished),
        )
        .await;
        let done = s.finished("lint", 6, "success", 3.0, T0).await;
        assert_eq!(
            done.body,
            json!({ "result": "ignored", "reason": "status", "status": "success", "ms": 0 })
        );

        let (s, _) = queue_setup(
            lane_config(),
            lane_secrets(),
            FaultySchedule::new(ScheduleOp::TakeStart),
        )
        .await;
        let vm = busy_vm(&s, [None, None]).await;
        s.pending_job("deploy", 3, T0 + 60).await;
        let report = s.report_at(&vm, VmState::Busy, 0, T0 + 151).await;
        assert_eq!(report.body, reported(15));
        assert_eq!(s.started_pools(), ["vm"], "no marker, no start");
        assert!(schedule_warnings(&s)[0].starts_with(r#"warn schedule_failed op="take_start""#));
    }

    #[tokio::test(start_paused = true)]
    async fn a_job_beyond_the_slots_of_a_starting_vm_gets_a_vm_of_its_own() {
        let ledger = Arc::new(MemoryLedger::new());
        let (mut s, _) = queue_setup(
            lane_config(),
            lane_secrets(),
            Arc::new(MemorySchedule::new()),
        )
        .await;
        s.dispatcher = s.dispatcher.with_ledger(Some(ledger.clone()));
        let builds = [
            build(2, "e2e-a", "test"),
            build(3, "e2e-b", "test"),
            build(4, "e2e-c", "test"),
        ];
        let warm = s.pipeline_at(9, "pending", &builds, T0).await;
        assert_eq!(warm.body["pools"][0]["action"], "started", "{}", warm.body);
        let mut answers = Vec::new();
        for (job, name) in [(2, "e2e-a"), (3, "e2e-b"), (4, "e2e-c")] {
            let response = s.event_at(name, job, 9, "pending", json!({}), T0 + 5).await;
            answers.push((
                response.body["result"].clone(),
                response.body["etaS"].clone(),
            ));
        }
        assert_eq!(
            answers,
            [
                (json!("queued"), json!(0)),
                (json!("queued"), json!(0)),
                (json!("queued"), Value::Null),
            ],
            "the warm VM's two slots go to the first two"
        );
        assert_eq!(s.taken(), [0, 0], "the warm VM and one for the third job");
        assert_eq!(ledger.all().len(), 1, "only the warm intent");
        assert!(
            s.lines()
                .iter()
                .any(|l| l.starts_with(r#"info queue_started pool="vm" job="queue:4""#))
        );
    }
}
