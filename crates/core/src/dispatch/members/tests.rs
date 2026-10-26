use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::super::{DispatchResponse, Dispatcher};
use super::{FLEET_CACHE, FleetCache, PATH_FLEET, PATH_JOIN, PATH_LEAVE};
use crate::config::{Config, parse_config};
use crate::fake::FakeProvider;
use crate::fleet::{FleetStore, Member, MemoryFleetStore};
use crate::invite::{self, TTL_S};
use crate::log::Log;
use crate::store::MemoryStore;
use crate::types::{
    Capabilities, KeyPair, Outputs, Provider, Rotation, Run, Secrets, StartRequest, Step,
    VmCapacity, VmReport, VmReportAnswer, VmState,
};
use crate::vm;
use crate::{Error, Result};

const NOW: u64 = 1_800_000_000;
const HOOK: &str = "hook-secret";
const VM_SECRET: &str = "vm-secret";
const URL: &str = "https://dispatch.example";

fn config_text(default_pool: &str, project_ids: &str) -> String {
    format!(
        r#"{{
          "name": "fleet",
          "gitlab": {{ "url": "https://gitlab.example", "projectIds": {project_ids} }},
          "host": {{ "type": "fake" }}, "provider": {{ "type": "fake" }},
          "defaultPool": "{default_pool}",
          "pools": [
            {{ "tag": "vm", "image": "img/node", "runtime": "instances", "slots": 2, "maxRuns": 4 }},
            {{ "tag": "jobs", "image": "img/kaniko", "stages": ["build"], "runtime": "jobs" }}
          ],
          "cache": {{ "type": "s3" }}
        }}"#
    )
}

fn config(project_ids: &str) -> Arc<Config> {
    Arc::new(parse_config(&config_text("vm", project_ids)).unwrap())
}

fn secrets() -> Secrets {
    Secrets {
        hook_token: Some(HOOK.into()),
        runner_tokens: BTreeMap::from([
            ("vm".to_string(), "glrt-vm".to_string()),
            ("jobs".to_string(), "glrt-jobs".to_string()),
        ]),
        cache: Some(KeyPair {
            access_key: "AK".into(),
            secret_key: "SK".into(),
        }),
        vm_secret: Some(VM_SECRET.into()),
        ..Secrets::default()
    }
}

fn member(project_id: u64, runners: &[(&str, &str)]) -> Member {
    Member {
        project_id,
        hook_secret: invite::hook_secret(HOOK, project_id),
        runners: runners
            .iter()
            .map(|(tag, token)| (tag.to_string(), token.to_string()))
            .collect(),
        joined_at: NOW - 100,
    }
}

/// The fake provider; records each start request.
struct Capture {
    inner: FakeProvider,
    started: Mutex<Vec<StartRequest>>,
}

#[async_trait]
impl Provider for Capture {
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
}

/// A fleet store whose reads fail while `failing` is set.
struct Flaky {
    inner: MemoryFleetStore,
    failing: Mutex<bool>,
}

impl Flaky {
    fn check(&self) -> Result<()> {
        if *self.failing.lock().unwrap() {
            return Err(Error::other("bucket down"));
        }
        Ok(())
    }
}

#[async_trait]
impl FleetStore for Flaky {
    async fn config(&self) -> Result<Option<String>> {
        self.check()?;
        self.inner.config().await
    }
    async fn put_config(&self, json: &str) -> Result<()> {
        self.inner.put_config(json).await
    }
    async fn members(&self) -> Result<Vec<Member>> {
        self.check()?;
        self.inner.members().await
    }
    async fn put_member(&self, member: &Member) -> Result<()> {
        self.check()?;
        self.inner.put_member(member).await
    }
    async fn delete_member(&self, project_id: u64) -> Result<()> {
        self.check()?;
        self.inner.delete_member(project_id).await
    }
}

struct Setup {
    provider: Arc<Capture>,
    store: Arc<Flaky>,
    cache: Arc<FleetCache>,
    config: Arc<Config>,
    lines: Arc<Mutex<Vec<String>>>,
    log: Log,
    fleet: bool,
}

