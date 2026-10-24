use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::config::{CacheType, Config};
use ciburst_core::http::{HttpClient, Request};
use ciburst_core::log::Log;
use ciburst_core::testing::contract::{
    ProviderFactory, changed_config, contract_config, run_provider_contract,
};
use ciburst_core::types::{Action, Provider, Rotation, RunState, StartRequest, Step};
use ciburst_scaleway::client::{Credentials, ScalewayClient};
use ciburst_scaleway::provider::ScalewayJobs;
use ciburst_scaleway::provider::s3::{S3, lifecycle_xml};
use ciburst_scaleway::testing::scaleway_fake::{
    FAKE_ACCESS_KEY, FAKE_REGION, FAKE_SECRET_KEY, ScalewayFake,
};
use serde_json::json;

struct Factory;

#[async_trait]
impl ProviderFactory for Factory {
    type Backend = Arc<ScalewayFake>;

    async fn backend(&self) -> Self::Backend {
        Arc::new(ScalewayFake::new())
    }

    fn make(&self, config: Arc<Config>, backend: &Self::Backend) -> Arc<dyn Provider> {
        Arc::new(provider(config, backend, Log::silent()))
    }
}

/// The config with `provider.runtime: jobs`: the contract and these tests cover Serverless
/// Jobs; `provider_instances.rs` covers the default runtime.
fn jobs(config: Arc<Config>) -> Arc<Config> {
    let mut config = (*config).clone();
    config
        .provider
        .extra
        .insert("runtime".into(), json!("jobs"));
    Arc::new(config)
}

fn provider(config: Arc<Config>, backend: &Arc<ScalewayFake>, log: Log) -> ScalewayJobs {
    ScalewayJobs::new(jobs(config), backend.client(), log).with_s3_access_key(FAKE_ACCESS_KEY)
}

fn with_credentials(
    config: Arc<Config>,
    backend: &Arc<ScalewayFake>,
    credentials: Credentials,
    log: Log,
) -> ScalewayJobs {
    let client = ScalewayClient::new(backend.clone(), credentials, FAKE_REGION);
    ScalewayJobs::new(jobs(config), Arc::new(client), log)
}

fn cache_off() -> Arc<Config> {
    let mut config = (*contract_config()).clone();
    config.cache.kind = CacheType::None;
    Arc::new(config)
}

fn actions(steps: &[Step]) -> Vec<(String, Action)> {
    steps.iter().map(|s| (s.id.clone(), s.action)).collect()
}

async fn provisioned(log: Log) -> (Arc<ScalewayFake>, ScalewayJobs, BTreeMap<String, String>) {
    let backend = Arc::new(ScalewayFake::new());
    let provider = provider(contract_config(), &backend, log);
    let steps = provider.plan(&[]).await.expect("plan");
    let outputs = provider.apply(&steps).await.expect("apply");
    (backend, provider, outputs)
}

