//! Every interaction of the agent with the operating system, behind traits so the protocol in
//! `agent.rs` runs against fakes with a fake clock. Production implementations are below.

use std::collections::BTreeMap;
use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ciburst_core::http::HttpClient;

/// Label the Docker executor puts on every job container (`executors/docker/internal/labels`).
pub const JOB_LABEL: &str = "com.gitlab.gitlab-runner.job.id";
const DOCKER_TIMEOUT: Duration = Duration::from_secs(20);

#[async_trait]
pub trait Clock: Send + Sync {
    /// Time since a fixed start; only differences matter.
    fn now(&self) -> Duration;
    async fn sleep(&self, duration: Duration);
}

pub trait Uptime: Send + Sync {
    /// Whole seconds since the kernel booted.
    fn seconds(&self) -> io::Result<u64>;
}

/// One `gitlab-runner run-single` process per slot.
pub trait Slots: Send {
    fn spawn(&mut self, command: &SlotCommand) -> io::Result<()>;
    /// `Some(code)` once the slot's process has exited, `-1` when a signal ended it.
    fn try_exit(&mut self, slot: u32) -> io::Result<Option<i32>>;
    /// Sends `SIGQUIT`: the runner stops requesting jobs, finishes the current one and exits.
    fn quit(&mut self, slot: u32) -> io::Result<()>;
}

#[async_trait]
pub trait Docker: Send + Sync {
    /// Output of `docker ps` printing the job id label of every running job container.
    async fn job_labels(&self) -> io::Result<String>;
}

pub struct SlotCommand {
    pub slot: u32,
    pub program: String,
    pub args: Vec<String>,
    pub config_dir: String,
}

pub struct System {
    pub clock: Arc<dyn Clock>,
    pub uptime: Arc<dyn Uptime>,
    pub slots: Box<dyn Slots>,
    pub docker: Arc<dyn Docker>,
    pub http: Arc<dyn HttpClient>,
}

pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> SystemClock {
        SystemClock {
            start: Instant::now(),
        }
    }
}

#[async_trait]
impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

pub struct ProcUptime;

impl Uptime for ProcUptime {
    fn seconds(&self) -> io::Result<u64> {
        parse_uptime(&std::fs::read_to_string("/proc/uptime")?)
    }
}

pub fn parse_uptime(text: &str) -> io::Result<u64> {
    text.split_whitespace()
        .next()
        .and_then(|first| first.split('.').next())
        .and_then(|seconds| seconds.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unexpected /proc/uptime"))
}

pub struct DockerCli;

#[async_trait]
impl Docker for DockerCli {
    async fn job_labels(&self) -> io::Result<String> {
        let output = tokio::time::timeout(
            DOCKER_TIMEOUT,
            tokio::process::Command::new("docker")
                .args(docker_ps_args())
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "docker ps timed out"))??;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "docker ps exited with {}",
                output.status
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

pub fn docker_ps_args() -> Vec<String> {
    vec![
        "ps".to_string(),
        "--filter".to_string(),
        format!("label={JOB_LABEL}"),
        "--format".to_string(),
        format!("{{{{.Label \"{JOB_LABEL}\"}}}}"),
    ]
}

#[derive(Default)]
pub struct ProcessSlots {
    children: BTreeMap<u32, Child>,
}

impl Slots for ProcessSlots {
    fn spawn(&mut self, command: &SlotCommand) -> io::Result<()> {
        std::fs::create_dir_all(&command.config_dir)?;
        let mut process = Command::new(&command.program);
        // The runner reads many settings from its environment; the flags are the only source.
        process.args(&command.args).env_clear().stdin(Stdio::null());
        for key in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(key) {
                process.env(key, value);
            }
        }
        let child = process.spawn()?;
        self.children.insert(command.slot, child);
        Ok(())
    }

    fn try_exit(&mut self, slot: u32) -> io::Result<Option<i32>> {
        let child = self.child(slot)?;
        let Some(status) = child.try_wait()? else {
            return Ok(None);
        };
        self.children.remove(&slot);
        Ok(Some(status.code().unwrap_or(-1)))
    }

    fn quit(&mut self, slot: u32) -> io::Result<()> {
        send_sigquit(self.child(slot)?.id())
    }
}

impl ProcessSlots {
    fn child(&mut self, slot: u32) -> io::Result<&mut Child> {
        self.children
            .get_mut(&slot)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no slot {slot}")))
    }
}

#[cfg(unix)]
fn send_sigquit(pid: u32) -> io::Result<()> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let pid = i32::try_from(pid).map_err(io::Error::other)?;
    kill(Pid::from_raw(pid), Signal::SIGQUIT).map_err(io::Error::from)
}

#[cfg(not(unix))]
fn send_sigquit(_pid: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SIGQUIT needs a Unix host",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_is_the_whole_seconds_of_the_first_field() {
        assert_eq!(parse_uptime("3312.57 6400.12\n").unwrap(), 3312);
        assert_eq!(parse_uptime("42 1").unwrap(), 42);
        assert!(parse_uptime("").is_err());
        assert!(parse_uptime("abc 1").is_err());
    }

    #[test]
    fn docker_query_filters_and_prints_the_job_label() {
        assert_eq!(
            docker_ps_args(),
            [
                "ps",
                "--filter",
                "label=com.gitlab.gitlab-runner.job.id",
                "--format",
                "{{.Label \"com.gitlab.gitlab-runner.job.id\"}}",
            ]
        );
    }
}
