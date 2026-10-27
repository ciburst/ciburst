//! gitlab-runner in the cache bucket (D45, M6 rule R6), step `runner-binary`: the pinned runner
//! as the public object `ciburst/runner/<sha256>`, which lane runs and the VMs of every
//! provider download and check against its SHA-256. `apply` copies it from the upstream
//! download `cloud_init::RUNNER_URL` and refuses any other hash. Every pool's runs fetch it, so
//! the step exists whenever a pool does.

use ciburst_core::cloud_init::{RUNNER_SHA256, RUNNER_URL};
use ciburst_core::config::Config;
use ciburst_core::http::Request;
use ciburst_core::lane::RUNNER_PREFIX;
use ciburst_core::types::{Action, Step};
use ciburst_core::{Error, Result};
use sha2::{Digest, Sha256};

use super::s3::{self, TRANSFER_TIMEOUT};
use super::{PROVIDER_NAME, Scaleway, jobs};

pub const RUNNER_STEP: &str = "runner-binary";

/// `ciburst/runner/<sha256>`.
pub fn runner_key(sha256: &str) -> String {
    format!("{RUNNER_PREFIX}{sha256}")
}

/// The anonymous URL of the runner `sha256` in the cache bucket.
pub fn runner_url(config: &Config, region: &str, project_id: &str, sha256: &str) -> String {
    s3::public_url(
        &jobs::bucket_name(config, project_id),
        region,
        &runner_key(sha256),
    )
}

/// Where `apply` downloads the runner, and the SHA-256 it must have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RunnerSource {
    pub url: String,
    pub sha256: String,
}

impl Default for RunnerSource {
    fn default() -> Self {
        RunnerSource {
            url: RUNNER_URL.to_string(),
            sha256: RUNNER_SHA256.to_string(),
        }
    }
}

impl Scaleway {
    /// The runner `apply` copies into the bucket: `cloud_init::RUNNER_URL` with
    /// `cloud_init::RUNNER_SHA256` unless set. Lane definitions, VM user data and `shared_env`
    /// name the object of this SHA-256.
    pub fn with_runner_source(
        mut self,
        url: impl Into<String>,
        sha256: impl Into<String>,
    ) -> Scaleway {
        self.runner = RunnerSource {
            url: url.into(),
            sha256: sha256.into(),
        };
        self
    }

    /// The SHA-256 of the runner object lane runs and VMs download.
    pub(super) fn runner_sha256(&self) -> &str {
        &self.runner.sha256
    }

    /// Whether the cache bucket holds the runner: always with an S3 cache, which config
    /// validation requires, because the VM runner and every lane download it.
    pub(super) fn needs_runner(&self) -> bool {
        self.cache_on()
    }

    /// `noop` when the object exists, else `create`.
    pub(super) async fn runner_step(
        &self,
        project: &Option<String>,
        bucket_exists: bool,
    ) -> Result<Step> {
        let key = runner_key(&self.runner.sha256);
        let present = match project {
            Some(project_id) if bucket_exists => {
                self.s3(project_id)?
                    .object_exists(&self.bucket(project_id), &key)
                    .await?
            }
            _ => false,
        };
        let resource = format!("gitlab-runner {key}");
        Ok(if present {
            Step::new(RUNNER_STEP, Action::Noop, resource)
        } else {
            Step::new(RUNNER_STEP, Action::Create, resource)
                .with_detail(format!("copy of {}, public-read", self.runner.url))
        })
    }

    /// Downloads the runner and uploads it as a public object unless it is there; a download
    /// with another SHA-256 is refused before anything is written.
    pub(super) async fn apply_runner(&self, project: &Option<String>) -> Result<()> {
        let project_id = self.require_project(project)?;
        let s3 = self.s3(project_id)?;
        let bucket = self.bucket(project_id);
        let source = &self.runner;
        let key = runner_key(&source.sha256);
        if s3.object_exists(&bucket, &key).await? {
            return Ok(());
        }
        let request = Request::new("GET", source.url.as_str()).timeout(TRANSFER_TIMEOUT);
        let response = self.client.http().send(request).await?;
        if !response.ok() {
            return Err(Error::provider(format!(
                "{PROVIDER_NAME}: runner download {}: {}",
                source.url, response.status
            )));
        }
        let sha256 = hex::encode(Sha256::digest(&response.body));
        if sha256 != source.sha256 {
            return Err(Error::provider(format!(
                "{PROVIDER_NAME}: runner {} has SHA-256 {sha256}, expected {}",
                source.url, source.sha256
            )));
        }
        let bytes = response.body.len().to_string();
        s3.put_public_object(&bucket, &key, response.body).await?;
        self.log.info(
            "uploaded runner",
            &[
                ("bucket", &bucket),
                ("key", &key),
                ("bytes", &bytes),
                ("source", &source.url),
            ],
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::s3::LIFECYCLE_RULES;
    use ciburst_core::testing::contract::contract_config;

    #[test]
    fn the_runner_lives_in_the_bucket_below_a_prefix_nothing_expires() {
        let key = runner_key(RUNNER_SHA256);
        assert_eq!(key, format!("ciburst/runner/{RUNNER_SHA256}"));
        for (id, prefix) in LIFECYCLE_RULES {
            assert!(!key.starts_with(prefix), "{id} would expire the runner");
        }
        assert_eq!(
            runner_url(&contract_config(), "nl-ams", "p-1", RUNNER_SHA256),
            format!(
                "https://contract-cache-p-1.s3.nl-ams.scw.cloud/ciburst/runner/{RUNNER_SHA256}"
            )
        );
        assert_eq!(
            RunnerSource::default(),
            RunnerSource {
                url: RUNNER_URL.to_string(),
                sha256: RUNNER_SHA256.to_string(),
            }
        );
    }
}
