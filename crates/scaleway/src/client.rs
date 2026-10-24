use std::collections::BTreeMap;
use std::sync::Arc;

use ciburst_core::http::{HttpClient, Request, Response};
use ciburst_core::{Error, Result};
use serde_json::Value;

pub const DEFAULT_ENDPOINT: &str = "https://api.scaleway.com";
pub const DEFAULT_REGION: &str = "fr-par";

/// Credentials from `.local/scaleway.env`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub secret_key: String,
    /// Access key of the same API key; S3 signing needs it, the JSON APIs do not.
    pub access_key: Option<String>,
    pub organization_id: String,
    pub default_project_id: Option<String>,
}

impl Credentials {
    pub fn from_env(env: &BTreeMap<String, String>) -> Result<Credentials> {
        let get = |key: &str| {
            env.get(key)
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or_else(|| Error::other(format!("{key} missing in .local/scaleway.env")))
        };
        let optional = |key: &str| env.get(key).filter(|v| !v.is_empty()).cloned();
        Ok(Credentials {
            secret_key: get("SCW_SECRET_KEY")?,
            access_key: optional("SCW_ACCESS_KEY"),
            organization_id: get("SCW_DEFAULT_ORGANIZATION_ID")?,
            default_project_id: optional("SCW_DEFAULT_PROJECT_ID"),
        })
    }
}

/// JSON client for the Scaleway API: auth header, error mapping, region. Provider and host build
/// their endpoints on top of `get`, `post`, `patch` and `delete`.
pub struct ScalewayClient {
    http: Arc<dyn HttpClient>,
    endpoint: String,
    credentials: Credentials,
    region: String,
}

impl ScalewayClient {
    pub fn new(
        http: Arc<dyn HttpClient>,
        credentials: Credentials,
        region: &str,
    ) -> ScalewayClient {
        ScalewayClient {
            http,
            endpoint: DEFAULT_ENDPOINT.to_string(),
            credentials,
            region: region.to_string(),
        }
    }

    pub fn with_endpoint(mut self, endpoint: &str) -> ScalewayClient {
        self.endpoint = endpoint.trim_end_matches('/').to_string();
        self
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn organization_id(&self) -> &str {
        &self.credentials.organization_id
    }

    pub fn default_project_id(&self) -> Option<&str> {
        self.credentials.default_project_id.as_deref()
    }

    pub fn access_key(&self) -> Option<&str> {
        self.credentials.access_key.as_deref()
    }

    pub fn secret_key(&self) -> &str {
        &self.credentials.secret_key
    }

    pub fn http(&self) -> Arc<dyn HttpClient> {
        self.http.clone()
    }

    /// Sends a JSON request to `<endpoint><path>`; `path` starts with a slash and names the API
    /// and version, for example `/serverless-jobs/v1alpha1/regions/fr-par/job-definitions`.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        let mut url = format!("{}{}", self.endpoint, path);
        for (index, (key, value)) in query.iter().enumerate() {
            url.push(if index == 0 { '?' } else { '&' });
            url.push_str(key);
            url.push('=');
            url.push_str(&urlencode(value));
        }
        let mut request = Request::new(method, url)
            .header("X-Auth-Token", self.credentials.secret_key.clone())
            .header("accept", "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = self.http.send(request).await?;
        if response.ok() {
            return parse(&response);
        }
        Err(Error::provider(format!(
            "scaleway {method} {path}: {} {}",
            response.status,
            error_message(&response)
        )))
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.call("GET", path, query, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.call("POST", path, &[], Some(body)).await
    }

    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.call("PATCH", path, &[], Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.call("DELETE", path, &[], None).await
    }

    /// The project id every resource lives in: `SCW_DEFAULT_PROJECT_ID`, else the project named
    /// like the config, else `None` (the provider creates it).
    pub async fn find_project(&self, name: &str) -> Result<Option<String>> {
        if let Some(id) = &self.credentials.default_project_id {
            return Ok(Some(id.clone()));
        }
        let list = self
            .get(
                "/account/v3/projects",
                &[
                    ("organization_id", self.credentials.organization_id.clone()),
                    ("name", name.to_string()),
                ],
            )
            .await?;
        Ok(list["projects"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["name"].as_str() == Some(name))
            .and_then(|p| p["id"].as_str().map(String::from)))
    }
}

fn parse(response: &Response) -> Result<Value> {
    if response.body.is_empty() {
        return Ok(Value::Null);
    }
    response.json()
}

fn error_message(response: &Response) -> String {
    response
        .json::<Value>()
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| response.text())
}

pub fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct Recorder {
        requests: Mutex<Vec<Request>>,
        response: Response,
    }

