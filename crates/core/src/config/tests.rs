use super::*;
use crate::types::Billing;

fn valid() -> Value {
    serde_json::json!({
        "name": "demo",
        "gitlab": { "url": "https://gitlab.example/", "projectIds": [1, 2] },
        "host": { "type": "fake" },
        "provider": { "type": "fake", "region": "fr-par" },
        "vm": { "image": "img/node", "maxRuns": 3 },
        "lanes": [
            { "tag": "node-24", "image": "node:24-bookworm-slim", "jobs": ["lint", "unit-*"] },
            { "tag": "rust-1", "image": "rust:1.96.0-bookworm", "jobs": ["unit-rust", "clippy?"], "vcpu": 4 },
            { "tag": "ts-5", "image": "node:24-bookworm-slim", "jobs": ["*-ts"] }
        ],
        "cache": { "type": "s3" }
    })
}

fn errors_of(value: Value) -> Vec<String> {
    match parse_config(&value.to_string()) {
        Err(Error::Config(errors)) => errors,
        other => panic!("expected a config error, got {other:?}"),
    }
}

#[test]
fn applies_defaults_and_normalizes() {
    let config = parse_config(&valid().to_string()).unwrap();
    assert_eq!(config.gitlab.url, "https://gitlab.example");
    assert_eq!(config.vm.tag, "vm");
    assert_eq!(config.vm.runtime(), INSTANCES_RUNTIME);
    assert_eq!((config.vm.slots, config.vm.max_runs), (DEFAULT_SLOTS, 3));
    assert_eq!(config.lanes[0].vcpu, DEFAULT_VCPU);
    assert_eq!(config.lanes[0].max_runs, DEFAULT_MAX_RUNS);
    assert_eq!(config.lanes[1].vcpu, 4);
    assert_eq!(
        config.cache,
        CacheConfig {
            kind: CacheType::S3,
            expire_days: DEFAULT_EXPIRE_DAYS
        }
    );
    assert_eq!(config.provider.extra["region"], "fr-par");
}

#[test]
fn a_file_without_vm_gets_the_default_vm_runner() {
    let mut raw = valid();
    raw.as_object_mut().unwrap().remove("vm");
    let config = parse_config(&raw.to_string()).unwrap();
    assert_eq!(config.vm, VmConfig::default());
    assert_eq!(config.vm.image, "ubuntu:24.04");
    assert_eq!((config.vm.slots, config.vm.max_runs), (2, 4));
    let back = serde_json::to_value(&config).unwrap();
    assert_eq!(back["vm"]["tag"], "vm");
}

#[test]
fn reports_structural_errors_from_serde() {
    let errors = errors_of(serde_json::json!({ "name": "x" }));
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("missing field"), "{errors:?}");
    let errors = errors_of(serde_json::json!([]));
    assert_eq!(errors.len(), 1);
    let mut raw = valid();
    raw["vm"]["jobs"] = serde_json::json!(["x"]);
    let errors = errors_of(raw);
    assert!(errors[0].contains("unknown field `jobs`"), "{errors:?}");
}

#[test]
fn a_refused_key_names_its_replacement() {
    let mut raw = valid();
    raw["pools"] = serde_json::json!([{ "tag": "vm", "image": "x" }]);
    raw["defaultPool"] = "vm".into();
    raw["spillRunner"] = true.into();
    raw["vm"]["stages"] = serde_json::json!(["test"]);
    raw["lanes"][1]["vmFirst"] = false.into();
    raw["lanes"][2]["stages"] = serde_json::json!(["build"]);
    assert_eq!(
        errors_of(raw),
        [
            "defaultPool: removed: a job that no lane and no ignoreJobs entry takes runs on the VM runner (vm)",
            "pools: replaced by vm (the VM runner) and lanes (Serverless Jobs lanes of this project)",
            "spillRunner: removed: the VM runner carries only its tag",
            "vm.stages: removed: lanes take jobs by name with their jobs patterns",
            "lanes[1].vmFirst: removed: lane jobs run only in their lane, the VM runner carries only its tag",
            "lanes[2].stages: removed: lanes take jobs by name with their jobs patterns",
        ]
    );
    let defaults = Config::ci_defaults(5, "https://gitlab.example");
    match merge_config(&defaults, r#"{ "pools": [] }"#) {
        Err(Error::Config(errors)) => assert!(errors[0].starts_with("pools: replaced by vm")),
        other => panic!("expected a config error, got {other:?}"),
    }
}

