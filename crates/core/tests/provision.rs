use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ciburst_core::check::LintJob;
use ciburst_core::config::{Config, parse_config};
use ciburst_core::dispatch::Dispatcher;
use ciburst_core::fake::{FakeHost, FakeProvider};
use ciburst_core::gitlab::Gitlab;
use ciburst_core::ledger::{MemoryPipelineRecords, PipelineRecords};
use ciburst_core::log::Log;
use ciburst_core::plan::format_plan;
use ciburst_core::plan::{GitlabOutputs, MemoryOutputs, OUTPUTS_SCOPE, OutputsStore};
use ciburst_core::provision::{Provisioner, check, keys, secrets_from_outputs};
use ciburst_core::store::MemoryStore;
use ciburst_core::testing::gitlab_fake::{FAKE_TOKEN, FakeBuild, FakeGitlab};
use ciburst_core::types::{Action, Host, Outputs, Provider, Rotation, Step};
use serde_json::json;

fn config(project_ids: &[u64]) -> Arc<Config> {
    let raw = json!({
        "name": "demo",
        "gitlab": { "url": "https://gitlab.example", "projectIds": project_ids },
        "host": { "type": "fake" },
        "provider": { "type": "fake" },
        "defaultPool": "ci",
        "pools": [
            { "tag": "ci", "image": "img/node", "stages": ["test"] },
            { "tag": "build", "image": "img/kaniko", "stages": ["build"] }
        ],
        "cache": { "type": "s3" }
    });
    Arc::new(parse_config(&raw.to_string()).unwrap())
}

struct Setup {
    config: Arc<Config>,
    fake: Arc<FakeGitlab>,
    provider: Arc<FakeProvider>,
    host: Arc<FakeHost>,
    outputs: Arc<MemoryOutputs>,
    lines: Arc<Mutex<Vec<String>>>,
    provisioner: Provisioner,
}

fn setup(config: Arc<Config>, with_gitlab: bool) -> Setup {
    let fake = Arc::new(FakeGitlab::new());
    let provider = Arc::new(FakeProvider::new(
        config.clone(),
        Arc::new(MemoryStore::new()),
    ));
    let host = Arc::new(FakeHost::new(config.clone(), Arc::new(MemoryStore::new())));
    let outputs = Arc::new(MemoryOutputs::new());
    let (log, lines) = Log::memory();
    let gitlab =
        with_gitlab.then(|| Arc::new(Gitlab::new(&config.gitlab.url, FAKE_TOKEN, fake.clone())));
    let provisioner = Provisioner::new(
        config.clone(),
        provider.clone(),
        host.clone(),
        gitlab,
        outputs.clone(),
        log,
    );
    Setup {
        config,
        fake,
        provider,
        host,
        outputs,
        lines,
        provisioner,
    }
}

fn actions(steps: &[Step]) -> BTreeMap<String, Action> {
    steps.iter().map(|s| (s.id.clone(), s.action)).collect()
}

fn all(steps: &[Step], action: Action) -> bool {
    steps.iter().all(|s| s.action == action)
}

#[tokio::test]
async fn plans_everything_applies_then_plans_nothing() {
    let s = setup(config(&[7]), true);
    let first = s.provisioner.plan(&[]).await.unwrap();
    assert!(all(&first, Action::Create), "{first:?}");
    let ids: Vec<&str> = first.iter().map(|s| s.id.as_str()).collect();
    for id in [
        "project",
        "key:dispatch",
        "key:cache",
        "bucket",
        "definition:ci",
        "definition:build",
        "runner:ci",
        "runner:build",
        "dispatcher",
        "hook:7",
    ] {
        assert!(ids.contains(&id), "{id} in {ids:?}");
    }
    assert_eq!(ids.len(), 10);
    let hook = first.iter().find(|s| s.id == "hook:7").unwrap();
    assert_eq!(
        hook.detail.as_deref(),
        Some("url known after the dispatcher is deployed")
    );

    let applied = s.provisioner.apply(&[]).await.unwrap();
    assert!(all(&applied, Action::Create), "{applied:?}");
    let values = s.outputs.read().await.unwrap();
    assert!(values.contains_key(keys::HOOK_TOKEN));
    assert!(values.contains_key(keys::VM_SECRET));
    assert!(values[&keys::runner_token("ci")].starts_with("glrt-"));
    assert!(values[&keys::runner_token("build")].starts_with("glrt-"));
    assert!(values.contains_key(&keys::runner_id("ci")));
    assert!(values.contains_key(keys::DISPATCH_SECRET_KEY));
    assert!(values.contains_key(keys::CACHE_SECRET_KEY));
    assert_eq!(
        values[keys::DISPATCH_URL],
        "https://demo-dispatch.fake.invalid/"
    );
    assert_eq!(s.fake.runner_count(), 2);
    let hooks = s.fake.hooks();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].2, values[keys::DISPATCH_URL]);
    assert_eq!(
        s.fake.hook_token(hooks[0].0).as_deref(),
        Some(values[keys::HOOK_TOKEN].as_str())
    );
    assert_eq!(s.fake.hook_flags(hooks[0].0), Some((true, false, true)));

    let second = s.provisioner.plan(&[]).await.unwrap();
    assert!(all(&second, Action::Noop), "{second:?}");
    let again = s.provisioner.apply(&[]).await.unwrap();
    assert!(all(&again, Action::Noop), "{again:?}");
    assert_eq!(s.outputs.read().await.unwrap(), values);
    assert!(!s.lines.lock().unwrap().iter().any(|l| l.contains("glrt-")));
}

#[tokio::test]
async fn secrets_reach_the_dispatcher_and_status_counts_runs() {
    let s = setup(config(&[7]), true);
    s.provisioner.apply(&[]).await.unwrap();
    let secrets = secrets_from_outputs(&s.config, &s.outputs.read().await.unwrap());
    assert_eq!(secrets.runner_tokens.len(), 2);
    assert!(secrets.cache.is_some());
    let dispatcher = Dispatcher::new(
        s.config.clone(),
        s.provider.clone(),
        Arc::new(secrets.clone()),
        Log::silent(),
    );
    let headers = vec![(
        "x-gitlab-token".to_string(),
        secrets.hook_token.clone().unwrap(),
    )];
    let body = json!({
        "object_kind": "build", "build_status": "pending", "build_id": 1,
        "pipeline_id": 2, "project_id": 7, "build_stage": "test"
    });
    let response = dispatcher
        .handle(&headers, body.to_string().as_bytes())
        .await;
    assert_eq!(response.body["result"], "started", "{}", response.body);
    let status = s.provisioner.status().await.unwrap();
    assert_eq!(
        status.url.as_deref(),
        Some("https://demo-dispatch.fake.invalid/")
    );
    assert_eq!(status.pools.len(), 2);
    assert_eq!(
        (
            status.pools[0].tag.as_str(),
            status.pools[0].live,
            status.pools[0].max_runs
        ),
        ("ci", 1, 10)
    );
    assert_eq!(
        (status.pools[1].tag.as_str(), status.pools[1].live),
        ("build", 0)
    );
    assert_eq!(status.runs.len(), 1);
    let _ = &s.host;
}

