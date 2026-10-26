//! VM runs on Hetzner Cloud servers, spec sections 6.1 and 6.2: the labels that carry their
//! state, run ids, server names, statuses and the RFC 3339 timestamps of the API.
//!
//! Label rules (API overview, "Labels"): keys and values of at most 63 characters, values
//! empty or starting and ending with a letter or digit, with `-`, `_` and `.` in between.

use std::collections::BTreeMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ciburst_core::time::{DateTime, parse_rfc3339};
use ciburst_core::types::{RunState, VmCapacity, VmReport, VmState};
use serde_json::Value;

use crate::RUNTIME;

pub const LABEL_CIBURST: &str = "ciburst";
pub const LABEL_NAME: &str = "ciburst-name";
pub const LABEL_POOL: &str = "ciburst-pool";
pub const LABEL_JOB: &str = "ciburst-job";
pub const LABEL_STATE: &str = "ciburst-state";
pub const LABEL_FREE: &str = "ciburst-free";
pub const LABEL_SEEN: &str = "ciburst-seen";
pub const LABEL_DRAIN: &str = "ciburst-drain";

/// Longest server name: Hetzner wants an RFC 1123 hostname, and the name has no dots, so it is
/// one hostname label.
pub const MAX_SERVER_NAME: usize = 63;
/// Length of `-<8 hex>` after `<name>-<tag>`.
const SUFFIX_LEN: usize = 9;

pub type Labels = BTreeMap<String, String>;

/// The labels of a server or firewall; missing or malformed labels read as none.
pub fn labels_of(resource: &Value) -> Labels {
    resource["labels"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
        .collect()
}

/// Whether a server belongs to the deployment `name`.
pub fn is_deployment(server: &Value, name: &str) -> bool {
    let labels = labels_of(server);
    labels.get(LABEL_CIBURST).map(String::as_str) == Some("true")
        && labels.get(LABEL_NAME).map(String::as_str) == Some(name)
}

/// The servers of the deployment `name`.
pub fn deployment_selector(name: &str) -> String {
    format!("{LABEL_CIBURST}==true,{LABEL_NAME}=={name}")
}

/// The servers of one pool of the deployment `name`.
pub fn pool_selector(name: &str, tag: &str) -> String {
    format!("{},{LABEL_POOL}=={tag}", deployment_selector(name))
}

/// The selector the firewall is applied to (M4 rule R12).
pub fn firewall_selector(name: &str) -> String {
    format!("{LABEL_NAME}=={name}")
}

/// Rule R12: `ciburst=true`, `ciburst-name`, `ciburst-pool`, `ciburst-job` when the job id is a
/// valid label value, `ciburst-state=starting` and `ciburst-free` (slots minus those taken).
pub fn initial_labels(name: &str, pool: &str, job: Option<&str>, free: u32) -> Labels {
    let mut labels = Labels::from([
        (LABEL_CIBURST.to_string(), "true".to_string()),
        (LABEL_NAME.to_string(), name.to_string()),
        (LABEL_POOL.to_string(), pool.to_string()),
        (
            LABEL_STATE.to_string(),
            VmState::Starting.as_str().to_string(),
        ),
        (LABEL_FREE.to_string(), free.to_string()),
    ]);
    if let Some(job) = job.filter(|j| !j.is_empty() && valid_label_value(j)) {
        labels.insert(LABEL_JOB.to_string(), job.to_string());
    }
    labels
}

/// `labels` after a report at `now`. The drain label keeps the time draining started while the
/// VM stays in `draining`, and disappears in any other state.
pub fn reported(labels: &Labels, report: &VmReport, now: u64) -> Labels {
    let drain_since = (report.state == VmState::Draining).then(|| {
        let was_draining =
            labels.get(LABEL_STATE).map(String::as_str) == Some(VmState::Draining.as_str());
        labels
            .get(LABEL_DRAIN)
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|_| was_draining)
            .unwrap_or(now)
    });
    let mut out = labels.clone();
    out.remove(LABEL_DRAIN);
    out.insert(LABEL_STATE.to_string(), report.state.as_str().to_string());
    out.insert(LABEL_FREE.to_string(), report.free.to_string());
    out.insert(LABEL_SEEN.to_string(), now.to_string());
    if let Some(since) = drain_since {
        out.insert(LABEL_DRAIN.to_string(), since.to_string());
    }
    out
}

