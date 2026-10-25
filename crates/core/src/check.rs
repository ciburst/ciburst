use serde::{Deserialize, Serialize};

use crate::config::{Config, Pool};
use crate::pipeline::{JobSpec, Pipeline};

const DEFAULT_STAGE: &str = "test";

/// Size of a lane `check --suggest` proposes (D46).
pub const SUGGESTED_VCPU: u32 = 2;
pub const SUGGESTED_MEMORY_MIB: u32 = 4096;

/// Jobs of the CI/CD component run on hosted runners; the dispatcher and `check` skip them
/// (spec D41).
pub const OWN_JOB_PREFIX: &str = "ciburst-";

pub fn is_own_job(name: &str) -> bool {
    name.starts_with(OWN_JOB_PREFIX)
}

/// A job as returned by GitLab's CI lint with `include_jobs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintJob {
    pub name: String,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub tag_list: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub job: String,
    pub stage: String,
    pub tags: Vec<String>,
    pub pool: String,
    pub problem: String,
}

/// A lane for Docker-free jobs that run on a VM pool (D46).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub tag: String,
    /// The jobs' image, variables expanded.
    pub image: String,
    pub jobs: Vec<String>,
}

/// Compares the jobs of a linted pipeline with the routing of the config (D46): each job's
/// tags, and for a job routed to a lane what keeps it from running there.
pub fn check_jobs(config: &Config, jobs: &[LintJob], pipeline: &Pipeline) -> Vec<Problem> {
    let mut problems = Vec::new();
    for job in jobs.iter().filter(|job| !is_own_job(&job.name)) {
        let stage = job.stage.as_deref().unwrap_or(DEFAULT_STAGE);
        let pool = config.pool_for(&job.name, Some(stage));
        let expected = pool.map(|p| p.tag.as_str());
        let route = if config.pool_by_name(&job.name).is_some() {
            "the job name".to_string()
        } else {
            format!("stage {stage}")
        };
        let chosen: Vec<&str> = job
            .tag_list
            .iter()
            .map(String::as_str)
            .filter(|tag| config.pool_by_tag(tag).is_some())
            .collect();
        let mut found = Vec::new();
        if chosen.len() > 1 {
            found.push(format!("tagged for several pools: {}", chosen.join(", ")));
        } else if chosen.len() == 1 && Some(chosen[0]) != expected {
            found.push(match expected {
                Some(pool) => format!(
                    "{route} maps to pool {pool} but the job is tagged {}",
                    chosen[0]
                ),
                None => format!(
                    "{route} maps to no pool but the job is tagged {}",
                    chosen[0]
                ),
            });
        } else if job.tag_list.is_empty()
            && let Some(pool) = expected
        {
            found.push(format!(
                "untagged: add tags [{pool}] or a hosted runner tag"
            ));
        }
        if let Some(lane) = pool.filter(|pool| config.is_lane(pool))
            && let Some(spec) = pipeline.job(&job.name)
        {
            found.extend(lane_problems(lane, spec, pipeline));
        }
        for problem in found {
            problems.push(Problem {
                job: job.name.clone(),
                stage: stage.to_string(),
                tags: job.tag_list.clone(),
                pool: expected.unwrap_or("").to_string(),
                problem,
            });
        }
    }
    problems
}

/// D46: what keeps a job from a lane, whose run has no Docker daemon and starts from the
/// lane's image.
fn lane_problems(lane: &Pool, job: &JobSpec, pipeline: &Pipeline) -> Vec<String> {
    let mut problems = Vec::new();
    if !job.services.is_empty() {
        problems.push(format!(
            "lane {} has no Docker daemon but the job has services: {}",
            lane.tag,
            job.services.join(", ")
        ));
    }
    let image = job
        .image
        .as_deref()
        .map(|image| (image, pipeline.expand(image)));
    if let Some((_, Ok(image))) = &image
        && is_docker_image(image)
    {
        problems.push(format!(
            "lane {} has no Docker daemon but the job's image is {image}",
            lane.tag
        ));
    }
    if let Some(line) = docker_line(job) {
        problems.push(format!(
            "lane {} has no Docker daemon but a script line runs docker: {line}",
            lane.tag
        ));
    }
    match image {
        Some((_, Ok(image))) if image != lane.image => problems.push(format!(
            "lane {} runs {} but the job's image is {image}",
            lane.tag, lane.image
        )),
        Some((raw, Err(name))) => problems.push(format!(
            "image {raw}: variable {name} is not in the pipeline's top-level variables"
        )),
        _ => {}
    }
    problems
}

