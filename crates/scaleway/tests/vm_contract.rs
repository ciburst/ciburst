//! The `instances` runtime against the VM provider contract, on the fake Instance API with the
//! agent and runner fixtures in the fake cache bucket.

use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::log::Log;
use ciburst_core::testing::vm_contract::{VmProviderFactory, run_vm_provider_contract};
use ciburst_core::types::Provider;
use ciburst_scaleway::provider::Scaleway;
use ciburst_scaleway::testing::scaleway_fake::{FAKE_ACCESS_KEY, ScalewayFake};
use ciburst_scaleway::testing::{RUNNER_FIXTURE_URL, agent_fixture, runner_fixture_sha256};

struct Instances;

#[async_trait]
impl VmProviderFactory for Instances {
    type Backend = Arc<ScalewayFake>;

    async fn backend(&self) -> Self::Backend {
        Arc::new(ScalewayFake::new())
    }

    fn config(&self, mut config: Config) -> Config {
        config.provider.kind = "scaleway".to_string();
        config
    }

    fn make(&self, config: Arc<Config>, backend: &Self::Backend) -> Arc<dyn Provider> {
        Arc::new(
            Scaleway::new(config, backend.client(), Log::silent())
                .with_s3_access_key(FAKE_ACCESS_KEY)
                .with_agent(Some(agent_fixture()))
                .with_runner_source(RUNNER_FIXTURE_URL, runner_fixture_sha256()),
        )
    }

    fn set_clock(&self, backend: &Self::Backend, unix: u64) {
        backend.instance.set_clock(unix);
    }
}

#[tokio::test]
async fn scaleway_instances_pass_the_vm_contract() {
    run_vm_provider_contract(&Instances).await;
}
