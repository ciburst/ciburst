//! Serverless Jobs `serverless-jobs/v1alpha1`: job definitions per pool, runs, start and stop.

use std::collections::BTreeMap;

use ciburst_core::config::{CacheType, Config, Pool};
use ciburst_core::provision::keys;
use ciburst_core::types::{Action, Run, RunState, StartRequest};
use ciburst_core::{Error, Result};
use serde_json::{Value, json};

use super::{PROVIDER_NAME, Scaleway, list_all, s3};
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

/// The run environment: per-run overrides plus the labels as `CIBURST_*` variables. The VM
/// secret signs VM tokens and stays in the dispatcher, so a job never receives it.
pub fn run_env(
    env: &BTreeMap<String, String>,
    labels: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = env.clone();
    out.remove(keys::VM_SECRET);
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

impl Scaleway {
    pub(super) fn definition_name(&self, tag: &str) -> String {
        format!("{}-{tag}", self.name())
    }

    pub(super) fn definition_tag<'a>(&self, definition: &'a Value) -> Option<&'a str> {
        definition["name"]
            .as_str()?
            .strip_prefix(self.name())?
            .strip_prefix('-')
            .filter(|tag| !tag.is_empty())
    }

    pub(super) async fn apply_definition(
        &self,
        tag: &str,
        action: Action,
        project_id: &str,
    ) -> Result<()> {
        let name = self.definition_name(tag);
        let existing = list_definitions(&self.client, project_id)
            .await?
            .into_iter()
            .find(|d| d["name"].as_str() == Some(name.as_str()));
        let existing_id = existing
            .as_ref()
            .and_then(|d| d["id"].as_str())
            .map(String::from);
        if action == Action::Delete {
            if let Some(id) = existing_id {
                delete_definition(&self.client, &id).await?;
                self.log
                    .info("deleted job definition", &[("name", &name), ("id", &id)]);
            }
            return Ok(());
        }
        let pool = self
            .config
            .pool_by_tag(tag)
            .ok_or_else(|| Error::provider(format!("{PROVIDER_NAME}: unknown pool {tag}")))?;
        let body = desired_definition(&self.config, pool, self.client.region(), project_id);
        match (existing, existing_id) {
            (Some(current), Some(id)) => {
                if !differences(&body, &current).is_empty() {
                    update_definition(&self.client, &id, &body).await?;
                    self.log
                        .info("updated job definition", &[("name", &name), ("id", &id)]);
                }
            }
            _ => {
                let id = create_definition(&self.client, &body).await?;
                self.log
                    .info("created job definition", &[("name", &name), ("id", &id)]);
            }
        }
        Ok(())
    }

    async fn find_definition(&self, project_id: &str, tag: &str) -> Result<Option<String>> {
        let name = self.definition_name(tag);
        Ok(list_definitions(&self.client, project_id)
            .await?
            .iter()
            .find(|d| d["name"].as_str() == Some(name.as_str()))
            .and_then(|d| d["id"].as_str().map(String::from)))
    }

    pub(super) async fn start_job(&self, request: StartRequest) -> Result<String> {
        let tag = &request.pool.tag;
        let not_provisioned =
            || Error::provider(format!("{PROVIDER_NAME}: pool {tag} is not provisioned"));
        let project_id = self
            .client
            .find_project(self.name())
            .await?
            .ok_or_else(not_provisioned)?;
        let definition_id = self
            .find_definition(&project_id, tag)
            .await?
            .ok_or_else(not_provisioned)?;
        let env = run_env(&request.env, &request.labels);
        start(&self.client, &definition_id, &env).await
    }

    pub(super) async fn job_runs(&self, project_id: &str, tags: &[&str]) -> Result<Vec<Run>> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }
        let definitions = list_definitions(&self.client, project_id).await?;
        let mut runs = Vec::new();
        for tag in tags {
            let name = self.definition_name(tag);
            let Some(definition_id) = definitions
                .iter()
                .find(|d| d["name"].as_str() == Some(name.as_str()))
                .and_then(|d| d["id"].as_str())
            else {
                continue;
            };
            for run in list_runs(&self.client, project_id, definition_id).await? {
                runs.push(Run {
                    run_id: run["id"].as_str().unwrap_or_default().to_string(),
                    pool: tag.to_string(),
                    state: run_state(run["state"].as_str().unwrap_or_default()),
                    created_at: run["created_at"].as_str().unwrap_or_default().to_string(),
                    labels: labels_from_env(&run["environment_variables"]),
                });
            }
        }
        Ok(runs)
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
        let env = BTreeMap::from([
            ("RUNNER_TOKEN".to_string(), "t".to_string()),
            ("CIBURST_VM_SECRET".to_string(), "s".to_string()),
        ]);
        let merged = run_env(&env, &labels);
        assert_eq!(merged["CIBURST_STAGE"], "test");
        assert_eq!(merged["RUNNER_TOKEN"], "t");
        assert!(!merged.contains_key("CIBURST_VM_SECRET"));
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
