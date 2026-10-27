//! `ScalewayFleetStore` against the S3 fake (spec 8.1): the key layout, the config
//! and its MAC, member records sealed at rest, listings across pages, replace and delete, the
//! errors of a wrong key or an altered object, and a lifecycle that never expires the fleet.

use std::collections::BTreeMap;
use std::sync::Arc;

use ciburst_core::fleet::{FleetStore, Member, MemberLane};
use ciburst_core::types::KeyPair;
use ciburst_scaleway::fleet::{
    CONFIG_KEY, FleetKey, MEMBER_PREFIX, ScalewayFleetStore, base64_decode, member_key,
};
use ciburst_scaleway::ledger::MAX_PAGES;
use ciburst_scaleway::provider::s3::S3;
use ciburst_scaleway::testing::s3_api::FakeS3;
use ciburst_scaleway::testing::scaleway_fake::{FAKE_ACCESS_KEY, FAKE_SECRET_KEY, Failure};
use serde_json::Value;

const BUCKET: &str = "demo-cache-p-1";
const CACHE_ACCESS: &str = "SCWCACHEACCESS000000";
const CACHE_SECRET: &str = "cache-secret-key-value";
const FLEET_SECRET: &str = "fleetsecretfleetsecretfleetsecr1";
const OTHER_SECRET: &str = "othersecretothersecretothersecr2";
const HOOK: &str = "hook-secret-of-member";
const TOKEN: &str = "glrt-member-runner-token";

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

fn store_on(fake: &Arc<FakeS3>, secret: &str) -> ScalewayFleetStore {
    ScalewayFleetStore::new(
        fake.clone(),
        "fr-par",
        BUCKET,
        &cache_key(),
        FleetKey::from_secret(secret).unwrap(),
    )
}

async fn store() -> (Arc<FakeS3>, ScalewayFleetStore) {
    let fake = Arc::new(FakeS3::new());
    owner(&fake).create_bucket(BUCKET).await.unwrap();
    fake.add_key(CACHE_ACCESS, CACHE_SECRET);
    let store = store_on(&fake, FLEET_SECRET);
    (fake, store)
}

fn member(project_id: u64, hook: &str) -> Member {
    Member {
        project_id,
        hook_secret: hook.into(),
        runners: BTreeMap::from([
            ("node24".to_string(), format!("{TOKEN}-lane")),
            ("vm".to_string(), TOKEN.to_string()),
        ]),
        joined_at: 1_800_000_000,
        ..Default::default()
    }
}

fn object(fake: &FakeS3, key: &str) -> Vec<u8> {
    fake.objects(BUCKET)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, body)| body)
        .unwrap_or_else(|| panic!("no object {key}"))
}

fn keys(fake: &FakeS3) -> Vec<String> {
    fake.objects(BUCKET).into_iter().map(|(k, _)| k).collect()
}

fn sorted(mut members: Vec<Member>) -> Vec<Member> {
    members.sort_by_key(|m| m.project_id);
    members
}

#[tokio::test]
async fn the_config_is_plain_and_bound_to_the_fleet_key() {
    let (fake, store) = store().await;
    assert_eq!(store.config().await.unwrap(), None);
    let json = r#"{"name":"demo","pools":[{"tag":"vm"}]}"#;
    store.put_config(json).await.unwrap();
    assert_eq!(keys(&fake), [CONFIG_KEY]);
    assert_eq!(store.config().await.unwrap().as_deref(), Some(json));
    let stored: Value = serde_json::from_slice(&object(&fake, CONFIG_KEY)).unwrap();
    assert_eq!(stored["config"], json, "plain text");
    assert_eq!(
        base64_decode(stored["mac"].as_str().unwrap())
            .unwrap()
            .len(),
        32
    );

    let other = store_on(&fake, OTHER_SECRET);
    let error = other.config().await.unwrap_err().to_string();
    assert!(error.contains(CONFIG_KEY), "{error}");

    // A job holding the cache key rewrites the config but cannot make its MAC.
    let forged = serde_json::json!({
        "config": r#"{"name":"demo","pools":[{"tag":"evil"}]}"#,
        "mac": stored["mac"],
    });
    fake.put_object(BUCKET, CONFIG_KEY, forged.to_string().as_bytes());
    let error = store.config().await.unwrap_err().to_string();
    assert!(
        error.contains(CONFIG_KEY) && error.contains("MAC"),
        "{error}"
    );
    fake.put_object(BUCKET, CONFIG_KEY, json.as_bytes());
    let error = store.config().await.unwrap_err().to_string();
    assert!(error.contains(CONFIG_KEY), "{error}");

    store.put_config("{}").await.unwrap();
    assert_eq!(store.config().await.unwrap().as_deref(), Some("{}"));
}

