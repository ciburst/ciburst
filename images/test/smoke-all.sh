#!/usr/bin/env bash
# Runs smoke.sh for every pool image and prints one summary line per image.
set -uo pipefail

dir="$(cd "$(dirname "$0")" && pwd)"
summary=()
status=0
for image in base node rust playwright; do
  if output="$(SMOKE_QUIET=1 "$dir/smoke.sh" "$image" 2>&1)"; then
    result="passed"
  else
    result="FAILED"
    status=1
  fi
  echo "$output"
  seconds="$(sed -n 's/^build seconds: //p' <<< "$output")"
  bytes="$(sed -n 's/^image bytes: //p' <<< "$output")"
  summary+=("$image: $result, build ${seconds:-?} s, ${bytes:-?} bytes")
done
printf '%s\n' "${summary[@]}"
exit "$status"
