//! Provider `scaleway`. A pool runs on Scaleway Instances (`instances`, the default: one VM per
//! run with its own Docker daemon, spec section 6.1) or on Serverless Jobs (`jobs`: one job
//! definition per pool, one job run per pending job). Shared resources: project, IAM
//! applications and keys, cache bucket with lifecycle, registry namespace; per runtime the VM
//! security group or the job definitions. Every resource is looked up by name or tag; nothing
//! is stored locally.
//!
//! Pools whose runtime is a key of the config's `providers` belong to another provider (spec
//! D35): this one plans, starts and lists nothing for them and answers their runs with the agent
//! image and cache location of [`Provider::shared_env`].

mod cloud_init;
mod iam;
mod instance;
mod jobs;
mod registry;
pub mod s3;
pub mod sigv4;
pub mod vm;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::{CacheType, Config, Pool};
use ciburst_core::log::Log;
use ciburst_core::provision::keys;
use ciburst_core::types::{
    Action, Billing, Capabilities, KeyPair, Outputs, Provider, Rotation, Run, StartRequest, Step,
    VmCapacity, VmReport,
};
use ciburst_core::{Error, Result};
use serde_json::{Value, json};

use crate::client::ScalewayClient;
use iam::AppState;
use instance::{ApiError, DOCKER_IMAGE_LABEL, FALLBACK_IMAGE_LABEL, InstanceApi};
use registry::NamespaceState;

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

const SECURITY_GROUP_STEP: &str = "vm:security-group";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    Dispatch,
    Cache,
}

impl KeyKind {
    fn suffix(self) -> &'static str {
        match self {
            KeyKind::Dispatch => "dispatch",
            KeyKind::Cache => "cache",
        }
    }

    fn step_id(self) -> &'static str {
        match self {
            KeyKind::Dispatch => "key:dispatch",
            KeyKind::Cache => "key:cache",
        }
    }

    /// Starting a job whose image is in a private registry of the project needs registry read
    /// access besides the Jobs permissions (Scaleway answers 403 on `registry_image` otherwise).
    /// The dispatcher also starts, tags and terminates VMs.
    fn permission_sets(self) -> &'static [&'static str] {
        match self {
            KeyKind::Dispatch => &[
                "ServerlessJobsFullAccess",
                "ContainerRegistryReadOnly",
                "InstancesFullAccess",
            ],
            KeyKind::Cache => &["ObjectStorageFullAccess"],
        }
    }

    fn rotation(self) -> Rotation {
        match self {
            KeyKind::Dispatch => Rotation::Key,
            KeyKind::Cache => Rotation::Cache,
        }
    }

    fn outputs(self) -> (&'static str, &'static str) {
        match self {
            KeyKind::Dispatch => ("CIBURST_DISPATCH_ACCESS_KEY", "CIBURST_DISPATCH_SECRET_KEY"),
            KeyKind::Cache => ("CIBURST_CACHE_ACCESS_KEY", "CIBURST_CACHE_SECRET_KEY"),
        }
    }
}

/// The cache bucket as found: `None` when it does not exist, else its lifecycle days.
type BucketState = Option<Option<u32>>;

struct Observed {
    project: Option<String>,
    dispatch: Option<AppState>,
    cache: Option<AppState>,
    bucket: BucketState,
    registry: Option<NamespaceState>,
    security_group: Option<String>,
    definitions: Vec<Value>,
}

/// What a start attempt on one commercial type came to.
enum Attempt {
    Started,
    Unavailable(ApiError),
}

pub struct ScalewayJobs {
    config: Arc<Config>,
    client: Arc<ScalewayClient>,
    log: Log,
    s3_access_key: Option<String>,
    zone: String,
}

/// The provider serves both runtimes; the old name stays for existing callers.
pub type Scaleway = ScalewayJobs;

