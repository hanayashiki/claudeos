//! 4-level x86_64 page tables.
//!
//! Tables are always reached through the direct map, so no recursive mapping
//! or temporary windows are needed.

use crate::mm::frame::{self, Frame};
use crate::mm::{page_align_down, phys_to_virt, HHDM_BASE, PAGE_SIZE_U64};
use crate::sync::NoInterrupts;
use core::arch::asm;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::Deref;
use core::sync::atomic::{AtomicU64, Ordering};

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

    /// Take away the mapping at `at`, which translated `virt`, handing back
    /// what it held. `None`, and nothing written, when there was no mapping.
    ///
    /// Clearing the entry and invalidating the address are one operation
    /// because a translation the hardware has cached outlives the entry it was
    /// read from. Written apart, a tick lands between them, and a sibling
    /// thread on this address space still reaches the page: a switch to a task
    /// on the same tables reloads nothing, so nothing throws the translation
    /// away. Where the frame is released in the same breath -- and taking a
    /// table away releases one -- that is a page the allocator has given to
    /// somebody else. Being one operation is what stops the two being written
    /// apart; the token is what says nothing runs between them.
    #[inline]
    pub unsafe fn take(at: *mut Entry, virt: u64, _irq: NoInterrupts) -> Option<Entry> {
        let old = *at;
        if !old.is_present() {
            return None;
        }
        Entry::EMPTY.store(at);
        flush_tlb(virt);
        Some(old)
    }

    /// The same for an entry that names a table rather than a page.
    ///
    /// What this entry covered is every address under it, and what the
    /// hardware holds of it is the unfinished walks it has cached as well as
    /// the finished translations, so naming one address invalidates nothing
    /// that matters. The whole space goes: on this processor that is a reload
    /// of the table base register, and since no entry in this kernel is marked
    /// global it discards the unfinished walks with everything else.
    #[inline]
    pub unsafe fn take_table(at: *mut Entry, _irq: NoInterrupts) -> Option<Entry> {
        let old = *at;
        if !old.is_present() {
            return None;
        }
        Entry::EMPTY.store(at);
        flush_tlb_all();
        Some(old)
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

/// True when nothing in the table at `phys` is present.
///
/// The sweep starts one past `cleared`, the entry an unmap has just emptied,
/// because a range taken away a page at a time still has the next page in
/// place there and the sweep stops on its first look. Only the last page of a
/// table costs all five hundred and twelve.
unsafe fn table_is_empty(phys: u64, cleared: usize) -> bool {
    let table = table_at(phys);
    for step in 1..=512 {
        if (*table.add((cleared + step) % 512)).is_present() {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    OutOfMemory,
    AlreadyMapped,
}

/// A page on its way in: a frame the allocator has just handed over, zeroed,
/// and mapped nowhere yet.
///
/// This is the only thing `publish` takes, and it consumes it. A caller with
/// contents to put in a page therefore puts them in here, through the direct
/// map, while the page is somewhere no program can reach; once it is published
/// there is nothing left to write through. The other order -- map the page
/// wide enough to write through, write, then narrow it to what it should have
/// been -- has no spelling, and it is the order that leaves a page a sibling
/// thread can read blank, or run, before it holds anything.
///
/// Only the allocator makes one, so this is not a way to move a frame that
/// another mapping is holding: taking a mapping away and putting the same
/// frame back somewhere else still cannot be written.
pub struct FreshPage(Frame);

impl FreshPage {
    /// A zeroed frame, or `None` when there is none to be had.
    pub fn new() -> Option<FreshPage> {
        frame::alloc_zeroed().map(FreshPage)
    }

    /// The page's bytes, through the direct map. The address is also what a
    /// cache maintenance operation on these bytes has to name, since it is the
    /// one they were written through.
    pub fn bytes(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(
                phys_to_virt(self.0.addr()) as *mut u8,
                PAGE_SIZE_U64 as usize,
            )
        }
    }
}

/// The PML4 the kernel was running on when it took over the machine.
///
/// Every address space copies its kernel half from it, and the processor is
/// put back on it when it is left on no address space of a program's. It is
/// in the kernel image rather than taken from the frame allocator, and nothing
/// frees it.
static KERNEL_PML4: AtomicU64 = AtomicU64::new(0);

/// Record the tables the processor is on as the kernel's own. Once, at boot,
/// before anything is mapped into the kernel half and before any address space
/// exists.
pub fn adopt_boot_tables() {
    KERNEL_PML4.store(read_cr3(), Ordering::Release);
}

/// The PML4 the processor is walking right now.
pub fn live_root() -> u64 {
    read_cr3()
}

/// Print the walk of `virt` through the tables the processor is on, which in
/// a fault report may not be the ones the running task is recorded on.
pub fn dump_live_walk(virt: u64) {
    // Never dropped, so it frees nothing. The tables it names are the
    // kernel's own or an address space the processor holds a reference to
    // while it is loaded (`mm::space`), so they are not freed under the walk.
    ManuallyDrop::new(PageTables { pml4: live_root() }).dump_walk(virt);
}

/// A page table hierarchy for one address space: its PML4, and every table and
/// frame the user half of it reaches.
///
/// It has one owner. It is not `Copy` or `Clone`, the PML4's address is
/// private, and the only way the tables go back is dropping the owner, so a
/// second value naming the same tables cannot be written outside this file and
/// cannot free them twice. Threads that share an address space share this
/// owner through `mm::Mm`, and the last reference to that is what drops it.
/// When this was a `Copy` value with a public address and a safe `destroy`,
/// `sched::release` decided whether to destroy one by looking for a task still
/// naming the same PML4. Two tasks releasing threads of one process could both
/// find none, and the second destroy freed a PML4 that had already been given
/// to another process, along with every frame its old entries still named.
pub struct PageTables {
    pml4: u64,
}

impl Drop for PageTables {
    /// Release every user mapping, every table that held one, and the PML4.
    /// The kernel half is shared, so it is not freed.
    ///
    /// The processor must not be on these tables, since it walks them for the
    /// kernel's own addresses as well. `mm::space` holds a reference to the
    /// address space that is loaded and gives it up only after another has
    /// been, so the last owner is never dropped while its tables are loaded.
    /// The check turns a mistake there into a panic rather than a page table
    /// freed under the processor.
    fn drop(&mut self) {
        assert_ne!(self.pml4, read_cr3(), "freeing the tables the processor is on");
        self.free_user_memory();
        // SAFETY: `new_user` recorded the PML4's reference in this owner, and
        // this is the owner's one drop.
        drop(unsafe { Frame::from_recorded(self.pml4) });
    }
}

/// Tables that an owner elsewhere holds, reached without going through it.
///
/// For the page table changes that are not yet made under the lock of the
/// address space they belong to (`mm::Mm::tables_unlocked`). It releases
/// nothing when dropped, and borrows from what keeps the owner alive, so it
/// cannot outlive the tables.
pub struct Borrowed<'a> {
    tables: ManuallyDrop<PageTables>,
    owner: PhantomData<&'a ()>,
}

impl Deref for Borrowed<'_> {
    type Target = PageTables;

    fn deref(&self) -> &PageTables {
        &self.tables
    }
}

