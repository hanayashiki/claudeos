//! 4-level x86_64 page tables.
//!
//! Tables are always reached through the direct map, so no recursive mapping
//! or temporary windows are needed.

use crate::mm::frame::{self, Frame};
use crate::mm::{page_align_down, page_align_up, phys_to_virt, HHDM_BASE, PAGE_SIZE_U64};
use crate::sync::NoInterrupts;
use core::arch::asm;

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const WRITE_THROUGH: u64 = 1 << 3;
pub const NO_CACHE: u64 = 1 << 4;
pub const ACCESSED: u64 = 1 << 5;
pub const DIRTY: u64 = 1 << 6;
pub const HUGE: u64 = 1 << 7;
pub const GLOBAL: u64 = 1 << 8;
pub const NO_EXECUTE: u64 = 1 << 63;
/// Software bit: the page is shared with another address space and must be
/// copied before it is written to. Bits 9..11 are free for the kernel's use.
pub const COW: u64 = 1 << 9;

/// Bits software may use freely in a non-present entry.
pub const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

#[inline]
pub fn read_cr3() -> u64 {
    let value: u64;
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value & ADDR_MASK
}

#[inline]
pub unsafe fn write_cr3(phys: u64) {
    asm!("mov cr3, {}", in(reg) phys, options(nostack, preserves_flags));
}

#[inline]
pub fn flush_tlb(virt: u64) {
    unsafe { asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags)) };
}

#[inline]
pub fn flush_tlb_all() {
    unsafe { write_cr3(read_cr3()) };
}

#[inline]
fn index_of(virt: u64, level: u32) -> usize {
    ((virt >> (12 + 9 * level)) & 0x1FF) as usize
}

/// One entry in a page table.
///
/// The hardware that translates addresses walks these tables itself, so an
/// entry is memory a second observer reads, and a store to one has to be
/// ordered against whatever the entry makes reachable. `store` is the only way
/// to write one and carries that ordering, so it cannot be left out: a table is
/// an array of these and nothing hands out the word inside.
///
/// This processor orders a walker's reads against its own stores, so there is
/// nothing to emit here. It is spelled this way because the other machine needs
/// a barrier and the two halves keep one shape; writing the word directly is
/// how that barrier gets left out on the machine that needs it.
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

    /// The physical address the entry names.
    #[inline]
    pub const fn addr(self) -> u64 {
        self.0 & ADDR_MASK
    }

    #[inline]
    pub const fn flags(self) -> u64 {
        self.0 & !ADDR_MASK
    }

    /// Write this entry where the walker will read it.
    #[inline]
    pub unsafe fn store(self, at: *mut Entry) {
        core::ptr::write(at, self);
    }
}

/// Put a fresh table under `at` and link it there.
///
/// The three steps belong together and are here and nowhere else: the frame is
/// zeroed, the zeroing is made visible to the walker, and only then does an
/// entry name it. In any other order, or with anything in between, the walker
/// can reach the entry and read whatever the frame held before it was cleared.
/// The entry above holds the table's reference from here on; `free_table`
/// takes it back.
unsafe fn publish_table(at: *mut Entry, flags: u64) -> Result<u64, MapError> {
    let table = frame::alloc_zeroed().ok_or(MapError::OutOfMemory)?.into_recorded();
    Entry::new(table | PRESENT | WRITABLE | (flags & USER)).store(at);
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

/// A page table hierarchy, identified by the physical address of its PML4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressSpace {
    pub pml4: u64,
}

impl AddressSpace {
    /// A number that tells this address space apart from every other one alive
    /// at the same moment, for anything that has to key on which one it is.
    pub fn id(&self) -> u64 {
        self.pml4
    }

    pub fn current() -> AddressSpace {
        AddressSpace { pml4: read_cr3() }
    }

    /// Create a fresh address space that shares the kernel half.
    pub fn new_user() -> Option<AddressSpace> {
        // The space owns its top table; `destroy` takes the reference back.
        let pml4 = frame::alloc_zeroed()?.into_recorded();
        let kernel = Self::current();
        unsafe {
            let src = table_at(kernel.pml4);
            let dst = table_at(pml4);
            // Entries 256..512 cover the kernel: direct map, heap, image.
            for i in 256..512 {
                (*src.add(i)).store(dst.add(i));
            }
        }
        Some(AddressSpace { pml4 })
    }

