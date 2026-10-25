//! Provider `scaleway`. A pool runs on Scaleway Instances (`instances`, the default: one VM per
//! run with its own Docker daemon, spec section 6.1) or on Serverless Jobs (`jobs`: one job
//! definition per pool, one job run per pending job). Shared resources: project, IAM
//! applications and keys, cache bucket with lifecycle and the VM agent (D38), the optional
//! registry namespace; per runtime the VM security group or the job definitions. Every
//! resource is looked up by name or tag; nothing is stored locally.
//!
//! Pools whose runtime is a key of the config's `providers` belong to another provider (spec
//! D35): this one plans, starts and lists nothing for them and answers their runs with the agent
//! URL and cache location of [`Provider::shared_env`].
//!
//! Modules: `steps` plans and applies the resources, `keys` the IAM keys, `bucket` the cache
//! bucket, `agent` the VM agent in it, `registry` the registry namespace, `servers` the VM
//! runtime, `jobs` the Serverless Jobs runtime; `iam`, `instance`, `s3` and `sigv4` are the API
//! clients.

mod agent;
mod bucket;
mod cloud_init;
mod iam;
mod instance;
mod jobs;
mod keys;
mod registry;
pub mod s3;
mod servers;
pub mod sigv4;
mod steps;
pub mod vm;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::{CacheType, Config, Pool};
use ciburst_core::log::Log;
use ciburst_core::types::{
    Billing, Capabilities, Outputs, Provider, Rotation, Run, StartRequest, Step, VmCapacity,
    VmReport,
};
use ciburst_core::{Error, Result};
use serde_json::{Value, json};

use crate::client::ScalewayClient;
use instance::InstanceApi;

pub use jobs::bucket_name;

pub const PROVIDER_NAME: &str = "scaleway";

/// The provider type of M2, which only had Serverless Jobs.
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

    /// `provider.runtime`, the runtime of pools that name none: `instances` unless set. A config
    /// still typed `scaleway-jobs` keeps its job definitions, so upgrading changes nothing.
    fn default_runtime(&self) -> Result<Runtime> {
        match self.config.provider.extra.get("runtime") {
            None | Some(Value::Null) if self.config.provider.kind == LEGACY_JOBS_TYPE => {
                Ok(Runtime::Jobs)
            }
            None | Some(Value::Null) => Ok(Runtime::Instances),
            Some(value) => value.as_str().and_then(Runtime::parse).ok_or_else(|| {
                Error::provider(format!(
                    "{PROVIDER_NAME}: provider.runtime {value} is not instances or jobs"
                ))
            }),
        }
    }

    fn runtime(&self, pool: &Pool) -> Result<Runtime> {
        match &pool.runtime {
            None => self.default_runtime(),
            Some(value) => Runtime::parse(value).ok_or_else(|| {
                Error::provider(format!(
                    "{PROVIDER_NAME}: pool {}: runtime {value} is not instances or jobs",
                    pool.tag
                ))
            }),
        }
    }

    /// The runtime of the pool `tag`; a pool gone from the config counts as the default.
    fn runtime_of(&self, tag: &str) -> Result<Runtime> {
        match self.config.pool_by_tag(tag) {
            Some(pool) => self.runtime(pool),
            None => self.default_runtime(),
        }
    }

    /// The pools this provider serves: every pool not handed to another provider.
    fn own_pools(&self) -> impl Iterator<Item = &Pool> {
        self.config
            .pools
            .iter()
            .filter(|pool| self.config.extra_provider_of(pool).is_none())
    }

    /// The own pools with their runtimes.
    fn own_runtimes(&self) -> Result<Vec<(&Pool, Runtime)>> {
        let mut runtimes = Vec::new();
        for pool in self.own_pools() {
            runtimes.push((pool, self.runtime(pool)?));
        }
        Ok(runtimes)
    }

    /// The other provider the pool `tag` belongs to, `None` for a pool of this one.
    fn other_provider(&self, tag: &str) -> Option<&str> {
        self.config
            .pool_by_tag(tag)
            .and_then(|pool| self.config.extra_provider_of(pool))
    }

    fn pools_on(&self, runtime: Runtime) -> Result<Vec<&Pool>> {
        let mut pools = Vec::new();
        for pool in self.own_pools() {
            if self.runtime(pool)? == runtime {
                pools.push(pool);
            }
        }
        Ok(pools)
    }

    /// Object Storage scoped to `project_id`: Scaleway creates and finds buckets in the API
    /// key's preferred project unless the access key names another as `<access key>@<project>`.
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
    fn capabilities(&self) -> Capabilities {
        let vm = self.default_runtime().unwrap_or(Runtime::Instances) == Runtime::Instances;
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
        self.preflight_checks().await
    }

    async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
        self.plan_steps(rotate).await
    }

    async fn apply(&self, steps: &[Step]) -> Result<Outputs> {
        self.apply_steps(steps).await
    }

    async fn destroy(&self) -> Result<()> {
        self.destroy_all().await
    }

    async fn start(&self, request: StartRequest) -> Result<String> {
        if let Some(other) = self.config.extra_provider_of(&request.pool) {
            return Err(Error::provider(format!(
                "{PROVIDER_NAME}: pool {} belongs to provider {other}",
                request.pool.tag
            )));
        }
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

    /// A pool of another provider has no runs here. Servers left from a pool that moved to
    /// another provider stay in `list(None)` and in the sweep: they run here until they end.
    async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
        if pool.is_some_and(|tag| self.other_provider(tag).is_some()) {
            return Ok(Vec::new());
        }
        let Some(project_id) = self.client.find_project(self.name()).await? else {
            return Ok(Vec::new());
        };
        let (job_tags, vms): (Vec<&str>, Option<Option<&str>>) = match pool {
            Some(tag) => match self.runtime_of(tag)? {
                Runtime::Jobs => (vec![tag], None),
                Runtime::Instances => (Vec::new(), Some(Some(tag))),
            },
            None => (
                self.pools_on(Runtime::Jobs)?
                    .iter()
                    .map(|p| p.tag.as_str())
                    .collect(),
                Some(None),
            ),
        };
        let mut runs = self.job_runs(&project_id, &job_tags).await?;
        if let Some(pool) = vms {
            runs.extend(self.vm_runs(&project_id, pool).await?);
        }
        Ok(runs)
    }

    /// A run id is a server id or a job run id; servers are looked up first, so a VM that
    /// outlived its pool's switch to `jobs` still ends.
    async fn stop(&self, run_id: &str) -> Result<()> {
        if self.stop_server(run_id).await? {
            return Ok(());
        }
        if self.pools_on(Runtime::Jobs)?.is_empty() {
            return Err(Error::provider(format!(
                "{PROVIDER_NAME}: server {run_id} not found"
            )));
        }
        jobs::stop(&self.client, run_id).await
    }

    async fn capacity(&self, pool: &str) -> Result<Option<Vec<VmCapacity>>> {
        if self.other_provider(pool).is_some() {
            return Ok(Some(Vec::new()));
        }
        if self.runtime_of(pool)? != Runtime::Instances {
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
}
