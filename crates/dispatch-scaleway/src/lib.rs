//! Scaleway Serverless Function entry: `handle` receives the GitLab webhook and hands it to
//! `ciburst_core::dispatch::Dispatcher` with a `ScalewayJobs` provider built from the function's
//! environment. The variable names are `ciburst_scaleway::host::vars`, the ones the host sets.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, OnceLock};

use axum::body::Body;
use ciburst_core::config::{Config, parse_config};
use ciburst_core::dispatch::Dispatcher;
use ciburst_core::http::ReqwestClient;
use ciburst_core::log::Log;
use ciburst_core::types::{KeyPair, Provider, Secrets};
use ciburst_scaleway::client::{Credentials, ScalewayClient};
use ciburst_scaleway::host::vars;
use ciburst_scaleway::provider::ScalewayJobs;
use http::{Request, Response, StatusCode};

/// GitLab job events are a few kilobytes.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// What the function reads from its environment.
pub struct Env {
    pub config: Arc<Config>,
    pub secrets: Arc<Secrets>,
    pub credentials: Credentials,
    pub region: String,
}

/// A variable that is missing or unusable. Carries the name only, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvError {
    Missing(String),
    Invalid(String),
}

impl EnvError {
    pub fn variable(&self) -> &str {
        match self {
            EnvError::Missing(name) | EnvError::Invalid(name) => name,
        }
    }
}

impl fmt::Display for EnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvError::Missing(name) => write!(f, "{name} missing"),
            EnvError::Invalid(name) => write!(f, "{name} invalid"),
        }
    }
}

impl Env {
    /// Reads the variables through `lookup`; empty values count as missing.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Env, EnvError> {
        let optional = |name: &str| lookup(name).filter(|v| !v.is_empty());
        let required =
            |name: &str| optional(name).ok_or_else(|| EnvError::Missing(name.to_string()));

        let config = parse_config(&required(vars::CONFIG)?)
            .map_err(|_| EnvError::Invalid(vars::CONFIG.to_string()))?;
        let mut runner_tokens = BTreeMap::new();
        for pool in &config.pools {
            runner_tokens.insert(pool.tag.clone(), required(&vars::runner_token(&pool.tag))?);
        }
        let cache = match (
            optional(vars::CACHE_ACCESS_KEY),
            optional(vars::CACHE_SECRET_KEY),
        ) {
            (Some(access_key), Some(secret_key)) => Some(KeyPair {
                access_key,
                secret_key,
            }),
            _ => None,
        };
        let secrets = Secrets {
            hook_token: Some(required(vars::HOOK_TOKEN)?),
            runner_tokens,
            dispatch: None,
            cache,
        };
        let credentials = Credentials {
            secret_key: required(vars::SECRET_KEY)?,
            access_key: None,
            organization_id: required(vars::ORGANIZATION_ID)?,
            default_project_id: Some(required(vars::PROJECT_ID)?),
        };
        Ok(Env {
            config: Arc::new(config),
            secrets: Arc::new(secrets),
            credentials,
            region: required(vars::REGION)?,
        })
    }

    pub fn from_process() -> Result<Env, EnvError> {
        Env::from_lookup(|name| std::env::var(name).ok())
    }

    /// The `ScalewayJobs` provider on the real HTTP client.
    pub fn provider(&self, log: Log) -> Result<Arc<dyn Provider>, EnvError> {
        let http =
            ReqwestClient::new().map_err(|_| EnvError::Invalid("http client".to_string()))?;
        let client = ScalewayClient::new(Arc::new(http), self.credentials.clone(), &self.region);
        Ok(Arc::new(ScalewayJobs::new(
            self.config.clone(),
            Arc::new(client),
            log,
        )))
    }
}

/// Runs the dispatcher on one request; answers its status and JSON body.
pub async fn respond(
    env: &Env,
    provider: Arc<dyn Provider>,
    log: Log,
    headers: &[(String, String)],
    body: &[u8],
) -> (u16, Vec<u8>) {
    let dispatcher = Dispatcher::new(env.config.clone(), provider, env.secrets.clone(), log);
    let response = dispatcher.handle(headers, body).await;
    (response.status, response.body.to_string().into_bytes())
}

/// The answer when the environment is unusable: 500, logged with the variable name only.
pub fn env_error(error: &EnvError, log: &Log) -> (u16, Vec<u8>) {
    log.error("env", &[("variable", error.variable())]);
    error_body()
}

