//! Burst intents (spec D33, M4 rules R3 and R13): one record per pending job that found no free
//! slot, read back by every dispatcher call of the same burst. Records are append-only; the
//! order is time, then job id.

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
