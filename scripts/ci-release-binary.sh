#!/usr/bin/env bash
# Builds the static ciburst binary for x86_64-unknown-linux-musl, uploads it with its checksum
# to the generic package registry and writes release-notes.md for the release job.
set -euo pipefail

target="x86_64-unknown-linux-musl"
tag="${CI_COMMIT_TAG:?CI_COMMIT_TAG is not set}"
version="${tag#v}"
file="ciburst-$target"
package_url="${CI_API_V4_URL}/projects/${CI_PROJECT_ID}/packages/generic/ciburst/${tag}"

apt-get update
apt-get install -y --no-install-recommends musl-tools
rustup target add "$target"
cargo build --release --locked --target "$target" --package ciburst

mkdir -p dist
cp "target/$target/release/ciburst" "dist/$file"
(cd dist && sha256sum "$file" > "$file.sha256")
"dist/$file" --version

for upload in "$file" "$file.sha256"; do
  curl --fail --silent --show-error --header "JOB-TOKEN: ${CI_JOB_TOKEN}" \
    --upload-file "dist/$upload" "$package_url/$upload"
  echo
done

{
  echo "Binary: \`$file\` (static, Linux x86_64), checksum in \`$file.sha256\`."
  echo
  echo "Pool images:"
  echo
  for image in base node rust playwright; do
    echo "- \`${CI_REGISTRY_IMAGE}/$image:$version\`"
  done
} > release-notes.md
cat release-notes.md