/// D46: lanes for the Docker-free jobs routed to a VM pool, one per image in the order the
/// jobs come. A job that carries only tags of no pool runs elsewhere on purpose; a job
/// without an image, or with one that names an unknown variable, gets no lane.
pub fn suggest_lanes(config: &Config, jobs: &[LintJob], pipeline: &Pipeline) -> Vec<Suggestion> {
    let mut suggestions: Vec<Suggestion> = Vec::new();
    for job in jobs.iter().filter(|job| !is_own_job(&job.name)) {
        let stage = job.stage.as_deref().unwrap_or(DEFAULT_STAGE);
        let on_vm = config
            .pool_for(&job.name, Some(stage))
            .is_some_and(|pool| config.is_vm_pool(pool));
        let here = job.tag_list.is_empty()
            || job
                .tag_list
                .iter()
                .any(|tag| config.pool_by_tag(tag).is_some());
        let Some(spec) = pipeline.job(&job.name).filter(|_| on_vm && here) else {
            continue;
        };
        let Some(Ok(image)) = spec.image.as_deref().map(|image| pipeline.expand(image)) else {
            continue;
        };
        if !spec.services.is_empty() || is_docker_image(&image) || docker_line(spec).is_some() {
            continue;
        }
        if let Some(lane) = suggestions.iter_mut().find(|lane| lane.image == image) {
            lane.jobs.push(job.name.clone());
            continue;
        }
        let base = lane_tag(&image);
        let taken = |tag: &str| {
            config.pool_by_tag(tag).is_some() || suggestions.iter().any(|lane| lane.tag == tag)
        };
        let mut tag = base.clone();
        let mut next = 2;
        while taken(&tag) {
            tag = format!("{base}-{next}");
            next += 1;
        }
        suggestions.push(Suggestion {
            tag,
            image,
            jobs: vec![job.name.clone()],
        });
    }
    suggestions
}

/// The last path segment of an image reference without registry, path and digest, and its
/// tag.
fn image_name(image: &str) -> (&str, Option<&str>) {
    let image = image.split('@').next().unwrap_or(image);
    let last = image.rsplit('/').next().unwrap_or(image);
    match last.split_once(':') {
        Some((name, tag)) => (name, Some(tag)),
        None => (last, None),
    }
}

/// D46: an image whose name starts with `docker`, such as `docker:27` or
/// `registry.example/group/docker`, needs a Docker daemon.
pub fn is_docker_image(image: &str) -> bool {
    image_name(image).0.starts_with("docker")
}

/// The first script line that runs `docker`.
fn docker_line(job: &JobSpec) -> Option<&str> {
    job.script
        .iter()
        .map(|line| line.trim())
        .find(|line| line.starts_with("docker "))
}

