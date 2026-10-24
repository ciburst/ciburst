//! The user data `cloud-init` of a VM, the contract between provider and agent (rule 6 of M3):
//! `/etc/ciburst/vm.env`, the pinned `gitlab-runner`, the agent copied out of its image, and
//! the agent's systemd unit. The template and rendering live in `ciburst_core::cloud_init`.

use std::collections::BTreeMap;

use ciburst_core::config::{CacheType, Config, Pool};
use ciburst_core::{Error, Result};

use super::{PROVIDER_NAME, jobs, s3};

pub use ciburst_core::cloud_init::{AGENT_IMAGE, AGENT_IMAGE_KEY, DISPATCH_URL, RUN_ID, VM_TOKEN};

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

/// The full reference of the agent image below `registry_endpoint`.
pub fn agent_image(registry_endpoint: &str) -> Result<String> {
    if registry_endpoint.is_empty() {
        return Err(Error::provider(format!(
            "{PROVIDER_NAME}: no registry endpoint for the agent image"
        )));
    }
    Ok(format!("{registry_endpoint}/{AGENT_IMAGE}"))
}

/// The cloud-config document with the agent image below `registry_endpoint`.
/// `install_docker` is for the plain Ubuntu fallback image.
pub fn render(
    env: &BTreeMap<String, String>,
    registry_endpoint: &str,
    install_docker: bool,
) -> Result<String> {
    ciburst_core::cloud_init::render(env, &agent_image(registry_endpoint)?, install_docker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciburst_core::testing::contract::contract_config;

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
    fn render_refuses_unsafe_values() {
        let env = BTreeMap::from([("A".to_string(), "x\ny".to_string())]);
        assert!(render(&env, "rg.fr-par.scw.cloud/demo", false).is_err());
        let env = BTreeMap::from([("a b".to_string(), "x".to_string())]);
        assert!(render(&env, "rg.fr-par.scw.cloud/demo", false).is_err());
        let env = BTreeMap::from([("A".to_string(), "x".to_string())]);
        assert!(render(&env, "rg.example/x' y", false).is_err());
        assert!(render(&env, "", false).is_err());
        let text = render(&env, "rg.fr-par.scw.cloud/demo", true).unwrap();
        assert!(!text.contains("{{"), "{text}");
        assert!(text.starts_with("#cloud-config\n"));
    }
}
