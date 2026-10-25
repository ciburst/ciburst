use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::gitlab::{Gitlab, VariableSpec};
use crate::log::Log;
use crate::provision::keys;
use crate::store::write_private;
use crate::types::{Action, Outputs, Step};
use crate::{Error, Result};

/// Environment scope of the outputs in project variables (spec D39): only jobs of the
/// environment `ciburst` see them.
pub const OUTPUTS_SCOPE: &str = "ciburst";

pub fn format_plan(steps: &[Step]) -> String {
    if steps.is_empty() {
        return "nothing to do".to_string();
    }
    let width = steps
        .iter()
        .map(|step| step.action.as_str().len())
        .max()
        .unwrap_or(0);
    steps
        .iter()
        .map(|step| {
            let detail = step
                .detail
                .as_ref()
                .map(|d| format!("  ({d})"))
                .unwrap_or_default();
            format!("{:width$}  {}{detail}", step.action.as_str(), step.resource)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn summarize_plan(steps: &[Step]) -> String {
    let count = |action: Action| steps.iter().filter(|s| s.action == action).count();
    format!(
        "{} to create, {} to update, {} to delete, {} unchanged",
        count(Action::Create),
        count(Action::Update),
        count(Action::Delete),
        count(Action::Noop)
    )
}

pub fn parse_env(text: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let mut value = value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        values.insert(key.to_string(), value.to_string());
    }
    values
}

pub fn format_env(values: &BTreeMap<String, String>) -> String {
    let mut text = String::new();
    for (key, value) in values {
        text.push_str(key);
        text.push('=');
        text.push_str(value);
        text.push('\n');
    }
    text
}

pub async fn read_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(parse_env(&text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error.into()),
    }
}

/// Merges values into an env file. Existing keys win unless `overwrite` is set, so a secret
/// shown once by a provider survives reruns.
pub async fn write_env_file(
    path: &Path,
    values: &BTreeMap<String, String>,
    overwrite: bool,
) -> Result<BTreeMap<String, String>> {
    let mut merged = read_env_file(path).await?;
    for (key, value) in values {
        if overwrite || !merged.contains_key(key) {
            merged.insert(key.clone(), value.clone());
        }
    }
    write_private(path, format_env(&merged).as_bytes()).await?;
    Ok(merged)
}

#[async_trait]
pub trait OutputsStore: Send + Sync {
    async fn read(&self) -> Result<Outputs>;
    async fn write(&self, values: &Outputs, overwrite: bool) -> Result<Outputs>;
    async fn clear(&self) -> Result<()>;
}

pub struct FileOutputs {
    path: PathBuf,
}

impl FileOutputs {
    pub fn new(path: impl Into<PathBuf>) -> FileOutputs {
        FileOutputs { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl OutputsStore for FileOutputs {
    async fn read(&self) -> Result<Outputs> {
        read_env_file(&self.path).await
    }

    async fn write(&self, values: &Outputs, overwrite: bool) -> Result<Outputs> {
        write_env_file(&self.path, values, overwrite).await
    }

    async fn clear(&self) -> Result<()> {
        write_private(&self.path, b"").await
    }
}

#[derive(Default)]
pub struct MemoryOutputs {
    values: Mutex<Outputs>,
}

impl MemoryOutputs {
    pub fn new() -> MemoryOutputs {
        MemoryOutputs::default()
    }
}

#[async_trait]
impl OutputsStore for MemoryOutputs {
    async fn read(&self) -> Result<Outputs> {
        Ok(self.values.lock().expect("outputs").clone())
    }

    async fn write(&self, values: &Outputs, overwrite: bool) -> Result<Outputs> {
        let mut current = self.values.lock().expect("outputs");
        for (key, value) in values {
            if overwrite || !current.contains_key(key) {
                current.insert(key.clone(), value.clone());
            }
        }
        Ok(current.clone())
    }

    async fn clear(&self) -> Result<()> {
        self.values.lock().expect("outputs").clear();
        Ok(())
    }
}

/// Outputs as variables of the setup's own project, for `ciburst --ci` (M5 rule R2). GitLab
/// never returns a hidden variable's value, so reads come from the job environment, where
/// GitLab put the variables, overlaid by this run's writes. Variables are protected, raw and in
/// scope [`OUTPUTS_SCOPE`]; created masked and hidden, or unmasked where GitLab refuses.
/// `PROVIDER_ENV` values stay in memory.
pub struct GitlabOutputs {
    gitlab: Arc<Gitlab>,
    project_id: u64,
    keys: BTreeSet<String>,
    values: Mutex<Outputs>,
    /// Keys present in the scope, listed on the first write.
    existing: Mutex<Option<BTreeSet<String>>>,
    log: Log,
}

impl GitlabOutputs {
    /// `keys` are the output keys (`provision::keys::output_keys`); their values in `env` are
    /// what earlier runs stored.
    pub fn new(
        gitlab: Arc<Gitlab>,
        project_id: u64,
        keys: Vec<String>,
        env: &BTreeMap<String, String>,
        log: Log,
    ) -> GitlabOutputs {
        let values = keys
            .iter()
            .filter_map(|key| {
                env.get(key)
                    .filter(|value| !value.is_empty())
                    .map(|value| (key.clone(), value.clone()))
            })
            .collect();
        GitlabOutputs {
            gitlab,
            project_id,
            keys: keys.into_iter().collect(),
            values: Mutex::new(values),
            existing: Mutex::new(None),
            log,
        }
    }

    async fn existing(&self) -> Result<BTreeSet<String>> {
        if let Some(existing) = self.existing.lock().expect("outputs").clone() {
            return Ok(existing);
        }
        let listed: BTreeSet<String> = self
            .gitlab
            .list_variables(self.project_id)
            .await?
            .into_iter()
            .filter(|variable| variable.environment_scope == OUTPUTS_SCOPE)
            .map(|variable| variable.key)
            .collect();
        *self.existing.lock().expect("outputs") = Some(listed.clone());
        Ok(listed)
    }

    /// Secrets are created masked and hidden; a 400 to that create is GitLab refusing to mask
    /// the value, and the unmasked create that follows reports any other cause.
    async fn store(&self, key: &str, value: &str, exists: bool) -> Result<()> {
        let secret = keys::is_secret(key);
        let spec = VariableSpec {
            key,
            value,
            environment_scope: OUTPUTS_SCOPE,
            protected: true,
            raw: true,
            masked_and_hidden: secret,
        };
        if exists {
            self.gitlab.update_variable(self.project_id, &spec).await?;
            return Ok(());
        }
        match self.gitlab.create_variable(self.project_id, &spec).await {
            Ok(_) => {}
            Err(Error::Gitlab { status: 400, .. }) if secret => {
                let unmasked = VariableSpec {
                    masked_and_hidden: false,
                    ..spec
                };
                self.gitlab
                    .create_variable(self.project_id, &unmasked)
                    .await?;
                self.log.warn("output_unmasked", &[("key", key)]);
            }
            Err(error) => return Err(error),
        }
        if let Some(existing) = self.existing.lock().expect("outputs").as_mut() {
            existing.insert(key.to_string());
        }
        Ok(())
    }
}

#[async_trait]
impl OutputsStore for GitlabOutputs {
    async fn read(&self) -> Result<Outputs> {
        Ok(self.values.lock().expect("outputs").clone())
    }

    async fn write(&self, values: &Outputs, overwrite: bool) -> Result<Outputs> {
        for (key, value) in values {
            let current = self.values.lock().expect("outputs").get(key).cloned();
            if current.is_some() && !overwrite {
                continue;
            }
            if !keys::PROVIDER_ENV.contains(&key.as_str()) {
                let exists = self.existing().await?.contains(key);
                if !exists || current.as_ref() != Some(value) {
                    self.store(key, value, exists).await?;
                }
            }
            self.values
                .lock()
                .expect("outputs")
                .insert(key.clone(), value.clone());
        }
        self.read().await
    }

    /// Deletes the output variables of the scope; other variables there, the inputs among
    /// them, stay.
    async fn clear(&self) -> Result<()> {
        for variable in self.gitlab.list_variables(self.project_id).await? {
            if variable.environment_scope == OUTPUTS_SCOPE && self.keys.contains(&variable.key) {
                self.gitlab
                    .delete_variable(self.project_id, &variable.key, OUTPUTS_SCOPE)
                    .await?;
            }
        }
        self.values.lock().expect("outputs").clear();
        *self.existing.lock().expect("outputs") = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps() -> Vec<Step> {
        vec![
            Step::new("a", Action::Create, "project demo"),
            Step::new("b", Action::Noop, "job definition demo-ci").with_detail("unchanged"),
            Step::new("c", Action::Delete, "job definition demo-old"),
        ]
    }

    #[test]
    fn formats_and_summarizes() {
        assert_eq!(
            format_plan(&steps()),
            "create  project demo\nnoop    job definition demo-ci  (unchanged)\ndelete  job definition demo-old"
        );
        assert_eq!(
            summarize_plan(&steps()),
            "1 to create, 0 to update, 1 to delete, 1 unchanged"
        );
        assert_eq!(format_plan(&[]), "nothing to do");
    }

    #[test]
    fn parses_env_text() {
        let parsed = parse_env("# c\n\nA=1\nB=\"two words\"\nC='x'\nBAD\n=nokey\nD=a=b\n");
        let expected: BTreeMap<String, String> =
            [("A", "1"), ("B", "two words"), ("C", "x"), ("D", "a=b")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        assert_eq!(parsed, expected);
        assert_eq!(format_env(&expected), "A=1\nB=two words\nC=x\nD=a=b\n");
    }

    #[tokio::test]
    async fn env_file_keeps_existing_keys_unless_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("outputs.env");
        let one = |k: &str, v: &str| BTreeMap::from([(k.to_string(), v.to_string())]);
        assert!(read_env_file(&path).await.unwrap().is_empty());
        write_env_file(&path, &one("SECRET", "first"), false)
            .await
            .unwrap();
        write_env_file(&path, &one("SECRET", "second"), false)
            .await
            .unwrap();
        assert_eq!(read_env_file(&path).await.unwrap()["SECRET"], "first");
        write_env_file(&path, &one("SECRET", "third"), true)
            .await
            .unwrap();
        assert_eq!(read_env_file(&path).await.unwrap()["SECRET"], "third");
        let outputs = FileOutputs::new(&path);
        outputs.clear().await.unwrap();
        assert!(outputs.read().await.unwrap().is_empty());
    }

    mod gitlab_outputs {
        use super::*;
        use crate::testing::gitlab_fake::{FAKE_TOKEN, FakeGitlab, FakeVariable};

        const PROJECT: u64 = 7;
        const TOKEN: &str = "abcdefghijklmnopqrstuvwxyz012345";

        fn keys() -> Vec<String> {
            let config = crate::config::Config::ci_defaults(PROJECT, "https://gitlab.example");
            keys::output_keys(&config)
        }

        struct Setup {
            fake: Arc<FakeGitlab>,
            outputs: GitlabOutputs,
            lines: Arc<Mutex<Vec<String>>>,
        }

        fn setup(env: &[(&str, &str)]) -> Setup {
            let fake = Arc::new(FakeGitlab::new());
            outputs_on(fake, env)
        }

        fn outputs_on(fake: Arc<FakeGitlab>, env: &[(&str, &str)]) -> Setup {
            let gitlab = Arc::new(Gitlab::new(
                "https://gitlab.example",
                FAKE_TOKEN,
                fake.clone(),
            ));
            let env: BTreeMap<String, String> = env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let (log, lines) = Log::memory();
            let outputs = GitlabOutputs::new(gitlab, PROJECT, keys(), &env, log);
            Setup {
                fake,
                outputs,
                lines,
            }
        }

        fn one(key: &str, value: &str) -> Outputs {
            Outputs::from([(key.to_string(), value.to_string())])
        }

        #[tokio::test]
        async fn creates_protected_raw_hidden_variables_in_the_scope() {
            let s = setup(&[]);
            let written = s
                .outputs
                .write(
                    &Outputs::from([
                        (keys::HOOK_TOKEN.to_string(), TOKEN.to_string()),
                        (
                            keys::DISPATCH_URL.to_string(),
                            "https://demo.functions.example/".to_string(),
                        ),
                    ]),
                    false,
                )
                .await
                .unwrap();
            assert_eq!(written[keys::HOOK_TOKEN], TOKEN);
            let stored = s.fake.variables(PROJECT);
            assert_eq!(stored.len(), 2);
            for variable in &stored {
                assert_eq!(variable.environment_scope, "ciburst");
                assert!(variable.protected && variable.raw);
                let secret = variable.key == keys::HOOK_TOKEN;
                assert_eq!(variable.masked && variable.hidden, secret, "{variable:?}");
            }
            assert!(s.lines.lock().unwrap().is_empty());
            assert_eq!(s.outputs.read().await.unwrap(), written);
        }

        #[tokio::test]
        async fn updates_an_existing_key_in_place() {
            let fake = Arc::new(FakeGitlab::new());
            let first = outputs_on(fake.clone(), &[]);
            first
                .outputs
                .write(&one(keys::VM_SECRET, TOKEN), false)
                .await
                .unwrap();
            let next = outputs_on(fake.clone(), &[(keys::VM_SECRET, TOKEN)]);
            assert_eq!(next.outputs.read().await.unwrap()[keys::VM_SECRET], TOKEN);
            next.outputs
                .write(&one(keys::VM_SECRET, "changed-secret-value"), false)
                .await
                .unwrap();
            assert_eq!(
                fake.variables(PROJECT)[0].value,
                TOKEN,
                "kept without overwrite"
            );
            next.outputs
                .write(&one(keys::VM_SECRET, TOKEN), true)
                .await
                .unwrap();
            assert!(
                !fake.calls().iter().any(|c| c.method == "PUT"),
                "an unchanged value is not sent"
            );
            next.outputs
                .write(&one(keys::VM_SECRET, "changed-secret-value"), true)
                .await
                .unwrap();
            let stored = fake.variables(PROJECT);
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].value, "changed-secret-value");
            assert!(stored[0].hidden);
            assert_eq!(
                next.outputs.read().await.unwrap()[keys::VM_SECRET],
                "changed-secret-value"
            );
        }

        #[tokio::test]
        async fn a_secret_gitlab_cannot_mask_is_stored_unmasked_and_logged() {
            let s = setup(&[]);
            let key = keys::HOOK_TOKEN;
            s.outputs.write(&one(key, "short"), false).await.unwrap();
            let stored = s.fake.variables(PROJECT);
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].value, "short");
            assert!(!stored[0].masked && !stored[0].hidden);
            assert!(stored[0].protected && stored[0].raw);
            assert_eq!(stored[0].environment_scope, "ciburst");
            assert_eq!(
                *s.lines.lock().unwrap(),
                [format!(r#"warn output_unmasked key="{key}""#)]
            );
        }

        #[tokio::test]
        async fn ids_urls_and_the_agent_hash_are_stored_readable() {
            let s = setup(&[]);
            for key in [keys::runner_id("vm"), keys::AGENT_SHA256.to_string()] {
                s.outputs
                    .write(&one(&key, "0a1b2c3d4e5f"), false)
                    .await
                    .unwrap();
            }
            for variable in s.fake.variables(PROJECT) {
                assert!(!variable.masked && !variable.hidden, "{variable:?}");
                assert!(variable.protected && variable.raw);
            }
            assert!(s.lines.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn hidden_values_come_from_the_job_environment() {
            let fake = Arc::new(FakeGitlab::new());
            let first = outputs_on(fake.clone(), &[]);
            first
                .outputs
                .write(&one(keys::SWEEP_TOKEN, TOKEN), false)
                .await
                .unwrap();
            let gitlab = Gitlab::new("https://gitlab.example", FAKE_TOKEN, fake.clone());
            let listed = gitlab.list_variables(PROJECT).await.unwrap();
            assert_eq!(listed[0].key, keys::SWEEP_TOKEN);
            assert_eq!(listed[0].value, None, "the API never returns it");

            let next = outputs_on(
                fake.clone(),
                &[
                    (keys::SWEEP_TOKEN, TOKEN),
                    ("CIBURST_SCW_SECRET_KEY", "an-input"),
                    ("UNRELATED", "x"),
                ],
            );
            assert_eq!(
                next.outputs.read().await.unwrap(),
                one(keys::SWEEP_TOKEN, TOKEN),
                "only output keys"
            );
            let empty = outputs_on(fake, &[]);
            assert!(empty.outputs.read().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn provider_env_stays_in_memory() {
            let s = setup(&[]);
            s.outputs
                .write(&one("HCLOUD_TOKEN", "hetzner-token-value"), true)
                .await
                .unwrap();
            assert!(s.fake.variables(PROJECT).is_empty());
            assert!(s.fake.calls().is_empty());
            assert_eq!(
                s.outputs.read().await.unwrap()["HCLOUD_TOKEN"],
                "hetzner-token-value"
            );
        }

        #[tokio::test]
        async fn clear_deletes_only_output_keys_of_the_scope() {
            let s = setup(&[]);
            s.outputs
                .write(
                    &Outputs::from([
                        (keys::HOOK_TOKEN.to_string(), TOKEN.to_string()),
                        (keys::runner_token("vm"), "glrt-abcdefgh".to_string()),
                    ]),
                    false,
                )
                .await
                .unwrap();
            let input = FakeVariable {
                environment_scope: "ciburst".into(),
                ..FakeVariable::plain("CIBURST_SCW_SECRET_KEY", "input-secret")
            };
            s.fake.add_variable(PROJECT, input.clone());
            let elsewhere = FakeVariable::plain(keys::HOOK_TOKEN, "a project-wide one");
            s.fake.add_variable(PROJECT, elsewhere.clone());
            s.outputs.clear().await.unwrap();
            assert_eq!(s.fake.variables(PROJECT), [input, elsewhere]);
            assert!(s.outputs.read().await.unwrap().is_empty());
            let deletes: Vec<String> = s
                .fake
                .calls()
                .into_iter()
                .filter(|c| c.method == "DELETE")
                .map(|c| format!("{}?{}", c.path, c.query))
                .collect();
            assert_eq!(
                deletes,
                [
                    "/projects/7/variables/CIBURST_HOOK_TOKEN?filter[environment_scope]=ciburst",
                    "/projects/7/variables/CIBURST_RUNNER_TOKEN_VM?filter[environment_scope]=ciburst",
                ]
            );
            s.outputs
                .write(&one(keys::HOOK_TOKEN, TOKEN), false)
                .await
                .unwrap();
            assert_eq!(s.fake.variables(PROJECT).len(), 3, "created again");
        }
    }

    #[tokio::test]
    async fn memory_outputs_behave_the_same() {
        let outputs = MemoryOutputs::new();
        let values = Outputs::from([("A".to_string(), "1".to_string())]);
        outputs.write(&values, false).await.unwrap();
        let next = Outputs::from([
            ("A".to_string(), "2".to_string()),
            ("B".to_string(), "3".to_string()),
        ]);
        outputs.write(&next, false).await.unwrap();
        let read = outputs.read().await.unwrap();
        assert_eq!(read["A"], "1");
        assert_eq!(read["B"], "3");
        outputs.write(&next, true).await.unwrap();
        assert_eq!(outputs.read().await.unwrap()["A"], "2");
        outputs.clear().await.unwrap();
        assert!(outputs.read().await.unwrap().is_empty());
    }
}
