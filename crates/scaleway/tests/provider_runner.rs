//! The runner in the cache bucket and the lanes that fetch it (D45, spec 7.1 and 9.1): step
//! `runner-binary` against the S3 fake and the download host, the public object lane runs and
//! VMs read without credentials, and lane job definitions that run `lane-bootstrap.sh` with the
//! runner's URL and SHA-256.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ciburst_core::cloud_init::RUNNER_SHA256;
use ciburst_core::config::{CacheType, Config};
use ciburst_core::http::{HttpClient, Request, Response};
use ciburst_core::lane;
use ciburst_core::log::Log;
use ciburst_core::testing::contract::contract_config;
use ciburst_core::types::{Action, Provider, Step};
use ciburst_scaleway::provider::Scaleway;
use ciburst_scaleway::provider::s3::{LARGE_BODY, S3, TRANSFER_TIMEOUT};
use ciburst_scaleway::testing::s3_api::base64_decode;
use ciburst_scaleway::testing::scaleway_fake::{
    FAKE_ACCESS_KEY, FAKE_REGION, FAKE_SECRET_KEY, ScalewayFake,
};
use ciburst_scaleway::testing::{
    RUNNER_FIXTURE, RUNNER_FIXTURE_URL, agent_fixture, runner_fixture_sha256,
};
use serde_json::json;
use sha2::{Digest, Sha256};

/// The contract config (S3 cache) with both pools as lanes.
/// The contract config: the VM runner and the host's lanes `ci` and `build`.
fn lanes() -> Arc<Config> {
    contract_config()
}

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

fn step<'a>(steps: &'a [Step], id: &str) -> &'a Step {
    steps.iter().find(|s| s.id == id).expect(id)
}

fn ids(steps: &[Step]) -> Vec<&str> {
    steps.iter().map(|s| s.id.as_str()).collect()
}

fn bucket(project_id: &str) -> String {
    format!("contract-cache-{project_id}")
}

fn runner_key(sha256: &str) -> String {
    format!("ciburst/runner/{sha256}")
}

/// The objects below `ciburst/runner/`, with their content.
fn runners(backend: &ScalewayFake, bucket: &str) -> Vec<(String, Vec<u8>)> {
    backend
        .s3
        .objects(bucket)
        .into_iter()
        .filter(|(key, _)| key.starts_with("ciburst/runner/"))
        .collect()
}

async fn anonymous_get(backend: &ScalewayFake, url: &str) -> (u16, Vec<u8>) {
    let response = backend.send(Request::new("GET", url)).await.unwrap();
    (response.status, response.body)
}

#[tokio::test]
async fn the_runner_is_copied_once_as_a_public_object() {
    let backend = Arc::new(ScalewayFake::new());
    let (log, lines) = Log::memory();
    let provider = provider(contract_config(), &backend, log);
    let sha = runner_fixture_sha256();
    let key = runner_key(&sha);
    let steps = provider.plan(&[]).await.unwrap();
    let runner = step(&steps, "runner-binary");
    assert_eq!(
        (runner.action, runner.resource.as_str()),
        (Action::Create, format!("gitlab-runner {key}").as_str())
    );
    assert_eq!(
        runner.detail.as_deref(),
        Some(format!("copy of {RUNNER_FIXTURE_URL}, public-read").as_str())
    );
    assert!(
        backend.downloads.calls().is_empty(),
        "plan downloads nothing"
    );

    let outputs = provider.apply(&steps).await.unwrap();
    let bucket = bucket(&outputs["CIBURST_PROJECT_ID"]);
    assert_eq!(
        runners(&backend, &bucket),
        [(key.clone(), RUNNER_FIXTURE.to_vec())]
    );
    assert_eq!(
        backend.s3.acl(&bucket, &key).as_deref(),
        Some("public-read")
    );
    let downloads = backend.downloads.calls();
    assert_eq!(downloads.len(), 1);
    assert_eq!(downloads[0].method, "GET");
    assert_eq!(
        backend.downloads.timeouts(),
        [Some(TRANSFER_TIMEOUT)],
        "a 120 MB download outlasts the client's own timeout"
    );
    assert!(lines.lock().unwrap().iter().any(|l| {
        l.starts_with("info uploaded runner")
            && l.contains(&format!("key=\"{key}\""))
            && l.contains(&format!("source=\"{RUNNER_FIXTURE_URL}\""))
    }));

    let url = format!("https://{bucket}.s3.fr-par.scw.cloud/{key}");
    assert_eq!(
        anonymous_get(&backend, &url).await,
        (200, RUNNER_FIXTURE.to_vec()),
        "lane runs and VMs download without credentials"
    );
    let path_style = format!("https://s3.fr-par.scw.cloud/{bucket}/{key}");
    assert_eq!(anonymous_get(&backend, &path_style).await.0, 200);

    let s3_calls = backend.s3.calls().len();
    let again = provider.plan(&[]).await.unwrap();
    assert!(again.iter().all(Step::is_noop), "{again:?}");
    assert_eq!(step(&again, "runner-binary").detail, None);
    provider.apply(&again).await.unwrap();
    provider
        .apply(&[Step::new("runner-binary", Action::Create, "again")])
        .await
        .unwrap();
    assert_eq!(backend.downloads.calls().len(), 1, "no second download");
    assert!(
        backend.s3.calls()[s3_calls..]
            .iter()
            .all(|c| c.method != "PUT"),
        "nothing uploaded once the object exists"
    );
}

