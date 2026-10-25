#!/bin/sh
# ciburst lane bootstrap (D45): a serverless run of a GitLab job's own image. Installs git, curl
# and CA certificates when the image lacks them, fetches the pinned gitlab-runner from the
# deployment's bucket, checks its SHA-256 and execs `run-single` with the shell executor.
# Builds and cache live in /builds and /cache when the user may create them, else below $HOME,
# else below /tmp; CIBURST_BUILDS_DIR and CIBURST_CACHE_DIR override each.
# CIBURST_LANE_DRY_RUN=1 installs the tools, skips the download and prints the command with
# secrets masked instead of running it.
set -eu

fail() {
  echo "ciburst-lane: $1" >&2
  exit 64
}

have() {
  command -v "$1" > /dev/null 2>&1
}

writable_home() {
  [ -n "${HOME:-}" ] && [ -d "$HOME" ] && [ -w "$HOME" ]
}

require() {
  for name in "$@"; do
    eval "value=\${$name:-}"
    [ -n "$value" ] || fail "missing environment variable $name"
  done
}

require CI_SERVER_URL RUNNER_TOKEN CIBURST_RUNNER_URL CIBURST_RUNNER_SHA256

missing=""
have git || missing="$missing git"
have curl || have wget || missing="$missing curl"
if [ ! -s /etc/ssl/certs/ca-certificates.crt ] && [ ! -s /etc/pki/tls/certs/ca-bundle.crt ] \
  && [ ! -s /etc/ssl/cert.pem ]; then
  missing="$missing ca-certificates"
fi

if [ -n "$missing" ]; then
  [ "$(id -u)" = 0 ] || fail "the image lacks$missing and runs as a non-root user"
  # shellcheck disable=SC2086 # $missing is a list of package names
  if have apk; then
    apk add --no-cache $missing > /dev/null 2>&1 || fail "apk could not install$missing"
  elif have apt-get; then
    # The sandbox drops root's capabilities, so apt keeps root and works in its own directories.
    apt_dir=/tmp/ciburst-apt
    mkdir -p "$apt_dir/lists/partial" "$apt_dir/archives/partial"
    set -- -o APT::Sandbox::User=root -o "Dir::State::Lists=$apt_dir/lists" \
      -o "Dir::Cache::Archives=$apt_dir/archives" -o Dir::Cache::pkgcache= -o Dir::Cache::srcpkgcache=
    { apt-get "$@" update -qq && DEBIAN_FRONTEND=noninteractive \
      apt-get "$@" install -y -qq --no-install-recommends $missing; } > /dev/null 2>&1 \
      || fail "apt-get could not install$missing"
    rm -rf "$apt_dir"
  elif have microdnf || have dnf; then
    # RPM transactions need capabilities the sandbox drops; such images must ship the tools.
    fail "the image lacks$missing; RPM-based images must contain them, RPM cannot install in the sandbox"
  else
    fail "the image lacks$missing and has no apk, apt-get, microdnf or dnf"
  fi
fi

bin_dir=/usr/local/bin
if [ ! -d "$bin_dir" ] || [ ! -w "$bin_dir" ]; then
  bin_dir=/tmp/.ciburst/bin
  if writable_home; then
    bin_dir="$HOME/.ciburst/bin"
  fi
fi
mkdir -p "$bin_dir"
runner="$bin_dir/gitlab-runner"
if [ "${CIBURST_LANE_DRY_RUN:-0}" != "1" ]; then
  if have curl; then
    curl -fsSL -o "$runner" "$CIBURST_RUNNER_URL" || fail "cannot download gitlab-runner"
  else
    wget -q -O "$runner" "$CIBURST_RUNNER_URL" || fail "cannot download gitlab-runner"
  fi
  echo "$CIBURST_RUNNER_SHA256  $runner" | sha256sum -c - > /dev/null 2>&1 \
    || fail "gitlab-runner does not match its SHA-256"
  chmod 0755 "$runner"
fi

shell="bash"
if ! have bash; then
  shell="sh"
fi
wait_timeout="${RUNNER_WAIT_TIMEOUT:-60}"
case "$wait_timeout" in
  '' | *[!0-9]*) fail "RUNNER_WAIT_TIMEOUT must be a whole number of seconds" ;;
esac

# The runner creates both directories. /tmp is memory in Serverless Jobs, so it comes last.
if [ "$(id -u)" = 0 ] || [ -w / ]; then
  builds_dir=/builds
  cache_dir=/cache
elif writable_home; then
  builds_dir="$HOME/builds"
  cache_dir="$HOME/cache"
else
  builds_dir=/tmp/ciburst-builds
  cache_dir=/tmp/ciburst-cache
fi

# The shell executor runs jobs in a login shell, whose /etc/profile replaces the image's PATH
# (rust images keep cargo in /usr/local/cargo/bin); the job gets it back as a variable.
set -- "$runner" run-single \
  --url "$CI_SERVER_URL" \
  --token "$RUNNER_TOKEN" \
  --executor shell \
  --shell "$shell" \
  --builds-dir "${CIBURST_BUILDS_DIR:-$builds_dir}" \
  --cache-dir "${CIBURST_CACHE_DIR:-$cache_dir}" \
  --max-builds 0 \
  --wait-timeout "$wait_timeout" \
  --env "PATH=$PATH"

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
      --cache-s3-secret-key "$CACHE_S3_SECRET_KEY" \
      --env FF_USE_FASTZIP=true \
      --env CACHE_COMPRESSION_LEVEL=fastest
    if [ "${CACHE_SHARED:-false}" = "true" ]; then
      set -- "$@" --cache-shared
    fi
    ;;
  *) fail "CACHE_TYPE must be s3 or empty" ;;
esac

if [ "${CIBURST_LANE_DRY_RUN:-0}" = "1" ]; then
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
