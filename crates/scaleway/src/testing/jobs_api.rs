//! Fake of the Serverless Jobs API (`/serverless-jobs/v1alpha1/regions/<region>`): job
//! definitions, start with environment overrides, job runs and stop. Shapes from scaleway-sdk-go
//! `api/jobs/v1alpha1`: `job_definitions`, `job_runs`, `total_count`; start answers
//! `{ "job_runs": [...] }`; run states `queued`, `scheduled`, `running`, `succeeded`, `failed`,
//! `canceled`, `internal_error`. [`FakeJobs::fail_next`] fails the next call of a method on a
//! path once (spec 6.6).

use std::sync::Mutex;

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::http::{HttpClient, Request, Response};
use serde_json::{Map, Value, json};

use super::scaleway_fake::{
    Call, Failure, Failures, Url, answer, api_error, authorized, body_json, call_of,
    failure_status, fake_time, fake_uuid, paginate, reply, unauthorized,
};

const LIVE: [&str; 3] = ["queued", "scheduled", "running"];

struct Inner {
    definitions: Vec<Value>,
    runs: Vec<Value>,
    next: u64,
    calls: Vec<Call>,
    max_page_size: usize,
    failures: Failures,
}

pub struct FakeJobs {
    inner: Mutex<Inner>,
}

impl Default for FakeJobs {
    fn default() -> Self {
        FakeJobs::new()
    }
}

impl FakeJobs {
    pub fn new() -> FakeJobs {
        FakeJobs {
            inner: Mutex::new(Inner {
                definitions: Vec::new(),
                runs: Vec::new(),
                next: 0,
                calls: Vec::new(),
                max_page_size: 100,
                failures: Failures::default(),
            }),
        }
    }

    /// The next `method` call on `path` (a `*` segment stands for any one) fails once with
    /// `failure`.
    pub fn fail_next(&self, method: &str, path: &str, failure: Failure) {
        self.inner
            .lock()
            .expect("fake jobs")
            .failures
            .arm(method, path, failure);
    }

    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().expect("fake jobs").calls.clone()
    }

    pub fn definitions(&self) -> Vec<Value> {
        self.inner.lock().expect("fake jobs").definitions.clone()
    }

    pub fn runs(&self) -> Vec<Value> {
        self.inner.lock().expect("fake jobs").runs.clone()
    }

    /// Adds a definition named `name` in `project_id`, as another tool or an earlier version
    /// would have made it; answers its id.
    pub fn add_definition(&self, project_id: &str, name: &str) -> String {
        let mut inner = self.inner.lock().expect("fake jobs");
        inner.next += 1;
        let n = inner.next;
        let id = fake_uuid(5, n);
        inner.definitions.push(json!({
            "id": id, "name": name, "project_id": project_id,
            "region": super::scaleway_fake::FAKE_REGION,
            "cpu_limit": 1000, "memory_limit": 2048, "local_storage_capacity": 1024,
            "image_uri": "example.invalid/x", "command": "", "description": "",
            "environment_variables": {}, "job_timeout": null, "cron_schedule": null,
            "created_at": fake_time(n), "updated_at": fake_time(n),
        }));
        id
    }

    /// Replaces the environment of the definition `id`, as an earlier version would have set it.
    pub fn set_environment(&self, id: &str, environment: Value) {
        let mut inner = self.inner.lock().expect("fake jobs");
        let definition = inner
            .definitions
            .iter_mut()
            .find(|d| d["id"] == id)
            .unwrap_or_else(|| panic!("definition {id}"));
        definition["environment_variables"] = environment;
    }

    /// Caps `page_size` so that tests can force several pages.
    pub fn set_max_page_size(&self, size: usize) {
        self.inner.lock().expect("fake jobs").max_page_size = size.max(1);
    }

    pub fn set_run_state(&self, run_id: &str, state: &str) {
        let mut inner = self.inner.lock().expect("fake jobs");
        if let Some(run) = inner.runs.iter_mut().find(|r| r["id"] == run_id) {
            run["state"] = json!(state);
        }
    }
}

fn string_map(value: &Value) -> Option<Map<String, Value>> {
    match value {
        Value::Null => Some(Map::new()),
        Value::Object(map) if map.values().all(Value::is_string) => Some(map.clone()),
        _ => None,
    }
}

