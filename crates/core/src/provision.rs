use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;
use rand::distr::Alphanumeric;
use serde_json::{Value, json};

use crate::check::{Problem, Suggestion};
#[cfg(feature = "pipeline")]
use crate::check::{check_jobs, suggest_lanes};
use crate::config::{CacheType, Config, Pool};
use crate::fake::Clock;
use crate::gitlab::{Gitlab, Job};
use crate::http::{HttpClient, Request};
use crate::log::Log;
#[cfg(feature = "pipeline")]
use crate::pipeline::Pipeline;
use crate::plan::OutputsStore;
use crate::time::parse_rfc3339;
use crate::types::{Action, Host, KeyPair, Outputs, Provider, Rotation, Run, Secrets, Step};
use crate::{Error, Result};

/// Pending jobs younger than this still have their own webhook on the way (spec D41).
pub const REPLAY_MIN_AGE_S: u64 = 60;

/// Detail of a runner step that only replaces the runner's tags (D47).
const RETAG: &str = "set tags ";

/// Output keys: `.local/outputs.env`, or project variables in pipeline mode.
pub mod keys {
    use crate::config::Config;

    pub const HOOK_TOKEN: &str = "CIBURST_HOOK_TOKEN";
    pub const VM_SECRET: &str = "CIBURST_VM_SECRET";
    pub const SWEEP_TOKEN: &str = "CIBURST_SWEEP_TOKEN";
    pub const DISPATCH_URL: &str = "CIBURST_DISPATCH_URL";
    pub const DISPATCH_ACCESS_KEY: &str = "CIBURST_DISPATCH_ACCESS_KEY";
    pub const DISPATCH_SECRET_KEY: &str = "CIBURST_DISPATCH_SECRET_KEY";
    pub const CACHE_ACCESS_KEY: &str = "CIBURST_CACHE_ACCESS_KEY";
    pub const CACHE_SECRET_KEY: &str = "CIBURST_CACHE_SECRET_KEY";
    /// Outputs of added providers that the dispatcher needs as secret environment variables.
    pub const PROVIDER_ENV: [&str; 1] = ["HCLOUD_TOKEN"];
    /// SHA-256 of the uploaded VM agent (M5 rule R3); plain dispatcher environment.
    pub const AGENT_SHA256: &str = "CIBURST_AGENT_SHA256";
    /// Outputs the dispatcher gets as plain environment variables.
    pub const DISPATCHER_ENV: [&str; 1] = [AGENT_SHA256];
    pub const PROJECT_ID: &str = "CIBURST_PROJECT_ID";
    pub const REGISTRY_ENDPOINT: &str = "CIBURST_REGISTRY_ENDPOINT";

    /// Output keys that do not depend on the pools. `PROVIDER_ENV` is not among them: in
    /// pipeline mode its values never leave memory.
    pub const FIXED: [&str; 11] = [
        HOOK_TOKEN,
        VM_SECRET,
        SWEEP_TOKEN,
        DISPATCH_URL,
        DISPATCH_ACCESS_KEY,
        DISPATCH_SECRET_KEY,
        CACHE_ACCESS_KEY,
        CACHE_SECRET_KEY,
        PROJECT_ID,
        REGISTRY_ENDPOINT,
        AGENT_SHA256,
    ];

    /// Outputs that grant access; only these are stored masked and hidden. Ids, URLs and the
    /// agent hash stay readable in job logs.
    pub fn is_secret(key: &str) -> bool {
        [
            HOOK_TOKEN,
            VM_SECRET,
            SWEEP_TOKEN,
            DISPATCH_SECRET_KEY,
            CACHE_SECRET_KEY,
        ]
        .contains(&key)
            || PROVIDER_ENV.contains(&key)
            || key.starts_with("CIBURST_RUNNER_TOKEN_")
    }

    /// Keys a provider answers from every `apply` call; they replace stored values.
    pub fn is_refreshed(key: &str) -> bool {
        PROVIDER_ENV.contains(&key) || DISPATCHER_ENV.contains(&key)
    }

