//! The `ciburst` command. `run` is the whole program so tests can drive it in process.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ciburst_core::composite::Composite;
use ciburst_core::config::{
    Config, check_capabilities, load_config, merge_config, read_config_file,
};
use ciburst_core::fake::{FakeHost, FakeProvider};
use ciburst_core::gitlab::Gitlab;
use ciburst_core::http::{HttpClient, ReqwestClient};
use ciburst_core::log::Log;
use ciburst_core::plan::{
    FileOutputs, GitlabOutputs, MemoryOutputs, OUTPUTS_SCOPE, OutputsStore, format_plan,
    read_env_file, summarize_plan,
};
use ciburst_core::provision::{self, ProjectReport, Provisioner, keys};
use ciburst_core::store::{FileStore, MemoryStore, Store};
use ciburst_core::types::{Host, Provider, Rotation};
use ciburst_core::{Error, Result};
use ciburst_hetzner::Hetzner;
use ciburst_scaleway::client::{Credentials, DEFAULT_REGION, ScalewayClient};
use ciburst_scaleway::host::{HOST_NAME as SCALEWAY_HOST, ScalewayFunction};
use ciburst_scaleway::provider::Scaleway;
use clap::{Parser, Subcommand};
use serde_json::Value;

pub const PROVIDERS: &[&str] = &["fake", "scaleway", "scaleway-jobs"];
pub const HOSTS: &[&str] = &["fake", SCALEWAY_HOST];

/// File name of the VM agent binary, Linux musl (spec D38).
pub const AGENT_FILE: &str = "ciburst-vm-agent";
/// Variable that names the VM agent binary.
pub const AGENT_VAR: &str = "CIBURST_AGENT";

/// The inputs of pipeline mode (M5 rule R1, spec D39); nothing else is read from the job.
pub mod ci_vars {
    pub const SCW_ACCESS_KEY: &str = "CIBURST_SCW_ACCESS_KEY";
    pub const SCW_SECRET_KEY: &str = "CIBURST_SCW_SECRET_KEY";
    pub const SCW_ORGANIZATION_ID: &str = "CIBURST_SCW_ORGANIZATION_ID";
    pub const SCW_PROJECT_ID: &str = "CIBURST_SCW_PROJECT_ID";
    pub const GITLAB_TOKEN: &str = "CIBURST_GITLAB_TOKEN";
    pub const HCLOUD_TOKEN: &str = "CIBURST_HCLOUD_TOKEN";
    pub const PROJECT_ID: &str = "CI_PROJECT_ID";
    pub const SERVER_URL: &str = "CI_SERVER_URL";
    pub const REF_PROTECTED: &str = "CI_COMMIT_REF_PROTECTED";
}

#[derive(Parser)]
#[command(
    name = "ciburst",
    version,
    about = "GitLab runners that exist only while jobs run"
)]
struct Cli {
    /// Config file [default: config.json; with --ci ciburst.json if present, merged over the
    /// pipeline defaults]
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Directory with scaleway.env, gitlab.env, hetzner.env, outputs.env and state.json
    #[arg(long, default_value = ".local", global = true)]
    local: PathBuf,
    /// Plan or apply without touching GitLab
    #[arg(long, global = true)]
    skip_gitlab: bool,
    /// Pipeline mode: inputs from CIBURST_* variables, outputs in project variables of
    /// CI_PROJECT_ID; runs only on a protected ref
    #[arg(long, global = true)]
    ci: bool,
    /// VM agent binary (Linux musl) to upload [env: CIBURST_AGENT; default:
    /// <local>/ciburst-vm-agent, then ciburst-vm-agent next to ciburst]
    #[arg(long, global = true)]
    agent: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show what apply would change
    Plan,
    /// Create or update every resource, store the outputs and replay waiting jobs
    Apply,
    /// Remove every resource of the configured name
    Destroy {
        /// The config name, as confirmation
        #[arg(long)]
        yes: Option<String>,
    },
    /// Live runs per pool
    Status,
    /// Compare every project's pipeline with the stage map
    Check {
        /// Branch, tag or commit to lint instead of the default branch
        #[arg(long = "ref")]
        content_ref: Option<String>,
    },
    /// Replace a secret and redeploy: runner, hook, key, cache or vm
    Rotate { kind: String },
}

