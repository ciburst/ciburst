//! The warm start records of spec D48 (M6.1 rule R5) in the cache bucket, beside the burst
//! intents: `ciburst/intents/_pipelines/<pipeline id>/vm-jobs.json` holds the pipeline's VM jobs
//! per pool, `ciburst/intents/_pipelines/<pipeline id>/seen/<job id>` is an empty object per VM
//! job that went pending. A pool tag starts with a letter or a digit, so the intents of a pool
//! never share this prefix; the lifecycle rule of `ciburst/intents/` expires what a pipeline's
//! final event did not delete. Signed with the cache key, like the ledger.

use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::http::HttpClient;
use ciburst_core::ledger::{PipelineRecords, VmJobs};
use ciburst_core::types::KeyPair;
use ciburst_core::{Error, Result};

use crate::ledger::{MAX_PAGES, is_id};
use crate::provider::bucket_name;
use crate::provider::s3::S3;

/// Every pipeline's records live below this prefix, inside [`crate::ledger::INTENT_PREFIX`].
pub const PIPELINE_PREFIX: &str = "ciburst/intents/_pipelines/";
/// The object of a pipeline's VM jobs, below its prefix.
pub const VM_JOBS: &str = "vm-jobs.json";
/// The markers of a pipeline's pending VM jobs, below its prefix.
pub const SEEN: &str = "seen/";

pub struct ScalewayPipelineRecords {
    s3: S3,
    bucket: String,
}

impl ScalewayPipelineRecords {
    pub fn new(
        http: Arc<dyn HttpClient>,
        region: &str,
        bucket: impl Into<String>,
        key: &KeyPair,
    ) -> ScalewayPipelineRecords {
        ScalewayPipelineRecords {
            s3: S3::with_key(http, region, key),
            bucket: bucket.into(),
        }
    }

    /// The records in the deployment's cache bucket `<name>-cache-<project id>`.
    pub fn in_cache_bucket(
        http: Arc<dyn HttpClient>,
        config: &Config,
        project_id: &str,
        region: &str,
        key: &KeyPair,
    ) -> ScalewayPipelineRecords {
        ScalewayPipelineRecords::new(http, region, bucket_name(config, project_id), key)
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }
}

fn invalid(what: String) -> Error {
    Error::provider(format!("scaleway records: {what}"))
}

/// `ciburst/intents/_pipelines/<pipeline>/`; the pipeline id is an [`is_id`] number.
pub fn pipeline_prefix(pipeline: &str) -> Result<String> {
    if !is_id(pipeline) {
        return Err(invalid(format!("pipeline id {pipeline:?} is not a number")));
    }
    Ok(format!("{PIPELINE_PREFIX}{pipeline}/"))
}

pub fn vm_jobs_key(pipeline: &str) -> Result<String> {
    Ok(format!("{}{VM_JOBS}", pipeline_prefix(pipeline)?))
}

pub fn seen_prefix(pipeline: &str) -> Result<String> {
    Ok(format!("{}{SEEN}", pipeline_prefix(pipeline)?))
}

/// The marker of job `job`; the job id is an [`is_id`] number.
pub fn seen_key(pipeline: &str, job: &str) -> Result<String> {
    let prefix = seen_prefix(pipeline)?;
    if !is_id(job) {
        return Err(invalid(format!("job id {job:?} is not a number")));
    }
    Ok(format!("{prefix}{job}"))
}

/// The job a key below `prefix` marks; `None` for any other key.
pub fn parse_seen(prefix: &str, key: &str) -> Option<String> {
    let job = key.strip_prefix(prefix)?;
    is_id(job).then(|| job.to_string())
}

#[async_trait]
impl PipelineRecords for ScalewayPipelineRecords {
    async fn open(&self, pipeline: &str, vm_jobs: &VmJobs) -> Result<bool> {
        let key = vm_jobs_key(pipeline)?;
        // Without a lock: two calls in the same second may both find no record and both open.
        // That warms at most one VM more per pool, one VM-hour, the same limit as the ledger's.
        if self.s3.object_exists(&self.bucket, &key).await? {
            return Ok(false);
        }
        let body = serde_json::to_vec(vm_jobs)?;
        self.s3.put_object(&self.bucket, &key, body).await?;
        Ok(true)
    }

