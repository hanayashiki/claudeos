#!/bin/bash
# Assemble the boot partition of an SD card for a Raspberry Pi 4, and put it on
# a card if you name one.
#
#   scripts/mkcard.sh                 assemble build/boot and stop
#   scripts/mkcard.sh /dev/disk4      assemble, then erase that card and write it
#   scripts/mkcard.sh --dir DIR /dev/disk4
#                                     erase that card and write DIR to it
#                                     instead, without assembling anything
#
# A Pi 4 boots from a single FAT32 partition. Its bootloader lives in an EEPROM
# on the board and can read nothing else, so everything it needs -- its own
# firmware, a device tree, our kernel and our ram disk -- is a plain file on
# that partition. There is no boot sector to install and nothing to make
# bootable; the firmware looks for files by name.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BOOT="$ROOT/build/boot"
CACHE="$ROOT/build/thirdparty/firmware"

# The Raspberry Pi firmware, which is not ours and is not in this repository.
FIRMWARE_URL="https://github.com/raspberrypi/firmware/raw/master/boot"
# start4.elf is the firmware the EEPROM bootloader loads, fixup4.dat tells it
# how to split memory with the video core, and the two device tree files
# describe the board and move the serial port to the header pins.
FIRMWARE_FILES="start4.elf fixup4.dat bcm2711-rpi-4-b.dtb overlays/disable-bt.dtbo"

say() { printf '%s\n' "$*"; }
die() { printf '%s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Assemble what goes on the card
# ---------------------------------------------------------------------------

fetch_firmware() {
  mkdir -p "$CACHE/overlays"
  for file in $FIRMWARE_FILES; do
    if [ -s "$CACHE/$file" ]; then continue; fi
    say "fetching $file"
    curl -fsSL -o "$CACHE/$file" "$FIRMWARE_URL/$file" \
      || die "could not fetch $file; check the network and try again"
  done
}

assemble() {
  [ -f "$ROOT/build/kernel8.img" ] \
    || die "build/kernel8.img is missing; run ARCH=aarch64 ./scripts/build.sh"
  [ -f "$ROOT/build/initramfs-aarch64.cpio" ] \
    || die "build/initramfs-aarch64.cpio is missing; run ./scripts/build-user-aarch64.sh"

  fetch_firmware

  rm -rf "$BOOT"
  mkdir -p "$BOOT/overlays"
  for file in $FIRMWARE_FILES; do
    cp "$CACHE/$file" "$BOOT/$file"
  done
  cp "$ROOT/build/kernel8.img" "$BOOT/kernel8.img"
  cp "$ROOT/build/initramfs-aarch64.cpio" "$BOOT/initramfs-aarch64.cpio"

  # The firmware reads this before it loads anything else.
  #
  # arm_64bit    start the processor in 64-bit mode and look for kernel8.img.
  # enable_uart  turn the serial console on and hold the clock steady.
  # dtoverlay    move the full serial port to the pins on the header. Without
  #              it that port is wired to the Bluetooth radio and a cable on
  #              the header sees nothing, whatever the kernel writes.
  # initramfs    load our ram disk and tell the kernel where it landed. The
  #              word takes a space rather than an equals sign, which is a
  #              quirk of this file rather than a mistake here. `followkernel`
  #              places it directly after the kernel image.
  cat > "$BOOT/config.txt" <<'EOF'
arm_64bit=1
enable_uart=1
dtoverlay=disable-bt
kernel=kernel8.img
initramfs initramfs-aarch64.cpio followkernel
EOF

  # Passed to the kernel as its command line, through the device tree.
  echo 'init=/bin/init' > "$BOOT/cmdline.txt"

  say ""
  say "assembled $BOOT:"
  ls -la "$BOOT" | sed 's/^/  /'
}

# ---------------------------------------------------------------------------
# Write it to a card
# ---------------------------------------------------------------------------

# Refuse anything that is not a removable disk. Naming the wrong device here
# destroys whatever was on it, and the usual wrong answer is the disk this
# machine booted from.
require_removable() {
  local device="$1"
  case "$(uname -s)" in
    Darwin)
      local info
      info="$(diskutil info "$device" 2>/dev/null)" \
        || die "$device: no such disk (diskutil info could not read it)"
      echo "$info" | grep -q "Removable Media:.*Removable" \
        || echo "$info" | grep -q "Device Location:.*External" \
        || die "$device is not a removable disk; refusing to write to it"
      echo "$info" | grep -q "Whole:.*Yes" \
        || die "$device is a partition, not a whole disk; name the disk itself"
      ;;
    Linux)
      local name
      name="$(basename "$device")"
      [ -b "$device" ] || die "$device is not a block device"
      [ -e "/sys/block/$name/removable" ] \
        || die "$device is not a whole disk; name the disk itself"
      [ "$(cat "/sys/block/$name/removable")" = "1" ] \
        || die "$device is not a removable disk; refusing to write to it"
      ;;
    *)
      die "writing a card is not implemented on $(uname -s); the files to copy onto a FAT32 partition are in $BOOT"
      ;;
  esac
}