async fn setup(project_ids: &str, fleet: bool) -> Setup {
    let config = config(project_ids);
    let inner = FakeProvider::new(config.clone(), Arc::new(MemoryStore::new()))
        .with_clock(Arc::new(|| NOW - 3600));
    inner.apply(&inner.plan(&[]).await.unwrap()).await.unwrap();
    let store = Arc::new(Flaky {
        inner: MemoryFleetStore::new(),
        failing: Mutex::new(false),
    });
    let (log, lines) = Log::memory();
    Setup {
        provider: Arc::new(Capture {
            inner,
            started: Mutex::new(Vec::new()),
        }),
        store,
        cache: Arc::new(FleetCache::new()),
        config,
        lines,
        log,
        fleet,
    }
}

impl Setup {
    /// A dispatcher per request, as the function builds one, sharing the cache.
    fn dispatcher(&self) -> Dispatcher {
        let fleet: Option<Arc<dyn FleetStore>> = self.fleet.then(|| self.store.clone() as _);
        Dispatcher::new(
            self.config.clone(),
            self.provider.clone(),
            Arc::new(secrets()),
            self.log.clone(),
        )
        .with_dispatch_url(Some(URL.into()))
        .with_fleet(fleet)
        .with_fleet_cache(self.cache.clone())
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        now: u64,
    ) -> DispatchResponse {
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        self.dispatcher()
            .handle_request(method, path, &headers, body, now)
            .await
    }

    async fn event(&self, token: &str, project: u64, stage: &str, job: u64) -> DispatchResponse {
        let body = json!({
            "object_kind": "build", "build_id": job, "build_stage": stage,
            "build_name": format!("job-{job}"), "build_status": "pending",
            "pipeline_id": 9, "project_id": project
        });
        self.request(
            "POST",
            "/",
            &[("X-Gitlab-Token", token)],
            body.to_string().as_bytes(),
            NOW,
        )
        .await
    }

