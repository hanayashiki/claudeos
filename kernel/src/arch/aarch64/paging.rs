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
use crate::mm::{phys_to_virt, HHDM_BASE, PAGE_SIZE_U64};
use crate::sync::NoInterrupts;
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
fn encode(phys: u64, flags: u64) -> Entry {
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
    Entry::new(entry)
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
    //
    // The leading barrier is what makes whatever was written into this table
    // visible to the walkers before they are pointed at it; without it they
    // are entitled to read what was there before. Nothing is left of the
    // previous space afterwards, because no address space identifiers are in
    // use and every entry still cached belongs to it.
    asm!(
        "dsb ishst",
        "msr ttbr0_el1, {table}",
        "msr ttbr1_el1, {table}",
        "isb",
        "tlbi vmalle1is",
        "dsb ish",
        "isb",
        table = in(reg) phys,
    );
}

/// The operand `tlbi` by address takes: bits 55 to 12 of the address in the
/// low forty-four bits, and a hint in bits 47 to 44 saying which level of the
/// walk the entry came from. The mask is what keeps the address out of the
/// hint. An address in the kernel's half is sign-extended, so shifting alone
/// would leave its bits 59 to 56 sitting in that field, all ones, claiming a
/// level that does not match this granule. The Pi 4's core does not implement
/// the field and ignores it; on a part that does, the invalidation is then
/// permitted not to happen.
#[inline]
pub const fn invalidation_operand(virt: u64) -> u64 {
    (virt >> 12) & 0xFFF_FFFF_FFFF
}

