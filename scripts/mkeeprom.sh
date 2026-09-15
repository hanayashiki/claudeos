#!/bin/bash
# Prepare the update that rewrites a Raspberry Pi 4's bootloader EEPROM so the
# Pi boots from its SD card when the card holds boot files and otherwise asks
# this machine for them over the network, and put it on a card if you name one.
#
# A board already booting over the network also takes the update without a
# card: put build/eeprom/pieeprom.upd and pieeprom.sig in the directory
# scripts/netboot-serve.py serves, and the bootloader writes them into flash on
# its next network boot, because their timestamp is newer than its own.
#
#   scripts/mkeeprom.sh 192.168.86.43               prepare build/eeprom and stop
#   scripts/mkeeprom.sh 192.168.86.43 /dev/disk4    prepare, then erase that card
#                                                   and write it
#
# The address is the TFTP server the bootloader should ask: this machine's
# address on the network the Pi's ethernet is plugged into.
#
# The bootloader and its settings are one image in a flash chip on the board,
# and nothing outside the Pi can edit the settings in place, so the whole image
# is rewritten with a copy of Raspberry Pi's release image whose settings block
# has been changed. At power on the Pi 4's ROM looks for recovery.bin on the SD
# card before it runs the bootloader in flash. recovery.bin writes pieeprom.upd
# into flash if its sha256 is the one in pieeprom.sig, renames itself to
# RECOVERY.000 so the next power on does not write it again, and resets the
# board, which then runs the new bootloader.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/build/eeprom"
CONF="$ROOT/build/eeprom-config"
CACHE="$ROOT/build/thirdparty/eeprom"

# Raspberry Pi's bootloader image and the tools that edit it, which are not ours
# and are not in this repository. All four come from one commit of
# github.com/raspberrypi/rpi-eeprom, so a rerun gets the same bytes, and each is
# checked against the sha256 recorded here before it is used.
EEPROM_COMMIT="da7cd207d7ee97361ff76e1390634afae05aa3fa"
EEPROM_URL="https://raw.githubusercontent.com/raspberrypi/rpi-eeprom/$EEPROM_COMMIT"
# `default` is the release channel rpi-eeprom-update installs unless told
# otherwise, and the one Raspberry Pi Imager builds its recovery cards from.
# New bootloaders go to `latest` first and move to `default` once proven.
IMAGE="pieeprom-2026-05-17.bin"
ARTIFACTS="
firmware-2711/default/$IMAGE        f1da1bda48c8f19d6eccd94f160a2c8da48fd0acdbfa47f0ac58353208d188c3
firmware-2711/default/recovery.bin  9ec8816886f3938d962a837347d65ccc1d03e811a84bf1ad4b608906b288d995
rpi-eeprom-config                   dfa8e2a819e0922fc7fd948ef4fa55f44066fa04ccf5da210d880f0b35ad9d1b
rpi-eeprom-digest                   2885e3603f9d89995cf5b033f2a616bd1e932547c1d3cffff1ad98e427996a03
"

say() { printf '%s\n' "$*"; }
die() { printf '%s\n' "$*" >&2; exit 1; }

sha256() { shasum -a 256 "$1" | awk '{print $1}'; }

valid_ipv4() {
  echo "$1" | grep -Eq '^[0-9]{1,3}(\.[0-9]{1,3}){3}$' || return 1
  local IFS=. octet
  for octet in $1; do
    [ "$octet" -le 255 ] || return 1
  done
}

# ---------------------------------------------------------------------------
# Fetch Raspberry Pi's image and tools
# ---------------------------------------------------------------------------

fetch() {
  mkdir -p "$CACHE"
  local path hash file
  while read -r path hash; do
    [ -n "$path" ] || continue
    file="$CACHE/$(basename "$path")"
    if [ -s "$file" ] && [ "$(sha256 "$file")" = "$hash" ]; then continue; fi
    say "fetching $path"
    curl -fsSL --max-time 60 -o "$file" "$EEPROM_URL/$path" \
      || die "could not fetch $path; check the network and try again"
    [ "$(sha256 "$file")" = "$hash" ] \
      || die "$path does not have the sha256 recorded in $0; refusing to use it"
  done <<EOF
$ARTIFACTS
EOF
}

# rpi-eeprom-digest runs sha256sum, which some macOS releases do not have.
# shasum -a 256 prints the same line, hash first, and the hash is all the tool
# reads, so where sha256sum is missing a stand-in that runs shasum goes first on
# PATH.
provide_sha256sum() {
  command -v sha256sum >/dev/null && return
  local shim="$CACHE/bin"
  mkdir -p "$shim"
  printf '#!/bin/sh\nexec shasum -a 256 "$@"\n' > "$shim/sha256sum"
  chmod +x "$shim/sha256sum"
  PATH="$shim:$PATH"
  say "sha256sum not found; using shasum -a 256 in its place"
}

# ---------------------------------------------------------------------------
# Change the settings and build the files recovery.bin reads
# ---------------------------------------------------------------------------

# Set KEY=VALUE in a bootloader configuration file: replace the line that sets
# KEY if there is one, and add a line at the end if there is not.
set_key() {
  local file="$1" key="$2" value="$3"
  if grep -q "^$key=" "$file"; then
    awk -v k="$key" -v v="$value" \
      'index($0, k "=") == 1 { print k "=" v; next } { print }' \
      "$file" > "$file.new"
    mv "$file.new" "$file"
  else
    printf '%s=%s\n' "$key" "$value" >> "$file"
  fi
}

