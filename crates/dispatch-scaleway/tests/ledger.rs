//! The function's burst ledger against the fake Scaleway (M4 rule R13): built from the cache
//! key, project and region, absent without them, and written by `respond` for a pending job of a
//! VM pool that finds no free slot.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ciburst_core::config::parse_config;
use ciburst_core::ledger::Intent;
use ciburst_core::log::Log;
use ciburst_core::types::Provider;
use ciburst_dispatch_scaleway::{Env, Incoming, respond};
use ciburst_scaleway::host::vars;
use ciburst_scaleway::provider::Scaleway;
use ciburst_scaleway::testing::scaleway_fake::{
    FAKE_ACCESS_KEY, FAKE_ORGANIZATION_ID, FAKE_SECRET_KEY, ScalewayFake,
};
use serde_json::{Value, json};

const NOW: u64 = 1_800_000_000;
const HOST: &str = "demo-dispatch.functions.fnc.fr-par.scw.cloud";
const INTENT: &str = "ciburst/intents/vm/1800000000-00000000000000000042";

fn config(cache: &str) -> String {
    format!(
        r#"{{
          "name": "demo",
          "gitlab": {{ "url": "https://gitlab.example", "projectIds": [7] }},
          "host": {{ "type": "scaleway-function" }},
          "provider": {{ "type": "scaleway", "registry": true, "registryPublic": true }},
          "defaultPool": "vm",
          "pools": [ {{ "tag": "vm", "image": "img/docker", "slots": 2 }} ],
          "cache": {{ "type": "{cache}" }}
        }}"#
    )
}

/// The fake Scaleway after the provider's apply, the S3 fake accepting the cache key IAM made,
/// and the function's variables for that deployment.
async fn deployed(cache: &str) -> (Arc<ScalewayFake>, BTreeMap<String, String>) {
    let backend = Arc::new(ScalewayFake::new());
    let text = config(cache);
    let provider = Scaleway::new(
        Arc::new(parse_config(&text).unwrap()),
        backend.client(),
        Log::silent(),
    )
    .with_s3_access_key(FAKE_ACCESS_KEY);
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
        backend.s3.add_key(access, secret);
        vars.insert(vars::CACHE_ACCESS_KEY.to_string(), access.clone());
        vars.insert(vars::CACHE_SECRET_KEY.to_string(), secret.clone());
    }
    (backend, vars)
}

fn env(vars: &BTreeMap<String, String>) -> Env {
    let mut env = Env::from_lookup(|name| vars.get(name).cloned()).unwrap();
    env.group_wait = Duration::ZERO;
    env
}

fn bucket(vars: &BTreeMap<String, String>) -> String {
    format!("demo-cache-{}", vars[vars::PROJECT_ID])
}

/// `respond` on the pending event of job 42, through provider and ledger on the fake.
async fn pending_job(backend: &Arc<ScalewayFake>, env: &Env) -> (u16, Value) {
    let backends = env.backends_on(backend.clone(), Log::silent());
    let headers: Vec<(String, String)> = [
        ("x-gitlab-token", "hook-secret"),
        ("x-gitlab-event", "Job Hook"),
        ("host", HOST),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let body = json!({
        "object_kind": "build", "build_id": 42, "build_stage": "test",
        "build_status": "pending", "pipeline_id": 9, "project_id": 7
    })
    .to_string();
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

#[tokio::test]
async fn the_ledger_lives_in_the_cache_bucket_under_the_cache_key() {
    let (backend, vars) = deployed("s3").await;
    let env = env(&vars);
    let ledger = env.ledger_on(backend.clone()).expect("ledger");
    ledger.record("vm", NOW, "42").await.unwrap();
    assert_eq!(
        backend.s3.objects(&bucket(&vars)),
        [(INTENT.to_string(), Vec::new())]
    );
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
    let (backend, vars) = deployed("s3").await;
    for key in [vars::CACHE_ACCESS_KEY, vars::CACHE_SECRET_KEY] {
        let mut partial = vars.clone();
        partial.remove(key);
        let env = env(&partial);
        assert!(env.ledger_on(backend.clone()).is_none(), "{key}");
        assert!(
            env.backends_on(backend.clone(), Log::silent())
                .ledger
                .is_none()
        );
    }
    let (backend, mut vars) = deployed("none").await;
    vars.insert(vars::CACHE_ACCESS_KEY.to_string(), "SCWSTRAY".to_string());
    vars.insert(vars::CACHE_SECRET_KEY.to_string(), "stray".to_string());
    assert!(env(&vars).ledger_on(backend.clone()).is_none());
}

#[tokio::test]
async fn a_pending_vm_job_without_a_free_slot_starts_a_vm_with_one_slot_taken() {
    let (backend, vars) = deployed("s3").await;
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
    let (backend, vars) = deployed("s3").await;
    let (status, body) = pending_job(&backend, &env(&vars)).await;
    assert_eq!(
        (status, &body["result"]),
        (200, &json!("started")),
        "{body}"
    );
    assert_eq!(
        backend.s3.objects(&bucket(&vars)),
        [(INTENT.to_string(), Vec::new())]
    );
}
