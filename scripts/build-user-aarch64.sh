#!/bin/bash
# Build the userland for aarch64 and assemble an initramfs image from it.
#
# The same rootfs as scripts/build-user.sh, for the other machine. It writes
# build/rootfs-aarch64 and build/initramfs-aarch64.cpio, so the x86-64 image
# stays where it is and the two can be built side by side.
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

# rustc picks the linker flavour from the linker's file name, so expose
# rust-lld under the name it recognises for ELF targets.
mkdir -p "$ROOT/build/toolchain"
ln -sf "$LLVMBIN/rust-lld" "$ROOT/build/toolchain/ld.lld"

export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$ROOT/build/toolchain/ld.lld"
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static -C relocation-model=static"

cd "$ROOT/user/cbox"
cargo build --release --target "$TARGET"
CBOX="$ROOT/user/cbox/target/$TARGET/release/cbox"

cd "$ROOT/user/inet"
cargo build --release --target "$TARGET"
INET="$ROOT/user/inet/target/$TARGET/release/inet"

# ---- assemble the root filesystem ----------------------------------------
RFS="$ROOT/build/rootfs-aarch64"
rm -rf "$RFS"
mkdir -p "$RFS"/{bin,etc,root,tmp,dev,proc}

cp "$CBOX" "$RFS/bin/cbox"
chmod +x "$RFS/bin/cbox"

cp "$INET" "$RFS/bin/inet"
chmod +x "$RFS/bin/inet"

# One symlink per applet, busybox style.
APPLETS=$("$ROOT/scripts/list-applets.sh" "$ROOT/user/cbox/src/main.rs")
for applet in $APPLETS; do
  [ "$applet" = "cbox" ] && continue
  ln -sf cbox "$RFS/bin/$applet"
done
# "[" cannot appear in the applet table's identifier list.
ln -sf cbox "$RFS/bin/["

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

# The upstream busybox and Alpine images fetch/ downloads are x86-64 builds, so
# there is no third-party binary here yet. An aarch64 busybox dropped into
# build/thirdparty/busybox-aarch64 is picked up.
if [ -x "$ROOT/build/thirdparty/busybox-aarch64" ]; then
  cp "$ROOT/build/thirdparty/busybox-aarch64" "$RFS/bin/busybox"
  chmod +x "$RFS/bin/busybox"
fi

cat > "$RFS/etc/motd" <<'MOTD'
Welcome to claudeos.

This is a kernel written from scratch in Rust that implements enough of the
Linux system call interface to run unmodified static Linux binaries. The
userland you are talking to was built for aarch64-unknown-linux-musl.

Try:  ls -l /bin | head      ps      free      cat /proc/cpuinfo
      echo hi | tr a-z A-Z   sh /root/demo.sh
      rtest                  hello_c 60
MOTD

cat > "$RFS/etc/passwd" <<'PASSWD'
root:x:0:0:root:/root:/bin/sh
PASSWD

cat > "$RFS/etc/hostname" <<'HOSTNAME'
claudeos
HOSTNAME

cp "$ROOT/tests/suite.sh" "$RFS/root/suite.sh"

cat > "$RFS/root/hello.txt" <<'HELLO'
This file came from the initramfs, unpacked by the kernel at boot.
HELLO

# Scripts need the execute bit and a #! line to run as ./script.
for script in "$RFS"/root/*.sh; do
  if ! head -n 1 "$script" | grep -q '^#!'; then
    printf '#!/bin/sh\n%s' "$(cat "$script")" > "$script.tmp"
    mv "$script.tmp" "$script"
  fi
  chmod +x "$script"
done

python3 "$ROOT/tools/mkcpio.py" "$RFS" "$ROOT/build/initramfs-aarch64.cpio"
ls -la "$ROOT/build/initramfs-aarch64.cpio"

# Say what came out, since the point of this script is binaries for a machine
# this one is not.
echo
for binary in "$RFS/bin/cbox" "$RFS/bin/inet" "$RFS/bin/hello_c"; do
  [ -f "$binary" ] || continue
  echo "== $(basename "$binary")"
  file "$binary"
  # llvm-readobj is llvm-readelf under its other name; ask for readelf's output.
  "$LLVMBIN/llvm-readobj" --elf-output-style=GNU -h "$binary" |
      grep -E "^  (Class|Type|Machine|Entry)"
done
