//! The runner's way to GitLab (D67, spec 6.2): a reverse proxy on the Docker bridge that
//! forwards every request to `gitlab.url` with bodies streamed both ways, and drops
//! `last_update` from job requests so GitLab answers each one at once instead of holding it.

use std::io;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{Request, State};
use axum::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use ciburst_core::log::Log;
use futures_util::StreamExt;
use tokio::net::TcpListener;

/// The proxy's port on the bridge address.
pub const PORT: u16 = 8780;
/// The runner's job request; Workhorse holds it while its body carries `last_update`.
pub const JOB_REQUEST: &str = "/api/v4/jobs/request";
/// A job request body is a few kB of JSON.
const JOB_REQUEST_MAX: usize = 1 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Headers of one connection only (RFC 9110 section 7.6.1), plus `Host`, which names the
/// proxy, and `Expect`, which the proxy's own server already answered.
const HOP_BY_HOP: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "expect",
];

#[derive(Clone)]
struct Upstream {
    /// `gitlab.url` without a trailing slash.
    base: String,
    client: reqwest::Client,
    log: Log,
}

/// The client towards GitLab: redirects go back to the runner, which follows them itself (an
/// artifact download answers with a redirect to object storage); no timeout, an artifact
/// transfer may take long; no proxy from the environment, as for the runner.
pub fn client() -> io::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .no_proxy()
        .build()
        .map_err(io::Error::other)
}

/// Serves `listener` until the process ends.
pub async fn serve(listener: TcpListener, base: &str, log: Log) -> io::Result<()> {
    let upstream = Upstream {
        base: base.trim_end_matches('/').to_string(),
        client: client()?,
        log,
    };
    let app = Router::new().fallback(forward).with_state(upstream);
    axum::serve(listener, app).await
}

