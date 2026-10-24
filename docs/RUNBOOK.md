# Runbook

## Facts

| Item | Value |
|---|---|
| Region | `fr-par` (provider and host) |
| Resources | project `<name>`, IAM applications `<name>-dispatch` and `<name>-cache`, bucket `<name>-cache-<project id>`, job definitions `<name>-<tag>`, namespace `<name>`, function `<name>-dispatch` |
| Pool images | `registry.gitlab.com/<namespace>/ciburst/{base,node,rust,playwright,kaniko}` |
| Image tags | `edge` from the default branch; `X.Y.Z`, `X.Y`, `X`, `latest` from a tag `vX.Y.Z` |
| Binary | `ciburst-x86_64-unknown-linux-musl` in the framework project's generic package registry, linked from each release |
| Runner | `gitlab-runner` 18.11.4, `run-single`, shell executor, one run per pending job, exits after `waitTimeoutS` idle seconds |
| Run limits | 6 vCPU, 16 GB, 24 h per run, 400 parallel runs per organization, `linux/amd64`, images under 2 GB recommended; `/tmp` counts against the run's memory |

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
2. Jobs in `lint` and `test` need nothing: `default: tags: [ci]`.
3. Jobs of another pool extend `.ciburst-rust` (stage `rust`), `.ciburst-playwright` (stage `e2e`) or `.ciburst-build` (stage `build`, registry auth for kaniko). Declare these stages in `stages:`.
4. Jobs that need a Docker daemon set `tags: [saas-linux-small-amd64]` and keep `image:` and `services:`.
5. The `image:` keyword is ignored in every pool; the playwright pool ships browsers for Playwright 1.64.1, so pin `@playwright/test` to 1.64.1.
Check: `ciburst check` reports no problem; a pipeline shows each job on a runner named `<name>-<tag>`.

## Pool images

| Image | From | Toolchain | User |
|---|---|---|---|
| `base` | `debian:bookworm-20261020-slim` | git, git-lfs, curl, bash | `runner` |
| `node` | `node:24.22.1-bookworm-slim` | Node 24 | `runner` |
| `rust` | `rust:1.96.0-bookworm` | Rust 1.96.0, rustfmt, clippy | `runner` |
| `playwright` | `mcr.microsoft.com/playwright:v1.64.1-noble` | Node, Playwright 1.64.1 browsers | `runner` |
| `kaniko` | `ghcr.io/osscontainertools/kaniko:v1.28.6-alpine` | `/kaniko/executor`, git, bash | `root` |

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

## Rotation and updates

1. Secrets: `ciburst rotate runner`, `hook`, `key` or `cache`.
   Check: `ciburst plan` prints only `noop`; a test pipeline passes.
2. Yearly runtime bump: set the new Rust version in `rust-toolchain.toml`, the host runtime in `crates/scaleway`, `.gitlab-ci.yml` and `images/rust/Dockerfile`; tag a release; `ciburst apply`.
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
3. Pipeline with a node, a rust, a playwright and a kaniko job plus one `saas-linux-small-amd64` job.
   Result: ____
4. Start latency.
   Dispatcher answer: 0.1 to 0.4 s warm. Run queued to running: 14 s (`ci`, node image). Synthetic pending event, no job to take: run `succeeded` 111 s after running, of which 60 s idle window.
   Per pool with a real job: ci ____ s, rust ____ s, playwright ____ s, build ____ s.
5. Kaniko verdict: multi-stage build and push from the `build` pool under gVisor.
   Result: ____ (works / needs `--force` / fails: ____)
6. Cache hit on the second run of the same job.
   Result: ____ (`Successfully extracted cache`: yes / no)
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

## Decisions

| Date | Decision |
|---|---|
| 2026-10-24 | Runner pinned to `gitlab/gitlab-runner:v18.11.4`. |
| 2026-10-24 | Build pool on `ghcr.io/osscontainertools/kaniko:v1.28.6-alpine`, the maintained fork of the archived kaniko; its alpine variant ships git and restores the filesystem after a build (`KANIKO_PRE_CLEANUP`, `KANIKO_CLEANUP`, `KANIKO_PRESERVE_CONTEXT`). |
| 2026-10-24 | Build pool builds dir `/kaniko/builds`: kaniko never deletes `/kaniko`, so a multi-stage build keeps its context. |
| 2026-10-24 | The entrypoint passes every value as a flag and unsets `RUNNER_TOKEN`, `CACHE_S3_ACCESS_KEY` and `CACHE_S3_SECRET_KEY` before `exec`, so job scripts do not inherit them. |
| 2026-10-24 | The shell executor runs job steps in a bash login shell: every image writes its `PATH` to `/etc/profile.d/ciburst-path.sh` and the `runner` user has no `.bash_logout`. |
| 2026-10-24 | Releases use `glab release create` in `registry.gitlab.com/gitlab-org/cli:v1.124.0` with `GLAB_ENABLE_CI_AUTOLOGIN`; `release-cli` is deprecated since GitLab 18.0. |
| 2026-10-24 | Image builds use `docker:27.5.1` with `docker:27.5.1-dind`. |