    pub unsafe fn switch_to(&self) {
        write_cr3(self.pml4);
    }

    /// Walk to the page table entry for `virt`, allocating tables if asked.
    unsafe fn entry_for(
        &self,
        virt: u64,
        create: bool,
        parent_flags: u64,
    ) -> Result<*mut Entry, MapError> {
        let mut table = self.pml4;
        for level in (1..4).rev() {
            let idx = index_of(virt, level);
            let entry_ptr = table_at(table).add(idx);
            let entry = *entry_ptr;
            if !entry.is_present() {
                if !create {
                    return Err(MapError::OutOfMemory);
                }
                table = publish_table(entry_ptr, parent_flags)?;
            } else {
                if entry.bits() & HUGE != 0 {
                    // A large page already covers this address.
                    return Err(MapError::AlreadyMapped);
                }
                // Widen permissions on the way down: a user leaf is
                // unreachable if any parent lacks the user bit.
                if parent_flags & USER != 0 && entry.bits() & USER == 0 {
                    Entry::new(entry.bits() | USER).store(entry_ptr);
                }
                table = entry.addr();
            }
        }
        Ok(table_at(table).add(index_of(virt, 0)))
    }

    /// Map `frame` at `virt`. The entry holds the reference from here on, and
    /// `unmap` or teardown gives it back. A mapping that fails releases it.
    ///
    /// An address that already has a mapping is refused rather than replaced.
    /// The entry is the only record of the reference the frame it names holds,
    /// so writing over it would leave that frame with no owner and no way back
    /// to the allocator.
    ///
    /// Private. The one caller outside this file was the second half of a
    /// change to a live address space, with `unmap` as the first, and between
    /// the two the address had nothing at it -- a state everything else in the
    /// kernel reads as a page that has never been touched. From outside, a
    /// page that was not there goes in through `map_new` and a page that
    /// stands in for one that was goes in through `replace`, which is one
    /// store and so cannot be split.
    fn map(&self, virt: u64, frame: Frame, flags: u64) -> Result<(), MapError> {
        let virt = page_align_down(virt);
        unsafe {
            let entry = self.entry_for(virt, true, flags)?;
            if (*entry).is_present() {
                return Err(MapError::AlreadyMapped);
            }
            Entry::new((frame.into_recorded() & ADDR_MASK) | flags | PRESENT).store(entry);
        }
        flush_tlb(virt);
        Ok(())
    }

    /// Map memory this address space does not own: the direct map, the kernel
    /// image, device registers. Only the kernel half is mapped this way, and
    /// the kernel half is never torn down.
    pub fn map_fixed(&self, virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
        let virt = page_align_down(virt);
        unsafe {
            let entry = self.entry_for(virt, true, flags)?;
            Entry::new((phys & ADDR_MASK) | flags | PRESENT).store(entry);
        }
        flush_tlb(virt);
        Ok(())
    }

    /// Allocate a frame and map it at `virt`.
    pub fn map_new(&self, virt: u64, flags: u64) -> Result<u64, MapError> {
        let frame = frame::alloc_zeroed().ok_or(MapError::OutOfMemory)?;
        let phys = frame.addr();
        self.map(virt, frame, flags)?;
        Ok(phys)
    }

