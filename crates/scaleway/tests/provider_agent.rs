//! The VM agent in the cache bucket (D38): step `vm:agent` against the S3 fake, the public
//! object VMs download without credentials, `CIBURST_AGENT_SHA256` from every apply, and the
//! lifecycle rules that leave the agent alone.

use std::path::PathBuf;
use std::sync::Arc;

use ciburst_core::config::Config;
use ciburst_core::http::{HttpClient, Request};
use ciburst_core::log::Log;
use ciburst_core::testing::contract::contract_config;
use ciburst_core::types::{Action, Provider, Rotation, Step};
use ciburst_scaleway::provider::Scaleway;
use ciburst_scaleway::provider::s3::{S3, lifecycle_xml};
use ciburst_scaleway::testing::s3_api::{FAKE_CLOCK, FAKE_PREFERRED_PROJECT};
use ciburst_scaleway::testing::scaleway_fake::{
    FAKE_ACCESS_KEY, FAKE_REGION, FAKE_SECRET_KEY, ScalewayFake,
};
use ciburst_scaleway::testing::{
    RUNNER_FIXTURE_URL, agent_fixture, agent_fixture_sha256, runner_fixture_sha256,
};
use sha2::{Digest, Sha256};

fn provider(backend: &Arc<ScalewayFake>, agent: Option<PathBuf>) -> Scaleway {
    with_config(contract_config(), backend, agent, Log::silent())
}

fn with_config(
    config: Arc<Config>,
    backend: &Arc<ScalewayFake>,
    agent: Option<PathBuf>,
    log: Log,
) -> Scaleway {
    Scaleway::new(config, backend.client(), log)
        .with_s3_access_key(FAKE_ACCESS_KEY)
        .with_agent(agent)
        .with_runner_source(RUNNER_FIXTURE_URL, runner_fixture_sha256())
}

/// The keys below `ciburst/agent/` in `bucket`, with their content.
fn agents(backend: &ScalewayFake, bucket: &str) -> Vec<(String, Vec<u8>)> {
    backend
        .s3
        .objects(bucket)
        .into_iter()
        .filter(|(key, _)| key.starts_with("ciburst/agent/"))
        .collect()
}

fn step<'a>(steps: &'a [Step], id: &str) -> &'a Step {
    steps.iter().find(|s| s.id == id).expect(id)
}

/// A temporary agent binary with `content` behind the ELF magic, and its SHA-256.
fn binary(dir: &tempfile::TempDir, content: &str) -> (PathBuf, String) {
    let path = dir.path().join(format!("agent-{content}"));
    let bytes = [b"\x7fELF".as_slice(), content.as_bytes()].concat();
    std::fs::write(&path, &bytes).unwrap();
    (path, hex::encode(Sha256::digest(&bytes)))
}

fn bucket(project_id: &str) -> String {
    format!("contract-cache-{project_id}")
}

async fn anonymous_get(backend: &ScalewayFake, url: &str) -> (u16, Vec<u8>) {
    let response = backend.send(Request::new("GET", url)).await.unwrap();
    (response.status, response.body)
}

#[tokio::test]
async fn the_binary_is_uploaded_once_as_a_public_object() {
    let backend = Arc::new(ScalewayFake::new());
    let (log, lines) = Log::memory();
    let provider = with_config(contract_config(), &backend, Some(agent_fixture()), log);
    let sha = agent_fixture_sha256();
    let steps = provider.plan(&[]).await.unwrap();
    let agent = step(&steps, "vm:agent");
    assert_eq!(
        (agent.action, agent.resource.as_str()),
        (
            Action::Create,
            format!("vm agent ciburst/agent/{sha}").as_str()
        )
    );
    let outputs = provider.apply(&steps).await.unwrap();
    assert_eq!(outputs["CIBURST_AGENT_SHA256"], sha);
    let project_id = outputs["CIBURST_PROJECT_ID"].clone();
    let key = format!("ciburst/agent/{sha}");
    let fixture = std::fs::read(agent_fixture()).unwrap();
    assert_eq!(
        agents(&backend, &bucket(&project_id)),
        [(key.clone(), fixture.clone())]
    );
    assert_eq!(
        backend.s3.acl(&bucket(&project_id), &key).as_deref(),
        Some("public-read")
    );
    let put = backend
        .s3
        .calls()
        .into_iter()
        .find(|c| c.method == "PUT" && c.path.ends_with(&key))
        .unwrap();
    assert_eq!(put.path, format!("/{}/{key}", bucket(&project_id)));
    assert!(lines.lock().unwrap().iter().any(|l| {
        l.starts_with("info uploaded agent") && l.contains(&format!("key=\"{key}\""))
    }));

    let url = format!("https://{}.s3.fr-par.scw.cloud/{key}", bucket(&project_id));
    assert_eq!(anonymous_get(&backend, &url).await, (200, fixture));

    let writes = backend.s3.calls().len();
    let again = provider.plan(&[]).await.unwrap();
    assert!(again.iter().all(Step::is_noop), "{again:?}");
    let outputs = provider.apply(&again).await.unwrap();
    assert_eq!(outputs["CIBURST_AGENT_SHA256"], sha);
    assert!(
        backend.s3.calls()[writes..]
            .iter()
            .all(|c| c.method != "PUT"),
        "nothing uploaded on the same hash"
    );
}

