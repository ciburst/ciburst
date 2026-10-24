#!/usr/bin/env bash
# Prints `gitlab-runner run-single --help` of the pinned runner image.
# Usage: images/test/runner-help.sh [runner image]
set -euo pipefail

image="${1:-gitlab/gitlab-runner:v18.11.4}"
docker run --rm --entrypoint gitlab-runner "$image" run-single --help
