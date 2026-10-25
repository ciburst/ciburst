# Providers and hosts

A provider runs jobs; a host runs the dispatcher. Both implement a trait from `ciburst_core::types`. `crates/hetzner` is the template for a VM provider: the API client, an in-process API fake under `src/testing`, and the contract tests.

The config's `provider.type` selects the main provider. Each key of the config's `providers` adds a provider that serves the pools whose `runtime` names that key (D35); `ciburst_core::composite::Composite` routes between them.

## Register a provider

1. Crate `crates/<provider>`, listed in `members` of the root `Cargo.toml`.
   Check: `cargo build --workspace` passes.
2. CLI, `crates/cli/src/lib.rs`: a main provider in `Modules::main_provider` and `PROVIDERS`; an added provider in `Modules::provider`, with its credential from `<local>/<provider>.env` or, with `--ci`, from `CIBURST_<VAR>` (`HCLOUD_TOKEN` becomes `CIBURST_HCLOUD_TOKEN`).
   Check: `cargo run -p ciburst -- plan --config <config>` with a pool of the provider lists its steps.
3. Function, `crates/dispatch-scaleway`: the crate as a dependency, its credential read in `Env::from_lookup` when the config adds the provider, the provider added to the `Composite` in `Env::backends_on`. `crates/scaleway/build.rs` vendors every path dependency of the function into the zip.
   Check: `cargo test -p ciburst-dispatch-scaleway` passes.
4. The credential the function needs, by variable name, in `ciburst_core::provision::keys::PROVIDER_ENV`. The host stores it as a secret environment variable; the name is upper case and does not start with `CIBURST_`, `RUNNER_TOKEN_` or `SCW_`.
   Check: after `ciburst apply` the function lists the variable among its secret environment variables.
5. `docs/RUNBOOK.md`: the `<provider>.env` row of the `.local/` table, facts, console checks and failure modes.
   Check: the developer setup works from the runbook alone.

A host registers in `Modules::host` and `HOSTS` of the CLI and needs its own function crate.

## Provider: `trait Provider`

| Method | Contract |
|---|---|
| `capabilities()` | `Capabilities { name, docker_daemon, max_vcpu, max_memory_mib, max_storage_mib, max_duration_s, archs, billing }` |
| `capabilities_for(pool)` | the capabilities of one pool; the config check tests every pool against them, and `Composite` answers with the member serving the pool. Default `capabilities()` |
| `preflight()` | fails when a pool could not start a run after `apply` (no quota, no agent); `apply` calls it before any change, `plan` only warns. Default `Ok(())` |
| `plan(rotate)` | `Vec<Step>` with `id`, `action` (`Create`, `Update`, `Delete`, `Noop`), `resource`, `detail`; no writes. `rotate` holds `Key` and/or `Cache` |
| `apply(steps)` | runs the given steps in order, each idempotent; returns outputs (below) |
| `destroy()` | removes every resource of the config's `name`, running VMs included |
| `start(StartRequest { pool, env, labels, taken })` | starts one run with `env` (below) and `labels` `project`, `pipeline`, `job`, `stage`; a VM starts with `free = slots - taken`; fails without `RUNNER_TOKEN` or for an unprovisioned pool; returns the run id |
| `list(pool)` | `Run { run_id, pool, state, created_at, labels }` with state `Queued`, `Running`, `Done` or `Failed`; `None` lists every pool |
| `stop(run_id)` | cancels a run; a VM is deleted |
| `capacity(pool)` | VM providers: every VM of the pool as `VmCapacity { run_id, state, free, seen, created_at }`; `None` (default) for one-job runtimes, whose live runs the dispatcher counts |
| `claim(run_id)` | lowers a VM's free slots by one, not below zero |
| `report(run_id, report, now)` | stores a VM's `VmReport { state, free, slots }` with `now` as last seen, and the drain time when it turns `draining` |
| `shared_env()` | main provider only: what VMs of added providers need, `CIBURST_AGENT_URL`, `CIBURST_AGENT_SHA256`, `CIBURST_RUNNER_URL` and the cache location (`CACHE_TYPE`, `CACHE_S3_SERVER_ADDRESS`, `CACHE_S3_BUCKET_NAME`, `CACHE_S3_BUCKET_LOCATION`, `CACHE_SHARED`); never a secret |
| `sweep(now)` | ends the VMs for which `ciburst_core::vm::sweep_reason` gives a reason; returns their ids |

