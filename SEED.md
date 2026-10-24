# Seed: on-demand GitLab runner on Scaleway (public project)

Paste this file into a fresh Claude Code conversation started in an empty directory, for example `C:\Users\<user>\Documents\git\gitlab-runner-scaleway-ondemand`. First prompt: "Read SEED.md, write the spec with the open questions and their defaults, then wait for my go." (Spec first, go before code, see the owner's rules below.)

## 1. Purpose

A public, MIT-licensed project that gives a GitLab.com project its own CI runner on Scaleway that costs nothing while idle: one Instance that sleeps (powered off, compute not billed), wakes for a pipeline, runs all jobs with the docker executor, and powers itself off again. Everything is provisioned and torn down by one idempotent script; nothing is clicked.

Why not GitLab's docker-autoscaler executor: it needs a fleeting plugin per cloud. Official plugins exist for AWS, GCP and Azure, community ones for Hetzner, OpenStack and Proxmox; none for Scaleway. The sleeping VM needs no plugin and keeps its caches between runs.

## 2. Owner's rules (apply as if they were CLAUDE.md)

- No personal or deployment-identifying data in the repository: no real hostnames, IPs, emails, names, organization or project ids, keys. Placeholders such as `<namespace>`, `<project-id>`; examples use fictitious values.
- Never em or en dashes anywhere (code, docs, commit messages, UI). Plain hyphens only.
- Docs are brief: facts, numbered steps with their check, decisions with their date. No rationale essays, no process notes, no fluff tables. README under about 150 lines.
- Config as code: every external setup with an API is a rerunnable script; secrets live in a gitignored `.local/` directory and in CI variables; clicks only where no API exists (document those as owner steps).
- Spec first: write `docs/SPEC.md` (brief) from this seed plus the open questions with defaults, wait for the owner's go, then implement. Push only after the owner confirms that specific push.
- Verified work over speed. State what was tested and how.
- No UI, so the visual design rules do not apply; keep CLI output plain.

## 3. Design

Components (all in the repository):

1. `provision.mjs` (Node 22+, zero dependencies, Scaleway REST API with `X-Auth-Token`, GitLab REST API with a private token). Idempotent: every step looks up by name or tag before creating. Steps:
   1. Scaleway project `<name>` (API `POST /account/v3/projects`) or an existing project id from the config.
   2. IAM application `<name>` with a policy scoped to that project only: permission set `InstancesFullAccess` (needed for poweron/poweroff; nothing in the organization outside the project is reachable). One API key; secret shown once, written to `.local/outputs.env`.
   3. Security group: inbound default drop, outbound accept, stateful, optional SSH rule from `ADMIN_CIDR`.
   4. Block Storage volume (default 60 GB, 5K IOPS) for root; flexible routed IPv4 (gitlab.com and registry.gitlab.com are IPv4 only).
   5. Instance (default `PLAY2-MICRO`, zone `fr-par-1`, Ubuntu 24.04 marketplace image resolved by label, `routed_ip_enabled`), cloud-init user data from `cloud-init/user-data.yaml` rendered with the runner token and the self-stop key, tags `<name>`.
   6. GitLab project runner via `POST /api/v4/user/runners` (`runner_type=project_type`, `project_id`, `tag_list`, `run_untagged=false`, `description`), token `glrt-...` passed to cloud-init once (never stored in the repo).
   7. Writes `.local/outputs.env`: `SCW_RUNNER_ZONE`, `SCW_RUNNER_SERVER_ID`, `SCW_RUNNER_WAKE_KEY` (the application's secret key) and prints the GitLab CI variables to create (masked). Flags: `--plan` (print what would change, no writes), `--destroy` (removes instance, volume, IP, security group, runner, IAM application; asks for the project name as confirmation), `--rotate-key`.
   8. Config from `.local/scaleway.env` (`SCW_ACCESS_KEY`, `SCW_SECRET_KEY`, `SCW_DEFAULT_ORGANIZATION_ID`, optional `SCW_DEFAULT_PROJECT_ID`) and `.local/gitlab.env` (`GITLAB_TOKEN` with scope `create_runner`, `GITLAB_PROJECT_ID`), plus `runner.config.json` in the repo (name, zone, instance type, volume size, tags, concurrent, idle minutes).
2. `cloud-init/user-data.yaml`: Docker CE from Docker's apt repo, gitlab-runner from GitLab's apt repo, unattended upgrades on, `gitlab-runner register --non-interactive --url https://gitlab.com --token <glrt> --executor docker --docker-image alpine:3 --docker-privileged --docker-volumes /cache`, config.toml: `concurrent = 3`, `listen_address = ":9252"`, `[runners.docker] privileged = true` (docker:dind services work), cache volume on the root disk. Installs `/usr/local/sbin/idle-stop.sh`, `/etc/scw-runner/env` (0600 root, the self-stop key, zone, server id) and a systemd timer every minute.
3. `vm/idle-stop.sh` (bash, shellcheck clean): reads `http://127.0.0.1:9252/metrics`, sums `gitlab_runner_jobs{...}`; when the sum has been 0 for `IDLE_MINUTES` (default 15) consecutive checks: `systemctl stop gitlab-runner` (waits for in-flight jobs, the runner stops accepting new ones), re-check the sum, then `POST /instance/v1/zones/<zone>/servers/<id>/action {"action":"poweroff"}` with the key. Scaleway stops compute billing only through the API or console poweroff; a guest `halt` keeps billing. The runner service is enabled, so the next poweron starts it again.
4. `ci/wake-runner.yml` for consumers to include:
   ```yaml
   default:
     tags: [ondemand]
   wake-runner:
     stage: .pre
     tags: [saas-linux-small-amd64]
     image: curlimages/curl:8
     variables:
       GIT_STRATEGY: none
     script:
       - sh -c 'state=$(curl -fsS -H "X-Auth-Token: $SCW_RUNNER_WAKE_KEY" "https://api.scaleway.com/instance/v1/zones/$SCW_RUNNER_ZONE/servers/$SCW_RUNNER_SERVER_ID" | sed -n "s/.*\"state\":\"\([a-z]*\)\".*/\1/p"); echo "runner $state"; if [ "$state" = "stopped" ]; then curl -fsS -X POST -H "X-Auth-Token: $SCW_RUNNER_WAKE_KEY" -H "Content-Type: application/json" -d "{\"action\":\"poweron\"}" "https://api.scaleway.com/instance/v1/zones/$SCW_RUNNER_ZONE/servers/$SCW_RUNNER_SERVER_ID/action"; fi'
   ```
   One shared compute minute per pipeline (about 10 s of work, billed as one minute). The three variables are masked CI variables. `stopping` state: wait up to 60 s, then poweron. All other jobs carry the default tag and wait for the runner (boot plus service start about 60 s).
5. Tests in the project's own CI (free, light): `node --test` for the plan builder against a mocked `fetch` (idempotency: a second run plans nothing), shellcheck for `vm/*.sh`, prettier for Markdown and YAML, a cloud-init `cloud-init schema` validation if available in a container. The real provisioning and the wake/sleep drill are manual and documented.

## 4. Facts verified on 2026-10-23 (recheck prices before publishing)

- Scaleway Instances are billed per hour; a powered-off Instance pays only for its volumes and its flexible IPv4. Prices: PLAY2-PICO 0.01428 EUR/h, PLAY2-NANO 0.02754 EUR/h, PLAY2-MICRO (4 shared vCPU, 8 GB) 0.05508 EUR/h, PRO2-XXS 0.0561 EUR/h, PRO2-XS 0.1122 EUR/h, POP2-HC-4C-8G 0.1064 EUR/h; flexible IPv4 0.005 EUR/h (about 3.65 EUR/month); Block Storage 5K about 0.00013 EUR/GB/h (60 GB about 5.70 EUR/month).
- Estimated cost for 30 pipelines a month, 45 minutes powered on each: about 11 EUR/month on PLAY2-MICRO, about 12 EUR/month on POP2-HC-4C-8G. GitLab.com: 400 free compute minutes per namespace and month; additional minutes 10 USD per 1,000, one-time, roll over; only instance (shared) runners consume them, project runners do not.
- GitLab runner metrics: `listen_address` in config.toml, gauge `gitlab_runner_jobs` with labels runner, state, stage, executor_stage.
- Sources: docs.gitlab.com/runner/executors/, docs.gitlab.com/runner/executors/docker_autoscaler, docs.gitlab.com/ci/pipelines/compute_minutes/, scaleway.com/en/docs/instances/reference-content/understanding-instance-pricing/, scaleway.com/en/docs/compute/instances/how-to/power-off-instance/, scaleway.com/en/pricing/virtual-instances/, scaleway.com/en/blog/a-transparent-update-on-scaleway-pricing/.

## 5. Open questions with defaults (put them in the spec, let the owner answer)

1. Project name and repo: default `gitlab-runner-scaleway-ondemand`, public project on GitLab.com under the owner's namespace (GitHub mirror later).
2. License: default MIT.
3. Implementation language of the provisioning: default Node 22 zero-dependency script (alternatives: OpenTofu module, Go CLI).
4. Instance type default: `PLAY2-MICRO`; `POP2-HC-4C-8G` when cargo-style builds dominate.
5. Idle window: default 15 minutes; optional hour-aware variant (stop only near the end of a started hour) if Scaleway rounds hours up: verify on the first invoice.
6. `concurrent`: default 3 on 4 vCPU.
7. Wake path: default the `.pre` job on shared runners; alternative a GitLab job-event webhook to a Scaleway Serverless Function (no shared minutes, more parts).
8. SSH access to the VM: default none (security group drops inbound); `ADMIN_CIDR` opens port 22 for one address.
9. Cache: default local disk only; optional `[runners.cache]` on Scaleway Object Storage later.
10. A job queued while the VM sleeps (for example after a manual gate longer than the idle window) waits for the next wake; remedy documented as re-running `wake-runner`. Accept, or add a scheduled wake every N minutes while a pipeline is running? Default accept.

## 6. Acceptance (the drill, documented in the README)

1. `node provision.mjs --plan` on an empty project lists every resource; `node provision.mjs` creates them; a second run plans nothing.
2. The GitLab project shows the runner online within 3 minutes of creation.
3. A pipeline with `ci/wake-runner.yml` included runs all jobs on the runner (job log header names the runner), including a `docker:dind` build job.
4. 15 minutes after the last job the Instance state is `stopped` (API and console); the next pipeline's `wake-runner` job starts it and the jobs run.
5. `node provision.mjs --destroy` removes everything; `--plan` afterwards lists every resource as missing.
6. Costs after one month match the estimate within the prices above.

## 7. Owner inputs for the new conversation

- `.local/scaleway.env` with a setup key that may create projects, IAM applications, policies, keys and Instances (delete the setup application afterwards; the runtime key is the project-scoped one the script makes).
- `.local/gitlab.env` with a personal access token (scope `create_runner`, expiry chosen) and the consumer project id.
- One purchased block of GitLab compute minutes (the wake job needs shared minutes).
- Answers to section 5.

## 8. Consumer side (done later in the consumer repository, not in this project)

Include `ci/wake-runner.yml` (or copy its two blocks), set the three masked variables, keep shared runners enabled for the project, and tag jobs that must stay on shared runners (`saas-linux-small-amd64`).
