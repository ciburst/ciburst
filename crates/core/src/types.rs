use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::config::Pool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Create,
    Update,
    Delete,
    Noop,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Update => "update",
            Action::Delete => "delete",
            Action::Noop => "noop",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One planned change. `id` is stable across plans so that `apply` can find its step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub action: Action,
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Step {
    pub fn new(id: impl Into<String>, action: Action, resource: impl Into<String>) -> Self {
        Step {
            id: id.into(),
            action,
            resource: resource.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn is_noop(&self) -> bool {
        self.action == Action::Noop
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Queued,
    Running,
    Done,
    Failed,
}

impl RunState {
    pub fn is_live(self) -> bool {
        matches!(self, RunState::Queued | RunState::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub run_id: String,
    pub pool: String,
    pub state: RunState,
    pub created_at: String,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Billing {
    PerSecond,
    PerMinute,
    PerHour,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub name: String,
    pub docker_daemon: bool,
    pub max_vcpu: u32,
    pub max_memory_mib: u32,
    pub max_storage_mib: u32,
    pub max_duration_s: u32,
    pub archs: Vec<String>,
    pub billing: Billing,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPair {
    pub access_key: String,
    pub secret_key: String,
}

/// Runtime secrets of one deployment. Never logged, never written anywhere but the outputs file
/// and the host's secret environment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secrets {
    pub hook_token: Option<String>,
    pub runner_tokens: BTreeMap<String, String>,
    pub dispatch: Option<KeyPair>,
    pub cache: Option<KeyPair>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    Runner,
    Hook,
    Key,
    Cache,
}

impl Rotation {
    pub const ALL: [Rotation; 4] = [
        Rotation::Runner,
        Rotation::Hook,
        Rotation::Key,
        Rotation::Cache,
    ];

    pub fn parse(value: &str) -> Option<Rotation> {
        Rotation::ALL.into_iter().find(|r| r.as_str() == value)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Rotation::Runner => "runner",
            Rotation::Hook => "hook",
            Rotation::Key => "key",
            Rotation::Cache => "cache",
        }
    }

    pub fn names() -> String {
        Rotation::ALL
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Key value outputs of a phase, written to `.local/outputs.env`.
pub type Outputs = BTreeMap<String, String>;

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub pool: Pool,
    pub env: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
}

/// Where runs execute. See `docs/PROVIDERS.md` for the contract.
#[async_trait]
pub trait Provider: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>>;
    async fn apply(&self, steps: &[Step]) -> Result<Outputs>;
    async fn destroy(&self) -> Result<()>;
    async fn start(&self, request: StartRequest) -> Result<String>;
    async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>>;
    async fn stop(&self, run_id: &str) -> Result<()>;
}

/// Where the dispatcher runs.
#[async_trait]
pub trait Host: Send + Sync {
    async fn plan(&self, secrets: &Secrets) -> Result<Vec<Step>>;
    async fn apply(&self, steps: &[Step], secrets: &Secrets) -> Result<Outputs>;
    async fn destroy(&self) -> Result<()>;
    async fn url(&self) -> Result<Option<String>>;
}
