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
echo "-- expansion and builtins --"
check "escaped dollar"     'lit: $HOME'  "$(echo "lit: \$HOME")"
check "default value"      "def"         "$(unset U; echo ${U:-def})"
check "assign default"     "set set"     "$(unset U; echo ${U:=set} ${U})"
check "alternate value"    "yes"         "$(V=abc; echo ${V:+yes})"
check "string length"      "3"           "$(V=abc; echo ${#V})"
check "arithmetic error"   "1"           "$(echo $((1/0)) 2>/dev/null; echo $?)"
check "or-else break"      "3"           "$(for i in 1 2 3; do true || break; echo x; done | wc -l)"
check "plain var is local"  "0"          "$(MYVAR=hello; env | grep -c MYVAR)"
check "export publishes"    "1"          "$(MYVAR=hello; export MYVAR; env | grep -c MYVAR)"
check "prefix assignment"   "1"          "$(BAR=beer env | grep -c BAR)"
check "prefix is temporary" ""           "$(BAR=beer env > /dev/null; echo $BAR)"
check "set --"             "3 p"         "$(set -- p q r; echo $# $1)"
check "shift"              "2 q"         "$(set -- p q r; shift; echo $# $1)"
check "cd dash"            "/root"       "$(cd /bin; cd - > /dev/null; pwd)"
check "kill a job"         "0"           "$(sleep 20 & kill $! ; echo $?)"
check "command -v"         "0"           "$(command -v echo > /dev/null; echo $?)"
check "brace group"        "2"           "$({ echo a; echo b; } | wc -l)"

echo
echo "-- pipelines with compound commands --"
check "while read count"   "2"           "$(seq 1 2 | while read l; do echo "got:$l"; done | wc -l)"
check "while read value"   "got:2"       "$(seq 1 2 | while read l; do echo "got:$l"; done | tail -n 1)"
check "for into a pipe"    "3"           "$(for i in 1 2 3; do echo $i; done | wc -l)"
check "if in a pipe"       "yes"         "$(echo hi | if grep -q hi; then echo yes; fi)"
check "redirect a compound" "4"          "$(seq 1 4 > /tmp/rc.txt; while read l; do echo $l; done < /tmp/rc.txt | wc -l; rm -f /tmp/rc.txt)"
check "while sets a var"   "10"          "$(seq 1 4 > /tmp/rc.txt; t=0; while read n; do t=$((t+n)); done < /tmp/rc.txt; echo $t; rm -f /tmp/rc.txt)"
check "stderr to stdout"   "1"           "$(ls /nope > /tmp/b.txt 2>&1; grep -c . /tmp/b.txt; rm -f /tmp/b.txt)"
check "glob class"         "2"           "$(mkdir -p /tmp/gc; cd /tmp/gc; touch aa.txt ab.txt zz.txt; echo [ab][ab].txt | wc -w; cd /root; rm -rf /tmp/gc)"

echo
echo "-- keywords where a command cannot start --"
check "keyword as an argument"  "probe done"  "$(echo probe done)"
check "several of them"         "if then fi"  "$(echo if then fi)"
check "in a word list"          "dodone"      "$(for w in do done; do printf %s $w; done)"
check "a brace as an argument"  "}"           "$(echo })"
check "it still ends a command" "2"           "$(i=0; while [ $i -lt 2 ]; do i=$((i + 1)); done; echo $i)"

echo
echo "-- reading fields --"
check "read splits"        "a|b c"     "$(printf 'a b c\n' | while read one rest; do echo "$one|$rest"; done)"
check "read fills three"   "a-b-c"     "$(printf 'a b c\n' | while read x y z; do echo "$x-$y-$z"; done)"
check "IFS chooses"        "a-b-c"     "$(printf 'a:b:c\n' | while IFS=: read p q r; do echo "$p-$q-$r"; done)"
check "surrounding space"  "spaced"    "$(printf '   spaced   \n' | while read a; do echo "$a"; done)"
check "missing fields"     "a--"       "$(printf 'a\n' | while read x y z; do echo "$x-$y-$z"; done)"

