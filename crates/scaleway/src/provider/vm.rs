//! VM runs on Scaleway Instances, spec section 6.1: the server tags that carry their state, the
//! sweep rules, and the RFC 3339 timestamps of the Instance API.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ciburst_core::time::{DateTime, parse_rfc3339};
use ciburst_core::types::{RunState, VmCapacity, VmReport, VmState};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const TAG_CIBURST: &str = "ciburst";
pub const TAG_POOL: &str = "pool";
pub const TAG_JOB: &str = "job";
pub const TAG_STATE: &str = "ciburst-state";
pub const TAG_FREE: &str = "ciburst-free";
pub const TAG_SEEN: &str = "ciburst-seen";
pub const TAG_DRAIN: &str = "ciburst-drain";
/// Slots of the last report that poll with the spill runner (M6.2 rule R6); written only when
/// there are any, so a VM that never spills keeps the tags of M6.1.
pub const TAG_SPILL: &str = "ciburst-spill";

/// The tags `report` rewrites, each at most once; every other tag of a server is kept.
const STATE_TAGS: [&str; 5] = [TAG_STATE, TAG_FREE, TAG_SPILL, TAG_SEEN, TAG_DRAIN];

/// The value of the first `key:value` tag.
pub fn tag_value<'a>(tags: &'a [String], key: &str) -> Option<&'a str> {
    tags.iter()
        .find_map(|tag| tag.strip_prefix(key)?.strip_prefix(':'))
}

fn has_key(tag: &str, key: &str) -> bool {
    tag.strip_prefix(key)
        .is_some_and(|rest| rest.starts_with(':'))
}

/// The tags of a server as strings; missing or malformed tags read as none.
pub fn tags_of(server: &Value) -> Vec<String> {
    server["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tag| tag.as_str().map(String::from))
        .collect()
}

/// Whether a server belongs to the deployment `name`.
pub fn is_deployment(server: &Value, name: &str) -> bool {
    let tags = tags_of(server);
    tags.iter().any(|t| t == TAG_CIBURST) && tags.iter().any(|t| t == name)
}

/// Rule 4: `ciburst`, `<name>`, `pool:<tag>`, `job:<job>`, `ciburst-state:starting`,
/// `ciburst-free:<free>` (slots minus those taken at start, M4 rule R6).
pub fn initial_tags(name: &str, pool: &str, job: Option<&str>, free: u32) -> Vec<String> {
    let mut tags = vec![
        TAG_CIBURST.to_string(),
        name.to_string(),
        format!("{TAG_POOL}:{pool}"),
    ];
    if let Some(job) = job.filter(|j| !j.is_empty()) {
        tags.push(format!("{TAG_JOB}:{job}"));
    }
    tags.push(format!("{TAG_STATE}:{}", VmState::Starting.as_str()));
    tags.push(format!("{TAG_FREE}:{free}"));
    tags
}

/// `tags` after a report at `now`. The drain tag keeps the time draining started while the VM
/// stays in `draining`, and disappears in any other state.
pub fn reported(tags: &[String], report: &VmReport, now: u64) -> Vec<String> {
    let drain_since = (report.state == VmState::Draining).then(|| {
        let was_draining = tag_value(tags, TAG_STATE) == Some(VmState::Draining.as_str());
        tag_value(tags, TAG_DRAIN)
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|_| was_draining)
            .unwrap_or(now)
    });
    let mut out: Vec<String> = tags
        .iter()
        .filter(|tag| !STATE_TAGS.iter().any(|key| has_key(tag, key)))
        .cloned()
        .collect();
    out.push(format!("{TAG_STATE}:{}", report.state.as_str()));
    out.push(format!("{TAG_FREE}:{}", report.free));
    if report.spill > 0 {
        out.push(format!("{TAG_SPILL}:{}", report.spill));
    }
    out.push(format!("{TAG_SEEN}:{now}"));
    if let Some(since) = drain_since {
        out.push(format!("{TAG_DRAIN}:{since}"));
    }
    out
}

