# Spec: ciburst, GitLab runners that exist only while jobs run

Status: M4 cost done 2026-10-24 (Hetzner drill waits for the owner's token); M5 roundup (D37 to D42, `docs/milestones/M5-roundup.md`) in progress.

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

## 1. Purpose and scope

ciburst gives GitLab projects CI runners that exist only while jobs run. A GitLab job webhook reaches a dispatcher, a Scaleway Function. For a VM pool the dispatcher gives the job a free slot on a running VM or starts a VM (Scaleway Instances by default, Hetzner Cloud optional) that runs jobs with GitLab's Docker executor and ends itself once idle at minute 55 of its paid hour. For a fast lane it starts one Scaleway Serverless Job per pending job. Nothing runs between pipelines.

v1:

1. `crates/core`: config, dispatcher, GitLab client, plan and apply engine, provider and host traits, composite provider, burst ledger, VM user data, contract harnesses, fakes.
2. `crates/scaleway` (provider `scaleway` with the runtimes `instances` and `jobs`, host `scaleway-function`), `crates/hetzner` (provider `hetzner`), `crates/dispatch-scaleway` (the function), `crates/vm-agent` (the VM supervisor).
3. Binary `ciburst`: `plan`, `apply`, `destroy`, `status`, `check`, `rotate`, pipeline mode `--ci`.
4. Images: `cli` for the setup jobs; the fast-lane images `base`, `node`, `rust`, `playwright`.
5. CI/CD component `templates/ciburst.yml`; README, runbook, provider guide.

Backlog, not v1: providers `fly-machines`, `aws-lambda-microvms` (amd64 support pending), spot VMs and providers on other clouds, remote build services; a scheduled poll of pending jobs; routing by job name patterns; a self-hosted dispatcher binary; group-level webhooks; a hosted dispatcher as a service; GitHub Actions support.

## 2. Guiding principles

- Zero fixed cost: nothing runs or is reserved between pipelines; the function and its cron stay inside Scaleway's free tier.
- Fill every paid VM hour: group bursts, reuse warm VMs, start no VM that is not needed (D32).
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
| Project owner | Change pools, add a fast lane or Hetzner | edit `ciburst.json`, run `ciburst-setup`, run `ciburst-check` |
| Developer | Run ciburst from a workstation | runbook, Developer setup: `.local/` files, `ciburst plan`, `apply`, `check` |
| Provider author | Add a cloud | implement `Provider` in a crate, pass the contract harnesses against an API fake, register it, document it (`docs/PROVIDERS.md`) |
| Operator | See what runs and what it costs | `ciburst status`, the webhook's recent events, the VM tags, the provider's consumption page |

## 5. Architecture

### 5.1 Stack

| Part | Choice | Reason |
|---|---|---|
| Language | Rust 1.96, edition 2024, Cargo workspace | one static CLI binary, typed traits, the toolchain Scaleway compiles the function with |
| Dispatcher host | Scaleway Serverless Function, runtime `rust196`, 128 MB, timeout 30 s, a cron trigger every 30 minutes | free tier of 1,000,000 requests per month, cold start well under a second |
| Compute, default | Scaleway Instances DEV1-M (3 vCPU, 4 GB, 40 GB local SSD), two slots, 4 GiB swap | a Docker daemon per VM; billed per started hour with a 60 minute minimum, about 0.03 EUR per hour |
| Compute, optional | Hetzner Cloud CX33 (4 vCPU, 8 GB, 80 GB); Scaleway Serverless Jobs fast lanes | CX33 at about half the price; fast lanes billed per second, without a Docker daemon |
| Cache | Scaleway Object Storage bucket, S3 API | shared by every run; also holds the burst intents and the agent object |
| Runner | `gitlab-runner` 18.11.4 `run-single`: Docker executor on VMs, shell executor in fast lanes | no runner manager, no registration state |
| Tests | `cargo test`, contract harnesses, in-memory API fakes behind the `HttpClient` trait | no sockets, no cloud |

### 5.2 Runtime topology

1. GitLab sends a job event (`X-Gitlab-Event: Job Hook`) to the dispatcher URL with the hook token.
2. The dispatcher verifies the token and answers `200 ignored` for anything but a pending job of a configured project, for jobs named `ciburst-*` (D41), and for a stage no pool lists when there is no default pool (D36). Otherwise the job's stage selects the pool; the event carries no tags.
3. VM pool: the dispatcher claims a free slot on a VM of the pool; without one the job joins the burst ledger, and every `slots`-th job of a burst starts a VM, up to `maxRuns` VMs (section 6.1, scheduling).
4. The VM boots; cloud-init installs `gitlab-runner` and the agent; the agent runs `slots` `run-single` processes with the Docker executor, which poll GitLab for jobs with the pool's tag. Any polling slot takes any pending job of the pool.
5. An idle VM drains at minute 55 of its paid hour and asks the dispatcher to terminate it; the cron sweep ends VMs that stop reporting.
6. Fast lane: the dispatcher counts the pool's live runs; below `maxRuns` it starts one Serverless Job, which runs `run-single` with the shell executor and exits after `waitTimeoutS` idle seconds.
7. A job whose tag does not match the pool its stage selects stays pending; `ciburst check` reports such jobs.

### 5.3 Ports

The `Provider` and `Host` traits live in `crates/core/src/types.rs`; their contracts, the registration of a provider and its tests are in `docs/PROVIDERS.md`.

Core modules: `config` (schema, defaults, validation reporting every error), `dispatch` (routes and scheduling, pure apart from provider calls), `gitlab` (runners, hooks, CI lint, variables, pending jobs), `http` (the one HTTP seam, `ReqwestClient` in production), `plan` (formatting, outputs stores), `provision` (phases: provider, runners, host, hooks, replay), `check`, `composite` (per-pool providers, D35), `ledger` (burst intents, D33), `cloud_init` (VM user data), `vm` (VM tokens, timings, the sweep rule), `time`, `store`, `log` (one line per action, never a secret), `fake`, `testing` (contract harnesses, GitLab fake).

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
| `provider` | `type` `scaleway`, `region`; optional `zone` (Instances, default `<region>-1`), `registry` and `registryPublic` (a registry namespace for fast-lane images) |
| `providers` | added providers by runtime name, for example `{"hetzner": {"location": "nbg1", "image": "docker-ce"}}` |
| `defaultPool` | pool of the stages no pool lists; without it such jobs start nothing |
| `pools[]`, VM | `tag`, `runtime` (`instances` by default, or a key of `providers`), `image` (the job image when a job names none), `stages`, `slots` (default 2), `instanceTypes` (default `DEV1-M`, `DEV1-L`; Hetzner `cx33`, `cx43`), `maxRuns` (VMs, default 10) |
| `pools[]`, fast lane | `runtime` `jobs`, `tag`, `image` (the pool image), `stages`, `vcpu` (2), `memoryMiB` (4096), `storageMiB` (10240), `timeoutS` (3600), `waitTimeoutS` (60), `maxRuns` (runs, 10) |
| `cache` | `type` `s3` (VM pools require it), `expireDays` (30) |

Validation rejects, naming each field: a tag or stage used twice, a `defaultPool` that is not a pool, a pool above its provider's capabilities, a VM pool without an S3 cache, a `providers` key from the reserved names (`docs/PROVIDERS.md`).

Credentials: on the developer path `.local/scaleway.env` (`SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID`, optional `SCW_DEFAULT_PROJECT_ID`), `.local/gitlab.env` (`GITLAB_TOKEN`, scopes `api` and `create_runner`; a `GITLAB_TOKEN` in the environment wins) and, with Hetzner pools, `.local/hetzner.env` (`HCLOUD_TOKEN`); in pipeline mode the variables of section 8.

### 5.5 Repository layout

```
Cargo.toml  rust-toolchain.toml  deny.toml  LICENSE  README.md  config.example.json
crates/core/               ciburst-core: config, dispatch, gitlab, provision, composite, ledger, cloud_init, vm, fake, testing
crates/cli/                binary ciburst; registry of providers and hosts
crates/scaleway/           provider scaleway (instances, jobs, IAM, S3, registry), host scaleway-function, ledger, build.rs packing the function zip, API fakes
crates/hetzner/            provider hetzner with an API fake
crates/dispatch-scaleway/  the function: handle, serve, respond
crates/vm-agent/           binary ciburst-vm-agent: slots, reports, drain
images/cli/                the setup image with ciburst and ciburst-vm-agent
images/base/ node/ rust/ playwright/ test/   fast-lane images, shared entrypoint, smoke tests
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
| `bucket` | `<name>-cache-<project id>`, lifecycle rules expiring `project/`, `runner/` and `ciburst/intents/` after `cache.expireDays` | S3 with SigV4 and the owner key; drift by the rules; destroy empties it first |
| `vm:agent` | public object `ciburst/agent/<sha256>` in the bucket: the musl `ciburst-vm-agent` | with a VM pool of any provider: `create` when the given binary's object is missing; without a binary the newest object is current, none is an error; output `CIBURST_AGENT_SHA256` |
| `registry` | Container Registry namespace `<name>` | only with `provider.registry: true` (fast-lane images), public with `registryPublic`; output `CIBURST_REGISTRY_ENDPOINT` |
| `vm:security-group` | security group `<name>-vm`: inbound drop, outbound accept, stateful | with a VM pool |
| `definition:<tag>` | Serverless Jobs definition `<name>-<tag>` per fast lane: `cpu_limit`, `memory_limit`, `local_storage_capacity`, `image_uri`, `job_timeout`, environment (pool, GitLab URL, idle window, cache location) | `update` when a field differs |

Runtime `instances` (VM pools):

| Call | Effect |
|---|---|
| start | `POST servers`: name `<name>-<tag>-<8 hex>`; the first type of `instanceTypes` with capacity, only types with a local root volume (terminate deletes local volumes but only detaches block volumes, which keep billing); Scaleway's marketplace image labelled `docker` in its `instance_local` variant, fallback `ubuntu_noble` with Docker installed by cloud-init; `dynamic_ip_required`, `routed_ip_enabled`, the security group; tags `ciburst`, `<name>`, `pool:<tag>`, `job:<id>`, `ciburst-state:starting`, `ciburst-free:<slots - taken>`. Then user data `cloud-init` and `poweron`. The run id is the server id. |
| cloud-init | SSH password login off, a 4 GiB swap file, `/etc/ciburst/vm.env` (mode 0600), `gitlab-runner` 18.11.4 with its sha256 checked, the agent from `https://<bucket>.s3.<region>.scw.cloud/ciburst/agent/<sha256>` with its sha256 checked, the unit `ciburst-vm-agent.service` after `docker.service` |
| list, capacity | servers tagged `<name>`; `starting` and `running` are live; the tags give state, free slots and last report |
| claim, report | rewrite the state tags |
| stop | `terminate`: deletes the local volume, releases the dynamic IP |

Runtime `jobs` (fast lanes): start `POST job-definitions/<id>/start` with the overrides `RUNNER_TOKEN`, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY` and the labels; list `job-runs` of the definition (`queued`, `scheduled`, `running` are live); stop `POST job-runs/<id>/stop`.

Preflight (D42): `apply` stops before any change when a VM pool's zone has no quota for any of its types (the message names zones with quota) or no agent object is available; `plan` only warns.

Host `scaleway-function` (API `functions/v1beta1`):

| Step id | Resource |
|---|---|
| `namespace` | namespace `<name>` |
| `function` | `<name>-dispatch`: runtime `rust196`, handler `handle`, privacy `public`, `min_scale 0`, `max_scale 5`, `memory_limit 128`, `timeout 30s`, `http_option redirected`, `sandbox v2`. Secret environment: `HOOK_TOKEN`, `CIBURST_DISPATCH_KEY` (the dispatch secret key; Scaleway reserves the `SCW_` prefix), `RUNNER_TOKEN_<TAG>` per pool, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`, `CIBURST_VM_SECRET`, `CIBURST_SWEEP_TOKEN`, the credentials of added providers (`HCLOUD_TOKEN`). Plain environment: `CIBURST_CONFIG` (config JSON), `CIBURST_ORGANIZATION_ID`, `CIBURST_PROJECT_ID`, `CIBURST_REGION`, `CIBURST_AGENT_SHA256`, `CIBURST_CODE_DIGEST` (sha256 of the deployed zip). The `description` `ciburst:<sha256 of settings and both environments>` decides `update`. |
| `code` | a deterministic zip packed by `crates/scaleway/build.rs`: the `ciburst-dispatch-scaleway` package (`handle` on axum 0.8) with every path dependency under `vendor/`, inherited manifest values written out, dev and build dependencies dropped, plus the workspace `Cargo.lock`; uploaded, deployed and polled until `ready` (up to 20 minutes); output `CIBURST_DISPATCH_URL` |
| `cron` | `<name>-sweep`, schedule `*/30 * * * *`, arguments `{"ciburst_sweep": "<sweep token>"}`, which the function receives as `POST /` with that JSON body |

GitLab steps (`crates/core/src/gitlab.rs`), per project id: one runner per pool, `POST /user/runners` (`project_type`, `tag_list` `[pool.tag]`, `run_untagged false`, description `<name>-<pool.tag>`), enabled in every further project; a webhook `POST /projects/:id/hooks` (`url` the function URL, `token`, `job_events true`, `push_events false`, SSL verification on); `check`: `GET /projects/:id/ci/lint?include_jobs=true`, then every job's stage and tags against the stage map. Lookups by description and URL. Pipeline mode adds project variables and pending jobs (section 8).

### 6.1 VM lifecycle

Facts from `gitlab-runner` v18.11.4 (`commands/single.go`, `common/consts.go`): a runner receives a job only by requesting one, about every 3 s while idle; `run-single` runs one job at a time; `SIGQUIT` stops further requests, lets the current job finish, then exits; a runner whose requests fail waits 300 s between requests, and `SIGQUIT` does not shorten that wait, so a draining VM cut off from GitLab ends minutes late while it keeps reporting.

Detection, on the VM. The agent (`crates/vm-agent`) runs `slots` `run-single` processes with the Docker executor, `--docker-privileged` and the volumes `/cache` and `/certs/client` (Docker-in-Docker with TLS needs the latter). All slots share the VM's system id, because `run-single` reads `--config` only with `--runner`; every slot therefore reports concurrent id 0, and the executor would give two jobs of one project the same volumes, `/builds` included, so each slot gets its own `--docker-cache-dir`, which turns them into host directories of that slot. Busy slots are the distinct `com.gitlab.gitlab-runner.job.id` labels over `docker ps`, read every 5 s. The agent posts `{state, free, slots}` to the dispatcher's `/vm` (headers `X-Ciburst-Run` and `X-Ciburst-Token`) on every change and every 10 minutes; the provider writes the report into the server's tags `ciburst-state:<state>`, `ciburst-free:<n>`, `ciburst-seen:<unix time>` and, once draining, `ciburst-drain:<unix time>`. The tags are the only shared state; the function stays stateless.

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

Cost: CX33 0.0144 EUR per hour with IPv4 and disk, every started hour billed; the cache and the agent object come from Scaleway over the internet.

## 7. Security

- Access control: the dispatcher URL is public (GitLab cannot sign requests with IAM). A webhook must carry the hook token, a VM request the VM's capability token, the cron body the sweep token, each compared in constant time; anything else gets `401` without a body.
- The dispatch key holds `ServerlessJobsFullAccess`, `InstancesFullAccess` and `ContainerRegistryReadOnly` on the dedicated project only. The dispatcher holds no GitLab token. With Hetzner pools it holds a Hetzner project token, which Hetzner cannot narrow further: use a Hetzner project for ciburst alone.
- A fast-lane job can read its run's environment (its pool's runner token, the cache key), as on any runner; fast-lane images run as the non-root user `runner`. The dispatch key never enters a run; the cache key can only read and write the cache bucket.
- A VM runs jobs privileged (Docker-in-Docker), so every job can read the VM's user data: its pool's runner token, the cache key and the VM's own token. This equals GitLab's own privileged runners; the VM token can only report for and end its own VM. Scaleway and Hetzner keep the user data with the server until it is deleted; it never holds the VM secret, the dispatch key or the Hetzner token.
- The agent object is public and holds no secret; cloud-init checks it against the sha256 in the user data.
- Burst intents live in the cache bucket, which every job can write with the cache key. A forged intent can at most cost one VM-hour or delay one job until a slot frees.
- Pipeline mode (D39): inputs and outputs are protected project variables scoped to the environment `ciburst`, masked and hidden where GitLab allows, so only the component's jobs on the protected default branch receive them; `--ci` never reads `SCW_*`, so a project's own Scaleway variables cannot redirect the setup.

