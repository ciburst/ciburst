# Spec: ciburst, GitLab runners that exist only while jobs run

Status: built through M6.4 (one VM runner per fleet, lanes per project); next M7, go public.

## 0. Decisions

### 0.1 Decisions in force

Each row states the current state. The date is that of the decision.

| Id | Date | Decision |
|---|---|---|
| D1 | 2026-10-23 | A framework with two ports, a compute provider and a dispatch host, around a provider-agnostic core. Scaleway implements both; the ports keep a provider replaceable (D65). |
| D3 | 2026-10-24 | Rust, edition 2024, one Cargo workspace: `crates/core` (library with the ports), `crates/cli` (binary `ciburst`), `crates/scaleway` (provider and host), `crates/dispatch-scaleway` (the function), `crates/vm-agent` (the VM supervisor). |
| D6 | 2026-10-23 | Secrets live in the dispatcher's secret environment; the fleet's member records keep theirs sealed in the bucket (D58). Runner tokens and the cache key reach each run as environment overrides. |
| D8 | 2026-10-23 | Name `ciburst`. The config value `name` prefixes every cloud resource and every GitLab runner description, so the code name stays stable. |
| D9 | 2026-10-23 | Cache on a Scaleway Object Storage bucket shared by every run; cache objects expire after `cache.expireDays` (30). |
| D10 | 2026-10-23 | The dispatcher never calls GitLab. It routes by the event's project and job name (section 5.5); each lane has its own GitLab runner and tag, so a job runs only in its lane's image or on the VM runner. |
| D11 | 2026-10-23 | A lane run executes `run-single --max-builds 0 --wait-timeout <waitTimeoutS>`: it takes jobs while they arrive and exits after the idle window. A lane's `maxRuns` caps its parallel runs. |
| D12 | 2026-10-23 | Delivery by milestones, each with an explicit owner go; implementation by agents in worktrees; pushes only on the owner's confirmation. |
| D13 | 2026-10-23 | License Apache-2.0. |
| D15 | 2026-10-23 | `ciburst check` lints each project's pipeline against that project's lanes, its `ignoreJobs` and the VM runner (CI lint API, owner token), so no job waits for a runner that will not take it. A scheduled sweep for lost deliveries is backlog. |
| D16, D43 | 2026-10-23, 2026-10-25 | One region per deployment, default `fr-par`: the dispatcher, the cache bucket, the agent and runner objects, the VMs and the lane runs live in `provider.region`, and a `provider.zone` outside it is refused. Measured: VMs in `nl-ams-1` against a bucket in `fr-par` spent 555 s moving a warm Rust job's caches against 277 s of script. Scaleway offers `rust196` in every region. The drill consumer's project id lives in `.local/` only. |
| D17 | 2026-10-24 | The dispatcher is a Scaleway Serverless Function on runtime `rust196`. Scaleway compiles the uploaded source, so the workspace is pinned to Rust 1.96.0 (`rust-toolchain.toml`) and CI builds with the same version. A runtime bump is a one-line change plus a redeploy. |
| D18 | 2026-10-24 | Few audited dependencies instead of none (tokio, reqwest with rustls, serde, clap, axum, async-trait, sha2, hmac, aes-gcm, subtle, rand, thiserror and a few more). `cargo deny` gates licenses, advisories and sources in CI. The HTTP seam is one trait, so every API client is tested against in-memory fakes. |
| D19 | 2026-10-24 | `gitlab-runner` is pinned at 18.11.4 by URL and SHA-256 (`ciburst_core::cloud_init::RUNNER_URL`, `RUNNER_SHA256`); every flag ciburst passes was verified against its help text. A move to 19.x follows a drill. |
| D22 | 2026-10-24 | Releases on tags `vX.Y.Z` publish the `cli` image as `X.Y.Z`, `X.Y`, `X` and `latest` (`edge` on `main`) and the Linux x86_64 musl binary through `glab`. macOS and Windows binaries are backlog. |
| D23 | 2026-10-24 | Adopters set up through their own pipeline, not from a workstation: a manual `ciburst-setup` job of the component runs the `ciburst` binary on a GitLab hosted runner (free 400 minutes per month), takes the Scaleway key and a personal access token from four masked CI variables, and stores its outputs as masked project variables. The local CLI with `.local/` is the developer path. |
| D24 | 2026-10-24 | Hosting: canonical on GitLab.com in the group `ciburst`, project `ciburst/ciburst`, private until v1.0.0, then public with a read-only push mirror to the GitHub organization `ciburst`. Issues and merge requests stay on GitLab. |
| D25 | 2026-10-24 | No GitLab for Open Source program, so a commercial service stays possible. The framework's CI runs on the Free tier's 400 minutes per month; path rules and merge-request pipelines keep it there. |
| D26 | 2026-10-24 | Project domain `ciburst.zwa.la`: the docs site on GitLab Pages at v1.0.0. It is the one real hostname allowed in the repository. |
| D27 | 2026-10-24 | Contributions under the Developer Certificate of Origin (`Signed-off-by`), copyright "The ciburst Authors", commit authors unchanged. Policy files (`CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, `NOTICE`) land with v1.0.0. |
| D28 | 2026-10-24 | The Scaleway Container Registry namespace `<name>` is optional (`provider.registry`, public with `registryPublic`) and holds private lane images; Serverless Jobs pull from it without credentials (intra-region pulls free, storage 0.027 EUR per GB per month). |
| D30 | 2026-10-24 | The VM runner: one Scaleway Instance per VM with a local Docker daemon and GitLab's Docker executor, so per-job `image:`, `services:`, Docker-in-Docker, image builds and testcontainers work as on GitLab's own runners. Lanes on Serverless Jobs serve jobs without a Docker daemon (D45). Other providers (Fly Machines, Lambda MicroVMs, spot VMs, remote builders) are backlog. |
| D31 | 2026-10-24 | A VM ends itself through the dispatcher, never with a cloud key: Scaleway bills a guest `poweroff` as standby, and IAM cannot limit a key to one Instance. Each VM gets a capability token (HMAC of its server id with a secret only the function holds) that can only report its own state and end its own VM. A cron trigger every 30 minutes sweeps VMs that stopped reporting (sections 6.2, 6.8). |
| D32 | 2026-10-24 | Cost first (owner: "Optimize for costs"). Scaleway bills a CPU Instance from creation per started hour with a 60 minute minimum, so the dispatcher starts no VM it does not need and fills every VM it pays for: burst grouping (D33), queue first (D53), `starting` VMs count as capacity, claims spread over VMs, `maxRuns` holds under parallel events, two slots per VM by default with a 4 GiB swap file, and an idle VM drains at minute 55 of its paid hour. |
| D33 | 2026-10-24 | Burst grouping without a lock. A pending VM job that finds no free slot records an intent (an empty object in the cache bucket), waits 2 s and reads the intents of the last 30 s. Ranked by time and job id, every `slots`-th job leads a group and starts one VM; the others ride on their leader's VM; leaders count toward `maxRuns` by rank. A cold pipeline with five VM jobs starts three VMs at two slots. When the ledger fails, the job starts one VM (section 6.3). |
| D38 | 2026-10-25 | VMs fetch the agent binary from the cache bucket: `apply` uploads the musl `ciburst-vm-agent` as the public object `ciburst/agent/<sha256>`; cloud-init downloads it and checks the hash from the user data. The lifecycle rules expire only the cache and intent prefixes. Every deployment has an S3 cache, which also holds the runner object (D45); the registry is optional (D28). |
| D39 | 2026-10-25 | Pipeline mode `ciburst --ci`: inputs `CIBURST_SCW_ACCESS_KEY`, `CIBURST_SCW_SECRET_KEY`, `CIBURST_SCW_ORGANIZATION_ID`, `CIBURST_GITLAB_TOKEN`, never `SCW_*`, so an adopter's own Scaleway variables cannot redirect the setup. Inputs and outputs are protected, masked and hidden where GitLab allows, and scoped to the environment `ciburst`, so no build job, VM jobs included, sees them. The config is `ciburst.json` merged over defaults from the job environment (the VM runner, no lanes). The setup waits for the dispatcher's deploy. |
| D40 | 2026-10-25 | The consumer include is the CI/CD component `templates/ciburst.yml` (`include: component: gitlab.com/ciburst/ciburst/ciburst@<version>`) with the inputs `vm_tag`, `version`, `hosted_tag` and `role`. As `host` it adds the manual jobs `ciburst-setup`, `ciburst-check`, `ciburst-destroy` and `ciburst-invite`, as `member` the jobs `ciburst-join` and `ciburst-leave`, all on the protected default branch. Docker jobs extend `.ciburst-docker`. Lanes come from the project's own `ciburst.json` and its jobs' tags. |
| D41 | 2026-10-25 | The dispatcher ignores pending jobs named `ciburst-*` (the component's own jobs run on hosted runners), and `apply` ends by replaying every job that waits for a tag of the host's runners, so the pipeline that ran the setup continues. The CLI calls GitLab; the dispatcher does not (D10). |
| D42 | 2026-10-25 | `apply` changes nothing when the VM runner could not start a VM (no Instance quota for its types in the zone, no agent): `Provider::preflight`. `plan` only warns. |
| D44 | 2026-10-25 | VM jobs and lane runs archive caches with `FF_USE_FASTZIP=true` and `CACHE_COMPRESSION_LEVEL=fastest`: the bucket is in the runs' region, so bytes are cheap and compressing was the slow part. |
| D45 | 2026-10-25 | Lanes run the job's own image (owner: "implement the new strategy"). A lane run is a Scaleway Serverless Job from any public `linux/amd64` image with `sh`; it executes `lane-bootstrap.sh`, which installs missing `git`, `curl` and CA certificates (`apk`; `apt-get` as root in its own directories, because the sandbox drops root's capabilities; RPM-based images must ship them), fetches the pinned gitlab-runner from the deployment's bucket (public object `ciburst/runner/<sha256>`, hash-checked) and runs `run-single` with the shell executor. Measured in `nl-ams`: runs start in 5 to 28 s, a Rust compile at 2 vCPU matches GitLab's small runner and at 4 vCPU takes half its time, a TypeScript check is within 12 %; the bootstrap adds 0 to 21 s by image. |
| D46 | 2026-10-25 | A lane takes jobs by name: `jobs` glob patterns over the job name. `ciburst check` flags a lane job that needs a Docker daemon or names another image than its lane; `check --suggest` prints lanes for the Docker-free jobs on the VM runner. |
| D48 | 2026-10-25 | Warm start (owner: "spec the optimization"). The project hook also delivers pipeline events, which list every job of a pipeline by name, stage, status and `when`, without tags. On a pipeline's first `pending` or `running` event the dispatcher routes all its jobs; when any goes to the VM runner (manual and `on_failure` jobs, own jobs and `ignoreJobs` left out) and no VM has a free slot or is starting, it starts one VM at once, so a Docker job of a later stage finds a booted VM instead of waiting 60 to 100 s. The VM joins the burst ledger with every slot free. Cost: one VM-hour (0.03 EUR on DEV1-M) when a pipeline ends before its VM jobs run. Option `vm.warmStart`, default true. |
| D49, D61 | 2026-10-25, 2026-10-27 | `ignoreJobs`: job name patterns of a project that run on other runners, such as GitLab's hosted runners, declared in that project's own `ciburst.json`, because job names are a project's own. Job events and pipeline events skip them, so their pending events start nothing. `ciburst check` reports a job tagged only for other runners that `ignoreJobs` misses, and a job in `ignoreJobs` tagged for a ciburst runner; `check --suggest` prints the list. |
| D50 | 2026-10-25 | Answer budget (owner: "harden"). GitLab.com counts a webhook answer after 10 s as failed and disables the webhook after four failures in a row. Every answer carries `ms`, the dispatcher's elapsed milliseconds, which the webhook's recent events show. The function gets 1024 MB, because Scaleway scales a function's CPU with its memory. `apply` resolves the VM security group and the image per instance type and hands them to the function, so a VM start makes no lookup unless one is missing or refused. A start that fails with 429 or 503 (for idempotent calls also another 5xx or a transport failure) is retried once while less than 5 s have passed; a create that timed out is not, because it may have created the server. Target: a VM start answers within 6 s on a warm function and within 9 s right after a deploy. |
| D51 | 2026-10-25 | Webhook health. `plan` reads each project hook's `alert_status` and `disabled_until`; a hook that is not `executable` plans `update` with the reason, and `apply` sends GitLab's test delivery, which the dispatcher answers 200 and GitLab takes as the sign to enable the hook again, before it replays the waiting jobs. `ciburst check`, and with it the `ciburst-check` job, reports a disabled hook. |
| D52 | 2026-10-25 | Deliveries in `ciburst status`: with a GitLab token it prints each project hook's last 20 deliveries (time, job or pipeline, HTTP status, result, runner, `ms`) from GitLab's hook events API, because Cockpit holds no function logs. |
| D53 | 2026-10-26 | Queue first (owner: "leverage some amount of queueing ... only one (or 2 in case of real needed parallelism) VMs"). A VM job that finds every slot of a running VM busy waits in a queue in the cache bucket instead of starting a VM, and GitLab hands it to the first slot that frees. The dispatcher learns each job name's typical duration from the job events (running, finished) and buys another VM only for a queued job whose wait would pass `vm.maxQueueS` (default 90 s, about one VM boot), or that no running job will free a slot for. Busy VMs report every 30 s, and each report re-evaluates the queue. Measured cause: the drill consumer's pipeline used 4 VMs where 1 would do. |
| D56 | 2026-10-26 | Late pending events start nothing (owner: "make the guard right now"). GitLab.com delivered job status events about 35 minutes late on 2026-10-26 for hours, while pipeline events stayed on time, and the late `pending` events started runs for jobs that had long finished. A pipeline's final event leaves `ciburst/intents/_closed/<pipeline>` with its time and the ids of its jobs that were still manual. A `pending` job event of that pipeline whose job was created before the close and was not manual then answers `ignored`, reason `pipeline_closed`, and logs `stale`; a retried job (created after the close) and a manual job played later start as usual. One read per pending event; the lifecycle rule of `ciburst/intents/` removes the records. |
| D57 | 2026-10-26 | One fleet, many projects (owner: "Plan B looks good"). The first project sets ciburst up with all its variables and becomes the host; any later project, in any group or organization, joins through an invite code from the host and a manual `ciburst-join` with two variables (the code and its own GitLab token for its own project). The member registers its own runners and webhook, because GitLab.com shares runners across groups only with Maintainer rights in both and offers group webhooks only on paid tiers, and hands the runner tokens to the dispatcher's `/join`. One dispatcher, bucket and VM runner serve all members; lanes and `ignoreJobs` stay each project's own (D60, D61). |
| D58 | 2026-10-26 | Fleet state in the bucket: the routing config and the member records (webhook secret, sealed runner tokens, lanes, `ignoreJobs`) live in `ciburst/fleet/`, and the dispatcher reads them with a cache of at most 30 s. A config change or a join takes effect without a function deploy, which takes about 10 minutes. |
| D59 | 2026-10-26 | VMs run one `gitlab-runner run` with a runner entry per member and `concurrent = slots`, so one VM serves every member's VM jobs within its slots; VM jobs of all members share VMs (owner: "we can assume vm sharing per default for now"). Measured on a real runner before it was built (section 6.2). |
| D60 | 2026-10-27 | Lanes belong to one project (owner: "never share lanes between projects ... scope per project"). Each project declares its lanes in its own `ciburst.json`; a lane is the definition `<name>-<project id>-<tag>` and one runner in that project. Routing looks at the job's own project only, so a lane never takes, routes or is reused by another project's job. The host's lanes are made by `ciburst-setup`, a member's by the dispatcher on `/join` with the dispatch key. No ciburst limit beyond Scaleway's (owner's choice). |
| D62 | 2026-10-27 | The VM runner's GitLab runner carries only its own tag, and lane jobs run only in lane runs (owner: "Drop vmFirst"). |
| D63 | 2026-10-27 | One VM runner per fleet (owner: "we have a generic serverless runner and a vm runner"): the config key `vm`; a job that no lane of its project and no `ignoreJobs` entry takes runs on the VM runner. |
| D64 | 2026-10-27 | The docs describe the current state only (owner: "Reorganize the project documentation completely"): the decision log holds the decisions in force, each superseded one keeps one row with the reason it was dropped, and the spec holds every binding rule. |
| D65 | 2026-10-27 | Scaleway is the only provider (owner: "you can also remove the hetzner plug for now"). The `Provider` and `Host` ports stay, so a provider can return (`docs/PROVIDERS.md`). |
| D66 | 2026-10-27 | No pools in what a user sees (owner: "We do not have pools"). The dispatcher's answers name the runner a job went to as `runner` (its GitLab tag) and `kind` (`vm` or `lane`), the warm answer lists `runners`, log lines carry `runner=`, messages say `vm runner <tag>` or `lane <tag> of project <id>`, the VM's runner entry is named `vm`, and `GET /fleet` no longer sends `pools`. Owner, 2026-10-27: the environment variable `CIBURST_POOL` in `vm.env` and in a lane definition is `CIBURST_RUNNER`, and the server tag `pool:<tag>` is `runner:<tag>`. New servers carry only `runner:<tag>`; listing, capacity, claim, report and sweep read `pool:<tag>` where `runner:<tag>` is missing, for servers started before the deploy. Setup and `/join` update a lane definition whose environment still names `CIBURST_POOL`. The VM agent reads only `CIBURST_RUNNER`: a VM gets its agent and its `vm.env` from the same dispatcher at start. |
| D67 | 2026-10-28 | A VM asks GitLab for the jobs of every project in turn, without long polling (owner: "a shared runner that can take jobs from every project but no single project has to queue at the runner ... Parallelism is reached through scaling to multiple runners"). `gitlab-runner` starts `concurrent` request workers, and gitlab.com holds a job request that finds no job for about 50 s when it carries `last_update`; with `concurrent = slots` below the number of entries, a project's job waited for the other entries' held requests (measured 52 to 64 s with 1 slot and 3 entries). The agent relays the runner's GitLab traffic through a local proxy that drops `last_update` from job requests, so each request is answered at once: the same setup measured 0.3 to 2.6 s, and `concurrent = slots` stays the exact job limit. Raising `concurrent` was rejected: it is `gitlab-runner`'s only limit on jobs, and every entry would take a job when a slot frees. |
| D68 | 2026-10-28 | In the start rule a busy slot of a live VM that no running record accounts for frees now (section 6.4). A VM reports a freed slot up to 5 s late, so a job that went pending right after its predecessor ended met `free 0`, expected no slot and started a VM (production, 2026-10-28). A busy slot whose job's `running` event is still on its way counts as free too; that job's follower waits at most `vm.maxQueueS`, then step 5 starts a VM. |

### 0.2 Dropped

| Id | Was | Why we do not want it | Replaced by |
|---|---|---|---|
| D2 | One serverless `run-single` container per job, no VMs | Serverless containers have no Docker daemon, so `image:`, `services:` and image builds failed | D30, D45 |
| D4 | Shell executor in ciburst's own image; the job's `image:` ignored | Jobs lost their own images | D30, D45 |
| D5 | No Docker daemon anywhere; Docker jobs on hosted runners | Docker jobs belong on the adopter's runners at VM prices | D30 |
| D7 | ciburst's own job images built by the framework's CI | Lanes run the job's own image; nothing to build or keep current | D45 |
| D14 | Fixed lanes `ci`, `rust`, `playwright` | Each project derives its lanes from its own images | D45, D60 |
| D20 | A kaniko build lane | Kaniko cannot unpack images in the Serverless Jobs sandbox | D30 |
| D21 | Images write their `PATH` to `/etc/profile.d` | No ciburst images; the bootstrap passes the image's `PATH` as a job variable | D45 |
| D29 | Image builds on hosted runners through `.ciburst-image-build` | VMs build images with Docker-in-Docker at VM prices | D30, D40 |
| D34 | Hetzner Cloud as a second VM backend | One provider keeps setup and code small; it was never drilled | D65 |
| D35 | A composite provider per runtime | One provider remains | D65 |
| D36 | Routing by stage with an optional default runner | Stages say nothing about the image or the runner a job needs; a project's `ignoreJobs` names the jobs that run elsewhere | D49, D61, D63 |
| D37 | The scope of the M5 roundup | A milestone scope, delivered (section 14) | D64 |
| D47 | Free VM slots take lane jobs first (`vmFirst`) | Lane jobs took the slots Docker jobs needed, which started extra VMs (4 instead of 1 on the drill consumer); lane tags on a shared VM runner would cross projects | D62 |
| D54 | A spill runner that pointed idle slots at lane jobs | Lane runs that lost the race billed idle tails, and a shared VM runner must not carry a project's lane tags | D62 |
| D55 | The VM runner carries the `vmFirst` lanes' tags by default | As D54; the late events it answered are guarded by D56 | D62 |

## 1. Purpose and scope

ciburst gives GitLab projects CI runners that exist only while jobs run. A GitLab webhook reaches a dispatcher, a Scaleway Function, which routes each pending job by its project and name. A job of a project's lane starts one Scaleway Serverless Job from the lane's image. Any other job goes to the VM runner: the dispatcher gives it a free slot on a running VM, queues it for a slot that frees soon, or starts a Scaleway Instance that runs jobs with GitLab's Docker executor and ends itself once idle at minute 55 of its paid hour. One setup (a fleet) serves many projects. Nothing runs between pipelines.

v1:

1. `crates/core`: config, routing, dispatcher, GitLab client, plan and apply engine, provider and host ports, burst ledger, queue, fleet state, invite codes, VM user data, lane bootstrap, contract harnesses, fakes.
2. `crates/scaleway` (provider `scaleway` with the runtimes `instances` and `jobs`, host `scaleway-function`), `crates/dispatch-scaleway` (the function), `crates/vm-agent` (the VM supervisor).
3. Binary `ciburst`: `plan`, `apply`, `destroy`, `status`, `check` (with `--suggest`), `rotate`, `invite`, `join`, `leave`, pipeline mode `--ci`.
4. Image `cli` for the component's jobs.
5. CI/CD component `templates/ciburst.yml`; README, runbook, provider guide.

Backlog, not v1: providers `fly-machines`, `aws-lambda-microvms` (amd64 support pending), spot VMs and providers on other clouds, remote build services; a scheduled poll of pending jobs; a self-hosted dispatcher binary; group-level webhooks; a hosted dispatcher as a service; GitHub Actions support.

Not applicable: UI, offline mode, languages other than English, legal pages. Authentication is section 10.

## 2. Principles

- Zero fixed cost: nothing runs or is reserved between pipelines; the function and its cron stay inside Scaleway's free tier.
- Fill every paid VM hour: group bursts, queue for busy slots, reuse warm VMs, start no VM that is not needed (D32, D33, D53).
- Zero ops: no runner manager, no host to patch; every VM ends itself, the sweep ends any VM that stops reporting, and no VM lives longer than 24 hours.
- Contract first: the provider and host ports and their contract harnesses exist before any provider.
- Config as code: every setup with an API is a step in the idempotent plan; clicks only where no API exists.
- Few dependencies, every one gated by `cargo deny`; one static binary for users.
- No identifying data in the repository; brief docs; no em or en dashes.

## 3. Terms

| Term | Meaning |
|---|---|
| VM runner | The fleet's one runner on Scaleway Instances VMs with GitLab's Docker executor, shared by every member; config key `vm`, GitLab runners with its tag (default `vm`) |
| Lane | One project's Serverless Jobs definition for one image and size, with that project's own GitLab runner and tag; config key `lanes` |
| VM job, lane job | A job routed to the VM runner, or to a lane |
| Slot | One of the `concurrent` jobs of a VM's `gitlab-runner run` |
| Run | A VM, or one Serverless Job of a lane |
| Fleet | A deployment and the projects it serves |
| Host | The project that ran the first `ciburst-setup`; its CI variables hold the fleet's outputs; the fleet's first member |
| Member | A project the fleet serves; any but the host joined through `ciburst-join` |
| Project settings | A project's own `lanes` and `ignoreJobs`, from its own `ciburst.json` |
| Fleet settings | `name`, `gitlab`, `host`, `provider`, `cache`, `vm`: the host's `ciburst.json` only |
| Burst intent | An empty object in the cache bucket recording a pending VM job that found no free slot |
| Bootstrap | `lane-bootstrap.sh`, the command of every lane run |
| Agent object, runner object | `ciburst-vm-agent` and `gitlab-runner` in the cache bucket, each named by its SHA-256 |
| Dispatcher | The webhook handler; host: where it runs; provider: where runs execute |
| Job definition | Scaleway's template for a Serverless Job |
| `Pool` | Internal Rust type for what one run needs, the VM runner or one lane; answers name it as `runner` (the GitLab runner's tag) and `kind` (`vm` or `lane`), logs as `runner=` (D66) |

## 4. Personas and journeys

| Persona | Journey | Steps |
|---|---|---|
| Project owner | Give a GitLab project runners | README, Adopt: a Scaleway key, a personal access token, four protected CI variables, the component include, the manual `ciburst-setup` job |
| Project owner | Join an existing fleet | README, Join an existing setup: a code from the host, two variables, the component with `role: member`, `ciburst-join` |
| Project owner | Add or change lanes | `ciburst check --suggest`; the project's own `ciburst.json` and its lane jobs' `tags:`; `ciburst-setup` in the host, `ciburst-join` in a member; `ciburst-check` |
| Developer | Run ciburst from a workstation | runbook, Developer setup: `.local/` files, `ciburst plan`, `apply`, `check` |
| Provider author | Add a cloud | implement `Provider` in a crate, pass the contract harnesses against an API fake, register it, document it (`docs/PROVIDERS.md`) |
| Operator | See what runs and what it costs | `ciburst status`, the webhook's recent events, the VM tags, the provider's consumption page |

## 5. Architecture

### 5.1 Stack

| Part | Choice | Reason |
|---|---|---|
| Language | Rust 1.96, edition 2024, Cargo workspace | one static CLI binary, typed ports, the toolchain Scaleway compiles the function with |
| Dispatcher host | Scaleway Serverless Function, runtime `rust196`, 1024 MB (Scaleway gives it 560 mvCPU; CPU scales with memory), timeout 30 s, a cron trigger every 30 minutes | free tier of 1,000,000 requests per month, cold start well under a second |
| VM runner | Scaleway Instances DEV1-M (3 vCPU, 4 GB, 40 GB local SSD), two slots, 4 GiB swap | a Docker daemon per VM; billed per started hour with a 60 minute minimum, about 0.03 EUR per hour |
| Lanes | Scaleway Serverless Jobs, up to 6 vCPU and 16 GB | start in seconds from the job's own image and bill per second, without a Docker daemon |
| Cache and state | Scaleway Object Storage bucket, S3 API | shared by every run; also holds the burst intents, the queue, the pipeline records, the fleet state, the agent object and the runner object |
| Runner | `gitlab-runner` 18.11.4 from the runner object: one `run` with the Docker executor on each VM, `run-single` with the shell executor in each lane run | no runner manager, no registration state |
| Tests | `cargo test`, contract harnesses, in-memory API fakes behind the `HttpClient` trait | no sockets, no cloud |

### 5.2 Request flow

1. GitLab sends the job events and pipeline events of each member project to the dispatcher URL, with that project's webhook secret in `X-Gitlab-Token`.
2. The dispatcher accepts an event of a member only with that member's secret, the host's with the hook token; anything else answers `401` without a body.
3. A job event with status `pending` is routed (section 5.5): a lane job starts a lane run (section 7.2), a VM job claims a slot, queues or starts a VM (sections 6.3, 6.4). `running`, `success`, `failed`, `canceled` and `skipped` are tracked for the queue (section 6.4); other statuses answer `ignored`, reason `status`.
4. A pipeline event warms the VM runner or closes the pipeline's records (section 6.5).
5. `POST /vm` and `POST /terminate` come from VMs (section 6.2), a JSON body with `ciburst_sweep` from the cron (section 6.8), `GET /fleet`, `POST /join` and `POST /leave` from members' jobs (section 8.3).
6. Every answer with a body carries `ms`, the whole milliseconds since the request arrived, and every decision log line ends with `ms` (D50). An answer `500` carries only `{"result":"error"}`, on `/join` and `/leave` also the `step`.
7. The function keeps no state of its own: VM state lives in the servers' tags, everything else in the bucket.

### 5.3 Ports and core modules

The `Provider` and `Host` traits live in `crates/core/src/types.rs`; their contracts, the registration of a provider and its tests are in `docs/PROVIDERS.md`.

Core modules: `config` (schema, defaults, routing, validation reporting every error), `dispatch` (routes and scheduling, pure apart from provider, ledger, record and fleet calls; `dispatch::members` for the fleet), `gitlab` (runners, hooks, CI lint, variables, pending jobs, hook events), `http` (the one HTTP seam, `ReqwestClient` in production), `plan` (formatting, outputs stores), `provision` (phases: provider, runners, fleet state, host, hooks, replay), `check` (lint against lanes and `ignoreJobs`, lane suggestions), `pipeline` (the pipeline YAML a lint reads), `ledger` (burst intents, queue, pipeline records), `queue` (the start rule), `fleet` (fleet state and member records), `invite` (join codes), `cloud_init` (VM user data, the runner pin), `lane` (the bootstrap script and the job command), `vm` (VM tokens, timings, the sweep rule), `time`, `store`, `log` (one line per action, never a secret), `fake`, `testing` (contract harnesses, GitLab fake).

### 5.4 Configuration

A project's `ciburst.json` at its repository root. The host's holds the fleet settings and the host's project settings; a member's holds only its project settings.

Pipeline mode merges the host's file over the defaults by top-level key: a key in the file replaces the default's whole value. The defaults for a project with id 12345678:

```json
{
  "name": "ci-12345678",
  "gitlab": { "url": "https://gitlab.com", "projectIds": [12345678] },
  "host": { "type": "scaleway-function", "region": "fr-par" },
  "provider": { "type": "scaleway", "region": "fr-par" },
  "vm": { "tag": "vm", "runtime": "instances", "image": "ubuntu:24.04", "slots": 2, "instanceTypes": ["DEV1-M", "DEV1-L"], "maxRuns": 4 },
  "cache": { "type": "s3", "expireDays": 30 }
}
```

Fleet settings, in the host's file only:

| Key | Meaning |
|---|---|
| `name` | prefix of every cloud resource and runner description; 2 to 20 lowercase letters, digits or hyphens, starting with a letter, so the bucket name `<name>-cache-<project id>` stays within the S3 limit of 63 |
| `gitlab` | `url` (https) and `projectIds`: the host first; runners belong to the host, the VM runner's is enabled in the others, every project gets a webhook (section 8.6) |
| `host` | `type` `scaleway-function`, `region` |
| `provider` | `type` `scaleway`, `region` (every resource); optional `zone` (Instances, default `<region>-1`, must lie in `region`), `registry` and `registryPublic` (D28) |
| `vm` | the VM runner, one object; a key left out takes its default: `tag` (`vm`; lowercase letters, digits and hyphens, not digits followed by a hyphen, the form of a lane key), `runtime` (`instances`, the only one), `image` (`ubuntu:24.04`, the job image when a job names none), `slots` (2), `instanceTypes` (`DEV1-M`, `DEV1-L`; types with a local root volume), `maxRuns` (VMs at once, 4), `maxQueueS` (0 to 3600, 90, D53), `warmStart` (true, D48) |
| `cache` | `type` `s3` (required: the bucket holds the runner object, the agent object, the intents and the fleet state), `expireDays` (30) |

Project settings, in every project's file, the host's included:

| Key | Meaning |
|---|---|
| `lanes[]` | `tag` (lowercase letters, digits and hyphens, unique, not the VM runner's tag; the definition name `<name>-<project id>-<tag>` stays within 63 characters), `image` (the job image: `linux/amd64` with `/bin/sh`, pullable without credentials), `jobs` (at least one job name pattern; a pattern belongs to one lane), `vcpu` (2, at most 6), `memoryMiB` (4096, at most 16384), `storageMiB` (10240, at most 10240), `timeoutS` (3600, at most 86400), `waitTimeoutS` (60), `maxRuns` (lane runs at once, 10) |
| `ignoreJobs` | job name patterns of jobs on other runners (D49, D61); each non-empty |

Validation reports every error together, each naming its field. Keys of earlier configs are refused with a message naming the replacement, at the top level, in `vm` and in `lanes[]`: `pools` (`vm` and `lanes`), `defaultPool` (a job no lane and no `ignoreJobs` entry takes runs on the VM runner), `stages` (lanes take jobs by name), `vmFirst` and `spillRunner` (the VM runner carries only its tag), `providers` (Scaleway is the only provider). Any other unknown key is refused: at the top level of the host's and a member's file with `<key>: unknown key; the keys are <the keys that file may hold>`, in `vm` and in a lane by its name. `ciburst-join` refuses a fleet setting in a member's file: `<key>: a fleet setting; it belongs in the host's ciburst.json`.

Where the config takes effect: `ciburst-setup` writes the whole config to the fleet state (section 8.1), which the dispatcher reads within 30 s. The function's `CIBURST_CONFIG` holds the deployment view (`Config::deployment_view`: without `ignoreJobs`, the lanes' `jobs` and `maxRuns`, and `vm.slots`, `vm.maxRuns`, `vm.maxQueueS`, `vm.warmStart`), so a change of only these keys writes the fleet config and leaves the function as it is. A member's settings take effect with its next `ciburst-join`.