/// What a command reads besides its arguments; tests inject every part.
pub struct Env {
    /// Environment variables.
    pub vars: BTreeMap<String, String>,
    /// Client of every API call; `None` builds a reqwest client.
    pub http: Option<Arc<dyn HttpClient>>,
    /// The running executable; the VM agent may sit next to it.
    pub exe: Option<PathBuf>,
    pub log: Log,
}

impl Env {
    pub fn process() -> Env {
        Env {
            vars: std::env::vars_os()
                .filter_map(|(key, value)| {
                    Some((key.into_string().ok()?, value.into_string().ok()?))
                })
                .collect(),
            http: None,
            exe: std::env::current_exe().ok(),
            log: Log::stderr(),
        }
    }
}

enum Mode {
    /// The developer path: secrets, state and outputs in this directory.
    Local(PathBuf),
    /// Pipeline mode in the setup's own project.
    Ci { project_id: u64 },
}

/// Modules selected by the config, built when a command needs them.
struct Modules {
    config: Arc<Config>,
    mode: Mode,
    vars: BTreeMap<String, String>,
    http: Option<Arc<dyn HttpClient>>,
    exe: Option<PathBuf>,
    agent: Option<PathBuf>,
    log: Log,
    scaleway: Option<Arc<ScalewayClient>>,
}

impl Modules {
    fn http(&mut self) -> Result<Arc<dyn HttpClient>> {
        if let Some(http) = &self.http {
            return Ok(http.clone());
        }
        let http: Arc<dyn HttpClient> = Arc::new(ReqwestClient::new()?);
        self.http = Some(http.clone());
        Ok(http)
    }

    fn input(&self, key: &str) -> Result<String> {
        self.vars
            .get(key)
            .filter(|value| !value.is_empty())
            .cloned()
            .ok_or_else(|| {
                let workstation = workstation_name(key).and_then(|name| {
                    [name.to_string(), format!("CIBURST_{name}")]
                        .into_iter()
                        .find(|found| self.vars.get(found).is_some_and(|v| !v.is_empty()))
                });
                Error::other(match workstation {
                    Some(name) => format!(
                        "{key} missing; the job has {name}, but pipeline mode reads only CIBURST_* variables: rename it to {key}"
                    ),
                    None => format!("{key} missing"),
                })
            })
    }

    async fn scaleway_credentials(&self) -> Result<Credentials> {
        let env = match &self.mode {
            Mode::Local(local) => {
                env_file(
                    &local.join("scaleway.env"),
                    &["SCW_SECRET_KEY", "SCW_DEFAULT_ORGANIZATION_ID"],
                )
                .await?
            }
            Mode::Ci { .. } => {
                let mut env = BTreeMap::new();
                for (input, key) in [
                    (ci_vars::SCW_ACCESS_KEY, "SCW_ACCESS_KEY"),
                    (ci_vars::SCW_SECRET_KEY, "SCW_SECRET_KEY"),
                    (ci_vars::SCW_ORGANIZATION_ID, "SCW_DEFAULT_ORGANIZATION_ID"),
                ] {
                    env.insert(key.to_string(), self.input(input)?);
                }
                if let Ok(project) = self.input(ci_vars::SCW_PROJECT_ID) {
                    env.insert("SCW_DEFAULT_PROJECT_ID".to_string(), project);
                }
                env
            }
        };
        Credentials::from_env(&env)
    }

