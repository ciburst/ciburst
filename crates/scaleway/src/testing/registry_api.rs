//! Fake of the Container Registry API (`/registry/v1/regions/<region>/namespaces`): list by
//! project and name, create, patch visibility, delete. Shapes from scaleway-sdk-go
//! `api/registry/v1` (`ListNamespacesResponse.namespaces`, `total_count`; `Namespace.id`,
//! `name`, `endpoint`, `is_public`, `status`, `project_id`, `region`).

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
    namespaces: Vec<Value>,
    next: u64,
    calls: Vec<Call>,
}

#[derive(Default)]
pub struct FakeRegistry {
    inner: Mutex<Inner>,
}

impl FakeRegistry {
    pub fn new() -> FakeRegistry {
        FakeRegistry::default()
    }

    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().expect("fake registry").calls.clone()
    }

    /// `(name, is_public)` of every namespace.
    pub fn namespaces(&self) -> Vec<(String, bool)> {
        self.inner
            .lock()
            .expect("fake registry")
            .namespaces
            .iter()
            .map(|n| {
                (
                    n["name"].as_str().unwrap_or_default().to_string(),
                    n["is_public"].as_bool().unwrap_or(false),
                )
            })
            .collect()
    }
}

#[async_trait]
impl HttpClient for FakeRegistry {
    async fn send(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        let mut inner = self.inner.lock().expect("fake registry");
        inner.calls.push(call_of(&request, &url));
        if !authorized(&request) {
            return Ok(unauthorized());
        }
        let body = body_json(&request);
        let segments = url.segments();
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        let ["registry", "v1", "regions", region, "namespaces", rest @ ..] = segments.as_slice()
        else {
            return Ok(api_error(404, "not_found", "no route"));
        };
        let region = region.to_string();
        match (request.method.as_str(), rest) {
            ("GET", []) => {
                let items: Vec<Value> = inner
                    .namespaces
                    .iter()
                    .filter(|n| n["region"] == region.as_str())
                    .filter(|n| url.get("project_id").is_none_or(|p| n["project_id"] == p))
                    .filter(|n| url.get("name").is_none_or(|p| n["name"] == p))
                    .cloned()
                    .collect();
                let (page, total) = paginate(items, &url, 100);
                Ok(reply(
                    200,
                    json!({ "namespaces": page, "total_count": total }),
                ))
            }
            ("POST", []) => {
                let name = body["name"].as_str().unwrap_or_default();
                let project = body["project_id"].as_str().unwrap_or_default();
                if name.is_empty() || project.is_empty() {
                    return Ok(api_error(
                        400,
                        "invalid_arguments",
                        "name and project_id required",
                    ));
                }
                let duplicate = inner
                    .namespaces
                    .iter()
                    .any(|n| n["name"] == name && n["region"] == region.as_str());
                if duplicate {
                    return Ok(api_error(409, "conflict", "namespace name exists"));
                }
                inner.next += 1;
                let n = inner.next;
                let namespace = json!({
                    "id": fake_uuid(7, n),
                    "name": name,
                    "project_id": project,
                    "organization_id": body["organization_id"],
                    "description": body["description"],
                    "is_public": body["is_public"].as_bool().unwrap_or(false),
                    "endpoint": format!("rg.{region}.scw.cloud/{name}"),
                    "status": "ready",
                    "created_at": fake_time(n),
                    "region": region,
                    "size": 0,
                    "image_count": 0,
                });
                inner.namespaces.push(namespace.clone());
                Ok(reply(200, namespace))
            }
            ("GET" | "PATCH" | "DELETE", [id]) => {
                let Some(index) = inner.namespaces.iter().position(|n| n["id"] == *id) else {
                    return Ok(api_error(404, "not_found", "namespace not found"));
                };
                match request.method.as_str() {
                    "GET" => Ok(reply(200, inner.namespaces[index].clone())),
                    "DELETE" => {
                        let removed = inner.namespaces.remove(index);
                        Ok(reply(200, removed))
                    }
                    _ => {
                        if let Some(public) = body["is_public"].as_bool() {
                            inner.namespaces[index]["is_public"] = json!(public);
                        }
                        if let Some(description) = body["description"].as_str() {
                            inner.namespaces[index]["description"] = json!(description);
                        }
                        Ok(reply(200, inner.namespaces[index].clone()))
                    }
                }
            }
            _ => Ok(api_error(404, "not_found", "no route")),
        }
    }
}
