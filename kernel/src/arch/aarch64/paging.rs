//! 4-level AArch64 translation tables, 4 KiB granule, 48-bit addresses.
//!
//! Tables are always reached through the direct map, so no recursive mapping
//! or temporary windows are needed.
//!
//! The descriptor bits are not the ones x86 uses, and two of them do not exist
//! at all. Write permission is expressed the other way round here — bit 7 set
//! means read-only — and whether a page may be executed is two bits, one per
//! privilege level. So the flags the portable half passes are kept in the
//! descriptor as software bits, in the positions the architecture leaves for
//! an operating system, and the hardware bits are derived from them on every
//! write. Reading them back therefore gives the same word that was asked for.

use crate::mm::frame::{self, Frame};
use crate::mm::{page_align_down, page_align_up, phys_to_virt, HHDM_BASE, PAGE_SIZE_U64};
use core::arch::asm;

/// The descriptor is valid. At the last level a page descriptor also needs
/// bit 1, which `encode` adds.
pub const PRESENT: u64 = 1 << 0;
/// Software bit: the page may be written. Bits 55..58 are reserved for the
/// operating system's own use.
pub const WRITABLE: u64 = 1 << 55;
/// AP[1]: the level below may reach this page.
pub const USER: u64 = 1 << 6;
/// The low bit of the attribute index, which selects slot one of MAIR_EL1:
/// device memory, which is not cached, reordered or merged.
pub const NO_CACHE: u64 = 1 << 2;
/// UXN. `encode` sets the matching privileged bit alongside it.
pub const NO_EXECUTE: u64 = 1 << 54;
/// Software bit: the page is shared with another address space and must be
/// copied before it is written to.
pub const COW: u64 = 1 << 56;

/// The output address of a descriptor.
pub const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// Bit 1 of a last-level descriptor, which says page rather than block.
const PAGE_DESCRIPTOR: u64 = 1 << 1;
/// The access flag. Hardware faults on any access to a page that has it clear,
/// which is how a system tracks what has been touched. Nothing here does, so
/// every mapping sets it.
const ACCESSED: u64 = 1 << 10;
/// Inner shareable, which is what memory wants on a machine whose cores must
/// see each other's writes.
const SHARED: u64 = 3 << 8;
/// AP[2]: the page may be read but not written.
const READ_ONLY: u64 = 1 << 7;
/// Privileged execute never.
const NO_EXECUTE_EL1: u64 = 1 << 53;
/// The bits of a descriptor `flags_of` hands back and `set_flags` accepts:
/// everything except the output address.
const FLAG_MASK: u64 = !ADDR_MASK;

/// Turn the flags the portable half speaks into a last-level descriptor for
/// `phys`. The software bits are kept as they were asked for; the hardware
/// permission bits are derived from them.
fn encode(phys: u64, flags: u64) -> u64 {
    let mut entry = (phys & ADDR_MASK) | (flags & FLAG_MASK) | PAGE_DESCRIPTOR | ACCESSED | SHARED;
    if flags & WRITABLE == 0 || flags & COW != 0 {
        entry |= READ_ONLY;
    } else {
        entry &= !READ_ONLY;
    }
    // Nothing the kernel maps for a program is ever executed at EL1, and
    // anything the portable half calls non-executable is non-executable at
    // both levels.
    if flags & (NO_EXECUTE | USER) != 0 {
        entry |= NO_EXECUTE_EL1;
    }
    entry
}

#[inline]
pub fn read_ttbr0() -> u64 {
    let value: u64;
    unsafe { asm!("mrs {}, ttbr0_el1", out(reg) value, options(nomem, nostack)) };
    value & ADDR_MASK
}

#[inline]
pub unsafe fn write_ttbr(phys: u64) {
    // One table serves both bases: the walk takes the same nine bits for the
    // top level whichever register it came from, and the kernel's addresses
    // all land in the upper half of it.
    asm!(
        "msr ttbr0_el1, {table}",
        "msr ttbr1_el1, {table}",
        "isb",
        "tlbi vmalle1is",
        "dsb ish",
        "isb",
        table = in(reg) phys,
    );
}

#[inline]
pub fn flush_tlb(virt: u64) {
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vaae1is, {page}",
            "dsb ish",
            "isb",
            page = in(reg) virt >> 12,
        );
    }
}

#[inline]
pub fn flush_tlb_all() {
    unsafe {
        asm!("dsb ishst", "tlbi vmalle1is", "dsb ish", "isb");
    }
}

#[inline]
fn index_of(virt: u64, level: u32) -> usize {
    ((virt >> (12 + 9 * level)) & 0x1FF) as usize
}

#[inline]
unsafe fn table_at(phys: u64) -> *mut u64 {
    phys_to_virt(phys) as *mut u64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    OutOfMemory,
    AlreadyMapped,
}

