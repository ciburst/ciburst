# Providers and hosts

A provider runs jobs. A host runs the dispatcher. Both implement a trait from `ciburst_core::types` and are registered by name in `crates/cli/src/lib.rs` (`make_provider`, `make_host`), selected by `provider.type` and `host.type` in the config. The extra keys of those sections reach the module as `Section::extra`.

## Provider: `trait Provider`

| Method | Contract |
|---|---|
| `capabilities()` | `Capabilities { name, docker_daemon, max_vcpu, max_memory_mib, max_storage_mib, max_duration_s, archs, billing }`. The config check rejects pools above the maxima. |
| `plan(rotate)` | `Vec<Step>` with `id`, `action` (`Create`, `Update`, `Delete`, `Noop`), `resource`, `detail`. Pure: no writes. Pool definitions use the id `definition:<tag>`; pools missing from the config plan a `Delete`. `rotate` holds `Key` and/or `Cache`; a present key then plans an `Update`. |
| `apply(steps)` | Runs the steps in order, each idempotent. Returns outputs `CIBURST_DISPATCH_ACCESS_KEY`, `CIBURST_DISPATCH_SECRET_KEY`, `CIBURST_CACHE_ACCESS_KEY`, `CIBURST_CACHE_SECRET_KEY` when created or rotated, never otherwise. |
| `destroy()` | Removes every resource carrying the `name` prefix. |
| `start(StartRequest { pool, env, labels })` | Starts one run of the pool's definition with `env` as per-run overrides (`RUNNER_TOKEN`, cache keys) and `labels` (`project`, `pipeline`, `job`, `stage`). Errors without `RUNNER_TOKEN` or for an unprovisioned pool. Returns the run id. |
| `list(pool)` | `Run { run_id, pool, state, created_at, labels }`, state one of `Queued`, `Running`, `Done`, `Failed`. |
| `stop(run_id)` | Cancels a run. |

Local state for modules that need it goes through `ciburst_core::store::Store` (`MemoryStore`, `FileStore`); cloud providers look resources up by name instead.

Contract tests: implement `ciburst_core::testing::contract::ProviderFactory` (feature `testing`) and call `run_provider_contract` from a test. `backend()` returns an empty backend, `make(config, &backend)` binds a provider to it, so a changed config is planned against existing resources. See `crates/core/tests/contract_fake.rs`. A cloud provider runs the contract against an in-process fake of its API behind `ciburst_core::http::HttpClient`, as `testing::gitlab_fake::FakeGitlab` does for GitLab.

## Host: `trait Host`

| Method | Contract |
|---|---|
| `plan(secrets)` | Steps for the dispatcher deployment; `Update` when config or secrets changed. |
| `apply(steps, secrets)` | Deploys; returns `CIBURST_DISPATCH_URL`. |
| `destroy()` | Removes the deployment. |
| `url()` | The public URL, or `None` before the first deploy. |

`Secrets { hook_token, runner_tokens, dispatch, cache }` comes from `ciburst_core::provision::secrets_from_outputs`. The host's function crate adapts the platform request to `Dispatcher::handle(headers, body)` from `ciburst_core::dispatch` and returns its `status` and `body`.

## Apply order

provider, GitLab runners (one per pool, enabled in every further project), host, GitLab hooks. Outputs go to `.local/outputs.env`; existing keys are never overwritten except by `rotate`.
