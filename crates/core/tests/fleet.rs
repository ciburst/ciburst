//! spec 8.1 and 8.6 in the setup: the fleet state `apply` writes, and what `status`,
//! `check` and `destroy` make of the members.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ciburst_core::check::LintJob;
use ciburst_core::config::{Config, parse_config};
use ciburst_core::fake::{FakeHost, FakeProvider};
use ciburst_core::fleet::{FleetStore, Member, MemberLane, MemoryFleetStore};
use ciburst_core::gitlab::Gitlab;
use ciburst_core::log::Log;
use ciburst_core::plan::{MemoryOutputs, OutputsStore, format_plan};
use ciburst_core::provision::{Provisioner, keys};
use ciburst_core::store::MemoryStore;
use ciburst_core::testing::gitlab_fake::{FAKE_TOKEN, FakeGitlab};
use ciburst_core::types::{Action, Provider, Step};
use serde_json::json;

const NOW: u64 = 1_800_000_000;

fn config(project_ids: &[u64]) -> Arc<Config> {
    let raw = json!({
        "name": "demo",
        "gitlab": { "url": "https://gitlab.example", "projectIds": project_ids },
        "host": { "type": "fake" },
        "provider": { "type": "fake" },
        "vm": { "tag": "ci", "image": "img/node" },
        "lanes": [ { "tag": "build", "image": "img/kaniko", "jobs": ["image"] } ],
        "cache": { "type": "s3" }
    });
    Arc::new(parse_config(&raw.to_string()).unwrap())
}

struct Setup {
    config: Arc<Config>,
    fake: Arc<FakeGitlab>,
    fleet: Arc<MemoryFleetStore>,
    outputs: Arc<MemoryOutputs>,
    lines: Arc<Mutex<Vec<String>>>,
    provisioner: Provisioner,
}

/// The fleet reaches the provisioner through the provider's hook.
fn setup(project_ids: &[u64]) -> Setup {
    let config = config(project_ids);
    let fake = Arc::new(FakeGitlab::new());
    let fleet = Arc::new(MemoryFleetStore::new());
    let provider: Arc<dyn Provider> = Arc::new(
        FakeProvider::new(config.clone(), Arc::new(MemoryStore::new())).with_fleet(fleet.clone()),
    );
    let host = Arc::new(FakeHost::new(config.clone(), Arc::new(MemoryStore::new())));
    let outputs = Arc::new(MemoryOutputs::new());
    let (log, lines) = Log::memory();
    let gitlab = Arc::new(Gitlab::new(&config.gitlab.url, FAKE_TOKEN, fake.clone()));
    let provisioner = Provisioner::new(
        config.clone(),
        provider,
        host,
        Some(gitlab),
        outputs.clone(),
        log,
    )
    .with_clock(Arc::new(|| NOW));
    Setup {
        config,
        fake,
        fleet,
        outputs,
        lines,
        provisioner,
    }
}

fn fleet_lines(steps: &[Step]) -> Vec<String> {
    let fleet: Vec<Step> = steps
        .iter()
        .filter(|s| s.id.starts_with("fleet:") || s.id.starts_with("member:"))
        .cloned()
        .collect();
    format_plan(&fleet).lines().map(String::from).collect()
}

fn joined(project_id: u64) -> Member {
    Member {
        project_id,
        hook_secret: format!("secret-{project_id}"),
        runners: BTreeMap::from([("ci".to_string(), format!("glrt-{project_id}"))]),
        joined_at: NOW - 60,
        ..Default::default()
    }
}

#[tokio::test]
async fn apply_writes_the_config_and_the_host_member() {
    let s = setup(&[7]);
    let plan = s.provisioner.plan(&[]).await.unwrap();
    assert_eq!(
        fleet_lines(&plan),
        ["update  fleet config", "create  member 7"]
    );
    let applied = s.provisioner.apply(&[]).await.unwrap();
    assert_eq!(
        fleet_lines(&applied),
        ["update  fleet config", "create  member 7"]
    );
    let stored = s.fleet.config().await.unwrap().unwrap();
    assert_eq!(parse_config(&stored).unwrap(), *s.config);
    let outputs = s.outputs.read().await.unwrap();
    let members = s.fleet.members().await.unwrap();
    assert_eq!(
        members,
        [Member {
            project_id: 7,
            hook_secret: outputs[keys::HOOK_TOKEN].clone(),
            runners: BTreeMap::from([(
                "ci".to_string(),
                outputs[&keys::runner_token("ci")].clone()
            )]),
            joined_at: NOW,
            ..Default::default()
        }]
    );
    let lines = s.lines.lock().unwrap().clone();
    assert!(lines.iter().any(|l| l.starts_with("info fleet_config")));
    assert!(!lines.iter().any(|l| l.contains("glrt-")));

    let again = s.provisioner.plan(&[]).await.unwrap();
    assert_eq!(
        fleet_lines(&again),
        ["noop  fleet config", "noop  member 7"]
    );
    assert!(again.iter().all(|s| s.action == Action::Noop), "{again:?}");
}

