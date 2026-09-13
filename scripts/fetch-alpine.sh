#!/bin/bash
# Fetch an Alpine Linux root filesystem and turn it into an initramfs.
#
# Everything in it is dynamically linked against musl and loaded by Alpine's
# own ld-musl, so booting it exercises the program interpreter path with a
# userland this project had no hand in building. Optional.
#
# ARCH picks which root filesystem, x86_64 unless told otherwise. Alpine
# publishes the same release for both, and the products are named apart so the
# two can sit side by side.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${ARCH:-x86_64}"
VERSION="${ALPINE_VERSION:-3.19.1}"
BRANCH="v${VERSION%.*}"
case "$ARCH" in
  x86_64)  SUFFIX="" ;;
  aarch64) SUFFIX="-aarch64" ;;
  *) echo "unknown ARCH: $ARCH" >&2; exit 1 ;;
esac
URL="https://dl-cdn.alpinelinux.org/alpine/$BRANCH/releases/$ARCH/alpine-minirootfs-$VERSION-$ARCH.tar.gz"
ARCHIVE="$ROOT/build/thirdparty/alpine-$VERSION$SUFFIX.tar.gz"
TREE="$ROOT/build/alpine-rootfs$SUFFIX"
CPIO="$ROOT/build/alpine$SUFFIX.cpio"

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

python3 "$ROOT/tools/mkcpio.py" "$TREE" "$CPIO"
echo "boot it with: ARCH=$ARCH ./scripts/run.sh --initrd ${CPIO#$ROOT/} --append 'init=/bin/sh'"
