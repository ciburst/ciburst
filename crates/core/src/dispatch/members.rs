//! The dispatcher across the fleet's members (spec 8). The routing config and the members come
//! from the fleet store, cached for [`FLEET_CACHE`] (D58); events are accepted per member
//! secret (spec 8.5); `GET /fleet`, `POST /join` and `POST /leave` take a join code (spec
//! 8.3); VM reports answer the member runners of the VM runner (spec 8.5). Each project
//! routes its jobs by its own lanes and `ignoreJobs` (D60, D61), and `/join` makes a member's
//! lane definitions (spec 8.3).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;

use super::{Arrival, DispatchResponse, Dispatcher, header, reply, token_matches, unauthorized};
use crate::config::{
    Config, Lane, Pool, ProjectConfig, Route, check_lane_capabilities, parse_config,
    validate_project,
};
use crate::fleet::{FleetStore, Member, MemberLane};
use crate::invite::{self, Invite};
use crate::types::VmRunner;

/// D58: the longest a warm dispatcher keeps the fleet state it read.
pub const FLEET_CACHE: Duration = Duration::from_secs(30);

/// spec 8.3: `/join` and `/leave` finish their steps this long after the request arrived,
/// inside the function's 30 s, or answer with the step that did not finish.
pub const JOIN_BUDGET: Duration = Duration::from_secs(25);

/// Path of the fleet description a joining member reads.
pub const PATH_FLEET: &str = "/fleet";
pub const PATH_JOIN: &str = "/join";
pub const PATH_LEAVE: &str = "/leave";

/// The fleet state a warm dispatcher read last. The function keeps one per instance and hands
/// it to the dispatcher of every request.
#[derive(Default)]
pub struct FleetCache {
    state: Mutex<Cached>,
}

#[derive(Default, Clone)]
struct Cached {
    /// The stored routing config; `None` before a setup wrote one.
    config: Option<Arc<Config>>,
    members: Vec<Member>,
    /// When both reads last succeeded, on the dispatcher's clock.
    read: Option<Instant>,
}

impl FleetCache {
    pub fn new() -> FleetCache {
        FleetCache::default()
    }

    fn get(&self) -> Cached {
        self.state.lock().expect("fleet cache").clone()
    }

    fn set(&self, cached: Cached) {
        *self.state.lock().expect("fleet cache") = cached;
    }

    fn update(&self, change: impl FnOnce(&mut Vec<Member>)) {
        change(&mut self.state.lock().expect("fleet cache").members);
    }
}

/// Whose runner takes a project's VM jobs.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RunnerOf {
    /// The host's runner: the host, and projects that use the host's runners (spec 8.6).
    Host,
    /// The runner the member registered, with its token.
    Member(String),
    /// The member registered no VM runner.
    Missing,
}

/// Where a job of a project goes (D60 to D63).
#[derive(Debug)]
pub(super) enum Routed {
    /// The project's `ignoreJobs` matches: nothing starts.
    Ignored,
    /// The VM runner.
    Vm,
    /// One of the project's own lanes.
    Lane(Pool),
}

/// The body of `POST /join` (spec 8.3): the member's VM runner token by the VM runner's
/// tag, its lanes, each lane's runner token by lane tag, and its `ignoreJobs`.
#[derive(Deserialize)]
struct JoinBody {
    runners: BTreeMap<String, String>,
    #[serde(default)]
    lanes: Vec<Lane>,
    #[serde(default, rename = "laneRunners")]
    lane_runners: BTreeMap<String, String>,
    #[serde(default, rename = "ignoreJobs")]
    ignore_jobs: Vec<String>,
}

/// A step of `/join` or `/leave` that failed or did not finish within [`JOIN_BUDGET`].
struct StepFailed {
    step: &'static str,
    timeout: bool,
    message: String,
}

impl Dispatcher {
    /// The fleet store (D58). Without it, or while it holds no config, the dispatcher
    /// routes by the config of its environment and the host's hook token.
    pub fn with_fleet(mut self, fleet: Option<Arc<dyn FleetStore>>) -> Dispatcher {
        self.fleet = fleet;
        self
    }

    /// The cache of the fleet state, shared by the dispatchers of one warm instance.
    pub fn with_fleet_cache(mut self, cache: Arc<FleetCache>) -> Dispatcher {
        self.fleet_cache = cache;
        self
    }

    /// The routing config: the fleet's, else the environment's.
    pub fn config(&self) -> Arc<Config> {
        self.fleet_cache
            .get()
            .config
            .filter(|_| self.fleet.is_some())
            .unwrap_or_else(|| self.base.clone())
    }

