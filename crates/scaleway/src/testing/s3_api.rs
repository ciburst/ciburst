//! Fake of the Object Storage S3 endpoint (`s3.<region>.scw.cloud`), path-style: bucket create,
//! head, lifecycle, ListObjectsV2, object put, object and bucket delete. Verifies the SigV4
//! authorization header against the owner key and the keys added with [`FakeS3::add_key`] and
//! answers `403 SignatureDoesNotMatch` otherwise; lifecycle writes need `Content-MD5` as
//! Scaleway documents, object writes a `Content-Length` as S3 does.
//!
//! ListObjectsV2 pages hold [`DEFAULT_PAGE`] keys unless `max-keys` asks for another size (at
//! most 1000, as on S3), so every caller meets pagination. The answer has the element order and
//! names of the AWS documentation; continuation tokens are opaque.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::http::{HttpClient, Request, Response};

use super::scaleway_fake::{Call, FAKE_ACCESS_KEY, FAKE_SECRET_KEY, Url, call_of, percent_decode};
use crate::provider::s3::{base64, content_md5, xml_escape};
use crate::provider::sigv4;

/// Keys per ListObjectsV2 page without `max-keys`; S3 answers 1000.
pub const DEFAULT_PAGE: usize = 3;
/// The largest page S3 answers, whatever `max-keys` asks for.
pub const MAX_PAGE: usize = 1000;
/// The longest object key S3 accepts, in bytes.
pub const MAX_KEY_BYTES: usize = 1024;

const TOKEN_PREFIX: &str = "fake-token:";

#[derive(Debug, Clone, Default)]
struct Bucket {
    lifecycle: Option<String>,
    objects: BTreeMap<String, Vec<u8>>,
}

#[derive(Default)]
struct Inner {
    buckets: BTreeMap<String, Bucket>,
    calls: Vec<Call>,
    /// Further access keys and their secrets, besides the owner key.
    keys: BTreeMap<String, String>,
    page: Option<usize>,
}

#[derive(Default)]
pub struct FakeS3 {
    inner: Mutex<Inner>,
}

fn xml(status: u16, body: String) -> Response {
    Response {
        status,
        body: body.into_bytes(),
    }
}

fn s3_error(status: u16, code: &str, message: &str) -> Response {
    xml(
        status,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code>\
             <Message>{message}</Message></Error>"
        ),
    )
}

/// An opaque continuation token naming the last key of the page before.
fn token_for(last_key: &str) -> String {
    base64(format!("{TOKEN_PREFIX}{last_key}").as_bytes())
}

fn key_of_token(token: &str) -> Option<String> {
    let bytes = base64_decode(token)?;
    String::from_utf8(bytes)
        .ok()?
        .strip_prefix(TOKEN_PREFIX)
        .map(String::from)
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if text.is_empty() || !text.len().is_multiple_of(4) {
        return None;
    }
    let chunks: Vec<&[u8]> = text.as_bytes().chunks(4).collect();
    let mut out = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let mut n = 0u32;
        let mut padding = 0;
        for &byte in *chunk {
            let value = if byte == b'=' {
                padding += 1;
                0
            } else if padding > 0 {
                return None;
            } else {
                ALPHABET.iter().position(|&c| c == byte)? as u32
            };
            n = (n << 6) | value;
        }
        if padding > 2 || (padding > 0 && index + 1 != chunks.len()) {
            return None;
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - padding]);
    }
    Some(out)
}