    async fn scaleway(&mut self) -> Result<Arc<ScalewayClient>> {
        if let Some(client) = &self.scaleway {
            return Ok(client.clone());
        }
        let credentials = self.scaleway_credentials().await?;
        let region = self
            .config
            .provider
            .extra
            .get("region")
            .or_else(|| self.config.host.extra.get("region"))
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_REGION)
            .to_string();
        let client = Arc::new(ScalewayClient::new(self.http()?, credentials, &region));
        self.scaleway = Some(client.clone());
        Ok(client)
    }

    async fn gitlab(&mut self) -> Result<Arc<Gitlab>> {
        let token = match &self.mode {
            Mode::Ci { .. } => self.input(ci_vars::GITLAB_TOKEN)?,
            Mode::Local(local) => match self.input("GITLAB_TOKEN") {
                Ok(token) => token,
                Err(_) => env_file(&local.join("gitlab.env"), &["GITLAB_TOKEN"])
                    .await?
                    .remove("GITLAB_TOKEN")
                    .unwrap_or_default(),
            },
        };
        let http = self.http()?;
        Ok(Arc::new(Gitlab::new(&self.config.gitlab.url, token, http)))
    }

    async fn hetzner_token(&self) -> Result<String> {
        match &self.mode {
            Mode::Ci { .. } => self.input(ci_vars::HCLOUD_TOKEN),
            Mode::Local(local) => Ok(env_file(
                &local.join("hetzner.env"),
                &[ciburst_hetzner::TOKEN_VAR],
            )
            .await?
            .remove(ciburst_hetzner::TOKEN_VAR)
            .unwrap_or_default()),
        }
    }

    fn store(&self, slot: &str) -> Arc<dyn Store> {
        match &self.mode {
            Mode::Local(local) => Arc::new(FileStore::new(local.join("state.json"), slot)),
            Mode::Ci { .. } => Arc::new(MemoryStore::new()),
        }
    }

    fn agent(&self) -> Result<Option<PathBuf>> {
        let local = match &self.mode {
            Mode::Local(local) => Some(local.as_path()),
            Mode::Ci { .. } => None,
        };
        agent_path(
            self.agent.as_deref(),
            &self.vars,
            local,
            self.exe.as_deref(),
        )
    }

    /// The config's provider, wrapped in a [`Composite`] with the added providers of
    /// `providers` (spec D35).
    async fn provider(&mut self) -> Result<Arc<dyn Provider>> {
        let main = self.main_provider().await?;
        let mut extras: Vec<(String, Arc<dyn Provider>)> = Vec::new();
        let config = self.config.clone();
        for runtime in config.providers.keys() {
            match runtime.as_str() {
                ciburst_hetzner::RUNTIME => {
                    let token = self.hetzner_token().await?;
                    extras.push((
                        runtime.clone(),
                        Arc::new(Hetzner::new(
                            self.config.clone(),
                            token,
                            self.http()?,
                            self.log.clone(),
                        )),
                    ));
                }
                other => {
                    return Err(Error::other(format!(
                        "providers.{other}: no provider named {other}; available: {}",
                        ciburst_hetzner::RUNTIME
                    )));
                }
            }
        }
        if extras.is_empty() {
            return Ok(main);
        }
        Ok(Arc::new(Composite::new(
            self.config.clone(),
            main,
            extras,
            self.log.clone(),
        )))
    }

    async fn main_provider(&mut self) -> Result<Arc<dyn Provider>> {
        match self.config.provider.kind.as_str() {
            "fake" => Ok(Arc::new(FakeProvider::new(
                self.config.clone(),
                self.store("provider"),
            ))),
            "scaleway" | "scaleway-jobs" => {
                let agent = self.agent()?;
                let client = self.scaleway().await?;
                Ok(Arc::new(
                    Scaleway::new(self.config.clone(), client, self.log.clone()).with_agent(agent),
                ))
            }
            other => Err(Error::other(format!(
                "no provider named {other}; available: {}",
                PROVIDERS.join(", ")
            ))),
        }
    }

    async fn host(&mut self) -> Result<Arc<dyn Host>> {
        match self.config.host.kind.as_str() {
            "fake" => Ok(Arc::new(FakeHost::new(
                self.config.clone(),
                self.store("host"),
            ))),
            SCALEWAY_HOST => {
                let client = self.scaleway().await?;
                Ok(Arc::new(ScalewayFunction::new(
                    self.config.clone(),
                    client,
                    self.log.clone(),
                )))
            }
            other => Err(Error::other(format!(
                "no host named {other}; available: {}",
                HOSTS.join(", ")
            ))),
        }
    }
}

/// The values of an env file; a missing file or a missing required variable is an error that
/// names it.
async fn env_file(path: &Path, required: &[&str]) -> Result<BTreeMap<String, String>> {
    if !tokio::fs::try_exists(path).await? {
        return Err(Error::other(format!(
            "{} not found; it holds {}",
            path.display(),
            required.join(", ")
        )));
    }
    let values = read_env_file(path).await?;
    if let Some(key) = required
        .iter()
        .find(|key| values.get(**key).is_none_or(|value| value.is_empty()))
    {
        return Err(Error::other(format!("{key} missing in {}", path.display())));
    }
    Ok(values)
}

