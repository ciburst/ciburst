//! The configuration (spec 5.4, D60 to D63): the fleet settings of the host's `ciburst.json`
//! with its one VM runner (`vm`), and the project settings of every project's file: its lanes
//! and `ignoreJobs`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::types::Capabilities;
use crate::{Error, Result};

pub const DEFAULT_VCPU: u32 = 2;
pub const DEFAULT_MEMORY_MIB: u32 = 4096;
pub const DEFAULT_STORAGE_MIB: u32 = 10240;
pub const DEFAULT_TIMEOUT_S: u32 = 3600;
pub const DEFAULT_WAIT_TIMEOUT_S: u32 = 60;
/// Lane runs of one lane at once, by default.
pub const DEFAULT_MAX_RUNS: u32 = 10;
/// VMs of the VM runner at once, by default.
pub const DEFAULT_VM_MAX_RUNS: u32 = 4;
pub const DEFAULT_EXPIRE_DAYS: u32 = 30;
pub const DEFAULT_SLOTS: u32 = 2;
/// The VM runner's tag by default (D63).
pub const DEFAULT_VM_TAG: &str = "vm";
/// The image of a VM job that names none.
pub const DEFAULT_VM_IMAGE: &str = "ubuntu:24.04";
/// spec 5.4: seconds a VM job waits for a paid slot before another VM is bought, about one
/// VM boot.
pub const DEFAULT_MAX_QUEUE_S: u32 = 90;
/// spec 5.4: the largest `maxQueueS`.
pub const MAX_QUEUE_S_LIMIT: u32 = 3600;
/// The longest job definition name `<name>-<project id>-<tag>` (D60). Scaleway documents no
/// limit; 63 keeps the name a DNS label, the limit of its other resource names.
pub const MAX_DEFINITION_NAME: usize = 63;

/// The runtime of the VM runner on the main provider.
pub const INSTANCES_RUNTIME: &str = "instances";
/// The runtime of lanes on the main provider (D45).
pub const JOBS_RUNTIME: &str = "jobs";

/// The keys of the fleet settings, which only the host's `ciburst.json` holds (D60).
pub const FLEET_KEYS: [&str; 6] = ["name", "gitlab", "host", "provider", "cache", "vm"];

/// The keys of the project settings, which every project's `ciburst.json` may hold (D60).
pub const PROJECT_KEYS: [&str; 2] = ["lanes", "ignoreJobs"];

/// Keys of earlier configs and what replaces them (D62, D63, D65).
const REMOVED_KEYS: [(&str, &str); 6] = [
    (
        "pools",
        "replaced by vm (the VM runner) and lanes (Serverless Jobs lanes of this project)",
    ),
    (
        "defaultPool",
        "removed: a job that no lane and no ignoreJobs entry takes runs on the VM runner (vm)",
    ),
    (
        "stages",
        "removed: lanes take jobs by name with their jobs patterns",
    ),
    (
        "vmFirst",
        "removed: lane jobs run only in their lane, the VM runner carries only its tag",
    ),
    ("spillRunner", "removed: the VM runner carries only its tag"),
    (
        "providers",
        "removed: Scaleway is the only provider (D65); the VM runner runs on Scaleway Instances",
    ),
];

const fn d_vcpu() -> u32 {
    DEFAULT_VCPU
}
const fn d_memory() -> u32 {
    DEFAULT_MEMORY_MIB
}
const fn d_storage() -> u32 {
    DEFAULT_STORAGE_MIB
}
const fn d_timeout() -> u32 {
    DEFAULT_TIMEOUT_S
}
const fn d_wait() -> u32 {
    DEFAULT_WAIT_TIMEOUT_S
}
const fn d_max_runs() -> u32 {
    DEFAULT_MAX_RUNS
}
const fn d_vm_max_runs() -> u32 {
    DEFAULT_VM_MAX_RUNS
}
const fn d_expire() -> u32 {
    DEFAULT_EXPIRE_DAYS
}
const fn d_slots() -> u32 {
    DEFAULT_SLOTS
}
fn d_vm_tag() -> String {
    DEFAULT_VM_TAG.to_string()
}
fn d_vm_image() -> String {
    DEFAULT_VM_IMAGE.to_string()
}

