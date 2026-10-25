//! The function against the fake Scaleway: the burst ledger (M4 rule R13) and the warm start
//! records beside it (M6.1 rule R5), built from the cache key, project and region, absent
//! without them, the ledger written by `respond` for a pending job of a VM pool that finds no
//! free slot; the agent of `CIBURST_AGENT_SHA256` (D38) and the pinned runner of the cache bucket
//! (M6 rule R6) in the user data of the VMs it starts; and the Scaleway calls of such a start
//! with and without the variables of M7 hardening rule R3.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ciburst_core::cloud_init::RUNNER_SHA256;
use ciburst_core::config::{CacheType, parse_config};
use ciburst_core::http::{HttpClient, Request};
use ciburst_core::ledger::{Intent, VmJobs};
use ciburst_core::log::Log;
use ciburst_core::types::Provider;
use ciburst_dispatch_scaleway::{Env, EnvError, Incoming, respond};
use ciburst_scaleway::host::vars;
use ciburst_scaleway::provider::Scaleway;
use ciburst_scaleway::testing::scaleway_fake::{
    Call, FAKE_ACCESS_KEY, FAKE_ORGANIZATION_ID, FAKE_SECRET_KEY, ScalewayFake,
};
use ciburst_scaleway::testing::{
    RUNNER_FIXTURE_URL, agent_fixture, agent_fixture_sha256, runner_fixture_sha256,
};
use serde_json::{Value, json};

const NOW: u64 = 1_800_000_000;
const HOST: &str = "demo-dispatch.functions.fnc.fr-par.scw.cloud";
const INTENT: &str = "ciburst/intents/vm/1800000000-00000000000000000042";

/// A VM pool on `runtime`; every config with a pool has an S3 cache (M6 rule R1).
fn config(runtime: &str) -> String {
    format!(
        r#"{{
          "name": "demo",
          "gitlab": {{ "url": "https://gitlab.example", "projectIds": [7] }},
          "host": {{ "type": "scaleway-function" }},
          "provider": {{ "type": "scaleway" }},
          "defaultPool": "vm",
          "pools": [ {{ "tag": "vm", "image": "img/docker", "slots": 2, "runtime": "{runtime}" }} ],
          "cache": {{ "type": "s3" }}
        }}"#
    )
}

/// The fake Scaleway after the provider's apply with the agent and runner fixtures, the S3
/// fake accepting the cache key IAM made (scoped to the deployment's project, as Scaleway
/// scopes a key to its preferred project), and the function's variables for that deployment.
async fn deployed(runtime: &str) -> (Arc<ScalewayFake>, BTreeMap<String, String>) {
    let backend = Arc::new(ScalewayFake::new());
    let text = config(runtime);
    let provider = Scaleway::new(
        Arc::new(parse_config(&text).unwrap()),
        backend.client(),
        Log::silent(),
    )
    .with_s3_access_key(FAKE_ACCESS_KEY)
    .with_agent(Some(agent_fixture()))
    .with_runner_source(RUNNER_FIXTURE_URL, runner_fixture_sha256());
    let outputs = provider
        .apply(&provider.plan(&[]).await.unwrap())
        .await
        .unwrap();
    let mut vars = BTreeMap::from([
        (vars::CONFIG.to_string(), text),
        (vars::HOOK_TOKEN.to_string(), "hook-secret".to_string()),
        (vars::VM_SECRET.to_string(), "vm-secret".to_string()),
        (vars::SWEEP_TOKEN.to_string(), "sweep-secret".to_string()),
        ("RUNNER_TOKEN_VM".to_string(), "glrt-vm".to_string()),
        (vars::SECRET_KEY.to_string(), FAKE_SECRET_KEY.to_string()),
        (
            vars::ORGANIZATION_ID.to_string(),
            FAKE_ORGANIZATION_ID.to_string(),
        ),
        (
            vars::PROJECT_ID.to_string(),
            outputs["CIBURST_PROJECT_ID"].clone(),
        ),
        (vars::REGION.to_string(), "fr-par".to_string()),
    ]);
    if let (Some(access), Some(secret)) = (
        outputs.get("CIBURST_CACHE_ACCESS_KEY"),
        outputs.get("CIBURST_CACHE_SECRET_KEY"),
    ) {
        backend
            .s3
            .add_key_in(access, secret, &outputs["CIBURST_PROJECT_ID"]);
        vars.insert(vars::CACHE_ACCESS_KEY.to_string(), access.clone());
        vars.insert(vars::CACHE_SECRET_KEY.to_string(), secret.clone());
    }
    if let Some(sha256) = outputs.get(vars::AGENT_SHA256) {
        vars.insert(vars::AGENT_SHA256.to_string(), sha256.clone());
    }
    (backend, vars)
}

