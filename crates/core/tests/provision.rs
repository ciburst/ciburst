use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ciburst_core::check::LintJob;
use ciburst_core::config::{Config, parse_config};
use ciburst_core::dispatch::Dispatcher;
use ciburst_core::fake::{FakeHost, FakeProvider};
use ciburst_core::gitlab::Gitlab;
use ciburst_core::log::Log;
use ciburst_core::plan::{MemoryOutputs, OutputsStore};
use ciburst_core::provision::{Provisioner, keys, secrets_from_outputs};
use ciburst_core::store::MemoryStore;
use ciburst_core::testing::gitlab_fake::{FAKE_TOKEN, FakeGitlab};
use ciburst_core::types::{Action, Host, Provider, Rotation, Step};
use serde_json::json;

fn config(project_ids: &[u64]) -> Arc<Config> {
    let raw = json!({
        "name": "demo",
        "gitlab": { "url": "https://gitlab.example", "projectIds": project_ids },
        "host": { "type": "fake" },
        "provider": { "type": "fake" },
        "defaultPool": "ci",
        "pools": [
            { "tag": "ci", "image": "img/node", "stages": ["test"] },
            { "tag": "build", "image": "img/kaniko", "stages": ["build"] }
        ],
        "cache": { "type": "s3" }
    });
    Arc::new(parse_config(&raw.to_string()).unwrap())
}

struct Setup {
    config: Arc<Config>,
    fake: Arc<FakeGitlab>,
    provider: Arc<FakeProvider>,
    host: Arc<FakeHost>,
    outputs: Arc<MemoryOutputs>,
    lines: Arc<Mutex<Vec<String>>>,
    provisioner: Provisioner,
}

fn setup(config: Arc<Config>, with_gitlab: bool) -> Setup {
    let fake = Arc::new(FakeGitlab::new());
    let provider = Arc::new(FakeProvider::new(
        config.clone(),
        Arc::new(MemoryStore::new()),
    ));
    let host = Arc::new(FakeHost::new(config.clone(), Arc::new(MemoryStore::new())));
    let outputs = Arc::new(MemoryOutputs::new());
    let (log, lines) = Log::memory();
    let gitlab =
        with_gitlab.then(|| Arc::new(Gitlab::new(&config.gitlab.url, FAKE_TOKEN, fake.clone())));
    let provisioner = Provisioner::new(
        config.clone(),
        provider.clone(),
        host.clone(),
        gitlab,
        outputs.clone(),
        log,
    );
    Setup {
        config,
        fake,
        provider,
        host,
        outputs,
        lines,
        provisioner,
    }
}

fn actions(steps: &[Step]) -> BTreeMap<String, Action> {
    steps.iter().map(|s| (s.id.clone(), s.action)).collect()
}

fn all(steps: &[Step], action: Action) -> bool {
    steps.iter().all(|s| s.action == action)
}

#[tokio::test]
async fn plans_everything_applies_then_plans_nothing() {
    let s = setup(config(&[7]), true);
    let first = s.provisioner.plan(&[]).await.unwrap();
    assert!(all(&first, Action::Create), "{first:?}");
    let ids: Vec<&str> = first.iter().map(|s| s.id.as_str()).collect();
    for id in [
        "project",
        "key:dispatch",
        "key:cache",
        "bucket",
        "definition:ci",
        "definition:build",
        "runner:ci",
        "runner:build",
        "dispatcher",
        "hook:7",
    ] {
        assert!(ids.contains(&id), "{id} in {ids:?}");
    }
    assert_eq!(ids.len(), 10);
    let hook = first.iter().find(|s| s.id == "hook:7").unwrap();
    assert_eq!(
        hook.detail.as_deref(),
        Some("url known after the dispatcher is deployed")
    );

    let applied = s.provisioner.apply(&[]).await.unwrap();
    assert!(all(&applied, Action::Create), "{applied:?}");
    let values = s.outputs.read().await.unwrap();
    assert!(values.contains_key(keys::HOOK_TOKEN));
    assert!(values.contains_key(keys::VM_SECRET));
    assert!(values[&keys::runner_token("ci")].starts_with("glrt-"));
    assert!(values[&keys::runner_token("build")].starts_with("glrt-"));
    assert!(values.contains_key(&keys::runner_id("ci")));
    assert!(values.contains_key(keys::DISPATCH_SECRET_KEY));
    assert!(values.contains_key(keys::CACHE_SECRET_KEY));
    assert_eq!(
        values[keys::DISPATCH_URL],
        "https://demo-dispatch.fake.invalid/"
    );
    assert_eq!(s.fake.runner_count(), 2);
    let hooks = s.fake.hooks();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].2, values[keys::DISPATCH_URL]);
    assert_eq!(
        s.fake.hook_token(hooks[0].0).as_deref(),
        Some(values[keys::HOOK_TOKEN].as_str())
    );
    assert_eq!(s.fake.hook_flags(hooks[0].0), Some((true, false)));

    let second = s.provisioner.plan(&[]).await.unwrap();
    assert!(all(&second, Action::Noop), "{second:?}");
    let again = s.provisioner.apply(&[]).await.unwrap();
    assert!(all(&again, Action::Noop), "{again:?}");
    assert_eq!(s.outputs.read().await.unwrap(), values);
    assert!(!s.lines.lock().unwrap().iter().any(|l| l.contains("glrt-")));
}

