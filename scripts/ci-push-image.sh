#!/usr/bin/env bash
# Tags and pushes the built cli image to the project registry as `cli`: `edge` on the default
# branch; X.Y.Z, X.Y, X and `latest` on a tag X.Y.Z; nothing elsewhere (merge request
# pipelines only build and test).
# Usage: scripts/ci-push-image.sh <local reference>
set -euo pipefail

source_ref="${1:?usage: ci-push-image.sh <local reference>}"

tags=()
if [[ "${CI_COMMIT_TAG:-}" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
  major="${BASH_REMATCH[1]}"
  minor="${BASH_REMATCH[2]}"
  patch="${BASH_REMATCH[3]}"
  tags=("$major.$minor.$patch" "$major.$minor" "$major" latest)
elif [ -n "${CI_COMMIT_BRANCH:-}" ] && [ "${CI_COMMIT_BRANCH}" = "${CI_DEFAULT_BRANCH:-}" ]; then
  tags=(edge)
else
  echo "not a release tag or the default branch: nothing to push"
  exit 0
fi

repo="${CI_REGISTRY_IMAGE:?CI_REGISTRY_IMAGE is not set}/cli"
printf '%s' "${CI_REGISTRY_PASSWORD:?CI_REGISTRY_PASSWORD is not set}" \
  | docker login --username "${CI_REGISTRY_USER:?CI_REGISTRY_USER is not set}" --password-stdin "${CI_REGISTRY:?CI_REGISTRY is not set}"

for tag in "${tags[@]}"; do
  docker tag "$source_ref" "$repo:$tag"
  docker push "$repo:$tag"
done
