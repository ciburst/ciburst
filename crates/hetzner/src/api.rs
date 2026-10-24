//! The Hetzner Cloud API over the core's HTTP seam (hcloud-go v2 `hcloud/client.go`,
//! `schema/meta.go`, `schema/error.go`): `Authorization: Bearer <token>`, JSON bodies, lists
//! paged by `page` and `per_page` with `meta.pagination.next_page`, and errors as
//! `{ "error": { "code", "message" } }`. The token never enters an error or a log line.

use std::fmt;
use std::sync::Arc;

use ciburst_core::http::{HttpClient, Request};
use ciburst_core::{Error, Result};
use serde_json::Value;

use crate::{RUNTIME, TOKEN_VAR};

/// The largest `per_page` the API serves.
pub const PAGE_SIZE: u32 = 50;
/// Pages read at most from one listing, a guard against a pagination loop.
const MAX_PAGES: u64 = 1_000;
const REDACTED: &str = "[redacted]";

/// An error answer: the HTTP status and, when the body is the API's error object, its code and
/// message. Non-JSON bodies are not kept, so nothing a proxy echoes ends up in a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl ApiError {
    /// `resource_unavailable`, `resource_limit_exceeded` or `placement_error`: another server
    /// type may still start (M4 rule R12).
    pub fn is_capacity(&self) -> bool {
        matches!(
            self.code.as_str(),
            "resource_unavailable" | "resource_limit_exceeded" | "placement_error"
        )
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.code.is_empty() {
            write!(f, "status {}", self.status)
        } else {
            write!(f, "{} ({}): {}", self.code, self.status, self.message)
        }
    }
}

/// What one request came to: the JSON body (`Null` when empty) or the API's error.
pub type Answer = std::result::Result<Value, ApiError>;

pub struct Api {
    http: Arc<dyn HttpClient>,
    base: String,
    token: String,
}

