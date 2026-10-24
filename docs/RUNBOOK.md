# Runbook

## Facts

| Item | Value |
|---|---|
| Region | `fr-par` (provider and host) |
| Resources | project `<name>`, IAM applications `<name>-dispatch` and `<name>-cache`, bucket `<name>-cache-<project id>`, job definitions `<name>-<tag>`, namespace `<name>`, function `<name>-dispatch`; for VM pools security group `<name>-vm` and servers `<name>-<tag>-<8 hex>` |
| Pool images | `registry.gitlab.com/<namespace>/ciburst/{base,node,rust,playwright}` |
| VM agent image | `registry.gitlab.com/<namespace>/ciburst/vm-agent`, also pushed to `<registry endpoint>/vm-agent`; `FROM scratch` with the static binary `/ciburst-vm-agent` |
| Image tags | `edge` from the default branch; `X.Y.Z`, `X.Y`, `X`, `latest` from a tag `vX.Y.Z` |
| Binary | `ciburst-x86_64-unknown-linux-musl` in the framework project's generic package registry, linked from each release |
| Runner | `gitlab-runner` 18.11.4, `run-single`. Serverless Jobs pools: shell executor, one run per pending job, exits after `waitTimeoutS` idle seconds. VM pools: Docker executor, `slots` processes per VM |
| Instance quotas | per organization, zone and commercial type, resource `cp_servers_type_<TYPE>` with `_` for `-` (`cp_servers_type_DEV1_M`); read with `GET https://api.scaleway.com/iam/v1alpha1/quota/cp_servers_type_<TYPE>?organization_id=<org id>`. On 2026-10-24 the drill organization had 0 for DEV1, PLAY2 and BASIC3 types in `fr-par-1` to `fr-par-3`, and DEV1-S 10, DEV1-M 15, DEV1-L 5 in `nl-ams-1` to `-3`, `pl-waw-1` to `-3`, `it-mil-1` to `-3` and `se-sto-1` |
| Serverless Jobs limits | 6 vCPU, 16 GB, 24 h per run, 400 parallel runs per organization, `linux/amd64`, images under 2 GB recommended; `/tmp` counts against the run's memory |
| Hetzner (M4) | provider `hetzner` for pools with `runtime: "hetzner"`: firewall `<name>-vm`, servers `<name>-<tag>-<8 hex>` with labels `ciburst-*`, run ids `hetzner:<server name>`; API limit 3600 requests per hour per project |
| Burst intents (M4) | empty objects `ciburst/intents/<pool>/<unix s, 10 digits>-<job id, 20 digits>` in the cache bucket, expired by its lifecycle rule |

## Owner setup

1. Scaleway: create an API key with a policy on the organization holding `IAMManager`, `ProjectManager`, `ServerlessJobsFullAccess`, `FunctionsFullAccess`, `ObjectStorageFullAccess`. Write `.local/scaleway.env` with `SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID` (optional `SCW_DEFAULT_PROJECT_ID`), then `chmod 600 .local/scaleway.env`.
   Check: `ciburst plan --config .local/config.json --skip-gitlab` prints the provider and host steps without an authentication error.
2. GitLab: create a personal access token with scopes `api` and `create_runner` and an expiry. Write `GITLAB_TOKEN=<token>` to `.local/gitlab.env`, then `chmod 600 .local/gitlab.env`.
   Check: `ciburst plan --config .local/config.json` prints runner and hook steps.
3. Framework project `<namespace>/ciburst`: public, instance runners enabled (Settings, CI/CD, Runners), container registry visible to everyone.
   Check: the default-branch pipeline is green and `docker pull registry.gitlab.com/<namespace>/ciburst/node:edge` works without `docker login`.
4. Consumer: copy `config.example.json` to `.local/config.json`; set `name`, `gitlab.projectIds` (the consumer project id) and each pool's `image` (`:edge` until the first release tag, then `:<major>`).
   Check: `ciburst plan --config .local/config.json` exits 0.
5. Hetzner, only for pools with `runtime: "hetzner"`: in the Hetzner Console create a project used by ciburst alone (a token cannot be narrowed below a project), then Security, API tokens, Generate API token with permission Read & Write. Write `HCLOUD_TOKEN=<token>` to `.local/hetzner.env`, then `chmod 600 .local/hetzner.env`. Add `"providers": { "hetzner": {} }` to the config (optional `"location"`: `nbg1` default, `fsn1`, `hel1`; optional `"image"`: `docker-ce` default, any image must ship Docker), and give each Hetzner pool `"runtime": "hetzner"` (optional `"instanceTypes"`, default `cx33` then `cx43`; never `cax*`, the agent is amd64). Tags end with a letter or digit; `<name>-<tag>` stays within 54 characters. The registry namespace must be public (`provider.registryPublic`), because Hetzner VMs pull the agent from it.
   Check: `ciburst plan` shows `hetzner:firewall create`; after `apply` the function holds `HCLOUD_TOKEN` as a secret variable.

