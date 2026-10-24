use std::sync::Arc;

use rand::Rng;
use rand::distr::Alphanumeric;

use crate::check::{Problem, check_jobs};
use crate::config::{Config, Pool};
use crate::gitlab::Gitlab;
use crate::log::Log;
use crate::plan::OutputsStore;
use crate::types::{Action, Host, KeyPair, Outputs, Provider, Rotation, Run, Secrets, Step};
use crate::{Error, Result};

/// Keys of `.local/outputs.env`.
pub mod keys {
    pub const HOOK_TOKEN: &str = "CIBURST_HOOK_TOKEN";
    pub const VM_SECRET: &str = "CIBURST_VM_SECRET";
    pub const SWEEP_TOKEN: &str = "CIBURST_SWEEP_TOKEN";
    pub const DISPATCH_URL: &str = "CIBURST_DISPATCH_URL";
    pub const DISPATCH_ACCESS_KEY: &str = "CIBURST_DISPATCH_ACCESS_KEY";
    pub const DISPATCH_SECRET_KEY: &str = "CIBURST_DISPATCH_SECRET_KEY";
    pub const CACHE_ACCESS_KEY: &str = "CIBURST_CACHE_ACCESS_KEY";
    pub const CACHE_SECRET_KEY: &str = "CIBURST_CACHE_SECRET_KEY";

    pub fn runner_id(tag: &str) -> String {
        format!("CIBURST_RUNNER_ID_{}", env_name(tag))
    }

    pub fn runner_token(tag: &str) -> String {
        format!("CIBURST_RUNNER_TOKEN_{}", env_name(tag))
    }

    fn env_name(tag: &str) -> String {
        tag.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect()
    }
}

