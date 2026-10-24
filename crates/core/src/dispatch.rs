use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::config::{CacheType, Config, Pool};
use crate::log::Log;
use crate::provision::keys;
use crate::types::{Provider, Secrets, StartRequest, VmCapacity, VmReport, VmState};
use crate::vm;
use crate::{Error, Result};

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
        }
    }

    /// Entry of every request. `POST /vm` and `POST /terminate` come from VMs with their
    /// capability token, a body with `ciburst_sweep` from the cron trigger, everything else is a
    /// GitLab webhook. `now` is unix seconds.
    pub async fn handle_request(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
        now: u64,
    ) -> DispatchResponse {
        if method.eq_ignore_ascii_case("POST") {
            let path = path.trim_end_matches('/');
            if path == vm::PATH_REPORT {
                return self.vm_report(headers, body, now).await;
            }
            if path == vm::PATH_TERMINATE {
                return self.vm_terminate(headers).await;
            }
        }
        if let Some(given) = sweep_arg(body) {
            return self.sweep(given.as_deref(), now).await;
        }
        self.handle(headers, body).await
    }

    /// The GitLab webhook.
    pub async fn handle(&self, headers: &[(String, String)], body: &[u8]) -> DispatchResponse {
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

        let stage = event.get("build_stage").and_then(Value::as_str);
        let pool = self.config.pool_for_stage(stage).clone();
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
        fields.push(("pool", &pool.tag));
        match self.schedule(&pool, labels.clone()).await {
            Ok(Decision::Claimed { run_id, free }) => {
                let free = free.to_string();
                fields.push(("run", &run_id));
                fields.push(("free", &free));
                self.log.info("claimed", &fields);
                reply(200, "claimed", json!({ "pool": pool.tag, "runId": run_id }))
            }
            Ok(Decision::AtCapacity(live)) => {
                let live = live.to_string();
                fields.push(("live", &live));
                self.log.info("at_capacity", &fields);
                reply(200, "at_capacity", json!({ "pool": pool.tag }))
            }
            Ok(Decision::Started(run_id)) => {
                fields.push(("run", &run_id));
                self.log.info("started", &fields);
                reply(200, "started", json!({ "pool": pool.tag, "runId": run_id }))
            }
            Err(error) => {
                let message = error.to_string();
                fields.push(("error", &message));
                self.log.error("start_failed", &fields);
                reply(500, "error", json!({}))
            }
        }
    }

    /// Spec 6.1, scheduling: a free slot of an `idle` or `busy` VM is claimed; otherwise a run
    /// starts unless `maxRuns` runs are live (VMs: not draining). Providers without VMs answer
    /// `None` from `capacity` and count live runs.
    async fn schedule(&self, pool: &Pool, labels: BTreeMap<String, String>) -> Result<Decision> {
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
            return self.start(pool, labels, false).await;
        };
        if let Some(target) = best_slot(&vms) {
            let free: u64 = vms
                .iter()
                .filter(|v| counts_as_capacity(v.state))
                .map(|v| u64::from(v.free))
                .sum();
            self.provider.claim(&target.run_id).await?;
            return Ok(Decision::Claimed {
                run_id: target.run_id.clone(),
                free,
            });
        }
        let live = vms.iter().filter(|v| v.state != VmState::Draining).count();
        if live >= pool.max_runs as usize {
            return Ok(Decision::AtCapacity(live));
        }
        self.start(pool, labels, true).await
    }

    async fn start(
        &self,
        pool: &Pool,
        labels: BTreeMap<String, String>,
        vm: bool,
    ) -> Result<Decision> {
        let env = run_env(&self.config, &self.secrets, pool, vm)?;
        let run_id = self
            .provider
            .start(StartRequest {
                pool: pool.clone(),
                env,
                labels,
            })
            .await?;
        Ok(Decision::Started(run_id))
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
    fn refuse(&self, route: &str, headers: &[(String, String)]) -> DispatchResponse {
        let run: String = header(headers, vm::HEADER_RUN)
            .unwrap_or("")
            .chars()
            .take(64)
            .collect();
        self.log
            .warn("unauthorized", &[("route", route), ("run", &run)]);
        unauthorized()
    }

    async fn vm_report(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        now: u64,
    ) -> DispatchResponse {
        let Some(run) = self.vm_caller(headers) else {
            return self.refuse(vm::PATH_REPORT, headers);
        };
        let Ok(report) = serde_json::from_slice::<VmReport>(body) else {
            return reply(400, "bad_request", json!({}));
        };
        match self.provider.report(run, &report, now).await {
            Ok(()) => {
                let free = report.free.to_string();
                self.log.info(
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
                self.log
                    .error("report_failed", &[("run", run), ("error", &message)]);
                reply(500, "error", json!({}))
            }
        }
    }

    async fn vm_terminate(&self, headers: &[(String, String)]) -> DispatchResponse {
        let Some(run) = self.vm_caller(headers) else {
            return self.refuse(vm::PATH_TERMINATE, headers);
        };
        match self.provider.stop(run).await {
            Ok(()) => {
                self.log.info("terminated", &[("run", run)]);
                reply(200, "terminated", json!({}))
            }
            Err(error) => {
                let message = error.to_string();
                self.log
                    .error("terminate_failed", &[("run", run), ("error", &message)]);
                reply(500, "error", json!({}))
            }
        }
    }

    async fn sweep(&self, given: Option<&str>, now: u64) -> DispatchResponse {
        if !token_matches(given, self.secrets.hook_token.as_deref()) {
            self.log.warn("unauthorized", &[("route", "sweep")]);
            return unauthorized();
        }
        match self.provider.sweep(now).await {
            Ok(runs) => {
                let count = runs.len().to_string();
                let joined = runs.join(",");
                self.log
                    .info("swept", &[("count", &count), ("runs", &joined)]);
                reply(200, "swept", json!({ "runs": runs }))
            }
            Err(error) => {
                let message = error.to_string();
                self.log.error("sweep_failed", &[("error", &message)]);
                reply(500, "error", json!({}))
            }
        }
    }
}

enum Decision {
    Claimed { run_id: String, free: u64 },
    AtCapacity(usize),
    Started(String),
}

/// `starting` VMs do not poll yet and `draining` ones stop soon; neither takes a routed job.
fn counts_as_capacity(state: VmState) -> bool {
    matches!(state, VmState::Idle | VmState::Busy)
}

/// The VM with the most free slots; on a tie the one seen longest ago, then by id, so the
/// choice is deterministic.
fn best_slot(vms: &[VmCapacity]) -> Option<&VmCapacity> {
    vms.iter()
        .filter(|v| counts_as_capacity(v.state) && v.free > 0)
        .min_by(|a, b| {
            b.free
                .cmp(&a.free)
                .then(a.seen.unwrap_or(0).cmp(&b.seen.unwrap_or(0)))
                .then(a.run_id.cmp(&b.run_id))
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
    use crate::store::MemoryStore;
    use crate::types::{KeyPair, Run};
    use async_trait::async_trait;

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
    }

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
            if self.fail {
                return Err(Error::provider("quota"));
            }
            self.started.lock().unwrap().push(request.clone());
            self.inner.start(request).await
        }
        async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
            self.inner.list(pool).await
        }
        async fn stop(&self, run_id: &str) -> Result<()> {
            self.inner.stop(run_id).await
        }
        async fn capacity(&self, pool: &str) -> Result<Option<Vec<VmCapacity>>> {
            self.inner.capacity(pool).await
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

    async fn setup(fail: bool) -> (Dispatcher, Arc<Recording>, Arc<Mutex<Vec<String>>>) {
        let config = config();
        let inner = Arc::new(FakeProvider::new(
            config.clone(),
            Arc::new(MemoryStore::new()),
        ));
        let plan = inner.plan(&[]).await.unwrap();
        inner.apply(&plan).await.unwrap();
        let recording = Arc::new(Recording {
            inner,
            started: Mutex::new(Vec::new()),
            fail,
        });
        let (log, lines) = Log::memory();
        let dispatcher = Dispatcher::new(config, recording.clone(), secrets(), log);
        (dispatcher, recording, lines)
    }

    #[tokio::test]
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

    #[tokio::test]
    async fn answers_400_for_bad_json() {
        let (d, _, _) = setup(false).await;
        let response = d.handle(&headers("hook-secret"), b"{not json").await;
        assert_eq!(response.status, 400);
        assert_eq!(response.body["result"], "bad_request");
    }

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
    async fn default_pool_and_case_insensitive_headers() {
        let (d, _, _) = setup(false).await;
        let headers = vec![("x-gitlab-token".to_string(), "hook-secret".to_string())];
        let response = d.handle(&headers, &event("deploy", "pending", 7, 1)).await;
        assert_eq!(response.body["result"], "started");
        assert_eq!(response.body["pool"], "ci");
    }

    #[tokio::test]
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

    #[tokio::test]
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

    #[test]
    fn the_best_slot_has_most_free_then_the_oldest_report() {
        let vm = |id: &str, state: VmState, free: u32, seen: Option<u64>| VmCapacity {
            run_id: id.into(),
            state,
            free,
            seen,
            created_at: 0,
        };
        let pick = |vms: &[VmCapacity]| best_slot(vms).map(|v| v.run_id.clone());
        assert_eq!(pick(&[]), None);
        assert_eq!(
            pick(&[
                vm("starting", VmState::Starting, 4, None),
                vm("draining", VmState::Draining, 4, Some(1)),
                vm("full", VmState::Busy, 0, Some(1)),
            ]),
            None
        );
        assert_eq!(
            pick(&[
                vm("a", VmState::Idle, 1, Some(5)),
                vm("b", VmState::Busy, 2, Some(9)),
                vm("c", VmState::Idle, 2, Some(7)),
                vm("d", VmState::Starting, 3, None),
            ])
            .as_deref(),
            Some("c")
        );
        assert_eq!(
            pick(&[
                vm("y", VmState::Idle, 1, Some(5)),
                vm("x", VmState::Idle, 1, Some(5)),
            ])
            .as_deref(),
            Some("x")
        );
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
                  ]
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
            cache: None,
            vm_secret: Some(VM_SECRET.into()),
        }
    }

    struct VmSetup {
        dispatcher: Dispatcher,
        recording: Arc<Recording>,
        lines: Arc<Mutex<Vec<String>>>,
    }

    async fn vm_setup(secrets: Secrets) -> VmSetup {
        let config = vm_config();
        let inner = Arc::new(
            FakeProvider::new(config.clone(), Arc::new(MemoryStore::new()))
                .with_clock(Arc::new(|| T0)),
        );
        inner.apply(&inner.plan(&[]).await.unwrap()).await.unwrap();
        let recording = Arc::new(Recording {
            inner,
            started: Mutex::new(Vec::new()),
            fail: false,
        });
        let (log, lines) = Log::memory();
        VmSetup {
            dispatcher: Dispatcher::new(config, recording.clone(), Arc::new(secrets), log),
            recording,
            lines,
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
            self.post(
                "/",
                &headers("hook-secret"),
                &event(stage, "pending", 7, job),
            )
            .await
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

    #[tokio::test]
    async fn starts_vms_until_max_runs_not_counting_draining_ones() {
        let s = vm_setup(vm_secrets()).await;
        let first = s.pending("test", 1).await;
        assert_eq!(first.body["result"], "started", "{}", first.body);
        assert_eq!(first.body["pool"], "vm");
        let second = s.pending("test", 2).await;
        assert_eq!(
            second.body["result"], "started",
            "starting VMs offer no slot"
        );
        let third = s.pending("test", 3).await;
        assert_eq!(third.status, 200);
        assert_eq!(third.body, json!({ "result": "at_capacity", "pool": "vm" }));

        s.report(&run_of(&first), VmState::Draining, 2).await;
        let fourth = s.pending("test", 4).await;
        assert_eq!(
            fourth.body["result"], "started",
            "a draining VM frees its place"
        );

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

    #[tokio::test]
    async fn claims_a_free_slot_instead_of_starting() {
        let s = vm_setup(vm_secrets()).await;
        let a = run_of(&s.pending("test", 1).await);
        let b = run_of(&s.pending("test", 2).await);
        s.report(&a, VmState::Busy, 1).await;
        s.report(&b, VmState::Idle, 2).await;

        let claimed = s.pending("test", 3).await;
        assert_eq!(claimed.status, 200);
        assert_eq!(
            claimed.body,
            json!({ "result": "claimed", "pool": "vm", "runId": b })
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

    #[tokio::test]
    async fn one_job_pools_keep_counting_runs_and_never_get_the_vm_secret() {
        let s = vm_setup(vm_secrets()).await;
        let response = s.pending("build", 1).await;
        assert_eq!(response.body["result"], "started");
        assert_eq!(response.body["pool"], "jobs");
        let started = s.recording.started.lock().unwrap();
        assert!(!started[0].env.contains_key(keys::VM_SECRET));
    }

    #[tokio::test]
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

    #[tokio::test]
    async fn vm_requests_need_the_token_of_their_run() {
        let s = vm_setup(vm_secrets()).await;
        let run = run_of(&s.pending("test", 1).await);
        let other = run_of(&s.pending("test", 2).await);
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
                    .any(|l| l == &format!(r#"warn unauthorized route="{route}" run="{run}""#)),
                "{lines:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_vm_reports_its_state() {
        let s = vm_setup(vm_secrets()).await;
        let run = run_of(&s.pending("test", 1).await);
        let headers = vm_headers(&run, &vm::token(VM_SECRET, &run));
        let response = s
            .post("/vm/", &headers, br#"{"state":"busy","free":1,"slots":2}"#)
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body, json!({ "result": "reported" }));
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

    #[tokio::test]
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

    #[tokio::test]
    async fn a_vm_terminates_itself_only() {
        let s = vm_setup(vm_secrets()).await;
        let run = run_of(&s.pending("test", 1).await);
        let other = run_of(&s.pending("test", 2).await);
        let response = s
            .post(
                "/terminate",
                &vm_headers(&run, &vm::token(VM_SECRET, &run)),
                b"",
            )
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body, json!({ "result": "terminated" }));
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

    #[tokio::test]
    async fn the_cron_body_sweeps_with_the_hook_token() {
        let s = vm_setup(vm_secrets()).await;
        let run = run_of(&s.pending("test", 1).await);
        let sweep = |token: Value| json!({ vm::SWEEP_ARG: token }).to_string().into_bytes();
        for body in [
            sweep(json!("wrong")),
            sweep(json!(7)),
            sweep(Value::Null),
            sweep(json!("hook-secret-x")),
        ] {
            let response = s.post("/", &[], &body).await;
            assert_eq!(response.status, 401);
            assert_eq!(response.body, Value::Null);
        }
        let late = T0 + vm::SWEEP_STARTING_S + 1;
        let early = s
            .dispatcher
            .handle_request("POST", "/", &[], &sweep(json!("hook-secret")), T0 + 60)
            .await;
        assert_eq!(early.status, 200);
        assert_eq!(early.body, json!({ "result": "swept", "runs": [] }));
        let response = s
            .dispatcher
            .handle_request("POST", "/", &[], &sweep(json!("hook-secret")), late)
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body, json!({ "result": "swept", "runs": [run] }));
        assert!(s.vms().await.is_empty());
        let lines = s.lines();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("info swept") && l.contains("count=\"1\""))
        );
        assert!(!lines.iter().any(|l| l.contains("hook-secret")));
        assert_eq!(
            lines
                .iter()
                .filter(|l| *l == r#"warn unauthorized route="sweep""#)
                .count(),
            4
        );

        let mut secrets = vm_secrets();
        secrets.hook_token = None;
        let no_hook = vm_setup(secrets).await;
        let response = no_hook.post("/", &[], &sweep(json!(""))).await;
        assert_eq!(response.status, 401);
    }

    #[tokio::test]
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
}