async fn forward(State(upstream): State<Upstream>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_string();
    let target = match parts.uri.path_and_query() {
        Some(path_and_query) => format!("{}{}", upstream.base, path_and_query.as_str()),
        None => format!("{}{path}", upstream.base),
    };
    let mut headers = end_to_end(&parts.headers);
    let fail = |error: &str| {
        upstream.log.warn(
            "forward failed",
            &[
                ("method", parts.method.as_str()),
                ("path", &path),
                ("error", error),
            ],
        );
    };
    let mut outgoing = upstream.client.request(parts.method.clone(), target);
    if parts.method == Method::POST && path == JOB_REQUEST {
        let bytes = match axum::body::to_bytes(body, JOB_REQUEST_MAX).await {
            Ok(bytes) => without_last_update(bytes),
            Err(error) => {
                fail(&chain(&error));
                return StatusCode::BAD_REQUEST.into_response();
            }
        };
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
        outgoing = outgoing.body(bytes);
    } else if body.size_hint().exact() != Some(0) {
        // `Content-Length`, when the runner sent one, stays and fixes the framing.
        outgoing = outgoing.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }
    let answer = match outgoing.headers(headers).send().await {
        Ok(answer) => answer,
        Err(error) => {
            fail(&chain(&error.without_url()));
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let status = answer.status();
    let headers = end_to_end(answer.headers());
    let (log, method) = (upstream.log.clone(), parts.method.clone());
    let stream = answer.bytes_stream().map(move |chunk| {
        chunk.map_err(|error| {
            let error = error.without_url();
            log.warn(
                "forward failed",
                &[
                    ("method", method.as_str()),
                    ("path", &path),
                    ("error", &chain(&error)),
                ],
            );
            error
        })
    });
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// `headers` without the hop-by-hop ones and those the `Connection` header names.
fn end_to_end(headers: &HeaderMap) -> HeaderMap {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if name == header::HOST || HOP_BY_HOP.contains(&name.as_str()) || named.contains(name) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// The job request body without `last_update`; any other body unchanged.
fn without_last_update(body: Bytes) -> Bytes {
    let Ok(serde_json::Value::Object(mut fields)) = serde_json::from_slice(&body) else {
        return body;
    };
    if fields.remove("last_update").is_none() {
        return body;
    }
    serde_json::to_vec(&fields).map_or(body, Bytes::from)
}

/// An error with its causes, `a: b: c`.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use axum::http::Uri;
    use serde_json::{Value, json};

    use super::*;

    /// A request as the fake GitLab received it.
    #[derive(Clone)]
    struct Seen {
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    }

    /// GitLab behind Workhorse, as strict as the runner's endpoints need: a job request whose
    /// JSON carries `last_update` is held for `hold` (about 50 s on gitlab.com, see
    /// `internal/builds/register.go`), one without it is answered at once with 204.
    #[derive(Clone)]
    struct FakeGitLab {
        hold: Duration,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    const BIG: usize = 8 << 20;

    fn big_body() -> Vec<u8> {
        (0..BIG).map(|i| (i * 31 % 251) as u8).collect()
    }

    async fn gitlab(State(fake): State<FakeGitLab>, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let body = axum::body::to_bytes(body, 2 * BIG).await.unwrap();
        fake.seen.lock().unwrap().push(Seen {
            method: parts.method.clone(),
            uri: parts.uri.clone(),
            headers: parts.headers.clone(),
            body: body.clone(),
        });
        match (parts.method, parts.uri.path()) {
            (Method::POST, JOB_REQUEST) => {
                let request: Value = serde_json::from_slice(&body).unwrap();
                if request.get("last_update").is_some() {
                    tokio::time::sleep(fake.hold).await;
                }
                StatusCode::NO_CONTENT.into_response()
            }
            (Method::PATCH, "/api/v4/jobs/5/trace") => (
                StatusCode::ACCEPTED,
                [
                    ("Range", "0-1023"),
                    ("Job-Status", "running"),
                    ("X-GitLab-Trace-Update-Interval", "30"),
                ],
            )
                .into_response(),
            (Method::POST, "/api/v4/jobs/5/artifacts") => (
                StatusCode::CREATED,
                [("Content-Type", "application/octet-stream")],
                body,
            )
                .into_response(),
            (Method::GET, "/api/v4/jobs/5/artifacts") => (
                StatusCode::FOUND,
                [("Location", "https://storage.example.invalid/artifacts.zip")],
            )
                .into_response(),
            (Method::GET, "/big") => Bytes::from(big_body()).into_response(),
            _ => (StatusCode::NOT_FOUND, "404 Not Found").into_response(),
        }
    }

    async fn spawn_gitlab(hold: Duration) -> (String, FakeGitLab) {
        let fake = FakeGitLab {
            hold,
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new().fallback(gitlab).with_state(fake.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/"), fake)
    }

    async fn spawn_proxy(base: &str) -> (String, Arc<Mutex<Vec<String>>>) {
        let (log, lines) = Log::memory();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        let base = base.to_string();
        tokio::spawn(async move { serve(listener, &base, log).await.unwrap() });
        (format!("http://{address}"), lines)
    }

    fn runner() -> reqwest::Client {
        client().unwrap()
    }

    fn job_request() -> Value {
        json!({
            "token": "glrt-runner-secret",
            "info": { "name": "gitlab-runner", "version": "18.11.4", "features": { "artifacts": true } },
            "last_update": "a1b2c3d4e5f6",
            "system_id": "s_0123456789ab"
        })
    }

    #[tokio::test]
    async fn the_fake_holds_a_job_request_with_last_update() {
        let (gitlab, fake) = spawn_gitlab(Duration::from_secs(2)).await;
        let begin = Instant::now();
        let answer = runner()
            .post(format!("{gitlab}api/v4/jobs/request"))
            .json(&job_request())
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status(), 204);
        assert!(begin.elapsed() >= Duration::from_secs(2));
        assert_eq!(fake.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_job_request_through_the_proxy_is_answered_at_once_without_last_update() {
        let (gitlab, fake) = spawn_gitlab(Duration::from_secs(2)).await;
        let (proxy, lines) = spawn_proxy(&gitlab).await;
        let begin = Instant::now();
        let answer = runner()
            .post(format!("{proxy}{JOB_REQUEST}"))
            .header("User-Agent", "gitlab-runner 18.11.4")
            .json(&job_request())
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status(), 204);
        assert!(
            begin.elapsed() < Duration::from_secs(1),
            "held for {:?}",
            begin.elapsed()
        );
        let seen = fake.seen.lock().unwrap()[0].clone();
        assert_eq!(seen.method, Method::POST);
        assert_eq!(seen.uri.path(), JOB_REQUEST);
        let mut expected = job_request();
        expected.as_object_mut().unwrap().remove("last_update");
        assert_eq!(
            serde_json::from_slice::<Value>(&seen.body).unwrap(),
            expected
        );
        assert_eq!(seen.headers["content-type"], "application/json");
        assert_eq!(seen.headers["user-agent"], "gitlab-runner 18.11.4");
        assert_eq!(
            seen.headers["content-length"],
            seen.body.len().to_string().as_str()
        );
        assert_eq!(
            seen.headers["host"],
            gitlab.trim_start_matches("http://").trim_end_matches('/'),
            "Host names GitLab"
        );
        assert!(lines.lock().unwrap().is_empty());
    }

    #[test]
    fn only_a_json_object_with_last_update_changes() {
        let plain = Bytes::from_static(br#"{"token":"t"}"#);
        assert_eq!(without_last_update(plain.clone()), plain);
        let other = Bytes::from_static(b"not json");
        assert_eq!(without_last_update(other.clone()), other);
        let list = Bytes::from_static(br#"[{"last_update":"x"}]"#);
        assert_eq!(without_last_update(list.clone()), list);
        assert_eq!(
            without_last_update(Bytes::from_static(br#"{"last_update":"x","token":"t"}"#)),
            Bytes::from_static(br#"{"token":"t"}"#)
        );
    }

    #[tokio::test]
    async fn other_requests_keep_method_path_query_and_headers() {
        let (gitlab, fake) = spawn_gitlab(Duration::from_secs(2)).await;
        let (proxy, _) = spawn_proxy(&gitlab).await;
        let answer = runner()
            .patch(format!("{proxy}/api/v4/jobs/5/trace?debug_trace=false"))
            .header("JOB-TOKEN", "glcbt-job-secret")
            .header("PRIVATE-TOKEN", "glpat-private-secret")
            .header("Content-Type", "text/plain")
            .header("Content-Range", "0-9")
            .body("0123456789")
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status(), 202);
        assert_eq!(answer.headers()["range"], "0-1023");
        assert_eq!(answer.headers()["job-status"], "running");
        assert_eq!(answer.headers()["x-gitlab-trace-update-interval"], "30");
        let seen = fake.seen.lock().unwrap()[0].clone();
        assert_eq!(seen.method, Method::PATCH);
        assert_eq!(
            seen.uri.path_and_query().unwrap().as_str(),
            "/api/v4/jobs/5/trace?debug_trace=false"
        );
        assert_eq!(seen.headers["job-token"], "glcbt-job-secret");
        assert_eq!(seen.headers["private-token"], "glpat-private-secret");
        assert_eq!(seen.headers["content-type"], "text/plain");
        assert_eq!(seen.headers["content-range"], "0-9");
        assert_eq!(seen.headers["content-length"], "10");
        assert_eq!(seen.body, "0123456789");
    }

    #[tokio::test]
    async fn statuses_and_redirects_pass_unchanged() {
        let (gitlab, _) = spawn_gitlab(Duration::from_secs(2)).await;
        let (proxy, lines) = spawn_proxy(&gitlab).await;
        let redirect = runner()
            .get(format!(
                "{proxy}/api/v4/jobs/5/artifacts?direct_download=true"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            redirect.status(),
            302,
            "the runner follows it, not the proxy"
        );
        assert_eq!(
            redirect.headers()["location"],
            "https://storage.example.invalid/artifacts.zip"
        );
        let missing = runner()
            .get(format!("{proxy}/nothing"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
        assert_eq!(missing.text().await.unwrap(), "404 Not Found");
        assert!(lines.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn large_bodies_stream_through_intact_both_ways() {
        let (gitlab, fake) = spawn_gitlab(Duration::from_secs(2)).await;
        let (proxy, _) = spawn_proxy(&gitlab).await;
        let body = big_body();

        let download = runner().get(format!("{proxy}/big")).send().await.unwrap();
        assert_eq!(download.status(), 200);
        assert_eq!(
            download.headers()["content-length"],
            BIG.to_string().as_str()
        );
        assert!(download.bytes().await.unwrap() == body);

        let sized = runner()
            .post(format!(
                "{proxy}/api/v4/jobs/5/artifacts?artifact_format=zip"
            ))
            .header("JOB-TOKEN", "glcbt-job-secret")
            .body(body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(sized.status(), 201);
        assert!(sized.bytes().await.unwrap() == body);

        let chunks: Vec<io::Result<Bytes>> = body
            .chunks(64 << 10)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        let chunked = runner()
            .post(format!("{proxy}/api/v4/jobs/5/artifacts"))
            .body(reqwest::Body::wrap_stream(futures_util::stream::iter(
                chunks,
            )))
            .send()
            .await
            .unwrap();
        assert_eq!(chunked.status(), 201);
        assert!(chunked.bytes().await.unwrap() == body);

        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].headers.get("transfer-encoding"), None);
        assert_eq!(seen[1].headers["content-length"], BIG.to_string().as_str());
        assert_eq!(seen[2].headers["transfer-encoding"], "chunked");
        assert!(seen[1].body == body && seen[2].body == body);
    }

    #[tokio::test]
    async fn a_failed_forward_answers_502_and_logs_no_secret() {
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = closed.local_addr().unwrap();
        drop(closed);
        let (proxy, lines) = spawn_proxy(&format!("http://{address}")).await;
        let answer = runner()
            .post(format!("{proxy}{JOB_REQUEST}?secret=query-secret"))
            .header("PRIVATE-TOKEN", "glpat-private-secret")
            .json(&job_request())
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status(), 502);
        let lines = lines.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert!(
            lines[0].starts_with(
                r#"warn forward failed method="POST" path="/api/v4/jobs/request" error=""#
            ),
            "{}",
            lines[0]
        );
        for secret in ["query-secret", "glpat-private-secret", "glrt-runner-secret"] {
            assert!(!lines[0].contains(secret), "{}", lines[0]);
        }
    }

    #[test]
    fn hop_by_hop_headers_and_host_stay_behind() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("host", "172.17.0.1:8780"),
            ("connection", "keep-alive, x-hop"),
            ("keep-alive", "timeout=5"),
            ("transfer-encoding", "chunked"),
            ("te", "trailers"),
            ("upgrade", "h2c"),
            ("expect", "100-continue"),
            ("x-hop", "1"),
            ("job-token", "glcbt-job-secret"),
            ("content-range", "0-9"),
            ("accept", "application/json"),
        ] {
            headers.append(name, HeaderValue::from_static(value));
        }
        headers.append("accept", HeaderValue::from_static("text/plain"));
        let kept = end_to_end(&headers);
        let names: Vec<&str> = kept.keys().map(|name| name.as_str()).collect();
        assert_eq!(names, ["job-token", "content-range", "accept"]);
        assert_eq!(kept.get_all("accept").iter().count(), 2);
    }
}
