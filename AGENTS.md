# AGENTS.md

Instructions for AI coding agents. Two tasks: adopting ciburst in a GitLab project, and changing this repository.

## Adopt ciburst in a GitLab project

Goal: the project's jobs run on ciburst: jobs that need a Docker daemon on VMs, the others in lanes. You change the project's repository; a human creates credentials and runs the manual jobs. Never create, read, print or store a secret.

1. Add the component to `.gitlab-ci.yml` on the default branch:
   ```yaml
   include:
     - component: gitlab.com/ciburst/ciburst/ciburst@main
   ```
   It sets `default: tags: [vm]` and adds the manual jobs `ciburst-setup`, `ciburst-check` and `ciburst-destroy` in stage `.pre`. Inputs, all optional: `pool` (`vm`), `version` (cli image tag, `edge`), `hosted_tag` (`saas-linux-small-amd64`). GitLab merges this `default:` with the project's own `default:` keys (`interruptible`, `image`); a `default: tags` in the project overrides it and keeps every job where it was.
2. Keep each job's `image:`, `services:` and `script:`. A job without `image:` would get `ubuntu:24.04` instead of GitLab's `ruby:3.1`: add `image:` where a job relied on the default.
3. Docker-in-Docker jobs may extend `.ciburst-docker` instead of their own `image: docker`, `docker:dind` service, `DOCKER_TLS_CERTDIR` and registry login. An existing template works unchanged when it uses the `docker:dind` service under its default alias `docker` with `DOCKER_TLS_CERTDIR: "/certs"`: VMs run jobs privileged and share `/certs/client` between job and service.
4. Jobs that must stay on GitLab's hosted runners keep or get `tags: [saas-linux-small-amd64]`, and their names go into `ignoreJobs` in `ciburst.json` (step 6), for example `"ignoreJobs": ["pages", "release-*"]`; without it their pending events start VMs. `ciburst check --suggest` prints the list.
5. A job with its own `tags:` runs where those tags point, not on ciburst: remove `tags:` from jobs that should move. A tag no runner serves leaves the job waiting forever.
6. Add `ciburst.json` at the root when the defaults do not fit or the project gets lanes (step 7). Each top-level key replaces the default's whole value (`docs/SPEC.md` section 5.4). Typical changes:
   - `provider.region` when the Scaleway organization has no Instance quota in `fr-par-1`, for example `"provider": { "type": "scaleway", "region": "nl-ams" }`. The whole deployment moves with the region, so caches stay next to the VMs; a `zone` outside the region is refused.
   - Pool size. The default, two slots on DEV1-M (3 vCPU, 4 GB, 4 GB swap), suits jobs of up to about 2 GB: unit tests, linters, small builds. Use `"slots": 1` with `"instanceTypes": ["DEV1-L", "DEV1-M"]` (8 GB, 4 vCPU per job) when a job does release builds with fat LTO, runs a browser together with a server, or runs Playwright with more than one worker. `maxRuns` caps VMs, so `slots` times `maxRuns` jobs run at once; the rest queue.
