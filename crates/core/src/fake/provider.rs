use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rand::Rng;
use rand::distr::Alphanumeric;
use serde::{Deserialize, Serialize};

use crate::config::{CacheType, Config, Pool};
use crate::store::Store;
use crate::types::{
    Action, Billing, Capabilities, KeyPair, Outputs, Provider, Rotation, Run, RunState,
    StartRequest, Step,
};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Definition {
    image: String,
    vcpu: u32,
    memory_mib: u32,
    storage_mib: u32,
    timeout_s: u32,
    wait_timeout_s: u32,
}

impl From<&Pool> for Definition {
    fn from(pool: &Pool) -> Definition {
        Definition {
            image: pool.image.clone(),
            vcpu: pool.vcpu,
            memory_mib: pool.memory_mib,
            storage_mib: pool.storage_mib,
            timeout_s: pool.timeout_s,
            wait_timeout_s: pool.wait_timeout_s,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bucket {
    name: String,
    expire_days: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    project: Option<String>,
    keys: BTreeMap<String, KeyPair>,
    bucket: Option<Bucket>,
    definitions: BTreeMap<String, Definition>,
    runs: Vec<Run>,
}

pub struct FakeProvider {
    config: Arc<Config>,
    store: Arc<dyn Store>,
}

impl FakeProvider {
    pub fn new(config: Arc<Config>, store: Arc<dyn Store>) -> FakeProvider {
        FakeProvider { config, store }
    }

    async fn state(&self) -> Result<State> {
        match self.store.load().await? {
            Some(value) => Ok(serde_json::from_value(value)?),
            None => Ok(State::default()),
        }
    }

    async fn save(&self, state: &State) -> Result<()> {
        self.store.save(Some(serde_json::to_value(state)?)).await
    }

    fn definition_name(&self, tag: &str) -> String {
        format!("{}-{tag}", self.config.name)
    }

    fn key_step(&self, id: &str, label: &str, present: bool, rotate: bool) -> Step {
        let action = if !present {
            Action::Create
        } else if rotate {
            Action::Update
        } else {
            Action::Noop
        };
        let step = Step::new(id, action, format!("api key {}-{label}", self.config.name));
        if action == Action::Update {
            step.with_detail("rotate")
        } else {
            step
        }
    }

    /// Test helper outside the contract.
    pub async fn set_run_state(&self, run_id: &str, run_state: RunState) -> Result<()> {
        let mut state = self.state().await?;
        let run = state
            .runs
            .iter_mut()
            .find(|r| r.run_id == run_id)
            .ok_or_else(|| Error::provider(format!("unknown run {run_id}")))?;
        run.state = run_state;
        self.save(&state).await
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            name: "fake".into(),
            docker_daemon: false,
            max_vcpu: 6,
            max_memory_mib: 16384,
            max_storage_mib: 10240,
            max_duration_s: 86400,
            archs: vec!["amd64".into()],
            billing: Billing::PerSecond,
        }
    }

    async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
        let state = self.state().await?;
        let name = &self.config.name;
        let mut steps = vec![Step::new(
            "project",
            if state.project.is_some() {
                Action::Noop
            } else {
                Action::Create
            },
            format!("project {name}"),
        )];
        steps.push(self.key_step(
            "key:dispatch",
            "dispatch",
            state.keys.contains_key("dispatch"),
            rotate.contains(&Rotation::Key),
        ));
        if self.config.cache.kind == CacheType::S3 {
            steps.push(self.key_step(
                "key:cache",
                "cache",
                state.keys.contains_key("cache"),
                rotate.contains(&Rotation::Cache),
            ));
            let action = match &state.bucket {
                None => Action::Create,
                Some(bucket) if bucket.expire_days == self.config.cache.expire_days => Action::Noop,
                Some(_) => Action::Update,
            };
            steps.push(Step::new(
                "bucket",
                action,
                format!("cache bucket {name}-cache"),
            ));
        } else {
            if state.keys.contains_key("cache") {
                steps.push(Step::new(
                    "key:cache",
                    Action::Delete,
                    format!("api key {name}-cache"),
                ));
            }
            if state.bucket.is_some() {
                steps.push(Step::new(
                    "bucket",
                    Action::Delete,
                    format!("cache bucket {name}-cache"),
                ));
            }
        }
        for pool in &self.config.pools {
            let action = match state.definitions.get(&pool.tag) {
                None => Action::Create,
                Some(current) if *current == Definition::from(pool) => Action::Noop,
                Some(_) => Action::Update,
            };
            steps.push(Step::new(
                format!("definition:{}", pool.tag),
                action,
                format!("job definition {}", self.definition_name(&pool.tag)),
            ));
        }
        for tag in state.definitions.keys() {
            if self.config.pool_by_tag(tag).is_none() {
                steps.push(Step::new(
                    format!("definition:{tag}"),
                    Action::Delete,
                    format!("job definition {}", self.definition_name(tag)),
                ));
            }
        }
        Ok(steps)
    }

    async fn apply(&self, steps: &[Step]) -> Result<Outputs> {
        let mut state = self.state().await?;
        let mut outputs = Outputs::new();
        for step in steps {
            if step.is_noop() {
                continue;
            }
            match step.id.as_str() {
                "project" => state.project = Some(random_id(32)),
                "key:dispatch" => {
                    let key = fake_key();
                    outputs.insert("CIBURST_DISPATCH_ACCESS_KEY".into(), key.access_key.clone());
                    outputs.insert("CIBURST_DISPATCH_SECRET_KEY".into(), key.secret_key.clone());
                    state.keys.insert("dispatch".into(), key);
                }
                "key:cache" => {
                    if step.action == Action::Delete {
                        state.keys.remove("cache");
                    } else {
                        let key = fake_key();
                        outputs.insert("CIBURST_CACHE_ACCESS_KEY".into(), key.access_key.clone());
                        outputs.insert("CIBURST_CACHE_SECRET_KEY".into(), key.secret_key.clone());
                        state.keys.insert("cache".into(), key);
                    }
                }
                "bucket" => {
                    state.bucket = if step.action == Action::Delete {
                        None
                    } else {
                        Some(Bucket {
                            name: format!("{}-cache", self.config.name),
                            expire_days: self.config.cache.expire_days,
                        })
                    };
                }
                id => {
                    let tag = id
                        .strip_prefix("definition:")
                        .ok_or_else(|| Error::provider(format!("unknown step {id}")))?;
                    if step.action == Action::Delete {
                        state.definitions.remove(tag);
                    } else {
                        let pool = self
                            .config
                            .pool_by_tag(tag)
                            .ok_or_else(|| Error::provider(format!("unknown pool {tag}")))?;
                        state
                            .definitions
                            .insert(tag.to_string(), Definition::from(pool));
                    }
                }
            }
        }
        self.save(&state).await?;
        Ok(outputs)
    }

    async fn destroy(&self) -> Result<()> {
        self.save(&State::default()).await
    }

    async fn start(&self, request: StartRequest) -> Result<String> {
        let mut state = self.state().await?;
        if !state.definitions.contains_key(&request.pool.tag) {
            return Err(Error::provider(format!(
                "pool {} is not provisioned",
                request.pool.tag
            )));
        }
        if request.env.get("RUNNER_TOKEN").is_none_or(|t| t.is_empty()) {
            return Err(Error::provider("RUNNER_TOKEN missing"));
        }
        let run = Run {
            run_id: format!("fake-{}", random_id(12)),
            pool: request.pool.tag.clone(),
            state: RunState::Queued,
            created_at: now_seconds().to_string(),
            labels: request.labels,
        };
        let run_id = run.run_id.clone();
        state.runs.push(run);
        self.save(&state).await?;
        Ok(run_id)
    }

    async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
        Ok(self
            .state()
            .await?
            .runs
            .into_iter()
            .filter(|run| pool.is_none_or(|p| run.pool == p))
            .collect())
    }

    async fn stop(&self, run_id: &str) -> Result<()> {
        self.set_run_state(run_id, RunState::Done).await
    }
}

fn random_id(len: usize) -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(len)
        .map(|c| char::from(c).to_ascii_lowercase())
        .collect()
}

fn fake_key() -> KeyPair {
    KeyPair {
        access_key: format!("SCWFAKE{}", random_id(12).to_ascii_uppercase()),
        secret_key: random_id(36),
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
