//! `ScalewayPipelineRecords` against the S3 fake (M6.1 rule R5): the key layout, one record per
//! pipeline, seen markers only beside a record, listings across pages, `close`, the failures
//! that may be retried (M7 hardening rule R4), and the burst ledger reading past the records.

use std::sync::Arc;

use ciburst_core::config::parse_config;
use ciburst_core::ledger::{Intent, Ledger, PipelineRecords, VmJobs};
use ciburst_core::types::KeyPair;
use ciburst_scaleway::ledger::{MAX_PAGES, ScalewayLedger};
use ciburst_scaleway::provider::s3::S3;
use ciburst_scaleway::records::ScalewayPipelineRecords;
use ciburst_scaleway::testing::s3_api::FakeS3;
use ciburst_scaleway::testing::scaleway_fake::{FAKE_ACCESS_KEY, FAKE_SECRET_KEY, Failure};

const BUCKET: &str = "demo-cache-p-1";
const CACHE_ACCESS: &str = "SCWCACHEACCESS000000";
const CACHE_SECRET: &str = "cache-secret-key-value";
const NOW: u64 = 1_800_000_000;
const RECORD_9: &str = "ciburst/intents/_pipelines/9/vm-jobs.json";

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

/// A fake with the cache bucket and the cache key, and the records on it.
async fn records() -> (Arc<FakeS3>, ScalewayPipelineRecords) {
    let fake = Arc::new(FakeS3::new());
    owner(&fake).create_bucket(BUCKET).await.unwrap();
    fake.add_key(CACHE_ACCESS, CACHE_SECRET);
    let records = ScalewayPipelineRecords::new(fake.clone(), "fr-par", BUCKET, &cache_key());
    (fake, records)
}

fn vm_jobs(pools: &[(&str, &[&str])]) -> VmJobs {
    pools
        .iter()
        .map(|(pool, jobs)| {
            (
                pool.to_string(),
                jobs.iter().map(|j| j.to_string()).collect(),
            )
        })
        .collect()
}

/// `<method> <path>` of each call after the first `from`.
fn calls_since(fake: &FakeS3, from: usize) -> Vec<String> {
    fake.calls()[from..]
        .iter()
        .map(|c| format!("{} {}", c.method, c.path))
        .collect()
}

/// Query strings of the listings the fake received after the first `from` calls.
fn listings_since(fake: &FakeS3, from: usize) -> Vec<String> {
    fake.calls()[from..]
        .iter()
        .filter(|c| c.method == "GET" && c.query.contains("list-type=2"))
        .map(|c| c.query.clone())
        .collect()
}

