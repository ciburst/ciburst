//! Provider `scaleway-jobs`: one Serverless Jobs definition per pool, started per pending job.
//! Resources: project, IAM applications and keys, cache bucket with lifecycle, job definitions.
//! Every resource is looked up by name; nothing is stored locally.

mod iam;
mod jobs;
pub mod s3;
pub mod sigv4;

use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::{CacheType, Config};
use ciburst_core::log::Log;
use ciburst_core::types::{
    Action, Billing, Capabilities, KeyPair, Outputs, Provider, Rotation, Run, StartRequest, Step,
};
use ciburst_core::{Error, Result};
use serde_json::{Value, json};

use crate::client::ScalewayClient;
use iam::AppState;

pub const PROVIDER_NAME: &str = "scaleway-jobs";

const PAGE_SIZE: usize = 100;

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

    fn permission_set(self) -> &'static str {
        match self {
            KeyKind::Dispatch => "ServerlessJobsFullAccess",
            KeyKind::Cache => "ObjectStorageFullAccess",
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
    definitions: Vec<Value>,
}

pub struct ScalewayJobs {
    config: Arc<Config>,
    client: Arc<ScalewayClient>,
    log: Log,
    s3_access_key: Option<String>,
}

impl ScalewayJobs {
    pub fn new(config: Arc<Config>, client: Arc<ScalewayClient>, log: Log) -> ScalewayJobs {
        ScalewayJobs {
            config,
            client,
            log,
            s3_access_key: None,
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

    fn cache_on(&self) -> bool {
        self.config.cache.kind == CacheType::S3
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
        let mut definitions = Vec::new();
        if let Some(project_id) = &project {
            if self.cache_on() || self.s3_access_key.is_some() {
                let s3 = self.s3()?;
                let name = self.bucket(project_id);
                if s3.bucket_exists(&name).await? {
                    bucket = Some(s3.lifecycle_days(&name).await?);
                }
            }
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
            definitions,
        })
    }

    fn key_step(&self, kind: KeyKind, app: &Option<AppState>, rotate: &[Rotation]) -> Step {
        let resource = format!("api key {}", self.app_name(kind));
        match app {
            None => Step::new(kind.step_id(), Action::Create, resource),
            Some(app) if !app.complete() => {
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
        if app.policies.is_empty() {
            let id = iam::create_policy(
                &self.client,
                &name,
                &app.id,
                kind.permission_set(),
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
        Capabilities {
            name: PROVIDER_NAME.into(),
            docker_daemon: false,
            max_vcpu: 6,
            max_memory_mib: 16384,
            max_storage_mib: 10240,
            max_duration_s: 86400,
            archs: vec!["amd64".into()],
            billing: Billing::PerSecond,
        }
    }

    async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
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
        let region = self.client.region();
        for pool in &self.config.pools {
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
            if let Some(tag) = self.definition_tag(definition)
                && self.config.pool_by_tag(tag).is_none()
            {
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
        Ok(outputs)
    }

    async fn destroy(&self) -> Result<()> {
        let project = self.client.find_project(self.name()).await?;
        if let Some(project_id) = &project {
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
        if request.env.get("RUNNER_TOKEN").is_none_or(|t| t.is_empty()) {
            return Err(Error::provider(format!(
                "{PROVIDER_NAME}: RUNNER_TOKEN missing"
            )));
        }
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

    async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
        let Some(project_id) = self.client.find_project(self.name()).await? else {
            return Ok(Vec::new());
        };
        let definitions = jobs::list_definitions(&self.client, &project_id).await?;
        let tags: Vec<&str> = match pool {
            Some(tag) => vec![tag],
            None => self.config.pools.iter().map(|p| p.tag.as_str()).collect(),
        };
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
            for run in jobs::list_runs(&self.client, &project_id, definition_id).await? {
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

    async fn stop(&self, run_id: &str) -> Result<()> {
        jobs::stop(&self.client, run_id).await
    }
}
