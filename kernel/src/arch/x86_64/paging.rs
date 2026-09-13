//! 4-level x86_64 page tables.
//!
//! Tables are always reached through the direct map, so no recursive mapping
//! or temporary windows are needed.

use crate::mm::frame::{self, Frame};
use crate::mm::{page_align_down, page_align_up, phys_to_virt, HHDM_BASE, PAGE_SIZE_U64};
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

#[inline]
unsafe fn table_at(phys: u64) -> *mut u64 {
    phys_to_virt(phys) as *mut u64
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
                *dst.add(i) = *src.add(i);
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
    ) -> Result<*mut u64, MapError> {
        let mut table = self.pml4;
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
                *entry_ptr = new | PRESENT | WRITABLE | (parent_flags & USER);
                table = new;
            } else {
                if entry & HUGE != 0 {
                    // A large page already covers this address.
                    return Err(MapError::AlreadyMapped);
                }
                // Widen permissions on the way down: a user leaf is
                // unreachable if any parent lacks the user bit.
                if parent_flags & USER != 0 && entry & USER == 0 {
                    *entry_ptr = entry | USER;
                }
                table = entry & ADDR_MASK;
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
    /// to the allocator. A caller that means to replace a mapping takes the
    /// old one away first and decides for itself what to do with what `unmap`
    /// hands back.
    pub fn map(&self, virt: u64, frame: Frame, flags: u64) -> Result<(), MapError> {
        let virt = page_align_down(virt);
        unsafe {
            let entry = self.entry_for(virt, true, flags)?;
            if *entry & PRESENT != 0 {
                return Err(MapError::AlreadyMapped);
            }
            *entry = (frame.into_recorded() & ADDR_MASK) | flags | PRESENT;
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
            *entry = (phys & ADDR_MASK) | flags | PRESENT;
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

    /// Take the mapping at `virt` away, handing back the reference the entry
    /// held. Dropping the result releases the frame.
    #[must_use = "dropping the frame is what releases it"]
    pub fn unmap(&self, virt: u64) -> Option<Frame> {
        let virt = page_align_down(virt);
        unsafe {
            let entry = self.entry_for(virt, false, 0).ok()?;
            let value = *entry;
            if value & PRESENT == 0 {
                return None;
            }
            *entry = 0;
            flush_tlb(virt);
            Some(Frame::from_recorded(value & ADDR_MASK))
        }
    }

    pub fn translate(&self, virt: u64) -> Option<u64> {
        unsafe {
            let mut table = self.pml4;
            for level in (1..4).rev() {
                let entry = *table_at(table).add(index_of(virt, level));
                if entry & PRESENT == 0 {
                    return None;
                }
                if entry & HUGE != 0 {
                    let page_size = 1u64 << (12 + 9 * level);
                    return Some((entry & ADDR_MASK & !(page_size - 1)) | (virt & (page_size - 1)));
                }
                table = entry & ADDR_MASK;
            }
            let entry = *table_at(table).add(index_of(virt, 0));
            if entry & PRESENT == 0 {
                return None;
            }
            Some((entry & ADDR_MASK) | (virt & 0xFFF))
        }
    }

    pub fn flags_of(&self, virt: u64) -> Option<u64> {
        unsafe {
            let entry = self.entry_for(virt, false, 0).ok()?;
            let value = *entry;
            if value & PRESENT == 0 {
                None
            } else {
                Some(value & !ADDR_MASK)
            }
        }
    }

    pub fn set_flags(&self, virt: u64, flags: u64) -> Option<()> {
        unsafe {
            let entry = self.entry_for(virt, false, flags).ok()?;
            let value = *entry;
            if value & PRESENT == 0 {
                return None;
            }
            *entry = (value & ADDR_MASK) | flags | PRESENT;
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
        let mut detached = [0u64; 256];
        unsafe {
            let pml4 = table_at(self.pml4);
            for (i, entry) in detached.iter_mut().enumerate() {
                *entry = *pml4.add(i);
                *pml4.add(i) = 0;
            }
        }
        flush_tlb_all();
        for entry in detached {
            if entry & PRESENT != 0 {
                unsafe { self.free_table(entry & ADDR_MASK, 3) };
            }
        }
    }

    unsafe fn free_table(&self, table_phys: u64, level: u32) {
        if level > 1 {
            let table = table_at(table_phys);
            for i in 0..512 {
                let entry = *table.add(i);
                if entry & PRESENT == 0 || entry & HUGE != 0 {
                    continue;
                }
                self.free_table(entry & ADDR_MASK, level - 1);
            }
        } else {
            let table = table_at(table_phys);
            for i in 0..512 {
                let entry = *table.add(i);
                if entry & PRESENT != 0 {
                    drop(Frame::from_recorded(entry & ADDR_MASK));
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
    pub fn clone_user_from(&self, src: &AddressSpace) -> Result<(), MapError> {
        unsafe {
            let src_pml4 = table_at(src.pml4);
            for i in 0..256usize {
                let e4 = *src_pml4.add(i);
                if e4 & PRESENT == 0 {
                    continue;
                }
                let pdpt = table_at(e4 & ADDR_MASK);
                for j in 0..512usize {
                    let e3 = *pdpt.add(j);
                    if e3 & PRESENT == 0 || e3 & HUGE != 0 {
                        continue;
                    }
                    let pd = table_at(e3 & ADDR_MASK);
                    for k in 0..512usize {
                        let e2 = *pd.add(k);
                        if e2 & PRESENT == 0 || e2 & HUGE != 0 {
                            continue;
                        }
                        let pt = table_at(e2 & ADDR_MASK);
                        for l in 0..512usize {
                            let entry = *pt.add(l);
                            if entry & PRESENT == 0 {
                                continue;
                            }
                            let virt = sign_extend(
                                ((i as u64) << 39)
                                    | ((j as u64) << 30)
                                    | ((k as u64) << 21)
                                    | ((l as u64) << 12),
                            );
                            let phys = entry & ADDR_MASK;
                            let flags = entry & !ADDR_MASK;

                            let shared = if flags & WRITABLE != 0 {
                                let shared = (flags & !WRITABLE) | COW;
                                // The parent loses write access too, or it
                                // would change pages the child can see.
                                *pt.add(l) = phys | shared;
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
        }
        // The parent's write permissions just changed underneath it.
        flush_tlb_all();
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
    *pml4.add(0) = 0;
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
