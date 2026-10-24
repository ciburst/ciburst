//! The provider's fake Scaleway: Account, IAM, Serverless Jobs, Container Registry, Instances
//! with the Marketplace, and S3 behind one `HttpClient` that routes by host and path, plus the
//! helpers the fakes share.

use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::http::{HttpClient, Request, Response};
use serde_json::{Value, json};

use super::account_api::FakeAccount;
use super::iam_api::FakeIam;
use super::instance_api::FakeInstance;
use super::jobs_api::FakeJobs;
use super::registry_api::FakeRegistry;
use super::s3_api::FakeS3;
use crate::client::{Credentials, ScalewayClient};

pub const FAKE_SECRET_KEY: &str = "11111111-2222-4333-8444-555555555555";
pub const FAKE_ACCESS_KEY: &str = "SCWFAKEOWNER00000000";
pub const FAKE_ORGANIZATION_ID: &str = "00000000-0000-4000-8000-0000000000aa";
pub const FAKE_REGION: &str = "fr-par";
pub const API_HOST: &str = "api.scaleway.com";

/// One recorded request: method, path, raw query and JSON body when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Option<Value>,
}

/// A request URL split into host, path and query pairs (decoded).
#[derive(Debug, Clone)]
pub struct Url {
    pub host: String,
    pub path: String,
    pub raw_query: String,
    pub query: Vec<(String, String)>,
}

impl Url {
    pub fn parse(url: &str) -> Url {
        let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
        let (host, path_query) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let (path, raw_query) = path_query.split_once('?').unwrap_or((path_query, ""));
        let query = raw_query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (percent_decode(k), percent_decode(v))
            })
            .collect();
        Url {
            host: host.to_string(),
            path: path.to_string(),
            raw_query: raw_query.to_string(),
            query,
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn all(&self, key: &str) -> Vec<&str> {
        self.query
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn segments(&self) -> Vec<String> {
        self.path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .map(percent_decode)
            .collect()
    }
}

pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
            && let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn call_of(request: &Request, url: &Url) -> Call {
    Call {
        method: request.method.clone(),
        path: url.path.clone(),
        query: url.raw_query.clone(),
        body: request
            .body
            .as_ref()
            .and_then(|b| serde_json::from_slice(b).ok()),
    }
}

pub fn body_json(request: &Request) -> Value {
    request
        .body
        .as_ref()
        .and_then(|b| serde_json::from_slice(b).ok())
        .unwrap_or(Value::Null)
}

pub fn reply(status: u16, value: Value) -> Response {
    Response::json_body(status, &value)
}

/// The Scaleway error body: `{ "message": ..., "type": ... }`.
pub fn api_error(status: u16, kind: &str, message: &str) -> Response {
    reply(status, json!({ "type": kind, "message": message }))
}

pub fn unauthorized() -> Response {
    api_error(401, "denied_authentication", "authentication is denied")
}

pub fn authorized(request: &Request) -> bool {
    request.header_value("X-Auth-Token") == Some(FAKE_SECRET_KEY)
}

/// One page of `items` per `page` and `page_size` (default 20, at most `max_page_size`).
pub fn paginate(items: Vec<Value>, url: &Url, max_page_size: usize) -> (Vec<Value>, usize) {
    let total = items.len();
    let page = url
        .get("page")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let size = url
        .get("page_size")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(20)
        .clamp(1, max_page_size);
    let page_items = items
        .into_iter()
        .skip((page - 1) * size)
        .take(size)
        .collect();
    (page_items, total)
}

/// Deterministic UUID-shaped ids; `kind` keeps ids of different resources apart.
pub fn fake_uuid(kind: u32, n: u64) -> String {
    format!("{kind:08x}-0000-4000-8000-{n:012x}")
}

/// Deterministic RFC 3339 timestamps, one second apart.
pub fn fake_time(n: u64) -> String {
    format!(
        "2026-10-24T{:02}:{:02}:{:02}Z",
        (n / 3600) % 24,
        (n / 60) % 60,
        n % 60
    )
}

/// The provider's backend: one fake per API, routed by host and path.
pub struct ScalewayFake {
    pub account: FakeAccount,
    pub iam: FakeIam,
    pub jobs: FakeJobs,
    pub registry: FakeRegistry,
    pub instance: FakeInstance,
    pub s3: FakeS3,
}

impl Default for ScalewayFake {
    fn default() -> Self {
        ScalewayFake::new()
    }
}

impl ScalewayFake {
    pub fn new() -> ScalewayFake {
        ScalewayFake {
            account: FakeAccount::new(),
            iam: FakeIam::new(),
            jobs: FakeJobs::new(),
            registry: FakeRegistry::new(),
            instance: FakeInstance::new(),
            s3: FakeS3::new(),
        }
    }

    pub fn credentials() -> Credentials {
        Credentials {
            secret_key: FAKE_SECRET_KEY.to_string(),
            access_key: Some(FAKE_ACCESS_KEY.to_string()),
            organization_id: FAKE_ORGANIZATION_ID.to_string(),
            default_project_id: None,
        }
    }

    pub fn client(self: &Arc<Self>) -> Arc<ScalewayClient> {
        Arc::new(ScalewayClient::new(
            self.clone(),
            ScalewayFake::credentials(),
            FAKE_REGION,
        ))
    }

    /// Every write request (not GET or HEAD) across the APIs.
    pub fn writes(&self) -> Vec<Call> {
        self.account
            .calls()
            .into_iter()
            .chain(self.iam.calls())
            .chain(self.jobs.calls())
            .chain(self.registry.calls())
            .chain(self.instance.calls())
            .chain(self.s3.calls())
            .filter(|c| c.method != "GET" && c.method != "HEAD")
            .collect()
    }
}

#[async_trait]
impl HttpClient for ScalewayFake {
    async fn send(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        if url.host == API_HOST {
            if url.path.starts_with("/account/") {
                return self.account.send(request).await;
            }
            if url.path.starts_with("/iam/") {
                return self.iam.send(request).await;
            }
            if url.path.starts_with("/serverless-jobs/") {
                return self.jobs.send(request).await;
            }
            if url.path.starts_with("/registry/") {
                return self.registry.send(request).await;
            }
            if url.path.starts_with("/instance/") || url.path.starts_with("/marketplace/") {
                return self.instance.send(request).await;
            }
        } else if url.host.starts_with("s3.") && url.host.ends_with(".scw.cloud") {
            return self.s3.send(request).await;
        }
        Ok(api_error(
            404,
            "not_found",
            &format!("no route for {} {}{}", request.method, url.host, url.path),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let url =
            Url::parse("https://api.scaleway.com/iam/v1alpha1/policies?a=1&b=x%20y&lifecycle");
        assert_eq!(url.host, "api.scaleway.com");
        assert_eq!(url.path, "/iam/v1alpha1/policies");
        assert_eq!(url.get("b"), Some("x y"));
        assert_eq!(url.get("lifecycle"), Some(""));
        assert_eq!(url.segments(), ["iam", "v1alpha1", "policies"]);
        assert_eq!(percent_decode("%2"), "%2");
    }
}