#[tokio::test]
async fn further_projects_become_members_with_the_host_runners() {
    let s = setup(&[7, 11, 12]);
    s.fleet.put_member(&joined(12)).await.unwrap();
    let applied = s.provisioner.apply(&[]).await.unwrap();
    assert_eq!(
        fleet_lines(&applied),
        [
            "update  fleet config",
            "create  member 7",
            "create  member 11  (host runners until it joins)",
            "noop    member 12",
        ]
    );
    let members = s.fleet.members().await.unwrap();
    let host = &members[0];
    assert_eq!(members[1].runners, host.runners);
    assert_eq!(members[1].hook_secret, host.hook_secret);
    assert_eq!(members[2], joined(12), "a joined project keeps its record");

    s.provisioner
        .apply(&[ciburst_core::types::Rotation::Hook])
        .await
        .unwrap();
    let rotated = s.fleet.members().await.unwrap();
    assert_ne!(rotated[0].hook_secret, host.hook_secret);
    assert_eq!(rotated[1].hook_secret, rotated[0].hook_secret);
    assert_eq!(rotated[2], joined(12));
}

#[tokio::test]
async fn destroy_refuses_while_members_joined() {
    let s = setup(&[7, 11]);
    s.provisioner.apply(&[]).await.unwrap();
    s.fleet.put_member(&joined(21)).await.unwrap();
    s.fleet.put_member(&joined(20)).await.unwrap();
    let error = s.provisioner.destroy().await.unwrap_err().to_string();
    assert_eq!(
        error,
        "fleet demo has members 20, 21: run ciburst-leave there first"
    );
    assert_eq!(s.fake.runner_count(), 2, "nothing was removed");

    s.fleet.delete_member(20).await.unwrap();
    s.fleet.delete_member(21).await.unwrap();
    s.provisioner.destroy().await.unwrap();
    assert_eq!(s.fake.runner_count(), 0);
}

#[tokio::test]
async fn status_lists_the_members_and_their_deliveries() {
    let s = setup(&[7]);
    s.provisioner.apply(&[]).await.unwrap();
    s.fleet.put_member(&joined(21)).await.unwrap();
    let mut eight = joined(8);
    eight.lanes = vec![MemberLane {
        tag: "build".into(),
        image: "img/kaniko".into(),
        jobs: vec!["image".into()],
        max_runs: 3,
        definition_id: "def".into(),
        runner_token: "glrt-8-build".into(),
    }];
    s.fleet.put_member(&eight).await.unwrap();
    let url = "https://demo-dispatch.fake.invalid/";
    let status = s.provisioner.status().await.unwrap();
    let hook = status
        .members
        .iter()
        .map(|m| m.project_id)
        .collect::<Vec<_>>();
    assert_eq!(hook, [7, 8, 21], "the host first");
    // spec 13.1: the VM runner, then the lanes per member, the host's first.
    let pools: Vec<(Option<u64>, &str, u32)> = status
        .pools
        .iter()
        .map(|p| (p.project, p.tag.as_str(), p.max_runs))
        .collect();
    assert_eq!(
        pools,
        [
            (None, "ci", 4),
            (Some(7), "build", 10),
            (Some(8), "build", 3)
        ]
    );

    let member_hook = Gitlab::new("https://gitlab.example", FAKE_TOKEN, s.fake.clone())
        .create_hook(8, url, "secret-8")
        .await
        .unwrap();
    s.fake.add_hook_event(
        member_hook.id,
        json!({ "id": 1, "created_at": "2026-10-26T10:00:00Z",
                "request_data": { "object_kind": "build", "build_name": "unit" },
                "response_status": "200", "response_body": "{\"result\":\"started\",\"ms\":5}" }),
    );
    s.fake.hide_project(21);
    let deliveries = s.provisioner.hook_deliveries(Some(url)).await.unwrap();
    let projects: Vec<u64> = deliveries.iter().map(|d| d.project_id).collect();
    assert_eq!(projects, [7, 8, 21]);
    assert_eq!(deliveries[1].deliveries.len(), 1);
    assert_eq!(deliveries[1].deliveries[0].result, "started");
    assert!(
        deliveries[2]
            .unreadable
            .as_deref()
            .is_some_and(|e| e.contains("404")),
        "{:?}",
        deliveries[2]
    );
}

/// spec 13.2: each member is checked against its own lanes, never the host's.
#[tokio::test]
async fn check_lints_every_member() {
    let s = setup(&[7]);
    s.provisioner.apply(&[]).await.unwrap();
    let mut eight = joined(8);
    eight.lanes = vec![MemberLane {
        tag: "node".into(),
        image: "node:24".into(),
        jobs: vec!["lint".into()],
        max_runs: 2,
        definition_id: "def".into(),
        runner_token: "glrt-8-node".into(),
    }];
    s.fleet.put_member(&eight).await.unwrap();
    s.fleet.put_member(&joined(9)).await.unwrap();
    let job = |name: &str, stage: &str, tags: &[&str]| LintJob {
        name: name.into(),
        stage: Some(stage.into()),
        tag_list: tags.iter().map(|t| t.to_string()).collect(),
    };
    s.fake.set_jobs(7, vec![job("unit", "test", &["ci"])]);
    s.fake.set_jobs(
        8,
        vec![
            job("lint", "test", &["node"]),
            job("image", "build", &["build"]),
        ],
    );
    s.fake.hide_project(9);
    let report = s.provisioner.check(None).await.unwrap();
    let summary: Vec<(u64, usize, bool)> = report
        .iter()
        .map(|r| (r.project_id, r.problems.len(), r.unreadable.is_some()))
        .collect();
    assert_eq!(summary, [(7, 0, false), (8, 1, false), (9, 0, true)]);
    assert_eq!(
        report[1].problems[0].job, "image",
        "the host's lane is not 8's"
    );

    s.fake.hide_project(7);
    assert!(
        s.provisioner.check(None).await.is_err(),
        "the host's own project must be readable"
    );
}
