#!/bin/sh
# /data on an SD card, run by the /data section of scripts/test.sh with a card
# image in the emulated Pi 4's slot. Which part runs comes from `datatest=PART`,
# given on the kernel command line or in front of the command:
#
#   write     the first boot: every operation, programs run from the card,
#             /usr and /root on it, then an fsync, a pause in which the harness
#             reads the image from the Mac, and a sync
#   verify    the second boot on the same image: what the first left is there
#   damaged   a card damaged on purpose, or no card: say what /usr and /root
#             are, walk /data, read it and write to it, and reach the end
#             whatever happens

pass=0
fail=0

check() {
  if [ "$2" = "$3" ]; then
    echo "PASS  $1"
    pass=$((pass + 1))
  else
    echo "FAIL  $1: expected [$2] got [$3]"
    fail=$((fail + 1))
  fi
}

# 1 when $1 and $2 are at most $3 apart, else 0.
near() {
  if [ $(($1 - $2)) -le "$3" ] && [ $(($2 - $1)) -le "$3" ]; then echo 1; else echo 0; fi
}

# What /usr or /root is: a link into /data, or a directory in memory and the
# names it holds.
where() {
  if [ -L "$1" ]; then
    echo "a link to $(readlink "$1")"
  elif [ -d "$1" ] && [ "$(stat -c %d "$1")" = "$(stat -c %d /)" ]; then
    echo "a directory in memory holding [$(ls -A "$1" | tr '\n' ' ' | sed 's/ $//')]"
  else
    echo "neither a link nor a directory in memory"
  fi
}

T=/data/claudeos-test
LONG="An index page with a long name, spaces, and (brackets).html"
LOADER=/lib/ld-musl-aarch64.so.1

