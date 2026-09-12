//! Multiboot1 boot information, decoded into `crate::boot::BootInfo`.
//!
//! The loader-supplied blob is packed, so every field is read with
//! `read_unaligned` at an explicit offset rather than overlaid with a struct.
//!
//! Everything the blob points at is read here, while the low memory it lives
//! in is still identity mapped, and copied into the result. The kernel drops
//! that mapping once it is running on its own tables.

use crate::boot::BootInfo;

pub const MULTIBOOT_BOOTLOADER_MAGIC: u32 = 0x2BADB002;

const MEMORY_AVAILABLE: u32 = 1;

/// How much of the info blob itself to keep back. The fixed part is 116 bytes;
/// this rounds up past anything a loader might append to it.
const INFO_BLOB_BYTES: u64 = 128;

unsafe fn rd32(addr: u64) -> u32 {
    core::ptr::read_unaligned(addr as *const u32)
}

unsafe fn rd64(addr: u64) -> u64 {
    core::ptr::read_unaligned(addr as *const u64)
}

/// # Safety
/// `phys` must be the loader-supplied multiboot info pointer, and low physical
/// memory must still be identity mapped.
pub unsafe fn parse(phys: u64) -> BootInfo {
    let mut info = BootInfo::new();
    let flags = rd32(phys);
    info.reserve(phys, phys + INFO_BLOB_BYTES);

    if flags & (1 << 2) != 0 {
        let cmdline = rd32(phys + 16) as u64;
        if cmdline != 0 {
            let len = cstr_len(cmdline);
            let text = core::slice::from_raw_parts(cmdline as *const u8, len);
            // Multiboot's command line starts with the path the loader was
            // told to load, which is not an argument to anything. No other
            // handoff includes it, so it is dropped here rather than being
            // skipped over by everything that reads the line.
            let arguments = match text.iter().position(|&b| b == b' ') {
                Some(space) => &text[space + 1..],
                None => &[],
            };
            info.set_cmdline(arguments);
            info.reserve(cmdline, cmdline + len as u64 + 1);
        }
    }

    // Modules: mods_count at +20, mods_addr at +24.
    if flags & (1 << 3) != 0 {
        let count = rd32(phys + 20) as usize;
        let addr = rd32(phys + 24) as u64;
        info.reserve(addr, addr + (count as u64) * 16);
        for i in 0..count.min(crate::boot::MAX_MODULES) {
            let base = addr + (i as u64) * 16;
            info.add_module(rd32(base) as u64, rd32(base + 4) as u64);
            let cmdline = rd32(base + 8) as u64;
            if cmdline != 0 {
                info.reserve(cmdline, cmdline + cstr_len(cmdline) as u64 + 1);
            }
        }
    }

    // Memory map: mmap_length at +44, mmap_addr at +48.
    // Each entry is: u32 size; u64 addr; u64 len; u32 type, with `size` not
    // counting itself.
    let mut mapped = false;
    if flags & (1 << 6) != 0 {
        let len = rd32(phys + 44) as u64;
        let addr = rd32(phys + 48) as u64;
        info.reserve(addr, addr + len);
        let mut cur = addr;
        let end = addr + len;
        while cur + 4 <= end {
            let size = rd32(cur) as u64;
            if size < 20 {
                break;
            }
            info.add_region(rd64(cur + 4), rd64(cur + 12), rd32(cur + 20) == MEMORY_AVAILABLE);
            mapped = true;
            cur += size + 4;
        }
    }

    // Fall back to the basic mem_lower/mem_upper report if no E820 map came in.
    if !mapped && flags & (1 << 0) != 0 {
        info.add_region(0, (rd32(phys + 4) as u64) * 1024, true);
        info.add_region(0x100000, (rd32(phys + 8) as u64) * 1024, true);
    }

    info
}

unsafe fn cstr_len(addr: u64) -> usize {
    let ptr = addr as *const u8;
    let mut len = 0usize;
    while len < 4096 && *ptr.add(len) != 0 {
        len += 1;
    }
    len
}
