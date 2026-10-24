//! The user data `cloud-init` of a VM, shared by every VM provider (spec section 6.1, M4 rule
//! R10): a swap file, `/etc/ciburst/vm.env`, the pinned `gitlab-runner`, the agent copied out
//! of its image, and the agent's systemd unit. The template is `cloud-init.yaml` next to this
//! file.

use std::collections::BTreeMap;

use crate::{Error, Result};

pub const RUNNER_URL: &str =
    "https://gitlab-runner-downloads.s3.amazonaws.com/v18.11.4/binaries/gitlab-runner-linux-amd64";
/// From `v18.11.4/release.sha256`, matched against the downloaded binary on 2026-10-24.
pub const RUNNER_SHA256: &str = "bed4af01315780cf27d259b9d7a74d20cc0a6fc0d34c9952205b59c244639fd1";
/// The agent image below a registry endpoint, built by CI.
pub const AGENT_IMAGE: &str = "vm-agent:edge";
/// Size of the swap file: two slots on 4 GB degrade instead of hitting the OOM killer.
pub const SWAP: &str = "4G";

pub const RUN_ID: &str = "CIBURST_RUN_ID";
pub const VM_TOKEN: &str = "CIBURST_VM_TOKEN";
pub const DISPATCH_URL: &str = "CIBURST_DISPATCH_URL";
/// `shared_env` key with the full agent image reference (M4 rule R11).
pub const AGENT_IMAGE_KEY: &str = "CIBURST_AGENT_IMAGE";

const TEMPLATE: &str = include_str!("cloud-init.yaml");

/// Ubuntu without Docker needs the daemon first.
const INSTALL_DOCKER: &str = "  - 'apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y docker.io && systemctl enable --now docker'\n";

/// The cloud-config document. `agent_image` is a full image reference; `install_docker` is for
/// images without Docker.
pub fn render(
    env: &BTreeMap<String, String>,
    agent_image: &str,
    install_docker: bool,
) -> Result<String> {
    for (key, value) in env {
        let key_ok = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !key_ok || value.chars().any(char::is_control) {
            return Err(Error::provider(format!(
                "cloud-init: {key} cannot be written to vm.env"
            )));
        }
    }
    let image_ok = agent_image.contains('/')
        && !agent_image.starts_with('/')
        && agent_image
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "./:_-".contains(c));
    if !image_ok {
        return Err(Error::provider(format!(
            "cloud-init: {agent_image} is not an image reference"
        )));
    }
    let vm_env: Vec<String> = env.iter().map(|(k, v)| format!("      {k}={v}")).collect();
    Ok(TEMPLATE
        .replace("{{VM_ENV}}", &vm_env.join("\n"))
        .replace(
            "{{INSTALL_DOCKER}}",
            if install_docker { INSTALL_DOCKER } else { "" },
        )
        .replace("{{SWAP}}", SWAP)
        .replace("{{RUNNER_URL}}", RUNNER_URL)
        .replace("{{RUNNER_SHA256}}", RUNNER_SHA256)
        .replace("{{AGENT_IMAGE}}", agent_image))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE: &str = "rg.fr-par.scw.cloud/demo/vm-agent:edge";

    #[test]
    fn render_refuses_unsafe_values() {
        let env = BTreeMap::from([("A".to_string(), "x\ny".to_string())]);
        assert!(render(&env, IMAGE, false).is_err());
        let env = BTreeMap::from([("a b".to_string(), "x".to_string())]);
        assert!(render(&env, IMAGE, false).is_err());
        let env = BTreeMap::from([("A".to_string(), "x".to_string())]);
        assert!(render(&env, "rg.example/x' y", false).is_err());
        assert!(render(&env, "", false).is_err());
        assert!(render(&env, "/vm-agent:edge", false).is_err());
        let text = render(&env, IMAGE, true).unwrap();
        assert!(!text.contains("{{"), "{text}");
        assert!(text.starts_with("#cloud-config\nssh_pwauth: false\n"));
    }

    #[test]
    fn swap_comes_first_and_runs_once() {
        let text = render(&BTreeMap::new(), IMAGE, false).unwrap();
        let runcmd = text.split("runcmd:\n").nth(1).unwrap();
        let first = runcmd.lines().next().unwrap();
        assert_eq!(
            first,
            "  - 'test -f /swapfile || (fallocate -l 4G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile)'"
        );
    }
}
