#!/usr/bin/env bash
# Runs crates/core/src/lane-bootstrap.sh as a lane run does (D45): base64 encoded in the
# environment key SCRIPT_KEY and started by COMMAND, both from crates/core/src/lane.rs, in Docker
# with every capability dropped. The runner URL and its sha256 come from
# crates/core/src/cloud_init.rs. Images are pulled before a case starts, so the seconds are the
# bootstrap's own. Prints one line per case; exits 1 when a case ends otherwise than expected.
# The script travels in the environment, so this works against a remote daemon (dind) too.
# Usage: images/test/lane-bootstrap.sh
set -euo pipefail

export MSYS_NO_PATHCONV=1

root="$(cd "$(dirname "$0")/../.." && pwd)"
src="$root/crates/core/src"

# The value of `pub const <name>: &str = "...";` in <file>, on one line or wrapped by rustfmt.
rust_const() {
  local name="$1" file="$2" text bs="\\" q='"'
  text="$(awk -v name="$name" '
    $0 ~ "^pub const " name ": &str" { found = 1 }
    found { text = text $0; if ($0 ~ /;[ \t]*$/) { print text; exit } }
  ' "$file")"
  local literal='^[^"]*"(.*)";[[:space:]]*$'
  if [[ ! "$text" =~ $literal ]]; then
    echo "cannot read $name from $file" >&2
    return 1
  fi
  text="${BASH_REMATCH[1]}"
  text="${text//"$bs$q"/"$q"}"
  printf '%s\n' "${text//"$bs$bs"/"$bs"}"
}