Credentials: on the developer path `.local/scaleway.env` (`SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID`, optional `SCW_DEFAULT_PROJECT_ID`) and `.local/gitlab.env` (`GITLAB_TOKEN`, scopes `api` and `create_runner`; a `GITLAB_TOKEN` in the environment wins); in pipeline mode the variables of section 11.3.

### 5.5 Routing

A pending job of project P (`config::route`):

1. A name starting with `ciburst-` starts nothing (`own_job`, D41).
2. A name that matches a pattern of P's `ignoreJobs` starts nothing (`ignore_jobs`).
3. The first of P's lanes, in file order, with a `jobs` pattern that matches the name gets a lane run.
4. Any other job goes to the VM runner. A member that registered no VM runner answers `ignored`, reason `no_runner`.

Patterns: `*` matches any run of characters, `?` one character, every other character itself. The host's settings come from the fleet config, a member's from its record. Patterns of another project never apply to P; two projects may use the same lane tags and patterns, and a job of project A whose name matches only project B's lane goes to the VM runner. The stage plays no part.

The event carries no tags, so a job must carry the tag of the runner its route selects: its lane's tag, or the VM runner's tag (the component sets it as `default: tags`). A job tagged only for other runners runs elsewhere and belongs in `ignoreJobs`. `ciburst check` reports each mismatch (section 13.2).

