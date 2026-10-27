//! The queue-first schedule of D53 (spec 6.4) in the cache bucket, beside the
//! burst intents, one object per entry:
//! - `ciburst/intents/_queue/<pool>/<job id>`: a waiting job, JSON [`Queued`];
//! - `ciburst/intents/_running/<pool>/<job id>`: a running job, JSON [`Running`];
//! - `ciburst/intents/_stats/<pool>/<first 16 hex of the sha256 of the name>`: the typical
//!   duration of a job name, JSON [`Stats`];
//! - `ciburst/intents/_queue/<pool>/.started`: the start marker, its body the Unix seconds it
//!   was taken.
//!
//! A pool tag starts with a letter or a digit, so the intents of a pool never share these
//! prefixes; the lifecycle rule of `ciburst/intents/` expires what a lost event left behind.
//! Every job can write the bucket, so a read skips an entry whose body is not the entry its key
//! names, and a stats object of another name counts as none. Signed with the cache key, like
//! the ledger.

use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::http::HttpClient;
use ciburst_core::ledger::{Queued, Running, START_MARKER_S, Schedule, fold_duration};
use ciburst_core::types::KeyPair;
use ciburst_core::{Error, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ledger::{MAX_PAGES, is_id, is_pool};
use crate::provider::bucket_name;
use crate::provider::s3::S3;

/// The waiting jobs of every pool, and their start markers.
pub const QUEUE_PREFIX: &str = "ciburst/intents/_queue/";
/// The running jobs of every pool.
pub const RUNNING_PREFIX: &str = "ciburst/intents/_running/";
/// The typical durations of every pool.
pub const STATS_PREFIX: &str = "ciburst/intents/_stats/";
/// The start marker's name below a pool's queue prefix; no job id.
pub const START_MARKER: &str = ".started";
/// Hex digits of a name's sha256 in its stats key.
const NAME_HEX: usize = 16;

/// The typical duration of one job name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub name: String,
    #[serde(rename = "typicalS")]
    pub typical_s: u64,
    /// Finished runs folded into `typical_s`.
    pub runs: u64,
}

pub struct ScalewaySchedule {
    s3: S3,
    bucket: String,
}

impl ScalewaySchedule {
    pub fn new(
        http: Arc<dyn HttpClient>,
        region: &str,
        bucket: impl Into<String>,
        key: &KeyPair,
    ) -> ScalewaySchedule {
        ScalewaySchedule {
            s3: S3::with_key(http, region, key),
            bucket: bucket.into(),
        }
    }