## Step ids, run ids, runtime names

- Step ids are stable across plans. A one-job runtime names its pool definitions `definition:<tag>`; shared resources have fixed ids such as `project`, `bucket`, `runner-binary`, `vm:agent`, `vm:security-group`.
- An added provider starts every step id and run id with `<runtime>:` (`hetzner:firewall`, `hetzner:<server name>`) and ignores pools of other runtimes; `Composite` routes `apply`, `stop`, `claim` and `report` by that prefix.
- Reserved, so not a runtime name: `instances`, `jobs`, `vm`, `key`, `definition`, `bucket`, `registry`, `project`, `iam`, `hook`, `runner`, `runner-enable`, `function`, `namespace`, `cron`.

## Outputs

`Provisioner::apply` calls `apply` once per non-noop step and writes that call's outputs with overwrite, then calls `apply(&[])` and writes its outputs without overwrite. Keys of `keys::PROVIDER_ENV` and `keys::DISPATCHER_ENV` are overwritten from every call.

- Return a secret only from the call that created or rotated it; a later call without it keeps the stored value.
- `PROVIDER_ENV` keys (`HCLOUD_TOKEN`) reach the function as secret environment variables, `DISPATCHER_ENV` keys (`CIBURST_AGENT_SHA256`) as plain ones (`provision::secrets_from_outputs`). With `--ci` the `PROVIDER_ENV` keys stay in memory.

## Runner object

Step `runner-binary`, present whenever the config has a pool: the object `ciburst/runner/<ciburst_core::cloud_init::RUNNER_SHA256>` in the cache bucket, public-read. `apply` downloads `cloud_init::RUNNER_URL`, refuses a SHA-256 other than `RUNNER_SHA256` and uploads the binary; the step is `noop` when the object exists. VMs of every provider and every lane run download `gitlab-runner` from it and check the hash; `shared_env()` hands its URL to added providers as `CIBURST_RUNNER_URL`.

## VM providers

`StartRequest.env` of a VM pool holds `RUNNER_TOKEN`, `CIBURST_VM_SECRET`, `CIBURST_DISPATCH_URL` and, with an S3 cache, `CACHE_S3_ACCESS_KEY` and `CACHE_S3_SECRET_KEY`. An added provider also gets the main provider's `shared_env()`; keys already in the request win.

The provider adds `CIBURST_RUN_ID` (chosen before create, so the token fits into the user data) and `CIBURST_VM_TOKEN`, which is `ciburst_core::vm::token(<CIBURST_VM_SECRET>, <run id>)`, and renders the user data with `ciburst_core::cloud_init::render`. The user data downloads the runner object and the agent object (an added provider takes their URLs from `shared_env()`) and checks each against its SHA-256. The VM secret never enters user data.

