# Runbook

Rules and design: `docs/SPEC.md`. Below, `ciburst` stands for the release binary or `cargo run -p ciburst --`.

## Facts

| Item | Value |
|---|---|
| Region | `fr-par` (provider and host); Instances in `provider.zone`, default `fr-par-1` |
| Scaleway resources | project `<name>`, IAM applications `<name>-dispatch` and `<name>-cache`, bucket `<name>-cache-<project id>`, namespace `<name>`, function `<name>-dispatch`, cron `<name>-sweep`; with VM pools the security group `<name>-vm` and servers `<name>-<tag>-<8 hex>`; with fast lanes job definitions `<name>-<tag>`; with `provider.registry` the registry namespace `<name>` |
| GitLab resources | runner `<name>-<tag>` per pool in the first project, enabled in the others; a webhook to the function URL in every project |
| Agent object | `ciburst/agent/<sha256>` in the cache bucket, public read: the musl `ciburst-vm-agent` |
| Burst intents | empty objects `ciburst/intents/<pool>/<unix s, 10 digits>-<job id, 20 digits>` in the cache bucket, expired by its lifecycle rules |
| Images | `registry.gitlab.com/ciburst/ciburst/cli` (the setup jobs, with `ciburst` and `ciburst-vm-agent`) and the fast-lane images `registry.gitlab.com/ciburst/ciburst/{base,node,rust,playwright}`; `edge` from `main`, `X.Y.Z`, `X.Y`, `X` and `latest` from a tag `vX.Y.Z` |
| Binary | `ciburst-x86_64-unknown-linux-musl` in the framework project's generic package registry, linked from each release |
| Runner | `gitlab-runner` 18.11.4, `run-single`. VM pools: Docker executor, `slots` processes per VM. Fast lanes: shell executor, one run per pending job, exits after `waitTimeoutS` idle seconds |
| VM cost | DEV1-M 0.0272 EUR per hour by price list (VM 0.020196, IPv4 0.005, 40 GB local SSD 0.00196), about 0.030 in practice because the SSD bills per clock hour; Hetzner CX33 0.0144 with IPv4 |
| VM network | VMs need IPv4: `registry.gitlab.com` and the Scaleway APIs have no IPv6 address (2026-10-24); a VM accepts no inbound connection |
| Instance quotas | per organization, zone and commercial type, resource `cp_servers_type_<TYPE>` with `_` for `-` (`cp_servers_type_DEV1_M`); read with `GET https://api.scaleway.com/iam/v1alpha1/quota/cp_servers_type_<TYPE>?organization_id=<org id>` |
| Serverless Jobs limits | 6 vCPU, 16 GB, 24 h per run, 400 parallel runs per organization, `linux/amd64`, images under 2 GB recommended; `/tmp` counts against the run's memory |
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
   Check: the plan lists the agent object as `create`, or `noop` when this build is already uploaded.
6. `ciburst apply --config .local/config.json`.
   Check: exits 0; `.local/outputs.env` holds `CIBURST_DISPATCH_URL`.
7. `ciburst plan --config .local/config.json`.
   Check: only `noop` lines.
8. `ciburst check --config .local/config.json` (`--ref <branch>` lints a branch before it is merged).
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
   Check: `destroyed <name>`; no output variable, webhook or runner remains; delete `CIBURST_DESTROY` afterwards. A `warn` line names what is left; a second run removes it.

Project variables after a setup, environment `ciburst`, protected, raw: the keys of `outputs.env` above except `HCLOUD_TOKEN`, which stays in memory. Tokens, secrets and secret keys are masked and hidden where GitLab allows; ids, URLs and the agent hash stay readable in job logs.

Where to look when a job waits:

