//! Serverless Jobs `serverless-jobs/v1alpha1`: job definitions per pool, runs, start and stop.

use std::collections::BTreeMap;

use ciburst_core::config::{CacheType, Config, Pool};
use ciburst_core::types::RunState;
use ciburst_core::{Error, Result};
use serde_json::{Value, json};

use super::{list_all, s3};
use crate::client::ScalewayClient;

/// Label key and the run environment variable that carries it.
pub const LABEL_ENV: [(&str, &str); 4] = [
    ("project", "CIBURST_PROJECT"),
    ("pipeline", "CIBURST_PIPELINE"),
    ("job", "CIBURST_JOB"),
    ("stage", "CIBURST_STAGE"),
];

pub fn base(client: &ScalewayClient) -> String {
    format!("/serverless-jobs/v1alpha1/regions/{}", client.region())
}

pub async fn list_definitions(client: &ScalewayClient, project_id: &str) -> Result<Vec<Value>> {
    list_all(
        client,
        &format!("{}/job-definitions", base(client)),
        vec![("project_id", project_id.to_string())],
        "job_definitions",
    )
    .await
}

/// The cache bucket of a project, `<name>-cache-<project id>`.
pub fn bucket_name(config: &Config, project_id: &str) -> String {
    format!("{}-cache-{project_id}", config.name)
}

pub fn definition_env(config: &Config, pool: &Pool, region: &str, project_id: &str) -> Value {
    let mut env = BTreeMap::from([
        ("CIBURST_POOL".to_string(), pool.tag.clone()),
        ("CI_SERVER_URL".to_string(), config.gitlab.url.clone()),
        (
            "RUNNER_WAIT_TIMEOUT".to_string(),
            pool.wait_timeout_s.to_string(),
        ),
    ]);
    if config.cache.kind == CacheType::S3 {
        for (key, value) in [
            ("CACHE_TYPE", "s3".to_string()),
            ("CACHE_S3_SERVER_ADDRESS", s3::host(region)),
            ("CACHE_S3_BUCKET_NAME", bucket_name(config, project_id)),
            ("CACHE_S3_BUCKET_LOCATION", region.to_string()),
            ("CACHE_SHARED", "true".to_string()),
        ] {
            env.insert(key.to_string(), value);
        }
    }
    json!(env)
}

/// The full definition body for create and update.
pub fn desired_definition(config: &Config, pool: &Pool, region: &str, project_id: &str) -> Value {
    json!({
        "name": format!("{}-{}", config.name, pool.tag),
        "cpu_limit": pool.vcpu * 1000,
        "memory_limit": pool.memory_mib,
        "local_storage_capacity": pool.storage_mib,
        "image_uri": pool.image,
        "command": "",
        "project_id": project_id,
        "environment_variables": definition_env(config, pool, region, project_id),
        "description": format!("ciburst pool {}", pool.tag),
        "job_timeout": format!("{}s", pool.timeout_s),
    })
}

/// Seconds of a protobuf JSON duration such as `"3600s"` or `"3600.000000000s"`.
pub fn duration_seconds(value: &Value) -> Option<u64> {
    let text = value.as_str()?.strip_suffix('s')?;
    let seconds: f64 = text.parse().ok()?;
    (seconds >= 0.0).then(|| seconds.round() as u64)
}

/// Fields of `desired` that differ in `actual`; empty when the definition is up to date.
pub fn differences(desired: &Value, actual: &Value) -> Vec<&'static str> {
    let mut out = Vec::new();
    for field in [
        "cpu_limit",
        "memory_limit",
        "local_storage_capacity",
        "image_uri",
        "command",
        "environment_variables",
    ] {
        let actual_value = match (field, &actual[field]) {
            ("command", Value::Null) => json!(""),
            ("environment_variables", Value::Null) => json!({}),
            (_, value) => value.clone(),
        };
        if desired[field] != actual_value {
            out.push(field);
        }
    }
    if duration_seconds(&desired["job_timeout"]) != duration_seconds(&actual["job_timeout"]) {
        out.push("job_timeout");
    }
    out
}

pub async fn create_definition(client: &ScalewayClient, body: &Value) -> Result<String> {
    let created = client
        .post(&format!("{}/job-definitions", base(client)), body)
        .await?;
    created["id"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| Error::provider("scaleway job definition: response without id"))
}

pub async fn update_definition(client: &ScalewayClient, id: &str, body: &Value) -> Result<()> {
    let mut patch = body.clone();
    if let Some(fields) = patch.as_object_mut() {
        fields.remove("project_id");
    }
    client
        .patch(&format!("{}/job-definitions/{id}", base(client)), &patch)
        .await
        .map(drop)
}

pub async fn delete_definition(client: &ScalewayClient, id: &str) -> Result<()> {
    client
        .delete(&format!("{}/job-definitions/{id}", base(client)))
        .await
        .map(drop)
}

