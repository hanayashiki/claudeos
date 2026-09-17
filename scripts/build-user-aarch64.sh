#!/bin/bash
# Build the userland for aarch64 and assemble an initramfs image from it.
#
# The same rootfs as scripts/build-user.sh, for the other machine. It writes
# build/rootfs-aarch64 and build/initramfs-aarch64.cpio, so the x86-64 image
# stays where it is and the two can be built side by side. It also writes the
# board image, build/initramfs-aarch64-board.cpio, and build/data-aarch64,
# what a new card's /data starts with.
#
# No cross toolchain is needed beyond rustup's: the musl libc, the C runtime
# objects and rust-lld all come out of the aarch64-unknown-linux-musl target's
# own directory, and clang on macOS already emits aarch64 ELF objects.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SYSROOT="$(rustc --print sysroot)"
HOST="$(rustc -vV | sed -n 's/host: //p')"
LLVMBIN="$SYSROOT/lib/rustlib/$HOST/bin"
TARGET=aarch64-unknown-linux-musl

if [ ! -d "$SYSROOT/lib/rustlib/$TARGET" ]; then
  echo "missing target: run  rustup target add $TARGET" >&2
  exit 1
fi

# The C program below is linked by hand, and lld picks its flavour from the
# name it is run under, so expose rust-lld under the name of the ELF one. The
# Rust programs get their linker from .cargo/config.toml.
mkdir -p "$ROOT/build/toolchain"
ln -sf "$LLVMBIN/rust-lld" "$ROOT/build/toolchain/ld.lld"

# ---- which image gets what -----------------------------------------------
# The list, and the steps that make each image out of the staging tree below,
# are in scripts/images.sh, shared with the x86-64 build. cbox is built there,
# once per image, because the two images build it with different features.
. "$ROOT/scripts/images.sh"
check_image_items
print_image_items
TEST_TREE="$ROOT/build/rootfs-aarch64"
BOARD_TREE="$ROOT/build/rootfs-aarch64-board"
echo

cd "$ROOT"
cargo build -p inet --profile user --target "$TARGET"
INET="$ROOT/target/$TARGET/user/inet"

# ---- stage everything either image can have ------------------------------
RFS="$ROOT/build/stage-aarch64"
rm -rf "$RFS"
mkdir -p "$RFS"/{bin,etc,tests,tmp,dev,proc}

cp "$INET" "$RFS/bin/inet"
chmod +x "$RFS/bin/inet"

# A C program built against musl, to show the ABI is not Rust-specific.
#
# `long double` is 128-bit on aarch64 and the processor has no instructions for
# it, so musl's printf calls out to the soft-float helpers (__addtf3 and the
# rest). x86-64 has those in hardware, which is why the other script needs
# nothing beyond libc. The only build of them on this machine is rustc's own
# compiler_builtins; it carries a reference to rust_eh_personality from a
# unwinding table that nothing here executes, so the symbol is defined away.
MUSL="$SYSROOT/lib/rustlib/$TARGET/lib/self-contained"
BUILTINS=$(ls "$SYSROOT/lib/rustlib/$TARGET/lib"/libcompiler_builtins-*.rlib | head -n 1)
if command -v clang >/dev/null 2>&1; then
  clang --target=aarch64-unknown-linux-musl -O2 -ffreestanding -nostdinc \
        -fno-stack-protector -fno-builtin -c "$ROOT/user/c/hello.c" \
        -o "$ROOT/build/hello_c-aarch64.o"
  "$ROOT/build/toolchain/ld.lld" -o "$RFS/bin/hello_c" --no-pie -e _start \
        --defsym rust_eh_personality=0 \
        "$MUSL/crt1.o" "$MUSL/crti.o" "$ROOT/build/hello_c-aarch64.o" \
        "$MUSL/libc.a" "$BUILTINS" "$MUSL/crtn.o"
  chmod +x "$RFS/bin/hello_c"
fi