1. The job's tags. A job without the pool tag waits for another runner; `ciburst-check` names it.
2. The webhook's recent events (Settings, Webhooks, Edit, Recent events): the job's pending delivery and its answer (section Dispatcher answers). No delivery: the webhook is missing or disabled; `ciburst-setup` re-verifies it and replays waiting jobs.
3. `started` or `grouped`: a VM boots in about a minute; its tags go from `starting` to `idle` or `busy`.
4. `claimed`: a polling slot of the pool takes the job within seconds once one is free; if none does, read the VMs' tags (section VM runtime and the sweep).
5. `at_capacity`: `maxRuns` VMs run; the job starts when a slot frees.
6. `500`: the start failed; section Failure modes.
7. Jobs created before the first setup: `ciburst-setup` replays jobs pending for more than 60 s.

## Dispatcher answers

| Status | `result` | Meaning |
|---|---|---|
| 200 | `claimed` (`runId`) | a free slot on a VM created more than 30 s ago; `runId` names the VM |
| 200 | `started` (`runId`) | VM pool: the job leads its burst group and started a VM with the group's slots taken; fast lane: a run started |
| 200 | `grouped` (`leader`) | rides on the VM its leader's delivery starts; any polling slot of the pool takes it, GitLab does not bind it to that VM |
| 200 | `duplicate` | GitLab delivered this job's pending event again within 30 s; nothing started |
| 200 | `at_capacity` | `maxRuns` reached; the job waits for a free slot or run |
| 200 | `ignored`, reason `not_job_event` | not a job event |
| 200 | `ignored`, reason `status` (`status`) | a job event other than `pending` |
| 200 | `ignored`, reason `project` | a project not in `gitlab.projectIds` |
| 200 | `ignored`, reason `unmapped_stage` (`stage`) | no pool lists the stage and there is no `defaultPool` |
| 200 | `ignored`, reason `own_job` | a job named `ciburst-*`, the component's own jobs |
| 200 | `reported`, `terminated`, `swept` (`runs`) | a VM report, a VM's termination, the cron sweep |
| 400 | `bad_request` | the body is not JSON, or a VM report is malformed |
| 401 | no body | wrong or missing hook, VM or sweep token |
| 500 | `error` | the provider failed (start, report, terminate, sweep) or the function's environment is incomplete; the body carries nothing more |

Every 200 answer of a job event except `ignored` also names the `pool`. A `grouped` job depends on its leader's delivery: `started` (the VM boots), `at_capacity` (the group waits for a slot) or 500 (the group waits for the next VM).

## Logs

Every line reads `<level> <message> key="value" ...` and never holds a secret.

| Source | Where | Lines |
|---|---|---|
| Dispatcher | the function's stderr, which Cockpit does not show (2026-10-24): read the answers on the webhook's recent events instead | `info` `claimed`, `started`, `grouped`, `duplicate`, `at_capacity` with `project`, `pipeline`, `job`, `stage`, `pool`, `run`; `warn` `claim_failed`, `ledger_failed`, `unauthorized`; `error` `start_failed`, `report_failed`, `terminate_failed`, `sweep_failed`, and `env` naming a missing variable |
| CLI | stderr; in pipeline mode the `ciburst-setup` job log | one line per action; `plan` warns `vm pools cannot start pool="<tag>" reason="..."`; `apply` logs `replayed` per replayed job, `replay_failed` and `output_unmasked` |
| VM agent | the VM's journal, `journalctl -u ciburst-vm-agent`, through the provider's console view only | `agent started`, `state from=... to=...` (`starting`, `idle`, `busy`, `draining`, `quitting`), `slot started`, `slot exited`, `report failed`, `terminate accepted`, `terminate failed`, `terminate gave up` |

## Rotation and updates

| Command | Replaces | Note |
|---|---|---|
| `ciburst rotate runner` | every pool's runner and token (deleted and recreated) | VMs started before hold the old token: rotate while no VM runs |
| `ciburst rotate hook` | the hook token and the sweep token | updates the webhooks, the function and the cron |
| `ciburst rotate key` | the dispatch key | the old key is deleted, the function redeployed |
| `ciburst rotate cache` | the cache key | the old key is deleted; VMs started before lose the cache: rotate while no VM runs |
| `ciburst rotate vm` | the VM secret | refused while VMs run, because their tokens would stop working |

