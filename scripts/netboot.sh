#!/bin/bash
# Boot a Raspberry Pi 4 over the network from this machine.
#
#   scripts/netboot.sh                  assemble build/boot, then serve it over TFTP on udp port 69
#   scripts/netboot.sh --port 6969      anything after the script name goes to the server
#
# With the network first in the Pi's BOOT_ORDER and TFTP_IP set to this
# machine's address, the bootloader in the Pi's EEPROM reads every file it
# would otherwise read from the SD card from here instead. Trying a new kernel
# is then a build and a power cycle.
#
# The server opens each file when the Pi asks for it, so it can stay running:
# after a new build, run ./scripts/mkcard.sh again, with no arguments, and
# power cycle the Pi.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# With no device named, mkcard.sh only copies the kernel, the board image
# (build/initramfs-aarch64-board.cpio, without the test suites) and the Pi
# firmware into build/boot. It writes to no disk.
"$ROOT/scripts/mkcard.sh"

echo ""
exec python3 "$ROOT/scripts/netboot-serve.py" --root "$ROOT/build/boot" "$@"
