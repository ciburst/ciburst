use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::fake::FakeProvider;
use ciburst_core::store::MemoryStore;
use ciburst_core::testing::contract::{ProviderFactory, run_provider_contract};
use ciburst_core::types::Provider;

struct FakeFactory;

#[async_trait]
impl ProviderFactory for FakeFactory {
    type Backend = Arc<MemoryStore>;

    async fn backend(&self) -> Self::Backend {
        Arc::new(MemoryStore::new())
    }

    fn make(&self, config: Arc<Config>, backend: &Self::Backend) -> Arc<dyn Provider> {
        Arc::new(FakeProvider::new(config, backend.clone()))
    }
}

#[tokio::test]
async fn fake_provider_passes_the_contract() {
    run_provider_contract(&FakeFactory).await;
}