/// The fleet's one VM runner (D63): VMs with a Docker executor that every member shares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VmConfig {
    #[serde(default = "d_vm_tag")]
    pub tag: String,
    /// `instances`, Scaleway Instances, the only runtime (D65).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// The job image when a job names none.
    #[serde(default = "d_vm_image")]
    pub image: String,
    /// Jobs one VM takes in parallel.
    #[serde(default = "d_slots")]
    pub slots: u32,
    /// Machine types tried in order; empty means the provider's default list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instance_types: Vec<String>,
    /// VMs at once.
    #[serde(default = "d_vm_max_runs")]
    pub max_runs: u32,
    /// Seconds a VM job may wait for a paid slot before another VM is bought (D53); `None`
    /// means [`DEFAULT_MAX_QUEUE_S`], 0 starts a VM at once.
    #[serde(default, rename = "maxQueueS", skip_serializing_if = "Option::is_none")]
    pub max_queue_s: Option<u32>,
    /// A pipeline that will need a VM starts one ahead of its first VM job (D48); `None`
    /// means yes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_start: Option<bool>,
}

impl Default for VmConfig {
    fn default() -> Self {
        VmConfig {
            tag: d_vm_tag(),
            runtime: None,
            image: d_vm_image(),
            slots: DEFAULT_SLOTS,
            instance_types: Vec::new(),
            max_runs: DEFAULT_VM_MAX_RUNS,
            max_queue_s: None,
            warm_start: None,
        }
    }
}

impl VmConfig {
    /// The runtime, `instances` unless set.
    pub fn runtime(&self) -> &str {
        self.runtime.as_deref().unwrap_or(INSTANCES_RUNTIME)
    }
}

/// One project's lane (D60): a Serverless Jobs definition for one image and size, with a runner
/// in that project only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Lane {
    pub tag: String,
    /// The job image: `linux/amd64` with `/bin/sh`.
    pub image: String,
    /// Job name patterns (`*`, `?`) the lane takes (D46).
    #[serde(default)]
    pub jobs: Vec<String>,
    #[serde(default = "d_vcpu")]
    pub vcpu: u32,
    #[serde(default = "d_memory", rename = "memoryMiB")]
    pub memory_mib: u32,
    #[serde(default = "d_storage", rename = "storageMiB")]
    pub storage_mib: u32,
    #[serde(default = "d_timeout")]
    pub timeout_s: u32,
    #[serde(default = "d_wait")]
    pub wait_timeout_s: u32,
    /// Lane runs at once.
    #[serde(default = "d_max_runs")]
    pub max_runs: u32,
}

impl Lane {
    /// The run spec of this lane of project `project`.
    pub fn pool(&self, project: u64) -> Pool {
        Pool {
            tag: lane_key(project, &self.tag),
            image: self.image.clone(),
            vcpu: self.vcpu,
            memory_mib: self.memory_mib,
            storage_mib: self.storage_mib,
            timeout_s: self.timeout_s,
            wait_timeout_s: self.wait_timeout_s,
            max_runs: self.max_runs,
            runtime: Some(JOBS_RUNTIME.to_string()),
            slots: 1,
            instance_types: Vec::new(),
            warm_start: None,
            max_queue_s: None,
            project: Some(project),
        }
    }
}

/// What one run needs from a provider: the VM runner, or one project's lane. Internal; the
/// config knows the VM runner and lanes only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    /// The VM runner's tag, or for a lane its key `<project id>-<lane tag>` ([`lane_key`]),
    /// which names its job definition `<name>-<key>` and its runs.
    pub tag: String,
    pub image: String,
    pub vcpu: u32,
    pub memory_mib: u32,
    pub storage_mib: u32,
    pub timeout_s: u32,
    pub wait_timeout_s: u32,
    pub max_runs: u32,
    /// `instances` or `jobs`.
    pub runtime: Option<String>,
    pub slots: u32,
    pub instance_types: Vec<String>,
    pub warm_start: Option<bool>,
    pub max_queue_s: Option<u32>,
    /// The project of a lane; `None` for the VM runner.
    pub project: Option<u64>,
}

impl Pool {
    /// D48: whether pipeline events may start a VM ahead of its jobs.
    pub fn warm_start(&self) -> bool {
        self.warm_start != Some(false)
    }