/// The VM agent binary to upload: `--agent`, else `CIBURST_AGENT`, else
/// `<local>/ciburst-vm-agent` when it exists, else `ciburst-vm-agent` next to the executable
/// when it exists, else none. A path given by flag or variable must exist.
pub fn agent_path(
    flag: Option<&Path>,
    vars: &BTreeMap<String, String>,
    local: Option<&Path>,
    exe: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let given = flag.map(Path::to_path_buf).or_else(|| {
        vars.get(AGENT_VAR)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    if let Some(path) = given {
        if path.is_file() {
            return Ok(Some(path));
        }
        return Err(Error::other(format!(
            "agent binary {} not found",
            path.display()
        )));
    }
    Ok(local
        .map(|dir| dir.join(AGENT_FILE))
        .into_iter()
        .chain(exe.and_then(Path::parent).map(|dir| dir.join(AGENT_FILE)))
        .find(|path| path.is_file()))
}

/// The workstation name of a pipeline input, which an adopter may have copied by mistake.
fn workstation_name(key: &str) -> Option<&'static str> {
    Some(match key {
        "CIBURST_SCW_ACCESS_KEY" => "SCW_ACCESS_KEY",
        "CIBURST_SCW_SECRET_KEY" => "SCW_SECRET_KEY",
        "CIBURST_SCW_ORGANIZATION_ID" => "SCW_DEFAULT_ORGANIZATION_ID",
        "CIBURST_SCW_PROJECT_ID" => "SCW_DEFAULT_PROJECT_ID",
        "CIBURST_GITLAB_TOKEN" => "GITLAB_TOKEN",
        "CIBURST_HCLOUD_TOKEN" => "HCLOUD_TOKEN",
        _ => return None,
    })
}

/// The project and GitLab URL of a `--ci` run; refuses unless the ref is protected, because
/// the protected inputs would be missing.
fn ci_project(vars: &BTreeMap<String, String>) -> Result<(u64, String)> {
    let get = |key: &str| {
        vars.get(key)
            .filter(|value| !value.is_empty())
            .cloned()
            .ok_or_else(|| Error::other(format!("{key} missing")))
    };
    let project = get(ci_vars::PROJECT_ID)?;
    let project_id = project.parse::<u64>().map_err(|_| {
        Error::other(format!(
            "{}: {project} is no project id",
            ci_vars::PROJECT_ID
        ))
    })?;
    let server_url = get(ci_vars::SERVER_URL)?;
    if vars.get(ci_vars::REF_PROTECTED).map(String::as_str) != Some("true") {
        return Err(Error::other(format!(
            "{} is not true: run ciburst-setup on a protected branch",
            ci_vars::REF_PROTECTED
        )));
    }
    Ok((project_id, server_url))
}

pub async fn run(args: Vec<String>, out: &mut dyn Write) -> Result<i32> {
    run_with_env(args, Env::process(), out).await
}

pub async fn run_with_env(args: Vec<String>, env: Env, out: &mut dyn Write) -> Result<i32> {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let text = error.to_string();
            if error.use_stderr() {
                eprint!("{text}");
                return Ok(2);
            }
            write!(out, "{text}")?;
            return Ok(0);
        }
    };

    let (mode, config) = if cli.ci {
        if cli.skip_gitlab {
            return Err(Error::other(
                "--skip-gitlab does not combine with --ci: the outputs live in project variables",
            ));
        }
        let (project_id, server_url) = ci_project(&env.vars)?;
        let path = cli
            .config
            .clone()
            .unwrap_or_else(|| PathBuf::from("ciburst.json"));
        let overlay = if tokio::fs::try_exists(&path).await? {
            read_config_file(&path).await?
        } else {
            "{}".to_string()
        };
        let config = merge_config(&Config::ci_defaults(project_id, &server_url), &overlay)?;
        (Mode::Ci { project_id }, config)
    } else {
        let path = cli
            .config
            .clone()
            .unwrap_or_else(|| PathBuf::from("config.json"));
        (Mode::Local(cli.local.clone()), load_config(&path).await?)
    };
    let config = Arc::new(config);
    let log = env.log.clone();
    let mut modules = Modules {
        config: config.clone(),
        mode,
        vars: env.vars,
        http: env.http,
        exe: env.exe,
        agent: cli.agent.clone(),
        log: log.clone(),
        scaleway: None,
    };

    let rotation = match &cli.command {
        Command::Check { content_ref } => {
            let gitlab = modules.gitlab().await?;
            let report = provision::check(&config, &gitlab, content_ref.as_deref()).await?;
            return write_check(out, &report);
        }
        Command::Status => {
            let provider = modules.provider().await?;
            let host = modules.host().await?;
            let provisioner = Provisioner::new(
                config.clone(),
                provider,
                host,
                None,
                Arc::new(MemoryOutputs::new()),
                log,
            );
            return write_status(out, &provisioner).await;
        }
        Command::Destroy { yes } if yes.as_deref() != Some(config.name.as_str()) => {
            writeln!(
                out,
                "destroy removes every resource of {0}; confirm with --yes {0}",
                config.name
            )?;
            return Ok(2);
        }
        Command::Rotate { kind } => match Rotation::parse(kind) {
            Some(rotation) => Some(rotation),
            None => {
                writeln!(out, "rotate needs one of: {}", Rotation::names())?;
                return Ok(2);
            }
        },
        Command::Plan | Command::Apply | Command::Destroy { .. } => None,
    };

    let provider = modules.provider().await?;
    if !matches!(cli.command, Command::Destroy { .. }) {
        let errors = check_capabilities(&config, |pool| provider.capabilities_for(pool));
        if !errors.is_empty() {
            return Err(Error::Config(errors));
        }
    }
    let host = modules.host().await?;
    let gitlab = if cli.skip_gitlab {
        None
    } else {
        Some(modules.gitlab().await?)
    };
    let (outputs, stored_in): (Arc<dyn OutputsStore>, String) = match (&modules.mode, &gitlab) {
        (Mode::Ci { project_id }, Some(gitlab)) => (
            Arc::new(GitlabOutputs::new(
                gitlab.clone(),
                *project_id,
                keys::output_keys(&config),
                &modules.vars,
                log.clone(),
            )),
            format!("variables of project {project_id}, environment scope {OUTPUTS_SCOPE}"),
        ),
        (Mode::Ci { .. }, None) => return Err(Error::other("--ci needs GitLab")),
        (Mode::Local(local), _) => {
            let outputs = FileOutputs::new(local.join("outputs.env"));
            let path = outputs.path().display().to_string();
            (Arc::new(outputs), path)
        }
    };
    let provisioner = Provisioner::new(config.clone(), provider, host, gitlab, outputs, log)
        .with_replay(modules.http()?);

    match cli.command {
        Command::Plan => {
            let steps = provisioner.plan(&[]).await?;
            writeln!(out, "{}", format_plan(&steps))?;
            writeln!(out, "{}", summarize_plan(&steps))?;
        }
        Command::Destroy { .. } => {
            provisioner.destroy().await?;
            writeln!(out, "destroyed {}", config.name)?;
        }
        _ => {
            let steps = provisioner.apply(rotation.as_slice()).await?;
            writeln!(out, "{}", format_plan(&steps))?;
            writeln!(out, "{}", summarize_plan(&steps))?;
            writeln!(out, "outputs: {stored_in}")?;
        }
    }
    Ok(0)
}

