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
  TREE="$ROOT/build/rootfs-aarch64"
  BUSYBOX="$TREE/bin/busybox"
  ALPINE="$ROOT/build/alpine-aarch64.cpio"
  BOARD_IMAGE="$ROOT/build/initramfs-aarch64-board.cpio"
  KERNEL_ELF="$ROOT/build/kernel-aarch64.elf"
  KERNEL_IMAGE="$ROOT/build/kernel8.img"
else
  IMAGE="$ROOT/build/initramfs.cpio"
  TREE="$ROOT/build/rootfs"
  BUSYBOX="$TREE/bin/busybox"
  ALPINE="$ROOT/build/alpine.cpio"
  KERNEL_ELF="$ROOT/build/kernel.elf"
  KERNEL_IMAGE="$ROOT/build/kernel.elf"
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

# The integrity check the kernel runs at boot, against the test image as built
# and against copies of it, or of the kernel, with one thing wrong in each.
# Every boot has to reach the shell whatever the check found, and
# /proc/claudeos/integrity has to say what the console said.
run_integrity() {
  banner "integrity check at boot"
  local dir ok=1 manifest="$TREE/etc/claudeos/checksums" name victim cbox_line cbox_digest extra
  local every_item=() count
  dir="$(mktemp -d)"
  if [ ! -f "$manifest" ]; then
    echo "   $manifest is missing, so the image was built without a manifest"
    echo ">> integrity check at boot: FAILED"
    status=1
    rm -rf "$dir"
    echo
    return
  fi
  echo "--- the manifest in the test image"
  cat "$manifest"

  # As built: a line saying ok for every item in the manifest.
  integrity_boot matching "$IMAGE" "$KERNEL_IMAGE"
  while read -r _ name; do every_item+=("^integrity: $name ok$"); done < "$manifest"
  count="$(grep -c . "$manifest")"
  integrity_expect matching "${every_item[@]}" \
      "^integrity: $count ok, 0 damaged, 0 missing, 0 malformed lines skipped" || ok=0

  # One byte of /bin/cbox changed: the last byte, which in a stripped binary
  # is in the section header table. The program loader does not read that
  # table, so the changed cbox still runs as init and as the shell.
  integrity_image cbox rewrite-last-byte bin/cbox
  integrity_boot cbox "$dir/cbox.cpio" "$KERNEL_IMAGE"
  integrity_expect cbox \
      "^integrity: /bin/cbox DAMAGED: expected $(integrity_digest /bin/cbox), got [0-9a-f]\{40\}$" \
      "^integrity: kernel ok$" \
      "^integrity: $((count - 1)) ok, 1 damaged, 0 missing, 0 malformed lines skipped" || ok=0

  # One byte of the kernel's code changed, past the end of its last function,
  # in a copy of the image QEMU boots.
  python3 "$ROOT/tools/checksums.py" flip-code-byte "$KERNEL_ELF" "$KERNEL_IMAGE" "$dir/kernel.img" || ok=0
  integrity_boot kernel "$IMAGE" "$dir/kernel.img"
  integrity_expect kernel \
      "^integrity: kernel DAMAGED: expected $(integrity_digest kernel), got [0-9a-f]\{40\}$" \
      "^integrity: /bin/cbox ok$" \
      "^integrity: $((count - 1)) ok, 1 damaged, 0 missing, 0 malformed lines skipped" || ok=0

  # An item the manifest names taken out of the image. cbox is init and the
  # shell, so the item is the first one after it.
  victim="$(awk '$2 != "kernel" && $2 != "/bin/cbox" { print $2; exit }' "$manifest")"
  if [ -z "$victim" ]; then
    echo "   the manifest names nothing besides the kernel and /bin/cbox to take out"
    ok=0
  else
    integrity_image missing remove "${victim#/}"
    integrity_boot missing "$dir/missing.cpio" "$KERNEL_IMAGE"
    integrity_expect missing "^integrity: $victim missing$" "^integrity: /bin/cbox ok$" \
        "^integrity: $((count - 1)) ok, 0 damaged, 1 missing, 0 malformed lines skipped" || ok=0
  fi

  # A manifest that is mostly not one: a digest with a letter that is not hex,
  # a line past the length limit, a line with no digest, a tab where the two
  # spaces go, a name with an escape character, a relative name, bytes that
  # are not text, and five more short lines so that there are more malformed
  # lines than get a line each. The good line at the end is still checked.
  cbox_line="$(grep '  /bin/cbox$' "$manifest")"
  cbox_digest="${cbox_line%%  *}"
  extra="$(printf '%0600d' 0)"
  {
    printf 'g%s\n' "${cbox_line#?}"
    printf '%s  /bin/%s\n' "$cbox_digest" "$extra"
    printf 'not a manifest line at all\n'
    printf '%s\t/bin/cbox\n' "$cbox_digest"
    printf '%s  /bin/\033[2Jcbox\n' "$cbox_digest"
    printf '%s  bin/cbox\n' "$cbox_digest"
    printf '\377\376\375\374\n'
    for name in 1 2 3 4 5; do printf 'short\n'; done
    printf '%s\n' "$cbox_line"
  } > "$dir/garbled-manifest"
  integrity_image garbled replace-manifest "$dir/garbled-manifest"
  integrity_boot garbled "$dir/garbled.cpio" "$KERNEL_IMAGE"
  integrity_expect garbled \
      "^integrity: line 1 of /etc/claudeos/checksums skipped: the digest holds a character that is not a hex digit$" \
      "^integrity: line 2 of /etc/claudeos/checksums skipped: it is 647 bytes long, and a line may be 512$" \
      "^integrity: line 3 of /etc/claudeos/checksums skipped: too short for a digest, two spaces and a name$" \
      "^integrity: line 4 of /etc/claudeos/checksums skipped: the 40 hex digits of the digest are not followed by two spaces$" \
      "^integrity: line 5 of /etc/claudeos/checksums skipped: the name holds a control character$" \
      "^integrity: line 6 of /etc/claudeos/checksums skipped: the name is neither \`kernel\` nor an absolute path$" \
      "^integrity: line 7 of /etc/claudeos/checksums skipped: too short for a digest, two spaces and a name$" \
      "^integrity: line 8 of /etc/claudeos/checksums skipped: too short" \
      "^integrity: 4 more malformed lines skipped without a line each$" \
      "^integrity: /bin/cbox ok$" \
      "^integrity: 1 ok, 0 damaged, 0 missing, 12 malformed lines skipped" || ok=0

  # No manifest at all, as in the Alpine image.
  integrity_image none remove etc/claudeos
  integrity_boot none "$dir/none.cpio" "$KERNEL_IMAGE"
  integrity_expect none \
      "^integrity: no /etc/claudeos/checksums in the image, so there is nothing to check$" || ok=0

  echo "--- how long the check took on the image as built"
  grep "^integrity: .* ok, .* in [0-9.]* ms$" "$dir/matching" | head -n 1
  rm -rf "$dir"

  if [ $ok -eq 1 ]; then
    echo ">> integrity check at boot: OK"
  else
    echo ">> integrity check at boot: FAILED"
    status=1
  fi
  echo
}