Secrets inventory ("outputs" is `.local/outputs.env` on the developer path, project variables in pipeline mode):

| Secret | Where | Lifetime | Rotation |
|---|---|---|---|
| Owner Scaleway key (`IAMManager`, `ProjectManager`, `InstancesFullAccess`, `ServerlessJobsFullAccess`, `FunctionsFullAccess`, `ObjectStorageFullAccess`; `ContainerRegistryFullAccess` only with `provider.registry`) | `.local/scaleway.env`, or the variables `CIBURST_SCW_*` | owner chosen | console, then the file or the variables |
| Owner GitLab token, scopes `api` and `create_runner`, of a project Owner | `.local/gitlab.env`, or `CIBURST_GITLAB_TOKEN` | expiry chosen | new token, then the file or the variable |
| Dispatch IAM key | function secret env, outputs | none | `ciburst rotate key` |
| VM secret (signs the VM tokens) | function secret env, outputs | none | `ciburst rotate vm`, refused while VMs run |
| Sweep token | function secret env, the cron's arguments, outputs | none | `ciburst rotate hook` |
| VM capability token | the VM's user data | the VM's life | ends with the VM |
| Cache IAM key | function secret env, VM user data, fast-lane run env, outputs | none | `ciburst rotate cache` |
| Runner tokens `glrt-`, one per pool | function secret env, VM user data, fast-lane run env, outputs | until the runner is deleted | `ciburst rotate runner` |
| Hook token | GitLab webhook, function secret env, outputs | none | `ciburst rotate hook` |
| Registry push key (private phase) | CI variables of the framework project | none | console, then the variable |
| Hetzner project token, read and write | `.local/hetzner.env` and `.local/outputs.env`, or `CIBURST_HCLOUD_TOKEN`; function secret env | owner chosen | console, then `ciburst apply` |