impl FakeS3 {
    pub fn new() -> FakeS3 {
        FakeS3::default()
    }

    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().expect("fake s3").calls.clone()
    }

    /// Accepts requests signed with this key pair too, as the cache key's.
    pub fn add_key(&self, access_key: &str, secret_key: &str) {
        self.inner
            .lock()
            .expect("fake s3")
            .keys
            .insert(access_key.to_string(), secret_key.to_string());
    }

    /// Keys per ListObjectsV2 page without `max-keys`, [`DEFAULT_PAGE`] unless set.
    pub fn set_page_size(&self, keys: usize) {
        self.inner.lock().expect("fake s3").page = Some(keys.clamp(1, MAX_PAGE));
    }

    pub fn buckets(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("fake s3")
            .buckets
            .keys()
            .cloned()
            .collect()
    }

    pub fn lifecycle(&self, bucket: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("fake s3")
            .buckets
            .get(bucket)
            .and_then(|b| b.lifecycle.clone())
    }

    pub fn set_lifecycle(&self, bucket: &str, document: Option<String>) {
        if let Some(b) = self.inner.lock().expect("fake s3").buckets.get_mut(bucket) {
            b.lifecycle = document;
        }
    }

    pub fn put_object(&self, bucket: &str, key: &str, data: &[u8]) {
        if let Some(b) = self.inner.lock().expect("fake s3").buckets.get_mut(bucket) {
            b.objects.insert(key.to_string(), data.to_vec());
        }
    }

    /// Every object of `bucket` as key and content, sorted by key.
    pub fn objects(&self, bucket: &str) -> Vec<(String, Vec<u8>)> {
        self.inner
            .lock()
            .expect("fake s3")
            .buckets
            .get(bucket)
            .map(|b| {
                b.objects
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `None` when the request is signed correctly, else the error response.
    fn verify(keys: &BTreeMap<String, String>, request: &Request, url: &Url) -> Option<Response> {
        let denied = |code: &str| Some(s3_error(403, code, "signature check failed"));
        let Some(authorization) = request.header_value("authorization") else {
            return denied("AccessDenied");
        };
        let Some((access_key, scope, signed, signature)) =
            sigv4::parse_authorization(authorization)
        else {
            return denied("AuthorizationHeaderMalformed");
        };
        let secret_key = if access_key == FAKE_ACCESS_KEY {
            FAKE_SECRET_KEY
        } else if let Some(secret_key) = keys.get(&access_key) {
            secret_key.as_str()
        } else {
            return denied("InvalidAccessKeyId");
        };
        let Some(amz_date) = request.header_value("x-amz-date") else {
            return denied("AccessDenied");
        };
        let region = url
            .host
            .strip_prefix("s3.")
            .and_then(|h| h.strip_suffix(".scw.cloud"))
            .unwrap_or_default();
        if amz_date.len() != 16 || scope != sigv4::scope(amz_date, region) {
            return denied("AuthorizationHeaderMalformed");
        }
        let body = request.body.clone().unwrap_or_default();
        let payload_hash = sigv4::sha256_hex(&body);
        if request.header_value("x-amz-content-sha256") != Some(payload_hash.as_str()) {
            return Some(s3_error(400, "XAmzContentSHA256Mismatch", "payload hash"));
        }
        let mut headers = Vec::new();
        for name in signed.split(';') {
            let value = if name == "host" {
                Some(url.host.as_str())
            } else {
                request.header_value(name)
            };
            let Some(value) = value else {
                return denied("AccessDenied");
            };
            headers.push((name.to_string(), value.to_string()));
        }
        if !signed.split(';').any(|n| n == "host") {
            return denied("AccessDenied");
        }
        let canonical = sigv4::canonical_request(
            &request.method,
            &url.path,
            &sigv4::canonical_query_from_raw(&url.raw_query),
            &headers,
            &payload_hash,
        );
        let expected = sigv4::signature(secret_key, amz_date, region, &canonical);
        if expected != signature {
            return denied("SignatureDoesNotMatch");
        }
        None
    }
}

/// `PUT /<bucket>/<key>`: a length is required; the HTTP client derives it from a non-empty
/// body and sends none for an empty one.
fn put_object(bucket: &mut Bucket, key: String, request: &Request) -> Response {
    let body = request.body.clone().unwrap_or_default();
    match request.header_value("content-length") {
        None if body.is_empty() => {
            return s3_error(
                411,
                "MissingContentLength",
                "You must provide the Content-Length HTTP header.",
            );
        }
        Some(length) if length.parse::<usize>().ok() != Some(body.len()) => {
            return s3_error(
                400,
                "IncompleteBody",
                "You did not provide the number of bytes specified by the Content-Length HTTP header",
            );
        }
        _ => {}
    }
    if key.len() > MAX_KEY_BYTES {
        return s3_error(400, "KeyTooLongError", "Your key is too long");
    }
    bucket.objects.insert(key, body);
    Response::empty(200)
}

/// `GET /<bucket>?list-type=2`: keys below `prefix`, after `start-after` or, when given, after
/// the key the `continuation-token` names, ascending, `max-keys` per page.
fn list_objects(name: &str, bucket: &Bucket, url: &Url, default_page: usize) -> Response {
    let invalid = |message: &str| s3_error(400, "InvalidArgument", message);
    if url.get("delimiter").is_some() || url.get("encoding-type").is_some() {
        return s3_error(501, "NotImplemented", "not supported by the fake");
    }
    let prefix = url.get("prefix").unwrap_or_default();
    let start_after = url.get("start-after");
    let token = url.get("continuation-token");
    let max_keys = match url.get("max-keys") {
        None => default_page,
        Some(value) => match value.parse::<usize>() {
            Ok(n) => n.min(MAX_PAGE),
            Err(_) => return invalid("Provided max-keys not an integer or within integer range"),
        },
    };
    let after = match token {
        Some(token) => match key_of_token(token) {
            Some(key) => Some(key),
            None => return invalid("The continuation token provided is incorrect"),
        },
        None => start_after.map(String::from),
    };
    let mut matching = bucket
        .objects
        .iter()
        .filter(|(key, _)| key.starts_with(prefix))
        .filter(|(key, _)| after.as_deref().is_none_or(|after| key.as_str() > after));
    let page: Vec<(&String, &Vec<u8>)> = matching.by_ref().take(max_keys).collect();
    let truncated = max_keys > 0 && matching.next().is_some();
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Name>{}</Name><Prefix>{}</Prefix>",
        xml_escape(name),
        xml_escape(prefix)
    );
    if let Some(start_after) = start_after {
        out.push_str(&format!(
            "<StartAfter>{}</StartAfter>",
            xml_escape(start_after)
        ));
    }
    if let Some(token) = token {
        out.push_str(&format!(
            "<ContinuationToken>{}</ContinuationToken>",
            xml_escape(token)
        ));
    }
    if truncated && let Some((last, _)) = page.last() {
        out.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            xml_escape(&token_for(last))
        ));
    }
    out.push_str(&format!(
        "<KeyCount>{}</KeyCount><MaxKeys>{max_keys}</MaxKeys><IsTruncated>{truncated}</IsTruncated>",
        page.len()
    ));
    for (key, data) in &page {
        out.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>2026-10-24T00:00:00.000Z</LastModified>\
             <ETag>&quot;{}&quot;</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass>\
             </Contents>",
            xml_escape(key),
            sigv4::sha256_hex(data).get(..32).unwrap_or_default(),
            data.len()
        ));
    }
    out.push_str("</ListBucketResult>");
    xml(200, out)
}

