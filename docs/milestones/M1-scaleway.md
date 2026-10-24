# M1: Scaleway provider, Scaleway Function host, pool images

Base: the commit that adds this file. Three agents in worktrees, merged by the orchestrator. Spec: `docs/SPEC.md` sections 5 to 9. Contract: `docs/PROVIDERS.md`, `crates/core/src/types.rs`.

## Ownership

| Agent | Owns | Must not touch |
|---|---|---|
| A, provider | `crates/scaleway/src/provider.rs` and any new `crates/scaleway/src/provider/*.rs`, `crates/scaleway/src/testing/{account_api,iam_api,jobs_api,s3_api}.rs`, `crates/scaleway/tests/provider*.rs`, `crates/scaleway/Cargo.toml` (add dependencies) | `host.rs`, `functions_api.rs`, `client.rs`, `crates/cli`, `crates/dispatch-scaleway`, root `Cargo.toml` |
| B, host and function | `crates/scaleway/src/host.rs` and any new `crates/scaleway/src/host/*.rs`, `crates/scaleway/build.rs`, `crates/scaleway/src/testing/functions_api.rs`, `crates/scaleway/tests/host*.rs`, `crates/dispatch-scaleway/**`, `crates/scaleway/Cargo.toml` (add dependencies, build-dependencies) | `provider.rs`, the four provider fakes, `client.rs`, `crates/cli`, root `Cargo.toml` |
| C, images and ops | `images/**`, `ci/ciburst.yml`, `.gitlab-ci.yml`, `docs/RUNBOOK.md`, `scripts/**` | every crate, `docs/SPEC.md`, `README.md` |

`client.rs` and `crates/cli` stay with the orchestrator; ask for a change through the final report. `Cargo.lock` conflicts are resolved by the orchestrator at integration. Nobody edits `docs/SPEC.md`.

## Binding rules