/// `tags` with one free slot fewer, never below zero.
pub fn claimed(tags: &[String]) -> Vec<String> {
    let free = tag_value(tags, TAG_FREE)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0)
        .saturating_sub(1);
    let replacement = format!("{TAG_FREE}:{free}");
    let mut out: Vec<String> = Vec::with_capacity(tags.len() + 1);
    let mut replaced = false;
    for tag in tags {
        if has_key(tag, TAG_FREE) {
            if !replaced {
                out.push(replacement.clone());
                replaced = true;
            }
        } else {
            out.push(tag.clone());
        }
    }
    if !replaced {
        out.push(replacement);
    }
    out
}

/// A server as the scheduler sees it; `None` without an id or a readable `creation_date`.
pub fn capacity(server: &Value) -> Option<VmCapacity> {
    let tags = tags_of(server);
    Some(VmCapacity {
        run_id: server["id"].as_str()?.to_string(),
        state: tag_value(&tags, TAG_STATE)
            .and_then(VmState::parse)
            .unwrap_or(VmState::Starting),
        free: tag_value(&tags, TAG_FREE)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        spill: tag_value(&tags, TAG_SPILL)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        seen: tag_value(&tags, TAG_SEEN).and_then(|v| v.parse().ok()),
        created_at: parse_rfc3339(server["creation_date"].as_str()?)?,
    })
}

/// Unix seconds at which the server started draining, from its drain tag.
pub fn drain_since(server: &Value) -> Option<u64> {
    tag_value(&tags_of(server), TAG_DRAIN).and_then(|v| v.parse().ok())
}

pub use ciburst_core::vm::sweep_reason;

/// Server states of `instance/v1` (`ServerState`): `starting` and `running` are live.
pub fn run_state(state: &str) -> RunState {
    match state {
        "starting" => RunState::Queued,
        "running" => RunState::Running,
        "stopping" | "stopped" | "stopped in place" => RunState::Done,
        _ => RunState::Failed,
    }
}

/// `<name>-<tag>-<8 hex>`; the suffix only has to differ between servers started together.
pub fn server_name(name: &str, tag: &str, job: Option<&str>) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let digest = Sha256::digest(format!(
        "{name}/{tag}/{}/{nanos}/{count}",
        job.unwrap_or_default()
    ));
    format!("{name}-{tag}-{}", hex::encode(&digest[..4]))
}