/// The intents in the cache bucket, without the agent next to them.
fn intents(backend: &ScalewayFake, vars: &BTreeMap<String, String>) -> Vec<(String, Vec<u8>)> {
    backend
        .s3
        .objects(&bucket(vars))
        .into_iter()
        .filter(|(key, _)| key.starts_with("ciburst/intents/"))
        .collect()
}

fn env(vars: &BTreeMap<String, String>) -> Env {
    let mut env = Env::from_lookup(|name| vars.get(name).cloned()).unwrap();
    env.group_wait = Duration::ZERO;
    env
}

fn bucket(vars: &BTreeMap<String, String>) -> String {
    format!("demo-cache-{}", vars[vars::PROJECT_ID])
}

/// `respond` on a webhook of kind `event` (`Job Hook`, `Pipeline Hook`), through provider,
/// ledger and records on the fake.
async fn hook(backend: &Arc<ScalewayFake>, env: &Env, event: &str, body: &Value) -> (u16, Value) {
    let backends = env.backends_on(backend.clone(), Log::silent());
    let headers: Vec<(String, String)> = [
        ("x-gitlab-token", "hook-secret"),
        ("x-gitlab-event", event),
        ("host", HOST),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let body = body.to_string();
    let request = Incoming {
        method: "POST",
        path: "/",
        headers: &headers,
        body: body.as_bytes(),
        now: NOW,
    };
    let (status, body) = respond(env, &backends, Log::silent(), &request).await;
    (status, serde_json::from_slice(&body).unwrap())
}

/// The pending event of job `job` of pipeline 9.
fn job_event(job: u64) -> Value {
    json!({
        "object_kind": "build", "build_id": job, "build_name": format!("job-{job}"),
        "build_stage": "test", "build_status": "pending", "pipeline_id": 9, "project_id": 7
    })
}

/// `respond` on the pending event of job 42, through provider and ledger on the fake.
async fn pending_job(backend: &Arc<ScalewayFake>, env: &Env) -> (u16, Value) {
    hook(backend, env, "Job Hook", &job_event(42)).await
}

#[tokio::test]
async fn the_ledger_lives_in_the_cache_bucket_under_the_cache_key() {
    let (backend, vars) = deployed("instances").await;
    let env = env(&vars);
    let ledger = env.ledger_on(backend.clone()).expect("ledger");
    ledger.record("vm", NOW, "42").await.unwrap();
    assert_eq!(intents(&backend, &vars), [(INTENT.to_string(), Vec::new())]);
    assert_eq!(
        ledger.since("vm", NOW - 30).await.unwrap(),
        [Intent {
            at: NOW,
            job: "42".into()
        }]
    );
    let put = backend
        .s3
        .calls()
        .into_iter()
        .find(|c| c.method == "PUT" && c.path.ends_with("-00000000000000000042"))
        .expect("intent put");
    assert_eq!(put.path, format!("/{}/{INTENT}", bucket(&vars)));
    assert!(
        env.backends_on(backend.clone(), Log::silent())
            .ledger
            .is_some()
    );
}

#[tokio::test]
async fn no_ledger_without_both_cache_keys_or_an_s3_cache() {
    let (backend, vars) = deployed("instances").await;
    for key in [vars::CACHE_ACCESS_KEY, vars::CACHE_SECRET_KEY] {
        let mut partial = vars.clone();
        partial.remove(key);
        let env = env(&partial);
        assert!(env.ledger_on(backend.clone()).is_none(), "{key}");
        assert!(env.records_on(backend.clone()).is_none(), "{key}");
        let backends = env.backends_on(backend.clone(), Log::silent());
        assert!(backends.ledger.is_none() && backends.records.is_none());
    }
    // A config with pools always has an S3 cache (M6 rule R1); the function still builds no
    // ledger from cache keys when the cache is not S3.
    let mut no_cache = env(&vars);
    assert!(no_cache.secrets.cache.is_some());
    let mut config = (*no_cache.config).clone();
    config.cache.kind = CacheType::None;
    no_cache.config = Arc::new(config);
    assert!(no_cache.ledger_on(backend.clone()).is_none());
    assert!(no_cache.records_on(backend.clone()).is_none());
}

/// The warm start records (M6.1 rule R5) beside the intents, under the cache key IAM scoped to
/// the deployment's project.
#[tokio::test]
async fn the_records_live_beside_the_ledger_under_the_cache_key() {
    let (backend, vars) = deployed("instances").await;
    let env = env(&vars);
    let records = env.records_on(backend.clone()).expect("records");
    let jobs: VmJobs = [("vm".to_string(), vec!["41".to_string(), "42".to_string()])].into();
    assert!(records.open("9", &jobs).await.unwrap());
    assert!(!records.open("9", &jobs).await.unwrap());
    records.seen("9", "41").await.unwrap();
    assert_eq!(records.vm_jobs("9").await.unwrap(), Some(jobs));
    assert_eq!(records.seen_jobs("9").await.unwrap(), ["41"]);
    assert_eq!(
        intents(&backend, &vars),
        [
            (
                "ciburst/intents/_pipelines/9/seen/41".to_string(),
                Vec::new()
            ),
            (
                "ciburst/intents/_pipelines/9/vm-jobs.json".to_string(),
                br#"{"vm":["41","42"]}"#.to_vec()
            ),
        ]
    );
    records.close("9").await.unwrap();
    assert!(intents(&backend, &vars).is_empty());
    assert!(
        env.backends_on(backend.clone(), Log::silent())
            .records
            .is_some()
    );
}

/// A pipeline event of pipeline 9 with `status` and the builds `(id, stage, status)`.
fn pipeline_event(status: &str, builds: &[(u64, &str, &str)]) -> Value {
    let builds: Vec<Value> = builds
        .iter()
        .map(|(id, stage, status)| {
            json!({
                "id": id, "name": format!("job-{id}"), "stage": stage, "status": status,
                "when": "on_success", "manual": false
            })
        })
        .collect();
    json!({
        "object_kind": "pipeline",
        "object_attributes": { "id": 9, "status": status },
        "project": { "id": 7 },
        "builds": builds
    })
}

/// The records' keys in the cache bucket.
fn record_keys(backend: &ScalewayFake, vars: &BTreeMap<String, String>) -> Vec<String> {
    intents(backend, vars)
        .into_iter()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with("ciburst/intents/_pipelines/"))
        .collect()
}

