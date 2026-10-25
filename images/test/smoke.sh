#!/usr/bin/env bash
# Builds the cli image and checks its two static binaries.
# Usage: images/test/smoke.sh [image reference]
# SMOKE_NO_BUILD=1 skips the build and tests an existing reference. On a tag vX.Y.Z (CI_COMMIT_TAG)
# the binaries must report X.Y.Z, else the workspace version of Cargo.toml.
set -euo pipefail

export MSYS_NO_PATHCONV=1

ref="${1:-ciburst-smoke/cli:local}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
if command -v cygpath > /dev/null; then
  root="$(cygpath -m "$root")"
fi

failures=0
pass() { echo "ok    $1"; }
fail() { echo "FAIL  $1" >&2; failures=$((failures + 1)); }

contains() {
  local label="$1" haystack="$2" needle="$3"
  if [[ "$haystack" == *"$needle"* ]]; then pass "$label"; else fail "$label: expected '$needle' in: $haystack"; fi
}

equals() {
  local label="$1" actual="$2" expected="$3"
  if [ "$actual" = "$expected" ]; then pass "$label"; else fail "$label: expected '$expected', got: $actual"; fi
}

if [ "${SMOKE_NO_BUILD:-0}" != "1" ]; then
  start=$(date +%s)
  docker build --platform linux/amd64 --file "$root/images/cli/Dockerfile" --tag "$ref" "$root"
  echo "build seconds: $(( $(date +%s) - start ))"
fi
echo "image bytes: $(docker image inspect --format '{{.Size}}' "$ref")"

version="$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml")"
if [[ "${CI_COMMIT_TAG:-}" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
  version="${BASH_REMATCH[1]}"
fi

out="$(docker run --rm "$ref" ciburst --version 2>&1)"
equals "ciburst --version" "$out" "ciburst $version"
out="$(docker run --rm "$ref" ciburst-vm-agent --version 2>&1)"
equals "ciburst-vm-agent --version" "$out" "ciburst-vm-agent $version"

# GitLab's Docker executor runs job scripts with the image's sh; the CLI finds the agent
# next to its own executable.
out="$(docker run --rm "$ref" sh -c 'command -v ciburst && command -v ciburst-vm-agent' 2>&1)"
equals "sh finds both binaries" "$out" "$(printf '/usr/local/bin/ciburst\n/usr/local/bin/ciburst-vm-agent')"

status=0
out="$(docker run --rm -e CIBURST_VM_ENV=/missing.env "$ref" ciburst-vm-agent 2>&1)" || status=$?
equals "agent without env file exits 64" "$status" "64"
contains "agent names the missing env file" "$out" 'error config path="/missing.env"'

# The agent runs on VMs of any distribution and the CLI on any Linux: both must be static.
bin_dir="$root/target/smoke-cli"
mkdir -p "$bin_dir"
id="$(docker create --platform linux/amd64 "$ref")"
for bin in ciburst ciburst-vm-agent; do
  docker cp "$id:/usr/local/bin/$bin" "$bin_dir/$bin"
done
docker rm "$id" > /dev/null
if out="$("$root/scripts/check-static-elf.sh" "$bin_dir/ciburst" "$bin_dir/ciburst-vm-agent" 2>&1)"; then
  pass "static x86-64 binaries"
  echo "$out"
else
  fail "static x86-64 binaries: $out"
fi

if [ "$failures" -ne 0 ]; then
  echo "$failures check(s) failed for the cli image" >&2
  exit 1
fi
echo "smoke passed for the cli image"