#[tokio::test]
async fn secrets_reach_the_dispatcher_and_status_counts_runs() {
    let s = setup(config(&[7]), true);
    s.provisioner.apply(&[]).await.unwrap();
    let secrets = secrets_from_outputs(&s.config, &s.outputs.read().await.unwrap());
    assert_eq!(secrets.runner_tokens.len(), 2);
    assert!(secrets.cache.is_some());
    let dispatcher = Dispatcher::new(
        s.config.clone(),
        s.provider.clone(),
        Arc::new(secrets.clone()),
        Log::silent(),
    );
    let headers = vec![(
        "x-gitlab-token".to_string(),
        secrets.hook_token.clone().unwrap(),
    )];
    let body = json!({
        "object_kind": "build", "build_status": "pending", "build_id": 1,
        "pipeline_id": 2, "project_id": 7, "build_stage": "test"
    });
    let response = dispatcher
        .handle(&headers, body.to_string().as_bytes())
        .await;
    assert_eq!(response.body["result"], "started", "{}", response.body);
    let status = s.provisioner.status().await.unwrap();
    assert_eq!(
        status.url.as_deref(),
        Some("https://demo-dispatch.fake.invalid/")
    );
    assert_eq!(status.pools.len(), 2);
    assert_eq!(
        (
            status.pools[0].tag.as_str(),
            status.pools[0].live,
            status.pools[0].max_runs
        ),
        ("ci", 1, 10)
    );
    assert_eq!(
        (status.pools[1].tag.as_str(), status.pools[1].live),
        ("build", 0)
    );
    assert_eq!(status.runs.len(), 1);
    let _ = &s.host;
}

#[tokio::test]
async fn the_vm_secret_is_written_once_and_rotates_on_its_own() {
    let s = setup(config(&[7]), false);
    s.provisioner.apply(&[]).await.unwrap();
    let first = s.outputs.read().await.unwrap();
    let secret = first[keys::VM_SECRET].clone();
    assert_eq!(secret.len(), 32);
    assert!(secret.chars().all(|c| c.is_ascii_alphanumeric()));
    assert_ne!(secret, first[keys::HOOK_TOKEN]);
    let secrets = secrets_from_outputs(&s.config, &first);
    assert_eq!(secrets.vm_secret.as_deref(), Some(secret.as_str()));
    assert!(
        s.host
            .plan(&secrets)
            .await
            .unwrap()
            .iter()
            .all(Step::is_noop),
        "the host got the same secrets, VM secret included"
    );

    s.provisioner.apply(&[]).await.unwrap();
    s.provisioner
        .apply(&[Rotation::Key, Rotation::Cache, Rotation::Runner])
        .await
        .unwrap();
    assert_eq!(s.outputs.read().await.unwrap()[keys::VM_SECRET], secret);

    s.provisioner.apply(&[Rotation::Hook]).await.unwrap();
    let hooked = s.outputs.read().await.unwrap();
    assert_eq!(
        hooked[keys::VM_SECRET],
        secret,
        "the hook leaves running VMs alone"
    );
    assert_ne!(hooked[keys::HOOK_TOKEN], first[keys::HOOK_TOKEN]);
    assert_ne!(hooked[keys::SWEEP_TOKEN], first[keys::SWEEP_TOKEN]);

    s.provisioner.apply(&[Rotation::Vm]).await.unwrap();
    let rotated = s.outputs.read().await.unwrap();
    assert_ne!(rotated[keys::VM_SECRET], secret);
    let secrets = secrets_from_outputs(&s.config, &rotated);
    assert!(
        s.host
            .plan(&secrets)
            .await
            .unwrap()
            .iter()
            .all(Step::is_noop)
    );
    assert!(
        !s.lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains(&secret) || l.contains(&rotated[keys::VM_SECRET]))
    );
}