/// The kernel half of every address space, through the kernel's own PML4.
///
/// Below the PML4 the kernel half is one set of tables that every address
/// space names, because `new_user` copies the PML4's upper half, so a page
/// mapped there through one PML4 is reached through all of them. The kernel's
/// own is the one this walks. The one the processor happens to be on can
/// belong to a process that exits and is freed while a kernel task that was
/// preempted partway through a walk of it still holds its address.
pub struct KernelTables(ManuallyDrop<PageTables>);

/// The kernel's own tables, which `adopt_boot_tables` recorded.
pub fn kernel_tables() -> KernelTables {
    let pml4 = KERNEL_PML4.load(Ordering::Acquire);
    assert!(pml4 != 0, "the kernel's tables are used before they are recorded");
    KernelTables(ManuallyDrop::new(PageTables { pml4 }))
}

impl KernelTables {
    /// Map a zeroed page at `virt`, an address in the kernel half: the heap.
    pub fn map_new(&self, virt: u64, flags: u64) -> Result<u64, MapError> {
        assert!(!is_user_addr(virt), "a kernel mapping at a program's address");
        self.0.map_new(virt, flags)
    }

    /// Map `phys` at `virt`, an address in the kernel half, without taking a
    /// reference on it: in practice a device's registers.
    ///
    /// # Safety
    ///
    /// `phys` must not be memory the frame allocator can hand out for as long
    /// as the mapping stays. Nothing takes this mapping away when a frame is
    /// released, so a frame given to a program afterwards would be reachable
    /// through it.
    pub unsafe fn map_fixed(&self, virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
        assert!(!is_user_addr(virt), "a kernel mapping at a program's address");
        self.0.map_fixed(virt, phys, flags)
    }

    /// Put the processor on the kernel's own tables.
    ///
    /// # Safety
    ///
    /// Interrupts must be off, and the caller must give up whatever kept the
    /// tables it is leaving alive only after this returns.
    pub unsafe fn load(&self) {
        // SAFETY: the kernel's tables map the kernel half, which is all that
        // runs with no address space of a program's.
        unsafe { write_cr3(self.0.pml4) };
    }
}