/// A translation table hierarchy, identified by the physical address of its
/// top-level table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressSpace {
    pub root: u64,
}

impl AddressSpace {
    /// A number that tells this address space apart from every other one alive
    /// at the same moment, for anything that has to key on which one it is.
    pub fn id(&self) -> u64 {
        self.root
    }

    pub fn current() -> AddressSpace {
        AddressSpace { root: read_ttbr0() }
    }

    /// Create a fresh address space that shares the kernel half.
    pub fn new_user() -> Option<AddressSpace> {
        // The space owns its top table; `destroy` takes the reference back.
        let root = frame::alloc_zeroed()?.into_recorded();
        let kernel = Self::current();
        unsafe {
            let src = table_at(kernel.root);
            let dst = table_at(root);
            // Entries 256..512 cover the kernel: direct map, heap, image.
            for i in 256..512 {
                *dst.add(i) = *src.add(i);
            }
        }
        Some(AddressSpace { root })
    }

    pub unsafe fn switch_to(&self) {
        write_ttbr(self.root);
    }

    /// Walk to the last-level descriptor for `virt`, allocating tables if
    /// asked.
    unsafe fn entry_for(
        &self,
        virt: u64,
        create: bool,
        parent_flags: u64,
    ) -> Result<*mut u64, MapError> {
        let mut table = self.root;
        for level in (1..4).rev() {
            let idx = index_of(virt, level);
            let entry_ptr = table_at(table).add(idx);
            let entry = *entry_ptr;
            if entry & PRESENT == 0 {
                if !create {
                    return Err(MapError::OutOfMemory);
                }
                // The entry above it holds the table's reference from here
                // on; `free_table` takes it back.
                let new = frame::alloc_zeroed().ok_or(MapError::OutOfMemory)?.into_recorded();
                *entry_ptr = new | PRESENT | PAGE_DESCRIPTOR;
                table = new;
            } else {
                table = entry & ADDR_MASK;
            }
            // A table descriptor can forbid what the pages under it allow, so
            // it is left permissive and the last level decides.
            let _ = parent_flags;
        }
        Ok(table_at(table).add(index_of(virt, 0)))
    }

    pub fn map(&self, virt: u64, frame: Frame, flags: u64) -> Result<(), MapError> {
        unsafe {
            let entry = self.entry_for(virt, true, flags)?;
            if *entry & PRESENT != 0 {
                return Err(MapError::AlreadyMapped);
            }
            *entry = encode(frame.into_recorded(), flags);
        }
        flush_tlb(virt);
        Ok(())
    }

    /// Map a physical address the kernel does not own a reference to, in
    /// practice a device's registers.
    pub fn map_fixed(&self, virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
        unsafe {
            let entry = self.entry_for(virt, true, flags)?;
            if *entry & PRESENT != 0 {
                return Err(MapError::AlreadyMapped);
            }
            *entry = encode(phys, flags);
        }
        flush_tlb(virt);
        Ok(())
    }

    /// Map a fresh zeroed frame at `virt`, giving back its physical address.
    pub fn map_new(&self, virt: u64, flags: u64) -> Result<u64, MapError> {
        let frame = frame::alloc_zeroed().ok_or(MapError::OutOfMemory)?;
        let phys = frame.addr();
        self.map(virt, frame, flags)?;
        Ok(phys)
    }

    /// Take the mapping away, handing back the reference it held.
    pub fn unmap(&self, virt: u64) -> Option<Frame> {
        let frame = unsafe {
            let entry = self.entry_for(virt, false, 0).ok()?;
            if *entry & PRESENT == 0 {
                return None;
            }
            let phys = *entry & ADDR_MASK;
            *entry = 0;
            Frame::from_recorded(phys)
        };
        flush_tlb(virt);
        Some(frame)
    }

    /// The physical address `virt` reaches, including its offset in the page.
    pub fn translate(&self, virt: u64) -> Option<u64> {
        unsafe {
            let mut table = self.root;
            for level in (1..4).rev() {
                let entry = *table_at(table).add(index_of(virt, level));
                if entry & PRESENT == 0 {
                    return None;
                }
                // A block descriptor at this level ends the walk.
                if entry & PAGE_DESCRIPTOR == 0 {
                    let size = 1u64 << (12 + 9 * level);
                    return Some((entry & ADDR_MASK) + (virt & (size - 1)));
                }
                table = entry & ADDR_MASK;
            }
            let entry = *table_at(table).add(index_of(virt, 0));
            if entry & PRESENT == 0 {
                return None;
            }
            Some((entry & ADDR_MASK) + (virt & 0xFFF))
        }
    }

    /// The flags on the mapping of `virt`, as they were asked for.
    pub fn flags_of(&self, virt: u64) -> Option<u64> {
        unsafe {
            let entry = self.entry_for(virt, false, 0).ok()?;
            if *entry & PRESENT == 0 {
                return None;
            }
            Some(*entry & FLAG_MASK)
        }
    }