#[tokio::test]
async fn enables_runners_and_hooks_in_every_project() {
    let s = setup(config(&[7, 8]), true);
    let plan = s.provisioner.plan(&[]).await.unwrap();
    let a = actions(&plan);
    assert_eq!(a["runner-enable:8:ci"], Action::Create);
    assert_eq!(a["hook:8"], Action::Create);
    s.provisioner.apply(&[]).await.unwrap();
    assert_eq!(s.fake.runner_count(), 2);
    assert!(
        s.fake
            .runner_projects()
            .iter()
            .all(|(_, projects)| projects.contains(&8))
    );
    assert_eq!(s.fake.hooks().len(), 2);
    let again = s.provisioner.plan(&[]).await.unwrap();
    assert!(all(&again, Action::Noop), "{again:?}");
}

#[tokio::test]
async fn recreates_a_runner_without_token_and_rotates_everything() {
    let s = setup(config(&[7]), true);
    s.provisioner.apply(&[]).await.unwrap();
    let before = s.outputs.read().await.unwrap();
    s.outputs.clear().await.unwrap();
    s.outputs
        .write(
            &BTreeMap::from([(
                keys::HOOK_TOKEN.to_string(),
                before[keys::HOOK_TOKEN].clone(),
            )]),
            true,
        )
        .await
        .unwrap();
    let plan = s.provisioner.plan(&[]).await.unwrap();
    let step = plan.iter().find(|s| s.id == "runner:ci").unwrap();
    assert_eq!(step.action, Action::Update);
    assert!(step.detail.as_deref().unwrap().contains("token missing"));
    s.provisioner.apply(&[]).await.unwrap();
    let after = s.outputs.read().await.unwrap();
    assert_ne!(
        after[&keys::runner_token("ci")],
        before[&keys::runner_token("ci")]
    );
    assert_eq!(s.fake.runner_count(), 2);

    s.provisioner.apply(&Rotation::ALL).await.unwrap();
    let rotated = s.outputs.read().await.unwrap();
    assert_ne!(
        rotated[&keys::runner_token("ci")],
        after[&keys::runner_token("ci")]
    );
    assert_ne!(rotated[keys::HOOK_TOKEN], after[keys::HOOK_TOKEN]);
    assert_ne!(
        rotated[keys::DISPATCH_SECRET_KEY],
        before[keys::DISPATCH_SECRET_KEY]
    );
    assert_ne!(
        rotated[keys::CACHE_SECRET_KEY],
        before[keys::CACHE_SECRET_KEY]
    );
    let hooks = s.fake.hooks();
    assert_eq!(
        s.fake.hook_token(hooks[0].0).as_deref(),
        Some(rotated[keys::HOOK_TOKEN].as_str())
    );
    assert_eq!(s.fake.runner_count(), 2);
    let plan = s.provisioner.plan(&[]).await.unwrap();
    assert!(all(&plan, Action::Noop), "{plan:?}");
}

#[tokio::test]
async fn destroy_removes_everything() {
    let s = setup(config(&[7]), true);
    s.provisioner.apply(&[]).await.unwrap();
    s.provisioner.destroy().await.unwrap();
    assert_eq!(s.fake.runner_count(), 0);
    assert!(s.fake.hooks().is_empty());
    assert!(s.outputs.read().await.unwrap().is_empty());
    assert_eq!(s.host.url().await.unwrap(), None);
    let plan = s.provider.plan(&[]).await.unwrap();
    assert!(all(&plan, Action::Create), "{plan:?}");
}

