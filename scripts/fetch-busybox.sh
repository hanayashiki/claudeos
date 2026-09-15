#!/bin/bash
# Fetch an upstream static busybox to test against a binary this project did
# not build, and on aarch64 the web server that busybox lacks. Optional:
# everything else works without them.
#
# ARCH picks which one, x86_64 unless told otherwise. busybox.net publishes
# prebuilt binaries for x86-64 and for 32-bit ARM, but none for aarch64, so
# that one comes from Alpine's busybox-static package. An .apk is a tarball of
# concatenated gzip streams, here with the binary at bin/busybox.static.
#
# busybox.net's x86-64 build has httpd. Alpine's busybox-static does not:
# Alpine builds httpd, telnetd and nc into busybox-extras, and publishes that
# only dynamically linked. So on aarch64 this also takes bin/busybox-extras out
# of that package, and lib/ld-musl-aarch64.so.1, the interpreter its program
# header names, which is musl's libc as well, out of Alpine's musl package, from
# the same Alpine release. The kernel hands a program with an interpreter to it.
#
# Every download is checked against the sha256 recorded below before anything
# is taken out of it, and every file taken out against its own, so a rerun gets
# the same bytes or stops. A file already in build/thirdparty with its recorded
# sha256 is left as it is.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${ARCH:-x86_64}"
CACHE="$ROOT/build/thirdparty"
ALPINE="https://dl-cdn.alpinelinux.org/alpine/v3.19/main/aarch64"

# One file per line: its name in build/thirdparty and its sha256; the URL it
# comes from and the sha256 of that download; and the file's path inside the
# package, or `-` when the download is the file.
case "$ARCH" in
  x86_64)
    FILES="
busybox  6e123e7f3202a8c1e9b1f94d8941580a25135382b99e8d3e34fb858bba311348  https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox  6e123e7f3202a8c1e9b1f94d8941580a25135382b99e8d3e34fb858bba311348  -
"
    ;;
  aarch64)
    FILES="
busybox-aarch64         f83aa5afa7ea458a57a5ee1e1d2c80a4f2754e11221b12236e18500426cce7b4  $ALPINE/busybox-static-1.36.1-r21.apk  d574d8c6434223367f140bd16d34f09bd01a4ea11e480c2e735ec2e2cfcc3346  bin/busybox.static
busybox-extras-aarch64  23df02a3eb3278ddbeb65dbeb317a73efd7cf5bdae606d34b10d3bec8b91a545  $ALPINE/busybox-extras-1.36.1-r21.apk  8771a6f036f628925167b076e6c78eecf1424efd607416eb3776eff6d313da18  bin/busybox-extras
ld-musl-aarch64.so.1    7d605adcd1cdc4d37251f0e8e3099954e13554323aabfd5e9cf47c10ff63b76c  $ALPINE/musl-1.2.4_git20230717-r6.apk  6685d4a557c963a198426edbf9ecc66fa1f12bfe51185df69173984d67bd1b40  lib/ld-musl-aarch64.so.1
"
    ;;
  *) echo "unknown ARCH: $ARCH" >&2; exit 1 ;;
esac

die() { printf '%s\n' "$*" >&2; exit 1; }
sha256() { shasum -a 256 "$1" | awk '{print $1}'; }

mkdir -p "$CACHE"
WORK="$CACHE/busybox-fetch.$$"
trap 'rm -rf "$WORK"' EXIT
fetched=""
while read -r name hash url download_hash member; do
  [ -n "$name" ] || continue
  dest="$CACHE/$name"
  fetched="$fetched $dest"
  if [ -f "$dest" ] && [ "$(sha256 "$dest")" = "$hash" ]; then
    echo "already present: $dest"
    continue
  fi
  echo "fetching $url"
  rm -rf "$WORK"
  mkdir -p "$WORK"
  curl -fsSL --max-time 120 -o "$WORK/download" "$url" \
    || die "could not fetch $url; check the network and try again"
  [ "$(sha256 "$WORK/download")" = "$download_hash" ] \
    || die "$url does not have the sha256 recorded in $0; refusing to use it"
  if [ "$member" = - ]; then
    file="$WORK/download"
  else
    # The signature stream ahead of the package confuses tar about where the
    # archive ends; the files still come out, and the check below says whether
    # this one did.
    tar xzf "$WORK/download" -C "$WORK" 2>/dev/null || true
    file="$WORK/$member"
    [ -f "$file" ] || die "$url holds no $member"
  fi
  [ "$(sha256 "$file")" = "$hash" ] \
    || die "$member from $url does not have the sha256 recorded in $0; refusing to use it"
  chmod +x "$file"
  # Renamed into place, so a build copying the old file never reads half of
  # the new one.
  mv "$file" "$dest.new"
  mv "$dest.new" "$dest"
done <<EOF
$FILES
EOF

ls -la $fetched
file $fetched
if [ "$ARCH" = aarch64 ]; then
  echo "then run ./scripts/build-user-aarch64.sh, or this will not reach the initramfs"
else
  echo "then run ./scripts/build-user.sh, or this will not reach the initramfs"
fi
