//! M5 rule R4: `apply` ends by posting the jobs that wait for a pool tag to the dispatcher.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ciburst_core::config::{Config, parse_config};
use ciburst_core::dispatch::Dispatcher;
use ciburst_core::fake::{FakeHost, FakeProvider};
use ciburst_core::gitlab::Gitlab;
use ciburst_core::http::{HttpClient, Request, Response};
use ciburst_core::log::Log;
use ciburst_core::plan::{MemoryOutputs, OutputsStore};
use ciburst_core::provision::{Provisioner, keys, secrets_from_outputs};
use ciburst_core::store::MemoryStore;
use ciburst_core::testing::gitlab_fake::{FAKE_TOKEN, FakeGitlab, FakeJob};
use ciburst_core::types::Provider;
use ciburst_core::{Error, Result};
use serde_json::{Value, json};

/// 2026-10-25T12:00:00Z.
const NOW: u64 = 1_792_929_600;

/// The RFC 3339 time `ago` seconds before [`NOW`].
fn at(ago: u64) -> String {
    let t = 12 * 3600 - ago;
    format!(
        "2026-10-25T{:02}:{:02}:{:02}.000Z",
        t / 3600,
        t / 60 % 60,
        t % 60
    )
}

fn config() -> Arc<Config> {
    Arc::new(
        parse_config(
            &json!({
                "name": "demo",
                "gitlab": { "url": "https://gitlab.example", "projectIds": [7, 8] },
                "host": { "type": "fake" },
                "provider": { "type": "fake" },
                "defaultPool": "ci",
                "pools": [
                    { "tag": "ci", "image": "img/node", "stages": ["test"] },
                    { "tag": "build", "image": "img/node", "stages": ["build"] }
                ],
                "cache": { "type": "s3" }
            })
            .to_string(),
        )
        .unwrap(),
    )
}

type Answer = fn(&Request) -> Result<Response>;

/// Records every request and answers with `answer`.
struct Recorder {
    requests: Mutex<Vec<Request>>,
    answer: Answer,
}

impl Recorder {
    fn new(answer: Answer) -> Arc<Recorder> {
        Arc::new(Recorder {
            requests: Mutex::new(Vec::new()),
            answer,
        })
    }

    fn bodies(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(r.body.as_deref().unwrap_or_default()).unwrap())
            .collect()
    }
}

#[async_trait]
impl HttpClient for Recorder {
    async fn send(&self, request: Request) -> Result<Response> {
        let answer = (self.answer)(&request);
        self.requests.lock().unwrap().push(request);
        answer
    }
}

fn started(_: &Request) -> Result<Response> {
    Ok(Response::json_body(
        200,
        &json!({ "result": "started", "pool": "ci" }),
    ))
}

/// Routes the posts into an in-process dispatcher with the secrets of the current outputs.
struct ToDispatcher {
    config: Arc<Config>,
    provider: Arc<FakeProvider>,
    outputs: Arc<MemoryOutputs>,
}

#[async_trait]
impl HttpClient for ToDispatcher {
    async fn send(&self, request: Request) -> Result<Response> {
        let secrets = secrets_from_outputs(&self.config, &self.outputs.read().await?);
        let dispatcher = Dispatcher::new(
            self.config.clone(),
            self.provider.clone(),
            Arc::new(secrets),
            Log::silent(),
        );
        let path = request
            .url
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|i| rest[i..].to_string()))
            .unwrap_or_else(|| "/".to_string());
        let answer = dispatcher
            .handle_request(
                &request.method,
                &path,
                &request.headers,
                request.body.as_deref().unwrap_or_default(),
                NOW,
            )
            .await;
        Ok(Response::json_body(answer.status, &answer.body))
    }
}

struct Setup {
    config: Arc<Config>,
    fake: Arc<FakeGitlab>,
    provider: Arc<FakeProvider>,
    outputs: Arc<MemoryOutputs>,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Setup {
    fn new() -> Setup {
        let config = config();
        let (_, lines) = Log::memory();
        Setup {
            provider: Arc::new(FakeProvider::new(
                config.clone(),
                Arc::new(MemoryStore::new()),
            )),
            config,
            fake: Arc::new(FakeGitlab::new()),
            outputs: Arc::new(MemoryOutputs::new()),
            lines,
        }
    }

    fn provisioner(&mut self, with_gitlab: bool, http: Option<Arc<dyn HttpClient>>) -> Provisioner {
        let (log, lines) = Log::memory();
        self.lines = lines;
        let gitlab = with_gitlab.then(|| {
            Arc::new(Gitlab::new(
                &self.config.gitlab.url,
                FAKE_TOKEN,
                self.fake.clone(),
            ))
        });
        let provisioner = Provisioner::new(
            self.config.clone(),
            self.provider.clone(),
            Arc::new(FakeHost::new(
                self.config.clone(),
                Arc::new(MemoryStore::new()),
            )),
            gitlab,
            self.outputs.clone(),
            log,
        )
        .with_clock(Arc::new(|| NOW));
        match http {
            Some(http) => provisioner.with_replay(http),
            None => provisioner,
        }
    }

    fn lines(&self, prefix: &str) -> Vec<String> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains(prefix))
            .cloned()
            .collect()
    }
}

