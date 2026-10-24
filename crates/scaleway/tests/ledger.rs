//! `ScalewayLedger` against the S3 fake (M4 rule R13), and the fake's object writes and
//! ListObjectsV2 against what S3 answers.

use std::sync::Arc;

use ciburst_core::http::{HttpClient, Request};
use ciburst_core::ledger::{Intent, Ledger};
use ciburst_core::types::KeyPair;
use ciburst_scaleway::ledger::{MAX_PAGES, ScalewayLedger};
use ciburst_scaleway::provider::s3::{S3, xml_values};
use ciburst_scaleway::provider::sigv4;
use ciburst_scaleway::testing::s3_api::{DEFAULT_PAGE, FakeS3};
use ciburst_scaleway::testing::scaleway_fake::{FAKE_ACCESS_KEY, FAKE_SECRET_KEY};

const BUCKET: &str = "demo-cache-p-1";
const CACHE_ACCESS: &str = "SCWCACHEACCESS000000";
const CACHE_SECRET: &str = "cache-secret-key-value";
const NOW: u64 = 1_800_000_000;

fn owner(fake: &Arc<FakeS3>) -> S3 {
    S3 {
        http: fake.clone(),
        region: "fr-par".into(),
        access_key: FAKE_ACCESS_KEY.into(),
        secret_key: FAKE_SECRET_KEY.into(),
    }
}

fn cache_key() -> KeyPair {
    KeyPair {
        access_key: CACHE_ACCESS.into(),
        secret_key: CACHE_SECRET.into(),
    }
}

/// A fake with the cache bucket and the cache key, and the ledger on it.
async fn ledger() -> (Arc<FakeS3>, ScalewayLedger) {
    let fake = Arc::new(FakeS3::new());
    owner(&fake).create_bucket(BUCKET).await.unwrap();
    fake.add_key(CACHE_ACCESS, CACHE_SECRET);
    let ledger = ScalewayLedger::new(fake.clone(), "fr-par", BUCKET, &cache_key());
    (fake, ledger)
}

fn intent(at: u64, job: &str) -> Intent {
    Intent {
        at,
        job: job.to_string(),
    }
}

fn sorted(mut intents: Vec<Intent>) -> Vec<Intent> {
    intents.sort_by(|a, b| (a.at, a.job.len(), &a.job).cmp(&(b.at, b.job.len(), &b.job)));
    intents
}

/// Query strings of the listings the fake received.
fn listings(fake: &FakeS3) -> Vec<String> {
    fake.calls()
        .into_iter()
        .filter(|c| c.method == "GET" && c.query.contains("list-type=2"))
        .map(|c| c.query)
        .collect()
}

#[tokio::test]
async fn record_puts_an_empty_object_under_the_intent_key() {
    let (fake, ledger) = ledger().await;
    assert_eq!(ledger.bucket(), BUCKET);
    ledger.record("vm", NOW, "42").await.unwrap();
    ledger.record("vm", NOW, "7").await.unwrap();
    assert_eq!(
        fake.objects(BUCKET),
        [
            (
                "ciburst/intents/vm/1800000000-00000000000000000007".to_string(),
                Vec::new()
            ),
            (
                "ciburst/intents/vm/1800000000-00000000000000000042".to_string(),
                Vec::new()
            ),
        ]
    );
    let puts: Vec<String> = fake
        .calls()
        .into_iter()
        .filter(|c| c.method == "PUT")
        .map(|c| c.path)
        .collect();
    assert_eq!(
        puts[1..],
        [
            format!("/{BUCKET}/ciburst/intents/vm/1800000000-00000000000000000042"),
            format!("/{BUCKET}/ciburst/intents/vm/1800000000-00000000000000000007"),
        ]
    );
}

