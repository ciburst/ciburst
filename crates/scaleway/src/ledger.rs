//! The burst ledger of spec D33 (M4 rules R3 and R13) in the cache bucket: one empty object per
//! intent, `ciburst/intents/<pool>/<unix s, 10 digits>-<job id, 20 digits>`, so the keys of a pool
//! sort by time, then job id, and ListObjectsV2 with `start-after` reads a time window. Signed
//! with the cache key, which the function holds anyway.

use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::http::HttpClient;
use ciburst_core::ledger::{Intent, Ledger};
use ciburst_core::types::KeyPair;
use ciburst_core::{Error, Result};

use crate::provider::bucket_name;
use crate::provider::s3::S3;

/// Every intent lives below this prefix.
pub const INTENT_PREFIX: &str = "ciburst/intents/";

const AT_DIGITS: usize = 10;
const JOB_DIGITS: usize = 20;

/// Pages one read follows at most. Every job can write the bucket, so a flood of forged keys
/// fails the read, and the dispatcher falls back to starting a VM, instead of running into the
/// function's timeout.
pub const MAX_PAGES: usize = 20;

pub struct ScalewayLedger {
    s3: S3,
    bucket: String,
}

impl ScalewayLedger {
    pub fn new(
        http: Arc<dyn HttpClient>,
        region: &str,
        bucket: impl Into<String>,
        key: &KeyPair,
    ) -> ScalewayLedger {
        ScalewayLedger {
            s3: S3 {
                http,
                region: region.to_string(),
                access_key: key.access_key.clone(),
                secret_key: key.secret_key.clone(),
            },
            bucket: bucket.into(),
        }
    }

    /// The ledger in the deployment's cache bucket `<name>-cache-<project id>`.
    pub fn in_cache_bucket(
        http: Arc<dyn HttpClient>,
        config: &Config,
        project_id: &str,
        region: &str,
        key: &KeyPair,
    ) -> ScalewayLedger {
        ScalewayLedger::new(http, region, bucket_name(config, project_id), key)
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }
}

fn invalid(what: String) -> Error {
    Error::provider(format!("scaleway ledger: {what}"))
}

/// `ciburst/intents/<pool>/`; a pool is a config tag (lowercase letters, digits, hyphens).
pub fn pool_prefix(pool: &str) -> Result<String> {
    let valid = !pool.is_empty()
        && pool
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        return Err(invalid(format!("pool {pool:?} is not a pool tag")));
    }
    Ok(format!("{INTENT_PREFIX}{pool}/"))
}

/// The object key of an intent. The job id is a number without leading zeros, as GitLab sends
/// it, so that the key reads back as the same id.
pub fn intent_key(pool: &str, at: u64, job: &str) -> Result<String> {
    let prefix = pool_prefix(pool)?;
    let canonical = (1..=JOB_DIGITS).contains(&job.len())
        && job.bytes().all(|b| b.is_ascii_digit())
        && (job == "0" || !job.starts_with('0'));
    if !canonical {
        return Err(invalid(format!("job id {job:?} is not a number")));
    }
    if at.to_string().len() > AT_DIGITS {
        return Err(invalid(format!(
            "time {at} has more than {AT_DIGITS} digits"
        )));
    }
    Ok(format!("{prefix}{at:0AT_DIGITS$}-{job:0>JOB_DIGITS$}"))
}

/// The intent a key below `prefix` stands for; `None` for any other key.
pub fn parse_intent(prefix: &str, key: &str) -> Option<Intent> {
    let (at, job) = key.strip_prefix(prefix)?.split_once('-')?;
    let digits =
        |text: &str, len: usize| text.len() == len && text.bytes().all(|b| b.is_ascii_digit());
    if !digits(at, AT_DIGITS) || !digits(job, JOB_DIGITS) {
        return None;
    }
    let job = job.trim_start_matches('0');
    Some(Intent {
        at: at.parse().ok()?,
        job: if job.is_empty() { "0" } else { job }.to_string(),
    })
}

#[async_trait]
impl Ledger for ScalewayLedger {
    async fn record(&self, pool: &str, at: u64, job: &str) -> Result<()> {
        let key = intent_key(pool, at, job)?;
        self.s3.put_object(&self.bucket, &key, Vec::new()).await
    }

    async fn since(&self, pool: &str, since: u64) -> Result<Vec<Intent>> {
        let prefix = pool_prefix(pool)?;
        let start_after = format!("{prefix}{since:0AT_DIGITS$}");
        let mut intents = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let page = self
                .s3
                .list_page(&self.bucket, &prefix, Some(&start_after), token.as_deref())
                .await?;
            intents.extend(
                page.keys
                    .iter()
                    .filter_map(|key| parse_intent(&prefix, key))
                    .filter(|intent| intent.at >= since),
            );
            match page.next {
                Some(next) => token = Some(next),
                None => return Ok(intents),
            }
        }
        Err(invalid(format!(
            "pool {pool}: more than {MAX_PAGES} pages of intents"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_sort_by_time_then_job() {
        assert_eq!(
            intent_key("vm", 1_800_000_000, "42").unwrap(),
            "ciburst/intents/vm/1800000000-00000000000000000042"
        );
        assert_eq!(
            intent_key("ci-2", 7, "18446744073709551615").unwrap(),
            "ciburst/intents/ci-2/0000000007-18446744073709551615"
        );
        assert_eq!(
            intent_key("vm", 9_999_999_999, "0").unwrap(),
            "ciburst/intents/vm/9999999999-00000000000000000000"
        );
        let mut keys = [
            intent_key("vm", 100, "9").unwrap(),
            intent_key("vm", 99, "10").unwrap(),
            intent_key("vm", 100, "10").unwrap(),
        ];
        keys.sort();
        let read: Vec<Intent> = keys
            .iter()
            .map(|k| parse_intent("ciburst/intents/vm/", k).unwrap())
            .collect();
        let pairs: Vec<(u64, &str)> = read.iter().map(|i| (i.at, i.job.as_str())).collect();
        assert_eq!(pairs, [(99, "10"), (100, "9"), (100, "10")]);
    }

    #[test]
    fn refuses_what_would_not_read_back() {
        for job in ["", "042", "4a", "-1", "123456789012345678901", "1/2"] {
            assert!(intent_key("vm", 1, job).is_err(), "{job:?}");
        }
        for pool in ["", "VM", "a/b", "..", "a b"] {
            assert!(intent_key(pool, 1, "1").is_err(), "{pool:?}");
            assert!(pool_prefix(pool).is_err(), "{pool:?}");
        }
        assert!(intent_key("vm", 10_000_000_000, "1").is_err());
    }

    #[test]
    fn parses_only_intent_keys_of_the_pool() {
        let prefix = "ciburst/intents/vm/";
        assert_eq!(
            parse_intent(prefix, "ciburst/intents/vm/0000000005-00000000000000000000"),
            Some(Intent {
                at: 5,
                job: "0".into()
            })
        );
        for key in [
            "ciburst/intents/vm-2/0000000005-00000000000000000001",
            "ciburst/intents/vm/5-1",
            "ciburst/intents/vm/0000000005-0000000000000000000x",
            "ciburst/intents/vm/0000000005_00000000000000000001",
            "ciburst/intents/vm/0000000005-000000000000000000001",
            "ciburst/intents/vm/0000000005-00000000000000000001/x",
            "runner/cache.zip",
        ] {
            assert_eq!(parse_intent(prefix, key), None, "{key}");
        }
    }
}
