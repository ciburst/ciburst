#!/usr/bin/env bash
# Release on a tag X.Y.Z: builds the static `ciburst` and `ciburst-vm-agent` binaries for
# x86_64-unknown-linux-musl, fails when `--version` of either differs from the tag, uploads both
# with their checksums to the generic package registry and writes release-notes.md, the
# description the release job's `release` keyword links them from.
set -euo pipefail

target="x86_64-unknown-linux-musl"
tag="${CI_COMMIT_TAG:?CI_COMMIT_TAG is not set}"
if [[ ! "$tag" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "not a release tag X.Y.Z: $tag" >&2
  exit 1
fi
version="$tag"
package_url="${CI_API_V4_URL:?CI_API_V4_URL is not set}/projects/${CI_PROJECT_ID:?CI_PROJECT_ID is not set}/packages/generic/ciburst/${tag}"
binaries=(ciburst ciburst-vm-agent)

apt-get update
apt-get install -y --no-install-recommends musl-tools
rustup target add "$target"
cargo build --release --locked --target "$target" --package ciburst --package ciburst-vm-agent

mkdir -p dist
for bin in "${binaries[@]}"; do
  file="$bin-$target"
  cp "target/$target/release/$bin" "dist/$file"
  reported="$("dist/$file" --version)"
  if [ "$reported" != "$bin $version" ]; then
    echo "dist/$file --version prints '$reported'; the tag $tag needs '$bin $version'" >&2
    exit 1
  fi
  echo "$reported"
  scripts/check-static-elf.sh "dist/$file"
  (cd dist && sha256sum "$file" > "$file.sha256")
done

for bin in "${binaries[@]}"; do
  file="$bin-$target"
  for upload in "$file" "$file.sha256"; do
    curl --fail --silent --show-error --header "JOB-TOKEN: ${CI_JOB_TOKEN:?CI_JOB_TOKEN is not set}" \
      --upload-file "dist/$upload" "$package_url/$upload"
    echo
  done
done

{
  echo "Component: \`gitlab.com/${CI_PROJECT_PATH:?CI_PROJECT_PATH is not set}/ciburst@$version\`, or \`@${version%%.*}\` for the newest release of this major version."
  echo
  echo "Binaries (static, Linux x86_64), each with its checksum in \`<file>.sha256\`:"
  echo
  for bin in "${binaries[@]}"; do
    echo "- \`$bin-$target\`"
  done
  echo
  echo "Image: \`${CI_REGISTRY_IMAGE:?CI_REGISTRY_IMAGE is not set}/cli:$version\` with \`/usr/local/bin/ciburst\` and \`/usr/local/bin/ciburst-vm-agent\`."
} > release-notes.md
cat release-notes.md