Check after each: `ciburst plan` prints only `noop`; a test pipeline passes.

1. Yearly runtime bump: the new Rust version in `rust-toolchain.toml` and the host runtime (`RUNTIME` in `crates/scaleway/src/host.rs`), then in every file `scripts/check-pins.sh` lists (it runs in the `lint` job); tag a release; `ciburst apply`.
   Check: `lint` passes; the function is `ready`; a test pipeline passes.
2. Upstream images: bump the tags in `images/*/Dockerfile`.
   Check: `images/test/check-tags.sh` and the image jobs pass.
3. Agent: after a change in `crates/vm-agent`, `scripts/agent-binary.sh`, then `ciburst apply`; in pipeline mode a new `cli` image carries the agent and `ciburst-setup` uploads it. VMs created afterwards download the new object.
   Check: `CIBURST_AGENT_SHA256` in the outputs changed.

## VM runtime and the sweep

Lifecycle rules: spec section 6.1.

- cloud-init writes `/etc/ciburst/vm.env`, installs `gitlab-runner` 18.11.4, downloads the agent object and checks its sha256, then starts `ciburst-vm-agent.service` (`Restart=on-failure`, 5 s).
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

## Fast lanes

A fast lane is a Serverless Jobs pool for jobs that need no Docker daemon: it starts in seconds and bills per second, but it ignores `image:` (the pool's image is the job's environment) and reserves its stages, so a job in such a stage must carry the pool's tag and a job with the tag must use such a stage.

1. Config, in `ciburst.json` or `.local/config.json`:
   ```json
   {
     "provider": { "type": "scaleway", "region": "fr-par", "registry": true, "registryPublic": true },
     "pools": [
       { "tag": "vm", "runtime": "instances", "image": "ubuntu:24.04", "stages": [], "slots": 2, "instanceTypes": ["DEV1-M", "DEV1-L"], "maxRuns": 4 },
       { "tag": "rust", "runtime": "jobs", "image": "<registry endpoint>/rust:edge", "stages": ["rust"], "vcpu": 4, "memoryMiB": 8192, "maxRuns": 4 }
     ]
   }
   ```
   The image must be pullable without credentials, or live in the deployment's registry namespace, whose endpoint is the output `CIBURST_REGISTRY_ENDPOINT`. A job definition is refused while its image is missing: with `provider.registry`, `apply`, push the image, `apply` again.
   Check: `ciburst plan` lists `job definition <name>-rust`.
2. Jobs set both the stage and the tag, and `stages:` lists the stage:
   ```yaml
   clippy:
     stage: rust
     tags: [rust]
     script:
       - cargo clippy --all-targets -- -D warnings
   ```
   Check: `ciburst check` reports no problem; the job runs on the runner `<name>-rust`.

### Fast-lane images

| Image | From | Toolchain | User |
|---|---|---|---|
| `base` | `debian:bookworm-20261020-slim` | git, git-lfs, curl, bash | `runner` |
| `node` | `node:24.22.1-bookworm-slim` | Node 24 | `runner` |
| `rust` | `rust:1.96.0-bookworm` | Rust 1.96.0, rustfmt, clippy | `runner` |
| `playwright` | `mcr.microsoft.com/playwright:v1.64.1-noble` | Node, Playwright 1.64.1 browsers (pin `@playwright/test` to 1.64.1) | `runner` |

