use serde::{Deserialize, Serialize};

use crate::config::{Lane, ProjectConfig, Route};
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
    /// The tag of the runner the job's name routes it to: a lane's or the VM runner's; empty
    /// for a job in `ignoreJobs`.
    pub runner: String,
    pub problem: String,
}

/// A lane for Docker-free jobs that run on the VM runner (D46).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub tag: String,
    /// The jobs' image, variables expanded.
    pub image: String,
    /// Job names and patterns for the lane's `jobs`.
    pub jobs: Vec<String>,
    /// The jobs of the pipeline's YAML that need the lane's tag, a parallel or matrix job
    /// once under its own name (M7 rule R7).
    pub tagged: Vec<String>,
}

/// The tags of `job` that name a runner of the project: the VM runner's or one of its lanes'.
fn own_tags<'a>(vm_tag: &str, project: &ProjectConfig, job: &'a LintJob) -> Vec<&'a str> {
    job.tag_list
        .iter()
        .map(String::as_str)
        .filter(|tag| *tag == vm_tag || project.lane_by_tag(tag).is_some())
        .collect()
}

/// M6.1 rule R8: a job tagged only for other runners whose pending events still start a VM or
/// a lane run, because the project's `ignoreJobs` misses it.
fn runs_elsewhere_unignored(vm_tag: &str, project: &ProjectConfig, job: &LintJob) -> bool {
    !job.tag_list.is_empty()
        && own_tags(vm_tag, project, job).is_empty()
        && project.route(&job.name) != Route::Ignored
}

/// Compares the jobs of a linted pipeline with the project's own lanes and `ignoreJobs` (M6.4
/// rule R8): a lane job without the lane's tag, with another image or with a Docker need; a VM
/// job with a lane's tag; the jobs of other runners that `ignoreJobs` misses (M6.1 rule R8).
pub fn check_jobs(
    vm_tag: &str,
    project: &ProjectConfig,
    jobs: &[LintJob],
    pipeline: &Pipeline,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    for job in jobs.iter().filter(|job| !is_own_job(&job.name)) {
        let stage = job.stage.as_deref().unwrap_or(DEFAULT_STAGE);
        let route = project.route(&job.name);
        let expected = match route {
            Route::Ignored => "",
            Route::Lane(lane) => lane.tag.as_str(),
            Route::Vm => vm_tag,
        };
        let chosen = own_tags(vm_tag, project, job);
        let mut found = Vec::new();
        if route == Route::Ignored {
            if let Some(tag) = chosen.first() {
                found.push(format!(
                    "is in ignoreJobs but tagged {tag}: no runner starts for it"
                ));
            }
        } else if runs_elsewhere_unignored(vm_tag, project, job) {
            found.push(format!(
                "runs on other runners (tags {}) but ignoreJobs misses it: its pending events start runs",
                job.tag_list.join(", ")
            ));
        } else if chosen.len() > 1 {
            found.push(format!("tagged for several runners: {}", chosen.join(", ")));
        } else if chosen.len() == 1 && chosen[0] != expected {
            found.push(match route {
                Route::Lane(lane) => format!(
                    "lane {} takes the job by name but the job is tagged {}",
                    lane.tag, chosen[0]
                ),
                _ => format!(
                    "tagged for lane {} but no jobs pattern of that lane matches the name: the VM runner gets its events",
                    chosen[0]
                ),
            });
        } else if job.tag_list.is_empty() {
            found.push(format!(
                "untagged: add tags [{expected}] or a hosted runner tag"
            ));
        }
        let here = job.tag_list.is_empty() || !chosen.is_empty();
        if let Route::Lane(lane) = route
            && here
            && let Some(spec) = pipeline.job(&job.name)
        {
            found.extend(lane_problems(lane, spec, pipeline));
        }
        for problem in found {
            problems.push(Problem {
                job: job.name.clone(),
                stage: stage.to_string(),
                tags: job.tag_list.clone(),
                runner: expected.to_string(),
                problem,
            });
        }
    }
    problems
}

/// D46: what keeps a job from a lane, whose run has no Docker daemon and starts from the
/// lane's image.
fn lane_problems(lane: &Lane, job: &JobSpec, pipeline: &Pipeline) -> Vec<String> {
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

/// D46, M6.4 rule R8: lanes for the project's Docker-free jobs on the VM runner, one per image
/// in the order the jobs come. A job that carries only tags of other runners runs elsewhere on
/// purpose; a job without an image, or with one that names an unknown variable, gets no lane.
/// The copies of a parallel or matrix job share one pattern `<name>*` unless another job's
/// name starts with `<name>` (M7 rule R7).
pub fn suggest_lanes(
    vm_tag: &str,
    project: &ProjectConfig,
    jobs: &[LintJob],
    pipeline: &Pipeline,
) -> Vec<Suggestion> {
    let names: Vec<&str> = jobs
        .iter()
        .map(|job| job.name.as_str())
        .chain(pipeline.jobs.keys().map(String::as_str))
        .collect();
    let mut suggestions: Vec<Suggestion> = Vec::new();
    for job in jobs.iter().filter(|job| !is_own_job(&job.name)) {
        let on_vm = project.route(&job.name) == Route::Vm;
        let here = job.tag_list.is_empty() || !own_tags(vm_tag, project, job).is_empty();
        let Some(spec) = pipeline.job(&job.name).filter(|_| on_vm && here) else {
            continue;
        };
        let Some(Ok(image)) = spec.image.as_deref().map(|image| pipeline.expand(image)) else {
            continue;
        };
        if !spec.services.is_empty() || is_docker_image(&image) || docker_line(spec).is_some() {
            continue;
        }
        let (pattern, tagged) = match pipeline.parallel_of(&job.name) {
            Some(base) => {
                let shared = names.iter().any(|other| {
                    other.starts_with(base)
                        && *other != base
                        && pipeline.parallel_of(other) != Some(base)
                });
                let pattern = if shared {
                    job.name.clone()
                } else {
                    format!("{base}*")
                };
                (pattern, base.to_string())
            }
            None => (job.name.clone(), job.name.clone()),
        };
        if let Some(lane) = suggestions.iter_mut().find(|lane| lane.image == image) {
            if !lane.jobs.contains(&pattern) {
                lane.jobs.push(pattern);
            }
            if !lane.tagged.contains(&tagged) {
                lane.tagged.push(tagged);
            }
            continue;
        }
        let base = lane_tag(&image);
        let taken = |tag: &str| {
            tag == vm_tag
                || project.lane_by_tag(tag).is_some()
                || suggestions.iter().any(|lane| lane.tag == tag)
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
            jobs: vec![pattern],
            tagged: vec![tagged],
        });
    }
    suggestions
}

/// M6.1 rule R8: the names for the project's `ignoreJobs` of the jobs tagged only for other
/// runners that it misses, in the order the jobs come.
pub fn suggest_ignore_jobs(vm_tag: &str, project: &ProjectConfig, jobs: &[LintJob]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for job in jobs.iter().filter(|job| !is_own_job(&job.name)) {
        if runs_elsewhere_unignored(vm_tag, project, job) && !names.contains(&job.name) {
            names.push(job.name.clone());
        }
    }
    names
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
mod tests;