fn keys(fake: &FakeS3) -> Vec<String> {
    fake.objects(BUCKET)
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

fn sorted(mut jobs: Vec<String>) -> Vec<String> {
    jobs.sort_by(|a, b| (a.len(), a).cmp(&(b.len(), b)));
    jobs
}

#[tokio::test]
async fn open_writes_the_vm_jobs_once() {
    let (fake, records) = records().await;
    assert_eq!(records.bucket(), BUCKET);
    let jobs = vm_jobs(&[("vm", &["11", "12"]), ("big", &["13"])]);

    let from = fake.calls().len();
    assert_eq!(records.vm_jobs("9").await.unwrap(), None);
    assert!(records.open("9", &jobs).await.unwrap());
    assert_eq!(records.vm_jobs("9").await.unwrap(), Some(jobs.clone()));
    assert!(
        !records
            .open("9", &vm_jobs(&[("vm", &["14"])]))
            .await
            .unwrap()
    );
    assert_eq!(records.vm_jobs("9").await.unwrap(), Some(jobs));
    let call = |method: &str| format!("{method} /{BUCKET}/{RECORD_9}");
    assert_eq!(
        calls_since(&fake, from),
        [
            call("GET"),
            call("HEAD"),
            call("PUT"),
            call("GET"),
            call("HEAD"),
            call("GET")
        ]
    );
    assert_eq!(
        fake.objects(BUCKET),
        [(
            RECORD_9.to_string(),
            br#"{"big":["13"],"vm":["11","12"]}"#.to_vec()
        )]
    );

    assert!(records.open("10", &VmJobs::new()).await.unwrap());
    assert_eq!(records.vm_jobs("10").await.unwrap(), Some(VmJobs::new()));
}

#[tokio::test]
async fn seen_marks_a_job_only_beside_a_record() {
    let (fake, records) = records().await;
    let from = fake.calls().len();
    records.seen("9", "11").await.unwrap();
    assert_eq!(
        calls_since(&fake, from),
        [format!("HEAD /{BUCKET}/{RECORD_9}")]
    );
    assert!(fake.objects(BUCKET).is_empty());
    assert!(records.seen_jobs("9").await.unwrap().is_empty());

    records
        .open("9", &vm_jobs(&[("vm", &["11", "12"])]))
        .await
        .unwrap();
    let from = fake.calls().len();
    records.seen("9", "11").await.unwrap();
    assert_eq!(
        calls_since(&fake, from),
        [
            format!("HEAD /{BUCKET}/{RECORD_9}"),
            format!("PUT /{BUCKET}/ciburst/intents/_pipelines/9/seen/11"),
        ]
    );
    records.seen("9", "11").await.unwrap();
    assert_eq!(
        fake.objects(BUCKET),
        [
            (
                "ciburst/intents/_pipelines/9/seen/11".to_string(),
                Vec::new()
            ),
            (RECORD_9.to_string(), br#"{"vm":["11","12"]}"#.to_vec()),
        ]
    );
    assert_eq!(records.seen_jobs("9").await.unwrap(), ["11"]);
}

#[tokio::test]
async fn seen_jobs_reads_the_markers_across_pages() {
    let (fake, records) = records().await;
    let jobs = ["1", "2", "10", "11", "12", "300", "18446744073709551615"];
    records.open("9", &vm_jobs(&[("vm", &jobs)])).await.unwrap();
    records
        .open("90", &vm_jobs(&[("vm", &["5"])]))
        .await
        .unwrap();
    for job in jobs {
        records.seen("9", job).await.unwrap();
    }
    records.seen("90", "5").await.unwrap();
    for junk in ["x", "042", "1/2", ""] {
        fake.put_object(
            BUCKET,
            &format!("ciburst/intents/_pipelines/9/seen/{junk}"),
            b"",
        );
    }

    let from = fake.calls().len();
    assert_eq!(sorted(records.seen_jobs("9").await.unwrap()), jobs);
    let queries = listings_since(&fake, from);
    assert!(queries.len() >= 3, "{queries:?}");
    for (index, query) in queries.iter().enumerate() {
        assert!(
            query.contains("prefix=ciburst%2Fintents%2F_pipelines%2F9%2Fseen%2F"),
            "{query}"
        );
        assert!(!query.contains("start-after"), "{query}");
        assert_eq!(query.contains("continuation-token="), index > 0, "{query}");
    }
    assert_eq!(records.seen_jobs("90").await.unwrap(), ["5"]);
    assert!(records.seen_jobs("8").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_listing_follows_at_most_max_pages() {
    let (fake, records) = records().await;
    fake.set_page_size(1);
    for job in 1..=MAX_PAGES {
        fake.put_object(
            BUCKET,
            &format!("ciburst/intents/_pipelines/9/seen/{job}"),
            b"",
        );
    }
    assert_eq!(records.seen_jobs("9").await.unwrap().len(), MAX_PAGES);
    fake.put_object(BUCKET, "ciburst/intents/_pipelines/9/seen/999", b"");
    let error = records.seen_jobs("9").await.unwrap_err().to_string();
    assert_eq!(
        error,
        format!(
            "scaleway s3 list {BUCKET}/ciburst/intents/_pipelines/9/seen/: more than \
             {MAX_PAGES} pages"
        )
    );
    assert!(records.close("9").await.is_err());
    assert_eq!(keys(&fake).len(), MAX_PAGES + 1, "nothing deleted");
}

#[tokio::test]
async fn close_deletes_every_record_of_the_pipeline_and_nothing_else() {
    let (fake, records) = records().await;
    let ledger = ScalewayLedger::new(fake.clone(), "fr-par", BUCKET, &cache_key());
    ledger.record("vm", NOW, "11").await.unwrap();
    records
        .open("9", &vm_jobs(&[("vm", &["11", "12", "13", "14"])]))
        .await
        .unwrap();
    records
        .open("90", &vm_jobs(&[("vm", &["5"])]))
        .await
        .unwrap();
    for job in ["11", "12", "13", "14"] {
        records.seen("9", job).await.unwrap();
    }
    records.seen("90", "5").await.unwrap();
    fake.put_object(BUCKET, "ciburst/intents/_pipelines/9/other", b"x");
    fake.set_page_size(2);

    let from = fake.calls().len();
    records.close("9").await.unwrap();
    let deletes: Vec<String> = calls_since(&fake, from)
        .into_iter()
        .filter(|call| call.starts_with("DELETE "))
        .collect();
    assert_eq!(deletes.len(), 6, "{deletes:#?}");
    let prefix = format!("DELETE /{BUCKET}/ciburst/intents/_pipelines/9/");
    assert!(
        deletes.iter().all(|d| d.starts_with(&prefix)),
        "{deletes:#?}"
    );
    assert_eq!(
        keys(&fake),
        [
            "ciburst/intents/_pipelines/90/seen/5",
            "ciburst/intents/_pipelines/90/vm-jobs.json",
            "ciburst/intents/vm/1800000000-00000000000000000011",
        ]
    );
    assert_eq!(records.vm_jobs("9").await.unwrap(), None);
    assert!(records.seen_jobs("9").await.unwrap().is_empty());

    for pipeline in ["9", "8"] {
        let from = fake.calls().len();
        records.close(pipeline).await.unwrap();
        let calls = calls_since(&fake, from);
        assert_eq!(calls, [format!("GET /{BUCKET}")], "{pipeline}");
    }
    assert_eq!(keys(&fake).len(), 3);
}

#[tokio::test]
async fn bad_ids_send_nothing_and_bucket_errors_name_the_cause() {
    let (fake, records) = records().await;
    let calls = fake.calls().len();
    for id in ["", "09", "x", "a/b", ".."] {
        assert!(records.open(id, &VmJobs::new()).await.is_err(), "{id:?}");
        assert!(records.vm_jobs(id).await.is_err(), "{id:?}");
        assert!(records.seen(id, "1").await.is_err(), "{id:?}");
        assert!(records.seen("9", id).await.is_err(), "{id:?}");
        assert!(records.seen_jobs(id).await.is_err(), "{id:?}");
        assert!(records.close(id).await.is_err(), "{id:?}");
    }
    assert_eq!(fake.calls().len(), calls, "nothing sent");

    let elsewhere =
        ScalewayPipelineRecords::new(fake.clone(), "fr-par", "missing-bucket", &cache_key());
    for error in [
        elsewhere.open("9", &VmJobs::new()).await.unwrap_err(),
        elsewhere.vm_jobs("9").await.unwrap_err(),
        elsewhere.seen_jobs("9").await.unwrap_err(),
        elsewhere.close("9").await.unwrap_err(),
    ] {
        assert!(error.to_string().contains("404 NoSuchBucket"), "{error}");
    }

    fake.put_object(BUCKET, RECORD_9, b"not json");
    let error = records.vm_jobs("9").await.unwrap_err().to_string();
    assert!(
        error.starts_with(&format!("scaleway records: {RECORD_9}: ")),
        "{error}"
    );

    let wrong = KeyPair {
        access_key: CACHE_ACCESS.into(),
        secret_key: "not-the-secret".into(),
    };
    let forged = ScalewayPipelineRecords::new(fake.clone(), "fr-par", BUCKET, &wrong);
    let error = forged.vm_jobs("9").await.unwrap_err().to_string();
    assert!(error.contains("403 SignatureDoesNotMatch"), "{error}");
}

/// M7 hardening rule R4 for the records: 429 and 503 may pass on a repeat for every call,
/// another 5xx and a transport timeout only for reads.
#[tokio::test]
async fn failures_are_retryable_where_a_repeat_may_pass() {
    let (fake, records) = records().await;
    let record = format!("/{BUCKET}/{RECORD_9}");
    let record = record.as_str();
    let marker = format!("/{BUCKET}/ciburst/intents/_pipelines/9/seen/*");
    let marker = marker.as_str();
    let any_key = format!("/{BUCKET}/ciburst/intents/_pipelines/9/*");
    let listing = format!("/{BUCKET}");
    let jobs = vm_jobs(&[("vm", &["11"])]);
    for (method, path, failure, retryable) in [
        ("HEAD", record, Failure::Status(500), true),
        ("PUT", record, Failure::Status(429), true),
        ("PUT", record, Failure::Status(503), true),
        ("PUT", record, Failure::Status(500), false),
        ("GET", record, Failure::Status(500), true),
        ("GET", record, Failure::Timeout, true),
        ("PUT", marker, Failure::Status(503), true),
        ("PUT", marker, Failure::Timeout, false),
        ("GET", &listing, Failure::Status(502), true),
        ("DELETE", &any_key, Failure::Status(503), true),
        ("DELETE", &any_key, Failure::Status(500), false),
    ] {
        records.close("9").await.unwrap();
        let opens = (method, path) == ("PUT", record);
        if !opens {
            records.open("9", &jobs).await.unwrap();
        }
        fake.fail_next(method, path, failure);
        let error = match (method, path) {
            ("GET", p) if p == record => records.vm_jobs("9").await.unwrap_err(),
            ("PUT", p) if p == marker => records.seen("9", "11").await.unwrap_err(),
            ("GET", _) => records.seen_jobs("9").await.unwrap_err(),
            ("DELETE", _) => records.close("9").await.unwrap_err(),
            _ => records.open("9", &jobs).await.unwrap_err(),
        };
        assert_eq!(
            error.is_retryable(),
            retryable,
            "{method} {path} {failure:?}: {error}"
        );
    }
    records.close("9").await.unwrap();
    fake.fail_next("PUT", record, Failure::Status(503));
    let error = records.open("9", &jobs).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("scaleway s3 PUT {record}: 503 SlowDown Please reduce your request rate.")
    );
    assert!(records.open("9", &jobs).await.unwrap(), "the repeat opens");
}

/// The ledger lists `ciburst/intents/<pool>/`, and no pool tag starts with an underscore, so
/// the records neither show up in a burst nor add pages to its listing.
#[tokio::test]
async fn the_ledger_reads_past_the_records() {
    let (fake, records) = records().await;
    let ledger = ScalewayLedger::new(fake.clone(), "fr-par", BUCKET, &cache_key());
    for job in ["1", "2", "3", "4"] {
        ledger.record("vm", NOW, job).await.unwrap();
    }
    let from = fake.calls().len();
    let before = ledger.since("vm", NOW - 30).await.unwrap();
    let pages = listings_since(&fake, from).len();
    let intent = |job: &str| Intent {
        at: NOW,
        job: job.to_string(),
    };
    assert_eq!(before, [intent("1"), intent("2"), intent("3"), intent("4")]);

    for pipeline in 1..=10 {
        let pipeline = pipeline.to_string();
        records
            .open(&pipeline, &vm_jobs(&[("vm", &["1", "2", "3"])]))
            .await
            .unwrap();
        for job in ["1", "2", "3"] {
            records.seen(&pipeline, job).await.unwrap();
        }
    }
    let from = fake.calls().len();
    assert_eq!(ledger.since("vm", NOW - 30).await.unwrap(), before);
    assert_eq!(listings_since(&fake, from).len(), pages);

    let config = r#"{
      "name": "demo",
      "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
      "host": { "type": "scaleway-function" },
      "provider": { "type": "scaleway" },
      "defaultPool": "_pipelines",
      "pools": [ { "tag": "_pipelines", "image": "img/docker", "runtime": "instances" } ],
      "cache": { "type": "s3" }
    }"#;
    let error = parse_config(config).unwrap_err().to_string();
    assert!(
        error.contains("pools[0].tag: lowercase letters, digits and hyphens"),
        "{error}"
    );
}

/// Destroy empties the cache bucket page by page, the records with everything else.
#[tokio::test]
async fn destroy_deletes_the_records_with_the_bucket() {
    let (fake, records) = records().await;
    for pipeline in ["9", "10"] {
        records
            .open(pipeline, &vm_jobs(&[("vm", &["1", "2"])]))
            .await
            .unwrap();
        records.seen(pipeline, "1").await.unwrap();
    }
    assert_eq!(keys(&fake).len(), 4);
    owner(&fake).empty_and_delete(BUCKET).await.unwrap();
    assert!(fake.buckets().is_empty());
}

#[tokio::test]
async fn open_pipelines_lists_the_records_without_their_seen_jobs() {
    let (fake, records) = records().await;
    records
        .open("9", &vm_jobs(&[("vm", &["11", "12"]), ("big", &["13"])]))
        .await
        .unwrap();
    records
        .open("90", &vm_jobs(&[("vm", &["21"])]))
        .await
        .unwrap();
    records.seen("9", "11").await.unwrap();
    records.seen("9", "13").await.unwrap();
    let owner = owner(&fake);
    for key in [
        "ciburst/intents/_pipelines/7/seen/1",
        "ciburst/intents/_pipelines/x/vm-jobs.json",
        "ciburst/intents/_pipelines/9/notes",
    ] {
        owner.put_object(BUCKET, key, Vec::new()).await.unwrap();
    }

    let open = records.open_pipelines().await.unwrap();
    let found: Vec<(String, VmJobs)> = open.into_iter().map(|p| (p.pipeline, p.unseen)).collect();
    assert_eq!(
        found,
        [
            ("9".to_string(), vm_jobs(&[("vm", &["12"])])),
            ("90".to_string(), vm_jobs(&[("vm", &["21"])])),
        ]
    );

    records.close("9").await.unwrap();
    let open = records.open_pipelines().await.unwrap();
    assert_eq!(
        open.iter().map(|p| p.pipeline.as_str()).collect::<Vec<_>>(),
        ["90"]
    );
}