Every image adds `gitlab-runner` from `gitlab/gitlab-runner:v18.11.4` and `images/base/entrypoint.sh` as `ciburst-entrypoint`. The entrypoint needs `CI_SERVER_URL` and `RUNNER_TOKEN`; with `CACHE_TYPE=s3` also `CACHE_S3_SERVER_ADDRESS`, `CACHE_S3_BUCKET_NAME`, `CACHE_S3_BUCKET_LOCATION`, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`. A missing one exits 64 and names it. `CIBURST_DRY_RUN=1` prints the command with the token and the cache keys masked. With arguments the entrypoint runs them instead.

Add a toolchain:

1. `images/<image>/Dockerfile`: `FROM` the official image, then the runner `COPY`, the entrypoint `COPY`, the `runner` user without `.bash_logout`, `/etc/profile.d/ciburst-path.sh`, `/builds` and `/cache`, as in `images/node/Dockerfile`.
   Check: `images/test/check-tags.sh` prints `ok` for every `FROM`.
2. Add the image to the `case` in `images/test/smoke.sh` and an image job to `.gitlab-ci.yml`.
   Check: `images/test/smoke.sh <image>` ends with `smoke passed`; `images/test/smoke-all.sh` checks every image.
3. Merge to the default branch.
   Check: `registry.gitlab.com/ciburst/ciburst/<image>:edge` exists.
4. Add a fast lane with the image and its stages; `ciburst apply`.
   Check: `ciburst check` reports no problem.

## Failure modes

| Symptom | Cause | Action |
|---|---|---|
| `ciburst-setup` is missing from the pipeline | the default branch is not protected, or the pipeline runs on another branch | protect the default branch; run the pipeline there |
| `ciburst-setup` refuses to run, naming `CI_COMMIT_REF_PROTECTED` | an unprotected ref: protected variables would be missing | run it on the protected default branch |
| `ciburst-setup` fails naming a `CIBURST_*` variable | the variable is missing, unprotected, or scoped to another environment | add it protected with environment scope `ciburst` |
| `ciburst-setup` cannot pull `registry.gitlab.com/ciburst/ciburst/cli:<version>`, or the include fails | while `ciburst/ciburst` is private: no access to it, or the project is missing from its job token allowlist | ask its maintainers for access and the allowlist entry |
| `ciburst-setup` fails with 403 on `POST /user/runners`, naming `create_runner` and the Owner role | the token lacks the scope, or its user is not an Owner of the project | a new token of a project Owner with `api` and `create_runner` in `CIBURST_GITLAB_TOKEN` |
| setup log `output_unmasked key=<key>` | GitLab refused to mask that value | none; the variable stays protected and scoped to `ciburst` |
| Running `ciburst-destroy` with a variable is refused (403), or the UI offers no variable form | new projects allow no pipeline variables, which include variables typed into a manual job | set `CIBURST_DESTROY` as a project variable instead |
| destroy log `warn servers still ending`, `security group not deleted` or `project not deleted` | a VM was still terminating after 5 minutes | run `ciburst-destroy` (or `ciburst destroy`) again |
| apply log `replay_failed ... 403` | the token cannot list jobs (a fine-grained token without read access to jobs) | retry the waiting jobs by hand; use a token with the scope `api` |
| `apply` stops: the zone has no quota for the pool's types | Instance quota 0 for every type of the pool in the zone | set `provider.zone` to a zone the message names, or ask Scaleway support to raise the quota; `ciburst apply`; delete the security group `<name>-vm` of the old zone (console, Instances, Security groups) |
| `apply` stops naming `--agent` and `scripts/agent-binary.sh` | no agent binary given and no agent object in the bucket | developer path: `scripts/agent-binary.sh`, then `ciburst apply`; pipeline mode: the `cli` image carries the agent, check the component's `version` input |
| Jobs stay pending, the webhook shows failures | GitLab disabled the webhook after four failures | re-enable it; `ciburst apply` re-verifies it and replays waiting jobs |
| Deliveries answer 500, no server appears | the start failed: quota used up by other servers, provider credentials, or an incomplete Scaleway side (`scaleway: shared environment: ...` for added providers) | `ciburst plan` shows repairs; `ciburst apply`; quota as above |
| VMs stay `starting` until the sweep ends them | cloud-init failed: runner or agent download, a hash mismatch, or an incomplete `vm.env` (the agent exits 64 naming the key) | fix the cause, `ciburst apply`, re-run the job |
| A VM job stays pending while VMs of the pool idle | the job's stage maps to a fast lane | move the job to a stage of the VM pool; `ciburst check` |
| VMs start or slots get claimed for jobs that run on GitLab's hosted runners | the dispatcher sees no tags; those jobs' stages fall to the default pool, a VM pool | remove `defaultPool` (unmapped stages then start nothing) or list the hosted jobs' stages in a fast lane; started VMs end at minute 55 |
| A cold burst starts one VM per job; no delivery answers `grouped` | the ledger failed (`ledger_failed`): cache bucket missing, cache key rotated without `apply`, or more than 20 pages of intents | `ciburst apply` |
| A draining VM takes minutes to end | a slot cut off from GitLab waits 300 s between job requests, and `SIGQUIT` does not shorten that wait | none; the VM keeps reporting `draining` until its slots exit |
| Fast lane: job pending, a run started and exited idle | the stage maps to a pool the job is not tagged for | fix config or tags, `ciburst apply`, `ciburst check`, re-run |
| Fast lane: run `failed` right after start, job pending | image pull failure | fix the image or its tag, `ciburst apply`, re-run the job |
| Fast lane: run logs `missing environment variable` | job definition or run environment incomplete | `ciburst apply`; for keys `ciburst rotate cache` |
| Fast lane: run ends `out of memory` | files in `/tmp` count against memory | raise `memoryMiB` or write to the builds dir |
| Fast lane: costs beyond the free tier | Serverless Jobs free tier exceeded | consumption page; per-second prices apply |
| Function deploy fails with an unknown runtime | Scaleway retired `rust196` | runtime bump (Rotation and updates) |
| `hetzner: pool X: no server type started (cx33: resource_unavailable ...)` | the location is out of stock for every type | add types to `instanceTypes` or set `providers.hetzner.location` |
| Hetzner `resource_limit_exceeded` | the project's server or core limit | ask Hetzner for a higher limit or lower `maxRuns` |
| Hetzner `unauthorized` (401) or `forbidden` (403) | token deleted or read-only | a new Read & Write token, `ciburst apply` |
| Hetzner `rate_limit_exceeded` (429) | 3600 requests per hour per project | wait; look for report storms |
| `hetzner: pool X is not provisioned` | firewall missing or without its selector | `ciburst plan` shows the repair, `ciburst apply` |
| Hetzner VM swept from `starting` | cloud-init failed (agent download, GitLab unreachable); no SSH | the server's Console view: `/var/log/cloud-init-output.log`, `journalctl -u ciburst-vm-agent` |
| Primary IPs left after VMs ended | the automatic IPv4 was not deleted with its server | delete them under Primary IPs; they bill hourly |

## Acceptance drill

Config: the default pool `vm` (`runtime: instances`, `slots` 2, `maxRuns` 4) in a zone with quota, the fast lane `ci` (`runtime: jobs`) for stage `test`. Pipeline: `test/drill/gitlab-ci.yml` as the `.gitlab-ci.yml` of the drill consumer's branch `ciburst-drill`.

1. `ciburst apply`, then `ciburst plan`.
   Check: only `noop`; the security group `<name>-vm`, the cron `<name>-sweep` and the agent object exist.
2. `ciburst check --ref ciburst-drill`.
   Check: `every job matches the stage map`.
3. Cold pipeline: push an empty commit to `ciburst-drill` while no VM runs.
   Check: the five VM jobs start three VMs (deliveries `started` three times, `grouped` twice); `node`, `rust`, `postgres`, `dind` and `docker-build` pass on the runner `<name>-vm`, `fast-lane` on `<name>-ci`, `hosted` on a GitLab runner; `$CI_REGISTRY_IMAGE/ciburst-drill:<short sha>` exists.
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
| Fast-lane job, pending to running | 3.5 to 38.6 s, slowest on the first pull of a large image |
| Dispatcher answer, warm | 0.1 to 0.4 s |
| Cache restore on the next pipeline | 0.3 s |
| Function deploy | 5 to 13 minutes |

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
4. Hetzner, once `.local/hetzner.env` exists: the `vm` pool on `runtime: "hetzner"`, `apply`, a cold pipeline.
   Check: three CX33 servers with the labels; the firewall applied (`public_net.firewalls[].status` `applied`); the jobs pass; the drain at minute 55 deletes every server; Primary IPs empty afterwards.
   Result: ____

## `run-single` flags

Source: `gitlab-runner run-single --help` in `gitlab/gitlab-runner:v18.11.4` (`images/test/runner-help.sh`) and `commands/single.go` at `v18.11.4`; every flag below is present there.

| Flag | VM slot | Fast lane | Note |
|---|---|---|---|
| `--url`, `--token` | yes | yes | the runner does not read `RUNNER_TOKEN`, so the token is a flag |
| `--executor` | `docker` | `shell` | |
| `--shell bash` | | yes | |
| `--builds-dir /builds`, `--cache-dir /cache` | | yes | |
| `--docker-image` | the pool's `image` | | for jobs without `image:` |
| `--docker-privileged` | yes | | Docker-in-Docker |
| `--docker-volumes` | `/cache`, `/certs/client` | | `/certs/client` for Docker-in-Docker with TLS |
| `--docker-cache-dir` | `/var/lib/ciburst/slot-<n>/volumes` | | the executor names volumes after token, system id, project id and concurrent id (`executors/docker/volume.go`); every slot reports concurrent id 0, so without it two jobs of one project on one VM share `/builds` and `/certs/client`; with it the volumes are host directories `<dir>/<unique name>/<md5 of path>`, and `CI_PROJECT_DIR` stays `/builds/<namespace>/<project>` |
| `--config` | `/var/lib/ciburst/slot-<n>/config.toml` | | read only with `--runner`, so every slot of a VM gets the system id derived from the machine id, as one `gitlab-runner run` with `concurrent` slots does |
| `--max-builds 0` | yes | yes | no build limit |
| `--wait-timeout` | `0` | `waitTimeoutS` | 0 never exits for idleness (`checkFinishedConditions` tests `WaitTimeout > 0`); otherwise the timer restarts after every build |
| `--cache-type s3`, `--cache-shared`, `--cache-s3-server-address`, `--cache-s3-bucket-name`, `--cache-s3-bucket-location`, `--cache-s3-access-key`, `--cache-s3-secret-key` | yes | yes | with an S3 cache |

Job containers: the Docker executor labels build and service containers with `com.gitlab.gitlab-runner.job.id` (`executors/docker/internal/labels/labels.go`). A slot requests a job every 2 to 3 s and exits about three seconds after `SIGQUIT`, logging `Requested quit, waiting for builds to finish` (`images/test/run-single-slot.sh`). A runner whose requests fail waits `NotHealthyCheckInterval` (300 s) between requests (`common/consts.go`).

## Decisions

| Date | Decision |
|---|---|
| 2026-10-24 | The fast-lane entrypoint passes every value as a flag and unsets `RUNNER_TOKEN`, `CACHE_S3_ACCESS_KEY` and `CACHE_S3_SECRET_KEY` before `exec`, so job scripts do not inherit them. |
| 2026-10-24 | Releases use `glab release create` in `registry.gitlab.com/gitlab-org/cli:v1.124.0` with `GLAB_ENABLE_CI_AUTOLOGIN`; `release-cli` is deprecated since GitLab 18.0. |
| 2026-10-24 | Framework image builds use `docker:27.5.1` with `docker:27.5.1-dind`. |
| 2026-10-24 | VM slots mount `/certs/client` besides `/cache`, so Docker-in-Docker with TLS (`DOCKER_TLS_CERTDIR: "/certs"`) works as on GitLab's hosted runners. |
| 2026-10-24 | The agent starts slots with only `PATH` and `HOME` in their environment: the runner reads `CI_SERVER_URL`, `CACHE_*` and `DOCKER_*` from its environment, and the flags stay the only source. |
| 2026-10-24 | `free` counts running slots without a job; a draining VM reports `free` 0. The 30 s grace starts when the dispatcher accepted the `draining` report. |
