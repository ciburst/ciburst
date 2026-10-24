use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::config::{CacheType, Config, Pool};
use crate::log::Log;
use crate::types::{Provider, Secrets, StartRequest};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DispatchResponse {
    pub status: u16,
    pub body: Value,
}

/// Environment a run receives on top of its pool definition. Secret values only.
pub fn run_env(
    config: &Config,
    secrets: &Secrets,
    pool: &Pool,
) -> Result<BTreeMap<String, String>> {
    let token = secrets
        .runner_tokens
        .get(&pool.tag)
        .ok_or_else(|| Error::other(format!("no runner token for pool {}", pool.tag)))?;
    let mut env = BTreeMap::from([("RUNNER_TOKEN".to_string(), token.clone())]);
    if config.cache.kind == CacheType::S3 {
        let cache = secrets
            .cache
            .as_ref()
            .ok_or_else(|| Error::other("cache is s3 but no cache key is configured"))?;
        env.insert("CACHE_S3_ACCESS_KEY".to_string(), cache.access_key.clone());
        env.insert("CACHE_S3_SECRET_KEY".to_string(), cache.secret_key.clone());
    }
    Ok(env)
}

/// Turns a GitLab job event into at most one run. Pure apart from the provider calls.
pub struct Dispatcher {
    config: Arc<Config>,
    provider: Arc<dyn Provider>,
    secrets: Arc<Secrets>,
    log: Log,
}

impl Dispatcher {
    pub fn new(
        config: Arc<Config>,
        provider: Arc<dyn Provider>,
        secrets: Arc<Secrets>,
        log: Log,
    ) -> Dispatcher {
        Dispatcher {
            config,
            provider,
            secrets,
            log,
        }
    }

    pub async fn handle(&self, headers: &[(String, String)], body: &[u8]) -> DispatchResponse {
        if !token_matches(
            header(headers, "x-gitlab-token"),
            self.secrets.hook_token.as_deref(),
        ) {
            return DispatchResponse {
                status: 401,
                body: json!({}),
            };
        }
        let Ok(event) = serde_json::from_slice::<Value>(body) else {
            return reply(400, "bad_request", json!({}));
        };
        if event.get("object_kind").and_then(Value::as_str) != Some("build") {
            return ignored("not_job_event", json!({}));
        }
        let status = event
            .get("build_status")
            .and_then(Value::as_str)
            .unwrap_or("");
        if status != "pending" {
            return ignored("status", json!({ "status": status }));
        }
        let project_id = event.get("project_id").and_then(Value::as_u64).unwrap_or(0);
        if !self.config.gitlab.project_ids.contains(&project_id) {
            return ignored("project", json!({}));
        }

        let stage = event.get("build_stage").and_then(Value::as_str);
        let pool = self.config.pool_for_stage(stage).clone();
        let labels = BTreeMap::from([
            ("project".to_string(), project_id.to_string()),
            ("pipeline".to_string(), number(&event, "pipeline_id")),
            ("job".to_string(), number(&event, "build_id")),
            ("stage".to_string(), stage.unwrap_or("").to_string()),
        ]);
        let fields: Vec<(&str, &str)> = labels
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        match self.start(&pool, labels.clone()).await {
            Ok(Started::AtCapacity(live)) => {
                let live = live.to_string();
                let mut f = fields.clone();
                f.push(("pool", &pool.tag));
                f.push(("live", &live));
                self.log.info("at_capacity", &f);
                reply(200, "at_capacity", json!({ "pool": pool.tag }))
            }
            Ok(Started::Run(run_id)) => {
                let mut f = fields.clone();
                f.push(("pool", &pool.tag));
                f.push(("run", &run_id));
                self.log.info("started", &f);
                reply(200, "started", json!({ "pool": pool.tag, "runId": run_id }))
            }
            Err(error) => {
                let message = error.to_string();
                let mut f = fields.clone();
                f.push(("pool", &pool.tag));
                f.push(("error", &message));
                self.log.error("start_failed", &f);
                reply(500, "error", json!({}))
            }
        }
    }

    async fn start(&self, pool: &Pool, labels: BTreeMap<String, String>) -> Result<Started> {
        let live = self
            .provider
            .list(Some(&pool.tag))
            .await?
            .into_iter()
            .filter(|run| run.state.is_live())
            .count();
        if live >= pool.max_runs as usize {
            return Ok(Started::AtCapacity(live));
        }
        let env = run_env(&self.config, &self.secrets, pool)?;
        let run_id = self
            .provider
            .start(StartRequest {
                pool: pool.clone(),
                env,
                labels,
            })
            .await?;
        Ok(Started::Run(run_id))
    }
}

enum Started {
    AtCapacity(usize),
    Run(String),
}

