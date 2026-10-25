//! Fake of the Instance API (`/instance/v1/zones/<zone>`) and of the Marketplace local images
//! (`/marketplace/v2/local-images`). Shapes from scaleway-sdk-go `api/instance/v1`
//! (`CreateServerRequest`, `Server`, `VolumeServer`, `ServerActionRequest` and its `Task`,
//! `UpdateServerRequest.tags`, `CreateSecurityGroupRequest`, `SecurityGroup`; user data from
//! `server_utils.go`, a `text/plain` body), the errors of `scw/errors.go`, and
//! `api/marketplace/v2` (`LocalImage`). Instance lists page with `per_page` and carry their total
//! only in the `X-Total-Count` header, so their bodies have no `total_count`.
//!
//! Lifecycle: a created server is `stopped`; `poweron` makes it `starting`, and a read after
//! that shows it `running`; `terminate` removes it with its local volumes; deleting a stopped
//! server leaves its volumes. With [`FakeInstance::set_slow_terminate`] a terminated server stays
//! `stopping`, holding its security group, until a read has shown it so, as Scaleway keeps it
//! for a while. The marketplace answers without a token, like the real one.
//!
//! [`FakeInstance::fail_next`] fails the next call of a method on a path once (M7 hardening
//! rule R4).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::http::{HttpClient, Request, Response};
use serde_json::{Value, json};

use super::scaleway_fake::{
    Call, Failure, Failures, Url, answer, api_error, authorized, body_json, call_of,
    failure_status, fake_uuid, reply, unauthorized,
};
use crate::provider::vm::rfc3339;

/// 2026-10-24T00:00:00Z, the fake's clock until a test sets it.
pub const FAKE_CLOCK: u64 = 1_792_800_000;
pub const FAKE_ZONE: &str = "fr-par-1";

/// Commercial types the fake knows, and whether they take local volumes (PLAY2 and PRO2 have
/// no local storage, `per_volume_constraint.l_ssd.max_size` 0 in `products/servers`).
const TYPES: [(&str, bool); 5] = [
    ("DEV1-S", true),
    ("DEV1-M", true),
    ("DEV1-L", true),
    ("PLAY2-NANO", false),
    ("PRO2-XXS", false),
];

struct Inner {
    servers: Vec<Value>,
    volumes: Vec<Value>,
    security_groups: Vec<Value>,
    user_data: BTreeMap<(String, String), String>,
    local_images: Vec<Value>,
    out_of_stock: BTreeSet<String>,
    over_quota: BTreeSet<String>,
    poweron_out_of_stock: BTreeSet<String>,
    slow_terminate: bool,
    /// Servers terminated in slow mode; `true` once a read has shown them `stopping`.
    terminating: BTreeMap<String, bool>,
    clock: u64,
    next: u64,
    calls: Vec<Call>,
    failures: Failures,
}

