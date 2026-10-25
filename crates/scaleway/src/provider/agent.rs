//! The VM agent in the cache bucket (D38), step `vm:agent`: the Linux musl build of
//! `ciburst-vm-agent` as the public object `ciburst/agent/<sha256>`, which cloud-init downloads
//! and checks against the SHA-256 of the user data. Without a binary the newest uploaded agent
//! stays current. VMs of every provider fetch it, so the step exists whenever a pool runs on
//! VMs.

use ciburst_core::cloud_init::is_sha256;
use ciburst_core::types::{Action, Step};
use ciburst_core::{Error, Result};
use sha2::{Digest, Sha256};

use super::s3::{Listed, S3};
use super::{PROVIDER_NAME, Runtime, Scaleway};

pub const AGENT_STEP: &str = "vm:agent";
/// Every agent lives below this prefix, which no lifecycle rule expires.
pub const AGENT_PREFIX: &str = "ciburst/agent/";
/// Why no VM could start: neither a binary nor an uploaded agent.
pub const NO_AGENT: &str =
    "no VM agent: pass --agent <path> or build one with scripts/agent-binary.sh";

/// Listing pages read at most while looking for the newest agent.
const MAX_LIST_PAGES: usize = 100;
/// The first bytes of an ELF executable; a Linux VM runs nothing else.
const ELF_MAGIC: &[u8] = b"\x7fELF";

/// `ciburst/agent/<sha256>`.
pub fn agent_key(sha256: &str) -> String {
    format!("{AGENT_PREFIX}{sha256}")
}

/// The agent binary given with `--agent`: its bytes and SHA-256 in hex.
struct Binary {
    bytes: Vec<u8>,
    sha256: String,
}

/// The newest agent of a listing below [`AGENT_PREFIX`]; keys that are no SHA-256 are skipped.
fn newest(objects: Vec<Listed>) -> Option<String> {
    objects
        .into_iter()
        .filter_map(|object| {
            let sha256 = object.key.strip_prefix(AGENT_PREFIX)?;
            is_sha256(sha256).then(|| (object.modified, sha256.to_string()))
        })
        .max()
        .map(|(_, sha256)| sha256)
}

impl Scaleway {
    /// The agent of the function's environment (`CIBURST_AGENT_SHA256`), which VMs started by
    /// the dispatcher download. The dispatcher cannot list the bucket, so it needs this.
    pub fn with_agent_sha256(mut self, sha256: Option<String>) -> Scaleway {
        self.agent_sha256 = sha256.filter(|s| !s.is_empty());
        self
    }

    /// Whether a pool runs on VMs: an own pool on Instances or a pool of another provider,
    /// whose VMs fetch the agent from this bucket too.
    pub(super) fn needs_agent(&self) -> Result<bool> {
        let other_pools = self.own_pools().count() < self.config.pools.len();
        let own_vms = self
            .own_runtimes()?
            .iter()
            .any(|(_, runtime)| *runtime == Runtime::Instances);
        Ok(own_vms || other_pools)
    }

    fn binary(&self) -> Result<Option<Binary>> {
        let Some(path) = &self.agent else {
            return Ok(None);
        };
        let bytes = std::fs::read(path).map_err(|error| {
            Error::provider(format!(
                "{PROVIDER_NAME}: agent {}: {error}",
                path.display()
            ))
        })?;
        if !bytes.starts_with(ELF_MAGIC) {
            return Err(Error::provider(format!(
                "{PROVIDER_NAME}: agent {} is not a Linux executable; build it with \
                 scripts/agent-binary.sh",
                path.display()
            )));
        }
        let sha256 = hex::encode(Sha256::digest(&bytes));
        Ok(Some(Binary { bytes, sha256 }))
    }

    async fn newest_uploaded(&self, s3: &S3, bucket: &str) -> Result<Option<String>> {
        Ok(newest(
            s3.list_all(bucket, AGENT_PREFIX, MAX_LIST_PAGES).await?,
        ))
    }

    /// The newest agent in the bucket; `None` without project, bucket or agent.
    async fn uploaded_agent(&self, project: &Option<String>) -> Result<Option<String>> {
        let Some(project_id) = project else {
            return Ok(None);
        };
        let s3 = self.s3()?;
        let bucket = self.bucket(project_id);
        if !s3.bucket_exists(&bucket).await? {
            return Ok(None);
        }
        self.newest_uploaded(&s3, &bucket).await
    }

