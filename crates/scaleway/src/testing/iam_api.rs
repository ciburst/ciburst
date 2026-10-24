//! Fake of the IAM API (`/iam/v1alpha1`): applications, policies, api-keys (secret key shown
//! once on creation). Shapes from scaleway-sdk-go `api/iam/v1alpha1`: `applications`,
//! `policies`, `api_keys` with `total_count`; `RuleSpecs.permission_set_names`, `project_ids`;
//! `APIKey.access_key`, `secret_key`, `application_id`, `default_project_id`.

use std::sync::Mutex;

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::http::{HttpClient, Request, Response};
use serde_json::{Value, json};

use super::scaleway_fake::{
    Call, Url, api_error, authorized, body_json, call_of, fake_time, fake_uuid, paginate, reply,
    unauthorized,
};

/// Permission sets the fake accepts; the provider uses the first four.
pub const PERMISSION_SETS: [&str; 6] = [
    "ContainerRegistryReadOnly",
    "ServerlessJobsFullAccess",
    "ObjectStorageFullAccess",
    "InstancesFullAccess",
    "ServerlessJobsReadOnly",
    "ObjectStorageReadOnly",
];

#[derive(Default)]
struct Inner {
    applications: Vec<Value>,
    policies: Vec<Value>,
    api_keys: Vec<Value>,
    next: u64,
    calls: Vec<Call>,
}

impl Inner {
    fn next(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn application_exists(&self, id: &str) -> bool {
        self.applications.iter().any(|a| a["id"] == id)
    }
}

#[derive(Default)]
pub struct FakeIam {
    inner: Mutex<Inner>,
}

impl FakeIam {
    pub fn new() -> FakeIam {
        FakeIam::default()
    }

    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().expect("fake iam").calls.clone()
    }

    pub fn applications(&self) -> Vec<Value> {
        self.inner.lock().expect("fake iam").applications.clone()
    }

    /// Policies as created, including their `rules`.
    pub fn policies(&self) -> Vec<Value> {
        self.inner.lock().expect("fake iam").policies.clone()
    }

    /// Replaces the permission sets of the named policy, as an older ciburst version left them.
    pub fn set_policy_sets(&self, name: &str, sets: &[&str]) {
        let mut inner = self.inner.lock().expect("fake iam");
        if let Some(policy) = inner.policies.iter_mut().find(|p| p["name"] == name)
            && let Some(rules) = policy["rules"].as_array_mut()
        {
            for rule in rules {
                rule["permission_set_names"] = json!(sets);
            }
        }
    }

    /// API keys as listed (without secrets).
    pub fn api_keys(&self) -> Vec<Value> {
        self.inner
            .lock()
            .expect("fake iam")
            .api_keys
            .iter()
            .map(listed_key)
            .collect()
    }
}

fn listed_key(key: &Value) -> Value {
    let mut key = key.clone();
    key["secret_key"] = Value::Null;
    key
}

fn list(items: Vec<Value>, url: &Url, field: &str) -> Response {
    let (page, total) = paginate(items, url, 100);
    reply(200, json!({ field: page, "total_count": total }))
}

fn delete_where(items: &mut Vec<Value>, field: &str, id: &str, what: &str) -> Response {
    let before = items.len();
    items.retain(|item| item[field] != id);
    if items.len() == before {
        api_error(404, "not_found", &format!("{what} not found"))
    } else {
        Response::empty(204)
    }
}

