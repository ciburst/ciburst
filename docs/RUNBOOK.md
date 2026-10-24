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

## Owner setup

1. Scaleway: create an API key with a policy on the organization holding `IAMManager`, `ProjectManager`, `ServerlessJobsFullAccess`, `FunctionsFullAccess`, `ObjectStorageFullAccess`. Write `.local/scaleway.env` with `SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID` (optional `SCW_DEFAULT_PROJECT_ID`), then `chmod 600 .local/scaleway.env`.
   Check: `ciburst plan --config .local/config.json --skip-gitlab` prints the provider and host steps without an authentication error.
2. GitLab: create a personal access token with scopes `api` and `create_runner` and an expiry. Write `GITLAB_TOKEN=<token>` to `.local/gitlab.env`, then `chmod 600 .local/gitlab.env`.
   Check: `ciburst plan --config .local/config.json` prints runner and hook steps.
3. Framework project `<namespace>/ciburst`: public, instance runners enabled (Settings, CI/CD, Runners), container registry visible to everyone.
   Check: the default-branch pipeline is green and `docker pull registry.gitlab.com/<namespace>/ciburst/node:edge` works without `docker login`.
4. Consumer: copy `config.example.json` to `.local/config.json`; set `name`, `gitlab.projectIds` (the consumer project id) and each pool's `image` (`:edge` until the first release tag, then `:<major>`).
   Check: `ciburst plan --config .local/config.json` exits 0.

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
   Result: ____ EUR
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
   Result: passed. Run 1's four VMs reported `draining` 55 min 26 s to 55 min 31 s after create and were gone 67 to 86 s later; no server, IP or volume remained.
9. Sweep: create a powered-on server in the VM zone that never reports, tagged `ciburst`, `<name>`, `pool:<tag>`, `ciburst-state:starting` (no user data).
   Check: the first sweep after its tenth minute terminates it; VMs that report stay.
   Result: passed. The DEV1-S test server, created at minute 26, was terminated by the sweep at minute 0 of the next hour and gone 15 s later; the four reporting VMs stayed.
10. Cost per drill pipeline on the consumption page (Instances, IPs, Serverless Jobs).
    Result: ____ EUR

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
