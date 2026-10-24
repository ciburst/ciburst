//! The real binary with fake `gitlab-runner` and `docker` executables and a dispatcher on the
//! loopback interface: config file, slot processes, `docker ps` and reports over HTTP.
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

#[derive(Clone)]
struct Received {
    path: String,
    headers: BTreeMap<String, String>,
    body: String,
}

type Inbox = Arc<Mutex<Vec<Received>>>;

fn serve(listener: TcpListener, inbox: Inbox) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        let inbox = Arc::clone(&inbox);
        std::thread::spawn(move || connection(stream, &inbox));
    }
}

fn connection(stream: TcpStream, inbox: &Inbox) {
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
        inbox.lock().unwrap().push(Received {
            path,
            headers,
            body: String::from_utf8(body).expect("utf-8 body"),
        });
        if writer
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
            .is_err()
        {
            return;
        }
    }
}

fn script(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn report(inbox: &Inbox, state: &str) -> Option<Received> {
    inbox
        .lock()
        .unwrap()
        .iter()
        .find(|received| {
            received.path == "/vm" && received.body.contains(&format!("\"state\":\"{state}\""))
        })
        .cloned()
}

/// Non-empty files the fake runner wrote, `slot-<pid><suffix>`.
fn slot_files(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with("slot-") && name.ends_with(suffix)
        })
        .filter(|path| fs::metadata(path).is_ok_and(|meta| meta.len() > 0))
        .collect();
    files.sort();
    files
}

/// Ends the agent and its fake slots however the test ends.
struct Stop {
    agent: Child,
    running: PathBuf,
}

impl Drop for Stop {
    fn drop(&mut self) {
        let _ = self.agent.kill();
        let _ = self.agent.wait();
        let _ = fs::remove_file(&self.running);
    }
}

#[test]
fn the_binary_runs_slots_and_reports_over_http() {
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

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let at = root.display();
    script(
        &bin.join("gitlab-runner"),
        &format!(
            "#!/bin/sh\nenv > \"{at}/slot-$$.env\"\nprintf '%s\\n' \"$@\" > \"{at}/slot-$$.args\"\n\
             trap 'exit 0' QUIT\nwhile [ -e \"{at}/running\" ]; do sleep 0.1; done\n"
        ),
    );
    script(
        &bin.join("docker"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{at}/docker.args\"\ncat \"{at}/jobs\" 2> /dev/null\nexit 0\n"
        ),
    );
    fs::write(root.join("running"), "").unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let inbox: Inbox = Arc::default();
    let server_inbox = Arc::clone(&inbox);
    std::thread::spawn(move || serve(listener, server_inbox));

    let env_file = root.join("vm.env");
    fs::write(
        &env_file,
        format!(
            "CIBURST_RUN_ID=run-e2e\nCIBURST_VM_TOKEN=vm-token-secret\n\
             CIBURST_DISPATCH_URL=http://127.0.0.1:{port}\nCI_SERVER_URL=https://gitlab.example.invalid\n\
             RUNNER_TOKEN=glrt-runner-secret\nCIBURST_POOL=vm\nCIBURST_SLOTS=2\n\
             CIBURST_DEFAULT_IMAGE=node:24\nCIBURST_TIMEOUT_S=3600\n"
        ),
    )
    .unwrap();
    let log_path = root.join("agent.log");
    let log = fs::File::create(&log_path).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let agent = Command::new(env!("CARGO_BIN_EXE_ciburst-vm-agent"))
        .env("PATH", path)
        .env("CIBURST_VM_ENV", &env_file)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let stop = Stop {
        agent,
        running: root.join("running"),
    };

    let idle = wait_for("the idle report", || report(&inbox, "idle"));
    assert_eq!(idle.headers["x-ciburst-run"], "run-e2e");
    assert_eq!(idle.headers["x-ciburst-token"], "vm-token-secret");
    assert_eq!(idle.headers["content-type"], "application/json");
    let body: serde_json::Value = serde_json::from_str(&idle.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"state": "idle", "free": 2, "slots": 2})
    );

    let args = wait_for("two slots", || {
        let files = slot_files(root, ".args");
        (files.len() == 2).then_some(files)
    });
    let mut configs = Vec::new();
    for file in args {
        let lines: Vec<String> = fs::read_to_string(file)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(
            lines[..18],
            [
                "run-single",
                "--url",
                "https://gitlab.example.invalid",
                "--token",
                "glrt-runner-secret",
                "--executor",
                "docker",
                "--docker-image",
                "node:24",
                "--docker-privileged",
                "--docker-volumes",
                "/cache",
                "--docker-volumes",
                "/certs/client",
                "--max-builds",
                "0",
                "--wait-timeout",
                "0",
            ]
        );
        assert_eq!(lines.len(), 20);
        configs.push(lines[19].clone());
    }
    configs.sort();
    assert_eq!(
        configs,
        [
            "/var/lib/ciburst/slot-1/config.toml",
            "/var/lib/ciburst/slot-2/config.toml"
        ]
    );
    let envs = slot_files(root, ".env");
    assert_eq!(envs.len(), 2);
    for file in envs {
        let env = fs::read_to_string(file).unwrap();
        assert!(!env.contains("secret"), "{env}");
    }
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

    fs::write(root.join("jobs"), "42\n42\n").unwrap();
    let busy = wait_for("the busy report", || report(&inbox, "busy"));
    let body: serde_json::Value = serde_json::from_str(&busy.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"state": "busy", "free": 1, "slots": 2})
    );

    drop(stop);
    let log = fs::read_to_string(log_path).unwrap();
    for line in [
        r#"info agent started run="run-e2e" pool="vm" slots="2" timeout_s="3600""#,
        r#"info slot started slot="1""#,
        r#"info slot started slot="2""#,
        r#"info state from="starting" to="idle" free="2""#,
        r#"info state from="idle" to="busy" free="1""#,
    ] {
        assert!(log.lines().any(|l| l == line), "{line} not in:\n{log}");
    }
    assert!(!log.contains("secret"), "{log}");
}
