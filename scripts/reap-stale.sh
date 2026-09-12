#!/bin/bash
# Kill claudeos QEMU instances that have outlived any plausible run. A guest
# left spinning in a busy loop burns a core and starves every later run, and
# it survives its driver being killed.
LIMIT_MINUTES="${1:-15}"
ps -eo pid,etime,command 2>/dev/null | grep '[q]emu-system-x86_64' | grep 'claudeos' |
while read -r pid etime rest; do
  minutes=$(echo "$etime" | awk -F: '{ if (NF == 3) print $1 * 60 + $2; else print $1 }')
  if [ "${minutes:-0}" -ge "$LIMIT_MINUTES" ]; then
    echo "[reap] killing stale qemu pid $pid (running $etime)" >&2
    kill -9 "$pid" 2>/dev/null
  fi
done