## Provisioning

| Step | Command | Check |
|---|---|---|
| 1 | `ciburst plan --config .local/config.json` | lists every step with `create` on a new account |
| 2 | `ciburst apply --config .local/config.json` | exits 0; `.local/outputs.env` holds `CIBURST_DISPATCH_URL` |
| 3 | `ciburst plan --config .local/config.json` | only `noop` lines |
| 4 | `ciburst check --config .local/config.json` | no problem lines for any consumer project |
| 5 | `ciburst status --config .local/config.json` | dispatcher URL, every pool, live runs |
| 6 | `ciburst rotate <runner\|hook\|key\|cache> --config .local/config.json` | the next `plan` prints only `noop`; a test job runs |
| 7 | `ciburst destroy --yes <name> --config .local/config.json` | `plan` lists every step as `create` again |

## `.local/`

Gitignored. Never committed.

| File | Written by | Mode | Content |
|---|---|---|---|
| `config.json` | owner | 644 | config with the real project ids |
| `scaleway.env` | owner | 600 | owner Scaleway key |
| `gitlab.env` | owner | 600 | owner GitLab token |
| `hetzner.env` | owner | 600 | `HCLOUD_TOKEN`, only with Hetzner pools |
| `outputs.env` | `ciburst apply` | 600 | `CIBURST_DISPATCH_URL`, `CIBURST_HOOK_TOKEN`, dispatch and cache keys, `CIBURST_RUNNER_ID_<TAG>`, `CIBURST_RUNNER_TOKEN_<TAG>` |
| `state.json` | `ciburst` | 600 | provider and host state |

## Consumer side

1. In the consumer's `.gitlab-ci.yml`:
   ```yaml
   include:
     - project: <namespace>/ciburst
       ref: v1
       file: ci/ciburst.yml
   ```
2. Jobs need nothing: `default: tags: [vm]` runs them on the VM pool with their `image:` and `services:`; Docker-in-Docker works with `docker:27-dind` and `DOCKER_TLS_CERTDIR: "/certs"`.
3. Image builds extend `.ciburst-image-build` (stage `build`, `docker:27` with `docker:27-dind` on the VM pool).
4. Fast lanes on Serverless Jobs extend `.ciburst-rust` (stage `rust`) or `.ciburst-playwright` (stage `e2e`). They ignore `image:` and have no Docker daemon; the playwright pool ships browsers for Playwright 1.64.1, so pin `@playwright/test` to 1.64.1. Declare the stages in `stages:`.
5. The job's stage selects the pool: VM jobs use stages no fast lane lists. A job for GitLab's hosted runners (`tags: [saas-linux-small-amd64]`) belongs in a fast-lane stage, so its pending event starts a cheap run instead of a VM.
Check: `ciburst check` reports no problem (`--ref <branch>` lints a branch before it is merged); a pipeline shows each job on a runner named `<name>-<tag>`.

## Pool images

| Image | From | Toolchain | User |
|---|---|---|---|
| `base` | `debian:bookworm-20261020-slim` | git, git-lfs, curl, bash | `runner` |
| `node` | `node:24.22.1-bookworm-slim` | Node 24 | `runner` |
| `rust` | `rust:1.96.0-bookworm` | Rust 1.96.0, rustfmt, clippy | `runner` |
| `playwright` | `mcr.microsoft.com/playwright:v1.64.1-noble` | Node, Playwright 1.64.1 browsers | `runner` |