    #[async_trait]
    impl HttpClient for Recorder {
        async fn send(&self, request: Request) -> Result<Response> {
            self.requests.lock().unwrap().push(request);
            Ok(self.response.clone())
        }
    }

    fn credentials() -> Credentials {
        Credentials {
            secret_key: "secret".into(),
            access_key: None,
            organization_id: "org".into(),
            default_project_id: None,
        }
    }

    #[tokio::test]
    async fn sends_auth_header_and_encodes_queries() {
        let recorder = Arc::new(Recorder {
            requests: Mutex::new(Vec::new()),
            response: Response::json_body(200, &serde_json::json!({ "projects": [] })),
        });
        let client = ScalewayClient::new(recorder.clone(), credentials(), "fr-par");
        let value = client
            .get(
                "/account/v3/projects",
                &[("name", "a b".into()), ("organization_id", "org".into())],
            )
            .await
            .unwrap();
        assert_eq!(value["projects"], serde_json::json!([]));
        let requests = recorder.requests.lock().unwrap();
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests[0].url,
            "https://api.scaleway.com/account/v3/projects?name=a%20b&organization_id=org"
        );
        assert_eq!(requests[0].header_value("x-auth-token"), Some("secret"));
    }

    #[tokio::test]
    async fn maps_errors_with_the_message() {
        let recorder = Arc::new(Recorder {
            requests: Mutex::new(Vec::new()),
            response: Response::json_body(
                403,
                &serde_json::json!({ "message": "insufficient permissions" }),
            ),
        });
        let client = ScalewayClient::new(recorder, credentials(), "fr-par");
        let error = client.delete("/x/v1/y").await.unwrap_err().to_string();
        assert_eq!(
            error,
            "scaleway DELETE /x/v1/y: 403 insufficient permissions"
        );
    }

    #[tokio::test]
    async fn finds_the_project_by_name_or_default() {
        let recorder = Arc::new(Recorder {
            requests: Mutex::new(Vec::new()),
            response: Response::json_body(
                200,
                &serde_json::json!({ "projects": [{ "id": "p-1", "name": "demo" }, { "id": "p-2", "name": "demo-2" }] }),
            ),
        });
        let client = ScalewayClient::new(recorder.clone(), credentials(), "fr-par");
        assert_eq!(
            client.find_project("demo").await.unwrap().as_deref(),
            Some("p-1")
        );
        assert_eq!(client.find_project("other").await.unwrap(), None);
        let fixed = ScalewayClient::new(
            recorder,
            Credentials {
                default_project_id: Some("fixed".into()),
                ..credentials()
            },
            "fr-par",
        );
        assert_eq!(
            fixed.find_project("demo").await.unwrap().as_deref(),
            Some("fixed")
        );
    }

    #[test]
    fn credentials_from_env() {
        let env: BTreeMap<String, String> = [
            ("SCW_SECRET_KEY", "s"),
            ("SCW_DEFAULT_ORGANIZATION_ID", "o"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let credentials = Credentials::from_env(&env).unwrap();
        assert_eq!(credentials.secret_key, "s");
        assert_eq!(credentials.default_project_id, None);
        assert!(
            Credentials::from_env(&BTreeMap::new())
                .unwrap_err()
                .to_string()
                .contains("SCW_SECRET_KEY")
        );
        assert_eq!(urlencode("a-b_c.d~e f/g"), "a-b_c.d~e%20f%2Fg");
    }
}
