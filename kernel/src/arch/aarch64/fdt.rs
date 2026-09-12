//! The flattened device tree, read far enough to describe the machine.
//!
//! This is how a board following the Linux AArch64 boot protocol says what it
//! has: the firmware leaves a blob in memory and puts its address in x0. Only
//! four things are taken from it here — where memory is, what the command line
//! says, and where an initial ram disk was placed — because everything else
//! this kernel needs it already knows.
//!
//! Every number in the blob is big-endian, and the strings are in a separate
//! block indexed by offset, so nothing can be overlaid with a struct.

use crate::boot::BootInfo;
use crate::mm::phys_to_virt;

const MAGIC: u32 = 0xD00D_FEED;

const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;

unsafe fn be32(virt: u64) -> u32 {
    u32::from_be(core::ptr::read_unaligned(virt as *const u32))
}

unsafe fn be64(virt: u64) -> u64 {
    u64::from_be(core::ptr::read_unaligned(virt as *const u64))
}

/// A big-endian value of one, two or more cells, of which only the low two
/// carry anything an address can hold.
unsafe fn cells(virt: u64, count: u32) -> u64 {
    match count {
        1 => be32(virt) as u64,
        2 => be64(virt),
        _ => be64(virt + (count as u64 - 2) * 4),
    }
}

unsafe fn cstr(virt: u64) -> &'static [u8] {
    let ptr = virt as *const u8;
    let mut len = 0usize;
    while len < 1024 && *ptr.add(len) != 0 {
        len += 1;
    }
    core::slice::from_raw_parts(ptr, len)
}

#[inline]
fn align4(value: u64) -> u64 {
    (value + 3) & !3
}

/// Is there a device tree at `phys`?
pub fn present(phys: u64) -> bool {
    if phys == 0 || phys & 3 != 0 {
        return false;
    }
    unsafe { be32(phys_to_virt(phys)) == MAGIC }
}

/// Read the blob at `phys` into `info`. Returns false if there is no device
/// tree there, in which case `info` is untouched.
pub fn parse(phys: u64, info: &mut BootInfo) -> bool {
    if !present(phys) {
        return false;
    }
    let base = phys_to_virt(phys);
    unsafe {
        let total_size = be32(base + 4) as u64;
        let struct_offset = be32(base + 8) as u64;
        let strings_offset = be32(base + 12) as u64;
        let reserve_offset = be32(base + 16) as u64;
        let struct_size = be32(base + 36) as u64;

        // The blob itself is read all through start-up, so it has to survive
        // the frame allocator's first pass.
        info.reserve(phys, phys + total_size);

        // Ranges the firmware says are already spoken for.
        let mut entry = base + reserve_offset;
        loop {
            let address = be64(entry);
            let size = be64(entry + 8);
            if size == 0 {
                break;
            }
            info.reserve(address, address + size);
            entry += 16;
        }

        let strings = base + strings_offset;
        let mut cursor = base + struct_offset;
        let end = cursor + struct_size;

        let mut depth = 0usize;
        let mut in_memory = false;
        let mut in_chosen = false;
        // What the specification says to assume when the root does not say.
        let mut address_cells = 2u32;
        let mut size_cells = 1u32;
        // The two ends of the ram disk arrive as separate properties in
        // either order, so they are held until the walk is over.
        let mut initrd_start = 0u64;
        let mut initrd_end = 0u64;

        while cursor + 4 <= end {
            let token = be32(cursor);
            cursor += 4;
            match token {
                BEGIN_NODE => {
                    let name = cstr(cursor);
                    cursor += align4(name.len() as u64 + 1);
                    depth += 1;
                    if depth == 2 {
                        in_memory = name.starts_with(b"memory");
                        in_chosen = name == b"chosen";
                    }
                }
                END_NODE => {
                    if depth == 2 {
                        in_memory = false;
                        in_chosen = false;
                    }
                    depth = depth.saturating_sub(1);
                }
                PROP => {
                    let length = be32(cursor) as u64;
                    let name = cstr(strings + be32(cursor + 4) as u64);
                    let value = cursor + 8;
                    cursor = value + align4(length);

                    if depth == 1 {
                        // The root's cell counts say how wide the addresses
                        // and sizes in its children are.
                        if name == b"#address-cells" {
                            address_cells = be32(value);
                        } else if name == b"#size-cells" {
                            size_cells = be32(value);
                        }
                    } else if in_memory && name == b"reg" {
                        let stride = (address_cells + size_cells) as u64 * 4;
                        let mut at = value;
                        while at + stride <= value + length {
                            let address = cells(at, address_cells);
                            let size = cells(at + address_cells as u64 * 4, size_cells);
                            info.add_region(address, size, true);
                            at += stride;
                        }
                    } else if in_chosen {
                        if name == b"bootargs" {
                            // The property carries its terminator; the command
                            // line does not want it.
                            let text = &core::slice::from_raw_parts(
                                value as *const u8,
                                length as usize,
                            )[..];
                            let text = match text.iter().position(|&b| b == 0) {
                                Some(at) => &text[..at],
                                None => text,
                            };
                            info.set_cmdline(text);
                        } else if name == b"linux,initrd-start" {
                            initrd_start = cells(value, (length / 4) as u32);
                        } else if name == b"linux,initrd-end" {
                            initrd_end = cells(value, (length / 4) as u32);
                        }
                    }
                }
                NOP => {}
                END => break,
                _ => break,
            }
        }

        if initrd_end > initrd_start {
            info.add_module(initrd_start, initrd_end);
        }
    }
    true
}
