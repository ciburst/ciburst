//! The cache bucket on the Object Storage S3 endpoint `s3.<region>.scw.cloud`, path-style,
//! signed with SigV4.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ciburst_core::http::{HttpClient, Request, Response};
use ciburst_core::{Error, Result};
use md5::{Digest, Md5};

use super::sigv4;

pub const LIFECYCLE_RULE_ID: &str = "ciburst-expire";

pub fn host(region: &str) -> String {
    format!("s3.{region}.scw.cloud")
}

pub struct S3 {
    pub http: Arc<dyn HttpClient>,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

/// One page of a ListObjectsV2 answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPage {
    pub keys: Vec<String>,
    /// `NextContinuationToken` of a truncated listing.
    pub next: Option<String>,
}

impl S3 {
    async fn send(
        &self,
        method: &str,
        bucket: &str,
        key: Option<&str>,
        query: &[(&str, &str)],
        body: Vec<u8>,
        extra_headers: &[(&str, String)],
    ) -> Result<(String, Response)> {
        let length = body.len();
        let host = host(&self.region);
        let mut path = format!("/{}", sigv4::uri_encode(bucket, true));
        if let Some(key) = key {
            path.push('/');
            path.push_str(&sigv4::uri_encode(key, false));
        }
        let query = sigv4::canonical_query(query);
        let mut url = format!("https://{host}{path}");
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query);
        }
        let payload_hash = sigv4::sha256_hex(&body);
        let amz_date = sigv4::amz_date(now_seconds());
        let mut headers = vec![
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        for (name, value) in extra_headers {
            headers.push((name.to_string(), value.clone()));
        }
        let mut signed = headers.clone();
        signed.push(("host".to_string(), host));
        let canonical = sigv4::canonical_request(method, &path, &query, &signed, &payload_hash);
        let signature = sigv4::signature(&self.secret_key, &amz_date, &self.region, &canonical);
        let authorization = sigv4::authorization(
            &self.access_key,
            &amz_date,
            &self.region,
            &sigv4::signed_headers(&signed),
            &signature,
        );
        let mut request = Request::new(method, url).header("authorization", authorization);
        for (name, value) in headers {
            request = request.header(&name, value);
        }
        // S3 answers 411 to an object PUT without a length, and the HTTP client sends none for
        // an empty body.
        if method == "PUT" && key.is_some() {
            request = request.header("content-length", length.to_string());
        }
        if !body.is_empty() {
            request.body = Some(body);
        }
        let response = self.http.send(request).await?;
        Ok((path, response))
    }

    async fn expect_ok(
        &self,
        method: &str,
        bucket: &str,
        key: Option<&str>,
        query: &[(&str, &str)],
        body: Vec<u8>,
        extra_headers: &[(&str, String)],
    ) -> Result<Response> {
        let (path, response) = self
            .send(method, bucket, key, query, body, extra_headers)
            .await?;
        if response.ok() {
            Ok(response)
        } else {
            Err(s3_error(method, &path, &response))
        }
    }

    pub async fn bucket_exists(&self, bucket: &str) -> Result<bool> {
        let (path, response) = self
            .send("HEAD", bucket, None, &[], Vec::new(), &[])
            .await?;
        match response.status {
            200..=299 => Ok(true),
            404 => Ok(false),
            _ => Err(s3_error("HEAD", &path, &response)),
        }
    }

    pub async fn create_bucket(&self, bucket: &str) -> Result<()> {
        self.expect_ok("PUT", bucket, None, &[], Vec::new(), &[])
            .await
            .map(drop)
    }

    /// Expiration days of the ciburst rule, `None` when the bucket has no such rule.
    pub async fn lifecycle_days(&self, bucket: &str) -> Result<Option<u32>> {
        let (path, response) = self
            .send("GET", bucket, None, &[("lifecycle", "")], Vec::new(), &[])
            .await?;
        match response.status {
            200..=299 => Ok(parse_lifecycle_days(&response.text())),
            404 => Ok(None),
            _ => Err(s3_error("GET", &path, &response)),
        }
    }

    pub async fn put_lifecycle(&self, bucket: &str, days: u32) -> Result<()> {
        let body = lifecycle_xml(days).into_bytes();
        let headers = [
            ("content-md5", content_md5(&body)),
            ("content-type", "application/xml".to_string()),
        ];
        self.expect_ok("PUT", bucket, None, &[("lifecycle", "")], body, &headers)
            .await
            .map(drop)
    }

    pub async fn object_keys(&self, bucket: &str) -> Result<Vec<String>> {
        let response = self
            .expect_ok("GET", bucket, None, &[("list-type", "2")], Vec::new(), &[])
            .await?;
        Ok(xml_values(&response.text(), "Key"))
    }

    pub async fn put_object(&self, bucket: &str, key: &str, body: Vec<u8>) -> Result<()> {
        self.expect_ok("PUT", bucket, Some(key), &[], body, &[])
            .await
            .map(drop)
    }

    /// One page of ListObjectsV2 below `prefix`, after the key `start_after`, continued at
    /// `token`. The page size is the endpoint's default.
    pub async fn list_page(
        &self,
        bucket: &str,
        prefix: &str,
        start_after: Option<&str>,
        token: Option<&str>,
    ) -> Result<ListPage> {
        let mut query = vec![("list-type", "2"), ("prefix", prefix)];
        if let Some(start_after) = start_after {
            query.push(("start-after", start_after));
        }
        if let Some(token) = token {
            query.push(("continuation-token", token));
        }
        let response = self
            .expect_ok("GET", bucket, None, &query, Vec::new(), &[])
            .await?;
        parse_list_page(&response.text())
    }

    pub async fn delete_object(&self, bucket: &str, key: &str) -> Result<()> {
        self.expect_ok("DELETE", bucket, Some(key), &[], Vec::new(), &[])
            .await
            .map(drop)
    }

    pub async fn delete_bucket(&self, bucket: &str) -> Result<()> {
        self.expect_ok("DELETE", bucket, None, &[], Vec::new(), &[])
            .await
            .map(drop)
    }

    /// Deletes every object, then the bucket. A listing returns at most 1000 keys, so it repeats.
    pub async fn empty_and_delete(&self, bucket: &str) -> Result<()> {
        loop {
            let keys = self.object_keys(bucket).await?;
            if keys.is_empty() {
                break;
            }
            for key in keys {
                self.delete_object(bucket, &key).await?;
            }
        }
        self.delete_bucket(bucket).await
    }
}

