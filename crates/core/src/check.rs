use serde::{Deserialize, Serialize};

use crate::config::Config;

const DEFAULT_STAGE: &str = "test";

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
    for job in jobs {
        let stage = job.stage.as_deref().unwrap_or(DEFAULT_STAGE);
        let expected = config.pool_for_stage(Some(stage));
        let chosen: Vec<&str> = job
            .tag_list
            .iter()
            .map(String::as_str)
            .filter(|tag| config.pool_by_tag(tag).is_some())
            .collect();
        let problem = if chosen.len() > 1 {
            Some(format!("tagged for several pools: {}", chosen.join(", ")))
        } else if chosen.len() == 1 && chosen[0] != expected.tag {
            Some(format!(
                "stage {stage} maps to pool {} but the job is tagged {}",
                expected.tag, chosen[0]
            ))
        } else if job.tag_list.is_empty() {
            Some(format!(
                "untagged: add tags [{}] or a hosted runner tag",
                expected.tag
            ))
        } else {
            None
        };
        if let Some(problem) = problem {
            problems.push(Problem {
                job: job.name.clone(),
                stage: stage.to_string(),
                tags: job.tag_list.clone(),
                pool: expected.tag.clone(),
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
