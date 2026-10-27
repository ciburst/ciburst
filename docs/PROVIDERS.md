# Providers and hosts

A provider runs jobs: the VM runner's VMs and the lanes' runs. A host runs the dispatcher. Both implement a trait from `ciburst_core::types`. Scaleway is the only provider and host (D65); `crates/scaleway` is the template for another: the API clients, in-process API fakes under `src/testing`, and the contract tests. The config's `provider.type` and `host.type` select them.

## Register a provider

1. Crate `crates/<provider>`, listed in `members` of the root `Cargo.toml`.
   Check: `cargo build --workspace` passes.
2. CLI, `crates/cli/src/lib.rs`: the provider in `Modules::main_provider` and `PROVIDERS`, with its credentials from `<local>/<provider>.env` or, with `--ci`, from `CIBURST_<VAR>` variables that pipeline mode reads and never writes.
   Check: `cargo run -p ciburst -- plan --config <config>` with `provider.type` set to it lists its steps.
3. Function: a host's function crate builds the provider from its environment, as `crates/dispatch-scaleway` does in `Env::backends_on`; `crates/scaleway/build.rs` vendors every path dependency of the function into the zip.
   Check: `cargo test -p ciburst-dispatch-scaleway` passes.
4. Config: `vm.runtime` accepts only `instances` today (`ciburst_core::config::validate`); a provider with another VM runtime adds its name there.
   Check: a config with the new runtime validates.
5. `docs/RUNBOOK.md`: the `<provider>.env` row of the `.local/` table, facts, console checks and failure modes.
   Check: the developer setup works from the runbook alone.

A host registers in `Modules::host` and `HOSTS` of the CLI and needs its own function crate.

## Provider: `trait Provider`

| Method | Contract |
|---|---|
| `capabilities()` | `Capabilities { name, docker_daemon, max_vcpu, max_memory_mib, max_storage_mib, max_duration_s, archs, billing }` |
| `capabilities_for(pool)` | the capabilities of one run spec (`config::Pool`: the VM runner or one lane); the config check tests every lane against them, `/join` every member lane. Default `capabilities()` |
| `preflight()` | fails when the VM runner could not start a VM after `apply` (no quota, no agent); `apply` calls it before any change, `plan` only warns. Default `Ok(())` |
| `plan(rotate)` | `Vec<Step>` with `id`, `action` (`Create`, `Update`, `Delete`, `Noop`), `resource`, `detail`; no writes. `rotate` holds `Key` and/or `Cache` |
| `apply(steps)` | runs the given steps in order, each idempotent; returns outputs (below) |
| `destroy()` | removes every resource of the config's `name`, running VMs and every member's lane definitions included |
| `start(StartRequest { pool, env, labels, taken })` | starts one run with `env` (below) and `labels` `project`, `pipeline`, `job`, `stage`; a VM starts with `free = slots - taken`; fails without `RUNNER_TOKEN` or for an unprovisioned run spec; returns the run id |
| `list(pool)` | `Run { run_id, pool, state, created_at, labels }` with state `Queued`, `Running`, `Done` or `Failed`, `pool` the VM runner's tag or the lane key `<project id>-<lane tag>`; `None` lists every run |
| `stop(run_id)` | cancels a run; a VM is deleted |
| `capacity(pool)` | for the VM runner every VM as `VmCapacity { run_id, state, free, seen, created_at }`; `None` (default) for lanes, whose live runs the dispatcher counts |
| `claim(run_id)` | lowers a VM's free slots by one, not below zero |
| `report(run_id, report, now)` | stores a VM's `VmReport { state, free, slots }` with `now` as last seen, and the drain time when it turns `draining` |
| `shared_env()` | what a VM needs besides the start request: `CIBURST_AGENT_URL`, `CIBURST_AGENT_SHA256`, `CIBURST_RUNNER_URL` and the cache location (`CACHE_TYPE`, `CACHE_S3_SERVER_ADDRESS`, `CACHE_S3_BUCKET_NAME`, `CACHE_S3_BUCKET_LOCATION`, `CACHE_SHARED`); never a secret |
| `sweep(now)` | ends the VMs for which `ciburst_core::vm::sweep_reason` gives a reason; returns their ids |
| `sync_lanes(project, lanes)` | makes the lane definitions of `project` match `lanes` (run specs of `config::Lane::pool`): creates and updates them, deletes the project's other lane definitions, and answers each definition's id by lane tag. The dispatcher calls it on `/join` and `/leave` with the dispatch key, within the 25 s of `JOIN_BUDGET`. Default: an error, no lanes |
| `fleet(secrets)` | the deployment's fleet state (`FleetStore`), opened with the secrets of `apply`; `None` (default) for a provider that keeps none, whose deployments serve the projects of their config |