echo
echo "-- status and scope --"
check "while reports the body" "0"     "$(printf 'x\n' | while read l; do true; done; echo $?)"
check "while keeps a failure"  "1"     "$(printf 'x\n' | while read l; do false; done; echo $?)"
check "for with no words"      "0"     "$(for i in; do echo x; done; echo $?)"
check "quoted parameters"      "<p q> <r>" "$(set -- 'p q' r; for x in "$@"; do printf '<%s> ' "$x"; done | sed 's/ $//')"
check "no parameters"          "none"  "$(set --; for x in "$@"; do echo item; done; echo none)"
check "printf reuses"          "3"     "$(printf '%s\n' one two three | wc -l)"
check "printf without one"     "1"     "$(printf 'plain\n' | wc -l)"
check "local is local"         "outer" "$(f() { local V=inner; }; V=outer; f; echo $V)"
check "local restores nothing"  ""     "$(f() { local V=inner; }; unset V; f; echo $V)"

echo
echo "-- unfinished input --"
printf 'echo ran\nwhile true\ndo\n  echo body\n' > /tmp/unfinished.sh
check "the prefix still runs" "ran"    "$(sh /tmp/unfinished.sh 2>/dev/null)"
check "and it fails"          "2"      "$(sh /tmp/unfinished.sh >/dev/null 2>&1; echo $?)"
check "and says so"           "1"      "$(sh /tmp/unfinished.sh 2>&1 >/dev/null | grep -c 'end of input')"
rm -f /tmp/unfinished.sh

echo
echo "-- sed --"
check "a group"            "42"        "$(echo 'id=42 end' | sed 's/.*id=\([0-9]*\) .*/\1/')"
check "two groups"         "b-a"       "$(echo a-b | sed -E 's/(a)-(b)/\2-\1/')"
check "extended plus"      "Xb"        "$(echo aab | sed -E 's/a+/X/')"
check "extended alternation" "Zb"      "$(echo aab | sed -E 's/z|aa/Z/')"
check "basic alternation"  "2"         "$(printf 'cat\ndog\nfox\n' | sed -n '/^\(cat\|dog\)$/p' | wc -l)"
check "grep -o"            "2"         "$(printf 'foo123bar456\n' | grep -o '[0-9][0-9]*' | wc -l)"
check "grep -o content"    "123"       "$(printf 'foo123bar456\n' | grep -o '[0-9][0-9]*' | head -n 1)"

echo
echo "-- signals and jobs --"
check "kill 0 finds a job" "0"         "$(sleep 5 & p=$!; kill -0 $p; echo $?; kill $p)"
check "kill 0 misses one"  "1"         "$(kill -0 999999 2>/dev/null; echo $?)"
check "kill names no job"  "1"         "$(kill %9 2>/dev/null; echo $?)"
check "no usage for a job" "0"         "$(kill %9 2>&1 | grep -c usage)"