impl PageTables {
    /// A number that tells these tables apart from every other set alive at the
    /// same moment, for anything that has to key on which one it is.
    pub fn id(&self) -> u64 {
        self.pml4
    }

    /// Create a fresh address space that shares the kernel half.
    pub fn new_user() -> Option<PageTables> {
        // The owner holds the PML4's reference; its drop gives it back.
        let pml4 = frame::alloc_zeroed()?.into_recorded();
        let kernel = kernel_tables();
        unsafe {
            let src = table_at(kernel.0.pml4);
            let dst = table_at(pml4);
            // Entries 256..512 cover the kernel: direct map, heap, image.
            for i in 256..512 {
                (*src.add(i)).store(dst.add(i));
            }
        }
        Some(PageTables { pml4 })
    }

    /// A handle on the tables whose `id` is `id`, that releases nothing.
    ///
    /// # Safety
    ///
    /// `id` must be the `id()` of a `PageTables` that is not dropped for as
    /// long as `'a` lasts.
    pub unsafe fn borrow<'a>(id: u64) -> Borrowed<'a> {
        Borrowed { tables: ManuallyDrop::new(PageTables { pml4: id }), owner: PhantomData }
    }

    /// Put the processor on these tables.
    ///
    /// # Safety
    ///
    /// Interrupts must be off, and the tables must stay alive for as long as
    /// they are loaded: the caller keeps a reference to their owner until the
    /// processor has been put on other tables.
    pub unsafe fn load(&self) {
        // SAFETY: the kernel half is copied into every address space, so the
        // kernel runs on as it did; the caller keeps the tables alive.
        unsafe { write_cr3(self.pml4) };
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

    /// The entry the walk of `virt` reads at `level`: the one in the top table
    /// when `level` is 3, the last-level one when it is 0. `None` when the walk
    /// does not get that far, or when a large page ends it first.
    unsafe fn entry_at(&self, virt: u64, level: u32) -> Option<*mut Entry> {
        let mut table = self.pml4;
        for above in (level + 1..4).rev() {
            let entry = *table_at(table).add(index_of(virt, above));
            if !entry.is_present() || entry.bits() & HUGE != 0 {
                return None;
            }
            table = entry.addr();
        }
        Some(table_at(table).add(index_of(virt, level)))
    }

    /// Give back the tables the unmap of `virt` has left with nothing in them,
    /// from the last level upwards, stopping at the first that still holds a
    /// mapping. The top table is not among them: it belongs to the address
    /// space and goes back when the space does.
    ///
    /// A last-level table covers two megabytes. Without this, a program that
    /// maps one page, touches it and takes it away again at one fresh
    /// two-megabyte-aligned address after another gets every page back and
    /// leaves a table behind each time: kernel memory no program owns, which
    /// nothing asks for again until the process ends.
    ///
    /// A table freed here must be named by nothing else. The kernel's own
    /// tables are named by every address space, because `new_user` copies the
    /// top table's upper half rather than taking a reference on what it points
    /// at, so an address outside the half a program owns is declined whatever
    /// its tables hold. Below that the frame's reference count is the answer,
    /// which is the same question a shared page is asked: a fork builds the
    /// child's tables with `map` rather than pointing at the parent's, so a
    /// live table's count is one, and a path that did share one would be
    /// declined here rather than freed under the other side.
    ///
    /// The frame goes back only after `take_table` has invalidated what it
    /// covered and waited for that to take effect, and the whole of this is
    /// inside the section that did the unmap: with a gap there another task
    /// maps into a table between its being found empty and its being freed.
    unsafe fn reclaim_tables(&self, virt: u64, irq: NoInterrupts) {
        if !is_user_addr(virt) {
            return;
        }
        for level in 1..4 {
            let Some(parent) = self.entry_at(virt, level) else {
                return;
            };
            let entry = *parent;
            if !entry.is_present() || entry.bits() & HUGE != 0 {
                return;
            }
            let table = entry.addr();
            if !table_is_empty(table, index_of(virt, level - 1))
                || frame::frame_references(table) != 1
            {
                return;
            }
            if Entry::take_table(parent, irq).is_some() {
                drop(Frame::from_recorded(table));
            }
        }
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
    /// the kernel half is never torn down. Private: `KernelTables::map_fixed`
    /// is the way in, and says what the caller has to promise.
    fn map_fixed(&self, virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
        let virt = page_align_down(virt);
        unsafe {
            let entry = self.entry_for(virt, true, flags)?;
            Entry::new((phys & ADDR_MASK) | flags | PRESENT).store(entry);
        }
        flush_tlb(virt);
        Ok(())
    }

    /// Put `page` in at `virt` with `flags`, giving back where it is. The entry
    /// holds the page's reference from here on, and `unmap` or teardown gives
    /// it back.
    ///
    /// The one way a page with contents in it becomes reachable. It takes the
    /// page by value, so what goes in was finished beforehand: there is no
    /// moment when the address has something at it that is not what it is going
    /// to hold, and none when it is reachable with permissions it is not going
    /// to keep.
    ///
    /// The store that writes the entry carries the ordering, so nothing has to
    /// order the contents against it by hand.
    pub fn publish(&self, virt: u64, page: FreshPage, flags: u64) -> Result<u64, MapError> {
        let frame = page.0;
        let phys = frame.addr();
        self.map(virt, frame, flags)?;
        Ok(phys)
    }

    /// Map a page whose finished contents are zero, which a fresh frame already
    /// is: the heap, and anonymous memory.
    pub fn map_new(&self, virt: u64, flags: u64) -> Result<u64, MapError> {
        self.publish(virt, FreshPage::new().ok_or(MapError::OutOfMemory)?, flags)
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
    ///
    /// The table the entry was in goes back too when it holds nothing else,
    /// and the ones above it while they keep emptying. The token is what says
    /// nothing runs between the entry being cleared and that decision being
    /// made on it.
    #[must_use = "dropping the frame is what releases it"]
    pub fn unmap(&self, virt: u64, irq: NoInterrupts) -> Option<Frame> {
        let virt = page_align_down(virt);
        unsafe {
            // The walk creates nothing, so there is no allocation inside the
            // section this opens: an entry that is not there is an address
            // with nothing mapped at it, which is what this reports.
            let entry = self.entry_for(virt, false, 0).ok()?;
            let old = Entry::take(entry, virt, irq)?;
            self.reclaim_tables(virt, irq);
            Some(Frame::from_recorded(old.addr()))
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

    /// Free every user frame and page table below the kernel half.
    ///
    /// The whole user half is detached first and the detachment made visible
    /// before anything under it is handed back. A frame released while a
    /// mapping to it still exists can be given to another address space and
    /// written through the old one.
    ///
    /// Private, and reached only from the owner's drop: called on tables that
    /// are still in use, it empties them under whoever is running on them.
    fn free_user_memory(&self) {
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
    /// Each page is invalidated where its permission changes, so a walk that
    /// stops partway leaves nothing behind that a flush here would have to
    /// clean up. What this address space has collected by then is the
    /// caller's to release.
    pub fn clone_user_from(&self, src: &PageTables) -> Result<(), MapError> {
        unsafe { self.share_user_tables(src) }
    }

    /// Share the page `at` names with this address space at `virt`, leaving
    /// both sides copy-on-write when it was writable.
    ///
    /// One call, because the two halves are one account of the page. Taking
    /// the parent's write permission away and then taking the child's
    /// reference on the frame were two calls with a window between them, and
    /// the second takes the allocator's lock, which unmasks interrupts on the
    /// way out. A sibling thread that faulted on the page there read a
    /// reference count of one, took the last-owner path and cleared the mark,
    /// so the parent kept write access to a frame the child was about to
    /// share. Two processes then had a writable mapping of one page and
    /// neither knew.
    ///
    /// The token is what says nothing runs between the two halves. It could
    /// not say that while they were separate calls: each is legitimate alone,
    /// and a token proves a section exists rather than that two calls are
    /// inside one. The walk opens the section around one page rather than
    /// around itself, because it is tens of thousands of pages long.
    ///
    /// The reference for the child is taken first, so the count is never
    /// lower than the number of mappings that are going to hold it.
    unsafe fn share_page(
        &self,
        virt: u64,
        at: *mut Entry,
        _irq: NoInterrupts,
    ) -> Result<(), MapError> {
        let entry = *at;
        if !entry.is_present() {
            // Gone between the walk reading the entry and this section
            // opening, so there is nothing here to share.
            return Ok(());
        }
        let phys = entry.addr();
        let shared = frame::share_recorded(phys);
        let flags = entry.flags();
        let flags = if flags & WRITABLE != 0 {
            let cow = (flags & !WRITABLE) | COW;
            // The parent loses write access too, or it would change pages the
            // child can see. The walk runs on the parent's tables, so the
            // translation this contradicts is in the processor's cache of them
            // right now, and a sibling's store would go through it into the
            // page the child is about to share until it is thrown away.
            Entry::new(phys | cow).store(at);
            flush_tlb(virt);
            cow
        } else {
            flags
        };
        self.map(virt, shared, flags)
    }

    unsafe fn share_user_tables(&self, src: &PageTables) -> Result<(), MapError> {
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
                        let entry_ptr = pt.add(l);
                        crate::sync::without_interrupts(|irq| {
                            self.share_page(virt, entry_ptr, irq)
                        })?;
                    }
                }
            }
        }
        Ok(())
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