prepare() {
  local tftp_ip="$1"
  rm -rf "$OUT" "$CONF"
  mkdir -p "$OUT" "$CONF"

  # The configuration Raspberry Pi ships inside the image is the starting
  # point, so every setting not named below keeps the value they chose.
  python3 "$CACHE/rpi-eeprom-config" --out "$CONF/default.conf" "$CACHE/$IMAGE"

  # A line added at the end belongs to the last section header above it, so
  # adding lines is only right while [all] is the only section.
  if grep '^\[' "$CONF/default.conf" | grep -qv '^\[all\]$'; then
    die "the configuration in $IMAGE has a section other than [all]; settings added at its end would land in that section"
  fi

  cp "$CONF/default.conf" "$CONF/boot.conf"
  # Without a final newline the first added setting would join the last line.
  [ -z "$(tail -c 1 "$CONF/boot.conf")" ] || echo >> "$CONF/boot.conf"

  # BOOT_UART=1       print the bootloader's progress on the serial port on
  #                   GPIO 14 and 15 at 115200 baud. This Pi has no screen, so
  #                   the serial port is the only place a failed boot says why.
  # BOOT_ORDER=0xf21  the boot modes to try, read from the lowest hex digit up:
  #                   1 is the SD card, 2 is the network, and f starts again
  #                   from the lowest digit. A card made by mkcard.sh holds a
  #                   boot partition and boots on its own, with no wait on this
  #                   machine; a card without start4.elf, such as one holding
  #                   only /data, is passed over and the board boots from here.
  #                   So which way a board boots is decided by the card in it.
  # TFTP_IP           the TFTP server to ask for the boot files. Without it the
  #                   bootloader asks the server named in the DHCP answer, and
  #                   the home router that answers DHCP names none.
  #
  # TFTP_FILE_TIMEOUT is left at its default of 30000 ms for one whole file.
  # start4.elf, the largest file the bootloader itself fetches, took 7.4 s from
  # this machine over WiFi, and with the SD card tried first a card that boots
  # never waits on it.
  set_key "$CONF/boot.conf" BOOT_UART 1
  set_key "$CONF/boot.conf" BOOT_ORDER 0xf21
  set_key "$CONF/boot.conf" TFTP_IP "$tftp_ip"

  python3 "$CACHE/rpi-eeprom-config" --config "$CONF/boot.conf" \
    --out "$OUT/pieeprom.upd" "$CACHE/$IMAGE"
  # The first line of the .sig is the sha256 of the image and the second the
  # time it was made. -c 2711 adds a third naming the chip, as rpi-eeprom-update
  # and Raspberry Pi's own recovery cards do.
  sh "$CACHE/rpi-eeprom-digest" -c 2711 -i "$OUT/pieeprom.upd" -o "$OUT/pieeprom.sig"
  cp "$CACHE/recovery.bin" "$OUT/recovery.bin"

  # recovery.bin reads config.txt from the card. uart_2ndstage=1 has it print
  # what it is doing on the serial port, as the new bootloader will.
  echo 'uart_2ndstage=1' > "$OUT/config.txt"
}

print_pins() {
  say ""
  say "from github.com/raspberrypi/rpi-eeprom at commit $EEPROM_COMMIT:"
  local path hash
  while read -r path hash; do
    [ -n "$path" ] || continue
    say "  $hash  $path"
  done <<EOF
$ARTIFACTS
EOF
}

verify() {
  say ""
  say "settings changed from the configuration in $IMAGE:"
  diff "$CONF/default.conf" "$CONF/boot.conf" | sed 's/^/  /'

  say ""
  say "configuration read back out of $OUT/pieeprom.upd:"
  python3 "$CACHE/rpi-eeprom-config" --out "$CONF/readback.conf" "$OUT/pieeprom.upd"
  sed 's/^/  /' "$CONF/readback.conf"
  cmp -s "$CONF/readback.conf" "$CONF/boot.conf" \
    || die "the configuration read back out of pieeprom.upd is not the one written into it"

  say ""
  say "$OUT/pieeprom.sig:"
  sed 's/^/  /' "$OUT/pieeprom.sig"
  local recorded actual
  recorded="$(head -n 1 "$OUT/pieeprom.sig")"
  actual="$(sha256 "$OUT/pieeprom.upd")"
  say "shasum -a 256 of pieeprom.upd:"
  say "  $actual"
  [ "$recorded" = "$actual" ] \
    || die "pieeprom.sig records $recorded, which is not the sha256 of pieeprom.upd"
  say "  the same as the first line of pieeprom.sig"
}

# ---------------------------------------------------------------------------

[ $# -ge 1 ] && [ $# -le 2 ] || die "usage: $0 TFTP_SERVER_IP [/dev/diskN]"
valid_ipv4 "$1" || die "$1 is not a dotted decimal IPv4 address such as 192.168.86.43"

fetch
provide_sha256sum
prepare "$1"
print_pins
verify

say ""
say "prepared $OUT:"
ls -la "$OUT" | sed 's/^/  /'
say ""
say "On the Pi, recovery.bin writes pieeprom.upd to the EEPROM, renames itself to"
say "RECOVERY.000 and resets the board. The new bootloader then prints its progress"
say "on the serial port, boots from the SD card when it holds boot files, and
otherwise boots from the network."

if [ $# -ge 2 ]; then
  # --new: a recovery card is a card erased for the purpose, and mkcard.sh
  # without it only rewrites the boot partition of a card it made before.
  "$ROOT/scripts/mkcard.sh" --dir "$OUT" --new "$2"
else
  say ""
  say "no device named, so nothing was written."
  say "to write a card:  $0 $1 /dev/diskN        (macOS)"
  say "                  $0 $1 /dev/sdX          (Linux, as root)"
fi