/// `YYYY-MM-DDTHH:MM:SS.000000+00:00`, the form the Instance API returns.
pub fn rfc3339(unix: u64) -> String {
    let at = DateTime::from_unix(unix);
    format!("{}T{}.000000+00:00", at.date(), at.time())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciburst_core::vm::{SWEEP_DRAINING_S, SWEEP_MAX_AGE_S, SWEEP_SILENT_S, SWEEP_STARTING_S};
    use serde_json::json;

    const T0: u64 = 1_792_800_000;

    fn tags(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn vm(state: VmState, seen: Option<u64>, created_at: u64) -> VmCapacity {
        VmCapacity {
            run_id: "s".into(),
            state,
            free: 1,
            spill: 0,
            seen,
            created_at,
        }
    }

    #[test]
    fn timestamps_round_trip() {
        assert_eq!(rfc3339(T0), "2026-10-24T00:00:00.000000+00:00");
        assert_eq!(parse_rfc3339(&rfc3339(T0 + 3_723)), Some(T0 + 3_723));
    }

    #[test]
    fn initial_tags_follow_rule_four() {
        assert_eq!(
            initial_tags("demo", "ci", Some("42"), 2),
            tags(&[
                "ciburst",
                "demo",
                "pool:ci",
                "job:42",
                "ciburst-state:starting",
                "ciburst-free:2"
            ])
        );
        assert!(
            !initial_tags("demo", "ci", None, 1)
                .iter()
                .any(|t| t.starts_with("job:"))
        );
    }

    #[test]
    fn reports_rewrite_only_the_state_tags() {
        let before = tags(&[
            "ciburst",
            "demo",
            "pool:ci",
            "job:42",
            "owner:x",
            "ciburst-state:starting",
            "ciburst-free:2",
        ]);
        let busy = VmReport {
            state: VmState::Busy,
            free: 1,
            slots: 2,
            spill: 0,
        };
        let after = reported(&before, &busy, T0);
        assert_eq!(
            after,
            tags(&[
                "ciburst",
                "demo",
                "pool:ci",
                "job:42",
                "owner:x",
                "ciburst-state:busy",
                "ciburst-free:1",
                &format!("ciburst-seen:{T0}"),
            ])
        );
        let draining = VmReport {
            state: VmState::Draining,
            free: 2,
            slots: 2,
            spill: 0,
        };
        let first = reported(&after, &draining, T0 + 10);
        assert_eq!(tag_value(&first, TAG_DRAIN), Some(&*format!("{}", T0 + 10)));
        let again = reported(&first, &draining, T0 + 70);
        assert_eq!(tag_value(&again, TAG_DRAIN), Some(&*format!("{}", T0 + 10)));
        assert_eq!(tag_value(&again, TAG_SEEN), Some(&*format!("{}", T0 + 70)));
        let idle = VmReport {
            state: VmState::Idle,
            free: 2,
            slots: 2,
            spill: 0,
        };
        let back = reported(&again, &idle, T0 + 80);
        assert_eq!(tag_value(&back, TAG_DRAIN), None);
        let redrain = reported(&back, &draining, T0 + 90);
        assert_eq!(
            tag_value(&redrain, TAG_DRAIN),
            Some(&*format!("{}", T0 + 90))
        );
        assert_eq!(
            redrain.iter().filter(|t| t.starts_with("ciburst-")).count(),
            4
        );
    }

    /// M6.2 rule R6: the spill tag sits next to the free tag while slots spill, replaces itself
    /// and disappears at zero, so a report never adds a tag twice.
    #[test]
    fn reports_write_the_spill_tag_while_slots_spill() {
        let before = initial_tags("demo", "vm", Some("42"), 1);
        let spilling = VmReport {
            state: VmState::Idle,
            free: 2,
            slots: 2,
            spill: 2,
        };
        let after = reported(&before, &spilling, T0);
        assert_eq!(
            after,
            tags(&[
                "ciburst",
                "demo",
                "pool:vm",
                "job:42",
                "ciburst-state:idle",
                "ciburst-free:2",
                "ciburst-spill:2",
                &format!("ciburst-seen:{T0}"),
            ])
        );
        let one = VmReport {
            state: VmState::Busy,
            free: 1,
            spill: 1,
            ..spilling
        };
        let again = reported(&reported(&after, &one, T0 + 15), &one, T0 + 30);
        assert_eq!(again.len(), after.len());
        assert_eq!(tag_value(&again, TAG_SPILL), Some("1"));
        let off = VmReport { spill: 0, ..one };
        let off = reported(&again, &off, T0 + 45);
        assert!(
            !off.iter().any(|t| t.starts_with("ciburst-spill")),
            "{off:?}"
        );
        assert_eq!(off.len(), after.len() - 1);
        let mut doubled = after.clone();
        doubled.push("ciburst-spill:7".into());
        assert_eq!(reported(&doubled, &spilling, T0), after);
    }

    #[test]
    fn claims_floor_at_zero_and_keep_order() {
        let start = tags(&["ciburst", "ciburst-free:2", "pool:ci"]);
        let one = claimed(&start);
        assert_eq!(one, tags(&["ciburst", "ciburst-free:1", "pool:ci"]));
        let zero = claimed(&claimed(&one));
        assert_eq!(zero, tags(&["ciburst", "ciburst-free:0", "pool:ci"]));
        assert_eq!(
            claimed(&tags(&["ciburst"])),
            tags(&["ciburst", "ciburst-free:0"])
        );
        assert_eq!(
            claimed(&tags(&["ciburst-free:x", "ciburst-free:3"])),
            tags(&["ciburst-free:0"])
        );
    }

    #[test]
    fn capacity_reads_tags_and_creation_date() {
        let server = json!({
            "id": "s-1",
            "creation_date": rfc3339(T0),
            "tags": ["ciburst", "demo", "ciburst-state:busy", "ciburst-free:3", "ciburst-seen:17"],
        });
        assert_eq!(
            capacity(&server),
            Some(VmCapacity {
                run_id: "s-1".into(),
                state: VmState::Busy,
                free: 3,
                spill: 0,
                seen: Some(17),
                created_at: T0,
            })
        );
        let fresh = json!({ "id": "s-2", "creation_date": rfc3339(T0), "tags": ["ciburst"] });
        let fresh = capacity(&fresh).unwrap();
        assert_eq!(
            (fresh.state, fresh.free, fresh.spill, fresh.seen),
            (VmState::Starting, 0, 0, None)
        );
        for (tag, spill) in [("ciburst-spill:2", 2), ("ciburst-spill:x", 0)] {
            let spilling = json!({
                "id": "s-4",
                "creation_date": rfc3339(T0),
                "tags": ["ciburst", "ciburst-free:2", tag],
            });
            assert_eq!(capacity(&spilling).unwrap().spill, spill, "{tag}");
        }
        assert_eq!(capacity(&json!({ "id": "s-3", "tags": [] })), None);
        assert!(is_deployment(&server, "demo"));
        assert!(!is_deployment(&server, "other"));
        assert!(!is_deployment(&json!({ "tags": ["demo"] }), "demo"));
    }

    #[test]
    fn sweep_rules_each_with_their_boundary() {
        let created = T0;
        // starting: more than SWEEP_STARTING_S after creation, never seen
        let starting = vm(VmState::Starting, None, created);
        assert_eq!(
            sweep_reason(&starting, None, created + SWEEP_STARTING_S),
            None
        );
        assert_eq!(
            sweep_reason(&starting, None, created + SWEEP_STARTING_S + 1),
            Some("starting")
        );
        // silent: seen more than SWEEP_SILENT_S ago
        let seen = created + 100;
        let idle = vm(VmState::Idle, Some(seen), created);
        assert_eq!(sweep_reason(&idle, None, seen + SWEEP_SILENT_S), None);
        assert_eq!(
            sweep_reason(&idle, None, seen + SWEEP_SILENT_S + 1),
            Some("silent")
        );
        // never seen and older than SWEEP_SILENT_S, in a state other than starting
        let unseen = vm(VmState::Idle, None, created);
        assert_eq!(sweep_reason(&unseen, None, created + SWEEP_SILENT_S), None);
        assert_eq!(
            sweep_reason(&unseen, None, created + SWEEP_SILENT_S + 1),
            Some("silent")
        );
        // draining: drain older than SWEEP_DRAINING_S, only once the VM stopped reporting
        let drain = created + 200;
        let draining = vm(VmState::Draining, None, created);
        assert_eq!(
            sweep_reason(&draining, Some(drain), drain + SWEEP_DRAINING_S),
            None
        );
        assert_eq!(
            sweep_reason(&draining, Some(drain), drain + SWEEP_DRAINING_S + 1),
            Some("draining")
        );
        let reporting = vm(VmState::Draining, Some(drain + SWEEP_DRAINING_S), created);
        assert_eq!(
            sweep_reason(&reporting, Some(drain), drain + SWEEP_DRAINING_S + 1),
            None,
            "a VM that reports is never swept"
        );
        // maximum age wins over a fresh report
        let old = vm(VmState::Busy, Some(created + SWEEP_MAX_AGE_S), created);
        assert_eq!(sweep_reason(&old, None, created + SWEEP_MAX_AGE_S), None);
        assert_eq!(
            sweep_reason(&old, None, created + SWEEP_MAX_AGE_S + 1),
            Some("max_age")
        );
    }

    #[test]
    fn maps_server_states() {
        for (state, expected, live) in [
            ("starting", RunState::Queued, true),
            ("running", RunState::Running, true),
            ("stopping", RunState::Done, false),
            ("stopped", RunState::Done, false),
            ("stopped in place", RunState::Done, false),
            ("locked", RunState::Failed, false),
        ] {
            assert_eq!(run_state(state), expected, "{state}");
            assert_eq!(run_state(state).is_live(), live, "{state}");
        }
    }

    #[test]
    fn server_names_are_prefixed_and_distinct() {
        let a = server_name("demo", "ci", Some("1"));
        let b = server_name("demo", "ci", Some("1"));
        assert!(a.starts_with("demo-ci-"), "{a}");
        let suffix = a.strip_prefix("demo-ci-").unwrap();
        assert_eq!(suffix.len(), 8);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
