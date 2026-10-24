//! The user data `cloud-init` of a VM, the contract between provider and agent (rule 6 of M3):
//! `/etc/ciburst/vm.env`, the pinned `gitlab-runner`, the agent copied out of its image, and
//! the agent's systemd unit. The template is `cloud-init.yaml` next to this file.

use std::collections::BTreeMap;

use ciburst_core::config::{CacheType, Config, Pool};
use ciburst_core::{Error, Result};

use super::{PROVIDER_NAME, jobs, s3};

pub const RUNNER_URL: &str =
    "https://gitlab-runner-downloads.s3.amazonaws.com/v18.11.4/binaries/gitlab-runner-linux-amd64";
/// From `v18.11.4/release.sha256`, matched against the downloaded binary on 2026-10-24.
pub const RUNNER_SHA256: &str = "bed4af01315780cf27d259b9d7a74d20cc0a6fc0d34c9952205b59c244639fd1";
/// The agent image below the registry endpoint, built by CI.
pub const AGENT_IMAGE: &str = "vm-agent:edge";

pub const RUN_ID: &str = "CIBURST_RUN_ID";
pub const VM_TOKEN: &str = "CIBURST_VM_TOKEN";
pub const DISPATCH_URL: &str = "CIBURST_DISPATCH_URL";

const TEMPLATE: &str = include_str!("cloud-init.yaml");

/// Ubuntu without the marketplace Docker image needs the daemon first.
const INSTALL_DOCKER: &str = "  - 'apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y docker.io && systemctl enable --now docker'\n";

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
        for (key, value) in [
            ("CACHE_TYPE", "s3".to_string()),
            ("CACHE_S3_SERVER_ADDRESS", s3::host(region)),
            (
                "CACHE_S3_BUCKET_NAME",
                jobs::bucket_name(config, project_id),
            ),
            ("CACHE_S3_BUCKET_LOCATION", region.to_string()),
            ("CACHE_S3_ACCESS_KEY", required("CACHE_S3_ACCESS_KEY")?),
            ("CACHE_S3_SECRET_KEY", required("CACHE_S3_SECRET_KEY")?),
            ("CACHE_SHARED", "true".to_string()),
        ] {
            env.insert(key.to_string(), value);
        }
    }
    Ok(env)
}

/// The cloud-config document. `install_docker` is for the plain Ubuntu fallback image.
pub fn render(
    env: &BTreeMap<String, String>,
    registry_endpoint: &str,
    install_docker: bool,
) -> Result<String> {
    for (key, value) in env {
        let key_ok = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !key_ok || value.chars().any(char::is_control) {
            return Err(Error::provider(format!(
                "{PROVIDER_NAME}: {key} cannot be written to vm.env"
            )));
        }
    }
    let image = format!("{registry_endpoint}/{AGENT_IMAGE}");
    let image_ok = !registry_endpoint.is_empty()
        && image
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "./:_-".contains(c));
    if !image_ok {
        return Err(Error::provider(format!(
            "{PROVIDER_NAME}: registry endpoint {registry_endpoint} is not an image reference"
        )));
    }
    let vm_env: Vec<String> = env.iter().map(|(k, v)| format!("      {k}={v}")).collect();
    Ok(TEMPLATE
        .replace("{{VM_ENV}}", &vm_env.join("\n"))
        .replace(
            "{{INSTALL_DOCKER}}",
            if install_docker { INSTALL_DOCKER } else { "" },
        )
        .replace("{{RUNNER_URL}}", RUNNER_URL)
        .replace("{{RUNNER_SHA256}}", RUNNER_SHA256)
        .replace("{{AGENT_IMAGE}}", &image))
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
