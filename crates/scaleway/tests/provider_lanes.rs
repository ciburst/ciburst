//! `Provider::sync_lanes` against the Serverless Jobs fake (D60, spec 7.1, 8.3): a member's
//! definitions follow its `/join`, and the host's and other members' stay.

use std::sync::Arc;

use ciburst_core::config::{Lane, MAX_DEFINITION_NAME, Pool};
use ciburst_core::log::Log;
use ciburst_core::testing::contract::contract_config;
use ciburst_core::types::{Action, Provider};
use ciburst_scaleway::provider::Scaleway;
use ciburst_scaleway::testing::scaleway_fake::{FAKE_ACCESS_KEY, ScalewayFake};
use ciburst_scaleway::testing::{RUNNER_FIXTURE_URL, agent_fixture, runner_fixture_sha256};
use serde_json::{Value, json};

fn provider(backend: &Arc<ScalewayFake>) -> Scaleway {
    Scaleway::new(contract_config(), backend.client(), Log::silent())
        .with_s3_access_key(FAKE_ACCESS_KEY)
        .with_agent(Some(agent_fixture()))
        .with_runner_source(RUNNER_FIXTURE_URL, runner_fixture_sha256())
}

/// The host's resources applied, with its lanes `contract-7-ci` and `contract-7-build`.
async fn provisioned() -> (Arc<ScalewayFake>, Scaleway) {
    let backend = Arc::new(ScalewayFake::new());
    let provider = provider(&backend);
    let steps = provider.plan(&[]).await.expect("plan");
    provider.apply(&steps).await.expect("apply");
    (backend, provider)
}

fn lane(project: u64, value: Value) -> Pool {
    serde_json::from_value::<Lane>(value)
        .expect("lane")
        .pool(project)
}

fn node(project: u64) -> Pool {
    lane(
        project,
        json!({ "tag": "node", "image": "node:24-alpine", "jobs": ["lint"] }),
    )
}