#[tokio::test]
async fn the_vm_secret_is_written_once_and_rotates_on_its_own() {
    let s = setup(config(&[7]), false);
    s.provisioner.apply(&[]).await.unwrap();
    let first = s.outputs.read().await.unwrap();
    let secret = first[keys::VM_SECRET].clone();
    assert_eq!(secret.len(), 32);
    assert!(secret.chars().all(|c| c.is_ascii_alphanumeric()));
    assert_ne!(secret, first[keys::HOOK_TOKEN]);
    let secrets = secrets_from_outputs(&s.config, &first);
    assert_eq!(secrets.vm_secret.as_deref(), Some(secret.as_str()));
    assert!(
        s.host
            .plan(&secrets)
            .await
            .unwrap()
            .iter()
            .all(Step::is_noop),
        "the host got the same secrets, VM secret included"
    );

    s.provisioner.apply(&[]).await.unwrap();
    s.provisioner
        .apply(&[Rotation::Key, Rotation::Cache, Rotation::Runner])
        .await
        .unwrap();
    assert_eq!(s.outputs.read().await.unwrap()[keys::VM_SECRET], secret);

    s.provisioner.apply(&[Rotation::Hook]).await.unwrap();
    let hooked = s.outputs.read().await.unwrap();
    assert_eq!(
        hooked[keys::VM_SECRET],
        secret,
        "the hook leaves running VMs alone"
    );
    assert_ne!(hooked[keys::HOOK_TOKEN], first[keys::HOOK_TOKEN]);
    assert_ne!(hooked[keys::SWEEP_TOKEN], first[keys::SWEEP_TOKEN]);

    s.provisioner.apply(&[Rotation::Vm]).await.unwrap();
    let rotated = s.outputs.read().await.unwrap();
    assert_ne!(rotated[keys::VM_SECRET], secret);
    let secrets = secrets_from_outputs(&s.config, &rotated);
    assert!(
        s.host
            .plan(&secrets)
            .await
            .unwrap()
            .iter()
            .all(Step::is_noop)
    );
    assert!(
        !s.lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains(&secret) || l.contains(&rotated[keys::VM_SECRET]))
    );
}

#[tokio::test]
async fn enables_runners_and_hooks_in_every_project() {
    let s = setup(config(&[7, 8]), true);
    let plan = s.provisioner.plan(&[]).await.unwrap();
    let a = actions(&plan);
    assert_eq!(a["runner-enable:8:ci"], Action::Create);
    assert_eq!(a["hook:8"], Action::Create);
    s.provisioner.apply(&[]).await.unwrap();
    assert_eq!(s.fake.runner_count(), 2);
    assert!(
        s.fake
            .runner_projects()
            .iter()
            .all(|(_, projects)| projects.contains(&8))
    );
    assert_eq!(s.fake.hooks().len(), 2);
    let again = s.provisioner.plan(&[]).await.unwrap();
    assert!(all(&again, Action::Noop), "{again:?}");
}

#[tokio::test]
async fn recreates_a_runner_without_token_and_rotates_everything() {
    let s = setup(config(&[7]), true);
    s.provisioner.apply(&[]).await.unwrap();
    let before = s.outputs.read().await.unwrap();
    s.outputs.clear().await.unwrap();
    s.outputs
        .write(
            &BTreeMap::from([(
                keys::HOOK_TOKEN.to_string(),
                before[keys::HOOK_TOKEN].clone(),
            )]),
            true,
        )
        .await
        .unwrap();
    let plan = s.provisioner.plan(&[]).await.unwrap();
    let step = plan.iter().find(|s| s.id == "runner:ci").unwrap();
    assert_eq!(step.action, Action::Update);
    assert!(step.detail.as_deref().unwrap().contains("token missing"));
    s.provisioner.apply(&[]).await.unwrap();
    let after = s.outputs.read().await.unwrap();
    assert_ne!(
        after[&keys::runner_token("ci")],
        before[&keys::runner_token("ci")]
    );
    assert_eq!(s.fake.runner_count(), 2);

    s.provisioner.apply(&Rotation::ALL).await.unwrap();
    let rotated = s.outputs.read().await.unwrap();
    assert_ne!(
        rotated[&keys::runner_token("ci")],
        after[&keys::runner_token("ci")]
    );
    assert_ne!(rotated[keys::HOOK_TOKEN], after[keys::HOOK_TOKEN]);
    assert_ne!(
        rotated[keys::DISPATCH_SECRET_KEY],
        before[keys::DISPATCH_SECRET_KEY]
    );
    assert_ne!(
        rotated[keys::CACHE_SECRET_KEY],
        before[keys::CACHE_SECRET_KEY]
    );
    let hooks = s.fake.hooks();
    assert_eq!(
        s.fake.hook_token(hooks[0].0).as_deref(),
        Some(rotated[keys::HOOK_TOKEN].as_str())
    );
    assert_eq!(s.fake.runner_count(), 2);
    let plan = s.provisioner.plan(&[]).await.unwrap();
    assert!(all(&plan, Action::Noop), "{plan:?}");
}

#[tokio::test]
async fn destroy_removes_everything() {
    let s = setup(config(&[7]), true);
    s.provisioner.apply(&[]).await.unwrap();
    s.provisioner.destroy().await.unwrap();
    assert_eq!(s.fake.runner_count(), 0);
    assert!(s.fake.hooks().is_empty());
    assert!(s.outputs.read().await.unwrap().is_empty());
    assert_eq!(s.host.url().await.unwrap(), None);
    let plan = s.provider.plan(&[]).await.unwrap();
    assert!(all(&plan, Action::Create), "{plan:?}");
}

#[tokio::test]
async fn works_without_gitlab() {
    let s = setup(config(&[7]), false);
    let plan = s.provisioner.plan(&[]).await.unwrap();
    assert!(
        !plan
            .iter()
            .any(|s| s.id.starts_with("runner") || s.id.starts_with("hook"))
    );
    s.provisioner.apply(&[]).await.unwrap();
    let values = s.outputs.read().await.unwrap();
    assert!(values.contains_key(keys::DISPATCH_URL));
    assert!(!values.contains_key(&keys::runner_token("ci")));
    assert!(
        s.provisioner
            .check(None)
            .await
            .unwrap_err()
            .to_string()
            .contains("needs a GitLab token")
    );
}

