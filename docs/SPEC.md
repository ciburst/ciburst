# Spec: ciburst, serverless GitLab runners, Scaleway first

Status: M0 done 2026-10-24, waiting for the owner's go for M1. Background: `SEED.md`.

## 0. Decision log

| Id  | Date       | Decision                                                                                                                                                                                                                                                                |
| --- | ---------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| D1  | 2026-10-23 | A framework with two ports (compute provider, dispatch host) and a provider-agnostic core. Scaleway is the first implementation of both ports.                                                                                                                          |
| D2  | 2026-10-23 | One ephemeral runner process per pending GitLab job, started by a job webhook, running `gitlab-runner run-single` in a serverless container. No runner manager, no fleeting plugin, no Instances, no fixed cost. The earlier Instance designs stay in git history only. |
| D3  | 2026-10-23 | Node 22+, ES modules, zero runtime dependencies, JSDoc types. Providers and hosts are modules under `providers/` and `hosts/`, selected by config.                                                                                                                      |
| D4  | 2026-10-23 | Shell executor inside the pool image. The `image:` keyword of a job is ignored; the job's tag selects the pool and therefore the image.                                                                                                                                 |
| D5  | 2026-10-23 | No Docker daemon on Scaleway (capability flag `dockerDaemon: false`). Image builds run in the kaniko pool; jobs that need dind keep the hosted-runner tag.                                                                                                              |
| D6  | 2026-10-23 | Secrets live in the dispatcher's secret environment (free). Runner tokens and the cache key are passed to each run as environment overrides.                                                                                                                            |
| D7  | 2026-10-23 | Pool images are built by the framework's CI and published to the framework project's GitLab registry, public pull.                                                                                                                                                      |
| D8  | 2026-10-23 | Name `ciburst`. The config value `name` prefixes every cloud resource, so the code name stays stable.                                                                                                                                                                   |
| D9  | 2026-10-23 | Cache on a Scaleway Object Storage bucket, shared between runs, objects expire after 30 days.                                                                                                                                                                           |
| D10 | 2026-10-23 | The dispatcher never calls GitLab. It routes by the job's stage through a stage-to-pool map in config; unmapped stages go to the default pool. One runner (own token, own tag) per pool, so a job can only ever run in its pool's image.                                |
| D11 | 2026-10-23 | Runs use `--max-builds 0 --wait-timeout <pool.waitTimeoutS>`: a run keeps taking jobs while they arrive and exits after the idle window. A per-pool `maxRuns` caps parallel runs.                                                                                       |
| D12 | 2026-10-23 | Delivery: M0 by the orchestrator, M1 by three Opus agents in worktrees, an explicit go per milestone, pushes only on confirmation.                                                                                                                                      |
| D13 | 2026-10-23 | License Apache-2.0.                                                                                                                                                                                                                                                     |
| D14 | 2026-10-23 | v1 pools: `ci` (node 24, default), `rust`, `playwright`, `build` (kaniko).                                                                                                                                                                                              |
| D15 | 2026-10-23 | `provision check` lints every consumer pipeline against the stage map (CI lint API, owner token) so a job never waits for a pool that will not take it. A scheduled sweep for lost deliveries is backlog.                                                               |
| D16 | 2026-10-23 | Region `fr-par`. Drill consumer: the drill consumer project, its id in `.local/` only.                                                                                                                                                                                         |

## 1. Purpose and scope

ciburst gives GitLab projects CI runners that exist only while a job runs. A GitLab job webhook reaches a small dispatcher; the dispatcher starts one serverless container per pending job; the container runs the job with `gitlab-runner run-single` and exits. Billing is per second, with a monthly free tier on Scaleway.

v1:

1. Core: dispatcher, config validation, GitLab client, plan and apply engine, fake provider.
2. Provider `scaleway-jobs` and host `scaleway-function`.
3. Pool images: `base`, `node`, `rust`, `playwright`, `kaniko`.
4. `bin/provision.mjs` with `plan`, `apply`, `destroy`, `status`, `check`, `rotate`.
5. Consumer include `ci/ciburst.yml`, runbook, provider authoring guide.

Backlog, not v1: a scheduled sweep that polls pending jobs; routing by job name patterns; host `node-http` for self-hosting the dispatcher; providers for other clouds; group-level webhooks; a hosted dispatcher as a service; GitHub Actions support.

