#!/usr/bin/env bash
# Checks that every pinned upstream image in images/*/Dockerfile exists.
set -euo pipefail

cd "$(dirname "$0")/../.."
status=0
while read -r ref; do
  if [ "$ref" = "scratch" ]; then
    continue
  fi
  if docker manifest inspect "$ref" > /dev/null 2>&1; then
    echo "ok      $ref"
  else
    echo "missing $ref"
    status=1
  fi
done < <(sed -n -E 's/^FROM ([^ ]+).*/\1/p' images/*/Dockerfile | sort -u)
exit "$status"
