//! Fake of the Account API (`/account/v3/projects`): list by organization and name, create,
//! delete. Shapes from scaleway-sdk-go `api/account/v3` (`ListProjectsResponse.projects`,
//! `total_count`; `Project.id`, `name`, `organization_id`, `description`, `created_at`).

use std::sync::Mutex;

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::http::{HttpClient, Request, Response};
use serde_json::{Value, json};

use super::scaleway_fake::{
    Call, Url, api_error, authorized, body_json, call_of, fake_time, fake_uuid, paginate, reply,
    unauthorized,
};

#[derive(Default)]
struct Inner {
    projects: Vec<Value>,
    next: u64,
    calls: Vec<Call>,
    fail_delete: bool,
}

#[derive(Default)]
pub struct FakeAccount {
    inner: Mutex<Inner>,
}

impl FakeAccount {
    pub fn new() -> FakeAccount {
        FakeAccount::default()
    }

    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().expect("fake account").calls.clone()
    }

    /// `(id, name)` of every project.
    pub fn projects(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .expect("fake account")
            .projects
            .iter()
            .map(|p| {
                (
                    p["id"].as_str().unwrap_or_default().to_string(),
                    p["name"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    /// Makes project deletion fail, as the real API does while resources remain in it.
    pub fn fail_delete(&self, fail: bool) {
        self.inner.lock().expect("fake account").fail_delete = fail;
    }
}

#[async_trait]
impl HttpClient for FakeAccount {
    async fn send(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        let mut inner = self.inner.lock().expect("fake account");
        inner.calls.push(call_of(&request, &url));
        if !authorized(&request) {
            return Ok(unauthorized());
        }
        let segments = url.segments();
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        match (request.method.as_str(), segments.as_slice()) {
            ("GET", ["account", "v3", "projects"]) => {
                let Some(organization) = url.get("organization_id") else {
                    return Ok(api_error(
                        400,
                        "invalid_arguments",
                        "organization_id required",
                    ));
                };
                let items: Vec<Value> = inner
                    .projects
                    .iter()
                    .filter(|p| p["organization_id"] == organization)
                    .filter(|p| url.get("name").is_none_or(|n| p["name"] == n))
                    .cloned()
                    .collect();
                let (page, total) = paginate(items, &url, 100);
                Ok(reply(
                    200,
                    json!({ "total_count": total, "projects": page }),
                ))
            }
            ("POST", ["account", "v3", "projects"]) => {
                let body = body_json(&request);
                let (Some(name), Some(organization)) =
                    (body["name"].as_str(), body["organization_id"].as_str())
                else {
                    return Ok(api_error(
                        400,
                        "invalid_arguments",
                        "name and organization_id required",
                    ));
                };
                inner.next += 1;
                let project = json!({
                    "id": fake_uuid(1, inner.next),
                    "name": name,
                    "organization_id": organization,
                    "description": body["description"].as_str().unwrap_or_default(),
                    "created_at": fake_time(inner.next),
                    "updated_at": fake_time(inner.next),
                });
                inner.projects.push(project.clone());
                Ok(reply(200, project))
            }
            ("DELETE", ["account", "v3", "projects", id]) => {
                if inner.fail_delete {
                    return Ok(api_error(
                        412,
                        "precondition_failed",
                        "project still holds resources",
                    ));
                }
                let before = inner.projects.len();
                inner.projects.retain(|p| p["id"] != *id);
                if inner.projects.len() == before {
                    Ok(api_error(404, "not_found", "project not found"))
                } else {
                    Ok(Response::empty(204))
                }
            }
            _ => Ok(api_error(404, "not_found", "no route")),
        }
    }
}