fn pending(id: u64, name: &str, stage: &str, tags: &[&str], ago: u64) -> FakeJob {
    FakeJob::pending(id, name, stage, tags, &at(ago))
}

fn waiting_jobs(fake: &FakeGitlab) {
    fake.add_job(7, pending(16, "docker", "test", &["docker", "ci"], 90));
    fake.add_job(7, pending(11, "unit", "test", &["ci"], 120));
    fake.add_job(7, pending(12, "image", "build", &["build"], 61));
    fake.add_job(7, pending(13, "young", "test", &["ci"], 60));
    fake.add_job(
        7,
        pending(14, "hosted", "test", &["saas-linux-small-amd64"], 300),
    );
    let mut done = pending(15, "done", "test", &["ci"], 300);
    done.status = "success".into();
    fake.add_job(7, done);
    fake.add_job(8, pending(21, "lint", "test", &["ci"], 100));
}

#[tokio::test]
async fn posts_exactly_the_jobs_that_wait_for_a_pool_tag() {
    let mut s = Setup::new();
    waiting_jobs(&s.fake);
    let recorder = Recorder::new(started);
    s.provisioner(true, Some(recorder.clone()))
        .apply(&[])
        .await
        .unwrap();

    let outputs = s.outputs.read().await.unwrap();
    let job = |project: u64, id: u64, name: &str, stage: &str| {
        json!({
            "object_kind": "build", "build_status": "pending", "project_id": project,
            "build_id": id, "build_name": name, "build_stage": stage, "pipeline_id": 900 + id
        })
    };
    assert_eq!(
        recorder.bodies(),
        [
            job(7, 11, "unit", "test"),
            job(7, 12, "image", "build"),
            job(7, 16, "docker", "test"),
            job(8, 21, "lint", "test"),
        ]
    );
    for request in recorder.requests.lock().unwrap().iter() {
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, outputs[keys::DISPATCH_URL]);
        assert_eq!(request.header_value("X-Gitlab-Event"), Some("Job Hook"));
        assert_eq!(
            request.header_value("X-Gitlab-Token"),
            Some(outputs[keys::HOOK_TOKEN].as_str())
        );
    }
    assert_eq!(
        s.lines("replayed"),
        [
            r#"info replayed project="7" job="11" result="started""#,
            r#"info replayed project="7" job="12" result="started""#,
            r#"info replayed project="7" job="16" result="started""#,
            r#"info replayed project="8" job="21" result="started""#,
        ]
    );
}

#[tokio::test]
async fn nothing_is_replayed_without_gitlab() {
    let mut s = Setup::new();
    waiting_jobs(&s.fake);
    let recorder = Recorder::new(started);
    s.provisioner(false, Some(recorder.clone()))
        .apply(&[])
        .await
        .unwrap();
    assert!(recorder.requests.lock().unwrap().is_empty());
    assert!(s.fake.calls().is_empty());

    s.provisioner(true, None).apply(&[]).await.unwrap();
    assert!(
        !s.fake.calls().iter().any(|c| c.path.ends_with("/jobs")),
        "no replay client, no job list"
    );
}

#[tokio::test]
async fn a_failing_post_does_not_fail_apply() {
    let mut s = Setup::new();
    waiting_jobs(&s.fake);
    let recorder = Recorder::new(|request| {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        match body["build_id"].as_u64() {
            Some(11) => Err(Error::Http("connection refused".into())),
            Some(12) => Ok(Response::empty(500)),
            _ => started(request),
        }
    });
    let applied = s.provisioner(true, Some(recorder.clone())).apply(&[]).await;
    assert!(applied.is_ok(), "{applied:?}");
    assert_eq!(recorder.requests.lock().unwrap().len(), 4);
    assert_eq!(
        s.lines("replay"),
        [
            r#"warn replay_failed project="7" job="11" error="http: connection refused""#,
            r#"warn replay_failed project="7" job="12" status="500""#,
            r#"info replayed project="7" job="16" result="started""#,
            r#"info replayed project="8" job="21" result="started""#,
        ]
    );
}

#[tokio::test]
async fn replayed_jobs_reach_the_dispatcher() {
    let mut s = Setup::new();
    s.fake.add_job(7, pending(11, "unit", "test", &["ci"], 120));
    s.fake
        .add_job(7, pending(12, "ciburst-setup", ".pre", &["ci"], 120));
    let to_dispatcher = Arc::new(ToDispatcher {
        config: s.config.clone(),
        provider: s.provider.clone(),
        outputs: s.outputs.clone(),
    });
    s.provisioner(true, Some(to_dispatcher))
        .apply(&[])
        .await
        .unwrap();
    assert_eq!(
        s.lines("replayed"),
        [
            r#"info replayed project="7" job="11" result="started""#,
            r#"info replayed project="7" job="12" result="ignored""#,
        ]
    );
    let runs = s.provider.list(Some("ci")).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].labels["job"], "11");
    assert_eq!(runs[0].labels["project"], "7");
}