fn valid_duration(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text
            .strip_suffix('s')
            .is_some_and(|n| n.parse::<f64>().is_ok_and(|n| n >= 0.0)),
        _ => false,
    }
}

/// The longest job definition name the fake takes. Scaleway documents lowercase letters,
/// digits and dashes and no length; the fake holds names to the DNS label length that
/// `ciburst_core::config::MAX_DEFINITION_NAME` keeps them within.
pub const MAX_NAME: usize = 63;

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Checks and copies the writable definition fields of `body` into `definition`.
fn write_fields(
    definition: &mut Value,
    body: &Value,
    create: bool,
) -> std::result::Result<(), String> {
    let required = |field: &str| create || !body[field].is_null();
    for field in ["name", "image_uri"] {
        if required(field) && body[field].as_str().is_none_or(str::is_empty) {
            return Err(format!("{field} required"));
        }
    }
    if let Some(name) = body["name"].as_str()
        && !valid_name(name)
    {
        return Err(format!(
            "name {name}: lowercase letters, digits and dashes, at most {MAX_NAME} characters"
        ));
    }
    if create && body["project_id"].as_str().is_none_or(str::is_empty) {
        return Err("project_id required".into());
    }
    for field in ["cpu_limit", "memory_limit"] {
        if required(field) && body[field].as_u64().is_none_or(|n| n == 0) {
            return Err(format!("{field} positive integer"));
        }
    }
    if !body["local_storage_capacity"].is_null()
        && body["local_storage_capacity"].as_u64().is_none()
    {
        return Err("local_storage_capacity integer".into());
    }
    if !valid_duration(&body["job_timeout"]) {
        return Err("job_timeout duration".into());
    }
    let Some(env) = string_map(&body["environment_variables"]) else {
        return Err("environment_variables map of strings".into());
    };
    for field in [
        "name",
        "cpu_limit",
        "memory_limit",
        "local_storage_capacity",
        "image_uri",
        "command",
        "description",
        "job_timeout",
    ] {
        if !body[field].is_null() {
            definition[field] = body[field].clone();
        }
    }
    if create || !body["environment_variables"].is_null() {
        definition["environment_variables"] = Value::Object(env);
    }
    Ok(())
}

#[async_trait]
impl HttpClient for FakeJobs {
    async fn send(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        let failure = {
            let mut inner = self.inner.lock().expect("fake jobs");
            inner.calls.push(call_of(&request, &url));
            inner.failures.take(&request.method, &url.path)
        };
        let target = request.url.clone();
        answer(failure, &target, failure_status, self.respond(request)).await
    }
}

