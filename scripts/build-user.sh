#!/bin/bash
# Build the userland and assemble the initramfs image.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SYSROOT="$(rustc --print sysroot)"
HOST="$(rustc -vV | sed -n 's/host: //p')"
LLVMBIN="$SYSROOT/lib/rustlib/$HOST/bin"
TARGET=x86_64-unknown-linux-musl

# rustc picks the linker flavour from the linker's file name, so expose
# rust-lld under the name it recognises for ELF targets.
mkdir -p "$ROOT/build/toolchain"
ln -sf "$LLVMBIN/rust-lld" "$ROOT/build/toolchain/ld.lld"

export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$ROOT/build/toolchain/ld.lld"
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static -C relocation-model=static"

# ---- which image gets what -----------------------------------------------
# The list, and the steps that make an image out of the staging tree below,
# are in scripts/images.sh, shared with the aarch64 build. This machine has
# only the test image: the board image is for the Raspberry Pi 4. cbox is built
# there, with the features the list gives the image.
. "$ROOT/scripts/images.sh"
check_image_items
print_image_items
echo

# Sockets, through the standard library rather than through this project's own
# code: `inet` on its own is the socket test, `inet serve` an HTTP server.
cd "$ROOT/user/inet"
cargo build --release --target "$TARGET"
INET="$ROOT/user/inet/target/$TARGET/release/inet"

# ---- stage everything the image can have ---------------------------------
RFS="$ROOT/build/stage"
rm -rf "$RFS"
mkdir -p "$RFS"/{bin,etc,tests,tmp,dev,proc}

cp "$INET" "$RFS/bin/inet"
chmod +x "$RFS/bin/inet"


# A C program built against musl, to show the ABI is not Rust-specific.
MUSL="$SYSROOT/lib/rustlib/$TARGET/lib/self-contained"
if command -v clang >/dev/null 2>&1; then
  clang --target=x86_64-unknown-linux-musl -O2 -ffreestanding -nostdinc \
        -fno-stack-protector -fno-builtin -c "$ROOT/user/c/hello.c" \
        -o "$ROOT/build/hello_c.o"
  "$ROOT/build/toolchain/ld.lld" -o "$RFS/bin/hello_c" --no-pie -e _start \
        "$MUSL/crt1.o" "$MUSL/crti.o" "$ROOT/build/hello_c.o" \
        "$MUSL/libc.a" "$MUSL/crtn.o"
  chmod +x "$RFS/bin/hello_c"
fi

# An upstream busybox, if scripts/fetch-busybox.sh has been run. It is a
# binary this project did not build, so it is the strictest ABI test here.
if [ -x "$ROOT/build/thirdparty/busybox" ]; then
  cp "$ROOT/build/thirdparty/busybox" "$RFS/bin/busybox"
  chmod +x "$RFS/bin/busybox"
  # The web server the suites name as /bin/httpd on either machine. This
  # busybox has httpd; on aarch64 the link is to busybox-extras.
  ln -sf busybox "$RFS/bin/httpd"
fi

# Cloudflare's own cloudflared, if scripts/fetch-cloudflared.sh has been run.
# It is a static Go program, and Go brings its own threads, its own resolver
# and its own TLS rather than calling a libc for any of them, so it asks for
# things nothing built against musl here has asked for.
if [ -x "$ROOT/build/thirdparty/cloudflared" ]; then
  cp "$ROOT/build/thirdparty/cloudflared" "$RFS/bin/cloudflared"
  chmod +x "$RFS/bin/cloudflared"
fi

# The Go program under user/go, built from source here for this machine rather
# than copied, so it is never the wrong architecture or a stale build for
# the host. Go needs no libc and links statically with cgo off. A program
# that fails to build is left out with a warning rather than failing the
# image, since the test harness builds this image before every run.
if command -v go >/dev/null 2>&1; then
  mkdir -p "$RFS/tests"
  if ! (cd "$ROOT/user/go" && CGO_ENABLED=0 GOOS=linux GOARCH=amd64 \
        go build -trimpath -o "$RFS/tests/go_main" .); then
    echo "warning: user/go did not build; /tests/go_main left out" >&2
  fi
fi

# No /etc/resolv.conf: the kernel writes it once it has a network
# configuration, from `nameserver=` or from the DHCP lease, so a fixed one here
# would name a server the machine may not be able to reach.

# A certificate store, so a program with its own TLS has roots to verify
# against. Nothing here builds one; this is the bundle out of the Alpine root
# filesystem scripts/fetch-alpine.sh downloads.
CERTS="$ROOT/build/alpine-rootfs/etc/ssl/certs/ca-certificates.crt"
if [ -f "$CERTS" ]; then
  mkdir -p "$RFS/etc/ssl/certs"
  cp "$CERTS" "$RFS/etc/ssl/certs/ca-certificates.crt"
  ln -sf certs/ca-certificates.crt "$RFS/etc/ssl/cert.pem"
fi

cat > "$RFS/etc/passwd" <<'PASSWD'
root:x:0:0:root:/root:/bin/sh
PASSWD

cat > "$RFS/etc/hostname" <<'HOSTNAME'
claudeos
HOSTNAME

# The system services init starts at boot, the same list in every image.
mkdir -p "$RFS/etc/claudeos"
cp "$ROOT/user/services" "$RFS/etc/claudeos/services"

cp "$ROOT/tests/demo.sh" "$RFS/tests/demo.sh"

cat > "$RFS/tests/hello.txt" <<'HELLO'
This file came from the initramfs, unpacked by the kernel at boot.
HELLO

cp "$ROOT/tests/suite.sh" "$RFS/tests/suite.sh"
cp "$ROOT/tests/busybox.sh" "$RFS/tests/busybox.sh"

# Scripts need the execute bit and a #! line to run as ./script.
for script in "$RFS"/tests/*.sh; do
  if ! head -n 1 "$script" | grep -q '^#!'; then
    printf '#!/bin/sh\n%s' "$(cat "$script")" > "$script.tmp"
    mv "$script.tmp" "$script"
  fi
  chmod +x "$script"
done

# ---- the image -------------------------------------------------------------
# The kernel is built before this, by scripts/test.sh and by hand alike, so its
# digest in the manifest is the kernel's that boots with it.
check_staged "$RFS"
assemble_image test "$RFS" "$ROOT/build/rootfs" "$ROOT/build/initramfs.cpio" \
    "$ROOT/build/kernel.elf"
echo
ls -la "$ROOT/build/initramfs.cpio"
