//! The real binary with the pinned `gitlab-runner` (spec 8.5), a fake `docker` and one
//! loopback server playing both GitLab's job request API and the dispatcher: one `gitlab-runner
//! run` polls with every runner entry, reloads the rewritten `config.toml`, and exits on
//! `SIGQUIT`, after which the agent starts it again.
//!
//! The runner is `ciburst_core::cloud_init::RUNNER_URL`, downloaded once into the target
//! directory and checked against `RUNNER_SHA256`, or the file `CIBURST_TEST_GITLAB_RUNNER` names.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ciburst_core::cloud_init::{RUNNER_SHA256, RUNNER_URL};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const POOL: &str = "glrt-runner-secret";
/// The token of a spill runner a VM started before D62 still has in `vm.env`; the agent
/// ignores it.
const SPILL: &str = "glrt-spill-secret";
const MEMBER_A: &str = "glrt-member-a-secret";
const MEMBER_B: &str = "glrt-member-b-secret";
const JOB_REQUEST: &str = "/api/v4/jobs/request";

#[derive(Clone)]
struct Received {
    at: Instant,
    path: String,
    headers: BTreeMap<String, String>,
    body: String,
}

#[derive(Default)]
struct Server {
    inbox: Mutex<Vec<Received>>,
    /// The body of every `/vm` answer.
    answer: Mutex<String>,
}

impl Server {
    fn set_answer(&self, members: &[(u64, &str)]) {
        let runners: Vec<serde_json::Value> = members
            .iter()
            .map(|(project, token)| serde_json::json!({"projectId": project, "token": token}))
            .collect();
        *self.answer.lock().unwrap() = serde_json::json!({
            "result": "reported",
            "nextReportS": 10,
            "runners": runners,
            "ms": 1,
        })
        .to_string();
    }

    /// Job requests per runner token since `since`.
    fn requests(&self, since: Instant) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for received in self.inbox.lock().unwrap().iter() {
            if received.path != JOB_REQUEST || received.at < since {
                continue;
            }
            let body: serde_json::Value =
                serde_json::from_str(&received.body).expect("job request json");
            let token = body["token"].as_str().unwrap_or_default().to_string();
            *counts.entry(token).or_default() += 1;
        }
        counts
    }

    fn reports(&self) -> Vec<Received> {
        self.inbox
            .lock()
            .unwrap()
            .iter()
            .filter(|received| received.path == "/vm")
            .cloned()
            .collect()
    }
}

fn serve(listener: TcpListener, server: Arc<Server>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        let server = Arc::clone(&server);
        std::thread::spawn(move || connection(stream, &server));
    }
}

fn connection(stream: TcpStream, server: &Server) {
    let mut writer = stream.try_clone().expect("clone stream");
    let mut reader = BufReader::new(stream);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string();
        let mut headers = BTreeMap::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let length = headers
            .get("content-length")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let (status, answer) = match path.as_str() {
            "/vm" => ("200 OK", server.answer.lock().unwrap().clone()),
            "/terminate" => ("200 OK", String::new()),
            // No job pending: the runner asks again after `check_interval`.
            JOB_REQUEST => ("204 No Content", String::new()),
            _ => ("404 Not Found", String::new()),
        };
        server.inbox.lock().unwrap().push(Received {
            at: Instant::now(),
            path,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        });
        let response = if status.starts_with("204") {
            format!("HTTP/1.1 {status}\r\n\r\n")
        } else {
            format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{answer}",
                answer.len()
            )
        };
        if writer.write_all(response.as_bytes()).is_err() {
            return;
        }
    }
}

