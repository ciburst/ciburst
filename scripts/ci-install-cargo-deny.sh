#!/usr/bin/env bash
# Installs a pinned cargo-deny binary into $CARGO_HOME/bin (cached by CI).
set -euo pipefail

VERSION="0.20.2"
SHA256="9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f"
BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
NAME="cargo-deny-${VERSION}-x86_64-unknown-linux-musl"

if [ -x "$BIN/cargo-deny" ] && "$BIN/cargo-deny" --version | grep -q "$VERSION"; then
  echo "cargo-deny $VERSION present"
  exit 0
fi

tmp="$(mktemp -d)"
curl -fsSL -o "$tmp/$NAME.tar.gz" \
  "https://github.com/EmbarkStudios/cargo-deny/releases/download/${VERSION}/${NAME}.tar.gz"
echo "$SHA256  $tmp/$NAME.tar.gz" | sha256sum -c -
tar -xzf "$tmp/$NAME.tar.gz" -C "$tmp"
mkdir -p "$BIN"
install -m 0755 "$tmp/$NAME/cargo-deny" "$BIN/cargo-deny"
rm -rf "$tmp"
"$BIN/cargo-deny" --version