    /// Every output key of a deployment of `config`: the fixed ones and the runner keys of its
    /// pools.
    pub fn output_keys(config: &Config) -> Vec<String> {
        FIXED
            .iter()
            .map(|key| key.to_string())
            .chain(
                config
                    .pools
                    .iter()
                    .flat_map(|pool| [runner_id(&pool.tag), runner_token(&pool.tag)]),
            )
            .collect()
    }

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
        provider_env: keys::PROVIDER_ENV
            .iter()
            .filter_map(|key| outputs.get(*key).map(|v| (key.to_string(), v.clone())))
            .collect(),
        dispatcher_env: keys::DISPATCHER_ENV
            .iter()
            .filter_map(|key| outputs.get(*key).map(|v| (key.to_string(), v.clone())))
            .collect(),
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
    /// Lanes for the Docker-free jobs on VM pools (D46).
    pub suggestions: Vec<Suggestion>,
}

/// Orchestrates provider, GitLab and host steps. GitLab is optional for plan and apply.
pub struct Provisioner {
    config: Arc<Config>,
    provider: Arc<dyn Provider>,
    host: Arc<dyn Host>,
    gitlab: Option<Arc<Gitlab>>,
    outputs: Arc<dyn OutputsStore>,
    log: Log,
    replay: Option<Arc<dyn HttpClient>>,
    clock: Clock,
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
            replay: None,
            clock: Arc::new(now_seconds),
        }
    }

    /// The client `apply` posts waiting jobs to the dispatcher with (M5 rule R4); without it
    /// nothing is replayed.
    pub fn with_replay(mut self, http: Arc<dyn HttpClient>) -> Provisioner {
        self.replay = Some(http);
        self
    }

    /// Replaces the system clock, which dates pending jobs for the replay.
    pub fn with_clock(mut self, clock: Clock) -> Provisioner {
        self.clock = clock;
        self
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
            let existing = runners.iter().find(|r| r.description == description);
            let has_token = current.contains_key(&keys::runner_token(&pool.tag));
            let mut step = Step::new(
                format!("runner:{}", pool.tag),
                Action::Create,
                format!("gitlab runner {description}"),
            );
            if existing.is_some() && rotate {
                step.action = Action::Update;
                step.detail = Some("delete and recreate, new token".into());
            } else if let Some(runner) = existing
                && has_token
            {
                let wanted = self.config.runner_tags(pool);
                let tagged = gitlab.runner(runner.id).await?.tag_list;
                let same =
                    wanted.len() == tagged.len() && wanted.iter().all(|t| tagged.contains(t));
                if same {
                    step.action = Action::Noop;
                } else {
                    step.action = Action::Update;
                    step.detail = Some(format!("{RETAG}{}", wanted.join(", ")));
                }
            } else if existing.is_some() {
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
            let tags = self.config.runner_tags(pool);
            let existing = runners.iter().find(|r| r.description == description);
            if let Some(existing) = existing
                && step.detail.as_deref().is_some_and(|d| d.starts_with(RETAG))
            {
                gitlab.update_runner_tags(existing.id, &tags).await?;
                self.log.info(
                    "runner_tags",
                    &[
                        ("pool", &pool.tag),
                        ("id", &existing.id.to_string()),
                        ("tags", &tags.join(",")),
                    ],
                );
                continue;
            }
            if let Some(existing) = existing {
                gitlab.delete_runner(existing.id).await?;
            }
            let runner = gitlab.create_runner(primary, &tags, &description).await?;
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

    /// `rotate` plus the rotation of every output secret the config needs that `current`
    /// lacks (M5 rule R7): a key that exists only at the provider would otherwise stay lost
    /// behind a `noop`. A pool's missing runner token recreates that runner (`runner_steps`).
    fn with_missing_secrets(&self, rotate: &[Rotation], current: &Outputs) -> Vec<Rotation> {
        let mut all = rotate.to_vec();
        let pair = |access: &str, secret: &str| {
            current.contains_key(access) && current.contains_key(secret)
        };
        let mut needed = Vec::new();
        if !pair(keys::DISPATCH_ACCESS_KEY, keys::DISPATCH_SECRET_KEY) {
            needed.push(Rotation::Key);
        }
        if self.config.cache.kind == CacheType::S3
            && !pair(keys::CACHE_ACCESS_KEY, keys::CACHE_SECRET_KEY)
        {
            needed.push(Rotation::Cache);
        }
        for rotation in needed {
            if !all.contains(&rotation) {
                all.push(rotation);
            }
        }
        all
    }

    pub async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
        let current = self.outputs.read().await?;
        let rotate = self.with_missing_secrets(rotate, &current);
        let rotate = rotate.as_slice();
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

    /// Applies in phases so that later phases see the outputs of earlier ones. Nothing changes
    /// when the provider's preflight fails (M5 rule R6); waiting jobs are replayed last.
    pub async fn apply(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
        self.provider.preflight().await?;
        let rotate = self.with_missing_secrets(rotate, &self.outputs.read().await?);
        let rotate = rotate.as_slice();
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
        // The tail call keeps secrets shown once; values the provider derives on every call
        // (agent digest, provider credentials) replace the stored ones.
        let tail = self.provider.apply(&[]).await?;
        let (fresh, kept): (Outputs, Outputs) = tail
            .into_iter()
            .partition(|(key, _)| keys::is_refreshed(key));
        self.outputs.write(&fresh, true).await?;
        self.outputs.write(&kept, false).await?;
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
        self.replay(&hook_token).await;
        Ok(applied)
    }

    /// Posts every pending job that waits for a pool tag longer than [`REPLAY_MIN_AGE_S`] to the
    /// dispatcher as GitLab would (M5 rule R4, spec D41), so jobs that queued before the setup
    /// start. Failures are logged; they never fail `apply`.
    async fn replay(&self, hook_token: &str) {
        let (Some(gitlab), Some(http)) = (&self.gitlab, &self.replay) else {
            return;
        };
        let url = match self.host.url().await {
            Ok(Some(url)) if !hook_token.is_empty() => url,
            Ok(_) => return,
            Err(error) => {
                self.log
                    .warn("replay_failed", &[("error", &error.to_string())]);
                return;
            }
        };
        let now = (self.clock)();
        let tags: BTreeSet<&str> = self.config.pools.iter().map(|p| p.tag.as_str()).collect();
        for &project_id in &self.config.gitlab.project_ids {
            let project = project_id.to_string();
            let jobs = match gitlab.list_pending_jobs(project_id).await {
                Ok(jobs) => jobs,
                Err(error) => {
                    self.log.warn(
                        "replay_failed",
                        &[("project", &project), ("error", &error.to_string())],
                    );
                    continue;
                }
            };
            let mut waiting: Vec<Job> = jobs
                .into_iter()
                .filter(|job| job.tag_list.iter().any(|t| tags.contains(t.as_str())))
                .filter(|job| {
                    parse_rfc3339(&job.created_at)
                        .is_some_and(|created| now.saturating_sub(created) > REPLAY_MIN_AGE_S)
                })
                .collect();
            waiting.sort_by_key(|job| job.id);
            for job in waiting {
                let body = json!({
                    "object_kind": "build",
                    "build_status": "pending",
                    "project_id": project_id,
                    "build_id": job.id,
                    "build_name": job.name,
                    "build_stage": job.stage,
                    "pipeline_id": job.pipeline.as_ref().map(|p| p.id),
                });
                let request = Request::new("POST", url.clone())
                    .header("X-Gitlab-Event", "Job Hook")
                    .header("X-Gitlab-Token", hook_token)
                    .json(&body);
                let job_id = job.id.to_string();
                match http.send(request).await {
                    Ok(response) if response.ok() => {
                        let result = response
                            .json::<Value>()
                            .ok()
                            .and_then(|v| v.get("result").and_then(Value::as_str).map(String::from))
                            .unwrap_or_else(|| response.status.to_string());
                        self.log.info(
                            "replayed",
                            &[("project", &project), ("job", &job_id), ("result", &result)],
                        );
                    }
                    Ok(response) => self.log.warn(
                        "replay_failed",
                        &[
                            ("project", &project),
                            ("job", &job_id),
                            ("status", &response.status.to_string()),
                        ],
                    ),
                    Err(error) => self.log.warn(
                        "replay_failed",
                        &[
                            ("project", &project),
                            ("job", &job_id),
                            ("error", &error.to_string()),
                        ],
                    ),
                }
            }
        }
    }

    pub async fn destroy(&self) -> Result<()> {
        let current = self.outputs.read().await?;
        if let Some(gitlab) = &self.gitlab {
            let url = match self.host.url().await? {
                Some(url) => Some(url),
                None => current.get(keys::DISPATCH_URL).cloned(),
            };
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

    /// See [`check`].
    #[cfg(feature = "pipeline")]
    pub async fn check(&self, content_ref: Option<&str>) -> Result<Vec<ProjectReport>> {
        let gitlab = self
            .gitlab
            .as_ref()
            .ok_or_else(|| Error::other("check needs a GitLab token"))?;
        check(&self.config, gitlab, content_ref).await
    }
}

/// Lints every project's pipeline at `content_ref` (the default branch when `None`), compares
/// its jobs with the routing of the config and proposes lanes (D46). Needs only the config and
/// GitLab.
#[cfg(feature = "pipeline")]
pub async fn check(
    config: &Config,
    gitlab: &Gitlab,
    content_ref: Option<&str>,
) -> Result<Vec<ProjectReport>> {
    let mut report = Vec::new();
    for &project_id in &config.gitlab.project_ids {
        let lint = gitlab.lint_jobs(project_id, content_ref).await?;
        let (problems, suggestions) = if lint.valid {
            let pipeline = match &lint.merged_yaml {
                Some(text) => crate::pipeline::parse_merged_yaml(text)
                    .map_err(|error| Error::other(format!("project {project_id}: {error}")))?,
                None => Pipeline::default(),
            };
            (
                check_jobs(config, &lint.jobs, &pipeline),
                suggest_lanes(config, &lint.jobs, &pipeline),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        report.push(ProjectReport {
            project_id,
            valid: lint.valid,
            errors: lint.errors,
            problems,
            suggestions,
        });
    }
    Ok(report)
}

fn provider_rotations(rotate: &[Rotation]) -> Vec<Rotation> {
    rotate
        .iter()
        .copied()
        .filter(|r| matches!(r, Rotation::Key | Rotation::Cache))
        .collect()
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gitlab_timestamps() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-10-25T12:00:00.123Z"),
            Some(1_792_929_600)
        );
        assert_eq!(
            parse_rfc3339("2026-10-25T14:00:00+02:00"),
            Some(1_792_929_600)
        );
        assert_eq!(parse_rfc3339("2000-03-01T00:00:00Z"), Some(951_868_800));
        for bad in [
            "",
            "2026-10-25",
            "2026-13-25T12:00:00Z",
            "2026-10-25T12:00:00",
            "x",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn output_keys_cover_the_pools_and_leave_provider_env_out() {
        let config = Config::ci_defaults(5, "https://gitlab.example");
        let all = keys::output_keys(&config);
        assert_eq!(all.len(), keys::FIXED.len() + 2);
        assert!(all.contains(&keys::runner_token("vm")));
        assert!(all.contains(&keys::runner_id("vm")));
        assert!(all.contains(&keys::AGENT_SHA256.to_string()));
        assert!(!all.iter().any(|k| keys::PROVIDER_ENV.contains(&k.as_str())));
        assert!(keys::is_refreshed("HCLOUD_TOKEN"));
        assert!(keys::is_refreshed(keys::AGENT_SHA256));
        assert!(!keys::is_refreshed(keys::DISPATCH_SECRET_KEY));
    }
}
