#!/usr/bin/env bash
# Builds one pool image and checks the runner, the toolchain and the entrypoint.
# Usage: images/test/smoke.sh <base|node|rust|playwright|kaniko> [image reference]
# SMOKE_NO_BUILD=1 skips the build and tests an existing reference.
set -euo pipefail

export MSYS_NO_PATHCONV=1

name="${1:?usage: smoke.sh <image> [reference]}"
ref="${2:-ciburst-smoke/$name:local}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
if command -v cygpath > /dev/null; then
  root="$(cygpath -m "$root")"
fi
runner_version="18.11.4"

user="runner"
builds_dir="/builds"
case "$name" in
  base) toolchain=(git --version) expect_tool="git version" ;;
  node) toolchain=(node --version) expect_tool="v24." ;;
  rust) toolchain=(cargo --version) expect_tool="cargo 1.96.0" ;;
  playwright) toolchain=(npx --yes playwright@1.64.1 --version) expect_tool="Version 1.64.1" ;;
  kaniko) toolchain=(/kaniko/executor version) expect_tool="Kaniko version" user="root" builds_dir="/kaniko/builds" ;;
  *) echo "unknown image: $name" >&2; exit 2 ;;
esac

failures=0
pass() { echo "ok    $1"; }
fail() { echo "FAIL  $1" >&2; failures=$((failures + 1)); }

contains() {
  local label="$1" haystack="$2" needle="$3"
  if [[ "$haystack" == *"$needle"* ]]; then pass "$label"; else fail "$label: expected '$needle' in: $haystack"; fi
}

lacks() {
  local label="$1" haystack="$2" needle="$3"
  if [[ "$haystack" != *"$needle"* ]]; then pass "$label"; else fail "$label: unexpected '$needle'"; fi
}

if [ "${SMOKE_NO_BUILD:-0}" != "1" ]; then
  start=$(date +%s)
  docker build ${SMOKE_QUIET:+--quiet} --platform linux/amd64 --file "$root/images/$name/Dockerfile" --tag "$ref" "$root/images"
  echo "build seconds: $(( $(date +%s) - start ))"
fi
echo "image bytes: $(docker image inspect --format '{{.Size}}' "$ref")"

out="$(docker run --rm "$ref" gitlab-runner --version 2>&1)"
contains "gitlab-runner --version" "$out" "Version:      $runner_version"

out="$(docker run --rm "$ref" "${toolchain[@]}" 2>&1)"
contains "${toolchain[*]}" "$out" "$expect_tool"

out="$(docker run --rm "$ref" id -un 2>&1)"
contains "user" "$out" "$user"

if [ "$name" = "rust" ]; then
  out="$(docker run --rm "$ref" cargo clippy --version 2>&1)"
  contains "cargo clippy --version" "$out" "clippy"
  out="$(docker run --rm "$ref" cargo fmt --version 2>&1)"
  contains "cargo fmt --version" "$out" "rustfmt"
fi

sample=(
  -e CIBURST_DRY_RUN=1
  -e CIBURST_POOL="$name"
  -e CI_SERVER_URL=https://gitlab.example.com
  -e RUNNER_TOKEN=glrt-smoke-token
  -e RUNNER_WAIT_TIMEOUT=45
  -e CACHE_TYPE=s3
  -e CACHE_S3_SERVER_ADDRESS=s3.fr-par.scw.cloud
  -e CACHE_S3_BUCKET_NAME=ci-cache-smoke
  -e CACHE_S3_BUCKET_LOCATION=fr-par
  -e CACHE_S3_ACCESS_KEY=SCWSMOKEACCESSKEY
  -e CACHE_S3_SECRET_KEY=smoke-secret-key
  -e CACHE_SHARED=true
)
out="$(docker run --rm "${sample[@]}" "$ref" 2>&1)"
expected=(
  "gitlab-runner run-single"
  "--url https://gitlab.example.com"
  "--token [masked]"
  "--executor shell"
  "--shell bash"
  "--builds-dir $builds_dir"
  "--cache-dir /cache"
  "--max-builds 0"
  "--wait-timeout 45"
  "--cache-type s3"
  "--cache-s3-server-address s3.fr-par.scw.cloud"
  "--cache-s3-bucket-name ci-cache-smoke"
  "--cache-s3-bucket-location fr-par"
  "--cache-s3-access-key [masked]"
  "--cache-s3-secret-key [masked]"
  "--cache-shared"
)
for flag in "${expected[@]}"; do
  contains "dry run $flag" "$out" "$flag"
done
lacks "dry run masks the token" "$out" "glrt-smoke-token"
lacks "dry run masks the secret key" "$out" "smoke-secret-key"

status=0
docker run --rm -e CI_SERVER_URL=https://gitlab.example.com -e CIBURST_DRY_RUN=1 "$ref" > /dev/null 2>&1 || status=$?
if [ "$status" -ne 0 ]; then pass "missing RUNNER_TOKEN exits $status"; else fail "missing RUNNER_TOKEN exits 0"; fi
out="$(docker run --rm -e CI_SERVER_URL=https://gitlab.example.com -e CIBURST_DRY_RUN=1 "$ref" 2>&1 || true)"
contains "missing RUNNER_TOKEN is named" "$out" "missing environment variable RUNNER_TOKEN"

out="$(docker run --rm "${sample[@]}" -e CACHE_S3_SECRET_KEY= "$ref" 2>&1 || true)"
contains "missing CACHE_S3_SECRET_KEY is named" "$out" "missing environment variable CACHE_S3_SECRET_KEY"

out="$(docker run --rm -e CI_SERVER_URL=http://127.0.0.1:9 -e RUNNER_TOKEN=glrt-smoke-token -e RUNNER_WAIT_TIMEOUT=2 "$ref" 2>&1 || true)"
contains "run-single starts and exits after the idle window" "$out" "has not received a job"

if [ "$name" = "kaniko" ]; then
  if docker run --rm -i "$ref" bash -s < "$root/images/test/kaniko-in-run.sh"; then
    pass "multi-stage kaniko build keeps the context and the runner tools"
  else
    fail "multi-stage kaniko build"
  fi
fi

if [ "$failures" -ne 0 ]; then
  echo "$failures check(s) failed for $name" >&2
  exit 1
fi
echo "smoke passed for $name"
