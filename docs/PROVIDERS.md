# Providers and hosts

A provider runs jobs. A host runs the dispatcher. Both implement a trait from `ciburst_core::types` and are registered by name in `crates/cli/src/lib.rs` (`main_provider`, `provider`, `host`), selected by `provider.type` and `host.type` in the config. The extra keys of those sections reach the module as `Section::extra`.

Further providers serve single pools (spec D35): each key of the config's `providers` names a runtime, its object holds that provider's settings, and pools with that `runtime` belong to it. `ciburst_core::composite::Composite` routes between the main provider and the added ones. An added provider prefixes its run ids and step ids with `<runtime>:`, ignores every pool of another runtime, and returns the credentials the dispatcher needs as outputs listed in `provision::keys::PROVIDER_ENV`.

## Provider: `trait Provider`

| Method | Contract |
|---|---|
| `capabilities()` | `Capabilities { name, docker_daemon, max_vcpu, max_memory_mib, max_storage_mib, max_duration_s, archs, billing }`. The config check rejects pools above the maxima. |
| `plan(rotate)` | `Vec<Step>` with `id`, `action` (`Create`, `Update`, `Delete`, `Noop`), `resource`, `detail`. Pure: no writes. Pool definitions use the id `definition:<tag>`; pools missing from the config plan a `Delete`. `rotate` holds `Key` and/or `Cache`; a present key then plans an `Update`. |
| `apply(steps)` | Runs the steps in order, each idempotent. Returns outputs `CIBURST_DISPATCH_ACCESS_KEY`, `CIBURST_DISPATCH_SECRET_KEY`, `CIBURST_CACHE_ACCESS_KEY`, `CIBURST_CACHE_SECRET_KEY` when created or rotated, never otherwise. |
| `destroy()` | Removes every resource carrying the `name` prefix. |
| `start(StartRequest { pool, env, labels, taken })` | Starts one run of the pool's definition with `env` as per-run overrides (`RUNNER_TOKEN`, cache keys; for VMs also `CIBURST_VM_SECRET` and `CIBURST_DISPATCH_URL`) and `labels` (`project`, `pipeline`, `job`, `stage`). A VM starts with `free = slots - taken`. Errors without `RUNNER_TOKEN` or for an unprovisioned pool. Returns the run id. |
| `list(pool)` | `Run { run_id, pool, state, created_at, labels }`, state one of `Queued`, `Running`, `Done`, `Failed`. |
| `stop(run_id)` | Cancels a run; for a VM, terminates it. |
| `capacity(pool)` | VM pools: every VM with state, free slots, last report and creation time (spec 6.1); `None` for one-job runtimes. |
| `claim(run_id)`, `report(run_id, report, now)` | Lower a VM's free slots by one; store a VM's own report with `now` as last seen. |
| `shared_env()` | The main provider's agent image (`CIBURST_AGENT_IMAGE`) and cache location for runs of added providers; never a secret. |
| `sweep(now)` | Ends VMs that stopped behaving, through `ciburst_core::vm::sweep_reason`. |

VM providers render user data with `ciburst_core::cloud_init::render` and sign each VM's token with `ciburst_core::vm::token(vm secret, run id)`; the VM secret itself never enters user data.

Local state for modules that need it goes through `ciburst_core::store::Store` (`MemoryStore`, `FileStore`); cloud providers look resources up by name instead.

Contract tests: implement `ciburst_core::testing::contract::ProviderFactory` (feature `testing`) and call `run_provider_contract` from a test. `backend()` returns an empty backend, `make(config, &backend)` binds a provider to it, so a changed config is planned against existing resources. See `crates/core/tests/contract_fake.rs`. A cloud provider runs the contract against an in-process fake of its API behind `ciburst_core::http::HttpClient`, as `testing::gitlab_fake::FakeGitlab` does for GitLab.

## Host: `trait Host`

| Method | Contract |
|---|---|
| `plan(secrets)` | Steps for the dispatcher deployment; `Update` when config or secrets changed. |
| `apply(steps, secrets)` | Deploys; returns `CIBURST_DISPATCH_URL`. |
| `destroy()` | Removes the deployment. |
| `url()` | The public URL, or `None` before the first deploy. |

`Secrets { hook_token, runner_tokens, dispatch, cache, vm_secret, sweep_token, provider_env }` comes from `ciburst_core::provision::secrets_from_outputs`. The host's function crate adapts the platform request to `Dispatcher::handle(headers, body)` from `ciburst_core::dispatch` and returns its `status` and `body`.

## Apply order

provider, GitLab runners (one per pool, enabled in every further project), host, GitLab hooks. Outputs go to `.local/outputs.env`; existing keys are never overwritten except by `rotate`.
