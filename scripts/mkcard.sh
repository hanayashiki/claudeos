#!/bin/bash
# Assemble the boot partition of an SD card for a Raspberry Pi 4, and put it on
# a card if you name one.
#
#   scripts/mkcard.sh                    assemble build/boot and stop
#   scripts/mkcard.sh --new /dev/disk4   assemble, erase the whole card, make its
#                                        two partitions, write the boot one,
#                                        and put user/data on /data
#   scripts/mkcard.sh /dev/disk4         assemble, and rewrite only the boot
#                                        partition of a card made with --new;
#                                        the /data partition is not touched
#   scripts/mkcard.sh --dir DIR [--new] /dev/disk4
#                                        the same with DIR in place of
#                                        build/boot, assembling nothing
#   scripts/mkcard.sh [--dir DIR] [--new] --image FILE
#                                        the same against a disk image file
#                                        instead of a card, on macOS, which is
#                                        how the writes here are tested
#
# A card made with --new has two partitions in an MBR:
#
#   1  FAT32, labelled CLAUDEOS, 512 MiB: the firmware, the kernel and the ram
#      disk. A Pi 4's bootloader lives in an EEPROM on the board and reads the
#      first FAT partition and nothing else, finding files by name, so there is
#      no boot sector to install and nothing to mark bootable.
#   2  FAT32, labelled CLAUDEDATA, the rest of the card: /data, which the
#      kernel mounts read-write (README.md, "/data on the SD card"). --new
#      puts the files under user/data on it: services.txt, the user services
#      list, and site/index.html, the page its web server serves.
#
# Updating a card rewrites partition 1 and nothing else, so what is on /data
# stays. The whole card is erased, and /data given its first files, only with
# --new.
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

# The two partitions a new card gets.
BOOT_LABEL=CLAUDEOS
BOOT_SIZE=512M
DATA_LABEL=CLAUDEDATA

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

# The board image, which scripts/build-user-aarch64.sh builds beside the test
# image and which has none of the test suites or the programs they drive.
IMAGE=initramfs-aarch64-board.cpio

assemble() {
  [ -f "$ROOT/build/kernel8.img" ] \
    || die "build/kernel8.img is missing; run ARCH=aarch64 ./scripts/build.sh"
  [ -f "$ROOT/build/$IMAGE" ] \
    || die "build/$IMAGE is missing; run ./scripts/build-user-aarch64.sh"

  # The image carries the digest of the kernel it was built after, and the
  # kernel checks itself against it at every boot. A kernel rebuilt since would
  # report itself DAMAGED on every boot of the card, so refuse the pair here.
  local manifest="$ROOT/build/rootfs-aarch64-board/etc/claudeos/checksums" built recorded
  built="$(python3 "$ROOT/tools/checksums.py" kernel "$ROOT/build/kernel-aarch64.elf")" \
    || die "cannot take the digest of build/kernel-aarch64.elf; run ARCH=aarch64 ./scripts/build.sh"
  recorded="$(sed -n 's/^\([0-9a-f]*\)  kernel$/\1/p' "$manifest" 2>/dev/null)"
  [ "$built" = "$recorded" ] \
    || die "build/$IMAGE was built against a different kernel than build/kernel8.img; run ./scripts/build-user-aarch64.sh"

  fetch_firmware

  rm -rf "$BOOT"
  mkdir -p "$BOOT/overlays"
  for file in $FIRMWARE_FILES; do
    cp "$CACHE/$file" "$BOOT/$file"
  done
  cp "$ROOT/build/kernel8.img" "$BOOT/kernel8.img"
  cp "$ROOT/build/$IMAGE" "$BOOT/$IMAGE"

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
  cat > "$BOOT/config.txt" <<EOF
arm_64bit=1
enable_uart=1
dtoverlay=disable-bt
kernel=kernel8.img
initramfs $IMAGE followkernel
EOF

  # Passed to the kernel as its command line, through the device tree.
  #
  # net=wifi  bring up the WiFi rather than the wired port, when the image
  #           carries a network to join in /etc/wifi.conf. The kernel's
  #           network stack holds one interface, and without this word it
  #           takes the wired port whether or not a cable is plugged in.
  local cmdline='init=/bin/init'
  if [ -f "$ROOT/build/rootfs-aarch64-board/etc/wifi.conf" ]; then
    cmdline="net=wifi $cmdline"
  fi
  echo "$cmdline" > "$BOOT/cmdline.txt"
  say "command line: $cmdline"

  say ""
  say "assembled $BOOT:"
  ls -la "$BOOT" | sed 's/^/  /'
}

# ---------------------------------------------------------------------------
# Which disk to write
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