Privacy: the dispatcher logs project id, job id, pipeline id, stage, pool and run id. No payload is stored.

## 8. Automation

Framework CI (`.gitlab-ci.yml`, GitLab hosted runners, `rust:1.96.0-bookworm`):

- `lint` (`cargo fmt --check`, `cargo clippy -D warnings`, shellcheck over every script, the pin check of Rust 1.96.0 and `gitlab-runner` 18.11.4), `deny` (`cargo deny check`), `test` (`cargo test --workspace --all-features`); the Rust jobs run only when Rust, CI or script paths change.
- Images: `cli` (alpine with the musl `ciburst` and `ciburst-vm-agent`) and the fast-lane images; `edge` on `main`, `X.Y.Z`, `X.Y`, `X` and `latest` on a tag `vX.Y.Z`; merge requests build and smoke-test them without pushing. While the project is private, `scripts/ci-push-image.sh` also pushes to a Scaleway registry when `CIBURST_SCW_REGISTRY` and `CIBURST_SCW_REGISTRY_KEY` are set.
- Release on a tag `vX.Y.Z`: the Linux x86_64 musl binary through `glab`; the job fails when `ciburst --version` differs from the tag.

Consumer side (D40): the CI/CD component `templates/ciburst.yml`, included as `component: gitlab.com/ciburst/ciburst/ciburst@<version>` (`main` while private, `1` after the first catalog release). Inputs: `pool` (default `vm`), `version` (the `cli` image tag, default `edge`), `hosted_tag` (default `saas-linux-small-amd64`). It sets `default: tags: [<pool>]`, defines `.ciburst-docker` (`docker:27` with `docker:27-dind`, TLS certificates, registry login, no stage) and three jobs in stage `.pre`: `ciburst-setup` (`ciburst --ci apply`), `ciburst-check` (`ciburst --ci check`) and `ciburst-destroy` (`ciburst --ci destroy --yes "$CIBURST_DESTROY"`, the name from a project variable, because new GitLab projects refuse variables typed into a manual job). Each is manual on the default branch when it is protected, `allow_failure: true`, image `registry.gitlab.com/ciburst/ciburst/cli:<version>`, `tags: [<hosted_tag>]`, `environment: {name: ciburst, action: prepare}`, `resource_group: ciburst`, `interruptible: false`. Fast lanes are config, not part of the component.

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
| Unit | dispatch decisions (routes, status filter, own jobs, stage map, default pool, capacity, claims, burst ledger, tokens), config validation, plan diffing, outputs stores, cloud-init, the sweep rule, log format | `#[cfg(test)]` modules of every crate |
| Contract | `testing::contract::run_provider_contract` for one-job providers: plan on an empty backend lists every resource, plan after apply is empty, a changed pool updates, a removed pool deletes, rotation returns new secrets, start, list and stop, destroy empties the backend | `crates/core/tests/contract_fake.rs`, `crates/scaleway/tests` |
| VM contract | `testing::vm_contract::run_vm_provider_contract`: start with `taken`, capacity, claim, report, sweep of a silent VM, other pools and runtimes ignored, stop | Scaleway Instances, Hetzner, the core fake |
| Races | double claims, pending during drain, bursts of 1 to 10 events, silent VMs, with an injectable clock | `crates/core/tests/vm_races.rs` |
| API fakes | Scaleway (account, IAM, Jobs, Instances, S3, registry, Functions), Hetzner, GitLab (`testing::gitlab_fake::FakeGitlab`: runners, hooks, lint, variables, pending jobs), as stingy as the real APIs | `src/testing` of each crate |
| CLI | plan, apply, plan, status, destroy on the fake modules; usage and config errors | `crates/cli/tests/cli.rs` |
| Function | routes, environment errors, the ledger | `crates/dispatch-scaleway` |
| Agent | slots, busy detection, reports, drain protocol, terminate retries; the real binary with fake `gitlab-runner` and `docker` on Linux | `crates/vm-agent`, `scripts/test-linux.sh` |
| Images | `images/test/smoke.sh` per image | CI image jobs, locally with Docker |
| Drill | real Scaleway, Hetzner and GitLab | runbook, Acceptance drill |

