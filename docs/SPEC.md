# Spec: ciburst, serverless GitLab runners, Scaleway first

Status: M1 done 2026-10-24 (provider, host and function, images, CI, runbook); waiting for the owner's push confirmation and the go for M2. Background: `SEED.md`.

## 0. Decision log

| Id | Date | Decision |
|---|---|---|
| D1 | 2026-10-23 | A framework with two ports (compute provider, dispatch host) and a provider-agnostic core. Scaleway is the first implementation of both ports. |
| D2 | 2026-10-23 | One ephemeral runner process per pending GitLab job, started by a job webhook, running `gitlab-runner run-single` in a serverless container. No runner manager, no fleeting plugin, no Instances, no fixed cost. The earlier Instance designs stay in git history only. |
| D3 | 2026-10-24 | Rust, edition 2024, one Cargo workspace: `crates/core` (library with the traits), `crates/cli` (binary `ciburst`), one crate per provider and host. Replaces the Node M0 of 2026-10-23, which stays in git history. |
| D4 | 2026-10-23 | Shell executor inside the pool image. The `image:` keyword of a job is ignored; the job's tag selects the pool and therefore the image. |
| D5 | 2026-10-23 | No Docker daemon on Scaleway (capability flag `docker_daemon: false`). Image builds run in the kaniko pool; jobs that need dind keep the hosted-runner tag. |
| D6 | 2026-10-23 | Secrets live in the dispatcher's secret environment (free). Runner tokens and the cache key are passed to each run as environment overrides. |
| D7 | 2026-10-23 | Pool images are built by the framework's CI and published to the framework project's GitLab registry, public pull. |
| D8 | 2026-10-23 | Name `ciburst`. The config value `name` prefixes every cloud resource, so the code name stays stable. |
| D9 | 2026-10-23 | Cache on a Scaleway Object Storage bucket, shared between runs, objects expire after 30 days. |
| D10 | 2026-10-23 | The dispatcher never calls GitLab. It routes by the job's stage through a stage-to-pool map in config; unmapped stages go to the default pool. One runner (own token, own tag) per pool, so a job can only ever run in its pool's image. |
| D11 | 2026-10-23 | Runs use `--max-builds 0 --wait-timeout <pool.waitTimeoutS>`: a run keeps taking jobs while they arrive and exits after the idle window. A per-pool `maxRuns` caps parallel runs. |
| D12 | 2026-10-23 | Delivery: M0 by the orchestrator, M1 by three Opus agents in worktrees, an explicit go per milestone, pushes only on confirmation. |
| D13 | 2026-10-23 | License Apache-2.0. |
| D14 | 2026-10-23 | v1 pools: `ci` (node 24, default), `rust`, `playwright`, `build` (kaniko). |
| D15 | 2026-10-23 | `ciburst check` lints every consumer pipeline against the stage map (CI lint API, owner token) so a job never waits for a pool that will not take it. A scheduled sweep for lost deliveries is backlog. |
| D16 | 2026-10-23 | Region `fr-par`. Drill consumer: the drill consumer project, its id in `.local/` only. |
| D17 | 2026-10-24 | The dispatcher is a Scaleway Serverless Function on runtime `rust196`. Scaleway compiles the uploaded source, so the workspace is pinned to Rust 1.96.0 (`rust-toolchain.toml`) and CI builds with the same version. A runtime bump is a one-line change plus a redeploy. |
| D18 | 2026-10-24 | Few audited dependencies instead of none: tokio, reqwest with rustls, serde, clap, async-trait, sha2, subtle, rand, thiserror. `cargo deny` gates licenses, advisories and sources in CI. The HTTP seam is one trait, so every API client is tested against in-memory fakes. |
| D19 | 2026-10-24 | Pool images pin `gitlab/gitlab-runner:v18.11.4`; every `run-single` flag was verified against its help text. A move to 19.x follows the M2 drill. |
| D20 | 2026-10-24 | The `build` pool is the osscontainertools kaniko fork, alpine variant (ships git), with builds dir `/kaniko/builds`: kaniko wipes the filesystem between stages but never `/kaniko`. |
| D21 | 2026-10-24 | Every image writes its PATH to `/etc/profile.d/ciburst-path.sh` and removes `~/.bash_logout`, because the shell executor runs job steps in a login shell. |
| D22 | 2026-10-24 | Releases on tags `vX.Y.Z` publish the five images as `X.Y.Z`, `X.Y`, `X` and `latest` (`edge` on `main`) and the Linux x86_64 musl binary through `glab`. Configs reference images by major tag (`node:1`), or `edge` before the first release. macOS and Windows binaries are backlog. |
| D23 | 2026-10-24 | Adopters set up through their own pipeline, not from a workstation: a manual `ciburst-setup` job from the include runs the `ciburst` binary on a GitLab hosted runner (free 400 minutes per month), takes the Scaleway key and a project access token from four masked CI variables, and stores the outputs as masked project variables instead of `.local/outputs.env`. The local CLI with `.local/` stays the developer path; the owner's personal token is used only for the M2 drill. |

