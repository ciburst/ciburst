//! M6.3 rules R3 and R4: `invite` in the host, `join` and `leave` in a member, against the
//! GitLab fake and an in-process dispatcher with fleet state.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ciburst::Env;
use ciburst_core::Result;
use ciburst_core::config::parse_config;
use ciburst_core::dispatch::Dispatcher;
use ciburst_core::fake::FakeProvider;
use ciburst_core::fleet::{FleetStore, MemoryFleetStore};
use ciburst_core::http::{Headers, HttpClient, Request, Response};
use ciburst_core::invite;
use ciburst_core::log::Log;
use ciburst_core::store::MemoryStore;
use ciburst_core::testing::gitlab_fake::{FAKE_TOKEN, FakeGitlab};
use ciburst_core::types::{Provider, Secrets};
use serde_json::{Value, json};

const HOST: u64 = 5;
const MEMBER: u64 = 8;
const HOOK: &str = "hook-token";
const URL: &str = "https://dispatch.example";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// GitLab calls go to the fake, calls to the dispatcher's URL to the dispatcher.
struct Router {
    gitlab: Arc<FakeGitlab>,
    dispatcher: Dispatcher,
}

#[async_trait]
impl HttpClient for Router {
    async fn send(&self, request: Request) -> Result<Response> {
        Ok(self.send_with_headers(request).await?.0)
    }

    async fn send_with_headers(&self, request: Request) -> Result<(Response, Headers)> {
        if let Some(path) = request.url.strip_prefix(URL) {
            let answer = self
                .dispatcher
                .handle_request(
                    &request.method,
                    path,
                    &request.headers,
                    request.body.as_deref().unwrap_or(b""),
                    now(),
                )
                .await;
            let response = if answer.body.is_null() {
                Response::empty(answer.status)
            } else {
                Response::json_body(answer.status, &answer.body)
            };
            return Ok((response, Headers::new()));
        }
        self.gitlab.send_with_headers(request).await
    }
}

struct Setup {
    gitlab: Arc<FakeGitlab>,
    fleet: Arc<MemoryFleetStore>,
    router: Arc<Router>,
    dir: tempfile::TempDir,
}

async fn setup() -> Setup {
    let config = Arc::new(
        parse_config(&format!(
            r#"{{
              "name": "demo",
              "gitlab": {{ "url": "https://gitlab.example", "projectIds": [{HOST}] }},
              "host": {{ "type": "fake" }}, "provider": {{ "type": "fake" }},
              "defaultPool": "vm",
              "pools": [
                {{ "tag": "vm", "image": "img/node", "runtime": "instances" }},
                {{ "tag": "node", "image": "node:24", "jobs": ["lint"], "runtime": "jobs" }}
              ],
              "cache": {{ "type": "s3" }}
            }}"#
        ))
        .unwrap(),
    );
    let provider = FakeProvider::new(config.clone(), Arc::new(MemoryStore::new()));
    provider
        .apply(&provider.plan(&[]).await.unwrap())
        .await
        .unwrap();
    let fleet = Arc::new(MemoryFleetStore::new());
    let secrets = Secrets {
        hook_token: Some(HOOK.into()),
        runner_tokens: BTreeMap::from([
            ("vm".to_string(), "glrt-host-vm".to_string()),
            ("node".to_string(), "glrt-host-node".to_string()),
        ]),
        cache: Some(ciburst_core::types::KeyPair {
            access_key: "AK".into(),
            secret_key: "SK".into(),
        }),
        vm_secret: Some("vm-secret".into()),
        ..Secrets::default()
    };
    let dispatcher = Dispatcher::new(config, Arc::new(provider), Arc::new(secrets), Log::silent())
        .with_fleet(Some(fleet.clone() as Arc<dyn FleetStore>));
    let gitlab = Arc::new(FakeGitlab::new());
    Setup {
        gitlab: gitlab.clone(),
        fleet,
        router: Arc::new(Router { gitlab, dispatcher }),
        dir: tempfile::tempdir().unwrap(),
    }
}