impl FakeJobs {
    async fn respond(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        let mut inner = self.inner.lock().expect("fake jobs");
        if !authorized(&request) {
            return Ok(unauthorized());
        }
        let body = body_json(&request);
        let segments = url.segments();
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        let ["serverless-jobs", "v1alpha1", "regions", region, rest @ ..] = segments.as_slice()
        else {
            return Ok(api_error(404, "not_found", "no route"));
        };
        let region = region.to_string();
        let max = inner.max_page_size;
        match (request.method.as_str(), rest) {
            ("GET", ["job-definitions"]) => {
                let items = inner
                    .definitions
                    .iter()
                    .filter(|d| d["region"] == region.as_str())
                    .filter(|d| url.get("project_id").is_none_or(|p| d["project_id"] == p))
                    .cloned()
                    .collect();
                let (page, total) = paginate(items, &url, max);
                Ok(reply(
                    200,
                    json!({ "job_definitions": page, "total_count": total }),
                ))
            }
            ("POST", ["job-definitions"]) => {
                let duplicate = inner
                    .definitions
                    .iter()
                    .any(|d| d["project_id"] == body["project_id"] && d["name"] == body["name"]);
                if duplicate {
                    return Ok(api_error(409, "conflict", "job definition name exists"));
                }
                inner.next += 1;
                let n = inner.next;
                let mut definition = json!({
                    "id": fake_uuid(5, n),
                    "created_at": fake_time(n),
                    "updated_at": fake_time(n),
                    "command": "",
                    "description": "",
                    "local_storage_capacity": 1024,
                    "job_timeout": null,
                    "cron_schedule": null,
                    "project_id": body["project_id"].clone(),
                    "region": region,
                });
                if let Err(message) = write_fields(&mut definition, &body, true) {
                    return Ok(api_error(400, "invalid_arguments", &message));
                }
                inner.definitions.push(definition.clone());
                Ok(reply(200, definition))
            }
            ("GET" | "PATCH" | "DELETE", ["job-definitions", id]) => {
                let Some(index) = inner.definitions.iter().position(|d| d["id"] == *id) else {
                    return Ok(api_error(404, "not_found", "job definition not found"));
                };
                match request.method.as_str() {
                    "GET" => Ok(reply(200, inner.definitions[index].clone())),
                    "DELETE" => {
                        inner.definitions.remove(index);
                        Ok(Response::empty(204))
                    }
                    _ => {
                        let mut definition = inner.definitions[index].clone();
                        if let Err(message) = write_fields(&mut definition, &body, false) {
                            return Ok(api_error(400, "invalid_arguments", &message));
                        }
                        inner.next += 1;
                        definition["updated_at"] = json!(fake_time(inner.next));
                        inner.definitions[index] = definition.clone();
                        Ok(reply(200, definition))
                    }
                }
            }
            ("POST", ["job-definitions", id, "start"]) => {
                let Some(definition) = inner.definitions.iter().find(|d| d["id"] == *id).cloned()
                else {
                    return Ok(api_error(404, "not_found", "job definition not found"));
                };
                let Some(overrides) = string_map(&body["environment_variables"]) else {
                    return Ok(api_error(
                        400,
                        "invalid_arguments",
                        "environment_variables map of strings",
                    ));
                };
                let replicas = body["replicas"].as_u64().unwrap_or(1).max(1);
                let mut env = definition["environment_variables"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                env.extend(overrides);
                let mut started = Vec::new();
                for _ in 0..replicas {
                    inner.next += 1;
                    let n = inner.next;
                    let run = json!({
                        "id": fake_uuid(6, n),
                        "job_definition_id": definition["id"].clone(),
                        "state": "queued",
                        "created_at": fake_time(n),
                        "updated_at": fake_time(n),
                        "started_at": null,
                        "terminated_at": null,
                        "exit_code": null,
                        "run_duration": null,
                        "error_message": "",
                        "cpu_limit": definition["cpu_limit"].clone(),
                        "memory_limit": definition["memory_limit"].clone(),
                        "local_storage_capacity": definition["local_storage_capacity"].clone(),
                        "command": definition["command"].clone(),
                        "environment_variables": Value::Object(env.clone()),
                        "region": region,
                        "project_id": definition["project_id"].clone(),
                    });
                    inner.runs.push(run.clone());
                    started.push(run);
                }
                Ok(reply(200, json!({ "job_runs": started })))
            }
            ("GET", ["job-runs"]) => {
                let items = inner
                    .runs
                    .iter()
                    .filter(|r| r["region"] == region.as_str())
                    .filter(|r| {
                        url.get("job_definition_id")
                            .is_none_or(|d| r["job_definition_id"] == d)
                    })
                    .filter(|r| url.get("project_id").is_none_or(|p| r["project_id"] == p))
                    .cloned()
                    .collect();
                let (page, total) = paginate(items, &url, max);
                Ok(reply(
                    200,
                    json!({ "job_runs": page, "total_count": total }),
                ))
            }
            ("GET", ["job-runs", id]) => match inner.runs.iter().find(|r| r["id"] == *id) {
                Some(run) => Ok(reply(200, run.clone())),
                None => Ok(api_error(404, "not_found", "job run not found")),
            },
            ("POST", ["job-runs", id, "stop"]) => {
                inner.next += 1;
                let now = fake_time(inner.next);
                let Some(run) = inner.runs.iter_mut().find(|r| r["id"] == *id) else {
                    return Ok(api_error(404, "not_found", "job run not found"));
                };
                if LIVE.contains(&run["state"].as_str().unwrap_or_default()) {
                    run["state"] = json!("canceled");
                    run["terminated_at"] = json!(now);
                }
                Ok(reply(200, run.clone()))
            }
            _ => Ok(api_error(404, "not_found", "no route")),
        }
    }
}
