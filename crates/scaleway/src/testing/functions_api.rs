//! Fake of the Serverless Functions API (`/functions/v1beta1/regions/<region>`): namespaces,
//! functions, upload URL and the presigned `PUT`, deploy, crons, status transitions. Shapes follow
//! `scaleway-sdk-go/api/function/v1beta1`. Also serves `GET /account/v3/projects` for projects
//! registered with [`FakeFunctions::add_project`], so host tests can run without a default
//! project id.
//!
//! Transitions: a namespace is `pending` after create and `ready` on the next `GET`; a function
//! is `created` until its first deploy, `pending` after a deploy (or an update of a deployed
//! function) and `ready` on the next `GET`, or `error` when [`FakeFunctions::fail_next_build`]
//! was called. A cron is `creating` after create, `pending` after an update and `ready` on the
//! next `GET`; deleting a function or namespace deletes its crons.
//!
//! `memory_limit` takes the sizes of [`MEMORY_LIMITS`], and `cpu_limit` follows it at 70 mvCPU
//! per 128 MB, as Scaleway allocates a function's CPU.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::http::{HttpClient, Request, Response};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub method: String,
    pub host: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
}

#[derive(Debug, Clone)]
struct FunctionRecord {
    value: Value,
    secrets: BTreeMap<String, String>,
    expected_length: Option<usize>,
    uploaded: Option<Vec<u8>>,
    deployed: Option<Vec<u8>>,
    deploys: usize,
    building: bool,
}

#[derive(Debug, Default)]
struct Inner {
    projects: Vec<(String, String)>,
    namespaces: Vec<Value>,
    functions: Vec<FunctionRecord>,
    crons: Vec<Value>,
    calls: Vec<Call>,
    next_id: u64,
    fail_next_build: bool,
}

/// In-memory Functions API behind the [`HttpClient`] seam.
#[derive(Default)]
pub struct FakeFunctions {
    inner: Mutex<Inner>,
}

const UPLOAD_HOST_PREFIX: &str = "s3.";
const UPLOAD_CONTENT_TYPE: &str = "application/octet-stream";