/// The definitions by name, sorted.
fn names(backend: &ScalewayFake) -> Vec<String> {
    let mut names: Vec<String> = backend
        .jobs
        .definitions()
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

fn definition(backend: &ScalewayFake, name: &str) -> Value {
    backend
        .jobs
        .definitions()
        .into_iter()
        .find(|d| d["name"] == name)
        .unwrap_or_else(|| panic!("definition {name}"))
}

/// The writes to job definitions since `from` calls, as `METHOD path`.
fn writes_since(backend: &ScalewayFake, from: usize) -> Vec<String> {
    backend.jobs.calls()[from..]
        .iter()
        .filter(|c| c.method != "GET")
        .map(|c| format!("{} {}", c.method, c.path))
        .collect()
}

#[tokio::test]
async fn a_new_member_lane_gets_its_definition() {
    let (backend, provider) = provisioned().await;
    let host_before = definition(&backend, "contract-7-ci");
    let ids = provider.sync_lanes(8, &[node(8)]).await.unwrap();

    let created = definition(&backend, "contract-8-node");
    assert_eq!(ids.len(), 1);
    assert_eq!(ids["node"], created["id"].as_str().unwrap());
    assert_eq!(created["image_uri"], "node:24-alpine");
    assert_eq!(created["cpu_limit"], 2000);
    assert_eq!(created["memory_limit"], 4096);
    assert_eq!(created["description"], "ciburst lane 8-node");
    let env = &created["environment_variables"];
    assert_eq!(env["CIBURST_RUNNER"], "8-node");
    assert_eq!(env["CIBURST_PROJECT"], "8");
    assert_eq!(env["CIBURST_RUNNER_SHA256"], runner_fixture_sha256());
    assert!(env.get("CIBURST_VM_SECRET").is_none());
    assert_eq!(
        names(&backend),
        ["contract-7-build", "contract-7-ci", "contract-8-node"]
    );
    assert_eq!(definition(&backend, "contract-7-ci"), host_before);

    let calls = backend.jobs.calls().len();
    let again = provider.sync_lanes(8, &[node(8)]).await.unwrap();
    assert_eq!(again, ids);
    assert!(
        writes_since(&backend, calls).is_empty(),
        "unchanged: no write"
    );
}

#[tokio::test]
async fn a_changed_image_size_or_environment_updates_in_place() {
    let (backend, provider) = provisioned().await;
    let ids = provider.sync_lanes(8, &[node(8)]).await.unwrap();
    let id = ids["node"].clone();
    let path = format!("/serverless-jobs/v1alpha1/regions/fr-par/job-definitions/{id}");

    for (change, field, expected) in [
        (
            json!({ "image": "node:22-alpine" }),
            "image_uri",
            json!("node:22-alpine"),
        ),
        (json!({ "vcpu": 4 }), "cpu_limit", json!(4000)),
        (json!({ "memoryMiB": 8192 }), "memory_limit", json!(8192)),
        (
            json!({ "waitTimeoutS": 120 }),
            "environment_variables",
            json!("120"),
        ),
    ] {
        let mut value = json!({ "tag": "node", "image": "node:24-alpine", "jobs": ["lint"] });
        value
            .as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        let calls = backend.jobs.calls().len();
        let again = provider.sync_lanes(8, &[lane(8, value)]).await.unwrap();
        assert_eq!(again["node"], id, "{field}: same definition");
        assert_eq!(writes_since(&backend, calls), [format!("PATCH {path}")]);
        let patch = backend.jobs.calls().last().unwrap().body.clone().unwrap();
        assert!(patch.get("project_id").is_none(), "{patch}");
        let current = definition(&backend, "contract-8-node");
        let actual = match field {
            "environment_variables" => current[field]["RUNNER_WAIT_TIMEOUT"].clone(),
            _ => current[field].clone(),
        };
        assert_eq!(actual, expected, "{field}");
    }
    assert_eq!(backend.jobs.definitions().len(), 3);
}

/// D66: a definition of an earlier version names the lane key `CIBURST_POOL`; setup plans an
/// update of the host's and `/join` patches the member's to `CIBURST_RUNNER`.
#[tokio::test]
async fn a_definition_with_the_old_pool_variable_is_updated() {
    let (backend, provider) = provisioned().await;
    let ids = provider.sync_lanes(8, &[node(8)]).await.unwrap();
    let old_env = |name: &str| {
        let current = definition(&backend, name);
        let mut env = current["environment_variables"].clone();
        let map = env.as_object_mut().unwrap();
        let key = map.remove("CIBURST_RUNNER").expect("CIBURST_RUNNER");
        map.insert("CIBURST_POOL".into(), key);
        backend
            .jobs
            .set_environment(current["id"].as_str().unwrap(), env);
    };
    old_env("contract-7-build");
    old_env("contract-8-node");

    let steps = provider.plan(&[]).await.unwrap();
    let step = steps
        .iter()
        .find(|s| s.id == "definition:7-build")
        .expect("definition step");
    assert_eq!(step.action, Action::Update, "{step:?}");
    provider.apply(&steps).await.unwrap();
    let host = definition(&backend, "contract-7-build");
    assert_eq!(host["environment_variables"]["CIBURST_RUNNER"], "7-build");
    assert!(host["environment_variables"].get("CIBURST_POOL").is_none());

    let calls = backend.jobs.calls().len();
    let again = provider.sync_lanes(8, &[node(8)]).await.unwrap();
    assert_eq!(again, ids);
    let id = &ids["node"];
    assert_eq!(
        writes_since(&backend, calls),
        [format!(
            "PATCH /serverless-jobs/v1alpha1/regions/fr-par/job-definitions/{id}"
        )]
    );
    let member = definition(&backend, "contract-8-node");
    assert_eq!(member["environment_variables"]["CIBURST_RUNNER"], "8-node");
    assert!(
        member["environment_variables"]
            .get("CIBURST_POOL")
            .is_none()
    );
}

#[tokio::test]
async fn a_lane_no_longer_listed_is_deleted_and_only_that_projects() {
    let (backend, provider) = provisioned().await;
    let rust = lane(
        8,
        json!({ "tag": "rust", "image": "rust:1.96.0", "jobs": ["cargo-*"] }),
    );
    provider
        .sync_lanes(8, &[node(8), rust.clone()])
        .await
        .unwrap();
    provider.sync_lanes(9, &[node(9)]).await.unwrap();
    // Not lanes: another deployment's definition and one of an earlier version without a
    // project id in its name.
    let project = backend.jobs.definitions()[0]["project_id"]
        .as_str()
        .unwrap()
        .to_string();
    backend.jobs.add_definition(&project, "contractor-8-node");
    backend.jobs.add_definition(&project, "contract-node");
    let others = |backend: &ScalewayFake| {
        backend
            .jobs
            .definitions()
            .into_iter()
            .filter(|d| !d["name"].as_str().unwrap().starts_with("contract-8-"))
            .collect::<Vec<_>>()
    };
    let before = others(&backend);
    backend.jobs.set_max_page_size(1);

    let ids = provider.sync_lanes(8, &[rust]).await.unwrap();
    assert_eq!(ids.keys().collect::<Vec<_>>(), ["rust"]);
    assert_eq!(
        names(&backend),
        [
            "contract-7-build",
            "contract-7-ci",
            "contract-8-rust",
            "contract-9-node",
            "contract-node",
            "contractor-8-node",
        ]
    );
    assert_eq!(others(&backend), before);

    assert!(provider.sync_lanes(8, &[]).await.unwrap().is_empty());
    assert_eq!(others(&backend), before);
    assert_eq!(names(&backend).len(), 5);
}

#[tokio::test]
async fn a_lane_of_another_project_is_refused_before_any_call() {
    let (backend, provider) = provisioned().await;
    let calls = backend.calls().len();
    let error = provider
        .sync_lanes(8, &[node(8), node(9)])
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("9-node is not a lane of project 8"),
        "{error}"
    );
    assert_eq!(backend.calls().len(), calls);
}

#[tokio::test]
async fn definition_names_hold_to_63_characters() {
    let (backend, provider) = provisioned().await;
    // `contract-8-` takes 11 characters.
    let fits = "a".repeat(MAX_DEFINITION_NAME - 11);
    let ids = provider
        .sync_lanes(
            8,
            &[lane(
                8,
                json!({ "tag": fits, "image": "node:24-alpine", "jobs": ["a"] }),
            )],
        )
        .await
        .unwrap();
    assert!(ids.contains_key(&fits));
    assert_eq!(
        definition(&backend, &format!("contract-8-{fits}"))["name"]
            .as_str()
            .unwrap()
            .len(),
        MAX_DEFINITION_NAME
    );

    let long = "a".repeat(MAX_DEFINITION_NAME - 10);
    let error = provider
        .sync_lanes(
            8,
            &[lane(
                8,
                json!({ "tag": long, "image": "node:24-alpine", "jobs": ["a"] }),
            )],
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("at most 63 characters"),
        "{error}"
    );
    assert!(
        names(&backend)
            .iter()
            .all(|name| name.len() <= MAX_DEFINITION_NAME),
        "{:?}",
        names(&backend)
    );
}
