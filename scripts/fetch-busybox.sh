#!/bin/bash
# Fetch an upstream static busybox to test against a binary this project did
# not build. Optional: everything else works without it.
#
# ARCH picks which one, x86_64 unless told otherwise. busybox.net publishes
# prebuilt binaries for x86-64 and for 32-bit ARM, but none for aarch64, so
# that one comes from Alpine's busybox-static package. An .apk is a tarball of
# concatenated gzip streams with the binary at bin/busybox.static.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${ARCH:-x86_64}"

case "$ARCH" in
  x86_64)
    URL="https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox"
    DEST="$ROOT/build/thirdparty/busybox"
    MACHINE="Advanced Micro Devices X86-64"
    ;;
  aarch64)
    ALPINE_BRANCH="${ALPINE_BRANCH:-v3.19}"
    BUSYBOX_APK="${BUSYBOX_APK:-busybox-static-1.36.1-r21.apk}"
    URL="https://dl-cdn.alpinelinux.org/alpine/$ALPINE_BRANCH/main/aarch64/$BUSYBOX_APK"
    DEST="$ROOT/build/thirdparty/busybox-aarch64"
    MACHINE="AArch64"
    ;;
  *) echo "unknown ARCH: $ARCH" >&2; exit 1 ;;
esac

mkdir -p "$(dirname "$DEST")"
# The userland build copies this binary into the root filesystem, so fetching
# it after building leaves it out, and the suite then skips with nothing to
# run rather than failing.
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
if [ "$ARCH" = aarch64 ]; then
  WORK="$ROOT/build/thirdparty/busybox-apk"
  rm -rf "$WORK"
  mkdir -p "$WORK"
  curl -fsSL -o "$WORK/package.apk" "$URL"
  # The signature stream ahead of the package confuses tar about where the
  # archive ends; the files still come out.
  tar xzf "$WORK/package.apk" -C "$WORK" 2>/dev/null || true
  cp "$WORK/bin/busybox.static" "$DEST"
  rm -rf "$WORK"
else
  curl -fsSL -o "$DEST" "$URL"
fi
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