impl Inner {
    fn next(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn server_index(&self, id: &str) -> Option<usize> {
        self.servers.iter().position(|s| s["id"] == id)
    }

    /// A read shows the current state, and a starting server has booted by the next one.
    fn read(&mut self, index: usize) -> Value {
        let server = self.servers[index].clone();
        if server["state"] == "starting" {
            self.servers[index]["state"] = json!("running");
        }
        if let Some(id) = server["id"].as_str()
            && let Some(seen) = self.terminating.get_mut(id)
        {
            *seen = true;
        }
        server
    }

    /// Removes the server at `index` with its local volumes and user data.
    fn remove(&mut self, index: usize) {
        let removed = self.servers.remove(index);
        let id = removed["id"].as_str().unwrap_or_default().to_string();
        let local: Vec<Value> = removed["volumes"]
            .as_object()
            .into_iter()
            .flat_map(|v| v.values())
            .filter(|v| v["volume_type"] == "l_ssd")
            .map(|v| v["id"].clone())
            .collect();
        self.volumes.retain(|v| !local.contains(&v["id"]));
        self.user_data.retain(|(server, _), _| *server != id);
        self.terminating.remove(&id);
    }

    /// Slowly terminated servers that a read has shown are gone.
    fn purge(&mut self) {
        let shown: Vec<String> = self
            .terminating
            .iter()
            .filter(|(_, seen)| **seen)
            .map(|(id, _)| id.clone())
            .collect();
        for id in shown {
            if let Some(index) = self.server_index(&id) {
                self.remove(index);
            }
        }
    }
}

pub struct FakeInstance {
    inner: Mutex<Inner>,
}

impl Default for FakeInstance {
    fn default() -> Self {
        FakeInstance::new()
    }
}

fn local_image(n: u64, label: &str, kind: &str, arch: &str, types: &[&str]) -> Value {
    json!({
        "id": fake_uuid(9, n),
        "arch": arch,
        "zone": FAKE_ZONE,
        "compatible_commercial_types": types,
        "label": label,
        "type": kind,
    })
}

/// Local images as `GET /marketplace/v2/local-images` lists them for fr-par-1 (2026-10-24),
/// trimmed to the fake's commercial types: local images exclude PLAY2 and PRO2.
fn default_local_images() -> Vec<Value> {
    let local = ["DEV1-S", "DEV1-M", "DEV1-L"];
    let sbs = ["DEV1-S", "DEV1-M", "DEV1-L", "PLAY2-NANO", "PRO2-XXS"];
    vec![
        local_image(1, "docker", "instance_local", "x86_64", &local),
        local_image(2, "docker", "instance_sbs", "x86_64", &sbs),
        local_image(3, "ubuntu_noble", "instance_local", "x86_64", &local),
        local_image(4, "ubuntu_noble", "instance_sbs", "x86_64", &sbs),
        local_image(
            5,
            "ubuntu_noble",
            "instance_sbs",
            "arm64",
            &["COPARM1-2C-8G"],
        ),
    ]
}

impl FakeInstance {
    pub fn new() -> FakeInstance {
        FakeInstance {
            inner: Mutex::new(Inner {
                servers: Vec::new(),
                volumes: Vec::new(),
                security_groups: Vec::new(),
                user_data: BTreeMap::new(),
                local_images: default_local_images(),
                out_of_stock: BTreeSet::new(),
                over_quota: BTreeSet::new(),
                poweron_out_of_stock: BTreeSet::new(),
                slow_terminate: false,
                terminating: BTreeMap::new(),
                clock: FAKE_CLOCK,
                next: 0,
                calls: Vec::new(),
                failures: Failures::default(),
            }),
        }
    }

    /// The next `method` call on `path` (a `*` segment stands for any one) fails once with
    /// `failure`.
    pub fn fail_next(&self, method: &str, path: &str, failure: Failure) {
        self.lock().failures.arm(method, path, failure);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("fake instance")
    }

    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// Servers as stored, without advancing their state.
    pub fn servers(&self) -> Vec<Value> {
        self.lock().servers.clone()
    }

    pub fn server(&self, id: &str) -> Option<Value> {
        self.lock().servers.iter().find(|s| s["id"] == id).cloned()
    }

    pub fn volumes(&self) -> Vec<Value> {
        self.lock().volumes.clone()
    }

    pub fn security_groups(&self) -> Vec<Value> {
        self.lock().security_groups.clone()
    }

    pub fn user_data(&self, server_id: &str, key: &str) -> Option<String> {
        self.lock()
            .user_data
            .get(&(server_id.to_string(), key.to_string()))
            .cloned()
    }

    /// Unix seconds the next server's `creation_date` carries.
    pub fn set_clock(&self, unix: u64) {
        self.lock().clock = unix;
    }

    /// Creating a server of `commercial_type` fails with `out_of_stock`.
    pub fn set_out_of_stock(&self, commercial_type: &str) {
        self.lock().out_of_stock.insert(commercial_type.to_string());
    }

    /// Creating a server of `commercial_type` exceeds the Project's quota.
    pub fn set_over_quota(&self, commercial_type: &str) {
        self.lock().over_quota.insert(commercial_type.to_string());
    }

    /// Powering on a server of `commercial_type` fails with `out_of_stock`.
    /// Terminated servers stay `stopping` until a read has shown them.
    pub fn set_slow_terminate(&self, slow: bool) {
        self.lock().slow_terminate = slow;
    }