/// The run environment: per-run overrides plus the labels as `CIBURST_*` variables.
pub fn run_env(
    env: &BTreeMap<String, String>,
    labels: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = env.clone();
    for (label, var) in LABEL_ENV {
        if let Some(value) = labels.get(label) {
            out.insert(var.to_string(), value.clone());
        }
    }
    out
}

/// Labels rebuilt from a run's environment variables.
pub fn labels_from_env(env: &Value) -> BTreeMap<String, String> {
    LABEL_ENV
        .iter()
        .filter_map(|(label, var)| {
            env[*var]
                .as_str()
                .map(|value| (label.to_string(), value.to_string()))
        })
        .collect()
}

pub async fn start(
    client: &ScalewayClient,
    definition_id: &str,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let response = client
        .post(
            &format!("{}/job-definitions/{definition_id}/start", base(client)),
            &json!({ "environment_variables": env }),
        )
        .await?;
    response["job_runs"]
        .as_array()
        .and_then(|runs| runs.first())
        .and_then(|run| run["id"].as_str())
        .map(String::from)
        .ok_or_else(|| Error::provider("scaleway start: response without job_runs[0].id"))
}

pub async fn list_runs(
    client: &ScalewayClient,
    project_id: &str,
    definition_id: &str,
) -> Result<Vec<Value>> {
    list_all(
        client,
        &format!("{}/job-runs", base(client)),
        vec![
            ("project_id", project_id.to_string()),
            ("job_definition_id", definition_id.to_string()),
        ],
        "job_runs",
    )
    .await
}

pub async fn stop(client: &ScalewayClient, run_id: &str) -> Result<()> {
    client
        .post(
            &format!("{}/job-runs/{run_id}/stop", base(client)),
            &json!({}),
        )
        .await
        .map(drop)
}

/// Rule 5: `queued` stays queued, `scheduled` and `running` run, `succeeded` is done; `failed`,
/// `canceled`, `internal_error` and anything unknown are failed, so they never hold capacity.
pub fn run_state(state: &str) -> RunState {
    match state {
        "queued" => RunState::Queued,
        "scheduled" | "running" => RunState::Running,
        "succeeded" => RunState::Done,
        _ => RunState::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciburst_core::testing::contract::contract_config;

    #[test]
    fn maps_every_documented_state() {
        for (state, expected, live) in [
            ("queued", RunState::Queued, true),
            ("scheduled", RunState::Running, true),
            ("running", RunState::Running, true),
            ("succeeded", RunState::Done, false),
            ("failed", RunState::Failed, false),
            ("canceled", RunState::Failed, false),
            ("internal_error", RunState::Failed, false),
            ("unknown_state", RunState::Failed, false),
        ] {
            assert_eq!(run_state(state), expected, "{state}");
            assert_eq!(run_state(state).is_live(), live, "{state}");
        }
    }

    #[test]
    fn labels_round_trip_through_the_run_env() {
        let labels: BTreeMap<String, String> = [
            ("project", "7"),
            ("pipeline", "1"),
            ("job", "2"),
            ("stage", "test"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let env = BTreeMap::from([("RUNNER_TOKEN".to_string(), "t".to_string())]);
        let merged = run_env(&env, &labels);
        assert_eq!(merged["CIBURST_STAGE"], "test");
        assert_eq!(merged["RUNNER_TOKEN"], "t");
        let mut as_run = json!(merged);
        as_run["CIBURST_POOL"] = json!("ci");
        assert_eq!(labels_from_env(&as_run), labels);
        assert!(labels_from_env(&Value::Null).is_empty());
    }

    #[test]
    fn builds_the_definition_of_rule_four() {
        let config = contract_config();
        let pool = &config.pools[0];
        let body = desired_definition(&config, pool, "fr-par", "p-1");
        assert_eq!(body["name"], "contract-ci");
        assert_eq!(body["cpu_limit"], 2000);
        assert_eq!(body["memory_limit"], 4096);
        assert_eq!(body["local_storage_capacity"], 10240);
        assert_eq!(body["job_timeout"], "3600s");
        assert_eq!(
            body["environment_variables"],
            json!({
                "CIBURST_POOL": "ci",
                "CI_SERVER_URL": "https://gitlab.example",
                "RUNNER_WAIT_TIMEOUT": "60",
                "CACHE_TYPE": "s3",
                "CACHE_S3_SERVER_ADDRESS": "s3.fr-par.scw.cloud",
                "CACHE_S3_BUCKET_NAME": "contract-cache-p-1",
                "CACHE_S3_BUCKET_LOCATION": "fr-par",
                "CACHE_SHARED": "true",
            })
        );
        let mut actual = body.clone();
        actual["job_timeout"] = json!("3600.000000000s");
        actual["command"] = Value::Null;
        assert!(differences(&body, &actual).is_empty());
        actual["cpu_limit"] = json!(4000);
        actual["job_timeout"] = json!("60s");
        assert_eq!(differences(&body, &actual), ["cpu_limit", "job_timeout"]);
    }
}
