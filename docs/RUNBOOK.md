# Runbook

Rules and design: `docs/SPEC.md`. Below, `ciburst` stands for the release binary or `cargo run -p ciburst --`.

## Facts

| Item | Value |
|---|---|
| Region | `provider.region` (default `fr-par`) holds the dispatcher, the cache bucket, the agent and runner objects, the VMs and the lane runs; `provider.zone` defaults to `<region>-1` and must lie in the region |
| Scaleway resources | project `<name>`, IAM applications `<name>-dispatch` and `<name>-cache`, bucket `<name>-cache-<project id>`, namespace `<name>`, function `<name>-dispatch`, cron `<name>-sweep`; with VM pools the security group `<name>-vm` and servers `<name>-<tag>-<8 hex>`; with lanes job definitions `<name>-<tag>`; with `provider.registry` the registry namespace `<name>` |
| GitLab resources | runner `<name>-<tag>` per pool in the first project, enabled in the others; the runner of the VM pool that lanes borrow slots from also carries their tags; a webhook to the function URL in every project |
| Agent object | `ciburst/agent/<sha256>` in the cache bucket, public read: the musl `ciburst-vm-agent` |
| Runner object | `ciburst/runner/<sha256>` in the cache bucket, public read: `gitlab-runner` 18.11.4, which VMs and lane runs download and check against its sha256 |
| Burst intents | empty objects `ciburst/intents/<pool>/<unix s, 10 digits>-<job id, 20 digits>` in the cache bucket, expired by its lifecycle rules |
| Image | `registry.gitlab.com/ciburst/ciburst/cli` (the setup jobs, with `ciburst` and `ciburst-vm-agent`); `edge` from `main`, `X.Y.Z`, `X.Y`, `X` and `latest` from a tag `vX.Y.Z` |
| Binary | `ciburst-x86_64-unknown-linux-musl` in the framework project's generic package registry, linked from each release |
| Runner | `gitlab-runner` 18.11.4, `run-single`. VM pools: Docker executor, `slots` processes per VM. Lanes: shell executor in the lane's image, one run per pending job that found no free VM slot, exits after `waitTimeoutS` idle seconds |
| VM cost | DEV1-M 0.0272 EUR per hour by price list (VM 0.020196, IPv4 0.005, 40 GB local SSD 0.00196), about 0.030 in practice because the SSD bills per clock hour; Hetzner CX33 0.0144 with IPv4 |
| VM network | VMs need IPv4: `registry.gitlab.com` and the Scaleway APIs have no IPv6 address (2026-10-24); a VM accepts no inbound connection |
| Instance quotas | per organization, zone and commercial type, resource `cp_servers_type_<TYPE>` with `_` for `-` (`cp_servers_type_DEV1_M`); read with `GET https://api.scaleway.com/iam/v1alpha1/quota/cp_servers_type_<TYPE>?organization_id=<org id>` |
| Serverless Jobs limits | 6 vCPU, 16 GB, 10 GB disk, 24 h per run, 400 parallel runs per organization, `linux/amd64`, images under 2 GB recommended; no privileged mode, no Docker daemon; `/tmp` lives in memory and counts against the run's memory |
| Serverless Jobs cost | 0.00001 EUR per vCPU-second and 0.000002 EUR per GB-second, billed per second: about 0.10 EUR per hour at 2 vCPU and 4 GB, 0.20 at 4 vCPU and 8 GB; free each month: 200,000 vCPU-seconds and 400,000 GB-seconds. A run bills its bootstrap and its idle `waitTimeoutS` too |
| Function | a deploy takes 5 to 13 minutes, Scaleway compiles the zip; Cockpit holds no function logs (2026-10-24) |
| Hetzner | firewall `<name>-vm`, servers `<name>-<tag>-<8 hex>` with labels `ciburst-*`, run ids `hetzner:<server name>`; API limit 3600 requests per hour per project |

## Developer setup

1. Config: copy `config.example.json` to `.local/config.json`; set `name`, `gitlab.projectIds` and the pools (spec section 5.4).
   Check: `ciburst plan --config .local/config.json --skip-gitlab` reports no config error; it stops at `SCW_SECRET_KEY missing in .local/scaleway.env` until step 2.
2. Scaleway: in the console, IAM, an application with a policy on the organization holding `IAMManager`, `ProjectManager`, `InstancesFullAccess`, `ServerlessJobsFullAccess`, `FunctionsFullAccess` and `ObjectStorageFullAccess` (`ContainerRegistryFullAccess` only with `provider.registry`), and an API key. Write `SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID` (optional `SCW_DEFAULT_PROJECT_ID`) to `.local/scaleway.env`, then `chmod 600 .local/scaleway.env`.
   Check: the plan of step 1 prints the provider and host steps without an authentication error.
3. GitLab: a personal access token of a project Owner with the scopes `api` and `create_runner` and an expiry. Write `GITLAB_TOKEN=<token>` to `.local/gitlab.env`, then `chmod 600 .local/gitlab.env`.
   Check: `ciburst plan --config .local/config.json` prints runner and hook steps.
4. Hetzner, only for pools with `runtime: "hetzner"`: section Hetzner.
5. Agent: `scripts/agent-binary.sh` writes the Linux musl agent to `.local/ciburst-vm-agent`.
   Check: the plan lists the agent object as `create`, or `noop` when this build is already uploaded; the runner object (`runner-binary`) as `create`, or `noop` once uploaded.
6. `ciburst apply --config .local/config.json`.
   Check: exits 0; `.local/outputs.env` holds `CIBURST_DISPATCH_URL`.
7. `ciburst plan --config .local/config.json`.
   Check: only `noop` lines.
8. `ciburst check --config .local/config.json` (`--ref <branch>` lints a branch before it is merged; `--suggest` proposes lanes, section Lanes).
   Check: `every job matches the stage map` for every project.
9. `ciburst status --config .local/config.json`.
   Check: the dispatcher URL and a line per pool.

`.local/` is gitignored and never committed:

| File | Written by | Mode | Content |
|---|---|---|---|
| `config.json` | developer | 644 | config with the real project ids |
| `scaleway.env` | developer | 600 | Scaleway key |
| `gitlab.env` | developer | 600 | `GITLAB_TOKEN`; a `GITLAB_TOKEN` in the environment wins |
| `hetzner.env` | developer | 600 | `HCLOUD_TOKEN`, only with Hetzner pools |
| `ciburst-vm-agent` | `scripts/agent-binary.sh` | 755 | the agent `apply` uploads |
| `outputs.env` | `ciburst apply` | 600 | `CIBURST_DISPATCH_URL`, `CIBURST_PROJECT_ID`, `CIBURST_HOOK_TOKEN`, `CIBURST_SWEEP_TOKEN`, `CIBURST_VM_SECRET`, `CIBURST_DISPATCH_ACCESS_KEY`, `CIBURST_DISPATCH_SECRET_KEY`, `CIBURST_CACHE_ACCESS_KEY`, `CIBURST_CACHE_SECRET_KEY`, `CIBURST_AGENT_SHA256`, `CIBURST_RUNNER_ID_<TAG>` and `CIBURST_RUNNER_TOKEN_<TAG>` per pool; `CIBURST_REGISTRY_ENDPOINT` with a registry; `HCLOUD_TOKEN` with Hetzner pools |
| `state.json` | `ciburst` | 600 | state of the fake provider and host |

## Adopter setup

The adopter follows README, Adopt. The operator's view:

1. Component: an `include:` entry `component: gitlab.com/ciburst/ciburst/ciburst@main` (`@1` after the first catalog release) in the project's `.gitlab-ci.yml`.
   Check: a default-branch pipeline shows `ciburst-setup`, `ciburst-check` and `ciburst-destroy` as manual jobs in `.pre`.
