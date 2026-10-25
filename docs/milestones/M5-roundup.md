# M5: roundup, the adopter path and the developer experience

Go: owner, 2026-10-25 ("Push and start the project roundup for the final dev experience"). Spec D37 to D42. Base commit: named in each agent prompt.

## Scope

1. Adopter path: CI/CD component, `ciburst --ci`, outputs in project variables, `cli` image, the VM agent from the cache bucket, replay of waiting jobs, own jobs ignored, preflight.
2. Contributor path: the review fixes in R7 to R10.
3. Docs truth pass: README, RUNBOOK, SPEC sections 1 to 15, PROVIDERS; `SEED.md` and the M1 brief go.
4. Drills: the dev deployment on the agent object; an adoption drill in a fresh project; Hetzner once the token exists.

## Rules

R1. `--ci` (global flag). Config: the file at `--config` (default `ciburst.json` in the current directory) when it exists, else `{}`, merged over `Config::ci_defaults(project_id, server_url)` by top-level key (a key in the file replaces the default's whole value). Defaults: `name` `ci-<CI_PROJECT_ID>`; `gitlab` `{url: CI_SERVER_URL, projectIds: [CI_PROJECT_ID]}`; `host` `{type: scaleway-function, region: fr-par}`; `provider` `{type: scaleway, region: fr-par}`; `pools` one pool `vm` (runtime `instances`, image `ubuntu:24.04`, stages `[]`, slots 2, instanceTypes `DEV1-M`, `DEV1-L`, maxRuns 4); `defaultPool` `vm`; `cache` `{type: s3, expireDays: 30}`. Inputs from the process environment only: `CIBURST_SCW_ACCESS_KEY`, `CIBURST_SCW_SECRET_KEY`, `CIBURST_SCW_ORGANIZATION_ID`, optional `CIBURST_SCW_PROJECT_ID`, `CIBURST_GITLAB_TOKEN`, and `CIBURST_HCLOUD_TOKEN` with Hetzner pools; `SCW_*`, `GITLAB_TOKEN` and `--local` are not read. A missing input fails with the variable's name. `--ci` refuses to run unless `CI_COMMIT_REF_PROTECTED` is `true` (protected variables would be missing). Without `--ci` nothing changes.

R2. `GitlabOutputs` (core, `OutputsStore`) on project `CI_PROJECT_ID`. `read`: the output keys present in the process environment at start, overlaid by this run's writes. `write`: create or update a project variable per key with `protected: true`, `raw: true`, `environment_scope: "ciburst"`; create with `masked_and_hidden: true` and, when GitLab answers 400 about masking, create it unmasked and log `output_unmasked key=<key>`. `clear`: delete every output key (`provision::keys` and the runner keys of the config's pools) in scope `ciburst`. Keys of `keys::PROVIDER_ENV` stay in memory and are never written. The GitLab client gets variables (list paginated, create, update and delete with `filter[environment_scope]`) and pending jobs (`GET /projects/:id/jobs?scope[]=pending`, paginated, with `tag_list` and `created_at`); `FakeGitlab` gets both, as stingy as GitLab.

R3. VM agent from the cache bucket (D38), Scaleway provider. `with_agent(Some(path))` gives the Linux musl `ciburst-vm-agent`; step `vm:agent` exists whenever a pool has a VM runtime (any provider): `create` when `ciburst/agent/<sha256 of the binary>` is missing in the cache bucket, else `noop`; apply PUTs it with `x-amz-acl: public-read`. Without a binary the newest object under `ciburst/agent/` is current; none is an error naming `--agent` and `scripts/agent-binary.sh`. Every `apply` call returns `CIBURST_AGENT_SHA256` (current agent). Lifecycle: the single rule `ciburst-expire` becomes three rules with the configured days, prefixes `project/`, `runner/`, `ciburst/intents/`; an old document plans `update`. Cloud-init downloads `https://<bucket>.s3.<region>.scw.cloud/ciburst/agent/<sha256>` with curl, checks the hash and installs `/usr/local/bin/ciburst-vm-agent`; no image pull. `shared_env` answers `CIBURST_AGENT_URL`, `CIBURST_AGENT_SHA256` and the cache keys; `CIBURST_AGENT_IMAGE` goes. The host stores `Secrets.dispatcher_env` as plain environment variables; the function hands `CIBURST_AGENT_SHA256` to the provider. The registry step is needed only for `registry: true`; VM starts do not depend on it.

R4. Replay (D41), last step of `apply` with GitLab: for each project, every pending job whose `tag_list` contains a pool tag and whose `created_at` is more than 60 s old is posted to the dispatcher URL as `{object_kind: "build", build_status: "pending", project_id, build_id, build_name, build_stage, pipeline_id}` with `X-Gitlab-Event: Job Hook` and `X-Gitlab-Token`; log `replayed project=<id> job=<id> result=<result>`; a failure logs `replay_failed` and apply still succeeds.

R5. Own jobs (D41): a pending job whose `build_name` starts with `ciburst-` is answered `200 ignored`, reason `own_job`; `check` skips such jobs.

