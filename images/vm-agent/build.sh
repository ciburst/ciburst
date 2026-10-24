#!/usr/bin/env bash
# Builds the VM agent image (a static x86_64-unknown-linux-musl binary in a scratch image),
# copies the binary out as the VM's cloud-init does, and checks it.
# Usage: images/vm-agent/build.sh [image reference]. The binary lands in target/vm-agent/.
set -euo pipefail

export MSYS_NO_PATHCONV=1

ref="${1:-ciburst-smoke/vm-agent:local}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
if command -v cygpath > /dev/null; then
  root="$(cygpath -m "$root")"
fi
out="$root/target/vm-agent"

start=$(date +%s)
docker build --platform linux/amd64 --file "$root/images/vm-agent/Dockerfile" --tag "$ref" "$root"
echo "build seconds: $(( $(date +%s) - start ))"

mkdir -p "$out"
id="$(docker create --platform linux/amd64 "$ref")"
docker cp "$id:/ciburst-vm-agent" "$out/ciburst-vm-agent"
docker rm "$id" > /dev/null
echo "binary bytes: $(wc -c < "$out/ciburst-vm-agent" | tr -d ' ')"

# A scratch image has no dynamic loader: the binary only runs if it is static.
version="$(docker run --rm --platform linux/amd64 "$ref" --version)"
echo "$version"
case "$version" in
  "ciburst-vm-agent "*) ;;
  *) echo "unexpected --version output" >&2; exit 1 ;;
esac

status=0
output="$(docker run --rm --platform linux/amd64 -e CIBURST_VM_ENV=/missing.env "$ref" 2>&1)" || status=$?
if [ "$status" -ne 64 ] || [[ "$output" != *'error config path="/missing.env"'* ]]; then
  echo "missing env file: expected exit 64 and a config error, got $status: $output" >&2
  exit 1
fi
echo "vm-agent build passed"