/// `labels` with one free slot fewer, never below zero.
pub fn claimed(labels: &Labels) -> Labels {
    let free = labels
        .get(LABEL_FREE)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0)
        .saturating_sub(1);
    let mut out = labels.clone();
    out.insert(LABEL_FREE.to_string(), free.to_string());
    out
}

/// `hetzner:<server name>`.
pub fn run_id(server_name: &str) -> String {
    format!("{RUNTIME}:{server_name}")
}

/// The server name a run id of this provider names; `None` for other ids.
pub fn server_name_of(run_id: &str) -> Option<&str> {
    run_id
        .strip_prefix(RUNTIME)?
        .strip_prefix(':')
        .filter(|name| valid_server_name(name))
}

/// A server as the scheduler sees it; `None` without a name or a readable `created`.
pub fn capacity(server: &Value) -> Option<VmCapacity> {
    let labels = labels_of(server);
    Some(VmCapacity {
        run_id: run_id(server["name"].as_str().filter(|n| !n.is_empty())?),
        state: labels
            .get(LABEL_STATE)
            .and_then(|v| VmState::parse(v))
            .unwrap_or(VmState::Starting),
        free: labels
            .get(LABEL_FREE)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        // Read from the report's spill tag by M6.2 rule R6.
        spill: 0,
        seen: labels.get(LABEL_SEEN).and_then(|v| v.parse().ok()),
        created_at: parse_rfc3339(server["created"].as_str()?)?,
    })
}

/// Unix seconds at which the server started draining, from its drain label.
pub fn drain_since(server: &Value) -> Option<u64> {
    labels_of(server)
        .get(LABEL_DRAIN)
        .and_then(|v| v.parse().ok())
}

/// Server statuses of the API: `initializing` and `starting` are queued, `running`,
/// `migrating` and `rebuilding` run, `stopping`, `off` and `deleting` are done.
pub fn run_state(status: &str) -> RunState {
    match status {
        "initializing" | "starting" => RunState::Queued,
        "running" | "migrating" | "rebuilding" => RunState::Running,
        "stopping" | "off" | "deleting" => RunState::Done,
        _ => RunState::Failed,
    }
}

/// Whether `<name>-<tag>-<8 hex>` fits [`MAX_SERVER_NAME`].
pub fn server_name_fits(name: &str, tag: &str) -> bool {
    name.len() + 1 + tag.len() + SUFFIX_LEN <= MAX_SERVER_NAME
}

/// `<name>-<tag>-<8 hex>`; the suffix only has to differ between servers started together.
pub fn server_name(name: &str, tag: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut hasher = RandomState::new().build_hasher();
    hasher.write(name.as_bytes());
    hasher.write(tag.as_bytes());
    hasher.write_u128(nanos);
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{name}-{tag}-{:08x}", hasher.finish() as u32)
}

