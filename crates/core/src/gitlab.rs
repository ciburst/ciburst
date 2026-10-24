use std::sync::Arc;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::check::LintJob;
use crate::http::{HttpClient, Request, Response};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Runner {
    pub id: u64,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tag_list: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CreatedRunner {
    pub id: u64,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Hook {
    pub id: u64,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Lint {
    pub valid: bool,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub jobs: Vec<LintJob>,
}

/// Minimal GitLab REST client for the resources ciburst manages.
pub struct Gitlab {
    base: String,
    token: String,
    http: Arc<dyn HttpClient>,
}

impl Gitlab {
    pub fn new(url: &str, token: impl Into<String>, http: Arc<dyn HttpClient>) -> Gitlab {
        Gitlab {
            base: format!("{}/api/v4", url.trim_end_matches('/')),
            token: token.into(),
            http,
        }
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Response> {
        let mut url = format!("{}{}", self.base, path);
        for (index, (key, value)) in query.iter().enumerate() {
            url.push(if index == 0 { '?' } else { '&' });
            url.push_str(key);
            url.push('=');
            url.push_str(value);
        }
        let mut request = Request::new(method, url)
            .header("PRIVATE-TOKEN", self.token.clone())
            .header("accept", "application/json");
        if let Some(body) = &body {
            request = request.json(body);
        }
        let response = self.http.send(request).await?;
        if response.ok() {
            return Ok(response);
        }
        let message = response
            .json::<Value>()
            .ok()
            .and_then(|value| {
                value
                    .get("message")
                    .or_else(|| value.get("error"))
                    .map(|m| m.to_string())
            })
            .unwrap_or_else(|| response.text());
        Err(Error::Gitlab {
            status: response.status,
            method: method.to_string(),
            path: path.to_string(),
            message,
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        self.call("GET", path, query, None).await?.json()
    }

    pub async fn list_runners(&self, project_id: u64) -> Result<Vec<Runner>> {
        self.get(
            &format!("/projects/{project_id}/runners"),
            &[("type", "project_type".into()), ("per_page", "100".into())],
        )
        .await
    }

    /// Project runners carrying `tag`; GitLab's list responses omit `tag_list`, the filter does not.
    pub async fn list_runners_tagged(&self, project_id: u64, tag: &str) -> Result<Vec<Runner>> {
        self.get(
            &format!("/projects/{project_id}/runners"),
            &[
                ("type", "project_type".into()),
                ("tag_list", tag.to_string()),
                ("per_page", "100".into()),
            ],
        )
        .await
    }

    pub async fn create_runner(
        &self,
        project_id: u64,
        tag: &str,
        description: &str,
    ) -> Result<CreatedRunner> {
        let body = json!({
            "runner_type": "project_type",
            "project_id": project_id,
            "tag_list": [tag],
            "run_untagged": false,
            "description": description,
        });
        self.call("POST", "/user/runners", &[], Some(body))
            .await?
            .json()
    }

    pub async fn enable_runner(&self, project_id: u64, runner_id: u64) -> Result<()> {
        self.call(
            "POST",
            &format!("/projects/{project_id}/runners"),
            &[],
            Some(json!({ "runner_id": runner_id })),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_runner(&self, runner_id: u64) -> Result<()> {
        self.call("DELETE", &format!("/runners/{runner_id}"), &[], None)
            .await?;
        Ok(())
    }

    pub async fn list_hooks(&self, project_id: u64) -> Result<Vec<Hook>> {
        self.get(
            &format!("/projects/{project_id}/hooks"),
            &[("per_page", "100".into())],
        )
        .await
    }

    pub async fn create_hook(&self, project_id: u64, url: &str, token: &str) -> Result<Hook> {
        self.call(
            "POST",
            &format!("/projects/{project_id}/hooks"),
            &[],
            Some(hook_body(url, token)),
        )
        .await?
        .json()
    }

    pub async fn update_hook(
        &self,
        project_id: u64,
        hook_id: u64,
        url: &str,
        token: &str,
    ) -> Result<Hook> {
        self.call(
            "PUT",
            &format!("/projects/{project_id}/hooks/{hook_id}"),
            &[],
            Some(hook_body(url, token)),
        )
        .await?
        .json()
    }

    pub async fn delete_hook(&self, project_id: u64, hook_id: u64) -> Result<()> {
        self.call(
            "DELETE",
            &format!("/projects/{project_id}/hooks/{hook_id}"),
            &[],
            None,
        )
        .await?;
        Ok(())
    }

    /// Lints the pipeline at `content_ref` (branch, tag or commit), the default branch when
    /// `None`.
    pub async fn lint_jobs(&self, project_id: u64, content_ref: Option<&str>) -> Result<Lint> {
        let mut query = vec![("include_jobs", "true".to_string())];
        if let Some(content_ref) = content_ref {
            query.push(("content_ref", content_ref.to_string()));
        }
        self.get(&format!("/projects/{project_id}/ci/lint"), &query)
            .await
    }
}

fn hook_body(url: &str, token: &str) -> Value {
    json!({
        "url": url,
        "token": token,
        "job_events": true,
        "push_events": false,
        "enable_ssl_verification": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::gitlab_fake::{FAKE_TOKEN, FakeGitlab};

    fn client(fake: &Arc<FakeGitlab>) -> Gitlab {
        Gitlab::new("https://gitlab.example/", FAKE_TOKEN, fake.clone())
    }

    #[tokio::test]
    async fn runners_round_trip() {
        let fake = Arc::new(FakeGitlab::new());
        let gitlab = client(&fake);
        let created = gitlab.create_runner(7, "ci", "demo-ci").await.unwrap();
        assert!(created.token.starts_with("glrt-"));
        let listed = gitlab.list_runners(7).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].description, "demo-ci");
        assert!(listed[0].tag_list.is_empty(), "GitLab lists omit tag_list");
        assert_eq!(gitlab.list_runners_tagged(7, "ci").await.unwrap().len(), 1);
        assert!(
            gitlab
                .list_runners_tagged(7, "rust")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(gitlab.list_runners(8).await.unwrap().is_empty());
        gitlab.enable_runner(8, created.id).await.unwrap();
        assert_eq!(gitlab.list_runners(8).await.unwrap().len(), 1);
        gitlab.delete_runner(created.id).await.unwrap();
        assert!(gitlab.list_runners(7).await.unwrap().is_empty());
        let call = fake
            .calls()
            .into_iter()
            .find(|c| c.path == "/user/runners")
            .unwrap();
        assert_eq!(
            call.body.unwrap(),
            json!({
                "runner_type": "project_type", "project_id": 7, "tag_list": ["ci"],
                "run_untagged": false, "description": "demo-ci"
            })
        );
    }

    #[tokio::test]
    async fn hooks_round_trip() {
        let fake = Arc::new(FakeGitlab::new());
        let gitlab = client(&fake);
        let hook = gitlab
            .create_hook(7, "https://d.example/", "t1")
            .await
            .unwrap();
        assert_eq!(
            gitlab.list_hooks(7).await.unwrap()[0].url,
            "https://d.example/"
        );
        assert_eq!(fake.hook_token(hook.id), Some("t1".into()));
        gitlab
            .update_hook(7, hook.id, "https://d.example/", "t2")
            .await
            .unwrap();
        assert_eq!(fake.hook_token(hook.id), Some("t2".into()));
        gitlab.delete_hook(7, hook.id).await.unwrap();
        assert!(gitlab.list_hooks(7).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn lint_and_errors() {
        let fake = Arc::new(FakeGitlab::new());
        fake.set_jobs(
            7,
            vec![LintJob {
                name: "unit".into(),
                stage: Some("test".into()),
                tag_list: vec!["ci".into()],
            }],
        );
        fake.set_lint_errors(8, vec!["invalid yaml".into()]);
        let gitlab = client(&fake);
        let ok = gitlab.lint_jobs(7, None).await.unwrap();
        assert!(ok.valid);
        assert_eq!(ok.jobs.len(), 1);
        fake.set_jobs_at(7, "drill", Vec::new());
        assert!(
            gitlab
                .lint_jobs(7, Some("drill"))
                .await
                .unwrap()
                .jobs
                .is_empty()
        );
        assert!(!gitlab.lint_jobs(7, Some("gone")).await.unwrap().valid);
        let bad = gitlab.lint_jobs(8, None).await.unwrap();
        assert!(!bad.valid);
        assert_eq!(bad.errors, ["invalid yaml"]);

        let wrong = Gitlab::new("https://gitlab.example", "wrong", fake.clone());
        match wrong.list_runners(7).await {
            Err(Error::Gitlab { status: 401, .. }) => {}
            other => panic!("expected 401, got {other:?}"),
        }
        match gitlab.delete_runner(1).await {
            Err(Error::Gitlab { status: 404, .. }) => {}
            other => panic!("expected 404, got {other:?}"),
        }
    }
}
