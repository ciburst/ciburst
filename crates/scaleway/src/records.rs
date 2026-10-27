//! The warm start records of D48 (spec 6.5) in the cache bucket, beside the burst
//! intents: `ciburst/intents/_pipelines/<pipeline id>/vm-jobs.json` holds the pipeline's VM jobs
//! by runner tag; earlier versions also wrote `seen/<job id>` markers below that prefix, which
//! `close` deletes with the record. A runner tag starts with a letter or a digit, so the
//! intents of a runner never share this prefix; the lifecycle rule of `ciburst/intents/`
//! expires what a pipeline's final event did not delete. Signed with the cache key, like the
//! ledger.

use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::http::HttpClient;
use ciburst_core::ledger::{Closed, PipelineRecords, VmJobs};
use ciburst_core::types::KeyPair;
use ciburst_core::{Error, Result};

use crate::ledger::{MAX_PAGES, is_id};
use crate::provider::bucket_name;
use crate::provider::s3::S3;

/// Every pipeline's records live below this prefix, inside [`crate::ledger::INTENT_PREFIX`].
pub const PIPELINE_PREFIX: &str = "ciburst/intents/_pipelines/";
/// The object of a pipeline's VM jobs, below its prefix.
pub const VM_JOBS: &str = "vm-jobs.json";

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

/// D56: the closed records sit outside `_pipelines/`, so the listing of open records stays short;
/// the lifecycle rule of `ciburst/intents/` removes them.
pub const CLOSED_PREFIX: &str = "ciburst/intents/_closed/";

/// `ciburst/intents/_closed/<pipeline>`; the pipeline id is an [`is_id`] number.
pub fn closed_key(pipeline: &str) -> Result<String> {
    if !is_id(pipeline) {
        return Err(invalid(format!("pipeline id {pipeline:?} is not a number")));
    }
    Ok(format!("{CLOSED_PREFIX}{pipeline}"))
}

pub fn vm_jobs_key(pipeline: &str) -> Result<String> {
    Ok(format!("{}{VM_JOBS}", pipeline_prefix(pipeline)?))
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

    /// Lists first, then deletes, so no listing continues behind its own deletes.
    async fn close(&self, pipeline: &str) -> Result<()> {
        let prefix = pipeline_prefix(pipeline)?;
        for object in self.s3.list_all(&self.bucket, &prefix, MAX_PAGES).await? {
            self.s3.delete_object(&self.bucket, &object.key).await?;
        }
        Ok(())
    }

    async fn mark_closed(&self, pipeline: &str, closed: &Closed) -> Result<()> {
        let key = closed_key(pipeline)?;
        let body = serde_json::to_vec(closed)?;
        self.s3.put_object(&self.bucket, &key, body).await
    }

    async fn closed(&self, pipeline: &str) -> Result<Option<Closed>> {
        let key = closed_key(pipeline)?;
        let Some(body) = self.s3.get_object(&self.bucket, &key).await? else {
            return Ok(None);
        };
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| invalid(format!("{key}: {error}")))
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
            vm_jobs_key("18446744073709551615").unwrap(),
            "ciburst/intents/_pipelines/18446744073709551615/vm-jobs.json"
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
            assert!(closed_key(id).is_err(), "{id:?}");
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
