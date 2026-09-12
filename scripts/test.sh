#!/bin/bash
# Boot the OS twice: once for the shell/userland suite and once for the
# standard-library suite. Both must report zero failures.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

status=0

run_suite() {
  local name="$1" append="$2" timeout="$3"
  echo "=============================================================="
  echo "  $name"
  echo "=============================================================="
  local output
  output="$("$ROOT/scripts/run.sh" --timeout "$timeout" \
      --initrd "$ROOT/build/initramfs.cpio" --append "$append" 2>&1 | tr -d '\r')"
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
# background job. These only work if interrupts reach the kernel during a
# blocking read, so they are worth checking separately.
run_interactive() {
  echo "=============================================================="
  echo "  interactive session"
  echo "=============================================================="
  local output
  output="$(python3 "$ROOT/tools/drive.py" --timeout 45 -- \
      "wait:2.5" "echo live-input-works\n" "wait:0.6" \
      "yes > /dev/null\n" "wait:1.8" "\x03" "wait:1" \
      "echo survived-interrupt\n" "wait:0.6" \
      "sleep 1 &\n" "wait:2" \
      "uptime\n" "wait:0.6" \
      "exit\n" "wait:2" 2>&1 | tr -d '\r')"
  echo "$output"
  echo

  local ok=1
  for expected in "live-input-works" "survived-interrupt" "session ended"; do
    if ! echo "$output" | grep -q "$expected"; then
      echo "   missing expected output: $expected"
      ok=0
    fi
  done
  # The clock has to advance while the shell is blocked reading.
  if echo "$output" | grep -q "up 0 hours, 0 minutes, 0\.0"; then
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

run_suite "userland and shell" "/root/suite.sh" 120
run_suite "rust standard library" "init=/bin/rtest" 180
run_interactive

if [ $status -eq 0 ]; then
  echo "all suites passed"
else
  echo "some suites failed"
fi
exit $status
