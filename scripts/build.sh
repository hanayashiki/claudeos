#!/bin/bash
# Build the kernel. ARCH picks the machine; x86_64 unless told otherwise.
#
# x86_64 is handed to QEMU's multiboot loader, which only reads ELF32, so the
# image is converted. On aarch64 the flat image is the product: it is the only
# thing a Pi 4's firmware will load off the card, and it is also the only form
# QEMU follows the Linux boot protocol for. The ELF is kept beside it for
# symbols and disassembly and is not what boots.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LLVMBIN="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/host: //p')/bin"
PROFILE="${PROFILE:-release}"
ARCH="${ARCH:-x86_64}"

case "$ARCH" in
  x86_64)  TARGET=x86_64-unknown-none ;;
  aarch64) TARGET=aarch64-unknown-none-softfloat ;;
  *) echo "unknown ARCH: $ARCH" >&2; exit 1 ;;
esac

cd "$ROOT/kernel"
if [ "$PROFILE" = "release" ]; then
  cargo build --release --target "$TARGET"
else
  cargo build --target "$TARGET"
fi

RAW="$ROOT/kernel/target/$TARGET/$PROFILE/kernel"
mkdir -p "$ROOT/build"

if [ "$ARCH" = aarch64 ]; then
  OUT="$ROOT/build/kernel-aarch64.elf"
  cp "$RAW" "$OUT"
  "$LLVMBIN/llvm-objcopy" -O binary "$RAW" "$ROOT/build/kernel8.img"
else
  OUT="$ROOT/build/kernel.elf"
  "$LLVMBIN/llvm-objcopy" -I elf64-x86-64 -O elf32-i386 "$RAW" "$OUT"
  cp "$RAW" "$ROOT/build/kernel64.elf"
fi
echo "built $OUT ($(wc -c < "$OUT") bytes)"