# Attach the image file $1 as a disk without mounting anything, and print the
# disk's path. The disk is checked to be a disk image, so nothing this script
# does next can reach a physical disk.
attach_image() {
  local device
  device="$(hdiutil attach -nomount -imagekey diskimage-class=CRawDiskImage "$1" | awk 'NR == 1 { print $1 }')"
  case "$device" in
    /dev/disk[0-9]*) ;;
    *) die "hdiutil did not attach $1" ;;
  esac
  if ! diskutil info "$device" | grep -q "Protocol:.*Disk Image"; then
    hdiutil detach "$device" > /dev/null 2>&1 || true
    die "$device, attached from $1, does not say it is a disk image; nothing was written"
  fi
  echo "$device"
}

# A field of what diskutil knows about the disk or partition $1, such as
# VolumeName or MountPoint, or nothing.
volume_field() {
  diskutil info -plist "$1" 2>/dev/null | plutil -extract "$2" raw - 2>/dev/null || true
}

# The path of partition $2 of the disk $1 on Linux: sdb1, or mmcblk0p1.
linux_partition() {
  if [ -b "$1$2" ]; then echo "$1$2"; else echo "$1p$2"; fi
}

# Show the disk $2 and require its path to be typed again. $1 says what is
# about to happen to it.
confirm() {
  local what="$1" device="$2" answer
  say ""
  say "$what"
  case "$(uname -s)" in
    Darwin) diskutil list "$device" | sed 's/^/  /' ;;
    Linux)  lsblk -o NAME,SIZE,FSTYPE,LABEL "$device" | sed 's/^/  /' ;;
  esac
  say ""
  printf 'Type the device path again to go ahead: '
  read -r answer
  [ "$answer" = "$device" ] || die "not confirmed; nothing was written"
}

# ---------------------------------------------------------------------------
# Writing
# ---------------------------------------------------------------------------

# Where the partition $1 is mounted on macOS, mounting it if it is not.
mount_point() {
  local partition="$1" mount
  mount="$(volume_field "$partition" MountPoint)"
  if [ -z "$mount" ]; then
    diskutil mount "$partition" > /dev/null
    mount="$(volume_field "$partition" MountPoint)"
  fi
  [ -n "$mount" ] && [ -d "$mount" ] || die "$partition did not mount"
  echo "$mount"
}

# Delete what macOS put on the FAT volume at $2 while writing to it, and
# unmount the partition $1. The board and its firmware would see these as files:
# a `._NAME` beside a file, holding extended attributes, and .fseventsd,
# .Spotlight-V100 and .Trashes. Copying with -X keeps the source files'
# attributes from being copied, and still a card written that way had a `._`
# file beside every boot file. Unmounting straight after the deletion leaves
# nothing time to write them again; the board image section of scripts/test.sh
# lists both partitions of a card made this way and requires exactly the files
# copied.
tidy_and_unmount() {
  local partition="$1" mount="$2"
  find "$mount" -name '._*' -type f -exec rm -f {} +
  rm -rf "$mount/.fseventsd" "$mount/.Spotlight-V100" "$mount/.Trashes"
  sync
  diskutil unmount "$partition" > /dev/null
}

# Copy the boot files onto the partition $1, and unmount it.
copy_boot() {
  local partition="$1" mount
  case "$(uname -s)" in
    Darwin)
      mount="$(mount_point "$partition")"
      cp -RX "$BOOT"/ "$mount"/
      tidy_and_unmount "$partition" "$mount"
      ;;
    Linux)
      mount="$(mktemp -d)"
      command mount "$partition" "$mount"
      cp -R "$BOOT"/. "$mount"/
      sync
      umount "$mount"
      rmdir "$mount"
      ;;
  esac
}

# What a new card's /data starts with (README.md, "Services started at boot").
DATA_SEED="$ROOT/user/data"

# Copy the files under user/data onto the partition $1, and unmount it.
seed_data() {
  local partition="$1" mount
  case "$(uname -s)" in
    Darwin)
      mount="$(mount_point "$partition")"
      cp -RX "$DATA_SEED"/ "$mount"/
      tidy_and_unmount "$partition" "$mount"
      ;;
    Linux)
      mount="$(mktemp -d)"
      command mount "$partition" "$mount"
      cp -R "$DATA_SEED"/. "$mount"/
      sync
      umount "$mount"
      rmdir "$mount"
      ;;
  esac
}

# Erase the whole disk $1, make both partitions, write the boot files, and put
# the files under user/data on /data.
write_new() {
  local device="$1" p1 p2
  case "$(uname -s)" in
    Darwin)
      diskutil unmountDisk "$device"
      # MBR, which is the only partition table this bootloader reads.
      diskutil partitionDisk "$device" 2 MBR \
        FAT32 "$BOOT_LABEL" "$BOOT_SIZE" FAT32 "$DATA_LABEL" R
      copy_boot "${device}s1"
      seed_data "${device}s2"
      diskutil eject "$device"
      ;;
    Linux)
      umount "$device"* 2>/dev/null || true
      # Both of type W95 FAT32 (LBA), the first marked bootable.
      printf 'label: dos\n,%s,c,*\n,,c\n' "$BOOT_SIZE" | sfdisk "$device"
      partprobe "$device" 2>/dev/null || true
      udevadm settle 2>/dev/null || true
      p1="$(linux_partition "$device" 1)"
      p2="$(linux_partition "$device" 2)"
      mkfs.vfat -F 32 -n "$BOOT_LABEL" "$p1"
      mkfs.vfat -F 32 -n "$DATA_LABEL" "$p2"
      copy_boot "$p1"
      seed_data "$p2"
      ;;
  esac
}