impl ScalewayJobs {
    pub fn new(config: Arc<Config>, client: Arc<ScalewayClient>, log: Log) -> ScalewayJobs {
        let s3_access_key = client.access_key().map(String::from);
        let zone = config
            .provider
            .extra
            .get("zone")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("{}-1", client.region()));
        ScalewayJobs {
            config,
            client,
            log,
            s3_access_key,
            zone,
        }
    }

    /// The access key that belongs to the client's secret key. S3 signs with the pair, so the
    /// cache bucket steps need it; start, list and stop do not.
    pub fn with_s3_access_key(mut self, access_key: impl Into<String>) -> ScalewayJobs {
        self.s3_access_key = Some(access_key.into()).filter(|k| !k.is_empty());
        self
    }

    fn name(&self) -> &str {
        &self.config.name
    }

    fn app_name(&self, kind: KeyKind) -> String {
        format!("{}-{}", self.name(), kind.suffix())
    }

    fn definition_name(&self, tag: &str) -> String {
        format!("{}-{tag}", self.name())
    }

    fn definition_tag<'a>(&self, definition: &'a Value) -> Option<&'a str> {
        definition["name"]
            .as_str()?
            .strip_prefix(self.name())?
            .strip_prefix('-')
            .filter(|tag| !tag.is_empty())
    }

    fn security_group_name(&self) -> String {
        format!("{}-vm", self.name())
    }

    /// Warns for each VM pool none of whose instance types has quota in the zone, as the
    /// dispatcher could start no VM for it. Quotas are per organization, zone and type; a
    /// failed lookup only logs.
    async fn warn_without_quota(&self, runtimes: &[(&Pool, Runtime)]) {
        let mut limits: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();
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
                            return;
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
                self.log.warn(
                    "vm pools cannot start",
                    &[("pool", &pool.tag), ("reason", &reason)],
                );
            }
        }
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

    fn s3(&self) -> Result<s3::S3> {
        let access_key = self.s3_access_key.clone().ok_or_else(|| {
            Error::provider(format!(
                "{PROVIDER_NAME}: the cache bucket needs the access key of SCW_SECRET_KEY"
            ))
        })?;
        Ok(s3::S3 {
            http: self.client.http(),
            region: self.client.region().to_string(),
            access_key,
            secret_key: self.client.secret_key().to_string(),
        })
    }

    fn bucket(&self, project_id: &str) -> String {
        jobs::bucket_name(&self.config, project_id)
    }

    async fn observe(&self) -> Result<Observed> {
        let project = self.client.find_project(self.name()).await?;
        let dispatch =
            iam::find_application(&self.client, &self.app_name(KeyKind::Dispatch)).await?;
        let cache = iam::find_application(&self.client, &self.app_name(KeyKind::Cache)).await?;
        let mut bucket = None;
        let mut namespace = None;
        let mut security_group = None;
        let mut definitions = Vec::new();
        if let Some(project_id) = &project {
            if self.cache_on() || self.s3_access_key.is_some() {
                let s3 = self.s3()?;
                let name = self.bucket(project_id);
                if s3.bucket_exists(&name).await? {
                    bucket = Some(s3.lifecycle_days(&name).await?);
                }
            }
            namespace = registry::find_namespace(&self.client, project_id, self.name()).await?;
            security_group = self
                .instances()
                .find_security_group(project_id, &self.security_group_name())
                .await?;
            definitions = jobs::list_definitions(&self.client, project_id)
                .await?
                .into_iter()
                .filter(|d| self.definition_tag(d).is_some())
                .collect();
        }
        Ok(Observed {
            project,
            dispatch,
            cache,
            bucket,
            registry: namespace,
            security_group,
            definitions,
        })
    }

    fn key_step(&self, kind: KeyKind, app: &Option<AppState>, rotate: &[Rotation]) -> Step {
        let resource = format!("api key {}", self.app_name(kind));
        match app {
            None => Step::new(kind.step_id(), Action::Create, resource),
            Some(app) if !app.complete(kind.permission_sets()) => {
                Step::new(kind.step_id(), Action::Update, resource).with_detail("repair")
            }
            Some(_) if rotate.contains(&kind.rotation()) => {
                Step::new(kind.step_id(), Action::Update, resource).with_detail("rotate")
            }
            Some(_) => Step::new(kind.step_id(), Action::Noop, resource),
        }
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

    /// Application, policy and a fresh API key; existing keys of the application are deleted
    /// because their secrets cannot be read back.
    async fn ensure_key(&self, kind: KeyKind, project_id: &str) -> Result<KeyPair> {
        let name = self.app_name(kind);
        let app = match iam::find_application(&self.client, &name).await? {
            Some(app) => app,
            None => {
                let id = iam::create_application(&self.client, &name).await?;
                self.log
                    .info("created iam application", &[("name", &name), ("id", &id)]);
                AppState {
                    id,
                    ..AppState::default()
                }
            }
        };
        if !app.grants(kind.permission_sets()) {
            for policy in &app.policies {
                iam::delete_policy(&self.client, policy).await?;
                self.log
                    .info("deleted iam policy", &[("name", &name), ("id", policy)]);
            }
            let id = iam::create_policy(
                &self.client,
                &name,
                &app.id,
                kind.permission_sets(),
                project_id,
            )
            .await?;
            self.log
                .info("created iam policy", &[("name", &name), ("id", &id)]);
        }
        for access_key in &app.keys {
            iam::delete_api_key(&self.client, access_key).await?;
            self.log.info(
                "deleted api key",
                &[("application", &name), ("id", &app.id)],
            );
        }
        let pair = iam::create_api_key(&self.client, &app.id, project_id).await?;
        self.log.info(
            "created api key",
            &[("application", &name), ("id", &app.id)],
        );
        Ok(pair)
    }

    async fn delete_key(&self, kind: KeyKind) -> Result<()> {
        let name = self.app_name(kind);
        if let Some(app) = iam::find_application(&self.client, &name).await? {
            iam::delete_application(&self.client, &app).await?;
            self.log.info(
                "deleted iam application",
                &[("name", &name), ("id", &app.id)],
            );
        }
        Ok(())
    }

    async fn apply_bucket(&self, action: Action, project: &Option<String>) -> Result<()> {
        if action == Action::Delete {
            if let Some(project_id) = project {
                self.delete_bucket(project_id).await?;
            }
            return Ok(());
        }
        let project_id = self.require_project(project)?;
        let s3 = self.s3()?;
        let name = self.bucket(project_id);
        if !s3.bucket_exists(&name).await? {
            s3.create_bucket(&name).await?;
            self.log.info("created bucket", &[("name", &name)]);
        }
        let days = self.config.cache.expire_days;
        if s3.lifecycle_days(&name).await? != Some(days) {
            s3.put_lifecycle(&name, days).await?;
            self.log.info(
                "updated bucket lifecycle",
                &[("name", &name), ("expire_days", &days.to_string())],
            );
        }
        Ok(())
    }

    async fn delete_bucket(&self, project_id: &str) -> Result<()> {
        let s3 = self.s3()?;
        let name = self.bucket(project_id);
        if s3.bucket_exists(&name).await? {
            s3.empty_and_delete(&name).await?;
            self.log.info("deleted bucket", &[("name", &name)]);
        }
        Ok(())
    }

    async fn ensure_registry(&self, project_id: &str) -> Result<NamespaceState> {
        let name = self.name();
        let public = self.registry_public();
        let mut namespace = match registry::find_namespace(&self.client, project_id, name).await? {
            Some(namespace) => namespace,
            None => {
                let namespace =
                    registry::create_namespace(&self.client, project_id, name, public).await?;
                self.log.info(
                    "created registry namespace",
                    &[
                        ("name", name),
                        ("id", &namespace.id),
                        ("endpoint", &namespace.endpoint),
                    ],
                );
                namespace
            }
        };
        if namespace.is_public != public {
            registry::update_namespace(&self.client, &namespace.id, public).await?;
            namespace.is_public = public;
            self.log.info(
                "updated registry namespace",
                &[("name", name), ("public", &public.to_string())],
            );
        }
        Ok(namespace)
    }

    async fn delete_registry(&self, project: &Option<String>) -> Result<()> {
        let Some(project_id) = project else {
            return Ok(());
        };
        if let Some(namespace) =
            registry::find_namespace(&self.client, project_id, self.name()).await?
        {
            registry::delete_namespace(&self.client, &namespace.id).await?;
            self.log.info(
                "deleted registry namespace",
                &[("name", self.name()), ("id", &namespace.id)],
            );
        }
        Ok(())
    }

    async fn ensure_security_group(&self, project_id: &str) -> Result<()> {
        let name = self.security_group_name();
        let api = self.instances();
        if api.find_security_group(project_id, &name).await?.is_none() {
            let id = api.create_security_group(project_id, &name).await?;
            self.log
                .info("created security group", &[("name", &name), ("id", &id)]);
        }
        Ok(())
    }

    async fn delete_security_group(&self, project_id: &str) -> Result<()> {
        let name = self.security_group_name();
        let api = self.instances();
        if let Some(id) = api.find_security_group(project_id, &name).await? {
            api.delete_security_group(&id).await?;
            self.log
                .info("deleted security group", &[("name", &name), ("id", &id)]);
        }
        Ok(())
    }

    async fn apply_definition(&self, tag: &str, action: Action, project_id: &str) -> Result<()> {
        let name = self.definition_name(tag);
        let existing = jobs::list_definitions(&self.client, project_id)
            .await?
            .into_iter()
            .find(|d| d["name"].as_str() == Some(name.as_str()));
        let existing_id = existing
            .as_ref()
            .and_then(|d| d["id"].as_str())
            .map(String::from);
        if action == Action::Delete {
            if let Some(id) = existing_id {
                jobs::delete_definition(&self.client, &id).await?;
                self.log
                    .info("deleted job definition", &[("name", &name), ("id", &id)]);
            }
            return Ok(());
        }
        let pool = self
            .config
            .pool_by_tag(tag)
            .ok_or_else(|| Error::provider(format!("{PROVIDER_NAME}: unknown pool {tag}")))?;
        let body = jobs::desired_definition(&self.config, pool, self.client.region(), project_id);
        match (existing, existing_id) {
            (Some(current), Some(id)) => {
                if !jobs::differences(&body, &current).is_empty() {
                    jobs::update_definition(&self.client, &id, &body).await?;
                    self.log
                        .info("updated job definition", &[("name", &name), ("id", &id)]);
                }
            }
            _ => {
                let id = jobs::create_definition(&self.client, &body).await?;
                self.log
                    .info("created job definition", &[("name", &name), ("id", &id)]);
            }
        }
        Ok(())
    }

    async fn find_definition(&self, project_id: &str, tag: &str) -> Result<Option<String>> {
        let name = self.definition_name(tag);
        Ok(jobs::list_definitions(&self.client, project_id)
            .await?
            .iter()
            .find(|d| d["name"].as_str() == Some(name.as_str()))
            .and_then(|d| d["id"].as_str().map(String::from)))
    }

    async fn start_job(&self, request: StartRequest) -> Result<String> {
        let tag = &request.pool.tag;
        let not_provisioned =
            || Error::provider(format!("{PROVIDER_NAME}: pool {tag} is not provisioned"));
        let project_id = self
            .client
            .find_project(self.name())
            .await?
            .ok_or_else(not_provisioned)?;
        let definition_id = self
            .find_definition(&project_id, tag)
            .await?
            .ok_or_else(not_provisioned)?;
        let env = jobs::run_env(&request.env, &request.labels);
        jobs::start(&self.client, &definition_id, &env).await
    }

    /// Rule 4 of M3: the first commercial type that has a local image and capacity runs the
    /// VM; the server id is the run id.
    async fn start_vm(&self, request: StartRequest) -> Result<String> {
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
        if !self.registry_on() {
            return Err(Error::provider(format!(
                "{PROVIDER_NAME}: pool {tag}: the instances runtime pulls its agent from the \
                 registry namespace; set provider.registry"
            )));
        }
        let not_provisioned =
            || Error::provider(format!("{PROVIDER_NAME}: pool {tag} is not provisioned"));
        let project_id = self
            .client
            .find_project(self.name())
            .await?
            .ok_or_else(not_provisioned)?;
        let namespace = registry::find_namespace(&self.client, &project_id, self.name())
            .await?
            .ok_or_else(not_provisioned)?;
        if !namespace.is_public {
            return Err(Error::provider(format!(
                "{PROVIDER_NAME}: pool {tag}: VMs pull the agent image without credentials; \
                 set provider.registryPublic"
            )));
        }
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
                .boot(
                    &api,
                    &id,
                    secret,
                    &base,
                    &namespace.endpoint,
                    install_docker,
                )
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

    /// User data, then power on. The VM gets its token, never the secret behind it.
    async fn boot(
        &self,
        api: &InstanceApi<'_>,
        id: &str,
        secret: &str,
        base: &BTreeMap<String, String>,
        registry_endpoint: &str,
        install_docker: bool,
    ) -> Result<Attempt> {
        let mut env = base.clone();
        env.insert(cloud_init::RUN_ID.to_string(), id.to_string());
        env.insert(
            cloud_init::VM_TOKEN.to_string(),
            ciburst_core::vm::token(secret, id),
        );
        let user_data = cloud_init::render(&env, registry_endpoint, install_docker)?;
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
    async fn remove_server(&self, api: &InstanceApi<'_>, server: &Value) -> Result<bool> {
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
    async fn servers(&self, project_id: &str) -> Result<Vec<Value>> {
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

    async fn job_runs(&self, project_id: &str, tags: &[&str]) -> Result<Vec<Run>> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }
        let definitions = jobs::list_definitions(&self.client, project_id).await?;
        let mut runs = Vec::new();
        for tag in tags {
            let name = self.definition_name(tag);
            let Some(definition_id) = definitions
                .iter()
                .find(|d| d["name"].as_str() == Some(name.as_str()))
                .and_then(|d| d["id"].as_str())
            else {
                continue;
            };
            for run in jobs::list_runs(&self.client, project_id, definition_id).await? {
                runs.push(Run {
                    run_id: run["id"].as_str().unwrap_or_default().to_string(),
                    pool: tag.to_string(),
                    state: jobs::run_state(run["state"].as_str().unwrap_or_default()),
                    created_at: run["created_at"].as_str().unwrap_or_default().to_string(),
                    labels: jobs::labels_from_env(&run["environment_variables"]),
                });
            }
        }
        Ok(runs)
    }

    /// Servers as runs, of one pool or of the whole deployment.
    async fn vm_runs(&self, project_id: &str, pool: Option<&str>) -> Result<Vec<Run>> {
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
impl Provider for ScalewayJobs {
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

    async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
        self.default_runtime()?;
        let mut runtimes = Vec::new();
        for pool in self.own_pools() {
            runtimes.push((pool, self.runtime(pool)?));
        }
        // Pools of other providers run VMs that pull the agent from this registry too.
        let other_pools = runtimes.len() < self.config.pools.len();
        let observed = self.observe().await?;
        let name = self.name();
        let mut steps = vec![Step::new(
            "project",
            if observed.project.is_some() {
                Action::Noop
            } else {
                Action::Create
            },
            format!("project {name}"),
        )];
        steps.push(self.key_step(KeyKind::Dispatch, &observed.dispatch, rotate));
        let bucket_resource = match &observed.project {
            Some(project_id) => format!("cache bucket {}", self.bucket(project_id)),
            None => format!("cache bucket {name}-cache"),
        };
        if self.cache_on() {
            steps.push(self.key_step(KeyKind::Cache, &observed.cache, rotate));
            let expire_days = self.config.cache.expire_days;
            let step = match observed.bucket {
                None => Step::new("bucket", Action::Create, bucket_resource),
                Some(Some(days)) if days == expire_days => {
                    Step::new("bucket", Action::Noop, bucket_resource)
                }
                Some(_) => Step::new("bucket", Action::Update, bucket_resource)
                    .with_detail(format!("lifecycle {expire_days} days")),
            };
            steps.push(step);
        } else {
            if observed.cache.is_some() {
                steps.push(Step::new(
                    "key:cache",
                    Action::Delete,
                    format!("api key {}", self.app_name(KeyKind::Cache)),
                ));
            }
            if observed.bucket.is_some() {
                steps.push(Step::new("bucket", Action::Delete, bucket_resource));
            }
        }
        let registry_resource = format!("registry namespace {name}");
        if self.registry_on() {
            let public = self.registry_public();
            let step = match &observed.registry {
                None => Step::new("registry", Action::Create, registry_resource),
                Some(namespace) if namespace.is_public == public => {
                    Step::new("registry", Action::Noop, registry_resource)
                }
                Some(_) => Step::new("registry", Action::Update, registry_resource)
                    .with_detail(if public { "public" } else { "private" }),
            };
            steps.push(step);
        } else if observed.registry.is_some() {
            steps.push(Step::new("registry", Action::Delete, registry_resource));
        }
        let group_resource = format!("security group {}", self.security_group_name());
        let own_vms = runtimes.iter().any(|(_, r)| *r == Runtime::Instances);
        if (own_vms || other_pools) && (!self.registry_on() || !self.registry_public()) {
            self.log.warn(
                "vm pools cannot start",
                &[(
                    "reason",
                    "VMs pull their agent from a public registry namespace: set \
                     provider.registry and provider.registryPublic",
                )],
            );
        }
        if own_vms {
            self.warn_without_quota(&runtimes).await;
            let action = if observed.security_group.is_some() {
                Action::Noop
            } else {
                Action::Create
            };
            steps.push(Step::new(SECURITY_GROUP_STEP, action, group_resource));
        } else if observed.security_group.is_some() {
            steps.push(Step::new(
                SECURITY_GROUP_STEP,
                Action::Delete,
                group_resource,
            ));
        }
        let region = self.client.region();
        for (pool, runtime) in &runtimes {
            if *runtime != Runtime::Jobs {
                continue;
            }
            let resource = format!("job definition {}", self.definition_name(&pool.tag));
            let id = format!("definition:{}", pool.tag);
            let current = observed
                .definitions
                .iter()
                .find(|d| self.definition_tag(d) == Some(pool.tag.as_str()));
            let step = match (current, &observed.project) {
                (Some(current), Some(project_id)) => {
                    let desired = jobs::desired_definition(&self.config, pool, region, project_id);
                    let changed = jobs::differences(&desired, current);
                    if changed.is_empty() {
                        Step::new(id, Action::Noop, resource)
                    } else {
                        Step::new(id, Action::Update, resource).with_detail(changed.join(", "))
                    }
                }
                _ => Step::new(id, Action::Create, resource),
            };
            steps.push(step);
        }
        for definition in &observed.definitions {
            let Some(tag) = self.definition_tag(definition) else {
                continue;
            };
            let kept = runtimes
                .iter()
                .any(|(pool, runtime)| pool.tag == tag && *runtime == Runtime::Jobs);
            if !kept {
                steps.push(Step::new(
                    format!("definition:{tag}"),
                    Action::Delete,
                    format!("job definition {}", self.definition_name(tag)),
                ));
            }
        }
        Ok(steps)
    }

    async fn apply(&self, steps: &[Step]) -> Result<Outputs> {
        let mut project = self.client.find_project(self.name()).await?;
        let mut outputs = Outputs::new();
        for step in steps {
            if step.is_noop() {
                continue;
            }
            match step.id.as_str() {
                "project" => {
                    if project.is_none() {
                        project = Some(self.create_project().await?);
                    }
                }
                "key:dispatch" | "key:cache" => {
                    let kind = if step.id == "key:dispatch" {
                        KeyKind::Dispatch
                    } else {
                        KeyKind::Cache
                    };
                    if step.action == Action::Delete {
                        self.delete_key(kind).await?;
                    } else {
                        let project_id = self.require_project(&project)?;
                        let pair = self.ensure_key(kind, project_id).await?;
                        let (access, secret) = kind.outputs();
                        outputs.insert(access.to_string(), pair.access_key);
                        outputs.insert(secret.to_string(), pair.secret_key);
                    }
                }
                "bucket" => self.apply_bucket(step.action, &project).await?,
                "registry" => {
                    if step.action == Action::Delete {
                        self.delete_registry(&project).await?;
                    } else {
                        let project_id = self.require_project(&project)?;
                        let namespace = self.ensure_registry(project_id).await?;
                        outputs.insert("CIBURST_REGISTRY_ENDPOINT".to_string(), namespace.endpoint);
                    }
                }
                SECURITY_GROUP_STEP => {
                    if step.action == Action::Delete {
                        if let Some(project_id) = &project {
                            self.delete_security_group(project_id).await?;
                        }
                    } else {
                        let project_id = self.require_project(&project)?;
                        self.ensure_security_group(project_id).await?;
                    }
                }
                id => {
                    let tag = id.strip_prefix("definition:").ok_or_else(|| {
                        Error::provider(format!("{PROVIDER_NAME}: unknown step {id}"))
                    })?;
                    match &project {
                        Some(project_id) => {
                            self.apply_definition(tag, step.action, project_id).await?
                        }
                        None if step.action == Action::Delete => {}
                        None => {
                            self.require_project(&project)?;
                        }
                    }
                }
            }
        }
        if let Some(project_id) = project {
            outputs.insert("CIBURST_PROJECT_ID".to_string(), project_id);
        }
        Ok(outputs)
    }

    async fn destroy(&self) -> Result<()> {
        let project = self.client.find_project(self.name()).await?;
        if let Some(project_id) = &project {
            let api = self.instances();
            for server in self.servers(project_id).await? {
                if self.remove_server(&api, &server).await? {
                    let id = server["id"].as_str().unwrap_or_default();
                    self.log.info("terminated server", &[("id", id)]);
                }
            }
            if let Err(error) = self.delete_security_group(project_id).await {
                self.log.warn(
                    "security group not deleted",
                    &[
                        ("name", &self.security_group_name()),
                        ("error", &error.to_string()),
                    ],
                );
            }
            for definition in jobs::list_definitions(&self.client, project_id).await? {
                if self.definition_tag(&definition).is_none() {
                    continue;
                }
                if let Some(id) = definition["id"].as_str() {
                    jobs::delete_definition(&self.client, id).await?;
                    let name = definition["name"].as_str().unwrap_or_default();
                    self.log
                        .info("deleted job definition", &[("name", name), ("id", id)]);
                }
            }
            if self.cache_on() || self.s3_access_key.is_some() {
                self.delete_bucket(project_id).await?;
            }
        }
        self.delete_registry(&project).await?;
        for kind in [KeyKind::Dispatch, KeyKind::Cache] {
            self.delete_key(kind).await?;
        }
        if let Some(project_id) = &project
            && self.client.default_project_id().is_none()
        {
            match self
                .client
                .delete(&format!("/account/v3/projects/{project_id}"))
                .await
            {
                Ok(_) => self.log.info(
                    "deleted project",
                    &[("name", self.name()), ("id", project_id)],
                ),
                Err(error) => self.log.warn(
                    "project not deleted",
                    &[
                        ("name", self.name()),
                        ("id", project_id),
                        ("error", &error.to_string()),
                    ],
                ),
            }
        }
        Ok(())
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
        let api = self.instances();
        if let Some(server) = api.get_server(run_id).await? {
            if !vm::is_deployment(&server, self.name()) {
                return Err(self.foreign(run_id));
            }
            if self.remove_server(&api, &server).await? {
                self.log.info("terminated server", &[("id", run_id)]);
            }
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
        let Some(project_id) = self.client.find_project(self.name()).await? else {
            return Ok(Some(Vec::new()));
        };
        Ok(Some(
            self.servers(&project_id)
                .await?
                .iter()
                .filter(|s| vm::tag_value(&vm::tags_of(s), vm::TAG_POOL) == Some(pool))
                .filter(|s| matches!(s["state"].as_str(), Some("starting" | "running")))
                .filter_map(vm::capacity)
                .collect(),
        ))
    }

    async fn claim(&self, run_id: &str) -> Result<()> {
        let api = self.instances();
        let server = self.own_server(&api, run_id).await?;
        api.set_tags(run_id, &vm::claimed(&vm::tags_of(&server)))
            .await
    }

    async fn report(&self, run_id: &str, report: &VmReport, now: u64) -> Result<()> {
        let api = self.instances();
        let server = self.own_server(&api, run_id).await?;
        api.set_tags(run_id, &vm::reported(&vm::tags_of(&server), report, now))
            .await
    }

    /// M4 rule R11: the agent image in the registry namespace, which VMs of every provider pull
    /// without credentials, and with an S3 cache the bucket's location. No secret.
    async fn shared_env(&self) -> Result<BTreeMap<String, String>> {
        let unusable = |reason: String| {
            Error::provider(format!("{PROVIDER_NAME}: shared environment: {reason}"))
        };
        if !self.registry_on() {
            return Err(unusable(
                "VMs pull their agent from the registry namespace; set provider.registry".into(),
            ));
        }
        let project_id = self
            .client
            .find_project(self.name())
            .await?
            .ok_or_else(|| unusable(format!("project {} does not exist", self.name())))?;
        let namespace = registry::find_namespace(&self.client, &project_id, self.name())
            .await?
            .ok_or_else(|| {
                unusable(format!("registry namespace {} does not exist", self.name()))
            })?;
        if !namespace.is_public {
            return Err(unusable(
                "VMs pull the agent image without credentials; set provider.registryPublic".into(),
            ));
        }
        let mut env = BTreeMap::from([(
            cloud_init::AGENT_IMAGE_KEY.to_string(),
            cloud_init::agent_image(&namespace.endpoint)?,
        )]);
        if self.cache_on() {
            env.extend(cloud_init::cache_location(
                &self.config,
                self.client.region(),
                &project_id,
            ));
        }
        Ok(env)
    }

    async fn sweep(&self, now: u64) -> Result<Vec<String>> {
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
