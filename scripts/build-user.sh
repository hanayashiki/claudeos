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
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static"

cd "$ROOT/user/cbox"
cargo build --release --target "$TARGET"
CBOX="$ROOT/user/cbox/target/$TARGET/release/cbox"

# ---- assemble the root filesystem ----------------------------------------
RFS="$ROOT/build/rootfs"
rm -rf "$RFS"
mkdir -p "$RFS"/{bin,etc,root,tmp,dev,proc}

cp "$CBOX" "$RFS/bin/cbox"
chmod +x "$RFS/bin/cbox"

# One symlink per applet, busybox style.
APPLETS=$("$ROOT/scripts/list-applets.sh" "$ROOT/user/cbox/src/main.rs")
for applet in $APPLETS; do
  [ "$applet" = "cbox" ] && continue
  ln -sf cbox "$RFS/bin/$applet"
done
# "[" cannot appear in the applet table's identifier list.
ln -sf cbox "$RFS/bin/["


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
fi

cat > "$RFS/etc/motd" <<'MOTD'
Welcome to claudeos.

This is a kernel written from scratch in Rust that implements enough of the
Linux system call interface to run unmodified static Linux binaries. The
userland you are talking to was built for x86_64-unknown-linux-musl.

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

cat > "$RFS/root/demo.sh" <<'DEMO'
# A tour of what the shell and kernel can do.
echo "--- processes ---"
ps
echo
echo "--- pipes and redirection ---"
seq 1 20 | grep -c ""
seq 1 5 | tr '0-9' 'a-j'
echo "written by a pipeline" > /tmp/pipe.txt
cat /tmp/pipe.txt
echo
echo "--- files ---"
mkdir -p /tmp/demo/inner
echo alpha > /tmp/demo/a.txt
echo beta  > /tmp/demo/inner/b.txt
find /tmp/demo
wc -l /tmp/demo/a.txt
echo
echo "--- globbing and exit status ---"
ls /tmp/demo/*.txt
test -d /tmp/demo && echo "directory exists"
test -f /nope || echo "missing file reports failure"
echo
echo "--- a C program built against musl ---"
hello_c 60
echo
echo "--- system ---"
uname -a
free
uptime
DEMO

cat > "$RFS/root/hello.txt" <<'HELLO'
This file came from the initramfs, unpacked by the kernel at boot.
HELLO

cp "$ROOT/tests/suite.sh" "$RFS/root/suite.sh"
cp "$ROOT/tests/busybox.sh" "$RFS/root/busybox.sh"

# Scripts need the execute bit and a #! line to run as ./script.
for script in "$RFS"/root/*.sh; do
  if ! head -n 1 "$script" | grep -q '^#!'; then
    printf '#!/bin/sh\n%s' "$(cat "$script")" > "$script.tmp"
    mv "$script.tmp" "$script"
  fi
  chmod +x "$script"
done

python3 "$ROOT/tools/mkcpio.py" "$RFS" "$ROOT/build/initramfs.cpio"
ls -la "$ROOT/build/initramfs.cpio"
