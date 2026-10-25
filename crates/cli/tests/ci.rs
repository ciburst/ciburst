//! Pipeline mode, `ciburst --ci` (M5 rules R1 and R2).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ciburst::Env;
use ciburst_core::Result;
use ciburst_core::check::LintJob;
use ciburst_core::http::{Headers, HttpClient, Request, Response};
use ciburst_core::log::Log;
use ciburst_core::plan::OUTPUTS_SCOPE;
use ciburst_core::provision::keys;
use ciburst_core::testing::gitlab_fake::{FAKE_TOKEN, FakeGitlab, FakeVariable};
use serde_json::json;

const PROJECT: u64 = 5;

/// A config file that keeps the defaults but runs on the fake modules.
const FAKE_MODULES: &str = r#"{ "provider": { "type": "fake" }, "host": { "type": "fake" } }"#;

fn job_vars(protected: &str) -> BTreeMap<String, String> {
    [
        ("CI_PROJECT_ID", PROJECT.to_string()),
        ("CI_SERVER_URL", "https://gitlab.example".to_string()),
        ("CI_COMMIT_REF_PROTECTED", protected.to_string()),
        ("CIBURST_GITLAB_TOKEN", FAKE_TOKEN.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// Sends GitLab calls to the fake, answers every other call 404, and records all of them.
struct Router {
    gitlab: Arc<FakeGitlab>,
    requests: Mutex<Vec<Request>>,
}

#[async_trait]
impl HttpClient for Router {
    async fn send(&self, request: Request) -> Result<Response> {
        Ok(self.send_with_headers(request).await?.0)
    }

    async fn send_with_headers(&self, request: Request) -> Result<(Response, Headers)> {
        self.requests.lock().unwrap().push(request.clone());
        if request.url.contains("/api/v4/") {
            return self.gitlab.send_with_headers(request).await;
        }
        let missing = json!({ "message": "not found", "type": "not_found" });
        Ok((Response::json_body(404, &missing), Headers::new()))
    }
}

struct Run {
    code: i32,
    out: String,
    lines: Vec<String>,
}

async fn ciburst(vars: BTreeMap<String, String>, http: Arc<dyn HttpClient>, args: &[&str]) -> Run {
    let (log, lines) = Log::memory();
    let env = Env {
        vars,
        http: Some(http),
        exe: None,
        log,
    };
    let mut all = vec!["ciburst".to_string(), "--ci".to_string()];
    all.extend(args.iter().map(|s| s.to_string()));
    let mut out = Vec::new();
    let code = ciburst::run_with_env(all, env, &mut out)
        .await
        .unwrap_or_else(|error| {
            out.extend_from_slice(error.to_string().as_bytes());
            1
        });
    let lines = lines.lock().unwrap().clone();
    Run {
        code,
        out: String::from_utf8(out).unwrap(),
        lines,
    }
}

fn config_file(dir: &Path, text: &str) -> String {
    let path = dir.join("ciburst.json");
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

#[tokio::test]
async fn refuses_an_unprotected_ref() {
    let fake = Arc::new(FakeGitlab::new());
    for protected in ["false", ""] {
        let run = ciburst(job_vars(protected), fake.clone(), &["plan"]).await;
        assert_eq!(run.code, 1);
        assert_eq!(
            run.out,
            "CI_COMMIT_REF_PROTECTED is not true: run ciburst-setup on a protected branch"
        );
    }
    let mut vars = job_vars("true");
    vars.remove("CI_COMMIT_REF_PROTECTED");
    let run = ciburst(vars, fake.clone(), &["apply"]).await;
    assert!(run.out.contains("run ciburst-setup on a protected branch"));
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn names_a_missing_input() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeGitlab::new());
    for missing in ["CI_PROJECT_ID", "CI_SERVER_URL"] {
        let mut vars = job_vars("true");
        vars.remove(missing);
        let run = ciburst(vars, fake.clone(), &["plan"]).await;
        assert_eq!((run.code, run.out), (1, format!("{missing} missing")));
    }

    let absent = dir.path().join("absent.json").display().to_string();
    let mut vars = job_vars("true");
    vars.insert("CIBURST_SCW_ACCESS_KEY".into(), "SCWACCESS".into());
    let run = ciburst(vars.clone(), fake.clone(), &["--config", &absent, "plan"]).await;
    assert_eq!(run.out, "CIBURST_SCW_SECRET_KEY missing");
    vars.insert("CIBURST_SCW_SECRET_KEY".into(), "secret".into());
    let run = ciburst(vars, fake.clone(), &["--config", &absent, "plan"]).await;
    assert_eq!(run.out, "CIBURST_SCW_ORGANIZATION_ID missing");

    let fake_modules = config_file(dir.path(), FAKE_MODULES);
    let mut vars = job_vars("true");
    vars.remove("CIBURST_GITLAB_TOKEN");
    vars.insert("GITLAB_TOKEN".into(), FAKE_TOKEN.into());
    let run = ciburst(vars, fake.clone(), &["--config", &fake_modules, "plan"]).await;
    assert_eq!(run.out, "CIBURST_GITLAB_TOKEN missing");

    let hetzner = config_file(
        dir.path(),
        r#"{ "provider": { "type": "fake" }, "host": { "type": "fake" },
             "providers": { "hetzner": {} } }"#,
    );
    let mut vars = job_vars("true");
    vars.insert("HCLOUD_TOKEN".into(), "a-local-token".into());
    let run = ciburst(vars, fake.clone(), &["--config", &hetzner, "plan"]).await;
    assert_eq!(run.out, "CIBURST_HCLOUD_TOKEN missing");

    let run = ciburst(job_vars("true"), fake.clone(), &["--skip-gitlab", "plan"]).await;
    assert_eq!(
        run.out,
        "--skip-gitlab does not combine with --ci: the outputs live in project variables"
    );
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn never_reads_scw_gitlab_token_or_local() {
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("scaleway.env"),
        "SCW_SECRET_KEY=junk-file-secret\nSCW_DEFAULT_ORGANIZATION_ID=junk-file-org\n",
    )
    .unwrap();
    std::fs::write(local.join("gitlab.env"), "GITLAB_TOKEN=junk-file-token\n").unwrap();
    let mut vars = job_vars("true");
    for (key, value) in [
        ("CIBURST_SCW_ACCESS_KEY", "SCWCIACCESSKEY0000000"),
        ("CIBURST_SCW_SECRET_KEY", "ci-secret-key"),
        ("CIBURST_SCW_ORGANIZATION_ID", "ci-organization"),
        ("SCW_ACCESS_KEY", "junk-access"),
        ("SCW_SECRET_KEY", "junk-secret"),
        ("SCW_DEFAULT_ORGANIZATION_ID", "junk-organization"),
        ("SCW_DEFAULT_PROJECT_ID", "junk-project"),
        ("GITLAB_TOKEN", "junk-token"),
    ] {
        vars.insert(key.to_string(), value.to_string());
    }
    let fake = Arc::new(FakeGitlab::new());
    fake.set_jobs(
        PROJECT,
        vec![LintJob {
            name: "unit".into(),
            stage: Some("test".into()),
            tag_list: vec!["vm".into()],
        }],
    );
    let router = Arc::new(Router {
        gitlab: fake.clone(),
        requests: Mutex::new(Vec::new()),
    });
    let absent = dir.path().join("absent.json").display().to_string();
    let local = local.display().to_string();
    let args = ["--config", &absent, "--local", &local];

    let run = ciburst(
        vars.clone(),
        router.clone(),
        &[&args[..], &["status"]].concat(),
    )
    .await;
    assert_eq!(run.code, 1, "the Scaleway fake answers 404: {}", run.out);
    let run = ciburst(vars, router.clone(), &[&args[..], &["check"]].concat()).await;
    assert_eq!(run.code, 0, "{}", run.out);
    assert_eq!(run.out, "project 5: every job matches the stage map\n");

    let requests = router.requests.lock().unwrap();
    let scaleway: Vec<&Request> = requests
        .iter()
        .filter(|r| r.url.starts_with("https://api.scaleway.com/"))
        .collect();
    assert!(!scaleway.is_empty());
    assert!(
        scaleway
            .iter()
            .all(|r| r.header_value("X-Auth-Token") == Some("ci-secret-key"))
    );
    for request in requests.iter() {
        let seen = format!(
            "{} {:?} {}",
            request.url,
            request.headers,
            String::from_utf8_lossy(request.body.as_deref().unwrap_or_default())
        );
        assert!(!seen.contains("junk"), "{seen}");
    }
}

#[tokio::test]
async fn apply_stores_the_outputs_and_destroy_clears_them() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), FAKE_MODULES);
    let fake = Arc::new(FakeGitlab::new());
    let input = FakeVariable {
        environment_scope: OUTPUTS_SCOPE.into(),
        protected: true,
        ..FakeVariable::plain("CIBURST_GITLAB_TOKEN", FAKE_TOKEN)
    };
    fake.add_variable(PROJECT, input.clone());

    let run = ciburst(
        job_vars("true"),
        fake.clone(),
        &["--config", &config, "apply"],
    )
    .await;
    assert_eq!(run.code, 0, "{}", run.out);
    assert!(
        run.out
            .contains("outputs: variables of project 5, environment scope ciburst"),
        "{}",
        run.out
    );
    assert_eq!(fake.runner_count(), 1);
    assert_eq!(fake.hooks()[0].2, "https://ci-5-dispatch.fake.invalid/");

    let stored = fake.variables(PROJECT);
    let job_env: BTreeMap<String, String> = stored
        .iter()
        .filter(|v| v.environment_scope == OUTPUTS_SCOPE)
        .map(|v| (v.key.clone(), v.value.clone()))
        .collect();
    for key in [
        keys::HOOK_TOKEN,
        keys::VM_SECRET,
        keys::SWEEP_TOKEN,
        keys::DISPATCH_SECRET_KEY,
        keys::CACHE_SECRET_KEY,
    ] {
        let variable = stored.iter().find(|v| v.key == key).unwrap();
        assert!(variable.protected && variable.raw, "{variable:?}");
        assert!(variable.masked && variable.hidden, "{variable:?}");
    }
    assert!(job_env[&keys::runner_token("vm")].starts_with("glrt-"));
    let runner_id = keys::runner_id("vm");
    for key in [keys::DISPATCH_URL, runner_id.as_str()] {
        let variable = stored.iter().find(|v| v.key == key).unwrap();
        assert!(variable.protected && !variable.masked, "{variable:?}");
    }
    assert_eq!(
        run.lines
            .iter()
            .filter(|l| l.contains("output_unmasked"))
            .count(),
        0
    );

    let mut vars = job_vars("true");
    vars.extend(job_env);
    let run = ciburst(
        vars,
        fake.clone(),
        &["--config", &config, "destroy", "--yes", "ci-5"],
    )
    .await;
    assert_eq!(run.code, 0, "{}", run.out);
    assert_eq!(run.out, "destroyed ci-5\n");
    assert_eq!(fake.variables(PROJECT), [input], "the input stays");
    assert_eq!(fake.runner_count(), 0);
    assert!(fake.hooks().is_empty());
}

#[tokio::test]
async fn a_missing_file_gives_the_pure_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent.json").display().to_string();
    let run = ciburst(
        job_vars("true"),
        Arc::new(FakeGitlab::new()),
        &["--config", &absent, "destroy"],
    )
    .await;
    assert_eq!(run.code, 2);
    assert_eq!(
        run.out,
        "destroy removes every resource of ci-5; confirm with --yes ci-5\n"
    );
}