    async fn vm_jobs(&self, pipeline: &str) -> Result<Option<VmJobs>> {
        let key = vm_jobs_key(pipeline)?;
        let Some(body) = self.s3.get_object(&self.bucket, &key).await? else {
            return Ok(None);
        };
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| invalid(format!("{key}: {error}")))
    }

    /// One HEAD of the pipeline's record, and the marker only when there is one (M6.1 rule R5).
    /// A job event that comes before the record leaves no marker, so the reserve counts that
    /// job until the pipeline ends.
    async fn seen(&self, pipeline: &str, job: &str) -> Result<()> {
        let marker = seen_key(pipeline, job)?;
        if self
            .s3
            .object_exists(&self.bucket, &vm_jobs_key(pipeline)?)
            .await?
        {
            self.s3
                .put_object(&self.bucket, &marker, Vec::new())
                .await?;
        }
        Ok(())
    }

    async fn seen_jobs(&self, pipeline: &str) -> Result<Vec<String>> {
        let prefix = seen_prefix(pipeline)?;
        let objects = self.s3.list_all(&self.bucket, &prefix, MAX_PAGES).await?;
        Ok(objects
            .iter()
            .filter_map(|object| parse_seen(&prefix, &object.key))
            .collect())
    }

    /// Lists first, then deletes, so no listing continues behind its own deletes.
    async fn close(&self, pipeline: &str) -> Result<()> {
        let prefix = pipeline_prefix(pipeline)?;
        for object in self.s3.list_all(&self.bucket, &prefix, MAX_PAGES).await? {
            self.s3.delete_object(&self.bucket, &object.key).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{INTENT_PREFIX, intent_key, pool_prefix};
    use crate::provider::s3::LIFECYCLE_RULES;

    #[test]
    fn keys_sit_below_the_pipeline_prefix() {
        assert_eq!(
            vm_jobs_key("9").unwrap(),
            "ciburst/intents/_pipelines/9/vm-jobs.json"
        );
        assert_eq!(
            seen_key("9", "42").unwrap(),
            "ciburst/intents/_pipelines/9/seen/42"
        );
        assert_eq!(
            seen_prefix("18446744073709551615").unwrap(),
            "ciburst/intents/_pipelines/18446744073709551615/seen/"
        );
        assert!(
            !pipeline_prefix("90")
                .unwrap()
                .starts_with(&pipeline_prefix("9").unwrap()),
            "a pipeline's prefix holds no other pipeline"
        );
    }

    #[test]
    fn refuses_ids_that_would_not_read_back() {
        for id in ["", "09", "x", "-1", "1/2", "..", "123456789012345678901"] {
            assert!(pipeline_prefix(id).is_err(), "{id:?}");
            assert!(vm_jobs_key(id).is_err(), "{id:?}");
            assert!(seen_key(id, "1").is_err(), "{id:?}");
            assert!(seen_key("1", id).is_err(), "{id:?}");
        }
    }

    #[test]
    fn parses_only_markers_below_the_prefix() {
        let prefix = "ciburst/intents/_pipelines/9/seen/";
        assert_eq!(
            parse_seen(prefix, "ciburst/intents/_pipelines/9/seen/42"),
            Some("42".into())
        );
        for key in [
            "ciburst/intents/_pipelines/9/seen/042",
            "ciburst/intents/_pipelines/9/seen/",
            "ciburst/intents/_pipelines/9/seen/4/2",
            "ciburst/intents/_pipelines/9/vm-jobs.json",
            "ciburst/intents/_pipelines/90/seen/42",
        ] {
            assert_eq!(parse_seen(prefix, key), None, "{key}");
        }
    }

    /// No pool's intents share the records' prefix, and the intents' lifecycle rule covers it.
    #[test]
    fn records_sit_apart_from_the_intents_and_expire_with_them() {
        for pool in ["_", "_pipelines"] {
            assert!(pool_prefix(pool).is_err(), "{pool}");
            assert!(intent_key(pool, 1, "1").is_err(), "{pool}");
        }
        for pool in ["vm", "0", "a-b", "-"] {
            assert!(!PIPELINE_PREFIX.starts_with(&pool_prefix(pool).unwrap()));
        }
        let rule = LIFECYCLE_RULES
            .iter()
            .find(|(_, prefix)| PIPELINE_PREFIX.starts_with(prefix))
            .expect("a rule expires the records");
        assert_eq!(rule.1, INTENT_PREFIX);
    }
}
