#!/usr/bin/env bash
# Tags and pushes a built image to the project registry: `edge` on the default branch; X.Y.Z,
# X.Y, X and `latest` on a tag vX.Y.Z; nothing elsewhere (merge request pipelines only build
# and test). With CIBURST_SCW_REGISTRY (`rg.<region>.scw.cloud/<namespace>`) and
# CIBURST_SCW_REGISTRY_KEY (a Scaleway secret key) set, the same tags of a pool image also go to
# that registry; the cli image runs on GitLab's runners and stays in the project registry.
# Usage: scripts/ci-push-image.sh <local reference> <image name>
set -euo pipefail

source_ref="${1:?usage: ci-push-image.sh <local reference> <image name>}"
name="${2:?usage: ci-push-image.sh <local reference> <image name>}"

tags=()
if [[ "${CI_COMMIT_TAG:-}" =~ ^v([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
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

repos=("${CI_REGISTRY_IMAGE:?CI_REGISTRY_IMAGE is not set}/$name")
printf '%s' "${CI_REGISTRY_PASSWORD:?CI_REGISTRY_PASSWORD is not set}" \
  | docker login --username "${CI_REGISTRY_USER:?CI_REGISTRY_USER is not set}" --password-stdin "${CI_REGISTRY:?CI_REGISTRY is not set}"

if [ "$name" != "cli" ] && [ -n "${CIBURST_SCW_REGISTRY:-}" ] && [ -n "${CIBURST_SCW_REGISTRY_KEY:-}" ]; then
  printf '%s' "$CIBURST_SCW_REGISTRY_KEY" \
    | docker login --username nologin --password-stdin "${CIBURST_SCW_REGISTRY%%/*}"
  repos+=("$CIBURST_SCW_REGISTRY/$name")
fi

for repo in "${repos[@]}"; do
  for tag in "${tags[@]}"; do
    docker tag "$source_ref" "$repo:$tag"
    docker push "$repo:$tag"
  done
done
