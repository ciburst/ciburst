# M4: cost

Go: owner, 2026-10-24 ("Optimize for costs"), on the three options after the M3 drill. Spec D32 to D35.

## Rules

R1. Claim candidates: VMs of the pool, not draining, `free > 0`, created more than `GROUP_WINDOW_S` (30) seconds ago, and either `idle` or `busy` with a report within `CAPACITY_FRESH_S`, or `starting` and created less than `SWEEP_STARTING_S` (600) seconds ago. A VM created within the window is never a candidate; the ledger accounts for it.

R2. Claim choice: among the candidates with the highest `free`, sorted by run id, index `fnv1a64(job id) % count`. Parallel claims of one burst spread instead of all hitting one VM.

R3. No candidate and a ledger: record the intent `(pool, now, job)`, wait `GROUP_WAIT` (2 s, injectable), read the pool's intents since `now - GROUP_WINDOW_S`. Order by time, then job id as a number (string order if either is not a number); keep each job's earliest intent.
- My job's earliest intent is older than `now`: duplicate event, answer `200 duplicate`, start nothing.
- Rank `r` (0-based) of my intent; `r % slots != 0`: answer `200 grouped` with the leader's job id; the leader's VM takes the job.
- Leader: `g = r / slots`; `live_old` = VMs of the pool not draining and created before `now - GROUP_WINDOW_S`; `live_old + g >= maxRuns` answers `200 at_capacity`; otherwise start a VM with `taken = min(slots, n - r)`, `n` the number of intents read.

R4. A ledger error (record or read) logs `ledger_failed` and falls back to R5.

R5. No ledger: the M3 rule. `live` (not draining) `>= maxRuns` answers `at_capacity`, otherwise start with `taken = 1`.

R6. `StartRequest.taken` (default 1): slots of the new VM already assigned; a VM provider writes `free = slots - taken` at creation. One-job runtimes ignore it.

R7. `Dispatcher::with_ledger(Option<Arc<dyn Ledger>>)` and `with_group_wait(Duration)`. Answers keep the shape `{ "result": ..., "pool": ... }`; `grouped` adds `"leader"`, `duplicate` nothing else. Log lines `grouped`, `duplicate`, `ledger_failed`.

R8. Composite (`core::composite::Composite::new(main, extras)`, `extras` keyed by runtime name):
- `start`: member of the pool's runtime (none means main); for an extra the request env is extended by `main.shared_env()` (keys already present win).
- `list(Some(pool))`, `capacity(pool)`: member of the pool; `list(None)`: all members, concatenated.
- `stop`, `claim`, `report`: the extra whose run ids start with `<runtime>:`, else main.
- `sweep`: every member, concatenated; one member's error is logged and the others still run.
- `plan`: main's steps, then each extra's (an extra's step ids start with `<runtime>:`); `apply`: each extra gets its prefixed steps, main the rest, outputs merged; `destroy`: extras first.
- `capabilities`: main's.

R9. Config: `providers` (object, default empty) maps a runtime name to that provider's settings; a pool whose `runtime` is a key of `providers` belongs to that provider; `DEFAULT_SLOTS` is 2.

R10. `core::cloud_init::render(env, agent_image, install_docker)`: the M3 template plus, first in `runcmd`, a 4 GiB swap file at `/swapfile` (skipped when it exists); `agent_image` is a full image reference. `CIBURST_AGENT_IMAGE` and the cache location keys come from `shared_env`.

R11. `Provider::shared_env()` (default empty). Scaleway answers `CIBURST_AGENT_IMAGE` (`<registry endpoint>/vm-agent:edge`) and, with an S3 cache, `CACHE_TYPE`, `CACHE_S3_SERVER_ADDRESS`, `CACHE_S3_BUCKET_NAME`, `CACHE_S3_BUCKET_LOCATION`, `CACHE_SHARED`. The cache keys travel in the request env as before.

R12. Provider `hetzner` (crate `ciburst-hetzner`, runtime `hetzner`, API `https://api.hetzner.cloud/v1`, `Authorization: Bearer <HCLOUD_TOKEN>`):
- Settings `providers.hetzner`: `location` (default `nbg1`), `image` (default `docker-ce`).
- Step `hetzner:firewall`: firewall `<name>-vm` without rules, applied to the label selector `ciburst-name==<name>`; create, noop, delete with the last VM pool.
- `start`: server name `<name>-<tag>-<8 hex>`, run id `hetzner:<server name>`; types from `instanceTypes`, default `["cx33", "cx43"]`, next type on `resource_unavailable`, `resource_limit_exceeded` or a placement error; the location and image above; IPv4 and IPv6 on; labels `ciburst=true`, `ciburst-name=<name>`, `ciburst-pool=<tag>`, `ciburst-job=<job>`, `ciburst-state=starting`, `ciburst-free=<slots - taken>`; user data from `core::cloud_init::render` with `CIBURST_RUN_ID` and `CIBURST_VM_TOKEN` (HMAC of the run id), known before create because the name is chosen; the VM secret never enters user data.
- `capacity`, `claim`, `report`, `sweep`, `list`, `stop` as on Scaleway, with labels `ciburst-state`, `ciburst-free`, `ciburst-seen`, `ciburst-drain` and `core::vm::sweep_reason`; `stop` deletes the server.
- `destroy`: every server with `ciburst-name==<name>`, then the firewall.

R13. Scaleway: `ScalewayLedger` over the cache bucket with the cache key, objects `ciburst/intents/<pool>/<unix s, 10 digits>-<job id, 20 digits>`, empty body; read with ListObjectsV2 `prefix` and `start-after`. The function builds it when the cache is S3 and its keys are set. The host writes `Secrets.provider_env` (for example `HCLOUD_TOKEN`) as secret environment variables of the function. `ScalewayJobs` ignores pools of other providers.
