//! Scaleway Serverless Function entry: `handle` receives the GitLab webhook (job and pipeline
//! events), the VM agents' `POST /vm` and `POST /terminate`, and the sweep cron, and hands
//! method, path, headers, body and the current time to
//! `ciburst_core::dispatch::Dispatcher::handle_request` with a `Scaleway` provider and, with an
//! S3 cache, the burst ledger, the warm start records and the queue-first schedule in the cache
//! bucket, all built from the function's environment. The variable names are
//! `ciburst_scaleway::host::vars`, the ones the host sets.
//!
//! A cron trigger arrives as `POST /` with its arguments as the JSON body (Scaleway docs, "Add a
//! trigger to a function"), so the sweep is recognized by its body, not by its path.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use ciburst_core::cloud_init::is_sha256;
use ciburst_core::composite::Composite;
use ciburst_core::config::{CacheType, Config, parse_config};
use ciburst_core::dispatch::Dispatcher;
use ciburst_core::http::{HttpClient, ReqwestClient};
use ciburst_core::ledger::{GROUP_WAIT_S, Ledger, PipelineRecords, Schedule};
use ciburst_core::log::Log;
use ciburst_core::types::{KeyPair, Provider, Secrets};
use ciburst_hetzner::Hetzner;
use ciburst_scaleway::client::{Credentials, ScalewayClient};
use ciburst_scaleway::host::vars;
use ciburst_scaleway::ledger::ScalewayLedger;
use ciburst_scaleway::provider::{Scaleway, VmStart};
use ciburst_scaleway::records::ScalewayPipelineRecords;
use ciburst_scaleway::schedule::ScalewaySchedule;
use http::{Request, Response, StatusCode};

/// GitLab job events are a few kilobytes, VM reports and cron arguments less. A pipeline event
/// lists every job of the pipeline in under 1 KB each, so this holds more than a thousand jobs.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// What the function reads from its environment.
pub struct Env {
    pub config: Arc<Config>,
    pub secrets: Arc<Secrets>,
    pub credentials: Credentials,
    pub region: String,
    /// Optional override of the callback URL VMs use; by default the webhook's Host header.
    pub dispatch_url: Option<String>,
    /// `HCLOUD_TOKEN`, required when the config adds the `hetzner` provider.
    pub hetzner_token: Option<String>,
    /// `CIBURST_AGENT_SHA256`, the agent new VMs download from the cache bucket (D38); a VM
    /// start without it fails naming the variable.
    pub agent_sha256: Option<String>,
    /// `CIBURST_VM_SECURITY_GROUP` and `CIBURST_VM_IMAGES`, which spare a VM start its lookups
    /// (M7 hardening rule R3); a start without them looks them up.
    pub vm_start: VmStart,
    /// Pause between recording a burst intent and reading the burst (M4 rule R3); not read from
    /// the environment.
    pub group_wait: Duration,
}

/// What the dispatcher works on besides the environment.
pub struct Backends {
    pub provider: Arc<dyn Provider>,
    /// The burst ledger in the cache bucket (M4 rule R13); `None` without an S3 cache key.
    pub ledger: Option<Arc<dyn Ledger>>,
    /// The warm start records beside it (M6.1 rule R5); `None` without an S3 cache key.
    pub records: Option<Arc<dyn PipelineRecords>>,
    /// The queue-first schedule beside it (M6.2 rules R2 to R4); `None` without an S3 cache
    /// key.
    pub schedule: Option<Arc<dyn Schedule>>,
}

impl Backends {
    pub fn new(provider: Arc<dyn Provider>) -> Backends {
        Backends {
            provider,
            ledger: None,
            records: None,
            schedule: None,
        }
    }

    pub fn with_ledger(mut self, ledger: Option<Arc<dyn Ledger>>) -> Backends {
        self.ledger = ledger;
        self
    }

    pub fn with_records(mut self, records: Option<Arc<dyn PipelineRecords>>) -> Backends {
        self.records = records;
        self
    }

    pub fn with_schedule(mut self, schedule: Option<Arc<dyn Schedule>>) -> Backends {
        self.schedule = schedule;
        self
    }
}

/// A variable that is missing or unusable. Carries the name only, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvError {
    Missing(String),
    Invalid(String),
}