### 5.6 Repository layout

```
Cargo.toml  rust-toolchain.toml  deny.toml  LICENSE  README.md  AGENTS.md  config.example.json
crates/core/               ciburst-core: config, dispatch, gitlab, provision, check, ledger, queue, fleet, invite, cloud_init, lane (lane-bootstrap.sh), vm, fake, testing
crates/cli/                binary ciburst: commands, pipeline mode, invite, join, leave
crates/scaleway/           provider scaleway (instances, jobs, IAM, S3, registry), host scaleway-function, ledger, schedule, records and fleet store in S3, build.rs packing the function zip, API fakes
crates/dispatch-scaleway/  the function: handle, serve, respond
crates/vm-agent/           binary ciburst-vm-agent: config.toml, reports, drain
images/cli/                the setup image with ciburst and ciburst-vm-agent
images/test/               the lane bootstrap test, runner checks, the cli smoke test
templates/ciburst.yml      the CI/CD component
scripts/                   CI helpers, agent-binary.sh, test-linux.sh, shellcheck.sh, check-pins.sh
test/config.fake.json      dry-run config
test/drill/gitlab-ci.yml   drill pipeline
docs/                      SPEC.md RUNBOOK.md PROVIDERS.md
.gitlab-ci.yml  .local/ (gitignored)
```

