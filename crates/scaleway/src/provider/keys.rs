//! The dispatch and cache keys: one IAM application per key with a project scoped policy and
//! one API key, steps `key:dispatch` and `key:cache`.

use ciburst_core::Result;
use ciburst_core::types::{Action, KeyPair, Rotation, Step};

use super::Scaleway;
use super::iam::{self, AppState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyKind {
    Dispatch,
    Cache,
}

impl KeyKind {
    fn suffix(self) -> &'static str {
        match self {
            KeyKind::Dispatch => "dispatch",
            KeyKind::Cache => "cache",
        }
    }

    pub(super) fn step_id(self) -> &'static str {
        match self {
            KeyKind::Dispatch => "key:dispatch",
            KeyKind::Cache => "key:cache",
        }
    }

    /// Starting a job whose image is in a private registry of the project needs registry read
    /// access besides the Jobs permissions (Scaleway answers 403 on `registry_image` otherwise).
    /// The dispatcher also starts, tags and terminates VMs.
    fn permission_sets(self) -> &'static [&'static str] {
        match self {
            KeyKind::Dispatch => &[
                "ServerlessJobsFullAccess",
                "ContainerRegistryReadOnly",
                "InstancesFullAccess",
            ],
            KeyKind::Cache => &["ObjectStorageFullAccess"],
        }
    }

    fn rotation(self) -> Rotation {
        match self {
            KeyKind::Dispatch => Rotation::Key,
            KeyKind::Cache => Rotation::Cache,
        }
    }

    pub(super) fn outputs(self) -> (&'static str, &'static str) {
        match self {
            KeyKind::Dispatch => ("CIBURST_DISPATCH_ACCESS_KEY", "CIBURST_DISPATCH_SECRET_KEY"),
            KeyKind::Cache => ("CIBURST_CACHE_ACCESS_KEY", "CIBURST_CACHE_SECRET_KEY"),
        }
    }
}

impl Scaleway {
    pub(super) fn app_name(&self, kind: KeyKind) -> String {
        format!("{}-{}", self.name(), kind.suffix())
    }

    pub(super) fn key_step(
        &self,
        kind: KeyKind,
        app: &Option<AppState>,
        rotate: &[Rotation],
    ) -> Step {
        let resource = format!("api key {}", self.app_name(kind));
        match app {
            None => Step::new(kind.step_id(), Action::Create, resource),
            Some(app) if !app.complete(kind.permission_sets()) => {
                Step::new(kind.step_id(), Action::Update, resource).with_detail("repair")
            }
            Some(_) if rotate.contains(&kind.rotation()) => {
                Step::new(kind.step_id(), Action::Update, resource).with_detail("rotate")
            }
            Some(_) => Step::new(kind.step_id(), Action::Noop, resource),
        }
    }

    /// Application, policy and a fresh API key; existing keys of the application are deleted
    /// because their secrets cannot be read back.
    pub(super) async fn ensure_key(&self, kind: KeyKind, project_id: &str) -> Result<KeyPair> {
        let name = self.app_name(kind);
        let app = match iam::find_application(&self.client, &name).await? {
            Some(app) => app,
            None => {
                let id = iam::create_application(&self.client, &name).await?;
                self.log
                    .info("created iam application", &[("name", &name), ("id", &id)]);
                AppState {
                    id,
                    ..AppState::default()
                }
            }
        };
        if !app.grants(kind.permission_sets()) {
            for policy in &app.policies {
                iam::delete_policy(&self.client, policy).await?;
                self.log
                    .info("deleted iam policy", &[("name", &name), ("id", policy)]);
            }
            let id = iam::create_policy(
                &self.client,
                &name,
                &app.id,
                kind.permission_sets(),
                project_id,
            )
            .await?;
            self.log
                .info("created iam policy", &[("name", &name), ("id", &id)]);
        }
        for access_key in &app.keys {
            iam::delete_api_key(&self.client, access_key).await?;
            self.log.info(
                "deleted api key",
                &[("application", &name), ("id", &app.id)],
            );
        }
        let pair = iam::create_api_key(&self.client, &app.id, project_id).await?;
        self.log.info(
            "created api key",
            &[("application", &name), ("id", &app.id)],
        );
        Ok(pair)
    }

    pub(super) async fn delete_key(&self, kind: KeyKind) -> Result<()> {
        let name = self.app_name(kind);
        if let Some(app) = iam::find_application(&self.client, &name).await? {
            iam::delete_application(&self.client, &app).await?;
            self.log.info(
                "deleted iam application",
                &[("name", &name), ("id", &app.id)],
            );
        }
        Ok(())
    }
}
