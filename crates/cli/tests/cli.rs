use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ciburst::{Env, agent_path};
use ciburst_core::check::LintJob;
use ciburst_core::config::{Config, Route, parse_config};
use ciburst_core::gitlab::Gitlab;
use ciburst_core::http::HttpClient;
use ciburst_core::log::Log;
use ciburst_core::testing::gitlab_fake::{FAKE_TOKEN, FakeGitlab};

const FAKE_CONFIG: &str = r#"{
  "name": "demo",
  "gitlab": { "url": "https://gitlab.com", "projectIds": [1] },
  "host": { "type": "fake" },
  "provider": { "type": "fake" },
  "vm": { "tag": "ci", "image": "example.invalid/node:24" },
  "lanes": [ { "tag": "build", "image": "example.invalid/node:24", "jobs": ["build"], "maxRuns": 2 } ],
  "cache": { "type": "s3" }
}"#;

const SCALEWAY_CONFIG: &str = r#"{
  "name": "demo",
  "gitlab": { "url": "https://gitlab.com", "projectIds": [1] },
  "host": { "type": "scaleway-function", "region": "fr-par" },
  "provider": { "type": "scaleway", "region": "fr-par" },
  "vm": { "tag": "vm", "image": "ubuntu:24.04", "runtime": "instances" },
  "cache": { "type": "s3" }
}"#;

fn repo_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(name)
}

fn env(http: Option<Arc<dyn HttpClient>>) -> Env {
    Env {
        vars: BTreeMap::new(),
        http,
        exe: None,
        log: Log::silent(),
    }
}

async fn run(env: Env, args: &[&str]) -> (i32, String) {
    let mut all = vec!["ciburst".to_string()];
    all.extend(args.iter().map(|s| s.to_string()));
    let mut out = Vec::new();
    let code = ciburst::run_with_env(all, env, &mut out)
        .await
        .unwrap_or_else(|error| {
            out.extend_from_slice(error.to_string().as_bytes());
            1
        });
    (code, String::from_utf8(out).unwrap())
}

fn paths(dir: &Path) -> (String, PathBuf) {
    (
        dir.join("config.json").display().to_string(),
        dir.join("local"),
    )
}

async fn ciburst(dir: &Path, command: &[&str]) -> (i32, String) {
    let (config, local) = paths(dir);
    let local = local.display().to_string();
    let mut args = vec!["--config", &config, "--local", &local, "--skip-gitlab"];
    args.extend_from_slice(command);
    run(env(None), &args).await
}

#[tokio::test]
async fn dry_run_is_idempotent_and_destroy_asks_for_the_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), FAKE_CONFIG).unwrap();

    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("create  project demo"), "{out}");
    assert!(out.contains("create  job definition demo-1-build"), "{out}");
    assert!(out.contains("update  fleet config"), "{out}");
    assert!(out.contains("create  member 1"), "{out}");
    assert!(
        out.contains("8 to create, 1 to update, 0 to delete, 0 unchanged"),
        "{out}"
    );

    let (code, out) = ciburst(dir.path(), &["apply"]).await;
    assert_eq!(code, 0, "{out}");
    let outputs_path = dir.path().join("local").join("outputs.env");
    assert!(
        out.contains(&format!("outputs: {}", outputs_path.display())),
        "{out}"
    );
    let outputs = std::fs::read_to_string(&outputs_path).unwrap();
    assert!(outputs.contains("CIBURST_DISPATCH_URL=https://demo-dispatch.fake.invalid/"));
    assert!(outputs.contains("CIBURST_HOOK_TOKEN="));

    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("0 to create, 0 to update, 0 to delete, 9 unchanged"),
        "{out}"
    );

    let (code, out) = ciburst(dir.path(), &["status"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("dispatcher: https://demo-dispatch.fake.invalid/"),
        "{out}"
    );
    assert!(out.contains("vm runner ci: 0 live of 4"), "{out}");
    assert!(
        out.contains("lane build of project 1 (example.invalid/node:24): 0 live of 2"),
        "{out}"
    );
    assert!(out.contains("\nmember 1 joined "), "{out}");

    let (code, out) = ciburst(dir.path(), &["destroy"]).await;
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("--yes demo"), "{out}");

    let (code, out) = ciburst(dir.path(), &["destroy", "--yes", "demo"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("destroyed demo"), "{out}");
    let outputs = std::fs::read_to_string(&outputs_path).unwrap();
    assert!(outputs.is_empty());

    let (code, out) = ciburst(dir.path(), &["rotate", "everything"]).await;
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("runner, hook, key, cache, vm"), "{out}");
}