1. Names: `config.name` is at most 20 characters. Every cloud resource is `<config.name>-<suffix>`: project `<name>`, IAM applications `<name>-dispatch` and `<name>-cache`, bucket `<name>-cache-<project id>`, job definitions `<name>-<pool.tag>`, namespace `<name>`, function `<name>-dispatch`.
2. Step ids: `project`, `key:dispatch`, `key:cache`, `bucket`, `definition:<tag>` (provider); `namespace`, `function`, `code` (host). Lookup by name before every create; a second `apply` plans only `noop`.
3. Outputs: `CIBURST_PROJECT_ID`, `CIBURST_DISPATCH_ACCESS_KEY`, `CIBURST_DISPATCH_SECRET_KEY`, `CIBURST_CACHE_ACCESS_KEY`, `CIBURST_CACHE_SECRET_KEY` (provider, secrets only when created or rotated), `CIBURST_DISPATCH_URL` (host).
4. Job definition fields: `cpu_limit` = vcpu x 1000 mvCPU, `memory_limit` MiB, `local_storage_capacity` MiB, `image_uri`, `job_timeout` as `"<seconds>s"`, `environment_variables` `CIBURST_POOL`, `CI_SERVER_URL`, `RUNNER_WAIT_TIMEOUT`, `CACHE_TYPE=s3`, `CACHE_S3_SERVER_ADDRESS=s3.<region>.scw.cloud`, `CACHE_S3_BUCKET_NAME`, `CACHE_S3_BUCKET_LOCATION=<region>`, `CACHE_SHARED=true` (cache keys only when `cache.type` is `s3`). Per-run overrides from `StartRequest.env` plus `CIBURST_PROJECT`, `CIBURST_PIPELINE`, `CIBURST_JOB`, `CIBURST_STAGE` from the labels.
5. Run states: `queued`, `scheduled`, `running` are live; `succeeded` is `Done`; `failed`, `canceled`, `internal_error` are `Failed`. `list` filters by job definition id of the pool.
6. Function: runtime `rust196`, handler `handle`, privacy `public`, `min_scale 0`, `max_scale 5`, `memory_limit 128`, `timeout 30s`, `http_option redirected`, secret env `HOOK_TOKEN`, `SCW_SECRET_KEY`, `RUNNER_TOKEN_<TAG>` (tag upper-cased, non-alphanumerics to `_`), `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`; plain env `CIBURST_CONFIG` (config JSON), `SCW_DEFAULT_ORGANIZATION_ID`, `SCW_DEFAULT_PROJECT_ID`, `SCW_REGION`. The host plans `update` for `function` when any of these differ from the deployed function (compare a digest stored in the function's `description`).
7. Function code: a zip embedded into `ciburst-scaleway` by `build.rs`; its root is the `ciburst-dispatch-scaleway` package (`Cargo.toml` with a `[workspace]` table of its own, `src/lib.rs`) with `ciburst-core` and `ciburst-scaleway` copied to `vendor/` as path dependencies and the workspace `Cargo.lock`. Verify the layout Scaleway's Rust builder expects against the `cors-rust` example in github.com/scaleway/serverless-examples and the docs before deciding; record the finding in the report.
8. The function builds `Config` from `CIBURST_CONFIG`, `Secrets` from the env, a `ScalewayJobs` provider with a `ScalewayClient` on `ReqwestClient`, and calls `Dispatcher::handle(headers, body)`; it answers the dispatcher's status and JSON body. A missing env var answers `500` with `{"result":"error"}` and logs the variable name, never a value.
9. Every HTTP call goes through `ScalewayClient` or, for S3, through `ciburst_core::http::HttpClient` with SigV4 (region from the client, service `s3`, host `s3.<region>.scw.cloud`, path-style bucket). No real network in tests.
10. Images: `FROM` the official toolchain image, `COPY --from=gitlab/gitlab-runner:<pinned> /usr/bin/gitlab-runner /usr/local/bin/gitlab-runner`, shared `images/base/entrypoint.sh` that composes `gitlab-runner run-single --url $CI_SERVER_URL --token $RUNNER_TOKEN --executor shell --builds-dir /builds --max-builds 0 --wait-timeout $RUNNER_WAIT_TIMEOUT` plus the cache flags from the `CACHE_*` variables; `CIBURST_DRY_RUN=1` prints the command and exits 0. Verify every flag against `gitlab-runner run-single --help` inside the image and record the result in the runbook.
11. Dependencies: add them in your crate's `Cargo.toml` with an explicit version; keep `cargo deny check` green (`deny.toml`); no new git or wildcard sources.
12. Docs are brief: facts, steps with their check, decisions with their date. No process notes in the repo; reports go to the orchestrator.

## APIs

Jobs `serverless-jobs/v1alpha1` (job definitions, start with `environment_variables` override, job runs, stop), Functions `functions/v1beta1` (namespaces, functions, `upload-url`, `deploy`, `domain_name`, `status`), IAM `iam/v1alpha1` (applications, policies with `rules[].permission_set_names` and `project_ids`, api-keys), Account `account/v3/projects`, Object Storage S3 at `s3.<region>.scw.cloud` (bucket create, lifecycle XML). Field names as in `github.com/scaleway/scaleway-sdk-go` and `www.scaleway.com/en/developers/api/`. Permission sets: `ServerlessJobsFullAccess`, `ObjectStorageFullAccess`.

## Tests

A: `crates/scaleway/tests/provider.rs` runs `ciburst_core::testing::contract::run_provider_contract` through a `ProviderFactory` whose backend is a fresh fake API; plus tests for idempotent lookups, rotation, S3 signing against a known vector, and error mapping. B: `crates/scaleway/tests/host.rs` for plan, apply, update on changed secrets, destroy and `url` against the Functions fake, zip layout test, and `crates/dispatch-scaleway` tests of `handle` with a fake provider. C: an `images` CI stage that builds every image, runs `gitlab-runner --version`, the toolchain's version command and `CIBURST_DRY_RUN=1` of the entrypoint; `shellcheck` on every script.

## Definition of done

`cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `cargo deny check` green in the worktree; no secret and no identifying value; no em or en dash; commits on the worktree branch with a final report to the orchestrator.
