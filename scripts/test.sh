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

run_suite "userland and shell" "/root/suite.sh" 120
run_suite "rust standard library" "init=/bin/rtest" 180

if [ $status -eq 0 ]; then
  echo "all suites passed"
else
  echo "some suites failed"
fi
exit $status
