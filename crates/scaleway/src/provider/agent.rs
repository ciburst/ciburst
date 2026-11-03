//! The VM agent in the cache bucket (D38), step `vm:agent`: the Linux musl build of
//! `ciburst-vm-agent` as the public object `ciburst/agent/<sha256>`, which cloud-init downloads
//! and checks against the SHA-256 of the user data. Without a binary the newest uploaded agent
//! stays current. VMs of every provider fetch it, so the step exists whenever a pool runs on
//! VMs. Older agents beyond the two newest others are deleted, each by a step
//! `vm:agent:<sha256>` (D69).

use ciburst_core::cloud_init::is_sha256;
use ciburst_core::types::{Action, Step};
use ciburst_core::{Error, Result};
use sha2::{Digest, Sha256};

use super::s3::{Listed, S3};
use super::{PROVIDER_NAME, Runtime, Scaleway};

pub const AGENT_STEP: &str = "vm:agent";
/// The step that deletes an older agent: `vm:agent:<sha256>`.
pub const OLD_AGENT_STEP: &str = "vm:agent:";
/// Every agent lives below this prefix, which no lifecycle rule expires.
pub const AGENT_PREFIX: &str = "ciburst/agent/";
/// Why no VM could start: neither a binary nor an uploaded agent.
pub const NO_AGENT: &str =
    "no VM agent: pass --agent <path> or build one with scripts/agent-binary.sh";
/// Why the VM runner could not start without an S3 cache.
pub const NO_CACHE: &str =
    "the VM runner needs cache.type s3: VMs fetch their agent from the cache bucket";

/// Listing pages read at most while looking for the newest agent.
const MAX_LIST_PAGES: usize = 100;
/// Agents kept besides the current one (D69): a VM boots with the agent the function names,
/// which is the previous apply's, or the one before when an apply stopped before its function
/// update.
const KEPT_OLDER: usize = 2;
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

/// The agents of a listing below [`AGENT_PREFIX`], newest first, and in the same second the
/// larger key first, so every reader orders them alike; keys that are no SHA-256 are skipped.
fn by_age(objects: Vec<Listed>) -> Vec<String> {
    let mut agents: Vec<(Option<u64>, String)> = objects
        .into_iter()
        .filter_map(|object| {
            let sha256 = object.key.strip_prefix(AGENT_PREFIX)?;
            is_sha256(sha256).then(|| (object.modified, sha256.to_string()))
        })
        .collect();
    agents.sort_unstable_by(|a, b| b.cmp(a));
    agents.into_iter().map(|(_, sha256)| sha256).collect()
}

/// The newest agent of a listing below [`AGENT_PREFIX`].
fn newest(objects: Vec<Listed>) -> Option<String> {
    by_age(objects).into_iter().next()
}

/// The agents to delete, given newest first: all but `current` and the [`KEPT_OLDER`] newest
/// others.
fn stale(agents: &[String], current: &str) -> Vec<String> {
    agents
        .iter()
        .filter(|sha256| *sha256 != current)
        .skip(KEPT_OLDER)
        .cloned()
        .collect()
}

impl Scaleway {
    /// The agent of the function's environment (`CIBURST_AGENT_SHA256`), which VMs started by
    /// the dispatcher download. The dispatcher cannot list the bucket, so it needs this.
    pub fn with_agent_sha256(mut self, sha256: Option<String>) -> Scaleway {
        self.agent_sha256 = sha256.filter(|s| !s.is_empty());
        self
    }

    /// The tags of the run specs that run on VMs: the VM runner's.
    pub(super) fn vm_pools(&self) -> Result<Vec<String>> {
        Ok(self
            .pools_on(Runtime::Instances)?
            .into_iter()
            .map(|pool| pool.tag)
            .collect())
    }

    /// Whether a pool runs on VMs, so the cache bucket holds the agent.
    pub(super) fn needs_agent(&self) -> Result<bool> {
        Ok(!self.vm_pools()?.is_empty())
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
        let s3 = self.s3(project_id)?;
        let bucket = self.bucket(project_id);
        if !s3.bucket_exists(&bucket).await? {
            return Ok(None);
        }
        self.newest_uploaded(&s3, &bucket).await
    }

