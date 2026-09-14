//! What the kernel is told about the machine before it can look for itself.
//!
//! Every loader on every board describes memory, a command line and whatever
//! it placed alongside the kernel in some format of its own. The decoders live
//! under `arch`; this is what they all produce, and it is the only shape the
//! portable half knows. Nothing here points back into loader-owned memory,
//! because that memory is often reachable only through a mapping the kernel
//! drops once it is running on its own tables.

/// A span of physical memory the loader reported.
#[derive(Debug, Clone, Copy)]
pub struct MemRegion {
    pub addr: u64,
    pub len: u64,
    /// Free for the kernel to hand out. Anything else is firmware tables,
    /// device registers or memory the board has claimed.
    pub usable: bool,
}

impl MemRegion {
    pub fn end(&self) -> u64 {
        self.addr + self.len
    }
}

/// A span of physical memory that holds something, whether or not the memory
/// map calls it usable.
#[derive(Debug, Clone, Copy)]
pub struct PhysRange {
    pub start: u64,
    pub end: u64,
}

/// A file the loader placed in memory for the kernel to find, in practice the
/// initial ram disk.
#[derive(Debug, Clone, Copy)]
pub struct Module {
    pub start: u64,
    pub end: u64,
}

impl Module {
    pub fn len(&self) -> usize {
        (self.end - self.start) as usize
    }
}

pub const MAX_REGIONS: usize = 32;
pub const MAX_MODULES: usize = 8;
pub const MAX_RESERVED: usize = 16;
/// The same length Linux accepts on arm64. The Raspberry Pi firmware puts a
/// few hundred bytes of its own settings in front of cmdline.txt, so a shorter
/// limit cuts off the end of the line, which is the part that came from us.
pub const CMDLINE_MAX: usize = 2048;

pub struct BootInfo {
    regions: [MemRegion; MAX_REGIONS],
    region_count: usize,
    modules: [Module; MAX_MODULES],
    module_count: usize,
    reserved: [PhysRange; MAX_RESERVED],
    reserved_count: usize,
    /// The command line copied out of loader memory rather than pointed at.
    cmdline: [u8; CMDLINE_MAX],
    cmdline_len: usize,
}

impl BootInfo {
    pub const fn new() -> BootInfo {
        BootInfo {
            regions: [MemRegion { addr: 0, len: 0, usable: false }; MAX_REGIONS],
            region_count: 0,
            modules: [Module { start: 0, end: 0 }; MAX_MODULES],
            module_count: 0,
            reserved: [PhysRange { start: 0, end: 0 }; MAX_RESERVED],
            reserved_count: 0,
            cmdline: [0; CMDLINE_MAX],
            cmdline_len: 0,
        }
    }

    pub fn add_region(&mut self, addr: u64, len: u64, usable: bool) {
        if self.region_count < MAX_REGIONS {
            self.regions[self.region_count] = MemRegion { addr, len, usable };
            self.region_count += 1;
        }
    }

    pub fn add_module(&mut self, start: u64, end: u64) {
        if end > start && self.module_count < MAX_MODULES {
            self.modules[self.module_count] = Module { start, end };
            self.module_count += 1;
        }
    }

    /// Say that physical memory from `start` to `end` already holds something
    /// the kernel will read, so the frame allocator must not give it away.
    ///
    /// Ranges that touch are joined. When the table is full the new range is
    /// folded into the nearest existing one, which reserves more than was
    /// asked for but never less; loaders put this material in one clump, so
    /// the widening stays local.
    pub fn reserve(&mut self, start: u64, end: u64) {
        if end <= start {
            return;
        }
        for r in &mut self.reserved[..self.reserved_count] {
            if start <= r.end && end >= r.start {
                r.start = r.start.min(start);
                r.end = r.end.max(end);
                return;
            }
        }
        if self.reserved_count < MAX_RESERVED {
            self.reserved[self.reserved_count] = PhysRange { start, end };
            self.reserved_count += 1;
            return;
        }
        let mut nearest = 0usize;
        let mut distance = u64::MAX;
        for (i, r) in self.reserved[..self.reserved_count].iter().enumerate() {
            let d = start.abs_diff(r.start);
            if d < distance {
                distance = d;
                nearest = i;
            }
        }
        self.reserved[nearest].start = self.reserved[nearest].start.min(start);
        self.reserved[nearest].end = self.reserved[nearest].end.max(end);
    }

    pub fn set_cmdline(&mut self, bytes: &[u8]) {
        let len = bytes.len().min(CMDLINE_MAX);
        self.cmdline[..len].copy_from_slice(&bytes[..len]);
        self.cmdline_len = len;
    }

    /// The command line, or the empty string if the loader gave none. A line
    /// that is not valid UTF-8 reads as empty rather than being truncated at
    /// the bad byte, so a mangled handoff cannot half-apply.
    pub fn cmdline(&self) -> &str {
        core::str::from_utf8(&self.cmdline[..self.cmdline_len]).unwrap_or("")
    }

    pub fn regions(&self) -> &[MemRegion] {
        &self.regions[..self.region_count]
    }

    pub fn modules(&self) -> &[Module] {
        &self.modules[..self.module_count]
    }

    pub fn reserved(&self) -> &[PhysRange] {
        &self.reserved[..self.reserved_count]
    }
}
