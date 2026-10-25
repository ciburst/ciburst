//! Provider `hetzner` (spec section 6.2, D34, M4 rule R12): VM pools on Hetzner Cloud servers
//! with the shared cloud-init and agent of spec section 6.1, state in server labels, and a
//! firewall without inbound rules. The dispatcher, cache and registry stay with the main
//! provider; `Composite` hands this provider the pools whose `runtime` is [`RUNTIME`].
//!
//! Every resource is found by name or label; nothing is stored locally. Run ids are
//! `hetzner:<server name>`, so the name chosen before create carries the VM's token.

pub mod api;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod vm;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ciburst_core::cloud_init::{
    AGENT_SHA256_KEY, AGENT_URL_KEY, DISPATCH_URL, Download, RUN_ID, RUNNER_SHA256, RUNNER_URL_KEY,
    VM_TOKEN,
};
use ciburst_core::config::{CacheType, Config, Pool};
use ciburst_core::http::HttpClient;
use ciburst_core::log::Log;
use ciburst_core::provision::keys;
use ciburst_core::types::{
    Action, Billing, Capabilities, Outputs, Provider, Rotation, Run, StartRequest, Step,
    VmCapacity, VmReport,
};
use ciburst_core::{Error, Result};
use serde_json::{Value, json};

use api::{Answer, Api, ApiError};

/// Runtime name, key in the config's `providers`, and prefix of run and step ids.
pub const RUNTIME: &str = "hetzner";
/// The API token's variable in `.local/hetzner.env`, the function's secret environment and
/// the provider's outputs.
pub const TOKEN_VAR: &str = "HCLOUD_TOKEN";
pub const API: &str = "https://api.hetzner.cloud/v1";

/// The firewall step of `plan` and `apply`.
pub const FIREWALL_STEP: &str = "hetzner:firewall";
/// `providers.hetzner.location` when unset.
pub const DEFAULT_LOCATION: &str = "nbg1";
/// `providers.hetzner.image` when unset: Ubuntu 24.04 with Docker CE.
pub const DEFAULT_IMAGE: &str = "docker-ce";
/// Server types tried in order when a pool names none.
pub const DEFAULT_SERVER_TYPES: [&str; 2] = ["cx33", "cx43"];
/// The API refuses user data above 32 KiB.
pub const MAX_USER_DATA: usize = 32 * 1024;

/// Attempts to delete the firewall while removing it from its resources is still running.
const FIREWALL_DELETE_ATTEMPTS: u32 = 10;
const FIREWALL_RETRY_DELAY: Duration = Duration::from_secs(3);

const SETTINGS: [&str; 2] = ["location", "image"];

/// Cache variables a VM needs when the config's cache is S3: the location from the main
/// provider's `shared_env`, the key from the dispatcher (M4 rule R11).
const CACHE_KEYS: [&str; 6] = [
    "CACHE_TYPE",
    "CACHE_S3_SERVER_ADDRESS",
    "CACHE_S3_BUCKET_NAME",
    "CACHE_S3_BUCKET_LOCATION",
    "CACHE_S3_ACCESS_KEY",
    "CACHE_S3_SECRET_KEY",
];
const CACHE_SHARED: &str = "CACHE_SHARED";

/// What a new VM is built from: `vm.env` without run id and token, the VM secret, and where
/// it downloads runner and agent (D38, M6 rule R6).
struct Base {
    env: BTreeMap<String, String>,
    secret: String,
    runner_url: String,
    agent_url: String,
    agent_sha256: String,
}

/// The settings of `providers.hetzner`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub location: String,
    pub image: String,
}