    /// The step `vm:agent`: `create` when the binary's object is missing, else `noop`; without
    /// a binary `noop` on the newest uploaded agent, or a `create` that `apply` refuses when
    /// there is none. Then a `delete` step `vm:agent:<sha256>` per [`stale`] agent.
    pub(super) async fn agent_steps(
        &self,
        project: &Option<String>,
        bucket_exists: bool,
    ) -> Result<Vec<Step>> {
        let resource = |sha256: &str| format!("vm agent {}", agent_key(sha256));
        let agents = match project {
            Some(project_id) if bucket_exists => by_age(
                self.s3(project_id)?
                    .list_all(&self.bucket(project_id), AGENT_PREFIX, MAX_LIST_PAGES)
                    .await?,
            ),
            _ => Vec::new(),
        };
        let (step, current) = if let Some(binary) = self.binary()? {
            let step = if agents.contains(&binary.sha256) {
                Step::new(AGENT_STEP, Action::Noop, resource(&binary.sha256))
            } else {
                Step::new(AGENT_STEP, Action::Create, resource(&binary.sha256))
                    .with_detail("upload, public-read")
            };
            (step, binary.sha256)
        } else if let Some(sha256) = agents.first() {
            let step = Step::new(AGENT_STEP, Action::Noop, resource(sha256))
                .with_detail("newest uploaded agent");
            (step, sha256.clone())
        } else {
            self.log
                .warn("vm runner cannot start", &[("reason", NO_AGENT)]);
            let step = Step::new(
                AGENT_STEP,
                Action::Create,
                format!("vm agent {AGENT_PREFIX}"),
            )
            .with_detail(NO_AGENT);
            return Ok(vec![step]);
        };
        let mut steps = vec![step];
        for sha256 in stale(&agents, &current) {
            steps.push(Step::new(
                format!("{OLD_AGENT_STEP}{sha256}"),
                Action::Delete,
                resource(&sha256),
            ));
        }
        Ok(steps)
    }

    /// Deletes the agent of a step `vm:agent:<sha256>`; nothing without a project.
    pub(super) async fn delete_agent(&self, id: &str, project: &Option<String>) -> Result<()> {
        let sha256 = id
            .strip_prefix(OLD_AGENT_STEP)
            .filter(|sha256| is_sha256(sha256))
            .ok_or_else(|| Error::provider(format!("{PROVIDER_NAME}: unknown step {id}")))?;
        let Some(project_id) = project else {
            return Ok(());
        };
        let bucket = self.bucket(project_id);
        let key = agent_key(sha256);
        self.s3(project_id)?.delete_object(&bucket, &key).await?;
        self.log
            .info("deleted agent", &[("bucket", &bucket), ("key", &key)]);
        Ok(())
    }

    /// Uploads the binary as a public object unless it is there; answers the agent's SHA-256.
    pub(super) async fn apply_agent(&self, project: &Option<String>) -> Result<String> {
        let project_id = self.require_project(project)?;
        let s3 = self.s3(project_id)?;
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

    #[test]
    fn all_but_the_current_and_the_two_newest_others_are_stale() {
        let shas: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|c| c.repeat(64))
            .collect();
        let agents = by_age(
            shas.iter()
                .enumerate()
                .map(|(i, sha)| listed(&agent_key(sha), Some(i as u64)))
                .collect(),
        );
        assert_eq!(
            agents,
            ["e", "d", "c", "b", "a"].map(|c| c.repeat(64)),
            "newest first"
        );
        assert_eq!(stale(&agents, &shas[4]), [shas[1].clone(), shas[0].clone()]);
        assert_eq!(
            stale(&agents, &shas[0]),
            [shas[2].clone(), shas[1].clone()],
            "an older current keeps the two newest others"
        );
        assert_eq!(
            stale(&agents, &"f".repeat(64)),
            [shas[2].clone(), shas[1].clone(), shas[0].clone()],
            "a binary not uploaded yet"
        );
        assert!(stale(&agents[..3], &shas[4]).is_empty());
        assert!(stale(&[], &shas[0]).is_empty());
    }
}