Every image adds `gitlab-runner` from `gitlab/gitlab-runner:v18.11.4` and `images/base/entrypoint.sh` as `ciburst-entrypoint`. The entrypoint needs `CI_SERVER_URL` and `RUNNER_TOKEN`; with `CACHE_TYPE=s3` also `CACHE_S3_SERVER_ADDRESS`, `CACHE_S3_BUCKET_NAME`, `CACHE_S3_BUCKET_LOCATION`, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`. A missing one exits 64 and names it. `CIBURST_DRY_RUN=1` prints the command with the token and the cache keys masked. With arguments the entrypoint runs them instead.

Add a toolchain:

1. `images/<image>/Dockerfile`: `FROM` the official image, then the runner `COPY`, the entrypoint `COPY`, the `runner` user without `.bash_logout`, `/etc/profile.d/ciburst-path.sh`, `/builds` and `/cache`, as in `images/node/Dockerfile`.
   Check: `images/test/check-tags.sh` prints `ok` for every `FROM`.
2. Add the image to the `case` in `images/test/smoke.sh` and a job `image:<image>` to `.gitlab-ci.yml`.
   Check: `images/test/smoke.sh <image>` ends with `smoke passed`; `images/test/smoke-all.sh` checks every image.
3. Merge to the default branch.
   Check: `registry.gitlab.com/<namespace>/ciburst/<image>:edge` exists.
4. Add a pool with the image and its stages to `.local/config.json`; `ciburst apply`.
   Check: `ciburst check` reports no problem.

## VM runtime

A pool with `runtime: instances` runs jobs on Scaleway Instances (spec section 6.1). The dispatcher starts a VM for a pending job when no VM of the pool has a free slot. cloud-init writes `/etc/ciburst/vm.env`, installs `gitlab-runner` 18.11.4, copies `/ciburst-vm-agent` out of `<registry endpoint>/vm-agent:edge` and starts `ciburst-vm-agent.service`. The agent:

- runs `slots` processes of `gitlab-runner run-single --executor docker` (flags in the verification table below) and restarts one that exits while the VM is not draining;
- counts busy slots every 5 s as distinct `com.gitlab.gitlab-runner.job.id` labels over `docker ps`;
- posts `{state, free, slots}` to the dispatcher's `/vm` on every change and every 10 minutes, with headers `X-Ciburst-Run` and `X-Ciburst-Token`;
- drains when idle at minute 55 of the VM's hour (uptime), then posts `/terminate`; failed posts are retried for 10 minutes (2 s doubling to 60 s), then the agent exits 1 and systemd restarts it;
- logs one line per transition to its journal: `state from=... to=...`, `slot started`, `slot exited`, `report failed`, `terminate accepted`, `terminate failed`, `terminate gave up`. No token appears in the log.

| State | Meaning | Next |
|---|---|---|
| `starting` | created, no report accepted yet | `idle` or `busy` on the first accepted report |
| `idle` | every slot polls GitLab, no job | `busy`; `draining` at minute 55 |
| `busy` | at least one job runs | `idle` when no job container runs |
| `draining` | reported at minute 55, `free` 0; slots poll for 30 s more, then get `SIGQUIT` and finish their job | `busy` if a slot takes a job in the 30 s; terminated once every slot exited |

The state lives in the server's tags: `ciburst-state:<state>`, `ciburst-free:<n>`, `ciburst-seen:<unix time>` and, once draining, `ciburst-drain:<unix time>`, beside `ciburst`, `<name>`, `pool:<tag>`, `job:<id>`. See them in the console (Instances, the server, Tags) or with `GET https://api.scaleway.com/instance/v1/zones/<zone>/servers?tags=<name>` and header `X-Auth-Token: <secret key>`. The VM accepts no inbound connection.

The cron `<name>-sweep` (every 30 minutes) terminates a VM in `starting` for more than 10 minutes, in `draining` for more than 15 minutes, silent for more than 30 minutes, or older than 24 hours.

End a VM by hand, when the sweep does not:

1. Find the server: console Instances filtered by the tag `<name>`, or the `GET` above.
   Check: its `ciburst-seen` is older than 30 minutes, or no job of the pool runs on it in GitLab.
2. Terminate it: console, the server, Delete with its volumes; or `POST https://api.scaleway.com/instance/v1/zones/<zone>/servers/<server id>/action` with body `{"action": "terminate"}`. Never `poweroff` from inside the VM: standby is billed as running.
   Check: the server is gone from the list and no IP of it remains under Flexible IPs.
3. A job that ran on it fails once GitLab stops receiving its updates; re-run it.

Cost (Scaleway product catalog, `nl-ams-1`, 2026-10-24): billed per hour with a 60 minute minimum. DEV1-M (3 vCPU, 4 GB) 0.020196 EUR/h, the dynamic IPv4 0.005 EUR/h and the 40 GB local SSD root volume at 0.000049 EUR per GB-hour (0.00196 EUR/h), billed as its own line: 0.0272 EUR per VM-hour. IPv6 addresses are free, but VMs need IPv4: `registry.gitlab.com`, the Scaleway registry and API have no IPv6 address (2026-10-24). PLAY2-NANO (2 vCPU, 4 GB) 0.02754 EUR/h plus the IPv4 and a block volume, as it has no local storage.

## Burst grouping (M4)

A pending job of a VM pool first claims a free slot on a VM created more than 30 s ago: `idle` or `busy` and reported within 21 minutes, or `starting` and younger than 10 minutes. Without one, it leaves an intent in the cache bucket, waits 2 s and reads the intents of the last 30 s. Ranked by time and job id, every `slots`-th job starts a VM with its group's slots already taken; the others ride on that VM. The function writes and lists intents with the cache key; bucket and endpoint come from `CIBURST_PROJECT_ID` and `CIBURST_REGION`. A read follows at most 20 pages of 1000 keys.

| Result | Meaning |
|---|---|
| `claimed` | a free slot on a VM older than 30 s; `runId` names it |
| `started` | this job leads its burst group; a VM started with `taken` slots assigned |
| `grouped` | rides on the VM its leader starts; `leader` is that job's id |
| `duplicate` | GitLab delivered this job's pending event again within 30 s; nothing started |
| `at_capacity` | `maxRuns` reached by VMs older than 30 s plus leaders ranked earlier; the job waits for a free slot |

