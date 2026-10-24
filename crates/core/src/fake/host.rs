use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::store::Store;
use crate::types::{Action, Host, Outputs, Secrets, Step};
use crate::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    digest: String,
    url: String,
}

/// Remembers a digest of config and secrets and a fake URL.
pub struct FakeHost {
    config: Arc<Config>,
    store: Arc<dyn Store>,
}

impl FakeHost {
    pub fn new(config: Arc<Config>, store: Arc<dyn Store>) -> FakeHost {
        FakeHost { config, store }
    }

    fn digest(&self, secrets: &Secrets) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(serde_json::to_vec(&*self.config)?);
        hasher.update(serde_json::to_vec(secrets)?);
        Ok(format!("{:x}", hasher.finalize()))
    }

    async fn state(&self) -> Result<Option<State>> {
        match self.store.load().await? {
            Some(value) => Ok(Some(serde_json::from_value(value)?)),
            None => Ok(None),
        }
    }

    fn fake_url(&self) -> String {
        format!("https://{}-dispatch.fake.invalid/", self.config.name)
    }
}

#[async_trait]
impl Host for FakeHost {
    async fn plan(&self, secrets: &Secrets) -> Result<Vec<Step>> {
        let digest = self.digest(secrets)?;
        let action = match self.state().await? {
            None => Action::Create,
            Some(state) if state.digest == digest => Action::Noop,
            Some(_) => Action::Update,
        };
        Ok(vec![Step::new(
            "dispatcher",
            action,
            format!("dispatcher {}-dispatch", self.config.name),
        )])
    }

    async fn apply(&self, steps: &[Step], secrets: &Secrets) -> Result<Outputs> {
        for step in steps {
            if step.id != "dispatcher" {
                return Err(Error::provider(format!("unknown step {}", step.id)));
            }
            if !step.is_noop() {
                let state = State {
                    digest: self.digest(secrets)?,
                    url: self.fake_url(),
                };
                self.store.save(Some(serde_json::to_value(state)?)).await?;
            }
        }
        Ok(match self.state().await? {
            Some(state) => Outputs::from([("CIBURST_DISPATCH_URL".to_string(), state.url)]),
            None => Outputs::new(),
        })
    }

    async fn destroy(&self) -> Result<()> {
        self.store.save(None).await
    }

    async fn url(&self) -> Result<Option<String>> {
        Ok(self.state().await?.map(|state| state.url))
    }
}