    pub fn set_poweron_out_of_stock(&self, commercial_type: &str) {
        self.lock()
            .poweron_out_of_stock
            .insert(commercial_type.to_string());
    }

    /// Drops the marketplace's local images labelled `label`.
    pub fn remove_local_images(&self, label: &str) {
        self.lock().local_images.retain(|i| i["label"] != label);
    }

    pub fn set_tags(&self, server_id: &str, tags: &[&str]) {
        let mut inner = self.lock();
        if let Some(index) = inner.server_index(server_id) {
            inner.servers[index]["tags"] = json!(tags);
        }
    }

    pub fn set_state(&self, server_id: &str, state: &str) {
        let mut inner = self.lock();
        if let Some(index) = inner.server_index(server_id) {
            inner.servers[index]["state"] = json!(state);
        }
    }

    /// A server another deployment or a person created, straight in `running`.
    pub fn add_server(&self, name: &str, project: &str, tags: &[&str]) -> String {
        let mut inner = self.lock();
        let n = inner.next();
        let id = fake_uuid(10, n);
        let now = rfc3339(inner.clock);
        inner.servers.push(json!({
            "id": id,
            "name": name,
            "project": project,
            "tags": tags,
            "commercial_type": "DEV1-S",
            "creation_date": now,
            "modification_date": now,
            "state": "running",
            "volumes": {},
            "security_group": null,
            "zone": FAKE_ZONE,
        }));
        id
    }
}

fn task(id: u64, action: &str, server_id: &str, now: &str) -> Value {
    json!({
        "id": fake_uuid(13, id),
        "description": format!("server_{action}"),
        "progress": 0,
        "started_at": now,
        "terminated_at": null,
        "status": "pending",
        "href_from": format!("/servers/{server_id}/action"),
        "href_result": format!("/servers/{server_id}"),
        "zone": FAKE_ZONE,
    })
}

fn invalid(message: &str) -> Response {
    api_error(400, "invalid_arguments", message)
}

/// `out_of_stock` with its `resource` (`OutOfStockError`); the status code is not documented.
fn out_of_stock(commercial_type: &str) -> Response {
    reply(
        503,
        json!({ "type": "out_of_stock", "resource": commercial_type, "message": "out of stock" }),
    )
}

/// The Instance API's quota answer, as in scaleway-sdk-go `scw/errors_test.go`.
fn over_quota(commercial_type: &str) -> Response {
    reply(
        403,
        json!({
            "type": "invalid_request_error",
            "message": "Quota exceeded for this resource.",
            "resource": format!("compute_instances_type_{}", commercial_type.replace('-', "_")),
        }),
    )
}

fn page(items: Vec<Value>, url: &Url) -> Vec<Value> {
    let per_page = url
        .get("per_page")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(50)
        .clamp(1, 100);
    let page = url
        .get("page")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    items
        .into_iter()
        .skip((page - 1) * per_page)
        .take(per_page)
        .collect()
}

fn string_list(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Null => Some(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().map(String::from))
            .collect(),
        _ => None,
    }
}