#[tokio::test]
async fn works_without_gitlab() {
    let s = setup(config(&[7]), false);
    let plan = s.provisioner.plan(&[]).await.unwrap();
    assert!(
        !plan
            .iter()
            .any(|s| s.id.starts_with("runner") || s.id.starts_with("hook"))
    );
    s.provisioner.apply(&[]).await.unwrap();
    let values = s.outputs.read().await.unwrap();
    assert!(values.contains_key(keys::DISPATCH_URL));
    assert!(!values.contains_key(&keys::runner_token("ci")));
    assert!(
        s.provisioner
            .check()
            .await
            .unwrap_err()
            .to_string()
            .contains("needs a GitLab token")
    );
}

#[tokio::test]
async fn checks_pipelines_against_the_stage_map() {
    let s = setup(config(&[7, 8, 9]), true);
    let job = |name: &str, stage: &str, tags: &[&str]| LintJob {
        name: name.into(),
        stage: Some(stage.into()),
        tag_list: tags.iter().map(|t| t.to_string()).collect(),
    };
    s.fake.set_jobs(7, vec![job("unit", "test", &["ci"])]);
    s.fake.set_jobs(8, vec![job("image", "build", &["ci"])]);
    s.fake.set_lint_errors(9, vec!["invalid yaml".into()]);
    let report = s.provisioner.check().await.unwrap();
    let summary: Vec<(u64, bool, usize, usize)> = report
        .iter()
        .map(|r| (r.project_id, r.valid, r.problems.len(), r.errors.len()))
        .collect();
    assert_eq!(
        summary,
        vec![(7, true, 0, 0), (8, true, 1, 0), (9, false, 0, 1)]
    );
    assert_eq!(report[1].problems[0].job, "image");
}

#[tokio::test]
async fn host_url_is_known_after_apply() {
    let s = setup(config(&[7]), false);
    assert_eq!(s.host.url().await.unwrap(), None);
    s.provisioner.apply(&[]).await.unwrap();
    assert!(s.host.url().await.unwrap().is_some());
    let steps: Vec<Step> = s.provisioner.plan(&[]).await.unwrap();
    assert!(
        steps
            .iter()
            .any(|s| s.id == "dispatcher" && s.action == Action::Noop)
    );
}

/// Fails every step with the given id, like a Scaleway call that rejects a job definition.
struct FailingStep {
    inner: Arc<FakeProvider>,
    fail: &'static str,
}

#[async_trait::async_trait]
impl Provider for FailingStep {
    fn capabilities(&self) -> ciburst_core::types::Capabilities {
        self.inner.capabilities()
    }
    async fn plan(&self, rotate: &[Rotation]) -> ciburst_core::Result<Vec<Step>> {
        self.inner.plan(rotate).await
    }
    async fn apply(&self, steps: &[Step]) -> ciburst_core::Result<ciburst_core::types::Outputs> {
        if steps.iter().any(|s| s.id == self.fail) {
            return Err(ciburst_core::Error::provider("registry image not found"));
        }
        self.inner.apply(steps).await
    }
    async fn destroy(&self) -> ciburst_core::Result<()> {
        self.inner.destroy().await
    }
    async fn start(
        &self,
        request: ciburst_core::types::StartRequest,
    ) -> ciburst_core::Result<String> {
        self.inner.start(request).await
    }
    async fn list(
        &self,
        pool: Option<&str>,
    ) -> ciburst_core::Result<Vec<ciburst_core::types::Run>> {
        self.inner.list(pool).await
    }
    async fn stop(&self, run_id: &str) -> ciburst_core::Result<()> {
        self.inner.stop(run_id).await
    }
}