impl EnvError {
    pub fn variable(&self) -> &str {
        match self {
            EnvError::Missing(name) | EnvError::Invalid(name) => name,
        }
    }
}

impl fmt::Display for EnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvError::Missing(name) => write!(f, "{name} missing"),
            EnvError::Invalid(name) => write!(f, "{name} invalid"),
        }
    }
}

impl Env {
    /// Reads the variables through `lookup`; empty values count as missing.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Env, EnvError> {
        let optional = |name: &str| lookup(name).filter(|v| !v.is_empty());
        let required =
            |name: &str| optional(name).ok_or_else(|| EnvError::Missing(name.to_string()));

        let config = parse_config(&required(vars::CONFIG)?)
            .map_err(|_| EnvError::Invalid(vars::CONFIG.to_string()))?;
        let config_has_hetzner = config.providers.contains_key(ciburst_hetzner::RUNTIME);
        let mut runner_tokens = BTreeMap::new();
        let mut spill_tokens = BTreeMap::new();
        for pool in &config.pools {
            runner_tokens.insert(pool.tag.clone(), required(&vars::runner_token(&pool.tag))?);
            // The host sets it for a VM pool with a spill runner only (M6.2 rule R6).
            if config.is_vm_pool(pool)
                && let Some(token) = optional(&vars::spill_token(&pool.tag))
            {
                spill_tokens.insert(pool.tag.clone(), token);
            }
        }
        let cache = match (
            optional(vars::CACHE_ACCESS_KEY),
            optional(vars::CACHE_SECRET_KEY),
        ) {
            (Some(access_key), Some(secret_key)) => Some(KeyPair {
                access_key,
                secret_key,
            }),
            _ => None,
        };
        let secrets = Secrets {
            spill_tokens,
            hook_token: Some(required(vars::HOOK_TOKEN)?),
            runner_tokens,
            dispatch: None,
            cache,
            vm_secret: optional(vars::VM_SECRET),
            sweep_token: optional(vars::SWEEP_TOKEN),
            provider_env: BTreeMap::new(),
            dispatcher_env: BTreeMap::new(),
        };
        let credentials = Credentials {
            secret_key: required(vars::SECRET_KEY)?,
            access_key: None,
            organization_id: required(vars::ORGANIZATION_ID)?,
            default_project_id: Some(required(vars::PROJECT_ID)?),
        };
        let agent_sha256 = match optional(vars::AGENT_SHA256) {
            Some(sha256) if !is_sha256(&sha256) => {
                return Err(EnvError::Invalid(vars::AGENT_SHA256.to_string()));
            }
            found => found,
        };
        let vm_start = VmStart::from_vars(
            optional(vars::VM_SECURITY_GROUP).as_deref(),
            optional(vars::VM_IMAGES).as_deref(),
        )
        .map_err(|_| EnvError::Invalid(vars::VM_IMAGES.to_string()))?;
        Ok(Env {
            config: Arc::new(config),
            secrets: Arc::new(secrets),
            credentials,
            region: required(vars::REGION)?,
            dispatch_url: optional("CIBURST_DISPATCH_URL"),
            hetzner_token: if config_has_hetzner {
                Some(required(ciburst_hetzner::TOKEN_VAR)?)
            } else {
                None
            },
            agent_sha256,
            vm_start,
            group_wait: Duration::from_secs(GROUP_WAIT_S),
        })
    }

    pub fn from_process() -> Result<Env, EnvError> {
        Env::from_lookup(|name| std::env::var(name).ok())
    }

    /// Provider, ledger, records and schedule on the real HTTP client.
    pub fn backends(&self, log: Log) -> Result<Backends, EnvError> {
        let http: Arc<dyn HttpClient> = Arc::new(
            ReqwestClient::new().map_err(|_| EnvError::Invalid("http client".to_string()))?,
        );
        Ok(self.backends_on(http, log))
    }

    /// The `Scaleway` provider, in a [`Composite`] with Hetzner when the config adds it
    /// (spec D35), the ledger, the records and the schedule, all on `http`.
    pub fn backends_on(&self, http: Arc<dyn HttpClient>, log: Log) -> Backends {
        let client = ScalewayClient::new(http.clone(), self.credentials.clone(), &self.region);
        let main: Arc<dyn Provider> = Arc::new(
            Scaleway::new(self.config.clone(), Arc::new(client), log.clone())
                .with_agent_sha256(self.agent_sha256.clone())
                .with_vm_start(self.vm_start.clone()),
        );
        let ledger = self.ledger_on(http.clone());
        let records = self.records_on(http.clone());
        let schedule = self.schedule_on(http.clone());
        let Some(token) = &self.hetzner_token else {
            return Backends::new(main)
                .with_ledger(ledger)
                .with_records(records)
                .with_schedule(schedule);
        };
        let hetzner: Arc<dyn Provider> = Arc::new(Hetzner::new(
            self.config.clone(),
            token.clone(),
            http,
            log.clone(),
        ));
        let composite = Composite::new(
            self.config.clone(),
            main,
            vec![(ciburst_hetzner::RUNTIME.to_string(), hetzner)],
            log,
        );
        Backends::new(Arc::new(composite))
            .with_ledger(ledger)
            .with_records(records)
            .with_schedule(schedule)
    }

    /// The burst ledger in the cache bucket `<name>-cache-<CIBURST_PROJECT_ID>` of
    /// `CIBURST_REGION`, signed with the cache key; `None` unless the cache is S3 and both cache
    /// keys are set.
    pub fn ledger_on(&self, http: Arc<dyn HttpClient>) -> Option<Arc<dyn Ledger>> {
        let (key, project_id) = self.cache_bucket()?;
        Some(Arc::new(ScalewayLedger::in_cache_bucket(
            http,
            &self.config,
            project_id,
            &self.region,
            key,
        )))
    }

    /// The warm start records in the ledger's bucket, under the same key and condition.
    pub fn records_on(&self, http: Arc<dyn HttpClient>) -> Option<Arc<dyn PipelineRecords>> {
        let (key, project_id) = self.cache_bucket()?;
        Some(Arc::new(ScalewayPipelineRecords::in_cache_bucket(
            http,
            &self.config,
            project_id,
            &self.region,
            key,
        )))
    }

    /// The queue-first schedule in the ledger's bucket, under the same key and condition.
    pub fn schedule_on(&self, http: Arc<dyn HttpClient>) -> Option<Arc<dyn Schedule>> {
        let (key, project_id) = self.cache_bucket()?;
        Some(Arc::new(ScalewaySchedule::in_cache_bucket(
            http,
            &self.config,
            project_id,
            &self.region,
            key,
        )))
    }

    /// The cache key and the project of the cache bucket, when the cache is S3 and both cache
    /// keys are set.
    fn cache_bucket(&self) -> Option<(&KeyPair, &str)> {
        if self.config.cache.kind != CacheType::S3 {
            return None;
        }
        let key = self.secrets.cache.as_ref()?;
        let project_id = self.credentials.default_project_id.as_deref()?;
        Some((key, project_id))
    }
}

