//! The Scaleway provider as the main member of a composite (spec D35, D38, M4 rules R8, R11,
//! R13): pools of other providers are none of its business, and its `shared_env` hands their
//! VMs the agent's URL and SHA-256 and the cache location.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::composite::Composite;
use ciburst_core::config::{CacheType, Config};
use ciburst_core::log::Log;
use ciburst_core::testing::contract::contract_config;
use ciburst_core::types::{
    Action, Billing, Capabilities, Outputs, Provider, Rotation, Run, StartRequest, Step,
};
use ciburst_scaleway::provider::Scaleway;
use ciburst_scaleway::testing::scaleway_fake::{FAKE_ACCESS_KEY, ScalewayFake};
use ciburst_scaleway::testing::{agent_fixture, agent_fixture_sha256};
use serde_json::Map;

/// The contract config (S3 cache); the `build` pool belongs to `hetzner`.
fn config(change: impl FnOnce(&mut Config)) -> Arc<Config> {
    let mut config = (*contract_config()).clone();
    config.providers.insert("hetzner".into(), Map::new());
    config.pools[1].runtime = Some("hetzner".into());
    change(&mut config);
    Arc::new(config)
}

fn provider(config: Arc<Config>, backend: &Arc<ScalewayFake>, log: Log) -> Scaleway {
    Scaleway::new(config, backend.client(), log)
        .with_s3_access_key(FAKE_ACCESS_KEY)
        .with_agent(Some(agent_fixture()))
}

fn agent_url(project_id: &str) -> String {
    format!(
        "https://contract-cache-{project_id}.s3.fr-par.scw.cloud/ciburst/agent/{}",
        agent_fixture_sha256()
    )
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

fn request(config: &Config, tag: &str) -> StartRequest {
    let env = [
        ("RUNNER_TOKEN", "glrt-runner"),
        ("CACHE_S3_ACCESS_KEY", "SCWCACHEACCESS000000"),
        ("CACHE_S3_SECRET_KEY", "cache-secret"),
        ("CIBURST_VM_SECRET", "vm-secret"),
        ("CIBURST_DISPATCH_URL", "https://dispatch.example"),
    ];
    StartRequest {
        pool: config.pool_by_tag(tag).unwrap().clone(),
        env: env
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        labels: BTreeMap::from([("job".to_string(), "2".to_string())]),
        taken: 1,
    }
}

fn ids(steps: &[Step]) -> Vec<&str> {
    steps.iter().map(|s| s.id.as_str()).collect()
}

#[tokio::test]
async fn pools_of_other_providers_get_no_steps_no_warning_and_no_runs() {
    let backend = Arc::new(ScalewayFake::new());
    for quota in ["cp_servers_type_DEV1_M", "cp_servers_type_DEV1_L"] {
        backend.iam.set_zone_quota(quota, &[("fr-par-1", 0)]);
    }
    let jobs_and_hetzner = config(|c| c.pools[0].runtime = Some("jobs".into()));
    let (log, lines) = Log::memory();
    let scaleway = provider(jobs_and_hetzner.clone(), &backend, log);
    let steps = scaleway.plan(&[]).await.unwrap();
    assert_eq!(
        ids(&steps),
        [
            "project",
            "key:dispatch",
            "key:cache",
            "bucket",
            "vm:agent",
            "definition:ci"
        ],
        "the hetzner pool's VMs fetch the agent from this bucket"
    );
    assert!(
        lines.lock().unwrap().is_empty(),
        "{:?}",
        lines.lock().unwrap()
    );
    assert!(
        !backend
            .iam
            .calls()
            .iter()
            .any(|c| c.path.contains("/quota/")),
        "no quota lookup for the hetzner pool"
    );
    scaleway.apply(&steps).await.unwrap();
    let definitions: Vec<String> = backend
        .jobs
        .definitions()
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(definitions, ["contract-ci"]);
    assert!(scaleway.plan(&[]).await.unwrap().iter().all(Step::is_noop));

    let writes = backend.writes().len();
    let error = scaleway
        .start(request(&jobs_and_hetzner, "build"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "scaleway: pool build belongs to provider hetzner");
    assert_eq!(backend.writes().len(), writes, "nothing started");
    assert!(scaleway.list(Some("build")).await.unwrap().is_empty());
    assert_eq!(scaleway.capacity("build").await.unwrap(), Some(Vec::new()));
}

#[tokio::test]
async fn a_pool_moved_to_another_provider_loses_its_definition_and_keeps_its_servers() {
    let all_jobs = config(|c| {
        c.providers.clear();
        c.pools[1].runtime = Some("jobs".into());
        c.pools[0].runtime = Some("jobs".into());
    });
    let (backend, jobs, project_id) = provisioned(all_jobs).await;
    assert_eq!(backend.jobs.definitions().len(), 2);
    let leftover = backend.instance.add_server(
        "contract-build-0000aaaa",
        &project_id,
        &["ciburst", "contract", "pool:build", "ciburst-state:idle"],
    );
    assert!(
        jobs.list(None)
            .await
            .unwrap()
            .iter()
            .any(|r| r.run_id == leftover)
    );

    let moved = config(|c| c.pools[0].runtime = Some("jobs".into()));
    let scaleway = provider(moved, &backend, Log::silent());
    let steps = scaleway.plan(&[]).await.unwrap();
    let build = steps.iter().find(|s| s.id == "definition:build").unwrap();
    assert_eq!(build.action, ciburst_core::types::Action::Delete);
    scaleway.apply(&steps).await.unwrap();
    assert_eq!(backend.jobs.definitions().len(), 1);
    assert!(scaleway.list(Some("build")).await.unwrap().is_empty());
    let all: Vec<String> = scaleway
        .list(None)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.run_id)
        .collect();
    assert!(all.contains(&leftover), "a server here stays a run here");
}

#[tokio::test]
async fn the_runtime_name_of_another_provider_is_unknown_without_its_provider() {
    let backend = Arc::new(ScalewayFake::new());
    let orphan = config(|c| c.providers.clear());
    let error = provider(orphan, &backend, Log::silent())
        .plan(&[])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway: pool build: runtime hetzner is not instances or jobs"
    );
}

