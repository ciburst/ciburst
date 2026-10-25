//! The `hetzner` provider against the fake Hetzner Cloud API: plan and apply of the firewall,
//! start with type fallback, the cloud-init user data, capacity, claim, report, list, stop,
//! sweep, destroy, pagination and error mapping.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ciburst_core::Error;
use ciburst_core::cloud_init::RUNNER_SHA256;
use ciburst_core::composite::Composite;
use ciburst_core::config::{CacheType, Config};
use ciburst_core::http::HttpClient;
use ciburst_core::log::Log;
use ciburst_core::testing::contract::contract_config;
use ciburst_core::types::{
    Action, Capabilities, Outputs, Provider, Rotation, Run, RunState, StartRequest, Step, VmReport,
    VmState,
};
use ciburst_core::vm::{
    SWEEP_DRAINING_S, SWEEP_MAX_AGE_S, SWEEP_SILENT_S, SWEEP_STARTING_S, verify,
};
use ciburst_hetzner::testing::hcloud_api::{FAKE_CLOCK, FAKE_TOKEN, FakeHetzner};
use ciburst_hetzner::{FIREWALL_STEP, Hetzner, TOKEN_VAR};
use serde_json::{Value, json};

const SECRET: &str = "vm-secret-must-stay-in-the-dispatcher";
const RUNNER_TOKEN: &str = "glrt-vm-runner-token";
const CACHE_SECRET: &str = "cache-secret-key-value";
const DISPATCH_URL: &str = "https://dispatch.example.invalid";
const AGENT_SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const AGENT_URL: &str = "https://contract-cache-p-1.s3.example.invalid/ciburst/agent/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const RUNNER_URL: &str = "https://contract-cache-p-1.s3.example.invalid/ciburst/runner/bed4af01315780cf27d259b9d7a74d20cc0a6fc0d34c9952205b59c244639fd1";

/// The contract config with both pools on Hetzner and an empty `providers.hetzner`.
fn config(change: impl FnOnce(&mut Config)) -> Arc<Config> {
    let mut config = (*contract_config()).clone();
    config
        .providers
        .insert("hetzner".to_string(), serde_json::Map::new());
    for pool in &mut config.pools {
        pool.runtime = Some("hetzner".to_string());
    }
    change(&mut config);
    Arc::new(config)
}

fn plain() -> Arc<Config> {
    config(|_| {})
}

fn provider(config: Arc<Config>, fake: &Arc<FakeHetzner>, log: Log) -> Hetzner {
    let http: Arc<dyn HttpClient> = fake.clone();
    Hetzner::new(config, FAKE_TOKEN.to_string(), http, log).with_retry_delay(Duration::ZERO)
}

async fn provisioned(config: Arc<Config>, log: Log) -> (Arc<FakeHetzner>, Hetzner) {
    let fake = Arc::new(FakeHetzner::new());
    let provider = provider(config, &fake, log);
    let steps = provider.plan(&[]).await.expect("plan");
    provider.apply(&steps).await.expect("apply");
    (fake, provider)
}

