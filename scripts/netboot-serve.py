#!/usr/bin/env python3
# A read-only TFTP server, so a Raspberry Pi 4 can boot from files on this
# machine instead of from its SD card.
#
#   scripts/netboot-serve.py                        serve build/boot on udp port 69
#   scripts/netboot-serve.py --root DIR --port N    serve DIR on port N
#
# The Pi 4 keeps its bootloader in an EEPROM on the board. When BOOT_ORDER puts
# the network first, that bootloader gets an address over DHCP and then reads
# start4.elf, config.txt, the kernel, the ram disk and every other file the
# firmware asks for over TFTP from TFTP_IP, one request per file. This answers
# those requests from one directory and refuses everything else: no writes,
# nothing outside that directory.
#
# Standard library only, so there is nothing to install. On macOS 10.14 and
# later an ordinary user may bind a port below 1024; elsewhere run it as root
# or pass --port.
#
# The protocol is RFC 1350, with the option negotiation of RFC 2347 and the
# blksize, timeout and tsize options of RFC 2348 and RFC 2349. What the Pi 4
# bootloader does with it, which the code below is arranged around:
#
# - Its requests carry options, "tsize 0 blksize 1024" in the traces in
#   rpi-eeprom issue #74. Release 2022-10-03 raised the largest block size it
#   accepts to 1468.
# - A client may start a transfer only to learn the file exists or how big it
#   is, and abandon it with an ERROR. Raspberry Pi's network boot notes say the
#   boot ROM of earlier boards does this, so a client ERROR in the middle of a
#   transfer is an ordinary end.
# - It asks for <serial>/start4.elf and <serial>/start.elf first and drops the
#   prefix when neither exists, so "file not found" has to come back at once.
# - When a request goes unanswered it repeats it every two seconds from the
#   same port, and after TFTP_FILE_TIMEOUT (30 seconds by default) it sends an
#   ERROR to port 69, never having learned a transfer's port.
# - Since release 2025-09-22 it follows the block number from 65535 round to 0.
#   Earlier releases stop there, which at 1024 bytes a block is a 67 MiB file.
import argparse
import datetime
import errno
import json
import os
import signal
import socket
import stat
import struct
import sys
import threading
import time

RRQ, WRQ, DATA, ACK, ERROR, OACK = 1, 2, 3, 4, 5, 6
OPCODE_NAMES = {RRQ: "RRQ", WRQ: "WRQ", DATA: "DATA", ACK: "ACK", ERROR: "ERROR", OACK: "OACK"}

# Error codes, from RFC 1350.
ERR_UNDEFINED = 0
ERR_NOT_FOUND = 1
ERR_ACCESS = 2
ERR_ILLEGAL = 4
ERR_UNKNOWN_TID = 5

DEFAULT_BLKSIZE = 512
MIN_BLKSIZE, MAX_BLKSIZE = 8, 65464     # RFC 2348
MIN_TIMEOUT, MAX_TIMEOUT = 1, 255       # RFC 2349, whole seconds

# With no timeout option, wait this many seconds for an ACK before sending the
# last packet again, and give up after this many repeats. Six seconds without
# an answer is far past anything a live client on the same network does.
DEFAULT_TIMEOUT = 1
RETRIES = 5

# A reply has to come from the address the request was sent to: the Pi was
# told TFTP_IP and ignores packets from any other address. A socket bound to
# 0.0.0.0 takes its source address from the routing table, and on a machine
# with two interfaces on the Pi's network (Wi-Fi and an Ethernet adapter, say)
# that can be the other interface's address. So the listening socket asks the
# kernel for each request's destination address, and each transfer binds its
# socket to that address.
if sys.platform == "darwin" or "bsd" in sys.platform:
    # The control message is a bare struct in_addr.
    DSTADDR = getattr(socket, "IP_RECVDSTADDR", 7)
    DSTADDR_OFFSET = 0
