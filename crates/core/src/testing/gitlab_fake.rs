use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::Result;
use crate::check::LintJob;
use crate::http::{Headers, HttpClient, Request, Response};

pub const FAKE_TOKEN: &str = "glpat-test";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub method: String,
    pub path: String,
    /// The raw query string, without `?`.
    pub query: String,
    pub body: Option<Value>,
}

#[derive(Debug, Clone)]
struct RunnerRecord {
    id: u64,
    description: String,
    tag_list: Vec<String>,
    run_untagged: bool,
    projects: Vec<u64>,
}

impl RunnerRecord {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "description": self.description,
            "tag_list": self.tag_list,
            "run_untagged": self.run_untagged,
            "runner_type": "project_type",
            "projects": self.projects.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone)]
struct HookRecord {
    id: u64,
    project_id: u64,
    url: String,
    token: String,
    job_events: bool,
    push_events: bool,
    alert_status: String,
    disabled_until: Option<String>,
    /// Deliveries, oldest first.
    events: Vec<Value>,
}

impl HookRecord {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "url": self.url,
            "project_id": self.project_id,
            "job_events": self.job_events,
            "push_events": self.push_events,
            "enable_ssl_verification": true,
            "alert_status": self.alert_status,
            "disabled_until": self.disabled_until,
        })
    }
}

/// The `trigger` values of `POST /projects/:id/hooks/:hook_id/test/:trigger`.
const HOOK_TRIGGERS: [&str; 12] = [
    "push_events",
    "tag_push_events",
    "issues_events",
    "confidential_issues_events",
    "note_events",
    "merge_requests_events",
    "job_events",
    "pipeline_events",
    "wiki_page_events",
    "releases_events",
    "emoji_events",
    "resource_access_token_events",
];

/// A project variable as the fake stores it, the value of a hidden one included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeVariable {
    pub key: String,
    pub value: String,
    pub environment_scope: String,
    pub protected: bool,
    pub masked: bool,
    pub hidden: bool,
    pub raw: bool,
}

impl FakeVariable {
    /// A plain variable in scope `*`.
    pub fn plain(key: &str, value: &str) -> FakeVariable {
        FakeVariable {
            key: key.to_string(),
            value: value.to_string(),
            environment_scope: "*".to_string(),
            protected: false,
            masked: false,
            hidden: false,
            raw: false,
        }
    }

    fn to_json(&self) -> Value {
        let value = (!self.hidden).then_some(self.value.as_str());
        json!({
            "key": self.key,
            "value": value,
            "variable_type": "env_var",
            "environment_scope": self.environment_scope,
            "protected": self.protected,
            "masked": self.masked,
            "hidden": self.hidden,
            "raw": self.raw,
            "description": null,
        })
    }
}

/// A CI job of the fake's pipelines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeJob {
    pub id: u64,
    pub name: String,
    pub stage: String,
    pub status: String,
    pub tag_list: Vec<String>,
    /// RFC 3339, as GitLab writes it.
    pub created_at: String,
    pub pipeline_id: u64,
}

impl FakeJob {
    pub fn pending(id: u64, name: &str, stage: &str, tags: &[&str], created_at: &str) -> FakeJob {
        FakeJob {
            id,
            name: name.to_string(),
            stage: stage.to_string(),
            status: "pending".to_string(),
            tag_list: tags.iter().map(|t| t.to_string()).collect(),
            created_at: created_at.to_string(),
            pipeline_id: 900 + id,
        }
    }

    fn to_json(&self, project_id: u64) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "stage": self.stage,
            "status": self.status,
            "tag_list": self.tag_list,
            "created_at": self.created_at,
            "pipeline": { "id": self.pipeline_id, "project_id": project_id, "status": "pending" },
        })
    }
}