Log lines: `started ... taken="<n>"`, `grouped ... leader="<id>"`, `duplicate ... job="<id>"`, `warn ledger_failed pool="<tag>" job="<id>" error="..."` (this event used the M3 rule, one VM per job up to `maxRuns`).

Reading a `grouped` delivery (hook page, Recent events): its leader's delivery shows `started` with the VM sized for the group, `at_capacity` (the group waits for a slot) or 500 (the start failed; the group waits for the next VM). GitLab does not bind a job to the leader's VM: any VM of the pool that asks first takes it.

Inspect a pool's intents:

1. Console: Object Storage, bucket `<name>-cache-<project id>`, folder `ciburst/intents/<pool>/`.
2. CLI with `CIBURST_CACHE_ACCESS_KEY` and `CIBURST_CACHE_SECRET_KEY` from `.local/outputs.env` as the key pair: `aws s3api list-objects-v2 --endpoint-url https://s3.<region>.scw.cloud --region <region> --bucket <name>-cache-<project id> --prefix ciburst/intents/<pool>/ --start-after ciburst/intents/<pool>/<unix seconds>`.
   Check: a cold burst of N jobs at `slots` S shows N keys within about 2 s and `ceil(N / S)` new servers.
3. Never delete intents by hand. A key outside the format is ignored; a forged key in the format costs at most one VM-hour.

Known limits, each at most one VM-hour: a VM created in the 2 to 3 s after its intent left the 30 s window counts neither as old nor by rank, so a new burst at that instant can pass `maxRuns` by one; a repeated delivery within the same second is not recognised as a duplicate.

## Hetzner (M4)

Same agent, cloud-init and lifecycle as the VM runtime above; state lives in server labels (`ciburst-state`, `ciburst-free`, `ciburst-seen`, `ciburst-drain`, plus `ciburst`, `ciburst-name`, `ciburst-pool`, `ciburst-job`). The firewall `<name>-vm` has no rules and is applied to the label selector `ciburst-name==<name>`, so no inbound traffic reaches a VM; cloud-init also turns SSH password login off. A start is refused while the firewall is missing or lost its selector.

Check in the console: Servers (labels per VM), Firewalls (`<name>-vm`, no rules, the selector lists the running VMs), Primary IPs (empty while no VM runs). CLI: `hcloud server list -l ciburst-name=<name>`, `hcloud firewall describe <name>-vm`.

End a VM by hand: Console, the server, Delete; or `DELETE https://api.hetzner.cloud/v1/servers/<id>` with `Authorization: Bearer <token>`. Powered-off servers still bill.

Cost (hetzner.com/cloud/cost-optimized, 2026-10-24): CX33 (4 vCPU, 8 GB, 80 GB) 0.0144 EUR/h, the price page includes the IPv4 while the invoice lists the Primary IP as its own line; every started hour is billed, IPv6 is free.

## Failure modes