#[tokio::test]
async fn checks_pipelines_against_the_stage_map() {
    let s = setup(config(&[7, 8, 9]), true);
    let job = |name: &str, stage: &str, tags: &[&str]| LintJob {
        name: name.into(),
        stage: Some(stage.into()),
        tag_list: tags.iter().map(|t| t.to_string()).collect(),
    };
    s.fake.set_jobs(7, vec![job("unit", "test", &["ci"])]);
    s.fake.set_jobs(8, vec![job("image", "build", &["ci"])]);
    s.fake.set_lint_errors(9, vec!["invalid yaml".into()]);
    let report = s.provisioner.check(None).await.unwrap();
    let summary: Vec<(u64, bool, usize, usize)> = report
        .iter()
        .map(|r| (r.project_id, r.valid, r.problems.len(), r.errors.len()))
        .collect();
    assert_eq!(
        summary,
        vec![(7, true, 0, 0), (8, true, 1, 0), (9, false, 0, 1)]
    );
    assert_eq!(report[1].problems[0].job, "image");
}

fn lanes_config(rust_vm_first: bool) -> Arc<Config> {
    let raw = json!({
        "name": "demo",
        "gitlab": { "url": "https://gitlab.example", "projectIds": [7, 8] },
        "host": { "type": "fake" },
        "provider": { "type": "fake" },
        "defaultPool": "vm",
        "pools": [
            { "tag": "vm", "image": "ubuntu:24.04", "runtime": "instances" },
            { "tag": "node-24", "image": "node:24-bookworm-slim", "runtime": "jobs", "jobs": ["lint"] },
            { "tag": "rust-1", "image": "rust:1.96.0-bookworm", "runtime": "jobs",
              "jobs": ["clippy"], "vmFirst": rust_vm_first }
        ],
        "cache": { "type": "s3" }
    });
    Arc::new(parse_config(&raw.to_string()).unwrap())
}

/// [`lanes_config`] with `vmFirst` of both lanes given.
fn spill_config(node_vm_first: bool, rust_vm_first: bool) -> Arc<Config> {
    let mut config = (*lanes_config(rust_vm_first)).clone();
    config.pools[1].vm_first = Some(node_vm_first);
    Arc::new(config)
}