    async fn put(&self, member: Member) {
        self.store.inner.put_member(&member).await.unwrap();
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    fn started(&self) -> Vec<StartRequest> {
        self.provider.started.lock().unwrap().clone()
    }
}

fn code_for(project: u64) -> String {
    invite::issue(HOOK, URL, project, NOW)
}

#[tokio::test(start_paused = true)]
async fn a_members_events_need_its_own_secret() {
    let s = setup("[7]", true).await;
    s.put(member(8, &[("vm", "glrt-8"), ("jobs", "glrt-8-jobs")]))
        .await;
    s.put(member(9, &[("vm", "glrt-9")])).await;
    let secret8 = invite::hook_secret(HOOK, 8);
    let secret9 = invite::hook_secret(HOOK, 9);

    let own = s.event(&secret8, 8, "test", 1).await;
    assert_eq!(own.body["result"], "started", "{}", own.body);
    assert_eq!(s.event(&secret9, 8, "test", 2).await.status, 401);
    assert_eq!(
        s.event(HOOK, 8, "test", 3).await.status,
        401,
        "the host's token is not a member's"
    );
    assert_eq!(s.event(&secret8, 7, "test", 4).await.status, 401);
    assert_eq!(s.event(HOOK, 7, "test", 5).await.body["result"], "started");
    let unknown = s.event(HOOK, 10, "test", 6).await;
    assert_eq!(unknown.body["reason"], "project", "{}", unknown.body);
    assert_eq!(s.event(&secret8, 10, "test", 7).await.status, 401);

    let pipeline = json!({
        "object_kind": "pipeline", "project": { "id": 8 },
        "object_attributes": { "id": 3, "status": "success" }
    })
    .to_string();
    let other = s
        .request(
            "POST",
            "/",
            &[("X-Gitlab-Token", &secret9)],
            pipeline.as_bytes(),
            NOW,
        )
        .await;
    assert_eq!(other.status, 401);
    let closed = s
        .request(
            "POST",
            "/",
            &[("X-Gitlab-Token", &secret8)],
            pipeline.as_bytes(),
            NOW,
        )
        .await;
    assert_eq!(closed.body["result"], "closed");
}

#[tokio::test(start_paused = true)]
async fn without_a_fleet_the_host_token_serves_the_config_projects() {
    let s = setup("[7, 11]", false).await;
    s.put(member(8, &[("vm", "glrt-8")])).await;
    assert_eq!(s.event(HOOK, 11, "test", 1).await.body["result"], "started");
    let member = s.event(&invite::hook_secret(HOOK, 8), 8, "test", 2).await;
    assert_eq!(member.status, 401, "members need the fleet store");
    let join = s
        .request(
            "POST",
            PATH_JOIN,
            &[(invite::HEADER, &code_for(8))],
            br#"{"runners":{"vm":"glrt-8"}}"#,
            NOW,
        )
        .await;
    assert_eq!(join.status, 503);
    assert_eq!(join.body["result"], "no_fleet");
}

#[tokio::test(start_paused = true)]
async fn a_config_change_takes_effect_within_the_cache_time() {
    let s = setup("[7]", true).await;
    let before = s.event(HOOK, 7, "deploy", 1).await;
    assert_eq!(
        before.body["pool"], "vm",
        "the env config without a stored one"
    );
    s.store
        .inner
        .put_config(&config_text("jobs", "[7]"))
        .await
        .unwrap();
    tokio::time::advance(FLEET_CACHE - std::time::Duration::from_secs(1)).await;
    assert_eq!(s.event(HOOK, 7, "deploy", 2).await.body["pool"], "vm");
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let after = s.event(HOOK, 7, "deploy", 3).await;
    assert_eq!(after.body["pool"], "jobs", "{}", after.body);
    assert_eq!(s.started().last().unwrap().pool.tag, "jobs");
}

#[tokio::test(start_paused = true)]
async fn a_failed_read_keeps_the_last_state() {
    let s = setup("[7]", true).await;
    *s.store.failing.lock().unwrap() = true;
    let first = s.event(HOOK, 7, "deploy", 1).await;
    assert_eq!(first.body["pool"], "vm", "the env config");
    let lines = s.lines();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("warn fleet_failed") && l.contains("op=\"config\"")),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("op=\"members\"")));

    *s.store.failing.lock().unwrap() = false;
    s.store
        .inner
        .put_config(&config_text("jobs", "[7]"))
        .await
        .unwrap();
    s.put(member(8, &[("vm", "glrt-8"), ("jobs", "glrt-8-jobs")]))
        .await;
    let read = s.event(HOOK, 7, "deploy", 2).await;
    assert_eq!(
        read.body["pool"], "jobs",
        "a failed read is retried at once"
    );

    *s.store.failing.lock().unwrap() = true;
    tokio::time::advance(FLEET_CACHE).await;
    assert_eq!(s.event(HOOK, 7, "deploy", 3).await.body["pool"], "jobs");
    let secret8 = invite::hook_secret(HOOK, 8);
    assert_eq!(
        s.event(&secret8, 8, "test", 4).await.body["result"],
        "started",
        "the members read last"
    );
}

