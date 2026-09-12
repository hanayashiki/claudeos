#!/usr/bin/env python3
"""Drive a claudeos boot over the serial console with timed input.

QEMU's stdio character device does not pick up input written to a pipe after
start-up, so the console is exposed as a unix socket instead and this script
connects to it.

Usage:
    drive.py [--timeout SECS] [--initramfs FILE] [--append CMDLINE] [--raw]
             -- <step> [<step> ...]

Each step is either text to send (backslash escapes are interpreted) or
"wait:SECONDS" to pause.

Output is rendered the way a terminal would render it, so the redraws a line
editor performs collapse into the line you would actually see. Pass --raw to
get the bytes untouched.
"""
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


class Screen:
    """Just enough terminal to render one line at a time.

    A line editor repaints the whole line after every keystroke, which is
    unreadable in a raw capture. Applying the carriage returns, erases and
    cursor moves leaves only the final state of each line.
    """

    def __init__(self, out):
        self.out = out
        self.line = []
        self.column = 0
        self.pending = b""

    def feed(self, chunk):
        data = self.pending + chunk
        self.pending = b""
        index = 0
        while index < len(data):
            byte = data[index]
            if byte == 0x1B:
                consumed = self._escape(data, index)
                if consumed is None:
                    self.pending = data[index:]
                    return
                index += consumed
                continue
            index += 1
            if byte == 0x0A:
                self._flush()
            elif byte == 0x0D:
                self.column = 0
            elif byte == 0x08:
                self.column = max(0, self.column - 1)
            elif byte == 0x07:
                pass
            else:
                self._put(chr(byte) if 0x20 <= byte < 0x7F else ".")

    def _put(self, char):
        while len(self.line) <= self.column:
            self.line.append(" ")
        self.line[self.column] = char
        self.column += 1

    def _escape(self, data, index):
        # CSI sequences look like ESC [ <digits> <final>.
        if index + 1 >= len(data):
            return None
        if data[index + 1] not in (0x5B, 0x4F):
            return 2
        cursor = index + 2
        params = ""
        # Parameters are digits and separators; the sequence ends at a letter.
        while cursor < len(data) and (chr(data[cursor]).isdigit() or chr(data[cursor]) in ";?"):
            params += chr(data[cursor])
            cursor += 1
        if cursor >= len(data):
            return None
        final = chr(data[cursor])
        leading = params.split(";")[0]
        count = int(leading) if leading.isdigit() else 1
        if final == "K":
            del self.line[self.column:]
        elif final == "C":
            self.column += count
        elif final == "D":
            self.column = max(0, self.column - count)
        elif final == "J":
            self.line = []
            self.column = 0
        elif final == "H":
            self.column = 0
        return cursor - index + 1

    def _flush(self):
        self.out.write("".join(self.line).rstrip() + "\n")
        self.out.flush()
        self.line = []
        self.column = 0

    def close(self):
        if self.line:
            self._flush()


def main():
    args = sys.argv[1:]
    timeout = 60
    initramfs = os.path.join(ROOT, "build", "initramfs.cpio")
    append = None
    raw = False

    while args and args[0].startswith("--"):
        if args[0] == "--timeout":
            timeout, args = int(args[1]), args[2:]
        elif args[0] == "--initramfs":
            initramfs, args = args[1], args[2:]
        elif args[0] == "--append":
            append, args = args[1], args[2:]
        elif args[0] == "--raw":
            raw, args = True, args[1:]
        elif args[0] == "--":
            args.pop(0)
            break
        else:
            args.pop(0)

    # A guest left spinning starves every later run, so clear any stale one.
    reaper = os.path.join(ROOT, "scripts", "reap-stale.sh")
    if os.path.exists(reaper):
        subprocess.run([reaper, "15"], check=False)

    sock_path = os.path.join(tempfile.mkdtemp(), "console.sock")
    command = [
        "qemu-system-x86_64",
        "-kernel", os.path.join(ROOT, "build", "kernel.elf"),
        "-initrd", initramfs,
        "-chardev", f"socket,id=console,path={sock_path},server=on,wait=off",
        "-serial", "chardev:console",
        "-display", "none",
        "-m", "512M",
        "-no-reboot",
        "-cpu", "qemu64,+pdpe1gb,+rdrand,+fsgsbase,+xsave",
    ]
    if append:
        command += ["-append", append]
    process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

    # Wait for QEMU to create the socket.
    connection = None
    for _ in range(200):
        if os.path.exists(sock_path):
            try:
                connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                connection.connect(sock_path)
                break
            except OSError:
                connection = None
        time.sleep(0.05)
    if connection is None:
        process.kill()
        print("could not connect to the console socket", file=sys.stderr)
        return 1

    stop = threading.Event()
    screen = None if raw else Screen(sys.stdout)

    def reader():
        connection.settimeout(0.3)
        while not stop.is_set():
            try:
                chunk = connection.recv(4096)
            except socket.timeout:
                continue
            except OSError:
                break
            if not chunk:
                break
            if screen is None:
                sys.stdout.buffer.write(chunk)
                sys.stdout.buffer.flush()
            else:
                screen.feed(chunk)

    thread = threading.Thread(target=reader, daemon=True)
    thread.start()

    deadline = time.time() + timeout
    try:
        for step in args:
            if step.startswith("wait:"):
                time.sleep(float(step[5:]))
                continue
            payload = step.encode().decode("unicode_escape").encode("latin-1")
            connection.sendall(payload)
            time.sleep(0.3)
    except OSError:
        pass

    while time.time() < deadline and process.poll() is None:
        time.sleep(0.2)

    stop.set()
    if process.poll() is None:
        process.kill()
    thread.join(timeout=2)
    if screen is not None:
        screen.close()
    try:
        connection.close()
    except OSError:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
