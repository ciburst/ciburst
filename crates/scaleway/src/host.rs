//! Host `scaleway-function`: the dispatcher as a Serverless Function (`functions/v1beta1`) built
//! by Scaleway from a zip embedded in this crate by `build.rs`.
//!
//! Steps: `namespace` (`<name>`), `function` (`<name>-dispatch`), `code` (the embedded zip),
//! `cron` (`<name>-sweep`, the sweep trigger of spec 6.1).
//!
//! The function environment is exactly what `ciburst_dispatch_scaleway::Env` reads, named in
//! [`vars`]:
//! - plain: `CIBURST_CONFIG` (config JSON), `CIBURST_ORGANIZATION_ID`, `CIBURST_PROJECT_ID`,
//!   `CIBURST_REGION` (Scaleway reserves the `SCW_` prefix), every variable of
//!   `Secrets::dispatcher_env` under its own name (`CIBURST_AGENT_SHA256`, D38), and
//!   `CIBURST_VM_SECURITY_GROUP` and `CIBURST_VM_IMAGES`, which
//!   this host resolves on every plan and apply (`Scaleway::resolve_vm_start`, spec
//!   6.6), so a changed image or security group plans a function update;
//! - secret: `HOOK_TOKEN`, `CIBURST_DISPATCH_KEY` (the dispatch key), `RUNNER_TOKEN_<TAG>` for
//!   the VM runner and each host lane, `CACHE_S3_ACCESS_KEY` and `CACHE_S3_SECRET_KEY` when a
//!   cache key exists, `CIBURST_VM_SECRET`, `CIBURST_SWEEP_TOKEN` and `CIBURST_FLEET_KEY` (spec
//!   8.1) when the provisioner made them.
//!
//! On top, the plain variable `CIBURST_CODE_DIGEST` records the sha256 of the deployed zip for
//! this host's plan; the function does not read it. The sha256 of the settings and both
//! environments is kept in the function's `description` as `ciburst:<hex>`.
//!
//! The cron posts `{"ciburst_sweep": "<sweep token>"}` to the function every 30 minutes. The API
//! returns a cron's `args` as stored, so the plan compares schedule and args directly.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::http::Request;
use ciburst_core::log::Log;
use ciburst_core::types::{Action, Host, Outputs, Secrets, Step};
use ciburst_core::vm::SWEEP_ARG;
use ciburst_core::{Error, Result};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::client::ScalewayClient;
use crate::provider::{LEGACY_JOBS_TYPE, PROVIDER_NAME, Scaleway};

pub const HOST_NAME: &str = "scaleway-function";

pub const RUNTIME: &str = "rust196";
pub const HANDLER: &str = "handle";
pub const PRIVACY: &str = "public";
pub const MIN_SCALE: u32 = 0;
pub const MAX_SCALE: u32 = 5;
/// Scaleway gives a function CPU in proportion to its memory: 1024 MB get 560 mvCPU, 128 MB
/// only 70 (D50).
pub const MEMORY_LIMIT_MB: u32 = 1024;
pub const TIMEOUT: &str = "30s";
pub const HTTP_OPTION: &str = "redirected";
pub const SANDBOX: &str = "v2";
pub const DESCRIPTION_PREFIX: &str = "ciburst:";
/// Spec 6.1: the sweep runs every 30 minutes (UTC, standard five-field cron syntax).
pub const SWEEP_SCHEDULE: &str = "*/30 * * * *";

const STEP_NAMESPACE: &str = "namespace";
const STEP_FUNCTION: &str = "function";
const STEP_CODE: &str = "code";
const STEP_CRON: &str = "cron";

const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// A Rust build on Scaleway compiles the whole dependency set; 20 minutes at most.
const POLL_ATTEMPTS: u32 = 240;