    /// Reads the fleet state unless the cache is younger than [`FLEET_CACHE`]. A failed read
    /// logs `fleet_failed` and keeps the last value, or the environment's config.
    pub(super) async fn refresh_fleet(&self, at: Arrival) {
        let Some(store) = &self.fleet else {
            return;
        };
        let mut cached = self.fleet_cache.get();
        if cached.read.is_some_and(|read| read.elapsed() < FLEET_CACHE) {
            return;
        }
        let mut ok = true;
        let (config, members) = tokio::join!(store.config(), store.members());
        let config = config.and_then(|json| {
            json.map(|json| parse_config(&json).map(Arc::new))
                .transpose()
        });
        match config {
            Ok(config) => cached.config = config,
            Err(error) => {
                ok = false;
                self.fleet_failed(at, "config", &error);
            }
        }
        match members {
            Ok(members) => cached.members = members,
            Err(error) => {
                ok = false;
                self.fleet_failed(at, "members", &error);
            }
        }
        if ok {
            cached.read = Some(Instant::now());
        }
        self.fleet_cache.set(cached);
    }

    fn fleet_failed(&self, at: Arrival, op: &str, error: &crate::Error) {
        let message = error.to_string();
        self.warn(at, "fleet_failed", &[("op", op), ("error", &message)]);
    }

    fn host_project(&self, config: &Config) -> u64 {
        config.host_project()
    }

    /// The route of job `name` of `member`: the host's by the config, every other project's
    /// by its own record. Patterns of another project never apply (D60).
    pub(super) fn route_of(&self, member: &Member, name: &str) -> Routed {
        let config = self.config();
        let project = member.project_id;
        if project == self.host_project(&config) {
            return match config.route(name) {
                Route::Ignored => Routed::Ignored,
                Route::Vm => Routed::Vm,
                Route::Lane(lane) => Routed::Lane(lane.pool(project)),
            };
        }
        match member.route(name) {
            Route::Ignored => Routed::Ignored,
            Route::Vm => Routed::Vm,
            Route::Lane(lane) => Routed::Lane(lane.pool(project)),
        }
    }

    /// Every member: the stored ones; the host with the hook token and runners of the
    /// environment; and each further project of `gitlab.projectIds` without a record, with the
    /// host's secret and runners until it joins (spec 8.6).
    pub(super) fn members(&self) -> Vec<Member> {
        let config = self.config();
        let host = self.host_project(&config);
        let hook = self.secrets.hook_token.clone().unwrap_or_default();
        let mut all: BTreeMap<u64, Member> = self
            .fleet_cache
            .get()
            .members
            .into_iter()
            .filter(|_| self.fleet.is_some())
            .map(|member| (member.project_id, member))
            .collect();
        let joined_at = all.get(&host).map_or(0, |member| member.joined_at);
        all.insert(
            host,
            Member {
                project_id: host,
                hook_secret: hook.clone(),
                runners: self.secrets.runner_tokens.clone(),
                joined_at,
                ..Default::default()
            },
        );
        for &project_id in config.gitlab.project_ids.iter().skip(1) {
            all.entry(project_id).or_insert_with(|| Member {
                project_id,
                hook_secret: hook.clone(),
                runners: self.secrets.runner_tokens.clone(),
                joined_at: 0,
                ..Default::default()
            });
        }
        all.into_values().collect()
    }

    pub(super) fn member(&self, project_id: u64) -> Option<Member> {
        self.members()
            .into_iter()
            .find(|member| member.project_id == project_id)
    }

    /// spec 8.5: an event of a member carries that member's secret; any other event the
    /// host's hook token, and answers as today.
    pub(super) fn authorized(&self, token: Option<&str>, project_id: Option<u64>) -> bool {
        match project_id.and_then(|id| self.member(id)) {
            Some(member) => token_matches(token, Some(&member.hook_secret)),
            None => token_matches(token, self.secrets.hook_token.as_deref()),
        }
    }

    pub(super) fn runner_of(&self, project_id: u64, pool: &str) -> RunnerOf {
        if project_id == self.host_project(&self.config()) {
            return RunnerOf::Host;
        }
        let host = self.secrets.runner_tokens.get(pool);
        match self.member(project_id).map(|member| member.runners) {
            None => RunnerOf::Host,
            Some(runners) => match runners.get(pool) {
                Some(token) if Some(token) == host => RunnerOf::Host,
                Some(token) => RunnerOf::Member(token.clone()),
                None => RunnerOf::Missing,
            },
        }
    }

    /// spec 8.5: the VM runner of every member that registered its own, one per token.
    /// The host's runner is the VM's `RUNNER_TOKEN` and not among them.
    pub(super) fn member_runners(&self, pool: &str) -> Vec<VmRunner> {
        let host = self.secrets.runner_tokens.get(pool);
        let mut runners: Vec<VmRunner> = Vec::new();
        for member in self.members() {
            if let RunnerOf::Member(token) = self.runner_of(member.project_id, pool)
                && Some(&token) != host
                && !runners.iter().any(|r| r.token == token)
            {
                runners.push(VmRunner {
                    project_id: member.project_id,
                    token,
                });
            }
        }
        runners
    }