/// A label value: empty, or up to 63 characters starting and ending with a letter or digit,
/// with `-`, `_` and `.` in between.
pub fn valid_label_value(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let bytes = value.as_bytes();
    bytes.len() <= 63
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A label key: an optional DNS subdomain prefix of at most 253 characters and `/`, then a name
/// like a non-empty label value.
pub fn valid_label_key(key: &str) -> bool {
    let (prefix, name) = match key.rsplit_once('/') {
        Some((prefix, name)) => (Some(prefix), name),
        None => (None, key),
    };
    let prefix_ok = prefix.is_none_or(|p| {
        !p.is_empty()
            && p.len() <= 253
            && p.as_bytes()[0].is_ascii_alphanumeric()
            && p.as_bytes()[p.len() - 1].is_ascii_alphanumeric()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    });
    prefix_ok && !name.is_empty() && valid_label_value(name)
}

/// One RFC 1123 hostname label: letters, digits and hyphens, not starting or ending with a
/// hyphen, at most [`MAX_SERVER_NAME`] characters.
pub fn valid_server_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_SERVER_NAME
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

/// `YYYY-MM-DDTHH:MM:SS+00:00`, the form the API returns.
pub fn rfc3339(unix: u64) -> String {
    let at = DateTime::from_unix(unix);
    format!("{}T{}+00:00", at.date(), at.time())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T0: u64 = 1_792_800_000;

    fn labels(items: &[(&str, &str)]) -> Labels {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn timestamps_round_trip() {
        assert_eq!(rfc3339(T0), "2026-10-24T00:00:00+00:00");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00+00:00");
        assert_eq!(parse_rfc3339(&rfc3339(T0 + 3_723)), Some(T0 + 3_723));
    }

    #[test]
    fn initial_labels_follow_rule_r12() {
        assert_eq!(
            initial_labels("demo", "ci", Some("42"), 1),
            labels(&[
                ("ciburst", "true"),
                ("ciburst-name", "demo"),
                ("ciburst-pool", "ci"),
                ("ciburst-job", "42"),
                ("ciburst-state", "starting"),
                ("ciburst-free", "1"),
            ])
        );
        for job in [None, Some(""), Some("not a label")] {
            assert!(!initial_labels("demo", "ci", job, 0).contains_key(LABEL_JOB));
        }
        for (key, value) in initial_labels("demo", "ci-2", Some("7"), 3) {
            assert!(valid_label_key(&key) && valid_label_value(&value));
        }
    }

    #[test]
    fn reports_rewrite_only_the_state_labels() {
        let before = labels(&[
            ("ciburst", "true"),
            ("ciburst-name", "demo"),
            ("ciburst-pool", "ci"),
            ("ciburst-job", "42"),
            ("owner", "ops"),
            ("ciburst-state", "starting"),
            ("ciburst-free", "2"),
        ]);
        let busy = VmReport {
            state: VmState::Busy,
            free: 1,
            slots: 2,
            spill: 0,
        };
        let after = reported(&before, &busy, T0);
        let mut expected = before.clone();
        expected.insert("ciburst-state".into(), "busy".into());
        expected.insert("ciburst-free".into(), "1".into());
        expected.insert("ciburst-seen".into(), T0.to_string());
        assert_eq!(after, expected);

        let draining = VmReport {
            state: VmState::Draining,
            free: 2,
            slots: 2,
            spill: 0,
        };
        let first = reported(&after, &draining, T0 + 10);
        assert_eq!(first[LABEL_DRAIN], (T0 + 10).to_string());
        let again = reported(&first, &draining, T0 + 70);
        assert_eq!(again[LABEL_DRAIN], (T0 + 10).to_string());
        assert_eq!(again[LABEL_SEEN], (T0 + 70).to_string());
        let idle = VmReport {
            state: VmState::Idle,
            free: 2,
            slots: 2,
            spill: 0,
        };
        let back = reported(&again, &idle, T0 + 80);
        assert!(!back.contains_key(LABEL_DRAIN));
        let redrain = reported(&back, &draining, T0 + 90);
        assert_eq!(redrain[LABEL_DRAIN], (T0 + 90).to_string());
        let mut stale = back.clone();
        stale.insert(LABEL_DRAIN.into(), "5".into());
        assert_eq!(
            reported(&stale, &draining, T0 + 95)[LABEL_DRAIN],
            (T0 + 95).to_string(),
            "a drain label without the draining state starts over"
        );
        assert_eq!(
            redrain.keys().filter(|k| k.starts_with("ciburst-")).count(),
            7
        );
    }

    #[test]
    fn claims_floor_at_zero() {
        let start = labels(&[("ciburst", "true"), ("ciburst-free", "2")]);
        let one = claimed(&start);
        assert_eq!(one, labels(&[("ciburst", "true"), ("ciburst-free", "1")]));
        let zero = claimed(&claimed(&one));
        assert_eq!(zero["ciburst-free"], "0");
        assert_eq!(claimed(&labels(&[]))["ciburst-free"], "0");
        assert_eq!(
            claimed(&labels(&[("ciburst-free", "x")]))["ciburst-free"],
            "0"
        );
    }

    #[test]
    fn capacity_reads_labels_and_created() {
        let server = json!({
            "id": 42,
            "name": "demo-ci-0000abcd",
            "created": rfc3339(T0),
            "labels": {
                "ciburst": "true",
                "ciburst-name": "demo",
                "ciburst-state": "busy",
                "ciburst-free": "3",
                "ciburst-seen": "17"
            },
        });
        assert_eq!(
            capacity(&server),
            Some(VmCapacity {
                run_id: "hetzner:demo-ci-0000abcd".into(),
                state: VmState::Busy,
                free: 3,
                spill: 0,
                seen: Some(17),
                created_at: T0,
            })
        );
        let fresh = json!({ "name": "demo-ci-1", "created": rfc3339(T0), "labels": {} });
        let fresh = capacity(&fresh).unwrap();
        assert_eq!(
            (fresh.state, fresh.free, fresh.seen),
            (VmState::Starting, 0, None)
        );
        assert_eq!(capacity(&json!({ "name": "x", "labels": {} })), None);
        assert_eq!(capacity(&json!({ "created": rfc3339(T0) })), None);
        assert!(is_deployment(&server, "demo"));
        assert!(!is_deployment(&server, "other"));
        assert!(!is_deployment(
            &json!({ "labels": { "ciburst-name": "demo" } }),
            "demo"
        ));
        assert!(!is_deployment(
            &json!({ "labels": { "ciburst": "false", "ciburst-name": "demo" } }),
            "demo"
        ));
    }

    #[test]
    fn maps_server_statuses() {
        for (status, expected, live) in [
            ("initializing", RunState::Queued, true),
            ("starting", RunState::Queued, true),
            ("running", RunState::Running, true),
            ("migrating", RunState::Running, true),
            ("rebuilding", RunState::Running, true),
            ("stopping", RunState::Done, false),
            ("off", RunState::Done, false),
            ("deleting", RunState::Done, false),
            ("unknown", RunState::Failed, false),
        ] {
            assert_eq!(run_state(status), expected, "{status}");
            assert_eq!(run_state(status).is_live(), live, "{status}");
        }
    }

    #[test]
    fn run_ids_name_servers() {
        assert_eq!(run_id("demo-ci-0000abcd"), "hetzner:demo-ci-0000abcd");
        assert_eq!(
            server_name_of("hetzner:demo-ci-0000abcd"),
            Some("demo-ci-0000abcd")
        );
        for foreign in [
            "demo-ci-0000abcd",
            "hetzner",
            "hetzner:",
            "hetznerx:demo",
            "scaleway:demo",
            "hetzner:demo ci",
            "hetzner:demo&x=1",
            "hetzner:-demo",
        ] {
            assert_eq!(server_name_of(foreign), None, "{foreign}");
        }
    }

    #[test]
    fn server_names_are_prefixed_distinct_and_bounded() {
        let a = server_name("demo", "ci");
        let b = server_name("demo", "ci");
        let suffix = a.strip_prefix("demo-ci-").expect("prefix");
        assert_eq!(suffix.len(), 8);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert!(valid_server_name(&a));
        let tag = "t".repeat(MAX_SERVER_NAME - "demo".len() - 1 - SUFFIX_LEN);
        assert!(server_name_fits("demo", &tag));
        assert_eq!(server_name("demo", &tag).len(), MAX_SERVER_NAME);
        assert!(valid_server_name(&server_name("demo", &tag)));
        assert!(!server_name_fits("demo", &format!("{tag}t")));
    }

    #[test]
    fn label_rules_match_the_api() {
        for good in ["", "a", "ci", "a-b_c.d", "42", &"x".repeat(63)] {
            assert!(valid_label_value(good), "{good}");
        }
        for bad in ["-a", "a-", "a b", "a/b", "ä", &"x".repeat(64)] {
            assert!(!valid_label_value(bad), "{bad}");
        }
        for good in ["ciburst", "ciburst-free", "example.com/my", "a"] {
            assert!(valid_label_key(good), "{good}");
        }
        for bad in ["", "/a", "a/", "-a", "a b", "example.com/", &"x".repeat(64)] {
            assert!(!valid_label_key(bad), "{bad}");
        }
    }

    #[test]
    fn selectors_name_the_deployment() {
        assert_eq!(
            deployment_selector("demo"),
            "ciburst==true,ciburst-name==demo"
        );
        assert_eq!(
            pool_selector("demo", "ci"),
            "ciburst==true,ciburst-name==demo,ciburst-pool==ci"
        );
        assert_eq!(firewall_selector("demo"), "ciburst-name==demo");
    }
}