    /// D53, spec 5.4: the seconds a VM job may wait for a paid slot.
    pub fn max_queue_s(&self) -> u64 {
        u64::from(self.max_queue_s.unwrap_or(DEFAULT_MAX_QUEUE_S))
    }

    /// The tag of the GitLab runner that takes this pool's jobs: the lane tag of a lane, else
    /// the tag.
    pub fn runner_tag(&self) -> &str {
        match self.project {
            Some(project) => parse_lane_key(&self.tag)
                .filter(|(p, _)| *p == project)
                .map_or(self.tag.as_str(), |(_, tag)| tag),
            None => &self.tag,
        }
    }
}

/// The key of lane `tag` of project `project`: `<project id>-<tag>` (D60).
pub fn lane_key(project: u64, tag: &str) -> String {
    format!("{project}-{tag}")
}

/// The project and lane tag of a [`lane_key`]; `None` for any other text.
pub fn parse_lane_key(key: &str) -> Option<(u64, &str)> {
    let (project, tag) = key.split_once('-')?;
    if project.is_empty() || !project.bytes().all(|b| b.is_ascii_digit()) || tag.is_empty() {
        return None;
    }
    Some((project.parse().ok()?, tag))
}

/// The job definition name of lane `tag` of project `project` in deployment `name`.
pub fn definition_name(name: &str, project: u64, tag: &str) -> String {
    format!("{name}-{}", lane_key(project, tag))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitlabConfig {
    pub url: String,
    pub project_ids: Vec<u64>,
}

/// A `{ "type": "...", ... }` section; the extra keys belong to the selected module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CacheType {
    #[default]
    None,
    S3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheConfig {
    #[serde(rename = "type", default)]
    pub kind: CacheType,
    #[serde(default = "d_expire")]
    pub expire_days: u32,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            kind: CacheType::None,
            expire_days: DEFAULT_EXPIRE_DAYS,
        }
    }
}

/// The project settings of a member's own `ciburst.json` (D60, D61): its lanes and the jobs
/// that run on other runners. The host's are part of [`Config`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lanes: Vec<Lane>,
    #[serde(default, rename = "ignoreJobs", skip_serializing_if = "Vec::is_empty")]
    pub ignore_jobs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub name: String,
    pub gitlab: GitlabConfig,
    pub host: Section,
    pub provider: Section,
    /// The fleet's VM runner (D63).
    #[serde(default)]
    pub vm: VmConfig,
    /// The host project's lanes (D60).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lanes: Vec<Lane>,
    #[serde(default)]
    pub cache: CacheConfig,
    /// The host project's job name patterns of jobs on other runners (D49, D61).
    #[serde(default, rename = "ignoreJobs", skip_serializing_if = "Vec::is_empty")]
    pub ignore_jobs: Vec<String>,
}

/// Where a job of a project goes (D60 to D63).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route<'a, L> {
    /// A pattern of the project's `ignoreJobs` matches: nothing starts.
    Ignored,
    /// The first of the project's lanes with a matching `jobs` pattern.
    Lane(&'a L),
    /// The VM runner.
    Vm,
}

/// The route of job `name` of a project with `lanes` and `ignore_jobs`; `jobs` reads a lane's
/// patterns. Patterns of another project never apply.
pub fn route<'a, L>(
    name: &str,
    ignore_jobs: &[String],
    lanes: &'a [L],
    jobs: impl Fn(&L) -> &[String],
) -> Route<'a, L> {
    if ignore_jobs.iter().any(|pattern| glob_match(pattern, name)) {
        return Route::Ignored;
    }
    match lanes
        .iter()
        .find(|lane| jobs(lane).iter().any(|pattern| glob_match(pattern, name)))
    {
        Some(lane) => Route::Lane(lane),
        None => Route::Vm,
    }
}

impl ProjectConfig {
    /// The route of job `name` in this project.
    pub fn route(&self, name: &str) -> Route<'_, Lane> {
        route(name, &self.ignore_jobs, &self.lanes, |lane| &lane.jobs)
    }

    /// The lane with tag `tag`.
    pub fn lane_by_tag(&self, tag: &str) -> Option<&Lane> {
        self.lanes.iter().find(|lane| lane.tag == tag)
    }
}

