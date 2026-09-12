#!/bin/bash
# Boot the OS once per suite. Every suite must report zero failures.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

status=0

banner() {
  echo "=============================================================="
  echo "  $1"
  echo "=============================================================="
}

# Run a script or program inside the OS and require "N passed, 0 failed".
run_suite() {
  local name="$1" append="$2" timeout="$3" image="${4:-$ROOT/build/initramfs.cpio}"
  banner "$name"
  local output
  output="$("$ROOT/scripts/run.sh" --timeout "$timeout" \
      --initrd "$image" --append "$append" 2>&1 | tr -d '\r')"
  echo "$output"
  echo

  if echo "$output" | grep -qE "^=== [0-9]+ passed, 0 failed ===$"; then
    echo ">> $name: OK"
  else
    echo ">> $name: FAILED"
    status=1
  fi
  echo
}

# An interactive session: typing after boot, Ctrl-C on a running job, and a
# background job. These only work if interrupts reach the kernel while a
# process is blocked in a read.
run_interactive() {
  banner "interactive session"
  local output
  output="$(python3 "$ROOT/tools/drive.py" --timeout 60 -- \
      "wait:2.5" "echo live-input-works\n" "wait:0.6" \
      "echo abcXY" "wait:0.4" "\x7f\x7f" "wait:0.4" "Z\n" "wait:0.6" \
      "echo throwaway" "wait:0.4" "\x15" "wait:0.4" "echo line-kill-works\n" "wait:0.6" \
      "yes > /dev/null\n" "wait:1.8" "\x03" "wait:1" \
      "echo survived-interrupt\n" "wait:0.6" \
      "sleep 1 &\n" "wait:2.5" \
      "uptime\n" "wait:1.5" \
      "exit\n" "wait:3" 2>&1 | tr -d '\r')"
  echo "$output"
  echo

  local ok=1
  # "^abcZ$" and "^line-kill-works$" only appear if the line discipline erased
  # characters instead of passing them straight through.
  for expected in "live-input-works" "^abcZ$" "^line-kill-works$" \
                  "survived-interrupt" "session ended"; do
    if ! echo "$output" | grep -q "$expected"; then
      echo "   missing expected output: $expected"
      ok=0
    fi
  done
  # The clock has to advance while the shell is blocked reading. Require the
  # line to be there, so a session that never got that far fails rather than
  # passing by omission.
  if ! echo "$output" | grep -qE "^up [0-9]+ hours"; then
    echo "   uptime never reported; the session did not get that far"
    ok=0
  elif echo "$output" | grep -q "up 0 hours, 0 minutes, 0\.0"; then
    echo "   the clock did not advance during the session"
    ok=0
  fi

  if [ $ok -eq 1 ]; then
    echo ">> interactive session: OK"
  else
    echo ">> interactive session: FAILED"
    status=1
  fi
  echo
}

run_suite "userland and shell" "/root/suite.sh" 240
run_suite "rust standard library" "init=/bin/rtest" 300
if [ -x "$ROOT/build/rootfs/bin/busybox" ]; then
  run_suite "upstream busybox" "/root/busybox.sh" 300
else
  echo ">> upstream busybox: skipped (run scripts/fetch-busybox.sh)"
  echo
fi
if [ -f "$ROOT/build/alpine.cpio" ]; then
  run_suite "alpine linux userland" "init=/bin/sh /root/alpine.sh" 300 \
      "$ROOT/build/alpine.cpio"
else
  echo ">> alpine linux userland: skipped (run scripts/fetch-alpine.sh)"
  echo
fi
run_interactive

if [ $status -eq 0 ]; then
  echo "all suites passed"
else
  echo "some suites failed"
fi
exit $status
