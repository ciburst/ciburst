//! `ScalewaySchedule` against the S3 fake (M6.2 rules R2 to R4): the key layout and the calls
//! of each method, the same answers as `MemorySchedule`, forged entries skipped, listings across
//! pages, the failures that may be retried (M7 hardening rule R4), and the ledger and the warm
//! start records reading past the schedule.

use std::sync::Arc;

use ciburst_core::ledger::{
    Intent, Ledger, MemorySchedule, PipelineRecords, Queued, Running, START_MARKER_S, Schedule,
    VmJobs,
};
use ciburst_core::types::KeyPair;
use ciburst_scaleway::ledger::{MAX_PAGES, ScalewayLedger};
use ciburst_scaleway::provider::s3::S3;
use ciburst_scaleway::records::ScalewayPipelineRecords;
use ciburst_scaleway::schedule::{ScalewaySchedule, Stats, stats_key};
use ciburst_scaleway::testing::s3_api::FakeS3;
use ciburst_scaleway::testing::scaleway_fake::{FAKE_ACCESS_KEY, FAKE_SECRET_KEY, Failure};

const BUCKET: &str = "demo-cache-p-1";
const CACHE_ACCESS: &str = "SCWCACHEACCESS000000";
const CACHE_SECRET: &str = "cache-secret-key-value";
const NOW: u64 = 1_800_000_000;
const QUEUE_VM: &str = "ciburst/intents/_queue/vm/";
const RUNNING_VM: &str = "ciburst/intents/_running/vm/";
const MARKER_VM: &str = "ciburst/intents/_queue/vm/.started";

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

/// A fake with the cache bucket and the cache key, and the schedule on it.
async fn schedule() -> (Arc<FakeS3>, ScalewaySchedule) {
    let fake = Arc::new(FakeS3::new());
    owner(&fake).create_bucket(BUCKET).await.unwrap();
    fake.add_key(CACHE_ACCESS, CACHE_SECRET);
    let schedule = ScalewaySchedule::new(fake.clone(), "fr-par", BUCKET, &cache_key());
    (fake, schedule)
}

fn queued(job: &str, at: u64) -> Queued {
    Queued {
        job: job.into(),
        name: "build-image".into(),
        pipeline: "9".into(),
        at,
    }
}

fn running(job: &str, started: u64) -> Running {
    Running {
        job: job.into(),
        name: "build-image".into(),
        started,
    }
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

fn object(fake: &FakeS3, key: &str) -> Option<Vec<u8>> {
    fake.objects(BUCKET)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, body)| body)
}

