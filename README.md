# ![ciburst](brand/logo-tile.svg)

Serverless GitLab runners in your own cloud account.

Jobs that need no Docker daemon can run as Scaleway Serverless Jobs in their own image, billed per second. Jobs that need one get a VM with GitLab's Docker executor, so `services:` and `docker build` work as on GitLab's runners; the VM ends itself when it is idle at minute 55 of its paid hour. Between pipelines nothing runs.

Site: [ciburst.zwa.la](https://ciburst.zwa.la), built from this README and `docs/`.

Design: `docs/SPEC.md`. Operations: `docs/RUNBOOK.md`. Providers: `docs/PROVIDERS.md`. AI agents: `AGENTS.md`.

Example: [ciburst-adopt-demo](https://gitlab.com/ciburst/ciburst-adopt-demo), a small project before and after adopting ciburst. Its second commit, `Adopt ciburst`, is the whole change, and its pipelines show which job ran on a VM and which in a lane.

Status: 1.0.0. The component is in the GitLab CI/CD Catalog: `@1` is the newest 1.x release and runs the `cli` image of that release; `@main` runs the image `edge`.

## How it works

1. GitLab sends every job and pipeline event of each project to the dispatcher, a Scaleway Function.
2. Each project routes its own jobs by name: a job in its `ignoreJobs` runs on other runners and starts nothing; a job that a lane of its `ciburst.json` names runs in that lane; every other job runs on the VM runner. `ciburst check --suggest` prints lanes for the jobs that need no Docker daemon.
3. A lane job starts a Serverless Job of its project's lane in the lane's image, which runs it and exits after 60 idle seconds; billed per second.
4. When a pipeline starts and any of its jobs will need a VM, one VM boots right away if none has a free slot, so a Docker job in a later stage finds it ready (`warmStart`, on by default; it costs one VM-hour, 0.03 EUR, when the pipeline ends before that job runs).
5. A pending VM job claims a free slot on a running VM. When every slot is busy it waits in a queue, and GitLab hands it to the first slot that frees; ciburst buys another VM only when the wait would pass `maxQueueS` (default 90 s, about one VM boot), judged by how long the running jobs usually take. With no VM running, jobs that wait together share new VMs (two slots each, so five jobs start three VMs).
6. Each VM runs one `gitlab-runner run` for its slots, serving every project of the setup, and reports its state to the dispatcher. An idle VM ends at minute 55; a sweep every 30 minutes removes VMs that stopped reporting.
7. Setup, check and teardown run in your own pipeline on GitLab's hosted runners, from manual jobs of the CI/CD component.

## Cost and limits

- VMs: about 0.03 EUR per Scaleway DEV1-M hour (VM, IPv4, disk), billed per started hour. A cold pipeline with five jobs costs about 0.09 EUR; pipelines within the same hour reuse the VMs.
- A cold job waits 60 to 100 s for its VM; on a running VM about 3 s.
- DEV1-M: 3 vCPU and 4 GB shared by two slots, plus 4 GB swap. A job without `image:` runs in `ubuntu:24.04`.
- DEV1 vCPUs are shared: CPU-bound jobs (compiles, lint and typecheck, browser tests) take 1.3 to 4 times as long as on GitLab's small hosted runner, even on a DEV1-L of their own; network-bound and short jobs are on par.
- Lanes: 0.00001 EUR per vCPU-second and 0.000002 EUR per GB-second, so about 0.0017 EUR per minute at 2 vCPU and 4 GB (0.10 EUR per hour) and 0.0034 EUR per minute at 4 vCPU and 8 GB (0.20 EUR per hour). A run also bills its bootstrap and its idle `waitTimeoutS` (60 s by default) before it exits. The first 200,000 vCPU-seconds and 400,000 GB-seconds of each month are free, about 28 hours at 2 vCPU and 4 GB.
- A lane run starts in 5 to 12 s for small images, 19 to 28 s for Rust and Playwright images, 93 s at 6 vCPU. Installing missing `git` and `curl` adds 2 to 4 s on Alpine and about 21 s on Debian and Ubuntu slim images.
- Lane speed: a Rust compile at 2 vCPU takes as long as on GitLab's small hosted runner, at 4 vCPU about half as long; a TypeScript check is within 12 %.
- The first setup takes 6 to 15 hosted-runner minutes while Scaleway compiles the dispatcher; later setups take under a minute.
- `linux/amd64` only.
- A job cache over 5 GiB is not saved (Scaleway's S3 copy limit); the job still passes and logs `Failed to create cache`.

Compared per job-minute for a small runner (providers' price pages, 2026-10-25, 1 EUR = 1.1193 USD; sources in `docs/SPEC.md` section 16):

| Option | EUR per job-minute | Billing |
|---|---|---|
| GitLab.com hosted, small (2 vCPU, 8 GB) | 0.0089 after 400 free minutes a month | per minute |
| AWS CodeBuild GitLab runner, small | 0.0045 | per job, rounded up to minutes |
| Self-built on AWS spot, m6i.large | 0.0009, plus about 10 EUR a month for the manager | per second |
| RocketRunner small, one always-on VM | 9.46 EUR a month flat | per minute, capped |
| ciburst VM slot, half a DEV1-M, half the paid hour used | 0.00045 | per started hour |
| ciburst VMs, measured: the drill consumer's 3 pipelines on one DEV1-L per job | 0.0046 | per started hour, VMs busy about 20 % of it |
| ciburst lane, 2 vCPU | 0.0017, free up to about 1,700 minutes a month | per second |

A month of 3,000 job-minutes: GitLab 23.23 EUR, self-built AWS spot 12.95, RocketRunner 9.46, ciburst lanes 2.24, ciburst VM slots 1.36. A VM slot has 1.5 shared vCPU and 2 GB, GitLab's small runner 2 vCPU and 8 GB.

VMs reach their per-minute price only when pipelines keep them busy through their paid hours. A project with a few pipelines a day pays mostly for idle minutes until minute 55; its Docker-free jobs cost less in lanes, which bill per second and start inside the free tier.

## Adopt

You need a Scaleway account with a payment method and the Owner role in the GitLab project. The commit `Adopt ciburst` of [ciburst-adopt-demo](https://gitlab.com/ciburst/ciburst-adopt-demo) shows the repository side on a real project: the include, a `ciburst.json` with two lanes, and the lane jobs' tags.

1. Scaleway key. Console, IAM: an application with a policy on the organization holding `IAMManager`, `ProjectManager`, `InstancesFullAccess`, `ServerlessJobsFullAccess`, `FunctionsFullAccess` and `ObjectStorageFullAccess`; then an API key for it.
   Check: you hold its access key, its secret key and the organization id.
2. GitLab token. Edit profile, Access tokens: a personal access token with the scopes `api` and `create_runner`, with an expiry.
   Check: the token lists both scopes.
3. Variables. Project, Settings, CI/CD, Variables: `CIBURST_SCW_ACCESS_KEY`, `CIBURST_SCW_SECRET_KEY`, `CIBURST_SCW_ORGANIZATION_ID`, `CIBURST_GITLAB_TOKEN`; each protected, visibility "Masked and hidden", environment scope `ciburst`. The default branch must be protected.
   Check: four variables with environment `ciburst`.
4. Include. In `.gitlab-ci.yml` on the default branch:
   ```yaml
   include:
     - component: gitlab.com/ciburst/ciburst/ciburst@1
   ```
   Every job without its own `tags` now runs on the VM runner. Docker-in-Docker jobs extend `.ciburst-docker`. Jobs that must stay on GitLab's runners keep `tags: [saas-linux-small-amd64]` and go into `ignoreJobs` of `ciburst.json` (below), or their pending events start VMs.
   Check: the pipeline shows the manual jobs `ciburst-setup`, `ciburst-check`, `ciburst-destroy` and `ciburst-invite`; the other jobs wait.
5. Setup. Run `ciburst-setup`.
   Check: the waiting jobs start on the runner `ci-<project id>-vm`. Running it again changes nothing.

Customize with `ciburst.json` at the repository root, then run `ciburst-setup` again. Each top-level key replaces the default's whole value; a key left out of `vm` keeps its default (`docs/SPEC.md` section 5.4). Example, the Amsterdam region, one job per VM, and a job that stays on GitLab's runners:

```json
{
  "provider": { "type": "scaleway", "region": "nl-ams" },
  "vm": { "slots": 1 },
  "ignoreJobs": ["pages"]
}
```

Undo: add the project variable `CIBURST_DESTROY` with the deployment's name (`ci-<project id>` unless `ciburst.json` sets `name`), protected, environment scope `ciburst`, then run `ciburst-destroy`.
Check: the log ends with `destroyed <name>`; the webhook, the runners, the output variables and the Scaleway project are gone. Then remove the include and `ciburst.json`, and delete the five variables. Removing the include first leaves the webhook behind, and every later job still starts a VM.

## Join an existing setup

One setup serves many projects, from any group or organization: they share its dispatcher and its VM runner, so a VM's paid hour serves all of them. Each project keeps its own lanes and `ignoreJobs`. Jobs of all projects share the VMs, and VM jobs run privileged: join only projects you trust.

1. In the project that ran `ciburst-setup` (the host): add the project variable `CIBURST_INVITE` with the joining project's id or path (protected, environment scope `ciburst`), run `ciburst-invite` and download `join-code.txt` from its artifacts (Maintainers and Owners can; the code never enters the log). The code is valid for 7 days; treat it as a secret.
   Check: the log reads `join code for project <id>, valid until <time>, in the artifact join-code.txt`.
2. In the joining project: include the component with `role: member`:
   ```yaml
   include:
     - component: gitlab.com/ciburst/ciburst/ciburst@1
       inputs:
         role: member
   ```
3. Optional: the joining project's own `ciburst.json` with only `lanes` and `ignoreJobs` (section Lanes); fleet settings such as `vm` or `provider` belong to the host and are refused.
4. Add two project variables, protected, "Masked and hidden", environment scope `ciburst`: `CIBURST_JOIN` (the code) and `CIBURST_GITLAB_TOKEN` (a personal access token of an Owner of this project with `api` and `create_runner`). No Scaleway credentials.
5. Run the manual job `ciburst-join` (about a minute).
   Check: its log prints `lane <tag> (<image>)` per lane and ends with `joined fleet <name>`; Settings, CI/CD, Runners lists `<name>-vm` and one runner per lane; the next pipeline runs on them.

A change of the member's lanes or `ignoreJobs` takes effect with the next `ciburst-join`, with a code that has not expired.

Leave: run `ciburst-leave` in the member; it removes the member's lanes, runners, webhook and record. The host's `ciburst-destroy` refuses while members are joined.

## Lanes

A lane is one project's Scaleway Serverless Jobs definition for one image and size, with its own runner and tag in that project. Each run starts from the lane's image, installs `git` and `curl` when the image lacks them, fetches the pinned `gitlab-runner` from the deployment's bucket and runs the job with the shell executor. A lane takes only its own project's jobs.

- Keep on the VM runner: jobs with `services:`, Docker-in-Docker, `docker build` or testcontainers, and steady load that keeps paid VM hours busy.
- Move to a lane: Docker-free jobs (lint, type checks, unit tests, compiles, browser tests) that would leave a VM idle for most of its paid hour, or that need more CPU than a DEV1 slot has.

Example `ciburst.json`: a lane for the jobs `lint`, `typecheck` and every `unit-*`.

```json
{
  "lanes": [
    { "tag": "node24", "image": "node:24-bookworm-slim", "jobs": ["lint", "typecheck", "unit-*"], "vcpu": 2, "memoryMiB": 4096, "maxRuns": 10 }
  ]
}
```

`jobs` patterns match the job name (`*` any characters, `?` one character); a pattern belongs to one lane. Each lane job carries the lane's tag instead of the component's default, and its `image:` equals the lane's `image`, because the lane run ignores `image:` and runs the lane's:

```yaml
unit-api:
  image: node:24-bookworm-slim
  tags: [node24]
  script:
    - npm ci
    - npm test
```

Lane images: `linux/amd64` with `/bin/sh`, public or in the deployment's Scaleway registry. RPM-based images (UBI, Fedora) and images that run as a non-root user must already contain `git` and `curl`. A run gets up to 6 vCPU, 16 GB memory, 10 GB disk and 24 hours, with at most 400 runs per organization at once; it has no Docker daemon and no privileged mode, root has no capabilities (`chown` fails: extract archives with `tar --no-same-owner`), and `/tmp` lives in memory.

`ciburst check --suggest` proposes lanes: per image of the Docker-free jobs on the VM runner, a lane for `ciburst.json` and the `tags:` line each job needs, and the `ignoreJobs` list of the jobs that run on other runners. `ciburst check` and the `ciburst-check` job report a lane job with `services:`, a Docker image, a `docker` command, or an image other than its lane's. Details: `docs/RUNBOOK.md`, Lanes.

## With an AI assistant

`AGENTS.md` tells coding agents how to adopt ciburst in a repository, how to join a project to an existing setup, and how to work on this one. Prompt for an agent in your project:

> Adopt ciburst in this GitLab project as https://gitlab.com/ciburst/ciburst/-/blob/main/AGENTS.md describes. Change only the repository, do not handle secrets, and list the tokens and CI variables I have to create.

## Run from a workstation

1. `.local/` (gitignored): `config.json` from `config.example.json`; `scaleway.env` with `SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID`; `gitlab.env` with `GITLAB_TOKEN`.
2. `scripts/agent-binary.sh` builds the VM agent into `.local/ciburst-vm-agent`.
3. `cargo run -p ciburst -- plan --config .local/config.json`, then `apply`. Details: `docs/RUNBOOK.md`, Developer setup.

Without a cloud: `cargo run -p ciburst -- apply --config test/config.fake.json --skip-gitlab` runs against in-memory fakes.

Without a clone: `cargo install --git https://gitlab.com/ciburst/ciburst --tag <version> --locked ciburst` builds the command line of a release, enough for `check`, `status` and `invite`; `apply` also needs that release's `ciburst-vm-agent` (Linux x86_64, linked on the release page), passed with `--agent`.

| Command | Effect |
|---|---|
| `plan` | every step `apply` would run |
| `apply` | checks that the VM runner can start a VM, creates or updates everything, then starts the jobs that wait |
| `status` | dispatcher URL, live runs of the VM runner and of each project's lanes, the members, the last 20 runs; with a GitLab token each webhook's state and its last 20 deliveries |
| `check [--ref <ref>] [--suggest]` | lints each project's pipeline against its own lanes and `ignoreJobs` and exits 1 on a problem; `--suggest` prints lanes for the Docker-free jobs and exits 0, or 1 when a pipeline cannot be read |
| `rotate runner\|hook\|key\|cache\|vm` | replaces that secret and redeploys |
| `invite <project>` | a join code for another project |
| `destroy --yes <name>` | removes every resource of the deployment |

Global options: `--ci` (pipeline mode), `--config <path>`, `--local <dir>`, `--agent <path>`, `--skip-gitlab`. `join` and `leave` run only with `--ci`, in a member's pipeline.

## Development

Rust 1.96.0 (`rust-toolchain.toml`). Before a commit: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `cargo deny check`, `scripts/shellcheck.sh`, `scripts/check-pins.sh`; `scripts/test-linux.sh` for Linux-only code, `images/test/lane-bootstrap.sh` after a change to the lane bootstrap. The site: `cargo run -p ciburst-site -- public` builds it from this README, `AGENTS.md` and `docs/` (`docs/SPEC.md` section 11.5). The logo: `cargo run -p ciburst-brand -- svg` writes `brand/`, `cargo run -p ciburst-brand -- png target/brand` renders it as PNG (section 11.6). New providers: `docs/PROVIDERS.md`. Contributions: `CONTRIBUTING.md` (Developer Certificate of Origin, `git commit -s`); conduct: `CODE_OF_CONDUCT.md`; vulnerabilities: `SECURITY.md`.

## License

Apache-2.0, see `LICENSE` and `NOTICE`.
