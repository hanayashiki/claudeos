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
      "echo タ日本🎉x" "wait:0.4" "\x7f\x7f" "wait:0.4" " | hexdump -C\n" "wait:0.8" \
      "echo raw\xffbyte | hexdump -C\n" "wait:0.8" \
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
  # A line typed with Japanese and an emoji, and two backspaces that take off
  # the x and the whole emoji, reaches the command as its UTF-8 bytes:
  # タ日本 is e3 82 bf e6 97 a5 e6 9c ac.
  if ! echo "$output" | grep -q "^00000000  e3 82 bf e6 97 a5 e6 9c  ac 0a"; then
    echo "   the line typed as タ日本 did not reach the command as its UTF-8 bytes"
    ok=0
  fi
  # A byte that is not UTF-8, 0xff, typed in a line. The editor keeps it; the
  # shell still takes lines as text, so it reaches the command as U+FFFD
  # (ef bf bd), once. Either that or the byte itself is accepted, and which one
  # is said, because the shell taking bytes is what would change it.
  if echo "$output" | grep -q "^00000000  72 61 77 ff 62 79 74 65  0a"; then
    echo "   a typed 0xff byte reached the command unchanged"
  elif echo "$output" | grep -q "^00000000  72 61 77 ef bf bd 62 79  74 65 0a"; then
    echo "   a typed 0xff byte reached the command as one U+FFFD, at the shell's text boundary"
  else
    echo "   a typed 0xff byte reached the command as neither itself nor one U+FFFD"
    ok=0
  fi
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

  # The system services list changed: its last byte, the newline after the
  # ntpd line, made a byte that is not text. The check reports the list, and
  # init's reading of it skips the line the byte is in.
  integrity_image services rewrite-last-byte etc/claudeos/services
  integrity_boot services "$dir/services.cpio" "$KERNEL_IMAGE"
  integrity_expect services \
      "^integrity: /etc/claudeos/services DAMAGED: expected $(integrity_digest /etc/claudeos/services), got [0-9a-f]\{40\}$" \
      "^integrity: /bin/cbox ok$" \
      "^integrity: $((count - 1)) ok, 1 damaged, 0 missing, 0 malformed lines skipped" || ok=0
  if ! grep -q "^services: 0 system started, 1 line skipped; " "$dir/services"; then
    echo "   services: the summary line does not say the damaged line was skipped"
    grep "^services:" "$dir/services" | sed 's/^/   /'
    ok=0
  fi

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
  # The same two lines as /etc/ntp.conf in the board image, which
  # scripts/build-user-aarch64.sh writes.
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
# of the test image's own files, and its cbox has to have no rtest applet. Then
# a card mkcard.sh makes on the Mac is checked and booted (see board_card).
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

  if [ "$(uname -s)" != Darwin ]; then
    echo "   the card check attaches a disk image with hdiutil, which only macOS has"
    skipped=$((skipped + 1))
  else
    board_card || ok=0
  fi

  if [ $ok -eq 1 ]; then
    echo ">> board image: OK"
  else
    echo ">> board image: FAILED"
    status=1
  fi
  echo
}