fn keys(fake: &FakeS3) -> Vec<String> {
    fake.objects(BUCKET)
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

fn path(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

fn by_job<T>(mut entries: Vec<T>, job: impl Fn(&T) -> &str) -> Vec<T> {
    entries.sort_by(|a, b| (job(a).len(), job(a)).cmp(&(job(b).len(), job(b))));
    entries
}

#[tokio::test]
async fn enqueue_puts_one_object_per_job_and_queued_reads_them_back() {
    let (fake, schedule) = schedule().await;
    assert_eq!(schedule.bucket(), BUCKET);
    let from = fake.calls().len();
    schedule.enqueue("vm", &queued("42", NOW)).await.unwrap();
    assert_eq!(
        calls_since(&fake, from),
        [format!("PUT {}", path(&format!("{QUEUE_VM}42")))]
    );
    assert_eq!(
        object(&fake, &format!("{QUEUE_VM}42")).unwrap(),
        br#"{"job":"42","name":"build-image","pipeline":"9","at":1800000000}"#
    );
    schedule
        .enqueue("vm", &queued("42", NOW + 5))
        .await
        .unwrap();
    schedule.enqueue("vm", &queued("7", NOW + 1)).await.unwrap();
    schedule.enqueue("vm-2", &queued("8", NOW)).await.unwrap();
    assert!(schedule.take_start("vm", NOW).await.unwrap());

    let from = fake.calls().len();
    let read = by_job(schedule.queued("vm").await.unwrap(), |q| &q.job);
    assert_eq!(read, [queued("7", NOW + 1), queued("42", NOW + 5)]);
    let calls = calls_since(&fake, from);
    assert_eq!(calls[0], format!("GET /{BUCKET}"));
    let mut gets = calls[1..].to_vec();
    gets.sort();
    assert_eq!(
        gets,
        [
            format!("GET {}", path(&format!("{QUEUE_VM}42"))),
            format!("GET {}", path(&format!("{QUEUE_VM}7"))),
        ],
        "one GET per job, none of the start marker"
    );
    assert_eq!(
        listings_since(&fake, from),
        ["list-type=2&prefix=ciburst%2Fintents%2F_queue%2Fvm%2F"]
    );
    assert_eq!(schedule.queued("vm-2").await.unwrap(), [queued("8", NOW)]);
    assert!(schedule.queued("v").await.unwrap().is_empty());
    assert!(schedule.running("vm").await.unwrap().is_empty());
}

#[tokio::test]
async fn started_moves_the_job_from_the_queue_to_the_running_jobs() {
    let (fake, schedule) = schedule().await;
    schedule.enqueue("vm", &queued("41", NOW)).await.unwrap();
    schedule.enqueue("vm", &queued("42", NOW)).await.unwrap();

    let from = fake.calls().len();
    schedule
        .started("vm", &running("41", NOW + 9))
        .await
        .unwrap();
    assert_eq!(
        calls_since(&fake, from),
        [
            format!("DELETE {}", path(&format!("{QUEUE_VM}41"))),
            format!("PUT {}", path(&format!("{RUNNING_VM}41"))),
        ]
    );
    assert_eq!(
        object(&fake, &format!("{RUNNING_VM}41")).unwrap(),
        br#"{"job":"41","name":"build-image","started":1800000009}"#
    );
    assert_eq!(schedule.queued("vm").await.unwrap(), [queued("42", NOW)]);
    assert_eq!(
        schedule.running("vm").await.unwrap(),
        [running("41", NOW + 9)]
    );

    schedule
        .started("node24", &running("43", NOW))
        .await
        .unwrap();
    assert_eq!(
        schedule.running("node24").await.unwrap(),
        [running("43", NOW)],
        "a job that never queued, such as a lane job, runs too"
    );
    assert_eq!(schedule.running("vm").await.unwrap().len(), 1);
}

#[tokio::test]
async fn finished_deletes_both_entries_and_folds_the_duration_into_the_stats() {
    let (fake, schedule) = schedule().await;
    let stats = stats_key("vm", "build-image").unwrap();
    schedule.started("vm", &running("41", NOW)).await.unwrap();

    let from = fake.calls().len();
    schedule
        .finished("vm", "41", "build-image", Some(60))
        .await
        .unwrap();
    assert_eq!(
        calls_since(&fake, from),
        [
            format!("DELETE {}", path(&format!("{QUEUE_VM}41"))),
            format!("DELETE {}", path(&format!("{RUNNING_VM}41"))),
            format!("GET {}", path(&stats)),
            format!("PUT {}", path(&stats)),
        ]
    );
    assert_eq!(
        object(&fake, &stats).unwrap(),
        br#"{"name":"build-image","typicalS":60,"runs":1}"#
    );
    assert!(schedule.running("vm").await.unwrap().is_empty());
    assert_eq!(
        schedule.typical("vm", "build-image").await.unwrap(),
        Some(60)
    );

    schedule
        .finished("vm", "42", "build-image", Some(160))
        .await
        .unwrap();
    let read: Stats = serde_json::from_slice(&object(&fake, &stats).unwrap()).unwrap();
    assert_eq!((read.typical_s, read.runs), (90, 2));

    let from = fake.calls().len();
    schedule
        .finished("vm", "43", "build-image", None)
        .await
        .unwrap();
    assert_eq!(
        calls_since(&fake, from),
        [
            format!("DELETE {}", path(&format!("{QUEUE_VM}43"))),
            format!("DELETE {}", path(&format!("{RUNNING_VM}43"))),
        ],
        "a canceled or skipped job leaves the stats alone"
    );
    assert_eq!(
        schedule.typical("vm", "build-image").await.unwrap(),
        Some(90)
    );
    assert_eq!(schedule.typical("vm", "lint").await.unwrap(), None);
    assert_eq!(schedule.typical("vm-2", "build-image").await.unwrap(), None);
}

#[tokio::test]
async fn the_start_marker_holds_for_its_seconds() {
    let (fake, schedule) = schedule().await;
    let from = fake.calls().len();
    assert!(schedule.take_start("vm", NOW).await.unwrap());
    assert_eq!(
        calls_since(&fake, from),
        [
            format!("GET {}", path(MARKER_VM)),
            format!("PUT {}", path(MARKER_VM)),
        ]
    );
    assert_eq!(object(&fake, MARKER_VM).unwrap(), b"1800000000");

    let from = fake.calls().len();
    assert!(
        !schedule
            .take_start("vm", NOW + START_MARKER_S - 1)
            .await
            .unwrap()
    );
    assert_eq!(
        calls_since(&fake, from),
        [format!("GET {}", path(MARKER_VM))]
    );
    assert!(schedule.take_start("vm-2", NOW + 1).await.unwrap());
    assert!(
        schedule
            .take_start("vm", NOW + START_MARKER_S)
            .await
            .unwrap()
    );
    let taken = (NOW + START_MARKER_S).to_string();
    assert_eq!(object(&fake, MARKER_VM).unwrap(), taken.as_bytes());

    assert!(
        !schedule.take_start("vm", NOW + 1).await.unwrap(),
        "a marker a little ahead holds, as in the memory schedule"
    );
    for forged in [
        b"x".to_vec(),
        Vec::new(),
        (NOW + 2 * START_MARKER_S).to_string().into_bytes(),
        u64::MAX.to_string().into_bytes(),
    ] {
        fake.put_object(BUCKET, MARKER_VM, &forged);
        assert!(
            schedule.take_start("vm", NOW).await.unwrap(),
            "{}",
            String::from_utf8_lossy(&forged)
        );
        assert_eq!(object(&fake, MARKER_VM).unwrap(), b"1800000000");
    }
}

/// What a scenario of every method answers; the same for every `Schedule`.
async fn transcript(schedule: &dyn Schedule) -> Vec<String> {
    let mut out = Vec::new();
    let mut note = |line: String| out.push(line);
    let q = |schedule_queued: Vec<Queued>| {
        by_job(schedule_queued, |q| &q.job)
            .iter()
            .map(|q| format!("{}@{}", q.job, q.at))
            .collect::<Vec<_>>()
            .join(",")
    };
    let r = |schedule_running: Vec<Running>| {
        by_job(schedule_running, |r| &r.job)
            .iter()
            .map(|r| format!("{}@{}", r.job, r.started))
            .collect::<Vec<_>>()
            .join(",")
    };
    for (pool, job, at) in [
        ("vm", "41", 100),
        ("vm", "42", 101),
        ("vm", "42", 102),
        ("big", "43", 100),
        ("vm", "18446744073709551615", 103),
    ] {
        schedule.enqueue(pool, &queued(job, at)).await.unwrap();
    }
    note(format!(
        "queued vm {}",
        q(schedule.queued("vm").await.unwrap())
    ));
    schedule.started("vm", &running("41", 110)).await.unwrap();
    schedule.started("vm", &running("44", 111)).await.unwrap();
    note(format!(
        "queued vm {}",
        q(schedule.queued("vm").await.unwrap())
    ));
    note(format!(
        "running vm {}",
        r(schedule.running("vm").await.unwrap())
    ));
    note(format!(
        "running big {}",
        r(schedule.running("big").await.unwrap())
    ));
    for (job, name, duration) in [
        ("41", "build-image", Some(400)),
        ("42", "build-image", None),
        ("44", "build-image", Some(0)),
        ("45", "lint", Some(31)),
    ] {
        schedule.finished("vm", job, name, duration).await.unwrap();
    }
    note(format!(
        "queued vm {}",
        q(schedule.queued("vm").await.unwrap())
    ));
    note(format!(
        "running vm {}",
        r(schedule.running("vm").await.unwrap())
    ));
    for (pool, name) in [
        ("vm", "build-image"),
        ("vm", "lint"),
        ("vm", "deploy"),
        ("big", "build-image"),
    ] {
        note(format!(
            "typical {pool} {name} {:?}",
            schedule.typical(pool, name).await.unwrap()
        ));
    }
    for (pool, now) in [
        ("vm", 1000),
        ("vm", 1014),
        ("big", 1014),
        ("vm", 1015),
        ("vm", 1029),
        ("vm", 1030),
    ] {
        note(format!(
            "take_start {pool} {now} {}",
            schedule.take_start(pool, now).await.unwrap()
        ));
    }
    out
}

#[tokio::test]
async fn the_schedule_answers_like_the_memory_schedule() {
    let (_, schedule) = schedule().await;
    let reference = transcript(&MemorySchedule::new()).await;
    assert_eq!(transcript(&schedule).await, reference);
    assert_eq!(
        reference,
        [
            "queued vm 41@100,42@102,18446744073709551615@103",
            "queued vm 42@102,18446744073709551615@103",
            "running vm 41@110,44@111",
            "running big ",
            "queued vm 18446744073709551615@103",
            "running vm ",
            "typical vm build-image Some(280)",
            "typical vm lint Some(31)",
            "typical vm deploy None",
            "typical big build-image None",
            "take_start vm 1000 true",
            "take_start vm 1014 false",
            "take_start big 1014 true",
            "take_start vm 1015 true",
            "take_start vm 1029 false",
            "take_start vm 1030 true",
        ]
    );
}

/// Every job can write the bucket: a read skips what is not the entry its key names, and the
/// stats of a name count only for that name.
#[tokio::test]
async fn reads_skip_forged_entries() {
    let (fake, schedule) = schedule().await;
    schedule.enqueue("vm", &queued("42", NOW)).await.unwrap();
    schedule.started("vm", &running("43", NOW)).await.unwrap();
    for (key, body) in [
        (format!("{QUEUE_VM}7"), b"not json".to_vec()),
        (
            format!("{QUEUE_VM}8"),
            serde_json::to_vec(&queued("9", NOW)).unwrap(),
        ),
        (
            format!("{QUEUE_VM}10"),
            serde_json::to_vec(&running("10", NOW)).unwrap(),
        ),
        (format!("{QUEUE_VM}042"), b"{}".to_vec()),
        (
            format!("{QUEUE_VM}x"),
            serde_json::to_vec(&queued("1", NOW)).unwrap(),
        ),
        (format!("{RUNNING_VM}11"), b"[]".to_vec()),
        (
            format!("{RUNNING_VM}12"),
            serde_json::to_vec(&running("13", NOW)).unwrap(),
        ),
    ] {
        fake.put_object(BUCKET, &key, &body);
    }
    assert_eq!(schedule.queued("vm").await.unwrap(), [queued("42", NOW)]);
    assert_eq!(schedule.running("vm").await.unwrap(), [running("43", NOW)]);

    let stats = stats_key("vm", "build-image").unwrap();
    let other = br#"{"name":"other","typicalS":5,"runs":3}"#;
    fake.put_object(BUCKET, &stats, other);
    assert_eq!(schedule.typical("vm", "build-image").await.unwrap(), None);
    schedule
        .finished("vm", "42", "build-image", Some(50))
        .await
        .unwrap();
    assert_eq!(
        object(&fake, &stats).unwrap(),
        br#"{"name":"build-image","typicalS":50,"runs":1}"#
    );
    fake.put_object(BUCKET, &stats, b"garbage");
    assert_eq!(schedule.typical("vm", "build-image").await.unwrap(), None);
    schedule
        .finished("vm", "42", "build-image", Some(70))
        .await
        .unwrap();
    assert_eq!(
        schedule.typical("vm", "build-image").await.unwrap(),
        Some(70)
    );
}

#[tokio::test]
async fn listings_follow_continuation_tokens_up_to_max_pages() {
    let (fake, schedule) = schedule().await;
    fake.set_page_size(2);
    let jobs: Vec<String> = (1..=7).map(|j| j.to_string()).collect();
    for job in &jobs {
        schedule.enqueue("vm", &queued(job, NOW)).await.unwrap();
        schedule.started("vm-2", &running(job, NOW)).await.unwrap();
    }
    assert!(schedule.take_start("vm", NOW).await.unwrap());
    let from = fake.calls().len();
    let read: Vec<String> = by_job(schedule.queued("vm").await.unwrap(), |q| &q.job)
        .into_iter()
        .map(|q| q.job)
        .collect();
    assert_eq!(read, jobs);
    let queries = listings_since(&fake, from);
    assert_eq!(queries.len(), 4, "{queries:?}");
    for (index, query) in queries.iter().enumerate() {
        assert_eq!(query.contains("continuation-token="), index > 0, "{query}");
        assert!(!query.contains("start-after"), "{query}");
    }
    assert_eq!(schedule.running("vm-2").await.unwrap().len(), 7);
}

#[tokio::test]
async fn a_listing_follows_at_most_max_pages() {
    let (fake, schedule) = schedule().await;
    fake.set_page_size(1);
    for job in 1..=MAX_PAGES {
        fake.put_object(BUCKET, &format!("{RUNNING_VM}{job}"), b"{}");
    }
    assert!(schedule.running("vm").await.unwrap().is_empty());
    fake.put_object(BUCKET, &format!("{RUNNING_VM}999"), b"{}");
    let error = schedule.running("vm").await.unwrap_err().to_string();
    assert_eq!(
        error,
        format!("scaleway s3 list {BUCKET}/{RUNNING_VM}: more than {MAX_PAGES} pages")
    );
}

#[tokio::test]
async fn bad_input_sends_nothing_and_bucket_errors_name_the_cause() {
    let (fake, schedule) = schedule().await;
    let calls = fake.calls().len();
    for (pool, job) in [
        ("vm", "042"),
        ("vm", "x"),
        ("vm", ""),
        ("a/b", "1"),
        ("_queue", "1"),
    ] {
        let entry = queued(job, NOW);
        assert!(
            schedule.enqueue(pool, &entry).await.is_err(),
            "{pool} {job}"
        );
        let entry = running(job, NOW);
        assert!(
            schedule.started(pool, &entry).await.is_err(),
            "{pool} {job}"
        );
        let finished = schedule.finished(pool, job, "build", Some(1)).await;
        assert!(finished.is_err(), "{pool} {job}");
    }
    for pool in ["", "VM", "a/b", "_stats"] {
        assert!(schedule.queued(pool).await.is_err(), "{pool}");
        assert!(schedule.running(pool).await.is_err(), "{pool}");
        assert!(schedule.typical(pool, "build").await.is_err(), "{pool}");
        assert!(schedule.take_start(pool, NOW).await.is_err(), "{pool}");
    }
    assert_eq!(fake.calls().len(), calls, "nothing sent");
    assert_eq!(
        schedule
            .enqueue("vm", &queued("042", NOW))
            .await
            .unwrap_err()
            .to_string(),
        "scaleway schedule: job id \"042\" is not a number"
    );
    assert_eq!(
        schedule.queued("VM").await.unwrap_err().to_string(),
        "scaleway schedule: pool \"VM\" is not a pool tag"
    );

    let elsewhere = ScalewaySchedule::new(fake.clone(), "fr-par", "missing-bucket", &cache_key());
    for error in [
        elsewhere
            .enqueue("vm", &queued("1", NOW))
            .await
            .unwrap_err(),
        elsewhere.queued("vm").await.unwrap_err(),
        elsewhere.running("vm").await.unwrap_err(),
        elsewhere.typical("vm", "build").await.unwrap_err(),
        elsewhere.take_start("vm", NOW).await.unwrap_err(),
        elsewhere
            .finished("vm", "1", "build", None)
            .await
            .unwrap_err(),
    ] {
        assert!(error.to_string().contains("404 NoSuchBucket"), "{error}");
    }

    let wrong = KeyPair {
        access_key: CACHE_ACCESS.into(),
        secret_key: "not-the-secret".into(),
    };
    let forged = ScalewaySchedule::new(fake.clone(), "fr-par", BUCKET, &wrong);
    let error = forged.take_start("vm", NOW).await.unwrap_err().to_string();
    assert!(error.contains("403 SignatureDoesNotMatch"), "{error}");
    assert!(fake.objects(BUCKET).is_empty());
}

/// M7 hardening rule R4 for the schedule: 429 and 503 may pass on a repeat for every call,
/// another 5xx and a transport timeout only for reads.
#[tokio::test]
async fn failures_are_retryable_where_a_repeat_may_pass() {
    #[derive(Debug, Clone, Copy)]
    enum Op {
        Enqueue,
        Queued,
        Started,
        Finished,
        Typical,
        TakeStart,
    }
    let (fake, schedule) = schedule().await;
    let queue = format!("/{BUCKET}/{QUEUE_VM}*");
    let running_any = format!("/{BUCKET}/{RUNNING_VM}*");
    let marker = path(MARKER_VM);
    let stats = path(&stats_key("vm", "build-image").unwrap());
    let listing = format!("/{BUCKET}");
    for (op, method, path, failure, retryable) in [
        (Op::Enqueue, "PUT", &queue, Failure::Status(429), true),
        (Op::Enqueue, "PUT", &queue, Failure::Status(500), false),
        (Op::Enqueue, "PUT", &queue, Failure::Timeout, false),
        (Op::Queued, "GET", &listing, Failure::Status(500), true),
        (Op::Queued, "GET", &listing, Failure::Timeout, true),
        (Op::Started, "DELETE", &queue, Failure::Status(503), true),
        (Op::Started, "DELETE", &queue, Failure::Status(500), false),
        (Op::Started, "PUT", &running_any, Failure::Status(503), true),
        (Op::Typical, "GET", &stats, Failure::Status(502), true),
        (
            Op::Finished,
            "DELETE",
            &running_any,
            Failure::Status(429),
            true,
        ),
        (Op::Finished, "PUT", &stats, Failure::Status(500), false),
        (Op::TakeStart, "GET", &marker, Failure::Timeout, true),
        (Op::TakeStart, "PUT", &marker, Failure::Status(503), true),
        (Op::TakeStart, "PUT", &marker, Failure::Timeout, false),
    ] {
        fake.put_object(BUCKET, MARKER_VM, b"1");
        fake.fail_next(method, path, failure);
        let error = match op {
            Op::Enqueue => schedule.enqueue("vm", &queued("42", NOW)).await,
            Op::Queued => schedule.queued("vm").await.map(drop),
            Op::Started => schedule.started("vm", &running("42", NOW)).await,
            Op::Finished => schedule.finished("vm", "42", "build-image", Some(5)).await,
            Op::Typical => schedule.typical("vm", "build-image").await.map(drop),
            Op::TakeStart => schedule.take_start("vm", NOW).await.map(drop),
        }
        .unwrap_err();
        assert_eq!(
            error.is_retryable(),
            retryable,
            "{method} {path} {failure:?}: {error}"
        );
    }
    fake.put_object(BUCKET, MARKER_VM, b"1");
    fake.fail_next("PUT", &marker, Failure::Status(503));
    let error = schedule.take_start("vm", NOW).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("scaleway s3 PUT {marker}: 503 SlowDown Please reduce your request rate.")
    );
    assert!(
        schedule.take_start("vm", NOW).await.unwrap(),
        "the repeat takes it"
    );
}

/// The ledger lists `ciburst/intents/<pool>/` and the records `ciburst/intents/_pipelines/`,
/// and no pool tag starts with an underscore, so the schedule neither shows up in a burst or a
/// pipeline's markers nor adds pages to their listings.
#[tokio::test]
async fn the_ledger_and_the_records_read_past_the_schedule() {
    let (fake, schedule) = schedule().await;
    let ledger = ScalewayLedger::new(fake.clone(), "fr-par", BUCKET, &cache_key());
    let records = ScalewayPipelineRecords::new(fake.clone(), "fr-par", BUCKET, &cache_key());
    for job in ["1", "2", "3", "4"] {
        ledger.record("vm", NOW, job).await.unwrap();
    }
    let jobs: VmJobs = [("vm".to_string(), vec!["1".to_string()])].into();
    records.open("9", &jobs).await.unwrap();
    records.seen("9", "1").await.unwrap();
    let from = fake.calls().len();
    let before = ledger.since("vm", NOW - 30).await.unwrap();
    let ledger_pages = listings_since(&fake, from).len();
    let from = fake.calls().len();
    assert_eq!(records.seen_jobs("9").await.unwrap(), ["1"]);
    let records_pages = listings_since(&fake, from).len();
    let intent = |job: &str| Intent {
        at: NOW,
        job: job.to_string(),
    };
    assert_eq!(before, [intent("1"), intent("2"), intent("3"), intent("4")]);

    for job in 10..40 {
        let job = job.to_string();
        schedule.enqueue("vm", &queued(&job, NOW)).await.unwrap();
        schedule.started("vm", &running(&job, NOW)).await.unwrap();
        schedule
            .finished("vm", &job, &format!("name-{job}"), Some(5))
            .await
            .unwrap();
        schedule.started("vm", &running(&job, NOW)).await.unwrap();
        schedule.enqueue("vm", &queued(&job, NOW)).await.unwrap();
    }
    assert!(schedule.take_start("vm", NOW).await.unwrap());
    assert!(keys(&fake).len() > 90);

    let from = fake.calls().len();
    assert_eq!(ledger.since("vm", NOW - 30).await.unwrap(), before);
    assert_eq!(listings_since(&fake, from).len(), ledger_pages);
    let from = fake.calls().len();
    assert_eq!(records.seen_jobs("9").await.unwrap(), ["1"]);
    assert_eq!(listings_since(&fake, from).len(), records_pages);
    assert_eq!(schedule.queued("vm").await.unwrap().len(), 30);
    assert_eq!(schedule.running("vm").await.unwrap().len(), 30);
}

/// Destroy empties the cache bucket page by page, the schedule with everything else.
#[tokio::test]
async fn destroy_deletes_the_schedule_with_the_bucket() {
    let (fake, schedule) = schedule().await;
    schedule.enqueue("vm", &queued("1", NOW)).await.unwrap();
    schedule.started("vm", &running("2", NOW)).await.unwrap();
    schedule
        .finished("vm", "3", "build-image", Some(5))
        .await
        .unwrap();
    assert!(schedule.take_start("vm", NOW).await.unwrap());
    assert_eq!(keys(&fake).len(), 4);
    owner(&fake).empty_and_delete(BUCKET).await.unwrap();
    assert!(fake.buckets().is_empty());
}
