//! Fleet state (M6.3 rules R1 and R2, D57 and D58): the routing config and the member projects,
//! kept in the deployment's bucket so that a change takes effect without a function deploy.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::store::Store;

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

/// Fleet state in a JSON document slot, `{"config": <text>, "members": {<id>: <member>}}`, for
/// the fake modules and the developer path. It keeps secrets in plain text.
pub struct StoreFleet {
    store: Arc<dyn Store>,
}

#[derive(Default, Serialize, Deserialize)]
struct FleetDocument {
    #[serde(default)]
    config: Option<String>,
    #[serde(default)]
    members: BTreeMap<u64, Member>,
}

impl StoreFleet {
    pub fn new(store: Arc<dyn Store>) -> StoreFleet {
        StoreFleet { store }
    }

    async fn load(&self) -> Result<FleetDocument> {
        Ok(match self.store.load().await? {
            Some(value) => serde_json::from_value(value)?,
            None => FleetDocument::default(),
        })
    }

    async fn save(&self, document: &FleetDocument) -> Result<()> {
        self.store.save(Some(serde_json::to_value(document)?)).await
    }
}

#[async_trait]
impl FleetStore for StoreFleet {
    async fn config(&self) -> Result<Option<String>> {
        Ok(self.load().await?.config)
    }

    async fn put_config(&self, json: &str) -> Result<()> {
        let mut document = self.load().await?;
        document.config = Some(json.to_string());
        self.save(&document).await
    }

    async fn members(&self) -> Result<Vec<Member>> {
        Ok(self.load().await?.members.into_values().collect())
    }

    async fn put_member(&self, member: &Member) -> Result<()> {
        let mut document = self.load().await?;
        document.members.insert(member.project_id, member.clone());
        self.save(&document).await
    }

    async fn delete_member(&self, project_id: u64) -> Result<()> {
        let mut document = self.load().await?;
        if document.members.remove(&project_id).is_some() {
            self.save(&document).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_store_fleet_keeps_its_state_in_the_slot() {
        let slot: Arc<dyn Store> = Arc::new(crate::store::MemoryStore::new());
        let fleet = StoreFleet::new(slot.clone());
        assert_eq!(fleet.config().await.unwrap(), None);
        fleet.put_config("{\"a\":1}").await.unwrap();
        let member = Member {
            project_id: 9,
            hook_secret: "s".into(),
            runners: [("vm".to_string(), "glrt-9".to_string())].into(),
            joined_at: 3,
        };
        fleet.put_member(&member).await.unwrap();
        let again = StoreFleet::new(slot);
        assert_eq!(again.config().await.unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(again.members().await.unwrap(), [member]);
        again.delete_member(9).await.unwrap();
        again.delete_member(9).await.unwrap();
        assert!(fleet.members().await.unwrap().is_empty());
    }

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
