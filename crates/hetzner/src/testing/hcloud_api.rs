//! Fake of the Hetzner Cloud API (`https://api.hetzner.cloud/v1`) for servers and firewalls.
//! Shapes from the OpenAPI document at `docs.hetzner.cloud/cloud.spec.json` and hcloud-go v2
//! (`schema/server.go`, `schema/firewall.go`, `schema/action.go`, `schema/meta.go`,
//! `schema/error.go`), checked 2026-10-24. It answers only fields the real API answers; user
//! data, which the API never returns, is readable through [`FakeHetzner::user_data`].
//!
//! Behaviour: `Authorization: Bearer <token>` or `401 unauthorized`; lists page with `page` and
//! `per_page` (default 25, at most 50, invalid values give the default) and carry
//! `meta.pagination`; `label_selector` and `name` filter; a created server is `initializing`
//! and `running` from the read after; `PUT /servers/{id}` replaces the whole label set;
//! `DELETE /servers/{id}` removes the server at once; a firewall applied to resources cannot be
//! deleted (`resource_in_use`); errors are `{ "error": { "code", "message", "details" } }` with
//! the status of the overview's error table.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use async_trait::async_trait;
use ciburst_core::Result;
use ciburst_core::http::{HttpClient, Request, Response};
use serde_json::{Map, Value, json};

use super::selector;
use crate::MAX_USER_DATA;
use crate::vm::{Labels, labels_of, rfc3339, valid_label_key, valid_label_value};

/// The token the fake accepts.
pub const FAKE_TOKEN: &str = "fake-hcloud-token-0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJ";
/// 2026-10-24T00:00:00Z, the fake's clock until a test sets it.
pub const FAKE_CLOCK: u64 = 1_792_800_000;
pub const API_HOST: &str = "api.hetzner.cloud";

const DEFAULT_PER_PAGE: usize = 25;
const MAX_PER_PAGE: usize = 50;

/// Server types the fake offers: name, cores, memory in GB, disk in GB, architecture.
const SERVER_TYPES: [(&str, u32, u32, u32, &str); 5] = [
    ("cx23", 2, 4, 40, "x86"),
    ("cx33", 4, 8, 80, "x86"),
    ("cx43", 8, 16, 160, "x86"),
    ("cx53", 16, 32, 320, "x86"),
    ("cax11", 2, 4, 40, "arm"),
];
/// Images by name: type, OS flavor and version.
const IMAGES: [(&str, &str, &str, &str); 2] = [
    ("docker-ce", "app", "ubuntu", "24.04"),
    ("ubuntu-24.04", "system", "ubuntu", "24.04"),
];
/// Locations: name, city, country, network zone.
const LOCATIONS: [(&str, &str, &str, &str); 4] = [
    ("fsn1", "Falkenstein", "DE", "eu-central"),
    ("nbg1", "Nuremberg", "DE", "eu-central"),
    ("hel1", "Helsinki", "FI", "eu-central"),
    ("ash", "Ashburn, VA", "US", "us-east"),
];

const SERVER_FIELDS: [&str; 15] = [
    "name",
    "server_type",
    "image",
    "location",
    "datacenter",
    "start_after_create",
    "user_data",
    "labels",
    "ssh_keys",
    "volumes",
    "networks",
    "firewalls",
    "automount",
    "public_net",
    "placement_group",
];

/// One recorded request: method, path below `/v1`, raw query and JSON body when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Option<Value>,
}

struct Inner {
    servers: Vec<Value>,
    user_data: BTreeMap<u64, String>,
    firewalls: Vec<Value>,
    unavailable: BTreeSet<String>,
    limit_exceeded: BTreeSet<String>,
    placement_error: BTreeSet<String>,
    rate_limited: bool,
    page_cap: usize,
    firewall_busy: u32,
    clock: u64,
    next: u64,
    calls: Vec<Call>,
}