fn labels() -> BTreeMap<String, String> {
    [
        ("project", "7"),
        ("pipeline", "1"),
        ("job", "2"),
        ("stage", "test"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn start_request(tag: usize) -> StartRequest {
    StartRequest {
        pool: contract_config().pools[tag].clone(),
        env: BTreeMap::from([("RUNNER_TOKEN".to_string(), "glrt-test".to_string())]),
        labels: labels(),
        taken: 1,
    }
}

#[tokio::test]
async fn scaleway_jobs_passes_the_contract() {
    run_provider_contract(&Factory).await;
}

#[tokio::test]
async fn first_plan_lists_every_resource_in_order() {
    let backend = Arc::new(ScalewayFake::new());
    let steps = provider(contract_config(), &backend, Log::silent())
        .plan(&[])
        .await
        .unwrap();
    assert_eq!(
        actions(&steps),
        [
            ("project".to_string(), Action::Create),
            ("key:dispatch".to_string(), Action::Create),
            ("key:cache".to_string(), Action::Create),
            ("bucket".to_string(), Action::Create),
            ("definition:ci".to_string(), Action::Create),
            ("definition:build".to_string(), Action::Create),
        ]
    );
    assert!(backend.writes().is_empty(), "plan writes nothing");
}

#[tokio::test]
async fn apply_creates_the_resources_of_the_spec() {
    let (backend, _, outputs) = provisioned(Log::silent()).await;
    let projects = backend.account.projects();
    assert_eq!(projects.len(), 1);
    let (project_id, project_name) = &projects[0];
    assert_eq!(project_name, "contract");
    assert_eq!(outputs["CIBURST_PROJECT_ID"], *project_id);

    let apps: Vec<String> = backend
        .iam
        .applications()
        .iter()
        .map(|a| a["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(apps, ["contract-dispatch", "contract-cache"]);
    let rules: Vec<_> = backend
        .iam
        .policies()
        .iter()
        .map(|p| (p["name"].clone(), p["rules"].clone()))
        .collect();
    assert_eq!(
        rules,
        [
            (
                json!("contract-dispatch"),
                json!([{ "permission_set_names": ["ServerlessJobsFullAccess", "ContainerRegistryReadOnly", "InstancesFullAccess"], "project_ids": [project_id] }])
            ),
            (
                json!("contract-cache"),
                json!([{ "permission_set_names": ["ObjectStorageFullAccess"], "project_ids": [project_id] }])
            ),
        ]
    );
    let keys = backend.iam.api_keys();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().all(|k| k["default_project_id"] == *project_id));
    assert!(keys.iter().all(|k| k["secret_key"].is_null()));
    for key in [
        "CIBURST_DISPATCH_ACCESS_KEY",
        "CIBURST_DISPATCH_SECRET_KEY",
        "CIBURST_CACHE_ACCESS_KEY",
        "CIBURST_CACHE_SECRET_KEY",
    ] {
        assert!(!outputs[key].is_empty(), "{key}");
    }

    let bucket = format!("contract-cache-{project_id}");
    assert_eq!(backend.s3.buckets(), std::slice::from_ref(&bucket));
    assert_eq!(backend.s3.lifecycle(&bucket), Some(lifecycle_xml(14)));
    assert!(
        backend
            .s3
            .calls()
            .iter()
            .all(|c| c.path.starts_with(&format!("/{bucket}"))),
        "path-style requests"
    );

    let definitions = backend.jobs.definitions();
    assert_eq!(definitions.len(), 2);
    let ci = &definitions[0];
    assert_eq!(ci["name"], "contract-ci");
    assert_eq!(ci["project_id"], *project_id);
    assert_eq!(ci["cpu_limit"], 2000);
    assert_eq!(ci["memory_limit"], 4096);
    assert_eq!(ci["local_storage_capacity"], 10240);
    assert_eq!(ci["image_uri"], "example.invalid/node:24");
    assert_eq!(ci["job_timeout"], "3600s");
    assert_eq!(ci["environment_variables"]["CACHE_S3_BUCKET_NAME"], bucket);
    assert_eq!(
        ci["environment_variables"]["CACHE_S3_SERVER_ADDRESS"],
        "s3.fr-par.scw.cloud"
    );
}

#[tokio::test]
async fn second_apply_is_noop_and_returns_no_secret() {
    let (backend, provider, _) = provisioned(Log::silent()).await;
    let writes = backend.writes().len();
    let steps = provider.plan(&[]).await.unwrap();
    assert!(steps.iter().all(Step::is_noop), "{steps:?}");
    let outputs = provider.apply(&steps).await.unwrap();
    assert_eq!(outputs.keys().collect::<Vec<_>>(), ["CIBURST_PROJECT_ID"]);
    assert_eq!(backend.writes().len(), writes, "no write after noop");
}

#[tokio::test]
async fn rotating_the_dispatch_key_replaces_only_that_key() {
    let (backend, provider, created) = provisioned(Log::silent()).await;
    let steps = provider.plan(&[Rotation::Key]).await.unwrap();
    let rotate = steps.iter().find(|s| s.id == "key:dispatch").unwrap();
    assert_eq!(rotate.action, Action::Update);
    assert_eq!(rotate.detail.as_deref(), Some("rotate"));
    assert!(
        steps
            .iter()
            .filter(|s| s.id != "key:dispatch")
            .all(Step::is_noop)
    );
    let rotated = provider.apply(&steps).await.unwrap();
    assert_ne!(
        rotated["CIBURST_DISPATCH_SECRET_KEY"],
        created["CIBURST_DISPATCH_SECRET_KEY"]
    );
    assert!(!rotated.contains_key("CIBURST_CACHE_SECRET_KEY"));
    let access_keys: Vec<String> = backend
        .iam
        .api_keys()
        .iter()
        .map(|k| k["access_key"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(access_keys.len(), 2, "old dispatch key deleted");
    assert!(access_keys.contains(&rotated["CIBURST_DISPATCH_ACCESS_KEY"]));
    assert!(access_keys.contains(&created["CIBURST_CACHE_ACCESS_KEY"]));
    assert!(!access_keys.contains(&created["CIBURST_DISPATCH_ACCESS_KEY"]));
}

#[tokio::test]
async fn changed_pool_updates_and_removed_pool_deletes() {
    let (backend, _, _) = provisioned(Log::silent()).await;
    let changed = provider(changed_config(), &backend, Log::silent());
    let steps = changed.plan(&[]).await.unwrap();
    let ci = steps.iter().find(|s| s.id == "definition:ci").unwrap();
    assert_eq!(ci.action, Action::Update);
    assert_eq!(ci.detail.as_deref(), Some("cpu_limit"));
    let build = steps.iter().find(|s| s.id == "definition:build").unwrap();
    assert_eq!(build.action, Action::Delete);
    assert_eq!(build.resource, "job definition contract-build");
    changed.apply(&steps).await.unwrap();
    let definitions = backend.jobs.definitions();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0]["cpu_limit"], 4000);
}

#[tokio::test]
async fn lifecycle_drift_plans_a_bucket_update() {
    let (backend, provider, _) = provisioned(Log::silent()).await;
    let bucket = backend.s3.buckets()[0].clone();
    backend.s3.set_lifecycle(&bucket, Some(lifecycle_xml(3)));
    let steps = provider.plan(&[]).await.unwrap();
    let step = steps.iter().find(|s| s.id == "bucket").unwrap();
    assert_eq!(step.action, Action::Update);
    provider.apply(&steps).await.unwrap();
    assert_eq!(backend.s3.lifecycle(&bucket), Some(lifecycle_xml(14)));
    backend.s3.set_lifecycle(&bucket, None);
    let steps = provider.plan(&[]).await.unwrap();
    assert_eq!(
        steps.iter().find(|s| s.id == "bucket").unwrap().action,
        Action::Update
    );
}

#[tokio::test]
async fn turning_the_cache_off_deletes_its_key_and_bucket() {
    let (backend, _, _) = provisioned(Log::silent()).await;
    let bucket = backend.s3.buckets()[0].clone();
    backend.s3.put_object(&bucket, "runner/cache.zip", b"zip");
    let off = provider(cache_off(), &backend, Log::silent());
    let steps = off.plan(&[]).await.unwrap();
    let find = |id: &str| steps.iter().find(|s| s.id == id).map(|s| s.action);
    assert_eq!(find("key:cache"), Some(Action::Delete));
    assert_eq!(find("bucket"), Some(Action::Delete));
    assert_eq!(find("definition:ci"), Some(Action::Update));
    let outputs = off.apply(&steps).await.unwrap();
    assert!(!outputs.contains_key("CIBURST_CACHE_SECRET_KEY"));
    assert!(backend.s3.buckets().is_empty());
    assert_eq!(backend.iam.applications().len(), 1);
    assert_eq!(backend.iam.policies().len(), 1);
    assert_eq!(backend.iam.api_keys().len(), 1);
    let env = &backend.jobs.definitions()[0]["environment_variables"];
    assert!(env.get("CACHE_TYPE").is_none(), "{env}");
    let steps = off.plan(&[]).await.unwrap();
    assert!(steps.iter().all(Step::is_noop), "{steps:?}");
    assert!(
        steps
            .iter()
            .all(|s| s.id != "key:cache" && s.id != "bucket")
    );
}

#[tokio::test]
async fn a_key_without_api_key_is_repaired() {
    let (backend, provider, _) = provisioned(Log::silent()).await;
    let dispatch_id = backend.iam.applications()[0]["id"].clone();
    let access = backend
        .iam
        .api_keys()
        .into_iter()
        .find(|k| k["application_id"] == dispatch_id)
        .unwrap();
    let client = backend.client();
    client
        .delete(&format!(
            "/iam/v1alpha1/api-keys/{}",
            access["access_key"].as_str().unwrap()
        ))
        .await
        .unwrap();
    let steps = provider.plan(&[]).await.unwrap();
    let step = steps.iter().find(|s| s.id == "key:dispatch").unwrap();
    assert_eq!(
        (step.action, step.detail.as_deref()),
        (Action::Update, Some("repair"))
    );
    let outputs = provider.apply(&steps).await.unwrap();
    assert!(outputs.contains_key("CIBURST_DISPATCH_SECRET_KEY"));
    assert_eq!(backend.iam.policies().len(), 2, "policy kept");
}

#[tokio::test]
async fn start_passes_env_and_labels_and_list_reads_them_back() {
    let (backend, provider, _) = provisioned(Log::silent()).await;
    let run_id = provider.start(start_request(0)).await.unwrap();
    let start = backend
        .jobs
        .calls()
        .into_iter()
        .find(|c| c.path.ends_with("/start"))
        .unwrap();
    assert_eq!(
        start.body,
        Some(json!({ "environment_variables": {
            "RUNNER_TOKEN": "glrt-test",
            "CIBURST_PROJECT": "7",
            "CIBURST_PIPELINE": "1",
            "CIBURST_JOB": "2",
            "CIBURST_STAGE": "test",
        }}))
    );
    let runs = provider.list(Some("ci")).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, run_id);
    assert_eq!(runs[0].pool, "ci");
    assert_eq!(runs[0].state, RunState::Queued);
    assert_eq!(runs[0].labels, labels());
    assert!(!runs[0].created_at.is_empty());

    backend.jobs.set_run_state(&run_id, "running");
    assert_eq!(
        provider.list(None).await.unwrap()[0].state,
        RunState::Running
    );
    backend.jobs.set_run_state(&run_id, "succeeded");
    assert_eq!(provider.list(None).await.unwrap()[0].state, RunState::Done);
    backend.jobs.set_run_state(&run_id, "internal_error");
    assert_eq!(
        provider.list(None).await.unwrap()[0].state,
        RunState::Failed
    );
}

#[tokio::test]
async fn list_reads_every_page_of_every_pool() {
    let (backend, provider, _) = provisioned(Log::silent()).await;
    backend.jobs.set_max_page_size(2);
    for _ in 0..5 {
        provider.start(start_request(0)).await.unwrap();
    }
    provider.start(start_request(1)).await.unwrap();
    assert_eq!(provider.list(Some("ci")).await.unwrap().len(), 5);
    assert_eq!(provider.list(Some("build")).await.unwrap().len(), 1);
    assert_eq!(provider.list(None).await.unwrap().len(), 6);
    assert!(provider.list(Some("unknown")).await.unwrap().is_empty());
}

#[tokio::test]
async fn stop_cancels_the_run() {
    let (backend, provider, _) = provisioned(Log::silent()).await;
    let run_id = provider.start(start_request(0)).await.unwrap();
    provider.stop(&run_id).await.unwrap();
    assert!(
        backend
            .jobs
            .calls()
            .iter()
            .any(|c| c.method == "POST" && c.path.ends_with(&format!("/job-runs/{run_id}/stop")))
    );
    let runs = provider.list(None).await.unwrap();
    assert_eq!(runs[0].state, RunState::Failed);
    assert!(provider.stop("missing").await.is_err());
}

#[tokio::test]
async fn destroy_empties_the_bucket_and_removes_everything() {
    let (log, lines) = Log::memory();
    let (backend, provider, _) = provisioned(log).await;
    let bucket = backend.s3.buckets()[0].clone();
    backend.s3.put_object(&bucket, "a b/c.zip", b"1");
    backend.s3.put_object(&bucket, "d&e.zip", b"2");
    provider.destroy().await.unwrap();
    assert!(backend.account.projects().is_empty());
    assert!(backend.iam.applications().is_empty());
    assert!(backend.iam.policies().is_empty());
    assert!(backend.iam.api_keys().is_empty());
    assert!(backend.jobs.definitions().is_empty());
    assert!(backend.s3.buckets().is_empty());
    let lines = lines.lock().unwrap();
    assert!(lines.iter().any(|l| l.starts_with("info deleted project")));
    assert!(lines.iter().any(|l| l.starts_with("info deleted bucket")));
}

#[tokio::test]
async fn destroy_tolerates_a_project_that_cannot_be_deleted() {
    let (log, lines) = Log::memory();
    let (backend, provider, _) = provisioned(log).await;
    backend.account.fail_delete(true);
    provider.destroy().await.unwrap();
    assert_eq!(backend.account.projects().len(), 1);
    assert!(
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("warn project not deleted") && l.contains("412"))
    );
}

#[tokio::test]
async fn a_given_project_is_used_and_never_deleted() {
    let backend = Arc::new(ScalewayFake::new());
    let credentials = Credentials {
        default_project_id: Some("fixed-project".into()),
        ..ScalewayFake::credentials()
    };
    let provider = with_credentials(contract_config(), &backend, credentials, Log::silent())
        .with_s3_access_key(FAKE_ACCESS_KEY);
    let steps = provider.plan(&[]).await.unwrap();
    assert_eq!(steps[0].action, Action::Noop);
    let outputs = provider.apply(&steps).await.unwrap();
    assert_eq!(outputs["CIBURST_PROJECT_ID"], "fixed-project");
    assert_eq!(backend.s3.buckets(), ["contract-cache-fixed-project"]);
    provider.destroy().await.unwrap();
    assert!(backend.account.calls().is_empty(), "no project call at all");
    assert!(backend.jobs.definitions().is_empty());
}

#[tokio::test]
async fn start_refuses_without_token_or_definition() {
    let (_, provider, _) = provisioned(Log::silent()).await;
    let mut request = start_request(0);
    request.env.clear();
    let error = provider.start(request).await.unwrap_err().to_string();
    assert_eq!(error, "scaleway: RUNNER_TOKEN missing");
    let mut request = start_request(0);
    request.pool.tag = "gone".into();
    let error = provider.start(request).await.unwrap_err().to_string();
    assert_eq!(error, "scaleway: pool gone is not provisioned");
}

#[tokio::test]
async fn api_errors_carry_method_path_status_and_message() {
    let backend = Arc::new(ScalewayFake::new());
    let wrong = Credentials {
        secret_key: "wrong".into(),
        ..ScalewayFake::credentials()
    };
    let provider = with_credentials(contract_config(), &backend, wrong, Log::silent());
    let error = provider.plan(&[]).await.unwrap_err().to_string();
    assert_eq!(
        error,
        "scaleway GET /account/v3/projects: 401 authentication is denied"
    );
}

#[tokio::test]
async fn s3_errors_carry_the_s3_code() {
    let (backend, _, outputs) = provisioned(Log::silent()).await;
    let project_id = &outputs["CIBURST_PROJECT_ID"];
    let wrong = ScalewayJobs::new(jobs(contract_config()), backend.client(), Log::silent())
        .with_s3_access_key("SCWWRONGACCESSKEY000");
    let error = wrong.plan(&[]).await.unwrap_err().to_string();
    assert_eq!(
        error,
        format!(
            "scaleway s3 HEAD /contract-cache-{project_id}: 403 InvalidAccessKeyId signature check failed"
        )
    );
    let missing = with_credentials(
        contract_config(),
        &backend,
        Credentials {
            access_key: None,
            ..ScalewayFake::credentials()
        },
        Log::silent(),
    );
    let error = missing.plan(&[]).await.unwrap_err().to_string();
    assert_eq!(
        error,
        "scaleway: the cache bucket needs the access key of SCW_SECRET_KEY"
    );
    let no_cache = ScalewayJobs::new(jobs(cache_off()), backend.client(), Log::silent());
    assert!(no_cache.plan(&[]).await.is_ok(), "no S3 without the cache");
}

#[tokio::test]
async fn the_fake_s3_checks_the_signature() {
    let backend = Arc::new(ScalewayFake::new());
    let s3 = |secret: &str| S3 {
        http: backend.clone(),
        region: FAKE_REGION.to_string(),
        access_key: FAKE_ACCESS_KEY.to_string(),
        secret_key: secret.to_string(),
    };
    let error = s3("wrong")
        .create_bucket("b")
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway s3 PUT /b: 403 SignatureDoesNotMatch signature check failed"
    );
    let good = s3(FAKE_SECRET_KEY);
    good.create_bucket("b").await.unwrap();
    good.put_lifecycle("b", 5).await.unwrap();
    assert_eq!(good.lifecycle_days("b").await.unwrap(), Some(5));
    assert!(good.bucket_exists("b").await.unwrap());
    assert!(!good.bucket_exists("c").await.unwrap());
}

#[tokio::test]
async fn logs_name_resources_and_never_a_secret() {
    let (log, lines) = Log::memory();
    let (_, provider, created) = provisioned(log).await;
    let steps = provider
        .plan(&[Rotation::Key, Rotation::Cache])
        .await
        .unwrap();
    let rotated = provider.apply(&steps).await.unwrap();
    let lines: Vec<String> = lines.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("info created job definition name=\"contract-ci\""))
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("info created iam application name=\"contract-dispatch\""))
    );
    let secrets: Vec<&String> = created
        .iter()
        .chain(rotated.iter())
        .filter(|(k, _)| k.ends_with("_KEY"))
        .map(|(_, v)| v)
        .collect();
    for line in &lines {
        for secret in &secrets {
            assert!(!line.contains(secret.as_str()), "{line}");
        }
    }
}

