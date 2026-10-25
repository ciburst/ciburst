//! The user data `cloud-init` of a VM, the contract between provider and agent (rule 6 of M3,
//! D38, M6 rule R6): `/etc/ciburst/vm.env`, the pinned `gitlab-runner` and the agent, both
//! downloaded from the cache bucket, and the agent's systemd unit. The template and rendering
//! live in `ciburst_core::cloud_init`.

use std::collections::BTreeMap;

use ciburst_core::config::{CacheType, Config, Pool};
use ciburst_core::{Error, Result};

use super::agent::agent_key;
use super::{PROVIDER_NAME, jobs, s3};

pub use ciburst_core::cloud_init::{
    AGENT_SHA256_KEY, AGENT_URL_KEY, DISPATCH_URL, Download, RUN_ID, RUNNER_URL_KEY, VM_TOKEN,
    render,
};

/// `vm.env` without the run id and token, which exist only once the server does. Fails before
/// anything is created when the request lacks a value the VM needs.
pub fn base_env(
    config: &Config,
    pool: &Pool,
    region: &str,
    project_id: &str,
    request: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let required = |key: &str| {
        request
            .get(key)
            .filter(|v| !v.is_empty())
            .cloned()
            .ok_or_else(|| {
                Error::provider(format!("{PROVIDER_NAME}: pool {}: {key} missing", pool.tag))
            })
    };
    let mut env = BTreeMap::from([
        (DISPATCH_URL.to_string(), required(DISPATCH_URL)?),
        ("CI_SERVER_URL".to_string(), config.gitlab.url.clone()),
        ("RUNNER_TOKEN".to_string(), required("RUNNER_TOKEN")?),
        ("CIBURST_POOL".to_string(), pool.tag.clone()),
        ("CIBURST_SLOTS".to_string(), pool.slots.to_string()),
        ("CIBURST_DEFAULT_IMAGE".to_string(), pool.image.clone()),
        ("CIBURST_TIMEOUT_S".to_string(), pool.timeout_s.to_string()),
    ]);
    if config.cache.kind == CacheType::S3 {
        env.extend(cache_location(config, region, project_id));
        for key in ["CACHE_S3_ACCESS_KEY", "CACHE_S3_SECRET_KEY"] {
            env.insert(key.to_string(), required(key)?);
        }
    }
    Ok(env)
}

/// Where the S3 cache lives, without its key: the part of `vm.env` that VMs of other providers
/// get through `shared_env` (M4 rule R11).
pub fn cache_location(config: &Config, region: &str, project_id: &str) -> BTreeMap<String, String> {
    [
        ("CACHE_TYPE", "s3".to_string()),
        ("CACHE_S3_SERVER_ADDRESS", s3::host(region)),
        (
            "CACHE_S3_BUCKET_NAME",
            jobs::bucket_name(config, project_id),
        ),
        ("CACHE_S3_BUCKET_LOCATION", region.to_string()),
        ("CACHE_SHARED", "true".to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

/// The anonymous URL of the agent `sha256` in the cache bucket (D38).
pub fn agent_url(config: &Config, region: &str, project_id: &str, sha256: &str) -> String {
    s3::public_url(
        &jobs::bucket_name(config, project_id),
        region,
        &agent_key(sha256),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::runner::runner_url;
    use ciburst_core::cloud_init::RUNNER_SHA256;
    use ciburst_core::testing::contract::contract_config;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn request() -> BTreeMap<String, String> {
        [
            ("RUNNER_TOKEN", "glrt-x"),
            ("CIBURST_DISPATCH_URL", "https://dispatch.example"),
            ("CACHE_S3_ACCESS_KEY", "AK"),
            ("CACHE_S3_SECRET_KEY", "SK"),
            ("CIBURST_VM_SECRET", "never-in-user-data"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    #[test]
    fn base_env_names_what_is_missing() {
        let config = contract_config();
        let pool = &config.pools[0];
        let env = base_env(&config, pool, "fr-par", "p-1", &request()).unwrap();
        assert_eq!(env["CACHE_S3_BUCKET_NAME"], "contract-cache-p-1");
        assert!(!env.contains_key("CIBURST_VM_SECRET"));
        for key in [
            "RUNNER_TOKEN",
            "CIBURST_DISPATCH_URL",
            "CACHE_S3_ACCESS_KEY",
            "CACHE_S3_SECRET_KEY",
        ] {
            let mut partial = request();
            partial.remove(key);
            let error = base_env(&config, pool, "fr-par", "p-1", &partial)
                .unwrap_err()
                .to_string();
            assert_eq!(error, format!("scaleway: pool ci: {key} missing"));
        }
    }

    #[test]
    fn agent_and_runner_urls_are_public_objects_in_the_cache_bucket() {
        let config = contract_config();
        let url = agent_url(&config, "nl-ams", "p-1", SHA);
        assert_eq!(
            url,
            format!("https://contract-cache-p-1.s3.nl-ams.scw.cloud/ciburst/agent/{SHA}")
        );
        let runner = runner_url(&config, "nl-ams", "p-1", RUNNER_SHA256);
        let text = render(
            &BTreeMap::new(),
            Download {
                url: &runner,
                sha256: RUNNER_SHA256,
            },
            Download {
                url: &url,
                sha256: SHA,
            },
            false,
        )
        .unwrap();
        assert!(text.contains(&format!("curl -fsSL -o /tmp/ciburst-vm-agent {url} ")));
        assert!(text.contains(&format!(
            "curl -fsSL -o /tmp/gitlab-runner https://contract-cache-p-1.s3.nl-ams.scw.cloud/ciburst/runner/{RUNNER_SHA256} "
        )));
    }
}