impl FakeFunctions {
    pub fn new() -> FakeFunctions {
        FakeFunctions {
            inner: Mutex::new(Inner {
                next_id: 1,
                ..Inner::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("fake functions")
    }

    /// Registers a project for `GET /account/v3/projects`.
    pub fn add_project(&self, id: &str, name: &str) {
        self.lock()
            .projects
            .push((id.to_string(), name.to_string()));
    }

    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    pub fn namespace_count(&self) -> usize {
        self.lock().namespaces.len()
    }

    pub fn function_count(&self) -> usize {
        self.lock().functions.len()
    }

    /// The function as the API returns it.
    pub fn function(&self, name: &str) -> Option<Value> {
        self.lock()
            .functions
            .iter()
            .find(|f| f.value["name"] == name)
            .map(|f| f.value.clone())
    }

    /// The plain value of a secret variable, which the real API never returns.
    pub fn secret(&self, function: &str, key: &str) -> Option<String> {
        self.lock()
            .functions
            .iter()
            .find(|f| f.value["name"] == function)
            .and_then(|f| f.secrets.get(key).cloned())
    }

    pub fn secret_keys(&self, function: &str) -> Vec<String> {
        self.lock()
            .functions
            .iter()
            .find(|f| f.value["name"] == function)
            .map(|f| f.secrets.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn deployed_code(&self, function: &str) -> Option<Vec<u8>> {
        self.lock()
            .functions
            .iter()
            .find(|f| f.value["name"] == function)
            .and_then(|f| f.deployed.clone())
    }

    pub fn deploy_count(&self, function: &str) -> usize {
        self.lock()
            .functions
            .iter()
            .find(|f| f.value["name"] == function)
            .map(|f| f.deploys)
            .unwrap_or(0)
    }

    /// Sets a plain environment variable, as a change made outside ciburst would.
    pub fn set_env(&self, function: &str, key: &str, value: &str) {
        let mut inner = self.lock();
        if let Some(f) = inner
            .functions
            .iter_mut()
            .find(|f| f.value["name"] == function)
        {
            f.value["environment_variables"][key] = json!(value);
        }
    }

    /// The next deploy ends in `error` with a build message.
    pub fn fail_next_build(&self) {
        self.lock().fail_next_build = true;
    }

    pub fn cron_count(&self) -> usize {
        self.lock().crons.len()
    }

    /// The cron as the API returns it.
    pub fn cron(&self, name: &str) -> Option<Value> {
        self.lock()
            .crons
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
    }

    /// Sets a cron field, as a change made outside ciburst would.
    pub fn set_cron(&self, name: &str, field: &str, value: Value) {
        if let Some(cron) = self.lock().crons.iter_mut().find(|c| c["name"] == name) {
            cron[field] = value;
        }
    }
}

fn reply(status: u16, value: Value) -> Response {
    Response::json_body(status, &value)
}

fn not_found(what: &str) -> Response {
    reply(
        404,
        json!({ "type": "not_found", "message": format!("{what} not found") }),
    )
}

fn invalid(message: &str) -> Response {
    reply(
        400,
        json!({ "type": "invalid_arguments", "message": message }),
    )
}

fn split_url(url: &str) -> (String, String, Vec<(String, String)>) {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let (host, rest) = rest.split_once('/').unwrap_or((rest, ""));
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (k.to_string(), decode(v))
        })
        .collect();
    (host.to_string(), format!("/{path}"), query)
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn param<'a>(query: &'a [(String, String)], key: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn hashed(secrets: &BTreeMap<String, String>) -> Value {
    Value::Array(
        secrets
            .iter()
            .map(|(key, value)| {
                json!({ "key": key, "hashed_value": hex::encode(Sha256::digest(value.as_bytes())) })
            })
            .collect(),
    )
}

/// Applies `secret_environment_variables` of a request: a value sets, a key alone deletes.
fn apply_secrets(
    secrets: &mut BTreeMap<String, String>,
    body: &Value,
) -> std::result::Result<(), String> {
    for item in body["secret_environment_variables"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let key = item["key"]
            .as_str()
            .ok_or("secret_environment_variables: key required")?;
        match item.get("value") {
            Some(Value::String(value)) => {
                secrets.insert(key.to_string(), value.clone());
            }
            None | Some(Value::Null) => {
                secrets.remove(key);
            }
            Some(_) => return Err("secret_environment_variables: value must be a string".into()),
        }
    }
    Ok(())
}

/// Five whitespace-separated cron fields when a schedule is given; `args` a JSON object.
fn check_cron(body: &Value) -> std::result::Result<(), String> {
    if let Some(schedule) = body.get("schedule") {
        let fields = schedule.as_str().map(|s| s.split_whitespace().count());
        if fields != Some(5) {
            return Err("schedule: five cron fields".into());
        }
    }
    if body.get("args").is_some_and(|args| !args.is_object()) {
        return Err("args: JSON object".into());
    }
    Ok(())
}

/// The memory sizes of a function in MB.
pub const MEMORY_LIMITS: [u64; 7] = [128, 256, 512, 1024, 2048, 3072, 4096];

/// The `cpu_limit` in mvCPU of the body's `memory_limit`; `None` without one.
fn cpu_limit(body: &Value) -> std::result::Result<Option<u64>, String> {
    match body.get("memory_limit") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(mb) if MEMORY_LIMITS.contains(&mb) => Ok(Some(mb * 70 / 128)),
            _ => Err(format!(
                "memory_limit {value} is not one of {MEMORY_LIMITS:?}"
            )),
        },
    }
}

const FUNCTION_FIELDS: [&str; 11] = [
    "runtime",
    "handler",
    "privacy",
    "min_scale",
    "max_scale",
    "memory_limit",
    "timeout",
    "http_option",
    "sandbox",
    "description",
    "environment_variables",
];

impl Inner {
    fn id(&mut self, kind: &str) -> String {
        let id = self.next_id;
        self.next_id += 1;
        format!("00000000-0000-4000-8000-{kind}{id:08}")
    }

    fn route(
        &mut self,
        method: &str,
        region: &str,
        rest: &[&str],
        query: &[(String, String)],
        body: Value,
    ) -> Response {
        match (method, rest) {
            ("GET", ["namespaces"]) => {
                let list: Vec<Value> = self
                    .namespaces
                    .iter()
                    .filter(|n| param(query, "project_id").is_none_or(|p| n["project_id"] == p))
                    .filter(|n| param(query, "name").is_none_or(|p| n["name"] == p))
                    .cloned()
                    .collect();
                let total = list.len();
                reply(200, json!({ "namespaces": list, "total_count": total }))
            }
            ("POST", ["namespaces"]) => {
                let (Some(name), Some(project_id)) =
                    (body["name"].as_str(), body["project_id"].as_str())
                else {
                    return invalid("name and project_id required");
                };
                let id = self.id("0001");
                let namespace = json!({
                    "id": id, "name": name, "project_id": project_id,
                    "organization_id": "00000000-0000-4000-8000-000000000000",
                    "environment_variables": body["environment_variables"].as_object().cloned().unwrap_or_default(),
                    "secret_environment_variables": [],
                    "status": "pending", "error_message": null, "description": null,
                    "registry_namespace_id": "", "registry_endpoint": "",
                    "region": region, "tags": [],
                });
                self.namespaces.push(namespace.clone());
                reply(200, namespace)
            }
            ("GET", ["namespaces", id]) => {
                match self.namespaces.iter_mut().find(|n| n["id"] == *id) {
                    Some(namespace) => {
                        if namespace["status"] == "pending" {
                            namespace["status"] = json!("ready");
                        }
                        reply(200, namespace.clone())
                    }
                    None => not_found("namespace"),
                }
            }
            ("DELETE", ["namespaces", id]) => {
                let Some(index) = self.namespaces.iter().position(|n| n["id"] == *id) else {
                    return not_found("namespace");
                };
                let mut namespace = self.namespaces.remove(index);
                let functions: Vec<Value> = self
                    .functions
                    .iter()
                    .filter(|f| f.value["namespace_id"] == *id)
                    .map(|f| f.value["id"].clone())
                    .collect();
                self.functions.retain(|f| f.value["namespace_id"] != *id);
                self.crons
                    .retain(|c| !functions.contains(&c["function_id"]));
                namespace["status"] = json!("deleting");
                reply(200, namespace)
            }
            ("GET", ["functions"]) => {
                let list: Vec<Value> = self
                    .functions
                    .iter()
                    .map(|f| f.value.clone())
                    .filter(|f| param(query, "namespace_id").is_none_or(|p| f["namespace_id"] == p))
                    .filter(|f| param(query, "name").is_none_or(|p| f["name"] == p))
                    .collect();
                let total = list.len();
                reply(200, json!({ "functions": list, "total_count": total }))
            }
            ("POST", ["functions"]) => self.create_function(region, body),
            ("GET", ["functions", id]) => {
                let fail = self.fail_next_build;
                let Some(record) = self.functions.iter_mut().find(|f| f.value["id"] == *id) else {
                    return not_found("function");
                };
                let mut failed = false;
                if record.value["status"] == "pending" {
                    if fail && record.building {
                        record.value["status"] = json!("error");
                        record.value["build_message"] = json!("error: could not compile");
                        failed = true;
                    } else {
                        record.value["status"] = json!("ready");
                        record.value["build_message"] = Value::Null;
                    }
                    record.building = false;
                }
                let value = record.value.clone();
                if failed {
                    self.fail_next_build = false;
                }
                reply(200, value)
            }
            ("PATCH", ["functions", id]) => {
                let Some(record) = self.functions.iter_mut().find(|f| f.value["id"] == *id) else {
                    return not_found("function");
                };
                if record.value["status"] == "pending" {
                    return reply(
                        409,
                        json!({ "type": "transient_state", "message": "function is in a transient state" }),
                    );
                }
                let cpu = match cpu_limit(&body) {
                    Ok(cpu) => cpu,
                    Err(message) => return invalid(&message),
                };
                if let Err(message) = apply_secrets(&mut record.secrets, &body) {
                    return invalid(&message);
                }
                for field in FUNCTION_FIELDS {
                    if let Some(value) = body.get(field) {
                        record.value[field] = value.clone();
                    }
                }
                if let Some(cpu) = cpu {
                    record.value["cpu_limit"] = json!(cpu);
                }
                record.value["secret_environment_variables"] = hashed(&record.secrets);
                if record.deployed.is_some() {
                    record.value["status"] = json!("pending");
                }
                reply(200, record.value.clone())
            }
            ("DELETE", ["functions", id]) => {
                let Some(index) = self.functions.iter().position(|f| f.value["id"] == *id) else {
                    return not_found("function");
                };
                let mut record = self.functions.remove(index);
                self.crons.retain(|c| c["function_id"] != *id);
                record.value["status"] = json!("deleting");
                reply(200, record.value)
            }
            ("GET", ["crons"]) => {
                let Some(function_id) = param(query, "function_id").filter(|f| !f.is_empty())
                else {
                    return invalid("function_id required");
                };
                let list: Vec<Value> = self
                    .crons
                    .iter()
                    .filter(|c| c["function_id"] == function_id)
                    .cloned()
                    .collect();
                let total = list.len();
                reply(200, json!({ "crons": list, "total_count": total }))
            }
            ("POST", ["crons"]) => self.create_cron(body),
            ("GET", ["crons", id]) => match self.crons.iter_mut().find(|c| c["id"] == *id) {
                Some(cron) => {
                    if cron["status"] == "creating" || cron["status"] == "pending" {
                        cron["status"] = json!("ready");
                    }
                    reply(200, cron.clone())
                }
                None => not_found("cron"),
            },
            ("PATCH", ["crons", id]) => {
                if let Err(message) = check_cron(&body) {
                    return invalid(&message);
                }
                let Some(cron) = self.crons.iter_mut().find(|c| c["id"] == *id) else {
                    return not_found("cron");
                };
                for field in ["function_id", "schedule", "args", "name"] {
                    if let Some(value) = body.get(field) {
                        cron[field] = value.clone();
                    }
                }
                cron["status"] = json!("pending");
                reply(200, cron.clone())
            }
            ("DELETE", ["crons", id]) => {
                let Some(index) = self.crons.iter().position(|c| c["id"] == *id) else {
                    return not_found("cron");
                };
                let mut cron = self.crons.remove(index);
                cron["status"] = json!("deleting");
                reply(200, cron)
            }
            ("GET", ["functions", id, "upload-url"]) => {
                let Some(length) = param(query, "content_length").and_then(|l| l.parse().ok())
                else {
                    return invalid("content_length required");
                };
                let Some(record) = self.functions.iter_mut().find(|f| f.value["id"] == *id) else {
                    return not_found("function");
                };
                record.expected_length = Some(length);
                reply(
                    200,
                    json!({
                        "url": format!("https://{UPLOAD_HOST_PREFIX}{region}.scw.cloud/fake-functions/{id}.zip?X-Amz-Signature=fake"),
                        "headers": {
                            "Content-Type": [UPLOAD_CONTENT_TYPE],
                            "Content-Length": [length.to_string()],
                        },
                    }),
                )
            }
            ("POST", ["functions", id, "deploy"]) => {
                let Some(record) = self.functions.iter_mut().find(|f| f.value["id"] == *id) else {
                    return not_found("function");
                };
                if record.value["status"] == "pending" {
                    return reply(
                        409,
                        json!({ "type": "transient_state", "message": "function is in a transient state" }),
                    );
                }
                let Some(code) = record.uploaded.clone() else {
                    return reply(
                        412,
                        json!({ "type": "precondition_failed", "message": "no code uploaded" }),
                    );
                };
                record.deployed = Some(code);
                record.deploys += 1;
                record.building = true;
                record.value["status"] = json!("pending");
                reply(200, record.value.clone())
            }
            _ => reply(
                404,
                json!({ "message": format!("no route for {method} {}", rest.join("/")) }),
            ),
        }
    }

    fn create_function(&mut self, region: &str, body: Value) -> Response {
        let (Some(name), Some(namespace_id)) =
            (body["name"].as_str(), body["namespace_id"].as_str())
        else {
            return invalid("name and namespace_id required");
        };
        let Some(namespace) = self.namespaces.iter().find(|n| n["id"] == namespace_id) else {
            return not_found("namespace");
        };
        if namespace["status"] != "ready" {
            return reply(
                412,
                json!({ "type": "precondition_failed", "message": "namespace not ready" }),
            );
        }
        let namespace_name = namespace["name"].as_str().unwrap_or("").to_string();
        if self
            .functions
            .iter()
            .any(|f| f.value["namespace_id"] == namespace_id && f.value["name"] == name)
        {
            return reply(
                409,
                json!({ "type": "conflict", "message": "function name already used" }),
            );
        }
        if body["runtime"].as_str().unwrap_or("").is_empty() {
            return invalid("runtime required");
        }
        let cpu = match cpu_limit(&body) {
            Ok(cpu) => cpu.unwrap_or(70),
            Err(message) => return invalid(&message),
        };
        let mut secrets = BTreeMap::new();
        if let Err(message) = apply_secrets(&mut secrets, &body) {
            return invalid(&message);
        }
        let id = self.id("0002");
        let mut value = Value::Object(Map::new());
        for field in FUNCTION_FIELDS {
            value[field] = body.get(field).cloned().unwrap_or(Value::Null);
        }
        if value["environment_variables"].is_null() {
            value["environment_variables"] = json!({});
        }
        value["id"] = json!(id);
        value["name"] = json!(name);
        value["namespace_id"] = json!(namespace_id);
        value["status"] = json!("created");
        value["cpu_limit"] = json!(cpu);
        value["error_message"] = Value::Null;
        value["build_message"] = Value::Null;
        value["domain_name"] = json!(format!(
            "{namespace_name}fake{:04}-{name}.functions.fnc.{region}.scw.cloud",
            self.next_id
        ));
        value["region"] = json!(region);
        value["secret_environment_variables"] = hashed(&secrets);
        value["tags"] = json!([]);
        self.functions.push(FunctionRecord {
            value: value.clone(),
            secrets,
            expected_length: None,
            uploaded: None,
            deployed: None,
            deploys: 0,
            building: false,
        });
        reply(200, value)
    }

    /// `CreateCronRequest`: `function_id` and `schedule` required, `args` and `name` optional.
    fn create_cron(&mut self, body: Value) -> Response {
        let Some(function_id) = body["function_id"].as_str() else {
            return invalid("function_id required");
        };
        if body["schedule"].as_str().is_none() {
            return invalid("schedule required");
        }
        if let Err(message) = check_cron(&body) {
            return invalid(&message);
        }
        if !self.functions.iter().any(|f| f.value["id"] == function_id) {
            return not_found("function");
        }
        let id = self.id("0003");
        let name = body["name"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| format!("cron-{id}"));
        let cron = json!({
            "id": id,
            "function_id": function_id,
            "schedule": body["schedule"],
            "args": body.get("args").cloned().unwrap_or_else(|| json!({})),
            "status": "creating",
            "name": name,
        });
        self.crons.push(cron.clone());
        reply(200, cron)
    }

    fn upload(&mut self, path: &str, request: &Request) -> Response {
        if request.method != "PUT" {
            return reply(405, json!({ "message": "method not allowed" }));
        }
        if request.header_value("X-Auth-Token").is_some() {
            return reply(
                400,
                json!({ "message": "the presigned upload takes no API token" }),
            );
        }
        if request.header_value("Content-Type") != Some(UPLOAD_CONTENT_TYPE) {
            return reply(403, json!({ "message": "signature does not match" }));
        }
        let Some(id) = path
            .strip_prefix("/fake-functions/")
            .and_then(|p| p.strip_suffix(".zip"))
        else {
            return reply(404, json!({ "message": "no such key" }));
        };
        let Some(record) = self.functions.iter_mut().find(|f| f.value["id"] == id) else {
            return reply(404, json!({ "message": "no such key" }));
        };
        let body = request.body.clone().unwrap_or_default();
        if record.expected_length != Some(body.len()) {
            return reply(403, json!({ "message": "content length does not match" }));
        }
        record.uploaded = Some(body);
        Response::empty(200)
    }
}

#[async_trait]
impl HttpClient for FakeFunctions {
    async fn send(&self, request: Request) -> Result<Response> {
        let mut inner = self.lock();
        let (host, path, query) = split_url(&request.url);
        let body: Value = request
            .body
            .as_ref()
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or(Value::Null);
        inner.calls.push(Call {
            method: request.method.clone(),
            host: host.clone(),
            path: path.clone(),
            query: query.clone(),
            body: (!body.is_null()).then(|| body.clone()),
        });
        if host.starts_with(UPLOAD_HOST_PREFIX) {
            return Ok(inner.upload(&path, &request));
        }
        if request
            .header_value("X-Auth-Token")
            .is_none_or(|token| token.is_empty())
        {
            return Ok(reply(
                401,
                json!({ "type": "denied_authentication", "message": "authentication is denied" }),
            ));
        }
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        Ok(match segments.as_slice() {
            ["account", "v3", "projects"] => {
                let list: Vec<Value> = inner
                    .projects
                    .iter()
                    .filter(|(_, name)| param(&query, "name").is_none_or(|n| n == name))
                    .map(|(id, name)| json!({ "id": id, "name": name }))
                    .collect();
                let total = list.len();
                reply(200, json!({ "projects": list, "total_count": total }))
            }
            ["functions", "v1beta1", "regions", region, rest @ ..] => {
                let region = region.to_string();
                inner.route(&request.method, &region, rest, &query, body)
            }
            _ => reply(404, json!({ "message": format!("no route for {path}") })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_urls_and_decodes_queries() {
        let (host, path, query) = split_url(
            "https://api.scaleway.com/functions/v1beta1/regions/fr-par/namespaces?name=a%20b&project_id=p",
        );
        assert_eq!(host, "api.scaleway.com");
        assert_eq!(path, "/functions/v1beta1/regions/fr-par/namespaces");
        assert_eq!(
            query,
            vec![
                ("name".to_string(), "a b".to_string()),
                ("project_id".to_string(), "p".to_string())
            ]
        );
        assert_eq!(decode("%2"), "%2");
    }
}
