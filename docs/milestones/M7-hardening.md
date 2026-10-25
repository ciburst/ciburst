# M7, part 1: hardening

Go: owner, 2026-10-25 ("push then apply to the drill consumer project, then harden"). Spec D50 to D52. Going public (policy files, visibility, mirror, site, crates.io, v1.0.0) stays in M7 after this part. Base commit: named in each agent prompt.

## Findings this part answers

1. A VM start answered GitLab after 10.4 s (M6 drill); GitLab.com counts answers after 10 s as failed and disables a webhook after four failures in a row. The start path makes up to seven sequential calls on a function with 128 MB, which Scaleway gives a small CPU share, plus the ledger's 2 s wait.
2. A disabled webhook loses every pending event silently until someone notices; nothing in ciburst shows it.
3. Cockpit holds no function logs, so the only view of the dispatcher is the webhook's recent events page.
4. `check --suggest` lists the copies of a matrix job one by one (`e2e: [android]`, `e2e: [ios]`) and prints their `tags:` line per copy.

## Rules

R1. Elapsed time. Every dispatcher response body, on every route (job and pipeline events, `/vm`, `/terminate`, `/sweep`), carries `"ms": <whole milliseconds since the request arrived>`; every decision log line carries `ms` too. Tests use the dispatcher's clock, not wall time.

R2. Function size. `MEMORY_LIMIT_MB` becomes 1024; an existing function plans `update  function <name>  (settings, config or secrets changed)`.

R3. Start without lookups. `apply` resolves, with today's selection rules, the security group id of `<name>-vm` and per instance type of each VM pool the image and whether cloud-init installs Docker, and hands them to the function as plain variables `CIBURST_VM_SECURITY_GROUP` and `CIBURST_VM_IMAGES` (JSON object: commercial type to `{"image": "<id>", "installDocker": <bool>}`). `start_vm` uses them and makes no security group or image lookup. A type missing from them, or a create refused because the image or the security group no longer exists, falls back to today's lookups for that start and logs `warn start_lookup` with the reason. A changed resolution plans the function update of R2.

R4. Retries. On the start path a call that fails with 429 or 503, or for the idempotent calls (user data, power on) also with another 5xx or a transport timeout, is retried once after 300 ms while less than 5 s have passed since the request arrived; a create that timed out is not retried, because it may have created the server. Errors carry their kind (`Error::is_retryable` in core, set by the Scaleway client); the fakes can fail a call once with a chosen status.

R5. Webhook health. `Gitlab` reads `alert_status` and `disabled_until` of each project hook. A hook whose `alert_status` is not `executable` plans `update  gitlab hook in project <id>  (re-enable: <alert_status> until <disabled_until or never>)`; `apply` calls `POST /projects/:id/hooks/:hook_id/test/push_events`, logs `info hook_reenabled project="<id>"`, and replays waiting jobs as today. `FakeGitlab` models both fields, the test endpoint (it enables the hook) and a way for tests to disable a hook. `check` prints `project <id>: webhook <alert_status> until <time or never>: run ciburst-setup to enable it again` and exits 1.

R6. Deliveries. `ciburst status`, when a GitLab token is available, prints per project `hook <id>: <alert_status>` and up to the 20 newest deliveries of `GET /projects/:id/hooks/:hook_id/events` as `delivery <time> <job name or pipeline id> <http status> <result> <pool or -> <ms or ->`, read from the request and response bodies; an unreadable body prints `?`. Without a token, status prints what it printed before.

R7. Suggest matrix jobs. `check --suggest` gives a parallel or matrix job (`<name>: [<values>]` or `<name> <i>/<n>`) one pattern `<name>*` when no other job name of the pipeline starts with `<name>`, else its instance names, and prints its `tags:` line once, as `job <name>: tags: [<lane>]`.

R8. Docs: RUNBOOK (the `ms` field, re-enabling a webhook, `ciburst status` deliveries, failure modes, an M7 hardening drill section), SPEC sections 5 and 6 (function size, start lookups, retries), README command table (`status`), `AGENTS.md` verify list (`ciburst status` deliveries).

## Ownership

| Owner | Paths |
|---|---|
| A, core and CLI | `crates/core/**` except B's files (R1, R4 error kind and retry wrapper, R5, R6 parsing, R7, `FakeGitlab`), `crates/cli/**` |
| B, providers | `crates/scaleway/**` (R2, R3, R4 classification and fake failures), `crates/hetzner/**`, `crates/dispatch-scaleway/**`, `crates/core/src/{lane.rs,lane-bootstrap.sh,cloud_init.rs,cloud-init.yaml}` |
| Coordinator | docs (R8), merge, live drill |

## Definition of done

`cargo fmt --check`, `clippy -D warnings`, `cargo test` (Windows and `scripts/test-linux.sh`), `cargo deny`, shellcheck, check-pins and the bootstrap test green. Fake-based tests for each rule, among them: a VM start makes no image or security group lookup with the variables set and falls back when the image is refused; a 503 on create is retried once, a create timeout is not; a disabled hook plans the re-enable and `apply` re-enables it; `ms` is present on every route. Live on the demo: the setup plans the function update; every delivery shows `ms`; three VM starts answer within 6 s on a warm function and the first after the deploy within 9 s; `ciburst status` lists the deliveries. No identifying data, no em or en dash.
