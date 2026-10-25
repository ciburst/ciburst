use std::sync::Arc;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::check::LintJob;
use crate::http::{Headers, HttpClient, Request, Response, header_value};
use crate::{Error, Result};

/// Page size of every paginated list; GitLab's maximum.
pub const PER_PAGE: usize = 100;

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

/// A project CI/CD variable as GitLab lists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Variable {
    pub key: String,
    /// `None` for a hidden variable: GitLab never returns its value.
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default = "any_scope")]
    pub environment_scope: String,
    #[serde(default)]
    pub protected: bool,
    #[serde(default)]
    pub masked: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub raw: bool,
}

fn any_scope() -> String {
    "*".to_string()
}

/// A project variable to create, or to update in its scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VariableSpec<'a> {
    pub key: &'a str,
    pub value: &'a str,
    pub environment_scope: &'a str,
    pub protected: bool,
    pub raw: bool,
    /// Masked and hidden; GitLab takes it on create only, so updates ignore it.
    pub masked_and_hidden: bool,
}

/// A job as `GET /projects/:id/jobs` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Job {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub tag_list: Vec<String>,
    /// RFC 3339.
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub pipeline: Option<JobPipeline>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct JobPipeline {
    pub id: u64,
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
        Ok(self.call_with_headers(method, path, query, body).await?.0)
    }

    async fn call_with_headers(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<(Response, Headers)> {
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
        let (response, headers) = self.http.send_with_headers(request).await?;
        if response.ok() {
            return Ok((response, headers));
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

    /// Every page of a list, following `x-next-page`; without the header a full page means
    /// another one.
    async fn get_all<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Vec<T>> {
        let mut items = Vec::new();
        let mut page: u64 = 1;
        loop {
            let mut paged = query.to_vec();
            paged.push(("per_page", PER_PAGE.to_string()));
            paged.push(("page", page.to_string()));
            let (response, headers) = self.call_with_headers("GET", path, &paged, None).await?;
            let batch: Vec<T> = response.json()?;
            let full = batch.len() >= PER_PAGE;
            items.extend(batch);
            let next = match header_value(&headers, "x-next-page") {
                Some(next) => next.trim().parse::<u64>().ok(),
                None => full.then_some(page + 1),
            };
            match next {
                Some(next) if next > page => page = next,
                _ => return Ok(items),
            }
        }
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
            .await
            .map_err(|error| match error {
                Error::Gitlab {
                    status: 403,
                    method,
                    path,
                    message,
                } => Error::Gitlab {
                    status: 403,
                    method,
                    path,
                    message: format!(
                        "{message}; the token needs the scope create_runner and the Owner \
                         role in project {project_id}"
                    ),
                },
                other => other,
            })?
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

    pub async fn list_variables(&self, project_id: u64) -> Result<Vec<Variable>> {
        self.get_all(&format!("/projects/{project_id}/variables"), &[])
            .await
    }

    pub async fn create_variable(
        &self,
        project_id: u64,
        variable: &VariableSpec<'_>,
    ) -> Result<Variable> {
        let mut body = json!({
            "key": variable.key,
            "value": variable.value,
            "environment_scope": variable.environment_scope,
            "protected": variable.protected,
            "raw": variable.raw,
        });
        if variable.masked_and_hidden {
            body["masked_and_hidden"] = true.into();
        }
        self.call(
            "POST",
            &format!("/projects/{project_id}/variables"),
            &[],
            Some(body),
        )
        .await?
        .json()
    }

    /// Updates the variable of `variable.key` in `variable.environment_scope`.
    pub async fn update_variable(
        &self,
        project_id: u64,
        variable: &VariableSpec<'_>,
    ) -> Result<Variable> {
        let body = json!({
            "value": variable.value,
            "protected": variable.protected,
            "raw": variable.raw,
        });
        self.call(
            "PUT",
            &format!("/projects/{project_id}/variables/{}", variable.key),
            &[scope_filter(variable.environment_scope)],
            Some(body),
        )
        .await?
        .json()
    }

    pub async fn delete_variable(
        &self,
        project_id: u64,
        key: &str,
        environment_scope: &str,
    ) -> Result<()> {
        self.call(
            "DELETE",
            &format!("/projects/{project_id}/variables/{key}"),
            &[scope_filter(environment_scope)],
            None,
        )
        .await?;
        Ok(())
    }

    /// Jobs waiting for a runner.
    pub async fn list_pending_jobs(&self, project_id: u64) -> Result<Vec<Job>> {
        self.get_all(
            &format!("/projects/{project_id}/jobs"),
            &[("scope[]", "pending".to_string())],
        )
        .await
    }
}

fn scope_filter(environment_scope: &str) -> (&'static str, String) {
    ("filter[environment_scope]", environment_scope.to_string())
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
    use crate::testing::gitlab_fake::{FAKE_TOKEN, FakeGitlab, FakeJob, maskable};

    fn client(fake: &Arc<FakeGitlab>) -> Gitlab {
        Gitlab::new("https://gitlab.example/", FAKE_TOKEN, fake.clone())
    }

    fn spec<'a>(key: &'a str, value: &'a str, scope: &'a str, hidden: bool) -> VariableSpec<'a> {
        VariableSpec {
            key,
            value,
            environment_scope: scope,
            protected: true,
            raw: true,
            masked_and_hidden: hidden,
        }
    }

    fn status_of<T: std::fmt::Debug>(result: Result<T>) -> u16 {
        match result {
            Err(Error::Gitlab { status, .. }) => status,
            other => panic!("expected a GitLab error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn variables_are_as_stingy_as_gitlab() {
        let fake = Arc::new(FakeGitlab::new());
        let gitlab = client(&fake);
        let created = gitlab
            .create_variable(7, &spec("A_TOKEN", "abcdefgh123", "ciburst", true))
            .await
            .unwrap();
        assert_eq!(created.value, None, "a hidden value is never returned");
        assert!(created.masked && created.hidden && created.protected && created.raw);
        assert_eq!(created.environment_scope, "ciburst");
        assert_eq!(
            status_of(
                gitlab
                    .create_variable(7, &spec("A_TOKEN", "other-value", "ciburst", false))
                    .await
            ),
            400,
            "duplicate key in the same scope"
        );
        gitlab
            .create_variable(7, &spec("A_TOKEN", "x", "*", false))
            .await
            .unwrap();
        for short_or_odd in ["1234567", "has space in it", "ümlaut-value"] {
            assert!(!maskable(short_or_odd));
            assert_eq!(
                status_of(
                    gitlab
                        .create_variable(7, &spec("B", short_or_odd, "ciburst", true))
                        .await
                ),
                400,
                "{short_or_odd}"
            );
        }
        assert!(maskable("https://a.example/x@y:z~+=_-"));

        let listed = gitlab.list_variables(7).await.unwrap();
        assert_eq!(listed.len(), 2);
        let hidden = listed
            .iter()
            .find(|v| v.environment_scope == "ciburst")
            .unwrap();
        assert_eq!(hidden.value, None);
        let plain = listed.iter().find(|v| v.environment_scope == "*").unwrap();
        assert_eq!(plain.value.as_deref(), Some("x"));

        let update = spec("A_TOKEN", "abcdefgh456", "ciburst", false);
        gitlab.update_variable(7, &update).await.unwrap();
        let stored = fake.variables(7);
        assert_eq!(stored[0].value, "abcdefgh456");
        assert_eq!(stored[1].value, "x", "the other scope stays");
        assert!(stored[0].hidden, "hidden stays");
        assert_eq!(
            status_of(
                gitlab
                    .update_variable(7, &spec("A_TOKEN", "short", "ciburst", false))
                    .await
            ),
            400,
            "a masked variable keeps the mask rule"
        );
        assert_eq!(
            status_of(
                gitlab
                    .update_variable(7, &spec("NONE", "abcdefgh456", "ciburst", false))
                    .await
            ),
            404
        );

        let put = fake
            .calls()
            .into_iter()
            .find(|c| c.method == "PUT")
            .unwrap();
        assert_eq!(put.path, "/projects/7/variables/A_TOKEN");
        assert_eq!(put.query, "filter[environment_scope]=ciburst");
        assert_eq!(
            put.body.unwrap(),
            json!({ "value": "abcdefgh456", "protected": true, "raw": true })
        );
        let post = fake
            .calls()
            .into_iter()
            .find(|c| c.method == "POST")
            .unwrap();
        assert_eq!(
            post.body.unwrap(),
            json!({
                "key": "A_TOKEN", "value": "abcdefgh123", "environment_scope": "ciburst",
                "protected": true, "raw": true, "masked_and_hidden": true
            })
        );

        gitlab
            .delete_variable(7, "A_TOKEN", "ciburst")
            .await
            .unwrap();
        assert_eq!(
            status_of(gitlab.delete_variable(7, "A_TOKEN", "ciburst").await),
            404
        );
        assert_eq!(fake.variables(7).len(), 1);
        assert_eq!(fake.variables(7)[0].environment_scope, "*");
    }

    #[tokio::test]
    async fn the_fake_needs_the_scope_filter_and_creates_hidden_only() {
        let fake = Arc::new(FakeGitlab::new());
        let gitlab = client(&fake);
        gitlab
            .create_variable(7, &spec("K", "abcdefgh", "ciburst", true))
            .await
            .unwrap();
        let raw = |method: &str, query: &str, body: Value| {
            let fake = fake.clone();
            let request = Request::new(
                method,
                format!("https://gitlab.example/api/v4/projects/7/variables/K{query}"),
            )
            .header("PRIVATE-TOKEN", FAKE_TOKEN)
            .json(&body);
            async move { fake.send(request).await.unwrap().status }
        };
        assert_eq!(raw("PUT", "", json!({ "value": "abcdefgh2" })).await, 404);
        assert_eq!(raw("DELETE", "", json!({})).await, 404);
        assert_eq!(
            raw(
                "PUT",
                "?filter[environment_scope]=ciburst",
                json!({ "value": "abcdefgh2", "masked_and_hidden": false })
            )
            .await,
            400,
            "masked_and_hidden only on create"
        );
        assert_eq!(
            raw("DELETE", "?filter[environment_scope]=ciburst", json!({})).await,
            204
        );
    }

    #[tokio::test]
    async fn lists_follow_every_page() {
        let fake = Arc::new(FakeGitlab::new());
        let gitlab = client(&fake);
        for index in 0..130 {
            gitlab
                .create_variable(7, &spec(&format!("V{index}"), "value", "*", false))
                .await
                .unwrap();
        }
        let listed = gitlab.list_variables(7).await.unwrap();
        assert_eq!(listed.len(), 130);
        assert_eq!(listed[129].key, "V129");
        let pages: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| c.method == "GET")
            .map(|c| c.query)
            .collect();
        assert_eq!(pages, ["per_page=100&page=1", "per_page=100&page=2"]);
    }

    #[tokio::test]
    async fn pending_jobs_with_tags_and_creation_time() {
        let fake = Arc::new(FakeGitlab::new());
        let gitlab = client(&fake);
        for id in 1..=105 {
            fake.add_job(
                7,
                FakeJob::pending(id, "unit", "test", &["vm"], "2026-10-25T12:00:00.000Z"),
            );
        }
        let mut done = FakeJob::pending(200, "old", "test", &["vm"], "2026-10-25T11:00:00Z");
        done.status = "success".into();
        fake.add_job(7, done);
        let jobs = gitlab.list_pending_jobs(7).await.unwrap();
        assert_eq!(jobs.len(), 105);
        assert!(jobs.iter().all(|j| j.id != 200));
        let first = jobs.iter().find(|j| j.id == 1).unwrap();
        assert_eq!(first.name, "unit");
        assert_eq!(first.stage.as_deref(), Some("test"));
        assert_eq!(first.tag_list, ["vm"]);
        assert_eq!(first.created_at, "2026-10-25T12:00:00.000Z");
        assert_eq!(first.pipeline.as_ref().map(|p| p.id), Some(901));
        let call = fake.calls().pop().unwrap();
        assert_eq!(call.path, "/projects/7/jobs");
        assert_eq!(call.query, "scope[]=pending&per_page=100&page=2");
        assert!(gitlab.list_pending_jobs(8).await.unwrap().is_empty());
    }

    /// A client that keeps no headers: a full page means another one.
    struct Headerless(Arc<FakeGitlab>);

    #[async_trait::async_trait]
    impl HttpClient for Headerless {
        async fn send(&self, request: Request) -> Result<Response> {
            self.0.send(request).await
        }
    }

    #[tokio::test]
    async fn without_headers_a_full_page_asks_for_the_next() {
        let fake = Arc::new(FakeGitlab::new());
        for id in 1..=200 {
            fake.add_job(
                7,
                FakeJob::pending(id, "u", "test", &[], "2026-10-25T12:00:00Z"),
            );
        }
        let gitlab = Gitlab::new(
            "https://gitlab.example",
            FAKE_TOKEN,
            Arc::new(Headerless(fake.clone())),
        );
        assert_eq!(gitlab.list_pending_jobs(7).await.unwrap().len(), 200);
        assert_eq!(fake.calls().len(), 3, "the third page is empty");
    }

    #[tokio::test]
    async fn a_forbidden_runner_creation_names_scope_and_role() {
        let fake = Arc::new(FakeGitlab::new());
        fake.forbid_runner_creation();
        let error = client(&fake)
            .create_runner(7, "vm", "demo-vm")
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Gitlab { status: 403, .. }));
        assert!(
            error.to_string().ends_with(
                "; the token needs the scope create_runner and the Owner role in project 7"
            ),
            "{error}"
        );
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