impl Config {
    /// The config without the keys only routing reads, which a fleet's dispatcher takes from
    /// the fleet config (D58): a change of these alone needs no function deploy.
    pub fn deployment_view(&self) -> Config {
        let mut view = self.clone();
        view.ignore_jobs.clear();
        view.vm.warm_start = None;
        view.vm.max_queue_s = None;
        view.vm.max_runs = 0;
        view.vm.slots = 0;
        for lane in &mut view.lanes {
            lane.jobs.clear();
            lane.max_runs = 0;
        }
        view
    }

    /// The config `ciburst --ci` starts from (spec 11.3): the VM runner on Scaleway
    /// Instances and no lanes.
    pub fn ci_defaults(project_id: u64, server_url: &str) -> Config {
        let region = |kind: &str| Section {
            kind: kind.to_string(),
            extra: Map::from_iter([("region".to_string(), Value::from("fr-par"))]),
        };
        Config {
            name: format!("ci-{project_id}"),
            gitlab: GitlabConfig {
                url: server_url.to_string(),
                project_ids: vec![project_id],
            },
            host: region("scaleway-function"),
            provider: region("scaleway"),
            vm: VmConfig {
                runtime: Some(INSTANCES_RUNTIME.to_string()),
                instance_types: vec!["DEV1-M".to_string(), "DEV1-L".to_string()],
                ..VmConfig::default()
            },
            lanes: Vec::new(),
            cache: CacheConfig {
                kind: CacheType::S3,
                expire_days: DEFAULT_EXPIRE_DAYS,
            },
            ignore_jobs: Vec::new(),
        }
    }

    /// The host project: the first of `gitlab.projectIds`.
    pub fn host_project(&self) -> u64 {
        self.gitlab.project_ids.first().copied().unwrap_or(0)
    }

    /// The host project's settings.
    pub fn project(&self) -> ProjectConfig {
        ProjectConfig {
            lanes: self.lanes.clone(),
            ignore_jobs: self.ignore_jobs.clone(),
        }
    }

    /// The run spec of the VM runner.
    pub fn vm_pool(&self) -> Pool {
        Pool {
            tag: self.vm.tag.clone(),
            image: self.vm.image.clone(),
            vcpu: DEFAULT_VCPU,
            memory_mib: DEFAULT_MEMORY_MIB,
            storage_mib: DEFAULT_STORAGE_MIB,
            timeout_s: DEFAULT_TIMEOUT_S,
            wait_timeout_s: DEFAULT_WAIT_TIMEOUT_S,
            max_runs: self.vm.max_runs,
            // Unset, the provider's own default applies (`instances` on Scaleway).
            runtime: self.vm.runtime.clone(),
            slots: self.vm.slots,
            instance_types: self.vm.instance_types.clone(),
            warm_start: self.vm.warm_start,
            max_queue_s: self.vm.max_queue_s,
            project: None,
        }
    }

    /// The run specs of this config: the VM runner, then the host project's lanes.
    pub fn pools(&self) -> Vec<Pool> {
        let host = self.host_project();
        std::iter::once(self.vm_pool())
            .chain(self.lanes.iter().map(|lane| lane.pool(host)))
            .collect()
    }

    /// The run spec with tag `tag`: the VM runner's tag or a host lane's key.
    pub fn pool_by_tag(&self, tag: &str) -> Option<Pool> {
        self.pools().into_iter().find(|pool| pool.tag == tag)
    }

    /// Whether `pool` is the VM runner.
    pub fn is_vm_pool(&self, pool: &Pool) -> bool {
        pool.project.is_none() && pool.tag == self.vm.tag
    }

    /// Whether `pool` is a lane (D45).
    pub fn is_lane(&self, pool: &Pool) -> bool {
        pool.runtime.as_deref() == Some(JOBS_RUNTIME)
    }

    /// D49: whether `name` matches a pattern of the host project's `ignoreJobs`.
    pub fn ignores(&self, name: &str) -> bool {
        self.ignore_jobs
            .iter()
            .any(|pattern| glob_match(pattern, name))
    }

