//! The cache bucket on the Object Storage S3 endpoint `s3.<region>.scw.cloud`, path-style,
//! signed with SigV4. Public objects are read anonymously at the virtual-hosted URL
//! [`public_url`].

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ciburst_core::http::{HttpClient, Request, Response};
use ciburst_core::time::parse_rfc3339;
use ciburst_core::{Error, Result};
use md5::{Digest, Md5};

use super::sigv4;
use crate::ledger::INTENT_PREFIX;

/// The single rule of M4, which expired every object; a bucket that still has it gets the
/// rules of [`LIFECYCLE_RULES`] instead.
pub const LIFECYCLE_RULE_ID: &str = "ciburst-expire";

/// Rule id and prefix of every expiring part of the bucket (D38): the runner cache with
/// `--cache-shared` (`project/<id>/<key>`) and without (`runner/<token>/project/...`), and the
/// burst intents. The agent below `ciburst/agent/` never expires.
pub const LIFECYCLE_RULES: [(&str, &str); 3] = [
    ("ciburst-expire-project", "project/"),
    ("ciburst-expire-runner", "runner/"),
    ("ciburst-expire-intents", INTENT_PREFIX),
];

/// The canned ACL that lets anybody read an object without credentials.
pub const PUBLIC_READ: &str = "public-read";

pub fn host(region: &str) -> String {
    format!("s3.{region}.scw.cloud")
}

/// The anonymous, virtual-hosted URL of an object: `https://<bucket>.s3.<region>.scw.cloud/<key>`.
pub fn public_url(bucket: &str, region: &str, key: &str) -> String {
    format!(
        "https://{bucket}.{}/{}",
        host(region),
        sigv4::uri_encode(key, false)
    )
}

pub struct S3 {
    pub http: Arc<dyn HttpClient>,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

/// One object of a listing: its key and `LastModified` in Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub key: String,
    pub modified: Option<u64>,
}

