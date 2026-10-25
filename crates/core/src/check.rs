use serde::{Deserialize, Serialize};

use crate::config::Config;

const DEFAULT_STAGE: &str = "test";

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

/// Compares the jobs of a linted pipeline with the stage map.
pub fn check_jobs(config: &Config, jobs: &[LintJob]) -> Vec<Problem> {
    let mut problems = Vec::new();
    for job in jobs.iter().filter(|job| !is_own_job(&job.name)) {
        let stage = job.stage.as_deref().unwrap_or(DEFAULT_STAGE);
        let expected = config.pool_for_stage(Some(stage)).map(|p| p.tag.as_str());
        let chosen: Vec<&str> = job
            .tag_list
            .iter()
            .map(String::as_str)
            .filter(|tag| config.pool_by_tag(tag).is_some())
            .collect();
        let problem = if chosen.len() > 1 {
            Some(format!("tagged for several pools: {}", chosen.join(", ")))
        } else if chosen.len() == 1 && Some(chosen[0]) != expected {
            Some(match expected {
                Some(pool) => format!(
                    "stage {stage} maps to pool {pool} but the job is tagged {}",
                    chosen[0]
                ),
                None => format!(
                    "stage {stage} maps to no pool but the job is tagged {}",
                    chosen[0]
                ),
            })
        } else if job.tag_list.is_empty()
            && let Some(pool) = expected
        {
            Some(format!(
                "untagged: add tags [{pool}] or a hosted runner tag"
            ))
        } else {
            None
        };
        if let Some(problem) = problem {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;

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
              ]
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
        );
        let summary: Vec<(&str, &str, &str)> = problems
            .iter()
            .map(|p| (p.job.as_str(), p.pool.as_str(), p.problem.as_str()))
            .collect();
        assert_eq!(
            summary,
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
        );
        let summary: Vec<(&str, &str, &str)> = problems
            .iter()
            .map(|p| (p.job.as_str(), p.pool.as_str(), p.problem.as_str()))
            .collect();
        assert_eq!(
            summary,
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
}
