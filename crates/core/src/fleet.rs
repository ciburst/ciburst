//! Fleet state (M6.3 rules R1 and R2, D57 and D58): the routing config and the member projects,
//! kept in the deployment's bucket so that a change takes effect without a function deploy.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;

/// A project the fleet serves (M6.3 rule R2). The host is the first member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    #[serde(rename = "projectId")]
    pub project_id: u64,
    /// The `X-Gitlab-Token` of the member's webhook.
    #[serde(rename = "hookSecret")]
    pub hook_secret: String,
    /// The token of each runner the member registered, by pool tag.
    pub runners: BTreeMap<String, String>,
    /// Unix seconds of the join.
    #[serde(rename = "joinedAt")]
    pub joined_at: u64,
}

/// Where a fleet keeps its state. A store that persists members encrypts their secrets at rest
/// (M6.3 rule R2); callers always see them in plain text.
#[async_trait]
pub trait FleetStore: Send + Sync {
    /// The routing config as JSON, `None` before the first setup wrote it.
    async fn config(&self) -> Result<Option<String>>;
    /// Replaces the routing config.
    async fn put_config(&self, json: &str) -> Result<()>;
    /// Every member, in any order.
    async fn members(&self) -> Result<Vec<Member>>;
    /// Adds a member or replaces the record of the same project.
    async fn put_member(&self, member: &Member) -> Result<()>;
    /// Removes a member; no error when there is none.
    async fn delete_member(&self, project_id: u64) -> Result<()>;
}

/// In-memory fleet store for tests and the fake modules.
#[derive(Default)]
pub struct MemoryFleetStore {
    config: Mutex<Option<String>>,
    members: Mutex<BTreeMap<u64, Member>>,
}

impl MemoryFleetStore {
    pub fn new() -> MemoryFleetStore {
        MemoryFleetStore::default()
    }
}

#[async_trait]
impl FleetStore for MemoryFleetStore {
    async fn config(&self) -> Result<Option<String>> {
        Ok(self.config.lock().expect("fleet").clone())
    }

    async fn put_config(&self, json: &str) -> Result<()> {
        *self.config.lock().expect("fleet") = Some(json.to_string());
        Ok(())
    }

    async fn members(&self) -> Result<Vec<Member>> {
        Ok(self
            .members
            .lock()
            .expect("fleet")
            .values()
            .cloned()
            .collect())
    }

    async fn put_member(&self, member: &Member) -> Result<()> {
        self.members
            .lock()
            .expect("fleet")
            .insert(member.project_id, member.clone());
        Ok(())
    }

    async fn delete_member(&self, project_id: u64) -> Result<()> {
        self.members.lock().expect("fleet").remove(&project_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn members_are_kept_by_project() {
        let store = MemoryFleetStore::new();
        assert_eq!(store.config().await.unwrap(), None);
        store.put_config("{}").await.unwrap();
        assert_eq!(store.config().await.unwrap().as_deref(), Some("{}"));
        let member = |project_id: u64, secret: &str| Member {
            project_id,
            hook_secret: secret.into(),
            runners: [("vm".to_string(), "glrt-a".to_string())].into(),
            joined_at: 1,
        };
        store.put_member(&member(7, "a")).await.unwrap();
        store.put_member(&member(8, "b")).await.unwrap();
        store.put_member(&member(7, "c")).await.unwrap();
        let members = store.members().await.unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].hook_secret, "c");
        store.delete_member(8).await.unwrap();
        store.delete_member(9).await.unwrap();
        assert_eq!(store.members().await.unwrap(), [member(7, "c")]);
        assert_eq!(
            serde_json::to_value(member(7, "c")).unwrap()["projectId"],
            7
        );
    }
}