## 1. Purpose and scope

ciburst gives GitLab projects CI runners that exist only while a job runs. A GitLab job webhook reaches a small dispatcher; the dispatcher starts one serverless container per pending job; the container runs the job with `gitlab-runner run-single` and exits. Billing is per second, with a monthly free tier on Scaleway.

v1:

1. Core crate: config validation, dispatcher, GitLab client, plan and apply engine, provider and host traits, contract test harness, fake provider and host.
2. Crates `scaleway` (provider `scaleway-jobs`, host `scaleway-function`) and `dispatch-scaleway` (the function).
3. Pool images: `base`, `node`, `rust`, `playwright`, `kaniko`.
4. Binary `ciburst` with `plan`, `apply`, `destroy`, `status`, `check`, `rotate`.
5. Consumer include `ci/ciburst.yml`, runbook, provider authoring guide.
6. Setup pipeline for adopters: `ciburst-setup`, `ciburst-check` and `ciburst-destroy` jobs in the include, a `cli` image with the binary, outputs in project variables (M3).

Backlog, not v1: a scheduled sweep that polls pending jobs; routing by job name patterns; a self-hosted dispatcher binary; providers for other clouds; group-level webhooks; a hosted dispatcher as a service; GitHub Actions support.

## 2. Guiding principles

- Zero fixed cost: nothing runs or is reserved between jobs; the cache bucket stays inside the free tier.
- Zero ops: no host to patch, no runner to upgrade, no orphan to reap. Every run has a timeout.
- Contract first: the provider and host traits and their contract tests exist before any provider.
- Config as code: every setup with an API is a step in the idempotent plan; clicks only where no API exists.
- Few dependencies, every one of them gated by `cargo deny`; one static binary for users.
- No identifying data in the repository; brief docs; no em or en dashes.

## 3. Name

`ciburst`: free on npm, GitHub and GitLab on 2026-10-23. Rejected: `fleetless` (GitHub organization exists), `jobspark` (89 repos), `runnerless` (kept as alternative). Differentiators: the only per-job serverless runner framework for GitLab with pluggable providers; zero fixed cost; per-second billing; retries, manual gates and scheduled pipelines work because every pending job fires its own event.

## 4. Personas and journeys

| Persona | Journey | Steps |
|---|---|---|
| Project owner | Give a GitLab project serverless runners | 1 create a Scaleway API key and a project access token, 2 set four masked CI variables, 3 include `ci/ciburst.yml` and run the manual `ciburst-setup` job; tag jobs of non-default pools |
| Project owner | Add a toolchain | 1 add a pool with an image and its stages, 2 `ciburst apply`, 3 `ciburst check` |
| Provider author | Add a cloud | 1 implement the `Provider` trait in a crate, 2 pass the contract tests, 3 register the name, 4 document it |
| Operator | See what runs and what it costs | `ciburst status` and the provider's consumption page |