#[tokio::test(start_paused = true)]
async fn a_member_joins_with_a_code_and_leaves() {
    let s = setup("[7]", true).await;
    let code = code_for(8);
    let fleet = s
        .request("GET", PATH_FLEET, &[(invite::HEADER, &code)], b"", NOW)
        .await;
    assert_eq!(fleet.status, 200, "{}", fleet.body);
    assert_eq!(
        fleet.body,
        json!({
            "result": "fleet", "name": "fleet", "ms": 0,
            "pools": [
                { "tag": "vm", "runtime": "instances", "tags": ["vm", "jobs"] },
                { "tag": "jobs", "runtime": "jobs", "tags": ["jobs"] }
            ],
            "hookSecret": invite::hook_secret(HOOK, 8),
        })
    );

    let body = br#"{"runners":{"vm":"glrt-8","jobs":"glrt-8-jobs"}}"#;
    let join = s
        .request("POST", PATH_JOIN, &[(invite::HEADER, &code)], body, NOW)
        .await;
    assert_eq!(join.status, 200, "{}", join.body);
    assert_eq!(
        join.body,
        json!({ "result": "joined", "fleet": "fleet", "ms": 0 })
    );
    let stored = s.store.inner.members().await.unwrap();
    assert_eq!(
        stored,
        [Member {
            project_id: 8,
            hook_secret: invite::hook_secret(HOOK, 8),
            runners: BTreeMap::from([
                ("jobs".to_string(), "glrt-8-jobs".to_string()),
                ("vm".to_string(), "glrt-8".to_string()),
            ]),
            joined_at: NOW,
        }]
    );
    assert!(
        s.lines()
            .iter()
            .any(|l| l.starts_with("info joined") && l.contains("project=\"8\""))
    );
    let secret = invite::hook_secret(HOOK, 8);
    let lane = s.event(&secret, 8, "build", 1).await;
    assert_eq!(
        lane.body["result"], "started",
        "at once, without a cache wait"
    );
    let started = s.started();
    assert_eq!(started[0].env["RUNNER_TOKEN"], "glrt-8-jobs");

    let again = s
        .request(
            "POST",
            PATH_JOIN,
            &[(invite::HEADER, &code)],
            br#"{"runners":{"vm":"glrt-8b"}}"#,
            NOW + 5,
        )
        .await;
    assert_eq!(again.body["result"], "joined");
    let stored = s.store.inner.members().await.unwrap();
    assert_eq!(stored.len(), 1, "a second join replaces the record");
    assert_eq!(
        stored[0].runners,
        BTreeMap::from([("vm".into(), "glrt-8b".into())])
    );
    let no_lane = s.event(&secret, 8, "build", 2).await;
    assert_eq!(
        no_lane.body,
        json!({ "result": "ignored", "reason": "no_runner", "pool": "jobs", "ms": 0 })
    );

    let expired = invite::issue(HOOK, URL, 8, NOW - TTL_S);
    let leave = s
        .request("POST", PATH_LEAVE, &[(invite::HEADER, &expired)], b"", NOW)
        .await;
    assert_eq!(
        leave.body,
        json!({ "result": "left", "fleet": "fleet", "ms": 0 }),
        "an expired code still leaves"
    );
    assert!(s.store.inner.members().await.unwrap().is_empty());
    assert_eq!(s.event(&secret, 8, "test", 3).await.status, 401);
    assert!(s.lines().iter().any(|l| l.starts_with("info left")));
}

#[tokio::test(start_paused = true)]
async fn bad_codes_unknown_pools_and_the_host_are_refused() {
    let s = setup("[7]", true).await;
    let runners = br#"{"runners":{"vm":"glrt-8"}}"#;
    let expired = invite::issue(HOOK, URL, 8, NOW - TTL_S);
    let forged = invite::issue("another-hook", URL, 8, NOW);
    for code in [expired.as_str(), forged.as_str(), "", "garbage"] {
        for (method, path) in [("GET", PATH_FLEET), ("POST", PATH_JOIN)] {
            let response = s
                .request(method, path, &[(invite::HEADER, code)], runners, NOW)
                .await;
            assert_eq!(response.status, 401, "{method} {path} {code}");
        }
    }
    let leave = s
        .request("POST", PATH_LEAVE, &[(invite::HEADER, &forged)], b"", NOW)
        .await;
    assert_eq!(leave.status, 401);
    let lines = s.lines();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("warn join_refused") && l.contains("reason=\"expired\""))
    );
    assert!(lines.iter().any(|l| l.contains("reason=\"forged\"")));

    let code = code_for(8);
    let unknown = s
        .request(
            "POST",
            PATH_JOIN,
            &[(invite::HEADER, &code)],
            br#"{"runners":{"vm":"glrt-8","gpu":"glrt-x"}}"#,
            NOW,
        )
        .await;
    assert_eq!(unknown.status, 400);
    assert_eq!(unknown.body["result"], "unknown_pools");
    assert_eq!(unknown.body["pools"], json!(["gpu"]));
    for body in [&b"{}"[..], b"{\"runners\":{}}", b"nope"] {
        let bad = s
            .request("POST", PATH_JOIN, &[(invite::HEADER, &code)], body, NOW)
            .await;
        assert_eq!(bad.status, 400);
    }
    assert!(s.store.inner.members().await.unwrap().is_empty());

    let host = code_for(7);
    for (method, path) in [
        ("GET", PATH_FLEET),
        ("POST", PATH_JOIN),
        ("POST", PATH_LEAVE),
    ] {
        let response = s
            .request(method, path, &[(invite::HEADER, &host)], runners, NOW)
            .await;
        assert_eq!(response.status, 400, "{path}");
        assert_eq!(response.body["reason"], "host");
    }
}