/// Names of the function's environment variables, shared with the function crate.
pub mod vars {
    pub const CONFIG: &str = "CIBURST_CONFIG";
    pub const ORGANIZATION_ID: &str = "CIBURST_ORGANIZATION_ID";
    pub const PROJECT_ID: &str = "CIBURST_PROJECT_ID";
    pub const REGION: &str = "CIBURST_REGION";
    pub const HOOK_TOKEN: &str = "HOOK_TOKEN";
    pub const SECRET_KEY: &str = "CIBURST_DISPATCH_KEY";
    pub const CACHE_ACCESS_KEY: &str = "CACHE_S3_ACCESS_KEY";
    pub const CACHE_SECRET_KEY: &str = "CACHE_S3_SECRET_KEY";
    pub const VM_SECRET: &str = "CIBURST_VM_SECRET";
    pub const SWEEP_TOKEN: &str = "CIBURST_SWEEP_TOKEN";
    /// Seals the fleet state in the bucket (spec 8.1).
    pub const FLEET_KEY: &str = "CIBURST_FLEET_KEY";
    pub const CODE_DIGEST: &str = "CIBURST_CODE_DIGEST";
    /// The agent VMs download (D38), from `Secrets::dispatcher_env`.
    pub const AGENT_SHA256: &str = ciburst_core::provision::keys::AGENT_SHA256;
    /// What a VM start uses without looking it up (spec 6.6).
    pub const VM_SECURITY_GROUP: &str = crate::provider::SECURITY_GROUP_VAR;
    pub const VM_IMAGES: &str = crate::provider::IMAGES_VAR;

    /// Whether `name` can carry a plain variable of the dispatcher (`Secrets::dispatcher_env`,
    /// for example `CIBURST_AGENT_SHA256`): upper-case letters, digits and `_`, not starting
    /// with a digit, outside the names of this module, `RUNNER_TOKEN_<TAG>` and Scaleway's
    /// reserved `SCW_`.
    pub fn dispatcher_name_ok(name: &str) -> bool {
        let shaped = name.starts_with(|c: char| c.is_ascii_uppercase())
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        let taken = [
            CONFIG,
            ORGANIZATION_ID,
            PROJECT_ID,
            REGION,
            HOOK_TOKEN,
            SECRET_KEY,
            CACHE_ACCESS_KEY,
            CACHE_SECRET_KEY,
            VM_SECRET,
            SWEEP_TOKEN,
            FLEET_KEY,
            CODE_DIGEST,
            VM_SECURITY_GROUP,
            VM_IMAGES,
        ]
        .contains(&name)
            || [RUNNER_TOKEN_PREFIX, "SCW_"]
                .iter()
                .any(|prefix| name.starts_with(prefix));
        shaped && !taken
    }

    const RUNNER_TOKEN_PREFIX: &str = "RUNNER_TOKEN_";

    /// A tag as a variable name suffix: upper-cased, every other character than ASCII letters
    /// and digits replaced by `_`, the rule of `ciburst_core::provision::keys`.
    fn tag_suffix(tag: &str) -> String {
        tag.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// `RUNNER_TOKEN_<TAG>`.
    pub fn runner_token(tag: &str) -> String {
        format!("{RUNNER_TOKEN_PREFIX}{}", tag_suffix(tag))
    }
}

#[cfg(ciburst_embedded_code)]
static CODE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/function.zip"));
#[cfg(not(ciburst_embedded_code))]
static CODE: &[u8] = &[];

/// The function's source zip, packed by `build.rs`. Empty in the vendored copy inside the zip.
pub fn embedded_code() -> &'static [u8] {
    CODE
}

/// Hex sha256 of [`embedded_code`].
pub fn embedded_code_digest() -> &'static str {
    static DIGEST: OnceLock<String> = OnceLock::new();
    DIGEST.get_or_init(|| hex::encode(Sha256::digest(CODE)))
}

/// Everything the `function` step compares. Holds secrets: never logged, never serialized
/// anywhere but into the digest and the API request.
#[derive(Serialize)]
struct Settings {
    runtime: &'static str,
    handler: &'static str,
    privacy: &'static str,
    min_scale: u32,
    max_scale: u32,
    memory_limit: u32,
    timeout: &'static str,
    http_option: &'static str,
    sandbox: &'static str,
    environment_variables: BTreeMap<String, String>,
    secret_environment_variables: BTreeMap<String, String>,
    /// `CIBURST_CONFIG` as the digest sees it: [`Config::deployment_view`], so a routing
    /// change alone leaves the function as it is (D58).
    #[serde(skip)]
    config_view: String,
}

