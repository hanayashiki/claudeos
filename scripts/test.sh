#!/bin/bash
# Boot the OS once per suite. Every suite must report zero failures.
#
# ARCH picks the machine, x86_64 unless told otherwise, and is passed on to
# run.sh and to the interactive driver.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ARCH="${ARCH:-x86_64}"
export ARCH
if [ "$ARCH" = aarch64 ]; then
  IMAGE="$ROOT/build/initramfs-aarch64.cpio"
  BUSYBOX="$ROOT/build/rootfs-aarch64/bin/busybox"
  ALPINE="$ROOT/build/alpine-aarch64.cpio"
else
  IMAGE="$ROOT/build/initramfs.cpio"
  BUSYBOX="$ROOT/build/rootfs/bin/busybox"
  ALPINE="$ROOT/build/alpine.cpio"
fi

# Build before testing, rather than running whatever was last left in build/.
# Without this the suites silently test a stale kernel or userland, which reads
# as a passing run of code that is not the code in the tree. Both builds are
# incremental, so this costs nothing when there is nothing to do. NOBUILD=1
# skips it for the rare case of testing a binary on purpose.
if [ -z "${NOBUILD:-}" ]; then
  "$ROOT/scripts/build.sh" > /dev/null || exit 1
  if [ "$ARCH" = aarch64 ]; then
    "$ROOT/scripts/build-user-aarch64.sh" > /dev/null || exit 1
  else
    "$ROOT/scripts/build-user.sh" > /dev/null || exit 1
  fi
fi

status=0
# Suites that did not run at all. A skip is not a pass, so the summary says so.
skipped=0
# The boot id the kernel printed in each boot below. It is drawn from the pool
# the random number generator was seeded from, and two boots sharing one would
# mean the seed did not vary -- which would make every byte the generator hands
# out the same on both, sequence numbers and all.
boot_ids=""

banner() {
  echo "=============================================================="
  echo "  $1"
  echo "=============================================================="
}

record_boot_id() {
  local id
  id="$(printf '%s\n' "$1" | sed -n 's/.*boot id \([0-9a-f][0-9a-f]*\).*/\1/p' | head -n 1)"
  if [ -n "$id" ]; then boot_ids="$boot_ids $id"; fi
}