#[tokio::test]
async fn the_dry_run_config_shows_the_default_vm_runner() {
    let dir = tempfile::tempdir().unwrap();
    let config = repo_file("test/config.fake.json").display().to_string();
    let local = dir.path().display().to_string();
    let (code, out) = run(
        env(None),
        &[
            "plan",
            "--config",
            &config,
            "--local",
            &local,
            "--skip-gitlab",
        ],
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("create  job definition demo-vm"), "{out}");
    assert!(
        out.contains("7 to create, 1 to update, 0 to delete, 0 unchanged"),
        "{out}"
    );
    let config =
        parse_config(&std::fs::read_to_string(repo_file("test/config.fake.json")).unwrap())
            .unwrap();
    assert_eq!(config.vm, Config::ci_defaults(1, "https://gitlab.com").vm);
    assert!(config.lanes.is_empty());
}

#[test]
fn the_example_config_is_the_pipeline_default() {
    let text = std::fs::read_to_string(repo_file("config.example.json")).unwrap();
    assert!(text.contains(r#""projectIds": [0]"#));
    let example =
        parse_config(&text.replace(r#""projectIds": [0]"#, r#""projectIds": [5]"#)).unwrap();
    let defaults = Config {
        name: "ci-example".to_string(),
        ..Config::ci_defaults(5, "https://gitlab.com")
    };
    assert_eq!(example, defaults);
}

#[tokio::test]
async fn reports_unknown_modules_and_bad_config() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        FAKE_CONFIG.replace(
            r#""provider": { "type": "fake" }"#,
            r#""provider": { "type": "moon" }"#,
        ),
    )
    .unwrap();
    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 1);
    assert!(out.contains("no provider named moon"), "{out}");

    std::fs::write(
        dir.path().join("config.json"),
        FAKE_CONFIG.replace(r#""lanes""#, r#""defaultPool": "ci", "lanes""#),
    )
    .unwrap();
    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 1);
    assert!(
        out.contains("defaultPool: removed: a job that no lane and no ignoreJobs entry takes runs on the VM runner (vm)"),
        "{out}"
    );

    std::fs::write(
        dir.path().join("config.json"),
        FAKE_CONFIG.replace(r#""maxRuns": 2"#, r#""maxRuns": 2, "vcpu": 64"#),
    )
    .unwrap();
    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 1);
    assert!(out.contains("above fake maximum 6"), "{out}");

    std::fs::write(
        dir.path().join("config.json"),
        FAKE_CONFIG.replace(
            r#""cache": { "type": "s3" }"#,
            r#""cache": { "type": "s3" }, "providers": { "hetzner": {} }"#,
        ),
    )
    .unwrap();
    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 1);
    assert!(
        out.contains("providers: removed: Scaleway is the only provider (D65)"),
        "{out}"
    );
}

#[tokio::test]
async fn help_and_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (code, out) = run(env(None), &["--help"]).await;
    assert_eq!(code, 0);
    assert!(
        out.contains("GitLab runners that exist only while jobs run"),
        "{out}"
    );
    assert!(!out.contains("hetzner"), "{out}");
    assert!(out.contains("VM runner"), "{out}");
    assert!(!out.contains("pool"), "{out}");
    assert!(out.contains("--ci"), "{out}");
    assert!(out.contains("--agent"), "{out}");
    let (code, out) = run(env(None), &["rotate", "--help"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("runner, hook, key, cache or vm"), "{out}");
    let (code, _) = ciburst(dir.path(), &["frobnicate"]).await;
    assert_eq!(code, 2);
}

#[tokio::test]
async fn check_needs_only_the_config_and_gitlab() {
    let dir = tempfile::tempdir().unwrap();
    let (config, local) = paths(dir.path());
    std::fs::write(&config, SCALEWAY_CONFIG).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("gitlab.env"),
        format!("GITLAB_TOKEN={FAKE_TOKEN}\n"),
    )
    .unwrap();
    let fake = Arc::new(FakeGitlab::new());
    fake.set_jobs(
        1,
        vec![LintJob {
            name: "unit".into(),
            stage: Some("test".into()),
            tag_list: vec!["vm".into()],
        }],
    );
    let local = local.display().to_string();
    let (code, out) = run(
        env(Some(fake.clone())),
        &["--config", &config, "--local", &local, "check"],
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out,
        "project 1: every job matches its lanes and the VM runner\n"
    );
    assert!(fake.calls().iter().all(|c| c.path == "/projects/1/ci/lint"));
}