## 5. Architecture

### 5.1 Stack

| Part | Choice | Reason |
|---|---|---|
| Language | Rust 1.96, edition 2024, Cargo workspace | one static CLI binary, typed traits, the same toolchain Scaleway compiles the function with |
| Dispatcher host | Scaleway Serverless Function, runtime `rust196`, 128 MB, timeout 30 s | free tier of 1,000,000 requests per month, cold start well under a second |
| Compute | Scaleway Serverless Jobs | per second, 200,000 vCPU-s and 400,000 GB-s free per month, 6 vCPU, 16 GB, 24 h max |
| Cache | Scaleway Object Storage, S3 API | 75 GB storage and 75 GB egress free per month |
| Runner | `gitlab-runner run-single`, shell executor | one process, no daemon, no registration state |
| Images | official toolchain images plus the static `gitlab-runner` binary | small, reproducible |
| Tests | `cargo test`, contract harness, in-memory API fakes behind the `HttpClient` trait | no sockets, no cloud |

### 5.2 Runtime topology

1. GitLab sends a job event (`X-Gitlab-Event: Job Hook`) to the dispatcher URL with the hook token.
2. The dispatcher verifies the token, ignores every status except `pending`, maps `build_stage` to a pool through the config, falls back to the default pool, and answers `200 ignored` for projects not in the config.
3. The dispatcher counts the pool's live runs through the provider; at `maxRuns` it answers `200 at_capacity` and the live runs drain the queue. Otherwise it calls `Provider::start` with the pool, the pool's runner token, the cache settings and labels (project, pipeline, job).
4. The run's entrypoint executes `gitlab-runner run-single --url <gitlab> --token <pool token> --executor shell --builds-dir /builds --max-builds 0 --wait-timeout <s>` with the S3 cache flags; it exits after the idle window, at the run timeout, or when the provider stops it.
5. Duplicate events start spare runs, which exit after the idle window; cost is bounded by `maxRuns` and `timeoutS`.
6. A job whose tag does not match the pool chosen by its stage stays pending; `ciburst check` reports such jobs before they exist.

### 5.3 Ports (`crates/core/src/types.rs`)

| `Provider` method | Semantics |
|---|---|
| `capabilities()` | `Capabilities { name, docker_daemon, max_vcpu, max_memory_mib, max_storage_mib, max_duration_s, archs, billing }` |
| `plan(rotate)` | steps `{ id, action: Create, Update, Delete, Noop, resource, detail }`; pure; pool definitions carry the id `definition:<tag>` |
| `apply(steps)` | executes steps in order, idempotent per step, returns outputs |
| `destroy()` | removes every resource carrying the `name` prefix |
| `start(StartRequest { pool, env, labels })` | starts one run, returns its id |
| `list(pool)` | live runs `Run { run_id, pool, state, created_at, labels }` with states `Queued, Running, Done, Failed` |
| `stop(run_id)` | cancels a run |

`Host`: `plan(secrets)`, `apply(steps, secrets)`, `destroy()`, `url()` for the dispatcher deployment. The function crate adapts the platform request to `Dispatcher::handle(headers, body)`.

Core modules: `config` (schema, defaults, validation reporting every error), `dispatch` (pure apart from provider calls), `gitlab` (runners, hooks, CI lint), `http` (the one HTTP seam, `ReqwestClient` in production), `plan` (formatting, env-file outputs), `provision` (phases: provider, runners, host, hooks), `check`, `store`, `log` (one line per action, never a secret), `fake`, `testing` (contract harness, GitLab fake).

### 5.4 Configuration (`config.json`, validated at start)

