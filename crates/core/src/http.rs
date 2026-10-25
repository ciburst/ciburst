use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;

use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// Replaces the client's timeout, for the transfer of a large file.
    pub timeout: Option<Duration>,
}

impl Request {
    pub fn new(method: &str, url: impl Into<String>) -> Request {
        Request {
            method: method.to_string(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: None,
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Request {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// A timeout of its own instead of the client's, from connecting until the response body
    /// has arrived.
    pub fn timeout(mut self, timeout: Duration) -> Request {
        self.timeout = Some(timeout);
        self
    }

    pub fn json(mut self, value: &serde_json::Value) -> Request {
        self.body = Some(value.to_string().into_bytes());
        self.header("content-type", "application/json")
    }

    pub fn header_value(&self, name: &str) -> Option<&str> {
        header_value(&self.headers, name)
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json_body(status: u16, value: &serde_json::Value) -> Response {
        Response {
            status,
            body: value.to_string().into_bytes(),
        }
    }

    pub fn empty(status: u16) -> Response {
        Response {
            status,
            body: Vec::new(),
        }
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

/// Response headers with lower-case names.
pub type Headers = Vec<(String, String)>;

/// The value of header `name` (any case) in `headers`.
pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The one HTTP seam of the core. Production uses [`ReqwestClient`]; tests use in-memory fakes.
#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn send(&self, request: Request) -> Result<Response>;

    /// `send` with the response headers, for paginated lists; a client without them
    /// answers none.
    async fn send_with_headers(&self, request: Request) -> Result<(Response, Headers)> {
        Ok((self.send(request).await?, Headers::new()))
    }
}

#[cfg(feature = "reqwest")]
pub struct ReqwestClient {
    client: reqwest::Client,
}

#[cfg(feature = "reqwest")]
impl ReqwestClient {
    pub fn new() -> Result<ReqwestClient> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("ciburst/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| Error::Http(error.to_string()))?;
        Ok(ReqwestClient { client })
    }
}

#[cfg(feature = "reqwest")]
#[async_trait]
impl HttpClient for ReqwestClient {
    async fn send(&self, request: Request) -> Result<Response> {
        Ok(self.send_with_headers(request).await?.0)
    }

    async fn send_with_headers(&self, request: Request) -> Result<(Response, Headers)> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|error| Error::Http(error.to_string()))?;
        let mut builder = self.client.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        if let Some(timeout) = request.timeout {
            builder = builder.timeout(timeout);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| Error::Http(error.to_string()))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
            })
            .collect();
        let body = response
            .bytes()
            .await
            .map_err(|error| Error::Http(error.to_string()))?
            .to_vec();
        Ok((Response { status, body }, headers))
    }
}