case "$datatest" in
write)
  echo "=== /data: the first boot ==="
  check "mounted as vfat"            "1"               "$(grep -c ' /data vfat rw' /proc/mounts)"
  check "df names its device"        "/dev/mmcblk0p2 /data" "$(df /data | tail -n 1 | busybox awk '{ print $1, $6 }')"
  check "mount lists it"             "1"               "$(mount | grep -c '^/dev/mmcblk0p2 on /data type vfat (rw)$')"
  check "a new directory"            "0"               "$(mkdir $T; echo $?)"
  echo "<h1>first</h1>" > "$T/$LONG"
  check "long name read back"        "<h1>first</h1>"  "$(cat "$T/$LONG")"
  check "long name listed"           "$LONG"           "$(ls $T)"
  check "long name found in any case" "<h1>first</h1>" "$(cat "$T/an INDEX page with a long name, spaces, and (brackets).HTML")"
  check "its size"                   "15"              "$(stat -c %s "$T/$LONG")"

  seq 1 3000 > $T/lines.txt
  check "a file of many clusters"    "3000"            "$(wc -l < $T/lines.txt)"
  check "its last line"              "3000"            "$(tail -n 1 $T/lines.txt)"
  check "its length"                 "13893"           "$(wc -c < $T/lines.txt)"
  echo appended >> $T/lines.txt
  check "append"                     "appended"        "$(tail -n 1 $T/lines.txt)"
  echo rewritten > $T/lines.txt
  check "rewrite truncates"          "rewritten"       "$(cat $T/lines.txt)"
  check "length after the rewrite"   "10"              "$(wc -c < $T/lines.txt)"
  : > $T/lines.txt
  check "truncate to nothing"        "0"               "$(wc -c < $T/lines.txt)"

  # A file of 1288895 bytes, copied in from memory. The harness compares it
  # with the same `seq` output on the Mac after this boot.
  seq 1 200000 > /tmp/big.txt
  cp /tmp/big.txt $T/big.txt
  check "a large file copied in"     "1288895"         "$(wc -c < $T/big.txt)"
  check "a line from its middle"     "123456"          "$(sed -n 123456p $T/big.txt)"

  # More long names than one cluster of directory entries holds.
  mkdir $T/many
  for i in $(seq 1 300); do echo $i > "$T/many/a file with a long name, number $i.txt"; done
  check "300 long names listed"      "300"             "$(ls $T/many | wc -l)"
  check "one of them read back"      "217"             "$(cat "$T/many/a file with a long name, number 217.txt")"

  echo one > $T/a.txt
  mv $T/a.txt $T/b.txt
  check "rename: the old name gone"  "1"               "$(test -e $T/a.txt; echo $?)"
  check "rename: the new name"       "one"             "$(cat $T/b.txt)"
  mkdir $T/sub
  mv $T/b.txt $T/sub/b.txt
  check "rename into a directory"    "one"             "$(cat $T/sub/b.txt)"
  mv $T/sub/b.txt $T/sub/B.TXT
  check "rename changing only case"  "B.TXT"           "$(ls $T/sub)"
  echo old > $T/target
  echo new > $T/source
  mv $T/source $T/target
  check "rename over a file"         "new"             "$(cat $T/target)"
  check "and the source is gone"     "1"               "$(test -e $T/source; echo $?)"
  mkdir $T/d1
  mkdir $T/d1/inner
  echo deep > $T/d1/inner/f
  mkdir $T/d2
  mv $T/d1/inner $T/d2/moved
  check "move a directory"           "deep"            "$(cat $T/d2/moved/f)"
  check "into its own subdirectory"  "1"               "$(mv $T/d2 $T/d2/moved/x 2>/dev/null; echo $?)"
  check "a moved directory's .."     "moved"           "$(cd $T/d2/moved/.. && ls)"

  echo gone > $T/doomed
  rm $T/doomed
  check "unlink"                     "1"               "$(test -e $T/doomed; echo $?)"
  check "rmdir of a directory with files" "1"          "$(rmdir $T/d2/moved 2>/dev/null; echo $?)"
  rmdir $T/d1
  check "rmdir"                      "1"               "$(test -d $T/d1; echo $?)"

  check "a file's mode"              "-rwxrwxrwx"      "$(ls -l $T/sub/B.TXT | cut -c 1-10)"
  check "a directory's mode"         "drwxrwxrwx"      "$(ls -ld $T/sub | cut -c 1-10)"
  # vfat without `quiet`: EPERM for setuid, setgid and sticky and for another
  # owner or group, and success that changes nothing for every other mode.
  check "chmod to the mode it has"   "0"               "$(chmod 777 $T/sub/B.TXT; echo $?)"
  check "chmod to another mode"      "0"               "$(chmod 644 $T/sub/B.TXT; echo $?)"
  check "a directory to another"     "0"               "$(chmod 755 $T/sub; echo $?)"
  check "no write bits"              "0"               "$(chmod 555 $T/sub/B.TXT; echo $?)"
  check "setuid"                     "EPERM"           "$(chmod 4777 $T/sub/B.TXT 2>&1 | grep -q 'Operation not permitted' && echo EPERM)"
  check "setgid"                     "EPERM"           "$(chmod 2777 $T/sub/B.TXT 2>&1 | grep -q 'Operation not permitted' && echo EPERM)"
  check "sticky, on a directory"     "EPERM"           "$(chmod 1777 $T/sub 2>&1 | grep -q 'Operation not permitted' && echo EPERM)"
  check "and the modes are as they were" "-rwxrwxrwx drwxrwxrwx" "$(ls -l $T/sub/B.TXT | cut -c 1-10) $(ls -ld $T/sub | cut -c 1-10)"
  check "chown to root"              "0"               "$(busybox chown 0:0 $T/sub/B.TXT; echo $?)"
  check "chown to another owner"     "EPERM"           "$(busybox chown 1 $T/sub/B.TXT 2>&1 | grep -q 'Operation not permitted' && echo EPERM)"
  check "chgrp to another group"     "EPERM"           "$(busybox chgrp 1 $T/sub/B.TXT 2>&1 | grep -q 'Operation not permitted' && echo EPERM)"
  printf 'copied\n' > /tmp/mode644.txt
  chmod 644 /tmp/mode644.txt
  check "cp -p from memory"          "0 copied"        "$(busybox cp -p /tmp/mode644.txt $T/cp-p.txt 2>&1; echo $? $(cat $T/cp-p.txt))"
  check "mv from memory"             "0 1 copied"      "$(busybox mv /tmp/mode644.txt $T/mv.txt 2>&1; s=$?; test -e /tmp/mode644.txt; gone=$?; echo $s $gone $(cat $T/mv.txt))"
  check "and they read 0777"         "-rwxrwxrwx -rwxrwxrwx" "$(ls -l $T/cp-p.txt | cut -c 1-10) $(ls -l $T/mv.txt | cut -c 1-10)"
  check "no hard links"              "1"               "$(ln $T/sub/B.TXT $T/link 2>/dev/null; echo $?)"
  check "no symbolic links"          "1"               "$(ln -s B.TXT $T/sub/sym 2>/dev/null; echo $?)"
  check "a directory on the card is not /" "0"         "$(test "$(stat -c %d /data)" != "$(stat -c %d /)"; echo $?)"

  # Programs kept on the card, run from it: a static one and a script.
  cp /bin/busybox $T/busybox
  check "a static program on /data"  "run from the card" "$($T/busybox echo run from the card)"
  printf '#!/bin/sh\necho "a script on the card, given $1"\n' > $T/script
  check "a script on /data"          "a script on the card, given one" "$($T/script one)"

  # /usr and /root are the card's.
  check "/usr"                       "a link to /data/usr"  "$(where /usr)"
  check "/root"                      "a link to /data/root" "$(where /root)"
  echo "kept at home" > /root/claudeos-test-home.txt
  check "a file in /root is on /data" "kept at home"   "$(cat /data/root/claudeos-test-home.txt)"

  # A dynamic program on the card whose loader is on the card, reached as the
  # board image reaches it: the test image's own loader moved aside and its
  # path made a link to /usr/lib.
  mkdir -p /usr/bin /usr/lib
  cp /bin/busybox-extras /usr/bin/busybox-extras
  cp $LOADER /usr/lib/ld-musl-aarch64.so.1
  mv $LOADER /tmp/ld-musl.image
  ln -s /usr/lib/ld-musl-aarch64.so.1 $LOADER
  check "a dynamic program and its loader on /data" "1" "$(/usr/bin/busybox-extras --list | grep -c '^httpd$')"
  rm $LOADER
  mv /tmp/ld-musl.image $LOADER

  echo "survives a reboot" > $T/persist.txt
  modified="$(stat -c %Y $T/persist.txt)"
  check "mtime from the kernel clock" "1"              "$(near "$modified" "$(date +%s)" 60)"
  echo "$modified" > $T/persist.mtime

  echo "reached the card" > $T/fsync.txt
  check "fsync"                      "0"               "$(fsync $T/fsync.txt; echo $?)"
  # The harness reads the image during this pause, before the sync below: what
  # it finds there is what was on the card after fsync.
  echo "data-test: fsync done; the image can be read"
  sleep 8
  echo "data-test: syncing"
  check "sync"                       "0"               "$(sync; echo $?)"
  ;;