## Step ids, run ids, run specs

- Step ids are stable across plans. Lane definitions are `definition:<project id>-<lane tag>`; shared resources have fixed ids such as `project`, `bucket`, `runner-binary`, `vm:agent`, `vm:security-group`.
- `plan` and `apply` handle the host's lanes from the config. A definition whose project is not the host's belongs to a member and follows that member's `/join`; `apply` leaves it alone.
- A run spec (`config::Pool`) is either the VM runner (`project` `None`, `tag` the VM runner's tag) or one lane (`project` set, `tag` the lane key `<project id>-<lane tag>`, `runner_tag()` the lane tag).

## Outputs

`Provisioner::apply` calls `apply` once per non-noop step and writes that call's outputs with overwrite, then calls `apply(&[])` and writes its outputs without overwrite. Keys of `keys::DISPATCHER_ENV` (`CIBURST_AGENT_SHA256`) are overwritten from every call and reach the function as plain environment variables (`provision::secrets_from_outputs`).

- Return a secret only from the call that created or rotated it; a later call without it keeps the stored value.

## Runner object

Step `runner-binary`: the object `ciburst/runner/<ciburst_core::cloud_init::RUNNER_SHA256>` in the cache bucket, public-read. `apply` downloads `cloud_init::RUNNER_URL`, refuses a SHA-256 other than `RUNNER_SHA256` and uploads the binary; the step is `noop` when the object exists. VMs and lane runs download `gitlab-runner` from it and check the hash; `shared_env()` hands its URL to the VM's user data as `CIBURST_RUNNER_URL`.

## VMs

`StartRequest.env` of the VM runner holds `RUNNER_TOKEN` (the host's VM runner), `CIBURST_VM_SECRET`, `CIBURST_DISPATCH_URL` and, with the S3 cache, `CACHE_S3_ACCESS_KEY` and `CACHE_S3_SECRET_KEY`.

The provider adds `CIBURST_RUN_ID` (known before the user data is set) and `CIBURST_VM_TOKEN`, which is `ciburst_core::vm::token(<CIBURST_VM_SECRET>, <run id>)`, and renders the user data with `ciburst_core::cloud_init::render`. The user data downloads the runner object and the agent object and checks each against its SHA-256. The VM secret never enters user data.

`/etc/ciburst/vm.env`, read by the agent: `CIBURST_RUN_ID`, `CIBURST_VM_TOKEN`, `CIBURST_DISPATCH_URL`, `CI_SERVER_URL`, `RUNNER_TOKEN`, `CIBURST_POOL` (the VM runner's tag), `CIBURST_SLOTS`, `CIBURST_DEFAULT_IMAGE` (`vm.image`), `CIBURST_TIMEOUT_S`; with the S3 cache also `CACHE_TYPE=s3`, `CACHE_S3_SERVER_ADDRESS`, `CACHE_S3_BUCKET_NAME`, `CACHE_S3_BUCKET_LOCATION`, `CACHE_S3_ACCESS_KEY`, `CACHE_S3_SECRET_KEY`, `CACHE_SHARED`. The agent exits 64 naming a missing key. Members' VM runners reach the agent through the report answer, not through `vm.env` (spec 6.2).

A VM's state, free slots, last report and drain time live with the server (tags on Scaleway), so the function stays stateless.

## Lane runs

A lane is a one-job runtime that runs the lane's image (D45, `runtime` `jobs` on Scaleway). Its definition carries:

| Field | Value |
|---|---|
| name | `<name>-<project id>-<lane tag>` |
| image | the lane's `image` |
| command | `ciburst_core::lane::COMMAND`: `/bin/sh -c` with one argument without spaces (Scaleway splits a command at spaces and ignores quotes), which decodes `CIBURST_LANE` with `base64 -d` and evaluates it |
| environment | `CIBURST_LANE` (base64 of `ciburst_core::lane::BOOTSTRAP`, the script `lane-bootstrap.sh`), `CIBURST_RUNNER_URL` (the runner object's public URL), `CIBURST_RUNNER_SHA256` (`cloud_init::RUNNER_SHA256`), `CIBURST_POOL` (the lane key), `CIBURST_PROJECT` (the lane's project), `CI_SERVER_URL`, `RUNNER_WAIT_TIMEOUT` (`waitTimeoutS`) and, with the S3 cache, `CACHE_TYPE`, `CACHE_S3_SERVER_ADDRESS`, `CACHE_S3_BUCKET_NAME`, `CACHE_S3_BUCKET_LOCATION`, `CACHE_SHARED` |

`plan` and `sync_lanes` update a definition when any of these changes. `StartRequest.env` of a lane holds `RUNNER_TOKEN` (the lane's runner in its own project) and, with the S3 cache, `CACHE_S3_ACCESS_KEY` and `CACHE_S3_SECRET_KEY`, never `CIBURST_VM_SECRET`; the run also gets the labels as `CIBURST_PROJECT`, `CIBURST_PIPELINE`, `CIBURST_JOB` and `CIBURST_STAGE`.

The bootstrap needs `CI_SERVER_URL`, `RUNNER_TOKEN`, `CIBURST_RUNNER_URL` and `CIBURST_RUNNER_SHA256`, and with `CACHE_TYPE=s3` the cache location and key. As root it installs missing `git`, `curl` and CA certificates with `apk` or `apt-get` (spec section 7.3); then it downloads and checks the runner, unsets the token and cache keys and execs `gitlab-runner run-single` with the shell executor. Optional: `CIBURST_BUILDS_DIR` (`/builds`, for a non-root user `$HOME/builds`), `CIBURST_CACHE_DIR` (`/cache`, `$HOME/cache`); `CIBURST_LANE_DRY_RUN=1` installs, skips the download and prints the command with secrets masked. A failed check prints `ciburst-lane: <reason>` to stderr and exits 64 (`docs/RUNBOOK.md`, Failure modes).

## Tests

- The feature `testing` of `ciburst-core` exposes `ciburst_core::testing`: the contract harnesses and `gitlab_fake::FakeGitlab`. A provider crate gets its own `testing` feature that enables `ciburst-core/testing` and holds its API fakes behind `ciburst_core::http::HttpClient`; its dev-dependency on itself turns the feature on (`crates/scaleway/Cargo.toml`).
- `testing::contract::run_provider_contract` fits lane runtimes. Implement `ProviderFactory`: `backend()` returns an empty backend, `make(config, &backend)` binds a provider to it, so a changed config is planned against existing resources. It checks plan and idempotent apply, `definition:<project id>-<tag>` updates and deletes, key rotation, start, list, stop and destroy (`crates/core/tests/contract_fake.rs`).
- `testing::vm_contract::run_vm_provider_contract` fits VM runtimes: start with `taken` gives `free = slots - taken`; list and capacity show the run; claim lowers `free`; report sets state, free and seen; a silent VM is swept by `vm::sweep_reason`; other runs are ignored; stop ends the run. Scaleway Instances and the core fake run it.
- `sync_lanes`: a join with a lane change updates its definition, a removed lane deletes it, and another project's definitions stay (`crates/core/src/dispatch/members/tests.rs` on the core fake; `crates/scaleway/tests/provider_lanes.rs` on the Serverless Jobs fake, with the 63-character name limit); `apply` leaves member definitions alone (`crates/scaleway/tests/provider_instances.rs`).

## Host: `trait Host`

| Method | Contract |
|---|---|
| `plan(secrets)` | steps of the dispatcher deployment; `Update` when its settings, environment or code changed |
| `apply(steps, secrets)` | deploys; returns `CIBURST_DISPATCH_URL` |
| `destroy()` | removes the deployment |
| `url()` | the public URL, `None` before the first deploy |

`Secrets { hook_token, runner_tokens, dispatch, cache, vm_secret, sweep_token, dispatcher_env, fleet_key }` comes from `ciburst_core::provision::secrets_from_outputs`; the host stores `dispatcher_env` as plain and the rest as secret environment variables, and compares the config by `Config::deployment_view`, so a routing change alone needs no deploy (D58).

The host's function hands every request to `Dispatcher::handle_request(method, path, headers, body, now)` from `ciburst_core::dispatch` and answers with its `status` and JSON `body` (no body for `Value::Null`):

| Request | Handling |
|---|---|
| `POST /vm`, headers `X-Ciburst-Run` and `X-Ciburst-Token`, body `{state, free, slots}` | the VM's report, `200 {"result":"reported","nextReportS":<s>,"runners":[...]}` |
| `POST /terminate`, the same headers | ends the calling VM, `200 {"result":"terminated"}` |
| a JSON body `{"ciburst_sweep": "<sweep token>"}`, any path | `sweep(now)`, `200 {"result":"swept","runs":[...]}` |
| `GET /fleet`, `POST /join`, `POST /leave`, header `X-Ciburst-Join` | the fleet endpoints (spec section 8.3) |
| anything else | the GitLab webhook (`docs/RUNBOOK.md`, Dispatcher answers) |

A wrong or missing token answers `401` without a body. The host also needs a schedule that posts the sweep body every 30 minutes.

## Apply order

`preflight`, provider steps, GitLab runners (the VM runner's, enabled in every further project of `gitlab.projectIds`, and one per host lane in the host project), fleet state (config and member records), host, GitLab webhooks, replay of waiting jobs. Outputs go to `.local/outputs.env`, or with `--ci` to project variables.
