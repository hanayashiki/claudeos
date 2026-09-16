# What goes in each userland image, and what the kernel checks at boot.
#
# Sourced by scripts/build-user.sh and scripts/build-user-aarch64.sh, which set
# ROOT and TARGET before they do.
#
# There are two images. The test image is what scripts/test.sh boots under
# QEMU: the suites, the programs they drive and the rtest applet. The board
# image is what scripts/mkcard.sh puts on the card and scripts/netboot.sh
# serves, and it holds only what the kernel and init need to boot and to reach
# the network and the console without the card's data partition. Every other
# program a board runs is on that partition, /data, which the kernel makes
# /usr (README.md, "Programs on the card"); CARD_ITEMS below is what a new
# card's /data starts with. Only the aarch64 build makes a board image and a
# card, because the board is a Raspberry Pi 4.
#
# A build script first puts everything it has into a staging tree. Each image,
# and the card, then takes the items listed for it here and nothing else, and a
# staged file that no line names stops the build.

# One item per line: which images it goes in (`both`, `test` or `board`), the
# item, and what it is. Moving an item from one image to the other is a change
# to the first word of its line.
#
# An item is a path in the image, which is a file or a directory with
# everything under it; `cbox:FEATURE`, a cargo feature cbox is built with; or
# `link:PATH=TARGET`, a symbolic link at PATH to TARGET. /bin/cbox is built once
# for each image with that image's features, and /bin gets a link for every
# applet that build has. /etc/motd is written for each image, and suggests only
# commands that image has. An item the build did not produce, such as busybox
# before scripts/fetch-busybox.sh has run, is left out. No item may be under
# /usr or /root: once the card is mounted the kernel replaces both with links
# into /data, and whatever the image held there is out of reach.
IMAGE_ITEMS='
both   /bin/cbox          the shell, init and the applets
test   /bin/busybox       upstream busybox, if fetched, for the suites; a board runs /usr/bin/busybox
test   /bin/busybox-extras  httpd, telnetd and nc from Alpine, on aarch64, if fetched
test   /lib/ld-musl-aarch64.so.1  the musl dynamic loader and libc busybox-extras runs under
board  link:/lib/ld-musl-aarch64.so.1=/usr/lib/ld-musl-aarch64.so.1  where the loader busybox-extras names is on a board
test   /bin/httpd         the web server: a link to busybox-extras on aarch64, to busybox on x86-64
test   /bin/cloudflared   the Cloudflare tunnel client, if fetched
test   /bin/hello_c       a C program built against musl
test   /bin/inet          the socket suite and a small HTTP server
test   cbox:rtest         the rtest applet, the Rust standard library suite
test   /tests/suite.sh    the userland suite
test   /tests/busybox.sh  the upstream busybox suite
test   /tests/demo.sh     the scripted tour
test   /tests/data.sh     the /data suite, run with a card in the emulated slot
test   /tests/hello.txt   a file to read back
test   /tests/go_main     the Go program under user/go, when the build makes it
both   /etc/passwd        the root account
both   /etc/hostname      the machine name
both   /etc/motd          the welcome text
both   /etc/claudeos/services  the system services init starts, from user/services
board  /etc/ntp.conf      the time servers ntpd asks to set the clock
both   /etc/wifi.conf     the network to join, if build/wifi.conf exists
both   /etc/ssl           the certificate store, if Alpine was fetched
both   /lib/firmware      the WiFi firmware, which the kernel loads before there is a network, if fetched
'