#[async_trait]
impl HttpClient for FakeIam {
    async fn send(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        let mut inner = self.inner.lock().expect("fake iam");
        inner.calls.push(call_of(&request, &url));
        if !authorized(&request) {
            return Ok(unauthorized());
        }
        let body = body_json(&request);
        let segments = url.segments();
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        match (request.method.as_str(), segments.as_slice()) {
            ("GET", ["iam", "v1alpha1", "applications"]) => {
                let items = inner
                    .applications
                    .iter()
                    .filter(|a| {
                        url.get("organization_id")
                            .is_none_or(|o| a["organization_id"] == o)
                    })
                    .filter(|a| url.get("name").is_none_or(|n| a["name"] == n))
                    .cloned()
                    .collect();
                Ok(list(items, &url, "applications"))
            }
            ("POST", ["iam", "v1alpha1", "applications"]) => {
                let (Some(name), Some(organization)) =
                    (body["name"].as_str(), body["organization_id"].as_str())
                else {
                    return Ok(api_error(
                        400,
                        "invalid_arguments",
                        "name and organization_id required",
                    ));
                };
                let n = inner.next();
                let application = json!({
                    "id": fake_uuid(2, n),
                    "name": name,
                    "description": body["description"].as_str().unwrap_or_default(),
                    "organization_id": organization,
                    "created_at": fake_time(n),
                    "updated_at": fake_time(n),
                    "editable": true,
                    "deletable": true,
                    "managed": false,
                    "nb_api_keys": 0,
                    "tags": [],
                });
                inner.applications.push(application.clone());
                Ok(reply(200, application))
            }
            ("DELETE", ["iam", "v1alpha1", "applications", id]) => Ok(delete_where(
                &mut inner.applications,
                "id",
                id,
                "application",
            )),
            ("GET", ["iam", "v1alpha1", "policies"]) => {
                let applications = url.all("application_ids");
                let items = inner
                    .policies
                    .iter()
                    .filter(|p| {
                        url.get("organization_id")
                            .is_none_or(|o| p["organization_id"] == o)
                    })
                    .filter(|p| {
                        applications.is_empty()
                            || applications.iter().any(|a| p["application_id"] == *a)
                    })
                    .map(|p| {
                        let mut p = p.clone();
                        if let Some(fields) = p.as_object_mut() {
                            fields.remove("rules");
                        }
                        p
                    })
                    .collect();
                Ok(list(items, &url, "policies"))
            }
            ("POST", ["iam", "v1alpha1", "policies"]) => {
                let (Some(name), Some(organization)) =
                    (body["name"].as_str(), body["organization_id"].as_str())
                else {
                    return Ok(api_error(
                        400,
                        "invalid_arguments",
                        "name and organization_id required",
                    ));
                };
                let application = body["application_id"].as_str().unwrap_or_default();
                if !inner.application_exists(application) {
                    return Ok(api_error(404, "not_found", "application not found"));
                }
                let rules = body["rules"].as_array().cloned().unwrap_or_default();
                let valid = !rules.is_empty()
                    && rules.iter().all(|rule| {
                        let sets = rule["permission_set_names"].as_array();
                        let projects = rule["project_ids"].as_array();
                        sets.is_some_and(|s| {
                            !s.is_empty()
                                && s.iter().all(|n| {
                                    n.as_str().is_some_and(|n| PERMISSION_SETS.contains(&n))
                                })
                        }) && projects.is_some_and(|p| !p.is_empty())
                    });
                if !valid {
                    return Ok(api_error(400, "invalid_arguments", "invalid rules"));
                }
                let n = inner.next();
                let policy = json!({
                    "id": fake_uuid(3, n),
                    "name": name,
                    "description": body["description"].as_str().unwrap_or_default(),
                    "organization_id": organization,
                    "created_at": fake_time(n),
                    "updated_at": fake_time(n),
                    "editable": true,
                    "deletable": true,
                    "managed": false,
                    "nb_rules": rules.len(),
                    "nb_scopes": 1,
                    "nb_permission_sets": 1,
                    "tags": [],
                    "application_id": application,
                    "rules": rules,
                });
                inner.policies.push(policy.clone());
                let mut response = policy;
                if let Some(fields) = response.as_object_mut() {
                    fields.remove("rules");
                }
                Ok(reply(200, response))
            }
            ("DELETE", ["iam", "v1alpha1", "policies", id]) => {
                Ok(delete_where(&mut inner.policies, "id", id, "policy"))
            }
            ("GET", ["iam", "v1alpha1", "rules"]) => {
                let Some(policy) = url.get("policy_id") else {
                    return Ok(api_error(400, "invalid_arguments", "policy_id required"));
                };
                let Some(found) = inner.policies.iter().find(|p| p["id"] == policy) else {
                    return Ok(api_error(404, "not_found", "policy not found"));
                };
                let items = found["rules"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .enumerate()
                    .map(|(index, mut rule)| {
                        rule["id"] = json!(format!("{policy}-rule-{index}"));
                        rule
                    })
                    .collect();
                Ok(list(items, &url, "rules"))
            }
            ("GET", ["iam", "v1alpha1", "api-keys"]) => {
                let items = inner
                    .api_keys
                    .iter()
                    .filter(|k| {
                        url.get("application_id")
                            .is_none_or(|a| k["application_id"] == a)
                    })
                    .map(listed_key)
                    .collect();
                Ok(list(items, &url, "api_keys"))
            }
            ("POST", ["iam", "v1alpha1", "api-keys"]) => {
                let application = body["application_id"].as_str().unwrap_or_default();
                if !inner.application_exists(application) {
                    return Ok(api_error(404, "not_found", "application not found"));
                }
                let n = inner.next();
                let key = json!({
                    "access_key": format!("SCW{:017X}", n),
                    "secret_key": fake_uuid(4, n),
                    "application_id": application,
                    "description": body["description"].as_str().unwrap_or_default(),
                    "created_at": fake_time(n),
                    "updated_at": fake_time(n),
                    "expires_at": null,
                    "default_project_id": body["default_project_id"].as_str().unwrap_or_default(),
                    "editable": true,
                    "deletable": true,
                    "managed": false,
                    "creation_ip": "",
                });
                inner.api_keys.push(key.clone());
                if let Some(app) = inner
                    .applications
                    .iter_mut()
                    .find(|a| a["id"] == application)
                {
                    let count = app["nb_api_keys"].as_u64().unwrap_or(0);
                    app["nb_api_keys"] = json!(count + 1);
                }
                Ok(reply(200, key))
            }
            ("DELETE", ["iam", "v1alpha1", "api-keys", access_key]) => Ok(delete_where(
                &mut inner.api_keys,
                "access_key",
                access_key,
                "api key",
            )),
            _ => Ok(api_error(404, "not_found", "no route")),
        }
    }
}