#[async_trait]
impl HttpClient for FakeS3 {
    async fn send(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        let mut inner = self.inner.lock().expect("fake s3");
        inner.calls.push(call_of(&request, &url));
        if let Some(denied) = FakeS3::verify(&inner.keys, &request, &url) {
            return Ok(denied);
        }
        let path = url.path.trim_start_matches('/');
        let (bucket, key) = match path.split_once('/') {
            Some((bucket, key)) if !key.is_empty() => (bucket, Some(percent_decode(key))),
            Some((bucket, _)) => (bucket, None),
            None => (path, None),
        };
        if bucket.is_empty() {
            return Ok(s3_error(405, "MethodNotAllowed", "service level request"));
        }
        let bucket = percent_decode(bucket);
        let has = |name: &str| url.query.iter().any(|(k, _)| k == name);
        let method = request.method.as_str();
        let no_bucket = || s3_error(404, "NoSuchBucket", "The specified bucket does not exist");
        let page = inner.page.unwrap_or(DEFAULT_PAGE);
        match (method, key) {
            ("PUT", None) if has("lifecycle") => {
                let body = request.body.clone().unwrap_or_default();
                if request.header_value("content-md5") != Some(content_md5(&body).as_str()) {
                    return Ok(s3_error(400, "InvalidDigest", "Content-MD5 required"));
                }
                let Some(b) = inner.buckets.get_mut(&bucket) else {
                    return Ok(no_bucket());
                };
                let text = String::from_utf8_lossy(&body).into_owned();
                if !text.contains("<LifecycleConfiguration") || !text.contains("<Rule>") {
                    return Ok(s3_error(400, "MalformedXML", "lifecycle document"));
                }
                b.lifecycle = Some(text);
                Ok(Response::empty(200))
            }
            ("PUT", None) => {
                if inner.buckets.contains_key(&bucket) {
                    return Ok(s3_error(409, "BucketAlreadyOwnedByYou", "bucket exists"));
                }
                inner.buckets.insert(bucket, Bucket::default());
                Ok(Response::empty(200))
            }
            ("PUT", Some(key)) if url.query.is_empty() => match inner.buckets.get_mut(&bucket) {
                None => Ok(no_bucket()),
                Some(b) => Ok(put_object(b, key, &request)),
            },
            ("HEAD", None) => Ok(if inner.buckets.contains_key(&bucket) {
                Response::empty(200)
            } else {
                Response::empty(404)
            }),
            ("GET", None) if has("lifecycle") => match inner.buckets.get(&bucket) {
                None => Ok(no_bucket()),
                Some(Bucket {
                    lifecycle: Some(document),
                    ..
                }) => Ok(xml(200, document.clone())),
                Some(_) => Ok(s3_error(
                    404,
                    "NoSuchLifecycleConfiguration",
                    "The lifecycle configuration does not exist",
                )),
            },
            ("GET", None) if url.get("list-type") == Some("2") => {
                match inner.buckets.get(&bucket) {
                    None => Ok(no_bucket()),
                    Some(b) => Ok(list_objects(&bucket, b, &url, page)),
                }
            }
            ("DELETE", None) => match inner.buckets.get(&bucket) {
                None => Ok(no_bucket()),
                Some(b) if !b.objects.is_empty() => Ok(s3_error(
                    409,
                    "BucketNotEmpty",
                    "The bucket you tried to delete is not empty",
                )),
                Some(_) => {
                    inner.buckets.remove(&bucket);
                    Ok(Response::empty(204))
                }
            },
            ("DELETE", Some(key)) => {
                let Some(b) = inner.buckets.get_mut(&bucket) else {
                    return Ok(no_bucket());
                };
                b.objects.remove(&key);
                Ok(Response::empty(204))
            }
            _ => Ok(s3_error(
                405,
                "MethodNotAllowed",
                "not supported by the fake",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_and_reject_garbage() {
        for key in [
            "a",
            "ab",
            "abc",
            "ciburst/intents/vm/0000000001-00000000000000000001",
        ] {
            assert_eq!(key_of_token(&token_for(key)).as_deref(), Some(key));
        }
        for token in ["", "abc", "a===", "####", &base64(b"other")] {
            assert_eq!(key_of_token(token), None, "{token}");
        }
    }
}
