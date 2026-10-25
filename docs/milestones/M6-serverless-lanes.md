# M6: serverless lanes

Go: owner, 2026-10-25 ("Start measuring and implement the new strategy. I like it better than the old one."). Spec D45 to D47; going public becomes M7. Phase 0 measurements: `docs/RUNBOOK.md`, Measured. Base commit: named in each agent prompt.

## Scope

1. Lanes run the job's own image through `lane-bootstrap.sh` (D45); ciburst's pool images go.
2. Routing by job name (D46); `check` guards lanes and suggests them.
3. Paid VM slots first, lanes for the overflow (D47).
4. Drill: the demo and the drill consumer with lanes.

## Rules

R1. Config. Every pool may list `jobs`: glob patterns over the job name (`*` any run, `?` one character, no other syntax), each non-empty; a pattern in two pools is a validation error naming both. `Config::pool_for(name, stage)` replaces `pool_for_stage`: the first pool in config order with a matching pattern, else the stage map, else `defaultPool`, else none. `vmFirst` is valid on lanes (`runtime: jobs`) only. A lane's `image` is any image reference. Every config with a pool needs `cache.type: s3` (the runner object lives in the bucket).

R2. Dispatcher. Routing uses `pool_for(build_name, build_stage)`. For a lane with `vmFirst` (default true when a VM pool exists), the VM pool is the default pool when it is a VM pool, else the first VM pool. When that VM pool has a claim candidate (M4 rule R1), the job claims it: `200 {"result":"claimed","pool":"<vm tag>","lane":"<lane tag>","runId":"<run>"}`, log `claimed ... lane="<lane>"`. Otherwise the lane path of today: `started` below `maxRuns`, `at_capacity` above. Lanes never use the burst ledger.

R3. Runners. The GitLab runner of that VM pool carries its own tag plus the tag of every lane with `vmFirst`, in config order. `plan` shows `update` when the runner's tags differ; `apply` sets them (`PUT /runners/:id` with `tag_list`); `FakeGitlab` supports it as GitLab does.

R4. `check`. Each job routes like the dispatcher. New problems for a job routed to a lane: it has `services:`; its image name starts with `docker` (any registry path ending in `/docker` or `docker:`); a script line starts with `docker `; its `image:` differs from the lane's after resolving `$VAR` and `${VAR}` from the pipeline's top-level `variables:` (an unresolvable name is a problem that names it). `check --suggest` groups the Docker-free jobs routed to a VM pool by resolved image and prints, per image, a lane block for `ciburst.json` (`tag` from the image's last path segment and major version, `runtime` `jobs`, `image`, `jobs` with the job names, `vcpu` 2, `memoryMiB` 4096) and the `tags:` line each job needs; it exits 0.

R5. Lane runs (Scaleway). A lane's job definition has `image_uri` the lane image, `command` `core::lane::COMMAND`, and in its environment `CIBURST_LANE` (base64 of `core::lane::BOOTSTRAP`), `CIBURST_RUNNER_URL` and `CIBURST_RUNNER_SHA256`, besides today's keys; `plan` updates a definition when any of them changes. Runs start with today's overrides.

R6. Runner object. Provider step `runner-binary`, present whenever a pool exists: object `ciburst/runner/<cloud_init::RUNNER_SHA256>` in the cache bucket, public-read. `apply` downloads `cloud_init::RUNNER_URL`, refuses a hash other than `RUNNER_SHA256`, uploads; `noop` when the object exists. `shared_env` adds `CIBURST_RUNNER_URL`; VM cloud-init (Scaleway and Hetzner) downloads the runner from it with the same hash check.

R7. Images and CI. `images/base`, `images/node`, `images/rust`, `images/playwright`, their CI jobs, smoke tests and tag checks go; `scripts/ci-push-image.sh` keeps the `cli` image only. `images/test/lane-bootstrap.sh` runs the bootstrap in Docker with `--cap-drop=ALL`: dry run passes on `alpine:3.21`, `debian:bookworm-slim`, `node:24-bookworm-slim`, `ubuntu:24.04`; fails with its message on `registry.access.redhat.com/ubi9/ubi-minimal` and on `node:24-bookworm-slim` as user `node`; one full run with the real runner against an unreachable URL exits 0 after the idle timeout; a wrong hash fails. CI job `lane-bootstrap` runs it when the script or the test change.

R8. Docs: README (lanes, when to use them, price per minute), `AGENTS.md` (classify Docker-free jobs, `check --suggest`, lane tags), RUNBOOK (lanes, bootstrap failures, spillover answers), SPEC sections 1 to 15 and the milestone table (M6 lanes, M7 go public), PROVIDERS (lane runs, the runner object).

## Ownership

| Owner | Paths |
|---|---|
| A, core and CLI | `crates/core/**` except B's files, `crates/cli/**`, `config.example.json`, `test/config.fake.json` |
| B, providers | `crates/scaleway/**`, `crates/hetzner/**`, `crates/dispatch-scaleway/**`, `crates/core/src/{lane.rs,lane-bootstrap.sh,cloud_init.rs,cloud-init.yaml}` |
| C, CI and images | `.gitlab-ci.yml`, `images/**`, `scripts/**`, `templates/**`, `deny.toml` |
| D, docs | `README.md`, `AGENTS.md`, `docs/**` except this file, SPEC section 0 and its status line |

## Definition of done

`cargo fmt --check`, `clippy -D warnings`, `cargo test` (Windows and `scripts/test-linux.sh`), `cargo deny`, shellcheck and the bootstrap test green; a demo pipeline runs lane jobs from `node:24-bookworm-slim` and `rust:1.99.0-bookworm` on Serverless Jobs and claims a free VM slot first when one exists; the drill consumer runs with the lanes `check --suggest` proposes, wall clock and cost recorded against its 18-minute warm VM pipeline; docs describe lanes only as they work; no identifying data, no em or en dash.
