use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::sync::Arc;

use ciburst_core::config::{Config, parse_config};
use ciburst_core::log::Log;
use ciburst_core::types::{Action, Host, KeyPair, Secrets, Step};
use ciburst_scaleway::client::{Credentials, ScalewayClient};
use ciburst_scaleway::host::{ScalewayFunction, embedded_code, embedded_code_digest, vars};
use ciburst_scaleway::testing::functions_api::FakeFunctions;

const FUNCTION: &str = "demo-dispatch";
const PROJECT: &str = "11111111-1111-4111-8111-111111111111";

fn config(pools: &str) -> Arc<Config> {
    Arc::new(
        parse_config(&format!(
            r#"{{
              "name": "demo",
              "gitlab": {{ "url": "https://gitlab.example", "projectIds": [7] }},
              "host": {{ "type": "scaleway-function", "region": "fr-par" }},
              "provider": {{ "type": "scaleway-jobs", "region": "fr-par" }},
              "defaultPool": "ci",
              "pools": [{pools}],
              "cache": {{ "type": "s3" }}
            }}"#
        ))
        .unwrap(),
    )
}

fn two_pools() -> Arc<Config> {
    config(
        r#"{ "tag": "ci", "image": "img/node" },
           { "tag": "build-x", "image": "img/kaniko", "stages": ["build"] }"#,
    )
}

fn secrets() -> Secrets {
    Secrets {
        hook_token: Some("hook-secret".into()),
        runner_tokens: BTreeMap::from([
            ("ci".to_string(), "glrt-ci".to_string()),
            ("build-x".to_string(), "glrt-build".to_string()),
        ]),
        dispatch: Some(KeyPair {
            access_key: "SCWDISPATCH".into(),
            secret_key: "dispatch-secret".into(),
        }),
        cache: Some(KeyPair {
            access_key: "SCWCACHE".into(),
            secret_key: "cache-secret".into(),
        }),
    }
}

fn client(fake: &Arc<FakeFunctions>, project: Option<&str>) -> Arc<ScalewayClient> {
    Arc::new(ScalewayClient::new(
        fake.clone(),
        Credentials {
            secret_key: "owner-secret".into(),
            access_key: None,
            organization_id: "22222222-2222-4222-8222-222222222222".into(),
            default_project_id: project.map(String::from),
        },
        "fr-par",
    ))
}

fn host(config: Arc<Config>, fake: &Arc<FakeFunctions>) -> ScalewayFunction {
    ScalewayFunction::new(config, client(fake, Some(PROJECT)), Log::silent())
}

fn actions(steps: &[Step]) -> Vec<(&str, Action)> {
    steps.iter().map(|s| (s.id.as_str(), s.action)).collect()
}

#[tokio::test]
async fn plans_applies_and_converges() {
    let fake = Arc::new(FakeFunctions::new());
    let host = host(two_pools(), &fake);
    assert_eq!(host.url().await.unwrap(), None);

    let plan = host.plan(&secrets()).await.unwrap();
    assert_eq!(
        actions(&plan),
        [
            ("namespace", Action::Create),
            ("function", Action::Create),
            ("code", Action::Create)
        ]
    );
    assert_eq!(fake.namespace_count(), 0, "plan is pure");

    let outputs = host.apply(&plan, &secrets()).await.unwrap();
    let url = outputs["CIBURST_DISPATCH_URL"].clone();
    assert!(url.starts_with("https://demo"), "{url}");
    assert!(url.ends_with(".functions.fnc.fr-par.scw.cloud/"), "{url}");
    assert_eq!(host.url().await.unwrap().as_deref(), Some(url.as_str()));

    let function = fake.function(FUNCTION).unwrap();
    assert_eq!(function["status"], "ready");
    assert_eq!(function["runtime"], "rust196");
    assert_eq!(function["handler"], "handle");
    assert_eq!(function["privacy"], "public");
    assert_eq!(function["min_scale"], 0);
    assert_eq!(function["max_scale"], 5);
    assert_eq!(function["memory_limit"], 128);
    assert_eq!(function["timeout"], "30s");
    assert_eq!(function["http_option"], "redirected");
    assert!(
        function["description"]
            .as_str()
            .unwrap()
            .starts_with("ciburst:")
    );
    let env = function["environment_variables"].as_object().unwrap();
    let mut env_keys: Vec<&str> = env.keys().map(String::as_str).collect();
    env_keys.sort();
    assert_eq!(
        env_keys,
        [
            vars::CODE_DIGEST,
            vars::CONFIG,
            vars::ORGANIZATION_ID,
            vars::PROJECT_ID,
            vars::REGION
        ]
    );
    assert_eq!(env[vars::PROJECT_ID], PROJECT);
    assert_eq!(env[vars::REGION], "fr-par");
    assert_eq!(env[vars::CODE_DIGEST], embedded_code_digest());
    let embedded_config = parse_config(env[vars::CONFIG].as_str().unwrap()).unwrap();
    assert_eq!(embedded_config, *two_pools());
    assert_eq!(
        fake.secret_keys(FUNCTION),
        [
            "CACHE_S3_ACCESS_KEY",
            "CACHE_S3_SECRET_KEY",
            "CIBURST_DISPATCH_KEY",
            "HOOK_TOKEN",
            "RUNNER_TOKEN_BUILD_X",
            "RUNNER_TOKEN_CI"
        ]
    );
    assert_eq!(
        fake.secret(FUNCTION, "CIBURST_DISPATCH_KEY").as_deref(),
        Some("dispatch-secret")
    );
    assert!(
        !function.to_string().contains("glrt-"),
        "secrets come back hashed"
    );
    assert_eq!(
        fake.deployed_code(FUNCTION).as_deref(),
        Some(embedded_code())
    );
    assert_eq!(fake.deploy_count(FUNCTION), 1);

    let upload = fake
        .calls()
        .into_iter()
        .find(|c| c.method == "PUT")
        .unwrap();
    assert_eq!(upload.host, "s3.fr-par.scw.cloud");

    let again = host.plan(&secrets()).await.unwrap();
    assert!(again.iter().all(Step::is_noop), "{again:?}");
    let outputs = host.apply(&again, &secrets()).await.unwrap();
    assert_eq!(outputs["CIBURST_DISPATCH_URL"], url);
    assert_eq!(
        fake.deploy_count(FUNCTION),
        1,
        "a noop apply deploys nothing"
    );
    assert_eq!(fake.namespace_count(), 1);
    assert_eq!(fake.function_count(), 1);
}