#[tokio::test]
async fn the_bucket_lives_in_the_deployment_project_not_the_keys_preferred_one() {
    let backend = Arc::new(ScalewayFake::new());
    let first = provider(&backend, Some(agent_fixture()));
    let outputs = first.apply(&first.plan(&[]).await.unwrap()).await.unwrap();
    let project_id = outputs["CIBURST_PROJECT_ID"].clone();
    assert_ne!(project_id, FAKE_PREFERRED_PROJECT);
    assert_eq!(
        backend.s3.bucket_project(&bucket(&project_id)).as_deref(),
        Some(project_id.as_str())
    );
    let again = provider(&backend, Some(agent_fixture()));
    let steps = again.plan(&[]).await.unwrap();
    assert!(steps.iter().all(Step::is_noop), "{steps:?}");
}

#[tokio::test]
async fn only_public_objects_are_readable_without_credentials() {
    let backend = Arc::new(ScalewayFake::new());
    let provider = provider(&backend, Some(agent_fixture()));
    let outputs = provider
        .apply(&provider.plan(&[]).await.unwrap())
        .await
        .unwrap();
    let bucket = bucket(&outputs["CIBURST_PROJECT_ID"]);
    backend
        .s3
        .put_object(&bucket, "project/7/cache.zip", b"cache");
    let host = format!("https://{bucket}.s3.fr-par.scw.cloud");
    let agent = format!("ciburst/agent/{}", agent_fixture_sha256());
    for (path, status) in [
        (agent.as_str(), 200),
        ("project/7/cache.zip", 403),
        ("ciburst/agent/missing", 403),
        ("", 403),
    ] {
        let (got, _) = anonymous_get(&backend, &format!("{host}/{path}")).await;
        assert_eq!(got, status, "{path}");
    }
    let path_style = format!("https://s3.fr-par.scw.cloud/{bucket}/{agent}");
    assert_eq!(anonymous_get(&backend, &path_style).await.0, 200);
    let head = backend
        .send(Request::new("HEAD", format!("{host}/{agent}")))
        .await
        .unwrap();
    assert_eq!((head.status, head.body.len()), (200, 0));
    let list = format!("{host}/?list-type=2");
    assert_eq!(anonymous_get(&backend, &list).await.0, 403);
    let put = backend
        .send(Request::new("PUT", format!("{host}/{agent}")).header("x-amz-acl", "public-read"))
        .await
        .unwrap();
    assert_eq!(put.status, 403, "no anonymous write");
}

