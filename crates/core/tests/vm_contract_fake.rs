use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::fake::FakeProvider;
use ciburst_core::store::MemoryStore;
use ciburst_core::testing::vm_contract::{VmProviderFactory, run_vm_provider_contract};
use ciburst_core::types::Provider;

struct FakeFactory;

#[async_trait]
impl VmProviderFactory for FakeFactory {
    type Backend = (Arc<MemoryStore>, Arc<AtomicU64>);

    async fn backend(&self) -> Self::Backend {
        (Arc::new(MemoryStore::new()), Arc::new(AtomicU64::new(0)))
    }

    fn make(&self, config: Arc<Config>, backend: &Self::Backend) -> Arc<dyn Provider> {
        let clock = backend.1.clone();
        Arc::new(
            FakeProvider::new(config, backend.0.clone())
                .with_clock(Arc::new(move || clock.load(Ordering::SeqCst))),
        )
    }

    fn set_clock(&self, backend: &Self::Backend, unix: u64) {
        backend.1.store(unix, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn the_fake_vm_provider_passes_the_vm_contract() {
    run_vm_provider_contract(&FakeFactory).await;
}