## 6. VM runner

### 6.1 VMs on Scaleway Instances

Runtime `instances` of provider `scaleway`, in `provider.zone` (default `<region>-1`):

| Call | Effect |
|---|---|
| start | `POST /instance/v1/zones/<zone>/servers`: name `<name>-<tag>-<8 hex>`; the first type of `vm.instanceTypes` with capacity, falling back to the next on out-of-stock or quota errors, only types with a local root volume (terminate deletes local volumes but only detaches block volumes, which keep billing); Scaleway's marketplace image labelled `docker` in its `instance_local` variant, fallback `ubuntu_noble` with Docker installed by cloud-init; `dynamic_ip_required`, `routed_ip_enabled`, the security group `<name>-vm`; tags `ciburst`, `<name>`, `runner:<tag>`, `job:<id>`, `ciburst-state:starting`, `ciburst-free:<slots - taken>`. A server without `runner:<tag>` is read by its `pool:<tag>` (D66). Then user data `cloud-init` and `poweron`. The run id is the server id. The provider computes `ciburst_core::vm::token(<VM secret>, <server id>)`; the VM secret never reaches the VM. |
| list, capacity | servers tagged `<name>`; `starting` and `running` are live; the tags give state, free slots and last report; `created_at` from `creation_date` |
| claim | lowers `ciburst-free` by one, not below zero |
| report | rewrites `ciburst-state:<state>`, `ciburst-free:<n>`, `ciburst-seen:<unix time>` and, once draining, `ciburst-drain:<unix time>`, keeping every other tag |
| stop | `terminate`: deletes the local volume, releases the dynamic IP |

Cloud-init (`ciburst_core::cloud_init::render`): SSH password login off, a 4 GiB swap file at `/swapfile` (skipped when it exists), `/etc/ciburst/vm.env` (mode 0600), `gitlab-runner` from the runner object `https://<bucket>.s3.<region>.scw.cloud/ciburst/runner/<sha256>` and the agent from `https://<bucket>.s3.<region>.scw.cloud/ciburst/agent/<sha256>`, each with its SHA-256 checked, the unit `ciburst-vm-agent.service` (`Restart=on-failure`, `EnvironmentFile=/etc/ciburst/vm.env`) after `docker.service`.

`/etc/ciburst/vm.env`: `CIBURST_RUN_ID`, `CIBURST_VM_TOKEN`, `CIBURST_DISPATCH_URL`, `CI_SERVER_URL`, `RUNNER_TOKEN` (the host's VM runner), `CIBURST_RUNNER` (the VM runner's tag), `CIBURST_SLOTS`, `CIBURST_DEFAULT_IMAGE` (`vm.image`), `CIBURST_TIMEOUT_S`; with the S3 cache `CACHE_TYPE=s3`, `CACHE_S3_SERVER_ADDRESS`, `CACHE_S3_BUCKET_NAME`, `CACHE_S3_BUCKET_LOCATION`, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`, `CACHE_SHARED=true`. The agent exits 64 naming a missing key.

The tags are the only shared VM state; the function stays stateless.

### 6.2 Agent, states and drain

Facts from `gitlab-runner` v18.11.4 (`commands/multi.go`, `commands/builds_helper.go`, `common/consts.go`), GitLab Workhorse (`internal/builds/register.go`) and measurements on a real runner (`docs/RUNBOOK.md`, Measured): a runner receives a job only by requesting one; `run` starts `concurrent` workers and hands them its `[[runners]]` entries in turn, one entry every `check_interval` divided by the number of entries, and a worker that takes a job runs it to its end; Workhorse holds a job request that carries `last_update` for up to about 50 s until the runner's queue changes, and answers one without it at once; one `gitlab-runner run` runs at most `concurrent` jobs over all its `[[runners]]` entries and gives parallel jobs of one runner distinct concurrency ids, so their volumes stay apart; it reloads a changed `config.toml` within about 2 s; `SIGQUIT` stops further requests, lets the running jobs finish, then exits; a runner whose requests fail waits 300 s between requests, and `SIGQUIT` does not shorten that wait.

The agent (`crates/vm-agent`) runs one `gitlab-runner run --config <state dir>/config.toml` (state dir `CIBURST_STATE_DIR`, default `/var/lib/ciburst`). It writes the file with `concurrent = slots`, `check_interval = 3` and one Docker executor entry per distinct token: `vm` (the host's VM runner, `RUNNER_TOKEN`), then `member-<project id>` for each runner in the last report answer's `runners` (section 8.5). Every entry runs privileged with `vm.image` as default image, the volumes `/cache` and `/certs/client` (Docker-in-Docker with TLS needs the latter) and the S3 cache settings with `FF_USE_FASTZIP=true` and `CACHE_COMPRESSION_LEVEL=fastest` (D44). The agent rewrites the file atomically only when its content changes; the runner starts once the file exists, again after an unexpected exit, and `free` is 0 while no runner process runs. Busy slots are the distinct `com.gitlab.gitlab-runner.job.id` labels over `docker ps`, read every 5 s.

Job requests (D67): every entry's `url` is the agent's proxy, `http://<docker bridge address>:8780`, on the address of the `docker0` bridge, which the runner and its helper containers both reach (the helper uploads artifacts to the entry's `url`). The proxy forwards each request to `gitlab.url` over HTTPS with method, path, query and headers unchanged, streams bodies in both directions, and removes `last_update` from the JSON body of `POST /api/v4/jobs/request`, so GitLab answers every job request at once and one worker asks every entry about every `check_interval` (3 s). It logs one line per failed forward, never a body or a header value. The runner starts after the proxy listens. Clone and job URLs come from the job payload and do not pass the proxy.

Reports: the agent posts `{state, free, slots}` to the dispatcher's `/vm` (headers `X-Ciburst-Run` and `X-Ciburst-Token`) on every change and at the latest after the last answer's `nextReportS`, clamped to 10 to 600 s, 600 s before the first answer; a failed report is retried every 5 s. The answer is `200 {"result":"reported","nextReportS":<s>,"runners":[{"projectId":<id>,"token":<token>}]}`: `nextReportS` is 15 while VM jobs queue, 30 while the VM is busy, else 600; `runners` lists the members' own VM runners and is left out when empty. Each report also re-evaluates the queue (section 6.4).

| State | Meaning | Next |
|---|---|---|
| `starting` | created, no report accepted yet | `idle` or `busy` on the first report |
| `idle` | every slot polling, no job | `busy` when a slot takes a job; `draining` at minute 55 of a paid hour |
| `busy` | at least one slot runs a job | `idle` when all slots are free |
| `draining` | idle at minute 55, `free` 0; the runner keeps polling for a 30 s grace, then gets one `SIGQUIT` | `busy` if it takes a job during the grace; `POST /terminate` after the runner exited |

Drain protocol, which keeps every job served:

1. The VM reports `draining` first. From then on the dispatcher does not count it as capacity.
2. Its runner keeps polling for the 30 s grace, which starts once the dispatcher accepted the report. A job that went pending before step 1 was routed to this VM, and the runner takes it within about 3 s.
3. After the grace the agent sends `SIGQUIT`. A job taken in the last request still runs to its end.
4. When the runner has exited, no request can reach GitLab, so no job can be assigned to the VM. Only then does it post `/terminate`, retried for 10 minutes (2 s doubling to 60 s); the dispatcher checks the token and terminates the server.

### 6.3 Claims and burst grouping

On a pending VM job (D32, D33):

1. Claim candidates: VMs of the VM runner, not draining, `free > 0`, created more than 30 s ago (`GROUP_WINDOW_S`), and either `idle` or `busy` with a report within 21 minutes (`CAPACITY_FRESH_S`), or `starting` and created less than 10 minutes ago (`SWEEP_STARTING_S`). A VM created within the window is never a candidate; the ledger accounts for it.
2. Claim choice: among the candidates with the highest `free`, sorted by run id, the one at `fnv1a64(job id) % count`, so parallel claims of one burst spread. The answer is `claimed` with `runId`. A failed claim logs `claim_failed` and starts a VM.
3. No candidate: when a VM is live, the queue decides first (section 6.4). Otherwise the job records the intent `ciburst/intents/<vm tag>/<unix s, 10 digits>-<job id, 20 digits>` (an empty object, written and listed with the cache key), waits 2 s (`GROUP_WAIT`) and reads the intents since `now - 30`, ordered by time, then job id as a number (string order if either is not one), each job's earliest intent kept.
   - The job's earliest intent is older than `now`: a repeated delivery, `duplicate`, nothing starts.
   - Rank `r` (from 0) with `r % slots != 0`: `grouped` with the leader's job id; any polling slot of the VM runner takes the job.
   - A leader: group `g = r / slots`; `live_old` counts the VMs not draining and created before `now - 30`; `live_old + g >= maxRuns` answers `at_capacity`, which with the queue becomes `queued`; else a VM starts with `taken = min(slots, n - r)` slots already assigned (`started`), `n` the intents read.
4. A ledger error logs `ledger_failed`; the job then starts a VM with `taken = 1` unless `maxRuns` VMs are live.
5. A VM is created with `free = slots - taken`.

Known limits, each at most one VM-hour: a VM created in the 2 to 3 s after its intent left the 30 s window counts neither as old nor by rank, so a new burst at that instant can pass `maxRuns` by one; a repeated delivery within the same second is not recognised as a duplicate; an intent written after an earlier caller's 2 s wait can start one extra VM.

### 6.4 Queue first

