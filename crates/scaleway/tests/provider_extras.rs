//! The Scaleway provider as the main member of a composite (spec D35, M4 rules R8, R11, R13):
//! pools of other providers are none of its business, and its `shared_env` hands their VMs the
//! agent image and the cache location.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::composite::Composite;
use ciburst_core::config::{CacheType, Config};
use ciburst_core::log::Log;
use ciburst_core::testing::contract::contract_config;
use ciburst_core::types::{
    Billing, Capabilities, Outputs, Provider, Rotation, Run, StartRequest, Step,
};
use ciburst_scaleway::provider::Scaleway;
use ciburst_scaleway::testing::scaleway_fake::{FAKE_ACCESS_KEY, ScalewayFake};
use serde_json::{Map, json};

const AGENT_IMAGE: &str = "rg.fr-par.scw.cloud/contract/vm-agent:edge";

/// The contract config with a public registry; the `build` pool belongs to `hetzner`.
fn config(change: impl FnOnce(&mut Config)) -> Arc<Config> {
    let mut config = (*contract_config()).clone();
    config.provider.extra.insert("registry".into(), json!(true));
    config
        .provider
        .extra
        .insert("registryPublic".into(), json!(true));
    config.providers.insert("hetzner".into(), Map::new());
    config.pools[1].runtime = Some("hetzner".into());
    change(&mut config);
    Arc::new(config)
}

fn provider(config: Arc<Config>, backend: &Arc<ScalewayFake>, log: Log) -> Scaleway {
    Scaleway::new(config, backend.client(), log).with_s3_access_key(FAKE_ACCESS_KEY)
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
            "registry",
            "definition:ci"
        ]
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
async fn the_registry_warning_covers_vms_of_other_providers() {
    let backend = Arc::new(ScalewayFake::new());
    let private = config(|c| {
        c.pools[0].runtime = Some("jobs".into());
        c.provider
            .extra
            .insert("registryPublic".into(), json!(false));
    });
    let (log, lines) = Log::memory();
    provider(private, &backend, log).plan(&[]).await.unwrap();
    let lines = lines.lock().unwrap();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("warn vm pools cannot start"),
        "{lines:?}"
    );
}

#[tokio::test]
async fn shared_env_is_the_agent_image_and_the_cache_location_of_vm_env() {
    let plain = config(|_| {});
    let (backend, scaleway, project_id) = provisioned(plain.clone()).await;
    let shared = scaleway.shared_env().await.unwrap();
    let bucket = format!("contract-cache-{project_id}");
    let expected: BTreeMap<String, String> = [
        ("CIBURST_AGENT_IMAGE", AGENT_IMAGE),
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

    let id = scaleway.start(request(&plain, "ci")).await.unwrap();
    let user_data = backend.instance.user_data(&id, "cloud-init").unwrap();
    for (key, value) in shared.iter().filter(|(k, _)| k.starts_with("CACHE_")) {
        assert!(
            user_data.contains(&format!("{key}={value}\n")),
            "{key}={value} as in vm.env"
        );
    }
    assert!(user_data.contains(&format!("docker create {AGENT_IMAGE} ")));

    let no_cache = config(|c| c.cache.kind = CacheType::None);
    let (_, scaleway, _) = provisioned(no_cache).await;
    assert_eq!(
        scaleway.shared_env().await.unwrap(),
        BTreeMap::from([("CIBURST_AGENT_IMAGE".to_string(), AGENT_IMAGE.to_string())])
    );
}

#[tokio::test]
async fn shared_env_refuses_an_agent_image_vms_cannot_pull() {
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

    let off = config(|c| {
        c.provider.extra.insert("registry".into(), json!(false));
    });
    let (_, scaleway, _) = provisioned(off).await;
    let error = scaleway.shared_env().await.unwrap_err().to_string();
    assert_eq!(
        error,
        "scaleway: shared environment: VMs pull their agent from the registry namespace; set \
         provider.registry"
    );

    let private = config(|c| {
        c.provider
            .extra
            .insert("registryPublic".into(), json!(false));
    });
    let (backend, scaleway, project_id) = provisioned(private).await;
    let error = scaleway.shared_env().await.unwrap_err().to_string();
    assert_eq!(
        error,
        "scaleway: shared environment: VMs pull the agent image without credentials; set \
         provider.registryPublic"
    );
    assert!(!project_id.is_empty());
    assert_eq!(backend.registry.namespaces().len(), 1);
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
    assert_eq!(env["CIBURST_AGENT_IMAGE"], AGENT_IMAGE);
    assert_eq!(
        env["CACHE_S3_BUCKET_NAME"],
        format!("contract-cache-{project_id}")
    );
    assert_eq!(env["CACHE_TYPE"], "kept-from-request");
    assert_eq!(env["CACHE_S3_SECRET_KEY"], "cache-secret");
    assert_eq!(env["RUNNER_TOKEN"], "glrt-runner");
}