# Reformat partition 1 of the disk $1 and write the boot files to it. The
# disk has to look like what write_new makes: partition 1 labelled CLAUDEOS
# and a partition 2 after it. On a card with one partition across it, that
# partition may be the one holding /data, so such a card is refused.
write_update() {
  local device="$1" p1 p2 label
  case "$(uname -s)" in
    Darwin)
      p1="${device}s1"
      p2="${device}s2"
      label="$(volume_field "$p1" VolumeName)"
      ;;
    Linux)
      p1="$(linux_partition "$device" 1)"
      p2="$(linux_partition "$device" 2)"
      label="$(lsblk -no LABEL "$p1" 2>/dev/null || true)"
      ;;
  esac
  if [ "$label" != "$BOOT_LABEL" ] || ! { [ -b "$p2" ] || diskutil info "$p2" > /dev/null 2>&1; }; then
    die "$device does not have partition 1 labelled $BOOT_LABEL and a partition 2 after it, which is what --new makes; nothing was written. To erase the whole card and make them, run: $0 --new $device"
  fi
  case "$(uname -s)" in
    Darwin)
      diskutil unmount "$p1" > /dev/null 2>&1 || true
      diskutil eraseVolume FAT32 "$BOOT_LABEL" "$p1"
      copy_boot "$p1"
      diskutil eject "$device"
      ;;
    Linux)
      umount "$p1" 2>/dev/null || true
      mkfs.vfat -F 32 -n "$BOOT_LABEL" "$p1"
      copy_boot "$p1"
      ;;
  esac
}

# ---------------------------------------------------------------------------

NEW=""
DIR=""
CARD_IMAGE=""
DEVICE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --new)   NEW=1; shift ;;
    --dir)   [ $# -ge 2 ] || die "--dir needs a directory"; DIR="$2"; shift 2 ;;
    --image) [ $# -ge 2 ] || die "--image needs a file"; CARD_IMAGE="$2"; shift 2 ;;
    -*)      die "unknown option $1; see the top of $0" ;;
    *)       [ -z "$DEVICE" ] || die "name one device"; DEVICE="$1"; shift ;;
  esac
done

if [ -z "$DEVICE" ] && [ -z "$CARD_IMAGE" ]; then
  [ -z "$NEW" ] || die "--new needs a device, or --image FILE, to erase"
  [ -z "$DIR" ] || die "--dir needs a device, or --image FILE, to write to"
  assemble
  say ""
  say "no device named, so nothing was written."
  say "to prepare a new card:       $0 --new /dev/diskN"
  say "to update its boot files:    $0 /dev/diskN"
  say "(/dev/sdX on Linux, as root)"
  exit 0
fi
[ -z "$DEVICE" ] || [ -z "$CARD_IMAGE" ] || die "name a device or --image FILE, not both"

# Check the target before building anything, so a wrong one is refused at once
# rather than after a page of output.
if [ -n "$DEVICE" ]; then
  require_removable "$DEVICE"
else
  [ "$(uname -s)" = Darwin ] || die "--image attaches the file with hdiutil, which only macOS has"
  [ -f "$CARD_IMAGE" ] || die "$CARD_IMAGE: no such file; make an empty one with: mkfile -n 1g $CARD_IMAGE"
fi

if [ -n "$DIR" ]; then
  # A directory something else prepared, such as the EEPROM update files from
  # scripts/mkeeprom.sh, goes through the same checks and the same write.
  [ -d "$DIR" ] || die "$DIR is not a directory"
  BOOT="$(cd "$DIR" && pwd)"
else
  assemble
fi

if [ -n "$CARD_IMAGE" ]; then
  DEVICE="$(attach_image "$CARD_IMAGE")"
  trap 'hdiutil detach "$DEVICE" > /dev/null 2>&1 || true' EXIT
  if [ -n "$NEW" ]; then write_new "$DEVICE"; else write_update "$DEVICE"; fi
  say ""
  say "written to $CARD_IMAGE."
  exit 0
fi

if [ -n "$NEW" ]; then
  confirm "About to ERASE all of $DEVICE, make partition 1 ($BOOT_LABEL, $BOOT_SIZE) and partition 2 ($DATA_LABEL, the rest), and write the boot files to partition 1. Everything on the card will be lost." "$DEVICE"
  write_new "$DEVICE"
else
  confirm "About to erase partition 1 ($BOOT_LABEL) of $DEVICE and write the boot files to it. Partition 2, which holds /data, is not touched." "$DEVICE"
  write_update "$DEVICE"
fi

say ""
say "written. Put the card in the Pi, connect a serial cable to GPIO 14, 15"
say "and ground, and read it at 115200 baud, 8 bits, no parity, one stop bit."