/// GitLab's merged YAML of a pipeline: includes merged, `extends` and `default:` not applied.
const MERGED_YAML: &str = r#"---
variables:
  RUST_VERSION: 1.96.0
default:
  interruptible: true
stages:
- build
- test
".node":
  image: node:24-bookworm-slim
  before_script:
  - npm ci
lint:
  extends: ".node"
  stage: test
  script:
  - npm run lint
cargo-test:
  stage: test
  image: rust:${RUST_VERSION}-bookworm
  script:
  - cargo test --locked
integration:
  extends: ".node"
  stage: test
  services:
  - postgres:17
  script:
  - npm run test:integration
docker-build:
  stage: build
  image: docker:27
  services:
  - docker:dind
  script:
  - docker build -t app .
ciburst-check:
  stage: ".pre"
  image: registry.gitlab.com/ciburst/ciburst/cli:edge
  script:
  - ciburst check
"#;

#[tokio::test]
async fn check_suggest_prints_lanes_for_docker_free_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let (config, local) = paths(dir.path());
    std::fs::write(&config, SCALEWAY_CONFIG).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("gitlab.env"),
        format!("GITLAB_TOKEN={FAKE_TOKEN}\n"),
    )
    .unwrap();
    let job = |name: &str, stage: &str, tag: &str| LintJob {
        name: name.into(),
        stage: Some(stage.into()),
        tag_list: vec![tag.into()],
    };
    let jobs = |lint: &str, cargo: &str| {
        vec![
            job("ciburst-check", ".pre", "saas-linux-small-amd64"),
            job("docker-build", "build", "vm"),
            job("lint", "test", lint),
            job("cargo-test", "test", cargo),
            job("integration", "test", "vm"),
        ]
    };
    let fake = Arc::new(FakeGitlab::new());
    fake.set_jobs(1, jobs("vm", "vm"));
    fake.set_merged_yaml(1, MERGED_YAML);
    let local = local.display().to_string();
    let args = ["--config", &config, "--local", &local, "check", "--suggest"];
    let (code, out) = run(env(Some(fake.clone())), &args).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out,
        concat!(
            "project 1: lane node-24 for node:24-bookworm-slim\n",
            r#"  { "tag": "node-24", "image": "node:24-bookworm-slim", "jobs": ["lint"], "vcpu": 2, "memoryMiB": 4096 }"#,
            "\n",
            "  job lint: tags: [node-24]\n",
            "project 1: lane rust-1 for rust:1.96.0-bookworm\n",
            r#"  { "tag": "rust-1", "image": "rust:1.96.0-bookworm", "jobs": ["cargo-test"], "vcpu": 2, "memoryMiB": 4096 }"#,
            "\n",
            "  job cargo-test: tags: [rust-1]\n",
        )
    );
    let lint = fake.calls().pop().unwrap();
    assert_eq!(lint.query, "include_jobs=true&include_merged_yaml=true");

    let entries: Vec<&str> = out
        .lines()
        .filter_map(|line| line.strip_prefix("  {").map(|_| line.trim()))
        .collect();
    let with_lanes = SCALEWAY_CONFIG.replace(
        r#""cache""#,
        &format!(r#""lanes": [{}], "cache""#, entries.join(", ")),
    );
    let parsed = parse_config(&with_lanes).unwrap();
    assert_eq!(parsed.lanes.len(), 2);
    assert!(matches!(parsed.route("lint"), Route::Lane(lane) if lane.tag == "node-24"));
    assert_eq!(
        (parsed.lanes[1].vcpu, parsed.lanes[1].memory_mib),
        (2, 4096)
    );
    std::fs::write(&config, &with_lanes).unwrap();
    fake.set_jobs(1, jobs("node-24", "rust-1"));
    let (code, out) = run(env(Some(fake.clone())), &args[..5]).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out,
        "project 1: every job matches its lanes and the VM runner\n"
    );
    let (code, out) = run(env(Some(fake.clone())), &args).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(out, "project 1: no Docker-free job runs on the VM runner\n");

    fake.set_jobs(1, jobs("vm", "rust-1"));
    let (code, out) = run(env(Some(fake.clone())), &args[..5]).await;
    assert_eq!(code, 1, "{out}");
    assert_eq!(
        out,
        "project 1: job lint (stage test): lane node-24 takes the job by name but the job is tagged vm\n"
    );
    fake.set_lint_errors(
        1,
        vec!["jobs config should contain at least one visible job".into()],
    );
    let (code, out) = run(env(Some(fake.clone())), &args).await;
    assert_eq!(code, 1, "{out}");
    assert_eq!(
        out,
        "project 1: pipeline invalid: jobs config should contain at least one visible job\n"
    );
}