elif sys.platform.startswith("linux"):
    # The control message is a struct in_pktinfo, whose second field is the
    # local address a reply should come from.
    DSTADDR = getattr(socket, "IP_PKTINFO", 8)
    DSTADDR_OFFSET = 4
else:
    DSTADDR = None
    DSTADDR_OFFSET = 0

log_lock = threading.Lock()


def log(line):
    stamp = datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S.%f")[:-3]
    with log_lock:
        print(f"{stamp}  {line}", flush=True)


def client(peer):
    return f"{peer[0]}:{peer[1]}"


def count(n, one, many):
    return f"{n} {one if n == 1 else many}"


# ---------------------------------------------------------------------------
# Packets
# ---------------------------------------------------------------------------

def data_packet(block, chunk):
    # Block numbers are 16 bits and go from 65535 round to 0.
    return struct.pack("!HH", DATA, block & 0xFFFF) + chunk


def oack_packet(options):
    fields = b"".join(name.encode() + b"\0" + value.encode() + b"\0"
                      for name, value in options.items())
    return struct.pack("!H", OACK) + fields


def error_packet(code, message):
    return struct.pack("!HH", ERROR, code) + message.encode("ascii", "replace") + b"\0"


def error_message(packet):
    return packet[4:].split(b"\0")[0].decode("ascii", "replace")


class Malformed(Exception):
    pass


def parse_request(packet):
    """Split an RRQ or WRQ into its file name, mode and options.

    The options come back as (name, value) pairs in the client's order, with
    the names in lower case, since RFC 2347 makes them case-insensitive."""
    fields = packet[2:].split(b"\0")
    # Every field ends in a NUL, so a well-formed request splits into its
    # fields and one empty string after the last NUL.
    if len(fields) < 3 or fields[-1] != b"":
        raise Malformed("request is not a file name and a mode, each ending in NUL")
    fields.pop()
    # Some boot ROMs pad the packet with more NULs.
    while len(fields) > 2 and fields[-1] == b"":
        fields.pop()
    if fields[0] == b"":
        raise Malformed("request has an empty file name")
    name = fields[0].decode("utf-8", "surrogateescape")
    mode = fields[1].decode("ascii", "replace").lower()
    options = [(fields[i].decode("ascii", "replace").lower(),
                fields[i + 1].decode("ascii", "replace"))
               for i in range(2, len(fields) - 1, 2)]
    return name, mode, options


# ---------------------------------------------------------------------------
# Files and options
# ---------------------------------------------------------------------------

class Refused(Exception):
    """A request answered with an error packet. `outcome` is what the log says."""

    def __init__(self, code, message, outcome):
        super().__init__(message)
        self.code = code
        self.message = message
        self.outcome = outcome


def open_requested(root, name):
    """Open the file a request names, and return it with its size.

    The name is taken relative to root whatever it starts with, and nothing
    that resolves outside root is served, whether the path gets there through
    '..' or through a symbolic link. The root is resolved again on every
    request, since mkcard.sh deletes and recreates it."""
    relative = name.lstrip("/")
    if relative == "":
        raise Refused(ERR_NOT_FOUND, "file not found", "not found (no name after the leading /)")
    if ".." in relative.split("/"):
        raise Refused(ERR_ACCESS, "access violation", "refused: the path contains '..'")
    base = os.path.realpath(root)
    path = os.path.realpath(os.path.join(base, relative))
    if os.path.commonpath([base, path]) != base:
        raise Refused(ERR_ACCESS, "access violation",
                      f"refused: the path resolves outside the served directory, to {path}")
    try:
        info = os.stat(path)
    except (FileNotFoundError, NotADirectoryError):
        raise Refused(ERR_NOT_FOUND, "file not found", "not found")
    except OSError as e:
        raise Refused(ERR_ACCESS, "access violation", f"refused: {e.strerror}")
    # Checked before opening, because opening a FIFO waits for a writer.
    if not stat.S_ISREG(info.st_mode):
        raise Refused(ERR_ACCESS, "not a regular file", "refused: not a regular file")
    try:
        f = open(path, "rb")
    except OSError as e:
        raise Refused(ERR_ACCESS, "access violation", f"refused: {e.strerror}")
    return f, os.fstat(f.fileno()).st_size