#[inline]
pub fn flush_tlb(virt: u64) {
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vaae1is, {page}",
            "dsb ish",
            "isb",
            page = in(reg) invalidation_operand(virt),
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

/// One descriptor in a translation table.
///
/// The hardware that translates addresses walks these tables itself, so a
/// descriptor is memory a second observer reads, and this one is allowed to
/// read it speculatively, without any instruction naming the address it
/// covers. A store to one therefore has to be ordered against whatever the
/// descriptor makes reachable. `store` is the only way to write one and emits
/// that ordering, so it cannot be left out: a table is an array of these and
/// nothing hands out the word inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct Entry(u64);

impl Entry {
    pub const EMPTY: Entry = Entry(0);

    #[inline]
    pub const fn new(bits: u64) -> Entry {
        Entry(bits)
    }

    #[inline]
    pub const fn bits(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn is_present(self) -> bool {
        self.0 & PRESENT != 0
    }

    /// The output address the descriptor names.
    #[inline]
    pub const fn addr(self) -> u64 {
        self.0 & ADDR_MASK
    }

    /// The flags as they were asked for, software bits included.
    #[inline]
    pub const fn flags(self) -> u64 {
        self.0 & FLAG_MASK
    }

    /// Write this descriptor where the walker will read it.
    ///
    /// The barrier is what puts everything the descriptor makes reachable --
    /// a table that was just zeroed, a frame that was just filled in -- in
    /// memory before the descriptor naming it is there. Without it the walker
    /// can reach the descriptor and read what was at that address before.
    /// QEMU does not model this; a board does.
    #[inline]
    pub unsafe fn store(self, at: *mut Entry) {
        asm!("dsb ishst", options(nostack, preserves_flags));
        core::ptr::write(at, self);
    }
}

/// Put a fresh table under `at` and link it there.
///
/// The three steps belong together and are here and nowhere else: the frame is
/// zeroed, the zeroing is made visible to the walker, and only then does a
/// descriptor name it. In any other order, or with anything in between, the
/// walker can reach the descriptor and read whatever the frame held before it
/// was cleared. The descriptor above holds the table's reference from here on;
/// `free_table` takes it back.
unsafe fn publish_table(at: *mut Entry) -> Result<u64, MapError> {
    let table = frame::alloc_zeroed().ok_or(MapError::OutOfMemory)?.into_recorded();
    Entry::new(table | PRESENT | PAGE_DESCRIPTOR).store(at);
    Ok(table)
}

#[inline]
unsafe fn table_at(phys: u64) -> *mut Entry {
    phys_to_virt(phys) as *mut Entry
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
                (*src.add(i)).store(dst.add(i));
            }
        }
        Some(AddressSpace { root })
    }

    pub unsafe fn switch_to(&self) {
        write_ttbr(self.root);
    }

    /// Walk to the last-level descriptor for `virt`, allocating tables if
    /// asked.
    ///
    /// Nothing is said about permissions on the way down. A table descriptor
    /// can forbid what the pages under it allow, and using that would mean
    /// revisiting every ancestor whenever one page's permissions changed, so
    /// the tables are left permissive and the last level decides.
    unsafe fn entry_for(&self, virt: u64, create: bool) -> Result<*mut Entry, MapError> {
        let mut table = self.root;
        for level in (1..4).rev() {
            let idx = index_of(virt, level);
            let entry_ptr = table_at(table).add(idx);
            let entry = *entry_ptr;
            if !entry.is_present() {
                if !create {
                    return Err(MapError::OutOfMemory);
                }
                table = publish_table(entry_ptr)?;
            } else {
                table = entry.addr();
            }
        }
        Ok(table_at(table).add(index_of(virt, 0)))
    }

    /// Map `frame` at `virt`. The descriptor holds the reference from here on,
    /// and `unmap` or teardown gives it back.
    ///
    /// An address that already has a mapping is refused rather than replaced.
    /// The descriptor is the only record of the reference the frame it names
    /// holds, so writing over it would leave that frame with no owner and no
    /// way back to the allocator.
    ///
    /// Private. The one caller outside this file was the second half of a
    /// change to a live address space, with `unmap` as the first, and between
    /// the two the address had nothing at it -- a state everything else in the
    /// kernel reads as a page that has never been touched. From outside, a
    /// page that was not there goes in through `map_new` and a page that
    /// stands in for one that was goes in through `replace`, which is one
    /// store and so cannot be split.
    fn map(&self, virt: u64, frame: Frame, flags: u64) -> Result<(), MapError> {
        unsafe {
            let entry = self.entry_for(virt, true)?;
            if (*entry).is_present() {
                return Err(MapError::AlreadyMapped);
            }
            encode(frame.into_recorded(), flags).store(entry);
        }
        flush_tlb(virt);
        Ok(())
    }

    /// Map a physical address the kernel does not own a reference to, in
    /// practice a device's registers.
    pub fn map_fixed(&self, virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
        unsafe {
            let entry = self.entry_for(virt, true)?;
            if (*entry).is_present() {
                return Err(MapError::AlreadyMapped);
            }
            encode(phys, flags).store(entry);
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

    /// Print the walk of `virt` through this hierarchy, descriptor by
    /// descriptor.
    ///
    /// A fault report that says a page is present and writable is a summary of
    /// the last descriptor only. When that descriptor looks right and the
    /// access faulted anyway, what is wanted is every descriptor the walker
    /// actually reads, out of the tables the machine is pointed at rather than
    /// the ones a task is recorded on.
    ///
    /// Only the ones that end the walk are named by their permissions. The
    /// bits that say read-only and reachable from the level below mean nothing
    /// in a descriptor that points at another table, so printing them there
    /// would be reporting a permission the walk does not have.
    pub fn dump_walk(&self, virt: u64) {
        crate::println!("  walk of {:#018x} through {:#x}:", virt, self.root);
        unsafe {
            let mut table = self.root;
            for level in (0..4).rev() {
                let idx = index_of(virt, level);
                let entry = *table_at(table).add(idx);
                if !entry.is_present() {
                    crate::println!(
                        "    level {} index {:3} = {:#018x} absent",
                        level + 1,
                        idx,
                        entry.bits(),
                    );
                    return;
                }
                let block = level > 0 && entry.bits() & PAGE_DESCRIPTOR == 0;
                if !block && level > 0 {
                    crate::println!(
                        "    level {} index {:3} = {:#018x} present, a table",
                        level + 1,
                        idx,
                        entry.bits(),
                    );
                    table = entry.addr();
                    continue;
                }
                crate::println!(
                    "    level {} index {:3} = {:#018x} present{}{}{}{}",
                    level + 1,
                    idx,
                    entry.bits(),
                    if entry.bits() & READ_ONLY != 0 { " read-only" } else { " writable" },
                    if entry.bits() & USER != 0 { " user" } else { " supervisor" },
                    if entry.bits() & COW != 0 { " copy-on-write" } else { "" },
                    if block { ", a block" } else { "" },
                );
                return;
            }
        }
    }

    /// Put `frame` at `virt` in place of what is mapped there, handing back the
    /// reference the old descriptor held. Dropping the result releases it.
    ///
    /// One store, because the two-step form is wrong and reads as if it were
    /// not. Taking the old mapping away and putting the new one in as two
    /// calls leaves the address with nothing at it in between, and a sibling
    /// task on this address space that touches it there does not find a page
    /// that is on its way back: it finds one that was never there, and the
    /// fault handler gives it a fresh zero page over the top. The contents are
    /// gone and the mapping that was coming in is then refused as well. A
    /// fault from the level below is handled here with interrupts in the state
    /// the faulting code was in, so on this machine the sibling needs nothing
    /// unusual to get in.
    ///
    /// The token says nothing else runs between reading the old descriptor and
    /// writing the new one, which is what stops the frame this hands back from
    /// being released twice.
    ///
    /// `None` when nothing was mapped at `virt`: nothing is written and
    /// `frame` is released, because putting it in would be creating a mapping
    /// rather than replacing one.
    #[must_use = "dropping the frame is what releases it"]
    pub fn replace(
        &self,
        virt: u64,
        frame: Frame,
        flags: u64,
        _irq: NoInterrupts,
    ) -> Option<Frame> {
        let old = unsafe {
            let entry = self.entry_for(virt, false).ok()?;
            if !(*entry).is_present() {
                return None;
            }
            let old = (*entry).addr();
            encode(frame.into_recorded(), flags).store(entry);
            old
        };
        flush_tlb(virt);
        Some(unsafe { Frame::from_recorded(old) })
    }

    /// Take the mapping away, handing back the reference it held.
    pub fn unmap(&self, virt: u64) -> Option<Frame> {
        let frame = unsafe {
            let entry = self.entry_for(virt, false).ok()?;
            if !(*entry).is_present() {
                return None;
            }
            let phys = (*entry).addr();
            Entry::EMPTY.store(entry);
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
                if !entry.is_present() {
                    return None;
                }
                // A block descriptor at this level ends the walk.
                if entry.bits() & PAGE_DESCRIPTOR == 0 {
                    let size = 1u64 << (12 + 9 * level);
                    return Some(entry.addr() + (virt & (size - 1)));
                }
                table = entry.addr();
            }
            let entry = *table_at(table).add(index_of(virt, 0));
            if !entry.is_present() {
                return None;
            }
            Some(entry.addr() + (virt & 0xFFF))
        }
    }

    /// The flags on the mapping of `virt`, as they were asked for.
    pub fn flags_of(&self, virt: u64) -> Option<u64> {
        unsafe {
            let entry = self.entry_for(virt, false).ok()?;
            if !(*entry).is_present() {
                return None;
            }
            Some((*entry).flags())
        }
    }

    /// Change the flags on an existing mapping, leaving the frame alone.
    pub fn set_flags(&self, virt: u64, flags: u64) -> Option<()> {
        unsafe {
            let entry = self.entry_for(virt, false).ok()?;
            if !(*entry).is_present() {
                return None;
            }
            encode((*entry).addr(), flags).store(entry);
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
    ///
    /// The whole user half is detached first and the detachment made visible
    /// before anything under it is handed back. A frame released while a
    /// mapping to it still exists can be given to another address space and
    /// written through the old one, and here the walker is allowed to act on
    /// such an entry speculatively, without any instruction naming that
    /// address.
    pub fn free_user_memory(&self) {
        // Entries 0..256 are the low half: everything a program owns.
        let mut detached = [Entry::EMPTY; 256];
        unsafe {
            let root = table_at(self.root);
            for (i, entry) in detached.iter_mut().enumerate() {
                *entry = *root.add(i);
                Entry::EMPTY.store(root.add(i));
            }
        }
        flush_tlb_all();
        for entry in detached {
            if entry.is_present() {
                unsafe { free_table(entry.addr(), 3) };
            }
        }
    }

    /// Give this address space the same user mappings `src` has, shared and
    /// read-only so that the first write to either copy makes its own frame.
    ///
    /// Each page is invalidated where its permission changes, so a walk that
    /// stops partway leaves nothing behind that a flush here would have to
    /// clean up. What this address space has collected by then is the
    /// caller's to release.
    pub fn clone_user_from(&self, src: &AddressSpace) -> Result<(), MapError> {
        unsafe { self.share_user_tables(src) }
    }

    unsafe fn share_user_tables(&self, src: &AddressSpace) -> Result<(), MapError> {
        let from = table_at(src.root);
        for i in 0..256usize {
            let entry = *from.add(i);
            if !entry.is_present() {
                continue;
            }
            let virt = (i as u64) << 39;
            clone_table(self, src, entry.addr(), virt, 3)?;
        }
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
        if !entry.is_present() {
            continue;
        }
        if level == 1 {
            drop(Frame::from_recorded(entry.addr()));
        } else {
            free_table(entry.addr(), level - 1);
        }
        Entry::EMPTY.store(table.add(i));
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
        if !entry.is_present() {
            continue;
        }
        let virt = base | (i as u64) << (12 + 9 * (level - 1));
        if level == 1 {
            let mut flags = entry.flags();
            if flags & WRITABLE != 0 {
                flags |= COW;
                // The page the parent is still running on has to lose write
                // permission too, or its writes would be seen by the child.
                // The walk runs on the parent's tables, so the translation
                // this contradicts is in the processor's cache of them right
                // now: a sibling's store goes through it, into a page the
                // child is about to share, until it is thrown away. That is
                // why the invalidation is here and not at the end of the walk.
                encode(entry.addr(), flags).store(entry_ptr);
                flush_tlb(virt);
            }
            let shared = frame::share_recorded(entry.addr());
            dst.map(virt, shared, flags)?;
        } else {
            clone_table(dst, src, entry.addr(), virt, level - 1)?;
        }
    }
    Ok(())
}

/// Drop the boot-time identity map of the low 4 GiB now that the kernel runs
/// entirely out of the higher half. This frees the low half for programs.
pub unsafe fn drop_identity_map() {
    let root = table_at(read_ttbr0());
    Entry::EMPTY.store(root.add(0));
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