R6. Preflight (D42): `apply` calls `Provider::preflight` before any change; Scaleway fails when a VM pool's zone has no quota for any of its types (the message names zones with quota) or no agent is available (R3); Composite runs every member's. `plan` keeps warnings.

R7. Core and CLI fixes: provision writes `PROVIDER_ENV` and `DISPATCHER_ENV` keys with overwrite from every apply call; `check_capabilities` uses `capabilities_for` per pool (Composite answers with the member); `providers` keys must not be `instances`, `jobs`, `vm`, `key`, `definition`, `bucket`, `registry`, `project`, `iam`, `hook`, `runner`, `runner-enable`, `function`, `namespace`, `cron`; an output secret the config needs that is missing (dispatch key, cache key with an S3 cache, a pool's runner token) adds its rotation to plan and apply; VM pools require `cache.type: s3`; `check` needs only the config and GitLab, `status` no GitLab; errors name the file or variable and, for a 403 on `POST /user/runners`, the scope `create_runner` and the Owner role; `--agent <path>` (env `CIBURST_AGENT`, default `<local>/ciburst-vm-agent`, then `ciburst-vm-agent` next to the executable); help texts name `vm` for rotate and `hetzner.env` for `--local`; about text "GitLab runners that exist only while jobs run".

R8. VM contract harness `ciburst_core::testing::vm_contract::run_vm_provider_contract`: start with `taken` gives `free = slots - taken`; list and capacity show the run; claim lowers `free`; report sets state, free and seen; a silent VM is swept by `core::vm::sweep_reason`; runs of other pools and ids of other runtimes are ignored; stop ends the run. Run for Scaleway Instances, Hetzner and the core fake.

R9. Providers: `ScalewayJobs` is `Scaleway` (done in the contract); `crates/scaleway/src/provider.rs` split by concern without behaviour change; RFC 3339 and civil-date helpers in `ciburst_core::time`; `build.rs` vendors every path dependency of the function and skips `src/testing`.

R10. CI and images: `images/cli` (alpine, `ciburst` and `ciburst-vm-agent` musl binaries) replaces `images/vm-agent`; `cli:edge` on `main`, `cli:X.Y.Z`, `X.Y`, `X`, `latest` on tags; Rust jobs only when Rust, CI or script paths change; image jobs build and smoke-test without pushing in merge requests; pool images `needs: []`; separate cargo cache keys with a `main` fallback; `scripts/test-linux.sh` runs the CI test command in `rust:1.96.0-bookworm`; `scripts/agent-binary.sh [out]` writes the agent to `.local/ciburst-vm-agent`; a pin check in lint (Rust 1.96.0, gitlab-runner 18.11.4); `deny.toml` without unused allowances; the release script fails when `ciburst --version` differs from the tag.

R11. Component `templates/ciburst.yml` replaces `ci/ciburst.yml`. Inputs `pool` (default `vm`), `version` (cli image tag, default `edge`), `hosted_tag` (default `saas-linux-small-amd64`). Content: `default: tags: [<pool>]`; `.ciburst-docker` (`docker:27` with `docker:27-dind`, TLS certs, registry login, no stage); jobs `ciburst-setup` (`.pre`, `ciburst --ci apply`), `ciburst-check` (`.pre`, `ciburst --ci check`) and `ciburst-destroy` (`.pre`, `ciburst --ci destroy --yes "$CIBURST_DESTROY"`), each manual on the default branch when it is protected, `allow_failure: true`, image `registry.gitlab.com/ciburst/ciburst/cli:<version>`, `tags: [<hosted_tag>]`, `environment: {name: ciburst, action: prepare}`, `resource_group: ciburst`, `interruptible: false`. Fast lanes leave the include; the runbook shows them as config.

## Ownership

| Owner | Paths |
|---|---|
| A, core and CLI | `crates/core/**` except B's files, `crates/cli/**`, `test/config.fake.json`, `config.example.json` |
| B, providers | `crates/scaleway/**`, `crates/hetzner/**`, `crates/dispatch-scaleway/**`, `crates/core/src/{cloud_init.rs,cloud-init.yaml,time.rs,testing/vm_contract.rs}` and their `mod` lines |
| C, CI and images | `.gitlab-ci.yml`, `images/**`, `scripts/**`, `templates/**`, `ci/**`, `deny.toml` |
| D, docs | `README.md`, `docs/**` except this file and SPEC section 0 and status line, `SEED.md`, `test/drill/**` |
| Orchestrator | this file, SPEC section 0, `Cargo.toml`, `rust-toolchain.toml`, integration |

## Definition of done

`cargo fmt --check`, `clippy -D warnings`, `cargo test` (Windows and `scripts/test-linux.sh`), `cargo deny` green; the function zip builds; the dev deployment runs a cold drill pipeline on the agent object; a fresh project adopts through README and component alone, its second `ciburst-setup` plans only `noop`, jobs that waited start after setup, hosted minutes per setup recorded; docs describe the current design only; no identifying data, no em or en dash.