2. Variables: `CIBURST_SCW_ACCESS_KEY`, `CIBURST_SCW_SECRET_KEY`, `CIBURST_SCW_ORGANIZATION_ID`, `CIBURST_GITLAB_TOKEN`; optional `CIBURST_SCW_PROJECT_ID` (an existing Scaleway project instead of a new one); `CIBURST_HCLOUD_TOKEN` with Hetzner pools. Each protected, masked and hidden, environment scope `ciburst`.
   Check: Settings, CI/CD, Variables lists them with environment `ciburst`.
3. Run `ciburst-setup`.
   Check: the log prints the plan and a `replayed` line per waiting job; those jobs run on the runner `<name>-vm`; a second run prints only `noop`. Note the job's duration: it counts against the hosted-runner minutes.
4. Run `ciburst-check`.
   Check: `every job matches the stage map`.
5. Undo: the project variable `CIBURST_DESTROY=<name>` (protected, environment scope `ciburst`), then run `ciburst-destroy`.
   Check: `destroyed <name>`; no output variable, webhook or runner remains; delete `CIBURST_DESTROY` afterwards. A `warn` line names what is left; a second run removes it. Remove the include only after this: a revert alone leaves the webhook, and every later job still starts a VM.

Project variables after a setup, environment `ciburst`, protected, raw: the keys of `outputs.env` above except `HCLOUD_TOKEN`, which stays in memory. Tokens, secrets and secret keys are masked and hidden where GitLab allows; ids, URLs and the agent hash stay readable in job logs.

Where to look when a job waits:

1. The job's tags. A job without the tag of the pool its name or stage selects waits for another runner; `ciburst-check` names it.
2. The webhook's recent events (Settings, Webhooks, Edit, Recent events): the job's pending delivery and its answer (section Dispatcher answers). No delivery: the webhook is missing or disabled; `ciburst-setup` re-verifies it and replays waiting jobs.
3. `started` or `grouped` for a VM pool: a VM boots in about a minute; its tags go from `starting` to `idle` or `busy`.
4. `started` for a lane: the run takes the job within 5 to 28 s (93 s at 6 vCPU) plus its bootstrap. A job still pending after a minute: `ciburst status` shows the run; a `Failed` run's log names the cause with `ciburst-lane:` (section Failure modes).
5. `claimed`, with `lane` for a lane job: a polling slot of the VM pool takes the job within seconds once one is free; if none does, read the VMs' tags (section VM runtime and the sweep).
6. `at_capacity`: `maxRuns` VMs or lane runs run; the job starts when a slot or run frees.
7. `500`: the start failed; section Failure modes.
8. Jobs created before the first setup: `ciburst-setup` replays jobs pending for more than 60 s.

## Dispatcher answers

| Status | `result` | Meaning |
|---|---|---|
| 200 | `claimed` (`runId`) | a free slot on a VM created more than 30 s ago; `runId` names the VM |
| 200 | `queued` (`etaS`) | a VM job behind busy slots: it waits for the first slot that frees; `etaS` is the expected wait, `null` when no running job will free a slot (then a VM starts at the next evaluation) |
| 200 | `spilled` (`vm`) | a lane job an idle slot of the paid VM `vm` takes; `pool` names the lane; a lane run starts if no slot took it within 20 s |
| 200 | `tracked` (`status`) | a running or finished VM or lane job; feeds the queue and the typical durations |
| 200 | `started` (`runId`) | VM pool: the job leads its burst group and started a VM with the group's slots taken; lane: a lane run started, `runId` names it |
| 200 | `grouped` (`leader`) | rides on the VM its leader's delivery starts; any polling slot of the pool takes it, GitLab does not bind it to that VM |
| 200 | `duplicate` | GitLab delivered this job's pending event again within 30 s; nothing started |
| 200 | `at_capacity` | `maxRuns` reached; the job waits for a free slot or run |
| 200 | `ignored`, reason `not_job_event` | neither a job nor a pipeline event |
| 200 | `ignored`, reason `status` (`status`) | a job event other than `pending` |
| 200 | `ignored`, reason `project` | a project not in `gitlab.projectIds` |
| 200 | `ignored`, reason `unmapped_stage` (`stage`) | no `jobs` pattern matches the job's name, no pool lists its stage and there is no `defaultPool` |
| 200 | `ignored`, reason `own_job` | a job named `ciburst-*`, the component's own jobs |
| 200 | `ignored`, reason `pipeline_closed` (`pipeline`) | a pending event GitLab delivered after its pipeline had ended, for a job that existed before the end and was not manual (D56); nothing starts |
| 200 | `ignored`, reason `ignore_jobs` | a job named in `ignoreJobs`, which runs on other runners |
| 200 | `warm` (`pipeline`, `pools[]` with `pool`, `action`, `vmJobs`, `runId`) | a pipeline event: per VM pool `started` (a warm VM boots with every slot free), `capacity` (a VM with a free slot exists or one is starting), `at_capacity`, `disabled` (`warmStart` false) |
| 200 | `ignored`, reason `no_vm_jobs` | a pipeline event whose open jobs all run in lanes or elsewhere |
| 200 | `duplicate` (`pipeline`) | a pipeline event for a pipeline whose record exists; nothing started |
| 200 | `closed` (`pipeline`) | a pipeline event with a final status; the pipeline's records are deleted |
| 200 | `reported` (`spill`, `nextReportS`), `terminated`, `swept` (`runs`) | a VM report (with whether idle slots may take lane jobs and when to report next), a VM's termination, the cron sweep |
| 400 | `bad_request` | the body is not JSON, or a VM report is malformed |
| 401 | no body | wrong or missing hook, VM or sweep token |
| 500 | `error` | the provider failed (start, report, terminate, sweep) or the function's environment is incomplete; the body carries nothing more |

Every answer with a body carries `ms`, the dispatcher's elapsed milliseconds; GitLab.com counts an answer after 10 s as a failed delivery and disables the webhook after four failures in a row (section Webhook health). Every 200 answer of a job event except `ignored` also names the `pool`. A `grouped` job depends on its leader's delivery: `started` (the VM boots), `at_capacity` (the group waits for a slot) or 500 (the group waits for the next VM). Lane jobs never answer `grouped` or `duplicate`: lanes do not use the burst ledger.

## Webhook health

GitLab disables a webhook after four failed deliveries in a row, and GitLab.com counts an answer after 10 s as failed. A disabled webhook delivers nothing, so pending jobs wait without a trace.

1. Detect: `ciburst check` (the `ciburst-check` job) prints `project <id>: webhook <alert_status> until <time or never>: run ciburst-setup to enable it again` and exits 1; `ciburst status` prints `hook <id>: <alert_status>`.
2. Repair: `ciburst-setup` (or `ciburst apply`). The plan shows `update  gitlab hook in project <id>  (re-enable: <alert_status> until <time or never>)`; `apply` sends GitLab's test delivery, which the dispatcher answers 200, logs `hook_reenabled` and replays the waiting jobs.
   Check: `ciburst status` prints `hook <id>: executable`; the waiting jobs start.
3. Cause: the deliveries in `ciburst status` show the failed answers and their `ms`.

## Logs

Every line reads `<level> <message> key="value" ...` and never holds a secret.