fn error_body() -> (u16, Vec<u8>) {
    (500, br#"{"result":"error"}"#.to_vec())
}

struct State {
    env: Env,
    provider: Arc<dyn Provider>,
}

fn state() -> &'static Result<State, EnvError> {
    static STATE: OnceLock<Result<State, EnvError>> = OnceLock::new();
    STATE.get_or_init(|| {
        let env = Env::from_process()?;
        let provider = env.provider(Log::stderr())?;
        Ok(State { env, provider })
    })
}

pub async fn handle(req: Request<Body>) -> Response<Body> {
    let log = Log::stderr();
    let (status, body) = match state() {
        Err(error) => env_error(error, &log),
        Ok(state) => {
            let (parts, body) = req.into_parts();
            let headers: Vec<(String, String)> = parts
                .headers
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|v| (name.as_str().to_string(), v.to_string()))
                })
                .collect();
            match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
                Ok(bytes) => {
                    respond(&state.env, state.provider.clone(), log, &headers, &bytes).await
                }
                Err(_) => (400, br#"{"result":"bad_request"}"#.to_vec()),
            }
        }
    };
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| {
            let (_, body) = error_body();
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            response
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciburst_core::fake::FakeProvider;
    use ciburst_core::store::MemoryStore;
    use ciburst_core::types::{Outputs, Rotation, Run, StartRequest, Step};
    use ciburst_core::{Error, Result};
    use serde_json::{Value, json};

    const CONFIG: &str = r#"{
      "name": "demo",
      "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
      "host": { "type": "scaleway-function" },
      "provider": { "type": "scaleway-jobs" },
      "defaultPool": "ci",
      "pools": [
        { "tag": "ci", "image": "img/node" },
        { "tag": "build-x", "image": "img/kaniko", "stages": ["build"] }
      ],
      "cache": { "type": "s3" }
    }"#;

    fn vars_map() -> BTreeMap<String, String> {
        [
            (vars::CONFIG, CONFIG),
            (vars::HOOK_TOKEN, "hook-secret"),
            ("RUNNER_TOKEN_CI", "glrt-ci"),
            ("RUNNER_TOKEN_BUILD_X", "glrt-build"),
            (vars::CACHE_ACCESS_KEY, "AK"),
            (vars::CACHE_SECRET_KEY, "SK"),
            (vars::SECRET_KEY, "dispatch-secret"),
            (vars::ORGANIZATION_ID, "org"),
            (vars::PROJECT_ID, "project"),
            (vars::REGION, "fr-par"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn env_from(map: &BTreeMap<String, String>) -> std::result::Result<Env, EnvError> {
        Env::from_lookup(|name| map.get(name).cloned())
    }

    async fn fake_provider(env: &Env) -> Arc<FakeProvider> {
        let provider = Arc::new(FakeProvider::new(
            env.config.clone(),
            Arc::new(MemoryStore::new()),
        ));
        provider
            .apply(&provider.plan(&[]).await.unwrap())
            .await
            .unwrap();
        provider
    }

    fn headers(token: &str) -> Vec<(String, String)> {
        vec![
            ("x-gitlab-token".to_string(), token.to_string()),
            ("x-gitlab-event".to_string(), "Job Hook".to_string()),
        ]
    }

    fn event(status: &str, stage: &str) -> Vec<u8> {
        json!({
            "object_kind": "build", "build_id": 42, "build_stage": stage,
            "build_status": status, "pipeline_id": 9, "project_id": 7
        })
        .to_string()
        .into_bytes()
    }

    fn parse(body: &[u8]) -> Value {
        serde_json::from_slice(body).unwrap()
    }

    #[test]
    fn reads_every_variable() {
        let env = env_from(&vars_map()).unwrap();
        assert_eq!(env.config.name, "demo");
        assert_eq!(env.secrets.hook_token.as_deref(), Some("hook-secret"));
        assert_eq!(env.secrets.runner_tokens["build-x"], "glrt-build");
        assert_eq!(env.secrets.cache.as_ref().unwrap().secret_key, "SK");
        assert_eq!(env.credentials.secret_key, "dispatch-secret");
        assert_eq!(
            env.credentials.default_project_id.as_deref(),
            Some("project")
        );
        assert_eq!(env.region, "fr-par");

        let mut no_cache = vars_map();
        no_cache.remove(vars::CACHE_SECRET_KEY);
        assert!(env_from(&no_cache).unwrap().secrets.cache.is_none());
    }

    #[test]
    fn names_the_missing_variable() {
        for name in [
            vars::CONFIG,
            vars::HOOK_TOKEN,
            "RUNNER_TOKEN_BUILD_X",
            vars::SECRET_KEY,
            vars::ORGANIZATION_ID,
            vars::PROJECT_ID,
            vars::REGION,
        ] {
            let mut map = vars_map();
            map.remove(name);
            let error = env_from(&map).err().unwrap();
            assert_eq!(error, EnvError::Missing(name.to_string()));
            let mut empty = vars_map();
            empty.insert(name.to_string(), String::new());
            assert_eq!(env_from(&empty).err().unwrap().variable(), name);
        }
        let mut bad = vars_map();
        bad.insert(vars::CONFIG.to_string(), "{".to_string());
        assert_eq!(
            env_from(&bad).err().unwrap(),
            EnvError::Invalid(vars::CONFIG.to_string())
        );
    }

    #[test]
    fn an_env_error_answers_500_and_logs_the_name_only() {
        let mut map = vars_map();
        map.remove(vars::HOOK_TOKEN);
        let error = env_from(&map).err().unwrap();
        let (log, lines) = Log::memory();
        let (status, body) = env_error(&error, &log);
        assert_eq!(status, 500);
        assert_eq!(parse(&body), json!({ "result": "error" }));
        let lines = lines.lock().unwrap();
        assert_eq!(lines.as_slice(), [r#"error env variable="HOOK_TOKEN""#]);
    }

    #[tokio::test]
    async fn rejects_requests_without_the_token() {
        let env = env_from(&vars_map()).unwrap();
        let provider = fake_provider(&env).await;
        let (status, _) = respond(
            &env,
            provider.clone(),
            Log::silent(),
            &[],
            &event("pending", "test"),
        )
        .await;
        assert_eq!(status, 401);
        let (status, _) = respond(
            &env,
            provider.clone(),
            Log::silent(),
            &headers("wrong"),
            &event("pending", "test"),
        )
        .await;
        assert_eq!(status, 401);
        assert!(provider.list(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignores_jobs_that_are_not_pending() {
        let env = env_from(&vars_map()).unwrap();
        let provider = fake_provider(&env).await;
        let (status, body) = respond(
            &env,
            provider.clone(),
            Log::silent(),
            &headers("hook-secret"),
            &event("running", "test"),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            parse(&body),
            json!({ "result": "ignored", "reason": "status", "status": "running" })
        );
        assert!(provider.list(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn starts_a_run_for_a_pending_job() {
        let env = env_from(&vars_map()).unwrap();
        let provider = fake_provider(&env).await;
        let (log, lines) = Log::memory();
        let (status, body) = respond(
            &env,
            provider.clone(),
            log,
            &headers("hook-secret"),
            &event("pending", "build"),
        )
        .await;
        assert_eq!(status, 200);
        let body = parse(&body);
        assert_eq!(body["result"], "started");
        assert_eq!(body["pool"], "build-x");
        let runs = provider.list(Some("build-x")).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].labels["job"], "42");
        let lines = lines.lock().unwrap();
        assert!(lines.iter().any(|l| l.starts_with("info started")));
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("glrt-") || l.contains("SK"))
        );
    }

    struct Failing(Arc<FakeProvider>);

    #[async_trait::async_trait]
    impl Provider for Failing {
        fn capabilities(&self) -> ciburst_core::types::Capabilities {
            self.0.capabilities()
        }
        async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
            self.0.plan(rotate).await
        }
        async fn apply(&self, steps: &[Step]) -> Result<Outputs> {
            self.0.apply(steps).await
        }
        async fn destroy(&self) -> Result<()> {
            self.0.destroy().await
        }
        async fn start(&self, _request: StartRequest) -> Result<String> {
            Err(Error::provider("scaleway POST /x: 403 quota"))
        }
        async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
            self.0.list(pool).await
        }
        async fn stop(&self, run_id: &str) -> Result<()> {
            self.0.stop(run_id).await
        }
    }

    #[tokio::test]
    async fn answers_500_when_the_provider_fails() {
        let env = env_from(&vars_map()).unwrap();
        let provider = Arc::new(Failing(fake_provider(&env).await));
        let (status, body) = respond(
            &env,
            provider,
            Log::silent(),
            &headers("hook-secret"),
            &event("pending", "test"),
        )
        .await;
        assert_eq!(status, 500);
        assert_eq!(parse(&body), json!({ "result": "error" }));
    }

    #[tokio::test]
    async fn handle_answers_json_with_the_status() {
        // The test process has no function environment, so `handle` reports it.
        let response = handle(
            Request::builder()
                .method("POST")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.headers()["content-type"], "application/json");
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(parse(&bytes), json!({ "result": "error" }));
    }
}
