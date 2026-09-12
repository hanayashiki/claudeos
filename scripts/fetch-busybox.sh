#!/bin/bash
# Fetch an upstream static busybox to test against a binary this project did
# not build. Optional: everything else works without it.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
URL="https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox"
DEST="$ROOT/build/thirdparty/busybox"

mkdir -p "$(dirname "$DEST")"
if [ -x "$DEST" ]; then
  echo "already present: $DEST"
  exit 0
fi
echo "fetching $URL"
curl -fsSL -o "$DEST" "$URL"
chmod +x "$DEST"
ls -la "$DEST"