| Symptom | Cause | Action |
|---|---|---|
| Jobs stay pending, hook shows failures | GitLab disabled the hook after four failures | re-enable the hook; `ciburst apply` re-verifies it |
| Run `failed` right after start, job pending | image pull failure | fix the image or the tag in the config, `ciburst apply`, re-run the job |
| Delivery with status 5xx on the hook page | dispatcher error | read the function log in Cockpit, fix, resend the delivery from the hook page |
| Job pending, a run started and exited idle | stage mapped to a pool the job is not tagged for | fix config or tags, `ciburst apply`, `ciburst check`, re-run |
| A run per hosted-runner job | the dispatcher sees no tags, starts a run for the job's stage | none; the run exits after `waitTimeoutS` |
| Runs log `missing environment variable` | job definition or run env incomplete | `ciburst apply`; for keys `ciburst rotate cache` |
| Run ends `out of memory` | files in `/tmp` count against memory | raise `memoryMiB` or write to the builds dir |
| Costs beyond the free tier | free tier exceeded | consumption page; per-second prices apply |
| Function deploy fails with an unknown runtime | Scaleway retired `rust196` | runtime bump below |
| VMs stay `starting` until the sweep ends them | cloud-init failed: runner download, agent image pull (`registryPublic`), or `vm.env` incomplete (the agent exits 64 naming the key) | fix the image or the config, `ciburst apply`, re-run the job |
| VM jobs stay pending, no server appears, the function log shows `instance type unavailable` with `quota(s) exceeded` | the zone's quota for every type of the pool is 0 | set `provider.zone` to a zone with quota (Facts, Instance quotas) or ask Scaleway support to raise it, `ciburst apply`, delete the security group `<name>-vm` of the old zone (console, Instances, Security groups), re-run the jobs |
| VMs start or slots get claimed for a pipeline whose jobs all run on hosted runners | the dispatcher sees no tags; the jobs' stages fall to the default pool, a VM pool | remove `defaultPool` (unmapped stages then start nothing) or make a fast lane the default, list the VM pool's stages, `ciburst apply`; started VMs end at minute 55 |
| Function log shows `ledger_failed` | cache bucket missing, cache key rotated without `ciburst apply`, or more than 20 pages of intents | `ciburst apply`; until then every job starts its own VM |
| `hetzner: pool X: no server type started (cx33: resource_unavailable ...)` | the location is out of stock for every type | add types to `instanceTypes` or set `providers.hetzner.location` |
| Hetzner `resource_limit_exceeded` | the project's server or core limit | ask Hetzner for a higher limit or lower `maxRuns` |
| Hetzner `unauthorized` (401) or `forbidden` (403) | token deleted or read-only | new Read & Write token in `.local/hetzner.env`, `ciburst apply` |
| Hetzner `rate_limit_exceeded` (429) | 3600 requests per hour per project | wait; look for report storms in the function log |
| `hetzner: pool X is not provisioned` | firewall missing or without its selector | `ciburst plan` shows the repair, `ciburst apply` |
| `scaleway: shared environment: ...` on a Hetzner start | registry off, private, or not provisioned | `provider.registry` and `provider.registryPublic` true, `ciburst apply` |
| Hetzner VM swept from `starting` | cloud-init failed (agent pull, GitLab unreachable); no SSH | server Console view: `/var/log/cloud-init-output.log`, `journalctl -u ciburst-vm-agent` |
| Primary IPs left after VMs ended | the automatic IPv4 was not deleted with its server | delete them under Primary IPs; they bill hourly |
| A `vm` job stays pending while VMs of the pool idle | the job's stage maps to a fast lane | move the job to a stage of the VM pool, `ciburst check` |
| A draining VM takes minutes to end | a slot cut off from GitLab waits 300 s between job requests, and `SIGQUIT` does not shorten that wait | none; the VM keeps reporting `draining` until its slots exit |

## Rotation and updates

1. Secrets: `ciburst rotate runner`, `hook`, `key` or `cache`. `rotate hook` also replaces the VM secret: running VMs can no longer report or end themselves, and the sweep removes them within an hour, with any job still running there; rotate it while the VM pools are idle.
   Check: `ciburst plan` prints only `noop`; a test pipeline passes.
2. Yearly runtime bump: set the new Rust version in `rust-toolchain.toml`, the host runtime in `crates/scaleway`, `.gitlab-ci.yml`, `images/rust/Dockerfile` and `images/vm-agent/Dockerfile`; tag a release; `ciburst apply`.
   Check: the function is `ready` and a test pipeline passes.
3. Upstream images: bump the tags in `images/*/Dockerfile`.
   Check: `images/test/check-tags.sh` and the `images` stage are green.
4. Owner keys after M2: delete the Scaleway key and the GitLab token; recreate the token only for `check` and `rotate`.

## M2 drill

Date: 2026-10-24. Region `fr-par`, config name `ciburst-dev`, images in the project's Scaleway registry.

1. `ciburst apply` on the Scaleway project.
   Result: done after five fixes below; second `plan` only `noop`: yes (17 unchanged).
2. `ciburst check` on the drill consumer.
   Result: valid pipeline, 30 untagged jobs; every job needs a pool tag or a hosted runner tag.
3. Pipeline with a node, a rust, a playwright and a kaniko job plus one `saas-linux-small-amd64` job (drill consumer branch `ciburst-drill`).
   Result: node, rust, playwright pass; kaniko fails (item 5); the `saas-linux-small-amd64` job runs on GitLab's shared runners (0.1 s queued), never on a pool.
4. Start latency.
   Dispatcher answer: 0.1 to 0.4 s warm. Run queued to running: 14 s (`ci`, node image). Synthetic pending event, no job to take: run `succeeded` 111 s after running, of which 60 s idle window.
   Per pool with a real job, queued then run: ci 3.5 to 6.9 s and 8 to 11 s; rust 27.5 to 28.5 s and 7.8 s; playwright 5.7 to 38.6 s and 9.6 to 17.9 s; first pulls of the larger images are the slow cases.
5. Kaniko verdict: multi-stage build and push from the `build` pool under gVisor.
   Result: fails. Unpacking the base rootfs needs `chown`, which the sandbox denies (`chown /etc/shadow: operation not permitted`), for RUN and for COPY. The build pool is removed.
6. Cache hit on the second run of the same job.
   Result: yes. First run uploads to the bucket in 4.7 s; the next pipeline restores it in 0.3 s (`Successfully extracted cache`).
7. Cost on the Scaleway consumption page after the drill.
   Result: 0 EUR for compute. Month to date on 2026-10-24, M2 and M3 together: Serverless Jobs 12838 vCPU-s and 27570 GB-s (0.19 EUR) and the function, all inside the free tier; Object Storage 0.01 EUR and Container Registry 0.01 EUR.
