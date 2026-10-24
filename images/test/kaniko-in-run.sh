#!/usr/bin/env bash
# Runs inside the kaniko pool image, fed on stdin by smoke.sh: builds a multi-stage image from a
# context in the runner's builds dir with --no-push, then checks that the context and the
# runner's tools survive the build.
set -euo pipefail

project="${CIBURST_BUILDS_DIR:-/builds}/smoke/project"
mkdir -p "$project"
cd "$project"
git init -q .
echo "context file" > hello.txt
printf '%s\n' \
  'FROM alpine:3.22 AS build' \
  'RUN echo built > /out.txt' \
  'FROM busybox:musl' \
  'COPY --from=build /out.txt /out.txt' \
  'COPY hello.txt /hello.txt' \
  'RUN cat /out.txt /hello.txt' > Dockerfile

/kaniko/executor --context "$project" --dockerfile "$project/Dockerfile" --no-push

status=0
for check in "test -f $project/hello.txt" "test -d $project/.git" "git --version" "git lfs version" \
  "gitlab-runner --version" "bash --version" "curl --version" \
  "test -f /etc/ssl/certs/ca-certificates.crt"; do
  if eval "$check" > /dev/null 2>&1; then
    echo "after build ok    $check"
  else
    echo "after build FAIL  $check"
    status=1
  fi
done
exit "$status"