#[tokio::test]
async fn fakes_reject_unauthenticated_requests() {
    let fake = ScalewayFake::new();
    for url in [
        "https://api.scaleway.com/account/v3/projects",
        "https://api.scaleway.com/iam/v1alpha1/applications",
        "https://api.scaleway.com/serverless-jobs/v1alpha1/regions/fr-par/job-runs",
        "https://api.scaleway.com/instance/v1/zones/fr-par-1/servers",
    ] {
        let response = fake.send(Request::new("GET", url)).await.unwrap();
        assert_eq!(response.status, 401, "{url}");
    }
    let s3 = Request::new("HEAD", "https://s3.fr-par.scw.cloud/b");
    assert_eq!(fake.send(s3).await.unwrap().status, 403);
}

#[tokio::test]
async fn a_dispatch_policy_without_registry_read_is_repaired() {
    let (backend, provider, _) = provisioned(Log::silent()).await;
    backend
        .iam
        .set_policy_sets("contract-dispatch", &["ServerlessJobsFullAccess"]);
    let steps = provider.plan(&[]).await.unwrap();
    let step = steps.iter().find(|s| s.id == "key:dispatch").unwrap();
    assert_eq!(step.action, Action::Update);
    assert_eq!(step.detail.as_deref(), Some("repair"));
    let outputs = provider.apply(&steps).await.unwrap();
    assert!(outputs.contains_key("CIBURST_DISPATCH_SECRET_KEY"));
    let dispatch: Vec<_> = backend
        .iam
        .policies()
        .into_iter()
        .filter(|p| p["name"] == "contract-dispatch")
        .collect();
    assert_eq!(dispatch.len(), 1);
    assert_eq!(
        dispatch[0]["rules"][0]["permission_set_names"],
        json!([
            "ServerlessJobsFullAccess",
            "ContainerRegistryReadOnly",
            "InstancesFullAccess"
        ])
    );
    let again = provider.plan(&[]).await.unwrap();
    assert!(again.iter().all(|s| s.action == Action::Noop), "{again:?}");
}