fn env() -> BTreeMap<String, String> {
    [
        ("RUNNER_TOKEN", RUNNER_TOKEN),
        ("CACHE_S3_ACCESS_KEY", "CACHEACCESS000000000"),
        ("CACHE_S3_SECRET_KEY", CACHE_SECRET),
        ("CIBURST_VM_SECRET", SECRET),
        ("CIBURST_DISPATCH_URL", DISPATCH_URL),
        ("CIBURST_AGENT_URL", AGENT_URL),
        ("CIBURST_AGENT_SHA256", AGENT_SHA256),
        ("CIBURST_RUNNER_URL", RUNNER_URL),
        ("CACHE_TYPE", "s3"),
        ("CACHE_S3_SERVER_ADDRESS", "s3.example.invalid"),
        ("CACHE_S3_BUCKET_NAME", "contract-cache-p-1"),
        ("CACHE_S3_BUCKET_LOCATION", "fr-par"),
        ("CACHE_SHARED", "true"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn request(config: &Config, tag: &str) -> StartRequest {
    StartRequest {
        pool: config.pool_by_tag(tag).expect("pool").clone(),
        env: env(),
        labels: [
            ("project", "7"),
            ("pipeline", "1"),
            ("job", "2"),
            ("stage", "test"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
        taken: 1,
    }
}

fn actions(steps: &[Step]) -> Vec<(String, Action)> {
    steps.iter().map(|s| (s.id.clone(), s.action)).collect()
}

fn labels(server: &Value) -> BTreeMap<String, String> {
    server["labels"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect()
}

fn pairs(items: &[(&str, &str)]) -> BTreeMap<String, String> {
    items
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn name_of(run_id: &str) -> &str {
    run_id.strip_prefix("hetzner:").expect("hetzner run id")
}

/// `vm.env` of a cloud-config document as key and value pairs.
fn vm_env(user_data: &str) -> BTreeMap<String, String> {
    let mut lines = user_data.lines();
    lines
        .by_ref()
        .find(|l| *l == "  - path: /etc/ciburst/vm.env")
        .expect("vm.env in write_files");
    assert_eq!(lines.next(), Some("    owner: root:root"));
    assert_eq!(lines.next(), Some("    permissions: '0600'"));
    assert_eq!(lines.next(), Some("    content: |"));
    lines
        .take_while(|l| l.starts_with("      "))
        .map(|line| {
            let (key, value) = line.trim().split_once('=').expect("KEY=VALUE");
            (key.to_string(), value.to_string())
        })
        .collect()
}

fn runcmd(user_data: &str) -> Vec<String> {
    user_data
        .lines()
        .skip_while(|l| *l != "runcmd:")
        .skip(1)
        .map(|l| {
            l.strip_prefix("  - '")
                .and_then(|l| l.strip_suffix('\''))
                .expect("runcmd entries are quoted shell strings")
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn first_plan_creates_the_firewall_and_apply_outputs_the_token() {
    let fake = Arc::new(FakeHetzner::new());
    let (log, lines) = Log::memory();
    let provider = provider(plain(), &fake, log);
    let steps = provider.plan(&[]).await.unwrap();
    assert_eq!(
        actions(&steps),
        [(FIREWALL_STEP.to_string(), Action::Create)]
    );
    assert_eq!(steps[0].resource, "firewall contract-vm");
    assert!(fake.writes().is_empty(), "plan writes nothing");

    let outputs = provider.apply(&steps).await.unwrap();
    assert_eq!(
        outputs,
        BTreeMap::from([(TOKEN_VAR.to_string(), FAKE_TOKEN.to_string())])
    );
    let writes = fake.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].body,
        Some(json!({
            "name": "contract-vm",
            "labels": { "ciburst": "true", "ciburst-name": "contract" },
            "apply_to": [
                { "type": "label_selector", "label_selector": { "selector": "ciburst-name==contract" } }
            ],
        }))
    );
    let firewalls = fake.firewalls();
    assert_eq!(firewalls.len(), 1);
    assert_eq!(firewalls[0]["rules"], json!([]));
    assert!(
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("info created firewall") && l.contains("contract-vm"))
    );

    let again = provider.plan(&[Rotation::Vm]).await.unwrap();
    assert_eq!(actions(&again), [(FIREWALL_STEP.to_string(), Action::Noop)]);
    let outputs = provider.apply(&again).await.unwrap();
    assert_eq!(outputs[TOKEN_VAR], FAKE_TOKEN);
    assert_eq!(fake.writes().len(), 1, "a noop plan writes nothing");
    let caps = provider.capabilities();
    assert_eq!(caps.name, "hetzner");
    assert!(caps.docker_daemon);
}

#[tokio::test]
async fn the_firewall_goes_with_the_last_hetzner_pool() {
    let (fake, _) = provisioned(plain(), Log::silent()).await;
    let elsewhere = config(|c| {
        for pool in &mut c.pools {
            pool.runtime = None;
        }
    });
    let (log, lines) = Log::memory();
    let provider = provider(elsewhere.clone(), &fake, log);
    let steps = provider.plan(&[]).await.unwrap();
    assert_eq!(
        actions(&steps),
        [(FIREWALL_STEP.to_string(), Action::Delete)]
    );
    assert_eq!(steps[0].resource, "firewall contract-vm");
    provider.apply(&steps).await.unwrap();
    assert!(fake.firewalls().is_empty());
    let paths: Vec<(String, String)> = fake
        .writes()
        .into_iter()
        .map(|c| (c.method, c.path))
        .skip(1)
        .collect();
    let id = paths[0].1.split('/').nth(2).unwrap().to_string();
    assert_eq!(
        paths,
        [
            (
                "POST".to_string(),
                format!("/firewalls/{id}/actions/remove_from_resources")
            ),
            ("DELETE".to_string(), format!("/firewalls/{id}")),
        ]
    );
    assert!(
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("info deleted firewall"))
    );
    assert!(provider.plan(&[]).await.unwrap().is_empty());
    assert_eq!(
        provider
            .start(request(&elsewhere, "ci"))
            .await
            .unwrap_err()
            .to_string(),
        "hetzner: pool ci does not run on hetzner"
    );
    assert_eq!(
        provider
            .start(request(&plain(), "ci"))
            .await
            .unwrap_err()
            .to_string(),
        "hetzner: pool ci is not provisioned"
    );
    assert!(fake.servers().is_empty());
}

#[tokio::test]
async fn plan_repairs_a_firewall_that_drifted() {
    let (fake, provider) = provisioned(plain(), Log::silent()).await;
    fake.add_firewall_rule(
        "contract-vm",
        json!({ "direction": "in", "protocol": "tcp", "port": "22", "source_ips": ["0.0.0.0/0"] }),
    );
    fake.clear_firewall("contract-vm");
    let steps = provider.plan(&[]).await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].action, Action::Update);
    assert_eq!(
        steps[0].detail.as_deref(),
        Some("remove rules, apply to the label selector")
    );
    let error = provider
        .start(request(&plain(), "ci"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "hetzner: pool ci is not provisioned");
    provider.apply(&steps).await.unwrap();
    let firewall = &fake.firewalls()[0];
    assert_eq!(firewall["rules"], json!([]));
    assert_eq!(
        firewall["applied_to"][0]["label_selector"]["selector"],
        "ciburst-name==contract"
    );
    let again = provider.plan(&[]).await.unwrap();
    assert!(again.iter().all(Step::is_noop), "{again:?}");
}

#[tokio::test]
async fn start_creates_the_server_of_rule_r12_with_its_user_data() {
    let (log, lines) = Log::memory();
    let (fake, provider) = provisioned(plain(), log).await;
    let before = fake.calls().len();
    let run_id = provider.start(request(&plain(), "ci")).await.unwrap();
    let name = name_of(&run_id).to_string();
    let suffix = name.strip_prefix("contract-ci-").expect("name prefix");
    assert!(suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()));

    let calls: Vec<_> = fake.calls().into_iter().skip(before).collect();
    let routes: Vec<(&str, &str)> = calls
        .iter()
        .map(|c| (c.method.as_str(), c.path.as_str()))
        .collect();
    assert_eq!(routes, [("GET", "/firewalls"), ("POST", "/servers")]);
    let body = calls[1].body.clone().unwrap();
    let user_data = body["user_data"].as_str().unwrap().to_string();
    assert_eq!(
        body,
        json!({
            "name": name,
            "server_type": "cx33",
            "image": "docker-ce",
            "location": "nbg1",
            "start_after_create": true,
            "user_data": user_data,
            "labels": {
                "ciburst": "true",
                "ciburst-name": "contract",
                "ciburst-pool": "ci",
                "ciburst-job": "2",
                "ciburst-state": "starting",
                "ciburst-free": "1",
            },
            "public_net": { "enable_ipv4": true, "enable_ipv6": true },
        })
    );

    let server = fake.server(&name).unwrap();
    assert_eq!(server["status"], "initializing");
    assert_eq!(server["server_type"]["name"], "cx33");
    let firewall = &fake.firewalls()[0];
    assert_eq!(
        server["public_net"]["firewalls"],
        json!([{ "id": firewall["id"], "status": "applied" }])
    );
    assert_eq!(
        firewall["applied_to"][0]["applied_to_resources"],
        json!([{ "type": "server", "server": { "id": server["id"] } }])
    );
    assert_eq!(fake.user_data(&name).as_deref(), Some(user_data.as_str()));

    assert!(user_data.starts_with("#cloud-config\n"));
    let vm_env = vm_env(&user_data);
    let expected: BTreeMap<String, String> = [
        ("CIBURST_RUN_ID", run_id.as_str()),
        ("CIBURST_DISPATCH_URL", DISPATCH_URL),
        ("CI_SERVER_URL", "https://gitlab.example"),
        ("RUNNER_TOKEN", RUNNER_TOKEN),
        ("CIBURST_POOL", "ci"),
        ("CIBURST_SLOTS", "2"),
        ("CIBURST_DEFAULT_IMAGE", "example.invalid/node:24"),
        ("CIBURST_TIMEOUT_S", "3600"),
        ("CACHE_TYPE", "s3"),
        ("CACHE_S3_SERVER_ADDRESS", "s3.example.invalid"),
        ("CACHE_S3_BUCKET_NAME", "contract-cache-p-1"),
        ("CACHE_S3_BUCKET_LOCATION", "fr-par"),
        ("CACHE_S3_ACCESS_KEY", "CACHEACCESS000000000"),
        ("CACHE_S3_SECRET_KEY", CACHE_SECRET),
        ("CACHE_SHARED", "true"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .chain([(
        "CIBURST_VM_TOKEN".to_string(),
        vm_env["CIBURST_VM_TOKEN"].clone(),
    )])
    .collect();
    assert_eq!(vm_env, expected);
    let token = vm_env["CIBURST_VM_TOKEN"].clone();
    assert!(verify(SECRET, &run_id, &token));
    assert!(!verify(SECRET, &name, &token), "the token binds the run id");
    assert!(!verify(SECRET, "hetzner:contract-ci-00000000", &token));

    let commands = runcmd(&user_data);
    assert_eq!(commands.len(), 4, "{commands:?}");
    assert!(commands[0].starts_with("test -f /swapfile || (fallocate -l 4G /swapfile"));
    assert_eq!(
        commands[1],
        format!(
            "curl -fsSL -o /tmp/gitlab-runner {RUNNER_URL} && echo \"{RUNNER_SHA256}  /tmp/gitlab-runner\" | sha256sum -c - && install -m 0755 /tmp/gitlab-runner /usr/local/bin/gitlab-runner && rm -f /tmp/gitlab-runner"
        ),
        "M6 rule R6: the runner of the cache bucket, checked against the pinned hash"
    );
    assert!(!user_data.contains(ciburst_core::cloud_init::RUNNER_URL));
    assert_eq!(
        commands[2],
        format!(
            "curl -fsSL -o /tmp/ciburst-vm-agent {AGENT_URL} && echo \"{AGENT_SHA256}  /tmp/ciburst-vm-agent\" | sha256sum -c - && install -m 0755 /tmp/ciburst-vm-agent /usr/local/bin/ciburst-vm-agent && rm -f /tmp/ciburst-vm-agent"
        )
    );
    assert!(
        !user_data.contains("apt-get install"),
        "docker-ce has Docker"
    );
    assert!(!user_data.contains("docker create"));

    let logged = lines.lock().unwrap().clone();
    let everything = format!(
        "{user_data}\n{:?}\n{:?}\n{logged:?}",
        labels(&server),
        fake.calls()
    );
    assert!(!everything.contains(SECRET), "the VM secret leaked");
    assert!(!vm_env.contains_key("CIBURST_VM_SECRET"));
    assert!(!vm_env.contains_key("CIBURST_AGENT_URL"));
    assert!(!vm_env.contains_key("CIBURST_AGENT_SHA256"));
    assert!(!vm_env.contains_key("CIBURST_RUNNER_URL"));
    assert!(
        logged
            .iter()
            .any(|l| l.starts_with("info started server") && l.contains(&name))
    );
    let password = format!("fake-root-password-{}", server["id"]);
    for line in &logged {
        for secret in [
            RUNNER_TOKEN,
            CACHE_SECRET,
            token.as_str(),
            FAKE_TOKEN,
            password.as_str(),
        ] {
            assert!(!line.contains(secret), "{line}");
        }
    }
}

#[tokio::test]
async fn location_and_image_come_from_the_settings() {
    let custom = config(|c| {
        c.providers.insert(
            "hetzner".to_string(),
            json!({ "location": "hel1", "image": "ubuntu-24.04" })
                .as_object()
                .cloned()
                .unwrap(),
        );
    });
    let (fake, provider) = provisioned(custom.clone(), Log::silent()).await;
    let run_id = provider.start(request(&custom, "ci")).await.unwrap();
    let server = fake.server(name_of(&run_id)).unwrap();
    assert_eq!(server["location"]["name"], "hel1");
    assert_eq!(server["image"]["name"], "ubuntu-24.04");
}

#[tokio::test]
async fn start_falls_back_to_the_next_type_and_names_every_failure() {
    let (log, lines) = Log::memory();
    let (fake, provider) = provisioned(plain(), log).await;
    fake.set_unavailable("cx33");
    let run_id = provider.start(request(&plain(), "ci")).await.unwrap();
    let server = fake.server(name_of(&run_id)).unwrap();
    assert_eq!(server["server_type"]["name"], "cx43");
    assert_eq!(fake.servers().len(), 1);
    assert!(lines.lock().unwrap().iter().any(|l| {
        l.starts_with("warn server type unavailable")
            && l.contains("type=\"cx33\"")
            && l.contains("code=\"resource_unavailable\"")
    }));

    fake.set_limit_exceeded("cx43");
    let error = provider
        .start(request(&plain(), "ci"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "hetzner: pool ci: no server type started (cx33: resource_unavailable (412): server type \
         cx33 is unavailable in nbg1; cx43: resource_limit_exceeded (403): server limit exceeded)"
    );
    assert_eq!(fake.servers().len(), 1);
}

#[tokio::test]
async fn pool_types_fall_back_on_placement_errors_and_free_follows_taken() {
    let custom = config(|c| {
        c.pools[0].instance_types = vec!["cx23".into(), "cx53".into()];
        c.pools[0].slots = 3;
    });
    let (fake, provider) = provisioned(custom.clone(), Log::silent()).await;
    fake.set_placement_error("cx23");
    let mut start = request(&custom, "ci");
    let run_id = provider.start(start.clone()).await.unwrap();
    let server = fake.server(name_of(&run_id)).unwrap();
    assert_eq!(server["server_type"]["name"], "cx53");
    assert_eq!(labels(&server)["ciburst-free"], "2");
    for (taken, free) in [(0, "3"), (2, "1"), (3, "0"), (5, "0")] {
        start.taken = taken;
        let run_id = provider.start(start.clone()).await.unwrap();
        let server = fake.server(name_of(&run_id)).unwrap();
        assert_eq!(labels(&server)["ciburst-free"], free, "taken {taken}");
    }
}

#[tokio::test]
async fn other_errors_stop_the_start_at_once() {
    let custom = config(|c| c.pools[0].instance_types = vec!["cx99".into(), "cx33".into()]);
    let (fake, provider) = provisioned(custom.clone(), Log::silent()).await;
    let error = provider
        .start(request(&custom, "ci"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "hetzner: POST /servers: invalid_input (422): server type not found"
    );
    assert!(fake.servers().is_empty());
}

#[tokio::test]
async fn start_refuses_what_a_vm_cannot_use_before_creating_anything() {
    let fake = Arc::new(FakeHetzner::new());
    let early = provider(plain(), &fake, Log::silent());
    let error = early
        .start(request(&plain(), "ci"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "hetzner: pool ci is not provisioned");

    let (fake, provider) = provisioned(plain(), Log::silent()).await;
    for key in [
        "RUNNER_TOKEN",
        "CIBURST_VM_SECRET",
        "CIBURST_DISPATCH_URL",
        "CIBURST_AGENT_URL",
        "CIBURST_AGENT_SHA256",
        "CIBURST_RUNNER_URL",
        "CACHE_TYPE",
        "CACHE_S3_SERVER_ADDRESS",
        "CACHE_S3_BUCKET_NAME",
        "CACHE_S3_BUCKET_LOCATION",
        "CACHE_S3_ACCESS_KEY",
        "CACHE_S3_SECRET_KEY",
    ] {
        let mut start = request(&plain(), "ci");
        start.env.remove(key);
        let error = provider.start(start).await.unwrap_err().to_string();
        assert_eq!(error, format!("hetzner: pool ci: {key} missing"));
        let mut start = request(&plain(), "ci");
        start.env.insert(key.to_string(), String::new());
        let error = provider.start(start).await.unwrap_err().to_string();
        assert_eq!(error, format!("hetzner: pool ci: {key} missing"));
    }
    let mut start = request(&plain(), "ci");
    start.env.insert("CACHE_TYPE".into(), "gcs".into());
    let error = provider.start(start).await.unwrap_err().to_string();
    assert_eq!(
        error,
        "hetzner: pool ci: CACHE_TYPE is gcs, the VM agent takes s3"
    );
    let mut start = request(&plain(), "ci");
    start.env.insert(
        "CIBURST_AGENT_URL".into(),
        "http://plain.example/agent".into(),
    );
    let error = provider.start(start).await.unwrap_err().to_string();
    assert_eq!(
        error,
        "cloud-init: CIBURST_AGENT_URL \"http://plain.example/agent\" is not an https URL"
    );
    let mut start = request(&plain(), "ci");
    start
        .env
        .insert("CIBURST_AGENT_SHA256".into(), "not-a-hash".into());
    let error = provider.start(start).await.unwrap_err().to_string();
    assert_eq!(
        error,
        "cloud-init: CIBURST_AGENT_SHA256 \"not-a-hash\" is not a SHA-256 in hex"
    );
    let mut start = request(&plain(), "ci");
    start.env.insert(
        "CIBURST_RUNNER_URL".into(),
        "https://cache.example/runner;reboot".into(),
    );
    let error = provider.start(start).await.unwrap_err().to_string();
    assert_eq!(
        error,
        "cloud-init: CIBURST_RUNNER_URL \"https://cache.example/runner;reboot\" is not an https URL"
    );

    let jobs = config(|c| c.pools[1].runtime = Some("jobs".into()));
    let error = provider
        .start(request(&jobs, "build"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "hetzner: pool build does not run on hetzner");
    assert!(fake.servers().is_empty(), "nothing started");
    assert!(!fake.calls().iter().any(|c| c.path == "/servers"));
}

#[tokio::test]
async fn preflight_checks_token_location_and_types_in_two_reads() {
    let fake = Arc::new(FakeHetzner::new());
    provider(plain(), &fake, Log::silent())
        .preflight()
        .await
        .unwrap();
    let calls: Vec<String> = fake.calls().iter().map(|c| c.path.clone()).collect();
    assert_eq!(calls, ["/locations", "/server_types"]);
    assert!(fake.writes().is_empty());

    let http: Arc<dyn HttpClient> = fake.clone();
    let error = Hetzner::new(plain(), "wrong-token".into(), http, Log::silent())
        .preflight()
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "hetzner: the API refuses HCLOUD_TOKEN: unauthorized (401): unable to authenticate"
    );

    let at = |location: &str| {
        config(|c| {
            c.providers.insert(
                "hetzner".to_string(),
                json!({ "location": location })
                    .as_object()
                    .cloned()
                    .unwrap(),
            );
        })
    };
    let error = provider(at("xyz1"), &fake, Log::silent())
        .preflight()
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "hetzner: location xyz1 does not exist; set providers.hetzner.location"
    );
    let error = provider(at("ash"), &fake, Log::silent())
        .preflight()
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "hetzner: pool ci: none of cx33, cx43 is offered in ash; pool build: none of cx33, cx43 \
         is offered in ash"
    );
    let unknown_type = config(|c| c.pools[0].instance_types = vec!["cx99".into()]);
    let error = provider(unknown_type, &fake, Log::silent())
        .preflight()
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "hetzner: pool ci: none of cx99 is offered in nbg1");
    let fallback = config(|c| c.pools[0].instance_types = vec!["cx99".into(), "cx23".into()]);
    provider(fallback, &fake, Log::silent())
        .preflight()
        .await
        .unwrap();

    let none = config(|c| {
        for pool in &mut c.pools {
            pool.runtime = None;
        }
    });
    let fresh = Arc::new(FakeHetzner::new());
    provider(none, &fresh, Log::silent())
        .preflight()
        .await
        .unwrap();
    assert!(fresh.calls().is_empty(), "no Hetzner pool, no request");
}

#[tokio::test]
async fn without_an_s3_cache_no_cache_key_reaches_the_vm() {
    let no_cache = config(|c| c.cache.kind = CacheType::None);
    let (fake, provider) = provisioned(no_cache.clone(), Log::silent()).await;
    let mut start = request(&no_cache, "ci");
    start.env.remove("CACHE_TYPE");
    let run_id = provider.start(start).await.unwrap();
    let user_data = fake.user_data(name_of(&run_id)).unwrap();
    let vm_env = vm_env(&user_data);
    assert!(
        !vm_env.keys().any(|k| k.starts_with("CACHE_")),
        "{vm_env:?}"
    );
    assert!(!user_data.contains(CACHE_SECRET));
}

#[tokio::test]
async fn invalid_settings_and_pools_are_config_errors_naming_the_key() {
    let fake = Arc::new(FakeHetzner::new());
    let bad = config(|c| {
        c.providers.insert(
            "hetzner".to_string(),
            json!({ "location": 5, "imag": "docker-ce" })
                .as_object()
                .cloned()
                .unwrap(),
        );
        c.pools[0].tag = "ci-".into();
        c.pools[1].tag = "b".repeat(46);
        c.pools[1].instance_types = vec!["CX33".into()];
        c.default_pool = Some("ci-".into());
    });
    let provider = provider(bad.clone(), &fake, Log::silent());
    let errors = match provider.plan(&[]).await {
        Err(Error::Config(errors)) => errors,
        other => panic!("expected a config error, got {other:?}"),
    };
    let long = "b".repeat(46);
    assert_eq!(
        errors,
        [
            "providers.hetzner.imag: unknown setting; known: location, image".to_string(),
            "providers.hetzner.location: a location name such as nbg1, fsn1 or hel1".to_string(),
            "pools[0].tag: ci- must end with a letter or digit to be a Hetzner label value"
                .to_string(),
            format!(
                "pools[1].tag: {long} makes the Hetzner server name contract-{long}-<8 hex> \
                 longer than 63 characters"
            ),
            "pools[1].instanceTypes: CX33 is not a Hetzner server type name".to_string(),
        ]
    );
    let error = provider.start(request(&bad, "ci-")).await.unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
    assert!(fake.calls().is_empty(), "no API call with a bad config");
}

#[tokio::test]
async fn capacity_reads_the_pool_servers_and_ignores_other_deployments() {
    let (fake, provider) = provisioned(plain(), Log::silent()).await;
    let first = provider.start(request(&plain(), "ci")).await.unwrap();
    fake.set_clock(FAKE_CLOCK + 60);
    let second = provider.start(request(&plain(), "ci")).await.unwrap();
    let build = provider.start(request(&plain(), "build")).await.unwrap();
    let off = provider.start(request(&plain(), "ci")).await.unwrap();
    fake.set_status(name_of(&off), "off");
    fake.add_server(
        "other-ci-00000000",
        &[
            ("ciburst", "true"),
            ("ciburst-name", "other"),
            ("ciburst-pool", "ci"),
            ("ciburst-free", "5"),
        ],
    );
    fake.add_server(
        "contract-ci-manual",
        &[
            ("ciburst-name", "contract"),
            ("ciburst-pool", "ci"),
            ("ciburst-free", "5"),
        ],
    );
    let report = VmReport {
        state: VmState::Busy,
        free: 1,
        slots: 2,
    };
    provider
        .report(&first, &report, FAKE_CLOCK + 120)
        .await
        .unwrap();

    let mut ci = provider.capacity("ci").await.unwrap().expect("a VM pool");
    ci.sort_by_key(|c| c.created_at);
    assert_eq!(ci.len(), 2, "{ci:?}");
    assert_eq!(ci[0].run_id, first);
    assert_eq!(ci[0].state, VmState::Busy);
    assert_eq!(ci[0].free, 1);
    assert_eq!(ci[0].seen, Some(FAKE_CLOCK + 120));
    assert_eq!(ci[0].created_at, FAKE_CLOCK);
    assert_eq!(ci[1].run_id, second);
    assert_eq!(ci[1].state, VmState::Starting);
    assert_eq!(ci[1].free, 1);
    assert_eq!(ci[1].seen, None);
    assert_eq!(ci[1].created_at, FAKE_CLOCK + 60);
    let build_capacity = provider.capacity("build").await.unwrap().unwrap();
    assert_eq!(
        build_capacity
            .iter()
            .map(|c| c.run_id.as_str())
            .collect::<Vec<_>>(),
        [build.as_str()]
    );
    assert_eq!(provider.capacity("gone").await.unwrap(), Some(Vec::new()));
    assert_eq!(provider.capacity("odd-").await.unwrap(), Some(Vec::new()));
    let selector = fake
        .calls()
        .into_iter()
        .rev()
        .find(|c| c.method == "GET" && c.path == "/servers")
        .unwrap()
        .query;
    assert!(
        selector.contains(
            "label_selector=ciburst%3D%3Dtrue%2Cciburst-name%3D%3Dcontract%2Cciburst-pool%3D%3Dgone"
        ),
        "{selector}"
    );
}

#[tokio::test]
async fn claim_lowers_free_slots_down_to_zero() {
    let (fake, provider) = provisioned(plain(), Log::silent()).await;
    let run_id = provider.start(request(&plain(), "ci")).await.unwrap();
    let name = name_of(&run_id).to_string();
    let before = labels(&fake.server(&name).unwrap());
    let free = || labels(&fake.server(&name).unwrap())["ciburst-free"].clone();
    assert_eq!(free(), "1", "the starting job's slot is taken");
    provider.claim(&run_id).await.unwrap();
    assert_eq!(free(), "0");
    provider.claim(&run_id).await.unwrap();
    assert_eq!(free(), "0");
    let mut after = labels(&fake.server(&name).unwrap());
    after.insert("ciburst-free".into(), "1".into());
    assert_eq!(after, before, "only the free label changes");
    let put = fake.writes().into_iter().last().unwrap();
    assert_eq!(put.method, "PUT");
    assert_eq!(put.body.unwrap()["labels"]["ciburst-pool"], "ci");
}

#[tokio::test]
async fn report_rewrites_the_state_labels_and_keeps_every_other_label() {
    let (fake, provider) = provisioned(plain(), Log::silent()).await;
    let run_id = provider.start(request(&plain(), "ci")).await.unwrap();
    let name = name_of(&run_id).to_string();
    let mut custom: Vec<(String, String)> =
        labels(&fake.server(&name).unwrap()).into_iter().collect();
    custom.push(("owner".into(), "ops".into()));
    let custom: Vec<(&str, &str)> = custom
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    fake.set_labels(&name, &custom);

    let draining = VmReport {
        state: VmState::Draining,
        free: 1,
        slots: 2,
    };
    provider.report(&run_id, &draining, 1_000).await.unwrap();
    provider.report(&run_id, &draining, 1_060).await.unwrap();
    assert_eq!(
        labels(&fake.server(&name).unwrap()),
        pairs(&[
            ("ciburst", "true"),
            ("ciburst-name", "contract"),
            ("ciburst-pool", "ci"),
            ("ciburst-job", "2"),
            ("owner", "ops"),
            ("ciburst-state", "draining"),
            ("ciburst-free", "1"),
            ("ciburst-seen", "1060"),
            ("ciburst-drain", "1000"),
        ])
    );
    let idle = VmReport {
        state: VmState::Idle,
        free: 2,
        slots: 2,
    };
    provider.report(&run_id, &idle, 1_100).await.unwrap();
    let after = labels(&fake.server(&name).unwrap());
    assert_eq!(after["owner"], "ops");
    assert_eq!(after["ciburst-state"], "idle");
    assert_eq!(after["ciburst-free"], "2");
    assert!(!after.contains_key("ciburst-drain"));
}

#[tokio::test]
async fn claim_report_and_stop_refuse_servers_of_others() {
    let (fake, provider) = provisioned(plain(), Log::silent()).await;
    let foreign_labels = [
        ("ciburst", "true"),
        ("ciburst-name", "other"),
        ("ciburst-pool", "ci"),
        ("ciburst-free", "1"),
    ];
    fake.add_server("other-ci-00000000", &foreign_labels);
    let foreign = "hetzner:other-ci-00000000";
    let idle = VmReport {
        state: VmState::Idle,
        free: 1,
        slots: 1,
    };
    let message = "hetzner: server other-ci-00000000 does not belong to contract";
    assert_eq!(
        provider.claim(foreign).await.unwrap_err().to_string(),
        message
    );
    assert_eq!(
        provider
            .report(foreign, &idle, 1)
            .await
            .unwrap_err()
            .to_string(),
        message
    );
    assert_eq!(
        provider.stop(foreign).await.unwrap_err().to_string(),
        message
    );
    assert_eq!(
        provider
            .claim("hetzner:contract-ci-missing")
            .await
            .unwrap_err()
            .to_string(),
        "hetzner: server contract-ci-missing not found"
    );
    for id in [
        "11111111-2222-3333-4444-555555555555",
        "hetzner:",
        "hetzner:a&b",
    ] {
        assert_eq!(
            provider.stop(id).await.unwrap_err().to_string(),
            format!("hetzner: {id} is not a hetzner run id")
        );
    }
    assert_eq!(
        labels(&fake.server("other-ci-00000000").unwrap()),
        pairs(&foreign_labels)
    );
    assert!(
        fake.writes()
            .iter()
            .all(|c| c.path.starts_with("/firewalls")),
        "nothing but the firewall was written"
    );
}

#[tokio::test]
async fn list_shows_servers_as_runs_and_stop_deletes() {
    let (log, lines) = Log::memory();
    let (fake, provider) = provisioned(plain(), log).await;
    let run_id = provider.start(request(&plain(), "ci")).await.unwrap();
    let runs = provider.list(Some("ci")).await.unwrap();
    assert_eq!(
        runs,
        [Run {
            run_id: run_id.clone(),
            pool: "ci".into(),
            state: RunState::Queued,
            created_at: "2026-10-24T00:00:00+00:00".into(),
            labels: pairs(&[("job", "2")]),
        }]
    );
    assert_eq!(
        provider.list(None).await.unwrap()[0].state,
        RunState::Running
    );
    assert!(provider.list(Some("build")).await.unwrap().is_empty());

    let id = fake.server(name_of(&run_id)).unwrap()["id"].clone();
    provider.stop(&run_id).await.unwrap();
    let delete = fake.writes().into_iter().last().unwrap();
    assert_eq!(
        (delete.method.as_str(), delete.path.as_str()),
        ("DELETE", &*format!("/servers/{id}"))
    );
    assert!(provider.list(None).await.unwrap().is_empty());
    assert!(fake.user_data(name_of(&run_id)).is_none());
    assert_eq!(
        provider.stop(&run_id).await.unwrap_err().to_string(),
        format!("hetzner: server {} not found", name_of(&run_id))
    );
    assert!(
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("info deleted server") && l.contains(&run_id))
    );
}

#[tokio::test]
async fn stop_leaves_a_server_alone_that_is_already_deleting() {
    let (fake, provider) = provisioned(plain(), Log::silent()).await;
    let run_id = provider.start(request(&plain(), "ci")).await.unwrap();
    fake.set_status(name_of(&run_id), "deleting");
    provider.stop(&run_id).await.unwrap();
    assert!(!fake.writes().iter().any(|c| c.method == "DELETE"));
    assert_eq!(provider.list(None).await.unwrap()[0].state, RunState::Done);
}

#[tokio::test]
async fn sweep_applies_each_rule_at_its_boundary() {
    let (log, lines) = Log::memory();
    let (fake, provider) = provisioned(plain(), log).await;
    let now = FAKE_CLOCK + 200_000;
    let seen = |ago: u64| (now - ago).to_string();
    let add = |name: &str, age: u64, extra: &[(&str, &str)]| {
        fake.set_clock(now - age);
        let mut labels = vec![
            ("ciburst", "true"),
            ("ciburst-name", "contract"),
            ("ciburst-pool", "ci"),
        ];
        labels.extend_from_slice(extra);
        fake.add_server(name, &labels);
        format!("hetzner:{name}")
    };
    let (seen_limit, seen_late) = (seen(SWEEP_SILENT_S), seen(SWEEP_SILENT_S + 1));
    let (drain_limit, drain_late) = (seen(SWEEP_DRAINING_S), seen(SWEEP_DRAINING_S + 1));
    let (drain_old, seen_recent, seen_five) = (seen(4_000), seen(10), seen(5));
    let (seen_limit, seen_late) = (seen_limit.as_str(), seen_late.as_str());
    let (drain_limit, drain_late) = (drain_limit.as_str(), drain_late.as_str());
    let (drain_old, seen_recent, seen_five) =
        (drain_old.as_str(), seen_recent.as_str(), seen_five.as_str());

    let mut kept = BTreeSet::new();
    let mut swept = BTreeSet::new();
    kept.insert(add(
        "starting-at-limit",
        SWEEP_STARTING_S,
        &[("ciburst-state", "starting")],
    ));
    swept.insert(add(
        "starting-late",
        SWEEP_STARTING_S + 1,
        &[("ciburst-state", "starting")],
    ));
    swept.insert(add("unlabelled-late", SWEEP_STARTING_S + 1, &[]));
    kept.insert(add(
        "seen-at-limit",
        5_000,
        &[("ciburst-state", "idle"), ("ciburst-seen", seen_limit)],
    ));
    swept.insert(add(
        "seen-long-ago",
        5_000,
        &[("ciburst-state", "idle"), ("ciburst-seen", seen_late)],
    ));
    kept.insert(add(
        "never-seen-at-limit",
        SWEEP_SILENT_S,
        &[("ciburst-state", "idle")],
    ));
    swept.insert(add(
        "never-seen-late",
        SWEEP_SILENT_S + 1,
        &[("ciburst-state", "idle")],
    ));
    kept.insert(add(
        "draining-at-limit",
        1_000,
        &[
            ("ciburst-state", "draining"),
            ("ciburst-drain", drain_limit),
        ],
    ));
    swept.insert(add(
        "draining-late",
        1_000,
        &[("ciburst-state", "draining"), ("ciburst-drain", drain_late)],
    ));
    kept.insert(add(
        "draining-but-reporting",
        5_000,
        &[
            ("ciburst-state", "draining"),
            ("ciburst-drain", drain_old),
            ("ciburst-seen", seen_recent),
        ],
    ));
    kept.insert(add(
        "max-age-at-limit",
        SWEEP_MAX_AGE_S,
        &[("ciburst-state", "busy"), ("ciburst-seen", seen_five)],
    ));
    swept.insert(add(
        "max-age-past",
        SWEEP_MAX_AGE_S + 1,
        &[("ciburst-state", "busy"), ("ciburst-seen", seen_five)],
    ));
    let off = add(
        "powered-off",
        SWEEP_STARTING_S + 1,
        &[("ciburst-state", "starting")],
    );
    fake.set_status(name_of(&off), "off");
    swept.insert(off);
    let deleting = add(
        "already-deleting",
        SWEEP_MAX_AGE_S + 1,
        &[("ciburst-state", "idle")],
    );
    fake.set_status(name_of(&deleting), "deleting");
    kept.insert(deleting);
    fake.set_clock(now - SWEEP_MAX_AGE_S * 2);
    fake.add_server(
        "other-ci-00000000",
        &[
            ("ciburst", "true"),
            ("ciburst-name", "other"),
            ("ciburst-state", "starting"),
        ],
    );

    let result: BTreeSet<String> = provider.sweep(now).await.unwrap().into_iter().collect();
    assert_eq!(result, swept);
    let left: BTreeSet<String> = fake
        .servers()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    let mut expected: BTreeSet<String> = kept.iter().map(|r| name_of(r).to_string()).collect();
    expected.insert("other-ci-00000000".to_string());
    assert_eq!(left, expected);
    let lines = lines.lock().unwrap();
    for reason in ["starting", "silent", "draining", "max_age"] {
        assert!(
            lines.iter().any(|l| l.starts_with("info swept server")
                && l.contains(&format!("reason=\"{reason}\""))),
            "{reason} in {lines:?}"
        );
    }
}

#[tokio::test]
async fn sweep_keeps_going_when_one_deletion_fails() {
    let (log, lines) = Log::memory();
    let (fake, provider) = provisioned(plain(), log).await;
    let now = FAKE_CLOCK + SWEEP_MAX_AGE_S + 1;
    for name in ["contract-ci-00000001", "contract-ci-00000002"] {
        fake.add_server(
            name,
            &[
                ("ciburst", "true"),
                ("ciburst-name", "contract"),
                ("ciburst-pool", "ci"),
            ],
        );
    }
    fake.set_rate_limited(true);
    assert!(provider.sweep(now).await.is_err(), "the listing fails");
    fake.set_rate_limited(false);
    fake.set_locked("contract-ci-00000001", true);
    let swept = provider.sweep(now).await.unwrap();
    assert_eq!(swept, ["hetzner:contract-ci-00000002"]);
    assert!(fake.server("contract-ci-00000001").is_some());
    let lines = lines.lock().unwrap();
    assert!(lines.iter().any(|l| {
        l.starts_with("warn sweep failed")
            && l.contains("hetzner:contract-ci-00000001")
            && l.contains("locked (423)")
    }));
    assert!(lines.iter().all(|l| !l.contains(FAKE_TOKEN)));
}

#[tokio::test]
async fn listings_read_every_page() {
    let (fake, provider) = provisioned(plain(), Log::silent()).await;
    fake.set_page_cap(2);
    for n in 0..5 {
        fake.add_server(
            &format!("contract-ci-{n:08x}"),
            &[
                ("ciburst", "true"),
                ("ciburst-name", "contract"),
                ("ciburst-pool", "ci"),
                ("ciburst-state", "idle"),
                ("ciburst-free", "1"),
            ],
        );
    }
    fake.add_server("unrelated", &[("team", "web")]);
    assert_eq!(provider.list(None).await.unwrap().len(), 5);
    assert_eq!(provider.capacity("ci").await.unwrap().unwrap().len(), 5);
    let pages: Vec<String> = fake
        .calls()
        .into_iter()
        .filter(|c| c.method == "GET" && c.path == "/servers")
        .map(|c| c.query)
        .collect();
    assert!(pages.iter().any(|q| q.contains("page=3")), "{pages:?}");
    assert!(!pages.iter().any(|q| q.contains("page=4")), "{pages:?}");
    assert!(pages.iter().all(|q| q.contains("per_page=50")));
    assert!(
        pages
            .iter()
            .all(|q| q.contains("label_selector=ciburst%3D%3Dtrue%2Cciburst-name%3D%3Dcontract"))
    );
}

#[tokio::test]
async fn destroy_removes_servers_then_the_firewall() {
    let (log, lines) = Log::memory();
    let (fake, provider) = provisioned(plain(), log).await;
    provider.start(request(&plain(), "ci")).await.unwrap();
    provider.start(request(&plain(), "build")).await.unwrap();
    fake.add_server(
        "other-ci-00000000",
        &[("ciburst", "true"), ("ciburst-name", "other")],
    );
    let before = fake.writes().len();
    provider.destroy().await.unwrap();
    let left: Vec<String> = fake
        .servers()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(left, ["other-ci-00000000"]);
    assert!(fake.firewalls().is_empty());
    let writes: Vec<String> = fake
        .writes()
        .into_iter()
        .skip(before)
        .map(|c| format!("{} {}", c.method, c.path.split('/').nth(1).unwrap_or("")))
        .collect();
    assert_eq!(
        writes,
        [
            "DELETE servers",
            "DELETE servers",
            "POST firewalls",
            "DELETE firewalls"
        ]
    );
    let logged = lines.lock().unwrap().clone();
    assert_eq!(
        logged
            .iter()
            .filter(|l| l.starts_with("info deleted server"))
            .count(),
        2
    );
    let steps = provider.plan(&[]).await.unwrap();
    assert_eq!(
        actions(&steps),
        [(FIREWALL_STEP.to_string(), Action::Create)]
    );
    provider.destroy().await.unwrap();
}

#[tokio::test]
async fn destroy_waits_while_the_firewall_is_still_detaching() {
    let (fake, hetzner) = provisioned(plain(), Log::silent()).await;
    fake.set_firewall_busy(3);
    hetzner.destroy().await.unwrap();
    assert!(fake.firewalls().is_empty());
    let attempts = fake
        .calls()
        .iter()
        .filter(|c| c.method == "DELETE" && c.path.starts_with("/firewalls/"))
        .count();
    assert_eq!(attempts, 4);

    let (log, lines) = Log::memory();
    let (fake, _) = provisioned(plain(), Log::silent()).await;
    let provider = provider(plain(), &fake, log);
    fake.set_firewall_busy(100);
    provider.destroy().await.unwrap();
    assert_eq!(fake.firewalls().len(), 1);
    assert!(
        lines.lock().unwrap().iter().any(|l| {
            l.starts_with("warn firewall not deleted") && l.contains("resource_in_use")
        })
    );
}

#[tokio::test]
async fn api_errors_name_code_and_message_without_the_token() {
    let fake = Arc::new(FakeHetzner::new());
    fake.set_rate_limited(true);
    let error = provider(plain(), &fake, Log::silent())
        .plan(&[])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "hetzner: GET /firewalls: rate_limit_exceeded (429): limit of 3600 requests per hour reached"
    );
    fake.set_rate_limited(false);
    let wrong = "wrong-token-value-that-must-not-leak";
    let http: Arc<dyn HttpClient> = fake.clone();
    let stranger = Hetzner::new(plain(), wrong.to_string(), http, Log::silent());
    let error = stranger.plan(&[]).await.unwrap_err().to_string();
    assert_eq!(
        error,
        "hetzner: GET /firewalls: unauthorized (401): unable to authenticate"
    );
    let error = stranger
        .stop("hetzner:contract-ci-00000000")
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains(wrong), "{error}");
    let outputs = stranger.apply(&[]).await.unwrap();
    assert_eq!(outputs[TOKEN_VAR], wrong, "apply hands the token on");
    let error = stranger
        .apply(&[Step::new("vm:security-group", Action::Create, "x")])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "hetzner: unknown step vm:security-group");
}

/// A main provider that serves no pool and shares the agent and cache location, as Scaleway
/// does (D38, M4 rule R11).
struct Main {
    started: Mutex<Vec<String>>,
}

#[async_trait]
impl Provider for Main {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            name: "main".into(),
            docker_daemon: true,
            max_vcpu: 8,
            max_memory_mib: 16_384,
            max_storage_mib: 10_240,
            max_duration_s: 86_400,
            archs: vec!["amd64".into()],
            billing: ciburst_core::types::Billing::PerHour,
        }
    }
    async fn plan(&self, _rotate: &[Rotation]) -> ciburst_core::Result<Vec<Step>> {
        Ok(vec![Step::new("project", Action::Noop, "project")])
    }
    async fn apply(&self, steps: &[Step]) -> ciburst_core::Result<Outputs> {
        assert!(steps.iter().all(|s| !s.id.starts_with("hetzner:")));
        Ok(BTreeMap::from([(
            "CIBURST_PROJECT_ID".into(),
            "p-1".into(),
        )]))
    }
    async fn destroy(&self) -> ciburst_core::Result<()> {
        Ok(())
    }
    async fn start(&self, request: StartRequest) -> ciburst_core::Result<String> {
        self.started.lock().unwrap().push(request.pool.tag.clone());
        Ok("main-run".into())
    }
    async fn list(&self, _pool: Option<&str>) -> ciburst_core::Result<Vec<Run>> {
        Ok(Vec::new())
    }
    async fn stop(&self, _run_id: &str) -> ciburst_core::Result<()> {
        Ok(())
    }
    async fn shared_env(&self) -> ciburst_core::Result<BTreeMap<String, String>> {
        Ok([
            ("CIBURST_AGENT_URL", AGENT_URL),
            ("CIBURST_AGENT_SHA256", AGENT_SHA256),
            ("CIBURST_RUNNER_URL", RUNNER_URL),
            ("CACHE_TYPE", "s3"),
            ("CACHE_S3_SERVER_ADDRESS", "s3.example.invalid"),
            ("CACHE_S3_BUCKET_NAME", "contract-cache-p-1"),
            ("CACHE_S3_BUCKET_LOCATION", "fr-par"),
            ("CACHE_SHARED", "true"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect())
    }
}

#[tokio::test]
async fn the_composite_routes_steps_starts_and_run_ids_here() {
    let mixed = config(|c| c.pools[1].runtime = None);
    let fake = Arc::new(FakeHetzner::new());
    let main = Arc::new(Main {
        started: Mutex::new(Vec::new()),
    });
    let hetzner: Arc<dyn Provider> = Arc::new(provider(mixed.clone(), &fake, Log::silent()));
    let composite = Composite::new(
        mixed.clone(),
        main.clone(),
        vec![("hetzner".to_string(), hetzner)],
        Log::silent(),
    );
    let steps = composite.plan(&[]).await.unwrap();
    assert_eq!(
        actions(&steps),
        [
            ("project".to_string(), Action::Noop),
            (FIREWALL_STEP.to_string(), Action::Create),
        ]
    );
    let outputs = composite.apply(&steps).await.unwrap();
    assert_eq!(outputs[TOKEN_VAR], FAKE_TOKEN);
    assert_eq!(outputs["CIBURST_PROJECT_ID"], "p-1");

    let mut start = request(&mixed, "ci");
    for key in [
        "CIBURST_AGENT_URL",
        "CIBURST_AGENT_SHA256",
        "CIBURST_RUNNER_URL",
        "CACHE_TYPE",
        "CACHE_S3_SERVER_ADDRESS",
        "CACHE_S3_BUCKET_NAME",
        "CACHE_S3_BUCKET_LOCATION",
        "CACHE_SHARED",
    ] {
        start.env.remove(key);
    }
    let run_id = composite.start(start).await.unwrap();
    assert!(run_id.starts_with("hetzner:contract-ci-"), "{run_id}");
    let user_data = fake.user_data(name_of(&run_id)).unwrap();
    assert!(user_data.contains(&format!("curl -fsSL -o /tmp/ciburst-vm-agent {AGENT_URL} ")));
    assert!(user_data.contains(&format!("curl -fsSL -o /tmp/gitlab-runner {RUNNER_URL} ")));
    let vm_env = vm_env(&user_data);
    assert_eq!(vm_env["CACHE_S3_BUCKET_NAME"], "contract-cache-p-1");
    assert_eq!(
        composite.start(request(&mixed, "build")).await.unwrap(),
        "main-run"
    );
    assert_eq!(*main.started.lock().unwrap(), ["build"]);

    composite.claim(&run_id).await.unwrap();
    let report = VmReport {
        state: VmState::Idle,
        free: 2,
        slots: 2,
    };
    composite.report(&run_id, &report, 5).await.unwrap();
    let capacity = composite.capacity("ci").await.unwrap().unwrap();
    assert_eq!(capacity[0].run_id, run_id);
    assert_eq!(capacity[0].seen, Some(5));
    assert_eq!(composite.sweep(10).await.unwrap(), Vec::<String>::new());
    composite.stop(&run_id).await.unwrap();
    assert!(fake.servers().is_empty());
    composite.destroy().await.unwrap();
    assert!(fake.firewalls().is_empty());
}
