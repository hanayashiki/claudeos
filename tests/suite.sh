# claudeos self-test, run by the shell on the OS itself.
# Each check prints PASS or FAIL; a total is reported at the end.

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

echo "=== claudeos test suite ==="
echo

echo "-- shell --"
check "echo"              "hello"    "$(echo hello)"
check "echo -n"           "ab"       "$(echo -n ab)"
check "true status"       "0"        "$(true; echo $?)"
check "false status"      "1"        "$(false; echo $?)"
check "and-then"          "yes"      "$(true && echo yes)"
check "or-else"           "yes"      "$(false || echo yes)"
check "sequencing"        "b"        "$(echo a > /dev/null; echo b)"
check "variables"         "world"    "$(X=world; echo $X)"
check "braced variable"   "world"    "$(X=world; echo ${X})"
check "single quotes"     "a b"      "$(echo 'a b')"
check "double quotes"     "a b"      "$(echo "a b")"
check "arithmetic"        "7"        "$((3 + 4))"
check "arithmetic nested" "20"       "$(( (2 + 3) * 4 ))"
check "command sub"       "3"        "$(echo $(seq 1 3 | wc -l))"

echo
echo "-- pipes --"
check "one pipe"          "5"        "$(seq 1 5 | wc -l)"
check "two pipes"         "1"        "$(seq 1 9 | grep 3 | wc -l)"
check "three pipes"       "2"        "$(seq 1 100 | grep 7 | grep 1 | wc -l)"
check "tr ranges"         "ABC"      "$(echo abc | tr a-z A-Z)"
check "tr delete"         "ac"       "$(echo abc | tr -d b)"
check "rev"               "cba"      "$(echo abc | rev)"
check "head"              "1"        "$(seq 1 100 | head -n 1)"
check "tail"              "100"      "$(seq 1 100 | tail -n 1)"
check "sort -n first"     "1"        "$(seq 1 20 | sort -n | head -n 1)"
check "sort -r first"     "9"        "$(seq 1 9 | sort -r | head -n 1)"
check "uniq"              "2"        "$(printf 'a\na\nb\n' | uniq | wc -l)"
check "cut"               "b"        "$(echo 'a:b:c' | cut -d: -f2)"
check "wc -w"             "3"        "$(echo one two three | wc -w)"

echo
echo "-- files --"
mkdir -p /tmp/t
echo content > /tmp/t/file
check "write then read"   "content"  "$(cat /tmp/t/file)"
check "test -f"           "0"        "$(test -f /tmp/t/file; echo $?)"
check "test -d"           "0"        "$(test -d /tmp/t; echo $?)"
check "test missing"      "1"        "$(test -f /tmp/t/none; echo $?)"
check "byte count"        "8"        "$(wc -c < /tmp/t/file)"
cp /tmp/t/file /tmp/t/copy
check "copy"              "content"  "$(cat /tmp/t/copy)"
mv /tmp/t/copy /tmp/t/moved
check "move"              "content"  "$(cat /tmp/t/moved)"
rm /tmp/t/moved
check "remove"            "1"        "$(test -f /tmp/t/moved; echo $?)"
echo more >> /tmp/t/file
check "append"            "2"        "$(wc -l < /tmp/t/file)"
mkdir -p /tmp/t/sub
echo nested > /tmp/t/sub/deep
check "nested read"       "nested"   "$(cat /tmp/t/sub/deep)"
mkdir -p /tmp/g
echo one > /tmp/g/one.txt
echo two > /tmp/g/two.txt
echo three > /tmp/g/other
check "glob suffix"       "2"        "$(ls /tmp/g/*.txt | wc -l)"
check "glob all"          "3"        "$(ls /tmp/g/* | wc -l)"
check "find count"        "4"        "$(find /tmp/g | wc -l)"
ln -s /tmp/t/file /tmp/t/link
check "symlink follows"   "2"        "$(wc -l < /tmp/t/link)"
check "redirect in"       "content"  "$(head -n 1 < /tmp/t/file)"
check "seq to file"       "3"        "$(seq 1 3 > /tmp/t/n; wc -l < /tmp/t/n)"
check "stderr redirect"   ""         "$(cat /tmp/t/missing 2> /dev/null)"

echo
echo "-- devices --"
check "/dev/null read"    "0"        "$(wc -c < /dev/null)"
check "/dev/null write"   "0"        "$(echo discard > /dev/null; echo $?)"
check "/dev/zero"         "16"       "$(head -c 16 < /dev/zero | wc -c)"

echo
echo "-- processes --"
check "subshell ok"       "0"        "$(sh -c 'exit 0'; echo $?)"
check "subshell status"   "3"        "$(sh -c 'exit 3'; echo $?)"
check "nested shell"      "deep"     "$(sh -c 'sh -c "echo deep"')"
check "init is pid 1"     "1"        "$(ps | grep -c ' 1     0 ')"
check "for loop"          "6"        "$(total=0; for n in 1 2 3; do total=$((total + n)); done; echo $total)"
check "while loop"        "5"        "$(i=0; while [ $i -lt 5 ]; do i=$((i + 1)); done; echo $i)"
check "function"          "hi bob"   "$(greet() { echo hi $1; }; greet bob)"
check "if else"           "small"    "$(if [ 1 -gt 2 ]; then echo big; else echo small; fi)"

echo
echo "-- system --"
check "uname"             "Linux"    "$(uname)"
check "uname -m"          "x86_64"   "$(uname -m)"
check "hostname"          "claudeos" "$(hostname)"
check "whoami"            "root"     "$(whoami)"
check "proc version"      "1"        "$(grep -c claudeos /proc/version)"
check "proc meminfo"      "1"        "$(grep -c MemTotal /proc/meminfo)"
check "proc cpuinfo"      "1"        "$(grep -c processor /proc/cpuinfo)"
check "proc uptime"       "1"        "$(wc -l < /proc/uptime)"
check "proc self exe"     "0"        "$(ls -l /proc/self/exe > /dev/null; echo $?)"
check "basename"          "file"     "$(basename /tmp/t/file)"
check "dirname"           "/tmp/t"   "$(dirname /tmp/t/file)"
check "expr add"          "9"        "$(expr 4 + 5)"
check "printf"            "x=5"      "$(printf 'x=%d' 5)"

echo
echo "=== $pass passed, $fail failed ==="
rm -rf /tmp/t /tmp/g
