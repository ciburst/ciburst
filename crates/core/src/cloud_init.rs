//! The user data `cloud-init` of a VM, shared by every VM provider (spec section 6.1, D38): a
//! swap file, `/etc/ciburst/vm.env`, the pinned `gitlab-runner`, the agent downloaded from the
//! cache bucket and checked against its SHA-256, and the agent's systemd unit. The template is
//! `cloud-init.yaml` next to this file.

use std::collections::BTreeMap;

use crate::provision::keys;
use crate::{Error, Result};

pub const RUNNER_URL: &str =
    "https://gitlab-runner-downloads.s3.amazonaws.com/v18.11.4/binaries/gitlab-runner-linux-amd64";
/// From `v18.11.4/release.sha256`, matched against the downloaded binary on 2026-10-24.
pub const RUNNER_SHA256: &str = "bed4af01315780cf27d259b9d7a74d20cc0a6fc0d34c9952205b59c244639fd1";
/// Size of the swap file: two slots on 4 GB degrade instead of hitting the OOM killer.
pub const SWAP: &str = "4G";

pub const RUN_ID: &str = "CIBURST_RUN_ID";
pub const VM_TOKEN: &str = "CIBURST_VM_TOKEN";
pub const DISPATCH_URL: &str = "CIBURST_DISPATCH_URL";
/// `shared_env` key with the HTTPS URL a VM downloads the agent from (D38).
pub const AGENT_URL_KEY: &str = "CIBURST_AGENT_URL";
/// `shared_env` key with the agent's SHA-256, which cloud-init checks.
pub const AGENT_SHA256_KEY: &str = keys::AGENT_SHA256;

const TEMPLATE: &str = include_str!("cloud-init.yaml");

/// Ubuntu without Docker needs the daemon first.
const INSTALL_DOCKER: &str = "  - 'apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y docker.io && systemctl enable --now docker'\n";

/// Whether `value` is a SHA-256 in lower-case hex.
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// An `https://` URL that the single-quoted shell line of the template can carry as is.
fn is_agent_url(value: &str) -> bool {
    value.strip_prefix("https://").is_some_and(|rest| {
        rest.contains('/')
            && !rest.starts_with('/')
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"./_-".contains(&b))
    })
}

/// The cloud-config document. The VM downloads the agent from `agent_url` and installs it only
/// when its SHA-256 is `agent_sha256`; `install_docker` is for images without Docker.
pub fn render(
    env: &BTreeMap<String, String>,
    agent_url: &str,
    agent_sha256: &str,
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
    if !is_agent_url(agent_url) {
        return Err(Error::provider(format!(
            "cloud-init: {AGENT_URL_KEY} {agent_url:?} is not an https URL"
        )));
    }
    if !is_sha256(agent_sha256) {
        return Err(Error::provider(format!(
            "cloud-init: {AGENT_SHA256_KEY} {agent_sha256:?} is not a SHA-256 in hex"
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
        .replace("{{AGENT_URL}}", agent_url)
        .replace("{{AGENT_SHA256}}", agent_sha256))
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://demo-cache-p-1.s3.fr-par.scw.cloud/ciburst/agent/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn runcmd(text: &str) -> Vec<&str> {
        text.split("runcmd:\n").nth(1).unwrap().lines().collect()
    }

    #[test]
    fn render_refuses_unsafe_values() {
        let env = BTreeMap::from([("A".to_string(), "x\ny".to_string())]);
        assert!(render(&env, URL, SHA, false).is_err());
        let env = BTreeMap::from([("a b".to_string(), "x".to_string())]);
        assert!(render(&env, URL, SHA, false).is_err());
        let env = BTreeMap::from([("A".to_string(), "x".to_string())]);
        for url in [
            "",
            "http://demo.s3.fr-par.scw.cloud/ciburst/agent/x",
            "https://demo.s3.fr-par.scw.cloud",
            "https:///ciburst/agent/x",
            "https://demo.s3.fr-par.scw.cloud/a' b",
            "https://demo.s3.fr-par.scw.cloud/a;reboot",
            "https://demo.s3.fr-par.scw.cloud/$(id)",
        ] {
            let error = render(&env, url, SHA, false).unwrap_err().to_string();
            assert!(error.contains("CIBURST_AGENT_URL"), "{url}: {error}");
        }
        for sha in [
            "",
            &SHA[1..],
            &SHA.to_uppercase(),
            &format!("{}g", &SHA[1..]),
        ] {
            let error = render(&env, URL, sha, false).unwrap_err().to_string();
            assert!(error.contains("CIBURST_AGENT_SHA256"), "{sha}: {error}");
        }
        let text = render(&env, URL, SHA, true).unwrap();
        assert!(!text.contains("{{"), "{text}");
        assert!(text.starts_with("#cloud-config\nssh_pwauth: false\n"));
    }

    #[test]
    fn swap_comes_first_and_runs_once() {
        let text = render(&BTreeMap::new(), URL, SHA, false).unwrap();
        assert_eq!(
            runcmd(&text)[0],
            "  - 'test -f /swapfile || (fallocate -l 4G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile)'"
        );
    }

    #[test]
    fn the_agent_is_downloaded_checked_and_installed_without_docker() {
        let text = render(&BTreeMap::new(), URL, SHA, false).unwrap();
        let commands = runcmd(&text);
        assert_eq!(commands.len(), 4, "{commands:?}");
        assert!(commands[1].starts_with("  - 'curl -fsSL -o /tmp/gitlab-runner "));
        assert_eq!(
            commands[2],
            format!(
                "  - 'curl -fsSL -o /tmp/ciburst-vm-agent {URL} && echo \"{SHA}  /tmp/ciburst-vm-agent\" | sha256sum -c - && install -m 0755 /tmp/ciburst-vm-agent /usr/local/bin/ciburst-vm-agent && rm -f /tmp/ciburst-vm-agent'"
            )
        );
        assert_eq!(
            commands[3],
            "  - 'systemctl daemon-reload && systemctl enable --now ciburst-vm-agent.service'"
        );
        assert!(!text.contains("docker create"));
        assert!(!text.contains("docker cp"));
        let with_docker = render(&BTreeMap::new(), URL, SHA, true).unwrap();
        assert!(runcmd(&with_docker)[1].contains("apt-get install -y docker.io"));
    }

    #[test]
    fn sha256_is_lower_case_hex_of_64() {
        assert!(is_sha256(SHA));
        assert!(!is_sha256(&SHA.to_uppercase()));
        assert!(!is_sha256(&SHA[..63]));
        assert!(!is_sha256(&format!("{SHA}0")));
    }
}
