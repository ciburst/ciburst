//! The `instances` runtime, spec section 6.1: the security group `<name>-vm` (step
//! `vm:security-group`), the quota check, starting a server with its user data, and the
//! capacity, claim, report, stop and sweep of servers.

use std::collections::{BTreeMap, BTreeSet};

use ciburst_core::config::Pool;
use ciburst_core::provision::keys;
use ciburst_core::types::{Run, StartRequest, VmCapacity, VmReport};
use ciburst_core::{Error, Result};
use serde_json::{Value, json};

use super::cloud_init::Download;
use super::instance::{self, ApiError, DOCKER_IMAGE_LABEL, FALLBACK_IMAGE_LABEL, InstanceApi};
use super::runner::runner_url;
use super::vm;
use super::{PROVIDER_NAME, Runtime, Scaleway, USER_DATA_KEY, cloud_init, iam, instance_types};

pub(super) const SECURITY_GROUP_STEP: &str = "vm:security-group";

/// What a start attempt on one commercial type came to.
enum Attempt {
    Started,
    Unavailable(ApiError),
}

impl Scaleway {
    pub(super) fn security_group_name(&self) -> String {
        format!("{}-vm", self.name())
    }

    /// The VM pools none of whose instance types has quota in the zone, as tag and reason: the
    /// dispatcher could start no VM for them. Quotas are per organization, zone and type; a
    /// failed lookup only logs and ends the check.
    pub(super) async fn quota_problems(
        &self,
        runtimes: &[(&Pool, Runtime)],
    ) -> Vec<(String, String)> {
        let mut limits: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();
        let mut problems = Vec::new();
        for (pool, runtime) in runtimes {
            if *runtime != Runtime::Instances {
                continue;
            }
            let types = instance_types(pool);
            let mut startable = false;
            let mut elsewhere = BTreeSet::new();
            for commercial_type in &types {
                if !limits.contains_key(commercial_type) {
                    let name = iam::server_quota_name(commercial_type);
                    match iam::zone_quota(&self.client, &name).await {
                        Ok(found) => {
                            limits.insert(commercial_type.clone(), found);
                        }
                        Err(error) => {
                            self.log.warn(
                                "quota check skipped",
                                &[("quota", &name), ("error", &error.to_string())],
                            );
                            return problems;
                        }
                    }
                }
                let zones = &limits[commercial_type];
                startable |=
                    zones.is_empty() || zones.iter().any(|(zone, n)| *zone == self.zone && *n > 0);
                elsewhere.extend(
                    zones
                        .iter()
                        .filter(|(_, n)| *n > 0)
                        .map(|(zone, _)| zone.clone()),
                );
            }
            if !startable {
                let zones: Vec<String> = elsewhere.into_iter().collect();
                let zones = if zones.is_empty() {
                    "no zone".to_string()
                } else {
                    zones.join(", ")
                };
                let reason = format!(
                    "zone {} has no quota for {}: set provider.zone (quota in {zones}) or ask \
                     Scaleway support to raise it",
                    self.zone,
                    types.join(", "),
                );
                problems.push((pool.tag.clone(), reason));
            }
        }
        problems
    }

    /// Warns for each pool of [`Scaleway::quota_problems`].
    pub(super) async fn warn_without_quota(&self, runtimes: &[(&Pool, Runtime)]) {
        for (tag, reason) in self.quota_problems(runtimes).await {
            self.log.warn(
                "vm pools cannot start",
                &[("pool", &tag), ("reason", &reason)],
            );
        }
    }

    pub(super) async fn ensure_security_group(&self, project_id: &str) -> Result<()> {
        let name = self.security_group_name();
        let api = self.instances();
        if api.find_security_group(project_id, &name).await?.is_none() {
            let id = api.create_security_group(project_id, &name).await?;
            self.log
                .info("created security group", &[("name", &name), ("id", &id)]);
        }
        Ok(())
    }

    pub(super) async fn delete_security_group(&self, project_id: &str) -> Result<()> {
        let name = self.security_group_name();
        let api = self.instances();
        if let Some(id) = api.find_security_group(project_id, &name).await? {
            api.delete_security_group(&id).await?;
            self.log
                .info("deleted security group", &[("name", &name), ("id", &id)]);
        }
        Ok(())
    }

