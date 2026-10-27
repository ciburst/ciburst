//! M6.3: the dispatcher across the fleet's members. The routing config and the members come
//! from the fleet store, cached for [`FLEET_CACHE`] (rule R1); events are accepted per member
//! secret (rule R6); `GET /fleet`, `POST /join` and `POST /leave` take a join code (rules R3
//! and R4); VM reports answer the member runners of the VM's pool (rule R5).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{Arrival, DispatchResponse, Dispatcher, header, reply, token_matches, unauthorized};
use crate::config::{Config, INSTANCES_RUNTIME, parse_config};
use crate::fleet::{FleetStore, Member};
use crate::invite::{self, Invite};
use crate::types::VmRunner;

/// M6.3 rule R1: the longest a warm dispatcher keeps the fleet state it read.
pub const FLEET_CACHE: Duration = Duration::from_secs(30);

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

/// Whose runner takes a project's jobs of a pool.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RunnerOf {
    /// The host's runner: the host, and projects that use the host's runners (rule R9).
    Host,
    /// The runner the member registered, with its token.
    Member(String),
    /// The member registered no runner for the pool.
    Missing,
}

#[derive(Deserialize)]
struct JoinBody {
    runners: BTreeMap<String, String>,
}

impl Dispatcher {
    /// The fleet store (M6.3 rule R1). Without it, or while it holds no config, the dispatcher
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
        config.gitlab.project_ids.first().copied().unwrap_or(0)
    }

    /// Every member: the stored ones; the host with the hook token and runners of the
    /// environment; and each further project of `gitlab.projectIds` without a record, with the
    /// host's secret and runners until it joins (M6.3 rule R9).
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
            },
        );
        for &project_id in config.gitlab.project_ids.iter().skip(1) {
            all.entry(project_id).or_insert_with(|| Member {
                project_id,
                hook_secret: hook.clone(),
                runners: self.secrets.runner_tokens.clone(),
                joined_at: 0,
            });
        }
        all.into_values().collect()
    }

    pub(super) fn member(&self, project_id: u64) -> Option<Member> {
        self.members()
            .into_iter()
            .find(|member| member.project_id == project_id)
    }

    /// M6.3 rule R6: an event of a member carries that member's secret; any other event the
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

    /// M6.3 rule R5: the runner of `pool` of every member that registered its own, one per
    /// token. The host's runner is the VM's `RUNNER_TOKEN` and not among them.
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

    /// `GET /fleet`: the fleet's name, its pools with the tags of their runners, and the
    /// webhook secret of the code's project.
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
        let pools: Vec<Value> = config
            .pools
            .iter()
            .map(|pool| {
                json!({
                    "tag": pool.tag,
                    "runtime": pool.runtime.as_deref().unwrap_or(INSTANCES_RUNTIME),
                    "tags": config.runner_tags(pool),
                })
            })
            .collect();
        let hook = self.secrets.hook_token.as_deref().unwrap_or("");
        reply(
            200,
            "fleet",
            json!({
                "name": config.name,
                "pools": pools,
                "hookSecret": invite::hook_secret(hook, invite.project),
            }),
        )
    }

    /// `POST /join` (M6.3 rule R4): stores the member of the code's project with its runners,
    /// one per known pool, and the webhook secret derived from the hook token. A second join
    /// replaces the record.
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
        let Some(runners) = serde_json::from_slice::<JoinBody>(body)
            .ok()
            .map(|body| body.runners)
            .filter(|runners| !runners.is_empty() && runners.values().all(|t| !t.is_empty()))
        else {
            return reply(400, "bad_request", json!({}));
        };
        let config = self.config();
        let unknown: Vec<&String> = runners
            .keys()
            .filter(|tag| config.pool_by_tag(tag).is_none())
            .collect();
        if !unknown.is_empty() {
            return reply(400, "unknown_pools", json!({ "pools": unknown }));
        }
        let Some(store) = &self.fleet else {
            return reply(503, "no_fleet", json!({}));
        };
        let hook = self.secrets.hook_token.as_deref().unwrap_or("");
        let member = Member {
            project_id: invite.project,
            hook_secret: invite::hook_secret(hook, invite.project),
            runners,
            joined_at: now,
        };
        let project = invite.project.to_string();
        if let Err(error) = store.put_member(&member).await {
            let message = error.to_string();
            self.error(
                at,
                "join_failed",
                &[("project", &project), ("error", &message)],
            );
            return reply(500, "error", json!({}));
        }
        let pools = member.runners.keys().cloned().collect::<Vec<_>>().join(",");
        self.fleet_cache.update(|members| {
            members.retain(|m| m.project_id != member.project_id);
            members.push(member);
        });
        self.info(at, "joined", &[("project", &project), ("pools", &pools)]);
        reply(200, "joined", json!({ "fleet": config.name }))
    }

    /// `POST /leave` (M6.3 rule R4): removes the member of the code's project. An expired code
    /// still leaves; the host cannot.
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
        if let Err(error) = store.delete_member(invite.project).await {
            let message = error.to_string();
            self.error(
                at,
                "leave_failed",
                &[("project", &project), ("error", &message)],
            );
            return reply(500, "error", json!({}));
        }
        self.fleet_cache
            .update(|members| members.retain(|m| m.project_id != invite.project));
        self.info(at, "left", &[("project", &project)]);
        reply(200, "left", json!({ "fleet": self.config().name }))
    }
}

#[cfg(test)]
mod tests;
