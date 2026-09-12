//! The tag list, which is how a board with no device tree says what it has.
//!
//! This is the older of the two ARM handoffs: a chain of records in low
//! memory, each one a length in 32-bit words, a tag, and its contents, ending
//! at a record whose tag is zero. It is what QEMU's emulated Pi 4 leaves
//! behind, because that machine has no device tree to give.
//!
//! Only three tags are read: where memory is, where an initial ram disk was
//! placed, and what the command line says.

use crate::boot::BootInfo;
use crate::mm::phys_to_virt;

const CORE: u32 = 0x5441_0001;
const MEM: u32 = 0x5441_0002;
const INITRD2: u32 = 0x5442_0005;
const CMDLINE: u32 = 0x5441_0009;

/// A tag list is not allowed to run on forever; a corrupt length would
/// otherwise walk the whole address space.
const MAX_TAGS: usize = 64;

unsafe fn rd32(virt: u64) -> u32 {
    core::ptr::read_unaligned(virt as *const u32)
}

/// Is there a tag list at `phys`? The first record has to be the one that says
/// a list is starting.
pub fn present(phys: u64) -> bool {
    if phys == 0 || phys & 3 != 0 {
        return false;
    }
    unsafe { rd32(phys_to_virt(phys) + 4) == CORE }
}

/// Read the list at `phys` into `info`. Returns false if there is no tag list
/// there, in which case `info` is untouched.
pub fn parse(phys: u64, info: &mut BootInfo) -> bool {
    if !present(phys) {
        return false;
    }
    unsafe {
        let mut at = phys_to_virt(phys);
        let start = at;
        for _ in 0..MAX_TAGS {
            let words = rd32(at) as u64;
            let tag = rd32(at + 4);
            if tag == 0 || words < 2 {
                break;
            }
            let body = at + 8;
            match tag {
                MEM => {
                    let size = rd32(body) as u64;
                    let address = rd32(body + 4) as u64;
                    info.add_region(address, size, true);
                }
                INITRD2 => {
                    let address = rd32(body) as u64;
                    let size = rd32(body + 4) as u64;
                    info.add_module(address, address + size);
                }
                CMDLINE => {
                    // The rest of the record is the line, NUL-terminated and
                    // padded out to a whole number of words.
                    let bytes = core::slice::from_raw_parts(
                        body as *const u8,
                        (words - 2) as usize * 4,
                    );
                    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                    info.set_cmdline(&bytes[..end]);
                }
                _ => {}
            }
            at += words * 4;
        }
        info.reserve(phys, phys + (at - start));
    }
    true
}