confirm() {
  local device="$1"
  say ""
  say "About to ERASE $device and write the boot partition to it."
  case "$(uname -s)" in
    Darwin) diskutil list "$device" | sed 's/^/  /' ;;
    Linux)  lsblk "$device" | sed 's/^/  /' ;;
  esac
  say ""
  printf 'Everything on it will be lost. Type the device path again to go ahead: '
  local answer
  read -r answer
  [ "$answer" = "$device" ] || die "not confirmed; nothing was written"
}

write_card() {
  local device="$1"
  confirm "$device"

  case "$(uname -s)" in
    Darwin)
      diskutil unmountDisk "$device"
      # One FAT32 partition in an old-style partition table, which is the only
      # arrangement this bootloader understands.
      diskutil eraseDisk FAT32 CLAUDEOS MBRFormat "$device"
      local mount="/Volumes/CLAUDEOS"
      [ -d "$mount" ] || die "the card did not mount at $mount"
      cp -R "$BOOT"/ "$mount"/
      sync
      diskutil eject "$device"
      ;;
    Linux)
      local partition="${device}1"
      [ -b "$partition" ] || partition="${device}p1"
      umount "$device"* 2>/dev/null || true
      # One partition of type W95 FAT32 (LBA), marked bootable.
      printf 'label: dos\n,,c,*\n' | sfdisk "$device"
      sync
      mkfs.vfat -F 32 -n CLAUDEOS "$partition"
      local where
      where="$(mktemp -d)"
      mount "$partition" "$where"
      cp -R "$BOOT"/. "$where"/
      sync
      umount "$where"
      rmdir "$where"
      ;;
  esac

  say ""
  say "written. Put the card in the Pi, connect a serial cable to GPIO 14, 15"
  say "and ground, and read it at 115200 baud, 8 bits, no parity, one stop bit."
}

# ---------------------------------------------------------------------------

if [ "${1:-}" = "--dir" ]; then
  # A directory something else prepared, such as the EEPROM update files from
  # scripts/mkeeprom.sh, goes through the same checks and the same write.
  [ $# -eq 3 ] || die "usage: $0 --dir DIR /dev/diskN"
  require_removable "$3"
  [ -d "$2" ] || die "$2 is not a directory"
  BOOT="$(cd "$2" && pwd)"
  write_card "$3"
elif [ $# -ge 1 ]; then
  # Check the device before building anything, so a wrong one is refused at
  # once rather than after a page of output.
  require_removable "$1"
  assemble
  write_card "$1"
else
  assemble
  say ""
  say "no device named, so nothing was written."
  say "to write a card:  $0 /dev/diskN        (macOS)"
  say "                  $0 /dev/sdX          (Linux, as root)"
fi
