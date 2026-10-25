//! Fake of the Object Storage S3 endpoint, path-style (`s3.<region>.scw.cloud/<bucket>/<key>`)
//! and virtual-hosted (`<bucket>.s3.<region>.scw.cloud/<key>`): bucket create, head, lifecycle,
//! ListObjectsV2, object put, head, get and delete, bucket delete.
//!
//! Signed requests: the SigV4 authorization header is verified against the owner key and the
//! keys added with [`FakeS3::add_key`] (`403 SignatureDoesNotMatch` otherwise), and every
//! `x-amz-*` header must be among the signed headers (`403 AccessDenied`, as S3 answers).
//! Lifecycle writes need `Content-MD5` as Scaleway documents, object writes a `Content-Length`
//! as S3 does. A PutObject may carry a canned ACL in `x-amz-acl`; unknown ACLs are refused.
//!
//! Buckets belong to a project, as on Scaleway: a bucket is created in the signing key's
//! preferred project, or in the project the access key names as `<access key>@<project>`; a
//! request for a bucket of another project is `403 AccessDenied`. The owner key's preferred
//! project is [`FAKE_PREFERRED_PROJECT`]; keys added with [`FakeS3::add_key`] reach every
//! bucket, keys added with [`FakeS3::add_key_in`] only their project's.
//!
//! Anonymous requests (no authorization header) may only GET or HEAD an object whose ACL is
//! `public-read` or `public-read-write`; anything else, a missing key included, is `403
//! AccessDenied`, as for a reader without list permission.
//!
//! Every object write stamps `LastModified` with the fake's clock, which then moves one second
//! on. ListObjectsV2 pages hold [`DEFAULT_PAGE`] keys unless `max-keys` asks for another size
//! (at most 1000, as on S3), so every caller meets pagination. The answer has the element order
//! and names of the AWS documentation; continuation tokens are opaque.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::http::{HttpClient, Request, Response};
use ciburst_core::time::DateTime;

use super::scaleway_fake::{Call, FAKE_ACCESS_KEY, FAKE_SECRET_KEY, Url, call_of, percent_decode};
use crate::provider::s3::{base64, content_md5, xml_escape, xml_values};
use crate::provider::sigv4;

/// Keys per ListObjectsV2 page without `max-keys`; S3 answers 1000.
pub const DEFAULT_PAGE: usize = 3;
/// The largest page S3 answers, whatever `max-keys` asks for.
pub const MAX_PAGE: usize = 1000;
/// The longest object key S3 accepts, in bytes.
pub const MAX_KEY_BYTES: usize = 1024;
/// 2026-10-24T00:00:00Z, the first `LastModified` until a test sets the clock.
pub const FAKE_CLOCK: u64 = 1_792_800_000;
/// The owner key's preferred project, where its buckets land without an `@<project>` suffix.
pub const FAKE_PREFERRED_PROJECT: &str = "fake-preferred-project";
/// The canned ACLs of the S3 API.
pub const CANNED_ACLS: [&str; 6] = [
    "private",
    "public-read",
    "public-read-write",
    "authenticated-read",
    "bucket-owner-read",
    "bucket-owner-full-control",
];

const TOKEN_PREFIX: &str = "fake-token:";

#[derive(Debug, Clone)]
struct Object {
    data: Vec<u8>,
    acl: String,
    modified: u64,
}

impl Object {
    fn public(&self) -> bool {
        matches!(self.acl.as_str(), "public-read" | "public-read-write")
    }
}

#[derive(Debug, Clone, Default)]
struct Bucket {
    /// `None` when an unscoped key created it.
    project: Option<String>,
    lifecycle: Option<String>,
    objects: BTreeMap<String, Object>,
}

struct Inner {
    buckets: BTreeMap<String, Bucket>,
    calls: Vec<Call>,
    /// Further access keys and their secrets, besides the owner key.
    keys: BTreeMap<String, String>,
    /// Preferred projects of the further keys that have one.
    key_projects: BTreeMap<String, String>,
    page: Option<usize>,
    clock: u64,
}

impl Inner {
    fn tick(&mut self) -> u64 {
        let now = self.clock;
        self.clock += 1;
        now
    }
}

pub struct FakeS3 {
    inner: Mutex<Inner>,
}

