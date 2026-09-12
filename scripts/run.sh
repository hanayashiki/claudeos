#!/bin/bash
# Boot the kernel under QEMU. Usage: run.sh [--timeout SECS] [--initrd FILE] [extra qemu args...]
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KERNEL="$ROOT/build/kernel.elf"
TIMEOUT=20
INITRD=""
APPEND=""
EXTRA=()

while [ $# -gt 0 ]; do
  case "$1" in
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --initrd)  INITRD="$2";  shift 2 ;;
    --kernel)  KERNEL="$2";  shift 2 ;;
    --append)  APPEND="$2";  shift 2 ;;
    *) EXTRA+=("$1"); shift ;;
  esac
done

"$ROOT/scripts/reap-stale.sh" 15 || true

ARGS=(-kernel "$KERNEL" -serial stdio -display none -m 512M
      -no-reboot -device isa-debug-exit,iobase=0xf4,iosize=0x04
      -cpu qemu64,+pdpe1gb,+rdrand,+fsgsbase,+xsave)
if [ -n "$INITRD" ]; then ARGS+=(-initrd "$INITRD"); fi
if [ -n "$APPEND" ]; then ARGS+=(-append "$APPEND"); fi
if [ ${#EXTRA[@]} -gt 0 ]; then ARGS+=("${EXTRA[@]}"); fi

exec perl -e '
  my $t = shift;
  $| = 1;
  my $pid = fork();
  if (!defined $pid) { die "fork: $!" }
  if ($pid == 0) { exec @ARGV or die "exec: $!" }
  my $timed_out = 0;
  $SIG{ALRM} = sub { $timed_out = 1; kill "KILL", $pid; };
  alarm $t;
  waitpid($pid, 0);
  my $st = $?;
  alarm 0;
  if ($timed_out) { print STDERR "\n[run.sh] killed after ${t}s\n"; exit 124 }
  exit($st >> 8);
' "$TIMEOUT" qemu-system-x86_64 "${ARGS[@]}"