def number(text):
    """The value of a decimal option, or None if it is not one."""
    if text.isdigit() and len(text) <= 10:
        return int(text)
    return None


def negotiate(options, size):
    """Decide which of a request's options to acknowledge.

    Returns the block size and retransmission timeout to use, and the options
    for the OACK as a dict. An option this server does not know, or one whose
    value it cannot use, is left out of the OACK, and RFC 2347 has the client
    carry on without it. An empty dict means no OACK: the file starts at once
    with block 1, as in RFC 1350."""
    blksize, timeout, reply = DEFAULT_BLKSIZE, DEFAULT_TIMEOUT, {}
    for name, value in options:
        n = number(value)
        if n is None:
            continue
        if name == "blksize" and n >= MIN_BLKSIZE:
            # RFC 2348 lets the server answer with a smaller size than asked.
            blksize = min(n, MAX_BLKSIZE)
            reply["blksize"] = str(blksize)
        elif name == "tsize":
            # In a read request the client sends 0 and the answer is the size.
            reply["tsize"] = str(size)
        elif name == "timeout" and MIN_TIMEOUT <= n <= MAX_TIMEOUT:
            # RFC 2349: acknowledged with the client's own value or not at all.
            timeout = n
            reply["timeout"] = str(n)
    return blksize, timeout, reply


# ---------------------------------------------------------------------------
# Transfers
# ---------------------------------------------------------------------------

