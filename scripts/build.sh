#!/bin/bash
# Build the kernel and convert it to the ELF32 image QEMU's multiboot loader wants.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LLVMBIN="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/host: //p')/bin"
PROFILE="${PROFILE:-release}"

cd "$ROOT/kernel"
if [ "$PROFILE" = "release" ]; then cargo build --release; else cargo build; fi

RAW="$ROOT/kernel/target/x86_64-unknown-none/$PROFILE/kernel"
OUT="$ROOT/build/kernel.elf"
mkdir -p "$ROOT/build"
"$LLVMBIN/llvm-objcopy" -I elf64-x86-64 -O elf32-i386 "$RAW" "$OUT"
cp "$RAW" "$ROOT/build/kernel64.elf"
echo "built $OUT ($(wc -c < "$OUT") bytes)"
