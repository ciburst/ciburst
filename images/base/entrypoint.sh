#!/bin/sh
# Pool image entrypoint: composes `gitlab-runner run-single` from the run's environment and execs it.
# With arguments it execs them instead. CIBURST_DRY_RUN=1 prints the command with secrets masked.
set -eu

if [ "$#" -gt 0 ]; then
  exec "$@"
fi

fail() {
  echo "ciburst-entrypoint: $1" >&2
  exit 64
}

require() {
  for name in "$@"; do
    eval "value=\${$name:-}"
    [ -n "$value" ] || fail "missing environment variable $name"
  done
}

require CI_SERVER_URL RUNNER_TOKEN

wait_timeout="${RUNNER_WAIT_TIMEOUT:-60}"
case "$wait_timeout" in
  '' | *[!0-9]*) fail "RUNNER_WAIT_TIMEOUT must be a whole number of seconds" ;;
esac

set -- gitlab-runner run-single \
  --url "$CI_SERVER_URL" \
  --token "$RUNNER_TOKEN" \
  --executor shell \
  --shell bash \
  --builds-dir "${CIBURST_BUILDS_DIR:-/builds}" \
  --cache-dir "${CIBURST_CACHE_DIR:-/cache}" \
  --max-builds 0 \
  --wait-timeout "$wait_timeout"

case "${CACHE_TYPE:-}" in
  '') ;;
  s3)
    require CACHE_S3_SERVER_ADDRESS CACHE_S3_BUCKET_NAME CACHE_S3_BUCKET_LOCATION \
      CACHE_S3_ACCESS_KEY CACHE_S3_SECRET_KEY
    set -- "$@" \
      --cache-type s3 \
      --cache-s3-server-address "$CACHE_S3_SERVER_ADDRESS" \
      --cache-s3-bucket-name "$CACHE_S3_BUCKET_NAME" \
      --cache-s3-bucket-location "$CACHE_S3_BUCKET_LOCATION" \
      --cache-s3-access-key "$CACHE_S3_ACCESS_KEY" \
      --cache-s3-secret-key "$CACHE_S3_SECRET_KEY"
    if [ "${CACHE_SHARED:-false}" = "true" ]; then
      set -- "$@" --cache-shared
    fi
    ;;
  *) fail "CACHE_TYPE must be s3 or empty" ;;
esac

if [ "${CIBURST_DRY_RUN:-0}" = "1" ]; then
  line=""
  previous=""
  for arg in "$@"; do
    case "$previous" in
      --token | --cache-s3-access-key | --cache-s3-secret-key) arg="[masked]" ;;
    esac
    line="$line${line:+ }$arg"
    previous="$arg"
  done
  echo "$line"
  exit 0
fi

unset RUNNER_TOKEN CACHE_S3_ACCESS_KEY CACHE_S3_SECRET_KEY
exec "$@"