fn reply(status: u16, result: &str, mut extra: Value) -> DispatchResponse {
    if let Value::Object(map) = &mut extra {
        map.insert("result".to_string(), Value::String(result.to_string()));
    }
    DispatchResponse {
        status,
        body: extra,
    }
}

fn ignored(reason: &str, mut extra: Value) -> DispatchResponse {
    if let Value::Object(map) = &mut extra {
        map.insert("reason".to_string(), Value::String(reason.to_string()));
    }
    reply(200, "ignored", extra)
}

fn number(event: &Value, key: &str) -> String {
    event
        .get(key)
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn token_matches(given: Option<&str>, expected: Option<&str>) -> bool {
    match (given, expected) {
        (Some(given), Some(expected)) if !expected.is_empty() => {
            given.len() == expected.len() && bool::from(given.as_bytes().ct_eq(expected.as_bytes()))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;
    use crate::fake::FakeProvider;
    use crate::store::MemoryStore;
    use crate::types::{KeyPair, Run};
    use async_trait::async_trait;

    fn config() -> Arc<Config> {
        Arc::new(
            parse_config(
                r#"{
                  "name": "demo",
                  "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
                  "host": { "type": "fake" }, "provider": { "type": "fake" },
                  "defaultPool": "ci",
                  "pools": [
                    { "tag": "ci", "image": "img/node", "stages": ["test"] },
                    { "tag": "build", "image": "img/kaniko", "stages": ["build"], "maxRuns": 1 }
                  ],
                  "cache": { "type": "s3" }
                }"#,
            )
            .unwrap(),
        )
    }

    fn secrets() -> Arc<Secrets> {
        Arc::new(Secrets {
            hook_token: Some("hook-secret".into()),
            runner_tokens: BTreeMap::from([
                ("ci".to_string(), "glrt-ci".to_string()),
                ("build".to_string(), "glrt-build".to_string()),
            ]),
            dispatch: None,
            cache: Some(KeyPair {
                access_key: "AK".into(),
                secret_key: "SK".into(),
            }),
        })
    }

    fn event(stage: &str, status: &str, project: u64, job: u64) -> Vec<u8> {
        json!({
            "object_kind": "build", "build_id": job, "build_stage": stage,
            "build_status": status, "pipeline_id": 9, "project_id": project
        })
        .to_string()
        .into_bytes()
    }

    fn headers(token: &str) -> Vec<(String, String)> {
        vec![
            ("X-Gitlab-Token".to_string(), token.to_string()),
            ("X-Gitlab-Event".to_string(), "Job Hook".to_string()),
        ]
    }

    struct Recording {
        inner: Arc<FakeProvider>,
        started: Mutex<Vec<StartRequest>>,
        fail: bool,
    }

    use std::sync::Mutex;

    #[async_trait]
    impl Provider for Recording {
        fn capabilities(&self) -> crate::types::Capabilities {
            self.inner.capabilities()
        }
        async fn plan(&self, rotate: &[crate::types::Rotation]) -> Result<Vec<crate::types::Step>> {
            self.inner.plan(rotate).await
        }
        async fn apply(&self, steps: &[crate::types::Step]) -> Result<crate::types::Outputs> {
            self.inner.apply(steps).await
        }
        async fn destroy(&self) -> Result<()> {
            self.inner.destroy().await
        }
        async fn start(&self, request: StartRequest) -> Result<String> {
            if self.fail {
                return Err(Error::provider("quota"));
            }
            self.started.lock().unwrap().push(request.clone());
            self.inner.start(request).await
        }
        async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
            self.inner.list(pool).await
        }
        async fn stop(&self, run_id: &str) -> Result<()> {
            self.inner.stop(run_id).await
        }
    }

    async fn setup(fail: bool) -> (Dispatcher, Arc<Recording>, Arc<Mutex<Vec<String>>>) {
        let config = config();
        let inner = Arc::new(FakeProvider::new(
            config.clone(),
            Arc::new(MemoryStore::new()),
        ));
        let plan = inner.plan(&[]).await.unwrap();
        inner.apply(&plan).await.unwrap();
        let recording = Arc::new(Recording {
            inner,
            started: Mutex::new(Vec::new()),
            fail,
        });
        let (log, lines) = Log::memory();
        let dispatcher = Dispatcher::new(config, recording.clone(), secrets(), log);
        (dispatcher, recording, lines)
    }

    #[tokio::test]
    async fn rejects_bad_tokens() {
        let (d, _, _) = setup(false).await;
        assert_eq!(d.handle(&[], b"{}").await.status, 401);
        assert_eq!(
            d.handle(&headers("nope"), &event("test", "pending", 7, 1))
                .await
                .status,
            401
        );
        assert_eq!(
            d.handle(&headers("hook-secret-longer"), b"{}").await.status,
            401
        );
        let empty = Dispatcher::new(
            config(),
            Arc::new(FakeProvider::new(config(), Arc::new(MemoryStore::new()))),
            Arc::new(Secrets::default()),
            Log::silent(),
        );
        assert_eq!(empty.handle(&headers(""), b"{}").await.status, 401);
    }

    #[tokio::test]
    async fn answers_400_for_bad_json() {
        let (d, _, _) = setup(false).await;
        let response = d.handle(&headers("hook-secret"), b"{not json").await;
        assert_eq!(response.status, 400);
        assert_eq!(response.body["result"], "bad_request");
    }

    #[tokio::test]
    async fn ignores_other_events() {
        let (d, recording, _) = setup(false).await;
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (b"{\"object_kind\":\"pipeline\"}".to_vec(), "not_job_event"),
            (b"null".to_vec(), "not_job_event"),
            (event("test", "running", 7, 1), "status"),
            (event("test", "pending", 8, 1), "project"),
        ];
        for (body, reason) in cases {
            let response = d.handle(&headers("hook-secret"), &body).await;
            assert_eq!(response.status, 200);
            assert_eq!(response.body["result"], "ignored");
            assert_eq!(response.body["reason"], reason);
        }
        assert!(recording.started.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn starts_in_the_pool_of_the_stage() {
        let (d, recording, lines) = setup(false).await;
        let response = d
            .handle(&headers("hook-secret"), &event("build", "pending", 7, 42))
            .await;
        assert_eq!(response.status, 200, "{}", response.body);
        assert_eq!(response.body["result"], "started");
        assert_eq!(response.body["pool"], "build");
        assert!(
            response.body["runId"]
                .as_str()
                .unwrap()
                .starts_with("fake-")
        );
        let started = recording.started.lock().unwrap();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].pool.tag, "build");
        assert_eq!(started[0].env["RUNNER_TOKEN"], "glrt-build");
        assert_eq!(started[0].env["CACHE_S3_ACCESS_KEY"], "AK");
        assert_eq!(started[0].env["CACHE_S3_SECRET_KEY"], "SK");
        assert_eq!(started[0].labels["job"], "42");
        assert_eq!(started[0].labels["pipeline"], "9");
        assert_eq!(started[0].labels["stage"], "build");
        let lines = lines.lock().unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("info started") && l.contains("pool=\"build\""))
        );
        assert!(!lines.iter().any(|l| l.contains("glrt-")));
    }

    #[tokio::test]
    async fn default_pool_and_case_insensitive_headers() {
        let (d, _, _) = setup(false).await;
        let headers = vec![("x-gitlab-token".to_string(), "hook-secret".to_string())];
        let response = d.handle(&headers, &event("deploy", "pending", 7, 1)).await;
        assert_eq!(response.body["result"], "started");
        assert_eq!(response.body["pool"], "ci");
    }

    #[tokio::test]
    async fn at_capacity_until_a_run_ends() {
        let (d, recording, _) = setup(false).await;
        let first = d
            .handle(&headers("hook-secret"), &event("build", "pending", 7, 1))
            .await;
        assert_eq!(first.body["result"], "started");
        let second = d
            .handle(&headers("hook-secret"), &event("build", "pending", 7, 2))
            .await;
        assert_eq!(second.body["result"], "at_capacity");
        assert_eq!(second.body["pool"], "build");
        recording
            .inner
            .stop(first.body["runId"].as_str().unwrap())
            .await
            .unwrap();
        let third = d
            .handle(&headers("hook-secret"), &event("build", "pending", 7, 3))
            .await;
        assert_eq!(third.body["result"], "started");
    }

    #[tokio::test]
    async fn answers_500_when_the_provider_fails() {
        let (d, _, lines) = setup(true).await;
        let response = d
            .handle(&headers("hook-secret"), &event("test", "pending", 7, 1))
            .await;
        assert_eq!(response.status, 500);
        assert_eq!(response.body["result"], "error");
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.starts_with("error start_failed") && l.contains("quota"))
        );
    }

    #[test]
    fn run_env_requires_tokens() {
        let config = config();
        let pool = config.pools[0].clone();
        assert!(run_env(&config, &Secrets::default(), &pool).is_err());
        let mut partial = Secrets::default();
        partial.runner_tokens.insert("ci".into(), "t".into());
        assert!(
            run_env(&config, &partial, &pool)
                .unwrap_err()
                .to_string()
                .contains("no cache key")
        );
        let mut no_cache = (*config).clone();
        no_cache.cache.kind = CacheType::None;
        assert_eq!(
            run_env(&no_cache, &partial, &pool).unwrap()["RUNNER_TOKEN"],
            "t"
        );
    }
}