# What a new card's /data starts with, which scripts/mkcard.sh --new copies
# there and --usr copies the /usr part of, one item per line: the path on the
# card, where it comes from, and what it is. The source is a path in the
# staging tree when it starts with /, a path in the repository otherwise, and
# `-` for an empty directory. A staged program the build did not produce is
# left out, as for the images. The aarch64 build assembles these into
# build/data-aarch64.
#
# FAT has no symbolic links, so an applet name such as httpd cannot be a link
# to busybox-extras. /usr/bin/httpd is a two-line script that runs the applet
# instead: a copy would be a second busybox-extras to keep in step with the
# first, and a link in the image would put the card's layout in the image and
# give the program a name outside /usr/bin.
CARD_ITEMS='
/services.txt                  user/data/services.txt     the user services list
/site                          user/data/site             the page the site service serves
/root                          -                          the home directory, /root
/usr/bin/busybox               /bin/busybox               upstream busybox: ntpd, which the system list starts, and the other applets
/usr/bin/busybox-extras        /bin/busybox-extras        httpd, telnetd and nc from Alpine
/usr/bin/httpd                 user/data/usr/bin/httpd    runs busybox-extras httpd
/usr/lib/ld-musl-aarch64.so.1  /lib/ld-musl-aarch64.so.1  the musl dynamic loader busybox-extras names, through the image link at /lib
/usr/bin/cloudflared           /bin/cloudflared           the Cloudflare tunnel client
'

# What the kernel checks at boot against /etc/claudeos/checksums, one item per
# line: `kernel` for the kernel's own code and read-only data, or a path in the
# image. Each image's manifest has a line for every item that image has. The
# programs on the card are not here: the check runs before the card is
# mounted, and what is on /data is the user's to change.
# /etc/wifi.conf is not here either: it is the user's configuration, not
# software. /etc/claudeos/services is: it decides what runs at every boot.
CHECKSUM_ITEMS='
kernel
/bin/cbox
/etc/claudeos/services
/lib/firmware/brcm/brcmfmac43455-sdio.bin
/lib/firmware/brcm/brcmfmac43455-sdio.clm_blob
/lib/firmware/brcm/brcmfmac43455-sdio.txt
'

