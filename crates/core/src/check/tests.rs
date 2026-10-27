use super::*;
use serde_json::json;

const VM: &str = "vm";

fn project(value: serde_json::Value) -> ProjectConfig {
    serde_json::from_value(value).unwrap()
}

fn plain() -> ProjectConfig {
    ProjectConfig::default()
}

fn lanes() -> ProjectConfig {
    project(json!({
        "lanes": [
            { "tag": "node-24", "image": "node:24-bookworm-slim", "jobs": ["lint", "unit-*"] },
            { "tag": "build", "image": "img/kaniko", "jobs": ["image-*"] }
        ]
    }))
}

fn job(name: &str, stage: Option<&str>, tags: &[&str]) -> LintJob {
    LintJob {
        name: name.into(),
        stage: stage.map(String::from),
        tag_list: tags.iter().map(|t| t.to_string()).collect(),
    }
}

fn summary(problems: &[Problem]) -> Vec<(&str, &str, &str)> {
    problems
        .iter()
        .map(|p| (p.job.as_str(), p.runner.as_str(), p.problem.as_str()))
        .collect()
}

fn pipeline(root: serde_json::Value) -> Pipeline {
    Pipeline::from_value(&root).unwrap()
}

#[test]
fn skips_the_components_own_jobs() {
    let problems = check_jobs(
        VM,
        &plain(),
        &[
            job("ciburst-setup", Some(".pre"), &["saas-linux-small-amd64"]),
            job("ciburst-check", Some(".pre"), &["vm"]),
            job("ciburst-destroy", Some("build"), &[]),
            job("ciburst", Some("build"), &[]),
        ],
        &Pipeline::default(),
    );
    let names: Vec<&str> = problems.iter().map(|p| p.job.as_str()).collect();
    assert_eq!(names, ["ciburst"]);
    assert!(is_own_job("ciburst-setup"));
    assert!(!is_own_job("ciburst"));
    assert!(!is_own_job("my-ciburst-job"));
}

#[test]
fn accepts_matching_tags_and_ignored_foreign_ones() {
    let mut settings = lanes();
    settings.ignore_jobs = vec!["hosted-*".into()];
    let jobs = [
        job("unit", Some("test"), &["vm"]),
        job("lint", Some("test"), &["node-24"]),
        job("image-a", Some("build"), &["build"]),
        job("hosted-a", Some("test"), &["saas-linux-small-amd64"]),
        job("hosted-b", Some("test"), &[]),
    ];
    let problems = check_jobs(VM, &settings, &jobs, &Pipeline::default());
    assert!(problems.is_empty(), "{problems:?}");
    assert!(suggest_ignore_jobs(VM, &settings, &jobs).is_empty());
}

#[test]
fn ignore_jobs_must_cover_the_jobs_of_other_runners_and_only_them() {
    let mut settings = lanes();
    settings.ignore_jobs = vec!["pages".into(), "release*".into()];
    let jobs = [
        job("hosted", Some("test"), &["saas-linux-small-amd64"]),
        job("macos", Some("build"), &["saas-macos-medium-m1", "macos"]),
        job("pages", Some("deploy"), &["saas-linux-small-amd64"]),
        job("release-notes", Some("deploy"), &["vm"]),
        job("release-tag", Some("deploy"), &["build", "vm"]),
        job("release-plain", Some("deploy"), &[]),
        job("ciburst-setup", Some(".pre"), &["saas-linux-small-amd64"]),
    ];
    let problems = check_jobs(VM, &settings, &jobs, &Pipeline::default());
    assert_eq!(
        summary(&problems),
        vec![
            (
                "hosted",
                "vm",
                "runs on other runners (tags saas-linux-small-amd64) but ignoreJobs misses it: its pending events start runs"
            ),
            (
                "macos",
                "vm",
                "runs on other runners (tags saas-macos-medium-m1, macos) but ignoreJobs misses it: its pending events start runs"
            ),
            (
                "release-notes",
                "",
                "is in ignoreJobs but tagged vm: no runner starts for it"
            ),
            (
                "release-tag",
                "",
                "is in ignoreJobs but tagged build: no runner starts for it"
            ),
        ]
    );
    assert_eq!(
        suggest_ignore_jobs(VM, &settings, &jobs),
        ["hosted", "macos"]
    );
}

