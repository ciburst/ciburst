//! Plan, apply, preflight and destroy: the steps `project`, `key:dispatch`, `key:cache`,
//! `bucket`, `runner-binary`, `registry`, `vm:agent`, `vm:security-group` and
//! `definition:<tag>`, each found by name before it is planned.

use std::time::Duration;

use ciburst_core::provision::keys;
use ciburst_core::types::{Action, Outputs, Rotation, Step};
use ciburst_core::{Error, Result};
use serde_json::Value;

use super::agent::{AGENT_STEP, NO_AGENT, NO_CACHE};
use super::iam::{self, AppState};
use super::keys::KeyKind;
use super::registry::{self, NamespaceState};
use super::runner::RUNNER_STEP;
use super::s3::LIFECYCLE_RULES;
use super::servers::SECURITY_GROUP_STEP;
use super::{PROVIDER_NAME, Runtime, Scaleway, jobs};

/// How long `destroy` waits for terminated servers to disappear, and how often it looks.
const TEARDOWN_WAIT: Duration = Duration::from_secs(300);
const TEARDOWN_POLL: Duration = Duration::from_secs(5);

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

impl Scaleway {
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
                let s3 = self.s3(project_id)?;
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

    pub(super) async fn plan_steps(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
        let runtimes = self.own_runtimes()?;
        let needs_agent = self.needs_agent()?;
        if !self.cache_on() {
            for tag in self.vm_pools()? {
                self.log.warn(
                    "vm pools cannot start",
                    &[("pool", &tag), ("reason", NO_CACHE)],
                );
            }
        }
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
                Some(_) => {
                    let prefixes: Vec<&str> = LIFECYCLE_RULES.iter().map(|(_, p)| *p).collect();
                    Step::new("bucket", Action::Update, bucket_resource).with_detail(format!(
                        "lifecycle {expire_days} days on {}",
                        prefixes.join(", ")
                    ))
                }
            };
            steps.push(step);
            if self.needs_runner() {
                steps.push(
                    self.runner_step(&observed.project, observed.bucket.is_some())
                        .await?,
                );
            }
        } else {
            if observed.cache.is_some() {
                steps.push(Step::new(
                    KeyKind::Cache.step_id(),
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
        if needs_agent && self.cache_on() {
            steps.push(
                self.agent_step(&observed.project, observed.bucket.is_some())
                    .await?,
            );
        }
        let group_resource = format!("security group {}", self.security_group_name());
        let own_vms = runtimes.iter().any(|(_, r)| *r == Runtime::Instances);
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
                    let desired = jobs::desired_definition(
                        &self.config,
                        pool,
                        region,
                        project_id,
                        self.runner_sha256(),
                    );
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
            // A member's lanes follow its `/join` (M6.4 rule R5), not this config.
            let kept = self.is_member_lane(tag)
                || runtimes
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

    /// D42: what would keep a VM pool from starting after `apply`, as one error.
    pub(super) async fn preflight_checks(&self) -> Result<()> {
        let runtimes = self.own_runtimes()?;
        let mut problems: Vec<String> = self
            .quota_problems(&runtimes)
            .await
            .into_iter()
            .map(|(tag, reason)| format!("pool {tag}: {reason}"))
            .collect();
        if !self.cache_on() {
            for tag in self.vm_pools()? {
                problems.push(format!("pool {tag}: {NO_CACHE}"));
            }
        } else if self.needs_agent()? && !self.agent_available().await? {
            problems.push(NO_AGENT.to_string());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::provider(format!(
                "{PROVIDER_NAME}: {}",
                problems.join("; ")
            )))
        }
    }

    /// Applies `steps` in order. Every call with a VM pool answers the current agent as
    /// `CIBURST_AGENT_SHA256`, so the dispatcher's environment follows it (D38).
    pub(super) async fn apply_steps(&self, steps: &[Step]) -> Result<Outputs> {
        let mut project = self.client.find_project(self.name()).await?;
        let mut outputs = Outputs::new();
        let mut agent = None;
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
                    let kind = if step.id == KeyKind::Dispatch.step_id() {
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
                RUNNER_STEP => self.apply_runner(&project).await?,
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
                AGENT_STEP => agent = Some(self.apply_agent(&project).await?),
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
        if self.cache_on() && self.needs_agent()? {
            if agent.is_none() {
                agent = self.current_agent(&project).await?;
            }
            if let Some(sha256) = agent {
                outputs.insert(keys::AGENT_SHA256.to_string(), sha256);
            }
        }
        if let Some(project_id) = project {
            outputs.insert("CIBURST_PROJECT_ID".to_string(), project_id);
        }
        Ok(outputs)
    }

    /// Ends every server of the deployment and waits until Scaleway has removed them: a
    /// terminated server stays `stopping` for a while and holds the security group meanwhile.
    async fn end_servers(&self, project_id: &str) -> Result<()> {
        let api = self.instances();
        let mut waited = Duration::ZERO;
        loop {
            for server in self.servers(project_id).await? {
                if self.remove_server(&api, &server).await? {
                    let id = server["id"].as_str().unwrap_or_default();
                    self.log.info("terminated server", &[("id", id)]);
                }
            }
            let left = self.servers(project_id).await?.len();
            if left == 0 {
                return Ok(());
            }
            if waited >= TEARDOWN_WAIT {
                self.log
                    .warn("servers still ending", &[("count", &left.to_string())]);
                return Ok(());
            }
            tokio::time::sleep(TEARDOWN_POLL).await;
            waited += TEARDOWN_POLL;
        }
    }

    pub(super) async fn destroy_all(&self) -> Result<()> {
        let project = self.client.find_project(self.name()).await?;
        if let Some(project_id) = &project {
            self.end_servers(project_id).await?;
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
}
