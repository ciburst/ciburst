//! Fake of a public download host such as GitLab's runner downloads bucket, an S3
//! bucket that anybody may read: GET and HEAD of a served file without credentials, `404
//! NoSuchKey` for any other key of a served host, `403 AccessDenied` for any other method, as
//! S3 answers an anonymous writer. A query string is refused like an unknown key.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::http::{HttpClient, Request, Response};

use super::scaleway_fake::{Call, Url, call_of};

struct Inner {
    /// Content by URL without query.
    files: BTreeMap<String, Vec<u8>>,
    calls: Vec<Call>,
    /// The timeout of every request, in order.
    timeouts: Vec<Option<std::time::Duration>>,
}

pub struct FakeDownloads {
    inner: Mutex<Inner>,
}

impl Default for FakeDownloads {
    fn default() -> Self {
        FakeDownloads::new()
    }
}

fn s3_error(status: u16, code: &str, message: &str) -> Response {
    Response {
        status,
        body: format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code>\
             <Message>{message}</Message></Error>"
        )
        .into_bytes(),
    }
}

impl FakeDownloads {
    pub fn new() -> FakeDownloads {
        FakeDownloads {
            inner: Mutex::new(Inner {
                files: BTreeMap::new(),
                calls: Vec::new(),
                timeouts: Vec::new(),
            }),
        }
    }

    /// Serves `bytes` at `url`, replacing what was there.
    pub fn serve(&self, url: &str, bytes: &[u8]) {
        self.inner
            .lock()
            .expect("fake downloads")
            .files
            .insert(url.to_string(), bytes.to_vec());
    }

    /// Whether a served URL lives on `host`.
    pub fn serves_host(&self, host: &str) -> bool {
        self.inner
            .lock()
            .expect("fake downloads")
            .files
            .keys()
            .any(|url| Url::parse(url).host == host)
    }

    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().expect("fake downloads").calls.clone()
    }

    /// The timeout each request carried, in the order of [`FakeDownloads::calls`].
    pub fn timeouts(&self) -> Vec<Option<std::time::Duration>> {
        self.inner.lock().expect("fake downloads").timeouts.clone()
    }
}

#[async_trait]
impl HttpClient for FakeDownloads {
    async fn send(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        let mut inner = self.inner.lock().expect("fake downloads");
        inner.calls.push(call_of(&request, &url));
        inner.timeouts.push(request.timeout);
        let method = request.method.as_str();
        if !matches!(method, "GET" | "HEAD") {
            return Ok(s3_error(403, "AccessDenied", "Access Denied"));
        }
        let file = (url.raw_query.is_empty())
            .then(|| inner.files.get(&request.url))
            .flatten();
        Ok(match file {
            Some(_) if method == "HEAD" => Response::empty(200),
            Some(bytes) => Response {
                status: 200,
                body: bytes.clone(),
            },
            None => s3_error(404, "NoSuchKey", "The specified key does not exist."),
        })
    }
}