## 2. Guiding principles

- Zero fixed cost: nothing runs or is reserved between jobs; the cache bucket stays inside the free tier.
- Zero ops: no host to patch, no runner to upgrade, no orphan to reap. Every run has a timeout.
- Contract first: the provider and host interfaces and their contract tests exist before any provider.
- Config as code: every setup with an API is a step in the idempotent plan; clicks only where no API exists.
- Few dependencies: none at runtime, Node's standard library only.
- No identifying data in the repository; brief docs; no em or en dashes.

## 3. Name

`ciburst`: free on npm, GitHub and GitLab on 2026-10-23. Rejected: `fleetless` (GitHub organization exists), `jobspark` (89 repos), `runnerless` (kept as alternative). Differentiators: the only per-job serverless runner framework for GitLab with pluggable providers; zero fixed cost; per-second billing; retries, manual gates and scheduled pipelines work because every pending job fires its own event.

## 4. Personas and journeys

| Persona         | Journey                                  | Steps                                                                                        |
| --------------- | ---------------------------------------- | -------------------------------------------------------------------------------------------- |
| Project owner   | Give a GitLab project serverless runners | 1 write `config.json`, 2 `provision apply`, 3 include `ci/ciburst.yml`, tag non-default jobs |
| Project owner   | Add a toolchain                          | 1 add a pool with an image and its stages, 2 `provision apply`, 3 `provision check`          |
| Provider author | Add a cloud                              | 1 implement the provider module, 2 pass the contract tests, 3 document it                    |
| Operator        | See what runs and what it costs          | `provision status` and the provider's consumption page                                       |

## 5. Architecture

### 5.1 Stack

| Part            | Choice                                                               | Reason                                                                              |
| --------------- | -------------------------------------------------------------------- | ----------------------------------------------------------------------------------- |
| Language        | Node 22+, ESM, JSDoc types                                           | runs unchanged in Scaleway Functions and locally, zero dependencies                 |
| Dispatcher host | Scaleway Serverless Function, runtime `node22`, 128 MB, timeout 30 s | free tier of 1,000,000 requests per month                                           |
| Compute         | Scaleway Serverless Jobs                                             | per second, 200,000 vCPU-s and 400,000 GB-s free per month, 6 vCPU, 16 GB, 24 h max |
| Cache           | Scaleway Object Storage, S3 API                                      | 75 GB storage and 75 GB egress free per month                                       |
| Runner          | `gitlab-runner run-single`, shell executor                           | one process, no daemon, no registration state                                       |
| Images          | official toolchain images plus the static `gitlab-runner` binary     | small, reproducible                                                                 |
| Tests           | `node --test`, shellcheck, prettier                                  | no test framework to install                                                        |

### 5.2 Runtime topology

1. GitLab sends a job event (`X-Gitlab-Event: Job Hook`) to the dispatcher URL with the hook token.
2. The dispatcher verifies the token, ignores every status except `pending`, maps `build_stage` to a pool through the config, falls back to the default pool, and answers `200 ignored` for projects not in the config.
3. The dispatcher counts the pool's live runs through the provider; at `maxRuns` it answers `200 at_capacity` and the live runs drain the queue. Otherwise it calls `provider.start` with the pool, the pool's runner token, the cache settings and labels (project, pipeline, job).
4. The run's entrypoint executes `gitlab-runner run-single --url <gitlab> --token <pool token> --executor shell --builds-dir /builds --max-builds 0 --wait-timeout <s>` with the S3 cache flags; it exits after the idle window, at the run timeout, or when the provider stops it.
5. Duplicate events start spare runs, which exit after the idle window; cost is bounded by `maxRuns` and `timeoutS`.
6. A job whose tag does not match the pool chosen by its stage stays pending; `provision check` reports such jobs before they exist.

### 5.3 Ports

Provider (`providers/<name>/provider.mjs`, default export `createProvider(config, deps)`):

| Method                         | Semantics                                                                                         |
| ------------------------------ | ------------------------------------------------------------------------------------------------- |
| `capabilities()`               | `{ dockerDaemon, maxVcpu, maxMemoryMiB, maxStorageMiB, maxDurationS, archs, billing }`            |
| `plan(desired, state)`         | list of steps `{ id, action: create, update, delete, noop, resource, detail }`; pure              |
| `apply(steps)`                 | executes steps in order, idempotent per step, returns outputs                                     |
| `destroy()`                    | removes every resource carrying the `name` prefix                                                 |
| `start({ pool, env, labels })` | starts one run, returns `{ runId }`                                                               |
| `list({ pool })`               | live runs `{ runId, pool, state, createdAt, labels }` with states `queued, running, done, failed` |
| `stop(runId)`                  | cancels a run                                                                                     |