impl Api {
    pub fn new(http: Arc<dyn HttpClient>, base: &str, token: &str) -> Api {
        Api {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// One request. Transport failures and an unusable token are errors; API errors are the
    /// inner `Err`.
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Answer> {
        let token_ok = !self.token.is_empty() && self.token.bytes().all(|b| b.is_ascii_graphic());
        if !token_ok {
            return Err(Error::provider(format!(
                "{RUNTIME}: {TOKEN_VAR} is empty or not a valid header value"
            )));
        }
        let mut url = format!("{}{path}", self.base);
        if !query.is_empty() {
            let pairs: Vec<String> = query
                .iter()
                .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
                .collect();
            url.push('?');
            url.push_str(&pairs.join("&"));
        }
        let mut request = Request::new(method, url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = self.http.send(request).await.map_err(|error| {
            Error::provider(format!(
                "{RUNTIME}: {method} {path}: {}",
                self.redact(&error.to_string())
            ))
        })?;
        let parsed: Option<Value> = if response.body.is_empty() {
            Some(Value::Null)
        } else {
            serde_json::from_slice(&response.body).ok()
        };
        if response.ok() {
            return parsed.map(Ok).ok_or_else(|| {
                Error::provider(format!(
                    "{RUNTIME}: {method} {path}: the response is not JSON"
                ))
            });
        }
        let error = parsed.as_ref().map(|v| &v["error"]);
        let text = |key: &str| {
            error
                .and_then(|e| e[key].as_str())
                .map(|s| self.redact(s))
                .unwrap_or_default()
        };
        Ok(Err(ApiError {
            status: response.status,
            code: text("code"),
            message: text("message"),
        }))
    }

    fn redact(&self, text: &str) -> String {
        if self.token.is_empty() {
            text.to_string()
        } else {
            text.replace(&self.token, REDACTED)
        }
    }

    /// The error of `method path`, naming the API's code and message.
    pub fn error(method: &str, path: &str, error: &ApiError) -> Error {
        Error::provider(format!("{RUNTIME}: {method} {path}: {error}"))
    }

    async fn expect(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        self.send(method, path, query, body)
            .await?
            .map_err(|error| Api::error(method, path, &error))
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.expect("GET", path, query, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.expect("POST", path, &[], Some(body)).await
    }

    pub async fn put(&self, path: &str, body: &Value) -> Result<Value> {
        self.expect("PUT", path, &[], Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.expect("DELETE", path, &[], None).await
    }

    /// Every item of a paged list under `field`, following `meta.pagination.next_page`.
    pub async fn list(
        &self,
        path: &str,
        query: &[(&str, String)],
        field: &str,
    ) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut page: u64 = 1;
        for _ in 0..MAX_PAGES {
            let mut q = query.to_vec();
            q.push(("page", page.to_string()));
            q.push(("per_page", PAGE_SIZE.to_string()));
            let value = self.get(path, &q).await?;
            let found = value[field].as_array().ok_or_else(|| {
                Error::provider(format!(
                    "{RUNTIME}: GET {path}: the response has no {field}"
                ))
            })?;
            items.extend(found.iter().cloned());
            match value["meta"]["pagination"]["next_page"].as_u64() {
                Some(next) if next > page => page = next,
                _ => return Ok(items),
            }
        }
        Err(Error::provider(format!(
            "{RUNTIME}: GET {path}: more than {MAX_PAGES} pages"
        )))
    }
}

/// Percent-encodes a query component; letters, digits and `-_.~` stay.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ciburst_core::http::Response;
    use serde_json::json;
    use std::sync::Mutex;

    const TOKEN: &str = "hcloud-token-under-test";

    /// Answers each request with the next canned response and keeps the requests.
    struct Canned {
        responses: Mutex<Vec<Response>>,
        seen: Mutex<Vec<Request>>,
    }

    impl Canned {
        fn new(responses: Vec<Response>) -> Arc<Canned> {
            Arc::new(Canned {
                responses: Mutex::new(responses),
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl HttpClient for Canned {
        async fn send(&self, request: Request) -> Result<Response> {
            self.seen.lock().unwrap().push(request);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err(Error::Http(format!("connection reset, token {TOKEN}")));
            }
            Ok(responses.remove(0))
        }
    }

    #[test]
    fn encodes_query_components() {
        assert_eq!(
            encode("ciburst==true,ciburst-name==demo"),
            "ciburst%3D%3Dtrue%2Cciburst-name%3D%3Ddemo"
        );
        assert_eq!(encode("a b/ä~"), "a%20b%2F%C3%A4~");
    }

    #[tokio::test]
    async fn sends_the_bearer_token_and_json() {
        let http = Canned::new(vec![Response::json_body(201, &json!({ "server": {} }))]);
        let api = Api::new(http.clone(), "https://api.example.invalid/v1/", TOKEN);
        api.post("/servers", &json!({ "name": "x" })).await.unwrap();
        let seen = http.seen.lock().unwrap();
        let request = &seen[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "https://api.example.invalid/v1/servers");
        assert_eq!(
            request.header_value("authorization"),
            Some(&*format!("Bearer {TOKEN}"))
        );
        assert_eq!(
            request.header_value("content-type"),
            Some("application/json")
        );
        assert_eq!(request.body.as_deref(), Some(&b"{\"name\":\"x\"}"[..]));
    }

    #[tokio::test]
    async fn errors_carry_code_and_message_but_never_the_token() {
        let echo = json!({ "error": {
            "code": "unauthorized",
            "message": format!("unable to authenticate {TOKEN}"),
        }});
        let http = Canned::new(vec![
            Response::json_body(401, &echo),
            Response {
                status: 502,
                body: format!("<html>{TOKEN}</html>").into_bytes(),
            },
        ]);
        let api = Api::new(http, "https://api.example.invalid/v1", TOKEN);
        let first = api.get("/servers", &[]).await.unwrap_err().to_string();
        assert_eq!(
            first,
            "hetzner: GET /servers: unauthorized (401): unable to authenticate [redacted]"
        );
        let second = api.delete("/servers/1").await.unwrap_err().to_string();
        assert_eq!(second, "hetzner: DELETE /servers/1: status 502");
        let third = api.get("/servers", &[]).await.unwrap_err().to_string();
        assert!(third.starts_with("hetzner: GET /servers: http: connection reset"));
        for message in [first, second, third] {
            assert!(!message.contains(TOKEN), "{message}");
        }
    }

    #[tokio::test]
    async fn refuses_tokens_a_header_cannot_carry() {
        for token in ["", "with space", "line\nbreak"] {
            let http = Canned::new(Vec::new());
            let api = Api::new(http.clone(), "https://api.example.invalid/v1", token);
            let error = api.get("/servers", &[]).await.unwrap_err().to_string();
            assert_eq!(
                error,
                "hetzner: HCLOUD_TOKEN is empty or not a valid header value"
            );
            assert!(http.seen.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn lists_follow_next_page_and_stop_on_null() {
        let page = |items: Value, next: Value| {
            Response::json_body(
                200,
                &json!({ "servers": items, "meta": { "pagination": { "next_page": next } } }),
            )
        };
        let http = Canned::new(vec![
            page(json!([1, 2]), json!(3)),
            page(json!([3]), json!(null)),
        ]);
        let api = Api::new(http.clone(), "https://api.example.invalid/v1", TOKEN);
        let items = api
            .list("/servers", &[("label_selector", "a==b".into())], "servers")
            .await
            .unwrap();
        assert_eq!(items, vec![json!(1), json!(2), json!(3)]);
        let urls: Vec<String> = http
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.url.clone())
            .collect();
        assert_eq!(
            urls,
            [
                "https://api.example.invalid/v1/servers?label_selector=a%3D%3Db&page=1&per_page=50",
                "https://api.example.invalid/v1/servers?label_selector=a%3D%3Db&page=3&per_page=50",
            ]
        );

        let looping = Canned::new(vec![page(json!([1]), json!(1))]);
        let api = Api::new(looping, "https://api.example.invalid/v1", TOKEN);
        assert_eq!(
            api.list("/servers", &[], "servers").await.unwrap(),
            vec![json!(1)]
        );
        let missing = Canned::new(vec![Response::json_body(200, &json!({ "meta": {} }))]);
        let api = Api::new(missing, "https://api.example.invalid/v1", TOKEN);
        assert_eq!(
            api.list("/servers", &[], "servers")
                .await
                .unwrap_err()
                .to_string(),
            "hetzner: GET /servers: the response has no servers"
        );
    }

    #[test]
    fn capacity_codes_allow_the_next_type() {
        let error = |code: &str| ApiError {
            status: 412,
            code: code.into(),
            message: String::new(),
        };
        for code in [
            "resource_unavailable",
            "resource_limit_exceeded",
            "placement_error",
        ] {
            assert!(error(code).is_capacity(), "{code}");
        }
        for code in [
            "invalid_input",
            "uniqueness_error",
            "rate_limit_exceeded",
            "",
        ] {
            assert!(!error(code).is_capacity(), "{code}");
        }
    }
}