#[tokio::test]
async fn vms_of_other_providers_need_the_agent_and_the_s3_cache() {
    let backend = Arc::new(ScalewayFake::new());
    let jobs_and_hetzner = config(|c| c.pools[0].runtime = Some("jobs".into()));
    let (log, lines) = Log::memory();
    let without_agent = Scaleway::new(jobs_and_hetzner.clone(), backend.client(), log);
    let steps = without_agent.plan(&[]).await.unwrap();
    let agent = steps.iter().find(|s| s.id == "vm:agent").unwrap();
    assert_eq!(agent.action, Action::Create);
    let logged = lines.lock().unwrap().clone();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(
        logged[0].starts_with("warn vm pools cannot start"),
        "{logged:?}"
    );
    assert!(without_agent.preflight().await.is_err());

    let no_cache = config(|c| {
        c.pools[0].runtime = Some("jobs".into());
        c.cache.kind = CacheType::None;
    });
    let error = provider(no_cache, &backend, Log::silent())
        .plan(&[])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway: VM pools need cache.type s3: VMs fetch their agent from the cache bucket"
    );
    let only_jobs = config(|c| {
        c.providers.clear();
        c.pools[0].runtime = Some("jobs".into());
        c.pools[1].runtime = Some("jobs".into());
        c.cache.kind = CacheType::None;
    });
    let steps = provider(only_jobs, &backend, Log::silent())
        .plan(&[])
        .await
        .unwrap();
    assert!(steps.iter().all(|s| s.id != "vm:agent"), "{steps:?}");
}