async fn write_status(out: &mut dyn Write, provisioner: &Provisioner) -> Result<i32> {
    let status = provisioner.status().await?;
    writeln!(
        out,
        "dispatcher: {}",
        status.url.as_deref().unwrap_or("not deployed")
    )?;
    for pool in &status.pools {
        writeln!(
            out,
            "pool {}: {} live of {}",
            pool.tag, pool.live, pool.max_runs
        )?;
    }
    let skip = status.runs.len().saturating_sub(20);
    for run in status.runs.iter().skip(skip) {
        writeln!(
            out,
            "run {} {} {:?} {} job={}",
            run.run_id,
            run.pool,
            run.state,
            run.created_at,
            run.labels.get("job").map(String::as_str).unwrap_or("")
        )?;
    }
    Ok(0)
}

fn write_check(out: &mut dyn Write, report: &[ProjectReport]) -> Result<i32> {
    let mut failed = false;
    for project in report {
        if !project.valid {
            failed = true;
            writeln!(
                out,
                "project {}: pipeline invalid: {}",
                project.project_id,
                project.errors.join("; ")
            )?;
            continue;
        }
        if project.problems.is_empty() {
            writeln!(
                out,
                "project {}: every job matches the stage map",
                project.project_id
            )?;
            continue;
        }
        failed = true;
        for p in &project.problems {
            writeln!(
                out,
                "project {}: job {} (stage {}): {}",
                project.project_id, p.job, p.stage, p.problem
            )?;
        }
    }
    Ok(if failed { 1 } else { 0 })
}