#[tokio::test]
async fn since_reads_the_pool_window_across_pages() {
    let (fake, ledger) = ledger().await;
    let since = NOW - 30;
    for (pool, at, job) in [
        ("vm", since - 1, "1"),
        ("vm", since, "10"),
        ("vm", since, "9"),
        ("vm", since + 1, "3"),
        ("vm", NOW, "42"),
        ("vm", NOW, "18446744073709551615"),
        ("vm", NOW + 5, "0"),
        ("vm-2", NOW, "5"),
        ("v", NOW, "6"),
    ] {
        ledger.record(pool, at, job).await.unwrap();
    }
    for junk in [
        "ciburst/intents/vm/1800000000-42",
        "ciburst/intents/vm/zzzz",
        "ciburst/intents/vm/1800000000-00000000000000000042/x",
        "runner/project/7/cache.zip",
    ] {
        fake.put_object(BUCKET, junk, b"x");
    }

    let read = ledger.since("vm", since).await.unwrap();
    assert_eq!(
        sorted(read),
        [
            intent(since, "9"),
            intent(since, "10"),
            intent(since + 1, "3"),
            intent(NOW, "42"),
            intent(NOW, "18446744073709551615"),
            intent(NOW + 5, "0"),
        ]
    );

    let queries = listings(&fake);
    assert!(queries.len() >= 2, "{queries:?}");
    let start_after = format!("start-after=ciburst%2Fintents%2Fvm%2F{since:010}");
    for (index, query) in queries.iter().enumerate() {
        assert!(
            query.contains("prefix=ciburst%2Fintents%2Fvm%2F"),
            "{query}"
        );
        assert!(query.contains(&start_after), "{query}");
        assert_eq!(query.contains("continuation-token="), index > 0, "{query}");
        assert!(!query.contains("max-keys"), "{query}");
    }

    assert_eq!(
        sorted(ledger.since("vm", NOW + 5).await.unwrap()),
        [intent(NOW + 5, "0")]
    );
    assert!(ledger.since("vm", NOW + 6).await.unwrap().is_empty());
    assert_eq!(ledger.since("vm-2", 0).await.unwrap(), [intent(NOW, "5")]);
    assert!(ledger.since("other", 0).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_read_follows_at_most_max_pages() {
    let (fake, ledger) = ledger().await;
    fake.set_page_size(1);
    for job in 1..=MAX_PAGES {
        ledger.record("vm", NOW, &job.to_string()).await.unwrap();
    }
    assert_eq!(ledger.since("vm", NOW).await.unwrap().len(), MAX_PAGES);
    assert_eq!(listings(&fake).len(), MAX_PAGES);

    ledger.record("vm", NOW, "999").await.unwrap();
    let error = ledger.since("vm", NOW).await.unwrap_err().to_string();
    assert_eq!(
        error,
        format!("scaleway ledger: pool vm: more than {MAX_PAGES} pages of intents")
    );
}

#[tokio::test]
async fn ledger_errors_name_the_cause_and_bad_input_sends_nothing() {
    let (fake, ledger) = ledger().await;
    let calls = fake.calls().len();
    for (pool, job) in [("vm", "042"), ("vm", "x"), ("vm", ""), ("a/b", "1")] {
        assert!(ledger.record(pool, NOW, job).await.is_err(), "{pool} {job}");
    }
    assert!(ledger.since("a/b", NOW).await.is_err());
    assert_eq!(fake.calls().len(), calls, "nothing sent");

    let elsewhere = ScalewayLedger::new(fake.clone(), "fr-par", "missing-bucket", &cache_key());
    let error = elsewhere
        .record("vm", NOW, "1")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("404 NoSuchBucket"), "{error}");
    let error = elsewhere.since("vm", NOW).await.unwrap_err().to_string();
    assert!(error.contains("404 NoSuchBucket"), "{error}");

    let wrong = KeyPair {
        access_key: CACHE_ACCESS.into(),
        secret_key: "not-the-secret".into(),
    };
    let forged = ScalewayLedger::new(fake.clone(), "fr-par", BUCKET, &wrong);
    let error = forged.record("vm", NOW, "1").await.unwrap_err().to_string();
    assert!(error.contains("403 SignatureDoesNotMatch"), "{error}");
    let unknown = KeyPair {
        access_key: "SCWUNKNOWN".into(),
        secret_key: CACHE_SECRET.into(),
    };
    let unknown = ScalewayLedger::new(fake.clone(), "fr-par", BUCKET, &unknown);
    let error = unknown.since("vm", NOW).await.unwrap_err().to_string();
    assert!(error.contains("403 InvalidAccessKeyId"), "{error}");
    assert!(fake.objects(BUCKET).is_empty());
}

/// A request signed with the owner key as the S3 client signs it, without the length header
/// the client adds to object writes.
fn signed(method: &str, path: &str, query: &str, headers: &[(&str, &str)], body: &[u8]) -> Request {
    let host = "s3.fr-par.scw.cloud";
    let amz_date = sigv4::amz_date(NOW);
    let payload = sigv4::sha256_hex(body);
    let signed_headers = vec![
        ("host".to_string(), host.to_string()),
        ("x-amz-content-sha256".to_string(), payload.clone()),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];
    let canonical = sigv4::canonical_request(
        method,
        path,
        &sigv4::canonical_query_from_raw(query),
        &signed_headers,
        &payload,
    );
    let signature = sigv4::signature(FAKE_SECRET_KEY, &amz_date, "fr-par", &canonical);
    let authorization = sigv4::authorization(
        FAKE_ACCESS_KEY,
        &amz_date,
        "fr-par",
        &sigv4::signed_headers(&signed_headers),
        &signature,
    );
    let url = if query.is_empty() {
        format!("https://{host}{path}")
    } else {
        format!("https://{host}{path}?{query}")
    };
    let mut request = Request::new(method, url)
        .header("authorization", authorization)
        .header("x-amz-content-sha256", payload)
        .header("x-amz-date", amz_date);
    for (name, value) in headers {
        request = request.header(name, *value);
    }
    if !body.is_empty() {
        request.body = Some(body.to_vec());
    }
    request
}

async fn send(fake: &FakeS3, request: Request) -> (u16, String) {
    let response = fake.send(request).await.unwrap();
    (response.status, response.text())
}

fn code(text: &str) -> String {
    xml_values(text, "Code")
        .into_iter()
        .next()
        .unwrap_or_default()
}

#[tokio::test]
async fn the_fake_writes_objects_like_s3() {
    let fake = FakeS3::new();
    let path = format!("/{BUCKET}/k");
    let (status, text) = send(
        &fake,
        signed("PUT", &path, "", &[("content-length", "0")], b""),
    )
    .await;
    assert_eq!((status, code(&text)), (404, "NoSuchBucket".to_string()));
    let (status, _) = send(&fake, signed("PUT", &format!("/{BUCKET}"), "", &[], b"")).await;
    assert_eq!(status, 200);

    let (status, text) = send(&fake, signed("PUT", &path, "", &[], b"")).await;
    assert_eq!(
        (status, code(&text)),
        (411, "MissingContentLength".to_string())
    );
    let (status, text) = send(
        &fake,
        signed("PUT", &path, "", &[("content-length", "5")], b""),
    )
    .await;
    assert_eq!((status, code(&text)), (400, "IncompleteBody".to_string()));
    let long = format!("/{BUCKET}/{}", "k".repeat(1025));
    let (status, text) = send(
        &fake,
        signed("PUT", &long, "", &[("content-length", "0")], b""),
    )
    .await;
    assert_eq!((status, code(&text)), (400, "KeyTooLongError".to_string()));
    assert!(fake.objects(BUCKET).is_empty());

    let (status, _) = send(
        &fake,
        signed("PUT", &path, "", &[("content-length", "0")], b""),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = send(
        &fake,
        signed("PUT", &format!("/{BUCKET}/data"), "", &[], b"abc"),
    )
    .await;
    assert_eq!(status, 200, "the HTTP client derives the length of a body");
    assert_eq!(
        fake.objects(BUCKET),
        [
            ("data".to_string(), b"abc".to_vec()),
            ("k".to_string(), Vec::new())
        ]
    );

    let mut tampered = signed(
        "PUT",
        &format!("/{BUCKET}/other"),
        "",
        &[("content-length", "0")],
        b"",
    );
    tampered.url = tampered.url.replace("/other", "/forged");
    let (status, text) = send(&fake, tampered).await;
    assert_eq!(
        (status, code(&text)),
        (403, "SignatureDoesNotMatch".to_string())
    );
}

#[tokio::test]
async fn the_fake_lists_objects_v2_like_s3() {
    let fake = FakeS3::new();
    send(&fake, signed("PUT", &format!("/{BUCKET}"), "", &[], b"")).await;
    for key in ["a/1", "a/2", "a/3", "a/4", "a/5", "b/1"] {
        fake.put_object(BUCKET, key, b"");
    }
    let list = |query: &str| signed("GET", &format!("/{BUCKET}"), query, &[], b"");

    let (status, text) = send(&fake, list("list-type=2&prefix=a%2F")).await;
    assert_eq!(status, 200);
    assert!(
        text.starts_with(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult \
         xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>demo-cache-p-1</Name>\
         <Prefix>a/</Prefix><NextContinuationToken>"
        ),
        "{text}"
    );
    assert_eq!(xml_values(&text, "Key"), ["a/1", "a/2", "a/3"]);
    assert_eq!(DEFAULT_PAGE, 3);
    assert_eq!(xml_values(&text, "KeyCount"), ["3"]);
    assert_eq!(xml_values(&text, "MaxKeys"), ["3"]);
    assert_eq!(xml_values(&text, "IsTruncated"), ["true"]);
    assert_eq!(xml_values(&text, "Size"), ["0", "0", "0"]);
    let token = xml_values(&text, "NextContinuationToken").remove(0);

    let encoded = sigv4::uri_encode(&token, true);
    let (status, text) = send(
        &fake,
        list(&format!(
            "list-type=2&prefix=a%2F&start-after=zzz&continuation-token={encoded}"
        )),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        xml_values(&text, "Key"),
        ["a/4", "a/5"],
        "the token wins over start-after"
    );
    assert_eq!(xml_values(&text, "ContinuationToken"), [token.as_str()]);
    assert_eq!(xml_values(&text, "StartAfter"), ["zzz"]);
    assert_eq!(xml_values(&text, "IsTruncated"), ["false"]);
    assert!(xml_values(&text, "NextContinuationToken").is_empty());

    let (_, text) = send(
        &fake,
        list("list-type=2&prefix=a%2F&start-after=a%2F2&max-keys=2"),
    )
    .await;
    assert_eq!(xml_values(&text, "Key"), ["a/3", "a/4"]);
    assert_eq!(xml_values(&text, "IsTruncated"), ["true"]);
    let (_, text) = send(
        &fake,
        list("list-type=2&prefix=a%2F&start-after=a%2F2&max-keys=3"),
    )
    .await;
    assert_eq!(xml_values(&text, "Key"), ["a/3", "a/4", "a/5"]);
    assert_eq!(
        xml_values(&text, "IsTruncated"),
        ["false"],
        "nothing after the last key"
    );
    let (_, text) = send(&fake, list("list-type=2&max-keys=5000")).await;
    assert_eq!(xml_values(&text, "MaxKeys"), ["1000"]);
    assert_eq!(xml_values(&text, "Key").len(), 6);
    let (_, text) = send(&fake, list("list-type=2&max-keys=0")).await;
    assert_eq!(xml_values(&text, "KeyCount"), ["0"]);
    assert_eq!(xml_values(&text, "IsTruncated"), ["false"]);

    for (query, status, expected) in [
        ("list-type=2&max-keys=many", 400, "InvalidArgument"),
        (
            "list-type=2&continuation-token=bm9wZQ%3D%3D",
            400,
            "InvalidArgument",
        ),
        ("list-type=2&delimiter=%2F", 501, "NotImplemented"),
    ] {
        let (got, text) = send(&fake, list(query)).await;
        assert_eq!(
            (got, code(&text)),
            (status, expected.to_string()),
            "{query}"
        );
    }
    let (status, text) = send(&fake, signed("GET", "/missing", "list-type=2", &[], b"")).await;
    assert_eq!((status, code(&text)), (404, "NoSuchBucket".to_string()));

    let mut tampered = list("list-type=2&prefix=a%2F");
    tampered.url = tampered.url.replace("prefix=a%2F", "prefix=b%2F");
    let (status, text) = send(&fake, tampered).await;
    assert_eq!(
        (status, code(&text)),
        (403, "SignatureDoesNotMatch".to_string())
    );
}
