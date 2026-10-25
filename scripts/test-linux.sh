#!/usr/bin/env bash
# Runs the CI test command `cargo test --workspace --all-features` in the CI image
# rust:1.96.0-bookworm. The repository is mounted read-only; the cargo home and the target
# directory live in target/linux-ci/ and persist between runs. Extra arguments go to cargo test.
# Usage: scripts/test-linux.sh [cargo test arguments]
set -euo pipefail

export MSYS_NO_PATHCONV=1

root="$(cd "$(dirname "$0")/.." && pwd)"
user=()
if command -v cygpath > /dev/null; then
  root="$(cygpath -m "$root")"
elif [ "$(id -u)" -ne 0 ]; then
  user=(--user "$(id -u):$(id -g)" --env HOME=/tmp)
fi
mkdir -p "$root/target/linux-ci"

docker run --rm --platform linux/amd64 ${user[@]+"${user[@]}"} \
  --volume "$root:/src:ro" \
  --volume "$root/target/linux-ci:/src/target/linux-ci" \
  --workdir /src \
  --env CARGO_HOME=/src/target/linux-ci/cargo \
  --env CARGO_TARGET_DIR=/src/target/linux-ci/target \
  --env CARGO_TERM_COLOR=always \
  --env RUSTUP_TOOLCHAIN=1.96.0 \
  rust:1.96.0-bookworm \
  cargo test --workspace --all-features "$@"