    /// `create` when the binary's object is missing, else `noop`; without a binary `noop` on
    /// the newest uploaded agent, or a `create` that `apply` refuses when there is none.
    pub(super) async fn agent_step(
        &self,
        project: &Option<String>,
        bucket_exists: bool,
    ) -> Result<Step> {
        let resource = |sha256: &str| format!("vm agent {}", agent_key(sha256));
        if let Some(binary) = self.binary()? {
            let present = match project {
                Some(project_id) if bucket_exists => {
                    self.s3()?
                        .object_exists(&self.bucket(project_id), &agent_key(&binary.sha256))
                        .await?
                }
                _ => false,
            };
            let step = if present {
                Step::new(AGENT_STEP, Action::Noop, resource(&binary.sha256))
            } else {
                Step::new(AGENT_STEP, Action::Create, resource(&binary.sha256))
                    .with_detail("upload, public-read")
            };
            return Ok(step);
        }
        let uploaded = if bucket_exists {
            self.uploaded_agent(project).await?
        } else {
            None
        };
        Ok(match uploaded {
            Some(sha256) => Step::new(AGENT_STEP, Action::Noop, resource(&sha256))
                .with_detail("newest uploaded agent"),
            None => {
                self.log
                    .warn("vm pools cannot start", &[("reason", NO_AGENT)]);
                Step::new(
                    AGENT_STEP,
                    Action::Create,
                    format!("vm agent {AGENT_PREFIX}"),
                )
                .with_detail(NO_AGENT)
            }
        })
    }

    /// Uploads the binary as a public object unless it is there; answers the agent's SHA-256.
    pub(super) async fn apply_agent(&self, project: &Option<String>) -> Result<String> {
        let project_id = self.require_project(project)?;
        let s3 = self.s3()?;
        let bucket = self.bucket(project_id);
        let Some(binary) = self.binary()? else {
            return self
                .newest_uploaded(&s3, &bucket)
                .await?
                .ok_or_else(|| Error::provider(format!("{PROVIDER_NAME}: {NO_AGENT}")));
        };
        let key = agent_key(&binary.sha256);
        if !s3.object_exists(&bucket, &key).await? {
            let bytes = binary.bytes.len().to_string();
            s3.put_public_object(&bucket, &key, binary.bytes).await?;
            self.log.info(
                "uploaded agent",
                &[("bucket", &bucket), ("key", &key), ("bytes", &bytes)],
            );
        }
        Ok(binary.sha256)
    }

    /// The agent `apply` reports as `CIBURST_AGENT_SHA256`: the binary's, else the newest
    /// uploaded one.
    pub(super) async fn current_agent(&self, project: &Option<String>) -> Result<Option<String>> {
        if let Some(binary) = self.binary()? {
            return Ok(Some(binary.sha256));
        }
        self.uploaded_agent(project).await
    }

    /// Whether `apply` would leave an agent for VMs to fetch.
    pub(super) async fn agent_available(&self) -> Result<bool> {
        if self.binary()?.is_some() {
            return Ok(true);
        }
        let project = self.client.find_project(self.name()).await?;
        Ok(self.uploaded_agent(&project).await?.is_some())
    }

    /// The agent a new VM downloads: the function's `CIBURST_AGENT_SHA256`, else the binary's.
    /// A start never lists the bucket.
    pub(super) fn start_agent(&self) -> Result<Option<String>> {
        if let Some(sha256) = &self.agent_sha256 {
            return Ok(Some(sha256.clone()));
        }
        Ok(self.binary()?.map(|binary| binary.sha256))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(key: &str, modified: Option<u64>) -> Listed {
        Listed {
            key: key.to_string(),
            modified,
        }
    }

    #[test]
    fn the_newest_sha256_key_wins() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        assert_eq!(newest(Vec::new()), None);
        assert_eq!(
            newest(vec![
                listed(&agent_key(&b), Some(20)),
                listed(&agent_key(&a), Some(30)),
                listed(&agent_key(&c), None),
                listed("ciburst/agent/latest", Some(99)),
                listed(&format!("{}/x", agent_key(&c)), Some(99)),
            ]),
            Some(a.clone())
        );
        assert_eq!(
            newest(vec![
                listed(&agent_key(&a), Some(30)),
                listed(&agent_key(&b), Some(30))
            ]),
            Some(b),
            "the same second: the larger key, so every reader picks the same"
        );
        assert_eq!(agent_key(&a), format!("ciburst/agent/{a}"));
    }
}
