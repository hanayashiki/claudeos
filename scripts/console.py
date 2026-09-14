#!/usr/bin/env python3
r"""A client for the telnet console, since macOS ships no telnet.

Interactive:

    scripts/console.py [HOST]

puts this terminal in raw mode and joins it to the board's console. Every key
goes to the board, Ctrl-C included. Ctrl-] quits.

Scripted:

    scripts/console.py [HOST] --send 'uname -a' --until 'Linux'

connects, waits until the kernel log the console sends on connect has arrived,
then runs the steps in the order given and prints what the board prints while
they run:

    --send TEXT      type TEXT, then Enter
    --type TEXT      type TEXT and nothing else
    --until REGEX    print until REGEX matches a line printed since the
                     previous step, and fail after --timeout seconds
    --wait SECS      print what arrives for SECS
    --stall SECS     read nothing for SECS

TEXT takes backslash escapes: \x03 is Ctrl-C and \x7f is backspace. --log also
prints the kernel log sent on connect, and --timestamps puts the seconds since
connecting in front of every line.

HOST is the board's address. Without one, the Mac's ARP table is searched for
a Raspberry Pi's hardware address, which begins dc:a6:32.

Exit status: 0 when every step finished; 1 when an --until timed out; 2 when
there was no host to connect to; 3 when the console was busy; 4 when the
connection closed before anything arrived, which is what a machine that is not
listening looks like through QEMU's port forwarding; 5 when the board closed or
reset the connection while the steps ran.
"""
import argparse
import codecs
import os
import re
import select
import socket
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "tools"))
# Importing would otherwise leave a __pycache__ directory in the source tree.
sys.dont_write_bytecode = True
from drive import Screen  # noqa: E402  the same rendering the serial driver uses

IAC, DONT, DO, WONT, WILL, SB, SE = 255, 254, 253, 252, 251, 250, 240
ECHO, SUPPRESS_GO_AHEAD = 1, 3
QUIT_KEY = 0x1D  # Ctrl-]
PI_PREFIX = (0xDC, 0xA6, 0x32)

ATTACHED = "telnet: console attached from "
BUSY = "telnet console busy"

# How long the connection has to be quiet, after the console says it attached,
# before the kernel log is taken to have arrived.
SETTLE_SECONDS = 0.5


def fail(status, message):
    sys.stderr.write("console.py: " + message + "\n")
    sys.exit(status)


# ---- finding the board ------------------------------------------------------

def raspberry_pis(arp_output):
    """(address, hardware address) for every Raspberry Pi in `arp -an` output.

    macOS leaves out leading zeros, as in dc:a6:32:1:2:3, so each octet is
    read as a number rather than compared as text.
    """
    found = []
    for line in arp_output.splitlines():
        match = re.search(r"\((\d+\.\d+\.\d+\.\d+)\) at ([0-9A-Fa-f:]+)", line)
        if not match:
            continue
        parts = match.group(2).split(":")
        if len(parts) != 6:
            continue
        octets = tuple(int(part, 16) for part in parts)
        if octets[:3] == PI_PREFIX:
            entry = (match.group(1), ":".join("%02x" % octet for octet in octets))
            if entry not in found:
                found.append(entry)
    return found


def discover():
    try:
        output = subprocess.run(["arp", "-an"], capture_output=True, text=True,
                                timeout=10).stdout
    except (OSError, subprocess.SubprocessError):
        output = ""
    pis = raspberry_pis(output)
    if len(pis) == 1:
        address, hardware = pis[0]
        sys.stderr.write("console.py: the Raspberry Pi in the ARP table is %s (%s)\n"
                         % (address, hardware))
        return address
    if not pis:
        fail(2, "no Raspberry Pi, a hardware address beginning dc:a6:32, is in this "
                "Mac's ARP table.\n"
                "Give the board's address instead: scripts/console.py 192.168.1.23\n"
                "The board prints it on the serial console at boot, on the line "
                "beginning 'telnet: the console is on', and the DHCP server lists it "
                "among its leases. The ARP table holds the board only after this Mac "
                "has exchanged packets with it, so pinging that address also puts it "
                "there.")
    fail(2, "more than one Raspberry Pi is in the ARP table; give one of these "
            "addresses: " + ", ".join("%s (%s)" % pi for pi in pis))


# ---- the protocol -----------------------------------------------------------