fn s3_error(method: &str, path: &str, response: &Response) -> Error {
    let text = response.text();
    let code = xml_values(&text, "Code").into_iter().next();
    let message = xml_values(&text, "Message").into_iter().next();
    let detail = match (code, message) {
        (Some(code), Some(message)) => format!("{code} {message}"),
        (Some(code), None) => code,
        (None, Some(message)) => message,
        (None, None) => text,
    };
    let message = format!("scaleway s3 {method} {path}: {} {detail}", response.status);
    Error::provider(message.trim_end())
}

pub fn lifecycle_xml(days: u32) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <LifecycleConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Rule><ID>{LIFECYCLE_RULE_ID}</ID><Filter><Prefix></Prefix></Filter>\
         <Status>Enabled</Status><Expiration><Days>{days}</Days></Expiration></Rule>\
         </LifecycleConfiguration>"
    )
}

/// The expiration days of the enabled ciburst rule in a lifecycle document.
pub fn parse_lifecycle_days(xml: &str) -> Option<u32> {
    xml_values(xml, "Rule").into_iter().find_map(|rule| {
        let ours = xml_values(&rule, "ID")
            .iter()
            .any(|id| id == LIFECYCLE_RULE_ID);
        let enabled = xml_values(&rule, "Status").iter().any(|s| s == "Enabled");
        if !(ours && enabled) {
            return None;
        }
        let expiration = xml_values(&rule, "Expiration").into_iter().next()?;
        xml_values(&expiration, "Days")
            .into_iter()
            .next()?
            .parse()
            .ok()
    })
}

