use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::sync::Arc;

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::config::{Config, parse_config};
use ciburst_core::http::{HttpClient, Request, Response};
use ciburst_core::log::Log;
use ciburst_core::types::{Action, Host, KeyPair, Provider, Secrets, Step};
use ciburst_scaleway::client::{Credentials, ScalewayClient};
use ciburst_scaleway::host::{ScalewayFunction, embedded_code, embedded_code_digest, vars};
use ciburst_scaleway::provider::Scaleway;
use ciburst_scaleway::testing::functions_api::FakeFunctions;
use ciburst_scaleway::testing::scaleway_fake::{
    FAKE_ACCESS_KEY, FAKE_REGION, ScalewayFake, fake_uuid,
};
use ciburst_scaleway::testing::{RUNNER_FIXTURE_URL, agent_fixture, runner_fixture_sha256};
use serde_json::{Value, json};

const FUNCTION: &str = "demo-dispatch";
const CRON: &str = "demo-sweep";
const PROJECT: &str = "11111111-1111-4111-8111-111111111111";

/// The VM runner `ci` and the host's `lanes`; the provider type leaves the VM start lookups
/// out, which the Functions fake cannot answer.
fn config(lanes: &str) -> Arc<Config> {
    Arc::new(
        parse_config(&format!(
            r#"{{
              "name": "demo",
              "gitlab": {{ "url": "https://gitlab.example", "projectIds": [7] }},
              "host": {{ "type": "scaleway-function", "region": "fr-par" }},
              "provider": {{ "type": "fake", "region": "fr-par" }},
              "vm": {{ "tag": "ci", "image": "img/node" }},
              "lanes": [{lanes}],
              "cache": {{ "type": "s3" }}
            }}"#
        ))
        .unwrap(),
    )
}

fn two_pools() -> Arc<Config> {
    config(r#"{ "tag": "build-x", "image": "img/kaniko", "jobs": ["image"] }"#)
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
        vm_secret: Some("vm-secret".into()),
        sweep_token: Some("sweep-secret".into()),
        dispatcher_env: BTreeMap::new(),
        fleet_key: Some("fleet-secret".into()),
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
            ("code", Action::Create),
            ("cron", Action::Create)
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
    assert_eq!(function["memory_limit"], 1024);
    assert_eq!(
        function["cpu_limit"], 560,
        "Scaleway's CPU share of 1024 MB"
    );
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
            "CIBURST_FLEET_KEY",
            "CIBURST_SWEEP_TOKEN",
            "CIBURST_VM_SECRET",
            "HOOK_TOKEN",
            "RUNNER_TOKEN_BUILD_X",
            "RUNNER_TOKEN_CI"
        ]
    );
    assert_eq!(
        fake.secret(FUNCTION, "CIBURST_DISPATCH_KEY").as_deref(),
        Some("dispatch-secret")
    );
    assert_eq!(
        fake.secret(FUNCTION, vars::VM_SECRET).as_deref(),
        Some("vm-secret")
    );
    assert_eq!(
        fake.secret(FUNCTION, vars::FLEET_KEY).as_deref(),
        Some("fleet-secret")
    );
    let cron = fake.cron(CRON).unwrap();
    assert_eq!(cron["function_id"], function["id"]);
    assert_eq!(cron["schedule"], "*/30 * * * *");
    assert_eq!(cron["args"], json!({ "ciburst_sweep": "sweep-secret" }));
    assert_eq!(cron["status"], "ready", "apply waits for the cron");
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
    assert_eq!(fake.cron_count(), 1);
}

#[tokio::test]
async fn a_changed_vm_secret_updates_the_function_only() {
    let fake = Arc::new(FakeFunctions::new());
    let host = host(two_pools(), &fake);
    host.apply(&host.plan(&secrets()).await.unwrap(), &secrets())
        .await
        .unwrap();
    let mut rotated = secrets();
    rotated.vm_secret = Some("vm-rotated".into());
    let plan = host.plan(&rotated).await.unwrap();
    assert_eq!(
        actions(&plan),
        [
            ("namespace", Action::Noop),
            ("function", Action::Update),
            ("code", Action::Noop),
            ("cron", Action::Noop)
        ]
    );
    host.apply(&plan, &rotated).await.unwrap();
    assert_eq!(
        fake.secret(FUNCTION, vars::VM_SECRET).as_deref(),
        Some("vm-rotated")
    );
    assert!(host.plan(&rotated).await.unwrap().iter().all(Step::is_noop));
}