## 10. Operations

- `ciburst status` prints `dispatcher: <url>`, one line `pool <tag>: <n> live of <maxRuns>` per pool, then up to 20 runs as `run <id> <pool> <state> <created_at> job=<id>` with the states `Queued`, `Running`, `Done`, `Failed`; it needs no GitLab token.
- `ciburst check` prints per project `every job matches the stage map` or one line per job whose tags disagree with its stage's pool, and exits 1 on any problem; `--ref` lints a branch, tag or commit instead of the default branch.
- Logs: one line per action, `<level> <message> key="value"`, never a secret. Cockpit holds no function logs (2026-10-24), so the webhook's recent events (status and JSON answer per delivery) are the dispatcher's visible output; a 500 answer carries only `{"result":"error"}`. `ciburst plan` warns `vm pools cannot start` with the pool and the reason.
- Failure modes and their handling: `docs/RUNBOOK.md`.
- Limits: Instance quotas per organization, zone and type; Serverless Jobs 6 vCPU, 16 GB and 24 h per run, 400 parallel runs per organization; `linux/amd64` only.
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
| M5, roundup | `docs/milestones/M5-roundup.md`, D37 to D42: CI/CD component, `--ci`, outputs in project variables, `cli` image, agent from the cache bucket, replay, own jobs, preflight, contributor fixes, docs truth pass | the roundup's definition of done |
| M6, go public | hardening from M2 to M5, policy files (D27), project visibility public, GitHub mirror, Pages site at `ciburst.zwa.la`, crates.io publication of `ciburst-core`, `ciburst-scaleway` and `ciburst`, `v1.0.0` tag with binaries and images | release pipeline green, the images pull anonymously from the GitLab registry, the docs site answers |

