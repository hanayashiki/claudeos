#!/usr/bin/env python3
"""Build a cpio 'newc' archive from a directory tree.

Usage: mkcpio.py <source-dir> <output.cpio>
"""
import os
import stat
import sys

MAGIC = b"070701"
TRAILER = "TRAILER!!!"


def field(value):
    return b"%08X" % (value & 0xFFFFFFFF)


def pad4(stream):
    remainder = stream.tell() % 4
    if remainder:
        stream.write(b"\0" * (4 - remainder))


def write_entry(out, name, mode, data, ino, nlink=1):
    out.write(MAGIC)
    for value in (ino, mode, 0, 0, nlink, 0, len(data), 0, 0, 0, 0, len(name) + 1, 0):
        out.write(field(value))
    out.write(name.encode() + b"\0")
    pad4(out)
    if data:
        out.write(data)
        pad4(out)


def main():
    if len(sys.argv) != 3:
        print(__doc__.strip(), file=sys.stderr)
        return 1
    source, target = sys.argv[1], sys.argv[2]
    ino = 1
    entries = []

    for root, dirs, files in os.walk(source):
        dirs.sort()
        files.sort()
        for name in dirs + files:
            full = os.path.join(root, name)
            rel = os.path.relpath(full, source)
            info = os.lstat(full)
            if stat.S_ISDIR(info.st_mode):
                entries.append((rel, stat.S_IFDIR | 0o755, b""))
            elif stat.S_ISLNK(info.st_mode):
                entries.append((rel, stat.S_IFLNK | 0o777, os.readlink(full).encode()))
            elif stat.S_ISREG(info.st_mode):
                with open(full, "rb") as handle:
                    data = handle.read()
                mode = stat.S_IFREG | (0o755 if info.st_mode & 0o111 else 0o644)
                entries.append((rel, mode, data))

    entries.sort(key=lambda e: e[0])
    with open(target, "wb") as out:
        for rel, mode, data in entries:
            write_entry(out, rel, mode, data, ino)
            ino += 1
        write_entry(out, TRAILER, 0, b"", 0)

    total = sum(len(data) for _, _, data in entries)
    print(f"{target}: {len(entries)} entries, {total} bytes of file data")
    return 0


if __name__ == "__main__":
    sys.exit(main())