Host (`hosts/<name>/host.mjs`): `plan`, `apply`, `destroy` for the dispatcher deployment and `url()`; plus `handler.mjs`, the platform entry that wires `core/dispatch.mjs` to the platform's event shape.

Core (`core/`): `dispatch.mjs` (pure, injected `provider`, `config`, `secrets`, `log`), `config.mjs` (schema, defaults, validation at start, fails with every error listed), `gitlab.mjs` (runners, hooks, CI lint), `plan.mjs` (plan printing, apply loop, `.local/outputs.env` writer), `log.mjs` (one line per action, never a secret).

### 5.4 Configuration (`config.json`, validated at start)

```json
{
  "name": "ci",
  "gitlab": { "url": "https://gitlab.com", "projectIds": [0] },
  "host": { "type": "scaleway-function", "region": "fr-par" },
  "provider": { "type": "scaleway-jobs", "region": "fr-par" },
  "defaultPool": "ci",
  "pools": [
    {
      "tag": "ci",
      "image": "registry.gitlab.com/<namespace>/ciburst/node:24",
      "stages": ["lint", "test"],
      "vcpu": 2,
      "memoryMiB": 4096,
      "storageMiB": 10240,
      "timeoutS": 3600,
      "waitTimeoutS": 60,
      "maxRuns": 10
    },
    {
      "tag": "rust",
      "image": "registry.gitlab.com/<namespace>/ciburst/rust:1",
      "stages": ["rust"],
      "vcpu": 4,
      "memoryMiB": 8192,
      "storageMiB": 10240,
      "timeoutS": 3600,
      "waitTimeoutS": 60,
      "maxRuns": 4
    },
    {
      "tag": "playwright",
      "image": "registry.gitlab.com/<namespace>/ciburst/playwright:1",
      "stages": ["e2e"],
      "vcpu": 4,
      "memoryMiB": 8192,
      "storageMiB": 10240,
      "timeoutS": 3600,
      "waitTimeoutS": 60,
      "maxRuns": 4
    },
    {
      "tag": "build",
      "image": "registry.gitlab.com/<namespace>/ciburst/kaniko:1",
      "stages": ["build", "image"],
      "vcpu": 2,
      "memoryMiB": 4096,
      "storageMiB": 10240,
      "timeoutS": 1800,
      "waitTimeoutS": 30,
      "maxRuns": 4
    }
  ],
  "cache": { "type": "s3", "expireDays": 30 }
}
```

Secrets come from `.local/scaleway.env` (`SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID`, `SCW_DEFAULT_PROJECT_ID` optional) and `.local/gitlab.env` (`GITLAB_TOKEN`, scope `api`). The validator rejects a pool that exceeds the provider's capabilities, a tag or a stage used twice, and a `defaultPool` that is not a pool.

### 5.5 Repository layout

```
bin/provision.mjs            plan | apply | destroy | status | check | rotate <runner|hook|key|cache>
core/                        dispatch.mjs config.mjs gitlab.mjs plan.mjs log.mjs types.mjs
providers/fake/              in-memory provider used by tests
providers/scaleway-jobs/     provider.mjs api.mjs
hosts/scaleway-function/     host.mjs handler.mjs package.json
images/base/                 Dockerfile entrypoint.sh
images/node/ images/rust/ images/playwright/ images/kaniko/   Dockerfile
ci/ciburst.yml               consumer include
test/                        *.test.mjs  contract/provider.contract.mjs  fakes/scaleway-api.mjs  fakes/gitlab-api.mjs
docs/                        SPEC.md RUNBOOK.md PROVIDERS.md
config.example.json  .gitlab-ci.yml  LICENSE  README.md  .local/ (gitignored)
```

## 6. Scaleway implementation

Provider `scaleway-jobs` (API `serverless-jobs/v1alpha1`, region from config):

