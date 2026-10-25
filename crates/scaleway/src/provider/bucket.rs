//! The cache bucket `<name>-cache-<project id>` and its lifecycle, step `bucket`.

use ciburst_core::Result;
use ciburst_core::types::Action;

use super::Scaleway;

impl Scaleway {
    pub(super) async fn apply_bucket(
        &self,
        action: Action,
        project: &Option<String>,
    ) -> Result<()> {
        if action == Action::Delete {
            if let Some(project_id) = project {
                self.delete_bucket(project_id).await?;
            }
            return Ok(());
        }
        let project_id = self.require_project(project)?;
        let s3 = self.s3(project_id)?;
        let name = self.bucket(project_id);
        if !s3.bucket_exists(&name).await? {
            s3.create_bucket(&name).await?;
            self.log.info("created bucket", &[("name", &name)]);
        }
        let days = self.config.cache.expire_days;
        if s3.lifecycle_days(&name).await? != Some(days) {
            s3.put_lifecycle(&name, days).await?;
            self.log.info(
                "updated bucket lifecycle",
                &[("name", &name), ("expire_days", &days.to_string())],
            );
        }
        Ok(())
    }

    pub(super) async fn delete_bucket(&self, project_id: &str) -> Result<()> {
        let s3 = self.s3(project_id)?;
        let name = self.bucket(project_id);
        if s3.bucket_exists(&name).await? {
            s3.empty_and_delete(&name).await?;
            self.log.info("deleted bucket", &[("name", &name)]);
        }
        Ok(())
    }
}