pub fn secrets_from_outputs(config: &Config, outputs: &Outputs) -> Secrets {
    let runner_tokens = config
        .pools
        .iter()
        .filter_map(|pool| {
            outputs
                .get(&keys::runner_token(&pool.tag))
                .map(|token| (pool.tag.clone(), token.clone()))
        })
        .collect();
    let pair = |access: &str, secret: &str| {
        outputs.get(secret).map(|secret_key| KeyPair {
            access_key: outputs.get(access).cloned().unwrap_or_default(),
            secret_key: secret_key.clone(),
        })
    };
    Secrets {
        hook_token: outputs.get(keys::HOOK_TOKEN).cloned(),
        vm_secret: outputs.get(keys::VM_SECRET).cloned(),
        sweep_token: outputs.get(keys::SWEEP_TOKEN).cloned(),
        runner_tokens,
        dispatch: pair(keys::DISPATCH_ACCESS_KEY, keys::DISPATCH_SECRET_KEY),
        cache: pair(keys::CACHE_ACCESS_KEY, keys::CACHE_SECRET_KEY),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStatus {
    pub tag: String,
    pub live: usize,
    pub max_runs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub url: Option<String>,
    pub pools: Vec<PoolStatus>,
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectReport {
    pub project_id: u64,
    pub valid: bool,
    pub errors: Vec<String>,
    pub problems: Vec<Problem>,
}

/// Orchestrates provider, GitLab and host steps. GitLab is optional for plan and apply.
pub struct Provisioner {
    config: Arc<Config>,
    provider: Arc<dyn Provider>,
    host: Arc<dyn Host>,
    gitlab: Option<Arc<Gitlab>>,
    outputs: Arc<dyn OutputsStore>,
    log: Log,
}

impl Provisioner {
    pub fn new(
        config: Arc<Config>,
        provider: Arc<dyn Provider>,
        host: Arc<dyn Host>,
        gitlab: Option<Arc<Gitlab>>,
        outputs: Arc<dyn OutputsStore>,
        log: Log,
    ) -> Provisioner {
        Provisioner {
            config,
            provider,
            host,
            gitlab,
            outputs,
            log,
        }
    }

    fn runner_description(&self, pool: &Pool) -> String {
        format!("{}-{}", self.config.name, pool.tag)
    }

    fn primary_project(&self) -> u64 {
        self.config.gitlab.project_ids[0]
    }

    fn other_projects(&self) -> &[u64] {
        &self.config.gitlab.project_ids[1..]
    }

    async fn runner_steps(&self, current: &Outputs, rotate: bool) -> Result<Vec<Step>> {
        let Some(gitlab) = &self.gitlab else {
            return Ok(Vec::new());
        };
        let runners = gitlab.list_runners(self.primary_project()).await?;
        let mut steps = Vec::new();
        for pool in &self.config.pools {
            let description = self.runner_description(pool);
            let existing = runners.iter().any(|r| r.description == description);
            let has_token = current.contains_key(&keys::runner_token(&pool.tag));
            let mut step = Step::new(
                format!("runner:{}", pool.tag),
                Action::Create,
                format!("gitlab runner {description}"),
            );
            if existing && rotate {
                step.action = Action::Update;
                step.detail = Some("delete and recreate, new token".into());
            } else if existing && has_token {
                step.action = Action::Noop;
            } else if existing {
                step.action = Action::Update;
                step.detail = Some("token missing in outputs, delete and recreate".into());
            }
            steps.push(step);
        }
        for tag in self.orphan_runner_tags(gitlab, &runners).await? {
            steps.push(Step::new(
                format!("runner:{tag}"),
                Action::Delete,
                format!("gitlab runner {}-{tag}", self.config.name),
            ));
        }
        for &project_id in self.other_projects() {
            let enabled = gitlab.list_runners(project_id).await?;
            for pool in &self.config.pools {
                let description = self.runner_description(pool);
                let present = enabled.iter().any(|r| r.description == description);
                steps.push(Step::new(
                    format!("runner-enable:{project_id}:{}", pool.tag),
                    if present {
                        Action::Noop
                    } else {
                        Action::Create
                    },
                    format!("gitlab runner {description} enabled in project {project_id}"),
                ));
            }
        }
        Ok(steps)
    }

    /// Tags of runners this deployment created for pools gone from the config: described
    /// `<name>-<tag>` and carrying the tag `<tag>`, so another deployment's runners never match.
    async fn orphan_runner_tags(
        &self,
        gitlab: &Gitlab,
        runners: &[crate::gitlab::Runner],
    ) -> Result<Vec<String>> {
        let prefix = format!("{}-", self.config.name);
        let mut tags = Vec::new();
        for runner in runners {
            let Some(tag) = runner.description.strip_prefix(&prefix) else {
                continue;
            };
            if tag.is_empty() || self.config.pool_by_tag(tag).is_some() {
                continue;
            }
            let tagged = gitlab
                .list_runners_tagged(self.primary_project(), tag)
                .await?;
            if tagged.iter().any(|r| r.id == runner.id) {
                tags.push(tag.to_string());
            }
        }
        Ok(tags)
    }

    async fn hook_steps(&self, rotate: bool) -> Result<Vec<Step>> {
        let Some(gitlab) = &self.gitlab else {
            return Ok(Vec::new());
        };
        let url = self.host.url().await?;
        let mut steps = Vec::new();
        for &project_id in &self.config.gitlab.project_ids {
            let hooks = gitlab.list_hooks(project_id).await?;
            let existing = url
                .as_ref()
                .map(|u| hooks.iter().any(|h| &h.url == u))
                .unwrap_or(false);
            let mut step = Step::new(
                format!("hook:{project_id}"),
                Action::Create,
                format!("gitlab hook in project {project_id}"),
            );
            if url.is_none() {
                step.detail = Some("url known after the dispatcher is deployed".into());
            }
            if existing && rotate {
                step.action = Action::Update;
                step.detail = Some("new token".into());
            } else if existing {
                step.action = Action::Noop;
            }
            steps.push(step);
        }
        Ok(steps)
    }

    async fn apply_runners(&self, steps: &[Step]) -> Result<()> {
        let Some(gitlab) = &self.gitlab else {
            return Ok(());
        };
        let primary = self.primary_project();
        let runners = gitlab.list_runners(primary).await?;
        for step in steps.iter().filter(|s| s.action == Action::Delete) {
            let Some(tag) = step.id.strip_prefix("runner:") else {
                continue;
            };
            let description = format!("{}-{tag}", self.config.name);
            if let Some(runner) = runners.iter().find(|r| r.description == description) {
                gitlab.delete_runner(runner.id).await?;
                self.log.info(
                    "runner_deleted",
                    &[("pool", tag), ("id", &runner.id.to_string())],
                );
            }
        }
        let mut created = Outputs::new();
        for pool in &self.config.pools {
            let id = format!("runner:{}", pool.tag);
            let Some(step) = steps.iter().find(|s| s.id == id) else {
                continue;
            };
            if step.is_noop() {
                continue;
            }
            let description = self.runner_description(pool);
            if let Some(existing) = runners.iter().find(|r| r.description == description) {
                gitlab.delete_runner(existing.id).await?;
            }
            let runner = gitlab
                .create_runner(primary, &pool.tag, &description)
                .await?;
            created.insert(keys::runner_id(&pool.tag), runner.id.to_string());
            created.insert(keys::runner_token(&pool.tag), runner.token.clone());
            for &project_id in self.other_projects() {
                gitlab.enable_runner(project_id, runner.id).await?;
            }
            self.log.info(
                "runner",
                &[("pool", &pool.tag), ("id", &runner.id.to_string())],
            );
        }
        if !created.is_empty() {
            self.outputs.write(&created, true).await?;
        }
        let current = self.outputs.read().await?;
        for step in steps {
            let Some(rest) = step.id.strip_prefix("runner-enable:") else {
                continue;
            };
            if step.is_noop() {
                continue;
            }
            let Some((project, tag)) = rest.split_once(':') else {
                continue;
            };
            if created.contains_key(&keys::runner_id(tag)) {
                continue;
            }
            match current
                .get(&keys::runner_id(tag))
                .and_then(|id| id.parse::<u64>().ok())
            {
                Some(runner_id) => {
                    let project_id: u64 =
                        project.parse().map_err(|_| Error::other("bad step id"))?;
                    gitlab.enable_runner(project_id, runner_id).await?;
                }
                None => self.log.warn(
                    "runner_enable_skipped",
                    &[
                        ("project", project),
                        ("pool", tag),
                        ("reason", "runner id unknown"),
                    ],
                ),
            }
        }
        Ok(())
    }

    async fn apply_hooks(&self, steps: &[Step], hook_token: &str) -> Result<()> {
        let Some(gitlab) = &self.gitlab else {
            return Ok(());
        };
        let url = self
            .host
            .url()
            .await?
            .ok_or_else(|| Error::other("dispatcher url unknown, the host did not deploy"))?;
        for step in steps {
            if step.is_noop() {
                continue;
            }
            let project_id: u64 = step
                .id
                .strip_prefix("hook:")
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| Error::other("bad step id"))?;
            let hooks = gitlab.list_hooks(project_id).await?;
            match hooks.iter().find(|h| h.url == url) {
                Some(existing) => {
                    gitlab
                        .update_hook(project_id, existing.id, &url, hook_token)
                        .await?;
                }
                None => {
                    gitlab.create_hook(project_id, &url, hook_token).await?;
                }
            }
            self.log
                .info("hook", &[("project", &project_id.to_string())]);
        }
        Ok(())
    }

    /// A new VM secret would make every running VM's token invalid: its reports and its
    /// `/terminate` fail, and the sweep ends it once silent, even mid-job.
    async fn refuse_vm_rotation_while_running(&self) -> Result<()> {
        let mut running = 0;
        for pool in &self.config.pools {
            if let Some(vms) = self.provider.capacity(&pool.tag).await? {
                running += vms.len();
            }
        }
        if running > 0 {
            return Err(Error::other(format!(
                "{running} VMs are running; rotate the VM secret when none runs"
            )));
        }
        Ok(())
    }

    /// A random secret of 32 alphanumerics under `key`, written once and replaced on rotation.
    async fn ensure_secret(&self, key: &str, rotate: bool) -> Result<String> {
        let current = self.outputs.read().await?;
        if let Some(token) = current.get(key).filter(|_| !rotate) {
            return Ok(token.clone());
        }
        let token: String = rand::rng()
            .sample_iter(Alphanumeric)
            .take(32)
            .map(char::from)
            .collect();
        self.outputs
            .write(&Outputs::from([(key.to_string(), token.clone())]), true)
            .await?;
        Ok(token)
    }

    pub async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
        let current = self.outputs.read().await?;
        let secrets = secrets_from_outputs(&self.config, &current);
        let mut steps = self.provider.plan(&provider_rotations(rotate)).await?;
        steps.extend(
            self.runner_steps(&current, rotate.contains(&Rotation::Runner))
                .await?,
        );
        steps.extend(self.host.plan(&secrets).await?);
        steps.extend(self.hook_steps(rotate.contains(&Rotation::Hook)).await?);
        Ok(steps)
    }

    /// Applies in phases so that later phases see the outputs of earlier ones.
    pub async fn apply(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
        let mut applied = Vec::new();
        let rotate_hook = rotate.contains(&Rotation::Hook);
        let hook_token = self.ensure_secret(keys::HOOK_TOKEN, rotate_hook).await?;
        let sweep_token = self.ensure_secret(keys::SWEEP_TOKEN, rotate_hook).await?;
        let rotate_vm = rotate.contains(&Rotation::Vm);
        if rotate_vm {
            self.refuse_vm_rotation_while_running().await?;
        }
        let vm_secret = self.ensure_secret(keys::VM_SECRET, rotate_vm).await?;

        let provider_rotate = provider_rotations(rotate);
        let provider_steps = self.provider.plan(&provider_rotate).await?;
        // One step at a time: a secret the provider shows once is saved before a later step
        // can fail. Providers return secrets only when they created them, so they overwrite.
        for step in &provider_steps {
            if step.is_noop() {
                continue;
            }
            let outputs = self.provider.apply(std::slice::from_ref(step)).await?;
            self.outputs.write(&outputs, true).await?;
        }
        let tail = self.provider.apply(&[]).await?;
        self.outputs.write(&tail, false).await?;
        applied.extend(provider_steps);

        let current = self.outputs.read().await?;
        let runner_steps = self
            .runner_steps(&current, rotate.contains(&Rotation::Runner))
            .await?;
        self.apply_runners(&runner_steps).await?;
        applied.extend(runner_steps);

        let mut secrets = secrets_from_outputs(&self.config, &self.outputs.read().await?);
        secrets.hook_token = Some(hook_token.clone());
        secrets.vm_secret = Some(vm_secret);
        secrets.sweep_token = Some(sweep_token);
        let host_steps = self.host.plan(&secrets).await?;
        let host_outputs = self.host.apply(&host_steps, &secrets).await?;
        self.outputs.write(&host_outputs, true).await?;
        applied.extend(host_steps);

        let hook_steps = self.hook_steps(rotate.contains(&Rotation::Hook)).await?;
        self.apply_hooks(&hook_steps, &hook_token).await?;
        applied.extend(hook_steps);
        Ok(applied)
    }

    pub async fn destroy(&self) -> Result<()> {
        let current = self.outputs.read().await?;
        if let Some(gitlab) = &self.gitlab {
            let url = self.host.url().await?;
            for &project_id in &self.config.gitlab.project_ids {
                for hook in gitlab.list_hooks(project_id).await? {
                    if url.as_deref() == Some(hook.url.as_str()) {
                        gitlab.delete_hook(project_id, hook.id).await?;
                    }
                }
            }
            let runners = gitlab.list_runners(self.primary_project()).await?;
            for pool in &self.config.pools {
                let description = self.runner_description(pool);
                let id = runners
                    .iter()
                    .find(|r| r.description == description)
                    .map(|r| r.id)
                    .or_else(|| {
                        current
                            .get(&keys::runner_id(&pool.tag))
                            .and_then(|id| id.parse().ok())
                    });
                if let Some(id) = id {
                    gitlab.delete_runner(id).await?;
                }
            }
        }
        self.host.destroy().await?;
        self.provider.destroy().await?;
        self.outputs.clear().await?;
        self.log.info("destroyed", &[("name", &self.config.name)]);
        Ok(())
    }

    pub async fn status(&self) -> Result<Status> {
        let runs = self.provider.list(None).await?;
        let pools = self
            .config
            .pools
            .iter()
            .map(|pool| PoolStatus {
                tag: pool.tag.clone(),
                live: runs
                    .iter()
                    .filter(|run| run.pool == pool.tag && run.state.is_live())
                    .count(),
                max_runs: pool.max_runs,
            })
            .collect();
        Ok(Status {
            url: self.host.url().await?,
            pools,
            runs,
        })
    }

    pub async fn check(&self) -> Result<Vec<ProjectReport>> {
        let gitlab = self
            .gitlab
            .as_ref()
            .ok_or_else(|| Error::other("check needs a GitLab token"))?;
        let mut report = Vec::new();
        for &project_id in &self.config.gitlab.project_ids {
            let lint = gitlab.lint_jobs(project_id).await?;
            let problems = if lint.valid {
                check_jobs(&self.config, &lint.jobs)
            } else {
                Vec::new()
            };
            report.push(ProjectReport {
                project_id,
                valid: lint.valid,
                errors: lint.errors,
                problems,
            });
        }
        Ok(report)
    }
}

fn provider_rotations(rotate: &[Rotation]) -> Vec<Rotation> {
    rotate
        .iter()
        .copied()
        .filter(|r| matches!(r, Rotation::Key | Rotation::Cache))
        .collect()
}
