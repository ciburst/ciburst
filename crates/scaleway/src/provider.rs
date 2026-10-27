//! Provider `scaleway`, the only provider (D65). The VM runner runs on Scaleway Instances (one
//! VM per run with its own Docker daemon, spec section 6.1); each project's lanes run on
//! Serverless Jobs (one job definition `<name>-<project id>-<tag>` per lane, one job run per
//! pending job, D45, D60). Shared resources:
//! project, IAM applications and keys, cache bucket with lifecycle, the runner (M6 rule R6) and
//! the VM agent (D38) in it, the optional registry namespace; per runtime the VM security group
//! or the job definitions. Every resource is looked up by name or tag; nothing is stored
//! locally.
//!
//! The definitions of the host's lanes follow the config on `apply`; those of a member's lanes
//! follow its `/join` through [`Provider::sync_lanes`], and `apply` leaves them alone.
//!
//! Modules: `steps` plans and applies the resources, `keys` the IAM keys, `bucket` the cache
//! bucket, `runner` the runner in it, `agent` the VM agent in it, `registry` the registry
//! namespace, `servers` the VM runtime, `vm_start` what a VM start uses without looking it up,
//! `jobs` the Serverless Jobs runtime; `iam`, `instance`, `s3` and `sigv4` are the API clients.

mod agent;
mod bucket;
mod cloud_init;
mod iam;
mod instance;
mod jobs;
mod keys;
mod registry;
mod runner;
pub mod s3;
mod servers;
pub mod sigv4;
mod steps;
pub mod vm;
mod vm_start;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::{CacheType, Config, Pool, parse_lane_key};
use ciburst_core::log::Log;
use ciburst_core::types::{
    Billing, Capabilities, Outputs, Provider, Rotation, Run, StartRequest, Step, VmCapacity,
    VmReport,
};
use ciburst_core::{Error, Result};
use serde_json::{Value, json};

use crate::client::ScalewayClient;
use instance::InstanceApi;
use runner::RunnerSource;

pub use jobs::bucket_name;
pub use vm_start::{IMAGES_VAR, SECURITY_GROUP_VAR, VmImage, VmStart};

pub const PROVIDER_NAME: &str = "scaleway";

/// The provider type of M2, which only had Serverless Jobs; read as `scaleway`.
pub const LEGACY_JOBS_TYPE: &str = "scaleway-jobs";

/// Commercial types tried in order when a pool names none (rule 4 of M3). PLAY2 types have no
/// local storage, so no local image fits them and start skips them.
/// Types with a local root volume, which terminate deletes; PLAY2 types only take block
/// volumes, which terminate detaches and which would keep billing.
pub const DEFAULT_INSTANCE_TYPES: [&str; 2] = ["DEV1-M", "DEV1-L"];

/// The pool's commercial types in fallback order, [`DEFAULT_INSTANCE_TYPES`] when unset.
fn instance_types(pool: &Pool) -> Vec<String> {
    if pool.instance_types.is_empty() {
        DEFAULT_INSTANCE_TYPES
            .iter()
            .map(|t| t.to_string())
            .collect()
    } else {
        pool.instance_types.clone()
    }
}

/// The user data key cloud-init reads on Scaleway Instances.
pub const USER_DATA_KEY: &str = "cloud-init";

const PAGE_SIZE: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runtime {
    Instances,
    Jobs,
}

impl Runtime {
    fn parse(value: &str) -> Option<Runtime> {
        match value {
            "instances" => Some(Runtime::Instances),
            "jobs" => Some(Runtime::Jobs),
            _ => None,
        }
    }
}

pub struct Scaleway {
    config: Arc<Config>,
    client: Arc<ScalewayClient>,
    log: Log,
    s3_access_key: Option<String>,
    zone: String,
    /// The Linux musl build of `ciburst-vm-agent` that `apply` uploads (D38).
    agent: Option<PathBuf>,
    /// `CIBURST_AGENT_SHA256` of the function's environment.
    agent_sha256: Option<String>,
    /// The runner that `apply` copies into the cache bucket (M6 rule R6).
    runner: RunnerSource,
    /// `CIBURST_VM_SECURITY_GROUP` and `CIBURST_VM_IMAGES` of the function's environment.
    vm_start: VmStart,
}