#[test]
fn an_unknown_top_level_key_is_refused_by_name() {
    let mut raw = valid();
    raw["ignorejobs"] = serde_json::json!(["pages"]);
    raw["lane"] = serde_json::json!([]);
    raw["stages"] = serde_json::json!(["test"]);
    let keys = "name, gitlab, host, provider, cache, vm, lanes, ignoreJobs";
    assert_eq!(
        errors_of(raw),
        [
            "stages: removed: lanes take jobs by name with their jobs patterns".to_string(),
            format!("ignorejobs: unknown key; the keys are {keys}"),
            format!("lane: unknown key; the keys are {keys}"),
        ]
    );

    let defaults = Config::ci_defaults(5, "https://gitlab.example");
    match merge_config(&defaults, r#"{ "ignorejobs": ["pages"] }"#) {
        Err(Error::Config(errors)) => assert_eq!(
            errors,
            [format!("ignorejobs: unknown key; the keys are {keys}")]
        ),
        other => panic!("expected a config error, got {other:?}"),
    }
    let merged = merge_config(&defaults, r#"{ "ignoreJobs": ["pages"] }"#).unwrap();
    assert_eq!(merged.ignore_jobs, ["pages"]);
}

#[test]
fn collects_every_semantic_error() {
    let mut raw = valid();
    raw["name"] = "Bad Name".into();
    raw["gitlab"] = serde_json::json!({ "url": "http://insecure", "projectIds": [] });
    raw["host"] = serde_json::json!({ "type": "Fake!" });
    raw["vm"] =
        serde_json::json!({ "tag": "Vm", "slots": 0, "runtime": "jobs", "maxQueueS": 3601 });
    raw["lanes"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({ "tag": "node-24", "image": "", "vcpu": 0, "jobs": [] }));
    raw["cache"] = serde_json::json!({ "type": "s3", "expireDays": 0 });
    let errors = errors_of(raw);
    let expect = |needle: &str| {
        assert!(
            errors.iter().any(|e| e.contains(needle)),
            "{needle} in {errors:?}"
        )
    };
    expect("name:");
    expect("gitlab.url");
    expect("gitlab.projectIds");
    expect("host.type");
    expect("vm.tag: lowercase");
    expect("vm.slots: positive integer");
    expect("vm.runtime: jobs is not instances; Scaleway Instances is the only VM runtime (D65)");
    expect("vm.maxQueueS: whole number 0 to 3600");
    expect("lanes[3].tag: duplicate node-24");
    expect("lanes[3].image: required");
    expect("lanes[3].vcpu: positive integer");
    expect("lanes[3].jobs: at least one job name pattern");
    expect("cache.expireDays");
}

#[test]
fn a_lane_never_takes_the_vm_runners_tag_or_a_lane_key_form() {
    let mut raw = valid();
    raw["lanes"][0]["tag"] = "vm".into();
    assert_eq!(errors_of(raw), ["lanes[0].tag: vm is the VM runner's tag"]);
    let mut raw = valid();
    raw["vm"]["tag"] = "12-node".into();
    assert_eq!(
        errors_of(raw),
        ["vm.tag: 12-node starts with digits and a hyphen, the form of a lane's key"]
    );
}

#[test]
fn a_lane_whose_definition_name_passes_63_characters_is_refused() {
    let mut raw = valid();
    // demo-1- is 7 characters, so a tag of 56 fits and one of 57 does not.
    raw["lanes"][0]["tag"] = "a".repeat(56).into();
    assert!(parse_config(&raw.to_string()).is_ok());
    raw["lanes"][0]["tag"] = "a".repeat(57).into();
    assert_eq!(
        errors_of(raw),
        [format!(
            "lanes[0].tag: job definition name demo-1-{} is longer than 63 characters",
            "a".repeat(57)
        )]
    );
}

#[test]
fn job_names_route_to_the_first_matching_lane_else_the_vm_runner() {
    let config = parse_config(&valid().to_string()).unwrap();
    let tag = |name| match config.route(name) {
        Route::Lane(lane) => lane.tag.as_str(),
        Route::Vm => "vm",
        Route::Ignored => "ignored",
    };
    assert_eq!(tag("lint"), "node-24");
    assert_eq!(tag("unit-rust"), "node-24", "first lane in order");
    assert_eq!(tag("unit-"), "node-24", "* matches nothing too");
    assert_eq!(tag("clippy1"), "rust-1");
    assert_eq!(tag("clippy"), "vm", "? is one character");
    assert_eq!(tag("clippy12"), "vm");
    assert_eq!(tag("check-ts"), "ts-5");
    assert_eq!(tag("Lint"), "vm", "case matters");
    assert_eq!(tag("deploy"), "vm");
}

#[test]
fn ignore_jobs_route_nowhere_before_the_lanes() {
    let mut raw = valid();
    raw["ignoreJobs"] = serde_json::json!(["deploy-*", "lint", "pages"]);
    let config = parse_config(&raw.to_string()).unwrap();
    assert_eq!(config.route("lint"), Route::Ignored, "before the lane");
    assert_eq!(config.route("deploy-prod"), Route::Ignored);
    assert_eq!(config.route("pages"), Route::Ignored);
    assert!(matches!(config.route("unit-a"), Route::Lane(lane) if lane.tag == "node-24"));
    assert_eq!(config.route("deploy"), Route::Vm);
    assert!(config.ignores("deploy-") && !config.ignores("Lint"));
    let back = serde_json::to_value(&config).unwrap();
    assert_eq!(back["ignoreJobs"], raw["ignoreJobs"]);
    assert_eq!(merge_config(&config, "{}").unwrap(), config);

    raw["ignoreJobs"] = serde_json::json!(["lint", ""]);
    assert_eq!(errors_of(raw), ["ignoreJobs: empty pattern"]);
}

#[test]
fn glob_patterns_know_star_and_question_mark_only() {
    let cases = [
        ("*", "", true),
        ("*", "anything at all", true),
        ("?", "", false),
        ("?", "x", true),
        ("??", "x", false),
        ("a*b*c", "abc", true),
        ("a*b*c", "a-b-b-c", true),
        ("a*b*c", "a-b-b-cd", false),
        ("*b", "abab", true),
        ("*ab", "aab", true),
        ("test:*", "test: [node, 24]", true),
        ("unit ?/3", "unit 2/3", true),
        ("[ab]", "a", false),
        ("[ab]", "[ab]", true),
        ("a.c", "abc", false),
        ("ä?", "äö", true),
        ("lint", "lint ", false),
        ("**x", "x", true),
    ];
    for (pattern, name, expected) in cases {
        assert_eq!(glob_match(pattern, name), expected, "{pattern} {name}");
    }
}

#[test]
fn job_patterns_must_be_non_empty_and_in_one_lane() {
    let mut raw = valid();
    raw["lanes"][0]["jobs"] = serde_json::json!(["lint", "", "lint", "unit-*"]);
    raw["lanes"][1]["jobs"] = serde_json::json!(["lint", "unit-*", "clippy?"]);
    raw["lanes"][2]["jobs"] = serde_json::json!(["unit-*"]);
    assert_eq!(
        errors_of(raw),
        [
            "lanes[0].jobs: empty pattern",
            "lanes[1].jobs: lint is in lane node-24 and lane rust-1",
            "lanes[1].jobs: unit-* is in lane node-24 and lane rust-1",
            "lanes[2].jobs: unit-* is in lane node-24 and lane ts-5",
        ]
    );
}

#[test]
fn the_vm_runners_tags_hold_no_lane_tag() {
    let config = parse_config(&valid().to_string()).unwrap();
    assert_eq!(config.runner_tags(&config.vm_pool()), ["vm"]);
    for lane in &config.lanes {
        let pool = lane.pool(1);
        assert_eq!(pool.tag, format!("1-{}", lane.tag));
        assert_eq!(config.runner_tags(&pool), [lane.tag.as_str()]);
        assert!(config.is_lane(&pool) && !config.is_vm_pool(&pool));
    }
    let pools: Vec<String> = config.pools().into_iter().map(|p| p.tag).collect();
    assert_eq!(pools, ["vm", "1-node-24", "1-rust-1", "1-ts-5"]);
    assert!(config.is_vm_pool(&config.pool_by_tag("vm").unwrap()));
    assert_eq!(config.pool_by_tag("node-24"), None);
}

#[test]
fn lane_keys_name_project_and_tag() {
    assert_eq!(lane_key(42, "node-24"), "42-node-24");
    assert_eq!(parse_lane_key("42-node-24"), Some((42, "node-24")));
    for other in ["vm", "-x", "42-", "4a-x", "node-24"] {
        assert_eq!(parse_lane_key(other), None, "{other}");
    }
    assert_eq!(definition_name("ci", 42, "node"), "ci-42-node");
    let pool = Lane {
        tag: "7-x".into(),
        image: "i".into(),
        jobs: vec!["a".into()],
        vcpu: 1,
        memory_mib: 1,
        storage_mib: 1,
        timeout_s: 1,
        wait_timeout_s: 1,
        max_runs: 1,
    }
    .pool(3);
    assert_eq!((pool.tag.as_str(), pool.runner_tag()), ("3-7-x", "7-x"));
}

#[test]
fn max_queue_s_and_warm_start_belong_to_the_vm_runner() {
    let mut raw = valid();
    raw["vm"]["maxQueueS"] = 3600.into();
    raw["vm"]["warmStart"] = false.into();
    let config = parse_config(&raw.to_string()).unwrap();
    let vm = config.vm_pool();
    assert_eq!((vm.max_queue_s(), vm.warm_start()), (3600, false));
    let back = serde_json::to_value(&config).unwrap();
    assert_eq!(back["vm"]["maxQueueS"], 3600);
    assert_eq!(merge_config(&config, "{}").unwrap(), config);
    let defaults = Config::ci_defaults(5, "https://gitlab.example");
    assert_eq!(defaults.vm_pool().max_queue_s(), 90);
    assert!(defaults.vm_pool().warm_start());
    for bad in [
        serde_json::json!(-1),
        serde_json::json!(1.5),
        serde_json::json!("90"),
    ] {
        let mut raw = valid();
        raw["vm"]["maxQueueS"] = bad.clone();
        assert_eq!(errors_of(raw).len(), 1, "{bad}: serde refuses it");
    }
    let mut raw = valid();
    raw["lanes"][0]["maxQueueS"] = 10.into();
    assert!(errors_of(raw)[0].contains("unknown field `maxQueueS`"));
}

/// D65: Scaleway Instances is the only VM runtime; `providers` names its removal.
#[test]
fn the_vm_runner_runs_on_scaleway_instances_only() {
    let mut raw = valid();
    raw["vm"]["runtime"] = "instances".into();
    assert!(parse_config(&raw.to_string()).is_ok());
    raw["vm"]["runtime"] = "hetzner".into();
    assert_eq!(
        errors_of(raw),
        ["vm.runtime: hetzner is not instances; Scaleway Instances is the only VM runtime (D65)"]
    );
    let mut raw = valid();
    raw["providers"] = serde_json::json!({ "hetzner": {} });
    assert_eq!(
        errors_of(raw),
        [
            "providers: removed: Scaleway is the only provider (D65); the VM runner runs on Scaleway Instances"
        ]
    );
}

#[test]
fn checks_capabilities() {
    let config = parse_config(&valid().to_string()).unwrap();
    let caps = Capabilities {
        name: "tiny".into(),
        docker_daemon: false,
        max_vcpu: 2,
        max_memory_mib: 4096,
        max_storage_mib: 10240,
        max_duration_s: 600,
        archs: vec!["amd64".into()],
        billing: Billing::PerSecond,
    };
    assert_eq!(
        check_capabilities(&config, |_| caps.clone()),
        vec![
            "lanes[0].timeoutS: 3600 above tiny maximum 600",
            "lanes[1].vcpu: 4 above tiny maximum 2",
            "lanes[1].timeoutS: 3600 above tiny maximum 600",
            "lanes[2].timeoutS: 3600 above tiny maximum 600",
        ]
    );
    let roomy = Capabilities {
        max_vcpu: 6,
        max_duration_s: 86400,
        ..caps.clone()
    };
    assert!(check_capabilities(&config, |_| roomy.clone()).is_empty());
}

#[test]
fn ci_defaults_are_the_vm_runner_on_scaleway() {
    let defaults = Config::ci_defaults(4711, "https://gitlab.example");
    let config = validate(defaults.clone()).unwrap();
    assert_eq!(config, defaults);
    assert_eq!(config.name, "ci-4711");
    assert_eq!(config.gitlab.project_ids, [4711]);
    assert_eq!(config.host.kind, "scaleway-function");
    assert_eq!(config.provider.kind, "scaleway");
    assert_eq!(config.provider.extra.len(), 1, "no registry key");
    assert_eq!(config.vm.tag, "vm");
    assert_eq!(config.vm.runtime.as_deref(), Some("instances"));
    assert_eq!(config.vm.image, "ubuntu:24.04");
    assert_eq!((config.vm.slots, config.vm.max_runs), (2, 4));
    assert_eq!(config.vm.instance_types, ["DEV1-M", "DEV1-L"]);
    assert!(config.lanes.is_empty());
    assert_eq!(config.pools().len(), 1);
}

#[test]
fn a_file_replaces_whole_top_level_keys_of_the_defaults() {
    let defaults = Config::ci_defaults(5, "https://gitlab.example");
    assert_eq!(merge_config(&defaults, "{}").unwrap(), defaults);

    let merged = merge_config(
        &defaults,
        r#"{
          "provider": { "type": "scaleway", "zone": "nl-ams-1" },
          "vm": { "slots": 1 },
          "lanes": [ { "tag": "rust", "image": "img/rust", "jobs": ["cargo-*"] } ]
        }"#,
    )
    .unwrap();
    assert_eq!(merged.provider.extra.get("region"), None, "replaced whole");
    assert_eq!(merged.provider.extra["zone"], "nl-ams-1");
    assert_eq!(merged.vm.slots, 1);
    assert!(merged.vm.instance_types.is_empty(), "no default kept");
    assert_eq!(merged.lanes.len(), 1);
    assert_eq!(merged.name, defaults.name);
    assert_eq!(merged.gitlab, defaults.gitlab);
    assert_eq!(merged.cache, defaults.cache);

    let named = merge_config(&defaults, r#"{ "name": "shop" }"#).unwrap();
    assert_eq!(named.name, "shop");

    match merge_config(&defaults, r#"{ "cache": { "type": "none" } }"#) {
        Err(Error::Config(errors)) => {
            assert_eq!(
                errors,
                ["cache.type: s3 required; the runner object lives in the bucket"]
            )
        }
        other => panic!("expected a config error, got {other:?}"),
    }
    for bad in ["[]", "{", r#"{ "lanes": 3 }"#] {
        assert!(
            matches!(merge_config(&defaults, bad), Err(Error::Config(_))),
            "{bad}"
        );
    }
}

#[test]
fn the_deployment_view_leaves_out_the_routing_keys() {
    let mut raw = valid();
    raw["ignoreJobs"] = serde_json::json!(["pages"]);
    let config = parse_config(&raw.to_string()).unwrap();
    let mut changed = config.clone();
    changed.lanes[0].jobs = vec!["other".into()];
    changed.lanes[0].max_runs = 1;
    changed.vm.slots = 1;
    changed.vm.max_runs = 9;
    changed.ignore_jobs.clear();
    assert_eq!(changed.deployment_view(), config.deployment_view());
    changed.lanes[0].image = "other".into();
    assert_ne!(changed.deployment_view(), config.deployment_view());
}

#[test]
fn every_config_needs_an_s3_cache() {
    let needs = ["cache.type: s3 required; the runner object lives in the bucket"];
    let mut raw = valid();
    raw.as_object_mut().unwrap().remove("cache");
    assert_eq!(errors_of(raw.clone()), needs);
    raw["cache"] = serde_json::json!({ "type": "none" });
    assert_eq!(errors_of(raw.clone()), needs);
    raw["cache"] = serde_json::json!({ "type": "s3" });
    assert!(parse_config(&raw.to_string()).is_ok());
}

#[test]
fn a_members_file_holds_lanes_and_ignore_jobs_only() {
    let file = r#"{
      "lanes": [ { "tag": "node", "image": "node:24-alpine", "jobs": ["*"], "vcpu": 1 } ],
      "ignoreJobs": ["pages"]
    }"#;
    let project = parse_project_config(file, "ci-1", "vm", 77).unwrap();
    assert_eq!(project.lanes[0].vcpu, 1);
    assert_eq!(project.ignore_jobs, ["pages"]);
    assert!(matches!(project.route("build"), Route::Lane(lane) if lane.tag == "node"));
    assert_eq!(project.route("pages"), Route::Ignored);
    assert_eq!(
        parse_project_config("{}", "ci-1", "vm", 77).unwrap(),
        ProjectConfig::default()
    );

    let fleet = r#"{ "name": "x", "vm": { "slots": 1 }, "cache": {}, "lanes": [] }"#;
    match parse_project_config(fleet, "ci-1", "vm", 77) {
        Err(Error::Config(errors)) => assert_eq!(
            errors,
            [
                "cache: a fleet setting; it belongs in the host's ciburst.json",
                "name: a fleet setting; it belongs in the host's ciburst.json",
                "vm: a fleet setting; it belongs in the host's ciburst.json",
            ]
        ),
        other => panic!("expected a config error, got {other:?}"),
    }
    let bad =
        r#"{ "lanes": [ { "tag": "vm", "image": "x", "jobs": ["a"] } ], "ignoreJobs": [""] }"#;
    match parse_project_config(bad, "ci-1", "vm", 77) {
        Err(Error::Config(errors)) => assert_eq!(
            errors,
            [
                "lanes[0].tag: vm is the VM runner's tag",
                "ignoreJobs: empty pattern"
            ]
        ),
        other => panic!("expected a config error, got {other:?}"),
    }
    match parse_project_config(r#"{ "pools": [] }"#, "ci-1", "vm", 77) {
        Err(Error::Config(errors)) => assert!(errors[0].starts_with("pools: replaced by vm")),
        other => panic!("expected a config error, got {other:?}"),
    }
    let typo = r#"{ "ignorejobs": ["pages"], "vmFirst": false, "vm": {} }"#;
    match parse_project_config(typo, "ci-1", "vm", 77) {
        Err(Error::Config(errors)) => assert_eq!(
            errors,
            [
                "vmFirst: removed: lane jobs run only in their lane, the VM runner carries only its tag",
                "vm: a fleet setting; it belongs in the host's ciburst.json",
                "ignorejobs: unknown key; the keys are lanes, ignoreJobs",
            ]
        ),
        other => panic!("expected a config error, got {other:?}"),
    }
}

#[test]
fn two_projects_with_the_same_lanes_route_by_their_own_patterns_only() {
    let a: ProjectConfig = serde_json::from_value(serde_json::json!({
        "lanes": [ { "tag": "node", "image": "node:24", "jobs": ["lint"] } ]
    }))
    .unwrap();
    let b: ProjectConfig = serde_json::from_value(serde_json::json!({
        "lanes": [ { "tag": "node", "image": "node:24", "jobs": ["lint", "e2e"] } ]
    }))
    .unwrap();
    assert!(matches!(a.route("lint"), Route::Lane(_)));
    assert!(matches!(b.route("e2e"), Route::Lane(_)));
    assert_eq!(a.route("e2e"), Route::Vm, "B's pattern never applies to A");
    assert_ne!(a.lanes[0].pool(1).tag, b.lanes[0].pool(2).tag);
}

#[tokio::test]
async fn a_missing_config_file_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.json");
    let error = load_config(&path).await.unwrap_err().to_string();
    assert_eq!(error, format!("config file {} not found", path.display()));
}