    /// Change the flags on an existing mapping, leaving the frame alone.
    pub fn set_flags(&self, virt: u64, flags: u64) -> Option<()> {
        unsafe {
            let entry = self.entry_for(virt, false, 0).ok()?;
            if *entry & PRESENT == 0 {
                return None;
            }
            *entry = encode(*entry & ADDR_MASK, flags);
        }
        flush_tlb(virt);
        Some(())
    }

    /// Map `pages` fresh frames from `virt` up.
    pub fn map_range(&self, virt: u64, pages: usize, flags: u64) -> Result<(), MapError> {
        for page in 0..pages as u64 {
            self.map_new(virt + page * PAGE_SIZE_U64, flags)?;
        }
        Ok(())
    }

    /// Release every user mapping and every table that held one.
    pub fn free_user_memory(&self) {
        unsafe {
            let root = table_at(self.root);
            // Entries 0..256 are the low half: everything a program owns.
            for i in 0..256 {
                let entry = *root.add(i);
                if entry & PRESENT == 0 {
                    continue;
                }
                free_table(entry & ADDR_MASK, 3);
                *root.add(i) = 0;
            }
        }
        flush_tlb_all();
    }

    /// Give this address space the same user mappings `src` has, shared and
    /// read-only so that the first write to either copy makes its own frame.
    pub fn clone_user_from(&self, src: &AddressSpace) -> Result<(), MapError> {
        unsafe {
            let from = table_at(src.root);
            for i in 0..256usize {
                let entry = *from.add(i);
                if entry & PRESENT == 0 {
                    continue;
                }
                let virt = (i as u64) << 39;
                clone_table(self, src, entry & ADDR_MASK, virt, 3)?;
            }
        }
        // The parent's write permissions just changed underneath it.
        flush_tlb_all();
        Ok(())
    }

    /// Release the top table itself. The kernel half is shared, so it is not
    /// freed.
    pub fn destroy(self) {
        self.free_user_memory();
        drop(unsafe { Frame::from_recorded(self.root) });
    }
}

/// Release every frame a table below the top level reaches, then the table.
unsafe fn free_table(phys: u64, level: u32) {
    let table = table_at(phys);
    for i in 0..512 {
        let entry = *table.add(i);
        if entry & PRESENT == 0 {
            continue;
        }
        if level == 1 {
            drop(Frame::from_recorded(entry & ADDR_MASK));
        } else {
            free_table(entry & ADDR_MASK, level - 1);
        }
        *table.add(i) = 0;
    }
    drop(Frame::from_recorded(phys));
}

/// Copy one level of `src`'s user tables into `dst`, sharing the frames at the
/// bottom and marking both sides copy-on-write.
unsafe fn clone_table(
    dst: &AddressSpace,
    src: &AddressSpace,
    phys: u64,
    base: u64,
    level: u32,
) -> Result<(), MapError> {
    let table = table_at(phys);
    for i in 0..512usize {
        let entry_ptr = table.add(i);
        let entry = *entry_ptr;
        if entry & PRESENT == 0 {
            continue;
        }
        let virt = base | (i as u64) << (12 + 9 * (level - 1));
        if level == 1 {
            let mut flags = entry & FLAG_MASK;
            if flags & WRITABLE != 0 {
                flags |= COW;
                // The page the parent is still running on has to lose write
                // permission too, or its writes would be seen by the child.
                *entry_ptr = encode(entry & ADDR_MASK, flags);
            }
            let shared = frame::share_recorded(entry & ADDR_MASK);
            dst.map(virt, shared, flags)?;
        } else {
            clone_table(dst, src, entry & ADDR_MASK, virt, level - 1)?;
        }
    }
    Ok(())
}

/// Drop the boot-time identity map of the low 4 GiB now that the kernel runs
/// entirely out of the higher half. This frees the low half for programs.
pub unsafe fn drop_identity_map() {
    let root = table_at(read_ttbr0());
    *root.add(0) = 0;
    flush_tlb_all();
}

/// Sign-extend a 48-bit virtual address into canonical form.
#[inline]
pub fn sign_extend(virt: u64) -> u64 {
    ((virt << 16) as i64 >> 16) as u64
}

/// True when `virt` is an address the lower half translates, which is the half
/// a program owns.
#[inline]
pub fn is_user_addr(virt: u64) -> bool {
    virt < 0x0000_8000_0000_0000
}

#[inline]
pub fn is_hhdm_addr(virt: u64) -> bool {
    virt >= HHDM_BASE && virt < HHDM_BASE + crate::mm::HHDM_LIMIT
}

/// Keep the helpers the portable half does not call from being warned about;
/// they are named here because this module is part of the contract.
#[allow(dead_code)]
fn _unused(virt: u64) -> u64 {
    page_align_down(virt) + page_align_up(virt)
}