impl Settings {
    /// The settings with their defaults; every invalid or unknown key is named.
    pub fn from_config(config: &Config) -> std::result::Result<Settings, Vec<String>> {
        let section = config.providers.get(RUNTIME);
        let mut errors = Vec::new();
        for key in section.into_iter().flat_map(|s| s.keys()) {
            if !SETTINGS.contains(&key.as_str()) {
                errors.push(format!(
                    "providers.{RUNTIME}.{key}: unknown setting; known: {}",
                    SETTINGS.join(", ")
                ));
            }
        }
        let mut setting =
            |key: &str, default: &str, valid: fn(&str) -> bool, what: &str| match section
                .and_then(|s| s.get(key))
            {
                None | Some(Value::Null) => default.to_string(),
                Some(Value::String(value)) if valid(value) => value.clone(),
                Some(_) => {
                    errors.push(format!("providers.{RUNTIME}.{key}: {what}"));
                    default.to_string()
                }
            };
        let location = setting(
            "location",
            DEFAULT_LOCATION,
            valid_location,
            "a location name such as nbg1, fsn1 or hel1",
        );
        let image = setting(
            "image",
            DEFAULT_IMAGE,
            valid_image,
            "an image name such as docker-ce, or an image id",
        );
        if errors.is_empty() {
            Ok(Settings { location, image })
        } else {
            Err(errors)
        }
    }
}

