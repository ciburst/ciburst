//! Burst intents (spec D33, M4 rules R3 and R13): one record per pending job that found no free
//! slot, read back by every dispatcher call of the same burst. Records are append-only; the
//! order is time, then job id.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::Result;

/// Seconds a dispatcher call looks back for intents of its burst.
pub const GROUP_WINDOW_S: u64 = 30;
/// Seconds a dispatcher call waits between recording its intent and reading the burst.
pub const GROUP_WAIT_S: u64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    /// Unix seconds of the recording dispatcher call.
    pub at: u64,
    pub job: String,
}

#[async_trait]
pub trait Ledger: Send + Sync {
    /// Records that `job` of `pool` found no free slot at `at`.
    async fn record(&self, pool: &str, at: u64, job: &str) -> Result<()>;
    /// Intents of `pool` recorded at or after `since`, in any order.
    async fn since(&self, pool: &str, since: u64) -> Result<Vec<Intent>>;
}

/// In-memory ledger for tests and the fake modules.
#[derive(Default)]
pub struct MemoryLedger {
    intents: Mutex<Vec<(String, Intent)>>,
}

impl MemoryLedger {
    pub fn new() -> MemoryLedger {
        MemoryLedger::default()
    }

    /// Every record as `(pool, intent)`, in recording order.
    pub fn all(&self) -> Vec<(String, Intent)> {
        self.intents.lock().expect("ledger").clone()
    }
}

#[async_trait]
impl Ledger for MemoryLedger {
    async fn record(&self, pool: &str, at: u64, job: &str) -> Result<()> {
        self.intents.lock().expect("ledger").push((
            pool.to_string(),
            Intent {
                at,
                job: job.to_string(),
            },
        ));
        Ok(())
    }

    async fn since(&self, pool: &str, since: u64) -> Result<Vec<Intent>> {
        Ok(self
            .intents
            .lock()
            .expect("ledger")
            .iter()
            .filter(|(p, intent)| p == pool && intent.at >= since)
            .map(|(_, intent)| intent.clone())
            .collect())
    }
}

/// The VM jobs of one pipeline by VM pool tag, as the warm step recorded them (M6.1 rule R5).
pub type VmJobs = BTreeMap<String, Vec<String>>;

/// Warm start records of a pipeline (M6.1 rules R4 to R6): the pipeline's VM jobs per pool and
/// a marker per VM job that went pending. Lives beside the intents, under
/// `ciburst/intents/_pipelines/<pipeline>/` in the cache bucket.
#[async_trait]
pub trait PipelineRecords: Send + Sync {
    /// Writes the VM jobs of `pipeline`; `false`, and nothing written, when a record exists.
    async fn open(&self, pipeline: &str, vm_jobs: &VmJobs) -> Result<bool>;
    /// The recorded VM jobs of `pipeline`, `None` without a record.
    async fn vm_jobs(&self, pipeline: &str) -> Result<Option<VmJobs>>;
    /// Marks VM job `job` of `pipeline` as gone pending.
    async fn seen(&self, pipeline: &str, job: &str) -> Result<()>;
    /// The jobs of `pipeline` marked by [`PipelineRecords::seen`], in any order.
    async fn seen_jobs(&self, pipeline: &str) -> Result<Vec<String>>;
    /// Removes every record of `pipeline`; no error when there is none.
    async fn close(&self, pipeline: &str) -> Result<()>;
}

/// In-memory pipeline records for tests and the fake modules.
#[derive(Default)]
pub struct MemoryPipelineRecords {
    records: Mutex<BTreeMap<String, (VmJobs, Vec<String>)>>,
}

impl MemoryPipelineRecords {
    pub fn new() -> MemoryPipelineRecords {
        MemoryPipelineRecords::default()
    }
}

#[async_trait]
impl PipelineRecords for MemoryPipelineRecords {
    async fn open(&self, pipeline: &str, vm_jobs: &VmJobs) -> Result<bool> {
        let mut records = self.records.lock().expect("records");
        if records.contains_key(pipeline) {
            return Ok(false);
        }
        records.insert(pipeline.to_string(), (vm_jobs.clone(), Vec::new()));
        Ok(true)
    }

    async fn vm_jobs(&self, pipeline: &str) -> Result<Option<VmJobs>> {
        let records = self.records.lock().expect("records");
        Ok(records.get(pipeline).map(|(jobs, _)| jobs.clone()))
    }

    async fn seen(&self, pipeline: &str, job: &str) -> Result<()> {
        let mut records = self.records.lock().expect("records");
        if let Some((_, seen)) = records.get_mut(pipeline)
            && !seen.iter().any(|j| j == job)
        {
            seen.push(job.to_string());
        }
        Ok(())
    }

    async fn seen_jobs(&self, pipeline: &str) -> Result<Vec<String>> {
        let records = self.records.lock().expect("records");
        Ok(records
            .get(pipeline)
            .map(|(_, seen)| seen.clone())
            .unwrap_or_default())
    }

    async fn close(&self, pipeline: &str) -> Result<()> {
        self.records.lock().expect("records").remove(pipeline);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pipeline_records_open_once_mark_seen_and_close() {
        let records = MemoryPipelineRecords::new();
        let jobs: VmJobs = [("vm".to_string(), vec!["11".to_string(), "12".to_string()])].into();
        assert!(records.open("9", &jobs).await.unwrap());
        assert!(!records.open("9", &VmJobs::new()).await.unwrap());
        assert_eq!(records.vm_jobs("9").await.unwrap(), Some(jobs));
        records.seen("9", "11").await.unwrap();
        records.seen("9", "11").await.unwrap();
        records.seen("8", "1").await.unwrap();
        assert_eq!(
            records.seen_jobs("9").await.unwrap(),
            vec!["11".to_string()]
        );
        assert!(records.seen_jobs("8").await.unwrap().is_empty());
        records.close("9").await.unwrap();
        assert_eq!(records.vm_jobs("9").await.unwrap(), None);
        records.close("9").await.unwrap();
    }
}
