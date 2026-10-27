//! Burst intents (D33, spec 6.3): one record per pending job that found no free
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

/// The VM jobs of one pipeline by VM pool tag, as the warm step recorded them (spec 6.5).
pub type VmJobs = BTreeMap<String, Vec<String>>;

/// A pipeline with a record and its VM jobs that have not gone pending yet (spec 6.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPipeline {
    pub pipeline: String,
    /// Unix seconds the record was written, `None` where the store keeps no time.
    pub opened: Option<u64>,
    /// The recorded VM jobs without a seen marker, by pool; a pool whose every VM job has a
    /// marker is left out.
    pub unseen: VmJobs,
}

/// A finished pipeline (D56): when it ended and which of its jobs were still manual then, so a
/// pending event that GitLab delivers late for one of its jobs can be told from a new one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closed {
    /// Unix seconds of the pipeline's final event.
    pub at: u64,
    /// Ids of the jobs with status `manual` in that event; playing one later is a new run.
    #[serde(default)]
    pub manual: Vec<String>,
}

/// Warm start records of a pipeline (spec 6.5): the pipeline's VM jobs per pool and
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
    /// Every pipeline with a record, in any order, with its VM jobs that have no seen marker
    /// (spec 6.5). A store without it fails; the dispatcher does not call it.
    async fn open_pipelines(&self) -> Result<Vec<OpenPipeline>> {
        Err(crate::Error::other(
            "the records cannot list open pipelines",
        ))
    }
    /// Records that `pipeline` finished (D56); a later call replaces the earlier one.
    async fn mark_closed(&self, _pipeline: &str, _closed: &Closed) -> Result<()> {
        Ok(())
    }
    /// The [`Closed`] record of `pipeline`, `None` while it runs or after the lifecycle rule
    /// removed it.
    async fn closed(&self, _pipeline: &str) -> Result<Option<Closed>> {
        Ok(None)
    }
}

/// One pipeline's record in [`MemoryPipelineRecords`].
struct MemoryRecord {
    jobs: VmJobs,
    seen: Vec<String>,
    opened: Option<u64>,
}

/// In-memory pipeline records for tests and the fake modules.
#[derive(Default)]
pub struct MemoryPipelineRecords {
    records: Mutex<BTreeMap<String, MemoryRecord>>,
    closed: Mutex<BTreeMap<String, Closed>>,
    clock: Option<crate::fake::Clock>,
}

impl MemoryPipelineRecords {
    pub fn new() -> MemoryPipelineRecords {
        MemoryPipelineRecords::default()
    }

    /// Stamps each record with the clock's time, which [`PipelineRecords::open_pipelines`]
    /// answers as `opened`; without a clock it answers `None`.
    pub fn with_clock(mut self, clock: crate::fake::Clock) -> MemoryPipelineRecords {
        self.clock = Some(clock);
        self
    }
}

#[async_trait]
impl PipelineRecords for MemoryPipelineRecords {
    async fn open(&self, pipeline: &str, vm_jobs: &VmJobs) -> Result<bool> {
        let opened = self.clock.as_ref().map(|clock| clock());
        let mut records = self.records.lock().expect("records");
        if records.contains_key(pipeline) {
            return Ok(false);
        }
        let record = MemoryRecord {
            jobs: vm_jobs.clone(),
            seen: Vec::new(),
            opened,
        };
        records.insert(pipeline.to_string(), record);
        Ok(true)
    }

    async fn vm_jobs(&self, pipeline: &str) -> Result<Option<VmJobs>> {
        let records = self.records.lock().expect("records");
        Ok(records.get(pipeline).map(|record| record.jobs.clone()))
    }

    async fn seen(&self, pipeline: &str, job: &str) -> Result<()> {
        let mut records = self.records.lock().expect("records");
        if let Some(record) = records.get_mut(pipeline)
            && !record.seen.iter().any(|j| j == job)
        {
            record.seen.push(job.to_string());
        }
        Ok(())
    }

    async fn seen_jobs(&self, pipeline: &str) -> Result<Vec<String>> {
        let records = self.records.lock().expect("records");
        Ok(records
            .get(pipeline)
            .map(|record| record.seen.clone())
            .unwrap_or_default())
    }

    async fn close(&self, pipeline: &str) -> Result<()> {
        self.records.lock().expect("records").remove(pipeline);
        Ok(())
    }

    async fn open_pipelines(&self) -> Result<Vec<OpenPipeline>> {
        let records = self.records.lock().expect("records");
        Ok(records
            .iter()
            .map(|(pipeline, record)| OpenPipeline {
                pipeline: pipeline.clone(),
                opened: record.opened,
                unseen: unseen_of(&record.jobs, &record.seen),
            })
            .collect())
    }