class Telnet:
    """The client's half of RFC 854: options answered, commands taken out."""

    def __init__(self, sock):
        self.sock = sock
        self.state = "data"
        self.verb = None
        # Options the board does that this end has agreed to.
        self.agreed = set()

    def type(self, data):
        """Send keys. Enter is CR, which RFC 854 sends as CR NUL, and a 0xFF
        is doubled so that it is not taken for a command."""
        data = data.replace(b"\xff", b"\xff\xff").replace(b"\r", b"\r\0")
        self.sock.sendall(data)

    def feed(self, chunk):
        """The data in what arrived, with the protocol taken out."""
        out = bytearray()
        for byte in chunk:
            state = self.state
            if state == "cr":
                # The NUL after a CR only says the CR is alone; LF is data.
                self.state = "data"
                if byte == 0:
                    continue
                state = "data"
            if state == "data":
                if byte == IAC:
                    self.state = "iac"
                else:
                    out.append(byte)
                    if byte == 0x0D:
                        self.state = "cr"
            elif state == "iac":
                if byte == IAC:
                    out.append(IAC)
                    self.state = "data"
                elif byte in (WILL, WONT, DO, DONT):
                    self.verb = byte
                    self.state = "option"
                elif byte == SB:
                    self.state = "sb"
                else:
                    self.state = "data"
            elif state == "option":
                self.answer(self.verb, byte)
                self.state = "data"
            elif state == "sb":
                if byte == IAC:
                    self.state = "sb-iac"
            elif state == "sb-iac":
                self.state = "data" if byte == SE else "sb"
        return bytes(out)

    def answer(self, verb, option):
        # RFC 1143: answer a request that changes something and nothing else,
        # so two ends that both keep to that cannot answer each other for ever.
        if verb == WILL:
            if option in (ECHO, SUPPRESS_GO_AHEAD):
                if option not in self.agreed:
                    self.agreed.add(option)
                    self.sock.sendall(bytes([IAC, DO, option]))
            else:
                self.sock.sendall(bytes([IAC, DONT, option]))
        elif verb == WONT:
            if option in self.agreed:
                self.agreed.discard(option)
                self.sock.sendall(bytes([IAC, DONT, option]))
        elif verb == DO:
            # This end does nothing on the board's behalf.
            self.sock.sendall(bytes([IAC, WONT, option]))
        # DONT asks this end to stop something it is not doing.


def connect(host, port, receive_buffer):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    if receive_buffer:
        # Before connecting, because the window offered in the handshake is
        # sized from it.
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, receive_buffer)
    sock.settimeout(10)
    try:
        sock.connect((host, port))
    except OSError as err:
        fail(2, "cannot connect to %s port %d: %s" % (host, port, err))
    sock.settimeout(None)
    return sock


# ---- interactive ------------------------------------------------------------

def interactive(sock, telnet, host, port, timestamps):
    import termios
    import tty

    stdin = sys.stdin.fileno()
    if not os.isatty(stdin):
        fail(2, "interactive mode needs a terminal; give steps to run a script")
    sys.stderr.write("console.py: connected to %s port %d; Ctrl-] quits\n" % (host, port))
    saved = termios.tcgetattr(stdin)
    start = time.time()
    line_start = True
    ended = "closed by the board"
    tty.setraw(stdin)
    try:
        while True:
            readable = select.select([sock, stdin], [], [])[0]
            if sock in readable:
                try:
                    chunk = sock.recv(65536)
                except ConnectionResetError:
                    ended = "reset by the board"
                    break
                if not chunk:
                    break
                data = telnet.feed(chunk)
                if timestamps:
                    stamped = bytearray()
                    for byte in data:
                        if line_start:
                            stamped += b"[%9.3f] " % (time.time() - start)
                        stamped.append(byte)
                        line_start = byte == 0x0A
                    data = bytes(stamped)
                os.write(sys.stdout.fileno(), data)
            if stdin in readable:
                keys = os.read(stdin, 1024)
                if QUIT_KEY in keys:
                    telnet.type(keys[:keys.index(QUIT_KEY)])
                    ended = "closed"
                    break
                telnet.type(keys)
    finally:
        termios.tcsetattr(stdin, termios.TCSADRAIN, saved)
    sock.close()
    sys.stderr.write("\nconsole.py: connection %s\n" % ended)
    return 0


# ---- scripted ---------------------------------------------------------------

class Lines:
    """Where the screen puts finished lines: kept for matching, and printed
    once printing is on."""

    def __init__(self, timestamps, start):
        self.timestamps = timestamps
        self.start = start
        self.printing = False
        self.lines = []

    def write(self, text):
        for line in text.splitlines():
            self.lines.append(line)
            if self.printing:
                if self.timestamps:
                    sys.stdout.write("[%9.3f] " % (time.time() - self.start))
                sys.stdout.write(line + "\n")

    def flush(self):
        sys.stdout.flush()