echo
echo "-- grep patterns --"
check "anchor start"   "1"  "$(seq 1 100 | grep -c '^42$')"
check "anchor end"     "10" "$(seq 1 100 | grep -c '0$')"
check "dot"            "2"  "$(printf 'cat\ncot\ncar\n' | grep -c '^c.t$')"
check "star"           "3"  "$(printf 'ab\naab\naaab\n' | grep -c '^a*b$')"
check "class"          "2"  "$(printf 'cat\ncot\ncut\n' | grep -c '^c[ao]t$')"
check "negated class"  "1"  "$(printf 'cat\ncot\ncut\n' | grep -c '^c[^ao]t$')"
check "range"          "5"  "$(seq 1 20 | grep -c '^1[0-4]$')"
check "escaped dot"    "1"  "$(printf 'a.b\naxb\n' | grep -c '^a\.b$')"
check "extended plus"  "2"  "$(printf 'ab\naab\nb\n' | grep -cE '^a+b$')"
check "extended alt"   "2"  "$(printf 'cat\ndog\nfox\n' | grep -cE '^(cat|dog)$')"
check "extended opt"   "2"  "$(printf 'color\ncolour\ncolr\n' | grep -cE '^colou?r$')"
check "fixed strings"  "1"  "$(printf 'a.b\naxb\n' | grep -cF 'a.b')"
check "invert"         "99" "$(seq 1 100 | grep -vc '^42$')"
check "ignore case"    "2"  "$(printf 'Cat\ncat\ndog\n' | grep -ci '^cat$')"
check "quiet status"   "1"  "$(seq 1 10 | grep -q '^99$'; echo $?)"
check "recursive"      "2"  "$(grep -rl claudeos /etc | wc -l)"

echo
echo "-- sed --"
check "substitute"        "hi there"  "$(echo hi world | sed 's/world/there/')"
check "global"            "bbb"       "$(echo aaa | sed 's/a/b/g')"
check "the second one"    "aba"       "$(echo aaa | sed 's/a/b/2')"
check "the whole match"   "[cat]"     "$(echo cat | sed 's/c.t/[&]/')"
check "anchored"          "Xb"        "$(printf 'ab\nba\n' | sed 's/^a/X/' | head -n 1)"
check "delete a line"     "4"         "$(seq 1 5 | sed '2d' | wc -l)"
check "print one line"    "3"         "$(seq 1 5 | sed -n '3p')"
check "the last line"     "5"         "$(seq 1 5 | sed -n '$p')"
check "a range"           "3"         "$(seq 1 10 | sed -n '3,5p' | wc -l)"
check "a pattern address" "2"         "$(printf 'one\ntwo\nthree\n' | sed -n '/^t/p' | wc -l)"
check "negated"           "3"         "$(seq 1 4 | sed -n '2!p' | wc -l)"
check "quit early"        "3"         "$(seq 1 10 | sed '3q' | wc -l)"
check "line numbers"      "2"         "$(printf 'a\nb\n' | sed -n '=' | tail -n 1)"
check "transliterate"     "xyz"       "$(echo abc | sed 'y/abc/xyz/')"
check "two scripts"       "1b3"       "$(echo abc | sed -e 's/a/1/' -e 's/c/3/')"
check "another delimiter" "/opt/bin"  "$(echo /usr/bin | sed 's|/usr|/opt|')"
check "a character class" "ab"        "$(echo a1b2 | sed 's/[0-9]//g')"
check "substitute and print" "A"      "$(printf 'a\nb\n' | sed -n 's/a/A/p')"
rm -f /tmp/sed.txt
printf 'one\ntwo\n' > /tmp/sed.txt
sed -i 's/one/1/' /tmp/sed.txt
check "in place"          "1"         "$(head -n 1 /tmp/sed.txt)"
rm -f /tmp/sed.txt

