//! The provider contract. Every provider crate runs [`run_provider_contract`] in its tests.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{Config, parse_config};
use crate::types::{Action, Provider, Rotation, StartRequest};

pub const CONTRACT_CONFIG: &str = r#"{
  "name": "contract",
  "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
  "host": { "type": "fake" },
  "provider": { "type": "fake" },
  "vm": { "image": "example.invalid/node:24" },
  "lanes": [
    { "tag": "ci", "image": "example.invalid/node:24", "jobs": ["test-*"] },
    { "tag": "build", "image": "example.invalid/node:24", "jobs": ["build-*"], "maxRuns": 1 }
  ],
  "cache": { "type": "s3", "expireDays": 14 }
}"#;

pub fn contract_config() -> Arc<Config> {
    Arc::new(parse_config(CONTRACT_CONFIG).expect("contract config"))
}

/// The contract config with the lane `ci` changed and the lane `build` removed.
pub fn changed_config() -> Arc<Config> {
    let mut config = (*contract_config()).clone();
    config.lanes.truncate(1);
    config.lanes[0].vcpu = 4;
    Arc::new(config)
}

/// Builds providers for the contract. `backend()` yields an empty backend; `make` binds a
/// provider for a config to it, so that a changed config is planned against existing resources.
#[async_trait]
pub trait ProviderFactory: Send + Sync {
    type Backend: Send + Sync;
    async fn backend(&self) -> Self::Backend;
    fn make(&self, config: Arc<Config>, backend: &Self::Backend) -> Arc<dyn Provider>;
}

fn env() -> BTreeMap<String, String> {
    BTreeMap::from([("RUNNER_TOKEN".to_string(), "glrt-contract".to_string())])
}

fn labels() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("project".to_string(), "7".to_string()),
        ("pipeline".to_string(), "1".to_string()),
        ("job".to_string(), "2".to_string()),
        ("stage".to_string(), "test".to_string()),
    ])
}

pub async fn run_provider_contract<F: ProviderFactory>(factory: &F) {
    let config = contract_config();

    // capabilities
    {
        let backend = factory.backend().await;
        let caps = factory.make(config.clone(), &backend).capabilities();
        assert!(!caps.name.is_empty(), "capabilities.name");
        assert!(
            caps.max_vcpu > 0 && caps.max_memory_mib > 0,
            "capabilities limits"
        );
        assert!(
            caps.max_storage_mib > 0 && caps.max_duration_s > 0,
            "capabilities limits"
        );
        assert!(!caps.archs.is_empty(), "capabilities.archs");
    }

    // plan everything on an empty backend, nothing after apply
    {
        let backend = factory.backend().await;
        let provider = factory.make(config.clone(), &backend);
        let first = provider.plan(&[]).await.expect("plan");
        assert!(
            first.len() >= config.lanes.len(),
            "one step per lane at least"
        );
        assert!(
            first.iter().all(|s| s.action == Action::Create),
            "all create on empty backend: {first:?}"
        );
        for pool in &config.pools()[1..] {
            assert!(
                first
                    .iter()
                    .any(|s| s.id == format!("definition:{}", pool.tag)),
                "definition step for {}",
                pool.tag
            );
        }
        provider.apply(&first).await.expect("apply");
        let second = provider.plan(&[]).await.expect("plan again");
        assert!(
            second.iter().all(|s| s.action == Action::Noop),
            "all noop after apply: {second:?}"
        );
        assert_eq!(second.len(), first.len(), "same steps after apply");
    }

    // changed pool updates, removed pool deletes
    {
        let backend = factory.backend().await;
        let provider = factory.make(config.clone(), &backend);
        let steps = provider.plan(&[]).await.expect("plan");
        provider.apply(&steps).await.expect("apply");
        let again = factory.make(changed_config(), &backend);
        let steps = again.plan(&[]).await.expect("plan changed");
        assert!(
            steps
                .iter()
                .any(|s| s.id == "definition:7-ci" && s.action == Action::Update),
            "update for the changed lane: {steps:?}"
        );
        assert!(
            steps
                .iter()
                .any(|s| s.id == "definition:7-build" && s.action == Action::Delete),
            "delete for the removed lane: {steps:?}"
        );
        again.apply(&steps).await.expect("apply changed");
        let after = again.plan(&[]).await.expect("plan after change");
        assert!(
            after.iter().all(|s| s.action == Action::Noop),
            "noop after change: {after:?}"
        );
    }

    // rotation returns new secrets
    {
        let backend = factory.backend().await;
        let provider = factory.make(config.clone(), &backend);
        let steps = provider.plan(&[]).await.expect("plan");
        let created = provider.apply(&steps).await.expect("apply");
        let steps = provider
            .plan(&[Rotation::Key, Rotation::Cache])
            .await
            .expect("plan rotate");
        let rotated = provider.apply(&steps).await.expect("apply rotate");
        for key in ["CIBURST_DISPATCH_SECRET_KEY", "CIBURST_CACHE_SECRET_KEY"] {
            assert!(created.contains_key(key), "{key} on create");
            assert!(rotated.contains_key(key), "{key} on rotate");
            assert_ne!(created[key], rotated[key], "{key} changed by rotation");
        }
    }

    // start refuses an unprovisioned lane and a missing token
    {
        let backend = factory.backend().await;
        let provider = factory.make(config.clone(), &backend);
        let request = StartRequest {
            pool: config.lanes[0].pool(7),
            env: env(),
            labels: labels(),
            taken: 1,
        };
        assert!(
            provider.start(request.clone()).await.is_err(),
            "start before apply"
        );
        let steps = provider.plan(&[]).await.expect("plan");
        provider.apply(&steps).await.expect("apply");
        let no_token = StartRequest {
            env: BTreeMap::new(),
            ..request
        };
        assert!(
            provider.start(no_token).await.is_err(),
            "start without token"
        );
    }

    // start, list, stop
    {
        let backend = factory.backend().await;
        let provider = factory.make(config.clone(), &backend);
        let steps = provider.plan(&[]).await.expect("plan");
        provider.apply(&steps).await.expect("apply");
        let run_id = provider
            .start(StartRequest {
                pool: config.lanes[0].pool(7),
                env: env(),
                labels: labels(),
                taken: 1,
            })
            .await
            .expect("start");
        assert!(!run_id.is_empty());
        let listed = provider.list(Some("7-ci")).await.expect("list");
        let run = listed
            .iter()
            .find(|r| r.run_id == run_id)
            .expect("run listed");
        assert!(run.state.is_live(), "run live after start");
        assert_eq!(run.labels, labels());
        assert!(
            provider
                .list(Some("7-build"))
                .await
                .expect("list build")
                .is_empty()
        );
        provider.stop(&run_id).await.expect("stop");
        let after = provider.list(None).await.expect("list all");
        assert!(
            after
                .iter()
                .all(|r| r.run_id != run_id || !r.state.is_live()),
            "run not live after stop"
        );
    }

    // destroy starts over
    {
        let backend = factory.backend().await;
        let provider = factory.make(config.clone(), &backend);
        let steps = provider.plan(&[]).await.expect("plan");
        provider.apply(&steps).await.expect("apply");
        provider.destroy().await.expect("destroy");
        let steps = provider.plan(&[]).await.expect("plan after destroy");
        assert!(
            steps.iter().all(|s| s.action == Action::Create),
            "{steps:?}"
        );
    }
}
