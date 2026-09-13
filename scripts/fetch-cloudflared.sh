#!/bin/bash
# Fetch Cloudflare's own static cloudflared build to run against. Optional:
# everything else works without it.
#
# ARCH picks which one, x86_64 unless told otherwise. Cloudflare publishes one
# statically linked binary per machine on its releases page and names them by
# Go's architecture words, so x86_64 is amd64 and aarch64 is arm64.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${ARCH:-x86_64}"
VERSION="${CLOUDFLARED_VERSION:-2026.9.1}"

case "$ARCH" in
  x86_64)
    GOARCH=amd64
    DEST="$ROOT/build/thirdparty/cloudflared"
    MACHINE="Advanced Micro Devices X86-64"
    ;;
  aarch64)
    GOARCH=arm64
    DEST="$ROOT/build/thirdparty/cloudflared-aarch64"
    MACHINE="AArch64"
    ;;
  *) echo "unknown ARCH: $ARCH" >&2; exit 1 ;;
esac
URL="https://github.com/cloudflare/cloudflared/releases/download/$VERSION/cloudflared-linux-$GOARCH"

mkdir -p "$(dirname "$DEST")"
# The userland build copies this binary into the root filesystem, so fetching
# it after building leaves it out.
remind() {
  if [ "$ARCH" = aarch64 ]; then
    echo "then run ./scripts/build-user-aarch64.sh, or this will not reach the initramfs"
  else
    echo "then run ./scripts/build-user.sh, or this will not reach the initramfs"
  fi
}

if [ -x "$DEST" ]; then
  echo "already present: $DEST"
  remind
  exit 0
fi
echo "fetching $URL"
curl -fsSL -o "$DEST" "$URL"
chmod +x "$DEST"

# The kernel loads a static executable directly and hands a dynamic one to its
# interpreter, which is not in this initramfs. Say so here rather than at boot.
LLVMBIN="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/host: //p')/bin"
HEADER="$("$LLVMBIN/llvm-readobj" --elf-output-style=GNU -h "$DEST")"
if ! echo "$HEADER" | grep -q "Machine: *$MACHINE"; then
  echo "$DEST is not $MACHINE:" >&2
  echo "$HEADER" | grep -E "^  (Class|Type|Machine)" >&2
  rm -f "$DEST"
  exit 1
fi
if "$LLVMBIN/llvm-readobj" --elf-output-style=GNU -l "$DEST" | grep -q INTERP; then
  echo "$DEST is dynamically linked; it needs an interpreter this OS has not got" >&2
  rm -f "$DEST"
  exit 1
fi
ls -la "$DEST"
file "$DEST"
remind