#[test]
fn a_job_of_other_runners_gets_no_lane_problems() {
    let pipeline = pipeline(json!({
        "lint": { "image": "docker:27", "script": ["docker build ."] }
    }));
    let problems = check_jobs(
        VM,
        &lanes(),
        &[job("lint", Some("test"), &["saas-linux-small-amd64"])],
        &pipeline,
    );
    assert_eq!(
        summary(&problems),
        vec![(
            "lint",
            "node-24",
            "runs on other runners (tags saas-linux-small-amd64) but ignoreJobs misses it: its pending events start runs"
        )]
    );
}

#[test]
fn reports_lane_jobs_without_their_tag_and_vm_jobs_with_a_lane_tag() {
    let pipeline = pipeline(json!({
        "lint": { "image": "node:24-bookworm-slim", "script": ["npm run lint"] },
        "unit-a": { "image": "node:24-bookworm-slim", "script": ["npm test"] }
    }));
    let problems = check_jobs(
        VM,
        &lanes(),
        &[
            job("lint", Some("test"), &["node-24"]),
            job("unit-a", Some("build"), &["vm"]),
            job("unit-b", Some("build"), &[]),
            job("e2e", Some("build"), &["node-24"]),
            job("both", Some("test"), &["vm", "build"]),
            job("naked", Some("test"), &[]),
        ],
        &pipeline,
    );
    assert_eq!(
        summary(&problems),
        vec![
            (
                "unit-a",
                "node-24",
                "lane node-24 takes the job by name but the job is tagged vm"
            ),
            (
                "unit-b",
                "node-24",
                "untagged: add tags [node-24] or a hosted runner tag"
            ),
            (
                "e2e",
                "vm",
                "tagged for lane node-24 but no jobs pattern of that lane matches the name: the VM runner gets its events"
            ),
            ("both", "vm", "tagged for several runners: vm, build"),
            (
                "naked",
                "vm",
                "untagged: add tags [vm] or a hosted runner tag"
            ),
        ]
    );
}

#[test]
fn another_projects_lanes_never_count() {
    let other = project(json!({
        "lanes": [ { "tag": "node-24", "image": "node:24-bookworm-slim", "jobs": ["lint"] } ]
    }));
    let jobs = [job("lint", Some("test"), &["node-24"])];
    let problems = check_jobs(VM, &plain(), &jobs, &Pipeline::default());
    assert_eq!(
        summary(&problems),
        vec![(
            "lint",
            "vm",
            "runs on other runners (tags node-24) but ignoreJobs misses it: its pending events start runs"
        )],
        "a tag of another project's lane is just another runner's tag"
    );
    assert!(check_jobs(VM, &other, &jobs, &Pipeline::default()).is_empty());
}

