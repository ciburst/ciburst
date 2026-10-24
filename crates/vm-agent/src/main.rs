//! VM supervisor, spec section 6.1: runs `gitlab-runner run-single` slots, reports the VM's
//! state to the dispatcher, drains at minute 55 of a paid hour and asks the dispatcher to
//! terminate the VM.

mod agent;
mod config;
mod system;

use std::process::ExitCode;
use std::sync::Arc;

use ciburst_core::http::ReqwestClient;
use ciburst_core::log::Log;

use crate::system::{DockerCli, ProcUptime, ProcessSlots, System, SystemClock};

const USAGE: u8 = 64;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [flag] if flag == "--version" || flag == "-V" => {
            println!("ciburst-vm-agent {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!(
                "usage: ciburst-vm-agent [--version]\nreads {} (or ${})",
                config::DEFAULT_PATH,
                config::PATH_ENV
            );
            return ExitCode::from(USAGE);
        }
    }

    let log = Log::stderr();
    let path = std::env::var(config::PATH_ENV).unwrap_or_else(|_| config::DEFAULT_PATH.to_string());
    let config = match std::fs::read_to_string(&path) {
        Ok(text) => config::parse(&text).map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    let config = match config {
        Ok(config) => config,
        Err(error) => {
            log.error("config", &[("path", &path), ("error", &error)]);
            return ExitCode::from(USAGE);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log.error("runtime", &[("error", &error.to_string())]);
            return ExitCode::FAILURE;
        }
    };
    let _runtime = runtime.enter();
    let http = match ReqwestClient::new() {
        Ok(http) => http,
        Err(error) => {
            log.error("http client", &[("error", &error.to_string())]);
            return ExitCode::FAILURE;
        }
    };
    let sys = System {
        clock: Arc::new(SystemClock::new()),
        uptime: Arc::new(ProcUptime),
        slots: Box::new(ProcessSlots::default()),
        docker: Arc::new(DockerCli),
        http: Arc::new(http),
    };
    let code = runtime.block_on(agent::Agent::new(config, sys, log).run());
    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
