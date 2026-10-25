# ciburst

GitLab CI runners that exist only while your jobs run, in your own cloud account.

A job webhook reaches a small dispatcher, a Scaleway Function. The dispatcher puts each waiting job on a VM with a free slot, or starts a VM. The VM runs the job with GitLab's Docker executor and ends itself when it is idle at minute 55 of its paid hour. Nothing runs between pipelines, and jobs keep GitLab's full feature set: `image:`, `services:`, Docker-in-Docker and `docker build`.

Design: `docs/SPEC.md`. Operations: `docs/RUNBOOK.md`. Providers: `docs/PROVIDERS.md`. AI agents: `AGENTS.md`.

Status: pre-release. `ciburst/ciburst` stays private until v1.0.0; the component and images track `main` (`@main`, image tag `edge`).

## How it works

1. GitLab sends every job event of the project to the dispatcher.
2. A pending job claims a free slot on a running VM. Without one it starts a VM; jobs that wait together share VMs (two slots each, so five jobs start three VMs).
3. Each VM runs one `gitlab-runner run-single` per slot and reports its state to the dispatcher. An idle VM ends at minute 55; a sweep every 30 minutes removes VMs that stopped reporting.
4. Setup, check and teardown run in your own pipeline on GitLab's hosted runners, from three manual jobs of the CI/CD component.

## Cost and limits

- About 0.03 EUR per Scaleway DEV1-M VM hour (VM, IPv4, disk), billed per started hour. A cold pipeline with five jobs costs about 0.09 EUR; pipelines within the same hour reuse the VMs.
- A cold job waits 60 to 100 s for its VM; on a running VM about 3 s.
- The first setup takes 6 to 15 hosted-runner minutes while Scaleway compiles the dispatcher; later setups take under a minute.
- DEV1-M: 3 vCPU and 4 GB shared by two slots, plus 4 GB swap. A job without `image:` runs in `ubuntu:24.04`. `linux/amd64` only.
- Optional: Hetzner CX33 VMs at about half the price, and fast lanes on Scaleway Serverless Jobs (`docs/RUNBOOK.md`).

## Adopt

You need a Scaleway account with a payment method and the Owner role in the GitLab project. While `ciburst/ciburst` is private, the adopting user also needs access to it and the adopting project an entry in its CI/CD job token allowlist.

1. Scaleway key. Console, IAM: an application with a policy on the organization holding `IAMManager`, `ProjectManager`, `InstancesFullAccess`, `ServerlessJobsFullAccess`, `FunctionsFullAccess` and `ObjectStorageFullAccess`; then an API key for it.
   Check: you hold its access key, its secret key and the organization id.
2. GitLab token. Edit profile, Access tokens: a personal access token with the scopes `api` and `create_runner`, with an expiry.
   Check: the token lists both scopes.
3. Variables. Project, Settings, CI/CD, Variables: `CIBURST_SCW_ACCESS_KEY`, `CIBURST_SCW_SECRET_KEY`, `CIBURST_SCW_ORGANIZATION_ID`, `CIBURST_GITLAB_TOKEN`; each protected, visibility "Masked and hidden", environment scope `ciburst`. The default branch must be protected.
   Check: four variables with environment `ciburst`.
4. Include. In `.gitlab-ci.yml` on the default branch:
   ```yaml
   include:
     - component: gitlab.com/ciburst/ciburst/ciburst@main
   ```
   Every job without its own `tags` now runs on a ciburst VM. Docker-in-Docker jobs extend `.ciburst-docker`; jobs that must stay on GitLab's runners keep `tags: [saas-linux-small-amd64]` (their pending events still wake a VM).
   Check: the pipeline shows the manual jobs `ciburst-setup`, `ciburst-check` and `ciburst-destroy`; the other jobs wait.
5. Setup. Run `ciburst-setup`.
   Check: the waiting jobs start on the runner `ci-<project id>-vm`. Running it again changes nothing.

Customize with `ciburst.json` at the repository root, then run `ciburst-setup` again. Each top-level key replaces the default's whole value (`docs/SPEC.md` section 5.4). Example, another zone and one job per VM:

```json
{
  "provider": { "type": "scaleway", "region": "fr-par", "zone": "nl-ams-1" },
  "pools": [
    { "tag": "vm", "runtime": "instances", "image": "ubuntu:24.04", "stages": [], "slots": 1, "instanceTypes": ["DEV1-M", "DEV1-L"], "maxRuns": 4 }
  ]
}
```

Undo: add the project variable `CIBURST_DESTROY` with the deployment's name (`ci-<project id>` unless `ciburst.json` sets `name`), protected, environment scope `ciburst`, then run `ciburst-destroy`.
Check: the log ends with `destroyed <name>`; the webhook, the runner, the output variables and the Scaleway project are gone. Then remove the include and `ciburst.json`, and delete the five variables. Removing the include first leaves the webhook behind, and every later job still starts a VM.

## With an AI assistant

`AGENTS.md` tells coding agents how to adopt ciburst in a repository and how to work on this one. Prompt for an agent in your project:

> Adopt ciburst in this GitLab project as https://gitlab.com/ciburst/ciburst/-/blob/main/AGENTS.md describes. Change only the repository, do not handle secrets, and list the tokens and CI variables I have to create.

## Run from a workstation

1. `.local/` (gitignored): `config.json` from `config.example.json`; `scaleway.env` with `SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID`; `gitlab.env` with `GITLAB_TOKEN`.
2. `scripts/agent-binary.sh` builds the VM agent into `.local/ciburst-vm-agent`.
3. `cargo run -p ciburst -- plan --config .local/config.json`, then `apply`. Details: `docs/RUNBOOK.md`, Developer setup.

Without a cloud: `cargo run -p ciburst -- apply --config test/config.fake.json --skip-gitlab` runs against in-memory fakes.

| Command | Effect |
|---|---|
| `plan` | every step `apply` would run |
| `apply` | checks that each pool can start, creates or updates everything, then starts the jobs that wait |
| `status` | dispatcher URL, live runs per pool, the last 20 runs |
| `check [--ref <ref>]` | lints each project's pipeline against the pools; exits 1 on a problem |
| `rotate runner\|hook\|key\|cache\|vm` | replaces that secret and redeploys |
| `destroy --yes <name>` | removes every resource of the deployment |

Global options: `--ci` (pipeline mode), `--config <path>`, `--local <dir>`, `--agent <path>`, `--skip-gitlab`.

## Development

Rust 1.96.0 (`rust-toolchain.toml`). Before a commit: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `cargo deny check`, `scripts/shellcheck.sh`, `scripts/check-pins.sh`; `scripts/test-linux.sh` for Linux-only code. New providers: `docs/PROVIDERS.md`.

## License

Apache-2.0, see `LICENSE`.