impl Default for FakeS3 {
    fn default() -> Self {
        FakeS3::new()
    }
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

fn access_denied() -> Response {
    s3_error(403, "AccessDenied", "Access Denied")
}

/// `LastModified` as S3 writes it: `2026-10-24T00:00:00.000Z`.
fn last_modified(unix: u64) -> String {
    let at = DateTime::from_unix(unix);
    format!("{}T{}.000Z", at.date(), at.time())
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

/// The region and, for a virtual-hosted request, the bucket of an S3 host; `None` for any
/// other host.
fn split_host(host: &str) -> Option<(Option<String>, String)> {
    let rest = host.strip_suffix(".scw.cloud")?;
    if let Some(region) = rest.strip_prefix("s3.") {
        return Some((None, region.to_string()));
    }
    let (bucket, region) = rest.split_once(".s3.")?;
    (!bucket.is_empty()).then(|| (Some(bucket.to_string()), region.to_string()))
}

/// Bucket and key of a request: from the host and path when virtual-hosted, from the path when
/// path-style.
fn target(url: &Url, virtual_bucket: Option<String>) -> (String, Option<String>) {
    let path = url.path.trim_start_matches('/');
    match virtual_bucket {
        Some(bucket) => (bucket, Some(percent_decode(path)).filter(|k| !k.is_empty())),
        None => match path.split_once('/') {
            Some((bucket, key)) if !key.is_empty() => {
                (percent_decode(bucket), Some(percent_decode(key)))
            }
            Some((bucket, _)) => (percent_decode(bucket), None),
            None => (percent_decode(path), None),
        },
    }
}

impl FakeS3 {
    pub fn new() -> FakeS3 {
        FakeS3 {
            inner: Mutex::new(Inner {
                buckets: BTreeMap::new(),
                calls: Vec::new(),
                keys: BTreeMap::new(),
                key_projects: BTreeMap::new(),
                page: None,
                clock: FAKE_CLOCK,
            }),
        }
    }

    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().expect("fake s3").calls.clone()
    }

    /// Accepts requests signed with this key pair too, for buckets of every project.
    pub fn add_key(&self, access_key: &str, secret_key: &str) {
        self.inner
            .lock()
            .expect("fake s3")
            .keys
            .insert(access_key.to_string(), secret_key.to_string());
    }

    /// Accepts requests signed with this key pair, whose preferred project is `project`.
    pub fn add_key_in(&self, access_key: &str, secret_key: &str, project: &str) {
        let mut inner = self.inner.lock().expect("fake s3");
        inner
            .keys
            .insert(access_key.to_string(), secret_key.to_string());
        inner
            .key_projects
            .insert(access_key.to_string(), project.to_string());
    }

    /// The project a bucket was created in.
    pub fn bucket_project(&self, bucket: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("fake s3")
            .buckets
            .get(bucket)?
            .project
            .clone()
    }

    /// Keys per ListObjectsV2 page without `max-keys`, [`DEFAULT_PAGE`] unless set.
    pub fn set_page_size(&self, keys: usize) {
        self.inner.lock().expect("fake s3").page = Some(keys.clamp(1, MAX_PAGE));
    }

    /// Unix seconds the next object write stamps as `LastModified`.
    pub fn set_clock(&self, unix: u64) {
        self.inner.lock().expect("fake s3").clock = unix;
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

    /// Writes a private object, as a job's cache upload would.
    pub fn put_object(&self, bucket: &str, key: &str, data: &[u8]) {
        let mut inner = self.inner.lock().expect("fake s3");
        let modified = inner.tick();
        if let Some(b) = inner.buckets.get_mut(bucket) {
            b.objects.insert(
                key.to_string(),
                Object {
                    data: data.to_vec(),
                    acl: "private".to_string(),
                    modified,
                },
            );
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
                    .map(|(k, v)| (k.clone(), v.data.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The canned ACL an object was written with.
    pub fn acl(&self, bucket: &str, key: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("fake s3")
            .buckets
            .get(bucket)?
            .objects
            .get(key)
            .map(|o| o.acl.clone())
    }

    /// The project the request acts in when it is signed correctly (`None` for a key that
    /// reaches every project), else the error response.
    fn verify(
        inner: &Inner,
        request: &Request,
        url: &Url,
        region: &str,
    ) -> std::result::Result<Option<String>, Response> {
        let denied = |code: &str| Err(s3_error(403, code, "signature check failed"));
        let Some(authorization) = request.header_value("authorization") else {
            return denied("AccessDenied");
        };
        let Some((access_key, scope, signed, signature)) =
            sigv4::parse_authorization(authorization)
        else {
            return denied("AuthorizationHeaderMalformed");
        };
        let (access_key, named_project) = match access_key.split_once('@') {
            Some((key, project)) => (key.to_string(), Some(project.to_string())),
            None => (access_key, None),
        };
        let (secret_key, preferred) = if access_key == FAKE_ACCESS_KEY {
            (FAKE_SECRET_KEY, Some(FAKE_PREFERRED_PROJECT.to_string()))
        } else if let Some(secret_key) = inner.keys.get(&access_key) {
            (
                secret_key.as_str(),
                inner.key_projects.get(&access_key).cloned(),
            )
        } else {
            return denied("InvalidAccessKeyId");
        };
        let project = named_project.or(preferred);
        let Some(amz_date) = request.header_value("x-amz-date") else {
            return denied("AccessDenied");
        };
        if amz_date.len() != 16 || scope != sigv4::scope(amz_date, region) {
            return denied("AuthorizationHeaderMalformed");
        }
        let body = request.body.clone().unwrap_or_default();
        let payload_hash = sigv4::sha256_hex(&body);
        if request.header_value("x-amz-content-sha256") != Some(payload_hash.as_str()) {
            return Err(s3_error(400, "XAmzContentSHA256Mismatch", "payload hash"));
        }
        let signed_names: BTreeSet<&str> = signed.split(';').collect();
        let unsigned = request.headers.iter().any(|(name, _)| {
            let name = name.to_ascii_lowercase();
            name.starts_with("x-amz-") && !signed_names.contains(name.as_str())
        });
        if unsigned {
            return Err(s3_error(
                403,
                "AccessDenied",
                "There were headers present in the request which were not signed",
            ));
        }
        let mut headers = Vec::new();
        for name in &signed_names {
            let value = if *name == "host" {
                Some(url.host.as_str())
            } else {
                request.header_value(name)
            };
            let Some(value) = value else {
                return denied("AccessDenied");
            };
            headers.push((name.to_string(), value.to_string()));
        }
        if !signed_names.contains("host") {
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
        Ok(project)
    }
}

/// `PUT <key>`: a length is required, the HTTP client derives it from a non-empty body and
/// sends none for an empty one; `x-amz-acl` must name a canned ACL.
fn put_object(inner: &mut Inner, bucket: &str, key: String, request: &Request) -> Response {
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
    let acl = request.header_value("x-amz-acl").unwrap_or("private");
    if !CANNED_ACLS.contains(&acl) {
        return s3_error(400, "InvalidArgument", "Invalid canned ACL");
    }
    let acl = acl.to_string();
    let modified = inner.tick();
    let Some(b) = inner.buckets.get_mut(bucket) else {
        return no_bucket();
    };
    b.objects.insert(
        key,
        Object {
            data: body,
            acl,
            modified,
        },
    );
    Response::empty(200)
}

fn no_bucket() -> Response {
    s3_error(404, "NoSuchBucket", "The specified bucket does not exist")
}

/// `PUT ?lifecycle`: every rule has a unique id, a status and an expiration of at least a day.
fn put_lifecycle(bucket: &mut Bucket, request: &Request) -> Response {
    let body = request.body.clone().unwrap_or_default();
    if request.header_value("content-md5") != Some(content_md5(&body).as_str()) {
        return s3_error(400, "InvalidDigest", "Content-MD5 required");
    }
    let text = String::from_utf8_lossy(&body).into_owned();
    let rules = xml_values(&text, "Rule");
    if !text.contains("<LifecycleConfiguration") || rules.is_empty() || rules.len() > 1000 {
        return s3_error(400, "MalformedXML", "lifecycle document");
    }
    let mut ids = BTreeSet::new();
    for rule in &rules {
        let id = xml_values(rule, "ID")
            .into_iter()
            .next()
            .unwrap_or_default();
        let status = xml_values(rule, "Status").into_iter().next();
        let days = xml_values(rule, "Expiration")
            .into_iter()
            .next()
            .and_then(|e| xml_values(&e, "Days").into_iter().next())
            .and_then(|d| d.parse::<u32>().ok());
        if id.len() > 255 || !ids.insert(id) {
            return s3_error(400, "InvalidArgument", "Rule ID must be unique");
        }
        if !matches!(status.as_deref(), Some("Enabled" | "Disabled")) {
            return s3_error(400, "MalformedXML", "rule status");
        }
        if days.is_none_or(|d| d < 1) {
            return s3_error(
                400,
                "InvalidArgument",
                "'Days' for Expiration action must be a positive integer",
            );
        }
    }
    bucket.lifecycle = Some(text);
    Response::empty(200)
}

/// `GET ?list-type=2`: keys below `prefix`, after `start-after` or, when given, after the key
/// the `continuation-token` names, ascending, `max-keys` per page.
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
    let page: Vec<(&String, &Object)> = matching.by_ref().take(max_keys).collect();
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
    for (key, object) in &page {
        out.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>{}</LastModified>\
             <ETag>&quot;{}&quot;</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass>\
             </Contents>",
            xml_escape(key),
            last_modified(object.modified),
            sigv4::sha256_hex(&object.data)
                .get(..32)
                .unwrap_or_default(),
            object.data.len()
        ));
    }
    out.push_str("</ListBucketResult>");
    xml(200, out)
}

/// GET or HEAD of an object; a HEAD answer has no body.
fn read_object(object: &Object, method: &str) -> Response {
    if method == "HEAD" {
        Response::empty(200)
    } else {
        Response {
            status: 200,
            body: object.data.clone(),
        }
    }
}

#[async_trait]
impl HttpClient for FakeS3 {
    async fn send(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        let mut inner = self.inner.lock().expect("fake s3");
        inner.calls.push(call_of(&request, &url));
        let Some((virtual_bucket, region)) = split_host(&url.host) else {
            return Ok(s3_error(400, "InvalidURI", "not an S3 host"));
        };
        let (bucket, key) = target(&url, virtual_bucket);
        let method = request.method.as_str();
        if request.header_value("authorization").is_none() {
            let object = key
                .as_ref()
                .and_then(|key| inner.buckets.get(&bucket)?.objects.get(key));
            return Ok(match object {
                Some(object)
                    if matches!(method, "GET" | "HEAD")
                        && url.query.is_empty()
                        && object.public() =>
                {
                    read_object(object, method)
                }
                _ => access_denied(),
            });
        }
        let project = match FakeS3::verify(&inner, &request, &url, &region) {
            Ok(project) => project,
            Err(denied) => return Ok(denied),
        };
        if bucket.is_empty() {
            return Ok(s3_error(405, "MethodNotAllowed", "service level request"));
        }
        let elsewhere = inner.buckets.get(&bucket).is_some_and(
            |b| matches!((&project, &b.project), (Some(mine), Some(theirs)) if mine != theirs),
        );
        if elsewhere {
            let create = method == "PUT" && key.is_none() && url.query.is_empty();
            return Ok(if create {
                s3_error(409, "BucketAlreadyExists", "bucket name taken")
            } else {
                access_denied()
            });
        }
        let has = |name: &str| url.query.iter().any(|(k, _)| k == name);
        let page = inner.page.unwrap_or(DEFAULT_PAGE);
        match (method, key) {
            ("PUT", None) if has("lifecycle") => match inner.buckets.get_mut(&bucket) {
                None => Ok(no_bucket()),
                Some(b) => Ok(put_lifecycle(b, &request)),
            },
            ("PUT", None) => {
                if inner.buckets.contains_key(&bucket) {
                    return Ok(s3_error(409, "BucketAlreadyOwnedByYou", "bucket exists"));
                }
                inner.buckets.insert(
                    bucket,
                    Bucket {
                        project,
                        ..Bucket::default()
                    },
                );
                Ok(Response::empty(200))
            }
            ("PUT", Some(key)) if url.query.is_empty() => {
                Ok(put_object(&mut inner, &bucket, key, &request))
            }
            ("HEAD", None) => Ok(if inner.buckets.contains_key(&bucket) {
                Response::empty(200)
            } else {
                Response::empty(404)
            }),
            ("GET" | "HEAD", Some(key)) if url.query.is_empty() => {
                let Some(b) = inner.buckets.get(&bucket) else {
                    return Ok(if method == "HEAD" {
                        Response::empty(404)
                    } else {
                        no_bucket()
                    });
                };
                Ok(match b.objects.get(&key) {
                    Some(object) => read_object(object, method),
                    None if method == "HEAD" => Response::empty(404),
                    None => s3_error(404, "NoSuchKey", "The specified key does not exist."),
                })
            }
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

    #[test]
    fn hosts_name_the_region_and_the_virtual_bucket() {
        assert_eq!(
            split_host("s3.fr-par.scw.cloud"),
            Some((None, "fr-par".into()))
        );
        assert_eq!(
            split_host("demo-cache-p.s3.nl-ams.scw.cloud"),
            Some((Some("demo-cache-p".into()), "nl-ams".into()))
        );
        for other in [
            "api.scaleway.com",
            ".s3.fr-par.scw.cloud",
            "s3.fr-par.example",
        ] {
            assert_eq!(split_host(other), None, "{other}");
        }
        assert_eq!(last_modified(FAKE_CLOCK + 1), "2026-10-24T00:00:01.000Z");
    }
}