impl Inner {
    fn next(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn now(&self) -> String {
        rfc3339(self.clock)
    }

    fn server_index(&self, id: u64) -> Option<usize> {
        self.servers.iter().position(|s| s["id"] == id)
    }

    fn firewall_index(&self, id: u64) -> Option<usize> {
        self.firewalls.iter().position(|f| f["id"] == id)
    }

    /// A server as the API shows it: its firewalls follow from the label selectors.
    fn show_server(&self, server: &Value) -> Value {
        let labels = labels_of(server);
        let id = server["id"].as_u64().unwrap_or_default();
        let firewalls: Vec<Value> = self
            .firewalls
            .iter()
            .filter(|f| applies_to(f, id, &labels))
            .map(|f| json!({ "id": f["id"], "status": "applied" }))
            .collect();
        let mut shown = server.clone();
        shown["public_net"]["firewalls"] = json!(firewalls);
        shown
    }

    /// Shows the server at `index`; a starting server has booted by the next read.
    fn read_server(&mut self, index: usize) -> Value {
        let shown = self.show_server(&self.servers[index]);
        if matches!(
            self.servers[index]["status"].as_str(),
            Some("initializing" | "starting")
        ) {
            self.servers[index]["status"] = json!("running");
        }
        shown
    }

    /// A firewall as the API shows it, with the servers each label selector applies to.
    fn show_firewall(&self, firewall: &Value) -> Value {
        let mut shown = firewall.clone();
        let applied: Vec<Value> = firewall["applied_to"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| {
                if entry["type"] != "label_selector" {
                    return entry.clone();
                }
                let requirements = entry["label_selector"]["selector"]
                    .as_str()
                    .and_then(|s| selector::parse(s).ok())
                    .unwrap_or_default();
                let servers: Vec<Value> = self
                    .servers
                    .iter()
                    .filter(|s| selector::matches(&requirements, &labels_of(s)))
                    .map(|s| json!({ "type": "server", "server": { "id": s["id"] } }))
                    .collect();
                let mut entry = entry.clone();
                entry["applied_to_resources"] = json!(servers);
                entry
            })
            .collect();
        shown["applied_to"] = json!(applied);
        shown
    }

    fn action(&mut self, command: &str, resources: &[(u64, &str)]) -> Value {
        let id = self.next();
        let resources: Vec<Value> = resources
            .iter()
            .map(|(id, kind)| json!({ "id": id, "type": kind }))
            .collect();
        json!({
            "id": id,
            "command": command,
            "status": "running",
            "progress": 0,
            "started": self.now(),
            "finished": null,
            "resources": resources,
            "error": null,
        })
    }
}

/// Whether `firewall` applies to the server `id` with `labels`, directly or by a selector.
fn applies_to(firewall: &Value, id: u64, labels: &Labels) -> bool {
    firewall["applied_to"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|entry| match entry["type"].as_str() {
            Some("server") => entry["server"]["id"] == id,
            Some("label_selector") => entry["label_selector"]["selector"]
                .as_str()
                .and_then(|s| selector::parse(s).ok())
                .is_some_and(|r| selector::matches(&r, labels)),
            _ => false,
        })
}

pub struct FakeHetzner {
    inner: Mutex<Inner>,
}

impl Default for FakeHetzner {
    fn default() -> Self {
        FakeHetzner::new()
    }
}

/// The error body of the API.
pub fn api_error(status: u16, code: &str, message: &str) -> Response {
    Response::json_body(
        status,
        &json!({ "error": { "code": code, "message": message, "details": {} } }),
    )
}

fn invalid(message: &str) -> Response {
    api_error(422, "invalid_input", message)
}

fn not_found(what: &str) -> Response {
    api_error(404, "not_found", &format!("{what} not found"))
}

fn reply(status: u16, value: Value) -> Response {
    Response::json_body(status, &value)
}

/// A request URL split into path below `/v1` and decoded query pairs.
struct Url {
    host: String,
    path: String,
    raw_query: String,
    query: Vec<(String, String)>,
}

impl Url {
    fn parse(url: &str) -> Url {
        let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
        let (host, path_query) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let (path, raw_query) = path_query.split_once('?').unwrap_or((path_query, ""));
        let query = raw_query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (decode(k), decode(v))
            })
            .collect();
        Url {
            host: host.to_string(),
            path: path.to_string(),
            raw_query: raw_query.to_string(),
            query,
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, key: &str) -> Vec<&str> {
        self.query
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
            && let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Labels of a request body, or why they are invalid. The `hetzner.cloud/` prefix is reserved.
fn request_labels(value: &Value) -> std::result::Result<Labels, String> {
    let Some(object) = value.as_object() else {
        return Err("labels must be an object".into());
    };
    let mut labels = Labels::new();
    for (key, value) in object {
        let Some(value) = value.as_str() else {
            return Err(format!("label {key} must be a string"));
        };
        if !valid_label_key(key) || key.starts_with("hetzner.cloud/") {
            return Err(format!("label key {key} is invalid"));
        }
        if !valid_label_value(value) {
            return Err(format!("label value of {key} is invalid"));
        }
        labels.insert(key.clone(), value.to_string());
    }
    Ok(labels)
}

/// A hostname as in RFC 1123: dot-separated labels of letters, digits and hyphens, each 1 to 63
/// characters and not starting or ending with a hyphen.
fn valid_hostname(name: &str) -> bool {
    name.len() <= 253
        && name.split('.').all(|label| {
            let bytes = label.as_bytes();
            !bytes.is_empty()
                && bytes.len() <= 63
                && bytes[0] != b'-'
                && bytes[bytes.len() - 1] != b'-'
                && bytes
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        })
}

/// A firewall resource entry of a request, normalized, or the error answer.
fn resource_entry(inner: &Inner, entry: &Value) -> std::result::Result<Value, Response> {
    match entry["type"].as_str() {
        Some("server") => {
            let Some(id) = entry["server"]["id"].as_u64() else {
                return Err(invalid("server.id required"));
            };
            if inner.server_index(id).is_none() {
                return Err(api_error(
                    404,
                    "firewall_resource_not_found",
                    "server not found",
                ));
            }
            Ok(json!({ "type": "server", "server": { "id": id } }))
        }
        Some("label_selector") => {
            let Some(text) = entry["label_selector"]["selector"].as_str() else {
                return Err(invalid("label_selector.selector required"));
            };
            if let Err(error) = selector::parse(text) {
                return Err(invalid(&format!("invalid label selector: {error}")));
            }
            Ok(json!({ "type": "label_selector", "label_selector": { "selector": text } }))
        }
        _ => Err(invalid("type must be server or label_selector")),
    }
}

fn valid_rule(rule: &Value) -> bool {
    let direction_ok = matches!(rule["direction"].as_str(), Some("in" | "out"));
    let protocol_ok = matches!(
        rule["protocol"].as_str(),
        Some("tcp" | "udp" | "icmp" | "esp" | "gre")
    );
    direction_ok && protocol_ok
}

fn rules_of(value: &Value) -> std::result::Result<Vec<Value>, Response> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(rules) if rules.iter().all(valid_rule) => Ok(rules
            .iter()
            .map(|r| {
                json!({
                    "direction": r["direction"],
                    "source_ips": r.get("source_ips").cloned().unwrap_or(json!([])),
                    "destination_ips": r.get("destination_ips").cloned().unwrap_or(json!([])),
                    "protocol": r["protocol"],
                    "port": r.get("port").cloned().unwrap_or(Value::Null),
                    "description": r.get("description").cloned().unwrap_or(Value::Null),
                })
            })
            .collect()),
        _ => Err(invalid(
            "rules must be a list of rules with direction and protocol",
        )),
    }
}