#[tokio::test]
async fn a_download_with_another_hash_is_refused_naming_both() {
    let backend = Arc::new(ScalewayFake::new());
    let tampered = b"\x7fELF not the pinned runner";
    backend.downloads.serve(RUNNER_FIXTURE_URL, tampered);
    let provider = provider(contract_config(), &backend, Log::silent());
    let steps = provider.plan(&[]).await.unwrap();
    let error = provider.apply(&steps).await.unwrap_err().to_string();
    let actual = hex::encode(Sha256::digest(tampered));
    assert_eq!(
        error,
        format!(
            "scaleway: runner {RUNNER_FIXTURE_URL} has SHA-256 {actual}, expected {}",
            runner_fixture_sha256()
        )
    );
    for bucket in backend.s3.buckets() {
        assert!(runners(&backend, &bucket).is_empty(), "nothing uploaded");
    }

    let missing = "https://gitlab-runner-downloads.example.invalid/v0.0.0/binaries/missing";
    let elsewhere = Scaleway::new(contract_config(), backend.client(), Log::silent())
        .with_s3_access_key(FAKE_ACCESS_KEY)
        .with_agent(Some(agent_fixture()))
        .with_runner_source(missing, runner_fixture_sha256());
    let error = elsewhere
        .apply(&elsewhere.plan(&[]).await.unwrap())
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, format!("scaleway: runner download {missing}: 404"));
}

#[tokio::test]
async fn the_pinned_runner_is_the_default_source() {
    let backend = Arc::new(ScalewayFake::new());
    let pinned = Scaleway::new(contract_config(), backend.client(), Log::silent());
    let steps = pinned.plan(&[]).await.unwrap();
    let runner = step(&steps, "runner-binary");
    assert_eq!(
        runner.resource,
        format!("gitlab-runner ciburst/runner/{RUNNER_SHA256}")
    );
    assert_eq!(
        runner.detail.as_deref(),
        Some(
            "copy of https://gitlab-runner-downloads.s3.amazonaws.com/v18.11.4/binaries/gitlab-runner-linux-amd64, public-read"
        )
    );
}

#[tokio::test]
async fn the_step_follows_the_bucket() {
    let backend = Arc::new(ScalewayFake::new());
    let plan = |config: Arc<Config>| {
        let provider = provider(config, &backend, Log::silent());
        async move { provider.plan(&[]).await.unwrap() }
    };
    let vms = plan(contract_config()).await;
    assert_eq!(
        ids(&vms)[..6],
        [
            "project",
            "key:dispatch",
            "key:cache",
            "bucket",
            "runner-binary",
            "vm:agent"
        ]
    );
    let ids_of = ids(&vms);
    assert!(ids_of.contains(&"definition:7-ci") && ids_of.contains(&"definition:7-build"));
    let mut no_cache = (*lanes()).clone();
    no_cache.cache.kind = CacheType::None;
    let steps = plan(Arc::new(no_cache)).await;
    assert!(!ids(&steps).contains(&"runner-binary"), "{steps:?}");
}

#[tokio::test]
async fn lane_definitions_run_the_bootstrap_with_the_runner_of_the_bucket() {
    let (backend, _, project_id) = provisioned(lanes()).await;
    let bucket = bucket(&project_id);
    let sha = runner_fixture_sha256();
    let definitions = backend.jobs.definitions();
    assert_eq!(definitions.len(), 2);
    for definition in &definitions {
        assert_eq!(definition["image_uri"], "example.invalid/node:24");
        assert_eq!(definition["command"], lane::COMMAND);
        let env = &definition["environment_variables"];
        let script = base64_decode(env["CIBURST_LANE"].as_str().unwrap()).unwrap();
        assert_eq!(String::from_utf8(script).unwrap(), lane::BOOTSTRAP);
        let url = env["CIBURST_RUNNER_URL"].as_str().unwrap();
        assert_eq!(
            url,
            format!("https://{bucket}.s3.fr-par.scw.cloud/ciburst/runner/{sha}")
        );
        assert_eq!(env["CIBURST_RUNNER_SHA256"], sha.as_str());
        let (status, body) = anonymous_get(&backend, url).await;
        assert_eq!(status, 200, "the lane downloads without credentials");
        assert_eq!(
            hex::encode(Sha256::digest(&body)),
            sha,
            "the bytes the bootstrap checks"
        );
        for key in ["CIBURST_POOL", "CI_SERVER_URL", "RUNNER_WAIT_TIMEOUT"] {
            assert!(env[key].is_string(), "{key} stays");
        }
        assert_eq!(env["CACHE_S3_BUCKET_NAME"], bucket.as_str());
    }
}