#[tokio::test]
async fn a_new_fleet_key_updates_the_function_only() {
    let fake = Arc::new(FakeFunctions::new());
    let host = host(two_pools(), &fake);
    let mut before = secrets();
    before.fleet_key = None;
    let plan = host.plan(&before).await.unwrap();
    host.apply(&plan, &before).await.unwrap();
    assert_eq!(fake.secret(FUNCTION, vars::FLEET_KEY), None);

    let plan = host.plan(&secrets()).await.unwrap();
    assert_eq!(
        actions(&plan),
        [
            ("namespace", Action::Noop),
            ("function", Action::Update),
            ("code", Action::Noop),
            ("cron", Action::Noop)
        ]
    );
    assert!(!format!("{plan:?}").contains("fleet-secret"), "{plan:?}");
    host.apply(&plan, &secrets()).await.unwrap();
    assert_eq!(
        fake.secret(FUNCTION, vars::FLEET_KEY).as_deref(),
        Some("fleet-secret")
    );
}

/// M6.3 rule R1, found in the live drill: a routing change of a fleet redeployed the function.
#[tokio::test]
async fn a_fleet_routing_change_leaves_the_function_alone() {
    let fake = Arc::new(FakeFunctions::new());
    let first = host(two_pools(), &fake);
    first
        .apply(&first.plan(&secrets()).await.unwrap(), &secrets())
        .await
        .unwrap();

    let mut routing = (*two_pools()).clone();
    routing.ignore_jobs = vec!["prettier".into()];
    routing.lanes[0].jobs = vec!["lint-*".into(), "image".into()];
    routing.lanes[0].max_runs = 9;
    routing.vm.max_runs = 9;
    routing.vm.slots = 3;
    routing.vm.max_queue_s = Some(30);
    let routed = host(Arc::new(routing.clone()), &fake);
    let plan = routed.plan(&secrets()).await.unwrap();
    assert!(plan.iter().all(Step::is_noop), "{plan:?}");

    let mut unfleeted = secrets();
    unfleeted.fleet_key = None;
    let plan = routed.plan(&unfleeted).await.unwrap();
    assert_eq!(
        plan[1].action,
        Action::Update,
        "without a fleet key routing counts"
    );

    let mut shape = routing;
    shape.lanes[0].image = "img/buildkit".into();
    let reshaped = host(Arc::new(shape), &fake);
    let plan = reshaped.plan(&secrets()).await.unwrap();
    assert_eq!(
        plan[1].action,
        Action::Update,
        "a lane's image is not routing"
    );
}

