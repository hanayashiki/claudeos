#!/usr/bin/env python3
"""Digests for the boot-time integrity check in kernel/src/integrity.rs.

Usage:
    checksums.py manifest TREE KERNEL_ELF ITEM...
        Write TREE/etc/claudeos/checksums, one line per ITEM in the form
        shasum prints, and print what was written. ITEM is `kernel`, for the
        kernel's code and read-only data, or a path inside the image. An item
        with nothing to hash is left out with a line saying so: the kernel
        when KERNEL_ELF has not been built, a file when this image does not
        have it.
    checksums.py kernel KERNEL_ELF
        Print the digest of the kernel's checked bytes.
    checksums.py flip-code-byte KERNEL_ELF IMAGE OUT
        Copy IMAGE to OUT with one byte of the kernel's code changed, for the
        test that the check notices. The byte is the last one of .text, past
        the end of the last function, so no instruction changes and the kernel
        still boots. IMAGE is the ELF itself or the flat image made from it.

The kernel's checked bytes run from the symbol __integrity_start to the symbol
__integrity_end. The linker scripts define both, and the kernel hashes the
bytes between them in memory. Here they are read out of the ELF's loadable
segments at the addresses they are loaded at, so both ends hash the same bytes
from the same definition.
"""
import hashlib
import os
import struct
import sys

PT_LOAD = 1
PF_W = 2
SHT_SYMTAB = 2
STT_FUNC = 2


def fail(message):
    print(f"checksums.py: {message}", file=sys.stderr)
    sys.exit(1)


class Elf:
    """The loadable segments and the symbols of a little-endian ELF file,
    32-bit or 64-bit. The x86-64 kernel QEMU boots is converted to ELF32, and
    its addresses are truncated to 32 bits there, symbols and segments alike,
    so they still agree with each other."""

    def __init__(self, path):
        self.path = path
        with open(path, "rb") as handle:
            data = handle.read()
        self.data = data
        if data[:4] != b"\x7fELF":
            fail(f"{path} is not an ELF file")
        if data[5] != 1:
            fail(f"{path} is not little-endian")
        wide = data[4] == 2
        if wide:
            phoff, shoff = struct.unpack_from("<QQ", data, 32)
            phentsize, phnum, shentsize, shnum = struct.unpack_from("<HHHH", data, 54)
        else:
            phoff, shoff = struct.unpack_from("<II", data, 28)
            phentsize, phnum, shentsize, shnum = struct.unpack_from("<HHHH", data, 42)

        # (virtual address, load address, file offset, size in the file, flags)
        self.segments = []
        for index in range(phnum):
            at = phoff + index * phentsize
            if wide:
                kind, flags, offset, vaddr, paddr, filesz, _, _ = struct.unpack_from("<IIQQQQQQ", data, at)
            else:
                kind, offset, vaddr, paddr, filesz, _, flags, _ = struct.unpack_from("<8I", data, at)
            if kind == PT_LOAD:
                self.segments.append((vaddr, paddr, offset, filesz, flags))

        sections = []
        for index in range(shnum):
            at = shoff + index * shentsize
            if wide:
                _, kind, _, _, offset, size, link, _, _, entsize = struct.unpack_from("<IIQQQQIIQQ", data, at)
            else:
                _, kind, _, _, offset, size, link, _, _, entsize = struct.unpack_from("<10I", data, at)
            sections.append((kind, offset, size, link, entsize))

        self.symbols = {}
        # (address, size) of every function.
        self.functions = []
        for kind, offset, size, link, entsize in sections:
            if kind != SHT_SYMTAB:
                continue
            names = sections[link][1]
            for at in range(offset, offset + size, entsize):
                if wide:
                    name, info, _, _, value, length = struct.unpack_from("<IBBHQQ", data, at)
                else:
                    name, value, length, info, _, _ = struct.unpack_from("<IIIBBH", data, at)
                end = data.index(b"\0", names + name)
                self.symbols[data[names + name:end].decode(errors="replace")] = value
                if info & 0xF == STT_FUNC:
                    self.functions.append((value, length))

    def symbol(self, name):
        if name not in self.symbols:
            fail(f"{self.path} has no symbol {name}; the kernel's linker script defines it")
        return self.symbols[name]

    def segment_at(self, address):
        for segment in self.segments:
            vaddr, _, _, filesz, _ = segment
            if vaddr <= address < vaddr + filesz:
                return segment
        return None

    def checked_range(self):
        start = self.symbol("__integrity_start")
        end = self.symbol("__integrity_end")
        if not start < end:
            fail(f"{self.path}: __integrity_start {start:#x} is not below __integrity_end {end:#x}")
        return start, end

    def loaded_bytes(self, start, end):
        """The bytes from `start` to `end` as the loader puts them in memory."""
        out = bytearray()
        address = start
        while address < end:
            segment = self.segment_at(address)
            if segment is None:
                fail(f"{self.path}: {address:#x}, inside the checked range, has no contents in the file")
            vaddr, _, offset, filesz, flags = segment
            if flags & PF_W:
                fail(f"{self.path}: {address:#x}, inside the checked range, is in a writable segment")
            stop = min(end, vaddr + filesz)
            out += self.data[offset + address - vaddr:offset + stop - vaddr]
            address = stop
        return bytes(out)