Definition of done per milestone: tests green locally and on CI, `cargo fmt`, `clippy -D warnings` and `cargo deny` clean, docs updated, no secret and no identifying value in the repo, no em or en dash, summary to the owner with the go question for the next milestone.

Risks:

| Risk | Mitigation |
|---|---|
| GitLab disables a webhook after four failed deliveries | the dispatcher answers within 30 s; re-enable the webhook, `ciburst apply` re-verifies it and replays waiting jobs |
| No Instance quota for a pool's types in its zone | preflight stops `apply` and names zones with quota; `provider.zone` |
| Scaleway retires the runtime `rust196` | bump `rust-toolchain.toml` and the host runtime, `ciburst apply` |
| The `v1alpha1` Jobs API changes | client isolated in `crates/scaleway`, fakes pin the shapes |
| Stage map and tags disagree | `ciburst check` and the `ciburst-check` job |
| Free tiers or prices change | prices in the runbook; the consumption page |

Glossary: pool (tag, runtime, image, stages; one GitLab runner), run (a VM, or one Serverless Job of a fast lane), slot (one `run-single` process on a VM, one job at a time), burst intent (an empty object in the cache bucket recording a pending job that found no free slot), fast lane (a Serverless Jobs pool for jobs that need no Docker daemon), agent object (the `ciburst-vm-agent` binary in the cache bucket, named by its sha256), dispatcher (the webhook handler), host (where the dispatcher runs), provider (where runs execute), job definition (Scaleway's template for a Serverless Job).

## 14. Owner inputs

1. M5 drills: a fresh GitLab project for the adoption drill; while `ciburst/ciburst` is private, that project's entry in its CI/CD job token allowlist; a token that can read the `ciburst` group's pipelines.
2. Hetzner drill: a Hetzner Cloud account, a project used only by ciburst, an API token with read and write permission in `.local/hetzner.env` as `HCLOUD_TOKEN=...`, and the project's server limit checked under Limits (three servers suffice).
3. At v1.0.0: DNS for `ciburst.zwa.la` (CNAME to GitLab Pages plus the verification TXT record) and the GitHub organization `ciburst`.

## 15. Sources, checked 2026-10-23 and 2026-10-24

Scaleway Jobs API shapes: pkg.go.dev scaleway-sdk-go api/jobs/v1alpha1. Functions API and runtimes: pkg.go.dev scaleway-sdk-go api/function/v1beta1, docs serverless-functions/reference-content/functions-runtimes (Rust 1.96 available, 1.85 end of support), the `cors-rust` example in github.com/scaleway/serverless-examples (axum 0.8 handler, source compiled on upload). Jobs limits: docs serverless-jobs/reference-content/jobs-limitations. Prices: scaleway.com/en/pricing/serverless, scaleway.com/en/pricing/storage. IAM permission sets: docs iam/policies-permissions/permission-sets. GitLab: docs runner/commands (run-single), user/project/integrations/webhook_events (job events carry stage and name, no tags), user/project/integrations/webhooks (templates, auto-disable). Precedent: Yandex Cloud tutorial serverless-gitlab-runner.

M4, 2026-10-24: Scaleway Instance billing (docs instances/reference-content/understanding-instance-pricing: from creation, per hour, 60 minute minimum), prices from the public product catalog API; Hetzner billing FAQ (docs.hetzner.com/cloud/billing/faq: every started hour, servers bill until deleted, IPv6 free) and hetzner.com/cloud/cost-optimized (CX23 0.0096, CX33 0.0144 EUR/h with IPv4); the `docker-ce` app (docs.hetzner.com/cloud/apps/list/docker-ce).