#[derive(Debug, Default)]
struct Inner {
    runners: Vec<RunnerRecord>,
    hooks: Vec<HookRecord>,
    next_id: u64,
    calls: Vec<Call>,
    jobs: BTreeMap<u64, Vec<LintJob>>,
    ref_jobs: BTreeMap<(u64, String), Vec<LintJob>>,
    lint_errors: BTreeMap<u64, Vec<String>>,
    merged_yaml: BTreeMap<u64, String>,
    variables: BTreeMap<u64, Vec<FakeVariable>>,
    pipeline_jobs: BTreeMap<u64, Vec<FakeJob>>,
    runner_creation_forbidden: bool,
    /// Test deliveries fail, as when the dispatcher does not answer 2xx.
    hook_tests_fail: bool,
}

/// In-memory GitLab with the endpoints ciburst uses, behind the [`HttpClient`] seam. It is as
/// stingy as GitLab where ciburst depends on it: lists are paginated, a hidden variable's value
/// never leaves it, a scoped variable needs `filter[environment_scope]`.
#[derive(Default)]
pub struct FakeGitlab {
    inner: Mutex<Inner>,
}

impl FakeGitlab {
    pub fn new() -> FakeGitlab {
        FakeGitlab {
            inner: Mutex::new(Inner {
                next_id: 100,
                ..Inner::default()
            }),
        }
    }

    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().expect("fake gitlab").calls.clone()
    }

    pub fn runner_count(&self) -> usize {
        self.inner.lock().expect("fake gitlab").runners.len()
    }

    /// Runner ids with the projects they are enabled in.
    pub fn runner_projects(&self) -> Vec<(u64, Vec<u64>)> {
        self.inner
            .lock()
            .expect("fake gitlab")
            .runners
            .iter()
            .map(|r| (r.id, r.projects.clone()))
            .collect()
    }

    pub fn hooks(&self) -> Vec<(u64, u64, String)> {
        self.inner
            .lock()
            .expect("fake gitlab")
            .hooks
            .iter()
            .map(|h| (h.id, h.project_id, h.url.clone()))
            .collect()
    }

    pub fn hook_token(&self, hook_id: u64) -> Option<String> {
        self.inner
            .lock()
            .expect("fake gitlab")
            .hooks
            .iter()
            .find(|h| h.id == hook_id)
            .map(|h| h.token.clone())
    }

    pub fn hook_flags(&self, hook_id: u64) -> Option<(bool, bool)> {
        self.inner
            .lock()
            .expect("fake gitlab")
            .hooks
            .iter()
            .find(|h| h.id == hook_id)
            .map(|h| (h.job_events, h.push_events))
    }

    pub fn set_jobs(&self, project_id: u64, jobs: Vec<LintJob>) {
        self.inner
            .lock()
            .expect("fake gitlab")
            .jobs
            .insert(project_id, jobs);
    }

    /// Jobs of the pipeline at `content_ref`; a ref without jobs set does not exist.
    pub fn set_jobs_at(&self, project_id: u64, content_ref: &str, jobs: Vec<LintJob>) {
        self.inner
            .lock()
            .expect("fake gitlab")
            .ref_jobs
            .insert((project_id, content_ref.to_string()), jobs);
    }

    /// The merged YAML that CI lint answers with `include_merged_yaml=true` for a valid
    /// pipeline of the project, at any ref.
    pub fn set_merged_yaml(&self, project_id: u64, yaml: &str) {
        self.inner
            .lock()
            .expect("fake gitlab")
            .merged_yaml
            .insert(project_id, yaml.to_string());
    }

    /// The tags of a runner, `None` for an unknown one.
    pub fn runner_tags(&self, runner_id: u64) -> Option<Vec<String>> {
        self.inner
            .lock()
            .expect("fake gitlab")
            .runners
            .iter()
            .find(|r| r.id == runner_id)
            .map(|r| r.tag_list.clone())
    }

    pub fn set_lint_errors(&self, project_id: u64, errors: Vec<String>) {
        self.inner
            .lock()
            .expect("fake gitlab")
            .lint_errors
            .insert(project_id, errors);
    }

    /// The project's variables with their stored values, hidden ones included.
    pub fn variables(&self, project_id: u64) -> Vec<FakeVariable> {
        self.inner
            .lock()
            .expect("fake gitlab")
            .variables
            .get(&project_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Adds a variable as a user would in the UI, without the API's checks.
    pub fn add_variable(&self, project_id: u64, variable: FakeVariable) {
        self.inner
            .lock()
            .expect("fake gitlab")
            .variables
            .entry(project_id)
            .or_default()
            .push(variable);
    }

    pub fn add_job(&self, project_id: u64, job: FakeJob) {
        self.inner
            .lock()
            .expect("fake gitlab")
            .pipeline_jobs
            .entry(project_id)
            .or_default()
            .push(job);
    }

    /// `alert_status` and `disabled_until` of a hook.
    pub fn hook_alert(&self, hook_id: u64) -> Option<(String, Option<String>)> {
        self.inner
            .lock()
            .expect("fake gitlab")
            .hooks
            .iter()
            .find(|h| h.id == hook_id)
            .map(|h| (h.alert_status.clone(), h.disabled_until.clone()))
    }

    /// Disables a hook as GitLab does after failed deliveries: `temporarily_disabled` with
    /// the time it ends, or `disabled` without one.
    pub fn disable_hook(&self, hook_id: u64, alert_status: &str, disabled_until: Option<&str>) {
        let mut inner = self.inner.lock().expect("fake gitlab");
        let hook = inner
            .hooks
            .iter_mut()
            .find(|h| h.id == hook_id)
            .expect("hook to disable");
        hook.alert_status = alert_status.to_string();
        hook.disabled_until = disabled_until.map(String::from);
    }

    /// Test deliveries fail from now on and leave the hook as it is.
    pub fn fail_hook_tests(&self) {
        self.inner.lock().expect("fake gitlab").hook_tests_fail = true;
    }

    /// Records a delivery of a hook; the events list shows it first.
    pub fn add_hook_event(&self, hook_id: u64, event: Value) {
        let mut inner = self.inner.lock().expect("fake gitlab");
        let hook = inner
            .hooks
            .iter_mut()
            .find(|h| h.id == hook_id)
            .expect("hook of the event");
        hook.events.push(event);
    }

    /// `POST /user/runners` answers 403, as for a token without `create_runner`.
    pub fn forbid_runner_creation(&self) {
        self.inner
            .lock()
            .expect("fake gitlab")
            .runner_creation_forbidden = true;
    }
}

/// The values of a query parameter of `url`, as GitLab's list filters read them.
fn query_values(url: &str, key: &str) -> Vec<String> {
    let Some((_, query)) = url.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
        .collect()
}

fn query_value(url: &str, key: &str) -> Option<String> {
    query_values(url, key).into_iter().next()
}

fn json(status: u16, value: Value) -> Response {
    Response::json_body(status, &value)
}

fn segments(path: &str) -> Vec<&str> {
    path.trim_matches('/').split('/').collect()
}

/// GitLab's offset pagination: `page` from 1, `per_page` 20 unless given and at most 100, and
/// the headers `x-page`, `x-per-page`, `x-total` and `x-next-page`, empty on the last page.
fn paginate(url: &str, items: Vec<Value>) -> (Response, Headers) {
    let number = |key: &str| query_value(url, key).and_then(|v| v.parse::<usize>().ok());
    let per_page = number("per_page").unwrap_or(20).clamp(1, 100);
    let page = number("page").unwrap_or(1).max(1);
    let total = items.len();
    let start = (page - 1).saturating_mul(per_page).min(total);
    let end = start.saturating_add(per_page).min(total);
    let next = if end < total {
        (page + 1).to_string()
    } else {
        String::new()
    };
    let headers = vec![
        ("x-page".to_string(), page.to_string()),
        ("x-per-page".to_string(), per_page.to_string()),
        ("x-total".to_string(), total.to_string()),
        ("x-next-page".to_string(), next),
    ];
    (json(200, Value::Array(items[start..end].to_vec())), headers)
}

/// The fake's assumption of GitLab's masking rule: at least 8 characters, each of
/// `[A-Za-z0-9@:._~+/=-]`. GitLab answers 400 to a masked variable with any other value.
pub fn maskable(value: &str) -> bool {
    value.len() >= 8
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@:._~+/=-".contains(c))
}

fn valid_key(key: &str) -> bool {
    (1..=255).contains(&key.len()) && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn bad_request(field: &str, message: String) -> Response {
    let mut errors = serde_json::Map::new();
    errors.insert(field.to_string(), json!([message]));
    json(400, json!({ "message": errors }))
}

fn variable_not_found() -> Response {
    json(404, json!({ "message": "404 Variable Not Found" }))
}

fn runner_not_found() -> Response {
    json(404, json!({ "message": "404 Runner Not Found" }))
}

/// Parameters of `PUT /runners/:id`, at least one of which GitLab requires.
const RUNNER_UPDATE_KEYS: [&str; 9] = [
    "description",
    "active",
    "paused",
    "tag_list",
    "run_untagged",
    "locked",
    "access_level",
    "maximum_timeout",
    "maintenance_note",
];

/// `tag_list` as GitLab takes it: an array of strings or a comma-separated string, trimmed,
/// without empty or repeated tags.
fn tag_list(value: &Value) -> std::result::Result<Vec<String>, Response> {
    let given: Vec<String> = match value {
        Value::Null => Vec::new(),
        Value::String(text) => text.split(',').map(String::from).collect(),
        Value::Array(items) if items.iter().all(Value::is_string) => items
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => return Err(json(400, json!({ "error": "tag_list is invalid" }))),
    };
    let mut tags: Vec<String> = Vec::new();
    for tag in given.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
        if !tags.iter().any(|t| t == tag) {
            tags.push(tag.to_string());
        }
    }
    Ok(tags)
}

fn untagged_without_tags() -> Response {
    bad_request(
        "tags_list",
        "can not be empty when runner is not allowed to pick untagged jobs".to_string(),
    )
}

impl FakeGitlab {
    fn route(&self, request: &Request) -> (Response, Headers) {
        let mut inner = self.inner.lock().expect("fake gitlab");
        let after_base = request
            .url
            .find("/api/v4")
            .map(|i| &request.url[i + "/api/v4".len()..])
            .unwrap_or("");
        let (path, query) = after_base.split_once('?').unwrap_or((after_base, ""));
        let path = path.to_string();
        let body: Option<Value> = request
            .body
            .as_ref()
            .and_then(|b| serde_json::from_slice(b).ok());
        inner.calls.push(Call {
            method: request.method.clone(),
            path: path.clone(),
            query: query.to_string(),
            body: body.clone(),
        });
        if request.header_value("PRIVATE-TOKEN") != Some(FAKE_TOKEN) {
            return (
                json(401, json!({ "message": "401 Unauthorized" })),
                Headers::new(),
            );
        }
        let method = request.method.as_str();
        let seg = segments(&path);
        let id = |s: &str| s.parse::<u64>().ok();

        let response = match (method, seg.as_slice()) {
            ("GET", ["projects", p, "variables"]) => {
                let list = inner
                    .variables
                    .get(&id(p).unwrap_or(0))
                    .map(|vars| vars.iter().map(FakeVariable::to_json).collect())
                    .unwrap_or_default();
                return paginate(&request.url, list);
            }
            ("POST", ["projects", p, "variables"]) => {
                create_variable(&mut inner, id(p).unwrap_or(0), body.unwrap_or(Value::Null))
            }
            ("PUT" | "DELETE", ["projects", p, "variables", key]) => {
                let scope = query_value(&request.url, "filter[environment_scope]")
                    .unwrap_or_else(|| "*".to_string());
                let vars = inner.variables.entry(id(p).unwrap_or(0)).or_default();
                match vars
                    .iter()
                    .position(|v| v.key == *key && v.environment_scope == scope)
                {
                    None => variable_not_found(),
                    Some(index) if method == "DELETE" => {
                        vars.remove(index);
                        Response::empty(204)
                    }
                    Some(index) => update_variable(&mut vars[index], body.unwrap_or(Value::Null)),
                }
            }
            ("GET", ["projects", p, "jobs"]) => {
                let project = id(p).unwrap_or(0);
                let scopes = query_values(&request.url, "scope[]");
                let mut jobs: Vec<&FakeJob> = inner
                    .pipeline_jobs
                    .get(&project)
                    .map(|jobs| {
                        jobs.iter()
                            .filter(|job| scopes.is_empty() || scopes.contains(&job.status))
                            .collect()
                    })
                    .unwrap_or_default();
                jobs.sort_by_key(|job| std::cmp::Reverse(job.id));
                let list = jobs.iter().map(|job| job.to_json(project)).collect();
                return paginate(&request.url, list);
            }
            ("GET", ["projects", p, "runners"]) => {
                let project = id(p).unwrap_or(0);
                let wanted = query_value(&request.url, "tag_list");
                let list: Vec<Value> = inner
                    .runners
                    .iter()
                    .filter(|r| r.projects.contains(&project))
                    .filter(|r| wanted.as_ref().is_none_or(|t| r.tag_list.contains(t)))
                    .map(|r| json!({ "id": r.id, "description": r.description }))
                    .collect();
                json(200, Value::Array(list))
            }
            ("POST", ["projects", p, "runners"]) => {
                let project = id(p).unwrap_or(0);
                let runner_id = body
                    .as_ref()
                    .and_then(|b| b["runner_id"].as_u64())
                    .unwrap_or(0);
                match inner.runners.iter_mut().find(|r| r.id == runner_id) {
                    Some(runner) => {
                        if !runner.projects.contains(&project) {
                            runner.projects.push(project);
                        }
                        json(
                            201,
                            json!({ "id": runner.id, "description": runner.description }),
                        )
                    }
                    None => json(404, json!({ "message": "404 Runner Not Found" })),
                }
            }
            ("POST", ["user", "runners"]) if inner.runner_creation_forbidden => {
                json(403, json!({ "message": "403 Forbidden" }))
            }
            ("POST", ["user", "runners"]) => {
                let body = body.unwrap_or(Value::Null);
                let tag_list = match tag_list(&body["tag_list"]) {
                    Ok(tags) => tags,
                    Err(response) => return (response, Headers::new()),
                };
                let run_untagged = body["run_untagged"].as_bool().unwrap_or(true);
                if tag_list.is_empty() && !run_untagged {
                    return (untagged_without_tags(), Headers::new());
                }
                let runner_id = inner.next_id;
                inner.next_id += 1;
                inner.runners.push(RunnerRecord {
                    id: runner_id,
                    description: body["description"].as_str().unwrap_or("").to_string(),
                    tag_list,
                    run_untagged,
                    projects: vec![body["project_id"].as_u64().unwrap_or(0)],
                });
                json(
                    201,
                    json!({ "id": runner_id, "token": format!("glrt-fake-{runner_id}"), "token_expires_at": null }),
                )
            }
            ("GET", ["runners", r]) => match inner.runners.iter().find(|x| Some(x.id) == id(r)) {
                Some(runner) => json(200, runner.to_json()),
                None => runner_not_found(),
            },
            ("PUT", ["runners", r]) => {
                let body = body.unwrap_or(Value::Null);
                let Some(runner) = inner.runners.iter_mut().find(|x| Some(x.id) == id(r)) else {
                    return (runner_not_found(), Headers::new());
                };
                if !RUNNER_UPDATE_KEYS
                    .iter()
                    .any(|key| body.get(key).is_some_and(|v| !v.is_null()))
                {
                    return (
                        json(
                            400,
                            json!({ "error": format!("{} are missing, at least one parameter must be provided", RUNNER_UPDATE_KEYS.join(", ")) }),
                        ),
                        Headers::new(),
                    );
                }
                let mut tags = runner.tag_list.clone();
                if body.get("tag_list").is_some() {
                    match tag_list(&body["tag_list"]) {
                        Ok(given) => tags = given,
                        Err(response) => return (response, Headers::new()),
                    }
                }
                let run_untagged = body["run_untagged"]
                    .as_bool()
                    .unwrap_or(runner.run_untagged);
                if tags.is_empty() && !run_untagged {
                    return (untagged_without_tags(), Headers::new());
                }
                runner.tag_list = tags;
                runner.run_untagged = run_untagged;
                if let Some(description) = body["description"].as_str() {
                    runner.description = description.to_string();
                }
                json(200, runner.to_json())
            }
            ("DELETE", ["runners", r]) => {
                let runner_id = id(r).unwrap_or(0);
                let before = inner.runners.len();
                inner.runners.retain(|runner| runner.id != runner_id);
                if inner.runners.len() == before {
                    json(404, json!({ "message": "404 Not found" }))
                } else {
                    Response::empty(204)
                }
            }
            ("GET", ["projects", p, "hooks"]) => {
                let project = id(p).unwrap_or(0);
                let list: Vec<Value> = inner
                    .hooks
                    .iter()
                    .filter(|h| h.project_id == project)
                    .map(HookRecord::to_json)
                    .collect();
                json(200, Value::Array(list))
            }
            ("GET", ["projects", p, "hooks", h, "events"]) => {
                let (project, hook_id) = (id(p).unwrap_or(0), id(h).unwrap_or(0));
                match inner
                    .hooks
                    .iter()
                    .find(|hook| hook.project_id == project && hook.id == hook_id)
                {
                    None => json(404, json!({ "message": "404 Not found" })),
                    Some(hook) => {
                        let newest_first = hook.events.iter().rev().cloned().collect();
                        return paginate(&request.url, newest_first);
                    }
                }
            }
            ("POST", ["projects", p, "hooks", h, "test", trigger]) => {
                let (project, hook_id) = (id(p).unwrap_or(0), id(h).unwrap_or(0));
                let failing = inner.hook_tests_fail;
                match inner
                    .hooks
                    .iter_mut()
                    .find(|hook| hook.project_id == project && hook.id == hook_id)
                {
                    None => json(404, json!({ "message": "404 Not found" })),
                    Some(_) if !HOOK_TRIGGERS.contains(trigger) => json(
                        400,
                        json!({ "error": "trigger does not have a valid value" }),
                    ),
                    Some(_) if failing => json(
                        422,
                        json!({ "message": "Hook execution failed: Failed to open TCP connection" }),
                    ),
                    Some(hook) => {
                        hook.alert_status = "executable".to_string();
                        hook.disabled_until = None;
                        json(201, json!({ "message": "201 Created" }))
                    }
                }
            }
            ("POST", ["projects", p, "hooks"]) => {
                let body = body.unwrap_or(Value::Null);
                let hook_id = inner.next_id;
                inner.next_id += 1;
                let hook = HookRecord {
                    id: hook_id,
                    project_id: id(p).unwrap_or(0),
                    url: body["url"].as_str().unwrap_or("").to_string(),
                    token: body["token"].as_str().unwrap_or("").to_string(),
                    job_events: body["job_events"].as_bool().unwrap_or(false),
                    push_events: body["push_events"].as_bool().unwrap_or(true),
                    alert_status: "executable".to_string(),
                    disabled_until: None,
                    events: Vec::new(),
                };
                let created = hook.to_json();
                inner.hooks.push(hook);
                json(201, created)
            }
            ("PUT" | "DELETE", ["projects", p, "hooks", h]) => {
                let project = id(p).unwrap_or(0);
                let hook_id = id(h).unwrap_or(0);
                match inner
                    .hooks
                    .iter()
                    .position(|hook| hook.project_id == project && hook.id == hook_id)
                {
                    None => json(404, json!({ "message": "404 Not found" })),
                    Some(index) if method == "DELETE" => {
                        inner.hooks.remove(index);
                        Response::empty(204)
                    }
                    Some(index) => {
                        let body = body.unwrap_or(Value::Null);
                        let hook = &mut inner.hooks[index];
                        hook.url = body["url"].as_str().unwrap_or("").to_string();
                        hook.token = body["token"].as_str().unwrap_or("").to_string();
                        hook.job_events = body["job_events"].as_bool().unwrap_or(false);
                        hook.push_events = body["push_events"].as_bool().unwrap_or(true);
                        json(200, hook.to_json())
                    }
                }
            }
            ("GET", ["projects", p, "ci", "lint"]) => {
                let project = id(p).unwrap_or(0);
                let content_ref = query_value(&request.url, "content_ref");
                let at_ref = content_ref
                    .as_ref()
                    .map(|r| inner.ref_jobs.get(&(project, r.clone())).cloned());
                let errors = match &at_ref {
                    Some(None) => vec!["Reference not found".to_string()],
                    _ => inner.lint_errors.get(&project).cloned().unwrap_or_default(),
                };
                let valid = errors.is_empty();
                let jobs = match at_ref {
                    _ if !valid => Vec::new(),
                    Some(jobs) => jobs.unwrap_or_default(),
                    None => inner.jobs.get(&project).cloned().unwrap_or_default(),
                };
                let mut answer =
                    json!({ "valid": valid, "errors": errors, "warnings": [], "jobs": jobs });
                if let Some(yaml) = inner.merged_yaml.get(&project).filter(|_| {
                    valid
                        && query_value(&request.url, "include_merged_yaml").as_deref()
                            == Some("true")
                }) {
                    answer["merged_yaml"] = yaml.clone().into();
                }
                json(200, answer)
            }
            _ => json(
                404,
                json!({ "message": format!("404 no route for {method} {path}") }),
            ),
        };
        (response, Headers::new())
    }
}

fn create_variable(inner: &mut Inner, project_id: u64, body: Value) -> Response {
    let key = body["key"].as_str().unwrap_or("").to_string();
    if !valid_key(&key) {
        return bad_request("key", "is invalid".to_string());
    }
    let Some(value) = body["value"].as_str().map(String::from) else {
        return json(400, json!({ "error": "value is missing" }));
    };
    let flag = |name: &str| body[name].as_bool().unwrap_or(false);
    let hidden = flag("masked_and_hidden");
    let variable = FakeVariable {
        key,
        value,
        environment_scope: body["environment_scope"]
            .as_str()
            .unwrap_or("*")
            .to_string(),
        protected: flag("protected"),
        masked: hidden || flag("masked"),
        hidden,
        raw: flag("raw"),
    };
    let vars = inner.variables.entry(project_id).or_default();
    if vars
        .iter()
        .any(|v| v.key == variable.key && v.environment_scope == variable.environment_scope)
    {
        return bad_request("key", format!("({}) has already been taken", variable.key));
    }
    if variable.masked && !maskable(&variable.value) {
        return bad_request("value", "is invalid".to_string());
    }
    let created = variable.to_json();
    vars.push(variable);
    json(201, created)
}

fn update_variable(variable: &mut FakeVariable, body: Value) -> Response {
    if body.get("masked_and_hidden").is_some() || body.get("hidden").is_some() {
        return json(
            400,
            json!({ "message": "masked_and_hidden can only be set on create" }),
        );
    }
    if let Some(value) = body["value"].as_str() {
        if variable.masked && !maskable(value) {
            return bad_request("value", "is invalid".to_string());
        }
        variable.value = value.to_string();
    }
    if let Some(protected) = body["protected"].as_bool() {
        variable.protected = protected;
    }
    if let Some(raw) = body["raw"].as_bool() {
        variable.raw = raw;
    }
    json(200, variable.to_json())
}

#[async_trait]
impl HttpClient for FakeGitlab {
    async fn send(&self, request: Request) -> Result<Response> {
        Ok(self.route(&request).0)
    }

    async fn send_with_headers(&self, request: Request) -> Result<(Response, Headers)> {
        Ok(self.route(&request))
    }
}
