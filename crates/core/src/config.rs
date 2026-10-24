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
pub const DEFAULT_SLOTS: u32 = 1;

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
    /// Provider-specific kind of run, for `scaleway`: `instances` (default) or `jobs`.
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
    pub default_pool: String,
    pub pools: Vec<Pool>,
    #[serde(default)]
    pub cache: CacheConfig,
}

impl Config {
    pub fn pool_by_tag(&self, tag: &str) -> Option<&Pool> {
        self.pools.iter().find(|pool| pool.tag == tag)
    }

    /// The pool a stage is mapped to, or the default pool.
    pub fn pool_for_stage(&self, stage: Option<&str>) -> &Pool {
        stage
            .and_then(|s| {
                self.pools
                    .iter()
                    .find(|pool| pool.stages.iter().any(|x| x == s))
            })
            .unwrap_or_else(|| self.default_pool())
    }

    pub fn default_pool(&self) -> &Pool {
        self.pool_by_tag(&self.default_pool)
            .expect("validated config has its default pool")
    }
}

/// Parses and validates a config document. Structural errors come from serde one at a time;
/// semantic errors are all reported together.
pub fn parse_config(json: &str) -> Result<Config> {
    let config: Config =
        serde_json::from_str(json).map_err(|error| Error::Config(vec![error.to_string()]))?;
    validate(config)
}

pub async fn load_config(path: &Path) -> Result<Config> {
    let text = tokio::fs::read_to_string(path).await?;
    parse_config(&text)
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
        if !tags.contains(config.default_pool.as_str()) {
            errors.push(format!(
                "defaultPool: {} is not a pool",
                config.default_pool
            ));
        }
    }

    if config.cache.expire_days == 0 {
        errors.push("cache.expireDays: positive integer".to_string());
    }

    if errors.is_empty() {
        Ok(config)
    } else {
        Err(Error::Config(errors))
    }
}

/// Pools that exceed what the provider offers.
pub fn check_capabilities(config: &Config, caps: &Capabilities) -> Vec<String> {
    let mut errors = Vec::new();
    for (index, pool) in config.pools.iter().enumerate() {
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
        assert_eq!(config.pool_for_stage(Some("build")).tag, "build");
        assert_eq!(config.pool_for_stage(Some("test")).tag, "ci");
        assert_eq!(config.pool_for_stage(Some("deploy")).tag, "ci");
        assert_eq!(config.pool_for_stage(None).tag, "ci");
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
            check_capabilities(&config, &caps),
            vec![
                "pools[0].timeoutS: 3600 above tiny maximum 600",
                "pools[1].vcpu: 4 above tiny maximum 2",
                "pools[1].timeoutS: 3600 above tiny maximum 600",
            ]
        );
        let roomy = Capabilities {
            max_vcpu: 6,
            max_duration_s: 86400,
            ..caps
        };
        assert!(check_capabilities(&config, &roomy).is_empty());
    }
}