#[tokio::test]
async fn a_vm_pool_with_vm_first_lanes_gets_a_spill_runner() {
    let fake = Arc::new(FakeGitlab::new());
    let store = Arc::new(MemoryStore::new());
    let host_store = Arc::new(MemoryStore::new());
    let outputs = Arc::new(MemoryOutputs::new());
    let (log, lines) = Log::memory();
    let provisioner = |config: Arc<Config>| {
        Provisioner::new(
            config.clone(),
            Arc::new(FakeProvider::new(config.clone(), store.clone())),
            Arc::new(FakeHost::new(config.clone(), host_store.clone())),
            Some(Arc::new(Gitlab::new(
                &config.gitlab.url,
                FAKE_TOKEN,
                fake.clone(),
            ))),
            outputs.clone(),
            log.clone(),
        )
    };
    let runner_steps = |steps: &[Step]| -> Vec<(String, Action, Option<String>)> {
        steps
            .iter()
            .filter(|s| s.id.starts_with("runner:"))
            .map(|s| (s.id.clone(), s.action, s.detail.clone()))
            .collect()
    };
    let step = |id: &str, action: Action, detail: Option<&str>| {
        (id.to_string(), action, detail.map(String::from))
    };
    let id_of = |values: &Outputs, key: &str| -> u64 { values[key].parse().unwrap() };
    const SPILL_ID: &str = "CIBURST_SPILL_RUNNER_ID_VM";
    const SPILL_TOKEN: &str = "CIBURST_SPILL_RUNNER_TOKEN_VM";

    // M6.1 left the lane tags on the VM pool's runner.
    let first = provisioner(spill_config(false, false));
    first.apply(&[]).await.unwrap();
    let values = outputs.read().await.unwrap();
    assert!(
        !values.contains_key(SPILL_ID),
        "no lane spills, no spill runner"
    );
    assert_eq!(fake.runner_count(), 3);
    let gitlab = Gitlab::new("https://gitlab.example", FAKE_TOKEN, fake.clone());
    let vm_runner = id_of(&values, &keys::runner_id("vm"));
    gitlab
        .update_runner_tags(vm_runner, &["vm".to_string(), "node-24".to_string()])
        .await
        .unwrap();

    let second = provisioner(spill_config(true, false));
    let plan = second.plan(&[]).await.unwrap();
    assert_eq!(
        runner_steps(&plan),
        [
            step("runner:vm", Action::Update, Some("set tags vm")),
            step("runner:vm-spill", Action::Create, Some("tags vm, node-24")),
            step("runner:node-24", Action::Noop, None),
            step("runner:rust-1", Action::Noop, None),
        ]
    );
    let text = format_plan(&plan);
    for line in [
        "update  gitlab runner demo-vm  (set tags vm)",
        "create  gitlab runner demo-vm-spill  (tags vm, node-24)",
        "create  gitlab runner demo-vm-spill enabled in project 8",
    ] {
        assert!(text.lines().any(|l| l == line), "{line} in\n{text}");
    }
    second.apply(&[]).await.unwrap();
    let values = outputs.read().await.unwrap();
    let spill = id_of(&values, SPILL_ID);
    assert!(values[SPILL_TOKEN].starts_with("glrt-"));
    assert!(keys::is_secret(SPILL_TOKEN));
    assert_eq!(fake.runner_tags(vm_runner).unwrap(), ["vm"]);
    assert_eq!(fake.runner_tags(spill).unwrap(), ["vm", "node-24"]);
    assert!(
        fake.runner_projects()
            .iter()
            .any(|(id, projects)| *id == spill && projects == &[7, 8]),
        "enabled in every project"
    );
    let secrets = secrets_from_outputs(&spill_config(true, false), &values);
    assert_eq!(
        secrets.spill_tokens,
        BTreeMap::from([("vm".to_string(), values[SPILL_TOKEN].clone())])
    );
    assert!(
        lines
            .lock()
            .unwrap()
            .contains(&format!(r#"info runner pool="vm-spill" id="{spill}""#))
    );
    assert!(all(&second.plan(&[]).await.unwrap(), Action::Noop));

    let third = provisioner(spill_config(true, true));
    let plan = third.plan(&[]).await.unwrap();
    assert_eq!(
        runner_steps(&plan)[1],
        step(
            "runner:vm-spill",
            Action::Update,
            Some("set tags vm, node-24, rust-1")
        )
    );
    third.apply(&[]).await.unwrap();
    assert_eq!(
        fake.runner_tags(spill).unwrap(),
        ["vm", "node-24", "rust-1"]
    );
    assert_eq!(
        outputs.read().await.unwrap()[SPILL_TOKEN],
        values[SPILL_TOKEN]
    );

    third.apply(&[Rotation::Runner]).await.unwrap();
    let rotated = outputs.read().await.unwrap();
    assert_ne!(id_of(&rotated, SPILL_ID), spill);
    assert_ne!(rotated[SPILL_TOKEN], values[SPILL_TOKEN]);
    assert_eq!(
        fake.runner_tags(id_of(&rotated, SPILL_ID)).unwrap(),
        ["vm", "node-24", "rust-1"],
        "a recreated spill runner gets every tag"
    );

    let fourth = provisioner(spill_config(false, false));
    let plan = fourth.plan(&[]).await.unwrap();
    assert_eq!(
        runner_steps(&plan).last().unwrap(),
        &step("runner:vm-spill", Action::Delete, None)
    );
    fourth.apply(&[]).await.unwrap();
    assert_eq!(fake.runner_count(), 3);
    assert_eq!(fake.runner_tags(id_of(&rotated, SPILL_ID)), None);
    assert!(
        secrets_from_outputs(&spill_config(false, false), &outputs.read().await.unwrap())
            .spill_tokens
            .is_empty(),
        "a stale token is not handed to the dispatcher"
    );

    let fifth = provisioner(spill_config(true, false));
    fifth.apply(&[]).await.unwrap();
    assert_eq!(fake.runner_count(), 4);
    provisioner(spill_config(false, false))
        .destroy()
        .await
        .unwrap();
    assert_eq!(
        fake.runner_count(),
        0,
        "destroy removes the spill runner of a config without lanes too"
    );
}

#[tokio::test]
async fn checks_lane_jobs_and_suggests_lanes_from_the_merged_yaml() {
    let s = setup(lanes_config(true), true);
    let job = |name: &str, tags: &[&str]| LintJob {
        name: name.into(),
        stage: Some("test".into()),
        tag_list: tags.iter().map(|t| t.to_string()).collect(),
    };
    s.fake.set_jobs(
        7,
        vec![
            job("lint", &["node-24"]),
            job("clippy", &["rust-1"]),
            job("unit", &["vm"]),
        ],
    );
    s.fake.set_merged_yaml(
        7,
        "---\nlint:\n  image: node:24-bookworm-slim\n  services:\n  - postgres:17\n  script:\n  - npm run lint\nclippy:\n  image: rust:1.96.0-bookworm\n  script:\n  - cargo clippy\nunit:\n  image: alpine:3.21\n  script:\n  - make test\n",
    );
    s.fake.set_merged_yaml(8, "unit: [");
    let error = s.provisioner.check(None).await.unwrap_err().to_string();
    assert!(error.starts_with("project 8: merged YAML: "), "{error}");

    s.fake.set_merged_yaml(8, "{}");
    let report = s.provisioner.check(None).await.unwrap();
    let problems: Vec<&str> = report[0]
        .problems
        .iter()
        .map(|p| p.problem.as_str())
        .collect();
    assert_eq!(
        problems,
        ["lane node-24 has no Docker daemon but the job has services: postgres:17"]
    );
    assert_eq!(report[0].suggestions.len(), 1);
    assert_eq!(report[0].suggestions[0].tag, "alpine-3");
    assert_eq!(report[0].suggestions[0].jobs, ["unit"]);
    assert!(report[1].problems.is_empty() && report[1].suggestions.is_empty());
}

fn hook_lines(s: &Setup) -> Vec<String> {
    s.lines
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("info hook"))
        .cloned()
        .collect()
}

#[tokio::test]
async fn a_disabled_hook_plans_the_re_enable_and_apply_enables_it() {
    let s = setup(config(&[7, 8]), true);
    s.provisioner.apply(&[]).await.unwrap();
    let hooks = s.fake.hooks();
    let (seven, eight) = (hooks[0].0, hooks[1].0);
    assert_eq!((hooks[0].1, hooks[1].1), (7, 8));
    s.fake.disable_hook(
        seven,
        "temporarily_disabled",
        Some("2026-10-25T12:00:00.000Z"),
    );

    let plan = s.provisioner.plan(&[]).await.unwrap();
    let changed: Vec<&Step> = plan.iter().filter(|step| !step.is_noop()).collect();
    assert_eq!(changed.len(), 1, "{plan:?}");
    assert_eq!(
        format_plan(&changed.into_iter().cloned().collect::<Vec<_>>()),
        "update  gitlab hook in project 7  (re-enable: temporarily_disabled until 2026-10-25T12:00:00Z)"
    );
    let token = s.fake.hook_token(seven);

    s.lines.lock().unwrap().clear();
    s.provisioner.apply(&[]).await.unwrap();
    assert_eq!(hook_lines(&s), [r#"info hook_reenabled project="7""#]);
    assert_eq!(
        s.fake.hook_alert(seven),
        Some(("executable".to_string(), None))
    );
    assert_eq!(s.fake.hook_token(seven), token, "the token stays");
    let calls = s.fake.calls();
    assert!(!calls.iter().any(|c| c.method == "PUT"));
    assert!(
        calls.iter().any(|c| c.method == "POST"
            && c.path == format!("/projects/7/hooks/{seven}/test/push_events"))
    );
    assert!(
        s.provisioner
            .plan(&[])
            .await
            .unwrap()
            .iter()
            .all(Step::is_noop)
    );

    s.fake.disable_hook(eight, "disabled", None);
    let plan = s.provisioner.plan(&[Rotation::Hook]).await.unwrap();
    let details: Vec<(&str, Option<&str>)> = plan
        .iter()
        .filter(|step| step.id.starts_with("hook:"))
        .map(|step| (step.id.as_str(), step.detail.as_deref()))
        .collect();
    assert_eq!(
        details,
        [
            ("hook:7", Some("new token")),
            ("hook:8", Some("new token, re-enable: disabled until never")),
        ]
    );
    s.lines.lock().unwrap().clear();
    s.provisioner.apply(&[Rotation::Hook]).await.unwrap();
    assert_eq!(
        hook_lines(&s),
        [
            r#"info hook project="7""#,
            r#"info hook project="8""#,
            r#"info hook_reenabled project="8""#,
        ]
    );
    assert_eq!(s.fake.hook_alert(eight).unwrap().0, "executable");
}

#[tokio::test]
async fn a_hook_without_pipeline_events_gets_them() {
    let s = setup(config(&[7, 8]), true);
    s.provisioner.apply(&[]).await.unwrap();
    let hooks = s.fake.hooks();
    let (seven, eight) = (hooks[0].0, hooks[1].0);
    s.fake.clear_pipeline_events(seven);
    let plan = s.provisioner.plan(&[]).await.unwrap();
    let changed: Vec<Step> = plan.into_iter().filter(|step| !step.is_noop()).collect();
    assert_eq!(
        format_plan(&changed),
        "update  gitlab hook in project 7  (pipeline events)"
    );
    let token = s.fake.hook_token(seven);
    s.lines.lock().unwrap().clear();
    s.provisioner.apply(&[]).await.unwrap();
    assert_eq!(hook_lines(&s), [r#"info hook project="7""#]);
    assert_eq!(s.fake.hook_flags(seven), Some((true, false, true)));
    assert_eq!(s.fake.hook_token(seven), token, "the token stays");
    assert!(
        s.provisioner
            .plan(&[])
            .await
            .unwrap()
            .iter()
            .all(Step::is_noop)
    );

    s.fake.clear_pipeline_events(seven);
    s.fake.clear_pipeline_events(eight);
    s.fake.disable_hook(eight, "disabled", None);
    let details = |plan: Vec<Step>| -> Vec<(String, Option<String>)> {
        plan.into_iter()
            .filter(|step| step.id.starts_with("hook:"))
            .map(|step| (step.id, step.detail))
            .collect()
    };
    assert_eq!(
        details(s.provisioner.plan(&[]).await.unwrap()),
        [
            ("hook:7".to_string(), Some("pipeline events".to_string())),
            (
                "hook:8".to_string(),
                Some("pipeline events, re-enable: disabled until never".to_string())
            ),
        ]
    );
    assert_eq!(
        details(s.provisioner.plan(&[Rotation::Hook]).await.unwrap())[1].1,
        Some("new token, pipeline events, re-enable: disabled until never".to_string())
    );
    s.lines.lock().unwrap().clear();
    s.provisioner.apply(&[]).await.unwrap();
    assert_eq!(
        hook_lines(&s),
        [
            r#"info hook project="7""#,
            r#"info hook project="8""#,
            r#"info hook_reenabled project="8""#,
        ]
    );
    for hook in [seven, eight] {
        assert_eq!(s.fake.hook_flags(hook), Some((true, false, true)));
    }
    assert_eq!(s.fake.hook_alert(eight).unwrap().0, "executable");
}

#[tokio::test]
async fn the_hook_delivers_pipeline_events_that_warm_a_vm() {
    let raw = json!({
        "name": "demo",
        "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
        "host": { "type": "fake" },
        "provider": { "type": "fake" },
        "defaultPool": "vm",
        "ignoreJobs": ["pages"],
        "pools": [
            { "tag": "vm", "image": "img/node", "runtime": "instances" },
            { "tag": "node", "image": "img/node", "runtime": "jobs", "stages": ["lint"] }
        ],
        "cache": { "type": "s3" }
    });
    let s = setup(Arc::new(parse_config(&raw.to_string()).unwrap()), true);
    s.provisioner.apply(&[]).await.unwrap();
    let hook = s.fake.hooks()[0].0;
    let secrets = secrets_from_outputs(&s.config, &s.outputs.read().await.unwrap());
    let records = Arc::new(MemoryPipelineRecords::new());
    let dispatcher = Dispatcher::new(
        s.config.clone(),
        s.provider.clone(),
        Arc::new(secrets),
        Log::silent(),
    )
    .with_records(Some(records.clone()));
    let builds = [
        FakeBuild::created(11, "eslint", "lint"),
        FakeBuild::created(12, "image", "build"),
        FakeBuild::created(13, "pages", "deploy"),
        FakeBuild::manual(14, "release", "deploy"),
        FakeBuild::created(15, "rollback", "deploy").with_when("on_failure"),
        FakeBuild::created(16, "e2e", "test").with_status("pending"),
    ];
    let (headers, body) = s.fake.pipeline_hook(hook, 31, "pending", &builds).unwrap();
    let event: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(event["object_kind"], "pipeline");
    assert_eq!(event["project"]["id"], 7);
    assert_eq!(
        event["object_attributes"]["stages"],
        json!(["lint", "build", "deploy", "test"])
    );
    assert_eq!(event["builds"][3]["when"], "manual");
    assert!(event["builds"][0].get("tag_list").is_none(), "no tags");
    assert!(headers.contains(&("X-Gitlab-Event".to_string(), "Pipeline Hook".to_string())));
    let response = dispatcher
        .handle_request("POST", "/", &headers, &body, 0)
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body["result"], "warm");
    assert_eq!(response.body["pipeline"], 31);
    assert_eq!(response.body["pools"][0]["action"], "started");
    assert_eq!(response.body["pools"][0]["vmJobs"], 2);
    let jobs = records.vm_jobs("31").await.unwrap().unwrap();
    assert_eq!(jobs["vm"], ["12", "16"]);
    let vms = s.provider.capacity("vm").await.unwrap().unwrap();
    assert_eq!(vms.len(), 1);
    assert_eq!(vms[0].free, 2, "the warm VM has every slot free");

    let (headers, body) = s.fake.pipeline_hook(hook, 31, "running", &builds).unwrap();
    let again = dispatcher
        .handle_request("POST", "/", &headers, &body, 1)
        .await;
    assert_eq!(again.body["result"], "duplicate");
    let (headers, body) = s.fake.pipeline_hook(hook, 31, "success", &builds).unwrap();
    let closed = dispatcher
        .handle_request("POST", "/", &headers, &body, 2)
        .await;
    assert_eq!(closed.body["result"], "closed");
    assert_eq!(records.vm_jobs("31").await.unwrap(), None);

    s.fake.clear_pipeline_events(hook);
    assert!(
        s.fake.pipeline_hook(hook, 32, "pending", &builds).is_none(),
        "a hook without pipeline events gets none"
    );
}

#[tokio::test]
async fn a_failed_test_delivery_fails_apply() {
    let s = setup(config(&[7]), true);
    s.provisioner.apply(&[]).await.unwrap();
    let hook = s.fake.hooks()[0].0;
    s.fake.disable_hook(hook, "disabled", None);
    s.fake.fail_hook_tests();
    let error = s.provisioner.apply(&[]).await.unwrap_err().to_string();
    assert!(
        error.contains("422")
            && error.contains(&format!("/projects/7/hooks/{hook}/test/push_events")),
        "{error}"
    );
    assert_eq!(s.fake.hook_alert(hook).unwrap().0, "disabled");
    assert!(hook_lines(&s).iter().all(|l| !l.contains("hook_reenabled")));
}

#[tokio::test]
async fn check_reports_the_hook_to_the_dispatcher() {
    let s = setup(config(&[7, 8]), true);
    s.provisioner.apply(&[]).await.unwrap();
    let url = s.host.url().await.unwrap().unwrap();
    let other = Gitlab::new(&s.config.gitlab.url, FAKE_TOKEN, s.fake.clone())
        .create_hook(7, "https://other.example/", "x")
        .await
        .unwrap();
    s.fake.disable_hook(other.id, "disabled", None);
    let eight = s.fake.hooks()[1].0;
    s.fake
        .disable_hook(eight, "temporarily_disabled", Some("2026-10-25T13:00:00Z"));
    let report = s.provisioner.check(None).await.unwrap();
    let hooks: Vec<(u64, Option<String>)> = report
        .iter()
        .map(|r| {
            (
                r.project_id,
                r.hook
                    .as_ref()
                    .filter(|h| h.url == url)
                    .map(|h| h.disabled()),
            )
        })
        .collect();
    assert_eq!(
        hooks,
        [
            (7, Some("executable until never".to_string())),
            (
                8,
                Some("temporarily_disabled until 2026-10-25T13:00:00Z".to_string())
            ),
        ],
        "another hook of the project is not ciburst's"
    );
    let report = check(
        &s.config,
        &Gitlab::new(&s.config.gitlab.url, FAKE_TOKEN, s.fake.clone()),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(report.iter().all(|r| r.hook.is_none()), "no URL, no hook");
}

#[tokio::test]
async fn deliveries_come_from_the_hook_to_the_dispatcher() {
    let s = setup(config(&[7, 8]), true);
    assert!(
        s.provisioner
            .hook_deliveries(None)
            .await
            .unwrap()
            .is_empty()
    );
    s.provisioner.apply(&[]).await.unwrap();
    let url = s.host.url().await.unwrap().unwrap();
    let seven = s.fake.hooks()[0].0;
    for id in 1..=25 {
        s.fake.add_hook_event(
            seven,
            json!({
                "id": id,
                "created_at": "2026-10-25T12:00:00Z",
                "request_data": { "object_kind": "build", "build_name": format!("unit {id}/25") },
                "response_status": "200",
                "response_body": json!({ "result": "started", "pool": "ci", "ms": id * 10 }).to_string(),
            }),
        );
    }
    let eight = s.fake.hooks()[1].0;
    Gitlab::new(&s.config.gitlab.url, FAKE_TOKEN, s.fake.clone())
        .delete_hook(8, eight)
        .await
        .unwrap();
    let status = s.provisioner.hook_deliveries(Some(&url)).await.unwrap();
    assert_eq!(status.len(), 2);
    assert_eq!(status[0].hook.as_ref().map(|h| h.id), Some(seven));
    let subjects: Vec<&str> = status[0]
        .deliveries
        .iter()
        .map(|d| d.subject.as_str())
        .collect();
    assert_eq!(subjects.len(), 20);
    assert_eq!(
        (subjects[0], subjects[19]),
        ("unit 6/25", "unit 25/25"),
        "the 20 newest, oldest first"
    );
    assert_eq!(status[0].deliveries[19].ms, "250");
    assert_eq!((status[1].project_id, &status[1].hook), (8, &None));
    assert!(status[1].deliveries.is_empty());
}

#[tokio::test]
async fn host_url_is_known_after_apply() {
    let s = setup(config(&[7]), false);
    assert_eq!(s.host.url().await.unwrap(), None);
    s.provisioner.apply(&[]).await.unwrap();
    assert!(s.host.url().await.unwrap().is_some());
    let steps: Vec<Step> = s.provisioner.plan(&[]).await.unwrap();
    assert!(
        steps
            .iter()
            .any(|s| s.id == "dispatcher" && s.action == Action::Noop)
    );
}

/// Fails every step with the given id, like a Scaleway call that rejects a job definition.
struct FailingStep {
    inner: Arc<FakeProvider>,
    fail: &'static str,
}

#[async_trait::async_trait]
impl Provider for FailingStep {
    fn capabilities(&self) -> ciburst_core::types::Capabilities {
        self.inner.capabilities()
    }
    async fn plan(&self, rotate: &[Rotation]) -> ciburst_core::Result<Vec<Step>> {
        self.inner.plan(rotate).await
    }
    async fn apply(&self, steps: &[Step]) -> ciburst_core::Result<ciburst_core::types::Outputs> {
        if steps.iter().any(|s| s.id == self.fail) {
            return Err(ciburst_core::Error::provider("registry image not found"));
        }
        self.inner.apply(steps).await
    }
    async fn destroy(&self) -> ciburst_core::Result<()> {
        self.inner.destroy().await
    }
    async fn start(
        &self,
        request: ciburst_core::types::StartRequest,
    ) -> ciburst_core::Result<String> {
        self.inner.start(request).await
    }
    async fn list(
        &self,
        pool: Option<&str>,
    ) -> ciburst_core::Result<Vec<ciburst_core::types::Run>> {
        self.inner.list(pool).await
    }
    async fn stop(&self, run_id: &str) -> ciburst_core::Result<()> {
        self.inner.stop(run_id).await
    }
}

#[tokio::test]
async fn secrets_survive_a_later_failing_step() {
    let config = config(&[7]);
    let inner = Arc::new(FakeProvider::new(
        config.clone(),
        Arc::new(MemoryStore::new()),
    ));
    let failing = Arc::new(FailingStep {
        inner: inner.clone(),
        fail: "definition:build",
    });
    let outputs = Arc::new(MemoryOutputs::new());
    let host = Arc::new(FakeHost::new(config.clone(), Arc::new(MemoryStore::new())));
    let provisioner = Provisioner::new(
        config.clone(),
        failing,
        host.clone(),
        None,
        outputs.clone(),
        Log::silent(),
    );
    assert!(provisioner.apply(&[]).await.is_err());
    let saved = outputs.read().await.unwrap();
    assert!(saved.contains_key(keys::DISPATCH_SECRET_KEY), "{saved:?}");
    assert!(saved.contains_key(keys::CACHE_SECRET_KEY), "{saved:?}");

    let healthy = Provisioner::new(config, inner, host, None, outputs.clone(), Log::silent());
    healthy.apply(&[]).await.unwrap();
    let after = outputs.read().await.unwrap();
    assert_eq!(
        after[keys::DISPATCH_SECRET_KEY],
        saved[keys::DISPATCH_SECRET_KEY]
    );
}

#[tokio::test]
async fn a_removed_pool_deletes_its_runner() {
    let s = setup(config(&[7]), true);
    s.provisioner.apply(&[]).await.unwrap();
    assert_eq!(s.fake.runner_count(), 2);
    let mut smaller = (*s.config).clone();
    smaller.pools.retain(|p| p.tag != "build");
    let smaller = Arc::new(smaller);
    let provider = Arc::new(FakeProvider::new(
        smaller.clone(),
        Arc::new(MemoryStore::new()),
    ));
    let provisioner = Provisioner::new(
        smaller,
        provider,
        s.host.clone(),
        Some(Arc::new(Gitlab::new(
            "https://gitlab.example",
            FAKE_TOKEN,
            s.fake.clone(),
        ))),
        s.outputs.clone(),
        Log::silent(),
    );
    let plan = provisioner.plan(&[]).await.unwrap();
    let step = plan.iter().find(|s| s.id == "runner:build").unwrap();
    assert_eq!(step.action, Action::Delete);
    provisioner.apply(&[]).await.unwrap();
    assert_eq!(s.fake.runner_count(), 1);
    let again = provisioner.plan(&[]).await.unwrap();
    assert!(!again.iter().any(|s| s.id == "runner:build"), "{again:?}");

    let mut prefix_config = (*s.config).clone();
    prefix_config.name = "de".into();
    prefix_config.pools.retain(|p| p.tag == "ci");
    let prefix_config = Arc::new(prefix_config);
    let other = Provisioner::new(
        prefix_config.clone(),
        Arc::new(FakeProvider::new(
            prefix_config,
            Arc::new(MemoryStore::new()),
        )),
        s.host.clone(),
        Some(Arc::new(Gitlab::new(
            "https://gitlab.example",
            FAKE_TOKEN,
            s.fake.clone(),
        ))),
        Arc::new(MemoryOutputs::new()),
        Log::silent(),
    );
    let foreign = other.plan(&[]).await.unwrap();
    assert!(
        !foreign
            .iter()
            .any(|s| s.action == Action::Delete && s.id.starts_with("runner:")),
        "runner demo-ci belongs to another deployment: {foreign:?}"
    );
}

#[tokio::test]
async fn the_vm_secret_is_not_rotated_while_a_vm_runs() {
    let mut vm_config = (*config(&[7])).clone();
    vm_config.pools[0].runtime = Some("instances".into());
    let s = setup(Arc::new(vm_config), false);
    s.provisioner.apply(&[]).await.unwrap();
    let before = s.outputs.read().await.unwrap()[keys::VM_SECRET].clone();
    let pool = s.config.pools[0].clone();
    s.provider
        .start(ciburst_core::types::StartRequest {
            pool,
            env: BTreeMap::from([
                ("RUNNER_TOKEN".to_string(), "glrt-ci".to_string()),
                ("CIBURST_VM_SECRET".to_string(), before.clone()),
            ]),
            labels: BTreeMap::new(),
            taken: 1,
        })
        .await
        .unwrap();
    let error = s.provisioner.apply(&[Rotation::Vm]).await.unwrap_err();
    assert!(error.to_string().contains("1 VMs are running"), "{error}");
    assert_eq!(s.outputs.read().await.unwrap()[keys::VM_SECRET], before);
}

/// The fake provider behind a preflight that fails, and answering `extra` from every apply
/// call, as providers answer values they derive each time.
struct Wrapped {
    inner: Arc<FakeProvider>,
    preflight: Option<&'static str>,
    calls: AtomicU64,
    extra: fn(u64) -> Outputs,
}

impl Wrapped {
    fn new(inner: Arc<FakeProvider>) -> Wrapped {
        Wrapped {
            inner,
            preflight: None,
            calls: AtomicU64::new(0),
            extra: |_| Outputs::new(),
        }
    }
}

#[async_trait::async_trait]
impl Provider for Wrapped {
    fn capabilities(&self) -> ciburst_core::types::Capabilities {
        self.inner.capabilities()
    }
    async fn preflight(&self) -> ciburst_core::Result<()> {
        match self.preflight {
            Some(reason) => Err(ciburst_core::Error::provider(reason)),
            None => Ok(()),
        }
    }
    async fn plan(&self, rotate: &[Rotation]) -> ciburst_core::Result<Vec<Step>> {
        self.inner.plan(rotate).await
    }
    async fn apply(&self, steps: &[Step]) -> ciburst_core::Result<Outputs> {
        let mut outputs = self.inner.apply(steps).await?;
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        outputs.extend((self.extra)(call));
        Ok(outputs)
    }
    async fn destroy(&self) -> ciburst_core::Result<()> {
        self.inner.destroy().await
    }
    async fn start(
        &self,
        request: ciburst_core::types::StartRequest,
    ) -> ciburst_core::Result<String> {
        self.inner.start(request).await
    }
    async fn list(
        &self,
        pool: Option<&str>,
    ) -> ciburst_core::Result<Vec<ciburst_core::types::Run>> {
        self.inner.list(pool).await
    }
    async fn stop(&self, run_id: &str) -> ciburst_core::Result<()> {
        self.inner.stop(run_id).await
    }
}

fn provisioner_with(s: &Setup, provider: Arc<dyn Provider>, with_gitlab: bool) -> Provisioner {
    let gitlab = with_gitlab.then(|| {
        Arc::new(Gitlab::new(
            &s.config.gitlab.url,
            FAKE_TOKEN,
            s.fake.clone(),
        ))
    });
    Provisioner::new(
        s.config.clone(),
        provider,
        s.host.clone(),
        gitlab,
        s.outputs.clone(),
        Log::silent(),
    )
}

#[tokio::test]
async fn a_failing_preflight_changes_nothing() {
    let s = setup(config(&[7]), true);
    let mut wrapped = Wrapped::new(s.provider.clone());
    wrapped.preflight = Some("fr-par-1 has no quota for DEV1-M; zones with quota: nl-ams-1");
    let provisioner = provisioner_with(&s, Arc::new(wrapped), true);
    let error = provisioner.apply(&[]).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "fr-par-1 has no quota for DEV1-M; zones with quota: nl-ams-1"
    );
    assert!(all(&s.provider.plan(&[]).await.unwrap(), Action::Create));
    assert_eq!(s.host.url().await.unwrap(), None);
    assert_eq!(s.fake.runner_count(), 0);
    assert!(s.fake.hooks().is_empty());
    assert!(s.outputs.read().await.unwrap().is_empty());
    assert!(s.fake.calls().is_empty(), "GitLab untouched");

    let plan = provisioner.plan(&[]).await.unwrap();
    assert!(
        all(&plan, Action::Create),
        "plan does not run the preflight"
    );
}

#[tokio::test]
async fn a_missing_key_pair_in_the_outputs_rotates_it() {
    for (access, secret, step) in [
        (
            keys::DISPATCH_ACCESS_KEY,
            keys::DISPATCH_SECRET_KEY,
            "key:dispatch",
        ),
        (keys::CACHE_ACCESS_KEY, keys::CACHE_SECRET_KEY, "key:cache"),
    ] {
        let s = setup(config(&[7]), true);
        s.provisioner.apply(&[]).await.unwrap();
        let before = s.outputs.read().await.unwrap();
        let mut lost = before.clone();
        lost.remove(secret);
        s.outputs.clear().await.unwrap();
        s.outputs.write(&lost, true).await.unwrap();

        let plan = s.provisioner.plan(&[]).await.unwrap();
        let changed: Vec<(&str, Action)> = plan
            .iter()
            .filter(|s| !s.is_noop())
            .map(|s| (s.id.as_str(), s.action))
            .collect();
        assert_eq!(
            changed,
            [(step, Action::Update), ("dispatcher", Action::Update)],
            "{secret}"
        );
        s.provisioner.apply(&[]).await.unwrap();
        let after = s.outputs.read().await.unwrap();
        assert_ne!(after[secret], before[secret]);
        assert_ne!(after[access], before[access]);
        let again = s.provisioner.plan(&[]).await.unwrap();
        assert!(all(&again, Action::Noop), "{again:?}");
    }
}

#[tokio::test]
async fn a_missing_runner_token_recreates_only_its_runner() {
    let s = setup(config(&[7]), true);
    s.provisioner.apply(&[]).await.unwrap();
    let before = s.outputs.read().await.unwrap();
    let mut lost = before.clone();
    lost.remove(&keys::runner_token("build"));
    s.outputs.clear().await.unwrap();
    s.outputs.write(&lost, true).await.unwrap();
    let plan = s.provisioner.plan(&[]).await.unwrap();
    let changed: Vec<&str> = plan
        .iter()
        .filter(|s| !s.is_noop())
        .map(|s| s.id.as_str())
        .collect();
    assert_eq!(changed, ["runner:build", "dispatcher"]);
    s.provisioner.apply(&[]).await.unwrap();
    let after = s.outputs.read().await.unwrap();
    assert!(after.contains_key(&keys::runner_token("build")));
    assert_eq!(
        after[&keys::runner_token("ci")],
        before[&keys::runner_token("ci")]
    );
}

#[tokio::test]
async fn without_an_s3_cache_no_cache_key_is_needed() {
    let mut raw = (*config(&[7])).clone();
    raw.cache = Default::default();
    let s = setup(Arc::new(raw), false);
    s.provisioner.apply(&[]).await.unwrap();
    assert!(
        !s.outputs
            .read()
            .await
            .unwrap()
            .contains_key(keys::CACHE_SECRET_KEY)
    );
    let plan = s.provisioner.plan(&[]).await.unwrap();
    assert!(all(&plan, Action::Noop), "{plan:?}");
    assert!(!plan.iter().any(|s| s.id == "key:cache"));
}

#[tokio::test]
async fn every_apply_call_refreshes_provider_and_dispatcher_env() {
    let s = setup(config(&[7]), false);
    let mut wrapped = Wrapped::new(s.provider.clone());
    wrapped.extra = |call| {
        Outputs::from([
            (keys::AGENT_SHA256.to_string(), format!("sha-{call}")),
            ("HCLOUD_TOKEN".to_string(), format!("hcloud-{call}")),
            (keys::PROJECT_ID.to_string(), format!("project-{call}")),
        ])
    };
    let wrapped = Arc::new(wrapped);
    let provisioner = provisioner_with(&s, wrapped.clone(), false);
    provisioner.apply(&[]).await.unwrap();
    let first = s.outputs.read().await.unwrap();
    let calls = wrapped.calls.load(Ordering::SeqCst);
    assert_eq!(
        first[keys::AGENT_SHA256],
        format!("sha-{calls}"),
        "tail call"
    );
    assert_eq!(first["HCLOUD_TOKEN"], format!("hcloud-{calls}"));
    let kept = format!("project-{}", calls - 1);
    assert_eq!(
        first[keys::PROJECT_ID],
        kept,
        "the last step's, not the tail's"
    );

    provisioner.apply(&[]).await.unwrap();
    let second = s.outputs.read().await.unwrap();
    let calls = wrapped.calls.load(Ordering::SeqCst);
    assert_eq!(second[keys::AGENT_SHA256], format!("sha-{calls}"));
    assert_eq!(second["HCLOUD_TOKEN"], format!("hcloud-{calls}"));
    assert_eq!(
        second[keys::PROJECT_ID],
        kept,
        "a tail value does not replace it"
    );
    let secrets = secrets_from_outputs(&s.config, &second);
    assert_eq!(
        secrets.dispatcher_env[keys::AGENT_SHA256],
        format!("sha-{calls}")
    );
    assert_eq!(
        secrets.provider_env["HCLOUD_TOKEN"],
        format!("hcloud-{calls}")
    );
    assert!(
        s.host
            .plan(&secrets)
            .await
            .unwrap()
            .iter()
            .all(Step::is_noop),
        "the host got the refreshed values"
    );
}

/// The variables of `project` in scope `ciburst`, as a pipeline job of that environment sees
/// them.
fn job_env(fake: &FakeGitlab, project: u64) -> BTreeMap<String, String> {
    fake.variables(project)
        .into_iter()
        .filter(|v| v.environment_scope == OUTPUTS_SCOPE)
        .map(|v| (v.key, v.value))
        .collect()
}

#[tokio::test]
async fn pipeline_mode_keeps_the_outputs_in_project_variables() {
    let s = setup(config(&[7]), true);
    let gitlab = Arc::new(Gitlab::new(
        &s.config.gitlab.url,
        FAKE_TOKEN,
        s.fake.clone(),
    ));
    let store = |env: &BTreeMap<String, String>| {
        Arc::new(GitlabOutputs::new(
            gitlab.clone(),
            7,
            keys::output_keys(&s.config),
            env,
            Log::silent(),
        ))
    };
    let first = Provisioner::new(
        s.config.clone(),
        s.provider.clone(),
        s.host.clone(),
        Some(gitlab.clone()),
        store(&BTreeMap::new()),
        Log::silent(),
    );
    first.apply(&[]).await.unwrap();
    let env = job_env(&s.fake, 7);
    for key in [
        keys::HOOK_TOKEN,
        keys::VM_SECRET,
        keys::SWEEP_TOKEN,
        keys::DISPATCH_URL,
        keys::DISPATCH_SECRET_KEY,
        keys::CACHE_SECRET_KEY,
    ] {
        assert!(env.contains_key(key), "{key} in {env:?}");
    }
    assert!(env[&keys::runner_token("ci")].starts_with("glrt-"));
    assert!(s.fake.variables(7).iter().all(|v| v.protected && v.raw));

    let second = Provisioner::new(
        s.config.clone(),
        s.provider.clone(),
        s.host.clone(),
        Some(gitlab.clone()),
        store(&env),
        Log::silent(),
    );
    let plan = second.plan(&[]).await.unwrap();
    assert!(all(&plan, Action::Noop), "{plan:?}");
    let writes = s.fake.calls().len();
    second.apply(&[]).await.unwrap();
    assert!(
        !s.fake.calls()[writes..]
            .iter()
            .any(|c| c.path.contains("/variables") && c.method != "GET"),
        "a second setup writes no variable"
    );

    second.destroy().await.unwrap();
    assert!(job_env(&s.fake, 7).is_empty());
    assert_eq!(s.fake.runner_count(), 0);
}