/// One request as the dispatcher sees it.
pub struct Incoming<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
    /// Unix seconds.
    pub now: u64,
}

/// Runs the dispatcher on one request; answers its status and JSON body, empty for none.
pub async fn respond(
    env: &Env,
    backends: &Backends,
    log: Log,
    request: &Incoming<'_>,
) -> (u16, Vec<u8>) {
    let dispatcher = Dispatcher::new(
        env.config.clone(),
        backends.provider.clone(),
        env.secrets.clone(),
        log,
    )
    .with_dispatch_url(env.dispatch_url.clone())
    .with_ledger(backends.ledger.clone())
    .with_records(backends.records.clone())
    .with_schedule(backends.schedule.clone())
    .with_group_wait(env.group_wait);
    let response = dispatcher
        .handle_request(
            request.method,
            request.path,
            request.headers,
            request.body,
            request.now,
        )
        .await;
    let body = if response.body.is_null() {
        Vec::new()
    } else {
        response.body.to_string().into_bytes()
    };
    (response.status, body)
}

/// Reads method, URI path, headers and body of an HTTP request and answers it.
pub async fn serve(
    env: &Env,
    backends: &Backends,
    log: Log,
    req: Request<Body>,
    now: u64,
) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), v.to_string()))
        })
        .collect();
    let (status, body) = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => {
            let request = Incoming {
                method: parts.method.as_str(),
                path: parts.uri.path(),
                headers: &headers,
                body: &bytes,
                now,
            };
            respond(env, backends, log, &request).await
        }
        Err(_) => (400, br#"{"result":"bad_request"}"#.to_vec()),
    };
    http_response(status, body)
}

