use std::sync::Arc;

use ciburst_core::config::Config;
use ciburst_core::log::Log;
use ciburst_core::testing::contract::contract_config;
use ciburst_core::types::{Action, Provider, Step};
use ciburst_scaleway::provider::Scaleway;
use ciburst_scaleway::testing::scaleway_fake::ScalewayFake;
use serde_json::json;

fn config(registry: bool, public: bool) -> Arc<Config> {
    let mut config = (*contract_config()).clone();
    config
        .provider
        .extra
        .insert("registry".into(), json!(registry));
    config
        .provider
        .extra
        .insert("registryPublic".into(), json!(public));
    Arc::new(config)
}

fn provider(config: Arc<Config>, backend: &Arc<ScalewayFake>) -> Scaleway {
    Scaleway::new(config, backend.client(), Log::silent())
}

fn action(steps: &[Step], id: &str) -> Option<Action> {
    steps.iter().find(|s| s.id == id).map(|s| s.action)
}

#[tokio::test]
async fn registry_namespace_is_created_updated_and_deleted() {
    let backend = Arc::new(ScalewayFake::new());
    let private = provider(config(true, false), &backend);
    let steps = private.plan(&[]).await.unwrap();
    assert_eq!(action(&steps, "registry"), Some(Action::Create));
    let outputs = private.apply(&steps).await.unwrap();
    assert_eq!(
        outputs["CIBURST_REGISTRY_ENDPOINT"],
        "rg.fr-par.scw.cloud/contract"
    );
    assert_eq!(
        backend.registry.namespaces(),
        vec![("contract".to_string(), false)]
    );
    let again = private.plan(&[]).await.unwrap();
    assert!(again.iter().all(|s| s.action == Action::Noop), "{again:?}");

    let public = provider(config(true, true), &backend);
    let steps = public.plan(&[]).await.unwrap();
    assert_eq!(action(&steps, "registry"), Some(Action::Update));
    public.apply(&steps).await.unwrap();
    assert_eq!(
        backend.registry.namespaces(),
        vec![("contract".to_string(), true)]
    );

    let off = provider(config(false, false), &backend);
    let steps = off.plan(&[]).await.unwrap();
    assert_eq!(action(&steps, "registry"), Some(Action::Delete));
    off.apply(&steps).await.unwrap();
    assert!(backend.registry.namespaces().is_empty());
    assert_eq!(action(&off.plan(&[]).await.unwrap(), "registry"), None);
}

#[tokio::test]
async fn registry_is_absent_by_default_and_removed_by_destroy() {
    let backend = Arc::new(ScalewayFake::new());
    let plain = provider(contract_config(), &backend);
    assert_eq!(action(&plain.plan(&[]).await.unwrap(), "registry"), None);
    let with_registry = provider(config(true, false), &backend);
    let steps = with_registry.plan(&[]).await.unwrap();
    with_registry.apply(&steps).await.unwrap();
    assert_eq!(backend.registry.namespaces().len(), 1);
    with_registry.destroy().await.unwrap();
    assert!(backend.registry.namespaces().is_empty());
}
