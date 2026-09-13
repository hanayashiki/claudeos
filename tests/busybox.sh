# Run an upstream busybox against the kernel. busybox is not built by this
# project, so it exercises the system call interface as a third party.

if ! test -x /bin/busybox; then
  echo "busybox is not installed; run scripts/fetch-busybox.sh"
  echo "=== 0 passed, 0 failed ==="
  exit 0
fi

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

bb() { /bin/busybox "$@"; }

echo "=== upstream busybox on claudeos ==="
bb busybox | head -n 1
echo

rm -rf /tmp/bb /tmp/extract /tmp/bb.tar /tmp/copy.txt
bb mkdir -p /tmp/bb/deep
bb seq 1 100 > /tmp/bb/n.txt

echo "-- core --"
# Which machine this is comes from /proc/cpuinfo, whose fields are written per
# architecture, so the answer does not come from the call under test.
if bb grep -q "^CPU implementer" /proc/cpuinfo; then machine=aarch64; else machine=x86_64; fi
check "uname"           "Linux"       "$(bb uname)"
check "arch"            "$machine"    "$(bb uname -m)"
check "echo"            "hi"          "$(bb echo hi)"
check "seq and wc"      "100"         "$(bb seq 1 100 | bb wc -l)"
check "mkdir -p"        "0"           "$(bb test -d /tmp/bb/deep; echo $?)"
check "md5sum"          "d632eba71107bf7bc3ec423eab256d78" "$(bb md5sum /tmp/bb/n.txt | bb cut -d' ' -f1)"
check "sha256sum"       "93d4e5c77838e0aa5cb6647c385c810a7c2782bf769029e6c420052048ab22bb" "$(bb sha256sum /tmp/bb/n.txt | bb cut -d' ' -f1)"

echo
echo "-- text --"
check "sed"             "5"           "$(bb sed -n '5p' /tmp/bb/n.txt)"
check "awk constant"    "42"          "$(bb awk 'BEGIN { print 6*7 }')"
check "awk sum"         "5050"        "$(bb awk '{ t += $1 } END { print t }' /tmp/bb/n.txt)"
check "sort -rn"        "100"         "$(bb sort -rn /tmp/bb/n.txt | bb head -n 1)"
check "grep -c"         "1"           "$(bb grep -c '^42$' /tmp/bb/n.txt)"
check "tr"              "ABC"         "$(bb echo abc | bb tr a-z A-Z)"
check "cut"             "b"           "$(bb echo a:b:c | bb cut -d: -f2)"
check "xargs"           "1 2 3"       "$(bb echo '1 2 3' | bb xargs /bin/busybox echo)"
check "uniq"            "2"           "$(bb printf 'a\na\nb\n' | bb uniq | bb wc -l)"

echo
echo "-- files --"
bb tar cf /tmp/bb.tar -C /tmp bb
check "tar lists"       "3"           "$(bb tar tf /tmp/bb.tar | bb wc -l)"
bb mkdir -p /tmp/extract
bb tar xf /tmp/bb.tar -C /tmp/extract
check "tar round trip"  "d632eba71107bf7bc3ec423eab256d78" "$(bb md5sum /tmp/extract/bb/n.txt | bb cut -d' ' -f1)"
check "find"            "/tmp/bb/n.txt" "$(bb find /tmp/bb -name '*.txt')"
check "cp then cmp"     "0"           "$(bb cp /tmp/bb/n.txt /tmp/copy.txt; bb cmp /tmp/bb/n.txt /tmp/copy.txt; echo $?)"
check "stat size"       "292"         "$(bb stat -c %s /tmp/bb/n.txt)"
check "readlink"        "cbox"        "$(bb readlink /bin/ls)"
check "df runs"         "0"           "$(bb df > /dev/null; echo $?)"
check "du runs"         "0"           "$(bb du -s /tmp > /dev/null; echo $?)"

echo
echo "-- processes --"
check "ps sees init"    "1"           "$(bb ps | bb grep -c '/bin/init')"
check "sh arithmetic"   "7"           "$(bb sh -c 'echo $((3+4))')"
check "sh loop"         "6"           "$(bb sh -c 'n=0; for i in 1 2 3; do n=$((n+i)); done; echo $n')"
check "sh pipeline"     "5"           "$(bb sh -c 'seq 1 5 | wc -l')"
check "sh exit status"  "3"           "$(bb sh -c 'exit 3'; echo $?)"
check "sh reads a pipe" "beta"        "$(bb echo beta | bb sh -c 'read x; echo $x')"
check "sh own pid"      "yes"         "$(bb sh -c 'if [ $$ -gt 0 ]; then echo yes; fi')"
check "sleep"           "0"           "$(bb sleep 0.1; echo $?)"
check "timeout"         "0"           "$(bb timeout 5 /bin/busybox true; echo $?)"
check "nested busybox"  "deep"        "$(bb sh -c '/bin/busybox echo deep')"

echo
echo "-- terminal --"
check "tty"             "/dev/console" "$(bb tty)"
check "stty size"       "24 80"       "$(bb stty size)"

rm -rf /tmp/bb /tmp/bb.tar /tmp/extract /tmp/copy.txt
echo
echo "=== $pass passed, $fail failed ==="
