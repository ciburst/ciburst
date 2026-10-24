# ciburst

Serverless per-job GitLab runners with pluggable providers. A GitLab job webhook reaches a small dispatcher; the dispatcher starts one serverless container per pending job; the container runs the job with `gitlab-runner run-single` and exits. Nothing runs or is reserved between jobs.

Status: M0, core and fake provider. The Scaleway provider and host, the pool images and the runbook are M1. See `docs/SPEC.md`.

## How it works

1. GitLab sends a job event to the dispatcher URL with a hook token.
2. The dispatcher keeps `pending` events of configured projects, maps the job's stage to a pool, and starts a run in that pool unless the pool is at `maxRuns`.
3. The run executes `gitlab-runner run-single` with the pool's runner token and the shell executor, keeps taking jobs while they arrive, and exits after `waitTimeoutS` idle seconds.
4. Each pool has its own GitLab runner and tag, so a job can only run in its pool's image.

## Quick start

1. Copy `config.example.json` to `config.json`; set `name`, `gitlab.projectIds`, pools and stages.
2. Put credentials into `.local/scaleway.env` (`SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID`) and `.local/gitlab.env` (`GITLAB_TOKEN` with scope `api`).
3. `node bin/provision.mjs plan`, then `node bin/provision.mjs apply`. Outputs land in `.local/outputs.env` (mode 600).
4. `node bin/provision.mjs check` compares every project's pipeline with the stage map.
5. In the consumer pipeline: `default: tags: [<default pool>]`; jobs of other pools set `tags: [<pool>]`; jobs that need a Docker daemon keep a hosted runner tag.

## Commands

| Command                           | Effect                                                                                     |
| --------------------------------- | ------------------------------------------------------------------------------------------ |
| `plan`                            | prints every step `apply` would run                                                        |
| `apply`                           | creates or updates provider resources, GitLab runners, the dispatcher and the hooks        |
| `check`                           | lints each project's pipeline and reports jobs whose tags disagree with their stage's pool |
| `status`                          | dispatcher URL, live runs per pool, last runs                                              |
| `rotate runner\|hook\|key\|cache` | replaces the secret and redeploys                                                          |
| `destroy --yes <name>`            | removes every resource                                                                     |

Options: `--config <path>` (default `config.json`), `--local <dir>` (default `.local`), `--skip-gitlab`.

## Dry run without a cloud

```
node bin/provision.mjs plan --config test/fixtures/config.fake.json --skip-gitlab
node bin/provision.mjs apply --config test/fixtures/config.fake.json --skip-gitlab
node bin/provision.mjs plan --config test/fixtures/config.fake.json --skip-gitlab
```

The second plan prints only `noop` lines. State lives in `.local/state.json`.

## Development

`npm install`, `npm test`, `npm run lint`, `npm run format`. Node 22 or later, no runtime dependencies. Providers and hosts: `docs/PROVIDERS.md`.

## License

Apache-2.0, see `LICENSE`.
