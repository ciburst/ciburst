#!/usr/bin/env bash
# Runs shellcheck over every tracked *.sh file. Without a local shellcheck it uses the
# koalaman/shellcheck image through Docker.
set -euo pipefail

cd "$(dirname "$0")/.."
mapfile -t files < <(git ls-files '*.sh')
if [ "${#files[@]}" -eq 0 ]; then
  echo "no shell scripts found"
  exit 0
fi

if command -v shellcheck > /dev/null; then
  shellcheck --version | sed -n 2p
  shellcheck "${files[@]}"
else
  root="$(pwd)"
  if command -v cygpath > /dev/null; then
    root="$(cygpath -m "$root")"
  fi
  MSYS_NO_PATHCONV=1 docker run --rm -v "$root:/mnt:ro" -w /mnt koalaman/shellcheck:stable "${files[@]}"
fi
echo "shellcheck passed: ${#files[@]} files"