    /// The schedule in the deployment's cache bucket `<name>-cache-<project id>`.
    pub fn in_cache_bucket(
        http: Arc<dyn HttpClient>,
        config: &Config,
        project_id: &str,
        region: &str,
        key: &KeyPair,
    ) -> ScalewaySchedule {
        ScalewaySchedule::new(http, region, bucket_name(config, project_id), key)
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Every entry below `prefix` whose key is a job id, each read with one GET. An entry
    /// deleted between the listing and its GET is gone; one whose body is not a `T` of the job
    /// its key names is skipped.
    async fn entries<T: DeserializeOwned>(
        &self,
        prefix: &str,
        job_of: impl Fn(&T) -> &str,
    ) -> Result<Vec<T>> {
        let mut entries = Vec::new();
        for object in self.s3.list_all(&self.bucket, prefix, MAX_PAGES).await? {
            let Some(job) = parse_job(prefix, &object.key) else {
                continue;
            };
            let Some(body) = self.s3.get_object(&self.bucket, &object.key).await? else {
                continue;
            };
            if let Ok(entry) = serde_json::from_slice::<T>(&body)
                && job_of(&entry) == job
            {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// The stats of `name` at `key`; none when missing, unreadable or of another name.
    async fn stats(&self, key: &str, name: &str) -> Result<Option<Stats>> {
        let Some(body) = self.s3.get_object(&self.bucket, key).await? else {
            return Ok(None);
        };
        Ok(serde_json::from_slice::<Stats>(&body)
            .ok()
            .filter(|stats| stats.name == name))
    }
}

fn invalid(what: String) -> Error {
    Error::provider(format!("scaleway schedule: {what}"))
}

/// `<base><pool>/`; a pool is a config tag.
fn pool_prefix(base: &str, pool: &str) -> Result<String> {
    if !is_pool(pool) {
        return Err(invalid(format!("pool {pool:?} is not a pool tag")));
    }
    Ok(format!("{base}{pool}/"))
}

fn job_key(prefix: String, job: &str) -> Result<String> {
    if !is_id(job) {
        return Err(invalid(format!("job id {job:?} is not a number")));
    }
    Ok(format!("{prefix}{job}"))
}

/// `ciburst/intents/_queue/<pool>/`.
pub fn queue_prefix(pool: &str) -> Result<String> {
    pool_prefix(QUEUE_PREFIX, pool)
}

/// `ciburst/intents/_running/<pool>/`.
pub fn running_prefix(pool: &str) -> Result<String> {
    pool_prefix(RUNNING_PREFIX, pool)
}

/// The queue entry of job `job`; the job id is an [`is_id`] number.
pub fn queue_key(pool: &str, job: &str) -> Result<String> {
    job_key(queue_prefix(pool)?, job)
}

/// The running entry of job `job`; the job id is an [`is_id`] number.
pub fn running_key(pool: &str, job: &str) -> Result<String> {
    job_key(running_prefix(pool)?, job)
}

/// `ciburst/intents/_queue/<pool>/.started`.
pub fn start_key(pool: &str) -> Result<String> {
    Ok(format!("{}{START_MARKER}", queue_prefix(pool)?))
}

/// The stats of `name`: any name, so the key holds a hash of it.
pub fn stats_key(pool: &str, name: &str) -> Result<String> {
    let digest = hex::encode(Sha256::digest(name.as_bytes()));
    Ok(format!(
        "{}{}",
        pool_prefix(STATS_PREFIX, pool)?,
        &digest[..NAME_HEX]
    ))
}

/// The job a key below `prefix` stands for; `None` for any other key, the start marker too.
pub fn parse_job(prefix: &str, key: &str) -> Option<String> {
    let job = key.strip_prefix(prefix)?;
    is_id(job).then(|| job.to_string())
}

#[async_trait]
impl Schedule for ScalewaySchedule {
    async fn enqueue(&self, pool: &str, job: &Queued) -> Result<()> {
        let key = queue_key(pool, &job.job)?;
        self.s3
            .put_object(&self.bucket, &key, serde_json::to_vec(job)?)
            .await
    }

    async fn queued(&self, pool: &str) -> Result<Vec<Queued>> {
        self.entries(&queue_prefix(pool)?, |job: &Queued| &job.job)
            .await
    }

    async fn started(&self, pool: &str, job: &Running) -> Result<()> {
        let queued = queue_key(pool, &job.job)?;
        let running = running_key(pool, &job.job)?;
        self.s3.delete_object(&self.bucket, &queued).await?;
        self.s3
            .put_object(&self.bucket, &running, serde_json::to_vec(job)?)
            .await
    }

    async fn running(&self, pool: &str) -> Result<Vec<Running>> {
        self.entries(&running_prefix(pool)?, |job: &Running| &job.job)
            .await
    }

    /// Without a lock: two runs of the same name that finish at once may both read the old
    /// stats, and the later write drops the other run from the fold.
    async fn finished(
        &self,
        pool: &str,
        job: &str,
        name: &str,
        duration_s: Option<u64>,
    ) -> Result<()> {
        let queued = queue_key(pool, job)?;
        let running = running_key(pool, job)?;
        let key = stats_key(pool, name)?;
        self.s3.delete_object(&self.bucket, &queued).await?;
        self.s3.delete_object(&self.bucket, &running).await?;
        let Some(duration) = duration_s else {
            return Ok(());
        };
        let old = self.stats(&key, name).await?;
        let stats = Stats {
            name: name.to_string(),
            typical_s: fold_duration(old.as_ref().map(|s| s.typical_s), duration),
            runs: old.map_or(0, |s| s.runs).saturating_add(1),
        };
        self.s3
            .put_object(&self.bucket, &key, serde_json::to_vec(&stats)?)
            .await
    }

    async fn typical(&self, pool: &str, name: &str) -> Result<Option<u64>> {
        let key = stats_key(pool, name)?;
        Ok(self.stats(&key, name).await?.map(|stats| stats.typical_s))
    }

    /// One GET of the marker, and a PUT when it is taken. Without a lock: two evaluations in
    /// the same moment may both find no marker and both start VMs, at most one VM more and one
    /// VM-hour, the same limit as the ledger's. A marker more than [`START_MARKER_S`] ahead of
    /// `now` is a forged or skewed one and holds nothing.
    async fn take_start(&self, pool: &str, now: u64) -> Result<bool> {
        let key = start_key(pool)?;
        if let Some(body) = self.s3.get_object(&self.bucket, &key).await? {
            let at = std::str::from_utf8(&body)
                .ok()
                .and_then(|text| text.parse::<u64>().ok());
            if at.is_some_and(|at| at.abs_diff(now) < START_MARKER_S) {
                return Ok(false);
            }
        }
        self.s3
            .put_object(&self.bucket, &key, now.to_string().into_bytes())
            .await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{INTENT_PREFIX, intent_key, pool_prefix as intent_prefix};
    use crate::provider::s3::LIFECYCLE_RULES;
    use crate::records::PIPELINE_PREFIX;

    #[test]
    fn keys_sit_below_their_prefixes() {
        assert_eq!(
            queue_key("vm", "42").unwrap(),
            "ciburst/intents/_queue/vm/42"
        );
        assert_eq!(
            running_key("node24", "18446744073709551615").unwrap(),
            "ciburst/intents/_running/node24/18446744073709551615"
        );
        assert_eq!(
            start_key("vm").unwrap(),
            "ciburst/intents/_queue/vm/.started"
        );
        let stats = stats_key("vm", "build-image").unwrap();
        let digest = hex::encode(Sha256::digest(b"build-image"));
        assert_eq!(
            stats,
            format!("ciburst/intents/_stats/vm/{}", &digest[..16])
        );
        assert_ne!(stats, stats_key("vm", "build-image2").unwrap());
        assert_ne!(stats, stats_key("vm-2", "build-image").unwrap());
        assert!(
            stats_key("vm", "deploy: site/dev [1/2]")
                .unwrap()
                .strip_prefix("ciburst/intents/_stats/vm/")
                .is_some_and(|hash| hash.len() == 16
                    && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        );
        assert!(
            !queue_prefix("vm-2")
                .unwrap()
                .starts_with(&queue_prefix("vm").unwrap()),
            "a pool's prefix holds no other pool"
        );
    }

    #[test]
    fn refuses_what_would_not_read_back() {
        for job in [
            "",
            "042",
            "4a",
            "-1",
            "1/2",
            ".started",
            "123456789012345678901",
        ] {
            assert!(queue_key("vm", job).is_err(), "{job:?}");
            assert!(running_key("vm", job).is_err(), "{job:?}");
        }
        for pool in ["", "VM", "a/b", "..", "a b", "_queue", "_"] {
            assert!(queue_prefix(pool).is_err(), "{pool:?}");
            assert!(running_prefix(pool).is_err(), "{pool:?}");
            assert!(start_key(pool).is_err(), "{pool:?}");
            assert!(stats_key(pool, "x").is_err(), "{pool:?}");
        }
    }

    #[test]
    fn parses_only_job_keys_below_the_prefix() {
        let prefix = "ciburst/intents/_queue/vm/";
        assert_eq!(
            parse_job(prefix, "ciburst/intents/_queue/vm/42"),
            Some("42".into())
        );
        for key in [
            "ciburst/intents/_queue/vm/.started",
            "ciburst/intents/_queue/vm/042",
            "ciburst/intents/_queue/vm/",
            "ciburst/intents/_queue/vm/4/2",
            "ciburst/intents/_queue/vm-2/42",
            "ciburst/intents/_running/vm/42",
        ] {
            assert_eq!(parse_job(prefix, key), None, "{key}");
        }
    }

    /// No pool's intents, and no pipeline's records, share the schedule's prefixes, and the
    /// intents' lifecycle rule covers all of them.
    #[test]
    fn the_schedule_sits_apart_from_the_intents_and_expires_with_them() {
        for pool in ["_", "_queue", "_running", "_stats"] {
            assert!(intent_prefix(pool).is_err(), "{pool}");
            assert!(intent_key(pool, 1, "1").is_err(), "{pool}");
        }
        let prefixes = [QUEUE_PREFIX, RUNNING_PREFIX, STATS_PREFIX];
        for prefix in prefixes {
            for pool in ["vm", "0", "a-b", "-", "queue", "running", "stats"] {
                assert!(!prefix.starts_with(&intent_prefix(pool).unwrap()), "{pool}");
                assert!(!intent_prefix(pool).unwrap().starts_with(prefix), "{pool}");
            }
            assert!(!prefix.starts_with(PIPELINE_PREFIX));
            assert!(!PIPELINE_PREFIX.starts_with(prefix));
            let rule = LIFECYCLE_RULES
                .iter()
                .find(|(_, rule)| prefix.starts_with(rule))
                .expect("a rule expires the schedule");
            assert_eq!(rule.1, INTENT_PREFIX, "{prefix}");
        }
    }
}
