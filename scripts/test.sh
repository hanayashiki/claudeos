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
# process is blocked in a read. It boots without shell_exit=poweroff, as a
# board does, so `exit` has to start a new shell rather than end the session,
# and `poweroff` is what ends it.
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
      "exit\n" "wait:2" "echo new-shell-works\n" "wait:0.8" \
      "poweroff\n" "wait:3" 2>&1 | tr -d '\r')"
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
                  "init: shell exited; starting a new one" "^new-shell-works$" \
                  "powering off"; do
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
  # shell_exit=poweroff, so the `exit` at the end ends the session, which is
  # also what checks that word still does.
  output="$(python3 "$ROOT/tools/drive.py" --tty --timeout 60 --initramfs "$IMAGE" \
      --append shell_exit=poweroff -- \
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

# The telnet console: the same terminal over TCP port 23, driven with
# scripts/console.py through a port on this machine that QEMU forwards to the
# guest's port 23. One boot with it on, one with telnet=off. QEMU's raspi4b
# emulates no network card, so on aarch64 the console never starts and there
# is nothing here to drive.
run_telnet() {
  banner "telnet console"
  if [ "$ARCH" != x86_64 ]; then
    echo "   QEMU's raspi4b emulates no network card, so the telnet console never"
    echo "   starts on this machine; the sections above ran without it."
    echo ">> telnet console: not run on $ARCH"
    echo
    return
  fi
  local dir port serial qemu first ok=1 attached
  dir="$(mktemp -d)"
  serial="$dir/serial"
  port="$(telnet_free_port)"
  local console=(python3 "$ROOT/scripts/console.py" 127.0.0.1 --port "$port" --timeout 20)

  # The forwarded port listens on this machine's loopback address only: what
  # is behind it is a root shell with no password.
  "$ROOT/scripts/run.sh" --timeout 150 --hostfwd "tcp:127.0.0.1:$port-:23" \
      --initrd "$IMAGE" --append shell_exit=poweroff > "$serial" 2>&1 < /dev/null &
  qemu=$!
  if ! telnet_wait "$serial" "telnet: the console is on"; then
    echo "   the kernel never said it was listening"
    ok=0
  fi

  # Connected after boot: the kernel log first, then a command, then a line
  # edited with backspace, which reads "abcZ" only if the two backspaces
  # erased "XY".
  telnet_client after-boot 0 --log --send 'echo telnet-works' --until '^telnet-works$' \
      --send 'uname -a' --until '^Linux claudeos' \
      --send 'echo abcXY\x7f\x7fZ' --until '^abcZ$' || ok=0
  telnet_expect after-boot "claudeos: booting" "^dhcp: 10\.0\.2\.15" \
      "^telnet: console attached from" "^telnet-works$" "^Linux claudeos" "^abcZ$" || ok=0

  # The interrupt key from the connection. The session is over well inside
  # the hundred seconds the sleep asks for, so the prompt coming back means
  # the sleep was interrupted rather than finished.
  telnet_client interrupt 0 --send 'sleep 100' --wait 1.5 --type '\x03' \
      --send 'echo sleep-interrupted' --until '^sleep-interrupted$' || ok=0
  telnet_expect interrupt "\^C" "^sleep-interrupted$" || ok=0

  # A second connection while the first is up is told the console is busy,
  # and the first carries on. The second is started once the serial port has
  # said the first attached, so it cannot be the one that gets in.
  attached=$(grep -c "telnet: console attached from" "$serial")
  "${console[@]}" --wait 4 --send 'echo first-still-up' --until '^first-still-up$' \
      > "$dir/first" 2>&1 &
  first=$!
  telnet_wait_count "$serial" "telnet: console attached from" $((attached + 1))
  telnet_client second 3 --send 'echo second-got-in' --until '^second-got-in$' || ok=0
  telnet_expect second "telnet console busy: in use from" || ok=0
  if ! wait $first; then
    echo "   the first connection did not carry on after the second was turned away"
    ok=0
  fi
  echo "--- first"
  tr -d '\r' < "$dir/first"
  telnet_expect first "^first-still-up$" || ok=0

  # Gone and back: a new connection after the last one closed.
  telnet_client reconnect 0 --send 'echo reconnected' --until '^reconnected$' || ok=0
  telnet_expect reconnect "^reconnected$" || ok=0

  # A client that stops reading while the board prints a great deal. Its
  # socket buffer is made small so the output backs up into the guest rather
  # than into this machine. It has to be dropped, the kernel has to say so,
  # the printing has to carry on to the serial port, and the next connection
  # has to work.
  telnet_client stalled 5 --receive-buffer 4096 --send 'seq 1 200000' --stall 10 --wait 5 || ok=0
  if ! telnet_wait "$serial" "^200000"; then
    echo "   the output stopped on the serial port as well"
    ok=0
  fi
  if ! grep -q "telnet: dropped 10.0.2.2:[0-9]*, which fell more than" "$serial"; then
    echo "   the kernel did not say it dropped the client that stopped reading"
    ok=0
  fi
  telnet_client after-drop 0 --send 'echo after-drop' --until '^after-drop$' || ok=0
  telnet_expect after-drop "^after-drop$" || ok=0

  telnet_client leave 5 --send 'exit' --wait 5 > /dev/null
  if ! telnet_wait "$serial" "powering off"; then
    pkill -f "hostfwd=tcp:127.0.0.1:$port-:23"
  fi
  wait $qemu
  echo "--- what the kernel said on the serial port"
  grep "telnet:" "$serial" | tr -d '\r'
  record_boot_id "$(cat "$serial")"

  # telnet=off: the network comes up and nothing answers on port 23. Through
  # QEMU's forwarding that is a connection closed before anything arrives.
  serial="$dir/serial-off"
  port="$(telnet_free_port)"
  console=(python3 "$ROOT/scripts/console.py" 127.0.0.1 --port "$port" --timeout 20)
  "$ROOT/scripts/run.sh" --timeout 60 --hostfwd "tcp:127.0.0.1:$port-:23" \
      --initrd "$IMAGE" --append 'telnet=off' > "$serial" 2>&1 < /dev/null &
  qemu=$!
  telnet_wait "$serial" "claudeos shell"
  telnet_client off 4 --send 'echo telnet-is-on' --until '^telnet-is-on$' || ok=0
  pkill -f "hostfwd=tcp:127.0.0.1:$port-:23"
  wait $qemu
  echo "--- what the kernel said on the serial port"
  grep "telnet\|dhcp:" "$serial" | tr -d '\r'
  if ! grep -q "^telnet: off, from the command line" "$serial" \
      || ! grep -q "^dhcp: 10\.0\.2\.15" "$serial" \
      || grep -q "telnet: the console is on" "$serial"; then
    echo "   with telnet=off the network has to come up and the console must not listen"
    ok=0
  fi
  record_boot_id "$(cat "$serial")"
  rm -rf "$dir"

  if [ $ok -eq 1 ]; then
    echo ">> telnet console: OK"
  else
    echo ">> telnet console: FAILED"
    status=1
  fi
  echo
}

