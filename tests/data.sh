# /data on an SD card, run by the data section of scripts/test.sh with a card
# image in the emulated Pi 4's slot. Which part runs comes from the kernel
# command line as `datatest=PART`, which reaches this script through init's
# environment:
#
#   write     the first boot: every operation, ending with a file for the
#             second boot, fsync, sync, and a line the harness waits for
#             before it reads the image from the Mac
#   verify    the second boot on the same image: what the first left is there
#   damaged   a card damaged on purpose: walk it, read it and write to it,
#             and reach the end whatever happens

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

T=/data/claudeos-test
LONG="An index page with a long name, spaces, and (brackets).html"

case "$datatest" in
write)
  echo "=== /data: the first boot ==="
  check "mounted as vfat"            "1"               "$(grep -c ' /data vfat rw' /proc/mounts)"
  check "a new directory"            "0"               "$(mkdir $T; echo $?)"
  echo "<h1>first</h1>" > "$T/$LONG"
  check "long name read back"        "<h1>first</h1>"  "$(cat "$T/$LONG")"
  check "long name listed"           "$LONG"           "$(ls $T)"
  check "long name found in any case" "<h1>first</h1>" "$(cat "$T/an INDEX page with a long name, spaces, and (brackets).HTML")"

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

  echo gone > $T/doomed
  rm $T/doomed
  check "unlink"                     "1"               "$(test -e $T/doomed; echo $?)"
  check "rmdir of a directory with files" "1"          "$(rmdir $T/d2/moved 2>/dev/null; echo $?)"
  rmdir $T/d1
  check "rmdir"                      "1"               "$(test -d $T/d1; echo $?)"

  check "a file's mode"              "-rw-r--r--"      "$(ls -l $T/sub/B.TXT | cut -c 1-10)"
  check "a directory's mode"         "drwxr-xr-x"      "$(ls -ld $T/sub | cut -c 1-10)"
  check "chmod succeeds"             "0"               "$(chmod 777 $T/sub/B.TXT; echo $?)"
  check "and changes nothing"        "-rw-r--r--"      "$(ls -l $T/sub/B.TXT | cut -c 1-10)"
  check "no hard links"              "1"               "$(ln $T/sub/B.TXT $T/link 2>/dev/null; echo $?)"
  check "no symbolic links"          "1"               "$(ln -s B.TXT $T/sub/sym 2>/dev/null; echo $?)"
  check "a directory on the card is not /" "0"         "$(test "$(stat -c %d /data)" != "$(stat -c %d /)"; echo $?)"

  echo "survives a reboot" > $T/persist.txt
  echo "reached the card" > $T/fsync.txt
  check "fsync"                      "0"               "$(fsync $T/fsync.txt; echo $?)"
  check "sync"                       "0"               "$(sync; echo $?)"
  echo "data-test: written; the image can be read"
  sleep 4
  ;;

verify)
  echo "=== /data: the second boot ==="
  check "mounted as vfat"            "1"               "$(grep -c ' /data vfat rw' /proc/mounts)"
  check "the file for this boot"     "survives a reboot" "$(cat $T/persist.txt)"
  check "the long name"              "<h1>first</h1>"  "$(cat "$T/$LONG")"
  check "the renamed file"           "one"             "$(cat $T/sub/B.TXT)"
  check "the moved directory"        "deep"            "$(cat $T/d2/moved/f)"
  check "the file renamed over"      "new"             "$(cat $T/target)"
  check "the truncated file"         "0"               "$(wc -c < $T/lines.txt)"
  check "nothing removed came back"  "1 1 1"           "$(test -e $T/doomed; a=$?; test -e $T/d1; b=$?; test -e $T/a.txt; echo $a $b $?)"
  rm $T/d2/moved/f
  rmdir $T/d2/moved
  rmdir $T/d2
  check "cleaned up"                 "1"               "$(test -e $T/d2; echo $?)"
  ;;

damaged)
  echo "=== /data: a damaged card ==="
  # Whatever is on the card, all of this has to come to an end, with errors
  # or without. Nothing is checked but that the end is reached.
  find /data > /tmp/walked 2>/dev/null
  echo "data-test: $(wc -l < /tmp/walked) names walked"
  find /data -type f 2>/dev/null | head -n 64 > /tmp/files
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