verify)
  echo "=== /data: the second boot ==="
  check "mounted as vfat"            "1"               "$(grep -c ' /data vfat rw' /proc/mounts)"
  check "the file for this boot"     "survives a reboot" "$(cat $T/persist.txt)"
  check "its mtime, to FAT's two seconds" "1"          "$(near "$(stat -c %Y $T/persist.txt)" "$(cat $T/persist.mtime)" 2)"
  check "the long name"              "<h1>first</h1>"  "$(cat "$T/$LONG")"
  check "the large file's length"    "1288895"         "$(wc -c < $T/big.txt)"
  check "the large file's last line" "200000"          "$(tail -n 1 $T/big.txt)"
  check "300 long names"             "300"             "$(ls $T/many | wc -l)"
  check "the renamed file"           "one"             "$(cat $T/sub/B.TXT)"
  check "the moved directory"        "deep"            "$(cat $T/d2/moved/f)"
  check "the file renamed over"      "new"             "$(cat $T/target)"
  check "the truncated file"         "0"               "$(wc -c < $T/lines.txt)"
  check "nothing removed came back"  "1 1 1"           "$(test -e $T/doomed; a=$?; test -e $T/d1; b=$?; test -e $T/a.txt; echo $a $b $?)"
  check "the program on /data"       "run again"       "$($T/busybox echo run again)"
  check "/root, still the card's"    "kept at home"    "$(cat /root/claudeos-test-home.txt)"
  rm $T/d2/moved/f
  rmdir $T/d2/moved
  rmdir $T/d2
  check "cleaned up"                 "1"               "$(test -e $T/d2; echo $?)"
  rm -r $T/many
  check "rm -r of 300 files"         "1"               "$(test -e $T/many; echo $?)"
  sync
  ;;

damaged)
  echo "=== /data: a damaged card ==="
  # What the harness compares with the data: line: links into /data when the
  # volume was mounted, in-memory directories when it was not.
  echo "data-test: /usr is $(where /usr)"
  echo "data-test: /root is $(where /root)"
  # Whatever is on the card, all of this has to come to an end, with errors
  # or without. Nothing is checked but that the end is reached. The walks stop
  # 16 levels down: damaged directory entries can point back at a directory
  # above them, and then the tree has no bottom and a walk without a limit goes
  # down until paths reach 4096 bytes, which is thousands of lookups.
  find /data -maxdepth 16 > /tmp/walked 2>/dev/null
  echo "data-test: $(wc -l < /tmp/walked) names walked"
  find /data -maxdepth 16 -type f 2>/dev/null | head -n 64 > /tmp/files
  while read -r name; do
    cat "$name" > /dev/null 2>&1
  done < /tmp/files
  ls -l /data/loopdir > /dev/null 2>&1
  echo written > /data/after-damage.txt 2>/dev/null
  cat /data/after-damage.txt > /dev/null 2>&1
  mkdir /data/after-damage 2>/dev/null
  mv /data/www /data/www-moved 2>/dev/null
  rm /data/big.bin 2>/dev/null
  rm /data/loop.bin 2>/dev/null
  sync
  echo "data-test: the damaged card was walked to the end"
  pass=1
  ;;

*)
  echo "data.sh: datatest=$datatest names no part"
  fail=1
  ;;
esac

echo
echo "=== $pass passed, $fail failed ==="
