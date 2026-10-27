//! The user data `cloud-init` of a VM, shared by every VM provider (spec section 6.1, D38, spec
//! 9.1): a swap file, `/etc/ciburst/vm.env`, the pinned `gitlab-runner` and the agent, both
//! downloaded from the cache bucket and checked against their SHA-256, and the agent's systemd
//! unit. The template is `cloud-init.yaml` next to this file.

use std::collections::BTreeMap;

use crate::provision::keys;
use crate::{Error, Result, lane};

/// The upstream download of the pinned runner. `apply` copies it into the cache bucket (spec
/// 9.1); VMs and lane runs download the copy, never this URL.
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
/// `shared_env` key with the HTTPS URL of the runner object in the cache bucket (spec 9.1).
pub const RUNNER_URL_KEY: &str = lane::RUNNER_URL_KEY;
/// The name a runner SHA-256 goes by in errors, as in a lane run's environment.
pub const RUNNER_SHA256_KEY: &str = lane::RUNNER_SHA256_KEY;

const TEMPLATE: &str = include_str!("cloud-init.yaml");

/// Ubuntu without Docker needs the daemon first.
const INSTALL_DOCKER: &str = "  - 'apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y docker.io && systemctl enable --now docker'\n";

/// A binary the VM downloads, and the SHA-256 it installs it only with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Download<'a> {
    pub url: &'a str,
    pub sha256: &'a str,
}

/// Whether `value` is a SHA-256 in lower-case hex.
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// An `https://` URL that the single-quoted shell line of the template can carry as is.
fn is_download_url(value: &str) -> bool {
    value.strip_prefix("https://").is_some_and(|rest| {
        rest.contains('/')
            && !rest.starts_with('/')
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"./_-".contains(&b))
    })
}

/// Fails naming `url_key` or `sha256_key` when the download cannot go into the template.
fn check(download: Download<'_>, url_key: &str, sha256_key: &str) -> Result<()> {
    if !is_download_url(download.url) {
        return Err(Error::provider(format!(
            "cloud-init: {url_key} {:?} is not an https URL",
            download.url
        )));
    }
    if !is_sha256(download.sha256) {
        return Err(Error::provider(format!(
            "cloud-init: {sha256_key} {:?} is not a SHA-256 in hex",
            download.sha256
        )));
    }
    Ok(())
}