#[tokio::test]
async fn check_suggest_gives_parallel_and_matrix_jobs_one_tags_line() {
    let dir = tempfile::tempdir().unwrap();
    let (config, local) = paths(dir.path());
    std::fs::write(&config, SCALEWAY_CONFIG).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("gitlab.env"),
        format!("GITLAB_TOKEN={FAKE_TOKEN}\n"),
    )
    .unwrap();
    let yaml = r#"---
".node":
  image: node:24-bookworm-slim
e2e:
  extends: ".node"
  parallel:
    matrix:
    - PLATFORM:
      - android
      - ios
  script:
  - npm run e2e
unit:
  extends: ".node"
  parallel: 2
  script:
  - npm test
unit-smoke:
  image: alpine:3.21
  script:
  - "true"
"#;
    let job = |name: &str, tag: &str| LintJob {
        name: name.into(),
        stage: Some("test".into()),
        tag_list: vec![tag.into()],
    };
    let fake = Arc::new(FakeGitlab::new());
    fake.set_jobs(
        1,
        vec![
            job("e2e: [android]", "vm"),
            job("e2e: [ios]", "vm"),
            job("unit 1/2", "vm"),
            job("unit 2/2", "vm"),
            job("unit-smoke", "saas-linux-small-amd64"),
        ],
    );
    fake.set_merged_yaml(1, yaml);
    let local = local.display().to_string();
    let args = ["--config", &config, "--local", &local, "check", "--suggest"];
    let (code, out) = run(env(Some(fake.clone())), &args).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out,
        concat!(
            "project 1: lane node-24 for node:24-bookworm-slim\n",
            r#"  { "tag": "node-24", "image": "node:24-bookworm-slim", "jobs": ["e2e*", "unit 1/2", "unit 2/2"], "vcpu": 2, "memoryMiB": 4096 }"#,
            "\n",
            "  job e2e: tags: [node-24]\n",
            "  job unit: tags: [node-24]\n",
            "project 1: jobs on other runners\n",
            "  \"ignoreJobs\": [\"unit-smoke\"]\n",
        )
    );
    let (code, out) = run(env(Some(fake.clone())), &args[..5]).await;
    assert_eq!(code, 1, "{out}");
    assert_eq!(
        out,
        "project 1: job unit-smoke (stage test): runs on other runners (tags saas-linux-small-amd64) but ignoreJobs misses it: its pending events start runs\n"
    );

    let ignoring = SCALEWAY_CONFIG.replacen('{', r#"{ "ignoreJobs": ["unit-smoke"],"#, 1);
    std::fs::write(&config, ignoring).unwrap();
    let (code, out) = run(env(Some(fake.clone())), &args[..5]).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out,
        "project 1: every job matches its lanes and the VM runner\n"
    );
    let (code, out) = run(env(Some(fake.clone())), &args).await;
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("ignoreJobs"), "{out}");
}

/// A delivery as GitLab's hook events list shows it.
fn delivery(id: u64, request: serde_json::Value, response: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "url": "https://demo-dispatch.fake.invalid/",
        "trigger": "job_hooks",
        "request_data": request,
        "response_body": response,
        "response_status": "200",
        "execution_duration": 0.5,
        "created_at": format!("2026-10-25T12:00:0{id}.000Z"),
    })
}

