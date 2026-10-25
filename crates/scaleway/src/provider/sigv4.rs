//! AWS Signature Version 4, header based, as the Scaleway Object Storage S3 endpoint expects it.
//! The fake S3 endpoint verifies requests with the same functions, so the known vector test
//! below is what ties both to the published algorithm.

use ciburst_core::time::DateTime;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
pub const SERVICE: &str = "s3";
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// RFC 3986 encoding as SigV4 defines it; `/` stays literal in object key paths.
pub fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Encodes and sorts query parameters; the same string is used in the URL.
pub fn canonical_query(pairs: &[(&str, &str)]) -> String {
    let mut encoded: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", uri_encode(k, true), uri_encode(v, true)))
        .collect();
    encoded.sort();
    encoded.join("&")
}

/// Canonical form of an already encoded query string as it appears in a URL.
pub fn canonical_query_from_raw(raw: &str) -> String {
    let mut pairs: Vec<String> = raw
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            if p.contains('=') {
                p.to_string()
            } else {
                format!("{p}=")
            }
        })
        .collect();
    pairs.sort();
    pairs.join("&")
}

/// Headers as `(lowercase name, trimmed value)`, sorted by name.
pub fn normalize_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    out.sort();
    out
}

pub fn signed_headers(headers: &[(String, String)]) -> String {
    normalize_headers(headers)
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";")
}

pub fn canonical_request(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
) -> String {
    let headers = normalize_headers(headers);
    let mut out = format!("{method}\n{path}\n{query}\n");
    for (k, v) in &headers {
        out.push_str(k);
        out.push(':');
        out.push_str(v);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(
        &headers
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";"),
    );
    out.push('\n');
    out.push_str(payload_hash);
    out
}

pub fn scope(amz_date: &str, region: &str) -> String {
    format!("{}/{region}/{SERVICE}/aws4_request", &amz_date[..8])
}

pub fn string_to_sign(amz_date: &str, region: &str, canonical_request: &str) -> String {
    format!(
        "{ALGORITHM}\n{amz_date}\n{}\n{}",
        scope(amz_date, region),
        sha256_hex(canonical_request.as_bytes())
    )
}

pub fn signature(
    secret_key: &str,
    amz_date: &str,
    region: &str,
    canonical_request: &str,
) -> String {
    let date_key = hmac(format!("AWS4{secret_key}").as_bytes(), &amz_date[..8]);
    let region_key = hmac(&date_key, region);
    let service_key = hmac(&region_key, SERVICE);
    let signing_key = hmac(&service_key, "aws4_request");
    hex::encode(hmac(
        &signing_key,
        &string_to_sign(amz_date, region, canonical_request),
    ))
}

pub fn authorization(
    access_key: &str,
    amz_date: &str,
    region: &str,
    signed_headers: &str,
    signature: &str,
) -> String {
    format!(
        "{ALGORITHM} Credential={access_key}/{}, SignedHeaders={signed_headers}, Signature={signature}",
        scope(amz_date, region)
    )
}

/// The fields of an `Authorization` header: access key, scope, signed headers, signature.
pub fn parse_authorization(value: &str) -> Option<(String, String, String, String)> {
    let rest = value.strip_prefix(ALGORITHM)?.trim();
    let mut credential = None;
    let mut signed = None;
    let mut sig = None;
    for part in rest.split(',') {
        let (k, v) = part.trim().split_once('=')?;
        match k {
            "Credential" => credential = Some(v.to_string()),
            "SignedHeaders" => signed = Some(v.to_string()),
            "Signature" => sig = Some(v.to_string()),
            _ => return None,
        }
    }
    let credential = credential?;
    let (access_key, scope) = credential.split_once('/')?;
    Some((access_key.to_string(), scope.to_string(), signed?, sig?))
}

/// `YYYYMMDDTHHMMSSZ` for a Unix time in seconds.
pub fn amz_date(unix_seconds: u64) -> String {
    let at = DateTime::from_unix(unix_seconds);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        at.year, at.month, at.day, at.hour, at.minute, at.second
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // The "GET Object" example of the AWS S3 SigV4 documentation (header based auth):
    // bucket examplebucket, key test.txt, Range bytes=0-9, 2013-05-24, region us-east-1.
    #[test]
    fn aws_get_object_vector() {
        let headers = headers(&[
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("Range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let canonical = canonical_request("GET", "/test.txt", "", &headers, EMPTY_SHA256);
        assert_eq!(
            canonical,
            "GET\n/test.txt\n\nhost:examplebucket.s3.amazonaws.com\nrange:bytes=0-9\n\
             x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
             x-amz-date:20130524T000000Z\n\nhost;range;x-amz-content-sha256;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            string_to_sign("20130524T000000Z", "us-east-1", &canonical),
            "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n\
             7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972"
        );
        assert_eq!(
            signature(SECRET, "20130524T000000Z", "us-east-1", &canonical),
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert_eq!(
            signed_headers(&headers),
            "host;range;x-amz-content-sha256;x-amz-date"
        );
    }

    // The "GET Bucket Lifecycle" example of the same page: query `lifecycle` without a value.
    #[test]
    fn aws_get_bucket_lifecycle_vector() {
        let headers = headers(&[
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let query = canonical_query_from_raw("lifecycle");
        assert_eq!(query, "lifecycle=");
        assert_eq!(query, canonical_query(&[("lifecycle", "")]));
        let canonical = canonical_request("GET", "/", &query, &headers, EMPTY_SHA256);
        assert_eq!(
            signature(SECRET, "20130524T000000Z", "us-east-1", &canonical),
            "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
        );
    }

    #[test]
    fn authorization_round_trip() {
        let value = authorization(
            "AKIDEXAMPLE",
            "20261024T101500Z",
            "fr-par",
            "host;x-amz-date",
            "abc",
        );
        assert_eq!(
            value,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261024/fr-par/s3/aws4_request, \
             SignedHeaders=host;x-amz-date, Signature=abc"
        );
        assert_eq!(
            parse_authorization(&value),
            Some((
                "AKIDEXAMPLE".into(),
                "20261024/fr-par/s3/aws4_request".into(),
                "host;x-amz-date".into(),
                "abc".into()
            ))
        );
        assert_eq!(parse_authorization("Bearer x"), None);
    }

    #[test]
    fn encodes_and_sorts_queries() {
        assert_eq!(
            canonical_query(&[("prefix", "a b/c"), ("list-type", "2")]),
            "list-type=2&prefix=a%20b%2Fc"
        );
        assert_eq!(uri_encode("dir/a b", false), "dir/a%20b");
    }

    #[test]
    fn formats_dates() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_369_353_600), "20130524T000000Z");
        assert_eq!(amz_date(1_709_210_096), "20240229T123456Z");
    }
}
