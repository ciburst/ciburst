//! The `hetzner` provider against the VM provider contract, on the fake Hetzner Cloud API.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::http::HttpClient;
use ciburst_core::log::Log;
use ciburst_core::testing::vm_contract::{VmProviderFactory, run_vm_provider_contract};
use ciburst_core::types::Provider;
use ciburst_hetzner::testing::hcloud_api::{FAKE_TOKEN, FakeHetzner};
use ciburst_hetzner::{Hetzner, RUNTIME};

struct HetznerFactory;

#[async_trait]
impl VmProviderFactory for HetznerFactory {
    type Backend = Arc<FakeHetzner>;

    async fn backend(&self) -> Self::Backend {
        Arc::new(FakeHetzner::new())
    }

    fn config(&self, mut config: Config) -> Config {
        config
            .providers
            .insert(RUNTIME.to_string(), serde_json::Map::new());
        for pool in &mut config.pools {
            pool.runtime = Some(RUNTIME.to_string());
        }
        config
    }

    fn make(&self, config: Arc<Config>, backend: &Self::Backend) -> Arc<dyn Provider> {
        let http: Arc<dyn HttpClient> = backend.clone();
        Arc::new(
            Hetzner::new(config, FAKE_TOKEN.to_string(), http, Log::silent())
                .with_retry_delay(Duration::ZERO),
        )
    }

    fn set_clock(&self, backend: &Self::Backend, unix: u64) {
        backend.set_clock(unix);
    }

    /// A Scaleway server id.
    fn foreign_run_id(&self) -> String {
        "0f6c1a52-7d1e-4b8e-9a43-2f1d5e0c9b11".to_string()
    }
}

#[tokio::test]
async fn hetzner_passes_the_vm_contract() {
    run_vm_provider_contract(&HetznerFactory).await;
}
