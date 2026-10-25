#!/usr/bin/env bash
# Checks that the Rust toolchain and the gitlab-runner release carry the same pin everywhere.
# Sources: rust-toolchain.toml (Rust) and crates/core/src/cloud_init.rs (RUNNER_URL with the
# gitlab-runner version, RUNNER_SHA256; images/test/lane-bootstrap.sh reads both from there).
# Every place listed below must carry the pin, and no file outside docs/ and test/drill/ may
# name another version (the drill pipeline plays a consumer with its own toolchain). A version
# bump edits every listed place.
# Usage: scripts/check-pins.sh
set -euo pipefail

cd "$(dirname "$0")/.."
git rev-parse --is-inside-work-tree > /dev/null

rust="$(sed -n 's/^channel = "\([0-9]*\.[0-9]*\.[0-9]*\)"$/\1/p' rust-toolchain.toml)"
runner="$(sed -n '/^pub const RUNNER_URL: &str/,/;$/s|.*"https://gitlab-runner-downloads\.s3\.amazonaws\.com/v\([0-9]*\.[0-9]*\.[0-9]*\)/binaries/gitlab-runner-linux-amd64";$|\1|p' crates/core/src/cloud_init.rs)"
runner_sha="$(sed -n 's/^pub const RUNNER_SHA256: &str = "\([0-9a-f]\{64\}\)";$/\1/p' crates/core/src/cloud_init.rs)"
if [ -z "$rust" ] || [ -z "$runner" ] || [ -z "$runner_sha" ]; then
  echo "a pin source is unreadable: rust '$rust', runner '$runner', runner sha256 '$runner_sha'" >&2
  exit 1
fi
rust_minor="${rust%.*}"
runtime="rust${rust_minor/./}"
runner_url="https://gitlab-runner-downloads.s3.amazonaws.com/v$runner/binaries/gitlab-runner-linux-amd64"

places=(
  "rust-toolchain.toml|channel = \"$rust\""
  "Cargo.toml|rust-version = \"$rust_minor\""
  ".gitlab-ci.yml|image: rust:$rust-bookworm"
  ".gitlab-ci.yml|RUSTUP_TOOLCHAIN: $rust"
  "images/cli/Dockerfile|FROM rust:$rust-bookworm AS build"
  "scripts/test-linux.sh|rust:$rust-bookworm"
  "scripts/test-linux.sh|RUSTUP_TOOLCHAIN=$rust"
  "crates/scaleway/src/host.rs|pub const RUNTIME: &str = \"$runtime\";"
  "crates/scaleway/tests/host.rs|assert_eq!(function[\"runtime\"], \"$runtime\");"
  "crates/scaleway/tests/host.rs|rust-version = \\\"$rust_minor\\\""
  "images/test/runner-help.sh|gitlab/gitlab-runner:v$runner"
  "images/test/run-single-slot.sh|gitlab/gitlab-runner:v$runner"
  "crates/core/src/cloud_init.rs|$runner_url"
  "crates/core/src/cloud_init.rs|$runner_sha"
)

failures=0
fail() {
  echo "FAIL  $1" >&2
  failures=$((failures + 1))
}

for place in "${places[@]}"; do
  file="${place%%|*}"
  literal="${place#*|}"
  if [ ! -f "$file" ]; then
    fail "$file: missing (expected: $literal)"
  elif ! grep -qF -- "$literal" "$file"; then
    fail "$file: expected $literal"
  fi
done

# Every match of <extended regex> in tracked files (working tree content) outside docs/ and
# test/drill/ must carry <expected>; <kind> is `version` (the first X.Y or X.Y.Z of the match)
# or `exact`. Untracked files stay out: CI keeps CARGO_HOME, with crate sources, in the checkout.
scan() {
  local regex="$1" kind="$2" expected="$3" line file rest number match value
  while IFS= read -r line; do
    file="${line%%:*}"
    rest="${line#*:}"
    number="${rest%%:*}"
    match="${rest#*:}"
    value="$match"
    if [ "$kind" = "version" ] && [[ "$match" =~ [0-9]+\.[0-9]+(\.[0-9]+)? ]]; then
      value="${BASH_REMATCH[0]}"
    fi
    if [ "$value" != "$expected" ]; then
      fail "$file:$number: $match (pin: $expected)"
    fi
  done < <(git grep -n -o -E "$regex" -- . \
    ':(exclude)docs' ':(exclude)test/drill' ':(exclude)scripts/check-pins.sh' || true)
}

scan 'rust:[0-9]+\.[0-9]+(\.[0-9]+)?' version "$rust"
scan 'RUSTUP_TOOLCHAIN[:=] *"?[0-9]+\.[0-9]+(\.[0-9]+)?' version "$rust"
scan 'cargo [0-9]+\.[0-9]+\.[0-9]+' version "$rust"
scan 'rust-version = \\?"[0-9]+\.[0-9]+(\.[0-9]+)?' version "$rust_minor"
scan '"rust[0-9]{3,}"' exact "\"$runtime\""
scan 'gitlab-runner:v[0-9]+\.[0-9]+\.[0-9]+' version "$runner"
scan 'gitlab-runner-downloads\.s3\.amazonaws\.com/v[0-9]+\.[0-9]+\.[0-9]+' version "$runner"

# A file that names the runner download also names its sha256, as literal or as the constant.
while IFS= read -r file; do
  if ! grep -qF -e "$runner_sha" -e RUNNER_SHA256 "$file"; then
    fail "$file: downloads gitlab-runner without the sha256 $runner_sha"
  fi
done < <(git grep -l -F 'gitlab-runner-downloads.s3.amazonaws.com' -- . \
  ':(exclude)docs' ':(exclude)test/drill' ':(exclude)scripts/check-pins.sh' || true)

if [ "$failures" -ne 0 ]; then
  echo "$failures pin check(s) failed" >&2
  exit 1
fi
echo "pins ok: Rust $rust (runtime $runtime), gitlab-runner $runner (sha256 $runner_sha), ${#places[@]} places"
