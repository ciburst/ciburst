//! Host `scaleway-function`: the dispatcher as a Serverless Function built by Scaleway from a
//! zip embedded in this crate at build time. Implemented in M1 by agent B; see
//! `docs/milestones/M1-scaleway.md`.

use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::log::Log;
use ciburst_core::types::{Host, Outputs, Secrets, Step};
use ciburst_core::{Error, Result};

use crate::client::ScalewayClient;

pub const HOST_NAME: &str = "scaleway-function";

pub struct ScalewayFunction {
    config: Arc<Config>,
    client: Arc<ScalewayClient>,
    log: Log,
}

impl ScalewayFunction {
    pub fn new(config: Arc<Config>, client: Arc<ScalewayClient>, log: Log) -> ScalewayFunction {
        ScalewayFunction {
            config,
            client,
            log,
        }
    }

    fn todo(what: &str) -> Error {
        Error::provider(format!("{HOST_NAME}: {what} is implemented in M1"))
    }
}

#[async_trait]
impl Host for ScalewayFunction {
    async fn plan(&self, _secrets: &Secrets) -> Result<Vec<Step>> {
        let _ = (&self.config, &self.client, &self.log);
        Err(Self::todo("plan"))
    }

    async fn apply(&self, _steps: &[Step], _secrets: &Secrets) -> Result<Outputs> {
        Err(Self::todo("apply"))
    }

    async fn destroy(&self) -> Result<()> {
        Err(Self::todo("destroy"))
    }

    async fn url(&self) -> Result<Option<String>> {
        Err(Self::todo("url"))
    }
}