class Transfer(threading.Thread):
    """One request, answered from a socket of its own.

    RFC 1350 has each end of a transfer take a fresh port, its transfer ID,
    for that transfer alone. The server's is this socket's port, which is how
    the Pi's many transfers at once stay apart."""

    def __init__(self, server, request, peer, local):
        super().__init__(daemon=True)
        self.server = server
        self.request = request
        self.peer = peer
        self.local = local
        self.sock = None
        # Held while sending, since the listening thread can also send the
        # last packet again (see Server.start).
        self.send_lock = threading.Lock()
        self.last = None
        self.heard_from_client = False
        self.superseded = False
        self.retransmits = 0
        self.repeats = 0
        self.strays = 0
        self.send_failure = None
        self.summary = "request"

    def run(self):
        started = time.monotonic()
        try:
            self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            try:
                self.sock.bind((self.local, 0))
            except OSError:
                # A request sent to a broadcast address names no address of
                # this machine to answer from.
                self.sock.bind(("", 0))
            outcome = self.serve()
        except Exception as e:
            outcome = f"failed inside the server: {e!r}"
        finally:
            self.server.forget(self)
            if self.sock is not None:
                self.sock.close()
        notes = [f"{time.monotonic() - started:.3f} s"]
        if self.retransmits:
            notes.append(f"{count(self.retransmits, 'packet', 'packets')} sent again after a timeout")
        if self.repeats:
            notes.append(f"client repeated the request {count(self.repeats, 'time', 'times')}")
        if self.strays:
            notes.append(f"refused {count(self.strays, 'packet', 'packets')} from another port")
        if self.send_failure:
            notes.append(f"a send failed: {self.send_failure}")
        log(f"{client(self.peer)}  {self.summary}  ->  {outcome}  ({', '.join(notes)})")

    def serve(self):
        opcode = struct.unpack("!H", self.request[:2])[0]
        kind = OPCODE_NAMES[opcode]
        try:
            name, mode, options = parse_request(self.request)
        except Malformed as e:
            self.summary = f"{kind} (malformed)"
            self.send(error_packet(ERR_ILLEGAL, str(e)))
            return f"refused: {e}"
        shown = " ".join(f"{k}={v}" for k, v in options) or "no options"
        self.summary = f"{kind} {json.dumps(name)}  {shown}"
        if opcode == WRQ:
            self.send(error_packet(ERR_ACCESS, "this server is read-only"))
            return "refused: write request, this server is read-only"
        if mode != "octet":
            self.send(error_packet(ERR_ILLEGAL, "only octet mode is served"))
            return f"refused: mode {mode}, only octet is served"
        try:
            f, size = open_requested(self.server.root, name)
        except Refused as r:
            self.send(error_packet(r.code, r.message))
            return r.outcome
        with f:
            return self.send_file(f, size, options)

    def send_file(self, f, size, options):
        blksize, timeout, reply = negotiate(options, size)
        if reply:
            oack = " ".join(f"{k}={v}" for k, v in reply.items())
            failure = self.exchange(oack_packet(reply), 0, timeout)
            if failure:
                return f"{failure}; the OACK was {oack}"
        block = 1
        sent = 0
        while True:
            # A block shorter than blksize ends the file, so a file whose size
            # is a multiple of blksize ends with an empty block.
            chunk = f.read(blksize)
            failure = self.exchange(data_packet(block, chunk), block, timeout)
            if failure:
                return f"{failure} at block {block}, after {sent} of {size} bytes"
            sent += len(chunk)
            if len(chunk) < blksize:
                return f"sent {sent} bytes in {count(block, 'block', 'blocks')} of {blksize}"
            block += 1

    def exchange(self, packet, block, timeout):
        """Send a packet and wait for the ACK of `block`.

        Returns None once that ACK arrives, or a description of why the
        transfer is over."""
        self.send(packet)
        wanted = block & 0xFFFF
        retries = 0
        deadline = time.monotonic() + timeout
        while True:
            if self.superseded:
                return "abandoned: the client sent another request from the same port"
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                if retries == RETRIES:
                    return f"timed out after {RETRIES} retransmissions"
                retries += 1
                self.retransmits += 1
                self.send(packet)
                deadline = time.monotonic() + timeout
                continue
            self.sock.settimeout(remaining)
            try:
                reply, source = self.sock.recvfrom(65536)
            except socket.timeout:
                continue
            if source != self.peer:
                # RFC 1350: tell the sender it has the wrong transfer, and
                # leave this one undisturbed.
                self.strays += 1
                try:
                    self.sock.sendto(error_packet(ERR_UNKNOWN_TID, "unknown transfer ID"), source)
                except OSError:
                    pass
                continue
            if len(reply) < 4:
                continue
            opcode, value = struct.unpack("!HH", reply[:4])
            if opcode == ACK:
                self.heard_from_client = True
                if value == wanted:
                    return None
                # An ACK of an earlier block is a duplicate. Answering one by
                # sending again makes both ends send every later block twice
                # (RFC 1123 section 4.2.3.1), so only the timeout sends again.
                continue
            if opcode == ERROR:
                self.heard_from_client = True
                return f"aborted by client (error {value} {json.dumps(error_message(reply))})"
            self.send(error_packet(ERR_ILLEGAL, "expected an ACK"))
            return f"refused: client sent {OPCODE_NAMES.get(opcode, opcode)} during the transfer"

    def send(self, packet):
        with self.send_lock:
            self.last = packet
            try:
                self.sock.sendto(packet, self.peer)
            except OSError as e:
                # Treated as a lost packet: the timeout sends it again.
                self.send_failure = e.strerror

    def repeat_last(self):
        """Send the last packet again, for a client that repeated its request."""
        with self.send_lock:
            self.repeats += 1
            if self.last is not None:
                try:
                    self.sock.sendto(self.last, self.peer)
                except OSError as e:
                    self.send_failure = e.strerror