impl Settings {
    fn description(&self) -> Result<String> {
        let mut compared = serde_json::to_value(self)?;
        compared["environment_variables"][vars::CONFIG] = Value::String(self.config_view.clone());
        let digest = Sha256::digest(serde_json::to_vec(&compared)?);
        Ok(format!("{DESCRIPTION_PREFIX}{}", hex::encode(digest)))
    }

    /// The create or update body; `code_digest` is carried in the plain environment.
    fn body(&self, code_digest: Option<&str>, remove_secrets: &[String]) -> Result<Value> {
        let mut secrets: Vec<Value> = self
            .secret_environment_variables
            .iter()
            .map(|(key, value)| json!({ "key": key, "value": value }))
            .collect();
        secrets.extend(remove_secrets.iter().map(|key| json!({ "key": key })));
        Ok(json!({
            "runtime": self.runtime,
            "handler": self.handler,
            "privacy": self.privacy,
            "min_scale": self.min_scale,
            "max_scale": self.max_scale,
            "memory_limit": self.memory_limit,
            "timeout": self.timeout,
            "http_option": self.http_option,
            "sandbox": self.sandbox,
            "description": self.description()?,
            "environment_variables": self.environment(code_digest),
            "secret_environment_variables": secrets,
        }))
    }

    fn environment(&self, code_digest: Option<&str>) -> BTreeMap<String, String> {
        let mut env = self.environment_variables.clone();
        if let Some(digest) = code_digest {
            env.insert(vars::CODE_DIGEST.to_string(), digest.to_string());
        }
        env
    }
}

/// The resources found by name.
struct Found {
    project: Option<String>,
    namespace: Option<Value>,
    function: Option<Value>,
}

pub struct ScalewayFunction {
    config: Arc<Config>,
    client: Arc<ScalewayClient>,
    log: Log,
}

impl ScalewayFunction {
    pub fn new(config: Arc<Config>, client: Arc<ScalewayClient>, log: Log) -> ScalewayFunction {
        ScalewayFunction {
            config,
            client,
            log,
        }
    }

    fn namespace_name(&self) -> &str {
        &self.config.name
    }

    fn function_name(&self) -> String {
        format!("{}-dispatch", self.config.name)
    }

    fn base(&self) -> String {
        format!("/functions/v1beta1/regions/{}", self.client.region())
    }

    /// `CIBURST_VM_SECURITY_GROUP` and `CIBURST_VM_IMAGES` for the VM pools of the `scaleway`
    /// provider; none for another provider type or without such pools.
    async fn vm_start_vars(&self, project_id: &str) -> Result<BTreeMap<String, String>> {
        if ![PROVIDER_NAME, LEGACY_JOBS_TYPE].contains(&self.config.provider.kind.as_str()) {
            return Ok(BTreeMap::new());
        }
        let provider = Scaleway::new(self.config.clone(), self.client.clone(), self.log.clone());
        Ok(provider
            .resolve_vm_start(project_id)
            .await?
            .map(|start| start.vars())
            .unwrap_or_default())
    }