#[tokio::test]
async fn status_lists_the_deliveries_and_check_a_disabled_hook() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), FAKE_CONFIG).unwrap();
    let (code, out) = ciburst(dir.path(), &["apply"]).await;
    assert_eq!(code, 0, "{out}");
    let (config, local) = paths(dir.path());
    std::fs::write(
        local.join("gitlab.env"),
        format!("GITLAB_TOKEN={FAKE_TOKEN}\n"),
    )
    .unwrap();
    let fake = Arc::new(FakeGitlab::new());
    let gitlab = Gitlab::new("https://gitlab.com", FAKE_TOKEN, fake.clone());
    let hook = gitlab
        .create_hook(1, "https://demo-dispatch.fake.invalid/", "t")
        .await
        .unwrap()
        .id;
    fake.add_hook_event(
        hook,
        delivery(
            1,
            serde_json::json!({ "object_kind": "build", "build_name": "e2e: [ios]" }),
            r#"{"result":"started","pool":"ci","runId":"fake-1","ms":5120}"#,
        ),
    );
    fake.add_hook_event(
        hook,
        delivery(
            2,
            serde_json::json!({ "object_kind": "pipeline", "object_attributes": { "id": 901 } }),
            r#"{"result":"ignored","reason":"not_job_event"}"#,
        ),
    );
    let local = local.display().to_string();
    let status = ["--config", &config, "--local", &local, "status"];
    let (code, out) = run(env(Some(fake.clone())), &status).await;
    assert_eq!(code, 0, "{out}");
    let (members, out): (Vec<&str>, Vec<&str>) = out
        .split_inclusive('\n')
        .partition(|l| l.starts_with("member "));
    assert_eq!(members.len(), 1);
    assert!(members[0].starts_with("member 1 joined 20"), "{members:?}");
    assert_eq!(
        out.concat(),
        format!(
            concat!(
                "dispatcher: https://demo-dispatch.fake.invalid/\n",
                "vm runner ci: 0 live of 4\n",
                "lane build of project 1 (example.invalid/node:24): 0 live of 2\n",
                "hook {hook}: executable\n",
                "delivery 2026-10-25T12:00:01Z e2e: [ios] 200 started ci 5120\n",
                "delivery 2026-10-25T12:00:02Z pipeline 901 200 ignored - -\n",
            ),
            hook = hook
        )
    );
    let (code, out) = run(
        env(Some(fake.clone())),
        &[&status[..4], &["--skip-gitlab", "status"]].concat(),
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("hook"), "--skip-gitlab: {out}");

    fake.set_jobs(
        1,
        vec![LintJob {
            name: "unit".into(),
            stage: Some("test".into()),
            tag_list: vec!["ci".into()],
        }],
    );
    let check = ["--config", &config, "--local", &local, "check"];
    let (code, out) = run(env(Some(fake.clone())), &check).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out,
        "project 1: every job matches its lanes and the VM runner\n"
    );
    fake.disable_hook(hook, "temporarily_disabled", Some("2026-10-25T12:10:00Z"));
    let (code, out) = run(env(Some(fake.clone())), &check).await;
    assert_eq!(code, 1, "{out}");
    assert_eq!(
        out,
        concat!(
            "project 1: webhook temporarily_disabled until 2026-10-25T12:10:00Z: run ciburst-setup to enable it again\n",
            "project 1: every job matches its lanes and the VM runner\n",
        )
    );
    let (code, out) = run(env(Some(fake.clone())), &status).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains(&format!(
            "hook {hook}: temporarily_disabled until 2026-10-25T12:10:00Z\n"
        )),
        "{out}"
    );
    gitlab.delete_hook(1, hook).await.unwrap();
    let (code, out) = run(env(Some(fake.clone())), &status).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.ends_with("project 1: no hook to the dispatcher\n"),
        "{out}"
    );
}

