//! The `instances` runtime against the fake Instance API: plan, start with type fallback, the
//! cloud-init user data, capacity, claim, report, list, stop, sweep and destroy.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ciburst_core::config::Config;
use ciburst_core::log::Log;
use ciburst_core::testing::contract::contract_config;
use ciburst_core::types::{
    Action, Billing, Provider, RunState, StartRequest, Step, VmReport, VmState,
};
use ciburst_core::vm::{
    SWEEP_DRAINING_S, SWEEP_MAX_AGE_S, SWEEP_SILENT_S, SWEEP_STARTING_S, verify,
};
use ciburst_scaleway::provider::Scaleway;
use ciburst_scaleway::testing::instance_api::FAKE_CLOCK;
use ciburst_scaleway::testing::scaleway_fake::{FAKE_ACCESS_KEY, ScalewayFake};
use serde_json::{Value, json};
use yaml_rust2::{Yaml, YamlLoader};

const SECRET: &str = "vm-secret-must-stay-in-the-dispatcher";
const RUNNER_TOKEN: &str = "glrt-vm-runner-token";
const CACHE_SECRET: &str = "cache-secret-key-value";
const DISPATCH_URL: &str = "https://dispatch.example";
const AGENT_IMAGE: &str = "rg.fr-par.scw.cloud/contract/vm-agent:edge";

/// The contract config on the default runtime, with the public registry the agent comes from.
fn config(change: impl FnOnce(&mut Config)) -> Arc<Config> {
    let mut config = (*contract_config()).clone();
    config.provider.extra.insert("registry".into(), json!(true));
    config
        .provider
        .extra
        .insert("registryPublic".into(), json!(true));
    change(&mut config);
    Arc::new(config)
}

fn plain() -> Arc<Config> {
    config(|_| {})
}

fn provider(config: Arc<Config>, backend: &Arc<ScalewayFake>, log: Log) -> Scaleway {
    Scaleway::new(config, backend.client(), log).with_s3_access_key(FAKE_ACCESS_KEY)
}

async fn provisioned(config: Arc<Config>, log: Log) -> (Arc<ScalewayFake>, Scaleway, String) {
    let backend = Arc::new(ScalewayFake::new());
    let provider = provider(config, &backend, log);
    let steps = provider.plan(&[]).await.expect("plan");
    let outputs = provider.apply(&steps).await.expect("apply");
    let project_id = outputs["CIBURST_PROJECT_ID"].clone();
    (backend, provider, project_id)
}