# An upstream busybox, if ARCH=aarch64 scripts/fetch-busybox.sh has been run.
# It is a binary this project did not build, so it is the strictest ABI test
# here.
if [ -x "$ROOT/build/thirdparty/busybox-aarch64" ]; then
  cp "$ROOT/build/thirdparty/busybox-aarch64" "$RFS/bin/busybox"
  chmod +x "$RFS/bin/busybox"
fi

# A web server. Alpine's busybox-static above has no httpd, telnetd or nc:
# Alpine builds those into busybox-extras, which it publishes only dynamically
# linked, so it comes with musl's dynamic loader, the file its program header
# names, which is also musl's libc. In the test image /bin/httpd is the name
# the suites use on either machine; the x86-64 build points it at busybox.net's
# busybox, which has httpd of its own. A card gets these under /usr instead,
# with /usr/bin/httpd a script, and the board image a link from the path the
# program header names to /usr/lib (scripts/images.sh).
if [ -x "$ROOT/build/thirdparty/busybox-extras-aarch64" ] && [ -f "$ROOT/build/thirdparty/ld-musl-aarch64.so.1" ]; then
  mkdir -p "$RFS/lib"
  cp "$ROOT/build/thirdparty/busybox-extras-aarch64" "$RFS/bin/busybox-extras"
  cp "$ROOT/build/thirdparty/ld-musl-aarch64.so.1" "$RFS/lib/ld-musl-aarch64.so.1"
  chmod +x "$RFS/bin/busybox-extras" "$RFS/lib/ld-musl-aarch64.so.1"
  ln -sf busybox-extras "$RFS/bin/httpd"
fi

# Cloudflare's own cloudflared, if ARCH=aarch64 scripts/fetch-cloudflared.sh
# has been run. It is a static Go program, and Go brings its own threads, its
# own resolver and its own TLS rather than calling a libc for any of them, so
# it asks for things nothing built against musl here has asked for.
if [ -x "$ROOT/build/thirdparty/cloudflared-aarch64" ]; then
  cp "$ROOT/build/thirdparty/cloudflared-aarch64" "$RFS/bin/cloudflared"
  chmod +x "$RFS/bin/cloudflared"
fi

# The Pi 4 WiFi chip's firmware, NVRAM and regulatory data, if
# scripts/fetch-wifi-firmware.sh has fetched them, under the names and the
# directory Linux's brcmfmac uses. And the network to join, if build/wifi.conf
# exists: it holds a passphrase, so it is copied and never printed, and the
# kernel removes it from the running system once it has read it.
if [ -d "$ROOT/build/thirdparty/wifi-firmware" ]; then
  mkdir -p "$RFS/lib/firmware/brcm"
  cp "$ROOT/build/thirdparty/wifi-firmware"/brcmfmac43455-sdio.* "$RFS/lib/firmware/brcm/"
fi
if [ -f "$ROOT/build/wifi.conf" ]; then
  cp "$ROOT/build/wifi.conf" "$RFS/etc/wifi.conf"
  chmod 600 "$RFS/etc/wifi.conf"
fi

# The Go program under user/go, built from source here for this machine rather
# than copied, so it is never the wrong architecture or a stale build for
# the host. Go needs no libc and links statically with cgo off. A program
# that fails to build is left out with a warning rather than failing the
# image, since the test harness builds this image before every run.
if command -v go >/dev/null 2>&1; then
  mkdir -p "$RFS/tests"
  if ! (cd "$ROOT/user/go" && CGO_ENABLED=0 GOOS=linux GOARCH=arm64 \
        go build -trimpath -o "$RFS/tests/go_main" .); then
    echo "warning: user/go did not build; /tests/go_main left out" >&2
  fi
fi

# No /etc/resolv.conf: the kernel writes it once it has a network
# configuration, from `nameserver=` or from the DHCP lease, so a fixed one here
# would name a server the machine may not be able to reach.

