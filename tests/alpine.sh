#!/bin/sh
# Run inside an unmodified Alpine Linux root filesystem. Everything here is
# dynamically linked against musl and loaded by Alpine's own ld-musl, so this
# exercises the program interpreter path end to end.

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

echo "=== Alpine Linux on claudeos ==="
echo "release: $(cat /etc/alpine-release)"
busybox | head -n 1
echo

rm -rf /tmp/w
mkdir -p /tmp/w
seq 1 200 > /tmp/w/n.txt

# Which machine this is comes from /proc/cpuinfo, whose fields are written per
# architecture, so the answer does not come from the calls under test.
if grep -q "^CPU implementer" /proc/cpuinfo; then machine=aarch64; else machine=x86_64; fi

echo "-- dynamic loading --"
check "interpreter present" "0"       "$(test -x /lib/ld-musl-$machine.so.1; echo $?)"
check "echo"                "hi"      "$(echo hi)"
check "uname"               "Linux"   "$(uname)"
check "arch"                "$machine" "$(uname -m)"
check "shell is pid 1"      "1"       "$(echo $$)"

echo
echo "-- text tools --"
check "wc"                  "200"     "$(wc -l < /tmp/w/n.txt)"
check "awk"                 "20100"   "$(awk '{ t += $1 } END { print t }' /tmp/w/n.txt)"
check "sed"                 "100"     "$(sed -n '100p' /tmp/w/n.txt)"
check "grep regex"          "1"       "$(grep -c '^142$' /tmp/w/n.txt)"
check "sort -rn"            "200"     "$(sort -rn /tmp/w/n.txt | head -n 1)"
check "cut"                 "b"       "$(echo a:b:c | cut -d: -f2)"
check "tr"                  "ABC"     "$(echo abc | tr a-z A-Z)"
check "head -c"             "4"       "$(head -c 4 /tmp/w/n.txt | wc -c)"

echo
echo "-- digests and archives --"
check "md5sum"     "1d577c85e1a9ac3d9efa7be955a70d04" "$(printf 'claudeos\n' | md5sum | cut -d' ' -f1)"
check "sha256sum"  "b7703f7bd998bf1bd1b143ad055c4bbc828d0855b5be7d662747a48ef14c437a" "$(sha256sum /tmp/w/n.txt | cut -d' ' -f1)"
tar czf /tmp/w.tgz -C /tmp w
check "gzip tar"            "2"       "$(tar tzf /tmp/w.tgz | wc -l)"
mkdir -p /tmp/x
tar xzf /tmp/w.tgz -C /tmp/x
check "tar round trip"      "b7703f7bd998bf1bd1b143ad055c4bbc828d0855b5be7d662747a48ef14c437a" "$(sha256sum /tmp/x/w/n.txt | cut -d' ' -f1)"
check "gzip round trip"     "200"     "$(gzip < /tmp/w/n.txt | gunzip | wc -l)"

echo
echo "-- processes and files --"
check "subshell status"     "5"       "$(sh -c 'exit 5'; echo $?)"
check "pipeline"            "3"       "$(seq 1 3 | wc -l)"
check "find"                "1"       "$(find /tmp/w -name 'n.txt' | wc -l)"
check "stat size"           "692"     "$(stat -c %s /tmp/w/n.txt)"
check "readlink"            "0"       "$(test -L /bin/sh; echo $?)"
check "proc cpuinfo"        "1"       "$(grep -c processor /proc/cpuinfo)"
check "proc meminfo"        "1"       "$(grep -c MemTotal /proc/meminfo)"
check "ps sees itself"      "1"       "$(ps -o comm | grep -c ps)"
check "hard link"           "2"       "$(ln /tmp/w/n.txt /tmp/w/n2.txt; stat -c %h /tmp/w/n.txt)"
check "named pipe"          "fifo"    "$(mkfifo /tmp/w/p; echo fifo > /tmp/w/p & cat /tmp/w/p)"
check "descriptor list"     "1"       "$(ls /proc/self/fd | grep -c '^0$')"
check "kernel log"          "1"       "$(dmesg | grep -c 'claudeos: booting')"
check "nice"                "0"       "$(nice -n 5 true; echo $?)"
check "loop"                "6"       "$(n=0; for i in 1 2 3; do n=$((n+i)); done; echo $n)"
check "case"                "yes"     "$(case abc in a*) echo yes ;; *) echo no ;; esac)"
check "here-document"       "2"       "$(cat <<EOF | wc -l
one
two
EOF
)"

rm -rf /tmp/w /tmp/x /tmp/w.tgz
echo
echo "=== $pass passed, $fail failed ==="