fn valid_location(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_image(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn valid_server_type(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The pool's server types in fallback order, [`DEFAULT_SERVER_TYPES`] when unset.
pub fn server_types(pool: &Pool) -> Vec<String> {
    if pool.instance_types.is_empty() {
        DEFAULT_SERVER_TYPES.iter().map(|t| t.to_string()).collect()
    } else {
        pool.instance_types.clone()
    }
}

/// Whether the pool runs on this provider.
pub fn is_mine(pool: &Pool) -> bool {
    pool.runtime.as_deref() == Some(RUNTIME)
}

pub struct Hetzner {
    config: Arc<Config>,
    token: String,
    api: Api,
    log: Log,
    retry_delay: Duration,
}

impl Hetzner {
    pub fn new(config: Arc<Config>, token: String, http: Arc<dyn HttpClient>, log: Log) -> Hetzner {
        let api = Api::new(http, API, &token);
        Hetzner {
            config,
            token,
            api,
            log,
            retry_delay: FIREWALL_RETRY_DELAY,
        }
    }

    /// The pause between attempts to delete the firewall; tests pass zero.
    pub fn with_retry_delay(mut self, delay: Duration) -> Hetzner {
        self.retry_delay = delay;
        self
    }

    fn name(&self) -> &str {
        &self.config.name
    }

    fn firewall_name(&self) -> String {
        format!("{}-vm", self.name())
    }

    /// The pools of this provider with their index in the config.
    fn pools(&self) -> Vec<(usize, &Pool)> {
        self.config
            .pools
            .iter()
            .enumerate()
            .filter(|(_, pool)| is_mine(pool))
            .collect()
    }

    /// Why `pool` cannot run on Hetzner: name length, label values and server type names.
    fn pool_errors(&self, index: usize, pool: &Pool) -> Vec<String> {
        let mut errors = Vec::new();
        let tag = &pool.tag;
        if !vm::valid_label_value(tag) {
            errors.push(format!(
                "pools[{index}].tag: {tag} must end with a letter or digit to be a Hetzner label value"
            ));
        }
        if !vm::server_name_fits(self.name(), tag) {
            errors.push(format!(
                "pools[{index}].tag: {tag} makes the Hetzner server name {}-{tag}-<8 hex> longer \
                 than {} characters",
                self.name(),
                vm::MAX_SERVER_NAME
            ));
        }
        for server_type in &pool.instance_types {
            if !valid_server_type(server_type) {
                errors.push(format!(
                    "pools[{index}].instanceTypes: {server_type} is not a Hetzner server type name"
                ));
            }
        }
        errors
    }

    /// The settings, after checking them, the deployment name and the pools of this provider.
    fn checked(&self, pools: &[(usize, &Pool)]) -> Result<Settings> {
        let (settings, mut errors) = match Settings::from_config(&self.config) {
            Ok(settings) => (Some(settings), Vec::new()),
            Err(errors) => (None, errors),
        };
        if !vm::valid_label_value(self.name()) {
            errors.push(format!(
                "name: {} must end with a letter or digit to be a Hetzner label value",
                self.name()
            ));
        }
        for (index, pool) in pools {
            errors.extend(self.pool_errors(*index, pool));
        }
        match settings {
            Some(settings) if errors.is_empty() => Ok(settings),
            _ => Err(Error::Config(errors)),
        }
    }

    async fn find_firewall(&self) -> Result<Option<Value>> {
        let name = self.firewall_name();
        Ok(self
            .api
            .list("/firewalls", &[("name", name.clone())], "firewalls")
            .await?
            .into_iter()
            .find(|f| f["name"].as_str() == Some(name.as_str())))
    }

    fn selector_entry(&self) -> Value {
        json!({
            "type": "label_selector",
            "label_selector": { "selector": vm::firewall_selector(self.name()) },
        })
    }

    fn has_selector(&self, firewall: &Value) -> bool {
        let selector = vm::firewall_selector(self.name());
        firewall["applied_to"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|a| {
                a["type"] == "label_selector"
                    && a["label_selector"]["selector"].as_str() == Some(selector.as_str())
            })
    }

    fn has_rules(firewall: &Value) -> bool {
        firewall["rules"].as_array().is_some_and(|r| !r.is_empty())
    }

    fn firewall_id(firewall: &Value) -> Result<u64> {
        firewall["id"]
            .as_u64()
            .ok_or_else(|| Error::provider(format!("{RUNTIME}: firewall without id")))
    }

    /// The firewall exists, has no rules and is applied to the deployment's selector.
    async fn ensure_firewall(&self) -> Result<()> {
        let name = self.firewall_name();
        let Some(firewall) = self.find_firewall().await? else {
            let created = self
                .api
                .post(
                    "/firewalls",
                    &json!({
                        "name": name,
                        "labels": {
                            vm::LABEL_CIBURST: "true",
                            vm::LABEL_NAME: self.name(),
                        },
                        "apply_to": [self.selector_entry()],
                    }),
                )
                .await?;
            let id = created["firewall"]["id"].as_u64().unwrap_or_default();
            self.log.info(
                "created firewall",
                &[("name", &name), ("id", &id.to_string())],
            );
            return Ok(());
        };
        let id = Hetzner::firewall_id(&firewall)?;
        if Hetzner::has_rules(&firewall) {
            self.api
                .post(
                    &format!("/firewalls/{id}/actions/set_rules"),
                    &json!({ "rules": [] }),
                )
                .await?;
            self.log.info(
                "removed firewall rules",
                &[("name", &name), ("id", &id.to_string())],
            );
        }
        if !self.has_selector(&firewall) {
            self.api
                .post(
                    &format!("/firewalls/{id}/actions/apply_to_resources"),
                    &json!({ "apply_to": [self.selector_entry()] }),
                )
                .await?;
            self.log.info(
                "applied firewall",
                &[
                    ("name", &name),
                    ("id", &id.to_string()),
                    ("selector", &vm::firewall_selector(self.name())),
                ],
            );
        }
        Ok(())
    }

    /// Removes the firewall from its resources, then deletes it, waiting while the removal
    /// runs. `false` when there was none.
    async fn delete_firewall(&self) -> Result<bool> {
        let name = self.firewall_name();
        let Some(firewall) = self.find_firewall().await? else {
            return Ok(false);
        };
        let id = Hetzner::firewall_id(&firewall)?;
        let remove_from: Vec<Value> = firewall["applied_to"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|applied| match applied["type"].as_str()? {
                "label_selector" => Some(json!({
                    "type": "label_selector",
                    "label_selector": { "selector": applied["label_selector"]["selector"] },
                })),
                "server" => Some(json!({
                    "type": "server",
                    "server": { "id": applied["server"]["id"] },
                })),
                _ => None,
            })
            .collect();
        if !remove_from.is_empty() {
            let path = format!("/firewalls/{id}/actions/remove_from_resources");
            match self
                .api
                .send(
                    "POST",
                    &path,
                    &[],
                    Some(&json!({ "remove_from": remove_from })),
                )
                .await?
            {
                Ok(_) => {}
                Err(error) if error.code == "firewall_resource_not_found" => {}
                Err(error) => return Err(Api::error("POST", &path, &error)),
            }
        }
        let path = format!("/firewalls/{id}");
        let mut attempt = 1;
        loop {
            match self.api.send("DELETE", &path, &[], None).await? {
                Ok(_) => {
                    self.log.info(
                        "deleted firewall",
                        &[("name", &name), ("id", &id.to_string())],
                    );
                    return Ok(true);
                }
                Err(error) if error.code == "not_found" => return Ok(false),
                Err(error)
                    if attempt < FIREWALL_DELETE_ATTEMPTS
                        && matches!(
                            error.code.as_str(),
                            "resource_in_use" | "conflict" | "locked"
                        ) =>
                {
                    attempt += 1;
                    tokio::time::sleep(self.retry_delay).await;
                }
                Err(error) => return Err(Api::error("DELETE", &path, &error)),
            }
        }
    }

    /// Servers of this deployment matching `selector`, in every status.
    async fn servers(&self, selector: &str) -> Result<Vec<Value>> {
        Ok(self
            .api
            .list(
                "/servers",
                &[("label_selector", selector.to_string())],
                "servers",
            )
            .await?
            .into_iter()
            .filter(|s| vm::is_deployment(s, self.name()))
            .collect())
    }

    /// Servers of one pool; none for a tag no label can carry.
    async fn pool_servers(&self, tag: &str) -> Result<Vec<Value>> {
        if tag.is_empty() || !vm::valid_label_value(tag) {
            return Ok(Vec::new());
        }
        Ok(self
            .servers(&vm::pool_selector(self.name(), tag))
            .await?
            .into_iter()
            .filter(|s| vm::labels_of(s).get(vm::LABEL_POOL).map(String::as_str) == Some(tag))
            .collect())
    }

    /// The server a run id of this deployment names.
    async fn own_server(&self, run_id: &str) -> Result<Value> {
        let name = vm::server_name_of(run_id).ok_or_else(|| {
            Error::provider(format!("{RUNTIME}: {run_id} is not a {RUNTIME} run id"))
        })?;
        let server = self
            .api
            .list("/servers", &[("name", name.to_string())], "servers")
            .await?
            .into_iter()
            .find(|s| s["name"].as_str() == Some(name))
            .ok_or_else(|| Error::provider(format!("{RUNTIME}: server {name} not found")))?;
        if !vm::is_deployment(&server, self.name()) {
            return Err(Error::provider(format!(
                "{RUNTIME}: server {name} does not belong to {}",
                self.name()
            )));
        }
        Ok(server)
    }

    fn server_id(server: &Value) -> Result<u64> {
        server["id"]
            .as_u64()
            .ok_or_else(|| Error::provider(format!("{RUNTIME}: server without id")))
    }

    /// Replaces the server's labels; the API keeps no label that is not sent.
    async fn put_labels(&self, server: &Value, labels: &vm::Labels) -> Result<()> {
        let id = Hetzner::server_id(server)?;
        self.api
            .put(&format!("/servers/{id}"), &json!({ "labels": labels }))
            .await?;
        Ok(())
    }

    /// Deletes a server; `false` when it is already being deleted or gone.
    async fn delete_server(&self, server: &Value) -> Result<bool> {
        if server["status"] == "deleting" {
            return Ok(false);
        }
        let id = Hetzner::server_id(server)?;
        let path = format!("/servers/{id}");
        match self.api.send("DELETE", &path, &[], None).await? {
            Ok(_) => Ok(true),
            Err(error) if error.code == "not_found" => Ok(false),
            Err(error) => Err(Api::error("DELETE", &path, &error)),
        }
    }

    /// `vm.env` of a new VM without its run id and token, the VM secret, the agent's URL and
    /// SHA-256 and the runner's URL (D38, M6 rule R6, from the main provider's `shared_env`;
    /// the runner's SHA-256 is the pinned `cloud_init::RUNNER_SHA256`). Fails before anything
    /// is created when the request lacks a value, naming the key.
    fn base_env(&self, pool: &Pool, request: &BTreeMap<String, String>) -> Result<Base> {
        let required = |key: &str| {
            request
                .get(key)
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or_else(|| {
                    Error::provider(format!("{RUNTIME}: pool {}: {key} missing", pool.tag))
                })
        };
        let runner_token = required("RUNNER_TOKEN")?;
        let secret = required(keys::VM_SECRET)?;
        let dispatch_url = required(DISPATCH_URL)?;
        let agent_url = required(AGENT_URL_KEY)?;
        let agent_sha256 = required(AGENT_SHA256_KEY)?;
        let runner_url = required(RUNNER_URL_KEY)?;
        let mut env = BTreeMap::from([
            (DISPATCH_URL.to_string(), dispatch_url),
            ("CI_SERVER_URL".to_string(), self.config.gitlab.url.clone()),
            ("RUNNER_TOKEN".to_string(), runner_token),
            ("CIBURST_POOL".to_string(), pool.tag.clone()),
            ("CIBURST_SLOTS".to_string(), pool.slots.to_string()),
            ("CIBURST_DEFAULT_IMAGE".to_string(), pool.image.clone()),
            ("CIBURST_TIMEOUT_S".to_string(), pool.timeout_s.to_string()),
        ]);
        if self.config.cache.kind == CacheType::S3 {
            for key in CACHE_KEYS {
                env.insert(key.to_string(), required(key)?);
            }
            if env["CACHE_TYPE"] != "s3" {
                return Err(Error::provider(format!(
                    "{RUNTIME}: pool {}: CACHE_TYPE is {}, the VM agent takes s3",
                    pool.tag, env["CACHE_TYPE"]
                )));
            }
            if let Some(shared) = request.get(CACHE_SHARED).filter(|v| !v.is_empty()) {
                env.insert(CACHE_SHARED.to_string(), shared.clone());
            }
        }
        Ok(Base {
            env,
            secret,
            runner_url,
            agent_url,
            agent_sha256,
        })
    }

    /// Rule R12: the first server type with capacity runs the VM; the name is chosen first, so
    /// the user data carries the run id and its token.
    async fn start_vm(&self, request: StartRequest) -> Result<String> {
        let pool = &request.pool;
        let tag = &pool.tag;
        let index = self
            .config
            .pools
            .iter()
            .position(|p| p.tag == *tag)
            .unwrap_or_default();
        let settings = self.checked(&[(index, pool)])?;
        let Base {
            mut env,
            secret,
            runner_url,
            agent_url,
            agent_sha256,
        } = self.base_env(pool, &request.env)?;
        let not_provisioned =
            || Error::provider(format!("{RUNTIME}: pool {tag} is not provisioned"));
        let firewall = self.find_firewall().await?.ok_or_else(not_provisioned)?;
        if !self.has_selector(&firewall) {
            return Err(not_provisioned());
        }
        let name = vm::server_name(self.name(), tag);
        let run_id = vm::run_id(&name);
        env.insert(RUN_ID.to_string(), run_id.clone());
        env.insert(
            VM_TOKEN.to_string(),
            ciburst_core::vm::token(&secret, &run_id),
        );
        let user_data = ciburst_core::cloud_init::render(
            &env,
            Download {
                url: &runner_url,
                sha256: RUNNER_SHA256,
            },
            Download {
                url: &agent_url,
                sha256: &agent_sha256,
            },
            false,
        )?;
        if user_data.len() > MAX_USER_DATA {
            return Err(Error::provider(format!(
                "{RUNTIME}: pool {tag}: user data is {} bytes, the API takes at most {MAX_USER_DATA}",
                user_data.len()
            )));
        }
        let taken = request.taken.max(1);
        let job = request.labels.get("job").map(String::as_str);
        let labels = vm::initial_labels(self.name(), tag, job, pool.slots.saturating_sub(taken));
        let mut failures = Vec::new();
        for server_type in server_types(pool) {
            let body = json!({
                "name": name,
                "server_type": server_type,
                "image": settings.image,
                "location": settings.location,
                "start_after_create": true,
                "user_data": user_data,
                "labels": labels,
                "public_net": { "enable_ipv4": true, "enable_ipv6": true },
            });
            let answer: Answer = self.api.send("POST", "/servers", &[], Some(&body)).await?;
            match answer {
                Ok(created) => {
                    let id = created["server"]["id"].as_u64().unwrap_or_default();
                    self.log.info(
                        "started server",
                        &[
                            ("pool", tag),
                            ("name", &name),
                            ("id", &id.to_string()),
                            ("type", &server_type),
                            ("location", &settings.location),
                        ],
                    );
                    return Ok(run_id);
                }
                Err(error) if error.is_capacity() => {
                    self.unavailable(tag, &server_type, &error);
                    failures.push(format!("{server_type}: {error}"));
                }
                Err(error) => return Err(Api::error("POST", "/servers", &error)),
            }
        }
        Err(Error::provider(format!(
            "{RUNTIME}: pool {tag}: no server type started ({})",
            failures.join("; ")
        )))
    }

    fn unavailable(&self, pool: &str, server_type: &str, error: &ApiError) {
        self.log.warn(
            "server type unavailable",
            &[
                ("pool", pool),
                ("type", server_type),
                ("code", &error.code),
                ("error", &error.message),
            ],
        );
    }

    fn as_run(server: &Value) -> Option<Run> {
        let labels = vm::labels_of(server);
        Some(Run {
            run_id: vm::run_id(server["name"].as_str().filter(|n| !n.is_empty())?),
            pool: labels.get(vm::LABEL_POOL).cloned().unwrap_or_default(),
            state: vm::run_state(server["status"].as_str().unwrap_or_default()),
            created_at: server["created"].as_str().unwrap_or_default().to_string(),
            labels: labels
                .get(vm::LABEL_JOB)
                .map(|job| BTreeMap::from([("job".to_string(), job.clone())]))
                .unwrap_or_default(),
        })
    }
}

#[async_trait]
impl Provider for Hetzner {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            name: RUNTIME.to_string(),
            docker_daemon: true,
            max_vcpu: 16,
            max_memory_mib: 32_768,
            max_storage_mib: 327_680,
            max_duration_s: 86_400,
            archs: vec!["amd64".into()],
            billing: Billing::PerHour,
        }
    }

    /// D42 in two requests: the location (which proves the token) and the server types, each
    /// pool needing one that is offered there.
    async fn preflight(&self) -> Result<()> {
        let pools = self.pools();
        if pools.is_empty() {
            return Ok(());
        }
        let settings = self.checked(&pools)?;
        let location = &settings.location;
        let query = [("name", location.clone())];
        let locations = match self.api.send("GET", "/locations", &query, None).await? {
            Ok(found) => found,
            Err(error) if error.status == 401 => {
                return Err(Error::provider(format!(
                    "{RUNTIME}: the API refuses {TOKEN_VAR}: {error}"
                )));
            }
            Err(error) => return Err(Api::error("GET", "/locations", &error)),
        };
        let known = locations["locations"]
            .as_array()
            .is_some_and(|all| all.iter().any(|l| l["name"] == location.as_str()));
        if !known {
            return Err(Error::provider(format!(
                "{RUNTIME}: location {location} does not exist; set providers.{RUNTIME}.location"
            )));
        }
        let types = self.api.list("/server_types", &[], "server_types").await?;
        let offered = |name: &str| {
            types.iter().any(|t| {
                t["name"] == name
                    && t["deprecated"] != true
                    && t["prices"]
                        .as_array()
                        .is_some_and(|p| p.iter().any(|p| p["location"] == location.as_str()))
            })
        };
        let problems: Vec<String> = pools
            .iter()
            .filter_map(|(_, pool)| {
                let wanted = server_types(pool);
                (!wanted.iter().any(|name| offered(name))).then(|| {
                    format!(
                        "pool {}: none of {} is offered in {location}",
                        pool.tag,
                        wanted.join(", ")
                    )
                })
            })
            .collect();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::provider(format!(
                "{RUNTIME}: {}",
                problems.join("; ")
            )))
        }
    }

    async fn plan(&self, _rotate: &[Rotation]) -> Result<Vec<Step>> {
        let pools = self.pools();
        self.checked(&pools)?;
        let firewall = self.find_firewall().await?;
        let resource = format!("firewall {}", self.firewall_name());
        let step = |action| Step::new(FIREWALL_STEP, action, resource.clone());
        Ok(match (pools.is_empty(), firewall) {
            (false, None) => vec![step(Action::Create)],
            (false, Some(firewall)) => {
                let mut repairs = Vec::new();
                if Hetzner::has_rules(&firewall) {
                    repairs.push("remove rules");
                }
                if !self.has_selector(&firewall) {
                    repairs.push("apply to the label selector");
                }
                if repairs.is_empty() {
                    vec![step(Action::Noop)]
                } else {
                    vec![step(Action::Update).with_detail(repairs.join(", "))]
                }
            }
            (true, Some(_)) => vec![step(Action::Delete)],
            (true, None) => Vec::new(),
        })
    }

    async fn apply(&self, steps: &[Step]) -> Result<Outputs> {
        for step in steps {
            if step.is_noop() {
                continue;
            }
            if step.id != FIREWALL_STEP {
                return Err(Error::provider(format!(
                    "{RUNTIME}: unknown step {}",
                    step.id
                )));
            }
            if step.action == Action::Delete {
                self.delete_firewall().await?;
            } else {
                self.ensure_firewall().await?;
            }
        }
        Ok(BTreeMap::from([(
            TOKEN_VAR.to_string(),
            self.token.clone(),
        )]))
    }

    async fn destroy(&self) -> Result<()> {
        for server in self.servers(&vm::deployment_selector(self.name())).await? {
            if self.delete_server(&server).await? {
                let name = server["name"].as_str().unwrap_or_default();
                let id = server["id"].as_u64().unwrap_or_default().to_string();
                self.log
                    .info("deleted server", &[("name", name), ("id", &id)]);
            }
        }
        if let Err(error) = self.delete_firewall().await {
            self.log.warn(
                "firewall not deleted",
                &[
                    ("name", &self.firewall_name()),
                    ("error", &error.to_string()),
                ],
            );
        }
        Ok(())
    }

    async fn start(&self, request: StartRequest) -> Result<String> {
        if !is_mine(&request.pool) {
            return Err(Error::provider(format!(
                "{RUNTIME}: pool {} does not run on {RUNTIME}",
                request.pool.tag
            )));
        }
        self.start_vm(request).await
    }

    async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
        let servers = match pool {
            Some(tag) => self.pool_servers(tag).await?,
            None => self.servers(&vm::deployment_selector(self.name())).await?,
        };
        Ok(servers.iter().filter_map(Hetzner::as_run).collect())
    }

    /// Deletes the server the run id names; ids of other deployments and providers fail.
    async fn stop(&self, run_id: &str) -> Result<()> {
        let server = self.own_server(run_id).await?;
        if self.delete_server(&server).await? {
            let id = server["id"].as_u64().unwrap_or_default().to_string();
            self.log
                .info("deleted server", &[("run", run_id), ("id", &id)]);
        }
        Ok(())
    }

    async fn capacity(&self, pool: &str) -> Result<Option<Vec<VmCapacity>>> {
        Ok(Some(
            self.pool_servers(pool)
                .await?
                .iter()
                .filter(|s| vm::run_state(s["status"].as_str().unwrap_or_default()).is_live())
                .filter_map(vm::capacity)
                .collect(),
        ))
    }

    async fn claim(&self, run_id: &str) -> Result<()> {
        let server = self.own_server(run_id).await?;
        self.put_labels(&server, &vm::claimed(&vm::labels_of(&server)))
            .await
    }

    async fn report(&self, run_id: &str, report: &VmReport, now: u64) -> Result<()> {
        let server = self.own_server(run_id).await?;
        self.put_labels(&server, &vm::reported(&vm::labels_of(&server), report, now))
            .await
    }

    async fn sweep(&self, now: u64) -> Result<Vec<String>> {
        let mut swept = Vec::new();
        for server in self.servers(&vm::deployment_selector(self.name())).await? {
            let name = server["name"].as_str().unwrap_or_default();
            if server["status"] == "deleting" {
                continue;
            }
            let Some(capacity) = vm::capacity(&server) else {
                self.log.warn(
                    "sweep skipped",
                    &[("name", name), ("reason", "no creation time")],
                );
                continue;
            };
            let Some(reason) =
                ciburst_core::vm::sweep_reason(&capacity, vm::drain_since(&server), now)
            else {
                continue;
            };
            match self.delete_server(&server).await {
                Ok(true) => {
                    self.log.info(
                        "swept server",
                        &[("run", &capacity.run_id), ("reason", reason)],
                    );
                    swept.push(capacity.run_id);
                }
                Ok(false) => {}
                Err(error) => self.log.warn(
                    "sweep failed",
                    &[("run", &capacity.run_id), ("error", &error.to_string())],
                ),
            }
        }
        Ok(swept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciburst_core::testing::contract::contract_config;

    fn with_settings(settings: Value) -> Config {
        let mut config = (*contract_config()).clone();
        config.providers.insert(
            RUNTIME.to_string(),
            settings.as_object().cloned().unwrap_or_default(),
        );
        config
    }

    #[test]
    fn settings_default_and_name_invalid_keys() {
        let config = (*contract_config()).clone();
        assert_eq!(
            Settings::from_config(&config),
            Ok(Settings {
                location: "nbg1".into(),
                image: "docker-ce".into(),
            })
        );
        let config = with_settings(json!({ "location": "hel1", "image": "ubuntu-24.04" }));
        assert_eq!(
            Settings::from_config(&config),
            Ok(Settings {
                location: "hel1".into(),
                image: "ubuntu-24.04".into(),
            })
        );
        let config = with_settings(json!({ "location": "NBG 1", "image": 7, "zone": "x" }));
        assert_eq!(
            Settings::from_config(&config),
            Err(vec![
                "providers.hetzner.zone: unknown setting; known: location, image".to_string(),
                "providers.hetzner.location: a location name such as nbg1, fsn1 or hel1"
                    .to_string(),
                "providers.hetzner.image: an image name such as docker-ce, or an image id"
                    .to_string(),
            ])
        );
        let config = with_settings(json!({ "location": "", "image": "" }));
        assert_eq!(Settings::from_config(&config).unwrap_err().len(), 2);
    }

    #[test]
    fn default_types_apply_only_without_instance_types() {
        let mut pool = contract_config().pools[0].clone();
        assert_eq!(server_types(&pool), ["cx33", "cx43"]);
        pool.instance_types = vec!["cx23".into()];
        assert_eq!(server_types(&pool), ["cx23"]);
    }
}