```json
{
  "name": "ci",
  "gitlab": { "url": "https://gitlab.com", "projectIds": [0] },
  "host": { "type": "scaleway-function", "region": "fr-par" },
  "provider": { "type": "scaleway-jobs", "region": "fr-par" },
  "defaultPool": "ci",
  "pools": [
    { "tag": "ci", "image": "registry.gitlab.com/<namespace>/ciburst/node:1", "stages": ["lint", "test"], "vcpu": 2, "memoryMiB": 4096, "storageMiB": 10240, "timeoutS": 3600, "waitTimeoutS": 60, "maxRuns": 10 },
    { "tag": "rust", "image": "registry.gitlab.com/<namespace>/ciburst/rust:1", "stages": ["rust"], "vcpu": 4, "memoryMiB": 8192, "storageMiB": 10240, "timeoutS": 3600, "waitTimeoutS": 60, "maxRuns": 4 },
    { "tag": "playwright", "image": "registry.gitlab.com/<namespace>/ciburst/playwright:1", "stages": ["e2e"], "vcpu": 4, "memoryMiB": 8192, "storageMiB": 10240, "timeoutS": 3600, "waitTimeoutS": 60, "maxRuns": 4 },
    { "tag": "build", "image": "registry.gitlab.com/<namespace>/ciburst/kaniko:1", "stages": ["build", "image"], "vcpu": 2, "memoryMiB": 4096, "storageMiB": 10240, "timeoutS": 1800, "waitTimeoutS": 30, "maxRuns": 4 }
  ],
  "cache": { "type": "s3", "expireDays": 30 }
}
```

Secrets come from `.local/scaleway.env` (`SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID`, `SCW_DEFAULT_PROJECT_ID` optional) and `.local/gitlab.env` (`GITLAB_TOKEN`, scope `api`). Validation rejects a pool above the provider's capabilities, a tag or a stage used twice, and a `defaultPool` that is not a pool. `name` is 2 to 20 characters, so the bucket name `<name>-cache-<project id>` stays within the S3 limit of 63.

### 5.5 Repository layout

```
Cargo.toml  rust-toolchain.toml  deny.toml  LICENSE  README.md  config.example.json
crates/core/                 ciburst-core: config, dispatch, gitlab, http, plan, provision, check, store, log, types, fake, testing
crates/cli/                  binary ciburst: plan | apply | destroy | status | check | rotate; registry of providers and hosts
crates/scaleway/             provider scaleway-jobs (jobs, iam, s3, sigv4), host scaleway-function, build.rs embedding the function zip, API fakes
crates/dispatch-scaleway/    the function: `handle` and the testable `respond`
images/base/ node/ rust/ playwright/ kaniko/ cli/ test/   Dockerfiles, shared entrypoint, smoke tests; `cli` holds the binary for the setup job (M3)
ci/ciburst.yml               consumer include with the pool templates
scripts/                     ci-install-cargo-deny.sh ci-push-image.sh ci-release-binary.sh shellcheck.sh
test/config.fake.json        dry-run config
docs/                        SPEC.md RUNBOOK.md PROVIDERS.md
.gitlab-ci.yml  .local/ (gitignored)
```

## 6. Scaleway implementation

Provider `scaleway-jobs` (API `serverless-jobs/v1alpha1`, region from config):

