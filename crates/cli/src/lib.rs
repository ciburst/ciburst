//! The `ciburst` command. `run` is the whole program so tests can drive it in process.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ciburst_core::config::{Config, check_capabilities, load_config};
use ciburst_core::fake::{FakeHost, FakeProvider};
use ciburst_core::gitlab::Gitlab;
use ciburst_core::http::ReqwestClient;
use ciburst_core::log::Log;
use ciburst_core::plan::{FileOutputs, format_plan, read_env_file, summarize_plan};
use ciburst_core::provision::Provisioner;
use ciburst_core::store::FileStore;
use ciburst_core::types::{Host, Provider, Rotation};
use ciburst_core::{Error, Result};
use ciburst_scaleway::client::{Credentials, DEFAULT_REGION, ScalewayClient};
use ciburst_scaleway::host::{HOST_NAME as SCALEWAY_HOST, ScalewayFunction};
use ciburst_scaleway::provider::{PROVIDER_NAME as SCALEWAY_PROVIDER, ScalewayJobs};
use clap::{Parser, Subcommand};
use serde_json::Value;

pub const PROVIDERS: &[&str] = &["fake", SCALEWAY_PROVIDER];
pub const HOSTS: &[&str] = &["fake", SCALEWAY_HOST];

#[derive(Parser)]
#[command(name = "ciburst", version, about = "Serverless per-job GitLab runners")]
struct Cli {
    /// Config file
    #[arg(long, default_value = "config.json", global = true)]
    config: PathBuf,
    /// Directory with scaleway.env, gitlab.env, outputs.env and state.json
    #[arg(long, default_value = ".local", global = true)]
    local: PathBuf,
    /// Plan or apply without touching GitLab
    #[arg(long, global = true)]
    skip_gitlab: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show what apply would change
    Plan,
    /// Create or update every resource and write <local>/outputs.env
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
    Check,
    /// Replace a secret and redeploy: runner, hook, key or cache
    Rotate { kind: String },
}

/// Modules selected by the config, built once per command.
struct Modules {
    config: Arc<Config>,
    local: PathBuf,
    log: Log,
    scaleway: Option<Arc<ScalewayClient>>,
}

impl Modules {
    async fn scaleway(&mut self) -> Result<Arc<ScalewayClient>> {
        if let Some(client) = &self.scaleway {
            return Ok(client.clone());
        }
        let env = read_env_file(&self.local.join("scaleway.env")).await?;
        let credentials = Credentials::from_env(&env)?;
        let region = self
            .config
            .provider
            .extra
            .get("region")
            .or_else(|| self.config.host.extra.get("region"))
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_REGION)
            .to_string();
        let client = Arc::new(ScalewayClient::new(
            Arc::new(ReqwestClient::new()?),
            credentials,
            &region,
        ));
        self.scaleway = Some(client.clone());
        Ok(client)
    }

    fn store(&self, slot: &str) -> Arc<FileStore> {
        Arc::new(FileStore::new(self.local.join("state.json"), slot))
    }

    async fn provider(&mut self) -> Result<Arc<dyn Provider>> {
        match self.config.provider.kind.as_str() {
            "fake" => Ok(Arc::new(FakeProvider::new(
                self.config.clone(),
                self.store("provider"),
            ))),
            SCALEWAY_PROVIDER => {
                let client = self.scaleway().await?;
                Ok(Arc::new(ScalewayJobs::new(
                    self.config.clone(),
                    client,
                    self.log.clone(),
                )))
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

pub async fn run(args: Vec<String>, out: &mut dyn Write) -> Result<i32> {
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

    let log = Log::stderr();
    let config = Arc::new(load_config(&cli.config).await?);
    let mut modules = Modules {
        config: config.clone(),
        local: cli.local.clone(),
        log: log.clone(),
        scaleway: None,
    };
    let provider = modules.provider().await?;
    let capability_errors = check_capabilities(&config, &provider.capabilities());
    if !capability_errors.is_empty() {
        return Err(Error::Config(capability_errors));
    }
    let host = modules.host().await?;
    let gitlab = if cli.skip_gitlab {
        None
    } else {
        Some(Arc::new(gitlab_client(&config, &cli.local).await?))
    };

    let outputs = Arc::new(FileOutputs::new(cli.local.join("outputs.env")));
    let outputs_path = outputs.path().to_path_buf();
    let provisioner = Provisioner::new(config.clone(), provider, host, gitlab, outputs, log);

    match cli.command {
        Command::Plan => {
            let steps = provisioner.plan(&[]).await?;
            writeln!(out, "{}", format_plan(&steps))?;
            writeln!(out, "{}", summarize_plan(&steps))?;
            Ok(0)
        }
        Command::Apply => {
            let steps = provisioner.apply(&[]).await?;
            writeln!(out, "{}", format_plan(&steps))?;
            writeln!(out, "{}", summarize_plan(&steps))?;
            writeln!(out, "outputs: {}", outputs_path.display())?;
            Ok(0)
        }
        Command::Rotate { kind } => {
            let Some(rotation) = Rotation::parse(&kind) else {
                writeln!(out, "rotate needs one of: {}", Rotation::names())?;
                return Ok(2);
            };
            let steps = provisioner.apply(&[rotation]).await?;
            writeln!(out, "{}", format_plan(&steps))?;
            writeln!(out, "{}", summarize_plan(&steps))?;
            Ok(0)
        }
        Command::Destroy { yes } => {
            if yes.as_deref() != Some(config.name.as_str()) {
                writeln!(
                    out,
                    "destroy removes every resource of {0}; confirm with --yes {0}",
                    config.name
                )?;
                return Ok(2);
            }
            provisioner.destroy().await?;
            writeln!(out, "destroyed {}", config.name)?;
            Ok(0)
        }
        Command::Status => {
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
        Command::Check => {
            let report = provisioner.check().await?;
            let mut failed = false;
            for project in &report {
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
    }
}

async fn gitlab_client(config: &Config, local: &Path) -> Result<Gitlab> {
    let token = match std::env::var("GITLAB_TOKEN") {
        Ok(token) if !token.is_empty() => token,
        _ => read_env_file(&local.join("gitlab.env"))
            .await?
            .remove("GITLAB_TOKEN")
            .ok_or_else(|| {
                Error::other(format!(
                    "GITLAB_TOKEN missing in {}; pass --skip-gitlab to leave GitLab untouched",
                    local.join("gitlab.env").display()
                ))
            })?,
    };
    Ok(Gitlab::new(
        &config.gitlab.url,
        token,
        Arc::new(ReqwestClient::new()?),
    ))
}