    /// Rule 4 of M3: the first commercial type that has a local image and capacity runs the
    /// VM; the server id is the run id.
    pub(super) async fn start_vm(&self, request: StartRequest) -> Result<String> {
        let pool = &request.pool;
        let tag = &pool.tag;
        let taken = request.taken.max(1);
        let secret = request
            .env
            .get(keys::VM_SECRET)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::provider(format!(
                    "{PROVIDER_NAME}: pool {tag}: {} missing",
                    keys::VM_SECRET
                ))
            })?;
        let agent = self.start_agent()?.ok_or_else(|| {
            Error::provider(format!(
                "{PROVIDER_NAME}: pool {tag}: {} missing",
                cloud_init::AGENT_SHA256_KEY
            ))
        })?;
        let not_provisioned =
            || Error::provider(format!("{PROVIDER_NAME}: pool {tag} is not provisioned"));
        let project_id = self
            .client
            .find_project(self.name())
            .await?
            .ok_or_else(not_provisioned)?;
        let agent_url =
            cloud_init::agent_url(&self.config, self.client.region(), &project_id, &agent);
        let runner_url = runner_url(
            &self.config,
            self.client.region(),
            &project_id,
            self.runner_sha256(),
        );
        let downloads = (
            Download {
                url: &runner_url,
                sha256: self.runner_sha256(),
            },
            Download {
                url: &agent_url,
                sha256: &agent,
            },
        );
        let api = self.instances();
        let security_group = api
            .find_security_group(&project_id, &self.security_group_name())
            .await?
            .ok_or_else(not_provisioned)?;
        let base = cloud_init::base_env(
            &self.config,
            pool,
            self.client.region(),
            &project_id,
            &request.env,
        )?;
        let types = instance_types(pool);
        let job = request.labels.get("job").map(String::as_str);
        let docker_images = api.local_images(DOCKER_IMAGE_LABEL).await?;
        let mut fallback_images = None;
        let mut failures = Vec::new();
        for commercial_type in &types {
            let (image, install_docker) =
                match instance::compatible(&docker_images, commercial_type) {
                    Some(image) => (image, false),
                    None => {
                        if fallback_images.is_none() {
                            fallback_images = Some(api.local_images(FALLBACK_IMAGE_LABEL).await?);
                        }
                        let fallback = fallback_images.as_deref().unwrap_or_default();
                        match instance::compatible(fallback, commercial_type) {
                            Some(image) => (image, true),
                            None => {
                                self.log.warn(
                                    "instance type skipped",
                                    &[
                                        ("pool", tag),
                                        ("type", commercial_type),
                                        ("reason", "no local image"),
                                    ],
                                );
                                failures.push(format!("{commercial_type}: no local image"));
                                continue;
                            }
                        }
                    }
                };
            let name = vm::server_name(self.name(), tag, job);
            let body = json!({
                "name": name,
                "commercial_type": commercial_type,
                "image": image,
                "project": project_id,
                "dynamic_ip_required": true,
                "routed_ip_enabled": true,
                "security_group": security_group,
                "tags": vm::initial_tags(self.name(), tag, job, pool.slots.saturating_sub(taken)),
            });
            let server = match api.create_server(&body).await? {
                Ok(server) => server,
                Err(error) if error.is_capacity() => {
                    self.unavailable(tag, commercial_type, &error);
                    failures.push(format!("{commercial_type}: {}", error.message));
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let id = server["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(String::from)
                .ok_or_else(|| Error::provider("scaleway server: response without id"))?;
            match self
                .boot(&api, &id, secret, &base, downloads, install_docker)
                .await
            {
                Ok(Attempt::Started) => {
                    self.log.info(
                        "started server",
                        &[
                            ("pool", tag),
                            ("name", &name),
                            ("id", &id),
                            ("type", commercial_type),
                        ],
                    );
                    return Ok(id);
                }
                Ok(Attempt::Unavailable(error)) => {
                    self.unavailable(tag, commercial_type, &error);
                    failures.push(format!("{commercial_type}: {}", error.message));
                    self.discard(&api, &id).await;
                }
                Err(error) => {
                    self.discard(&api, &id).await;
                    return Err(error);
                }
            }
        }
        Err(Error::provider(format!(
            "{PROVIDER_NAME}: pool {tag}: no instance type started ({})",
            failures.join("; ")
        )))
    }

    /// User data, then power on. The VM gets its token, never the secret behind it, and the
    /// URLs and SHA-256 of runner and agent.
    async fn boot(
        &self,
        api: &InstanceApi<'_>,
        id: &str,
        secret: &str,
        base: &BTreeMap<String, String>,
        (runner, agent): (Download<'_>, Download<'_>),
        install_docker: bool,
    ) -> Result<Attempt> {
        let mut env = base.clone();
        env.insert(cloud_init::RUN_ID.to_string(), id.to_string());
        env.insert(
            cloud_init::VM_TOKEN.to_string(),
            ciburst_core::vm::token(secret, id),
        );
        let user_data = cloud_init::render(&env, runner, agent, install_docker)?;
        api.set_user_data(id, USER_DATA_KEY, &user_data).await?;
        match api.action(id, "poweron").await? {
            Ok(()) => Ok(Attempt::Started),
            Err(error) if error.is_capacity() => Ok(Attempt::Unavailable(error)),
            Err(error) => Err(error.into()),
        }
    }

    fn unavailable(&self, pool: &str, commercial_type: &str, error: &ApiError) {
        self.log.warn(
            "instance type unavailable",
            &[
                ("pool", pool),
                ("type", commercial_type),
                ("error", &error.message),
            ],
        );
    }

    /// Removes a server that failed to start; the original error matters more than this one.
    async fn discard(&self, api: &InstanceApi<'_>, id: &str) {
        let removed = match api.get_server(id).await {
            Ok(Some(server)) => self.remove_server(api, &server).await.map(drop),
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        };
        if let Err(error) = removed {
            self.log.warn(
                "server not removed",
                &[("id", id), ("error", &error.to_string())],
            );
        }
    }

    /// Ends a server: `terminate` deletes its local volumes and releases its dynamic IP; a
    /// server that never powered on cannot terminate, so it and its local volumes are deleted.
    /// `false` when it is already stopping.
    pub(super) async fn remove_server(
        &self,
        api: &InstanceApi<'_>,
        server: &Value,
    ) -> Result<bool> {
        let id = server["id"].as_str().unwrap_or_default();
        match server["state"].as_str().unwrap_or_default() {
            "stopping" => Ok(false),
            "stopped" => {
                api.delete_server(id).await?;
                for volume in server["volumes"]
                    .as_object()
                    .into_iter()
                    .flat_map(|v| v.values())
                {
                    if volume["volume_type"] == "l_ssd"
                        && let Some(volume_id) = volume["id"].as_str()
                    {
                        api.delete_volume(volume_id).await?;
                    }
                }
                Ok(true)
            }
            _ => {
                api.action(id, "terminate").await??;
                Ok(true)
            }
        }
    }

    /// Servers of this deployment in every state.
    pub(super) async fn servers(&self, project_id: &str) -> Result<Vec<Value>> {
        Ok(self
            .instances()
            .list_servers(project_id, self.name())
            .await?
            .into_iter()
            .filter(|s| vm::is_deployment(s, self.name()))
            .collect())
    }

    fn foreign(&self, run_id: &str) -> Error {
        Error::provider(format!(
            "{PROVIDER_NAME}: server {run_id} does not belong to {}",
            self.name()
        ))
    }

    /// The server `run_id` of this deployment.
    async fn own_server(&self, api: &InstanceApi<'_>, run_id: &str) -> Result<Value> {
        match api.get_server(run_id).await? {
            Some(server) if vm::is_deployment(&server, self.name()) => Ok(server),
            Some(_) => Err(self.foreign(run_id)),
            None => Err(Error::provider(format!(
                "{PROVIDER_NAME}: server {run_id} not found"
            ))),
        }
    }

    /// Ends the server `run_id`; `false` when there is no such server.
    pub(super) async fn stop_server(&self, run_id: &str) -> Result<bool> {
        let api = self.instances();
        let Some(server) = api.get_server(run_id).await? else {
            return Ok(false);
        };
        if !vm::is_deployment(&server, self.name()) {
            return Err(self.foreign(run_id));
        }
        if self.remove_server(&api, &server).await? {
            self.log.info("terminated server", &[("id", run_id)]);
        }
        Ok(true)
    }

    /// Servers as runs, of one pool or of the whole deployment.
    pub(super) async fn vm_runs(&self, project_id: &str, pool: Option<&str>) -> Result<Vec<Run>> {
        Ok(self
            .servers(project_id)
            .await?
            .iter()
            .filter_map(|server| {
                let tags = vm::tags_of(server);
                let tag = vm::tag_value(&tags, vm::TAG_POOL).unwrap_or_default();
                if pool.is_some_and(|p| p != tag) {
                    return None;
                }
                Some(Run {
                    run_id: server["id"].as_str()?.to_string(),
                    pool: tag.to_string(),
                    state: vm::run_state(server["state"].as_str().unwrap_or_default()),
                    created_at: server["creation_date"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    labels: vm::tag_value(&tags, vm::TAG_JOB)
                        .map(|job| BTreeMap::from([("job".to_string(), job.to_string())]))
                        .unwrap_or_default(),
                })
            })
            .collect())
    }

    /// The live servers of the VM pool `pool`.
    pub(super) async fn vm_capacity(&self, pool: &str) -> Result<Vec<VmCapacity>> {
        let Some(project_id) = self.client.find_project(self.name()).await? else {
            return Ok(Vec::new());
        };
        Ok(self
            .servers(&project_id)
            .await?
            .iter()
            .filter(|s| vm::tag_value(&vm::tags_of(s), vm::TAG_POOL) == Some(pool))
            .filter(|s| matches!(s["state"].as_str(), Some("starting" | "running")))
            .filter_map(vm::capacity)
            .collect())
    }

    pub(super) async fn claim_slot(&self, run_id: &str) -> Result<()> {
        let api = self.instances();
        let server = self.own_server(&api, run_id).await?;
        api.set_tags(run_id, &vm::claimed(&vm::tags_of(&server)))
            .await
    }

    pub(super) async fn store_report(
        &self,
        run_id: &str,
        report: &VmReport,
        now: u64,
    ) -> Result<()> {
        let api = self.instances();
        let server = self.own_server(&api, run_id).await?;
        api.set_tags(run_id, &vm::reported(&vm::tags_of(&server), report, now))
            .await
    }

    /// D38, M6 rule R6: the URL and SHA-256 of the agent and the URL of the runner in the cache
    /// bucket, which VMs of every provider download without credentials, and with an S3 cache
    /// the bucket's location. No secret.
    pub(super) async fn vm_shared_env(&self) -> Result<BTreeMap<String, String>> {
        let unusable = |reason: String| {
            Error::provider(format!("{PROVIDER_NAME}: shared environment: {reason}"))
        };
        let agent = self
            .start_agent()?
            .ok_or_else(|| unusable(format!("{} missing", cloud_init::AGENT_SHA256_KEY)))?;
        let project_id = self
            .client
            .find_project(self.name())
            .await?
            .ok_or_else(|| unusable(format!("project {} does not exist", self.name())))?;
        let region = self.client.region();
        let mut env = BTreeMap::from([
            (
                cloud_init::AGENT_URL_KEY.to_string(),
                cloud_init::agent_url(&self.config, region, &project_id, &agent),
            ),
            (cloud_init::AGENT_SHA256_KEY.to_string(), agent),
            (
                cloud_init::RUNNER_URL_KEY.to_string(),
                runner_url(&self.config, region, &project_id, self.runner_sha256()),
            ),
        ]);
        if self.cache_on() {
            env.extend(cloud_init::cache_location(
                &self.config,
                self.client.region(),
                &project_id,
            ));
        }
        Ok(env)
    }

    pub(super) async fn sweep_servers(&self, now: u64) -> Result<Vec<String>> {
        let Some(project_id) = self.client.find_project(self.name()).await? else {
            return Ok(Vec::new());
        };
        let api = self.instances();
        let mut swept = Vec::new();
        for server in self.servers(&project_id).await? {
            let id = server["id"].as_str().unwrap_or_default().to_string();
            if matches!(server["state"].as_str(), Some("stopping" | "locked")) {
                continue;
            }
            let Some(capacity) = vm::capacity(&server) else {
                self.log.warn(
                    "sweep skipped",
                    &[("id", &id), ("reason", "no creation date")],
                );
                continue;
            };
            let Some(reason) = vm::sweep_reason(&capacity, vm::drain_since(&server), now) else {
                continue;
            };
            match self.remove_server(&api, &server).await {
                Ok(true) => {
                    self.log
                        .info("swept server", &[("id", &id), ("reason", reason)]);
                    swept.push(id);
                }
                Ok(false) => {}
                Err(error) => self.log.warn(
                    "sweep failed",
                    &[("id", &id), ("error", &error.to_string())],
                ),
            }
        }
        Ok(swept)
    }
}
