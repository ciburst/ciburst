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
pub const DEFAULT_MAX_RUNS: u32 = 10;
pub const DEFAULT_EXPIRE_DAYS: u32 = 30;
pub const DEFAULT_SLOTS: u32 = 2;

/// The runtime of VM pools on the main provider.
pub const INSTANCES_RUNTIME: &str = "instances";

/// Names a key of `providers` must not take: runtimes of the main provider and step id
/// prefixes, which the composite routes by (M5 rule R7).
pub const RESERVED_RUNTIMES: [&str; 15] = [
    "instances",
    "jobs",
    "vm",
    "key",
    "definition",
    "bucket",
    "registry",
    "project",
    "iam",
    "hook",
    "runner",
    "runner-enable",
    "function",
    "namespace",
    "cron",
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
const fn d_expire() -> u32 {
    DEFAULT_EXPIRE_DAYS
}
const fn d_slots() -> u32 {
    DEFAULT_SLOTS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pool {
    pub tag: String,
    pub image: String,
    #[serde(default)]
    pub stages: Vec<String>,
    /// Job name patterns (`*`, `?`) routed to this pool before any stage (D46).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
    /// Lanes: a pending job claims a free slot of the VM pool before a lane run starts (D47);
    /// `None` means yes when a VM pool exists.
    #[serde(default, rename = "vmFirst", skip_serializing_if = "Option::is_none")]
    pub vm_first: Option<bool>,
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
    #[serde(default = "d_max_runs")]
    pub max_runs: u32,
    /// Kind of run: for `scaleway` `instances` (default) or `jobs`; a key of `providers`
    /// hands the pool to that provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Jobs one run takes in parallel (VM runtimes); one-job runtimes ignore it.
    #[serde(default = "d_slots")]
    pub slots: u32,
    /// Machine types tried in order (VM runtimes); empty means the provider's default list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instance_types: Vec<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub name: String,
    pub gitlab: GitlabConfig,
    pub host: Section,
    pub provider: Section,
    /// Pool of the stages no pool lists; without it such jobs start nothing (spec D36).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_pool: Option<String>,
    pub pools: Vec<Pool>,
    #[serde(default)]
    pub cache: CacheConfig,
    /// Further providers by runtime name (spec D35); their pools set `runtime` to the key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, Map<String, Value>>,
}

impl Config {
    /// The config `ciburst --ci` starts from (M5 rule R1): one VM pool on Scaleway Instances
    /// that takes every stage.
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
            default_pool: Some("vm".to_string()),
            pools: vec![Pool {
                tag: "vm".to_string(),
                image: "ubuntu:24.04".to_string(),
                stages: Vec::new(),
                jobs: Vec::new(),
                vm_first: None,
                vcpu: DEFAULT_VCPU,
                memory_mib: DEFAULT_MEMORY_MIB,
                storage_mib: DEFAULT_STORAGE_MIB,
                timeout_s: DEFAULT_TIMEOUT_S,
                wait_timeout_s: DEFAULT_WAIT_TIMEOUT_S,
                max_runs: 4,
                runtime: Some(INSTANCES_RUNTIME.to_string()),
                slots: DEFAULT_SLOTS,
                instance_types: vec!["DEV1-M".to_string(), "DEV1-L".to_string()],
            }],
            cache: CacheConfig {
                kind: CacheType::S3,
                expire_days: DEFAULT_EXPIRE_DAYS,
            },
            providers: BTreeMap::new(),
        }
    }

    /// Whether `pool` runs on VMs: runtime `instances` or an added provider.
    pub fn is_vm_pool(&self, pool: &Pool) -> bool {
        pool.runtime.as_deref() == Some(INSTANCES_RUNTIME) || self.extra_provider_of(pool).is_some()
    }

    pub fn pool_by_tag(&self, tag: &str) -> Option<&Pool> {
        self.pools.iter().find(|pool| pool.tag == tag)
    }

    /// The pool a stage is mapped to, else the default pool, else none.
    pub fn pool_for_stage(&self, stage: Option<&str>) -> Option<&Pool> {
        stage
            .and_then(|s| {
                self.pools
                    .iter()
                    .find(|pool| pool.stages.iter().any(|x| x == s))
            })
            .or_else(|| self.default_pool())
    }

    /// The added provider a pool belongs to, `None` for the main provider.
    pub fn extra_provider_of<'a>(&self, pool: &'a Pool) -> Option<&'a str> {
        pool.runtime
            .as_deref()
            .filter(|runtime| self.providers.contains_key(*runtime))
    }

    pub fn default_pool(&self) -> Option<&Pool> {
        self.default_pool
            .as_deref()
            .and_then(|tag| self.pool_by_tag(tag))
    }
}