/// The answer when the environment is unusable: 500, logged with the variable name only.
pub fn env_error(error: &EnvError, log: &Log) -> (u16, Vec<u8>) {
    log.error("env", &[("variable", error.variable())]);
    error_body()
}

fn error_body() -> (u16, Vec<u8>) {
    (500, br#"{"result":"error"}"#.to_vec())
}

struct State {
    env: Env,
    backends: Backends,
}

fn state() -> &'static Result<State, EnvError> {
    static STATE: OnceLock<Result<State, EnvError>> = OnceLock::new();
    STATE.get_or_init(|| {
        let env = Env::from_process()?;
        let backends = env.backends(Log::stderr())?;
        Ok(State { env, backends })
    })
}

pub async fn handle(req: Request<Body>) -> Response<Body> {
    let log = Log::stderr();
    match state() {
        Err(error) => {
            let (status, body) = env_error(error, &log);
            http_response(status, body)
        }
        Ok(state) => serve(&state.env, &state.backends, log, req, now_seconds()).await,
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn http_response(status: u16, body: Vec<u8>) -> Response<Body> {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    if !body.is_empty() {
        builder = builder.header("content-type", "application/json");
    }
    builder.body(Body::from(body)).unwrap_or_else(|_| {
        let (_, body) = error_body();
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        response
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciburst_core::fake::FakeProvider;
    use ciburst_core::store::MemoryStore;
    use ciburst_core::types::{Outputs, Rotation, Run, StartRequest, Step};
    use ciburst_core::{Error, Result};
    use serde_json::{Value, json};

    const CONFIG: &str = r#"{
      "name": "demo",
      "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
      "host": { "type": "scaleway-function" },
      "provider": { "type": "scaleway-jobs" },
      "defaultPool": "ci",
      "pools": [
        { "tag": "ci", "image": "img/node" },
        { "tag": "build-x", "image": "img/kaniko", "stages": ["build"] },
        { "tag": "vm", "image": "img/docker", "stages": ["vm"], "runtime": "instances", "slots": 2 }
      ],
      "cache": { "type": "s3" }
    }"#;

    const NOW: u64 = 1_800_000_000;
    const VM_SECRET: &str = "vm-secret";

    fn vars_map() -> BTreeMap<String, String> {
        [
            (vars::CONFIG, CONFIG),
            (vars::HOOK_TOKEN, "hook-secret"),
            (vars::VM_SECRET, VM_SECRET),
            (vars::SWEEP_TOKEN, "sweep-secret"),
            ("RUNNER_TOKEN_CI", "glrt-ci"),
            ("RUNNER_TOKEN_BUILD_X", "glrt-build"),
            ("RUNNER_TOKEN_VM", "glrt-vm"),
            (vars::CACHE_ACCESS_KEY, "AK"),
            (vars::CACHE_SECRET_KEY, "SK"),
            (vars::SECRET_KEY, "dispatch-secret"),
            (vars::ORGANIZATION_ID, "org"),
            (vars::PROJECT_ID, "project"),
            (vars::REGION, "fr-par"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn env_from(map: &BTreeMap<String, String>) -> std::result::Result<Env, EnvError> {
        Env::from_lookup(|name| map.get(name).cloned())
    }

    async fn fake_provider(env: &Env) -> Arc<FakeProvider> {
        let provider = Arc::new(
            FakeProvider::new(env.config.clone(), Arc::new(MemoryStore::new()))
                .with_clock(Arc::new(|| NOW)),
        );
        provider
            .apply(&provider.plan(&[]).await.unwrap())
            .await
            .unwrap();
        provider
    }

    async fn webhook(
        env: &Env,
        provider: Arc<dyn Provider>,
        log: Log,
        headers: &[(String, String)],
        body: &[u8],
    ) -> (u16, Vec<u8>) {
        let request = Incoming {
            method: "POST",
            path: "/",
            headers,
            body,
            now: NOW,
        };
        respond(env, &Backends::new(provider), log, &request).await
    }

    /// Status, content type and body of `serve` on one HTTP request.
    async fn http(
        env: &Env,
        provider: Arc<dyn Provider>,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        now: u64,
    ) -> (u16, Option<String>, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let req = builder.body(Body::from(body.to_vec())).unwrap();
        let response = serve(env, &Backends::new(provider), Log::silent(), req, now).await;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string());
        let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .unwrap();
        (status, content_type, bytes.to_vec())
    }

    /// Starts a VM through the webhook and answers its run id and token.
    async fn started_vm(env: &Env, provider: Arc<FakeProvider>) -> (String, String) {
        let (status, _, body) = http(
            env,
            provider,
            "POST",
            "/",
            &[("x-gitlab-token", "hook-secret")],
            &event("pending", "vm"),
            NOW,
        )
        .await;
        assert_eq!(status, 200);
        let body = parse(&body);
        assert_eq!(body["result"], "started", "{body}");
        assert_eq!(body["pool"], "vm");
        let run = body["runId"].as_str().unwrap().to_string();
        let token = ciburst_core::vm::token(VM_SECRET, &run);
        (run, token)
    }

    fn headers(token: &str) -> Vec<(String, String)> {
        vec![
            ("x-gitlab-token".to_string(), token.to_string()),
            ("x-gitlab-event".to_string(), "Job Hook".to_string()),
        ]
    }

    fn event(status: &str, stage: &str) -> Vec<u8> {
        json!({
            "object_kind": "build", "build_id": 42, "build_stage": stage,
            "build_status": status, "pipeline_id": 9, "project_id": 7
        })
        .to_string()
        .into_bytes()
    }

    /// The body without `ms`, which every answer of the dispatcher carries as a whole number
    /// (M7 rule R1).
    fn parse(body: &[u8]) -> Value {
        let mut value: Value = serde_json::from_slice(body).unwrap();
        if let Some(ms) = value.as_object_mut().and_then(|map| map.remove("ms")) {
            assert!(ms.is_u64(), "ms {ms}");
        }
        value
    }

    #[test]
    fn reads_every_variable() {
        let env = env_from(&vars_map()).unwrap();
        assert_eq!(env.config.name, "demo");
        assert_eq!(env.secrets.hook_token.as_deref(), Some("hook-secret"));
        assert_eq!(env.secrets.runner_tokens["build-x"], "glrt-build");
        assert_eq!(env.secrets.cache.as_ref().unwrap().secret_key, "SK");
        assert_eq!(env.credentials.secret_key, "dispatch-secret");
        assert_eq!(
            env.credentials.default_project_id.as_deref(),
            Some("project")
        );
        assert_eq!(env.region, "fr-par");

        assert_eq!(env.secrets.vm_secret.as_deref(), Some(VM_SECRET));
        assert_eq!(env.agent_sha256, None);
        let sha = "ab".repeat(32);
        let mut with_agent = vars_map();
        with_agent.insert(vars::AGENT_SHA256.to_string(), sha.clone());
        assert_eq!(env_from(&with_agent).unwrap().agent_sha256, Some(sha));
        with_agent.insert(vars::AGENT_SHA256.to_string(), "AB".repeat(32));
        assert_eq!(
            env_from(&with_agent).err(),
            Some(EnvError::Invalid(vars::AGENT_SHA256.to_string()))
        );

        let mut no_cache = vars_map();
        no_cache.remove(vars::CACHE_SECRET_KEY);
        assert!(env_from(&no_cache).unwrap().secrets.cache.is_none());
        let mut no_vm = vars_map();
        no_vm.remove(vars::VM_SECRET);
        assert!(env_from(&no_vm).unwrap().secrets.vm_secret.is_none());
    }

    /// M6.2 rule R6: `RUNNER_SPILL_TOKEN_<TAG>` of a VM pool, optional, as the host sets it.
    #[test]
    fn reads_the_spill_tokens_of_vm_pools() {
        assert!(
            env_from(&vars_map())
                .unwrap()
                .secrets
                .spill_tokens
                .is_empty()
        );
        let mut spill = vars_map();
        spill.insert(vars::spill_token("vm"), "glrt-spill".to_string());
        spill.insert(vars::spill_token("build-x"), "glrt-lane".to_string());
        let env = env_from(&spill).unwrap();
        assert_eq!(
            env.secrets.spill_tokens,
            BTreeMap::from([("vm".to_string(), "glrt-spill".to_string())]),
            "a lane has no spill runner"
        );
        spill.insert(vars::spill_token("vm"), String::new());
        assert!(env_from(&spill).unwrap().secrets.spill_tokens.is_empty());
    }

    #[test]
    fn reads_what_apply_resolved_for_vm_starts() {
        assert_eq!(env_from(&vars_map()).unwrap().vm_start, VmStart::default());
        let mut resolved = vars_map();
        resolved.insert(vars::VM_SECURITY_GROUP.to_string(), "sg-1".to_string());
        resolved.insert(
            vars::VM_IMAGES.to_string(),
            r#"{"DEV1-M":{"image":"img-1","installDocker":false}}"#.to_string(),
        );
        let start = env_from(&resolved).unwrap().vm_start;
        assert_eq!(start.security_group.as_deref(), Some("sg-1"));
        let images = start.images.unwrap();
        assert_eq!(images["DEV1-M"].image, "img-1");
        assert!(!images["DEV1-M"].install_docker);
        resolved.insert(vars::VM_IMAGES.to_string(), "DEV1-M=img-1".to_string());
        assert_eq!(
            env_from(&resolved).err(),
            Some(EnvError::Invalid(vars::VM_IMAGES.to_string()))
        );
    }

    #[test]
    fn names_the_missing_variable() {
        for name in [
            vars::CONFIG,
            vars::HOOK_TOKEN,
            "RUNNER_TOKEN_BUILD_X",
            vars::SECRET_KEY,
            vars::ORGANIZATION_ID,
            vars::PROJECT_ID,
            vars::REGION,
        ] {
            let mut map = vars_map();
            map.remove(name);
            let error = env_from(&map).err().unwrap();
            assert_eq!(error, EnvError::Missing(name.to_string()));
            let mut empty = vars_map();
            empty.insert(name.to_string(), String::new());
            assert_eq!(env_from(&empty).err().unwrap().variable(), name);
        }
        let mut bad = vars_map();
        bad.insert(vars::CONFIG.to_string(), "{".to_string());
        assert_eq!(
            env_from(&bad).err().unwrap(),
            EnvError::Invalid(vars::CONFIG.to_string())
        );
    }

    #[test]
    fn an_env_error_answers_500_and_logs_the_name_only() {
        let mut map = vars_map();
        map.remove(vars::HOOK_TOKEN);
        let error = env_from(&map).err().unwrap();
        let (log, lines) = Log::memory();
        let (status, body) = env_error(&error, &log);
        assert_eq!(status, 500);
        assert_eq!(parse(&body), json!({ "result": "error" }));
        let lines = lines.lock().unwrap();
        assert_eq!(lines.as_slice(), [r#"error env variable="HOOK_TOKEN""#]);
    }

    #[tokio::test]
    async fn rejects_requests_without_the_token() {
        let env = env_from(&vars_map()).unwrap();
        let provider = fake_provider(&env).await;
        let (status, _) = webhook(
            &env,
            provider.clone(),
            Log::silent(),
            &[],
            &event("pending", "test"),
        )
        .await;
        assert_eq!(status, 401);
        let (status, _) = webhook(
            &env,
            provider.clone(),
            Log::silent(),
            &headers("wrong"),
            &event("pending", "test"),
        )
        .await;
        assert_eq!(status, 401);
        assert!(provider.list(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignores_jobs_that_are_not_pending() {
        let env = env_from(&vars_map()).unwrap();
        let provider = fake_provider(&env).await;
        let (status, body) = webhook(
            &env,
            provider.clone(),
            Log::silent(),
            &headers("hook-secret"),
            &event("running", "test"),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            parse(&body),
            json!({ "result": "ignored", "reason": "status", "status": "running" })
        );
        assert!(provider.list(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn starts_a_run_for_a_pending_job() {
        let env = env_from(&vars_map()).unwrap();
        let provider = fake_provider(&env).await;
        let (log, lines) = Log::memory();
        let (status, body) = webhook(
            &env,
            provider.clone(),
            log,
            &headers("hook-secret"),
            &event("pending", "build"),
        )
        .await;
        assert_eq!(status, 200);
        let body = parse(&body);
        assert_eq!(body["result"], "started");
        assert_eq!(body["pool"], "build-x");
        let runs = provider.list(Some("build-x")).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].labels["job"], "42");
        let lines = lines.lock().unwrap();
        assert!(lines.iter().any(|l| l.starts_with("info started")));
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("glrt-") || l.contains("SK"))
        );
    }

    struct Failing(Arc<FakeProvider>);

    #[async_trait::async_trait]
    impl Provider for Failing {
        fn capabilities(&self) -> ciburst_core::types::Capabilities {
            self.0.capabilities()
        }
        async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
            self.0.plan(rotate).await
        }
        async fn apply(&self, steps: &[Step]) -> Result<Outputs> {
            self.0.apply(steps).await
        }
        async fn destroy(&self) -> Result<()> {
            self.0.destroy().await
        }
        async fn start(&self, _request: StartRequest) -> Result<String> {
            Err(Error::provider("scaleway POST /x: 403 quota"))
        }
        async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
            self.0.list(pool).await
        }
        async fn stop(&self, run_id: &str) -> Result<()> {
            self.0.stop(run_id).await
        }
    }

    #[tokio::test]
    async fn answers_500_when_the_provider_fails() {
        let env = env_from(&vars_map()).unwrap();
        let provider = Arc::new(Failing(fake_provider(&env).await));
        let (status, body) = webhook(
            &env,
            provider,
            Log::silent(),
            &headers("hook-secret"),
            &event("pending", "test"),
        )
        .await;
        assert_eq!(status, 500);
        assert_eq!(parse(&body), json!({ "result": "error" }));
    }

    #[tokio::test]
    async fn handle_answers_json_with_the_status() {
        // The test process has no function environment, so `handle` reports it.
        let response = handle(
            Request::builder()
                .method("POST")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.headers()["content-type"], "application/json");
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(parse(&bytes), json!({ "result": "error" }));
    }

    /// A pipeline event lists every job of the pipeline (M6.1 rule R2): one of a thousand jobs,
    /// each with the fields GitLab sends, fits the body limit and reaches the dispatcher.
    #[tokio::test]
    async fn a_pipeline_event_of_a_thousand_jobs_reaches_the_dispatcher() {
        let env = env_from(&vars_map()).unwrap();
        let provider = fake_provider(&env).await;
        let build = |id: u64| {
            json!({
                "id": id, "stage": "test", "name": format!("job-{id}"), "status": "created",
                "created_at": "2026-10-25 08:00:00 UTC", "started_at": null,
                "finished_at": null, "duration": null, "queued_duration": null,
                "failure_reason": null, "when": "on_success", "manual": false,
                "allow_failure": false,
                "user": {
                    "id": 1, "name": "Maria Muster", "username": "muster",
                    "avatar_url": "https://gitlab.example/uploads/-/system/user/avatar/1/avatar.png",
                    "email": "[REDACTED]"
                },
                "runner": {
                    "id": 12, "description": "ci-7-vm", "runner_type": "project_type",
                    "active": true, "is_shared": false, "tags": ["vm"]
                },
                "artifacts_file": { "filename": null, "size": null },
                "environment": null
            })
        };
        let event = |builds: u64| {
            json!({
                "object_kind": "pipeline",
                "object_attributes": { "id": 9, "status": "pending" },
                "project": { "id": 7 },
                "builds": (1..=builds).map(build).collect::<Vec<_>>()
            })
            .to_string()
            .into_bytes()
        };
        let headers = [
            ("x-gitlab-token", "hook-secret"),
            ("x-gitlab-event", "Pipeline Hook"),
        ];
        let body = event(1000);
        assert!(body.len() < MAX_BODY_BYTES, "{}", body.len());
        let (status, _, answer) =
            http(&env, provider.clone(), "POST", "/", &headers, &body, NOW).await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
        let body = event(2000);
        assert!(body.len() > MAX_BODY_BYTES, "{}", body.len());
        let (status, _, answer) = http(&env, provider, "POST", "/", &headers, &body, NOW).await;
        assert_eq!(
            (status, parse(&answer)),
            (400, json!({ "result": "bad_request" }))
        );
    }

    #[tokio::test]
    async fn a_vm_reports_by_path_with_its_token() {
        let env = env_from(&vars_map()).unwrap();
        let provider = fake_provider(&env).await;
        let (run, token) = started_vm(&env, provider.clone()).await;
        let report = br#"{"state":"idle","free":2,"slots":2}"#;

        for headers in [
            vec![],
            vec![("x-ciburst-run", run.as_str())],
            vec![("x-ciburst-run", run.as_str()), ("x-ciburst-token", "0")],
            vec![("x-gitlab-token", "hook-secret")],
        ] {
            let (status, content_type, body) =
                http(&env, provider.clone(), "POST", "/vm", &headers, report, NOW).await;
            assert_eq!(status, 401, "{headers:?}");
            assert_eq!(content_type, None);
            assert!(body.is_empty(), "401 has no body");
        }
        let (status, _, _) = http(
            &env,
            provider.clone(),
            "GET",
            "/vm",
            &[("X-Ciburst-Run", &run), ("X-Ciburst-Token", &token)],
            b"",
            NOW,
        )
        .await;
        assert_eq!(status, 401, "only POST reaches the VM routes");

        for uri in [
            "/vm",
            "https://demo-dispatch.functions.fnc.fr-par.scw.cloud/vm",
        ] {
            let (status, content_type, body) = http(
                &env,
                provider.clone(),
                "POST",
                uri,
                &[("X-Ciburst-Run", &run), ("X-Ciburst-Token", &token)],
                report,
                NOW + 5,
            )
            .await;
            assert_eq!(status, 200, "{uri}");
            assert_eq!(content_type.as_deref(), Some("application/json"));
            assert_eq!(
                parse(&body),
                json!({ "result": "reported", "spill": false, "nextReportS": 600 })
            );
        }
        let vms = provider.capacity("vm").await.unwrap().unwrap();
        assert_eq!(vms[0].state, ciburst_core::types::VmState::Idle);
        assert_eq!(vms[0].seen, Some(NOW + 5));
    }

    #[tokio::test]
    async fn a_vm_terminates_itself_by_path() {
        let env = env_from(&vars_map()).unwrap();
        let provider = fake_provider(&env).await;
        let (run, token) = started_vm(&env, provider.clone()).await;
        let (other, _) = started_vm(&env, provider.clone()).await;
        let other_token = ciburst_core::vm::token(VM_SECRET, &other);
        let (status, _, body) = http(
            &env,
            provider.clone(),
            "POST",
            "/terminate",
            &[("X-Ciburst-Run", &run), ("X-Ciburst-Token", &other_token)],
            b"",
            NOW,
        )
        .await;
        assert_eq!((status, body.is_empty()), (401, true));
        let (status, _, body) = http(
            &env,
            provider.clone(),
            "POST",
            "/terminate",
            &[("X-Ciburst-Run", &run), ("X-Ciburst-Token", &token)],
            b"",
            NOW,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(parse(&body), json!({ "result": "terminated" }));
        let vms = provider.capacity("vm").await.unwrap().unwrap();
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0].run_id, other);
    }

    #[tokio::test]
    async fn the_cron_body_sweeps() {
        let env = env_from(&vars_map()).unwrap();
        let provider = fake_provider(&env).await;
        let (run, _) = started_vm(&env, provider.clone()).await;
        let stuck = NOW + ciburst_core::vm::SWEEP_STARTING_S + 1;
        let (status, content_type, body) = http(
            &env,
            provider.clone(),
            "POST",
            "/",
            &[],
            br#"{"ciburst_sweep":"wrong"}"#,
            stuck,
        )
        .await;
        assert_eq!((status, content_type, body.is_empty()), (401, None, true));
        assert_eq!(provider.capacity("vm").await.unwrap().unwrap().len(), 1);

        let (status, _, body) = http(
            &env,
            provider.clone(),
            "POST",
            "/",
            &[("content-type", "application/json")],
            br#"{"ciburst_sweep":"sweep-secret"}"#,
            stuck,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(parse(&body), json!({ "result": "swept", "runs": [run] }));
        assert!(provider.capacity("vm").await.unwrap().unwrap().is_empty());
    }

    #[tokio::test]
    async fn one_job_pools_keep_counting_runs() {
        let env = env_from(&vars_map()).unwrap();
        let provider = fake_provider(&env).await;
        let (status, _, body) = http(
            &env,
            provider.clone(),
            "POST",
            "/",
            &[("x-gitlab-token", "hook-secret")],
            &event("pending", "build"),
            NOW,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(parse(&body)["pool"], "build-x");
        assert_eq!(provider.capacity("build-x").await.unwrap(), None);
    }
}
