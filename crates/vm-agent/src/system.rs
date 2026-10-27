//! Every interaction of the agent with the operating system, behind traits so the protocol in
//! `agent.rs` runs against fakes with a fake clock. Production implementations are below.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
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

/// The VM's one `gitlab-runner run` process (spec 8.5).
pub trait Process: Send {
    /// Starts the process and returns its id.
    fn spawn(&mut self, command: &ProcessCommand) -> io::Result<u32>;
    /// `Some(code)` once the process has exited, `-1` when a signal ended it; an error while
    /// none runs.
    fn try_exit(&mut self) -> io::Result<Option<i32>>;
    /// Sends `SIGQUIT`: the runner stops requesting jobs, finishes the running ones and exits.
    fn quit(&mut self) -> io::Result<()>;
}

/// The runner's `config.toml`.
pub trait Files: Send + Sync {
    /// Replaces `path` with `content` in one rename, readable by its owner only.
    fn write_atomic(&self, path: &Path, content: &str) -> io::Result<()>;
}

#[async_trait]
pub trait Docker: Send + Sync {
    /// Output of `docker ps` printing the job id label of every running job container, one
    /// container per line.
    async fn job_labels(&self) -> io::Result<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCommand {
    pub program: String,
    pub args: Vec<String>,
}

pub struct System {
    pub clock: Arc<dyn Clock>,
    pub uptime: Arc<dyn Uptime>,
    pub process: Box<dyn Process>,
    pub files: Arc<dyn Files>,
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
        parse_uptime(&fs::read_to_string("/proc/uptime")?)
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

pub struct LocalFiles;

impl Files for LocalFiles {
    fn write_atomic(&self, path: &Path, content: &str) -> io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
        fs::create_dir_all(dir)?;
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".new");
        let temp = dir.join(name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // The file holds runner tokens and the cache key.
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    }
}

#[derive(Default)]
pub struct LocalProcess {
    child: Option<Child>,
}

impl Process for LocalProcess {
    fn spawn(&mut self, command: &ProcessCommand) -> io::Result<u32> {
        let mut process = Command::new(&command.program);
        // The runner reads settings from its environment; `config.toml` is the only source.
        process.args(&command.args).env_clear().stdin(Stdio::null());
        for key in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(key) {
                process.env(key, value);
            }
        }
        let child = process.spawn()?;
        let id = child.id();
        self.child = Some(child);
        Ok(id)
    }

    fn try_exit(&mut self) -> io::Result<Option<i32>> {
        let child = self.child()?;
        let Some(status) = child.try_wait()? else {
            return Ok(None);
        };
        self.child = None;
        Ok(Some(status.code().unwrap_or(-1)))
    }

    fn quit(&mut self) -> io::Result<()> {
        send_sigquit(self.child()?.id())
    }
}

impl LocalProcess {
    fn child(&mut self) -> io::Result<&mut Child> {
        self.child
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no runner process"))
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

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_the_kernel_uptime() {
        assert!(ProcUptime.seconds().unwrap() > 0);
    }

    #[cfg(unix)]
    fn wait_exit(process: &mut LocalProcess) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(code) = process.try_exit().unwrap() {
                return code;
            }
            assert!(Instant::now() < deadline, "the process did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_process_gets_sigquit_and_a_clean_environment() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let env = dir.path().join("env");
        let script = "env > \"$2\"; trap 'exit 3' QUIT; : > \"$1\"; while :; do sleep 0.05; done";
        let mut process = LocalProcess::default();
        assert!(process.try_exit().is_err());
        let id = process
            .spawn(&ProcessCommand {
                program: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    script.to_string(),
                    "sh".to_string(),
                    ready.to_string_lossy().into_owned(),
                    env.to_string_lossy().into_owned(),
                ],
            })
            .unwrap();
        assert!(id > 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "the process never became ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(process.try_exit().unwrap(), None);
        process.quit().unwrap();
        assert_eq!(wait_exit(&mut process), 3);
        assert!(process.try_exit().is_err());
        assert!(process.quit().is_err());

        let env = fs::read_to_string(env).unwrap();
        let keys: Vec<&str> = env
            .lines()
            .filter_map(|line| line.split_once('=').map(|(key, _)| key))
            .collect();
        assert!(keys.contains(&"PATH"), "{env}");
        for key in keys {
            // Only what the agent passes on, plus what the shell sets itself.
            assert!(
                ["PATH", "HOME", "PWD", "OLDPWD", "SHLVL", "_"].contains(&key),
                "{env}"
            );
        }
    }

    #[test]
    fn config_files_are_replaced_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("config.toml");
        LocalFiles.write_atomic(&path, "concurrent = 1\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "concurrent = 1\n");
        LocalFiles.write_atomic(&path, "concurrent = 2\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "concurrent = 2\n");
        let names: Vec<String> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["config.toml"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
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
