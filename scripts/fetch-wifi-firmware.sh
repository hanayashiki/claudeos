#!/bin/bash
# Fetch the firmware, NVRAM and regulatory data for the Raspberry Pi 4's WiFi
# chip, a Cypress CYW43455, and cache them under build/thirdparty/wifi-firmware.
#
#   scripts/fetch-wifi-firmware.sh
#
# The files are not ours and are not in this repository. They are the ones
# Raspberry Pi OS installs, from github.com/RPi-Distro/firmware-nonfree at one
# commit, so a rerun gets the same bytes, and each is checked against the
# sha256 recorded here before it is kept.
#
# Linux's brcmfmac asks for brcmfmac43455-sdio.raspberrypi,4-model-b.bin,
# .clm_blob and .txt on this board. In that repository those three names are
# symbolic links:
#
#   .bin       -> ../cypress/cyfmac43455-sdio.bin
#   .clm_blob  -> ../cypress/cyfmac43455-sdio.clm_blob
#   .txt       -> brcmfmac43455-sdio.txt
#
# and cyfmac43455-sdio.bin is not a file there either: the package's postinst
# makes it an alternative between -standard.bin, at priority 50, and
# -minimal.bin, at priority 10, so an installed system gets -standard. Those
# are the real files fetched below, saved under the names brcmfmac asks for.
#
# This takes -minimal.bin, not the -standard.bin an installed system gets,
# and it must not be "upgraded" back. The kernel joins a WPA2 network by
# letting the firmware's own supplicant do the key exchange, and only
# -minimal.bin has one. Its build line names "idsup" (the in-dongle
# supplicant) and "idauth"; -standard.bin's build line has neither. On the
# board, -standard.bin (7.45.265) answered setting "sup_wpa" to 1 with error
# -23, BCME_UNSUPPORTED. -minimal.bin is the older 7.45.241, from November
# 2021. Linux on Raspberry Pi OS uses -standard.bin and runs wpa_supplicant
# on the host instead.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="$ROOT/build/thirdparty/wifi-firmware"

# The bookworm branch, which is what Raspberry Pi OS bookworm installs.
COMMIT="c91cd2804cf7463aab913e7247c176049f16bbd6"
URL="https://raw.githubusercontent.com/RPi-Distro/firmware-nonfree/$COMMIT/debian/config/brcm80211"
ARTIFACTS="
cypress/cyfmac43455-sdio-minimal.bin   brcmfmac43455-sdio.bin       3075cb0bdc4b28ed4f08e01b1a216d0ebc70f4022d9d3272a4a43b3c90456e60
cypress/cyfmac43455-sdio.clm_blob      brcmfmac43455-sdio.clm_blob  9823842cae9fb9a5dd1e5fb31f595516ec7deee341354bef30bb3026eee29cc1
brcm/brcmfmac43455-sdio.txt            brcmfmac43455-sdio.txt       ca709be81a78bdb6932936374f39943acbd7af07fae6151011127599a3ce9e3d
"

say() { printf '%s\n' "$*"; }
die() { printf '%s\n' "$*" >&2; exit 1; }

sha256() { shasum -a 256 "$1" | awk '{print $1}'; }

mkdir -p "$CACHE"
while read -r path name hash; do
  [ -n "$path" ] || continue
  file="$CACHE/$name"
  if [ -s "$file" ] && [ "$(sha256 "$file")" = "$hash" ]; then
    say "have $name"
    continue
  fi
  say "fetching $path as $name"
  curl -fsSL --max-time 120 -o "$file.part" "$URL/$path" \
    || die "could not fetch $path; check the network and try again"
  [ "$(sha256 "$file.part")" = "$hash" ] \
    || { rm -f "$file.part"; die "$path does not have the sha256 recorded in $0; refusing to use it"; }
  mv "$file.part" "$file"
done <<EOF
$ARTIFACTS
EOF

say ""
say "from github.com/RPi-Distro/firmware-nonfree at commit $COMMIT:"
ls -l "$CACHE" | sed 's/^/  /'
