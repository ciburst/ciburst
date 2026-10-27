use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;
use rand::distr::Alphanumeric;
use serde_json::{Value, json};

use crate::check::{Problem, Suggestion};
#[cfg(feature = "pipeline")]
use crate::check::{check_jobs, suggest_ignore_jobs, suggest_lanes};
use crate::config::{CacheType, Config, ProjectConfig};
use crate::fake::Clock;
use crate::fleet::{FleetStore, Member};
use crate::gitlab::{DELIVERIES, Delivery, Gitlab, Hook, Job, REENABLE_TRIGGER};
use crate::http::{HttpClient, Request};
use crate::log::Log;
#[cfg(feature = "pipeline")]
use crate::pipeline::Pipeline;
use crate::plan::OutputsStore;
use crate::time::parse_rfc3339;
use crate::types::{Action, Host, KeyPair, Outputs, Provider, Rotation, Run, Secrets, Step};
use crate::{Error, Result};

/// Pending jobs younger than this still have their own webhook on the way (D41).
pub const REPLAY_MIN_AGE_S: u64 = 60;

/// Detail of a runner step that only replaces the runner's tags (D47).
const RETAG: &str = "set tags ";

/// The description suffix of the spill runner of earlier deployments (D54), which `apply` and
/// `destroy` delete (D62).
const SPILL_SUFFIX: &str = "-spill";

/// Output keys: `.local/outputs.env`, or project variables in pipeline mode.
pub mod keys {
    use crate::config::Config;

    pub const HOOK_TOKEN: &str = "CIBURST_HOOK_TOKEN";
    pub const VM_SECRET: &str = "CIBURST_VM_SECRET";
    pub const SWEEP_TOKEN: &str = "CIBURST_SWEEP_TOKEN";
    /// Seals the fleet state in the bucket (spec 8.1); written once, never rotated.
    pub const FLEET_KEY: &str = "CIBURST_FLEET_KEY";
    pub const DISPATCH_URL: &str = "CIBURST_DISPATCH_URL";
    pub const DISPATCH_ACCESS_KEY: &str = "CIBURST_DISPATCH_ACCESS_KEY";
    pub const DISPATCH_SECRET_KEY: &str = "CIBURST_DISPATCH_SECRET_KEY";
    pub const CACHE_ACCESS_KEY: &str = "CIBURST_CACHE_ACCESS_KEY";
    pub const CACHE_SECRET_KEY: &str = "CIBURST_CACHE_SECRET_KEY";
    /// SHA-256 of the uploaded VM agent (D38); plain dispatcher environment.
    pub const AGENT_SHA256: &str = "CIBURST_AGENT_SHA256";
    /// Outputs the dispatcher gets as plain environment variables.
    pub const DISPATCHER_ENV: [&str; 1] = [AGENT_SHA256];
    pub const PROJECT_ID: &str = "CIBURST_PROJECT_ID";
    pub const REGISTRY_ENDPOINT: &str = "CIBURST_REGISTRY_ENDPOINT";