#[tokio::test]
async fn members_are_sealed_at_rest_and_read_back() {
    let (fake, store) = store().await;
    store.put_member(&member(7, HOOK)).await.unwrap();
    assert_eq!(keys(&fake), ["ciburst/fleet/members/7.json"]);
    let body = object(&fake, &member_key(7));
    let text = String::from_utf8(body.clone()).unwrap();
    assert!(!text.contains(HOOK) && !text.contains(TOKEN), "{text}");
    let stored: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(stored["projectId"], 7);
    assert_eq!(stored["joinedAt"], 1_800_000_000u64);
    assert_eq!(stored["cipher"], "aes-256-gcm");
    let runners = stored["runners"].as_object().unwrap();
    assert_eq!(runners.keys().collect::<Vec<_>>(), ["node24", "vm"]);
    let sealed = base64_decode(runners["vm"].as_str().unwrap()).unwrap();
    assert_eq!(
        sealed.len(),
        12 + TOKEN.len() + 16,
        "nonce, ciphertext, tag"
    );

    assert_eq!(store.members().await.unwrap(), [member(7, HOOK)]);

    // The same values sealed again use fresh nonces.
    store.put_member(&member(7, HOOK)).await.unwrap();
    let again: Value = serde_json::from_slice(&object(&fake, &member_key(7))).unwrap();
    assert_ne!(again["hookSecret"], stored["hookSecret"]);
}

/// spec 8.1: a member's lanes keep their routing fields in clear text and their runner
/// token sealed; a record written before M6.4 reads as a member without lanes.
#[tokio::test]
async fn member_lanes_seal_their_runner_token_and_old_records_have_none() {
    let (fake, store) = store().await;
    let mut with_lanes = member(8, HOOK);
    with_lanes.runners = BTreeMap::from([("vm".to_string(), TOKEN.to_string())]);
    with_lanes.lanes = vec![MemberLane {
        tag: "node".into(),
        image: "node:24".into(),
        jobs: vec!["lint".into()],
        max_runs: 3,
        definition_id: "def-1".into(),
        runner_token: "glrt-lane-token-of-member".into(),
    }];
    with_lanes.ignore_jobs = vec!["pages".into()];
    store.put_member(&with_lanes).await.unwrap();
    let text = String::from_utf8(object(&fake, &member_key(8))).unwrap();
    assert!(!text.contains("glrt-lane-token-of-member"), "{text}");
    let stored: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(stored["lanes"][0]["tag"], "node");
    assert_eq!(stored["lanes"][0]["jobs"], serde_json::json!(["lint"]));
    assert_eq!(stored["lanes"][0]["maxRuns"], 3);
    assert_eq!(stored["ignoreJobs"], serde_json::json!(["pages"]));
    assert_eq!(store.members().await.unwrap(), [with_lanes.clone()]);

    // The sealed token is bound to its member and lane: moved to another lane, it fails.
    let mut moved = stored.clone();
    moved["lanes"][0]["tag"] = "rust".into();
    owner(&fake)
        .put_object(BUCKET, &member_key(8), serde_json::to_vec(&moved).unwrap())
        .await
        .unwrap();
    assert!(store.members().await.is_err());

    let mut old = stored;
    old.as_object_mut().unwrap().remove("lanes");
    old.as_object_mut().unwrap().remove("ignoreJobs");
    owner(&fake)
        .put_object(BUCKET, &member_key(8), serde_json::to_vec(&old).unwrap())
        .await
        .unwrap();
    let read = store.members().await.unwrap();
    assert!(read[0].lanes.is_empty() && read[0].ignore_jobs.is_empty());
    assert_eq!(read[0].runners, with_lanes.runners);
}

