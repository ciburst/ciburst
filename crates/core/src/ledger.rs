//! Burst intents (spec D33, M4 rules R3 and R13): one record per pending job that found no free
//! slot, read back by every dispatcher call of the same burst. Records are append-only; the
//! order is time, then job id.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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

/// A VM job waiting for a paid slot (M6.2 rule R2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queued {
    pub job: String,
    pub name: String,
    pub pipeline: String,
    /// Unix seconds of its pending event.
    pub at: u64,
}

/// A VM job running on a VM of the pool (M6.2 rule R3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Running {
    pub job: String,
    pub name: String,
    /// Unix seconds the job started.
    pub started: u64,
}

/// The typical duration after one more finished run: the first run sets it, later runs weigh
/// 3 to 7 against it (M6.2 rule R3). Every [`Schedule`] folds durations with this function.
pub fn fold_duration(typical: Option<u64>, duration_s: u64) -> u64 {
    match typical {
        None => duration_s,
        Some(old) => (old * 7 + duration_s * 3 + 5) / 10,
    }
}

/// Queue-first scheduling state of the VM pools (M6.2 rules R2 and R3): the VM jobs waiting for
/// a slot, the VM jobs running, and per job name its typical duration. Lives beside the
/// intents, under `ciburst/intents/_queue/`, `_running/` and `_stats/` in the cache bucket.
#[async_trait]
pub trait Schedule: Send + Sync {
    /// Records a waiting VM job; a second call for the same job replaces the first.
    async fn enqueue(&self, pool: &str, job: &Queued) -> Result<()>;
    /// The waiting VM jobs of `pool`, in any order.
    async fn queued(&self, pool: &str) -> Result<Vec<Queued>>;
    /// Removes the job from the queue and records it as running.
    async fn started(&self, pool: &str, job: &Running) -> Result<()>;
    /// The running VM jobs of `pool`, in any order.
    async fn running(&self, pool: &str) -> Result<Vec<Running>>;
    /// Removes the job from the queue and from the running jobs; with a duration, folds it into
    /// the typical duration of `name` ([`fold_duration`]).
    async fn finished(
        &self,
        pool: &str,
        job: &str,
        name: &str,
        duration_s: Option<u64>,
    ) -> Result<()>;
    /// The typical duration of `name` in `pool`, `None` before its first finished run.
    async fn typical(&self, pool: &str, name: &str) -> Result<Option<u64>>;
}

/// In-memory schedule for tests and the fake modules.
#[derive(Default)]
pub struct MemorySchedule {
    queued: Mutex<BTreeMap<(String, String), Queued>>,
    running: Mutex<BTreeMap<(String, String), Running>>,
    typical: Mutex<BTreeMap<(String, String), u64>>,
}

impl MemorySchedule {
    pub fn new() -> MemorySchedule {
        MemorySchedule::default()
    }
}

#[async_trait]
impl Schedule for MemorySchedule {
    async fn enqueue(&self, pool: &str, job: &Queued) -> Result<()> {
        self.queued
            .lock()
            .expect("schedule")
            .insert((pool.to_string(), job.job.clone()), job.clone());
        Ok(())
    }

    async fn queued(&self, pool: &str) -> Result<Vec<Queued>> {
        let queued = self.queued.lock().expect("schedule");
        Ok(queued
            .iter()
            .filter(|((p, _), _)| p == pool)
            .map(|(_, job)| job.clone())
            .collect())
    }

    async fn started(&self, pool: &str, job: &Running) -> Result<()> {
        let key = (pool.to_string(), job.job.clone());
        self.queued.lock().expect("schedule").remove(&key);
        self.running
            .lock()
            .expect("schedule")
            .insert(key, job.clone());
        Ok(())
    }

    async fn running(&self, pool: &str) -> Result<Vec<Running>> {
        let running = self.running.lock().expect("schedule");
        Ok(running
            .iter()
            .filter(|((p, _), _)| p == pool)
            .map(|(_, job)| job.clone())
            .collect())
    }

    async fn finished(
        &self,
        pool: &str,
        job: &str,
        name: &str,
        duration_s: Option<u64>,
    ) -> Result<()> {
        let key = (pool.to_string(), job.to_string());
        self.queued.lock().expect("schedule").remove(&key);
        self.running.lock().expect("schedule").remove(&key);
        if let Some(duration) = duration_s {
            let mut typical = self.typical.lock().expect("schedule");
            let slot = (pool.to_string(), name.to_string());
            let folded = fold_duration(typical.get(&slot).copied(), duration);
            typical.insert(slot, folded);
        }
        Ok(())
    }

    async fn typical(&self, pool: &str, name: &str) -> Result<Option<u64>> {
        let typical = self.typical.lock().expect("schedule");
        Ok(typical.get(&(pool.to_string(), name.to_string())).copied())
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
    #[test]
    fn durations_fold_three_to_seven() {
        assert_eq!(fold_duration(None, 60), 60);
        assert_eq!(fold_duration(Some(60), 160), 90);
        assert_eq!(fold_duration(Some(100), 0), 70);
    }

    #[tokio::test]
    async fn the_schedule_moves_jobs_from_queued_to_running_to_finished() {
        let schedule = MemorySchedule::new();
        let job = |id: &str| Queued {
            job: id.into(),
            name: "build-image".into(),
            pipeline: "9".into(),
            at: 100,
        };
        schedule.enqueue("vm", &job("41")).await.unwrap();
        schedule.enqueue("vm", &job("42")).await.unwrap();
        schedule.enqueue("big", &job("43")).await.unwrap();
        assert_eq!(schedule.queued("vm").await.unwrap().len(), 2);
        let running = Running {
            job: "41".into(),
            name: "build-image".into(),
            started: 110,
        };
        schedule.started("vm", &running).await.unwrap();
        assert_eq!(schedule.queued("vm").await.unwrap(), vec![job("42")]);
        assert_eq!(schedule.running("vm").await.unwrap(), vec![running]);
        schedule
            .finished("vm", "41", "build-image", Some(400))
            .await
            .unwrap();
        schedule
            .finished("vm", "42", "build-image", None)
            .await
            .unwrap();
        assert!(schedule.running("vm").await.unwrap().is_empty());
        assert!(schedule.queued("vm").await.unwrap().is_empty());
        assert_eq!(
            schedule.typical("vm", "build-image").await.unwrap(),
            Some(400)
        );
        assert_eq!(schedule.typical("big", "build-image").await.unwrap(), None);
        assert_eq!(schedule.queued("big").await.unwrap().len(), 1);
    }
}
