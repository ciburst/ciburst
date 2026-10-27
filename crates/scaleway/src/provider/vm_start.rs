//! What a VM start needs besides the request (spec 6.6): the id of the security
//! group `<name>-vm` and per commercial type the image and whether cloud-init installs Docker.
//! `apply` resolves them with the start's own rules and hands them to the function as the plain
//! variables [`SECURITY_GROUP_VAR`] and [`IMAGES_VAR`]; a start without them, or whose create
//! Scaleway refuses because they are gone, makes today's lookups ([`Lookups`]).

use std::collections::BTreeMap;

use ciburst_core::Result;
use serde::{Deserialize, Serialize};

use super::instance::{self, DOCKER_IMAGE_LABEL, FALLBACK_IMAGE_LABEL, InstanceApi};
use super::{Runtime, Scaleway, instance_types};

/// The id of the VMs' security group.
pub const SECURITY_GROUP_VAR: &str = "CIBURST_VM_SECURITY_GROUP";
/// JSON object: commercial type to [`VmImage`].
pub const IMAGES_VAR: &str = "CIBURST_VM_IMAGES";

/// The image a commercial type boots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VmImage {
    pub image: String,
    /// The image is plain Ubuntu, so cloud-init installs Docker first.
    pub install_docker: bool,
}

/// The resolved values a start uses; `None` where the function's variable is unset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VmStart {
    pub security_group: Option<String>,
    pub images: Option<BTreeMap<String, VmImage>>,
}

impl VmStart {
    /// From the function's variables; empty values count as unset. Fails on an
    /// [`IMAGES_VAR`] that is no such JSON object.
    pub fn from_vars(
        security_group: Option<&str>,
        images: Option<&str>,
    ) -> serde_json::Result<VmStart> {
        let images = match images.filter(|v| !v.is_empty()) {
            Some(json) => Some(serde_json::from_str(json)?),
            None => None,
        };
        Ok(VmStart {
            security_group: security_group.filter(|v| !v.is_empty()).map(String::from),
            images,
        })
    }

    /// The function's plain variables for these values; a value that is unset has none.
    pub fn vars(&self) -> BTreeMap<String, String> {
        let mut vars = BTreeMap::new();
        if let Some(id) = &self.security_group {
            vars.insert(SECURITY_GROUP_VAR.to_string(), id.clone());
        }
        if let Some(images) = &self.images {
            let json = serde_json::to_string(images).expect("a map of strings serializes");
            vars.insert(IMAGES_VAR.to_string(), json);
        }
        vars
    }
}

/// Today's lookups of a VM start, each made at most once: the security group by name, the
/// local Docker images, and only when a type has none of them the local Ubuntu images.
#[derive(Default)]
pub(super) struct Lookups {
    security_group: Option<Option<String>>,
    docker: Option<Vec<(String, Vec<String>)>>,
    fallback: Option<Vec<(String, Vec<String>)>>,
}

impl Lookups {
    pub(super) async fn security_group(
        &mut self,
        api: &InstanceApi<'_>,
        project_id: &str,
        name: &str,
    ) -> Result<Option<String>> {
        if self.security_group.is_none() {
            self.security_group = Some(api.find_security_group(project_id, name).await?);
        }
        Ok(self.security_group.clone().flatten())
    }

    /// spec 6.1: the first local Docker image compatible with `commercial_type`, else the
    /// first local Ubuntu image, on which cloud-init installs Docker; `None` without either.
    pub(super) async fn image(
        &mut self,
        api: &InstanceApi<'_>,
        commercial_type: &str,
    ) -> Result<Option<VmImage>> {
        if self.docker.is_none() {
            self.docker = Some(api.local_images(DOCKER_IMAGE_LABEL).await?);
        }
        let docker = self.docker.as_deref().unwrap_or_default();
        if let Some(image) = instance::compatible(docker, commercial_type) {
            return Ok(Some(VmImage {
                image,
                install_docker: false,
            }));
        }
        if self.fallback.is_none() {
            self.fallback = Some(api.local_images(FALLBACK_IMAGE_LABEL).await?);
        }
        let fallback = self.fallback.as_deref().unwrap_or_default();
        Ok(
            instance::compatible(fallback, commercial_type).map(|image| VmImage {
                image,
                install_docker: true,
            }),
        )
    }
}

impl Scaleway {
    /// The values a start of an own VM pool would look up, resolved now for the function's
    /// environment; `None` without own VM pools. The security group stays unset while it does
    /// not exist, a type without a local image stays out of the images.
    pub async fn resolve_vm_start(&self, project_id: &str) -> Result<Option<VmStart>> {
        let pools = self.pools_on(Runtime::Instances)?;
        if pools.is_empty() {
            return Ok(None);
        }
        let api = self.instances();
        let mut lookups = Lookups::default();
        let security_group = lookups
            .security_group(&api, project_id, &self.security_group_name())
            .await?;
        let mut images = BTreeMap::new();
        for pool in pools {
            for commercial_type in instance_types(&pool) {
                if images.contains_key(&commercial_type) {
                    continue;
                }
                if let Some(image) = lookups.image(&api, &commercial_type).await? {
                    images.insert(commercial_type, image);
                }
            }
        }
        Ok(Some(VmStart {
            security_group,
            images: Some(images),
        }))
    }

    /// The values `start` uses instead of looking them up (spec 6.6).
    pub fn with_vm_start(mut self, vm_start: VmStart) -> Scaleway {
        self.vm_start = vm_start;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_round_trip() {
        let start = VmStart {
            security_group: Some("sg-1".into()),
            images: Some(BTreeMap::from([
                (
                    "DEV1-M".to_string(),
                    VmImage {
                        image: "img-1".into(),
                        install_docker: false,
                    },
                ),
                (
                    "DEV1-L".to_string(),
                    VmImage {
                        image: "img-2".into(),
                        install_docker: true,
                    },
                ),
            ])),
        };
        let vars = start.vars();
        assert_eq!(vars[SECURITY_GROUP_VAR], "sg-1");
        assert_eq!(
            vars[IMAGES_VAR],
            r#"{"DEV1-L":{"image":"img-2","installDocker":true},"DEV1-M":{"image":"img-1","installDocker":false}}"#
        );
        let read = VmStart::from_vars(
            vars.get(SECURITY_GROUP_VAR).map(String::as_str),
            vars.get(IMAGES_VAR).map(String::as_str),
        )
        .unwrap();
        assert_eq!(read, start);
        assert_eq!(
            VmStart::from_vars(Some(""), None).unwrap(),
            VmStart::default()
        );
        assert!(VmStart::default().vars().is_empty());
        for broken in [
            "[]",
            r#"{"DEV1-M":"img-1"}"#,
            r#"{"DEV1-M":{"image":"img-1"}}"#,
            r#"{"DEV1-M":{"image":"img-1","installDocker":false,"x":1}}"#,
        ] {
            assert!(VmStart::from_vars(None, Some(broken)).is_err(), "{broken}");
        }
    }
}