8. Logs.
   Result: Cockpit's Scaleway logs source holds bucket logs only; function build logs and function logs are not exported there (2026-10-24). Builder failures are diagnosed with probe functions.
9. Image push into the project registry, five images.
   Result: 3 min (rust 60 s, playwright 81 s).
10. Function build on Scaleway (`rust196`).
   Result: about 5 min per deploy.

Fixes found by the drill, all in the code since 2026-10-24:

| Finding | Fix |
|---|---|
| A job definition is rejected with `registry image not found` while its image is missing | push the images before the definitions; with `provider.registry` run `apply`, push, `apply` again |
| Function variables may not start with `SCW_` | the function reads `CIBURST_DISPATCH_KEY`, `CIBURST_ORGANIZATION_ID`, `CIBURST_PROJECT_ID`, `CIBURST_REGION` |
| Scaleway's Rust builder fails on workspace inheritance | `build.rs` writes every inherited value into the zipped manifests |
| A failed apply lost the IAM secrets Scaleway shows once | outputs are saved after every provider step |
| Starting a job with a private registry image needs registry read | the dispatch policy grants `ContainerRegistryReadOnly`; older policies are repaired |

## M3 drill

Date: 2026-10-24, VMs in `nl-ams-1` (`provider.zone`; the organization has no DEV1 quota in the Paris zones). Config: default pool `vm` (`runtime: instances`), fast lane `ci` (`runtime: jobs`) for stage `test`, `provider.registryPublic: true`. Pipeline `test/drill/gitlab-ci.yml` as `.gitlab-ci.yml` of the drill consumer branch `ciburst-drill`.

1. Default-branch pipeline of the framework project with `CIBURST_SCW_REGISTRY` and `CIBURST_SCW_REGISTRY_KEY` set.
   Check: `image:vm-agent` passes; `<registry endpoint>/vm-agent:edge` pulls without login.
   Result: not run, the framework project is not pushed yet. The image was built with `images/vm-agent/build.sh` and pushed by hand; every VM pulled it without login.
2. `ciburst apply`, then `ciburst plan`.
   Check: the plan prints only `noop`; the security group `<name>-vm` and the cron `<name>-sweep` exist.
   Result: passed after the zone change: apply created `<name>-vm` in `nl-ams-1`, the plan printed 14 unchanged. The group left in `fr-par-1` was deleted by hand.
3. `ciburst check --ref ciburst-drill` on the drill consumer project.
   Check: no problem line.
   Result: every job matches the stage map. Without `--ref` the check lints the default branch, which is not converted and reports its untagged jobs.
4. Run the drill pipeline.
   Check: `node`, `rust`, `postgres`, `dind`, `docker-build` pass on runner `<name>-vm`; `fast-lane` on `<name>-ci`; `hosted` on a GitLab runner; `$CI_REGISTRY_IMAGE/ciburst-drill:<short sha>` exists.
   Result: passed in the third run.
   - Run 1, GitLab's deliveries replayed after the zone change: 5 of 7. `node` and `rust` ran at the same time on one VM and broke each other's checkout: every slot reports concurrent id 0, so both got the same `/builds` volume. Fixed with a `--docker-cache-dir` per slot.
   - Run 2, five new VMs: 6 of 7. `node` and `rust` overlapped on one VM with clean checkouts, `postgres` and `dind` overlapped without a certificate clash. `rust` failed because the consumer's `rust-toolchain.toml` pins another toolchain; the drill job now sets `RUSTUP_TOOLCHAIN`.
   - Run 3, warm VMs: 7 of 7, five VM jobs on four VMs, the drill image pushed.
5. VM start: job pending to running for the first VM job (create, boot, cloud-init, first poll).
   Result: 47 to 50 s in run 2 (DEV1-M, `nl-ams-1`), 66 s in run 1.
6. Tags while the pipeline runs: `starting`, then `idle` or `busy` with `ciburst-free`, `ciburst-seen` refreshed.
   Result: passed. `starting` at create, `idle` with `ciburst-free:2` about 45 s later, `busy` while a job container runs, `idle` after it; `ciburst-seen` was under 2 minutes old on every idle VM. A slot that pulls its job's image has no labelled container yet and counts as free, for 10 to 50 s per job in the drill.
7. Longest wait of a drill job against one job duration plus one VM boot.
   Result: 51 s queued with new VMs (run 2), 3.4 s on warm VMs (run 3); jobs ran 19 to 66 s.
8. End by callback: each VM reports `draining` and is gone between minute 55 and 60 of its hour.
   Result: passed. Run 1's four VMs reported `draining` 55 min 26 s to 55 min 31 s after create and were gone 67 to 86 s later; run 2's five VMs reported `draining` 55 min 17 s to 55 min 32 s after create and were gone within 2 minutes. No server, IP or volume remained.