/// One page of a ListObjectsV2 answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPage {
    pub objects: Vec<Listed>,
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

    /// Expiration days of the ciburst rules, `None` when the bucket lacks one of them or still
    /// has the rule of M4 ([`parse_lifecycle_days`]).
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

    /// PutObject with the canned ACL `public-read`: anybody may GET it without credentials.
    pub async fn put_public_object(&self, bucket: &str, key: &str, body: Vec<u8>) -> Result<()> {
        let headers = [
            ("content-type", "application/octet-stream".to_string()),
            ("x-amz-acl", PUBLIC_READ.to_string()),
        ];
        self.expect_ok("PUT", bucket, Some(key), &[], body, &headers)
            .await
            .map(drop)
    }

    /// HeadObject: whether `key` exists.
    pub async fn object_exists(&self, bucket: &str, key: &str) -> Result<bool> {
        let (path, response) = self
            .send("HEAD", bucket, Some(key), &[], Vec::new(), &[])
            .await?;
        match response.status {
            200..=299 => Ok(true),
            404 => Ok(false),
            _ => Err(s3_error("HEAD", &path, &response)),
        }
    }

    /// Every object below `prefix`, page after page, at most `max_pages` pages.
    pub async fn list_all(
        &self,
        bucket: &str,
        prefix: &str,
        max_pages: usize,
    ) -> Result<Vec<Listed>> {
        let mut objects = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..max_pages {
            let page = self
                .list_page(bucket, prefix, None, token.as_deref())
                .await?;
            objects.extend(page.objects);
            match page.next {
                Some(next) => token = Some(next),
                None => return Ok(objects),
            }
        }
        Err(Error::provider(format!(
            "scaleway s3 list {bucket}/{prefix}: more than {max_pages} pages"
        )))
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

/// The lifecycle document of [`LIFECYCLE_RULES`], each expiring after `days`.
pub fn lifecycle_xml(days: u32) -> String {
    let rules: String = LIFECYCLE_RULES
        .iter()
        .map(|(id, prefix)| {
            format!(
                "<Rule><ID>{id}</ID><Filter><Prefix>{}</Prefix></Filter><Status>Enabled</Status>\
                 <Expiration><Days>{days}</Days></Expiration></Rule>",
                xml_escape(prefix)
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <LifecycleConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">{rules}\
         </LifecycleConfiguration>"
    )
}

/// The expiration days of the ciburst rules in a lifecycle document: `Some` only when the
/// rules whose id starts with `ciburst-` are exactly those of [`LIFECYCLE_RULES`], enabled,
/// on their prefixes and with the same days. The single rule of M4 expired the agent too, so a
/// document that still has it reads as `None` and plans an update.
pub fn parse_lifecycle_days(xml: &str) -> Option<u32> {
    let ours: Vec<String> = xml_values(xml, "Rule")
        .into_iter()
        .filter(|rule| first(rule, "ID").is_some_and(|id| id.starts_with("ciburst-")))
        .collect();
    if ours.len() != LIFECYCLE_RULES.len() {
        return None;
    }
    let mut days = None;
    for (id, prefix) in LIFECYCLE_RULES {
        let rule = ours
            .iter()
            .find(|rule| first(rule, "ID").as_deref() == Some(id))?;
        let filter = first(rule, "Filter").unwrap_or_default();
        let found_prefix = first(&filter, "Prefix").or_else(|| first(rule, "Prefix"));
        if first(rule, "Status").as_deref() != Some("Enabled")
            || found_prefix.as_deref() != Some(prefix)
        {
            return None;
        }
        let expiration = first(rule, "Expiration")?;
        let rule_days: u32 = first(&expiration, "Days")?.parse().ok()?;
        if days.is_some_and(|d| d != rule_days) {
            return None;
        }
        days = Some(rule_days);
    }
    days
}

fn first(xml: &str, tag: &str) -> Option<String> {
    xml_values(xml, tag).into_iter().next()
}

/// Objects and continuation of a `ListBucketResult` document (ListObjectsV2).
pub fn parse_list_page(xml: &str) -> Result<ListPage> {
    if !xml.contains("<ListBucketResult") {
        return Err(Error::provider(
            "scaleway s3 list: answer without ListBucketResult",
        ));
    }
    let objects = xml_values(xml, "Contents")
        .iter()
        .filter_map(|contents| {
            Some(Listed {
                key: first(contents, "Key")?,
                modified: first(contents, "LastModified").and_then(|t| parse_rfc3339(&t)),
            })
        })
        .collect();
    let truncated = xml_values(xml, "IsTruncated").first().map(String::as_str) == Some("true");
    if !truncated {
        return Ok(ListPage {
            objects,
            next: None,
        });
    }
    match xml_values(xml, "NextContinuationToken").into_iter().next() {
        Some(token) if !token.is_empty() => Ok(ListPage {
            objects,
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

    /// The document `apply` wrote in M4: one rule over the whole bucket.
    fn m4_document(days: u32) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><LifecycleConfiguration \
             xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Rule><ID>ciburst-expire</ID>\
             <Filter><Prefix></Prefix></Filter><Status>Enabled</Status><Expiration><Days>{days}\
             </Days></Expiration></Rule></LifecycleConfiguration>"
        )
    }

    #[test]
    fn lifecycle_document_round_trip() {
        let xml = lifecycle_xml(14);
        let rule = |id: &str, prefix: &str| {
            format!(
                "<Rule><ID>{id}</ID><Filter><Prefix>{prefix}</Prefix></Filter><Status>Enabled\
                 </Status><Expiration><Days>14</Days></Expiration></Rule>"
            )
        };
        assert_eq!(
            xml,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><LifecycleConfiguration \
                 xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">{}{}{}</LifecycleConfiguration>",
                rule("ciburst-expire-project", "project/"),
                rule("ciburst-expire-runner", "runner/"),
                rule("ciburst-expire-intents", "ciburst/intents/"),
            )
        );
        assert!(
            !xml.contains("<Prefix></Prefix>"),
            "nothing expires the agent"
        );
        assert_eq!(parse_lifecycle_days(&xml), Some(14));
        for changed in [
            xml.replacen("Enabled", "Disabled", 1),
            xml.replace("ciburst-expire-runner", "other"),
            xml.replace("runner/", "runners/"),
            xml.replacen("<Days>14</Days>", "<Days>3</Days>", 1),
            xml.replace(
                "</LifecycleConfiguration>",
                "<Rule><ID>ciburst-expire</ID><Filter><Prefix></Prefix></Filter><Status>Enabled\
                 </Status><Expiration><Days>14</Days></Expiration></Rule></LifecycleConfiguration>",
            ),
            m4_document(14),
            "<LifecycleConfiguration/>".to_string(),
        ] {
            assert_eq!(parse_lifecycle_days(&changed), None, "{changed}");
        }
        let foreign = xml.replace(
            "</LifecycleConfiguration>",
            "<Rule><ID>owner-rule</ID><Filter><Prefix>tmp/</Prefix></Filter><Status>Enabled\
             </Status><Expiration><Days>1</Days></Expiration></Rule></LifecycleConfiguration>",
        );
        assert_eq!(
            parse_lifecycle_days(&foreign),
            Some(14),
            "other rules stay theirs"
        );
        let legacy_prefix = xml
            .replace("<Filter><Prefix>", "<Prefix>")
            .replace("</Prefix></Filter>", "</Prefix>");
        assert_eq!(parse_lifecycle_days(&legacy_prefix), Some(14));
    }

    #[test]
    fn public_urls_are_virtual_hosted() {
        assert_eq!(
            public_url("demo-cache-p-1", "fr-par", "ciburst/agent/ab12"),
            "https://demo-cache-p-1.s3.fr-par.scw.cloud/ciburst/agent/ab12"
        );
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
             <Contents><Key>p/a&amp;b</Key><LastModified>2026-10-24T00:00:01.000Z</LastModified>\
             <Size>0</Size></Contents>\
             <Contents><Key>p/c</Key><Size>0</Size></Contents></ListBucketResult>",
        )
        .unwrap();
        assert_eq!(
            page.objects,
            [
                Listed {
                    key: "p/a&b".into(),
                    modified: Some(1_792_800_001)
                },
                Listed {
                    key: "p/c".into(),
                    modified: None
                }
            ]
        );
        assert_eq!(page.next.as_deref(), Some("t+/="));
        let last = parse_list_page(
            "<ListBucketResult><ContinuationToken>t</ContinuationToken><KeyCount>0</KeyCount>\
             <IsTruncated>false</IsTruncated></ListBucketResult>",
        )
        .unwrap();
        assert_eq!(
            last,
            ListPage {
                objects: Vec::new(),
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
