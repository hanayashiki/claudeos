#!/usr/bin/env python3
"""Drive a claudeos boot over the serial console with timed input.

QEMU's stdio character device does not pick up input written to a pipe after
start-up, so the console is exposed as a unix socket instead and this script
connects to it.

Usage:
    drive.py [--timeout SECS] [--initramfs FILE] -- <step> [<step> ...]

Each step is either text to send (backslash escapes are interpreted) or
"wait:SECONDS" to pause.
"""
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def main():
    args = sys.argv[1:]
    timeout = 60
    initramfs = os.path.join(ROOT, "build", "initramfs.cpio")

    while args and args[0].startswith("--"):
        if args[0] == "--timeout":
            timeout, args = int(args[1]), args[2:]
        elif args[0] == "--initramfs":
            initramfs, args = args[1], args[2:]
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

    collected = bytearray()
    stop = threading.Event()

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
            collected.extend(chunk)
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()

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
    try:
        connection.close()
    except OSError:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