#[tokio::test(start_paused = true)]
async fn vm_reports_answer_the_member_runners_of_their_pool() {
    let s = setup("[7, 11]", true).await;
    s.put(member(8, &[("vm", "glrt-8"), ("jobs", "glrt-8-jobs")]))
        .await;
    s.put(member(9, &[("jobs", "glrt-9-jobs")])).await;
    s.put(member(12, &[("vm", "glrt-vm")])).await;
    let started = s.event(HOOK, 7, "test", 1).await;
    assert_eq!(started.body["result"], "started", "{}", started.body);
    let run = started.body["runId"].as_str().unwrap().to_string();
    let vm_env = &s.started()[0].env;
    assert_eq!(
        vm_env["RUNNER_TOKEN"], "glrt-vm",
        "a VM starts with the host's"
    );

    let report = serde_json::to_vec(&VmReport {
        state: VmState::Idle,
        free: 2,
        slots: 2,
        spill: 0,
    })
    .unwrap();
    let token = vm::token(VM_SECRET, &run);
    let answer = s
        .request(
            "POST",
            vm::PATH_REPORT,
            &[(vm::HEADER_RUN, &run), (vm::HEADER_TOKEN, &token)],
            &report,
            NOW,
        )
        .await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(
        answer.body["runners"],
        json!([{ "projectId": 8, "token": "glrt-8" }]),
        "project 11 and 12 use the host's runner, 9 has none of the pool"
    );
    let parsed: VmReportAnswer = serde_json::from_value(answer.body.clone()).unwrap();
    assert_eq!(parsed.runners.len(), 1);

    s.store.inner.delete_member(8).await.unwrap();
    tokio::time::advance(FLEET_CACHE).await;
    let after = s
        .request(
            "POST",
            vm::PATH_REPORT,
            &[(vm::HEADER_RUN, &run), (vm::HEADER_TOKEN, &token)],
            &report,
            NOW + 30,
        )
        .await;
    assert_eq!(after.body.get("runners"), None::<&Value>);
}

#[tokio::test(start_paused = true)]
async fn further_config_projects_use_the_host_runners_until_they_join() {
    let s = setup("[7, 11]", true).await;
    let lane = s.event(HOOK, 11, "build", 1).await;
    assert_eq!(lane.body["result"], "started");
    assert_eq!(s.started()[0].env["RUNNER_TOKEN"], "glrt-jobs");
    s.put(member(11, &[("jobs", "glrt-11-jobs")])).await;
    tokio::time::advance(FLEET_CACHE).await;
    assert_eq!(s.event(HOOK, 11, "build", 2).await.status, 401);
    let own = s
        .event(&invite::hook_secret(HOOK, 11), 11, "build", 3)
        .await;
    assert_eq!(own.body["result"], "started");
    assert_eq!(s.started()[1].env["RUNNER_TOKEN"], "glrt-11-jobs");
}