# A certificate store, so a program with its own TLS has roots to verify
# against. Nothing here builds one; this is the bundle out of the Alpine root
# filesystem scripts/fetch-alpine.sh downloads. Alpine publishes the same one
# for both machines, so either tree's copy will do.
for CERTS in "$ROOT/build/alpine-rootfs-aarch64/etc/ssl/certs/ca-certificates.crt" \
             "$ROOT/build/alpine-rootfs/etc/ssl/certs/ca-certificates.crt"; do
  [ -f "$CERTS" ] || continue
  mkdir -p "$RFS/etc/ssl/certs"
  cp "$CERTS" "$RFS/etc/ssl/certs/ca-certificates.crt"
  ln -sf certs/ca-certificates.crt "$RFS/etc/ssl/cert.pem"
  break
done

# ---- network time --------------------------------------------------------
# The servers BusyBox ntpd asks. The ntpd line in user/services, the system
# services list every image has, says needs=/etc/ntp.conf, so init starts ntpd
# only in an image that has this file, which is how a board with no
# battery-backed clock learns the date. scripts/images.sh puts it in the board
# image only, so the test images the suites boot never wait on servers across
# the internet; the network time section of scripts/test.sh adds it to a copy.
cat > "$RFS/etc/ntp.conf" <<'NTP'
server ntp.nict.jp
server time.cloudflare.com
NTP

cat > "$RFS/etc/passwd" <<'PASSWD'
root:x:0:0:root:/root:/bin/sh
PASSWD

cat > "$RFS/etc/hostname" <<'HOSTNAME'
claudeos
HOSTNAME

# The system services init starts at boot, the same list in both images.
mkdir -p "$RFS/etc/claudeos"
cp "$ROOT/user/services" "$RFS/etc/claudeos/services"

cp "$ROOT/tests/demo.sh" "$RFS/tests/demo.sh"
cp "$ROOT/tests/suite.sh" "$RFS/tests/suite.sh"
cp "$ROOT/tests/busybox.sh" "$RFS/tests/busybox.sh"
# Only the Pi 4 has a card slot, so only this image has the /data suite.
cp "$ROOT/tests/data.sh" "$RFS/tests/data.sh"

cat > "$RFS/tests/hello.txt" <<'HELLO'
This file came from the initramfs, unpacked by the kernel at boot.
HELLO

# Scripts need the execute bit and a #! line to run as ./script.
for script in "$RFS"/tests/*.sh; do
  if ! head -n 1 "$script" | grep -q '^#!'; then
    printf '#!/bin/sh\n%s' "$(cat "$script")" > "$script.tmp"
    mv "$script.tmp" "$script"
  fi
  chmod +x "$script"
done

# ---- the two images, and the card ------------------------------------------
# The kernel is built before this, by scripts/test.sh and by hand alike, so its
# digest in the manifests is the kernel's that boots with them.
check_staged "$RFS"
assemble_image test "$RFS" "$TEST_TREE" "$ROOT/build/initramfs-aarch64.cpio" \
    "$ROOT/build/kernel-aarch64.elf"
assemble_image board "$RFS" "$BOARD_TREE" "$ROOT/build/initramfs-aarch64-board.cpio" \
    "$ROOT/build/kernel-aarch64.elf"
# What scripts/mkcard.sh --new puts on /data, and --usr puts on /data/usr.
assemble_card "$RFS" "$ROOT/build/data-aarch64"
echo
ls -la "$ROOT/build/initramfs-aarch64.cpio" "$ROOT/build/initramfs-aarch64-board.cpio"

# Say what came out, since the point of this script is binaries for a machine
# this one is not.
echo
for binary in "$TEST_TREE/bin/cbox" "$TEST_TREE/bin/inet" "$TEST_TREE/bin/hello_c"; do
  [ -f "$binary" ] || continue
  echo "== $(basename "$binary")"
  file "$binary"
  # llvm-readobj is llvm-readelf under its other name; ask for readelf's output.
  "$LLVMBIN/llvm-readobj" --elf-output-style=GNU -h "$binary" |
      grep -E "^  (Class|Type|Machine|Entry)"
done
