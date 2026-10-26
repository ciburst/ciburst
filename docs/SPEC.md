# Spec: ciburst, GitLab runners that exist only while jobs run

Status: M5 roundup done 2026-10-25; M6 serverless lanes (D45 to D47) drilled on the demo, the drill consumer runs on lanes; M6.1 warm start (D48, D49) and the M7 hardening (D50 to D52) built and drilled; M6.2 queue first (D53) built, spill opt-in (D54, D55), late-event guard (D56); M6.3 fleet (D57 to D59) specified; then M7 go public; the Hetzner drill waits for the owner's token.

## 0. Decision log

| Id | Date | Decision |
|---|---|---|
| D1 | 2026-10-23 | A framework with two ports (compute provider, dispatch host) and a provider-agnostic core. Scaleway is the first implementation of both ports. |
| D2 | 2026-10-23 | One ephemeral runner process per pending GitLab job, started by a job webhook, running `gitlab-runner run-single` in a serverless container. No runner manager, no fleeting plugin, no Instances, no fixed cost. The earlier Instance designs stay in git history only. |
| D3 | 2026-10-24 | Rust, edition 2024, one Cargo workspace: `crates/core` (library with the traits), `crates/cli` (binary `ciburst`), one crate per provider and host. Replaces the Node M0 of 2026-10-23, which stays in git history. |
| D4 | 2026-10-23 | Shell executor inside the pool image. The `image:` keyword of a job is ignored; the job's tag selects the pool and therefore the image. |
| D5 | 2026-10-23 | No Docker daemon on Scaleway (capability flag `docker_daemon: false`). Image builds and jobs that need dind keep the hosted-runner tag (D29). |
| D6 | 2026-10-23 | Secrets live in the dispatcher's secret environment (free). Runner tokens and the cache key are passed to each run as environment overrides. |
| D7 | 2026-10-23 | Pool images are built by the framework's CI and published to the framework project's GitLab registry, public pull. |
| D8 | 2026-10-23 | Name `ciburst`. The config value `name` prefixes every cloud resource, so the code name stays stable. |
| D9 | 2026-10-23 | Cache on a Scaleway Object Storage bucket, shared between runs, objects expire after 30 days. |
| D10 | 2026-10-23 | The dispatcher never calls GitLab. It routes by the job's stage through a stage-to-pool map in config; unmapped stages go to the default pool, or start nothing without one (D36). One runner (own token, own tag) per pool, so a job can only ever run in its pool's image. |
| D11 | 2026-10-23 | Runs use `--max-builds 0 --wait-timeout <pool.waitTimeoutS>`: a run keeps taking jobs while they arrive and exits after the idle window. A per-pool `maxRuns` caps parallel runs. |
| D12 | 2026-10-23 | Delivery: M0 by the orchestrator, M1 by three Opus agents in worktrees, an explicit go per milestone, pushes only on confirmation. |
| D13 | 2026-10-23 | License Apache-2.0. |
| D14 | 2026-10-23 | v1 pools: `ci` (node 24, default), `rust`, `playwright`. |
| D15 | 2026-10-23 | `ciburst check` lints every consumer pipeline against the stage map (CI lint API, owner token) so a job never waits for a pool that will not take it. A scheduled sweep for lost deliveries is backlog. |
| D16 | 2026-10-23 | Region `fr-par`. Drill consumer: a project whose id lives in `.local/` only. |
| D17 | 2026-10-24 | The dispatcher is a Scaleway Serverless Function on runtime `rust196`. Scaleway compiles the uploaded source, so the workspace is pinned to Rust 1.96.0 (`rust-toolchain.toml`) and CI builds with the same version. A runtime bump is a one-line change plus a redeploy. |
| D18 | 2026-10-24 | Few audited dependencies instead of none: tokio, reqwest with rustls, serde, clap, async-trait, sha2, subtle, rand, thiserror. `cargo deny` gates licenses, advisories and sources in CI. The HTTP seam is one trait, so every API client is tested against in-memory fakes. |
| D19 | 2026-10-24 | Pool images pin `gitlab/gitlab-runner:v18.11.4`; every `run-single` flag was verified against its help text. A move to 19.x follows the M2 drill. |
| D20 | 2026-10-24 | The `build` pool is the osscontainertools kaniko fork, alpine variant (ships git), with builds dir `/kaniko/builds`: kaniko wipes the filesystem between stages but never `/kaniko`. Withdrawn by D29. |
| D21 | 2026-10-24 | Every image writes its PATH to `/etc/profile.d/ciburst-path.sh` and removes `~/.bash_logout`, because the shell executor runs job steps in a login shell. |
| D22 | 2026-10-24 | Releases on tags `vX.Y.Z` publish the five images as `X.Y.Z`, `X.Y`, `X` and `latest` (`edge` on `main`) and the Linux x86_64 musl binary through `glab`. Configs reference images by major tag (`node:1`), or `edge` before the first release. macOS and Windows binaries are backlog. |
| D23 | 2026-10-24 | Adopters set up through their own pipeline, not from a workstation: a manual `ciburst-setup` job from the include runs the `ciburst` binary on a GitLab hosted runner (free 400 minutes per month), takes the Scaleway key and a personal access token from four masked CI variables, and stores the outputs as masked project variables instead of `.local/outputs.env`. The local CLI with `.local/` stays the developer path; the owner's personal token is used only for the M2 drill. |
| D24 | 2026-10-24 | Hosting: canonical on GitLab.com in the group `ciburst`, project `ciburst/ciburst`, private until v1.0.0, then public with a read-only push mirror to the GitHub organization `ciburst`. Issues and merge requests stay on GitLab. |
| D25 | 2026-10-24 | No GitLab for Open Source program, so a commercial service stays possible. The framework's CI runs on the Free tier's 400 minutes per month; path rules and merge-request pipelines keep it there. |
| D26 | 2026-10-24 | Project domain `ciburst.zwa.la`: the docs site on GitLab Pages at v1.0.0. It is the one real hostname allowed in the repository. |
| D27 | 2026-10-24 | Contributions under the Developer Certificate of Origin (`Signed-off-by`), copyright "The ciburst Authors", commit authors unchanged. Policy files (`CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, `NOTICE`) land with v1.0.0. |
| D28 | 2026-10-24 | Pool images: during the private phase CI pushes them into a Scaleway Container Registry namespace of the ciburst project (private, intra-region pulls free, storage 0.027 EUR per GB per month), created by the provider's optional `registry` step; Serverless Jobs pull from it without credentials. From v1.0.0 the public GitLab registry is canonical for adopters and the Scaleway mirror stays an option (public images free up to 75 GB). |
| D30 | 2026-10-24 | Pools choose their provider (`pools[].provider`, default the config's `provider`). The default provider becomes `scaleway-instances`: one VM per run with a local Docker daemon and GitLab's Docker executor, so per-job `image:`, `services:`, Docker-in-Docker, image builds and testcontainers work as on GitLab's own runners. Serverless Jobs pools stay as optional fast lanes for jobs without a daemon. Other providers (Fly Machines, Lambda MicroVMs, spot VMs, remote builders) are backlog. |
| D31 | 2026-10-24 | A VM ends itself through the dispatcher, never with a cloud key: Scaleway bills a guest `poweroff` as standby, and IAM cannot limit a key to one Instance. Each VM gets a capability token (HMAC of its server id with a secret only the function holds) that can only report its own state and end its own VM. Lifecycle, detection and scheduling in section 6.1. A cron trigger every 30 minutes sweeps VMs that stopped reporting. GitLab's job events cannot do this: they name the pool's runner, not the VM. |
| D29 | 2026-10-24 | No build pool. The M2 drill showed kaniko cannot unpack any base image in Scaleway's Serverless Jobs sandbox (`chown` denied), for RUN and COPY alike. Image builds run on GitLab's hosted runners through the include's `.ciburst-image-build` template; D5's fallback becomes the rule and D20 is withdrawn. |
| D32 | 2026-10-24 | Cost first (owner: "Optimize for costs"). Scaleway bills a CPU Instance from creation per started hour with a 60 minute minimum, Hetzner every started hour, so the dispatcher must not create a VM it does not need and should fill every VM it pays for: burst grouping (D33), `starting` VMs count as capacity, claims spread over VMs, `maxRuns` holds under parallel events, two slots per VM by default with a 4 GiB swap file, and Hetzner as a second VM backend (D34). The drain at minute 55 stays. |
| D33 | 2026-10-24 | Burst grouping without a lock. A pending job that finds no free slot records an intent (an empty object in the cache bucket), waits 2 s and reads the intents of the last 30 s. Ranked by time and job id, every `slots`-th job leads a group and starts one VM; the others ride on their leader's VM; leaders count toward `maxRuns` by rank. A cold pipeline with five VM jobs starts three VMs at two slots instead of five. Without a cache bucket the dispatcher starts one VM per job as in M3. Rules in `docs/milestones/M4-cost.md`. |
| D34 | 2026-10-24 | Hetzner Cloud as a VM backend: pool `runtime: "hetzner"`, settings in `providers.hetzner`, default type `cx33` (4 vCPU, 8 GB, 80 GB, 0.0144 EUR/h with IPv4) with two slots, 0.0072 EUR per job slot-hour against 0.0136 on Scaleway DEV1-M and 0.0155 for RocketRunner's small runner. Same agent, cloud-init and lifecycle as section 6.1; labels instead of tags; a firewall without inbound rules. Dispatcher, cache, registry and Serverless Jobs stay on Scaleway. The owner provides a Hetzner project API token. |
| D35 | 2026-10-24 | Per-pool backends through a composite provider in core: the config's `provider` serves its runtimes, each key of `providers` adds the provider of that runtime. Run ids of an added provider start with `<runtime>:`, so VM reports and termination route without a lookup. This realizes D30's per-pool provider as `pools[].runtime`. |
| D36 | 2026-10-24 | `defaultPool` is optional. Without it a pending job whose stage no pool lists starts nothing (`200 ignored`, reason `unmapped_stage`), so a project whose pipelines mostly run on GitLab's hosted runners pays nothing for them; `ciburst check` then reports only jobs tagged for a pool whose stage maps elsewhere or nowhere. The include's model (every job tagged `vm`, the VM pool as default) keeps its `defaultPool`. |
| D37 | 2026-10-25 | Roundup (owner: "start the project roundup for the final dev experience"): M5 delivers the adopter path of D23 as a CI/CD component, the contributor fixes of the 2026-10-25 review, and docs that describe only the current design. Going public stays M6. |
| D38 | 2026-10-25 | VMs fetch the agent binary from the cache bucket: `apply` uploads the musl `ciburst-vm-agent` as the public object `ciburst/agent/<sha256>`; cloud-init downloads it and checks the hash from the user data. The lifecycle rule expires only the cache and intent prefixes. VM pools need an S3 cache; the registry is optional again (fast-lane images only), so an adopter needs no registry namespace and no public image. Replaces the agent image pull of M3. |
| D39 | 2026-10-25 | Pipeline mode `ciburst --ci` (refines D23): inputs `CIBURST_SCW_ACCESS_KEY`, `CIBURST_SCW_SECRET_KEY`, `CIBURST_SCW_ORGANIZATION_ID`, `CIBURST_GITLAB_TOKEN` (plus `CIBURST_HCLOUD_TOKEN` with Hetzner pools), never `SCW_*`, so an adopter's own Scaleway variables cannot redirect the setup. Inputs and outputs are protected, masked and hidden where GitLab allows, and scoped to the environment `ciburst`, so no build job, VM jobs included, sees them. The config is `ciburst.json` merged over defaults from the job environment (one VM pool). The setup waits for the dispatcher's deploy; `--no-wait` is dropped. |
| D40 | 2026-10-25 | The consumer include becomes the CI/CD component `templates/ciburst.yml` (`include: component: gitlab.com/ciburst/ciburst/ciburst@<version>`) with inputs for the pool tag, the cli image version and the hosted runner tag, and manual jobs `ciburst-setup`, `ciburst-check` and `ciburst-destroy` on the protected default branch. Fast lanes and `.ciburst-image-build` leave it; Docker jobs extend `.ciburst-docker`. |
| D41 | 2026-10-25 | The dispatcher ignores pending jobs named `ciburst-*` (the component's own jobs run on hosted runners), and `apply` ends by replaying every job that waits for a pool tag, so the pipeline that ran the setup continues. The CLI calls GitLab; the dispatcher still never does (D10). |
| D42 | 2026-10-25 | `apply` changes nothing when a pool could not start a run (no Instance quota for its types in the zone, no agent): `Provider::preflight`. `plan` only warns. |
| D43 | 2026-10-25 | One region per deployment: the dispatcher, the cache bucket, the agent object and the VMs live in `provider.region`, and a `provider.zone` outside it is refused. The drill consumer's first pipelines ran VMs in `nl-ams-1` against a cache bucket in `fr-par`: in a warm Rust test job, moving the caches took 555 s and the script 277 s. Scaleway offers `rust196` in every region. |
| D44 | 2026-10-25 | VM slots archive caches with `FF_USE_FASTZIP=true` and `CACHE_COMPRESSION_LEVEL=fastest` (run-single `--env`): the bucket is in the VM's region, so bytes are cheap and compressing was the slow part. |
| D45 | 2026-10-25 | Serverless lanes run the job's own image (owner: "implement the new strategy"). A pool with runtime `jobs` takes any public `linux/amd64` image with `sh`; its run executes `lane-bootstrap.sh`, which installs missing `git`, `curl` and CA certificates (`apk`; `apt-get` as root in its own directories, because the sandbox drops root's capabilities; RPM-based images must ship them), fetches the pinned gitlab-runner from the deployment's bucket (public object `ciburst/runner/<sha256>`, hash-checked) and runs `run-single` with the shell executor. ciburst's pool images go. Measured in `nl-ams`: runs start in 5 to 28 s, a Rust compile at 2 vCPU matches GitLab's small runner and at 4 vCPU takes half its time, a TypeScript check is within 12 %; the bootstrap adds 0 to 21 s by image. |
| D46 | 2026-10-25 | Pools route by job name too: `jobs` glob patterns, matched against the job name before `stages`, then `defaultPool`. `ciburst check` flags a lane job that needs a Docker daemon or names another image than its lane; `check --suggest` prints lanes for the Docker-free jobs. |
| D47 | 2026-10-25 | Paid VM slots first: the VM pool's GitLab runner also carries the tag of every lane with `vmFirst` (default when a VM pool exists). A pending lane job claims a free VM slot when there is one and starts a lane run otherwise; a lane run that loses the race ends after its `waitTimeoutS`. |
| D48 | 2026-10-25 | Warm start (owner: "spec the optimization"). The project hook also delivers pipeline events, which list every job of a pipeline by name, stage, status and `when`, without tags. On a pipeline's first `pending` or `running` event the dispatcher routes all its jobs; for each VM pool that gets a job (manual and `on_failure` jobs, own jobs and `ignoreJobs` left out) and has no free slot and no VM starting, it starts one VM at once, so a Docker job of a later stage finds a booted VM instead of waiting 60 to 100 s. The VM joins the burst ledger with every slot free. Lane jobs of that pipeline leave as many slots free as its VM jobs still to come, up to one VM's `slots`; lanes absorb every other burst, and VM jobs without a free slot start VMs as before. Cost: one VM-hour (0.03 EUR on DEV1-M) when a pipeline ends before its VM jobs run. Pool option `warmStart`, default true. |
| D49 | 2026-10-25 | `ignoreJobs`: job name patterns that run on other runners, such as GitLab's hosted runners. Job events and pipeline events skip them, so their pending events no longer start VMs. `ciburst check` reports a job tagged only for other runners that `ignoreJobs` misses, and a job in `ignoreJobs` tagged for a pool; `check --suggest` prints the list. |
| D50 | 2026-10-25 | Answer budget (owner: "harden"). GitLab.com counts a webhook answer after 10 s as failed and disables the webhook after four failures in a row; a VM start answered in 10.4 s in the M6 drill. Every answer carries `ms`, the dispatcher's elapsed milliseconds, which the webhook's recent events show. The function gets 1024 MB instead of 128 MB, because Scaleway scales a function's CPU with its memory. `apply` resolves the VM security group and the image per instance type and hands them to the function, so a VM start makes no lookup unless one is missing or refused. A start that fails with 429 or 503 (for idempotent calls also another 5xx or a transport failure) is retried once while less than 5 s have passed; a create that timed out is not, because it may have created the server. Target: a VM start answers within 6 s on a warm function and within 9 s right after a deploy. |
| D51 | 2026-10-25 | Webhook health. `plan` reads each project hook's `alert_status` and `disabled_until`; a hook that is not `executable` plans `update` with the reason, and `apply` sends GitLab's test delivery, which the dispatcher answers 200 and GitLab takes as the sign to enable the hook again, before it replays the waiting jobs. `ciburst check`, and with it the `ciburst-check` job, reports a disabled hook. |
| D52 | 2026-10-25 | Deliveries in `ciburst status`: with a GitLab token it prints each project hook's last 20 deliveries (time, job or pipeline, HTTP status, result, pool, `ms`) from GitLab's hook events API, because Cockpit holds no function logs. Without a token it prints what it printed before. |
| D53 | 2026-10-26 | Queue first (owner: "leverage some amount of queueing ... only one (or 2 in case of real needed parallelism) VMs"). A VM job that finds every slot of a running VM busy waits in a queue in the cache bucket instead of starting a VM, and GitLab hands it to the first slot that frees. The dispatcher learns each job name's typical duration from the job events (running, finished) and buys another VM only for a queued job whose wait would pass the VM pool's `maxQueueS` (default 90 s, about one VM boot), or that no running job will free a slot for. Busy VMs report every 30 s, and each report re-evaluates the queue. Measured cause: the drill consumer's pipeline used 4 VMs where 1 would do (pipeline 2988692655). |
| D54 | 2026-10-26 | Spill on spare slots replaces D47's VM slots first. Lane tags move from the VM pool's runner to a second runner, `<name>-<vm tag>-spill`; the VM agent points idle slots at it only while the dispatcher's report answer allows spill: no VM job queued, none of an open pipeline still to come, the VM not draining. A lane job then answers `spilled` and runs on the paid VM; a lane run starts only if no slot took it within 20 s. GitLab hands pending jobs to any polling runner whose tags match, so only the runner a slot polls with can keep lane jobs off the slots Docker jobs need. The M6.1 slot reservation goes. |
| D55 | 2026-10-26 | The spill runner of D54 is off by default (owner: "Hotfix now"). On 2026-10-26 GitLab.com delivered job status events (`pending`, `running`, `success`) to two projects 2,000 to 2,125 s late, in order, for hours, while `created` job events and pipeline events stayed on time and a fresh webhook showed the same (GitLab's hook event log; no status-page incident). Until a `pending` event arrives the dispatcher starts nothing for a job, so only polling runners pick it up: the VM pool's own runner carries the `vmFirst` lanes' tags again, as before M6.2, and its idle slots take lane jobs whenever the events fail. `"spillRunner": true` brings D54 back, for deployments whose events arrive in time. No poll of pending jobs: it would need a GitLab token in the dispatcher (owner: "We do not want to make the setup more complicated"). |
| D56 | 2026-10-26 | Late pending events start nothing (owner: "make the guard right now"). GitLab delivered job status events about 35 minutes late on 2026-10-26, and the late `pending` events started VMs and lane runs for jobs that had long finished. Pipeline events stayed on time, so a pipeline's final event leaves `ciburst/intents/_closed/<pipeline>` with its time and the ids of its jobs that were still manual. A `pending` job event of that pipeline whose job was created before the close and was not manual then answers `ignored`, reason `pipeline_closed`, and logs `stale`; a retried job (created after the close) and a manual job played later start as usual. One read per pending event; the lifecycle rule of `ciburst/intents/` removes the records. |
| D57 | 2026-10-26 | One fleet, many projects (owner: "Plan B looks good"). The first project sets ciburst up with all its variables and becomes the host; any later project, in any group or organization, joins through an invite code from the host and a manual `ciburst-join` with two variables (the code and its own GitLab token for its own project). The member registers its own runners and webhook, because GitLab.com shares runners across groups only with Maintainer rights in both and offers group webhooks only on paid tiers, and hands the runner tokens to the dispatcher's `/join`. One dispatcher, bucket, VM pool and lanes serve all members. |
| D58 | 2026-10-26 | Fleet state in the bucket: the routing config and the member records (webhook secret, encrypted runner tokens) live in `ciburst/fleet/`, and the dispatcher reads them with a cache of at most 30 s. A config change or a new member takes effect without a function deploy, which took about 10 minutes for every config change. |
| D59 | 2026-10-26 | VMs run one `gitlab-runner run` with a runner entry per member and `concurrent = slots`, replacing the per-slot `run-single` processes, so one VM serves every member's jobs within its slots; VM jobs of all members share VMs by default (owner: "we can assume vm sharing per default for now"). Gated by a phase 0 measurement on a real VM. |

## 1. Purpose and scope

ciburst gives GitLab projects CI runners that exist only while jobs run. A GitLab job webhook reaches a dispatcher, a Scaleway Function, which selects the job's pool by the job's name, else its stage. For a VM pool the dispatcher gives the job a free slot on a running VM or starts a VM (Scaleway Instances by default, Hetzner Cloud optional) that runs jobs with GitLab's Docker executor and ends itself once idle at minute 55 of its paid hour. For a lane, a pool for jobs without a Docker daemon, it gives the job a free VM slot first and otherwise starts one Scaleway Serverless Job from the job's own image. Nothing runs between pipelines.

v1:

1. `crates/core`: config, dispatcher, GitLab client, plan and apply engine, provider and host traits, composite provider, burst ledger, VM user data, lane bootstrap, contract harnesses, fakes.
2. `crates/scaleway` (provider `scaleway` with the runtimes `instances` and `jobs`, host `scaleway-function`), `crates/hetzner` (provider `hetzner`), `crates/dispatch-scaleway` (the function), `crates/vm-agent` (the VM supervisor).
3. Binary `ciburst`: `plan`, `apply`, `destroy`, `status`, `check` (with `--suggest`), `rotate`, pipeline mode `--ci`.
4. Image `cli` for the setup jobs.
5. CI/CD component `templates/ciburst.yml`; README, runbook, provider guide.

Backlog, not v1: providers `fly-machines`, `aws-lambda-microvms` (amd64 support pending), spot VMs and providers on other clouds, remote build services; a scheduled poll of pending jobs; a self-hosted dispatcher binary; group-level webhooks; a hosted dispatcher as a service; GitHub Actions support.

## 2. Guiding principles

- Zero fixed cost: nothing runs or is reserved between pipelines; the function and its cron stay inside Scaleway's free tier.
- Fill every paid VM hour: group bursts, reuse warm VMs, give free slots to lane jobs first, start no VM that is not needed (D32, D47).
- Zero ops: no runner manager, no host to patch; every VM ends itself, the sweep ends any VM that stops reporting, and no VM lives longer than 24 hours.
- Contract first: the provider and host traits and their contract harnesses exist before any provider.
- Config as code: every setup with an API is a step in the idempotent plan; clicks only where no API exists.
- Few dependencies, every one gated by `cargo deny`; one static binary for users.
- No identifying data in the repository; brief docs; no em or en dashes.

## 3. Name

`ciburst`, see D8.

## 4. Personas and journeys

| Persona | Journey | Steps |
|---|---|---|
| Project owner | Give a GitLab project runners | README, Adopt: a Scaleway key, a personal access token, four protected CI variables, the component include, the manual `ciburst-setup` job |
| Project owner | Change pools, add a lane or Hetzner | `ciburst check --suggest` for lanes; edit `ciburst.json` and the lane jobs' `tags:`, run `ciburst-setup`, run `ciburst-check` |
| Developer | Run ciburst from a workstation | runbook, Developer setup: `.local/` files, `ciburst plan`, `apply`, `check` |
| Provider author | Add a cloud | implement `Provider` in a crate, pass the contract harnesses against an API fake, register it, document it (`docs/PROVIDERS.md`) |
| Operator | See what runs and what it costs | `ciburst status`, the webhook's recent events, the VM tags, the provider's consumption page |

## 5. Architecture

### 5.1 Stack

| Part | Choice | Reason |
|---|---|---|
| Language | Rust 1.96, edition 2024, Cargo workspace | one static CLI binary, typed traits, the toolchain Scaleway compiles the function with |
| Dispatcher host | Scaleway Serverless Function, runtime `rust196`, 1024 MB (Scaleway gives it 560 mvCPU; CPU scales with memory), timeout 30 s, a cron trigger every 30 minutes | free tier of 1,000,000 requests per month, cold start well under a second |
| Compute, default | Scaleway Instances DEV1-M (3 vCPU, 4 GB, 40 GB local SSD), two slots, 4 GiB swap | a Docker daemon per VM; billed per started hour with a 60 minute minimum, about 0.03 EUR per hour |
| Compute, optional | Hetzner Cloud CX33 (4 vCPU, 8 GB, 80 GB); Scaleway Serverless Jobs lanes (up to 6 vCPU, 16 GB) | CX33 at about half the price; lanes start in seconds from the job's own image and bill per second, without a Docker daemon |
| Cache | Scaleway Object Storage bucket, S3 API | shared by every run; also holds the burst intents, the agent object and the runner object |
| Runner | `gitlab-runner` 18.11.4 `run-single` from the runner object: Docker executor on VMs, shell executor in lanes | no runner manager, no registration state |
| Tests | `cargo test`, contract harnesses, in-memory API fakes behind the `HttpClient` trait | no sockets, no cloud |

### 5.2 Runtime topology

1. GitLab sends a job event (`X-Gitlab-Event: Job Hook`) to the dispatcher URL with the hook token.
2. The dispatcher verifies the token and answers `200 ignored` for anything but a pending job of a configured project, for jobs named `ciburst-*` (D41), and for a job no pool takes when there is no default pool (D36). Otherwise the job's name, else its stage selects the pool (section 5.4); the event carries no tags.
3. VM pool: the dispatcher claims a free slot on a VM of the pool; without one the job joins the burst ledger, and every `slots`-th job of a burst starts a VM, up to `maxRuns` VMs (section 6.1, scheduling).
4. The VM boots; cloud-init installs `gitlab-runner` from the runner object and the agent from the agent object; the agent runs `slots` `run-single` processes with the Docker executor, which poll GitLab for jobs with the tags of the pool's runner. Any polling slot takes any pending job with those tags.
5. An idle VM drains at minute 55 of its paid hour and asks the dispatcher to terminate it; the cron sweep ends VMs that stop reporting.
6. Lane: with `vmFirst`, while spill is allowed and a VM of the lane's VM pool has an idle slot on the spill runner, the job answers `spilled` and that slot takes it; a lane run starts if none did within 20 s (D54). Otherwise it counts the lane's live runs; below `maxRuns` it starts one Serverless Job from the lane's image, whose bootstrap runs `run-single` with the shell executor until `waitTimeoutS` idle seconds pass (section 6.3). Lanes never use the burst ledger.
7. A job whose tags do not match the pool its name or stage selects stays pending; `ciburst check` reports such jobs.

### 5.3 Ports

The `Provider` and `Host` traits live in `crates/core/src/types.rs`; their contracts, the registration of a provider and its tests are in `docs/PROVIDERS.md`.

Core modules: `config` (schema, defaults, routing, validation reporting every error), `dispatch` (routes and scheduling, pure apart from provider calls), `gitlab` (runners, hooks, CI lint, variables, pending jobs), `http` (the one HTTP seam, `ReqwestClient` in production), `plan` (formatting, outputs stores), `provision` (phases: provider, runners, host, hooks, replay), `check` (lint against the pools, lane suggestions), `composite` (per-pool providers, D35), `ledger` (burst intents, D33), `cloud_init` (VM user data, the runner pin), `lane` (the bootstrap script and the job command, D45), `vm` (VM tokens, timings, the sweep rule), `time`, `store`, `log` (one line per action, never a secret), `fake`, `testing` (contract harnesses, GitLab fake).

### 5.4 Configuration

Pipeline mode reads `ciburst.json` and merges it over the defaults by top-level key: a key in the file replaces the default's whole value. The defaults for a project with id 12345678:

```json
{
  "name": "ci-12345678",
  "gitlab": { "url": "https://gitlab.com", "projectIds": [12345678] },
  "host": { "type": "scaleway-function", "region": "fr-par" },
  "provider": { "type": "scaleway", "region": "fr-par" },
  "defaultPool": "vm",
  "pools": [
    { "tag": "vm", "runtime": "instances", "image": "ubuntu:24.04", "stages": [], "slots": 2, "instanceTypes": ["DEV1-M", "DEV1-L"], "maxRuns": 4 }
  ],
  "cache": { "type": "s3", "expireDays": 30 }
}
```

| Key | Meaning |
|---|---|
| `name` | prefix of every cloud resource; 2 to 20 lowercase letters, digits or hyphens, starting with a letter, so the bucket name `<name>-cache-<project id>` stays within the S3 limit of 63 |
| `gitlab` | `url` (https) and `projectIds`; runners belong to the first project and are enabled in the others, every project gets a webhook |
| `host` | `type` `scaleway-function`, `region` |
| `provider` | `type` `scaleway`, `region` (every resource); optional `zone` (Instances, default `<region>-1`, must lie in `region`), `registry` and `registryPublic` (a registry namespace for lane images) |
| `providers` | added providers by runtime name, for example `{"hetzner": {"location": "nbg1", "image": "docker-ce"}}` |
| `defaultPool` | pool of the jobs that no `jobs` pattern and no `stages` entry takes; without it such jobs start nothing |
| `ignoreJobs` | job name patterns (as `jobs`) that run on other runners; routing checks them first, so they start nothing (D49) |
| `spillRunner` | `false` by default (D55): the VM pool's runner carries the `vmFirst` lanes' tags; `true` gives each such VM pool the spill runner of D54 |
| `pools[]`, VM | `tag`, `runtime` (`instances` by default, or a key of `providers`), `image` (the job image when a job names none), `jobs`, `stages`, `slots` (default 2), `instanceTypes` (default `DEV1-M`, `DEV1-L`; Hetzner `cx33`, `cx43`), `maxRuns` (VMs, default 10), `warmStart` (default true, D48), `maxQueueS` (0 to 3600, default 90, D53) |
| `pools[]`, lane | `runtime` `jobs`, `tag`, `image` (the job image: `linux/amd64` with `/bin/sh`, pullable without credentials), `jobs`, `stages`, `vmFirst` (default true when the config has a VM pool: its jobs may spill onto idle paid VM slots, D54), `vcpu` (2), `memoryMiB` (4096), `storageMiB` (10240), `timeoutS` (3600), `waitTimeoutS` (60), `maxRuns` (runs, 10) |
| `cache` | `type` `s3` (required: the bucket holds the runner object, the agent object and the intents), `expireDays` (30) |

Routing (D46, D49, `Config::pool_for`): a job whose name matches `ignoreJobs` goes nowhere; else a pending job goes to the first pool, in config order, with a `jobs` pattern that matches its name (`*` any run of characters, `?` one character, nothing else is special), else to the pool whose `stages` lists its stage, else to `defaultPool`, else nowhere.

Spill (D54, D55): a lane with `vmFirst` may use the idle slots of one VM pool, `defaultPool` when it is a VM pool, else the first VM pool. By default (D55) that pool's own GitLab runner carries its tag and the tag of every such lane, in config order, so its polling slots take those lanes' jobs. With `"spillRunner": true` the pool's own runner carries only its tag, and its spill runner `<name>-<vm tag>-spill` carries its tag and the lanes' tags. A pool tag `<vm tag>-spill` is refused.

Validation rejects, naming each field: a tag or stage used twice, an empty `jobs` pattern, the same pattern in two pools (naming both), `vmFirst` on a pool that is not a lane, a `defaultPool` that is not a pool, a pool above its provider's capabilities, a config without an S3 cache, a `providers` key from the reserved names (`docs/PROVIDERS.md`).

Credentials: on the developer path `.local/scaleway.env` (`SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID`, optional `SCW_DEFAULT_PROJECT_ID`), `.local/gitlab.env` (`GITLAB_TOKEN`, scopes `api` and `create_runner`; a `GITLAB_TOKEN` in the environment wins) and, with Hetzner pools, `.local/hetzner.env` (`HCLOUD_TOKEN`); in pipeline mode the variables of section 8.

### 5.5 Repository layout

```
Cargo.toml  rust-toolchain.toml  deny.toml  LICENSE  README.md  AGENTS.md  config.example.json
crates/core/               ciburst-core: config, dispatch, gitlab, provision, composite, ledger, cloud_init, lane (lane-bootstrap.sh), vm, fake, testing
crates/cli/                binary ciburst; registry of providers and hosts
crates/scaleway/           provider scaleway (instances, jobs, IAM, S3, registry), host scaleway-function, ledger, build.rs packing the function zip, API fakes
crates/hetzner/            provider hetzner with an API fake
crates/dispatch-scaleway/  the function: handle, serve, respond
crates/vm-agent/           binary ciburst-vm-agent: slots, reports, drain
images/cli/                the setup image with ciburst and ciburst-vm-agent
images/test/               the lane bootstrap test, run-single checks, the cli smoke test
templates/ciburst.yml      the CI/CD component
scripts/                   CI helpers, agent-binary.sh, test-linux.sh, shellcheck.sh
test/config.fake.json      dry-run config
test/drill/gitlab-ci.yml   drill pipeline
docs/                      SPEC.md RUNBOOK.md PROVIDERS.md milestones/
.gitlab-ci.yml  .local/ (gitignored)
```

## 6. Scaleway implementation

Provider `scaleway`, region from the config, Instances in `provider.zone` (default `<region>-1`):

| Step id | Resource | Plan and apply |
|---|---|---|
| `project` | project `<name>` | by name; an id in `SCW_DEFAULT_PROJECT_ID` (`CIBURST_SCW_PROJECT_ID`) is used instead; output `CIBURST_PROJECT_ID` |
| `key:dispatch` | IAM application `<name>-dispatch`, a policy on the project with `ServerlessJobsFullAccess`, `ContainerRegistryReadOnly` and `InstancesFullAccess`, one API key | a missing key or set plans `update` (repair); the secret is an output once |
| `key:cache` | IAM application `<name>-cache`, a policy with `ObjectStorageFullAccess`, one API key | with an S3 cache |
| `bucket` | `<name>-cache-<project id>`, lifecycle rules expiring the runner cache (`project/`, `runner/`) and `ciburst/intents/` after `cache.expireDays`; `ciburst/agent/` and `ciburst/runner/` never expire | S3 with SigV4 and the owner key; drift by the rules; destroy empties it first |
| `runner-binary` | public object `ciburst/runner/<sha256>` in the bucket: `gitlab-runner` 18.11.4, `RUNNER_SHA256` of `ciburst_core::cloud_init` | with any pool: `apply` downloads `RUNNER_URL`, refuses another sha256 and uploads it public-read; `noop` when the object exists |
| `vm:agent` | public object `ciburst/agent/<sha256>` in the bucket: the musl `ciburst-vm-agent` | with a VM pool of any provider: `create` when the given binary's object is missing; without a binary the newest object is current, none is an error; output `CIBURST_AGENT_SHA256` |
| `registry` | Container Registry namespace `<name>` | only with `provider.registry: true` (private lane images), public with `registryPublic`; output `CIBURST_REGISTRY_ENDPOINT` |
| `vm:security-group` | security group `<name>-vm`: inbound drop, outbound accept, stateful | with a VM pool |
| `definition:<tag>` | Serverless Jobs definition `<name>-<tag>` per lane: `cpu_limit`, `memory_limit`, `local_storage_capacity`, `image_uri` (the lane's `image`), `command` (`ciburst_core::lane::COMMAND`), `job_timeout`, environment (pool, GitLab URL, idle window, cache location, `CIBURST_LANE`, `CIBURST_RUNNER_URL`, `CIBURST_RUNNER_SHA256`) | `update` when a field differs |

Runtime `instances` (VM pools):

| Call | Effect |
|---|---|
| start | `POST servers`: name `<name>-<tag>-<8 hex>`; the first type of `instanceTypes` with capacity, only types with a local root volume (terminate deletes local volumes but only detaches block volumes, which keep billing); Scaleway's marketplace image labelled `docker` in its `instance_local` variant, fallback `ubuntu_noble` with Docker installed by cloud-init; `dynamic_ip_required`, `routed_ip_enabled`, the security group; tags `ciburst`, `<name>`, `pool:<tag>`, `job:<id>`, `ciburst-state:starting`, `ciburst-free:<slots - taken>`. Then user data `cloud-init` and `poweron`. The run id is the server id. |
| cloud-init | SSH password login off, a 4 GiB swap file, `/etc/ciburst/vm.env` (mode 0600), `gitlab-runner` from the runner object `https://<bucket>.s3.<region>.scw.cloud/ciburst/runner/<sha256>` and the agent from `https://<bucket>.s3.<region>.scw.cloud/ciburst/agent/<sha256>`, each with its sha256 checked, the unit `ciburst-vm-agent.service` after `docker.service` |
| list, capacity | servers tagged `<name>`; `starting` and `running` are live; the tags give state, free slots and last report |
| claim, report | rewrite the state tags |
| stop | `terminate`: deletes the local volume, releases the dynamic IP |

Runtime `jobs` (lanes, section 6.3): start `POST job-definitions/<id>/start` with the overrides `RUNNER_TOKEN`, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY` and the labels as `CIBURST_PROJECT`, `CIBURST_PIPELINE`, `CIBURST_JOB`, `CIBURST_STAGE`; list `job-runs` of the definition (`queued`, `scheduled`, `running` are live); stop `POST job-runs/<id>/stop`.

Preflight (D42): `apply` stops before any change when a VM pool's zone has no quota for any of its types (the message names zones with quota) or no agent object is available; `plan` only warns.

Host `scaleway-function` (API `functions/v1beta1`):

| Step id | Resource |
|---|---|
| `namespace` | namespace `<name>` |
| `function` | `<name>-dispatch`: runtime `rust196`, handler `handle`, privacy `public`, `min_scale 0`, `max_scale 5`, `memory_limit 1024`, `timeout 30s`, `http_option redirected`, `sandbox v2`. Secret environment: `HOOK_TOKEN`, `CIBURST_DISPATCH_KEY` (the dispatch secret key; Scaleway reserves the `SCW_` prefix), `RUNNER_TOKEN_<TAG>` per pool, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`, `CIBURST_VM_SECRET`, `CIBURST_SWEEP_TOKEN`, the credentials of added providers (`HCLOUD_TOKEN`). Plain environment: `CIBURST_CONFIG` (config JSON), `CIBURST_ORGANIZATION_ID`, `CIBURST_PROJECT_ID`, `CIBURST_REGION`, `CIBURST_AGENT_SHA256`, `CIBURST_CODE_DIGEST` (sha256 of the deployed zip), and with VM pools `CIBURST_VM_SECURITY_GROUP` (the id of `<name>-vm`) and `CIBURST_VM_IMAGES` (per instance type the image and whether cloud-init installs Docker; types without a local image are left out), which `plan` and `apply` resolve each time (D50). The `description` `ciburst:<sha256 of settings and both environments>` decides `update`. |
| `code` | a deterministic zip packed by `crates/scaleway/build.rs`: the `ciburst-dispatch-scaleway` package (`handle` on axum 0.8) with every path dependency under `vendor/`, inherited manifest values written out, dev and build dependencies dropped, plus the workspace `Cargo.lock`; uploaded, deployed and polled until `ready` (up to 20 minutes); output `CIBURST_DISPATCH_URL` |
| `cron` | `<name>-sweep`, schedule `*/30 * * * *`, arguments `{"ciburst_sweep": "<sweep token>"}`, which the function receives as `POST /` with that JSON body |

GitLab steps (`crates/core/src/gitlab.rs`), per project id: one runner per pool, `POST /user/runners` (`project_type`, `tag_list` the pool's tag, for the VM pool that lanes borrow slots from (section 5.4) also the tags of those lanes; `run_untagged false`, description `<name>-<pool.tag>`), enabled in every further project; `plan` shows `update` when a runner's tags differ and `apply` sets them with `PUT /runners/:id`; a webhook `POST /projects/:id/hooks` (`url` the function URL, `token`, `job_events true`, `push_events false`, SSL verification on); `check`: the CI lint `GET /projects/:id/ci/lint?include_jobs=true`, then every job's route and tags against the pools, and every lane job against the lane rules (section 10). Lookups by description and URL. Pipeline mode adds project variables and pending jobs (section 8).

### 6.1 VM lifecycle

Facts from `gitlab-runner` v18.11.4 (`commands/single.go`, `common/consts.go`): a runner receives a job only by requesting one, about every 3 s while idle; `run-single` runs one job at a time; `SIGQUIT` stops further requests, lets the current job finish, then exits; a runner whose requests fail waits 300 s between requests, and `SIGQUIT` does not shorten that wait, so a draining VM cut off from GitLab ends minutes late while it keeps reporting.

Detection, on the VM. The agent (`crates/vm-agent`) runs `slots` `run-single` processes with the Docker executor, `--docker-privileged` and the volumes `/cache` and `/certs/client` (Docker-in-Docker with TLS needs the latter). All slots share the VM's system id, because `run-single` reads `--config` only with `--runner`; every slot therefore reports concurrent id 0, and the executor would give two jobs of one project the same volumes, `/builds` included, so each slot gets its own `--docker-cache-dir`, which turns them into host directories of that slot. Every slot labels its containers `ciburst.slot:<n>` (`--docker-container-labels`); busy slots are the distinct `com.gitlab.gitlab-runner.job.id` labels over `docker ps`, read every 5 s, each with its slot. Each slot polls with the pool runner's token, or, while the dispatcher's last answer allowed spill and the VM has `RUNNER_SPILL_TOKEN`, an idle slot polls with the spill runner's token (D54): the agent sends an idle slot `SIGQUIT` and starts it again with the other token; a busy slot changes after its job. The agent posts `{state, free, slots, spill}` (`spill`: slots on the spill runner) to the dispatcher's `/vm` (headers `X-Ciburst-Run` and `X-Ciburst-Token`) on every change and after the answer's `nextReportS` (10 to 600 s; 600 before the first answer); the provider writes the report into the server's tags `ciburst-state:<state>`, `ciburst-free:<n>`, `ciburst-spill:<n>` (when not 0), `ciburst-seen:<unix time>` and, once draining, `ciburst-drain:<unix time>`. The tags are the only shared state; the function stays stateless.

States and transitions:

| State | Meaning | Next |
|---|---|---|
| `starting` | created, no report accepted yet | `idle` or `busy` on the first report |
| `idle` | every slot polling, no job | `busy` when a slot takes a job; `draining` at minute 55 of a paid hour |
| `busy` | at least one slot runs a job | `idle` when all slots are free |
| `draining` | idle at minute 55, `free` 0; slots keep polling for a 30 s grace, then get `SIGQUIT` | `busy` if a slot takes a job during the grace; `POST /terminate` after the last slot exited |

Drain protocol, which keeps every job served:

1. The VM reports `draining` first. From then on the dispatcher no longer counts it as capacity.
2. Its slots keep polling for the 30 s grace, which starts once the dispatcher accepted the report. A job that went pending before step 1 was routed to this VM, and a polling slot takes it within about 3 s.
3. After the grace the agent sends `SIGQUIT`. A job taken in the last request still runs to its end.
4. When all slots have exited, no request can reach GitLab, so no job can be assigned to the VM. Only then does it post `/terminate`; the dispatcher checks the token and terminates the server.

Scheduling, on a pending job event (M4 rules R1 to R7): a free slot is claimed on a VM created more than 30 s ago that is `idle` or `busy` and reported within 21 minutes, or is `starting` and younger than 10 minutes; among the VMs with the most free slots the job id picks one, so parallel claims spread. Otherwise the job joins the burst ledger (D33): every `slots`-th job of the last 30 s starts a VM with that many slots already taken, the others ride on it, and leaders count toward `maxRuns` by rank. A failed claim or ledger falls back to starting a VM.

Queue first (D53), when no slot is free but a VM of the pool is alive: the job waits in `ciburst/intents/_queue/<pool>/<job>` and answers `queued` with `etaS`. The start rule ranks the queued jobs oldest first; the free slots of starting VMs take the first, then each further job is matched to the running VM job that ends next, by `typical(name) - elapsed` (typical: the 7:3 fold of the job name's finished durations in `_stats/`; unknown counts as ending now). A job whose wait so far plus that remainder passes `maxQueueS`, or that no running job will free a slot for, needs a VM; `ceil(needing / slots)` VMs start, bounded by `maxRuns`, after the start marker `_queue/<pool>/.started` (15 s) is taken. Job events `running` and `success`, `failed`, `canceled`, `skipped` move jobs to `_running/` and out (`tracked`). The rule runs on each such pending event, on each VM report and on a lane job while its VM pool has queued jobs; entries older than 30 minutes (queued) or 24 hours (running) count as lost.

Spill (D54, only with `"spillRunner": true`, D55): each VM pool with a `vmFirst` lane has a second runner `<name>-<vm tag>-spill` carrying its own and those lanes' tags; its own runner carries only its own tag. The report answer `{"spill":<bool>,"nextReportS":<s>}` allows spill when the pool has no queued VM job, no open pipeline record (younger than 2 hours) lists an unseen VM job of the pool, and the VM is not draining; `nextReportS` is 15 while spill is allowed or jobs are queued, 30 while the VM is busy, else 600.

Warm start (D48), on a pipeline event with status `pending` or `running`: the jobs of the event's `builds` that are not `manual`, not `on_failure`, not finished, not own `ciburst-*` jobs and not in `ignoreJobs` are routed; those of VM pools are the pipeline's VM jobs. Without any the answer is `ignored`, reason `no_vm_jobs`. Otherwise the dispatcher writes the record `ciburst/intents/_pipelines/<pipeline>/vm-jobs.json` (an existing record answers `duplicate`) and per VM pool: `disabled` with `warmStart` false; `capacity` when a claim candidate exists, or after the burst wait an intent ranks before the pool's warm intent `warm:<pipeline>`; `at_capacity` at `maxRuns`; else it starts one VM with every slot free, whose ledger position lets the next `slots` jobs of the window ride on it (`started`). A VM job's pending event marks `seen/<job>` beside the record. A lane job with `vmFirst` claims a VM slot only when the claim candidates have more free slots than `min(slots, the pipeline's VM jobs of that pool without a seen marker)`. A final status (`success`, `failed`, `canceled`, `skipped`) deletes the records (`closed`). A records error logs `warn records_failed` and the dispatcher acts as without records.

Start path (D50): a VM start makes three Instance calls (create, user data, power on) on the values of `CIBURST_VM_SECURITY_GROUP` and `CIBURST_VM_IMAGES`; a type missing from them, or a create refused because the image or the security group is gone, looks them up for that start and logs `warn start_lookup` with the reason. User data and power on are retried inside the start; the dispatcher retries a whole start once on a retryable error (429 or 503, for idempotent calls also another 5xx or a transport failure) while less than 5 s have passed, logging `warn start_retry`. Every answer and log line carries `ms`.

Races and their effect:

| Race | Effect |
|---|---|
| Two dispatcher calls claim the same free slot | one job waits for the next free slot; never stranded, because the VM keeps polling until it is idle, and idle means no pending job for its tag |
| A job goes pending while a VM starts draining | taken in the grace, or the dispatcher already saw `draining` and started a VM; at worst one extra VM-hour |
| A VM boots for a job another VM already took | the new VM idles until minute 55 of its hour and drains; one VM-hour |
| A VM hangs or loses its network | its jobs fail on GitLab's side when the runner stops updating them; the sweep removes it |
| Pending events of one pool arrive in parallel | the ledger ranks them, so a burst starts `ceil(N / slots)` VMs up to `maxRuns`; an intent written after an earlier caller's 2 s wait can cost one extra VM-hour |

Sweep, the cron `<name>-sweep` every 30 minutes with its own sweep token (the hook token would sit readable in the cron's arguments): it ends every VM older than 24 hours, a VM that has not reported 10 minutes after its creation, and a VM whose last report is more than 30 minutes old (`ciburst_core::vm::sweep_reason`). A VM that reported within 30 minutes and is younger than 24 hours is never touched, so the sweep cannot race the scheduler or a running job: the agent reports every 10 minutes whatever its jobs do. The 24 hours bound every VM and every job on it.

### 6.2 Hetzner implementation (D34)

Provider `hetzner`, runtime `hetzner`, API `https://api.hetzner.cloud/v1` with `Authorization: Bearer <HCLOUD_TOKEN>`, settings `providers.hetzner` (`location` default `nbg1`, `image` default `docker-ce`, Ubuntu 24.04 with Docker):

| Step | Resource |
|---|---|
| `hetzner:firewall` | firewall `<name>-vm` without rules, applied to the label selector `ciburst-name==<name>`: no inbound traffic |
| start | server `<name>-<tag>-<8 hex>`, type from `instanceTypes` (default `cx33`, then `cx43`), the location and image above, IPv4 and IPv6, labels `ciburst`, `ciburst-name`, `ciburst-pool`, `ciburst-job`, `ciburst-state`, `ciburst-free`; user data from the shared cloud-init with the run id `hetzner:<server name>` and its token, known before create |
| capacity, claim, report, sweep | as section 6.1 with labels `ciburst-state`, `ciburst-free`, `ciburst-seen`, `ciburst-drain` |
| stop | `DELETE /servers/{id}` (IPv4 included in the price, nothing left behind) |

Cost: CX33 0.0144 EUR per hour with IPv4 and disk, every started hour billed; the cache, the runner object and the agent object come from Scaleway over the internet.

### 6.3 Lane runs (D45, D47)

A lane run is one Serverless Job from the lane's image. The definition's `command` is `/bin/sh -c` with one argument without spaces, because Scaleway splits a job command at spaces and ignores quotes; the argument decodes `CIBURST_LANE` with `base64 -d` and evaluates it. That is `crates/core/src/lane-bootstrap.sh`, which:

1. requires `CI_SERVER_URL`, `RUNNER_TOKEN`, `CIBURST_RUNNER_URL`, `CIBURST_RUNNER_SHA256`, and with `CACHE_TYPE=s3` the cache location and key;
2. installs what the image lacks of `git`, `curl` (`wget` also serves) and CA certificates: `apk` on Alpine; `apt-get` on Debian and Ubuntu, as root with `APT::Sandbox::User=root` and its lists and archives under `/tmp/ciburst-apt`, because the sandbox drops root's capabilities; RPM-based images (RPM cannot install in the sandbox) and non-root users get no install and fail when a tool is missing;
3. downloads the runner object to `/usr/local/bin`, or to `$HOME/.ciburst/bin` when that is not writable (`/tmp/.ciburst/bin` without a writable home), and refuses it unless its SHA-256 is `CIBURST_RUNNER_SHA256`;
4. unsets `RUNNER_TOKEN`, `CACHE_S3_ACCESS_KEY` and `CACHE_S3_SECRET_KEY` and execs `run-single` with the shell executor (`bash`, else `sh`), `--builds-dir /builds` and `--cache-dir /cache` for root (`$HOME/builds` and `$HOME/cache` for another user, `/tmp/ciburst-builds` and `/tmp/ciburst-cache` without a writable home), `--max-builds 0`, `--wait-timeout <waitTimeoutS>`, `--env PATH=<the image's PATH>` (the shell executor runs jobs in a login shell, whose `/etc/profile` replaces `PATH`, which hides `cargo` in Rust images) and the S3 cache with `FF_USE_FASTZIP=true` and `CACHE_COMPRESSION_LEVEL=fastest`.

Every failure the script detects prints `ciburst-lane: <reason>` to stderr and exits 64. Measured in `nl-ams` (`docs/RUNBOOK.md`, Measured): runs start in 5 to 28 s, 93 s at 6 vCPU; the bootstrap adds nothing for images with `git` and `curl`, 2 to 4 s on Alpine, about 21 s on Debian and Ubuntu slim.

In a lane run the shell executor runs the job in the lane's image and ignores `image:`; on a VM slot the Docker executor runs it in `image:`. `ciburst check` therefore requires a lane job's `image:` to equal the lane's `image`, and reports lane jobs that need a Docker daemon.

With `vmFirst`, a lane job that may spill answers `200 {"result":"spilled","pool":"<lane tag>","vm":"<run id>"}`, waits in `_queue/<lane tag>/` and is tracked like a VM job; the next evaluation older than 20 s starts a lane run for it (`spill_fallback`). Races:

| Race | Effect |
|---|---|
| A VM slot takes the job a lane run was started for | the run idles `waitTimeoutS` and exits; those seconds are billed |
| Another job takes the spill slot first | the lane job waits at most 20 s plus one evaluation, then gets a lane run |
| A lane job's pending event arrives while the agent still switches its slots | an idle slot may take up to `slots` lane jobs before the agent learns that spill stopped, at most `nextReportS` plus one GitLab job request later |

## 7. Security

- Access control: the dispatcher URL is public (GitLab cannot sign requests with IAM). A webhook must carry the hook token, a VM request the VM's capability token, the cron body the sweep token, each compared in constant time; anything else gets `401` without a body.
- The dispatch key holds `ServerlessJobsFullAccess`, `InstancesFullAccess` and `ContainerRegistryReadOnly` on the dedicated project only. The dispatcher holds no GitLab token. With Hetzner pools it holds a Hetzner project token, which Hetzner cannot narrow further: use a Hetzner project for ciburst alone.
- A lane job can read its lane's runner token and the cache key, as on any runner: the bootstrap passes them to `run-single` as flags and unsets them before `exec`, so job scripts do not inherit them, but a job can read the runner's command line. Lane jobs run as the image's user, root included; the Serverless Jobs sandbox drops root's capabilities and offers no privileged mode. The dispatch key never enters a run; the cache key can only read and write the cache bucket.
- A VM runs jobs privileged (Docker-in-Docker), so every job can read the VM's user data: its pool's runner token, the cache key and the VM's own token. This equals GitLab's own privileged runners; the VM token can only report for and end its own VM. Scaleway and Hetzner keep the user data with the server until it is deleted; it never holds the VM secret, the dispatch key or the Hetzner token.
- The agent and runner objects are public and hold no secret. Any job can read the cache key and overwrite them, so cloud-init and the lane bootstrap check each download against its pinned sha256 and stop on a mismatch.
- Burst intents live in the cache bucket, which every job can write with the cache key. A forged intent can at most cost one VM-hour or delay one job until a slot frees.
- Pipeline mode (D39): inputs and outputs are protected project variables scoped to the environment `ciburst`, masked and hidden where GitLab allows, so only the component's jobs on the protected default branch receive them; `--ci` never reads `SCW_*`, so a project's own Scaleway variables cannot redirect the setup.
- Job secrets: a job's variables, file variables such as deploy keys included, reach the VM that runs it, as on any runner; the runner deletes them after the job, the VM belongs to one deployment and ends within its hour.

Secrets inventory ("outputs" is `.local/outputs.env` on the developer path, project variables in pipeline mode):

| Secret | Where | Lifetime | Rotation |
|---|---|---|---|
| Owner Scaleway key (`IAMManager`, `ProjectManager`, `InstancesFullAccess`, `ServerlessJobsFullAccess`, `FunctionsFullAccess`, `ObjectStorageFullAccess`; `ContainerRegistryFullAccess` only with `provider.registry`) | `.local/scaleway.env`, or the variables `CIBURST_SCW_*` | owner chosen | console, then the file or the variables |
| Owner GitLab token, scopes `api` and `create_runner`, of a project Owner | `.local/gitlab.env`, or `CIBURST_GITLAB_TOKEN` | expiry chosen | new token, then the file or the variable |
| Dispatch IAM key | function secret env, outputs | none | `ciburst rotate key` |
| VM secret (signs the VM tokens) | function secret env, outputs | none | `ciburst rotate vm`, refused while VMs run |
| Sweep token | function secret env, the cron's arguments, outputs | none | `ciburst rotate hook` |
| VM capability token | the VM's user data | the VM's life | ends with the VM |
| Cache IAM key | function secret env, VM user data, lane run env, outputs | none | `ciburst rotate cache` |
| Runner tokens `glrt-`, one per pool | function secret env, VM user data, lane run env, outputs | until the runner is deleted | `ciburst rotate runner` |
| Hook token | GitLab webhook, function secret env, outputs | none | `ciburst rotate hook` |
| Registry push key (private phase) | CI variables of the framework project | none | console, then the variable |
| Hetzner project token, read and write | `.local/hetzner.env` and `.local/outputs.env`, or `CIBURST_HCLOUD_TOKEN`; function secret env | owner chosen | console, then `ciburst apply` |

Privacy: the dispatcher logs project id, job id, pipeline id, stage, pool and run id. No payload is stored.

## 8. Automation

Framework CI (`.gitlab-ci.yml`, GitLab hosted runners, `rust:1.96.0-bookworm`):

- `lint` (`cargo fmt --check`, `cargo clippy -D warnings`, shellcheck over every script, the pin check of Rust 1.96.0 and `gitlab-runner` 18.11.4), `deny` (`cargo deny check`), `test` (`cargo test --workspace --all-features`); the Rust jobs run only when Rust, CI or script paths change.
- `lane-bootstrap`, when `lane-bootstrap.sh` or its test change: `images/test/lane-bootstrap.sh` runs the bootstrap in Docker with `--cap-drop=ALL`. Dry runs pass on `alpine:3.21`, `debian:bookworm-slim`, `node:24-bookworm-slim` and `ubuntu:24.04`; `registry.access.redhat.com/ubi9/ubi-minimal` and `node:24-bookworm-slim` as user `node` fail with their message; `node:24-bookworm` as user `node` builds in `/home/node/builds`; a full run with the real runner against an unreachable URL exits 0 after the idle timeout; a wrong hash fails.
- Image `cli` (alpine with the musl `ciburst` and `ciburst-vm-agent`): `edge` on `main`, `X.Y.Z`, `X.Y`, `X` and `latest` on a tag `vX.Y.Z`; merge requests build and smoke-test it without pushing.
- Release on a tag `vX.Y.Z`: the Linux x86_64 musl binary through `glab`; the job fails when `ciburst --version` differs from the tag.

Consumer side (D40): the CI/CD component `templates/ciburst.yml`, included as `component: gitlab.com/ciburst/ciburst/ciburst@<version>` (`main` while private, `1` after the first catalog release). Inputs: `pool` (default `vm`), `version` (the `cli` image tag, default `edge`), `hosted_tag` (default `saas-linux-small-amd64`). It sets `default: tags: [<pool>]`, defines `.ciburst-docker` (`docker:27` with `docker:27-dind`, TLS certificates, registry login, no stage) and three jobs in stage `.pre`: `ciburst-setup` (`ciburst --ci apply`), `ciburst-check` (`ciburst --ci check`) and `ciburst-destroy` (`ciburst --ci destroy --yes "$CIBURST_DESTROY"`, the name from a project variable, because new GitLab projects refuse variables typed into a manual job). Each is manual on the default branch when it is protected, `allow_failure: true`, image `registry.gitlab.com/ciburst/ciburst/cli:<version>`, `tags: [<hosted_tag>]`, `environment: {name: ciburst, action: prepare}`, `resource_group: ciburst`, `interruptible: false`. Lanes are config and job tags, not part of the component.

Pipeline mode (D39), `ciburst --ci`:

1. Config: `ciburst.json` (or the file at `--config`) when it exists, else `{}`, merged over `Config::ci_defaults(project_id, server_url)` by top-level key (section 5.4).
2. Inputs from the process environment only: `CIBURST_SCW_ACCESS_KEY`, `CIBURST_SCW_SECRET_KEY`, `CIBURST_SCW_ORGANIZATION_ID`, optional `CIBURST_SCW_PROJECT_ID`, `CIBURST_GITLAB_TOKEN`, and `CIBURST_HCLOUD_TOKEN` with Hetzner pools. A missing input fails with its name. `SCW_*`, `GITLAB_TOKEN` and `--local` are not read.
3. It refuses to run unless `CI_COMMIT_REF_PROTECTED` is `true`, because protected variables would be missing.
4. Outputs are project variables of `CI_PROJECT_ID`: protected, raw, environment scope `ciburst`; secrets are created masked and hidden; a value GitLab refuses to mask is created unmasked and logged as `output_unmasked`. The next run reads them from its job environment, so a second setup plans only `noop`; `destroy` deletes them. The credentials of `keys::PROVIDER_ENV` stay in memory and are never written.
5. The setup waits for the dispatcher's deploy.

Replay and own jobs (D41): `apply` ends by posting every pending job of each project whose tags contain a pool tag and which is more than 60 s old to the dispatcher as a pending job event (`X-Gitlab-Event: Job Hook`, the hook token), logging `replayed` per job; a failure logs `replay_failed` and `apply` still succeeds. The dispatcher answers jobs named `ciburst-*` with `200 ignored`, reason `own_job`, and `check` skips them. The CLI calls GitLab; the dispatcher never does (D10).

## 9. Testing

| Level | What | Where |
|---|---|---|
| Unit | dispatch decisions (routes, status filter, own jobs, routing by name and stage, default pool, capacity, claims, lane claims of VM slots, burst ledger, tokens), config validation (`jobs` patterns, `vmFirst`), `check` and `--suggest`, plan diffing, outputs stores, cloud-init, the lane command, the sweep rule, log format | `#[cfg(test)]` modules of every crate |
| Contract | `testing::contract::run_provider_contract` for one-job providers: plan on an empty backend lists every resource, plan after apply is empty, a changed pool updates, a removed pool deletes, rotation returns new secrets, start, list and stop, destroy empties the backend | `crates/core/tests/contract_fake.rs`, `crates/scaleway/tests` |
| VM contract | `testing::vm_contract::run_vm_provider_contract`: start with `taken`, capacity, claim, report, sweep of a silent VM, other pools and runtimes ignored, stop | Scaleway Instances, Hetzner, the core fake |
| Races | double claims, pending during drain, bursts of 1 to 10 events, silent VMs, with an injectable clock | `crates/core/tests/vm_races.rs` |
| API fakes | Scaleway (account, IAM, Jobs, Instances, S3, registry, Functions), Hetzner, GitLab (`testing::gitlab_fake::FakeGitlab`: runners and their tags, hooks, lint, variables, pending jobs), as stingy as the real APIs | `src/testing` of each crate |
| CLI | plan, apply, plan, status, destroy on the fake modules; usage and config errors | `crates/cli/tests/cli.rs` |
| Function | routes, environment errors, the ledger | `crates/dispatch-scaleway` |
| Agent | slots, busy detection, reports, drain protocol, terminate retries; the real binary with fake `gitlab-runner` and `docker` on Linux | `crates/vm-agent`, `scripts/test-linux.sh` |
| Lane bootstrap | `images/test/lane-bootstrap.sh` (section 8) | CI job `lane-bootstrap`, locally with Docker |
| `cli` image | smoke test of its binaries | CI image job, locally with Docker |
| Drill | real Scaleway, Hetzner and GitLab | runbook, Acceptance drill |

## 10. Operations

- `ciburst status` prints `dispatcher: <url>`, one line `pool <tag>: <n> live of <maxRuns>` per pool, then up to 20 runs as `run <id> <pool> <state> <created_at> job=<id>` with the states `Queued`, `Running`, `Done`, `Failed`; with a GitLab token (D52) it adds per project `hook <id>: <alert_status>` and the hook's 20 newest deliveries as `delivery <time> <job, pipeline or event kind> <http status> <result> <pool> <ms>`, oldest first.
- `ciburst check` routes every job as the dispatcher does and prints per project `every job matches the stage map` or one line per problem, and exits 1 on any problem; `--ref` lints a branch, tag or commit instead of the default branch. Problems: tags that disagree with the job's pool; for a job routed to a lane, `services:`, an image named `docker` (`docker:...` or a path ending in `/docker`), a script line starting with `docker `, or an `image:` other than the lane's after resolving `$VAR` and `${VAR}` from the pipeline's top-level `variables:` (an unresolvable name is a problem that names it).
- `ciburst check --suggest` groups the Docker-free jobs routed to a VM pool by resolved image and prints per image a lane block for `ciburst.json` (`tag` from the image's last path segment and major version, `runtime` `jobs`, `image`, `jobs` with the job names, `vcpu` 2, `memoryMiB` 4096) and the `tags:` line each job needs; a tag already taken gets `-2`, `-3`; jobs without an image, with an unresolvable image or tagged for a hosted runner get no lane. It exits 0, or 1 when a pipeline cannot be read.
- Logs: one line per action, `<level> <message> key="value"`, never a secret. Cockpit holds no function logs (2026-10-24), so the webhook's recent events (status and JSON answer per delivery) are the dispatcher's visible output; a 500 answer carries only `{"result":"error"}`. `ciburst plan` warns `vm pools cannot start` with the pool and the reason.
- Failure modes and their handling: `docs/RUNBOOK.md`.
- Limits: Instance quotas per organization, zone and type; Serverless Jobs 6 vCPU, 16 GB, 10 GB disk and 24 h per run, 400 parallel runs per organization, no privileged mode, `/tmp` in memory; `linux/amd64` only.
- Backups: nothing to back up; the cache is disposable; `ciburst apply` recreates every resource.

## 11. Attributes not applicable

No UI, no offline mode, English only, no legal pages; authentication is section 7.

## 12. Provider authoring

`docs/PROVIDERS.md`: the trait contracts, the registration checklist, the contract harnesses, and `crates/hetzner` as the template.

## 13. Implementation plan

| Milestone | Deliverables | Verification |
|---|---|---|
| M0, done | workspace, core, CLI on fake modules, contract harness, CI | tests green |
| M1, done | Scaleway Serverless Jobs provider, Scaleway Function host and dispatcher, pool images | tests green against the fakes |
| M2, done | first deployment, registry step, drill | drill results in the runbook |
| M3, done | VM pools on Scaleway Instances with the Docker executor, agent, drain, sweep (`docs/milestones/M3-vm-provider.md`) | drill pipeline passes on VMs |
| M4, done | burst grouping, two slots, swap, composite provider, Hetzner provider (`docs/milestones/M4-cost.md`); the Hetzner drill waits for the owner's token | five VM jobs start three VMs |
| M5, done | `docs/milestones/M5-roundup.md`, D37 to D42: CI/CD component, `--ci`, outputs in project variables, `cli` image, agent from the cache bucket, replay, own jobs, preflight, contributor fixes, docs truth pass | the roundup's definition of done |
| M6, serverless lanes, done | `docs/milestones/M6-serverless-lanes.md`, D45 to D47: lanes in the job's own image with the bootstrap and the runner object, routing by job name, `check` lane rules and `--suggest`, VM slots first; the pool images removed | lane jobs from `node:24-bookworm-slim` and `rust:1.99.0-bookworm` pass on Serverless Jobs and claim a free VM slot first; the drill consumer on the suggested lanes, wall clock and cost against its 18-minute warm VM pipeline |
| M6.1, warm start, built | `docs/milestones/M6.1-warm-start.md`, D48 and D49: pipeline events, one warm VM per pipeline that needs one, slot reservation for its VM jobs, `ignoreJobs` | a Docker job in the second stage behind a lane stage queues under 10 s instead of 60 to 100 s; the drill consumer's pipeline with and without `warmStart`: wall clock, VMs, VM-hours, lane seconds, cost |
| M6.2, queue first and spill, built | `docs/milestones/M6.2-queueing.md`, D53 and D54: VM jobs queue on paid slots, typical durations, start rule with `maxQueueS`, spill runner and agent switching, `spilled` lane jobs with a lane run fallback | the drill consumer's pipeline on 1 VM, at most 2, VM-hours and cost against pipeline 2988692655 |
| M6.3, fleet, specified | `docs/milestones/M6.3-fleet.md`, D57 to D59: invite and join, fleet state in the bucket, VMs with `gitlab-runner run` for all members, check and status across members | a second project in another namespace joins with one job and two variables and shares the demo's VMs; a config change applies within 30 s |
| M7, go public | hardening (`docs/milestones/M7-hardening.md`, D50 to D52), policy files (D27), project visibility public, GitHub mirror, Pages site at `ciburst.zwa.la`, crates.io publication of `ciburst-core`, `ciburst-scaleway` and `ciburst`, `v1.0.0` tag with binaries and the `cli` image | release pipeline green, the `cli` image pulls anonymously from the GitLab registry, the docs site answers |

Definition of done per milestone: tests green locally and on CI, `cargo fmt`, `clippy -D warnings` and `cargo deny` clean, docs updated, no secret and no identifying value in the repo, no em or en dash, summary to the owner with the go question for the next milestone.

Risks:

| Risk | Mitigation |
|---|---|
| GitLab disables a webhook after four failed deliveries; GitLab.com counts an answer after 10 s as failed | a VM start answered in 10.4 s once (M6 drill, right after a dispatcher deploy) and the VM still started; D50 brings it under 6 s and shows `ms` in every answer; D51 re-enables a disabled webhook on the next setup. Re-enable the webhook; `ciburst apply` re-verifies it and replays waiting jobs |
| No Instance quota for a pool's types in its zone | preflight stops `apply` and names zones with quota; `provider.region` |
| Scaleway retires the runtime `rust196` | bump `rust-toolchain.toml` and the host runtime, `ciburst apply` |
| The `v1alpha1` Jobs API changes | client isolated in `crates/scaleway`, fakes pin the shapes |
| Routing and tags disagree, or a lane job needs Docker or names another image | `ciburst check` and the `ciburst-check` job; `check --suggest` prints lanes that pass |
| A lane image lacks `git` or `curl` and cannot install them (RPM-based, non-root) | the run fails with `ciburst-lane:` naming the gap; the lane takes an image that contains them |
| Free tiers or prices change | prices in the runbook; the consumption page |

Glossary: pool (tag, runtime, image, jobs, stages; one GitLab runner), run (a VM, or one Serverless Job of a lane), slot (one `run-single` process on a VM, one job at a time), burst intent (an empty object in the cache bucket recording a pending job that found no free slot), lane (a Serverless Jobs pool for jobs that need no Docker daemon, run in the job's own image), bootstrap (`lane-bootstrap.sh`, the command of every lane run), agent object and runner object (`ciburst-vm-agent` and `gitlab-runner` in the cache bucket, each named by its sha256), dispatcher (the webhook handler), host (where the dispatcher runs), provider (where runs execute), job definition (Scaleway's template for a Serverless Job).

## 14. Owner inputs

1. M5 drills: a fresh GitLab project for the adoption drill; while `ciburst/ciburst` is private, that project's entry in its CI/CD job token allowlist; a token that can read the `ciburst` group's pipelines.
2. Hetzner drill: a Hetzner Cloud account, a project used only by ciburst, an API token with read and write permission in `.local/hetzner.env` as `HCLOUD_TOKEN=...`, and the project's server limit checked under Limits (three servers suffice).
3. At v1.0.0: DNS for `ciburst.zwa.la` (CNAME to GitLab Pages plus the verification TXT record) and the GitHub organization `ciburst`.

## 15. Sources, checked 2026-10-23 to 2026-10-25

Scaleway Jobs API shapes: pkg.go.dev scaleway-sdk-go api/jobs/v1alpha1. Functions API and runtimes: pkg.go.dev scaleway-sdk-go api/function/v1beta1, docs serverless-functions/reference-content/functions-runtimes (Rust 1.96 available, 1.85 end of support), the `cors-rust` example in github.com/scaleway/serverless-examples (axum 0.8 handler, source compiled on upload). Jobs limits: docs serverless-jobs/reference-content/jobs-limitations. Prices: scaleway.com/en/pricing/serverless, scaleway.com/en/pricing/storage. IAM permission sets: docs iam/policies-permissions/permission-sets. GitLab: docs runner/commands (run-single), user/project/integrations/webhook_events (job events carry stage and name, no tags), user/project/integrations/webhooks (templates, auto-disable). Precedent: Yandex Cloud tutorial serverless-gitlab-runner.

M4, 2026-10-24: Scaleway Instance billing (docs instances/reference-content/understanding-instance-pricing: from creation, per hour, 60 minute minimum), prices from the public product catalog API; Hetzner billing FAQ (docs.hetzner.com/cloud/billing/faq: every started hour, servers bill until deleted, IPv6 free) and hetzner.com/cloud/cost-optimized (CX23 0.0096, CX33 0.0144 EUR/h with IPv4); the `docker-ce` app (docs.hetzner.com/cloud/apps/list/docker-ce).

M6, 2026-10-25: Serverless Jobs limits (docs serverless-jobs/reference-content/jobs-limitations: 6 vCPU, 16 GB, 10 GB disk, 24 h, 400 runs, capabilities dropped) and prices (scaleway.com/en/pricing/serverless); Instance types per zone from the Instance products API. Cost comparison: docs.gitlab.com/ci/pipelines/compute_minutes and about.gitlab.com/pricing (cost factors, 10 USD per 1,000 minutes); rocketrunner.io/gitlab-runners; aws.amazon.com/codebuild/pricing and docs.aws.amazon.com/codebuild (GitLab runner); aws.amazon.com/ec2/pricing with the spot price data for eu-central-1; stackhero.io GitLab Runner pricing; cloud-runner.com/pricing; namespace.so, blacksmith.sh, buildjet.com, ubicloud.com, runs-on.com, cirrus-runners.app (GitHub Actions only); ECB reference rate of 2026-10-23.
