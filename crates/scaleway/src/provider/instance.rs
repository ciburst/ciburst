//! Instance API `instance/v1` (servers, user data, actions, security groups, volumes) and the
//! Marketplace API `marketplace/v2` (local images a server boots from). Endpoints and fields
//! from scaleway-sdk-go `api/instance/v1/instance_sdk.go` and `server_utils.go`, and
//! `api/marketplace/v2/marketplace_sdk.go` and `marketplace_utils.go`.
//!
//! The Instance API is called on the HTTP seam directly rather than through
//! `ScalewayClient::call`: user data is a `text/plain` body, and start must read the error
//! `type` to fall back to the next commercial type.

use ciburst_core::http::{Request, Response};
use ciburst_core::{Error, Result};
use serde_json::{Value, json};

use super::list_all;
use crate::client::{ScalewayClient, retryable_status, transport_error, urlencode};

/// The Instance API pages with `per_page` (at most 100) and reports the total only in the
/// `X-Total-Count` header, which the HTTP seam does not carry: a short page ends a listing.
const PER_PAGE: usize = 100;

/// Marketplace image of Ubuntu with Docker (an InstantApp), checked in
/// `GET /marketplace/v2/local-images?image_label=docker&zone=fr-par-1` on 2026-10-24.
pub const DOCKER_IMAGE_LABEL: &str = "docker";
pub const FALLBACK_IMAGE_LABEL: &str = "ubuntu_noble";
/// Local images boot from a local (`l_ssd`) root volume.
const LOCAL_IMAGE_TYPE: &str = "instance_local";
const ARCH: &str = "x86_64";

/// A non-2xx answer of the Instance API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub method: String,
    pub path: String,
    pub status: u16,
    pub kind: String,
    pub message: String,
    /// What the answer names besides its message: `resource`, the `argument_name` of each of
    /// `details` and the keys of `fields` (`scw/errors.go`).
    pub names: Vec<String>,
    /// Whether repeating the call may succeed (M7 hardening rule R4).
    pub retryable: bool,
}

impl ApiError {
    fn of(method: &str, path: &str, response: &Response, idempotent: bool) -> ApiError {
        let body: Value = response.json().unwrap_or(Value::Null);
        let message = body["message"]
            .as_str()
            .filter(|m| !m.is_empty())
            .map(String::from)
            .unwrap_or_else(|| response.text());
        let details = body["details"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["argument_name"].as_str());
        let fields = body["fields"]
            .as_object()
            .into_iter()
            .flat_map(|f| f.keys().map(String::as_str));
        let names = body["resource"]
            .as_str()
            .into_iter()
            .chain(details)
            .chain(fields)
            .map(String::from)
            .collect();
        let mut error = ApiError {
            method: method.to_string(),
            path: path.to_string(),
            status: response.status,
            kind: body["type"].as_str().unwrap_or_default().to_string(),
            message,
            names,
            retryable: false,
        };
        error.retryable = !error.is_capacity() && retryable_status(error.status, idempotent);
        error
    }

    /// Whether a create was refused because its image or security group does not exist: a 404,
    /// or an invalid request that names one of them (M7 hardening rule R3).
    pub fn is_missing_image_or_group(&self) -> bool {
        if self.status == 404 {
            return true;
        }
        let invalid = matches!(
            self.kind.as_str(),
            "invalid_request_error" | "invalid_arguments"
        );
        let named = format!("{} {}", self.message, self.names.join(" "))
            .to_lowercase()
            .replace('_', " ");
        invalid && (named.contains("image") || named.contains("security group"))
    }

    /// Out of stock or over quota, so another commercial type may still start. Mirrors
    /// scaleway-sdk-go `scw/errors.go`: types `out_of_stock` and `quotas_exceeded`, and the
    /// Instance API's `invalid_request_error` whose message says the quota is exceeded.
    pub fn is_capacity(&self) -> bool {
        match self.kind.as_str() {
            "out_of_stock" | "quotas_exceeded" => true,
            "invalid_request_error" => self
                .message
                .to_lowercase()
                .contains("quota exceeded for this resource"),
            _ => false,
        }
    }
}