echo
echo "-- xargs --"
check "one line"          "args: 1 2 3" "$(echo '1 2 3' | xargs echo args:)"
check "one at a time"     "3"           "$(printf 'a\nb\nc\n' | xargs -n 1 echo | wc -l)"
check "replacing a marker" "[x]"        "$(printf 'x\n' | xargs -I {} echo '[{}]')"
check "nothing to do"     "0"           "$(echo '' | xargs -r echo ran | wc -l)"
check "quotes group"      "one two"     "$(echo \"'one two'\" | xargs -n 1 echo | head -n 1)"

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
echo "-- coreutils --"
rm -rf /tmp/cu; mkdir -p /tmp/cu/d2/d3
echo a > /tmp/cu/a.txt; echo b > /tmp/cu/d2/b.log; echo c > /tmp/cu/d2/d3/c.txt
check "find -name"         "2"           "$(find /tmp/cu -name '*.txt' | wc -l)"
check "find -type d"       "3"           "$(find /tmp/cu -type d | wc -l)"
check "find -maxdepth"     "3"           "$(find /tmp/cu -maxdepth 1 | wc -l)"
check "find -exec"         "1"           "$(find /tmp/cu -name '*.log' -exec wc -l {} ';' | tr -s ' ' | cut -d' ' -f2)"
check "ls -l has an owner" "root"        "$(ls -l /tmp/cu/a.txt | tr -s ' ' | cut -d' ' -f3)"
check "ls -l has a time"   "1"           "$(ls -l /tmp/cu/a.txt | grep -c ':')"
check "ls -d"              "/tmp/cu"     "$(ls -d /tmp/cu)"
# The only check here that needs the fetched busybox rather than this
# project's own tools, because nothing of ours prints a raw timestamp.
if [ -x /bin/busybox ]; then
  check "mtime is set"     "0"           "$(test $(busybox stat -c %Y /tmp/cu/a.txt) -gt 1000000000; echo $?)"
else
  echo "SKIP  mtime is set (needs upstream busybox)"
fi
check "tr escapes"         "a:b:"        "$(printf 'a\nb\n' | tr '\n' ':')"
check "sort -n with text"  "1 a"         "$(printf '3 c\n1 a\n2 b\n' | sort -n | head -n 1)"
check "sort -n decimals"   "1.25"        "$(printf '1.5\n1.25\n10\n' | sort -n | head -n 1)"
check "uniq -d"            "a"           "$(printf 'a\na\nb\n' | uniq -d)"
check "uniq -u"            "b"           "$(printf 'a\na\nb\n' | uniq -u)"
check "test -a"            "0"           "$(test -f /tmp/cu/a.txt -a -d /tmp/cu; echo $?)"
check "test -o"            "0"           "$(test -f /tmp/none -o -d /tmp/cu; echo $?)"
check "test negation"      "0"           "$(test ! -f /tmp/none; echo $?)"
check "printf width"       "str|42| 3.14|ff" "$(printf '%s|%d|%5.2f|%x' str 42 3.14159 255)"
check "echo -e"            "2"           "$(echo -e 'a\nb' | wc -l)"
check "cut -c"             "bcd"         "$(printf 'abcdef\n' | cut -c2-4)"
check "cat -e"             "one\$"        "$(printf 'one\n' | cat -e)"
check "expr length"        "5"           "$(expr length abcde)"
check "expr divide by 0"   "2"           "$(expr 1 / 0 2>/dev/null; echo $?)"
check "wc -L"              "4"           "$(printf 'a\nabcd\nab\n' | wc -L)"
check "readlink"           "cbox"        "$(readlink /bin/ls)"
check "ln -sf replaces"    "0"           "$(ln -sf /tmp/cu/a.txt /tmp/cu/L; ln -sf /tmp/cu/a.txt /tmp/cu/L; echo $?)"
check "date +format"       "1"           "$(date +%Y-%m-%d | grep -c '^[0-9][0-9][0-9][0-9]-')"
check "error has no code"  "0"           "$(ls /nope 2>&1 | grep -c 'os error')"
check "df tracks files"    "0"           "$(df > /dev/null; echo $?)"
rm -rf /tmp/cu

echo
echo "-- endless writers and background jobs --"
check "yes into head"      "3"           "$(yes | head -3 | wc -l)"
check "failed write fails" "1"           "$(echo x > /dev/full 2>/dev/null; echo $?)"
check "head -c on a device" "8"          "$(head -c 8 /dev/zero | wc -c)"
check "background reaped"  "0"           "$(sleep 1 & sleep 2; ps | grep -c ' Z ')"