# Refuse a line whose first word names no image, rather than leave its item
# out of both, and an item under /usr or /root, which a mounted card hides.
check_image_items() {
  local images item bad=0
  while read -r images item _; do
    case "$images" in
      ""|both|test|board) ;;
      *) echo "scripts/images.sh: $item is listed for '$images', which is not both, test or board" >&2
         bad=1 ;;
    esac
    case "${item#link:}" in
      /usr|/usr/*|/root|/root/*)
        echo "scripts/images.sh: $item is under /usr or /root, which the kernel replaces with links into /data once the card is mounted" >&2
        bad=1 ;;
    esac
  done <<< "$IMAGE_ITEMS"
  return $bad
}

print_image_items() {
  echo "image contents, from scripts/images.sh:"
  printf '%s\n' "$IMAGE_ITEMS" | sed '/^$/d; s/^/  /'
}

# The items listed for image $1, one per line.
image_items() {
  local images item
  while read -r images item _; do
    case "$images" in
      both|"$1") printf '%s\n' "$item" ;;
    esac
  done <<< "$IMAGE_ITEMS"
}

# The cargo features cbox is built with for image $1, separated by spaces.
cbox_features() {
  image_items "$1" | sed -n 's/^cbox://p' | tr '\n' ' ' | sed 's/ *$//'
}

# Stop the build if the staging tree $1 holds a file that is not an item of an
# image or the source of an item of the card, and not inside a directory that
# is, so that a file a build script adds cannot be left out of all three
# without a word.
check_staged() {
  local path item found unlisted=0 items
  items="$(printf '%s\n' "$IMAGE_ITEMS" | awk 'NF >= 2 { print $2 }')
$(printf '%s\n' "$CARD_ITEMS" | awk 'NF >= 2 && $2 ~ /^\// { print $2 }')"
  while IFS= read -r path; do
    path="${path#.}"
    found=0
    for item in $items; do
      case "$path" in
        "$item"|"$item"/*) found=1; break ;;
      esac
    done
    if [ $found = 0 ]; then
      echo "$path was built but is in neither image nor on the card; give it a line in IMAGE_ITEMS or CARD_ITEMS in scripts/images.sh" >&2
      unlisted=1
    fi
  done < <(cd "$1" && find . -type f -o -type l)
  return $unlisted
}

# Build image $1 from the staging tree $2 into the tree $3 and the archive $4,
# with a manifest that holds the digest of the kernel ELF $5.
assemble_image() {
  local image="$1" stage="$2" tree="$3" archive="$4" kernel="$5"
  local features applets item applet motd=""
  features="$(cbox_features "$image")"
  # Taken here rather than in the loop's word list, where a failure of the
  # script would not stop the build.
  applets="$("$ROOT/scripts/list-applets.sh" "$ROOT/user/cbox/src/main.rs" $features)"
  echo
  echo "== the $image image, ${archive#$ROOT/}, cbox features: ${features:-none}"

  rm -rf "$tree"
  mkdir -p "$tree"/{bin,etc,root,tmp,dev,proc}
  for item in $(image_items "$image"); do
    case "$item" in
      cbox:*) ;;
      link:*)
        item="${item#link:}"
        mkdir -p "$(dirname "$tree${item%%=*}")"
        ln -sfn "${item#*=}" "$tree${item%%=*}"
        ;;
      /bin/cbox)
        (cd "$ROOT/user/cbox" && cargo build --release --target "$TARGET" --features "$features")
        cp "$ROOT/user/cbox/target/$TARGET/release/cbox" "$tree/bin/cbox"
        chmod +x "$tree/bin/cbox"
        for applet in $applets; do
          [ "$applet" = cbox ] || ln -sf cbox "$tree/bin/$applet"
        done
        # "[" cannot appear in the applet table's identifier list.
        ln -sf cbox "$tree/bin/["
        ;;
      /etc/motd)
        # Written once every other item is in, since it names some of them.
        motd=1
        ;;
      *)
        if [ -e "$stage$item" ] || [ -L "$stage$item" ]; then
          mkdir -p "$(dirname "$tree$item")"
          cp -Rp "$stage$item" "$tree$item"
        fi
        ;;
    esac
  done
  if [ -n "$motd" ]; then
    write_motd "$tree"
  fi

  python3 "$ROOT/tools/checksums.py" manifest "$tree" "$kernel" $CHECKSUM_ITEMS
  python3 "$ROOT/tools/mkcpio.py" "$tree" "$archive"
}

# Assemble what a new card's /data starts with, from CARD_ITEMS and the staging
# tree $1, into the tree $2.
assemble_card() {
  local stage="$1" tree="$2" path source
  echo
  echo "== what a new card's /data starts with, ${tree#$ROOT/}"
  rm -rf "$tree"
  mkdir -p "$tree"
  while read -r path source _; do
    [ -n "$path" ] || continue
    case "$source" in
      -) mkdir -p "$tree$path" ;;
      /*)
        if [ -e "$stage$source" ]; then
          mkdir -p "$(dirname "$tree$path")"
          cp -Rp "$stage$source" "$tree$path"
        fi
        ;;
      *)
        mkdir -p "$(dirname "$tree$path")"
        cp -Rp "$ROOT/$source" "$tree$path"
        ;;
    esac
  done <<< "$CARD_ITEMS"
  (cd "$tree" && find . | LC_ALL=C sort | sed 's|^\.||; /^$/d; s/^/  /')
}

# /etc/motd in the tree $1, suggesting only commands the tree has.
write_motd() {
  cat > "$1/etc/motd" <<MOTD
Welcome to claudeos.

This is a kernel written from scratch in Rust that implements enough of the
Linux system call interface to run unmodified static Linux binaries. The
userland you are talking to was built for $TARGET.

Try:  ls -l /bin | head      ps      free      cat /proc/cpuinfo
MOTD
  if [ -f "$1/tests/demo.sh" ]; then
    echo "      echo hi | tr a-z A-Z   sh /tests/demo.sh" >> "$1/etc/motd"
  else
    echo "      echo hi | tr a-z A-Z   cat /proc/claudeos/integrity" >> "$1/etc/motd"
  fi
  if [ -L "$1/bin/rtest" ] && [ -f "$1/bin/hello_c" ]; then
    echo "      rtest                  hello_c 60" >> "$1/etc/motd"
  fi
}
