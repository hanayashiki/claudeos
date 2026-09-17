#!/bin/sh
# A tour of what the shell and kernel can do.
echo "--- processes ---"
ps
echo
echo "--- pipes and redirection ---"
seq 1 20 | grep -c ""
seq 1 5 | tr '0-9' 'a-j'
echo "written by a pipeline" > /tmp/pipe.txt
cat /tmp/pipe.txt
echo
echo "--- files ---"
mkdir -p /tmp/demo/inner
echo alpha > /tmp/demo/a.txt
echo beta  > /tmp/demo/inner/b.txt
find /tmp/demo
wc -l /tmp/demo/a.txt
echo
echo "--- globbing and exit status ---"
ls /tmp/demo/*.txt
test -d /tmp/demo && echo "directory exists"
test -f /nope || echo "missing file reports failure"
echo
echo "--- a C program built against musl ---"
hello_c 60
echo
echo "--- system ---"
uname -a
free
uptime