A pending VM job without a claim candidate while a VM of the VM runner is live (D53):

1. The start rule runs with the job as the newest queued job. If it gives the job no VM, or slots of VMs in the burst window are ready for queued jobs, the job is recorded in `ciburst/intents/_queue/<vm tag>/<job>` and answers `queued` with `etaS` (the expected wait in seconds, `null` when no running job will free a slot). If it gives the job a VM and no slot is ready, the job takes the burst path of section 6.3. At `maxRuns` live VMs the job queues.
2. Tracking: a job event `running` of a VM job or a lane job writes `ciburst/intents/_running/<key>/<job>` with its start (`build_started_at`, GitLab's `2026-10-26 06:45:46 UTC` form, now when missing); `success`, `failed`, `canceled` or `skipped` remove the job from the queue and from running and, for `success` and `failed`, fold `build_duration` into the job name's typical duration in `ciburst/intents/_stats/<key>/<16 hex>` (7:3 fold, `ledger::fold_duration`). The answer is `tracked` with `status`. The key is the VM runner's tag or the lane's key.
3. The start rule (`queue::start_rule`), at time `now`:
   1. Each running VM job frees its slot after `max(0, typical(name) - (now - started))`, 0 without a typical duration; each busy slot of a live VM that is not `draining` beyond the number of running VM jobs frees at once (D68); ascending.
   2. Queued jobs oldest first, by time, then job id.
   3. The free slots of VMs in state `starting` younger than 10 minutes and of the claim candidates take the oldest queued jobs, which expect a slot at once.
   4. Each further queued job is matched to the next running job and expects a slot after that job's remainder; a job left over expects none.
   5. A queued job needs a VM when it expects no slot, or when its wait so far plus its expected wait passes `vm.maxQueueS`.
   6. `ceil(needing / slots)` VMs start, at most `maxRuns` minus the live VMs, each with every slot free and the label `job=queue:<oldest needing job>`; log `queue_started`. An evaluation that starts VMs first writes the marker `ciburst/intents/_queue/<vm tag>/.started` and starts nothing while that marker is younger than 15 s; residual race at most one VM-hour.
4. The rule runs on every pending VM job without a free slot and on every `/vm` report of a VM of the VM runner.
5. Queued entries older than 30 minutes and running entries older than 24 hours count as lost events and are removed when read.
6. A queue error logs `schedule_failed` with `op` and the job takes the burst path; it never fails an answer. Without the queue (no cache key in the function) VM jobs never queue and events other than `pending` answer `ignored`.

`vm.maxQueueS` 0 starts a VM for every job that finds no free slot.

### 6.5 Warm start and closed pipelines

The project hooks deliver pipeline events (`pipeline_events: true`). A pipeline event is checked for token and member as a job event, then by `object_attributes.status`:

- `pending` or `running`: the warm step. Builds count when their `status` is `created`, `pending`, `waiting_for_resource`, `preparing` or `scheduled`, their `when` is neither `manual` nor `on_failure`, `manual` is false, and the name is not `ciburst-*`; a counted build that the project's routing sends to the VM runner is a VM job. Without VM jobs: `ignored`, reason `no_vm_jobs`. The record `ciburst/intents/_pipelines/<pipeline>/vm-jobs.json` (`{"<vm tag>": [<job ids>]}`) is written once; an existing one answers `duplicate` with `pipeline`. Then the VM runner's action: `disabled` with `vm.warmStart` false or without records; `capacity` when a claim candidate exists, or when, after the 2 s burst wait, an intent ranks before the warm intent `<unix s>-warm-<pipeline id, 20 digits>`; `at_capacity` at `maxRuns` live VMs; else one VM starts with every slot free (`started`, `runId`), whose ledger position lets the next `slots` VM jobs of the window ride on it. The answer is `warm` with `pipeline` and `runners`, a list with one entry `{runner, kind, action, vmJobs, runId}`; log `warm`. A failed start logs `start_failed` and answers 500.
- `success`, `failed`, `canceled`, `skipped`: deletes the pipeline's records and writes `ciburst/intents/_closed/<pipeline>` with the time and the ids of its builds with status `manual` (D56); answer `closed` with `pipeline`.
- Any other status: `ignored`, reason `status`.

Closing deletes everything below the pipeline's prefix, the `seen/<job>` markers of earlier versions included. A pending job event of a closed pipeline whose job was created before the close and was not manual then answers `ignored`, reason `pipeline_closed` (D56). A records error logs `warn records_failed` with `op` and the dispatcher acts as without records. The lifecycle rule of `ciburst/intents/` expires leftovers.

### 6.6 Start path

A VM start makes three Instance calls (create, user data, power on) on the values of `CIBURST_VM_SECURITY_GROUP` and `CIBURST_VM_IMAGES` (a JSON object: commercial type to `{"image": "<id>", "installDocker": <bool>}`), which `plan` and `apply` resolve with the selection rules of section 6.1 and hand to the function (D50). A type missing from them, or a create refused because the image or the security group is gone, looks them up for that start and logs `warn start_lookup` with the reason. User data and power on are retried inside the start. The dispatcher retries a whole start once after 300 ms on a retryable error (429 or 503, for idempotent calls also another 5xx or a transport timeout) while less than 5 s have passed since the request arrived, logging `warn start_retry`; a create that timed out is not retried. Errors carry their kind (`Error::is_retryable`, set by the Scaleway client).

### 6.7 Races

| Race | Effect |
|---|---|
| Two dispatcher calls claim the same free slot | one job waits for the next free slot; never stranded, because the VM keeps polling until it is idle, and idle means no pending job for its tag |
| A job goes pending while a VM starts draining | taken in the grace, or the dispatcher already saw `draining` and started a VM; at worst one extra VM-hour |
| A VM boots for a job another VM already took | the new VM idles until minute 55 of its hour and drains; one VM-hour |
| A VM hangs or loses its network | its jobs fail on GitLab's side when the runner stops updating them; the sweep removes it |
| Pending events arrive in parallel | the ledger ranks them, so a burst starts `ceil(N / slots)` VMs up to `maxRuns` |
| Two queue evaluations at once | the start marker lets one start VMs; at most one extra VM-hour |

The races are simulated with an injectable clock in `crates/core/tests/vm_races.rs`: every pending job is served by some slot within one job duration plus one boot.

### 6.8 Sweep

The cron `<name>-sweep` posts `{"ciburst_sweep": "<sweep token>"}` every 30 minutes; its own token, because the hook token would sit readable in the cron's arguments. It ends every VM older than 24 hours, a VM that has not reported 10 minutes after its creation, and a VM whose last report is more than 30 minutes old (`ciburst_core::vm::sweep_reason`). A VM that reported within 30 minutes and is younger than 24 hours is never touched, so the sweep cannot race the scheduler or a running job: the agent reports at least every 10 minutes whatever its jobs do. The 24 hours bound every VM and every job on it. Answer `swept` with `runs`.

## 7. Lanes

### 7.1 Lane resources

Per lane `<tag>` of project P in the fleet `<name>`:

- The job definition `<name>-<P>-<tag>` (step `definition:<P>-<tag>`): `cpu_limit` (`vcpu` times 1000), `memory_limit`, `local_storage_capacity`, `image_uri` (the lane's `image`), `command` (`ciburst_core::lane::COMMAND`), `job_timeout`, description `ciburst lane <P>-<tag>`, and the environment `CIBURST_RUNNER` (the lane key `<P>-<tag>`), `CIBURST_PROJECT` (P), `CI_SERVER_URL`, `RUNNER_WAIT_TIMEOUT` (`waitTimeoutS`), `CIBURST_LANE` (base64 of the bootstrap), `CIBURST_RUNNER_URL`, `CIBURST_RUNNER_SHA256` and the cache location (`CACHE_TYPE`, `CACHE_S3_SERVER_ADDRESS`, `CACHE_S3_BUCKET_NAME`, `CACHE_S3_BUCKET_LOCATION`, `CACHE_SHARED`). A changed field updates the definition.
- One GitLab runner in P only, with the lane's tag and `run_untagged false`.

Host lanes: `ciburst-setup` (`apply`) creates, updates and deletes the host's definitions and the runners `<name>-<tag>` with the owner key; the runner ids and tokens are outputs `CIBURST_RUNNER_ID_<TAG>` and `CIBURST_RUNNER_TOKEN_<TAG>`. `apply` also disables a host lane runner that an earlier deployment enabled in another project, sets the VM runner's tags to its own tag only, deletes a runner `<name>-<vm tag>-spill` of an earlier deployment, and deletes definitions `<name>-<tag>` of earlier deployments, whose names lack a project id.

Member lanes: `ciburst-join` registers the runners `<name>-<tag>` in the member project and posts the lanes to `/join`, where the dispatcher creates, updates and deletes the member's definitions with the dispatch key (`Provider::sync_lanes`); `apply` leaves them alone. `/leave` deletes them. `destroy` deletes the Scaleway project with every definition.

Live runs and `maxRuns` count per definition, so per project and lane. ciburst sets no limit across projects; Scaleway's apply: 6 vCPU, 16 GB, 10 GB disk and 24 hours per run, 400 parallel runs per organization.

### 7.2 Lane runs

A pending lane job counts the live runs of its definition (`queued`, `scheduled`, `running`); below `maxRuns` it starts one run (`started` with `runId`), else it answers `at_capacity`. The answer's `runner` is the lane's tag, its `kind` `lane`. Lanes never use the burst ledger or the queue.

The run starts with the overrides `RUNNER_TOKEN` (the lane's runner: the host's from the function's secrets, a member's from its record), `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY` and the labels as `CIBURST_PROJECT`, `CIBURST_PIPELINE`, `CIBURST_JOB`, `CIBURST_STAGE`; never `CIBURST_VM_SECRET`. It takes jobs with the lane's tag until `waitTimeoutS` idle seconds pass, which are billed.

The shell executor runs the job in the lane's image and ignores `image:`. `ciburst check` therefore requires a lane job's `image:` to equal the lane's `image`, so the pipeline names the image the job runs in, and reports lane jobs that need a Docker daemon (section 13.2).

### 7.3 Bootstrap

The definition's `command` is `/bin/sh -c` with one argument without spaces, because Scaleway splits a job command at spaces and ignores quotes; the argument decodes `CIBURST_LANE` with `base64 -d` and evaluates it. That is `crates/core/src/lane-bootstrap.sh`, which:

1. requires `CI_SERVER_URL`, `RUNNER_TOKEN`, `CIBURST_RUNNER_URL`, `CIBURST_RUNNER_SHA256`, and with `CACHE_TYPE=s3` the cache location and key;
2. installs what the image lacks of `git`, `curl` (`wget` also serves) and CA certificates: `apk` on Alpine; `apt-get` on Debian and Ubuntu, as root with `APT::Sandbox::User=root` and its lists and archives under `/tmp/ciburst-apt`, because the sandbox drops root's capabilities; RPM-based images (RPM cannot install in the sandbox) and non-root users get no install and fail when a tool is missing;
3. downloads the runner object to `/usr/local/bin`, or to `$HOME/.ciburst/bin` when that is not writable (`/tmp/.ciburst/bin` without a writable home), and refuses it unless its SHA-256 is `CIBURST_RUNNER_SHA256`;
4. unsets `RUNNER_TOKEN`, `CACHE_S3_ACCESS_KEY` and `CACHE_S3_SECRET_KEY` and execs `run-single` with the shell executor (`bash`, else `sh`), `--builds-dir /builds` and `--cache-dir /cache` for root (`$HOME/builds` and `$HOME/cache` for another user, `/tmp/ciburst-builds` and `/tmp/ciburst-cache` without a writable home), `--max-builds 0`, `--wait-timeout <waitTimeoutS>`, `--env PATH=<the image's PATH>` (the shell executor runs jobs in a login shell, whose `/etc/profile` replaces `PATH`, which hides `cargo` in Rust images) and the S3 cache with `FF_USE_FASTZIP=true` and `CACHE_COMPRESSION_LEVEL=fastest`.

Optional: `CIBURST_BUILDS_DIR`, `CIBURST_CACHE_DIR`; `CIBURST_LANE_DRY_RUN=1` installs, skips the download and prints the command with secrets masked. Every failure the script detects prints `ciburst-lane: <reason>` to stderr and exits 64. Measured in `nl-ams` (`docs/RUNBOOK.md`, Measured): runs start in 5 to 28 s, 93 s at 6 vCPU; the bootstrap adds nothing for images with `git` and `curl`, 2 to 4 s on Alpine, about 21 s on Debian and Ubuntu slim.

## 8. Fleet

A fleet is a deployment and the projects it serves (D57). The host ran the first `ciburst-setup`; any further project, in any group or organization, joins with `ciburst-join` and never touches Scaleway.

### 8.1 State

In the cache bucket (D58):

- `ciburst/fleet/config.json`: the whole config of the host, written by `ciburst-setup` (step `fleet:config`), bound to the deployment by a MAC (HMAC-SHA256) under `CIBURST_FLEET_KEY`.
- `ciburst/fleet/members/<project id>.json`: one record per member (step `member:<project id>` for the projects of `gitlab.projectIds`): the project id, its webhook secret, the token of its VM runner by the VM runner's tag, per lane its `tag`, `image`, `jobs`, `maxRuns`, `definitionId` and runner token, its `ignoreJobs`, and the join time. The secret fields (webhook secret and runner tokens) are sealed with AES-256-GCM under `CIBURST_FLEET_KEY`, which never leaves the function and the setup; `ignoreJobs` and the lane routing stay in clear text. A record without lanes has none; tokens of the host's lanes in a record of an earlier fleet are not read.

The dispatcher reads both at once and keeps them for at most 30 s (`FLEET_CACHE`), so a change applies within 30 s without a function deploy; a failed read logs `warn fleet_failed` and keeps the last value. Without fleet state (no fleet key, no bucket) it serves the projects of its environment's `CIBURST_CONFIG`. These keys never expire; a forged config or record fails the read.

### 8.2 Join code

`crates/core/src/invite.rs`: base64url of `{"v":1,"url":<dispatcher url>,"project":<id>,"exp":<unix>}`, a `.`, and base64url of its HMAC-SHA256 under the hook token; valid 7 days. `ciburst-invite` (variable `CIBURST_INVITE`, a project id or path) and `ciburst invite <project>` print `join code for project <id>, valid until <time>; treat it as a secret:` and the code; the host's own project gets none. The member's webhook secret is `hex(HMAC-SHA256(hook token, "hook:<project id>"))`. Rotating the hook token invalidates open codes; joined members keep the secret stored with their record.

### 8.3 Endpoints

Each is authenticated by the code in the header `X-Ciburst-Join`, which must be authentic and, except for `/leave`, not expired; a forged or expired code answers `401` and logs `warn join_refused` with the reason. A code for the host answers `400 {"result":"refused","reason":"host"}`.

- `GET /fleet`: `200 {"result":"fleet","name":<fleet>,"vm":{"tag":<vm tag>,"runtime":"instances"},"hookSecret":<the member's webhook secret>}`.
- `POST /join`, body `{"runners":{<vm tag>:<token>},"lanes":[<lanes>],"laneRunners":{<lane tag>:<token>},"ignoreJobs":[<patterns>]}`. Answers: `400 bad_request` without a VM runner token; `400 unknown_runners` with `runners` for a runner tag other than the VM runner's; `400 invalid_lanes` with `errors` for lanes or `ignoreJobs` that fail the validation of section 5.4 or the provider's limits, a lane without its runner token, or a runner token without its lane; `503 no_fleet` without fleet state. Then two steps within `JOIN_BUDGET` (25 s, inside the function's 30 s): `definitions` (the member's definitions become its lanes, others deleted, `Provider::sync_lanes`) and `record` (the member record, replacing an earlier one). A step that fails answers `500 {"result":"error","step":<step>}`, one that does not finish `504 {"result":"timeout","step":<step>}`; both log `error join_failed` with the step. Success: `200 {"result":"joined","fleet":<name>,"lanes":[<tags>]}`, log `joined`.
- `POST /leave`: the steps `definitions` (deletes the member's definitions) and `record` (deletes the record), with the same failure answers (`leave_failed`); success `200 {"result":"left","fleet":<name>}`.

### 8.4 Join and leave jobs

`ciburst --ci join` (`ciburst-join`), with `CIBURST_JOIN` and `CIBURST_GITLAB_TOKEN` (an Owner's token with `api` and `create_runner` on the member project):

1. checks that the code names this project and has not expired;
2. reads `GET /fleet`;
3. reads the member's settings from `--config` or `./ciburst.json` (none: no lanes and no `ignoreJobs`) and validates them against the fleet's name, the VM runner's tag and the project id; a fleet setting is refused;
4. deletes the project's runners described `<fleet>-*` of an earlier join;
5. registers `<fleet>-<vm tag>` with the VM runner's tag and `<fleet>-<lane tag>` per lane, each `run_untagged false`;
6. creates or updates its webhook to the dispatcher URL with the member's secret, job and pipeline events;
7. posts `/join`;
8. prints `lane <tag> (<image>)` per lane and `joined fleet <name>`.

`ciburst --ci leave` (`ciburst-leave`) posts `/leave` (an expired code still leaves), then deletes the member's webhook to the dispatcher and its `<fleet>-*` runners, and prints `left fleet <name>`.

A member changes its lanes or `ignoreJobs` with a new `ciburst-join`.

### 8.5 Members at runtime

- Events: accepted when the project is a member and `X-Gitlab-Token` equals that member's secret; the host's hook token stays valid for the host.
- Routing per project (section 5.5); queue, warm start, records and the burst ledger of the VM runner are fleet-wide.
- VMs: the answer to a VM report lists in `runners` the VM runner of every member that registered its own, one entry per token; the agent writes them into its one `gitlab-runner run` (section 6.2). A member's VM job runs under its own runner on a shared VM.
- Lane runs start with the lane runner token of the job's own project.

### 8.6 Tools and migration

- `ciburst check` and the host's `ciburst-check` check the config's projects and every member that joined, each against its own settings; a member project the token cannot read prints `not readable with this token, check it in its own pipeline`.
- `ciburst status` lists the VM runner, the lanes of every member, the members and each member's deliveries (section 13.1).
- `ciburst-destroy` in the host refuses while members other than the projects of `gitlab.projectIds` exist: `fleet <name> has members <ids>: run ciburst-leave there first`.
- `gitlab.projectIds` beyond the host: each becomes a member record with the host's VM runner, which `apply` enables there, until the project joins itself; a later `ciburst-join` there replaces the record. The host's lane runners serve the host only.

### 8.7 Trust

VM jobs of all members share VMs and run privileged, so a member's job can reach another member's job data on the same VM, read the runner tokens in the VM's `config.toml`, and read the fleet-wide cache. Untrusted projects belong in separate fleets.

## 9. Scaleway implementation

### 9.1 Provider steps

Provider `scaleway` (also accepted as `scaleway-jobs`), region from the config:

| Step id | Resource | Plan and apply |
|---|---|---|
| `project` | project `<name>` | by name; an id in `SCW_DEFAULT_PROJECT_ID` (`CIBURST_SCW_PROJECT_ID`) is used instead; output `CIBURST_PROJECT_ID` |
| `key:dispatch` | IAM application `<name>-dispatch`, a policy on the project with `ServerlessJobsFullAccess`, `ContainerRegistryReadOnly` and `InstancesFullAccess`, one API key | a missing key or permission set plans `update` (repair); the secret is an output once |
| `key:cache` | IAM application `<name>-cache`, a policy with `ObjectStorageFullAccess`, one API key | |
| `bucket` | `<name>-cache-<project id>`, lifecycle rules expiring the runner cache (`project/`, `runner/`) and `ciburst/intents/` after `cache.expireDays`; `ciburst/agent/`, `ciburst/runner/` and `ciburst/fleet/` never expire | S3 with SigV4 and the owner key; drift by the rules; destroy empties it first |
| `runner-binary` | public object `ciburst/runner/<sha256>`: `gitlab-runner` 18.11.4, `RUNNER_SHA256` of `ciburst_core::cloud_init` | `apply` downloads `RUNNER_URL`, refuses another SHA-256 and uploads it public-read; `noop` when the object exists |
| `vm:agent` | public object `ciburst/agent/<sha256>`: the musl `ciburst-vm-agent` | `create` when the given binary's object is missing; without a binary the newest object is current, none is an error naming `--agent` and `scripts/agent-binary.sh`; output `CIBURST_AGENT_SHA256` from every apply call |
| `registry` | Container Registry namespace `<name>` | only with `provider.registry: true`, public with `registryPublic`; output `CIBURST_REGISTRY_ENDPOINT` |
| `vm:security-group` | security group `<name>-vm`: inbound drop, outbound accept, stateful | |
| `definition:<project id>-<tag>` | the host's lane definitions (section 7.1) | `update` when a field differs; definitions of other names of this deployment are deleted, members' kept |

`Provider::shared_env` answers the agent's URL and SHA-256, the runner's URL and the cache location, never a secret.

### 9.2 Runtime calls of lanes

Start: `POST job-definitions/<id>/start` with the overrides of section 7.2. List: `job-runs` of the definition; `queued`, `scheduled` and `running` are live, `succeeded` done, everything else failed. Stop: `POST job-runs/<id>/stop`.

### 9.3 Preflight

`apply` calls `Provider::preflight` before any change (D42): it fails when the zone has no Instance quota for any of `vm.instanceTypes` (the message names zones with quota), when the cache is not S3, or when no agent object is available. `plan` warns `vm runner cannot start` with the reason.

### 9.4 Host `scaleway-function`

API `functions/v1beta1`:

| Step id | Resource |
|---|---|
| `namespace` | namespace `<name>` |
| `function` | `<name>-dispatch`: runtime `rust196`, handler `handle`, privacy `public`, `min_scale 0`, `max_scale 5`, `memory_limit 1024`, `timeout 30s`, `http_option redirected`, `sandbox v2`. Secret environment: `HOOK_TOKEN`, `CIBURST_DISPATCH_KEY` (the dispatch secret key; Scaleway reserves the `SCW_` prefix), `RUNNER_TOKEN_<TAG>` for the VM runner and each host lane, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`, `CIBURST_VM_SECRET`, `CIBURST_SWEEP_TOKEN`, `CIBURST_FLEET_KEY`. Plain environment: `CIBURST_CONFIG` (the config JSON), `CIBURST_ORGANIZATION_ID`, `CIBURST_PROJECT_ID`, `CIBURST_REGION`, `CIBURST_AGENT_SHA256`, `CIBURST_CODE_DIGEST` (SHA-256 of the deployed zip), `CIBURST_VM_SECURITY_GROUP` and `CIBURST_VM_IMAGES` (section 6.6). The `description` `ciburst:<SHA-256 of settings and both environments, CIBURST_CONFIG as its deployment view>` decides `update`. |
| `code` | a deterministic zip packed by `crates/scaleway/build.rs`: the `ciburst-dispatch-scaleway` package (`handle` on axum 0.8) with every path dependency under `vendor/`, inherited manifest values written out, dev and build dependencies dropped, plus the workspace `Cargo.lock`; uploaded, deployed and polled until `ready` (up to 20 minutes); output `CIBURST_DISPATCH_URL` |
| `cron` | `<name>-sweep`, schedule `*/30 * * * *`, arguments `{"ciburst_sweep": "<sweep token>"}`, which the function receives as `POST /` with that JSON body |

### 9.5 GitLab steps

`crates/core/src/gitlab.rs`, by lookups of description and URL:

| Step id | Effect |
|---|---|
| `runner:<tag>` | per runner of the host (the VM runner, then each host lane): `POST /user/runners` in the host project (`project_type`, `tag_list` the runner's tag only, `run_untagged false`, description `<name>-<tag>`); `update` with `set tags <tag>` when its tags differ (`PUT /runners/:id`), `update` with delete and recreate when its token is missing or on `rotate runner`; `delete` for a runner `<name>-<x>` tagged `<x>` that the config no longer wants, and for `<name>-<vm tag>-spill` |
| `runner-enable:<project>:<tag>` | the VM runner enabled in every further project of `gitlab.projectIds`; a host lane runner enabled there is disabled |
| `hook:<project>` | per project of `gitlab.projectIds`: `POST /projects/:id/hooks` (`url` the function URL, `token`, `job_events` and `pipeline_events` true, `push_events` false, SSL verification on); `update` for a new token, missing pipeline events, or a hook that is not `executable` (`re-enable: <alert_status> until <time or never>`), which `apply` enables with `POST /projects/:id/hooks/:hook_id/test/push_events` and logs `hook_reenabled` (D51) |

`check` uses the CI lint `GET /projects/:id/ci/lint?include_jobs=true`, `status` the hook events `GET /projects/:id/hooks/:hook_id/events`; pipeline mode adds project variables and pending jobs (section 11). A 403 on `POST /user/runners` names the scope `create_runner` and the Owner role.

## 10. Security

- Access control: the dispatcher URL is public (GitLab cannot sign requests with IAM). A webhook must carry its project's secret or the hook token, a VM request the VM's capability token, the cron body the sweep token, a fleet request a valid join code; each compared in constant time; anything else gets `401` without a body.
- The dispatch key holds `ServerlessJobsFullAccess`, `InstancesFullAccess` and `ContainerRegistryReadOnly` on the dedicated project only; it creates members' lane definitions on `/join`. The dispatcher holds no GitLab token.
- A lane job can read its lane's runner token and the cache key, as on any runner: the bootstrap passes them to `run-single` as flags and unsets them before `exec`, so job scripts do not inherit them, but a job can read the runner's command line. Lane jobs run as the image's user, root included; the Serverless Jobs sandbox drops root's capabilities and offers no privileged mode. The dispatch key never enters a run; the cache key can only read and write the cache bucket.
- The agent's proxy (D67) is reachable from every job container on the VM and holds no secret: it forwards to `gitlab.url` only, with the caller's own token.
- A VM runs jobs privileged (Docker-in-Docker), so every job can read the VM's user data: the host's VM runner token, the cache key and the VM's own token, and the members' runner tokens in `config.toml`. This equals GitLab's own privileged runners; the VM token can only report for and end its own VM. Scaleway keeps the user data with the server until it is deleted; it never holds the VM secret or the dispatch key.
- Fleet (D57): the cache key is fleet-wide, so a privileged job of one member can read the others' caches on a shared VM (section 8.7). A join code lets its project join until it expires and can be replayed within that time.
- The agent and runner objects are public and hold no secret. Any job can read the cache key and overwrite them, so cloud-init and the lane bootstrap check each download against its pinned SHA-256 and stop on a mismatch.
- Intents, queue entries and records live in the cache bucket, which every job can write with the cache key. A forged intent or queue entry can at most cost one VM-hour or delay one job until a slot frees. The fleet state is bound by its MAC and sealed, so a forged config or record fails the read.
- Pipeline mode (D39): inputs and outputs are protected project variables scoped to the environment `ciburst`, masked and hidden where GitLab allows, so only the component's jobs on the protected default branch receive them; `--ci` never reads `SCW_*`, so a project's own Scaleway variables cannot redirect the setup.
- Job secrets: a job's variables, file variables such as deploy keys included, reach the VM or lane run that runs it, as on any runner; the runner deletes them after the job, and the VM ends within its hour.

Secrets inventory ("outputs" is `.local/outputs.env` on the developer path, project variables in pipeline mode):

| Secret | Where | Lifetime | Rotation |
|---|---|---|---|
| Owner Scaleway key (`IAMManager`, `ProjectManager`, `InstancesFullAccess`, `ServerlessJobsFullAccess`, `FunctionsFullAccess`, `ObjectStorageFullAccess`; `ContainerRegistryFullAccess` only with `provider.registry`) | `.local/scaleway.env`, or the variables `CIBURST_SCW_*` | owner chosen | console, then the file or the variables |
| Owner GitLab token, scopes `api` and `create_runner`, of a project Owner | `.local/gitlab.env`, or `CIBURST_GITLAB_TOKEN` in the host and in each member | expiry chosen | new token, then the file or the variable |
| Dispatch IAM key | function secret env, outputs | none | `ciburst rotate key` |
| VM secret (signs the VM tokens) | function secret env, outputs | none | `ciburst rotate vm`, refused while VMs run |
| Sweep token | function secret env, the cron's arguments, outputs | none | `ciburst rotate hook` |
| Fleet key (seals the fleet state) | function secret env, outputs | none | none |
| VM capability token | the VM's user data | the VM's life | ends with the VM |
| Cache IAM key | function secret env, VM user data, lane run env, outputs | none | `ciburst rotate cache` |
| Runner tokens `glrt-` of the host, one per runner | function secret env, VM user data, lane run env, outputs | until the runner is deleted | `ciburst rotate runner` |
| Runner tokens of a member | the member's record (sealed), VM `config.toml`, lane run env | until the member leaves or joins again | a new `ciburst-join` |
| Hook token, and the member secrets derived from it | GitLab webhooks, function secret env, outputs | none | `ciburst rotate hook` (members keep their stored secret) |
| Join code | the member's `CIBURST_JOIN` | 7 days | a new code from `ciburst-invite` |
| Registry push key (private phase) | CI variables of the framework project | none | console, then the variable |

Privacy: the dispatcher logs project id, job id, pipeline id, stage, runner tag or lane key and run id. No payload is stored.

## 11. Automation

### 11.1 Framework CI

`.gitlab-ci.yml`, GitLab hosted runners, `rust:1.96.0-bookworm`:

- `lint` (`cargo fmt --check`, `cargo clippy -D warnings`, shellcheck over every script, the pin check of Rust 1.96.0 and `gitlab-runner` 18.11.4), `deny` (`cargo deny check`), `test` (`cargo test --workspace --all-features`); the Rust jobs run only when Rust, CI or script paths change.
- `lane-bootstrap`, when `lane-bootstrap.sh` or its test change: `images/test/lane-bootstrap.sh` runs the bootstrap in Docker with `--cap-drop=ALL`. Dry runs pass on `alpine:3.21`, `debian:bookworm-slim`, `node:24-bookworm-slim` and `ubuntu:24.04`; `registry.access.redhat.com/ubi9/ubi-minimal` and `node:24-bookworm-slim` as user `node` fail with their message; `node:24-bookworm` as user `node` builds in `/home/node/builds`; a full run with the real runner against an unreachable URL exits 0 after the idle timeout; a wrong hash fails.
- Image `cli` (alpine with the musl `ciburst` and `ciburst-vm-agent`): `edge` on `main`, `X.Y.Z`, `X.Y`, `X` and `latest` on a tag `vX.Y.Z`; merge requests build and smoke-test it without pushing.
- Release on a tag `vX.Y.Z`: the Linux x86_64 musl binary through `glab`; the job fails when `ciburst --version` differs from the tag.

### 11.2 Component

`templates/ciburst.yml`, included as `component: gitlab.com/ciburst/ciburst/ciburst@<version>` (`main` while private, `1` after the first catalog release). Inputs: `vm_tag` (default `vm`, the VM runner's tag), `version` (the `cli` image tag, default `edge`), `hosted_tag` (default `saas-linux-small-amd64`), `role` (`host` or `member`, default `host`). It sets `default: tags: [<vm_tag>]` and defines `.ciburst-docker` (`docker:27` with `docker:27-dind`, `DOCKER_TLS_CERTDIR: "/certs"`, registry login, no stage). Jobs in stage `.pre`, each manual on the default branch when it is protected, `allow_failure: true`, image `registry.gitlab.com/ciburst/ciburst/cli:<version>`, `tags: [<hosted_tag>]`, `environment: {name: ciburst, action: prepare}`, `resource_group: ciburst`, `interruptible: false`:

- `role: host`: `ciburst-setup` (`ciburst --ci apply`), `ciburst-check` (`ciburst --ci check`), `ciburst-destroy` (`ciburst --ci destroy --yes "$CIBURST_DESTROY"`, the name from a project variable, because new GitLab projects refuse variables typed into a manual job), `ciburst-invite` (`ciburst --ci invite`).
- `role: member`: `ciburst-join` (`ciburst --ci join`), `ciburst-leave` (`ciburst --ci leave`).

Lanes are the project's `ciburst.json` and its jobs' tags, not part of the component.

### 11.3 Pipeline mode

`ciburst --ci`:

1. Config: the host's `ciburst.json` (or the file at `--config`) when it exists, else `{}`, merged over `Config::ci_defaults(project_id, server_url)` by top-level key (section 5.4). `join` reads the member's file instead (section 8.4).
2. Inputs from the process environment only: `CIBURST_SCW_ACCESS_KEY`, `CIBURST_SCW_SECRET_KEY`, `CIBURST_SCW_ORGANIZATION_ID`, optional `CIBURST_SCW_PROJECT_ID`, `CIBURST_GITLAB_TOKEN`; a member needs `CIBURST_JOIN` and `CIBURST_GITLAB_TOKEN` only. A missing input fails with its name, and names a workstation variable of the same meaning it finds. `SCW_*`, `GITLAB_TOKEN` and `--local` are not read; `--skip-gitlab` is refused.
3. It refuses to run unless `CI_COMMIT_REF_PROTECTED` is `true`, because protected variables would be missing.
4. Outputs are project variables of `CI_PROJECT_ID`: protected, raw, environment scope `ciburst`; secrets (tokens, secret keys, the VM secret, the fleet key) are created masked and hidden; a value GitLab refuses to mask is created unmasked and logged as `output_unmasked`. The next run reads them from its job environment, so a second setup plans only `noop`; `destroy` deletes them. An output secret the config needs that is missing (dispatch key, cache key, a runner token) adds its rotation to plan and apply.
5. The setup waits for the dispatcher's deploy.

### 11.4 Replay and own jobs

`apply` ends by posting every pending job of each project of `gitlab.projectIds` whose tags contain a tag of the host's runners and which is more than 60 s old to the dispatcher as a pending job event (`X-Gitlab-Event: Job Hook`, the hook token, `object_kind`, `build_status`, `project_id`, `build_id`, `build_name`, `build_stage`, `pipeline_id`), logging `replayed` with the answer's result per job; a failure logs `replay_failed` and `apply` still succeeds. The dispatcher answers jobs named `ciburst-*` with `ignored`, reason `own_job`, and `check` skips them.

## 12. Testing

| Level | What | Where |
|---|---|---|
| Unit | dispatch decisions (routes, status filter, own jobs, routing per project, capacity, claims, burst ledger, queue, warm start, fleet endpoints, tokens), config validation and refused keys, `check` and `--suggest`, plan diffing, outputs stores, cloud-init, the lane command, the sweep rule, invite codes, log format | `#[cfg(test)]` modules of every crate |
| Contract | `testing::contract::run_provider_contract` for one-job runtimes: plan on an empty backend lists every resource, plan after apply is empty, a changed lane updates, a removed lane deletes, rotation returns new secrets, start, list and stop, destroy empties the backend | `crates/core/tests/contract_fake.rs`, `crates/scaleway/tests` |
| VM contract | `testing::vm_contract::run_vm_provider_contract`: start with `taken`, capacity, claim, report, sweep of a silent VM, other runs ignored, stop | Scaleway Instances, the core fake |
| Races | double claims, pending during drain, bursts of 1 to 10 events, silent VMs, with an injectable clock | `crates/core/tests/vm_races.rs` |
| Fleet | two projects with the same lane tag and patterns route only to their own definitions; a job matching only another project's lane goes to the VM runner; a join with a lane change updates the definition and a removed lane deletes it; forged and expired codes; a member's events with another member's secret; a config change without a deploy | `crates/core/src/dispatch/members/tests.rs`, `crates/core/tests/fleet.rs`, `crates/cli/tests/fleet.rs`, `crates/scaleway/tests/fleet.rs` |
| API fakes | Scaleway (account, IAM, Jobs, Instances, S3, registry, Functions), GitLab (`testing::gitlab_fake::FakeGitlab`: runners and their tags, hooks with pipeline events and health, lint, variables, pending jobs, hook events), as strict as the real APIs | `src/testing` of each crate |
| CLI | plan, apply, plan, status, destroy on the fake modules; usage and config errors; pipeline mode | `crates/cli/tests` |
| Function | routes, environment errors, the ledger | `crates/dispatch-scaleway` |
| Agent | `config.toml`, busy detection, reports, drain protocol, terminate retries; the real binary with fake `gitlab-runner` and `docker` on Linux | `crates/vm-agent`, `scripts/test-linux.sh` |
| Lane bootstrap | `images/test/lane-bootstrap.sh` (section 11.1) | CI job `lane-bootstrap`, locally with Docker |
| `cli` image | smoke test of its binaries | CI image job, locally with Docker |
| Drill | real Scaleway and GitLab | runbook, Acceptance drill |

## 13. Operations

### 13.1 `ciburst status`

Prints `dispatcher: <url>`, `vm runner <tag>: <n> live of <maxRuns>`, per lane `lane <tag> of project <id> (<image>): <n> live of <maxRuns>` (the host's lanes from the config, the members' from their records), `member <id> joined <time>` per member, then up to 20 runs as `run <id> <vm tag or lane key> <state> <created_at> job=<id>` with the states `Queued`, `Running`, `Done`, `Failed`. With a GitLab token (D52) it adds per project `hook <id>: <alert_status>` and the hook's 20 newest deliveries as `delivery <time> <job, pipeline <id> or event kind> <http status> <result> <runner> <ms>`, oldest first (a warm answer as `<runner>:<action>` per runner; answers of earlier dispatchers by their `pool`), `-` where the answer lacks the key, `?` for an unreadable body; a member project the token cannot read prints `hook not readable with this token`.

### 13.2 `ciburst check`

Routes every job of each project's pipeline as the dispatcher does (section 5.5), with that project's own settings, and prints per project `every job matches its lanes and the VM runner` or one line `project <id>: job <name> (stage <stage>): <problem>` per problem, and exits 1 on any problem, a disabled webhook (`project <id>: webhook <alert_status> until <time or never>: run ciburst-setup to enable it again`) or an invalid pipeline. `--ref` lints a branch, tag or commit instead of the default branch. Problems:

- a job in `ignoreJobs` tagged for a ciburst runner: `is in ignoreJobs but tagged <tag>: no runner starts for it`;
- a job tagged only for other runners that `ignoreJobs` misses: `runs on other runners (tags <tags>) but ignoreJobs misses it: its pending events start runs`;
- tags for several ciburst runners;
- a lane job tagged for another runner, or a job tagged for a lane whose patterns do not match its name;
- an untagged job;
- for a lane job: `services:`, an image named `docker` (`docker:...` or a path ending in `/docker`), a script line starting with `docker `, or an `image:` other than the lane's after resolving `$VAR` and `${VAR}` from the pipeline's top-level `variables:` (an unresolvable name is a problem that names it).

`ciburst check --suggest` groups each project's Docker-free jobs on the VM runner by resolved image and prints per image `project <id>: lane <tag> for <image>`, a line for the project's `lanes` (`{ "tag", "image", "jobs", "vcpu": 2, "memoryMiB": 4096 }`; `tag` from the image's last path segment and major version, `-2`, `-3` when taken) and `job <name>: tags: [<tag>]` per job; a parallel or matrix job gets one pattern `<name>*` unless another job's name starts with `<name>`, and its `tags:` line once. Jobs without an image, with an unresolvable image or tagged only for other runners get no lane. Then `"ignoreJobs": [<names>]` for the jobs on other runners that `ignoreJobs` misses. It exits 0, or 1 when a pipeline cannot be read.

### 13.3 Logs and limits

- Logs: one line per action, `<level> <message> key="value"`, never a secret. Cockpit holds no function logs (2026-10-24), so the webhook's recent events (status and JSON answer per delivery) are the dispatcher's visible output.
- Failure modes and their handling: `docs/RUNBOOK.md`.
- Limits: Instance quotas per organization, zone and type; Serverless Jobs 6 vCPU, 16 GB, 10 GB disk and 24 h per run, 400 parallel runs per organization, no privileged mode, `/tmp` in memory; `linux/amd64` only; a cache archive over 5 GiB is not saved (Scaleway's S3 copy limit).
- Backups: nothing to back up; the cache is disposable; `ciburst apply` recreates every resource, and members join again.

## 14. History

| Milestone | Delivered |
|---|---|
| M0 | Workspace, core, CLI on fake modules, contract harness, CI |
| M1 | Serverless Jobs provider, Scaleway Function host and dispatcher |
| M2 | First deployment, registry step, first drill |
| M3 | VMs on Scaleway Instances with the Docker executor, the agent, drain and sweep |
| M4 | Burst grouping, two slots per VM, swap |
| M5 | The adopter path: CI/CD component, `--ci`, outputs in project variables, `cli` image, agent from the bucket, replay, own jobs, preflight |
| M6 | Lanes in the job's own image with the bootstrap and the runner object, routing by job name, `check` lane rules and `--suggest` |
| M6.1 | Warm start from pipeline events, `ignoreJobs` |
| M6.2 | Queue first with typical durations and `maxQueueS`; the late-event guard |
| M6.3 | The fleet: invite and join, fleet state in the bucket, one `gitlab-runner run` per VM for all members |
| M6.4 | Lanes and `ignoreJobs` per project, one VM runner per fleet, Scaleway as the only provider, docs of the current state |
| M7, part 1 | Hardening: `ms` in every answer, function size, start without lookups, retries, webhook health, deliveries in `status` |
| M7, next | Go public: policy files (D27), project visibility public, GitHub mirror, Pages site at `ciburst.zwa.la`, crates.io publication of `ciburst-core`, `ciburst-scaleway` and `ciburst`, `v1.0.0` tag with binaries and the `cli` image |

Definition of done per milestone: tests green locally and on CI, `cargo fmt`, `clippy -D warnings` and `cargo deny` clean, docs updated, no secret and no identifying value in the repo, no em or en dash, summary to the owner with the go question for the next milestone.

Risks:

| Risk | Mitigation |
|---|---|
| GitLab disables a webhook after four failed deliveries; GitLab.com counts an answer after 10 s as failed | D50 keeps VM starts under 6 s warm and shows `ms` in every answer; D51 re-enables a disabled webhook on the next setup, which replays waiting jobs |
| GitLab delivers job status events late | D56 starts nothing for jobs of closed pipelines; `ciburst-setup` replays jobs pending longer than 60 s |
| No Instance quota for the VM runner's types in its zone | preflight stops `apply` and names zones with quota; `provider.region` |
| Scaleway retires the runtime `rust196` | bump `rust-toolchain.toml` and the host runtime, `ciburst apply` |
| The `v1alpha1` Jobs API changes | client isolated in `crates/scaleway`, fakes pin the shapes |
| Routing and tags disagree, or a lane job needs Docker or names another image | `ciburst check` and the `ciburst-check` job; `check --suggest` prints lanes that pass |
| A lane image lacks `git` or `curl` and cannot install them (RPM-based, non-root) | the run fails with `ciburst-lane:` naming the gap; the lane takes an image that contains them |
| Free tiers or prices change | prices in the runbook; the consumption page |

## 15. Owner inputs

1. While `ciburst/ciburst` is private: each adopting or joining project's entry in its CI/CD job token allowlist; a token that can read the `ciburst` group's pipelines for drills.
2. At v1.0.0: DNS for `ciburst.zwa.la` (CNAME to GitLab Pages plus the verification TXT record) and the GitHub organization `ciburst`.

## 16. Sources, checked 2026-10-23 to 2026-10-26

Scaleway Jobs API shapes: pkg.go.dev scaleway-sdk-go api/jobs/v1alpha1. Functions API and runtimes: pkg.go.dev scaleway-sdk-go api/function/v1beta1, docs serverless-functions/reference-content/functions-runtimes (Rust 1.96 available, 1.85 end of support), the `cors-rust` example in github.com/scaleway/serverless-examples (axum 0.8 handler, source compiled on upload). Every Scaleway endpoint and field is verified against the raw SDK sources (`raw.githubusercontent.com/scaleway/scaleway-sdk-go/master/api/<api>/<version>/<api>_sdk.go`). Jobs limits: docs serverless-jobs/reference-content/jobs-limitations (6 vCPU, 16 GB, 10 GB disk, 24 h, 400 runs, capabilities dropped). Prices: scaleway.com/en/pricing/serverless, scaleway.com/en/pricing/storage. IAM permission sets: docs iam/policies-permissions/permission-sets. Instance billing: docs instances/reference-content/understanding-instance-pricing (from creation, per hour, 60 minute minimum), prices from the public product catalog API; Instance types per zone from the Instance products API. GitLab: docs runner/commands (run-single), user/project/integrations/webhook_events (job events carry stage and name, no tags; pipeline events list builds), user/project/integrations/webhooks (templates, auto-disable). Precedent: Yandex Cloud tutorial serverless-gitlab-runner.

Cost comparison (2026-10-25): docs.gitlab.com/ci/pipelines/compute_minutes and about.gitlab.com/pricing (cost factors, 10 USD per 1,000 minutes); rocketrunner.io/gitlab-runners; aws.amazon.com/codebuild/pricing and docs.aws.amazon.com/codebuild (GitLab runner); aws.amazon.com/ec2/pricing with the spot price data for eu-central-1; stackhero.io GitLab Runner pricing; cloud-runner.com/pricing; namespace.so, blacksmith.sh, buildjet.com, ubicloud.com, runs-on.com, cirrus-runners.app (GitHub Actions only); ECB reference rate of 2026-10-23.
