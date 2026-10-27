//! `Provider::shared_env` of the Scaleway provider (D38, spec 9.1): what a VM needs besides
//! its own secrets, the agent's URL and SHA-256, the runner's URL and the cache location, the
//! same values the provider writes into `vm.env`. The port stays so a provider can return (D65).

use std::collections::BTreeMap;
use std::sync::Arc;

use ciburst_core::cloud_init::RUNNER_SHA256;
use ciburst_core::config::Config;
use ciburst_core::http::{HttpClient, Request};
use ciburst_core::log::Log;
use ciburst_core::testing::contract::contract_config;
use ciburst_core::types::{Provider, StartRequest};
use ciburst_scaleway::provider::Scaleway;
use ciburst_scaleway::testing::scaleway_fake::{FAKE_ACCESS_KEY, ScalewayFake};
use ciburst_scaleway::testing::{
    RUNNER_FIXTURE, RUNNER_FIXTURE_URL, agent_fixture, agent_fixture_sha256, runner_fixture_sha256,
};

fn provider(config: Arc<Config>, backend: &Arc<ScalewayFake>, log: Log) -> Scaleway {
    Scaleway::new(config, backend.client(), log)
        .with_s3_access_key(FAKE_ACCESS_KEY)
        .with_agent(Some(agent_fixture()))
        .with_runner_source(RUNNER_FIXTURE_URL, runner_fixture_sha256())
}

async fn provisioned(config: Arc<Config>) -> (Arc<ScalewayFake>, Scaleway, String) {
    let backend = Arc::new(ScalewayFake::new());
    let provider = provider(config, &backend, Log::silent());
    let outputs = provider
        .apply(&provider.plan(&[]).await.unwrap())
        .await
        .unwrap();
    let project_id = outputs["CIBURST_PROJECT_ID"].clone();
    (backend, provider, project_id)
}

fn request(config: &Config) -> StartRequest {
    let env = [
        ("RUNNER_TOKEN", "glrt-runner"),
        ("CACHE_S3_ACCESS_KEY", "SCWCACHEACCESS000000"),
        ("CACHE_S3_SECRET_KEY", "cache-secret"),
        ("CIBURST_VM_SECRET", "vm-secret"),
        ("CIBURST_DISPATCH_URL", "https://dispatch.example"),
    ];
    StartRequest {
        pool: config.vm_pool(),
        env: env
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        labels: BTreeMap::from([("job".to_string(), "2".to_string())]),
        taken: 1,
    }
}

#[tokio::test]
async fn shared_env_is_the_agent_and_the_cache_location_of_vm_env() {
    let config = contract_config();
    let (backend, scaleway, project_id) = provisioned(config.clone()).await;
    let shared = scaleway.shared_env().await.unwrap();
    let bucket = format!("contract-cache-{project_id}");
    let url = format!(
        "https://{bucket}.s3.fr-par.scw.cloud/ciburst/agent/{}",
        agent_fixture_sha256()
    );
    let sha = agent_fixture_sha256();
    let runner = format!(
        "https://{bucket}.s3.fr-par.scw.cloud/ciburst/runner/{}",
        runner_fixture_sha256()
    );
    let expected: BTreeMap<String, String> = [
        ("CIBURST_AGENT_URL", url.as_str()),
        ("CIBURST_AGENT_SHA256", sha.as_str()),
        ("CIBURST_RUNNER_URL", runner.as_str()),
        ("CACHE_TYPE", "s3"),
        ("CACHE_S3_SERVER_ADDRESS", "s3.fr-par.scw.cloud"),
        ("CACHE_S3_BUCKET_NAME", bucket.as_str()),
        ("CACHE_S3_BUCKET_LOCATION", "fr-par"),
        ("CACHE_SHARED", "true"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(shared, expected);

    let id = scaleway.start(request(&config)).await.unwrap();
    let user_data = backend.instance.user_data(&id, "cloud-init").unwrap();
    for (key, value) in shared.iter().filter(|(k, _)| k.starts_with("CACHE_")) {
        assert!(
            user_data.contains(&format!("{key}={value}\n")),
            "{key}={value} as in vm.env"
        );
    }
    assert!(user_data.contains(&format!("curl -fsSL -o /tmp/ciburst-vm-agent {url} ")));
    assert!(user_data.contains(&format!("curl -fsSL -o /tmp/gitlab-runner {runner} ")));
    let anonymous = backend
        .send(Request::new("GET", runner.as_str()))
        .await
        .unwrap();
    assert_eq!(
        (anonymous.status, anonymous.body.as_slice()),
        (200, RUNNER_FIXTURE),
        "VMs read the runner without credentials"
    );

    let dispatcher = Scaleway::new(config, backend.client(), Log::silent())
        .with_agent_sha256(Some("e".repeat(64)));
    let shared = dispatcher.shared_env().await.unwrap();
    assert_eq!(
        shared["CIBURST_AGENT_SHA256"],
        "e".repeat(64),
        "the function's environment wins"
    );
    assert_eq!(
        shared["CIBURST_RUNNER_URL"],
        format!("https://{bucket}.s3.fr-par.scw.cloud/ciburst/runner/{RUNNER_SHA256}"),
        "the function names the pinned runner"
    );
}

#[tokio::test]
async fn shared_env_names_what_is_missing() {
    let empty = Arc::new(ScalewayFake::new());
    let error = provider(contract_config(), &empty, Log::silent())
        .shared_env()
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway: shared environment: project contract does not exist"
    );
    let (backend, _, _) = provisioned(contract_config()).await;
    let error = Scaleway::new(contract_config(), backend.client(), Log::silent())
        .shared_env()
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway: shared environment: CIBURST_AGENT_SHA256 missing"
    );
}