#[tokio::test]
async fn secrets_survive_a_later_failing_step() {
    let config = config(&[7]);
    let inner = Arc::new(FakeProvider::new(
        config.clone(),
        Arc::new(MemoryStore::new()),
    ));
    let failing = Arc::new(FailingStep {
        inner: inner.clone(),
        fail: "definition:build",
    });
    let outputs = Arc::new(MemoryOutputs::new());
    let host = Arc::new(FakeHost::new(config.clone(), Arc::new(MemoryStore::new())));
    let provisioner = Provisioner::new(
        config.clone(),
        failing,
        host.clone(),
        None,
        outputs.clone(),
        Log::silent(),
    );
    assert!(provisioner.apply(&[]).await.is_err());
    let saved = outputs.read().await.unwrap();
    assert!(saved.contains_key(keys::DISPATCH_SECRET_KEY), "{saved:?}");
    assert!(saved.contains_key(keys::CACHE_SECRET_KEY), "{saved:?}");

    let healthy = Provisioner::new(config, inner, host, None, outputs.clone(), Log::silent());
    healthy.apply(&[]).await.unwrap();
    let after = outputs.read().await.unwrap();
    assert_eq!(
        after[keys::DISPATCH_SECRET_KEY],
        saved[keys::DISPATCH_SECRET_KEY]
    );
}

#[tokio::test]
async fn a_removed_pool_deletes_its_runner() {
    let s = setup(config(&[7]), true);
    s.provisioner.apply(&[]).await.unwrap();
    assert_eq!(s.fake.runner_count(), 2);
    let mut smaller = (*s.config).clone();
    smaller.pools.retain(|p| p.tag != "build");
    let smaller = Arc::new(smaller);
    let provider = Arc::new(FakeProvider::new(
        smaller.clone(),
        Arc::new(MemoryStore::new()),
    ));
    let provisioner = Provisioner::new(
        smaller,
        provider,
        s.host.clone(),
        Some(Arc::new(Gitlab::new(
            "https://gitlab.example",
            FAKE_TOKEN,
            s.fake.clone(),
        ))),
        s.outputs.clone(),
        Log::silent(),
    );
    let plan = provisioner.plan(&[]).await.unwrap();
    let step = plan.iter().find(|s| s.id == "runner:build").unwrap();
    assert_eq!(step.action, Action::Delete);
    provisioner.apply(&[]).await.unwrap();
    assert_eq!(s.fake.runner_count(), 1);
    let again = provisioner.plan(&[]).await.unwrap();
    assert!(!again.iter().any(|s| s.id == "runner:build"), "{again:?}");

    let mut prefix_config = (*s.config).clone();
    prefix_config.name = "de".into();
    prefix_config.pools.retain(|p| p.tag == "ci");
    let prefix_config = Arc::new(prefix_config);
    let other = Provisioner::new(
        prefix_config.clone(),
        Arc::new(FakeProvider::new(
            prefix_config,
            Arc::new(MemoryStore::new()),
        )),
        s.host.clone(),
        Some(Arc::new(Gitlab::new(
            "https://gitlab.example",
            FAKE_TOKEN,
            s.fake.clone(),
        ))),
        Arc::new(MemoryOutputs::new()),
        Log::silent(),
    );
    let foreign = other.plan(&[]).await.unwrap();
    assert!(
        !foreign
            .iter()
            .any(|s| s.action == Action::Delete && s.id.starts_with("runner:")),
        "runner demo-ci belongs to another deployment: {foreign:?}"
    );
}

#[tokio::test]
async fn the_vm_secret_is_not_rotated_while_a_vm_runs() {
    let mut vm_config = (*config(&[7])).clone();
    vm_config.pools[0].runtime = Some("instances".into());
    let s = setup(Arc::new(vm_config), false);
    s.provisioner.apply(&[]).await.unwrap();
    let before = s.outputs.read().await.unwrap()[keys::VM_SECRET].clone();
    let pool = s.config.pools[0].clone();
    s.provider
        .start(ciburst_core::types::StartRequest {
            pool,
            env: BTreeMap::from([
                ("RUNNER_TOKEN".to_string(), "glrt-ci".to_string()),
                ("CIBURST_VM_SECRET".to_string(), before.clone()),
            ]),
            labels: BTreeMap::new(),
        })
        .await
        .unwrap();
    let error = s.provisioner.apply(&[Rotation::Vm]).await.unwrap_err();
    assert!(error.to_string().contains("1 VMs are running"), "{error}");
    assert_eq!(s.outputs.read().await.unwrap()[keys::VM_SECRET], before);
}