| Step | Resource | Lookup | Create |
|---|---|---|---|
| 1 | Project `<name>` | by name | `account/v3/projects` unless `SCW_DEFAULT_PROJECT_ID` is set; output `CIBURST_PROJECT_ID` |
| 2 | IAM application `<name>-dispatch`, policy on the project with `ServerlessJobsFullAccess`, one API key | by name; an application without policy or key plans `update` (repair) | secret key written once to `.local/outputs.env` |
| 3 | IAM application `<name>-cache`, policy on the project with `ObjectStorageFullAccess`, one API key | by name | secret key written once to `.local/outputs.env` |
| 4 | Bucket `<name>-cache-<project id>` in the region, lifecycle rule `ciburst-expire` expiring objects after `cache.expireDays` | by name, drift by the rule's days | S3 API with SigV4 (access and secret key of the owner); destroy empties the bucket first |
| 5 | Job definition `<name>-<pool.tag>` per pool: `cpu_limit` = vcpu x 1000, `memory_limit`, `local_storage_capacity`, `image_uri`, `job_timeout`, `environment_variables` (`CIBURST_POOL`, `CI_SERVER_URL`, `RUNNER_WAIT_TIMEOUT`, cache address, bucket and region) | by name | create or update when any field differs |
| start | `POST job-definitions/<id>/start` with `environment_variables` override (`RUNNER_TOKEN`, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`, labels) | | |
| list | `GET job-runs?job_definition_id=<id>` states `queued, scheduled, running` map to live | | |
| stop | `POST job-runs/<id>/stop` | | |

Host `scaleway-function` (API `functions/v1beta1`):

| Step | Resource |
|---|---|
| 1 | Namespace `<name>` |
| 2 | Function `<name>-dispatch`: runtime `rust196`, handler `handle`, privacy `public`, `min_scale 0`, `max_scale 5`, `memory_limit 128`, `timeout 30s`, `http_option redirected`, `sandbox v2`; `secret_environment_variables` `HOOK_TOKEN`, `SCW_SECRET_KEY` of the dispatch key, `RUNNER_TOKEN_<POOL>` per pool, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`; `environment_variables` `CIBURST_CONFIG` (config JSON), `SCW_DEFAULT_ORGANIZATION_ID`, `SCW_DEFAULT_PROJECT_ID`, `SCW_REGION`, `CIBURST_CODE_DIGEST` (sha256 of the deployed zip); `description` `ciburst:<sha256 of settings and both environments>` decides `update` |
| 3 | Code: a deterministic zip embedded by `crates/scaleway/build.rs`; root is the `ciburst-dispatch-scaleway` package (`src/lib.rs` exporting `pub async fn handle(req: Request<Body>) -> Response<Body>` on axum 0.8, `Cargo.toml` with its own `[workspace]` table carrying the inherited package and dependency tables, dependencies pointing at `vendor/core` and `vendor/scaleway`, dev and build dependencies dropped) plus the workspace `Cargo.lock`; upload through the upload URL, record the digest, deploy, poll until `ready` (up to 20 minutes, the first build compiles the dependency set), read `domain_name` |

GitLab steps (`crates/core/src/gitlab.rs`), per project id: one runner per pool `POST /user/runners` (`project_type`, `tag_list` = `[pool.tag]`, `run_untagged false`, description `<name>-<pool.tag>`), enabled in every further project; hook `POST /projects/:id/hooks` (`url` = function URL, `token`, `job_events true`, `push_events false`, SSL verification on); `check`: `GET /projects/:id/ci/lint?include_jobs=true`, then every job's stage and tags against the stage map. Lookups by description and URL.

Cost per run: 0.00001 EUR per vCPU-second plus 0.000002 EUR per GB-second after the free tier. A 10 minute job at 2 vCPU, 4 GB costs 0.017 EUR beyond the free tier and 0 inside it. The seed workload (30 pipelines of 45 minutes) stays inside the free tier.

## 7. Security

- Access control: the dispatcher URL is public (GitLab cannot sign requests with IAM); every request must carry the hook token, compared in constant time; anything else gets `401` without a body.
- The dispatcher's IAM key can only start, list and stop Serverless Jobs in the dedicated project. The dispatcher holds no GitLab token.
- A job can read its run's environment (its pool's runner token, the cache key), as on any runner. The dispatch IAM key never enters a run. The cache key can only read and write the cache bucket.
- Images run as a non-root user where the toolchain allows it; kaniko runs as root inside its own run.

Secrets inventory:

| Secret | Where | Lifetime | Rotation |
|---|---|---|---|
| Owner Scaleway key, access and secret key (IAMManager, ProjectManager, ServerlessJobsFullAccess, FunctionsFullAccess, ObjectStorageFullAccess) | `.local/scaleway.env` | owner chosen | delete after M2 |
| Owner GitLab token, scope `api` | `.local/gitlab.env` | owner chosen | delete after M2, recreate for `check` and `rotate` |
| Dispatch IAM key | function secret env, `.local/outputs.env` | none | `ciburst rotate key` |
| Cache IAM key | function secret env, run env override, `.local/outputs.env` | none | `ciburst rotate cache` |
| Runner tokens `glrt-`, one per pool | function secret env, run env override | until the runner is deleted | `ciburst rotate runner` |
| Hook token | GitLab hook, function secret env | none | `ciburst rotate hook` |

Privacy: the dispatcher logs project id, job id, pipeline id, stage, pool and run id. No payload is stored.

## 8. Automation

Framework CI (`.gitlab-ci.yml`, hosted runners, image `rust:1.96.0-bookworm`): `lint` (`cargo fmt --check`, `cargo clippy -D warnings`, shellcheck over every script), `deny` (`cargo deny check` with a checksum-pinned binary), `test` (`cargo test --workspace --all-features`), `images` (build and push the five images with dind; on `main` when `images/**` changes and on tags), `release` (tags `vX.Y.Z` retag images, build the `ciburst` binary for Linux x86_64 musl, publish a GitLab release through `glab`). Branch mapping: `main` builds images tagged `edge`, tags build `X.Y.Z`, `X.Y`, `X` and `latest`.

Consumer side: `include: project: <namespace>/ciburst, ref: v1, file: ci/ciburst.yml` sets `default: tags: [ci]`; jobs in a non-default pool set `tags: [<pool>]`; jobs that need a Docker daemon set `tags: [saas-linux-small-amd64]`; image builds use the `build` pool and `/kaniko/executor`. `ciburst check` runs against every configured project after `apply`.

Adoption pipeline (M3): the include also carries `ciburst-setup` (manual, stage `.pre`, `tags: [saas-linux-small-amd64]`, image `registry.gitlab.com/<namespace>/ciburst/cli:1`), `ciburst-check` (automatic, same runner, reports jobs whose tags disagree with the stage map) and `ciburst-destroy` (manual). Inputs are four masked CI variables: `SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID`, `CIBURST_GITLAB_TOKEN` (a project access token, role Maintainer, scopes `api` and `create_runner`; the job token cannot create runners, hooks or variables). The config comes from `ciburst.json` in the consumer repo, or from defaults when the file is absent: `name` `ci-<project id>`, `projectIds` `[CI_PROJECT_ID]`, the four pools of `config.example.json` on the framework images. Outputs (`CIBURST_*`) are written as masked project variables through the API and read back from the job environment on the next run, so a second run plans only `noop`. `apply --no-wait` skips the readiness poll: the hook URL is known at function creation, the deploy continues on Scaleway, and the job ends in about two minutes, so a setup costs about two hosted-runner minutes. One owner step remains per adopter: creating the two tokens.

## 9. Testing

| Level | What | Where |
|---|---|---|
| Unit | dispatch decisions (status filter, stage map, default pool, capacity, token), config validation errors, plan diffing, env outputs, stores, log format | `#[cfg(test)]` modules in `crates/core` |
| Contract | every provider against `testing::contract::run_provider_contract`: plan on empty backend lists every resource, plan after apply is empty, changed pool updates, removed pool deletes, rotation returns new secrets, start then list shows the run, stop ends it, destroy empties the backend | `crates/core/tests/contract_fake.rs`; `crates/scaleway` against an in-process fake of the Jobs, IAM and S3 APIs |
| GitLab client | runners, hooks, lint and check against `testing::gitlab_fake::FakeGitlab` | `crates/core/src/gitlab.rs`, `crates/core/tests/provision.rs` |
| CLI | plan, apply, plan, status, destroy in a temp dir with the fake modules; usage and config errors | `crates/cli/tests/cli.rs` |
| Images | `images/test/smoke.sh` per image: build, `gitlab-runner --version`, the toolchain's version, the entrypoint dry run, an idle-exit run against an unreachable URL, a multi-stage kaniko build; shellcheck over every script | CI `images` stage, locally with Docker |
| Drill | real Scaleway and GitLab, section 13 M2 | manual, runbook |