#[tokio::test]
async fn status_needs_no_gitlab_token() {
    let dir = tempfile::tempdir().unwrap();
    let (config, local) = paths(dir.path());
    std::fs::write(&config, FAKE_CONFIG).unwrap();
    let fake = Arc::new(FakeGitlab::new());
    let local = local.display().to_string();
    let (code, out) = run(
        env(Some(fake.clone())),
        &["--config", &config, "--local", &local, "status"],
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("dispatcher: not deployed"), "{out}");
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn a_forbidden_runner_creation_names_scope_and_role() {
    let dir = tempfile::tempdir().unwrap();
    let (config, local) = paths(dir.path());
    std::fs::write(&config, FAKE_CONFIG).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    let fake = Arc::new(FakeGitlab::new());
    fake.forbid_runner_creation();
    let mut env = env(Some(fake));
    env.vars
        .insert("GITLAB_TOKEN".to_string(), FAKE_TOKEN.to_string());
    let local = local.display().to_string();
    let (code, out) = run(env, &["--config", &config, "--local", &local, "apply"]).await;
    assert_eq!(code, 1);
    assert!(out.starts_with("gitlab POST /user/runners: 403"), "{out}");
    assert!(
        out.ends_with("; the token needs the scope create_runner and the Owner role in project 1"),
        "{out}"
    );
}

#[tokio::test]
async fn missing_files_and_variables_are_named() {
    let dir = tempfile::tempdir().unwrap();
    let (config, local) = paths(dir.path());
    let local_arg = local.display().to_string();
    let missing = dir.path().join("nope.json");
    let (code, out) = run(
        env(None),
        &["--config", &missing.display().to_string(), "plan"],
    )
    .await;
    assert_eq!(code, 1);
    assert_eq!(out, format!("config file {} not found", missing.display()));

    std::fs::write(&config, SCALEWAY_CONFIG).unwrap();
    let plan = [
        "--config",
        &config,
        "--local",
        &local_arg,
        "--skip-gitlab",
        "plan",
    ];
    let (code, out) = run(env(None), &plan).await;
    assert_eq!(code, 1);
    let scaleway_env = local.join("scaleway.env");
    assert_eq!(
        out,
        format!(
            "{} not found; it holds SCW_SECRET_KEY, SCW_DEFAULT_ORGANIZATION_ID",
            scaleway_env.display()
        )
    );
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(&scaleway_env, "SCW_SECRET_KEY=s\n").unwrap();
    let (code, out) = run(env(None), &plan).await;
    assert_eq!(code, 1);
    assert_eq!(
        out,
        format!(
            "SCW_DEFAULT_ORGANIZATION_ID missing in {}",
            scaleway_env.display()
        )
    );

    std::fs::write(&config, FAKE_CONFIG).unwrap();
    let (code, out) = run(
        env(None),
        &["--config", &config, "--local", &local_arg, "plan"],
    )
    .await;
    assert_eq!(code, 1);
    assert_eq!(
        out,
        format!(
            "{} not found; it holds GITLAB_TOKEN",
            local.join("gitlab.env").display()
        )
    );
}

#[test]
fn the_agent_comes_from_flag_variable_local_then_next_to_the_executable() {
    let dir = tempfile::tempdir().unwrap();
    let touch = |path: &Path| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"agent").unwrap();
        path.to_path_buf()
    };
    let flag = touch(&dir.path().join("flag").join("agent-binary"));
    let variable = touch(&dir.path().join("variable").join("agent-binary"));
    let local = dir.path().join("local");
    let in_local = touch(&local.join("ciburst-vm-agent"));
    let exe = dir.path().join("bin").join("ciburst");
    let next_to_exe = touch(&dir.path().join("bin").join("ciburst-vm-agent"));
    let vars = BTreeMap::from([("CIBURST_AGENT".to_string(), variable.display().to_string())]);
    let none = BTreeMap::new();
    let empty_local = dir.path().join("empty");

    let resolve = |flag: Option<&Path>, vars, local: Option<&Path>, exe: Option<&Path>| {
        agent_path(flag, vars, local, exe).unwrap()
    };
    assert_eq!(
        resolve(Some(&flag), &vars, Some(&local), Some(&exe)),
        Some(flag.clone())
    );
    assert_eq!(
        resolve(None, &vars, Some(&local), Some(&exe)),
        Some(variable.clone())
    );
    assert_eq!(
        resolve(None, &none, Some(&local), Some(&exe)),
        Some(in_local)
    );
    assert_eq!(
        resolve(None, &none, Some(&empty_local), Some(&exe)),
        Some(next_to_exe.clone())
    );
    assert_eq!(
        resolve(None, &none, None, Some(&exe)),
        Some(next_to_exe),
        "pipeline mode has no local directory"
    );
    assert_eq!(resolve(None, &none, Some(&empty_local), None), None);

    let gone = dir.path().join("gone");
    let error = agent_path(Some(&gone), &none, Some(&local), Some(&exe)).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("agent binary {} not found", gone.display())
    );
    let vars = BTreeMap::from([("CIBURST_AGENT".to_string(), gone.display().to_string())]);
    assert!(agent_path(None, &vars, Some(&local), Some(&exe)).is_err());
}
