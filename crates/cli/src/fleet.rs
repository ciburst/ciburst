//! spec 8.2 to 8.4 on the command line: `invite` in the host, `join` and `leave` in a
//! member's pipeline. A member joins with its own lanes and `ignoreJobs` from its own
//! `ciburst.json` (D60, D61, spec 8.3).

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ciburst_core::config::{DEFAULT_VM_TAG, ProjectConfig, parse_project_config};
use ciburst_core::dispatch::{PATH_FLEET, PATH_JOIN, PATH_LEAVE};
use ciburst_core::gitlab::Gitlab;
use ciburst_core::http::{HttpClient, Request, Response};
use ciburst_core::invite::{self, Invite};
use ciburst_core::log::Log;
use ciburst_core::time::rfc3339;
use ciburst_core::{Error, Result};
use serde::Deserialize;
use serde_json::{Value, json};

/// The join code a member's `ciburst-join` and `ciburst-leave` read.
pub const JOIN_VAR: &str = "CIBURST_JOIN";
/// The project, id or path, the host's `ciburst-invite` makes a code for.
pub const INVITE_VAR: &str = "CIBURST_INVITE";

/// The answer of `GET /fleet`: the fleet's name, its VM runner and the member's webhook secret.
#[derive(Debug, Deserialize)]
struct FleetInfo {
    name: String,
    vm: Option<FleetVm>,
    #[serde(rename = "hookSecret")]
    hook_secret: String,
}

#[derive(Debug, Deserialize)]
struct FleetVm {
    tag: String,
}