/// Keys and continuation of a `ListBucketResult` document (ListObjectsV2).
pub fn parse_list_page(xml: &str) -> Result<ListPage> {
    if !xml.contains("<ListBucketResult") {
        return Err(Error::provider(
            "scaleway s3 list: answer without ListBucketResult",
        ));
    }
    let keys = xml_values(xml, "Contents")
        .iter()
        .filter_map(|contents| xml_values(contents, "Key").into_iter().next())
        .collect();
    let truncated = xml_values(xml, "IsTruncated").first().map(String::as_str) == Some("true");
    if !truncated {
        return Ok(ListPage { keys, next: None });
    }
    match xml_values(xml, "NextContinuationToken").into_iter().next() {
        Some(token) if !token.is_empty() => Ok(ListPage {
            keys,
            next: Some(token),
        }),
        _ => Err(Error::provider(
            "scaleway s3 list: truncated answer without NextContinuationToken",
        )),
    }
}

/// Text of every `<tag>...</tag>` element (no attributes, no nesting of the same tag), unescaped.
pub fn xml_values(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        out.push(xml_unescape(&after[..end]));
        rest = &after[end + close.len()..];
    }
    out
}

pub fn xml_unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

pub fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn content_md5(body: &[u8]) -> String {
    base64(&Md5::digest(body))
}

pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_document_round_trip() {
        let xml = lifecycle_xml(14);
        assert_eq!(
            xml,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><LifecycleConfiguration \
             xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Rule><ID>ciburst-expire</ID>\
             <Filter><Prefix></Prefix></Filter><Status>Enabled</Status><Expiration><Days>14</Days>\
             </Expiration></Rule></LifecycleConfiguration>"
        );
        assert_eq!(parse_lifecycle_days(&xml), Some(14));
        assert_eq!(
            parse_lifecycle_days(&xml.replace("Enabled", "Disabled")),
            None
        );
        assert_eq!(
            parse_lifecycle_days(&xml.replace("ciburst-expire", "other")),
            None
        );
        assert_eq!(parse_lifecycle_days("<LifecycleConfiguration/>"), None);
    }

    #[test]
    fn content_md5_is_base64_of_the_digest() {
        assert_eq!(content_md5(b""), "1B2M2Y8AsgTpgAmY7PhCfg==");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }

    #[test]
    fn reads_list_pages() {
        let page = parse_list_page(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Name>b</Name><Prefix>p/</Prefix><NextContinuationToken>t+/=</NextContinuationToken>\
             <KeyCount>2</KeyCount><MaxKeys>2</MaxKeys><IsTruncated>true</IsTruncated>\
             <Contents><Key>p/a&amp;b</Key><Size>0</Size></Contents>\
             <Contents><Key>p/c</Key><Size>0</Size></Contents></ListBucketResult>",
        )
        .unwrap();
        assert_eq!(page.keys, ["p/a&b", "p/c"]);
        assert_eq!(page.next.as_deref(), Some("t+/="));
        let last = parse_list_page(
            "<ListBucketResult><ContinuationToken>t</ContinuationToken><KeyCount>0</KeyCount>\
             <IsTruncated>false</IsTruncated></ListBucketResult>",
        )
        .unwrap();
        assert_eq!(
            last,
            ListPage {
                keys: Vec::new(),
                next: None
            }
        );
        assert!(
            parse_list_page("<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>")
                .is_err()
        );
        assert!(parse_list_page("<Error><Code>x</Code></Error>").is_err());
    }

    #[test]
    fn reads_xml_values() {
        let xml = "<R><Key>a&amp;b</Key><Key>c</Key></R>";
        assert_eq!(xml_values(xml, "Key"), ["a&b", "c"]);
        assert_eq!(xml_escape("a&b<"), "a&amp;b&lt;");
    }
}