# The digest the test image's manifest gives the item $1.
integrity_digest() {
  awk -v name="$1" '$2 == name { print $1 }' "$TREE/etc/claudeos/checksums"
}

# Make $dir/$1.cpio from a copy of the test image's tree, changed by $2:
# `rewrite-last-byte PATH`, `remove PATH` or `replace-manifest FILE`.
integrity_image() {
  local name="$1" change="$2" what="$3" copy="$dir/$1-tree"
  cp -Rp "$TREE" "$copy"
  case "$change" in
    rewrite-last-byte)
      python3 -c 'import sys
path = sys.argv[1]
data = bytearray(open(path, "rb").read())
data[-1] ^= 0xFF
open(path, "wb").write(data)' "$copy/$what" ;;
    remove) rm -r "${copy:?}/$what" ;;
    replace-manifest) cp "$what" "$copy/etc/claudeos/checksums" ;;
  esac
  python3 "$ROOT/tools/mkcpio.py" "$copy" "$dir/$name.cpio" > /dev/null
  rm -rf "$copy"
}

# Boot the image $2 with the kernel image $3, wait for the shell, read the
# status file back and power off. What the console showed is kept as $dir/$1,
# and the lines that matter here are printed.
integrity_boot() {
  local name="$1"
  python3 "$ROOT/tools/drive.py" --timeout 90 --initramfs "$2" --kernel "$3" -- \
      "until:claudeos shell" "wait:0.5" \
      "cat /proc/claudeos/integrity\n" "wait:1.5" \
      "poweroff\n" "wait:3" 2>&1 | tr -d '\r' > "$dir/$name"
  echo "--- $name"
  grep -E "^integrity:|claudeos shell|KERNEL PANIC|powering off" "$dir/$name"
  record_boot_id "$(cat "$dir/$name")"
  rm -f "$dir/$name.cpio"
}