impl FakeInstance {
    fn marketplace(&self, inner: &Inner, url: &Url) -> Response {
        let items: Vec<Value> = inner
            .local_images
            .iter()
            .filter(|i| url.get("image_label").is_none_or(|l| i["label"] == l))
            .filter(|i| url.get("zone").is_none_or(|z| i["zone"] == z))
            .filter(|i| url.get("type").is_none_or(|t| i["type"] == t))
            .filter(|i| url.get("arch").is_none_or(|a| i["arch"] == a))
            .cloned()
            .collect();
        let total = items.len();
        let size = url
            .get("page_size")
            .and_then(|p| p.parse::<usize>().ok())
            .unwrap_or(20)
            .clamp(1, 100);
        let page_number = url
            .get("page")
            .and_then(|p| p.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        let items: Vec<Value> = items
            .into_iter()
            .skip((page_number - 1) * size)
            .take(size)
            .collect();
        reply(200, json!({ "local_images": items, "total_count": total }))
    }

    fn create_server(inner: &mut Inner, zone: &str, body: &Value) -> Response {
        let Some(name) = body["name"].as_str().filter(|n| !n.is_empty()) else {
            return invalid("name required");
        };
        let Some(commercial_type) = body["commercial_type"].as_str() else {
            return invalid("commercial_type required");
        };
        let Some((_, local_storage)) = TYPES.iter().find(|(t, _)| *t == commercial_type) else {
            return invalid("unknown commercial_type");
        };
        let Some(project) = body["project"].as_str().filter(|p| !p.is_empty()) else {
            return invalid("project required");
        };
        if !body["organization"].is_null() {
            return invalid("precisely one of project, organization must be set");
        }
        let Some(tags) = string_list(&body["tags"]) else {
            return invalid("tags must be strings");
        };
        for field in ["dynamic_ip_required", "routed_ip_enabled"] {
            if !body[field].is_null() && !body[field].is_boolean() {
                return invalid(&format!("{field} must be a boolean"));
            }
        }
        if !body["volumes"].is_null() {
            return invalid("the fake creates the root volume from the image only");
        }
        let Some(image) = inner
            .local_images
            .iter()
            .find(|i| body["image"].as_str().is_some_and(|id| i["id"] == id))
            .cloned()
        else {
            return invalid("image not found");
        };
        let compatible = image["compatible_commercial_types"]
            .as_array()
            .is_some_and(|types| types.iter().any(|t| t == commercial_type));
        let local = image["type"] == "instance_local";
        if !compatible || (local && !local_storage) {
            return invalid("image not compatible with this commercial type");
        }
        let security_group = match body["security_group"].as_str() {
            None => Value::Null,
            Some(id) => match inner.security_groups.iter().find(|g| g["id"] == id) {
                Some(group) => json!({ "id": group["id"], "name": group["name"] }),
                None => return api_error(404, "unknown_resource", "security group not found"),
            },
        };
        if inner.out_of_stock.contains(commercial_type) {
            return out_of_stock(commercial_type);
        }
        if inner.over_quota.contains(commercial_type) {
            return over_quota(commercial_type);
        }
        let n = inner.next();
        let id = fake_uuid(11, n);
        let now = rfc3339(inner.clock);
        let volume_type = if local { "l_ssd" } else { "sbs_volume" };
        let volume = json!({
            "id": fake_uuid(12, n),
            "name": format!("{name}-root"),
            "export_uri": null,
            "organization": "",
            "server": { "id": id, "name": name },
            "size": 10_000_000_000u64,
            "volume_type": volume_type,
            "creation_date": now,
            "modification_date": now,
            "state": "available",
            "project": project,
            "boot": false,
            "zone": zone,
        });
        inner.volumes.push(volume.clone());
        let server = json!({
            "id": id,
            "name": name,
            "organization": "",
            "project": project,
            "tags": tags,
            "commercial_type": commercial_type,
            "creation_date": now,
            "dynamic_ip_required": body["dynamic_ip_required"].as_bool().unwrap_or(true),
            "routed_ip_enabled": body["routed_ip_enabled"].as_bool().unwrap_or(true),
            "hostname": name,
            "image": { "id": image["id"], "name": image["label"], "arch": image["arch"] },
            "protected": false,
            "public_ips": [],
            "modification_date": now,
            "state": "stopped",
            "boot_type": "local",
            "volumes": { "0": volume },
            "security_group": security_group,
            "state_detail": "",
            "arch": image["arch"],
            "zone": zone,
        });
        inner.servers.push(server.clone());
        reply(201, json!({ "server": server }))
    }

    fn action(inner: &mut Inner, index: usize, body: &Value) -> Response {
        let id = inner.servers[index]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let state = inner.servers[index]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let commercial_type = inner.servers[index]["commercial_type"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let action = body["action"].as_str().unwrap_or("poweron").to_string();
        let n = inner.next();
        let now = rfc3339(inner.clock);
        match action.as_str() {
            "poweron" => {
                if state != "stopped" {
                    return api_error(400, "invalid_request_error", "server is not stopped");
                }
                if inner.poweron_out_of_stock.contains(&commercial_type) {
                    return out_of_stock(&commercial_type);
                }
                inner.servers[index]["state"] = json!("starting");
            }
            "terminate" => {
                if state == "stopped" {
                    return api_error(400, "invalid_request_error", "server is stopped");
                }
                if inner.slow_terminate {
                    if inner.terminating.contains_key(&id) {
                        return api_error(400, "invalid_request_error", "server is stopping");
                    }
                    inner.servers[index]["state"] = json!("stopping");
                    inner.terminating.insert(id.clone(), false);
                } else {
                    inner.remove(index);
                }
            }
            _ => return invalid("action not supported by the fake"),
        }
        reply(202, json!({ "task": task(n, &action, &id, &now) }))
    }
}

#[async_trait]
impl HttpClient for FakeInstance {
    async fn send(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        let failure = {
            let mut inner = self.lock();
            inner.calls.push(call_of(&request, &url));
            inner.failures.take(&request.method, &url.path)
        };
        let target = request.url.clone();
        answer(failure, &target, failure_status, self.respond(request)).await
    }
}

impl FakeInstance {
    async fn respond(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        let mut inner = self.lock();
        inner.purge();
        let segments = url.segments();
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        if request.method == "GET" && segments == ["marketplace", "v2", "local-images"] {
            return Ok(self.marketplace(&inner, &url));
        }
        if !authorized(&request) {
            return Ok(unauthorized());
        }
        let ["instance", "v1", "zones", zone, rest @ ..] = segments.as_slice() else {
            return Ok(api_error(404, "not_found", "no route"));
        };
        let zone = zone.to_string();
        let body = body_json(&request);
        match (request.method.as_str(), rest) {
            ("GET", ["servers"]) => {
                let wanted: Vec<&str> = url
                    .get("tags")
                    .map(|t| t.split(',').filter(|t| !t.is_empty()).collect())
                    .unwrap_or_default();
                let mut indices: Vec<usize> = (0..inner.servers.len())
                    .filter(|&i| {
                        let server = &inner.servers[i];
                        server["zone"] == zone.as_str()
                            && url.get("project").is_none_or(|p| server["project"] == p)
                            && wanted.iter().all(|w| {
                                server["tags"]
                                    .as_array()
                                    .is_some_and(|tags| tags.iter().any(|t| t == *w))
                            })
                    })
                    .collect();
                indices.reverse();
                let items: Vec<Value> = indices.into_iter().map(|i| inner.read(i)).collect();
                Ok(reply(200, json!({ "servers": page(items, &url) })))
            }
            ("POST", ["servers"]) => Ok(Self::create_server(&mut inner, &zone, &body)),
            ("GET" | "PATCH" | "DELETE", ["servers", id]) => {
                let Some(index) = inner.server_index(id) else {
                    return Ok(api_error(404, "unknown_resource", "server not found"));
                };
                match request.method.as_str() {
                    "GET" => {
                        let server = inner.read(index);
                        Ok(reply(200, json!({ "server": server })))
                    }
                    "PATCH" => {
                        if !body["tags"].is_null() {
                            let Some(tags) = string_list(&body["tags"]) else {
                                return Ok(invalid("tags must be strings"));
                            };
                            inner.servers[index]["tags"] = json!(tags);
                        }
                        inner.servers[index]["modification_date"] = json!(rfc3339(inner.clock));
                        Ok(reply(200, json!({ "server": inner.servers[index] })))
                    }
                    _ => {
                        if inner.servers[index]["state"] != "stopped" {
                            return Ok(api_error(
                                400,
                                "invalid_request_error",
                                "server should be stopped",
                            ));
                        }
                        let removed = inner.servers.remove(index);
                        for volume in inner.volumes.iter_mut() {
                            if volume["server"]["id"] == removed["id"] {
                                volume["server"] = Value::Null;
                            }
                        }
                        inner.user_data.retain(|(server, _), _| server != id);
                        Ok(Response::empty(204))
                    }
                }
            }
            ("PATCH", ["servers", id, "user_data", key]) => {
                if inner.server_index(id).is_none() {
                    return Ok(api_error(404, "unknown_resource", "server not found"));
                }
                let plain = request
                    .header_value("content-type")
                    .is_some_and(|c| c.starts_with("text/plain"));
                if !plain {
                    return Ok(invalid("user data is a text/plain body"));
                }
                let content = String::from_utf8_lossy(request.body.as_deref().unwrap_or_default())
                    .into_owned();
                inner
                    .user_data
                    .insert((id.to_string(), key.to_string()), content);
                Ok(Response::empty(204))
            }
            ("POST", ["servers", id, "action"]) => {
                let Some(index) = inner.server_index(id) else {
                    return Ok(api_error(404, "unknown_resource", "server not found"));
                };
                Ok(Self::action(&mut inner, index, &body))
            }
            ("DELETE", ["volumes", id]) => {
                let Some(index) = inner.volumes.iter().position(|v| v["id"] == *id) else {
                    return Ok(api_error(404, "unknown_resource", "volume not found"));
                };
                if !inner.volumes[index]["server"].is_null() {
                    return Ok(api_error(
                        400,
                        "invalid_request_error",
                        "volume is attached",
                    ));
                }
                inner.volumes.remove(index);
                Ok(Response::empty(204))
            }
            ("GET", ["security_groups"]) => {
                let items: Vec<Value> = inner
                    .security_groups
                    .iter()
                    .filter(|g| g["zone"] == zone.as_str())
                    .filter(|g| url.get("project").is_none_or(|p| g["project"] == p))
                    .filter(|g| {
                        url.get("name")
                            .is_none_or(|n| g["name"].as_str().is_some_and(|name| name.contains(n)))
                    })
                    .cloned()
                    .collect();
                Ok(reply(200, json!({ "security_groups": page(items, &url) })))
            }
            ("POST", ["security_groups"]) => {
                let Some(name) = body["name"].as_str().filter(|n| !n.is_empty()) else {
                    return Ok(invalid("name required"));
                };
                let Some(project) = body["project"].as_str().filter(|p| !p.is_empty()) else {
                    return Ok(invalid("project required"));
                };
                let defaults = [&body["project_default"], &body["organization_default"]];
                if defaults.iter().filter(|d| !d.is_null()).count() != 1 {
                    return Ok(invalid(
                        "precisely one of organization_default, project_default must be set",
                    ));
                }
                let policies = [
                    &body["inbound_default_policy"],
                    &body["outbound_default_policy"],
                ];
                if policies
                    .iter()
                    .any(|p| !matches!(p.as_str(), Some("accept" | "drop")))
                {
                    return Ok(invalid("default policies are accept or drop"));
                }
                let n = inner.next();
                let now = rfc3339(inner.clock);
                let group = json!({
                    "id": fake_uuid(14, n),
                    "name": name,
                    "description": body["description"].as_str().unwrap_or_default(),
                    "enable_default_security": true,
                    "inbound_default_policy": body["inbound_default_policy"],
                    "outbound_default_policy": body["outbound_default_policy"],
                    "organization": "",
                    "project": project,
                    "tags": [],
                    "project_default": body["project_default"].as_bool().unwrap_or(false),
                    "creation_date": now,
                    "modification_date": now,
                    "servers": [],
                    "stateful": body["stateful"].as_bool().unwrap_or(false),
                    "state": "available",
                    "zone": zone,
                });
                inner.security_groups.push(group.clone());
                Ok(reply(201, json!({ "security_group": group })))
            }
            ("DELETE", ["security_groups", id]) => {
                let Some(index) = inner.security_groups.iter().position(|g| g["id"] == *id) else {
                    return Ok(api_error(
                        404,
                        "unknown_resource",
                        "security group not found",
                    ));
                };
                if inner
                    .servers
                    .iter()
                    .any(|s| s["security_group"]["id"] == *id)
                {
                    return Ok(api_error(
                        400,
                        "invalid_request_error",
                        "security group is in use",
                    ));
                }
                inner.security_groups.remove(index);
                Ok(Response::empty(204))
            }
            _ => Ok(api_error(404, "not_found", "no route")),
        }
    }
}