/// Parses and validates a config document. Structural errors come from serde one at a time;
/// semantic errors are all reported together.
pub fn parse_config(json: &str) -> Result<Config> {
    let config: Config =
        serde_json::from_str(json).map_err(|error| Error::Config(vec![error.to_string()]))?;
    validate(config)
}

/// The config document `overlay` over `base` by top-level key: a key of `overlay` replaces
/// the whole value of `base` (M5 rule R1). The result is validated.
pub fn merge_config(base: &Config, overlay: &str) -> Result<Config> {
    let structural = |error: serde_json::Error| Error::Config(vec![error.to_string()]);
    let Value::Object(overlay) = serde_json::from_str::<Value>(overlay).map_err(structural)? else {
        return Err(Error::Config(vec!["config: a JSON object".to_string()]));
    };
    let mut merged = match serde_json::to_value(base)? {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    merged.extend(overlay);
    validate(serde_json::from_value(Value::Object(merged)).map_err(structural)?)
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

    if config.pools.is_empty() {
        errors.push("pools: non-empty array".to_string());
    } else {
        let mut tags = BTreeSet::new();
        let mut stage_owner: BTreeMap<&str, &str> = BTreeMap::new();
        for (index, pool) in config.pools.iter().enumerate() {
            let where_ = format!("pools[{index}]");
            if !valid_tag(&pool.tag) {
                errors.push(format!(
                    "{where_}.tag: lowercase letters, digits and hyphens"
                ));
            } else if !tags.insert(pool.tag.as_str()) {
                errors.push(format!("{where_}.tag: duplicate {}", pool.tag));
            }
            if pool.image.is_empty() {
                errors.push(format!("{where_}.image: required"));
            }
            for (field, value) in [
                ("vcpu", pool.vcpu),
                ("memoryMiB", pool.memory_mib),
                ("storageMiB", pool.storage_mib),
                ("timeoutS", pool.timeout_s),
                ("waitTimeoutS", pool.wait_timeout_s),
                ("maxRuns", pool.max_runs),
                ("slots", pool.slots),
            ] {
                if value == 0 {
                    errors.push(format!("{where_}.{field}: positive integer"));
                }
            }
            for stage in &pool.stages {
                if stage.is_empty() {
                    errors.push(format!("{where_}.stages: empty stage name"));
                } else if let Some(owner) = stage_owner.get(stage.as_str()) {
                    errors.push(format!(
                        "{where_}.stages: {stage} already mapped to pool {owner}"
                    ));
                } else {
                    stage_owner.insert(stage.as_str(), pool.tag.as_str());
                }
            }
        }
        if let Some(default) = &config.default_pool
            && !tags.contains(default.as_str())
        {
            errors.push(format!("defaultPool: {default} is not a pool"));
        }
    }

    for runtime in config.providers.keys() {
        if !valid_type(runtime) {
            errors.push(format!(
                "providers.{runtime}: lowercase letters, digits and hyphens"
            ));
        } else if RESERVED_RUNTIMES.contains(&runtime.as_str()) {
            errors.push(format!("providers.{runtime}: reserved name"));
        }
    }

    if config.cache.expire_days == 0 {
        errors.push("cache.expireDays: positive integer".to_string());
    }
    if config.cache.kind != CacheType::S3 {
        for (index, pool) in config.pools.iter().enumerate() {
            if config.is_vm_pool(pool) {
                errors.push(format!(
                    "pools[{index}]: VM pool {} needs cache.type s3",
                    pool.tag
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(config)
    } else {
        Err(Error::Config(errors))
    }
}

/// Pools that exceed what their provider offers; `caps_for` answers per pool (a composite with
/// the member serving it).
pub fn check_capabilities(
    config: &Config,
    caps_for: impl Fn(&Pool) -> Capabilities,
) -> Vec<String> {
    let mut errors = Vec::new();
    for (index, pool) in config.pools.iter().enumerate() {
        let caps = caps_for(pool);
        for (field, value, limit) in [
            ("vcpu", pool.vcpu, caps.max_vcpu),
            ("memoryMiB", pool.memory_mib, caps.max_memory_mib),
            ("storageMiB", pool.storage_mib, caps.max_storage_mib),
            ("timeoutS", pool.timeout_s, caps.max_duration_s),
        ] {
            if value > limit {
                errors.push(format!(
                    "pools[{index}].{field}: {value} above {} maximum {limit}",
                    caps.name
                ));
            }
        }
    }
    errors
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
mod tests {
    use super::*;
    use crate::types::Billing;

    fn valid() -> Value {
        serde_json::json!({
            "name": "demo",
            "gitlab": { "url": "https://gitlab.example/", "projectIds": [1, 2] },
            "host": { "type": "fake" },
            "provider": { "type": "fake", "region": "fr-par" },
            "defaultPool": "ci",
            "pools": [
                { "tag": "ci", "image": "img/node", "stages": ["test"] },
                { "tag": "build", "image": "img/kaniko", "stages": ["build"], "vcpu": 4 }
            ]
        })
    }

    fn errors_of(value: Value) -> Vec<String> {
        match parse_config(&value.to_string()) {
            Err(Error::Config(errors)) => errors,
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn applies_defaults_and_normalizes() {
        let config = parse_config(&valid().to_string()).unwrap();
        assert_eq!(config.gitlab.url, "https://gitlab.example");
        assert_eq!(config.pools[0].vcpu, DEFAULT_VCPU);
        assert_eq!(config.pools[0].max_runs, DEFAULT_MAX_RUNS);
        assert_eq!(config.pools[1].vcpu, 4);
        assert_eq!(config.cache, CacheConfig::default());
        assert_eq!(config.provider.extra["region"], "fr-par");
    }

    #[test]
    fn reports_structural_errors_from_serde() {
        let errors = errors_of(serde_json::json!({ "name": "x" }));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("missing field"), "{errors:?}");
        let errors = errors_of(serde_json::json!([]));
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn collects_every_semantic_error() {
        let mut raw = valid();
        raw["name"] = "Bad Name".into();
        raw["gitlab"] = serde_json::json!({ "url": "http://insecure", "projectIds": [] });
        raw["host"] = serde_json::json!({ "type": "Fake!" });
        raw["defaultPool"] = "missing".into();
        raw["pools"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "tag": "ci", "image": "", "stages": ["test", ""], "vcpu": 0
            }));
        raw["cache"] = serde_json::json!({ "type": "s3", "expireDays": 0 });
        let errors = errors_of(raw);
        let expect = |needle: &str| {
            assert!(
                errors.iter().any(|e| e.contains(needle)),
                "{needle} in {errors:?}"
            )
        };
        expect("name:");
        expect("gitlab.url");
        expect("gitlab.projectIds");
        expect("host.type");
        expect("pools[2].tag: duplicate ci");
        expect("pools[2].image");
        expect("pools[2].vcpu");
        expect("pools[2].stages: test already mapped to pool ci");
        expect("pools[2].stages: empty stage name");
        expect("defaultPool: missing is not a pool");
        expect("cache.expireDays");
    }

    #[test]
    fn rejects_empty_pools() {
        let mut raw = valid();
        raw["pools"] = serde_json::json!([]);
        assert!(errors_of(raw).iter().any(|e| e.starts_with("pools:")));
    }

    #[test]
    fn routes_stages_with_default_fallback() {
        let config = parse_config(&valid().to_string()).unwrap();
        let tag = |stage| config.pool_for_stage(stage).map(|p| p.tag.as_str());
        assert_eq!(tag(Some("build")), Some("build"));
        assert_eq!(tag(Some("test")), Some("ci"));
        assert_eq!(tag(Some("deploy")), Some("ci"));
        assert_eq!(tag(None), Some("ci"));
    }

    #[test]
    fn without_a_default_pool_unmapped_stages_have_none() {
        let mut raw = valid();
        raw.as_object_mut().unwrap().remove("defaultPool");
        let config = parse_config(&raw.to_string()).unwrap();
        assert_eq!(config.pool_for_stage(Some("deploy")), None);
        assert_eq!(config.pool_for_stage(None), None);
        assert_eq!(
            config.pool_for_stage(Some("build")).map(|p| p.tag.as_str()),
            Some("build")
        );
    }

    #[test]
    fn checks_capabilities() {
        let config = parse_config(&valid().to_string()).unwrap();
        let caps = Capabilities {
            name: "tiny".into(),
            docker_daemon: false,
            max_vcpu: 2,
            max_memory_mib: 4096,
            max_storage_mib: 10240,
            max_duration_s: 600,
            archs: vec!["amd64".into()],
            billing: Billing::PerSecond,
        };
        assert_eq!(
            check_capabilities(&config, |_| caps.clone()),
            vec![
                "pools[0].timeoutS: 3600 above tiny maximum 600",
                "pools[1].vcpu: 4 above tiny maximum 2",
                "pools[1].timeoutS: 3600 above tiny maximum 600",
            ]
        );
        let roomy = Capabilities {
            max_vcpu: 6,
            max_duration_s: 86400,
            ..caps.clone()
        };
        assert!(check_capabilities(&config, |_| roomy.clone()).is_empty());
        let per_pool = check_capabilities(&config, |pool| {
            if pool.tag == "build" {
                roomy.clone()
            } else {
                caps.clone()
            }
        });
        assert_eq!(
            per_pool,
            vec!["pools[0].timeoutS: 3600 above tiny maximum 600"]
        );
    }

    #[test]
    fn ci_defaults_are_one_vm_pool_on_scaleway() {
        let defaults = Config::ci_defaults(4711, "https://gitlab.example");
        let config = validate(defaults.clone()).unwrap();
        assert_eq!(config, defaults);
        assert_eq!(config.name, "ci-4711");
        assert_eq!(config.gitlab.url, "https://gitlab.example");
        assert_eq!(config.gitlab.project_ids, [4711]);
        assert_eq!(config.host.kind, "scaleway-function");
        assert_eq!(config.host.extra["region"], "fr-par");
        assert_eq!(config.provider.kind, "scaleway");
        assert_eq!(config.provider.extra["region"], "fr-par");
        assert_eq!(config.provider.extra.len(), 1, "no registry key");
        assert_eq!(config.default_pool.as_deref(), Some("vm"));
        assert_eq!(config.pools.len(), 1);
        let pool = &config.pools[0];
        assert_eq!(pool.tag, "vm");
        assert_eq!(pool.runtime.as_deref(), Some("instances"));
        assert_eq!(pool.image, "ubuntu:24.04");
        assert!(pool.stages.is_empty());
        assert_eq!((pool.slots, pool.max_runs), (2, 4));
        assert_eq!(pool.instance_types, ["DEV1-M", "DEV1-L"]);
        assert_eq!(
            config.cache,
            CacheConfig {
                kind: CacheType::S3,
                expire_days: 30
            }
        );
        assert!(config.providers.is_empty());
        assert!(config.is_vm_pool(pool));
    }

    #[test]
    fn a_file_replaces_whole_top_level_keys_of_the_defaults() {
        let defaults = Config::ci_defaults(5, "https://gitlab.example");
        assert_eq!(merge_config(&defaults, "{}").unwrap(), defaults);

        let merged = merge_config(
            &defaults,
            r#"{
              "provider": { "type": "scaleway", "zone": "nl-ams-1" },
              "pools": [
                { "tag": "vm", "image": "ubuntu:24.04", "runtime": "instances" },
                { "tag": "rust", "image": "img/rust", "runtime": "jobs", "stages": ["rust"] }
              ]
            }"#,
        )
        .unwrap();
        assert_eq!(merged.provider.extra.get("region"), None, "replaced whole");
        assert_eq!(merged.provider.extra["zone"], "nl-ams-1");
        assert_eq!(merged.pools.len(), 2);
        assert_eq!(
            merged.pools[0].max_runs, DEFAULT_MAX_RUNS,
            "no default kept"
        );
        assert!(merged.pools[0].instance_types.is_empty());
        assert_eq!(merged.name, defaults.name);
        assert_eq!(merged.gitlab, defaults.gitlab);
        assert_eq!(merged.host, defaults.host);
        assert_eq!(merged.default_pool, defaults.default_pool);
        assert_eq!(merged.cache, defaults.cache);

        let named = merge_config(&defaults, r#"{ "name": "shop", "defaultPool": null }"#).unwrap();
        assert_eq!(named.name, "shop");
        assert_eq!(named.default_pool, None);

        match merge_config(&defaults, r#"{ "cache": { "type": "none" } }"#) {
            Err(Error::Config(errors)) => {
                assert_eq!(errors, ["pools[0]: VM pool vm needs cache.type s3"])
            }
            other => panic!("expected a config error, got {other:?}"),
        }
        for bad in ["[]", "{", r#"{ "pools": 3 }"#] {
            assert!(
                matches!(merge_config(&defaults, bad), Err(Error::Config(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn rejects_reserved_provider_names() {
        for name in RESERVED_RUNTIMES {
            let mut raw = valid();
            raw["providers"] = serde_json::json!({});
            raw["providers"][name] = serde_json::json!({});
            raw["cache"] = serde_json::json!({ "type": "s3" });
            assert_eq!(
                errors_of(raw),
                [format!("providers.{name}: reserved name")],
                "{name}"
            );
        }
        let mut raw = valid();
        raw["providers"] = serde_json::json!({ "hetzner": {} });
        assert!(parse_config(&raw.to_string()).is_ok());
    }

    #[test]
    fn vm_pools_need_an_s3_cache() {
        let mut raw = valid();
        raw["providers"] = serde_json::json!({ "hetzner": {} });
        raw["pools"] = serde_json::json!([
            { "tag": "vm", "image": "img/node", "runtime": "instances" },
            { "tag": "hz", "image": "img/node", "runtime": "hetzner" },
            { "tag": "ci", "image": "img/node", "runtime": "jobs" },
            { "tag": "other", "image": "img/node", "runtime": "elsewhere" },
            { "tag": "plain", "image": "img/node" }
        ]);
        assert_eq!(
            errors_of(raw.clone()),
            [
                "pools[0]: VM pool vm needs cache.type s3",
                "pools[1]: VM pool hz needs cache.type s3",
            ]
        );
        raw["cache"] = serde_json::json!({ "type": "none" });
        assert_eq!(errors_of(raw.clone()).len(), 2);
        raw["cache"] = serde_json::json!({ "type": "s3" });
        assert!(parse_config(&raw.to_string()).is_ok());
    }

    #[tokio::test]
    async fn a_missing_config_file_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.json");
        let error = load_config(&path).await.unwrap_err().to_string();
        assert_eq!(error, format!("config file {} not found", path.display()));
    }
}
