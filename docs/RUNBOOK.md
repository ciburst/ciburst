# Runbook

Rules and design: `docs/SPEC.md`. Below, `ciburst` stands for the release binary or `cargo run -p ciburst --`.

## Facts

| Item | Value |
|---|---|
| Region | `provider.region` (default `fr-par`) holds the dispatcher, the cache bucket, the agent and runner objects, the VMs and the lane runs; `provider.zone` defaults to `<region>-1` and must lie in the region |
| Scaleway resources | project `<name>`, IAM applications `<name>-dispatch` and `<name>-cache`, bucket `<name>-cache-<project id>`, namespace `<name>`, function `<name>-dispatch`, cron `<name>-sweep`, security group `<name>-vm`, servers `<name>-<vm tag>-<8 hex>`, one job definition `<name>-<project id>-<lane tag>` per lane of every member; with `provider.registry` the registry namespace `<name>` |
| GitLab resources | in the host: runner `<name>-<vm tag>`, enabled in the further projects of `gitlab.projectIds`, and runner `<name>-<lane tag>` per host lane; in a member: runner `<name>-<vm tag>` and `<name>-<lane tag>` per member lane; a webhook to the function URL in every project |
| Agent object | `ciburst/agent/<sha256>` in the cache bucket, public read: the musl `ciburst-vm-agent`; `apply` deletes all but the current one and the two newest others (D69) |
| Runner object | `ciburst/runner/<sha256>` in the cache bucket, public read: `gitlab-runner` 18.11.4, which VMs and lane runs download and check against its SHA-256 |
| Bucket state | `ciburst/intents/` (burst intents, queue, running jobs, typical durations, pipeline records, closed pipelines; expired by the lifecycle rule), `ciburst/fleet/` (fleet config and member records; never expire) |
| Image | `registry.gitlab.com/ciburst/ciburst/cli` (the component's jobs, with `ciburst` and `ciburst-vm-agent`); `edge` from `main`, `X.Y.Z`, `X.Y`, `X` and `latest` from a tag `vX.Y.Z` |
| Binary | `ciburst-x86_64-unknown-linux-musl` in the framework project's generic package registry, linked from each release |
| Runner | `gitlab-runner` 18.11.4. VM runner: one `run` per VM with the Docker executor, `concurrent = slots`, an entry for the host's and for each member's VM runner. Lanes: `run-single` with the shell executor in the lane's image, one run per pending lane job below `maxRuns`, exits after `waitTimeoutS` idle seconds |
| VM cost | DEV1-M 0.0272 EUR per hour by price list (VM 0.020196, IPv4 0.005, 40 GB local SSD 0.00196), about 0.030 in practice because the SSD bills per clock hour |
| VM network | VMs need IPv4: `registry.gitlab.com` and the Scaleway APIs have no IPv6 address (2026-10-24); a VM accepts no inbound connection |
| Instance quotas | per organization, zone and commercial type, resource `cp_servers_type_<TYPE>` with `_` for `-` (`cp_servers_type_DEV1_M`); read with `GET https://api.scaleway.com/iam/v1alpha1/quota/cp_servers_type_<TYPE>?organization_id=<org id>` |
| Serverless Jobs limits | 6 vCPU, 16 GB, 10 GB disk, 24 h per run, 400 parallel runs per organization, `linux/amd64`, images under 2 GB recommended; no privileged mode, no Docker daemon; `/tmp` lives in memory and counts against the run's memory |
| Serverless Jobs cost | 0.00001 EUR per vCPU-second and 0.000002 EUR per GB-second, billed per second: about 0.10 EUR per hour at 2 vCPU and 4 GB, 0.20 at 4 vCPU and 8 GB; free each month: 200,000 vCPU-seconds and 400,000 GB-seconds. A run bills its bootstrap and its idle `waitTimeoutS` too |
| Function | a deploy takes 5 to 13 minutes, Scaleway compiles the zip; Cockpit holds no function logs (2026-10-24) |

## Developer setup

1. Config: copy `config.example.json` to `.local/config.json`; set `name`, `gitlab.projectIds`, `vm` and the host's `lanes` (spec section 5.4).
   Check: `ciburst plan --config .local/config.json --skip-gitlab` reports no config error; it stops at `SCW_SECRET_KEY missing in .local/scaleway.env` until step 2.
2. Scaleway: in the console, IAM, an application with a policy on the organization holding `IAMManager`, `ProjectManager`, `InstancesFullAccess`, `ServerlessJobsFullAccess`, `FunctionsFullAccess` and `ObjectStorageFullAccess` (`ContainerRegistryFullAccess` only with `provider.registry`), and an API key. Write `SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID` (optional `SCW_DEFAULT_PROJECT_ID`) to `.local/scaleway.env`, then `chmod 600 .local/scaleway.env`.
   Check: the plan of step 1 prints the provider and host steps without an authentication error.
3. GitLab: a personal access token of a project Owner with the scopes `api` and `create_runner` and an expiry. Write `GITLAB_TOKEN=<token>` to `.local/gitlab.env`, then `chmod 600 .local/gitlab.env`.
   Check: `ciburst plan --config .local/config.json` prints runner and hook steps.
4. Agent: `scripts/agent-binary.sh` writes the Linux musl agent to `.local/ciburst-vm-agent`.
   Check: the plan lists the agent object as `create`, or `noop` when this build is already uploaded; the runner object (`runner-binary`) as `create`, or `noop` once uploaded.
5. `ciburst apply --config .local/config.json`.
   Check: exits 0; `.local/outputs.env` holds `CIBURST_DISPATCH_URL`.
6. `ciburst plan --config .local/config.json`.
   Check: only `noop` lines.
7. `ciburst check --config .local/config.json` (`--ref <branch>` lints a branch before it is merged; `--suggest` proposes lanes, section Lanes).
   Check: `every job matches its lanes and the VM runner` for every project.
8. `ciburst status --config .local/config.json`.
   Check: the dispatcher URL, `vm runner <tag>: <n> live of <maxRuns>` and a line per lane.

`.local/` is gitignored and never committed:

| File | Written by | Mode | Content |
|---|---|---|---|
| `config.json` | developer | 644 | config with the real project ids |
| `scaleway.env` | developer | 600 | Scaleway key |
| `gitlab.env` | developer | 600 | `GITLAB_TOKEN`; a `GITLAB_TOKEN` in the environment wins |
| `ciburst-vm-agent` | `scripts/agent-binary.sh` | 755 | the agent `apply` uploads |
| `outputs.env` | `ciburst apply` | 600 | `CIBURST_DISPATCH_URL`, `CIBURST_PROJECT_ID`, `CIBURST_HOOK_TOKEN`, `CIBURST_SWEEP_TOKEN`, `CIBURST_VM_SECRET`, `CIBURST_FLEET_KEY`, `CIBURST_DISPATCH_ACCESS_KEY`, `CIBURST_DISPATCH_SECRET_KEY`, `CIBURST_CACHE_ACCESS_KEY`, `CIBURST_CACHE_SECRET_KEY`, `CIBURST_AGENT_SHA256`, `CIBURST_RUNNER_ID_<TAG>` and `CIBURST_RUNNER_TOKEN_<TAG>` for the VM runner and each host lane; `CIBURST_REGISTRY_ENDPOINT` with a registry |
| `state.json` | `ciburst` | 600 | state of the fake provider, host and fleet |

## Adopter setup

The adopter follows README, Adopt. The operator's view:

1. Component: an `include:` entry `component: gitlab.com/ciburst/ciburst/ciburst@main` (`@1` after the first catalog release) in the project's `.gitlab-ci.yml`.
   Check: a default-branch pipeline shows `ciburst-setup`, `ciburst-check`, `ciburst-destroy` and `ciburst-invite` as manual jobs in `.pre`.
2. Variables: `CIBURST_SCW_ACCESS_KEY`, `CIBURST_SCW_SECRET_KEY`, `CIBURST_SCW_ORGANIZATION_ID`, `CIBURST_GITLAB_TOKEN`; optional `CIBURST_SCW_PROJECT_ID` (an existing Scaleway project instead of a new one). Each protected, masked and hidden, environment scope `ciburst`.
   Check: Settings, CI/CD, Variables lists them with environment `ciburst`.
3. Run `ciburst-setup`.
   Check: the log prints the plan and a `replayed` line per waiting job; those jobs run on the runner `<name>-vm`; a second run prints only `noop`. Note the job's duration: it counts against the hosted-runner minutes.
4. Run `ciburst-check`.
   Check: `every job matches its lanes and the VM runner`.
5. Undo: the project variable `CIBURST_DESTROY=<name>` (protected, environment scope `ciburst`), then run `ciburst-destroy`.
   Check: `destroyed <name>`; no output variable, webhook or runner remains; delete `CIBURST_DESTROY` afterwards. A `warn` line names what is left; a second run removes it. Remove the include only after this: a revert alone leaves the webhook, and every later job still starts a VM.

Project variables after a setup, environment `ciburst`, protected, raw: the keys of `outputs.env` above. Tokens, secrets and secret keys are masked and hidden where GitLab allows; ids, URLs and the agent hash stay readable in job logs.

Where to look when a job waits:

1. The job's tags. A job without the tag of the runner its name routes it to (its lane's, else the VM runner's) waits for another runner; `ciburst-check` names it.
2. The webhook's recent events (Settings, Webhooks, Edit, Recent events): the job's pending delivery and its answer (section Dispatcher answers). No delivery: the webhook is missing or disabled (section Webhook health); `ciburst-setup` re-verifies the host's and replays waiting jobs, `ciburst-join` recreates a member's missing webhook.
3. `started` or `grouped` for a VM job: a VM boots in about a minute; its tags go from `starting` to `idle` or `busy`.
4. `queued`: the job waits for a busy slot; a VM starts for it when the wait would pass `maxQueueS` (section Queue first).
5. `claimed`: a polling slot of the VM runner takes the job within seconds; if none does, read the VMs' tags (section VM runtime and the sweep).
6. `started` for a lane: the run takes the job within 5 to 28 s (93 s at 6 vCPU) plus its bootstrap. A job still pending after a minute: `ciburst status` shows the run; a `Failed` run's log names the cause with `ciburst-lane:` (section Failure modes).
7. `at_capacity`: `maxRuns` lane runs of the lane run; the job starts when a run takes it or ends.
8. `ignored` with reason `no_runner`: the member registered no VM runner; run `ciburst-join` there.
9. `500`: the start failed; section Failure modes.
10. Jobs created before the first setup: `ciburst-setup` replays jobs pending for more than 60 s.

## Dispatcher answers

| Status | `result` | Meaning |
|---|---|---|
| 200 | `claimed` (`runId`) | a free slot on a VM created more than 30 s ago; `runId` names the VM |
| 200 | `queued` (`etaS`) | a VM job behind busy slots: it waits for the first slot that frees; `etaS` is the expected wait, `null` when no running job will free a slot (then a VM starts at the next evaluation) |
| 200 | `tracked` (`status`) | a running or finished VM or lane job; feeds the queue and the typical durations |
| 200 | `started` (`runId`) | VM job: it leads its burst group and started a VM with the group's slots taken; lane job: a lane run started, `runId` names it |
| 200 | `grouped` (`leader`) | rides on the VM its leader's delivery starts; any polling slot of the VM runner takes it, GitLab does not bind it to that VM |
| 200 | `duplicate` | GitLab delivered this job's pending event again within 30 s; nothing started |
| 200 | `at_capacity` | a lane at its `maxRuns` (VM jobs queue instead); the job waits for a run |
| 200 | `ignored`, reason `not_job_event` | neither a job nor a pipeline event |
| 200 | `ignored`, reason `status` (`status`) | a job event the dispatcher does not act on |
| 200 | `ignored`, reason `project` | a project that is not a member |
| 200 | `ignored`, reason `own_job` | a job named `ciburst-*`, the component's own jobs |
| 200 | `ignored`, reason `ignore_jobs` | a job named in its project's `ignoreJobs`, which runs on other runners |
| 200 | `ignored`, reason `no_runner` (`runner`, `kind`) | a VM job of a member that registered no VM runner |
| 200 | `ignored`, reason `pipeline_closed` (`pipeline`) | a pending event GitLab delivered after its pipeline had ended, for a job that existed before the end and was not manual (D56); nothing starts |
| 200 | `warm` (`pipeline`, `runners[]` with `runner`, `kind`, `action`, `vmJobs`, `runId`) | a pipeline event with VM jobs: `started` (a warm VM boots with every slot free), `capacity` (a VM with a free slot exists or one is starting), `at_capacity`, `disabled` (`warmStart` false) |
| 200 | `ignored`, reason `no_vm_jobs` | a pipeline event whose open jobs all run in lanes or elsewhere |
| 200 | `duplicate` (`pipeline`) | a pipeline event for a pipeline whose record exists; nothing started |
| 200 | `closed` (`pipeline`) | a pipeline event with a final status; the pipeline's records are deleted |
| 200 | `reported` (`nextReportS`, `runners`), `terminated`, `swept` (`runs`) | a VM report (when to report next, the members' VM runners), a VM's termination, the cron sweep |
| 400 | `bad_request` | the body is not JSON, or a VM report is malformed |
| 401 | no body | wrong or missing webhook secret, hook, VM or sweep token |
| 500 | `error` | the provider failed (start, report, terminate, sweep) or the function's environment is incomplete; the body carries nothing more |

Every answer with a body carries `ms`, the dispatcher's elapsed milliseconds; GitLab.com counts an answer after 10 s as a failed delivery and disables the webhook after four failures in a row (section Webhook health). Every 200 answer of a job event except `ignored` also names the `runner` and its `kind`: the VM runner's tag with `vm`, or the lane's tag with `lane`. Example: `{"result":"started","runner":"node24","kind":"lane","runId":"<run id>","ms":1840}`. A `grouped` job depends on its leader's delivery: `started` (the VM boots), `queued` or `at_capacity` (the group waits for a slot) or 500 (the group waits for the next VM). Lane jobs never answer `grouped`, `duplicate` or `queued`: lanes use neither the burst ledger nor the queue. The fleet endpoints answer as in section Fleet members.

## Fleet members

Rules: spec section 8.

- State: `ciburst/fleet/config.json` (the host's config with a MAC; `ciburst-setup` writes it, the dispatcher reads it within 30 s) and `ciburst/fleet/members/<project id>.json` (webhook secret, VM runner token, lanes, `ignoreJobs`; secrets sealed with AES-256-GCM under `CIBURST_FLEET_KEY`). These keys never expire; never edit them by hand: a forged config or record makes the reads fail (`warn fleet_failed`).
- Invite: `ciburst-invite` with `CIBURST_INVITE`, or `ciburst invite <project>`; the code carries the dispatcher URL, the project id and a 7-day expiry, signed with the hook token. Rotating the hook token (`ciburst rotate hook`) invalidates open codes; joined members keep the webhook secret stored at their join.
- VMs serve every member: each VM runs one `gitlab-runner run` with an entry per member VM runner, from the `runners` of the report answer.
- Status: `ciburst status` prints `member <id> joined <time>`, each member's lanes and deliveries; `ciburst check` lints every member against its own lanes; `ciburst-destroy` refuses with `fleet <name> has members <ids>: run ciburst-leave there first`.

Join a project:

1. The project's own `ciburst.json`, when it gets lanes or has jobs on other runners: only `lanes` and `ignoreJobs`.
   Check: no other top-level key; a fleet setting fails the join with `<key>: a fleet setting; it belongs in the host's ciburst.json`.
2. In the host, `CIBURST_INVITE=<project id or path>`, then `ciburst-invite`.
   Check: `join code for project <id>, valid until <time>`.
3. In the member, `CIBURST_JOIN` (the code) and `CIBURST_GITLAB_TOKEN`, both protected, masked and hidden, scope `ciburst`; then `ciburst-join`.
   Check: the log shows `info runner tag="<vm tag>"`, `info runner tag="<lane tag>"` per lane, `info hook`, then `lane <tag> (<image>)` per lane and `joined fleet <name>`; the member's Runners page lists `<name>-<vm tag>` and `<name>-<lane tag>`; the console lists the job definitions `<name>-<project id>-<lane tag>`.
4. A member pipeline.
   Check: the deliveries answer with `runner` `<vm tag>` and `kind` `vm` for VM jobs, `<lane tag>` and `lane` for lane jobs; `ciburst status` in the host lists `lane <tag> of project <id> (<image>)`.

Change a member's lanes or `ignoreJobs`: edit its `ciburst.json`, then `ciburst-join` again (a new code from the host when the old one expired). `/join` updates changed definitions and deletes those of removed lanes; `ciburst-join` replaces the member's runners.

`/join` answers, shown in the failing `ciburst-join` log as `POST /join: <status> <body>`:

| Answer | Cause | Action |
|---|---|---|
| 401, `the dispatcher refused the join code on /join: expired or of another fleet` | the code expired, is forged, or names another project | a new code from the host's `ciburst-invite` |
| 400 `refused`, reason `host` | the code is for the host itself | the host needs no join |
| 400 `bad_request` | no VM runner token in the body | run `ciburst-join` again |
| 400 `unknown_runners` (`runners`) | a runner tag other than the fleet's VM runner tag | `vm_tag` of the member's component equals the host's `vm.tag`; run `ciburst-join` again |
| 400 `invalid_lanes` (`errors`) | a lane or `ignoreJobs` entry fails validation or exceeds Scaleway's limits, each error naming its field | fix the member's `ciburst.json`, run `ciburst-join` |
| 503 `no_fleet` | the dispatcher has no fleet state | run `ciburst-setup` in the host |
| 500 `error` (`step`) | a step failed: `definitions` (creating or deleting the member's job definitions, for example an image Scaleway refuses) or `record` (writing the member record) | for `definitions` check each lane's `image` (pullable, `linux/amd64`) and sizes; run `ciburst-join` again |
| 504 `timeout` (`step`) | the step did not finish within 25 s | run `ciburst-join` again; the steps are idempotent |

Leave: `ciburst-leave` in the member posts `/leave` (an expired code still leaves), which deletes the member's job definitions and record, then deletes its webhook and runners and prints `left fleet <name>`. `/leave` fails with the same `500 error` or `504 timeout` and `step`.

## Migrate a deployment to the VM runner and project lanes

A config with `pools`, `defaultPool`, `stages`, `vmFirst`, `spillRunner` or `providers` is refused; there is no automatic migration.

1. Host `ciburst.json`: replace `pools` by `vm` and `lanes`. `vm` takes the `pools` entry with runtime `instances`: its `tag`, `image`, `slots`, `instanceTypes`, `maxRuns`, `maxQueueS` and `warmStart`, without `runtime`, `jobs` and `stages`. `lanes` takes each entry with runtime `jobs`: its `tag`, `image`, `jobs`, sizes and `maxRuns`, without `runtime`, `stages` and `vmFirst`. Remove `defaultPool`, `spillRunner` and `providers`. A job that a lane took by stage needs its name in that lane's `jobs`.
   Check: `ciburst plan` (or the setup's first lines) reports no config error.
2. Component input: `pool` is now `vm_tag`, in the host and in every member; leave it out for the default `vm`.
   Check: the CI file validates.
3. Run `ciburst-setup` in the host.
   Check: the plan creates `definition:<project id>-<tag>` and deletes the old `definition:<tag>` per host lane, sets the runner `<name>-<vm tag>` to `set tags <vm tag>`, deletes `<name>-<vm tag>-spill` when it exists, and disables host lane runners in the further projects of `gitlab.projectIds`; `ciburst-check` passes.
4. Each member that used the host's lanes: its own `ciburst.json` with its `lanes` (same tags as before, so its jobs keep their `tags:`) and its `ignoreJobs`, then `ciburst-join`.
   Check: `lane <tag> (<image>)` per lane and `joined fleet <name>`; a member pipeline runs its lane jobs on `<name>-<lane tag>` of its own project.
5. Remove from the host's `ignoreJobs` the names of members' jobs, and from its `lanes` any lane only members used; `ciburst-setup`.
   Check: `ciburst status` lists each lane under its own project.

## Webhook health

GitLab disables a webhook after four failed deliveries in a row, and GitLab.com counts an answer after 10 s as failed. A disabled webhook delivers nothing, so pending jobs wait without a trace.

1. Detect: `ciburst check` (the `ciburst-check` job) prints `project <id>: webhook <alert_status> until <time or never>: run ciburst-setup to enable it again` and exits 1; `ciburst status` prints `hook <id>: <alert_status>`.
2. Repair: `ciburst-setup` (or `ciburst apply`). The plan shows `update  gitlab hook in project <id>  (re-enable: <alert_status> until <time or never>)`; `apply` sends GitLab's test delivery, which the dispatcher answers 200, logs `hook_reenabled` and replays the waiting jobs. A member's hook: in the member, Settings, Webhooks, the hook, Test, which sends a delivery the dispatcher answers 200.
   Check: `ciburst status` prints `hook <id>: executable`; the waiting jobs start.
3. Cause: the deliveries in `ciburst status` show the failed answers and their `ms`.

## Logs

Every line reads `<level> <message> key="value" ...` and never holds a secret.

| Source | Where | Lines |
|---|---|---|
| Dispatcher | the function's stderr, which Cockpit does not show (2026-10-24): read the answers on the webhook's recent events instead | `info` `claimed`, `started`, `grouped`, `duplicate`, `at_capacity`, `queued` (`eta_s`), `tracked` (`status`, `duration_s`), `stale`, `warm`, `closed` with `project`, `pipeline`, `job`, `stage`, `runner`, `run`; `queue_started` (`vms`, `needing`), `reported` (`state`, `free`, `next_report_s`), `joined` (`lanes`), `left`; `warn` `claim_failed`, `ledger_failed`, `schedule_failed` (`op`), `records_failed` (`op`), `capacity_failed`, `fleet_failed` (`op`), `join_refused` (`route`, `reason`), `start_retry` (a 429 or 503 of the provider, retried once after 300 ms within 5 s), `start_lookup` (a VM start looked up its image or security group: a type not in `CIBURST_VM_IMAGES`, or a create refused for a removed image; the next `ciburst apply` resolves them again), `unauthorized`; every line ends with `ms`; `error` `start_failed`, `report_failed`, `terminate_failed`, `sweep_failed`, `join_failed` and `leave_failed` (`step`), and `env` naming a missing variable |
| CLI | stderr; in pipeline mode the job log | one line per action; `plan` warns `vm runner cannot start reason="..."`; `apply` logs `runner`, `runner_tags`, `runner_deleted`, `runner_disabled`, `hook`, `hook_reenabled`, `fleet_config`, `member`, `replayed` per replayed job, `replay_failed` and `output_unmasked`; `ciburst-join` logs `runner` and `hook`, `ciburst-leave` `hook_deleted` and `runner_deleted` |
| Deliveries | `ciburst status` with a GitLab token: each webhook's 20 newest deliveries, oldest first | `delivery <time> <job name, pipeline <id> or event kind> <http status> <result> <runner> <ms>`; `-` where the answer lacks the key, `?` for an unreadable body |
| Lane run | the job run's stdout and stderr on Serverless Jobs; `ciburst status` lists the run and its state | `ciburst-lane: <reason>` before the bootstrap exits 64 (section Failure modes), then `gitlab-runner`'s own lines |
| VM agent | the VM's journal, `journalctl -u ciburst-vm-agent`, through the provider's console view only | `agent started`, `proxy listening address=<bridge address>:8780`, `proxy failed`, `forward failed method=... path=...`, `state from=... to=...` (`starting`, `idle`, `busy`, `draining`, `quitting`), `config written runners=<n>`, `runner started pid=...`, `runner exited code=...` (a warning when not draining; the agent starts it again), `runner start failed`, `config write failed`, `docker ps failed`, `report failed`, `report answer unreadable`, `terminate accepted`, `terminate failed`, `terminate gave up` |

## Rotation and updates

| Command | Replaces | Note |
|---|---|---|
| `ciburst rotate runner` | the host's runners and tokens (deleted and recreated): the VM runner's and each host lane's | VMs and lane runs started before hold the old token: rotate while none runs. Members keep theirs; a member renews its runners with `ciburst-join` |
| `ciburst rotate hook` | the hook token and the sweep token | updates the host's webhooks, the function and the cron; open join codes stop working |
| `ciburst rotate key` | the dispatch key | the old key is deleted, the function redeployed |
| `ciburst rotate cache` | the cache key | the old key is deleted; VMs and lane runs started before lose the cache: rotate while none runs |
| `ciburst rotate vm` | the VM secret | refused while VMs run, because their tokens would stop working |

Check after each: `ciburst plan` prints only `noop`; a test pipeline passes.

1. Yearly runtime bump: the new Rust version in `rust-toolchain.toml` and the host runtime (`RUNTIME` in `crates/scaleway/src/host.rs`), then in every file `scripts/check-pins.sh` lists (it runs in the `lint` job); tag a release; `ciburst apply`.
   Check: `lint` passes; the function is `ready`; a test pipeline passes.
2. Runner bump: the new version and its SHA-256 (from the release's `release.sha256`) in `RUNNER_URL` and `RUNNER_SHA256` of `crates/core/src/cloud_init.rs`, then in every file `scripts/check-pins.sh` lists; `ciburst apply`.
   Check: the plan shows `create` for the runner object (`runner-binary`) and `update` for every host `definition:<project id>-<tag>`; members update theirs with `ciburst-join`; VMs created and lane runs started afterwards run the new version.
3. Agent: after a change in `crates/vm-agent`, `scripts/agent-binary.sh`, then `ciburst apply`; in pipeline mode a new `cli` image carries the agent and `ciburst-setup` uploads it. VMs created afterwards download the new object.
   Check: `CIBURST_AGENT_SHA256` in the outputs changed; the plan lists `delete vm agent ciburst/agent/<sha256>` for each agent older than the two before it.

## VM runtime and the sweep

Rules: spec sections 6.1, 6.2 and 6.8.

- cloud-init writes `/etc/ciburst/vm.env`, downloads `gitlab-runner` 18.11.4 from the runner object and the agent from the agent object, checks each SHA-256, then starts `ciburst-vm-agent.service` (`Restart=on-failure`, 5 s).
- The agent reports on every change and at the latest after the answer's `nextReportS` (15 s while VM jobs queue, 30 s while busy, else 600 s); a failed report is retried every 5 s without limit. After the drain it posts `/terminate`, retried for 10 minutes (2 s doubling to 60 s); then it exits 1 and systemd restarts it.
- A slot that pulls its job's image has no labelled container yet and counts as free, 10 to 50 s per job.
- Tags: `ciburst-state:<state>`, `ciburst-free:<n>`, `ciburst-seen:<unix time>` and, once draining, `ciburst-drain:<unix time>`, beside `ciburst`, `<name>`, `runner:<vm tag>` (`pool:<vm tag>` on a server started before D66), `job:<id>`. Read them in the console (Instances, the server, Tags) or with `GET https://api.scaleway.com/instance/v1/zones/<zone>/servers?tags=<name>` and the header `X-Auth-Token: <secret key>`.
- The cron `<name>-sweep` runs at minutes 0 and 30 (UTC) and ends a VM older than 24 hours, a VM that has not reported 10 minutes after its creation, and a VM whose last report is more than 30 minutes old. A VM that reported within 30 minutes and is younger than 24 hours stays.

End a VM by hand, when the sweep does not:

1. Find the server: console Instances filtered by the tag `<name>`, or the `GET` above.
   Check: its `ciburst-seen` is older than 30 minutes, or no VM job runs on it in GitLab.
2. Terminate it: console, the server, Delete with its volumes; or `POST https://api.scaleway.com/instance/v1/zones/<zone>/servers/<server id>/action` with the body `{"action": "terminate"}`. Never `poweroff` from inside the VM: standby is billed as running.
   Check: the server is gone from the list and no IP of it remains under Flexible IPs.
3. A job that ran on it fails once GitLab stops receiving its updates; re-run it.

## Burst grouping

Rules: spec section 6.3. The function writes and lists intents with the cache key; a read follows at most 20 pages of 1000 keys.

Inspect the VM runner's intents:

1. Console: Object Storage, bucket `<name>-cache-<project id>`, folder `ciburst/intents/<vm tag>/`.
2. CLI with `CIBURST_CACHE_ACCESS_KEY` and `CIBURST_CACHE_SECRET_KEY` from the outputs as the key pair: `aws s3api list-objects-v2 --endpoint-url https://s3.<region>.scw.cloud --region <region> --bucket <name>-cache-<project id> --prefix ciburst/intents/<vm tag>/ --start-after ciburst/intents/<vm tag>/<unix seconds>`.
   Check: a cold burst of N jobs at `slots` S shows N keys within about 2 s and `ceil(N / S)` new servers.
3. Never delete intents by hand. A key outside the format is ignored; a forged key in the format costs at most one VM-hour.

Warm start (spec section 6.5): a warm intent is `<unix seconds>-warm-<pipeline id, 20 digits>`, and the pipeline's records live in `ciburst/intents/_pipelines/<pipeline>/` (`vm-jobs.json`; earlier versions also wrote `seen/<job>`) until its final status, which writes `ciburst/intents/_closed/<pipeline>`. Check: after a pipeline event with VM jobs `vm-jobs.json` exists; after the pipeline the folder is gone.

Known limits, each at most one VM-hour: a VM created in the 2 to 3 s after its intent left the 30 s window counts neither as old nor by rank, so a new burst at that instant can pass `maxRuns` by one; a repeated delivery within the same second is not recognised as a duplicate.

## Queue first

Rules: spec section 6.4.

- A VM job that finds every slot busy answers `queued` and waits; the slot that frees first takes it. A VM starts for it only when its wait plus the expected remainder of the running job it waits for would pass `vm.maxQueueS` (default 90 s), or when no running job will free a slot; `queue_started` logs those starts. `"maxQueueS": 0` starts a VM at once.
- Typical durations come from the job events: `ciburst/intents/_stats/<key>/<16 hex>` holds `{name, typicalS, runs}` per job name, the key being the VM runner's tag or a lane key. A new job name counts as ending now until it has finished once.
- Queue state in the cache bucket: `ciburst/intents/_queue/<vm tag>/<job>` (waiting), `_running/<key>/<job>` (running), `_queue/<vm tag>/.started` (start marker, 15 s). Never edit them by hand; entries older than 30 minutes (waiting) or 24 hours (running) are dropped when read.
- Busy VMs report every 30 s, every VM every 15 s while VM jobs queue; each report re-evaluates the queue.
- Check the queue: `aws s3api list-objects-v2 ... --prefix ciburst/intents/_queue/<vm tag>/` with the cache key (section Burst grouping).

## Lanes

Rules: spec section 7. A lane is one project's Serverless Jobs definition for one image and size: a job that needs no Docker daemon runs in the lane's image, billed per second.

Add lanes to a project:

1. Proposal: `ciburst check --suggest` (developer path: `--config .local/config.json`).
   Check: exits 0 and prints, per project and image of the Docker-free jobs on the VM runner, a lane for `lanes` and the `tags:` line of each job.
2. Config, in the project's own `ciburst.json` (the host's, or a member's):
   ```json
   {
     "lanes": [
       { "tag": "node24", "image": "node:24-bookworm-slim", "jobs": ["lint", "typecheck", "unit-*"], "vcpu": 2, "memoryMiB": 4096, "maxRuns": 10 }
     ]
   }
   ```
   `jobs`: job names with `*` (any characters) and `?` (one character); a pattern belongs to one lane. `image`: `linux/amd64` with `/bin/sh` and coreutils or BusyBox (`base64`, `sha256sum`), pullable without credentials (section Private lane images).
   Check: in the host `ciburst plan` lists `job definition <name>-<project id>-node24` and the runner `<name>-node24`.
3. Jobs: `tags: [<lane tag>]`, and `image:` exactly the lane's `image`:
   ```yaml
   lint:
     image: node:24-bookworm-slim
     tags: [node24]
     script:
       - npm ci
       - npm run lint
   ```
   Check: `ciburst check` reports no problem.
4. Host: `ciburst apply`, or `ciburst-setup`. Member: `ciburst-join`.
   Check: the project's Settings, CI/CD, Runners lists `<name>-node24` with the tag `node24`; a lane job's delivery answers `started` with `runner` `node24` and `kind` `lane`, and the job passes.

### Lane bootstrap

Every lane run executes `crates/core/src/lane-bootstrap.sh` (spec section 7.3) as the image's user:

| Image | Bootstrap |
|---|---|
| contains `git`, `curl` or `wget`, and CA certificates | installs nothing |
| Alpine, user root | `apk add` of what is missing: 2 to 4 s |
| Debian or Ubuntu, user root | `apt-get update` and `install` of what is missing, with lists and archives under `/tmp/ciburst-apt` and `APT::Sandbox::User=root`, because the sandbox drops root's capabilities: about 21 s on slim images |
| RPM-based (UBI, Fedora), or a non-root user | installs nothing; fails unless the image contains the tools |

Then it downloads `gitlab-runner` from `CIBURST_RUNNER_URL` to `/usr/local/bin` (`$HOME/.ciburst/bin` when that is not writable), checks it against `CIBURST_RUNNER_SHA256`, unsets the token and cache keys and execs `run-single` with the shell executor (`bash`, else `sh`) and the image's `PATH` as a job variable, builds in `/builds`, cache in `/cache`; a non-root user builds in `$HOME/builds` and caches in `$HOME/cache`, without a writable home under `/tmp`.

Try an image before it becomes a lane, from the repository root:

1. Dry run with every capability dropped, as in the sandbox:
   ```sh
   docker run --rm --platform linux/amd64 --cap-drop=ALL --entrypoint /bin/sh \
     -e CI_SERVER_URL=https://gitlab.com -e RUNNER_TOKEN=dry -e CIBURST_RUNNER_URL=dry \
     -e CIBURST_RUNNER_SHA256=dry -e CIBURST_LANE_DRY_RUN=1 \
     -v "$PWD/crates/core/src/lane-bootstrap.sh:/lane.sh:ro" <image> /lane.sh
   ```
   Check: the output ends with the `gitlab-runner run-single` command line, `--token [masked]` included; otherwise a `ciburst-lane:` line names what fails (section Failure modes).

### Private lane images

Serverless Jobs pull without credentials, so a lane image is public or lives in the deployment's registry namespace.

1. Host config: `"provider": { "type": "scaleway", "region": "<region>", "registry": true }`, without the lane yet; the owner key needs `ContainerRegistryFullAccess`. `ciburst apply`.
   Check: the output `CIBURST_REGISTRY_ENDPOINT` names the namespace.
2. Push the image to `<CIBURST_REGISTRY_ENDPOINT>/<image>:<tag>` with a Scaleway key that may write to the registry.
   Check: the console lists the image under Container Registry, namespace `<name>`.
3. Add the lane with that `image`, give its jobs the same `image:`, then `ciburst apply` (a member: `ciburst-join`). A job definition is refused while its image is missing.
   Check: `ciburst plan` prints only `noop`.

## Run-single flags of lanes

Source: `gitlab-runner run-single --help` in `gitlab/gitlab-runner:v18.11.4` (`images/test/runner-help.sh`) and `commands/single.go` at `v18.11.4`; every flag below is present there.

| Flag | Lane | Note |
|---|---|---|
| `--url`, `--token` | yes | the runner does not read `RUNNER_TOKEN`, so the token is a flag |
| `--executor` | `shell` | |
| `--shell` | `bash`, else `sh` | the lane image's shell |
| `--builds-dir /builds`, `--cache-dir /cache` | yes | `$HOME/builds`, `$HOME/cache` for a non-root user |
| `--max-builds 0` | yes | no build limit |
| `--wait-timeout` | `waitTimeoutS` | the timer restarts after every build |
| `--cache-type s3`, `--cache-shared`, `--cache-s3-server-address`, `--cache-s3-bucket-name`, `--cache-s3-bucket-location`, `--cache-s3-access-key`, `--cache-s3-secret-key` | yes | |
| `--env` | `PATH=<the image's PATH>`, `FF_USE_FASTZIP=true`, `CACHE_COMPRESSION_LEVEL=fastest` | D44: parallel cache archives with the fastest compression |

Job containers on VMs: the Docker executor labels build and service containers with `com.gitlab.gitlab-runner.job.id` (`executors/docker/internal/labels/labels.go`), which the agent counts. A runner whose requests fail waits `NotHealthyCheckInterval` (300 s) between requests (`common/consts.go`).

## Failure modes

| Symptom | Cause | Action |
|---|---|---|
| A VM job answers `queued` and waits longer than `maxQueueS` | no VM report arrived to re-evaluate the queue (a VM hangs), or the VM runner is at `maxRuns` | `ciburst status`: the VMs and their last reports; a hung VM ends by the sweep after 30 minutes; `ciburst-setup` replays waiting jobs |
| Jobs stay pending for minutes; the deliveries show `created` job events and pipeline events on time, `pending`, `running`, `success` much later | GitLab delivers job status events late (2026-10-26: about 35 minutes for hours) | VM slots that carry the job's tag take it meanwhile; late `pending` events of ended pipelines start nothing (`pipeline_closed`); `ciburst-setup` replays jobs pending longer than 60 s. Measure the delay: each delivery's `created_at` against the status time in its payload (`build_started_at`, `build_finished_at`) |
| `ciburst-setup` refuses the config naming `pools`, `defaultPool`, `stages`, `vmFirst`, `spillRunner` or `providers` | a config of an earlier version | section Migrate a deployment to the VM runner and project lanes |
| `ciburst-setup` is missing from the pipeline | the default branch is not protected, or the pipeline runs on another branch | protect the default branch; run the pipeline there |
| `ciburst-setup` refuses to run, naming `CI_COMMIT_REF_PROTECTED` | an unprotected ref: protected variables would be missing | run it on the protected default branch |
| `ciburst-setup` fails naming a `CIBURST_*` variable | the variable is missing, unprotected, or scoped to another environment | add it protected with environment scope `ciburst` |
| `ciburst-setup` cannot pull `registry.gitlab.com/ciburst/ciburst/cli:<version>`, or the include fails | while `ciburst/ciburst` is private: no access to it, or the project is missing from its job token allowlist | ask its maintainers for access and the allowlist entry |
| `ciburst-setup` fails with 403 on `POST /user/runners`, naming `create_runner` and the Owner role | the token lacks the scope, or its user is not an Owner of the project | a new token of a project Owner with `api` and `create_runner` in `CIBURST_GITLAB_TOKEN` |
| setup log `output_unmasked key=<key>` | GitLab refused to mask that value | none; the variable stays protected and scoped to `ciburst` |
| Running `ciburst-destroy` with a variable is refused (403), or the UI offers no variable form | new projects allow no pipeline variables, which include variables typed into a manual job | set `CIBURST_DESTROY` as a project variable instead |
| `ciburst-destroy` fails with `fleet <name> has members <ids>` | members joined the fleet | `ciburst-leave` in each member first |
| destroy log `warn servers still ending`, `security group not deleted` or `project not deleted` | a VM was still terminating after 5 minutes | run `ciburst-destroy` (or `ciburst destroy`) again |
| `ciburst-join` fails with `<key>: a fleet setting; it belongs in the host's ciburst.json` | the member's `ciburst.json` holds a fleet setting | keep only `lanes` and `ignoreJobs` there |
| `ciburst-join` fails with `pull access denied` | while `ciburst/ciburst` is private: the member is not on its job token allowlist | a maintainer adds the member's path under Settings, CI/CD, Job token permissions |
| `ciburst-join` fails with `POST /join: <status> ...` | section Fleet members, `/join` answers | as listed there |
| A job passes but logs `Failed to create cache` after `400 Bad Request ... InvalidRequest ... larger than the maximum allowable size for a copy source: 5368709120` | the cache archive exceeds 5 GiB, the largest object Scaleway's S3 lets the runner copy (2026-10-25: a Rust coverage build) | keep each cache under 5 GiB: cache fewer paths, prune build outputs such as test binaries before the job ends, or split the cache into several keys |
| apply log `replay_failed ... 403` | the token cannot list jobs (a fine-grained token without read access to jobs) | retry the waiting jobs by hand; use a token with the scope `api` |
| `apply` stops: the zone has no quota for the VM runner's types | Instance quota 0 for every type of `vm.instanceTypes` in the zone | set `provider.region` (or `provider.zone` within it) to one the message names, or ask Scaleway support to raise the quota; `ciburst apply`; delete the security group `<name>-vm` of the old zone (console, Instances, Security groups) |
| `apply` stops naming `--agent` and `scripts/agent-binary.sh` | no agent binary given and no agent object in the bucket | developer path: `scripts/agent-binary.sh`, then `ciburst apply`; pipeline mode: the `cli` image carries the agent, check the component's `version` input |
| Jobs stay pending, the webhook shows failures | GitLab disabled the webhook after four failures | section Webhook health |
| Deliveries answer 500, no server or lane run appears | the start failed: quota used up by other servers, provider credentials, a lane without its job definition | `ciburst plan` shows repairs; `ciburst apply`, in a member `ciburst-join`; quota as above |
| VMs stay `starting` until the sweep ends them | cloud-init failed: runner object or agent download, a hash mismatch, or an incomplete `vm.env` (the agent exits 64 naming the key) | fix the cause, `ciburst apply`, re-run the job; for a hash mismatch as in the row `gitlab-runner does not match its SHA-256` |
| A VM job stays pending while VMs idle | the job's name matches a lane's `jobs` pattern, so its project routes it to that lane | remove the name from the lane's `jobs`; `ciburst check` |
| VMs start or slots get claimed for jobs that run on GitLab's hosted runners | those jobs are not in their project's `ignoreJobs`, so they route to the VM runner | add them to `ignoreJobs` (`ciburst check --suggest` lists them); started VMs end at minute 55 |
| A member's VM job answers `ignored`, reason `no_runner` | the member's record holds no VM runner | `ciburst-join` in the member |
| A cold burst starts one VM per job; no delivery answers `grouped` | the ledger failed (`ledger_failed`): cache bucket missing, cache key rotated without `apply`, or more than 20 pages of intents | `ciburst apply` |
| A VM reports `free 0` while idle and runs no job; its journal repeats `proxy failed` | the agent's job request proxy cannot listen on the `docker0` bridge (Docker not up, the bridge has no IPv4 address, port 8780 taken), so it starts no runner (spec 6.2, D67) | `journalctl -u ciburst-vm-agent` names the error; the sweep or the drain ends the VM; a recurring cause belongs in the cloud-init |
| VM jobs take a VM's slot only after about 50 s; the runner logs `Worker starvation bottleneck` | the runner asks GitLab directly instead of through the agent's proxy, so GitLab holds each job request (D67) | VMs started with an agent before D67 | `ciburst-setup` (or `ciburst apply`) with a version that contains D67; VMs started after it run the new agent |
| VM jobs fail at artifact upload or download with `connection refused` to `172.17.0.1:8780` or another bridge address | the proxy stopped (journal `proxy stopped`), or a job's network cannot reach the host's bridge address | the journal names the error; the VM ends at the drain |
| A draining VM takes minutes to end | a runner cut off from GitLab waits 300 s between job requests, and `SIGQUIT` does not shorten that wait | none; the VM keeps reporting `draining` until its runner exits |
| `ciburst plan` reports a config error for a `jobs` pattern in two lanes | each pattern belongs to one lane | fix `ciburst.json`, run `ciburst-setup` (or `ciburst apply`, or in a member `ciburst-join`) |
| `ciburst check` names a lane job with `services:`, a `docker` image or a `docker ` script line | the job needs a Docker daemon, which lanes lack | keep the job on the VM runner: remove it from the lane's `jobs`, give it the VM runner's tag |
| `ciburst check` names a lane job whose `image:` differs from the lane's, or an unresolvable variable in it | the lane run would use another image than the job names | set `image:` to the lane's `image`, or define the variable in the pipeline's top-level `variables:` |
| Lane: job pending, a run started and exited idle | the job lacks the lane's tag | fix the job's `tags:` or the lane's `jobs`, `ciburst apply` or `ciburst-join`, `ciburst check`, re-run |
| Lane: run `failed` right after start without a `ciburst-lane:` line, job pending | the image cannot be pulled (missing, private outside the deployment's registry, not `linux/amd64`), or it has no `/bin/sh` or `base64` | fix the lane's `image` and the jobs' `image:`, `ciburst apply` or `ciburst-join`, re-run the job |
| `ciburst-lane: missing environment variable <NAME>` | job definition or run environment incomplete | `ciburst apply`, in a member `ciburst-join`; for the cache keys `ciburst rotate cache` |
| `ciburst-lane: the image lacks <tools> and runs as a non-root user` | the image misses `git`, `curl` or CA certificates, and its user cannot install packages | a root image, or one that contains the tools, as the lane's `image` and the jobs' `image:` |
| `ciburst-lane: apk could not install <tools>` | `apk add` failed: Alpine mirror unreachable, or the packages missing for the image's release | re-run the job; if it persists, an image that contains the tools |
| `ciburst-lane: apt-get could not install <tools>` | `apt-get update` or `install` failed: mirror unreachable, or the image's release archived | re-run the job; if it persists, a current release of the image or one that contains the tools |
| `ciburst-lane: the image lacks <tools>; RPM-based images must contain them, RPM cannot install in the sandbox` | an RPM-based image (UBI, Fedora) without `git`, `curl` or CA certificates | an image that contains them, for example one built on it and pushed to the deployment's registry (section Private lane images) |
| `ciburst-lane: the image lacks <tools> and has no apk, apt-get, microdnf or dnf` | no known package manager to install the tools with | an image that contains `git`, `curl` and CA certificates |
| `ciburst-lane: cannot download gitlab-runner` | the runner object is missing (deleted, or `apply` stopped before the `runner-binary` step) or the bucket is unreachable | `ciburst plan` shows `runner-binary` as `create`; `ciburst apply`; re-run the job |
| `ciburst-lane: gitlab-runner does not match its SHA-256` | the object at `CIBURST_RUNNER_URL` is not the pinned runner, for example overwritten by a job, which holds the cache key | delete `ciburst/runner/<sha256>` from the cache bucket, `ciburst apply`, re-run the job |
| `ciburst-lane: RUNNER_WAIT_TIMEOUT must be a whole number of seconds`, or `ciburst-lane: CACHE_TYPE must be s3 or empty` | the job definition's environment was edited outside ciburst | `ciburst apply`, in a member `ciburst-join`, restores it |
| Lane job: `tar: <file>: Cannot change ownership to uid <n>, gid <n>: Operation not permitted` | the sandbox drops root's capabilities, and `tar` as root restores the archive's owners | extract with `tar --no-same-owner` in the job's script |
| Lane: run ends `out of memory` | files in `/tmp` count against memory | raise `memoryMiB` or write to the builds dir |
| Lane: costs beyond the free tier | Serverless Jobs free tier exceeded | consumption page; per-second prices apply (section Facts) |
| Function deploy fails with an unknown runtime | Scaleway retired `rust196` | runtime bump (Rotation and updates) |

## Acceptance drill

Config: the VM runner `vm` (`slots` 2, `maxRuns` 4) in a zone with quota, the host lane `ci` (`image` `node:24-bookworm-slim`, `jobs` `["fast-lane"]`) and `"ignoreJobs": ["hosted"]`. Pipeline: `test/drill/gitlab-ci.yml` as the `.gitlab-ci.yml` of the drill consumer's branch `ciburst-drill`.

1. `ciburst apply`, then `ciburst plan`.
   Check: only `noop`; the security group `<name>-vm`, the cron `<name>-sweep`, the agent object, the runner object and the definition `<name>-<project id>-ci` exist.
2. `ciburst check --ref ciburst-drill`.
   Check: `every job matches its lanes and the VM runner`.
3. Cold pipeline: push an empty commit to `ciburst-drill` while no VM runs.
   Check: the five VM jobs start three VMs (deliveries `started` three times, `grouped` twice, or `warm` with `started` and claims); `node`, `rust`, `postgres`, `dind` and `docker-build` pass on the runner `<name>-vm`, `fast-lane` in a lane run on `<name>-ci`, `hosted` on a GitLab runner with its delivery `ignored`, reason `ignore_jobs`; `$CI_REGISTRY_IMAGE/ciburst-drill:<short sha>` exists.
4. Tags while the pipeline runs.
   Check: `starting`, then `idle` or `busy` with `ciburst-free` and a fresh `ciburst-seen`.
5. Warm pipeline within the VMs' paid hour.
   Check: `claimed` or `queued` for every VM job; no new server.
6. Drain.
   Check: each VM reports `draining` at minute 55 of its hour and is gone before minute 60; no server, IP or volume remains.
7. Sweep: in the VM zone create a powered-on server without user data, tagged `ciburst`, `<name>`, `runner:vm`, `ciburst-state:starting`.
   Check: the first sweep after its tenth minute terminates it; reporting VMs stay.
8. A member: a second project joins with its own lane (section Fleet members) and runs a pipeline.
   Check: its VM jobs share the host's VMs, its lane jobs answer `runner` `<lane tag>` with `kind` `lane`; `ciburst-leave` removes its definitions, runners and webhook.
9. Cost on the consumption page.
   Check: Instances, IPv4 and local SSD for the VM hours only; Serverless Jobs inside the free tier.

## Measured

| Measure | Value |
|---|---|
| VM boot: first VM job pending to running, DEV1-M | 47 to 96 s |
| Queue on warm VMs | 2 to 3 s |
| Cold burst of five VM jobs at two slots | three VMs |
| Cold plus warm pipeline within one hour | 0.09 EUR |
| Idle VM | `draining` at minute 55, gone within 2 minutes |
| Warm start (2026-10-26): a lane stage before a Docker job, no VM running | the warm VM booted to `idle` in 74 s; the Docker job of the next stage answered `claimed` on it and queued 3.4 s, against 61 to 96 s behind a cold VM boot |
| Queue first (2026-10-26): three 60 s VM jobs at once on one idle VM with two slots | two ran at once, the third took the first freed slot after 73 s in the queue; one VM in total |
| One `gitlab-runner run` for several runners (2026-10-26, `gitlab/gitlab-runner:v18.11.4`, Docker executor, two runners in one `config.toml`, `concurrent = 2`, four 60 s jobs) | never more than 2 jobs at once; each runner gave its parallel jobs the concurrency ids 0 and 1 (`runner-<token prefix>-project-<id>-concurrent-<n>`), so their volumes stay apart; a runner removed from the live `config.toml` was dropped about 1 s later and its jobs stayed pending; a runner added back took a waiting job 2 s after the change; `SIGQUIT` let the two running jobs finish and the process exited 22 s later |
| Fleet (2026-10-26): a member in another namespace | `ciburst-join` registered its runners, created the webhook and logged `joined fleet <name>` within 3 s; a host job and a member job ran on the same VM at once under their own runners; a routing-only config change (`ignoreJobs`) applied in a 27 s setup with `update fleet config` and the function `noop` |
| Job requests of several runners (2026-10-28, `gitlab/gitlab-runner:v18.11.4`, shell executor, three runners in one `config.toml`, a job for the third, six runs each) | `concurrent = 1`: queued 52 to 64 s, and the runner logs `Worker starvation bottleneck`; `concurrent = 4`: 0.5 to 2.4 s; `concurrent = 1` behind a proxy that drops `last_update`: 0.3 to 2.6 s, one job request per second answered `204` in about 300 ms (D67) |
| Fleet VM jobs before D67 (2026-10-28, a 1-slot VM serving three projects) | a VM job on the idle, running VM queued 73 and 100 s |
| Fleet VM jobs with D67 and D68 (2026-10-28, same pipeline, 1-slot VM serving three projects) | five VM jobs on one VM: the first 16 s on the booting warm VM, the others 1.8 to 2.4 s; no extra VM; artifacts, caches and Docker-in-Docker through the proxy unchanged; pipeline 1,077 s against 1,492 s |
| Dispatcher answer, warm | 0.1 to 0.4 s; a fleet refresh 0.6 to 1 s; a burst of 10 requests under 1.4 s |
| Dispatcher answer right after a function deploy (cold instances) | 3.4 to 5.3 s, every event answered |
| VM start answers on a warm function (2026-10-26) | 4,238 and 6,255 ms; the warm start 4,327 ms including the 2 s burst wait; lane starts 830 to 1,174 ms, claims 958 to 1,481 ms |
| Function after the hardening (2026-10-26) | `memory_limit` 1024, `cpu_limit` 560; a lane start 1,528 ms, the warm VM start 4,415 ms, a claim 1,644 ms, the closing pipeline event 4,540 ms right after the deploy; no answer above 10 s |
| Cache restore on the next pipeline | 0.3 s |
| Function deploy | 5 to 13 minutes |
| Setup in a fresh project (2026-10-25) | first `ciburst-setup` 6.1 hosted minutes including the dispatcher deploy; the second 16 s, only `noop`; `ciburst-check` 9 s; a setup that adds lanes and deploys about 10 hosted minutes |
| `ciburst-destroy` | 72 s; a VM still terminating can leave the security group and project for a second run (13 s) |
| CPU-bound job, DEV1-L with one slot against GitLab's small hosted runner (drill consumer, 2026-10-25) | 1.3 to 4 times as long: rust-check 135 s against 84 s, ts-check 245 s against 62 s, e2e 482 to 494 s against 231 to 299 s, image build 409 s against 303 s; lighthouse 80 s against 105 s |
| Cache transfer, bucket in the VMs' region | restore and archive of a Rust job in seconds; across regions 555 s for the same job (D43) |
| CPU per zone, one VM each at boot (2026-10-25): SHA-256 of 1 GiB on one core, then all cores | DEV1-M `pl-waw-1` and `pl-waw-2` EPYC 7282: 5.3 and 5.5 s; `nl-ams-2` EPYC 7282: 6.4 s; `nl-ams-1` EPYC 7281: 8.6 s; GP1-XS `nl-ams-1` EPYC 7401P: 9.8 s; GitLab's small hosted runner (Xeon 8581C): 5.5 s. `nl-ams-1` mixes both DEV1 hosts; boot to user data 18 to 37 s |
| Serverless Jobs `nl-ams`, start request to first command (2026-10-25) | 5 to 12 s small images, 19 to 28 s Rust and Playwright, 93 s at 6 vCPU |
| Same workloads: GitLab's small hosted runner, DEV1-M `nl-ams-1`, Serverless Job 2 vCPU, 4 vCPU | `cargo install ripgrep`: 56, 54, 52, 30 s; three TypeScript checks: 8.5, 14.3, 9.5 to 10.9 s; `npm install`: 1.4, 2.5, 2.7 to 3.0 s |
| Lane bootstrap in a Serverless Job | `apk add git` 2 to 4 s; `apt-get` with its own lists and archives directories (the sandbox drops root's capabilities) 21 s; images with `git` and `curl` need none |
| Lanes of the drill consumer (2026-10-26, lanes from `rust`, `alpine`, `node` and `playwright` images) | 10 lane runs, 2,222 s, 8,147 vCPU-s, 16,294 GB-s: 0.11 EUR at list price, inside the free tier; Chromium (Lighthouse) runs in the sandbox; a Rust test job peaked at 6.5 GB on the lane disk |
| `CIBURST_LANE` size, the lane `COMMAND` in `nl-ams` (2026-10-25) | job definitions with 6,728 to 87,528 characters accepted; dry runs on `alpine:3.21` and `node:24-bookworm-slim` succeeded |
| Scaleway bill, October to 2026-10-25 21:18 UTC (consumption API, amounts in cents per line) | 2.00 EUR in total, 1.97 of it VM-hours (compute, IPv4, local SSD); Serverless Jobs (29,556 vCPU-s, 63,472 GB-s) and Functions fully inside the free tier; Object Storage and registry 0.02 |
| Price per VM-hour from the bill | DEV1-M 0.021 + IPv4 0.005 + local SSD 0.002 = 0.028 EUR; DEV1-L 0.044 + 0.005 + 0.004 = 0.053 EUR; local SSD is billed per clock hour (960 GB-hours for 8 DEV1-L hours of 80 GB) |
| Cost per job-minute, drill consumer on VMs (3 pipelines, one DEV1-L per job, 2026-10-25) | 0.68 EUR for 146 job-minutes on 12 DEV1-L hours: 0.0046 EUR per job-minute, about half of GitLab's small runner (0.0089); the VMs were busy about 20 % of their paid hours |
| One paid hour per VM, the bill against the job logs (2026-10-25) | 8 VMs ran jobs, 8 DEV1-L hours billed, the longest ran jobs for 50 minutes across two pipelines; a VM woken by hosted-runner jobs' pending events costs one hour (D49). A VM busy at minute 55 pays its next hour, by design |
| GitLab job status webhook delay (2026-10-26, from the hook event log) | 1 to 3 s in the morning; 2,003 to 2,125 s from the afternoon on, for `pending`, `running` and `success`; `created` job events and pipeline events 1 to 4 s throughout |

## Decisions

| Date | Decision |
|---|---|
| 2026-10-25 | The lane bootstrap passes every value as a flag and unsets `RUNNER_TOKEN`, `CACHE_S3_ACCESS_KEY` and `CACHE_S3_SECRET_KEY` before `exec`, so job scripts do not inherit them. |
| 2026-10-24 | Releases use `glab release create` in `registry.gitlab.com/gitlab-org/cli:v1.124.0` with `GLAB_ENABLE_CI_AUTOLOGIN`; `release-cli` is deprecated since GitLab 18.0. |
| 2026-10-24 | Framework image builds use `docker:27.5.1` with `docker:27.5.1-dind`. |
| 2026-10-24 | VM jobs mount `/certs/client` besides `/cache`, so Docker-in-Docker with TLS (`DOCKER_TLS_CERTDIR: "/certs"`) works as on GitLab's hosted runners. |
| 2026-10-24 | The agent starts the runner with only `PATH` and `HOME` in its environment: the runner reads `CI_SERVER_URL`, `CACHE_*` and `DOCKER_*` from its environment, and `config.toml` stays the only source. |
| 2026-10-24 | `free` counts slots without a job; a draining VM reports `free` 0. The 30 s grace starts when the dispatcher accepted the `draining` report. |