fn page(items: Vec<Value>, url: &Url, cap: usize) -> (Vec<Value>, Value) {
    let per_page = url
        .get("per_page")
        .and_then(|p| p.parse::<usize>().ok())
        .filter(|p| (1..=MAX_PER_PAGE).contains(p))
        .unwrap_or(DEFAULT_PER_PAGE)
        .min(cap);
    let number = url
        .get("page")
        .and_then(|p| p.parse::<usize>().ok())
        .filter(|p| *p >= 1)
        .unwrap_or(1);
    let total = items.len();
    let last = total.div_ceil(per_page).max(1);
    let items: Vec<Value> = items
        .into_iter()
        .skip((number - 1) * per_page)
        .take(per_page)
        .collect();
    let meta = json!({
        "pagination": {
            "page": number,
            "per_page": per_page,
            "previous_page": if number > 1 { json!(number - 1) } else { Value::Null },
            "next_page": if number < last { json!(number + 1) } else { Value::Null },
            "last_page": last,
            "total_entries": total,
        }
    });
    (items, meta)
}

impl FakeHetzner {
    pub fn new() -> FakeHetzner {
        FakeHetzner {
            inner: Mutex::new(Inner {
                servers: Vec::new(),
                user_data: BTreeMap::new(),
                firewalls: Vec::new(),
                unavailable: BTreeSet::new(),
                limit_exceeded: BTreeSet::new(),
                placement_error: BTreeSet::new(),
                rate_limited: false,
                page_cap: MAX_PER_PAGE,
                firewall_busy: 0,
                clock: FAKE_CLOCK,
                next: 4_200_000,
                calls: Vec::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("fake hetzner")
    }

    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// Every request that is not a GET.
    pub fn writes(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| c.method != "GET")
            .collect()
    }

    /// Servers as stored, without advancing their status.
    pub fn servers(&self) -> Vec<Value> {
        let inner = self.lock();
        inner.servers.iter().map(|s| inner.show_server(s)).collect()
    }

    pub fn server(&self, name: &str) -> Option<Value> {
        let inner = self.lock();
        inner
            .servers
            .iter()
            .find(|s| s["name"] == name)
            .map(|s| inner.show_server(s))
    }

    /// The user data a server was created with; the API never returns it.
    pub fn user_data(&self, name: &str) -> Option<String> {
        let inner = self.lock();
        let id = inner.servers.iter().find(|s| s["name"] == name)?["id"].as_u64()?;
        inner.user_data.get(&id).cloned()
    }

    pub fn firewalls(&self) -> Vec<Value> {
        let inner = self.lock();
        inner
            .firewalls
            .iter()
            .map(|f| inner.show_firewall(f))
            .collect()
    }

    /// Unix seconds the next resource's `created` carries.
    pub fn set_clock(&self, unix: u64) {
        self.lock().clock = unix;
    }

    /// Creating a server of `server_type` fails with `resource_unavailable`.
    pub fn set_unavailable(&self, server_type: &str) {
        self.lock().unavailable.insert(server_type.to_string());
    }

    /// Creating a server of `server_type` fails with `resource_limit_exceeded`.
    pub fn set_limit_exceeded(&self, server_type: &str) {
        self.lock().limit_exceeded.insert(server_type.to_string());
    }

    /// Creating a server of `server_type` fails with `placement_error`.
    pub fn set_placement_error(&self, server_type: &str) {
        self.lock().placement_error.insert(server_type.to_string());
    }

    /// Every request fails with `rate_limit_exceeded` while set.
    pub fn set_rate_limited(&self, limited: bool) {
        self.lock().rate_limited = limited;
    }

    /// Caps every page below the API's 50 items, so tests page with a few resources.
    pub fn set_page_cap(&self, cap: usize) {
        self.lock().page_cap = cap.clamp(1, MAX_PER_PAGE);
    }

    /// The next `count` firewall deletions fail with `resource_in_use`, as while removing the
    /// firewall from its resources still runs.
    pub fn set_firewall_busy(&self, count: u32) {
        self.lock().firewall_busy = count;
    }

    pub fn set_status(&self, name: &str, status: &str) {
        let mut inner = self.lock();
        if let Some(server) = inner.servers.iter_mut().find(|s| s["name"] == name) {
            server["status"] = json!(status);
        }
    }

    /// A locked server (another action runs on it) refuses deletion with `locked`.
    pub fn set_locked(&self, name: &str, locked: bool) {
        let mut inner = self.lock();
        if let Some(server) = inner.servers.iter_mut().find(|s| s["name"] == name) {
            server["locked"] = json!(locked);
        }
    }

    pub fn set_labels(&self, name: &str, labels: &[(&str, &str)]) {
        let mut inner = self.lock();
        if let Some(server) = inner.servers.iter_mut().find(|s| s["name"] == name) {
            let labels: Map<String, Value> = labels
                .iter()
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect();
            server["labels"] = Value::Object(labels);
        }
    }

    /// Adds a firewall rule, as a person might in the console.
    pub fn add_firewall_rule(&self, firewall: &str, rule: Value) {
        let mut inner = self.lock();
        if let Some(f) = inner.firewalls.iter_mut().find(|f| f["name"] == firewall)
            && let Some(rules) = f["rules"].as_array_mut()
        {
            rules.push(rule);
        }
    }

    /// Drops every resource the firewall is applied to.
    pub fn clear_firewall(&self, firewall: &str) {
        let mut inner = self.lock();
        if let Some(f) = inner.firewalls.iter_mut().find(|f| f["name"] == firewall) {
            f["applied_to"] = json!([]);
        }
    }

    /// A running server another deployment or a person created; returns its id.
    pub fn add_server(&self, name: &str, labels: &[(&str, &str)]) -> u64 {
        let mut inner = self.lock();
        let labels: Map<String, Value> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect();
        let server = new_server(&mut inner, name, "cx23", "docker-ce", "nbg1", labels);
        let id = server["id"].as_u64().unwrap_or_default();
        let index = inner.servers.len();
        inner.servers.push(server);
        inner.servers[index]["status"] = json!("running");
        id
    }
}

fn server_type(name: &str) -> Option<Value> {
    let (index, (name, cores, memory, disk, arch)) =
        SERVER_TYPES.iter().enumerate().find(|(_, t)| t.0 == name)?;
    Some(json!({
        "id": 100 + index,
        "name": name,
        "description": name.to_uppercase(),
        "cores": cores,
        "memory": memory,
        "disk": disk,
        "deprecated": false,
        "prices": [],
        "storage_type": "local",
        "cpu_type": "shared",
        "category": "cost_optimized",
        "architecture": arch,
        "deprecation": null,
        "locations": [],
    }))
}

fn image(name: &str, architecture: &str) -> Option<Value> {
    let (index, (name, kind, flavor, version)) =
        IMAGES.iter().enumerate().find(|(_, i)| i.0 == name)?;
    Some(json!({
        "id": 200 + index,
        "type": kind,
        "status": "available",
        "name": name,
        "description": format!("{flavor} {version}"),
        "image_size": null,
        "disk_size": 5,
        "created": "2024-04-25T00:00:00+00:00",
        "created_from": null,
        "bound_to": null,
        "os_flavor": flavor,
        "os_version": version,
        "rapid_deploy": true,
        "protection": { "delete": false },
        "deprecated": null,
        "deleted": null,
        "labels": {},
        "architecture": architecture,
    }))
}

fn location(name: &str) -> Option<Value> {
    let (index, (name, city, country, zone)) =
        LOCATIONS.iter().enumerate().find(|(_, l)| l.0 == name)?;
    Some(json!({
        "id": 1 + index,
        "name": name,
        "description": format!("{city} DC Park 1"),
        "country": country,
        "city": city,
        "latitude": 0.0,
        "longitude": 0.0,
        "network_zone": zone,
    }))
}

fn new_server(
    inner: &mut Inner,
    name: &str,
    type_name: &str,
    image_name: &str,
    location_name: &str,
    labels: Map<String, Value>,
) -> Value {
    let id = inner.next();
    let kind = server_type(type_name).unwrap_or(Value::Null);
    let arch = kind["architecture"].as_str().unwrap_or("x86").to_string();
    let n = id % 250;
    json!({
        "id": id,
        "name": name,
        "status": "initializing",
        "created": inner.now(),
        "public_net": {
            "ipv4": {
                "id": id + 1_000_000,
                "ip": format!("203.0.113.{n}"),
                "blocked": false,
                "dns_ptr": format!("static.{n}.113.0.203.clients.example.invalid"),
            },
            "ipv6": {
                "id": id + 2_000_000,
                "ip": format!("2001:db8:{n:x}::/64"),
                "blocked": false,
                "dns_ptr": [],
            },
            "floating_ips": [],
            "firewalls": [],
        },
        "private_net": [],
        "server_type": kind,
        "location": location(location_name).unwrap_or(Value::Null),
        "image": image(image_name, &arch).unwrap_or(Value::Null),
        "iso": null,
        "rescue_enabled": false,
        "locked": false,
        "backup_window": null,
        "outgoing_traffic": null,
        "ingoing_traffic": null,
        "included_traffic": 21_990_232_555_520u64,
        "protection": { "delete": false, "rebuild": false },
        "labels": labels,
        "volumes": [],
        "load_balancers": [],
        "primary_disk_size": kind["disk"],
        "placement_group": null,
    })
}

impl FakeHetzner {
    fn create_server(inner: &mut Inner, body: &Value) -> Response {
        let Some(object) = body.as_object() else {
            return invalid("body must be an object");
        };
        if let Some(field) = object.keys().find(|k| !SERVER_FIELDS.contains(&k.as_str())) {
            return invalid(&format!("unknown field {field}"));
        }
        let Some(name) = body["name"].as_str().filter(|n| valid_hostname(n)) else {
            return invalid("invalid input in field 'name': must be a valid hostname");
        };
        let Some(type_name) = body["server_type"].as_str() else {
            return invalid("invalid input in field 'server_type'");
        };
        if server_type(type_name).is_none() {
            return invalid("server type not found");
        }
        let Some(image_name) = body["image"].as_str() else {
            return invalid("invalid input in field 'image'");
        };
        if image(image_name, "x86").is_none() {
            return invalid("image not found");
        }
        let location_name = match &body["location"] {
            Value::Null => "nbg1",
            Value::String(l) if location(l).is_some() => l.as_str(),
            _ => return invalid("location not found"),
        };
        let labels = match &body["labels"] {
            Value::Null => Labels::new(),
            value => match request_labels(value) {
                Ok(labels) => labels,
                Err(message) => return invalid(&message),
            },
        };
        let user_data = match &body["user_data"] {
            Value::Null => None,
            Value::String(text) if text.len() <= MAX_USER_DATA => Some(text.clone()),
            Value::String(_) => {
                return invalid("invalid input in field 'user_data': is too long");
            }
            _ => return invalid("invalid input in field 'user_data'"),
        };
        if !body["start_after_create"].is_null() && !body["start_after_create"].is_boolean() {
            return invalid("invalid input in field 'start_after_create'");
        }
        for field in ["enable_ipv4", "enable_ipv6"] {
            let value = &body["public_net"][field];
            if !value.is_null() && !value.is_boolean() {
                return invalid(&format!("invalid input in field 'public_net.{field}'"));
            }
        }
        if inner.servers.iter().any(|s| s["name"] == name) {
            return api_error(409, "uniqueness_error", "server name is already used");
        }
        if inner.unavailable.contains(type_name) {
            return api_error(
                412,
                "resource_unavailable",
                &format!("server type {type_name} is unavailable in {location_name}"),
            );
        }
        if inner.limit_exceeded.contains(type_name) {
            return api_error(403, "resource_limit_exceeded", "server limit exceeded");
        }
        if inner.placement_error.contains(type_name) {
            return api_error(422, "placement_error", "error during placement");
        }
        let labels: Map<String, Value> = labels.into_iter().map(|(k, v)| (k, json!(v))).collect();
        let mut server = new_server(inner, name, type_name, image_name, location_name, labels);
        for (field, key) in [("enable_ipv4", "ipv4"), ("enable_ipv6", "ipv6")] {
            if body["public_net"][field] == false {
                server["public_net"][key] = Value::Null;
            }
        }
        let id = server["id"].as_u64().unwrap_or_default();
        if let Some(text) = user_data {
            inner.user_data.insert(id, text);
        }
        inner.servers.push(server.clone());
        let action = inner.action("create_server", &[(id, "server")]);
        let start = body["start_after_create"].as_bool().unwrap_or(true);
        let next_actions = if start {
            vec![inner.action("start_server", &[(id, "server")])]
        } else {
            Vec::new()
        };
        let password = if body["ssh_keys"].as_array().is_some_and(|k| !k.is_empty()) {
            Value::Null
        } else {
            json!(format!("fake-root-password-{id}"))
        };
        reply(
            201,
            json!({
                "server": inner.show_server(&server),
                "action": action,
                "next_actions": next_actions,
                "root_password": password,
            }),
        )
    }

    fn update_server(inner: &mut Inner, index: usize, body: &Value) -> Response {
        let Some(object) = body.as_object() else {
            return invalid("body must be an object");
        };
        if let Some(field) = object
            .keys()
            .find(|k| !["name", "labels"].contains(&k.as_str()))
        {
            return invalid(&format!("unknown field {field}"));
        }
        if let Some(name) = object.get("name") {
            let Some(name) = name.as_str().filter(|n| valid_hostname(n)) else {
                return invalid("invalid input in field 'name': must be a valid hostname");
            };
            let id = &inner.servers[index]["id"];
            if inner
                .servers
                .iter()
                .any(|s| s["name"] == name && s["id"] != *id)
            {
                return api_error(409, "uniqueness_error", "server name is already used");
            }
            inner.servers[index]["name"] = json!(name);
        }
        if let Some(labels) = object.get("labels") {
            match request_labels(labels) {
                Ok(labels) => {
                    let labels: Map<String, Value> =
                        labels.into_iter().map(|(k, v)| (k, json!(v))).collect();
                    inner.servers[index]["labels"] = Value::Object(labels);
                }
                Err(message) => return invalid(&message),
            }
        }
        let shown = inner.show_server(&inner.servers[index]);
        reply(200, json!({ "server": shown }))
    }

    fn delete_server(inner: &mut Inner, index: usize) -> Response {
        if inner.servers[index]["locked"] == true {
            return api_error(423, "locked", "server is locked");
        }
        let removed = inner.servers.remove(index);
        let id = removed["id"].as_u64().unwrap_or_default();
        inner.user_data.remove(&id);
        for firewall in inner.firewalls.iter_mut() {
            if let Some(applied) = firewall["applied_to"].as_array_mut() {
                applied.retain(|a| !(a["type"] == "server" && a["server"]["id"] == id));
            }
        }
        let action = inner.action("delete_server", &[(id, "server")]);
        reply(200, json!({ "action": action }))
    }

    fn create_firewall(inner: &mut Inner, body: &Value) -> Response {
        let Some(object) = body.as_object() else {
            return invalid("body must be an object");
        };
        if let Some(field) = object
            .keys()
            .find(|k| !["name", "labels", "rules", "apply_to"].contains(&k.as_str()))
        {
            return invalid(&format!("unknown field {field}"));
        }
        let Some(name) = body["name"].as_str().filter(|n| !n.is_empty()) else {
            return invalid("invalid input in field 'name'");
        };
        if inner.firewalls.iter().any(|f| f["name"] == name) {
            return api_error(409, "uniqueness_error", "firewall name is already used");
        }
        let labels = match &body["labels"] {
            Value::Null => Labels::new(),
            value => match request_labels(value) {
                Ok(labels) => labels,
                Err(message) => return invalid(&message),
            },
        };
        let rules = match rules_of(&body["rules"]) {
            Ok(rules) => rules,
            Err(response) => return response,
        };
        let mut applied = Vec::new();
        for entry in body["apply_to"].as_array().into_iter().flatten() {
            match resource_entry(inner, entry) {
                Ok(entry) if applied.contains(&entry) => {
                    return api_error(422, "server_already_added", "resource added twice");
                }
                Ok(entry) => applied.push(entry),
                Err(response) => return response,
            }
        }
        let id = inner.next();
        let firewall = json!({
            "id": id,
            "name": name,
            "labels": labels,
            "created": inner.now(),
            "rules": rules,
            "applied_to": applied,
        });
        inner.firewalls.push(firewall.clone());
        let actions: Vec<Value> = applied
            .iter()
            .map(|_| inner.action("apply_firewall", &[(id, "firewall")]))
            .collect();
        let shown = inner.show_firewall(&firewall);
        reply(201, json!({ "firewall": shown, "actions": actions }))
    }

    fn firewall_action(inner: &mut Inner, index: usize, action: &str, body: &Value) -> Response {
        let id = inner.firewalls[index]["id"].as_u64().unwrap_or_default();
        match action {
            "set_rules" => {
                let rules = match rules_of(&body["rules"]) {
                    Ok(rules) if body["rules"].is_array() => rules,
                    Ok(_) => return invalid("invalid input in field 'rules'"),
                    Err(response) => return response,
                };
                inner.firewalls[index]["rules"] = json!(rules);
                let action = inner.action("set_firewall_rules", &[(id, "firewall")]);
                reply(201, json!({ "actions": [action] }))
            }
            "apply_to_resources" | "remove_from_resources" => {
                let field = if action == "apply_to_resources" {
                    "apply_to"
                } else {
                    "remove_from"
                };
                let Some(entries) = body[field].as_array() else {
                    return invalid(&format!("invalid input in field '{field}'"));
                };
                let mut normalized = Vec::new();
                for entry in entries {
                    match resource_entry(inner, entry) {
                        Ok(entry) => normalized.push(entry),
                        Err(response) => return response,
                    }
                }
                let mut applied: Vec<Value> = inner.firewalls[index]["applied_to"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let mut actions = Vec::new();
                for entry in normalized {
                    let present = applied.contains(&entry);
                    if field == "apply_to" {
                        if present {
                            return api_error(
                                422,
                                "firewall_already_applied",
                                "firewall already applied to the resource",
                            );
                        }
                        applied.push(entry);
                        actions.push(inner.action("apply_firewall", &[(id, "firewall")]));
                    } else {
                        if !present {
                            let by_selector = entry["type"] == "server"
                                && inner.show_firewall(&inner.firewalls[index])["applied_to"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .flat_map(|a| {
                                        a["applied_to_resources"]
                                            .as_array()
                                            .cloned()
                                            .unwrap_or_default()
                                    })
                                    .any(|r| r["server"]["id"] == entry["server"]["id"]);
                            return if by_selector {
                                api_error(
                                    422,
                                    "firewall_managed_by_label_selector",
                                    "firewall is applied via a label selector",
                                )
                            } else {
                                api_error(
                                    404,
                                    "firewall_resource_not_found",
                                    "the firewall is not applied to the resource",
                                )
                            };
                        }
                        applied.retain(|a| *a != entry);
                        actions.push(inner.action("remove_firewall", &[(id, "firewall")]));
                    }
                }
                inner.firewalls[index]["applied_to"] = json!(applied);
                reply(201, json!({ "actions": actions }))
            }
            _ => not_found("action"),
        }
    }

    fn route(&self, request: &Request, url: &Url, inner: &mut Inner) -> Response {
        if url.host != API_HOST {
            return not_found("host");
        }
        let Some(path) = url.path.strip_prefix("/v1/") else {
            return not_found("path");
        };
        let expected = format!("Bearer {FAKE_TOKEN}");
        if request.header_value("Authorization") != Some(expected.as_str()) {
            return api_error(401, "unauthorized", "unable to authenticate");
        }
        if inner.rate_limited {
            return api_error(
                429,
                "rate_limit_exceeded",
                "limit of 3600 requests per hour reached",
            );
        }
        let body = match &request.body {
            None => Value::Null,
            Some(bytes) => {
                let json_type = request
                    .header_value("content-type")
                    .is_some_and(|c| c.starts_with("application/json"));
                match serde_json::from_slice::<Value>(bytes) {
                    Ok(value) if json_type => value,
                    _ => return api_error(400, "json_error", "invalid JSON input"),
                }
            }
        };
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let id = |text: &str| text.parse::<u64>().ok();
        match (request.method.as_str(), segments.as_slice()) {
            ("GET", ["servers"]) => {
                let requirements = match url.get("label_selector").map(selector::parse) {
                    None => Vec::new(),
                    Some(Ok(r)) => r,
                    Some(Err(error)) => {
                        return invalid(&format!("invalid label selector: {error}"));
                    }
                };
                let statuses = url.all("status");
                let indices: Vec<usize> = (0..inner.servers.len())
                    .filter(|&i| {
                        let server = &inner.servers[i];
                        url.get("name").is_none_or(|n| server["name"] == n)
                            && selector::matches(&requirements, &labels_of(server))
                            && (statuses.is_empty()
                                || statuses.iter().any(|s| server["status"] == *s))
                    })
                    .collect();
                let items: Vec<Value> = indices.into_iter().map(|i| inner.read_server(i)).collect();
                let (items, meta) = page(items, url, inner.page_cap);
                reply(200, json!({ "servers": items, "meta": meta }))
            }
            ("POST", ["servers"]) => FakeHetzner::create_server(inner, &body),
            (method, ["servers", server_id]) => {
                let Some(index) = id(server_id).and_then(|i| inner.server_index(i)) else {
                    return not_found("server");
                };
                match method {
                    "GET" => {
                        let server = inner.read_server(index);
                        reply(200, json!({ "server": server }))
                    }
                    "PUT" => FakeHetzner::update_server(inner, index, &body),
                    "DELETE" => FakeHetzner::delete_server(inner, index),
                    _ => api_error(405, "method_not_allowed", "method not allowed"),
                }
            }
            ("GET", ["firewalls"]) => {
                let requirements = match url.get("label_selector").map(selector::parse) {
                    None => Vec::new(),
                    Some(Ok(r)) => r,
                    Some(Err(error)) => {
                        return invalid(&format!("invalid label selector: {error}"));
                    }
                };
                let items: Vec<Value> = inner
                    .firewalls
                    .iter()
                    .filter(|f| url.get("name").is_none_or(|n| f["name"] == n))
                    .filter(|f| selector::matches(&requirements, &labels_of(f)))
                    .map(|f| inner.show_firewall(f))
                    .collect();
                let (items, meta) = page(items, url, inner.page_cap);
                reply(200, json!({ "firewalls": items, "meta": meta }))
            }
            ("POST", ["firewalls"]) => FakeHetzner::create_firewall(inner, &body),
            (method, ["firewalls", firewall_id]) => {
                let Some(index) = id(firewall_id).and_then(|i| inner.firewall_index(i)) else {
                    return not_found("firewall");
                };
                match method {
                    "GET" => {
                        let shown = inner.show_firewall(&inner.firewalls[index]);
                        reply(200, json!({ "firewall": shown }))
                    }
                    "DELETE" => {
                        let in_use = inner.firewalls[index]["applied_to"]
                            .as_array()
                            .is_some_and(|a| !a.is_empty());
                        if in_use || inner.firewall_busy > 0 {
                            inner.firewall_busy = inner.firewall_busy.saturating_sub(1);
                            return api_error(
                                422,
                                "resource_in_use",
                                "firewall is still applied to a resource",
                            );
                        }
                        inner.firewalls.remove(index);
                        Response::empty(204)
                    }
                    _ => api_error(405, "method_not_allowed", "method not allowed"),
                }
            }
            ("POST", ["firewalls", firewall_id, "actions", action]) => {
                let Some(index) = id(firewall_id).and_then(|i| inner.firewall_index(i)) else {
                    return not_found("firewall");
                };
                FakeHetzner::firewall_action(inner, index, action, &body)
            }
            _ => not_found("route"),
        }
    }
}

#[async_trait]
impl HttpClient for FakeHetzner {
    async fn send(&self, request: Request) -> Result<Response> {
        let url = Url::parse(&request.url);
        let mut inner = self.lock();
        inner.calls.push(Call {
            method: request.method.clone(),
            path: url
                .path
                .strip_prefix("/v1")
                .unwrap_or(&url.path)
                .to_string(),
            query: url.raw_query.clone(),
            body: request
                .body
                .as_ref()
                .and_then(|b| serde_json::from_slice(b).ok()),
        });
        Ok(self.route(&request, &url, &mut inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(fake: &FakeHetzner, url: &str) -> (u16, Value) {
        let request = Request::new("GET", format!("https://{API_HOST}/v1{url}"))
            .header("Authorization", format!("Bearer {FAKE_TOKEN}"));
        let response = fake.send(request).await.unwrap();
        (response.status, response.json().unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn pages_carry_meta_pagination() {
        let fake = FakeHetzner::new();
        for n in 0..5 {
            fake.add_server(&format!("s{n}"), &[("ciburst", "true")]);
        }
        fake.set_page_cap(2);
        let (status, body) = get(&fake, "/servers?per_page=50&page=2").await;
        assert_eq!(status, 200);
        assert_eq!(body["servers"].as_array().unwrap().len(), 2);
        assert_eq!(body["servers"][0]["name"], "s2");
        assert_eq!(
            body["meta"]["pagination"],
            json!({
                "page": 2, "per_page": 2, "previous_page": 1, "next_page": 3,
                "last_page": 3, "total_entries": 5
            })
        );
        let (_, last) = get(&fake, "/servers?page=3").await;
        assert_eq!(last["meta"]["pagination"]["next_page"], Value::Null);
        let fake = FakeHetzner::new();
        let (_, empty) = get(&fake, "/servers?per_page=500").await;
        assert_eq!(empty["meta"]["pagination"]["per_page"], 25);
        assert_eq!(empty["meta"]["pagination"]["last_page"], 1);
    }

    #[tokio::test]
    async fn filters_by_selector_and_refuses_bad_ones() {
        let fake = FakeHetzner::new();
        fake.add_server("a", &[("ciburst", "true"), ("ciburst-name", "x")]);
        fake.add_server("b", &[("ciburst", "true"), ("ciburst-name", "y")]);
        let (_, body) = get(&fake, "/servers?label_selector=ciburst-name%3D%3Dx").await;
        let names: Vec<&str> = body["servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["a"]);
        let (status, body) = get(&fake, "/servers?label_selector=a%3D%3D-b").await;
        assert_eq!(status, 422);
        assert_eq!(body["error"]["code"], "invalid_input");
        let (_, body) = get(&fake, "/servers?name=b").await;
        assert_eq!(body["servers"][0]["name"], "b");
    }

    #[tokio::test]
    async fn answers_only_with_the_token() {
        let fake = FakeHetzner::new();
        let request = Request::new("GET", format!("https://{API_HOST}/v1/servers"))
            .header("Authorization", "Bearer wrong");
        let response = fake.send(request).await.unwrap();
        assert_eq!(response.status, 401);
        let body: Value = response.json().unwrap();
        assert_eq!(body["error"]["code"], "unauthorized");
        assert!(!response.text().contains("wrong"));
    }

    #[test]
    fn hostnames_follow_rfc_1123() {
        for good in ["a", "demo-ci-0000abcd", "a.b", &"x".repeat(63)] {
            assert!(valid_hostname(good), "{good}");
        }
        for bad in ["", "-a", "a-", "a_b", "a..b", &"x".repeat(64)] {
            assert!(!valid_hostname(bad), "{bad}");
        }
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(decode("a%3D%3Db%2Cc"), "a==b,c");
        assert_eq!(decode("%2"), "%2");
        assert_eq!(decode("%"), "%");
        assert_eq!(decode("a+b"), "a b");
    }
}