impl Scaleway {
    pub fn new(config: Arc<Config>, client: Arc<ScalewayClient>, log: Log) -> Scaleway {
        let s3_access_key = client.access_key().map(String::from);
        let zone = config
            .provider
            .extra
            .get("zone")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("{}-1", client.region()));
        Scaleway {
            config,
            client,
            log,
            s3_access_key,
            zone,
            agent: None,
            agent_sha256: None,
            runner: RunnerSource::default(),
            vm_start: VmStart::default(),
        }
    }

    /// The agent binary for the `vm:agent` step; `None` keeps an uploaded agent.
    pub fn with_agent(mut self, path: Option<PathBuf>) -> Scaleway {
        self.agent = path;
        self
    }

    /// The access key that belongs to the client's secret key. S3 signs with the pair, so the
    /// cache bucket steps need it; start, list and stop do not.
    pub fn with_s3_access_key(mut self, access_key: impl Into<String>) -> Scaleway {
        self.s3_access_key = Some(access_key.into()).filter(|k| !k.is_empty());
        self
    }

    fn name(&self) -> &str {
        &self.config.name
    }

    fn instances(&self) -> InstanceApi<'_> {
        InstanceApi::new(&self.client, &self.zone)
    }

    fn cache_on(&self) -> bool {
        self.config.cache.kind == CacheType::S3
    }

    /// `provider.registry: true` keeps a Container Registry namespace for the pool images.
    fn registry_on(&self) -> bool {
        self.config
            .provider
            .extra
            .get("registry")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    fn registry_public(&self) -> bool {
        self.config
            .provider
            .extra
            .get("registryPublic")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The runtime of a run spec: `jobs` for a lane, `instances` for the VM runner.
    fn runtime(&self, pool: &Pool) -> Result<Runtime> {
        match &pool.runtime {
            None if pool.project.is_some() => Ok(Runtime::Jobs),
            None => Ok(Runtime::Instances),
            Some(value) => Runtime::parse(value).ok_or_else(|| {
                Error::provider(format!(
                    "{PROVIDER_NAME}: pool {}: runtime {value} is not instances or jobs",
                    pool.tag
                ))
            }),
        }
    }

    /// The runtime of the runs tagged `tag`: `jobs` for a lane's key `<project id>-<tag>` (of
    /// any project), else `instances`.
    fn runtime_of(&self, tag: &str) -> Runtime {
        if tag != self.config.vm.tag && parse_lane_key(tag).is_some() {
            Runtime::Jobs
        } else {
            Runtime::Instances
        }
    }

    /// The run specs of the config: the VM runner and the host's lanes, with their runtimes.
    fn own_runtimes(&self) -> Result<Vec<(Pool, Runtime)>> {
        let mut runtimes = Vec::new();
        for pool in self.config.pools() {
            let runtime = self.runtime(&pool)?;
            runtimes.push((pool, runtime));
        }
        Ok(runtimes)
    }

    fn pools_on(&self, runtime: Runtime) -> Result<Vec<Pool>> {
        Ok(self
            .own_runtimes()?
            .into_iter()
            .filter(|(_, r)| *r == runtime)
            .map(|(pool, _)| pool)
            .collect())
    }

    /// Object Storage scoped to `project_id`: Scaleway creates and finds buckets in the API
    /// key's preferred project unless the access key names another as `<access key>@<project>`.
    /// D43: VMs, cache bucket, agent object and dispatcher share one region, so job caches never
    /// cross regions; a zone outside the provider's region is a config error.
    fn zone_in_region(&self) -> Result<()> {
        let region = self.client.region();
        if self.zone.starts_with(&format!("{region}-")) {
            return Ok(());
        }
        let wanted = self
            .zone
            .rsplit_once('-')
            .map_or(self.zone.as_str(), |(r, _)| r);
        Err(Error::provider(format!(
            "{PROVIDER_NAME}: provider.zone {} lies outside provider.region {region}: set provider.region to {wanted}; the zone defaults to {wanted}-1",
            self.zone
        )))
    }

    fn s3(&self, project_id: &str) -> Result<s3::S3> {
        let access_key = self.s3_access_key.as_deref().ok_or_else(|| {
            Error::provider(format!(
                "{PROVIDER_NAME}: the cache bucket needs the access key of SCW_SECRET_KEY"
            ))
        })?;
        Ok(s3::S3 {
            http: self.client.http(),
            region: self.client.region().to_string(),
            access_key: format!("{access_key}@{project_id}"),
            secret_key: self.client.secret_key().to_string(),
        })
    }

    fn bucket(&self, project_id: &str) -> String {
        jobs::bucket_name(&self.config, project_id)
    }

    fn require_project<'a>(&self, project: &'a Option<String>) -> Result<&'a str> {
        project.as_deref().ok_or_else(|| {
            Error::provider(format!(
                "{PROVIDER_NAME}: project {} does not exist; apply its step first",
                self.name()
            ))
        })
    }

    async fn create_project(&self) -> Result<String> {
        let project = self
            .client
            .post(
                "/account/v3/projects",
                &json!({
                    "name": self.name(),
                    "organization_id": self.client.organization_id(),
                }),
            )
            .await?;
        let id = project["id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| Error::provider("scaleway project: response without id"))?;
        self.log
            .info("created project", &[("name", self.name()), ("id", &id)]);
        Ok(id)
    }
}

