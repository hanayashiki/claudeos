#!/bin/bash
# Fetch an Alpine Linux root filesystem and turn it into an initramfs.
#
# Everything in it is dynamically linked against musl and loaded by Alpine's
# own ld-musl, so booting it exercises the program interpreter path with a
# userland this project had no hand in building. Optional.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${ALPINE_VERSION:-3.19.1}"
BRANCH="v${VERSION%.*}"
URL="https://dl-cdn.alpinelinux.org/alpine/$BRANCH/releases/x86_64/alpine-minirootfs-$VERSION-x86_64.tar.gz"
ARCHIVE="$ROOT/build/thirdparty/alpine-$VERSION.tar.gz"
TREE="$ROOT/build/alpine-rootfs"

mkdir -p "$(dirname "$ARCHIVE")"
if [ ! -f "$ARCHIVE" ]; then
  echo "fetching $URL"
  curl -fsSL -o "$ARCHIVE" "$URL"
fi

rm -rf "$TREE"
mkdir -p "$TREE"
tar xzf "$ARCHIVE" -C "$TREE" 2>/dev/null || true

cp "$ROOT/tests/alpine.sh" "$TREE/root/alpine.sh"
chmod +x "$TREE/root/alpine.sh"

python3 "$ROOT/tools/mkcpio.py" "$TREE" "$ROOT/build/alpine.cpio"
echo "boot it with: ./scripts/run.sh --initrd build/alpine.cpio --append 'init=/bin/sh'"