/// M6.1 rules R3 to R5 through the function: the warm step writes the record, a VM job's
/// pending event its marker, the final status deletes both.
#[tokio::test]
async fn pipeline_events_keep_their_records_in_the_cache_bucket() {
    let (backend, vars) = deployed("instances").await;
    let env = env(&vars);
    let builds = [(41, "test", "pending"), (42, "build", "created")];
    let (status, body) = hook(
        &backend,
        &env,
        "Pipeline Hook",
        &pipeline_event("pending", &builds),
    )
    .await;
    assert_eq!((status, &body["result"]), (200, &json!("warm")), "{body}");
    assert_eq!(body["pools"][0]["action"], "started", "{body}");
    let records = env.records_on(backend.clone()).unwrap();
    let mut jobs = records.vm_jobs("9").await.unwrap().expect("record")["vm"].clone();
    jobs.sort();
    assert_eq!(jobs, ["41", "42"]);

    let (status, body) = hook(&backend, &env, "Job Hook", &job_event(41)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(records.seen_jobs("9").await.unwrap(), ["41"]);

    let (status, body) = hook(
        &backend,
        &env,
        "Pipeline Hook",
        &pipeline_event("success", &builds),
    )
    .await;
    assert_eq!((status, &body["result"]), (200, &json!("closed")), "{body}");
    assert!(record_keys(&backend, &vars).is_empty());
}

#[tokio::test]
async fn a_pending_vm_job_without_a_free_slot_starts_a_vm_with_one_slot_taken() {
    let (backend, vars) = deployed("instances").await;
    let (status, body) = pending_job(&backend, &env(&vars)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"], "started", "{body}");
    assert_eq!(body["pool"], "vm");
    let server = backend
        .instance
        .server(body["runId"].as_str().unwrap())
        .expect("server");
    let tags: Vec<&str> = server["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert!(tags.contains(&"ciburst-free:1"), "{tags:?}");
}

#[tokio::test]
async fn a_pending_vm_job_without_a_free_slot_records_its_intent() {
    let (backend, vars) = deployed("instances").await;
    let (status, body) = pending_job(&backend, &env(&vars)).await;
    assert_eq!(
        (status, &body["result"]),
        (200, &json!("started")),
        "{body}"
    );
    assert_eq!(intents(&backend, &vars), [(INTENT.to_string(), Vec::new())]);
}

#[tokio::test]
async fn the_function_hands_its_agent_to_the_vms_it_starts() {
    let (backend, vars) = deployed("instances").await;
    let sha = agent_fixture_sha256();
    assert_eq!(vars[vars::AGENT_SHA256], sha, "apply reported the agent");
    let (status, body) = pending_job(&backend, &env(&vars)).await;
    assert_eq!(
        (status, &body["result"]),
        (200, &json!("started")),
        "{body}"
    );
    let id = body["runId"].as_str().unwrap();
    let user_data = backend.instance.user_data(id, "cloud-init").unwrap();
    let url = format!(
        "https://{}.s3.fr-par.scw.cloud/ciburst/agent/{sha}",
        bucket(&vars)
    );
    assert!(user_data.contains(&format!("curl -fsSL -o /tmp/ciburst-vm-agent {url} ")));
    assert!(user_data.contains(&format!("echo \"{sha}  /tmp/ciburst-vm-agent\"")));
    let runner = format!(
        "https://{}.s3.fr-par.scw.cloud/ciburst/runner/{RUNNER_SHA256}",
        bucket(&vars)
    );
    assert!(
        user_data.contains(&format!(
            "curl -fsSL -o /tmp/gitlab-runner {runner} && echo \"{RUNNER_SHA256}  /tmp/gitlab-runner\" | sha256sum -c - "
        )),
        "the function's VMs fetch the pinned runner from the cache bucket"
    );

    let download = backend
        .send(Request::new("GET", url.as_str()))
        .await
        .unwrap();
    assert_eq!(download.status, 200, "the VM downloads without credentials");
    assert_eq!(
        download.body,
        std::fs::read(agent_fixture()).unwrap(),
        "the bytes cloud-init checks"
    );

    let mut without = vars.clone();
    without.remove(vars::AGENT_SHA256);
    let (status, body) = pending_job(&backend, &env(&without)).await;
    assert_eq!((status, &body["result"]), (500, &json!("error")), "{body}");

    let mut invalid = vars.clone();
    invalid.insert(vars::AGENT_SHA256.to_string(), "not-a-hash".to_string());
    assert_eq!(
        Env::from_lookup(|name| invalid.get(name).cloned()).err(),
        Some(EnvError::Invalid(vars::AGENT_SHA256.to_string()))
    );
}

/// `<method> <path>` of each call after the first `from`.
fn since(calls: Vec<Call>, from: usize) -> Vec<String> {
    calls[from..]
        .iter()
        .map(|c| format!("{} {}", c.method, c.path))
        .collect()
}

/// The Instance calls and the number of all Scaleway calls of one pending job that starts a
/// VM through the function.
async fn vm_start_calls(backend: &Arc<ScalewayFake>, env: &Env) -> (Vec<String>, usize) {
    let (instance, all) = (backend.instance.calls().len(), backend.calls().len());
    let (status, body) = pending_job(backend, env).await;
    assert_eq!(
        (status, &body["result"]),
        (200, &json!("started")),
        "{body}"
    );
    (
        since(backend.instance.calls(), instance),
        backend.calls().len() - all,
    )
}

/// M7 hardening rule R3, measured on the fake: a pending job that starts a VM makes nine
/// Scaleway calls through a function without `CIBURST_VM_SECURITY_GROUP` and
/// `CIBURST_VM_IMAGES`, and seven with the values the host resolves at apply: the capacity read,
/// create, user data and power on, the burst intent's write and read, and the read of its
/// pipeline's warm start record (M6.1 rule R5).
#[tokio::test]
async fn the_function_starts_a_vm_without_lookups_on_the_resolved_variables() {
    let (backend, mut vars) = deployed("instances").await;
    let zone = "GET /instance/v1/zones/fr-par-1";
    let lookups = [
        format!("{zone}/security_groups"),
        "GET /marketplace/v2/local-images".to_string(),
    ];

    let (looked_up, total) = vm_start_calls(&backend, &env(&vars)).await;
    assert_eq!(looked_up.len(), 6, "{looked_up:#?}");
    assert_eq!(looked_up[0], format!("{zone}/servers"), "capacity");
    assert_eq!(looked_up[1..3], lookups);
    assert_eq!(total, 9);

    let config = Arc::new(parse_config(&vars[vars::CONFIG]).unwrap());
    let provider = Scaleway::new(config, backend.client(), Log::silent());
    let resolved = provider
        .resolve_vm_start(&vars[vars::PROJECT_ID])
        .await
        .unwrap()
        .unwrap();
    vars.extend(resolved.vars());
    let (direct, total) = vm_start_calls(&backend, &env(&vars)).await;
    assert_eq!(direct.len(), 4, "{direct:#?}");
    assert_eq!(direct[0], format!("{zone}/servers"), "capacity");
    assert!(
        direct.iter().all(|call| !lookups.contains(call)),
        "{direct:#?}"
    );
    assert_eq!(total, 7);
}