9. Sweep: create a powered-on server in the VM zone that never reports, tagged `ciburst`, `<name>`, `pool:<tag>`, `ciburst-state:starting` (no user data).
   Check: the first sweep after its tenth minute terminates it; VMs that report stay.
   Result: passed. The DEV1-S test server, created at minute 26, was terminated by the sweep at minute 0 of the next hour and gone 15 s later; the four reporting VMs stayed.
10. Cost per drill pipeline on the consumption page (Instances, IPs, Serverless Jobs).
    Result: 0.29 EUR for the whole drill: nine DEV1-M hours (four in run 1, five shared by runs 2 and 3) and the one-hour DEV1-S sweep test, with 10 IPv4 hours and 760 GB-hours of local SSD. The local SSD was billed for two hours per VM, as each VM spanned two clock hours, so a VM-hour cost about 0.030 EUR in practice. A cold pipeline with five VM jobs costs about 0.15 EUR, a warm one nothing extra; Serverless Jobs stayed inside the free tier.

## M4 drill

Date: 2026-10-24. Config: as the M3 drill, VMs in `nl-ams-1`, `slots` 2, `maxRuns` 4, default pool `ci`; the function at the M4 code.

1. Cold burst on Scaleway: push an empty commit to `ciburst-drill` with no VM running.
   Check: five VM jobs start three VMs; delivery results `started` three times and `grouped` twice; no `ledger_failed`; five intent keys in the bucket; every job passes.
   Result: passed. Seven of seven jobs green on three DEV1-M VMs (M3: five). The five intents carry the same second; ranked by job id, the first, third and fifth job started VMs with `ciburst-free` 0, 0 and 1, the second and fourth answered `grouped` with their leader. Each pair started on its VM within 20 ms; the VM jobs queued 59.6 to 66.5 s, the 2 s group wait included.
2. Warm reuse: a second pipeline within the VMs' paid hour.
   Check: `claimed` for every VM job, no new VM.
   Result: passed. Five `claimed` answers 3 minutes after the cold run, no new VM, every VM job queued 3.1 to 3.3 s. Three of the five claims named the same VM: parallel calls read the same free counts, and the job id spreads them only statistically; any polling slot takes the job anyway.
3. Swap: `/proc/swaps` read by a drill job.
   Result: passed. `/swapfile  file  4194300  268  -2` on the VM that ran the node job.
4. Cost of runs 1 and 2 on the consumption page.
   Result: ____ EUR (read after the VMs ended)
5. Without `defaultPool` (D36): a pending event of an unmapped stage.
   Result: passed. `200 {"reason":"unmapped_stage","result":"ignored","stage":"deploy"}`, nothing started; the drill consumer's own pipelines on hosted runners no longer start runs.
6. Hetzner, once `.local/hetzner.env` exists: the `vm` pool on `runtime: "hetzner"`, `apply`, a cold pipeline.
   Check: three CX33 servers with the labels; firewall applied (`public_net.firewalls[].status` `applied`); jobs pass; drain at minute 55 deletes every server; Primary IPs empty afterwards.
   Result: ____

## `run-single` flag verification

Date: 2026-10-24. Source: `gitlab-runner run-single --help` in `gitlab/gitlab-runner:v18.11.4` (`images/test/runner-help.sh`), and `commands/single.go` at `v18.11.4`.

| Flag | Help text (env) | Result |
|---|---|---|
| `--url` | GitLab instance URL (`CI_SERVER_URL`) | present |
| `--token` | Runner token (`CI_SERVER_TOKEN`) | present; `RUNNER_TOKEN` is not read by the runner, so the entrypoint passes it |
| `--executor shell` | (`RUNNER_EXECUTOR`) | present |
| `--shell bash` | (`RUNNER_SHELL`) | present |
| `--builds-dir` | (`RUNNER_BUILDS_DIR`) | present |
| `--cache-dir` | (`RUNNER_CACHE_DIR`) | present |
| `--max-builds 0` | builds before exiting, default 0 | present; 0 runs until the idle window ends |
| `--wait-timeout` | seconds before receiving the first job | present; the timer restarts after every build (`postBuild` sets `lastBuild`) |
| `--cache-type s3` | (`CACHE_TYPE`) | present |
| `--cache-shared` | (`CACHE_SHARED`), boolean | present |
| `--cache-s3-server-address` | (`CACHE_S3_SERVER_ADDRESS`) | present |
| `--cache-s3-bucket-name` | (`CACHE_S3_BUCKET_NAME`) | present |
| `--cache-s3-bucket-location` | (`CACHE_S3_BUCKET_LOCATION`) | present |
| `--cache-s3-access-key` | (`CACHE_S3_ACCESS_KEY`) | present |
| `--cache-s3-secret-key` | (`CACHE_S3_SECRET_KEY`) | present |