# Run a script or program inside the OS and require "N passed, 0 failed".
run_suite() {
  local name="$1" append="$2" timeout="$3" image="${4:-$IMAGE}"
  banner "$name"
  local output
  output="$("$ROOT/scripts/run.sh" --timeout "$timeout" \
      --initrd "$image" --append "$append" 2>&1 | tr -d '\r')"
  echo "$output"
  echo
  record_boot_id "$output"

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
  output="$(python3 "$ROOT/tools/drive.py" --timeout 60 --initramfs "$IMAGE" -- \
      "wait:2.5" "echo live-input-works\n" "wait:0.6" \
      "echo abcXY" "wait:0.4" "\x7f\x7f" "wait:0.4" "Z\n" "wait:0.6" \
      "echo throwaway" "wait:0.4" "\x15" "wait:0.4" "echo line-kill-works\n" "wait:0.6" \
      "yes > /dev/null\n" "wait:1.8" "\x03" "wait:1" \
      "echo survived-interrupt\n" "wait:0.6" \
      "sleep 1 &\n" "wait:2.5" \
      "yes > /dev/null\n" "wait:1.5" "\x1a" "wait:1" \
      "jobs\n" "wait:0.8" "bg\n" "wait:1" "jobs\n" "wait:0.8" \
      "kill %1\n" "wait:1.5" \
      "cat &\n" "wait:1.5" "jobs\n" "wait:0.8" \
      "fg\n" "wait:0.8" "into-cat\n" "wait:1" "\x04" "wait:1.2" \
      "uptime\n" "wait:1.5" \
      "exit\n" "wait:3" 2>&1 | tr -d '\r')"
  echo "$output"
  echo
  record_boot_id "$output"

  local ok=1
  # "^abcZ$" and "^line-kill-works$" only appear if the line discipline erased
  # characters instead of passing them straight through.
  # Ctrl-Z has to stop the foreground job, `bg` has to restart it in the
  # background, and `kill %1` has to reach it by job number.
  for expected in "live-input-works" "^abcZ$" "^line-kill-works$" \
                  "survived-interrupt" "Stopped  yes" "Running  yes" \
                  "Stopped  cat" "^into-cat$" \
                  "session ended"; do
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

# The interrupt key, pressed at a terminal rather than written to a socket.
# A socket hands the guest every byte as it stands, so it cannot tell whether
# the terminal in front of QEMU would have kept Ctrl-C for the host; only this
# form goes through the path a person's keyboard takes.
run_interrupt_key() {
  banner "interrupt key at a terminal"
  local output
  output="$(python3 "$ROOT/tools/drive.py" --tty --timeout 60 --initramfs "$IMAGE" -- \
      "until:claudeos shell" "wait:1" \
      "cat\n" "wait:1" "into-cat\n" "wait:1" "\x03" "wait:1.5" \
      "echo prompt-came-back\n" "wait:1.5" \
      "while true; do sleep 1; done\n" "wait:2.5" "\x03" "wait:2" \
      "echo loop-came-back\n" "wait:1.5" \
      "exit\n" "wait:4" 2>&1 | tr -d '\r')"
  echo "$output"
  echo
  record_boot_id "$output"

  local ok=1
  # "^C" is what the line discipline echoes for the interrupt character, so it
  # is there only if the key reached the guest at all. "prompt-came-back" on a
  # line of its own is the shell running a command afterwards: the same text
  # echoed by a cat that was never interrupted keeps "echo " in front of it.
  # "loop-came-back" says the key ended the whole loop rather than the `sleep`
  # the loop happened to be in, which would start the next iteration instead.
  for expected in "^into-cat$" "\^C" "^prompt-came-back$" "^loop-came-back$" \
                  "session ended"; do
    if ! echo "$output" | grep -q "$expected"; then
      echo "   missing expected output: $expected"
      ok=0
    fi
  done

  if [ $ok -eq 1 ]; then
    echo ">> interrupt key at a terminal: OK"
  else
    echo ">> interrupt key at a terminal: FAILED"
    status=1
  fi
  echo
}

# The boots above, compared against each other. Every one seeds its generator
# from what it can observe of its own start-up, and the id says where that left
# it; two the same would mean two machines produced one stream, which is the
# failure that would make the rest of the generator's work pointless. This
# costs no boot of its own: it reads what the sections above already printed.
run_boot_ids() {
  banner "two boots, two streams"
  local count distinct
  count=$(printf '%s\n' $boot_ids | grep -c .)
  distinct=$(printf '%s\n' $boot_ids | grep . | sort -u | wc -l | tr -d ' ')
  printf '%s\n' $boot_ids | grep . | sed 's/^/   /'
  echo
  if [ "$count" -lt 2 ]; then
    echo "   only $count boot reported an id; there is nothing to compare"
    echo ">> two boots, two streams: FAILED"
    status=1
  elif [ "$count" != "$distinct" ]; then
    echo "   $count boots, $distinct distinct ids: two boots produced one stream"
    echo ">> two boots, two streams: FAILED"
    status=1
  else
    echo "   $count boots, $count distinct ids"
    echo ">> two boots, two streams: OK"
  fi
  echo
}

run_suite "userland and shell" "/root/suite.sh" 240
run_suite "rust standard library" "init=/bin/rtest" 300
# The protocols against a card that only records what it is asked to send:
# frames in by hand, frames out compared byte for byte.
run_suite "network protocols" "net=test" 60
# The socket system calls, through the standard library, over the loopback
# address, so no card has to be there.
run_suite "internet sockets" "init=/bin/inet" 120
# The two suites below run software this project did not build. Both images
# are fetched for the machine ARCH names, so both run on either one.
if [ -x "$BUSYBOX" ]; then
  run_suite "upstream busybox" "/root/busybox.sh" 300
else
  echo ">> upstream busybox: skipped (run ARCH=$ARCH scripts/fetch-busybox.sh)"
  skipped=$((skipped + 1))
  echo
fi
if [ -f "$ALPINE" ]; then
  run_suite "alpine linux userland" "init=/bin/sh /root/alpine.sh" 300 "$ALPINE"
else
  echo ">> alpine linux userland: skipped (run ARCH=$ARCH scripts/fetch-alpine.sh)"
  skipped=$((skipped + 1))
  echo
fi
run_interactive
run_interrupt_key
run_boot_ids

if [ $status -ne 0 ]; then
  echo "some suites failed"
elif [ $skipped -gt 0 ]; then
  # Saying "all passed" here would be untrue: a skipped suite tested nothing.
  echo "the suites that ran passed, but $skipped did not run"
else
  echo "all suites passed"
fi
exit $status