#[tokio::test]
async fn a_rotated_hook_token_updates_the_function_and_the_cron() {
    let fake = Arc::new(FakeFunctions::new());
    let host = host(two_pools(), &fake);
    let plan = host.plan(&secrets()).await.unwrap();
    host.apply(&plan, &secrets()).await.unwrap();
    let cron_id = fake.cron(CRON).unwrap()["id"].clone();

    let mut rotated = secrets();
    rotated.hook_token = Some("hook-rotated".into());
    rotated.sweep_token = Some("sweep-rotated".into());
    let plan = host.plan(&rotated).await.unwrap();
    assert_eq!(
        actions(&plan),
        [
            ("namespace", Action::Noop),
            ("function", Action::Update),
            ("code", Action::Noop),
            ("cron", Action::Update)
        ]
    );
    assert_eq!(
        plan[3].detail.as_deref(),
        Some("schedule or arguments changed")
    );
    assert!(
        !format!("{plan:?}").contains("hook-rotated"),
        "the plan shows no token"
    );
    host.apply(&plan, &rotated).await.unwrap();
    assert_eq!(
        fake.secret(FUNCTION, "HOOK_TOKEN").as_deref(),
        Some("hook-rotated")
    );
    let cron = fake.cron(CRON).unwrap();
    assert_eq!(cron["id"], cron_id, "updated in place");
    assert_eq!(cron["args"], json!({ "ciburst_sweep": "sweep-rotated" }));
    assert_eq!(cron["status"], "ready");
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
    let one_pool = host(config(""), &fake);
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
async fn dispatcher_variables_are_plain_and_a_change_updates_the_function() {
    let fake = Arc::new(FakeFunctions::new());
    let host = host(two_pools(), &fake);
    let first = "a".repeat(64);
    let mut with_agent = secrets();
    with_agent
        .dispatcher_env
        .insert(vars::AGENT_SHA256.into(), first.clone());
    host.apply(&host.plan(&with_agent).await.unwrap(), &with_agent)
        .await
        .unwrap();
    let function = fake.function(FUNCTION).unwrap();
    assert_eq!(
        function["environment_variables"][vars::AGENT_SHA256],
        first.as_str()
    );
    assert!(
        !fake
            .secret_keys(FUNCTION)
            .contains(&vars::AGENT_SHA256.to_string()),
        "plain, not secret"
    );
    assert!(
        host.plan(&with_agent)
            .await
            .unwrap()
            .iter()
            .all(Step::is_noop)
    );

    let second = "b".repeat(64);
    let mut newer = with_agent.clone();
    newer
        .dispatcher_env
        .insert(vars::AGENT_SHA256.into(), second.clone());
    let plan = host.plan(&newer).await.unwrap();
    assert_eq!(
        actions(&plan),
        [
            ("namespace", Action::Noop),
            ("function", Action::Update),
            ("code", Action::Noop),
            ("cron", Action::Noop)
        ]
    );
    host.apply(&plan, &newer).await.unwrap();
    let env = fake.function(FUNCTION).unwrap()["environment_variables"].clone();
    assert_eq!(env[vars::AGENT_SHA256], second.as_str());
    assert_eq!(env[vars::CODE_DIGEST], embedded_code_digest(), "code kept");
    assert!(host.plan(&newer).await.unwrap().iter().all(Step::is_noop));

    let plan = host.plan(&secrets()).await.unwrap();
    assert_eq!(plan[1].action, Action::Update);
    host.apply(&plan, &secrets()).await.unwrap();
    let env = fake.function(FUNCTION).unwrap()["environment_variables"].clone();
    assert!(env.get(vars::AGENT_SHA256).is_none(), "{env}");
}

#[tokio::test]
async fn dispatcher_variables_cannot_take_the_functions_own_names() {
    let fake = Arc::new(FakeFunctions::new());
    let host = host(two_pools(), &fake);
    for name in [
        vars::CONFIG,
        vars::PROJECT_ID,
        vars::CODE_DIGEST,
        vars::VM_SECURITY_GROUP,
        vars::VM_IMAGES,
        "HOOK_TOKEN",
        "RUNNER_TOKEN_CI",
        "SCW_DEFAULT_PROJECT_ID",
        "lower",
    ] {
        let mut bad = secrets();
        bad.dispatcher_env.insert(name.into(), "value".into());
        let error = host.plan(&bad).await.unwrap_err().to_string();
        assert_eq!(
            error,
            format!(
                "scaleway-function: dispatcher variable {name} is reserved or not an upper-case name"
            )
        );
        let error = host.apply(&[], &bad).await.unwrap_err().to_string();
        assert!(error.contains(name), "{error}");
    }
    assert_eq!(fake.namespace_count(), 0, "refused before any write");
}

/// The Functions API on `FakeFunctions`, every other Scaleway API on the provider's fake.
struct Apis {
    functions: Arc<FakeFunctions>,
    scaleway: Arc<ScalewayFake>,
}

#[async_trait]
impl HttpClient for Apis {
    async fn send(&self, request: Request) -> Result<Response> {
        if request.url.contains("/functions/v1beta1/") || request.url.contains("/fake-functions/") {
            self.functions.send(request).await
        } else {
            self.scaleway.send(request).await
        }
    }
}

/// M7 hardening rule R3: with a VM pool of the `scaleway` provider the function gets the
/// security group and per instance type the image a start would look up, and a changed image
/// plans the function update.
#[tokio::test]
async fn vm_pools_hand_their_security_group_and_images_to_the_function() {
    let config = Arc::new(
        parse_config(
            r#"{
              "name": "demo",
              "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
              "host": { "type": "scaleway-function", "region": "fr-par" },
              "provider": { "type": "scaleway", "region": "fr-par" },
              "vm": { "tag": "ci", "image": "img/docker", "instanceTypes": ["DEV1-M", "PLAY2-NANO", "DEV1-L"] },
              "lanes": [ { "tag": "build-x", "image": "img/docker", "jobs": ["image"] } ],
              "cache": { "type": "s3" }
            }"#,
        )
        .unwrap(),
    );
    let scaleway = Arc::new(ScalewayFake::new());
    let provider = Scaleway::new(config.clone(), scaleway.client(), Log::silent())
        .with_s3_access_key(FAKE_ACCESS_KEY)
        .with_agent(Some(agent_fixture()))
        .with_runner_source(RUNNER_FIXTURE_URL, runner_fixture_sha256());
    provider
        .apply(&provider.plan(&[]).await.unwrap())
        .await
        .unwrap();
    let functions = Arc::new(FakeFunctions::new());
    let apis = Arc::new(Apis {
        functions: functions.clone(),
        scaleway: scaleway.clone(),
    });
    let client = ScalewayClient::new(apis, ScalewayFake::credentials(), FAKE_REGION);
    let host = ScalewayFunction::new(config, Arc::new(client), Log::silent());
    host.apply(&host.plan(&secrets()).await.unwrap(), &secrets())
        .await
        .unwrap();

    let env = functions.function(FUNCTION).unwrap()["environment_variables"].clone();
    let group = scaleway.instance.security_groups()[0]["id"].clone();
    assert_eq!(env[vars::VM_SECURITY_GROUP], group);
    let images = |env: &Value| -> Value {
        serde_json::from_str(env[vars::VM_IMAGES].as_str().unwrap()).unwrap()
    };
    let docker = fake_uuid(9, 1);
    assert_eq!(
        images(&env),
        json!({
            "DEV1-L": { "image": docker, "installDocker": false },
            "DEV1-M": { "image": docker, "installDocker": false },
        }),
        "PLAY2-NANO takes no local image, so a start of it looks up"
    );
    let secret = functions.secret_keys(FUNCTION);
    for name in [vars::VM_SECURITY_GROUP, vars::VM_IMAGES] {
        assert!(!secret.contains(&name.to_string()), "{name} is plain");
    }
    let again = host.plan(&secrets()).await.unwrap();
    assert!(again.iter().all(Step::is_noop), "{again:?}");

    scaleway.instance.remove_local_images("docker");
    let plan = host.plan(&secrets()).await.unwrap();
    assert_eq!(
        actions(&plan),
        [
            ("namespace", Action::Noop),
            ("function", Action::Update),
            ("code", Action::Noop),
            ("cron", Action::Noop)
        ]
    );
    assert_eq!(
        plan[1].detail.as_deref(),
        Some("settings, config or secrets changed")
    );
    host.apply(&plan, &secrets()).await.unwrap();
    let env = functions.function(FUNCTION).unwrap()["environment_variables"].clone();
    let ubuntu = fake_uuid(9, 3);
    assert_eq!(
        images(&env),
        json!({
            "DEV1-L": { "image": ubuntu, "installDocker": true },
            "DEV1-M": { "image": ubuntu, "installDocker": true },
        })
    );
    assert_eq!(env[vars::CODE_DIGEST], embedded_code_digest(), "code kept");
    assert!(
        host.plan(&secrets())
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
            ("code", Action::Update),
            ("cron", Action::Noop)
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
async fn a_cron_changed_or_removed_outside_is_repaired() {
    let fake = Arc::new(FakeFunctions::new());
    let host = host(two_pools(), &fake);
    host.apply(&host.plan(&secrets()).await.unwrap(), &secrets())
        .await
        .unwrap();
    let deploys = fake.deploy_count(FUNCTION);

    for (field, value) in [
        ("schedule", json!("0 * * * *")),
        ("args", json!({ "ciburst_sweep": "stale" })),
        ("args", json!({})),
    ] {
        fake.set_cron(CRON, field, value);
        let plan = host.plan(&secrets()).await.unwrap();
        assert_eq!(
            actions(&plan)[1..],
            [
                ("function", Action::Noop),
                ("code", Action::Noop),
                ("cron", Action::Update)
            ],
            "{field}"
        );
        host.apply(&plan, &secrets()).await.unwrap();
        let cron = fake.cron(CRON).unwrap();
        assert_eq!(cron["schedule"], "*/30 * * * *");
        assert_eq!(cron["args"], json!({ "ciburst_sweep": "sweep-secret" }));
    }

    let id = fake.cron(CRON).unwrap()["id"].as_str().unwrap().to_string();
    let path = format!("/functions/v1beta1/regions/fr-par/crons/{id}");
    let client = client(&fake, Some(PROJECT));
    client.delete(&path).await.unwrap();
    assert_eq!(fake.cron_count(), 0);
    let plan = host.plan(&secrets()).await.unwrap();
    assert_eq!(plan[3].id, "cron");
    assert_eq!(plan[3].action, Action::Create);
    host.apply(&plan, &secrets()).await.unwrap();
    assert_eq!(fake.cron_count(), 1);
    assert!(
        host.plan(&secrets())
            .await
            .unwrap()
            .iter()
            .all(Step::is_noop)
    );
    assert_eq!(
        fake.deploy_count(FUNCTION),
        deploys,
        "a cron change redeploys nothing"
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
    assert_eq!(fake.cron_count(), 1);
    let cron_id = fake.cron(CRON).unwrap()["id"].as_str().unwrap().to_string();
    host.destroy().await.unwrap();
    assert_eq!(fake.namespace_count(), 0);
    assert_eq!(fake.function_count(), 0);
    assert_eq!(fake.cron_count(), 0);
    assert!(
        fake.calls()
            .iter()
            .any(|c| c.method == "DELETE" && c.path.ends_with(&format!("/crons/{cron_id}"))),
        "the cron is deleted explicitly"
    );
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
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("src/testing") || n.contains("/src/testing")),
        "no test fakes: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|n| n == "vendor/scaleway/src/provider/steps.rs")
    );
    let vendored: BTreeSet<&str> = names
        .iter()
        .filter_map(|n| n.strip_prefix("vendor/")?.split('/').next())
        .filter(|name| !name.is_empty())
        .collect();
    assert_eq!(
        vendored,
        BTreeSet::from(["core", "scaleway"]),
        "every path dependency of the function, and nothing else"
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
    assert!(root.contains("rust-version = \"1.96\""));
    assert!(!root.contains("[workspace.package]"));
    assert!(!root.contains("[workspace.dependencies]"));
    assert!(!root.contains("[dev-dependencies]"));

    let scaleway = read("vendor/scaleway/Cargo.toml");
    assert!(scaleway.contains("path = \"../core\""));
    assert!(!scaleway.contains("[build-dependencies]"));
    assert!(!scaleway.contains("[dev-dependencies]"));
    assert!(!scaleway.contains("zip"));
    let core = read("vendor/core/Cargo.toml");
    assert!(!core.contains("[dev-dependencies]"));
    for manifest in [&root, &scaleway, &core] {
        assert!(!manifest.contains(".workspace"), "{manifest}");
        assert!(!manifest.contains("workspace = true"), "{manifest}");
    }
    assert!(scaleway.contains("\"time\""), "tokio features merged");

    assert!(read("src/lib.rs").contains("pub async fn handle("));
    assert!(read("Cargo.lock").contains("name = \"ciburst-dispatch-scaleway\""));
}

#[test]
fn the_zip_digest_is_stable() {
    assert_eq!(embedded_code_digest().len(), 64);
    assert_eq!(embedded_code_digest(), embedded_code_digest());
}
