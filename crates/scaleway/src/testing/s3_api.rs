//! Fake of the Object Storage S3 endpoint (`s3.<region>.scw.cloud`), path-style: bucket create,
//! head, lifecycle, list, object and bucket delete. Verifies the SigV4 authorization header
//! against the owner key and answers `403 SignatureDoesNotMatch` otherwise; lifecycle writes
//! need `Content-MD5` as Scaleway documents.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::http::{HttpClient, Request, Response};

use super::scaleway_fake::{Call, FAKE_ACCESS_KEY, FAKE_SECRET_KEY, Url, call_of, percent_decode};
use crate::provider::s3::{content_md5, xml_escape};
use crate::provider::sigv4;

#[derive(Debug, Clone, Default)]
struct Bucket {
    lifecycle: Option<String>,
    objects: BTreeMap<String, Vec<u8>>,
}

#[derive(Default)]
struct Inner {
    buckets: BTreeMap<String, Bucket>,
    calls: Vec<Call>,
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

impl FakeS3 {
    pub fn new() -> FakeS3 {
        FakeS3::default()
    }

    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().expect("fake s3").calls.clone()
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

    /// `None` when the request is signed correctly, else the error response.
    fn verify(request: &Request, url: &Url) -> Option<Response> {
        let denied = |code: &str| Some(s3_error(403, code, "signature check failed"));
        let Some(authorization) = request.header_value("authorization") else {
            return denied("AccessDenied");
        };
        let Some((access_key, scope, signed, signature)) =
            sigv4::parse_authorization(authorization)
        else {
            return denied("AuthorizationHeaderMalformed");
        };
        if access_key != FAKE_ACCESS_KEY {
            return denied("InvalidAccessKeyId");
        }
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
        let expected = sigv4::signature(FAKE_SECRET_KEY, amz_date, region, &canonical);
        if expected != signature {
            return denied("SignatureDoesNotMatch");
        }
        None
    }
}

#[async_trait]
impl HttpClient for FakeS3 {
    async fn send(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        let mut inner = self.inner.lock().expect("fake s3");
        inner.calls.push(call_of(&request, &url));
        if let Some(denied) = FakeS3::verify(&request, &url) {
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
                let Some(b) = inner.buckets.get(&bucket) else {
                    return Ok(no_bucket());
                };
                let contents: String = b
                    .objects
                    .iter()
                    .take(1000)
                    .map(|(k, v)| {
                        format!(
                            "<Contents><Key>{}</Key><Size>{}</Size></Contents>",
                            xml_escape(k),
                            v.len()
                        )
                    })
                    .collect();
                Ok(xml(
                    200,
                    format!(
                        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult>\
                         <Name>{bucket}</Name><KeyCount>{}</KeyCount>\
                         <IsTruncated>{}</IsTruncated>{contents}</ListBucketResult>",
                        b.objects.len().min(1000),
                        b.objects.len() > 1000
                    ),
                ))
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