def kernel_digest(path):
    elf = Elf(path)
    return hashlib.sha1(elf.loaded_bytes(*elf.checked_range())).hexdigest()


def manifest(tree, kernel_elf, items):
    lines = []
    for item in items:
        if item == "kernel":
            if not os.path.isfile(kernel_elf):
                print(f"warning: {kernel_elf} is missing, so the manifest has no kernel line; "
                      "build the kernel before the userland", file=sys.stderr)
                continue
            digest = kernel_digest(kernel_elf)
        elif item.startswith("/"):
            path = os.path.join(tree, item.lstrip("/"))
            if not os.path.isfile(path):
                print(f"note: {item} is not in this image, so the manifest has no line for it",
                      file=sys.stderr)
                continue
            with open(path, "rb") as handle:
                digest = hashlib.sha1(handle.read()).hexdigest()
        else:
            fail(f"{item!r} is neither `kernel` nor an absolute path")
        lines.append(f"{digest}  {item}\n")

    target = os.path.join(tree, "etc", "claudeos", "checksums")
    os.makedirs(os.path.dirname(target), exist_ok=True)
    with open(target, "w") as handle:
        handle.writelines(lines)
    print(f"/etc/claudeos/checksums in {tree}:")
    for line in lines:
        print(f"  {line}", end="")


def flip_code_byte(elf_path, image_path, out_path):
    elf = Elf(elf_path)
    text_start, text_end = elf.symbol("__text_start"), elf.symbol("__text_end")
    start, end = elf.checked_range()
    address = text_end - 1
    code_end = max((value + length for value, length in elf.functions
                    if text_start <= value < text_end), default=text_start)
    if code_end > address:
        fail(f"{elf_path}: .text ends inside a function, so no byte of it can be changed "
             "without changing an instruction")
    if not start <= address < end:
        fail(f"{elf_path}: the end of .text is outside the checked range")

    vaddr, paddr, offset, _, _ = elf.segment_at(address)
    in_elf = offset + address - vaddr
    with open(image_path, "rb") as handle:
        image = bytearray(handle.read())
    if image[:4] == b"\x7fELF":
        position = in_elf
    else:
        # llvm-objcopy -O binary starts the flat image at the lowest load
        # address of anything that has contents in the file.
        base = min(load for _, load, _, filesz, _ in elf.segments if filesz > 0)
        position = paddr + address - vaddr - base
    if position >= len(image) or image[position] != elf.data[in_elf]:
        fail(f"{image_path} does not hold the bytes of {elf_path} where they belong")

    before = image[position]
    image[position] ^= 0xFF
    with open(out_path, "wb") as handle:
        handle.write(image)
    print(f"changed the byte at {address:#x}, offset {position:#x} of {os.path.basename(image_path)}, "
          f"from {before:#04x} to {image[position]:#04x}; functions in .text end at {code_end:#x}")


def main():
    args = sys.argv[1:]
    if len(args) >= 3 and args[0] == "manifest":
        manifest(args[1], args[2], args[3:])
    elif len(args) == 2 and args[0] == "kernel":
        print(kernel_digest(args[1]))
    elif len(args) == 4 and args[0] == "flip-code-byte":
        flip_code_byte(args[1], args[2], args[3])
    else:
        print(__doc__.strip(), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