fn vars(project: u64, extra: &[(&str, &str)]) -> BTreeMap<String, String> {
    let mut vars: BTreeMap<String, String> = [
        ("CI_PROJECT_ID", project.to_string()),
        ("CI_SERVER_URL", "https://gitlab.example".to_string()),
        ("CI_COMMIT_REF_PROTECTED", "true".to_string()),
        ("CIBURST_GITLAB_TOKEN", FAKE_TOKEN.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for (key, value) in extra {
        vars.insert(key.to_string(), value.to_string());
    }
    vars
}

impl Setup {
    async fn run(&self, vars: BTreeMap<String, String>, args: &[&str]) -> (i32, String) {
        let env = Env {
            vars,
            http: Some(self.router.clone()),
            exe: None,
            log: Log::silent(),
        };
        let config = fake_modules(self.dir.path());
        let mut all = vec!["ciburst".to_string(), "--config".to_string(), config];
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

    /// `ciburst-invite` in the host: the code is the output's last line.
    async fn invite(&self, project: &str) -> (i32, String) {
        let vars = vars(
            HOST,
            &[
                ("CIBURST_INVITE", project),
                ("CIBURST_HOOK_TOKEN", HOOK),
                ("CIBURST_DISPATCH_URL", URL),
            ],
        );
        self.run(vars, &["--ci", "invite"]).await
    }

    async fn member(&self, command: &str, code: &str) -> (i32, String) {
        self.run(vars(MEMBER, &[("CIBURST_JOIN", code)]), &["--ci", command])
            .await
    }

    /// The ids and tags of the member project's runners.
    fn runners(&self) -> Vec<(String, Vec<String>)> {
        self.gitlab
            .runner_projects()
            .into_iter()
            .filter(|(_, projects)| projects.contains(&MEMBER))
            .map(|(id, _)| {
                let tags = self.gitlab.runner_tags(id).unwrap();
                (id.to_string(), tags)
            })
            .collect()
    }
}

fn fake_modules(dir: &Path) -> String {
    let path = dir.join("ciburst.json");
    std::fs::write(
        &path,
        r#"{ "provider": { "type": "fake" }, "host": { "type": "fake" } }"#,
    )
    .unwrap();
    path.display().to_string()
}

fn code_of(out: &str) -> String {
    out.lines().last().unwrap().to_string()
}

#[tokio::test]
async fn the_host_invites_a_project_by_id_or_path() {
    let s = setup().await;
    let (code, out) = s.invite("8").await;
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(
        lines[0].starts_with("join code for project 8, valid until ")
            && lines[0].ends_with("; treat it as a secret:"),
        "{out}"
    );
    let invite = invite::verify(HOOK, lines[1], now()).unwrap();
    assert_eq!((invite.project, invite.url.as_str()), (MEMBER, URL));

    s.gitlab.add_project(MEMBER, "other-group/member");
    let (code, out) = s.invite("other-group/member").await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(invite::decode(&code_of(&out)).unwrap().project, MEMBER);

    let (code, out) = s.invite("unknown/project").await;
    assert_eq!(code, 1);
    assert!(
        out.starts_with("project unknown/project not found with this token"),
        "{out}"
    );
    let (code, out) = s.invite("5").await;
    assert_eq!(
        (code, out.as_str()),
        (1, "project 5 is the host of this fleet")
    );
}

#[tokio::test]
async fn a_member_joins_and_leaves_with_one_code() {
    let s = setup().await;
    let code = code_of(&s.invite("8").await.1);
    let (status, out) = s.member("join", &code).await;
    assert_eq!((status, out.as_str()), (0, "joined fleet demo\n"));

    let mut runners = s.runners();
    runners.sort();
    let tags: Vec<Vec<String>> = runners.iter().map(|(_, tags)| tags.clone()).collect();
    assert_eq!(
        tags,
        [
            vec!["vm".to_string(), "node".to_string()],
            vec!["node".to_string()]
        ]
    );
    let hooks = s.gitlab.hooks();
    assert_eq!(hooks.len(), 1);
    let (hook_id, project, url) = hooks[0].clone();
    assert_eq!((project, url.as_str()), (MEMBER, URL));
    let secret = s.gitlab.hook_token(hook_id).unwrap();
    assert_eq!(secret, invite::hook_secret(HOOK, MEMBER));
    assert_eq!(s.gitlab.hook_flags(hook_id), Some((true, false, true)));
    let members = s.fleet.members().await.unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].project_id, MEMBER);
    assert_eq!(members[0].hook_secret, secret);
    assert_eq!(
        members[0].runners.keys().collect::<Vec<_>>(),
        ["node", "vm"]
    );

    let event = json!({
        "object_kind": "build", "build_id": 1, "build_name": "lint", "build_stage": "test",
        "build_status": "pending", "pipeline_id": 2, "project_id": MEMBER
    });
    let answer = s
        .router
        .send(
            Request::new("POST", format!("{URL}/"))
                .header("X-Gitlab-Token", secret.clone())
                .json(&event),
        )
        .await
        .unwrap();
    let body: Value = answer.json().unwrap();
    assert_eq!(body["result"], "started", "{body}");

    let (status, out) = s.member("join", &code).await;
    assert_eq!((status, out.as_str()), (0, "joined fleet demo\n"));
    assert_eq!(s.runners().len(), 2, "joining twice replaces the runners");
    assert_eq!(s.gitlab.hooks().len(), 1);
    let tokens: Vec<String> = s.fleet.members().await.unwrap()[0]
        .runners
        .values()
        .cloned()
        .collect();
    assert_ne!(
        tokens,
        members[0].runners.values().cloned().collect::<Vec<_>>()
    );

    let (status, out) = s.member("leave", &code).await;
    assert_eq!((status, out.as_str()), (0, "left fleet demo\n"));
    assert!(s.runners().is_empty());
    assert!(s.gitlab.hooks().is_empty());
    assert!(s.fleet.members().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_member_refuses_a_code_of_another_project_or_fleet() {
    let s = setup().await;
    let other = invite::issue(HOOK, URL, 9, now());
    let (status, out) = s.member("join", &other).await;
    assert_eq!(status, 1);
    assert_eq!(
        out,
        "CIBURST_JOIN: the code is for project 9, this is project 8"
    );
    let expired = invite::issue(HOOK, URL, MEMBER, now() - invite::TTL_S - 1);
    let (_, out) = s.member("join", &expired).await;
    assert_eq!(
        out,
        "CIBURST_JOIN: an expired join code: ask the host for a new one"
    );
    let forged = invite::issue("another-hook", URL, MEMBER, now());
    let (status, out) = s.member("join", &forged).await;
    assert_eq!(status, 1);
    assert_eq!(
        out,
        "the dispatcher refused the join code on /fleet: expired or of another fleet"
    );
    let (_, out) = s.member("join", "garbage").await;
    assert_eq!(out, "CIBURST_JOIN: not a join code");
    assert!(s.gitlab.runner_count() == 0 && s.gitlab.hooks().is_empty());

    let (_, out) = s
        .run(vars(MEMBER, &[("CIBURST_JOIN", "x")]), &["join"])
        .await;
    assert_eq!(
        out,
        "join and leave run in the member's pipeline: ciburst --ci join"
    );
    let (_, out) = s.run(vars(MEMBER, &[]), &["--ci", "leave"]).await;
    assert_eq!(out, "CIBURST_JOIN missing");
}