## 10. Operations

- `ciburst status` prints pools, live runs and the last 20 finished runs with duration and exit code; `ciburst check` prints jobs whose tags and stage disagree.
- Logs: the function's and the runs' logs in Scaleway Cockpit; the runbook names the queries.
- Failure modes and their handling: hook disabled by GitLab after four failures (re-enable, `ciburst apply` re-verifies); image pull failure (run `failed`, job stays pending, fix the image, re-run the job); dispatcher error (GitLab shows the delivery, resend from the hook page); stage mapped to the wrong pool (job stays pending, fix config or tags, `ciburst apply`, re-run); free tier exceeded (consumption page, per-second pricing applies); Scaleway retires runtime `rust196` (bump `rust-toolchain.toml` and the host's runtime, `ciburst apply`).
- Limits: 400 parallel runs per organization, 1,000 job definitions, 6 vCPU and 16 GB per run, images under 2 GB recommended, `linux/amd64` only.
- Backups: nothing to back up; the cache is disposable; every resource is recreated by `ciburst apply`.

## 11. Attributes not applicable

No UI, so usability, offline behaviour, visual identity and tutorial do not apply. Authentication is covered by section 7. Internationalization: English only; CLI output and docs. Legal pages: none, the project is a library and a CLI.

## 12. Provider authoring (`docs/PROVIDERS.md`)

A provider is a crate implementing `ciburst_core::types::Provider` with a `capabilities()` the validator trusts. It passes the contract tests through a `ProviderFactory` against its own API fake, documents its secrets and its step table as in section 6, ships a `config.example.json` fragment, and is registered by name in `crates/cli`. The same applies to hosts with `Host`.

## 13. Implementation plan

| Milestone | Deliverables | Verification |
|---|---|---|
| M0, orchestrator, done | workspace, LICENSE, README, `crates/core`, `crates/cli` with the fake modules, contract harness, GitLab fake, CI lint, deny and test, `docs/PROVIDERS.md` | `cargo test` green, `ciburst plan` against the fake modules prints every step, second `apply` plans nothing |
| M1, three agents, done | A: `crates/scaleway` provider (jobs, IAM, bucket, lifecycle) with an in-process fake of the Jobs, IAM and S3 APIs, passing the contract. B: `crates/scaleway` host (namespace, function, secrets, zip, upload, deploy) with a Functions fake, and `crates/dispatch-scaleway` (axum handler on `Dispatcher`, builds on Rust 1.96). C: `images/*` (five images, entrypoint with cache flags verified), CI `images` and `release` stages, `ci/ciburst.yml`, `docs/RUNBOOK.md` | all tests green on CI, image smoke green, plan and apply idempotent against the fakes |
| M2, orchestrator with owner keys | drill: `ciburst apply` on an empty project, `check` on the drill consumer project, a pipeline with node, rust, playwright and kaniko jobs plus one hosted-runner job; start latency; kaniko verdict; cache hit on the second run; cost on the consumption page; API behaviour assumed by the fakes checked live (stop sets a run to `canceled`, a run's environment merges the definition's with the overrides, deleting an IAM application does not cascade to its policies and keys, a PATCH of a deployed function does not redeploy by itself); fixes | runbook section "Drill" filled with dates and numbers |
| M3, pipeline setup | `OutputsStore` on GitLab project variables (masked where allowed), `apply --no-wait`, config defaults from the job environment, `images/cli` with the binary, `ciburst-setup`, `ciburst-check` and `ciburst-destroy` jobs in the include, README "adopt in ten minutes" | a fresh consumer project is provisioned by its own pipeline from the four variables; the second run of `ciburst-setup` plans only `noop`; hosted-runner minutes per setup under three |
| M4 | hardening from M2 and M3, GitHub mirror, `v1.0.0` tag with binaries and images | release pipeline green |

Definition of done per milestone: tests green locally and on CI, `cargo fmt`, `clippy -D warnings` and `cargo deny` clean, docs updated, no secret and no identifying value in the repo, no em or en dash, summary to the owner with the go question for the next milestone.

Risks:

| Risk | Mitigation |
|---|---|
| kaniko cannot extract layers without capabilities | `build` pool documented as experimental until M2; fallback hosted runners for builds |
| Scaleway runs jobs under gVisor and counts `/tmp` against memory; kaniko's container detection may need `--force` | drill item in the runbook; fallback hosted runners for builds |
| the kaniko fork stops publishing images | pin by digest; the Chainguard fork publishes source to build from |
| run start latency too high for short jobs | measured in M2; `waitTimeoutS` keeps a run alive for consecutive jobs |
| stage map and tags disagree | `ciburst check` after every apply; runbook failure mode |
| GitLab disables a hook after failures | dispatcher answers within 30 s; manual resend documented; sweep in backlog |
| Scaleway's Rust builder is undocumented: assumed to build the zip root as a library package, resolve path dependencies under `vendor/`, build without `--locked`, on a toolchain of at least 1.96 | the first real deploy in M2 proves it; `build.rs` can emit a pruned lock if `--locked` is used |
| `v1alpha1` Jobs API changes | API client isolated in `crates/scaleway`, contract tests pin the shapes |
| Scaleway's Rust toolchain moves | `rust-toolchain.toml` and the host runtime are the two places to bump; CI proves the build on the same version |
| `run-single` cache flags differ from `run` | verified by agent C in M1 with the fake GitLab before the images ship |
| free tier changes | per-second prices in the runbook; `status` shows consumption |

Glossary: pool (tag, image, resources, stages), run (one serverless container executing `run-single`), dispatcher (the webhook handler), host (where the dispatcher runs), provider (where runs execute), job definition (Scaleway's template for a run).

## 14. Owner inputs

1. The go for M1.
2. For M2: `.local/scaleway.env` with a setup key holding `IAMManager`, `ProjectManager`, `ServerlessJobsFullAccess`, `FunctionsFullAccess`, `ObjectStorageFullAccess` on the organization; `.local/gitlab.env` with a personal access token, scope `api`, expiry chosen, and the drill consumer project id.
3. The GitLab project `<namespace>/ciburst`, public, with hosted runners enabled for its own CI; created by me through the API on your push confirmation or by you.

## 15. Sources, checked 2026-10-23 and 2026-10-24

Scaleway Jobs API shapes: pkg.go.dev scaleway-sdk-go api/jobs/v1alpha1. Functions API and runtimes: pkg.go.dev scaleway-sdk-go api/function/v1beta1, docs serverless-functions/reference-content/functions-runtimes (Rust 1.96 available, 1.85 end of support), the `cors-rust` example in github.com/scaleway/serverless-examples (axum 0.8 handler, source compiled on upload). Jobs limits: docs serverless-jobs/reference-content/jobs-limitations. Prices: scaleway.com/en/pricing/serverless, scaleway.com/en/pricing/storage. IAM permission sets: docs iam/policies-permissions/permission-sets. GitLab: docs runner/commands (run-single), user/project/integrations/webhook_events (job events carry stage and name, no tags), user/project/integrations/webhooks (templates, auto-disable). Precedent: Yandex Cloud tutorial serverless-gitlab-runner.
