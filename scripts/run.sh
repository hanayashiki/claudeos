#!/bin/bash
# Boot the kernel under QEMU. Usage: run.sh [--timeout SECS] [--initrd FILE] [extra qemu args...]
#
# Ctrl-C goes to the guest and interrupts the job running there. The way out
# of the emulator is Ctrl-A then X.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${ARCH:-x86_64}"
if [ "$ARCH" = aarch64 ]; then
  # The flat image, not the ELF: QEMU only follows the Linux boot protocol for
  # a raw image, and an ELF gets no initrd and no command line.
  KERNEL="$ROOT/build/kernel8.img"
else
  KERNEL="$ROOT/build/kernel.elf"
fi
TIMEOUT=20
INITRD=""
APPEND=""
NET=""
PCAP=""
EXTRA=()

while [ $# -gt 0 ]; do
  case "$1" in
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --initrd)  INITRD="$2";  shift 2 ;;
    --kernel)  KERNEL="$2";  shift 2 ;;
    --append)  APPEND="$2";  shift 2 ;;
    --net)     NET=1; shift ;;
    --hostfwd) NET=1; HOSTFWD="$2"; shift 2 ;;
    --pcap)    NET=1; PCAP="$2";  shift 2 ;;
    *) EXTRA+=("$1"); shift ;;
  esac
done

"$ROOT/scripts/reap-stale.sh" 15 || true

# `mon:stdio` rather than `stdio`: the plain form leaves the terminal's
# interrupt character to the host, so Ctrl-C kills QEMU and the guest never
# sees the byte. Multiplexing the monitor onto the same stream puts the
# terminal in raw mode and passes every key through, which is the only way the
# console's own line discipline gets to act on one.
if [ "$ARCH" = aarch64 ]; then
  # The emulated Pi 4 has two serial ports and the first one is the PL011,
  # which is the console this kernel drives.
  ARGS=(-M raspi4b -kernel "$KERNEL" -serial mon:stdio -display none -no-reboot)
else
  ARGS=(-kernel "$KERNEL" -serial mon:stdio -display none -m 512M
        -no-reboot -device isa-debug-exit,iobase=0xf4,iosize=0x04
        -cpu qemu64,+pdpe1gb,+rdrand,+fsgsbase,+xsave)
fi
# A card on QEMU's user-mode network: the guest gets 10.0.2.15, the gateway
# and name server are 10.0.2.2 and 10.0.2.3, and --hostfwd maps a port on the
# Mac to one in the guest. --pcap records every frame for inspection.
if [ -n "$NET" ]; then
  NETDEV="user,id=n0"
  if [ -n "${HOSTFWD:-}" ]; then NETDEV="$NETDEV,hostfwd=$HOSTFWD"; fi
  ARGS+=(-netdev "$NETDEV" -device "e1000,netdev=n0")
  if [ -n "$PCAP" ]; then
    ARGS+=(-object "filter-dump,id=dump0,netdev=n0,file=$PCAP")
  fi
fi
if [ -n "$INITRD" ]; then ARGS+=(-initrd "$INITRD"); fi
if [ -n "$APPEND" ]; then ARGS+=(-append "$APPEND"); fi
if [ ${#EXTRA[@]} -gt 0 ]; then ARGS+=("${EXTRA[@]}"); fi

if [ "$ARCH" = aarch64 ]; then QEMU=qemu-system-aarch64; else QEMU=qemu-system-x86_64; fi

# Raw mode is QEMU's to undo, and a run the timeout below kills never gets to
# undo it. Put the terminal back whatever happens, so a timed-out boot does not
# leave the shell that started it without an echo.
if [ -t 0 ]; then
  TTY_STATE="$(stty -g)"
  trap 'stty "$TTY_STATE"' EXIT
fi

perl -e '
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
' "$TIMEOUT" "$QEMU" "${ARGS[@]}"
exit $?