impl From<ApiError> for Error {
    fn from(error: ApiError) -> Error {
        let message = format!(
            "scaleway {} {}: {} {}",
            error.method, error.path, error.status, error.message
        );
        if error.retryable {
            Error::retryable(message)
        } else {
            Error::provider(message)
        }
    }
}

pub struct InstanceApi<'a> {
    client: &'a ScalewayClient,
    zone: &'a str,
}

impl<'a> InstanceApi<'a> {
    pub fn new(client: &'a ScalewayClient, zone: &'a str) -> InstanceApi<'a> {
        InstanceApi { client, zone }
    }

    fn path(&self, rest: &str) -> String {
        format!("/instance/v1/zones/{}/{rest}", self.zone)
    }

    /// One call. `idempotent` marks a call that may be repeated without a second effect (M7
    /// hardening rule R4): GETs, user data and power on.
    async fn send(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<(&str, Vec<u8>)>,
        idempotent: bool,
    ) -> Result<std::result::Result<Value, ApiError>> {
        let mut url = format!("{}{path}", self.client.endpoint());
        for (index, (key, value)) in query.iter().enumerate() {
            url.push(if index == 0 { '?' } else { '&' });
            url.push_str(key);
            url.push('=');
            url.push_str(&urlencode(value));
        }
        let mut request = Request::new(method, url)
            .header("X-Auth-Token", self.client.secret_key())
            .header("accept", "application/json");
        if let Some((content_type, bytes)) = body {
            request = request.header("content-type", content_type);
            request.body = Some(bytes);
        }
        let response = self
            .client
            .http()
            .send(request)
            .await
            .map_err(|error| transport_error(error, method, path, idempotent))?;
        if !response.ok() {
            return Ok(Err(ApiError::of(method, path, &response, idempotent)));
        }
        if response.body.is_empty() {
            return Ok(Ok(Value::Null));
        }
        Ok(Ok(response.json()?))
    }

    async fn json(
        &self,
        method: &str,
        path: &str,
        body: &Value,
        idempotent: bool,
    ) -> Result<std::result::Result<Value, ApiError>> {
        self.send(
            method,
            path,
            &[],
            Some(("application/json", body.to_string().into_bytes())),
            idempotent,
        )
        .await
    }

    async fn list(
        &self,
        rest: &str,
        mut query: Vec<(&str, String)>,
        field: &str,
    ) -> Result<Vec<Value>> {
        let path = self.path(rest);
        query.push(("per_page", PER_PAGE.to_string()));
        let mut out = Vec::new();
        for page in 1u32.. {
            let mut q = query.clone();
            q.push(("page", page.to_string()));
            let value = self.send("GET", &path, &q, None, true).await??;
            let items = value[field].as_array().cloned().unwrap_or_default();
            let count = items.len();
            out.extend(items);
            if count < PER_PAGE {
                break;
            }
        }
        Ok(out)
    }

    /// Servers of `project_id` carrying the tag `tag`, in every state.
    pub async fn list_servers(&self, project_id: &str, tag: &str) -> Result<Vec<Value>> {
        self.list(
            "servers",
            vec![
                ("project", project_id.to_string()),
                ("tags", tag.to_string()),
            ],
            "servers",
        )
        .await
    }

    /// The server, or `None` when the API does not know it.
    pub async fn get_server(&self, id: &str) -> Result<Option<Value>> {
        match self
            .send("GET", &self.path(&format!("servers/{id}")), &[], None, true)
            .await?
        {
            Ok(value) => Ok(Some(value["server"].clone())),
            Err(error) if error.status == 404 => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn create_server(
        &self,
        body: &Value,
    ) -> Result<std::result::Result<Value, ApiError>> {
        Ok(self
            .json("POST", &self.path("servers"), body, false)
            .await?
            .map(|value| value["server"].clone()))
    }

    pub async fn set_tags(&self, id: &str, tags: &[String]) -> Result<()> {
        self.json(
            "PATCH",
            &self.path(&format!("servers/{id}")),
            &json!({ "tags": tags }),
            false,
        )
        .await??;
        Ok(())
    }

    /// `PATCH servers/<id>/user_data/<key>` with a `text/plain` body (`SetServerUserData`).
    pub async fn set_user_data(&self, id: &str, key: &str, content: &str) -> Result<()> {
        self.send(
            "PATCH",
            &self.path(&format!("servers/{id}/user_data/{key}")),
            &[],
            Some(("text/plain", content.as_bytes().to_vec())),
            true,
        )
        .await??;
        Ok(())
    }

    pub async fn action(
        &self,
        id: &str,
        action: &str,
    ) -> Result<std::result::Result<(), ApiError>> {
        Ok(self
            .json(
                "POST",
                &self.path(&format!("servers/{id}/action")),
                &json!({ "action": action }),
                action == "poweron",
            )
            .await?
            .map(drop))
    }

    pub async fn delete_server(&self, id: &str) -> Result<()> {
        self.send(
            "DELETE",
            &self.path(&format!("servers/{id}")),
            &[],
            None,
            false,
        )
        .await??;
        Ok(())
    }

    /// Deletes a volume; one that is already gone counts as deleted.
    pub async fn delete_volume(&self, id: &str) -> Result<()> {
        match self
            .send(
                "DELETE",
                &self.path(&format!("volumes/{id}")),
                &[],
                None,
                false,
            )
            .await?
        {
            Ok(_) => Ok(()),
            Err(error) if error.status == 404 => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// The id of the security group named exactly `name` in `project_id`.
    pub async fn find_security_group(
        &self,
        project_id: &str,
        name: &str,
    ) -> Result<Option<String>> {
        let groups = self
            .list(
                "security_groups",
                vec![
                    ("project", project_id.to_string()),
                    ("name", name.to_string()),
                ],
                "security_groups",
            )
            .await?;
        Ok(groups
            .iter()
            .find(|g| g["name"].as_str() == Some(name))
            .and_then(|g| g["id"].as_str().map(String::from)))
    }

    /// Rule 3 of M3: inbound drop, outbound accept, stateful.
    pub async fn create_security_group(&self, project_id: &str, name: &str) -> Result<String> {
        let created = self
            .json(
                "POST",
                &self.path("security_groups"),
                &json!({
                    "name": name,
                    "description": "ciburst VMs",
                    "project": project_id,
                    "project_default": false,
                    "stateful": true,
                    "inbound_default_policy": "drop",
                    "outbound_default_policy": "accept",
                }),
                false,
            )
            .await??;
        created["security_group"]["id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| Error::provider("scaleway security group: response without id"))
    }

    pub async fn delete_security_group(&self, id: &str) -> Result<()> {
        self.send(
            "DELETE",
            &self.path(&format!("security_groups/{id}")),
            &[],
            None,
            false,
        )
        .await??;
        Ok(())
    }

    /// Local images labelled `label` in this zone that boot from a local volume on x86_64,
    /// as `(id, compatible commercial types)`.
    pub async fn local_images(&self, label: &str) -> Result<Vec<(String, Vec<String>)>> {
        let images = list_all(
            self.client,
            "/marketplace/v2/local-images",
            vec![
                ("image_label", label.to_string()),
                ("zone", self.zone.to_string()),
                ("type", LOCAL_IMAGE_TYPE.to_string()),
            ],
            "local_images",
        )
        .await?;
        Ok(images
            .iter()
            .filter(|i| i["label"].as_str() == Some(label))
            .filter(|i| i["type"].as_str() == Some(LOCAL_IMAGE_TYPE))
            .filter(|i| i["arch"].as_str() == Some(ARCH))
            .filter_map(|i| {
                let id = i["id"].as_str()?.to_string();
                let types = i["compatible_commercial_types"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect();
                Some((id, types))
            })
            .collect())
    }
}

/// The first image compatible with `commercial_type` (`LocalImage.IsCompatible`, upper case).
pub fn compatible(images: &[(String, Vec<String>)], commercial_type: &str) -> Option<String> {
    let wanted = commercial_type.to_uppercase();
    images
        .iter()
        .find(|(_, types)| types.contains(&wanted))
        .map(|(id, _)| id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(kind: &str, message: &str) -> ApiError {
        ApiError {
            method: "POST".into(),
            path: "/instance/v1/zones/fr-par-1/servers".into(),
            status: 400,
            kind: kind.into(),
            message: message.into(),
            names: Vec::new(),
            retryable: false,
        }
    }

    fn answer(status: u16, body: Value, idempotent: bool) -> ApiError {
        let response = Response::json_body(status, &body);
        ApiError::of(
            "POST",
            "/instance/v1/zones/fr-par-1/servers",
            &response,
            idempotent,
        )
    }

    #[test]
    fn classifies_failures_for_one_retry() {
        let busy = json!({ "type": "unknown", "message": "busy" });
        for (status, any_call, idempotent_call) in [
            (429, true, true),
            (503, true, true),
            (500, false, true),
            (502, false, true),
            (400, false, false),
            (404, false, false),
        ] {
            let once = answer(status, busy.clone(), false);
            assert_eq!(once.retryable, any_call, "{status}");
            let again = answer(status, busy.clone(), true);
            assert_eq!(again.retryable, idempotent_call, "{status}");
        }
        let stock = json!({ "type": "out_of_stock", "message": "out of stock" });
        assert!(
            !answer(503, stock, true).retryable,
            "out of stock moves on to the next type instead"
        );
        assert!(Error::from(answer(503, busy.clone(), false)).is_retryable());
        assert!(!Error::from(answer(500, busy, false)).is_retryable());
    }

    #[test]
    fn recognizes_a_missing_image_or_security_group() {
        let refused =
            |status: u16, body: Value| answer(status, body, false).is_missing_image_or_group();
        let unknown = json!({ "type": "unknown_resource", "message": "server not found" });
        assert!(refused(404, unknown));
        let image = json!({ "type": "invalid_arguments", "message": "image not found" });
        assert!(refused(400, image));
        let detailed = json!({
            "type": "invalid_arguments",
            "message": "invalid argument(s)",
            "details": [{ "argument_name": "security_group", "reason": "constraint" }],
        });
        assert!(refused(400, detailed));
        let fields = json!({
            "type": "invalid_request_error",
            "message": "Validation Error",
            "fields": { "image": ["unknown"] },
        });
        assert!(refused(400, fields));
        let name = json!({ "type": "invalid_arguments", "message": "name required" });
        assert!(!refused(400, name));
        let stock = json!({ "type": "out_of_stock", "message": "image out of stock" });
        assert!(!refused(503, stock));
        let quota = json!({
            "type": "invalid_request_error",
            "message": "Quota exceeded for this resource.",
        });
        assert!(!refused(403, quota));
    }

    #[test]
    fn classifies_capacity_errors_like_the_sdk() {
        assert!(error("out_of_stock", "").is_capacity());
        assert!(error("quotas_exceeded", "quota(s) exceeded").is_capacity());
        assert!(error("invalid_request_error", "Quota exceeded for this resource.").is_capacity());
        assert!(!error("invalid_request_error", "image not compatible").is_capacity());
        assert!(!error("invalid_arguments", "out of stock").is_capacity());
        assert_eq!(
            Error::from(error("out_of_stock", "x")).to_string(),
            "scaleway POST /instance/v1/zones/fr-par-1/servers: 400 x"
        );
    }

    #[test]
    fn picks_the_first_compatible_image() {
        let images = vec![
            ("a".to_string(), vec!["DEV1-S".to_string()]),
            (
                "b".to_string(),
                vec!["DEV1-M".to_string(), "DEV1-L".to_string()],
            ),
        ];
        assert_eq!(compatible(&images, "dev1-m").as_deref(), Some("b"));
        assert_eq!(compatible(&images, "PLAY2-NANO"), None);
    }
}