runner_url="$(rust_const RUNNER_URL "$src/cloud_init.rs")"
runner_sha="$(rust_const RUNNER_SHA256 "$src/cloud_init.rs")"
command="$(rust_const COMMAND "$src/lane.rs")"
script_key="$(rust_const SCRIPT_KEY "$src/lane.rs")"
if [[ ! "$runner_url" =~ ^https:// ]] || [[ ! "$runner_sha" =~ ^[0-9a-f]{64}$ ]] \
  || [[ ! "$script_key" =~ ^[A-Z][A-Z0-9_]*$ ]] || [ -z "$command" ]; then
  echo "unexpected pin or lane constants: '$runner_url' '$runner_sha' '$script_key' '$command'" >&2
  exit 1
fi
# Scaleway splits the command at spaces; the first part replaces the image's entrypoint.
read -r -a command_parts <<< "$command"
lane="$(base64 < "$src/lane-bootstrap.sh" | tr -d '\n')"

token="glrt-lane-test-token"
secret="lane-test-secret-key"
access="SCWLANETESTACCESSKEY"
dry_run_env=(
  -e CIBURST_LANE_DRY_RUN=1
  -e CI_SERVER_URL=https://gitlab.example.com
  -e "CIBURST_RUNNER_SHA256=$runner_sha"
  -e CACHE_TYPE=s3
  -e CACHE_S3_SERVER_ADDRESS=s3.nl-ams.scw.cloud
  -e CACHE_S3_BUCKET_NAME=ci-cache-lane-test
  -e CACHE_S3_BUCKET_LOCATION=nl-ams
  -e "CACHE_S3_ACCESS_KEY=$access"
  -e "CACHE_S3_SECRET_KEY=$secret"
  -e CACHE_SHARED=true
)

cases=0
failures=0
ran=0
out=""
status=0
seconds=0
problems=""

# Runs the bootstrap in <image> with extra `docker run` options; sets ran, out, status and
# seconds.
bootstrap() {
  local image="$1" start
  shift
  ran=0
  out=""
  status=0
  seconds=0
  problems=""
  if ! out="$(docker pull --quiet --platform linux/amd64 "$image" 2>&1)"; then
    problems="cannot pull $image"
    return
  fi
  out=""
  start=$SECONDS
  out="$(docker run --rm --platform linux/amd64 --cap-drop=ALL \
    -e "$script_key=$lane" -e "RUNNER_TOKEN=$token" -e "CIBURST_RUNNER_URL=$runner_url" \
    "$@" --entrypoint "${command_parts[0]}" "$image" "${command_parts[@]:1}" 2>&1)" || status=$?
  seconds=$((SECONDS - start))
  ran=1
}

problem() {
  problems="${problems:+$problems; }$1"
}

expect_status() {
  if [ "$ran" -eq 1 ] && [ "$status" -ne "$1" ]; then
    problem "exit $status, expected $1"
  fi
}

expect_text() {
  if [ "$ran" -eq 1 ] && [[ "$out" != *"$1"* ]]; then
    problem "missing '$1'"
  fi
}

expect_no_text() {
  if [ "$ran" -eq 1 ] && [[ "$out" == *"$1"* ]]; then
    problem "unexpected '$1'"
  fi
}

report() {
  cases=$((cases + 1))
  if [ -z "$problems" ]; then
    printf 'ok    %-62s %3d s\n' "$1" "$seconds"
  else
    failures=$((failures + 1))
    printf 'FAIL  %-62s %3d s: %s\n' "$1" "$seconds" "$problems"
    if [ -n "$out" ]; then
      printf '      %s\n' "${out//$'\n'/$'\n      '}" >&2
    fi
  fi
}

# A dry run in <image> with extra `docker run` options: it passes and masks the secrets.
dry_run() {
  local image="$1"
  shift
  bootstrap "$image" "$@" "${dry_run_env[@]}"
  expect_status 0
  expect_text "gitlab-runner run-single"
  expect_text "--url https://gitlab.example.com"
  expect_text "--token [masked]"
  expect_text "--cache-s3-access-key [masked]"
  expect_text "--cache-s3-secret-key [masked]"
  expect_no_text "$token"
  expect_no_text "$access"
  expect_no_text "$secret"
}

for image in alpine:3.21 debian:bookworm-slim node:24-bookworm-slim ubuntu:24.04; do
  dry_run "$image"
  report "dry run on $image"
done

# The job keeps the image's PATH, which the shell executor's login shell would replace.
dry_run alpine:3.21 -e PATH=/opt/lane-test/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
expect_text "--env PATH=/opt/lane-test/bin:/usr/local/sbin"
report "dry run on alpine:3.21 keeps the image's PATH"

# An image with git and curl runs as its own user, with the builds in its home.
dry_run node:24-bookworm --user node
expect_text "--builds-dir /home/node/builds"
report "dry run on node:24-bookworm as user node"

bootstrap alpine:3.21 -e CIBURST_LANE_DRY_RUN=1 -e CI_SERVER_URL=https://gitlab.example.com
expect_status 64
expect_text "ciburst-lane: missing environment variable CIBURST_RUNNER_SHA256"
report "refused without CIBURST_RUNNER_SHA256"

bootstrap registry.access.redhat.com/ubi9/ubi-minimal "${dry_run_env[@]}"
expect_status 64
expect_text "ciburst-lane: the image lacks"
expect_text "RPM-based images must contain them"
report "refused on registry.access.redhat.com/ubi9/ubi-minimal"

bootstrap node:24-bookworm-slim --user node "${dry_run_env[@]}"
expect_status 64
expect_text "ciburst-lane: the image lacks"
expect_text "runs as a non-root user"
report "refused on node:24-bookworm-slim as user node"

bootstrap alpine:3.21 -e CI_SERVER_URL=http://127.0.0.1:9 -e RUNNER_WAIT_TIMEOUT=2 \
  -e "CIBURST_RUNNER_SHA256=$runner_sha"
expect_status 0
expect_text "has not received a job"
report "full run on alpine:3.21 against an unreachable GitLab"

bootstrap alpine:3.21 -e CI_SERVER_URL=http://127.0.0.1:9 -e RUNNER_WAIT_TIMEOUT=2 \
  -e "CIBURST_RUNNER_SHA256=$(printf '%064d' 0)"
expect_status 64
expect_text "ciburst-lane: gitlab-runner does not match its SHA-256"
report "wrong sha256 on alpine:3.21"

if [ "$failures" -ne 0 ]; then
  echo "lane bootstrap: $failures of $cases cases failed" >&2
  exit 1
fi
echo "lane bootstrap passed: $cases cases"
