use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ciburst::{Env, agent_path};
use ciburst_core::check::LintJob;
use ciburst_core::config::{Config, parse_config};
use ciburst_core::http::HttpClient;
use ciburst_core::log::Log;
use ciburst_core::testing::gitlab_fake::{FAKE_TOKEN, FakeGitlab};

const FAKE_CONFIG: &str = r#"{
  "name": "demo",
  "gitlab": { "url": "https://gitlab.com", "projectIds": [1] },
  "host": { "type": "fake" },
  "provider": { "type": "fake" },
  "defaultPool": "ci",
  "pools": [
    { "tag": "ci", "image": "example.invalid/node:24", "stages": ["lint", "test"] },
    { "tag": "build", "image": "example.invalid/node:24", "stages": ["build"], "maxRuns": 2 }
  ],
  "cache": { "type": "s3" }
}"#;

const SCALEWAY_CONFIG: &str = r#"{
  "name": "demo",
  "gitlab": { "url": "https://gitlab.com", "projectIds": [1] },
  "host": { "type": "scaleway-function", "region": "fr-par" },
  "provider": { "type": "scaleway", "region": "fr-par" },
  "defaultPool": "vm",
  "pools": [{ "tag": "vm", "image": "ubuntu:24.04", "runtime": "instances" }],
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
    assert!(out.contains("create  job definition demo-build"), "{out}");
    assert!(
        out.contains("7 to create, 0 to update, 0 to delete, 0 unchanged"),
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
        out.contains("0 to create, 0 to update, 0 to delete, 7 unchanged"),
        "{out}"
    );

    let (code, out) = ciburst(dir.path(), &["status"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("dispatcher: https://demo-dispatch.fake.invalid/"),
        "{out}"
    );
    assert!(out.contains("pool build: 0 live of 2"), "{out}");

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
async fn the_dry_run_config_shows_the_default_vm_pool() {
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
        out.contains("6 to create, 0 to update, 0 to delete, 0 unchanged"),
        "{out}"
    );
    let config =
        parse_config(&std::fs::read_to_string(repo_file("test/config.fake.json")).unwrap())
            .unwrap();
    assert_eq!(
        config.pools,
        Config::ci_defaults(1, "https://gitlab.com").pools
    );
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
        FAKE_CONFIG.replace(r#""defaultPool": "ci""#, r#""defaultPool": "nope""#),
    )
    .unwrap();
    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 1);
    assert!(out.contains("defaultPool: nope is not a pool"), "{out}");

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
            r#""providers": { "vm": {} }"#,
        ),
    )
    .unwrap();
    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 1);
    assert!(out.contains("providers.vm: reserved name"), "{out}");
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
    assert!(out.contains("hetzner.env"), "{out}");
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
    assert_eq!(out, "project 1: every job matches the stage map\n");
    assert!(fake.calls().iter().all(|c| c.path == "/projects/1/ci/lint"));
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

    std::fs::write(
        &config,
        FAKE_CONFIG.replace(
            r#""cache": { "type": "s3" }"#,
            r#""cache": { "type": "s3" }, "providers": { "hetzner": {} }"#,
        ),
    )
    .unwrap();
    let (code, out) = run(env(None), &plan).await;
    assert_eq!(code, 1);
    assert_eq!(
        out,
        format!(
            "{} not found; it holds HCLOUD_TOKEN",
            local.join("hetzner.env").display()
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