    async fn settings(&self, project_id: &str, secrets: &Secrets) -> Result<Settings> {
        check_dispatcher_env(secrets)?;
        let mut environment_variables: BTreeMap<String, String> = secrets
            .dispatcher_env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        environment_variables.extend(self.vm_start_vars(project_id).await?);
        environment_variables.extend([
            (
                vars::CONFIG.to_string(),
                serde_json::to_string(&*self.config)?,
            ),
            (
                vars::ORGANIZATION_ID.to_string(),
                self.client.organization_id().to_string(),
            ),
            (vars::PROJECT_ID.to_string(), project_id.to_string()),
            (vars::REGION.to_string(), self.client.region().to_string()),
        ]);
        // Without a fleet key the dispatcher routes by this config, so all of it counts.
        let config_view = match &secrets.fleet_key {
            Some(_) => serde_json::to_string(&self.config.deployment_view())?,
            None => serde_json::to_string(&*self.config)?,
        };
        let mut secret = BTreeMap::new();
        if let Some(token) = &secrets.hook_token {
            secret.insert(vars::HOOK_TOKEN.to_string(), token.clone());
        }
        if let Some(dispatch) = &secrets.dispatch {
            secret.insert(vars::SECRET_KEY.to_string(), dispatch.secret_key.clone());
        }
        for tag in ciburst_core::provision::keys::runner_tags(&self.config) {
            if let Some(token) = secrets.runner_tokens.get(&tag) {
                secret.insert(vars::runner_token(&tag), token.clone());
            }
        }
        if let Some(cache) = &secrets.cache {
            secret.insert(vars::CACHE_ACCESS_KEY.to_string(), cache.access_key.clone());
            secret.insert(vars::CACHE_SECRET_KEY.to_string(), cache.secret_key.clone());
        }
        if let Some(vm_secret) = &secrets.vm_secret {
            secret.insert(vars::VM_SECRET.to_string(), vm_secret.clone());
        }
        if let Some(sweep_token) = &secrets.sweep_token {
            secret.insert(vars::SWEEP_TOKEN.to_string(), sweep_token.clone());
        }
        if let Some(fleet_key) = &secrets.fleet_key {
            secret.insert(vars::FLEET_KEY.to_string(), fleet_key.clone());
        }
        Ok(Settings {
            runtime: RUNTIME,
            handler: HANDLER,
            privacy: PRIVACY,
            min_scale: MIN_SCALE,
            max_scale: MAX_SCALE,
            memory_limit: MEMORY_LIMIT_MB,
            timeout: TIMEOUT,
            http_option: HTTP_OPTION,
            sandbox: SANDBOX,
            environment_variables,
            secret_environment_variables: secret,
            config_view,
        })
    }

    async fn find_namespace(&self, project_id: &str) -> Result<Option<Value>> {
        let name = self.namespace_name();
        let list = self
            .client
            .get(
                &format!("{}/namespaces", self.base()),
                &[
                    ("project_id", project_id.to_string()),
                    ("name", name.to_string()),
                ],
            )
            .await?;
        Ok(find_named(&list["namespaces"], name))
    }

    async fn find_function(&self, namespace_id: &str) -> Result<Option<Value>> {
        let name = self.function_name();
        let list = self
            .client
            .get(
                &format!("{}/functions", self.base()),
                &[
                    ("namespace_id", namespace_id.to_string()),
                    ("name", name.clone()),
                ],
            )
            .await?;
        Ok(find_named(&list["functions"], &name))
    }

    fn cron_name(&self) -> String {
        format!("{}-sweep", self.config.name)
    }

    async fn find_cron(&self, function_id: &str) -> Result<Option<Value>> {
        let list = self
            .client
            .get(
                &format!("{}/crons", self.base()),
                &[("function_id", function_id.to_string())],
            )
            .await?;
        Ok(find_named(&list["crons"], &self.cron_name()))
    }

    /// Creates the sweep cron or brings its schedule and args up to date, then waits for it.
    async fn ensure_cron(&self, function_id: &str, secrets: &Secrets) -> Result<()> {
        let args = sweep_args(secrets);
        let name = self.cron_name();
        let id = match self.find_cron(function_id).await? {
            None => {
                let created = self
                    .client
                    .post(
                        &format!("{}/crons", self.base()),
                        &json!({
                            "function_id": function_id,
                            "schedule": SWEEP_SCHEDULE,
                            "args": args,
                            "name": name,
                        }),
                    )
                    .await?;
                self.log.info("cron_created", &[("name", &name)]);
                id_of(&created)?.to_string()
            }
            Some(existing) if cron_differs(&existing, &args) => {
                let id = id_of(&existing)?.to_string();
                self.client
                    .patch(
                        &format!("{}/crons/{id}", self.base()),
                        &json!({ "schedule": SWEEP_SCHEDULE, "args": args }),
                    )
                    .await?;
                self.log.info("cron_updated", &[("name", &name)]);
                id
            }
            Some(_) => return Ok(()),
        };
        self.wait(&format!("{}/crons/{id}", self.base()), &["ready"])
            .await
            .map(drop)
    }