impl FleetInfo {
    fn vm_tag(&self) -> &str {
        self.vm
            .as_ref()
            .map_or(DEFAULT_VM_TAG, |vm| vm.tag.as_str())
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `ciburst invite <project>`: a join code for `project`, an id or a path, signed with the
/// host's hook token. The host's own project gets none.
#[allow(clippy::too_many_arguments)]
pub async fn invite(
    out: &mut dyn Write,
    gitlab: Option<Arc<Gitlab>>,
    host_project: u64,
    project: &str,
    hook_token: Option<String>,
    url: Option<String>,
    now: u64,
) -> Result<i32> {
    let hook_token = hook_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| Error::other("no hook token in the outputs: run ciburst-setup first"))?;
    let url = url.ok_or_else(|| {
        Error::other("the dispatcher's URL is not in the outputs: run ciburst-setup first")
    })?;
    let project = project.trim();
    let project_id = match project.parse::<u64>() {
        Ok(id) if id > 0 => id,
        _ if project.is_empty() => return Err(Error::other("invite needs a project id or path")),
        _ => {
            let gitlab = gitlab.ok_or_else(|| {
                Error::other(format!("looking up {project} needs a GitLab token"))
            })?;
            gitlab
                .project(project)
                .await
                .map_err(|error| {
                    Error::other(format!(
                        "project {project} not found with this token ({error}): give its id"
                    ))
                })?
                .id
        }
    };
    if project_id == host_project {
        return Err(Error::other(format!(
            "project {project_id} is the host of this fleet"
        )));
    }
    let code = invite::issue(&hook_token, &url, project_id, now);
    writeln!(
        out,
        "join code for project {project_id}, valid until {}; treat it as a secret:",
        rfc3339(now + invite::TTL_S)
    )?;
    writeln!(out, "{code}")?;
    Ok(0)
}

/// The invite of the member's code: for this project, and not expired unless `expired_ok`.
fn member_invite(code: &str, project_id: u64, now: u64, expired_ok: bool) -> Result<Invite> {
    let invite =
        invite::decode(code).map_err(|error| Error::other(format!("{JOIN_VAR}: {error}")))?;
    if invite.project != project_id {
        return Err(Error::other(format!(
            "{JOIN_VAR}: the code is for project {}, this is project {project_id}",
            invite.project
        )));
    }
    if !expired_ok && now >= invite.exp {
        return Err(Error::other(format!(
            "{JOIN_VAR}: {}",
            invite::InviteError::Expired
        )));
    }
    Ok(invite)
}

/// A call to the dispatcher with the code; a refusal names the dispatcher's answer.
async fn call(
    http: &dyn HttpClient,
    method: &str,
    invite: &Invite,
    path: &str,
    code: &str,
    body: Option<&Value>,
) -> Result<Value> {
    let mut request =
        Request::new(method, format!("{}{path}", invite.url)).header(invite::HEADER, code);
    if let Some(body) = body {
        request = request.json(body);
    }
    let response: Response = http.send(request).await?;
    if response.status == 401 {
        return Err(Error::other(format!(
            "the dispatcher refused the join code on {path}: expired or of another fleet"
        )));
    }
    if !response.ok() {
        return Err(Error::other(format!(
            "{method} {path}: {} {}",
            response.status,
            response.text()
        )));
    }
    response.json()
}

/// `ciburst --ci join` (spec 8.3, 8.4): the member's own `ciburst.json`
/// (`project_file`; none means no lanes and no `ignoreJobs`) is validated first, then this
/// project's earlier runners of the fleet go, one runner for the fleet's VM runner and one per
/// lane are registered in this project, the webhook gets the member's secret, and `/join`
/// receives the runner tokens, the lanes and `ignoreJobs`.
#[allow(clippy::too_many_arguments)]
pub async fn join(
    out: &mut dyn Write,
    gitlab: &Gitlab,
    http: &dyn HttpClient,
    log: &Log,
    project_id: u64,
    code: &str,
    project_file: Option<&str>,
    now: u64,
) -> Result<i32> {
    let invite = member_invite(code, project_id, now, false)?;
    let fleet: FleetInfo =
        serde_json::from_value(call(http, "GET", &invite, PATH_FLEET, code, None).await?)?;
    let vm_tag = fleet.vm_tag().to_string();
    let settings = match project_file {
        Some(text) => parse_project_config(text, &fleet.name, &vm_tag, project_id)?,
        None => ProjectConfig::default(),
    };
    let prefix = format!("{}-", fleet.name);
    for runner in gitlab.list_runners(project_id).await? {
        if runner.description.starts_with(&prefix) {
            gitlab.delete_runner(runner.id).await?;
        }
    }
    let register = async |tag: &str| -> Result<String> {
        let description = format!("{prefix}{tag}");
        let runner = gitlab
            .create_runner(project_id, &[tag.to_string()], &description)
            .await?;
        log.info("runner", &[("tag", tag), ("id", &runner.id.to_string())]);
        Ok(runner.token)
    };
    let runners = BTreeMap::from([(vm_tag.clone(), register(&vm_tag).await?)]);
    let mut lane_runners = BTreeMap::new();
    for lane in &settings.lanes {
        lane_runners.insert(lane.tag.clone(), register(&lane.tag).await?);
    }
    let hooks = gitlab.list_hooks(project_id).await?;
    match hooks.iter().find(|h| same_url(&h.url, &invite.url)) {
        Some(hook) => {
            gitlab
                .update_hook(project_id, hook.id, &invite.url, &fleet.hook_secret)
                .await?;
        }
        None => {
            gitlab
                .create_hook(project_id, &invite.url, &fleet.hook_secret)
                .await?;
        }
    }
    log.info("hook", &[("project", &project_id.to_string())]);
    let body = json!({
        "runners": runners,
        "lanes": settings.lanes,
        "laneRunners": lane_runners,
        "ignoreJobs": settings.ignore_jobs,
    });
    let answer = call(http, "POST", &invite, PATH_JOIN, code, Some(&body)).await?;
    let name = answer
        .get("fleet")
        .and_then(Value::as_str)
        .unwrap_or(&fleet.name);
    for lane in &settings.lanes {
        writeln!(out, "lane {} ({})", lane.tag, lane.image)?;
    }
    writeln!(out, "joined fleet {name}")?;
    Ok(0)
}

/// `ciburst --ci leave` (spec 8.3, 8.4): `/leave`, which deletes the member's
/// lane definitions and record, then the member's runners and webhook go. An expired code
/// still leaves.
pub async fn leave(
    out: &mut dyn Write,
    gitlab: &Gitlab,
    http: &dyn HttpClient,
    log: &Log,
    project_id: u64,
    code: &str,
    now: u64,
) -> Result<i32> {
    let invite = member_invite(code, project_id, now, true)?;
    let answer = call(http, "POST", &invite, PATH_LEAVE, code, None).await?;
    let name = answer
        .get("fleet")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::other(format!("{PATH_LEAVE}: no fleet in the answer")))?
        .to_string();
    for hook in gitlab.list_hooks(project_id).await? {
        if same_url(&hook.url, &invite.url) {
            gitlab.delete_hook(project_id, hook.id).await?;
            log.info("hook_deleted", &[("project", &project_id.to_string())]);
        }
    }
    let prefix = format!("{name}-");
    for runner in gitlab.list_runners(project_id).await? {
        if runner.description.starts_with(&prefix) {
            gitlab.delete_runner(runner.id).await?;
            log.info("runner_deleted", &[("id", &runner.id.to_string())]);
        }
    }
    writeln!(out, "left fleet {name}")?;
    Ok(0)
}

fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}
