//! Provider `scaleway-jobs`: one Serverless Jobs definition per pool, started per pending job.
//! Resources: project, IAM applications and keys, cache bucket with lifecycle, job definitions.
//! Implemented in M1 by agent A; see `docs/milestones/M1-scaleway.md`.

use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::log::Log;
use ciburst_core::types::{
    Billing, Capabilities, Outputs, Provider, Rotation, Run, StartRequest, Step,
};
use ciburst_core::{Error, Result};

use crate::client::ScalewayClient;

pub const PROVIDER_NAME: &str = "scaleway-jobs";

pub struct ScalewayJobs {
    config: Arc<Config>,
    client: Arc<ScalewayClient>,
    log: Log,
}

impl ScalewayJobs {
    pub fn new(config: Arc<Config>, client: Arc<ScalewayClient>, log: Log) -> ScalewayJobs {
        ScalewayJobs {
            config,
            client,
            log,
        }
    }

    fn todo(what: &str) -> Error {
        Error::provider(format!("{PROVIDER_NAME}: {what} is implemented in M1"))
    }
}

#[async_trait]
impl Provider for ScalewayJobs {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            name: PROVIDER_NAME.into(),
            docker_daemon: false,
            max_vcpu: 6,
            max_memory_mib: 16384,
            max_storage_mib: 10240,
            max_duration_s: 86400,
            archs: vec!["amd64".into()],
            billing: Billing::PerSecond,
        }
    }

    async fn plan(&self, _rotate: &[Rotation]) -> Result<Vec<Step>> {
        let _ = (&self.config, &self.client, &self.log);
        Err(Self::todo("plan"))
    }

    async fn apply(&self, _steps: &[Step]) -> Result<Outputs> {
        Err(Self::todo("apply"))
    }

    async fn destroy(&self) -> Result<()> {
        Err(Self::todo("destroy"))
    }

    async fn start(&self, _request: StartRequest) -> Result<String> {
        Err(Self::todo("start"))
    }

    async fn list(&self, _pool: Option<&str>) -> Result<Vec<Run>> {
        Err(Self::todo("list"))
    }

    async fn stop(&self, _run_id: &str) -> Result<()> {
        Err(Self::todo("stop"))
    }
}