`/etc/ciburst/vm.env`, read by the agent: `CIBURST_RUN_ID`, `CIBURST_VM_TOKEN`, `CIBURST_DISPATCH_URL`, `CI_SERVER_URL`, `RUNNER_TOKEN`, `CIBURST_POOL`, `CIBURST_SLOTS`, `CIBURST_DEFAULT_IMAGE` (the pool's `image`), `CIBURST_TIMEOUT_S`; with an S3 cache also `CACHE_TYPE=s3`, `CACHE_S3_SERVER_ADDRESS`, `CACHE_S3_BUCKET_NAME`, `CACHE_S3_BUCKET_LOCATION`, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`, `CACHE_SHARED`. The agent exits 64 naming a missing key.

A VM's state, free slots, last report and drain time live with the server (tags on Scaleway, labels on Hetzner), so the function stays stateless.

## Lane runs

A lane (`runtime: jobs` on Scaleway) is a one-job runtime that runs the job's own image (D45). Its definition carries:

| Field | Value |
|---|---|
| image | the lane's `image` |
| command | `ciburst_core::lane::COMMAND`: `/bin/sh -c` with one argument without spaces (Scaleway splits a command at spaces and ignores quotes), which decodes `CIBURST_LANE` with `base64 -d` and evaluates it |
| environment | `CIBURST_LANE` (base64 of `ciburst_core::lane::BOOTSTRAP`, the script `lane-bootstrap.sh`), `CIBURST_RUNNER_URL` (the runner object's public URL), `CIBURST_RUNNER_SHA256` (`cloud_init::RUNNER_SHA256`), `CIBURST_POOL`, `CI_SERVER_URL`, `RUNNER_WAIT_TIMEOUT` (`waitTimeoutS`) and, with an S3 cache, `CACHE_TYPE`, `CACHE_S3_SERVER_ADDRESS`, `CACHE_S3_BUCKET_NAME`, `CACHE_S3_BUCKET_LOCATION`, `CACHE_SHARED` |

`plan` updates a definition when any of these changes. `StartRequest.env` of a lane holds `RUNNER_TOKEN` and, with an S3 cache, `CACHE_S3_ACCESS_KEY` and `CACHE_S3_SECRET_KEY`, never `CIBURST_VM_SECRET`; the run also gets the labels as `CIBURST_PROJECT`, `CIBURST_PIPELINE`, `CIBURST_JOB` and `CIBURST_STAGE`.

The bootstrap needs `CI_SERVER_URL`, `RUNNER_TOKEN`, `CIBURST_RUNNER_URL` and `CIBURST_RUNNER_SHA256`, and with `CACHE_TYPE=s3` the cache location and key. As root it installs missing `git`, `curl` and CA certificates with `apk` or `apt-get` (spec section 6.3); then it downloads and checks the runner, unsets the token and cache keys and execs `gitlab-runner run-single` with the shell executor. Optional: `CIBURST_BUILDS_DIR` (`/builds`), `CIBURST_CACHE_DIR` (`/cache`); `CIBURST_LANE_DRY_RUN=1` installs, skips the download and prints the command with secrets masked. A failed check prints `ciburst-lane: <reason>` to stderr and exits 64 (`docs/RUNBOOK.md`, Failure modes).

## Tests

- The feature `testing` of `ciburst-core` exposes `ciburst_core::testing`: the contract harnesses and `gitlab_fake::FakeGitlab`. A provider crate gets its own `testing` feature that enables `ciburst-core/testing` and holds its API fake behind `ciburst_core::http::HttpClient`; its dev-dependency on itself turns the feature on (`crates/hetzner/Cargo.toml`).
- `testing::contract::run_provider_contract` fits one-job providers. Implement `ProviderFactory`: `backend()` returns an empty backend, `make(config, &backend)` binds a provider to it, so a changed config is planned against existing resources. It checks plan and idempotent apply, `definition:<tag>` updates and deletes, key rotation, start, list, stop and destroy (`crates/core/tests/contract_fake.rs`).
- `testing::vm_contract::run_vm_provider_contract` fits VM providers: start with `taken` gives `free = slots - taken`; list and capacity show the run; claim lowers `free`; report sets state, free and seen; a silent VM is swept by `vm::sweep_reason`; runs of other pools and ids of other runtimes are ignored; stop ends the run. Scaleway Instances, Hetzner and the core fake run it.

## Host: `trait Host`

| Method | Contract |
|---|---|
| `plan(secrets)` | steps of the dispatcher deployment; `Update` when config or secrets changed |
| `apply(steps, secrets)` | deploys; returns `CIBURST_DISPATCH_URL` |
| `destroy()` | removes the deployment |
| `url()` | the public URL, `None` before the first deploy |

`Secrets { hook_token, runner_tokens, dispatch, cache, vm_secret, sweep_token, provider_env, dispatcher_env }` comes from `ciburst_core::provision::secrets_from_outputs`; the host stores `provider_env` as secret and `dispatcher_env` as plain environment variables.

The host's function hands every request to `Dispatcher::handle_request(method, path, headers, body, now)` from `ciburst_core::dispatch` and answers with its `status` and JSON `body` (no body for `Value::Null`):

| Request | Handling |
|---|---|
| `POST /vm`, headers `X-Ciburst-Run` and `X-Ciburst-Token`, body `{state, free, slots}` | the VM's report, `200 {"result":"reported"}` |
| `POST /terminate`, the same headers | ends the calling VM, `200 {"result":"terminated"}` |
| a JSON body `{"ciburst_sweep": "<sweep token>"}`, any path | `sweep(now)`, `200 {"result":"swept","runs":[...]}` |
| anything else | the GitLab webhook (`docs/RUNBOOK.md`, Dispatcher answers) |

A wrong or missing token answers `401` without a body. The host also needs a schedule that posts the sweep body every 30 minutes.

## Apply order

`preflight`, provider steps, GitLab runners (one per pool, enabled in every further project; the runner of the VM pool that lanes borrow slots from also carries their tags), host, GitLab webhooks, replay of waiting jobs. Outputs go to `.local/outputs.env`, or with `--ci` to project variables.