class Session:
    def __init__(self, sock, telnet, timestamps):
        self.sock = sock
        self.telnet = telnet
        self.lines = Lines(timestamps, time.time())
        self.screen = Screen(self.lines)
        self.received = 0
        self.last_arrival = time.time()
        self.ended = None

    def pump(self, seconds):
        """Read and render what arrives within `seconds`. False once the
        connection has ended."""
        if self.ended:
            return False
        if not select.select([self.sock], [], [], max(seconds, 0))[0]:
            return True
        try:
            chunk = self.sock.recv(65536)
        except ConnectionResetError:
            chunk = None
            self.ended = "reset"
        if not chunk:
            self.ended = self.ended or "closed"
            self.screen.close()
            return False
        self.received += len(chunk)
        self.last_arrival = time.time()
        self.screen.feed(self.telnet.feed(chunk))
        return True

    def partial_line(self):
        return "".join(self.screen.line).rstrip()

    def settle(self, timeout):
        """Wait for the console's line saying this connection attached, and
        then for the kernel log in front of it to stop arriving."""
        deadline = time.time() + timeout
        attached = False
        while time.time() < deadline:
            alive = self.pump(0.1)
            if any(line.startswith(BUSY) for line in self.lines.lines):
                self.screen.close()
                fail(3, [line for line in self.lines.lines if line.startswith(BUSY)][0])
            if not alive:
                if self.received == 0:
                    fail(4, "the connection closed before anything arrived; nothing "
                            "is listening on the telnet port")
                fail(5, "the connection %s before the console said it attached"
                     % self.ended)
            attached = attached or any(ATTACHED in line for line in self.lines.lines)
            if attached and time.time() - self.last_arrival >= SETTLE_SECONDS:
                return
        fail(1, "the console did not say it attached within %g s" % timeout)

    def until(self, pattern, timeout):
        regex = re.compile(pattern)
        mark = len(self.lines.lines)
        deadline = time.time() + timeout
        while True:
            if any(regex.search(line) for line in self.lines.lines[mark:]):
                return True
            if regex.search(self.partial_line()):
                return True
            if time.time() >= deadline:
                return False
            if not self.pump(min(0.1, deadline - time.time())):
                return any(regex.search(line) for line in self.lines.lines[mark:])

    def wait(self, seconds):
        deadline = time.time() + seconds
        while time.time() < deadline:
            if not self.pump(min(0.1, deadline - time.time())):
                return


def text_argument(value):
    return codecs.escape_decode(value.encode())[0]


def scripted(sock, telnet, args):
    session = Session(sock, telnet, args.timestamps)
    session.lines.printing = args.log
    session.settle(args.timeout)
    session.lines.printing = True
    for kind, value in args.steps:
        if kind == "send":
            telnet.type(text_argument(value) + b"\r")
        elif kind == "type":
            telnet.type(text_argument(value))
        elif kind == "until":
            if not session.until(value, args.timeout):
                if session.ended:
                    break
                sys.stdout.flush()
                fail(1, "no line matching %r within %g s" % (value, args.timeout))
        elif kind == "wait":
            session.wait(float(value))
        elif kind == "stall":
            time.sleep(float(value))
        if session.ended:
            break
    if not session.ended:
        # Whatever came with the last step is still worth printing.
        session.pump(0)
    sys.stdout.flush()
    if session.ended:
        session.screen.close()
        fail(5, "the board %s the connection" % session.ended)
    session.screen.close()
    sock.close()
    return 0


class Step(argparse.Action):
    """Keeps every step option in one list, in the order given."""

    def __call__(self, parser, namespace, values, option_string=None):
        steps = list(getattr(namespace, self.dest) or [])
        steps.append((option_string.lstrip("-"), values))
        setattr(namespace, self.dest, steps)


def main():
    parser = argparse.ArgumentParser(
        description="Reach the claudeos telnet console. With no steps, an "
                    "interactive terminal where Ctrl-] quits.")
    parser.add_argument("host", nargs="?",
                        help="the board's address; found in the ARP table if left out")
    parser.add_argument("--port", type=int, default=23)
    for name, metavar, text in (
            ("--send", "TEXT", "type TEXT, then Enter"),
            ("--type", "TEXT", "type TEXT and nothing else"),
            ("--until", "REGEX", "print until a line matches"),
            ("--wait", "SECS", "print what arrives for SECS"),
            ("--stall", "SECS", "read nothing for SECS")):
        parser.add_argument(name, action=Step, dest="steps", metavar=metavar, help=text)
    parser.add_argument("--timeout", type=float, default=10,
                        help="seconds an --until, or the wait for the console, may take")
    parser.add_argument("--log", action="store_true",
                        help="print the kernel log sent on connect as well")
    parser.add_argument("--timestamps", action="store_true",
                        help="seconds since connecting in front of every line")
    parser.add_argument("--receive-buffer", type=int, default=0, metavar="BYTES",
                        help="this end's socket receive buffer, to act as a slow reader")
    parser.set_defaults(steps=[])
    args = parser.parse_args()

    host = args.host or discover()
    sock = connect(host, args.port, args.receive_buffer)
    telnet = Telnet(sock)
    if args.steps:
        return scripted(sock, telnet, args)
    return interactive(sock, telnet, host, args.port, args.timestamps)


if __name__ == "__main__":
    sys.exit(main())