# A card scripts/mkcard.sh --new writes, to a disk image on the Mac. /data has
# to hold what user/data holds and nothing else, so no file macOS makes on a
# volume it mounts; updating the boot files has to leave /data's blocks as they
# were. The card is then booted with the board image: the web server the seeded
# list names has to serve the seeded page, and the quick tunnel, which has no
# network under QEMU, has to exit and be started again.
board_card() {
  local dir card fatdisk="$ROOT/tools/fatdisk/target/release/fatdisk" names before output expected failed=0
  if ! (cd "$ROOT/tools/fatdisk" && cargo build --release -q); then
    echo "   tools/fatdisk did not build"
    return 1
  fi
  dir="$(mktemp -d)"
  card="$dir/card.img"
  mkfile -n 1g "$card"
  if ! "$ROOT/scripts/mkcard.sh" --new --image "$card" > "$dir/new.log" 2>&1; then
    echo "   mkcard.sh --new --image failed:"
    tail -n 20 "$dir/new.log" | sed 's/^/     /'
    rm -rf "$dir"
    return 1
  fi
  # Both partitions, file by file, against what went onto them.
  board_card_files CLAUDEOS "$ROOT/build/boot" "after --new" || failed=1
  board_card_files CLAUDEDATA "$ROOT/user/data" "after --new" || failed=1
  for file in services.txt site/index.html; do
    if ! "$fatdisk" cat "$card" CLAUDEDATA "/$file" | cmp -s - "$ROOT/user/data/$file"; then
      echo "   /data/$file on the card is not user/data/$file"
      failed=1
    fi
  done

  before="$(data_digest "$card" CLAUDEDATA)"
  if ! "$ROOT/scripts/mkcard.sh" --image "$card" > "$dir/update.log" 2>&1; then
    echo "   mkcard.sh --image failed:"
    tail -n 20 "$dir/update.log" | sed 's/^/     /'
    failed=1
  elif [ "$(data_digest "$card" CLAUDEDATA)" != "$before" ]; then
    echo "   updating the boot files changed /data"
    failed=1
  else
    echo "   updating the boot files left the blocks of /data as they were"
    board_card_files CLAUDEOS "$ROOT/build/boot" "after an update" || failed=1
  fi

  # The wait is typed as one line, and its report is spelled so that the
  # typed line itself does not match it. The substitutions are quoted: the
  # shell splits an unquoted one in an assignment into words, and runs the
  # second word of `exited with status 1` as a command.
  output="$(python3 "$ROOT/tools/drive.py" --timeout 240 --initramfs "$BOARD_IMAGE" --sd "$card" -- \
      "until:claudeos shell" "wait:1" \
      "busybox wget -q -O - http://127.0.0.1:8080/index.html\n" "wait:2" \
      'i=0; until grep -q "^starts: [2-9]" /run/services/tunnel || [ $i -ge 150 ]; do sleep 1; i=$((i + 1)); done; s="$(sed -n "s/^starts: //p" /run/services/tunnel)"; l="$(sed -n "s/^last run: //p" /run/services/tunnel)"; f="$(grep -c "failed to request quick Tunnel" /var/log/tunnel.log)"; echo "tunnel-""check: $s starts; $l; $f failed requests"\n' \
      "until:tunnel-check:" "wait:0.5" \
      "cat /run/services/tunnel; tail -n 20 /var/log/tunnel.log\n" "wait:1.5" \
      "poweroff\n" "wait:3" 2>&1 | tr -d '\r')"
  record_boot_id "$output"
  echo "--- the card, booted"
  echo "$output" | grep -E "^services:|^data:|This page is|^tunnel-check:|KERNEL PANIC" | sed 's/^/   /'
  # The tunnel's run can also read `ended on signal 9`: cloudflared is a Go
  # program, which may call exit_group from a thread other than its first, and
  # the kernel then records SIGKILL for the first thread, whose status wait4
  # reports. Either way the run ended and counts as a failure.
  for expected in "^data: mounted partition 2 of the card, labelled CLAUDEDATA" \
                  "^services: 1 system started; 2 user started$" \
                  "This page is /data/site/index.html on the card" \
                  "^tunnel-check: [2-9] starts; (exited with status [1-9][0-9]*|ended on signal 9) after [0-9]+ s; [1-9][0-9]* failed requests$"; do
    if ! echo "$output" | grep -qE "$expected"; then
      echo "   missing expected output: $expected"
      failed=1
    fi
  done
  if echo "$output" | grep -q "KERNEL PANIC"; then
    echo "   the kernel panicked with the card"
    failed=1
  fi
  if [ $failed -ne 0 ]; then
    echo "   --- everything the boot with the card showed"
    echo "$output" | sed 's/^/   /'
  fi
  rm -rf "$dir"
  return $failed
}

# Every file on the volume labelled $1 of the card image $card, below the
# directory $2, one path per line.
card_files() {
  local label="$1" below="${2:-}" name
  "$fatdisk" ls "$card" "$label" "/$below" | while IFS= read -r name; do
    case "$name" in
      */) card_files "$label" "$below$name" ;;
      *) printf '%s\n' "$below$name" ;;
    esac
  done
}