/// D46: the tag of a suggested lane: the image's name, `-` and the major version, the first
/// group of digits of its tag. `rust:1.96.0-bookworm` gives `rust-1`, `alpine` gives `alpine`.
pub fn lane_tag(image: &str) -> String {
    let (name, tag) = image_name(image);
    let major = tag.and_then(|tag| {
        let digits = &tag[tag.find(|c: char| c.is_ascii_digit())?..];
        let end = digits
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(digits.len());
        Some(&digits[..end])
    });
    let raw = match major {
        Some(major) => format!("{name}-{major}"),
        None => name.to_string(),
    };
    let mut tag = String::new();
    for c in raw.chars().map(|c| c.to_ascii_lowercase()) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            tag.push(c);
        } else if !tag.is_empty() && !tag.ends_with('-') {
            tag.push('-');
        }
    }
    tag.truncate(63);
    let tag = tag.trim_end_matches('-');
    if tag.is_empty() {
        "lane".to_string()
    } else {
        tag.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;
    use serde_json::json;

    fn config() -> Config {
        parse_config(
            r#"{
              "name": "demo",
              "gitlab": { "url": "https://gitlab.example", "projectIds": [1] },
              "host": { "type": "fake" }, "provider": { "type": "fake" },
              "defaultPool": "ci",
              "pools": [
                { "tag": "ci", "image": "img/node", "stages": ["lint", "test"] },
                { "tag": "build", "image": "img/kaniko", "stages": ["build"] }
              ],
              "cache": { "type": "s3" }
            }"#,
        )
        .unwrap()
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
            .map(|p| (p.job.as_str(), p.pool.as_str(), p.problem.as_str()))
            .collect()
    }

    #[test]
    fn without_a_default_pool_untagged_jobs_are_fine_and_pool_tags_need_a_stage() {
        let mut config = config();
        config.default_pool = None;
        let problems = check_jobs(
            &config,
            &[
                job("hosted", Some("deploy"), &[]),
                job("unit", Some("test"), &["ci"]),
                job("lost", Some("deploy"), &["ci"]),
            ],
            &Pipeline::default(),
        );
        assert_eq!(
            summary(&problems),
            vec![(
                "lost",
                "",
                "stage deploy maps to no pool but the job is tagged ci"
            )]
        );
    }

    #[test]
    fn skips_the_components_own_jobs() {
        let problems = check_jobs(
            &config(),
            &[
                job("ciburst-setup", Some(".pre"), &["saas-linux-small-amd64"]),
                job("ciburst-check", Some(".pre"), &["ci"]),
                job("ciburst-destroy", Some("build"), &[]),
                job("ciburst", Some("build"), &["ci"]),
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
    fn accepts_matching_and_foreign_tags() {
        let problems = check_jobs(
            &config(),
            &[
                job("unit", Some("test"), &["ci"]),
                job("image", Some("build"), &["build"]),
                job("deploy", Some("deploy"), &["ci"]),
                job("hosted", Some("test"), &["saas-linux-small-amd64"]),
                job("implicit", None, &["ci"]),
            ],
            &Pipeline::default(),
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn reports_mismatch_untagged_and_double() {
        let problems = check_jobs(
            &config(),
            &[
                job("wrong", Some("build"), &["ci"]),
                job("naked", Some("test"), &[]),
                job("both", Some("test"), &["ci", "build"]),
                job("no-list", Some("lint"), &[]),
            ],
            &Pipeline::default(),
        );
        assert_eq!(
            summary(&problems),
            vec![
                (
                    "wrong",
                    "build",
                    "stage build maps to pool build but the job is tagged ci"
                ),
                (
                    "naked",
                    "ci",
                    "untagged: add tags [ci] or a hosted runner tag"
                ),
                ("both", "ci", "tagged for several pools: ci, build"),
                (
                    "no-list",
                    "ci",
                    "untagged: add tags [ci] or a hosted runner tag"
                ),
            ]
        );
    }

    fn lane_config() -> Config {
        parse_config(
            r#"{
              "name": "demo",
              "gitlab": { "url": "https://gitlab.example", "projectIds": [1] },
              "host": { "type": "fake" }, "provider": { "type": "fake" },
              "defaultPool": "vm",
              "pools": [
                { "tag": "vm", "image": "ubuntu:24.04", "runtime": "instances" },
                { "tag": "node-24", "image": "node:24-bookworm-slim", "runtime": "jobs",
                  "jobs": ["lint", "unit-*"] },
                { "tag": "build", "image": "img/kaniko", "runtime": "jobs", "stages": ["build"] }
              ],
              "cache": { "type": "s3" }
            }"#,
        )
        .unwrap()
    }

    fn pipeline(root: serde_json::Value) -> Pipeline {
        Pipeline::from_value(&root).unwrap()
    }

    #[test]
    fn routes_by_job_name_before_the_stage() {
        let pipeline = pipeline(json!({
            "lint": { "image": "node:24-bookworm-slim", "script": ["npm run lint"] },
            "unit-a": { "image": "node:24-bookworm-slim", "script": ["npm test"] }
        }));
        let problems = check_jobs(
            &lane_config(),
            &[
                job("lint", Some("test"), &["node-24"]),
                job("unit-a", Some("build"), &["vm"]),
                job("unit-b", Some("build"), &[]),
                job("e2e", Some("build"), &["vm"]),
            ],
            &pipeline,
        );
        assert_eq!(
            summary(&problems),
            vec![
                (
                    "unit-a",
                    "node-24",
                    "the job name maps to pool node-24 but the job is tagged vm"
                ),
                (
                    "unit-b",
                    "node-24",
                    "untagged: add tags [node-24] or a hosted runner tag"
                ),
                (
                    "e2e",
                    "build",
                    "stage build maps to pool build but the job is tagged vm"
                ),
            ]
        );
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
        let problems = check_jobs(&lane_config(), &jobs, &pipeline);
        assert!(problems.iter().all(|p| p.pool == "node-24"));
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
    fn suggests_lanes_for_docker_free_jobs_on_a_vm_pool() {
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
            job("kaniko", Some("build"), &["build"]),
            job("ciburst-check", Some(".pre"), &["vm"]),
            job("missing", Some("test"), &["vm"]),
        ];
        let suggestions = suggest_lanes(&lane_config(), &jobs, &pipeline);
        assert_eq!(
            suggestions,
            [
                Suggestion {
                    tag: "node-24-2".into(),
                    image: "node:24-bookworm-slim".into(),
                    jobs: vec!["eslint".into(), "vitest".into()],
                },
                Suggestion {
                    tag: "rust-1".into(),
                    image: "rust:1.96.0-bookworm".into(),
                    jobs: vec!["cargo".into()],
                },
                Suggestion {
                    tag: "rust-1-2".into(),
                    image: "rust:1.96.0-alpine".into(),
                    jobs: vec!["rust-alpine".into()],
                },
            ]
        );
    }
}