| Step  | Resource                                                                                                                                                                                                                                                         | Lookup  | Create                                                       |
| ----- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------- | ------------------------------------------------------------ |
| 1     | Project `<name>`                                                                                                                                                                                                                                                 | by name | `account/v3/projects` unless `SCW_DEFAULT_PROJECT_ID` is set |
| 2     | IAM application `<name>-dispatch`, policy on the project with `ServerlessJobsFullAccess`, one API key                                                                                                                                                            | by name | secret key written once to `.local/outputs.env`              |
| 3     | IAM application `<name>-cache`, policy on the project with `ObjectStorageFullAccess`, one API key                                                                                                                                                                | by name | secret key written once to `.local/outputs.env`              |
| 4     | Bucket `<name>-cache-<project id>` in the region, lifecycle rule expiring objects after `cache.expireDays`                                                                                                                                                       | by name | S3 API with the owner key                                    |
| 5     | Job definition `<name>-<pool.tag>` per pool: `cpu_limit` = vcpu x 1000, `memory_limit`, `local_storage_capacity`, `image_uri`, `job_timeout`, `environment_variables` (`CIBURST_POOL`, `CI_SERVER_URL`, `RUNNER_WAIT_TIMEOUT`, cache address, bucket and region) | by name | create or update when any field differs                      |
| start | `POST job-definitions/<id>/start` with `environment_variables` override (`RUNNER_TOKEN`, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`, labels)                                                                                                                   |         |                                                              |
| list  | `GET job-runs?job_definition_id=<id>` states `queued, scheduled, running` map to live                                                                                                                                                                            |         |                                                              |
| stop  | `POST job-runs/<id>/stop`                                                                                                                                                                                                                                        |         |                                                              |

Host `scaleway-function` (API `functions/v1beta1`):

| Step | Resource                                                                                                                                                                                                                                                                                                                                                                         |
| ---- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1    | Namespace `<name>`                                                                                                                                                                                                                                                                                                                                                               |
| 2    | Function `<name>-dispatch`: runtime `node22`, handler `handler.handle`, privacy `public`, `min_scale 0`, `max_scale 5`, `memory_limit 128`, `timeout 30s`, `secret_environment_variables` (`HOOK_TOKEN`, `SCW_SECRET_KEY` of the dispatch key, `RUNNER_TOKEN_<POOL>` per pool, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`), `environment_variables` (`CIBURST_CONFIG` as JSON) |
| 3    | Code: zip of `core/`, `providers/scaleway-jobs/`, `hosts/scaleway-function/handler.mjs`, `package.json` with `"type": "module"`; upload through the upload URL, then deploy, wait for `ready`, read `domain_name`                                                                                                                                                                |

GitLab steps (`core/gitlab.mjs`), per project id: one runner per pool `POST /user/runners` (`project_type`, `tag_list` = `[pool.tag]`, `run_untagged false`, description `<name>-<pool.tag>`); hook `POST /projects/:id/hooks` (`url` = function URL, `token`, `job_events true`, `push_events false`, SSL verification on); `check`: `POST /projects/:id/ci/lint` with `include_jobs`, then every job's stage and tags against the stage map. Lookups by description and URL.

Cost per run: 0.00001 EUR per vCPU-second plus 0.000002 EUR per GB-second after the free tier. A 10 minute job at 2 vCPU, 4 GB costs 0.017 EUR beyond the free tier and 0 inside it. The seed workload (30 pipelines of 45 minutes) stays inside the free tier.

## 7. Security

- Access control: the dispatcher URL is public (GitLab cannot sign requests with IAM); every request must carry the hook token, compared in constant time; anything else gets `401` without a body.
- The dispatcher's IAM key can only start, list and stop Serverless Jobs in the dedicated project. The dispatcher holds no GitLab token.
- A job can read its run's environment (its pool's runner token, the cache key), as on any runner. The dispatch IAM key never enters a run. The cache key can only read and write the cache bucket.
- Images run as a non-root user where the toolchain allows it; kaniko runs as root inside its own run.

Secrets inventory:

| Secret                                                                                                                  | Where                                                       | Lifetime                    | Rotation                                           |
| ----------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------- | --------------------------- | -------------------------------------------------- |
| Owner Scaleway key (IAMManager, ProjectManager, ServerlessJobsFullAccess, FunctionsFullAccess, ObjectStorageFullAccess) | `.local/scaleway.env`                                       | owner chosen                | delete after M2                                    |
| Owner GitLab token, scope `api`                                                                                         | `.local/gitlab.env`                                         | owner chosen                | delete after M2, recreate for `check` and `rotate` |
| Dispatch IAM key                                                                                                        | function secret env, `.local/outputs.env`                   | none                        | `provision rotate key`                             |
| Cache IAM key                                                                                                           | function secret env, run env override, `.local/outputs.env` | none                        | `provision rotate cache`                           |
| Runner tokens `glrt-`, one per pool                                                                                     | function secret env, run env override                       | until the runner is deleted | `provision rotate runner`                          |
| Hook token                                                                                                              | GitLab hook, function secret env                            | none                        | `provision rotate hook`                            |

Privacy: the dispatcher logs project id, job id, pipeline id, stage, pool and run id. No payload is stored.

## 8. Automation

Framework CI (`.gitlab-ci.yml`, hosted runners): `lint` (prettier check, shellcheck, `node --check`), `test` (`node --test`), `images` (build and push the five images with dind; on `main` when `images/**` changes and on tags), `release` (tags `vX.Y.Z` retag images, publish a release note). Branch mapping: `main` builds images tagged `edge`, tags build `X.Y.Z`, `X.Y`, `X` and `latest`.

Consumer side: `include: project: <namespace>/ciburst, ref: v1, file: ci/ciburst.yml` sets `default: tags: [ci]`; jobs in a non-default pool set `tags: [<pool>]`; jobs that need a Docker daemon set `tags: [saas-linux-small-amd64]`; image builds use the `build` pool and `/kaniko/executor`. `provision check` runs against every configured project after `apply`.

## 9. Testing

| Level         | What                                                                                                                                                                                                      | Where                                                                                                                                                  |
| ------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Unit          | dispatch decisions (status filter, stage map, default pool, capacity, token), config validation errors, plan diffing, outputs writer                                                                      | `test/*.test.mjs` with the fake provider                                                                                                               |
| Contract      | every provider against `test/contract/provider.contract.mjs`: plan on empty state lists every resource, plan after apply is empty, start then list shows the run, stop ends it, destroy empties the state | fake provider in memory, Scaleway provider against `test/fakes/scaleway-api.mjs` (an in-process HTTP server with the real request and response shapes) |
| GitLab client | runners, hook, lint and check against `test/fakes/gitlab-api.mjs`                                                                                                                                         | `test/gitlab.test.mjs`                                                                                                                                 |
| Images        | build, `gitlab-runner --version`, entrypoint with a fake GitLab that serves one job and expects the script output                                                                                         | CI `images` stage                                                                                                                                      |
| Drill         | real Scaleway and GitLab, section 13 M2                                                                                                                                                                   | manual, runbook                                                                                                                                        |

Coverage target: every branch of `dispatch.mjs` and `config.mjs`; measured with `node --test --experimental-test-coverage`.

## 10. Operations

- `provision status` prints pools, live runs and the last 20 finished runs with duration and exit code; `provision check` prints jobs whose tags and stage disagree.
- Logs: the function's and the runs' logs in Scaleway Cockpit; the runbook names the queries.
- Failure modes and their handling: hook disabled by GitLab after four failures (re-enable, `provision apply` re-verifies); image pull failure (run `failed`, job stays pending, fix the image, re-run the job); dispatcher error (GitLab shows the delivery, resend from the hook page); stage mapped to the wrong pool (job stays pending, fix config or tags, `provision apply`, re-run); free tier exceeded (consumption page, per-second pricing applies).
- Limits: 400 parallel runs per organization, 1,000 job definitions, 6 vCPU and 16 GB per run, images under 2 GB recommended, `linux/amd64` only.
- Backups: nothing to back up; the cache is disposable; every resource is recreated by `provision apply`.

## 11. Attributes not applicable

No UI, so usability, offline behaviour, visual identity and tutorial do not apply. Authentication is covered by section 7. Internationalization: English only; CLI output and docs. Legal pages: none, the project is a library and a CLI.

## 12. Provider authoring (`docs/PROVIDERS.md`)

A provider is one ESM module exporting `createProvider(config, deps)` with the methods of section 5.3 and a `capabilities()` that the validator trusts. It passes the contract tests against its own fake API, documents its secrets and its step table as in section 6, and ships a `config.example.json` fragment. The same applies to hosts with `createHost`.

## 13. Implementation plan