    /// Print the walk of `virt` through this hierarchy, entry by entry.
    ///
    /// A fault report that says a page is present and writable is a summary of
    /// the last entry only. When that entry looks right and the access faulted
    /// anyway, what is wanted is every entry the walker actually reads, out of
    /// the table the machine is pointed at rather than the one a task is
    /// recorded on.
    pub fn dump_walk(&self, virt: u64) {
        crate::println!("  walk of {:#018x} through {:#x}:", virt, self.pml4);
        unsafe {
            let mut table = self.pml4;
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
                crate::println!(
                    "    level {} index {:3} = {:#018x} present{}{}{}{}",
                    level + 1,
                    idx,
                    entry.bits(),
                    if entry.bits() & WRITABLE != 0 { " writable" } else { " read-only" },
                    if entry.bits() & USER != 0 { " user" } else { " supervisor" },
                    if entry.bits() & HUGE != 0 { " huge" } else { "" },
                    if level == 0 && entry.bits() & COW != 0 { " copy-on-write" } else { "" },
                );
                if entry.bits() & HUGE != 0 {
                    return;
                }
                table = entry.addr();
            }
        }
    }

    /// Put `frame` at `virt` in place of what is mapped there, handing back the
    /// reference the old entry held. Dropping the result releases it.
    ///
    /// One store, because the two-step form is wrong and reads as if it were
    /// not. Taking the old mapping away and putting the new one in as two
    /// calls leaves the address with nothing at it in between, and a sibling
    /// task on this address space that touches it there does not find a page
    /// that is on its way back: it finds one that was never there, and the
    /// fault handler gives it a fresh zero page over the top. The contents are
    /// gone and the mapping that was coming in is then refused as well.
    ///
    /// The token says nothing else runs between reading the old entry and
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
        let virt = page_align_down(virt);
        unsafe {
            let entry = self.entry_for(virt, false, flags).ok()?;
            let old = *entry;
            if !old.is_present() {
                return None;
            }
            Entry::new((frame.into_recorded() & ADDR_MASK) | flags | PRESENT).store(entry);
            flush_tlb(virt);
            Some(Frame::from_recorded(old.addr()))
        }
    }

    /// Take the mapping at `virt` away, handing back the reference the entry
    /// held. Dropping the result releases the frame.
    #[must_use = "dropping the frame is what releases it"]
    pub fn unmap(&self, virt: u64) -> Option<Frame> {
        let virt = page_align_down(virt);
        unsafe {
            let entry = self.entry_for(virt, false, 0).ok()?;
            let value = *entry;
            if !value.is_present() {
                return None;
            }
            Entry::EMPTY.store(entry);
            flush_tlb(virt);
            Some(Frame::from_recorded(value.addr()))
        }
    }

    pub fn translate(&self, virt: u64) -> Option<u64> {
        unsafe {
            let mut table = self.pml4;
            for level in (1..4).rev() {
                let entry = *table_at(table).add(index_of(virt, level));
                if !entry.is_present() {
                    return None;
                }
                if entry.bits() & HUGE != 0 {
                    let page_size = 1u64 << (12 + 9 * level);
                    return Some((entry.addr() & !(page_size - 1)) | (virt & (page_size - 1)));
                }
                table = entry.addr();
            }
            let entry = *table_at(table).add(index_of(virt, 0));
            if !entry.is_present() {
                return None;
            }
            Some(entry.addr() | (virt & 0xFFF))
        }
    }

    pub fn flags_of(&self, virt: u64) -> Option<u64> {
        unsafe {
            let entry = self.entry_for(virt, false, 0).ok()?;
            let value = *entry;
            if !value.is_present() {
                None
            } else {
                Some(value.flags())
            }
        }
    }

    pub fn set_flags(&self, virt: u64, flags: u64) -> Option<()> {
        unsafe {
            let entry = self.entry_for(virt, false, flags).ok()?;
            let value = *entry;
            if !value.is_present() {
                return None;
            }
            Entry::new(value.addr() | flags | PRESENT).store(entry);
        }
        flush_tlb(virt);
        Some(())
    }

    pub fn map_range(
        &self,
        virt: u64,
        phys: u64,
        size: u64,
        flags: u64,
    ) -> Result<(), MapError> {
        let pages = page_align_up(size) / PAGE_SIZE_U64;
        for i in 0..pages {
            self.map_fixed(virt + i * PAGE_SIZE_U64, phys + i * PAGE_SIZE_U64, flags)?;
        }
        Ok(())
    }

    /// Free every user frame and page table below the kernel half.
    ///
    /// The whole user half is detached first and the detachment made visible
    /// before anything under it is handed back. A frame released while a
    /// mapping to it still exists can be given to another address space and
    /// written through the old one.
    pub fn free_user_memory(&self) {
        let mut detached = [Entry::EMPTY; 256];
        unsafe {
            let pml4 = table_at(self.pml4);
            for (i, entry) in detached.iter_mut().enumerate() {
                *entry = *pml4.add(i);
                Entry::EMPTY.store(pml4.add(i));
            }
        }
        flush_tlb_all();
        for entry in detached {
            if entry.is_present() {
                unsafe { self.free_table(entry.addr(), 3) };
            }
        }
    }

    unsafe fn free_table(&self, table_phys: u64, level: u32) {
        if level > 1 {
            let table = table_at(table_phys);
            for i in 0..512 {
                let entry = *table.add(i);
                if !entry.is_present() || entry.bits() & HUGE != 0 {
                    continue;
                }
                self.free_table(entry.addr(), level - 1);
            }
        } else {
            let table = table_at(table_phys);
            for i in 0..512 {
                let entry = *table.add(i);
                if entry.is_present() {
                    drop(Frame::from_recorded(entry.addr()));
                }
            }
        }
        drop(Frame::from_recorded(table_phys));
    }

    /// Share every user mapping from `src` with this address space.
    ///
    /// Writable pages are made read-only on both sides and marked
    /// copy-on-write, so a fork costs a page table walk rather than a copy of
    /// the whole address space; the copy happens per page, only if written to.
    ///
    /// A walk that stops partway has still taken write permission away from
    /// every page it reached, so the flush belongs to both outcomes and is
    /// done here where neither can get past it. What this address space has
    /// collected by then is the caller's to release.
    pub fn clone_user_from(&self, src: &AddressSpace) -> Result<(), MapError> {
        let result = unsafe { self.share_user_tables(src) };
        // The parent's write permissions just changed underneath it.
        flush_tlb_all();
        result
    }

    unsafe fn share_user_tables(&self, src: &AddressSpace) -> Result<(), MapError> {
        let src_pml4 = table_at(src.pml4);
        for i in 0..256usize {
            let e4 = *src_pml4.add(i);
            if !e4.is_present() {
                continue;
            }
            let pdpt = table_at(e4.addr());
            for j in 0..512usize {
                let e3 = *pdpt.add(j);
                if !e3.is_present() || e3.bits() & HUGE != 0 {
                    continue;
                }
                let pd = table_at(e3.addr());
                for k in 0..512usize {
                    let e2 = *pd.add(k);
                    if !e2.is_present() || e2.bits() & HUGE != 0 {
                        continue;
                    }
                    let pt = table_at(e2.addr());
                    for l in 0..512usize {
                        let entry = *pt.add(l);
                        if !entry.is_present() {
                            continue;
                        }
                        let virt = sign_extend(
                            ((i as u64) << 39)
                                | ((j as u64) << 30)
                                | ((k as u64) << 21)
                                | ((l as u64) << 12),
                        );
                        let phys = entry.addr();
                        let flags = entry.flags();

                        let shared = if flags & WRITABLE != 0 {
                            let shared = (flags & !WRITABLE) | COW;
                            // The parent loses write access too, or it
                            // would change pages the child can see.
                            Entry::new(phys | shared).store(pt.add(l));
                            shared
                        } else {
                            flags
                        };
                        // A second reference for the entry about to be
                        // written in the child.
                        self.map(virt, frame::share_recorded(phys), shared)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Release the PML4 itself. The kernel half is shared, so it is not freed.
    pub fn destroy(self) {
        self.free_user_memory();
        drop(unsafe { Frame::from_recorded(self.pml4) });
    }
}

/// Drop the boot-time identity map of the low 4 GiB now that the kernel runs
/// entirely out of the higher half. This frees PML4[0] for user programs.
pub unsafe fn drop_identity_map() {
    let pml4 = table_at(read_cr3());
    Entry::EMPTY.store(pml4.add(0));
    flush_tlb_all();
}

/// Sign-extend a 48-bit virtual address into canonical form.
#[inline]
pub fn sign_extend(virt: u64) -> u64 {
    ((virt << 16) as i64 >> 16) as u64
}

/// True when `virt` is a canonical user-space address.
#[inline]
pub fn is_user_addr(virt: u64) -> bool {
    virt < 0x0000_8000_0000_0000
}

#[inline]
pub fn is_hhdm_addr(virt: u64) -> bool {
    virt >= HHDM_BASE && virt < HHDM_BASE + crate::mm::HHDM_LIMIT
}