Smoke result 2026-10-24: every image starts `run-single` as its user against an unreachable URL and exits with `has not received a job` after `RUNNER_WAIT_TIMEOUT=2`.

Docker executor flags of the VM slots, date 2026-10-24, same sources; cache flags as above:

| Flag | Help text (env) | Result |
|---|---|---|
| `--executor docker` | (`RUNNER_EXECUTOR`) | present |
| `--docker-image` | Docker image to be used (`DOCKER_IMAGE`) | present; the pool's `image` for jobs without `image:` |
| `--docker-privileged` | Give extended privileges to container (`DOCKER_PRIVILEGED`) | present |
| `--docker-volumes` | Bind-mount a volume and create it if it doesn't exist prior to mounting, repeatable (`DOCKER_VOLUMES`) | present; `/cache` and `/certs/client` |
| `--docker-cache-dir` | Directory where to store caches (`DOCKER_CACHE_DIR`) | present; `/var/lib/ciburst/slot-<n>/volumes` per slot. The executor names its volumes after token, system id, project id and concurrent id (`ProjectRealUniqueName`, `executors/docker/volume.go`); every slot reports concurrent id 0, so without it two jobs of one project on one VM share `/builds` and `/certs/client` (M3 drill). With it the volumes are host directories `<dir>/<unique name>/<md5 of path>` (`createHostBasedCacheVolume`); `CI_PROJECT_DIR` stays `/builds/<namespace>/<project>` |
| `--max-builds 0` | How many builds to process before exiting, default 0 | present; 0 runs forever |
| `--wait-timeout 0` | How long to wait in seconds before receiving the first job, default 0 | present; 0 never exits for idleness (`checkFinishedConditions` tests `WaitTimeout > 0`) |
| `--config` | Config file (`CONFIG_FILE`) | present; read only with `--runner`, so every slot of a VM gets the system id derived from the machine id (`configfile.GenerateUniqueSystemID`), as one `gitlab-runner run` with `concurrent` slots does |

Job containers: the Docker executor labels build and service containers with `com.gitlab.gitlab-runner.job.id` (`executors/docker/internal/labels/labels.go`, applied in `executors/docker/docker.go`, v18.11.4).

Slot smoke 2026-10-24 (`images/test/run-single-slot.sh`): the pinned runner with the slot flags requests a job every 2 to 3 s from a stub GitLab answering 204 (5 requests in 12 s) and exits with code 0 three seconds after `SIGQUIT`, logging `Requested quit, waiting for builds to finish`. A runner whose requests fail is unhealthy and waits `NotHealthyCheckInterval` (300 s) between requests (`common/consts.go`).

## Decisions

| Date | Decision |
|---|---|
| 2026-10-24 | Runner pinned to `gitlab/gitlab-runner:v18.11.4`. |
| 2026-10-24 | No build pool: kaniko fails in Scaleway's sandbox with `chown /etc/shadow: operation not permitted` while unpacking any base image (RUN and COPY alike); image builds run on hosted runners. |
| 2026-10-24 | The entrypoint passes every value as a flag and unsets `RUNNER_TOKEN`, `CACHE_S3_ACCESS_KEY` and `CACHE_S3_SECRET_KEY` before `exec`, so job scripts do not inherit them. |
| 2026-10-24 | The shell executor runs job steps in a bash login shell: every image writes its `PATH` to `/etc/profile.d/ciburst-path.sh` and the `runner` user has no `.bash_logout`. |
| 2026-10-24 | Releases use `glab release create` in `registry.gitlab.com/gitlab-org/cli:v1.124.0` with `GLAB_ENABLE_CI_AUTOLOGIN`; `release-cli` is deprecated since GitLab 18.0. |
| 2026-10-24 | Image builds use `docker:27.5.1` with `docker:27.5.1-dind`. |
| 2026-10-24 | Consumer image builds run on the VM pool (`.ciburst-image-build`: `docker:27`, `docker:27-dind`); the VM's runner is privileged. |
| 2026-10-24 | VM slots mount `/certs/client` besides `/cache`, so Docker-in-Docker with TLS (`DOCKER_TLS_CERTDIR: "/certs"`) works as on GitLab's hosted runners (GitLab docs, Docker-in-Docker with TLS in the Docker executor). |
| 2026-10-24 | The agent starts slots with only `PATH` and `HOME` in their environment: the runner reads `CI_SERVER_URL`, `CACHE_*` and `DOCKER_*` from its environment, and the flags stay the only source. |
| 2026-10-24 | `free` counts running slots without a job; a draining VM reports `free` 0. The 30 s grace starts when the dispatcher accepted the `draining` report. |
| 2026-10-24 | The VM agent image is `FROM scratch` with the static `x86_64-unknown-linux-musl` binary (rustls, no OpenSSL), built in `rust:1.96.0-bookworm` with `musl-tools` by `images/vm-agent/build.sh`. |
