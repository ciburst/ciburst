//! Container Registry `registry/v1`: one namespace per deployment, so Serverless Jobs pull the
//! pool images without credentials while the framework project is private.

use ciburst_core::{Error, Result};
use serde_json::{Value, json};

use super::{Scaleway, list_all};
use crate::client::ScalewayClient;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceState {
    pub id: String,
    pub endpoint: String,
    pub is_public: bool,
}

fn base(client: &ScalewayClient) -> String {
    format!("/registry/v1/regions/{}/namespaces", client.region())
}

fn state(value: &Value) -> Option<NamespaceState> {
    Some(NamespaceState {
        id: value["id"].as_str()?.to_string(),
        endpoint: value["endpoint"].as_str().unwrap_or_default().to_string(),
        is_public: value["is_public"].as_bool().unwrap_or(false),
    })
}

pub async fn find_namespace(
    client: &ScalewayClient,
    project_id: &str,
    name: &str,
) -> Result<Option<NamespaceState>> {
    let namespaces = list_all(
        client,
        &base(client),
        vec![
            ("project_id", project_id.to_string()),
            ("name", name.to_string()),
        ],
        "namespaces",
    )
    .await?;
    Ok(namespaces
        .iter()
        .find(|n| n["name"].as_str() == Some(name))
        .and_then(state))
}

pub async fn create_namespace(
    client: &ScalewayClient,
    project_id: &str,
    name: &str,
    is_public: bool,
) -> Result<NamespaceState> {
    let value = client
        .post(
            &base(client),
            &json!({
                "name": name,
                "project_id": project_id,
                "is_public": is_public,
                "description": "ciburst pool images",
            }),
        )
        .await?;
    state(&value).ok_or_else(|| Error::provider("scaleway registry: response without id"))
}

pub async fn update_namespace(client: &ScalewayClient, id: &str, is_public: bool) -> Result<()> {
    client
        .patch(
            &format!("{}/{id}", base(client)),
            &json!({ "is_public": is_public }),
        )
        .await?;
    Ok(())
}

pub async fn delete_namespace(client: &ScalewayClient, id: &str) -> Result<()> {
    client.delete(&format!("{}/{id}", base(client))).await?;
    Ok(())
}

impl Scaleway {
    pub(super) async fn ensure_registry(&self, project_id: &str) -> Result<NamespaceState> {
        let name = self.name();
        let public = self.registry_public();
        let mut namespace = match find_namespace(&self.client, project_id, name).await? {
            Some(namespace) => namespace,
            None => {
                let namespace = create_namespace(&self.client, project_id, name, public).await?;
                self.log.info(
                    "created registry namespace",
                    &[
                        ("name", name),
                        ("id", &namespace.id),
                        ("endpoint", &namespace.endpoint),
                    ],
                );
                namespace
            }
        };
        if namespace.is_public != public {
            update_namespace(&self.client, &namespace.id, public).await?;
            namespace.is_public = public;
            self.log.info(
                "updated registry namespace",
                &[("name", name), ("public", &public.to_string())],
            );
        }
        Ok(namespace)
    }

    pub(super) async fn delete_registry(&self, project: &Option<String>) -> Result<()> {
        let Some(project_id) = project else {
            return Ok(());
        };
        if let Some(namespace) = find_namespace(&self.client, project_id, self.name()).await? {
            delete_namespace(&self.client, &namespace.id).await?;
            self.log.info(
                "deleted registry namespace",
                &[("name", self.name()), ("id", &namespace.id)],
            );
        }
        Ok(())
    }
}