    /// Output keys that do not depend on the VM runner and the lanes.
    pub const FIXED: [&str; 12] = [
        HOOK_TOKEN,
        VM_SECRET,
        FLEET_KEY,
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
            FLEET_KEY,
            SWEEP_TOKEN,
            DISPATCH_SECRET_KEY,
            CACHE_SECRET_KEY,
        ]
        .contains(&key)
            || key.starts_with("CIBURST_RUNNER_TOKEN_")
    }

    /// Keys a provider answers from every `apply` call; they replace stored values.
    pub fn is_refreshed(key: &str) -> bool {
        DISPATCHER_ENV.contains(&key)
    }

    /// Every output key of a deployment of `config`: the fixed ones and the keys of the
    /// runners of the VM runner and of the host project's lanes.
    pub fn output_keys(config: &Config) -> Vec<String> {
        FIXED
            .iter()
            .map(|key| key.to_string())
            .chain(
                runner_tags(config)
                    .into_iter()
                    .flat_map(|tag| [runner_id(&tag), runner_token(&tag)]),
            )
            .collect()
    }

    /// The tags of the host project's runners: the VM runner's, then its lanes'.
    pub fn runner_tags(config: &Config) -> Vec<String> {
        std::iter::once(config.vm.tag.clone())
            .chain(config.lanes.iter().map(|lane| lane.tag.clone()))
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
    let runner_tokens = keys::runner_tags(config)
        .into_iter()
        .filter_map(|tag| {
            outputs
                .get(&keys::runner_token(&tag))
                .map(|token| (tag, token.clone()))
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
        dispatcher_env: keys::DISPATCHER_ENV
            .iter()
            .filter_map(|key| outputs.get(*key).map(|v| (key.to_string(), v.clone())))
            .collect(),
        fleet_key: outputs.get(keys::FLEET_KEY).cloned(),
    }
}

/// A GitLab runner the config wants: the VM runner's, enabled in every further project, or a
/// host lane's, in the host project only (D60). `name` follows `<config name>-` in the
/// description and `runner:` in step ids.
struct RunnerSpec {
    name: String,
    tags: Vec<String>,
    id_key: String,
    token_key: String,
    enable: bool,
}

/// The VM runner or one project's lane in `ciburst status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStatus {
    /// The project of a lane; `None` for the VM runner.
    pub project: Option<u64>,
    /// The VM runner's tag or the lane's tag.
    pub tag: String,
    /// The lane's image; empty for the VM runner.
    pub image: String,
    pub live: usize,
    pub max_runs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub url: Option<String>,
    /// The VM runner, then each member's lanes, the host's first (spec 13.1).
    pub pools: Vec<PoolStatus>,
    pub runs: Vec<Run>,
    /// The fleet's members, the host first (spec 8.6); empty without fleet state.
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectReport {
    pub project_id: u64,
    pub valid: bool,
    pub errors: Vec<String>,
    pub problems: Vec<Problem>,
    /// Lanes for the Docker-free jobs on the VM runner (D46).
    pub suggestions: Vec<Suggestion>,
    /// Jobs of other runners that `ignoreJobs` misses (spec 13.2).
    pub ignore: Vec<String>,
    /// The project's hook to the dispatcher; `None` when the dispatcher's URL is unknown or
    /// the project has no hook to it (D51).
    pub hook: Option<Hook>,
    /// Why a member's project could not be read with this token (spec 8.6); the other
    /// fields are empty then.
    pub unreadable: Option<String>,
}

/// The dispatcher's hook of a project and its newest deliveries, oldest first (D52).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookStatus {
    pub project_id: u64,
    /// `None` when the project has no hook to the dispatcher.
    pub hook: Option<Hook>,
    pub deliveries: Vec<Delivery>,
    /// Why a member's hook could not be read with this token (spec 8.6).
    pub unreadable: Option<String>,
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
    /// Fleet state (D58); `None` asks the provider with the secrets of the outputs.
    fleet: Option<Arc<dyn FleetStore>>,
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
            fleet: None,
        }
    }

    /// The fleet state `plan`, `apply`, `status`, `check` and `destroy` use, instead of the one
    /// the provider opens.
    pub fn with_fleet(mut self, fleet: Option<Arc<dyn FleetStore>>) -> Provisioner {
        self.fleet = fleet;
        self
    }

    /// The fleet state: the one handed in, else the provider's, opened with `secrets`.
    async fn fleet(&self, secrets: &Secrets) -> Result<Option<Arc<dyn FleetStore>>> {
        match &self.fleet {
            Some(fleet) => Ok(Some(fleet.clone())),
            None => self.provider.fleet(secrets).await,
        }
    }

    /// The fleet state opened with the stored outputs.
    async fn stored_fleet(&self) -> Result<Option<Arc<dyn FleetStore>>> {
        let current = self.outputs.read().await?;
        self.fleet(&secrets_from_outputs(&self.config, &current))
            .await
    }

    /// The members besides the projects of the config: projects that joined (spec 8.6).
    async fn joined_members(&self) -> Result<Vec<Member>> {
        let Some(fleet) = self.stored_fleet().await? else {
            return Ok(Vec::new());
        };
        let mut members: Vec<Member> = fleet
            .members()
            .await?
            .into_iter()
            .filter(|m| !self.config.gitlab.project_ids.contains(&m.project_id))
            .collect();
        members.sort_by_key(|m| m.project_id);
        Ok(members)
    }

    /// spec 8.1 and 8.6: the routing config in the fleet state, the host as a member, and
    /// each further project of `gitlab.projectIds` as a member with the host's runners until it
    /// joins. `secrets` hold the hook token and the runner tokens.
    async fn fleet_steps(
        &self,
        fleet: &dyn FleetStore,
        secrets: &Secrets,
    ) -> Result<(Vec<Step>, Vec<Member>)> {
        let mut steps = Vec::new();
        let wanted = serde_json::to_string(&*self.config)?;
        let stored = fleet.config().await?;
        steps.push(Step::new(
            "fleet:config",
            if stored.as_deref() == Some(wanted.as_str()) {
                Action::Noop
            } else {
                Action::Update
            },
            "fleet config",
        ));
        let members = fleet.members().await?;
        let hook = secrets.hook_token.clone().unwrap_or_default();
        let now = (self.clock)();
        let mut writes = Vec::new();
        let vm_tag = &self.config.vm.tag;
        let runners: std::collections::BTreeMap<String, String> = secrets
            .runner_tokens
            .iter()
            .filter(|(tag, _)| *tag == vm_tag)
            .map(|(tag, token)| (tag.clone(), token.clone()))
            .collect();
        for (index, &project_id) in self.config.gitlab.project_ids.iter().enumerate() {
            let existing = members.iter().find(|m| m.project_id == project_id);
            // A further project that joined keeps its own record.
            let joined =
                index > 0 && existing.is_some_and(|m| m.runners.get(vm_tag) != runners.get(vm_tag));
            let member = Member {
                project_id,
                hook_secret: hook.clone(),
                runners: runners.clone(),
                joined_at: existing.map_or(now, |m| m.joined_at),
                ..Member::default()
            };
            let action = match existing {
                _ if joined => Action::Noop,
                None => Action::Create,
                Some(m) if *m == member => Action::Noop,
                Some(_) => Action::Update,
            };
            let mut step = Step::new(
                format!("member:{project_id}"),
                action,
                format!("member {project_id}"),
            );
            if index > 0 && action == Action::Create {
                step.detail = Some("host runners until it joins".into());
            }
            if action != Action::Noop {
                writes.push(member);
            }
            steps.push(step);
        }
        Ok((steps, writes))
    }

    /// The client `apply` posts waiting jobs to the dispatcher with (spec 11.4); without it
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

    fn runner_description(&self, name: &str) -> String {
        format!("{}-{name}", self.config.name)
    }

    /// The runners the config wants: the VM runner's, carrying only its tag (D62), then one
    /// per host lane with the lane's tag (D60).
    fn runner_specs(&self) -> Vec<RunnerSpec> {
        keys::runner_tags(&self.config)
            .into_iter()
            .enumerate()
            .map(|(index, tag)| RunnerSpec {
                name: tag.clone(),
                tags: vec![tag.clone()],
                id_key: keys::runner_id(&tag),
                token_key: keys::runner_token(&tag),
                enable: index == 0,
            })
            .collect()
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
        let specs = self.runner_specs();
        let mut steps = Vec::new();
        for spec in &specs {
            let description = self.runner_description(&spec.name);
            let existing = runners.iter().find(|r| r.description == description);
            let has_token = current.contains_key(&spec.token_key);
            let mut step = Step::new(
                format!("runner:{}", spec.name),
                Action::Create,
                format!("gitlab runner {description}"),
            );
            if existing.is_some() && rotate {
                step.action = Action::Update;
                step.detail = Some("delete and recreate, new token".into());
            } else if let Some(runner) = existing
                && has_token
            {
                let tagged = gitlab.runner(runner.id).await?.tag_list;
                let same =
                    spec.tags.len() == tagged.len() && spec.tags.iter().all(|t| tagged.contains(t));
                if same {
                    step.action = Action::Noop;
                    step.detail = None;
                } else {
                    step.action = Action::Update;
                    step.detail = Some(format!("{RETAG}{}", spec.tags.join(", ")));
                }
            } else if existing.is_some() {
                step.action = Action::Update;
                step.detail = Some("token missing in outputs, delete and recreate".into());
            }
            steps.push(step);
        }
        for name in self.orphan_runners(gitlab, &runners, &specs).await? {
            steps.push(Step::new(
                format!("runner:{name}"),
                Action::Delete,
                format!("gitlab runner {}", self.runner_description(&name)),
            ));
        }
        for &project_id in self.other_projects() {
            let enabled = gitlab.list_runners(project_id).await?;
            for spec in &specs {
                let description = self.runner_description(&spec.name);
                let present = enabled.iter().any(|r| r.description == description);
                // D60: a host lane's runner serves the host project only; an earlier
                // deployment enabled it in every project.
                let action = match (spec.enable, present) {
                    (true, true) | (false, false) => Action::Noop,
                    (true, false) => Action::Create,
                    (false, true) => Action::Delete,
                };
                if !spec.enable && action == Action::Noop {
                    continue;
                }
                steps.push(Step::new(
                    format!("runner-enable:{project_id}:{}", spec.name),
                    action,
                    format!("gitlab runner {description} enabled in project {project_id}"),
                ));
            }
        }
        Ok(steps)
    }

    /// Names of runners this deployment created that the config no longer wants: described
    /// `<name>-<x>` and carrying the tag `<x>`, or for the spill runner of an earlier
    /// deployment `<name>-<vm tag>-spill` the tag `<vm tag>`, so another deployment's runners
    /// never match.
    async fn orphan_runners(
        &self,
        gitlab: &Gitlab,
        runners: &[crate::gitlab::Runner],
        specs: &[RunnerSpec],
    ) -> Result<Vec<String>> {
        let prefix = format!("{}-", self.config.name);
        let mut names = Vec::new();
        for runner in runners {
            let Some(name) = runner.description.strip_prefix(&prefix) else {
                continue;
            };
            if name.is_empty() || specs.iter().any(|spec| spec.name == name) {
                continue;
            }
            let mut tags = vec![name];
            if let Some(vm) = name.strip_suffix(SPILL_SUFFIX).filter(|t| !t.is_empty()) {
                tags.push(vm);
            }
            for tag in tags {
                let tagged = gitlab
                    .list_runners_tagged(self.primary_project(), tag)
                    .await?;
                if tagged.iter().any(|r| r.id == runner.id) {
                    names.push(name.to_string());
                    break;
                }
            }
        }
        Ok(names)
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
                .and_then(|u| hooks.iter().find(|h| &h.url == u));
            let mut step = Step::new(
                format!("hook:{project_id}"),
                Action::Create,
                format!("gitlab hook in project {project_id}"),
            );
            if url.is_none() {
                step.detail = Some("url known after the dispatcher is deployed".into());
            }
            if let Some(hook) = existing {
                // D51: GitLab delivers nothing to a disabled hook until a test delivery succeeds.
                let mut changes = Vec::new();
                if rotate {
                    changes.push("new token".to_string());
                }
                if !hook.pipeline_events {
                    changes.push("pipeline events".to_string());
                }
                if !hook.is_executable() {
                    changes.push(format!("re-enable: {}", hook.disabled()));
                }
                step.action = if changes.is_empty() {
                    Action::Noop
                } else {
                    Action::Update
                };
                step.detail = (!changes.is_empty()).then(|| changes.join(", "));
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
            let Some(name) = step.id.strip_prefix("runner:") else {
                continue;
            };
            let description = self.runner_description(name);
            if let Some(runner) = runners.iter().find(|r| r.description == description) {
                gitlab.delete_runner(runner.id).await?;
                self.log.info(
                    "runner_deleted",
                    &[("runner", name), ("id", &runner.id.to_string())],
                );
            }
        }
        let specs = self.runner_specs();
        let mut created = Outputs::new();
        for spec in &specs {
            let id = format!("runner:{}", spec.name);
            let Some(step) = steps.iter().find(|s| s.id == id) else {
                continue;
            };
            if step.is_noop() {
                continue;
            }
            let description = self.runner_description(&spec.name);
            let existing = runners.iter().find(|r| r.description == description);
            if let Some(existing) = existing
                && step.detail.as_deref().is_some_and(|d| d.starts_with(RETAG))
            {
                gitlab.update_runner_tags(existing.id, &spec.tags).await?;
                self.log.info(
                    "runner_tags",
                    &[
                        ("runner", &spec.name),
                        ("id", &existing.id.to_string()),
                        ("tags", &spec.tags.join(",")),
                    ],
                );
                continue;
            }
            if let Some(existing) = existing {
                gitlab.delete_runner(existing.id).await?;
            }
            let runner = gitlab
                .create_runner(primary, &spec.tags, &description)
                .await?;
            created.insert(spec.id_key.clone(), runner.id.to_string());
            created.insert(spec.token_key.clone(), runner.token.clone());
            if spec.enable {
                for &project_id in self.other_projects() {
                    gitlab.enable_runner(project_id, runner.id).await?;
                }
            }
            self.log.info(
                "runner",
                &[("runner", &spec.name), ("id", &runner.id.to_string())],
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
            let Some((project, name)) = rest.split_once(':') else {
                continue;
            };
            let Some(spec) = specs.iter().find(|spec| spec.name == name) else {
                continue;
            };
            if step.action == Action::Delete {
                let project_id: u64 = project.parse().map_err(|_| Error::other("bad step id"))?;
                let description = self.runner_description(&spec.name);
                if let Some(runner) = gitlab
                    .list_runners(project_id)
                    .await?
                    .into_iter()
                    .find(|r| r.description == description)
                {
                    gitlab.disable_runner(project_id, runner.id).await?;
                    self.log.info(
                        "runner_disabled",
                        &[("project", project), ("runner", &spec.name)],
                    );
                }
                continue;
            }
            if created.contains_key(&spec.id_key) {
                continue;
            }
            match current
                .get(&spec.id_key)
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
                        ("runner", name),
                        ("reason", "runner id unknown"),
                    ],
                ),
            }
        }
        Ok(())
    }

    /// Creates missing hooks, gives existing ones a rotated token or the pipeline events they
    /// lack (spec 9.5), and enables a disabled one again with a test delivery (D51).
    async fn apply_hooks(&self, steps: &[Step], hook_token: &str, rotate: bool) -> Result<()> {
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
            let project = project_id.to_string();
            let hooks = gitlab.list_hooks(project_id).await?;
            match hooks.iter().find(|h| h.url == url) {
                Some(existing) => {
                    if rotate || !existing.pipeline_events {
                        gitlab
                            .update_hook(project_id, existing.id, &url, hook_token)
                            .await?;
                        self.log.info("hook", &[("project", &project)]);
                    }
                    if !existing.is_executable() {
                        gitlab
                            .test_hook(project_id, existing.id, REENABLE_TRIGGER)
                            .await?;
                        self.log.info("hook_reenabled", &[("project", &project)]);
                    }
                }
                None => {
                    gitlab.create_hook(project_id, &url, hook_token).await?;
                    self.log.info("hook", &[("project", &project)]);
                }
            }
        }
        Ok(())
    }

    /// A new VM secret would make every running VM's token invalid: its reports and its
    /// `/terminate` fail, and the sweep ends it once silent, even mid-job.
    async fn refuse_vm_rotation_while_running(&self) -> Result<()> {
        let mut running = 0;
        if let Some(vms) = self.provider.capacity(&self.config.vm.tag).await? {
            running += vms.len();
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
    /// lacks (spec 11.3): a key that exists only at the provider would otherwise stay lost
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
        if let Some(fleet) = self.fleet(&secrets).await? {
            steps.extend(self.fleet_steps(fleet.as_ref(), &secrets).await?.0);
        }
        Ok(steps)
    }

    /// Applies in phases so that later phases see the outputs of earlier ones. Nothing changes
    /// when the provider's preflight fails (D42); waiting jobs are replayed last.
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
        let fleet_key = self.ensure_secret(keys::FLEET_KEY, false).await?;

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
        secrets.fleet_key = Some(fleet_key);
        secrets.sweep_token = Some(sweep_token);
        // D58: the dispatcher reads the config from the fleet state, so a config change
        // needs no deploy; the host still redeploys when code or secrets change.
        if let Some(fleet) = self.fleet(&secrets).await? {
            let (fleet_steps, members) = self.fleet_steps(fleet.as_ref(), &secrets).await?;
            if fleet_steps
                .iter()
                .any(|s| s.id == "fleet:config" && !s.is_noop())
            {
                fleet
                    .put_config(&serde_json::to_string(&*self.config)?)
                    .await?;
                self.log
                    .info("fleet_config", &[("name", &self.config.name)]);
            }
            for member in &members {
                fleet.put_member(member).await?;
                self.log
                    .info("member", &[("project", &member.project_id.to_string())]);
            }
            applied.extend(fleet_steps);
        }
        let host_steps = self.host.plan(&secrets).await?;
        let host_outputs = self.host.apply(&host_steps, &secrets).await?;
        self.outputs.write(&host_outputs, true).await?;
        applied.extend(host_steps);

        let hook_steps = self.hook_steps(rotate_hook).await?;
        self.apply_hooks(&hook_steps, &hook_token, rotate_hook)
            .await?;
        applied.extend(hook_steps);
        self.replay(&hook_token).await;
        Ok(applied)
    }

    /// Posts every pending job that waits for a pool tag longer than [`REPLAY_MIN_AGE_S`] to the
    /// dispatcher as GitLab would (D41, spec 11.4), so jobs that queued before the setup
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
        let runner_tags = keys::runner_tags(&self.config);
        let tags: BTreeSet<&str> = runner_tags.iter().map(String::as_str).collect();
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
        // spec 8.6: members that joined lose their runners' VMs and lanes with the fleet.
        let joined = self.joined_members().await?;
        if !joined.is_empty() {
            let ids: Vec<String> = joined.iter().map(|m| m.project_id.to_string()).collect();
            return Err(Error::other(format!(
                "fleet {} has members {}: run ciburst-leave there first",
                self.config.name,
                ids.join(", ")
            )));
        }
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
            // The host's runners, and the spill runner an earlier deployment may have left.
            let mut named: Vec<(String, Option<String>)> = keys::runner_tags(&self.config)
                .into_iter()
                .map(|tag| {
                    let id_key = keys::runner_id(&tag);
                    (tag, Some(id_key))
                })
                .collect();
            named.push((format!("{}{SPILL_SUFFIX}", self.config.vm.tag), None));
            for (name, id_key) in named {
                let description = self.runner_description(&name);
                let id = runners
                    .iter()
                    .find(|r| r.description == description)
                    .map(|r| r.id)
                    .or_else(|| {
                        id_key
                            .and_then(|key| current.get(&key).cloned())
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
        let mut members = match self.stored_fleet().await? {
            Some(fleet) => fleet.members().await?,
            None => Vec::new(),
        };
        let host = self.primary_project();
        members.sort_by_key(|m| (m.project_id != host, m.project_id));
        let live = |key: &str| {
            runs.iter()
                .filter(|run| run.pool == key && run.state.is_live())
                .count()
        };
        let mut pools = vec![PoolStatus {
            project: None,
            tag: self.config.vm.tag.clone(),
            image: String::new(),
            live: live(&self.config.vm.tag),
            max_runs: self.config.vm.max_runs,
        }];
        // spec 13.1: the host's lanes from the config, every other member's from its record.
        for lane in &self.config.lanes {
            pools.push(PoolStatus {
                project: Some(host),
                tag: lane.tag.clone(),
                image: lane.image.clone(),
                live: live(&crate::config::lane_key(host, &lane.tag)),
                max_runs: lane.max_runs,
            });
        }
        for member in members.iter().filter(|m| m.project_id != host) {
            for lane in &member.lanes {
                pools.push(PoolStatus {
                    project: Some(member.project_id),
                    tag: lane.tag.clone(),
                    image: lane.image.clone(),
                    live: live(&crate::config::lane_key(member.project_id, &lane.tag)),
                    max_runs: lane.max_runs,
                });
            }
        }
        Ok(Status {
            url: self.host.url().await?,
            pools,
            runs,
            members,
        })
    }

    /// The projects `status` and `check` look at: those of the config, then the members that
    /// joined.
    async fn projects(&self) -> Result<Vec<u64>> {
        let mut projects = self.config.gitlab.project_ids.clone();
        projects.extend(self.joined_members().await?.iter().map(|m| m.project_id));
        Ok(projects)
    }

    /// Per project the hook to the dispatcher at `url` and its newest [`DELIVERIES`]; nothing
    /// without GitLab or a URL (D52).
    pub async fn hook_deliveries(&self, url: Option<&str>) -> Result<Vec<HookStatus>> {
        let (Some(gitlab), Some(url)) = (&self.gitlab, url) else {
            return Ok(Vec::new());
        };
        let mut status = Vec::new();
        let host_projects = &self.config.gitlab.project_ids;
        for project_id in self.projects().await? {
            let hook = match dispatcher_hook(gitlab, project_id, url).await {
                Ok(hook) => hook,
                Err(error) if !host_projects.contains(&project_id) && no_access(&error) => {
                    status.push(HookStatus {
                        project_id,
                        hook: None,
                        deliveries: Vec::new(),
                        unreadable: Some(error.to_string()),
                    });
                    continue;
                }
                Err(error) => return Err(error),
            };
            let mut deliveries = Vec::new();
            if let Some(hook) = &hook {
                let mut events = gitlab.hook_events(project_id, hook.id, DELIVERIES).await?;
                events.truncate(DELIVERIES);
                events.sort_by_key(|event| event.id);
                deliveries = events.iter().map(|event| event.delivery()).collect();
            }
            status.push(HookStatus {
                project_id,
                hook,
                deliveries,
                unreadable: None,
            });
        }
        Ok(status)
    }

    /// See [`check`]; the hooks checked are those to the host's URL.
    #[cfg(feature = "pipeline")]
    pub async fn check(&self, content_ref: Option<&str>) -> Result<Vec<ProjectReport>> {
        let gitlab = self
            .gitlab
            .as_ref()
            .ok_or_else(|| Error::other("check needs a GitLab token"))?;
        let url = self.host.url().await?;
        let mut projects = config_projects(&self.config);
        for member in self.joined_members().await? {
            let settings = member.project_config();
            projects.push((member.project_id, settings));
        }
        check_projects(&self.config, gitlab, &projects, content_ref, url.as_deref()).await
    }
}

/// Whether a GitLab call failed because the token may not read the project: a member's
/// project in another namespace (spec 8.6).
fn no_access(error: &Error) -> bool {
    matches!(
        error,
        Error::Gitlab {
            status: 401 | 403 | 404,
            ..
        }
    )
}

/// The hook of project `project_id` that delivers to `url`.
async fn dispatcher_hook(gitlab: &Gitlab, project_id: u64, url: &str) -> Result<Option<Hook>> {
    Ok(gitlab
        .list_hooks(project_id)
        .await?
        .into_iter()
        .find(|hook| hook.url == url))
}

/// The projects of `gitlab.projectIds`, each with its own settings (spec 13.2): the host's
/// lanes and `ignoreJobs`, none for the further projects, which use the host's VM runner.
pub fn config_projects(config: &Config) -> Vec<(u64, ProjectConfig)> {
    let host = config.host_project();
    config
        .gitlab
        .project_ids
        .iter()
        .map(|&id| {
            let settings = if id == host {
                config.project()
            } else {
                ProjectConfig::default()
            };
            (id, settings)
        })
        .collect()
}

/// Lints the host project's pipeline at `content_ref` (the default branch when `None`),
/// compares its jobs with its own lanes and `ignoreJobs` and proposes lanes (D46, spec
/// 13.2). With the dispatcher's `url` it also reads the project's hook to it (D51). Needs only
/// the config and GitLab.
#[cfg(feature = "pipeline")]
pub async fn check(
    config: &Config,
    gitlab: &Gitlab,
    content_ref: Option<&str>,
    url: Option<&str>,
) -> Result<Vec<ProjectReport>> {
    check_projects(config, gitlab, &config_projects(config), content_ref, url).await
}

/// [`check`] of `projects`, each with its own settings (spec 13.2): the config's and the
/// fleet's members (spec 8.6). A project outside the config that the token may not read is
/// reported `unreadable`.
#[cfg(feature = "pipeline")]
pub async fn check_projects(
    config: &Config,
    gitlab: &Gitlab,
    projects: &[(u64, ProjectConfig)],
    content_ref: Option<&str>,
    url: Option<&str>,
) -> Result<Vec<ProjectReport>> {
    let mut report = Vec::new();
    let vm_tag = config.vm.tag.as_str();
    for (project_id, settings) in projects {
        let project_id = *project_id;
        let read = async {
            let hook = match url {
                Some(url) => dispatcher_hook(gitlab, project_id, url).await?,
                None => None,
            };
            Ok::<_, Error>((hook, gitlab.lint_jobs(project_id, content_ref).await?))
        };
        let (hook, lint) = match read.await {
            Ok(read) => read,
            Err(error) if !config.gitlab.project_ids.contains(&project_id) && no_access(&error) => {
                report.push(ProjectReport {
                    project_id,
                    valid: true,
                    errors: Vec::new(),
                    problems: Vec::new(),
                    suggestions: Vec::new(),
                    ignore: Vec::new(),
                    hook: None,
                    unreadable: Some(error.to_string()),
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let (problems, suggestions, ignore) = if lint.valid {
            let pipeline = match &lint.merged_yaml {
                Some(text) => crate::pipeline::parse_merged_yaml(text)
                    .map_err(|error| Error::other(format!("project {project_id}: {error}")))?,
                None => Pipeline::default(),
            };
            (
                check_jobs(vm_tag, settings, &lint.jobs, &pipeline),
                suggest_lanes(vm_tag, settings, &lint.jobs, &pipeline),
                suggest_ignore_jobs(vm_tag, settings, &lint.jobs),
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        report.push(ProjectReport {
            project_id,
            valid: lint.valid,
            errors: lint.errors,
            problems,
            suggestions,
            ignore,
            hook,
            unreadable: None,
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
    fn output_keys_cover_the_vm_runner_and_the_lanes() {
        let config = Config::ci_defaults(5, "https://gitlab.example");
        let all = keys::output_keys(&config);
        assert_eq!(all.len(), keys::FIXED.len() + 2);
        assert!(all.contains(&keys::runner_token("vm")));
        assert!(all.contains(&keys::runner_id("vm")));
        assert_eq!(
            keys::runner_token("node-24"),
            "CIBURST_RUNNER_TOKEN_NODE_24"
        );
        assert!(keys::is_secret(&keys::runner_token("vm")));
        assert!(!keys::is_secret(&keys::runner_id("vm")));
        let mut lanes = config.clone();
        lanes.lanes = vec![
            serde_json::from_value(json!({ "tag": "node", "image": "node:24", "jobs": ["a"] }))
                .unwrap(),
        ];
        let all = keys::output_keys(&lanes);
        assert_eq!(all.len(), keys::FIXED.len() + 4);
        assert!(all.contains(&keys::runner_token("node")));
        assert!(all.contains(&keys::AGENT_SHA256.to_string()));
        assert!(keys::is_refreshed(keys::AGENT_SHA256));
        assert!(!keys::is_refreshed(keys::DISPATCH_SECRET_KEY));
    }
}