/// The S3 fake with a round trip of 100 ms that counts the requests in flight.
struct Slow {
    inner: Arc<FakeS3>,
    now: std::sync::atomic::AtomicUsize,
    most: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ciburst_core::http::HttpClient for Slow {
    async fn send(
        &self,
        request: ciburst_core::http::Request,
    ) -> ciburst_core::Result<ciburst_core::http::Response> {
        use std::sync::atomic::Ordering::SeqCst;
        let now = self.now.fetch_add(1, SeqCst) + 1;
        self.most.fetch_max(now, SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let response = self.inner.send(request).await;
        self.now.fetch_sub(1, SeqCst);
        response
    }
}

/// Found in the M6.3 drill: a cold dispatcher read the members one by one before answering, and
/// GitLab gave up on its first events after 10 s.
#[tokio::test(start_paused = true)]
async fn members_are_read_at_once() {
    let (fake, store) = store().await;
    for id in [7, 8, 9] {
        store.put_member(&member(id, HOOK)).await.unwrap();
    }
    let slow = Arc::new(Slow {
        inner: fake.clone(),
        now: Default::default(),
        most: Default::default(),
    });
    let store = ScalewayFleetStore::new(
        slow.clone(),
        "fr-par",
        BUCKET,
        &cache_key(),
        FleetKey::from_secret(FLEET_SECRET).unwrap(),
    );
    let started = tokio::time::Instant::now();
    assert_eq!(store.members().await.unwrap().len(), 3);
    assert_eq!(slow.most.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(250),
        "one listing and one round of reads"
    );
}

#[tokio::test]
async fn joining_twice_replaces_and_leaving_deletes() {
    let (fake, store) = store().await;
    store.put_member(&member(7, "first")).await.unwrap();
    store.put_member(&member(8, "other")).await.unwrap();
    store.put_member(&member(7, "second")).await.unwrap();
    assert_eq!(
        sorted(store.members().await.unwrap()),
        [member(7, "second"), member(8, "other")]
    );
    store.delete_member(8).await.unwrap();
    store.delete_member(8).await.unwrap();
    store.delete_member(9).await.unwrap();
    assert_eq!(store.members().await.unwrap(), [member(7, "second")]);
    assert_eq!(keys(&fake), [member_key(7)]);
    store.delete_member(7).await.unwrap();
    assert_eq!(store.members().await.unwrap(), []);
}

#[tokio::test]
async fn members_follow_the_pages_up_to_the_limit() {
    let (fake, store) = store().await;
    store.put_config("{}").await.unwrap();
    let ids: Vec<u64> = (1..=8).collect();
    for id in &ids {
        store
            .put_member(&member(*id, &format!("hook-{id}")))
            .await
            .unwrap();
    }
    // Objects beside the members stay out of the listing.
    fake.put_object(BUCKET, "ciburst/intents/vm/1-1", b"");
    let from = fake.calls().len();
    let members = sorted(store.members().await.unwrap());
    assert_eq!(
        members.iter().map(|m| m.project_id).collect::<Vec<_>>(),
        ids
    );
    assert_eq!(members[4].hook_secret, "hook-5");
    let listings: Vec<String> = fake.calls()[from..]
        .iter()
        .filter(|c| c.method == "GET" && c.query.contains("list-type=2"))
        .map(|c| c.query.clone())
        .collect();
    assert_eq!(listings.len(), 3, "8 keys at 3 per page: {listings:?}");
    assert!(
        listings
            .iter()
            .all(|q| q.contains("prefix=ciburst%2Ffleet%2Fmembers%2F")),
        "{listings:?}"
    );

    fake.set_page_size(1);
    for id in 9..=(MAX_PAGES as u64 + 1) {
        store.put_member(&member(id, "x")).await.unwrap();
    }
    let error = store.members().await.unwrap_err().to_string();
    assert!(error.contains("pages"), "{error}");
}

#[tokio::test]
async fn a_wrong_key_or_an_altered_record_names_the_member_never_the_secret() {
    let (fake, store) = store().await;
    store.put_member(&member(7, HOOK)).await.unwrap();
    store.put_member(&member(8, "hook-8")).await.unwrap();
    let key_7 = member_key(7);
    let error = store_on(&fake, OTHER_SECRET)
        .members()
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("ciburst/fleet/members/"), "{error}");
    assert!(
        !error.contains(HOOK) && !error.contains(OTHER_SECRET),
        "{error}"
    );

    let original: Value = serde_json::from_slice(&object(&fake, &key_7)).unwrap();
    let other: Value = serde_json::from_slice(&object(&fake, &member_key(8))).unwrap();
    let altered = |edit: &dyn Fn(&mut Value)| {
        let mut value = original.clone();
        edit(&mut value);
        value.to_string()
    };
    let cases: Vec<(&str, String)> = vec![
        (
            "hook secret of another member",
            altered(&|v| v["hookSecret"] = other["hookSecret"].clone()),
        ),
        (
            "runner token moved to another pool",
            altered(&|v| v["runners"]["node24"] = original["runners"]["vm"].clone()),
        ),
        (
            "record of another project",
            altered(&|v| v["projectId"] = 8.into()),
        ),
        ("another cipher", altered(&|v| v["cipher"] = "none".into())),
        (
            "plain text secret",
            altered(&|v| v["hookSecret"] = HOOK.into()),
        ),
        ("not json", "{".to_string()),
    ];
    for (case, body) in cases {
        fake.put_object(BUCKET, &key_7, body.as_bytes());
        let error = store.members().await.unwrap_err().to_string();
        assert!(error.contains(&key_7), "{case}: {error}");
        assert!(
            !error.contains(HOOK) && !error.contains(TOKEN) && !error.contains(FLEET_SECRET),
            "{case}: {error}"
        );
    }

    fake.put_object(BUCKET, &key_7, original.to_string().as_bytes());
    assert_eq!(store.members().await.unwrap().len(), 2);
    let stray = format!("{MEMBER_PREFIX}07.json");
    fake.put_object(BUCKET, &stray, original.to_string().as_bytes());
    let error = store.members().await.unwrap_err().to_string();
    assert!(error.contains(&stray), "{error}");
}

#[tokio::test]
async fn failures_that_may_pass_on_a_repeat_are_retryable() {
    let (fake, store) = store().await;
    store.put_member(&member(7, HOOK)).await.unwrap();
    let path = format!("/{BUCKET}/{CONFIG_KEY}");
    fake.fail_next("GET", &path, Failure::Status(503));
    let error = store.config().await.unwrap_err();
    assert!(error.is_retryable(), "{error}");
    assert_eq!(store.config().await.unwrap(), None);
    fake.fail_next("GET", &format!("/{BUCKET}"), Failure::Status(500));
    assert!(store.members().await.unwrap_err().is_retryable());
    assert_eq!(store.members().await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_bucket_lifecycle_never_expires_the_fleet() {
    let (fake, store) = store().await;
    owner(&fake).put_lifecycle(BUCKET, 7).await.unwrap();
    store.put_config("{}").await.unwrap();
    store.put_member(&member(7, HOOK)).await.unwrap();
    for key in [CONFIG_KEY.to_string(), member_key(7)] {
        assert_eq!(fake.expiry_days(BUCKET, &key), None, "{key}");
    }
    for key in [
        "ciburst/intents/vm/0000000001-00000000000000000001",
        "runner/token/project/1/cache.zip",
        "project/1/cache.zip",
    ] {
        assert_eq!(fake.expiry_days(BUCKET, key), Some(7), "{key}");
    }
    for key in ["ciburst/agent/vm-agent", "ciburst/runner/gitlab-runner"] {
        assert_eq!(fake.expiry_days(BUCKET, key), None, "{key}");
    }
}