#[tokio::test]
async fn shared_env_is_the_agent_and_the_cache_location_of_vm_env() {
    let plain = config(|_| {});
    let (backend, scaleway, project_id) = provisioned(plain.clone()).await;
    let shared = scaleway.shared_env().await.unwrap();
    let bucket = format!("contract-cache-{project_id}");
    let url = agent_url(&project_id);
    let sha = agent_fixture_sha256();
    let expected: BTreeMap<String, String> = [
        ("CIBURST_AGENT_URL", url.as_str()),
        ("CIBURST_AGENT_SHA256", sha.as_str()),
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
    assert!(!shared.contains_key("CIBURST_AGENT_IMAGE"));

    let id = scaleway.start(request(&plain, "ci")).await.unwrap();
    let user_data = backend.instance.user_data(&id, "cloud-init").unwrap();
    for (key, value) in shared.iter().filter(|(k, _)| k.starts_with("CACHE_")) {
        assert!(
            user_data.contains(&format!("{key}={value}\n")),
            "{key}={value} as in vm.env"
        );
    }
    assert!(user_data.contains(&format!("curl -fsSL -o /tmp/ciburst-vm-agent {url} ")));

    let dispatcher = Scaleway::new(plain, backend.client(), Log::silent())
        .with_agent_sha256(Some("e".repeat(64)));
    assert_eq!(
        dispatcher.shared_env().await.unwrap()["CIBURST_AGENT_SHA256"],
        "e".repeat(64),
        "the function's environment wins"
    );
}

#[tokio::test]
async fn shared_env_names_what_is_missing() {
    let empty = Arc::new(ScalewayFake::new());
    let error = provider(config(|_| {}), &empty, Log::silent())
        .shared_env()
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway: shared environment: project contract does not exist"
    );
    let (backend, _, _) = provisioned(config(|_| {})).await;
    let error = Scaleway::new(config(|_| {}), backend.client(), Log::silent())
        .shared_env()
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway: shared environment: CIBURST_AGENT_SHA256 missing"
    );
}

/// An added member that only records what it is asked to start.
#[derive(Default)]
struct Recorder {
    started: Mutex<Vec<StartRequest>>,
}

#[async_trait]
impl Provider for Recorder {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            name: "hetzner".into(),
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
        Ok(Vec::new())
    }
    async fn apply(&self, _steps: &[Step]) -> Result<Outputs> {
        Ok(Outputs::new())
    }
    async fn destroy(&self) -> Result<()> {
        Ok(())
    }
    async fn start(&self, request: StartRequest) -> Result<String> {
        self.started.lock().unwrap().push(request);
        Ok("hetzner:contract-build-00000001".into())
    }
    async fn list(&self, _pool: Option<&str>) -> Result<Vec<Run>> {
        Ok(Vec::new())
    }
    async fn stop(&self, _run_id: &str) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_composite_start_on_another_provider_carries_the_shared_env() {
    let plain = config(|_| {});
    let (backend, scaleway, project_id) = provisioned(plain.clone()).await;
    let recorder = Arc::new(Recorder::default());
    let extra: Arc<dyn Provider> = recorder.clone();
    let composite = Composite::new(
        plain.clone(),
        Arc::new(scaleway),
        vec![("hetzner".to_string(), extra)],
        Log::silent(),
    );
    let writes = backend.writes().len();
    let mut start = request(&plain, "build");
    start
        .env
        .insert("CACHE_TYPE".to_string(), "kept-from-request".to_string());
    let run = composite.start(start).await.unwrap();
    assert_eq!(run, "hetzner:contract-build-00000001");
    assert_eq!(backend.writes().len(), writes, "no Scaleway server");
    let started = recorder.started.lock().unwrap();
    let env = &started[0].env;
    assert_eq!(env["CIBURST_AGENT_URL"], agent_url(&project_id));
    assert_eq!(env["CIBURST_AGENT_SHA256"], agent_fixture_sha256());
    assert_eq!(
        env["CACHE_S3_BUCKET_NAME"],
        format!("contract-cache-{project_id}")
    );
    assert_eq!(env["CACHE_TYPE"], "kept-from-request");
    assert_eq!(env["CACHE_S3_SECRET_KEY"], "cache-secret");
    assert_eq!(env["RUNNER_TOKEN"], "glrt-runner");
}