telnet_free_port() {
  python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

# Wait up to a minute for a line matching $2 to appear in the file $1.
telnet_wait() {
  local i
  for i in $(seq 1 120); do
    if tr -d '\r' < "$1" | grep -q "$2"; then return 0; fi
    sleep 0.5
  done
  return 1
}

# Wait up to twenty seconds for $3 lines matching $2 in the file $1.
telnet_wait_count() {
  local i
  for i in $(seq 1 40); do
    if [ "$(grep -c "$2" "$1")" -ge "$3" ]; then return 0; fi
    sleep 0.5
  done
  return 1
}

# Run the client with the arguments after the first two, keep what it
# printed under the name $1, print the end of it, and require exit status $2.
telnet_client() {
  local name="$1" expected="$2" code
  shift 2
  "${console[@]}" "$@" > "$dir/$name" 2>&1
  code=$?
  echo "--- $name (exit status $code)"
  tr -d '\r' < "$dir/$name" | tail -n 40
  if [ "$code" != "$expected" ]; then
    echo "   expected exit status $expected"
    return 1
  fi
}

# Require every pattern after the first argument in what client $1 printed.
telnet_expect() {
  local name="$1" pattern missing=0
  shift
  for pattern in "$@"; do
    if ! tr -d '\r' < "$dir/$name" | grep -q "$pattern"; then
      echo "   $name: missing expected output: $pattern"
      missing=1
    fi
  done
  return $missing
}

# Network time: init starts BusyBox ntpd when the image has /etc/ntp.conf, ntpd
# asks the servers the board image names, and the kernel steps the clock. The
# image is the one the suites above booted, with that file added and every date
# in it put an hour back, and the emulated battery clock reads 2000, so the
# kernel starts from a floor an hour behind, as a board does, and the step is
# large enough to see. A `sleep 20` spans the step and is timed on this
# machine's clock.
#
# It needs the internet. When this machine cannot get an answer from either
# server itself, the section says it did not run, which counts as a skip and
# not as a pass.
run_ntp() {
  banner "network time"
  if [ "$ARCH" != x86_64 ]; then
    echo "   QEMU's raspi4b emulates no network card, so ntpd has no server to"
    echo "   reach on this machine."
    echo ">> network time: not run on $ARCH"
    echo
    return
  fi
  # init starts the time keeper only when there is a busybox to run.
  if [ ! -x "$BUSYBOX" ]; then
    echo ">> network time: skipped (run ARCH=$ARCH scripts/fetch-busybox.sh)"
    skipped=$((skipped + 1))
    echo
    return
  fi
  if ! ntp_reachable; then
    echo "   neither ntp.nict.jp nor time.cloudflare.com answered this machine"
    echo ">> network time: not run: no internet"
    skipped=$((skipped + 1))
    echo
    return
  fi
  local dir floor ok=1
  dir="$(mktemp -d)"
  cp -R "$ROOT/build/rootfs" "$dir/rootfs"
  # The same two lines as the NTP_CONF=1 block in scripts/build-user.sh.
  printf 'server ntp.nict.jp\nserver time.cloudflare.com\n' > "$dir/rootfs/etc/ntp.conf"
  cat > "$dir/rootfs/root/ntp.sh" <<'SCRIPT'
#!/bin/sh
echo "ntp-check: booted at $(date +%s)"
echo "ntp-check: sleep-start"
sleep 20
echo "ntp-check: sleep-end"
i=0
until grep -q "setting time to" /var/log/ntpd.log 2>/dev/null || [ $i -ge 40 ]; do
  sleep 1
  i=$((i + 1))
done
echo "ntp-check: now $(date +%s)"
echo "--- /var/log/ntpd.log"
cat /var/log/ntpd.log
SCRIPT
  chmod +x "$dir/rootfs/root/ntp.sh"
  floor="$(python3 - "$dir/rootfs" <<'PY'
import os, sys, time
stamp = int(time.time()) - 3600
for top, dirs, files in os.walk(sys.argv[1]):
    for name in dirs + files:
        os.utime(os.path.join(top, name), (stamp, stamp), follow_symlinks=False)
os.utime(sys.argv[1], (stamp, stamp))
print(stamp)
PY
)"
  python3 "$ROOT/tools/mkcpio.py" "$dir/rootfs" "$dir/ntp.cpio" > /dev/null

  # Every console line, prefixed with the time this machine received it.
  "$ROOT/scripts/run.sh" --timeout 120 --net --initrd "$dir/ntp.cpio" \
      --append /root/ntp.sh -rtc base=2000-01-01T00:00:00 < /dev/null 2>&1 |
    python3 -u -c '
import sys, time
for line in iter(sys.stdin.buffer.readline, b""):
    text = line.decode("utf-8", "replace").rstrip("\r\n")
    print("%.3f %s" % (time.time(), text), flush=True)' > "$dir/serial"
  cat "$dir/serial"
  echo
  record_boot_id "$(cat "$dir/serial")"

  python3 - "$dir/serial" "$floor" <<'PY' || ok=0
import sys
floor = int(sys.argv[2])
keys = ["ntp-check: booted at", "ntp-check: sleep-start", "clock: set to",
        "ntp-check: sleep-end", "ntp-check: now", "setting time to"]
seen = {}
for raw in open(sys.argv[1], errors="replace"):
    stamp, _, text = raw.rstrip("\n").partition(" ")
    for key in keys:
        if key in text and key not in seen:
            seen[key] = (float(stamp), text)
missing = [key for key in keys if key not in seen]
if missing:
    print("   missing expected output: " + "; ".join(missing))
    sys.exit(1)
ok = True
booted = int(seen["ntp-check: booted at"][1].split()[-1])
print("   booted at %d, floor %d" % (booted, floor))
if not floor <= booted < floor + 120:
    print("   the clock did not start from the floor")
    ok = False
start = seen["ntp-check: sleep-start"][0]
step = seen["clock: set to"][0]
end = seen["ntp-check: sleep-end"][0]
print("   sleep 20 took %.2f s here; the step came %.2f s into it" % (end - start, step - start))
if not start < step < end:
    print("   the clock was not stepped while the sleep ran")
    ok = False
if not 19.5 <= end - start <= 21.5:
    print("   the sleep did not last 20 s by this machine's clock")
    ok = False
host, text = seen["ntp-check: now"]
guest = int(text.split()[-1])
print("   afterwards the guest said %d and this machine's clock read %.3f" % (guest, host))
if abs(guest - host) > 2:
    print("   the guest's clock is more than 2 s from this machine's")
    ok = False
sys.exit(0 if ok else 1)
PY
  rm -rf "$dir"

  if [ $ok -eq 1 ]; then
    echo ">> network time: OK"
  else
    echo ">> network time: FAILED"
    status=1
  fi
  echo
}

# Whether this machine gets an SNTP answer from either server the board image
# names, within three seconds each.
ntp_reachable() {
  python3 - <<'PY'
import socket, sys
for host in ("ntp.nict.jp", "time.cloudflare.com"):
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.settimeout(3)
            s.sendto(b"\x1b" + 47 * b"\0", (host, 123))
            if len(s.recv(48)) >= 48:
                sys.exit(0)
    except OSError:
        pass
sys.exit(1)
PY
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
run_telnet
run_ntp
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