    async fn mark_closed(&self, pipeline: &str, closed: &Closed) -> Result<()> {
        self.closed
            .lock()
            .expect("records")
            .insert(pipeline.to_string(), closed.clone());
        Ok(())
    }

    async fn closed(&self, pipeline: &str) -> Result<Option<Closed>> {
        Ok(self.closed.lock().expect("records").get(pipeline).cloned())
    }
}

/// The jobs of `jobs` without a marker in `seen`, by pool; pools left empty are left out.
pub fn unseen_of(jobs: &VmJobs, seen: &[String]) -> VmJobs {
    jobs.iter()
        .filter_map(|(pool, ids)| {
            let left: Vec<String> = ids
                .iter()
                .filter(|id| !seen.contains(id))
                .cloned()
                .collect();
            (!left.is_empty()).then(|| (pool.clone(), left))
        })
        .collect()
}

/// A VM job waiting for a paid slot (spec 6.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queued {
    pub job: String,
    pub name: String,
    pub pipeline: String,
    /// Unix seconds of its pending event.
    pub at: u64,
}

/// A VM job running on a VM of the pool (spec 6.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Running {
    pub job: String,
    pub name: String,
    /// Unix seconds the job started.
    pub started: u64,
}

/// The typical duration after one more finished run: the first run sets it, later runs weigh
/// 3 to 7 against it (spec 6.4). Every [`Schedule`] folds durations with this function.
pub fn fold_duration(typical: Option<u64>, duration_s: u64) -> u64 {
    match typical {
        None => duration_s,
        Some(old) => (old * 7 + duration_s * 3 + 5) / 10,
    }
}

/// Queue-first scheduling state of the VM pools (spec 6.4): the VM jobs waiting for
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
    /// Takes the pool's start marker at `now` unless one taken less than [`START_MARKER_S`]
    /// earlier exists; `true` when taken. Keeps two evaluations at once from starting VMs twice
    /// (spec 6.4).
    async fn take_start(&self, pool: &str, now: u64) -> Result<bool>;
}

/// Seconds a start marker of [`Schedule::take_start`] holds.
pub const START_MARKER_S: u64 = 15;

/// In-memory schedule for tests and the fake modules.
#[derive(Default)]
pub struct MemorySchedule {
    queued: Mutex<BTreeMap<(String, String), Queued>>,
    running: Mutex<BTreeMap<(String, String), Running>>,
    typical: Mutex<BTreeMap<(String, String), u64>>,
    started: Mutex<BTreeMap<String, u64>>,
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

    async fn take_start(&self, pool: &str, now: u64) -> Result<bool> {
        let mut started = self.started.lock().expect("schedule");
        match started.get(pool) {
            Some(at) if now.saturating_sub(*at) < START_MARKER_S => Ok(false),
            _ => {
                started.insert(pool.to_string(), now);
                Ok(true)
            }
        }
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

    #[tokio::test]
    async fn open_pipelines_list_the_vm_jobs_still_to_come() {
        let records = MemoryPipelineRecords::new().with_clock(std::sync::Arc::new(|| 500));
        let jobs: VmJobs = [
            ("vm".to_string(), vec!["11".to_string(), "12".to_string()]),
            ("big".to_string(), vec!["13".to_string()]),
        ]
        .into();
        records.open("9", &jobs).await.unwrap();
        records.open("8", &VmJobs::new()).await.unwrap();
        records.seen("9", "11").await.unwrap();
        records.seen("9", "13").await.unwrap();
        let mut open = records.open_pipelines().await.unwrap();
        open.sort_by(|a, b| a.pipeline.cmp(&b.pipeline));
        assert_eq!(
            open,
            [
                OpenPipeline {
                    pipeline: "8".into(),
                    opened: Some(500),
                    unseen: VmJobs::new(),
                },
                OpenPipeline {
                    pipeline: "9".into(),
                    opened: Some(500),
                    unseen: [("vm".to_string(), vec!["12".to_string()])].into(),
                },
            ]
        );
        records.close("9").await.unwrap();
        assert_eq!(records.open_pipelines().await.unwrap().len(), 1);
        let plain = MemoryPipelineRecords::new();
        plain.open("7", &jobs).await.unwrap();
        assert_eq!(plain.open_pipelines().await.unwrap()[0].opened, None);
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
        assert!(schedule.take_start("vm", 1000).await.unwrap());
        assert!(!schedule.take_start("vm", 1014).await.unwrap());
        assert!(schedule.take_start("big", 1014).await.unwrap());
        assert!(schedule.take_start("vm", 1015).await.unwrap());
    }
}