#[tokio::test]
async fn a_changed_secret_updates_the_function_only() {
    let fake = Arc::new(FakeFunctions::new());
    let host = host(two_pools(), &fake);
    let plan = host.plan(&secrets()).await.unwrap();
    host.apply(&plan, &secrets()).await.unwrap();

    let mut rotated = secrets();
    rotated.hook_token = Some("hook-rotated".into());
    let plan = host.plan(&rotated).await.unwrap();
    assert_eq!(
        actions(&plan),
        [
            ("namespace", Action::Noop),
            ("function", Action::Update),
            ("code", Action::Noop)
        ]
    );
    host.apply(&plan, &rotated).await.unwrap();
    assert_eq!(
        fake.secret(FUNCTION, "HOOK_TOKEN").as_deref(),
        Some("hook-rotated")
    );
    assert_eq!(
        fake.function(FUNCTION).unwrap()["environment_variables"][vars::CODE_DIGEST],
        embedded_code_digest(),
        "an update keeps the code digest"
    );
    assert_eq!(fake.deploy_count(FUNCTION), 2);
    assert!(host.plan(&rotated).await.unwrap().iter().all(Step::is_noop));
}

#[tokio::test]
async fn a_removed_pool_removes_its_secret() {
    let fake = Arc::new(FakeFunctions::new());
    let first = host(two_pools(), &fake);
    first
        .apply(&first.plan(&secrets()).await.unwrap(), &secrets())
        .await
        .unwrap();
    let one_pool = host(config(r#"{ "tag": "ci", "image": "img/node" }"#), &fake);
    let plan = one_pool.plan(&secrets()).await.unwrap();
    assert_eq!(plan[1].action, Action::Update);
    one_pool.apply(&plan, &secrets()).await.unwrap();
    assert!(
        !fake
            .secret_keys(FUNCTION)
            .contains(&"RUNNER_TOKEN_BUILD_X".to_string())
    );
    assert!(
        one_pool
            .plan(&secrets())
            .await
            .unwrap()
            .iter()
            .all(Step::is_noop)
    );
}

#[tokio::test]
async fn new_code_and_failed_builds_redeploy() {
    let fake = Arc::new(FakeFunctions::new());
    let host = host(two_pools(), &fake);
    host.apply(&host.plan(&secrets()).await.unwrap(), &secrets())
        .await
        .unwrap();

    fake.set_env(FUNCTION, vars::CODE_DIGEST, "older");
    let plan = host.plan(&secrets()).await.unwrap();
    assert_eq!(
        actions(&plan),
        [
            ("namespace", Action::Noop),
            ("function", Action::Noop),
            ("code", Action::Update)
        ]
    );
    fake.fail_next_build();
    let error = host.apply(&plan, &secrets()).await.unwrap_err().to_string();
    assert!(error.contains("could not compile"), "{error}");

    let plan = host.plan(&secrets()).await.unwrap();
    assert_eq!(plan[2].action, Action::Update);
    assert_eq!(plan[2].detail.as_deref(), Some("last deploy failed"));
    host.apply(&plan, &secrets()).await.unwrap();
    assert_eq!(fake.function(FUNCTION).unwrap()["status"], "ready");
    assert!(
        host.plan(&secrets())
            .await
            .unwrap()
            .iter()
            .all(Step::is_noop)
    );
}

#[tokio::test]
async fn destroy_empties_the_api() {
    let fake = Arc::new(FakeFunctions::new());
    let host = host(two_pools(), &fake);
    host.destroy().await.unwrap();
    host.apply(&host.plan(&secrets()).await.unwrap(), &secrets())
        .await
        .unwrap();
    assert!(host.url().await.unwrap().is_some());
    host.destroy().await.unwrap();
    assert_eq!(fake.namespace_count(), 0);
    assert_eq!(fake.function_count(), 0);
    assert_eq!(host.url().await.unwrap(), None);
    host.destroy().await.unwrap();
}

#[tokio::test]
async fn finds_the_project_by_name_and_needs_it_to_apply() {
    let fake = Arc::new(FakeFunctions::new());
    let host = ScalewayFunction::new(two_pools(), client(&fake, None), Log::silent());
    let plan = host.plan(&secrets()).await.unwrap();
    assert!(plan.iter().all(|s| s.action == Action::Create));
    assert_eq!(host.url().await.unwrap(), None);
    let error = host.apply(&plan, &secrets()).await.unwrap_err().to_string();
    assert!(error.contains("project demo not found"), "{error}");

    fake.add_project("33333333-3333-4333-8333-333333333333", "demo");
    host.apply(&plan, &secrets()).await.unwrap();
    let env = fake.function(FUNCTION).unwrap()["environment_variables"].clone();
    assert_eq!(
        env[vars::PROJECT_ID],
        "33333333-3333-4333-8333-333333333333"
    );
}

#[tokio::test]
async fn unknown_steps_and_missing_tokens_fail() {
    let fake = Arc::new(FakeFunctions::new());
    let host = host(two_pools(), &fake);
    let error = host
        .apply(&[Step::new("bucket", Action::Create, "x")], &secrets())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown step bucket"));

    let anonymous = ScalewayFunction::new(
        two_pools(),
        Arc::new(ScalewayClient::new(
            fake.clone(),
            Credentials {
                default_project_id: Some(PROJECT.into()),
                ..Credentials::default()
            },
            "fr-par",
        )),
        Log::silent(),
    );
    let error = anonymous.plan(&secrets()).await.unwrap_err().to_string();
    assert!(error.contains("401"), "{error}");
}

#[test]
fn the_embedded_zip_is_the_function_package() {
    let code = embedded_code();
    assert!(!code.is_empty());
    let mut archive = zip::ZipArchive::new(Cursor::new(code)).unwrap();
    let names: Vec<String> = archive
        .file_names()
        .map(|n| n.unwrap().into_owned())
        .collect();
    for required in [
        "Cargo.toml",
        "Cargo.lock",
        "src/lib.rs",
        "vendor/core/Cargo.toml",
        "vendor/core/src/lib.rs",
        "vendor/scaleway/Cargo.toml",
        "vendor/scaleway/src/lib.rs",
        "vendor/scaleway/src/host.rs",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "{required} in {names:?}"
        );
    }
    assert!(
        !names.iter().any(|n| n.ends_with("build.rs")
            || n.contains("/tests/")
            || n.contains('\\')
            || n.starts_with("crates/")),
        "{names:?}"
    );

    let mut read = |name: &str| {
        let mut text = String::new();
        archive
            .by_name(name)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text
    };
    let root = read("Cargo.toml");
    assert!(root.contains("name = \"ciburst-dispatch-scaleway\""));
    assert!(root.contains("path = \"vendor/core\""));
    assert!(root.contains("path = \"vendor/scaleway\""));
    assert!(!root.contains("../"));
    assert!(!root.contains("build.rs"));
    assert!(!root.contains("members"));
    assert!(root.contains("\n[workspace]\n"));
    assert!(root.contains("[workspace.package]"));
    assert!(root.contains("rust-version = \"1.96\""));
    assert!(root.contains("rust-version.workspace = true"));
    assert!(root.contains("[workspace.dependencies]"));
    assert!(!root.contains("[dev-dependencies]"));

    let scaleway = read("vendor/scaleway/Cargo.toml");
    assert!(scaleway.contains("path = \"../core\""));
    assert!(!scaleway.contains("[build-dependencies]"));
    assert!(!scaleway.contains("[dev-dependencies]"));
    assert!(!scaleway.contains("zip"));
    let core = read("vendor/core/Cargo.toml");
    assert!(!core.contains("[dev-dependencies]"));

    assert!(read("src/lib.rs").contains("pub async fn handle("));
    assert!(read("Cargo.lock").contains("name = \"ciburst-dispatch-scaleway\""));
}

#[test]
fn the_zip_digest_is_stable() {
    assert_eq!(embedded_code_digest().len(), 64);
    assert_eq!(embedded_code_digest(), embedded_code_digest());
}