| Milestone                        | Deliverables                                                                                                                                                                                                                                                                                                                                                                                                                                                               | Verification                                                                                                    |
| -------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| M0, orchestrator                 | repo init, LICENSE, README skeleton, `core/*`, `providers/fake`, config schema, contract test suite, `bin/provision.mjs` wired to the fake provider, `.gitlab-ci.yml` lint and test, `docs/PROVIDERS.md`                                                                                                                                                                                                                                                                   | `node --test` green, `provision plan` against the fake provider prints every step, second `apply` plans nothing |
| M1, three agents                 | A: `providers/scaleway-jobs` (jobs, IAM, bucket, lifecycle) plus `test/fakes/scaleway-api.mjs` and contract tests. B: `hosts/scaleway-function` (namespace, function, secrets, zip upload, deploy, handler) plus `test/fakes/scaleway-functions-api.mjs` and a host test; `core/gitlab.mjs`, `check` and `rotate` exist since M0. C: `images/*` (five images, entrypoint with cache flags verified), CI `images` and `release` stages, `ci/ciburst.yml`, `docs/RUNBOOK.md` | all tests green on CI, image smoke green, plan and apply idempotent against the fakes                           |
| M2, orchestrator with owner keys | drill: `provision apply` on an empty project, `check` on the drill consumer project, a pipeline with node, rust, playwright and kaniko jobs plus one hosted-runner job; start latency; kaniko verdict; cache hit on the second run; cost on the consumption page; fixes                                                                                                                                                                                                           | runbook section "Drill" filled with dates and numbers                                                           |
| M3                               | hardening from M2, GitHub mirror, `v1.0.0` tag                                                                                                                                                                                                                                                                                                                                                                                                                             | release pipeline green                                                                                          |

Definition of done per milestone: tests green locally and on CI, docs updated, no secret and no identifying value in the repo, prettier and shellcheck clean, no em or en dash, summary to the owner with the go question for the next milestone.

Risks:

| Risk                                              | Mitigation                                                                              |
| ------------------------------------------------- | --------------------------------------------------------------------------------------- |
| kaniko cannot extract layers without capabilities | `build` pool documented as experimental until M2; fallback hosted runners for builds    |
| run start latency too high for short jobs         | measured in M2; `waitTimeoutS` keeps a run alive for consecutive jobs                   |
| stage map and tags disagree                       | `provision check` after every apply; runbook failure mode                               |
| GitLab disables a hook after failures             | dispatcher answers within 30 s; manual resend documented; sweep in backlog              |
| `v1alpha1` Jobs API changes                       | API client isolated in `providers/scaleway-jobs/api.mjs`, contract tests pin the shapes |
| `run-single` cache flags differ from `run`        | verified by agent C in M1 with the fake GitLab before the images ship                   |
| free tier changes                                 | per-second prices in the runbook; `status` shows consumption                            |

Glossary: pool (tag, image, resources, stages), run (one serverless container executing `run-single`), dispatcher (the webhook handler), host (where the dispatcher runs), provider (where runs execute), job definition (Scaleway's template for a run).

## 14. Owner inputs

1. The go for M0.
2. For M2: `.local/scaleway.env` with a setup key holding `IAMManager`, `ProjectManager`, `ServerlessJobsFullAccess`, `FunctionsFullAccess`, `ObjectStorageFullAccess` on the organization; `.local/gitlab.env` with a personal access token, scope `api`, expiry chosen, and the drill consumer project id.
3. The GitLab project `<namespace>/ciburst`, public, with hosted runners enabled for its own CI; created by me through the API on your push confirmation or by you.

## 15. Sources, checked 2026-10-23

Scaleway Jobs API shapes: pkg.go.dev scaleway-sdk-go api/jobs/v1alpha1. Functions API: pkg.go.dev scaleway-sdk-go api/function/v1beta1 and docs serverless-functions/reference-content/code-examples. Jobs limits: docs serverless-jobs/reference-content/jobs-limitations. Prices: scaleway.com/en/pricing/serverless, scaleway.com/en/pricing/storage. IAM permission sets: docs iam/policies-permissions/permission-sets. GitLab: docs runner/commands (run-single), user/project/integrations/webhook_events (job events carry stage and name, no tags), user/project/integrations/webhooks (templates, auto-disable). Precedent: Yandex Cloud tutorial serverless-gitlab-runner.