    /// The route of job `name` of the host project.
    pub fn route(&self, name: &str) -> Route<'_, Lane> {
        route(name, &self.ignore_jobs, &self.lanes, |lane| &lane.jobs)
    }

    /// The host project's lane with tag `tag`.
    pub fn lane_by_tag(&self, tag: &str) -> Option<&Lane> {
        self.lanes.iter().find(|lane| lane.tag == tag)
    }

    /// The tags of a pool's own GitLab runner: its runner tag only (D62).
    pub fn runner_tags(&self, pool: &Pool) -> Vec<String> {
        vec![pool.runner_tag().to_string()]
    }
}

/// The messages for keys of earlier configs in a config or project document, each naming the
/// replacement (D62, D63).
fn removed_keys(document: &Map<String, Value>) -> Vec<String> {
    let removed = |key: &str| {
        REMOVED_KEYS
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, m)| *m)
    };
    let mut errors: Vec<String> = document
        .keys()
        .filter_map(|key| removed(key).map(|message| format!("{key}: {message}")))
        .collect();
    let mut nested = |at: String, value: &Value| {
        for key in value.as_object().into_iter().flat_map(|o| o.keys()) {
            if let Some(message) = removed(key) {
                errors.push(format!("{at}.{key}: {message}"));
            }
        }
    };
    if let Some(vm) = document.get("vm") {
        nested("vm".to_string(), vm);
    }
    if let Some(Value::Array(lanes)) = document.get("lanes") {
        for (index, lane) in lanes.iter().enumerate() {
            nested(format!("lanes[{index}]"), lane);
        }
    }
    errors
}

/// The messages for top-level keys of `document` that are neither in `known` nor a removed
/// key, each listing `allowed`, so a misspelt key is refused instead of dropped.
fn unknown_keys(document: &Map<String, Value>, known: &[&str], allowed: &[&str]) -> Vec<String> {
    document
        .keys()
        .filter(|key| !known.contains(&key.as_str()) && !REMOVED_KEYS.iter().any(|(k, _)| k == key))
        .map(|key| format!("{key}: unknown key; the keys are {}", allowed.join(", ")))
        .collect()
}

/// Every key of a host's `ciburst.json`: the fleet settings and the project settings.
fn host_keys() -> Vec<&'static str> {
    FLEET_KEYS.iter().chain(&PROJECT_KEYS).copied().collect()
}

fn structural(error: serde_json::Error) -> Error {
    Error::Config(vec![error.to_string()])
}

fn object_of(json: &str) -> Result<Map<String, Value>> {
    match serde_json::from_str::<Value>(json).map_err(structural)? {
        Value::Object(map) => Ok(map),
        _ => Err(Error::Config(vec!["config: a JSON object".to_string()])),
    }
}

/// The config of `document`, refusing removed and unknown keys first.
fn config_of(document: Map<String, Value>) -> Result<Config> {
    let keys = host_keys();
    let mut errors = removed_keys(&document);
    errors.extend(unknown_keys(&document, &keys, &keys));
    if !errors.is_empty() {
        return Err(Error::Config(errors));
    }
    validate(serde_json::from_value(Value::Object(document)).map_err(structural)?)
}

/// Parses and validates a config document. Structural errors come from serde one at a time;
/// semantic errors are all reported together.
pub fn parse_config(json: &str) -> Result<Config> {
    config_of(object_of(json)?)
}

/// The config document `overlay` over `base` by top-level key: a key of `overlay` replaces
/// the whole value of `base` (spec 11.3). The result is validated.
pub fn merge_config(base: &Config, overlay: &str) -> Result<Config> {
    let overlay = object_of(overlay)?;
    let mut merged = match serde_json::to_value(base)? {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    merged.extend(overlay);
    config_of(merged)
}

/// A member's own `ciburst.json` (D60): only `lanes` and `ignoreJobs`. A fleet setting is
/// refused naming the key, because it belongs to the host's file; lanes are validated for
/// fleet `name`, whose VM runner has tag `vm_tag`, and project `project`.
pub fn parse_project_config(
    json: &str,
    name: &str,
    vm_tag: &str,
    project: u64,
) -> Result<ProjectConfig> {
    let document = object_of(json)?;
    let mut errors = removed_keys(&document);
    for key in document.keys() {
        if FLEET_KEYS.contains(&key.as_str()) {
            errors.push(format!(
                "{key}: a fleet setting; it belongs in the host's ciburst.json"
            ));
        }
    }
    errors.extend(unknown_keys(&document, &host_keys(), &PROJECT_KEYS));
    if !errors.is_empty() {
        return Err(Error::Config(errors));
    }
    let project_config: ProjectConfig =
        serde_json::from_value(Value::Object(document)).map_err(structural)?;
    validate_project(&project_config, name, vm_tag, project)?;
    Ok(project_config)
}

/// The problems of a project's settings, all together (D60).
pub fn validate_project(
    project_config: &ProjectConfig,
    name: &str,
    vm_tag: &str,
    project: u64,
) -> Result<()> {
    let mut errors = validate_lanes(&project_config.lanes, name, vm_tag, project);
    errors.extend(validate_ignore_jobs(&project_config.ignore_jobs));
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Config(errors))
    }
}