    async fn find(&self) -> Result<Found> {
        let mut found = Found {
            project: self.client.find_project(&self.config.name).await?,
            namespace: None,
            function: None,
        };
        if let Some(project) = &found.project {
            found.namespace = self.find_namespace(project).await?;
        }
        if let Some(namespace) = &found.namespace {
            found.function = self.find_function(id_of(namespace)?).await?;
        }
        Ok(found)
    }

    /// Polls `path` until its `status` is one of `done`; `error` fails unless listed.
    async fn wait(&self, path: &str, done: &[&str]) -> Result<Value> {
        for attempt in 0..POLL_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            let value = self.client.get(path, &[]).await?;
            let status = value["status"].as_str().unwrap_or("");
            if done.contains(&status) {
                return Ok(value);
            }
            if status == "error" {
                return Err(Error::provider(format!(
                    "{HOST_NAME}: {path} is in error: {}",
                    failure_message(&value)
                )));
            }
        }
        Err(Error::provider(format!(
            "{HOST_NAME}: {path} not {} after {} attempts",
            done.join(" or "),
            POLL_ATTEMPTS
        )))
    }

    /// Polls `path` until it answers 404.
    async fn wait_gone(&self, path: &str) -> Result<()> {
        for attempt in 0..POLL_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            match self.client.get(path, &[]).await {
                Ok(_) => {}
                Err(error) if is_not_found(&error) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Err(Error::provider(format!(
            "{HOST_NAME}: {path} still present after {POLL_ATTEMPTS} attempts"
        )))
    }

    async fn ensure_namespace(&self, project_id: &str) -> Result<Value> {
        let namespace = match self.find_namespace(project_id).await? {
            Some(namespace) => namespace,
            None => {
                let created = self
                    .client
                    .post(
                        &format!("{}/namespaces", self.base()),
                        &json!({
                            "name": self.namespace_name(),
                            "project_id": project_id,
                            "environment_variables": {},
                            "secret_environment_variables": [],
                        }),
                    )
                    .await?;
                self.log
                    .info("namespace_created", &[("name", self.namespace_name())]);
                created
            }
        };
        let path = format!("{}/namespaces/{}", self.base(), id_of(&namespace)?);
        self.wait(&path, &["ready"]).await
    }

    async fn upload(&self, function_id: &str) -> Result<()> {
        let code = embedded_code();
        if code.is_empty() {
            return Err(Error::provider(format!(
                "{HOST_NAME}: this build carries no function code"
            )));
        }
        let target = self
            .client
            .get(
                &format!("{}/functions/{function_id}/upload-url", self.base()),
                &[("content_length", code.len().to_string())],
            )
            .await?;
        let url = target["url"]
            .as_str()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| Error::provider(format!("{HOST_NAME}: upload-url without url")))?;
        let mut request = Request::new("PUT", url);
        if let Some(headers) = target["headers"].as_object() {
            for (name, values) in headers {
                // The HTTP client sets the length from the body itself.
                if name.eq_ignore_ascii_case("content-length") {
                    continue;
                }
                for value in header_values(values) {
                    request = request.header(name, value);
                }
            }
        }
        request.body = Some(code.to_vec());
        let response = self.client.http().send(request).await?;
        if !response.ok() {
            return Err(Error::provider(format!(
                "{HOST_NAME}: code upload: {}",
                response.status
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl Host for ScalewayFunction {
    async fn plan(&self, secrets: &Secrets) -> Result<Vec<Step>> {
        check_dispatcher_env(secrets)?;
        let found = self.find().await?;
        let function_name = self.function_name();
        let namespace = Step::new(
            STEP_NAMESPACE,
            if found.namespace.is_some() {
                Action::Noop
            } else {
                Action::Create
            },
            format!("function namespace {}", self.namespace_name()),
        );
        let mut function = Step::new(
            STEP_FUNCTION,
            Action::Create,
            format!("function {function_name}"),
        );
        let mut code = Step::new(
            STEP_CODE,
            Action::Create,
            format!("function code {function_name}"),
        )
        .with_detail(format!("zip sha256 {}", embedded_code_digest()));
        let mut cron = Step::new(
            STEP_CRON,
            Action::Create,
            format!("function cron {}", self.cron_name()),
        )
        .with_detail(format!("sweep {SWEEP_SCHEDULE}"));
        if let Some(existing) = &found.function {
            cron.action = match self.find_cron(id_of(existing)?).await? {
                None => Action::Create,
                Some(current) if cron_differs(&current, &sweep_args(secrets)) => {
                    cron.detail = Some("schedule or arguments changed".into());
                    Action::Update
                }
                Some(_) => Action::Noop,
            };
        }
        if let (Some(project), Some(existing)) = (&found.project, &found.function) {
            let wanted = self.settings(project, secrets).await?.description()?;
            if existing["description"].as_str() == Some(wanted.as_str()) {
                function.action = Action::Noop;
            } else {
                function.action = Action::Update;
                function.detail = Some("settings, config or secrets changed".into());
            }
            let deployed = existing["environment_variables"][vars::CODE_DIGEST].as_str();
            let status = existing["status"].as_str().unwrap_or("");
            code.action = match deployed {
                _ if status == "created" => Action::Create,
                None => Action::Create,
                Some(_) if status == "error" => {
                    code.detail = Some("last deploy failed".into());
                    Action::Update
                }
                Some(digest) if digest != embedded_code_digest() => Action::Update,
                Some(_) => Action::Noop,
            };
        }
        Ok(vec![namespace, function, code, cron])
    }

    async fn apply(&self, steps: &[Step], secrets: &Secrets) -> Result<Outputs> {
        check_dispatcher_env(secrets)?;
        if let Some(step) = steps.iter().find(|s| {
            ![STEP_NAMESPACE, STEP_FUNCTION, STEP_CODE, STEP_CRON].contains(&s.id.as_str())
        }) {
            return Err(Error::provider(format!(
                "{HOST_NAME}: unknown step {}",
                step.id
            )));
        }
        let action = |id: &str| {
            steps
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.action)
                .unwrap_or(Action::Noop)
        };
        let project = self
            .client
            .find_project(&self.config.name)
            .await?
            .ok_or_else(|| {
                Error::provider(format!(
                    "{HOST_NAME}: project {} not found, the provider creates it",
                    self.config.name
                ))
            })?;
        let namespace = self.ensure_namespace(&project).await?;
        let namespace_id = id_of(&namespace)?.to_string();
        let settings = self.settings(&project, secrets).await?;
        let function_name = self.function_name();

        let mut changed = false;
        let function = match self.find_function(&namespace_id).await? {
            None => {
                let mut body = settings.body(None, &[])?;
                body["name"] = json!(function_name);
                body["namespace_id"] = json!(namespace_id);
                let created = self
                    .client
                    .post(&format!("{}/functions", self.base()), &body)
                    .await?;
                self.log
                    .info("function_created", &[("name", &function_name)]);
                changed = true;
                created
            }
            Some(existing) if action(STEP_FUNCTION) == Action::Update => {
                let code_digest = existing["environment_variables"][vars::CODE_DIGEST].as_str();
                let remove: Vec<String> = secret_keys(&existing)
                    .into_iter()
                    .filter(|key| !settings.secret_environment_variables.contains_key(key))
                    .collect();
                let body = settings.body(code_digest, &remove)?;
                let updated = self
                    .client
                    .patch(
                        &format!("{}/functions/{}", self.base(), id_of(&existing)?),
                        &body,
                    )
                    .await?;
                self.log
                    .info("function_updated", &[("name", &function_name)]);
                changed = true;
                updated
            }
            Some(existing) => existing,
        };
        let function_id = id_of(&function)?.to_string();
        let path = format!("{}/functions/{function_id}", self.base());
        let stable = ["ready", "created", "error"];
        let mut current = self.wait(&path, &stable).await?;

        // A function in `created` has never received code, whatever the plan said.
        if action(STEP_CODE) != Action::Noop || current["status"] == "created" {
            self.upload(&function_id).await?;
            self.client
                .patch(
                    &path,
                    &json!({
                        "environment_variables":
                            settings.environment(Some(embedded_code_digest())),
                    }),
                )
                .await?;
            self.log.info(
                "function_code_uploaded",
                &[("name", &function_name), ("sha256", embedded_code_digest())],
            );
            current = self.wait(&path, &stable).await?;
            changed = true;
        }
        if changed {
            self.client
                .post(&format!("{path}/deploy"), &json!({}))
                .await?;
            current = self.wait(&path, &["ready"]).await?;
            self.log
                .info("function_deployed", &[("name", &function_name)]);
        }
        self.ensure_cron(&function_id, secrets).await?;
        let url = function_url(&current).ok_or_else(|| {
            Error::provider(format!("{HOST_NAME}: {function_name} has no domain name"))
        })?;
        Ok(Outputs::from([(
            ciburst_core::provision::keys::DISPATCH_URL.to_string(),
            url,
        )]))
    }

    async fn destroy(&self) -> Result<()> {
        let found = self.find().await?;
        if let Some(function) = &found.function {
            let function_id = id_of(function)?;
            if let Some(cron) = self.find_cron(function_id).await? {
                let path = format!("{}/crons/{}", self.base(), id_of(&cron)?);
                tolerate_not_found(self.client.delete(&path).await)?;
                self.log
                    .info("cron_deleted", &[("name", &self.cron_name())]);
            }
            let path = format!("{}/functions/{function_id}", self.base());
            tolerate_not_found(self.client.delete(&path).await)?;
            self.log
                .info("function_deleted", &[("name", &self.function_name())]);
        }
        if let Some(namespace) = &found.namespace {
            let path = format!("{}/namespaces/{}", self.base(), id_of(namespace)?);
            tolerate_not_found(self.client.delete(&path).await)?;
            self.wait_gone(&path).await?;
            self.log
                .info("namespace_deleted", &[("name", self.namespace_name())]);
        }
        Ok(())
    }

    async fn url(&self) -> Result<Option<String>> {
        Ok(self.find().await?.function.as_ref().and_then(function_url))
    }
}

/// Refuses a plain dispatcher variable that would shadow one of the function's own variables,
/// before anything is read or written.
fn check_dispatcher_env(secrets: &Secrets) -> Result<()> {
    match secrets
        .dispatcher_env
        .keys()
        .find(|name| !vars::dispatcher_name_ok(name))
    {
        Some(name) => Err(Error::provider(format!(
            "{HOST_NAME}: dispatcher variable {name} is reserved or not an upper-case name"
        ))),
        None => Ok(()),
    }
}

/// The cron's arguments, which the function receives as its JSON body.
fn sweep_args(secrets: &Secrets) -> Value {
    json!({ SWEEP_ARG: secrets.sweep_token.as_deref().unwrap_or("") })
}

fn cron_differs(cron: &Value, args: &Value) -> bool {
    cron["schedule"].as_str() != Some(SWEEP_SCHEDULE) || cron["args"] != *args
}

fn find_named(list: &Value, name: &str) -> Option<Value> {
    list.as_array()?
        .iter()
        .find(|item| item["name"].as_str() == Some(name))
        .cloned()
}

fn id_of(value: &Value) -> Result<&str> {
    value["id"]
        .as_str()
        .ok_or_else(|| Error::provider(format!("{HOST_NAME}: resource without id")))
}

fn function_url(function: &Value) -> Option<String> {
    function["domain_name"]
        .as_str()
        .filter(|d| !d.is_empty())
        .map(|d| format!("https://{d}/"))
}

fn secret_keys(function: &Value) -> BTreeSet<String> {
    function["secret_environment_variables"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["key"].as_str().map(String::from))
        .collect()
}

/// `UploadURL.headers` is a map of header names to lists of values.
fn header_values(values: &Value) -> Vec<String> {
    match values {
        Value::Array(list) => list
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Value::String(value) => vec![value.clone()],
        _ => Vec::new(),
    }
}

fn failure_message(value: &Value) -> String {
    let mut parts: Map<String, Value> = Map::new();
    for key in ["error_message", "build_message"] {
        if let Some(text) = value[key].as_str().filter(|t| !t.is_empty()) {
            parts.insert(key.to_string(), Value::String(text.to_string()));
        }
    }
    if parts.is_empty() {
        "no message".to_string()
    } else {
        Value::Object(parts).to_string()
    }
}

fn is_not_found(error: &Error) -> bool {
    matches!(error, Error::Provider(message) if message.contains(": 404 "))
}

fn tolerate_not_found(result: Result<Value>) -> Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(error) if is_not_found(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_token_names_follow_the_outputs_rule() {
        assert_eq!(vars::runner_token("ci"), "RUNNER_TOKEN_CI");
        assert_eq!(vars::runner_token("rust-1.96"), "RUNNER_TOKEN_RUST_1_96");
        assert_eq!(
            ciburst_core::provision::keys::runner_token("rust-1.96"),
            format!("CIBURST_{}", vars::runner_token("rust-1.96"))
        );
    }

    #[test]
    fn dispatcher_variables_keep_out_of_the_functions_own_names() {
        for name in ["CIBURST_AGENT_SHA256_X", "A", "X1_Y"] {
            assert!(vars::dispatcher_name_ok(name), "{name}");
        }
        for name in [
            "",
            "lower_case",
            "1TOKEN",
            "_TOKEN",
            "WITH-DASH",
            "SCW_SECRET_KEY",
            vars::HOOK_TOKEN,
            vars::CACHE_ACCESS_KEY,
            vars::CACHE_SECRET_KEY,
            vars::SECRET_KEY,
            vars::CONFIG,
            vars::CODE_DIGEST,
            &vars::runner_token("ci"),
        ] {
            assert!(!vars::dispatcher_name_ok(name), "{name}");
        }
    }

    /// D50: a function deployed with 128 MB carries another description, so it plans
    /// `update  function <name>  (settings, config or secrets changed)`.
    #[test]
    fn the_description_covers_the_memory_limit() {
        let settings = |memory_limit| Settings {
            runtime: RUNTIME,
            handler: HANDLER,
            privacy: PRIVACY,
            min_scale: MIN_SCALE,
            max_scale: MAX_SCALE,
            memory_limit,
            timeout: TIMEOUT,
            http_option: HTTP_OPTION,
            sandbox: SANDBOX,
            environment_variables: BTreeMap::new(),
            secret_environment_variables: BTreeMap::new(),
            config_view: String::new(),
        };
        assert_eq!(MEMORY_LIMIT_MB, 1024);
        assert_ne!(
            settings(128).description().unwrap(),
            settings(MEMORY_LIMIT_MB).description().unwrap()
        );
    }

    #[test]
    fn not_found_is_read_from_the_client_error() {
        assert!(is_not_found(&Error::provider(
            "scaleway DELETE /x: 404 not found"
        )));
        assert!(!is_not_found(&Error::provider(
            "scaleway DELETE /x: 403 denied"
        )));
        assert!(tolerate_not_found(Err(Error::provider("scaleway GET /x: 404 gone"))).is_ok());
    }

    #[test]
    fn upload_headers_accept_lists_and_strings() {
        assert_eq!(header_values(&json!(["a", "b"])), ["a", "b"]);
        assert_eq!(header_values(&json!("c")), ["c"]);
        assert!(header_values(&json!(null)).is_empty());
    }
}
