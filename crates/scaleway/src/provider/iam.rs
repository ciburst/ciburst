//! IAM `iam/v1alpha1`: one application per key with a project scoped policy and one API key.

use ciburst_core::types::KeyPair;
use ciburst_core::{Error, Result};
use serde_json::{Value, json};

use super::list_all;
use crate::client::ScalewayClient;

const BASE: &str = "/iam/v1alpha1";

/// What exists of one application: its id, its policy ids and its API key access keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppState {
    pub id: String,
    pub policies: Vec<String>,
    pub keys: Vec<String>,
}

impl AppState {
    pub fn complete(&self) -> bool {
        !self.policies.is_empty() && !self.keys.is_empty()
    }
}

fn org(client: &ScalewayClient) -> (&'static str, String) {
    ("organization_id", client.organization_id().to_string())
}

pub async fn find_application(client: &ScalewayClient, name: &str) -> Result<Option<AppState>> {
    let apps = list_all(
        client,
        &format!("{BASE}/applications"),
        vec![org(client), ("name", name.to_string())],
        "applications",
    )
    .await?;
    let Some(id) = apps
        .iter()
        .find(|a| a["name"].as_str() == Some(name))
        .and_then(|a| a["id"].as_str())
    else {
        return Ok(None);
    };
    let policies = list_all(
        client,
        &format!("{BASE}/policies"),
        vec![org(client), ("application_ids", id.to_string())],
        "policies",
    )
    .await?
    .iter()
    .filter(|p| p["application_id"].as_str() == Some(id))
    .filter_map(|p| p["id"].as_str().map(String::from))
    .collect();
    let keys = list_all(
        client,
        &format!("{BASE}/api-keys"),
        vec![org(client), ("application_id", id.to_string())],
        "api_keys",
    )
    .await?
    .iter()
    .filter(|k| k["application_id"].as_str() == Some(id))
    .filter_map(|k| k["access_key"].as_str().map(String::from))
    .collect();
    Ok(Some(AppState {
        id: id.to_string(),
        policies,
        keys,
    }))
}

pub async fn create_application(client: &ScalewayClient, name: &str) -> Result<String> {
    let app = client
        .post(
            &format!("{BASE}/applications"),
            &json!({
                "name": name,
                "organization_id": client.organization_id(),
                "description": "ciburst",
            }),
        )
        .await?;
    id_of(&app, "id", "application")
}

pub async fn create_policy(
    client: &ScalewayClient,
    name: &str,
    application_id: &str,
    permission_set: &str,
    project_id: &str,
) -> Result<String> {
    let policy = client
        .post(
            &format!("{BASE}/policies"),
            &json!({
                "name": name,
                "description": "ciburst",
                "organization_id": client.organization_id(),
                "application_id": application_id,
                "rules": [{
                    "permission_set_names": [permission_set],
                    "project_ids": [project_id],
                }],
            }),
        )
        .await?;
    id_of(&policy, "id", "policy")
}

pub async fn create_api_key(
    client: &ScalewayClient,
    application_id: &str,
    project_id: &str,
) -> Result<KeyPair> {
    let key = client
        .post(
            &format!("{BASE}/api-keys"),
            &json!({
                "application_id": application_id,
                "default_project_id": project_id,
                "description": "ciburst",
            }),
        )
        .await?;
    Ok(KeyPair {
        access_key: id_of(&key, "access_key", "api key")?,
        secret_key: id_of(&key, "secret_key", "api key")?,
    })
}

pub async fn delete_application(client: &ScalewayClient, app: &AppState) -> Result<()> {
    for policy in &app.policies {
        client.delete(&format!("{BASE}/policies/{policy}")).await?;
    }
    for key in &app.keys {
        client.delete(&format!("{BASE}/api-keys/{key}")).await?;
    }
    client
        .delete(&format!("{BASE}/applications/{}", app.id))
        .await
        .map(drop)
}

pub async fn delete_api_key(client: &ScalewayClient, access_key: &str) -> Result<()> {
    client
        .delete(&format!("{BASE}/api-keys/{access_key}"))
        .await
        .map(drop)
}

fn id_of(value: &Value, field: &str, what: &str) -> Result<String> {
    value[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| Error::provider(format!("scaleway {what}: response without {field}")))
}