pub async fn load_config(path: &Path) -> Result<Config> {
    parse_config(&read_config_file(path).await?)
}

/// The text of a config file; a missing file is an error that names it.
pub async fn read_config_file(path: &Path) -> Result<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Error::other(format!(
            "config file {} not found",
            path.display()
        ))),
        Err(error) => Err(Error::other(format!("{}: {error}", path.display()))),
    }
}

/// The problems of `lanes` of project `project` in fleet `name`, whose VM runner has tag
/// `vm_tag`, each naming its field (D60).
pub fn validate_lanes(lanes: &[Lane], name: &str, vm_tag: &str, project: u64) -> Vec<String> {
    let mut errors = Vec::new();
    let mut tags = BTreeSet::new();
    let mut pattern_owner: BTreeMap<&str, &str> = BTreeMap::new();
    for (index, lane) in lanes.iter().enumerate() {
        let where_ = format!("lanes[{index}]");
        if !valid_tag(&lane.tag) {
            errors.push(format!(
                "{where_}.tag: lowercase letters, digits and hyphens"
            ));
        } else if lane.tag == vm_tag {
            errors.push(format!("{where_}.tag: {} is the VM runner's tag", lane.tag));
        } else if !tags.insert(lane.tag.as_str()) {
            errors.push(format!("{where_}.tag: duplicate {}", lane.tag));
        } else {
            let definition = definition_name(name, project, &lane.tag);
            if definition.len() > MAX_DEFINITION_NAME {
                errors.push(format!(
                    "{where_}.tag: job definition name {definition} is longer than {MAX_DEFINITION_NAME} characters"
                ));
            }
        }
        if lane.image.is_empty() {
            errors.push(format!("{where_}.image: required"));
        }
        for (field, value) in [
            ("vcpu", lane.vcpu),
            ("memoryMiB", lane.memory_mib),
            ("storageMiB", lane.storage_mib),
            ("timeoutS", lane.timeout_s),
            ("waitTimeoutS", lane.wait_timeout_s),
            ("maxRuns", lane.max_runs),
        ] {
            if value == 0 {
                errors.push(format!("{where_}.{field}: positive integer"));
            }
        }
        if lane.jobs.is_empty() {
            errors.push(format!("{where_}.jobs: at least one job name pattern"));
        }
        for pattern in &lane.jobs {
            if pattern.is_empty() {
                errors.push(format!("{where_}.jobs: empty pattern"));
            } else if let Some(owner) = pattern_owner.get(pattern.as_str()) {
                if *owner != lane.tag {
                    errors.push(format!(
                        "{where_}.jobs: {pattern} is in lane {owner} and lane {}",
                        lane.tag
                    ));
                }
            } else {
                pattern_owner.insert(pattern.as_str(), lane.tag.as_str());
            }
        }
    }
    errors
}

fn validate_ignore_jobs(ignore_jobs: &[String]) -> Vec<String> {
    if ignore_jobs.iter().any(String::is_empty) {
        vec!["ignoreJobs: empty pattern".to_string()]
    } else {
        Vec::new()
    }
}

