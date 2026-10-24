# Providers and hosts

A provider runs jobs. A host runs the dispatcher. Both are ES modules selected by `provider.type` and `host.type` in the config and loaded from `providers/<type>/provider.mjs` and `hosts/<type>/host.mjs`. Types are in `core/types.mjs`.

## Provider: `export function createProvider({ config, credentials, log, store })`

| Method                         | Contract                                                                                                                                                                                                                                 |
| ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `capabilities()`               | `{ name, dockerDaemon, maxVcpu, maxMemoryMiB, maxStorageMiB, maxDurationS, archs, billing }`. The config validator rejects pools above the maxima.                                                                                       |
| `plan({ rotate })`             | Steps `{ id, action, resource, detail? }` with action `create`, `update`, `delete` or `noop`. Pure: no writes. `rotate` holds `key` and/or `cache`; a present key then plans an `update`. Pools missing from the config plan a `delete`. |
| `apply(steps)`                 | Runs the steps in order, each idempotent. Returns outputs: `CIBURST_DISPATCH_ACCESS_KEY`, `CIBURST_DISPATCH_SECRET_KEY`, `CIBURST_CACHE_ACCESS_KEY`, `CIBURST_CACHE_SECRET_KEY` when created or rotated, never otherwise.                |
| `destroy()`                    | Removes every resource carrying the `name` prefix.                                                                                                                                                                                       |
| `start({ pool, env, labels })` | Starts one run of the pool's definition with `env` as per-run overrides (`RUNNER_TOKEN`, cache keys) and `labels` (`project`, `pipeline`, `job`, `stage`). Rejects without `RUNNER_TOKEN` or an unprovisioned pool. Returns `{ runId }`. |
| `list({ pool? })`              | Runs `{ runId, pool, state, createdAt, labels }`, state one of `queued`, `running`, `done`, `failed`.                                                                                                                                    |
| `stop(runId)`                  | Cancels a run.                                                                                                                                                                                                                           |

`store` is a `{ load, save }` JSON slot for providers that keep local state (`core/store.mjs`); cloud providers look resources up by name and ignore it.

Contract tests: import `providerContract` from `test/contract/provider.contract.mjs` and call it with a factory. The factory gets `(config, previous)`; with `previous` it must share that provider's backend. See `test/fake-provider.test.mjs`. A cloud provider runs the contract against an in-process fake of its API (`test/fakes/`).

## Host: `export function createHost({ config, credentials, log, store, root })`

| Method                      | Contract                                                                      |
| --------------------------- | ----------------------------------------------------------------------------- |
| `plan({ secrets })`         | Steps for the dispatcher deployment; `update` when config or secrets changed. |
| `apply(steps, { secrets })` | Deploys; returns `{ CIBURST_DISPATCH_URL }`.                                  |
| `destroy()`                 | Removes the deployment.                                                       |
| `url()`                     | The public URL, or `undefined` before the first deploy.                       |

`secrets` is `{ hookToken, runnerTokens, dispatch, cache }` from `core/provision.mjs`. The host's `handler.mjs` adapts the platform event to `createDispatcher(...).handle({ headers, body })` from `core/dispatch.mjs` and returns its `{ status, body }`.

## Apply order

provider, GitLab runners (one per pool, enabled in every further project), host, GitLab hooks. Outputs go to `.local/outputs.env`; existing keys are never overwritten except by `rotate`.