fn script(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn wait_for<T>(what: &str, seconds: u64, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn sha256(path: &Path) -> String {
    let output = Command::new("sha256sum").arg(path).output().unwrap();
    assert!(output.status.success(), "sha256sum failed");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The pinned `gitlab-runner`, downloaded once per target directory.
fn runner_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("CIBURST_TEST_GITLAB_RUNNER") {
        return PathBuf::from(path);
    }
    let path =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gitlab-runner-{RUNNER_SHA256}"));
    if path.exists() && sha256(&path) == RUNNER_SHA256 {
        return path;
    }
    let partial = path.with_extension("part");
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&partial)
        .arg(RUNNER_URL)
        .status()
        .expect("curl runs");
    assert!(status.success(), "download of {RUNNER_URL} failed");
    assert_eq!(sha256(&partial), RUNNER_SHA256, "{RUNNER_URL}");
    fs::set_permissions(&partial, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&partial, &path).unwrap();
    path
}

/// Process ids from the agent's `runner started` lines.
fn runner_pids(log: &str) -> Vec<i32> {
    log.lines()
        .filter_map(|line| line.strip_prefix("info runner started pid=\""))
        .filter_map(|pid| pid.trim_end_matches('"').parse().ok())
        .collect()
}

/// Ends the agent and every runner it started however the test ends.
struct Stop {
    agent: Child,
    log: PathBuf,
}

impl Drop for Stop {
    fn drop(&mut self) {
        let _ = self.agent.kill();
        let _ = self.agent.wait();
        for pid in runner_pids(&fs::read_to_string(&self.log).unwrap_or_default()) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

#[test]
fn one_runner_process_serves_every_entry_and_reloads_its_config() {
    let uptime: f64 = fs::read_to_string("/proc/uptime")
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    // Near minute 55 the agent would drain; the drain is covered by the unit tests.
    if (uptime as u64) % 3600 >= 53 * 60 {
        eprintln!("skipped: the host is near the drain window");
        return;
    }

    let runner = runner_binary();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(&runner, bin.join("gitlab-runner")).unwrap();
    let at = root.display();
    script(
        &bin.join("docker"),
        &format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{at}/docker.args\"\nexit 0\n"),
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server = Arc::new(Server::default());
    server.set_answer(&[(42, MEMBER_A)]);
    let serving = Arc::clone(&server);
    std::thread::spawn(move || serve(listener, serving));

    let state = root.join("state");
    let env_file = root.join("vm.env");
    fs::write(
        &env_file,
        format!(
            "CIBURST_RUN_ID=run-e2e\nCIBURST_VM_TOKEN=vm-token-secret\n\
             CIBURST_DISPATCH_URL={url}\nCI_SERVER_URL={url}\n\
             RUNNER_TOKEN={POOL}\nRUNNER_SPILL_TOKEN={SPILL}\n\
             CIBURST_POOL=vm\nCIBURST_SLOTS=2\n\
             CIBURST_DEFAULT_IMAGE=node:24\nCIBURST_TIMEOUT_S=3600\n\
             CIBURST_STATE_DIR={}\n",
            state.display()
        ),
    )
    .unwrap();
    let log_path = root.join("agent.log");
    let log = fs::File::create(&log_path).unwrap();
    let runner_log = root.join("runner.log");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let agent = Command::new(env!("CARGO_BIN_EXE_ciburst-vm-agent"))
        .env("PATH", path)
        .env("CIBURST_VM_ENV", &env_file)
        .stdin(Stdio::null())
        .stdout(fs::File::create(&runner_log).unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let _stop = Stop {
        agent,
        log: log_path.clone(),
    };
    let agent_log = || fs::read_to_string(&log_path).unwrap_or_default();

    // Two entries, one process: the VM runner and the member's.
    let start = Instant::now();
    wait_for("job requests of the VM runner and the member", 60, || {
        let requests = server.requests(start);
        (requests.contains_key(POOL) && requests.contains_key(MEMBER_A)).then_some(())
    });
    let config_path = state.join("config.toml");
    let config = fs::read_to_string(&config_path).unwrap();
    assert!(config.contains("concurrent = 2\n"), "{config}");
    assert_eq!(config.matches("[[runners]]").count(), 2, "{config}");
    let mode = fs::metadata(&config_path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let report = &server.reports()[0];
    assert_eq!(report.headers["x-ciburst-run"], "run-e2e");
    assert_eq!(report.headers["x-ciburst-token"], "vm-token-secret");
    let body: serde_json::Value = serde_json::from_str(&report.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"state": "idle", "free": 2, "slots": 2})
    );

    // A second member: the next report's answer rewrites the file, the same process reloads it
    // and polls with all three tokens.
    server.set_answer(&[(42, MEMBER_A), (77, MEMBER_B)]);
    let changed = Instant::now();
    wait_for("job requests of the added runner", 60, || {
        let requests = server.requests(changed);
        requests.contains_key(MEMBER_B).then_some(())
    });
    let config = fs::read_to_string(&config_path).unwrap();
    assert_eq!(config.matches("[[runners]]").count(), 3, "{config}");

    // Removed entries stop polling within a few seconds of the rewrite.
    server.set_answer(&[(77, MEMBER_B)]);
    wait_for("the config with two runners again", 30, || {
        (agent_log()
            .lines()
            .filter(|line| *line == r#"info config written runners="2""#)
            .count()
            == 2)
            .then_some(())
    });
    std::thread::sleep(Duration::from_secs(5));
    let settled = Instant::now();
    std::thread::sleep(Duration::from_secs(8));
    let requests = server.requests(settled);
    assert!(requests.contains_key(POOL), "{requests:?}");
    assert!(requests.contains_key(MEMBER_B), "{requests:?}");
    assert!(!requests.contains_key(MEMBER_A), "{requests:?}");
    assert!(!requests.contains_key(SPILL), "{requests:?}");

    // One process all along.
    let pids = runner_pids(&agent_log());
    assert_eq!(pids.len(), 1, "{}", agent_log());

    // `SIGQUIT` ends the idle runner; the agent starts it again.
    let quit = Instant::now();
    kill(Pid::from_raw(pids[0]), Signal::SIGQUIT).unwrap();
    wait_for("the runner's exit and restart", 30, || {
        (runner_pids(&agent_log()).len() == 2).then_some(())
    });
    eprintln!(
        "runner exited and restarted {:?} after SIGQUIT",
        quit.elapsed()
    );
    let restarted = Instant::now();
    wait_for("job requests of the restarted runner", 30, || {
        let requests = server.requests(restarted);
        (requests.contains_key(POOL) && requests.contains_key(MEMBER_B)).then_some(())
    });

    let docker = fs::read_to_string(root.join("docker.args")).unwrap();
    assert_eq!(
        docker.lines().collect::<Vec<_>>(),
        [
            "ps",
            "--filter",
            "label=com.gitlab.gitlab-runner.job.id",
            "--format",
            "{{.Label \"com.gitlab.gitlab-runner.job.id\"}}"
        ]
    );

    let log = agent_log();
    for line in [
        r#"info agent started run="run-e2e" runner="vm" slots="2" timeout_s="3600""#,
        r#"info config written runners="2""#,
        r#"info config written runners="3""#,
        r#"info state from="starting" to="idle" free="2""#,
        r#"warn runner exited code="0""#,
    ] {
        assert!(log.lines().any(|l| l == line), "{line} not in:\n{log}");
    }
    // The runner's own lines share the log, as in the journal, and name tokens by a short
    // prefix only.
    assert!(!log.contains("secret"), "{log}");
}