| Source | Where | Lines |
|---|---|---|
| Dispatcher | the function's stderr, which Cockpit does not show (2026-10-24): read the answers on the webhook's recent events instead | `info` `claimed`, `started`, `grouped`, `duplicate`, `at_capacity`, `queued` (`eta_s`), `spilled` (`vm`), `tracked` (`status`, `duration_s`) with `project`, `pipeline`, `job`, `stage`, `pool`, `run`; `queue_started` (`vms`, `needing`), `spill_fallback` (`lane`, `job`), `reported` (`spilling`, `spill`, `next_report_s`); `warn` `claim_failed`, `ledger_failed`, `schedule_failed` (`op`), `records_failed` (`op`), `capacity_failed`, `start_retry` (a 429 or 503 of the provider, retried once after 300 ms within 5 s), `start_lookup` (a VM start looked up its image or security group: a type not in `CIBURST_VM_IMAGES`, or a create refused for a removed image; the next `ciburst apply` resolves them again), `unauthorized`; every line ends with `ms`; `error` `start_failed`, `report_failed`, `terminate_failed`, `sweep_failed`, and `env` naming a missing variable |
| CLI | stderr; in pipeline mode the `ciburst-setup` job log | one line per action; `plan` warns `vm pools cannot start pool="<tag>" reason="..."`; `apply` logs `hook_reenabled`, `replayed` per replayed job, `replay_failed` and `output_unmasked` |
| Deliveries | `ciburst status` with a GitLab token: the webhook's 20 newest deliveries, oldest first | `delivery <time> <job name, pipeline <id> or event kind> <http status> <result> <pool> <ms>`; `-` where the answer lacks the key, `?` for an unreadable body |
| Lane run | the job run's stdout and stderr on Serverless Jobs; `ciburst status` lists the run and its state | `ciburst-lane: <reason>` before the bootstrap exits 64 (section Failure modes), then `gitlab-runner`'s own lines |
| VM agent | the VM's journal, `journalctl -u ciburst-vm-agent`, through the provider's console view only | `agent started`, `state from=... to=...` (`starting`, `idle`, `busy`, `draining`, `quitting`), `slot started` (with `runner="pool"` or `runner="spill"`), `slot exited`, `slot switching from=... to=...`, `spill allowed=...`, `report failed`, `report answer unreadable`, `job without slot label` (switching held), `terminate accepted`, `terminate failed`, `terminate gave up` |

## Rotation and updates

| Command | Replaces | Note |
|---|---|---|
| `ciburst rotate runner` | every pool's runner and token (deleted and recreated) | VMs and lane runs started before hold the old token: rotate while none runs |
| `ciburst rotate hook` | the hook token and the sweep token | updates the webhooks, the function and the cron |
| `ciburst rotate key` | the dispatch key | the old key is deleted, the function redeployed |
| `ciburst rotate cache` | the cache key | the old key is deleted; VMs and lane runs started before lose the cache: rotate while none runs |
| `ciburst rotate vm` | the VM secret | refused while VMs run, because their tokens would stop working |

Check after each: `ciburst plan` prints only `noop`; a test pipeline passes.

1. Yearly runtime bump: the new Rust version in `rust-toolchain.toml` and the host runtime (`RUNTIME` in `crates/scaleway/src/host.rs`), then in every file `scripts/check-pins.sh` lists (it runs in the `lint` job); tag a release; `ciburst apply`.
   Check: `lint` passes; the function is `ready`; a test pipeline passes.
