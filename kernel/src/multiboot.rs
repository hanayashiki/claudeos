//! Multiboot1 boot information, decoded into owned structs.
//!
//! The loader-supplied blob is packed, so every field is read with
//! `read_unaligned` at an explicit offset rather than overlaid with a struct.

pub const MULTIBOOT_BOOTLOADER_MAGIC: u32 = 0x2BADB002;

pub const MEMORY_AVAILABLE: u32 = 1;

#[derive(Debug, Clone, Copy)]
pub struct MemRegion {
    pub addr: u64,
    pub len: u64,
    pub typ: u32,
}

impl MemRegion {
    pub fn end(&self) -> u64 {
        self.addr + self.len
    }
    pub fn is_usable(&self) -> bool {
        self.typ == MEMORY_AVAILABLE
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Module {
    pub start: u64,
    pub end: u64,
    pub cmdline: u64,
}

impl Module {
    pub fn len(&self) -> usize {
        (self.end - self.start) as usize
    }
}

pub const MAX_REGIONS: usize = 32;
pub const MAX_MODULES: usize = 8;

pub struct BootInfo {
    /// Physical address of the multiboot info blob itself.
    pub info_phys: u64,
    pub flags: u32,
    pub mem_lower_kb: u32,
    pub mem_upper_kb: u32,
    pub regions: [MemRegion; MAX_REGIONS],
    pub region_count: usize,
    pub modules: [Module; MAX_MODULES],
    pub module_count: usize,
    pub cmdline_phys: u64,
    /// Physical end of everything the loader placed in memory (info blob,
    /// module contents, command lines). The frame allocator must not hand
    /// any of this out before it has been consumed.
    pub reserved_end: u64,
}

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
    let flags = rd32(phys);

    let zero_region = MemRegion { addr: 0, len: 0, typ: 0 };
    let zero_module = Module { start: 0, end: 0, cmdline: 0 };
    let mut info = BootInfo {
        info_phys: phys,
        flags,
        mem_lower_kb: 0,
        mem_upper_kb: 0,
        regions: [zero_region; MAX_REGIONS],
        region_count: 0,
        modules: [zero_module; MAX_MODULES],
        module_count: 0,
        cmdline_phys: 0,
        reserved_end: phys + 128,
    };

    if flags & (1 << 0) != 0 {
        info.mem_lower_kb = rd32(phys + 4);
        info.mem_upper_kb = rd32(phys + 8);
    }

    if flags & (1 << 2) != 0 {
        let cmdline = rd32(phys + 16) as u64;
        info.cmdline_phys = cmdline;
        if cmdline != 0 {
            info.reserved_end = info.reserved_end.max(cmdline + cstr_len(cmdline) as u64 + 1);
        }
    }

    // Modules: mods_count at +20, mods_addr at +24.
    if flags & (1 << 3) != 0 {
        let count = rd32(phys + 20) as usize;
        let addr = rd32(phys + 24) as u64;
        info.reserved_end = info.reserved_end.max(addr + (count as u64) * 16);
        for i in 0..count.min(MAX_MODULES) {
            let base = addr + (i as u64) * 16;
            let m = Module {
                start: rd32(base) as u64,
                end: rd32(base + 4) as u64,
                cmdline: rd32(base + 8) as u64,
            };
            info.reserved_end = info.reserved_end.max(m.end);
            if m.cmdline != 0 {
                info.reserved_end =
                    info.reserved_end.max(m.cmdline + cstr_len(m.cmdline) as u64 + 1);
            }
            info.modules[i] = m;
        }
        info.module_count = count.min(MAX_MODULES);
    }

    // Memory map: mmap_length at +44, mmap_addr at +48.
    // Each entry is: u32 size; u64 addr; u64 len; u32 type, with `size` not
    // counting itself.
    if flags & (1 << 6) != 0 {
        let len = rd32(phys + 44) as u64;
        let addr = rd32(phys + 48) as u64;
        info.reserved_end = info.reserved_end.max(addr + len);
        let mut cur = addr;
        let end = addr + len;
        while cur + 4 <= end && info.region_count < MAX_REGIONS {
            let size = rd32(cur) as u64;
            if size < 20 {
                break;
            }
            info.regions[info.region_count] = MemRegion {
                addr: rd64(cur + 4),
                len: rd64(cur + 12),
                typ: rd32(cur + 20),
            };
            info.region_count += 1;
            cur += size + 4;
        }
    }

    // Fall back to the basic mem_lower/mem_upper report if no E820 map came in.
    if info.region_count == 0 && flags & (1 << 0) != 0 {
        info.regions[0] =
            MemRegion { addr: 0, len: (info.mem_lower_kb as u64) * 1024, typ: MEMORY_AVAILABLE };
        info.regions[1] = MemRegion {
            addr: 0x100000,
            len: (info.mem_upper_kb as u64) * 1024,
            typ: MEMORY_AVAILABLE,
        };
        info.region_count = 2;
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

/// # Safety
/// `addr` must point at a NUL-terminated string in mapped memory.
pub unsafe fn cstr_at(addr: u64) -> Option<&'static str> {
    if addr == 0 {
        return None;
    }
    let len = cstr_len(addr);
    core::str::from_utf8(core::slice::from_raw_parts(addr as *const u8, len)).ok()
}