#[tokio::test]
async fn definitions_of_existing_deployments_plan_an_update() {
    let (backend, provider, _) = provisioned(lanes()).await;
    let client = backend.client();
    for definition in backend.jobs.definitions() {
        let mut env = definition["environment_variables"].clone();
        for key in [
            "CIBURST_LANE",
            "CIBURST_RUNNER_URL",
            "CIBURST_RUNNER_SHA256",
        ] {
            env.as_object_mut().unwrap().remove(key);
        }
        let id = definition["id"].as_str().unwrap();
        client
            .patch(
                &format!("/serverless-jobs/v1alpha1/regions/{FAKE_REGION}/job-definitions/{id}"),
                &json!({ "command": "", "environment_variables": env }),
            )
            .await
            .unwrap();
    }
    let steps = provider.plan(&[]).await.unwrap();
    for id in ["definition:7-ci", "definition:7-build"] {
        let step = step(&steps, id);
        assert_eq!(
            (step.action, step.detail.as_deref()),
            (Action::Update, Some("command, environment_variables")),
            "{id}: a definition of M5"
        );
    }
    assert!(
        steps
            .iter()
            .filter(|s| !s.id.starts_with("definition:"))
            .all(Step::is_noop)
    );
    provider.apply(&steps).await.unwrap();
    assert!(provider.plan(&[]).await.unwrap().iter().all(Step::is_noop));
    for definition in backend.jobs.definitions() {
        assert_eq!(definition["command"], lane::COMMAND);
    }
}

#[tokio::test]
async fn a_new_runner_pin_is_a_new_object_and_updates_the_lanes() {
    let (backend, _, project_id) = provisioned(lanes()).await;
    let newer = b"\x7fELF the next gitlab-runner";
    let newer_sha = hex::encode(Sha256::digest(newer));
    let newer_url =
        "https://gitlab-runner-downloads.example.invalid/v0.0.1/binaries/gitlab-runner-linux-amd64";
    backend.downloads.serve(newer_url, newer);
    let bumped = Scaleway::new(lanes(), backend.client(), Log::silent())
        .with_s3_access_key(FAKE_ACCESS_KEY)
        .with_runner_source(newer_url, newer_sha.clone());
    let steps = bumped.plan(&[]).await.unwrap();
    assert_eq!(step(&steps, "runner-binary").action, Action::Create);
    for id in ["definition:7-ci", "definition:7-build"] {
        assert_eq!(
            step(&steps, id).detail.as_deref(),
            Some("environment_variables")
        );
    }
    bumped.apply(&steps).await.unwrap();
    let bucket = bucket(&project_id);
    let keys: Vec<String> = runners(&backend, &bucket)
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(
        keys.len(),
        2,
        "runs that started on the older runner still find it"
    );
    assert!(keys.contains(&runner_key(&newer_sha)));
    let env = &backend.jobs.definitions()[0]["environment_variables"];
    assert_eq!(env["CIBURST_RUNNER_SHA256"], newer_sha.as_str());
    assert!(bumped.plan(&[]).await.unwrap().iter().all(Step::is_noop));
}

/// Records the timeout of every request before the fake answers it.
struct Timeouts {
    backend: Arc<ScalewayFake>,
    seen: Mutex<Vec<(String, Option<Duration>)>>,
}

#[async_trait]
impl HttpClient for Timeouts {
    async fn send(&self, request: Request) -> ciburst_core::Result<Response> {
        self.seen
            .lock()
            .unwrap()
            .push((request.method.clone(), request.timeout));
        self.backend.send(request).await
    }
}

#[tokio::test]
async fn only_large_transfers_outlast_the_clients_timeout() {
    let recorder = Arc::new(Timeouts {
        backend: Arc::new(ScalewayFake::new()),
        seen: Mutex::new(Vec::new()),
    });
    let s3 = S3 {
        http: recorder.clone(),
        region: FAKE_REGION.to_string(),
        access_key: FAKE_ACCESS_KEY.to_string(),
        secret_key: FAKE_SECRET_KEY.to_string(),
    };
    s3.create_bucket("b").await.unwrap();
    s3.put_public_object("b", "small", vec![1; LARGE_BODY])
        .await
        .unwrap();
    s3.put_public_object("b", "large", vec![2; LARGE_BODY + 1])
        .await
        .unwrap();
    assert!(s3.object_exists("b", "large").await.unwrap());
    let seen = recorder.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        [
            ("PUT".to_string(), None),
            ("PUT".to_string(), None),
            ("PUT".to_string(), Some(TRANSFER_TIMEOUT)),
            ("HEAD".to_string(), None),
        ]
    );
    assert_eq!(
        recorder.backend.s3.objects("b")[0].1.len(),
        LARGE_BODY + 1,
        "the fake took the whole body"
    );
}