# Require every pattern after the first argument twice in what boot $1 showed,
# once printed at boot and once read back from /proc/claudeos/integrity, and
# require the boot to have reached the shell without a panic.
integrity_expect() {
  local name="$1" pattern missing=0
  shift
  for pattern in "$@"; do
    if [ "$(grep -c -- "$pattern" "$dir/$name")" -lt 2 ]; then
      echo "   $name: expected at boot and in /proc/claudeos/integrity: $pattern"
      missing=1
    fi
  done
  if ! grep -q "claudeos shell" "$dir/$name"; then
    echo "   $name: the boot did not reach the shell"
    missing=1
  fi
  if grep -q "KERNEL PANIC" "$dir/$name"; then
    echo "   $name: the kernel panicked"
    missing=1
  fi
  if [ $missing -ne 0 ]; then
    echo "   --- everything boot $name showed"
    sed 's/^/   /' "$dir/$name"
  fi
  return $missing
}

# The image scripts/mkcard.sh puts on the card, booted under QEMU. It has to
# reach the shell and pass its own check, its /root and /bin have to hold none
# of the test image's own files, and its cbox has to have no rtest applet.
run_board_image() {
  banner "board image"
  if [ "$ARCH" != aarch64 ]; then
    echo "   the board image is built for the Raspberry Pi 4 only, so $ARCH has none"
    echo ">> board image: not run on $ARCH"
    echo
    return
  fi
  local output ok=1 unwanted
  output="$(python3 "$ROOT/tools/drive.py" --timeout 90 --initramfs "$BOARD_IMAGE" -- \
      "until:claudeos shell" "wait:0.5" \
      "find /root /bin\n" "wait:1.5" \
      "cbox rtest\n" "wait:1" \
      "cat /proc/claudeos/integrity\n" "wait:1" \
      "poweroff\n" "wait:3" 2>&1 | tr -d '\r')"
  echo "$output"
  echo
  record_boot_id "$output"

  for expected in "claudeos shell" "^/root$" "^/bin/cbox$" "^/bin/sh$" \
                  "cbox: rtest: unknown applet" \
                  "^integrity: [1-9][0-9]* ok, 0 damaged, 0 missing, 0 malformed lines skipped" \
                  "powering off"; do
    if ! echo "$output" | grep -q "$expected"; then
      echo "   missing expected output: $expected"
      ok=0
    fi
  done
  # What the test image has that this one must not: anything at all in /root,
  # and the test programs and the rtest link in /bin.
  unwanted="$(echo "$output" | grep -E "^/root/.|^/bin/(hello_c|inet|rtest|go_main)$")"
  if [ -n "$unwanted" ]; then
    echo "   the board image holds test files:"
    echo "$unwanted" | sed 's/^/     /'
    ok=0
  fi
  if echo "$output" | grep -q "KERNEL PANIC"; then
    echo "   the kernel panicked"
    ok=0
  fi

  if [ $ok -eq 1 ]; then
    echo ">> board image: OK"
  else
    echo ">> board image: FAILED"
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
run_integrity
run_board_image
run_interactive
run_interrupt_key
run_telnet
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
