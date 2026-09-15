#!/usr/bin/env python3
"""Drive a claudeos boot over the serial console with timed input.

QEMU's stdio character device does not pick up input written to a pipe after
start-up, so the console is exposed as a unix socket instead and this script
connects to it. Pass --tty to go through scripts/run.sh on a terminal instead:
a socket hands every byte to the guest untouched, so it says nothing about the
keys a terminal's line discipline acts on before the guest can see them.

Usage:
    drive.py [--timeout SECS] [--initramfs FILE] [--kernel FILE]
             [--append CMDLINE] [--raw] [--tty] -- <step> [<step> ...]

--kernel boots a kernel image other than the one in build/, such as a copy
with a byte changed.

Each step is either text to send (backslash escapes are interpreted),
"wait:SECONDS" to pause, or "until:TEXT" to pause until TEXT has been printed.
A boot takes a different length of time on each machine, so waiting for the
prompt is steadier than guessing at how long to sleep.

Output is rendered the way a terminal would render it, so the redraws a line
editor performs collapse into the line you would actually see. Pass --raw to
get the bytes untouched.
"""
import os
import pty
import select
import signal
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
            elif byte == 0x09:
                # A tab moves the cursor to the next stop, every eight columns,
                # without writing over what it passes, as a terminal does. It
                # was drawn as a '.' before, so output such as nslookup's
                # "Name:<tab>example.com" never matched a pattern with a space.
                self.column = (self.column // 8 + 1) * 8
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


class SocketConsole:
    """The console as a unix socket, with QEMU started here.

    Every byte written goes to the guest as it stands, which is what makes a
    scripted session repeatable. Nothing enforces the timeout on this side;
    the driver's own deadline is what ends the run.
    """

    def __init__(self, initramfs, append, timeout, kernel):
        sock_path = os.path.join(tempfile.mkdtemp(), "console.sock")
        console = [
            "-chardev", f"socket,id=console,path={sock_path},server=on,wait=off",
            "-serial", "chardev:console",
            "-display", "none",
            "-no-reboot",
        ]
        if os.environ.get("ARCH") == "aarch64":
            # The flat image, because only that form gets a ram disk and a
            # command line, and the first serial port, because that is the
            # PL011.
            command = [
                "qemu-system-aarch64",
                "-M", "raspi4b",
                "-kernel", kernel or os.path.join(ROOT, "build", "kernel8.img"),
                "-initrd", initramfs,
            ] + console
        else:
            command = [
                "qemu-system-x86_64",
                "-kernel", kernel or os.path.join(ROOT, "build", "kernel.elf"),
                "-initrd", initramfs,
            ] + console + [
                "-m", "512M",
                "-cpu", "qemu64,+pdpe1gb,+rdrand,+fsgsbase,+xsave",
            ]
        if append:
            command += ["-append", append]
        self.process = subprocess.Popen(
            command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

        # Wait for QEMU to create the socket.
        self.connection = None
        for _ in range(200):
            if os.path.exists(sock_path):
                try:
                    self.connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    self.connection.connect(sock_path)
                    break
                except OSError:
                    self.connection = None
            time.sleep(0.05)
        if self.connection is None:
            self.process.kill()
            raise OSError("could not connect to the console socket")
        self.connection.settimeout(0.3)

    def recv(self):
        try:
            chunk = self.connection.recv(4096)
        except socket.timeout:
            return b""
        except OSError:
            return None
        return chunk or None

    def send(self, payload):
        self.connection.sendall(payload)

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        try:
            self.connection.close()
        except OSError:
            pass


class TerminalConsole:
    """The console as a terminal, through scripts/run.sh.

    A socket carries a keystroke to the guest whatever it is, so it cannot
    tell whether the terminal would have kept that key for the host. This form
    goes through the same path a person does, which is where the interrupt
    character is either passed on to the guest or taken by QEMU.
    """

    def __init__(self, initramfs, append, timeout, kernel):
        self.master, slave = pty.openpty()
        command = [os.path.join(ROOT, "scripts", "run.sh"),
                   "--timeout", str(timeout), "--initrd", initramfs]
        if kernel:
            command += ["--kernel", kernel]
        if append:
            command += ["--append", append]
        # Its own session, so a signal the terminal raises reaches this guest
        # and not the script driving it, and the whole group can be cleared
        # away at the end.
        self.process = subprocess.Popen(
            command, stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
        os.close(slave)

    def recv(self):
        try:
            if not select.select([self.master], [], [], 0.3)[0]:
                return b""
            chunk = os.read(self.master, 4096)
        except OSError:
            return None
        return chunk or None

    def send(self, payload):
        os.write(self.master, payload)

    def close(self):
        if self.process.poll() is None:
            try:
                os.killpg(os.getpgid(self.process.pid), signal.SIGKILL)
            except OSError:
                pass
        try:
            os.close(self.master)
        except OSError:
            pass


def main():
    args = sys.argv[1:]
    timeout = 60
    initramfs = os.path.join(ROOT, "build", "initramfs.cpio")
    kernel = None
    append = None
    raw = False
    tty = False

    while args and args[0].startswith("--"):
        if args[0] == "--timeout":
            timeout, args = int(args[1]), args[2:]
        elif args[0] == "--initramfs":
            initramfs, args = args[1], args[2:]
        elif args[0] == "--kernel":
            kernel, args = args[1], args[2:]
        elif args[0] == "--append":
            append, args = args[1], args[2:]
        elif args[0] == "--raw":
            raw, args = True, args[1:]
        elif args[0] == "--tty":
            tty, args = True, args[1:]
        elif args[0] == "--":
            args.pop(0)
            break
        else:
            args.pop(0)

    # A guest left spinning starves every later run, so clear any stale one.
    reaper = os.path.join(ROOT, "scripts", "reap-stale.sh")
    if os.path.exists(reaper):
        subprocess.run([reaper, "15"], check=False)

    try:
        console = TerminalConsole(initramfs, append, timeout, kernel) if tty \
            else SocketConsole(initramfs, append, timeout, kernel)
    except OSError as err:
        print(err, file=sys.stderr)
        return 1

    stop = threading.Event()
    screen = None if raw else Screen(sys.stdout)
    transcript = bytearray()

    def reader():
        while not stop.is_set():
            chunk = console.recv()
            if chunk is None:
                break
            if not chunk:
                continue
            transcript.extend(chunk)
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
            if step.startswith("until:"):
                wanted = step[6:].encode()
                # Give up rather than hang: the session that follows will fail
                # on what it did not see, which says more than a stall does.
                limit = min(time.time() + 30, deadline)
                while wanted not in transcript and time.time() < limit:
                    time.sleep(0.1)
                continue
            payload = step.encode().decode("unicode_escape").encode("latin-1")
            console.send(payload)
            time.sleep(0.3)
    except OSError:
        pass

    while time.time() < deadline and console.process.poll() is None:
        time.sleep(0.2)

    # Stop the reader before taking its console away, or a session that ran
    # to the deadline ends on a read of a descriptor that has just been shut.
    stop.set()
    thread.join(timeout=2)
    console.close()
    if screen is not None:
        screen.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