7. Lanes, for the jobs that need no Docker daemon (Scaleway Serverless Jobs in the job's own image, billed per second; README, Lanes):
   - Classify every job. It needs a Docker daemon when it has `services:`, extends `.ciburst-docker`, uses an image named `docker` (`docker:27`, any path ending in `/docker`), runs a script line starting with `docker `, or uses testcontainers. Such jobs stay on the VM pool; the others go to lanes.
   - Derive the lanes: with a token at hand, `ciburst check --suggest` prints them. Otherwise apply its rule by hand: group the Docker-free jobs by image (their own `image:` or the project's `default: image:`), with `$VAR` and `${VAR}` resolved from the pipeline's top-level `variables:`, and make one lane per image.
   - Add each lane to `pools` in `ciburst.json`, after the VM pool, which must stay because `pools` replaces the default's whole value: `{ "tag": "node24", "runtime": "jobs", "image": "node:24-bookworm-slim", "jobs": ["lint", "unit-*"], "vcpu": 2, "memoryMiB": 4096 }`. `tag`: lowercase letters, digits and hyphens. `jobs`: job names or patterns (`*` any characters, `?` one character); a pattern may appear in one pool only. Up to 6 vCPU and 16384 MiB; a Rust compile at 4 vCPU takes about half as long as on GitLab's small hosted runner.
   - Give each lane job `tags: [<lane tag>]`, and keep its `image:` exactly equal to the lane's `image`: a free VM slot takes lane jobs first and runs them in their `image:`, a lane run in the lane's.
   - Lane images: `linux/amd64` with `/bin/sh`, public or in the deployment's Scaleway registry. RPM-based images (UBI, Fedora) and images that run as a non-root user must already contain `git` and `curl`; Alpine, Debian and Ubuntu images get them installed. No `services:`, no Docker, no privileged mode; files in `/tmp` count against `memoryMiB`. Checkout, build outputs, image and cache archive share the run's 10 GB disk: a job that builds several GB (a Rust coverage build) stays on the VM pool.
8. If the project guards secret variable names (a check that fails when a protected secret is visible to the wrong jobs), add the four `CIBURST_*` names with their scope `ciburst`.
9. Hand the human this list, unchanged:
   - Scaleway: an IAM application with a policy on the organization holding `IAMManager`, `ProjectManager`, `InstancesFullAccess`, `ServerlessJobsFullAccess`, `FunctionsFullAccess`, `ObjectStorageFullAccess`, and an API key for it.
   - GitLab: a personal access token of a project Owner with the scopes `api` and `create_runner`.
   - Project variables `CIBURST_SCW_ACCESS_KEY`, `CIBURST_SCW_SECRET_KEY`, `CIBURST_SCW_ORGANIZATION_ID`, `CIBURST_GITLAB_TOKEN`: protected, "Masked and hidden", environment scope `ciburst`; the default branch protected.
   - Then run the manual job `ciburst-setup` once.

Verify:
- The CI file is valid: Pipeline editor, Validate; or `POST /projects/:id/ci/lint` with a token that may.
- After the setup: jobs run on the runner `ci-<project id>-vm`, lane jobs on `ci-<project id>-<lane tag>` or on a free VM slot; `ciburst-check` prints `every job matches the stage map`.
- Webhook: Settings, Webhooks, recent events show each delivery's answer with `ms` (under 10 s, or GitLab counts it failed); `ciburst status` with a token lists the same.
- The first pipeline starts with empty caches: the jobs' caches live in the deployment's own bucket, and GitLab's hosted-runner cache does not carry over.

Limits: `linux/amd64` images only; a VM ends after 24 hours at the latest. Job variables, file variables such as deploy keys included, reach the VM that runs the job, as on any runner; the runner deletes them after the job.

Undo, in this order: the human adds the project variable `CIBURST_DESTROY` with the deployment's name (`ci-<project id>`), protected, scope `ciburst`, and runs `ciburst-destroy`; its log ends with `destroyed <name>`. Only then remove the `include` and `ciburst.json`, and the human deletes the five variables. A plain revert of the include leaves the webhook, the runner and the cloud resources behind, every later job still starts a VM, and the `ciburst-destroy` job is gone with the include.

## Work on this repository

Read first: `docs/SPEC.md` (decision log in section 0, then the design), `docs/milestones/` (binding rules cited in code as `M4 rule R3`), `docs/PROVIDERS.md` (ports, adding a provider), `docs/RUNBOOK.md` (operations, failure modes).

Layout: `crates/core` (config, dispatcher, provisioning, ports; API fakes behind the `testing` feature), `crates/cli` (binary `ciburst`), `crates/scaleway`, `crates/hetzner`, `crates/dispatch-scaleway` (the function Scaleway compiles from source), `crates/vm-agent`, `templates/ciburst.yml`, `images/`, `scripts/`.

Before every commit, all green:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny check
scripts/shellcheck.sh
scripts/check-pins.sh
scripts/test-linux.sh          # Linux-only code, such as the VM agent's process test
images/test/lane-bootstrap.sh  # Docker, after a change to the lane bootstrap
```

Rules:
- Spec first: a design change gets a decision row in `docs/SPEC.md` section 0 before code.
- Every external API is tested against an in-process fake that is as strict as the real service. A live finding becomes a rule of the fake and a test that fails without the fix.
- No identifying data in the repository: no real hostnames, ids, emails or personal namespaces. The only real names allowed are `ciburst`, `gitlab.com/ciburst/ciburst`, `registry.gitlab.com/ciburst/ciburst` and `ciburst.zwa.la`. Secrets live in `.local/` (gitignored) or CI variables.
- No em or en dashes anywhere. Docs state facts, numbered steps with checks, and decisions; no narrative.
- Rust stays on 1.96.0: Scaleway compiles the function with the runtime `rust196`; `scripts/check-pins.sh` lists every pin.
- Commits are signed off (`git commit -s`, DCO).
