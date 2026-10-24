use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::Result;
use crate::check::LintJob;
use crate::http::{HttpClient, Request, Response};

pub const FAKE_TOKEN: &str = "glpat-test";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub body: Option<Value>,
}

#[derive(Debug, Clone)]
struct RunnerRecord {
    id: u64,
    description: String,
    tag_list: Vec<String>,
    projects: Vec<u64>,
}

#[derive(Debug, Clone)]
struct HookRecord {
    id: u64,
    project_id: u64,
    url: String,
    token: String,
    job_events: bool,
    push_events: bool,
}

#[derive(Debug, Default)]
struct Inner {
    runners: Vec<RunnerRecord>,
    hooks: Vec<HookRecord>,
    next_id: u64,
    calls: Vec<Call>,
    jobs: BTreeMap<u64, Vec<LintJob>>,
    lint_errors: BTreeMap<u64, Vec<String>>,
}

/// In-memory GitLab with the endpoints ciburst uses, behind the [`HttpClient`] seam.
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

    pub fn set_lint_errors(&self, project_id: u64, errors: Vec<String>) {
        self.inner
            .lock()
            .expect("fake gitlab")
            .lint_errors
            .insert(project_id, errors);
    }
}

/// A query parameter of `url`, as GitLab's list filters read it.
fn query_value(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
}

fn json(status: u16, value: Value) -> Response {
    Response::json_body(status, &value)
}

fn segments(path: &str) -> Vec<&str> {
    path.trim_matches('/').split('/').collect()
}

#[async_trait]
impl HttpClient for FakeGitlab {
    async fn send(&self, request: Request) -> Result<Response> {
        let mut inner = self.inner.lock().expect("fake gitlab");
        let after_base = request
            .url
            .find("/api/v4")
            .map(|i| &request.url[i + "/api/v4".len()..])
            .unwrap_or("");
        let path = after_base.split('?').next().unwrap_or("").to_string();
        let body: Option<Value> = request
            .body
            .as_ref()
            .and_then(|b| serde_json::from_slice(b).ok());
        inner.calls.push(Call {
            method: request.method.clone(),
            path: path.clone(),
            body: body.clone(),
        });
        if request.header_value("PRIVATE-TOKEN") != Some(FAKE_TOKEN) {
            return Ok(json(401, json!({ "message": "401 Unauthorized" })));
        }
        let method = request.method.as_str();
        let seg = segments(&path);
        let id = |s: &str| s.parse::<u64>().ok();

        match (method, seg.as_slice()) {
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
                Ok(json(200, Value::Array(list)))
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
                        Ok(json(
                            201,
                            json!({ "id": runner.id, "description": runner.description }),
                        ))
                    }
                    None => Ok(json(404, json!({ "message": "404 Runner Not Found" }))),
                }
            }
            ("POST", ["user", "runners"]) => {
                let body = body.unwrap_or(Value::Null);
                let runner_id = inner.next_id;
                inner.next_id += 1;
                inner.runners.push(RunnerRecord {
                    id: runner_id,
                    description: body["description"].as_str().unwrap_or("").to_string(),
                    tag_list: body["tag_list"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    projects: vec![body["project_id"].as_u64().unwrap_or(0)],
                });
                Ok(json(
                    201,
                    json!({ "id": runner_id, "token": format!("glrt-fake-{runner_id}"), "token_expires_at": null }),
                ))
            }
            ("DELETE", ["runners", r]) => {
                let runner_id = id(r).unwrap_or(0);
                let before = inner.runners.len();
                inner.runners.retain(|runner| runner.id != runner_id);
                if inner.runners.len() == before {
                    Ok(json(404, json!({ "message": "404 Not found" })))
                } else {
                    Ok(Response::empty(204))
                }
            }
            ("GET", ["projects", p, "hooks"]) => {
                let project = id(p).unwrap_or(0);
                let list: Vec<Value> = inner
                    .hooks
                    .iter()
                    .filter(|h| h.project_id == project)
                    .map(|h| json!({ "id": h.id, "url": h.url }))
                    .collect();
                Ok(json(200, Value::Array(list)))
            }
            ("POST", ["projects", p, "hooks"]) => {
                let body = body.unwrap_or(Value::Null);
                let hook_id = inner.next_id;
                inner.next_id += 1;
                inner.hooks.push(HookRecord {
                    id: hook_id,
                    project_id: id(p).unwrap_or(0),
                    url: body["url"].as_str().unwrap_or("").to_string(),
                    token: body["token"].as_str().unwrap_or("").to_string(),
                    job_events: body["job_events"].as_bool().unwrap_or(false),
                    push_events: body["push_events"].as_bool().unwrap_or(true),
                });
                let url = body["url"].clone();
                Ok(json(201, json!({ "id": hook_id, "url": url })))
            }
            ("PUT" | "DELETE", ["projects", p, "hooks", h]) => {
                let project = id(p).unwrap_or(0);
                let hook_id = id(h).unwrap_or(0);
                let Some(index) = inner
                    .hooks
                    .iter()
                    .position(|hook| hook.project_id == project && hook.id == hook_id)
                else {
                    return Ok(json(404, json!({ "message": "404 Not found" })));
                };
                if method == "DELETE" {
                    inner.hooks.remove(index);
                    return Ok(Response::empty(204));
                }
                let body = body.unwrap_or(Value::Null);
                let hook = &mut inner.hooks[index];
                hook.url = body["url"].as_str().unwrap_or("").to_string();
                hook.token = body["token"].as_str().unwrap_or("").to_string();
                hook.job_events = body["job_events"].as_bool().unwrap_or(false);
                hook.push_events = body["push_events"].as_bool().unwrap_or(true);
                let url = hook.url.clone();
                Ok(json(200, json!({ "id": hook_id, "url": url })))
            }
            ("GET", ["projects", p, "ci", "lint"]) => {
                let project = id(p).unwrap_or(0);
                let errors = inner.lint_errors.get(&project).cloned().unwrap_or_default();
                let valid = errors.is_empty();
                let jobs = if valid {
                    inner.jobs.get(&project).cloned().unwrap_or_default()
                } else {
                    Vec::new()
                };
                Ok(json(
                    200,
                    json!({ "valid": valid, "errors": errors, "warnings": [], "jobs": jobs }),
                ))
            }
            _ => Ok(json(
                404,
                json!({ "message": format!("404 no route for {method} {path}") }),
            )),
        }
    }
}
