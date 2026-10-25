#!/usr/bin/env bash
# Builds the build stage of images/cli/Dockerfile and copies the static x86_64 musl
# `ciburst-vm-agent` out of it, for `ciburst apply` from a workstation (`--agent`, or the
# default `.local/ciburst-vm-agent`).
# Usage: scripts/agent-binary.sh [out]. Default out: .local/ciburst-vm-agent in the repository.
set -euo pipefail

export MSYS_NO_PATHCONV=1

root="$(cd "$(dirname "$0")/.." && pwd)"
if command -v cygpath > /dev/null; then
  root="$(cygpath -m "$root")"
fi
out="${1:-$root/.local/ciburst-vm-agent}"
if command -v cygpath > /dev/null; then
  out="$(cygpath -m "$out")"
fi
ref="ciburst-local/cli-build:local"

mkdir -p "$(dirname "$out")"
start=$(date +%s)
docker build --platform linux/amd64 --target build --file "$root/images/cli/Dockerfile" --tag "$ref" "$root"
echo "build seconds: $(( $(date +%s) - start ))"

id="$(docker create --platform linux/amd64 "$ref")"
trap 'docker rm "$id" > /dev/null' EXIT
docker cp "$id:/out/ciburst-vm-agent" "$out"

"$root/scripts/check-static-elf.sh" "$out"
if command -v sha256sum > /dev/null; then
  sum="$(sha256sum "$out")"
else
  sum="$(shasum -a 256 "$out")"
fi
echo "sha256: ${sum%% *}"
echo "agent written to $out"