#[tokio::test]
async fn the_fake_wants_every_amz_header_signed_and_a_canned_acl() {
    let backend = Arc::new(ScalewayFake::new());
    let s3 = S3 {
        http: backend.clone(),
        region: FAKE_REGION.to_string(),
        access_key: FAKE_ACCESS_KEY.to_string(),
        secret_key: FAKE_SECRET_KEY.to_string(),
    };
    s3.create_bucket("b").await.unwrap();
    s3.put_public_object("b", "k", b"x".to_vec()).await.unwrap();
    assert!(s3.object_exists("b", "k").await.unwrap());
    assert!(!s3.object_exists("b", "missing").await.unwrap());
    assert_eq!(backend.s3.acl("b", "k").as_deref(), Some("public-read"));

    /// Forwards to the fake with one more header, after the client signed the request.
    struct Extra(Arc<ScalewayFake>, &'static str, &'static str);
    #[async_trait::async_trait]
    impl HttpClient for Extra {
        async fn send(
            &self,
            request: Request,
        ) -> ciburst_core::Result<ciburst_core::http::Response> {
            self.0.send(request.header(self.1, self.2)).await
        }
    }
    for (name, value, status) in [
        ("x-amz-acl", "public-read", 403),
        ("x-amz-meta-owner", "ops", 403),
        ("cache-control", "no-cache", 200),
    ] {
        let s3 = S3 {
            http: Arc::new(Extra(backend.clone(), name, value)),
            ..s3_like(&backend)
        };
        let result = s3.put_object("b", "k2", b"y".to_vec()).await;
        assert_eq!(result.is_ok(), status == 200, "{name}: {result:?}");
        if let Err(error) = result {
            assert!(
                error.to_string().contains("403 AccessDenied"),
                "{name}: {error}"
            );
        }
    }
}

fn s3_like(backend: &Arc<ScalewayFake>) -> S3 {
    S3 {
        http: backend.clone(),
        region: FAKE_REGION.to_string(),
        access_key: FAKE_ACCESS_KEY.to_string(),
        secret_key: FAKE_SECRET_KEY.to_string(),
    }
}

#[tokio::test]
async fn a_new_binary_is_a_new_object_and_without_one_the_newest_stays() {
    let backend = Arc::new(ScalewayFake::new());
    let dir = tempfile::tempdir().unwrap();
    let (first, first_sha) = binary(&dir, "first");
    let (second, second_sha) = binary(&dir, "second");
    let outputs = provider(&backend, Some(first.clone()))
        .apply(&provider(&backend, Some(first)).plan(&[]).await.unwrap())
        .await
        .unwrap();
    assert_eq!(outputs["CIBURST_AGENT_SHA256"], first_sha);
    let bucket = bucket(&outputs["CIBURST_PROJECT_ID"]);

    backend.s3.set_clock(FAKE_CLOCK + 3_600);
    let newer = provider(&backend, Some(second));
    let steps = newer.plan(&[]).await.unwrap();
    assert_eq!(step(&steps, "vm:agent").action, Action::Create);
    assert_eq!(
        step(&steps, "vm:agent").resource,
        format!("vm agent ciburst/agent/{second_sha}")
    );
    assert!(
        steps
            .iter()
            .filter(|s| s.id != "vm:agent")
            .all(Step::is_noop)
    );
    let outputs = newer.apply(&steps).await.unwrap();
    assert_eq!(outputs["CIBURST_AGENT_SHA256"], second_sha);
    assert_eq!(
        agents(&backend, &bucket).len(),
        2,
        "the older agent stays for VMs that boot now"
    );

    let (log, lines) = Log::memory();
    let without = with_config(contract_config(), &backend, None, log);
    let steps = without.plan(&[]).await.unwrap();
    let agent = step(&steps, "vm:agent");
    assert_eq!(agent.action, Action::Noop);
    assert_eq!(
        agent.resource,
        format!("vm agent ciburst/agent/{second_sha}")
    );
    assert_eq!(agent.detail.as_deref(), Some("newest uploaded agent"));
    assert!(lines.lock().unwrap().is_empty());
    without.preflight().await.unwrap();
    assert_eq!(
        without.apply(&[]).await.unwrap()["CIBURST_AGENT_SHA256"],
        second_sha,
        "apply(&[]) reports the agent too"
    );
    let rotated = without.plan(&[Rotation::Cache]).await.unwrap();
    let outputs = without.apply(&rotated).await.unwrap();
    assert_eq!(outputs["CIBURST_AGENT_SHA256"], second_sha);
    assert!(outputs.contains_key("CIBURST_CACHE_SECRET_KEY"));
}