/// The cloud-config document. The VM downloads `runner` and `agent` and installs each only
/// when its SHA-256 matches; `install_docker` is for images without Docker.
pub fn render(
    env: &BTreeMap<String, String>,
    runner: Download<'_>,
    agent: Download<'_>,
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
    check(runner, RUNNER_URL_KEY, RUNNER_SHA256_KEY)?;
    check(agent, AGENT_URL_KEY, AGENT_SHA256_KEY)?;
    let vm_env: Vec<String> = env.iter().map(|(k, v)| format!("      {k}={v}")).collect();
    Ok(TEMPLATE
        .replace("{{VM_ENV}}", &vm_env.join("\n"))
        .replace(
            "{{INSTALL_DOCKER}}",
            if install_docker { INSTALL_DOCKER } else { "" },
        )
        .replace("{{SWAP}}", SWAP)
        .replace("{{RUNNER_URL}}", runner.url)
        .replace("{{RUNNER_SHA256}}", runner.sha256)
        .replace("{{AGENT_URL}}", agent.url)
        .replace("{{AGENT_SHA256}}", agent.sha256))
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://demo-cache-p-1.s3.fr-par.scw.cloud/ciburst/agent/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const RUNNER: &str = "https://demo-cache-p-1.s3.fr-par.scw.cloud/ciburst/runner/bed4af01315780cf27d259b9d7a74d20cc0a6fc0d34c9952205b59c244639fd1";

    fn runner() -> Download<'static> {
        Download {
            url: RUNNER,
            sha256: RUNNER_SHA256,
        }
    }

    fn agent() -> Download<'static> {
        Download {
            url: URL,
            sha256: SHA,
        }
    }

    fn runcmd(text: &str) -> Vec<&str> {
        text.split("runcmd:\n").nth(1).unwrap().lines().collect()
    }

    const UNSAFE_URLS: [&str; 7] = [
        "",
        "http://demo.s3.fr-par.scw.cloud/ciburst/agent/x",
        "https://demo.s3.fr-par.scw.cloud",
        "https:///ciburst/agent/x",
        "https://demo.s3.fr-par.scw.cloud/a' b",
        "https://demo.s3.fr-par.scw.cloud/a;reboot",
        "https://demo.s3.fr-par.scw.cloud/$(id)",
    ];

    fn unsafe_hashes() -> [String; 4] {
        [
            String::new(),
            SHA[1..].to_string(),
            SHA.to_uppercase(),
            format!("{}g", &SHA[1..]),
        ]
    }

    #[test]
    fn render_refuses_unsafe_values() {
        let env = BTreeMap::from([("A".to_string(), "x\ny".to_string())]);
        assert!(render(&env, runner(), agent(), false).is_err());
        let env = BTreeMap::from([("a b".to_string(), "x".to_string())]);
        assert!(render(&env, runner(), agent(), false).is_err());
        let env = BTreeMap::from([("A".to_string(), "x".to_string())]);
        for url in UNSAFE_URLS {
            let bad = Download { url, ..agent() };
            let error = render(&env, runner(), bad, false).unwrap_err().to_string();
            assert!(error.contains("CIBURST_AGENT_URL"), "{url}: {error}");
            let bad = Download { url, ..runner() };
            let error = render(&env, bad, agent(), false).unwrap_err().to_string();
            assert!(error.contains("CIBURST_RUNNER_URL"), "{url}: {error}");
        }
        for sha in unsafe_hashes() {
            let bad = Download {
                sha256: &sha,
                ..agent()
            };
            let error = render(&env, runner(), bad, false).unwrap_err().to_string();
            assert!(error.contains("CIBURST_AGENT_SHA256"), "{sha}: {error}");
            let bad = Download {
                sha256: &sha,
                ..runner()
            };
            let error = render(&env, bad, agent(), false).unwrap_err().to_string();
            assert!(error.contains("CIBURST_RUNNER_SHA256"), "{sha}: {error}");
        }
        let text = render(&env, runner(), agent(), true).unwrap();
        assert!(!text.contains("{{"), "{text}");
        assert!(text.starts_with("#cloud-config\nssh_pwauth: false\n"));
    }

    #[test]
    fn swap_comes_first_and_runs_once() {
        let text = render(&BTreeMap::new(), runner(), agent(), false).unwrap();
        assert_eq!(
            runcmd(&text)[0],
            "  - 'test -f /swapfile || (fallocate -l 4G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile)'"
        );
    }

    #[test]
    fn runner_and_agent_are_downloaded_checked_and_installed_without_docker() {
        let text = render(&BTreeMap::new(), runner(), agent(), false).unwrap();
        let commands = runcmd(&text);
        assert_eq!(commands.len(), 4, "{commands:?}");
        assert_eq!(
            commands[1],
            format!(
                "  - 'curl -fsSL -o /tmp/gitlab-runner {RUNNER} && echo \"{RUNNER_SHA256}  /tmp/gitlab-runner\" | sha256sum -c - && install -m 0755 /tmp/gitlab-runner /usr/local/bin/gitlab-runner && rm -f /tmp/gitlab-runner'"
            )
        );
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
        assert!(
            !text.contains(RUNNER_URL),
            "the upstream URL is apply's only"
        );
        assert!(!text.contains("docker create"));
        assert!(!text.contains("docker cp"));
        let with_docker = render(&BTreeMap::new(), runner(), agent(), true).unwrap();
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
