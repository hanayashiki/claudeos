#!/bin/bash
# A few megabytes each way through the real card, over a link that loses
# packets, compared byte for byte against what was meant to arrive.
#
# Usage: lossy-transfer.sh [MEGABYTES] [ONE_IN]
#
# ONE_IN is the fraction of frames thrown away in each direction, as one in
# that many. QEMU's user mode network has no way to lose a packet -- its
# netfilters delay, dump, mirror, redirect and rewrite, and none of them drops
# one -- so the kernel is told to lose them itself, at the seam every driver
# sends and receives through. Everything below that seam, the card and its
# rings and its interrupt, does what it does on any other link.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MEGABYTES="${1:-4}"
ONE_IN="${2:-50}"
BYTES=$((MEGABYTES * 1024 * 1024))
PORT="${PORT:-18080}"
ARCH="${ARCH:-x86_64}"
export ARCH
if [ "$ARCH" = aarch64 ]; then
  echo "the emulated Pi 4 has no network device, so there is nothing to run this over" >&2
  exit 1
fi

# KEEP names a directory to leave the serial log and the two streams in.
WORK="${KEEP:-$(mktemp -d)}"
mkdir -p "$WORK"
LOG="$WORK/serial.log"
trap '[ -z "${KEEP:-}" ] && rm -rf "$WORK"' EXIT

echo "== serving $MEGABYTES MiB on port $PORT, losing one frame in $ONE_IN =="
# Job control, so the guest and the wrapper run.sh puts around it land in a
# process group of their own and can be killed together. Killing the wrapper
# alone leaves the emulator running and holding the forwarded port.
set -m
"$ROOT/scripts/run.sh" --timeout 240 --hostfwd "tcp::$PORT-:$PORT" \
    --initrd "$ROOT/build/initramfs.cpio" \
    --append "netloss=$ONE_IN init=/bin/inet serve $PORT 4" \
    < /dev/null > "$LOG" 2>&1 &
QEMU=$!
# run.sh runs the emulator under a wrapper that holds the timeout, so killing
# the wrapper alone leaves the emulator holding the forwarded port.
set +m
stop_guest() {
  kill -- -"$QEMU" 2>/dev/null
  wait "$QEMU" 2>/dev/null
}
trap 'stop_guest; [ -z "${KEEP:-}" ] && rm -rf "$WORK"' EXIT

for _ in $(seq 1 60); do
  if grep -q "inet: listening" "$LOG" 2>/dev/null; then break; fi
  if ! kill -0 "$QEMU" 2>/dev/null; then break; fi
  sleep 1
done
if ! grep -q "inet: listening" "$LOG" 2>/dev/null; then
  echo "the guest never got as far as listening:"
  tr -d '\r' < "$LOG" | tail -20
  exit 1
fi
grep -a "net: losing" "$LOG" | tr -d '\r'
echo "listening; starting the transfers"

# The stream both ends agree on without either sending it: byte i is i mod 251.
python3 - "$BYTES" "$WORK/expected" <<'PY'
import sys
count = int(sys.argv[1])
cycle = bytes(range(251))
with open(sys.argv[2], 'wb') as out:
    written = 0
    while written < count:
        n = min(len(cycle), count - written)
        out.write(cycle[:n])
        written += n
PY

status=0

echo "-- the guest sending $MEGABYTES MiB --"
start=$(date +%s)
if ! curl -s --max-time 180 -o "$WORK/received" \
     "http://127.0.0.1:$PORT/bytes/$BYTES"; then
  echo "   the download failed"
  status=1
elif cmp -s "$WORK/received" "$WORK/expected"; then
  echo "   $(wc -c < "$WORK/received") bytes arrived byte for byte in $(( $(date +%s) - start ))s"
else
  echo "   what arrived is not what was sent:"
  cmp "$WORK/received" "$WORK/expected" | head -3
  status=1
fi

echo "-- the guest receiving $MEGABYTES MiB --"
start=$(date +%s)
want="$(python3 - "$WORK/expected" <<'PY'
import sys
h = 0xCBF29CE484222325
with open(sys.argv[1], 'rb') as f:
    while True:
        block = f.read(1 << 20)
        if not block:
            break
        for byte in block:
            h = ((h ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
print("bytes=%d digest=%016x" % (__import__('os').path.getsize(sys.argv[1]), h))
PY
)"
# Expect: 100-continue would have curl wait for a reply this server does not
# send, since it answers the whole request rather than its headers.
got="$(curl -s --max-time 180 --data-binary "@$WORK/expected" \
        -H 'Content-Type: application/octet-stream' -H 'Expect:' \
        "http://127.0.0.1:$PORT/sink" 2> "$WORK/upload.err" | tr -d '\r\n')"
upload_status=$?
if [ "$got" = "$want" ]; then
  echo "   $got, which is what was sent, in $(( $(date +%s) - start ))s"
else
  echo "   the guest read [$got], wanted [$want] (curl exited $upload_status)"
  cat "$WORK/upload.err"
  status=1
fi

stop_guest

if [ $status -eq 0 ]; then
  echo "== both transfers arrived whole over a lossy link =="
else
  echo "== a transfer over a lossy link did not arrive whole =="
fi
exit $status
