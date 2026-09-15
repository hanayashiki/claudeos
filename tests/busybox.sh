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
echo "-- a fifo opened read-write --"
# O_RDWR on a FIFO is how a program holds one open without waiting for the
# other side. busybox's shell is what asks for it here: `<>` is not a
# redirection the shell this project ships understands.
bb mkfifo /tmp/bb/rw1 /tmp/bb/rw2 /tmp/bb/rw3
check "rdwr fifo takes a write" "0" \
    "$(bb sh -c 'exec 3<>/tmp/bb/rw1; echo hello >&3' 2>/dev/null; echo $?)"
# The write is what the read depends on, so a failed write skips the read
# rather than blocking on a fifo nothing will ever fill.
check "rdwr fifo reads it back" "hello" \
    "$(bb sh -c 'exec 3<>/tmp/bb/rw2; echo hello >&3 2>/dev/null && read -r l <&3 && echo "$l"')"
# Closing a read-write end gives back the writer it took as well as the
# reader. A writer left behind is one nothing can close, and a later reader
# waits for an end of file that never comes; `timeout` is what turns that
# wait into a failure rather than a hung suite.
bb sh -c 'exec 3<>/tmp/bb/rw3'
check "closing it gives both back" "0" \
    "$(bb printf 'x\n' > /tmp/bb/rw3 & bb timeout 5 /bin/busybox cat /tmp/bb/rw3 > /dev/null; echo $?)"

echo
echo "-- a web server --"
# /bin/httpd is this busybox on x86-64 and Alpine's busybox-extras on aarch64,
# which runs under musl's dynamic loader. Either has to serve a file to
# busybox wget over the loopback address.
if [ -e /bin/httpd ]; then
  bb mkdir -p /tmp/bb/www
  bb echo "served by httpd" > /tmp/bb/www/index.html
  /bin/httpd -f -p 8081 -h /tmp/bb/www &
  httpd=$!
  bb sleep 1
  check "httpd serves a file" "served by httpd" "$(bb wget -q -O - http://127.0.0.1:8081/index.html)"
  kill $httpd
  bb sleep 0.2
else
  check "httpd is installed" "/bin/httpd" "nothing"
fi
if [ "$machine" = aarch64 ]; then
  check "httpd is busybox-extras" "busybox-extras" "$(bb readlink /bin/httpd)"
  check "busybox-extras has httpd" "1" "$(/bin/busybox-extras --list | bb grep -c '^httpd$')"
  # The kernel hands a program with an interpreter in its program header to
  # that interpreter, so with the loader moved aside busybox-extras cannot
  # start, and with it back it runs again.
  bb mv /lib/ld-musl-aarch64.so.1 /lib/ld-musl-aarch64.so.1.away
  check "busybox-extras needs the loader" "refused" "$(/bin/busybox-extras --list > /dev/null 2>&1 || echo refused)"
  bb mv /lib/ld-musl-aarch64.so.1.away /lib/ld-musl-aarch64.so.1
  check "and runs with it back" "0" "$(/bin/busybox-extras --list > /dev/null 2>&1; echo $?)"
fi

echo
echo "-- terminal --"
check "tty"             "/dev/console" "$(bb tty)"
check "stty size"       "24 80"       "$(bb stty size)"

rm -rf /tmp/bb /tmp/bb.tar /tmp/extract /tmp/copy.txt
echo
echo "=== $pass passed, $fail failed ==="