2. Runner bump: the new version and its sha256 (from the release's `release.sha256`) in `RUNNER_URL` and `RUNNER_SHA256` of `crates/core/src/cloud_init.rs`, then in every file `scripts/check-pins.sh` lists; `ciburst apply`.
   Check: the plan shows `create` for the runner object (`runner-binary`) and `update` for every `definition:<tag>`; VMs created and lane runs started afterwards run the new version.
3. Agent: after a change in `crates/vm-agent`, `scripts/agent-binary.sh`, then `ciburst apply`; in pipeline mode a new `cli` image carries the agent and `ciburst-setup` uploads it. VMs created afterwards download the new object.
   Check: `CIBURST_AGENT_SHA256` in the outputs changed.

## VM runtime and the sweep

Lifecycle rules: spec section 6.1.

- cloud-init writes `/etc/ciburst/vm.env`, downloads `gitlab-runner` 18.11.4 from the runner object and the agent from the agent object, checks each sha256, then starts `ciburst-vm-agent.service` (`Restart=on-failure`, 5 s).
- The agent reports on every change and every 10 minutes; a failed report is retried every 5 s without limit. After the drain it posts `/terminate`, retried for 10 minutes (2 s doubling to 60 s); then it exits 1 and systemd restarts it.
- A slot that pulls its job's image has no labelled container yet and counts as free, 10 to 50 s per job in the drill.
- Tags: `ciburst-state:<state>`, `ciburst-free:<n>`, `ciburst-seen:<unix time>` and, once draining, `ciburst-drain:<unix time>`, beside `ciburst`, `<name>`, `pool:<tag>`, `job:<id>`. Read them in the console (Instances, the server, Tags) or with `GET https://api.scaleway.com/instance/v1/zones/<zone>/servers?tags=<name>` and the header `X-Auth-Token: <secret key>`.
- The cron `<name>-sweep` runs at minutes 0 and 30 (UTC) and ends a VM older than 24 hours, a VM that has not reported 10 minutes after its creation, and a VM whose last report is more than 30 minutes old. A VM that reported within 30 minutes and is younger than 24 hours stays.

End a VM by hand, when the sweep does not:

1. Find the server: console Instances filtered by the tag `<name>`, or the `GET` above.
   Check: its `ciburst-seen` is older than 30 minutes, or no job of the pool runs on it in GitLab.
2. Terminate it: console, the server, Delete with its volumes; or `POST https://api.scaleway.com/instance/v1/zones/<zone>/servers/<server id>/action` with the body `{"action": "terminate"}`. Never `poweroff` from inside the VM: standby is billed as running.
   Check: the server is gone from the list and no IP of it remains under Flexible IPs.
3. A job that ran on it fails once GitLab stops receiving its updates; re-run it.

## Burst grouping

Rules: spec D33 and section 6.1. The function writes and lists intents with the cache key; a read follows at most 20 pages of 1000 keys.

Inspect a pool's intents:

1. Console: Object Storage, bucket `<name>-cache-<project id>`, folder `ciburst/intents/<pool>/`.
2. CLI with `CIBURST_CACHE_ACCESS_KEY` and `CIBURST_CACHE_SECRET_KEY` from the outputs as the key pair: `aws s3api list-objects-v2 --endpoint-url https://s3.<region>.scw.cloud --region <region> --bucket <name>-cache-<project id> --prefix ciburst/intents/<pool>/ --start-after ciburst/intents/<pool>/<unix seconds>`.
   Check: a cold burst of N jobs at `slots` S shows N keys within about 2 s and `ceil(N / S)` new servers.
3. Never delete intents by hand. A key outside the format is ignored; a forged key in the format costs at most one VM-hour.

Warm start (spec section 6.1): a warm intent is `<unix seconds>-warm-<pipeline id, 20 digits>`, and the pipeline's records live in `ciburst/intents/_pipelines/<pipeline>/` (`vm-jobs.json`, `seen/<job>`) until its final status. Check: after a pipeline event with VM jobs `vm-jobs.json` exists; after the pipeline the folder is gone.

Known limits, each at most one VM-hour: a VM created in the 2 to 3 s after its intent left the 30 s window counts neither as old nor by rank, so a new burst at that instant can pass `maxRuns` by one; a repeated delivery within the same second is not recognised as a duplicate.

## Hetzner

Rules: spec section 6.2.

1. In the Hetzner Console create a project used by ciburst alone (a token cannot be narrowed below a project), then Security, API tokens, Generate API token with the permission Read & Write. Developer path: `HCLOUD_TOKEN=<token>` in `.local/hetzner.env`, then `chmod 600 .local/hetzner.env`. Pipeline mode: the variable `CIBURST_HCLOUD_TOKEN`, protected, masked and hidden, environment scope `ciburst`.
   Check: the token is listed with Read & Write.
2. Config: `"providers": { "hetzner": {} }` (optional `location`: `nbg1` default, `fsn1`, `hel1`; optional `image`: `docker-ce` default, any image must ship Docker) and `"runtime": "hetzner"` on each Hetzner pool (optional `instanceTypes`, default `cx33` then `cx43`; never `cax*`, the agent is amd64). Tags end with a letter or digit; `<name>-<tag>` stays within 54 characters. In `ciburst.json`:
   ```json
   {
     "providers": { "hetzner": {} },
     "pools": [
       { "tag": "vm", "runtime": "hetzner", "image": "ubuntu:24.04", "stages": [], "slots": 2, "maxRuns": 4 }
     ]
   }
   ```
   Check: the plan lists `create  firewall <name>-vm`; after `apply` the function holds `HCLOUD_TOKEN` as a secret variable.

Console checks: Servers (labels per VM), Firewalls (`<name>-vm`, no rules, the selector lists the running VMs), Primary IPs (empty while no VM runs). CLI: `hcloud server list -l ciburst-name=<name>`, `hcloud firewall describe <name>-vm`.

End a VM by hand: Console, the server, Delete; or `DELETE https://api.hetzner.cloud/v1/servers/<id>` with `Authorization: Bearer <token>`. Powered-off servers still bill.

Cost (hetzner.com/cloud/cost-optimized, 2026-10-24): CX33 (4 vCPU, 8 GB, 80 GB) 0.0144 EUR per hour; the price page includes the IPv4 while the invoice lists the Primary IP as its own line; every started hour is billed, IPv6 is free.

## Lanes

Rules: spec D45 to D47, sections 5.4 and 6.3. A lane is a pool with `runtime: jobs`: a job that needs no Docker daemon runs in its own image on Scaleway Serverless Jobs, billed per second, or first on a free slot of the VM pool.

Add lanes:

1. Proposal: `ciburst check --suggest` (developer path: `--config .local/config.json`).
   Check: exits 0 and prints, per image of the Docker-free jobs routed to a VM pool, a lane block for the config and the `tags:` line of each job.
2. Config, in `ciburst.json` or `.local/config.json`, the VM pool and the lanes:
   ```json
   {
     "pools": [
       { "tag": "vm", "runtime": "instances", "image": "ubuntu:24.04", "stages": [], "slots": 2, "instanceTypes": ["DEV1-M", "DEV1-L"], "maxRuns": 4 },
       { "tag": "node24", "runtime": "jobs", "image": "node:24-bookworm-slim", "jobs": ["lint", "typecheck", "unit-*"], "vcpu": 2, "memoryMiB": 4096, "maxRuns": 10 }
     ]
   }
   ```
   `jobs`: job names with `*` (any characters) and `?` (one character), matched before `stages`; a pattern belongs to one pool. `"vmFirst": false` keeps a lane's jobs off the VM slots. `image`: `linux/amd64` with `/bin/sh` and coreutils or BusyBox (`base64`, `sha256sum`), pullable without credentials (section Private lane images).
   Check: `ciburst plan` lists `job definition <name>-node24` and the runner `<name>-node24`; on an existing deployment it also updates the runner `<name>-vm`, whose tags gain `node24`.
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
4. `ciburst apply`, or `ciburst-setup` in pipeline mode.
   Check: Settings, CI/CD, Runners lists `<name>-node24` with the tag `node24`, `<name>-vm` with `vm` and `<name>-vm-spill` with `vm` and `node24`; a lane job's delivery answers `started` (a lane run) or `spilled` (a VM slot), and the job passes.

### Queue first and spill

Rules: spec D53, D54 and section 6.1.

- A VM job that finds every slot busy answers `queued` and waits; the slot that frees first takes it. A VM starts for it only when its wait plus the expected remainder of the running job it waits for would pass the pool's `maxQueueS` (default 90 s), or when no running job will free a slot; `queue_started` logs those starts. `"maxQueueS": 0` restores starting a VM at once.
- Typical durations come from the job events: `ciburst/intents/_stats/<pool>/<16 hex>` holds `{name, typicalS, runs}` per job name. A new job name counts as ending now until it has finished once.
- Queue state in the cache bucket: `ciburst/intents/_queue/<pool>/<job>` (waiting), `_running/<pool>/<job>` (running), `_queue/<pool>/.started` (start marker, 15 s). Never edit them by hand; entries older than 30 minutes (waiting) or 24 hours (running) are dropped when read.
- By default (`spillRunner` false, D55) the VM pool's runner carries the `vmFirst` lanes' tags: an idle VM slot takes a lane job whenever it polls first, also when GitLab sends no job events. With `"spillRunner": true`, a `vmFirst` lane's jobs go through the VM pool's second runner `<name>-<vm tag>-spill` instead. The agent points idle slots at it only while the report answer says `"spill": true`: no VM job queued, no running pipeline with a VM job still to come, the VM not draining. Such a lane job answers `spilled` and runs on the paid VM in its `image:`; if no slot took it within 20 s, `spill_fallback` starts a lane run.
- Busy VMs report every 30 s, VMs with spill slots every 15 s; each report re-evaluates the queue.
- Check the queue of a pool: `aws s3api list-objects-v2 ... --prefix ciburst/intents/_queue/<pool>/` with the cache key (section Burst grouping).

### Lane bootstrap

Every lane run executes `crates/core/src/lane-bootstrap.sh` (spec section 6.3) as the image's user:

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

1. Config: `"provider": { "type": "scaleway", "region": "<region>", "registry": true }`, without the lane yet; the owner key needs `ContainerRegistryFullAccess`. `ciburst apply`.
   Check: the output `CIBURST_REGISTRY_ENDPOINT` names the namespace.
2. Push the image to `<CIBURST_REGISTRY_ENDPOINT>/<image>:<tag>` with a Scaleway key that may write to the registry.
   Check: the console lists the image under Container Registry, namespace `<name>`.
3. Add the lane with that `image`, give its jobs the same `image:`, `ciburst apply`. A job definition is refused while its image is missing.
   Check: `ciburst plan` prints only `noop`.

## Failure modes

| Symptom | Cause | Action |
|---|---|---|
| A VM job answers `queued` and waits longer than `maxQueueS` | no VM report arrived to re-evaluate the queue (a VM hangs), or the pool is at `maxRuns` | `ciburst status`: the VMs and their last reports; a hung VM ends by the sweep after 30 minutes; `ciburst-setup` replays waiting jobs |
| Jobs stay pending for minutes; the deliveries show `created` job events and pipeline events on time, `pending`, `running`, `success` much later | GitLab delivers job status events late (2026-10-26: about 35 minutes for hours, D55); the late `pending` events then start lane runs for finished jobs, which idle `waitTimeoutS` and exit | the VM slots that carry the job's tags take it meanwhile; `ciburst-setup` replays jobs pending longer than 60 s; keep `spillRunner` false. Measure the delay: each delivery's `created_at` against the status time in its payload (`build_started_at`, `build_finished_at`) |
| Lane jobs never answer `spilled` (`spillRunner` true) | no `vmFirst` lane, a queued VM job, an open pipeline with VM jobs still to come, or the agent logs `job without slot label` | the VM's journal (`spill allowed=...`, `slot switching`); the report answers in the deliveries |
| `ciburst-setup` is missing from the pipeline | the default branch is not protected, or the pipeline runs on another branch | protect the default branch; run the pipeline there |
| `ciburst-setup` refuses to run, naming `CI_COMMIT_REF_PROTECTED` | an unprotected ref: protected variables would be missing | run it on the protected default branch |
| `ciburst-setup` fails naming a `CIBURST_*` variable | the variable is missing, unprotected, or scoped to another environment | add it protected with environment scope `ciburst` |
| `ciburst-setup` cannot pull `registry.gitlab.com/ciburst/ciburst/cli:<version>`, or the include fails | while `ciburst/ciburst` is private: no access to it, or the project is missing from its job token allowlist | ask its maintainers for access and the allowlist entry |
| `ciburst-setup` fails with 403 on `POST /user/runners`, naming `create_runner` and the Owner role | the token lacks the scope, or its user is not an Owner of the project | a new token of a project Owner with `api` and `create_runner` in `CIBURST_GITLAB_TOKEN` |
| setup log `output_unmasked key=<key>` | GitLab refused to mask that value | none; the variable stays protected and scoped to `ciburst` |
| Running `ciburst-destroy` with a variable is refused (403), or the UI offers no variable form | new projects allow no pipeline variables, which include variables typed into a manual job | set `CIBURST_DESTROY` as a project variable instead |
| destroy log `warn servers still ending`, `security group not deleted` or `project not deleted` | a VM was still terminating after 5 minutes | run `ciburst-destroy` (or `ciburst destroy`) again |
| A job passes but logs `Failed to create cache` after `400 Bad Request ... InvalidRequest ... larger than the maximum allowable size for a copy source: 5368709120` | the cache archive exceeds 5 GiB, the largest object Scaleway's S3 lets the runner copy (drill consumer, 2026-10-25: a Rust coverage build) | keep each cache under 5 GiB: cache fewer paths, prune build outputs such as test binaries before the job ends, or split the cache into several keys |
| apply log `replay_failed ... 403` | the token cannot list jobs (a fine-grained token without read access to jobs) | retry the waiting jobs by hand; use a token with the scope `api` |
| `apply` stops: the zone has no quota for the pool's types | Instance quota 0 for every type of the pool in the zone | set `provider.region` (or `provider.zone` within it) to one the message names, or ask Scaleway support to raise the quota; `ciburst apply`; delete the security group `<name>-vm` of the old zone (console, Instances, Security groups) |
| `apply` stops naming `--agent` and `scripts/agent-binary.sh` | no agent binary given and no agent object in the bucket | developer path: `scripts/agent-binary.sh`, then `ciburst apply`; pipeline mode: the `cli` image carries the agent, check the component's `version` input |
| Jobs stay pending, the webhook shows failures | GitLab disabled the webhook after four failures | re-enable it; `ciburst apply` re-verifies it and replays waiting jobs |
| Deliveries answer 500, no server or lane run appears | the start failed: quota used up by other servers, provider credentials, a lane without its job definition, or an incomplete Scaleway side (`scaleway: shared environment: ...` for added providers) | `ciburst plan` shows repairs; `ciburst apply`; quota as above |
| VMs stay `starting` until the sweep ends them | cloud-init failed: runner object or agent download, a hash mismatch, or an incomplete `vm.env` (the agent exits 64 naming the key) | fix the cause, `ciburst apply`, re-run the job; for a hash mismatch as in the row `gitlab-runner does not match its SHA-256` |
| A VM job stays pending while VMs of the pool idle | the job's name or stage routes it to a lane | remove the name from the lane's `jobs` or the stage from its `stages`; `ciburst check` |
| VMs start or slots get claimed for jobs that run on GitLab's hosted runners | the dispatcher sees no tags; those jobs fall to the default pool, a VM pool | remove `defaultPool` (jobs no pool takes then start nothing) and route the ciburst jobs with `jobs` patterns or `stages`; started VMs end at minute 55 |
| A cold burst starts one VM per job; no delivery answers `grouped` | the ledger failed (`ledger_failed`): cache bucket missing, cache key rotated without `apply`, or more than 20 pages of intents | `ciburst apply` |
| A draining VM takes minutes to end | a slot cut off from GitLab waits 300 s between job requests, and `SIGQUIT` does not shorten that wait | none; the VM keeps reporting `draining` until its slots exit |
| `ciburst plan` reports a config error for a `jobs` pattern in two pools, or for `vmFirst` | each pattern belongs to one pool; `vmFirst` is valid on lanes only | fix `ciburst.json`, run `ciburst-setup` (or `ciburst apply`) |
| `ciburst check` names a lane job with `services:`, a `docker` image or a `docker ` script line | the job needs a Docker daemon, which lanes lack | keep the job on the VM pool: remove it from the lane's `jobs`, give it the VM pool's tag |
| `ciburst check` names a lane job whose `image:` differs from the lane's, or an unresolvable variable in it | a VM slot would run the job in another image than a lane run | set `image:` to the lane's `image`, or define the variable in the pipeline's top-level `variables:` |
| Lane: job pending, a run started and exited idle | the job lacks the lane's tag; a run that exits idle because a VM slot took its job is no failure | fix the job's `tags:` or the lane's `jobs`, `ciburst apply`, `ciburst check`, re-run |
| Lane: run `failed` right after start without a `ciburst-lane:` line, job pending | the image cannot be pulled (missing, private outside the deployment's registry, not `linux/amd64`), or it has no `/bin/sh` or `base64` | fix the lane's `image` and the jobs' `image:`, `ciburst apply`, re-run the job |
| `ciburst-lane: missing environment variable <NAME>` | job definition or run environment incomplete | `ciburst apply`; for the cache keys `ciburst rotate cache` |
| `ciburst-lane: the image lacks <tools> and runs as a non-root user` | the image misses `git`, `curl` or CA certificates, and its user cannot install packages | a root image, or one that contains the tools, as the lane's `image` and the jobs' `image:`; `ciburst apply` |
| `ciburst-lane: apk could not install <tools>` | `apk add` failed: Alpine mirror unreachable, or the packages missing for the image's release | re-run the job; if it persists, an image that contains the tools |
| `ciburst-lane: apt-get could not install <tools>` | `apt-get update` or `install` failed: mirror unreachable, or the image's release archived | re-run the job; if it persists, a current release of the image or one that contains the tools |
| `ciburst-lane: the image lacks <tools>; RPM-based images must contain them, RPM cannot install in the sandbox` | an RPM-based image (UBI, Fedora) without `git`, `curl` or CA certificates | an image that contains them, for example one built on it and pushed to the deployment's registry (section Private lane images) |
| `ciburst-lane: the image lacks <tools> and has no apk, apt-get, microdnf or dnf` | no known package manager to install the tools with | an image that contains `git`, `curl` and CA certificates |
| `ciburst-lane: cannot download gitlab-runner` | the runner object is missing (deleted, or `apply` stopped before the `runner-binary` step) or the bucket is unreachable | `ciburst plan` shows `runner-binary` as `create`; `ciburst apply`; re-run the job |
| `ciburst-lane: gitlab-runner does not match its SHA-256` | the object at `CIBURST_RUNNER_URL` is not the pinned runner, for example overwritten by a job, which holds the cache key | delete `ciburst/runner/<sha256>` from the cache bucket, `ciburst apply`, re-run the job |
| `ciburst-lane: RUNNER_WAIT_TIMEOUT must be a whole number of seconds`, or `ciburst-lane: CACHE_TYPE must be s3 or empty` | the job definition's environment was edited outside ciburst | `ciburst apply` restores it |
| Lane: run ends `out of memory` | files in `/tmp` count against memory | raise `memoryMiB` or write to the builds dir |
| Lane: costs beyond the free tier | Serverless Jobs free tier exceeded | consumption page; per-second prices apply (section Facts) |
| Function deploy fails with an unknown runtime | Scaleway retired `rust196` | runtime bump (Rotation and updates) |
| `hetzner: pool X: no server type started (cx33: resource_unavailable ...)` | the location is out of stock for every type | add types to `instanceTypes` or set `providers.hetzner.location` |
| Hetzner `resource_limit_exceeded` | the project's server or core limit | ask Hetzner for a higher limit or lower `maxRuns` |
| Hetzner `unauthorized` (401) or `forbidden` (403) | token deleted or read-only | a new Read & Write token, `ciburst apply` |
| Hetzner `rate_limit_exceeded` (429) | 3600 requests per hour per project | wait; look for report storms |
| `hetzner: pool X is not provisioned` | firewall missing or without its selector | `ciburst plan` shows the repair, `ciburst apply` |
| Hetzner VM swept from `starting` | cloud-init failed (runner or agent download, GitLab unreachable); no SSH | the server's Console view: `/var/log/cloud-init-output.log`, `journalctl -u ciburst-vm-agent` |
| Primary IPs left after VMs ended | the automatic IPv4 was not deleted with its server | delete them under Primary IPs; they bill hourly |

## Acceptance drill

Config: the default pool `vm` (`runtime: instances`, `slots` 2, `maxRuns` 4) in a zone with quota, the lane `ci` (`runtime: jobs`, `image` `node:24-bookworm-slim`, `stages` `["test"]`, `"vmFirst": false`). Pipeline: `test/drill/gitlab-ci.yml` as the `.gitlab-ci.yml` of the drill consumer's branch `ciburst-drill`. VM slots first is drilled in the M6 drill.

1. `ciburst apply`, then `ciburst plan`.
   Check: only `noop`; the security group `<name>-vm`, the cron `<name>-sweep`, the agent object and the runner object exist.
2. `ciburst check --ref ciburst-drill`.
   Check: `every job matches the stage map`.
3. Cold pipeline: push an empty commit to `ciburst-drill` while no VM runs.
   Check: the five VM jobs start three VMs (deliveries `started` three times, `grouped` twice); `node`, `rust`, `postgres`, `dind` and `docker-build` pass on the runner `<name>-vm`, `fast-lane` in a lane run on `<name>-ci`, `hosted` on a GitLab runner; `$CI_REGISTRY_IMAGE/ciburst-drill:<short sha>` exists.
4. Tags while the pipeline runs.
   Check: `starting`, then `idle` or `busy` with `ciburst-free` and a fresh `ciburst-seen`.
5. Warm pipeline within the VMs' paid hour.
   Check: `claimed` for every VM job; no new server.
6. Drain.
   Check: each VM reports `draining` at minute 55 of its hour and is gone before minute 60; no server, IP or volume remains.
7. Sweep: in the VM zone create a powered-on server without user data, tagged `ciburst`, `<name>`, `pool:vm`, `ciburst-state:starting`.
   Check: the first sweep after its tenth minute terminates it; reporting VMs stay.
8. Without `defaultPool`: a pending event of a stage no pool lists.
   Check: `200 {"reason":"unmapped_stage","result":"ignored","stage":"<stage>"}`; nothing starts.
9. Cost on the consumption page.
   Check: Instances, IPv4 and local SSD for the VM hours only; Serverless Jobs inside the free tier.

Measured:

| Measure | Value |
|---|---|
| VM boot: first VM job pending to running, DEV1-M | 47 to 96 s |
| Queue on warm VMs | about 3 s |
| Cold burst of five VM jobs at two slots | three VMs |
| Cold plus warm pipeline within one hour | 0.09 EUR |
| Idle VM | `draining` at minute 55, gone within 2 minutes |
| Serverless Jobs job, pending to running (M2 drill) | 3.5 to 38.6 s, slowest on the first pull of a large image |
| Dispatcher answer, warm | 0.1 to 0.4 s |
| Cache restore on the next pipeline | 0.3 s |
| Function deploy | 5 to 13 minutes |
| CPU-bound job, DEV1-L with one slot against GitLab's small hosted runner (drill consumer, 2026-10-25) | 1.3 to 4 times as long: rust-check 135 s against 84 s, ts-check 245 s against 62 s, e2e 482 to 494 s against 231 to 299 s, image build 409 s against 303 s; lighthouse 80 s against 105 s |
| Cache transfer, bucket in the VMs' region | restore and archive of a Rust job in seconds; across regions 555 s for the same job (D43) |
| CPU per zone, one VM each at boot (2026-10-25): SHA-256 of 1 GiB on one core, then all cores | DEV1-M `pl-waw-1` and `pl-waw-2` EPYC 7282: 5.3 and 5.5 s; `nl-ams-2` EPYC 7282: 6.4 s; `nl-ams-1` EPYC 7281: 8.6 s; GP1-XS `nl-ams-1` EPYC 7401P: 9.8 s; GitLab's small hosted runner (Xeon 8581C): 5.5 s. `nl-ams-1` mixes both DEV1 hosts; boot to user data 18 to 37 s |
| Serverless Jobs `nl-ams`, start request to first command (2026-10-25) | 5 to 12 s small images, 19 to 28 s Rust and Playwright, 93 s at 6 vCPU |
| Same workloads: GitLab's small hosted runner, DEV1-M `nl-ams-1`, Serverless Job 2 vCPU, 4 vCPU | `cargo install ripgrep`: 56, 54, 52, 30 s; three TypeScript checks: 8.5, 14.3, 9.5 to 10.9 s; `npm install`: 1.4, 2.5, 2.7 to 3.0 s |
| Lane bootstrap in a Serverless Job | `apk add git` 2 to 4 s; `apt-get` with its own lists and archives directories (the sandbox drops root's capabilities) 21 s; images with `git` and `curl` need none |
| `CIBURST_LANE` size, the lane `COMMAND` in `nl-ams` (2026-10-25) | job definitions with 6,728 to 87,528 characters accepted; dry runs on `alpine:3.21` and `node:24-bookworm-slim` succeeded |
| Scaleway bill, October to 2026-10-25 21:18 UTC (consumption API, amounts in cents per line) | 2.00 EUR in total, 1.97 of it VM-hours (compute, IPv4, local SSD); Serverless Jobs (29,556 vCPU-s, 63,472 GB-s) and Functions fully inside the free tier; Object Storage and registry 0.02 |
| Price per VM-hour from the bill | DEV1-M 0.021 + IPv4 0.005 + local SSD 0.002 = 0.028 EUR; DEV1-L 0.044 + 0.005 + 0.004 = 0.053 EUR; local SSD is billed per clock hour (960 GB-hours for 8 DEV1-L hours of 80 GB) |
| Cost per job-minute, drill consumer on VMs (3 pipelines, one DEV1-L per job, 2026-10-25) | 0.68 EUR for 146 job-minutes on 12 DEV1-L hours: 0.0046 EUR per job-minute, about half of GitLab's small runner (0.0089); the VMs were busy about 20 % of their paid hours |
| One paid hour per VM, the bill against the job logs (`via <vm>`, 2026-10-25) | drill consumer: 8 VMs ran jobs, 8 DEV1-L hours billed, the longest ran jobs for 50 minutes across two pipelines; demo: 8 VMs ran jobs, 9 DEV1-M hours billed, the ninth most likely a VM woken by the pending events of its two hosted-runner jobs (D49). The drill consumer's first deployment paid 4 DEV1-L hours for VMs that got no job. A VM busy at minute 55 pays its next hour, by design |
| VM start answers on a warm function (drill consumer, 2026-10-26) | 4,238 and 6,255 ms; the warm start 4,327 ms including the 2 s burst wait; lane starts 830 to 1,174 ms, claims 958 to 1,481 ms |
| GitLab job status webhook delay (both projects, 2026-10-26, from the hook event log) | 1 to 3 s in the morning; 2,003 to 2,125 s from the afternoon on, for `pending`, `running` and `success`; `created` job events and pipeline events 1 to 4 s throughout |

### M5 drill

1. Dev deployment on the agent object: `scripts/agent-binary.sh`, `ciburst apply`, then steps 1 to 3 of the acceptance drill.
   Check: the first apply creates the agent object, the next plan prints only `noop`; every VM downloads the agent and reaches `idle` or `busy`; the cold pipeline passes.
   Result (2026-10-25): apply uploaded the agent (3.5 MB) and moved the bucket to three lifecycle rules; the deploy took 12 min 46 s; the next plan was 15 `noop`. The object answers an anonymous GET with the uploaded hash; the bucket listing and unknown keys answer 403. A cold pipeline passed 7 of 7: five VM jobs on three VMs created 14 s after the pipeline, `idle` 61 to 92 s after creation, VM jobs queued 67 to 96 s. `replay_failed` with 403 on the fine-grained owner token (no read access to jobs).
2. Adoption drill in a fresh project: push a pipeline, then README, Adopt, steps 1 to 5.
   Check: the minutes of each README step; the hosted-runner minutes of `ciburst-setup`; the jobs pushed before the setup start after it; a second `ciburst-setup` plans only `noop`; `ciburst-check` passes.
   Result (2026-10-25, `ciburst/ciburst-adopt-demo`): the owner set the four variables by hand; the component resolved from `ciburst/ciburst@main` after the job token allowlist entry. First setup 6.1 hosted minutes including the dispatcher deploy; second setup 16 s, 12 `noop`; `ciburst-check` 9 s, passes. The three jobs pushed before the setup were replayed (`started`) and passed (node 20 s, postgres 23 s, docker build 38 s). A cold pipeline queued 61 to 80 s. Findings, fixed: the cache bucket landed in the setup key's preferred project instead of `ci-<project id>`, so the cache key got 403 (no burst grouping, no job cache); the setup masked ids and the agent hash in its log. Rerun with the fixed `cli:edge` (ba5f398): setup 6.1 hosted minutes; the bucket in `ci-<project id>`, the intents in it; the three waiting jobs answered `started`, `grouped`, `started` and passed on two VMs. A `cache:` job created its cache in the bucket, and the next pipeline restored it; on the warm VMs every job queued 2 to 3 s.
3. Destroy in that project: the project variable `CIBURST_DESTROY=<name>`, then `ciburst-destroy`.
   Check: `destroyed <name>`; no output variable, webhook, runner, server or Scaleway project `<name>` remains.
   Result (2026-10-25): GitLab refused `CIBURST_DESTROY` as a manual-job variable (403, pipeline variables off in new projects), so it became a project variable. The first destroy took 72 s and left the security group and project behind (VMs still terminating, fixed); a second run removed both in 13 s. Webhook, runner and output variables were gone after the first run.
4. The drill consumer adopts ciburst through its own pipeline (component, `ciburst.json` with one DEV1-L per job, maxRuns 4), guided by `AGENTS.md`.
   Result (2026-10-25): the Scaleway inputs were first created under workstation names (the setup now names them); the first deployment split region and zone (`fr-par`, `nl-ams-1`) and spent most of its Rust job time moving caches across regions (D43, D44). After moving to `nl-ams`, every job of the pipeline passed on ciburst, Playwright e2e, image build with Docker-in-Docker, compose stack test and deploy included: 28.6 minutes wall clock with a cold cache and a 6-minute setup. Two of its unit tests had timing assumptions that the slower vCPUs exposed; fixed in the consumer.
5. Hetzner, once `.local/hetzner.env` exists: the `vm` pool on `runtime: "hetzner"`, `apply`, a cold pipeline.
   Check: three CX33 servers with the labels; the firewall applied (`public_net.firewalls[].status` `applied`); the jobs pass; the drain at minute 55 deletes every server; Primary IPs empty afterwards.
   Result: ____

### M6 drill

1. Dev deployment with two lanes, from `node:24-bookworm-slim` and from `rust:1.99.0-bookworm`, each with `jobs` patterns for the lane jobs of a demo pipeline; `vmFirst` default. `ciburst apply`, then `ciburst plan`.
   Check: the apply creates the runner object, both job definitions and their runners, and updates the runner `<name>-vm` with both lane tags; the plan prints only `noop`.
   Result (2026-10-25, `ciburst/ciburst-adopt-demo`, lanes `node24` from `node:24-bookworm-slim` with `vmFirst` and `rust` from `rust:1.99.0-bookworm` at 4 vCPU with `"vmFirst": false`): the setup took 9.9 hosted minutes, 5 of them the dispatcher deploy. It copied the runner (119 MB) into the bucket in 2 s, created both job definitions and runners and set the tags of `<name>-vm` to `vm, node24`. After the PATH fix a setup updated both definitions; the next one planned 17 `noop` in 17 s.
2. Demo pipeline while no VM runs.
   Check: every lane job's delivery answers `started`; the jobs pass on the lanes' runners; the Debian slim run spends about 21 s in the bootstrap, the Rust run none.
   Result (2026-10-25): both lane jobs answered `started` and passed on their runners, no VM started. `lane-node` queued 38 s (start and `apt-get` of `git` and `curl`), restored the cache its VM run had created and ran three TypeScript checks in 9.9 s (14.4 s on the DEV1-M); `lane-rust` queued 30 s and compiled ripgrep in 29.6 s at 4 vCPU. Finding, fixed: the shell executor's login shell replaced the image's `PATH`, so `cargo` was missing in the first Rust run.
3. Demo pipeline while a VM of the pool is `idle` with free slots and older than 30 s.
   Check: the lane jobs answer `claimed` with `lane` and pass on the runner `<name>-vm`; no lane run starts.
   Result (2026-10-25): with one idle VM, `lane-node` answered `{"lane":"node24","pool":"vm","result":"claimed"}` and ran on `<name>-vm` after 1.4 s in the queue; `lane-rust` (`vmFirst` false) answered `started` and ran in its lane after 6.8 s. Before the setup, the old dispatcher had routed both lane jobs by stage and started a VM, which took the node job as soon as the setup added `node24` to its runner. Finding: the delivery that started the VM answered after 10.4 s, beyond GitLab.com's 10 s webhook timeout, and counted as failed although the VM started (spec section 13, risks).
4. The drill consumer: `ciburst check --suggest`, the proposed lanes in `ciburst.json` and the `tags:` in its pipeline, `ciburst-setup`, a pipeline.
   Check: `ciburst-check` passes; every job passes; wall clock and cost against its 18-minute warm VM pipeline.
   Result (2026-10-26, drill consumer, four lanes rust, alpine, node, playwright, `vmFirst` default, warm start on): `ciburst-check` passed; a main pipeline took 20.4 minutes, 44.2 job-minutes on ciburst. All eleven lane jobs that found no free VM slot ran in their lanes (10 runs, 2,222 s, 8,147 vCPU-s, 16,294 GB-s: 0.11 EUR at list price, inside the free tier); Chromium (Lighthouse) runs in the sandbox; `rust-test` peaked at 6.5 GB on the lane disk. It still used 4 DEV1-L VMs (0.21 EUR): GitLab hands a pending lane job to any free VM slot, because the VM runner carries the lane tags, and four parallel lane claims raced onto two free slots past the reservation, so the Docker job `build-image` started another VM. Finding: with `vmFirst` lane jobs take the slots that Docker jobs need; the drill consumer sets `"vmFirst": false` on its lanes.

### M7 hardening drill

1. Demo deployment: `ciburst-setup` with the hardened `cli:edge`.
   Check: the plan shows `update  function <name>-dispatch  (settings, config or secrets changed)`; the function has `memory_limit` 1024 and the plain variables `CIBURST_VM_SECURITY_GROUP` and `CIBURST_VM_IMAGES`; the next setup plans only `noop`.
   Result (2026-10-26): the setup planned the function and function code updates and the hook's pipeline events (9.7 hosted minutes); the function runs with `memory_limit` 1024 and `cpu_limit` 560, `CIBURST_VM_SECURITY_GROUP` is set and `CIBURST_VM_IMAGES` holds DEV1-M and DEV1-L with `installDocker` false.
2. A VM start right after the deploy, then three on a warm function (`[one-vm]` pipelines while no VM runs).
   Check: every delivery carries `ms`; the first start answers within 9 s, the warm ones within 6 s; no `start_lookup`.
   Result (2026-10-26): first answers after the deploy: a lane start 1,528 ms, the warm VM start 4,415 ms including the 2 s burst wait, a claim 1,644 ms, the closing pipeline event 4,540 ms (its deletes run one after another); no answer above 10 s. Starts on a warm function: from the drill consumer's next pipeline.
3. `ciburst status` with the owner token.
   Check: `hook <id>: executable` and the newest deliveries with result, pool and `ms`.
   Result (2026-10-26): with the deployment's project in `SCW_DEFAULT_PROJECT_ID` (an id there replaces the lookup by name), `status` printed `hook <id>: executable` and every delivery, for example `delivery 2026-10-26T06:44:16Z pipeline <id> 200 warm vm:started 4415`.

### M6.1 warm start drill

1. Demo deployment: `ciburst-setup` after the M6.1 code.
   Check: the plan shows `update  gitlab hook in project <id>  (pipeline events)`; the hook's pipeline events are on.
   Result (2026-10-26): the plan showed `update  gitlab hook in project <id>  (pipeline events)`; the setup log `info hook`.
2. A pipeline with a lane stage of about 60 s before a Docker job, no VM running, `warmStart` on.
   Check: the pipeline event answers `warm` with `started`; `vm-jobs.json` exists; the Docker job queues under 10 s and leaves `seen/<job>`; after the pipeline the records are gone.
   Result (2026-10-26): the pipeline event (`pending`) answered `{"result":"warm","pools":[{"action":"started","pool":"vm","vmJobs":1}]}` and the warm VM booted to `idle` in 74 s; the lane job ran in its lane (35 s queue: start and `apt-get`); the Docker job of the next stage answered `claimed` on the warm VM and queued 3.4 s; the `running` event answered `duplicate`, the final one `closed`, and `ciburst/intents/_pipelines/` was empty afterwards. The warm intent key read `<unix s>-warm-<pipeline id>`.
3. The same pipeline with `"warmStart": false`.
   Check: `warm` with `disabled`; the Docker job queues 60 to 100 s.
   Result (2026-10-26): not run again: the cold baseline is the M5 drill, where VM jobs queued 61 to 96 s behind a VM boot.
4. The drill consumer, once with `warmStart` true and once false, each from a cold start.
   Check: wall clock, VMs started, VM-hours, lane seconds and cost per pipeline.
   Result: ____

### M6.2 queue first drill

1. Demo deployment: `ciburst-setup` with the M6.2 code.
   Check: the plan shows `update  gitlab runner <name>-vm  (set tags vm)` and `create  gitlab runner <name>-vm-spill  (tags vm, node24)`; the function has `RUNNER_SPILL_TOKEN_VM`.
   Result (2026-10-26): the plan showed `update  gitlab runner <name>-vm  (set tags vm)` and `create  gitlab runner <name>-vm-spill  (tags vm, node24)` (10.2 hosted minutes). Superseded the same day by D55: the next setup (10.6 hosted minutes) planned `update  gitlab runner <name>-vm  (set tags vm, node24)` and `delete  gitlab runner <name>-vm-spill` and applied both.
2. A Docker chain (two Docker jobs in successive stages, slots 1) while one VM runs.
   Check: the second job answers `queued` or `claimed`, never `started`; one VM in total.
   Result (2026-10-26): with one idle VM, three 60 s VM jobs started at once: two ran at once on its two slots, the third took the first freed slot after 73 s in the queue; one VM in total. The job events of that hour arrived about 35 minutes late (D55), so the dispatcher answered nothing for them in time; the VM's polling slots carried the run.
3. Two long Docker jobs at once on a pool with slots 1 whose job name has a typical duration above `maxQueueS`.
   Check: the second answers `queued` and a `queue_started` start follows within one report interval; two VMs.
   Result: ____
4. Lane jobs within the paid hour, no VM job waiting.
   Check: `reported` answers carry `spill: true`; lane jobs answer `spilled` and run on `<name>-vm-spill`; the VM journal shows `slot switching`.
   Result: ____
5. The drill consumer, one main pipeline from a cold start.
   Check: VMs per pipeline (target 1, at most 2), VM-hours, lane seconds and cost against pipeline 2988692655.
   Result: ____

## `run-single` flags

Source: `gitlab-runner run-single --help` in `gitlab/gitlab-runner:v18.11.4` (`images/test/runner-help.sh`) and `commands/single.go` at `v18.11.4`; every flag below is present there.

| Flag | VM slot | Lane | Note |
|---|---|---|---|
| `--url`, `--token` | yes | yes | the runner does not read `RUNNER_TOKEN`, so the token is a flag |
| `--executor` | `docker` | `shell` | |
| `--shell` | | `bash`, else `sh` | the lane image's shell |
| `--builds-dir /builds`, `--cache-dir /cache` | | yes | |
| `--docker-image` | the pool's `image` | | for jobs without `image:` |
| `--docker-privileged` | yes | | Docker-in-Docker |
| `--docker-volumes` | `/cache`, `/certs/client` | | `/certs/client` for Docker-in-Docker with TLS |
| `--docker-cache-dir` | `/var/lib/ciburst/slot-<n>/volumes` | | the executor names volumes after token, system id, project id and concurrent id (`executors/docker/volume.go`); every slot reports concurrent id 0, so without it two jobs of one project on one VM share `/builds` and `/certs/client`; with it the volumes are host directories `<dir>/<unique name>/<md5 of path>`, and `CI_PROJECT_DIR` stays `/builds/<namespace>/<project>` |
| `--config` | `/var/lib/ciburst/slot-<n>/config.toml` | | read only with `--runner`, so every slot of a VM gets the system id derived from the machine id, as one `gitlab-runner run` with `concurrent` slots does |
| `--docker-container-labels ciburst.slot:<n>` | slot number | | marks build, helper and service containers with their slot, so the agent knows which slot is idle and may switch runners (M6.2) |
| `--max-builds 0` | yes | yes | no build limit |
| `--wait-timeout` | `0` | `waitTimeoutS` | 0 never exits for idleness (`checkFinishedConditions` tests `WaitTimeout > 0`); otherwise the timer restarts after every build |
| `--cache-type s3`, `--cache-shared`, `--cache-s3-server-address`, `--cache-s3-bucket-name`, `--cache-s3-bucket-location`, `--cache-s3-access-key`, `--cache-s3-secret-key` | yes | yes | with an S3 cache |
| `--env` | `FF_USE_FASTZIP=true`, `CACHE_COMPRESSION_LEVEL=fastest`, with an S3 cache | the same | D44: parallel cache archives with the fastest compression |

Job containers: the Docker executor labels build and service containers with `com.gitlab.gitlab-runner.job.id` (`executors/docker/internal/labels/labels.go`). A slot requests a job every 2 to 3 s and exits about three seconds after `SIGQUIT`, logging `Requested quit, waiting for builds to finish` (`images/test/run-single-slot.sh`). A runner whose requests fail waits `NotHealthyCheckInterval` (300 s) between requests (`common/consts.go`).

## Decisions

| Date | Decision |
|---|---|
| 2026-10-25 | The lane bootstrap passes every value as a flag and unsets `RUNNER_TOKEN`, `CACHE_S3_ACCESS_KEY` and `CACHE_S3_SECRET_KEY` before `exec`, so job scripts do not inherit them. |
| 2026-10-24 | Releases use `glab release create` in `registry.gitlab.com/gitlab-org/cli:v1.124.0` with `GLAB_ENABLE_CI_AUTOLOGIN`; `release-cli` is deprecated since GitLab 18.0. |
| 2026-10-24 | Framework image builds use `docker:27.5.1` with `docker:27.5.1-dind`. |
| 2026-10-24 | VM slots mount `/certs/client` besides `/cache`, so Docker-in-Docker with TLS (`DOCKER_TLS_CERTDIR: "/certs"`) works as on GitLab's hosted runners. |
| 2026-10-24 | The agent starts slots with only `PATH` and `HOME` in their environment: the runner reads `CI_SERVER_URL`, `CACHE_*` and `DOCKER_*` from its environment, and the flags stay the only source. |
| 2026-10-24 | `free` counts running slots without a job; a draining VM reports `free` 0. The 30 s grace starts when the dispatcher accepted the `draining` report. |
