//! Provider `hetzner` (spec section 6.2, D34, M4 rule R12): VM pools on Hetzner Cloud servers
//! with the shared cloud-init and agent of spec section 6.1, state in server labels, and a
//! firewall without inbound rules. The dispatcher, cache and registry stay with the main
//! provider; `Composite` hands this provider the pools whose `runtime` is [`RUNTIME`].

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::http::HttpClient;
use ciburst_core::log::Log;
use ciburst_core::types::{
    Billing, Capabilities, Outputs, Provider, Rotation, Run, StartRequest, Step, VmCapacity,
    VmReport,
};
use ciburst_core::{Error, Result};

/// Runtime name, key in the config's `providers`, and prefix of run and step ids.
pub const RUNTIME: &str = "hetzner";
/// The API token's variable in `.local/hetzner.env`, the function's secret environment and
/// the provider's outputs.
pub const TOKEN_VAR: &str = "HCLOUD_TOKEN";
pub const API: &str = "https://api.hetzner.cloud/v1";

pub struct Hetzner {
    config: Arc<Config>,
    token: String,
    #[allow(dead_code)]
    http: Arc<dyn HttpClient>,
    #[allow(dead_code)]
    log: Log,
}

impl Hetzner {
    pub fn new(config: Arc<Config>, token: String, http: Arc<dyn HttpClient>, log: Log) -> Hetzner {
        Hetzner {
            config,
            token,
            http,
            log,
        }
    }

    fn unimplemented() -> Error {
        Error::provider(format!("{RUNTIME}: not implemented yet (M4 agent C)"))
    }
}

#[async_trait]
impl Provider for Hetzner {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            name: RUNTIME.to_string(),
            docker_daemon: true,
            max_vcpu: 16,
            max_memory_mib: 32_768,
            max_storage_mib: 327_680,
            max_duration_s: 86_400,
            archs: vec!["amd64".into()],
            billing: Billing::PerHour,
        }
    }

    async fn plan(&self, _rotate: &[Rotation]) -> Result<Vec<Step>> {
        let _ = &self.config;
        Ok(Vec::new())
    }

    async fn apply(&self, _steps: &[Step]) -> Result<Outputs> {
        Ok(BTreeMap::from([(
            TOKEN_VAR.to_string(),
            self.token.clone(),
        )]))
    }

    async fn destroy(&self) -> Result<()> {
        Err(Hetzner::unimplemented())
    }

    async fn start(&self, _request: StartRequest) -> Result<String> {
        Err(Hetzner::unimplemented())
    }

    async fn list(&self, _pool: Option<&str>) -> Result<Vec<Run>> {
        Ok(Vec::new())
    }

    async fn stop(&self, _run_id: &str) -> Result<()> {
        Err(Hetzner::unimplemented())
    }

    async fn capacity(&self, _pool: &str) -> Result<Option<Vec<VmCapacity>>> {
        Ok(Some(Vec::new()))
    }

    async fn claim(&self, _run_id: &str) -> Result<()> {
        Err(Hetzner::unimplemented())
    }

    async fn report(&self, _run_id: &str, _report: &VmReport, _now: u64) -> Result<()> {
        Err(Hetzner::unimplemented())
    }

    async fn sweep(&self, _now: u64) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}