echo
echo "-- links, named pipes and /proc/self/fd --"
rm -rf /tmp/lk; mkdir -p /tmp/lk
echo hi > /tmp/lk/a
ln /tmp/lk/a /tmp/lk/b
check "hard link reads"      "hi"       "$(cat /tmp/lk/b)"
check "two names, one file"  "2"        "$(stat -c %h /tmp/lk/a)"
echo changed > /tmp/lk/b
check "writes are shared"    "changed"  "$(cat /tmp/lk/a)"
check "same inode"           "0"        "$(test $(stat -c %i /tmp/lk/a) -eq $(stat -c %i /tmp/lk/b); echo $?)"
rm /tmp/lk/b
check "unlink drops a name"  "1"        "$(stat -c %h /tmp/lk/a)"
check "the file is still there" "changed" "$(cat /tmp/lk/a)"
mkfifo /tmp/lk/f
check "mkfifo makes a fifo"  "0"        "$(test -p /tmp/lk/f; echo $?)"
check "a fifo carries data"  "through"  "$(echo through > /tmp/lk/f & cat /tmp/lk/f)"
# The writer can finish before the reader looks, so waiting for a writer to be
# present is not enough to meet one.
cat > /tmp/lk/race.sh <<'RACE'
n=0
i=0
while [ $i -lt 20 ]; do
  rm -f /tmp/lk/r
  mkfifo /tmp/lk/r
  sleep 0.01 &
  v=$(echo x > /tmp/lk/r & cat /tmp/lk/r)
  if [ "$v" = x ]; then n=$((n + 1)); fi
  i=$((i + 1))
done
echo $n
RACE
check "a fifo meets a fast writer" "20"  "$(sh /tmp/lk/race.sh)"
check "descriptors are listed" "1"      "$(ls /proc/self/fd | grep -c '^0$')"
check "a descriptor names its file" "/tmp/lk/a" "$(sh -c 'readlink /proc/self/fd/0' < /tmp/lk/a)"
check "a descriptor is a link" "l"      "$(sh -c 'ls -l /proc/self/fd/1' | cut -c1)"
check "writing to /dev/stdout"  "out"   "$(sh -c 'echo out > /dev/stdout')"
check "reading /dev/stdin"      "in"    "$(echo in | sh -c 'cat /dev/stdin')"
check "every descriptor is listed" "4"  "$(sh -c 'ls /proc/self/fd' | wc -l)"
check "dmesg has the boot log" "1"      "$(dmesg | grep -c 'claudeos: booting')"
rm -rf /tmp/lk

echo
echo "-- stopping and continuing --"
sleep 30 &
stopped=$!
kill -STOP $stopped
check "a stopped job shows T"  "1"  "$(ps | grep -c ' T ')"
kill -CONT $stopped
check "continuing clears it"   "0"  "$(ps | grep -c ' T ')"
kill -9 $stopped
check "kill reaches a stopped job" "0" "$(kill -STOP $stopped 2>/dev/null; sleep 1; ps | grep -c 'sleep 30')"

echo
echo "-- syntax errors --"
printf 'echo before\nfor i in 1 2\necho no do\n' > /tmp/bad.sh
check "commands before it run" "before"  "$(sh /tmp/bad.sh 2>/dev/null)"
check "status is 2"            "2"       "$(sh /tmp/bad.sh >/dev/null 2>&1; echo $?)"
check "the line is reported"   "1"       "$(sh /tmp/bad.sh 2>&1 >/dev/null | grep -c 'line 3: syntax error')"
check "the script is named"    "1"       "$(sh /tmp/bad.sh 2>&1 >/dev/null | grep -c 'bad.sh')"
check "the token is quoted"    "1"       "$(sh /tmp/bad.sh 2>&1 >/dev/null | grep -c 'found .echo.')"
rm -f /tmp/bad.sh

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
echo "-- here-documents, case, negation, subshells --"
rm -rf /tmp/h
mkdir -p /tmp/h
cat > /tmp/h/plain.txt <<EOF
first line
second line
EOF
check "here-document"      "2"          "$(wc -l < /tmp/h/plain.txt)"
who=world
cat > /tmp/h/expanded.txt <<EOF
hello $who
EOF
check "here-doc expands"   "hello world" "$(cat /tmp/h/expanded.txt)"
cat > /tmp/h/literal.txt <<'EOF'
hello $who
EOF
check "quoted delimiter"   'hello $who' "$(cat /tmp/h/literal.txt)"
sort <<EOF > /tmp/h/sorted.txt
charlie
alpha
bravo
EOF
check "here-doc as stdin"  "alpha"      "$(head -n 1 /tmp/h/sorted.txt)"