fn env() -> BTreeMap<String, String> {
    [
        ("RUNNER_TOKEN", RUNNER_TOKEN),
        ("CACHE_S3_ACCESS_KEY", "SCWCACHEACCESS000000"),
        ("CACHE_S3_SECRET_KEY", CACHE_SECRET),
        ("CIBURST_VM_SECRET", SECRET),
        ("CIBURST_DISPATCH_URL", DISPATCH_URL),
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

fn tags(server: &Value) -> Vec<String> {
    server["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// `vm.env` of a cloud-config document, parsed as YAML, as key and value pairs.
fn vm_env(document: &Yaml) -> BTreeMap<String, String> {
    let file = document["write_files"]
        .as_vec()
        .unwrap()
        .iter()
        .find(|f| f["path"].as_str() == Some("/etc/ciburst/vm.env"))
        .expect("vm.env in write_files");
    assert_eq!(file["permissions"].as_str(), Some("0600"));
    file["content"]
        .as_str()
        .unwrap()
        .lines()
        .map(|line| {
            let (key, value) = line.split_once('=').expect("KEY=VALUE");
            (key.to_string(), value.to_string())
        })
        .collect()
}

fn runcmd(document: &Yaml) -> Vec<String> {
    document["runcmd"]
        .as_vec()
        .unwrap()
        .iter()
        .map(|c| {
            c.as_str()
                .expect("runcmd entries are shell strings")
                .to_string()
        })
        .collect()
}

fn parse(user_data: &str) -> Yaml {
    assert!(user_data.starts_with("#cloud-config\n"));
    let mut documents = YamlLoader::load_from_str(user_data).expect("valid YAML");
    assert_eq!(documents.len(), 1);
    documents.remove(0)
}

#[tokio::test]
async fn first_plan_lists_the_security_group_and_no_job_definition() {
    let backend = Arc::new(ScalewayFake::new());
    let provider = provider(plain(), &backend, Log::silent());
    let steps = provider.plan(&[]).await.unwrap();
    assert_eq!(
        actions(&steps),
        [
            ("project".to_string(), Action::Create),
            ("key:dispatch".to_string(), Action::Create),
            ("key:cache".to_string(), Action::Create),
            ("bucket".to_string(), Action::Create),
            ("registry".to_string(), Action::Create),
            ("vm:security-group".to_string(), Action::Create),
        ]
    );
    assert_eq!(steps[5].resource, "security group contract-vm");
    assert!(backend.writes().is_empty(), "plan writes nothing");

    provider.apply(&steps).await.unwrap();
    let groups = backend.instance.security_groups();
    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    assert_eq!(group["name"], "contract-vm");
    assert_eq!(group["inbound_default_policy"], "drop");
    assert_eq!(group["outbound_default_policy"], "accept");
    assert_eq!(group["stateful"], true);
    assert_eq!(group["project_default"], false);
    assert_eq!(group["zone"], "fr-par-1");
    assert!(backend.jobs.definitions().is_empty());
    let again = provider.plan(&[]).await.unwrap();
    assert!(again.iter().all(Step::is_noop), "{again:?}");
    assert_eq!(again.len(), steps.len());
}

#[tokio::test]
async fn the_zone_comes_from_the_provider_section() {
    let (backend, _, _) = provisioned(
        config(|c| {
            c.provider.extra.insert("zone".into(), json!("fr-par-2"));
        }),
        Log::silent(),
    )
    .await;
    assert_eq!(backend.instance.security_groups()[0]["zone"], "fr-par-2");
}

#[tokio::test]
async fn start_creates_the_server_of_rule_four_then_user_data_then_poweron() {
    let (log, lines) = Log::memory();
    let (backend, provider, project_id) = provisioned(plain(), log).await;
    let config = plain();
    let id = provider.start(request(&config, "ci")).await.unwrap();

    let writes: Vec<_> = backend
        .instance
        .calls()
        .into_iter()
        .filter(|c| c.method != "GET")
        .collect();
    let paths: Vec<(String, String)> = writes
        .iter()
        .map(|c| (c.method.clone(), c.path.clone()))
        .filter(|(_, p)| p.contains("/servers"))
        .collect();
    let zone = "/instance/v1/zones/fr-par-1";
    assert_eq!(
        paths,
        [
            ("POST".to_string(), format!("{zone}/servers")),
            (
                "PATCH".to_string(),
                format!("{zone}/servers/{id}/user_data/cloud-init")
            ),
            ("POST".to_string(), format!("{zone}/servers/{id}/action")),
        ]
    );
    let create = writes
        .iter()
        .find(|c| c.path.ends_with("/servers"))
        .unwrap()
        .body
        .clone()
        .unwrap();
    let group_id = backend.instance.security_groups()[0]["id"].clone();
    let name = create["name"].as_str().unwrap();
    let suffix = name.strip_prefix("contract-ci-").expect("name prefix");
    assert!(suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(create["commercial_type"], "DEV1-M");
    assert_eq!(create["project"], project_id.as_str());
    assert_eq!(create["dynamic_ip_required"], true);
    assert_eq!(create["routed_ip_enabled"], true);
    assert_eq!(create["security_group"], group_id);
    assert!(
        create.get("volumes").is_none(),
        "root volume from the image"
    );
    assert_eq!(
        create["tags"],
        json!([
            "ciburst",
            "contract",
            "pool:ci",
            "job:2",
            "ciburst-state:starting",
            "ciburst-free:1"
        ])
    );
    let action = writes.last().unwrap().body.clone().unwrap();
    assert_eq!(action, json!({ "action": "poweron" }));

    let server = backend.instance.server(&id).unwrap();
    assert_eq!(server["state"], "starting");
    assert_eq!(server["image"]["name"], "docker");
    assert_eq!(server["volumes"]["0"]["volume_type"], "l_ssd");

    let user_data = backend.instance.user_data(&id, "cloud-init").unwrap();
    let document = parse(&user_data);
    let vm_env = vm_env(&document);
    let bucket = format!("contract-cache-{project_id}");
    let expected: BTreeMap<String, String> = [
        ("CIBURST_RUN_ID", id.as_str()),
        ("CIBURST_DISPATCH_URL", DISPATCH_URL),
        ("CI_SERVER_URL", "https://gitlab.example"),
        ("RUNNER_TOKEN", RUNNER_TOKEN),
        ("CIBURST_POOL", "ci"),
        ("CIBURST_SLOTS", "2"),
        ("CIBURST_DEFAULT_IMAGE", "example.invalid/node:24"),
        ("CIBURST_TIMEOUT_S", "3600"),
        ("CACHE_TYPE", "s3"),
        ("CACHE_S3_SERVER_ADDRESS", "s3.fr-par.scw.cloud"),
        ("CACHE_S3_BUCKET_NAME", bucket.as_str()),
        ("CACHE_S3_BUCKET_LOCATION", "fr-par"),
        ("CACHE_S3_ACCESS_KEY", "SCWCACHEACCESS000000"),
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
    assert!(verify(SECRET, &id, &vm_env["CIBURST_VM_TOKEN"]));
    assert!(!verify(
        SECRET,
        "another-server",
        &vm_env["CIBURST_VM_TOKEN"]
    ));

    let commands = runcmd(&document);
    assert_eq!(commands.len(), 4, "{commands:?}");
    assert!(commands[0].starts_with("test -f /swapfile || (fallocate -l 4G /swapfile"));
    let commands = &commands[1..];
    assert!(commands[0].starts_with(
        "curl -fsSL -o /tmp/gitlab-runner https://gitlab-runner-downloads.s3.amazonaws.com/v18.11.4/binaries/gitlab-runner-linux-amd64 && "
    ));
    assert!(commands[0].contains(
        "echo \"bed4af01315780cf27d259b9d7a74d20cc0a6fc0d34c9952205b59c244639fd1  /tmp/gitlab-runner\" | sha256sum -c -"
    ));
    assert!(
        commands[0].contains("install -m 0755 /tmp/gitlab-runner /usr/local/bin/gitlab-runner")
    );
    assert_eq!(
        commands[1],
        format!(
            "systemctl start docker && id=$(docker create {AGENT_IMAGE} /ciburst-vm-agent) && docker cp \"$id:/ciburst-vm-agent\" /usr/local/bin/ciburst-vm-agent && docker rm \"$id\""
        )
    );
    assert_eq!(
        commands[2],
        "systemctl daemon-reload && systemctl enable --now ciburst-vm-agent.service"
    );
    let unit = document["write_files"]
        .as_vec()
        .unwrap()
        .iter()
        .find(|f| f["path"].as_str() == Some("/etc/systemd/system/ciburst-vm-agent.service"))
        .expect("unit in write_files");
    let unit = unit["content"].as_str().unwrap();
    for line in [
        "EnvironmentFile=/etc/ciburst/vm.env",
        "ExecStart=/usr/local/bin/ciburst-vm-agent",
        "Restart=on-failure",
    ] {
        assert!(unit.lines().any(|l| l == line), "{line} in {unit}");
    }

    let everything = format!(
        "{user_data}\n{}\n{:?}\n{:?}",
        tags(&server).join(","),
        backend.instance.calls(),
        lines.lock().unwrap()
    );
    assert!(!everything.contains(SECRET), "the VM secret leaked");
    let lines = lines.lock().unwrap();
    assert!(lines.iter().any(|l| l.starts_with("info started server")));
    for line in lines.iter() {
        for secret in [
            RUNNER_TOKEN,
            CACHE_SECRET,
            vm_env["CIBURST_VM_TOKEN"].as_str(),
        ] {
            assert!(!line.contains(secret), "{line}");
        }
    }
}

#[tokio::test]
async fn a_server_starts_with_the_slots_the_dispatcher_already_took() {
    let config = config(|c| c.pools[0].slots = 3);
    let (backend, provider, _) = provisioned(config.clone(), Log::silent()).await;
    let free = |id: &str| {
        tags(&backend.instance.server(id).unwrap())
            .into_iter()
            .find(|t| t.starts_with("ciburst-free:"))
            .unwrap()
    };
    for (slots, taken, expected) in [
        (3, 2, "ciburst-free:1"),
        (2, 2, "ciburst-free:0"),
        (3, 3, "ciburst-free:0"),
        (2, 5, "ciburst-free:0"),
        (2, 0, "ciburst-free:1"),
        (3, 1, "ciburst-free:2"),
    ] {
        let mut start = request(&config, "ci");
        start.pool.slots = slots;
        start.taken = taken;
        let id = provider.start(start).await.unwrap();
        assert_eq!(free(&id), expected, "slots {slots}, taken {taken}");
        let user_data = backend.instance.user_data(&id, "cloud-init").unwrap();
        assert_eq!(
            vm_env(&parse(&user_data))["CIBURST_SLOTS"],
            slots.to_string(),
            "the agent still runs every slot"
        );
    }
}

#[tokio::test]
async fn start_falls_back_to_the_next_type_when_out_of_stock() {
    let config = config(|c| {
        c.pools[0].instance_types = strings(&["DEV1-M", "DEV1-L"]);
        c.pools[0].slots = 3;
    });
    let (log, lines) = Log::memory();
    let (backend, provider, _) = provisioned(config.clone(), log).await;
    backend.instance.set_out_of_stock("DEV1-M");
    let id = provider.start(request(&config, "ci")).await.unwrap();
    let server = backend.instance.server(&id).unwrap();
    assert_eq!(server["commercial_type"], "DEV1-L");
    assert!(tags(&server).contains(&"ciburst-free:2".to_string()));
    assert_eq!(backend.instance.servers().len(), 1);
    assert!(
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| { l.starts_with("warn instance type unavailable") && l.contains("DEV1-M") })
    );
}

#[tokio::test]
async fn start_falls_back_when_over_quota_and_names_every_failure() {
    let config = config(|c| c.pools[0].instance_types = strings(&["DEV1-M", "DEV1-L"]));
    let (backend, provider, _) = provisioned(config.clone(), Log::silent()).await;
    backend.instance.set_over_quota("DEV1-M");
    let id = provider.start(request(&config, "ci")).await.unwrap();
    assert_eq!(
        backend.instance.server(&id).unwrap()["commercial_type"],
        "DEV1-L"
    );
    backend.instance.set_out_of_stock("DEV1-L");
    let error = provider
        .start(request(&config, "ci"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway: pool ci: no instance type started (DEV1-M: Quota exceeded for this resource.; DEV1-L: out of stock)"
    );
    assert_eq!(backend.instance.servers().len(), 1);
}

#[tokio::test]
async fn the_default_types_fall_back_to_dev1_l() {
    let (backend, provider, _) = provisioned(plain(), Log::silent()).await;
    backend.instance.set_out_of_stock("DEV1-M");
    let id = provider.start(request(&plain(), "ci")).await.unwrap();
    let servers = backend.instance.servers();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["id"], id.as_str());
    assert_eq!(servers[0]["commercial_type"], "DEV1-L");

    backend.instance.set_out_of_stock("DEV1-L");
    let error = provider
        .start(request(&plain(), "ci"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway: pool ci: no instance type started (DEV1-M: out of stock; DEV1-L: out of stock)"
    );
}

#[tokio::test]
async fn out_of_stock_at_poweron_removes_the_server_and_tries_the_next_type() {
    let config = config(|c| c.pools[0].instance_types = strings(&["DEV1-M", "DEV1-L"]));
    let (backend, provider, _) = provisioned(config.clone(), Log::silent()).await;
    backend.instance.set_poweron_out_of_stock("DEV1-M");
    let id = provider.start(request(&config, "ci")).await.unwrap();
    let servers = backend.instance.servers();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["id"], id.as_str());
    assert_eq!(servers[0]["commercial_type"], "DEV1-L");
    let volumes = backend.instance.volumes();
    assert_eq!(volumes.len(), 1, "the stopped server's volume is deleted");
    assert_eq!(volumes[0]["server"]["id"], id.as_str());
}

#[tokio::test]
async fn without_the_docker_image_ubuntu_installs_docker_first() {
    let (backend, provider, _) = provisioned(plain(), Log::silent()).await;
    backend.instance.remove_local_images("docker");
    let id = provider.start(request(&plain(), "ci")).await.unwrap();
    let server = backend.instance.server(&id).unwrap();
    assert_eq!(server["image"]["name"], "ubuntu_noble");
    let document = parse(&backend.instance.user_data(&id, "cloud-init").unwrap());
    let commands = runcmd(&document);
    assert_eq!(commands.len(), 5, "{commands:?}");
    assert!(commands[0].starts_with("test -f /swapfile"));
    assert_eq!(
        commands[1],
        "apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y docker.io && systemctl enable --now docker"
    );
    assert!(commands[2].starts_with("curl -fsSL -o /tmp/gitlab-runner "));
}

#[tokio::test]
async fn start_refuses_what_a_vm_cannot_use() {
    let backend = Arc::new(ScalewayFake::new());
    let early = provider(plain(), &backend, Log::silent());
    let error = early
        .start(request(&plain(), "ci"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "scaleway: pool ci is not provisioned");

    let (backend, provider, _) = provisioned(plain(), Log::silent()).await;
    for (key, message) in [
        ("RUNNER_TOKEN", "scaleway: RUNNER_TOKEN missing"),
        (
            "CIBURST_VM_SECRET",
            "scaleway: pool ci: CIBURST_VM_SECRET missing",
        ),
        (
            "CIBURST_DISPATCH_URL",
            "scaleway: pool ci: CIBURST_DISPATCH_URL missing",
        ),
        (
            "CACHE_S3_SECRET_KEY",
            "scaleway: pool ci: CACHE_S3_SECRET_KEY missing",
        ),
    ] {
        let mut start = request(&plain(), "ci");
        start.env.remove(key);
        let error = provider.start(start).await.unwrap_err().to_string();
        assert_eq!(error, message);
    }

    let no_registry = config(|c| {
        c.provider.extra.insert("registry".into(), json!(false));
    });
    let error = Scaleway::new(no_registry.clone(), backend.client(), Log::silent())
        .start(request(&no_registry, "ci"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway: pool ci: the instances runtime pulls its agent from the registry namespace; set provider.registry"
    );

    let private = config(|c| {
        c.provider
            .extra
            .insert("registryPublic".into(), json!(false));
    });
    let private = provider_with(private, &backend);
    let steps = private.plan(&[]).await.unwrap();
    private.apply(&steps).await.unwrap();
    let error = private
        .start(request(&plain(), "ci"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway: pool ci: VMs pull the agent image without credentials; set provider.registryPublic"
    );
    assert!(backend.instance.servers().is_empty(), "nothing started");
}

fn provider_with(config: Arc<Config>, backend: &Arc<ScalewayFake>) -> Scaleway {
    provider(config, backend, Log::silent())
}

#[tokio::test]
async fn capacity_reads_the_pool_servers_and_ignores_other_deployments() {
    let config = config(|c| c.pools[0].slots = 2);
    let (backend, provider, project_id) = provisioned(config.clone(), Log::silent()).await;
    let first = provider.start(request(&config, "ci")).await.unwrap();
    backend.instance.set_clock(FAKE_CLOCK + 60);
    let second = provider.start(request(&config, "ci")).await.unwrap();
    let build = provider.start(request(&config, "build")).await.unwrap();
    let stopped = provider.start(request(&config, "ci")).await.unwrap();
    backend.instance.set_state(&stopped, "stopped");
    backend.instance.add_server(
        "other-ci-00000000",
        &project_id,
        &["ciburst", "other", "pool:ci", "ciburst-free:5"],
    );
    backend.instance.add_server(
        "contract-ci-manual",
        &project_id,
        &["contract", "pool:ci", "ciburst-free:5"],
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
}

#[tokio::test]
async fn jobs_pools_have_no_capacity() {
    let mixed = config(|c| c.pools[1].runtime = Some("jobs".into()));
    let (_, provider, _) = provisioned(mixed, Log::silent()).await;
    assert_eq!(provider.capacity("build").await.unwrap(), None);
    assert_eq!(provider.capacity("ci").await.unwrap(), Some(Vec::new()));
}

#[tokio::test]
async fn claim_lowers_free_slots_down_to_zero() {
    let config = config(|c| c.pools[0].slots = 2);
    let (backend, provider, _) = provisioned(config.clone(), Log::silent()).await;
    let id = provider.start(request(&config, "ci")).await.unwrap();
    let before = tags(&backend.instance.server(&id).unwrap());
    let free = || provider_free(&tags(&backend.instance.server(&id).unwrap()));
    assert_eq!(free(), Some(1), "the starting job's slot is taken");
    provider.claim(&id).await.unwrap();
    assert_eq!(free(), Some(0));
    provider.claim(&id).await.unwrap();
    assert_eq!(free(), Some(0));
    let after = tags(&backend.instance.server(&id).unwrap());
    assert_eq!(after.len(), before.len());
    for (old, new) in before.iter().zip(&after) {
        if !old.starts_with("ciburst-free:") {
            assert_eq!(old, new);
        }
    }
}

fn provider_free(tags: &[String]) -> Option<u32> {
    tags.iter()
        .find_map(|t| t.strip_prefix("ciburst-free:"))
        .and_then(|v| v.parse().ok())
}

#[tokio::test]
async fn report_rewrites_the_state_tags_and_keeps_every_other_tag() {
    let (backend, provider, _) = provisioned(plain(), Log::silent()).await;
    let id = provider.start(request(&plain(), "ci")).await.unwrap();
    let mut custom = tags(&backend.instance.server(&id).unwrap());
    custom.push("owner:ops".into());
    let custom: Vec<&str> = custom.iter().map(String::as_str).collect();
    backend.instance.set_tags(&id, &custom);

    let draining = VmReport {
        state: VmState::Draining,
        free: 1,
        slots: 1,
    };
    provider.report(&id, &draining, 1_000).await.unwrap();
    provider.report(&id, &draining, 1_060).await.unwrap();
    let after: BTreeSet<String> = tags(&backend.instance.server(&id).unwrap())
        .into_iter()
        .collect();
    let expected: BTreeSet<String> = strings(&[
        "ciburst",
        "contract",
        "pool:ci",
        "job:2",
        "owner:ops",
        "ciburst-state:draining",
        "ciburst-free:1",
        "ciburst-seen:1060",
        "ciburst-drain:1000",
    ])
    .into_iter()
    .collect();
    assert_eq!(after, expected);

    let idle = VmReport {
        state: VmState::Idle,
        free: 1,
        slots: 1,
    };
    provider.report(&id, &idle, 1_100).await.unwrap();
    let after = tags(&backend.instance.server(&id).unwrap());
    assert!(after.contains(&"owner:ops".to_string()));
    assert!(after.contains(&"ciburst-state:idle".to_string()));
    assert!(!after.iter().any(|t| t.starts_with("ciburst-drain:")));
}

#[tokio::test]
async fn claim_report_and_stop_refuse_servers_of_others() {
    let (backend, provider, project_id) = provisioned(plain(), Log::silent()).await;
    let foreign = backend.instance.add_server(
        "other-ci-00000000",
        &project_id,
        &["ciburst", "other", "pool:ci", "ciburst-free:1"],
    );
    let idle = VmReport {
        state: VmState::Idle,
        free: 1,
        slots: 1,
    };
    let message = format!("scaleway: server {foreign} does not belong to contract");
    assert_eq!(
        provider.claim(&foreign).await.unwrap_err().to_string(),
        message
    );
    assert_eq!(
        provider
            .report(&foreign, &idle, 1)
            .await
            .unwrap_err()
            .to_string(),
        message
    );
    assert_eq!(
        provider.stop(&foreign).await.unwrap_err().to_string(),
        message
    );
    assert_eq!(
        provider.claim("missing").await.unwrap_err().to_string(),
        "scaleway: server missing not found"
    );
    assert_eq!(
        tags(&backend.instance.server(&foreign).unwrap()),
        strings(&["ciburst", "other", "pool:ci", "ciburst-free:1"])
    );
}

#[tokio::test]
async fn list_shows_servers_as_runs_and_stop_terminates() {
    let (backend, provider, _) = provisioned(plain(), Log::silent()).await;
    let id = provider.start(request(&plain(), "ci")).await.unwrap();
    let runs = provider.list(Some("ci")).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, id);
    assert_eq!(runs[0].pool, "ci");
    assert_eq!(runs[0].state, RunState::Queued);
    assert!(runs[0].created_at.starts_with("2026-10-24T00:00:00"));
    assert_eq!(
        runs[0].labels,
        BTreeMap::from([("job".to_string(), "2".to_string())])
    );
    assert_eq!(
        provider.list(None).await.unwrap()[0].state,
        RunState::Running
    );
    assert!(provider.list(Some("build")).await.unwrap().is_empty());

    provider.stop(&id).await.unwrap();
    let terminate = backend.instance.calls().into_iter().last().unwrap();
    assert_eq!(terminate.method, "POST");
    assert!(terminate.path.ends_with(&format!("/servers/{id}/action")));
    assert_eq!(terminate.body, Some(json!({ "action": "terminate" })));
    assert!(provider.list(None).await.unwrap().is_empty());
    assert!(
        backend.instance.volumes().is_empty(),
        "local volume deleted"
    );
    assert_eq!(
        provider.stop("missing").await.unwrap_err().to_string(),
        "scaleway: server missing not found"
    );
}

#[tokio::test]
async fn stop_deletes_a_server_that_never_powered_on() {
    let (backend, provider, _) = provisioned(plain(), Log::silent()).await;
    let id = provider.start(request(&plain(), "ci")).await.unwrap();
    backend.instance.set_state(&id, "stopped");
    provider.stop(&id).await.unwrap();
    assert!(backend.instance.servers().is_empty());
    assert!(backend.instance.volumes().is_empty());
    let deletes: Vec<String> = backend
        .instance
        .calls()
        .into_iter()
        .filter(|c| c.method == "DELETE")
        .map(|c| c.path)
        .collect();
    assert_eq!(deletes.len(), 2, "{deletes:?}");
    assert!(deletes[0].ends_with(&format!("/servers/{id}")));
    assert!(deletes[1].contains("/volumes/"));
}

#[tokio::test]
async fn mixed_pools_keep_jobs_on_serverless_jobs() {
    let mixed = config(|c| c.pools[1].runtime = Some("jobs".into()));
    let (backend, provider, _) = provisioned(mixed.clone(), Log::silent()).await;
    let steps = provider.plan(&[]).await.unwrap();
    let ids: Vec<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"vm:security-group"));
    assert!(ids.contains(&"definition:build"));
    assert!(!ids.contains(&"definition:ci"));

    let job = provider.start(request(&mixed, "build")).await.unwrap();
    let start = backend
        .jobs
        .calls()
        .into_iter()
        .find(|c| c.path.ends_with("/start"))
        .unwrap();
    assert!(
        start.body.unwrap()["environment_variables"]
            .get("CIBURST_VM_SECRET")
            .is_none()
    );
    let vm = provider.start(request(&mixed, "ci")).await.unwrap();
    let all: BTreeSet<String> = provider
        .list(None)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.run_id)
        .collect();
    assert_eq!(all, BTreeSet::from([job.clone(), vm.clone()]));
    provider.stop(&job).await.unwrap();
    assert_eq!(backend.jobs.runs()[0]["state"], "canceled");
    assert!(backend.instance.server(&vm).is_some());
}

#[tokio::test]
async fn sweep_applies_each_rule_at_its_boundary() {
    let (log, lines) = Log::memory();
    let (backend, provider, project_id) = provisioned(plain(), log).await;
    let now = FAKE_CLOCK + 200_000;
    let add = |name: &str, age: u64, state_tags: &[String]| {
        backend.instance.set_clock(now - age);
        let mut tags = strings(&["ciburst", "contract", "pool:ci"]);
        tags.extend(state_tags.iter().cloned());
        let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
        backend.instance.add_server(name, &project_id, &tags)
    };
    let state = |s: &str| format!("ciburst-state:{s}");
    let seen = |ago: u64| format!("ciburst-seen:{}", now - ago);
    let drain = |ago: u64| format!("ciburst-drain:{}", now - ago);

    let mut kept = BTreeSet::new();
    let mut swept = BTreeSet::new();
    kept.insert(add(
        "starting-at-limit",
        SWEEP_STARTING_S,
        &[state("starting")],
    ));
    swept.insert(add(
        "starting-late",
        SWEEP_STARTING_S + 1,
        &[state("starting")],
    ));
    kept.insert(add(
        "seen-at-limit",
        5_000,
        &[state("idle"), seen(SWEEP_SILENT_S)],
    ));
    swept.insert(add(
        "seen-long-ago",
        5_000,
        &[state("idle"), seen(SWEEP_SILENT_S + 1)],
    ));
    kept.insert(add("never-seen-at-limit", SWEEP_SILENT_S, &[state("idle")]));
    swept.insert(add("never-seen-late", SWEEP_SILENT_S + 1, &[state("idle")]));
    kept.insert(add(
        "draining-at-limit",
        1_000,
        &[state("draining"), drain(SWEEP_DRAINING_S)],
    ));
    swept.insert(add(
        "draining-late",
        1_000,
        &[state("draining"), drain(SWEEP_DRAINING_S + 1)],
    ));
    kept.insert(add(
        "draining-but-reporting",
        5_000,
        &[state("draining"), drain(4_000), seen(10)],
    ));
    kept.insert(add(
        "max-age-at-limit",
        SWEEP_MAX_AGE_S,
        &[state("busy"), seen(5)],
    ));
    swept.insert(add(
        "max-age-past",
        SWEEP_MAX_AGE_S + 1,
        &[state("busy"), seen(5)],
    ));
    let never_powered = add("never-powered", SWEEP_STARTING_S + 1, &[state("starting")]);
    backend.instance.set_state(&never_powered, "stopped");
    swept.insert(never_powered.clone());
    let stopping = add("already-stopping", SWEEP_MAX_AGE_S + 1, &[state("idle")]);
    backend.instance.set_state(&stopping, "stopping");
    kept.insert(stopping);
    backend.instance.set_clock(now - SWEEP_MAX_AGE_S * 2);
    let foreign = backend.instance.add_server(
        "other-ci-00000000",
        &project_id,
        &["ciburst", "other", "pool:ci", "ciburst-state:starting"],
    );

    let result: BTreeSet<String> = provider.sweep(now).await.unwrap().into_iter().collect();
    assert_eq!(result, swept);
    let left: BTreeSet<String> = backend
        .instance
        .servers()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect();
    let mut expected_left = kept.clone();
    expected_left.insert(foreign);
    assert_eq!(left, expected_left);
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
async fn unknown_runtimes_fail_plan_naming_the_pool() {
    let backend = Arc::new(ScalewayFake::new());
    let bad_pool = config(|c| c.pools[1].runtime = Some("vm".into()));
    let error = provider(bad_pool, &backend, Log::silent())
        .plan(&[])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway: pool build: runtime vm is not instances or jobs"
    );
    let bad_default = config(|c| {
        c.provider.extra.insert("runtime".into(), json!("lambda"));
    });
    let error = provider(bad_default, &backend, Log::silent())
        .plan(&[])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "scaleway: provider.runtime \"lambda\" is not instances or jobs"
    );
    assert!(backend.writes().is_empty());
}

#[tokio::test]
async fn switching_runtimes_moves_between_definitions_and_the_security_group() {
    let jobs_config = config(|c| {
        c.provider.extra.insert("runtime".into(), json!("jobs"));
    });
    let (backend, jobs, _) = provisioned(jobs_config, Log::silent()).await;
    assert_eq!(backend.jobs.definitions().len(), 2);
    assert!(backend.instance.security_groups().is_empty());

    let instances = provider(plain(), &backend, Log::silent());
    let steps = instances.plan(&[]).await.unwrap();
    let find = |steps: &[Step], id: &str| steps.iter().find(|s| s.id == id).map(|s| s.action);
    assert_eq!(find(&steps, "vm:security-group"), Some(Action::Create));
    assert_eq!(find(&steps, "definition:ci"), Some(Action::Delete));
    assert_eq!(find(&steps, "definition:build"), Some(Action::Delete));
    instances.apply(&steps).await.unwrap();
    assert!(backend.jobs.definitions().is_empty());
    assert_eq!(backend.instance.security_groups().len(), 1);

    let steps = jobs.plan(&[]).await.unwrap();
    assert_eq!(find(&steps, "vm:security-group"), Some(Action::Delete));
    assert_eq!(find(&steps, "definition:ci"), Some(Action::Create));
    jobs.apply(&steps).await.unwrap();
    assert!(backend.instance.security_groups().is_empty());
    assert_eq!(backend.jobs.definitions().len(), 2);
    let again = jobs.plan(&[]).await.unwrap();
    assert!(again.iter().all(Step::is_noop), "{again:?}");
    assert_eq!(find(&again, "vm:security-group"), None);
}

#[tokio::test]
async fn the_type_scaleway_jobs_keeps_serverless_jobs_unless_told_otherwise() {
    let backend = Arc::new(ScalewayFake::new());
    let legacy = config(|c| c.provider.kind = "scaleway-jobs".into());
    let legacy = provider(legacy, &backend, Log::silent());
    let ids: Vec<String> = legacy
        .plan(&[])
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert!(ids.contains(&"definition:ci".to_string()), "{ids:?}");
    assert!(!ids.contains(&"vm:security-group".to_string()), "{ids:?}");
    let caps = legacy.capabilities();
    assert_eq!(caps.name, "scaleway");
    assert!(!caps.docker_daemon);
    assert_eq!(caps.billing, Billing::PerSecond);

    let explicit = config(|c| {
        c.provider.kind = "scaleway-jobs".into();
        c.provider
            .extra
            .insert("runtime".into(), json!("instances"));
    });
    let ids: Vec<String> = provider(explicit, &backend, Log::silent())
        .plan(&[])
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert!(ids.contains(&"vm:security-group".to_string()), "{ids:?}");

    let current = config(|c| c.provider.kind = "scaleway".into());
    let caps = provider(current, &backend, Log::silent()).capabilities();
    assert!(caps.docker_daemon);
    assert_eq!(caps.billing, Billing::PerHour);
}

#[tokio::test]
async fn plan_warns_when_the_zone_has_no_quota_for_the_pool_types() {
    let backend = Arc::new(ScalewayFake::new());
    for name in ["cp_servers_type_DEV1_M", "cp_servers_type_DEV1_L"] {
        backend
            .iam
            .set_zone_quota(name, &[("fr-par-1", 0), ("nl-ams-1", 15), ("pl-waw-1", 5)]);
    }
    let (log, lines) = Log::memory();
    provider(plain(), &backend, log).plan(&[]).await.unwrap();
    let warning = lines
        .lock()
        .unwrap()
        .iter()
        .find(|l| l.starts_with("warn vm pools cannot start"))
        .cloned()
        .expect("warning");
    assert!(warning.contains("zone fr-par-1 has no quota for DEV1-M, DEV1-L"));
    assert!(warning.contains("quota in nl-ams-1, pl-waw-1"));

    backend
        .iam
        .set_zone_quota("cp_servers_type_DEV1_L", &[("fr-par-1", 2)]);
    let (log, lines) = Log::memory();
    provider(plain(), &backend, log).plan(&[]).await.unwrap();
    assert!(lines.lock().unwrap().is_empty());
}

#[tokio::test]
async fn plan_warns_when_vms_cannot_pull_their_agent() {
    let backend = Arc::new(ScalewayFake::new());
    let (log, lines) = Log::memory();
    let private = config(|c| {
        c.provider
            .extra
            .insert("registryPublic".into(), json!(false));
    });
    provider(private, &backend, log).plan(&[]).await.unwrap();
    assert!(
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("warn vm pools cannot start"))
    );
    let (log, lines) = Log::memory();
    provider(plain(), &backend, log).plan(&[]).await.unwrap();
    assert!(lines.lock().unwrap().is_empty());
}

#[tokio::test]
async fn listings_read_every_page_of_servers() {
    let (backend, provider, project_id) = provisioned(plain(), Log::silent()).await;
    for n in 0..150 {
        backend.instance.add_server(
            &format!("contract-ci-{n:08x}"),
            &project_id,
            &[
                "ciburst",
                "contract",
                "pool:ci",
                "ciburst-state:idle",
                "ciburst-free:1",
            ],
        );
    }
    assert_eq!(provider.list(None).await.unwrap().len(), 150);
    assert_eq!(provider.capacity("ci").await.unwrap().unwrap().len(), 150);
    let pages: Vec<String> = backend
        .instance
        .calls()
        .into_iter()
        .filter(|c| c.method == "GET" && c.path.ends_with("/servers"))
        .map(|c| c.query)
        .collect();
    assert!(pages.iter().any(|q| q.contains("page=2")), "{pages:?}");
    assert!(pages.iter().all(|q| q.contains("per_page=100")));
    assert!(pages.iter().all(|q| q.contains("tags=contract")));
}

#[tokio::test]
async fn a_vm_still_ends_after_its_pool_moved_to_jobs() {
    let (backend, instances, _) = provisioned(plain(), Log::silent()).await;
    let id = instances.start(request(&plain(), "ci")).await.unwrap();
    let jobs_config = config(|c| {
        c.provider.extra.insert("runtime".into(), json!("jobs"));
    });
    let jobs = provider(jobs_config, &backend, Log::silent());
    jobs.stop(&id).await.unwrap();
    assert!(backend.instance.servers().is_empty());
}

#[tokio::test]
async fn an_m2_dispatch_policy_gains_instances_access() {
    let (backend, provider, _) = provisioned(plain(), Log::silent()).await;
    backend.iam.set_policy_sets(
        "contract-dispatch",
        &["ServerlessJobsFullAccess", "ContainerRegistryReadOnly"],
    );
    let steps = provider.plan(&[]).await.unwrap();
    let step = steps.iter().find(|s| s.id == "key:dispatch").unwrap();
    assert_eq!(
        (step.action, step.detail.as_deref()),
        (Action::Update, Some("repair"))
    );
    provider.apply(&steps).await.unwrap();
    let policy = backend
        .iam
        .policies()
        .into_iter()
        .find(|p| p["name"] == "contract-dispatch")
        .unwrap();
    assert_eq!(
        policy["rules"][0]["permission_set_names"],
        json!([
            "ServerlessJobsFullAccess",
            "ContainerRegistryReadOnly",
            "InstancesFullAccess"
        ])
    );
}

#[tokio::test]
async fn destroy_removes_servers_and_the_security_group() {
    let (log, lines) = Log::memory();
    let (backend, provider, project_id) = provisioned(plain(), log).await;
    let running = provider.start(request(&plain(), "ci")).await.unwrap();
    let stopped = provider.start(request(&plain(), "build")).await.unwrap();
    backend.instance.set_state(&stopped, "stopped");
    let foreign =
        backend
            .instance
            .add_server("other-ci-00000000", &project_id, &["ciburst", "other"]);
    provider.destroy().await.unwrap();
    let left: Vec<String> = backend
        .instance
        .servers()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(left, [foreign]);
    assert!(backend.instance.volumes().is_empty());
    assert!(backend.instance.security_groups().is_empty());
    assert!(backend.account.projects().is_empty());
    let logged = lines.lock().unwrap().clone();
    assert!(
        logged
            .iter()
            .any(|l| l.starts_with("info terminated server") && l.contains(&running))
    );
    assert!(
        logged
            .iter()
            .any(|l| l.starts_with("info deleted security group"))
    );
    let steps = provider.plan(&[]).await.unwrap();
    assert!(
        steps.iter().all(|s| s.action == Action::Create),
        "{steps:?}"
    );
}

#[tokio::test]
async fn destroy_keeps_going_when_the_security_group_is_still_in_use() {
    let (log, lines) = Log::memory();
    let (backend, provider, project_id) = provisioned(plain(), log).await;
    let group = backend.instance.security_groups()[0]["id"].clone();
    let id = provider.start(request(&plain(), "ci")).await.unwrap();
    backend.instance.set_state(&id, "stopping");
    provider.destroy().await.unwrap();
    assert_eq!(backend.instance.security_groups()[0]["id"], group);
    assert!(backend.iam.applications().is_empty());
    assert!(
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("warn security group not deleted") && l.contains("in use"))
    );
    let _ = project_id;
}

#[tokio::test]
async fn the_fake_wants_user_data_as_plain_text_and_serves_the_marketplace_openly() {
    use ciburst_core::http::{HttpClient, Request};
    let (backend, provider, _) = provisioned(plain(), Log::silent()).await;
    let id = provider.start(request(&plain(), "ci")).await.unwrap();
    let json_body = Request::new(
        "PATCH",
        format!(
            "https://api.scaleway.com/instance/v1/zones/fr-par-1/servers/{id}/user_data/cloud-init"
        ),
    )
    .header(
        "X-Auth-Token",
        ScalewayFake::credentials().secret_key.clone(),
    )
    .json(&json!("#cloud-config"));
    assert_eq!(backend.send(json_body).await.unwrap().status, 400);
    let marketplace = Request::new(
        "GET",
        "https://api.scaleway.com/marketplace/v2/local-images?image_label=docker&zone=fr-par-1",
    );
    let response = backend.send(marketplace).await.unwrap();
    assert_eq!(response.status, 200);
    let body: Value = response.json().unwrap();
    assert_eq!(body["total_count"], 2);
}