#[tokio::test]
async fn every_apply_call_reports_the_agent_of_the_binary() {
    let backend = Arc::new(ScalewayFake::new());
    let provider = provider(&backend, Some(agent_fixture()));
    let sha = agent_fixture_sha256();
    for step in provider.plan(&[]).await.unwrap() {
        let outputs = provider.apply(std::slice::from_ref(&step)).await.unwrap();
        assert_eq!(outputs["CIBURST_AGENT_SHA256"], sha, "{}", step.id);
    }
    assert_eq!(
        provider.apply(&[]).await.unwrap()["CIBURST_AGENT_SHA256"],
        sha
    );
}

#[tokio::test]
async fn an_agent_that_is_no_linux_executable_is_refused() {
    let backend = Arc::new(ScalewayFake::new());
    let dir = tempfile::tempdir().unwrap();
    let windows = dir.path().join("ciburst-vm-agent.exe");
    std::fs::write(&windows, b"MZ\x90\x00").unwrap();
    let error = provider(&backend, Some(windows.clone()))
        .plan(&[])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        format!(
            "scaleway: agent {} is not a Linux executable; build it with scripts/agent-binary.sh",
            windows.display()
        )
    );
    let missing = dir.path().join("missing");
    let error = provider(&backend, Some(missing.clone()))
        .preflight()
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with(&format!("scaleway: agent {}: ", missing.display())),
        "{error}"
    );
    assert!(backend.writes().is_empty());
}

#[tokio::test]
async fn the_lifecycle_expires_the_cache_and_the_intents_and_upgrades_the_m4_rule() {
    let backend = Arc::new(ScalewayFake::new());
    let provider = provider(&backend, Some(agent_fixture()));
    let outputs = provider
        .apply(&provider.plan(&[]).await.unwrap())
        .await
        .unwrap();
    let bucket = bucket(&outputs["CIBURST_PROJECT_ID"]);
    let document = backend.s3.lifecycle(&bucket).unwrap();
    assert_eq!(document, lifecycle_xml(14));
    for prefix in ["project/", "runner/", "ciburst/intents/"] {
        assert!(
            document.contains(&format!("<Prefix>{prefix}</Prefix>")),
            "{prefix}"
        );
    }
    assert!(!document.contains("<Prefix></Prefix>"));
    assert!(!document.contains("ciburst/agent/"));

    let m4 = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><LifecycleConfiguration \
              xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Rule><ID>ciburst-expire</ID>\
              <Filter><Prefix></Prefix></Filter><Status>Enabled</Status><Expiration><Days>14\
              </Days></Expiration></Rule></LifecycleConfiguration>";
    backend.s3.set_lifecycle(&bucket, Some(m4.to_string()));
    let steps = provider.plan(&[]).await.unwrap();
    let step = step(&steps, "bucket");
    assert_eq!(step.action, Action::Update);
    assert_eq!(
        step.detail.as_deref(),
        Some("lifecycle 14 days on project/, runner/, ciburst/intents/")
    );
    provider.apply(&steps).await.unwrap();
    assert_eq!(backend.s3.lifecycle(&bucket), Some(lifecycle_xml(14)));
    assert!(provider.plan(&[]).await.unwrap().iter().all(Step::is_noop));
}

#[tokio::test]
async fn the_fake_refuses_lifecycle_rules_s3_refuses() {
    let backend = Arc::new(ScalewayFake::new());
    let s3 = s3_like(&backend);
    s3.create_bucket("b").await.unwrap();
    s3.put_lifecycle("b", 3).await.unwrap();
    let error = s3.put_lifecycle("b", 0).await.unwrap_err().to_string();
    assert!(error.contains("400 InvalidArgument"), "{error}");
    assert_eq!(s3.lifecycle_days("b").await.unwrap(), Some(3));
}

#[tokio::test]
async fn rotating_keys_that_do_not_exist_creates_them() {
    let backend = Arc::new(ScalewayFake::new());
    let provider = provider(&backend, Some(agent_fixture()));
    let steps = provider
        .plan(&[Rotation::Key, Rotation::Cache])
        .await
        .unwrap();
    for id in ["key:dispatch", "key:cache"] {
        let step = step(&steps, id);
        assert_eq!(
            (step.action, step.detail.as_deref()),
            (Action::Create, None)
        );
    }
    let outputs = provider.apply(&steps).await.unwrap();
    for key in ["CIBURST_DISPATCH_SECRET_KEY", "CIBURST_CACHE_SECRET_KEY"] {
        assert!(outputs.contains_key(key), "{key}");
    }
}