class Server:
    def __init__(self, root, sock, port, dstaddr):
        self.root = root
        self.sock = sock
        self.port = port
        self.dstaddr = dstaddr
        self.lock = threading.Lock()
        # Transfers in progress, by client address and port.
        self.transfers = {}

    def serve_forever(self):
        while True:
            try:
                packet, peer, local = self.receive()
                self.dispatch(packet, peer, local)
            except Exception as e:
                log(f"error on the request port: {e!r}")

    def receive(self):
        if self.dstaddr is None:
            packet, peer = self.sock.recvfrom(65536)
            return packet, peer, ""
        packet, ancillary, _, peer = self.sock.recvmsg(65536, 256)
        local = ""
        for level, kind, data in ancillary:
            if level == socket.IPPROTO_IP and kind == self.dstaddr \
                    and len(data) >= DSTADDR_OFFSET + 4:
                local = socket.inet_ntoa(data[DSTADDR_OFFSET:DSTADDR_OFFSET + 4])
        return packet, peer, local

    def dispatch(self, packet, peer, local):
        if len(packet) < 2:
            return
        opcode = struct.unpack("!H", packet[:2])[0]
        if opcode in (RRQ, WRQ):
            self.start(packet, peer, local)
        elif opcode == ERROR:
            code = struct.unpack("!H", packet[2:4])[0] if len(packet) >= 4 else "?"
            log(f"{client(peer)}  ERROR sent to port {self.port}, outside any transfer: "
                f"error {code} {json.dumps(error_message(packet))}")
        else:
            log(f"{client(peer)}  ignored {OPCODE_NAMES.get(opcode, f'opcode {opcode}')} "
                f"sent to port {self.port}, outside any transfer")

    def start(self, packet, peer, local):
        with self.lock:
            current = self.transfers.get(peer)
            if current is not None:
                if current.request == packet and not current.heard_from_client:
                    # The client has heard nothing yet and asked again from
                    # the same port. Answer again from the transfer already
                    # waiting, rather than start a second one that the client
                    # would hear from as well.
                    current.repeat_last()
                    return
                # A client takes a new port for each transfer, so anything
                # else from this port means it has given up on the old one.
                current.superseded = True
            transfer = Transfer(self, packet, peer, local)
            self.transfers[peer] = transfer
        transfer.start()

    def forget(self, transfer):
        with self.lock:
            if self.transfers.get(transfer.peer) is transfer:
                del self.transfers[transfer.peer]


def main():
    repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    parser = argparse.ArgumentParser(
        description="Read-only TFTP server for booting a Raspberry Pi 4 over the network.")
    parser.add_argument("--root", default=os.path.join(repo, "build", "boot"),
                        help="directory to serve (default: %(default)s)")
    parser.add_argument("--port", type=int, default=69,
                        help="UDP port to listen on (default: %(default)s)")
    args = parser.parse_args()

    root = os.path.abspath(args.root)
    if not os.path.isdir(root):
        sys.exit(f"{root} is not a directory; ./scripts/mkcard.sh assembles build/boot")

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.bind(("0.0.0.0", args.port))
    except OSError as e:
        if e.errno == errno.EADDRINUSE:
            sys.exit(f"udp port {args.port} is taken; `lsof -nP -iUDP:{args.port}` names the process")
        if e.errno == errno.EACCES:
            sys.exit(f"not permitted to bind udp port {args.port}; below 1024 that needs root "
                     f"except on macOS 10.14 and later, so run as root or pass --port")
        raise
    dstaddr = DSTADDR
    if dstaddr is not None:
        try:
            sock.setsockopt(socket.IPPROTO_IP, dstaddr, 1)
        except OSError:
            dstaddr = None

    # `kill` stops it the way Ctrl-C does, with a last line in the log.
    signal.signal(signal.SIGTERM, signal.default_int_handler)
    log(f"serving {root} read-only on udp 0.0.0.0:{args.port}")
    try:
        Server(root, sock, args.port, dstaddr).serve_forever()
    except KeyboardInterrupt:
        log("stopped")


if __name__ == "__main__":
    main()
