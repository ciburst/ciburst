//! The VM's settings in `/etc/ciburst/vm.env`, written by the provider's cloud-init
//! (spec 6.1). Errors name keys, never values.

use std::collections::BTreeMap;
use std::fmt;

pub const DEFAULT_PATH: &str = "/etc/ciburst/vm.env";
/// Overrides [`DEFAULT_PATH`], for tests and manual runs.
pub const PATH_ENV: &str = "CIBURST_VM_ENV";
/// Directory of the runner's `config.toml`, unless `CIBURST_STATE_DIR` names another.
pub const STATE_DIR: &str = "/var/lib/ciburst";

const REQUIRED: [&str; 9] = [
    "CIBURST_RUN_ID",
    "CIBURST_VM_TOKEN",
    "CIBURST_DISPATCH_URL",
    "CI_SERVER_URL",
    "RUNNER_TOKEN",
    "CIBURST_POOL",
    "CIBURST_SLOTS",
    "CIBURST_DEFAULT_IMAGE",
    "CIBURST_TIMEOUT_S",
];

const CACHE_S3: [&str; 5] = [
    "CACHE_S3_SERVER_ADDRESS",
    "CACHE_S3_BUCKET_NAME",
    "CACHE_S3_BUCKET_LOCATION",
    "CACHE_S3_ACCESS_KEY",
    "CACHE_S3_SECRET_KEY",
];

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub run_id: String,
    pub token: String,
    /// Without a trailing slash.
    pub dispatch_url: String,
    pub server_url: String,
    pub runner_token: String,
    pub pool: String,
    pub slots: u32,
    pub default_image: String,
    pub timeout_s: u64,
    pub cache: Option<S3Cache>,
    /// Holds the runner's `config.toml` (spec 8.5).
    pub state_dir: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct S3Cache {
    pub server_address: String,
    pub bucket_name: String,
    pub bucket_location: String,
    pub access_key: String,
    pub secret_key: String,
    pub shared: bool,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("run_id", &self.run_id)
            .field("dispatch_url", &self.dispatch_url)
            .field("server_url", &self.server_url)
            .field("pool", &self.pool)
            .field("slots", &self.slots)
            .field("default_image", &self.default_image)
            .field("timeout_s", &self.timeout_s)
            .field("cache", &self.cache.is_some())
            .field("state_dir", &self.state_dir)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Line(usize),
    Missing(Vec<&'static str>),
    Invalid(&'static str, &'static str),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Line(line) => write!(f, "line {line} is not KEY=VALUE"),
            ConfigError::Missing(keys) => write!(f, "missing {}", keys.join(", ")),
            ConfigError::Invalid(key, expected) => write!(f, "{key} must be {expected}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Parses `KEY=VALUE` lines; blank lines and `#` comments are skipped, and one pair of
/// surrounding quotes is removed as systemd's `EnvironmentFile` does. An empty value counts
/// as missing.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let mut values = BTreeMap::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ConfigError::Line(index + 1));
        };
        values.insert(key.trim(), unquote(value.trim()));
    }
    let get = |key: &str| values.get(key).copied().filter(|value| !value.is_empty());

    let missing: Vec<&'static str> = REQUIRED
        .into_iter()
        .filter(|key| get(key).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(ConfigError::Missing(missing));
    }
    let required = |key: &str| get(key).unwrap_or_default().to_string();

    let slots = required("CIBURST_SLOTS")
        .parse::<u32>()
        .ok()
        .filter(|slots| *slots > 0)
        .ok_or(ConfigError::Invalid(
            "CIBURST_SLOTS",
            "a whole number from 1",
        ))?;
    let timeout_s = required("CIBURST_TIMEOUT_S")
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .ok_or(ConfigError::Invalid(
            "CIBURST_TIMEOUT_S",
            "a whole number of seconds from 1",
        ))?;

    let cache = match get("CACHE_TYPE") {
        None => None,
        Some("s3") => {
            let missing: Vec<&'static str> = CACHE_S3
                .into_iter()
                .filter(|key| get(key).is_none())
                .collect();
            if !missing.is_empty() {
                return Err(ConfigError::Missing(missing));
            }
            Some(S3Cache {
                server_address: required("CACHE_S3_SERVER_ADDRESS"),
                bucket_name: required("CACHE_S3_BUCKET_NAME"),
                bucket_location: required("CACHE_S3_BUCKET_LOCATION"),
                access_key: required("CACHE_S3_ACCESS_KEY"),
                secret_key: required("CACHE_S3_SECRET_KEY"),
                shared: get("CACHE_SHARED") == Some("true"),
            })
        }
        Some(_) => return Err(ConfigError::Invalid("CACHE_TYPE", "s3 or empty")),
    };

    Ok(Config {
        run_id: required("CIBURST_RUN_ID"),
        token: required("CIBURST_VM_TOKEN"),
        dispatch_url: required("CIBURST_DISPATCH_URL")
            .trim_end_matches('/')
            .to_string(),
        server_url: required("CI_SERVER_URL"),
        runner_token: required("RUNNER_TOKEN"),
        pool: required("CIBURST_POOL"),
        slots,
        default_image: required("CIBURST_DEFAULT_IMAGE"),
        timeout_s,
        cache,
        state_dir: get("CIBURST_STATE_DIR")
            .unwrap_or(STATE_DIR)
            .trim_end_matches('/')
            .to_string(),
    })
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const SAMPLE: &str = "\
# written by cloud-init
CIBURST_RUN_ID=11111111-2222-3333-4444-555555555555
CIBURST_VM_TOKEN=vm-token-secret
CIBURST_DISPATCH_URL=https://dispatch.example.invalid/
CI_SERVER_URL=https://gitlab.example.invalid
RUNNER_TOKEN=glrt-runner-secret
CIBURST_POOL=vm
CIBURST_SLOTS=2
CIBURST_DEFAULT_IMAGE=registry.example.invalid/ciburst/node:1
CIBURST_TIMEOUT_S=3600

CACHE_TYPE=s3
CACHE_S3_SERVER_ADDRESS=s3.fr-par.scw.cloud
CACHE_S3_BUCKET_NAME=ci-cache-bucket
CACHE_S3_BUCKET_LOCATION=fr-par
CACHE_S3_ACCESS_KEY=SCWACCESSKEYSAMPLE
CACHE_S3_SECRET_KEY=cache-secret-key
CACHE_SHARED=true
";

    pub(crate) fn sample() -> Config {
        parse(SAMPLE).expect("sample parses")
    }

    fn without(key: &str) -> String {
        SAMPLE
            .lines()
            .filter(|line| !line.starts_with(&format!("{key}=")))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn with(key: &str, value: &str) -> String {
        format!("{}\n{key}={value}\n", without(key))
    }

    #[test]
    fn parses_every_key() {
        let config = sample();
        assert_eq!(config.run_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(config.token, "vm-token-secret");
        assert_eq!(config.dispatch_url, "https://dispatch.example.invalid");
        assert_eq!(config.server_url, "https://gitlab.example.invalid");
        assert_eq!(config.runner_token, "glrt-runner-secret");
        assert_eq!(config.pool, "vm");
        assert_eq!(config.slots, 2);
        assert_eq!(
            config.default_image,
            "registry.example.invalid/ciburst/node:1"
        );
        assert_eq!(config.timeout_s, 3600);
        let cache = config.cache.expect("cache");
        assert_eq!(cache.server_address, "s3.fr-par.scw.cloud");
        assert_eq!(cache.bucket_name, "ci-cache-bucket");
        assert_eq!(cache.bucket_location, "fr-par");
        assert_eq!(cache.access_key, "SCWACCESSKEYSAMPLE");
        assert_eq!(cache.secret_key, "cache-secret-key");
        assert!(cache.shared);
        assert_eq!(config.state_dir, "/var/lib/ciburst");
        let config = parse(&with("CIBURST_STATE_DIR", "/srv/agent/")).unwrap();
        assert_eq!(config.state_dir, "/srv/agent");
    }

    #[test]
    fn missing_keys_are_named_without_values() {
        let error = parse(&without("RUNNER_TOKEN")).unwrap_err();
        assert_eq!(error, ConfigError::Missing(vec!["RUNNER_TOKEN"]));
        assert_eq!(error.to_string(), "missing RUNNER_TOKEN");

        let text = without("CIBURST_RUN_ID").replace("CIBURST_VM_TOKEN=vm-token-secret", "");
        let error = parse(&text).unwrap_err();
        assert_eq!(
            error.to_string(),
            "missing CIBURST_RUN_ID, CIBURST_VM_TOKEN"
        );
        for secret in [
            "glrt-runner-secret",
            "cache-secret-key",
            "SCWACCESSKEYSAMPLE",
        ] {
            assert!(!error.to_string().contains(secret));
        }

        let error = parse(&with("CIBURST_POOL", "")).unwrap_err();
        assert_eq!(error.to_string(), "missing CIBURST_POOL");
    }

    #[test]
    fn invalid_values_name_the_key_only() {
        let error = parse(&with("CIBURST_SLOTS", "two-secret")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "CIBURST_SLOTS must be a whole number from 1"
        );
        assert!(parse(&with("CIBURST_SLOTS", "0")).is_err());
        let error = parse(&with("CIBURST_TIMEOUT_S", "1h")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "CIBURST_TIMEOUT_S must be a whole number of seconds from 1"
        );
        let error = parse(&with("CACHE_TYPE", "gcs")).unwrap_err();
        assert_eq!(error.to_string(), "CACHE_TYPE must be s3 or empty");
        let error = parse("CIBURST_RUN_ID=x\nnot a pair\n").unwrap_err();
        assert_eq!(error.to_string(), "line 2 is not KEY=VALUE");
    }

    #[test]
    fn cache_keys_are_required_only_with_s3() {
        let error = parse(&without("CACHE_S3_SECRET_KEY")).unwrap_err();
        assert_eq!(error.to_string(), "missing CACHE_S3_SECRET_KEY");

        let mut text = SAMPLE.to_string();
        for key in ["CACHE_TYPE", "CACHE_S3_SECRET_KEY", "CACHE_SHARED"] {
            text = text
                .lines()
                .filter(|line| !line.starts_with(&format!("{key}=")))
                .collect::<Vec<_>>()
                .join("\n");
        }
        assert!(parse(&text).unwrap().cache.is_none());

        let config = parse(&with("CACHE_SHARED", "false")).unwrap();
        assert!(!config.cache.unwrap().shared);
    }

    /// D62: the spill token of a VM started before D62 is ignored.
    #[test]
    fn a_spill_token_is_ignored() {
        let text = format!("{SAMPLE}RUNNER_SPILL_TOKEN=glrt-spill-secret\n");
        assert_eq!(parse(&text).unwrap(), sample());
    }

    #[test]
    fn quotes_and_spaces_are_trimmed() {
        let config = parse(&with("CIBURST_POOL", "\"vm\"")).unwrap();
        assert_eq!(config.pool, "vm");
        let config = parse(&with("CIBURST_DEFAULT_IMAGE", " 'node:24' ")).unwrap();
        assert_eq!(config.default_image, "node:24");
    }

    #[test]
    fn debug_hides_secrets() {
        let text = format!("{:?}", sample());
        for secret in [
            "vm-token-secret",
            "glrt-runner-secret",
            "cache-secret-key",
            "SCWACCESSKEYSAMPLE",
        ] {
            assert!(!text.contains(secret), "{text}");
        }
    }
}
