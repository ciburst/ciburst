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

/// Unix seconds of an RFC 3339 timestamp such as `2026-10-24T12:00:00+00:00`.
pub fn parse_rfc3339(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = &text[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            let hours: i64 = number_in(rest, 1..3)?;
            let minutes: i64 = number_in(rest, 4..6)?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(seconds).ok()
}

fn number_in(text: &str, range: std::ops::Range<usize>) -> Option<i64> {
    let part = text.get(range)?;
    part.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| part.parse().ok())?
}

/// `YYYY-MM-DDTHH:MM:SS+00:00`, the form the API returns.
pub fn rfc3339(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+00:00",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

// Howard Hinnant's days_from_civil and civil_from_days.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
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
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00+00:00");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00+00:00");
        assert_eq!(parse_rfc3339("2026-10-24T00:00:00Z"), Some(T0));
        assert_eq!(parse_rfc3339(&rfc3339(T0 + 3_723)), Some(T0 + 3_723));
        assert_eq!(parse_rfc3339("2026-10-24T02:00:00.123+02:00"), Some(T0));
        assert_eq!(parse_rfc3339("2026-10-23T22:30:00-01:30"), Some(T0));
        for unix in [0, 951_782_400, 4_102_444_800, T0 + 86_399] {
            assert_eq!(parse_rfc3339(&rfc3339(unix)), Some(unix));
        }
        for bad in [
            "",
            "2026-10-24",
            "2026-13-01T00:00:00Z",
            "2026-10-24T00:00:00",
            "2026-10-24T00:00:00.Z",
            "2026-10-24T00:00:00+0200",
            "2026-10-24T00:00:00+0a:00",
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
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