#[test]
fn a_lane_job_needs_no_docker_and_the_lanes_image() {
    let pipeline = pipeline(json!({
        "variables": { "NODE": "24", "SLIM": { "value": "bookworm-slim" } },
        "lint": { "image": "node:${NODE}-$SLIM", "script": ["npm run lint"] },
        "unit-services": {
            "image": "node:24-bookworm-slim",
            "services": ["postgres:17", { "name": "redis:8" }],
            "script": ["npm test"]
        },
        "unit-dind": { "image": "docker:27", "services": ["docker:dind"], "script": ["docker build ."] },
        "unit-registry": { "image": "registry.example:5000/tools/docker-cli:1", "script": ["true"] },
        "unit-script": {
            "image": "node:24-bookworm-slim",
            "before_script": ["  docker login -u x"],
            "script": ["docker-compose up", "dockerd &", "echo docker run"]
        },
        "unit-other": { "image": "node:22-bookworm-slim", "script": ["npm test"] },
        "unit-variable": { "image": "$CI_REGISTRY_IMAGE/node:24", "script": ["npm test"] },
        "unit-none": { "script": ["npm test"] }
    }));
    let names = [
        "lint",
        "unit-services",
        "unit-dind",
        "unit-registry",
        "unit-script",
        "unit-other",
        "unit-variable",
        "unit-none",
        "unit-unknown",
    ];
    let jobs: Vec<LintJob> = names
        .iter()
        .map(|name| job(name, Some("test"), &["node-24"]))
        .collect();
    let problems = check_jobs(VM, &lanes(), &jobs, &pipeline);
    assert!(problems.iter().all(|p| p.runner == "node-24"));
    let found: Vec<(&str, &str)> = problems
        .iter()
        .map(|p| (p.job.as_str(), p.problem.as_str()))
        .collect();
    assert_eq!(
        found,
        vec![
            (
                "unit-services",
                "lane node-24 has no Docker daemon but the job has services: postgres:17, redis:8"
            ),
            (
                "unit-dind",
                "lane node-24 has no Docker daemon but the job has services: docker:dind"
            ),
            (
                "unit-dind",
                "lane node-24 has no Docker daemon but the job's image is docker:27"
            ),
            (
                "unit-dind",
                "lane node-24 has no Docker daemon but a script line runs docker: docker build ."
            ),
            (
                "unit-dind",
                "lane node-24 runs node:24-bookworm-slim but the job's image is docker:27"
            ),
            (
                "unit-registry",
                "lane node-24 has no Docker daemon but the job's image is registry.example:5000/tools/docker-cli:1"
            ),
            (
                "unit-registry",
                "lane node-24 runs node:24-bookworm-slim but the job's image is registry.example:5000/tools/docker-cli:1"
            ),
            (
                "unit-script",
                "lane node-24 has no Docker daemon but a script line runs docker: docker login -u x"
            ),
            (
                "unit-other",
                "lane node-24 runs node:24-bookworm-slim but the job's image is node:22-bookworm-slim"
            ),
            (
                "unit-variable",
                "image $CI_REGISTRY_IMAGE/node:24: variable CI_REGISTRY_IMAGE is not in the pipeline's top-level variables"
            ),
        ]
    );
}

#[test]
fn docker_images_are_named_docker() {
    for (image, docker) in [
        ("docker:27", true),
        ("docker", true),
        ("docker:27-dind", true),
        ("docker.io/library/docker:27@sha256:abc", true),
        ("registry.example:5000/group/docker", true),
        ("docker.io/library/node:24", false),
        ("registry.example:5000/docker/node:24", false),
        ("node:24-docker", false),
        ("gcr.io/kaniko-project/executor:debug", false),
    ] {
        assert_eq!(is_docker_image(image), docker, "{image}");
    }
}

#[test]
fn lane_tags_are_the_name_and_major_version() {
    for (image, tag) in [
        ("rust:1.96.0-bookworm", "rust-1"),
        ("node:24-bookworm-slim", "node-24"),
        ("mcr.microsoft.com/playwright:v1.64.0-noble", "playwright-1"),
        ("alpine:3.21", "alpine-3"),
        ("alpine", "alpine"),
        ("ubuntu:latest", "ubuntu"),
        (
            "registry.example:5000/Group/My_Tool:2024.1@sha256:abc",
            "my-tool-2024",
        ),
        ("python:3.13@sha256:abc", "python-3"),
        ("__", "lane"),
    ] {
        assert_eq!(lane_tag(image), tag, "{image}");
    }
    assert_eq!(lane_tag(&format!("{}:1", "a".repeat(80))).len(), 63);
}

