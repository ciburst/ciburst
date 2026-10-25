# ciburst

GitLab runners that exist only while jobs run. A GitLab job webhook reaches a dispatcher (a Scaleway Function); the dispatcher gives each waiting job a slot on a VM, starting VMs on Scaleway Instances when none is free; each VM runs its jobs with GitLab's Docker executor and ends itself once idle. Nothing runs between pipelines.

Design: `docs/SPEC.md`. Operations: `docs/RUNBOOK.md`. Providers: `docs/PROVIDERS.md`.

## What you get

- Jobs run with GitLab's Docker executor on a VM with its own Docker daemon, so `image:`, `services:`, Docker-in-Docker and `docker build` work as on GitLab's hosted runners.
- A VM takes two jobs at a time (two slots). Jobs that go pending together share VMs: five jobs start three VMs.
- Scaleway bills a VM per started hour, 60 minutes minimum. An idle VM ends at minute 55 of its hour; pipelines within that hour reuse it.
- About 0.03 EUR per DEV1-M hour (VM, IPv4, disk). A cold pipeline with five jobs costs about 0.09 EUR; further pipelines within the hour add nothing.
- DEV1-M has 3 vCPU and 4 GB shared by two slots, plus 4 GB swap (GitLab's small hosted runner gives 8 GB per job). For more per job, set `slots` to 1 or `instanceTypes` to `["DEV1-L"]` (example below).
- A job without `image:` runs in `ubuntu:24.04` (GitLab's hosted runners default to `ruby:3.1`).
- Optional: Hetzner Cloud CX33 VMs at about half the price, and fast lanes on Scaleway Serverless Jobs for jobs that need no Docker daemon (`docs/RUNBOOK.md`, sections Hetzner and Fast lanes).

## Adopt in under half an hour

About 12 minutes of work, then a one-time wait of 5 to 15 minutes while Scaleway compiles the dispatcher. While `ciburst/ciburst` is private (until v1.0.0), the adopting user needs access to it and the adopting project needs an entry in its CI/CD job token allowlist.

1. Scaleway API key (4 minutes). Console, IAM: create an application with a policy on the organization holding `IAMManager`, `ProjectManager`, `InstancesFullAccess`, `ServerlessJobsFullAccess`, `FunctionsFullAccess` and `ObjectStorageFullAccess` (add `ContainerRegistryFullAccess` only with `"registry": true`), then an API key for it.
   Check: you hold the access key, the secret key and the organization id (Organization settings).
2. GitLab token (2 minutes). As an Owner of the project: Edit profile, Personal access tokens, a token with the scopes `api` and `create_runner` and an expiry.
   Check: the token is listed with both scopes.
3. CI variables (3 minutes). Project, Settings, CI/CD, Variables: add `CIBURST_SCW_ACCESS_KEY`, `CIBURST_SCW_SECRET_KEY`, `CIBURST_SCW_ORGANIZATION_ID` and `CIBURST_GITLAB_TOKEN`, each protected, with visibility "Masked and hidden" and environment scope `ciburst`. Protect the default branch.
   Check: the list shows four protected variables with environment `ciburst`.
4. Include (2 minutes). Add to `.gitlab-ci.yml` on the default branch:
   ```yaml
   include:
     - component: gitlab.com/ciburst/ciburst/ciburst@main
   ```
   It sets `default: tags: [vm]`, so every job goes to the VM pool; Docker-in-Docker jobs extend `.ciburst-docker`. Inputs: `pool` (default `vm`), `version` (the `cli` image tag, default `edge`), `hosted_tag` (default `saas-linux-small-amd64`).
   Check: the pipeline shows the manual jobs `ciburst-setup`, `ciburst-check` and `ciburst-destroy`; the other jobs wait for a runner.
5. Setup (1 minute, then 5 to 15 minutes). Run `ciburst-setup`. On a GitLab hosted runner it creates the Scaleway resources, the GitLab runner and the webhook, waits for the dispatcher's deploy and posts the jobs that wait to it.
   Check: the waiting jobs start; done when jobs show the runner `ci-<project id>-vm`. A second `ciburst-setup` plans only `noop`.

Change the setup with `ciburst.json` at the repository root, then run `ciburst-setup` again. Each top-level key replaces the default's whole value (`docs/SPEC.md` section 5.4), so one slot per VM reads:

```json
{
  "pools": [
    { "tag": "vm", "runtime": "instances", "image": "ubuntu:24.04", "stages": [], "slots": 1, "instanceTypes": ["DEV1-M", "DEV1-L"], "maxRuns": 4 }
  ]
}
```

Undo: add the project variable `CIBURST_DESTROY` with the deployment's name (`ci-<project id>` unless `ciburst.json` sets `name`), protected, environment scope `ciburst`, then run `ciburst-destroy`. GitLab refuses variables typed into a manual job unless the project allows pipeline variables, so the name travels as a project variable.
Check: the job log ends with `destroyed <name>`; the webhook, the runner, the output variables and the Scaleway project `<name>` are gone. The four input variables stay until you delete them.

## Run from a workstation

1. `.local/` (gitignored): `config.json` from `config.example.json`, `scaleway.env` (`SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID`, optional `SCW_DEFAULT_PROJECT_ID`), `gitlab.env` (`GITLAB_TOKEN`), and `hetzner.env` (`HCLOUD_TOKEN`) with Hetzner pools.
2. `scripts/agent-binary.sh` builds the Linux musl VM agent into `.local/ciburst-vm-agent`, which `apply` uploads; `--agent <path>` names another build.
3. `cargo run -p ciburst -- plan --config .local/config.json`, then `apply`. Steps and checks: `docs/RUNBOOK.md`, Developer setup.

Dry run on the fake modules, no cloud:

```
cargo run -p ciburst -- plan --config test/config.fake.json --skip-gitlab
cargo run -p ciburst -- apply --config test/config.fake.json --skip-gitlab
cargo run -p ciburst -- plan --config test/config.fake.json --skip-gitlab
```

The second plan prints only `noop` lines. State lives in `.local/state.json`.

## Commands

| Command | Effect |
|---|---|
| `plan` | prints every step `apply` would run; warns when a VM pool could not start |
| `apply` | checks that every pool can start (quota, agent), creates or updates the provider resources, GitLab runners, the dispatcher and the webhooks, then posts waiting jobs to the dispatcher |
| `destroy --yes <name>` | removes webhooks, runners, the dispatcher, running VMs, the cache bucket, the registry namespace, the IAM applications, the outputs and the Scaleway project it created |
| `status` | dispatcher URL, live runs per pool and the last 20 runs; needs no GitLab token |
| `check [--ref <ref>]` | lints each project's pipeline (default branch, or a branch, tag or commit) against the stage map; exits 1 on any problem |
| `rotate runner\|hook\|key\|cache\|vm` | replaces that secret and redeploys; `vm` is refused while VMs run |

| Global option | Effect |
|---|---|
| `--ci` | pipeline mode: `ciburst.json` over defaults, credentials from `CIBURST_*` variables, outputs in project variables |
| `--config <path>` | config file, default `config.json` (`ciburst.json` with `--ci`) |
| `--local <dir>` | directory of the `.env` files, `outputs.env` and the agent, default `.local` |
| `--agent <path>` | VM agent binary to upload (env `CIBURST_AGENT`); default `<local>/ciburst-vm-agent`, then `ciburst-vm-agent` next to the executable |
| `--skip-gitlab` | leaves GitLab untouched in every command: `check` fails, `destroy` leaves webhooks and runners |

`GITLAB_TOKEN` in the environment overrides `.local/gitlab.env`.

## Development

Rust 1.96 (`rust-toolchain.toml`).

- `cargo test --workspace --all-features`; `scripts/test-linux.sh` runs the same command in `rust:1.96.0-bookworm`, Linux-only tests included (the VM agent's process test).
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo deny check`, `scripts/shellcheck.sh`, `scripts/check-pins.sh` (Rust and gitlab-runner versions agree everywhere).
- Fast-lane images: `images/test/smoke.sh <image>` with Docker.
- New providers and hosts: `docs/PROVIDERS.md`.

## License

Apache-2.0, see `LICENSE`.
