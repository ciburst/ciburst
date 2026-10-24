# ciburst

Serverless per-job GitLab runners with pluggable providers. A GitLab job webhook reaches a small dispatcher; the dispatcher starts one serverless container per pending job; the container runs the job with `gitlab-runner run-single` and exits. Nothing runs or is reserved between jobs.

Status: M1 done. Core, the Scaleway Serverless Jobs provider, the Scaleway Function host with the dispatcher, five pool images, CI and runbook exist and are tested against in-memory fakes. M2 is the first real deployment. See `docs/SPEC.md` and `docs/RUNBOOK.md`.

## How it works

1. GitLab sends a job event to the dispatcher URL with a hook token.
2. The dispatcher keeps `pending` events of configured projects, maps the job's stage to a pool, and starts a run in that pool unless the pool is at `maxRuns`.
3. The run executes `gitlab-runner run-single` with the pool's runner token and the shell executor, keeps taking jobs while they arrive, and exits after `waitTimeoutS` idle seconds.
4. Each pool has its own GitLab runner and tag, so a job can only run in its pool's image.

## Quick start

1. Copy `config.example.json` to `config.json` (gitignored); set `name`, `gitlab.projectIds`, pools and stages. Image tags: `1` after the first release, `edge` before.
2. Put credentials into `.local/scaleway.env` (`SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID`) and `.local/gitlab.env` (`GITLAB_TOKEN`, a personal access token of a project Owner with the scopes `api` and `create_runner`).
3. `ciburst plan`, then `ciburst apply`. Outputs land in `.local/outputs.env` (mode 600). The first apply compiles the dispatcher on Scaleway and takes several minutes.
4. `ciburst check` compares every project's pipeline with the stage map.
5. In the consumer pipeline include `ci/ciburst.yml`: `default: tags: [ci]`; jobs of other pools extend the `.ciburst-rust` or `.ciburst-playwright` templates; image builds extend `.ciburst-image-build`, which runs on GitLab's hosted runners.

## Commands

| Command | Effect |
|---|---|
| `plan` | prints every step `apply` would run |
| `apply` | creates or updates provider resources, GitLab runners, the dispatcher and the hooks |
| `check` | lints each project's pipeline and reports jobs whose tags disagree with their stage's pool |
| `status` | dispatcher URL, live runs per pool, last runs |
| `rotate runner\|hook\|key\|cache` | replaces the secret and redeploys |
| `destroy --yes <name>` | removes every resource |

Options: `--config <path>` (default `config.json`), `--local <dir>` (default `.local`), `--skip-gitlab`.

## Dry run without a cloud

```
cargo run -- plan --config test/config.fake.json --skip-gitlab
cargo run -- apply --config test/config.fake.json --skip-gitlab
cargo run -- plan --config test/config.fake.json --skip-gitlab
```

The second plan prints only `noop` lines. State lives in `.local/state.json`.

## Development

Rust 1.96 (`rust-toolchain.toml`). `cargo test --workspace --all-features`, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo deny check`, `scripts/shellcheck.sh`. Images: `images/test/smoke.sh <image>` with Docker. Providers and hosts: `docs/PROVIDERS.md`.

## License

Apache-2.0, see `LICENSE`.