    /// The invite of the request's join code: signed with the hook token and, unless
    /// `expired_ok`, valid at `now`. A refusal logs `join_refused` with the reason.
    fn invite_of(
        &self,
        route: &str,
        headers: &[(String, String)],
        now: u64,
        expired_ok: bool,
        at: Arrival,
    ) -> Option<Invite> {
        let hook = self.secrets.hook_token.as_deref().unwrap_or("");
        let code = header(headers, invite::HEADER).unwrap_or("");
        let verified = if expired_ok {
            invite::authentic(hook, code)
        } else {
            invite::verify(hook, code, now)
        };
        match verified {
            Ok(invite) => Some(invite),
            Err(error) => {
                let reason = format!("{error:?}").to_lowercase();
                self.warn(at, "join_refused", &[("route", route), ("reason", &reason)]);
                None
            }
        }
    }

    fn is_host(&self, project_id: u64) -> bool {
        project_id == self.host_project(&self.config())
    }

    fn host_refused(&self, route: &str, project: u64, at: Arrival) -> DispatchResponse {
        let project = project.to_string();
        self.warn(
            at,
            "join_refused",
            &[("route", route), ("reason", "host"), ("project", &project)],
        );
        reply(400, "refused", json!({ "reason": "host" }))
    }

    /// `GET /fleet`: the fleet's name, its VM runner, and the webhook secret of the code's
    /// project. `pools` lists the VM runner alone, for `ciburst-join` before M6.4.
    pub(super) async fn fleet_info(
        &self,
        headers: &[(String, String)],
        now: u64,
        at: Arrival,
    ) -> DispatchResponse {
        let Some(invite) = self.invite_of(PATH_FLEET, headers, now, false, at) else {
            return unauthorized();
        };
        if self.is_host(invite.project) {
            return self.host_refused(PATH_FLEET, invite.project, at);
        }
        let config = self.config();
        let vm = json!({ "tag": config.vm.tag, "runtime": config.vm.runtime() });
        let hook = self.secrets.hook_token.as_deref().unwrap_or("");
        reply(
            200,
            "fleet",
            json!({
                "name": config.name,
                "vm": vm,
                "hookSecret": invite::hook_secret(hook, invite.project),
            }),
        )
    }

    /// The problems of a join body for project `project`, each naming its field.
    fn join_problems(&self, body: &JoinBody, project: u64) -> Vec<String> {
        let config = self.config();
        let settings = ProjectConfig {
            lanes: body.lanes.clone(),
            ignore_jobs: body.ignore_jobs.clone(),
        };
        let mut problems = match validate_project(&settings, &config.name, &config.vm.tag, project)
        {
            Ok(()) => Vec::new(),
            Err(crate::Error::Config(errors)) => errors,
            Err(error) => vec![error.to_string()],
        };
        problems.extend(check_lane_capabilities(&body.lanes, project, |pool| {
            self.provider.capabilities_for(pool)
        }));
        for (index, lane) in body.lanes.iter().enumerate() {
            if body
                .lane_runners
                .get(&lane.tag)
                .is_none_or(String::is_empty)
            {
                problems.push(format!(
                    "laneRunners.{}: lanes[{index}] has no runner token",
                    lane.tag
                ));
            }
        }
        for tag in body.lane_runners.keys() {
            if !body.lanes.iter().any(|lane| lane.tag == *tag) {
                problems.push(format!("laneRunners.{tag}: no such lane"));
            }
        }
        problems
    }