#[test]
fn suggests_lanes_for_docker_free_jobs_on_the_vm_runner() {
    let pipeline = pipeline(json!({
        "variables": { "RUST": "1.96.0" },
        ".node": { "image": "node:24-bookworm-slim" },
        "eslint": { "extends": ".node", "script": ["npm run lint"] },
        "vitest": { "extends": ".node", "script": ["npm test"] },
        "cargo": { "image": "rust:${RUST}-bookworm", "script": ["cargo test"] },
        "rust-alpine": { "image": "rust:1.96.0-alpine", "script": ["cargo test"] },
        "with-db": { "image": "node:24-bookworm-slim", "services": ["postgres:17"], "script": ["npm test"] },
        "image": { "image": "docker:27", "services": ["docker:dind"], "script": ["docker build ."] },
        "login": { "image": "alpine:3.21", "script": ["docker login"] },
        "unknown": { "image": "$NOPE", "script": ["x"] },
        "bare": { "script": ["x"] },
        "hosted": { "image": "alpine:3.21", "script": ["deploy"] },
        "lint": { "image": "node:24-bookworm-slim", "script": ["already a lane"] },
        "kaniko": { "image": "alpine:3.21", "script": ["build"] }
    }));
    let jobs = [
        job("eslint", Some("test"), &["vm"]),
        job("cargo", Some("test"), &["vm"]),
        job("vitest", Some("test"), &[]),
        job("rust-alpine", Some("test"), &["vm"]),
        job("with-db", Some("test"), &["vm"]),
        job("image", Some("test"), &["vm"]),
        job("login", Some("test"), &["vm"]),
        job("unknown", Some("test"), &["vm"]),
        job("bare", Some("test"), &["vm"]),
        job("hosted", Some("test"), &["saas-linux-small-amd64"]),
        job("lint", Some("test"), &["node-24"]),
        job("image-kaniko", Some("build"), &["build"]),
        job("ciburst-check", Some(".pre"), &["vm"]),
        job("missing", Some("test"), &["vm"]),
    ];
    let suggestions = suggest_lanes(VM, &lanes(), &jobs, &pipeline);
    assert_eq!(
        suggestions,
        [
            Suggestion {
                tag: "node-24-2".into(),
                image: "node:24-bookworm-slim".into(),
                jobs: vec!["eslint".into(), "vitest".into()],
                tagged: vec!["eslint".into(), "vitest".into()],
            },
            Suggestion {
                tag: "rust-1".into(),
                image: "rust:1.96.0-bookworm".into(),
                jobs: vec!["cargo".into()],
                tagged: vec!["cargo".into()],
            },
            Suggestion {
                tag: "rust-1-2".into(),
                image: "rust:1.96.0-alpine".into(),
                jobs: vec!["rust-alpine".into()],
                tagged: vec!["rust-alpine".into()],
            },
        ]
    );
    let none = suggest_lanes(VM, &plain(), &jobs[..1], &pipeline);
    assert_eq!(
        none[0].tag, "node-24",
        "a project without lanes takes the plain tag"
    );
}

#[test]
fn parallel_and_matrix_jobs_get_one_pattern_and_one_tags_line() {
    let pipeline = pipeline(json!({
        "e2e": {
            "image": "node:24-bookworm-slim",
            "parallel": { "matrix": [{ "PLATFORM": ["android", "ios"] }] },
            "script": ["npm run e2e"]
        },
        "unit": { "image": "node:24-bookworm-slim", "parallel": 3, "script": ["npm test"] },
        "unit-smoke": { "image": "alpine:3.21", "script": ["true"] },
        "lint": { "image": "node:24-bookworm-slim", "script": ["npm run lint"] }
    }));
    let jobs = [
        job("e2e: [android]", Some("test"), &["vm"]),
        job("e2e: [ios]", Some("test"), &["vm"]),
        job("unit 1/3", Some("test"), &["vm"]),
        job("unit 2/3", Some("test"), &["vm"]),
        job("unit 3/3", Some("test"), &["vm"]),
        job("unit-smoke", Some("test"), &["saas-linux-small-amd64"]),
    ];
    let suggestions = suggest_lanes(VM, &plain(), &jobs, &pipeline);
    assert_eq!(
        suggestions,
        [Suggestion {
            tag: "node-24".into(),
            image: "node:24-bookworm-slim".into(),
            jobs: vec![
                "e2e*".into(),
                "unit 1/3".into(),
                "unit 2/3".into(),
                "unit 3/3".into()
            ],
            tagged: vec!["e2e".into(), "unit".into()],
        }],
        "unit-smoke starts with unit, so unit keeps its copies' names"
    );
    let settings = project(json!({
        "lanes": [ { "tag": "node-24", "image": "node:24-bookworm-slim", "jobs": suggestions[0].jobs } ]
    }));
    for name in ["e2e: [android]", "e2e: [ios]", "unit 2/3"] {
        assert!(
            matches!(settings.route(name), Route::Lane(lane) if lane.tag == "node-24"),
            "{name}"
        );
    }
    assert_eq!(settings.route("unit-smoke"), Route::Vm);
}
