use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::super::{DispatchResponse, Dispatcher};
use super::{FLEET_CACHE, FleetCache, JOIN_BUDGET, PATH_FLEET, PATH_JOIN, PATH_LEAVE};
use crate::config::Pool;
use crate::config::{Config, parse_config};
use crate::fake::FakeProvider;
use crate::fleet::{FleetStore, Member, MemberLane, MemoryFleetStore};
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

/// The host's config: the VM runner and the host's lane `jobs` with the patterns `jobs`.
fn config_text(jobs: &str, project_ids: &str) -> String {
    format!(
        r#"{{
          "name": "fleet",
          "gitlab": {{ "url": "https://gitlab.example", "projectIds": {project_ids} }},
          "host": {{ "type": "fake" }}, "provider": {{ "type": "fake" }},
          "vm": {{ "image": "img/node", "slots": 2, "maxRuns": 4 }},
          "lanes": [ {{ "tag": "jobs", "image": "img/kaniko", "jobs": {jobs} }} ],
          "cache": {{ "type": "s3" }}
        }}"#
    )
}

fn config(project_ids: &str) -> Arc<Config> {
    Arc::new(parse_config(&config_text(r#"["build"]"#, project_ids)).unwrap())
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
        ..Default::default()
    }
}

/// The fake provider; records each start request and may delay `sync_lanes`.
struct Capture {
    inner: FakeProvider,
    started: Mutex<Vec<StartRequest>>,
    sync_delay: Mutex<std::time::Duration>,
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
    async fn sync_lanes(&self, project: u64, lanes: &[Pool]) -> Result<BTreeMap<String, String>> {
        let delay = *self.sync_delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        self.inner.sync_lanes(project, lanes).await
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
            sync_delay: Mutex::new(std::time::Duration::ZERO),
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

    /// A pending event of job `job` of `project` named `name`; lanes take jobs by name.
    async fn event(&self, token: &str, project: u64, name: &str, job: u64) -> DispatchResponse {
        let body = json!({
            "object_kind": "build", "build_id": job, "build_stage": "test",
            "build_name": name, "build_status": "pending",
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

    async fn join(&self, project: u64, body: Value, now: u64) -> DispatchResponse {
        self.request(
            "POST",
            PATH_JOIN,
            &[(invite::HEADER, &code_for(project))],
            body.to_string().as_bytes(),
            now,
        )
        .await
    }
}

/// A join body with the VM runner token `vm` and `lanes` as (tag, image, patterns).
fn join_body(vm: &str, lanes: &[(&str, &str, &[&str])]) -> Value {
    let lane_runners: BTreeMap<String, String> = lanes
        .iter()
        .map(|(tag, _, _)| (tag.to_string(), format!("{vm}-{tag}")))
        .collect();
    let lanes: Vec<Value> = lanes
        .iter()
        .map(|(tag, image, jobs)| json!({ "tag": tag, "image": image, "jobs": jobs }))
        .collect();
    json!({ "runners": { "vm": vm }, "lanes": lanes, "laneRunners": lane_runners })
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
        before.body["runner"], "vm",
        "the env config without a stored one"
    );
    s.store
        .inner
        .put_config(&config_text(r#"["build", "deploy"]"#, "[7]"))
        .await
        .unwrap();
    tokio::time::advance(FLEET_CACHE - std::time::Duration::from_secs(1)).await;
    assert_eq!(s.event(HOOK, 7, "deploy", 2).await.body["runner"], "vm");
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let after = s.event(HOOK, 7, "deploy", 3).await;
    assert_eq!(after.body["runner"], "jobs", "{}", after.body);
    assert_eq!(s.started().last().unwrap().pool.tag, "7-jobs");
}

#[tokio::test(start_paused = true)]
async fn a_failed_read_keeps_the_last_state() {
    let s = setup("[7]", true).await;
    *s.store.failing.lock().unwrap() = true;
    let first = s.event(HOOK, 7, "deploy", 1).await;
    assert_eq!(first.body["runner"], "vm", "the env config");
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
        .put_config(&config_text(r#"["build", "deploy"]"#, "[7]"))
        .await
        .unwrap();
    s.put(member(8, &[("vm", "glrt-8")])).await;
    let read = s.event(HOOK, 7, "deploy", 2).await;
    assert_eq!(
        read.body["runner"], "jobs",
        "a failed read is retried at once"
    );

    *s.store.failing.lock().unwrap() = true;
    tokio::time::advance(FLEET_CACHE).await;
    assert_eq!(s.event(HOOK, 7, "deploy", 3).await.body["runner"], "jobs");
    let secret8 = invite::hook_secret(HOOK, 8);
    assert_eq!(
        s.event(&secret8, 8, "test", 4).await.body["result"],
        "started",
        "the members read last"
    );
}

#[tokio::test(start_paused = true)]
async fn a_member_joins_with_its_lanes_and_leaves() {
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
            "vm": { "tag": "vm", "runtime": "instances" },
            "hookSecret": invite::hook_secret(HOOK, 8),
        })
    );

    let mut body = join_body("glrt-8", &[("jobs", "img/node", &["build", "unit-*"])]);
    body["ignoreJobs"] = json!(["pages"]);
    let join = s.join(8, body, NOW).await;
    assert_eq!(join.status, 200, "{}", join.body);
    assert_eq!(
        join.body,
        json!({ "result": "joined", "fleet": "fleet", "lanes": ["jobs"], "ms": 0 })
    );
    let stored = s.store.inner.members().await.unwrap();
    assert_eq!(
        stored,
        [Member {
            project_id: 8,
            hook_secret: invite::hook_secret(HOOK, 8),
            runners: BTreeMap::from([("vm".to_string(), "glrt-8".to_string())]),
            lanes: vec![MemberLane {
                tag: "jobs".into(),
                image: "img/node".into(),
                jobs: vec!["build".into(), "unit-*".into()],
                max_runs: 10,
                definition_id: "def-8-jobs".into(),
                runner_token: "glrt-8-jobs".into(),
            }],
            ignore_jobs: vec!["pages".into()],
            joined_at: NOW,
        }]
    );
    assert!(s.lines().iter().any(|l| l.starts_with("info joined")
        && l.contains(r#"project="8""#)
        && l.contains(r#"lanes="jobs""#)));
    let secret = invite::hook_secret(HOOK, 8);
    let lane = s.event(&secret, 8, "unit-a", 1).await;
    assert_eq!(
        lane.body["result"], "started",
        "at once, without a cache wait"
    );
    assert_eq!(lane.body["runner"], "jobs");
    let started = s.started();
    assert_eq!(started[0].env["RUNNER_TOKEN"], "glrt-8-jobs");
    assert_eq!(started[0].pool.image, "img/node");
    let ignored = s.event(&secret, 8, "pages", 2).await;
    assert_eq!(ignored.body["reason"], "ignore_jobs");
    let host_pages = s.event(HOOK, 7, "pages", 3).await;
    assert_eq!(
        host_pages.body["runner"], "vm",
        "the member's ignoreJobs are its own"
    );

    // spec 8.3: a rejoin updates a changed lane and deletes a removed one.
    let changed = join_body(
        "glrt-8b",
        &[
            ("rust", "img/rust", &["clippy"]),
            ("jobs", "img/node2", &["build"]),
        ],
    );
    assert_eq!(s.join(8, changed, NOW + 5).await.body["result"], "joined");
    let stored = s.store.inner.members().await.unwrap();
    assert_eq!(stored.len(), 1, "a second join replaces the record");
    assert_eq!(stored[0].lanes.len(), 2);
    assert!(stored[0].ignore_jobs.is_empty());
    let rerun = s.event(&secret, 8, "build", 4).await;
    assert_eq!(rerun.body["runner"], "jobs");
    assert_eq!(s.started().last().unwrap().pool.image, "img/node2");
    assert_eq!(
        s.started().last().unwrap().env["RUNNER_TOKEN"],
        "glrt-8b-jobs"
    );
    let unit = s.event(&secret, 8, "unit-a", 5).await;
    assert_eq!(unit.body["runner"], "vm", "no lane takes it any more");
    let only_vm = join_body("glrt-8c", &[]);
    assert_eq!(s.join(8, only_vm, NOW + 6).await.body["result"], "joined");
    let defs = s.provider.inner.definition_keys().await;
    assert_eq!(defs, ["7-jobs", "vm"], "the member's definitions are gone");
    assert_eq!(s.event(&secret, 8, "build", 6).await.body["runner"], "vm");

    assert_eq!(
        s.join(
            8,
            join_body("glrt-8", &[("jobs", "img/node", &["build"])]),
            NOW + 7
        )
        .await
        .body["result"],
        "joined"
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
    assert_eq!(s.provider.inner.definition_keys().await, ["7-jobs", "vm"]);
    assert_eq!(s.event(&secret, 8, "test", 7).await.status, 401);
    assert!(s.lines().iter().any(|l| l.starts_with("info left")));
}

/// D60: two projects with the same lane tag and patterns route only to their own
/// definitions; a job of one project that only the other's lane matches runs on the VM
/// runner.
#[tokio::test(start_paused = true)]
async fn lanes_route_only_the_jobs_of_their_own_project() {
    let s = setup("[7]", true).await;
    let same = join_body("glrt-8", &[("jobs", "img/kaniko", &["build"])]);
    assert_eq!(s.join(8, same, NOW).await.body["result"], "joined");
    let other = join_body("glrt-9", &[("node", "node:24", &["lint"])]);
    assert_eq!(s.join(9, other, NOW).await.body["result"], "joined");

    let host = s.event(HOOK, 7, "build", 1).await;
    assert_eq!(host.body["runner"], "jobs", "{}", host.body);
    let member = s.event(&invite::hook_secret(HOOK, 8), 8, "build", 2).await;
    assert_eq!(member.body["runner"], "jobs", "{}", member.body);
    let started = s.started();
    assert_eq!(
        (
            started[0].pool.tag.as_str(),
            started[0].env["RUNNER_TOKEN"].as_str()
        ),
        ("7-jobs", "glrt-jobs")
    );
    assert_eq!(
        (
            started[1].pool.tag.as_str(),
            started[1].env["RUNNER_TOKEN"].as_str()
        ),
        ("8-jobs", "glrt-8-jobs")
    );
    for (token, project, job) in [
        (HOOK.to_string(), 7, 3),
        (invite::hook_secret(HOOK, 8), 8, 4),
    ] {
        let lint = s.event(&token, project, "lint", job).await;
        assert_eq!(
            lint.body["runner"], "vm",
            "only project 9's lane takes lint"
        );
    }
    let nine = s.event(&invite::hook_secret(HOOK, 9), 9, "build", 5).await;
    assert_eq!(
        nine.body["runner"], "vm",
        "the host's and 8's lanes are not 9's"
    );
    let lint = s.event(&invite::hook_secret(HOOK, 9), 9, "lint", 6).await;
    assert_eq!(lint.body["runner"], "node");
    assert_eq!(
        s.started().last().unwrap().env["RUNNER_TOKEN"],
        "glrt-9-node"
    );
}

/// spec 8.3: a step of `/join` that does not finish within the budget answers 504 naming
/// it, and stores nothing.
#[tokio::test(start_paused = true)]
async fn a_join_that_cannot_finish_in_time_names_the_step() {
    let s = setup("[7]", true).await;
    *s.provider.sync_delay.lock().unwrap() = JOIN_BUDGET + std::time::Duration::from_secs(1);
    let body = join_body("glrt-8", &[("jobs", "img/node", &["build"])]);
    let join = s.join(8, body, NOW).await;
    assert_eq!(join.status, 504, "{}", join.body);
    assert_eq!(join.body["result"], "timeout");
    assert_eq!(join.body["step"], "definitions");
    assert!(s.store.inner.members().await.unwrap().is_empty());
    assert!(
        s.lines()
            .iter()
            .any(|l| l.starts_with("error join_failed") && l.contains(r#"step="definitions""#))
    );
}

#[tokio::test(start_paused = true)]
async fn bad_codes_unknown_runners_bad_lanes_and_the_host_are_refused() {
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
    assert_eq!(unknown.body["result"], "unknown_runners");
    assert_eq!(unknown.body["runners"], json!(["gpu"]));
    let mut bad_lanes = join_body(
        "glrt-8",
        &[("vm", "img/node", &["a"]), ("big", "img/node", &[])],
    );
    bad_lanes["lanes"][1]["vcpu"] = json!(7);
    bad_lanes["laneRunners"]
        .as_object_mut()
        .unwrap()
        .remove("big");
    let invalid = s.join(8, bad_lanes, NOW).await;
    assert_eq!(invalid.status, 400);
    assert_eq!(invalid.body["result"], "invalid_lanes");
    assert_eq!(
        invalid.body["errors"],
        json!([
            "lanes[0].tag: vm is the VM runner's tag",
            "lanes[1].jobs: at least one job name pattern",
            "lanes[1].vcpu: 7 above fake maximum 6",
            "laneRunners.big: lanes[1] has no runner token",
        ])
    );
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
async fn further_config_projects_use_the_host_vm_runner_until_they_join() {
    let s = setup("[7, 11]", true).await;
    let host_lane = s.event(HOOK, 11, "build", 1).await;
    assert_eq!(
        host_lane.body["runner"], "vm",
        "the host's lanes take the host's jobs only"
    );
    assert_eq!(s.started()[0].env["RUNNER_TOKEN"], "glrt-vm");
    s.put(member(11, &[("vm", "glrt-11")])).await;
    tokio::time::advance(FLEET_CACHE).await;
    assert_eq!(s.event(HOOK, 11, "build", 2).await.status, 401);
    let own = s
        .event(&invite::hook_secret(HOOK, 11), 11, "build", 3)
        .await;
    assert_eq!(own.body["result"], "started");
    assert_eq!(own.body["runner"], "vm");
}