    /// Runs one step of `/join` or `/leave` until [`JOIN_BUDGET`] after the arrival.
    async fn step<T>(
        &self,
        step: &'static str,
        at: Arrival,
        work: impl std::future::Future<Output = crate::Result<T>>,
    ) -> std::result::Result<T, StepFailed> {
        match tokio::time::timeout_at(at.0 + JOIN_BUDGET, work).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(StepFailed {
                step,
                timeout: false,
                message: error.to_string(),
            }),
            Err(_) => Err(StepFailed {
                step,
                timeout: true,
                message: format!("not finished within {} s", JOIN_BUDGET.as_secs()),
            }),
        }
    }

    /// A failed step logs `<route>_failed` with the step and answers 504 `timeout` or 500
    /// `error`, both naming the step.
    fn step_failed(
        &self,
        route: &str,
        project: &str,
        failed: StepFailed,
        at: Arrival,
    ) -> DispatchResponse {
        self.error(
            at,
            &format!("{route}_failed"),
            &[
                ("project", project),
                ("step", failed.step),
                ("error", &failed.message),
            ],
        );
        if failed.timeout {
            reply(504, "timeout", json!({ "step": failed.step }))
        } else {
            reply(500, "error", json!({ "step": failed.step }))
        }
    }

    /// `POST /join` (spec 8.3, 8.4): makes the job definitions of the member's
    /// lanes with the dispatch key, deletes its definitions no longer listed, and stores the
    /// member of the code's project with its VM runner, its lanes and `ignoreJobs`, and the
    /// webhook secret derived from the hook token. A second join replaces the record.
    pub(super) async fn join(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        now: u64,
        at: Arrival,
    ) -> DispatchResponse {
        let Some(invite) = self.invite_of(PATH_JOIN, headers, now, false, at) else {
            return unauthorized();
        };
        if self.is_host(invite.project) {
            return self.host_refused(PATH_JOIN, invite.project, at);
        }
        let Some(body) = serde_json::from_slice::<JoinBody>(body)
            .ok()
            .filter(|body| {
                !body.runners.is_empty() && body.runners.values().all(|t| !t.is_empty())
            })
        else {
            return reply(400, "bad_request", json!({}));
        };
        let config = self.config();
        let unknown: Vec<&String> = body
            .runners
            .keys()
            .filter(|tag| **tag != config.vm.tag)
            .collect();
        if !unknown.is_empty() {
            return reply(400, "unknown_runners", json!({ "runners": unknown }));
        }
        let problems = self.join_problems(&body, invite.project);
        if !problems.is_empty() {
            return reply(400, "invalid_lanes", json!({ "errors": problems }));
        }
        let Some(store) = &self.fleet else {
            return reply(503, "no_fleet", json!({}));
        };
        let project = invite.project.to_string();
        let pools: Vec<Pool> = body
            .lanes
            .iter()
            .map(|lane| lane.pool(invite.project))
            .collect();
        let ids = match self
            .step(
                "definitions",
                at,
                self.provider.sync_lanes(invite.project, &pools),
            )
            .await
        {
            Ok(ids) => ids,
            Err(failed) => return self.step_failed("join", &project, failed, at),
        };
        let lanes: Vec<MemberLane> = body
            .lanes
            .iter()
            .map(|lane| MemberLane {
                tag: lane.tag.clone(),
                image: lane.image.clone(),
                jobs: lane.jobs.clone(),
                max_runs: lane.max_runs,
                definition_id: ids.get(&lane.tag).cloned().unwrap_or_default(),
                runner_token: body.lane_runners[&lane.tag].clone(),
            })
            .collect();
        let hook = self.secrets.hook_token.as_deref().unwrap_or("");
        let member = Member {
            project_id: invite.project,
            hook_secret: invite::hook_secret(hook, invite.project),
            runners: body.runners,
            lanes,
            ignore_jobs: body.ignore_jobs,
            joined_at: now,
        };
        if let Err(failed) = self.step("record", at, store.put_member(&member)).await {
            return self.step_failed("join", &project, failed, at);
        }
        let tags: Vec<String> = member.lanes.iter().map(|l| l.tag.clone()).collect();
        let lanes_field = tags.join(",");
        self.fleet_cache.update(|members| {
            members.retain(|m| m.project_id != member.project_id);
            members.push(member);
        });
        self.info(
            at,
            "joined",
            &[("project", &project), ("lanes", &lanes_field)],
        );
        reply(
            200,
            "joined",
            json!({ "fleet": config.name, "lanes": tags }),
        )
    }

    /// `POST /leave` (spec 8.3, 8.4): deletes the job definitions of the
    /// member's lanes and the member of the code's project. An expired code still leaves; the
    /// host cannot.
    pub(super) async fn leave(
        &self,
        headers: &[(String, String)],
        now: u64,
        at: Arrival,
    ) -> DispatchResponse {
        let Some(invite) = self.invite_of(PATH_LEAVE, headers, now, true, at) else {
            return unauthorized();
        };
        if self.is_host(invite.project) {
            return self.host_refused(PATH_LEAVE, invite.project, at);
        }
        let Some(store) = &self.fleet else {
            return reply(503, "no_fleet", json!({}));
        };
        let project = invite.project.to_string();
        if let Err(failed) = self
            .step(
                "definitions",
                at,
                self.provider.sync_lanes(invite.project, &[]),
            )
            .await
        {
            return self.step_failed("leave", &project, failed, at);
        }
        if let Err(failed) = self
            .step("record", at, store.delete_member(invite.project))
            .await
        {
            return self.step_failed("leave", &project, failed, at);
        }
        self.fleet_cache
            .update(|members| members.retain(|m| m.project_id != invite.project));
        self.info(at, "left", &[("project", &project)]);
        reply(200, "left", json!({ "fleet": self.config().name }))
    }
}

#[cfg(test)]
mod tests;