kind() {
  case "$1" in
    *.txt)   echo text ;;
    *.sh)    echo script ;;
    a|b|c)   echo letter ;;
    "")      echo empty ;;
    *)       echo other ;;
  esac
}
check "case first arm"     "text"       "$(kind notes.txt)"
check "case later arm"     "script"     "$(kind run.sh)"
check "case alternatives"  "letter"     "$(kind b)"
check "case empty pattern" "empty"      "$(kind '')"
check "case default"       "other"      "$(kind readme.md)"

check "negate false"       "0"          "$(! false; echo $?)"
check "negate true"        "1"          "$(! true; echo $?)"
check "negate in if"       "yes"        "$(if ! false; then echo yes; fi)"
check "negate a pipeline"  "0"          "$(! seq 1 3 | grep -q 99; echo $?)"

check "subshell variable"  "outer"      "$(v=outer; (v=inner); echo $v)"
check "subshell directory" "/root"      "$( (cd /tmp) ; pwd )"
check "subshell output"    "2"          "$( (echo a; echo b) | wc -l )"
check "subshell status"    "4"          "$( (exit 4) ; echo $? )"
rm -rf /tmp/h

echo
echo "-- scripts --"
rm -rf /tmp/s
mkdir -p /tmp/s
printf '#!/bin/sh\necho shebang-ok\n' > /tmp/s/withbang.sh
printf 'echo bare-ok\n' > /tmp/s/nobang.sh
printf '#!/bin/sh\necho "$1 $2"\n' > /tmp/s/args.sh
printf 'echo nope\n' > /tmp/s/plain.sh
chmod +x /tmp/s/withbang.sh /tmp/s/nobang.sh /tmp/s/args.sh
check "script with shebang" "shebang-ok" "$(/tmp/s/withbang.sh)"
check "script without one"  "bare-ok"    "$(/tmp/s/nobang.sh)"
check "relative path"       "shebang-ok" "$(cd /tmp/s && ./withbang.sh)"
check "script arguments"    "a b"        "$(/tmp/s/args.sh a b)"
check "sh runs a script"    "bare-ok"    "$(sh /tmp/s/nobang.sh)"
check "not executable"      "126"        "$(/tmp/s/plain.sh 2>/dev/null; echo $?)"
check "missing command"     "127"        "$(/tmp/s/absent 2>/dev/null; echo $?)"
check "chmod adds +x"       "nope"       "$(chmod +x /tmp/s/plain.sh; /tmp/s/plain.sh)"
check "chmod removes x"     "126"        "$(chmod -x /tmp/s/plain.sh; /tmp/s/plain.sh 2>/dev/null; echo $?)"
check "chmod octal"         "755"        "$(chmod 755 /tmp/s/plain.sh; stat /tmp/s/plain.sh | grep Mode | cut -d' ' -f4 | cut -d/ -f1)"
rm -rf /tmp/s

echo
echo "-- system --"
check "uname"             "Linux"    "$(uname)"
# Two places in the kernel say what machine this is, and they have to agree.
# /proc/cpuinfo is written per architecture, so which fields it has says which
# one is running without this script having to be told.
if grep -q "^CPU implementer" /proc/cpuinfo; then machine=aarch64; else machine=x86_64; fi
check "uname -m"          "$machine" "$(uname -m)"
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