# Require the volume labelled $1 on $card to hold exactly the files under the
# directory $2, by path, and nothing else; $3 says when.
board_card_files() {
  local expected actual
  expected="$(cd "$2" && find . -type f | sed 's|^\./||' | LC_ALL=C sort)"
  actual="$(card_files "$1" | LC_ALL=C sort)"
  if [ "$actual" = "$expected" ]; then
    echo "   $1 $3 holds exactly the $(echo "$expected" | grep -c .) files under ${2#$ROOT/}"
  else
    echo "   $1 $3 does not hold exactly the files under ${2#$ROOT/}:"
    diff <(echo "$expected") <(echo "$actual") | sed 's/^/     /'
    return 1
  fi
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

# /data on the emulated Pi 4's SD card, with card images made by tools/fatdisk,
# which needs no root. First the kernel's FAT code on the Mac: tools/fatdisk's
# tests against macOS's own FAT tools, and the fuzz seeds it keeps. Then two
# boots on one card go through every operation and read back what the first
# left; the Mac reads the image after the fsync, before the sync, and after
# each boot, runs fsck_msdos -n on it, and requires the boot partition beside
# /data to be unchanged. Then boots with no card, with data=off, with a label
# the card does not have, with a boot partition carrying the /data label, with
# a bad partition table and with volumes damaged on purpose each have to reach
# the shell with one `data:` line before it and no panic, and walk whatever
# /data they have to the end.
run_data() {
  banner "/data on an SD card"
  if [ "$ARCH" != aarch64 ]; then
    echo "   /data is on the Raspberry Pi 4's SD card, and $ARCH has no card slot"
    echo ">> /data on an SD card: not run on $ARCH"
    echo
    return
  fi
  if [ "$(uname -s)" != Darwin ]; then
    echo "   the card images are made by macOS's newfs_msdos through hdiutil and checked by its fsck_msdos"
    echo ">> /data on an SD card: skipped (needs macOS)"
    skipped=$((skipped + 1))
    echo
    return
  fi
  local fatdisk="$ROOT/tools/fatdisk/target/release/fatdisk"
  if ! (cd "$ROOT/tools/fatdisk" && cargo build --release -q); then
    echo "   tools/fatdisk did not build"
    echo ">> /data on an SD card: FAILED"
    status=1
    echo
    return
  fi
  local dir ok=1 card qemu boot_before whole_before start length seed seeds="${DATA_SEEDS:-12}"
  local long="/claudeos-test/An index page with a long name, spaces, and (brackets).html"
  dir="$(mktemp -d)"
  card="$dir/card.img"

  # The kernel's FAT code on the Mac, before any boot.
  if (cd "$ROOT/tools/fatdisk" && cargo test --release -q > "$dir/host-tests" 2>&1); then
    echo "   tools/fatdisk tests: $(grep -E '^test result' "$dir/host-tests" | awk '{ p += $4; f += $6 } END { print p " passed, " f " failed" }')"
  else
    echo "   tools/fatdisk tests failed:"
    tail -n 40 "$dir/host-tests" | sed 's/^/     /'
    ok=0
  fi
  if "$fatdisk" fuzz --seeds "$ROOT/tools/fatdisk/fuzz-seeds.txt" > "$dir/fuzz" 2>&1; then
    echo "   $(tail -n 1 "$dir/fuzz")"
  else
    echo "   the fuzz seeds found a panic:"
    tail -n 20 "$dir/fuzz" | sed 's/^/     /'
    ok=0
  fi

  "$fatdisk" mbr "$card" 256 CLAUDEOS:64+boot CLAUDEDATA:rest
  boot_before="$(data_digest "$card" CLAUDEOS)"

  # The first boot runs in the background, so that the image can be read in
  # the pause the guest makes between its fsync and its sync.
  "$ROOT/scripts/run.sh" --timeout 240 --sd "$card" --initrd "$IMAGE" \
      --append "/root/data.sh datatest=write" > "$dir/write" 2>&1 < /dev/null &
  qemu=$!
  if data_wait "$dir/write" "^data-test: fsync done" 200; then
    "$fatdisk" cat "$card" CLAUDEDATA /claudeos-test/fsync.txt > "$dir/after-fsync" 2>&1
    if tr -d '\r' < "$dir/write" | grep -q "^data-test: syncing"; then
      echo "   the image was read after the guest went on to sync, so it shows nothing about fsync"
      ok=0
    elif [ "$(cat "$dir/after-fsync")" != "reached the card" ]; then
      echo "   after the fsync, before the sync, the image did not hold the file:"
      sed 's/^/     /' "$dir/after-fsync"
      ok=0
    fi
  else
    echo "   the first boot never reached its fsync"
    ok=0
  fi
  wait $qemu
  data_suite write || ok=0
  if [ "$(cat "$dir/after-fsync" 2>/dev/null)" = "reached the card" ]; then
    echo "   read from the image after the fsync and before the sync: reached the card"
  fi

  # What the Mac finds on the card between the boots.
  if [ "$("$fatdisk" cat "$card" CLAUDEDATA "$long" 2>&1)" != "<h1>first</h1>" ]; then
    echo "   the Mac does not find the file with the long name the guest wrote"
    ok=0
  fi
  if [ "$("$fatdisk" cat "$card" CLAUDEDATA /claudeos-test/big.txt | shasum | cut -c 1-40)" \
      != "$(seq 1 200000 | shasum | cut -c 1-40)" ]; then
    echo "   the large file on the image is not the bytes the guest copied in"
    ok=0
  fi
  data_check "$card" after-write || ok=0

  "$ROOT/scripts/run.sh" --timeout 120 --sd "$card" --initrd "$IMAGE" \
      --append "/root/data.sh datatest=verify" > "$dir/verify" 2>&1 < /dev/null
  data_suite verify || ok=0
  data_check "$card" after-verify || ok=0
  if [ "$(data_digest "$card" CLAUDEOS)" != "$boot_before" ]; then
    echo "   partition 1, the boot partition beside /data, changed"
    ok=0
  else
    echo "   partition 1, the boot partition beside /data, is unchanged after both boots"
  fi

  # No card in the slot.
  data_boot nocard "" ""
  data_expect nocard "^data: /data is not mounted: nothing answered in the card slot" || ok=0

  # data=off leaves the card alone.
  whole_before="$(shasum "$card" | cut -c 1-40)"
  data_boot off "$card" "data=off"
  data_expect off "^data: off, from the command line" || ok=0

  # A label no volume on the card has: nothing is mounted and nothing written.
  data_boot label "$card" "data=ELSEWHERE"
  data_expect label "^data: /data is not mounted: no FAT32 volume on the card is labelled ELSEWHERE" || ok=0
  if [ "$(shasum "$card" | cut -c 1-40)" != "$whole_before" ]; then
    echo "   the card changed in a boot with data=off or a label it does not have"
    ok=0
  fi
  rm -f "$card"

  # A boot partition labelled CLAUDEDATA: the label matches, and start4.elf and
  # kernel8.img in its root keep it from being mounted or written.
  "$fatdisk" mbr "$dir/guard.img" 128 CLAUDEDATA:rest+boot
  whole_before="$(shasum "$dir/guard.img" | cut -c 1-40)"
  data_boot guard "$dir/guard.img" ""
  data_expect guard "^data: /data is not mounted: partition 1 is labelled CLAUDEDATA but its root holds start4.elf, so it is a boot partition" || ok=0
  if [ "$(shasum "$dir/guard.img" | cut -c 1-40)" != "$whole_before" ]; then
    echo "   the boot partition labelled CLAUDEDATA changed"
    ok=0
  fi
  rm -f "$dir/guard.img"

  # What the damaged cards start from: a boot partition, and /data holding
  # directories, long names, a file of many clusters, a directory of two
  # clusters and a file of three.
  "$fatdisk" mbr "$dir/template.img" 128 CLAUDEOS:40+boot CLAUDEDATA:rest
  "$fatdisk" fill "$dir/template.img" CLAUDEDATA
  read -r start length < <("$fatdisk" span "$dir/template.img" CLAUDEDATA)

  # A partition table with a boot indicator that is neither 0 nor 0x80.
  cp "$dir/template.img" "$dir/badmbr.img"
  printf '\063' | dd of="$dir/badmbr.img" bs=1 seek=446 conv=notrunc 2>/dev/null
  data_boot badmbr "$dir/badmbr.img" ""
  data_expect badmbr "^data: /data is not mounted: block 0 of the card has the 55 AA signature but no valid partition table" || ok=0
  rm -f "$dir/badmbr.img"

  # /data's boot sector zeroed, and then three with random bytes over its
  # fields.
  cp "$dir/template.img" "$dir/zeroed.img"
  dd if=/dev/zero of="$dir/zeroed.img" bs=512 seek="$start" count=1 conv=notrunc 2>/dev/null
  data_boot zeroed "$dir/zeroed.img" ""
  data_expect zeroed "^data: /data is not mounted: no FAT32 volume on the card is labelled CLAUDEDATA" || ok=0
  rm -f "$dir/zeroed.img"
  for seed in 1 2 3; do
    cp "$dir/template.img" "$dir/boot-$seed.img"
    "$fatdisk" damage "$dir/boot-$seed.img" CLAUDEDATA boot "$seed" 16
    data_boot "boot-$seed" "$dir/boot-$seed.img" ""
    data_expect "boot-$seed" || ok=0
    rm -f "$dir/boot-$seed.img"
  done

  # A directory and a file whose cluster chains loop back to their start.
  cp "$dir/template.img" "$dir/loops.img"
  "$fatdisk" damage "$dir/loops.img" CLAUDEDATA loops 0
  data_boot loops "$dir/loops.img" ""
  data_expect loops "^data: mounted partition 2 of the card, labelled CLAUDEDATA" || ok=0
  rm -f "$dir/loops.img"

  # www/css pointed at the root directory, so the tree under it has no bottom
  # and `find` goes down it until paths reach the length limit.
  cp "$dir/template.img" "$dir/cycle.img"
  "$fatdisk" damage "$dir/cycle.img" CLAUDEDATA cycle 0
  data_boot cycle "$dir/cycle.img" ""
  data_expect cycle "^data: mounted partition 2 of the card, labelled CLAUDEDATA" || ok=0
  rm -f "$dir/cycle.img"

  # Random bytes anywhere in the image: the partition table, both volumes'
  # boot sectors, FATs and directories, and file contents alike. 200000 bytes
  # over 128 MiB is one in about every 670, which changes /data's FAT in
  # every seed and the partition table's entries or /data's boot sector
  # fields in about one seed in five.
  for seed in $(seq 1 "$seeds"); do
    cp "$dir/template.img" "$dir/random-$seed.img"
    "$fatdisk" damage "$dir/random-$seed.img" CLAUDEDATA random "$seed" 200000
    data_boot "random-$seed" "$dir/random-$seed.img" ""
    data_expect "random-$seed" || ok=0
    rm -f "$dir/random-$seed.img"
  done
  rm -rf "$dir"

  if [ $ok -eq 1 ]; then
    echo ">> /data on an SD card: OK"
  else
    echo ">> /data on an SD card: FAILED"
    status=1
  fi
  echo
}

# Wait up to $3 seconds for a line matching $2 in the file $1.
data_wait() {
  local i
  for i in $(seq 1 $(($3 * 2))); do
    if tr -d '\r' < "$1" | grep -q "$2"; then return 0; fi
    sleep 0.5
  done
  return 1
}

# The SHA-1 of the blocks of the volume labelled $2 in the image $1.
data_digest() {
  local start length
  read -r start length < <("$fatdisk" span "$1" "$2")
  python3 -c 'import hashlib, sys
path, start, length = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
digest = hashlib.sha1()
with open(path, "rb") as image:
    image.seek(start * 512)
    left = length * 512
    while left:
        chunk = image.read(min(left, 1 << 20))
        if not chunk:
            break
        digest.update(chunk)
        left -= len(chunk)
print(digest.hexdigest())' "$1" "$start" "$length"
}

# Read every file on /data in the image $1 from the Mac, and require no cluster
# to belong to two files.
data_check() {
  if "$fatdisk" check "$1" CLAUDEDATA > "$dir/check-$2" 2>&1; then
    echo "   fatdisk check $2: $(cat "$dir/check-$2")"
  else
    echo "   fatdisk check $2 failed:"
    sed 's/^/     /' "$dir/check-$2"
    return 1
  fi
}

# Print what the boot $1 showed, and require its suite to have passed without a
# panic.
data_suite() {
  local output
  output="$(tr -d '\r' < "$dir/$1")"
  echo "--- $1"
  echo "$output"
  record_boot_id "$output"
  if ! echo "$output" | grep -qE "^=== [0-9]+ passed, 0 failed ===$"; then
    echo "   $1: the suite did not pass"
    return 1
  fi
  if echo "$output" | grep -q "KERNEL PANIC"; then
    echo "   $1: the kernel panicked"
    return 1
  fi
}

# Boot with the card image $2, or with no card when $2 is empty, and the
# command line $3; at the shell, walk /data with the damaged-card part of
# tests/data.sh, and power off. What the console showed is kept as $dir/$1.
data_boot() {
  local steps=("until:claudeos shell" "wait:0.5"
      "datatest=damaged sh /root/data.sh\n"
      "until:the damaged card was walked to the end" "until:the damaged card was walked to the end"
      "until:the damaged card was walked to the end" "until:the damaged card was walked to the end"
      "wait:0.5" "poweroff\n" "wait:3")
  if [ -n "$2" ]; then
    python3 "$ROOT/tools/drive.py" --timeout 180 --initramfs "$IMAGE" --sd "$2" --append "$3" -- "${steps[@]}"
  else
    python3 "$ROOT/tools/drive.py" --timeout 180 --initramfs "$IMAGE" --append "$3" -- "${steps[@]}"
  fi 2>&1 | tr -d '\r' > "$dir/$1"
}

# Require, in what the boot $1 showed: every pattern after the first argument;
# exactly one `data:` line before the shell started; the shell; the walk over
# /data finished; and no panic.
data_expect() {
  local name="$1" pattern missing=0 lines
  shift
  echo "--- $name"
  grep -E "^data|claudeos shell|KERNEL PANIC|powering off" "$dir/$name" | sed 's/^/   /'
  for pattern in "$@"; do
    if ! grep -q -- "$pattern" "$dir/$name"; then
      echo "   $name: missing expected output: $pattern"
      missing=1
    fi
  done
  lines="$(awk '/claudeos shell/ { exit } /^data: / { n++ } END { print n + 0 }' "$dir/$name")"
  if [ "$lines" != 1 ]; then
    echo "   $name: $lines data: lines before the shell, where one is expected"
    missing=1
  fi
  if ! grep -q "claudeos shell" "$dir/$name"; then
    echo "   $name: the boot did not reach the shell"
    missing=1
  fi
  if ! grep -q "^data-test: the damaged card was walked to the end" "$dir/$name"; then
    echo "   $name: the walk over /data did not finish"
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
  record_boot_id "$(cat "$dir/$name")"
  return $missing
}

# Services at boot: the system list in the image, /etc/claudeos/services, and
# the user list on the card, /data/services.txt. The image booted is the test
# image with /etc/ntp.conf added, so that the system list's ntpd starts, and
# with /root/services.sh, which prints what /run/services and the logs say.
# Every boot has to reach the shell with the expected summary line before it,
# or with none and init's line saying why, and show ntpd started from the
# system list.
#
# On aarch64 the cards hold: a list of an always service that exits at once,
# a once service writing to /tmp, busybox httpd serving /data/site, a once
# service whose output passes the log cap, a program that does not exist and
# an off service, in CRLF lines; a list of malformed lines that runs past the
# byte cap; no list; the first card again with data=off; and a list naming
# ntpd. x86-64 has no card slot, so there the test image as built, which has no
# /etc/ntp.conf, has to leave ntpd unstarted, and the image with it has to
# start ntpd and skip the user list. On both, a starter that hangs and one that
# aborts, through the test image's servicetest= hook, have to leave the shell
# arriving and ntpd started.
run_services() {
  banner "services at boot"
  local dir ok=1 fatdisk="$ROOT/tools/fatdisk/target/release/fatdisk" card="" system_ntpd filler
  dir="$(mktemp -d)"

  # The list format's own tests, which build cbox for the Mac.
  if (cd "$ROOT/user/cbox" && cargo test --release -q > "$dir/host-tests" 2>&1); then
    echo "   user/cbox tests: $(grep -E '^test result' "$dir/host-tests" | awk '{ p += $4; f += $6 } END { print p " passed, " f " failed" }')"
  else
    echo "   user/cbox tests failed:"
    tail -n 40 "$dir/host-tests" | sed 's/^/     /'
    ok=0
  fi

  cp -R "$TREE" "$dir/services-tree"
  printf 'server ntp.nict.jp\nserver time.cloudflare.com\n' > "$dir/services-tree/etc/ntp.conf"
  cat > "$dir/services-tree/root/services.sh" <<'SCRIPT'
#!/bin/sh
# Run at the shell by the services section of scripts/test.sh; $1 names the boot.
field() {
  sed -n "s/^$2: //p" "/run/services/$1"
}
echo "--- cat /run/services/*"
cat /run/services/*
for name in ntpd flap note site chatty missing spare good late other; do
  if [ -f "/run/services/$name" ]; then
    echo "svc-status: $name list=$(field $name list) state=$(field $name state) starts=$(field $name starts) last=$(field $name 'last run')"
  else
    echo "svc-status: $name absent"
  fi
done
echo "svc-ntpd-command: $(field ntpd command)"
sed 's/^/svc-errors: /' /run/services/errors.txt
case "$1" in
main)
  echo "svc-once: $(cat /tmp/once.txt)"
  echo "svc-web: $(busybox wget -q -O - http://127.0.0.1:8080/index.html)"
  echo "svc-flap-waits: $(grep -o 'next start in [0-9]* s' /var/log/flap.log | head -n 4 | cut -d ' ' -f 4 | xargs echo)"
  size=$(wc -c < /var/log/chatty.log)
  if [ "$size" -le 262144 ]; then
    echo "svc-chatty: $size bytes, under the cap"
  else
    echo "svc-chatty: $size bytes, over the cap"
  fi
  # The run's head: the keeper's start line and the first 63 lines of output,
  # once each, then one cut line, and nothing after 63 until the newest part.
  echo "svc-chatty-head: $(grep -c ' chatty started, pid ' /var/log/chatty.log) $(grep -c '^1$' /var/log/chatty.log) $(grep -c '^63$' /var/log/chatty.log) $(grep -c '^64$' /var/log/chatty.log) $(grep -c 'the log was cut here' /var/log/chatty.log)"
  echo "svc-chatty-end: $(grep -c '^60000$' /var/log/chatty.log) $(grep -c '^chatty done$' /var/log/chatty.log)"
  ;;
bad)
  if [ -f /tmp/good.txt ]; then echo "svc-good: ran"; else echo "svc-good: did not run"; fi
  if [ -f /tmp/late.txt ]; then echo "svc-late: ran"; else echo "svc-late: did not run"; fi
  ;;
ntpd)
  if [ -f /tmp/other.txt ]; then echo "svc-other: ran"; else echo "svc-other: did not run"; fi
  if [ -f /tmp/impostor.txt ]; then echo "svc-impostor: ran"; else echo "svc-impostor: did not run"; fi
  ;;
esac
echo "svc-done"
SCRIPT
  python3 "$ROOT/tools/mkcpio.py" "$dir/services-tree" "$dir/services.cpio" > /dev/null
  rm -rf "$dir/services-tree"
  system_ntpd='^svc-status: ntpd list=system state=(running|waiting) starts=[1-9][0-9]* last='

  if [ "$ARCH" = x86_64 ]; then
    services_boot plain "$IMAGE" "" "" \
        "wait:0.5" "cat /run/services/ntpd /run/services/errors.txt\n" "wait:1.5"
    services_expect plain "services: 0 system started, 1 not started; no user list, /data is not mounted" \
        '^state: not started: it needs /etc/ntp.conf, which does not exist$' \
        '^/data/services.txt: /data is not mounted, so no user services were started$' || ok=0

    services_boot system "$dir/services.cpio" "" "" "wait:2" "sh /root/services.sh system\n" "until:svc-done"
    services_expect system "services: 1 system started; no user list, /data is not mounted" \
        "$system_ntpd" '^svc-ntpd-command: /bin/busybox ntpd -n -q$' \
        '^svc-errors: /data/services.txt: /data is not mounted, so no user services were started$' || ok=0
  elif [ "$(uname -s)" != Darwin ]; then
    echo "   the card images are made by macOS's newfs_msdos through hdiutil"
    skipped=$((skipped + 1))
  elif ! (cd "$ROOT/tools/fatdisk" && cargo build --release -q); then
    echo "   tools/fatdisk did not build"
    ok=0
  else
    card="$dir/main.img"
    services_card main "$(printf '%s\r\n' \
        "# The harness's user services, saved with CRLF line endings." \
        "flap     always  /bin/sh -c 'echo flap ran; exit 3'" \
        "note     once    /bin/sh -c \"echo written by once > /tmp/once.txt\"" \
        "site     always  /bin/httpd -f -p 8080 -h /data/site" \
        "chatty   once    /bin/sh -c '/bin/busybox seq 1 60000; echo chatty done'" \
        "missing  always  /bin/no-such-program --flag" \
        "spare    off     /bin/true")" || ok=0
    "$fatdisk" put "$card" CLAUDEDATA /site/index.html "<h1>served from /data</h1>" || ok=0
    # Ctrl-C at the prompt ends the sleep and reaches no service; the rest of
    # the wait lets flap fail four times.
    services_boot main "$dir/services.cpio" "$card" "" \
        "wait:1" "sleep 5\n" "wait:1.5" "\x03" "wait:10" "sh /root/services.sh main\n" "until:svc-done"
    services_expect main "services: 1 system started; 5 user started, 1 not started" \
        "$system_ntpd" \
        '^svc-status: flap list=user state=(running|waiting) starts=([3-9]|[1-9][0-9]) last=exited with status 3 after 0 s$' \
        '^svc-status: note list=user state=finished starts=1 last=exited with status 0 after [0-9]+ s$' \
        '^svc-status: site list=user state=running starts=1 last=none yet$' \
        '^svc-status: chatty list=user state=finished starts=1 last=exited with status 0 after [0-9]+ s$' \
        '^svc-status: missing list=user state=waiting starts=([3-9]|[1-9][0-9]) last=could not be started: No such file or directory \(os error 2\)$' \
        '^svc-status: spare list=user state=off starts=0 last=none yet$' \
        '^svc-once: written by once$' \
        '^svc-web: <h1>served from /data</h1>$' \
        '^svc-flap-waits: 1 2 4 8$' \
        '^svc-chatty: [0-9]+ bytes, under the cap$' \
        '^svc-chatty-head: 1 1 1 0 1$' \
        '^svc-chatty-end: 1 1$' \
        '!^svc-errors: ' || ok=0

    # Eight lines wrong in eight ways with a good line among them, and 70 KiB
    # of comments, which put the last line past the byte cap.
    filler="$(for n in $(seq 1 700); do printf '# filler line %085d\n' "$n"; done)"
    services_card bad "$(printf '%s\n' \
        "Web      always  /bin/true" \
        "web      sometimes  /bin/true" \
        "web      always  port=80 /bin/true" \
        "web      always  busybox httpd -f" \
        "web      always  /bin/echo 'not closed" \
        "good     once    /bin/sh -c 'echo good > /tmp/good.txt'" \
        "good     once    /bin/true" \
        "long     once    /bin/echo $(printf '%01100d' 0)" \
        "$(printf 'web      always  /bin/echo \033[2J')" \
        "$filler" \
        "late     once    /bin/sh -c 'echo late > /tmp/late.txt'")" || ok=0
    services_boot bad "$dir/services.cpio" "$dir/bad.img" "" "wait:2" "sh /root/services.sh bad\n" "until:svc-done"
    services_expect bad "services: 1 system started; 1 user started, 8 lines skipped, the end of the file not read (see /run/services/errors.txt)" \
        "$system_ntpd" \
        '^svc-errors: /data/services.txt line 1: the name `Web` is not 1 to 32 of the characters a-z, 0-9, _ and -$' \
        '^svc-errors: /data/services.txt line 2: the policy `sometimes` is not always, once or off$' \
        '^svc-errors: /data/services.txt line 3: `port=` is not an option; the options are needs=, every=, limit= and backoff=$' \
        '^svc-errors: /data/services.txt line 4: `busybox` is neither an option nor the absolute path of a program$' \
        '^svc-errors: /data/services.txt line 5: a single quote is not closed$' \
        '^svc-errors: /data/services.txt line 7: the name good is already used on line 6$' \
        '^svc-errors: /data/services.txt line 8: it is 1127 bytes long, and a line may be 1024$' \
        '^svc-errors: /data/services.txt line 9: it holds a control character$' \
        '^svc-errors: /data/services.txt: only the first 65536 bytes were read, so lines from [0-9]+ on were not$' \
        '^svc-good: ran$' '^svc-late: did not run$' || ok=0
    rm -f "$dir/bad.img"

    services_card nofile "" || ok=0
    services_boot nofile "$dir/services.cpio" "$dir/nofile.img" "" "wait:2" "sh /root/services.sh nofile\n" "until:svc-done"
    services_expect nofile "services: 1 system started; no user list, /data/services.txt does not exist" \
        "$system_ntpd" \
        '^svc-errors: /data/services.txt: it does not exist, so no user services were started$' || ok=0
    rm -f "$dir/nofile.img"

    services_boot off "$dir/services.cpio" "$card" "data=off" "wait:2" "sh /root/services.sh off\n" "until:svc-done"
    services_expect off "services: 1 system started; no user list, /data is not mounted" \
        "$system_ntpd" '^svc-status: site absent$' \
        '^svc-errors: /data/services.txt: /data is not mounted, so no user services were started$' || ok=0

    services_card ntpd "$(printf '%s\n' \
        "ntpd     always  /bin/sh -c 'echo impostor > /tmp/impostor.txt'" \
        "other    once    /bin/sh -c 'echo other > /tmp/other.txt'")" || ok=0
    services_boot ntpd "$dir/services.cpio" "$dir/ntpd.img" "" "wait:2" "sh /root/services.sh ntpd\n" "until:svc-done"
    services_expect ntpd "services: 1 system started; 1 user started, 1 line skipped (see /run/services/errors.txt)" \
        "$system_ntpd" '^svc-ntpd-command: /bin/busybox ntpd -n -q$' \
        '^svc-errors: /data/services.txt line 1: ntpd is the name of a system service, which runs as the system list has it, and a user service cannot replace or disable it$' \
        '^svc-other: ran$' '^svc-impostor: did not run$' || ok=0
    rm -f "$dir/ntpd.img"
  fi

  # The starter stopped after the system services and before the user list,
  # with the first card in the slot on aarch64, whose services it would
  # otherwise start.
  services_boot hang "$dir/services.cpio" "$card" "servicetest=hang" "wait:1" "sh /root/services.sh hang\n" "until:svc-done"
  services_expect hang none \
      '^init: the service starter has not finished after 10 s; starting the shell, and the starter carries on$' \
      "$system_ntpd" '^svc-status: site absent$' || ok=0
  services_boot abort "$dir/services.cpio" "$card" "servicetest=abort" "wait:1" "sh /root/services.sh abort\n" "until:svc-done"
  services_expect abort none \
      '^init: the service starter ended on signal 6 before it finished; the services it had not started are not running$' \
      "$system_ntpd" '^svc-status: site absent$' || ok=0
  rm -rf "$dir"

  if [ $ok -eq 1 ]; then
    echo ">> services at boot: OK"
  else
    echo ">> services at boot: FAILED"
    status=1
  fi
  echo
}

# A card image $dir/$1.img of 128 MiB with one FAT32 volume labelled
# CLAUDEDATA, holding $2 as /services.txt unless $2 is empty.
services_card() {
  "$fatdisk" mbr "$dir/$1.img" 128 CLAUDEDATA:rest > /dev/null || return 1
  if [ -n "$2" ]; then
    "$fatdisk" put "$dir/$1.img" CLAUDEDATA /services.txt "$2" || return 1
  fi
}

# Boot the image $2 with the card image $3, or no card when $3 is empty, and
# the command line $4; once the shell has started, send the steps after those,
# and power off. What the console showed is kept as $dir/$1.
services_boot() {
  local name="$1" image="$2" card="$3" append="$4"
  shift 4
  if [ -n "$card" ]; then
    python3 "$ROOT/tools/drive.py" --timeout 150 --initramfs "$image" --sd "$card" --append "$append" -- \
        "until:claudeos shell" "$@" "wait:0.5" "poweroff\n" "wait:3"
  else
    python3 "$ROOT/tools/drive.py" --timeout 150 --initramfs "$image" --append "$append" -- \
        "until:claudeos shell" "$@" "wait:0.5" "poweroff\n" "wait:3"
  fi 2>&1 | tr -d '\r' > "$dir/$name"
  record_boot_id "$(cat "$dir/$name")"
}

# Require, in what boot $1 showed: the line $2 as the only `services:` line
# before the shell started, or no such line when $2 is `none`; the shell; no
# panic; and each pattern after those, an extended regular expression, which
# has to match a line, or match none when it starts with `!`.
services_expect() {
  local name="$1" summary="$2" pattern missing=0 before
  shift 2
  echo "--- $name"
  grep -E "^services:|^init:|^svc-|^state:|^/data/services.txt|claudeos shell|KERNEL PANIC" "$dir/$name" | sed 's/^/   /'
  before="$(awk '/claudeos shell/ { exit } /^services: / { print }' "$dir/$name")"
  if [ "$summary" = none ]; then
    if [ -n "$before" ]; then
      echo "   $name: a summary line before the shell, where none is expected"
      missing=1
    fi
  elif [ "$before" != "$summary" ]; then
    echo "   $name: expected before the shell: $summary"
    missing=1
  fi
  for pattern in "$@"; do
    case "$pattern" in
      !*)
        if grep -qE -- "${pattern#!}" "$dir/$name"; then
          echo "   $name: unexpected output: ${pattern#!}"
          missing=1
        fi ;;
      *)
        if ! grep -qE -- "$pattern" "$dir/$name"; then
          echo "   $name: missing expected output: $pattern"
          missing=1
        fi ;;
    esac
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
run_data
run_services
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