/// Every item of a paginated Scaleway list (`page`, `page_size`, `total_count`).
async fn list_all(
    client: &ScalewayClient,
    path: &str,
    query: Vec<(&'static str, String)>,
    field: &str,
) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut page = 1u32;
    loop {
        let mut q = query.clone();
        q.push(("page", page.to_string()));
        q.push(("page_size", PAGE_SIZE.to_string()));
        let value = client.get(path, &q).await?;
        let items = value[field].as_array().cloned().unwrap_or_default();
        let count = items.len();
        out.extend(items);
        let done = count == 0
            || match value["total_count"].as_u64() {
                Some(total) => out.len() as u64 >= total,
                None => count < PAGE_SIZE,
            };
        if done {
            return Ok(out);
        }
        page += 1;
    }
}

#[async_trait]
impl Provider for Scaleway {
    /// The fleet store in the deployment's cache bucket (M6.3 rule R1); `None` before the first
    /// apply made the project and the bucket, or without the cache and fleet keys.
    async fn fleet(
        &self,
        secrets: &ciburst_core::types::Secrets,
    ) -> Result<Option<Arc<dyn ciburst_core::fleet::FleetStore>>> {
        let (Some(cache), Some(key)) = (&secrets.cache, &secrets.fleet_key) else {
            return Ok(None);
        };
        let Some(project_id) = self.client.find_project(&self.config.name).await? else {
            return Ok(None);
        };
        if !self
            .s3(&project_id)?
            .bucket_exists(&self.bucket(&project_id))
            .await?
        {
            return Ok(None);
        }
        Ok(Some(Arc::new(
            crate::fleet::ScalewayFleetStore::in_cache_bucket(
                self.client.http(),
                &self.config,
                &project_id,
                self.client.region(),
                cache,
                crate::fleet::FleetKey::from_secret(key)?,
            ),
        )))
    }

    fn capabilities(&self) -> Capabilities {
        let vm = true;
        Capabilities {
            name: PROVIDER_NAME.into(),
            docker_daemon: vm,
            max_vcpu: 6,
            max_memory_mib: 16384,
            max_storage_mib: 10240,
            max_duration_s: 86400,
            archs: vec!["amd64".into()],
            billing: if vm {
                Billing::PerHour
            } else {
                Billing::PerSecond
            },
        }
    }

    /// D42: a VM pool without quota for any of its types in the zone, or no agent, fails
    /// before `apply` changes anything.
    async fn preflight(&self) -> Result<()> {
        self.zone_in_region()?;
        self.preflight_checks().await
    }

    async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
        self.zone_in_region()?;
        self.plan_steps(rotate).await
    }

    async fn apply(&self, steps: &[Step]) -> Result<Outputs> {
        self.apply_steps(steps).await
    }

    async fn destroy(&self) -> Result<()> {
        self.destroy_all().await
    }

    async fn start(&self, request: StartRequest) -> Result<String> {
        if request.env.get("RUNNER_TOKEN").is_none_or(|t| t.is_empty()) {
            return Err(Error::provider(format!(
                "{PROVIDER_NAME}: RUNNER_TOKEN missing"
            )));
        }
        match self.runtime(&request.pool)? {
            Runtime::Jobs => self.start_job(request).await,
            Runtime::Instances => self.start_vm(request).await,
        }
    }

    /// The runs of `pool`, the VM runner's tag or a lane's key; with `None` every VM and the
    /// runs of every lane of every project.
    async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
        let Some(project_id) = self.client.find_project(self.name()).await? else {
            return Ok(Vec::new());
        };
        let (job_tags, vms): (Option<Vec<String>>, Option<Option<&str>>) = match pool {
            Some(tag) => match self.runtime_of(tag) {
                Runtime::Jobs => (Some(vec![tag.to_string()]), None),
                Runtime::Instances => (Some(Vec::new()), Some(Some(tag))),
            },
            None => (None, Some(None)),
        };
        let mut runs = self.job_runs(&project_id, job_tags.as_deref()).await?;
        if let Some(pool) = vms {
            runs.extend(self.vm_runs(&project_id, pool).await?);
        }
        Ok(runs)
    }

    /// A run id is a server id or a job run id; servers are looked up first.
    async fn stop(&self, run_id: &str) -> Result<()> {
        if self.stop_server(run_id).await? {
            return Ok(());
        }
        jobs::stop(&self.client, run_id).await
    }

    async fn capacity(&self, pool: &str) -> Result<Option<Vec<VmCapacity>>> {
        if self.runtime_of(pool) != Runtime::Instances {
            return Ok(None);
        }
        self.vm_capacity(pool).await.map(Some)
    }

    async fn claim(&self, run_id: &str) -> Result<()> {
        self.claim_slot(run_id).await
    }

    async fn report(&self, run_id: &str, report: &VmReport, now: u64) -> Result<()> {
        self.store_report(run_id, report, now).await
    }

    async fn shared_env(&self) -> Result<BTreeMap<String, String>> {
        self.vm_shared_env().await
    }

    async fn sweep(&self, now: u64) -> Result<Vec<String>> {
        self.sweep_servers(now).await
    }

    async fn sync_lanes(&self, project: u64, lanes: &[Pool]) -> Result<BTreeMap<String, String>> {
        self.sync_lane_definitions(project, lanes).await
    }
}