pub fn validate(mut config: Config) -> Result<Config> {
    let mut errors = Vec::new();

    if !valid_name(&config.name) {
        errors.push(
            "name: 2 to 20 lowercase letters, digits or hyphens, starting with a letter"
                .to_string(),
        );
    }

    if !is_https_url(&config.gitlab.url) {
        errors.push("gitlab.url: https URL".to_string());
    } else {
        config.gitlab.url = config.gitlab.url.trim_end_matches('/').to_string();
    }
    if config.gitlab.project_ids.is_empty() || config.gitlab.project_ids.contains(&0) {
        errors.push("gitlab.projectIds: non-empty array of positive integers".to_string());
    }

    for (key, section) in [("host", &config.host), ("provider", &config.provider)] {
        if !valid_type(&section.kind) {
            errors.push(format!("{key}.type: lowercase letters, digits and hyphens"));
        }
    }

    let vm = &config.vm;
    if !valid_tag(&vm.tag) {
        errors.push("vm.tag: lowercase letters, digits and hyphens".to_string());
    } else if parse_lane_key(&vm.tag).is_some() {
        errors.push(format!(
            "vm.tag: {} starts with digits and a hyphen, the form of a lane's key",
            vm.tag
        ));
    }
    if vm.image.is_empty() {
        errors.push("vm.image: required".to_string());
    }
    let runtime = vm.runtime();
    if runtime != INSTANCES_RUNTIME {
        errors.push(format!(
            "vm.runtime: {runtime} is not {INSTANCES_RUNTIME}; Scaleway Instances is the only VM runtime (D65)"
        ));
    }
    for (field, value) in [("slots", vm.slots), ("maxRuns", vm.max_runs)] {
        if value == 0 {
            errors.push(format!("vm.{field}: positive integer"));
        }
    }
    if vm.max_queue_s.is_some_and(|s| s > MAX_QUEUE_S_LIMIT) {
        errors.push(format!(
            "vm.maxQueueS: whole number 0 to {MAX_QUEUE_S_LIMIT}"
        ));
    }

    errors.extend(validate_lanes(
        &config.lanes,
        &config.name,
        &config.vm.tag,
        config.host_project(),
    ));
    errors.extend(validate_ignore_jobs(&config.ignore_jobs));

    if config.cache.expire_days == 0 {
        errors.push("cache.expireDays: positive integer".to_string());
    }
    if config.cache.kind != CacheType::S3 {
        errors.push("cache.type: s3 required; the runner object lives in the bucket".to_string());
    }

    if errors.is_empty() {
        Ok(config)
    } else {
        Err(Error::Config(errors))
    }
}

/// Lanes of project `project` that exceed what their provider offers; `caps_for` answers per
/// run spec.
pub fn check_lane_capabilities(
    lanes: &[Lane],
    project: u64,
    caps_for: impl Fn(&Pool) -> Capabilities,
) -> Vec<String> {
    let mut errors = Vec::new();
    for (index, lane) in lanes.iter().enumerate() {
        let caps = caps_for(&lane.pool(project));
        for (field, value, limit) in [
            ("vcpu", lane.vcpu, caps.max_vcpu),
            ("memoryMiB", lane.memory_mib, caps.max_memory_mib),
            ("storageMiB", lane.storage_mib, caps.max_storage_mib),
            ("timeoutS", lane.timeout_s, caps.max_duration_s),
        ] {
            if value > limit {
                errors.push(format!(
                    "lanes[{index}].{field}: {value} above {} maximum {limit}",
                    caps.name
                ));
            }
        }
    }
    errors
}

/// The host project's lanes above their provider's capabilities.
pub fn check_capabilities(
    config: &Config,
    caps_for: impl Fn(&Pool) -> Capabilities,
) -> Vec<String> {
    check_lane_capabilities(&config.lanes, config.host_project(), caps_for)
}

/// D46: whether `name` matches `pattern`, where `*` matches any run of characters, `?` one
/// character and every other character itself.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0, 0);
    // The last `*` and the name position it matched up to; a mismatch retries one further.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((star_p, star_n)) => {
                    star = Some((star_p, star_n + 1));
                    p = star_p + 1;
                    n = star_n + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

fn valid_name(name: &str) -> bool {
    let len = name.len();
    (2..=20).contains(&len)
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_tag(tag: &str) -> bool {
    let len = tag.len();
    (1..=63).contains(&len)
        && tag.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && tag
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_type(kind: &str) -> bool {
    !kind.is_empty()
        && kind
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_https_url(url: &str) -> bool {
    match url.strip_prefix("https://") {
        Some(rest) => {
            let host = rest.split('/').next().unwrap_or("");
            !host.is_empty() && !host.contains(char::is_whitespace)
        }
        None => false,
    }
}

#[cfg(test)]
mod tests;
